//! The syslog driver's messages (D163), as moby's syslog driver writes them through
//! RackSec/srslog (moby daemon/logger/syslog; srslog formatter.go, framer.go, writer.go):
//! a line a message, info for stdout and err for stderr, of the facility asked for, in the
//! format asked for, framed for its transport. What Docker gets wrong is put right, each
//! difference recorded (D163):
//! - a message is stamped with when its output came, where srslog stamps when it is sent;
//! - it names this host, where srslog names its socket's local address and port, which
//!   no RFC 5424 HOSTNAME is (§6.2.4);
//! - its tag fills its fields and no more: RFC 5424's APP-NAME and MSGID are printable
//!   ASCII of at most 48 and 32 characters (§6.2.5, §6.2.7), and a space in a tag of
//!   either format would end the tag where the message's reader looks for it.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Where messages go: `syslog-address`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum To {
    /// No address: the host's own syslog, a datagram socket at `/dev/log`,
    /// `/var/run/syslog` or `/var/run/log`, else a stream one there (srslog unixSyslog).
    Local,
    Udp(String),
    Tcp(String),
    /// `tcp+tls`.
    Tls(String),
    Unix(PathBuf),
    Unixgram(PathBuf),
}

/// `syslog-format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// None given: srslog's UnixFormatter, whose messages name no host.
    Unix,
    Rfc3164,
    Rfc5424,
    /// RFC 5424 with its time to the microsecond.
    Rfc5424Micro,
}

/// A run's syslog driver, as its options say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub to: To,
    /// Its facility's code, 0 to 23.
    pub facility: u8,
    pub format: Format,
    /// `syslog-tls-ca-cert`, `syslog-tls-cert` and `syslog-tls-key`: trusted beside the
    /// platform's roots, and the client's certificate, as go-connections' tlsconfig.Client
    /// takes them.
    pub ca: Option<PathBuf>,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
}

/// The options the driver takes besides those every driver does (moby syslog
/// ValidateLogOpt).
pub const OPTIONS: [&str; 7] = [
    "syslog-address",
    "syslog-facility",
    "syslog-format",
    "syslog-tls-ca-cert",
    "syslog-tls-cert",
    "syslog-tls-key",
    "syslog-tls-skip-verify",
];

/// srslog's facilities: each name and its code.
const FACILITIES: [(&str, u8); 20] = [
    ("kern", 0),
    ("user", 1),
    ("mail", 2),
    ("daemon", 3),
    ("auth", 4),
    ("syslog", 5),
    ("lpr", 6),
    ("news", 7),
    ("uucp", 8),
    ("cron", 9),
    ("authpriv", 10),
    ("ftp", 11),
    ("local0", 16),
    ("local1", 17),
    ("local2", 18),
    ("local3", 19),
    ("local4", 20),
    ("local5", 21),
    ("local6", 22),
    ("local7", 23),
];

/// Severities: `Info` for stdout, `Err` for stderr (moby syslogger.Log).
const INFO: u8 = 6;
const ERR: u8 = 3;

/// The port an address names none of (srslog's default, moby's `defaultPort`).
const PORT: &str = "514";

