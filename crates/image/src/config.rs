//! An image's config as Docker reads it: `json.Unmarshal` into the Go type each of its
//! readers uses (moby at the commit docker/cli v29.8.1 goes with, on its containerd image
//! store, Docker 29's own):
//! - `dockerspec.DockerOCIImage` to run, create or inspect an image (daemon/containerd
//!   image.go `GetImage`, image_inspect.go), as BuildKit plans a build from a base image;
//! - `ocispec.Image`, whose rootfs containerd's unpack reads when an image is pulled
//!   (core/images `RootFS`);
//! - image_history.go's struct of the rootfs and history alone, for `docker history`.
//!
//! Each of the three is a part of `DockerOCIImage`, read alike, so a config is read once:
//! every type mismatch and every time that fails to parse is kept with the field it falls
//! in, in document order, and each type's answer is the first of those among its own
//! fields, worded with its own struct names. A time's failure ends a reading (an
//! `Unmarshaler`'s error) where a mismatch is only kept (`UnmarshalTypeError`, the first
//! returned once the rest is read).
//!
//! Reading follows `encoding/json` (Go 1.26.3, moby's): a key matches its field exactly,
//! or else as foldName folds both; unknown keys are passed over; a repeated key decodes
//! into what an earlier one left: structs and maps merged, a slice's elements decoded in
//! place over its backing array (a `null` element keeps what was there, and what a shorter
//! array leaves past its end comes back with a longer one after it), `[]` a new empty
//! slice; `null` makes a slice, map or pointer nil and leaves a string, number, bool or
//! struct as it was; a map's value of the wrong type is kept as its zero value; a time is
//! parsed from its text as written (`Time.UnmarshalJSON`). The document is scanned without
//! recursion ([`crate::json`]), so no nesting exhausts a thread's stack.
//!
//! Held to Go by `testdata/image-config.json`, which scripts/image-config/generate records
//! from moby's own types.

use std::collections::{BTreeMap, BTreeSet};

use crate::go::{self, Time};
use crate::json::{self, Value};

/// The Go type an image's config is read into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum As {
    /// `dockerspec.DockerOCIImage`: running, creating and inspecting an image.
    Docker,
    /// `ocispec.Image`: containerd's unpack of a pulled image.
    Oci,
    /// image_history.go's `struct { RootFS; History }`: `docker history`.
    History,
}

/// `HealthcheckConfig`. Durations are nanoseconds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Healthcheck {
    pub test: Option<Vec<String>>,
    pub interval: i64,
    pub timeout: i64,
    pub start_period: i64,
    pub start_interval: i64,
    pub retries: i64,
}

/// `DockerOCIImageConfig`: OCI's `ImageConfig` and Docker's additions. `None` is Go's nil.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub user: String,
    pub exposed_ports: Option<BTreeSet<String>>,
    pub env: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub volumes: Option<BTreeSet<String>>,
    pub working_dir: String,
    pub labels: Option<BTreeMap<String, String>>,
    pub stop_signal: String,
    pub args_escaped: bool,
    pub healthcheck: Option<Healthcheck>,
    pub on_build: Option<Vec<String>>,
    pub shell: Option<Vec<String>>,
}

/// `RootFS`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootFs {
    pub kind: String,
    pub diff_ids: Option<Vec<String>>,
}

/// `History`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    pub created: Option<Time>,
    pub created_by: String,
    pub author: String,
    pub comment: String,
    pub empty_layer: bool,
}

/// `DockerOCIImage`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Image {
    pub created: Option<Time>,
    pub author: String,
    pub architecture: String,
    pub os: String,
    pub os_version: String,
    pub os_features: Option<Vec<String>>,
    pub variant: String,
    pub config: Config,
    pub rootfs: RootFs,
    pub history: Option<Vec<History>>,
}

/// A config read once, for each Go type it is read into.
#[derive(Debug)]
pub struct Read {
    image: Image,
    events: Vec<Event>,
}

impl Read {
    /// `text` read. Fails, for every type alike, where Go's scanner finds it no JSON
    /// (its `SyntaxError`).
    pub fn new(text: &[u8]) -> Result<Read, String> {
        let v = json::parse(text).map_err(|e| String::from_utf8_lossy(&e).into_owned())?;
        let mut r = Reader { events: Vec::new() };
        let mut image = Img::default();
        r.image(&v, &mut image);
        Ok(Read {
            image: image.done(),
            events: r.events,
        })
    }

