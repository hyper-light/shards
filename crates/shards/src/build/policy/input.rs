//! What a policy is asked about, as buildx v0.37.1 tells it (policy/types.go,
//! policy/validate.go sourceToInput): a source, in Go's JSON (fields in their declared
//! order, the empty ones left out), and the parts of it not known yet, which the
//! source's metadata answers.

use std::collections::BTreeMap;

use shards_dockerfile::platform::{self, Platform};
use shards_image::reference::Reference;

use super::{Meta, Source};

/// A JSON document as encoding/json writes Go's values: an object's fields in the order
/// written, a map's in key order.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// `json.MarshalIndent(v, "", "  ")`.
    pub fn indented(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out
    }

    fn write(&self, out: &mut String, depth: usize) {
        let pad = |out: &mut String, d: usize| {
            out.push('\n');
            for _ in 0..d {
                out.push_str("  ");
            }
        };
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Int(i) => out.push_str(&i.to_string()),
            Json::Str(s) => go_string(out, s),
            Json::Arr(a) if a.is_empty() => out.push_str("[]"),
            Json::Obj(o) if o.is_empty() => out.push_str("{}"),
            Json::Arr(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    pad(out, depth + 1);
                    v.write(out, depth + 1);
                }
                pad(out, depth);
                out.push(']');
            }
            Json::Obj(o) => {
                out.push('{');
                for (i, (k, v)) in o.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    pad(out, depth + 1);
                    go_string(out, k);
                    out.push_str(": ");
                    v.write(out, depth + 1);
                }
                pad(out, depth);
                out.push('}');
            }
        }
    }

    /// The document as a Rego value, as ast.InterfaceToValue makes one of it.
    pub fn value(&self) -> shards_rego::value::Value {
        use shards_rego::value::Value;
        match self {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(*b),
            Json::Int(i) => Value::int(*i),
            Json::Str(s) => Value::string(s.as_str()),
            Json::Arr(a) => Value::array(a.iter().map(Json::value).collect()),
            Json::Obj(o) => Value::object(
                o.iter()
                    .map(|(k, v)| (Value::string(k.as_str()), v.value()))
                    .collect(),
            ),
        }
    }
}

/// encoding/json's string: `<`, `>`, `&`, U+2028 and U+2029 escaped, as are control
/// characters, `\n`, `\r` and `\t` by name.
fn go_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A struct's fields as encoding/json writes them with `omitempty`: those not empty, in
/// order.
#[derive(Default)]
struct Fields(Vec<(String, Json)>);

impl Fields {
    fn str(mut self, k: &str, v: &str) -> Self {
        if !v.is_empty() {
            self.0.push((k.into(), Json::Str(v.into())));
        }
        self
    }

    fn flag(mut self, k: &str, v: bool) -> Self {
        if v {
            self.0.push((k.into(), Json::Bool(true)));
        }
        self
    }

    fn strs(mut self, k: &str, v: &[String]) -> Self {
        if !v.is_empty() {
            self.0.push((
                k.into(),
                Json::Arr(v.iter().map(|s| Json::Str(s.clone())).collect()),
            ));
        }
        self
    }

    fn map(mut self, k: &str, v: &BTreeMap<String, String>) -> Self {
        if !v.is_empty() {
            let o = v.iter().map(|(a, b)| (a.clone(), Json::Str(b.clone()))).collect();
            self.0.push((k.into(), Json::Obj(o)));
        }
        self
    }

    fn json(mut self, k: &str, v: Option<Json>) -> Self {
        if let Some(v) = v {
            self.0.push((k.into(), v));
        }
        self
    }

    fn done(self) -> Json {
        Json::Obj(self.0)
    }
}

/// `Env`: what the build says of itself.
#[derive(Debug, Clone, Default)]
pub struct Env {
    /// Each build argument, `None` for one given without a value.
    pub args: BTreeMap<String, Option<String>>,
    pub labels: BTreeMap<String, String>,
    pub filename: String,
    pub target: String,
    pub caps_request: bool,
    pub depth: i64,
}

impl Env {
    fn json(&self) -> Json {
        let mut f = Fields::default();
        if !self.args.is_empty() {
            let args = self
                .args
                .iter()
                .map(|(k, v)| (k.clone(), v.as_ref().map_or(Json::Null, |v| Json::Str(v.clone()))))
                .collect();
            f.0.push(("args".into(), Json::Obj(args)));
        }
        let mut f = f
            .map("labels", &self.labels)
            .str("filename", &self.filename)
            .str("target", &self.target)
            .flag("capsRequest", self.caps_request);
        // `depth` has no omitempty.
        f.0.push(("depth".into(), Json::Int(self.depth)));
        f.done()
    }
}

/// `Image`: a `docker-image://` source.
#[derive(Debug, Clone, Default)]
pub struct Image {
    pub reference: String,
    pub host: String,
    pub repo: String,
    pub full_repo: String,
    pub tag: String,
    pub platform: String,
    pub os: String,
    pub arch: String,
    pub variant: String,
    pub is_canonical: bool,
    pub checksum: String,
    pub created: String,
    pub env: Vec<String>,
    pub labels: BTreeMap<String, String>,
    pub user: String,
    pub volumes: Vec<String>,
    pub working_dir: String,
}

