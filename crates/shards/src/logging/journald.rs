//! The journald driver's entries (D166), as moby's journald driver sends them through
//! go-systemd's journal.Send (moby daemon/logger/journald; go-systemd journal_unix.go):
//! each line an entry of systemd's native protocol (systemd.io, "Native Journal
//! Protocol"), each field `NAME=value` and a newline, or, where its value holds a newline,
//! its name and a newline, the value's length as a little-endian u64, the value and a
//! newline. Its priority (6 for stdout, 3 for stderr) and its line first; then the
//! container's own fields, the attributes asked for, its output's time, a part's place in
//! its line, and its ordinal in the run's log, as moby's journald.Log makes them. The
//! journal is Linux's: elsewhere its entries are made by its tests alone, held to
//! Docker's on every host.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::BTreeMap;

use super::fluentd::Part;

/// The options the driver takes besides those every driver does: none (moby journald
/// validateLogOpt).
pub const OPTIONS: [&str; 0] = [];

/// Where the journal takes entries (go-systemd's journalSocket; sd_journal_sendv's).
pub const SOCKET: &str = "/run/systemd/journal/socket";

/// The fields the driver writes itself. An attribute named as one is left out, where
/// moby's driver lets it replace the container's own (a label `container.id` its
/// CONTAINER_ID, so an image's labels could say another container wrote its lines) or
/// stand beside the driver's (a second MESSAGE or PRIORITY).
const OWN: [&str; 15] = [
    "PRIORITY",
    "MESSAGE",
    "CONTAINER_ID",
    "CONTAINER_ID_FULL",
    "CONTAINER_NAME",
    "CONTAINER_TAG",
    "IMAGE_NAME",
    "SYSLOG_IDENTIFIER",
    "CONTAINER_LOG_EPOCH",
    "SYSLOG_TIMESTAMP",
    "CONTAINER_PARTIAL_ID",
    "CONTAINER_PARTIAL_ORDINAL",
    "CONTAINER_PARTIAL_LAST",
    "CONTAINER_PARTIAL_MESSAGE",
    "CONTAINER_LOG_ORDINAL",
];

/// What every entry of a run carries besides its line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct From {
    /// The container's own fields, then the attributes asked for, by their fields' names.
    pub fields: Vec<(String, String)>,
    /// The attributes left out: those whose names no journal field's can be, which
    /// journald drops where moby's driver sends them, and those named as a field of the
    /// driver's own.
    pub left_out: Vec<String>,
}

impl From {
    /// Container `id`'s (its full ID) entries: named `name`, made from `image` as its run
    /// named it, its tag `tag`, its log's epoch `epoch` (an ID its driver makes as it
    /// starts, which its ordinals count from), and the attributes asked for, each named as
    /// a journal field (moby's sanitizeKeyMod).
    pub fn new(
        id: &str,
        name: &str,
        image: &str,
        tag: &str,
        epoch: &str,
        attributes: &BTreeMap<String, String>,
    ) -> From {
        let mut fields = vec![
            ("CONTAINER_ID".to_string(), id.get(..12).unwrap_or(id).to_string()),
            ("CONTAINER_ID_FULL".to_string(), id.to_string()),
            (
                "CONTAINER_NAME".to_string(),
                name.trim_start_matches('/').to_string(),
            ),
            ("CONTAINER_TAG".to_string(), tag.to_string()),
            ("IMAGE_NAME".to_string(), image.to_string()),
            ("SYSLOG_IDENTIFIER".to_string(), tag.to_string()),
            ("CONTAINER_LOG_EPOCH".to_string(), epoch.to_string()),
        ];
        let mut named = BTreeMap::new();
        let mut left_out = Vec::new();
        for (k, v) in attributes {
            let field = field_name(k);
            if valid(&field) && !OWN.contains(&field.as_str()) {
                named.insert(field, v.clone());
            } else {
                left_out.push(k.clone());
            }
        }
        fields.extend(named);
        From { fields, left_out }
    }
}

/// An attribute's name as a journal field's, as moby's sanitizeKeyMod makes it: ASCII
/// letters in upper case, digits as they are, any other character `_`, none leading.
pub fn field_name(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for c in key.chars() {
        let c = if c.is_ascii_lowercase() {
            c.to_ascii_uppercase()
        } else if c.is_ascii_uppercase() || c.is_ascii_digit() {
            c
        } else {
            '_'
        };
        if !(out.is_empty() && c == '_') {
            out.push(c);
        }
    }
    out
}

/// Whether journald keeps a field so named, as systemd's journal_field_valid says of one a
/// client sends: 1 to 64 of `A`-`Z`, `0`-`9` and `_`, neither a digit nor `_` first.
pub fn valid(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=64).contains(&b.len())
        && b.first().is_some_and(|c| c.is_ascii_uppercase())
        && b.iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