    /// What `json.Unmarshal` into `as_` fails with, if it does: a time that does not
    /// parse, else the first type mismatch, among its fields.
    pub fn error(&self, as_: As) -> Option<String> {
        let mut first = None;
        for e in &self.events {
            match e {
                Event::Abort { field, message } if field.at(as_).is_some() => return Some(message.clone()),
                Event::Mismatch { field, value, ty } if first.is_none() => {
                    if let Some((strukt, path)) = field.at(as_) {
                        let ty = ty.name(as_);
                        first = Some(if strukt.is_empty() && path.is_empty() {
                            format!("json: cannot unmarshal {value} into Go value of type {ty}")
                        } else {
                            format!(
                                "json: cannot unmarshal {value} into Go struct field {strukt}.{path} of type {ty}"
                            )
                        });
                    }
                }
                _ => {}
            }
        }
        first
    }

    /// What it holds, as a `DockerOCIImage` read holds it (and the others, of their parts).
    pub fn image(&self) -> &Image {
        &self.image
    }

    pub fn into_image(self) -> Image {
        self.image
    }

    /// What `json.Unmarshal` into `as_` reads, or its error.
    pub fn read(self, as_: As) -> Result<Image, String> {
        match self.error(as_) {
            Some(e) => Err(e),
            None => Ok(self.image),
        }
    }
}

/// `json.Unmarshal(text, &dockerspec.DockerOCIImage{})`: what Docker runs and inspects.
pub fn decode(text: &[u8]) -> Result<Image, String> {
    Read::new(text)?.read(As::Docker)
}

/// `network.ParsePort` (moby api types/network port.go), as a container's config keeps an
/// image's exposed ports (daemon/containerd imagespec.go, `dockerOCIImageConfigToContainerConfig`
/// drops those it does not take): the number before the first `/` as
/// `strconv.ParseUint(s, 10, 16)` reads it, and what follows lowercased as
/// `strings.ToLower` lowers it, `tcp` where there is nothing; written `<number>/<proto>`.
pub fn parse_port(s: &str) -> Option<String> {
    let (port, proto) = s.split_once('/').unwrap_or((s, ""));
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number: u16 = port.parse().ok()?;
    let proto = if proto.is_empty() {
        "tcp".to_string()
    } else {
        String::from_utf8_lossy(&go::to_lower(proto.as_bytes())).into_owned()
    };
    Some(format!("{number}/{proto}"))
}

/// What a reading met: a value of the wrong type, kept, or a time's failure, which ends
/// the reading of a type that has the field.
#[derive(Debug)]
enum Event {
    Mismatch { field: F, value: String, ty: Ty },
    Abort { field: F, message: String },
}

/// The Go type a mismatch names.
#[derive(Debug, Clone, Copy)]
enum Ty {
    Is(&'static str),
    /// The document's own type.
    Document,
    /// `config`'s, which `DockerOCIImage` shadows.
    Config,
}

/// image_history.go's struct, as reflect names it.
const HISTORY_TYPE: &str = "struct { RootFS v1.RootFS \"json:\\\"rootfs\\\"\"; History []v1.History \"json:\\\"history,omitempty\\\"\" }";

impl Ty {
    fn name(self, as_: As) -> &'static str {
        match (self, as_) {
            (Ty::Is(name), _) => name,
            (Ty::Document, As::Docker) => "v1.DockerOCIImage",
            (Ty::Document, As::Oci) => "v1.Image",
            (Ty::Document, As::History) => HISTORY_TYPE,
            (Ty::Config, As::Docker) => "v1.DockerOCIImageConfig",
            (Ty::Config, _) => "v1.ImageConfig",
        }
    }
}

/// A field of `DockerOCIImage`, the others' fields among them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum F {
    Document,
    Created,
    Author,
    Architecture,
    Os,
    OsVersion,
    OsFeatures,
    Variant,
    Config,
    User,
    ExposedPorts,
    Env,
    Entrypoint,
    Cmd,
    Volumes,
    WorkingDir,
    Labels,
    StopSignal,
    ArgsEscaped,
    Healthcheck,
    Test,
    Interval,
    Timeout,
    StartPeriod,
    StartInterval,
    Retries,
    OnBuild,
    Shell,
    RootFs,
    RootFsType,
    DiffIds,
    History,
    HistoryCreated,
    CreatedBy,
    HistoryAuthor,
    Comment,
    EmptyLayer,
}

