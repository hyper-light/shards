//! Go 1.26's time zones (time/zoneinfo.go, zoneinfo_read.go, and time/tzdata's reader),
//! over the IANA database Go 1.26.1 embeds (`zoneinfo.zip`, tzdata 2025c, the archive
//! `lib/time/zoneinfo.zip` that `time/tzdata`, which OPA's topdown imports, compiles in;
//! SHA-256 8f55634d05f8bca1f7bc7c69c5933428c69357e0bdf565e5ba224e3f88ff12e8).
//!
//! OPA's `time.LoadLocation` reads the host's database first and Go's embedded copy only
//! where the host has none; shards reads the embedded copy on every host, so a policy's
//! times are the same everywhere. `Local` is UTC, as Go makes it where `TZ` is empty.

use super::go::{self, SECONDS_PER_DAY, SECONDS_PER_HOUR, SECONDS_PER_MINUTE};

static ZONEINFO: &[u8] = include_bytes!("zoneinfo.zip");

/// UTC, and Local.
pub static UTC: Location = Location {
    zones: Vec::new(),
    tx: Vec::new(),
    extend: Vec::new(),
};

const ALPHA: i64 = i64::MIN;
const OMEGA: i64 = i64::MAX;

#[derive(Debug, Clone)]
struct Zone {
    name: Vec<u8>,
    offset: i64,
    is_dst: bool,
}

#[derive(Debug, Clone, Copy)]
struct Trans {
    when: i64,
    index: usize,
}

/// A `time.Location`. UTC (and Local) have no zones.
#[derive(Debug, Clone, Default)]
pub struct Location {
    zones: Vec<Zone>,
    tx: Vec<Trans>,
    extend: Vec<u8>,
}

/// What `Location.lookup` returns.
#[derive(Debug, Clone)]
pub struct Lookup {
    pub name: Vec<u8>,
    pub offset: i64,
    pub start: i64,
    pub end: i64,
}

impl Location {
    pub fn utc() -> Location {
        Location::default()
    }

    /// `time.LoadLocation(name)` for a name other than "", "UTC" and "Local".
    pub fn load(name: &str) -> Result<Location, String> {
        let b = name.as_bytes();
        if b.windows(2).any(|w| w == b"..") || b.first().is_some_and(|c| *c == b'/' || *c == b'\\') {
            return Err("time: invalid location name".into());
        }
        match embedded(b)? {
            Some(data) => from_tzdata(data).ok_or_else(|| "malformed time zone information".to_string()),
            None => Err(format!("unknown time zone {name}")),
        }
    }

    pub fn lookup(&self, sec: i64) -> Lookup {
        if self.zones.is_empty() {
            return Lookup {
                name: b"UTC".to_vec(),
                offset: 0,
                start: ALPHA,
                end: OMEGA,
            };
        }
        let first = self.tx.first();
        if first.is_none_or(|t| sec < t.when) {
            let z = self.zones.get(self.lookup_first_zone());
            return Lookup {
                name: z.map(|z| z.name.clone()).unwrap_or_default(),
                offset: z.map_or(0, |z| z.offset),
                start: ALPHA,
                end: first.map_or(OMEGA, |t| t.when),
            };
        }
        let tx = &self.tx;
        let mut end = OMEGA;
        let (mut lo, mut hi) = (0usize, tx.len());
        while hi - lo > 1 {
            let m = (lo + hi) >> 1;
            let lim = tx.get(m).map_or(OMEGA, |t| t.when);
            if sec < lim {
                end = lim;
                hi = m;
            } else {
                lo = m;
            }
        }
        let t = tx.get(lo).copied().unwrap_or(Trans {
            when: ALPHA,
            index: 0,
        });
        let z = self.zones.get(t.index);
        if lo + 1 == tx.len()
            && !self.extend.is_empty()
            && let Some(l) = tzset(&self.extend, t.when, sec)
        {
            return l;
        }
        Lookup {
            name: z.map(|z| z.name.clone()).unwrap_or_default(),
            offset: z.map_or(0, |z| z.offset),
            start: t.when,
            end,
        }
    }

    fn lookup_first_zone(&self) -> usize {
        if !self.tx.iter().any(|t| t.index == 0) {
            return 0;
        }
        if let Some(t) = self.tx.first()
            && self.zones.get(t.index).is_some_and(|z| z.is_dst)
        {
            for zi in (0..t.index).rev() {
                if self.zones.get(zi).is_some_and(|z| !z.is_dst) {
                    return zi;
                }
            }
        }
        self.zones.iter().position(|z| !z.is_dst).unwrap_or(0)
    }
}

fn get4(b: &[u8], at: usize) -> usize {
    match b.get(at..at.saturating_add(4)) {
        Some(&[a, b, c, d]) => {
            usize::from(a) | usize::from(b) << 8 | usize::from(c) << 16 | usize::from(d) << 24
        }
        _ => 0,
    }
}

