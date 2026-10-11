//! CDI devices for `RUN --device` (D96): the Container Device Interface's specs (v1.1.0,
//! tags.cncf.io/container-device-interface, BuildKit v0.28.1's) read from their
//! directories as its cache reads them, a step's devices resolved and granted as
//! BuildKit's CDI manager and `ValidateEntitlements` resolve and grant them, and the
//! edits they make merged in CDI's order: each device's spec's own once, then the
//! device's.
//!
//! A step is a microVM's, so the edits land in it as a microVM can carry them: the
//! environment and groups as they are; a host path mounted as a read-only snapshot of it
//! (writable where its options say, its writes kept to the step); a device node made in
//! the step's `/dev` from the guest's own device of that path, a host's numbers meaning
//! nothing in a guest. What only a host can do, a hook it runs beside the container, a
//! network interface it moves in, an Intel RDT class, is refused by name.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

const CLASS: &str = "org.mobyproject.buildkit.device.class";
const AUTO_ALLOW: &str = "org.mobyproject.buildkit.device.autoallow";

/// The CDI spec versions a reader takes (specs-go version.go), oldest first.
const VERSIONS: [&str; 10] = [
    "0.1.0", "0.2.0", "0.3.0", "0.4.0", "0.5.0", "0.6.0", "0.7.0", "0.8.0", "1.0.0", "1.1.0",
];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    #[serde(rename = "cdiVersion")]
    version: String,
    kind: String,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
    #[serde(default)]
    devices: Vec<Device>,
    #[serde(default, rename = "containerEdits")]
    edits: RawEdits,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Device {
    name: String,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
    #[serde(default, rename = "containerEdits")]
    edits: RawEdits,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawEdits {
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    device_nodes: Vec<Node>,
    #[serde(default)]
    net_devices: Vec<NetDevice>,
    #[serde(default)]
    hooks: Vec<Hook>,
    #[serde(default)]
    mounts: Vec<Mount>,
    #[serde(default)]
    intel_rdt: Option<serde_json::Value>,
    #[serde(default, rename = "additionalGids")]
    additional_gids: Vec<u32>,
}

/// A device node a device brings: its path in the container, the path whose device it is
/// (`hostPath`, else the same), and what it says of its kind, numbers, mode and owner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Node {
    pub path: String,
    #[serde(default)]
    pub host_path: String,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub major: i64,
    #[serde(default)]
    pub minor: i64,
    #[serde(default)]
    pub file_mode: Option<u32>,
    #[serde(default)]
    pub permissions: String,
    #[serde(default)]
    pub uid: Option<u32>,
    #[serde(default)]
    pub gid: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Mount {
    pub host_path: String,
    pub container_path: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default, rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Hook {
    hook_name: String,
    path: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    timeout: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct NetDevice {
    host_interface_name: String,
    name: String,
}

/// What a step's devices bring it, merged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edits {
    pub env: Vec<String>,
    pub nodes: Vec<Node>,
    pub mounts: Vec<Mount>,
    pub gids: Vec<u32>,
    /// What only a host can do, said as a refusal: hooks, network interfaces, Intel RDT.
    pub host_only: Vec<String>,
}

/// A spec read, its devices by name.
#[derive(Debug, Clone)]
struct Loaded {
    spec: Spec,
    path: PathBuf,
}

/// The devices of the spec directories, as CDI's cache holds them: each qualified name,
/// the spec it is in and its index there; a name two specs of one directory give is
/// neither's, one a later directory gives is the later's.
#[derive(Debug, Default)]
pub struct Registry {
    specs: Vec<Loaded>,
    devices: BTreeMap<String, (usize, usize)>,
    /// The specs that could not be read, each with why: said where a device is wanted.
    pub errors: Vec<String>,
    auto_allowed: BTreeSet<String>,
}

/// The directories CDI specs are read from: `SHARDS_CDI_SPEC_DIRS` (`:`-separated), else
/// CDI's defaults, `/etc/cdi` then `/var/run/cdi`.
pub fn spec_dirs(env: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    match env("SHARDS_CDI_SPEC_DIRS") {
        Some(v) => v
            .split(':')
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .collect(),
        None => vec![PathBuf::from("/etc/cdi"), PathBuf::from("/var/run/cdi")],
    }
}

/// `validateVendorOrClassName`: a letter, then letters, digits, `_`, `-` and `.`, ending
/// with a letter or digit.
fn vendor_or_class(name: &str) -> Result<(), String> {
    let b = name.as_bytes();
    let (Some(first), Some(last)) = (b.first(), b.last()) else {
        return Err("empty name".into());
    };
    if !first.is_ascii_alphabetic() {
        return Err(format!("{name:?}, should start with letter"));
    }
    if let Some(c) = b
        .get(1..b.len().saturating_sub(1))
        .unwrap_or_default()
        .iter()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.')))
    {
        return Err(format!("invalid character '{}' in name {name:?}", char::from(*c)));
    }
    if !last.is_ascii_alphanumeric() {
        return Err(format!("{name:?}, should end with a letter or digit"));
    }
    Ok(())
}

/// `ValidateDeviceName`.
fn device_name(name: &str) -> Result<(), String> {
    let b = name.as_bytes();
    let (Some(first), Some(last)) = (b.first(), b.last()) else {
        return Err("invalid (empty) device name".into());
    };
    if !first.is_ascii_alphanumeric() {
        return Err(format!(
            "invalid class {name:?}, should start with a letter or digit"
        ));
    }
    if b.len() == 1 {
        return Ok(());
    }
    if let Some(c) = b
        .get(1..b.len() - 1)
        .unwrap_or_default()
        .iter()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b':')))
    {
        return Err(format!(
            "invalid character '{}' in device name {name:?}",
            char::from(*c)
        ));
    }
    if !last.is_ascii_alphanumeric() {
        return Err(format!(
            "invalid name {name:?}, should end with a letter or digit"
        ));
    }
    Ok(())
}

