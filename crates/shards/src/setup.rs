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

/// What dockerd refuses of these as it makes a container: a `/dev/shm` below 0, and a
/// tmpfs at `/` or at a relative path (ValidateTmpfsMountDestination).
pub fn verify(run: &Run) -> Result<(), String> {
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
fn merge_tmpfs_options(options: &[&str]) -> Result<Vec<String>, String> {
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
pub fn setup(run: &Run) -> Result<Vec<Vec<u8>>, String> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut tmpfs: Vec<(String, String)> = tmpfs_map(run)
        .into_iter()
        .map(|(dest, opts)| (clean(&dest), opts))
        .collect();
    // sortMounts (daemon/volumes_unix.go): fewer path parts first.
    tmpfs.sort_by_key(|(dest, _)| dest.split('/').filter(|p| !p.is_empty()).count());
    for (dest, opts) in tmpfs {
        let mut options = vec!["noexec", "nosuid", "nodev", "rprivate"];
        if !opts.is_empty() {
            options.extend(opts.split(','));
        }
        let merged = merge_tmpfs_options(&options)?;
        out.push(format!("tmpfs={dest}\0{}", merged.join(",")).into_bytes());
    }
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
    if run.read_only {
        out.push(b"readonly".to_vec());
    }
    Ok(out)
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
            setup(&run(&["/x:foo"])),
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
        let text: Vec<String> = setup(&set)
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
