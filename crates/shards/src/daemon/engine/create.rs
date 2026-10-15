//! A create's body (moby's container.CreateRequest: Config's fields, HostConfig and
//! NetworkingConfig) made the run shards' CLI makes of the command line that made it
//! (D170). Held to docker/cli v29.8.1's own `parse` by testdata/create-bodies.json: each
//! body there is what the CLI posts for a command line, and its run is the run shards' CLI
//! makes of the same command line.
//!
//! A body says some things a command line says otherwise, and the run keeps them as the
//! command line would: a mount as `--mount`'s fields, a device as `HOST:CONTAINER:PERMS`, a
//! limit or a rate by its number. Two things no command line says, the run keeps as the
//! body gives them: the streams its client attaches, and GPU requests. What a run cannot
//! hold, shards refuses, naming the field and why.

use serde_json::Value;
use shards_cmdline::network::{self, Attachment, TopLevel};
use shards_ipc::{DeviceRequest, Endpoint, Health, Publish, Pull, Resources, Run, Streams};

/// The run a create's `body` asks for, named `name` and for `platform` where its query
/// gives them. Its image is the store's: a create pulls nothing. `image_cmd` gives the
/// image's command where moby's merge takes it: an entrypoint given empty (`[]`, which
/// `[""]` is not) and no command.
pub fn run_of(
    body: &Value,
    name: Option<&str>,
    platform: Option<&str>,
    image_cmd: impl FnOnce() -> Result<Option<Vec<String>>, String>,
) -> Result<Run, String> {
    let host = body.get("HostConfig").unwrap_or(&Value::Null);
    unheld(body, host)?;
    let image = text(body, "Image");
    if image.is_empty() {
        return Err("Config.Image: a create names the image it runs".into());
    }
    let platform = platform.unwrap_or_default();
    crate::cli::request::check_platform(platform)?;
    let mut cmd = words(body, "Cmd").unwrap_or_default();
    let entrypoint = match words(body, "Entrypoint") {
        None => None,
        // `[""]` clears the image's entrypoint and its command, as `--entrypoint ""` does.
        Some(e) if e == [""] => Some(Vec::new()),
        // `[]` clears its entrypoint, and takes its command where none is given.
        Some(e) if e.is_empty() => {
            if cmd.is_empty() {
                cmd = image_cmd()?.unwrap_or_default();
            }
            Some(Vec::new())
        }
        Some(e) => Some(e),
    };
    let modes = ["PidMode", "IpcMode", "UTSMode", "UsernsMode", "CgroupnsMode"].map(|k| text(host, k));
    let [pid, ipc, uts, userns, cgroupns] = &modes;
    crate::cli::request::namespace_modes([
        (pid, "HostConfig.PidMode"),
        (ipc, "HostConfig.IpcMode"),
        (uts, "HostConfig.UTSMode"),
        (userns, "HostConfig.UsernsMode"),
        (cgroupns, "HostConfig.CgroupnsMode"),
    ])?;
    let swappiness = host
        .get("MemorySwappiness")
        .and_then(Value::as_i64)
        .filter(|s| *s != -1);
    if let Some(s) = swappiness {
        crate::cli::request::check_swappiness(s, "HostConfig.MemorySwappiness")?;
    }
    let restart = host.get("RestartPolicy").unwrap_or(&Value::Null);
    let policy = text(restart, "Name");
    if flag(host, "AutoRemove") && !matches!(policy.as_str(), "" | "no") {
        return Err(format!(
            "HostConfig.AutoRemove and HostConfig.RestartPolicy {policy}: a run removed as it ends is not restarted: give one"
        ));
    }
    let umask = match host.get("Umask").filter(|u| !u.is_null()) {
        None => None,
        Some(u) => match u.as_u64().and_then(|u| u32::try_from(u).ok()) {
            Some(mask) if mask <= 0o777 => Some(mask),
            _ => {
                return Err(format!(
                    "HostConfig.Umask {u}: a umask is at most 0777 (511), the nine permission bits it clears"
                ));
            }
        },
    };
    let (network, endpoints) = networks(body, host)?;
    let publish = publish(host)?;
    let (devices, cdi_devices, device_requests) = devices(host)?;
    let (cap_add, cap_drop) =
        crate::cli::request::effective_caps(&strings(host, "CapAdd"), &strings(host, "CapDrop"));
    let console = host.get("ConsoleSize").and_then(Value::as_array);
    let size = |i: usize| {
        console
            .and_then(|c| c.get(i))
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
            .unwrap_or(0)
    };
    let log = host.get("LogConfig").unwrap_or(&Value::Null);
    let weight = number(host, "BlkioWeight");
    Ok(Run {
        image,
        cmd,
        entrypoint,
        env: strings(body, "Env"),
        labels: pairs(body, "Labels", "Config.Labels")?,
        expose: exposed(body, &publish)?,
        publish,
        publish_all: flag(host, "PublishAllPorts"),
        add_hosts: strings(host, "ExtraHosts"),
        dns: strings(host, "Dns"),
        dns_search: strings(host, "DnsSearch"),
        dns_options: strings(host, "DnsOptions"),
        domainname: text(body, "Domainname"),
        workdir: text(body, "WorkingDir"),
        user: text(body, "User"),
        hostname: Some(text(body, "Hostname")).filter(|h| !h.is_empty()),
        interactive: flag(body, "OpenStdin"),
        // Its terminal's rows and columns, as ConsoleSize gives them.
        tty: flag(body, "Tty").then(|| (size(0), size(1))),
        streams: Some(Streams {
            stdin: flag(body, "AttachStdin"),
            stdout: flag(body, "AttachStdout"),
            stderr: flag(body, "AttachStderr"),
            stdin_once: flag(body, "StdinOnce"),
        }),
        remove: flag(host, "AutoRemove"),
        restart_policy: (policy, number(restart, "MaximumRetryCount")),
        network,
        endpoints,
        stop_signal: Some(text(body, "StopSignal")).filter(|s| !s.is_empty()),
        stop_timeout: body.get("StopTimeout").and_then(Value::as_i64),
        health: health(body),
        cidfile: text(host, "ContainerIDFile"),
        resources: Resources {
            memory: number(host, "Memory"),
            memory_reservation: number(host, "MemoryReservation"),
            memory_swap: number(host, "MemorySwap"),
            memory_swappiness: swappiness,
            oom_kill_disable: flag(host, "OomKillDisable"),
            nano_cpus: number(host, "NanoCpus"),
            cpu_shares: number(host, "CpuShares"),
            cpu_period: number(host, "CpuPeriod"),
            cpu_quota: number(host, "CpuQuota"),
            cpuset_cpus: text(host, "CpusetCpus"),
            cpuset_mems: text(host, "CpusetMems"),
            pids_limit: number(host, "PidsLimit"),
            blkio_weight: u16::try_from(weight)
                .map_err(|_| format!("HostConfig.BlkioWeight {weight}: from 10 to 1000"))?,
            cpu_realtime_period: number(host, "CpuRealtimePeriod"),
            cpu_realtime_runtime: number(host, "CpuRealtimeRuntime"),
            cpu_count: number(host, "CpuCount"),
            cpu_percent: number(host, "CpuPercent"),
        },
        read_only: flag(host, "ReadonlyRootfs"),
        tmpfs: tmpfs(host)?,
        shm_size: number(host, "ShmSize"),
        ulimits: host
            .get("Ulimits")
            .and_then(Value::as_array)
            .map(|l| {
                l.iter()
                    .map(|u| format!("{}={}:{}", text(u, "Name"), number(u, "Soft"), number(u, "Hard")))
                    .collect()
            })
            .unwrap_or_default(),
        sysctls: pairs(host, "Sysctls", "HostConfig.Sysctls")?,
        cap_add,
        cap_drop,
        group_add: strings(host, "GroupAdd"),
        oom_score_adj: number(host, "OomScoreAdj"),
        privileged: flag(host, "Privileged"),
        security_opt: security_opts(host)?,
        system_paths: system_paths(host)?,
        binds: strings(host, "Binds"),
        volumes: keys(body, "Volumes"),
        mounts: mounts(host)?,
        volumes_from: strings(host, "VolumesFrom"),
        volume_driver: text(host, "VolumeDriver"),
        init_process: host.get("Init").and_then(Value::as_bool),
        pid: pid.clone(),
        devices,
        device_cgroup_rules: strings(host, "DeviceCgroupRules"),
        cdi_devices,
        device_requests,
        blkio_weight_device: rates(host, "BlkioWeightDevice", "Weight"),
        device_read_bps: rates(host, "BlkioDeviceReadBps", "Rate"),
        device_write_bps: rates(host, "BlkioDeviceWriteBps", "Rate"),
        device_read_iops: rates(host, "BlkioDeviceReadIOps", "Rate"),
        device_write_iops: rates(host, "BlkioDeviceWriteIOps", "Rate"),
        annotations: pairs(host, "Annotations", "HostConfig.Annotations")?,
        isolation: text(host, "Isolation"),
        runtime: text(host, "Runtime"),
        ipc: ipc.clone(),
        uts: uts.clone(),
        userns: userns.clone(),
        cgroupns: cgroupns.clone(),
        umask,
        storage_opt: pairs(host, "StorageOpt", "HostConfig.StorageOpt")?,
        log_driver: text(log, "Type"),
        log_opts: pairs(log, "Config", "HostConfig.LogConfig.Config")?,
        name: name.map(str::to_string),
        platform: platform.to_string(),
        pull: Pull::Never,
        // As `shards create` asks for it.
        create: true,
        detach: true,
        ..Run::default()
    })
}

