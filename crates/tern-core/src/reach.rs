//! Reachability: which saved hosts answer, and how fast, from checks run in
//! the background.
//!
//! A check is [`crate::ssh::probe`]: a timed TCP connection, then the
//! server's SSH greeting. Each place (address and port) is checked about
//! every [`CHECK_EVERY`], a few at a time; a host behind a jump host is
//! checked at the jump host, its first hop. [`Monitor`] keeps the books:
//! what's due, what's running, and the recent samples. Running the checks
//! is up to the caller.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::host::HostStatus;

/// How often each place is checked.
pub const CHECK_EVERY: Duration = Duration::from_secs(30);
/// Checks running at once, at most.
pub const AT_ONCE: usize = 4;
/// Samples kept per place: a little more than a host card's trace shows.
pub const KEEP: usize = 40;

/// Where a check goes: an address and a port.
pub type Place = (String, u16);

/// The checks of one place.
#[derive(Clone, Debug, Default)]
struct Watch {
    /// Round trips in milliseconds, oldest first; `None` for a check that
    /// failed.
    samples: Vec<Option<u32>>,
    checked_at: Option<Instant>,
    running: bool,
}

/// The books for background checks.
#[derive(Debug, Default)]
pub struct Monitor {
    watches: HashMap<Place, Watch>,
}

impl Monitor {
    /// The places to check now, in the order given: those never checked or
    /// not for [`CHECK_EVERY`], while fewer than [`AT_ONCE`] run. They count
    /// as running until [`Monitor::record`]. Places no longer listed are
    /// forgotten.
    pub fn due(&mut self, places: &[Place], now: Instant) -> Vec<Place> {
        self.due_among(places, places, now)
    }

    /// Like [`Monitor::due`], but only of the places `among`: the rest of
    /// `places` keep what their checks found. The places waiting longest
    /// go first: those never checked, then the longest ago, in the order
    /// given when even; so in a vault of thousands, every host's turn
    /// comes.
    pub fn due_among(&mut self, places: &[Place], among: &[Place], now: Instant) -> Vec<Place> {
        let listed: HashSet<&Place> = places.iter().collect();
        self.watches.retain(|place, _| listed.contains(place));
        let running = self.watches.values().filter(|watch| watch.running).count();
        let free = AT_ONCE.saturating_sub(running);
        // The free slots' places so far, by when they were last checked
        // and where they're listed.
        let mut chosen: Vec<(Option<Instant>, usize)> = Vec::with_capacity(free + 1);
        if free > 0 {
            for (index, place) in among.iter().enumerate() {
                let (checked_at, running) = self
                    .watches
                    .get(place)
                    .map_or((None, false), |watch| (watch.checked_at, watch.running));
                let stale =
                    checked_at.is_none_or(|at| now.saturating_duration_since(at) >= CHECK_EVERY);
                if !stale || running || chosen.iter().any(|(_, at)| among[*at] == *place) {
                    continue;
                }
                let key = (checked_at, index);
                let at = chosen.partition_point(|other| *other <= key);
                if at < free {
                    chosen.insert(at, key);
                    chosen.truncate(free);
                }
            }
        }
        chosen
            .into_iter()
            .map(|(_, index)| {
                let place = &among[index];
                self.watches.entry(place.clone()).or_default().running = true;
                place.clone()
            })
            .collect()
    }

    /// Records a check of `place`: its round trip, or `None` if it failed.
    pub fn record(&mut self, place: &Place, latency_ms: Option<u32>, now: Instant) {
        let watch = self.watches.entry(place.clone()).or_default();
        watch.running = false;
        watch.checked_at = Some(now);
        watch.samples.push(latency_ms);
        if watch.samples.len() > KEEP {
            watch.samples.remove(0);
        }
    }

    /// Unknown until checked, Connecting while the first check runs, then
    /// what the latest check found.
    pub fn status(&self, place: &Place) -> HostStatus {
        let Some(watch) = self.watches.get(place) else {
            return HostStatus::Unknown;
        };
        match watch.samples.last() {
            Some(Some(latency_ms)) => HostStatus::online(*latency_ms),
            Some(None) => HostStatus::Offline,
            None if watch.running => HostStatus::Connecting,
            None => HostStatus::Unknown,
        }
    }

