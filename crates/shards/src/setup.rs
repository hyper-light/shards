//! `run`'s flags that set up the workload's namespaces: `--read-only`, `--tmpfs`,
//! `--shm-size`, `--ulimit` and `--sysctl`, as dockerd checks them as it makes a container
//! (moby docker-v29.8.1 daemon/daemon_unix.go, verifyPlatformContainerSettings) and turns
//! them into the OCI spec as it starts one (daemon/oci_linux.go: setMounts, withRlimits,
//! WithSysctls), for shards-init to apply as runc v1.5.1 does.
// Only Unix has the daemon, so far.
#![cfg_attr(not(unix), allow(dead_code))]

use shards_abi::run::nesting::Grant;
use shards_ipc::Run;

/// CAP_SYS_ADMIN and CAP_NET_ADMIN, by the kernel's numbers (include/uapi/linux/capability.h).
const CAP_SYS_ADMIN: u32 = 21;
const CAP_NET_ADMIN: u32 = 12;

/// Go's path.Clean, for absolute and relative slash-separated paths.
fn clean(p: &str) -> String {
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

/// The driver option an endpoint's sysctls come in (moby libnetwork netlabel.EndpointSysctls).
pub const ENDPOINT_SYSCTLS: &str = "com.docker.network.endpoint.sysctls";

/// The sysctls the endpoint of `run`'s own network gives its interface (D46, PM M171),
/// each `net.X.Y.IFNAME.Z=V` as given: none on `none`, which has no interface, where
/// dockerd ignores them.
pub fn endpoint_sysctls(run: &Run) -> Vec<String> {
    if run.network == "none" {
        return Vec::new();
    }
    own_endpoint(run)
        .and_then(|e| e.driver_opts.iter().find(|(k, _)| k == ENDPOINT_SYSCTLS))
        .map(|(_, v)| v.split(',').filter(|s| !s.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

/// The endpoint of `run`'s own network, its mode's: the default bridge's whichever name
/// says it.
pub fn own_endpoint(run: &Run) -> Option<&shards_ipc::Endpoint> {
    let ours = |n: &str| {
        n == run.network
            || matches!(
                (n, run.network.as_str()),
                ("bridge", "default") | ("default", "bridge")
            )
    };
    run.endpoints.iter().find(|e| ours(&e.network))
}

/// docker/cli's map of `--tmpfs`: each destination's options, the last given for it
/// (container/opts.go).
fn tmpfs_map(run: &Run) -> std::collections::BTreeMap<String, String> {
    run.tmpfs
        .iter()
        .map(|t| {
            let (dest, opts) = t.split_once(':').unwrap_or((t.as_str(), ""));
            (dest.to_string(), opts.to_string())
        })
        .collect()
}

/// moby's NormalizeLegacyCapabilities (daemon/pkg/oci/caps/utils.go): each a known name,
/// or `ALL`.
fn known_caps(caps: &[String], which: &str) -> Result<(), String> {
    for c in caps {
        if c != "ALL" && !shards_abi::run::CAP_NAMES.contains(&c.as_str()) {
            return Err(format!(
                "invalid {which}: unknown capability: {}",
                shards_cmdline::go::quote(c)
            ));
        }
    }
    Ok(())
}

/// moby's TweakCapabilities: every capability for a privileged container; else the
/// defaults, less those dropped, and those added; `ALL` added is all but those dropped,
/// and `ALL` dropped is those added alone. As a set of capability numbers.
pub fn capabilities(run: &Run) -> u64 {
    let names = &shards_abi::run::CAP_NAMES;
    let all = (1u64 << names.len()) - 1;
    let mask = |list: &[String]| {
        list.iter()
            .filter_map(|c| names.iter().position(|n| n == c))
            .fold(0u64, |m, i| m | 1 << i)
    };
    if run.privileged {
        return all;
    }
    let (add, drop) = (mask(&run.cap_add), mask(&run.cap_drop));
    if run.cap_add.iter().any(|c| c == "ALL") {
        return all & !drop;
    }
    if run.cap_drop.iter().any(|c| c == "ALL") {
        return add;
    }
    let defaults = shards_abi::run::CAPS.iter().fold(0u64, |m, &c| m | 1 << c);
    (defaults & !drop) | add
}

/// A microVM's `--security-opt`, as dockerd reads them as it makes one (moby
/// daemon/daemon_unix.go, parseSecurityOpt): each `KEY=VALUE`, else `KEY:VALUE` (which it
/// calls deprecated), but `no-new-privileges`, `writable-cgroups` and `disable` alone.
/// Labels are kept, as on a host without SELinux, which the guest kernel has none of; an
/// AppArmor profile's name says only whether docker-default's mount rule holds (`MOUNTS`).
/// `nesting` is shards' own key (D234), where dockerd refuses every key it does not know.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Security {
    pub no_new_privileges: bool,
    pub writable_cgroups: Option<bool>,
    /// `seccomp=`'s value: `unconfined`, `builtin`, a profile's JSON, or none.
    pub seccomp: Option<String>,
    /// `apparmor=`'s value: `unconfined`, a profile's name, or none.
    pub apparmor: Option<String>,
    /// `nesting=true`: the run asks for the nesting grant (D234), which its image must
    /// declare too (crate::agentfile::nesting).
    pub nesting: bool,
}

pub fn security(run: &Run) -> Result<Security, String> {
    let mut out = Security::default();
    for opt in &run.security_opt {
        match opt.as_str() {
            "no-new-privileges" => {
                out.no_new_privileges = true;
                continue;
            }
            "writable-cgroups" => {
                out.writable_cgroups = Some(true);
                continue;
            }
            "disable" => continue,
            _ => {}
        }
        let cut = if opt.contains('=') {
            opt.split_once('=')
        } else {
            opt.split_once(':')
        };
        let Some((k, v)) = cut else {
            return Err(format!(
                "invalid --security-opt 1: {}",
                shards_cmdline::go::quote(opt)
            ));
        };
        let two = || format!("invalid --security-opt 2: {}", shards_cmdline::go::quote(opt));
        match k {
            "label" => {}
            "apparmor" => out.apparmor = Some(v.to_string()),
            "seccomp" => out.seccomp = Some(v.to_string()),
            "no-new-privileges" => {
                out.no_new_privileges = shards_cmdline::go::parse_bool(v).map_err(|_| two())?
            }
            "writable-cgroups" => {
                out.writable_cgroups = Some(shards_cmdline::go::parse_bool(v).map_err(|_| two())?)
            }
            "nesting" => out.nesting = shards_cmdline::go::parse_bool(v).map_err(|_| two())?,
            _ => return Err(two()),
        }
    }
    Ok(out)
}

/// The capabilities of the command of `run` given the nesting grant (D234): its own
/// ([`capabilities`]), Docker's default set where it adds and drops none, with
/// CAP_SYS_ADMIN and CAP_NET_ADMIN, which the grant always holds.
fn nesting_capabilities(run: &Run) -> u64 {
    capabilities(run) | 1 << CAP_SYS_ADMIN | 1 << CAP_NET_ADMIN
}

/// The capabilities of `mask`, a set of capability numbers, by name.
fn capability_names(mask: u64) -> Vec<String> {
    shards_abi::run::CAP_NAMES
        .iter()
        .enumerate()
        .filter(|(i, _)| mask & (1u64 << i) != 0)
        .map(|(_, n)| (*n).to_string())
        .collect()
}

/// The seccomp filter a build's `RUN` steps run under, as BuildKit gives each step
/// moby's default profile (its executor's `Seccomp: 2`, oci/spec.go
/// WithDefaultSeccomp): compiled for a step's capabilities, the defaults a run
/// has (shards_abi::run::CAPS), and the builder's kernel; as a step carries it
/// ([`shards_abi::build::Step::seccomp`]), empty where the profile asks for none.
pub fn step_seccomp(kernel: shards_seccomp::Kernel) -> Result<Vec<u8>, String> {
    let caps: Vec<String> = shards_abi::run::CAPS
        .iter()
        .filter_map(|&c| shards_abi::run::CAP_NAMES.get(usize::try_from(c).ok()?))
        .map(|n| (*n).to_string())
        .collect();
    let arch = shards_seccomp::Arch::host().ok_or("seccomp needs an amd64 or arm64 guest")?;
    let c = shards_seccomp::Container {
        arch,
        caps: &caps,
        kernel,
    };
    Ok(shards_seccomp::compile(shards_seccomp::DEFAULT, &c)?
        .map(|p| {
            let mut e = p.flags.to_le_bytes().to_vec();
            for i in &p.insns {
                e.extend_from_slice(&i.to_ne_bytes());
            }
            e
        })
        .unwrap_or_default())
}

/// The seccomp filter a microVM's workload runs under, as moby chooses its profile
/// (daemon/seccomp_linux.go, WithSeccomp): none for `unconfined`; a privileged
/// run's only where it names one; else the one it names or Docker's default. As the
/// setup entry init loads it by (`seccomp=`, the seccomp(2) flags and the program); none
/// where the profile asks for none. Each profile, set of capabilities and kernel is
/// compiled once.
///
/// moby reads `builtin` named for a privileged container as a profile's JSON, and fails
/// ("invalid character 'b'"): here it is the default profile, as it is for any other.
///
/// With the nesting grant (D234), compiled for the grant's capabilities, and the default
/// profile its own ([`nesting_profile`]).
pub fn seccomp(
    run: &Run,
    security: &Security,
    kernel: shards_seccomp::Kernel,
    nesting: bool,
) -> Result<Option<Vec<u8>>, String> {
    let nested;
    let profile: &[u8] = match security.seccomp.as_deref() {
        Some("unconfined") => return Ok(None),
        None | Some("") if run.privileged => return Ok(None),
        None | Some("" | "builtin") if nesting => {
            nested = nesting_profile()?;
            &nested
        }
        None | Some("" | "builtin") => shards_seccomp::DEFAULT,
        Some(json) => json.as_bytes(),
    };
    let caps = if nesting {
        nesting_capabilities(run)
    } else {
        capabilities(run)
    };
    compiled(profile, caps, kernel, b"seccomp=", false)
}

/// The profile a command given the nesting grant runs under (D234): Docker's default, with
/// a rule allowing `pivot_root`, which it has none for, so that the boxes the command makes
/// take their roots (moby/profiles seccomp v0.2.3, f16). The rest is the default's: what it
/// opens only for a capability, it opens for the grant's; what it refuses whatever the
/// capabilities (AF_ALG and AF_VSOCK sockets, io_uring, userfaultfd, keyctl, kexec), it
/// refuses.
fn nesting_profile() -> Result<Vec<u8>, String> {
    let mut p: serde_json::Value = serde_json::from_slice(shards_seccomp::DEFAULT)
        .map_err(|e| format!("the default seccomp profile: {e}"))?;
    p.get_mut("syscalls")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or("the default seccomp profile has no syscalls")?
        .push(serde_json::json!({"names": ["pivot_root"], "action": "SCMP_ACT_ALLOW"}));
    serde_json::to_vec(&p).map_err(|e| e.to_string())
}

/// What docker-default, Docker's AppArmor profile, refuses a container with CAP_SYS_ADMIN
/// where AppArmor is enforced (`deny mount,`, PM M174), as a filter of its own, for guests,
/// which have no AppArmor (`seccomp-mounts=`, shards_seccomp::MOUNTS): mount(2), and past
/// docker-default the new mount API and pivot_root (D121). So a run given SYS_ADMIN
/// mounts nothing of its microVM's, its shares among them. None for a privileged run
/// or `apparmor=unconfined`, which ask for no confinement, nor without the capability: the
/// kernel refuses every mount then, and a filter would change only mount(2)'s errno (AppArmor's
/// EACCES for the kernel's EPERM) while putting one on a run `seccomp=unconfined`
/// asked to have none.
pub fn mounts(
    run: &Run,
    security: &Security,
    kernel: shards_seccomp::Kernel,
) -> Result<Option<Vec<u8>>, String> {
    if run.privileged
        || security.apparmor.as_deref() == Some("unconfined")
        || capabilities(run) & (1 << CAP_SYS_ADMIN) == 0
    {
        return Ok(None);
    }
    compiled(
        shards_seccomp::MOUNTS,
        capabilities(run),
        kernel,
        b"seccomp-mounts=",
        false,
    )
}

/// On the host's network (`--network host`, D124), or joining a microVM there: the
/// filter by which init makes each bind(2) and listen(2) of the run's processes the host's
/// (`seccomp-binds=`, shards_seccomp::BINDS), loaded first and for a listener.
pub fn binds(run: &Run, kernel: shards_seccomp::Kernel) -> Result<Option<Vec<u8>>, String> {
    compiled(
        shards_seccomp::BINDS,
        capabilities(run),
        kernel,
        b"seccomp-binds=",
        true,
    )
}

/// `profile` compiled for the capabilities `caps` (a set of numbers) on `kernel`, as the
/// setup entry `name` names: its seccomp(2) flags, then its instructions; shards' own whose
/// notifications init answers where `agent`. Each profile, set of capabilities and kernel
/// is compiled once.
fn compiled(
    profile: &[u8],
    caps: u64,
    kernel: shards_seccomp::Kernel,
    name: &[u8],
    agent: bool,
) -> Result<Option<Vec<u8>>, String> {
    let arch = shards_seccomp::Arch::host().ok_or("seccomp needs an amd64 or arm64 guest")?;
    let version = kernel;
    let caps = capability_names(caps);
    type Key = (Vec<u8>, Vec<u8>, Vec<String>, shards_seccomp::Kernel);
    static COMPILED: std::sync::Mutex<std::collections::BTreeMap<Key, Option<Vec<u8>>>> =
        std::sync::Mutex::new(std::collections::BTreeMap::new());
    let key = (name.to_vec(), profile.to_vec(), caps, version);
    let cached = COMPILED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .cloned();
    if let Some(entry) = cached {
        return Ok(entry);
    }
    let c = shards_seccomp::Container {
        arch,
        caps: &key.2,
        kernel: version,
    };
    let program = if agent {
        shards_seccomp::compile_agent(profile, &c)?
    } else {
        shards_seccomp::compile(profile, &c)?
    };
    let entry = program.map(|p| {
        let mut e = name.to_vec();
        e.extend_from_slice(&p.flags.to_le_bytes());
        for i in &p.insns {
            e.extend_from_slice(&i.to_ne_bytes());
        }
        e
    });
    let mut compiled = COMPILED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // Bounded by what a daemon's runs have asked: a profile is a run's own request.
    if compiled.len() >= 256 {
        compiled.clear();
    }
    compiled.insert(key, entry.clone());
    Ok(entry)
}

/// The socket families a domain may open (§9.7): Unix, IPv4 and IPv6, by their Linux
/// numbers (include/linux/socket.h), which are every architecture's.
const DOMAIN_FAMILIES: [u64; 3] = [1, 2, 10];

/// The profile an agent's or harness's domain runs under (D59, AGENTFILE_ARCH.md §9.3):
/// Docker's default, compiled for no capability, so that its rules for capabilities give
/// nothing; and its `socket` and `socketpair` rules, which refuse AF_ALG and AF_VSOCK alone,
/// replaced by `socket` for the families above and `socketpair` for Unix alone. What a
/// domain must not call besides (io_uring, keyctl, add_key, request_key, userfaultfd) is on
/// no allow list of the default; bpf and perf_event_open only on capabilities' lists.
///
/// With `no_processes` (`--processes=none`, §9.9): no `fork` or `vfork`, and `clone` only
/// for a thread, `CLONE_THREAD` set and no namespace flag (Docker's own mask, 0x7E020000,
/// with CLONE_THREAD's bit, include/uapi/linux/sched.h); `clone3`, which a filter cannot
/// read the flags of, already fails with ENOSYS for no capability, and libc then clones.
fn domain_profile(no_processes: bool) -> Result<Vec<u8>, String> {
    let mut p: serde_json::Value = serde_json::from_slice(shards_seccomp::DEFAULT)
        .map_err(|e| format!("the default seccomp profile: {e}"))?;
    let rules = p
        .get_mut("syscalls")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or("the default seccomp profile has no syscalls")?;
    let refused: &[&str] = if no_processes {
        &["socket", "socketpair", "fork", "vfork", "clone"]
    } else {
        &["socket", "socketpair"]
    };
    for r in rules.iter_mut() {
        if let Some(names) = r.get_mut("names").and_then(serde_json::Value::as_array_mut) {
            names.retain(|n| !refused.iter().any(|r| n == r));
        }
    }
    rules.retain(|r| {
        r.get("names")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|n| !n.is_empty())
    });
    let allow = |name: &str, family: u64| {
        serde_json::json!({
            "names": [name],
            "action": "SCMP_ACT_ALLOW",
            "args": [{"index": 0, "value": family, "op": "SCMP_CMP_EQ"}],
        })
    };
    for family in DOMAIN_FAMILIES {
        rules.push(allow("socket", family));
    }
    rules.push(allow("socketpair", 1));
    if no_processes {
        const CLONE_THREAD: u64 = 0x0001_0000;
        rules.push(serde_json::json!({
            "names": ["clone"],
            "action": "SCMP_ACT_ALLOW",
            "args": [{"index": 0, "value": 0x7E02_0000u64 | CLONE_THREAD, "valueTwo": CLONE_THREAD, "op": "SCMP_CMP_MASKED_EQ"}],
        }));
    }
    serde_json::to_vec(&p).map_err(|e| e.to_string())
}

/// The setup entries of the filters the domains of the run's image run under
/// (`domains-seccomp=` and, for `--processes=none`, `domains-seccomp-none=`, then as
/// `seccomp=`), each compiled once per kernel.
pub fn domain_seccomp(kernel: shards_seccomp::Kernel) -> Result<[Vec<u8>; 2], String> {
    Ok([domain_filter(kernel, false)?, domain_filter(kernel, true)?])
}

/// Where this host's OS keeps its users' files and mounts their volumes, the run's domains'
/// plug roots for what is plugged into them later (D235): on macOS the home directories
/// and mountable disks of Apple's standard directories (File System Programming Guide,
/// "macOS Standard Directories"); on Linux the Filesystem Hierarchy Standard 3.0's users'
/// and mounted data (§3.8 `/home`, §3.11 `/media`, §3.12 `/mnt`, §3.17 `/srv`); on Windows
/// WSL's root of the host's drives, drive C: at `/mnt/c` (DrvFs's `automount.root`).
#[cfg(target_os = "macos")]
const STANDARD_ROOTS: &[&str] = &["/Users", "/Volumes"];
#[cfg(target_os = "linux")]
const STANDARD_ROOTS: &[&str] = &["/home", "/media", "/mnt", "/srv"];
#[cfg(windows)]
const STANDARD_ROOTS: &[&str] = &["/mnt"];
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
const STANDARD_ROOTS: &[&str] = &[];

/// The setup entries of the plug roots of the run's domains (D235, `plug-root=/Users`):
/// this host's own, and the top-level directory of each host path the run's binds give it,
/// so that a project the run starts with names the root that what is plugged beside it
/// later lies beneath. One a domain may not be given paths beneath (`plug::root_name`:
/// `/tmp`, say) is named not at all.
pub fn plug_roots(points: &[crate::volumes::MountPoint]) -> Vec<Vec<u8>> {
    use shards_abi::run::plug;
    let mut roots: Vec<String> = STANDARD_ROOTS.iter().map(|r| (*r).to_string()).collect();
    // A Unix host's paths, the guest's at the same path; a Windows host's drives lie beneath
    // its own root (`/mnt`) already.
    if cfg!(unix) {
        for p in points.iter().filter(|p| p.kind == "bind") {
            let first = p
                .source
                .strip_prefix('/')
                .and_then(|rest| rest.split('/').next())
                .filter(|f| !f.is_empty());
            if let Some(root) = first.map(|f| format!("/{f}"))
                && !roots.contains(&root)
            {
                roots.push(root);
            }
        }
    }
    roots
        .into_iter()
        .filter(|r| plug::root_name(r.as_bytes()).is_some())
        .map(|r| [plug::ROOT_KEY, r.as_bytes()].concat())
        .collect()
}

fn domain_filter(kernel: shards_seccomp::Kernel, no_processes: bool) -> Result<Vec<u8>, String> {
    type Compiled = Vec<((shards_seccomp::Kernel, bool), Vec<u8>)>;
    static COMPILED: std::sync::Mutex<Compiled> = std::sync::Mutex::new(Vec::new());
    let key = (kernel, no_processes);
    if let Some((_, e)) = COMPILED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|(k, _)| *k == key)
    {
        return Ok(e.clone());
    }
    let arch = shards_seccomp::Arch::host().ok_or("seccomp needs an amd64 or arm64 guest")?;
    let c = shards_seccomp::Container {
        arch,
        caps: &[],
        kernel,
    };
    let program = shards_seccomp::compile(&domain_profile(no_processes)?, &c)?
        .ok_or("the domains' seccomp profile asks for none")?;
    let mut e = if no_processes {
        b"domains-seccomp-none=".to_vec()
    } else {
        b"domains-seccomp=".to_vec()
    };
    e.extend_from_slice(&program.flags.to_le_bytes());
    for i in &program.insns {
        e.extend_from_slice(&i.to_ne_bytes());
    }
    // One per guest kernel a daemon boots.
    COMPILED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((key, e.clone()));
    Ok(e)
}

/// What dockerd refuses of these as it makes a container: unknown capabilities
/// (validateCapabilities), an OOM score out of range, a `/dev/shm` below 0, and a tmpfs
/// at `/` or at a relative path (ValidateTmpfsMountDestination).
pub fn verify(run: &Run) -> Result<(), String> {
    security(run)?;
    known_caps(&run.cap_add, "CapAdd")?;
    known_caps(&run.cap_drop, "CapDrop")?;
    if !(-1000..=1000).contains(&run.oom_score_adj) {
        return Err(format!(
            "Invalid value {}, range for oom score adj is [-1000, 1000]",
            run.oom_score_adj
        ));
    }
    if run.shm_size < 0 {
        return Err("SHM size can not be less than 0".into());
    }
    // Its runtime (D155): its microVM, which `inspect` names `shards`, asked for by that
    // name, by runc's, the default runtime's, or by Kata's, whose runs are VMs too.
    if !runtime_is_the_microvm(&run.runtime) {
        return Err(format!(
            "--runtime {}: every run is a microVM of its own, whose runtime is shards (asked for by runc's or Kata's names too)",
            run.runtime
        ));
    }
    for dest in tmpfs_map(run).keys() {
        let dest = dest.replace('\\', "/");
        if clean(&dest) == "/" {
            return Err("invalid specification: destination can't be '/'".into());
        }
        if !dest.starts_with('/') {
            return Err(format!(
                "invalid mount path: '{dest}' mount path must be absolute"
            ));
        }
    }
    Ok(())
}

/// Whether `--runtime` `name` asks for what a run is, its microVM (D155): shards, runc by
/// either of its names (the default runtime), or Kata Containers', whose runs are VMs.
fn runtime_is_the_microvm(name: &str) -> bool {
    matches!(
        name,
        "" | "shards" | "runc" | "io.containerd.runc.v2" | "kata" | "kata-runtime"
    ) || name.starts_with("kata-")
        || (name.starts_with("io.containerd.kata") && name.ends_with(".v2"))
}

/// moby/sys/mount's flags table: each option that names a flag, and the flag it names,
/// which options of the same flag share (propagation's are one).
fn tmpfs_flag(option: &str) -> Option<&'static str> {
    Some(match option {
        "ro" | "rw" => "rdonly",
        "suid" | "nosuid" => "nosuid",
        "dev" | "nodev" => "nodev",
        "exec" | "noexec" => "noexec",
        "sync" | "async" => "synchronous",
        "dirsync" => "dirsync",
        "remount" => "remount",
        "mand" | "nomand" => "mandlock",
        "atime" | "noatime" => "noatime",
        "diratime" | "nodiratime" => "nodiratime",
        "bind" => "bind",
        "rbind" => "rbind",
        "relatime" | "norelatime" => "relatime",
        "strictatime" | "nostrictatime" => "strictatime",
        "unbindable" | "runbindable" | "private" | "rprivate" | "shared" | "rshared" | "slave" | "rslave" => {
            "propagation"
        }
        _ => return None,
    })
}