impl F {
    /// Where a mismatch in this field falls when read as `as_`, as Go's decoder words it
    /// (`UnmarshalTypeError`'s Struct, the innermost struct read, and Field, the JSON names
    /// to it, with the embedded structs' names on the way); `None` where `as_` has no such
    /// field.
    fn at(self, as_: As) -> Option<(&'static str, &'static str)> {
        use As::{Docker, Oci};
        Some(match (as_, self) {
            (_, F::Document) => ("", ""),
            (As::History, F::RootFs) => ("", "rootfs"),
            (As::History, F::History) => ("", "history"),
            (As::History, F::RootFsType) => ("RootFS", "rootfs.type"),
            (As::History, F::DiffIds) => ("RootFS", "rootfs.diff_ids"),
            (As::History, F::HistoryCreated) => ("History", "history.created"),
            (As::History, F::CreatedBy) => ("History", "history.created_by"),
            (As::History, F::HistoryAuthor) => ("History", "history.author"),
            (As::History, F::Comment) => ("History", "history.comment"),
            (As::History, F::EmptyLayer) => ("History", "history.empty_layer"),
            (As::History, _) => return None,
            (Docker, F::Created) => ("DockerOCIImage", "Image.created"),
            (Docker, F::Author) => ("DockerOCIImage", "Image.author"),
            (Docker, F::Architecture) => ("DockerOCIImage", "Image.Platform.architecture"),
            (Docker, F::Os) => ("DockerOCIImage", "Image.Platform.os"),
            (Docker, F::OsVersion) => ("DockerOCIImage", "Image.Platform.os.version"),
            (Docker, F::OsFeatures) => ("DockerOCIImage", "Image.Platform.os.features"),
            (Docker, F::Variant) => ("DockerOCIImage", "Image.Platform.variant"),
            (Docker, F::Config) => ("DockerOCIImage", "config"),
            (Docker, F::User) => ("DockerOCIImageConfig", "config.ImageConfig.User"),
            (Docker, F::ExposedPorts) => ("DockerOCIImageConfig", "config.ImageConfig.ExposedPorts"),
            (Docker, F::Env) => ("DockerOCIImageConfig", "config.ImageConfig.Env"),
            (Docker, F::Entrypoint) => ("DockerOCIImageConfig", "config.ImageConfig.Entrypoint"),
            (Docker, F::Cmd) => ("DockerOCIImageConfig", "config.ImageConfig.Cmd"),
            (Docker, F::Volumes) => ("DockerOCIImageConfig", "config.ImageConfig.Volumes"),
            (Docker, F::WorkingDir) => ("DockerOCIImageConfig", "config.ImageConfig.WorkingDir"),
            (Docker, F::Labels) => ("DockerOCIImageConfig", "config.ImageConfig.Labels"),
            (Docker, F::StopSignal) => ("DockerOCIImageConfig", "config.ImageConfig.StopSignal"),
            (Docker, F::ArgsEscaped) => ("DockerOCIImageConfig", "config.ImageConfig.ArgsEscaped"),
            (Docker, F::Healthcheck) => (
                "DockerOCIImageConfig",
                "config.DockerOCIImageConfigExt.Healthcheck",
            ),
            (Docker, F::OnBuild) => ("DockerOCIImageConfig", "config.DockerOCIImageConfigExt.OnBuild"),
            (Docker, F::Shell) => ("DockerOCIImageConfig", "config.DockerOCIImageConfigExt.Shell"),
            (Docker, F::Test) => (
                "HealthcheckConfig",
                "config.DockerOCIImageConfigExt.Healthcheck.Test",
            ),
            (Docker, F::Interval) => (
                "HealthcheckConfig",
                "config.DockerOCIImageConfigExt.Healthcheck.Interval",
            ),
            (Docker, F::Timeout) => (
                "HealthcheckConfig",
                "config.DockerOCIImageConfigExt.Healthcheck.Timeout",
            ),
            (Docker, F::StartPeriod) => (
                "HealthcheckConfig",
                "config.DockerOCIImageConfigExt.Healthcheck.StartPeriod",
            ),
            (Docker, F::StartInterval) => (
                "HealthcheckConfig",
                "config.DockerOCIImageConfigExt.Healthcheck.StartInterval",
            ),
            (Docker, F::Retries) => (
                "HealthcheckConfig",
                "config.DockerOCIImageConfigExt.Healthcheck.Retries",
            ),
            (Docker, F::RootFs) => ("DockerOCIImage", "Image.rootfs"),
            (Docker, F::RootFsType) => ("RootFS", "Image.rootfs.type"),
            (Docker, F::DiffIds) => ("RootFS", "Image.rootfs.diff_ids"),
            (Docker, F::History) => ("DockerOCIImage", "Image.history"),
            (Docker, F::HistoryCreated) => ("History", "Image.history.created"),
            (Docker, F::CreatedBy) => ("History", "Image.history.created_by"),
            (Docker, F::HistoryAuthor) => ("History", "Image.history.author"),
            (Docker, F::Comment) => ("History", "Image.history.comment"),
            (Docker, F::EmptyLayer) => ("History", "Image.history.empty_layer"),
            (Oci, F::Created) => ("Image", "created"),
            (Oci, F::Author) => ("Image", "author"),
            (Oci, F::Architecture) => ("Image", "Platform.architecture"),
            (Oci, F::Os) => ("Image", "Platform.os"),
            (Oci, F::OsVersion) => ("Image", "Platform.os.version"),
            (Oci, F::OsFeatures) => ("Image", "Platform.os.features"),
            (Oci, F::Variant) => ("Image", "Platform.variant"),
            (Oci, F::Config) => ("Image", "config"),
            (Oci, F::User) => ("ImageConfig", "config.User"),
            (Oci, F::ExposedPorts) => ("ImageConfig", "config.ExposedPorts"),
            (Oci, F::Env) => ("ImageConfig", "config.Env"),
            (Oci, F::Entrypoint) => ("ImageConfig", "config.Entrypoint"),
            (Oci, F::Cmd) => ("ImageConfig", "config.Cmd"),
            (Oci, F::Volumes) => ("ImageConfig", "config.Volumes"),
            (Oci, F::WorkingDir) => ("ImageConfig", "config.WorkingDir"),
            (Oci, F::Labels) => ("ImageConfig", "config.Labels"),
            (Oci, F::StopSignal) => ("ImageConfig", "config.StopSignal"),
            (Oci, F::ArgsEscaped) => ("ImageConfig", "config.ArgsEscaped"),
            // Docker's additions to the config are none of ocispec.Image's.
            (
                Oci,
                F::Healthcheck
                | F::OnBuild
                | F::Shell
                | F::Test
                | F::Interval
                | F::Timeout
                | F::StartPeriod
                | F::StartInterval
                | F::Retries,
            ) => return None,
            (Oci, F::RootFs) => ("Image", "rootfs"),
            (Oci, F::RootFsType) => ("RootFS", "rootfs.type"),
            (Oci, F::DiffIds) => ("RootFS", "rootfs.diff_ids"),
            (Oci, F::History) => ("Image", "history"),
            (Oci, F::HistoryCreated) => ("History", "history.created"),
            (Oci, F::CreatedBy) => ("History", "history.created_by"),
            (Oci, F::HistoryAuthor) => ("History", "history.author"),
            (Oci, F::Comment) => ("History", "history.comment"),
            (Oci, F::EmptyLayer) => ("History", "history.empty_layer"),
        })
    }
}

