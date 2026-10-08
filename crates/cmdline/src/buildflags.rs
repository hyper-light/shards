//! What `build`'s `--secret`, `--allow` and `--ulimit` values mean. They are written and
//! answered as buildx v0.37.1 reads them (util/buildflags/secrets.go and entitlements.go,
//! with docker/cli's opts/ulimit.go and go-units' ulimit.go for `--ulimit`), and the
//! secrets are found as BuildKit's store finds them (secretsprovider/store.go), held to
//! buildx's own answers by tests/buildx.rs (scripts/buildx/oracle_test.go). Where buildx
//! refuses or misbehaves for reasons of its own machinery, shards does better, each
//! difference listed there with its reason:
//! - a secret may be as large as a build step carries it, not 500 KiB, the cap BuildKit's
//!   gRPC session sets;
//! - a secret is read once, as the build starts, not each time a step asks, so that every
//!   step of one build sees one value, and a file changed or removed mid-build changes
//!   nothing; its bytes are held in memory alone, and wiped when dropped;
//! - `--ulimit as=` is taken: go-units leaves `as` out for the way Docker starts a
//!   container, and shards' builder guest sets it as it sets the rest.

use std::collections::BTreeMap;
use std::fmt;

use crate::go;

/// A `--secret`: its id, and where its value comes from, a file or a variable, if said.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Secret {
    pub id: String,
    pub file: String,
    pub env: String,
}

/// The `--secret` values, each a CSV record of `type`, `id`, `src` (or `source`) and
/// `env`, keys in any case; empty ones are skipped. `type=env` reads `src` as the
/// variable's name.
/// An `--output`, as buildx's ExportEntry reads one (util/buildflags/export.go, v0.37.1):
/// its type, its destination, and its other attributes, keys lowercased.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Export {
    pub kind: String,
    pub dest: String,
    pub attrs: std::collections::BTreeMap<String, String>,
}

/// `--output`'s specs, empty ones skipped (ParseExports): `PATH` a local directory, `-`
/// a tar on stdout, else CSV fields `type=`, `dest=` and attributes.
pub fn parse_exports(specs: &[String]) -> Result<Vec<Export>, String> {
    let mut out = Vec::new();
    for spec in specs.iter().filter(|s| !s.is_empty()) {
        let fields = go::csv_fields(spec.as_bytes()).map_err(|e| String::from_utf8_lossy(&e).into_owned())?;
        let mut e = Export::default();
        let lone = fields.len() == 1
            && fields.first().is_some_and(|f| f == spec.as_bytes())
            && !spec.starts_with("type=");
        if lone {
            e.kind = if spec == "-" { "tar" } else { "local" }.into();
            e.dest.clone_from(spec);
        } else {
            for field in &fields {
                let field = String::from_utf8_lossy(field);
                let Some((k, v)) = field.split_once('=') else {
                    return Err(format!("invalid value {field}"));
                };
                match k.trim().to_lowercase().as_str() {
                    "type" => e.kind = v.to_string(),
                    "dest" => e.dest = v.to_string(),
                    key => {
                        e.attrs.insert(key.to_string(), v.to_string());
                    }
                }
            }
        }
        if e.kind.is_empty() {
            return Err("type is required for output".into());
        }
        out.push(e);
    }
    Ok(out)
}

/// A `--cache-from` or `--cache-to` entry: its backend and attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub kind: String,
    pub attrs: BTreeMap<String, String>,
}

/// One entry's text (`CacheOptionsEntry.UnmarshalText`): a lone field without `=` is a
/// registry reference; else CSV fields `type=` and attributes, keys lowercased.
fn cache_entry(text: &str) -> Result<CacheEntry, String> {
    let fields = go::csv_fields(text.as_bytes()).map_err(|e| String::from_utf8_lossy(&e).into_owned())?;
    if let [only] = fields.as_slice()
        && !only.contains(&b'=')
    {
        return Ok(CacheEntry {
            kind: "registry".into(),
            attrs: BTreeMap::from([("ref".into(), String::from_utf8_lossy(only).into_owned())]),
        });
    }
    let mut e = CacheEntry {
        kind: String::new(),
        attrs: BTreeMap::new(),
    };
    for field in &fields {
        let field = String::from_utf8_lossy(field);
        let Some((k, v)) = field.split_once('=') else {
            return Err(format!("invalid value {field}"));
        };
        match k.to_lowercase().as_str() {
            "type" => e.kind = v.to_string(),
            key => {
                e.attrs.insert(key.to_string(), v.to_string());
            }
        }
    }
    if e.kind.is_empty() {
        return Err(format!("type required for {}", go::quote(text)));
    }
    Ok(e)
}

