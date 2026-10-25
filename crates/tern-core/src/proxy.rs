//! Reaching a host's SSH port through a proxy: SOCKS5 or HTTP `CONNECT`,
//! written as a URL (`socks5://proxy:1080`, `http://proxy:8080`).

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

use crate::host::Unknown;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyKind {
    Socks5,
    Http,
}

/// A proxy to connect through. Only proxies that need no password: one
/// would have to live in the vault file, which keeps no secrets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proxy {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    /// What a newer Tern added that this one doesn't know, kept as it came.
    #[serde(flatten)]
    pub unknown: Unknown,
}

impl Proxy {
    /// Reads `socks5://host[:port]` or `http://host[:port]` (ports default
    /// to 1080 and 8080). Fails with a one-line reason.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (scheme, rest) = text
            .split_once("://")
            .ok_or("Write the proxy as socks5://host:port or http://host:port")?;
        let (kind, default_port) = match scheme.to_ascii_lowercase().as_str() {
            "socks5" | "socks5h" | "socks" => (ProxyKind::Socks5, 1080),
            "http" => (ProxyKind::Http, 8080),
            _ => return Err(format!("{scheme} proxies aren't supported")),
        };
        let rest = rest.trim_end_matches('/');
        if rest.contains('@') {
            return Err("Proxies that need a password aren't supported".into());
        }
        // `[v6::address]:port`, `host:port` or a bare host.
        let (host, port) = match rest.strip_prefix('[') {
            Some(bracketed) => {
                let (host, after) = bracketed.split_once(']').ok_or("A ] is missing")?;
                (host, after.strip_prefix(':'))
            }
            None => match rest.rsplit_once(':') {
                Some((host, port)) if !host.contains(':') => (host, Some(port)),
                _ => (rest, None),
            },
        };
        // OpenSSH runs a proxy's `nc` through a shell, so nothing in the
        // host may read as more than a name.
        if host.is_empty() || crate::host::address_problem(host).is_some() {
            return Err("The proxy's host isn't valid".into());
        }
        let port = match port {
            Some(port) => port
                .parse::<u16>()
                .ok()
                .filter(|&port| port > 0)
                .ok_or("The proxy's port must be a number from 1 to 65535")?,
            None => default_port,
        };
        Ok(Self {
            kind,
            host: host.to_string(),
            port,
            unknown: Unknown::new(),
        })
    }
}

impl std::fmt::Display for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scheme = match self.kind {
            ProxyKind::Socks5 => "socks5",
            ProxyKind::Http => "http",
        };
        if self.host.contains(':') {
            write!(f, "{scheme}://[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{scheme}://{}:{}", self.host, self.port)
        }
    }
}

/// A TCP stream to `host:port` through `proxy`. The proxy resolves the
/// name, so hosts only it can see work. Errors are a few lowercase words.
pub async fn connect(proxy: &Proxy, host: &str, port: u16) -> Result<TcpStream, String> {
    let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port))
        .await
        .map_err(|error| format!("proxy: {}", crate::ssh::describe(&error)))?;
    let _ = stream.set_nodelay(true);
    match proxy.kind {
        ProxyKind::Socks5 => socks5(&mut stream, host, port).await?,
        ProxyKind::Http => http_connect(&mut stream, host, port).await?,
    }
    Ok(stream)
}