/// A slice as Go's decoder fills one: its backing array, and its length, `None` for nil.
#[derive(Debug, Clone)]
struct Slice<T> {
    held: Vec<T>,
    len: Option<usize>,
}

impl<T> Default for Slice<T> {
    fn default() -> Self {
        Slice {
            held: Vec::new(),
            len: None,
        }
    }
}

impl<T: Clone> Slice<T> {
    fn done(&self) -> Option<Vec<T>> {
        self.len.map(|n| self.held.get(..n).unwrap_or_default().to_vec())
    }
}

#[derive(Debug, Default)]
struct Hc {
    test: Slice<String>,
    interval: i64,
    timeout: i64,
    start_period: i64,
    start_interval: i64,
    retries: i64,
}

#[derive(Debug, Default)]
struct Cfg {
    user: String,
    exposed_ports: Option<BTreeSet<String>>,
    env: Slice<String>,
    entrypoint: Slice<String>,
    cmd: Slice<String>,
    volumes: Option<BTreeSet<String>>,
    working_dir: String,
    labels: Option<BTreeMap<String, String>>,
    stop_signal: String,
    args_escaped: bool,
    healthcheck: Option<Hc>,
    on_build: Slice<String>,
    shell: Slice<String>,
}

#[derive(Debug, Default)]
struct Img {
    created: Option<Time>,
    author: String,
    architecture: String,
    os: String,
    os_version: String,
    os_features: Slice<String>,
    variant: String,
    config: Cfg,
    rootfs_type: String,
    diff_ids: Slice<String>,
    history: Slice<History>,
}