/// `--cache-from` or `--cache-to`'s values, as buildx reads them (`ParseCacheEntry`, then
/// `CreateCaches`): empty ones skipped; one without `=` a CSV list of registry
/// references; a GitHub Actions entry given its token and URLs from the environment
/// (`ACTIONS_RUNTIME_TOKEN`, `ACTIONS_CACHE_URL`, `ACTIONS_RESULTS_URL`, with
/// `ACTIONS_CACHE_SERVICE_V2`) and dropped without them (`isActive`).
pub fn cache_entries(
    values: &[String],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<CacheEntry>, String> {
    let mut out = Vec::new();
    for v in values.iter().filter(|v| !v.is_empty()) {
        if v.contains('=') {
            out.push(cache_entry(v)?);
        } else {
            let fields =
                go::csv_fields(v.as_bytes()).map_err(|e| String::from_utf8_lossy(&e).into_owned())?;
            for f in fields {
                out.push(cache_entry(&String::from_utf8_lossy(&f))?);
            }
        }
    }
    for e in out.iter_mut().filter(|e| e.kind == "gha") {
        let v2 = e.attrs.get("version").cloned().or_else(|| {
            env("ACTIONS_CACHE_SERVICE_V2")
                .and_then(|v| go::parse_bool(&v).ok())
                .filter(|b| *b)
                .map(|_| "2".to_string())
        });
        if !e.attrs.contains_key("token")
            && let Some(t) = env("ACTIONS_RUNTIME_TOKEN")
        {
            e.attrs.insert("token".into(), t);
        }
        if !e.attrs.contains_key("url_v2")
            && v2.as_deref() == Some("2")
            && let Some(u) = env("ACTIONS_RESULTS_URL")
        {
            e.attrs.insert("url_v2".into(), u);
        }
        if !e.attrs.contains_key("url")
            && let Some(u) = env("ACTIONS_CACHE_URL").or_else(|| env("ACTIONS_RESULTS_URL"))
        {
            e.attrs.insert("url".into(), u);
        }
    }
    out.retain(|e| {
        let set = |k: &str| e.attrs.get(k).is_some_and(|v| !v.is_empty());
        e.kind != "gha" || (set("token") && (set("url") || set("url_v2")))
    });
    Ok(out)
}

/// `--add-host`'s values as buildx sends them (build/utils.go `toBuildkitExtraHosts`): each
/// `host=ip` or `host:ip`, its IPs a comma list, each perhaps in brackets; `host-gateway`
/// the address `gateway` gives; joined `host=ip,...` for the frontend's `add-hosts`.
pub fn add_hosts(values: &[String], gateway: &dyn Fn() -> Result<String, String>) -> Result<String, String> {
    let mut hosts = Vec::new();
    for h in values {
        let (host, ip) = h
            .split_once('=')
            .or_else(|| h.split_once(':'))
            .filter(|(host, ip)| !host.is_empty() && !ip.is_empty())
            .ok_or_else(|| format!("invalid host {h}"))?;
        if ip == "host-gateway" {
            let g = gateway().map_err(|e| format!("unable to derive the IP value for host-gateway: {e}"))?;
            hosts.push(format!("{host}={g}"));
            continue;
        }
        for v in ip.split(',') {
            let v = v
                .strip_prefix('[')
                .and_then(|v| v.strip_suffix(']'))
                .filter(|_| v.len() > 2)
                .unwrap_or(v);
            if v.parse::<std::net::IpAddr>().is_err() {
                return Err(format!("invalid host {h}"));
            }
            hosts.push(format!("{host}={v}"));
        }
    }
    Ok(hosts.join(","))
}

/// `--resource`'s entries, and the legacy flags' as buildx makes them (`key=value`, the
/// legacy first), as the frontend's attributes (build/utils.go `ParseResourceLimits`,
/// `addResourceLimits`): non-zero values alone.
pub fn resource_attrs(entries: &[String]) -> Result<BTreeMap<String, String>, String> {
    let (mut memory, mut swap, mut shares, mut period, mut quota) = (0i64, 0i64, 0i64, 0i64, 0i64);
    let (mut cpus, mut mems) = (String::new(), String::new());
    for entry in entries {
        let (k, v) = entry
            .split_once('=')
            .ok_or_else(|| format!("invalid resource {}, expected key=value", go::quote(entry)))?;
        let (k, v) = (k.trim(), v.trim());
        let wrap = |e: String| format!("invalid value {} for resource {k}: {e}", go::quote(v));
        match k {
            "memory" => memory = crate::resources::ram_in_bytes(v).map_err(wrap)?,
            // MemSwapBytes takes -1 as itself.
            "memory-swap" if v == "-1" => swap = -1,
            "memory-swap" => swap = crate::resources::ram_in_bytes(v).map_err(wrap)?,
            "cpu-shares" | "cpu-period" | "cpu-quota" => {
                let n = go::parse_int10(v).map_err(|e| wrap(e.to_string()))?;
                match k {
                    "cpu-shares" => shares = n,
                    "cpu-period" => period = n,
                    _ => quota = n,
                }
            }
            "cpuset-cpus" => cpus = v.to_string(),
            "cpuset-mems" => mems = v.to_string(),
            _ => return Err(format!("unknown resource {}", go::quote(k))),
        }
    }
    let mut attrs = BTreeMap::new();
    if memory > 0 {
        attrs.insert("memory".into(), memory.to_string());
    }
    if swap != 0 {
        attrs.insert("memswap".into(), swap.to_string());
    }
    for (k, n) in [("cpushares", shares), ("cpuperiod", period), ("cpuquota", quota)] {
        if n > 0 {
            attrs.insert(k.into(), n.to_string());
        }
    }
    for (k, v) in [("cpusetcpus", cpus), ("cpusetmems", mems)] {
        if !v.is_empty() {
            attrs.insert(k.into(), v);
        }
    }
    Ok(attrs)
}

/// An annotation `--annotation` asks for (exptypes.AnnotationKey): its type (empty for
/// the default, the manifest), the platform it is for, as written, and its key and value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    pub kind: String,
    pub platform: Option<String>,
    pub key: String,
    pub value: String,
}

