//! Typing that doesn't wait: on a slow connection, what's typed shows at
//! once, faintly, until the server echoes it, as mosh does.
//!
//! A guess shows only on a line where the server has already echoed one,
//! and a new line (Enter, or output that feeds a line) starts over with
//! guesses that don't show until one is echoed. Input a program hides, like
//! a password, is never echoed, so it never shows. A wrong guess drops them
//! all.

use std::time::{Duration, Instant};

/// Echoes slower than this, smoothed, show guesses.
pub const SHOW_AFTER: Duration = Duration::from_millis(20);
/// Guesses the server hasn't echoed by then were wrong.
pub const GIVE_UP: Duration = Duration::from_millis(1500);

/// A place in the terminal's grid: its line and column.
pub type Cell = (i32, usize);

/// A character typed but not yet echoed, where it's expected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guess {
    pub at: Cell,
    pub c: char,
    made: Instant,
}

/// The guesses of one terminal.
#[derive(Debug, Default)]
pub struct Predictor {
    guesses: Vec<Guess>,
    /// The line where an echo matched a guess: guesses show only there.
    trusted: Option<i32>,
    /// A key was sent whose effect isn't guessed: no guesses until the
    /// server answers.
    waiting: bool,
    /// How long echoes take, smoothed.
    echo: Option<Duration>,
}

impl Predictor {
    /// `c` was typed with the cursor at `cursor`. `blank_after` says the
    /// cursor's line is empty from there on, and `cols` is the grid's width.
    pub fn typed(&mut self, c: char, cursor: Cell, blank_after: bool, cols: usize, now: Instant) {
        if self.waiting || !(' '..='~').contains(&c) {
            self.waiting = true;
            return;
        }
        let at = match self.guesses.last() {
            Some(last) => (last.at.0, last.at.1 + 1),
            None if blank_after => cursor,
            // Typed into the middle of a line: the rest of it would move.
            None => {
                self.waiting = true;
                return;
            }
        };
        // The line's last column wraps, which isn't guessed.
        if at.1 + 1 >= cols {
            self.waiting = true;
            return;
        }
        self.guesses.push(Guess { at, c, made: now });
    }

    /// Backspace: takes back the last guess. With none to take back it
    /// erases what's echoed, which the server shows.
    pub fn erased(&mut self) {
        if self.guesses.pop().is_none() {
            self.waiting = true;
        }
    }

    /// A key whose effect isn't guessed (arrows, Tab, Ctrl+C). Enter also
    /// starts a new line, where guesses don't show until one is echoed.
    pub fn other_key(&mut self, enter: bool) {
        self.waiting = true;
        if enter {
            self.trusted = None;
        }
    }

    /// The server's output was shown: the cursor is at `cursor`, `cell`
    /// reads the grid, and `line_fed` says the output fed a line.
    pub fn output(
        &mut self,
        cursor: Cell,
        line_fed: bool,
        cell: impl Fn(Cell) -> char,
        now: Instant,
    ) {
        self.waiting = false;
        if line_fed {
            self.trusted = None;
        }
        while let Some(guess) = self.guesses.first() {
            let (line, col) = guess.at;
            if cursor.0 == line && cursor.1 <= col {
                // Not echoed yet.
                break;
            }
            if cursor.0 != line || cell(guess.at) != guess.c {
                self.drop_all();
                return;
            }
            let took = now.saturating_duration_since(guess.made);
            self.echo = Some(match self.echo {
                Some(echo) => (echo * 7 + took) / 8,
                None => took,
            });
            self.trusted = Some(line);
            self.guesses.remove(0);
        }
        if self.trusted.is_some_and(|line| line != cursor.0) {
            self.trusted = None;
        }
    }

    /// Gives up on guesses the server hasn't echoed for [`GIVE_UP`];
    /// whether any were shown.
    pub fn expire(&mut self, now: Instant) -> bool {
        let old = self
            .guesses
            .first()
            .is_some_and(|guess| now.saturating_duration_since(guess.made) >= GIVE_UP);
        if !old {
            return false;
        }
        let shown = !self.shown().is_empty();
        self.drop_all();
        shown
    }

    /// Forgets every guess (the screen changed under them).
    pub fn clear(&mut self) {
        self.guesses.clear();
        self.trusted = None;
        self.waiting = false;
    }

