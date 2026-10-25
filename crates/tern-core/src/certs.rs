//! OpenSSH certificates: host certificates, which a server presents and a
//! known_hosts file's `@cert-authority` lines vouch for, and user
//! certificates (`<key>-cert.pub` beside a key file), which sign us in.

use std::path::{Path, PathBuf};

use russh::keys::ssh_key::certificate::CertType;
use russh::keys::{Certificate, HashAlg, PublicKey};

/// The certificate authorities `files` trust for `host:port`: the keys of
/// their `@cert-authority` lines whose host patterns take it.
pub fn authorities(files: &[PathBuf], host: &str, port: u16) -> Vec<PublicKey> {
    files
        .iter()
        .filter_map(|file| std::fs::read_to_string(file).ok())
        .flat_map(|text| authorities_in(&text, host, port))
        .collect()
}

fn authorities_in(text: &str, host: &str, port: u16) -> Vec<PublicKey> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line
                .trim()
                .strip_prefix("@cert-authority")?
                .split_whitespace();
            let patterns = fields.next()?;
            let key = format!("{} {}", fields.next()?, fields.next()?);
            crate::known_hosts::takes(patterns, host, port)
                .then(|| PublicKey::from_openssh(&key).ok())
                .flatten()
        })
        .collect()
}

/// Glob matching with `*` (any run) and `?` (any one character).
pub(crate) fn wildcard(pattern: &str, text: &str) -> bool {
    let (pattern, text): (Vec<char>, Vec<char>) =
        (pattern.chars().collect(), text.chars().collect());
    let (mut p, mut t) = (0, 0);
    // Where the last `*` was, and where in `text` it started matching.
    let mut star = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((at, from)) = star {
            // Let the `*` take one more character and try again.
            star = Some((at, from + 1));
            p = at + 1;
            t = from + 1;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

/// Checks a host certificate as OpenSSH does: a host certificate, valid
/// at `now` (Unix seconds), signed by one of `authorities`, naming `host`
/// among its principals (or naming none), with no critical options. The
/// error says what's wrong in a few lowercase words.
pub fn check_host(
    cert: &Certificate,
    host: &str,
    authorities: &[PublicKey],
    now: u64,
) -> Result<(), String> {
    if cert.cert_type() != CertType::Host {
        return Err("isn't a host certificate".into());
    }
    if now < cert.valid_after() || now >= cert.valid_before() {
        return Err("has expired or isn't valid yet".into());
    }
    let trusted: Vec<_> = authorities
        .iter()
        .map(|authority| authority.fingerprint(HashAlg::Sha256))
        .collect();
    cert.validate_at(now, &trusted)
        .map_err(|_| "isn't signed by a trusted authority".to_string())?;
    let principals = cert.valid_principals();
    if !principals.is_empty()
        && !principals
            .iter()
            .any(|name| name.eq_ignore_ascii_case(host))
    {
        return Err(format!("isn't for {host}"));
    }
    if !cert.critical_options().is_empty() {
        return Err("has options Tern doesn't know".into());
    }
    Ok(())
}

/// The certificate OpenSSH keeps beside a key file, `<key>-cert.pub`, if
/// there's a readable one.
pub fn user_certificate(key: &Path) -> Option<Certificate> {
    let mut path = key.as_os_str().to_owned();
    path.push("-cert.pub");
    let text = std::fs::read_to_string(path).ok()?;
    Certificate::from_openssh(text.trim()).ok()
}

/// Seconds since the Unix epoch, for checking validity.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use russh::keys::ssh_key::certificate::{Builder, CertType};
    use russh::keys::{Algorithm, PrivateKey};

    use super::*;

    fn key() -> PrivateKey {
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap()
    }

    /// A certificate for `key`, valid from 1000 to 2000, signed by `ca`.
    fn certificate(
        key: &PrivateKey,
        ca: &PrivateKey,
        kind: CertType,
        principals: &[&str],
    ) -> Certificate {
        let mut builder =
            Builder::new([7u8; 16], key.public_key().key_data().clone(), 1000, 2000).unwrap();
        builder.cert_type(kind).unwrap();
        if principals.is_empty() {
            builder.all_principals_valid().unwrap();
        }
        for principal in principals {
            builder.valid_principal(*principal).unwrap();
        }
        builder.sign(ca).unwrap()
    }

    #[test]
    fn wildcards_match_runs_and_single_characters() {
        assert!(wildcard("a*b*c", "aXbYbZc"));
        assert!(!wildcard("a*b", "aXbYc"));
        assert!(wildcard("c?", "c1") && !wildcard("c?", "c12"));
    }

    #[test]
    fn finds_the_authorities_for_a_host() {
        let ca = key();
        let other = key();
        let text = format!(
            "# a comment\n\
             db.example.com {}\n\
             @cert-authority *.example.com {}\n\
             @cert-authority *.example.org {}\n\
             @revoked * {}\n",
            other.public_key().to_openssh().unwrap(),
            ca.public_key().to_openssh().unwrap(),
            other.public_key().to_openssh().unwrap(),
            other.public_key().to_openssh().unwrap(),
        );
        let found = authorities_in(&text, "db.example.com", 22);
        assert_eq!(found, [ca.public_key().clone()]);
        assert!(authorities_in(&text, "db.example.net", 22).is_empty());
    }

    #[test]
    fn checks_host_certificates() {
        let (ca, host) = (key(), key());
        let cas = [ca.public_key().clone()];
        let cert = certificate(&host, &ca, CertType::Host, &["db.example.com"]);
        assert_eq!(check_host(&cert, "db.example.com", &cas, 1500), Ok(()));
        assert_eq!(
            check_host(&cert, "web.example.com", &cas, 1500).unwrap_err(),
            "isn't for web.example.com"
        );
        assert!(
            check_host(&cert, "db.example.com", &cas, 2500)
                .unwrap_err()
                .contains("expired")
        );
        let stranger = [key().public_key().clone()];
        assert!(
            check_host(&cert, "db.example.com", &stranger, 1500)
                .unwrap_err()
                .contains("authority")
        );

        // A user certificate isn't a host's, and one naming nobody names
        // any host.
        let user = certificate(&host, &ca, CertType::User, &["db.example.com"]);
        assert!(check_host(&user, "db.example.com", &cas, 1500).is_err());
        let anyone = certificate(&host, &ca, CertType::Host, &[]);
        assert_eq!(check_host(&anyone, "web.example.com", &cas, 1500), Ok(()));
    }

    #[test]
    fn reads_the_certificate_beside_a_key() {
        let dir = std::env::temp_dir().join(format!("tern-user-cert-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("id_ed25519");
        assert!(user_certificate(&path).is_none());
        let cert = certificate(&key(), &key(), CertType::User, &["me"]);
        std::fs::write(dir.join("id_ed25519-cert.pub"), cert.to_openssh().unwrap()).unwrap();
        assert_eq!(user_certificate(&path), Some(cert));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
