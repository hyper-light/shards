//! `run`'s flags that set up the workload's namespaces: `--read-only`, `--tmpfs`,
//! `--shm-size`, `--ulimit` and `--sysctl`, as dockerd checks them as it makes a container
//! (moby docker-v29.8.1 daemon/daemon_unix.go, verifyPlatformContainerSettings) and turns
//! them into the OCI spec as it starts one (daemon/oci_linux.go: setMounts, withRlimits,
//! WithSysctls), for shards-init to apply as runc v1.5.1 does.
// Only Unix has the daemon, so far.
#![cfg_attr(not(unix), allow(dead_code))]

use shards_ipc::Run;

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

/// A container's `--security-opt`, as dockerd reads them as it makes one (moby
/// daemon/daemon_unix.go, parseSecurityOpt): each `KEY=VALUE`, else `KEY:VALUE` (which it
/// calls deprecated), but `no-new-privileges`, `writable-cgroups` and `disable` alone.
/// Labels and AppArmor profiles are kept, as on a host without SELinux or AppArmor,
/// which the guest kernel has neither of.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Security {
    pub no_new_privileges: bool,
    pub writable_cgroups: Option<bool>,
    /// `seccomp=`'s value: `unconfined`, `builtin`, a profile's JSON, or none.
    pub seccomp: Option<String>,
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
            "label" | "apparmor" => {}
            "seccomp" => out.seccomp = Some(v.to_string()),
            "no-new-privileges" => {
                out.no_new_privileges = shards_cmdline::go::parse_bool(v).map_err(|_| two())?
            }
            "writable-cgroups" => {
                out.writable_cgroups = Some(shards_cmdline::go::parse_bool(v).map_err(|_| two())?)
            }
            _ => return Err(two()),
        }
    }
    Ok(out)
}

/// The capabilities of a container's bounding set, by name.
pub fn capability_names(run: &Run) -> Vec<String> {
    let mask = capabilities(run);
    shards_abi::run::CAP_NAMES
        .iter()
        .enumerate()
        .filter(|(i, _)| mask & (1u64 << i) != 0)
        .map(|(_, n)| (*n).to_string())
        .collect()
}

/// The seccomp filter a build's `RUN` steps run under, as BuildKit gives each step
/// moby's default profile (its executor's `Seccomp: 2`, oci/spec.go
/// WithDefaultSeccomp): compiled for a step's capabilities, the defaults a container
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

