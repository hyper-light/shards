//! An image's config as BuildKit holds it while it plans a build: moby's
//! `DockerOCIImage` (docker-image-spec v1.3.1), read as Go's `encoding/json` reads it, as
//! Docker's daemon reads it to run one (`shards_image::config`, one reading for both), and
//! written as `json.Marshal` writes it, so the config BuildKit would write, and its digest,
//! come out byte for byte.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::go::Time;
use crate::json;
use crate::platform::Platform;

/// `HealthcheckConfig`. Durations are nanoseconds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Healthcheck {
    pub test: Vec<Vec<u8>>,
    pub interval: i64,
    pub timeout: i64,
    pub start_period: i64,
    pub start_interval: i64,
    pub retries: i64,
}

/// `DockerOCIImageConfig`: OCI's `ImageConfig` and Docker's additions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub user: Vec<u8>,
    pub exposed_ports: BTreeMap<Vec<u8>, ()>,
    pub env: Vec<Vec<u8>>,
    pub entrypoint: Vec<Vec<u8>>,
    pub cmd: Vec<Vec<u8>>,
    pub volumes: BTreeMap<Vec<u8>, ()>,
    pub working_dir: Vec<u8>,
    pub labels: BTreeMap<Vec<u8>, Vec<u8>>,
    pub stop_signal: Vec<u8>,
    pub args_escaped: bool,
    pub healthcheck: Option<Healthcheck>,
    pub on_build: Vec<Vec<u8>>,
    pub shell: Vec<Vec<u8>>,
}

/// `RootFS`. `diff_ids` is written as `null` when absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootFs {
    pub kind: Vec<u8>,
    pub diff_ids: Option<Vec<Vec<u8>>>,
}

/// `History`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    pub created: Option<Time>,
    pub created_by: Vec<u8>,
    pub author: Vec<u8>,
    pub comment: Vec<u8>,
    pub empty_layer: bool,
}

/// `DockerOCIImage`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Image {
    pub created: Option<Time>,
    pub author: Vec<u8>,
    pub platform: Platform,
    pub config: Config,
    pub rootfs: RootFs,
    pub history: Vec<History>,
}

impl Image {
    /// An image config's JSON, read as `json.Unmarshal` reads it into a `DockerOCIImage`
    /// (`shards_image::config`). Fails with Go's message.
    pub fn from_json(text: &[u8]) -> Result<Image, Vec<u8>> {
        let read = shards_image::config::decode(text).map_err(String::into_bytes)?;
        Ok(Image::from(read))
    }

    /// The config as `json.Marshal` writes a `DockerOCIImage`: fields in Go's order, empty
    /// ones left out where Go leaves them out, maps sorted by key.
    pub fn to_json(&self) -> Result<String, Vec<u8>> {
        let mut o = Obj::new();
        for (k, v) in self.members()? {
            o.raw(k, &v);
        }
        Ok(o.end())
    }