/// moby/sys/mount's MergeTmpfsOptions: the options with each flag and each data key
/// once, the last given of each kept, in the order they were last given.
pub(crate) fn merge_tmpfs_options(options: &[&str]) -> Result<Vec<String>, String> {
    const DATA: [&str; 8] = ["", "size", "mode", "uid", "gid", "nr_inodes", "nr_blocks", "mpol"];
    let mut flags_seen: Vec<&str> = Vec::new();
    let mut data_seen: Vec<&str> = Vec::new();
    let mut merged: Vec<String> = Vec::new();
    for &option in options.iter().rev() {
        if option == "defaults" {
            continue;
        }
        if let Some(flag) = tmpfs_flag(option) {
            if !flags_seen.contains(&flag) {
                merged.insert(0, option.to_string());
                flags_seen.push(flag);
            }
            continue;
        }
        let Some((key, _)) = option.split_once('=').filter(|(k, _)| DATA.contains(k)) else {
            let key = option.split_once('=').map_or(option, |(k, _)| k);
            return Err(format!("invalid tmpfs option {}", shards_cmdline::go::quote(key)));
        };
        if !data_seen.contains(&key) {
            merged.insert(0, option.to_string());
            data_seen.push(key);
        }
    }
    Ok(merged)
}

/// What the workload's namespaces are given, in order (shards_abi::run::Spec::setup), or
/// what dockerd says as it cannot start the container: its IPC namespace and `/dev/shm`'s
/// size, before anything is mounted, so that what the run mounts at or under /dev/shm (a
/// volume, a tmpfs) goes over it, as a container's mounts go over its default /dev/shm,
/// and is never resized nor unmounted with it; each tmpfs and mount point, the shallowest
/// first, so that a deeper one lands on it, with the tmpfs defaults (MergeTmpfsOptions)
/// before the run's; the ulimits, the last of each by name; the sysctls, the last of each;
/// and the root read-only last, after everything is mounted on it.
pub fn setup(
    run: &Run,
    points: &[(String, Vec<u8>)],
    cdi: Option<&crate::build::cdi::Edits>,
    microvm_shm: Option<u64>,
) -> Result<Vec<Vec<u8>>, String> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    // Its IPC namespace and /dev/shm (D156): none, or the microVM's, before the size of
    // one of its own; another run's is the daemon's to name (`shared_ipc_entry`).
    if run.ipc == "none" || run.ipc == "host" {
        out.push(format!("ipc={}", run.ipc).into_bytes());
    }
    if run.shm_size != 0 {
        out.push(format!("shm={}", run.shm_size).into_bytes());
    } else if let Some(bytes) = microvm_shm
        && run.ipc != "none"
        && !run.ipc.starts_with("container:")
    {
        // Its own, or the microVM's (`--ipc host`), as large as the microVM it lives in
        // may have them (`microvm_shm`): its template's is sized for every limit's.
        out.push(format!("shm={bytes}").into_bytes());
    }
    let mut mounts: Vec<(String, Vec<u8>)> = Vec::new();
    for (dest, opts) in tmpfs_map(run) {
        let dest = clean(&dest);
        let mut options = vec!["noexec", "nosuid", "nodev", "rprivate"];
        if !opts.is_empty() {
            options.extend(opts.split(','));
        }
        let merged = merge_tmpfs_options(&options)?;
        let entry = format!("tmpfs={dest}\0{}", merged.join(",")).into_bytes();
        mounts.push((dest, entry));
    }
    // The mount points' (volumes.rs, `open`), then sortMounts (daemon/volumes_unix.go):
    // fewer path parts first, so that a mount's parent's is under it.
    mounts.extend(points.iter().cloned());
    mounts.sort_by_key(|(dest, _)| dest.split('/').filter(|p| !p.is_empty()).count());
    out.extend(mounts.into_iter().map(|(_, entry)| entry));
    // The microVM's UTS and cgroup namespaces (D156), and its writable layer's size.
    if run.uts == "host" {
        out.push(b"uts=host".to_vec());
    }
    if run.cgroupns == "host" {
        out.push(b"cgroupns=host".to_vec());
    }
    if let Some(size) = storage_size(run)? {
        out.push(format!("rw-size={size}").into_bytes());
    }
    if let Some(mask) = run.umask {
        out.push(format!("umask={mask:o}").into_bytes());
    }
    for u in shards_cmdline::buildflags::ulimits(&run.ulimits)? {
        out.push(format!("ulimit={u}").into_bytes());
    }
    let sysctls: std::collections::BTreeMap<&str, &str> = run
        .sysctls
        .iter()
        .map(|s| s.split_once('=').unwrap_or((s.as_str(), "")))
        .collect();
    for (k, v) in sysctls {
        out.push(format!("sysctl={k}={v}").into_bytes());
    }
    // Its swappiness (D159): its microVM's, whose kernel is the run's own.
    if let Some(s) = run.resources.memory_swappiness {
        out.push(format!("sysctl=vm.swappiness={s}").into_bytes());
    }
    // The process's: capabilities, groups and OOM score, which its execs take too.
    if run.privileged || !run.cap_add.is_empty() || !run.cap_drop.is_empty() {
        out.push(format!("caps={}", capabilities(run)).into_bytes());
    }
    for g in &run.group_add {
        out.push(format!("group={g}").into_bytes());
    }
    // CDI's groups (AddProcessAdditionalGid): each but root's, once (D146).
    for g in cdi.map(|e| e.gids.as_slice()).unwrap_or_default() {
        let g = g.to_string();
        if g != "0" && !run.group_add.contains(&g) {
            out.push(format!("group={g}").into_bytes());
        }
    }
    if run.oom_score_adj != 0 {
        out.push(format!("oom={}", run.oom_score_adj).into_bytes());
    }
    // Privileged: what Docker gives such a container of the host's, of the VM's.
    if run.privileged {
        out.push(b"privileged".to_vec());
    }
    if run.read_only {
        out.push(b"readonly".to_vec());
    }
    // Where the command is born (D115): PID 1 of a PID namespace of its own, but for these;
    // dockerd's init only in a namespace of the container's own (moby daemon/oci_linux.go).
    if run.init_process == Some(true) && run.pid.is_empty() {
        out.push(b"init".to_vec());
    }
    // Its guest's real-time budget (D155), the run's alone in its microVM: each 0 to keep
    // the guest kernel's own.
    let r = &run.resources;
    if r.cpu_realtime_period > 0 || r.cpu_realtime_runtime > 0 {
        out.push(format!("rt={},{}", r.cpu_realtime_period, r.cpu_realtime_runtime).into_bytes());
    }
    if run.pid == "host" {
        out.push(b"pid=host".to_vec());
    } else if run.pid.starts_with("container:") {
        // The namespace of the workload whose network a joiner joins (D119), the one other
        // run's that the daemon lets a joiner join.
        out.push(b"pid=workload".to_vec());
    }
    Ok(out)
}

