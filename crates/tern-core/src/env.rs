//! Environment variables a host's shell starts with, written as OpenSSH's
//! `SetEnv` takes them: `NAME=value` pairs separated by spaces, with
//! double quotes around a value that has spaces in it.

use std::collections::BTreeMap;

/// Variables by name.
pub type Env = BTreeMap<String, String>;

/// The variables in `text`. Fails with a one-line reason on a pair that
/// isn't `NAME=value` or a name a shell can't take.
pub fn parse(text: &str) -> Result<Env, String> {
    let mut env = Env::new();
    for word in words(text)? {
        let Some((name, value)) = word.split_once('=') else {
            return Err(format!("{word} isn't NAME=value"));
        };
        if !valid_name(name) {
            return Err(format!("{name} can't be a variable's name"));
        }
        env.insert(name.to_string(), value.to_string());
    }
    Ok(env)
}

/// `env` as [`parse`] reads it back.
pub fn format(env: &Env) -> String {
    env.iter()
        .map(|(name, value)| {
            if value.is_empty() || value.contains(char::is_whitespace) || value.contains('"') {
                format!(
                    "{name}=\"{}\"",
                    value.replace('\\', "\\\\").replace('"', "\\\"")
                )
            } else {
                format!("{name}={value}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Letters, digits and underscores, not starting with a digit.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Splits on spaces outside double quotes, which are dropped; `\` escapes
/// the next character inside them.
fn words(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let (mut quoted, mut started) = (false, false);
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => word.extend(chars.next()),
            c if c.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return Err("A quote isn't closed".into());
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_pairs_and_quoted_values() {
        let env = parse(r#"LANG=C.UTF-8  GREETING="hello world" EMPTY="" PATHS=a=b"#).unwrap();
        assert_eq!(env["LANG"], "C.UTF-8");
        assert_eq!(env["GREETING"], "hello world");
        assert_eq!(env["EMPTY"], "");
        assert_eq!(env["PATHS"], "a=b");
        assert!(parse("").unwrap().is_empty());
        assert!(parse("   ").unwrap().is_empty());
    }

    #[test]
    fn says_what_is_wrong() {
        assert_eq!(parse("LANG").unwrap_err(), "LANG isn't NAME=value");
        assert_eq!(parse("1X=y").unwrap_err(), "1X can't be a variable's name");
        assert_eq!(
            parse("A-B=y").unwrap_err(),
            "A-B can't be a variable's name"
        );
        assert_eq!(parse(r#"A="open"#).unwrap_err(), "A quote isn't closed");
    }

    #[test]
    fn writes_what_it_reads() {
        let env = parse(r#"B="two words" A=1 Q="say \"hi\"" E="""#).unwrap();
        let text = format(&env);
        assert_eq!(text, r#"A=1 B="two words" E="" Q="say \"hi\"""#);
        assert_eq!(parse(&text).unwrap(), env);
    }
}
