//! Go 1.26's times, layouts and durations (time/time.go, format.go, format_rfc3339.go), as
//! far as OPA's time builtins use them. Go's `int` arithmetic wraps, and so does this
//! port's wherever an operand's values can make it, so out-of-range dates land where
//! Go's land.

use super::zone::{Location, UTC};

pub const SECONDS_PER_MINUTE: i64 = 60;
pub const SECONDS_PER_HOUR: i64 = 60 * SECONDS_PER_MINUTE;
pub const SECONDS_PER_DAY: i64 = 24 * SECONDS_PER_HOUR;

const ABSOLUTE_YEARS: i64 = 292_277_022_400;
const MARCH_THRU_DECEMBER: i64 = 31 + 30 + 31 + 30 + 31 + 31 + 30 + 31 + 30 + 31;
/// -(absoluteYears*365.2425 + marchThruDecember) * secondsPerDay, exactly.
const ABSOLUTE_TO_INTERNAL: i64 =
    -((ABSOLUTE_YEARS * 365 + ABSOLUTE_YEARS / 400 * 97 + MARCH_THRU_DECEMBER) * SECONDS_PER_DAY);
const INTERNAL_TO_ABSOLUTE: i64 = -ABSOLUTE_TO_INTERNAL;
const UNIX_TO_INTERNAL: i64 = (1969 * 365 + 1969 / 4 - 1969 / 100 + 1969 / 400) * SECONDS_PER_DAY;
const INTERNAL_TO_UNIX: i64 = -UNIX_TO_INTERNAL;
const ABSOLUTE_TO_UNIX: i64 = ABSOLUTE_TO_INTERNAL + INTERNAL_TO_UNIX;
const UNIX_TO_ABSOLUTE: i64 = UNIX_TO_INTERNAL + INTERNAL_TO_ABSOLUTE;

pub const RFC3339: &[u8] = b"2006-01-02T15:04:05Z07:00";
pub const RFC3339_NANO: &[u8] = b"2006-01-02T15:04:05.999999999Z07:00";

const LONG_DAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const SHORT_DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const SHORT_MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const LONG_MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Seconds since the absolute epoch of a Unix second (`absSeconds`).
pub fn abs_seconds(unix: i64) -> u64 {
    unix.wrapping_add(UNIX_TO_ABSOLUTE) as u64
}

/// Days since the absolute epoch (`absDays`).
#[derive(Debug, Clone, Copy)]
pub struct AbsDays(u64);

pub fn abs_days(abs: u64) -> AbsDays {
    AbsDays(abs / SECONDS_PER_DAY as u64)
}

impl AbsDays {
    /// century, year of century, day of the March-based year.
    fn split(self) -> (u64, u64, u32) {
        let d = self.0.wrapping_mul(4).wrapping_add(3);
        let century = d / 146097;
        let cd = (d % 146097) as u32 | 3;
        let prod = 2939745u64 * u64::from(cd);
        let (hi, lo) = ((prod >> 32) as u32, prod as u32);
        (century, u64::from(hi), lo / 2939745 / 4)
    }

    pub fn date(self) -> (i64, i64, i64) {
        let (century, cyear, ayday) = self.split();
        let (amonth, day) = ayday_split(ayday);
        let jan_feb = jan_feb(ayday);
        (year(century, cyear, jan_feb), amonth - jan_feb * 12, day)
    }

    pub fn year_yday(self) -> (i64, i64) {
        let (century, cyear, ayday) = self.split();
        let jf = jan_feb(ayday);
        let leap = leap(century, cyear);
        (
            year(century, cyear, jf),
            i64::from(ayday) + (1 + 31 + 28) + (leap & !jf) - 365 * jf,
        )
    }

    pub fn weekday(self) -> i64 {
        // Wednesday is 3.
        (self.0.wrapping_add(3) % 7) as i64
    }
}

fn ayday_split(ayday: u32) -> (i64, i64) {
    let d = ayday.wrapping_mul(2141).wrapping_add(197913);
    (i64::from(d >> 16), 1 + i64::from((d & 0xFFFF) / 2141))
}

fn jan_feb(ayday: u32) -> i64 {
    i64::from(i64::from(ayday) >= MARCH_THRU_DECEMBER)
}

fn leap(century: u64, cyear: u64) -> i64 {
    let y4 = i64::from(cyear.is_multiple_of(4));
    let y100 = i64::from(cyear != 0);
    let y400 = i64::from(century.is_multiple_of(4));
    y4 & (y100 | y400)
}

fn year(century: u64, cyear: u64, jan_feb: i64) -> i64 {
    (century.wrapping_mul(100).wrapping_sub(ABSOLUTE_YEARS as u64) as i64)
        .wrapping_add(cyear as i64)
        .wrapping_add(jan_feb)
}

fn date_to_abs_days(year: i64, month: i64, day: i64) -> u64 {
    let mut amonth = month as u32;
    let jan_feb = u32::from(amonth < 3);
    amonth = amonth.wrapping_add(12 * jan_feb);
    let y = (year as u64)
        .wrapping_sub(u64::from(jan_feb))
        .wrapping_add(ABSOLUTE_YEARS as u64);
    let ayday = amonth.wrapping_mul(979).wrapping_sub(2919) >> 5;
    let century = y / 100;
    let cyear = (y % 100) as u32;
    let cday = 1461 * cyear / 4;
    let centurydays = century.wrapping_mul(146097) / 4;
    centurydays.wrapping_add(
        i64::from(cday.wrapping_add(ayday))
            .wrapping_add(day)
            .wrapping_sub(1) as u64,
    )
}

fn clock(abs: u64) -> (i64, i64, i64) {
    let mut sec = (abs % SECONDS_PER_DAY as u64) as i64;
    let hour = sec / SECONDS_PER_HOUR;
    sec -= hour * SECONDS_PER_HOUR;
    let min = sec / SECONDS_PER_MINUTE;
    sec -= min * SECONDS_PER_MINUTE;
    (hour, min, sec)
}

pub fn is_leap(year: i64) -> bool {
    let mask = if year % 25 != 0 { 3 } else { 0xf };
    year & mask == 0
}

pub fn days_before(m: i64) -> i64 {
    let adj = if m >= 3 { -2 } else { 0 };
    (214 * m - 211) / 7 + adj
}

pub fn days_in(m: i64, year: i64) -> i64 {
    if m == 2 {
        return if is_leap(year) { 29 } else { 28 };
    }
    30 + ((m + (m >> 3)) & 1)
}

fn norm(mut hi: i64, mut lo: i64, base: i64) -> (i64, i64) {
    if lo < 0 {
        let n = lo.wrapping_neg().wrapping_sub(1) / base + 1;
        hi = hi.wrapping_sub(n);
        lo = lo.wrapping_add(n.wrapping_mul(base));
    }
    if lo >= base {
        let n = lo / base;
        hi = hi.wrapping_add(n);
        lo = lo.wrapping_sub(n.wrapping_mul(base));
    }
    (hi, lo)
}