/// The driver `opts` asks for, or why not, in shards' words.
pub fn config(opts: &BTreeMap<String, String>) -> Result<Config, String> {
    if opts.contains_key("syslog-tls-skip-verify") {
        return Err("--log-opt syslog-tls-skip-verify: shards sends a log only to a syslog server whose certificate it verifies: give its CA with --log-opt syslog-tls-ca-cert".into());
    }
    let to = address(opts.get("syslog-address").map_or("", String::as_str))?;
    let facility = match opts.get("syslog-facility").map(String::as_str) {
        None | Some("") => 3,
        Some(f) => match FACILITIES.iter().find(|(name, _)| *name == f) {
            Some((_, code)) => *code,
            None => match f.parse::<u8>() {
                Ok(n) if n <= 23 => n,
                _ => {
                    return Err(format!(
                        "--log-opt syslog-facility={f}: a facility's name (daemon, local0 to local7, ...) or its number, 0 to 23"
                    ));
                }
            },
        },
    };
    let format = match opts.get("syslog-format").map(String::as_str) {
        None | Some("") => Format::Unix,
        Some("rfc3164") => Format::Rfc3164,
        Some("rfc5424") => Format::Rfc5424,
        Some("rfc5424micro") => Format::Rfc5424Micro,
        Some(f) => {
            return Err(format!(
                "--log-opt syslog-format={f}: rfc3164, rfc5424 or rfc5424micro"
            ));
        }
    };
    let file = |k: &str| opts.get(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let (ca, cert, key) = (
        file("syslog-tls-ca-cert"),
        file("syslog-tls-cert"),
        file("syslog-tls-key"),
    );
    if cert.is_some() != key.is_some() {
        return Err(
            "--log-opt syslog-tls-cert and syslog-tls-key: a client certificate and its key, both".into(),
        );
    }
    let config = Config {
        to,
        facility,
        format,
        ca,
        cert,
        key,
    };
    // Its files read as its run starts, as go-connections reads them as moby's driver
    // starts: none sent to that cannot be.
    if matches!(config.to, To::Tls(_)) {
        config.tls_material()?;
    }
    Ok(config)
}

/// `syslog-address` as moby's parseAddress reads it: `udp`, `tcp` or `tcp+tls` and a
/// host, its port 514 unless given; or `unix` or `unixgram` and a socket's absolute path,
/// which is there. Where Go's URL parser would take a `unix://` address's first name for a
/// host and drop it, this says so.
fn address(given: &str) -> Result<To, String> {
    if given.is_empty() {
        return Ok(To::Local);
    }
    let said = |why: &str| format!("--log-opt syslog-address={given}: {why}");
    let (scheme, rest) = given.split_once("://").ok_or_else(|| {
        said("udp://HOST[:PORT], tcp://HOST[:PORT], tcp+tls://HOST[:PORT], unix:///PATH or unixgram:///PATH")
    })?;
    match scheme {
        "unix" | "unixgram" => {
            if !rest.starts_with('/') {
                return Err(said("a socket's absolute path, after the scheme's three slashes"));
            }
            // On Windows a drive's path follows the third slash, as a file URL's does
            // (RFC 8089): unix:///C:/logs/syslog.sock.
            #[cfg(windows)]
            let rest = rest
                .strip_prefix('/')
                .filter(|path| path.get(1..2) == Some(":"))
                .unwrap_or(rest);
            let path = PathBuf::from(rest);
            std::fs::metadata(&path).map_err(|e| said(&e.to_string()))?;
            Ok(if scheme == "unix" {
                To::Unix(path)
            } else {
                To::Unixgram(path)
            })
        }
        "udp" | "tcp" | "tcp+tls" => {
            let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
            if host.contains('@') {
                return Err(said("a host and its port, with no user"));
            }
            let host = with_port(host).ok_or_else(|| said("a host and its port, 0 to 65535"))?;
            Ok(match scheme {
                "udp" => To::Udp(host),
                "tcp" => To::Tcp(host),
                _ => To::Tls(host),
            })
        }
        _ => Err(said(&format!(
            "the scheme {scheme}: udp, tcp, tcp+tls, unix or unixgram"
        ))),
    }
}

/// `host` with its port, 514 where it names none (Go's net.SplitHostPort, then
/// JoinHostPort); none where its port is not one.
fn with_port(host: &str) -> Option<String> {
    let (name, port) = if let Some(v6) = host.strip_prefix('[') {
        let (name, after) = v6.split_once(']')?;
        match after {
            "" => (format!("[{name}]"), None),
            _ => (format!("[{name}]"), Some(after.strip_prefix(':')?)),
        }
    } else {
        match host.rsplit_once(':') {
            // A bare IPv6 address needs its brackets, as Go's parser wants them.
            Some((name, _)) if name.contains(':') => return None,
            Some((name, port)) => (name.to_string(), Some(port)),
            None => (host.to_string(), None),
        }
    };
    let port = port.unwrap_or(PORT);
    port.parse::<u16>().ok()?;
    Some(format!("{name}:{port}"))
}

/// What a message is sent with: the run's tag, this host's name and this process's ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct From {
    pub tag: String,
    pub hostname: String,
    pub pid: u32,
}

/// TLS's material: certificates trusted beside the platform's roots, and a client's
/// certificate.
pub type Material = (
    Vec<rustls::pki_types::CertificateDer<'static>>,
    Option<shards_registry::certs::ClientCertificate>,
);

