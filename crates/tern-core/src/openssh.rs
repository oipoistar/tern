//! OpenSSH's client config, `~/.ssh/config`: its hosts, for importing them.
//! Tern only reads the file.
//!
//! Each `Host` alias without wildcards becomes a host, with what OpenSSH
//! would use for it: the first value of each setting from every block that
//! matches it, `Host *` included. `Match` blocks can't be evaluated here and
//! are skipped; `Include` is followed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::forward::{Forward, ForwardKind};
use crate::host::{Auth, Host};
use crate::proxy::{Proxy, ProxyKind};
use crate::vault::Vault;

/// The group imported hosts land in.
pub const GROUP: &str = "OpenSSH";

/// How deep `Include` goes, as OpenSSH limits it.
const MAX_INCLUDE_DEPTH: usize = 16;

/// Settings that collect every value instead of keeping the first.
/// (`SetEnv` isn't one: OpenSSH sends the first line that applies.)
const ADDITIVE: [&str; 4] = [
    "identityfile",
    "localforward",
    "remoteforward",
    "dynamicforward",
];

/// A `Host` block (or a skipped `Match` one): its patterns and settings,
/// keywords lowercased.
#[derive(Debug, Default)]
struct Block {
    patterns: Vec<String>,
    /// `Match` blocks never apply here.
    matchable: bool,
    settings: Vec<(String, Vec<String>)>,
}

impl Block {
    fn matches(&self, alias: &str) -> bool {
        if !self.matchable {
            return false;
        }
        let alias = alias.to_lowercase();
        let mut matched = false;
        for pattern in &self.patterns {
            let (negated, pattern) = match pattern.strip_prefix('!') {
                Some(pattern) => (true, pattern),
                None => (false, pattern.as_str()),
            };
            if crate::certs::wildcard(&pattern.to_lowercase(), &alias) {
                if negated {
                    return false;
                }
                matched = true;
            }
        }
        matched
    }
}

/// Hosts in this computer's `~/.ssh/config` that aren't in the vault yet.
pub fn importable_here(vault: &Vault) -> Vec<Host> {
    let Some(dir) = crate::paths::ssh_dir() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(dir.join("config")) else {
        return Vec::new();
    };
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default();
    importable(vault, &text, &dir, &user)
}

/// The hosts `text` describes (`Include`s are read relative to `dir`, the
/// `~/.ssh` folder), minus those the vault already has at the same user,
/// address and port, named as [`new_hosts`] names them. `user` is the
/// login for hosts that name none, as `ssh` uses yours.
pub fn importable(vault: &Vault, text: &str, dir: &Path, user: &str) -> Vec<Host> {
    let blocks = parse(text, dir);
    new_hosts(vault, to_hosts(&blocks, dir, user), GROUP)
}

/// `hosts` minus those the vault, or one before them, already has at the
/// same user, address and port. A name another host uses, in any case,
/// becomes a free one: "web (OpenSSH)" for the `group` "OpenSSH", then
/// "web (OpenSSH) 2" and so on. Jump hosts (named as in `hosts`, where
/// no two hosts with jumps share a name) follow a rename, or lead to the
/// host that was already there.
pub(crate) fn new_hosts(vault: &Vault, hosts: Vec<Host>, group: &str) -> Vec<Host> {
    let mut renamed: HashMap<String, String> = HashMap::new();
    let mut kept: Vec<Host> = Vec::new();
    for mut host in hosts {
        let known = || vault.hosts().chain(kept.iter());
        if let Some(have) = known().find(|have| same_place(have, &host)) {
            renamed.insert(host.name.clone(), have.name.clone());
            continue;
        }
        let taken = |name: &str| known().any(|have| have.name.eq_ignore_ascii_case(name));
        let name = (1..)
            .map(|n| match n {
                1 => host.name.clone(),
                2 => format!("{} ({group})", host.name),
                n => format!("{} ({group}) {}", host.name, n - 1),
            })
            .find(|name| !taken(name))
            .expect("an unused name");
        if name != host.name {
            renamed.insert(host.name.clone(), name.clone());
            host.name = name;
        }
        kept.push(host);
    }
    for host in &mut kept {
        if let Some(jump) = host.jump.as_mut()
            && let Some(name) = renamed.get(jump)
        {
            *jump = name.clone();
        }
    }
    kept
}

