//! The gcplogs driver's options and entries (D169), as moby's gcplogs driver
//! (daemon/logger/gcplogs) takes and makes them for Cloud Logging's WriteLogEntries: each
//! line an entry at its output's time, its JSON payload the run's details and its line,
//! `{"instance":…,"container":…,"message":…}`, written as Go's encoding/json writes moby's
//! dockerLogEntry; the resource the GCE instance it runs on, or the one the options name,
//! else the project's `global`. shards adds to it each line's stream, a label
//! (`stream`), which moby's leaves out, so stdout and stderr cannot be told apart there.

use std::collections::BTreeMap;

/// The options the driver takes besides those every driver does: moby's (ValidateLogOpts),
/// and shards' own log name and endpoint.
pub const OPTIONS: [&str; 7] = [
    "gcp-project",
    "gcp-log-cmd",
    "gcp-meta-zone",
    "gcp-meta-name",
    "gcp-meta-id",
    "gcp-log-name",
    "gcp-endpoint",
];

/// The name a run's entries are logged under, unless `gcp-log-name` says: shards' own.
/// moby's driver logs under `gcplogs-docker-driver`, which `gcp-log-name` gives to keep a
/// query or a sink written for it.
pub const LOG_NAME: &str = "shards";
/// Cloud Logging, unless `gcp-endpoint` names another (a private endpoint, a restricted
/// VIP's).
pub const ENDPOINT: &str = "https://logging.googleapis.com";
/// The scope a token is asked for: writing to Cloud Logging alone (the client's
/// WriteScope).
pub const SCOPE: &str = "https://www.googleapis.com/auth/logging.write";

/// A run's gcplogs driver, as its options say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `gcp-project`: else the instance's, the credentials' or GOOGLE_CLOUD_PROJECT's.
    pub project: Option<String>,
    /// `gcp-log-cmd=true`: the run's command in each entry.
    pub log_cmd: bool,
    /// `gcp-meta-zone`, `gcp-meta-name`, `gcp-meta-id`: the instance off GCE.
    pub zone: Option<String>,
    pub instance_name: Option<String>,
    pub instance_id: Option<String>,
    pub log_name: String,
    pub endpoint: String,
}

/// The driver `opts` asks for, or why not, in shards' words.
pub fn config(opts: &BTreeMap<String, String>) -> Result<Config, String> {
    if opts.contains_key("tag") {
        return Err(
            "--log-opt tag: gcplogs names each entry by its run's details, and by the log it is written to (gcp-log-name); it takes no tag"
                .into(),
        );
    }
    let get = |k: &str| opts.get(k).filter(|v| !v.is_empty()).cloned();
    let log_name = get("gcp-log-name").unwrap_or_else(|| LOG_NAME.to_string());
    // Cloud Logging's log IDs: at most 512 characters of letters, digits, `/`, `_`, `-`
    // and `.` (LogEntry.log_name).
    if log_name.len() > 512
        || !log_name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b))
    {
        return Err(format!(
            "--log-opt gcp-log-name={log_name}: a log's name is 1 to 512 of letters, digits, '/', '_', '-' and '.'"
        ));
    }
    let endpoint = get("gcp-endpoint").unwrap_or_else(|| ENDPOINT.to_string());
    if !(endpoint.starts_with("https://") || endpoint.starts_with("http://")) {
        return Err(format!(
            "--log-opt gcp-endpoint={endpoint}: Cloud Logging's URL, https://HOST[:PORT]"
        ));
    }
    shards_registry::url::Url::parse(&endpoint)
        .map_err(|e| format!("--log-opt gcp-endpoint={endpoint}: {e}"))?;
    let project = get("gcp-project");
    if let Some(p) = &project
        && !project_id(p)
    {
        return Err(format!(
            "--log-opt gcp-project={p}: a project's ID: 6 to 30 lower-case letters, digits and '-', a letter first"
        ));
    }
    Ok(Config {
        project,
        // As moby reads it: `true`, and nothing else, says so.
        log_cmd: opts.get("gcp-log-cmd").is_some_and(|v| v == "true"),
        zone: get("gcp-meta-zone"),
        instance_name: get("gcp-meta-name"),
        instance_id: get("gcp-meta-id"),
        log_name,
        endpoint: endpoint.trim_end_matches('/').to_string(),
    })
}

/// Whether `p` is a project's ID as Google allows one: 6 to 30 characters, lower-case
/// letters, digits and hyphens, a letter first and no hyphen last; or a domain-scoped one,
/// `DOMAIN:ID`.
fn project_id(p: &str) -> bool {
    let id = p.rsplit_once(':').map_or(p, |(_, id)| id);
    (6..=30).contains(&id.len())
        && id.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && !id.ends_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The instance a run's entries say they come from: GCE's, as its metadata server says,
/// or the one the options name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Instance {
    pub zone: String,
    pub name: String,
    pub id: String,
}

