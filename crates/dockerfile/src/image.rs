//! An image's config as BuildKit holds it while it plans a build: moby's
//! `DockerOCIImage` (docker-image-spec v1.3.1), read as Go's `encoding/json` reads it and
//! written as `json.Marshal` writes it, so the config BuildKit would write, and its digest,
//! come out byte for byte.
//!
//! Reading follows Go: keys match fields exactly or else ignoring case, unknown keys and
//! `null`s are skipped, maps merge across duplicate keys, a value of the wrong type is
//! skipped and reported once the rest is read, and a time is parsed from its text as
//! written.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::go::{self, Time};
use crate::json::{self, Value};
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

/// An `Unmarshaler`'s error, which ends the reading at once, where a value of the wrong
/// type (`UnmarshalTypeError`) is kept and reported once the rest is read.
struct Abort(Vec<u8>);

/// Where in the document a value goes, for Go's messages: the struct whose field it fills,
/// and the path to that field, the names of embedded structs among its JSON names
/// (`config.ImageConfig.User`). Empty for the document itself.
struct Ctx {
    strukt: &'static str,
    field: String,
}

impl Ctx {
    fn new(strukt: &'static str, field: impl Into<String>) -> Ctx {
        Ctx {
            strukt,
            field: field.into(),
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

struct Reader {
    /// The first type error.
    saved: Option<String>,
}

impl Reader {
    fn mismatch(&mut self, value: &str, ctx: &Ctx, go_type: &str) {
        if self.saved.is_some() {
            return;
        }
        self.saved = Some(if ctx.field.is_empty() {
            format!("json: cannot unmarshal {value} into Go value of type {go_type}")
        } else {
            format!(
                "json: cannot unmarshal {value} into Go struct field {}.{} of type {go_type}",
                ctx.strukt, ctx.field
            )
        });
    }

    fn string(&mut self, v: &Value, ctx: &Ctx, into: &mut Vec<u8>, go_type: &str) {
        match v {
            Value::Null => {}
            Value::String(s, _) => *into = s.clone(),
            _ => self.mismatch(kind(v), ctx, go_type),
        }
    }

    fn boolean(&mut self, v: &Value, ctx: &Ctx, into: &mut bool) {
        match v {
            Value::Null => {}
            Value::Bool(b) => *into = *b,
            _ => self.mismatch(kind(v), ctx, "bool"),
        }
    }

    /// An `int64`-backed field: an integer literal that fits, as `strconv.ParseInt`.
    fn int(&mut self, v: &Value, ctx: &Ctx, into: &mut i64, go_type: &str) {
        match v {
            Value::Null => {}
            Value::Number(n) => match std::str::from_utf8(n).ok().and_then(|t| t.parse::<i64>().ok()) {
                // ParseInt takes no leading `+`, and JSON has none.
                Some(i) => *into = i,
                None => {
                    let text = String::from_utf8_lossy(n);
                    self.mismatch(&format!("number {text}"), ctx, go_type);
                }
            },
            _ => self.mismatch(kind(v), ctx, go_type),
        }
    }

    /// A `[]string` (or a slice of a string type): `null` empties it; an element of
    /// another type stays empty.
    fn strings(&mut self, v: &Value, ctx: &Ctx, into: &mut Vec<Vec<u8>>, elem: &str) -> bool {
        match v {
            Value::Null => {
                into.clear();
                false
            }
            Value::Array(items) => {
                into.clear();
                for item in items {
                    let mut s = Vec::new();
                    self.string(item, ctx, &mut s, elem);
                    into.push(s);
                }
                true
            }
            _ => {
                self.mismatch(kind(v), ctx, &format!("[]{elem}"));
                false
            }
        }
    }

    /// A `map[string]struct{}`: members merge into what is there.
    fn set(&mut self, v: &Value, ctx: &Ctx, into: &mut BTreeMap<Vec<u8>, ()>) {
        match v {
            Value::Null => into.clear(),
            Value::Object(members) => {
                for (k, m) in members {
                    match m {
                        Value::Null | Value::Object(_) => {}
                        _ => self.mismatch(kind(m), ctx, "struct {}"),
                    }
                    into.insert(k.clone(), ());
                }
            }
            _ => self.mismatch(kind(v), ctx, "map[string]struct {}"),
        }
    }

    /// A `map[string]string`: members merge into what is there.
    fn map(&mut self, v: &Value, ctx: &Ctx, into: &mut BTreeMap<Vec<u8>, Vec<u8>>) {
        match v {
            Value::Null => into.clear(),
            Value::Object(members) => {
                for (k, m) in members {
                    let mut s = Vec::new();
                    self.string(m, ctx, &mut s, "string");
                    into.insert(k.clone(), s);
                }
            }
            _ => self.mismatch(kind(v), ctx, "map[string]string"),
        }
    }

    /// A `*time.Time`, which `Time.UnmarshalJSON` reads from the value's text as written.
    fn time(v: &Value, into: &mut Option<Time>) -> Result<(), Abort> {
        match v {
            Value::Null => {
                *into = None;
                Ok(())
            }
            Value::String(s, raw) => {
                let t = go::parse_rfc3339(raw.as_deref().unwrap_or(s)).map_err(Abort)?;
                *into = Some(t);
                Ok(())
            }
            _ => Err(Abort(b"Time.UnmarshalJSON: input is not a JSON string".to_vec())),
        }
    }

    /// The members of an object, each with the field it fills: exactly named, or else
    /// named alike ignoring case.
    fn members<'v>(
        &mut self,
        v: &'v Value,
        ctx: &Ctx,
        go_type: &str,
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
                self.mismatch(kind(v), ctx, go_type);
                Vec::new()
            }
        }
    }