fn get2(b: &[u8], at: usize) -> usize {
    match b.get(at..at.saturating_add(2)) {
        Some(&[a, b]) => usize::from(a) | usize::from(b) << 8,
        _ => 0,
    }
}

/// time/tzdata's `loadFromEmbeddedTZData`: the named file of the stored zip, if any.
fn embedded(name: &[u8]) -> Result<Option<&'static [u8]>, String> {
    const ZCHEADER: usize = 0x02014b50;
    const ZHEADER: usize = 0x04034b50;
    let z = ZONEINFO;
    let corrupt = || "corrupt embedded tzdata".to_string();
    let tail = z.len().checked_sub(22).ok_or_else(corrupt)?;
    let n = get2(z, tail + 10);
    let mut idx = get4(z, tail + 16);
    for _ in 0..n {
        if get4(z, idx) != ZCHEADER {
            break;
        }
        let meth = get2(z, idx + 10);
        let size = get4(z, idx + 24);
        let namelen = get2(z, idx + 28);
        let xlen = get2(z, idx + 30);
        let fclen = get2(z, idx + 32);
        let off = get4(z, idx + 42);
        let zname = z.get(idx + 46..idx + 46 + namelen).ok_or_else(corrupt)?;
        idx += 46 + namelen + xlen + fclen;
        if zname != name {
            continue;
        }
        if meth != 0 {
            return Err(format!(
                "unsupported compression for {} in embedded tzdata",
                String::from_utf8_lossy(name)
            ));
        }
        if get4(z, off) != ZHEADER
            || get2(z, off + 8) != meth
            || get2(z, off + 26) != namelen
            || z.get(off + 30..off + 30 + namelen) != Some(name)
        {
            return Err(corrupt());
        }
        let start = off + 30 + namelen + get2(z, off + 28);
        return z.get(start..start + size).map(Some).ok_or_else(corrupt);
    }
    Ok(None)
}

struct Data<'a> {
    p: &'a [u8],
    error: bool,
}

impl<'a> Data<'a> {
    fn read(&mut self, n: usize) -> &'a [u8] {
        match (self.p.get(..n), self.p.get(n..)) {
            (Some(a), Some(rest)) => {
                self.p = rest;
                a
            }
            _ => {
                self.p = &[];
                self.error = true;
                &[]
            }
        }
    }

    fn big4(&mut self) -> Option<u32> {
        match *self.read(4) {
            [a, b, c, d] => Some(u32::from_be_bytes([a, b, c, d])),
            _ => {
                self.error = true;
                None
            }
        }
    }

    fn big8(&mut self) -> Option<u64> {
        let (a, b) = (self.big4(), self.big4());
        match (a, b) {
            (Some(a), Some(b)) => Some(u64::from(a) << 32 | u64::from(b)),
            _ => {
                self.error = true;
                None
            }
        }
    }
}

/// `LoadLocationFromTZData`: None is errBadData.
fn from_tzdata(data: &[u8]) -> Option<Location> {
    let mut d = Data {
        p: data,
        error: false,
    };
    if d.read(4) != b"TZif" {
        return None;
    }
    let p = d.read(16);
    let version = match p.first() {
        Some(0) if p.len() == 16 => 1,
        Some(b'2') if p.len() == 16 => 2,
        Some(b'3') if p.len() == 16 => 3,
        _ => return None,
    };
    // NUTCLocal, NStdWall, NLeap, NTime, NZone, NChar.
    let mut n = [0usize; 6];
    for v in n.iter_mut() {
        *v = usize::try_from(d.big4()?).ok()?;
    }
    let [n_utc_local, n_std_wall, n_leap, n_time, n_zone, n_char] = n;
    let mut is64 = false;
    let mut n = [n_utc_local, n_std_wall, n_leap, n_time, n_zone, n_char];
    if version > 1 {
        let skip = n_time
            .saturating_mul(5)
            .saturating_add(n_zone.saturating_mul(6))
            .saturating_add(n_char)
            .saturating_add(n_leap.saturating_mul(8))
            .saturating_add(n_std_wall)
            .saturating_add(n_utc_local)
            .saturating_add(4 + 16);
        d.read(skip);
        is64 = true;
        for v in n.iter_mut() {
            *v = usize::try_from(d.big4()?).ok()?;
        }
    }
    let [n_utc_local, n_std_wall, n_leap, n_time, n_zone, n_char] = n;
    let size = if is64 { 8 } else { 4 };
    let mut txtimes = Data {
        p: d.read(n_time.saturating_mul(size)),
        error: false,
    };
    let txzones = d.read(n_time);
    let mut zonedata = Data {
        p: d.read(n_zone.saturating_mul(6)),
        error: false,
    };
    let abbrev = d.read(n_char);
    d.read(n_leap.saturating_mul(size + 4));
    d.read(n_std_wall);
    d.read(n_utc_local);
    if d.error {
        return None;
    }
    let rest = std::mem::take(&mut d.p);
    let extend = match rest {
        [b'\n', mid @ .., b'\n'] if rest.len() > 2 => mid.to_vec(),
        _ => Vec::new(),
    };
    if n_zone == 0 {
        return None;
    }
    let mut zones = Vec::with_capacity(n_zone);
    for _ in 0..n_zone {
        let off = zonedata.big4()?;
        let dst = *zonedata.read(1).first()?;
        let b = usize::from(*zonedata.read(1).first()?);
        let tail = abbrev.get(b..).filter(|_| b < abbrev.len())?;
        let name = tail.split(|c| *c == 0).next().unwrap_or_default().to_vec();
        zones.push(Zone {
            name,
            offset: i64::from(off as i32),
            is_dst: dst != 0,
        });
    }
    let mut tx = Vec::with_capacity(n_time);
    for i in 0..n_time {
        let when = if is64 {
            txtimes.big8()? as i64
        } else {
            i64::from(txtimes.big4()? as i32)
        };
        let index = usize::from(*txzones.get(i)?);
        if index >= zones.len() {
            return None;
        }
        tx.push(Trans { when, index });
    }
    if tx.is_empty() {
        tx.push(Trans {
            when: ALPHA,
            index: 0,
        });
    }
    Some(Location { zones, tx, extend })
}