/// `--annotation`'s values as buildx reads them (util/buildflags/export.go
/// `ParseAnnotations`): `key=value`, or `type[,type...]:key=value`, each type one of
/// `manifest`, `manifest-descriptor`, `index`, `index-descriptor`, perhaps with a platform
/// in brackets; a key given twice, its last value.
pub fn parse_annotations(values: &[String]) -> Result<Vec<Annotation>, String> {
    let type_re = regex::Regex::new(r"^([a-z-]+)(?:\[([A-Za-z0-9_/-]+)\])?$").map_err(|e| e.to_string())?;
    let mut out: Vec<Annotation> = Vec::new();
    let mut put = |a: Annotation| {
        out.retain(|o| (&o.kind, &o.platform, &o.key) != (&a.kind, &a.platform, &a.key));
        out.push(a);
    };
    for inp in values.iter().filter(|v| !v.is_empty()) {
        let (k, v) = inp
            .split_once('=')
            .ok_or_else(|| format!("invalid annotation {}, expected key=value", go::quote(inp)))?;
        let Some((types, key)) = k.split_once(':') else {
            put(Annotation {
                kind: String::new(),
                platform: None,
                key: k.to_string(),
                value: v.to_string(),
            });
            continue;
        };
        for type_and_platform in types.split(',') {
            let groups = type_re.captures(type_and_platform).ok_or_else(|| {
                format!(
                    "invalid annotation type {}, expected type and optional platform in square brackets",
                    go::quote(type_and_platform)
                )
            })?;
            let kind = groups.get(1).map_or("", |m| m.as_str());
            if !matches!(
                kind,
                "" | "index" | "index-descriptor" | "manifest" | "manifest-descriptor"
            ) {
                return Err(format!("unknown annotation type {}", go::quote(kind)));
            }
            put(Annotation {
                kind: kind.to_string(),
                platform: groups.get(2).map(|m| m.as_str().to_string()),
                key: key.to_string(),
                value: v.to_string(),
            });
        }
    }
    Ok(out)
}

/// Where an output goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dest {
    /// The image store (`image`, and `docker` with no file: loaded).
    Store,
    Stdout,
    Dir(std::path::PathBuf),
    File(std::path::PathBuf),
}

/// An output, as buildx's CreateExports makes it of an `--output`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub kind: String,
    pub attrs: std::collections::BTreeMap<String, String>,
    pub dest: Dest,
}

/// What a destination is, as the outputs' checks ask: none, a directory, or a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    Nothing,
    Dir,
    File,
}

/// buildx's CreateExports (build/opt.go, v0.37.1): each output's destination checked as
/// its type wants one (`local` a directory; `tar`, `oci` and `docker` a file, or with
/// `tar=false` a directory; stdout for a file not given, but `docker`'s, which loads),
/// `registry` an image pushed; then `--push` and `--load` folded in as buildx's build
/// folds them (commands/build.go); then a `local` output's `mode`, `delete` only with
/// `--allow buildx.local.delete` or into a directory under the working one.
pub fn create_exports(
    exports: &[Export],
    push: bool,
    load: bool,
    allow_delete: bool,
    stat: &dyn Fn(&str) -> Result<Found, String>,
    stdout_is_terminal: bool,
    safe_delete: &dyn Fn(&std::path::Path) -> bool,
) -> Result<Vec<Output>, String> {
    let mut outs = Vec::new();
    let mut stdout_used = false;
    for e in exports {
        let mut kind = e.kind.clone();
        let mut attrs = e.attrs.clone();
        let (mut file, mut dir) = (false, false);
        match kind.as_str() {
            "local" => dir = true,
            "tar" => file = true,
            "oci" | "docker" => {
                let tar = attrs.get("tar").is_none_or(|t| go::parse_bool(t).unwrap_or(true));
                file = tar;
                dir = !tar;
            }
            "registry" => {
                kind = "image".into();
                attrs.insert("push".into(), "true".into());
                attrs.entry("unpack".into()).or_insert_with(|| "false".into());
            }
            _ => {}
        }
        let mut dest = Dest::Store;
        if dir {
            if e.dest.is_empty() {
                return Err(format!("dest is required for {kind} exporter"));
            }
            if e.dest == "-" {
                return Err(format!("dest cannot be stdout for {kind} exporter"));
            }
            match stat(&e.dest).map_err(|why| format!("invalid destination directory: {}: {why}", e.dest))? {
                Found::File => return Err(format!("destination directory {} is a file", e.dest)),
                Found::Nothing | Found::Dir => dest = Dest::Dir(e.dest.clone().into()),
            }
        }
        if file {
            let mut at = e.dest.clone();
            if at.is_empty() && kind != "docker" {
                at = "-".into();
            }
            if at == "-" {
                if stdout_used {
                    return Err("multiple outputs configured to write to stdout".into());
                }
                if stdout_is_terminal {
                    return Err(format!(
                        "dest file is required for {kind} exporter. refusing to write to console"
                    ));
                }
                stdout_used = true;
                dest = Dest::Stdout;
            } else if !at.is_empty() {
                match stat(&at).map_err(|why| format!("invalid destination file: {at}: {why}"))? {
                    Found::Dir => return Err(format!("destination file {at} is a directory")),
                    Found::Nothing | Found::File => dest = Dest::File(at.into()),
                }
            }
        }
        outs.push(Output { kind, attrs, dest });
    }
    if push {
        let mut used = false;
        for o in outs.iter_mut().filter(|o| o.kind == "image") {
            o.attrs.insert("push".into(), "true".into());
            o.attrs.entry("unpack".into()).or_insert_with(|| "false".into());
            used = true;
        }
        if !used {
            outs.push(Output {
                kind: "image".into(),
                attrs: [
                    ("push".to_string(), "true".to_string()),
                    ("unpack".to_string(), "false".to_string()),
                ]
                .into_iter()
                .collect(),
                dest: Dest::Store,
            });
        }
    }
    if load
        && !outs
            .iter()
            .any(|o| o.kind == "docker" && !o.attrs.contains_key("dest"))
    {
        outs.push(Output {
            kind: "docker".into(),
            attrs: std::collections::BTreeMap::new(),
            dest: Dest::Store,
        });
    }
    for o in &outs {
        if o.kind != "local" {
            continue;
        }
        let mode = o
            .attrs
            .get("mode")
            .map(|m| m.trim().to_lowercase())
            .unwrap_or_default();
        match mode.as_str() {
            "" | "copy" => {}
            "delete" => {
                if let Dest::Dir(d) = &o.dest
                    && !allow_delete
                    && !safe_delete(d)
                {
                    return Err(format!(
                        "local output mode=delete for destination {} requires --allow=buildx.local.delete",
                        go::quote(&d.to_string_lossy())
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "invalid local exporter mode {}",
                    go::quote(o.attrs.get("mode").map_or("", String::as_str))
                ));
            }
        }
    }
    Ok(outs)
}

/// toBuildOptions' check of the outputs against `--iidfile`: a `local` or `tar` output
/// makes no image to name.
pub fn check_iidfile(exports: &[Export], iidfile: &str) -> Result<(), String> {
    if !iidfile.is_empty() && exports.iter().any(|e| e.kind == "local" || e.kind == "tar") {
        return Err("local and tar exporters are incompatible with image ID file".into());
    }
    Ok(())
}

/// `--build-context`'s values, as buildx's ParseContextNames reads them
/// (util/buildflags/context.go, v0.37.1): each `NAME=VALUE`, empty ones skipped, the name
/// as `familiar` makes it of a reference (FamiliarString of ParseNormalizedNamed, its
/// `:latest` dropped), a later value for a name replacing an earlier.
pub fn parse_contexts(
    values: &[String],
    familiar: &dyn Fn(&str) -> Result<String, String>,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut out = std::collections::BTreeMap::new();
    for value in values.iter().filter(|v| !v.is_empty()) {
        let Some((name, v)) = value.split_once('=') else {
            return Err(format!("invalid context value: {value}, expected key=value"));
        };
        let named = familiar(name).map_err(|e| format!("invalid context name {name}: {e}"))?;
        let named = named.strip_suffix(":latest").unwrap_or(&named).to_string();
        out.insert(named, v.to_string());
    }
    Ok(out)
}

/// An attestation asked for (`--attest`, or `--provenance` and `--sbom`), as buildx v0.37.1
/// reads one (util/buildflags/attests.go): its type, whether it is turned off, and its
/// other attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attest {
    pub kind: String,
    pub disabled: bool,
    pub attrs: BTreeMap<String, String>,
}

