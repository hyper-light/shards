//! The gelf driver's messages (D164), as moby's gelf driver writes them through Graylog's
//! go-gelf (moby daemon/logger/gelf; go-gelf message.go, udpwriter.go, tcpwriter.go): a
//! line a GELF 1.1 message, its level info for stdout and err for stderr, the container's
//! own fields after the message's, as Go's encoding/json writes them; over UDP compressed
//! and cut into chunks, over TCP each followed by a NUL. Each is stamped with when its
//! output came, to the millisecond, as GELF 1.1 has it.

use std::collections::BTreeMap;

/// Where messages go: `gelf-address`, `udp://HOST:PORT` or `tcp://HOST:PORT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum To {
    Udp(String),
    Tcp(String),
}

/// How a UDP message is compressed: `gelf-compression-type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Gzip,
    Zlib,
    None,
}

/// A run's gelf driver, as its options say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub to: To,
    pub compression: Compression,
    /// `gelf-compression-level`: -1 (flate's default) to 9; go-gelf's BestSpeed, 1, unless
    /// given.
    pub level: i32,
    /// `gelf-tcp-max-reconnect` and `gelf-tcp-reconnect-delay` (seconds): go-gelf's 3 and 1
    /// unless given.
    pub reconnects: u32,
    pub delay: u32,
}

/// The options the driver takes besides those every driver does (moby gelf
/// ValidateLogOpt).
pub const OPTIONS: [&str; 5] = [
    "gelf-address",
    "gelf-compression-type",
    "gelf-compression-level",
    "gelf-tcp-max-reconnect",
    "gelf-tcp-reconnect-delay",
];

/// The longest UDP datagram go-gelf sends, a chunk's header with it (udpwriter.go,
/// ChunkSize), and the header's length: its magic, its message's ID, its number, and how
/// many there are.
pub const CHUNK: usize = 1420;
const CHUNK_HEAD: usize = 12;
/// The most chunks a message is cut into (go-gelf's limit).
pub const CHUNKS: usize = 128;

/// The driver `opts` asks for, or why not, in shards' words.
pub fn config(opts: &BTreeMap<String, String>) -> Result<Config, String> {
    let given = opts.get("gelf-address").map_or("", String::as_str);
    if given.is_empty() {
        return Err(
            "--log-opt gelf-address: where to send the log, udp://HOST:PORT or tcp://HOST:PORT".into(),
        );
    }
    let said = |why: &str| format!("--log-opt gelf-address={given}: {why}");
    let (scheme, rest) = given
        .split_once("://")
        .ok_or_else(|| said("udp://HOST:PORT or tcp://HOST:PORT"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let port_given = host
        .rsplit_once(':')
        .is_some_and(|(name, port)| !port.is_empty() && !name.ends_with('[') && port.parse::<u16>().is_ok());
    if !port_given || host.contains('@') {
        return Err(said("a host and its port, as udp://HOST:PORT or tcp://HOST:PORT"));
    }
    let to = match scheme {
        "udp" => To::Udp(host.to_string()),
        "tcp" => To::Tcp(host.to_string()),
        _ => return Err(said(&format!("the scheme {scheme}: udp or tcp"))),
    };
    let udp = matches!(to, To::Udp(_));
    let compression = match opts.get("gelf-compression-type").map(String::as_str) {
        None => Compression::Gzip,
        Some(_) if !udp => {
            return Err(
                "--log-opt gelf-compression-type: compression is UDP's, and the address is TCP".into(),
            );
        }
        Some("gzip") => Compression::Gzip,
        Some("zlib") => Compression::Zlib,
        Some("none") => Compression::None,
        Some(t) => return Err(format!("--log-opt gelf-compression-type={t}: gzip, zlib or none")),
    };
    let level = match opts.get("gelf-compression-level") {
        None => 1,
        Some(_) if !udp => {
            return Err(
                "--log-opt gelf-compression-level: compression is UDP's, and the address is TCP".into(),
            );
        }
        Some(l) => match l.parse::<i32>() {
            Ok(n) if (-1..=9).contains(&n) => n,
            _ => return Err(format!("--log-opt gelf-compression-level={l}: -1 to 9")),
        },
    };
    let count = |key: &str, default: u32| match opts.get(key) {
        None => Ok(default),
        Some(_) if udp => Err(format!("--log-opt {key}: TCP's, and the address is UDP")),
        Some(v) => v
            .parse::<u32>()
            .map_err(|_| format!("--log-opt {key}={v}: a count, 0 or more")),
    };
    Ok(Config {
        to,
        compression,
        level,
        reconnects: count("gelf-tcp-max-reconnect", 3)?,
        delay: count("gelf-tcp-reconnect-delay", 1)?,
    })
}

/// What every message of a run carries besides its line: this host's name, and the
/// container's own fields, as moby's gelf driver makes them once (gelf.New):
/// `_container_id`, `_container_name`, `_image_id`, `_image_name`, `_command`, `_tag`,
/// `_created`, then the attributes asked for, each named with a `_` before it, written as
/// Go's encoding/json writes a map: sorted by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct From {
    pub host: String,
    /// The fields, written: `"_name":value` members, comma-separated.
    pub extra: String,
}

/// The container's own fields: `fields` its own (name and value, `_created` an RFC 3339
/// time) and `attributes` those asked for, the latter's names given a `_` first, all
/// sorted by name, as Go's encoding/json writes a map. An attribute named as a field of
/// the container's own is left out (D166): moby's driver lets it replace the field, so a
/// label `container_id` says another container wrote the line.
pub fn extra(fields: &[(&str, String)], attributes: &BTreeMap<String, String>) -> String {
    let mut all: BTreeMap<String, &str> = fields
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.as_str()))
        .collect();
    for (k, v) in attributes {
        let name = if k.starts_with('_') {
            k.clone()
        } else {
            format!("_{k}")
        };
        all.entry(name).or_insert(v);
    }
    let mut out = String::new();
    for (i, (k, v)) in all.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        shards_image::json::write_string(&mut out, k.as_bytes());
        out.push(':');
        shards_image::json::write_string(&mut out, v.as_bytes());
    }
    out
}