    fn healthcheck(&mut self, v: &Value, into: &mut Option<Healthcheck>) {
        if matches!(v, Value::Null) {
            *into = None;
            return;
        }
        const AT: &str = "config.DockerOCIImageConfigExt.Healthcheck";
        let outer = Ctx::new("DockerOCIImageConfig", AT);
        const FIELDS: &[&str] = &[
            "Test",
            "Interval",
            "Timeout",
            "StartPeriod",
            "StartInterval",
            "Retries",
        ];
        let members = self.members(v, &outer, "v1.HealthcheckConfig", FIELDS);
        if !matches!(v, Value::Object(_)) {
            return;
        }
        let h = into.get_or_insert_with(Healthcheck::default);
        for (name, m) in members {
            let ctx = Ctx::new("HealthcheckConfig", format!("{AT}.{name}"));
            match name {
                "Test" => {
                    self.strings(m, &ctx, &mut h.test, "string");
                }
                "Interval" => self.int(m, &ctx, &mut h.interval, "time.Duration"),
                "Timeout" => self.int(m, &ctx, &mut h.timeout, "time.Duration"),
                "StartPeriod" => self.int(m, &ctx, &mut h.start_period, "time.Duration"),
                "StartInterval" => self.int(m, &ctx, &mut h.start_interval, "time.Duration"),
                _ => self.int(m, &ctx, &mut h.retries, "int"),
            }
        }
    }

    fn config(&mut self, v: &Value, into: &mut Config) {
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
        let outer = Ctx::new("DockerOCIImage", "config");
        for (name, m) in self.members(v, &outer, "v1.DockerOCIImageConfig", FIELDS) {
            // OCI's fields are ImageConfig's; Docker's additions DockerOCIImageConfigExt's.
            let embedded = match name {
                "Healthcheck" | "OnBuild" | "Shell" => "DockerOCIImageConfigExt",
                _ => "ImageConfig",
            };
            let ctx = Ctx::new("DockerOCIImageConfig", format!("config.{embedded}.{name}"));
            match name {
                "User" => self.string(m, &ctx, &mut into.user, "string"),
                "ExposedPorts" => self.set(m, &ctx, &mut into.exposed_ports),
                "Env" => {
                    self.strings(m, &ctx, &mut into.env, "string");
                }
                "Entrypoint" => {
                    self.strings(m, &ctx, &mut into.entrypoint, "string");
                }
                "Cmd" => {
                    self.strings(m, &ctx, &mut into.cmd, "string");
                }
                "Volumes" => self.set(m, &ctx, &mut into.volumes),
                "WorkingDir" => self.string(m, &ctx, &mut into.working_dir, "string"),
                "Labels" => self.map(m, &ctx, &mut into.labels),
                "StopSignal" => self.string(m, &ctx, &mut into.stop_signal, "string"),
                "ArgsEscaped" => self.boolean(m, &ctx, &mut into.args_escaped),
                "Healthcheck" => self.healthcheck(m, &mut into.healthcheck),
                "OnBuild" => {
                    self.strings(m, &ctx, &mut into.on_build, "string");
                }
                _ => {
                    self.strings(m, &ctx, &mut into.shell, "string");
                }
            }
        }
    }

    fn rootfs(&mut self, v: &Value, into: &mut RootFs) {
        let outer = Ctx::new("DockerOCIImage", "Image.rootfs");
        for (name, m) in self.members(v, &outer, "v1.RootFS", &["type", "diff_ids"]) {
            let ctx = Ctx::new("RootFS", format!("Image.rootfs.{name}"));
            if name == "type" {
                self.string(m, &ctx, &mut into.kind, "string");
                continue;
            }
            match m {
                Value::Null => into.diff_ids = None,
                Value::Array(_) => {
                    let mut ids = Vec::new();
                    self.strings(m, &ctx, &mut ids, "digest.Digest");
                    into.diff_ids = Some(ids);
                }
                // A value of the wrong type leaves the field as it was.
                _ => self.mismatch(kind(m), &ctx, "[]digest.Digest"),
            }
        }
    }

