//! OPA's time builtins (topdown/time.go): thin wrappers over Go's `time` package, which
//! `go` ports, over the time zones `zone` reads from Go's embedded IANA database.

mod go;
mod zone;

use super::{Builtin, BuiltinError, Context, arg, int_operand, number_operand, string_operand};
use crate::value::{Number, Value};
use go::Time;
use zone::Location;

pub fn lookup(name: &str) -> Option<Builtin> {
    Some(match name {
        "time.add_date" => add_date,
        "time.clock" => clock,
        "time.date" => date,
        "time.diff" => diff,
        "time.format" => format,
        "time.now_ns" => now_ns,
        "time.parse_duration_ns" => parse_duration_ns,
        "time.parse_ns" => parse_ns,
        "time.parse_rfc3339_ns" => parse_rfc3339_ns,
        "time.weekday" => weekday,
        _ => return None,
    })
}

/// OPA's `acceptedTimeFormats`: the Go layouts named by their constants' names.
fn named_layout(layout: &str) -> &str {
    match layout {
        "ANSIC" => "Mon Jan _2 15:04:05 2006",
        "UnixDate" => "Mon Jan _2 15:04:05 MST 2006",
        "RubyDate" => "Mon Jan 02 15:04:05 -0700 2006",
        "RFC822" => "02 Jan 06 15:04 MST",
        "RFC822Z" => "02 Jan 06 15:04 -0700",
        "RFC850" => "Monday, 02-Jan-06 15:04:05 MST",
        "RFC1123" => "Mon, 02 Jan 2006 15:04:05 MST",
        "RFC1123Z" => "Mon, 02 Jan 2006 15:04:05 -0700",
        "RFC3339" => "2006-01-02T15:04:05Z07:00",
        "RFC3339Nano" => "2006-01-02T15:04:05.999999999Z07:00",
        other => other,
    }
}

/// `toSafeUnixNano`.
fn safe_unix_nano(t: &Time<'_>) -> Result<Option<Value>, BuiltinError> {
    let min = Time::unix(0, i64::MIN, &zone::UTC);
    let max = Time::unix(0, i64::MAX, &zone::UTC);
    if t.before(&min) || t.after(&max) {
        return Err(BuiltinError::Other("time outside of valid range".into()));
    }
    Ok(Some(Value::Number(Number::from_i64(t.unix_nano()))))
}

/// What `tzTime` reads from an operand: nanoseconds, a zone and a layout.
struct TzTime {
    ns: i64,
    loc: Location,
    layout: String,
}

const TZ_EXPECTED: &[&str] = &["either number (ns) or [number (ns), string (tz)]"];

/// `tzTime`.
fn tz_time(a: &Value) -> Result<TzTime, BuiltinError> {
    let mut loc = Location::utc();
    let mut layout = String::new();
    let n = match a {
        Value::Array(va) => {
            let Some(first) = va.first() else {
                return Err(BuiltinError::operand_type(1, a, TZ_EXPECTED));
            };
            let n = number_operand(first, 1)?;
            if let Some(tz) = va.get(1) {
                match string_operand(tz, 1)? {
                    // Local is UTC (see zone).
                    "" | "UTC" | "Local" => {}
                    name => loc = Location::load(name).map_err(BuiltinError::Other)?,
                }
            }
            if let Some(l) = va.get(2) {
                layout = string_operand(l, 1)?.to_string();
            }
            n
        }
        Value::Number(n) => n,
        _ => return Err(BuiltinError::operand_type(1, a, TZ_EXPECTED)),
    };
    let f = n.to_float().map_err(|e| BuiltinError::Other(e.to_string()))?;
    let (i, exact) = f.int();
    let ns = i64::try_from(&i)
        .ok()
        .filter(|_| exact)
        .ok_or_else(|| BuiltinError::Other("timestamp too big".into()))?;
    Ok(TzTime { ns, loc, layout })
}

fn now_ns(ctx: &mut Context, _: &[Value]) -> Result<Option<Value>, BuiltinError> {
    Ok(Some(Value::Number(Number::from_i64(ctx.time_ns))))
}

