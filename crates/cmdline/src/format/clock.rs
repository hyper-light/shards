//! The time the formatter prints times relative to, and the zone it prints them in: what
//! docker/cli takes from `time.Now()` and `time.Local` (Go 1.26.1 src/time/format.go and
//! time.go).

use std::fmt;

use super::units::human_duration;

/// A zone's rule at some moment: its offset east of UTC in seconds, and its abbreviation
/// (`UTC`, `CET`; empty for a zone with none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    pub offset: i64,
    pub name: String,
}

/// Now, and the local zone, as Go's `time.Now()` and `time.Local` give them.
pub struct Clock<'a> {
    /// Nanoseconds since the Unix epoch.
    pub now: i128,
    /// The local zone at a moment, in seconds since the Unix epoch.
    pub zone: &'a dyn Fn(i64) -> Zone,
}

impl fmt::Debug for Clock<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Clock")
            .field("now", &self.now)
            .finish_non_exhaustive()
    }
}

/// UTC, the zone of a host with no local zone.
pub fn utc(_: i64) -> Zone {
    Zone {
        offset: 0,
        name: "UTC".into(),
    }
}

const NANOS: i128 = 1_000_000_000;

impl Clock<'_> {
    /// `units.HumanDuration(time.Now().UTC().Sub(t)) + " ago"`, for `t` in nanoseconds
    /// since the epoch. Sub saturates at the longest durations Go has.
    pub(super) fn ago(&self, t: i128) -> String {
        let d = self.now.saturating_sub(t);
        let d = i64::try_from(d).unwrap_or(if d < 0 { i64::MIN } else { i64::MAX });
        format!("{} ago", human_duration(d))
    }

    /// `time.Unix(sec, nsec).String()`: `2006-01-02 15:04:05.999999999 -0700 MST` in the
    /// local zone.
    pub(super) fn string(&self, t: i128) -> String {
        let (sec, nsec) = split(t);
        let zone = (self.zone)(sec);
        let mut out = date_time(sec, zone.offset, ' ');
        if nsec > 0 {
            let frac = format!("{nsec:09}");
            out.push('.');
            out.push_str(frac.trim_end_matches('0'));
        }
        out.push(' ');
        out.push_str(&numeric_zone(zone.offset, false));
        out.push(' ');
        if zone.name.is_empty() {
            out.push_str(&numeric_zone(zone.offset, false));
        } else {
            out.push_str(&zone.name);
        }
        out
    }

    /// `time.Unix(sec, 0).Format(time.RFC3339)` in the local zone.
    pub(super) fn rfc3339(&self, sec: i64) -> String {
        let zone = (self.zone)(sec);
        let mut out = date_time(sec, zone.offset, 'T');
        if zone.offset == 0 {
            out.push('Z');
        } else {
            out.push_str(&numeric_zone(zone.offset, true));
        }
        out
    }
}

/// Seconds since the epoch and the nanoseconds into that second.
fn split(t: i128) -> (i64, i128) {
    let sec = t.div_euclid(NANOS);
    let sec = i64::try_from(sec).unwrap_or(if sec < 0 { i64::MIN } else { i64::MAX });
    (sec, t.rem_euclid(NANOS))
}

/// `2006-01-02 15:04:05`, with `sep` between the date and the time.
fn date_time(sec: i64, offset: i64, sep: char) -> String {
    let local = i128::from(sec) + i128::from(offset);
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    format!(
        "{}-{m:02}-{d:02}{sep}{:02}:{:02}:{:02}",
        year(y),
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// format.go's appendInt(b, year, 4): a sign, then at least four digits.
fn year(y: i128) -> String {
    if y < 0 {
        format!("-{:04}", y.unsigned_abs())
    } else {
        format!("{y:04}")
    }
}

/// `-0700`, or `-07:00` with `colon`, as format.go writes a zone's offset: whole minutes,
/// truncated toward zero.
fn numeric_zone(offset: i64, colon: bool) -> String {
    let mut zone = offset / 60;
    let sign = if zone < 0 { '-' } else { '+' };
    zone = zone.abs();
    let colon = if colon { ":" } else { "" };
    format!("{sign}{:02}{colon}{:02}", zone / 60, zone % 60)
}

/// The proleptic Gregorian date `days` after 1970-01-01 (Howard Hinnant's civil_from_days).
fn civil(days: i128) -> (i128, i128, i128) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i128::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_print_as_go_prints_them() {
        let ist = |_: i64| Zone {
            offset: 19_800,
            name: "IST".into(),
        };
        let clock = Clock { now: 0, zone: &ist };
        assert_eq!(clock.string(0), "1970-01-01 05:30:00 +0530 IST");
        assert_eq!(clock.string(1_500_000_000), "1970-01-01 05:30:01.5 +0530 IST");
        assert_eq!(clock.rfc3339(1_700_000_000), "2023-11-15T03:43:20+05:30");
        let clock = Clock { now: 0, zone: &utc };
        assert_eq!(clock.rfc3339(-1), "1969-12-31T23:59:59Z");
        assert_eq!(
            clock.string(-62_135_596_800 * NANOS),
            "0001-01-01 00:00:00 +0000 UTC"
        );
        assert_eq!(clock.ago(-3_600 * NANOS), "About an hour ago");
    }
}