    /// Nothing guessed or trusted: output needn't be looked at.
    pub fn is_idle(&self) -> bool {
        self.guesses.is_empty() && self.trusted.is_none() && !self.waiting
    }

    /// The guesses to draw: none until an echo on their line matched one,
    /// or while echoes come back quickly.
    pub fn shown(&self) -> &[Guess] {
        let slow = self.echo.is_some_and(|echo| echo >= SHOW_AFTER);
        let line = self.guesses.first().map(|guess| guess.at.0);
        if slow && line.is_some() && line == self.trusted {
            &self.guesses
        } else {
            &[]
        }
    }

    /// Where the cursor shows while guesses do: after the last one.
    pub fn cursor(&self) -> Option<Cell> {
        self.shown()
            .last()
            .map(|guess| (guess.at.0, guess.at.1 + 1))
    }

    fn drop_all(&mut self) {
        self.guesses.clear();
        self.trusted = None;
    }
}

/// Whether the text before the cursor asks for something kept hidden,
/// which is never guessed even where echoes were seen.
pub fn asks_secret(before_cursor: &str) -> bool {
    let text = before_cursor.to_lowercase();
    [
        "password",
        "passphrase",
        "passcode",
        "pin:",
        "verification code",
        "one-time",
    ]
    .iter()
    .any(|word| text.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-line screen: what's on it, and the cursor's column.
    struct Screen {
        text: Vec<char>,
        cursor: usize,
    }

    impl Screen {
        fn new(prompt: &str) -> Self {
            Screen {
                text: prompt.chars().collect(),
                cursor: prompt.len(),
            }
        }

        fn echo(&mut self, text: &str) {
            for c in text.chars() {
                if self.cursor < self.text.len() {
                    self.text[self.cursor] = c;
                } else {
                    self.text.push(c);
                }
                self.cursor += 1;
            }
        }

        fn cell(&self) -> impl Fn(Cell) -> char + '_ {
            |(_, col)| self.text.get(col).copied().unwrap_or(' ')
        }
    }

    fn type_text(predictor: &mut Predictor, screen: &Screen, text: &str, now: Instant) {
        for c in text.chars() {
            predictor.typed(c, (0, screen.cursor), true, 80, now);
        }
    }

    fn shown(predictor: &Predictor) -> String {
        predictor.shown().iter().map(|guess| guess.c).collect()
    }

    #[test]
    fn shows_typing_once_the_line_echoes_slowly() {
        let mut predictor = Predictor::default();
        let mut screen = Screen::new("$ ");
        let start = Instant::now();

        // The first key on a line doesn't show: it might be a password.
        type_text(&mut predictor, &screen, "l", start);
        assert_eq!(shown(&predictor), "");
        let later = start + Duration::from_millis(120);
        screen.echo("l");
        predictor.output((0, screen.cursor), false, screen.cell(), later);

        // Echoed, slowly: what's typed now shows at once, the cursor after it.
        type_text(&mut predictor, &screen, "s -", later);
        assert_eq!(shown(&predictor), "s -");
        assert_eq!(predictor.cursor(), Some((0, 6)));

        // Echoes take guesses away as they come.
        screen.echo("s ");
        predictor.output((0, screen.cursor), false, screen.cell(), later);
        assert_eq!(shown(&predictor), "-");
        type_text(&mut predictor, &screen, "l", later);
        assert_eq!(shown(&predictor), "-l");
        assert_eq!(predictor.shown()[1].at, (0, 6));
    }

    #[test]
    fn hidden_input_never_shows() {
        let mut predictor = Predictor::default();
        let mut screen = Screen::new("$ ");
        let start = Instant::now();
        type_text(&mut predictor, &screen, "s", start);
        screen.echo("s");
        predictor.output(
            (0, 3),
            false,
            screen.cell(),
            start + Duration::from_millis(80),
        );
        assert!(predictor.echo.is_some());

        // Enter: sudo asks for a password on the next line, which isn't echoed.
        predictor.other_key(true);
        let mut prompt = Screen::new("[sudo] password for lab: ");
        predictor.output((1, prompt.cursor), true, prompt.cell(), start);
        for c in "hunter2".chars() {
            predictor.typed(c, (1, prompt.cursor), true, 80, start);
            assert_eq!(shown(&predictor), "");
        }
        prompt.echo("");
        assert!(!predictor.expire(start + GIVE_UP));
        assert!(predictor.guesses.is_empty());
        assert!(asks_secret("[sudo] password for lab: "));
        assert!(asks_secret(
            "Enter passphrase for key '/home/lab/.ssh/id_ed25519': "
        ));
        assert!(!asks_secret("lab@web-01:~$ ping "));
    }

    #[test]
    fn a_wrong_guess_drops_them_all() {
        let mut predictor = Predictor::default();
        let mut screen = Screen::new("> ");
        let start = Instant::now();
        let later = start + Duration::from_millis(50);
        type_text(&mut predictor, &screen, "a", start);
        screen.echo("a");
        predictor.output((0, screen.cursor), false, screen.cell(), later);
        type_text(&mut predictor, &screen, "bc", later);
        assert_eq!(shown(&predictor), "bc");

        // The program shows something else (it upper-cases, say).
        screen.echo("B");
        predictor.output((0, screen.cursor), false, screen.cell(), later);
        assert_eq!(shown(&predictor), "");
        assert!(predictor.guesses.is_empty());
        // And what's typed next doesn't show until an echo matches again.
        type_text(&mut predictor, &screen, "d", later);
        assert_eq!(shown(&predictor), "");
    }

    #[test]
    fn backspace_takes_back_a_guess() {
        let mut predictor = Predictor::default();
        let mut screen = Screen::new("$ ");
        let start = Instant::now();
        type_text(&mut predictor, &screen, "c", start);
        screen.echo("c");
        let later = start + Duration::from_millis(40);
        predictor.output((0, screen.cursor), false, screen.cell(), later);
        type_text(&mut predictor, &screen, "dx", later);
        predictor.erased();
        assert_eq!(shown(&predictor), "d");

        // Erasing what's echoed waits for the server.
        predictor.erased();
        predictor.erased();
        type_text(&mut predictor, &screen, "y", later);
        assert_eq!(shown(&predictor), "");
    }

    #[test]
    fn guesses_only_what_it_can_place() {
        let mut predictor = Predictor::default();
        let now = Instant::now();
        // Something follows the cursor: typing would push it along.
        predictor.typed('a', (0, 2), false, 80, now);
        assert!(predictor.guesses.is_empty());
        predictor.output((0, 2), false, |_| ' ', now);
        // Not at the line's last column, and only plain characters.
        predictor.typed('a', (0, 79), true, 80, now);
        assert!(predictor.guesses.is_empty());
        predictor.output((0, 2), false, |_| ' ', now);
        predictor.typed('é', (0, 2), true, 80, now);
        predictor.typed('a', (0, 2), true, 80, now);
        assert!(predictor.guesses.is_empty());
    }

    #[test]
    fn quick_echoes_show_nothing_and_old_guesses_go() {
        let mut predictor = Predictor::default();
        let mut screen = Screen::new("$ ");
        let start = Instant::now();
        type_text(&mut predictor, &screen, "l", start);
        screen.echo("l");
        predictor.output(
            (0, 3),
            false,
            screen.cell(),
            start + Duration::from_millis(2),
        );
        type_text(&mut predictor, &screen, "s", start);
        assert_eq!(shown(&predictor), "");

        // Slow now, then the server stops answering.
        predictor.echo = Some(Duration::from_millis(300));
        assert_eq!(shown(&predictor), "s");
        assert!(!predictor.expire(start + GIVE_UP / 2));
        assert!(predictor.expire(start + GIVE_UP));
        assert_eq!(shown(&predictor), "");
    }

    #[test]
    fn a_fed_line_starts_over() {
        let mut predictor = Predictor::default();
        let mut screen = Screen::new("$ ");
        let start = Instant::now();
        let later = start + Duration::from_millis(60);
        type_text(&mut predictor, &screen, "l", start);
        screen.echo("l");
        predictor.output((0, 3), false, screen.cell(), later);
        // A job's output arrives on the same line index, the screen scrolled.
        predictor.output((0, 3), true, screen.cell(), later);
        type_text(&mut predictor, &screen, "s", later);
        assert_eq!(shown(&predictor), "");
    }
}