/// `ContainerEdits.Validate`.
fn check_edits(e: &RawEdits) -> Result<(), String> {
    for v in &e.env {
        if v.find('=').is_none_or(|i| i == 0) {
            return Err(format!(
                "invalid container edits: invalid environment variable {v:?}"
            ));
        }
    }
    for d in &e.device_nodes {
        if d.path.is_empty() {
            return Err("invalid (empty) device path".into());
        }
        if !matches!(d.kind.as_str(), "" | "b" | "c" | "u" | "p") {
            return Err(format!("device {:?}: invalid type {:?}", d.path, d.kind));
        }
        if d.permissions.chars().any(|c| !matches!(c, 'r' | 'w' | 'm')) {
            return Err(format!(
                "device {:?}: invalid permissions {:?}",
                d.path, d.permissions
            ));
        }
    }
    for h in &e.hooks {
        if !matches!(
            h.hook_name.as_str(),
            "prestart" | "createRuntime" | "createContainer" | "startContainer" | "poststart" | "poststop"
        ) {
            return Err(format!("invalid hook name {:?}", h.hook_name));
        }
        if h.path.is_empty() {
            return Err(format!("invalid hook {:?} with empty path", h.hook_name));
        }
    }
    for m in &e.mounts {
        if m.host_path.is_empty() {
            return Err("invalid mount, empty host path".into());
        }
        if m.container_path.is_empty() {
            return Err("invalid mount, empty container path".into());
        }
    }
    Ok(())
}

impl RawEdits {
    fn is_empty(&self) -> bool {
        self.env.is_empty()
            && self.device_nodes.is_empty()
            && self.net_devices.is_empty()
            && self.hooks.is_empty()
            && self.mounts.is_empty()
            && self.intel_rdt.is_none()
            && self.additional_gids.is_empty()
    }
}

/// The least version `spec` needs (`MinimumRequiredVersion`): an index into [`VERSIONS`].
fn required_version(spec: &Spec) -> usize {
    let all_edits: Vec<&RawEdits> = std::iter::once(&spec.edits)
        .chain(spec.devices.iter().map(|d| &d.edits))
        .collect();
    let rdt_new = |e: &RawEdits| {
        e.intel_rdt.as_ref().is_some_and(|r| {
            r.get("schemata").is_some_and(|s| !s.is_null())
                || r.get("enableMonitoring") == Some(&serde_json::Value::Bool(true))
        })
    };
    if all_edits.iter().any(|e| rdt_new(e) || !e.net_devices.is_empty()) {
        return 9;
    }
    if all_edits
        .iter()
        .any(|e| e.intel_rdt.is_some() || !e.additional_gids.is_empty())
    {
        return 6;
    }
    let class = spec.kind.split_once('/').map_or("", |(_, c)| c);
    if !spec.annotations.is_empty()
        || spec.devices.iter().any(|d| !d.annotations.is_empty())
        || class.contains('.')
    {
        return 5;
    }
    if spec
        .devices
        .iter()
        .any(|d| d.name.as_bytes().first().is_some_and(u8::is_ascii_digit))
        || all_edits
            .iter()
            .any(|e| e.device_nodes.iter().any(|n| !n.host_path.is_empty()))
    {
        return 4;
    }
    if all_edits
        .iter()
        .any(|e| e.mounts.iter().any(|m| !m.kind.is_empty()))
    {
        return 3;
    }
    2
}

