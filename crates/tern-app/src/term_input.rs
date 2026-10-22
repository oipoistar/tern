//! What a terminal sends the remote program for keys and the mouse, as
//! xterm does.

use alacritty_terminal::term::TermMode;
use gpui_kit::{Keystroke, Modifiers, MouseButton};

/// Encodes a key press as the bytes an xterm-compatible terminal would send.
pub fn encode_key(keystroke: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let m = &keystroke.modifiers;
    let key = keystroke.key.as_str();
    // xterm's modifier parameter: 1 + shift + 2·alt + 4·ctrl.
    let modifier = 1 + u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.control);
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let csi = |code: &str, final_byte: char| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[{code};{modifier}{final_byte}").into_bytes()
        } else {
            format!("\x1b[{code}{final_byte}").into_bytes()
        }
    };
    let arrow = |letter: char| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{letter}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };
    let bytes = match key {
        "enter" => b"\r".to_vec(),
        "tab" if m.shift => b"\x1b[Z".to_vec(),
        "tab" => b"\t".to_vec(),
        "backspace" if m.alt => b"\x1b\x7f".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "escape" => b"\x1b".to_vec(),
        "up" => arrow('A'),
        "down" => arrow('B'),
        "right" => arrow('C'),
        "left" => arrow('D'),
        "home" => arrow('H'),
        "end" => arrow('F'),
        "insert" => csi("2", '~'),
        "delete" => csi("3", '~'),
        "pageup" => csi("5", '~'),
        "pagedown" => csi("6", '~'),
        "f1" => b"\x1bOP".to_vec(),
        "f2" => b"\x1bOQ".to_vec(),
        "f3" => b"\x1bOR".to_vec(),
        "f4" => b"\x1bOS".to_vec(),
        "f5" => csi("15", '~'),
        "f6" => csi("17", '~'),
        "f7" => csi("18", '~'),
        "f8" => csi("19", '~'),
        "f9" => csi("20", '~'),
        "f10" => csi("21", '~'),
        "f11" => csi("23", '~'),
        "f12" => csi("24", '~'),
        "space" if m.control => vec![0],
        _ if m.control && !m.alt && key.len() == 1 => {
            let c = key.as_bytes()[0].to_ascii_lowercase();
            match c {
                b'a'..=b'z' => vec![c - b'a' + 1],
                b'[' => vec![0x1b],
                b'\\' => vec![0x1c],
                b']' => vec![0x1d],
                b'2' | b'@' => vec![0],
                b'6' | b'^' => vec![0x1e],
                // Ctrl+- zooms the font, so readline's undo is Ctrl+/ too.
                b'-' | b'_' | b'/' => vec![0x1f],
                _ => return None,
            }
        }
        // Alt+character is ESC then the character (meta key).
        _ if m.alt && !m.control && !m.platform => {
            let text = keystroke.key_char.as_deref().unwrap_or(key);
            [b"\x1b".as_slice(), text.as_bytes()].concat()
        }
        // Plain text arrives through the input handler instead, which also
        // covers IME and characters injected without a key code.
        _ => return None,
    };
    Some(bytes)
}

/// A mouse event a program can ask to hear about (vim, htop, tmux).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MouseReport {
    Press(MouseButton),
    Release(MouseButton),
    /// The pointer moved with this button held.
    Drag(MouseButton),
    /// The pointer moved with no button held.
    Move,
    WheelUp,
    WheelDown,
}

/// The bytes reporting `report` in the screen cell at `column` and `line`
/// (from 0, top left), or `None` when the program didn't ask for this kind
/// of report or the cell is too far out for its encoding.
pub fn encode_mouse(
    report: MouseReport,
    column: usize,
    line: usize,
    modifiers: &Modifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let wanted = match report {
        MouseReport::Drag(_) => TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION,
        MouseReport::Move => TermMode::MOUSE_MOTION,
        _ => TermMode::MOUSE_MODE,
    };
    if !mode.intersects(wanted) {
        return None;
    }
    let button = |button: MouseButton| match button {
        MouseButton::Left => Some(0),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
        _ => None,
    };
    let sgr = mode.contains(TermMode::SGR_MOUSE);
    let code = match report {
        MouseReport::Press(pressed) => button(pressed)?,
        // Only SGR says which button came up.
        MouseReport::Release(released) if sgr => button(released)?,
        MouseReport::Release(released) => button(released).map(|_| 3)?,
        MouseReport::Drag(held) => 32 + button(held)?,
        MouseReport::Move => 35,
        MouseReport::WheelUp => 64,
        MouseReport::WheelDown => 65,
    } + 4 * u32::from(modifiers.shift)
        + 8 * u32::from(modifiers.alt)
        + 16 * u32::from(modifiers.control);

    if sgr {
        let end = if matches!(report, MouseReport::Release(_)) {
            'm'
        } else {
            'M'
        };
        return Some(format!("\x1b[<{code};{};{}{end}", column + 1, line + 1).into_bytes());
    }
    // The classic encoding: each value as one byte offset by 32, or with
    // UTF-8 mode as a character, which reaches further.
    let mut bytes = b"\x1b[M".to_vec();
    for value in [code, column as u32 + 1, line as u32 + 1] {
        let value = value + 32;
        if mode.contains(TermMode::UTF8_MOUSE) {
            let c = char::from_u32(value).filter(|_| value < 0x800)?;
            bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
        } else {
            bytes.push(u8::try_from(value).ok()?);
        }
    }
    Some(bytes)
}

