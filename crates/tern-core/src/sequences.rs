//! Escape sequences Tern acts on itself, picked out of a session's output
//! before the terminal sees it: shell integration's command marks (OSC
//! 133, as FinalTerm, iTerm2, WezTerm and Windows Terminal read them), the
//! shell's folder (OSC 7) and inline images (iTerm2's OSC 1337 `File=`). A
//! sequence split between chunks of output waits for its end.

/// What a scan of output found, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    /// Output for the terminal, as it came.
    Text(Vec<u8>),
    Mark(Mark),
    /// The folder the shell is in now, on its host.
    Folder(String),
    /// An inline image: what followed `File=`, up to the terminator.
    Image(Vec<u8>),
}

/// Where a shell is in running a command (OSC 133).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// `A`: the prompt starts.
    Prompt,
    /// `B`: the prompt ends and the command is typed.
    Command,
    /// `C`: the command runs; its output starts.
    Output,
    /// `D`: the command finished, with its exit status when the shell says.
    Finished(Option<i32>),
}

/// The sequences picked out, by their start after `ESC ]`.
const MARK: &[u8] = b"133;";
const FOLDER: &[u8] = b"7;";
const IMAGE: &[u8] = b"1337;File=";
/// An unfinished sequence held for longer than this was never one.
const HOLD_AT_MOST: usize = 64 << 20;

/// Picks the sequences out of a session's output, chunk by chunk.
#[derive(Default)]
pub struct Scanner {
    /// The start of a sequence, or of what may be one, from the last chunk.
    held: Vec<u8>,
    /// How much of the held sequence's body was looked through for its
    /// end, in vain: a long one (an image) arrives over many chunks, and
    /// going through it again for each would take minutes.
    searched: usize,
}

impl Scanner {
    /// Whether `bytes` holds nothing for the scanner: no OSC sequence
    /// starts in it, nor may at its end (an `ESC` the next chunk's `]`
    /// follows), and nothing is held. Most output is like that.
    pub fn quiet(&self, bytes: &[u8]) -> bool {
        self.held.is_empty()
            && bytes.last() != Some(&0x1b)
            && !bytes.windows(2).any(|pair| pair == b"\x1b]")
    }

    /// Splits `bytes` (after what was held) into output for the terminal and
    /// the sequences between it.
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<Piece> {
        let searched = std::mem::take(&mut self.searched);
        let joined = (!self.held.is_empty()).then(|| {
            let mut held = std::mem::take(&mut self.held);
            held.extend_from_slice(bytes);
            held
        });
        let input: &[u8] = joined.as_deref().unwrap_or(bytes);
        let mut pieces = Vec::new();
        let mut text_from = 0;
        let mut at = 0;
        // Where a sequence starts that waits for more, and how much of its
        // body was looked through.
        let mut hold = None;
        while let Some(found) = input[at..].iter().position(|&byte| byte == 0x1b) {
            let start = at + found;
            let rest = &input[start..];
            // `ESC` alone, or `ESC ]` and part of a sequence's start: wait
            // for more.
            if rest.len() < 2 {
                hold = Some((start, 0));
                break;
            }
            if rest[1] != b']' {
                at = start + 1;
                continue;
            }
            let body = &rest[2..];
            let wanted = [MARK, FOLDER, IMAGE]
                .into_iter()
                .find(|prefix| body.starts_with(prefix));
            let Some(prefix) = wanted else {
                let partial = [MARK, FOLDER, IMAGE]
                    .iter()
                    .any(|prefix| body.len() < prefix.len() && prefix.starts_with(body));
                if partial {
                    hold = Some((start, 0));
                    break;
                }
                at = start + 2;
                continue;
            };
            // The held sequence's body was looked through before, but for
            // its last byte: an `ESC` there may meet its `\` only now.
            let skip = match joined {
                Some(_) if start == 0 => searched.saturating_sub(1),
                _ => 0,
            };
            match terminator(body, skip) {
                Some((content_end, after)) => {
                    if start > text_from {
                        pieces.push(Piece::Text(input[text_from..start].to_vec()));
                    }
                    let content = &body[prefix.len()..content_end];
                    pieces.push(if prefix == MARK {
                        match mark(content) {
                            Some(mark) => Piece::Mark(mark),
                            None => Piece::Text(Vec::new()),
                        }
                    } else if prefix == FOLDER {
                        match folder(content) {
                            Some(folder) => Piece::Folder(folder),
                            None => Piece::Text(Vec::new()),
                        }
                    } else {
                        Piece::Image(content.to_vec())
                    });
                    at = start + 2 + after;
                    text_from = at;
                }
                None if rest.len() <= HOLD_AT_MOST => {
                    hold = Some((start, body.len()));
                    break;
                }
                // Too long to be one: it's output.
                None => at = start + 2,
            }
        }
        let end = hold.map_or(input.len(), |(from, _)| from);
        if end > text_from {
            pieces.push(Piece::Text(input[text_from..end].to_vec()));
        }
        // The rest waits for the next chunk. All that was held, still
        // waiting, is kept as it is rather than copied again.
        if let Some((from, searched)) = hold {
            let tail = (from > 0 || joined.is_none()).then(|| input[from..].to_vec());
            self.held = tail.or(joined).unwrap_or_default();
            self.searched = searched;
        }
        pieces.retain(|piece| piece != &Piece::Text(Vec::new()));
        pieces
    }
}