/// `--storage-opt`'s size of the run's writable layer, in bytes (D156): the one key a
/// microVM's writable layer, a tmpfs, takes, read as `--memory` is.
pub(crate) fn storage_size(run: &Run) -> Result<Option<u64>, String> {
    let mut size = None;
    for opt in &run.storage_opt {
        let (key, value) = opt.split_once('=').unwrap_or((opt.as_str(), ""));
        if !key.eq_ignore_ascii_case("size") {
            return Err(format!(
                "--storage-opt {opt}: a run's writable layer takes size alone, as size=10G"
            ));
        }
        let bytes = shards_cmdline::resources::ram_in_bytes(value)
            .ok()
            .and_then(|b| u64::try_from(b).ok())
            .filter(|&b| b > 0)
            .ok_or_else(|| {
                format!("--storage-opt {opt}: a size is a number of bytes above 0, as 10G or 512m")
            })?;
        size = Some(bytes);
    }
    Ok(size)
}

/// `--security-opt`'s setup entries, after the rest (`setup`, the run's composed so far):
/// the paths left unmasked, the cgroup writable, no new privileges and the seccomp filter,
/// which its execs take too (init installs the filter last, so that none of its own setup
/// is filtered). Made as the microVM starts: a profile it cannot load fails its start, not
/// its making (D41).
///
/// With the nesting grant (`nesting`, D234): the command's capabilities the grant's, in
/// place of what `setup` gave it; the grant's own entries, as init reads them again
/// (shards_abi::run::nesting::read); its filter ([`seccomp`]); and no mount filter, whose
/// mounts and pivot_root the grant is for.
pub fn security_setup(
    setup: &mut Vec<Vec<u8>>,
    run: &Run,
    kernel: shards_seccomp::Kernel,
    cdi: Option<&crate::build::cdi::Edits>,
    vfio: &[crate::run::VfioGroup],
    nesting: Option<&Grant>,
) -> Result<(), String> {
    let security = security(run)?;
    if let Some(grant) = nesting {
        setup.retain(|e| !e.starts_with(b"caps="));
        setup.push(format!("caps={}", nesting_capabilities(run)).into_bytes());
        setup.extend(grant_entries(grant));
    }
    if run.system_paths && !run.privileged {
        setup.push(b"unmasked".to_vec());
    }
    if security.writable_cgroups == Some(true) && !run.privileged {
        setup.push(b"cgroups-rw".to_vec());
    }
    if let Some(devices) = devices(run, cdi, vfio) {
        setup.push(devices);
    }
    if security.no_new_privileges {
        setup.push(b"nnp".to_vec());
    }
    // Docker's AppArmor's mount rule first, so that its profile's errno is the one a
    // syscall both refuse returns (the most recent filter's).
    if nesting.is_none()
        && let Some(filter) = mounts(run, &security, kernel)?
    {
        setup.push(filter);
    }
    if let Some(filter) = seccomp(run, &security, kernel, nesting.is_some())? {
        setup.push(filter);
    }
    // On the host's network (D124): init told so, and its binds' filter, last.
    if run.network == "host" {
        setup.push(b"network=host".to_vec());
        if let Some(filter) = binds(run, kernel)? {
            setup.push(filter);
        }
    }
    Ok(())
}

