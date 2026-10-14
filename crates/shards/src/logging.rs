//! A run's log (D161): `--log-driver` and `--log-opt`, as docker/cli sends them
//! (HostConfig.LogConfig), on shards' own store. `shards`, the default, `json-file` and
//! `local` are the store, its segments as long and as many as the options say; `none`
//! keeps no log. Docker's own default keeps one file that grows without bound (json-file
//! with neither `max-size` nor `max-file`); shards' keeps its daemon's bound
//! (`SHARDS_LOG_MAX_SIZE`, `SHARDS_LOG_MAX_FILE`) where the options give none.
//!
//! Rotated segments are compressed (D162), seekably, unless a run's options say not to,
//! or its driver is json-file, which keeps them as written unless told to compress, as
//! Docker's does. In `mode=non-blocking` the run's output is queued for its log, at most
//! `max-buffer-size` bytes, and what comes while the queue is full is dropped and counted.

use std::collections::BTreeMap;

use shards_ipc::Run;

/// The drivers that are shards' own store.
const STORE: [&str; 3] = ["shards", "json-file", "local"];
/// The drivers Docker sends a log elsewhere with, which shards does not yet.
const ELSEWHERE: [&str; 8] = [
    "syslog", "journald", "gelf", "fluentd", "awslogs", "splunk", "etwlogs", "gcplogs",
];
/// The options the store takes: its segments', and the attributes `logs --details`
/// shows (moby daemon/logger: jsonfilelog's ValidateLogOpt, local's, and the modes every
/// driver takes, factory.go builtInLogOpts).
const OPTIONS: [&str; 10] = [
    "max-size",
    "max-file",
    "compress",
    "mode",
    "max-buffer-size",
    "labels",
    "labels-regex",
    "env",
    "env-regex",
    "tag",
];

/// What a run's log keeps, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kept {
    /// Each segment's most bytes.
    pub size: u64,
    /// How many segments it keeps, the newest written.
    pub files: u64,
    /// Its segments are compressed once rotated (D162).
    pub compress: bool,
    /// Non-blocking (`mode=non-blocking`): the most bytes of output queued for the log,
    /// past which more is dropped; none where the run waits for its log.
    pub queue: Option<u64>,
}

/// The queue of a non-blocking log whose options give no `max-buffer-size`: moby's ring
/// logger's (daemon/logger/ring.go, defaultRingMaxSize), what the option means unsaid.
pub const QUEUE: u64 = 1_000_000;

/// The driver a run's log is kept by, as `inspect` says it: as given, else shards' own.
pub fn driver(run: &Run) -> &str {
    if run.log_driver.is_empty() {
        "shards"
    } else {
        &run.log_driver
    }
}

/// `--log-opt`'s, as docker/cli makes them a map (opts.ConvertKVStringsToMap): `KEY=VALUE`,
/// or `KEY` alone for an empty value; the last of a key's.
pub fn options(run: &Run) -> BTreeMap<String, String> {
    run.log_opts
        .iter()
        .map(|o| match o.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (o.clone(), String::new()),
        })
        .collect()
}