/// Whether two hosts are reached as the same user at the same address and
/// port.
fn same_place(a: &Host, b: &Host) -> bool {
    a.user == b.user && a.address == b.address && a.port == b.port
}

/// The blocks of `text`, following `Include` into files under `dir`.
fn parse(text: &str, dir: &Path) -> Vec<Block> {
    // Lines before the first `Host` apply to every host.
    let mut blocks = vec![Block {
        patterns: vec!["*".into()],
        matchable: true,
        ..Block::default()
    }];
    read_lines(text, dir, &mut blocks, 0);
    blocks
}

fn read_lines(text: &str, dir: &Path, blocks: &mut Vec<Block>, depth: usize) {
    // Editors on Windows may start the file with a byte-order mark.
    for line in text.trim_start_matches('\u{feff}').lines() {
        let Some((keyword, arguments)) = split_line(line) else {
            continue;
        };
        let Some(arguments) = arguments else {
            // OpenSSH refuses a line whose quotes don't close; it sets
            // nothing here, and a `Host` line's block applies to no host.
            if keyword == "host" {
                blocks.push(Block::default());
            }
            continue;
        };
        match keyword.as_str() {
            "host" => blocks.push(Block {
                patterns: arguments,
                matchable: true,
                settings: Vec::new(),
            }),
            "match" => blocks.push(Block::default()),
            "include" if depth < MAX_INCLUDE_DEPTH => {
                for pattern in arguments {
                    for file in include_files(&pattern, dir) {
                        if let Ok(text) = std::fs::read_to_string(&file) {
                            read_lines(&text, dir, blocks, depth + 1);
                        }
                    }
                }
            }
            _ => {
                if let Some(block) = blocks.last_mut() {
                    block.settings.push((keyword, arguments));
                }
            }
        }
    }
}

/// A line's keyword (lowercased) and its arguments as [`words`] splits
/// them; `None` for blank lines and comments. The keyword ends at a space
/// or `=`.
fn split_line(line: &str) -> Option<(String, Option<Vec<String>>)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let keyword = line[..end].to_lowercase();
    let rest = line[end..].trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest).trim_start();
    Some((keyword, words(rest)))
}

/// Arguments as OpenSSH splits them: at spaces outside quotes ('…' or
/// "…"), with `\` before a quote, a backslash or (outside quotes) a space
/// taking that character as it is, any other `\` kept (`C:\tools`), and a
/// `#` that starts a word beginning a comment that ends the line. `None`
/// when a quote isn't closed.
fn words(text: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if(|&c| c == ' ' || c == '\t').is_some() {}
        if chars.peek().is_none_or(|&c| c == '#') {
            return Some(words);
        }
        let mut word = String::new();
        let mut quote = None;
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next_if(|&next| {
                    matches!(next, '\'' | '"' | '\\') || (next == ' ' && quote.is_none())
                }) {
                    Some(next) => word.push(next),
                    None => word.push(c),
                },
                ' ' | '\t' if quote.is_none() => break,
                '\'' | '"' if quote.is_none() => quote = Some(c),
                c if Some(c) == quote => quote = None,
                c => word.push(c),
            }
        }
        if quote.is_some() {
            return None;
        }
        words.push(word);
    }
}

/// The files an `Include` names: `~` is the home folder, relative names are
/// in `dir`, and `*` and `?` in the file's own name match files there.
fn include_files(pattern: &str, dir: &Path) -> Vec<PathBuf> {
    let path = expand_home(pattern);
    let path = if path.is_absolute() {
        path
    } else {
        dir.join(path)
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !name.contains(['*', '?']) {
        return vec![path];
    }
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|file| {
            file.file_name()
                .is_some_and(|file| crate::certs::wildcard(&name, &file.to_string_lossy()))
        })
        .collect();
    files.sort();
    files
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ if path == "~" => std::env::home_dir().unwrap_or_default(),
        _ => PathBuf::from(path),
    }
}

