//! Session recordings in asciinema's asciicast v2 format: a JSON header,
//! then one `[seconds, "o", text]` line per stretch of output (and
//! `[seconds, "r", "COLSxROWS"]` when the terminal resizes). Only what the
//! terminal showed is kept, never what was typed, so a password typed at a
//! prompt isn't in it. Files play in Tern and in asciinema alike.

use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// How long written events may wait in memory before reaching the file.
const FLUSH_EVERY: Duration = Duration::from_millis(250);
/// The largest terminal a recording plays in. A recording names its size,
/// and its terminal is made that size before anything plays: one naming
/// more (`65535x65535` is four billion cells) plays at this size instead.
pub const MAX_COLS: u16 = 500;
pub const MAX_ROWS: u16 = 300;
/// The largest recording read, and the most events played, so opening one
/// never fills memory.
pub const MAX_BYTES: u64 = 128 << 20;
const MAX_EVENTS: usize = 4 << 20;

/// A recording being written.
pub struct Recorder {
    file: BufWriter<File>,
    started: Instant,
    flushed: Instant,
    /// The start of a character split across chunks of output.
    carry: Vec<u8>,
}

impl Recorder {
    /// Starts a recording into `path` of a `cols` by `rows` terminal.
    pub fn start(path: &Path, cols: u16, rows: u16, title: &str) -> std::io::Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let header = json!({
            "version": 2,
            "width": cols,
            "height": rows,
            "timestamp": timestamp,
            "title": title,
        });
        writeln!(file, "{header}")?;
        Ok(Self {
            file,
            started: Instant::now(),
            flushed: Instant::now(),
            carry: Vec::new(),
        })
    }

    /// Adds what the terminal was given. A character split between two
    /// chunks waits for its end.
    pub fn output(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.carry.extend_from_slice(bytes);
        let whole = match std::str::from_utf8(&self.carry) {
            Ok(_) => self.carry.len(),
            // Up to a broken character: the rest may still be coming, or
            // it's broken for good and shown as such.
            Err(error) => match error.error_len() {
                None => error.valid_up_to(),
                Some(_) => self.carry.len(),
            },
        };
        if whole == 0 {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&self.carry[..whole]).into_owned();
        self.carry.drain(..whole);
        self.event("o", &text)
    }

    /// Notes that the terminal is now `cols` by `rows`.
    pub fn resize(&mut self, cols: u16, rows: u16) -> std::io::Result<()> {
        self.event("r", &format!("{cols}x{rows}"))
    }

    fn event(&mut self, kind: &str, data: &str) -> std::io::Result<()> {
        let at = self.started.elapsed().as_secs_f64();
        writeln!(
            self.file,
            "{}",
            json!([(at * 1e6).round() / 1e6, kind, data])
        )?;
        if self.flushed.elapsed() >= FLUSH_EVERY {
            self.file.flush()?;
            self.flushed = Instant::now();
        }
        Ok(())
    }

    /// Ends the recording, with all of it in the file.
    pub fn finish(mut self) -> std::io::Result<()> {
        if !self.carry.is_empty() {
            let rest = String::from_utf8_lossy(&std::mem::take(&mut self.carry)).into_owned();
            self.event("o", &rest)?;
        }
        self.file.flush()
    }
}

/// What a recording shows over time.
#[derive(Clone, Debug, PartialEq)]
pub enum Shown {
    Output(String),
    Resize(u16, u16),
}

/// A recording read back.
#[derive(Clone, Debug, PartialEq)]
pub struct Cast {
    pub cols: u16,
    pub rows: u16,
    pub title: Option<String>,
    /// When each thing was shown, in seconds from the start, in order.
    pub events: Vec<(f64, Shown)>,
}

impl Cast {
    /// How long it lasts.
    pub fn duration(&self) -> f64 {
        self.events.last().map_or(0., |(at, _)| *at)
    }
}

/// Reads the recording in the file `path`, as [`read`] does; one too big
/// to play says so.
pub fn load(path: &Path) -> Result<Cast, String> {
    let size = std::fs::metadata(path).map_err(|error| error.to_string())?;
    if size.len() > MAX_BYTES {
        return Err("It's too big to play".into());
    }
    read(&std::fs::read_to_string(path).map_err(|error| error.to_string())?)
}