/// What `run`'s log keeps, the daemon's bound, `default`, where its options say nothing;
/// none for `none`. Refused in shards' words where its driver or its options are none it
/// takes.
pub fn kept(run: &Run, default: crate::spec::LogRetention) -> Result<Option<Kept>, String> {
    let name = driver(run);
    let opts = options(run);
    if name == "none" {
        return match opts.keys().next() {
            Some(k) => Err(format!(
                "--log-opt {k}: --log-driver none keeps no log, which takes no options"
            )),
            None => Ok(None),
        };
    }
    if ELSEWHERE.contains(&name) {
        return Err(format!(
            "--log-driver {name}: shards keeps a run's log itself (shards, json-file or local) or none (none); sending it to {name} is not here yet"
        ));
    }
    if !STORE.contains(&name) {
        return Err(format!(
            "--log-driver {name}: no such log driver: shards keeps a run's log itself (shards, json-file or local) or none (none)"
        ));
    }
    if let Some(k) = opts.keys().find(|k| !OPTIONS.contains(&k.as_str())) {
        return Err(format!("--log-opt {k}: a run's log takes {}", OPTIONS.join(", ")));
    }
    let queue = match opts.get("mode").map(String::as_str) {
        None | Some("" | "blocking") => {
            if opts.contains_key("max-buffer-size") {
                return Err(
                    "--log-opt max-buffer-size: a buffer is non-blocking mode's (mode=non-blocking)".into(),
                );
            }
            None
        }
        // As moby reads it (ValidateLogOpts): go-units' RAMInBytes, binary units.
        Some("non-blocking") => Some(match opts.get("max-buffer-size") {
            None => QUEUE,
            Some(v) => match shards_cmdline::resources::ram_in_bytes(v).map(u64::try_from) {
                Ok(Ok(n)) => n,
                _ => return Err(format!("--log-opt max-buffer-size={v}: a size (1m, 512k)")),
            },
        }),
        Some(m) => return Err(format!("--log-opt mode={m}: blocking or non-blocking")),
    };
    let size = match opts.get("max-size") {
        Some(v) => match shards_cmdline::resources::from_human_size(v).map(u64::try_from) {
            Ok(Ok(n)) if n > 0 => Some(n),
            _ => {
                return Err(format!(
                    "--log-opt max-size={v}: a size, more than 0 bytes (10m, 1g)"
                ));
            }
        },
        None => None,
    };
    let files = match opts.get("max-file") {
        Some(v) => match v.parse::<u64>() {
            Ok(n) if n >= 1 => Some(n),
            _ => return Err(format!("--log-opt max-file={v}: how many files, at least 1")),
        },
        None => None,
    };
    let compress = match opts.get("compress") {
        Some(v) => match go_bool(v) {
            Some(c) => Some(c),
            None => return Err(format!("--log-opt compress={v}: true or false")),
        },
        None => None,
    };
    for k in ["labels-regex", "env-regex"] {
        if let Some(v) = opts.get(k)
            && let Err(e) = regex::Regex::new(v)
        {
            return Err(format!("--log-opt {k}={v}: {e}"));
        }
    }
    // json-file keeps one file where only its size is given (moby jsonfilelog.New); every
    // driver of the store keeps the daemon's bound where nothing is given.
    let files = files.unwrap_or(if name == "json-file" && size.is_some() {
        1
    } else {
        default.files
    });
    if compress == Some(true) && files < 2 {
        return Err(format!(
            "--log-opt compress=true: its log is one file (max-file {files}), never rotated, so nothing of it is compressed: give --log-opt max-file 2 or more"
        ));
    }
    Ok(Some(Kept {
        size: size.unwrap_or(default.size),
        files,
        // One file is never rotated, so never compressed.
        compress: compress.unwrap_or(name != "json-file" && files >= 2),
        queue,
    }))
}

/// A boolean as Go's strconv.ParseBool reads it.
fn go_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// What a run's container is, for its log's tag and attributes.
pub struct Info<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub image_id: &'a str,
    pub image_name: &'a str,
    /// Its entrypoint, then its command's arguments.
    pub entrypoint: &'a str,
    pub args: &'a [String],
    pub env: &'a [String],
    pub labels: &'a BTreeMap<String, String>,
    /// The host's name, which a tag's `{{.Hostname}}` is.
    pub hostname: &'a str,
}

/// The attributes `logs --details` shows before each of `run`'s lines, sorted by name as
/// Go's encoding of a map sorts them: its labels' (`labels`, `labels-regex`), its
/// environment's (`env`, `env-regex`) and its tag (`tag`), as moby's json-file keeps them
/// (logger.Info.ExtraAttributes, loggerutils.ParseLogTag).
pub fn attributes(run: &Run, info: &Info<'_>) -> Result<BTreeMap<String, String>, String> {
    let opts = options(run);
    let mut extra = BTreeMap::new();
    if let Some(keys) = opts.get("labels").filter(|v| !v.is_empty()) {
        for k in keys.split(',') {
            if let Some(v) = info.labels.get(k) {
                extra.insert(k.to_string(), v.clone());
            }
        }
    }
    if let Some(re) = opts.get("labels-regex").filter(|v| !v.is_empty()) {
        let re = regex::Regex::new(re).map_err(|e| e.to_string())?;
        for (k, v) in info.labels {
            if re.is_match(k) {
                extra.insert(k.clone(), v.clone());
            }
        }
    }
    let env: BTreeMap<&str, &str> = info.env.iter().filter_map(|e| e.split_once('=')).collect();
    if let Some(keys) = opts.get("env").filter(|v| !v.is_empty()) {
        for k in keys.split(',') {
            if let Some(v) = env.get(k) {
                extra.insert(k.to_string(), (*v).to_string());
            }
        }
    }
    if let Some(re) = opts.get("env-regex").filter(|v| !v.is_empty()) {
        let re = regex::Regex::new(re).map_err(|e| e.to_string())?;
        for (k, v) in &env {
            if re.is_match(k) {
                extra.insert((*k).to_string(), (*v).to_string());
            }
        }
    }
    if let Some(tag) = opts.get("tag").filter(|v| !v.is_empty()) {
        let tag = render_tag(tag, info, &opts)?;
        if !tag.is_empty() {
            extra.insert("tag".into(), tag);
        }
    }
    Ok(extra)
}

