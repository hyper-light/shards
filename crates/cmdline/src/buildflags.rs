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
fn os_error(e: &std::io::Error) -> String {
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
        _ => Ok(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
