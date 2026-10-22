//! OpenSSH's "drunken bishop" picture of a host-key fingerprint. A bishop
//! starts in the middle of a board and, for each 2-bit pair of the
//! fingerprint (low bits first), steps one square diagonally without leaving
//! the board. How often it lands on each square makes a shape that is easy to
//! tell apart at a glance, so a changed key looks different.

/// Squares per side. OpenSSH draws a 17×9 field; Tern's tile is square.
pub const SIZE: usize = 11;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Randomart {
    /// Landings per square, row by row.
    visits: [u8; SIZE * SIZE],
    /// The square the bishop stopped on, as (column, row).
    end: (usize, usize),
}

impl Randomart {
    pub fn new(fingerprint: &[u8]) -> Self {
        let mut visits = [0u8; SIZE * SIZE];
        let (mut x, mut y) = (SIZE / 2, SIZE / 2);
        for byte in fingerprint {
            for shift in (0..8).step_by(2) {
                let step = (byte >> shift) & 3;
                x = if step & 1 == 1 {
                    (x + 1).min(SIZE - 1)
                } else {
                    x.saturating_sub(1)
                };
                y = if step & 2 == 2 {
                    (y + 1).min(SIZE - 1)
                } else {
                    y.saturating_sub(1)
                };
                let square = &mut visits[y * SIZE + x];
                *square = square.saturating_add(1);
            }
        }
        Self {
            visits,
            end: (x, y),
        }
    }

    /// How often the bishop landed on column `x`, row `y`.
    pub fn visits(&self, x: usize, y: usize) -> u8 {
        self.visits[y * SIZE + x]
    }

    pub fn end(&self) -> (usize, usize) {
        self.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_from_the_centre_low_bits_first() {
        // Pairs, low first: 11 (right, down), 10 (left, down), 01 (right,
        // up), 00 (left, up).
        let art = Randomart::new(&[0b00_01_10_11]);
        assert_eq!(art.visits(6, 6), 2);
        assert_eq!(art.visits(5, 7), 1);
        assert_eq!(art.visits(5, 5), 1);
        assert_eq!(art.end(), (5, 5));
    }

    #[test]
    fn stays_on_the_board() {
        // 32 steps up-left: four squares on the way, then stuck in the corner.
        let art = Randomart::new(&[0; 8]);
        assert_eq!(art.visits(1, 1), 1);
        assert_eq!(art.visits(0, 0), 28);
        assert_eq!(art.end(), (0, 0));
    }

    #[test]
    fn every_step_lands_somewhere() {
        let art = Randomart::new(b"0123456789abcdef0123456789abcdef");
        let total: usize = (0..SIZE)
            .flat_map(|y| (0..SIZE).map(move |x| (x, y)))
            .map(|(x, y)| art.visits(x, y) as usize)
            .sum();
        assert_eq!(total, 32 * 4);
    }
}
