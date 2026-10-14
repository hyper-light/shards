//! The fluentd driver's messages (D165), as moby's fluentd driver sends them through
//! fluent-logger-golang (moby daemon/logger/fluentd; fluent-logger-golang fluent.go,
//! proto.go): a line an event of Fluentd's forward protocol, Message mode (Forward
//! Protocol Specification v1): an array of the tag, the time, the record and the options,
//! in MessagePack. The record holds the container's ID and name, the stream, the line,
//! the attributes asked for, and a part's place in its line; the time is whole seconds, or
//! with `fluentd-sub-second-precision` an EventTime to the nanosecond; with
//! `fluentd-request-ack` the options ask the server to acknowledge the event's chunk.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Where events go: `fluentd-address`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum To {
    Tcp(String),
    Tls(String),
    Unix(PathBuf),
}

/// A run's fluentd driver, as its options say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub to: To,
    /// `fluentd-sub-second-precision`.
    pub sub_second: bool,
    /// `fluentd-request-ack`.
    pub ack: bool,
    /// `fluentd-retry-wait`, and `fluentd-max-retries`: how long a send waits before it
    /// tries again, half as long again each time, and how many times it does.
    pub retry_wait: Duration,
    pub max_retries: u32,
    /// `fluentd-write-timeout` and `fluentd-read-timeout` (an ack's): none for none.
    pub write_timeout: Option<Duration>,
    pub read_timeout: Option<Duration>,
}

/// The options the driver takes besides those every driver does (moby fluentd
/// ValidateLogOpt).
pub const OPTIONS: [&str; 10] = [
    "fluentd-address",
    "fluentd-async",
    "fluentd-async-reconnect-interval",
    "fluentd-buffer-limit",
    "fluentd-max-retries",
    "fluentd-request-ack",
    "fluentd-retry-wait",
    "fluentd-sub-second-precision",
    "fluentd-write-timeout",
    "fluentd-read-timeout",
];

/// Where fluent-logger-golang sends with no address given (moby's defaultHost,
/// defaultPort).
const DEFAULT: &str = "127.0.0.1:24224";

/// The driver `opts` asks for, or why not, in shards' words. As moby reads them
/// (parseConfig): durations as Go's time.ParseDuration, booleans as strconv.ParseBool, the
/// buffer as go-units' RAMInBytes. `fluentd-async`, `fluentd-async-reconnect-interval` and
/// `fluentd-buffer-limit` are read and checked, and change nothing: every log is sent off
/// its run's path, the store its buffer (D163).
pub fn config(opts: &BTreeMap<String, String>) -> Result<Config, String> {
    let to = address(opts.get("fluentd-address").map_or("", String::as_str))?;
    let flag = |key: &str| match opts.get(key) {
        None => Ok(false),
        Some(v) if v.is_empty() => Ok(false),
        Some(v) => super::go_bool(v).ok_or_else(|| format!("--log-opt {key}={v}: true or false")),
    };
    let duration = |key: &str| -> Result<Option<Duration>, String> {
        match opts.get(key).filter(|v| !v.is_empty()) {
            None => Ok(None),
            Some(v) => match shards_cmdline::gotime::parse_duration(v) {
                Some(ns) if ns >= 0 => Ok(Some(Duration::from_nanos(ns as u64))),
                _ => Err(format!(
                    "--log-opt {key}={v}: a duration, 0 or more (500ms, 2s, 1m)"
                )),
            },
        }
    };
    flag("fluentd-async")?;
    if let Some(v) = opts.get("fluentd-buffer-limit").filter(|v| !v.is_empty()) {
        shards_cmdline::resources::ram_in_bytes(v)
            .map_err(|_| format!("--log-opt fluentd-buffer-limit={v}: a size (1m, 512k)"))?;
    }
    if let Some(interval) = duration("fluentd-async-reconnect-interval")?
        && !interval.is_zero()
        && !(Duration::from_millis(100)..=Duration::from_secs(10)).contains(&interval)
    {
        return Err(format!(
            "--log-opt fluentd-async-reconnect-interval={interval:?}: from 100ms to 10s"
        ));
    }
    let max_retries = match opts.get("fluentd-max-retries").filter(|v| !v.is_empty()) {
        None => i32::MAX as u32,
        Some(v) => match v.parse::<u32>() {
            Ok(n) if n <= i32::MAX as u32 => n,
            _ => {
                return Err(format!(
                    "--log-opt fluentd-max-retries={v}: a count, 0 to {}",
                    i32::MAX
                ));
            }
        },
    };
    Ok(Config {
        to,
        sub_second: flag("fluentd-sub-second-precision")?,
        ack: flag("fluentd-request-ack")?,
        retry_wait: duration("fluentd-retry-wait")?.unwrap_or(Duration::from_secs(1)),
        max_retries,
        write_timeout: duration("fluentd-write-timeout")?.filter(|d| !d.is_zero()),
        read_timeout: duration("fluentd-read-timeout")?.filter(|d| !d.is_zero()),
    })
}