/// A `time.Time` without a monotonic reading: seconds since year 1, nanoseconds, zone.
#[derive(Debug, Clone, Copy)]
pub struct Time<'a> {
    ext: i64,
    nsec: i32,
    loc: &'a Location,
}

/// `time.Date`.
#[allow(clippy::too_many_arguments)]
pub fn date(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    min: i64,
    sec: i64,
    nsec: i64,
    loc: &Location,
) -> Time<'_> {
    let (year, m) = norm(year, month.wrapping_sub(1), 12);
    let month = m + 1;
    let (sec, nsec) = norm(sec, nsec, 1_000_000_000);
    let (min, sec) = norm(min, sec, 60);
    let (hour, min) = norm(hour, min, 60);
    let (day, hour) = norm(day, hour, 24);
    let mut unix = (date_to_abs_days(year, month, day) as i64)
        .wrapping_mul(SECONDS_PER_DAY)
        .wrapping_add(hour * SECONDS_PER_HOUR + min * SECONDS_PER_MINUTE + sec)
        .wrapping_add(ABSOLUTE_TO_UNIX);
    let l = loc.lookup(unix);
    let mut offset = l.offset;
    if offset != 0 {
        let utc = unix.wrapping_sub(offset);
        if utc < l.start || utc >= l.end {
            offset = loc.lookup(utc).offset;
        }
        unix = unix.wrapping_sub(offset);
    }
    Time::unix_time(unix, nsec as i32, loc)
}

impl<'a> Time<'a> {
    fn unix_time(sec: i64, nsec: i32, loc: &'a Location) -> Time<'a> {
        Time {
            ext: sec.wrapping_add(UNIX_TO_INTERNAL),
            nsec,
            loc,
        }
    }

    /// `time.Unix(sec, nsec).In(loc)`.
    pub fn unix(mut sec: i64, mut nsec: i64, loc: &'a Location) -> Time<'a> {
        if !(0..1_000_000_000).contains(&nsec) {
            let n = nsec / 1_000_000_000;
            sec = sec.wrapping_add(n);
            nsec -= n * 1_000_000_000;
            if nsec < 0 {
                nsec += 1_000_000_000;
                sec = sec.wrapping_sub(1);
            }
        }
        Time::unix_time(sec, nsec as i32, loc)
    }

    pub fn location(&self) -> &'a Location {
        self.loc
    }

    /// `t.In(loc)`.
    pub fn in_location<'b>(&self, loc: &'b Location) -> Time<'b> {
        Time {
            ext: self.ext,
            nsec: self.nsec,
            loc,
        }
    }

    fn unix_sec(&self) -> i64 {
        self.ext.wrapping_add(INTERNAL_TO_UNIX)
    }

    pub fn unix_nano(&self) -> i64 {
        self.unix_sec()
            .wrapping_mul(1_000_000_000)
            .wrapping_add(i64::from(self.nsec))
    }

    pub fn before(&self, u: &Time<'_>) -> bool {
        self.ext < u.ext || self.ext == u.ext && self.nsec < u.nsec
    }

    pub fn after(&self, u: &Time<'_>) -> bool {
        self.ext > u.ext || self.ext == u.ext && self.nsec > u.nsec
    }

    fn add_sec(&mut self, d: i64) {
        let sum = self.ext.wrapping_add(d);
        if (sum > self.ext) == (d > 0) {
            self.ext = sum;
        } else if d > 0 {
            self.ext = i64::MAX;
        } else {
            self.ext = -i64::MAX;
        }
    }

    fn locabs(&self) -> (Vec<u8>, i64, u64) {
        let sec = self.unix_sec();
        let l = self.loc.lookup(sec);
        (l.name, l.offset, abs_seconds(sec.wrapping_add(l.offset)))
    }

    fn abs_sec(&self) -> u64 {
        self.locabs().2
    }

    pub fn date(&self) -> (i64, i64, i64) {
        abs_days(self.abs_sec()).date()
    }

    pub fn clock(&self) -> (i64, i64, i64) {
        clock(self.abs_sec())
    }

    pub fn weekday(&self) -> &'static str {
        let d = abs_days(self.abs_sec()).weekday();
        usize::try_from(d)
            .ok()
            .and_then(|d| LONG_DAY_NAMES.get(d))
            .copied()
            .unwrap_or_default()
    }

    /// `t.AddDate(years, months, days)`.
    pub fn add_date(&self, years: i64, months: i64, days: i64) -> Time<'a> {
        let (year, month, day) = self.date();
        let (hour, min, sec) = self.clock();
        date(
            year.wrapping_add(years),
            month.wrapping_add(months),
            day.wrapping_add(days),
            hour,
            min,
            sec,
            i64::from(self.nsec),
            self.loc,
        )
    }

    /// `t.Format(layout)`.
    pub fn format(&self, layout: &[u8]) -> Vec<u8> {
        let mut b = Vec::with_capacity(layout.len() + 10);
        if layout == RFC3339 || layout == RFC3339_NANO {
            self.format_rfc3339(&mut b, layout == RFC3339_NANO);
            return b;
        }
        let (name, offset, abs) = self.locabs();
        let days = abs_days(abs);
        let (mut year, mut month, mut day) = (-1i64, 0i64, 0i64);
        let mut yday = -1i64;
        let (mut hour, mut min, mut sec) = (-1i64, 0i64, 0i64);
        let mut layout = layout;
        while !layout.is_empty() {
            let (pe, std, ss) = next_std_chunk(layout);
            b.extend_from_slice(sub(layout, 0, pe));
            if std == 0 {
                break;
            }
            layout = layout.get(ss..).unwrap_or_default();
            if year < 0 && std & STD_NEED_DATE != 0 {
                (year, month, day) = days.date();
            }
            if yday < 0 && std & STD_NEED_YDAY != 0 {
                yday = days.year_yday().1;
            }
            if hour < 0 && std & STD_NEED_CLOCK != 0 {
                (hour, min, sec) = clock(abs);
            }
            match std & STD_MASK {
                STD_YEAR => append_int(&mut b, year.wrapping_abs() % 100, 2),
                STD_LONG_YEAR => append_int(&mut b, year, 4),
                STD_MONTH => b.extend_from_slice(name_of(&SHORT_MONTH_NAMES, month - 1).as_bytes()),
                STD_LONG_MONTH => b.extend_from_slice(name_of(&LONG_MONTH_NAMES, month - 1).as_bytes()),
                STD_NUM_MONTH => append_int(&mut b, month, 0),
                STD_ZERO_MONTH => append_int(&mut b, month, 2),
                STD_WEEKDAY => b.extend_from_slice(name_of(&SHORT_DAY_NAMES, days.weekday()).as_bytes()),
                STD_LONG_WEEKDAY => b.extend_from_slice(name_of(&LONG_DAY_NAMES, days.weekday()).as_bytes()),
                STD_DAY => append_int(&mut b, day, 0),
                STD_UNDER_DAY => {
                    if day < 10 {
                        b.push(b' ');
                    }
                    append_int(&mut b, day, 0);
                }
                STD_ZERO_DAY => append_int(&mut b, day, 2),
                STD_UNDER_YEAR_DAY => {
                    if yday < 100 {
                        b.push(b' ');
                        if yday < 10 {
                            b.push(b' ');
                        }
                    }
                    append_int(&mut b, yday, 0);
                }
                STD_ZERO_YEAR_DAY => append_int(&mut b, yday, 3),
                STD_HOUR => append_int(&mut b, hour, 2),
                STD_HOUR12 | STD_ZERO_HOUR12 => {
                    let hr = match hour % 12 {
                        0 => 12,
                        h => h,
                    };
                    append_int(&mut b, hr, if std == STD_HOUR12 { 0 } else { 2 });
                }
                STD_MINUTE => append_int(&mut b, min, 0),
                STD_ZERO_MINUTE => append_int(&mut b, min, 2),
                STD_SECOND => append_int(&mut b, sec, 0),
                STD_ZERO_SECOND => append_int(&mut b, sec, 2),
                STD_PM => b.extend_from_slice(if hour >= 12 { b"PM" } else { b"AM" }),
                STD_PM_LOWER => b.extend_from_slice(if hour >= 12 { b"pm" } else { b"am" }),
                STD_ISO8601_TZ
                | STD_ISO8601_COLON_TZ
                | STD_ISO8601_SECONDS_TZ
                | STD_ISO8601_SHORT_TZ
                | STD_ISO8601_COLON_SECONDS_TZ
                | STD_NUM_TZ
                | STD_NUM_COLON_TZ
                | STD_NUM_SECONDS_TZ
                | STD_NUM_SHORT_TZ
                | STD_NUM_COLON_SECONDS_TZ => {
                    let iso = matches!(
                        std,
                        STD_ISO8601_TZ
                            | STD_ISO8601_COLON_TZ
                            | STD_ISO8601_SECONDS_TZ
                            | STD_ISO8601_SHORT_TZ
                            | STD_ISO8601_COLON_SECONDS_TZ
                    );
                    if offset == 0 && iso {
                        b.push(b'Z');
                        continue;
                    }
                    let mut zone = offset / 60;
                    let mut absoffset = offset;
                    if zone < 0 {
                        b.push(b'-');
                        zone = -zone;
                        absoffset = -absoffset;
                    } else {
                        b.push(b'+');
                    }
                    append_int(&mut b, zone / 60, 2);
                    if matches!(
                        std,
                        STD_ISO8601_COLON_TZ
                            | STD_NUM_COLON_TZ
                            | STD_ISO8601_COLON_SECONDS_TZ
                            | STD_NUM_COLON_SECONDS_TZ
                    ) {
                        b.push(b':');
                    }
                    if std != STD_NUM_SHORT_TZ && std != STD_ISO8601_SHORT_TZ {
                        append_int(&mut b, zone % 60, 2);
                    }
                    if matches!(
                        std,
                        STD_ISO8601_SECONDS_TZ
                            | STD_NUM_SECONDS_TZ
                            | STD_NUM_COLON_SECONDS_TZ
                            | STD_ISO8601_COLON_SECONDS_TZ
                    ) {
                        if std == STD_NUM_COLON_SECONDS_TZ || std == STD_ISO8601_COLON_SECONDS_TZ {
                            b.push(b':');
                        }
                        append_int(&mut b, absoffset % 60, 2);
                    }
                }
                STD_TZ => {
                    if !name.is_empty() {
                        b.extend_from_slice(&name);
                        continue;
                    }
                    let mut zone = offset / 60;
                    if zone < 0 {
                        b.push(b'-');
                        zone = -zone;
                    } else {
                        b.push(b'+');
                    }
                    append_int(&mut b, zone / 60, 2);
                    append_int(&mut b, zone % 60, 2);
                }
                STD_FRAC_SECOND0 | STD_FRAC_SECOND9 => append_nano(&mut b, i64::from(self.nsec), std),
                _ => {}
            }
        }
        b
    }

    fn format_rfc3339(&self, b: &mut Vec<u8>, nanos: bool) {
        let (_, offset, abs) = self.locabs();
        let (year, month, day) = abs_days(abs).date();
        append_int(b, year, 4);
        b.push(b'-');
        append_int(b, month, 2);
        b.push(b'-');
        append_int(b, day, 2);
        b.push(b'T');
        let (hour, min, sec) = clock(abs);
        append_int(b, hour, 2);
        b.push(b':');
        append_int(b, min, 2);
        b.push(b':');
        append_int(b, sec, 2);
        if nanos {
            append_nano(
                b,
                i64::from(self.nsec),
                std_frac_second(STD_FRAC_SECOND9, 9, b'.'),
            );
        }
        if offset == 0 {
            b.push(b'Z');
            return;
        }
        let mut zone = offset / 60;
        if zone < 0 {
            b.push(b'-');
            zone = -zone;
        } else {
            b.push(b'+');
        }
        append_int(b, zone / 60, 2);
        b.push(b':');
        append_int(b, zone % 60, 2);
    }
}