/// A spec's bytes, as CDI's `ParseSpec` reads them: YAML (JSON among it) by sigs.k8s.io/yaml
/// v1.4.0, its go-yaml v2 resolving each plain scalar by YAML 1.1's rules (`0660` octal,
/// `yes` true, `~` null), a duplicate key refused (`UnmarshalStrict`), then a number or a
/// bool coerced to a string wherever the field it fills is one (`convertToJSONableObject`).
fn parse(bytes: &[u8]) -> Result<Spec, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let mut tree = Tree::default();
    yaml_rust2::parser::Parser::new_from_str(text)
        .load(&mut tree, false)
        .map_err(|e| e.to_string())?;
    if let Some(e) = tree.error {
        return Err(e);
    }
    let root = tree.root.ok_or("no Spec data")?;
    serde_json::from_value(coerce(root, None)).map_err(|e| e.to_string())
}

/// The YAML document built from its events, as go-yaml v2 decodes one into Go values.
#[derive(Default)]
struct Tree {
    stack: Vec<(Frame, usize)>,
    anchors: std::collections::HashMap<usize, serde_json::Value>,
    root: Option<serde_json::Value>,
    error: Option<String>,
}

enum Frame {
    Seq(Vec<serde_json::Value>),
    Map(serde_json::Map<String, serde_json::Value>, Option<String>),
}

impl Tree {
    fn put(&mut self, v: serde_json::Value, anchor: usize) {
        if anchor != 0 {
            self.anchors.insert(anchor, v.clone());
        }
        match self.stack.last_mut() {
            None => {
                if self.root.is_none() {
                    self.root = Some(v);
                }
            }
            Some((Frame::Seq(items), _)) => items.push(v),
            Some((Frame::Map(m, key), _)) => match key.take() {
                None => match key_string(&v) {
                    Ok(k) if k == "<<" => self.error = Some("YAML merge keys are not read".into()),
                    Ok(k) => *key = Some(k),
                    Err(e) => self.error = Some(e),
                },
                Some(k) => {
                    if m.contains_key(&k) {
                        self.error = Some(format!("key {k:?} already set in map"));
                    }
                    m.insert(k, v);
                }
            },
        }
    }
}

impl yaml_rust2::parser::EventReceiver for Tree {
    fn on_event(&mut self, ev: yaml_rust2::Event) {
        use yaml_rust2::Event;
        match ev {
            Event::SequenceStart(anchor, _) => self.stack.push((Frame::Seq(Vec::new()), anchor)),
            Event::MappingStart(anchor, _) => self
                .stack
                .push((Frame::Map(serde_json::Map::new(), None), anchor)),
            Event::SequenceEnd | Event::MappingEnd => {
                if let Some((frame, anchor)) = self.stack.pop() {
                    let v = match frame {
                        Frame::Seq(items) => serde_json::Value::Array(items),
                        Frame::Map(m, _) => serde_json::Value::Object(m),
                    };
                    self.put(v, anchor);
                }
            }
            Event::Scalar(text, style, anchor, tag) => {
                let as_str = tag
                    .as_ref()
                    .is_some_and(|t| t.handle == "!!" && t.suffix == "str");
                let v = if style == yaml_rust2::scanner::TScalarStyle::Plain && !as_str {
                    match resolve(&text) {
                        Ok(v) => v,
                        Err(e) => {
                            self.error = Some(e);
                            serde_json::Value::Null
                        }
                    }
                } else {
                    serde_json::Value::String(text)
                };
                self.put(v, anchor);
            }
            Event::Alias(id) => match self.anchors.get(&id).cloned() {
                Some(v) => self.put(v, 0),
                None => self.error = Some(format!("unknown anchor {id}")),
            },
            _ => {}
        }
    }
}

/// A map key as sigs.k8s.io/yaml makes a JSON key of one.
fn key_string(v: &serde_json::Value) -> Result<String, String> {
    match v {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        serde_json::Value::Bool(b) => Ok(b.to_string()),
        other => Err(format!("unsupported map key {other}")),
    }
}