/// Reads an asciicast v2 recording. Input and marker events are skipped.
/// Its terminal is at most [`MAX_COLS`] by [`MAX_ROWS`].
pub fn read(text: &str) -> Result<Cast, String> {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let header: Value = lines
        .next()
        .and_then(|line| serde_json::from_str(line).ok())
        .ok_or("It isn't a recording")?;
    if header["version"] != 2 {
        return Err("Only asciicast v2 recordings play".into());
    }
    let size = |field: &str, most: u16| {
        header[field]
            .as_u64()
            .filter(|&size| size > 0)
            .map(|size| size.min(most.into()) as u16)
            .ok_or("Its terminal size is missing")
    };
    let (cols, rows) = (size("width", MAX_COLS)?, size("height", MAX_ROWS)?);
    let mut events = Vec::new();
    for line in lines {
        if events.len() == MAX_EVENTS {
            return Err("It's too long to play".into());
        }
        let event: Value = serde_json::from_str(line).map_err(|error| error.to_string())?;
        let (Some(at), Some(kind), Some(data)) =
            (event[0].as_f64(), event[1].as_str(), event[2].as_str())
        else {
            return Err("A line of it isn't an event".into());
        };
        let shown = match kind {
            "o" => Shown::Output(data.to_string()),
            "r" => {
                let Some((cols, rows)) = data.split_once('x') else {
                    continue;
                };
                match (cols.parse::<u16>(), rows.parse::<u16>()) {
                    (Ok(cols), Ok(rows)) if cols > 0 && rows > 0 => {
                        Shown::Resize(cols.min(MAX_COLS), rows.min(MAX_ROWS))
                    }
                    _ => continue,
                }
            }
            _ => continue,
        };
        events.push((at.max(0.), shown));
    }
    // Played in time order, whatever order they were written in.
    events.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok(Cast {
        cols,
        rows,
        title: header["title"].as_str().map(str::to_string),
        events,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_what_was_shown_and_reads_it_back() {
        let path = std::env::temp_dir().join(format!("tern-cast-{}.cast", std::process::id()));
        let mut recorder = Recorder::start(&path, 100, 30, "web-01").unwrap();
        recorder.output(b"hello ").unwrap();
        // "é" split between two chunks stays one character.
        recorder.output(&[0xc3]).unwrap();
        recorder.output(&[0xa9, b'\r', b'\n']).unwrap();
        recorder.resize(120, 40).unwrap();
        recorder.output(b"\x1b[1mbold\x1b[0m").unwrap();
        recorder.finish().unwrap();

        let cast = read(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((cast.cols, cast.rows), (100, 30));
        assert_eq!(cast.title.as_deref(), Some("web-01"));
        let shown: Vec<&Shown> = cast.events.iter().map(|(_, shown)| shown).collect();
        assert_eq!(
            shown,
            [
                &Shown::Output("hello ".into()),
                &Shown::Output("é\r\n".into()),
                &Shown::Resize(120, 40),
                &Shown::Output("\x1b[1mbold\x1b[0m".into()),
            ]
        );
        assert!(cast.events.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reads_asciinemas_own_recordings() {
        let text = "{\"version\": 2, \"width\": 80, \"height\": 24, \"timestamp\": 1504467315, \"env\": {\"SHELL\": \"/bin/zsh\"}}\n\
                    [0.248848, \"o\", \"\\u001b[1;31mHello \\u001b[32mWorld!\\u001b[0m\\n\"]\n\
                    [1.001376, \"i\", \"ls\\r\"]\n\
                    [1.5, \"m\", \"\"]\n\
                    [2.5, \"o\", \"done\"]\n";
        let cast = read(text).unwrap();
        assert_eq!(
            (cast.cols, cast.rows, cast.title.as_deref()),
            (80, 24, None)
        );
        assert_eq!(cast.events.len(), 2);
        assert_eq!(cast.duration(), 2.5);
        assert!(read("not a recording").is_err());
        assert!(read("{\"version\": 1, \"width\": 80, \"height\": 24}").is_err());
    }

    #[test]
    fn plays_in_a_terminal_of_reasonable_size() {
        // A few bytes naming four billion cells, then more.
        let cast = read(
            "{\"version\": 2, \"width\": 65535, \"height\": 65535}\n\
             [1, \"r\", \"65535x65535\"]\n[2, \"r\", \"100000x9\"]\n[3, \"r\", \"120x40\"]\n",
        )
        .unwrap();
        assert_eq!((cast.cols, cast.rows), (MAX_COLS, MAX_ROWS));
        let shown: Vec<&Shown> = cast.events.iter().map(|(_, shown)| shown).collect();
        assert_eq!(
            shown,
            [&Shown::Resize(MAX_COLS, MAX_ROWS), &Shown::Resize(120, 40)]
        );
        let huge = read("{\"version\": 2, \"width\": 4294967296, \"height\": 9}").unwrap();
        assert_eq!((huge.cols, huge.rows), (MAX_COLS, 9));

        // And a file too big to read isn't.
        let path = std::env::temp_dir().join(format!("tern-cast-big-{}.cast", std::process::id()));
        File::create(&path).unwrap().set_len(MAX_BYTES + 1).unwrap();
        assert_eq!(load(&path).unwrap_err(), "It's too big to play");
        let _ = std::fs::remove_file(&path);
    }
}