/// The nesting grant's own setup entries (D234): the grant, its deadline where the run
/// gives one, and its GPUs where it names them, in D232's grammar.
fn grant_entries(grant: &Grant) -> Vec<Vec<u8>> {
    use shards_abi::run::nesting::{GPUS, GRANT, READY_BY};
    let mut out = vec![GRANT.to_vec()];
    if let Some(at) = grant.ready_by {
        out.push([READY_BY, at.to_string().as_bytes()].concat());
    }
    if !grant.gpus.is_empty() {
        let gpus: Vec<String> = grant.gpus.iter().map(ToString::to_string).collect();
        out.push([GPUS, gpus.join(",").as_bytes()].concat());
    }
    out
}

/// The setup entry by which init gives the container its devices and confines it to
/// them (D44): `devices=`, then, each NUL-ended, `p` where it is privileged (`-` where
/// not), and `d` with each device's host and guest paths and permissions, `r` with
/// each `--device-cgroup-rule`, `i` with each CDI device, and `w`, `rb`, `wb`, `ri` and
/// `wi` with each device weight's and throttle's path and number; and first, `v` with each
/// host VFIO group the microVM is given (D154), its path and its functions' addresses on
/// the microVM's bus, which a device that names the path is then in the guest. Without
/// one, a microVM keeps the filter of Docker's defaults that its VM booted with.
fn devices(
    run: &Run,
    cdi: Option<&crate::build::cdi::Edits>,
    vfio: &[crate::run::VfioGroup],
) -> Option<Vec<u8>> {
    let cdi_nodes = cdi.map(|e| e.nodes.as_slice()).unwrap_or_default();
    let io = [
        (&b"w"[..], &run.blkio_weight_device),
        (b"rb", &run.device_read_bps),
        (b"wb", &run.device_write_bps),
        (b"ri", &run.device_read_iops),
        (b"wi", &run.device_write_iops),
    ];
    // A run given a GPU has its render node, as `--device` gives one (D144).
    let gpu = crate::run::wants_gpu(run).unwrap_or(false);
    if !run.privileged
        && !gpu
        && run.devices.is_empty()
        && run.device_cgroup_rules.is_empty()
        && cdi_nodes.is_empty()
        && io.iter().all(|(_, v)| v.is_empty())
    {
        return None;
    }
    let mut e = b"devices=".to_vec();
    e.extend_from_slice(if run.privileged { b"p\0" } else { b"-\0" });
    let render = gpu.then(|| format!("{0}:{0}:rwm", crate::run::RENDER_NODE));
    // Each group, its functions placed as its VM process places the functions it is
    // given (vm::vfio_places).
    let functions = crate::run::vfio_functions(vfio);
    let places = shards_vmm::vm::vfio_places(&functions, &[]);
    for group in vfio {
        let at: Vec<String> = group
            .functions
            .iter()
            .filter_map(|f| {
                let i = functions.iter().position(|a| a.name == f.name)?;
                places
                    .get(i)
                    .copied()
                    .flatten()
                    .map(shards_vmm::vm::guest_address)
            })
            .collect();
        e.extend_from_slice(b"v\0");
        for w in [group.path.as_str(), &at.join(",")] {
            e.extend_from_slice(w.as_bytes());
            e.push(0);
        }
    }
    for d in run.devices.iter().chain(render.iter()) {
        let mut parts = d.splitn(3, ':');
        e.extend_from_slice(b"d\0");
        for _ in 0..3 {
            e.extend_from_slice(parts.next().unwrap_or_default().as_bytes());
            e.push(0);
        }
    }
    for v in &run.device_cgroup_rules {
        e.extend_from_slice(b"r\0");
        e.extend_from_slice(v.as_bytes());
        e.push(0);
    }
    // CDI's nodes (D146), resolved as the run starts: each its host path and path, its
    // cgroup access (CDI's `rwm` where its spec gives none), and the kind, mode and owner
    // its spec gives, `-` where it gives none.
    for n in cdi_nodes {
        let given = |s: &str| {
            if s.is_empty() {
                "-".to_string()
            } else {
                s.to_string()
            }
        };
        let number = |v: Option<String>| v.unwrap_or_else(|| "-".into());
        let words = [
            given(if n.host_path.is_empty() {
                &n.path
            } else {
                &n.host_path
            }),
            given(&n.path),
            if n.permissions.is_empty() {
                "rwm".into()
            } else {
                n.permissions.clone()
            },
            given(&n.kind),
            number(n.file_mode.map(|m| format!("{:o}", m & 0o7777))),
            number(n.uid.map(|u| u.to_string())),
            number(n.gid.map(|g| g.to_string())),
        ];
        e.extend_from_slice(b"n\0");
        for w in words {
            e.extend_from_slice(w.as_bytes());
            e.push(0);
        }
    }
    for (tag, values) in io {
        for v in values {
            let (path, number) = v.split_once(':').unwrap_or((v, ""));
            for word in [tag, path.as_bytes(), number.as_bytes()] {
                e.extend_from_slice(word);
                e.push(0);
            }
        }
    }
    Some(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `entries`, each of a key the run protocol registers (D257), as shards-init refuses a
    /// spec holding any other; else the first that is not, by its key.
    fn registered(what: &str, entries: &[Vec<u8>]) {
        if let Err(e) = shards_abi::run::setup::check(entries) {
            panic!("{what}: {e}");
        }
    }

    /// D257's guard (c), the composers that are functions: a run of every option `setup` and
    /// `security_setup` read (its IPC, UTS and cgroup namespaces the microVM's, /dev/shm, a
    /// tmpfs, its layer's size, umask, ulimits, sysctls, swappiness, capabilities, groups, OOM
    /// score, read-only root, init, real-time budget, unmasked paths, writable cgroups, devices
    /// and their rules and throttles, no new privileges, AppArmor's mount filter, the host's
    /// network and its binds' filter), with the nesting grant and without; a privileged run
    /// on the microVM's PID namespace, and one on another run's with no IPC namespace; the
    /// domains' filters and the plug roots. Expect: every entry of a registered key.
    #[test]
    fn every_entry_the_setup_composers_write_is_of_a_registered_key() {
        let kernel = shards_seccomp::Kernel(6, 1);
        let mut every = Run {
            ipc: "host".into(),
            shm_size: 64 << 20,
            tmpfs: vec!["/run/a:size=1m".into()],
            uts: "host".into(),
            cgroupns: "host".into(),
            storage_opt: vec!["size=1G".into()],
            umask: Some(0o22),
            ulimits: vec!["nofile=1024:2048".into()],
            sysctls: vec!["net.core.somaxconn=1024".into()],
            cap_add: vec!["CAP_SYS_ADMIN".into()],
            group_add: vec!["10".into()],
            oom_score_adj: 5,
            read_only: true,
            init_process: Some(true),
            system_paths: true,
            security_opt: vec!["writable-cgroups".into(), "no-new-privileges".into()],
            devices: vec!["/dev/fuse:/dev/fuse:rwm".into()],
            device_cgroup_rules: vec!["c 10:229 rwm".into()],
            blkio_weight_device: vec!["/dev/vda:100".into()],
            device_read_bps: vec!["/dev/vda:1mb".into()],
            network: "host".into(),
            ..Run::default()
        };
        every.resources.memory_swappiness = Some(10);
        every.resources.cpu_realtime_period = 1_000_000;
        every.resources.cpu_realtime_runtime = 950_000;
        let grant = Grant {
            ready_by: Some(1_794_466_320_362_000_000),
            gpus: vec![shards_abi::gpu::PhysicalGpu::MetalRegistry(0x1_0000_02a1)],
        };
        let privileged = Run {
            privileged: true,
            pid: "host".into(),
            ..Run::default()
        };
        let joining = Run {
            ipc: "none".into(),
            pid: "container:other".into(),
            ..Run::default()
        };
        for (what, run, nesting) in [
            ("every option", &every, None),
            ("every option, granted", &every, Some(&grant)),
            ("privileged", &privileged, None),
            ("another's PID namespace", &joining, None),
        ] {
            let mut entries = setup(run, &[], None, Some(64 << 20)).unwrap();
            security_setup(&mut entries, run, kernel, None, &[], nesting).unwrap();
            entries.extend(binds(run, kernel).unwrap());
            registered(what, &entries);
        }
        let mut entries = setup(&every, &[], None, None).unwrap();
        security_setup(&mut entries, &every, kernel, None, &[], None).unwrap();
        for kind in [
            &b"ipc"[..],
            b"shm",
            b"tmpfs",
            b"uts",
            b"cgroupns",
            b"rw-size",
            b"umask",
            b"ulimit",
            b"sysctl",
            b"caps",
            b"group",
            b"oom",
            b"readonly",
            b"init",
            b"rt",
            b"unmasked",
            b"cgroups-rw",
            b"devices",
            b"nnp",
            b"seccomp-mounts",
            b"seccomp",
            b"network",
            b"seccomp-binds",
        ] {
            assert!(
                entries.iter().any(|e| shards_abi::run::setup::key(e) == kind),
                "the run of every option writes no {:?} entry",
                kind.escape_ascii()
            );
        }
        registered("the domains' filters", &domain_seccomp(kernel).unwrap());
        let point = crate::volumes::MountPoint {
            kind: "bind".into(),
            source: "/data/proj".into(),
            destination: "/in".into(),
            ..crate::volumes::MountPoint::default()
        };
        registered("the plug roots", &plug_roots(&[point]));
    }

    /// The files of this crate that write a spec's setup inline, each with the words it
    /// spells as literals that are no entry's key: a device's throttle tags within its
    /// `devices=` entry, a volume's flags, a tmpfs's options, a terminal type, a test's
    /// marker. `volumes.rs` chooses one key as it runs (`domain-volume` or `volume`), which
    /// its own tests compose.
    const COMPOSERS: [(&str, &[&str]); 14] = [
        ("setup.rs", &["rb", "ri", "w", "wb", "wi"]),
        ("daemon.rs", &["ro", "rw"]),
        ("api_run.rs", &[]),
        ("warm.rs", &[]),
        ("vm_run.rs", &[]),
        ("volumes.rs", &["mode", "size"]),
        ("spec.rs", &["xterm"]),
        ("daemon/networks.rs", &[]),
        ("daemon/managed.rs", &["record"]),
        ("daemon/managed/execs.rs", &[]),
        ("daemon/commands.rs", &[]),
        ("daemon/visit.rs", &[]),
        ("workload/capture.rs", &["x"]),
        ("agentfile.rs", &[]),
    ];

    /// Whether attribute `line` builds what it gates in tests alone: `#[cfg(test)]`, or an
    /// `all(…)` naming `test`; never an `any(…)`, which a build of the binary meets too.
    fn test_only(line: &str) -> bool {
        let line = line.trim_start();
        line == "#[cfg(test)]"
            || (line.starts_with("#[cfg(all(")
                && line
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .any(|w| w == "test"))
    }

    /// A source file's text but its test modules (each attribute [`test_only`] before a `mod
    /// NAME {`, to its closing brace at its indent; a test module of a file of its own, `mod
    /// NAME;`, is that file's) and its comments.
    fn production(text: &str) -> String {
        let lines: Vec<&str> = text.lines().collect();
        let mut kept = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            let indent = line
                .get(..line.len() - line.trim_start().len())
                .unwrap_or_default();
            let opens = test_only(line)
                && lines
                    .iter()
                    .skip(i + 1)
                    .take(3)
                    .any(|l| l.starts_with(&format!("{indent}mod ")) && l.ends_with('{'));
            if opens {
                let close = format!("{indent}}}");
                i = (i + 1..lines.len())
                    .find(|&j| lines[j] == close)
                    .map_or(lines.len(), |j| j + 1);
                continue;
            }
            if !line.trim_start().starts_with("//") {
                kept.push(line);
            }
            i += 1;
        }
        kept.join("\n")
    }

    /// Each key `text` spells at an entry's start: a byte string's `KEY=…` or `KEY` alone, a
    /// format string's `KEY=…`; and how many format strings choose theirs as it runs
    /// (`format!("{…}=…")`).
    fn spelled(text: &str) -> (Vec<String>, usize) {
        let mut keys = Vec::new();
        for (start, bare) in [("b\"", true), ("format!(\"", false)] {
            for (at, _) in text.match_indices(start) {
                let rest = text.get(at + start.len()..).unwrap_or_default();
                let key: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
                    .collect();
                let next = rest.get(key.len()..).and_then(|r| r.chars().next());
                if key.starts_with(|c: char| c.is_ascii_lowercase())
                    && (next == Some('=') || (bare && next == Some('"')))
                {
                    keys.push(key);
                }
            }
        }
        let chosen = text
            .match_indices("format!(\"{")
            .filter(|(at, _)| {
                let rest = text.get(at + "format!(\"{".len()..).unwrap_or_default();
                rest.find('}')
                    .is_some_and(|end| rest.get(end + 1..).is_some_and(|r| r.starts_with('=')))
            })
            .count();
        (keys, chosen)
    }

    /// D257's guard (c), the composers that write a spec's setup inline (the daemon's run and
    /// its joiners' and execs' entries, the run host's, the warm VM's, the VM process's, the
    /// volumes', the networks', managed output's): a census of this crate's sources. Do: read
    /// every file of it that writes a spec's setup, outside its tests. Expect: each such file
    /// among [`COMPOSERS`], so that a new one is read; each key it spells registered, but its
    /// words that are no entry's key; and none choosing its key as it runs but `volumes.rs`.
    /// (The builder writes no setup: its steps go to the guest as its own protocol's.)
    #[test]
    fn every_entry_the_composers_spell_is_of_a_registered_key() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        let mut dirs = vec![src.clone()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    files.push(path);
                }
            }
        }
        let writes = [
            "setup.push(",
            "setup.extend(",
            "setup.insert(",
            ".setup = vec!",
            "setup: vec![",
        ];
        for path in &files {
            let name = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = production(&std::fs::read_to_string(path).unwrap());
            if writes.iter().any(|w| text.contains(w)) {
                assert!(
                    COMPOSERS.iter().any(|(f, _)| *f == name),
                    "{name} writes a spec's setup, and D257's census does not read it: add it to COMPOSERS"
                );
            }
        }
        for (name, words) in COMPOSERS {
            let text = production(&std::fs::read_to_string(src.join(name)).unwrap());
            let (keys, chosen) = spelled(&text);
            for key in keys {
                assert!(
                    shards_abi::run::setup::registered(key.as_bytes()) || words.contains(&key.as_str()),
                    "{name} spells a setup entry of key {key:?}, which the run protocol does not register (shards_abi::run::setup::KEYS)"
                );
            }
            let chooses = usize::from(name == "volumes.rs");
            assert_eq!(
                chosen, chooses,
                "{name}: format strings choosing their key as they run"
            );
        }
    }

    /// A run's domains are given this host's own roots, and on Unix the top-level directory
    /// of each host path its binds give it, once each; a volume's store is no project, and a
    /// root init refuses (`/tmp`) is never named, so that no run fails for one.
    #[test]
    fn plug_roots_are_the_hosts_own_and_its_binds_tops() {
        let point = |kind: &str, source: &str| crate::volumes::MountPoint {
            kind: kind.into(),
            source: source.into(),
            destination: "/in".into(),
            ..crate::volumes::MountPoint::default()
        };
        let said = |points: &[crate::volumes::MountPoint]| -> Vec<String> {
            plug_roots(points)
                .iter()
                .map(|e| String::from_utf8_lossy(e).into_owned())
                .collect()
        };
        let standard: Vec<String> = STANDARD_ROOTS.iter().map(|r| format!("plug-root={r}")).collect();
        assert_eq!(said(&[]), standard);
        let points = [
            point("bind", "/data/proj"),
            point("bind", "/data/other"),
            point("bind", "/tmp/x"),
            point("bind", "/etc/hosts"),
            point("volume", "/var/lib/shards/volumes/v/_data"),
            point("bind", r"C:\work\proj"),
        ];
        let mut want = standard.clone();
        if cfg!(unix) {
            want.push("plug-root=/data".into());
        }
        assert_eq!(said(&points), want);
    }

    fn run(tmpfs: &[&str]) -> Run {
        Run {
            tmpfs: tmpfs.iter().map(|s| (*s).to_string()).collect(),
            ..Run::default()
        }
    }

    /// moby's TweakCapabilities, and dockerd's refusals.
    #[test]
    fn capabilities_are_added_and_dropped() {
        let caps = |add: &[&str], drop: &[&str], privileged: bool| {
            capabilities(&Run {
                cap_add: add.iter().map(|s| (*s).to_string()).collect(),
                cap_drop: drop.iter().map(|s| (*s).to_string()).collect(),
                privileged,
                ..Run::default()
            })
        };
        assert_eq!(caps(&[], &[], false), 0xa804_25fb);
        assert_eq!(
            caps(&["CAP_NET_ADMIN"], &["CAP_CHOWN"], false),
            0xa804_25fb & !1 | 1 << 12
        );
        assert_eq!(caps(&["ALL"], &["CAP_CHOWN"], false), 0x1ff_ffff_fffe);
        assert_eq!(caps(&["CAP_KILL"], &["ALL"], false), 1 << 5);
        assert_eq!(caps(&[], &[], true), 0x1ff_ffff_ffff);
        let refused = verify(&Run {
            cap_add: vec!["CAP_FOO".into()],
            ..Run::default()
        });
        assert_eq!(
            refused,
            Err("invalid CapAdd: unknown capability: \"CAP_FOO\"".into())
        );
        let oom = verify(&Run {
            oom_score_adj: 1001,
            ..Run::default()
        });
        assert_eq!(
            oom,
            Err("Invalid value 1001, range for oom score adj is [-1000, 1000]".into())
        );
    }

    /// dockerd 29.3.1's refusals, and moby's merged options.
    #[test]
    fn namespaces_are_set_up() {
        assert_eq!(
            verify(&run(&["/"])),
            Err("invalid specification: destination can't be '/'".into())
        );
        assert_eq!(
            verify(&run(&["/a/.."])),
            Err("invalid specification: destination can't be '/'".into())
        );
        assert_eq!(
            verify(&run(&["run"])),
            Err("invalid mount path: 'run' mount path must be absolute".into())
        );
        assert_eq!(
            setup(&run(&["/x:foo"]), &[], None, None),
            Err("invalid tmpfs option \"foo\"".into())
        );
        let set = Run {
            tmpfs: vec![
                "/run/a:size=1m".into(),
                "/run:size=1m,exec,size=2m".into(),
                "/tmp".into(),
            ],
            shm_size: 128 << 20,
            ulimits: vec!["nproc=10".into(), "nofile=1024:2048".into(), "nproc=20".into()],
            sysctls: vec!["net.core.somaxconn=1024".into(), "kernel.shmmax=1".into()],
            read_only: true,
            ..Run::default()
        };
        let text: Vec<String> = setup(&set, &[], None, None)
            .unwrap()
            .into_iter()
            .map(|b| String::from_utf8(b).unwrap())
            .collect();
        assert_eq!(
            text,
            [
                // Resized before anything is mounted, which a mount at /dev/shm then covers.
                "shm=134217728",
                "tmpfs=/run\0nosuid,nodev,rprivate,exec,size=2m",
                "tmpfs=/tmp\0noexec,nosuid,nodev,rprivate",
                "tmpfs=/run/a\0noexec,nosuid,nodev,rprivate,size=1m",
                "ulimit=nofile=1024:2048",
                "ulimit=nproc=20:20",
                "sysctl=kernel.shmmax=1",
                "sysctl=net.core.somaxconn=1024",
                "readonly",
            ]
        );
    }

    /// Docker's AppArmor's mount rule is a filter of its own for a run given
    /// CAP_SYS_ADMIN, before Docker's profile; none without the capability, `seccomp=`
    /// unconfined or a profile of its own alike, for a privileged run, or with
    /// `apparmor=unconfined`, as Docker's has none then (PM M174).
    #[test]
    fn a_container_given_sys_admin_has_apparmors_mount_rule() {
        let kernel = shards_seccomp::Kernel(6, 1);
        let entries = |run: &Run| {
            let mut setup = Vec::new();
            security_setup(&mut setup, run, kernel, None, &[], None).unwrap();
            setup
                .into_iter()
                .filter_map(|e| {
                    let name = e.split(|&b| b == b'=').next().unwrap_or_default().to_vec();
                    name.starts_with(b"seccomp")
                        .then(|| String::from_utf8(name).unwrap())
                })
                .collect::<Vec<_>>()
        };
        // As the CLI hands capabilities on, normalized (docker/cli opts.NormalizeCapability).
        let admin = Run {
            cap_add: vec!["CAP_SYS_ADMIN".into()],
            ..Run::default()
        };
        assert_eq!(entries(&admin), ["seccomp-mounts", "seccomp"]);
        assert_eq!(entries(&Run::default()), ["seccomp"]);
        let all = Run {
            cap_add: vec!["ALL".into()],
            ..Run::default()
        };
        assert_eq!(entries(&all), ["seccomp-mounts", "seccomp"]);
        let unconfined = Run {
            security_opt: vec!["apparmor=unconfined".into()],
            ..admin.clone()
        };
        assert_eq!(entries(&unconfined), ["seccomp"]);
        let privileged = Run {
            privileged: true,
            ..Run::default()
        };
        assert!(entries(&privileged).is_empty());
        // `seccomp=unconfined` leaves AppArmor's rule: it asks for no seccomp filter, not
        // for AppArmor's mounts (D121).
        let no_seccomp = Run {
            security_opt: vec!["seccomp=unconfined".into()],
            ..admin.clone()
        };
        assert_eq!(entries(&no_seccomp), ["seccomp-mounts"]);
        // Without the capability, under no filter or one of its own: none.
        for (opt, want) in [
            ("seccomp=unconfined", &[][..]),
            ("seccomp={\"defaultAction\":\"SCMP_ACT_ALLOW\"}", &["seccomp"]),
        ] {
            let run = Run {
                security_opt: vec![opt.into()],
                ..Run::default()
            };
            assert_eq!(entries(&run), want, "{opt}");
        }
    }

    /// `--security-opt nesting=true` asks for the nesting grant (D234): shards' own key, a
    /// bool as Go reads one, after `:` too, the last given standing; alone, or with no
    /// bool, refused as dockerd refuses a malformed option.
    #[test]
    fn a_run_asks_for_the_nesting_grant_by_its_own_key() {
        let asks = |opts: &[&str]| {
            security(&Run {
                security_opt: opts.iter().map(|s| (*s).to_string()).collect(),
                ..Run::default()
            })
            .map(|s| s.nesting)
        };
        for (opts, want) in [
            (&["nesting=true"][..], Ok(true)),
            (&["nesting=1"][..], Ok(true)),
            (&["nesting:true"][..], Ok(true)),
            (&["nesting=false"][..], Ok(false)),
            (&["nesting=true", "nesting=false"][..], Ok(false)),
            (&[][..], Ok(false)),
            (
                &["nesting"][..],
                Err("invalid --security-opt 1: \"nesting\"".to_string()),
            ),
            (
                &["nesting=yes"][..],
                Err("invalid --security-opt 2: \"nesting=yes\"".to_string()),
            ),
        ] {
            assert_eq!(asks(opts), want, "{opts:?}");
        }
    }

    /// The runs `a_run_without_the_nesting_grant_is_composed_as_before` holds: each what
    /// shards composed of it at bbd908197, before the nesting grant (a filter shown by its
    /// name alone, whose program is its architecture's), and the SHA-256 of its entries
    /// there on x86_64, each after its length, a big-endian u32 (recorded on Linux x86_64
    /// by the same composition at that revision).
    fn before_the_grant() -> Vec<(&'static str, Run, Vec<&'static str>, &'static str)> {
        let opts = |o: &[&str]| o.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let default = "5b2b34f20829bb1fb022ad62070dced0a6bdd9c84c10b11a28042cd7b03c227d";
        vec![
            ("default", Run::default(), vec!["seccomp"], default),
            (
                "nesting-false",
                Run {
                    security_opt: opts(&["nesting=false"]),
                    ..Run::default()
                },
                vec!["seccomp"],
                default,
            ),
            (
                "sys-admin",
                Run {
                    cap_add: opts(&["CAP_SYS_ADMIN"]),
                    ..Run::default()
                },
                vec!["caps=2820941307", "seccomp-mounts", "seccomp"],
                "c48f56d50a73cb36af895f0c42546ac10fae507349d3316806935c3bfe383dbf",
            ),
            (
                "all-but-chown",
                Run {
                    cap_add: opts(&["ALL"]),
                    cap_drop: opts(&["CAP_CHOWN"]),
                    ..Run::default()
                },
                vec!["caps=2199023255550", "seccomp-mounts", "seccomp"],
                "26a009d82c18913c6111f98ac78365e386b6d9007d3ba0fcea606f3dcd5a5133",
            ),
            (
                "net-admin-alone",
                Run {
                    cap_add: opts(&["CAP_NET_ADMIN"]),
                    cap_drop: opts(&["ALL"]),
                    ..Run::default()
                },
                vec!["caps=4096", "seccomp"],
                "78b8bedfb4b374af629c4d94a36e5359a238792055e8ae5929015e8a4b75e089",
            ),
            (
                "privileged",
                Run {
                    privileged: true,
                    ..Run::default()
                },
                vec!["caps=2199023255551", "privileged", "devices=p\0"],
                "fcf57dca954aa4e69be29a5abec2fa2cd234cde51e7a8ecd9e243636958604da",
            ),
            (
                "apparmor-unconfined",
                Run {
                    cap_add: opts(&["CAP_SYS_ADMIN"]),
                    security_opt: opts(&["apparmor=unconfined"]),
                    ..Run::default()
                },
                vec!["caps=2820941307", "seccomp"],
                "7f3bc88392aeee1b9c7803ebb401c478ed0eb750c02fa977467707faa21f2fc7",
            ),
            (
                "seccomp-unconfined",
                Run {
                    security_opt: opts(&["seccomp=unconfined"]),
                    ..Run::default()
                },
                vec![],
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                "own-profile",
                Run {
                    security_opt: opts(&[r#"seccomp={"defaultAction":"SCMP_ACT_ALLOW"}"#]),
                    ..Run::default()
                },
                vec!["seccomp"],
                "a3ad1399e10b9a1e669bc84ae575a7f1ed91801e70df220c8b8e78cea20451e2",
            ),
            (
                "cgroups-nnp",
                Run {
                    security_opt: opts(&["writable-cgroups=true", "no-new-privileges"]),
                    ..Run::default()
                },
                vec!["cgroups-rw", "nnp", "seccomp"],
                "7533876735aeb2218d3635951243e91b17994b120ef4c4f84cd5cd15382dce7d",
            ),
            (
                "host-network",
                Run {
                    network: "host".into(),
                    ..Run::default()
                },
                vec!["seccomp", "network=host", "seccomp-binds"],
                "e7cd4df00bed84d6e66f332c9d2868fa018a0ddd5a99b85c32c3940964988708",
            ),
            (
                "cgroupns-pid-host",
                Run {
                    cgroupns: "host".into(),
                    pid: "host".into(),
                    ..Run::default()
                },
                vec!["cgroupns=host", "pid=host", "seccomp"],
                "d8618bbda440c020da75ba3a0f906af23461bb5bcd1f2cc08da4cf72093a28ec",
            ),
        ]
    }

    /// An entry as `before_the_grant` shows it: a filter by its name alone.
    fn shown(e: &[u8]) -> String {
        for name in ["seccomp-mounts=", "seccomp-binds=", "seccomp="] {
            if e.starts_with(name.as_bytes()) {
                return name.trim_end_matches('=').to_string();
            }
        }
        String::from_utf8_lossy(e).into_owned()
    }

    /// `profile` compiled for `run`'s own capabilities on `kernel`, as a filter's entry
    /// `name` was made before the nesting grant: its name, its flags, its instructions.
    fn filter_before(
        name: &[u8],
        profile: &[u8],
        run: &Run,
        agent: bool,
        kernel: shards_seccomp::Kernel,
    ) -> Vec<u8> {
        let mask = capabilities(run);
        let caps: Vec<String> = shards_abi::run::CAP_NAMES
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1u64 << i) != 0)
            .map(|(_, n)| (*n).to_string())
            .collect();
        let c = shards_seccomp::Container {
            arch: shards_seccomp::Arch::host().unwrap(),
            caps: &caps,
            kernel,
        };
        let program = if agent {
            shards_seccomp::compile_agent(profile, &c)
        } else {
            shards_seccomp::compile(profile, &c)
        };
        let program = program.unwrap().unwrap();
        let mut e = name.to_vec();
        e.extend_from_slice(&program.flags.to_le_bytes());
        for i in &program.insns {
            e.extend_from_slice(&i.to_ne_bytes());
        }
        e
    }

    /// A run without the nesting grant is composed byte for byte as before it (D234): each
    /// entry as bbd908197 made it; each filter what its profile, Docker's or the run's own,
    /// compiles to for the run's own capabilities, on every architecture; and on x86_64 the
    /// whole as recorded at bbd908197.
    #[test]
    fn a_run_without_the_nesting_grant_is_composed_as_before() {
        use sha2::Digest as _;
        let kernel = shards_seccomp::Kernel(6, 1);
        for (name, run, then, x86_64) in before_the_grant() {
            let mut entries = setup(&run, &[], None, None).unwrap();
            security_setup(&mut entries, &run, kernel, None, &[], None).unwrap();
            let now: Vec<String> = entries.iter().map(|e| shown(e)).collect();
            assert_eq!(now, then, "{name}");
            let own = security(&run).unwrap().seccomp.filter(|p| p.starts_with('{'));
            for e in &entries {
                let made =
                    |entry: &[u8], profile: &[u8], agent| filter_before(entry, profile, &run, agent, kernel);
                let want = if e.starts_with(b"seccomp-mounts=") {
                    made(b"seccomp-mounts=", shards_seccomp::MOUNTS, false)
                } else if e.starts_with(b"seccomp-binds=") {
                    made(b"seccomp-binds=", shards_seccomp::BINDS, true)
                } else if e.starts_with(b"seccomp=") {
                    let profile = own.as_deref().map_or(shards_seccomp::DEFAULT, str::as_bytes);
                    made(b"seccomp=", profile, false)
                } else {
                    continue;
                };
                assert!(*e == want, "{name}: {} differs", shown(e));
            }
            let mut digest = sha2::Sha256::new();
            for e in &entries {
                digest.update(u32::try_from(e.len()).unwrap().to_be_bytes());
                digest.update(e);
            }
            let digest: String = digest.finalize().iter().map(|b| format!("{b:02x}")).collect();
            if cfg!(target_arch = "x86_64") {
                assert_eq!(digest, x86_64, "{name}");
            }
        }
    }

    /// This architecture's audit number (include/uapi/linux/audit.h) and the calls a filter
    /// is read for, by their numbers (arch/x86/entry/syscalls/syscall_64.tbl;
    /// include/uapi/asm-generic/unistd.h).
    #[cfg(target_arch = "x86_64")]
    const CALLS: (u32, [(&str, u32); 10]) = (
        0xc000_003e,
        [
            ("socket", 41),
            ("pivot_root", 155),
            ("mount", 165),
            ("unshare", 272),
            ("bpf", 321),
            ("io_uring_setup", 425),
            ("userfaultfd", 323),
            ("keyctl", 250),
            ("kexec_load", 246),
            ("init_module", 175),
        ],
    );
    #[cfg(target_arch = "aarch64")]
    const CALLS: (u32, [(&str, u32); 10]) = (
        0xc000_00b7,
        [
            ("socket", 198),
            ("pivot_root", 41),
            ("mount", 40),
            ("unshare", 97),
            ("bpf", 280),
            ("io_uring_setup", 425),
            ("userfaultfd", 282),
            ("keyctl", 219),
            ("kexec_load", 104),
            ("init_module", 105),
        ],
    );

    /// What the filter entry `entry` (`seccomp=` and the program init loads) returns for
    /// call `name` with first argument `arg0`.
    fn decides(entry: &[u8], name: &str, arg0: u64) -> u32 {
        use shards_seccomp::bpf::{self, Data, Insn};
        let (_flags, insns) = entry.strip_prefix(b"seccomp=").unwrap().split_at(4);
        let (insns, []) = insns.as_chunks::<8>() else {
            panic!("a program of whole instructions")
        };
        let program: Vec<Insn> = insns
            .iter()
            .map(|&[c0, c1, jt, jf, k0, k1, k2, k3]| Insn {
                code: u16::from_ne_bytes([c0, c1]),
                jt,
                jf,
                k: u32::from_ne_bytes([k0, k1, k2, k3]),
            })
            .collect();
        let (arch, calls) = CALLS;
        let nr = calls.iter().find(|(n, _)| *n == name).unwrap().1;
        let data = Data {
            nr,
            arch,
            args: [arg0, 0, 0, 0, 0, 0],
            ..Data::default()
        };
        bpf::run(&program, &data).unwrap()
    }

    /// A run given the nesting grant is composed with it (D234): its capabilities Docker's
    /// default set's with CAP_SYS_ADMIN and CAP_NET_ADMIN and its own, in one entry in place
    /// of the one its flags gave; the grant's entries, which init reads back as the grant;
    /// no mount filter, its own CAP_SYS_ADMIN notwithstanding; and Docker's default filter
    /// for those capabilities with `pivot_root` allowed, which still refuses AF_ALG and
    /// AF_VSOCK sockets, io_uring, userfaultfd, keyctl, kexec and modules. A run that asks
    /// for no filter, or is privileged, has none.
    #[test]
    fn a_granted_run_is_composed_with_the_grant() {
        use shards_abi::gpu::PhysicalGpu;
        use shards_abi::run::nesting::{self, Grant};
        use shards_seccomp::{ACT_ALLOW, act_errno};
        let kernel = shards_seccomp::Kernel(6, 1);
        let compose = |run: &Run, grant: &Grant| {
            let mut entries = setup(run, &[], None, None).unwrap();
            security_setup(&mut entries, run, kernel, None, &[], Some(grant)).unwrap();
            entries
        };
        assert_eq!(shards_abi::run::CAP_NAMES[21], "CAP_SYS_ADMIN");
        assert_eq!(shards_abi::run::CAP_NAMES[12], "CAP_NET_ADMIN");
        let grant = Grant {
            ready_by: Some(1_794_466_320_362_000_000),
            gpus: vec![
                PhysicalGpu::Pci {
                    domain: 0,
                    bus: 1,
                    device: 0,
                    function: 0,
                },
                PhysicalGpu::MetalRegistry(0x1_0000_02a1),
            ],
        };
        let run = Run {
            cap_add: vec!["CAP_SYS_ADMIN".into(), "CAP_SYS_PTRACE".into()],
            security_opt: vec!["nesting=true".into()],
            ..Run::default()
        };
        let entries = compose(&run, &grant);
        let caps: Vec<&[u8]> = entries.iter().filter_map(|e| e.strip_prefix(b"caps=")).collect();
        let defaults = capabilities(&Run::default());
        let want = defaults | 1 << 21 | 1 << 12 | 1 << 19;
        assert_eq!(caps, [want.to_string().as_bytes()]);
        assert_eq!(nesting::read(&entries), Ok(Some(grant)));
        assert_eq!(entries.iter().filter(|e| e.as_slice() == b"nesting").count(), 1);
        assert!(!entries.iter().any(|e| e.starts_with(b"seccomp-mounts=")));
        let filters: Vec<&Vec<u8>> = entries.iter().filter(|e| e.starts_with(b"seccomp=")).collect();
        let [filter] = filters.as_slice() else {
            panic!("one filter: {}", filters.len())
        };
        let eperm = act_errno(1);
        for (call, arg0, want) in [
            ("pivot_root", 0, ACT_ALLOW),
            ("mount", 0, ACT_ALLOW),
            ("unshare", 0, ACT_ALLOW),
            ("bpf", 0, ACT_ALLOW),
            ("socket", 2, ACT_ALLOW),
            ("socket", 38, eperm),
            ("socket", 40, eperm),
            ("io_uring_setup", 0, eperm),
            ("userfaultfd", 0, eperm),
            ("keyctl", 0, eperm),
            ("kexec_load", 0, eperm),
            ("init_module", 0, eperm),
        ] {
            assert_eq!(decides(filter, call, arg0), want, "{call}({arg0})");
        }
        // Docker's own filter, a run's without the grant, refuses what the grant's allows.
        let plain = compose(&Run::default(), &Grant::default());
        let mut ungranted = Vec::new();
        security_setup(&mut ungranted, &Run::default(), kernel, None, &[], None).unwrap();
        let [default] = ungranted.as_slice() else {
            panic!("one filter")
        };
        for call in ["pivot_root", "mount", "unshare"] {
            assert_eq!(decides(default, call, 0), eperm, "{call}");
        }
        // A grant without a deadline or GPUs is the word alone; the run's default
        // capabilities with the grant's.
        assert_eq!(nesting::read(&plain), Ok(Some(Grant::default())));
        let caps = (defaults | 1 << 21 | 1 << 12).to_string();
        let words: Vec<&[u8]> = plain
            .iter()
            .filter(|e| !e.starts_with(b"seccomp="))
            .map(Vec::as_slice)
            .collect();
        assert_eq!(words, [format!("caps={caps}").as_bytes(), &b"nesting"[..]]);
        // A run that asks for no filter has none, granted or not; a privileged one every
        // capability and no filter.
        for (opts, privileged) in [(&["seccomp=unconfined"][..], false), (&[][..], true)] {
            let run = Run {
                security_opt: opts.iter().map(|s| (*s).to_string()).collect(),
                privileged,
                ..Run::default()
            };
            let entries = compose(&run, &Grant::default());
            assert!(
                !entries.iter().any(|e| e.starts_with(b"seccomp")),
                "{opts:?} {privileged}"
            );
        }
    }
}
