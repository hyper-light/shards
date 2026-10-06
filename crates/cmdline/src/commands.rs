//! The commands shards reads as `docker` reads them: docker/cli v29.8.1's specs and words
//! (cli/command/container: run.go, opts.go, list.go, wait.go, logs.go, rm.go, stop.go,
//! kill.go), with `shards` for `docker`. Every flag `docker` takes parses; the ones shards
//! does not serve yet are listed as `unserved`, and refuse to run (flags.rs).

use crate::flags::{Args, Command, Flag};

/// The root's help flag, which every command inherits, hidden (docker/cli cli/cobra.go
/// setupCommonRootCommand). docker/cli deprecates its shorthand; shards keeps `-h`.
const HELP: Flag = Flag::bool("help", Some(b'h'), "Print usage").hidden();

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
        Flag::string("health-cmd", None, "", "Command to run to check health"),
        Flag::duration(
            "health-interval",
            None,
            "Time between running the check (ms|s|m|h) (default 0s)",
        ),
        Flag::int(
            "health-retries",
            None,
            "0",
            "Consecutive failures needed to report unhealthy",
        ),
        Flag::duration(
            "health-start-interval",
            None,
            "Time between running the check during the start period (ms|s|m|h) (default 0s)",
        ),
        Flag::duration(
            "health-start-period",
            None,
            "Start period for the container to initialize before starting health-retries countdown (ms|s|m|h) (default 0s)",
        ),
        Flag::duration(
            "health-timeout",
            None,
            "Maximum time to allow one check to run (ms|s|m|h) (default 0s)",
        ),
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
        // One value, `--net` and `--network` alike (docker/cli opts.go addFlags).
        Flag::many("net", None, "network", "Connect a container to a network")
            .sharing("network")
            .hidden(),
        Flag::many("network", None, "network", "Connect a container to a network"),
        Flag::bool(
            "no-healthcheck",
            None,
            "Disable any container-specified HEALTHCHECK",
        ),
        Flag::many(
            "publish",
            Some(b'p'),
            "list",
            "Publish a container's port(s) to the host",
        ),
        Flag::bool(
            "publish-all",
            Some(b'P'),
            "Publish all exposed ports to random ports",
        ),
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
        Flag::string(
            "restart",
            None,
            "no",
            "Restart policy to apply when a container exits",
        ),
        Flag::string("stop-signal", None, "", "Signal to stop the container"),
        Flag::int(
            "stop-timeout",
            None,
            "0",
            "Timeout (in seconds) to stop a container",
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
        Flag::many(
            "add-host",
            None,
            "list",
            "Add a custom host-to-IP mapping (host:ip)",
        ),
        Flag::many("dns", None, "list", "Set custom DNS servers"),
        Flag::many("dns-opt", None, "list", "Set DNS options")
            .sharing("dns-option")
            .hidden(),
        Flag::many("dns-option", None, "list", "Set DNS options"),
        Flag::many("dns-search", None, "list", "Set custom DNS search domains"),
        Flag::string("domainname", None, "", "Container NIS domain name"),
        Flag::string("cidfile", None, "", "Write the container ID to the file"),
        Flag::many(
            "mount",
            None,
            "mount",
            "Attach a filesystem mount to the container",
        ),
        Flag::many("volume", Some(b'v'), "list", "Bind mount a volume"),
        Flag::string(
            "volume-driver",
            None,
            "",
            "Optional volume driver for the container",
        ),
        Flag::many(
            "volumes-from",
            None,
            "list",
            "Mount volumes from the specified container(s)",
        ),
        Flag::many("cap-add", None, "list", "Add Linux capabilities"),
        Flag::many("cap-drop", None, "list", "Drop Linux capabilities"),
        Flag::many("group-add", None, "list", "Add additional groups to join"),
        Flag::int(
            "oom-score-adj",
            None,
            "0",
            "Tune host's OOM preferences (-1000 to 1000)",
        ),
        Flag::bool("privileged", None, "Give extended privileges to this container"),
        Flag::many("security-opt", None, "list", "Security Options"),
        Flag::many("device", None, "list", "Add a host device to the container"),
        Flag::many(
            "device-cgroup-rule",
            None,
            "list",
            "Add a rule to the cgroup allowed devices list",
        ),
        Flag::many(
            "blkio-weight-device",
            None,
            "list",
            "Block IO weight (relative device weight)",
        )
        .defaulting("[]"),
        Flag::many(
            "device-read-bps",
            None,
            "list",
            "Limit read rate (bytes per second) from a device",
        )
        .defaulting("[]"),
        Flag::many(
            "device-read-iops",
            None,
            "list",
            "Limit read rate (IO per second) from a device",
        )
        .defaulting("[]"),
        Flag::many(
            "device-write-bps",
            None,
            "list",
            "Limit write rate (bytes per second) to a device",
        )
        .defaulting("[]"),
        Flag::many(
            "device-write-iops",
            None,
            "list",
            "Limit write rate (IO per second) to a device",
        )
        .defaulting("[]"),
        Flag::bool(
            "read-only",
            None,
            "Mount the container's root filesystem as read only",
        ),
        Flag::value("shm-size", None, "bytes", "Size of /dev/shm"),
        Flag::many("sysctl", None, "map", "Sysctl options").defaulting("map[]"),
        Flag::many("tmpfs", None, "list", "Mount a tmpfs directory"),
        Flag::many("ulimit", None, "ulimit", "Ulimit options").defaulting("[]"),
        Flag::int(
            "cpu-period",
            None,
            "0",
            "Limit CPU CFS (Completely Fair Scheduler) period",
        ),
        Flag::int(
            "cpu-quota",
            None,
            "0",
            "Limit CPU CFS (Completely Fair Scheduler) quota",
        ),
        Flag::value(
            "blkio-weight",
            None,
            "uint16",
            "Block IO (relative weight), between 10 and 1000, or 0 to disable (default 0)",
        ),
        Flag::int("cpu-shares", Some(b'c'), "0", "CPU shares (relative weight)"),
        Flag::value("cpus", None, "decimal", "Number of CPUs"),
        Flag::string(
            "cpuset-cpus",
            None,
            "",
            "CPUs in which to allow execution (0-3, 0,1)",
        ),
        Flag::string(
            "cpuset-mems",
            None,
            "",
            "MEMs in which to allow execution (0-3, 0,1)",
        ),
        Flag::value("memory", Some(b'm'), "bytes", "Memory limit"),
        Flag::value("memory-reservation", None, "bytes", "Memory soft limit"),
        Flag::value(
            "memory-swap",
            None,
            "bytes",
            "Swap limit equal to memory plus swap: '-1' to enable unlimited swap",
        ),
        Flag::int(
            "memory-swappiness",
            None,
            "-1",
            "Tune container memory swappiness (0 to 100)",
        ),
        Flag::bool("oom-kill-disable", None, "Disable OOM Killer"),
        Flag::int(
            "pids-limit",
            None,
            "0",
            "Tune container pids limit (set -1 for unlimited)",
        ),
        Flag::string(
            "platform",
            None,
            "",
            "Set platform if server is multi-platform capable",
        ),
        Flag::bool("quiet", Some(b'q'), "Suppress the pull output"),
        Flag::bool("sig-proxy", None, "Proxy received signals to the process").defaulting("true"),
        Flag::many(
            "env-file",
            None,
            "list",
            "Read in a file of environment variables",
        ),
        Flag::many("expose", None, "list", "Expose a port or a range of ports"),
        Flag::many("label", Some(b'l'), "list", "Set meta data on a container"),
        Flag::many(
            "label-file",
            None,
            "list",
            "Read in a line delimited file of labels",
        ),
    ],
    unserved: "\
annotation - m - -\n\
attach a m - -\n\
blkio-weight-cgroup-parent - s - -\n\
cgroupns - s - -\n\
cpu-count - i 0 -\n\
cpu-percent - i 0 -\n\
cpu-rt-period - i 0 -\n\
cpu-rt-runtime - i 0 -\n\
cpuset-gpus - m - -\n\
io-maxbandwidth - s 0 -\n\
io-maxiops - s 0 -\n\
ip - s <nil> -\n\
ip6 - s <nil> -\n\
ipc - s - -\n\
isolation - s - -\n\
link - m - -\n\
link-local-ip - m - -\n\
log-driver - s - -\n\
log-opt - m - -\n\
mac-address - s - -\n\
net-alias - m - network-alias\n\
network-alias - m - -\n\
pid - s - -\n\
runtime - s - -\n\
storage-opt - m - -\n\
umask - s - -\n\
use-api-socket - b false -\n\
userns - s - -\n\
uts - s - -",
    interspersed: false,
    error_prefix: "",
};