/// go-yaml v2's `resolve` of a plain scalar: null, a bool, an int (Go's `ParseInt` with base
/// 0, `_` dropped: `0x`, `0o`, `0b`, a leading `0` octal), a float, else a string. A float
/// JSON cannot hold (NaN, infinities) fails, as Go's JSON marshal of one does.
fn resolve(s: &str) -> Result<serde_json::Value, String> {
    use serde_json::Value;
    match s {
        "y" | "Y" | "yes" | "Yes" | "YES" | "true" | "True" | "TRUE" | "on" | "On" | "ON" => {
            return Ok(Value::Bool(true));
        }
        "n" | "N" | "no" | "No" | "NO" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF" => {
            return Ok(Value::Bool(false));
        }
        "" | "~" | "null" | "Null" | "NULL" => return Ok(Value::Null),
        ".nan" | ".NaN" | ".NAN" | ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" | "-.inf"
        | "-.Inf" | "-.INF" => return Err(format!("json: unsupported value: {s}")),
        _ => {}
    }
    let first = s.as_bytes().first().copied().unwrap_or(0);
    if first == b'.' {
        if let Some(n) = s.parse::<f64>().ok().and_then(serde_json::Number::from_f64) {
            return Ok(Value::Number(n));
        }
    } else if first.is_ascii_digit() || first == b'+' || first == b'-' {
        let plain = s.replace('_', "");
        if let Some(i) = go_parse_int(&plain) {
            return Ok(match i {
                Int::Signed(v) => v.into(),
                Int::Unsigned(v) => v.into(),
            });
        }
        let float = regex::Regex::new(r"^[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?$")
            .map_err(|e| e.to_string())?;
        if float.is_match(&plain)
            && let Some(n) = plain.parse::<f64>().ok().and_then(serde_json::Number::from_f64)
        {
            return Ok(Value::Number(n));
        }
    }
    Ok(Value::String(s.to_string()))
}

enum Int {
    Signed(i64),
    Unsigned(u64),
}

/// Go's `strconv.ParseInt(s, 0, 64)`, then `ParseUint(s, 0, 64)`.
fn go_parse_int(s: &str) -> Option<Int> {
    let (neg, body) = match s.as_bytes().first() {
        Some(b'-') => (true, s.get(1..)?),
        Some(b'+') => (false, s.get(1..)?),
        _ => (false, s),
    };
    let lower = body.to_ascii_lowercase();
    let (radix, digits) = if let Some(d) = lower.strip_prefix("0x") {
        (16, d.to_string())
    } else if let Some(d) = lower.strip_prefix("0b") {
        (2, d.to_string())
    } else if let Some(d) = lower.strip_prefix("0o") {
        (8, d.to_string())
    } else if lower.len() > 1 && lower.starts_with('0') {
        (8, lower.get(1..)?.to_string())
    } else {
        (10, lower)
    };
    if digits.is_empty() {
        return None;
    }
    let magnitude = u64::from_str_radix(&digits, radix).ok()?;
    if neg {
        if magnitude <= i64::MAX as u64 + 1 {
            return Some(Int::Signed(0i64.wrapping_sub_unsigned(magnitude)));
        }
        return None;
    }
    match i64::try_from(magnitude) {
        Ok(v) => Some(Int::Signed(v)),
        Err(_) => Some(Int::Unsigned(magnitude)),
    }
}

/// The spec's numeric and bool fields; every other scalar fills a string.
const NOT_STRINGS: [&str; 8] = [
    "major",
    "minor",
    "fileMode",
    "uid",
    "gid",
    "additionalGids",
    "timeout",
    "enableMonitoring",
];

/// A number or bool coerced to a string where the field it fills is one, as
/// sigs.k8s.io/yaml's `convertToJSONableObject` coerces them (`strconv` decimal and `g`).
fn coerce(v: serde_json::Value, key: Option<&str>) -> serde_json::Value {
    use serde_json::Value;
    let keeps = key.is_some_and(|k| NOT_STRINGS.contains(&k));
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| {
                    let c = coerce(v, Some(&k));
                    (k, c)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(|v| coerce(v, key)).collect()),
        Value::Number(n) if !keeps => Value::String(n.to_string()),
        Value::Bool(b) if !keeps => Value::String(b.to_string()),
        other => other,
    }
}