impl Img {
    fn done(self) -> Image {
        let c = self.config;
        Image {
            created: self.created,
            author: self.author,
            architecture: self.architecture,
            os: self.os,
            os_version: self.os_version,
            os_features: self.os_features.done(),
            variant: self.variant,
            config: Config {
                user: c.user,
                exposed_ports: c.exposed_ports,
                env: c.env.done(),
                entrypoint: c.entrypoint.done(),
                cmd: c.cmd.done(),
                volumes: c.volumes,
                working_dir: c.working_dir,
                labels: c.labels,
                stop_signal: c.stop_signal,
                args_escaped: c.args_escaped,
                healthcheck: c.healthcheck.map(|h| Healthcheck {
                    test: h.test.done(),
                    interval: h.interval,
                    timeout: h.timeout,
                    start_period: h.start_period,
                    start_interval: h.start_interval,
                    retries: h.retries,
                }),
                on_build: c.on_build.done(),
                shell: c.shell.done(),
            },
            rootfs: RootFs {
                kind: self.rootfs_type,
                diff_ids: self.diff_ids.done(),
            },
            history: self.history.done(),
        }
    }
}

/// What Go calls a JSON value's kind in its messages.
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(..) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// A decoded string: valid UTF-8, as Go's decoder leaves it (an invalid byte is U+FFFD).
fn text(s: &[u8]) -> String {
    String::from_utf8_lossy(s).into_owned()
}

struct Reader {
    events: Vec<Event>,
}

impl Reader {
    fn mismatch(&mut self, field: F, value: &str, ty: Ty) {
        self.events.push(Event::Mismatch {
            field,
            value: value.to_string(),
            ty,
        });
    }

    fn string(&mut self, v: &Value, field: F, into: &mut String, ty: &'static str) {
        match v {
            Value::Null => {}
            Value::String(s, _) => *into = text(s),
            _ => self.mismatch(field, kind(v), Ty::Is(ty)),
        }
    }

    fn boolean(&mut self, v: &Value, field: F, into: &mut bool) {
        match v {
            Value::Null => {}
            Value::Bool(b) => *into = *b,
            _ => self.mismatch(field, kind(v), Ty::Is("bool")),
        }
    }

    /// An integer field: `strconv.ParseInt(text, 10, 64)` of the number as written.
    fn int(&mut self, v: &Value, field: F, into: &mut i64, ty: &'static str) {
        match v {
            Value::Null => {}
            Value::Number(n) => match std::str::from_utf8(n).ok().and_then(|t| t.parse::<i64>().ok()) {
                // ParseInt takes no leading `+`, and JSON writes none.
                Some(i) => *into = i,
                None => self.mismatch(field, &format!("number {}", text(n)), Ty::Is(ty)),
            },
            _ => self.mismatch(field, kind(v), Ty::Is(ty)),
        }
    }