async fn socks5(stream: &mut TcpStream, host: &str, port: u16) -> Result<(), String> {
    let io = |error: std::io::Error| format!("proxy: {}", crate::ssh::describe(&error));
    // Greeting: version 5, one method, no authentication.
    stream.write_all(&[5, 1, 0]).await.map_err(io)?;
    let mut chosen = [0u8; 2];
    stream.read_exact(&mut chosen).await.map_err(io)?;
    match chosen {
        [5, 0] => {}
        [5, 0xff] => return Err("the proxy wants a password".into()),
        _ => return Err("not a SOCKS5 proxy".into()),
    }
    let name = host.as_bytes();
    if name.len() > 255 {
        return Err("the host name is too long for SOCKS5".into());
    }
    // Connect, by domain name (type 3), so the proxy resolves it.
    let mut request = vec![5, 1, 0, 3, name.len() as u8];
    request.extend_from_slice(name);
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await.map_err(io)?;
    let mut reply = [0u8; 4];
    stream.read_exact(&mut reply).await.map_err(io)?;
    if reply[1] != 0 {
        return Err(format!("the proxy {}", socks5_reason(reply[1])));
    }
    // The address the proxy bound, which nothing here needs.
    let address = match reply[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut length = [0u8; 1];
            stream.read_exact(&mut length).await.map_err(io)?;
            length[0] as usize
        }
        _ => return Err("not a SOCKS5 proxy".into()),
    };
    let mut rest = vec![0u8; address + 2];
    stream.read_exact(&mut rest).await.map_err(io)?;
    Ok(())
}

fn socks5_reason(code: u8) -> &'static str {
    match code {
        2 => "doesn't allow that connection",
        3 => "can't reach the network",
        4 => "can't reach the host",
        5 => "was refused by the host",
        6 => "timed out",
        8 => "can't resolve that address",
        _ => "couldn't connect",
    }
}

async fn http_connect(stream: &mut TcpStream, host: &str, port: u16) -> Result<(), String> {
    let io = |error: std::io::Error| format!("proxy: {}", crate::ssh::describe(&error));
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n");
    stream.write_all(request.as_bytes()).await.map_err(io)?;
    // Byte by byte up to the blank line, so nothing of the tunnel is read.
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 * 1024 {
            return Err("the proxy's answer is too long".into());
        }
        let mut byte = [0u8; 1];
        if stream.read(&mut byte).await.map_err(io)? == 0 {
            return Err("the proxy closed the connection".into());
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    let status = head.lines().next().unwrap_or_default();
    let mut parts = status.splitn(3, ' ');
    let (version, code) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    if !version.starts_with("HTTP/") {
        return Err("not an HTTP proxy".into());
    }
    if !code.starts_with('2') {
        let reason = parts.next().unwrap_or_default().trim();
        return Err(format!("the proxy answered {code} {reason}")
            .trim_end()
            .to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_proxy_urls() {
        let socks = Proxy::parse(" socks5://proxy.lan:1081 ").unwrap();
        assert_eq!(
            socks,
            Proxy {
                kind: ProxyKind::Socks5,
                host: "proxy.lan".into(),
                port: 1081,
                unknown: Default::default(),
            }
        );
        assert_eq!(socks.to_string(), "socks5://proxy.lan:1081");
        assert_eq!(Proxy::parse("SOCKS5H://p").unwrap().port, 1080);
        let http = Proxy::parse("http://10.0.0.1/").unwrap();
        assert_eq!((http.kind, http.port), (ProxyKind::Http, 8080));
        let v6 = Proxy::parse("http://[::1]:3128").unwrap();
        assert_eq!(v6.host, "::1");
        assert_eq!(v6.to_string(), "http://[::1]:3128");
        assert_eq!(Proxy::parse(&v6.to_string()).unwrap(), v6);
    }

    #[test]
    fn says_what_is_wrong() {
        assert!(
            Proxy::parse("proxy:1080")
                .unwrap_err()
                .contains("socks5://")
        );
        assert_eq!(
            Proxy::parse("ftp://proxy").unwrap_err(),
            "ftp proxies aren't supported"
        );
        assert!(
            Proxy::parse("http://user:pw@proxy")
                .unwrap_err()
                .contains("password")
        );
        assert!(
            Proxy::parse("socks5://proxy:0")
                .unwrap_err()
                .contains("port")
        );
        assert!(Proxy::parse("socks5://").unwrap_err().contains("host"));
        // OpenSSH runs a proxy's `nc` through a shell.
        assert!(
            Proxy::parse("socks5://p;reboot:1080")
                .unwrap_err()
                .contains("host")
        );
    }
}
