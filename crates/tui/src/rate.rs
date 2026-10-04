//! How fast bytes arrive: smoothed, so the rate and the time left read steadily, and
//! kept a while, as a sparkline of Braille columns.

use crate::canvas::{Canvas, Ink};
use crate::tokens::{self, Paint};

/// Samples the sparkline keeps: two a cell.
const KEPT: usize = 48;
/// A sample every quarter second.
const EVERY: f64 = 0.25;
/// The smoothing's time constant, seconds: a burst moves the rate, a stall pulls it down
/// within a few seconds.
const SMOOTH: f64 = 2.0;

#[derive(Debug, Clone)]
pub struct Rate {
    rate: f64,
    last: Option<(f64, u64)>,
    samples: Vec<f64>,
    next_sample: f64,
}

impl Default for Rate {
    fn default() -> Rate {
        Rate::new()
    }
}

impl Rate {
    pub fn new() -> Rate {
        Rate {
            rate: 0.0,
            last: None,
            samples: Vec::with_capacity(KEPT),
            next_sample: 0.0,
        }
    }

    /// `total` bytes have arrived by time `t`.
    pub fn at(&mut self, t: f64, total: u64) {
        if let Some((t0, b0)) = self.last {
            let dt = t - t0;
            if dt > 0.0 {
                let now = total.saturating_sub(b0) as f64 / dt;
                let k = 1.0 - (-dt / SMOOTH).exp();
                self.rate += (now - self.rate) * k;
            }
        }
        self.last = Some((t, total));
        while t >= self.next_sample {
            if self.samples.len() == KEPT {
                self.samples.remove(0);
            }
            self.samples.push(self.rate);
            self.next_sample += EVERY;
        }
    }

    /// Bytes a second, smoothed.
    pub fn bytes_per_second(&self) -> f64 {
        self.rate
    }

    /// Seconds until `remaining` bytes arrive at this rate, if it is moving.
    pub fn eta(&self, remaining: u64) -> Option<f64> {
        (self.rate > 1.0).then(|| remaining as f64 / self.rate)
    }

    /// The recent rates as a sparkline `cols` cells wide, its newest at the right,
    /// coloured along the prism by height.
    pub fn sparkline(&self, out: &mut String, paint: &Paint, cols: usize) {
        let mut canvas = Canvas::new(cols, 1);
        let shown = self.samples.iter().rev().take(cols * 2).collect::<Vec<_>>();
        let top = shown.iter().fold(0.0f64, |m, v| m.max(**v)).max(1.0);
        for (i, v) in shown.iter().enumerate() {
            let x = (cols * 2) as f64 - 1.0 - i as f64;
            let level = ((**v / top) * 4.0).round().clamp(0.0, 4.0) as usize;
            for y in 0..level {
                canvas.dot(
                    x,
                    3.0 - y as f64,
                    Ink {
                        rank: 1,
                        color: tokens::prism(**v / top),
                    },
                );
            }
        }
        canvas.row(0, paint, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_steady_rate_is_found_and_a_stall_pulls_it_down() {
        let mut r = Rate::new();
        for i in 0..=40 {
            r.at(f64::from(i) * 0.25, u64::try_from(i).unwrap_or(0) * 250_000);
        }
        let steady = r.bytes_per_second();
        assert!((steady - 1_000_000.0).abs() < 50_000.0, "{steady}");
        assert!(r.eta(2_000_000).is_some_and(|s| (s - 2.0).abs() < 0.2));
        for i in 41..=60 {
            r.at(f64::from(i) * 0.25, 40 * 250_000);
        }
        assert!(r.bytes_per_second() < steady / 5.0);
    }

    #[test]
    fn the_sparkline_is_as_wide_as_asked() {
        let mut r = Rate::new();
        for i in 0..20 {
            r.at(f64::from(i) * 0.25, u64::try_from(i * i).unwrap_or(0) * 1000);
        }
        let mut s = String::new();
        r.sparkline(&mut s, &Paint { truecolor: true }, 6);
        let cells = s
            .chars()
            .filter(|c| *c == ' ' || ('\u{2800}'..='\u{28ff}').contains(c))
            .count();
        assert_eq!(cells, 6, "{s:?}");
    }
}
