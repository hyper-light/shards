//! hyperlight's colours (hyperlight-site app/globals.css), and painting them.

use std::fmt::Write as _;

pub type Rgb = (u8, u8, u8);

pub const FOREGROUND: Rgb = (0xed, 0xed, 0xee);
/// The headline's highlight.
pub const BRIGHT: Rgb = (0xf0, 0xf0, 0xef);
pub const BODY: Rgb = (0xa3, 0xa4, 0xab);
pub const MUTED: Rgb = (0x97, 0x99, 0x9f);
/// Eyebrows.
pub const EYEBROW: Rgb = (0xa1, 0xa3, 0xab);
pub const SUBTLE: Rgb = (0x79, 0x7c, 0x84);
/// The tiny cross, and footnotes.
pub const FAINT: Rgb = (0x6d, 0x70, 0x78);
/// Hairlines.
pub const LINE: Rgb = (0x24, 0x26, 0x2a);
/// A card's border on hover: a hairline that is there.
pub const EDGE: Rgb = (0x47, 0x46, 0x4f);
/// `.status-available`: done, and well.
pub const SAGE: Rgb = (0xa5, 0xc8, 0xb3);
/// `.status-development`: under way.
pub const AMBER: Rgb = (0xc6, 0xb0, 0x8e);
/// `.status-design`'s ring.
pub const DESIGN: Rgb = (0xa8, 0xa1, 0xb8);
/// The mark's refraction stroke, and the focus ring.
pub const LAVENDER: Rgb = (0xbc, 0xb1, 0xd8);
/// The mark's second refraction stroke.
pub const TEAL: Rgb = (0xaa, 0xcb, 0xd0);
/// The prism's rose: the site has no red, and an error is the one place for this.
pub const ROSE: Rgb = (0xd6, 0xa2, 0xaa);

/// `--prism`: mint, sky, lavender, rose, sand, at 0, 28, 53, 75 and 100%.
pub const PRISM: [(f64, Rgb); 5] = [
    (0.0, (0xb8, 0xd9, 0xd2)),
    (0.28, (0xa6, 0xc4, 0xed)),
    (0.53, (0xb8, 0xa6, 0xd5)),
    (0.75, (0xd6, 0xa2, 0xaa)),
    (1.0, (0xd8, 0xcb, 0xb0)),
];

/// The shards study's palette, which its shards cycle through (components/studies/
/// shards.tsx).
pub const SHARDS: [Rgb; 5] = [
    (155, 191, 198),
    (154, 165, 211),
    (190, 155, 189),
    (212, 188, 152),
    (189, 212, 190),
];

pub fn mix(a: Rgb, b: Rgb, k: f64) -> Rgb {
    let k = if k.is_nan() { 0.0 } else { k.clamp(0.0, 1.0) };
    let m = |x: u8, y: u8| (f64::from(x) + (f64::from(y) - f64::from(x)) * k).round() as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// The prism at `x` in [0, 1].
pub fn prism(x: f64) -> Rgb {
    let x = if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) };
    let mut prev = PRISM[0];
    for stop in PRISM {
        if x <= stop.0 {
            let span = stop.0 - prev.0;
            let k = if span > 0.0 { (x - prev.0) / span } else { 0.0 };
            return mix(prev.1, stop.1, k);
        }
        prev = stop;
    }
    prev.1
}

/// The prism as `prism-current` moves it: the gradient drawn 2.5 times its width, its
/// position running there and back over `period` seconds, eased in and out. `x` in
/// [0, 1] across what it colours.
pub fn prism_current(x: f64, t: f64, period: f64) -> Rgb {
    let phase = crate::motion::ping_pong(t / period);
    let eased = crate::motion::smoothstep(phase);
    prism((x + eased * 1.5) / 2.5)
}

/// The study palette at position `p`, wrapping: `p = i + t*k` cycles it as the studies do.
pub fn cycle(palette: &[Rgb], p: f64) -> Rgb {
    let n = palette.len();
    if n == 0 {
        return FOREGROUND;
    }
    let p = p.rem_euclid(n as f64);
    let i = p.floor() as usize;
    let a = palette.get(i % n).copied().unwrap_or(FOREGROUND);
    let b = palette.get((i + 1) % n).copied().unwrap_or(FOREGROUND);
    mix(a, b, p - p.floor())
}

/// How colour is written: 24-bit where the terminal says it can, xterm's 256 otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Paint {
    pub truecolor: bool,
}

impl Paint {
    pub fn fg(&self, out: &mut String, c: Rgb) {
        if self.truecolor {
            let _ = write!(out, "\x1b[38;2;{};{};{}m", c.0, c.1, c.2);
        } else {
            let _ = write!(out, "\x1b[38;5;{}m", xterm256(c));
        }
    }

    pub fn bold(&self, out: &mut String, on: bool) {
        out.push_str(if on { "\x1b[1m" } else { "\x1b[22m" });
    }

    pub fn reset(&self, out: &mut String) {
        out.push_str("\x1b[0m");
    }
}

/// The nearest of xterm's 6×6×6 cube or its 24 greys.
pub fn xterm256(c: Rgb) -> u8 {
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |v: u8| -> (u8, u8) {
        let mut best = (0u8, 0u8);
        let mut err = i32::MAX;
        for (i, s) in STEPS.iter().enumerate() {
            let e = (i32::from(*s) - i32::from(v)).abs();
            if e < err {
                err = e;
                best = (i as u8, *s);
            }
        }
        best
    };
    let ((r, rv), (g, gv), (b, bv)) = (level(c.0), level(c.1), level(c.2));
    let cube = 16 + 36 * r + 6 * g + b;
    let grey = ((u32::from(c.0) + u32::from(c.1) + u32::from(c.2)) / 3).saturating_sub(3) / 10;
    let grey = grey.min(23) as u8;
    let gv_ = 8 + 10 * grey;
    if dist(c, (gv_, gv_, gv_)) < dist(c, (rv, gv, bv)) {
        232 + grey
    } else {
        cube
    }
}

fn dist(a: Rgb, b: Rgb) -> i32 {
    let d = |x: u8, y: u8| (i32::from(x) - i32::from(y)).pow(2);
    d(a.0, b.0) + d(a.1, b.1) + d(a.2, b.2)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn the_prism_runs_through_its_stops() {
        for (at, c) in PRISM {
            assert_eq!(prism(at), c);
        }
        assert_eq!(prism(-1.0), PRISM[0].1);
        assert_eq!(prism(2.0), PRISM[4].1);
        assert_eq!(prism(f64::NAN), PRISM[0].1);
    }

    #[test]
    fn the_palette_cycles_and_wraps() {
        assert_eq!(cycle(&SHARDS, 0.0), SHARDS[0]);
        assert_eq!(cycle(&SHARDS, 5.0), SHARDS[0]);
        assert_eq!(cycle(&SHARDS, -1.0), SHARDS[4]);
        assert_eq!(cycle(&[], 1.0), FOREGROUND);
    }

    #[test]
    fn colours_fall_back_to_xterms_256() {
        assert_eq!(xterm256((0, 0, 0)), 16);
        assert_eq!(xterm256((255, 255, 255)), 231);
        assert!((232..=255).contains(&xterm256((0x08, 0x09, 0x0a))));
        let mut s = String::new();
        Paint { truecolor: false }.fg(&mut s, SAGE);
        assert!(s.starts_with("\x1b[38;5;"));
    }
}
