//! The commands shards reads as `docker` reads them: docker/cli v29.8.1's specs and words
//! (cli/command/container: run.go, opts.go, list.go, wait.go, logs.go, rm.go, stop.go,
//! kill.go), with `shards` for `docker`. Every flag `docker` takes parses; the ones shards
//! does not serve yet are listed as `unserved`, and refuse to run (flags.rs).

use crate::flags::{Args, Command, Flag};

/// The root's help flag, which every command inherits with its shorthand deprecated and
/// itself hidden (docker/cli cli/cobra.go setupCommonRootCommand).
const HELP: Flag = Flag::bool("help", Some(b'h'), "Print usage")
    .short_deprecated("use --help")
    .hidden();

/// `shards run`, whose `-h` is the hostname, so its own `--help` has no shorthand.
pub static RUN: Command = Command {
    usage: "[OPTIONS] IMAGE [COMMAND] [ARG...]",
    about: "Create and run a new container from an image",
    aliases: "shards container run, shards run",
    args: Args::AtLeast(1),
    flags: &[
        Flag::bool(
            "detach",
            Some(b'd'),
            "Run container in background and print container ID",
        ),
        Flag::string(
            "detach-keys",
            None,
            "",
            "Override the key sequence for detaching a container",
        ),
        // Content trust is gone from the CLI; the flag only says so.
        Flag::bool(
            "disable-content-trust",
            None,
            "Skip image verification (deprecated)",
        )
        .defaulting("true")
        .deprecated("support for docker content trust was removed"),
        Flag::string(
            "entrypoint",
            None,
            "",
            "Overwrite the default ENTRYPOINT of the image",
        ),
        Flag::many("env", Some(b'e'), "list", "Set environment variables"),
        Flag::bool("help", None, "Print usage"),
        Flag::string("hostname", Some(b'h'), "", "Container host name"),
        // shards-init is every guest's PID 1, and does what docker-init does: it forwards
        // signals to the command and reaps orphans.
        Flag::bool(
            "init",
            None,
            "Run an init inside the container that forwards signals and reaps processes",
        ),
        Flag::bool("interactive", Some(b'i'), "Keep STDIN open even if not attached"),
        // Kernels no longer have the limit; the flag only says so.
        Flag::string("kernel-memory", None, "0", "Kernel memory limit (deprecated)")
            .deprecated("and no longer supported by the kernel"),
        Flag::string("name", None, "", "Assign a name to the container"),
        Flag::string(
            "pull",
            None,
            "missing",
            "Pull image before running (\"always\", \"missing\", \"never\")",
        ),
        Flag::bool(
            "rm",
            None,
            "Automatically remove the container and its associated anonymous volumes when it exits",
        ),
        Flag::bool("tty", Some(b't'), "Allocate a pseudo-TTY"),
        Flag::string(
            "user",
            Some(b'u'),
            "",
            "Username or UID (format: <name|uid>[:<group|gid>])",
        ),
        Flag::string(
            "workdir",
            Some(b'w'),
            "",
            "Working directory inside the container",
        ),
    ],
    unserved: "\
add-host - m - -\n\
annotation - m - -\n\
attach a m - -\n\
blkio-weight - s 0 -\n\
blkio-weight-device - m - -\n\
cap-add - m - -\n\
cap-drop - m - -\n\
cgroup-parent - s - -\n\
cgroupns - s - -\n\
cidfile - s - -\n\
cpu-count - i 0 -\n\
cpu-percent - i 0 -\n\
cpu-period - i 0 -\n\
cpu-quota - i 0 -\n\
cpu-rt-period - i 0 -\n\
cpu-rt-runtime - i 0 -\n\
cpu-shares c i 0 -\n\
cpus - s - -\n\
cpuset-cpus - s - -\n\
cpuset-mems - s - -\n\
device - m - -\n\
device-cgroup-rule - m - -\n\
device-read-bps - m - -\n\
device-read-iops - m - -\n\
device-write-bps - m - -\n\
device-write-iops - m - -\n\
dns - m - -\n\
dns-opt - m - dns-option\n\
dns-option - m - -\n\
dns-search - m - -\n\
domainname - s - -\n\
env-file - m - -\n\
expose - m - -\n\
gpus - m - -\n\
group-add - m - -\n\
health-cmd - s - -\n\
health-interval - s 0s -\n\
health-retries - i 0 -\n\
health-start-interval - s 0s -\n\
health-start-period - s 0s -\n\
health-timeout - s 0s -\n\
io-maxbandwidth - s 0 -\n\
io-maxiops - s 0 -\n\
ip - s <nil> -\n\
ip6 - s <nil> -\n\
ipc - s - -\n\
isolation - s - -\n\
label l m - -\n\
label-file - m - -\n\
link - m - -\n\
link-local-ip - m - -\n\
log-driver - s - -\n\
log-opt - m - -\n\
mac-address - s - -\n\
memory m s 0 -\n\
memory-reservation - s 0 -\n\
memory-swap - s 0 -\n\
memory-swappiness - i -1 -\n\
mount - m - -\n\
net - m - network\n\
net-alias - m - network-alias\n\
network - m - -\n\
network-alias - m - -\n\
no-healthcheck - b false -\n\
oom-kill-disable - b false -\n\
oom-score-adj - i 0 -\n\
pid - s - -\n\
pids-limit - i 0 -\n\
platform - s - -\n\
privileged - b false -\n\
publish p m - -\n\
publish-all P b false -\n\
quiet q b false -\n\
read-only - b false -\n\
restart - s no -\n\
runtime - s - -\n\
security-opt - m - -\n\
shm-size - s 0 -\n\
sig-proxy - b true -\n\
stop-signal - s - -\n\
stop-timeout - i 0 -\n\
storage-opt - m - -\n\
sysctl - m - -\n\
tmpfs - m - -\n\
ulimit - m - -\n\
umask - s - -\n\
use-api-socket - b false -\n\
userns - s - -\n\
uts - s - -\n\
volume v m - -\n\
volume-driver - s - -\n\
volumes-from - m - -",
    interspersed: false,
};

