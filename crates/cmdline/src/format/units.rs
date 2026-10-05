//! go-units v0.5.0 (docker/go-units, which docker/cli v29.8.1 vendors): sizes and
//! durations in words, as the formatter's contexts print them; and Go's `%g`, which they
//! print sizes with.

/// go-units' HumanDuration (duration.go), of `d` nanoseconds: Go's float division and
/// truncation included.
pub(super) fn human_duration(d: i64) -> String {
    const SECOND: i64 = 1_000_000_000;
    const MINUTE: i64 = 60 * SECOND;
    const HOUR: i64 = 60 * MINUTE;
    // time.Duration's Seconds, Minutes and Hours: whole units, then the rest as a fraction.
    #[allow(clippy::cast_precision_loss)]
    let unit = |u: i64| (d / u) as f64 + (d % u) as f64 / u as f64;
    #[allow(clippy::cast_possible_truncation)]
    let int = |f: f64| f as i64;
    let seconds = int(unit(SECOND));
    if seconds < 1 {
        return "Less than a second".into();
    }
    if seconds == 1 {
        return "1 second".into();
    }
    if seconds < 60 {
        return format!("{seconds} seconds");
    }
    let minutes = int(unit(MINUTE));
    if minutes == 1 {
        return "About a minute".into();
    }
    if minutes < 60 {
        return format!("{minutes} minutes");
    }
    let hours = int(unit(HOUR) + 0.5);
    if hours == 1 {
        "About an hour".into()
    } else if hours < 48 {
        format!("{hours} hours")
    } else if hours < 24 * 7 * 2 {
        format!("{} days", hours / 24)
    } else if hours < 24 * 30 * 2 {
        format!("{} weeks", hours / 24 / 7)
    } else if hours < 24 * 365 * 2 {
        format!("{} months", hours / 24 / 30)
    } else {
        format!("{} years", int(unit(HOUR)) / 24 / 365)
    }
}

const DECIMAL: [&str; 9] = ["B", "kB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];
const BINARY: [&str; 9] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB", "ZiB", "YiB"];

/// size.go's getSizeAndUnit and CustomSize with `%.{precision}g%s`.
fn custom_size(mut size: f64, precision: usize, base: f64, units: &[&str; 9]) -> String {
    let mut i = 0;
    while size >= base && i < units.len() - 1 {
        size /= base;
        i += 1;
    }
    format!("{}{}", go_g(size, precision), units.get(i).unwrap_or(&""))
}

/// go-units' HumanSizeWithPrecision: decimal units.
pub(super) fn human_size_precision(size: f64, precision: usize) -> String {
    custom_size(size, precision, 1000.0, &DECIMAL)
}

/// go-units' HumanSize: decimal units, four significant digits.
pub(super) fn human_size(size: f64) -> String {
    human_size_precision(size, 4)
}

/// go-units' BytesSize: binary units, four significant digits.
pub(super) fn bytes_size(size: f64) -> String {
    custom_size(size, 4, 1024.0, &BINARY)
}

/// `v` as Go's fmt prints it with `%.{precision}g` (strconv's %g, ftoa.go): rounded to
/// that many significant digits, trailing zeros dropped, in %e form when the exponent is
/// below -4 or at least the precision.
pub(super) fn go_g(v: f64, precision: usize) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "+Inf" } else { "-Inf" }.into();
    }
    let precision = precision.max(1);
    let sign = if v.is_sign_negative() { "-" } else { "" };
    if v == 0.0 {
        return format!("{sign}0");
    }
    // Rust rounds the exact value half to even at a digit, as strconv does.
    let e = format!("{:.*e}", precision - 1, v.abs());
    let (mantissa, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i64 = exp.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let nd = i64::try_from(digits.len()).unwrap_or(i64::MAX);
    if exp < -4 || exp >= i64::try_from(precision).unwrap_or(i64::MAX) {
        let (first, rest) = digits.split_at(1.min(digits.len()));
        let point = if rest.is_empty() { "" } else { "." };
        let esign = if exp < 0 { '-' } else { '+' };
        return format!("{sign}{first}{point}{rest}e{esign}{:02}", exp.unsigned_abs());
    }
    let dp = exp + 1;
    let s = if dp <= 0 {
        format!("0.{}{digits}", "0".repeat(usize::try_from(-dp).unwrap_or(0)))
    } else if dp >= nd {
        format!("{digits}{}", "0".repeat(usize::try_from(dp - nd).unwrap_or(0)))
    } else {
        let (int, frac) = digits.split_at(usize::try_from(dp).unwrap_or(0).min(digits.len()));
        format!("{int}.{frac}")
    };
    format!("{sign}{s}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g_prints_as_go_prints() {
        assert_eq!(go_g(999.999, 3), "1e+03");
        assert_eq!(go_g(1.125, 3), "1.12");
        assert_eq!(go_g(100.0, 4), "100");
        assert_eq!(go_g(0.000_012_3, 3), "1.23e-05");
        assert_eq!(go_g(-1.0, 3), "-1");
        assert_eq!(go_g(0.5, 4), "0.5");
        assert_eq!(go_g(13.4, 3), "13.4");
        assert_eq!(human_size(1_290_000_000.0), "1.29GB");
        assert_eq!(bytes_size(1536.0), "1.5KiB");
    }

    #[test]
    fn durations_are_worded_as_go_units_words_them() {
        let s = 1_000_000_000;
        assert_eq!(human_duration(-5), "Less than a second");
        assert_eq!(human_duration(59 * s), "59 seconds");
        assert_eq!(human_duration(5399 * s), "About an hour");
        assert_eq!(human_duration(5400 * s), "2 hours");
        assert_eq!(human_duration(i64::MAX), "292 years");
    }
}
