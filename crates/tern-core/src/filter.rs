//! Narrowing the vault's hosts by group, tag and reachability, and moving
//! the selection through the hosts on screen.

use std::collections::BTreeSet;

use crate::host::{Host, HostStatus};
use crate::vault::Vault;

/// Reachability as the filter sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Reach {
    Online,
    Offline,
    /// Not checked yet, or the first check is running.
    Unchecked,
}

impl Reach {
    pub fn of(status: HostStatus) -> Self {
        match status {
            HostStatus::Online { .. } => Self::Online,
            HostStatus::Offline => Self::Offline,
            HostStatus::Unknown | HostStatus::Connecting => Self::Unchecked,
        }
    }
}

/// What the vault shows. Within a kind, any chosen value lets a host
/// through; across kinds, it must pass each. A kind with nothing chosen
/// lets every host through.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    pub groups: BTreeSet<String>,
    pub tags: BTreeSet<String>,
    pub reach: BTreeSet<Reach>,
}

impl Filter {
    pub fn matches(&self, group: &str, host: &Host, status: HostStatus) -> bool {
        (self.groups.is_empty() || self.groups.contains(group))
            && (self.tags.is_empty() || host.tags.iter().any(|tag| self.tags.contains(tag)))
            && (self.reach.is_empty() || self.reach.contains(&Reach::of(status)))
    }

    /// How many values are chosen, shown on the Filter button.
    pub fn count(&self) -> usize {
        self.groups.len() + self.tags.len() + self.reach.len()
    }

    pub fn toggle_group(&mut self, group: &str) {
        toggle(&mut self.groups, group.to_string());
    }

    pub fn toggle_tag(&mut self, tag: &str) {
        toggle(&mut self.tags, tag.to_string());
    }

    pub fn toggle_reach(&mut self, reach: Reach) {
        toggle(&mut self.reach, reach);
    }

    /// Drops chosen groups and tags the vault no longer has.
    pub fn forget_missing(&mut self, vault: &Vault) {
        self.groups
            .retain(|group| vault.groups.iter().any(|have| have.name == *group));
        let tags = tags(vault);
        self.tags.retain(|tag| tags.contains(tag));
    }
}

fn toggle<T: Ord>(set: &mut BTreeSet<T>, value: T) {
    if !set.remove(&value) {
        set.insert(value);
    }
}

/// Every tag in the vault, sorted.
pub fn tags(vault: &Vault) -> Vec<String> {
    vault
        .hosts()
        .flat_map(|host| host.tags.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The host `delta` places from `selected` among `visible`, stopping at the
/// ends; the first one when the selection isn't among them.
pub fn step<'a>(visible: &[&'a str], selected: &str, delta: isize) -> Option<&'a str> {
    let last = visible.len().checked_sub(1)?;
    let index = match visible.iter().position(|name| *name == selected) {
        Some(index) => index.saturating_add_signed(delta).min(last),
        None => 0,
    };
    Some(visible[index])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_and_within_or_across() {
        let vault = Vault::sample();
        let db = vault.find("prod-db-01").unwrap();
        let online = HostStatus::online(12);
        let mut filter = Filter::default();
        assert!(filter.matches("Production", db, online));

        filter.toggle_tag("postgres");
        filter.toggle_tag("docker");
        assert!(filter.matches("Production", db, online));
        filter.toggle_group("Staging & lab");
        assert!(!filter.matches("Production", db, online), "wrong group");
        filter.toggle_group("Production");
        filter.toggle_reach(Reach::Offline);
        assert!(!filter.matches("Production", db, online), "not offline");
        assert!(filter.matches("Production", db, HostStatus::Offline));
        assert_eq!(filter.count(), 5);

        filter.toggle_tag("postgres");
        assert!(!filter.matches("Production", db, HostStatus::Offline));
        assert_eq!(Reach::of(HostStatus::Connecting), Reach::Unchecked);
    }

    #[test]
    fn lists_tags_and_forgets_ones_gone() {
        let mut vault = Vault::sample();
        assert_eq!(
            tags(&vault),
            [
                "docker",
                "eu-west",
                "jump host",
                "nginx",
                "postgres",
                "raspberry pi"
            ]
        );
        let mut filter = Filter::default();
        filter.toggle_tag("raspberry pi");
        filter.toggle_group("Staging & lab");
        for name in ["pi-homelab", "stage-api", "stage-worker"] {
            vault.delete(name).unwrap();
        }
        filter.forget_missing(&vault);
        assert_eq!(filter, Filter::default());
    }

    #[test]
    fn steps_through_the_hosts_on_screen() {
        let visible = ["a", "b", "c"];
        assert_eq!(step(&visible, "a", 1), Some("b"));
        assert_eq!(step(&visible, "c", 1), Some("c"));
        assert_eq!(step(&visible, "a", -1), Some("a"));
        assert_eq!(step(&visible, "gone", 1), Some("a"));
        assert_eq!(step(&[], "a", 1), None);
    }
}