fn name_of(tab: &[&'static str], i: i64) -> &'static str {
    usize::try_from(i)
        .ok()
        .and_then(|i| tab.get(i))
        .copied()
        .unwrap_or_default()
}

fn sub(s: &[u8], a: usize, b: usize) -> &[u8] {
    s.get(a..b).unwrap_or_default()
}

fn byte(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

fn rest(s: &[u8], i: usize) -> &[u8] {
    s.get(i..).unwrap_or_default()
}

// Layout elements (format.go's std* constants, same values).
const STD_NEED_DATE: i64 = 1 << 8;
const STD_NEED_YDAY: i64 = 1 << 9;
const STD_NEED_CLOCK: i64 = 1 << 10;
const STD_ARG_SHIFT: i64 = 16;
const STD_SEPARATOR_SHIFT: i64 = 28;
const STD_MASK: i64 = (1 << STD_ARG_SHIFT) - 1;
const STD_LONG_MONTH: i64 = 1 + STD_NEED_DATE;
const STD_MONTH: i64 = 2 + STD_NEED_DATE;
const STD_NUM_MONTH: i64 = 3 + STD_NEED_DATE;
const STD_ZERO_MONTH: i64 = 4 + STD_NEED_DATE;
const STD_LONG_WEEKDAY: i64 = 5 + STD_NEED_DATE;
const STD_WEEKDAY: i64 = 6 + STD_NEED_DATE;
const STD_DAY: i64 = 7 + STD_NEED_DATE;
const STD_UNDER_DAY: i64 = 8 + STD_NEED_DATE;
const STD_ZERO_DAY: i64 = 9 + STD_NEED_DATE;
const STD_UNDER_YEAR_DAY: i64 = 10 + STD_NEED_YDAY;
const STD_ZERO_YEAR_DAY: i64 = 11 + STD_NEED_YDAY;
const STD_HOUR: i64 = 12 + STD_NEED_CLOCK;
const STD_HOUR12: i64 = 13 + STD_NEED_CLOCK;
const STD_ZERO_HOUR12: i64 = 14 + STD_NEED_CLOCK;
const STD_MINUTE: i64 = 15 + STD_NEED_CLOCK;
const STD_ZERO_MINUTE: i64 = 16 + STD_NEED_CLOCK;
const STD_SECOND: i64 = 17 + STD_NEED_CLOCK;
const STD_ZERO_SECOND: i64 = 18 + STD_NEED_CLOCK;
const STD_LONG_YEAR: i64 = 19 + STD_NEED_DATE;
const STD_YEAR: i64 = 20 + STD_NEED_DATE;
const STD_PM: i64 = 21 + STD_NEED_CLOCK;
const STD_PM_LOWER: i64 = 22 + STD_NEED_CLOCK;
const STD_TZ: i64 = 23;
const STD_ISO8601_TZ: i64 = 24;
const STD_ISO8601_SECONDS_TZ: i64 = 25;
const STD_ISO8601_SHORT_TZ: i64 = 26;
const STD_ISO8601_COLON_TZ: i64 = 27;
const STD_ISO8601_COLON_SECONDS_TZ: i64 = 28;
const STD_NUM_TZ: i64 = 29;
const STD_NUM_SECONDS_TZ: i64 = 30;
const STD_NUM_SHORT_TZ: i64 = 31;
const STD_NUM_COLON_TZ: i64 = 32;
const STD_NUM_COLON_SECONDS_TZ: i64 = 33;
const STD_FRAC_SECOND0: i64 = 34;
const STD_FRAC_SECOND9: i64 = 35;

const STD0X: [i64; 6] = [
    STD_ZERO_MONTH,
    STD_ZERO_DAY,
    STD_ZERO_HOUR12,
    STD_ZERO_MINUTE,
    STD_ZERO_SECOND,
    STD_YEAR,
];

fn starts_with_lower(s: &[u8]) -> bool {
    s.first().is_some_and(u8::is_ascii_lowercase)
}

fn is_digit(s: &[u8], i: usize) -> bool {
    s.get(i).is_some_and(u8::is_ascii_digit)
}

/// `nextStdChunk`: where the first layout element starts, what it is (0 for none),
/// and where the rest starts.
fn next_std_chunk(layout: &[u8]) -> (usize, i64, usize) {
    let has = |i: usize, s: &[u8]| layout.get(i..i + s.len()) == Some(s);
    for i in 0..layout.len() {
        match byte(layout, i) {
            b'J' => {
                if has(i, b"Jan") {
                    if has(i, b"January") {
                        return (i, STD_LONG_MONTH, i + 7);
                    }
                    if !starts_with_lower(rest(layout, i + 3)) {
                        return (i, STD_MONTH, i + 3);
                    }
                }
            }
            b'M' => {
                if layout.len() >= i + 3 {
                    if has(i, b"Mon") {
                        if has(i, b"Monday") {
                            return (i, STD_LONG_WEEKDAY, i + 6);
                        }
                        if !starts_with_lower(rest(layout, i + 3)) {
                            return (i, STD_WEEKDAY, i + 3);
                        }
                    }
                    if has(i, b"MST") {
                        return (i, STD_TZ, i + 3);
                    }
                }
            }
            b'0' => {
                let c = byte(layout, i + 1);
                if layout.len() >= i + 2 && (b'1'..=b'6').contains(&c) {
                    return (i, STD0X.get(usize::from(c - b'1')).copied().unwrap_or(0), i + 2);
                }
                if has(i, b"002") {
                    return (i, STD_ZERO_YEAR_DAY, i + 3);
                }
            }
            b'1' => {
                if byte(layout, i + 1) == b'5' {
                    return (i, STD_HOUR, i + 2);
                }
                return (i, STD_NUM_MONTH, i + 1);
            }
            b'2' => {
                if has(i, b"2006") {
                    return (i, STD_LONG_YEAR, i + 4);
                }
                return (i, STD_DAY, i + 1);
            }
            b'_' => {
                if byte(layout, i + 1) == b'2' {
                    if has(i + 1, b"2006") {
                        return (i + 1, STD_LONG_YEAR, i + 5);
                    }
                    return (i, STD_UNDER_DAY, i + 2);
                }
                if has(i, b"__2") {
                    return (i, STD_UNDER_YEAR_DAY, i + 3);
                }
            }
            b'3' => return (i, STD_HOUR12, i + 1),
            b'4' => return (i, STD_MINUTE, i + 1),
            b'5' => return (i, STD_SECOND, i + 1),
            b'P' => {
                if byte(layout, i + 1) == b'M' {
                    return (i, STD_PM, i + 2);
                }
            }
            b'p' => {
                if byte(layout, i + 1) == b'm' {
                    return (i, STD_PM_LOWER, i + 2);
                }
            }
            b'-' => {
                if has(i, b"-070000") {
                    return (i, STD_NUM_SECONDS_TZ, i + 7);
                }
                if has(i, b"-07:00:00") {
                    return (i, STD_NUM_COLON_SECONDS_TZ, i + 9);
                }
                if has(i, b"-0700") {
                    return (i, STD_NUM_TZ, i + 5);
                }
                if has(i, b"-07:00") {
                    return (i, STD_NUM_COLON_TZ, i + 6);
                }
                if has(i, b"-07") {
                    return (i, STD_NUM_SHORT_TZ, i + 3);
                }
            }
            b'Z' => {
                if has(i, b"Z070000") {
                    return (i, STD_ISO8601_SECONDS_TZ, i + 7);
                }
                if has(i, b"Z07:00:00") {
                    return (i, STD_ISO8601_COLON_SECONDS_TZ, i + 9);
                }
                if has(i, b"Z0700") {
                    return (i, STD_ISO8601_TZ, i + 5);
                }
                if has(i, b"Z07:00") {
                    return (i, STD_ISO8601_COLON_TZ, i + 6);
                }
                if has(i, b"Z07") {
                    return (i, STD_ISO8601_SHORT_TZ, i + 3);
                }
            }
            c @ (b'.' | b',') => {
                let ch = byte(layout, i + 1);
                if i + 1 < layout.len() && (ch == b'0' || ch == b'9') {
                    let mut j = i + 1;
                    while j < layout.len() && byte(layout, j) == ch {
                        j += 1;
                    }
                    if !is_digit(layout, j) {
                        let code = if ch == b'9' {
                            STD_FRAC_SECOND9
                        } else {
                            STD_FRAC_SECOND0
                        };
                        let n = i64::try_from(j - (i + 1)).unwrap_or(0);
                        return (i, std_frac_second(code, n, c), j);
                    }
                }
            }
            _ => {}
        }
    }
    (layout.len(), 0, layout.len())
}

fn std_frac_second(code: i64, n: i64, c: u8) -> i64 {
    let std = code | ((n & 0xfff) << STD_ARG_SHIFT);
    if c == b'.' {
        std
    } else {
        std | 1 << STD_SEPARATOR_SHIFT
    }
}

fn digits_len(std: i64) -> i64 {
    (std >> STD_ARG_SHIFT) & 0xfff
}

fn separator(std: i64) -> u8 {
    if std >> STD_SEPARATOR_SHIFT == 0 {
        b'.'
    } else {
        b','
    }
}

/// `appendInt`.
fn append_int(b: &mut Vec<u8>, x: i64, width: i64) {
    let mut u = x as u64;
    if x < 0 {
        b.push(b'-');
        u = x.wrapping_neg() as u64;
    }
    let digits = u.to_string();
    let n = i64::try_from(digits.len()).unwrap_or(0);
    for _ in 0..(width - n).max(0) {
        b.push(b'0');
    }
    b.extend_from_slice(digits.as_bytes());
}

/// `appendNano`.
fn append_nano(b: &mut Vec<u8>, nanosec: i64, std: i64) {
    let trim = std & STD_MASK == STD_FRAC_SECOND9;
    let n = digits_len(std);
    if trim && (n == 0 || nanosec == 0) {
        return;
    }
    let dot = separator(std);
    b.push(dot);
    append_int(b, nanosec, 9);
    if n < 9 {
        let keep = b.len().saturating_sub(usize::try_from(9 - n).unwrap_or(0));
        b.truncate(keep);
    }
    if trim {
        while b.last() == Some(&b'0') {
            b.pop();
        }
        if b.last() == Some(&dot) {
            b.pop();
        }
    }
}

/// Go's time `quote`: ASCII as is (`"` and `\` escaped), every other byte as `\xNN`.
pub fn quote(s: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for &c in s {
        if !(b' '..0x80).contains(&c) {
            out.push_str("\\x");
            for nibble in [c >> 4, c & 0xF] {
                out.push(char::from(HEX.get(usize::from(nibble)).copied().unwrap_or(b'0')));
            }
        } else {
            if c == b'"' || c == b'\\' {
                out.push('\\');
            }
            out.push(char::from(c));
        }
    }
    out.push('"');
    out
}

fn parse_error(layout: &[u8], value: &[u8], layout_elem: &[u8], value_elem: &[u8]) -> String {
    format!(
        "parsing time {} as {}: cannot parse {} as {}",
        quote(value),
        quote(layout),
        quote(value_elem),
        quote(layout_elem)
    )
}

fn parse_error_msg(value: &[u8], message: &str) -> String {
    format!("parsing time {}{message}", quote(value))
}

/// `match`: ASCII letters equal ignoring case.
fn match_fold(s1: &[u8], s2: &[u8]) -> bool {
    s1.iter().zip(s2).all(|(&a, &b)| {
        if a == b {
            return true;
        }
        let (a, b) = (a | (b'a' - b'A'), b | (b'a' - b'A'));
        a == b && a.is_ascii_lowercase()
    })
}

fn lookup_name<'v>(tab: &[&str], val: &'v [u8]) -> Result<(i64, &'v [u8]), ()> {
    for (i, v) in tab.iter().enumerate() {
        let v = v.as_bytes();
        if val.len() >= v.len() && match_fold(sub(val, 0, v.len()), v) {
            return Ok((i64::try_from(i).unwrap_or(0), rest(val, v.len())));
        }
    }
    Err(())
}

/// `leadingInt`.
fn leading_int(s: &[u8]) -> Result<(u64, &[u8]), ()> {
    let mut x: u64 = 0;
    let mut i = 0;
    while let Some(&c) = s.get(i) {
        if !c.is_ascii_digit() {
            break;
        }
        if x > (1 << 63) / 10 {
            return Err(());
        }
        x = x * 10 + u64::from(c - b'0');
        if x > 1 << 63 {
            return Err(());
        }
        i += 1;
    }
    Ok((x, rest(s, i)))
}

/// `atoi`.
fn atoi(s: &[u8]) -> Result<i64, ()> {
    let (neg, s) = match s.first() {
        Some(b'-') => (true, rest(s, 1)),
        Some(b'+') => (false, rest(s, 1)),
        _ => (false, s),
    };
    let (q, rem) = leading_int(s)?;
    if !rem.is_empty() {
        return Err(());
    }
    let x = q as i64;
    Ok(if neg { x.wrapping_neg() } else { x })
}

fn getnum(s: &[u8], fixed: bool) -> Result<(i64, &[u8]), ()> {
    if !is_digit(s, 0) {
        return Err(());
    }
    let d0 = i64::from(byte(s, 0) - b'0');
    if !is_digit(s, 1) {
        if fixed {
            return Err(());
        }
        return Ok((d0, rest(s, 1)));
    }
    Ok((d0 * 10 + i64::from(byte(s, 1) - b'0'), rest(s, 2)))
}

fn getnum3(s: &[u8], fixed: bool) -> Result<(i64, &[u8]), ()> {
    let mut n = 0;
    let mut i = 0;
    while i < 3 && is_digit(s, i) {
        n = n * 10 + i64::from(byte(s, i) - b'0');
        i += 1;
    }
    if i == 0 || fixed && i != 3 {
        return Err(());
    }
    Ok((n, rest(s, i)))
}

fn cutspace(mut s: &[u8]) -> &[u8] {
    while s.first() == Some(&b' ') {
        s = rest(s, 1);
    }
    s
}

/// `skip`: the value past the layout's literal text, or where they differ.
fn skip<'v>(mut value: &'v [u8], mut prefix: &[u8]) -> Result<&'v [u8], &'v [u8]> {
    while let Some(&p) = prefix.first() {
        if p == b' ' {
            if value.first().is_some_and(|c| *c != b' ') {
                return Err(value);
            }
            prefix = cutspace(prefix);
            value = cutspace(value);
            continue;
        }
        if value.first() != Some(&p) {
            return Err(value);
        }
        prefix = rest(prefix, 1);
        value = rest(value, 1);
    }
    Ok(value)
}

fn comma_or_period(b: u8) -> bool {
    b == b'.' || b == b','
}

/// `parseNanoseconds`: nanoseconds, a range error's subject, or a bad value.
fn parse_nanoseconds(value: &[u8], nbytes: usize) -> Result<(i64, &'static str), ()> {
    if !value.first().is_some_and(|c| comma_or_period(*c)) {
        return Err(());
    }
    let nbytes = nbytes.min(10);
    let mut ns = atoi(sub(value, 1, nbytes))?;
    if ns < 0 {
        return Ok((ns, "fractional second"));
    }
    for _ in nbytes..10 {
        ns = ns.wrapping_mul(10);
    }
    Ok((ns, ""))
}

fn parse_signed_offset(value: &[u8]) -> usize {
    if !matches!(value.first(), Some(b'-' | b'+')) {
        return 0;
    }
    let tail = rest(value, 1);
    match leading_int(tail) {
        Ok((x, rem)) if rem.len() != tail.len() && x <= 23 => value.len() - rem.len(),
        _ => 0,
    }
}

fn parse_time_zone(value: &[u8]) -> Option<usize> {
    if value.len() < 3 {
        return None;
    }
    let head4 = value.get(..4);
    if head4 == Some(b"ChST") || head4 == Some(b"MeST") {
        return Some(4);
    }
    if value.get(..3) == Some(b"GMT") {
        let v = rest(value, 3);
        return Some(if v.is_empty() {
            3
        } else {
            3 + parse_signed_offset(v)
        });
    }
    if matches!(value.first(), Some(b'+' | b'-')) {
        let n = parse_signed_offset(value);
        return (n > 0).then_some(n);
    }
    let n_upper = value
        .iter()
        .take(6)
        .take_while(|c| c.is_ascii_uppercase())
        .count();
    match n_upper {
        5 if byte(value, 4) == b'T' => Some(5),
        4 if byte(value, 3) == b'T' || head4 == Some(b"WITA") => Some(4),
        3 => Some(3),
        _ => None,
    }
}

/// `time.Parse(layout, value)` with Go's Local being UTC: the instant.
pub fn parse(layout: &[u8], value: &[u8]) -> Result<Time<'static>, String> {
    if (layout == RFC3339 || layout == RFC3339_NANO)
        && let Some(t) = parse_rfc3339(value)
    {
        return Ok(t);
    }
    parse_layout(layout, value)
}

