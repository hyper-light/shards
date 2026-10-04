//! Braille cells as a canvas: each cell a 2×4 grid of dots (U+2800–U+28FF), so a
//! terminal draws lines at four times its rows' resolution. Terminal cells are about
//! twice as tall as wide, so the dots come out square.

use crate::tokens::{Paint, Rgb};

/// A dot's ink: how much it matters (the highest wins a cell's one colour) and its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ink {
    pub rank: u8,
    pub color: Rgb,
}

#[derive(Debug, Clone)]
pub struct Canvas {
    cols: usize,
    rows: usize,
    dots: Vec<Option<Ink>>,
}

/// The bit of a Braille cell's dot at column `x` (0–1) and row `y` (0–3).
fn bit(x: usize, y: usize) -> u32 {
    match (x, y) {
        (0, 0) => 0x01,
        (0, 1) => 0x02,
        (0, 2) => 0x04,
        (1, 0) => 0x08,
        (1, 1) => 0x10,
        (1, 2) => 0x20,
        (0, 3) => 0x40,
        (1, 3) => 0x80,
        _ => 0,
    }
}

impl Canvas {
    /// A canvas of `cols` × `rows` cells: twice as many dots across, four times down.
    pub fn new(cols: usize, rows: usize) -> Canvas {
        Canvas {
            cols,
            rows,
            dots: vec![None; cols * 2 * rows * 4],
        }
    }

    pub fn width(&self) -> f64 {
        (self.cols * 2) as f64
    }

    pub fn height(&self) -> f64 {
        (self.rows * 4) as f64
    }

    pub fn clear(&mut self) {
        self.dots.fill(None);
    }

    /// Inks the dot at (`x`, `y`), in dots, unless an ink that matters more is there.
    pub fn dot(&mut self, x: f64, y: f64, ink: Ink) {
        if !(x >= 0.0 && y >= 0.0) {
            return;
        }
        let (x, y) = (x.round() as usize, y.round() as usize);
        if x >= self.cols * 2 || y >= self.rows * 4 {
            return;
        }
        if let Some(slot) = self.dots.get_mut(y * self.cols * 2 + x)
            && slot.is_none_or(|held| held.rank <= ink.rank)
        {
            *slot = Some(ink);
        }
    }

    /// A line from `a` to `b`, in dots, its ink chosen along it: `ink(u)` for `u` in
    /// [0, 1] from `a` to `b`.
    pub fn line(&mut self, a: (f64, f64), b: (f64, f64), ink: impl Fn(f64) -> Ink) {
        let steps = ((b.0 - a.0).abs().max((b.1 - a.1).abs()) * 2.0).ceil().max(1.0) as usize;
        for i in 0..=steps {
            let u = i as f64 / steps as f64;
            self.dot(a.0 + (b.0 - a.0) * u, a.1 + (b.1 - a.1) * u, ink(u));
        }
    }

    /// Row `row` of cells, each its Braille character in the colour of its weightiest
    /// ink; empty cells blank.
    pub fn row(&self, row: usize, paint: &Paint, out: &mut String) {
        let width = self.cols * 2;
        let mut last: Option<Rgb> = None;
        for col in 0..self.cols {
            let mut bits = 0u32;
            let mut best: Option<Ink> = None;
            for y in 0..4 {
                for x in 0..2 {
                    let at = (row * 4 + y) * width + col * 2 + x;
                    if let Some(Some(ink)) = self.dots.get(at) {
                        bits |= bit(x, y);
                        if best.is_none_or(|b| b.rank < ink.rank) {
                            best = Some(*ink);
                        }
                    }
                }
            }
            match best {
                Some(ink) if bits != 0 => {
                    if last != Some(ink.color) {
                        paint.fg(out, ink.color);
                        last = Some(ink.color);
                    }
                    out.push(char::from_u32(0x2800 + bits).unwrap_or(' '));
                }
                _ => out.push(' '),
            }
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const INK: Ink = Ink {
        rank: 1,
        color: (255, 255, 255),
    };

    fn text(c: &Canvas, row: usize) -> String {
        let mut s = String::new();
        c.row(row, &Paint { truecolor: true }, &mut s);
        s.split('m').next_back().unwrap_or_default().to_string()
    }

    #[test]
    fn dots_land_in_their_cells() {
        let mut c = Canvas::new(2, 1);
        c.dot(0.0, 0.0, INK);
        c.dot(3.0, 3.0, INK);
        assert_eq!(text(&c, 0), "⠁⢀");
        // Off the canvas, or not a number: nothing.
        c.dot(-1.0, 0.0, INK);
        c.dot(f64::NAN, 0.0, INK);
        c.dot(9.0, 0.0, INK);
        assert_eq!(text(&c, 0), "⠁⢀");
    }

    #[test]
    fn a_line_inks_every_dot_on_its_way() {
        let mut c = Canvas::new(2, 1);
        c.line((0.0, 1.0), (3.0, 1.0), |_| INK);
        assert_eq!(text(&c, 0), "⠒⠒");
    }

    #[test]
    fn the_weightier_ink_colours_the_cell() {
        let mut c = Canvas::new(1, 1);
        c.dot(
            0.0,
            0.0,
            Ink {
                rank: 2,
                color: (1, 2, 3),
            },
        );
        c.dot(
            1.0,
            0.0,
            Ink {
                rank: 1,
                color: (9, 9, 9),
            },
        );
        let mut s = String::new();
        c.row(0, &Paint { truecolor: true }, &mut s);
        assert!(s.contains("38;2;1;2;3"), "{s:?}");
    }
}
