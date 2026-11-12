//! What a measurement keeps: every sample as a JSON line, and their distributions.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;

/// One JSON object, its fields in the order given.
#[derive(Debug, Default)]
pub struct Rec(String);

impl Rec {
    pub fn new(kind: &str) -> Rec {
        Rec(String::new()).s("kind", kind)
    }

    fn key(mut self, key: &str) -> Rec {
        self.0.push(if self.0.is_empty() { '{' } else { ',' });
        self.0.push_str(&quote(key));
        self.0.push(':');
        self
    }

    pub fn s(self, key: &str, value: &str) -> Rec {
        let mut r = self.key(key);
        r.0.push_str(&quote(value));
        r
    }

    pub fn u(self, key: &str, value: u64) -> Rec {
        let mut r = self.key(key);
        let _ = write!(r.0, "{value}");
        r
    }

    pub fn f(self, key: &str, value: f64) -> Rec {
        let mut r = self.key(key);
        if value.is_finite() {
            let _ = write!(r.0, "{value}");
        } else {
            r.0.push_str("null");
        }
        r
    }

    pub fn list(self, key: &str, values: &[u64]) -> Rec {
        let mut r = self.key(key);
        r.0.push('[');
        for (i, v) in values.iter().enumerate() {
            if i > 0 {
                r.0.push(',');
            }
            let _ = write!(r.0, "{v}");
        }
        r.0.push(']');
        r
    }

    pub fn line(mut self) -> String {
        self.0.push('}');
        self.0
    }
}

/// `text` as a JSON string (RFC 8259 §7).
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len().saturating_add(2));
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The records file, and the samples the summary is made of.
#[derive(Debug)]
pub struct Records {
    file: std::fs::File,
    /// Samples by metric, then arm.
    samples: BTreeMap<(String, String), Vec<f64>>,
}

impl Records {
    pub fn create(path: &std::path::Path) -> Result<Records, String> {
        let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Records {
            file,
            samples: BTreeMap::new(),
        })
    }

    pub fn write(&mut self, rec: Rec) {
        let mut line = rec.line();
        line.push('\n');
        let _ = self.file.write_all(line.as_bytes());
    }

    pub fn sample(&mut self, metric: &str, arm: &str, value: f64) {
        self.samples
            .entry((metric.to_string(), arm.to_string()))
            .or_default()
            .push(value);
    }

    pub fn samples(&self, metric: &str, arm: &str) -> &[f64] {
        self.samples
            .get(&(metric.to_string(), arm.to_string()))
            .map_or(&[], Vec::as_slice)
    }

    pub fn metrics(&self) -> Vec<(String, String)> {
        self.samples.keys().cloned().collect()
    }
}

/// n, p50, p90, p99 and max of `samples`, each at its nearest rank ((n - 1) * q, rounded), as
/// the fleet measurement takes them.
#[derive(Debug, Clone, Copy)]
pub struct Dist {
    pub n: usize,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub max: f64,
}

pub fn dist(samples: &[f64]) -> Option<Dist> {
    let mut sorted: Vec<f64> = samples.iter().copied().filter(|v| v.is_finite()).collect();
    sorted.sort_by(f64::total_cmp);
    let last = sorted.len().checked_sub(1)?;
    let at = |q: f64| {
        let i = (last as f64 * q).round() as usize;
        sorted.get(i.min(last)).copied().unwrap_or(f64::NAN)
    };
    Some(Dist {
        n: sorted.len(),
        p50: at(0.5),
        p90: at(0.9),
        p99: at(0.99),
        max: sorted.get(last).copied().unwrap_or(f64::NAN),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_is_one_json_object() {
        let r = Rec::new("x").s("a", "q\"\n").u("b", 7).f("c", f64::NAN).list("d", &[1, 2]);
        assert_eq!(r.line(), r#"{"kind":"x","a":"q\"\n","b":7,"c":null,"d":[1,2]}"#);
    }

    #[test]
    fn percentiles_are_nearest_ranks() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        let d = dist(&v).unwrap();
        assert_eq!((d.n, d.p50, d.p90, d.p99, d.max), (100, 51.0, 90.0, 99.0, 100.0));
        assert!(dist(&[]).is_none());
    }
}
