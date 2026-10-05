//! A command as `docker run` specifies it, which the daemon and each VM read alike: its
//! options and the workload they make, the words and status of one that cannot run, and
//! the records of a container's log.

// Where shards runs no workloads yet (Windows), nothing reads these.
#![cfg_attr(not(unix), allow(dead_code))]

use shards_abi::run::{self, Size, Spec};

/// Docker's PATH for Linux containers (moby daemon/pkg/oci/defaults.go).
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// `docker run`'s status when it could not run the command at all.
pub const NOT_RUN: u8 = 125;

/// How `docker run` reports what its daemon said kept the container from running, and
/// the status it exits with (docker/cli cli/command/container/run.go, withHelp and
/// toStatusError).
pub fn not_run(said: &str) -> (String, u8) {
    (
        format!("shards: Error response from daemon: {said}\n\nRun 'shards run --help' for more information"),
        shards_cmdline::commands::run_status(said),
    )
}

/// What `--`, `--env`, `--workdir`, `--user`, `--hostname`, `--interactive` and `--tty`
/// asked for.
#[derive(Debug, Default, Clone)]
pub struct Options {
    pub argv: Vec<String>,
    pub env: Vec<String>,
    /// An exec's own variables, laid over the container's environment whole (moby
    /// daemon/exec.go): empty for a run.
    pub exec_env: Vec<String>,
    pub workdir: String,
    pub user: String,
    pub hostname: Option<String>,
    pub interactive: bool,
    /// A pty for the command's stdio, of this size.
    pub tty: Option<Size>,
}

