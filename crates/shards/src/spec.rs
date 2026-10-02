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
#[derive(Debug, Default)]
pub struct Options {
    pub argv: Vec<String>,
    pub env: Vec<String>,
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
    let mut env: Vec<Option<Vec<u8>>> = defaults.iter().map(|d| Some(d.clone().into_bytes())).collect();
    let default_at = |key: &[u8]| {
        defaults
            .iter()
            .position(|d| d.split_once('=').is_some_and(|(k, _)| k.as_bytes() == key))
    };
    for entry in &o.env {
        let entry: Vec<u8> = match entry.split_once('=') {
            Some(("", _)) => return Err(format!("invalid environment variable: {entry}")),
            Some(_) => entry.clone().into_bytes(),
            None if entry.is_empty() => return Err("invalid environment variable: ".into()),
            None => match lookup(entry) {
                Some(value) => [entry.as_bytes(), b"=", &os_bytes(&value)].concat(),
                None => entry.clone().into_bytes(),
            },
        };
        match entry.iter().position(|&b| b == b'=') {
            None => {
                if let Some(slot) = default_at(&entry).and_then(|i| env.get_mut(i)) {
                    *slot = None;
                }
            }
            Some(eq) => match default_at(entry.get(..eq).unwrap_or_default()).and_then(|i| env.get_mut(i)) {
                Some(slot) => *slot = Some(entry),
                None => env.push(Some(entry)),
            },
        }
    }
    let spec = Spec {
        argv: o.argv.iter().map(|a| a.clone().into_bytes()).collect(),
        env: env.into_iter().flatten().collect(),
        cwd: o.workdir.clone().into_bytes(),
        user: o.user.clone().into_bytes(),
        hostname: hostname.into_bytes(),
        tty: o.tty,
        // A run on a network gets its resolvers from the daemon as it is handed over.
        resolv: None,
    };
    if spec.encoded_len().is_none_or(|n| n > run::MAX_PAYLOAD as usize) {
        return Err("the command and its environment are too large".into());
    }
    Ok(spec)
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
