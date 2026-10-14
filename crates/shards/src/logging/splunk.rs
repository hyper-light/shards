//! The splunk driver's events (D167), as moby's splunk driver posts them to an HTTP Event
//! Collector (moby daemon/logger/splunk): each line an event, written as Go's
//! encoding/json writes moby's splunkMessage, `{"event":…,"time":"…","host":"…"}` and its
//! `source`, `sourcetype` and `index` where given, the events of a post one after another.
//! The event as its format says:
//! - `inline`, the default: `{"line":"…","source":"stdout","tag":"…","attrs":{…}}`, its tag
//!   and its attributes where it has them;
//! - `json`: the same, its line JSON where the line is JSON, as Go's json.Marshal writes a
//!   json.RawMessage: compacted, HTML's characters escaped;
//! - `raw`: its tag, its attributes as `KEY=VALUE`, each followed by a space, then its line;
//!   none for a line of whitespace alone, which HEC takes none of.
//!
//! Its time is its output's, in seconds to the microsecond, as Go's `%f` writes the
//! float64 of its nanoseconds over a second's.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// The options the driver takes besides those every driver does (moby splunk
/// ValidateLogOpt).
pub const OPTIONS: [&str; 13] = [
    "splunk-url",
    "splunk-token",
    "splunk-source",
    "splunk-sourcetype",
    "splunk-index",
    "splunk-capath",
    "splunk-caname",
    "splunk-insecureskipverify",
    "splunk-format",
    "splunk-verify-connection",
    "splunk-gzip",
    "splunk-gzip-level",
    "splunk-index-acknowledgment",
];

/// HEC's path for events, which moby's driver posts to whatever the URL's (parseURL).
pub const EVENTS: &str = "/services/collector/event/1.0";
/// HEC's path for indexer acknowledgments.
pub const ACKS: &str = "/services/collector/ack";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Inline,
    Json,
    Raw,
}

/// A run's splunk driver, as its options say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The collector's scheme, host and port, `http` or `https`.
    pub base: String,
    pub token: String,
    pub source: Option<String>,
    pub sourcetype: Option<String>,
    pub index: Option<String>,
    /// `splunk-capath`: the CA its certificate is verified with, besides the platform's.
    pub ca: Option<PathBuf>,
    /// `splunk-caname`: the name its certificate is verified for, where not its host's.
    pub caname: Option<String>,
    pub format: Format,
    /// `splunk-gzip`, at `splunk-gzip-level` (-1, flate's default, to 9).
    pub gzip: Option<i32>,
    /// `splunk-index-acknowledgment`: an event is delivered once Splunk says it is indexed.
    pub index_ack: bool,
}

/// The driver `opts` asks for, or why not, in shards' words.
pub fn config(opts: &BTreeMap<String, String>) -> Result<Config, String> {
    let given = opts
        .get("splunk-url")
        .ok_or("--log-opt splunk-url: where to post the log, http(s)://HOST:PORT")?;
    let said = |why: &str| format!("--log-opt splunk-url={given}: {why}");
    // moby's parseURL: an absolute http or https URL with no path but /, no query and no
    // fragment, its own path HEC's.
    let (scheme, rest) = given
        .split_once("://")
        .ok_or_else(|| said("http(s)://HOST:PORT"))?;
    if scheme != "http" && scheme != "https" {
        return Err(said(&format!("the scheme {scheme}: http or https")));
    }
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    if authority.is_empty() || !path.is_empty() || authority.contains(['?', '#']) {
        return Err(said("a host and its port, with no path, query or fragment"));
    }
    let token = opts
        .get("splunk-token")
        .ok_or("--log-opt splunk-token: the token HEC takes")?
        .clone();
    let bool_of = |key: &str| -> Result<Option<bool>, String> {
        opts.get(key)
            .map(|v| super::go_bool(v).ok_or_else(|| format!("--log-opt {key}={v}: true or false")))
            .transpose()
    };
    if bool_of("splunk-insecureskipverify")? == Some(true) {
        return Err("--log-opt splunk-insecureskipverify: shards posts a log only to a Splunk whose certificate it verifies: give its CA with --log-opt splunk-capath".into());
    }
    bool_of("splunk-verify-connection")?;
    let format = match opts.get("splunk-format").map(String::as_str) {
        None | Some("inline") => Format::Inline,
        Some("json") => Format::Json,
        Some("raw") => Format::Raw,
        Some(f) => return Err(format!("--log-opt splunk-format={f}: inline, json or raw")),
    };
    let level = match opts.get("splunk-gzip-level") {
        None => -1,
        Some(l) => match l.parse::<i32>() {
            Ok(n) if (-1..=9).contains(&n) => n,
            _ => return Err(format!("--log-opt splunk-gzip-level={l}: -1 to 9")),
        },
    };
    let config = Config {
        base: format!("{scheme}://{authority}"),
        token,
        source: opts.get("splunk-source").cloned(),
        sourcetype: opts.get("splunk-sourcetype").cloned(),
        index: opts.get("splunk-index").cloned(),
        ca: opts.get("splunk-capath").map(PathBuf::from),
        caname: opts.get("splunk-caname").cloned(),
        format,
        gzip: bool_of("splunk-gzip")?.unwrap_or(false).then_some(level),
        index_ack: bool_of("splunk-index-acknowledgment")?.unwrap_or(false),
    };
    // Its CA read as its run is made, as moby's driver reads it as it starts: none posted
    // to that cannot be.
    config.roots()?;
    Ok(config)
}