/// What a body may give that a run cannot hold, each refused with why.
fn unheld(body: &Value, host: &Value) -> Result<(), String> {
    let given = |v: &Value, k: &str| {
        v.get(k).is_some_and(|x| match x {
            Value::Null => false,
            Value::String(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(o) => !o.is_empty(),
            Value::Number(n) => n.as_f64() != Some(0.0),
            Value::Bool(b) => *b,
        })
    };
    let refusals: [(&Value, &str, &str); 7] = [
        (
            body,
            "OnBuild",
            "Config.OnBuild: ONBUILD's instructions are an image's, for the builds made from it, and a run is no build",
        ),
        (
            body,
            "Shell",
            "Config.Shell: a run's shell form is its image's SHELL, which its Cmd and Entrypoint already say how to run",
        ),
        (
            host,
            "Links",
            "HostConfig.Links: a run reaches another by its network's names, as each grant to it allows; legacy links grant what no directive names",
        ),
        (
            host,
            "Cgroup",
            "HostConfig.Cgroup: a run's cgroup is its microVM's own; one of another run's it cannot be",
        ),
        (
            host,
            "CgroupParent",
            "HostConfig.CgroupParent: a run's cgroups are its microVM's own, and shards places no microVM under a host cgroup of a run's choosing",
        ),
        (
            host,
            "IOMaximumBandwidth",
            "HostConfig.IOMaximumBandwidth: a microVM's disks take no bandwidth limit of a run's",
        ),
        (
            host,
            "IOMaximumIOps",
            "HostConfig.IOMaximumIOps: a microVM's disks take no IOPS limit of a run's",
        ),
    ];
    for (v, k, why) in refusals {
        if given(v, k) {
            return Err(why.to_string());
        }
    }
    Ok(())
}

fn text(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn flag(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

fn number(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(Value::as_i64).unwrap_or(0)
}

fn strings(v: &Value, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// A command or entrypoint (strslice.StrSlice): a list of words, or one string as one
/// word; `None` where not given.
fn words(v: &Value, k: &str) -> Option<Vec<String>> {
    match v.get(k)? {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(a) => Some(a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()),
        _ => None,
    }
}

/// An object's keys, in its order.
fn keys(v: &Value, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// An object of strings as `KEY=VALUE`s, in its order: a key with `=` in it refused, as
/// no `KEY=VALUE` could say where it ends.
fn pairs(v: &Value, k: &str, field: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for (key, val) in v.get(k).and_then(Value::as_object).into_iter().flatten() {
        if key.contains('=') {
            return Err(format!(
                "{field}: {key:?}: a key holds no '=', which ends it as KEY=VALUE"
            ));
        }
        out.push(format!("{key}={}", val.as_str().unwrap_or_default()));
    }
    Ok(out)
}

/// Throttles and weights as their flags give them, `PATH:NUMBER`.
fn rates(v: &Value, k: &str, field: &str) -> Vec<String> {
    v.get(k)
        .and_then(Value::as_array)
        .map(|l| {
            l.iter()
                .map(|r| format!("{}:{}", text(r, "Path"), number(r, field)))
                .collect()
        })
        .unwrap_or_default()
}

/// Its tmpfs mounts as `--tmpfs` gives them, `PATH[:OPTIONS]`: a path with `:` in it
/// refused, which that could not hold.
fn tmpfs(host: &Value) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for (path, opts) in host.get("Tmpfs").and_then(Value::as_object).into_iter().flatten() {
        if path.contains(':') {
            return Err(format!(
                "HostConfig.Tmpfs: {path:?}: a tmpfs mount's path holds no ':'"
            ));
        }
        out.push(match opts.as_str().unwrap_or_default() {
            "" => path.clone(),
            opts => format!("{path}:{opts}"),
        });
    }
    Ok(out)
}

/// A port, `PORT[-LAST][/PROTO]` (nat.Port), as its first and last numbers and protocol.
fn port(key: &str, field: &str) -> Result<(u16, u16, String), String> {
    shards_cmdline::ports::parse_port_range(key).map_err(|why| format!("{field}: {key}: {why}"))
}

/// Its port bindings, each a `-p`'s: a port to each host address and port it gives.
fn publish(host: &Value) -> Result<Vec<Publish>, String> {
    let mut out = Vec::new();
    for (key, bindings) in host
        .get("PortBindings")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let (first, last, proto) = port(key, "HostConfig.PortBindings")?;
        if first != last {
            return Err(format!(
                "HostConfig.PortBindings: {key}: a binding's port is one port"
            ));
        }
        for b in bindings.as_array().into_iter().flatten() {
            let (host_ip, host_port) = (text(b, "HostIp"), text(b, "HostPort"));
            if !host_ip.is_empty() {
                network::parse_addr(&host_ip)
                    .map_err(|why| format!("HostConfig.PortBindings: {key}: {why}"))?;
            }
            if !host_port.is_empty() && host_port.splitn(2, '-').any(|p| p.parse::<u16>().is_err()) {
                return Err(format!(
                    "HostConfig.PortBindings: {key}: host port {host_port:?} is a port or a range of them, FIRST-LAST"
                ));
            }
            out.push(Publish {
                port: first,
                proto: proto.clone(),
                host_ip,
                host_port,
            });
        }
    }
    Ok(out)
}

/// The ports it exposes that it does not publish, a range a port at a time: one published
/// is exposed with its binding.
fn exposed(body: &Value, published: &[Publish]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for key in keys(body, "ExposedPorts") {
        let (first, last, proto) = port(&key, "Config.ExposedPorts")?;
        for p in first..=last {
            if !published.iter().any(|b| b.port == p && b.proto == proto) {
                out.push(format!("{p}/{proto}"));
            }
        }
    }
    Ok(out)
}

/// Its health check, `Test` and its timings in nanoseconds as `--health-*` make them;
/// none where it sets nothing, which leaves the image's.
fn health(body: &Value) -> Option<Health> {
    let h = body.get("Healthcheck").filter(|h| h.is_object())?;
    Some(Health {
        test: strings(h, "Test"),
        interval: number(h, "Interval"),
        timeout: number(h, "Timeout"),
        start_period: number(h, "StartPeriod"),
        start_interval: number(h, "StartInterval"),
        retries: number(h, "Retries"),
    })
    .filter(|h| *h != Health::default())
}

/// Its security options, each `KEY=VALUE`, `KEY:VALUE` or `no-new-privileges`: a seccomp
/// profile is its JSON, which the CLI reads from its file.
fn security_opts(host: &Value) -> Result<Vec<String>, String> {
    let opts = strings(host, "SecurityOpt");
    for opt in &opts {
        let value = opt
            .split_once('=')
            .or_else(|| opt.split_once(':'))
            .map(|(_, v)| v);
        if opt != "no-new-privileges" && value.is_none_or(str::is_empty) {
            return Err(format!(
                "HostConfig.SecurityOpt: {opt:?}: an option is KEY=VALUE, or no-new-privileges"
            ));
        }
    }
    Ok(opts)
}

/// Whether what a run's kernel hides and makes read-only is left as it is
/// (`systempaths=unconfined`, which the CLI sends as MaskedPaths and ReadonlyPaths empty);
/// lists of a run's own refused.
fn system_paths(host: &Value) -> Result<bool, String> {
    let list = |k: &str| {
        host.get(k)
            .filter(|v| !v.is_null())
            .map(|v| v.as_array().map(Vec::len))
    };
    match (list("MaskedPaths"), list("ReadonlyPaths")) {
        (None, None) => Ok(false),
        (Some(Some(0)), Some(Some(0))) => Ok(true),
        _ => Err(
            "HostConfig.MaskedPaths and ReadonlyPaths: a microVM's kernel shows a run what shards' masks leave it, or all of it with both empty (systempaths=unconfined); paths of a run's own it does not take"
                .into(),
        ),
    }
}

/// A run's devices, its CDI devices and its GPU requests.
type Devices = (Vec<String>, Vec<String>, Vec<DeviceRequest>);

/// Its devices, as `--device` keeps them (`HOST:CONTAINER:PERMS`, checked as it checks
/// them); its CDI devices, by name; and its GPU requests, as given.
fn devices(host: &Value) -> Result<Devices, String> {
    let mut given = Vec::new();
    for d in host
        .get("Devices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (src, dst) = (text(d, "PathOnHost"), text(d, "PathInContainer"));
        if !src.starts_with('/') {
            return Err(format!(
                "HostConfig.Devices: {src:?}: a device's host path is absolute"
            ));
        }
        let perms = Some(text(d, "CgroupPermissions")).filter(|p| !p.is_empty());
        given.push(format!(
            "{src}:{}:{}",
            if dst.is_empty() { &src } else { &dst },
            perms.as_deref().unwrap_or("rwm")
        ));
    }
    let (devices, _) = crate::cli::request::devices(&given)?;
    let (mut cdi, mut requests) = (Vec::new(), Vec::new());
    for r in host
        .get("DeviceRequests")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let ids = r
            .get("DeviceIDs")
            .filter(|i| !i.is_null())
            .map(|_| strings(r, "DeviceIDs"));
        if text(r, "Driver") == "cdi" {
            cdi.extend(ids.unwrap_or_default());
            continue;
        }
        let capabilities = r
            .get("Capabilities")
            .and_then(Value::as_array)
            .map(|sets| {
                sets.iter()
                    .map(|set| {
                        set.as_array()
                            .map(|c| c.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .unwrap_or_default();
        let options = r
            .get("Options")
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        requests.push(DeviceRequest {
            driver: text(r, "Driver"),
            count: number(r, "Count"),
            device_ids: ids,
            capabilities,
            options,
        });
    }
    Ok((devices, cdi, requests))
}

/// Its mounts, as `--mount` keeps them (shards_cmdline::mounts::encode), each checked as
/// `--mount` checks one.
fn mounts(host: &Value) -> Result<Vec<String>, String> {
    use shards_cmdline::mounts::{Bind, Image, Mount, Tmpfs, Volume, encode, parse_mount};
    let map = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    for m in host.get("Mounts").and_then(Value::as_array).into_iter().flatten() {
        let object = |k: &str| m.get(k).filter(|o| o.is_object());
        // Each type's options on its own type alone, as moby takes them, even empty.
        let kind = text(m, "Type");
        for (options, of) in [
            ("BindOptions", "bind"),
            ("VolumeOptions", "volume"),
            ("ImageOptions", "image"),
            ("TmpfsOptions", "tmpfs"),
            ("ClusterOptions", "cluster"),
        ] {
            if m.get(options).is_some_and(|o| !o.is_null()) && kind != of {
                return Err(format!(
                    "HostConfig.Mounts: {options} on a mount of type {kind:?}: they are a {of} mount's"
                ));
            }
        }
        let bind = object("BindOptions").map(|b| Bind {
            propagation: text(b, "Propagation"),
            non_recursive: flag(b, "NonRecursive"),
            read_only_non_recursive: flag(b, "ReadOnlyNonRecursive"),
            read_only_force_recursive: flag(b, "ReadOnlyForceRecursive"),
            create_mountpoint: flag(b, "CreateMountpoint"),
        });
        let volume = object("VolumeOptions").map(|v| Volume {
            no_copy: flag(v, "NoCopy"),
            labels: map(v, "Labels"),
            subpath: text(v, "Subpath"),
            driver: v
                .get("DriverConfig")
                .filter(|c| c.is_object())
                .map(|c| (text(c, "Name"), map(c, "Options"))),
        });
        let image = object("ImageOptions").map(|i| Image {
            subpath: text(i, "Subpath"),
        });
        let tmpfs = match object("TmpfsOptions") {
            None => None,
            Some(t) => {
                if t.get("Options")
                    .and_then(Value::as_array)
                    .is_some_and(|o| !o.is_empty())
                {
                    return Err(
                        "HostConfig.Mounts: TmpfsOptions.Options: give a tmpfs mount's flags as HostConfig.Tmpfs's options"
                            .into(),
                    );
                }
                let mode = number(t, "Mode");
                Some(Tmpfs {
                    size_bytes: number(t, "SizeBytes"),
                    mode: u32::try_from(mode).map_err(|_| format!("HostConfig.Mounts: tmpfs mode {mode}"))?,
                })
            }
        };
        let encoded = encode(&Mount {
            kind,
            source: text(m, "Source"),
            target: text(m, "Target"),
            read_only: flag(m, "ReadOnly"),
            consistency: text(m, "Consistency"),
            bind,
            volume,
            image,
            tmpfs,
        });
        parse_mount(&encoded, None).map_err(|why| format!("HostConfig.Mounts: {why}"))?;
        out.push(encoded);
    }
    Ok(out)
}

/// Its network mode and the endpoints it asks of the networks it joins, as the CLI makes
/// them of its `--network`s: its mode's network first, then the rest; checked as the CLI
/// checks them.
fn networks(body: &Value, host: &Value) -> Result<(String, Vec<Endpoint>), String> {
    let mode = Some(text(host, "NetworkMode"))
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "default".into());
    // No network at all is network none's (moby: a disabled network is set up as none).
    if flag(body, "NetworkDisabled") {
        return Ok(("none".into(), Vec::new()));
    }
    let empty = serde_json::Map::new();
    let configs = body
        .get("NetworkingConfig")
        .and_then(|n| n.get("EndpointsConfig"))
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let mut given = vec![attachment(&mode, configs.get(&mode))?];
    for (name, c) in configs {
        if *name != mode {
            given.push(attachment(name, Some(c))?);
        }
    }
    // An API before 1.44 gives the MAC address in Config, for the mode's network.
    let mac = text(body, "MacAddress");
    if let Some(first) = given.first_mut()
        && !mac.is_empty()
    {
        if !first.mac.is_empty() && first.mac != mac {
            return Err(format!(
                "Config.MacAddress {mac} and network {mode}'s MacAddress {}: give one",
                first.mac
            ));
        }
        first.mac = mac;
    }
    let mut endpoints = network::endpoints(&given, &TopLevel::default())?;
    // The default network alone: the endpoint the CLI asks of it where no `--network` is
    // given, which a bare one given leaves out.
    if mode == "default" && configs.len() == 1 && configs.contains_key("default") && endpoints.is_empty() {
        endpoints = given;
    }
    Ok((
        mode,
        endpoints
            .into_iter()
            .map(|a| Endpoint {
                network: a.target,
                aliases: a.aliases,
                ipv4: a.ipv4.map(|a| a.to_string()).unwrap_or_default(),
                ipv6: a.ipv6.map(|a| a.to_string()).unwrap_or_default(),
                link_local: a.link_local.iter().map(ToString::to_string).collect(),
                mac: a.mac,
                driver_opts: a.driver_opts,
                gw_priority: a.gw_priority,
            })
            .collect(),
    ))
}

/// What `c` (an EndpointSettings) asks of its endpoint on network `target`: its
/// addresses read as netip reads them; what moby fills in once it is made left out.
fn attachment(target: &str, c: Option<&Value>) -> Result<Attachment, String> {
    let c = c.unwrap_or(&Value::Null);
    if c.get("Links")
        .and_then(Value::as_array)
        .is_some_and(|l| !l.is_empty())
    {
        return Err(format!(
            "NetworkingConfig.EndpointsConfig.{target}.Links: a run reaches another by its network's names, as each grant to it allows; legacy links grant what no directive names"
        ));
    }
    let ipam = c.get("IPAMConfig").unwrap_or(&Value::Null);
    let field = |k: &str| format!("NetworkingConfig.EndpointsConfig.{target}.IPAMConfig.{k}");
    let addr = |k: &str| match text(ipam, k) {
        a if a.is_empty() => Ok(None),
        a => network::parse_addr(&a)
            .map(Some)
            .map_err(|why| format!("{}: {why}", field(k))),
    };
    Ok(Attachment {
        target: target.to_string(),
        aliases: strings(c, "Aliases"),
        driver_opts: c
            .get("DriverOpts")
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        ipv4: addr("IPv4Address")?,
        ipv6: addr("IPv6Address")?,
        link_local: strings(ipam, "LinkLocalIPs")
            .iter()
            .map(|a| network::parse_addr(a).map_err(|why| format!("{}: {why}", field("LinkLocalIPs"))))
            .collect::<Result<_, _>>()?,
        mac: text(c, "MacAddress"),
        gw_priority: number(c, "GwPriority"),
    })
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    /// A run as both sides say it, what each says alike made alike: lists whose order no
    /// one reads sorted, `KEY` and `KEY=` one, a limit by its numbers, the streams and GPU
    /// requests by what they ask; and the client's own (its environment, its terminal's
    /// size, how it asks) left out.
    fn normal(mut r: Run) -> Run {
        let kv = |l: &mut Vec<String>| {
            for e in l.iter_mut() {
                if !e.contains('=') {
                    e.push('=');
                }
            }
            l.sort();
        };
        kv(&mut r.labels);
        kv(&mut r.sysctls);
        kv(&mut r.annotations);
        kv(&mut r.storage_opt);
        kv(&mut r.log_opts);
        r.tmpfs.sort();
        r.binds.sort();
        // A ulimit as NAME=SOFT:HARD, its soft its hard where only one is given.
        for u in &mut r.ulimits {
            if let Some((name, limits)) = u.split_once('=')
                && !limits.contains(':')
            {
                *u = format!("{name}={limits}:{limits}");
            }
        }
        // Ports exposed: those named and those published.
        r.expose
            .extend(r.publish.iter().map(|p| format!("{}/{}", p.port, p.proto)));
        r.expose.sort();
        r.expose.dedup();
        r.publish.sort_by(|a, b| {
            (a.port, &a.proto, &a.host_ip, &a.host_port).cmp(&(b.port, &b.proto, &b.host_ip, &b.host_port))
        });
        // Rates and weights by their numbers: `1mb` is 1048576.
        for l in [
            &mut r.device_read_bps,
            &mut r.device_write_bps,
            &mut r.device_read_iops,
            &mut r.device_write_iops,
            &mut r.blkio_weight_device,
        ] {
            for e in l.iter_mut() {
                if let Some((path, rate)) = e.rsplit_once(':')
                    && let Ok(n) = shards_cmdline::resources::ram_in_bytes(rate)
                {
                    *e = format!("{path}:{n}");
                }
            }
        }
        // GPU requests by what they ask.
        r.device_requests = crate::run::gpu_requests(&r)
            .unwrap()
            .into_iter()
            .map(|(_, g)| DeviceRequest {
                driver: g.driver,
                count: g.count,
                device_ids: g.device_ids,
                capabilities: g.capabilities,
                options: g.options.into_iter().collect(),
            })
            .collect();
        r.gpus.clear();
        // The streams it attaches, as a create of the CLI's says them.
        r.streams = Some(r.streams.unwrap_or(Streams {
            stdin: r.interactive || r.attach.iter().any(|a| a == "stdin"),
            stdout: r.attach.is_empty() || r.attach.iter().any(|a| a == "stdout"),
            stderr: r.attach.is_empty() || r.attach.iter().any(|a| a == "stderr"),
            stdin_once: r.interactive,
        }));
        r.attach.clear();
        // The networks after its mode's, in no order.
        if let Some(rest) = r.endpoints.get_mut(1..) {
            rest.sort_by(|a, b| a.network.cmp(&b.network));
        }
        for e in &mut r.endpoints {
            e.driver_opts.sort();
        }
        // The client's: its environment, its terminal's size, and how it asks.
        r.tty = r.tty.map(|_| (0, 0));
        r.registry_env.clear();
        r.cloud_env.clear();
        r.pull = Pull::Never;
        r.create = true;
        r.detach = true;
        r.quiet = false;
        r.name = None;
        r.platform.clear();
        r
    }

    /// The lines of one run's pretty form the other's lacks, each way.
    fn differences(cli: &Run, ours: &Run) -> String {
        let (a, b) = (format!("{cli:#?}"), format!("{ours:#?}"));
        let (a, b): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
        let only = |x: &[&str], y: &[&str], tag: &str| {
            let mut rest: Vec<&str> = y.to_vec();
            let mut out = Vec::new();
            for line in x {
                match rest.iter().position(|l| l == line) {
                    Some(i) => {
                        rest.remove(i);
                    }
                    None => out.push(format!("  {tag} {}", line.trim())),
                }
            }
            out
        };
        let mut out = only(&a, &b, "cli: ");
        out.extend(only(&b, &a, "body:"));
        out.join("\n")
    }

    /// A body of `fields` over an image's.
    fn body(fields: Value) -> Value {
        let mut b = serde_json::json!({"Image": "img"});
        if let (Some(b), Some(f)) = (b.as_object_mut(), fields.as_object()) {
            b.extend(f.clone());
        }
        b
    }

    fn made(fields: Value) -> Result<Run, String> {
        run_of(&body(fields), None, None, || Ok(Some(vec!["from-image".into()])))
    }

    /// What an API's caller may give that no command line does: kept as given, or
    /// refused, naming its field.
    #[test]
    fn what_only_the_api_gives_is_kept_or_refused() {
        use serde_json::json;
        // `[]` clears the image's entrypoint and takes its command; `[""]` takes neither.
        let r = made(json!({"Entrypoint": []})).unwrap();
        assert_eq!(
            (r.entrypoint, r.cmd),
            (Some(Vec::new()), vec!["from-image".to_string()])
        );
        let r = made(json!({"Entrypoint": [], "Cmd": ["x"]})).unwrap();
        assert_eq!(r.cmd, ["x"]);
        let r = run_of(&body(json!({"Entrypoint": [""]})), None, None, || {
            Err("asked".into())
        })
        .unwrap();
        assert_eq!((r.entrypoint, r.cmd), (Some(Vec::new()), Vec::new()));
        // A string is one word.
        let r = made(json!({"Entrypoint": "/bin/sh -c", "Cmd": "echo hi"})).unwrap();
        assert_eq!(
            (r.entrypoint, r.cmd),
            (Some(vec!["/bin/sh -c".into()]), vec!["echo hi".to_string()])
        );
        // The streams as given: none attached, as `run -d` creates it, its stdin open.
        let r = made(json!({"OpenStdin": true, "AttachStdout": false, "AttachStderr": false})).unwrap();
        assert!(r.interactive);
        assert_eq!(r.streams, Some(Streams::default()));
        // GPU requests as given; CDI's by name.
        let r = made(json!({"HostConfig": {"DeviceRequests": [
            {"Driver": "nvidia", "Count": 1, "Capabilities": [["gpu", "utility"], ["compute"]], "Options": {"a": "b"}},
            {"Driver": "cdi", "DeviceIDs": ["vendor.com/gpu=0"]},
            {"DeviceIDs": ["0"]}
        ]}}))
        .unwrap();
        assert_eq!(
            r.device_requests,
            [
                DeviceRequest {
                    driver: "nvidia".into(),
                    count: 1,
                    device_ids: None,
                    capabilities: vec![vec!["gpu".into(), "utility".into()], vec!["compute".into()]],
                    options: vec![("a".into(), "b".into())],
                },
                DeviceRequest {
                    device_ids: Some(vec!["0".into()]),
                    ..DeviceRequest::default()
                },
            ]
        );
        assert_eq!(
            (r.cdi_devices, r.gpus),
            (vec!["vendor.com/gpu=0".to_string()], Vec::new())
        );
        // A network disabled is none; an API before 1.44 gives the MAC address in Config.
        let r = made(json!({"NetworkDisabled": true, "HostConfig": {"NetworkMode": "mynet"}})).unwrap();
        assert_eq!((r.network.as_str(), r.endpoints.len()), ("none", 0));
        let r =
            made(json!({"MacAddress": "02:42:ac:11:00:02", "HostConfig": {"NetworkMode": "mynet"}})).unwrap();
        assert_eq!(
            (r.endpoints[0].network.as_str(), r.endpoints[0].mac.as_str()),
            ("mynet", "02:42:ac:11:00:02")
        );
        let both = made(json!({
            "MacAddress": "02:42:ac:11:00:02",
            "HostConfig": {"NetworkMode": "mynet"},
            "NetworkingConfig": {"EndpointsConfig": {"mynet": {"MacAddress": "02:42:ac:11:00:03"}}}
        }));
        assert!(both.unwrap_err().starts_with("Config.MacAddress"));
        // Its mode's network first, then the rest.
        let r = made(json!({
            "HostConfig": {"NetworkMode": "b"},
            "NetworkingConfig": {"EndpointsConfig": {"a": {}, "b": {"Aliases": ["x"]}, "c": {}}}
        }))
        .unwrap();
        let order: Vec<&str> = r.endpoints.iter().map(|e| e.network.as_str()).collect();
        assert_eq!((r.network.as_str(), order), ("b", vec!["b", "a", "c"]));
        // Ports exposed a port at a time, `tcp` where none is named; a port bound to no
        // host port only exposed.
        let r = made(json!({
            "ExposedPorts": {"8000-8001/tcp": {}, "53": {}, "80/tcp": {}},
            "HostConfig": {"PortBindings": {"80/tcp": []}}
        }))
        .unwrap();
        assert_eq!(r.expose, ["53/tcp", "80/tcp", "8000/tcp", "8001/tcp"]);
        assert!(r.publish.is_empty());
        // The umask as a number, at most 0777; an empty health check is the image's.
        assert_eq!(
            made(json!({"HostConfig": {"Umask": 18}})).unwrap().umask,
            Some(0o22)
        );
        assert_eq!(made(json!({"Healthcheck": {}})).unwrap().health, None);
        assert_eq!(
            made(json!({"HostConfig": {"MemorySwappiness": -1}}))
                .unwrap()
                .resources
                .memory_swappiness,
            None
        );
        // What a run cannot hold, refused naming the field.
        for (fields, field) in [
            (json!({"OnBuild": ["RUN true"]}), "Config.OnBuild"),
            (json!({"Shell": ["/bin/bash", "-c"]}), "Config.Shell"),
            (json!({"HostConfig": {"Links": ["db:db"]}}), "HostConfig.Links"),
            (
                json!({"HostConfig": {"Cgroup": "container:x"}}),
                "HostConfig.Cgroup",
            ),
            (
                json!({"HostConfig": {"CgroupParent": "/agents"}}),
                "HostConfig.CgroupParent",
            ),
            (
                json!({"HostConfig": {"IOMaximumIOps": 100}}),
                "HostConfig.IOMaximumIOps",
            ),
            (
                json!({"HostConfig": {"IOMaximumBandwidth": 1}}),
                "HostConfig.IOMaximumBandwidth",
            ),
            (
                json!({"HostConfig": {"MaskedPaths": ["/proc/kcore"]}}),
                "HostConfig.MaskedPaths",
            ),
            (
                json!({"HostConfig": {"MaskedPaths": []}}),
                "HostConfig.MaskedPaths",
            ),
            (
                json!({"HostConfig": {"Mounts": [{"Type": "tmpfs", "Target": "/t", "TmpfsOptions": {"Options": [["noexec"]]}}]}}),
                "HostConfig.Mounts",
            ),
            (
                json!({"HostConfig": {"Mounts": [{"Type": "bind", "Target": "/t", "Source": "/s", "VolumeOptions": {}}]}}),
                "HostConfig.Mounts",
            ),
            (json!({"Labels": {"a=b": "c"}}), "Config.Labels"),
            (json!({"HostConfig": {"Tmpfs": {"/a:b": ""}}}), "HostConfig.Tmpfs"),
            (
                json!({"HostConfig": {"Devices": [{"PathOnHost": "dev/fuse"}]}}),
                "HostConfig.Devices",
            ),
            (
                json!({"HostConfig": {"AutoRemove": true, "RestartPolicy": {"Name": "always"}}}),
                "HostConfig.AutoRemove",
            ),
            (
                json!({"HostConfig": {"PortBindings": {"80/tcp": [{"HostPort": "http"}]}}}),
                "HostConfig.PortBindings",
            ),
            (
                json!({"HostConfig": {"PortBindings": {"80-81/tcp": [{}]}}}),
                "HostConfig.PortBindings",
            ),
            (json!({"HostConfig": {"Umask": 512}}), "HostConfig.Umask"),
            (
                json!({"HostConfig": {"MemorySwappiness": 101}}),
                "HostConfig.MemorySwappiness",
            ),
            (
                json!({"HostConfig": {"PidMode": "private"}}),
                "HostConfig.PidMode",
            ),
            (
                json!({"HostConfig": {"SecurityOpt": ["apparmor"]}}),
                "HostConfig.SecurityOpt",
            ),
            (
                json!({"NetworkingConfig": {"EndpointsConfig": {"default": {"Links": ["db"]}}}}),
                "Links",
            ),
            (
                json!({"HostConfig": {"NetworkMode": "n"}, "NetworkingConfig": {"EndpointsConfig": {"n": {"IPAMConfig": {"IPv4Address": "10.0.0.300"}}}}}),
                "IPAMConfig.IPv4Address",
            ),
            (json!({"Image": ""}), "Config.Image"),
        ] {
            match made(fields.clone()) {
                Err(e) => assert!(e.contains(field), "{fields}: {e}"),
                Ok(_) => panic!("{fields} was made a run"),
            }
        }
    }

    fn cases() -> Vec<Value> {
        serde_json::from_str(include_str!("testdata/create-bodies.json")).unwrap()
    }

    /// Every create body docker/cli's `run` makes of a command line becomes the run shards'
    /// CLI makes of the same command line, or both refuse it.
    #[test]
    fn create_bodies_become_the_runs_the_cli_makes() {
        let cases = cases();
        let (mut wrong, mut refused) = (Vec::new(), Vec::new());
        for case in &cases {
            let args: Vec<String> = case["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap().to_string())
                .collect();
            let cli = crate::cli::request::for_test(&args);
            let ours = run_of(&case["body"], None, None, || Ok(None));
            match (cli, ours) {
                (Ok(cli), Ok(ours)) => {
                    let (cli, ours) = (normal(cli), normal(ours));
                    if cli != ours {
                        wrong.push(format!("{args:?}\n{}", differences(&cli, &ours)));
                    }
                }
                (Err(cli), Err(ours)) => refused.push(format!("{args:?}\n  cli:  {cli}\n  body: {ours}")),
                (cli, ours) => wrong.push(format!("{args:?}\n  cli:  {cli:?}\n  body: {ours:?}")),
            }
        }
        assert!(
            wrong.is_empty(),
            "{} of {}:\n{}",
            wrong.len(),
            cases.len(),
            wrong.join("\n")
        );
        // Each a run cannot hold is refused by both: limits on a microVM's disks.
        assert_eq!(refused.len(), 1, "{}", refused.join("\n"));
        assert!(refused[0].contains("--io-maxbandwidth"), "{}", refused[0]);
    }
}
