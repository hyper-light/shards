//! Docker's seccomp profiles: decoded as dockerd decodes one (moby/profiles seccomp
//! v0.2.3, seccomp.go, by Go's encoding/json), resolved for a container as its
//! setupSeccomp resolves one (seccomp_linux.go: the architecture's archMap entry, and the
//! rules whose includes and excludes the container's capabilities, architecture and
//! kernel meet), and converted as runc v1.3.4's specconv.SetupSeccomp converts the result
//! (libcontainer/specconv, libcontainer/seccomp/config.go), each refusal in its words.

use crate::json::Json;

/// A guest architecture, by its Go name (GOARCH), which a profile's `arches` say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Amd64,
    Arm64,
}

impl Arch {
    pub const fn go_name(self) -> &'static str {
        match self {
            Arch::Amd64 => "amd64",
            Arch::Arm64 => "arm64",
        }
    }

    /// Its seccomp architecture (moby's nativeToSeccomp).
    pub const fn scmp(self) -> &'static str {
        match self {
            Arch::Amd64 => "SCMP_ARCH_X86_64",
            Arch::Arm64 => "SCMP_ARCH_AARCH64",
        }
    }

    /// This build's own.
    pub const fn host() -> Option<Arch> {
        if cfg!(target_arch = "x86_64") {
            Some(Arch::Amd64)
        } else if cfg!(target_arch = "aarch64") {
            Some(Arch::Arm64)
        } else {
            None
        }
    }
}

/// A kernel's "kernel" and "major revision" (moby's KernelVersion).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Kernel(pub u64, pub u64);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    pub caps: Vec<String>,
    pub arches: Vec<String>,
    pub min_kernel: Option<Kernel>,
}

/// An argument comparison (runtime-spec LinuxSeccompArg).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Arg {
    pub index: u64,
    pub value: u64,
    pub value_two: u64,
    pub op: String,
}

/// A profile's rule (moby's Syscall).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Syscall {
    pub names: Vec<String>,
    pub name: String,
    pub action: String,
    pub errno_ret: Option<u64>,
    pub args: Vec<Arg>,
    pub includes: Option<Filter>,
    pub excludes: Option<Filter>,
}

/// A profile (moby's Seccomp).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Profile {
    pub default_action: String,
    pub default_errno_ret: Option<u64>,
    pub architectures: Vec<String>,
    /// None where the profile names none, which differs from an empty list (runc then
    /// adds SECCOMP_FILTER_FLAG_SPEC_ALLOW).
    pub flags: Option<Vec<String>>,
    pub listener_path: String,
    pub arch_map: Vec<(String, Vec<String>)>,
    /// A `null` in the list is a rule Go decodes as nil.
    pub syscalls: Vec<Option<Syscall>>,
}

/// Docker's default profile, as moby/profiles seccomp v0.2.3 ships it (default.json,
/// generated from its DefaultProfile).
pub const DEFAULT: &[u8] = include_bytes!("../data/default.json");

fn type_error(value: &Json, path: &str, ty: &str) -> String {
    let value = match value {
        Json::Number(n) => format!("number {n}"),
        other => other.kind().to_string(),
    };
    format!("json: cannot unmarshal {value} into Go struct field {path} of type {ty}")
}

fn string(j: &Json, field: &str, path: &str) -> Result<String, String> {
    match j.value(field) {
        None => Ok(String::new()),
        Some(Json::String(s)) => Ok(s.clone()),
        Some(other) => Err(type_error(other, path, "string")),
    }
}

fn uint(v: &Json, path: &str, ty: &str) -> Result<u64, String> {
    match v {
        Json::Number(n) => n.as_u64().ok_or_else(|| type_error(v, path, ty)),
        other => Err(type_error(other, path, ty)),
    }
}

fn strings(j: &Json, field: &str, path: &str) -> Result<Option<Vec<String>>, String> {
    match j.field(field) {
        None => Ok(None),
        Some(Json::Array(items)) => items
            .iter()
            .map(|i| match i {
                Json::String(s) => Ok(s.clone()),
                Json::Null => Ok(String::new()),
                other => Err(type_error(other, path, "string")),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(other) => Err(type_error(other, path, "[]string")),
    }
}

fn kernel(raw: &Json) -> Result<Option<Kernel>, String> {
    let shown = match raw {
        Json::String(s) => serde_json::Value::String(s.clone()).to_string(),
        Json::Number(n) => n.to_string(),
        other => other.kind().to_string(),
    };
    let invalid = |why: &str| format!(r#"invalid kernel version: {shown}, expected "<kernel>.<major>"{why}"#);
    let Json::String(ver) = raw else {
        return Err(invalid(&format!(
            ": json: cannot unmarshal {} into Go value of type string",
            raw.kind()
        )));
    };
    if ver.is_empty() {
        return Ok(Some(Kernel(0, 0)));
    }
    let parts: Vec<&str> = ver.splitn(3, '.').collect();
    let [k, m] = parts.as_slice() else {
        return Err(invalid(""));
    };
    // strconv.ParseUint(s, 10, 8).
    let parse = |s: &str| -> Result<u64, String> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid(&format!(
                ": strconv.ParseUint: parsing {s:?}: invalid syntax"
            )));
        }
        match s.parse::<u64>() {
            Ok(n) if n <= 255 => Ok(n),
            _ => Err(invalid(&format!(
                ": strconv.ParseUint: parsing {s:?}: value out of range"
            ))),
        }
    };
    let (k, m) = (parse(k)?, parse(m)?);
    if k == 0 && m == 0 {
        return Err(invalid(": version cannot be 0.0"));
    }
    Ok(Some(Kernel(k, m)))
}