fn parse_rfc3339(s: &[u8]) -> Option<Time<'static>> {
    let mut ok = true;
    let mut parse_uint = |s: &[u8], min: i64, max: i64| -> i64 {
        match rfc3339_uint(s, min, max) {
            Some(x) => x,
            None => {
                ok = false;
                min
            }
        }
    };
    if s.len() < 19 {
        return None;
    }
    let year = parse_uint(sub(s, 0, 4), 0, 9999);
    let month = parse_uint(sub(s, 5, 7), 1, 12);
    let day = parse_uint(sub(s, 8, 10), 1, days_in(month, year));
    let hour = parse_uint(sub(s, 11, 13), 0, 23);
    let min = parse_uint(sub(s, 14, 16), 0, 59);
    let sec = parse_uint(sub(s, 17, 19), 0, 59);
    let fields_ok = ok;
    if !fields_ok
        || !(byte(s, 4) == b'-'
            && byte(s, 7) == b'-'
            && byte(s, 10) == b'T'
            && byte(s, 13) == b':'
            && byte(s, 16) == b':')
    {
        return None;
    }
    let mut s = rest(s, 19);
    let mut nsec = 0;
    if s.len() >= 2 && byte(s, 0) == b'.' && is_digit(s, 1) {
        let mut n = 2;
        while is_digit(s, n) {
            n += 1;
        }
        nsec = parse_nanoseconds(s, n).map_or(0, |r| r.0);
        s = rest(s, n);
    }
    let mut t = date(year, month, day, hour, min, sec, nsec, &UTC);
    if s != b"Z" {
        if s.len() != 6 {
            return None;
        }
        let mut ok = true;
        let mut parse_uint = |s: &[u8], min: i64, max: i64| -> i64 {
            rfc3339_uint(s, min, max).unwrap_or_else(|| {
                ok = false;
                min
            })
        };
        let hr = parse_uint(sub(s, 1, 3), 0, 23);
        let mm = parse_uint(sub(s, 4, 6), 0, 59);
        if !ok || !(matches!(byte(s, 0), b'-' | b'+') && byte(s, 3) == b':') {
            return None;
        }
        let mut zone_offset = (hr * 60 + mm) * 60;
        if byte(s, 0) == b'-' {
            zone_offset = -zone_offset;
        }
        t.add_sec(-zone_offset);
    }
    Some(t)
}