/// What a turn of the wheel sends a full-screen program that doesn't take
/// the mouse (less, man): arrow keys, `lines` up when positive, else down.
pub fn wheel_keys(lines: i32, mode: TermMode) -> Vec<u8> {
    let letter = if lines > 0 { 'A' } else { 'B' };
    let key = if mode.contains(TermMode::APP_CURSOR) {
        format!("\x1bO{letter}")
    } else {
        format!("\x1b[{letter}")
    };
    key.repeat(lines.unsigned_abs() as usize).into_bytes()
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::term::TermMode;
    use gpui_kit::{Keystroke, Modifiers, MouseButton};

    use super::{MouseReport, encode_key, encode_mouse, wheel_keys};

    fn key(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.into(),
            key_char: key_char.map(Into::into),
        }
    }

    fn ctrl() -> Modifiers {
        Modifiers {
            control: true,
            ..Default::default()
        }
    }

    #[test]
    fn encodes_common_keys() {
        let none = TermMode::empty();
        // Plain text is left to the input handler.
        assert_eq!(
            encode_key(&key("a", Some("a"), Modifiers::default()), none),
            None
        );
        assert_eq!(
            encode_key(&key("enter", None, Modifiers::default()), none),
            Some(b"\r".to_vec())
        );
        assert_eq!(encode_key(&key("c", None, ctrl()), none), Some(vec![3]));
        assert_eq!(encode_key(&key("/", None, ctrl()), none), Some(vec![0x1f]));
        assert_eq!(
            encode_key(&key("up", None, Modifiers::default()), none),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key(&key("up", None, Modifiers::default()), TermMode::APP_CURSOR),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            encode_key(&key("right", None, ctrl()), none),
            Some(b"\x1b[1;5C".to_vec())
        );
        assert_eq!(
            encode_key(&key("delete", None, Modifiers::default()), none),
            Some(b"\x1b[3~".to_vec())
        );
    }

    #[test]
    fn alt_prefixes_escape() {
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(
            encode_key(&key("b", Some("b"), alt), TermMode::empty()),
            Some(b"\x1bb".to_vec())
        );
    }

    #[test]
    fn reports_the_mouse_only_when_asked() {
        let none = Modifiers::default();
        let left = MouseButton::Left;
        let press = |mode| encode_mouse(MouseReport::Press(left), 4, 1, &none, mode);
        assert_eq!(press(TermMode::empty()), None);

        // Classic: button, column and line, each offset by 32.
        let clicks = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(press(clicks), Some(b"\x1b[M %\"".to_vec()));
        assert_eq!(
            encode_mouse(MouseReport::Release(left), 4, 1, &none, clicks),
            Some(b"\x1b[M#%\"".to_vec())
        );
        // Click reporting alone doesn't want drags or motion.
        assert_eq!(
            encode_mouse(MouseReport::Drag(left), 4, 1, &none, clicks),
            None
        );
        let drags = TermMode::MOUSE_DRAG;
        assert_eq!(
            encode_mouse(MouseReport::Drag(left), 4, 1, &none, drags),
            Some(b"\x1b[M@%\"".to_vec())
        );
        assert_eq!(encode_mouse(MouseReport::Move, 4, 1, &none, drags), None);
        // Too far right for one byte, unless UTF-8 mode is on.
        assert_eq!(
            encode_mouse(MouseReport::Press(left), 300, 0, &none, clicks),
            None
        );
        assert_eq!(
            encode_mouse(
                MouseReport::Press(left),
                300,
                0,
                &none,
                clicks | TermMode::UTF8_MOUSE
            ),
            Some("\x1b[M \u{14d}!".as_bytes().to_vec())
        );

        // SGR: decimal values, and release says which button.
        let sgr = clicks | TermMode::SGR_MOUSE;
        assert_eq!(
            encode_mouse(MouseReport::Press(MouseButton::Right), 300, 1, &none, sgr),
            Some(b"\x1b[<2;301;2M".to_vec())
        );
        assert_eq!(
            encode_mouse(MouseReport::Release(MouseButton::Right), 0, 0, &none, sgr),
            Some(b"\x1b[<2;1;1m".to_vec())
        );
        assert_eq!(
            encode_mouse(MouseReport::WheelDown, 0, 0, &ctrl(), sgr),
            Some(b"\x1b[<81;1;1M".to_vec())
        );
    }

    #[test]
    fn the_wheel_sends_arrows_to_full_screen_programs() {
        assert_eq!(wheel_keys(2, TermMode::empty()), b"\x1b[A\x1b[A".to_vec());
        assert_eq!(wheel_keys(-1, TermMode::APP_CURSOR), b"\x1bOB".to_vec());
    }
}