fn filter(j: Option<&Json>, path: &str) -> Result<Option<Filter>, String> {
    let Some(j) = j else {
        return Ok(None);
    };
    let Json::Object(_) = j else {
        return Err(type_error(j, path, "seccomp.Filter"));
    };
    Ok(Some(Filter {
        caps: strings(j, "caps", &format!("{path}.caps"))?.unwrap_or_default(),
        arches: strings(j, "arches", &format!("{path}.arches"))?.unwrap_or_default(),
        min_kernel: match j.field("minKernel") {
            None => None,
            Some(raw) => kernel(raw)?,
        },
    }))
}

fn syscall(j: &Json) -> Result<Option<Syscall>, String> {
    let path = "Seccomp.syscalls";
    match j {
        Json::Null => return Ok(None),
        Json::Object(_) => {}
        other => return Err(type_error(other, path, "seccomp.Syscall")),
    }
    let args = match j.field("args") {
        None => Vec::new(),
        Some(Json::Array(items)) => items
            .iter()
            .map(|a| {
                let at = format!("{path}.args");
                match a {
                    Json::Null => Ok(Arg::default()),
                    Json::Object(_) => {
                        let num = |f: &str, ty: &str| -> Result<u64, String> {
                            a.value(f).map_or(Ok(0), |v| uint(v, &format!("{at}.{f}"), ty))
                        };
                        Ok(Arg {
                            index: num("index", "uint")?,
                            value: num("value", "uint64")?,
                            value_two: num("valueTwo", "uint64")?,
                            op: string(a, "op", &format!("{at}.op"))?,
                        })
                    }
                    other => Err(type_error(other, &at, "specs.LinuxSeccompArg")),
                }
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(other) => {
            return Err(type_error(
                other,
                &format!("{path}.args"),
                "[]specs.LinuxSeccompArg",
            ));
        }
    };
    Ok(Some(Syscall {
        names: strings(j, "names", &format!("{path}.names"))?.unwrap_or_default(),
        name: string(j, "name", &format!("{path}.name"))?,
        action: string(j, "action", &format!("{path}.action"))?,
        errno_ret: j
            .field("errnoRet")
            .map(|v| uint(v, &format!("{path}.errnoRet"), "uint"))
            .transpose()?,
        args,
        includes: filter(j.field("includes"), &format!("{path}.includes"))?,
        excludes: filter(j.field("excludes"), &format!("{path}.excludes"))?,
    }))
}

/// A profile as dockerd decodes it (LoadProfile): its refusal is "Decoding seccomp
/// profile failed: …".
pub fn decode(bytes: &[u8]) -> Result<Profile, String> {
    let failed = |e: String| format!("Decoding seccomp profile failed: {e}");
    let j = Json::parse(bytes).map_err(|e| failed(go_syntax_error(&e)))?;
    let decoded = (|| -> Result<Profile, String> {
        let Json::Object(_) = j else {
            return Err(type_error(&j, "Seccomp", "seccomp.Seccomp")
                .replace(" into Go struct field Seccomp of type", " into Go value of type"));
        };
        let arch_map = match j.field("archMap") {
            None => Vec::new(),
            Some(Json::Array(items)) => items
                .iter()
                .map(|a| {
                    Ok((
                        string(a, "architecture", "Seccomp.archMap.architecture")?,
                        strings(a, "subArchitectures", "Seccomp.archMap.subArchitectures")?
                            .unwrap_or_default(),
                    ))
                })
                .collect::<Result<Vec<_>, String>>()?,
            Some(other) => return Err(type_error(other, "Seccomp.archMap", "[]seccomp.Architecture")),
        };
        let syscalls = match j.field("syscalls") {
            None => Vec::new(),
            Some(Json::Array(items)) => items.iter().map(syscall).collect::<Result<Vec<_>, _>>()?,
            Some(other) => return Err(type_error(other, "Seccomp.syscalls", "[]*seccomp.Syscall")),
        };
        Ok(Profile {
            default_action: string(&j, "defaultAction", "Seccomp.defaultAction")?,
            default_errno_ret: j
                .field("defaultErrnoRet")
                .map(|v| uint(v, "Seccomp.defaultErrnoRet", "uint"))
                .transpose()?,
            architectures: strings(&j, "architectures", "Seccomp.architectures")?.unwrap_or_default(),
            flags: strings(&j, "flags", "Seccomp.flags")?,
            listener_path: string(&j, "listenerPath", "Seccomp.listenerPath")?,
            arch_map,
            syscalls,
        })
    })();
    decoded.map_err(failed)
}

/// Go's words for JSON it cannot read, where serde_json's say the same thing.
fn go_syntax_error(e: &str) -> String {
    if e.starts_with("EOF while parsing") {
        "unexpected end of JSON input".to_string()
    } else {
        e.to_string()
    }
}

/// A rule as moby resolves it for a container: a runtime-spec LinuxSyscall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub names: Vec<String>,
    pub action: String,
    pub errno_ret: Option<u64>,
    pub args: Vec<Arg>,
}

/// A profile as moby resolves it for a container: a runtime-spec LinuxSeccomp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub default_action: String,
    pub default_errno_ret: Option<u64>,
    pub architectures: Vec<String>,
    pub flags: Option<Vec<String>>,
    pub listener_path: String,
    pub syscalls: Vec<Resolved>,
}

