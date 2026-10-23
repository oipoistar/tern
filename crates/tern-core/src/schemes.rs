//! Terminal colour schemes: the colours terminal text is drawn in. Tern's
//! own scheme (no name chosen) follows the app's light or dark look; these
//! are well-known schemes, as their authors publish them.

/// A scheme's colours, as `0xRRGGBB`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scheme {
    pub name: &'static str,
    pub background: u32,
    pub foreground: u32,
    pub cursor: u32,
    /// The 16 ANSI colours: normal 0–7, then bright 8–15.
    pub ansi: [u32; 16],
}

const SOLARIZED: [u32; 16] = [
    0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5, //
    0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
];

/// Every scheme there is to choose, besides Tern's own.
pub const SCHEMES: [Scheme; 9] = [
    Scheme {
        name: "Solarized Dark",
        background: 0x002b36,
        foreground: 0x839496,
        cursor: 0x93a1a1,
        ansi: SOLARIZED,
    },
    Scheme {
        name: "Solarized Light",
        background: 0xfdf6e3,
        foreground: 0x586e75,
        cursor: 0x586e75,
        ansi: SOLARIZED,
    },
    Scheme {
        name: "Dracula",
        background: 0x282a36,
        foreground: 0xf8f8f2,
        cursor: 0xf8f8f2,
        ansi: [
            0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2, //
            0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
        ],
    },
    Scheme {
        name: "Nord",
        background: 0x2e3440,
        foreground: 0xd8dee9,
        cursor: 0xd8dee9,
        ansi: [
            0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0, //
            0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
        ],
    },
    Scheme {
        name: "Gruvbox Dark",
        background: 0x282828,
        foreground: 0xebdbb2,
        cursor: 0xebdbb2,
        ansi: [
            0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984, //
            0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
        ],
    },
    Scheme {
        name: "One Dark",
        background: 0x282c34,
        foreground: 0xabb2bf,
        cursor: 0x528bff,
        ansi: [
            0x282c34, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf, //
            0x5c6370, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
        ],
    },
    Scheme {
        name: "Tokyo Night",
        background: 0x1a1b26,
        foreground: 0xc0caf5,
        cursor: 0xc0caf5,
        ansi: [
            0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6, //
            0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
        ],
    },
    Scheme {
        name: "Monokai",
        background: 0x272822,
        foreground: 0xf8f8f2,
        cursor: 0xf8f8f0,
        ansi: [
            0x272822, 0xf92672, 0xa6e22e, 0xf4bf75, 0x66d9ef, 0xae81ff, 0xa1efe4, 0xf8f8f2, //
            0x75715e, 0xf92672, 0xa6e22e, 0xf4bf75, 0x66d9ef, 0xae81ff, 0xa1efe4, 0xf9f8f5,
        ],
    },
    Scheme {
        name: "GitHub Light",
        background: 0xffffff,
        foreground: 0x24292f,
        cursor: 0x0969da,
        ansi: [
            0x24292f, 0xcf222e, 0x116329, 0x4d2d00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781, //
            0x57606a, 0xa40e26, 0x1a7f37, 0x633c01, 0x218bff, 0xa475f9, 0x3192aa, 0x8c959f,
        ],
    },
];

/// The scheme called `name`, ignoring case.
pub fn find(name: &str) -> Option<&'static Scheme> {
    SCHEMES
        .iter()
        .find(|scheme| scheme.name.eq_ignore_ascii_case(name.trim()))
}

/// WCAG's contrast ratio between two colours, 1 to 21.
pub fn contrast(a: u32, b: u32) -> f32 {
    let luminance = |color: u32| {
        let channel = |shift: u32| {
            let value = ((color >> shift) & 0xff) as f32 / 255.;
            if value <= 0.03928 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemes_are_distinct_and_readable() {
        for (at, scheme) in SCHEMES.iter().enumerate() {
            assert!(
                SCHEMES[..at].iter().all(|other| other.name != scheme.name),
                "{} twice",
                scheme.name
            );
            assert_eq!(find(&scheme.name.to_uppercase()), Some(scheme));
            // Text on the background reads, as each scheme intends.
            let ratio = contrast(scheme.foreground, scheme.background);
            assert!(ratio >= 4.5, "{}: contrast {ratio}", scheme.name);
        }
        assert_eq!(find("No such scheme"), None);
        assert!((contrast(0x000000, 0xffffff) - 21.).abs() < 0.01);
        assert!((contrast(0x777777, 0x777777) - 1.).abs() < 0.01);
    }
}