/// The workload `docker run` would start. Its environment is Docker's PATH and HOSTNAME,
/// and with a terminal TERM=xterm, then the variables given, which replace those,
/// unset them when given without a value, and are otherwise appended (moby
/// daemon/container/container.go, CreateDaemonEnvironment; daemon/container/env.go,
/// ReplaceOrAppendEnvValues). As in the Docker CLI, a variable given without a value first
/// takes its value from `lookup`: shards' own environment where the command line is read
/// (docker/cli opts/env.go, ValidateEnv), and nothing where the client already gave it
/// its value (run.rs).
pub fn spec(o: &Options, lookup: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<Spec, String> {
    let hostname = match &o.hostname {
        Some(h) => h.clone(),
        None => {
            // Docker names a container's host after the first 12 hex digits of its ID.
            let mut id = [0u8; 6];
            shards_vmm::platform::fill_random(&mut id).map_err(|e| format!("random hostname: {e}"))?;
            id.iter().map(|b| format!("{b:02x}")).collect()
        }
    };
    let mut defaults = vec![format!("PATH={DEFAULT_PATH}"), format!("HOSTNAME={hostname}")];
    if o.tty.is_some() {
        defaults.push("TERM=xterm".into());
    }
    let given = |list: &[String]| -> Result<Vec<Vec<u8>>, String> {
        list.iter()
            .map(|entry| match entry.split_once('=') {
                Some(("", _)) => Err(format!("invalid environment variable: {entry}")),
                Some(_) => Ok(entry.clone().into_bytes()),
                None if entry.is_empty() => Err("invalid environment variable: ".into()),
                None => Ok(match lookup(entry) {
                    Some(value) => [entry.as_bytes(), b"=", &os_bytes(&value)].concat(),
                    None => entry.clone().into_bytes(),
                }),
            })
            .collect()
    };
    let defaults = defaults.into_iter().map(String::into_bytes).collect();
    // The container's (CreateDaemonEnvironment), then an exec's over it whole.
    let mut env = replace_or_append(defaults, given(&o.env)?);
    if !o.exec_env.is_empty() {
        env = replace_or_append(env, given(&o.exec_env)?);
    }
    let spec = Spec {
        argv: o.argv.iter().map(|a| a.clone().into_bytes()).collect(),
        env,
        cwd: o.workdir.clone().into_bytes(),
        user: o.user.clone().into_bytes(),
        hostname: hostname.into_bytes(),
        tty: o.tty,
        // A run on a network gets its resolvers from the daemon as it is handed over.
        resolv: None,
        stdin: o.interactive,
        builtin: 0,
        // Set by the daemon as it is handed over, as the resolvers are.
        hosts: Vec::new(),
        domainname: Vec::new(),
        cgroup: Vec::new(),
        setup: Vec::new(),
    };
    fits(&spec)?;
    Ok(spec)
}

/// Whether `spec` goes to the guest in one frame, as everything a run sends it does: once
/// it is built, and again once the daemon has added its resolvers (review 1.y).
pub fn fits(spec: &run::Spec) -> Result<(), String> {
    if spec.encoded_len().is_none_or(|n| n > run::MAX_PAYLOAD as usize) {
        return Err("the command and its environment are too large".into());
    }
    Ok(())
}

/// Go's ReplaceOrAppendEnvValues (moby daemon/container/env.go): each of `overrides`
/// replaces the variable of its name in `defaults`, unsets it when it has no value, or is
/// appended. Names are looked up among `defaults` alone, the last of a name there being the
/// one replaced: two overrides of one new name are both kept, as Go keeps them.
fn replace_or_append(defaults: Vec<Vec<u8>>, overrides: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let name = |e: &[u8]| -> Vec<u8> { e.split(|&b| b == b'=').next().unwrap_or_default().to_vec() };
    let mut at = std::collections::HashMap::new();
    for (i, e) in defaults.iter().enumerate() {
        at.insert(name(e), i);
    }
    let mut env: Vec<Option<Vec<u8>>> = defaults.into_iter().map(Some).collect();
    for value in overrides {
        let has_value = value.contains(&b'=');
        match at.get(&name(&value)).and_then(|&i| env.get_mut(i)) {
            Some(slot) => *slot = has_value.then_some(value),
            None if has_value => env.push(Some(value)),
            None => {}
        }
    }
    env.into_iter().flatten().collect()
}

/// Nanoseconds since the Unix epoch, now.
#[cfg(unix)]
pub fn now() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// A container's log, `log` in its directory, is a sequence of records, each a stream
/// byte ([`LOG_STDOUT`] or [`LOG_STDERR`]), the time the output arrived in nanoseconds
/// since the Unix epoch (big-endian u64), the output's length (big-endian u32), then the
/// output. Appends keep the streams in the order they arrived, as `docker logs` shows
/// them.
///
/// `log.idx` beside it holds one big-endian u64 for each record, written once the
/// record is whole: where the record starts, with [`INDEX_STDERR`] set for stderr's and
/// [`INDEX_LINE`] for one whose output ends a line. Readers find records by it, so a
/// record cut short, or bytes a guest wrote to look like one, are never taken for
/// records, and `--tail` reads back from the end (audit A12). A log without an index is
/// from an earlier shards, and is indexed as it is first read.
pub const LOG_STDOUT: u8 = 1;
pub const LOG_STDERR: u8 = 2;
pub const INDEX_STDERR: u64 = 1 << 63;
pub const INDEX_LINE: u64 = 1 << 62;
/// A record's start, in an index entry.
pub const INDEX_START: u64 = INDEX_LINE - 1;
/// A record's bytes before its output.
pub const LOG_HEAD: u64 = 13;

/// How much of a container's output its log keeps: `files` segments of `size` bytes at
/// most, the oldest removed first, as its daemon's settings say (daemon.rs, `Settings`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogRetention {
    pub size: u64,
    pub files: u64,
}

/// An environment value's bytes, as the OS holds them.
#[cfg(unix)]
fn os_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    std::os::unix::ffi::OsStrExt::as_bytes(value).to_vec()
}