impl Image {
    pub fn json(&self) -> Json {
        Fields::default()
            .str("ref", &self.reference)
            .str("host", &self.host)
            .str("repo", &self.repo)
            .str("fullRepo", &self.full_repo)
            .str("tag", &self.tag)
            .str("platform", &self.platform)
            .str("os", &self.os)
            .str("arch", &self.arch)
            .str("variant", &self.variant)
            .flag("isCanonical", self.is_canonical)
            .str("checksum", &self.checksum)
            .str("createdTime", &self.created)
            .strs("env", &self.env)
            .map("labels", &self.labels)
            .str("user", &self.user)
            .strs("volumes", &self.volumes)
            .str("workingDir", &self.working_dir)
            .done()
    }
}

/// `Input`: the source, and the build's own `Env`.
#[derive(Debug, Clone, Default)]
pub struct Input {
    pub env: Env,
    pub local: Option<String>,
    pub image: Option<Image>,
    /// The fields not known yet, as `input.`-less refs (`image.checksum`).
    pub unknowns: Vec<String>,
}

impl Input {
    pub fn json(&self) -> Json {
        Fields::default()
            .json("env", Some(self.env.json()))
            .json(
                "local",
                self.local
                    .as_ref()
                    .map(|name| Fields::default().str("name", name).done()),
            )
            .json("image", self.image.as_ref().map(Image::json))
            .done()
    }

    /// `Input.Unknowns`: the refs not known yet, each from `input`.
    pub fn unknown_refs(&self) -> Vec<String> {
        self.unknowns.iter().map(|u| format!("input.{u}")).collect()
    }
}

/// The fields of an image's config a policy may ask for.
const CONFIG_FIELDS: [&str; 5] = ["labels", "user", "volumes", "workingDir", "env"];

/// `sourceToInput`: the input for `source` as its metadata `meta` tells it, for
/// `platform`, and what it leaves unknown.
pub fn of_source(source: &Source, meta: &Meta, wanted: Option<&Platform>) -> Result<Input, String> {
    let mut inp = Input::default();
    let Some((scheme, rest)) = source.identifier.split_once("://") else {
        return Err(format!("invalid source identifier: {}", source.identifier));
    };
    match scheme {
        "docker-image" => {
            let r = Reference::parse_normalized(rest)
                .map_err(|e| format!("failed to parse image source reference: {e}"))?;
            let mut img = Image {
                reference: r.to_string(),
                host: r.domain.clone(),
                repo: r.familiar_name(),
                full_repo: r.name(),
                ..Image::default()
            };
            if let Some(d) = &r.digest {
                img.checksum = d.to_string();
                img.is_canonical = true;
            }
            if let Some(t) = &r.tag {
                img.tag = t.clone();
            }
            let p = wanted.ok_or("platform required for image source")?;
            img.platform = String::from_utf8_lossy(&platform::format(p)).into_owned();
            img.os = String::from_utf8_lossy(&p.os).into_owned();
            img.arch = String::from_utf8_lossy(&p.architecture).into_owned();
            img.variant = String::from_utf8_lossy(&p.variant).into_owned();
            let config = |f: &str| format!("image.{f}");
            match &meta.image {
                None => {
                    if !img.is_canonical {
                        inp.unknowns.push("image.checksum".into());
                    }
                    inp.unknowns.extend(CONFIG_FIELDS.iter().map(|f| config(f)));
                    inp.unknowns.extend(
                        ["image.hasProvenance", "image.provenance", "image.signatures"].map(String::from),
                    );
                }
                Some(m) => {
                    img.checksum = m.digest.clone();
                    match &m.config {
                        Some(cfg) => config_fields(&mut img, cfg)?,
                        None => inp.unknowns.extend(CONFIG_FIELDS.iter().map(|f| config(f))),
                    }
                    inp.unknowns.extend(
                        ["image.hasProvenance", "image.provenance", "image.signatures"].map(String::from),
                    );
                }
            }
            inp.image = Some(img);
        }
        "local" => inp.local = Some(rest.to_string()),
        _ => return Err(format!("unsupported source scheme: {scheme}")),
    }
    Ok(inp)
}

/// The fields of an image's config (ocispecs.Image): when it was made, as RFC 3339
/// writes it, and what it runs with. Its volumes in name order, where Go's map gives
/// them in no order at all.
fn config_fields(img: &mut Image, raw: &[u8]) -> Result<(), String> {
    let doc: serde_json::Value =
        serde_json::from_slice(raw).map_err(|e| format!("failed to unmarshal image config: {e}"))?;
    if let Some(created) = doc.get("created").and_then(serde_json::Value::as_str) {
        let t = shards_dockerfile::go::parse_rfc3339(created.as_bytes()).map_err(|e| {
            format!(
                "failed to unmarshal image config: {}",
                String::from_utf8_lossy(&e)
            )
        })?;
        img.created = rfc3339(&t);
    }
    let Some(c) = doc.get("config") else {
        return Ok(());
    };
    img.env = c
        .get("Env")
        .and_then(serde_json::Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    if let Some(l) = c.get("Labels").and_then(serde_json::Value::as_object) {
        img.labels = l
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
            .collect();
    }
    img.user = c
        .get("User")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    if let Some(v) = c.get("Volumes").and_then(serde_json::Value::as_object) {
        img.volumes = v.keys().cloned().collect();
    }
    img.working_dir = c
        .get("WorkingDir")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(())
}

/// `Time.Format(time.RFC3339)`: to the second, `Z` for UTC, else the offset.
fn rfc3339(t: &shards_dockerfile::go::Time) -> String {
    let mut whole = *t;
    whole.nanosecond = 0;
    whole.rfc3339_nano().unwrap_or_default()
}
