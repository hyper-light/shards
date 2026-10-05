//! `run`'s `--mount` and `-v` as docker/cli v29.8.1 reads them: opts.MountOpt.Set (one CSV
//! record of `key=value` options, opts/mount.go and mount_utils.go) and volumespec.Parse
//! (internal/volumespec), which only tells a bind from a volume, the daemon checking the
//! rest.

use std::collections::BTreeMap;

/// A mount as the API's mount.Mount holds what the CLI read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mount {
    /// `bind`, `volume`, `tmpfs`, `image`, or what was given.
    pub kind: String,
    pub source: String,
    pub target: String,
    pub read_only: bool,
    pub consistency: String,
    pub bind: Option<Bind>,
    pub volume: Option<Volume>,
    pub image: Option<Image>,
    pub tmpfs: Option<Tmpfs>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bind {
    pub propagation: String,
    pub non_recursive: bool,
    pub read_only_non_recursive: bool,
    pub read_only_force_recursive: bool,
    pub create_mountpoint: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Volume {
    pub no_copy: bool,
    pub labels: BTreeMap<String, String>,
    pub subpath: String,
    /// The driver's name and options, where given.
    pub driver: Option<(String, BTreeMap<String, String>)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Image {
    pub subpath: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tmpfs {
    pub size_bytes: i64,
    pub mode: u32,
}

fn bool_value(key: &str, val: &str, has: bool) -> Result<bool, String> {
    if !has {
        return Ok(true);
    }
    match val {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        _ => Err(format!(
            "invalid value for '{key}': invalid boolean value ({}): must be one of \"true\", \"1\", \"false\", or \"0\" (default \"true\")",
            crate::go::quote(val)
        )),
    }
}

/// setValueOnMap: `k=v`, or nothing for an empty key.
fn set_on(map: &mut BTreeMap<String, String>, kv: &str) {
    let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
    if !k.is_empty() {
        map.insert(k.to_string(), v.to_string());
    }
}

fn bind(m: &mut Mount) -> &mut Bind {
    m.bind.get_or_insert_with(Bind::default)
}

fn volume(m: &mut Mount) -> &mut Volume {
    m.volume.get_or_insert_with(Volume::default)
}

/// opts.MountOpt.Set: `value` as one mount, a source starting with `.` made absolute
/// against `cwd`.
pub fn parse_mount(value: &str, cwd: Option<&std::path::Path>) -> Result<Mount, String> {
    let value = value.trim_matches(|c: char| c.is_whitespace());
    if value.is_empty() {
        return Err("value is empty".into());
    }
    let fields =
        crate::go::csv_fields(value.as_bytes()).map_err(|e| String::from_utf8_lossy(&e).into_owned())?;
    let mut m = Mount {
        kind: "volume".into(),
        ..Mount::default()
    };
    for field in fields {
        let field = String::from_utf8_lossy(&field).into_owned();
        let (key, val, has) = match field.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string(), true),
            None => (field.clone(), String::new(), false),
        };
        let trimmed = key.trim();
        if trimmed != key {
            return Err(format!(
                "invalid option '{trimmed}' in '{field}': option should not have whitespace"
            ));
        }
        if has {
            let v = val.trim();
            if v.is_empty() {
                return Err(format!("invalid value for '{key}': value is empty"));
            }
            if v != val {
                return Err(format!(
                    "invalid value for '{key}' in '{field}': value should not have whitespace"
                ));
            }
        }
        let key = key.to_lowercase();
        if !has
            && !matches!(
                key.as_str(),
                "readonly" | "ro" | "volume-nocopy" | "bind-nonrecursive" | "bind-create-src"
            )
        {
            return Err(format!("invalid field '{field}' must be a key=value pair"));
        }
        match key.as_str() {
            "type" => m.kind = val.to_lowercase(),
            "source" | "src" => {
                m.source = val.clone();
                if !val.starts_with('/')
                    && val.starts_with('.')
                    && let Some(cwd) = cwd
                {
                    m.source = clean(&cwd.join(&val).to_string_lossy());
                }
            }
            "target" | "dst" | "destination" => m.target = val,
            "readonly" | "ro" => m.read_only = bool_value(&key, &val, has)?,
            "consistency" => m.consistency = val.to_lowercase(),
            "bind-propagation" => bind(&mut m).propagation = val.to_lowercase(),
            "bind-nonrecursive" => {
                return Err("bind-nonrecursive is deprecated, use bind-recursive=disabled instead".into());
            }
            "bind-recursive" => match val.as_str() {
                "enabled" => {}
                "disabled" => bind(&mut m).non_recursive = true,
                "writable" => bind(&mut m).read_only_non_recursive = true,
                "readonly" => bind(&mut m).read_only_force_recursive = true,
                _ => {
                    return Err(format!(
                        "invalid value for {key}: {val} (must be \"enabled\", \"disabled\", \"writable\", or \"readonly\")"
                    ));
                }
            },
            "bind-create-src" => bind(&mut m).create_mountpoint = bool_value(&key, &val, has)?,
            "volume-subpath" => volume(&mut m).subpath = val,
            "volume-nocopy" => volume(&mut m).no_copy = bool_value(&key, &val, has)?,
            "volume-label" => set_on(&mut volume(&mut m).labels, &val),
            "volume-driver" => {
                let v = volume(&mut m);
                v.driver.get_or_insert_with(Default::default).0 = val;
            }
            "volume-opt" => {
                let v = volume(&mut m);
                set_on(&mut v.driver.get_or_insert_with(Default::default).1, &val);
            }
            "image-subpath" => m.image.get_or_insert_with(Image::default).subpath = val,
            "tmpfs-size" => {
                let size = crate::resources::ram_in_bytes(&val)
                    .map_err(|_| format!("invalid value for {key}: {val}"))?;
                m.tmpfs.get_or_insert_with(Tmpfs::default).size_bytes = size;
            }
            "tmpfs-mode" => {
                let mode =
                    u32::from_str_radix(&val, 8).map_err(|_| format!("invalid value for {key}: {val}"))?;
                m.tmpfs.get_or_insert_with(Tmpfs::default).mode = mode;
            }
            _ => return Err(format!("unknown option '{key}' in '{field}'")),
        }
    }
    validate(&m)?;
    Ok(m)
}

/// mount_utils.go validateMountOptions, with validateExclusiveOptions.
fn validate(m: &Mount) -> Result<(), String> {
    if m.kind.is_empty() {
        return Err("type is required".into());
    }
    let mixed = |what: &str| format!("cannot mix '{what}-*' options with mount type '{}'", m.kind);
    if m.kind != "bind" && m.bind.is_some() {
        return Err(mixed("bind"));
    }
    if m.kind != "volume" && m.volume.is_some() {
        return Err(mixed("volume"));
    }
    if m.kind != "image" && m.image.is_some() {
        return Err(mixed("image"));
    }
    if m.kind != "tmpfs" && m.tmpfs.is_some() {
        return Err(mixed("tmpfs"));
    }
    if let Some(b) = &m.bind {
        if b.read_only_non_recursive && !m.read_only {
            return Err(
                "option 'bind-recursive=writable' requires 'readonly' to be specified in conjunction".into(),
            );
        }
        if b.read_only_force_recursive {
            if !m.read_only {
                return Err(
                    "option 'bind-recursive=readonly' requires 'readonly' to be specified in conjunction"
                        .into(),
                );
            }
            if b.propagation != "rprivate" {
                return Err(
                    "option 'bind-recursive=readonly' requires 'bind-propagation=rprivate' to be specified in conjunction"
                        .into(),
                );
            }
        }
    }
    Ok(())
}

/// MountOpt.String: each mount's type, source and target, as `%s %s %s`, joined by `, `.
pub fn mounts_string(values: &[String]) -> String {
    values
        .iter()
        .filter_map(|v| parse_mount(v, None).ok())
        .map(|m| format!("{} {} {}", m.kind, m.source, m.target))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A mount as one CSV record that [`parse_mount`] reads back as it: what the client sends
/// its daemon, its source already absolute.
pub fn encode(m: &Mount) -> String {
    let mut fields: Vec<String> = vec![format!("type={}", m.kind)];
    let push = |fields: &mut Vec<String>, k: &str, v: &str| {
        if !v.is_empty() {
            fields.push(format!("{k}={v}"));
        }
    };
    push(&mut fields, "source", &m.source);
    push(&mut fields, "target", &m.target);
    push(&mut fields, "consistency", &m.consistency);
    if m.read_only {
        fields.push("readonly=true".into());
    }
    if let Some(b) = &m.bind {
        push(&mut fields, "bind-propagation", &b.propagation);
        if b.non_recursive {
            fields.push("bind-recursive=disabled".into());
        } else if b.read_only_non_recursive {
            fields.push("bind-recursive=writable".into());
        } else if b.read_only_force_recursive {
            fields.push("bind-recursive=readonly".into());
        }
        if b.create_mountpoint {
            fields.push("bind-create-src=true".into());
        }
    }
    if let Some(v) = &m.volume {
        if v.no_copy {
            fields.push("volume-nocopy=true".into());
        }
        if !v.subpath.is_empty() {
            fields.push(format!("volume-subpath={}", v.subpath));
        }
        for (k, val) in &v.labels {
            fields.push(format!("volume-label={k}={val}"));
        }
        if let Some((name, opts)) = &v.driver {
            if !name.is_empty() {
                fields.push(format!("volume-driver={name}"));
            }
            for (k, val) in opts {
                fields.push(format!("volume-opt={k}={val}"));
            }
        }
    }
    if let Some(i) = &m.image
        && !i.subpath.is_empty()
    {
        fields.push(format!("image-subpath={}", i.subpath));
    }
    if let Some(t) = &m.tmpfs {
        if t.size_bytes != 0 {
            fields.push(format!("tmpfs-size={}", t.size_bytes));
        }
        if t.mode != 0 {
            fields.push(format!("tmpfs-mode={:o}", t.mode));
        }
    }
    fields
        .iter()
        .map(|f| {
            if f.contains([',', '"', '\n', '\r']) || f.starts_with(' ') {
                format!("\"{}\"", f.replace('"', "\"\""))
            } else {
                f.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// What volumespec.Parse makes of a `-v` value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VolumeSpec {
    /// `bind` or `volume`.
    pub kind: String,
    pub source: String,
    pub target: String,
}

/// volumespec.Parse: `[SOURCE:]TARGET[:OPTIONS]`, a Windows drive's colon aside.
pub fn parse_volume(spec: &str) -> Result<VolumeSpec, String> {
    let chars: Vec<char> = spec.chars().collect();
    match chars.len() {
        0 => return Err("invalid empty volume spec".into()),
        1 | 2 => {
            return Ok(VolumeSpec {
                kind: "volume".into(),
                source: String::new(),
                target: spec.to_string(),
            });
        }
        _ => {}
    }
    let mut v = VolumeSpec::default();
    let mut buffer: Vec<char> = Vec::new();
    let end = '\0';
    let windows_drive = |buffer: &[char], c: char| {
        c == ':' && buffer.len() == 1 && buffer.first().is_some_and(|b| b.is_alphabetic())
    };
    for c in chars.iter().copied().chain(std::iter::once(end)) {
        if windows_drive(&buffer, c) {
            buffer.push(c);
        } else if c == ':' || c == end {
            let s: String = buffer.iter().collect();
            let failed = if buffer.is_empty() {
                Some("empty section between colons")
            } else if v.source.is_empty() && c == end {
                v.target = s;
                None
            } else if v.source.is_empty() {
                v.source = s;
                None
            } else if v.target.is_empty() {
                v.target = s;
                None
            } else if c == ':' {
                Some("too many colons")
            } else {
                None
            };
            if let Some(why) = failed {
                return Err(format!("invalid spec: {spec}: {why}"));
            }
            buffer.clear();
        } else {
            buffer.push(c);
        }
    }
    v.kind = if v.source.is_empty() || !is_file_path(&v.source) {
        "volume".into()
    } else {
        "bind".into()
    };
    Ok(v)
}

/// volumespec's isFilePath.
fn is_file_path(source: &str) -> bool {
    let mut chars = source.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if matches!(first, '.' | '/' | '~') {
        return true;
    }
    let Some(second) = chars.next() else {
        return false;
    };
    if source.starts_with("\\\\") {
        return true;
    }
    second == ':' && first.is_alphabetic()
}

/// Go's path.Clean, of a slash-separated path.
pub fn clean(p: &str) -> String {
    let rooted = p.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".into(),
        (false, false) => joined,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounts_read_as_docker_cli_reads_them() {
        let m = parse_mount("type=bind,src=/a,dst=/b,readonly,bind-propagation=rshared", None).unwrap();
        assert_eq!(
            (m.kind.as_str(), m.source.as_str(), m.target.as_str(), m.read_only),
            ("bind", "/a", "/b", true)
        );
        assert_eq!(m.bind.as_ref().unwrap().propagation, "rshared");
        assert_eq!(parse_mount(&encode(&m), None).unwrap(), m);
        let rel = parse_mount("type=bind,source=./x,target=/y", Some(std::path::Path::new("/w"))).unwrap();
        assert_eq!(rel.source, "/w/x");
        for (given, said) in [
            ("", "value is empty"),
            ("type=bind,src", "invalid field 'src' must be a key=value pair"),
            (
                "type=volume,bind-propagation=shared",
                "cannot mix 'bind-*' options with mount type 'volume'",
            ),
            (
                "type=bind,ro=maybe",
                "invalid value for 'ro': invalid boolean value (\"maybe\"): must be one of \"true\", \"1\", \"false\", or \"0\" (default \"true\")",
            ),
            ("type=bind,nope=1", "unknown option 'nope' in 'nope=1'"),
            ("type=tmpfs,tmpfs-size=x", "invalid value for tmpfs-size: x"),
            (
                "type=bind,bind-nonrecursive",
                "bind-nonrecursive is deprecated, use bind-recursive=disabled instead",
            ),
            (
                "type=bind,src= /a",
                "invalid value for 'src' in 'src= /a': value should not have whitespace",
            ),
        ] {
            assert_eq!(parse_mount(given, None), Err(said.to_string()), "{given}");
        }
        let t = parse_mount("type=tmpfs,dst=/t,tmpfs-size=64m,tmpfs-mode=1770", None).unwrap();
        assert_eq!(
            t.tmpfs,
            Some(Tmpfs {
                size_bytes: 64 << 20,
                mode: 0o1770
            })
        );
        assert_eq!(parse_mount(&encode(&t), None).unwrap(), t);
    }

    #[test]
    fn volumes_read_as_volumespec_reads_them() {
        let v = |s: &str| parse_volume(s).map(|v| (v.kind, v.source, v.target));
        assert_eq!(v("/data"), Ok(("volume".into(), "".into(), "/data".into())));
        assert_eq!(
            v("name:/data"),
            Ok(("volume".into(), "name".into(), "/data".into()))
        );
        assert_eq!(
            v("/host:/data:ro"),
            Ok(("bind".into(), "/host".into(), "/data".into()))
        );
        assert_eq!(v("./h:/d"), Ok(("bind".into(), "./h".into(), "/d".into())));
        assert_eq!(
            v("/a::b"),
            Err("invalid spec: /a::b: empty section between colons".into())
        );
        // A one-letter source is a Windows drive, as Go's reads it.
        assert_eq!(v("a::b"), Ok(("bind".into(), "a:".into(), "b".into())));
        assert_eq!(
            v("/a:/b:ro:x"),
            Err("invalid spec: /a:/b:ro:x: too many colons".into())
        );
        assert_eq!(v(""), Err("invalid empty volume spec".into()));
    }
}