/// GELF's levels: syslog's (moby gelfLogger.Log).
const INFO: u8 = 6;
const ERR: u8 = 3;

/// The message of `line` on `stream`, whose output came at `at` nanoseconds since the
/// epoch, as go-gelf's MarshalJSONBuf writes it: its own fields in their order, the
/// version, the host, the line, the time in seconds to the millisecond, the level, then
/// the container's.
pub fn message(from: &From, stream: u8, at: u64, line: &[u8]) -> Vec<u8> {
    let level = if stream == crate::spec::LOG_STDERR {
        ERR
    } else {
        INFO
    };
    // moby: float64(UnixNano / 1e6) / 1000.0, which Go writes as its shortest digits.
    let seconds = (at / 1_000_000) as f64 / 1000.0;
    let mut m = String::with_capacity(line.len() + from.extra.len() + 96);
    m.push_str("{\"version\":\"1.1\",\"host\":");
    shards_image::json::write_string(&mut m, from.host.as_bytes());
    m.push_str(",\"short_message\":");
    shards_image::json::write_string(&mut m, line);
    m.push_str(&format!(",\"timestamp\":{seconds},\"level\":{level}"));
    if !from.extra.is_empty() {
        m.push(',');
        m.push_str(&from.extra);
    }
    m.push('}');
    m.into_bytes()
}

/// A UDP message compressed as asked: gzip or zlib at its level, or as it is.
pub fn compress(compression: Compression, level: i32, message: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Write as _;
    // flate's DefaultCompression, -1, is its level 6.
    let level = flate2::Compression::new(u32::try_from(level).unwrap_or(6));
    match compression {
        Compression::None => Ok(message.to_vec()),
        Compression::Gzip => {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), level);
            e.write_all(message)?;
            e.finish()
        }
        Compression::Zlib => {
            let mut e = flate2::write::ZlibEncoder::new(Vec::new(), level);
            e.write_all(message)?;
            e.finish()
        }
    }
}