/// `tzset`: the zone at `sec` by a POSIX TZ string, past the last transition.
fn tzset(s: &[u8], last_tx_sec: i64, sec: i64) -> Option<Lookup> {
    let (mut std_name, s) = tzset_name(s)?;
    let (std_offset, s) = tzset_offset(s)?;
    let mut std_offset = std_offset.wrapping_neg();
    if s.first().is_none_or(|c| *c == b',') {
        return Some(Lookup {
            name: std_name.to_vec(),
            offset: std_offset,
            start: last_tx_sec,
            end: OMEGA,
        });
    }
    let (mut dst_name, mut s) = tzset_name(s)?;
    let mut dst_offset;
    if s.first().is_none_or(|c| *c == b',') {
        dst_offset = std_offset.wrapping_add(SECONDS_PER_HOUR);
    } else {
        let (o, rest) = tzset_offset(s)?;
        dst_offset = o.wrapping_neg();
        s = rest;
    }
    if s.is_empty() {
        s = b",M3.2.0,M11.1.0";
    }
    if !matches!(s.first(), Some(b',' | b';')) {
        return None;
    }
    let (start_rule, s) = tzset_rule(s.get(1..)?)?;
    if s.first() != Some(&b',') {
        return None;
    }
    let (end_rule, s) = tzset_rule(s.get(1..)?)?;
    if !s.is_empty() {
        return None;
    }
    let (year, yday) = go::abs_days(go::abs_seconds(sec)).year_yday();
    let ysec = yday
        .wrapping_sub(1)
        .wrapping_mul(SECONDS_PER_DAY)
        .wrapping_add(sec % SECONDS_PER_DAY);
    let ystart = sec.wrapping_sub(ysec);
    let mut start_sec = tzrule_time(year, &start_rule, std_offset);
    let mut end_sec = tzrule_time(year, &end_rule, dst_offset);
    if end_sec < start_sec {
        std::mem::swap(&mut start_sec, &mut end_sec);
        std::mem::swap(&mut std_name, &mut dst_name);
        std::mem::swap(&mut std_offset, &mut dst_offset);
    }
    Some(if ysec < start_sec {
        Lookup {
            name: std_name.to_vec(),
            offset: std_offset,
            start: ystart,
            end: start_sec.wrapping_add(ystart),
        }
    } else if ysec >= end_sec {
        Lookup {
            name: std_name.to_vec(),
            offset: std_offset,
            start: end_sec.wrapping_add(ystart),
            end: ystart.wrapping_add(365 * SECONDS_PER_DAY),
        }
    } else {
        Lookup {
            name: dst_name.to_vec(),
            offset: dst_offset,
            start: start_sec.wrapping_add(ystart),
            end: end_sec.wrapping_add(ystart),
        }
    })
}

fn tzset_name(s: &[u8]) -> Option<(&[u8], &[u8])> {
    if s.first()? != &b'<' {
        for (i, c) in s.iter().enumerate() {
            if c.is_ascii_digit() || matches!(c, b',' | b'-' | b'+') {
                if i < 3 {
                    return None;
                }
                return Some((s.get(..i)?, s.get(i..)?));
            }
        }
        if s.len() < 3 {
            return None;
        }
        return Some((s, &[]));
    }
    let i = s.iter().position(|c| *c == b'>')?;
    Some((s.get(1..i)?, s.get(i + 1..)?))
}