/// The entry of `line` on `stream`, whose output came at `at` nanoseconds since the epoch,
/// `part` its place in a longer line, `ordinal` its place in the run's log from 1.
pub fn entry(from: &From, stream: u8, at: u64, line: &[u8], part: Option<Part<'_>>, ordinal: u64) -> Vec<u8> {
    let priority: &[u8] = if stream == crate::spec::LOG_STDERR {
        b"3"
    } else {
        b"6"
    };
    let mut out = Vec::with_capacity(line.len() + 512);
    field(&mut out, "PRIORITY", priority);
    field(&mut out, "MESSAGE", line);
    for (k, v) in &from.fields {
        field(&mut out, k, v.as_bytes());
    }
    let time = shards_cmdline::format::rfc3339_nano(u128::from(at));
    field(&mut out, "SYSLOG_TIMESTAMP", time.as_bytes());
    if let Some(p) = part {
        field(&mut out, "CONTAINER_PARTIAL_ID", p.id.as_bytes());
        field(
            &mut out,
            "CONTAINER_PARTIAL_ORDINAL",
            p.ordinal.to_string().as_bytes(),
        );
        field(
            &mut out,
            "CONTAINER_PARTIAL_LAST",
            if p.last { b"true" } else { b"false" },
        );
        if !p.last {
            field(&mut out, "CONTAINER_PARTIAL_MESSAGE", b"true");
        }
    }
    field(&mut out, "CONTAINER_LOG_ORDINAL", ordinal.to_string().as_bytes());
    out
}

/// A field as go-systemd's appendVariable writes it.
fn field(out: &mut Vec<u8>, name: &str, value: &[u8]) {
    out.extend_from_slice(name.as_bytes());
    if value.contains(&b'\n') {
        out.push(b'\n');
        out.extend_from_slice(&(value.len() as u64).to_le_bytes());
    } else {
        out.push(b'=');
    }
    out.extend_from_slice(value);
    out.push(b'\n');
}