/// A container that did not start, as dockerd takes it (moby daemon/errors.go,
/// setExitCodeFromError): what dockerd then says, which it amends for directories, and
/// the exit code the container keeps: 126 when the command could not be invoked, 127
/// when it was not found, 128 for anything else.
pub fn start_failed(why: &str) -> (String, u8) {
    let lower = why.to_lowercase();
    if lower.contains("permission denied") {
        return (why.to_string(), 126);
    }
    // docker/cli reads 126 from "permission denied" alone (moby's comment).
    if lower.contains("is a directory") {
        return (format!("{why}: permission denied"), 126);
    }
    if lower.contains("not a directory") {
        return (
            format!(
                "{why}: Are you trying to mount a directory onto a file (or vice-versa)? Check if the specified host path exists and is the expected type"
            ),
            127,
        );
    }
    const NOT_FOUND: [&str; 4] = [
        "executable file not found",
        "no such file or directory",
        "system cannot find the file specified",
        "failed to run runc create/exec call",
    ];
    if NOT_FOUND.iter().any(|s| lower.contains(s)) {
        return (why.to_string(), 127);
    }
    (why.to_string(), 128)
}

/// `docker run`'s status for what its daemon said kept the container from running
/// (docker/cli cli/command/container/run.go, toStatusError): 127 when something was not
/// found, 126 when it was not permitted or is a directory, and 125 otherwise.
pub fn run_status(said: &str) -> u8 {
    const NOT_FOUND: [&str; 3] = [
        "executable file not found",
        "no such file or directory",
        "system cannot find the file specified",
    ];
    if NOT_FOUND.iter().any(|s| said.contains(s)) {
        127
    } else if ["permission denied", "is a directory"]
        .iter()
        .any(|s| said.contains(s))
    {
        126
    } else {
        125
    }
}

/// `shards ps`, and `shards container ls`.
pub static PS: Command = Command {
    usage: "[OPTIONS]",
    about: "List containers",
    aliases: "shards container ls, shards container list, shards container ps, shards ps",
    args: Args::None,
    flags: &[
        Flag::bool(
            "all",
            Some(b'a'),
            "Show all containers (default shows just running)",
        ),
        HELP,
        Flag::int(
            "last",
            Some(b'n'),
            "-1",
            "Show n last created containers (includes all states)",
        ),
        Flag::bool(
            "latest",
            Some(b'l'),
            "Show the latest created container (includes all states)",
        ),
        Flag::bool("no-trunc", None, "Don't truncate output"),
        Flag::bool("quiet", Some(b'q'), "Only display container IDs"),
    ],
    unserved: "\
filter f m - -\n\
format - s - -\n\
size s b false -",
    interspersed: true,
};

/// `shards wait`.
pub static WAIT: Command = Command {
    usage: "CONTAINER [CONTAINER...]",
    about: "Block until one or more containers stop, then print their exit codes",
    aliases: "shards container wait, shards wait",
    args: Args::AtLeast(1),
    flags: &[HELP],
    unserved: "",
    interspersed: true,
};

