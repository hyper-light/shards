//! A microVM as `shards inspect` shows it: dockerd's container.InspectResponse (moby
//! api/types/container/container.go, at docker-v29.8.1), as Go types, so that a
//! `--format` reads it as the Docker CLI reads dockerd's (typed first, then the raw JSON:
//! docker/cli cli/command/inspect/inspector.go), and its JSON is dockerd's field for
//! field. What it was made with comes from its request and its image's config, merged as
//! dockerd merges them (daemon/commit.go, merge); what it is now, from its record and
//! its VM. Held to what Docker Engine makes of the same command lines by
//! testdata/inspect.json (scripts/inspect/generate).

use std::net::Ipv4Addr;

use shards_ipc::Run;
use shards_template::{Kind, Struct, Value};

use crate::containers::{Container, State as Life};

/// What dockerd masks and makes read-only in a container that is not privileged
/// (daemon/pkg/oci/defaults.go), in its order: what shards-init does too (init
/// defaults.rs).
const MASKED: [&str; 12] = [
    "/proc/acpi",
    "/proc/asound",
    "/proc/interrupts",
    "/proc/kcore",
    "/proc/keys",
    "/proc/latency_stats",
    "/proc/sched_debug",
    "/proc/scsi",
    "/proc/timer_list",
    "/proc/timer_stats",
    "/sys/devices/virtual/powercap",
    "/sys/firmware",
];
const READONLY: [&str; 5] = [
    "/proc/bus",
    "/proc/fs",
    "/proc/irq",
    "/proc/sys",
    "/proc/sysrq-trigger",
];

/// dockerd's default /dev/shm (daemon/config, DefaultShmSize).
const SHM_SIZE: i64 = 64 << 20;

/// A health check's state, as `State.Health` holds it.
pub(super) struct Health {
    pub status: &'static str,
    pub failing_streak: u64,
    /// Each probe: its start and end in nanoseconds since 1970, its exit code, its output.
    pub log: Vec<(u128, u128, i64, String)>,
}

/// A running microVM's place on the default bridge.
pub(super) struct Net {
    pub ip: Ipv4Addr,
    pub gateway: Ipv4Addr,
    pub prefix: u8,
    pub mac: Option<[u8; 6]>,
}

/// What a microVM's document is made of.
pub(super) struct Facts<'a> {
    pub container: &'a Container,
    /// The request it was made by; one with nothing set where none was kept.
    pub request: &'a Run,
    /// Its image's config (the image config's `config`), where the image is here.
    pub image: Option<&'a serde_json::Value>,
    /// The descriptor of its image's manifest for this platform, as Go encodes one.
    pub manifest: Option<&'a serde_json::Value>,
    pub paused: bool,
    /// Its VM's process, while it runs.
    pub pid: Option<u32>,
    pub health: Option<Health>,
    /// Its execs under way.
    pub exec_ids: Vec<String>,
    pub log_path: String,
    /// Its place on the bridge, while it runs there.
    pub net: Option<Net>,
}

fn s(v: &str) -> Value {
    Value::String(v.to_owned())
}

/// A `[]string`, nil where `nil` says.
fn strs(list: &[String], nil: bool) -> Value {
    if nil {
        Value::NilList(Kind::String)
    } else {
        Value::strings(list.iter().cloned())
    }
}

/// A non-nil empty slice of structs (`[]*blkiodev.WeightDevice{}` and the like).
fn empty() -> Value {
    Value::List(Kind::Any, Vec::new())
}

fn int(n: i64) -> Value {
    Value::Int(n)
}

/// A time as dockerd keeps it in a container's state: `time.Time`'s text, in UTC.
pub(super) fn go_time(ns: Option<u128>) -> String {
    let Some(ns) = ns else {
        return "0001-01-01T00:00:00Z".to_owned();
    };
    let secs = i64::try_from(ns / 1_000_000_000).unwrap_or(i64::MAX);
    let time = shards_dockerfile::go::Time {
        nanosecond: u32::try_from(ns % 1_000_000_000).unwrap_or(0),
        ..shards_dockerfile::go::Time::from_unix(secs)
    };
    time.rfc3339_nano().unwrap_or_default()
}

