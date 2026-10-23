//! Shells kept running on a host: in tmux, or else screen, under a name
//! Tern can reattach to, so a dropped connection (or Tern quitting) doesn't
//! end what runs in them. A host without either gets a plain login shell.

/// The name of the kept shell in `slot` (1, 2, … for a host's panes) of
/// the host `key` (its id, or `user@host:port`): `tern-`, a short hash and
/// the slot, the same at every start and safe in a command line.
pub fn name(key: &str, slot: u32) -> String {
    format!("tern-{:08x}-{slot}", fnv(key) as u32)
}

/// The command that starts the kept shell `name`, or reattaches to it.
/// It runs under `sh`, whatever the user's shell is.
pub fn attach(name: &str) -> String {
    format!(
        "sh -c 'command -v tmux >/dev/null 2>&1 && exec tmux new-session -A -s {name}; \
         command -v screen >/dev/null 2>&1 && exec screen -xRR -S {name}; \
         exec \"$SHELL\" -l'"
    )
}

/// The command that ends the kept shell `name` for good.
pub fn end(name: &str) -> String {
    format!(
        "sh -c 'tmux kill-session -t {name} 2>/dev/null; screen -S {name} -X quit 2>/dev/null; true'"
    )
}

/// FNV-1a, which stays the same across Rust versions and platforms.
fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_stable_and_safe_in_a_command_line() {
        let first = name("9f39b2acf25b2d3b", 1);
        assert_eq!(first, name("9f39b2acf25b2d3b", 1));
        assert_ne!(first, name("9f39b2acf25b2d3b", 2));
        assert_ne!(first, name("99a83ab6785f2d68", 1));
        assert!(first.starts_with("tern-") && first.ends_with("-1"));
        assert!(first.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        // A typed host's key has characters a shell would read.
        let typed = name("me@host'; rm -rf /:22", 3);
        assert!(typed.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn attaches_with_tmux_then_screen_then_a_plain_shell() {
        let command = attach("tern-1a2b3c4d-1");
        let tmux = command
            .find("tmux new-session -A -s tern-1a2b3c4d-1")
            .unwrap();
        let screen = command.find("screen -xRR -S tern-1a2b3c4d-1").unwrap();
        let shell = command.find("exec \"$SHELL\" -l").unwrap();
        assert!(tmux < screen && screen < shell);
        assert!(command.starts_with("sh -c '") && command.ends_with('\''));
        let end = end("tern-1a2b3c4d-1");
        assert!(end.contains("tmux kill-session -t tern-1a2b3c4d-1"));
        assert!(end.contains("screen -S tern-1a2b3c4d-1 -X quit"));
    }
}