/// What OpenSSH uses for `alias`: the first value of each setting (all of
/// them for additive ones) from the blocks that match, in file order.
fn resolve<'a>(blocks: &'a [Block], alias: &str) -> HashMap<&'a str, Vec<&'a [String]>> {
    let mut found: HashMap<&str, Vec<&[String]>> = HashMap::new();
    for block in blocks.iter().filter(|block| block.matches(alias)) {
        for (keyword, arguments) in &block.settings {
            let values = found.entry(keyword.as_str()).or_default();
            if values.is_empty() || ADDITIVE.contains(&keyword.as_str()) {
                values.push(arguments);
            }
        }
    }
    found
}

/// Every alias in `blocks` that names one host (no wildcards, not negated),
/// as a host; jump hosts named only in a `ProxyJump` are added too.
fn to_hosts(blocks: &[Block], dir: &Path, user: &str) -> Vec<Host> {
    let mut aliases: Vec<&str> = Vec::new();
    for block in blocks.iter().filter(|block| block.matchable) {
        for pattern in &block.patterns {
            let concrete = !pattern.contains(['*', '?', '!']);
            if concrete
                && !aliases
                    .iter()
                    .any(|have| have.eq_ignore_ascii_case(pattern))
            {
                aliases.push(pattern);
            }
        }
    }
    let mut hosts: Vec<Host> = Vec::new();
    let mut extra: Vec<Host> = Vec::new();
    for alias in &aliases {
        let settings = resolve(blocks, alias);
        let first = |keyword: &str| -> Option<&str> {
            settings
                .get(keyword)
                .and_then(|values| values.first())
                .and_then(|arguments| arguments.first())
                .map(String::as_str)
        };
        let login = first("user").unwrap_or(user).trim();
        if login.is_empty() {
            continue;
        }
        let address = first("hostname").map_or_else(
            || alias.to_string(),
            |name| name.replace("%h", alias).replace("%%", "%"),
        );
        let port = first("port")
            .and_then(|port| port.parse::<u16>().ok())
            .filter(|&port| port > 0)
            .unwrap_or(22);
        let mut host = Host::new(alias, login, &address, port);
        let tokens = |path: &str| {
            let home = std::env::home_dir().unwrap_or_default();
            let expanded = path
                .replace("%d", &home.to_string_lossy())
                .replace("%u", user)
                .replace("%r", login)
                .replace("%h", &address)
                .replace("%%", "%");
            let path = expand_home(&expanded);
            if path.is_absolute() {
                path
            } else {
                dir.join(path)
            }
        };
        host.auth = match settings
            .get("identityfile")
            .and_then(|values| values.first())
        {
            Some(arguments) if arguments.first().is_some_and(|path| path != "none") => {
                Auth::Key(tokens(&arguments[0]))
            }
            _ if first("preferredauthentications")
                .is_some_and(|methods| methods.starts_with("password")) =>
            {
                Auth::Password
            }
            _ => Auth::Agent,
        };
        host.agent_forwarding =
            first("forwardagent").is_some_and(|value| value.eq_ignore_ascii_case("yes"));
        if let Some(interval) =
            first("serveraliveinterval").and_then(|value| value.parse::<u32>().ok())
        {
            host.keep_alive = interval > 0;
        }
        for (keyword, kind) in [
            ("localforward", ForwardKind::Local),
            ("remoteforward", ForwardKind::Remote),
            ("dynamicforward", ForwardKind::Dynamic),
        ] {
            for arguments in settings.get(keyword).into_iter().flatten() {
                // Tern listens on loopback only, whatever address is given.
                let port = arguments
                    .first()
                    .map(|listen| listen.rsplit(':').next().unwrap_or(listen))
                    .unwrap_or_default();
                let to = arguments.get(1).map(String::as_str).unwrap_or_default();
                if let Ok(mut forward) = Forward::typed(kind, port, to) {
                    forward.on = true;
                    host.forwards.push(forward);
                }
            }
        }
        let pairs = settings
            .get("setenv")
            .and_then(|values| values.first())
            .copied()
            .unwrap_or_default();
        for (name, value) in pairs.iter().filter_map(|pair| pair.split_once('=')) {
            // Of a name given twice, OpenSSH sends the first value. The
            // value goes to `env::parse` in quotes, where `\` escapes.
            let quoted = value.replace('\\', "\\\\").replace('"', "\\\"");
            if !host.env.contains_key(name)
                && let Ok(parsed) = crate::env::parse(&format!("{name}=\"{quoted}\""))
            {
                host.env.extend(parsed);
            }
        }
        let proxy_command = settings
            .get("proxycommand")
            .and_then(|values| values.first())
            .copied();
        if let Some(proxy) = proxy_command.and_then(proxy_of_command) {
            host.proxy = Some(proxy);
        }
        let jumps: Vec<String> = match first("proxyjump") {
            Some(list) if !list.eq_ignore_ascii_case("none") => {
                list.split(',').map(|hop| hop.trim().to_string()).collect()
            }
            // `ProxyCommand ssh -W %h:%p bastion` is the older spelling.
            _ => proxy_command
                .and_then(jump_of_command)
                .into_iter()
                .collect(),
        };
        // `a,b`: b goes through a, the host through b.
        let mut previous: Option<String> = None;
        for hop in jumps.iter().filter(|hop| !hop.is_empty()) {
            let name = match aliases.iter().find(|alias| alias.eq_ignore_ascii_case(hop)) {
                Some(alias) => alias.to_string(),
                None => {
                    let (hop_user, hop_rest) = match hop.rsplit_once('@') {
                        Some((hop_user, rest)) => (hop_user, rest),
                        None => (login, hop.as_str()),
                    };
                    let (hop_address, hop_port) = match hop_rest.rsplit_once(':') {
                        Some((address, port)) if !address.contains(':') => {
                            (address, port.parse::<u16>().unwrap_or(22))
                        }
                        _ => (hop_rest, 22),
                    };
                    let hop_address = hop_address.trim_start_matches('[').trim_end_matches(']');
                    if !extra.iter().any(|have| have.name == hop_address) {
                        extra.push(Host::new(hop_address, hop_user, hop_address, hop_port));
                    }
                    hop_address.to_string()
                }
            };
            if let Some(previous) = &previous
                && let Some(hop_host) = extra.iter_mut().find(|have| have.name == name)
                && hop_host.jump.is_none()
            {
                hop_host.jump = Some(previous.clone());
            }
            previous = Some(name);
        }
        host.jump = previous.filter(|via| !via.eq_ignore_ascii_case(alias));
        hosts.push(host);
    }
    hosts.extend(extra);
    hosts
}