/// parseRFC3339's `parseUint`: None where it clears `ok`.
fn rfc3339_uint(s: &[u8], min: i64, max: i64) -> Option<i64> {
    let mut x = 0i64;
    for &c in s {
        if !c.is_ascii_digit() {
            return None;
        }
        x = x * 10 + i64::from(c - b'0');
    }
    (min <= x && x <= max).then_some(x)
}

/// `parse(layout, value, UTC, Local)`.
fn parse_layout(alayout: &[u8], avalue: &[u8]) -> Result<Time<'static>, String> {
    let (mut layout, mut value) = (alayout, avalue);
    let mut range_err = "";
    let (mut am_set, mut pm_set) = (false, false);
    let (mut year, mut month, mut day, mut yday) = (0i64, -1i64, -1i64, -1i64);
    let (mut hour, mut min, mut sec, mut nsec) = (0i64, 0i64, 0i64, 0i64);
    let mut z_utc = false;
    let mut zone_offset = -1i64;
    loop {
        let (pe, std, ss) = next_std_chunk(layout);
        let prefix = sub(layout, 0, pe);
        let stdstr = sub(layout, pe, ss);
        value = skip(value, prefix).map_err(|v| parse_error(alayout, avalue, prefix, v))?;
        if std == 0 {
            if !value.is_empty() {
                return Err(parse_error_msg(
                    avalue,
                    &format!(": extra text: {}", quote(value)),
                ));
            }
            break;
        }
        layout = rest(layout, ss);
        let hold = value;
        let mut bad = false;
        match std & STD_MASK {
            STD_YEAR => {
                if value.len() < 2 {
                    bad = true;
                } else {
                    let p = sub(value, 0, 2);
                    value = rest(value, 2);
                    match atoi(p) {
                        Ok(y) => year = if y >= 69 { y + 1900 } else { y + 2000 },
                        Err(()) => bad = true,
                    }
                }
            }
            STD_LONG_YEAR => {
                if value.len() < 4 || !is_digit(value, 0) {
                    bad = true;
                } else {
                    let p = sub(value, 0, 4);
                    value = rest(value, 4);
                    match atoi(p) {
                        Ok(y) => year = y,
                        Err(()) => bad = true,
                    }
                }
            }
            STD_MONTH | STD_LONG_MONTH => {
                let tab: &[&str] = if std == STD_MONTH {
                    &SHORT_MONTH_NAMES
                } else {
                    &LONG_MONTH_NAMES
                };
                match lookup_name(tab, value) {
                    Ok((m, v)) => {
                        month = m + 1;
                        value = v;
                    }
                    Err(()) => bad = true,
                }
            }
            STD_NUM_MONTH | STD_ZERO_MONTH => match getnum(value, std == STD_ZERO_MONTH) {
                Ok((m, v)) => {
                    month = m;
                    value = v;
                    if month <= 0 || 12 < month {
                        range_err = "month";
                    }
                }
                Err(()) => bad = true,
            },
            STD_WEEKDAY | STD_LONG_WEEKDAY => {
                let tab: &[&str] = if std == STD_WEEKDAY {
                    &SHORT_DAY_NAMES
                } else {
                    &LONG_DAY_NAMES
                };
                match lookup_name(tab, value) {
                    Ok((_, v)) => value = v,
                    Err(()) => bad = true,
                }
            }
            STD_DAY | STD_UNDER_DAY | STD_ZERO_DAY => {
                if std == STD_UNDER_DAY && value.first() == Some(&b' ') {
                    value = rest(value, 1);
                }
                match getnum(value, std == STD_ZERO_DAY) {
                    Ok((d, v)) => {
                        day = d;
                        value = v;
                    }
                    Err(()) => bad = true,
                }
            }
            STD_UNDER_YEAR_DAY | STD_ZERO_YEAR_DAY => {
                for _ in 0..2 {
                    if std == STD_UNDER_YEAR_DAY && value.first() == Some(&b' ') {
                        value = rest(value, 1);
                    }
                }
                match getnum3(value, std == STD_ZERO_YEAR_DAY) {
                    Ok((d, v)) => {
                        yday = d;
                        value = v;
                    }
                    Err(()) => bad = true,
                }
            }
            STD_HOUR => match getnum(value, false) {
                Ok((h, v)) => {
                    hour = h;
                    value = v;
                    if !(0..24).contains(&hour) {
                        range_err = "hour";
                    }
                }
                Err(()) => bad = true,
            },
            STD_HOUR12 | STD_ZERO_HOUR12 => match getnum(value, std == STD_ZERO_HOUR12) {
                Ok((h, v)) => {
                    hour = h;
                    value = v;
                    if !(0..=12).contains(&hour) {
                        range_err = "hour";
                    }
                }
                Err(()) => bad = true,
            },
            STD_MINUTE | STD_ZERO_MINUTE => match getnum(value, std == STD_ZERO_MINUTE) {
                Ok((m, v)) => {
                    min = m;
                    value = v;
                    if !(0..60).contains(&min) {
                        range_err = "minute";
                    }
                }
                Err(()) => bad = true,
            },
            STD_SECOND | STD_ZERO_SECOND => 'sec: {
                let Ok((s, v)) = getnum(value, std == STD_ZERO_SECOND) else {
                    bad = true;
                    break 'sec;
                };
                sec = s;
                value = v;
                if !(0..60).contains(&sec) {
                    range_err = "second";
                    break 'sec;
                }
                if value.len() >= 2 && comma_or_period(byte(value, 0)) && is_digit(value, 1) {
                    let next = next_std_chunk(layout).1 & STD_MASK;
                    if next == STD_FRAC_SECOND0 || next == STD_FRAC_SECOND9 {
                        break 'sec;
                    }
                    let mut n = 2;
                    while is_digit(value, n) {
                        n += 1;
                    }
                    match parse_nanoseconds(value, n) {
                        Ok((ns, r)) => {
                            nsec = ns;
                            range_err = r;
                        }
                        Err(()) => bad = true,
                    }
                    value = rest(value, n);
                }
            }
            STD_PM | STD_PM_LOWER => {
                if value.len() < 2 {
                    bad = true;
                } else {
                    let p = sub(value, 0, 2);
                    value = rest(value, 2);
                    let (pm, am): (&[u8], &[u8]) = if std == STD_PM {
                        (b"PM", b"AM")
                    } else {
                        (b"pm", b"am")
                    };
                    if p == pm {
                        pm_set = true;
                    } else if p == am {
                        am_set = true;
                    } else {
                        bad = true;
                    }
                }
            }
            STD_ISO8601_TZ
            | STD_ISO8601_SHORT_TZ
            | STD_ISO8601_COLON_TZ
            | STD_ISO8601_SECONDS_TZ
            | STD_ISO8601_COLON_SECONDS_TZ
            | STD_NUM_TZ
            | STD_NUM_SHORT_TZ
            | STD_NUM_COLON_TZ
            | STD_NUM_SECONDS_TZ
            | STD_NUM_COLON_SECONDS_TZ => 'tz: {
                let iso = matches!(
                    std,
                    STD_ISO8601_TZ
                        | STD_ISO8601_SHORT_TZ
                        | STD_ISO8601_COLON_TZ
                        | STD_ISO8601_SECONDS_TZ
                        | STD_ISO8601_COLON_SECONDS_TZ
                );
                if iso && value.first() == Some(&b'Z') {
                    value = rest(value, 1);
                    z_utc = true;
                    break 'tz;
                }
                let v = value;
                let (sign, hh, mm, ss): (&[u8], &[u8], &[u8], &[u8]);
                if std == STD_ISO8601_COLON_TZ || std == STD_NUM_COLON_TZ {
                    if v.len() < 6 || byte(v, 3) != b':' {
                        bad = true;
                        break 'tz;
                    }
                    (sign, hh, mm, ss) = (sub(v, 0, 1), sub(v, 1, 3), sub(v, 4, 6), b"00");
                    value = rest(v, 6);
                } else if std == STD_NUM_SHORT_TZ || std == STD_ISO8601_SHORT_TZ {
                    if v.len() < 3 {
                        bad = true;
                        break 'tz;
                    }
                    (sign, hh, mm, ss) = (sub(v, 0, 1), sub(v, 1, 3), b"00", b"00");
                    value = rest(v, 3);
                } else if std == STD_ISO8601_COLON_SECONDS_TZ || std == STD_NUM_COLON_SECONDS_TZ {
                    if v.len() < 9 || byte(v, 3) != b':' || byte(v, 6) != b':' {
                        bad = true;
                        break 'tz;
                    }
                    (sign, hh, mm, ss) = (sub(v, 0, 1), sub(v, 1, 3), sub(v, 4, 6), sub(v, 7, 9));
                    value = rest(v, 9);
                } else if std == STD_ISO8601_SECONDS_TZ || std == STD_NUM_SECONDS_TZ {
                    if v.len() < 7 {
                        bad = true;
                        break 'tz;
                    }
                    (sign, hh, mm, ss) = (sub(v, 0, 1), sub(v, 1, 3), sub(v, 3, 5), sub(v, 5, 7));
                    value = rest(v, 7);
                } else {
                    if v.len() < 5 {
                        bad = true;
                        break 'tz;
                    }
                    (sign, hh, mm, ss) = (sub(v, 0, 1), sub(v, 1, 3), sub(v, 3, 5), b"00");
                    value = rest(v, 5);
                }
                let (mut hr, mut m, mut s) = (0, 0, 0);
                match getnum(hh, true) {
                    Ok((h, _)) => {
                        hr = h;
                        match getnum(mm, true) {
                            Ok((x, _)) => {
                                m = x;
                                match getnum(ss, true) {
                                    Ok((x, _)) => s = x,
                                    Err(()) => bad = true,
                                }
                            }
                            Err(()) => bad = true,
                        }
                    }
                    Err(()) => bad = true,
                }
                if hr > 24 {
                    range_err = "time zone offset hour";
                }
                if m > 60 {
                    range_err = "time zone offset minute";
                }
                if s > 60 {
                    range_err = "time zone offset second";
                }
                zone_offset = (hr * 60 + m) * 60 + s;
                match byte(sign, 0) {
                    b'+' => {}
                    b'-' => zone_offset = -zone_offset,
                    _ => bad = true,
                }
            }
            STD_TZ => {
                if value.get(..3) == Some(b"UTC") {
                    z_utc = true;
                    value = rest(value, 3);
                } else {
                    match parse_time_zone(value) {
                        // The zone's name: Local (UTC) knows none, so the instant is
                        // the wall time's in UTC (time.parse's FixedZone(zoneName, …)).
                        Some(n) => value = rest(value, n),
                        None => bad = true,
                    }
                }
            }
            STD_FRAC_SECOND0 => {
                let ndigit = usize::try_from(1 + digits_len(std)).unwrap_or(1);
                if value.len() < ndigit {
                    bad = true;
                } else {
                    match parse_nanoseconds(value, ndigit) {
                        Ok((ns, r)) => {
                            nsec = ns;
                            range_err = r;
                        }
                        Err(()) => bad = true,
                    }
                    value = rest(value, ndigit);
                }
            }
            STD_FRAC_SECOND9 if value.len() >= 2 && comma_or_period(byte(value, 0)) && is_digit(value, 1) => {
                let mut i = 0;
                while is_digit(value, i + 1) {
                    i += 1;
                }
                match parse_nanoseconds(value, 1 + i) {
                    Ok((ns, r)) => {
                        nsec = ns;
                        range_err = r;
                    }
                    Err(()) => bad = true,
                }
                value = rest(value, 1 + i);
            }
            _ => {}
        }
        if !range_err.is_empty() {
            return Err(parse_error_msg(avalue, &format!(": {range_err} out of range")));
        }
        if bad {
            return Err(parse_error(alayout, avalue, stdstr, hold));
        }
    }
    if pm_set && hour < 12 {
        hour += 12;
    } else if am_set && hour == 12 {
        hour = 0;
    }
    if yday >= 0 {
        let (mut d, mut m) = (0, 0);
        if is_leap(year) {
            if yday == 31 + 29 {
                m = 2;
                d = 29;
            } else if yday > 31 + 29 {
                yday -= 1;
            }
        }
        if !(1..=365).contains(&yday) {
            return Err(parse_error_msg(avalue, ": day-of-year out of range"));
        }
        if m == 0 {
            m = (yday - 1) / 31 + 1;
            if days_before(m + 1) < yday {
                m += 1;
            }
            d = yday - days_before(m);
        }
        if month >= 0 && month != m {
            return Err(parse_error_msg(avalue, ": day-of-year does not match month"));
        }
        month = m;
        if day >= 0 && day != d {
            return Err(parse_error_msg(avalue, ": day-of-year does not match day"));
        }
        day = d;
    } else {
        if month < 0 {
            month = 1;
        }
        if day < 0 {
            day = 1;
        }
    }
    if day < 1 || day > days_in(month, year) {
        return Err(parse_error_msg(avalue, ": day out of range"));
    }
    let mut t = date(year, month, day, hour, min, sec, nsec, &UTC);
    if !z_utc && zone_offset != -1 {
        t.add_sec(-zone_offset);
    }
    Ok(t)
}

