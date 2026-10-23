//! Reconnecting a session whose connection dropped: when to try again, and
//! which failures are worth another try.

use std::time::Duration;

use crate::ssh::ConnectError;

/// Attempts before giving up: about two minutes in all.
pub const ATTEMPTS: u32 = 8;

/// The longest wait between attempts.
const LONGEST: Duration = Duration::from_secs(30);

/// How long to wait before attempt `attempt`, counting from 1: a second,
/// doubling up to half a minute. `None` once the attempts are used up.
pub fn delay(attempt: u32) -> Option<Duration> {
    (1..=ATTEMPTS)
        .contains(&attempt)
        .then(|| Duration::from_secs(1 << (attempt - 1).min(5)).min(LONGEST))
}

/// Whether a failed attempt is worth repeating. The network or the server
/// may come back by itself; a refused sign-in or an unknown or changed
/// host key needs the user.
pub fn worth_retrying(error: &ConnectError) -> bool {
    matches!(error, ConnectError::Other(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_longer_each_time_then_gives_up() {
        let waits: Vec<u64> = (1..=ATTEMPTS)
            .map(|attempt| delay(attempt).unwrap().as_secs())
            .collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(delay(0), None);
        assert_eq!(delay(ATTEMPTS + 1), None);
    }

    #[test]
    fn retries_only_what_may_pass_by_itself() {
        assert!(worth_retrying(&ConnectError::Other(
            "connection refused".into()
        )));
        assert!(!worth_retrying(&ConnectError::JumpRefused(
            "can't sign in to bastion: authentication failed (tried password)".into()
        )));
        assert!(!worth_retrying(&ConnectError::AuthFailed {
            tried: Vec::new()
        }));
        assert!(!worth_retrying(&ConnectError::HostKeyChanged {
            fingerprint: "SHA256:x".into(),
            file: "known_hosts".into(),
            line: 1,
        }));
    }
}
