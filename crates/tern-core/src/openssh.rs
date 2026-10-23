//! OpenSSH's client config, `~/.ssh/config`: its hosts, for importing them.
//! Tern only reads the file.
//!
//! Each `Host` alias without wildcards becomes a host, with what OpenSSH
//! would use for it: the first value of each setting from every block that
//! matches it, `Host *` included. `Match` blocks can't be evaluated here and
//! are skipped; `Include` is followed. A jump host (`ProxyJump`, or `ssh -W`
//! in a `ProxyCommand`) is what the `ssh` that dials it would reach, from
//! the blocks for the name it's given, and becomes a host too when no
//! alias is the same place.

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
/// same user, address and port, reached the same way. A name another host
/// uses, in any case,
/// becomes a free one: "web (OpenSSH)" for the `group` "OpenSSH", then
/// "web (OpenSSH) 2" and so on. A jump to another of `hosts`, by a name
/// no other of them has, follows its rename, or leads to the host that
/// was already there.
pub(crate) fn new_hosts(vault: &Vault, hosts: Vec<Host>, group: &str) -> Vec<Host> {
    let mut renamed: HashMap<String, String> = HashMap::new();
    let mut kept: Vec<Host> = Vec::new();
    for mut host in hosts {
        let known = || vault.hosts().chain(kept.iter());
        if let Some(have) = known().find(|have| same_place(have, &host) && same_way(have, &host)) {
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

/// Whether two hosts are reached as the same user at the same address (in
/// any case) and port.
fn same_place(a: &Host, b: &Host) -> bool {
    a.user == b.user && a.address.eq_ignore_ascii_case(&b.address) && a.port == b.port
}

/// Whether two hosts are reached the same way: through the same proxy, and
/// both directly or both through a jump host. Aliases for one machine that
/// get there differently ("db" and "db-from-home") are both kept.
fn same_way(a: &Host, b: &Host) -> bool {
    a.proxy == b.proxy && a.jump.is_some() == b.jump.is_some()
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

/// What OpenSSH uses for a host, keyed by lowercased keyword: the
/// arguments of each setting's first line (of every line for additive
/// ones).
type Settings<'a> = HashMap<&'a str, Vec<&'a [String]>>;

/// What OpenSSH uses for `name` (an alias, or an address as given): the
/// first value of each setting (all of them for additive ones) from the
/// blocks that match, in file order. As in OpenSSH, a `ProxyCommand` also
/// keeps any later `ProxyJump` out, and a `ProxyJump` other than `none` a
/// later `ProxyCommand`.
fn resolve<'a>(blocks: &'a [Block], name: &str) -> Settings<'a> {
    let mut found: Settings = HashMap::new();
    for block in blocks.iter().filter(|block| block.matches(name)) {
        for (keyword, arguments) in &block.settings {
            let keyword = keyword.as_str();
            let kept_out = match keyword {
                "proxyjump" => found.contains_key("proxycommand"),
                "proxycommand" => found
                    .get("proxyjump")
                    .and_then(|values| values.first())
                    .and_then(|arguments| arguments.first())
                    .is_some_and(|jumps| !jumps.eq_ignore_ascii_case("none")),
                _ => false,
            };
            if kept_out {
                continue;
            }
            let values = found.entry(keyword).or_default();
            if values.is_empty() || ADDITIVE.contains(&keyword) {
                values.push(arguments);
            }
        }
    }
    found
}

/// How far the jump hosts of a jump host are followed, in case a config
/// sends them round in circles.
const MAX_JUMP_DEPTH: usize = 16;

/// How a host is reached.
#[derive(Clone, Debug, Default)]
enum Route {
    #[default]
    Direct,
    Proxy(Proxy),
    /// Through jump hosts, first hop first: each later one through the one
    /// before it, the first its own way.
    Jumps(Vec<Hop>),
}

/// A jump host as a `ProxyJump` hop or an `ssh` in a `ProxyCommand` names
/// it. What the line doesn't say comes from the config's blocks for its
/// `host`, as the `ssh` that dials it reads them.
#[derive(Clone, Debug, Default)]
struct Hop {
    /// An alias, or an address as given.
    host: String,
    user: Option<String>,
    port: Option<u16>,
    /// A key file, as `-i` gives it.
    key: Option<String>,
    /// How to reach it, when the line says.
    route: Option<Route>,
}

/// The hosts one config describes, as they're found.
struct Import<'a> {
    blocks: &'a [Block],
    dir: &'a Path,
    /// The login for hosts that name none.
    user: &'a str,
    hosts: Vec<Host>,
}

impl Import<'_> {
    /// The host `ssh` reaches for `hop`, and how it gets there: as the hop
    /// says, otherwise as the config's blocks for it do. `None` when it has
    /// no login.
    fn host(&self, hop: &Hop) -> Option<(Host, Route)> {
        let (name, dir, user) = (hop.host.as_str(), self.dir, self.user);
        let settings = resolve(self.blocks, name);
        let first = |keyword: &str| -> Option<&str> {
            settings
                .get(keyword)
                .and_then(|values| values.first())
                .and_then(|arguments| arguments.first())
                .map(String::as_str)
        };
        let login = hop.user.as_deref().or(first("user")).unwrap_or(user).trim();
        if login.is_empty() {
            return None;
        }
        let address = first("hostname").map_or_else(
            || name.to_string(),
            |hostname| hostname.replace("%h", name).replace("%%", "%"),
        );
        let port = hop
            .port
            .or_else(|| first("port").and_then(port_number))
            .unwrap_or(22);
        let mut host = Host::new(name, login, &address, port);
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
        host.auth = match hop.key.as_deref().or_else(|| first("identityfile")) {
            Some(path) if path != "none" => Auth::Key(tokens(path)),
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
        let route = match &hop.route {
            Some(route) => route.clone(),
            None => config_route(&settings),
        };
        if let Route::Proxy(proxy) = &route {
            host.proxy = Some(proxy.clone());
        }
        Some((host, route))
    }

    /// The name of the host to jump through for `route` (its last hop),
    /// adding the hops this import lacks. A new later hop goes through the
    /// one before it, a new first hop its own way; a host already found
    /// keeps its own.
    fn jump(&mut self, route: Route, depth: usize) -> Option<String> {
        let Route::Jumps(hops) = route else {
            return None;
        };
        if depth > MAX_JUMP_DEPTH {
            return None;
        }
        let mut previous: Option<String> = None;
        for hop in hops {
            let (mut host, own) = self.host(&hop)?;
            if let Some(have) = self.hosts.iter().find(|have| same_place(have, &host)) {
                previous = Some(have.name.clone());
                continue;
            }
            host.name = self.hop_name(&hop);
            let name = host.name.clone();
            self.hosts.push(host);
            let index = self.hosts.len() - 1;
            let via = match previous {
                Some(previous) => Some(previous),
                None => self.jump(own, depth + 1),
            };
            self.hosts[index].jump = via.filter(|jump| *jump != name);
            previous = Some(name);
        }
        previous
    }

    /// A name for a jump host no alias names: its host as the hop gives
    /// it, or with the hop's user and port when another host has that
    /// name, or else numbered.
    fn hop_name(&self, hop: &Hop) -> String {
        let mut written = hop.host.clone();
        if let Some(user) = &hop.user {
            written = format!("{user}@{written}");
        }
        if let Some(port) = hop.port {
            written = format!("{written}:{port}");
        }
        let taken = |name: &str| {
            self.hosts
                .iter()
                .any(|have| have.name.eq_ignore_ascii_case(name))
        };
        [hop.host.clone(), written]
            .into_iter()
            .chain((2..).map(|n| format!("{} {n}", hop.host)))
            .find(|name| !taken(name))
            .expect("an unused name")
    }
}

/// Every alias in `blocks` that names one host (no wildcards, not negated),
/// as a host, then the jump hosts they go through that no alias names. No
/// two share a name, in any case.
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
    let mut import = Import {
        blocks,
        dir,
        user,
        hosts: Vec::new(),
    };
    let mut routes = Vec::new();
    for alias in aliases {
        let hop = Hop {
            host: alias.to_string(),
            ..Hop::default()
        };
        if let Some((host, route)) = import.host(&hop) {
            import.hosts.push(host);
            routes.push(route);
        }
    }
    // Every alias is in first, so a jump host that is one is found as it.
    for (index, route) in routes.into_iter().enumerate() {
        let via = import.jump(route, 0);
        let host = &mut import.hosts[index];
        host.jump = via.filter(|name| *name != host.name);
    }
    import.hosts
}