/// `shards create` (docker/cli cli/command/container/create.go): `run`'s container flags,
/// without what only `run` has.
pub static CREATE: Command = Command {
    usage: "[OPTIONS] IMAGE [COMMAND] [ARG...]",
    about: "Create a new container",
    aliases: "shards container create, shards create",
    args: Args::AtLeast(1),
    flags: &[
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
        Flag::string("health-cmd", None, "", "Command to run to check health"),
        Flag::duration(
            "health-interval",
            None,
            "Time between running the check (ms|s|m|h) (default 0s)",
        ),
        Flag::int(
            "health-retries",
            None,
            "0",
            "Consecutive failures needed to report unhealthy",
        ),
        Flag::duration(
            "health-start-interval",
            None,
            "Time between running the check during the start period (ms|s|m|h) (default 0s)",
        ),
        Flag::duration(
            "health-start-period",
            None,
            "Start period for the container to initialize before starting health-retries countdown (ms|s|m|h) (default 0s)",
        ),
        Flag::duration(
            "health-timeout",
            None,
            "Maximum time to allow one check to run (ms|s|m|h) (default 0s)",
        ),
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
        // One value, `--net` and `--network` alike (docker/cli opts.go addFlags).
        Flag::many("net", None, "network", "Connect a container to a network")
            .sharing("network")
            .hidden(),
        Flag::many("network", None, "network", "Connect a container to a network"),
        Flag::bool(
            "no-healthcheck",
            None,
            "Disable any container-specified HEALTHCHECK",
        ),
        Flag::many(
            "publish",
            Some(b'p'),
            "list",
            "Publish a container's port(s) to the host",
        ),
        Flag::bool(
            "publish-all",
            Some(b'P'),
            "Publish all exposed ports to random ports",
        ),
        Flag::string(
            "pull",
            None,
            "missing",
            "Pull image before creating (\"always\", \"missing\", \"never\")",
        ),
        Flag::bool(
            "rm",
            None,
            "Automatically remove the container and its associated anonymous volumes when it exits",
        ),
        Flag::string(
            "restart",
            None,
            "no",
            "Restart policy to apply when a container exits",
        ),
        Flag::string("stop-signal", None, "", "Signal to stop the container"),
        Flag::int(
            "stop-timeout",
            None,
            "0",
            "Timeout (in seconds) to stop a container",
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
        Flag::many(
            "add-host",
            None,
            "list",
            "Add a custom host-to-IP mapping (host:ip)",
        ),
        Flag::many("dns", None, "list", "Set custom DNS servers"),
        Flag::many("dns-opt", None, "list", "Set DNS options")
            .sharing("dns-option")
            .hidden(),
        Flag::many("dns-option", None, "list", "Set DNS options"),
        Flag::many("dns-search", None, "list", "Set custom DNS search domains"),
        Flag::string("domainname", None, "", "Container NIS domain name"),
        Flag::string("cidfile", None, "", "Write the container ID to the file"),
        Flag::many(
            "mount",
            None,
            "mount",
            "Attach a filesystem mount to the container",
        ),
        Flag::many("volume", Some(b'v'), "list", "Bind mount a volume"),
        Flag::string(
            "volume-driver",
            None,
            "",
            "Optional volume driver for the container",
        ),
        Flag::many(
            "volumes-from",
            None,
            "list",
            "Mount volumes from the specified container(s)",
        ),
        Flag::many("cap-add", None, "list", "Add Linux capabilities"),
        Flag::many("cap-drop", None, "list", "Drop Linux capabilities"),
        Flag::many("group-add", None, "list", "Add additional groups to join"),
        Flag::int(
            "oom-score-adj",
            None,
            "0",
            "Tune host's OOM preferences (-1000 to 1000)",
        ),
        Flag::bool("privileged", None, "Give extended privileges to this container"),
        Flag::many("security-opt", None, "list", "Security Options"),
        Flag::many("device", None, "list", "Add a host device to the container"),
        Flag::many(
            "device-cgroup-rule",
            None,
            "list",
            "Add a rule to the cgroup allowed devices list",
        ),
        Flag::many(
            "blkio-weight-device",
            None,
            "list",
            "Block IO weight (relative device weight)",
        )
        .defaulting("[]"),
        Flag::many(
            "device-read-bps",
            None,
            "list",
            "Limit read rate (bytes per second) from a device",
        )
        .defaulting("[]"),
        Flag::many(
            "device-read-iops",
            None,
            "list",
            "Limit read rate (IO per second) from a device",
        )
        .defaulting("[]"),
        Flag::many(
            "device-write-bps",
            None,
            "list",
            "Limit write rate (bytes per second) to a device",
        )
        .defaulting("[]"),
        Flag::many(
            "device-write-iops",
            None,
            "list",
            "Limit write rate (IO per second) to a device",
        )
        .defaulting("[]"),
        Flag::bool(
            "read-only",
            None,
            "Mount the container's root filesystem as read only",
        ),
        Flag::value("shm-size", None, "bytes", "Size of /dev/shm"),
        Flag::many("sysctl", None, "map", "Sysctl options").defaulting("map[]"),
        Flag::many("tmpfs", None, "list", "Mount a tmpfs directory"),
        Flag::many("ulimit", None, "ulimit", "Ulimit options").defaulting("[]"),
        Flag::int(
            "cpu-period",
            None,
            "0",
            "Limit CPU CFS (Completely Fair Scheduler) period",
        ),
        Flag::int(
            "cpu-quota",
            None,
            "0",
            "Limit CPU CFS (Completely Fair Scheduler) quota",
        ),
        Flag::value(
            "blkio-weight",
            None,
            "uint16",
            "Block IO (relative weight), between 10 and 1000, or 0 to disable (default 0)",
        ),
        Flag::int("cpu-shares", Some(b'c'), "0", "CPU shares (relative weight)"),
        Flag::value("cpus", None, "decimal", "Number of CPUs"),
        Flag::string(
            "cpuset-cpus",
            None,
            "",
            "CPUs in which to allow execution (0-3, 0,1)",
        ),
        Flag::string(
            "cpuset-mems",
            None,
            "",
            "MEMs in which to allow execution (0-3, 0,1)",
        ),
        Flag::value("memory", Some(b'm'), "bytes", "Memory limit"),
        Flag::value("memory-reservation", None, "bytes", "Memory soft limit"),
        Flag::value(
            "memory-swap",
            None,
            "bytes",
            "Swap limit equal to memory plus swap: '-1' to enable unlimited swap",
        ),
        Flag::int(
            "memory-swappiness",
            None,
            "-1",
            "Tune container memory swappiness (0 to 100)",
        ),
        Flag::bool("oom-kill-disable", None, "Disable OOM Killer"),
        Flag::int(
            "pids-limit",
            None,
            "0",
            "Tune container pids limit (set -1 for unlimited)",
        ),
        Flag::string(
            "platform",
            None,
            "",
            "Set platform if server is multi-platform capable",
        ),
        Flag::bool("quiet", Some(b'q'), "Suppress the pull output"),
        Flag::many(
            "env-file",
            None,
            "list",
            "Read in a file of environment variables",
        ),
        Flag::many("expose", None, "list", "Expose a port or a range of ports"),
        Flag::many("label", Some(b'l'), "list", "Set meta data on a container"),
        Flag::many(
            "label-file",
            None,
            "list",
            "Read in a line delimited file of labels",
        ),
    ],
    unserved: "\
annotation - m - -\n\
attach a m - -\n\
blkio-weight-cgroup-parent - s - -\n\
cgroupns - s - -\n\
cpu-count - i 0 -\n\
cpu-percent - i 0 -\n\
cpu-rt-period - i 0 -\n\
cpu-rt-runtime - i 0 -\n\
cpus - s - -\n\
gpus - m - -\n\
io-maxbandwidth - s 0 -\n\
io-maxiops - s 0 -\n\
ip - s <nil> -\n\
ip6 - s <nil> -\n\
ipc - s - -\n\
isolation - s - -\n\
link - m - -\n\
link-local-ip - m - -\n\
log-driver - s - -\n\
log-opt - m - -\n\
mac-address - s - -\n\
net-alias - m - network-alias\n\
network-alias - m - -\n\
pid - s - -\n\
runtime - s - -\n\
storage-opt - m - -\n\
umask - s - -\n\
use-api-socket - b false -\n\
userns - s - -\n\
uts - s - -",
    interspersed: false,
    error_prefix: "",
};

/// `shards commit` (docker/cli cli/command/container/commit.go).
pub static COMMIT: Command = Command {
    usage: "[OPTIONS] CONTAINER [REPOSITORY[:TAG]]",
    about: "Create a new image from a container's changes",
    aliases: "shards container commit, shards commit",
    args: Args::Range(1, 2),
    flags: &[
        Flag::string(
            "author",
            Some(b'a'),
            "",
            "Author (e.g., \"John Hannibal Smith <hannibal@a-team.com>\")",
        ),
        Flag::many(
            "change",
            Some(b'c'),
            "list",
            "Apply Dockerfile instruction to the created image",
        ),
        HELP,
        Flag::string("message", Some(b'm'), "", "Commit message"),
        Flag::bool("no-pause", None, "Disable pausing container during commit"),
        Flag::bool(
            "pause",
            Some(b'p'),
            "Pause container during commit (deprecated: use --no-pause instead)",
        )
        .defaulting("true")
        .deprecated("and enabled by default. Use --no-pause to disable pausing during commit."),
    ],
    unserved: "",
    interspersed: false,
    error_prefix: "",
};

/// docker/cli's flags.InspectFormatHelp (cli/flags/options.go).
const INSPECT_FORMAT_HELP: &str = "Format output using a custom template:\n'json':             Print in JSON format\n'TEMPLATE':         Print output using the given Go template.\nRefer to https://docs.docker.com/go/formatting/ for more information about formatting output with templates";

/// `shards version` (docker/cli cli/command/system/version.go).
pub static VERSION: Command = Command {
    usage: "[OPTIONS]",
    about: "Show the Docker version information",
    aliases: "",
    args: Args::None,
    flags: &[Flag::string("format", Some(b'f'), "", INSPECT_FORMAT_HELP), HELP],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards inspect` (docker/cli cli/command/system/inspect.go).
pub static INSPECT: Command = Command {
    usage: "[OPTIONS] NAME|ID [NAME|ID...]",
    about: "Return low-level information on Docker objects",
    aliases: "",
    args: Args::AtLeast(1),
    flags: &[
        Flag::string("format", Some(b'f'), "", INSPECT_FORMAT_HELP),
        HELP,
        Flag::bool(
            "size",
            Some(b's'),
            "Display total file sizes if the type is container",
        ),
        Flag::string("type", None, "", "Only inspect objects of the given type"),
    ],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards cp` (docker/cli cli/command/container/cp.go): its Long, which its help shows.
pub static COPY: Command = Command {
    usage: "[OPTIONS] CONTAINER:SRC_PATH DEST_PATH|-\n\tdocker cp [OPTIONS] SRC_PATH|- CONTAINER:DEST_PATH",
    about: "Copy files/folders between a container and the local filesystem\n\nUse '-' as the source to read a tar archive from stdin\nand extract it to a directory destination in a container.\nUse '-' as the destination to stream a tar archive of a\ncontainer source to stdout.",
    aliases: "shards container cp, shards cp",
    args: Args::Exactly(2),
    flags: &[
        Flag::bool(
            "archive",
            Some(b'a'),
            "Archive mode (copy all uid/gid information)",
        ),
        Flag::bool("follow-link", Some(b'L'), "Always follow symlinks in SRC_PATH"),
        HELP,
        Flag::bool(
            "quiet",
            Some(b'q'),
            "Suppress progress output during copy. Progress output is automatically suppressed if no terminal is attached",
        ),
    ],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards start` (docker/cli cli/command/container/start.go).
pub static START: Command = Command {
    usage: "[OPTIONS] CONTAINER [CONTAINER...]",
    about: "Start one or more stopped containers",
    aliases: "shards container start, shards start",
    args: Args::AtLeast(1),
    flags: &[
        Flag::bool("attach", Some(b'a'), "Attach STDOUT/STDERR and forward signals"),
        Flag::string(
            "detach-keys",
            None,
            "",
            "Override the key sequence for detaching a container",
        ),
        HELP,
        Flag::bool("interactive", Some(b'i'), "Attach container's STDIN"),
    ],
    unserved: "\
checkpoint - s - -\n\
checkpoint-dir - s - -",
    interspersed: true,
    error_prefix: "",
};

/// `shards restart` (docker/cli cli/command/container/restart.go).
pub static RESTART: Command = Command {
    usage: "[OPTIONS] CONTAINER [CONTAINER...]",
    about: "Restart one or more containers",
    aliases: "shards container restart, shards restart",
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
    error_prefix: "",
};

/// A container that did not start, as dockerd takes it (moby daemon/errors.go,
/// setExitCodeFromError): what dockerd then says, which it amends for directories, and
/// the exit code the container keeps: 126 when the command could not be invoked, 127
/// when it was not found, 128 for anything else.
pub fn start_failed(why: &str) -> (String, u8) {
    // What dockerd refuses as it makes the spec, before the runtime: its code, untranslated.
    const DAEMONS: [&str; 5] = [
        "error gathering device information while adding custom device ",
        "invalid device cgroup rule format: ",
        "invalid major value in device cgroup rule format: ",
        "invalid minor value in device cgroup rule format: ",
        "CDI device injection failed: ",
    ];
    if DAEMONS.iter().any(|d| why.starts_with(d)) {
        return (why.to_string(), 128);
    }
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
        Flag::many(
            "filter",
            Some(b'f'),
            "filter",
            "Filter output based on conditions provided",
        ),
        Flag::string(
            "format",
            None,
            "",
            "Format output using a custom template:\n'table':            Print output in table format with column headers (default)\n'table TEMPLATE':   Print output in table format using the given Go template\n'json':             Print in JSON format\n'TEMPLATE':         Print output using the given Go template.\nRefer to https://docs.docker.com/go/formatting/ for more information about formatting output with templates",
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
        Flag::bool("size", Some(b's'), "Display total file sizes"),
    ],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards images`.
pub static IMAGES: Command = Command {
    usage: "[OPTIONS] [REPOSITORY[:TAG]]",
    about: "List images",
    aliases: "shards image ls, shards image list, shards images",
    args: Args::AtMost(1),
    flags: &[
        Flag::bool(
            "all",
            Some(b'a'),
            "Show all images (default hides intermediate and dangling images)",
        ),
        Flag::bool("digests", None, "Show digests"),
        Flag::many(
            "filter",
            Some(b'f'),
            "filter",
            "Filter output based on conditions provided",
        ),
        Flag::string(
            "format",
            None,
            "",
            "Format output using a custom template:\n'table':            Print output in table format with column headers (default)\n'table TEMPLATE':   Print output in table format using the given Go template\n'json':             Print in JSON format\n'TEMPLATE':         Print output using the given Go template.\nRefer to https://docs.docker.com/go/formatting/ for more information about formatting output with templates",
        ),
        HELP,
        Flag::bool("no-trunc", None, "Don't truncate output"),
        Flag::bool("quiet", Some(b'q'), "Only show image IDs"),
        Flag::bool(
            "tree",
            None,
            "List multi-platform images as a tree (EXPERIMENTAL)",
        ),
    ],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards rmi`.
pub static RMI: Command = Command {
    usage: "[OPTIONS] IMAGE [IMAGE...]",
    about: "Remove one or more images",
    aliases: "shards image rm, shards image remove, shards rmi",
    args: Args::AtLeast(1),
    flags: &[
        Flag::bool("force", Some(b'f'), "Force removal of the image"),
        HELP,
        Flag::bool("no-prune", None, "Do not delete untagged parents"),
        Flag::bool("vms", None, "Remove stopped microVMs").extension(),
    ],
    unserved: "\
platform - l - -",
    interspersed: true,
    error_prefix: "",
};

/// `shards image inspect`.
pub static IMAGE_INSPECT: Command = Command {
    usage: "[OPTIONS] IMAGE [IMAGE...]",
    about: "Display detailed information on one or more images",
    aliases: "",
    args: Args::AtLeast(1),
    flags: &[Flag::string("format", Some(b'f'), "", INSPECT_FORMAT_HELP), HELP],
    unserved: "\
platform - s - -",
    interspersed: true,
    error_prefix: "",
};

/// `shards container inspect`, `shards inspect vm`: a microVM's document, Docker's
/// container fields and its microVM's own.
pub static CONTAINER_INSPECT: Command = Command {
    usage: "[OPTIONS] CONTAINER [CONTAINER...]",
    about: "Display detailed information on one or more containers",
    aliases: "",
    args: Args::AtLeast(1),
    flags: &[
        Flag::string("format", Some(b'f'), "", INSPECT_FORMAT_HELP),
        HELP,
        Flag::bool("size", Some(b's'), "Display total file sizes"),
    ],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards history` (docker/cli cli/command/image/history.go): an image's layers and
/// how each was made, newest first.
pub static HISTORY: Command = Command {
    usage: "[OPTIONS] IMAGE",
    about: "Show the history of an image",
    aliases: "shards image history, shards history",
    args: Args::Exactly(1),
    flags: &[
        Flag::string(
            "format",
            None,
            "",
            "Format output using a custom template:\n'table':            Print output in table format with column headers (default)\n'table TEMPLATE':   Print output in table format using the given Go template\n'json':             Print in JSON format\n'TEMPLATE':         Print output using the given Go template.\nRefer to https://docs.docker.com/go/formatting/ for more information about formatting output with templates",
        ),
        HELP,
        Flag::bool(
            "human",
            Some(b'H'),
            "Print sizes and dates in human readable format",
        )
        .defaulting("true"),
        Flag::bool("no-trunc", None, "Don't truncate output"),
        Flag::bool("quiet", Some(b'q'), "Only show image IDs"),
    ],
    unserved: "\
platform - s - -",
    interspersed: true,
    error_prefix: "",
};

/// `shards rename` (docker/cli cli/command/container/rename.go).
pub static RENAME: Command = Command {
    usage: "CONTAINER NEW_NAME",
    about: "Rename a container",
    aliases: "shards container rename, shards rename",
    args: Args::Exactly(2),
    flags: &[HELP],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards system df` (docker/cli cli/command/system/df.go): what the store and the
/// microVMs take on disk.
pub static SYSTEM_DF: Command = Command {
    usage: "[OPTIONS]",
    about: "Show docker disk usage",
    aliases: "",
    args: Args::None,
    flags: &[
        Flag::string(
            "format",
            None,
            "",
            "Format output using a custom template:\n'table':            Print output in table format with column headers (default)\n'table TEMPLATE':   Print output in table format using the given Go template\n'json':             Print in JSON format\n'TEMPLATE':         Print output using the given Go template.\nRefer to https://docs.docker.com/go/formatting/ for more information about formatting output with templates",
        ),
        HELP,
        Flag::bool("verbose", Some(b'v'), "Show detailed information on space usage"),
    ],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards container prune` (docker/cli cli/command/container/prune.go).
pub static CONTAINER_PRUNE: Command = Command {
    usage: "[OPTIONS]",
    about: "Remove all stopped containers",
    aliases: "",
    args: Args::None,
    flags: &[
        Flag::many(
            "filter",
            None,
            "filter",
            "Provide filter values (e.g. \"until=<timestamp>\")",
        ),
        Flag::bool("force", Some(b'f'), "Do not prompt for confirmation"),
        HELP,
    ],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards image prune` (docker/cli cli/command/image/prune.go).
pub static IMAGE_PRUNE: Command = Command {
    usage: "[OPTIONS]",
    about: "Remove unused images",
    aliases: "",
    args: Args::None,
    flags: &[
        Flag::bool(
            "all",
            Some(b'a'),
            "Remove all unused images, not just dangling ones",
        ),
        Flag::many(
            "filter",
            None,
            "filter",
            "Provide filter values (e.g. \"until=<timestamp>\")",
        ),
        Flag::bool("force", Some(b'f'), "Do not prompt for confirmation"),
        HELP,
    ],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards import` (docker/cli cli/command/image/import.go).
pub static IMPORT: Command = Command {
    usage: "[OPTIONS] file|URL|- [REPOSITORY[:TAG]]",
    about: "Import the contents from a tarball to create a filesystem image",
    aliases: "shards image import, shards import",
    args: Args::AtLeast(1),
    flags: &[
        Flag::many(
            "change",
            Some(b'c'),
            "list",
            "Apply Dockerfile instruction to the created image",
        ),
        HELP,
        Flag::string("message", Some(b'm'), "", "Set commit message for imported image"),
        Flag::string(
            "platform",
            None,
            "",
            "Set platform if server is multi-platform capable",
        ),
    ],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards update` (docker/cli cli/command/container/update.go): a running container's
/// limits change as it runs.
pub static UPDATE: Command = Command {
    usage: "[OPTIONS] CONTAINER [CONTAINER...]",
    about: "Update configuration of one or more containers",
    aliases: "shards container update, shards update",
    args: Args::AtLeast(1),
    flags: &[
        Flag::value(
            "blkio-weight",
            None,
            "uint16",
            "Block IO (relative weight), between 10 and 1000, or 0 to disable (default 0)",
        ),
        Flag::int(
            "cpu-period",
            None,
            "0",
            "Limit CPU CFS (Completely Fair Scheduler) period",
        ),
        Flag::int(
            "cpu-quota",
            None,
            "0",
            "Limit CPU CFS (Completely Fair Scheduler) quota",
        ),
        Flag::int("cpu-shares", Some(b'c'), "0", "CPU shares (relative weight)"),
        Flag::value("cpus", None, "decimal", "Number of CPUs"),
        Flag::string(
            "cpuset-cpus",
            None,
            "",
            "CPUs in which to allow execution (0-3, 0,1)",
        ),
        Flag::string(
            "cpuset-mems",
            None,
            "",
            "MEMs in which to allow execution (0-3, 0,1)",
        ),
        HELP,
        // Kernels no longer have the limit; the flag only says so.
        Flag::value("kernel-memory", None, "bytes", "Kernel memory limit (deprecated)")
            .deprecated("and no longer supported by the kernel"),
        Flag::value("memory", Some(b'm'), "bytes", "Memory limit"),
        Flag::value("memory-reservation", None, "bytes", "Memory soft limit"),
        Flag::value(
            "memory-swap",
            None,
            "bytes",
            "Swap limit equal to memory plus swap: -1 to enable unlimited swap",
        ),
        Flag::int(
            "pids-limit",
            None,
            "0",
            "Tune container pids limit (set -1 for unlimited)",
        ),
        Flag::string(
            "restart",
            None,
            "",
            "Restart policy to apply when a container exits",
        ),
    ],
    unserved: "\
cpu-rt-period - i 0 -
cpu-rt-runtime - i 0 -",
    interspersed: true,
    error_prefix: "",
};

/// `shards volume create` (docker/cli cli/command/volume/create.go): its cluster volume
/// flags are swarm's, which shards does not serve.
pub static VOLUME_CREATE: Command = Command {
    usage: "[OPTIONS] [VOLUME]",
    about: "Create a volume",
    aliases: "",
    args: Args::AtMost(1),
    flags: &[
        Flag::string("driver", Some(b'd'), "local", "Specify volume driver name"),
        HELP,
        Flag::many("label", None, "list", "Set metadata for a volume"),
        Flag::string("name", None, "", "Specify volume name").hidden(),
        Flag::many("opt", Some(b'o'), "map", "Set driver specific options").defaulting("map[]"),
    ],
    unserved: "\
availability - s active -
group - s - -
limit-bytes - s 0 -
required-bytes - s 0 -
scope - s single -
secret - m - -
sharing - s none -
topology-preferred - m - -
topology-required - m - -
type - s block -",
    interspersed: true,
    error_prefix: "",
};

/// `shards volume ls` (docker/cli cli/command/volume/list.go).
pub static VOLUME_LS: Command = Command {
    usage: "[OPTIONS]",
    about: "List volumes",
    aliases: "shards volume ls, shards volume list",
    args: Args::None,
    flags: &[
        Flag::many(
            "filter",
            Some(b'f'),
            "filter",
            "Provide filter values (e.g. \"dangling=true\")",
        ),
        Flag::string(
            "format",
            None,
            "",
            "Format output using a custom template:\n'table':            Print output in table format with column headers (default)\n'table TEMPLATE':   Print output in table format using the given Go template\n'json':             Print in JSON format\n'TEMPLATE':         Print output using the given Go template.\nRefer to https://docs.docker.com/go/formatting/ for more information about formatting output with templates",
        ),
        HELP,
        Flag::bool("quiet", Some(b'q'), "Only display volume names"),
    ],
    unserved: "\
cluster - b false -",
    interspersed: true,
    error_prefix: "",
};

/// `shards volume inspect` (docker/cli cli/command/volume/inspect.go).
pub static VOLUME_INSPECT: Command = Command {
    usage: "[OPTIONS] VOLUME [VOLUME...]",
    about: "Display detailed information on one or more volumes",
    aliases: "",
    args: Args::AtLeast(1),
    flags: &[Flag::string("format", Some(b'f'), "", INSPECT_FORMAT_HELP), HELP],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards volume rm` (docker/cli cli/command/volume/remove.go).
pub static VOLUME_RM: Command = Command {
    usage: "[OPTIONS] VOLUME [VOLUME...]",
    about: "Remove one or more volumes. You cannot remove a volume that is in use by a container.",
    aliases: "shards volume rm, shards volume remove",
    args: Args::AtLeast(1),
    flags: &[
        Flag::bool("force", Some(b'f'), "Force the removal of one or more volumes"),
        HELP,
    ],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards volume prune` (docker/cli cli/command/volume/prune.go).
pub static VOLUME_PRUNE: Command = Command {
    usage: "[OPTIONS]",
    about: "Remove unused local volumes",
    aliases: "",
    args: Args::None,
    flags: &[
        Flag::bool(
            "all",
            Some(b'a'),
            "Remove all unused volumes, not just anonymous ones",
        ),
        Flag::many(
            "filter",
            None,
            "filter",
            "Provide filter values (e.g. \"label=<label>\")",
        ),
        Flag::bool("force", Some(b'f'), "Do not prompt for confirmation"),
        HELP,
    ],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards system prune` (docker/cli cli/command/system/prune.go).
pub static SYSTEM_PRUNE: Command = Command {
    usage: "[OPTIONS]",
    about: "Remove unused data",
    aliases: "",
    args: Args::None,
    flags: &[
        Flag::bool(
            "all",
            Some(b'a'),
            "Remove all unused images not just dangling ones",
        ),
        Flag::many(
            "filter",
            None,
            "filter",
            "Provide filter values (e.g. \"label=<key>=<value>\")",
        ),
        Flag::bool("force", Some(b'f'), "Do not prompt for confirmation"),
        HELP,
        Flag::bool("volumes", None, "Prune anonymous volumes"),
    ],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards stats` (docker/cli cli/command/container/stats.go).
pub static STATS: Command = Command {
    usage: "[OPTIONS] [CONTAINER...]",
    about: "Display a live stream of container(s) resource usage statistics",
    aliases: "shards container stats, shards stats",
    args: Args::Any,
    flags: &[
        Flag::bool(
            "all",
            Some(b'a'),
            "Show all containers (default shows just running)",
        ),
        Flag::string(
            "format",
            None,
            "",
            "Format output using a custom template:\n'table':            Print output in table format with column headers (default)\n'table TEMPLATE':   Print output in table format using the given Go template\n'json':             Print in JSON format\n'TEMPLATE':         Print output using the given Go template.\nRefer to https://docs.docker.com/go/formatting/ for more information about formatting output with templates",
        ),
        HELP,
        Flag::bool(
            "no-stream",
            None,
            "Disable streaming stats and only pull the first result",
        ),
        Flag::bool("no-trunc", None, "Do not truncate output"),
    ],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards save`.
pub static SAVE: Command = Command {
    usage: "[OPTIONS] IMAGE [IMAGE...]",
    about: "Save one or more images to a tar archive (streamed to STDOUT by default)",
    aliases: "shards image save, shards save",
    args: Args::AtLeast(1),
    flags: &[
        HELP,
        Flag::string("output", Some(b'o'), "", "Write to a file, instead of STDOUT"),
    ],
    unserved: "\
platform - l - -",
    interspersed: true,
    error_prefix: "",
};

/// `shards load`.
pub static LOAD: Command = Command {
    usage: "[OPTIONS]",
    about: "Load an image from a tar archive or STDIN",
    aliases: "shards image load, shards load",
    args: Args::None,
    flags: &[
        HELP,
        Flag::string(
            "input",
            Some(b'i'),
            "",
            "Read from tar archive file, instead of STDIN",
        ),
        Flag::bool("quiet", Some(b'q'), "Suppress the load output"),
    ],
    unserved: "\
platform - l - -",
    interspersed: true,
    error_prefix: "",
};

/// `shards pull`. Beyond docker/cli's flags, `--output-agentfile` writes the image's
/// Agentfile (docs/architecture/AGENTFILE_ARCH.md §10).
pub static PULL: Command = Command {
    usage: "[OPTIONS] NAME[:TAG|@DIGEST]",
    about: "Download an image from a registry",
    aliases: "shards image pull, shards pull",
    args: Args::Exactly(1),
    flags: &[
        Flag::bool(
            "all-tags",
            Some(b'a'),
            "Download all tagged images in the repository",
        ),
        Flag::bool(
            "disable-content-trust",
            None,
            "Skip image verification (deprecated)",
        )
        .defaulting("true")
        .deprecated("support for docker content trust was removed"),
        HELP,
        Flag::bool(
            "no-cache",
            None,
            "Fetch every layer again, though stored, and build its microVM again",
        )
        .extension(),
        Flag::string(
            "output-agentfile",
            None,
            "",
            "Write the image's Agentfile to this file, or into this directory",
        )
        .extension(),
        Flag::string(
            "platform",
            None,
            "",
            "Set platform if server is multi-platform capable",
        ),
        Flag::bool("quiet", Some(b'q'), "Suppress verbose output"),
    ],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards push`.
pub static PUSH: Command = Command {
    usage: "[OPTIONS] NAME[:TAG]",
    about: "Upload an image to a registry",
    aliases: "shards image push, shards push",
    args: Args::Exactly(1),
    flags: &[
        Flag::bool(
            "all-tags",
            Some(b'a'),
            "Push all tags of an image to the repository",
        ),
        Flag::bool(
            "disable-content-trust",
            None,
            "Skip image verification (deprecated)",
        )
        .defaulting("true")
        .deprecated("support for docker content trust was removed"),
        HELP,
        Flag::bool("quiet", Some(b'q'), "Suppress verbose output"),
    ],
    unserved: "\
platform - s - -",
    interspersed: true,
    error_prefix: "",
};

/// `shards tag`.
pub static TAG: Command = Command {
    usage: "SOURCE_IMAGE[:TAG] TARGET_IMAGE[:TAG]",
    about: "Create a tag TARGET_IMAGE that refers to SOURCE_IMAGE",
    aliases: "shards image tag, shards tag",
    args: Args::Exactly(2),
    flags: &[HELP],
    unserved: "",
    interspersed: false,
    error_prefix: "",
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
    error_prefix: "",
};

/// `shards pause` (docker/cli cli/command/container/pause.go).
pub static PAUSE: Command = Command {
    usage: "CONTAINER [CONTAINER...]",
    about: "Pause all processes within one or more containers",
    aliases: "shards container pause, shards pause",
    args: Args::AtLeast(1),
    flags: &[HELP],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards unpause` (docker/cli cli/command/container/unpause.go).
pub static UNPAUSE: Command = Command {
    usage: "CONTAINER [CONTAINER...]",
    about: "Unpause all processes within one or more containers",
    aliases: "shards container unpause, shards unpause",
    args: Args::AtLeast(1),
    flags: &[HELP],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards top` (docker/cli cli/command/container/top.go): ps's options follow the
/// container, as they are.
pub static TOP: Command = Command {
    usage: "CONTAINER [ps OPTIONS]",
    about: "Display the running processes of a container",
    aliases: "shards container top, shards top",
    args: Args::AtLeast(1),
    flags: &[HELP],
    unserved: "",
    interspersed: false,
    error_prefix: "",
};

/// `shards diff` (docker/cli cli/command/container/diff.go).
pub static DIFF: Command = Command {
    usage: "CONTAINER",
    about: "Inspect changes to files or directories on a container's filesystem",
    aliases: "shards container diff, shards diff",
    args: Args::Exactly(1),
    flags: &[HELP],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards events` (docker/cli cli/command/system/events.go).
pub static EVENTS: Command = Command {
    usage: "[OPTIONS]",
    about: "Get real time events from the server",
    aliases: "shards system events, shards events",
    args: Args::None,
    flags: &[
        Flag::many(
            "filter",
            Some(b'f'),
            "filter",
            "Filter output based on conditions provided",
        ),
        Flag::string(
            "format",
            None,
            "",
            "Format output using a custom template:\n'json':             Print in JSON format\n'TEMPLATE':         Print output using the given Go template.\nRefer to https://docs.docker.com/go/formatting/ for more information about formatting output with templates",
        ),
        HELP,
        Flag::string("since", None, "", "Show all events created since timestamp"),
        Flag::string("until", None, "", "Stream events until this timestamp"),
    ],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards info` (docker/cli cli/command/system/info.go).
pub static INFO: Command = Command {
    usage: "[OPTIONS]",
    about: "Display system-wide information",
    aliases: "shards system info, shards info",
    args: Args::None,
    flags: &[Flag::string("format", Some(b'f'), "", INSPECT_FORMAT_HELP), HELP],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards export` (docker/cli cli/command/container/export.go).
pub static EXPORT: Command = Command {
    usage: "[OPTIONS] CONTAINER",
    about: "Export a container's filesystem as a tar archive",
    aliases: "shards container export, shards export",
    args: Args::Exactly(1),
    flags: &[
        HELP,
        Flag::string("output", Some(b'o'), "", "Write to a file, instead of STDOUT"),
    ],
    unserved: "",
    interspersed: true,
    error_prefix: "",
};

/// `shards port`.
pub static PORT: Command = Command {
    usage: "CONTAINER [PRIVATE_PORT[/PROTO]]",
    about: "List port mappings or a specific mapping for the container",
    aliases: "shards container port, shards port",
    args: Args::Range(1, 2),
    flags: &[HELP],
    unserved: "",
    interspersed: true,
    error_prefix: "",
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
    error_prefix: "",
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
        Flag::bool(
            "volumes",
            Some(b'v'),
            "Remove anonymous volumes associated with the container",
        ),
    ],
    unserved: "\
link l b false -",
    interspersed: true,
    error_prefix: "",
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
    error_prefix: "",
};

/// `shards exec`, and `shards container exec`.
pub static EXEC: Command = Command {
    usage: "[OPTIONS] CONTAINER COMMAND [ARG...]",
    about: "Execute a command in a running container",
    aliases: "shards container exec, shards exec",
    args: Args::AtLeast(2),
    flags: &[
        Flag::bool(
            "detach",
            Some(b'd'),
            "Detached mode: run command in the background",
        ),
        Flag::string(
            "detach-keys",
            None,
            "",
            "Override the key sequence for detaching a container",
        ),
        Flag::many("env", Some(b'e'), "list", "Set environment variables"),
        Flag::many(
            "env-file",
            None,
            "list",
            "Read in a file of environment variables",
        ),
        HELP,
        Flag::bool("interactive", Some(b'i'), "Keep STDIN open even if not attached"),
        Flag::bool("privileged", None, "Give extended privileges to the command"),
        Flag::bool("tty", Some(b't'), "Allocate a pseudo-TTY"),
        Flag::string(
            "user",
            Some(b'u'),
            "",
            "Username or UID (format: \"<name|uid>[:<group|gid>]\")",
        ),
        Flag::string(
            "workdir",
            Some(b'w'),
            "",
            "Working directory inside the container",
        ),
    ],
    unserved: "",
    interspersed: false,
    error_prefix: "",
};

/// `shards attach`.
pub static ATTACH: Command = Command {
    usage: "[OPTIONS] CONTAINER",
    about: "Attach local standard input, output, and error streams to a running container",
    aliases: "shards container attach, shards attach",
    args: Args::Exactly(1),
    flags: &[
        Flag::string(
            "detach-keys",
            None,
            "",
            "Override the key sequence for detaching a container",
        ),
        HELP,
        Flag::bool("no-stdin", None, "Do not attach STDIN"),
        Flag::bool("sig-proxy", None, "Proxy all received signals to the process").defaulting("true"),
    ],
    unserved: "",
    interspersed: true,
    error_prefix: "",
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
    error_prefix: "",
};

/// `shards build`: buildx v0.37.1's `build` (commands/build.go), which `docker build` runs
/// as the CLI's plugin. Its words name `shards buildx build`, as buildx's name `docker
/// buildx build`. The flags its root adds (`--builder`, `--debug`) are its too.
pub static BUILD: Command = Command {
    usage: "[OPTIONS] PATH | URL | -",
    about: "Start a build",
    aliases: "shards build, shards builder build, shards image build, shards buildx b",
    args: Args::Exactly(1),
    flags: &[
        Flag::many(
            "allow",
            None,
            "stringArray",
            "Allow extra privileged entitlement (e.g., \"network.host\", \"security.insecure\", \"device\", \"buildx.local.delete\")",
        ),
        Flag::many("build-arg", None, "stringArray", "Set build-time variables"),
        Flag::string(
            "file",
            Some(b'f'),
            "",
            "Name of the Dockerfile (default: \"PATH/Dockerfile\")",
        ),
        HELP,
        Flag::string("iidfile", None, "", "Write the image ID to a file"),
        Flag::many("label", None, "stringArray", "Set metadata for an image"),
        Flag::bool("load", None, "Shorthand for \"--output=type=docker\""),
        Flag::bool("no-cache", None, "Do not use cache when building the image"),
        Flag::many("platform", None, "stringArray", "Set target platform for build"),
        Flag::string(
            "progress",
            None,
            "auto",
            "Set type of progress output (\"auto\", \"none\",  \"plain\", \"quiet\", \"rawjson\", \"tty\"). Use plain to show container output",
        ),
        Flag::bool("pull", None, "Always attempt to pull all referenced images"),
        Flag::bool(
            "quiet",
            Some(b'q'),
            "Suppress the build output and print image ID on success",
        ),
        Flag::many(
            "tag",
            Some(b't'),
            "stringArray",
            "Image identifier (format: \"[registry/]repository[:tag]\")",
        ),
        Flag::many(
            "secret",
            None,
            "stringArray",
            "Secret to expose to the build (format: \"id=mysecret[,src=/local/secret]\")",
        ),
        Flag::string("target", None, "", "Set the target build stage to build"),
        Flag::many("ulimit", None, "ulimit", "Ulimit options").defaulting("[]"),
    ],
    unserved: "\
add-host - m - -\n\
annotation - m - -\n\
attest - m - -\n\
build-context - m - -\n\
builder - s - -\n\
cache-from - m - -\n\
cache-to - m - -\n\
call - s build -\n\
cgroup-parent - s - -\n\
check - b - -\n\
compress - b false -\n\
cpu-period - s - -\n\
cpu-quota - s - -\n\
cpu-shares c s - -\n\
cpuset-mems - s - -\n\
debug D b false -\n\
force-rm - b false -\n\
isolation - s - -\n\
memory m s - -\n\
memory-swap - s - -\n\
metadata-file - s - -\n\
network - s default -\n\
no-cache-filter - m - -\n\
output o m - -\n\
policy - m - -\n\
print - s - -\n\
provenance - s - -\n\
push - b false -\n\
resource - m - -\n\
rm - b true -\n\
sbom - s - -\n\
security-opt - m - -\n\
shm-size - s 0 -\n\
squash - b false -\n\
ssh - m - -",
    interspersed: true,
    error_prefix: "ERROR: ",
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
        ["attach", ..] => (&ATTACH, "shards attach", 1),
        ["container", "attach", ..] => (&ATTACH, "shards container attach", 2),
        ["exec", ..] => (&EXEC, "shards exec", 1),
        ["container", "exec", ..] => (&EXEC, "shards container exec", 2),
        ["container", "kill", ..] => (&KILL, "shards container kill", 2),
        ["port", ..] => (&PORT, "shards port", 1),
        ["images", ..] => (&IMAGES, "shards images", 1),
        ["tag", ..] => (&TAG, "shards tag", 1),
        ["save", ..] => (&SAVE, "shards save", 1),
        ["load", ..] => (&LOAD, "shards load", 1),
        ["pull", ..] => (&PULL, "shards pull", 1),
        ["image", "pull", ..] => (&PULL, "shards image pull", 2),
        ["push", ..] => (&PUSH, "shards push", 1),
        ["image", "push", ..] => (&PUSH, "shards image push", 2),
        ["image", "load", ..] => (&LOAD, "shards image load", 2),
        ["image", "save", ..] => (&SAVE, "shards image save", 2),
        ["rmi", ..] => (&RMI, "shards rmi", 1),
        ["image", "rm" | "remove", ..] => (&RMI, "shards image rm", 2),
        ["image", "tag", ..] => (&TAG, "shards image tag", 2),
        ["image", "inspect", ..] => (&IMAGE_INSPECT, "shards image inspect", 2),
        ["history", ..] => (&HISTORY, "shards history", 1),
        ["system", "df", ..] => (&SYSTEM_DF, "shards system df", 2),
        ["stats", ..] => (&STATS, "shards stats", 1),
        ["pause", ..] => (&PAUSE, "shards pause", 1),
        ["top", ..] => (&TOP, "shards top", 1),
        ["diff", ..] => (&DIFF, "shards diff", 1),
        ["events", ..] => (&EVENTS, "shards events", 1),
        ["start", ..] => (&START, "shards start", 1),
        ["commit", ..] => (&COMMIT, "shards commit", 1),
        ["cp", ..] => (&COPY, "shards cp", 1),
        ["inspect", ..] => (&INSPECT, "shards inspect", 1),
        ["version", ..] => (&VERSION, "shards version", 1),
        ["container", "cp", ..] => (&COPY, "shards container cp", 2),
        ["container", "commit", ..] => (&COMMIT, "shards container commit", 2),
        ["container", "start", ..] => (&START, "shards container start", 2),
        ["restart", ..] => (&RESTART, "shards restart", 1),
        ["container", "restart", ..] => (&RESTART, "shards container restart", 2),
        ["create", ..] => (&CREATE, "shards create", 1),
        ["container", "create", ..] => (&CREATE, "shards container create", 2),
        ["info", ..] => (&INFO, "shards info", 1),
        ["export", ..] => (&EXPORT, "shards export", 1),
        ["container", "export", ..] => (&EXPORT, "shards container export", 2),
        ["system", "info", ..] => (&INFO, "shards system info", 2),
        ["system", "events", ..] => (&EVENTS, "shards system events", 2),
        ["container", "diff", ..] => (&DIFF, "shards container diff", 2),
        ["container", "top", ..] => (&TOP, "shards container top", 2),
        ["container", "pause", ..] => (&PAUSE, "shards container pause", 2),
        ["unpause", ..] => (&UNPAUSE, "shards unpause", 1),
        ["container", "unpause", ..] => (&UNPAUSE, "shards container unpause", 2),
        ["container", "stats", ..] => (&STATS, "shards container stats", 2),
        ["system", "prune", ..] => (&SYSTEM_PRUNE, "shards system prune", 2),
        ["container", "prune", ..] => (&CONTAINER_PRUNE, "shards container prune", 2),
        ["image", "prune", ..] => (&IMAGE_PRUNE, "shards image prune", 2),
        ["rename", ..] => (&RENAME, "shards rename", 1),
        ["container", "rename", ..] => (&RENAME, "shards container rename", 2),
        ["image", "history", ..] => (&HISTORY, "shards image history", 2),
        ["container", "inspect", ..] => (&CONTAINER_INSPECT, "shards container inspect", 2),
        ["image", "ls" | "list", ..] => (&IMAGES, "shards image ls", 2),
        ["container", "port", ..] => (&PORT, "shards container port", 2),
        ["update", ..] => (&UPDATE, "shards update", 1),
        ["import", ..] => (&IMPORT, "shards import", 1),
        ["image", "import", ..] => (&IMPORT, "shards image import", 2),
        ["container", "update", ..] => (&UPDATE, "shards container update", 2),
        ["volume", "create", ..] => (&VOLUME_CREATE, "shards volume create", 2),
        ["volume", "ls" | "list", ..] => (&VOLUME_LS, "shards volume ls", 2),
        ["volume", "inspect", ..] => (&VOLUME_INSPECT, "shards volume inspect", 2),
        ["volume", "rm" | "remove", ..] => (&VOLUME_RM, "shards volume rm", 2),
        ["volume", "prune", ..] => (&VOLUME_PRUNE, "shards volume prune", 2),
        _ => return None,
    })
}

/// `build` if the start of `words` names it, and how many words do: `build`, `builder
/// build`, `image build`, `buildx build` or `buildx b`. Its path is always `shards buildx
/// build`, as buildx's is `docker buildx build` however the CLI was asked.
pub fn build(words: &[&str]) -> Option<usize> {
    match words {
        ["build", ..] => Some(1),
        ["builder" | "image", "build", ..] | ["buildx", "build" | "b", ..] => Some(2),
        _ => None,
    }
}