    fn history(&mut self, v: &Value, into: &mut Vec<History>) -> Result<(), Abort> {
        let ctx = Ctx::new("DockerOCIImage", "Image.history");
        let items = match v {
            Value::Null => {
                into.clear();
                return Ok(());
            }
            Value::Array(items) => items,
            _ => {
                self.mismatch(kind(v), &ctx, "[]v1.History");
                return Ok(());
            }
        };
        into.clear();
        const FIELDS: &[&str] = &["created", "created_by", "author", "comment", "empty_layer"];
        for item in items {
            let mut h = History::default();
            for (name, m) in self.members(item, &ctx, "v1.History", FIELDS) {
                let ctx = Ctx::new("History", format!("Image.history.{name}"));
                match name {
                    "created" => Self::time(m, &mut h.created)?,
                    "created_by" => self.string(m, &ctx, &mut h.created_by, "string"),
                    "author" => self.string(m, &ctx, &mut h.author, "string"),
                    "comment" => self.string(m, &ctx, &mut h.comment, "string"),
                    _ => self.boolean(m, &ctx, &mut h.empty_layer),
                }
            }
            into.push(h);
        }
        Ok(())
    }

    fn image(&mut self, v: &Value, into: &mut Image) -> Result<(), Abort> {
        const FIELDS: &[&str] = &[
            "created",
            "author",
            "architecture",
            "os",
            "os.version",
            "os.features",
            "variant",
            "rootfs",
            "history",
            "config",
        ];
        let top = Ctx::new("", "");
        for (name, m) in self.members(v, &top, "v1.DockerOCIImage", FIELDS) {
            // OCI's Image is embedded, and its Platform within it.
            let field = match name {
                "config" => name.to_string(),
                "architecture" | "os" | "os.version" | "os.features" | "variant" => {
                    format!("Image.Platform.{name}")
                }
                _ => format!("Image.{name}"),
            };
            let ctx = Ctx::new("DockerOCIImage", field);
            match name {
                "created" => Self::time(m, &mut into.created)?,
                "author" => self.string(m, &ctx, &mut into.author, "string"),
                "architecture" => self.string(m, &ctx, &mut into.platform.architecture, "string"),
                "os" => self.string(m, &ctx, &mut into.platform.os, "string"),
                "os.version" => self.string(m, &ctx, &mut into.platform.os_version, "string"),
                "os.features" => {
                    self.strings(m, &ctx, &mut into.platform.os_features, "string");
                }
                "variant" => self.string(m, &ctx, &mut into.platform.variant, "string"),
                "rootfs" => self.rootfs(m, &mut into.rootfs),
                "history" => self.history(m, &mut into.history)?,
                _ => self.config(m, &mut into.config),
            }
        }
        Ok(())
    }
}

impl Image {
    /// An image config's JSON, read as `json.Unmarshal` reads it into a `DockerOCIImage`.
    /// Fails with Go's message.
    pub fn from_json(text: &[u8]) -> Result<Image, Vec<u8>> {
        let v = json::parse(text)?;
        let mut image = Image::default();
        let mut r = Reader { saved: None };
        match r.image(&v, &mut image) {
            Ok(()) => match r.saved {
                Some(e) => Err(e.into_bytes()),
                None => Ok(image),
            },
            Err(Abort(e)) => Err(e),
        }
    }

    /// The config as `json.Marshal` writes a `DockerOCIImage`: fields in Go's order, empty
    /// ones left out where Go leaves them out, maps sorted by key.
    pub fn to_json(&self) -> Result<String, Vec<u8>> {
        let mut o = Obj::new();
        if let Some(t) = &self.created {
            o.raw("created", &time_json(t)?);
        }
        o.string_nonempty("author", &self.author);
        let p = &self.platform;
        o.string("architecture", &p.architecture);
        o.string("os", &p.os);
        o.string_nonempty("os.version", &p.os_version);
        o.strings_nonempty("os.features", &p.os_features);
        o.string_nonempty("variant", &p.variant);
        let mut rootfs = Obj::new();
        rootfs.string("type", &self.rootfs.kind);
        match &self.rootfs.diff_ids {
            Some(ids) => rootfs.strings("diff_ids", ids),
            None => rootfs.raw("diff_ids", "null"),
        }
        o.raw("rootfs", &rootfs.end());
        if !self.history.is_empty() {
            let mut list = String::from("[");
            for (i, h) in self.history.iter().enumerate() {
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
            o.raw("history", &list);
        }
        o.raw("config", &self.config.to_json());
        Ok(o.end())
    }
}

impl Config {
    fn to_json(&self) -> String {
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

fn time_json(t: &Time) -> Result<String, Vec<u8>> {
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
