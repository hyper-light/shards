//! Civil time as glibc's localtime_r, ctime and strftime give it, in a zone a fixed
//! number of seconds east of UTC: what ps's start-time columns print (output.c
//! pr_stime, pr_start, pr_bsdstart, pr_lstart).

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// A broken-down time (struct tm, with the year in full).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Tm {
    pub year: i64,
    /// 0 to 11.
    pub mon: usize,
    pub mday: i64,
    pub hour: i64,
    pub min: i64,
    pub sec: i64,
    /// 0 (Sunday) to 6.
    pub wday: usize,
    /// 0 to 365.
    pub yday: i64,
}

/// localtime_r of `t` in a zone `offset` seconds east of UTC.
pub(super) fn localtime(t: i64, offset: i32) -> Tm {
    let t = t.saturating_add(i64::from(offset));
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days, on days since 1970-01-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let mday = doy - (153 * mp + 2) / 5 + 1;
    let mon = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(mon <= 2);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    // Days before each month, in a common year.
    const BEFORE: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let mon = (mon - 1) as usize;
    let yday = BEFORE.get(mon).copied().unwrap_or(0) + mday - 1 + i64::from(leap && mon >= 2);
    Tm {
        year,
        mon,
        mday,
        hour: secs / 3600,
        min: secs / 60 % 60,
        sec: secs % 60,
        // 1970-01-01 was a Thursday.
        wday: (days + 4).rem_euclid(7) as usize,
        yday,
    }
}

impl Tm {
    /// ctime's text: asctime's `"%.3s %.3s%3d %.2d:%.2d:%.2d %d\n"`.
    pub(super) fn ctime(&self) -> String {
        format!(
            "{} {}{:3} {:02}:{:02}:{:02} {}\n",
            DAYS.get(self.wday).unwrap_or(&"???"),
            self.month(),
            self.mday,
            self.hour,
            self.min,
            self.sec,
            self.year
        )
    }

    /// strftime's `%b`.
    pub(super) fn month(&self) -> &'static str {
        MONTHS.get(self.mon).unwrap_or(&"???")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_time_matches_glibc() {
        // 2026-10-05 06:49:11 UTC, a Monday: the oracle's processes' start.
        let tm = localtime(1_791_182_951, 0);
        assert_eq!(tm.ctime(), "Mon Oct  5 06:49:11 2026\n");
        assert_eq!(tm.yday, 277);
        // East of UTC past midnight: the next day.
        assert_eq!(
            localtime(1_791_182_951, 18 * 3600).ctime(),
            "Tue Oct  6 00:49:11 2026\n"
        );
        // West, across a year boundary.
        let tm = localtime(1_767_225_600, -3600);
        assert_eq!(tm.ctime(), "Wed Dec 31 23:00:00 2025\n");
        assert_eq!((tm.yday, tm.year), (364, 2025));
        // A leap day, and the year's last day of a leap year.
        assert_eq!(localtime(951_782_400, 0).ctime(), "Tue Feb 29 00:00:00 2000\n");
        assert_eq!(localtime(978_220_800, 0).yday, 365);
        assert_eq!(localtime(0, 0).ctime(), "Thu Jan  1 00:00:00 1970\n");
        assert_eq!(localtime(-1, 0).ctime(), "Wed Dec 31 23:59:59 1969\n");
    }
}
