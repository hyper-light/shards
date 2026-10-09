//! Instants as Go's time.Time holds them for verification: seconds and nanoseconds since
//! 1970, compared as instants, and the offset of the zone they print in (UTC for times
//! read from certificates and timestamps, the local zone for those time.Unix makes).

/// A zone's offset east of UTC, in seconds, at an instant (localtime(3)'s `tm_gmtoff`).
pub type Zone = fn(i64) -> i32;

/// UTC.
pub fn utc(_: i64) -> i32 {
    0
}

#[derive(Debug, Clone, Copy)]
pub struct Time {
    pub secs: i64,
    pub nanos: u32,
    /// The offset it prints with, east of UTC.
    pub offset: i32,
}

impl PartialEq for Time {
    fn eq(&self, other: &Self) -> bool {
        (self.secs, self.nanos) == (other.secs, other.nanos)
    }
}

impl Eq for Time {}

impl PartialOrd for Time {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Time {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.secs, self.nanos).cmp(&(other.secs, other.nanos))
    }
}

impl Time {
    /// A time in UTC.
    pub fn utc(secs: i64, nanos: u32) -> Time {
        Time {
            secs,
            nanos,
            offset: 0,
        }
    }

    /// time.Unix(secs, 0): in the local zone.
    pub fn unix(secs: i64, zone: Zone) -> Time {
        Time {
            secs,
            nanos: 0,
            offset: zone(secs),
        }
    }

    /// The zero Time's IsZero: January 1, year 1, UTC.
    pub fn is_zero(&self) -> bool {
        self.secs == ZERO_SECS && self.nanos == 0
    }

    /// Format(time.RFC3339): seconds, in its zone, `Z` where that is UTC.
    pub fn rfc3339(&self) -> String {
        let (date, clock) = civil(self.secs + i64::from(self.offset));
        let zone = if self.offset == 0 {
            "Z".to_string()
        } else {
            let o = self.offset.unsigned_abs();
            format!(
                "{}{:02}:{:02}",
                if self.offset < 0 { '-' } else { '+' },
                o / 3600,
                o / 60 % 60
            )
        };
        format!("{date}T{clock}{zone}")
    }
}

/// Seconds from 1970 to January 1, year 1, the zero Time.
pub const ZERO_SECS: i64 = -62_135_596_800;

/// The days from 1970 to a civil date (Hinnant's days_from_civil).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The days of month `m` of year `y`.
pub fn days_in(m: u32, y: i64) -> u32 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// A second's date and clock, `YYYY-MM-DD` and `hh:mm:ss`.
fn civil(t: i64) -> (String, String) {
    let days = t.div_euclid(86_400);
    let rem = t.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        format!("{y:04}-{m:02}-{d:02}"),
        format!("{:02}:{:02}:{:02}", rem / 3600, rem / 60 % 60, rem % 60),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_print_as_go_prints_them() {
        assert_eq!(Time::utc(1_774_446_092, 5).rfc3339(), "2026-03-25T13:41:32Z");
        let local = Time {
            secs: 1_774_446_092,
            nanos: 0,
            offset: -(7 * 3600 + 30 * 60),
        };
        assert_eq!(local.rfc3339(), "2026-03-25T06:11:32-07:30");
        assert_eq!(Time::utc(ZERO_SECS, 0).rfc3339(), "0001-01-01T00:00:00Z");
        assert!(Time::utc(ZERO_SECS, 0).is_zero());
        assert_eq!(days_from_civil(1, 1, 1) * 86_400, ZERO_SECS);
    }
}