/// The seccomp filter a container's workload runs under, as moby chooses its profile
/// (daemon/seccomp_linux.go, WithSeccomp): none for `unconfined`; a privileged
/// container's only where it names one; else the one it names or Docker's default. As the
/// setup entry init loads it by (`seccomp=`, the seccomp(2) flags and the program); none
/// where the profile asks for none. Each profile, set of capabilities and kernel is
/// compiled once.
///
/// moby reads `builtin` named for a privileged container as a profile's JSON, and fails
/// ("invalid character 'b'"): here it is the default profile, as it is for any other.
pub fn seccomp(
    run: &Run,
    security: &Security,
    kernel: shards_seccomp::Kernel,
) -> Result<Option<Vec<u8>>, String> {
    let profile: &[u8] = match security.seccomp.as_deref() {
        Some("unconfined") => return Ok(None),
        None | Some("") if run.privileged => return Ok(None),
        None | Some("" | "builtin") => shards_seccomp::DEFAULT,
        Some(json) => json.as_bytes(),
    };
    let arch = shards_seccomp::Arch::host().ok_or("seccomp needs an amd64 or arm64 guest")?;
    let version = kernel;
    let caps = capability_names(run);
    type Key = (Vec<u8>, Vec<String>, shards_seccomp::Kernel);
    static COMPILED: std::sync::Mutex<std::collections::BTreeMap<Key, Option<Vec<u8>>>> =
        std::sync::Mutex::new(std::collections::BTreeMap::new());
    let key = (profile.to_vec(), caps, version);
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
        caps: &key.1,
        kernel: version,
    };
    let entry = shards_seccomp::compile(profile, &c)?.map(|p| {
        let mut e = b"seccomp=".to_vec();
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
        .map_err(|e| format!("Docker's seccomp profile: {e}"))?;
    let rules = p
        .get_mut("syscalls")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or("Docker's seccomp profile has no syscalls")?;
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
/// what dockerd says as it cannot start the container: each tmpfs, the shallowest first
/// as moby sorts mounts, with moby's default options before the run's; `/dev/shm`'s size;
/// the ulimits, the last of each by name; the sysctls, the last of each; and the root
/// read-only last, after everything is mounted on it.
pub fn setup(run: &Run, points: &[(String, Vec<u8>)]) -> Result<Vec<Vec<u8>>, String> {
    let mut out: Vec<Vec<u8>> = Vec::new();
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
    if run.shm_size != 0 {
        out.push(format!("shm={}", run.shm_size).into_bytes());
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
    // The process's: capabilities, groups and OOM score, which its execs take too.
    if run.privileged || !run.cap_add.is_empty() || !run.cap_drop.is_empty() {
        out.push(format!("caps={}", capabilities(run)).into_bytes());
    }
    for g in &run.group_add {
        out.push(format!("group={g}").into_bytes());
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
    if run.docker_init == Some(true) && run.pid.is_empty() {
        out.push(b"init".to_vec());
    }
    if run.pid == "host" {
        out.push(b"pid=host".to_vec());
    } else if run.pid.starts_with("container:") {
        // The namespace of the workload whose network a joiner joins (D119), the one other
        // container's that the daemon lets a container join.
        out.push(b"pid=workload".to_vec());
    }
    Ok(out)
}

/// `--security-opt`'s setup entries, after the rest: the paths left unmasked, the cgroup
/// writable, no new privileges and the seccomp filter, which its execs take too (init
/// installs the filter last, as runc does). Made as the container starts, so that a
/// profile it cannot load fails its start and not its making, as dockerd's does.
pub fn security_setup(run: &Run, kernel: shards_seccomp::Kernel) -> Result<Vec<Vec<u8>>, String> {
    let mut out = Vec::new();
    let security = security(run)?;
    if run.system_paths && !run.privileged {
        out.push(b"unmasked".to_vec());
    }
    if security.writable_cgroups == Some(true) && !run.privileged {
        out.push(b"cgroups-rw".to_vec());
    }
    if let Some(devices) = devices(run) {
        out.push(devices);
    }
    if security.no_new_privileges {
        out.push(b"nnp".to_vec());
    }
    if let Some(filter) = seccomp(run, &security, kernel)? {
        out.push(filter);
    }
    Ok(out)
}

/// The setup entry by which init gives the container its devices and confines it to
/// them (D44): `devices=`, then, each NUL-ended, `p` where it is privileged (`-` where
/// not), and `d` with each device's host and container paths and permissions, `r` with
/// each `--device-cgroup-rule`, `i` with each CDI device, and `w`, `rb`, `wb`, `ri` and
/// `wi` with each device weight's and throttle's path and number. Without one, a container keeps
/// the filter of Docker's defaults that its VM booted with.
fn devices(run: &Run) -> Option<Vec<u8>> {
    let io = [
        (&b"w"[..], &run.blkio_weight_device),
        (b"rb", &run.device_read_bps),
        (b"wb", &run.device_write_bps),
        (b"ri", &run.device_read_iops),
        (b"wi", &run.device_write_iops),
    ];
    if !run.privileged
        && run.devices.is_empty()
        && run.device_cgroup_rules.is_empty()
        && run.cdi_devices.is_empty()
        && io.iter().all(|(_, v)| v.is_empty())
    {
        return None;
    }
    let mut e = b"devices=".to_vec();
    e.extend_from_slice(if run.privileged { b"p\0" } else { b"-\0" });
    for d in &run.devices {
        let mut parts = d.splitn(3, ':');
        e.extend_from_slice(b"d\0");
        for _ in 0..3 {
            e.extend_from_slice(parts.next().unwrap_or_default().as_bytes());
            e.push(0);
        }
    }
    for (tag, values) in [(b"r", &run.device_cgroup_rules), (b"i", &run.cdi_devices)] {
        for v in values {
            e.extend_from_slice(tag);
            e.push(0);
            e.extend_from_slice(v.as_bytes());
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

    fn run(tmpfs: &[&str]) -> Run {
        Run {
            tmpfs: tmpfs.iter().map(|s| (*s).to_string()).collect(),
            ..Run::default()
        }
    }

    /// moby's TweakCapabilities, and dockerd's refusals.
    #[test]
    fn capabilities_are_tweaked_as_dockerd_tweaks_them() {
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
    fn namespaces_are_set_up_as_dockerd_sets_them() {
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
            setup(&run(&["/x:foo"]), &[]),
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
        let text: Vec<String> = setup(&set, &[])
            .unwrap()
            .into_iter()
            .map(|b| String::from_utf8(b).unwrap())
            .collect();
        assert_eq!(
            text,
            [
                "tmpfs=/run\0nosuid,nodev,rprivate,exec,size=2m",
                "tmpfs=/tmp\0noexec,nosuid,nodev,rprivate",
                "tmpfs=/run/a\0noexec,nosuid,nodev,rprivate,size=1m",
                "shm=134217728",
                "ulimit=nofile=1024:2048",
                "ulimit=nproc=20:20",
                "sysctl=kernel.shmmax=1",
                "sysctl=net.core.somaxconn=1024",
                "readonly",
            ]
        );
    }
}