/// `fluentd-address` as moby's parseAddress reads it: none for 127.0.0.1:24224; `tcp://`
/// unless a scheme is given; `tcp` or `tls` and a host and port, each defaulted, and no
/// path; or `unix` and a socket's path.
fn address(given: &str) -> Result<To, String> {
    if given.is_empty() {
        return Ok(To::Tcp(DEFAULT.into()));
    }
    let said = |why: &str| format!("--log-opt fluentd-address={given}: {why}");
    let full = if given.contains("://") {
        given.to_string()
    } else {
        format!("tcp://{given}")
    };
    let (scheme, rest) = full.split_once("://").unwrap_or_default();
    match scheme {
        "unix" => {
            let path = rest.trim_start_matches('/');
            if path.is_empty() {
                return Err(said("a socket's path, after unix://"));
            }
            Ok(To::Unix(PathBuf::from(format!("/{path}"))))
        }
        "tcp" | "tls" => {
            let (host, path) = match rest.find(['/', '?', '#']) {
                Some(i) => rest.split_at(i),
                None => (rest, ""),
            };
            if !path.is_empty() {
                return Err(said("a host and a port, without a path"));
            }
            let (name, port) = if let Some(v6) = host.strip_prefix('[') {
                let (name, after) = v6
                    .split_once(']')
                    .ok_or_else(|| said("an IPv6 address in brackets"))?;
                (format!("[{name}]"), after.strip_prefix(':').unwrap_or(""))
            } else {
                match host.rsplit_once(':') {
                    Some((name, port)) => (name.to_string(), port),
                    None => (host.to_string(), ""),
                }
            };
            let name = if name.is_empty() {
                "127.0.0.1".to_string()
            } else {
                name
            };
            let port = if port.is_empty() {
                24224
            } else {
                port.parse::<u16>()
                    .map_err(|_| said(&format!("the port {port}: 0 to 65535")))?
            };
            let to = format!("{name}:{port}");
            Ok(if scheme == "tls" { To::Tls(to) } else { To::Tcp(to) })
        }
        _ => Err(said(&format!("the scheme {scheme}: tcp, tls or unix"))),
    }
}

/// What every event of a run carries besides its line: its tag, its container's ID and
/// name (Docker's, with its `/`), and the attributes asked for, as moby's fluentd.New makes
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct From {
    pub tag: String,
    pub id: String,
    pub name: String,
    pub extra: BTreeMap<String, String>,
}

/// A line's part where the line is longer than moby's copier takes whole: the line's ID,
/// the part's place from 1, and whether it is the last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Part<'a> {
    pub id: &'a str,
    pub ordinal: u32,
    pub last: bool,
}

/// The event of `line` on `stream`, whose output came at `at` nanoseconds since the
/// epoch, as fluent-logger-golang's EncodeData writes it in MessagePack: the tag, the time
/// (whole seconds, or an EventTime to the nanosecond), the record, and the options, which
/// ask for `chunk`'s ack where one is given. The record's members are moby's (fluentd.Log):
/// the container's ID and name, the stream, the line, the attributes over them, then a
/// part's place over all; sorted by name, where Go writes a map in no order.
pub fn event(
    config: &Config,
    from: &From,
    stream: u8,
    at: u64,
    line: &[u8],
    part: Option<Part<'_>>,
    chunk: Option<&str>,
) -> Vec<u8> {
    let source: &[u8] = if stream == crate::spec::LOG_STDERR {
        b"stderr"
    } else {
        b"stdout"
    };
    let mut record: BTreeMap<&str, &[u8]> = BTreeMap::new();
    record.insert("container_id", from.id.as_bytes());
    record.insert("container_name", from.name.as_bytes());
    record.insert("source", source);
    record.insert("log", line);
    // An attribute named as one of the record's own is left out (D166): moby's lets it
    // replace it, so that a label `log` takes the line's place.
    for (k, v) in &from.extra {
        record.entry(k).or_insert(v.as_bytes());
    }
    let ordinal;
    if let Some(p) = &part {
        ordinal = p.ordinal.to_string();
        record.insert("partial_message", b"true");
        record.insert("partial_id", p.id.as_bytes());
        record.insert("partial_ordinal", ordinal.as_bytes());
        record.insert("partial_last", if p.last { b"true" } else { b"false" });
    }
    let mut out = Vec::with_capacity(line.len() + 256);
    array_header(&mut out, 4);
    string(&mut out, from.tag.as_bytes());
    let secs = at / 1_000_000_000;
    if config.sub_second {
        // EventTime: MessagePack's extension 0, its seconds and nanoseconds, big-endian,
        // as fluent-logger-golang writes them, the seconds as an int32 would hold them.
        out.push(0xd7);
        out.push(0);
        out.extend_from_slice(&(secs as u32).to_be_bytes());
        out.extend_from_slice(&((at % 1_000_000_000) as u32).to_be_bytes());
    } else {
        int(&mut out, i64::try_from(secs).unwrap_or(i64::MAX));
    }
    map_header(&mut out, record.len());
    for (k, v) in &record {
        string(&mut out, k.as_bytes());
        string(&mut out, v);
    }
    match chunk {
        Some(c) => {
            map_header(&mut out, 1);
            string(&mut out, b"chunk");
            string(&mut out, c.as_bytes());
        }
        None => map_header(&mut out, 0),
    }
    out
}

