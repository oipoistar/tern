//! What a Linux host's `/proc` says, read over SFTP so nothing runs in the
//! user's shell: the ports it listens on, so the ones a session opens can
//! be forwarded in a click, and its vitals (load, memory, disk), kept as
//! short histories to draw as traces.

use std::collections::VecDeque;

/// How many readings of each vital are kept.
pub const KEEP: usize = 40;

/// A port listening on the host, and the address to reach it at from
/// there: `localhost` for one on every address or a loopback one.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Listening {
    pub port: u16,
    pub host: String,
}

/// The ports listening in a `/proc/net/tcp` or `tcp6` table, each once.
pub fn listening(table: &str) -> Vec<Listening> {
    let mut found: Vec<Listening> = table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // `0A` is TCP_LISTEN.
            if fields.get(3) != Some(&"0A") {
                return None;
            }
            let (address, port) = fields.get(1)?.split_once(':')?;
            let port = u16::from_str_radix(port, 16).ok()?;
            Some(Listening {
                port,
                host: reach_at(address)?,
            })
        })
        .collect();
    found.sort();
    // A port on both IPv4 and IPv6 is one port; `localhost` sorts first.
    found.dedup_by(|later, earlier| later.port == earlier.port);
    found
}

/// Where a listening address is reached from the host: `localhost` for
/// any address or a loopback one, else its IPv4 address. `None` for a
/// particular IPv6 address.
fn reach_at(hex: &str) -> Option<String> {
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok())
        .collect::<Option<_>>()?;
    match bytes.len() {
        // One word, in the host's byte order (little-endian on the
        // machines that matter).
        4 => {
            let ip = [bytes[3], bytes[2], bytes[1], bytes[0]];
            if ip == [0, 0, 0, 0] || ip[0] == 127 {
                Some("localhost".into())
            } else {
                Some(format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]))
            }
        }
        16 => {
            let any = bytes.iter().all(|&byte| byte == 0);
            // ::1 is three zero words, then one holding 1.
            let loopback = bytes[..12].iter().all(|&byte| byte == 0)
                && u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) == 1;
            (any || loopback).then(|| "localhost".into())
        }
        _ => None,
    }
}

/// The ports in `now` that weren't listening `before`: the ones a session
/// opened.
pub fn opened(before: &[Listening], now: &[Listening]) -> Vec<Listening> {
    now.iter()
        .filter(|port| !before.iter().any(|old| old.port == port.port))
        .cloned()
        .collect()
}

/// The one-minute load average, from `/proc/loadavg`.
pub fn load(loadavg: &str) -> Option<f32> {
    loadavg.split_whitespace().next()?.parse().ok()
}

/// How many CPUs `/proc/cpuinfo` lists.
pub fn cpus(cpuinfo: &str) -> usize {
    cpuinfo
        .lines()
        .filter(|line| line.split(':').next().map(str::trim) == Some("processor"))
        .count()
}

/// The share of memory in use, 0 to 1, from `/proc/meminfo`: what isn't
/// available to programs.
pub fn memory(meminfo: &str) -> Option<f32> {
    let field = |name: &str| -> Option<f64> {
        meminfo
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    let (total, available) = (field("MemTotal")?, field("MemAvailable")?);
    (total > 0.).then(|| (1. - available / total).clamp(0., 1.) as f32)
}

/// The share of a file system in use, 0 to 1, as `df` counts it: what's
/// used against what's used and still free for users.
pub fn disk(blocks: u64, free: u64, available: u64) -> Option<f32> {
    let used = blocks.checked_sub(free)?;
    let usable = used + available;
    (usable > 0).then(|| used as f32 / usable as f32)
}

/// A host's recent vitals, oldest first; a reading that failed is `None`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Vitals {
    pub load: VecDeque<Option<f32>>,
    pub memory: VecDeque<Option<f32>>,
    pub disk: VecDeque<Option<f32>>,
    /// How many CPUs share the load.
    pub cpus: usize,
}

impl Vitals {
    /// Adds a reading, forgetting the oldest beyond [`KEEP`].
    pub fn push(&mut self, load: Option<f32>, memory: Option<f32>, disk: Option<f32>) {
        for (history, reading) in [
            (&mut self.load, load),
            (&mut self.memory, memory),
            (&mut self.disk, disk),
        ] {
            history.push_back(reading);
            if history.len() > KEEP {
                history.pop_front();
            }
        }
    }

    /// Whether anything was read yet.
    pub fn is_empty(&self) -> bool {
        self.load
            .iter()
            .chain(&self.memory)
            .chain(&self.disk)
            .all(Option::is_none)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0 100 0 0 10 0
   1: 0100007F:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 2 1 0 100 0 0 10 0
   2: 0500000A:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 3 1 0 100 0 0 10 0
   3: 0100007F:0BB8 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 4 1 0 20 4 30 10 -1
";
    const TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:0016 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 5 1 0 100 0 0 10 0
   1: 00000000000000000000000001000000:1538 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 6 1 0 100 0 0 10 0
   2: 20010DB8000000000000000000000001:1A0B 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 7 1 0 100 0 0 10 0
";

    fn at(port: u16, host: &str) -> Listening {
        Listening {
            port,
            host: host.into(),
        }
    }

    #[test]
    fn reads_listening_ports_from_proc() {
        assert_eq!(
            listening(TCP),
            [
                at(22, "localhost"),
                at(3000, "localhost"),
                at(8080, "10.0.0.5")
            ]
        );
        // ::1 and :: are localhost; a particular IPv6 address is left out.
        assert_eq!(
            listening(TCP6),
            [at(22, "localhost"), at(5432, "localhost")]
        );
        // Both tables at once: each port once.
        let both = listening(&format!(
            "{TCP}{}",
            TCP6.lines().skip(1).collect::<Vec<_>>().join("\n")
        ));
        assert_eq!(both.iter().filter(|port| port.port == 22).count(), 1);
        assert!(listening("").is_empty());
        assert!(listening("header\n garbage line\n").is_empty());
    }

    #[test]
    fn the_ports_a_session_opened_are_the_new_ones() {
        let before = [at(22, "localhost")];
        let now = [at(22, "localhost"), at(3000, "localhost")];
        assert_eq!(opened(&before, &now), [at(3000, "localhost")]);
        assert!(opened(&now, &before).is_empty());
    }

    #[test]
    fn reads_vitals_from_proc() {
        assert_eq!(load("0.42 0.30 0.25 1/123 4567\n"), Some(0.42));
        assert_eq!(load(""), None);
        assert_eq!(
            cpus("processor\t: 0\nmodel name\t: x\n\nprocessor\t: 1\n"),
            2
        );
        let meminfo = "MemTotal:        8000000 kB\nMemFree:          100000 kB\nMemAvailable:    2000000 kB\n";
        assert_eq!(memory(meminfo), Some(0.75));
        assert_eq!(memory("MemTotal: 0 kB\nMemAvailable: 0 kB\n"), None);
        assert_eq!(memory("nothing"), None);
        // df: 40 used of 40 used + 50 free for users.
        assert_eq!(disk(100, 60, 50), Some(40. / 90.));
        assert_eq!(disk(0, 0, 0), None);
    }

    #[test]
    fn keeps_the_latest_readings() {
        let mut vitals = Vitals::default();
        assert!(vitals.is_empty());
        for i in 0..KEEP + 5 {
            vitals.push(Some(i as f32), None, Some(0.5));
        }
        assert_eq!(vitals.load.len(), KEEP);
        assert_eq!(vitals.load.front(), Some(&Some(5.)));
        assert!(!vitals.is_empty());
    }
}
