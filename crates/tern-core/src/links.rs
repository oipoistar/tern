//! Web addresses in terminal text, which Ctrl+click (Cmd+click on macOS)
//! opens. What a host prints is untrusted: only web, mail and ssh links
//! from it open, and `file:` links only from this computer's own shell.

use std::ops::Range;

/// The schemes a link starts with.
const SCHEMES: [&str; 5] = ["https://", "http://", "file://", "ssh://", "mailto:"];

/// Whether the link `url`, found in output, may be opened. A `file:` link
/// opens only from a shell on this computer (`local`) and only to a path
/// here: one naming another computer (`file://server/share`) would have
/// Windows hand that computer this account's credentials, and from a host
/// one could point at a program to run.
pub fn may_open(url: &str, local: bool) -> bool {
    let lower = url.to_ascii_lowercase();
    match lower.strip_prefix("file://") {
        Some(path) => local && path.starts_with('/') && !path.starts_with("//"),
        None => true,
    }
}

/// The byte ranges of the links in `text`, in order.
pub fn find(text: &str) -> Vec<Range<usize>> {
    // ASCII lowercasing keeps every byte where it was.
    let lower = text.to_ascii_lowercase();
    let mut found = Vec::new();
    let mut from = 0;
    while let Some((start, scheme)) = SCHEMES
        .iter()
        .filter_map(|scheme| {
            lower[from..]
                .find(scheme)
                .map(|at| (from + at, scheme.len()))
        })
        .min_by_key(|(at, _)| *at)
    {
        let rest = start + scheme;
        let end = text[rest..]
            .char_indices()
            .find(|(_, c)| ends_link(*c))
            .map_or(text.len(), |(at, _)| rest + at);
        let end = rest + trimmed(&text[rest..end]).len();
        if end > rest {
            found.push(start..end);
        }
        from = end.max(rest);
    }
    found
}

/// Characters a link never contains.
fn ends_link(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || matches!(
            c,
            '<' | '>' | '"' | '`' | '{' | '}' | '|' | '\\' | '^' | '⟨' | '⟩'
        )
}

/// `link` without what's more likely the sentence's than the link's:
/// trailing punctuation, quotes, and closing brackets it never opened.
fn trimmed(mut link: &str) -> &str {
    loop {
        let unopened = |close: char, open: char| {
            link.ends_with(close) && link.matches(close).count() > link.matches(open).count()
        };
        if link.ends_with(['.', ',', ';', ':', '!', '?', '\'', '*'])
            || unopened(')', '(')
            || unopened(']', '[')
        {
            link = &link[..link.len() - 1];
        } else {
            return link;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::find;

    fn links(text: &str) -> Vec<&str> {
        find(text).into_iter().map(|range| &text[range]).collect()
    }

    #[test]
    fn opens_files_only_from_here_and_only_here() {
        use super::may_open;

        assert!(may_open("https://example.com/a", false));
        assert!(may_open("mailto:ops@example.com", false));
        assert!(!may_open("file:///C:/Users/me/Downloads/run.exe", false));
        assert!(may_open("file:///home/me/report.html", true));
        assert!(!may_open("file://attacker.example/share/x", true));
        assert!(!may_open("FILE:////attacker.example/share/x", true));
    }

    #[test]
    fn finds_links_in_text() {
        assert_eq!(
            links("see https://example.com/a?b=1#c and http://x.org"),
            ["https://example.com/a?b=1#c", "http://x.org"]
        );
        assert_eq!(links("HTTPS://Example.com"), ["HTTPS://Example.com"]);
        assert_eq!(
            links("clone ssh://git@host:22/repo.git, then mailto:me@example.com"),
            ["ssh://git@host:22/repo.git", "mailto:me@example.com"]
        );
        assert!(links("no links here: http:// alone").is_empty());
        assert!(links("").is_empty());
    }

    #[test]
    fn leaves_the_sentence_out() {
        assert_eq!(links("Go to https://example.com."), ["https://example.com"]);
        assert_eq!(links("'https://example.com/x'"), ["https://example.com/x"]);
        assert_eq!(links("<https://example.com>"), ["https://example.com"]);
        assert_eq!(links("(https://example.com/a)"), ["https://example.com/a"]);
        assert_eq!(
            links("(see https://en.wikipedia.org/wiki/Tern_(bird))."),
            ["https://en.wikipedia.org/wiki/Tern_(bird)"]
        );
        assert_eq!(links("\"https://a.b/c\"|"), ["https://a.b/c"]);
    }

    #[test]
    fn counts_bytes_after_wide_text() {
        let text = "日本 https://例え.jp/パス ok";
        let found = find(text);
        assert_eq!(found.len(), 1);
        assert_eq!(&text[found[0].clone()], "https://例え.jp/パス");
    }
}