/// What every entry of a run carries: its payload's `instance` and `container`, the
/// resource it is logged for, and the log it is written to.
#[derive(Debug, Clone)]
pub struct From {
    /// The payload's members before `message`, as Go writes them.
    pub head: String,
    pub resource: serde_json::Value,
    pub log_name: String,
}

impl From {
    /// A run's: its instance's, where there is one; its details, as moby's containerInfo
    /// holds them (its name as given, `/` first; `created` always; `command` where
    /// `gcp-log-cmd` says; its attributes as `metadata`).
    pub fn new(
        config: &Config,
        project: &str,
        instance: Option<&Instance>,
        details: &super::Details,
        attributes: &BTreeMap<String, String>,
    ) -> From {
        let mut head = String::from("{");
        if let Some(i) = instance {
            head.push_str("\"instance\":{");
            let mut first = true;
            for (k, v) in [("zone", &i.zone), ("name", &i.name), ("id", &i.id)] {
                if !v.is_empty() {
                    if !first {
                        head.push(',');
                    }
                    first = false;
                    head.push_str(&format!("{}:{}", go_string(k), go_string(v)));
                }
            }
            head.push_str("},");
        }
        head.push_str("\"container\":{");
        let mut fields: Vec<String> = Vec::new();
        for (k, v) in [
            ("name", details.name.as_str()),
            ("id", details.id.as_str()),
            ("imageName", details.image_name.as_str()),
            ("imageId", details.image_id.as_str()),
        ] {
            if !v.is_empty() {
                fields.push(format!("{}:{}", go_string(k), go_string(v)));
            }
        }
        let created = if details.created.is_empty() {
            "0001-01-01T00:00:00Z"
        } else {
            details.created.as_str()
        };
        fields.push(format!("\"created\":{}", go_string(created)));
        let command = details.command();
        if config.log_cmd && !command.is_empty() {
            fields.push(format!("\"command\":{}", go_string(&command)));
        }
        if !attributes.is_empty() {
            let pairs: Vec<String> = attributes
                .iter()
                .map(|(k, v)| format!("{}:{}", go_string(k), go_string(v)))
                .collect();
            fields.push(format!("\"metadata\":{{{}}}", pairs.join(",")));
        }
        head.push_str(&fields.join(","));
        head.push('}');
        let resource = match instance {
            Some(i) => serde_json::json!({
                "type": "gce_instance",
                "labels": {"instance_id": i.id, "zone": i.zone},
            }),
            None => serde_json::json!({"type": "global", "labels": {"project_id": project}}),
        };
        From {
            head,
            resource,
            log_name: format!(
                "projects/{project}/logs/{}",
                crate::aws::query_escape(&config.log_name).replace('+', "%20")
            ),
        }
    }

    /// The entry of `line`, from `stream`, made at `at` (nanoseconds since the epoch): its
    /// time, its payload with the line as `message` (none for an empty line, as Go leaves
    /// an empty one out), and its stream as a label.
    pub fn entry(&self, stream: u8, at: u64, line: &[u8]) -> String {
        let mut payload = self.head.clone();
        if !line.is_empty() {
            payload.push_str(",\"message\":");
            payload.push_str(&go_string(&valid_utf8(line)));
        }
        payload.push('}');
        let stream = if stream == crate::spec::LOG_STDERR {
            "stderr"
        } else {
            "stdout"
        };
        format!(
            "{{\"timestamp\":{},\"labels\":{{\"stream\":\"{stream}\"}},\"jsonPayload\":{payload}}}",
            go_string(&shards_cmdline::format::rfc3339_nano(u128::from(at)))
        )
    }

    /// A write of `entries` (entries:write): the run's log and resource, partial success
    /// asked for, so that an entry Cloud Logging refuses holds up none of the others.
    pub fn write(&self, entries: &[String]) -> Vec<u8> {
        format!(
            "{{\"logName\":{},\"resource\":{},\"partialSuccess\":true,\"entries\":[{}]}}",
            go_string(&self.log_name),
            self.resource,
            entries.join(",")
        )
        .into_bytes()
    }
}

/// `line` made valid UTF-8 as Go's encoding/json makes a string: each byte that is not
/// UTF-8 replaced by U+FFFD, one for each.
fn valid_utf8(line: &[u8]) -> String {
    let mut out = String::with_capacity(line.len());
    for chunk in line.utf8_chunks() {
        out.push_str(chunk.valid());
        for _ in chunk.invalid() {
            out.push('\u{fffd}');
        }
    }
    out
}