impl Config {
    /// Its TLS material, read from its files: the CA's certificates, trusted beside the
    /// platform's roots, and the client's certificate and key, as go-connections'
    /// tlsconfig.Client reads them for moby's driver.
    pub fn tls_material(&self) -> Result<Material, String> {
        use rustls::pki_types::pem::PemObject as _;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let certs = |opt: &str, path: &std::path::Path| {
            let certs = CertificateDer::pem_file_iter(path)
                .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
                .map_err(|e| format!("--log-opt {opt}={}: {e}", path.display()))?;
            if certs.is_empty() {
                return Err(format!(
                    "--log-opt {opt}={}: no certificate in it",
                    path.display()
                ));
            }
            Ok(certs)
        };
        let roots = match &self.ca {
            Some(path) => certs("syslog-tls-ca-cert", path)?,
            None => Vec::new(),
        };
        let client = match (&self.cert, &self.key) {
            (Some(cert), Some(key)) => Some((
                certs("syslog-tls-cert", cert)?,
                PrivateKeyDer::from_pem_file(key)
                    .map_err(|e| format!("--log-opt syslog-tls-key={}: {e}", key.display()))?,
            )),
            _ => None,
        };
        Ok((roots, client))
    }

    /// The message of `line` (a line of the run's output without its newline, or a 16 KiB
    /// part of a longer one) on `stream`, whose output came at `at` nanoseconds since the
    /// epoch, in `zone`'s local time, framed for its transport.
    pub fn message(&self, from: &From, stream: u8, at: u64, zone: &Local, line: &[u8]) -> Vec<u8> {
        let severity = if stream == crate::spec::LOG_STDERR {
            ERR
        } else {
            INFO
        };
        let pri = u16::from(self.facility) << 3 | u16::from(severity);
        let mut m = Vec::with_capacity(line.len() + 128);
        let header = match self.format {
            Format::Unix => format!("<{pri}>{} {}[{}]: ", zone.stamp(), tag3164(&from.tag), from.pid),
            Format::Rfc3164 => format!(
                "<{pri}>{} {} {}[{}]: ",
                zone.stamp(),
                from.hostname,
                tag3164(&from.tag),
                from.pid
            ),
            Format::Rfc5424 | Format::Rfc5424Micro => format!(
                "<{pri}>1 {} {} {} {} {} - ",
                zone.rfc3339(at, self.format == Format::Rfc5424Micro),
                from.hostname,
                field5424(&from.tag, 48),
                from.pid,
                field5424(&from.tag, 32)
            ),
        };
        m.extend_from_slice(header.as_bytes());
        m.extend_from_slice(line);
        m.push(b'\n');
        // RFC 5425's octet counting over TLS, for RFC 5424's formats (moby parseLogFormat).
        if matches!(self.to, To::Tls(_)) && matches!(self.format, Format::Rfc5424 | Format::Rfc5424Micro) {
            let mut framed = format!("{} ", m.len()).into_bytes();
            framed.extend(m);
            return framed;
        }
        m
    }
}