/// An entry's fields, in order, as journald reads the native protocol: for the tests and
/// the oracle's.
#[cfg(test)]
pub fn fields(mut entry: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    while !entry.is_empty() {
        let end = entry.iter().position(|&c| c == b'\n' || c == b'=')?;
        let name = String::from_utf8(entry.get(..end)?.to_vec()).ok()?;
        let rest = entry.get(end + 1..)?;
        let (value, rest) = if entry.get(end) == Some(&b'=') {
            let nl = rest.iter().position(|&c| c == b'\n')?;
            (rest.get(..nl)?, rest.get(nl + 1..)?)
        } else {
            let (len, rest) = rest.split_first_chunk::<8>()?;
            let len = usize::try_from(u64::from_le_bytes(*len)).ok()?;
            let value = rest.get(..len)?;
            if rest.get(len) != Some(&b'\n') {
                return None;
            }
            (value, rest.get(len + 1..)?)
        };
        out.push((name, value.to_vec()));
        entry = rest;
    }
    Some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// What Docker 29's journald driver sent, and which of its options Docker took
    /// (scripts/logdrivers/generate).
    fn oracle() -> Value {
        serde_json::from_str(include_str!("testdata/journald.json")).unwrap()
    }

    const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    /// Its first 12 characters: its short ID.
    const SHORT: &str = "0123456789ab";
    const NAME: &str = "oracle";
    const EPOCH: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    const PARTIAL: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    /// An entry's fields as the oracle keeps Docker's: by name, in order within a name;
    /// the container's own values, its epoch, a part's ID and its time placeholders; a run
    /// of one letter its length.
    fn normal(fields: Vec<(String, Vec<u8>)>) -> Vec<Value> {
        let mut out: Vec<Value> = fields
            .into_iter()
            .map(|(field, v)| {
                let v = String::from_utf8(v).unwrap();
                let value = match field.as_str() {
                    "CONTAINER_ID_FULL" => json!("{id}"),
                    _ if v == SHORT => json!("{id12}"),
                    "CONTAINER_LOG_EPOCH" => json!("{epoch}"),
                    "CONTAINER_PARTIAL_ID" => json!("{partial}"),
                    "SYSLOG_TIMESTAMP" => json!("{time}"),
                    _ if v == NAME => json!("{name}"),
                    _ if v.len() > 1 && ["x", "y"].iter().any(|c| v.bytes().all(|b| [b] == c.as_bytes())) => {
                        let letter = if v.starts_with('x') { "x" } else { "y" };
                        let mut run = serde_json::Map::new();
                        run.insert(letter.to_string(), json!(v.len()));
                        Value::Object(run)
                    }
                    _ => json!(v),
                };
                json!([field, value])
            })
            .collect();
        out.sort_by(|a, b| a[0].as_str().cmp(&b[0].as_str()));
        out
    }

    /// Every entry Docker's driver sent is what shards makes of the same line, field for
    /// field, but for the attributes shards leaves out (D166): one journald drops (a digit
    /// first, `1D`), one named as the driver's own MESSAGE, which Docker sends as a second,
    /// and one named as the container's CONTAINER_NAME, which Docker lets replace it.
    #[test]
    fn entries_are_docker_s() {
        for case in oracle()["cases"].as_array().unwrap() {
            let name = case["case"].as_str().unwrap();
            let (tag, attributes): (&str, BTreeMap<String, String>) = match name {
                "streams" => (SHORT, BTreeMap::new()),
                "attributes" => (
                    NAME,
                    [
                        ("a.b", "1"),
                        ("_c", "2"),
                        ("1d", "3"),
                        ("message", "4"),
                        ("container.name", "5"),
                        ("E_F", "6"),
                    ]
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
                ),
                "large" => (
                    SHORT,
                    (1..=9).map(|n| (format!("big{n}"), "y".repeat(60_000))).collect(),
                ),
                other => panic!("a case of the oracle's this test has no attributes for: {other}"),
            };
            let from = From::new(ID, &format!("/{NAME}"), "alpine:3.22", tag, EPOCH, &attributes);
            if name == "attributes" {
                assert_eq!(from.left_out, ["1d", "container.name", "message"]);
            }
            for recorded in case["entries"].as_array().unwrap() {
                let recorded = recorded["fields"].as_array().unwrap();
                let get = |field: &str| -> Vec<&Value> {
                    recorded.iter().filter(|f| f[0] == field).map(|f| &f[1]).collect()
                };
                // The line, its stream, its part and its ordinal, as Docker sent them.
                let line = match get("MESSAGE")[0] {
                    Value::String(s) => s.as_bytes().to_vec(),
                    run => vec![b'x'; usize::try_from(run["x"].as_u64().unwrap()).unwrap()],
                };
                let stream = if get("PRIORITY")[0] == "3" {
                    crate::spec::LOG_STDERR
                } else {
                    crate::spec::LOG_STDOUT
                };
                let part = get("CONTAINER_PARTIAL_ORDINAL").first().map(|ordinal| Part {
                    id: PARTIAL,
                    ordinal: ordinal.as_str().unwrap().parse().unwrap(),
                    last: get("CONTAINER_PARTIAL_LAST")[0] == "true",
                });
                let ordinal = get("CONTAINER_LOG_ORDINAL")[0].as_str().unwrap().parse().unwrap();
                let ours = entry(&from, stream, 1_791_260_014_360_799_172, &line, part, ordinal);
                let docker: Vec<Value> = recorded
                    .iter()
                    .filter(|f| !(name == "attributes" && (f[0] == "1D" || f == &&json!(["MESSAGE", "4"]))))
                    .map(|f| {
                        if name == "attributes" && f[0] == "CONTAINER_NAME" {
                            json!(["CONTAINER_NAME", "{name}"])
                        } else {
                            f.clone()
                        }
                    })
                    .collect();
                assert_eq!(normal(fields(&ours).unwrap()), docker, "{name}");
            }
        }
    }

    /// A field whose value holds a newline goes as its name, its length and its bytes, as
    /// go-systemd's appendVariable writes it; one without, as `NAME=value`.
    #[test]
    fn fields_are_written_as_go_systemd_writes_them() {
        let mut out = Vec::new();
        field(&mut out, "MESSAGE", b"a\nb");
        field(&mut out, "PRIORITY", b"6");
        let mut want = b"MESSAGE\n".to_vec();
        want.extend(3u64.to_le_bytes());
        want.extend(b"a\nb\nPRIORITY=6\n");
        assert_eq!(out, want);
        assert_eq!(
            fields(&out).unwrap(),
            [
                ("MESSAGE".to_string(), b"a\nb".to_vec()),
                ("PRIORITY".to_string(), b"6".to_vec())
            ]
        );
    }

    /// The names journald keeps, as systemd's journal_field_valid says of a client's.
    #[test]
    fn field_names_are_journald_s() {
        assert_eq!(field_name("com.example-app"), "COM_EXAMPLE_APP");
        assert_eq!(field_name("__x"), "X");
        assert_eq!(field_name("é"), "");
        for good in ["A", "A_1", &"B".repeat(64)] {
            assert!(valid(good), "{good}");
        }
        for bad in ["", "1D", "_A", "a", "A-B", &"B".repeat(65)] {
            assert!(!valid(bad), "{bad}");
        }
    }

    /// Which options Docker took, shards takes, where the journal is; elsewhere none.
    #[test]
    fn options_are_taken_as_docker_takes_them() {
        for case in oracle()["options"].as_array().unwrap() {
            let opts: Vec<String> = case["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o.as_str().unwrap().to_string())
                .collect();
            let refused = !case["refused"].is_null() || !cfg!(target_os = "linux");
            let run = shards_ipc::Run {
                log_driver: "journald".into(),
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