/// `s` as Go's encoding/json writes a string: HTML's characters and U+2028 and U+2029
/// escaped too.
fn go_string(s: &str) -> String {
    let mut out = Vec::new();
    // compact_escaped writes a JSON value compact, HTML-escaped, as Go's Marshal does.
    let quoted = serde_json::to_vec(s).unwrap_or_default();
    match shards_image::json::compact_escaped(&quoted) {
        Ok(bytes) => out = bytes,
        Err(_) => out.extend_from_slice(&quoted),
    }
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn details() -> crate::logging::Details {
        crate::logging::Details {
            id: "abc".into(),
            name: "/web".into(),
            image_id: "sha256:0123".into(),
            image_name: "alpine:3".into(),
            entrypoint: "sh".into(),
            args: vec!["-c".into(), "echo <&>".into()],
            created: "2026-10-14T12:00:00.5Z".into(),
            ..crate::logging::Details::default()
        }
    }

    /// An entry holds the run's details as moby's dockerLogEntry does, its line as
    /// `message`, its time to the nanosecond, its stream as a label; written as Go writes
    /// it, `<`, `>` and `&` escaped.
    #[test]
    fn entries_hold_the_runs_details_and_its_line() {
        let with_command = config(&opts(&[("gcp-log-cmd", "true")])).unwrap();
        let attributes: BTreeMap<String, String> = [("team".to_string(), "logs".to_string())].into();
        let from = From::new(&with_command, "my-project", None, &details(), &attributes);
        let entry = from.entry(crate::spec::LOG_STDERR, 1_791_260_014_360_799_172, b"a \"b\" <c>");
        assert_eq!(
            entry,
            r#"{"timestamp":"2026-10-06T04:13:34.360799172Z","labels":{"stream":"stderr"},"jsonPayload":{"container":{"name":"/web","id":"abc","imageName":"alpine:3","imageId":"sha256:0123","created":"2026-10-14T12:00:00.5Z","command":"sh -c echo \u003c\u0026\u003e","metadata":{"team":"logs"}},"message":"a \"b\" \u003cc\u003e"}}"#
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&from.write(&[entry])).unwrap()["resource"],
            serde_json::json!({"type": "global", "labels": {"project_id": "my-project"}})
        );
        // On an instance: it, and its resource; an empty line no message.
        let instance = Instance {
            zone: "us-central1-a".into(),
            name: "vm".into(),
            id: "42".into(),
        };
        let from = From::new(
            &config(&opts(&[])).unwrap(),
            "p-123456",
            Some(&instance),
            &details(),
            &BTreeMap::new(),
        );
        let v: serde_json::Value = serde_json::from_str(&from.entry(1, 0, b"")).unwrap();
        assert_eq!(
            v["jsonPayload"]["instance"],
            serde_json::json!({"zone": "us-central1-a", "name": "vm", "id": "42"})
        );
        assert!(v["jsonPayload"].get("message").is_none());
        assert!(v["jsonPayload"]["container"].get("command").is_none());
        let w: serde_json::Value = serde_json::from_slice(&from.write(&[])).unwrap();
        assert_eq!(w["logName"], "projects/p-123456/logs/shards");
        assert_eq!(
            w["resource"],
            serde_json::json!({"type": "gce_instance", "labels": {"instance_id": "42", "zone": "us-central1-a"}})
        );
    }

    /// The options, tag refused (gcplogs has none), shards' own log name, endpoint and
    /// project checked as the run is made.
    #[test]
    fn options_are_checked_as_the_run_is_made() {
        assert!(config(&opts(&[("tag", "x")])).is_err());
        assert!(!config(&opts(&[("gcp-log-cmd", "yes")])).unwrap().log_cmd);
        assert_eq!(
            config(&opts(&[("gcp-log-name", "gcplogs-docker-driver")]))
                .unwrap()
                .log_name,
            "gcplogs-docker-driver"
        );
        for bad in [
            ("gcp-log-name", "a b"),
            ("gcp-endpoint", "logging.googleapis.com"),
            ("gcp-project", "Nope"),
        ] {
            assert!(config(&opts(&[bad])).is_err(), "{bad:?}");
        }
        assert!(config(&opts(&[("gcp-project", "example.com:my-project")])).is_ok());
    }

    /// A byte that is not UTF-8 is a U+FFFD of its own, as Go replaces one.
    #[test]
    fn bytes_not_utf8_are_replaced_one_by_one() {
        assert_eq!(valid_utf8(b"a\xe2\x82b\xff"), "a\u{fffd}\u{fffd}b\u{fffd}");
    }
}