/// The spec at `path`, checked as CDI's `newSpec` checks one.
fn read(path: &Path) -> Result<Spec, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("failed to read CDI Spec {:?}: {e}", path.display()))?;
    let spec = parse(&bytes).map_err(|e| format!("failed to parse CDI Spec {:?}: {e}", path.display()))?;
    let invalid = |e: String| format!("invalid CDI Spec: {e}");
    let have = VERSIONS
        .iter()
        .position(|v| *v == spec.version)
        .ok_or_else(|| format!("invalid version {:?}", spec.version))?;
    let need = required_version(&spec);
    if need > have {
        return Err(format!(
            "the spec version must be at least v{}",
            VERSIONS.get(need).unwrap_or(&"")
        ));
    }
    let (vendor, class) = spec.kind.split_once('/').unwrap_or(("", &spec.kind));
    vendor_or_class(vendor).map_err(|e| invalid(format!("invalid vendor. {e}")))?;
    vendor_or_class(class).map_err(|e| invalid(format!("invalid class. {e}")))?;
    check_edits(&spec.edits).map_err(invalid)?;
    let mut names = BTreeSet::new();
    for d in &spec.devices {
        device_name(&d.name).map_err(|e| invalid(format!("failed add device {:?}: {e}", d.name)))?;
        if d.edits.is_empty() {
            return Err(invalid(format!(
                "failed add device {:?}: invalid device, empty device edits",
                d.name
            )));
        }
        check_edits(&d.edits).map_err(|e| invalid(format!("failed add device {:?}: {e}", d.name)))?;
        if !names.insert(d.name.clone()) {
            return Err(invalid(format!("invalid spec, multiple device {:?}", d.name)));
        }
    }
    if names.is_empty() {
        return Err(invalid("invalid spec, no devices".into()));
    }
    Ok(spec)
}