/// MessagePack, as tinylib/msgp writes it: each the smallest form that holds it.
fn array_header(out: &mut Vec<u8>, n: usize) {
    match n {
        0..=15 => out.push(0x90 | n as u8),
        16..=0xffff => {
            out.push(0xdc);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        _ => {
            out.push(0xdd);
            out.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_be_bytes());
        }
    }
}

fn map_header(out: &mut Vec<u8>, n: usize) {
    match n {
        0..=15 => out.push(0x80 | n as u8),
        16..=0xffff => {
            out.push(0xde);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        _ => {
            out.push(0xdf);
            out.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_be_bytes());
        }
    }
}

fn string(out: &mut Vec<u8>, s: &[u8]) {
    match s.len() {
        n @ 0..=31 => out.push(0xa0 | n as u8),
        n @ 32..=0xff => {
            out.push(0xd9);
            out.push(n as u8);
        }
        n @ 0x100..=0xffff => {
            out.push(0xda);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(0xdb);
            out.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_be_bytes());
        }
    }
    out.extend_from_slice(s);
}

/// A non-negative int as msgp's AppendInt64 writes it.
fn int(out: &mut Vec<u8>, i: i64) {
    match i {
        0..=0x7f => out.push(i as u8),
        0x80..=0x7fff => {
            out.push(0xd1);
            out.extend_from_slice(&(i as i16).to_be_bytes());
        }
        0x8000..=0x7fff_ffff => {
            out.push(0xd2);
            out.extend_from_slice(&(i as i32).to_be_bytes());
        }
        _ => {
            out.push(0xd3);
            out.extend_from_slice(&i.to_be_bytes());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// What Docker 29's fluentd driver sent, and which of its options Docker took
    /// (scripts/logdrivers/generate).
    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("testdata/fluentd.json")).unwrap()
    }

    /// The MessagePack value at `b[*at]`, as the oracle records one: strings, maps and
    /// arrays as JSON's, ints and EventTimes by their forms.
    fn unpack(b: &[u8], at: &mut usize) -> serde_json::Value {
        use serde_json::Value;
        let t = b[*at];
        *at += 1;
        let mut take = |n: usize| {
            let s = &b[*at..*at + n];
            *at += n;
            s.to_vec()
        };
        let len = |bytes: Vec<u8>| bytes.iter().fold(0usize, |n, &x| n << 8 | usize::from(x));
        match t {
            0x00..=0x7f => Value::String("fixint".into()),
            0x80..=0x8f | 0xde | 0xdf => {
                let n = match t {
                    0xde => len(take(2)),
                    0xdf => len(take(4)),
                    _ => usize::from(t & 0x0f),
                };
                let mut m = serde_json::Map::new();
                for _ in 0..n {
                    let Value::String(k) = unpack(b, at) else {
                        panic!("a key")
                    };
                    let v = unpack(b, at);
                    m.insert(k, v);
                }
                Value::Object(m)
            }
            0x90..=0x9f => Value::Array((0..usize::from(t & 0x0f)).map(|_| unpack(b, at)).collect()),
            0xa0..=0xbf | 0xd9 | 0xda | 0xdb => {
                let n = match t {
                    0xd9 => len(take(1)),
                    0xda => len(take(2)),
                    0xdb => len(take(4)),
                    _ => usize::from(t & 0x1f),
                };
                Value::String(String::from_utf8(take(n)).unwrap())
            }
            0xd2 => {
                take(4);
                Value::String("int32".into())
            }
            0xd7 => {
                assert_eq!(take(1), [0]);
                take(8);
                Value::String("ext0".into())
            }
            t => panic!("a form {t:#x} no event holds"),
        }
    }

    /// Every event the recording holds (scripts/logdrivers/generate), in whole seconds and
    /// to the nanosecond, over TCP and a unix socket, is what shards sends of the same line:
    /// its tag, its time's form, its record, a part's place in it, and its options.
    #[test]
    fn events_match_the_recording() {
        let from = From {
            tag: "oracle".into(),
            id: "{id}".into(),
            name: "{name}".into(),
            extra: [("a".to_string(), "b".to_string())].into(),
        };
        let oracle = oracle();
        let cases = oracle["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 3);
        for case in cases {
            let config = Config {
                to: To::Tcp(DEFAULT.into()),
                sub_second: case["case"] == "tcp-nanoseconds",
                ack: false,
                retry_wait: Duration::from_secs(1),
                max_retries: 0,
                write_timeout: None,
                read_timeout: None,
            };
            for e in case["events"].as_array().unwrap() {
                let record = &e["record"];
                let stream = if record["source"] == "stderr" {
                    crate::spec::LOG_STDERR
                } else {
                    crate::spec::LOG_STDOUT
                };
                let ordinal = record["partial_ordinal"].as_str().map(|o| o.parse().unwrap());
                let part = ordinal.map(|ordinal| Part {
                    id: "{partial}",
                    ordinal,
                    last: record["partial_last"] == "true",
                });
                let line = record["log"].as_str().unwrap().as_bytes();
                let event = event(
                    &config,
                    &from,
                    stream,
                    1_760_436_000_123_456_789,
                    line,
                    part,
                    None,
                );
                let mut at = 0;
                let got = unpack(&event, &mut at);
                assert_eq!(at, event.len());
                let want = serde_json::json!([e["tag"], e["time"], e["record"], e["option"]]);
                assert_eq!(got, want, "{}", case["case"]);
            }
        }
    }

    /// The options the recording accepted, shards accepts; those it refused, shards refuses.
    #[test]
    fn options_are_accepted_and_refused_as_recorded() {
        let oracle = oracle();
        for case in oracle["options"].as_array().unwrap() {
            let opts: Vec<String> = case["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o.as_str().unwrap().to_string())
                .collect();
            let refused = !case["refused"].is_null();
            let run = shards_ipc::Run {
                log_driver: "fluentd".into(),
                log_opts: opts.clone(),
                ..shards_ipc::Run::default()
            };
            let ours = crate::logging::kept(
                &run,
                crate::spec::LogRetention {
                    size: 1 << 20,
                    files: 5,
                },
            );
            assert_eq!(
                ours.is_err(),
                refused,
                "{opts:?}: recorded {}, shards {ours:?}",
                case["refused"]
            );
        }
    }

    /// An event asking for its ack names its chunk in its options.
    #[test]
    fn an_ack_is_asked_for_by_its_chunk() {
        let config = Config {
            to: To::Tcp(DEFAULT.into()),
            sub_second: false,
            ack: true,
            retry_wait: Duration::from_secs(1),
            max_retries: 0,
            write_timeout: None,
            read_timeout: None,
        };
        let from = From {
            tag: "t".into(),
            id: "i".into(),
            name: "/n".into(),
            extra: BTreeMap::new(),
        };
        let event = event(
            &config,
            &from,
            crate::spec::LOG_STDOUT,
            0,
            b"x",
            None,
            Some("Y2h1bms="),
        );
        let mut at = 0;
        let got = unpack(&event, &mut at);
        assert_eq!(got[3], serde_json::json!({"chunk": "Y2h1bms="}));
        // Time 0: a positive fixint, as msgp's AppendInt64 writes it.
        assert_eq!(event[3], 0x00);
    }

    /// An attribute named as one of a record's own is left out: a label `log` does not
    /// take the line's place, nor one `container_id` say another container wrote it.
    #[test]
    fn a_records_own_keys_are_its_own() {
        let config = Config {
            to: To::Tcp(DEFAULT.into()),
            sub_second: false,
            ack: false,
            retry_wait: Duration::from_secs(1),
            max_retries: 0,
            write_timeout: None,
            read_timeout: None,
        };
        let from = From {
            tag: "t".into(),
            id: "i".into(),
            name: "/n".into(),
            extra: [("log", "spoof"), ("container_id", "other"), ("team", "logs")]
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        };
        let event = event(&config, &from, crate::spec::LOG_STDOUT, 0, b"x", None, None);
        let mut at = 0;
        assert_eq!(
            unpack(&event, &mut at)[2],
            serde_json::json!({"container_id": "i", "container_name": "/n", "source": "stdout", "log": "x", "team": "logs"})
        );
    }
}