    /// The samples kept for `place`, oldest first, and when the newest was
    /// taken.
    pub fn samples(&self, place: &Place) -> (&[Option<u32>], Option<Instant>) {
        self.watches
            .get(place)
            .map_or((&[], None), |watch| (&watch.samples, watch.checked_at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(name: &str) -> Place {
        (name.to_string(), 22)
    }

    #[test]
    fn checks_what_is_due_a_few_at_a_time() {
        let mut monitor = Monitor::default();
        let places: Vec<Place> = ["a", "b", "c", "d", "e", "f"].map(place).into();
        let start = Instant::now();

        let first = monitor.due(&places, start);
        assert_eq!(first, places[..AT_ONCE]);
        // Running checks aren't started again, and they fill every slot.
        assert!(monitor.due(&places, start).is_empty());

        monitor.record(&places[0], Some(12), start);
        monitor.record(&places[1], None, start);
        assert_eq!(monitor.due(&places, start), places[4..]);

        // A checked place is due again after the interval, not before.
        for place in &places[2..] {
            monitor.record(place, Some(1), start);
        }
        assert!(monitor.due(&places, start + CHECK_EVERY / 2).is_empty());
        assert_eq!(monitor.due(&places, start + CHECK_EVERY), places[..AT_ONCE]);
    }

    #[test]
    fn every_place_gets_its_turn_the_longest_waiting_first() {
        // A thousand places, polled every second, each check over before
        // the next poll: four a poll, never checked first, reach them all.
        let places: Vec<Place> = (0..1000).map(|i| place(&format!("host-{i:04}"))).collect();
        let start = Instant::now();
        let mut monitor = Monitor::default();
        let mut checked = std::collections::HashSet::new();
        for tick in 0..300 {
            let now = start + std::time::Duration::from_secs(tick);
            for due in monitor.due(&places, now) {
                checked.insert(due.clone());
                monitor.record(&due, Some(1), now);
            }
        }
        assert_eq!(checked.len(), places.len());

        // Of places due again, those checked longest ago go first.
        let mut monitor = Monitor::default();
        let few: Vec<Place> = ["a", "b", "c", "d", "e", "f"].map(place).into();
        for (i, each) in few.iter().enumerate().rev() {
            monitor.record(
                each,
                Some(1),
                start + std::time::Duration::from_secs(i as u64),
            );
        }
        let later = start + CHECK_EVERY * 2;
        monitor.record(&few[0], Some(1), later);
        assert_eq!(monitor.due(&few, later), few[1..5]);
        assert_eq!(monitor.due(&few, later), []);
    }

    #[test]
    fn status_follows_the_checks() {
        let mut monitor = Monitor::default();
        let db = place("db");
        let now = Instant::now();
        assert_eq!(monitor.status(&db), HostStatus::Unknown);
        monitor.due(std::slice::from_ref(&db), now);
        assert_eq!(monitor.status(&db), HostStatus::Connecting);
        monitor.record(&db, Some(14), now);
        assert_eq!(monitor.status(&db), HostStatus::online(14));

        // A re-check keeps showing the last result while it runs.
        let later = now + CHECK_EVERY;
        monitor.due(std::slice::from_ref(&db), later);
        assert_eq!(monitor.status(&db), HostStatus::online(14));
        monitor.record(&db, None, later);
        assert_eq!(monitor.status(&db), HostStatus::Offline);
        assert_eq!(monitor.samples(&db), (&[Some(14), None][..], Some(later)));
    }

    #[test]
    fn keeps_the_latest_samples_and_forgets_removed_places() {
        let mut monitor = Monitor::default();
        let db = place("db");
        let now = Instant::now();
        for ms in 0..KEEP as u32 + 5 {
            monitor.record(&db, Some(ms), now);
        }
        let (samples, _) = monitor.samples(&db);
        assert_eq!(samples.len(), KEEP);
        assert_eq!(samples[0], Some(5));

        monitor.due(&[place("web")], now);
        assert_eq!(monitor.status(&db), HostStatus::Unknown);
        assert!(monitor.samples(&db).0.is_empty());
    }

    #[test]
    fn checking_some_places_keeps_the_others() {
        let mut monitor = Monitor::default();
        let (db, web) = (place("db"), place("web"));
        let places = [db.clone(), web.clone()];
        let now = Instant::now();
        monitor.record(&db, Some(9), now);

        let later = now + CHECK_EVERY;
        let among = std::slice::from_ref(&web);
        assert_eq!(monitor.due_among(&places, among, later), among);
        assert_eq!(monitor.status(&db), HostStatus::online(9));
        assert_eq!(monitor.status(&web), HostStatus::Connecting);
    }
}