/// `host` as one `ssh` command line, for a terminal without Tern: its
/// port, key, jump hosts (`-J`), agent forwarding, proxy and the rules
/// switched on. A key path with spaces is quoted.
pub fn ssh_command(vault: &Vault, host: &Host) -> String {
    let (user, auth, _) = vault.sign_in(host);
    let mut words = vec!["ssh".to_string()];
    if host.port != 22 {
        words.push(format!("-p {}", host.port));
    }
    if let Auth::Key(path) = &auth {
        words.push(format!("-i {}", quote(&path.display().to_string())));
    }
    let jumps = jump_list(vault, host);
    if !jumps.is_empty() {
        words.push(format!("-J {jumps}"));
    }
    if host.agent_forwarding {
        words.push("-A".into());
    }
    if let Some(proxy) = &vault.first_hop_host(host).proxy {
        words.push(format!(
            "-o {}",
            quote(&format!("ProxyCommand={}", proxy_command(proxy)))
        ));
    }
    for forward in host.forwards.iter().filter(|forward| forward.on) {
        words.push(format!("{} {}", forward.flag(), forward_spec(forward)));
    }
    words.push(format!("{user}@{}", host.address));
    words.join(" ")
}

/// `host` as a `Host` block for `~/.ssh/config`, which the importer reads
/// back as the same host.
pub fn config_block(vault: &Vault, host: &Host) -> String {
    let (user, auth, _) = vault.sign_in(host);
    let alias: String = host
        .name
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .collect();
    let mut lines = vec![
        format!("Host {alias}"),
        format!("    HostName {}", host.address),
        format!("    User {}", config_quote(&user)),
    ];
    if host.port != 22 {
        lines.push(format!("    Port {}", host.port));
    }
    if let Auth::Key(path) = &auth {
        lines.push(format!(
            "    IdentityFile {}",
            config_quote(&path.display().to_string())
        ));
    }
    let jumps = jump_list(vault, host);
    if !jumps.is_empty() {
        lines.push(format!("    ProxyJump {jumps}"));
    } else if let Some(proxy) = &host.proxy {
        lines.push(format!("    ProxyCommand {}", proxy_command(proxy)));
    }
    if host.agent_forwarding {
        lines.push("    ForwardAgent yes".into());
    }
    lines.push(format!(
        "    ServerAliveInterval {}",
        if host.keep_alive { 30 } else { 0 }
    ));
    for forward in host.forwards.iter().filter(|forward| forward.on) {
        let keyword = match forward.kind {
            ForwardKind::Local => "LocalForward",
            ForwardKind::Remote => "RemoteForward",
            ForwardKind::Dynamic => "DynamicForward",
        };
        // `port host:port`, as the config writes it.
        lines.push(format!(
            "    {keyword} {}",
            forward_spec(forward).replacen(':', " ", 1)
        ));
    }
    if !host.env.is_empty() {
        let pairs: Vec<String> = host
            .env
            .iter()
            .map(|(name, value)| format!("{name}={}", config_quote(value)))
            .collect();
        lines.push(format!("    SetEnv {}", pairs.join(" ")));
    }
    lines.join("\n") + "\n"
}

