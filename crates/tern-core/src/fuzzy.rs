//! Fuzzy matching for the command palette: the query's characters in
//! order, anywhere in the text, scored so that tighter, earlier and
//! word-start matches rank first.

/// How well `query` matches `text`, higher being better; `None` when the
/// query's characters don't all appear in order. Case doesn't matter; an
/// empty query matches everything equally.
pub fn score(query: &str, text: &str) -> Option<u32> {
    let query: Vec<char> = query
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if query.is_empty() {
        return Some(0);
    }
    let text: Vec<char> = text.to_lowercase().chars().collect();
    // The best of the matches starting at each place the first letter is.
    (0..text.len())
        .filter(|&start| text[start] == query[0])
        .filter_map(|start| score_from(&query, &text, start))
        .max()
}

/// The score of matching `query` with its first letter at `start`, each
/// next letter taken right after the last when it's there, otherwise at
/// its next appearance.
fn score_from(query: &[char], text: &[char], start: usize) -> Option<u32> {
    let mut score = 0u32;
    let mut previous: Option<usize> = None;
    for wanted in query {
        let found = match previous {
            None => start,
            Some(last) if text.get(last + 1) == Some(wanted) => last + 1,
            Some(last) => (last + 1..text.len()).find(|&index| text[index] == *wanted)?,
        };
        // Right after the last letter is worth most, then the start of a
        // word, then anywhere; early in the text adds a little.
        let word_start = found == 0 || !text[found - 1].is_alphanumeric();
        score += match previous {
            Some(last) if found == last + 1 => 8,
            _ if word_start => 6,
            _ => 1,
        };
        if found < 8 {
            score += 1;
        }
        previous = Some(found);
    }
    // The whole text matched beats a longer one holding the same letters.
    if query.len() == text.len() {
        score += 4;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn matches_letters_in_order() {
        assert!(score("pdb", "prod-db-01").is_some());
        assert!(score("PROD", "prod-db-01").is_some());
        assert!(score("bdp", "prod-db-01").is_none());
        assert!(score("", "anything").is_some());
        assert!(score("prod web", "prod-web-01").is_some());
    }

    #[test]
    fn ranks_tight_and_word_start_matches_first() {
        let rank = |query: &str, a: &str, b: &str| score(query, a) > score(query, b);
        assert!(rank("db", "prod-db-01", "dashboard"));
        assert!(rank("web", "prod-web-01", "w-e-b"));
        assert!(rank("set", "Settings", "Reset terminal"));
        assert!(rank("vault", "Vault", "Vault backups"));
    }
}