/// `time.ParseDuration`, in nanoseconds.
pub fn parse_duration(orig: &[u8]) -> Result<i64, String> {
    let err = |m: &str| format!("time: {m} {}", quote(orig));
    let invalid = || err("invalid duration");
    let mut s = orig;
    let mut d: u64 = 0;
    let mut neg = false;
    if let Some(&c) = s.first()
        && (c == b'-' || c == b'+')
    {
        neg = c == b'-';
        s = rest(s, 1);
    }
    if s == b"0" {
        return Ok(0);
    }
    if s.is_empty() {
        return Err(invalid());
    }
    while let Some(&c0) = s.first() {
        if !(c0 == b'.' || c0.is_ascii_digit()) {
            return Err(invalid());
        }
        let pl = s.len();
        let (mut v, r) = leading_int(s).map_err(|()| invalid())?;
        s = r;
        let pre = pl != s.len();
        let mut post = false;
        let (mut f, mut scale) = (0u64, 1f64);
        if s.first() == Some(&b'.') {
            s = rest(s, 1);
            let pl = s.len();
            (f, scale, s) = leading_fraction(s);
            post = pl != s.len();
        }
        if !pre && !post {
            return Err(invalid());
        }
        let i = s
            .iter()
            .position(|c| *c == b'.' || c.is_ascii_digit())
            .unwrap_or(s.len());
        if i == 0 {
            return Err(err("missing unit in duration"));
        }
        let u = sub(s, 0, i);
        s = rest(s, i);
        let unit: u64 = match u {
            b"ns" => 1,
            b"us" | b"\xc2\xb5s" | b"\xce\xbcs" => 1_000,
            b"ms" => 1_000_000,
            b"s" => 1_000_000_000,
            b"m" => 60 * 1_000_000_000,
            b"h" => 3600 * 1_000_000_000,
            _ => return Err(err(&format!("unknown unit {} in duration", quote(u)))),
        };
        if v > (1 << 63) / unit {
            return Err(invalid());
        }
        v *= unit;
        if f > 0 {
            v = v.wrapping_add((f as f64 * (unit as f64 / scale)) as u64);
            if v > 1 << 63 {
                return Err(invalid());
            }
        }
        d = d.wrapping_add(v);
        if d > 1 << 63 {
            return Err(invalid());
        }
    }
    if neg {
        return Ok((d as i64).wrapping_neg());
    }
    if d > (1 << 63) - 1 {
        return Err(invalid());
    }
    Ok(d as i64)
}

/// `leadingFraction`.
fn leading_fraction(s: &[u8]) -> (u64, f64, &[u8]) {
    let mut x: u64 = 0;
    let mut scale = 1f64;
    let mut overflow = false;
    let mut i = 0;
    while let Some(&c) = s.get(i) {
        if !c.is_ascii_digit() {
            break;
        }
        i += 1;
        if overflow {
            continue;
        }
        if x > ((1 << 63) - 1) / 10 {
            overflow = true;
            continue;
        }
        let y = x * 10 + u64::from(c - b'0');
        if y > 1 << 63 {
            overflow = true;
            continue;
        }
        x = y;
        scale *= 10.0;
    }
    (x, scale, rest(s, i))
}