/// How the config says to reach a host: its `ProxyJump`, or its
/// `ProxyCommand` when that's `nc` or `ssh -W`; otherwise directly.
fn config_route(settings: &Settings) -> Route {
    let first = |keyword: &str| settings.get(keyword).and_then(|values| values.first());
    match first("proxyjump").and_then(|arguments| arguments.first()) {
        Some(list) if !list.eq_ignore_ascii_case("none") => proxy_jump(list).unwrap_or_default(),
        _ => first("proxycommand")
            .and_then(|words| command_route(words))
            .unwrap_or_default(),
    }
}

/// The jump hosts of a `ProxyJump` value or `-J`: hops separated by
/// commas, or `none`. `None` when a hop isn't one.
fn proxy_jump(text: &str) -> Option<Route> {
    if text.eq_ignore_ascii_case("none") {
        return Some(Route::Direct);
    }
    text.split(',')
        .map(hop)
        .collect::<Option<Vec<Hop>>>()
        .map(Route::Jumps)
}

/// A `ProxyJump` hop: `[user@]host[:port]`, an IPv6 address in brackets,
/// or `ssh://[user@]host[:port]`.
fn hop(text: &str) -> Option<Hop> {
    let text = match text.strip_prefix("ssh://") {
        Some(url) => url.strip_suffix('/').unwrap_or(url),
        None => text,
    };
    let (user, rest) = match text.rsplit_once('@') {
        Some((user, rest)) => (Some(user), rest),
        None => (None, text),
    };
    let (host, port) = match rest.strip_prefix('[') {
        Some(bracketed) => match bracketed.split_once(']')? {
            (host, "") => (host, None),
            (host, after) => (host, Some(after.strip_prefix(':')?)),
        },
        None => match rest.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        },
    };
    let port = match port {
        Some(port) => Some(port_number(port)?),
        None => None,
    };
    if host.is_empty() || user == Some("") {
        return None;
    }
    Some(Hop {
        host: host.to_string(),
        user: user.map(str::to_string),
        port,
        ..Hop::default()
    })
}