    /// A `[]string` (or a slice of a string type), decoded in place over its backing array.
    fn strings(
        &mut self,
        v: &Value,
        field: F,
        into: &mut Slice<String>,
        slice: &'static str,
        elem: &'static str,
    ) {
        match v {
            Value::Null => *into = Slice::default(),
            Value::Array(items) if items.is_empty() => {
                *into = Slice {
                    held: Vec::new(),
                    len: Some(0),
                };
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    if i >= into.held.len() {
                        into.held.push(String::new());
                    }
                    if let Some(slot) = into.held.get_mut(i) {
                        self.string(item, field, slot, elem);
                    }
                }
                into.len = Some(items.len());
            }
            _ => self.mismatch(field, kind(v), Ty::Is(slice)),
        }
    }

    /// A `map[string]struct{}`: keys merged into what is there.
    fn set(&mut self, v: &Value, field: F, into: &mut Option<BTreeSet<String>>) {
        match v {
            Value::Null => *into = None,
            Value::Object(members) => {
                let set = into.get_or_insert_with(BTreeSet::new);
                for (k, m) in members {
                    if !matches!(m, Value::Null | Value::Object(_)) {
                        self.mismatch(field, kind(m), Ty::Is("struct {}"));
                    }
                    set.insert(text(k));
                }
            }
            _ => self.mismatch(field, kind(v), Ty::Is("map[string]struct {}")),
        }
    }

    /// A `map[string]string`: members merged into what is there, each read into a zero
    /// value, so `null` or a value of the wrong type is "".
    fn map(&mut self, v: &Value, field: F, into: &mut Option<BTreeMap<String, String>>) {
        match v {
            Value::Null => *into = None,
            Value::Object(members) => {
                let mut read = Vec::with_capacity(members.len());
                for (k, m) in members {
                    let mut s = String::new();
                    self.string(m, field, &mut s, "string");
                    read.push((text(k), s));
                }
                into.get_or_insert_with(BTreeMap::new).extend(read);
            }
            _ => self.mismatch(field, kind(v), Ty::Is("map[string]string")),
        }
    }

    /// A `*time.Time`: `null` makes it nil, and `Time.UnmarshalJSON` reads anything else
    /// from its text as written; its failure ends the reading.
    fn time(&mut self, v: &Value, field: F, into: &mut Option<Time>) {
        match v {
            Value::Null => *into = None,
            Value::String(s, raw) => match go::parse_rfc3339(raw.as_deref().unwrap_or(s)) {
                Ok(t) => *into = Some(t),
                Err(e) => self.events.push(Event::Abort {
                    field,
                    message: text(&e),
                }),
            },
            _ => self.events.push(Event::Abort {
                field,
                message: "Time.UnmarshalJSON: input is not a JSON string".into(),
            }),
        }
    }

    /// The members of an object, each with the field it fills: named exactly, or else
    /// alike as foldName folds both; none for `null`, and a mismatch for anything else.
    fn members<'v>(
        &mut self,
        v: &'v Value,
        field: F,
        ty: Ty,
        fields: &[&'static str],
    ) -> Vec<(&'static str, &'v Value)> {
        match v {
            Value::Object(members) => members
                .iter()
                .filter_map(|(k, m)| {
                    let exact = fields.iter().find(|f| f.as_bytes() == k.as_slice());
                    exact
                        .or_else(|| fields.iter().find(|f| go::equal_fold_ascii(k, f.as_bytes())))
                        .map(|f| (*f, m))
                })
                .collect(),
            Value::Null => Vec::new(),
            _ => {
                self.mismatch(field, kind(v), ty);
                Vec::new()
            }
        }
    }

    /// `*HealthcheckConfig`: `null` makes it nil; an object is read into what is there,
    /// a new one where it is nil.
    fn healthcheck(&mut self, v: &Value, into: &mut Option<Hc>) {
        if matches!(v, Value::Null) {
            *into = None;
            return;
        }
        const FIELDS: &[&str] = &[
            "Test",
            "Interval",
            "Timeout",
            "StartPeriod",
            "StartInterval",
            "Retries",
        ];
        let members = self.members(v, F::Healthcheck, Ty::Is("v1.HealthcheckConfig"), FIELDS);
        if !matches!(v, Value::Object(_)) {
            return;
        }
        let h = into.get_or_insert_with(Hc::default);
        for (name, m) in members {
            match name {
                "Test" => self.strings(m, F::Test, &mut h.test, "[]string", "string"),
                "Interval" => self.int(m, F::Interval, &mut h.interval, "time.Duration"),
                "Timeout" => self.int(m, F::Timeout, &mut h.timeout, "time.Duration"),
                "StartPeriod" => self.int(m, F::StartPeriod, &mut h.start_period, "time.Duration"),
                "StartInterval" => self.int(m, F::StartInterval, &mut h.start_interval, "time.Duration"),
                _ => self.int(m, F::Retries, &mut h.retries, "int"),
            }
        }
    }

    fn config(&mut self, v: &Value, into: &mut Cfg) {
        const FIELDS: &[&str] = &[
            "User",
            "ExposedPorts",
            "Env",
            "Entrypoint",
            "Cmd",
            "Volumes",
            "WorkingDir",
            "Labels",
            "StopSignal",
            "ArgsEscaped",
            "Healthcheck",
            "OnBuild",
            "Shell",
        ];
        for (name, m) in self.members(v, F::Config, Ty::Config, FIELDS) {
            match name {
                "User" => self.string(m, F::User, &mut into.user, "string"),
                "ExposedPorts" => self.set(m, F::ExposedPorts, &mut into.exposed_ports),
                "Env" => self.strings(m, F::Env, &mut into.env, "[]string", "string"),
                "Entrypoint" => self.strings(m, F::Entrypoint, &mut into.entrypoint, "[]string", "string"),
                "Cmd" => self.strings(m, F::Cmd, &mut into.cmd, "[]string", "string"),
                "Volumes" => self.set(m, F::Volumes, &mut into.volumes),
                "WorkingDir" => self.string(m, F::WorkingDir, &mut into.working_dir, "string"),
                "Labels" => self.map(m, F::Labels, &mut into.labels),
                "StopSignal" => self.string(m, F::StopSignal, &mut into.stop_signal, "string"),
                "ArgsEscaped" => self.boolean(m, F::ArgsEscaped, &mut into.args_escaped),
                "Healthcheck" => self.healthcheck(m, &mut into.healthcheck),
                "OnBuild" => self.strings(m, F::OnBuild, &mut into.on_build, "[]string", "string"),
                _ => self.strings(m, F::Shell, &mut into.shell, "[]string", "string"),
            }
        }
    }

    fn rootfs(&mut self, v: &Value, into: &mut Img) {
        for (name, m) in self.members(v, F::RootFs, Ty::Is("v1.RootFS"), &["type", "diff_ids"]) {
            if name == "type" {
                self.string(m, F::RootFsType, &mut into.rootfs_type, "string");
            } else {
                self.strings(
                    m,
                    F::DiffIds,
                    &mut into.diff_ids,
                    "[]digest.Digest",
                    "digest.Digest",
                );
            }
        }
    }

    /// A `History`, read into what the slice's backing array holds there.
    fn history_entry(&mut self, v: &Value, into: &mut History) {
        const FIELDS: &[&str] = &["created", "created_by", "author", "comment", "empty_layer"];
        for (name, m) in self.members(v, F::History, Ty::Is("v1.History"), FIELDS) {
            match name {
                "created" => self.time(m, F::HistoryCreated, &mut into.created),
                "created_by" => self.string(m, F::CreatedBy, &mut into.created_by, "string"),
                "author" => self.string(m, F::HistoryAuthor, &mut into.author, "string"),
                "comment" => self.string(m, F::Comment, &mut into.comment, "string"),
                _ => self.boolean(m, F::EmptyLayer, &mut into.empty_layer),
            }
        }
    }

    /// `[]History`, its elements read in place over its backing array, as `strings`.
    fn history(&mut self, v: &Value, into: &mut Slice<History>) {
        match v {
            Value::Null => *into = Slice::default(),
            Value::Array(items) if items.is_empty() => {
                *into = Slice {
                    held: Vec::new(),
                    len: Some(0),
                };
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    if i >= into.held.len() {
                        into.held.push(History::default());
                    }
                    if let Some(slot) = into.held.get_mut(i) {
                        self.history_entry(item, slot);
                    }
                }
                into.len = Some(items.len());
            }
            _ => self.mismatch(F::History, kind(v), Ty::Is("[]v1.History")),
        }
    }

    fn image(&mut self, v: &Value, into: &mut Img) {
        const FIELDS: &[&str] = &[
            "created",
            "author",
            "architecture",
            "os",
            "os.version",
            "os.features",
            "variant",
            "config",
            "rootfs",
            "history",
        ];
        for (name, m) in self.members(v, F::Document, Ty::Document, FIELDS) {
            match name {
                "created" => self.time(m, F::Created, &mut into.created),
                "author" => self.string(m, F::Author, &mut into.author, "string"),
                "architecture" => self.string(m, F::Architecture, &mut into.architecture, "string"),
                "os" => self.string(m, F::Os, &mut into.os, "string"),
                "os.version" => self.string(m, F::OsVersion, &mut into.os_version, "string"),
                "os.features" => self.strings(m, F::OsFeatures, &mut into.os_features, "[]string", "string"),
                "variant" => self.string(m, F::Variant, &mut into.variant, "string"),
                "config" => self.config(m, &mut into.config),
                "rootfs" => self.rootfs(m, into),
                _ => self.history(m, &mut into.history),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use serde_json::{Value as Json, json};

    fn strings(v: &Option<Vec<String>>) -> Json {
        json!(v)
    }

    fn keys(v: &Option<BTreeSet<String>>) -> Json {
        json!(v.as_ref().map(|s| s.iter().collect::<Vec<_>>()))
    }

    fn time(t: &Option<Time>) -> Json {
        json!(t.as_ref().map(Time::format_rfc3339_nano))
    }

    fn history(h: &Option<Vec<History>>) -> Json {
        json!(h.as_ref().map(|h| {
            h.iter()
                .map(|h| {
                    json!({
                        "created": time(&h.created),
                        "created_by": h.created_by,
                        "author": h.author,
                        "comment": h.comment,
                        "empty_layer": h.empty_layer,
                    })
                })
                .collect::<Vec<_>>()
        }))
    }

    /// The image as the oracle dumps it for `as_`: what that type holds.
    fn dump(i: &Image, as_: As) -> Json {
        if as_ == As::History {
            return json!({
                "rootfs_type": i.rootfs.kind,
                "diff_ids": strings(&i.rootfs.diff_ids),
                "history": history(&i.history),
            });
        }
        let c = &i.config;
        let docker = as_ == As::Docker;
        let healthcheck = c.healthcheck.as_ref().filter(|_| docker).map(|h| {
            json!({
                "test": strings(&h.test),
                "interval": h.interval,
                "timeout": h.timeout,
                "start_period": h.start_period,
                "start_interval": h.start_interval,
                "retries": h.retries,
            })
        });
        let ports: Option<BTreeSet<String>> = docker.then(|| {
            c.exposed_ports
                .iter()
                .flatten()
                .filter_map(|p| parse_port(p))
                .collect()
        });
        json!({
            "created": time(&i.created),
            "author": i.author,
            "architecture": i.architecture,
            "os": i.os,
            "os_version": i.os_version,
            "os_features": strings(&i.os_features),
            "variant": i.variant,
            "rootfs_type": i.rootfs.kind,
            "diff_ids": strings(&i.rootfs.diff_ids),
            "history": history(&i.history),
            "config": {
                "user": c.user,
                "exposed_ports": keys(&c.exposed_ports),
                "env": strings(&c.env),
                "entrypoint": strings(&c.entrypoint),
                "cmd": strings(&c.cmd),
                "volumes": keys(&c.volumes),
                "working_dir": c.working_dir,
                "labels": c.labels,
                "stop_signal": c.stop_signal,
                "args_escaped": c.args_escaped,
                "healthcheck": healthcheck,
                "on_build": if docker { strings(&c.on_build) } else { Json::Null },
                "shell": if docker { strings(&c.shell) } else { Json::Null },
            },
            "ports": ports.map(|p| p.into_iter().collect::<Vec<_>>()),
        })
    }

    /// Every case scripts/image-config/generate records from moby's own types, read as
    /// each of the three: its error, or all it holds, nil and empty told apart.
    #[test]
    fn configs_read_as_moby_reads_them() {
        let data: Json = serde_json::from_str(include_str!("../testdata/image-config.json")).unwrap();
        let cases = data["cases"].as_array().unwrap();
        assert!(cases.len() > 300, "{}", cases.len());
        let mut failed = Vec::new();
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let text = base64::engine::general_purpose::STANDARD
                .decode(case["config"].as_str().unwrap())
                .unwrap();
            let read = Read::new(&text);
            for (as_, error, image) in [
                (As::Docker, "error", "image"),
                (As::Oci, "oci_error", "oci_image"),
                (As::History, "history_error", "history"),
            ] {
                let want = case[error].as_str().unwrap();
                let got = match &read {
                    Err(e) => e.clone(),
                    Ok(r) => r.error(as_).unwrap_or_default(),
                };
                if got != want {
                    failed.push(format!("{name} as {as_:?}: error {got:?}, Go {want:?}"));
                    continue;
                }
                if want.is_empty() {
                    let got = dump(read.as_ref().unwrap().image(), as_);
                    if got != case[image] {
                        failed.push(format!("{name} as {as_:?}: {got}\n  Go {}", case[image]));
                    }
                }
            }
        }
        assert!(
            failed.is_empty(),
            "{} of {}:\n{}",
            failed.len(),
            cases.len() * 3,
            failed.join("\n")
        );
    }

    /// network.ParsePort's own cases (port_test.go), and its normalizing.
    #[test]
    fn ports_parse_as_moby_parses_them() {
        for (s, want) in [
            ("80", Some("80/tcp")),
            ("80/tcp", Some("80/tcp")),
            ("80/TCP", Some("80/tcp")),
            ("80/tCp", Some("80/tcp")),
            ("53/udp", Some("53/udp")),
            ("443/sctp", Some("443/sctp")),
            ("80/xyz", Some("80/xyz")),
            ("0", Some("0/tcp")),
            ("65535", Some("65535/tcp")),
            ("080", Some("80/tcp")),
            ("80/tcp/x", Some("80/tcp/x")),
            ("", None),
            ("/tcp", None),
            ("65536", None),
            ("+80", None),
            ("-1", None),
            (" 80", None),
            ("80 ", None),
            ("8080-8090/tcp", None),
            ("x", None),
        ] {
            assert_eq!(parse_port(s).as_deref(), want, "{s:?}");
        }
    }
}
