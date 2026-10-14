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
//!
//! A driver that sends a run's log elsewhere (`syslog`, D163) keeps it in shards' store
//! too, as its local copy, which `cache-max-size`, `cache-max-file` and `cache-compress`
//! bound as Docker documents them (Docker takes them and applies none) and which
//! `cache-disabled` keeps from `logs`; the daemon sends it from there (daemon/ship.rs).

use std::collections::BTreeMap;

use shards_ipc::Run;

pub mod syslog;

/// The drivers that are shards' own store.
const STORE: [&str; 3] = ["shards", "json-file", "local"];
/// The drivers that send a log elsewhere, from shards' store (D163).
const SENDS: [&str; 1] = ["syslog"];
/// The drivers Docker sends a log elsewhere with, which shards does not yet.
const ELSEWHERE: [&str; 7] = [
    "journald", "gelf", "fluentd", "awslogs", "splunk", "etwlogs", "gcplogs",
];
/// The store's own options: its segments' (moby jsonfilelog's ValidateLogOpt, local's).
const STORE_OPTIONS: [&str; 3] = ["max-size", "max-file", "compress"];
/// The options every driver takes: the attributes `logs --details` shows and the tag,
/// the modes (moby factory.go, builtInLogOpts), and the local copy's of a driver that
/// sends its log elsewhere (loggerutils/cache, builtInCacheLogOpts).
const EVERY: [&str; 11] = [
    "labels",
    "labels-regex",
    "env",
    "env-regex",
    "tag",
    "mode",
    "max-buffer-size",
    "cache-disabled",
    "cache-max-size",
    "cache-max-file",
    "cache-compress",
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
    /// Its driver sends it elsewhere, from the store (D163).
    pub ships: bool,
    /// `logs` reads it: not where `cache-disabled` keeps a driver's local copy from it,
    /// which then holds only what is not yet sent.
    pub cached: bool,
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
            "--log-driver {name}: shards keeps a run's log itself (shards, json-file or local), sends it to syslog, or keeps none (none); sending it to {name} is not here yet"
        ));
    }
    let ships = SENDS.contains(&name);
    if !STORE.contains(&name) && !ships {
        return Err(format!(
            "--log-driver {name}: no such log driver: shards keeps a run's log itself (shards, json-file or local), sends it to syslog, or keeps none (none)"
        ));
    }
    let own: &[&str] = if ships { &syslog::OPTIONS } else { &STORE_OPTIONS };
    if let Some(k) = opts
        .keys()
        .find(|k| !own.contains(&k.as_str()) && !EVERY.contains(&k.as_str()))
    {
        let takes: Vec<&str> = own.iter().chain(&EVERY).copied().collect();
        return Err(format!(
            "--log-opt {k}: --log-driver {name} takes {}",
            takes.join(", ")
        ));
    }
    let cached = match opts.get("cache-disabled") {
        Some(v) => match go_bool(v) {
            Some(disabled) => !disabled,
            None => return Err(format!("--log-opt cache-disabled={v}: true or false")),
        },
        None => true,
    };
    if ships {
        syslog::config(&opts)?;
    }
    // The store's bound: a store driver's own options; a driver that sends the log
    // elsewhere, its local copy's.
    let [size_key, files_key, compress_key] = if ships {
        ["cache-max-size", "cache-max-file", "cache-compress"]
    } else {
        ["max-size", "max-file", "compress"]
    };
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
    let size = match opts.get(size_key) {
        Some(v) => match shards_cmdline::resources::from_human_size(v).map(u64::try_from) {
            Ok(Ok(n)) if n > 0 => Some(n),
            _ => {
                return Err(format!(
                    "--log-opt {size_key}={v}: a size, more than 0 bytes (10m, 1g)"
                ));
            }
        },
        None => None,
    };
    let files = match opts.get(files_key) {
        Some(v) => match v.parse::<u64>() {
            Ok(n) if n >= 1 => Some(n),
            _ => {
                return Err(format!("--log-opt {files_key}={v}: how many files, at least 1"));
            }
        },
        None => None,
    };
    let compress = match opts.get(compress_key) {
        Some(v) => match go_bool(v) {
            Some(c) => Some(c),
            None => return Err(format!("--log-opt {compress_key}={v}: true or false")),
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
            "--log-opt {compress_key}=true: its log is one file ({files_key} {files}), never rotated, so nothing of it is compressed: give --log-opt {files_key} 2 or more"
        ));
    }
    // One file is never rotated, so never compressed; nor is a local copy `logs` never
    // reads, whose segments go once sent.
    let compressed = name != "json-file" && files >= 2 && (!ships || cached);
    Ok(Some(Kept {
        size: size.unwrap_or(default.size),
        files,
        compress: compress.unwrap_or(compressed),
        queue,
        ships,
        cached: !ships || cached,
    }))
}