/// A port from 1 to 65535.
fn port_number(text: &str) -> Option<u16> {
    text.parse::<u16>().ok().filter(|&port| port > 0)
}

/// `host` as one `ssh` command line, for a terminal without Tern: its
/// port, key, jump hosts (`-J`), agent forwarding, proxy and the rules
/// switched on. A key path with spaces is quoted. OpenSSH ignores a
/// `ProxyCommand` beside `-J`, so when the first jump host is behind a
/// proxy, the jump hosts are a `ProxyCommand` too (see [`through`]).
pub fn ssh_command(vault: &Vault, host: &Host) -> String {
    let (user, auth, _) = vault.sign_in(host);
    let mut words = vec!["ssh".to_string()];
    if host.port != 22 {
        words.push(format!("-p {}", host.port));
    }
    if let Auth::Key(path) = &auth {
        words.push(format!("-i {}", quote(&path.display().to_string())));
    }
    let jumps = vault.jumps(host).unwrap_or_default();
    let proxy = jumps.first().copied().unwrap_or(host).proxy.as_ref();
    if proxy.is_none() && !jumps.is_empty() {
        words.push(format!("-J {}", jump_list(vault, &jumps)));
    }
    if host.agent_forwarding {
        words.push("-A".into());
    }
    if let Some(proxy) = proxy {
        words.push(format!(
            "-o {}",
            quote(&format!("ProxyCommand={}", through(vault, &jumps, proxy)))
        ));
    }
    for forward in host.forwards.iter().filter(|forward| forward.on) {
        words.push(format!("{} {}", forward.flag(), forward_spec(forward)));
    }
    words.push(format!("{user}@{}", host.address));
    words.join(" ")
}

/// A `ProxyCommand` that reaches a host behind `hops` (first hop first),
/// the first of them through `proxy`: `ssh -W` to the last hop, with a
/// `ProxyCommand` of its own that reaches it the same way, its `%` doubled
/// as it's expanded once more. Without hops, `nc` through the proxy.
fn through(vault: &Vault, hops: &[&Host], proxy: &Proxy) -> String {
    let Some((last, before)) = hops.split_last() else {
        return proxy_command(proxy);
    };
    let inner = through(vault, before, proxy).replace('%', "%%");
    let (user, auth, _) = vault.sign_in(last);
    let mut words = vec![
        "ssh -o".to_string(),
        sh_quote(&format!("ProxyCommand={inner}")),
    ];
    if last.port != 22 {
        words.push(format!("-p {}", last.port));
    }
    if let Auth::Key(path) = &auth {
        words.push(format!("-i {}", sh_quote(&path.display().to_string())));
    }
    words.push(format!("-W {}", sh_quote("[%h]:%p")));
    words.push(sh_quote(&format!("{user}@{}", last.address)));
    words.join(" ")
}

