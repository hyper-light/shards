//! The shards mark (hyperlight-site components/project-mark.tsx): three crystal shards
//! in a 32×32 box, each an outline, a facet at 40% and a glint tick, moving as the shards
//! study moves its fleet (components/studies/shards.tsx): each sways and bobs on its own
//! phase, its colour cycles the study's palette, and a short glint runs its rim as the
//! study's circuits carry theirs (`stroke-dasharray: 38 362`, pathLength 400).

use crate::canvas::{Canvas, Ink};
use crate::motion;
use crate::tokens::{self, Rgb};

type Pt = (f64, f64);

/// Each shard: its outline, its facet, its tick (the mark's three paths, split by shard).
const SHARDS: [(&[Pt], &[Pt], &[Pt]); 3] = [
    (
        &[(14.0, 6.0), (19.0, 2.0), (21.0, 12.0), (16.0, 19.0), (12.0, 14.0)],
        &[(19.0, 2.0), (16.0, 12.0), (16.0, 19.0)],
        &[(15.0, 10.0), (17.0, 13.0)],
    ),
    (
        &[(3.0, 14.0), (9.0, 9.0), (11.0, 18.0), (7.0, 26.0), (4.0, 20.0)],
        &[(9.0, 9.0), (6.0, 18.0), (7.0, 26.0)],
        &[(5.0, 17.0), (7.0, 19.0)],
    ),
    (
        &[
            (23.0, 17.0),
            (29.0, 11.0),
            (27.0, 25.0),
            (22.0, 30.0),
            (21.0, 23.0),
        ],
        &[(29.0, 11.0), (24.0, 23.0), (22.0, 30.0)],
        &[(23.0, 21.0), (25.0, 23.0)],
    ),
];

/// Ranks: a glint over a rim over a tick over a facet.
const FACET: u8 = 1;
const TICK: u8 = 2;
const RIM: u8 = 3;
const GLINT: u8 = 4;

/// Draws the mark at time `t` across the whole of `canvas`, centred.
pub fn draw(canvas: &mut Canvas, t: f64) {
    canvas.clear();
    let side = canvas.width().min(canvas.height());
    let scale = side / 32.0;
    let (ox, oy) = ((canvas.width() - side) / 2.0, (canvas.height() - side) / 2.0);
    for (s, (outline, facet, tick)) in SHARDS.iter().enumerate() {
        let sf = s as f64;
        let n = outline.len() as f64;
        let centre = outline
            .iter()
            .fold((0.0, 0.0), |c, p| (c.0 + p.0 / n, c.1 + p.1 / n));
        // The study's sway and bob, each shard on its own phase.
        let angle = (t * 0.39 + sf * 0.91).sin() * 0.085;
        let bob = (t * 0.53 + sf * 0.82).sin() * 0.9;
        let (sin, cos) = angle.sin_cos();
        let place = |p: &Pt| -> Pt {
            let (dx, dy) = (p.0 - centre.0, p.1 - centre.1);
            let x = centre.0 + dx * cos - dy * sin;
            let y = centre.1 + dx * sin + dy * cos + bob;
            (ox + x * scale, oy + y * scale)
        };
        let color = tokens::cycle(&tokens::SHARDS, sf + t * 0.45);
        let faint = tokens::mix(color, tokens::LINE, 0.6);
        polyline(canvas, facet.iter().map(&place), |_| Ink {
            rank: FACET,
            color: faint,
            alpha: 255,
        });
        polyline(canvas, tick.iter().map(&place), |_| Ink::solid(TICK, color));
        // The rim, closed, its glint where the circuit's dash is now.
        let rim: Vec<Pt> = outline.iter().chain(outline.first()).map(&place).collect();
        let length: f64 = rim
            .iter()
            .zip(rim.iter().skip(1))
            .map(|(a, b)| dist(*a, *b))
            .sum();
        let glint_at = ((t * (81.0 + 13.0 * sf) - sf * 43.0) / 400.0).rem_euclid(1.0);
        let mut walked = 0.0;
        for (&a, &b) in rim.iter().zip(rim.iter().skip(1)) {
            let span = dist(a, b);
            let start = walked;
            canvas.line(a, b, |u| {
                let along = if length > 0.0 {
                    (start + u * span) / length
                } else {
                    0.0
                };
                // Distance round the rim, either way.
                let d = (along - glint_at).abs();
                let d = d.min(1.0 - d);
                let light = motion::glint(d, 0.0, 0.05);
                if light > 0.35 {
                    Ink {
                        rank: GLINT,
                        color: tokens::mix(color, tokens::BRIGHT, light),
                        alpha: 255,
                    }
                } else {
                    Ink::solid(RIM, color)
                }
            });
            walked += span;
        }
    }
}

fn polyline(canvas: &mut Canvas, points: impl Iterator<Item = Pt>, ink: impl Fn(f64) -> Ink + Copy) {
    let mut last: Option<Pt> = None;
    for p in points {
        if let Some(a) = last {
            canvas.line(a, p, ink);
        }
        last = Some(p);
    }
}

fn dist(a: Pt, b: Pt) -> f64 {
    ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt()
}

/// The mark's colour, for text beside it: the first shard's, now.
pub fn hue(t: f64) -> Rgb {
    tokens::cycle(&tokens::SHARDS, t * 0.45)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::tokens::Paint;

    fn rows(c: &Canvas) -> Vec<String> {
        (0..c.rows())
            .map(|r| {
                let mut s = String::new();
                c.row(r, &Paint::new(true), &mut s);
                // What a reader sees: the cells, without their colours.
                let mut seen = String::new();
                let mut esc = false;
                for ch in s.chars() {
                    match (esc, ch) {
                        (false, '\x1b') => esc = true,
                        (true, 'm') => esc = false,
                        (true, _) => {}
                        (false, ch) => seen.push(ch),
                    }
                }
                seen
            })
            .collect()
    }

    #[test]
    fn the_mark_draws_three_shards_at_any_size() {
        for (cols, rows_) in [(8, 4), (16, 8)] {
            let mut c = Canvas::new(cols, rows_);
            draw(&mut c, 0.0);
            let drawn = rows(&c);
            assert_eq!(drawn.len(), rows_);
            let inked: usize = drawn
                .iter()
                .map(|r| r.chars().filter(|&ch| ch != ' ').count())
                .sum();
            assert!(inked > cols * rows_ / 3, "{cols}x{rows_}: {drawn:#?}");
            for r in &drawn {
                assert_eq!(r.chars().count(), cols);
            }
        }
    }

    #[test]
    fn the_mark_moves() {
        let mut a = Canvas::new(16, 8);
        let mut b = Canvas::new(16, 8);
        draw(&mut a, 0.0);
        draw(&mut b, 1.3);
        let (ra, rb) = (rows(&a), rows(&b));
        assert_ne!(ra, rb, "the shards sway and bob");
        // And it holds still at one time.
        let mut c = Canvas::new(16, 8);
        draw(&mut c, 1.3);
        assert_eq!(rows(&c), rb);
    }
}