fn parse_ns(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let layout = string_operand(arg(args, 0)?, 1)?;
    let value = string_operand(arg(args, 1)?, 2)?;
    let t = go::parse(named_layout(layout).as_bytes(), value.as_bytes()).map_err(BuiltinError::Other)?;
    safe_unix_nano(&t)
}

fn parse_rfc3339_ns(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let value = string_operand(arg(args, 0)?, 1)?;
    let t = go::parse(go::RFC3339, value.as_bytes()).map_err(BuiltinError::Other)?;
    safe_unix_nano(&t)
}

fn parse_duration_ns(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let d = string_operand(arg(args, 0)?, 1)?;
    let ns = go::parse_duration(d.as_bytes()).map_err(BuiltinError::Other)?;
    Ok(Some(Value::Number(Number::from_i64(ns))))
}

fn format(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let tt = tz_time(arg(args, 0)?)?;
    let layout = match tt.layout.as_str() {
        "" => "2006-01-02T15:04:05.999999999Z07:00",
        l => named_layout(l),
    };
    let t = Time::unix(0, tt.ns, &tt.loc);
    let out = t.format(layout.as_bytes());
    Ok(Some(Value::string(String::from_utf8_lossy(&out).as_ref())))
}

fn date(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let tt = tz_time(arg(args, 0)?)?;
    let (y, m, d) = Time::unix(0, tt.ns, &tt.loc).date();
    Ok(Some(Value::array(vec![
        Value::int(y),
        Value::int(m),
        Value::int(d),
    ])))
}

fn clock(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let tt = tz_time(arg(args, 0)?)?;
    let (h, m, s) = Time::unix(0, tt.ns, &tt.loc).clock();
    Ok(Some(Value::array(vec![
        Value::int(h),
        Value::int(m),
        Value::int(s),
    ])))
}

fn weekday(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let tt = tz_time(arg(args, 0)?)?;
    Ok(Some(Value::string(Time::unix(0, tt.ns, &tt.loc).weekday())))
}

fn add_date(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let tt = tz_time(arg(args, 0)?)?;
    let years = int_operand(arg(args, 1)?, 2)?;
    let months = int_operand(arg(args, 2)?, 3)?;
    let days = int_operand(arg(args, 3)?, 4)?;
    let t = Time::unix(0, tt.ns, &tt.loc).add_date(years, months, days);
    safe_unix_nano(&t)
}

/// `builtinDiff`: icza/gox's difference of two times in years, months, days, hours,
/// minutes and seconds, both read in the first's zone.
fn diff(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let a = tz_time(arg(args, 0)?)?;
    let b = tz_time(arg(args, 1)?)?;
    let mut t1 = Time::unix(0, a.ns, &a.loc);
    let mut t2 = Time::unix(0, b.ns, &b.loc).in_location(t1.location());
    if t1.after(&t2) {
        std::mem::swap(&mut t1, &mut t2);
    }
    let (y1, m1, d1) = t1.date();
    let (y2, m2, d2) = t2.date();
    let (h1, mi1, s1) = t1.clock();
    let (h2, mi2, s2) = t2.clock();
    let mut year = y2 - y1;
    let mut month = m2 - m1;
    let mut day = d2 - d1;
    let mut hour = h2 - h1;
    let mut min = mi2 - mi1;
    let mut sec = s2 - s1;
    if sec < 0 {
        sec += 60;
        min -= 1;
    }
    if min < 0 {
        min += 60;
        hour -= 1;
    }
    if hour < 0 {
        hour += 24;
        day -= 1;
    }
    if day < 0 {
        // time.Date(y1, M1, 32, 0, 0, 0, 0, time.UTC).Day() is 32 less the month's days.
        day += go::days_in(m1, y1);
        month -= 1;
    }
    if month < 0 {
        month += 12;
        year -= 1;
    }
    Ok(Some(Value::array(
        [year, month, day, hour, min, sec]
            .into_iter()
            .map(Value::int)
            .collect(),
    )))
}