/// `shards logs`.
pub static LOGS: Command = Command {
    usage: "[OPTIONS] CONTAINER",
    about: "Fetch the logs of a container",
    aliases: "shards container logs, shards logs",
    args: Args::Exactly(1),
    flags: &[
        // Log lines carry no attributes (labels, env, tag) in shards, so this adds
        // nothing but the space before the line.
        Flag::bool("details", None, "Show extra details provided to logs"),
        Flag::bool("follow", Some(b'f'), "Follow log output"),
        HELP,
        Flag::string(
            "since",
            None,
            "",
            "Show logs since timestamp (e.g. \"2013-01-02T13:23:37Z\") or relative (e.g. \"42m\" for 42 minutes)",
        ),
        Flag::string(
            "tail",
            Some(b'n'),
            "all",
            "Number of lines to show from the end of the logs",
        ),
        Flag::bool("timestamps", Some(b't'), "Show timestamps"),
        Flag::string(
            "until",
            None,
            "",
            "Show logs before a timestamp (e.g. \"2013-01-02T13:23:37Z\") or relative (e.g. \"42m\" for 42 minutes)",
        ),
    ],
    unserved: "",
    interspersed: true,
};

/// `shards rm`, and `shards container rm`.
pub static RM: Command = Command {
    usage: "[OPTIONS] CONTAINER [CONTAINER...]",
    about: "Remove one or more containers",
    aliases: "shards container rm, shards container remove, shards rm",
    args: Args::AtLeast(1),
    flags: &[
        Flag::bool(
            "force",
            Some(b'f'),
            "Force the removal of a running container (uses SIGKILL)",
        ),
        HELP,
        // A container has no anonymous volumes in shards: there is nothing more to remove.
        Flag::bool(
            "volumes",
            Some(b'v'),
            "Remove anonymous volumes associated with the container",
        ),
    ],
    unserved: "\
link l b false -",
    interspersed: true,
};

/// `shards stop`.
pub static STOP: Command = Command {
    usage: "[OPTIONS] CONTAINER [CONTAINER...]",
    about: "Stop one or more running containers",
    aliases: "shards container stop, shards stop",
    args: Args::AtLeast(1),
    flags: &[
        HELP,
        Flag::string("signal", Some(b's'), "", "Signal to send to the container"),
        Flag::int(
            "time",
            None,
            "0",
            "Seconds to wait before killing the container (deprecated: use --timeout)",
        )
        .sharing("timeout")
        .deprecated("use --timeout instead"),
        Flag::int(
            "timeout",
            Some(b't'),
            "0",
            "Seconds to wait before killing the container",
        ),
    ],
    unserved: "",
    interspersed: true,
};

/// `shards kill`.
pub static KILL: Command = Command {
    usage: "[OPTIONS] CONTAINER [CONTAINER...]",
    about: "Kill one or more running containers",
    aliases: "shards container kill, shards kill",
    args: Args::AtLeast(1),
    flags: &[
        HELP,
        Flag::string("signal", Some(b's'), "", "Signal to send to the container"),
    ],
    unserved: "",
    interspersed: true,
};

/// The container command the start of `words` names, its path (`shards ps`, `shards
/// container ls`) and how many words named it: `ps` or `container ls` (or `container ps`,
/// `container list`), `rm` or `container rm` (or `container remove`), and the others by
/// the one name, alone or under `container`.
pub fn find(words: &[&str]) -> Option<(&'static Command, &'static str, usize)> {
    Some(match words {
        ["ps", ..] => (&PS, "shards ps", 1),
        ["container", "ls" | "list" | "ps", ..] => (&PS, "shards container ls", 2),
        ["wait", ..] => (&WAIT, "shards wait", 1),
        ["container", "wait", ..] => (&WAIT, "shards container wait", 2),
        ["logs", ..] => (&LOGS, "shards logs", 1),
        ["container", "logs", ..] => (&LOGS, "shards container logs", 2),
        ["rm", ..] => (&RM, "shards rm", 1),
        ["container", "rm" | "remove", ..] => (&RM, "shards container rm", 2),
        ["stop", ..] => (&STOP, "shards stop", 1),
        ["container", "stop", ..] => (&STOP, "shards container stop", 2),
        ["kill", ..] => (&KILL, "shards kill", 1),
        ["container", "kill", ..] => (&KILL, "shards container kill", 2),
        _ => return None,
    })
}