impl Config {
    /// The certificates of the CA `splunk-capath` names, besides the platform's.
    pub fn roots(&self) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, String> {
        use rustls::pki_types::CertificateDer;
        use rustls::pki_types::pem::PemObject as _;
        let Some(path) = &self.ca else {
            return Ok(Vec::new());
        };
        let certs = CertificateDer::pem_file_iter(path)
            .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
            .map_err(|e| format!("--log-opt splunk-capath={}: {e}", path.display()))?;
        if certs.is_empty() {
            return Err(format!(
                "--log-opt splunk-capath={}: no certificate in it",
                path.display()
            ));
        }
        Ok(certs)
    }
}

/// What every event of a run carries besides its line: this host's name, its tag where it
/// has one (`tag` given empty takes it away), and the attributes asked for, by name, as Go
/// writes a map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct From {
    pub host: String,
    pub tag: Option<String>,
    pub attrs: BTreeMap<String, String>,
}

/// The event of `line` on `stream`, whose output came at `at` nanoseconds since the epoch,
/// as moby's driver writes it in `config`'s format; none for a line of whitespace alone in
/// `raw`, which HEC takes none of.
pub fn event(config: &Config, from: &From, stream: u8, at: u64, line: &[u8]) -> Option<Vec<u8>> {
    let tag = from.tag.as_deref().filter(|t| !t.is_empty());
    let mut out = Vec::with_capacity(line.len() + 256);
    out.extend_from_slice(b"{\"event\":");
    if config.format == Format::Raw {
        if line.iter().all(|&b| go_space(b)) {
            return None;
        }
        let mut text = Vec::with_capacity(line.len() + 64);
        if let Some(tag) = tag {
            text.extend_from_slice(tag.as_bytes());
            text.push(b' ');
        }
        for (k, v) in &from.attrs {
            text.extend_from_slice(k.as_bytes());
            text.push(b'=');
            text.extend_from_slice(v.as_bytes());
            text.push(b' ');
        }
        text.extend_from_slice(line);
        string(&mut out, &text);
    } else {
        out.extend_from_slice(b"{\"line\":");
        match (config.format == Format::Json)
            .then(|| shards_image::json::compact_escaped(line).ok())
            .flatten()
        {
            Some(json) => out.extend_from_slice(&json),
            None => string(&mut out, line),
        }
        out.extend_from_slice(b",\"source\":");
        let source: &[u8] = if stream == crate::spec::LOG_STDERR {
            b"stderr"
        } else {
            b"stdout"
        };
        string(&mut out, source);
        if let Some(tag) = tag {
            out.extend_from_slice(b",\"tag\":");
            string(&mut out, tag.as_bytes());
        }
        if !from.attrs.is_empty() {
            out.extend_from_slice(b",\"attrs\":{");
            for (i, (k, v)) in from.attrs.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                string(&mut out, k.as_bytes());
                out.push(b':');
                string(&mut out, v.as_bytes());
            }
            out.push(b'}');
        }
        out.push(b'}');
    }
    // moby: fmt.Sprintf("%f", float64(UnixNano) / float64(time.Second)).
    out.extend_from_slice(b",\"time\":");
    string(&mut out, format!("{:.6}", at as f64 / 1e9).as_bytes());
    out.extend_from_slice(b",\"host\":");
    string(&mut out, from.host.as_bytes());
    for (key, value) in [
        ("source", &config.source),
        ("sourcetype", &config.sourcetype),
        ("index", &config.index),
    ] {
        if let Some(v) = value.as_deref().filter(|v| !v.is_empty()) {
            out.extend_from_slice(format!(",\"{key}\":").as_bytes());
            string(&mut out, v.as_bytes());
        }
    }
    out.push(b'}');
    Some(out)
}

/// `s` as a JSON string, as Go's json.Marshal writes one.
fn string(out: &mut Vec<u8>, s: &[u8]) {
    let mut text = String::with_capacity(s.len() + 2);
    shards_image::json::write_string(&mut text, s);
    out.extend_from_slice(text.as_bytes());
}