/// Where a run's log is sent besides shards' store (D163).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remote {
    Syslog(syslog::Config),
}

/// The driver `run` sends its log elsewhere with, if one does.
pub fn remote(run: &Run) -> Result<Option<Remote>, String> {
    match driver(run) {
        "syslog" => Ok(Some(Remote::Syslog(syslog::config(&options(run))?))),
        _ => Ok(None),
    }
}

/// `run`'s tag (`--log-opt tag`): its template run against `info`, or, where it gives
/// none, the container's short ID (moby loggerutils.ParseLogTag, DefaultTemplate).
pub fn tag(run: &Run, info: &Info<'_>) -> Result<String, String> {
    let opts = options(run);
    let text = opts
        .get("tag")
        .filter(|t| !t.is_empty())
        .map_or("{{.ID}}", String::as_str);
    render_tag(text, info, &opts)
}

/// A boolean as Go's strconv.ParseBool reads it.
fn go_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// What a run's container is, owned, as `inspect` says it: for its log's tag and
/// attributes ([`Info`]).
#[derive(Debug, Clone, Default)]
pub struct Details {
    pub id: String,
    pub name: String,
    pub image_id: String,
    pub image_name: String,
    pub entrypoint: String,
    pub args: Vec<String>,
    pub env: Vec<String>,
    pub labels: BTreeMap<String, String>,
    pub hostname: String,
}

impl Details {
    pub fn info(&self) -> Info<'_> {
        Info {
            id: &self.id,
            name: &self.name,
            image_id: &self.image_id,
            image_name: &self.image_name,
            entrypoint: &self.entrypoint,
            args: &self.args,
            env: &self.env,
            labels: &self.labels,
            hostname: &self.hostname,
        }
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
            ships: false,
            cached: true,
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
        // A driver that sends its log elsewhere keeps its local copy as its cache options
        // say, which Docker takes and applies none of; every driver takes them.
        let shipped = |size, files, compress, cached| Kept {
            ships: true,
            cached,
            ..kept_as(size, files, compress, None)
        };
        assert_eq!(kept("syslog", &[]), Ok(Some(shipped(20 << 20, 5, true, true))));
        assert_eq!(
            kept(
                "syslog",
                &["cache-max-size=1m", "cache-max-file=2", "cache-compress=false"]
            ),
            Ok(Some(shipped(1_000_000, 2, false, true)))
        );
        // Kept from `logs`, its segments go once sent: none compressed.
        assert_eq!(
            kept("syslog", &["cache-disabled=true"]),
            Ok(Some(shipped(20 << 20, 5, false, false)))
        );
        assert_eq!(
            kept("json-file", &["cache-max-size=1m", "cache-disabled=true"]),
            Ok(Some(kept_as(20 << 20, 5, false, None)))
        );
        for (driver, opts, said) in [
            (
                "none",
                &["max-size=1m"][..],
                "--log-opt max-size: --log-driver none keeps no log",
            ),
            ("gelf", &[], "--log-driver gelf: shards keeps a run's log itself"),
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
                "--log-opt color: --log-driver json-file takes max-size",
            ),
            (
                "syslog",
                &["max-size=10m"],
                "--log-opt max-size: --log-driver syslog takes syslog-address",
            ),
            (
                "syslog",
                &["cache-max-file=0"],
                "--log-opt cache-max-file=0: how many files, at least 1",
            ),
            (
                "syslog",
                &["cache-disabled=maybe"],
                "--log-opt cache-disabled=maybe: true or false",
            ),
            (
                "syslog",
                &["syslog-tls-skip-verify=true"],
                "--log-opt syslog-tls-skip-verify: shards sends a log only to a syslog server whose certificate it verifies",
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