/// `tag`'s template, run against the container's details (moby loggerutils.ParseLogTag).
fn render_tag(text: &str, info: &Info<'_>, opts: &BTreeMap<String, String>) -> Result<String, String> {
    let t = shards_template::Template::parse("log-tag", text)?;
    let data = shards_template::Value::object(TagData {
        info: Owned::of(info),
        config: opts.clone(),
    });
    t.execute(&data)
}

/// A container's details, owned for a template's object.
#[derive(Debug, Clone)]
struct Owned {
    id: String,
    name: String,
    image_id: String,
    image_name: String,
    entrypoint: String,
    args: Vec<String>,
    env: Vec<String>,
    labels: BTreeMap<String, String>,
    hostname: String,
}

impl Owned {
    fn of(info: &Info<'_>) -> Owned {
        Owned {
            id: info.id.to_string(),
            name: info.name.trim_start_matches('/').to_string(),
            image_id: info.image_id.to_string(),
            image_name: info.image_name.to_string(),
            entrypoint: info.entrypoint.to_string(),
            args: info.args.to_vec(),
            env: info.env.to_vec(),
            labels: info.labels.clone(),
            hostname: info.hostname.to_string(),
        }
    }
}

#[derive(Debug)]
struct TagData {
    info: Owned,
    config: BTreeMap<String, String>,
}

/// A digest or ID cut to its first 12 hex digits (moby stringid.TruncateID).
fn short(id: &str) -> String {
    let id = id.split_once(':').map_or(id, |(_, hex)| hex);
    id.get(..12).unwrap_or(id).to_string()
}

impl shards_template::Object for TagData {
    fn type_name(&self) -> &str {
        "logger.Info"
    }