/// `CanonicalizeAttest`: `--provenance`'s or `--sbom`'s value as an `--attest`: a boolean
/// turns it on or off; anything else is its attributes.
pub fn canonicalize_attest(kind: &str, value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    match go::parse_bool(value) {
        Ok(b) => format!("type={kind},disabled={}", !b),
        Err(_) => format!("type={kind},{value}"),
    }
}

/// `ParseAttests`: each a CSV of `key=value`, `type` and `disabled` its own (any case),
/// the rest its attributes; a type required; an empty one none at all, as buildx's flag
/// leaves it.
pub fn parse_attests(values: &[String]) -> Result<Vec<Attest>, String> {
    let mut out = Vec::new();
    for v in values.iter().filter(|v| !v.is_empty()) {
        let fields = go::csv_fields(v.as_bytes()).map_err(|e| String::from_utf8_lossy(&e).into_owned())?;
        let mut a = Attest {
            kind: String::new(),
            disabled: false,
            attrs: BTreeMap::new(),
        };
        for field in fields {
            let field = String::from_utf8_lossy(&field).into_owned();
            let Some((key, value)) = field.split_once('=') else {
                return Err(format!("invalid value {field}"));
            };
            match key.trim().to_lowercase().as_str() {
                "type" => a.kind = value.to_string(),
                "disabled" => {
                    a.disabled = go::parse_bool(value).map_err(|e| format!("invalid value {field}: {e}"))?;
                }
                _ => {
                    a.attrs.insert(key.to_string(), value.to_string());
                }
            }
        }
        if a.kind.is_empty() {
            return Err("attestation type not specified".into());
        }
        out.push(a);
    }
    Ok(out)
}

/// `Attests.ToMap`: each type's first, as the frontend's `attest:TYPE` option: none where
/// it is turned off, else `type=TYPE` and its attributes in key order, each written as
/// buildx's csvBuilder writes a pair (quoted where it holds a comma or a quote).
pub fn attests_map(attests: &[Attest]) -> BTreeMap<String, Option<String>> {
    let pair = |k: &str, v: &str| {
        let p = format!("{k}={v}");
        if p.contains(',') || p.contains('"') {
            format!("\"{}\"", p.replace('"', "\"\""))
        } else {
            p
        }
    };
    let mut out = BTreeMap::new();
    for a in attests {
        if out.contains_key(&a.kind) {
            continue;
        }
        if a.disabled {
            out.insert(a.kind.clone(), None);
            continue;
        }
        let mut parts = vec![pair("type", &a.kind)];
        parts.extend(a.attrs.iter().map(|(k, v)| pair(k, v)));
        out.insert(a.kind.clone(), Some(parts.join(",")));
    }
    out
}

/// An `--ssh` spec, as buildx's ParseSSHSpecs reads one (util/buildflags/ssh.go,
/// v0.37.1): `ID[=PATH,...]`, the paths sockets or keys, none for `SSH_AUTH_SOCK`'s.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ssh {
    pub id: String,
    pub paths: Vec<String>,
}

/// `--ssh`'s specs, empty ones skipped, each kept: `buildx build` does not normalize them
/// (commands/build.go), so that an id given twice is refused ([`ssh_agents`]).
pub fn parse_ssh(specs: &[String]) -> Vec<Ssh> {
    specs
        .iter()
        .filter(|s| !s.is_empty())
        .map(|spec| match spec.split_once('=') {
            Some((id, paths)) => Ssh {
                id: id.to_string(),
                paths: paths.split(',').map(str::to_string).collect(),
            },
            None => Ssh {
                id: spec.clone(),
                paths: Vec::new(),
            },
        })
        .collect()
}