/// The container a profile is resolved for: its architecture, the capabilities of its
/// bounding set (`CAP_…`), and the kernel it runs.
#[derive(Debug, Clone)]
pub struct Container<'a> {
    pub arch: Arch,
    pub caps: &'a [String],
    pub kernel: Kernel,
}

/// setupSeccomp: none where the profile asks for none (no default action and no rules).
pub fn resolve(p: &Profile, c: &Container<'_>) -> Result<Option<Spec>, String> {
    if p.default_action.is_empty() && p.syscalls.is_empty() {
        return Ok(None);
    }
    if !p.architectures.is_empty() && !p.arch_map.is_empty() {
        return Err("both 'architectures' and 'archMap' are specified in the seccomp profile, use either 'architectures' or 'archMap'".into());
    }
    let mut architectures = p.architectures.clone();
    if let Some((a, subs)) = p.arch_map.iter().find(|(a, _)| a == c.arch.scmp()) {
        architectures.push(a.clone());
        architectures.extend(subs.iter().cloned());
    }
    let arch = c.arch.go_name();
    let has = |cap: &String| c.caps.contains(cap);
    let mut syscalls = Vec::new();
    for call in &p.syscalls {
        let Some(call) = call else {
            // dockerd dereferences it, and fails.
            return Err("encountered nil syscall while initializing Seccomp".into());
        };
        let names = if call.name.is_empty() {
            call.names.clone()
        } else {
            if !call.names.is_empty() {
                return Err("both 'name' and 'names' are specified in the seccomp profile, use either 'name' or 'names'".into());
            }
            vec![call.name.clone()]
        };
        if let Some(x) = &call.excludes
            && (x.arches.iter().any(|a| a == arch)
                || x.caps.iter().any(has)
                || x.min_kernel.is_some_and(|k| c.kernel >= k))
        {
            continue;
        }
        if let Some(i) = &call.includes
            && ((!i.arches.is_empty() && !i.arches.iter().any(|a| a == arch))
                || !i.caps.iter().all(has)
                || i.min_kernel.is_some_and(|k| c.kernel < k))
        {
            continue;
        }
        syscalls.push(Resolved {
            names,
            action: call.action.clone(),
            errno_ret: call.errno_ret,
            args: call.args.clone(),
        });
    }
    Ok(Some(Spec {
        default_action: p.default_action.clone(),
        default_errno_ret: p.default_errno_ret,
        architectures,
        flags: p.flags.clone(),
        listener_path: p.listener_path.clone(),
        syscalls,
    }))
}

/// A seccomp action (runc's configs.Action).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Kill,
    Errno,
    Trap,
    Allow,
    Trace,
    Log,
    Notify,
    KillThread,
    KillProcess,
}

fn action(s: &str) -> Result<Action, String> {
    Ok(match s {
        "SCMP_ACT_KILL" => Action::Kill,
        "SCMP_ACT_ERRNO" => Action::Errno,
        "SCMP_ACT_TRAP" => Action::Trap,
        "SCMP_ACT_ALLOW" => Action::Allow,
        "SCMP_ACT_TRACE" => Action::Trace,
        "SCMP_ACT_LOG" => Action::Log,
        "SCMP_ACT_NOTIFY" => Action::Notify,
        "SCMP_ACT_KILL_THREAD" => Action::KillThread,
        "SCMP_ACT_KILL_PROCESS" => Action::KillProcess,
        _ => return Err(format!("string {s} is not a valid action for seccomp")),
    })
}

