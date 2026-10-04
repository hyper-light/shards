//! Words and numbers as shards shows them.

/// An eyebrow: the site's small mono capitals, tracked out, here a space between letters.
pub fn eyebrow(word: &str) -> String {
    let mut out = String::with_capacity(word.len() * 2);
    for (i, c) in word.chars().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.extend(c.to_uppercase());
    }
    out
}

/// Bytes in decimal units, to three figures: `48.1 MB`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 999.5 && unit + 1 < UNITS.len() {
        v /= 1000.0;
        unit += 1;
    }
    let name = UNITS.get(unit).copied().unwrap_or("B");
    if unit == 0 {
        format!("{n} {name}")
    } else if v < 9.995 {
        format!("{v:.2} {name}")
    } else if v < 99.95 {
        format!("{v:.1} {name}")
    } else {
        format!("{v:.0} {name}")
    }
}

/// Bytes, as briefly as they can be said: `48M`.
pub fn bytes_short(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "k", "M", "G", "T", "P"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 999.5 && unit + 1 < UNITS.len() {
        v /= 1000.0;
        unit += 1;
    }
    let name = UNITS.get(unit).copied().unwrap_or("B");
    if v < 9.95 && unit > 0 {
        format!("{v:.1}{name}")
    } else {
        format!("{v:.0}{name}")
    }
}

/// A rate: `48.1 MB/s`.
pub fn rate(bytes_per_second: f64) -> String {
    format!("{}/s", bytes(bytes_per_second.max(0.0) as u64))
}

/// A span of time: `850 ms`, `4.2 s`, `1 m 05 s`, `2 h 03 m`.
pub fn duration(seconds: f64) -> String {
    let s = seconds.max(0.0);
    if s < 1.0 {
        format!("{} ms", (s * 1000.0).round())
    } else if s < 10.0 {
        format!("{s:.1} s")
    } else if s < 60.0 {
        format!("{} s", s.round())
    } else if s < 3600.0 {
        let s = s.round() as u64;
        format!("{} m {:02} s", s / 60, s % 60)
    } else {
        let m = (s / 60.0).round() as u64;
        format!("{} h {:02} m", m / 60, m % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_read_well() {
        assert_eq!(eyebrow("pull"), "P U L L");
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1_000), "1.00 kB");
        assert_eq!(bytes(48_123_456), "48.1 MB");
        assert_eq!(bytes(653_200_000), "653 MB");
        assert_eq!(bytes(29_500_000_000), "29.5 GB");
        assert_eq!(bytes_short(48_123_456), "48M");
        assert_eq!(bytes_short(1_400_000), "1.4M");
        assert_eq!(rate(2_500_000.0), "2.50 MB/s");
        assert_eq!(duration(0.85), "850 ms");
        assert_eq!(duration(4.21), "4.2 s");
        assert_eq!(duration(65.0), "1 m 05 s");
        assert_eq!(duration(7380.0), "2 h 03 m");
    }
}