/// An agent a build's steps reach: a socket forwarded, or the keys of files, which the
/// client serves as an agent of its own (BuildKit's keyring).
#[derive(Debug)]
pub enum Agent<K> {
    Socket(std::path::PathBuf),
    Keys(Vec<K>),
}

/// Why a key file is not taken: where buildx's agent parses it (`failed to parse FILE`),
/// or where its keyring takes the key (`failed to add FILE to agent`).
#[derive(Debug, PartialEq, Eq)]
pub enum KeyRefused {
    Parse(String),
    Add(String),
}

/// The most of a key file read, as BuildKit reads one.
const KEY_FILE_MAX: u64 = 100 * 1024;

/// The SSH agents `specs` forward, by id, as BuildKit's sshprovider takes them
/// (v0.33.0 session/sshforward/sshprovider/agentprovider.go, NewSSHAgentProvider and
/// toDialer): an id `default` where none is given; no path, `SSH_AUTH_SOCK`'s (`env`);
/// one socket, or key files, each read (its first 100 KiB) and made a key by `key`.
pub fn ssh_agents<K>(
    specs: &[Ssh],
    env: &dyn Fn(&str) -> Option<String>,
    key: &dyn Fn(&[u8]) -> Result<K, KeyRefused>,
) -> Result<BTreeMap<String, Agent<K>>, String> {
    #[cfg(unix)]
    let is_socket = |m: &std::fs::Metadata| std::os::unix::fs::FileTypeExt::is_socket(&m.file_type());
    // No builder runs on Windows yet, nor does a socket there say what it is.
    #[cfg(not(unix))]
    let is_socket = |_: &std::fs::Metadata| true;
    // An error of a path, as Go's *PathError says it.
    let os_err = |op: &str, p: &str, e: &std::io::Error| {
        let why = match e.kind() {
            std::io::ErrorKind::NotFound => "no such file or directory".to_string(),
            std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
            std::io::ErrorKind::IsADirectory => "is a directory".to_string(),
            _ => e.to_string(),
        };
        format!("{op} {p}: {why}")
    };
    let mut out = BTreeMap::new();
    for spec in specs {
        let id = if spec.id.is_empty() {
            "default"
        } else {
            spec.id.as_str()
        };
        if out.contains_key(id) {
            return Err(format!("duplicate agent ID {}", go::quote(id)));
        }
        // Go's %v of AgentConfig{ID, Paths, Raw}.
        let conf = format!("{{{id} [{}] false}}", spec.paths.join(" "));
        let fail = |why: String| format!("failed to convert agent config {conf}: {why}");
        let mut paths = spec.paths.clone();
        if paths.is_empty() || paths.len() == 1 && paths.first().is_some_and(String::is_empty) {
            paths = vec![env("SSH_AUTH_SOCK").unwrap_or_default()];
        }
        if paths.first().is_some_and(String::is_empty) {
            return Err(fail(
                "invalid empty ssh agent socket: make sure SSH_AUTH_SOCK is set".into(),
            ));
        }
        let wrap = |why: String| {
            fail(format!(
                "failed to convert agent config for ID: {}: {why}",
                go::quote(id)
            ))
        };
        let mut socket = None;
        let mut keys = Vec::new();
        for p in &paths {
            if socket.is_some() {
                return Err(wrap("only single socket allowed".into()));
            }
            let meta = std::fs::metadata(p).map_err(|e| wrap(os_err("stat", p, &e)))?;
            if is_socket(&meta) {
                socket = Some(std::path::PathBuf::from(p));
                continue;
            }
            let file = std::fs::File::open(p)
                .map_err(|e| wrap(format!("failed to open {p}: {}", os_err("open", p, &e))))?;
            let mut bytes = SecretBytes(Vec::new());
            std::io::Read::read_to_end(&mut std::io::Read::take(file, KEY_FILE_MAX), &mut bytes.0)
                .map_err(|e| wrap(format!("failed to read {p}: {}", os_err("read", p, &e))))?;
            keys.push(key(bytes.bytes()).map_err(|e| {
                wrap(match e {
                    KeyRefused::Parse(why) => format!("failed to parse {p}: {why}"),
                    KeyRefused::Add(why) => format!("failed to add {p} to agent: {why}"),
                })
            })?);
        }
        let agent = match socket {
            Some(_) if !keys.is_empty() => {
                return Err(wrap("invalid combination of keys and sockets".into()));
            }
            Some(s) => Agent::Socket(s),
            None => Agent::Keys(keys),
        };
        out.insert(id.to_string(), agent);
    }
    Ok(out)
}

pub fn parse_secrets(specs: &[String]) -> Result<Vec<Secret>, String> {
    specs
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| parse_secret(s))
        .collect()
}

fn parse_secret(value: &str) -> Result<Secret, String> {
    let fields = go::csv_fields(value.as_bytes())
        .map_err(|e| format!("failed to parse csv secret: {}", String::from_utf8_lossy(&e)))?;
    let mut s = Secret::default();
    let mut env_type = false;
    for field in fields {
        let field = String::from_utf8_lossy(&field).into_owned();
        let Some((key, value)) = field.split_once('=') else {
            return Err(format!("invalid field '{field}' must be a key=value pair"));
        };
        let key = key.to_lowercase();
        match key.as_str() {
            "type" => {
                if value != "file" && value != "env" {
                    return Err(format!("unsupported secret type {}", go::quote(value)));
                }
                env_type = value == "env";
            }
            "id" => s.id = value.to_string(),
            "source" | "src" => s.file = value.to_string(),
            "env" => s.env = value.to_string(),
            _ => return Err(format!("unexpected key '{key}' in '{field}'")),
        }
    }
    if env_type && s.env.is_empty() {
        s.env = std::mem::take(&mut s.file);
    }
    Ok(s)
}