fn tzset_offset(s: &[u8]) -> Option<(i64, &[u8])> {
    let (neg, s) = match s.first()? {
        b'+' => (false, s.get(1..)?),
        b'-' => (true, s.get(1..)?),
        _ => (false, s),
    };
    let sign = |off: i64| if neg { -off } else { off };
    let (hours, s) = tzset_num(s, 0, 24 * 7)?;
    let mut off = hours * SECONDS_PER_HOUR;
    if s.first() != Some(&b':') {
        return Some((sign(off), s));
    }
    let (mins, s) = tzset_num(s.get(1..)?, 0, 59)?;
    off += mins * SECONDS_PER_MINUTE;
    if s.first() != Some(&b':') {
        return Some((sign(off), s));
    }
    let (secs, s) = tzset_num(s.get(1..)?, 0, 59)?;
    off += secs;
    Some((sign(off), s))
}

enum RuleKind {
    Julian,
    Doy,
    MonthWeekDay,
}

struct Rule {
    kind: RuleKind,
    day: i64,
    week: i64,
    mon: i64,
    time: i64,
}

fn tzset_rule(s: &[u8]) -> Option<(Rule, &[u8])> {
    let mut r = Rule {
        kind: RuleKind::Julian,
        day: 0,
        week: 0,
        mon: 0,
        time: 0,
    };
    let mut s = match s.first()? {
        b'J' => {
            let (jday, s) = tzset_num(s.get(1..)?, 1, 365)?;
            r.day = jday;
            s
        }
        b'M' => {
            let (mon, s) = tzset_num(s.get(1..)?, 1, 12)?;
            if s.first() != Some(&b'.') {
                return None;
            }
            let (week, s) = tzset_num(s.get(1..)?, 1, 5)?;
            if s.first() != Some(&b'.') {
                return None;
            }
            let (day, s) = tzset_num(s.get(1..)?, 0, 6)?;
            r.kind = RuleKind::MonthWeekDay;
            r.day = day;
            r.week = week;
            r.mon = mon;
            s
        }
        _ => {
            let (day, s) = tzset_num(s, 0, 365)?;
            r.kind = RuleKind::Doy;
            r.day = day;
            s
        }
    };
    if s.first() != Some(&b'/') {
        r.time = 2 * SECONDS_PER_HOUR;
        return Some((r, s));
    }
    let (offset, rest) = tzset_offset(s.get(1..)?)?;
    s = rest;
    r.time = offset;
    Some((r, s))
}

fn tzset_num(s: &[u8], min: i64, max: i64) -> Option<(i64, &[u8])> {
    if s.is_empty() {
        return None;
    }
    let mut num = 0i64;
    for (i, c) in s.iter().enumerate() {
        if !c.is_ascii_digit() {
            if i == 0 || num < min {
                return None;
            }
            return Some((num, s.get(i..)?));
        }
        num = num * 10 + i64::from(c - b'0');
        if num > max {
            return None;
        }
    }
    if num < min {
        return None;
    }
    Some((num, &[]))
}

fn tzrule_time(year: i64, r: &Rule, off: i64) -> i64 {
    let s = match r.kind {
        RuleKind::Julian => {
            let mut s = (r.day - 1) * SECONDS_PER_DAY;
            if go::is_leap(year) && r.day >= 60 {
                s += SECONDS_PER_DAY;
            }
            s
        }
        RuleKind::Doy => r.day * SECONDS_PER_DAY,
        RuleKind::MonthWeekDay => {
            let m1 = (r.mon + 9) % 12 + 1;
            let mut yy0 = year;
            if r.mon <= 2 {
                yy0 = yy0.wrapping_sub(1);
            }
            let yy1 = yy0 / 100;
            let yy2 = yy0 % 100;
            let mut dow = ((26 * m1 - 2) / 10 + 1)
                .wrapping_add(yy2)
                .wrapping_add(yy2 / 4)
                .wrapping_add(yy1 / 4)
                .wrapping_sub(yy1.wrapping_mul(2))
                % 7;
            if dow < 0 {
                dow += 7;
            }
            let mut d = r.day - dow;
            if d < 0 {
                d += 7;
            }
            for _ in 1..r.week {
                if d + 7 >= go::days_in(r.mon, year) {
                    break;
                }
                d += 7;
            }
            d += go::days_before(r.mon);
            if go::is_leap(year) && r.mon > 2 {
                d += 1;
            }
            d * SECONDS_PER_DAY
        }
    };
    s.wrapping_add(r.time).wrapping_sub(off)
}