fn operator(s: &str) -> Result<crate::db::Op, String> {
    use crate::db::Op;
    Ok(match s {
        "SCMP_CMP_NE" => Op::Ne,
        "SCMP_CMP_LT" => Op::Lt,
        "SCMP_CMP_LE" => Op::Le,
        "SCMP_CMP_EQ" => Op::Eq,
        "SCMP_CMP_GE" => Op::Ge,
        "SCMP_CMP_GT" => Op::Gt,
        "SCMP_CMP_MASKED_EQ" => Op::MaskedEq,
        _ => return Err(format!("string {s} is not a valid operator for seccomp")),
    })
}

/// runc's architecture names for a profile's (ConvertStringToArch).
fn runc_arch(s: &str) -> Result<&'static str, String> {
    Ok(match s {
        "SCMP_ARCH_X86" => "x86",
        "SCMP_ARCH_X86_64" => "amd64",
        "SCMP_ARCH_X32" => "x32",
        "SCMP_ARCH_ARM" => "arm",
        "SCMP_ARCH_AARCH64" => "arm64",
        "SCMP_ARCH_MIPS" => "mips",
        "SCMP_ARCH_MIPS64" => "mips64",
        "SCMP_ARCH_MIPS64N32" => "mips64n32",
        "SCMP_ARCH_MIPSEL" => "mipsel",
        "SCMP_ARCH_MIPSEL64" => "mipsel64",
        "SCMP_ARCH_MIPSEL64N32" => "mipsel64n32",
        "SCMP_ARCH_PPC" => "ppc",
        "SCMP_ARCH_PPC64" => "ppc64",
        "SCMP_ARCH_PPC64LE" => "ppc64le",
        "SCMP_ARCH_RISCV64" => "riscv64",
        "SCMP_ARCH_S390" => "s390",
        "SCMP_ARCH_S390X" => "s390x",
        _ => return Err(format!("string {s} is not a valid arch for seccomp")),
    })
}

/// The seccomp() flags runc knows (config.go).
pub const FLAG_TSYNC: &str = "SECCOMP_FILTER_FLAG_TSYNC";
pub const FLAG_LOG: &str = "SECCOMP_FILTER_FLAG_LOG";
pub const FLAG_SPEC_ALLOW: &str = "SECCOMP_FILTER_FLAG_SPEC_ALLOW";

/// A rule as runc takes it: one syscall name (configs.Syscall).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub name: String,
    pub action: Action,
    pub errno_ret: Option<u64>,
    pub args: Vec<(u64, crate::db::Op, u64, u64)>,
}

/// A profile as runc takes it (configs.Seccomp).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub default_action: Action,
    pub default_errno_ret: Option<u64>,
    /// runc's names: `amd64`, `x86`, `x32`, `arm64`, `arm`, ….
    pub architectures: Vec<&'static str>,
    pub flags: Vec<String>,
    pub listener_path: String,
    pub syscalls: Vec<Rule>,
}

/// specconv.SetupSeccomp.
pub fn convert(spec: &Spec) -> Result<Option<Config>, String> {
    if spec.default_action.is_empty() && spec.syscalls.is_empty() {
        return Ok(None);
    }
    let flags = match &spec.flags {
        // runc's default: SECCOMP_FILTER_FLAG_SPEC_ALLOW, which the guest kernel supports.
        None => vec![FLAG_SPEC_ALLOW.to_string()],
        Some(flags) => {
            for f in flags {
                if ![FLAG_TSYNC, FLAG_LOG, FLAG_SPEC_ALLOW].contains(&f.as_str()) {
                    return Err(format!("seccomp flag {f} is not known to runc"));
                }
            }
            flags.clone()
        }
    };
    let architectures = spec
        .architectures
        .iter()
        .map(|a| runc_arch(a))
        .collect::<Result<Vec<_>, _>>()?;
    let default_action = action(&spec.default_action)?;
    let mut syscalls = Vec::new();
    for call in &spec.syscalls {
        let act = action(&call.action)?;
        for name in &call.names {
            let args = call
                .args
                .iter()
                .map(|a| Ok((a.index, operator(&a.op)?, a.value, a.value_two)))
                .collect::<Result<Vec<_>, String>>()?;
            syscalls.push(Rule {
                name: name.clone(),
                action: act,
                errno_ret: call.errno_ret,
                args,
            });
        }
    }
    Ok(Some(Config {
        default_action,
        default_errno_ret: spec.default_errno_ret,
        architectures,
        flags,
        listener_path: spec.listener_path.clone(),
        syscalls,
    }))
}