/// A tag as RFC 3164's TAG and srslog's Unix format carry it: whitespace and control
/// characters, which would end it early, as `_`.
fn tag3164(tag: &str) -> String {
    tag.chars()
        .map(|c| {
            if c.is_whitespace() || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// A tag as one of RFC 5424's fields: printable ASCII but space, at most `most` of it, or
/// its NILVALUE where none is left.
fn field5424(tag: &str, most: usize) -> String {
    let field: String = tag
        .chars()
        .map(|c| if ('!'..='~').contains(&c) { c } else { '_' })
        .take(most)
        .collect();
    if field.is_empty() { "-".into() } else { field }
}

/// A moment in this host's time zone, as Go's `time.Local` gives it: the fields
/// localtime(3) breaks it into, and its zone's offset east of UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Local {
    pub year: i64,
    /// 0 to 11.
    pub mon: usize,
    pub mday: i64,
    pub hour: i64,
    pub min: i64,
    pub sec: i64,
    pub offset: i64,
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

impl Local {
    /// `at`, nanoseconds since the epoch, in this host's time zone.
    #[cfg(unix)]
    pub fn of(at: u64) -> Option<Local> {
        let t = i64::try_from(at / 1_000_000_000).ok()?;
        // SAFETY: a zeroed tm is a valid out-parameter for localtime_r(3).
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: localtime_r(3) reads one time_t and fills one tm.
        if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
            return None;
        }
        Some(Local {
            year: i64::from(tm.tm_year) + 1900,
            mon: usize::try_from(tm.tm_mon).ok()?,
            mday: i64::from(tm.tm_mday),
            hour: i64::from(tm.tm_hour),
            min: i64::from(tm.tm_min),
            sec: i64::from(tm.tm_sec),
            offset: tm.tm_gmtoff,
        })
    }

    /// `at`, nanoseconds since the epoch, in this host's time zone on Windows: its offset
    /// that instant's, as Go's `time.Local` has it there (cli/listing.rs `local`), and its
    /// fields by the proleptic Gregorian calendar, as localtime(3) breaks a time into them.
    #[cfg(windows)]
    pub fn of(at: u64) -> Option<Local> {
        let t = i64::try_from(at / 1_000_000_000).ok()?;
        let offset = crate::cli::listing::local(t).offset;
        let local = i128::from(t).checked_add(i128::from(offset))?;
        let (year, month, mday) = shards_cmdline::format::civil(local.div_euclid(86_400));
        let secs = local.rem_euclid(86_400);
        Some(Local {
            year: i64::try_from(year).ok()?,
            mon: usize::try_from(month.checked_sub(1)?).ok()?,
            mday: i64::try_from(mday).ok()?,
            hour: i64::try_from(secs / 3600).ok()?,
            min: i64::try_from(secs / 60 % 60).ok()?,
            sec: i64::try_from(secs % 60).ok()?,
            offset,
        })
    }

    /// Go's `time.Stamp`: `Jan _2 15:04:05`.
    fn stamp(&self) -> String {
        format!(
            "{} {:>2} {:02}:{:02}:{:02}",
            MONTHS.get(self.mon).unwrap_or(&"???"),
            self.mday,
            self.hour,
            self.min,
            self.sec
        )
    }

    /// Go's `time.RFC3339`, or with six digits of fraction, its zone `Z` in UTC and
    /// `±hh:mm` elsewhere.
    fn rfc3339(&self, at: u64, micro: bool) -> String {
        let fraction = if micro {
            format!(".{:06}", at % 1_000_000_000 / 1_000)
        } else {
            String::new()
        };
        let zone = if self.offset == 0 {
            "Z".to_string()
        } else {
            let sign = if self.offset < 0 { '-' } else { '+' };
            let off = self.offset.abs();
            format!("{sign}{:02}:{:02}", off / 3600, off % 3600 / 60)
        };
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{fraction}{zone}",
            self.year,
            self.mon + 1,
            self.mday,
            self.hour,
            self.min,
            self.sec
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A moment broken into this host's local fields is that moment: its fields, read back
    /// by the proleptic Gregorian calendar, less its zone's offset, are its seconds since the
    /// epoch; localtime(3)'s on Unix, the zone's offset and the calendar's on Windows.
    #[test]
    fn a_local_time_is_its_moment() {
        // Howard Hinnant's days_from_civil.
        let days = |y: i64, m: i64, d: i64| {
            let y = if m <= 2 { y - 1 } else { y };
            let era = y.div_euclid(400);
            let yoe = y - era * 400;
            let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
            let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
            era * 146_097 + doe - 719_468
        };
        for t in [0u64, 951_782_400, 1_709_164_800, 1_761_566_400, 4_102_444_799] {
            let l = Local::of(t * 1_000_000_000).unwrap();
            let mon = i64::try_from(l.mon).unwrap() + 1;
            let read = days(l.year, mon, l.mday) * 86_400 + l.hour * 3_600 + l.min * 60 + l.sec;
            assert_eq!(read - l.offset, i64::try_from(t).unwrap(), "{t}: {l:?}");
            assert!((0..12).contains(&l.mon) && (1..=31).contains(&l.mday), "{l:?}");
        }
    }

    /// What Docker 29's syslog driver sent, and which of its options Docker took
    /// (scripts/logdrivers/generate).
    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("testdata/syslog.json")).unwrap()
    }

    /// Every message the recording holds (scripts/logdrivers/generate), on every
    /// transport, in every format and facility, is what shards makes of the same line: its
    /// time, host and process ID those a test gives (the recording's host was its socket's
    /// address, D163).
    #[test]
    fn messages_match_the_recording() {
        let from = From {
            tag: "oracle".into(),
            hostname: "host.example".into(),
            pid: 4242,
        };
        // 2026-10-14T10:00:00.123456789Z.
        let at = 1_760_436_000_123_456_789;
        let zone = Local {
            year: 2026,
            mon: 9,
            mday: 14,
            hour: 10,
            min: 0,
            sec: 0,
            offset: 0,
        };
        let oracle = oracle();
        let cases = oracle["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 25);
        for case in cases {
            let (transport, format) = (
                case["transport"].as_str().unwrap(),
                case["format"].as_str().unwrap(),
            );
            let config = Config {
                to: match transport {
                    "udp" => To::Udp("127.0.0.1:5514".into()),
                    "tcp" => To::Tcp("127.0.0.1:5515".into()),
                    "tls" => To::Tls("127.0.0.1:5516".into()),
                    "unix" => To::Unix("/s".into()),
                    _ => To::Unixgram("/s".into()),
                },
                facility: if case["facility"] == "local7" { 23 } else { 3 },
                format: match format {
                    "unix" => Format::Unix,
                    "rfc3164" => Format::Rfc3164,
                    "rfc5424" => Format::Rfc5424,
                    _ => Format::Rfc5424Micro,
                },
                ca: None,
                cert: None,
                key: None,
            };
            let time = if format == "rfc5424micro" {
                "2026-10-14T10:00:00.123456Z"
            } else {
                "2026-10-14T10:00:00Z"
            };
            for m in case["messages"].as_array().unwrap() {
                let content = match &m["content"] {
                    serde_json::Value::String(c) => c.clone(),
                    run => "x".repeat(run["x"].as_u64().unwrap() as usize),
                };
                let head = m["head"]
                    .as_str()
                    .unwrap()
                    .replace("{stamp}", "Oct 14 10:00:00")
                    .replace("{time}", time)
                    .replace("{host}", &from.hostname)
                    .replace("{pid}", "4242");
                // Its severity says its stream: err for stderr, info for stdout.
                let pri: u16 = head.get(1..head.find('>').unwrap()).unwrap().parse().unwrap();
                let stream = if pri & 7 == u16::from(ERR) {
                    crate::spec::LOG_STDERR
                } else {
                    crate::spec::LOG_STDOUT
                };
                let mut want = format!("{head}{content}\n").into_bytes();
                if case["framing"] == "octet-counted" {
                    want = [format!("{} ", want.len()).into_bytes(), want].concat();
                }
                let got = config.message(&from, stream, at, &zone, content.as_bytes());
                assert_eq!(
                    String::from_utf8_lossy(&got),
                    String::from_utf8_lossy(&want),
                    "{transport} {format}"
                );
            }
        }
    }

    /// The options the recording accepted, shards accepts, and those it refused, shards
    /// refuses; but for those shards refuses for reasons of its own, each given (D163).
    #[test]
    fn options_are_accepted_and_refused_as_recorded() {
        let dir = shards_testdir::TempDir::new("syslog-options").unwrap();
        for socket in ["stream.sock", "gram.sock"] {
            std::fs::write(dir.join(socket), b"").unwrap();
        }
        let deviations = [
            // Go's parser takes `host` for the URL's host, and moby's driver its path.
            "syslog-address=unix://host{dir}/stream.sock",
            // shards sends a log only to a server whose certificate it verifies.
            "syslog-tls-skip-verify=true",
        ];
        let oracle = oracle();
        for case in oracle["options"].as_array().unwrap() {
            let option = case["option"].as_str().unwrap();
            let refused = !case["refused"].is_null();
            // A URL's path: Windows' drive after the third slash, its separators forward.
            let url_path = if cfg!(windows) {
                format!("/{}", dir.display().to_string().replace('\\', "/"))
            } else {
                dir.display().to_string()
            };
            let given = option.replace("{dir}", &url_path);
            let run = shards_ipc::Run {
                log_driver: "syslog".into(),
                log_opts: vec![given.clone()],
                ..shards_ipc::Run::default()
            };
            let ours = crate::logging::kept(
                &run,
                crate::spec::LogRetention {
                    size: 1 << 20,
                    files: 5,
                },
            );
            let deviates = deviations.contains(&option);
            assert_eq!(
                ours.is_err(),
                refused != deviates,
                "{given}: recorded {}, shards {ours:?}",
                if refused { "refused" } else { "took" }
            );
        }
    }

    /// Addresses as moby reads them, its port 514 unless given; and the tag kept to its
    /// fields: a space would end it early, and RFC 5424 bounds them.
    #[test]
    fn addresses_ports_and_tags() {
        assert_eq!(address(""), Ok(To::Local));
        assert_eq!(
            address("udp://example.com"),
            Ok(To::Udp("example.com:514".into()))
        );
        assert_eq!(address("tcp://[::1]:600"), Ok(To::Tcp("[::1]:600".into())));
        assert_eq!(address("tcp+tls://h:6514/x"), Ok(To::Tls("h:6514".into())));
        for refused in ["udp://h:65536", "udp://u@h", "unixgram://rel/x", "ftp://h"] {
            assert!(address(refused).is_err(), "{refused}");
        }
        assert_eq!(tag3164("a b\tc"), "a_b_c");
        assert_eq!(field5424("a b", 48), "a_b");
        assert_eq!(field5424(&"t".repeat(60), 48).len(), 48);
        assert_eq!(field5424(&"t".repeat(60), 32).len(), 32);
        assert_eq!(field5424("", 32), "-");
    }
}