    /// The config's members in Go's order, each value as `json.Marshal` writes it.
    pub fn members(&self) -> Result<Vec<(&'static str, String)>, Vec<u8>> {
        let mut m: Vec<(&'static str, String)> = Vec::new();
        let string = |s: &[u8]| {
            let mut out = String::new();
            json::write_string(&mut out, s);
            out
        };
        if let Some(t) = &self.created {
            m.push(("created", time_json(t)?));
        }
        if !self.author.is_empty() {
            m.push(("author", string(&self.author)));
        }
        let p = &self.platform;
        m.push(("architecture", string(&p.architecture)));
        m.push(("os", string(&p.os)));
        if !p.os_version.is_empty() {
            m.push(("os.version", string(&p.os_version)));
        }
        if !p.os_features.is_empty() {
            let mut out = String::new();
            json::write_strings(&mut out, &p.os_features);
            m.push(("os.features", out));
        }
        if !p.variant.is_empty() {
            m.push(("variant", string(&p.variant)));
        }
        m.push((
            "rootfs",
            rootfs_json(&self.rootfs.kind, self.rootfs.diff_ids.as_deref()),
        ));
        if !self.history.is_empty() {
            m.push(("history", history_json(&self.history)?));
        }
        m.push(("config", self.config.to_json()));
        Ok(m)
    }
}

/// The planner's view of a config Go read: its strings Go strings, bytes, and its nil
/// slices and maps empty, as `json.Marshal` leaves both out alike.
impl From<shards_image::config::Image> for Image {
    fn from(i: shards_image::config::Image) -> Image {
        let bytes = |s: String| s.into_bytes();
        let list = |v: Option<Vec<String>>| v.into_iter().flatten().map(String::into_bytes).collect();
        let c = i.config;
        Image {
            created: i.created,
            author: bytes(i.author),
            platform: Platform {
                os: bytes(i.os),
                architecture: bytes(i.architecture),
                variant: bytes(i.variant),
                os_version: bytes(i.os_version),
                os_features: list(i.os_features),
            },
            config: Config {
                user: bytes(c.user),
                exposed_ports: c
                    .exposed_ports
                    .into_iter()
                    .flatten()
                    .map(|p| (p.into_bytes(), ()))
                    .collect(),
                env: list(c.env),
                entrypoint: list(c.entrypoint),
                cmd: list(c.cmd),
                volumes: c
                    .volumes
                    .into_iter()
                    .flatten()
                    .map(|v| (v.into_bytes(), ()))
                    .collect(),
                working_dir: bytes(c.working_dir),
                labels: c
                    .labels
                    .into_iter()
                    .flatten()
                    .map(|(k, v)| (k.into_bytes(), v.into_bytes()))
                    .collect(),
                stop_signal: bytes(c.stop_signal),
                args_escaped: c.args_escaped,
                healthcheck: c.healthcheck.map(|h| Healthcheck {
                    test: list(h.test),
                    interval: h.interval,
                    timeout: h.timeout,
                    start_period: h.start_period,
                    start_interval: h.start_interval,
                    retries: h.retries,
                }),
                on_build: list(c.on_build),
                shell: list(c.shell),
            },
            rootfs: RootFs {
                kind: bytes(i.rootfs.kind),
                diff_ids: i
                    .rootfs
                    .diff_ids
                    .map(|ids| ids.into_iter().map(String::into_bytes).collect()),
            },
            history: i
                .history
                .into_iter()
                .flatten()
                .map(|h| History {
                    created: h.created,
                    created_by: bytes(h.created_by),
                    author: bytes(h.author),
                    comment: bytes(h.comment),
                    empty_layer: h.empty_layer,
                })
                .collect(),
        }
    }
}

/// `s` as `json.Marshal` writes a string: HTML-escaped, invalid UTF-8 as U+FFFD.
pub fn json_string(s: &[u8]) -> String {
    let mut out = String::new();
    json::write_string(&mut out, s);
    out
}

/// `ocispec.RootFS`: `diff_ids` is `null` when there are none.
pub fn rootfs_json(kind: &[u8], diff_ids: Option<&[Vec<u8>]>) -> String {
    let mut rootfs = Obj::new();
    rootfs.string("type", kind);
    match diff_ids {
        Some(ids) => rootfs.strings("diff_ids", ids),
        None => rootfs.raw("diff_ids", "null"),
    }
    rootfs.end()
}

/// A `[]ocispec.History`: `null` when there is none.
pub fn history_json(history: &[History]) -> Result<String, Vec<u8>> {
    if history.is_empty() {
        return Ok("null".to_string());
    }
    let mut list = String::from("[");
    for (i, h) in history.iter().enumerate() {
        if i > 0 {
            list.push(',');
        }
        let mut e = Obj::new();
        if let Some(t) = &h.created {
            e.raw("created", &time_json(t)?);
        }
        e.string_nonempty("created_by", &h.created_by);
        e.string_nonempty("author", &h.author);
        e.string_nonempty("comment", &h.comment);
        if h.empty_layer {
            e.raw("empty_layer", "true");
        }
        list.push_str(&e.end());
    }
    list.push(']');
    Ok(list)
}

impl Config {
    /// `DockerOCIImageConfig` as `json.Marshal` writes it.
    pub fn to_json(&self) -> String {
        let mut o = Obj::new();
        o.string_nonempty("User", &self.user);
        o.set_nonempty("ExposedPorts", &self.exposed_ports);
        o.strings_nonempty("Env", &self.env);
        o.strings_nonempty("Entrypoint", &self.entrypoint);
        o.strings_nonempty("Cmd", &self.cmd);
        o.set_nonempty("Volumes", &self.volumes);
        o.string_nonempty("WorkingDir", &self.working_dir);
        if !self.labels.is_empty() {
            let mut m = Obj::new();
            for (k, v) in &self.labels {
                m.key_bytes(k);
                json::write_string(&mut m.out, v);
            }
            o.raw("Labels", &m.end());
        }
        o.string_nonempty("StopSignal", &self.stop_signal);
        if self.args_escaped {
            o.raw("ArgsEscaped", "true");
        }
        if let Some(h) = &self.healthcheck {
            let mut e = Obj::new();
            e.strings_nonempty("Test", &h.test);
            for (name, n) in [
                ("Interval", h.interval),
                ("Timeout", h.timeout),
                ("StartPeriod", h.start_period),
                ("StartInterval", h.start_interval),
                ("Retries", h.retries),
            ] {
                if n != 0 {
                    e.raw(name, &n.to_string());
                }
            }
            o.raw("Healthcheck", &e.end());
        }
        o.strings_nonempty("OnBuild", &self.on_build);
        o.strings_nonempty("Shell", &self.shell);
        o.end()
    }
}

pub fn time_json(t: &Time) -> Result<String, Vec<u8>> {
    let text = t.rfc3339_nano().map_err(|e| {
        [
            b"json: error calling MarshalJSON for type *time.Time: ".as_slice(),
            &e,
        ]
        .concat()
    })?;
    Ok(format!("\"{text}\""))
}

/// An object being written.
struct Obj {
    out: String,
    first: bool,
}

impl Obj {
    fn new() -> Obj {
        Obj {
            out: String::from("{"),
            first: true,
        }
    }

