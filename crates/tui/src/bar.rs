//! Hairline progress bars: a heavy rule (`━`) over a hairline track (`─`), the site's
//! 1–2px prism lines made of text. The fill's colour is the prism as `prism-current`
//! moves it; the cell the fill is part way into is lit by how far into it it is, so the
//! bar glides where whole cells would jump; while work goes on a glint travels the fill.
//! A finished bar holds still: ambient motion stays subordinate to what matters.

use crate::motion;
use crate::tokens::{self, Paint};

/// How a bar stands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fill {
    /// So much of it, in [0, 1], still going.
    Going(f64),
    /// All of it, done.
    Done,
    /// Under way, how much unknown: a glint runs the track.
    Unknown,
    /// Not begun.
    Waiting,
}

/// Draws a bar `width` cells wide into `out` at time `t`; `phase` sets bars of one
/// display apart, so their glints do not march together.
pub fn draw(out: &mut String, paint: &Paint, width: usize, fill: Fill, t: f64, phase: f64) {
    let heavy = '━';
    let light = '─';
    let w = width as f64;
    match fill {
        Fill::Waiting => {
            paint.fg(out, tokens::LINE);
            out.extend(std::iter::repeat_n(light, width));
        }
        Fill::Unknown => {
            let at = motion::travel(t, 0.35, 0.6, phase);
            for x in 0..width {
                let u = (x as f64 + 0.5) / w;
                let k = motion::glint(u, at, 0.07);
                paint.fg(out, tokens::mix(tokens::LINE, tokens::prism(u), k));
                out.push(if k > 0.3 { heavy } else { light });
            }
        }
        Fill::Done | Fill::Going(_) => {
            let (frac, going) = match fill {
                Fill::Going(f) => (if f.is_nan() { 0.0 } else { f.clamp(0.0, 1.0) }, true),
                _ => (1.0, false),
            };
            let filled = frac * w;
            let glint_at = motion::travel(t, 0.45, 0.4, phase);
            for x in 0..width {
                let xf = x as f64;
                let u = (xf + 0.5) / w;
                // A finished bar holds its prism still; a going one lets it drift.
                let base = if going {
                    tokens::prism_current(u, t, 4.8)
                } else {
                    tokens::mix(tokens::prism(u * 0.6 + 0.2), tokens::BODY, 0.25)
                };
                if xf + 1.0 <= filled {
                    let along = if filled > 0.0 { (xf + 0.5) / filled } else { 0.0 };
                    let lit = if going {
                        motion::glint(along, glint_at, 0.09) * 0.7
                    } else {
                        0.0
                    };
                    paint.fg(out, tokens::mix(base, tokens::BRIGHT, lit));
                    out.push(heavy);
                } else if xf < filled {
                    // Part way into this cell: lit by how far.
                    paint.fg(out, tokens::mix(tokens::LINE, base, filled - xf));
                    out.push(heavy);
                } else {
                    paint.fg(out, tokens::LINE);
                    out.push(light);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(s: &str) -> String {
        let mut out = String::new();
        let mut esc = false;
        for ch in s.chars() {
            match (esc, ch) {
                (false, '\x1b') => esc = true,
                (true, 'm') => esc = false,
                (true, _) => {}
                (false, ch) => out.push(ch),
            }
        }
        out
    }

    #[test]
    fn a_bar_is_as_wide_as_it_is_asked_and_fills_as_it_goes() {
        let paint = Paint { truecolor: true };
        for width in [1usize, 7, 32] {
            for fill in [
                Fill::Waiting,
                Fill::Unknown,
                Fill::Done,
                Fill::Going(0.0),
                Fill::Going(0.5),
                Fill::Going(f64::NAN),
            ] {
                let mut s = String::new();
                draw(&mut s, &paint, width, fill, 1.0, 0.0);
                assert_eq!(seen(&s).chars().count(), width, "{fill:?}");
            }
        }
        let mut s = String::new();
        draw(&mut s, &paint, 10, Fill::Going(0.45), 0.0, 0.0);
        assert_eq!(seen(&s), "━━━━━─────");
        let mut s = String::new();
        draw(&mut s, &paint, 10, Fill::Done, 0.0, 0.0);
        assert_eq!(seen(&s), "━━━━━━━━━━");
    }

    #[test]
    fn a_finished_bar_holds_still() {
        let paint = Paint { truecolor: true };
        let (mut a, mut b) = (String::new(), String::new());
        draw(&mut a, &paint, 20, Fill::Done, 0.0, 0.0);
        draw(&mut b, &paint, 20, Fill::Done, 3.7, 0.0);
        assert_eq!(a, b);
        let (mut c, mut d) = (String::new(), String::new());
        draw(&mut c, &paint, 20, Fill::Going(0.6), 0.0, 0.0);
        draw(&mut d, &paint, 20, Fill::Going(0.6), 0.9, 0.0);
        assert_ne!(c, d, "a going bar moves");
    }
}