/// A secret's bytes, held in memory alone, and overwritten when dropped: no copy outlives
/// the build that was given it but those the step it was given to holds.
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretBytes {
    /// The length alone: a secret is never printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretBytes({} bytes)", self.0.len())
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        let len = self.0.capacity();
        self.0.resize(len, 0);
        for b in self.0.iter_mut() {
            // SAFETY: a byte of our own buffer; volatile, so the write is not elided as
            // dead before the buffer is freed.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

/// The secrets `secrets` name, by id, the last of an id counting, read now: one without
/// a file or a variable is the variable of its id if there is one, else the file of that
/// name, as BuildKit's store finds it; a variable's value is empty if unset. A file must
/// be there, and no secret may hold more than `max` bytes, what a build step can carry.
/// `env` is the client's environment, its values' bytes as they are.
pub fn store(
    secrets: Vec<Secret>,
    env: &dyn Fn(&str) -> Option<Vec<u8>>,
    max: u64,
) -> Result<BTreeMap<String, SecretBytes>, String> {
    let mut store = BTreeMap::new();
    for s in secrets {
        if s.id.is_empty() {
            return Err("secret missing ID".into());
        }
        let file = if !s.env.is_empty() {
            None
        } else if !s.file.is_empty() {
            Some(s.file.as_str())
        } else if env(&s.id).is_some() {
            None
        } else {
            Some(s.id.as_str())
        };
        let bytes = match file {
            Some(path) => {
                let meta = std::fs::metadata(path)
                    .map_err(|e| format!("failed to stat {path}: stat {path}: {}", os_error(&e)))?;
                if meta.len() > max {
                    return Err(too_big(&s.id, meta.len(), max));
                }
                std::fs::read(path).map_err(|e| format!("open {path}: {}", os_error(&e)))?
            }
            None => {
                let name = if s.env.is_empty() { &s.id } else { &s.env };
                env(name).unwrap_or_default()
            }
        };
        if bytes.len() as u64 > max {
            return Err(too_big(&s.id, bytes.len() as u64, max));
        }
        store.insert(s.id, SecretBytes(bytes));
    }
    Ok(store)
}

/// BuildKit's words for a secret past the most it takes, with shards' most: go-units'
/// `%#.f` of a size in binary units (`500KiB`, `1MiB`), as secretsprovider prints it.
fn too_big(id: &str, _len: u64, max: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let (mut size, mut unit) = (max as f64, 0);
    while size >= 1024.0 && unit + 1 < units.len() {
        size /= 1024.0;
        unit += 1;
    }
    format!(
        "secret {id} too big. max size {size:.0}{}",
        units.get(unit).unwrap_or(&"B")
    )
}

/// An error of the OS in Go's words: on Linux its `syscall.Errno` table; elsewhere the C
/// library's `strerror`, its first letter lowered where the second is lower, as Go's
/// tables are made from it (mkerrors.sh).
pub fn os_error(e: &std::io::Error) -> String {
    let Some(code) = e.raw_os_error() else {
        return e.to_string();
    };
    if cfg!(target_os = "linux") {
        return go::linux_error(code);
    }
    let text = std::io::Error::from_raw_os_error(code).to_string();
    let text = text.split(" (os error").next().unwrap_or_default();
    let mut chars = text.chars();
    match (chars.next(), chars.clone().next()) {
        (Some(first), Some(second)) if first.is_ascii_uppercase() && second.is_ascii_lowercase() => {
            format!("{}{}", first.to_ascii_lowercase(), chars.as_str())
        }
        _ => text.to_string(),
    }
}

/// The `--allow` values: the entitlements granted, each as given, and whether
/// `buildx.local.delete` was.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entitlements {
    pub granted: Vec<String>,
    pub local_delete: bool,
}

impl Entitlements {
    /// Whether `name` is granted: `device` with or without its configuration.
    pub fn grants(&self, name: &str) -> bool {
        self.granted
            .iter()
            .any(|g| g == name || g.split_once('=').is_some_and(|(k, _)| k == name))
    }
}

pub const SECURITY_INSECURE: &str = "security.insecure";
pub const NETWORK_HOST: &str = "network.host";
pub const DEVICE: &str = "device";
const LOCAL_DELETE: &str = "buildx.local.delete";

/// `--allow`'s values as ParseEntitlements reads them; empty ones are skipped.
pub fn parse_entitlements(values: &[String]) -> Result<Entitlements, String> {
    let mut out = Entitlements::default();
    for v in values.iter().filter(|v| !v.is_empty()) {
        let (key, rest) = v
            .split_once('=')
            .map_or((v.as_str(), None), |(k, r)| (k, Some(r)));
        if key == LOCAL_DELETE {
            if rest.is_some() {
                return Err(format!("{LOCAL_DELETE} does not accept a value"));
            }
            out.local_delete = true;
            continue;
        }
        let name = if key == DEVICE {
            devices(rest.unwrap_or_default())?;
            key
        } else {
            v.as_str()
        };
        if ![SECURITY_INSECURE, NETWORK_HOST, DEVICE].contains(&name) {
            return Err(format!("unknown entitlement {name}"));
        }
        out.granted.push(v.clone());
    }
    Ok(out)
}

/// `device=`'s configuration, as ParseDevicesConfig checks it: a device, then `alias=`.
fn devices(config: &str) -> Result<(), String> {
    if config.is_empty() {
        return Ok(());
    }
    let fields = go::csv_fields(config.as_bytes()).map_err(|e| String::from_utf8_lossy(&e).into_owned())?;
    for field in fields.iter().skip(1) {
        let field = String::from_utf8_lossy(field);
        match field.split_once('=') {
            None => return Err(format!("invalid device config {}", go::quote(&field))),
            Some(("alias", _)) => {}
            Some((key, _)) => return Err(format!("unknown device config key {}", go::quote(key))),
        }
    }
    Ok(())
}

/// A `--ulimit`: a resource, and its soft and hard limits, -1 for none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ulimit {
    pub name: String,
    pub soft: i64,
    pub hard: i64,
}