/// Whether `b` is whitespace as Go's strings.TrimSpace takes it, of a line's bytes: ASCII's
/// (unicode.IsSpace's below U+0080); a line of other bytes is not whitespace alone.
fn go_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// What Docker 29's splunk driver posted, and which of its options Docker took
    /// (scripts/logdrivers/generate).
    fn oracle() -> Value {
        serde_json::from_str(include_str!("testdata/splunk.json")).unwrap()
    }

    /// The lines the oracle's run writes (generate's splunk_print), stderr's first, as its
    /// events are kept.
    fn lines() -> Vec<(u8, Vec<u8>)> {
        let stdout = crate::spec::LOG_STDOUT;
        let mut lines = vec![
            (crate::spec::LOG_STDERR, b"err".to_vec()),
            (stdout, b"out".to_vec()),
            (stdout, br#"{"a": [1, 2], "b": "<&>"}"#.to_vec()),
            (stdout, b"  ".to_vec()),
            (stdout, b"12".to_vec()),
        ];
        for len in [16384, 16384, 7232] {
            lines.push((stdout, vec![b'x'; len]));
        }
        lines
    }

    /// Every event Docker's driver posted, in every format, gzipped or not, over HTTP or
    /// HTTPS, with its fields or its tag taken away, is what shards makes of the same line,
    /// byte for byte, but for its time, its host and its tag, which are the run's.
    #[test]
    fn events_are_docker_s() {
        for case in oracle()["cases"].as_array().unwrap() {
            let name = case["case"].as_str().unwrap();
            let mut opts: BTreeMap<String, String> = [
                ("splunk-url", "http://127.0.0.1:8088"),
                ("splunk-token", "00000000-0000-0000-0000-000000000000"),
            ]
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
            let mut set = |k: &str, v: &str| opts.insert(k.to_string(), v.to_string());
            match name {
                "json" | "raw" => {
                    set("splunk-format", name);
                }
                "fields" => {
                    set("splunk-source", "src");
                    set("splunk-sourcetype", "st");
                    set("splunk-index", "main");
                }
                _ => {}
            }
            let config = config(&opts).unwrap();
            let from = From {
                host: "{host}".into(),
                tag: (name != "notag").then(|| "{id12}".to_string()),
                attrs: [("a".to_string(), "b".to_string())].into(),
            };
            let ours: Vec<String> = lines()
                .iter()
                .filter_map(|(stream, line)| event(&config, &from, *stream, 1_791_260_014_360_799_172, line))
                .map(|e| {
                    String::from_utf8(e)
                        .unwrap()
                        .replace("\"1791260014.360799\"", "\"{time}\"")
                })
                .collect();
            let docker: Vec<&str> = case["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e.as_str().unwrap())
                .collect();
            assert_eq!(ours, docker, "{name}");
            // Posted with its token, to HEC's path, gzipped where asked.
            let post = &case["requests"].as_array().unwrap()[1];
            assert_eq!(post["path"], EVENTS);
            assert_eq!(
                post["authorization"],
                "Splunk 00000000-0000-0000-0000-000000000000"
            );
            assert_eq!(post["encoding"] == "gzip", name == "gzip");
        }
    }

    /// Its time is its output's, to the microsecond, as Go's `%f` writes the float64 of
    /// its nanoseconds over a second's: rounded as the float64 is.
    #[test]
    fn times_are_written_as_go_writes_them() {
        let config = config(
            &[("splunk-url", "http://h:1"), ("splunk-token", "t")]
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
        .unwrap();
        let from = From {
            host: "h".into(),
            tag: None,
            attrs: BTreeMap::new(),
        };
        let time = |at: u64| {
            let e: Value = serde_json::from_slice(&event(&config, &from, 1, at, b"x").unwrap()).unwrap();
            e["time"].as_str().unwrap().to_string()
        };
        assert_eq!(time(1_791_260_014_360_799_172), "1791260014.360799");
        assert_eq!(time(1_000_000_000_000_000_500), "1000000000.000000");
        assert_eq!(time(0), "0.000000");
    }

    /// Which options Docker took, shards takes, but for its own refusals: no certificate
    /// left unverified, and a CA's file read as the run is made.
    #[test]
    fn options_are_taken_as_docker_takes_them() {
        for case in oracle()["options"].as_array().unwrap() {
            let opts: Vec<String> = case["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o.as_str().unwrap().to_string())
                .collect();
            let refused =
                !case["refused"].is_null() || opts.iter().any(|o| o == "splunk-insecureskipverify=true");
            let run = shards_ipc::Run {
                log_driver: "splunk".into(),
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
                "{opts:?}: Docker {}, shards {ours:?}",
                case["refused"]
            );
        }
    }
}