#[cfg(not(unix))]
fn os_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A spec within a frame without its resolvers, and past it with them, is refused once
    /// they are added (review 1.y).
    #[test]
    fn a_spec_its_resolvers_push_past_a_frame_is_refused() {
        let mut spec = run::Spec {
            argv: vec![b"true".to_vec()],
            ..run::Spec::default()
        };
        let room = run::MAX_PAYLOAD as usize - spec.encoded_len().unwrap() - 4;
        spec.env = vec![vec![b'x'; room]];
        assert_eq!(spec.encoded_len(), Some(run::MAX_PAYLOAD as usize));
        assert_eq!(fits(&spec), Ok(()));
        spec.resolv = Some(b"nameserver 10.0.0.1\n".to_vec());
        assert_eq!(
            fits(&spec),
            Err("the command and its environment are too large".to_string())
        );
    }

    /// The environment for `-e` entries `env`, with shards' own environment holding only
    /// `FROM_SHARDS=yes`.
    fn env_of(env: &[&str], hostname: &str) -> Vec<String> {
        let o = Options {
            argv: vec!["x".into()],
            env: env.iter().map(|s| (*s).to_string()).collect(),
            hostname: Some(hostname.into()),
            ..Options::default()
        };
        spec(&o, |name| (name == "FROM_SHARDS").then(|| "yes".into()))
            .unwrap()
            .env
            .into_iter()
            .map(|e| String::from_utf8(e).unwrap())
            .collect()
    }

    #[test]
    fn environments_compose_as_docker_composes_them() {
        assert_eq!(
            env_of(&[], "box"),
            [format!("PATH={DEFAULT_PATH}"), "HOSTNAME=box".into()]
        );
        // Duplicates are appended, for the guest to keep the last (runc's prepareEnv).
        assert_eq!(
            env_of(&["PATH=/bin", "A=1", "A=2"], "box"),
            ["PATH=/bin", "HOSTNAME=box", "A=1", "A=2"]
        );
        // A name alone takes shards' value; without one, it unsets PATH or HOSTNAME and is
        // otherwise dropped.
        assert_eq!(
            env_of(&["A=1", "HOSTNAME", "MISSING", "FROM_SHARDS"], "box"),
            [
                format!("PATH={DEFAULT_PATH}"),
                "A=1".into(),
                "FROM_SHARDS=yes".into()
            ]
        );
        assert!(
            spec(
                &Options {
                    argv: vec!["x".into()],
                    env: vec!["=v".into()],
                    ..Options::default()
                },
                |_| None
            )
            .is_err()
        );
    }

    /// An exec's variables over the container's, as dockerd lays them (daemon/exec.go):
    /// one replaces the container's of its name, the last of two; one without a value
    /// unsets it; a new one is appended.
    #[test]
    fn an_execs_environment_is_laid_over_its_containers() {
        let o = Options {
            argv: vec!["x".into()],
            env: vec![
                "MODE=a".into(),
                "KEEP=1".into(),
                "TWICE=1".into(),
                "TWICE=2".into(),
            ],
            exec_env: vec![
                "MODE=b".into(),
                "KEEP".into(),
                "TWICE=3".into(),
                "NEW=1".into(),
                "PATH".into(),
            ],
            hostname: Some("box".into()),
            ..Options::default()
        };
        let env: Vec<String> = spec(&o, |_| None)
            .unwrap()
            .env
            .into_iter()
            .map(|e| String::from_utf8(e).unwrap())
            .collect();
        assert_eq!(env, ["HOSTNAME=box", "MODE=b", "TWICE=1", "TWICE=3", "NEW=1"]);
    }

    #[test]
    fn a_terminal_brings_term_as_dockerd_sets_it() {
        let env = |given: &[&str]| {
            let o = Options {
                argv: vec!["x".into()],
                env: given.iter().map(|s| (*s).to_string()).collect(),
                hostname: Some("box".into()),
                tty: Some(Size::default()),
                ..Options::default()
            };
            spec(&o, |_| None).unwrap().env
        };
        assert_eq!(
            env(&["A=1"]),
            [
                format!("PATH={DEFAULT_PATH}").into_bytes(),
                b"HOSTNAME=box".to_vec(),
                b"TERM=xterm".to_vec(),
                b"A=1".to_vec()
            ]
        );
        assert_eq!(env(&["TERM=vt100"]).get(2), Some(&b"TERM=vt100".to_vec()));
        assert!(!env(&["TERM"]).iter().any(|e| e.starts_with(b"TERM")));
    }

    #[test]
    fn default_hostnames_are_twelve_hex_digits() {
        let s = spec(
            &Options {
                argv: vec!["x".into()],
                ..Options::default()
            },
            |_| None,
        )
        .unwrap();
        assert_eq!(s.hostname.len(), 12);
        assert!(s.hostname.iter().all(u8::is_ascii_hexdigit));
    }
}