/// Where an OSC sequence's content ends and what follows its terminator
/// (BEL, or `ESC \`), within `body`, looking from `from` on.
fn terminator(body: &[u8], from: usize) -> Option<(usize, usize)> {
    body.iter()
        .enumerate()
        .skip(from)
        .find_map(|(at, &byte)| match byte {
            0x07 => Some((at, at + 1)),
            0x1b if body.get(at + 1) == Some(&b'\\') => Some((at, at + 2)),
            _ => None,
        })
}

/// The path in what followed `7;`: a `file://host/path` URL, its
/// escapes (`%20`) undone. The host is the session's own.
fn folder(content: &[u8]) -> Option<String> {
    let url = std::str::from_utf8(content).ok()?;
    let rest = url.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = bytes
            .get(at + 1..at + 3)
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (bytes[at], hex) {
            (b'%', Some(byte)) => {
                decoded.push(byte);
                at += 3;
            }
            (byte, _) => {
                decoded.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8(decoded).ok()
}

/// The mark in what followed `133;`: `A`, `B`, `C` or `D[;status]`, with any
/// further options left aside.
fn mark(content: &[u8]) -> Option<Mark> {
    let text = std::str::from_utf8(content).ok()?;
    let mut fields = text.split(';');
    Some(match fields.next()? {
        "A" => Mark::Prompt,
        "B" => Mark::Command,
        "C" => Mark::Output,
        "D" => Mark::Finished(fields.next().and_then(|status| status.parse().ok())),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(text: &str) -> Piece {
        Piece::Text(text.as_bytes().to_vec())
    }

    #[test]
    fn picks_marks_out_of_output() {
        let mut scanner = Scanner::default();
        let pieces = scanner.scan(
            b"\x1b]133;A\x07$ \x1b]133;B\x1b\\ls\r\n\x1b]133;C\x07a\r\n\x1b]133;D;1\x07\x1b[1mbold\x1b]0;title\x07",
        );
        assert_eq!(
            pieces,
            [
                Piece::Mark(Mark::Prompt),
                text("$ "),
                Piece::Mark(Mark::Command),
                text("ls\r\n"),
                Piece::Mark(Mark::Output),
                text("a\r\n"),
                Piece::Mark(Mark::Finished(Some(1))),
                // Other sequences go to the terminal as they are.
                text("\x1b[1mbold\x1b]0;title\x07"),
            ]
        );
        assert_eq!(
            Scanner::default().scan(b"\x1b]133;D\x07\x1b]133;A;cl=m\x07"),
            [Piece::Mark(Mark::Finished(None)), Piece::Mark(Mark::Prompt)]
        );
    }

    #[test]
    fn a_sequence_split_between_chunks_waits_for_its_end() {
        let mut scanner = Scanner::default();
        assert_eq!(scanner.scan(b"out\x1b"), [text("out")]);
        assert_eq!(scanner.scan(b"]13"), []);
        assert_eq!(scanner.scan(b"3;"), []);
        assert_eq!(
            scanner.scan(b"C\x07more"),
            [Piece::Mark(Mark::Output), text("more")]
        );
        // An escape that turns out to be something else goes through.
        assert_eq!(scanner.scan(b"x\x1b"), [text("x")]);
        assert_eq!(scanner.scan(b"[0m"), [text("\x1b[0m")]);
        assert_eq!(
            scanner.scan(b"\x1b]2;title\x07"),
            [text("\x1b]2;title\x07")]
        );
    }

    #[test]
    fn a_sequence_split_after_its_escape_is_still_seen() {
        // Each split of each sequence, as feeding the terminal goes: past
        // the scanner while it's quiet, through it otherwise.
        for whole in [
            &b"ls\x1b]7;file://host/work\x07$ "[..],
            b"ls\x1b]133;A\x07$ ",
            b"ls\x1b]1337;File=inline=1:QUJD\x1b\\$ ",
        ] {
            for split in 1..whole.len() {
                let mut scanner = Scanner::default();
                let mut pieces = Vec::new();
                for chunk in [&whole[..split], &whole[split..]] {
                    if scanner.quiet(chunk) {
                        pieces.push(Piece::Text(chunk.to_vec()));
                    } else {
                        pieces.extend(scanner.scan(chunk));
                    }
                }
                let picked = pieces
                    .iter()
                    .filter(|piece| !matches!(piece, Piece::Text(_)))
                    .count();
                assert_eq!(picked, 1, "{whole:?} split at {split}");
            }
        }
    }

    #[test]
    fn picks_the_shells_folder_out() {
        let mut scanner = Scanner::default();
        assert_eq!(
            scanner.scan(b"\x1b]7;file://web-01/srv/my%20app\x07$ "),
            [Piece::Folder("/srv/my app".into()), text("$ ")]
        );
        assert_eq!(
            scanner.scan(b"\x1b]7;file:///home/lab\x1b\\"),
            [Piece::Folder("/home/lab".into())]
        );
        // Not a file URL: nothing to follow, nothing shown.
        assert_eq!(scanner.scan(b"\x1b]7;kitty-shell-cwd:/x\x07"), []);
        // OSC 7 isn't OSC 77 or 777.
        assert_eq!(
            scanner.scan(b"\x1b]777;notify;hi\x07"),
            [text("\x1b]777;notify;hi\x07")]
        );
    }

    #[test]
    fn picks_inline_images_out() {
        let mut scanner = Scanner::default();
        let pieces = scanner.scan(b"before\x1b]1337;File=inline=1;size=3:QUJD\x07after");
        assert_eq!(
            pieces,
            [
                text("before"),
                Piece::Image(b"inline=1;size=3:QUJD".to_vec()),
                text("after")
            ]
        );
        // Long ones come in many chunks.
        assert_eq!(scanner.scan(b"\x1b]1337;File=inline=1:QU"), []);
        assert_eq!(scanner.scan(b"JD"), []);
        assert_eq!(
            scanner.scan(b"\x1b\\"),
            [Piece::Image(b"inline=1:QUJD".to_vec())]
        );
        // Their end may be split between chunks too, and what follows
        // comes through.
        assert_eq!(scanner.scan(b"\x1b]1337;File=inline=1:QU"), []);
        assert_eq!(scanner.scan(b"JD\x1b"), []);
        assert_eq!(
            scanner.scan(b"\\$ \x1b]1337;File=:QQ"),
            [Piece::Image(b"inline=1:QUJD".to_vec()), text("$ ")]
        );
        assert_eq!(scanner.scan(b"==\x07"), [Piece::Image(b":QQ==".to_vec())]);
    }

    #[test]
    fn a_long_image_takes_as_long_as_its_chunks() {
        let mut scanner = Scanner::default();
        let chunk = vec![b'A'; 32 << 10];
        let started = std::time::Instant::now();
        assert_eq!(scanner.scan(b"\x1b]1337;File=inline=1:"), []);
        for _ in 0..(32 << 20) / chunk.len() {
            assert_eq!(scanner.scan(&chunk), []);
        }
        let pieces = scanner.scan(b"\x07");
        assert!(matches!(&pieces[..], [Piece::Image(image)] if image.len() == (32 << 20) + 9));
        // Going through all that was held for each chunk took half a
        // minute.
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}