impl fmt::Display for Ulimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={}:{}", self.name, self.soft, self.hard)
    }
}

/// The resources `--ulimit` names: go-units' ulimitNameMapping, and `as`, which it leaves
/// out and shards' builder sets.
pub const ULIMITS: [&str; 16] = [
    "as",
    "core",
    "cpu",
    "data",
    "fsize",
    "locks",
    "memlock",
    "msgqueue",
    "nice",
    "nofile",
    "nproc",
    "rss",
    "rtprio",
    "rttime",
    "sigpending",
    "stack",
];

/// `name=soft[:hard]` as go-units' ParseUlimit reads it: the limits decimal, the hard
/// one the soft one when not given, and no greater than it unless -1.
pub fn parse_ulimit(value: &str) -> Result<Ulimit, String> {
    let Some((name, limits)) = value.split_once('=') else {
        return Err(format!("invalid ulimit argument: {value}"));
    };
    if !ULIMITS.contains(&name) {
        return Err(format!("invalid ulimit type: {name}"));
    }
    let parts: Vec<&str> = limits.split(':').collect();
    let int = |s: &str| go::parse_int10(s).map_err(|e| e.to_string());
    let (soft, hard) = match parts.as_slice() {
        [soft] => {
            let soft = int(soft)?;
            (soft, soft)
        }
        [soft, hard] => {
            let hard = int(hard)?;
            (int(soft)?, hard)
        }
        _ => {
            return Err(format!(
                "too many limit value arguments - {limits}, can only have up to two, `soft[:hard]`"
            ));
        }
    };
    if hard != -1 {
        if soft == -1 {
            return Err(format!(
                "ulimit soft limit must be less than or equal to hard limit: soft: -1 (unlimited), hard: {hard}"
            ));
        }
        if soft > hard {
            return Err(format!(
                "ulimit soft limit must be less than or equal to hard limit: {soft} > {hard}"
            ));
        }
    }
    Ok(Ulimit {
        name: name.to_string(),
        soft,
        hard,
    })
}

/// `--ulimit`'s values as docker/cli's UlimitOpt keeps them: the last of each resource,
/// in the order of their names.
pub fn ulimits(values: &[String]) -> Result<Vec<Ulimit>, String> {
    let mut by_name = BTreeMap::new();
    for v in values {
        let u = parse_ulimit(v)?;
        by_name.insert(u.name.clone(), u);
    }
    Ok(by_name.into_values().collect())
}