/// `host` as a `Host` block for `~/.ssh/config`, then one for each host it
/// goes through, nearest first, each naming the next in its `ProxyJump`:
/// the importer reads them back as the same hosts. The first jump host's
/// proxy is its own block's `ProxyCommand`, as OpenSSH ignores one beside
/// a `ProxyJump`.
pub fn config_block(vault: &Vault, host: &Host) -> String {
    let mut chain = vec![host];
    chain.extend(vault.jumps(host).unwrap_or_default().into_iter().rev());
    let mut aliases: Vec<String> = Vec::new();
    for via in &chain {
        let alias = alias_of(&via.name);
        // `ProxyJump none` would mean no jump host at all.
        let taken = |name: &str| {
            name.eq_ignore_ascii_case("none")
                || aliases.iter().any(|have| have.eq_ignore_ascii_case(name))
        };
        let alias = (1..)
            .map(|n| match n {
                1 => alias.clone(),
                n => format!("{alias}-{n}"),
            })
            .find(|name| !taken(name))
            .expect("an unused alias");
        aliases.push(alias);
    }
    chain
        .iter()
        .zip(&aliases)
        .enumerate()
        .map(|(at, (via, alias))| host_block(vault, via, alias, aliases.get(at + 1)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A `Host` alias for a host called `name`, which `ProxyJump` can name
/// too: its letters, digits, `.` and `_`, with a `-` for any run of other
/// characters between them.
fn alias_of(name: &str) -> String {
    let mut alias = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() || matches!(c, '.' | '_') {
            alias.push(c);
        } else if !alias.is_empty() && !alias.ends_with('-') {
            alias.push('-');
        }
    }
    match alias.trim_end_matches('-') {
        "" => "host".to_string(),
        alias => alias.to_string(),
    }
}

/// `host`'s `Host` block under `alias`: through the host whose alias is
/// `jump`, if any, otherwise through its proxy.
fn host_block(vault: &Vault, host: &Host, alias: &str, jump: Option<&String>) -> String {
    let (user, auth, _) = vault.sign_in(host);
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
    if let Some(jump) = jump {
        lines.push(format!("    ProxyJump {jump}"));
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

/// Jump hosts (first hop first) as `-J` takes them: `user@host:port`,
/// comma-separated.
fn jump_list(vault: &Vault, jumps: &[&Host]) -> String {
    jumps
        .iter()
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

/// `text` as one word for the shell a command is pasted into: in double
/// quotes when it has spaces, or as [`sh_quote`] has it when it holds a
/// double quote itself.
fn quote(text: &str) -> String {
    if text.contains('"') {
        sh_quote(text)
    } else if text.contains(char::is_whitespace) {
        format!("\"{text}\"")
    } else {
        text.to_string()
    }
}

/// `text` as one word for `sh`, which runs a `ProxyCommand`, read the same
/// way by OpenSSH's own check of the line: as it is when plain, otherwise
/// in single quotes, with a quote inside as `'"'"'` (never a backslash).
fn sh_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./_-".contains(c));
    if plain {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', r#"'"'"'"#))
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

/// How a `ProxyCommand` (its words as the config line gives them) reaches
/// the host: `nc` through a proxy, or `ssh -W` through jump hosts. `None`
/// for any other command.
fn command_route(words: &[String]) -> Option<Route> {
    // ssh turns `%%` into `%` before it runs the command.
    let words: Vec<String> = words.iter().map(|word| word.replace("%%", "%")).collect();
    let words = match words.split_first() {
        Some((first, rest)) if first == "exec" => rest,
        _ => &words[..],
    };
    let program = words.first()?.rsplit(['/', '\\']).next()?;
    match program.strip_suffix(".exe").unwrap_or(program) {
        "nc" | "ncat" | "netcat" => proxy_of_command(words).map(Route::Proxy),
        "ssh" => ssh_route(&words[1..]),
        _ => None,
    }
}

/// The jump hosts of `ssh [options] -W host:port destination`, the
/// destination last: its user, port and key, and how to reach it, from
/// `-l` (or `user@`), `-p`, `-i`, `-J` and `-o`. As ssh reads them,
/// options may follow the destination and the first of each wins.
fn ssh_route(arguments: &[String]) -> Option<Route> {
    let mut last = Hop::default();
    let mut route: Option<Route> = None;
    let (mut named, mut stdio, mut options) = (false, false, true);
    let mut words = arguments.iter();
    while let Some(word) = words.next() {
        if options && word == "--" {
            options = false;
        } else if options && word.len() > 1 && word.starts_with('-') {
            // Switches, then perhaps an option whose value is the rest of
            // the word or the next one.
            for (at, flag) in word.char_indices().skip(1) {
                if !"BbcDEeFIiJLlmOoPpQRSWw".contains(flag) {
                    continue;
                }
                let value = match &word[at + flag.len_utf8()..] {
                    "" => words.next()?.as_str(),
                    rest => rest,
                };
                match flag {
                    'l' => {
                        last.user.get_or_insert_with(|| value.to_string());
                    }
                    'p' if last.port.is_none() => last.port = Some(port_number(value)?),
                    'i' => {
                        last.key.get_or_insert_with(|| value.to_string());
                    }
                    'J' if route.is_none() => route = Some(proxy_jump(value)?),
                    'o' => ssh_option(value, &mut last, &mut route)?,
                    'W' => stdio = true,
                    _ => {}
                }
                break;
            }
        } else if named {
            // A command to run there, which `-W` doesn't.
            break;
        } else {
            let destination = destination(word)?;
            last.host = destination.host;
            last.user = last.user.or(destination.user);
            last.port = last.port.or(destination.port);
            named = true;
        }
    }
    if !stdio || !named || last.host.contains('%') {
        return None;
    }
    Some(match route {
        Some(Route::Jumps(mut hops)) => {
            hops.push(last);
            Route::Jumps(hops)
        }
        route => {
            last.route = route;
            Route::Jumps(vec![last])
        }
    })
}

/// An `ssh -o` option, read as a config line: the user, port, key or way
/// to reach the destination it gives, unless one came before. `None` when
/// ssh would refuse it.
fn ssh_option(text: &str, last: &mut Hop, route: &mut Option<Route>) -> Option<()> {
    let Some((keyword, arguments)) = split_line(text) else {
        return Some(());
    };
    let arguments = arguments?;
    let value = arguments.first()?;
    match keyword.as_str() {
        "user" => {
            last.user.get_or_insert_with(|| value.clone());
        }
        "port" if last.port.is_none() => last.port = Some(port_number(value)?),
        "identityfile" => {
            last.key.get_or_insert_with(|| value.clone());
        }
        "proxyjump" if route.is_none() => *route = Some(proxy_jump(value)?),
        // A command Tern can't follow: it dials the destination itself.
        "proxycommand" if route.is_none() => {
            *route = Some(command_route(&arguments).unwrap_or_default());
        }
        _ => {}
    }
    Some(())
}

/// The destination of an `ssh` command: `[user@]host`, or an `ssh://` URL,
/// which may give a port.
fn destination(text: &str) -> Option<Hop> {
    if text.starts_with("ssh://") {
        return hop(text);
    }
    let (user, host) = match text.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, text),
    };
    (!host.is_empty() && user != Some("")).then(|| Hop {
        host: host.to_string(),
        user: user.map(str::to_string),
        ..Hop::default()
    })
}

/// The proxy an `nc` `ProxyCommand` goes through: `-X 5` (or none) is
/// SOCKS5 and `-X connect` HTTP, with the proxy after `-x`.
fn proxy_of_command(words: &[String]) -> Option<Proxy> {
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

    /// Jump hosts, written each way ssh takes them.
    const JUMPS: &str = r#"
Host bastion
    HostName bastion.example.com
    User admin
    Port 2222
    IdentityFile ~/.ssh/id_bastion
Host db
    HostName db.internal
    ProxyJump bastion
Host app
    HostName app.internal
    ProxyJump ops@bastion
Host api
    HostName api.internal
    ProxyJump bastion:2200
Host web
    HostName web.internal
    User deploy
    ProxyJump gw.example.com,plain.lan
Host after
    HostName after.internal
    ProxyCommand ssh bastion -W %h:%p
Host flags
    HostName flags.internal
    ProxyCommand /usr/bin/ssh -W %h:%p -p 2201 -l ops -i ~/.ssh/id_ops bastion.example.com
Host chained
    HostName chained.internal
    ProxyCommand exec ssh -J gw.example.com -qW [%h]:%p inner.lan
Host socks
    HostName socks.internal
    ProxyCommand ssh -o "ProxyCommand nc -X 5 -x socks.lan:1080 %%h %%p" -W %h:%p jump.lan
Host remote-command
    HostName rc.internal
    ProxyCommand ssh bastion nc %h %p
Host *.example.com
    User wild
    IdentityFile ~/.ssh/id_wild
"#;

    fn jump_of<'a>(hosts: &'a [Host], name: &str) -> &'a Host {
        let via = find(hosts, name).jump.as_deref();
        find(
            hosts,
            via.unwrap_or_else(|| panic!("{name} has no jump host")),
        )
    }

    #[test]
    fn reaches_jump_hosts_as_ssh_would() {
        let hosts = importable(&Vault::default(), JUMPS, Path::new("/home/me/.ssh"), "me");
        assert_eq!(find(&hosts, "db").jump.as_deref(), Some("bastion"));
        // An alias with a user or port of its own: the alias's other
        // settings, as a host of its own.
        let ops = jump_of(&hosts, "app");
        assert_eq!(ops.name, "ops@bastion");
        assert_eq!(ops.target(), "ops@bastion.example.com:2222");
        assert!(matches!(&ops.auth, Auth::Key(path) if path.ends_with("id_bastion")));
        let other_port = jump_of(&hosts, "api");
        assert_eq!(other_port.name, "bastion:2200");
        assert_eq!(other_port.target(), "admin@bastion.example.com:2200");

        // Not an alias: what the blocks for its name say, else the login
        // for hosts that name none, never the user of the host behind it.
        assert_eq!(find(&hosts, "web").target(), "deploy@web.internal");
        let plain = jump_of(&hosts, "web");
        assert_eq!(plain.target(), "me@plain.lan");
        let gw = jump_of(&hosts, "plain.lan");
        assert_eq!(gw.target(), "wild@gw.example.com");
        assert!(matches!(&gw.auth, Auth::Key(path) if path.ends_with("id_wild")));
        assert_eq!(gw.jump, None);
    }

    #[test]
    fn finds_the_jump_host_in_any_ssh_proxy_command() {
        let hosts = importable(&Vault::default(), JUMPS, Path::new("/home/me/.ssh"), "me");
        // Options after the destination.
        assert_eq!(find(&hosts, "after").jump.as_deref(), Some("bastion"));
        // -p, -l and -i, wherever -W is.
        let flags = jump_of(&hosts, "flags");
        assert_eq!(flags.target(), "ops@bastion.example.com:2201");
        assert!(matches!(&flags.auth, Auth::Key(path) if path.ends_with("id_ops")));
        // -J, and -W among other switches.
        let inner = jump_of(&hosts, "chained");
        assert_eq!(inner.target(), "me@inner.lan");
        assert_eq!(inner.jump.as_deref(), Some("gw.example.com"));
        // The jump host's own ProxyCommand.
        let through_socks = jump_of(&hosts, "socks");
        assert_eq!(through_socks.target(), "me@jump.lan");
        assert_eq!(
            through_socks.proxy.as_ref().map(ToString::to_string),
            Some("socks5://socks.lan:1080".to_string())
        );
        // Without -W, ssh runs a command there instead.
        assert_eq!(find(&hosts, "remote-command").jump, None);
    }

    #[test]
    fn keeps_jump_hosts_that_differ_by_user_apart() {
        let mut vault = Vault::default();
        vault.save(None, Host::new("bastion", "x", "elsewhere", 22), "Mine");
        let hosts = importable(&vault, JUMPS, Path::new("/home/me/.ssh"), "me");
        // The alias is renamed for the clash; only what went through it
        // follows.
        for name in ["db", "after"] {
            assert_eq!(
                find(&hosts, name).jump.as_deref(),
                Some("bastion (OpenSSH)")
            );
        }
        assert_eq!(
            jump_of(&hosts, "app").target(),
            "ops@bastion.example.com:2222"
        );
    }

    #[test]
    fn takes_the_first_of_proxy_jump_and_proxy_command() {
        let config = r#"
Host proxied
    ProxyCommand nc -x socks.lan:1080 %h %p
Host jumped
    ProxyJump gw.lan
Host no-jump
    ProxyJump none
Host no-command
    ProxyCommand none
Host * !gw.lan !other.lan
    ProxyJump gw.lan
    ProxyCommand ssh -W %h:%p other.lan
"#;
        let hosts = importable(&Vault::default(), config, Path::new("."), "me");
        let proxied = find(&hosts, "proxied");
        assert!(proxied.proxy.is_some() && proxied.jump.is_none());
        assert_eq!(find(&hosts, "jumped").jump.as_deref(), Some("gw.lan"));
        // `ProxyJump none` keeps out a later ProxyJump, not a ProxyCommand.
        assert_eq!(find(&hosts, "no-jump").jump.as_deref(), Some("other.lan"));
        assert_eq!(find(&hosts, "no-command").jump, None);
    }

    #[test]
    fn reads_hops_as_proxy_jump_takes_them() {
        let read = |text: &str| hop(text).map(|hop| (hop.user, hop.host, hop.port));
        let owned = |user: Option<&str>, host: &str, port| {
            Some((user.map(String::from), host.to_string(), port))
        };
        assert_eq!(read("bastion"), owned(None, "bastion", None));
        assert_eq!(
            read("ops@[fe80::1]:2200"),
            owned(Some("ops"), "fe80::1", Some(2200))
        );
        assert_eq!(
            read("ssh://ops@gw:2200/"),
            owned(Some("ops"), "gw", Some(2200))
        );
        for wrong in [
            "bastion:x",
            "bastion:0",
            "@bastion",
            "fe80::1",
            "[fe80::1",
            "",
        ] {
            assert_eq!(read(wrong), None, "{wrong}");
        }
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
        let mine = Host {
            jump: Some("bastion".into()),
            ..Host::new("my db", "me", "10.0.4.21", 22)
        };
        vault.save(None, mine, "Mine");
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
    fn keeps_aliases_that_get_there_differently() {
        let config = r#"
Host db
    HostName 10.0.4.21
    User me
Host db-from-home
    HostName 10.0.4.21
    User me
    ProxyJump gw
Host db-proxied
    HostName 10.0.4.21
    User me
    ProxyCommand nc -X 5 -x 127.0.0.1:1080 %h %p
Host db-again
    HostName 10.0.4.21
    User me
"#;
        let hosts = importable(&Vault::default(), config, Path::new("/home/me/.ssh"), "me");
        let names: Vec<&str> = hosts.iter().map(|host| host.name.as_str()).collect();
        // db-again is db, reached the same way.
        assert_eq!(names, ["db", "db-from-home", "db-proxied", "gw"]);
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
        assert!(block.contains("    ProxyJump bastion\n"), "{block}");
        assert!(
            block.contains("    LocalForward 5433 localhost:5432\n"),
            "{block}"
        );
        // The jump host follows, in a block of its own.
        assert!(
            block.contains(
                "\n\nHost bastion\n    HostName bastion.example.com\n    User admin\n    Port 2222\n"
            ),
            "{block}"
        );

        // Read back, they're the same hosts.
        let read = importable(&Vault::default(), &block, Path::new("."), "someone");
        let again = find(&read, "prod-db");
        assert_eq!(again.target(), "deploy@10.0.4.21");
        assert!(again.agent_forwarding && !again.keep_alive);
        assert_eq!(again.env, db.env);
        assert_eq!(again.forwards.len(), 1);
        assert_eq!(again.jump.as_deref(), Some("bastion"));
        let bastion = find(&read, "bastion");
        assert_eq!(bastion.target(), "admin@bastion.example.com:2222");
        assert_eq!(read.len(), 2);
    }

    /// prod db through bastion, which goes through gw, which is behind a
    /// SOCKS proxy.
    fn chain_behind_a_proxy() -> (Vault, Host) {
        let mut vault = Vault::default();
        let mut gw = Host::new("gw", "ops", "gw.example.com", 22);
        gw.proxy = Some(Proxy::parse("socks5://socks.lan:1080").unwrap());
        vault.save(None, gw, "Prod");
        let mut bastion = Host::new("bastion", "admin", "bastion.example.com", 2222);
        bastion.jump = Some("gw".into());
        bastion.auth = Auth::Key(PathBuf::from("/keys/id bastion"));
        vault.save(None, bastion, "Prod");
        let mut db = Host::new("prod db", "deploy", "10.0.4.21", 22);
        db.jump = Some("bastion".into());
        vault.save(None, db.clone(), "Prod");
        (vault, db)
    }

    /// What the importer makes of a command's `-o ProxyCommand=…`, as a
    /// host "back" at `user@address`.
    fn read_command(command: &str, user: &str, address: &str) -> Vec<Host> {
        let words = words(command).unwrap();
        let at = words.iter().position(|word| word == "-o").unwrap();
        let config = format!(
            "Host back\n    HostName {address}\n    User {user}\n    {}\n",
            words[at + 1]
        );
        importable(&Vault::default(), &config, Path::new("."), "someone")
    }

    #[test]
    fn keeps_the_first_jump_hosts_proxy_in_the_command() {
        let mut vault = Vault::default();
        let mut bastion = Host::new("bastion", "admin", "bastion.example.com", 2222);
        bastion.proxy = Some(Proxy::parse("socks5://socks.lan:1080").unwrap());
        vault.save(None, bastion, "Prod");
        let mut db = Host::new("prod db", "deploy", "10.0.4.21", 22);
        db.jump = Some("bastion".into());
        vault.save(None, db.clone(), "Prod");
        // OpenSSH ignores a ProxyCommand beside -J, so the jump host is
        // one, with the proxy in its own.
        let command = ssh_command(&vault, &db);
        assert_eq!(
            command,
            "ssh -o \"ProxyCommand=ssh -o 'ProxyCommand=nc -X 5 -x socks.lan:1080 %%h %%p' \
             -p 2222 -W '[%h]:%p' admin@bastion.example.com\" deploy@10.0.4.21"
        );
        let read = read_command(&command, "deploy", "10.0.4.21");
        let bastion = jump_of(&read, "back");
        assert_eq!(bastion.target(), "admin@bastion.example.com:2222");
        assert_eq!(bastion.proxy, vault.find("bastion").unwrap().proxy);

        // Through two, the first behind the proxy.
        let (vault, db) = chain_behind_a_proxy();
        let command = ssh_command(&vault, &db);
        let read = read_command(&command, "deploy", "10.0.4.21");
        let bastion = jump_of(&read, "back");
        assert_eq!(bastion.target(), "admin@bastion.example.com:2222");
        assert!(
            matches!(&bastion.auth, Auth::Key(path) if path.ends_with("id bastion")),
            "{command}"
        );
        let gw = jump_of(&read, &bastion.name);
        assert_eq!(gw.target(), "ops@gw.example.com");
        assert_eq!(gw.proxy, vault.find("gw").unwrap().proxy);
        assert_eq!(gw.jump, None);
    }

    #[test]
    fn keeps_the_first_jump_hosts_proxy_in_the_config() {
        let (vault, db) = chain_behind_a_proxy();
        let block = config_block(&vault, &db);
        assert_eq!(
            block,
            "Host prod-db\n    HostName 10.0.4.21\n    User deploy\n    ProxyJump bastion\n    \
             ServerAliveInterval 30\n\n\
             Host bastion\n    HostName bastion.example.com\n    User admin\n    Port 2222\n    \
             IdentityFile \"/keys/id bastion\"\n    ProxyJump gw\n    ServerAliveInterval 30\n\n\
             Host gw\n    HostName gw.example.com\n    User ops\n    \
             ProxyCommand nc -X 5 -x socks.lan:1080 %h %p\n    ServerAliveInterval 30\n"
        );

        let read = importable(&Vault::default(), &block, Path::new("."), "someone");
        assert_eq!(read.len(), 3);
        assert_eq!(find(&read, "prod-db").jump.as_deref(), Some("bastion"));
        let bastion = find(&read, "bastion");
        assert_eq!(bastion.jump.as_deref(), Some("gw"));
        assert!(matches!(&bastion.auth, Auth::Key(path) if path.ends_with("id bastion")));
        let gw = find(&read, "gw");
        assert_eq!(gw.proxy, vault.find("gw").unwrap().proxy);
        assert_eq!(gw.jump, None);
    }

    #[test]
    fn gives_each_block_an_alias_of_its_own() {
        let mut vault = Vault::default();
        vault.save(None, Host::new("none", "a", "first.lan", 22), "Mine");
        let mut middle = Host::new("prod-db", "b", "second.lan", 22);
        middle.jump = Some("none".into());
        vault.save(None, middle, "Mine");
        let mut host = Host::new("prod db", "c", "third.lan", 22);
        host.jump = Some("prod-db".into());
        vault.save(None, host.clone(), "Mine");
        let block = config_block(&vault, &host);
        let read = importable(&Vault::default(), &block, Path::new("."), "someone");
        assert_eq!(find(&read, "prod-db").jump.as_deref(), Some("prod-db-2"));
        assert_eq!(find(&read, "prod-db-2").jump.as_deref(), Some("none-2"));
        assert_eq!(find(&read, "none-2").target(), "a@first.lan");
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
