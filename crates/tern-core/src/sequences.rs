//! Escape sequences Tern acts on itself, picked out of a session's output
//! before the terminal sees it: shell integration's command marks (OSC
//! 133, as FinalTerm, iTerm2, WezTerm and Windows Terminal read them) and
//! inline images (iTerm2's OSC 1337 `File=`). A sequence split between
//! chunks of output waits for its end.

/// What a scan of output found, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    /// Output for the terminal, as it came.
    Text(Vec<u8>),
    Mark(Mark),
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
const IMAGE: &[u8] = b"1337;File=";
/// An unfinished sequence held for longer than this was never one.
const HOLD_AT_MOST: usize = 64 << 20;

/// Picks the sequences out of a session's output, chunk by chunk.
#[derive(Default)]
pub struct Scanner {
    /// The start of a sequence, or of what may be one, from the last chunk.
    held: Vec<u8>,
}

impl Scanner {
    /// Whether `bytes` holds nothing for the scanner: no OSC sequence
    /// starts in it and nothing is held. Most output is like that.
    pub fn quiet(&self, bytes: &[u8]) -> bool {
        self.held.is_empty() && !bytes.windows(2).any(|pair| pair == b"\x1b]")
    }

    /// Splits `bytes` (after what was held) into output for the terminal and
    /// the sequences between it.
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<Piece> {
        let joined;
        let input: &[u8] = if self.held.is_empty() {
            bytes
        } else {
            let mut held = std::mem::take(&mut self.held);
            held.extend_from_slice(bytes);
            joined = held;
            &joined
        };
        let mut pieces = Vec::new();
        let mut text_from = 0;
        let mut at = 0;
        while let Some(found) = input[at..].iter().position(|&byte| byte == 0x1b) {
            let start = at + found;
            let rest = &input[start..];
            // `ESC` alone, or `ESC ]` and part of a sequence's start: wait
            // for more.
            if rest.len() < 2 {
                return self.hold(input, text_from, start, pieces);
            }
            if rest[1] != b']' {
                at = start + 1;
                continue;
            }
            let body = &rest[2..];
            let wanted = [MARK, IMAGE]
                .into_iter()
                .find(|prefix| body.starts_with(prefix));
            let Some(prefix) = wanted else {
                let partial = [MARK, IMAGE]
                    .iter()
                    .any(|prefix| body.len() < prefix.len() && prefix.starts_with(body));
                if partial {
                    return self.hold(input, text_from, start, pieces);
                }
                at = start + 2;
                continue;
            };
            match terminator(body) {
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
                    } else {
                        Piece::Image(content.to_vec())
                    });
                    at = start + 2 + after;
                    text_from = at;
                }
                None if rest.len() <= HOLD_AT_MOST => {
                    return self.hold(input, text_from, start, pieces);
                }
                // Too long to be one: it's output.
                None => at = start + 2,
            }
        }
        if input.len() > text_from {
            pieces.push(Piece::Text(input[text_from..].to_vec()));
        }
        pieces.retain(|piece| piece != &Piece::Text(Vec::new()));
        pieces
    }

    /// Gives the output before `from` and keeps the rest for the next chunk.
    fn hold(
        &mut self,
        input: &[u8],
        text_from: usize,
        from: usize,
        mut pieces: Vec<Piece>,
    ) -> Vec<Piece> {
        if from > text_from {
            pieces.push(Piece::Text(input[text_from..from].to_vec()));
        }
        self.held = input[from..].to_vec();
        pieces.retain(|piece| piece != &Piece::Text(Vec::new()));
        pieces
    }
}

/// Where an OSC sequence's content ends and what follows its terminator
/// (BEL, or `ESC \`), within `body`.
fn terminator(body: &[u8]) -> Option<(usize, usize)> {
    body.iter().enumerate().find_map(|(at, &byte)| match byte {
        0x07 => Some((at, at + 1)),
        0x1b if body.get(at + 1) == Some(&b'\\') => Some((at, at + 2)),
        _ => None,
    })
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
    }
}