/// What `build`'s flags take, checked as pflag sets them: a `--ulimit` read, and shown
/// as UlimitOpt shows it; the rest as given.
pub fn validate(flag: &crate::flags::Flag, value: &str) -> Result<String, String> {
    match flag.name {
        "ulimit" => parse_ulimit(value).map(|u| u.to_string()),
        // opts.MemBytes, as buildx takes --shm-size and prune's sizes: go-units'
        // RAMInBytes; a filter as docker/cli's FilterOpt.
        _ if flag.kind == crate::flags::Kind::Value("bytes") => {
            crate::resources::ram_in_bytes(value).map(|n| n.to_string())
        }
        _ if flag.kind == crate::flags::Kind::Many("filter") => crate::flags::value(flag, value),
        _ => Ok(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn hosts_and_resources_are_sent_as_buildx_sends_them() {
        let gw = || Ok("172.17.0.1".to_string());
        let v = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(
            add_hosts(&v(&["db:10.0.0.2", "a=1.2.3.4,[::1]", "h:host-gateway"]), &gw).unwrap(),
            "db=10.0.0.2,a=1.2.3.4,a=::1,h=172.17.0.1"
        );
        assert_eq!(add_hosts(&v(&["db"]), &gw).unwrap_err(), "invalid host db");
        assert_eq!(add_hosts(&v(&["db:x"]), &gw).unwrap_err(), "invalid host db:x");
        let attrs = resource_attrs(&v(&[
            "memory=2g",
            "memory-swap=-1",
            "cpu-shares=512",
            "cpuset-cpus=0-1",
        ]))
        .unwrap();
        assert_eq!(
            attrs.into_iter().collect::<Vec<_>>(),
            [
                ("cpusetcpus", "0-1"),
                ("cpushares", "512"),
                ("memory", "2147483648"),
                ("memswap", "-1")
            ]
            .map(|(k, v)| (k.to_string(), v.to_string()))
        );
        assert_eq!(
            resource_attrs(&v(&["gpu=1"])).unwrap_err(),
            "unknown resource \"gpu\""
        );
        assert_eq!(
            resource_attrs(&v(&["memory"])).unwrap_err(),
            "invalid resource \"memory\", expected key=value"
        );
    }

    #[test]
    fn cache_entries_are_read_as_buildx_reads_them() {
        let none = |_: &str| None;
        let one = |v: &str| cache_entries(&[v.to_string()], &none);
        let entry = |kind: &str, attrs: &[(&str, &str)]| CacheEntry {
            kind: kind.into(),
            attrs: attrs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect(),
        };
        assert_eq!(
            one("user/app:cache").unwrap(),
            vec![entry("registry", &[("ref", "user/app:cache")])]
        );
        assert_eq!(
            one("a,b").unwrap(),
            vec![
                entry("registry", &[("ref", "a")]),
                entry("registry", &[("ref", "b")])
            ]
        );
        assert_eq!(
            one("Type=local,Dest=out,mode=max").unwrap(),
            vec![entry("local", &[("dest", "out"), ("mode", "max")])]
        );
        assert_eq!(one("mode=max").unwrap_err(), "type required for \"mode=max\"");
        assert_eq!(one("type=local,src").unwrap_err(), "invalid value src");
        assert!(one("").unwrap().is_empty());
        // GitHub Actions: active only with a token and a URL, from the environment.
        assert!(one("type=gha").unwrap().is_empty());
        let ci = |k: &str| match k {
            "ACTIONS_RUNTIME_TOKEN" => Some("t".to_string()),
            "ACTIONS_RESULTS_URL" => Some("u".to_string()),
            "ACTIONS_CACHE_SERVICE_V2" => Some("true".to_string()),
            _ => None,
        };
        assert_eq!(
            cache_entries(&["type=gha".to_string()], &ci).unwrap(),
            vec![entry("gha", &[("token", "t"), ("url", "u"), ("url_v2", "u")])]
        );
    }

    use super::*;

    /// buildx's: `ID[=PATH,...]`, each kept; BuildKit's provider's refusals.
    #[test]
    fn ssh_specs_are_read_as_buildx_reads_them() {
        let s = |v: &[&str]| parse_ssh(&v.iter().map(|x| x.to_string()).collect::<Vec<_>>());
        let ssh = |id: &str, paths: &[&str]| Ssh {
            id: id.into(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
        };
        assert_eq!(s(&["default"]), [ssh("default", &[])]);
        assert_eq!(s(&["a=/x,/y", "", "b"]), [ssh("a", &["/x", "/y"]), ssh("b", &[])]);
        assert_eq!(s(&["c="]), [ssh("c", &[""])]);
        let none = |_: &str| None;
        let key = |b: &[u8]| Ok::<_, KeyRefused>(String::from_utf8_lossy(b).into_owned());
        // A duplicate is met once the first of its id resolves: a socket here.
        #[cfg(unix)]
        {
            let dir = std::env::temp_dir().join(format!("shards-ssh-spec-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let sock = dir.join("agent");
            let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
            let at = sock.to_string_lossy().into_owned();
            let agents = ssh_agents(&s(&["a"]), &|_| Some(at.clone()), &key).unwrap();
            assert!(matches!(agents.get("a"), Some(Agent::Socket(p)) if *p == sock));
            assert_eq!(
                ssh_agents(&s(&["a", "a"]), &|_| Some(at.clone()), &key).unwrap_err(),
                "duplicate agent ID \"a\""
            );
            assert_eq!(
                ssh_agents(&s(&[&format!("b={at},{at}")]), &none, &key).unwrap_err(),
                format!(
                    "failed to convert agent config {{b [{at} {at}] false}}: failed to convert agent config for ID: \"b\": only single socket allowed"
                )
            );
            // Key files: each made a key, in order; never beside a socket.
            let (k1, k2) = (dir.join("k1"), dir.join("k2"));
            std::fs::write(&k1, "one").unwrap();
            std::fs::write(&k2, "two").unwrap();
            let (k1, k2) = (
                k1.to_string_lossy().into_owned(),
                k2.to_string_lossy().into_owned(),
            );
            let agents = ssh_agents(&s(&[&format!("k={k1},{k2}")]), &none, &key).unwrap();
            assert!(matches!(agents.get("k"), Some(Agent::Keys(v)) if *v == ["one", "two"]));
            let conf = |paths: &str, why: &str| {
                format!(
                    "failed to convert agent config {{k [{paths}] false}}: failed to convert agent config for ID: \"k\": {why}"
                )
            };
            assert_eq!(
                ssh_agents(&s(&[&format!("k={k1},{at}")]), &none, &key).unwrap_err(),
                conf(&format!("{k1} {at}"), "invalid combination of keys and sockets")
            );
            let refused = |b: &[u8]| -> Result<String, KeyRefused> {
                Err(if b == b"one" {
                    KeyRefused::Parse("ssh: no key found".into())
                } else {
                    KeyRefused::Add("ssh: unsupported key type *ecdh.PrivateKey".into())
                })
            };
            assert_eq!(
                ssh_agents(&s(&[&format!("k={k1}")]), &none, &refused).unwrap_err(),
                conf(&k1, &format!("failed to parse {k1}: ssh: no key found"))
            );
            assert_eq!(
                ssh_agents(&s(&[&format!("k={k2}")]), &none, &refused).unwrap_err(),
                conf(
                    &k2,
                    &format!("failed to add {k2} to agent: ssh: unsupported key type *ecdh.PrivateKey")
                )
            );
            let d = dir.to_string_lossy().into_owned();
            assert_eq!(
                ssh_agents(&s(&[&format!("k={d}")]), &none, &key).unwrap_err(),
                conf(&d, &format!("failed to read {d}: read {d}: is a directory"))
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
        assert_eq!(
            ssh_agents(&s(&["default"]), &none, &key).unwrap_err(),
            "failed to convert agent config {default [] false}: invalid empty ssh agent socket: make sure SSH_AUTH_SOCK is set"
        );
        assert_eq!(
            ssh_agents(&s(&["k=/no/such"]), &none, &key).unwrap_err(),
            "failed to convert agent config {k [/no/such] false}: failed to convert agent config for ID: \"k\": stat /no/such: no such file or directory"
        );
    }

    /// A secret past the limit is refused in BuildKit's words, with the limit it is past:
    /// BuildKit's own, as buildx printed it (tests/buildx.json), and shards' step's.
    #[test]
    fn a_secret_past_the_limit_is_refused_in_buildkits_words() {
        assert_eq!(
            too_big("big", 0, 500 * 1024),
            "secret big too big. max size 500KiB"
        );
        assert_eq!(too_big("big", 0, 1 << 20), "secret big too big. max size 1MiB");
    }
}