    fn field(&self, name: &str) -> Option<shards_template::Value> {
        use shards_template::Value;
        let i = &self.info;
        let strings = |v: &[String]| Value::strings(v.iter().cloned());
        Some(match name {
            "ID" => Value::String(short(&i.id)),
            "FullID" | "ContainerID" => Value::String(i.id.clone()),
            "Name" => Value::String(i.name.clone()),
            "ContainerName" => Value::String(format!("/{}", i.name)),
            "ImageID" => Value::String(short(&i.image_id)),
            "ImageFullID" | "ContainerImageID" => Value::String(i.image_id.clone()),
            "ImageName" | "ContainerImageName" => Value::String(i.image_name.clone()),
            "Command" => Value::String(
                std::iter::once(i.entrypoint.as_str())
                    .chain(i.args.iter().map(String::as_str))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            "ContainerEntrypoint" => Value::String(i.entrypoint.clone()),
            "ContainerArgs" => strings(&i.args),
            "ContainerEnv" => strings(&i.env),
            "ContainerLabels" => Value::string_map(i.labels.clone()),
            "Config" => Value::string_map(self.config.clone()),
            "Hostname" => Value::String(i.hostname.clone()),
            "DaemonName" => Value::String("shards".into()),
            _ => return None,
        })
    }

    fn format(&self, out: &mut String) {
        out.push_str(&format!("{{{} {}}}", self.info.id, self.info.name));
    }

    fn json(&self, _out: &mut String) -> Result<(), String> {
        Err(format!("cannot encode {} as JSON", self.type_name()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(driver: &str, opts: &[&str]) -> Run {
        Run {
            log_driver: driver.into(),
            log_opts: opts.iter().map(|o| (*o).to_string()).collect(),
            ..Run::default()
        }
    }

    const DEFAULT: crate::spec::LogRetention = crate::spec::LogRetention {
        size: 20 << 20,
        files: 5,
    };

    const fn kept_as(size: u64, files: u64, compress: bool, queue: Option<u64>) -> Kept {
        Kept {
            size,
            files,
            compress,
            queue,
        }
    }

    #[test]
    fn a_runs_log_is_kept_as_its_options_say() {
        let kept = |driver: &str, opts: &[&str]| kept(&run(driver, opts), DEFAULT);
        // Compressed once rotated but where json-file is asked for, as Docker's is.
        assert_eq!(kept("", &[]), Ok(Some(kept_as(20 << 20, 5, true, None))));
        assert_eq!(kept("none", &[]), Ok(None));
        // Decimal units, as go-units' FromHumanSize reads them.
        assert_eq!(
            kept("json-file", &["max-size=10m", "max-file=3"]),
            Ok(Some(kept_as(10_000_000, 3, false, None)))
        );
        // json-file keeps one file where only its size is given; the others, the bound.
        assert_eq!(
            kept("json-file", &["max-size=1k"]),
            Ok(Some(kept_as(1000, 1, false, None)))
        );
        assert_eq!(
            kept("local", &["max-size=1k"]),
            Ok(Some(kept_as(1000, 5, true, None)))
        );
        assert_eq!(
            kept(
                "shards",
                &["max-file=2", "compress=false", "mode=blocking", "tag={{.Name}}"]
            ),
            Ok(Some(kept_as(20 << 20, 2, false, None)))
        );
        // json-file without max-size is kept to the bound, so it may be compressed, which
        // Docker refuses for want of a bound.
        assert_eq!(
            kept("json-file", &["compress=true"]),
            Ok(Some(kept_as(20 << 20, 5, true, None)))
        );
        // A single file is never compressed, and not refused unless asked to be.
        assert_eq!(
            kept("local", &["max-file=1"]),
            Ok(Some(kept_as(20 << 20, 1, false, None)))
        );
        // Non-blocking: moby's ring's size unless given, in binary units, 0 too.
        assert_eq!(
            kept("", &["mode=non-blocking"]),
            Ok(Some(kept_as(20 << 20, 5, true, Some(1_000_000))))
        );
        assert_eq!(
            kept("json-file", &["mode=non-blocking", "max-buffer-size=4m"]),
            Ok(Some(kept_as(20 << 20, 5, false, Some(4 << 20))))
        );
        assert_eq!(
            kept("local", &["mode=non-blocking", "max-buffer-size=0"]),
            Ok(Some(kept_as(20 << 20, 5, true, Some(0))))
        );
        for (driver, opts, said) in [
            (
                "none",
                &["max-size=1m"][..],
                "--log-opt max-size: --log-driver none keeps no log",
            ),
            (
                "syslog",
                &[],
                "--log-driver syslog: shards keeps a run's log itself",
            ),
            ("bogus", &[], "--log-driver bogus: no such log driver"),
            ("json-file", &["max-size=-1"], "--log-opt max-size=-1: a size"),
            (
                "json-file",
                &["max-size=0"],
                "--log-opt max-size=0: a size, more than 0 bytes",
            ),
            (
                "local",
                &["max-file=0"],
                "--log-opt max-file=0: how many files, at least 1",
            ),
            (
                "json-file",
                &["compress=maybe"],
                "--log-opt compress=maybe: true or false",
            ),
            (
                "json-file",
                &["max-size=1m", "compress=true"],
                "--log-opt compress=true: its log is one file (max-file 1)",
            ),
            (
                "local",
                &["max-file=1", "compress=1"],
                "--log-opt compress=true: its log is one file (max-file 1)",
            ),
            (
                "local",
                &["mode=non-blocking", "max-buffer-size=lots"],
                "--log-opt max-buffer-size=lots: a size",
            ),
            (
                "json-file",
                &["mode=fast"],
                "--log-opt mode=fast: blocking or non-blocking",
            ),
            (
                "json-file",
                &["max-buffer-size=4m"],
                "--log-opt max-buffer-size: a buffer",
            ),
            ("json-file", &["labels-regex=("], "--log-opt labels-regex=(: "),
            (
                "json-file",
                &["color=red"],
                "--log-opt color: a run's log takes max-size",
            ),
        ] {
            let got = kept(driver, opts).unwrap_err();
            assert!(got.starts_with(said), "{driver} {opts:?}: {got}");
        }
    }

    #[test]
    fn details_hold_the_labels_environment_and_tag_asked_for() {
        let labels: BTreeMap<String, String> = [("app", "web"), ("tier", "front"), ("x.y", "z")]
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        let env = vec!["A=1".to_string(), "B=2".to_string(), "PATH=/bin".to_string()];
        let args = vec!["-c".to_string(), "x".to_string()];
        let info = Info {
            id: "0123456789abcdef0123",
            name: "/web",
            image_id: "sha256:fedcba9876543210",
            image_name: "alpine:3",
            entrypoint: "sh",
            args: &args,
            env: &env,
            labels: &labels,
            hostname: "host",
        };
        let attrs = |opts: &[&str]| attributes(&run("", opts), &info).unwrap();
        assert_eq!(
            attrs(&[
                "labels=app,missing",
                "env=B",
                "env-regex=^A$",
                "labels-regex=^x\\."
            ]),
            [("A", "1"), ("B", "2"), ("app", "web"), ("x.y", "z")]
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect()
        );
        assert_eq!(
            attrs(&["tag={{.Name}}/{{.ID}}/{{.ImageID}}/{{.ImageName}}/{{.Command}}"])["tag"],
            "web/0123456789ab/fedcba987654/alpine:3/sh -c x"
        );
        assert!(attrs(&["tag="]).is_empty());
    }
}