/// `host`'s jump hosts as `-J` and `ProxyJump` take them: `user@host:port`,
/// first hop first, comma-separated.
fn jump_list(vault: &Vault, host: &Host) -> String {
    vault
        .jumps(host)
        .unwrap_or_default()
        .into_iter()
        .map(|via| {
            let (user, _, _) = vault.sign_in(via);
            if via.port == 22 {
                format!("{user}@{}", via.address)
            } else {
                format!("{user}@{}:{}", via.address, via.port)
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// A rule as `-L`/`-R`/`-D` take it: `port:host:port`, or the port alone.
fn forward_spec(forward: &Forward) -> String {
    match forward.kind {
        ForwardKind::Dynamic => forward.listen_port.to_string(),
        _ => format!(
            "{}:{}:{}",
            forward.listen_port, forward.to_host, forward.to_port
        ),
    }
}

/// `nc` reaching a host through `proxy`, as `ProxyCommand` runs it.
fn proxy_command(proxy: &Proxy) -> String {
    let kind = match proxy.kind {
        ProxyKind::Socks5 => "5",
        ProxyKind::Http => "connect",
    };
    format!("nc -X {kind} -x {}:{} %h %p", proxy.host, proxy.port)
}

/// `text` in double quotes when it has spaces.
fn quote(text: &str) -> String {
    if text.contains(char::is_whitespace) {
        format!("\"{text}\"")
    } else {
        text.to_string()
    }
}

/// `text` as one argument of a config line, which OpenSSH (and [`words`])
/// reads back as it is: in double quotes when it's empty or has spaces,
/// quotes or a `\` that would be read as an escape, with `"` and those `\`
/// escaped. Other backslashes stay single, as in `C:\Users\me\.ssh\id`.
fn config_quote(text: &str) -> String {
    let mut plain = !text.is_empty() && !text.starts_with('#');
    let mut quoted = String::from("\"");
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                plain = false;
                quoted.push_str("\\\"");
            }
            '\\' if matches!(chars.peek(), None | Some('\'' | '"' | '\\' | ' ')) => {
                plain = false;
                quoted.push_str("\\\\");
            }
            c => {
                plain &= !c.is_whitespace() && c != '\'';
                quoted.push(c);
            }
        }
    }
    quoted.push('"');
    if plain { text.to_string() } else { quoted }
}

/// The proxy an `nc`/`ncat` `ProxyCommand` goes through: `-X 5` (or none)
/// is SOCKS5 and `-X connect` HTTP, with the proxy after `-x`.
fn proxy_of_command(words: &[String]) -> Option<Proxy> {
    let program = words.first()?.rsplit(['/', '\\']).next()?;
    if !matches!(program, "nc" | "ncat" | "netcat") {
        return None;
    }
    let after = |flag: &str| {
        words
            .iter()
            .position(|word| word == flag)
            .and_then(|at| words.get(at + 1))
    };
    let kind = match after("-X").map(String::as_str) {
        None | Some("5") => ProxyKind::Socks5,
        Some("connect") => ProxyKind::Http,
        Some(_) => return None,
    };
    let scheme = match kind {
        ProxyKind::Socks5 => "socks5",
        ProxyKind::Http => "http",
    };
    Proxy::parse(&format!("{scheme}://{}", after("-x")?)).ok()
}

/// The jump host of `ProxyCommand ssh [options] -W %h:%p host`.
fn jump_of_command(words: &[String]) -> Option<String> {
    let program = words.first()?.rsplit(['/', '\\']).next()?;
    if !program.trim_end_matches(".exe").eq("ssh") || !words.iter().any(|word| word == "-W") {
        return None;
    }
    words
        .last()
        .filter(|last| !last.starts_with('-') && !last.contains('%'))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
# Defaults first, as many configs have them.
ServerAliveInterval 0

Host bastion
    HostName bastion.example.com
    User admin
    Port 2222
    IdentityFile ~/.ssh/id_bastion

Host db db-alias
    HostName=10.0.4.21
    ProxyJump bastion
    LocalForward 127.0.0.1:5433 localhost:5432
    DynamicForward 1080
    SetEnv LANG=C.UTF-8 "GREETING=hello world"
    ForwardAgent yes

Host legacy
    HostName 10.0.9.9
    ProxyCommand ssh -q -W %h:%p bastion

Host behind-proxy
    HostName internal.lan
    ProxyCommand nc -X connect -x proxy.lan:3128 %h %p

Host far
    HostName far.example.com
    ProxyJump ops@gw.example.com:2200,bastion

Host *.example.com !skip.example.com
    User wild

Match host other
    User nobody

Host *
    User me
    ServerAliveInterval 30
"#;

    fn hosts() -> Vec<Host> {
        importable(
            &Vault::default(),
            CONFIG,
            Path::new("/home/me/.ssh"),
            "fallback",
        )
    }

    fn find<'a>(hosts: &'a [Host], name: &str) -> &'a Host {
        hosts
            .iter()
            .find(|host| host.name == name)
            .unwrap_or_else(|| panic!("{name} wasn't imported"))
    }

    #[test]
    fn imports_what_openssh_would_use() {
        let hosts = hosts();
        let bastion = find(&hosts, "bastion");
        assert_eq!(bastion.target(), "admin@bastion.example.com:2222");
        assert!(matches!(&bastion.auth, Auth::Key(path) if path.ends_with("id_bastion")));
        // The first value wins: the defaults said 0 before `Host *` said 30.
        assert!(!bastion.keep_alive);

        let db = find(&hosts, "db");
        // `User me` from `Host *`, since nothing earlier names a user.
        assert_eq!(db.target(), "me@10.0.4.21");
        assert_eq!(db.jump.as_deref(), Some("bastion"));
        assert!(db.agent_forwarding);
        let routes: Vec<String> = db.forwards.iter().map(Forward::route).collect();
        assert_eq!(
            routes,
            ["127.0.0.1:5433 → localhost:5432", "127.0.0.1:1080"]
        );
        assert!(db.forwards.iter().all(|forward| forward.on));
        assert_eq!(db.env["LANG"], "C.UTF-8");
        assert_eq!(db.env["GREETING"], "hello world");
        // A second alias of the same block is the same place: imported once.
        assert!(hosts.iter().all(|host| host.name != "db-alias"));
    }

    #[test]
    fn reads_old_style_jumps_proxies_and_chains() {
        let hosts = hosts();
        assert_eq!(find(&hosts, "legacy").jump.as_deref(), Some("bastion"));
        let proxied = find(&hosts, "behind-proxy");
        assert_eq!(
            proxied.proxy.as_ref().map(ToString::to_string).as_deref(),
            Some("http://proxy.lan:3128")
        );
        // `gw,bastion`: far through bastion; gw is new and bastion is
        // already a host, so the chain starts there.
        let far = find(&hosts, "far");
        assert_eq!(far.jump.as_deref(), Some("bastion"));
        let gw = find(&hosts, "gw.example.com");
        assert_eq!(gw.target(), "ops@gw.example.com:2200");
        // Wildcards and Match blocks aren't hosts.
        assert!(hosts.iter().all(|host| !host.name.contains('*')));
        assert!(hosts.iter().all(|host| host.name != "other"));
    }

    #[test]
    fn leaves_out_comments_after_arguments() {
        let config = r#"
Host build01.lan  # CI runner
    User ci # the robot
    ProxyCommand ssh -W %h:%p bastion # old
Host bastion
    HostName 10.0.0.1 #office
    SetEnv "NOTE=#1 # not a comment" TAG=a#b # a comment
Host "unclosed
    User nobody
"#;
        let hosts = importable(&Vault::default(), config, Path::new("."), "me");
        let names: Vec<&str> = hosts.iter().map(|host| host.name.as_str()).collect();
        assert_eq!(names, ["build01.lan", "bastion"]);
        let build = find(&hosts, "build01.lan");
        assert_eq!(build.target(), "ci@build01.lan");
        assert_eq!(build.jump.as_deref(), Some("bastion"));
        let bastion = find(&hosts, "bastion");
        assert_eq!(bastion.target(), "me@10.0.0.1");
        assert_eq!(bastion.env["NOTE"], "#1 # not a comment");
        assert_eq!(bastion.env["TAG"], "a#b");
        assert_eq!(bastion.env.len(), 2);
    }

    #[test]
    fn splits_arguments_as_openssh_does() {
        let split = |text: &str| words(text).map(|words| words.join("|"));
        assert_eq!(
            split(r#"a "b c" 'd "e"' f\ g"#).as_deref(),
            Some(r#"a|b c|d "e"|f g"#)
        );
        assert_eq!(
            split(r#"x=\"q\" C:\tools 'it\'s' "a\\b""#).as_deref(),
            Some(r#"x="q"|C:\tools|it's|a\b"#)
        );
        assert_eq!(split("one #two three").as_deref(), Some("one"));
        assert_eq!(split(r##"a#b "#c""##).as_deref(), Some("a#b|#c"));
        assert_eq!(split("'open"), None);
    }

    #[test]
    fn sets_the_environment_openssh_would() {
        let config = r#"
Host tools
    SetEnv TOOLS=C:\tools "WIN=C:\\Program Files" LANG=C LANG=fr
    SetEnv LATER=1
Host *
    SetEnv LANG=de EXTRA=1
"#;
        let hosts = importable(&Vault::default(), config, Path::new("."), "me");
        // The first SetEnv line that applies, and a name's first value.
        let env = &find(&hosts, "tools").env;
        assert_eq!(env.len(), 3, "{env:?}");
        assert_eq!(env["TOOLS"], r"C:\tools");
        assert_eq!(env["WIN"], r"C:\Program Files");
        assert_eq!(env["LANG"], "C");
    }

    #[test]
    fn writes_values_openssh_reads_back_as_they_are() {
        let mut vault = Vault::default();
        let mut host = Host::new("tools", "John Smith", "tools.lan", 22);
        host.auth = Auth::Key(PathBuf::from(r"C:\Users\O'Brien\.ssh\id key"));
        for (name, value) in [
            ("TOOLS", r"C:\tools"),
            ("SHARE", r"\\server\share"),
            ("SAY", r#"say "hi""#),
            ("IT", "it's"),
            ("EMPTY", ""),
            ("WORDS", "two words"),
            ("END", r"ends\"),
        ] {
            host.env.insert(name.into(), value.into());
        }
        vault.save(None, host.clone(), "Mine");
        let block = config_block(&vault, &host);
        assert!(block.contains(r" TOOLS=C:\tools "), "{block}");

        let read = importable(&Vault::default(), &block, Path::new("."), "someone");
        let again = find(&read, "tools");
        assert_eq!(again.user, "John Smith");
        assert_eq!(again.env, host.env);
        assert!(
            matches!(&again.auth, Auth::Key(path)
                if path.to_string_lossy().ends_with(r"\O'Brien\.ssh\id key")),
            "{:?}",
            again.auth
        );
    }

    #[test]
    fn leaves_out_what_the_vault_has_and_renames_clashes() {
        let mut vault = Vault::default();
        vault.save(None, Host::new("bastion", "x", "elsewhere", 22), "Mine");
        vault.save(None, Host::new("my db", "me", "10.0.4.21", 22), "Mine");
        let hosts = importable(&vault, CONFIG, Path::new("/home/me/.ssh"), "fallback");
        let bastion = find(&hosts, "bastion (OpenSSH)");
        assert_eq!(bastion.address, "bastion.example.com");
        // db is already in the vault as "my db"; legacy's jump follows the
        // rename.
        assert!(hosts.iter().all(|host| host.address != "10.0.4.21"));
        assert_eq!(
            find(&hosts, "legacy").jump.as_deref(),
            Some("bastion (OpenSSH)")
        );
    }

    #[test]
    fn picks_a_free_name_for_each_clash() {
        let mut vault = Vault::default();
        vault.save(None, Host::new("web", "a", "one.lan", 22), "Mine");
        vault.save(None, Host::new("WEB (OpenSSH)", "b", "two.lan", 22), "Mine");
        let config = "Host web\n  HostName three.lan\n  User c\n\
                      Host app\n  HostName app.lan\n  User c\n  ProxyJump web\n";
        let hosts = importable(&vault, config, Path::new("."), "me");
        assert_eq!(find(&hosts, "web (OpenSSH) 2").target(), "c@three.lan");
        assert_eq!(find(&hosts, "app").jump.as_deref(), Some("web (OpenSSH) 2"));

        // Once imported, that name is taken too.
        for host in hosts {
            vault.save(None, host, GROUP);
        }
        let hosts = importable(
            &vault,
            "Host web\n  HostName four.lan\n",
            Path::new("."),
            "c",
        );
        let names: Vec<&str> = hosts.iter().map(|host| host.name.as_str()).collect();
        assert_eq!(names, ["web (OpenSSH) 3"]);
    }

    #[test]
    fn writes_hosts_back_for_openssh() {
        let mut vault = Vault::default();
        let mut bastion = Host::new("bastion", "admin", "bastion.example.com", 2222);
        bastion.auth = Auth::Agent;
        vault.save(None, bastion, "Prod");
        let mut db = Host::new("prod db", "deploy", "10.0.4.21", 22);
        db.jump = Some("bastion".into());
        db.agent_forwarding = true;
        db.keep_alive = false;
        db.env = crate::env::parse("LANG=C.UTF-8").unwrap();
        let mut rule = Forward::typed(ForwardKind::Local, "5433", "localhost:5432").unwrap();
        rule.on = true;
        db.forwards.push(rule);
        vault.save(None, db.clone(), "Prod");

        assert_eq!(
            ssh_command(&vault, &db),
            "ssh -J admin@bastion.example.com:2222 -A -L 5433:localhost:5432 deploy@10.0.4.21"
        );
        let block = config_block(&vault, &db);
        assert!(
            block.starts_with("Host prod-db\n    HostName 10.0.4.21\n"),
            "{block}"
        );
        assert!(
            block.contains("    ProxyJump admin@bastion.example.com:2222\n"),
            "{block}"
        );
        assert!(
            block.contains("    LocalForward 5433 localhost:5432\n"),
            "{block}"
        );

        // Read back, it's the same host.
        let read = importable(&Vault::default(), &block, Path::new("."), "someone");
        let again = read.iter().find(|host| host.name == "prod-db").unwrap();
        assert_eq!(again.target(), "deploy@10.0.4.21");
        assert!(again.agent_forwarding && !again.keep_alive);
        assert_eq!(again.env, db.env);
        assert_eq!(again.forwards.len(), 1);
        assert_eq!(again.jump.as_deref(), Some("bastion.example.com"));
    }

    #[test]
    fn follows_includes() {
        let dir = std::env::temp_dir().join(format!("tern-ssh-config-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("config.d")).unwrap();
        std::fs::write(
            dir.join("config.d").join("work.conf"),
            "Host work\n  HostName w.lan\n",
        )
        .unwrap();
        let text = "\u{feff}Include config.d/*.conf\nHost home\n  HostName h.lan\n";
        let hosts = importable(&Vault::default(), text, &dir, "me");
        let names: Vec<&str> = hosts.iter().map(|host| host.name.as_str()).collect();
        assert_eq!(names, ["work", "home"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