/// `data` as go-gelf sends it over UDP: whole where it fits one datagram, else in chunks
/// of at most [`CHUNK`] bytes, each its magic, `id`, its number and how many there are;
/// none where it would take more than [`CHUNKS`].
pub fn chunks(data: &[u8], id: [u8; 8]) -> Option<Vec<Vec<u8>>> {
    if data.len() <= CHUNK {
        return Some(vec![data.to_vec()]);
    }
    let parts: Vec<&[u8]> = data.chunks(CHUNK - CHUNK_HEAD).collect();
    let count = u8::try_from(parts.len())
        .ok()
        .filter(|&n| usize::from(n) <= CHUNKS)?;
    Some(
        parts
            .iter()
            .enumerate()
            .map(|(i, part)| {
                let mut c = Vec::with_capacity(CHUNK_HEAD + part.len());
                c.extend_from_slice(&[0x1e, 0x0f]);
                c.extend_from_slice(&id);
                c.push(u8::try_from(i).unwrap_or(u8::MAX));
                c.push(count);
                c.extend_from_slice(part);
                c
            })
            .collect(),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// What Docker 29's gelf driver sent, and which of its options Docker took
    /// (scripts/logdrivers/generate).
    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("testdata/gelf.json")).unwrap()
    }

    /// Every message Docker's driver sent, over UDP compressed every way and over TCP, is
    /// what shards makes of the same line: its fields in Go's order, written as Go writes
    /// them, the container's own sorted after them; over UDP in as many chunks, compressed
    /// as asked.
    #[test]
    fn messages_are_docker_s() {
        let fields = [
            ("_container_id", "{id}".to_string()),
            ("_container_name", "{name}".to_string()),
            ("_image_id", "{image}".to_string()),
            ("_image_name", "alpine:3.22".to_string()),
            (
                "_command",
                r#"sh -c echo out; echo err >&2; head -c 40000 /dev/zero | tr "\000" x; echo"#.to_string(),
            ),
            ("_tag", "oracle".to_string()),
            ("_created", "{created}".to_string()),
        ];
        let attributes: BTreeMap<String, String> = [("a".to_string(), "b".to_string())].into();
        let from = From {
            host: "{host}".into(),
            extra: extra(&fields, &attributes),
        };
        let at = 1_760_436_000_123_456_789;
        let oracle = oracle();
        let cases = oracle["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 4);
        for case in cases {
            let compression = match case["compression"].as_str().unwrap() {
                "gzip" => Compression::Gzip,
                "zlib" => Compression::Zlib,
                _ => Compression::None,
            };
            for m in case["messages"].as_array().unwrap() {
                let want = m["json"].as_str().unwrap();
                let sent: serde_json::Value = serde_json::from_str(&want.replace("{time}", "0")).unwrap();
                let line = sent["short_message"].as_str().unwrap();
                let stream = if sent["level"] == 3 {
                    crate::spec::LOG_STDERR
                } else {
                    crate::spec::LOG_STDOUT
                };
                let got = String::from_utf8(message(&from, stream, at, line.as_bytes())).unwrap();
                assert_eq!(got.replace("1760436000.123", "{time}"), want);
                if case["transport"] == "udp" {
                    let packed = compress(compression, 1, got.as_bytes()).unwrap();
                    let chunks = chunks(&packed, [7; 8]).unwrap();
                    assert_eq!(
                        chunks.len() as u64,
                        m["chunks"].as_u64().unwrap(),
                        "{}",
                        case["compression"]
                    );
                    let magic = match m["compression"].as_str().unwrap() {
                        "gzip" => &[0x1f, 0x8b][..],
                        "zlib" => &[0x78][..],
                        _ => b"{",
                    };
                    let first = &chunks[0];
                    let body = if chunks.len() > 1 {
                        &first[CHUNK_HEAD..]
                    } else {
                        &first[..]
                    };
                    assert!(body.starts_with(magic), "{}", case["compression"]);
                }
            }
        }
    }

    /// The options Docker takes, shards takes; those it refuses, shards refuses.
    #[test]
    fn options_are_taken_as_docker_takes_them() {
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
                log_driver: "gelf".into(),
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

    /// A message past one datagram goes in chunks of at most 1,420 bytes, each its
    /// header, numbered in order; one past 128 of them is none.
    /// An attribute named as a field of the container's own is left out: an image's label
    /// says nothing of which container wrote a line.
    #[test]
    fn the_containers_own_fields_are_its_own() {
        let attributes: BTreeMap<String, String> = [("container_id", "other"), ("team", "logs")]
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        assert_eq!(
            extra(&[("_container_id", "mine".to_string())], &attributes),
            r#""_container_id":"mine","_team":"logs""#
        );
    }

    #[test]
    fn messages_are_chunked_as_go_gelf_chunks_them() {
        let data = vec![b'x'; 3 * (CHUNK - CHUNK_HEAD) + 5];
        let got = chunks(&data, [1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        assert_eq!(got.len(), 4);
        for (i, c) in got.iter().enumerate() {
            assert!(c.len() <= CHUNK);
            assert_eq!(&c[..12], &[0x1e, 0x0f, 1, 2, 3, 4, 5, 6, 7, 8, i as u8, 4]);
        }
        assert_eq!(chunks(&[b'x'; CHUNK], [0; 8]).unwrap().len(), 1);
        assert!(chunks(&vec![b'x'; 129 * (CHUNK - CHUNK_HEAD)], [0; 8]).is_none());
    }
}