impl Registry {
    /// The specs of `dirs`, as CDI's cache scans them: each directory's `.json` and
    /// `.yaml` files in name order, its subdirectories skipped, a later directory's
    /// device over an earlier's, two of one directory both dropped; `auto_allowed` the
    /// devices or kinds granted without `--allow` (BuildKit's `cdi.autoAllowed`).
    pub fn load(dirs: &[PathBuf], auto_allowed: &[String]) -> Registry {
        let mut r = Registry {
            auto_allowed: auto_allowed.iter().cloned().collect(),
            ..Registry::default()
        };
        let mut priority: BTreeMap<String, usize> = BTreeMap::new();
        let mut conflicts = BTreeSet::new();
        for (prio, dir) in dirs.iter().enumerate() {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut paths: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file() && matches!(p.extension().and_then(|e| e.to_str()), Some("json" | "yaml"))
                })
                .collect();
            paths.sort();
            for path in paths {
                let spec = match read(&path) {
                    Ok(s) => s,
                    Err(e) => {
                        r.errors.push(e);
                        continue;
                    }
                };
                let at = r.specs.len();
                for (i, d) in spec.devices.iter().enumerate() {
                    let qualified = format!("{}={}", spec.kind, d.name);
                    match priority.get(&qualified) {
                        Some(&p) if p > prio => continue,
                        Some(&p) if p == prio => {
                            r.errors.push(format!("conflicting device {qualified:?}"));
                            conflicts.insert(qualified);
                            continue;
                        }
                        _ => {}
                    }
                    priority.insert(qualified.clone(), prio);
                    r.devices.insert(qualified, (at, i));
                }
                r.specs.push(Loaded { spec, path });
            }
        }
        for c in conflicts {
            r.devices.remove(&c);
        }
        r
    }

    fn device(&self, name: &str) -> Option<(&Loaded, &Device)> {
        let &(s, d) = self.devices.get(name)?;
        let loaded = self.specs.get(s)?;
        Some((loaded, loaded.spec.devices.get(d)?))
    }

    /// A device's annotations: its spec's, then its own over them.
    fn annotations(&self, name: &str) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        if let Some((s, d)) = self.device(name) {
            out.extend(s.spec.annotations.clone());
            out.extend(d.annotations.clone());
        }
        out
    }

    fn auto_allows(&self, name: &str) -> bool {
        let kind = name.split_once('=').map_or(name, |(k, _)| k);
        self.auto_allowed.contains(name)
            || self.auto_allowed.contains(kind)
            || self
                .annotations(name)
                .get(AUTO_ALLOW)
                .is_some_and(|v| shards_cmdline::go::parse_bool(v).unwrap_or(false))
    }

    /// The devices `name` asks for (the manager's `parseDevice`): `VENDOR/CLASS=NAME` the
    /// one, `VENDOR/CLASS` the first of the kind, `VENDOR/CLASS=*` every one; otherwise, or
    /// where none is found, those whose class annotation is `name`. None found is refused
    /// unless the device is optional.
    pub fn find(&self, name: &str, optional: bool) -> Result<Vec<String>, String> {
        let (kind, dev) = name.split_once('=').unwrap_or((name, ""));
        let vendor = kind
            .split_once('/')
            .filter(|(v, c)| !v.is_empty() && !c.is_empty())
            .map(|(v, _)| v);
        let all: Vec<&String> = self.devices.keys().collect();
        let of_kind = |d: &&String| d.starts_with(&format!("{kind}="));
        let mut out: Vec<String> = Vec::new();
        if vendor.is_some() {
            match dev {
                "" => out.extend(all.iter().find(|d| of_kind(d)).map(|d| (*d).clone())),
                "*" => out.extend(all.iter().filter(|d| of_kind(d)).map(|d| (*d).clone())),
                _ => out.extend(all.iter().find(|d| d.as_str() == name).map(|d| (*d).clone())),
            }
        }
        if vendor.is_none() || out.is_empty() {
            out.extend(
                all.iter()
                    .filter(|d| self.annotations(d).get(CLASS).is_some_and(|c| c == name))
                    .map(|d| (*d).clone()),
            );
        }
        if out.is_empty() && !optional {
            return Err(format!("required device {name:?} is not registered"));
        }
        Ok(out)
    }

    /// The devices a step's requests come to, granted as BuildKit's `ValidateEntitlements`
    /// grants them: `--allow device` every one; `--allow device=NAME` that one,
    /// `device=NAME,alias=A` NAME where the step asks for A; else those auto-allowed.
    pub fn grant(
        &self,
        requests: &[(String, bool)],
        granted: Option<&shards_cmdline::buildflags::DevicesGrant>,
    ) -> Result<Vec<String>, String> {
        if granted.is_some_and(|g| g.all) {
            let mut out = Vec::new();
            for (name, optional) in requests {
                out.extend(self.find(name, *optional)?);
            }
            return Ok(dedup(out));
        }
        let mut allowed = Vec::new();
        let mut plain = Vec::new();
        for (name, optional) in requests {
            match granted
                .and_then(|g| g.devices.get(name))
                .filter(|n| !n.is_empty())
            {
                Some(real) => allowed.extend(self.find(real, *optional)?),
                None => plain.push((name.clone(), *optional)),
            }
        }
        let mut found = Vec::new();
        for (name, optional) in &plain {
            found.extend(self.find(name, *optional)?);
        }
        // Nothing left to check: the grants' own names are not looked up.
        if found.is_empty() {
            return Ok(dedup(allowed));
        }
        let mut grants = BTreeSet::new();
        for d in granted
            .map(|g| g.devices.keys().collect::<Vec<_>>())
            .unwrap_or_default()
        {
            grants.extend(self.find(d, false)?);
        }
        let mut forbidden = Vec::new();
        for d in dedup(found) {
            if grants.contains(&d) || self.auto_allows(&d) {
                allowed.push(d);
            } else {
                forbidden.push(d);
            }
        }
        match forbidden.as_slice() {
            [] => Ok(dedup(allowed)),
            [one] => Err(format!("device {one} is requested by the build but not allowed")),
            many => Err(format!(
                "devices {} are requested by the build but not allowed",
                many.join(", ")
            )),
        }
    }

    /// The edits of `devices`, in CDI's order (`InjectDevices`): each device's spec's own,
    /// the first time the spec is met, then the device's.
    pub fn edits(&self, devices: &[String]) -> Edits {
        let mut out = Edits::default();
        let mut specs_seen = BTreeSet::new();
        for name in devices {
            let Some(&(s, _)) = self.devices.get(name) else {
                continue;
            };
            let Some((spec, dev)) = self.device(name) else {
                continue;
            };
            if specs_seen.insert(s) {
                add(&mut out, &spec.spec.edits, &spec.path.display().to_string());
            }
            add(&mut out, &dev.edits, name);
        }
        out
    }
}

fn dedup(v: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    v.into_iter().filter(|d| seen.insert(d.clone())).collect()
}