    fn key(&mut self, k: &str) {
        self.key_bytes(k.as_bytes());
    }

    fn key_bytes(&mut self, k: &[u8]) {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
        json::write_string(&mut self.out, k);
        self.out.push(':');
    }

    fn raw(&mut self, k: &str, v: &str) {
        self.key(k);
        self.out.push_str(v);
    }

    fn string(&mut self, k: &str, v: &[u8]) {
        self.key(k);
        json::write_string(&mut self.out, v);
    }

    fn string_nonempty(&mut self, k: &str, v: &[u8]) {
        if !v.is_empty() {
            self.string(k, v);
        }
    }

    fn strings(&mut self, k: &str, v: &[Vec<u8>]) {
        self.key(k);
        json::write_strings(&mut self.out, v);
    }

    fn strings_nonempty(&mut self, k: &str, v: &[Vec<u8>]) {
        if !v.is_empty() {
            self.strings(k, v);
        }
    }

    fn set_nonempty(&mut self, k: &str, v: &BTreeMap<Vec<u8>, ()>) {
        if v.is_empty() {
            return;
        }
        let mut m = Obj::new();
        for key in v.keys() {
            m.key_bytes(key);
            m.out.push_str("{}");
        }
        self.raw(k, &m.end());
    }

    fn end(mut self) -> String {
        let _ = write!(self.out, "}}");
        self.out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A repeated key's array decoded over what the first left, as Go's decoder reuses a
    /// slice's backing array (shards_image's testdata/image-config.json, from Go), so the
    /// config BuildKit writes from a base image is the one Go read.
    #[test]
    fn a_repeated_array_reuses_what_the_first_left() {
        let img = Image::from_json(br#"{"config":{"Env":["A","B"],"Env":[null]}}"#).unwrap();
        assert_eq!(img.config.env, vec![b"A".to_vec()]);
        let img = Image::from_json(
            br#"{"history":[{"created_by":"a","comment":"c"}],"history":[{"created_by":"b"}]}"#,
        )
        .unwrap();
        assert_eq!(
            (
                img.history[0].created_by.as_slice(),
                img.history[0].comment.as_slice()
            ),
            (&b"b"[..], &b"c"[..])
        );
    }
}