/// Its image config's field `name`, as a list of strings, where it has one.
fn image_strings(image: Option<&serde_json::Value>, name: &str) -> Option<Vec<String>> {
    image?
        .get(name)?
        .as_array()
        .map(|l| l.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
}

fn image_string(image: Option<&serde_json::Value>, name: &str) -> String {
    image
        .and_then(|i| i.get(name))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// A JSON value as Go decodes it into `any` with UseNumber, as a template value.
fn any(v: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    match v {
        J::Null => Value::Nil,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .or_else(|| n.as_u64().map(Value::Uint))
            .unwrap_or_else(|| Value::Float(n.as_f64().unwrap_or(0.0))),
        J::String(t) => s(t),
        J::Array(l) => Value::list(l.iter().map(any).collect()),
        J::Object(m) => Value::map(m.iter().map(|(k, v)| (k.clone(), any(v))).collect()),
    }
}

/// A `map[string]struct{}` (ExposedPorts, Volumes), its keys `keys`.
fn set(keys: impl IntoIterator<Item = String>) -> Value {
    Value::Map(
        Kind::Any,
        keys.into_iter()
            .map(|k| (k, Struct::new("struct {}").value()))
            .collect(),
    )
}

/// The microVM's InspectResponse.
pub(super) fn document(f: &Facts<'_>) -> Value {
    let c = f.container;
    let run = f.request;
    let config = config(f);
    // What runs: Config's entrypoint, then its command (daemon/create.go, the container's
    // Path and Args).
    let (path, args) = {
        let words = command_words(&config.0, &config.1);
        match words.split_first() {
            Some((p, a)) => (p.clone(), a.to_vec()),
            None => (String::new(), Vec::new()),
        }
    };
    let image_id = c.image_id.clone().unwrap_or_default();
    Struct::new("container.InspectResponse")
        .tagged("ID", Some("Id"), false, s(&c.id))
        .field("Created", s(&go_time(Some(c.created))))
        .field("Path", s(&path))
        .field("Args", strs(&args, false))
        .field("State", state(f))
        .field("Image", s(&image_id))
        // Its files are its guest's: none is the host's to name.
        .field("ResolvConfPath", s(""))
        .field("HostnamePath", s(""))
        .field("HostsPath", s(""))
        .field("LogPath", s(&f.log_path))
        .field("Name", s(&format!("/{}", c.name)))
        .field("RestartCount", int(0))
        // As `info` names them: each image a microVM's read-only EROFS disk.
        .field("Driver", s("erofs"))
        .field("Platform", s("linux"))
        .field("MountLabel", s(""))
        .field("ProcessLabel", s(""))
        .field("AppArmorProfile", s(""))
        .field("ExecIDs", strs(&f.exec_ids, f.exec_ids.is_empty()))
        .field("HostConfig", host_config(f))
        .tagged(
            "GraphDriver",
            Some("GraphDriver"),
            true,
            Struct::nil("storage.DriverData"),
        )
        .tagged(
            "Storage",
            Some("Storage"),
            true,
            Struct::pointer("storage.Storage")
                .tagged(
                    "RootFS",
                    Some("RootFS"),
                    true,
                    Struct::pointer("storage.RootFSStorage")
                        .tagged(
                            "Snapshot",
                            Some("Snapshot"),
                            true,
                            Struct::pointer("storage.RootFSStorageSnapshot")
                                .tagged("Name", Some("Name"), true, s("erofs"))
                                .value(),
                        )
                        .value(),
                )
                .value(),
        )
        .tagged("SizeRw", Some("SizeRw"), true, Struct::nil("int64"))
        .tagged("SizeRootFs", Some("SizeRootFs"), true, Struct::nil("int64"))
        .field("Mounts", empty())
        .field("Config", config.2)
        .field("NetworkSettings", network_settings(f, run))
        .tagged(
            "ImageManifestDescriptor",
            Some("ImageManifestDescriptor"),
            true,
            f.manifest
                .map_or_else(|| Struct::nil("v1.Descriptor"), descriptor),
        )
        .value()
}

/// Entrypoint then command, as dockerd runs them.
fn command_words(entrypoint: &Option<Vec<String>>, cmd: &Option<Vec<String>>) -> Vec<String> {
    entrypoint
        .iter()
        .flatten()
        .chain(cmd.iter().flatten())
        .cloned()
        .collect()
}

/// State, now.
fn state(f: &Facts<'_>) -> Value {
    let c = f.container;
    let running = c.state == Life::Running;
    let status = match c.state {
        Life::Running if f.paused => "paused",
        Life::Running => "running",
        Life::Created => "created",
        Life::Exited => "exited",
    };
    let health = match &f.health {
        Some(h) => Struct::pointer("container.Health")
            .field("Status", s(h.status))
            .field(
                "FailingStreak",
                int(i64::try_from(h.failing_streak).unwrap_or(i64::MAX)),
            )
            .field(
                "Log",
                Value::list(
                    h.log
                        .iter()
                        .map(|(start, end, code, output)| {
                            Struct::pointer("container.HealthcheckResult")
                                .field("Start", s(&go_time(Some(*start))))
                                .field("End", s(&go_time(Some(*end))))
                                .field("ExitCode", int(*code))
                                .field("Output", s(output))
                                .value()
                        })
                        .collect(),
                ),
            )
            .value(),
        None => Struct::nil("container.Health"),
    };
    Struct::pointer("container.State")
        .field("Status", s(status))
        .field("Running", Value::Bool(running))
        .field("Paused", Value::Bool(f.paused))
        .field("Restarting", Value::Bool(false))
        .field("OOMKilled", Value::Bool(f.container.oom_killed))
        .field("Dead", Value::Bool(false))
        .field("Pid", int(f.pid.filter(|_| running).map_or(0, i64::from)))
        .field("ExitCode", int(c.exit_code.map_or(0, i64::from)))
        .field("Error", s(""))
        .field("StartedAt", s(&go_time(c.started)))
        .field("FinishedAt", s(&go_time(c.finished)))
        .tagged("Health", Some("Health"), true, health)
        .value()
}

/// Config: the request merged with its image's config (daemon/commit.go, merge, and
/// docker/cli's parse of the command line), with its entrypoint and command apart.
fn config(f: &Facts<'_>) -> (Option<Vec<String>>, Option<Vec<String>>, Value) {
    let (run, image, c) = (f.request, f.image, f.container);
    let user_or = |given: &str, name: &str| {
        if given.is_empty() {
            image_string(image, name)
        } else {
            given.to_owned()
        }
    };
    // Env: the request's, then the image's whose names it does not set.
    let mut env: Vec<String> = run.env.clone();
    if let Some(image_env) = image_strings(image, "Env") {
        if env.is_empty() {
            env = image_env;
        } else {
            for e in image_env {
                let key = e.split_once('=').map_or(e.as_str(), |(k, _)| k);
                if !env
                    .iter()
                    .any(|u| u.split_once('=').map_or(u.as_str(), |(k, _)| k) == key)
                {
                    env.push(e);
                }
            }
        }
    }
    // An entrypoint given as "" is one empty word, which keeps the image's out and
    // dockerd then drops; none given takes the image's, and its command if none is given.
    let user_cmd = (!run.cmd.is_empty()).then(|| run.cmd.clone());
    let (entrypoint, cmd) = match &run.entrypoint {
        Some(e) if e.is_empty() => (None, user_cmd),
        Some(e) => (Some(e.clone()), user_cmd),
        None => (
            image_strings(image, "Entrypoint"),
            user_cmd.or_else(|| image_strings(image, "Cmd")),
        ),
    };
    // ExposedPorts: what -p publishes and --expose exposes, then the image's.
    let mut exposed: Vec<String> = run
        .publish
        .iter()
        .map(|p| format!("{}/{}", p.port, p.proto))
        .collect();
    exposed.extend(run.expose.iter().cloned());
    if let Some(ports) = image
        .and_then(|i| i.get("ExposedPorts"))
        .and_then(serde_json::Value::as_object)
    {
        exposed.extend(ports.keys().cloned());
    }
    exposed.sort();
    exposed.dedup();
    // Labels: the run's, over the image's (daemon/commit.go, merge).
    let mut labels: std::collections::BTreeMap<String, String> = image
        .and_then(|i| i.get("Labels"))
        .and_then(serde_json::Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                .collect()
        })
        .unwrap_or_default();
    for l in &run.labels {
        let (k, v) = l.split_once('=').unwrap_or((l, ""));
        labels.insert(k.to_owned(), v.to_owned());
    }
    let volumes = image
        .and_then(|i| i.get("Volumes"))
        .and_then(serde_json::Value::as_object)
        .filter(|v| !v.is_empty())
        .map(|v| set(v.keys().cloned()));
    let health = healthcheck(run, image);
    let stop_signal = run
        .stop_signal
        .clone()
        .unwrap_or_else(|| image_string(image, "StopSignal"));
    let attach = !run.detach;
    let hostname = run
        .hostname
        .clone()
        .unwrap_or_else(|| c.id.get(..12).unwrap_or(&c.id).to_owned());
    let value = Struct::pointer("container.Config")
        .field("Hostname", s(&hostname))
        .field("Domainname", s(&run.domainname))
        .field("User", s(&user_or(&run.user, "User")))
        .field("AttachStdin", Value::Bool(attach && run.interactive))
        .field("AttachStdout", Value::Bool(attach))
        .field("AttachStderr", Value::Bool(attach))
        .tagged("ExposedPorts", Some("ExposedPorts"), true, set(exposed))
        .field("Tty", Value::Bool(run.tty.is_some()))
        .field("OpenStdin", Value::Bool(run.interactive))
        .field("StdinOnce", Value::Bool(attach && run.interactive))
        .field("Env", strs(&env, false))
        .field(
            "Cmd",
            cmd.as_ref()
                .map_or(Value::NilList(Kind::String), |c| strs(c, false)),
        )
        .tagged("Healthcheck", Some("Healthcheck"), true, health)
        .tagged("ArgsEscaped", Some("ArgsEscaped"), true, Value::Bool(false))
        .field("Image", s(&run.image))
        .field("Volumes", volumes.unwrap_or(Value::NilMap(Kind::Any)))
        .field("WorkingDir", s(&user_or(&run.workdir, "WorkingDir")))
        .field(
            "Entrypoint",
            entrypoint
                .as_ref()
                .map_or(Value::NilList(Kind::String), |e| strs(e, false)),
        )
        .tagged(
            "NetworkDisabled",
            Some("NetworkDisabled"),
            true,
            Value::Bool(false),
        )
        .tagged("OnBuild", Some("OnBuild"), true, Value::NilList(Kind::String))
        .field("Labels", Value::string_map(labels))
        .tagged("StopSignal", Some("StopSignal"), true, s(&stop_signal))
        .tagged(
            "StopTimeout",
            Some("StopTimeout"),
            true,
            run.stop_timeout.map_or_else(|| Struct::nil("int"), Value::Int),
        )
        .tagged("Shell", Some("Shell"), true, Value::NilList(Kind::String))
        .value();
    (entrypoint, cmd, value)
}

/// Healthcheck: the request's, each part not set taken from the image's.
fn healthcheck(run: &Run, image: Option<&serde_json::Value>) -> Value {
    let theirs = image.and_then(|i| i.get("Healthcheck")).filter(|h| !h.is_null());
    if run.health.is_none() && theirs.is_none() {
        return Struct::nil("v1.HealthcheckConfig");
    }
    let num = |name: &str| {
        theirs
            .and_then(|h| h.get(name))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
    };
    let ours = run.health.clone().unwrap_or_default();
    let pick = |own: i64, name: &str| if own != 0 { own } else { num(name) };
    let test = if ours.test.is_empty() {
        theirs
            .and_then(|h| h.get("Test"))
            .and_then(serde_json::Value::as_array)
            .map(|l| l.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    } else {
        ours.test.clone()
    };
    Struct::pointer("v1.HealthcheckConfig")
        .tagged("Test", Some("Test"), true, strs(&test, test.is_empty()))
        .tagged(
            "Interval",
            Some("Interval"),
            true,
            int(pick(ours.interval, "Interval")),
        )
        .tagged(
            "Timeout",
            Some("Timeout"),
            true,
            int(pick(ours.timeout, "Timeout")),
        )
        .tagged(
            "StartPeriod",
            Some("StartPeriod"),
            true,
            int(pick(ours.start_period, "StartPeriod")),
        )
        .tagged(
            "StartInterval",
            Some("StartInterval"),
            true,
            int(pick(ours.start_interval, "StartInterval")),
        )
        .tagged(
            "Retries",
            Some("Retries"),
            true,
            int(pick(ours.retries, "Retries")),
        )
        .value()
}

/// A port binding (network.PortBinding).
fn binding(ip: &str, port: &str) -> Value {
    Struct::new("network.PortBinding")
        .tagged("HostIP", Some("HostIp"), false, s(ip))
        .field("HostPort", s(port))
        .value()
}

/// HostConfig: what was asked, and dockerd's defaults for what was not.
fn host_config(f: &Facts<'_>) -> Value {
    let run = f.request;
    let res = &run.resources;
    let mut bindings: std::collections::BTreeMap<String, Vec<Value>> = std::collections::BTreeMap::new();
    for p in &run.publish {
        bindings
            .entry(format!("{}/{}", p.port, p.proto))
            .or_default()
            .push(binding(&p.host_ip, &p.host_port));
    }
    let (rows, cols) = run.tty.unwrap_or((0, 0));
    let mode = match run.network.as_str() {
        "" | "default" => "bridge",
        other => other,
    };
    Struct::pointer("container.HostConfig")
        .field("Binds", Value::NilList(Kind::String))
        .field("ContainerIDFile", s(&run.cidfile))
        .field(
            "LogConfig",
            Struct::new("container.LogConfig")
                .field("Type", s("shards"))
                .field("Config", Value::string_map(Vec::<(String, String)>::new()))
                .value(),
        )
        .field("NetworkMode", s(mode))
        .field(
            "PortBindings",
            Value::Map(
                Kind::Any,
                bindings.into_iter().map(|(k, v)| (k, Value::list(v))).collect(),
            ),
        )
        .field(
            "RestartPolicy",
            Struct::new("container.RestartPolicy")
                .field("Name", s("no"))
                .field("MaximumRetryCount", int(0))
                .value(),
        )
        .field("AutoRemove", Value::Bool(f.container.auto_remove))
        .field("VolumeDriver", s(""))
        .field("VolumesFrom", Value::NilList(Kind::String))
        .field(
            "ConsoleSize",
            Value::List(
                Kind::Uint,
                vec![Value::Uint(u64::from(rows)), Value::Uint(u64::from(cols))],
            ),
        )
        .tagged(
            "Annotations",
            Some("Annotations"),
            true,
            Value::NilMap(Kind::String),
        )
        .field("CapAdd", Value::NilList(Kind::String))
        .field("CapDrop", Value::NilList(Kind::String))
        .field("CgroupnsMode", s("private"))
        // docker/cli's toNetipAddrSlice is nil for none; the rest never nil but ExtraHosts.
        .tagged("DNS", Some("Dns"), false, strs(&run.dns, run.dns.is_empty()))
        .tagged(
            "DNSOptions",
            Some("DnsOptions"),
            false,
            strs(&run.dns_options, false),
        )
        .tagged(
            "DNSSearch",
            Some("DnsSearch"),
            false,
            strs(&run.dns_search, false),
        )
        .field("ExtraHosts", strs(&run.add_hosts, run.add_hosts.is_empty()))
        .field("GroupAdd", Value::NilList(Kind::String))
        .field("IpcMode", s("private"))
        .field("Cgroup", s(""))
        .field("Links", Value::NilList(Kind::String))
        .field("OomScoreAdj", int(0))
        .field("PidMode", s(""))
        .field("Privileged", Value::Bool(false))
        .field("PublishAllPorts", Value::Bool(run.publish_all))
        .field("ReadonlyRootfs", Value::Bool(false))
        .field("SecurityOpt", Value::NilList(Kind::String))
        .tagged(
            "StorageOpt",
            Some("StorageOpt"),
            true,
            Value::NilMap(Kind::String),
        )
        .tagged("Tmpfs", Some("Tmpfs"), true, Value::NilMap(Kind::String))
        .field("UTSMode", s(""))
        .field("UsernsMode", s(""))
        .field("ShmSize", int(SHM_SIZE))
        .tagged("Sysctls", Some("Sysctls"), true, Value::NilMap(Kind::String))
        .tagged("Runtime", Some("Runtime"), true, s("shards"))
        .tagged("Umask", Some("Umask"), true, Struct::nil("uint32"))
        .field("Isolation", s(""))
        // Resources, embedded, as dockerd keeps them: swap twice the memory where only
        // memory is limited (adaptContainerSettings), a pids limit of 0 or less unset
        // (postContainersCreate), and swappiness discarded on cgroup v2.
        .tagged("CPUShares", Some("CpuShares"), false, int(res.cpu_shares))
        .field("Memory", int(res.memory))
        .tagged("NanoCPUs", Some("NanoCpus"), false, int(res.nano_cpus))
        .field("CgroupParent", s(""))
        .field("BlkioWeight", Value::Uint(0))
        .field("BlkioWeightDevice", empty())
        .field("BlkioDeviceReadBps", empty())
        .field("BlkioDeviceWriteBps", empty())
        .field("BlkioDeviceReadIOps", empty())
        .field("BlkioDeviceWriteIOps", empty())
        .tagged("CPUPeriod", Some("CpuPeriod"), false, int(res.cpu_period))
        .tagged("CPUQuota", Some("CpuQuota"), false, int(res.cpu_quota))
        .tagged("CPURealtimePeriod", Some("CpuRealtimePeriod"), false, int(0))
        .tagged("CPURealtimeRuntime", Some("CpuRealtimeRuntime"), false, int(0))
        .field("CpusetCpus", s(&res.cpuset_cpus))
        .field("CpusetMems", s(&res.cpuset_mems))
        .field("Devices", empty())
        .field("DeviceCgroupRules", Value::NilList(Kind::String))
        .field("DeviceRequests", Value::NilList(Kind::Any))
        .field("MemoryReservation", int(res.memory_reservation))
        .field("MemorySwap", int(crate::resources::memory_swap(res)))
        .field("MemorySwappiness", Struct::nil("int64"))
        // Set false as it is made, and dropped as it starts where the kernel cannot
        // keep the OOM killer away (daemon/daemon_unix.go): cgroup v2's.
        .field(
            "OomKillDisable",
            if f.container.started.is_some() {
                Struct::nil("bool")
            } else {
                Value::Bool(false)
            },
        )
        .field(
            "PidsLimit",
            if res.pids_limit > 0 {
                int(res.pids_limit)
            } else {
                Struct::nil("int64")
            },
        )
        .field("Ulimits", empty())
        .tagged("CPUCount", Some("CpuCount"), false, int(0))
        .tagged("CPUPercent", Some("CpuPercent"), false, int(0))
        .field("IOMaximumIOps", Value::Uint(0))
        .field("IOMaximumBandwidth", Value::Uint(0))
        .tagged("Mounts", Some("Mounts"), true, Value::NilList(Kind::Any))
        .field("MaskedPaths", Value::strings(MASKED))
        .field("ReadonlyPaths", Value::strings(READONLY))
        .tagged("Init", Some("Init"), true, Struct::nil("bool"))
        .value()
}

/// NetworkSettings: its ports while it runs, and its endpoint on its network.
fn network_settings(f: &Facts<'_>, run: &Run) -> Value {
    let c = f.container;
    let running = c.state == Life::Running;
    let mut ports: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
    if running {
        for p in &c.ports {
            let key = format!("{}/{}", p.private, p.proto);
            let entry = ports.entry(key).or_insert(Value::NilList(Kind::Any));
            if let Some(ip) = p.ip {
                let b = binding(&ip.to_string(), &p.public.to_string());
                match entry {
                    Value::List(_, l) => l.push(b),
                    other => *other = Value::list(vec![b]),
                }
            }
        }
    }
    let mode = match run.network.as_str() {
        "" | "default" => "bridge",
        other => other,
    };
    let net = f.net.as_ref().filter(|_| running && mode == "bridge");
    let addr = |a: Option<Ipv4Addr>| a.map(|a| a.to_string()).unwrap_or_default();
    let endpoint = Struct::pointer("network.EndpointSettings")
        .field("IPAMConfig", Struct::nil("network.EndpointIPAMConfig"))
        .field("Links", Value::NilList(Kind::String))
        .field("Aliases", Value::NilList(Kind::String))
        .field("DriverOpts", Value::NilMap(Kind::String))
        .field("GwPriority", int(0))
        .field("NetworkID", s(""))
        .field("EndpointID", s(""))
        .field("Gateway", s(&addr(net.map(|n| n.gateway))))
        .field("IPAddress", s(&addr(net.map(|n| n.ip))))
        .field(
            "MacAddress",
            s(&net
                .and_then(|n| n.mac)
                .map(|m| shards_net::Mac(m).to_string())
                .unwrap_or_default()),
        )
        .field("IPPrefixLen", int(net.map_or(0, |n| i64::from(n.prefix))))
        .field("IPv6Gateway", s(""))
        .field("GlobalIPv6Address", s(""))
        .field("GlobalIPv6PrefixLen", int(0))
        .field("DNSNames", Value::NilList(Kind::String))
        .value();
    Struct::pointer("container.NetworkSettings")
        .field("SandboxID", s(""))
        .field("SandboxKey", s(""))
        .field("Ports", Value::Map(Kind::Any, ports))
        .field(
            "Networks",
            Value::Map(Kind::Any, [(mode.to_owned(), endpoint)].into_iter().collect()),
        )
        .value()
}

/// An OCI descriptor (v1.Descriptor) from its JSON.
fn descriptor(d: &serde_json::Value) -> Value {
    let get = |k: &str| d.get(k);
    let text = |k: &str| {
        get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let platform = match get("platform") {
        Some(p) => {
            let pt = |k: &str| {
                p.get(k)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            Struct::pointer("v1.Platform")
                .tagged(
                    "Architecture",
                    Some("architecture"),
                    false,
                    s(&pt("architecture")),
                )
                .tagged("OS", Some("os"), false, s(&pt("os")))
                .tagged("OSVersion", Some("os.version"), true, s(&pt("os.version")))
                .tagged(
                    "OSFeatures",
                    Some("os.features"),
                    true,
                    p.get("os.features").map_or(Value::NilList(Kind::String), any),
                )
                .tagged("Variant", Some("variant"), true, s(&pt("variant")))
                .value()
        }
        None => Struct::nil("v1.Platform"),
    };
    let annotations = match get("annotations").and_then(serde_json::Value::as_object) {
        Some(m) => Value::string_map(
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned())),
        ),
        None => Value::NilMap(Kind::String),
    };
    Struct::pointer("v1.Descriptor")
        .tagged("MediaType", Some("mediaType"), false, s(&text("mediaType")))
        .tagged("Digest", Some("digest"), false, s(&text("digest")))
        .tagged(
            "Size",
            Some("size"),
            false,
            int(get("size").and_then(serde_json::Value::as_i64).unwrap_or(0)),
        )
        .tagged(
            "URLs",
            Some("urls"),
            true,
            get("urls").map_or(Value::NilList(Kind::String), any),
        )
        .tagged("Annotations", Some("annotations"), true, annotations)
        .tagged(
            "Data",
            Some("data"),
            true,
            get("data").map_or(Value::NilList(Kind::Uint), any),
        )
        .tagged("Platform", Some("platform"), true, platform)
        .tagged(
            "ArtifactType",
            Some("artifactType"),
            true,
            s(&text("artifactType")),
        )
        .value()
}

/// A `time.Time`, as a template prints it (Time.String) and as JSON encodes it (RFC 3339
/// with its fraction), from its JSON text.
#[derive(Debug)]
struct GoTime(String);

impl shards_template::Object for GoTime {
    fn type_name(&self) -> &str {
        "time.Time"
    }

    fn format(&self, out: &mut String) {
        // "2006-01-02 15:04:05.999999999 -0700 MST", in UTC.
        let (date, rest) = self.0.split_once('T').unwrap_or((&self.0, ""));
        let clock = rest.trim_end_matches('Z');
        out.push_str(&format!("{date} {clock} +0000 UTC"));
    }

    fn json(&self, out: &mut String) -> Result<(), String> {
        out.push('"');
        out.push_str(&self.0);
        out.push('"');
        Ok(())
    }
}

/// An image's InspectResponse (moby api/types/image/image_inspect.go), as Go types, from
/// its JSON as images are inspected (inspect.rs, `document`).
pub(super) fn image_value(json: &str) -> Value {
    let d: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
    let text = |k: &str| {
        d.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let list = |v: Option<&serde_json::Value>| -> Vec<String> {
        v.and_then(serde_json::Value::as_array)
            .map(|l| l.iter().filter_map(|e| e.as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    };
    let config = d.get("Config");
    let cfg_list = |k: &str| {
        config
            .and_then(|c| c.get(k))
            .map_or(Value::NilList(Kind::String), |v| strs(&list(Some(v)), false))
    };
    let cfg_text = |k: &str| s(&image_string(config, k));
    let cfg_set = |k: &str| {
        config
            .and_then(|c| c.get(k))
            .and_then(serde_json::Value::as_object)
            .map_or(Value::NilMap(Kind::Any), |m| set(m.keys().cloned()))
    };
    let labels = config
        .and_then(|c| c.get("Labels"))
        .and_then(serde_json::Value::as_object)
        .map_or(Value::NilMap(Kind::String), |m| {
            Value::string_map(
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned())),
            )
        });
    let run = Run::default();
    let config_value = Struct::pointer("v1.DockerOCIImageConfig")
        .tagged("User", Some("User"), true, cfg_text("User"))
        .tagged(
            "ExposedPorts",
            Some("ExposedPorts"),
            true,
            cfg_set("ExposedPorts"),
        )
        .tagged("Env", Some("Env"), true, cfg_list("Env"))
        .tagged("Entrypoint", Some("Entrypoint"), true, cfg_list("Entrypoint"))
        .tagged("Cmd", Some("Cmd"), true, cfg_list("Cmd"))
        .tagged("Volumes", Some("Volumes"), true, cfg_set("Volumes"))
        .tagged("WorkingDir", Some("WorkingDir"), true, cfg_text("WorkingDir"))
        .tagged("Labels", Some("Labels"), true, labels)
        .tagged("StopSignal", Some("StopSignal"), true, cfg_text("StopSignal"))
        .tagged(
            "ArgsEscaped",
            Some("ArgsEscaped"),
            true,
            Value::Bool(
                config
                    .and_then(|c| c.get("ArgsEscaped"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            ),
        )
        .tagged(
            "Healthcheck",
            Some("Healthcheck"),
            true,
            healthcheck(&run, config),
        )
        .tagged("OnBuild", Some("OnBuild"), true, cfg_list("OnBuild"))
        .tagged("Shell", Some("Shell"), true, cfg_list("Shell"))
        .value();
    let rootfs = d.get("RootFS");
    let layers = list(rootfs.and_then(|r| r.get("Layers")));
    let pulls: Vec<Value> = d
        .pointer("/Identity/Pull")
        .and_then(serde_json::Value::as_array)
        .map(|l| {
            l.iter()
                .map(|p| {
                    Struct::new("image.PullIdentity")
                        .tagged(
                            "Repository",
                            Some("Repository"),
                            true,
                            s(p.get("Repository")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()),
                        )
                        .value()
                })
                .collect()
        })
        .unwrap_or_default();
    let identity = if d.get("Identity").is_some() {
        Struct::pointer("image.Identity")
            .tagged("Signature", Some("Signature"), true, Value::NilList(Kind::Any))
            .tagged(
                "Pull",
                Some("Pull"),
                true,
                if pulls.is_empty() {
                    Value::NilList(Kind::Any)
                } else {
                    Value::list(pulls)
                },
            )
            .tagged("Build", Some("Build"), true, Value::NilList(Kind::Any))
            .value()
    } else {
        Struct::nil("image.Identity")
    };
    let last_tag = d
        .pointer("/Metadata/LastTagTime")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("0001-01-01T00:00:00Z");
    Struct::new("image.InspectResponse")
        .tagged("ID", Some("Id"), false, s(&text("Id")))
        .field("RepoTags", strs(&list(d.get("RepoTags")), false))
        .field("RepoDigests", strs(&list(d.get("RepoDigests")), false))
        .tagged("Comment", Some("Comment"), true, s(&text("Comment")))
        .tagged("Created", Some("Created"), true, s(&text("Created")))
        .tagged("Author", Some("Author"), true, s(&text("Author")))
        .field("Config", config_value)
        .field("Architecture", s(&text("Architecture")))
        .tagged("Variant", Some("Variant"), true, s(&text("Variant")))
        .field("Os", s(&text("Os")))
        .tagged("OsVersion", Some("OsVersion"), true, s(&text("OsVersion")))
        .field(
            "Size",
            int(d.get("Size").and_then(serde_json::Value::as_i64).unwrap_or(0)),
        )
        .tagged(
            "GraphDriver",
            Some("GraphDriver"),
            true,
            Struct::nil("storage.DriverData"),
        )
        .field(
            "RootFS",
            Struct::new("image.RootFS")
                .tagged(
                    "Type",
                    Some("Type"),
                    true,
                    s(rootfs
                        .and_then(|r| r.get("Type"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()),
                )
                .tagged("Layers", Some("Layers"), true, strs(&layers, layers.is_empty()))
                .value(),
        )
        .field(
            "Metadata",
            Struct::new("image.Metadata")
                .tagged(
                    "LastTagTime",
                    Some("LastTagTime"),
                    false,
                    Value::object(GoTime(last_tag.to_owned())),
                )
                .value(),
        )
        .tagged(
            "Descriptor",
            Some("Descriptor"),
            true,
            d.get("Descriptor")
                .map_or_else(|| Struct::nil("v1.Descriptor"), descriptor),
        )
        .tagged("Manifests", Some("Manifests"), true, Value::NilList(Kind::Any))
        .tagged("Identity", Some("Identity"), true, identity)
        .value()
}

/// The document as Go's json.Marshal writes it.
pub(super) fn json(doc: &Value) -> String {
    let t = shards_template::Template::parse("", "{{json .}}");
    t.and_then(|t| t.execute(doc)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each case of testdata/inspect.json: the same command line, made into a request as
    /// shards' CLI makes it, into the document dockerd made of it, but for what only a
    /// host or a moment can say, and what is shards' own (DEVIATIONS).
    #[test]
    fn documents_are_dockerds() {
        let golden: serde_json::Value = serde_json::from_str(include_str!("testdata/inspect.json")).unwrap();
        let image = &golden["image_config"];
        for case in golden["cases"].as_array().unwrap() {
            let args: Vec<String> = case["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap().to_owned())
                .collect();
            let detached = case["verb"] == "run";
            let mut run = crate::cli::request::for_test(&args).unwrap();
            run.detach = detached;
            let want = &case["doc"];
            let started = want["State"]["Status"] == "running";
            let container = Container {
                id: "c".repeat(64),
                name: want["Name"].as_str().unwrap().trim_start_matches('/').to_owned(),
                image: run.image.clone(),
                command: Vec::new(),
                created: 1,
                state: if started { Life::Running } else { Life::Created },
                started: started.then_some(2),
                finished: None,
                exit_code: None,
                auto_remove: run.remove,
                log_lost: 0,
                stop_signal: None,
                stop_timeout: run.stop_timeout,
                ports: Vec::new(),
                image_id: Some(golden["image_id"].as_str().unwrap().to_owned()),
                labels: Default::default(),
                oom_killed: false,
            };
            let manifest = &want["ImageManifestDescriptor"];
            let facts = Facts {
                container: &container,
                request: &run,
                image: Some(image),
                manifest: Some(manifest),
                paused: false,
                pid: started.then_some(7),
                health: None,
                exec_ids: Vec::new(),
                log_path: String::new(),
                net: None,
            };
            let mut got: serde_json::Value = serde_json::from_str(&json(&document(&facts))).unwrap();
            let mut want = want.clone();
            normalize(&mut got, &mut want, started);
            assert_eq!(got, want, "{args:?}");
        }
    }

    /// What a test cannot share with a host, set alike in both; and shards' own values
    /// where Docker Engine's would be untrue of a microVM, each said why.
    fn normalize(got: &mut serde_json::Value, want: &mut serde_json::Value, started: bool) {
        use serde_json::json;
        for d in [&mut *got, &mut *want] {
            d["Id"] = json!("<ID>");
            d["Created"] = json!("<TIME>");
            // docker/cli sends the environment through a Go map (runCreate's
            // ParseProxyConfig), in an order of Go's choosing that differs run to run;
            // shards keeps the order given.
            if let Some(env) = d["Config"]["Env"].as_array_mut() {
                env.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
            }
            if started {
                d["State"]["StartedAt"] = json!("<TIME>");
                d["State"]["Pid"] = json!("<PID>");
            }
            if d["Config"]["Hostname"] == json!("c".repeat(12)) {
                d["Config"]["Hostname"] = json!("<ID12>");
            }
            // Docker Desktop's engine sets a stop timeout of its own where none was
            // asked (StopTimeout 1); dockerd leaves it unset, as shards does.
            if d["Config"]["StopTimeout"] == json!(1) {
                d["Config"].as_object_mut().unwrap().remove("StopTimeout");
            }
            // The guest's files are no host's: no paths. Its log is shards'.
            for k in ["ResolvConfPath", "HostnamePath", "HostsPath", "LogPath"] {
                d[k] = json!("");
            }
            // A microVM has no runc, and no sandbox or endpoint IDs of libnetwork's; its
            // address on the bridge is the same in every guest.
            d["HostConfig"]["Runtime"] = json!("shards");
            // Its storage and log are shards' own, as `info` names them.
            d["Driver"] = json!("erofs");
            d["Storage"]["RootFS"]["Snapshot"]["Name"] = json!("erofs");
            d["HostConfig"]["LogConfig"]["Type"] = json!("shards");
            d["NetworkSettings"]["SandboxID"] = json!("");
            d["NetworkSettings"]["SandboxKey"] = json!("");
            if let Some(nets) = d["NetworkSettings"]["Networks"].as_object_mut() {
                for n in nets.values_mut() {
                    for k in ["NetworkID", "EndpointID", "Gateway", "IPAddress", "MacAddress"] {
                        n[k] = json!("");
                    }
                    n["IPPrefixLen"] = json!(0);
                }
            }
        }
    }
}