fn add(out: &mut Edits, e: &RawEdits, whose: &str) {
    out.env.extend(e.env.iter().cloned());
    for n in &e.device_nodes {
        out.nodes.retain(|o| o.path != n.path);
        out.nodes.push(n.clone());
    }
    for m in &e.mounts {
        out.mounts.retain(|o| o.container_path != m.container_path);
        out.mounts.push(m.clone());
    }
    out.gids.extend(e.additional_gids.iter().copied());
    for h in &e.hooks {
        let _ = (&h.args, &h.env, &h.timeout);
        out.host_only.push(format!(
            "{whose}: a {} hook ({}), which runs on a container's host",
            h.hook_name, h.path
        ));
    }
    for n in &e.net_devices {
        out.host_only.push(format!(
            "{whose}: the host's network interface {:?} moved in as {:?}",
            n.host_interface_name, n.name
        ));
    }
    if e.intel_rdt.is_some() {
        out.host_only.push(format!(
            "{whose}: an Intel RDT class, which a host's resctrl holds"
        ));
    }
}

/// `AddMultipleProcessEnv`: each `KEY=value` in place of the variable of that key, else
/// after the rest.
pub fn merge_env(env: &mut Vec<Vec<u8>>, add: &[String]) {
    for kv in add {
        let key = kv.split_once('=').map_or(kv.as_str(), |(k, _)| k);
        let mut entry = kv.clone().into_bytes();
        match env
            .iter_mut()
            .find(|e| e.split(|&b| b == b'=').next() == Some(key.as_bytes()))
        {
            Some(e) => std::mem::swap(e, &mut entry),
            None => env.push(entry),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(files: &[(&str, &str)]) -> shards_testdir::TempDir {
        let d = shards_testdir::TempDir::new("cdi").unwrap();
        for (name, text) in files {
            std::fs::write(d.join(name), text).unwrap();
        }
        d
    }

    const GPU: &str = "cdiVersion: \"0.6.0\"\nkind: vendor.com/gpu\nannotations:\n  org.mobyproject.buildkit.device.class: gpu\ncontainerEdits:\n  env: [SPEC=1]\ndevices:\n  - name: \"0\"\n    containerEdits:\n      env: [GPU=0]\n      deviceNodes: [{path: /dev/fuse}]\n  - name: \"1\"\n    containerEdits:\n      env: [GPU=1]\n";

    /// Names resolve as BuildKit's CDI manager resolves them, and grants as its
    /// `ValidateEntitlements` grants them, in its words.
    #[test]
    fn devices_resolve_and_are_granted_as_buildkits() {
        let d = dir_with(&[("gpu.yaml", GPU), ("bad.json", "{\"cdiVersion\":\"9.9\"}")]);
        let r = Registry::load(&[d.to_path_buf()], &[]);
        assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
        assert_eq!(r.find("vendor.com/gpu", false).unwrap(), ["vendor.com/gpu=0"]);
        assert_eq!(
            r.find("vendor.com/gpu=*", false).unwrap(),
            ["vendor.com/gpu=0", "vendor.com/gpu=1"]
        );
        assert_eq!(r.find("vendor.com/gpu=1", false).unwrap(), ["vendor.com/gpu=1"]);
        assert_eq!(
            r.find("gpu", false).unwrap(),
            ["vendor.com/gpu=0", "vendor.com/gpu=1"]
        );
        assert_eq!(
            r.find("vendor.com/x", false).unwrap_err(),
            "required device \"vendor.com/x\" is not registered"
        );
        assert!(r.find("vendor.com/x", true).unwrap().is_empty());
        let req = |n: &str| vec![(n.to_string(), false)];
        assert_eq!(
            r.grant(&req("vendor.com/gpu=1"), None).unwrap_err(),
            "device vendor.com/gpu=1 is requested by the build but not allowed"
        );
        let all = shards_cmdline::buildflags::DevicesGrant {
            all: true,
            devices: BTreeMap::new(),
        };
        assert_eq!(r.grant(&req("gpu"), Some(&all)).unwrap().len(), 2);
        let one = shards_cmdline::buildflags::DevicesGrant {
            all: false,
            devices: [("vendor.com/gpu=1".to_string(), String::new())].into(),
        };
        assert_eq!(
            r.grant(&req("vendor.com/gpu=1"), Some(&one)).unwrap(),
            ["vendor.com/gpu=1"]
        );
        let alias = shards_cmdline::buildflags::DevicesGrant {
            all: false,
            devices: [("mine".to_string(), "vendor.com/gpu=1".to_string())].into(),
        };
        assert_eq!(r.grant(&req("mine"), Some(&alias)).unwrap(), ["vendor.com/gpu=1"]);
        // The spec's edits once, before each device's.
        let e = r.edits(&["vendor.com/gpu=0".into(), "vendor.com/gpu=1".into()]);
        assert_eq!(e.env, ["SPEC=1", "GPU=0", "GPU=1"]);
        assert_eq!(e.nodes.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A later directory's device over an earlier's; two of one directory, neither.
    #[test]
    fn later_directories_win_and_conflicts_drop() {
        let spec = |env: &str| {
            format!(
                "{{\"cdiVersion\":\"0.3.0\",\"kind\":\"v.com/c\",\"devices\":[{{\"name\":\"d\",\"containerEdits\":{{\"env\":[\"{env}\"]}}}}]}}"
            )
        };
        let a = dir_with(&[("a.json", &spec("A=1"))]);
        let b = dir_with(&[("b.json", &spec("B=1"))]);
        let r = Registry::load(&[a.to_path_buf(), b.to_path_buf()], &[]);
        assert_eq!(r.edits(&["v.com/c=d".into()]).env, ["B=1"]);
        let c = dir_with(&[("x.json", &spec("X=1")), ("y.json", &spec("Y=1"))]);
        let r = Registry::load(&[c.to_path_buf()], &[]);
        assert!(r.find("v.com/c=d", false).is_err());
        for d in [a, b, c] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// Specs refused as CDI refuses them.
    #[test]
    fn specs_are_checked_as_cdi_checks_them() {
        for (text, why) in [
            (
                "{\"cdiVersion\":\"0.3.0\",\"kind\":\"v.com/c\",\"devices\":[{\"name\":\"d\",\"containerEdits\":{\"env\":[\"A=1\"]}}],\"x\":1}",
                "unknown field",
            ),
            (
                "{\"cdiVersion\":\"0.3.0\",\"kind\":\"v.com/c\",\"devices\":[]}",
                "no devices",
            ),
            (
                "{\"cdiVersion\":\"0.3.0\",\"kind\":\"1v/c\",\"devices\":[{\"name\":\"d\",\"containerEdits\":{\"env\":[\"A=1\"]}}]}",
                "should start with letter",
            ),
            (
                "{\"cdiVersion\":\"0.3.0\",\"kind\":\"v.com/c\",\"devices\":[{\"name\":\"d\",\"containerEdits\":{\"env\":[\"=1\"]}}]}",
                "invalid environment variable",
            ),
            (
                "{\"cdiVersion\":\"0.3.0\",\"kind\":\"v.com/c\",\"devices\":[{\"name\":\"d\",\"containerEdits\":{\"additionalGids\":[5]}}]}",
                "at least v0.7.0",
            ),
            (
                "{\"cdiVersion\":\"0.3.0\",\"kind\":\"v.com/c\",\"devices\":[{\"name\":\"d\",\"containerEdits\":{}}]}",
                "empty device edits",
            ),
        ] {
            let d = dir_with(&[("s.json", text)]);
            let r = Registry::load(&[d.to_path_buf()], &[]);
            assert!(r.errors.iter().any(|e| e.contains(why)), "{why}: {:?}", r.errors);
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn env_replaces_its_key_or_follows() {
        let mut env = vec![b"A=1".to_vec(), b"B=2".to_vec()];
        merge_env(&mut env, &["B=3".into(), "C=4".into()]);
        assert_eq!(env, [b"A=1".to_vec(), b"B=3".to_vec(), b"C=4".to_vec()]);
    }

    /// Each spec of testdata/cdi-specs.json read as CDI's `ParseSpec` read it
    /// (`scripts/cdi/generate`, testdata/cdi-parsed.json): the same spec, field for field,
    /// or an error where it errs.
    #[test]
    fn specs_parse_as_cdis_parse_spec_parses_them() {
        let texts: Vec<String> = serde_json::from_str(include_str!("testdata/cdi-specs.json")).unwrap();
        let want: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("testdata/cdi-parsed.json")).unwrap();
        assert_eq!(texts.len(), want.len());
        for (text, w) in texts.iter().zip(&want) {
            let ours = parse(text.as_bytes());
            match w.get("spec") {
                Some(spec) => {
                    let theirs: Spec = serde_json::from_value(spec.clone()).unwrap();
                    assert_eq!(format!("{:?}", ours.unwrap()), format!("{theirs:?}"), "{text}");
                }
                None => assert!(ours.is_err(), "{text}: {ours:?}, not {}", w["error"]),
            }
        }
    }
}
