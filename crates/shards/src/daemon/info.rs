//! `shards info` (docker/cli cli/command/system/info.go): what shards is and holds, laid
//! out as docker/cli lays out dockerd's answer (prettyPrintServerInfo), each line shards
//! has a true value for; what shards has no counterpart of (cgroups, containerd, runc,
//! swarm) is left out where dockerd leaves an empty value out, and says so where Docker
//! always prints its line.

use std::fmt::Write as _;
use std::path::Path;

use super::commands::{Asker, Reply};
use super::{Daemon, lock};
use crate::containers::State as Life;
use crate::spec::LOG_STDOUT;

/// What `info` tells.
struct Facts {
    containers: usize,
    running: usize,
    paused: usize,
    stopped: usize,
    images: usize,
    kernel: Option<String>,
    os: Option<String>,
    cpus: usize,
    memory: Option<u64>,
    name: Option<String>,
    root: String,
}

impl<D: crate::containers::Disk> Daemon<D> {
    /// `shards info`: Docker's client and server sections.
    pub(super) fn info(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        asker: &Asker,
        reply: &Reply<'_>,
    ) -> u8 {
        let (containers, running, paused) = {
            let paused = lock(&self.paused).len();
            let registry = lock(&self.containers);
            let all: Vec<_> = registry.all().collect();
            let running = all.iter().filter(|c| c.state == Life::Running).count();
            (all.len(), running, paused)
        };
        let images = self
            .store()
            .ok()
            .flatten()
            .and_then(|s| s.named().ok())
            .map_or(0, |n| n.len());
        let kernel = crate::guest::in_use(&self.home)
            .ok()
            .flatten()
            .and_then(|g| kernel_version(&g.kernel));
        let facts = Facts {
            containers,
            // A paused microVM runs, as a paused container does (dockerd counts it once).
            running: running.saturating_sub(paused),
            paused,
            stopped: containers.saturating_sub(running),
            images,
            kernel,
            os: os_name(),
            cpus: std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
            memory: crate::build::host_memory(),
            name: host_name(),
            root: self.home.display().to_string(),
        };
        let format = parsed.string("format");
        if !format.is_empty() {
            return formatted(format, &facts, reply);
        }
        if asker.styled() {
            reply.sheet(&sheet(&facts));
            return 0;
        }
        let _ = reply.bytes(LOG_STDOUT, pretty(&facts).as_bytes());
        0
    }
}

/// formatInfo (docker/cli system/info.go): `format` (or `json`) run against the info as
/// Go types, then a newline; a template that does not parse exits 64.
fn formatted(format: &str, f: &Facts, reply: &Reply<'_>) -> u8 {
    let text = if format == "json" { "{{json .}}" } else { format };
    let t = match shards_template::Template::parse("", text) {
        Ok(t) => t,
        Err(e) => {
            reply.err(&format!("template parsing error: {e}"));
            return 64;
        }
    };
    let mut out = String::new();
    let failed = t.execute_into(&value(f), &mut out).err();
    out.push('\n');
    let _ = reply.bytes(LOG_STDOUT, out.as_bytes());
    match failed {
        Some(e) => {
            reply.err(&e);
            1
        }
        None => 0,
    }
}

/// The info as docker/cli's dockerInfo holds it (system.Info, embedded, then the
/// client's): shards' values, and Go's zero values for what shards has nothing of the
/// kind of (cgroups, containerd, runc, swarm).
fn value(f: &Facts) -> shards_template::Value {
    use shards_template::{Kind, Struct, Value};
    let s = |v: &str| Value::String(v.to_owned());
    let int = |n: usize| Value::Int(i64::try_from(n).unwrap_or(i64::MAX));
    let strs = |l: &[&str]| Value::strings(l.iter().copied());
    let env = |name: &str| {
        std::env::var(name)
            .or_else(|_| std::env::var(name.to_lowercase()))
            .unwrap_or_default()
    };
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    let commit = || Struct::new("system.Commit").field("ID", s("")).value();
    let index = Struct::pointer("registry.IndexInfo")
        .field("Name", s("docker.io"))
        .field("Mirrors", strs(&[]))
        .field("Secure", Value::Bool(true))
        .field("Official", Value::Bool(true))
        .value();
    let registry = Struct::pointer("registry.ServiceConfig")
        .field("InsecureRegistryCIDRs", strs(&["::1/128", "127.0.0.0/8"]))
        .field(
            "IndexConfigs",
            Value::Map(Kind::Any, [("docker.io".to_owned(), index)].into_iter().collect()),
        )
        .field("Mirrors", strs(&[]))
        .value();
    let runtime = Struct::new("system.RuntimeWithStatus")
        .tagged("Path", Some("path"), true, s("shards"))
        .tagged("Args", Some("runtimeArgs"), true, Value::NilList(Kind::String))
        .tagged("Type", Some("runtimeType"), true, s(""))
        .tagged("Options", Some("options"), true, Value::NilMap(Kind::Any))
        .tagged("Status", Some("status"), true, Value::NilMap(Kind::String))
        .value();
    let swarm = Struct::new("swarm.Info")
        .field("NodeID", s(""))
        .field("NodeAddr", s(""))
        .field("LocalNodeState", s("inactive"))
        .field("ControlAvailable", Value::Bool(false))
        .field("Error", s(""))
        .field("RemoteManagers", Value::NilList(Kind::Any))
        .tagged("Nodes", Some("Nodes"), true, Value::Int(0))
        .tagged("Managers", Some("Managers"), true, Value::Int(0))
        .tagged("Cluster", Some("Cluster"), true, Struct::nil("swarm.ClusterInfo"))
        .tagged("Warnings", Some("Warnings"), true, Value::NilList(Kind::String))
        .value();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let client = Struct::pointer("system.clientInfo")
        .field("Debug", Value::Bool(false))
        .tagged(
            "Platform",
            Some("Platform"),
            true,
            Struct::nil("system.platformInfo"),
        )
        .tagged("Version", Some("Version"), true, s(env!("CARGO_PKG_VERSION")))
        .tagged("APIVersion", Some("ApiVersion"), true, s("none"))
        .tagged("DefaultAPIVersion", Some("DefaultAPIVersion"), true, s("none"))
        .tagged(
            "GitCommit",
            Some("GitCommit"),
            true,
            s(option_env!("SHARDS_GIT_COMMIT").unwrap_or("unknown-commit")),
        )
        .tagged(
            "GoVersion",
            Some("GoVersion"),
            true,
            s(option_env!("SHARDS_RUSTC").unwrap_or_default()),
        )
        .tagged("Os", Some("Os"), true, s(os))
        .tagged("Arch", Some("Arch"), true, s(arch))
        .tagged(
            "BuildTime",
            Some("BuildTime"),
            true,
            s(option_env!("SHARDS_BUILD_TIME").unwrap_or("unknown-buildtime")),
        )
        .field("Context", s("default"))
        .field("Plugins", Value::list(Vec::new()))
        .field("Warnings", Value::NilList(Kind::String))
        .value();
    Struct::new("system.dockerInfo")
        .field("ID", s(""))
        .field("Containers", int(f.containers))
        .field("ContainersRunning", int(f.running))
        .field("ContainersPaused", int(f.paused))
        .field("ContainersStopped", int(f.stopped))
        .field("Images", int(f.images))
        .field("Driver", s("erofs"))
        .field(
            "DriverStatus",
            Value::list(vec![Value::strings(["driver-type", "shards microVM templates"])]),
        )
        .tagged(
            "SystemStatus",
            Some("SystemStatus"),
            true,
            Value::NilList(Kind::Any),
        )
        .field(
            "Plugins",
            Struct::new("system.PluginsInfo")
                .field("Volume", Value::NilList(Kind::String))
                .field("Network", strs(&["bridge", "none"]))
                .field("Authorization", Value::NilList(Kind::String))
                .field("Log", strs(&["shards"]))
                .value(),
        )
        .field("MemoryLimit", Value::Bool(false))
        .field("SwapLimit", Value::Bool(false))
        .tagged("CPUCfsPeriod", Some("CpuCfsPeriod"), false, Value::Bool(false))
        .tagged("CPUCfsQuota", Some("CpuCfsQuota"), false, Value::Bool(false))
        .field("CPUShares", Value::Bool(false))
        .field("CPUSet", Value::Bool(false))
        .field("PidsLimit", Value::Bool(false))
        .field("IPv4Forwarding", Value::Bool(true))
        .field("Debug", Value::Bool(false))
        .field("NFd", int(open_descriptors()))
        .field("OomKillDisable", Value::Bool(false))
        .field("NGoroutines", int(0))
        .field("SystemTime", s(&super::inspect_doc::go_time(Some(now))))
        .field("LoggingDriver", s("shards"))
        .field("CgroupDriver", s("none"))
        .tagged("CgroupVersion", Some("CgroupVersion"), true, s(""))
        .field("NEventsListener", int(0))
        .field("KernelVersion", s(f.kernel.as_deref().unwrap_or_default()))
        .field("OperatingSystem", s(f.os.as_deref().unwrap_or_default()))
        .field(
            "OSVersion",
            s(f.os
                .as_deref()
                .and_then(|o| o.rsplit_once(' '))
                .map_or("", |(_, v)| v)),
        )
        .field("OSType", s("linux"))
        .field("Architecture", s(std::env::consts::ARCH))
        .field("IndexServerAddress", s("https://index.docker.io/v1/"))
        .field("RegistryConfig", registry)
        .field("NCPU", int(f.cpus))
        .field(
            "MemTotal",
            Value::Int(f.memory.map_or(0, |m| i64::try_from(m).unwrap_or(i64::MAX))),
        )
        .field("GenericResources", Value::NilList(Kind::Any))
        .field("DockerRootDir", s(&f.root))
        .tagged("HTTPProxy", Some("HttpProxy"), false, s(&env("HTTP_PROXY")))
        .tagged("HTTPSProxy", Some("HttpsProxy"), false, s(&env("HTTPS_PROXY")))
        .field("NoProxy", s(&env("NO_PROXY")))
        .field("Name", s(f.name.as_deref().unwrap_or_default()))
        .field("Labels", strs(&[]))
        .field("ExperimentalBuild", Value::Bool(false))
        .field("ServerVersion", s(env!("CARGO_PKG_VERSION")))
        .field(
            "Runtimes",
            Value::Map(Kind::Any, [("shards".to_owned(), runtime)].into_iter().collect()),
        )
        .field("DefaultRuntime", s("shards"))
        .field("Swarm", swarm)
        .field("LiveRestoreEnabled", Value::Bool(false))
        .field("Isolation", s(""))
        .field("InitBinary", s("shards-init"))
        .field("ContainerdCommit", commit())
        .field("RuncCommit", commit())
        .field("InitCommit", commit())
        .field("SecurityOptions", strs(&[]))
        .tagged("ProductLicense", Some("ProductLicense"), true, s(""))
        .tagged(
            "DefaultAddressPools",
            Some("DefaultAddressPools"),
            true,
            Value::NilList(Kind::Any),
        )
        .tagged(
            "FirewallBackend",
            Some("FirewallBackend"),
            true,
            Struct::nil("system.FirewallInfo"),
        )
        .field("CDISpecDirs", strs(&[]))
        .tagged(
            "DiscoveredDevices",
            Some("DiscoveredDevices"),
            true,
            Value::NilList(Kind::Any),
        )
        .tagged("NRI", Some("NRI"), true, Struct::nil("system.NRIInfo"))
        .tagged(
            "Containerd",
            Some("Containerd"),
            true,
            Struct::nil("system.ContainerdInfo"),
        )
        .field("Warnings", Value::NilList(Kind::String))
        .tagged(
            "ServerErrors",
            Some("ServerErrors"),
            true,
            Value::NilList(Kind::String),
        )
        .tagged("UserName", None, false, s(""))
        .tagged("ClientInfo", Some("ClientInfo"), true, client)
        .tagged(
            "ClientErrors",
            Some("ClientErrors"),
            true,
            Value::NilList(Kind::String),
        )
        .value()
}

/// The descriptors this process has open, as dockerd counts its own (NFd).
fn open_descriptors() -> usize {
    let dir = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    std::fs::read_dir(dir).map_or(0, |d| d.count().saturating_sub(1))
}

/// docker/cli's layout of client and server (prettyPrintInfo, prettyPrintClientInfo,
/// prettyPrintServerInfo): `fmt.Fprintln`'s spaces between operands, a section's lines
/// one space in, theirs two, a blank line after each section.
fn pretty(f: &Facts) -> String {
    let mut out = String::new();
    let mut line = |text: String| {
        out.push_str(&text);
        out.push('\n');
    };
    line("Client:".into());
    line(format!(" Version:    {}", env!("CARGO_PKG_VERSION")));
    line(" Context:    default".into());
    line(" Debug Mode: false".into());
    line(String::new());
    line("Server:".into());
    line(format!(" Containers: {}", f.containers));
    line(format!("  Running: {}", f.running));
    line(format!("  Paused: {}", f.paused));
    line(format!("  Stopped: {}", f.stopped));
    line(format!(" Images: {}", f.images));
    line(format!(" Server Version: {}", env!("CARGO_PKG_VERSION")));
    // Each image a microVM's read-only EROFS disk, its template a memory snapshot.
    line(" Storage Driver: erofs".into());
    line("  driver-type: shards microVM templates".into());
    line(" Logging Driver: shards".into());
    line(" Plugins:".into());
    line("  Volume: ".into());
    line("  Network: bridge none".into());
    line("  Log: shards".into());
    line(" Swarm: inactive".into());
    line(" Runtimes: shards".into());
    line(" Default Runtime: shards".into());
    line(" Init Binary: shards-init".into());
    let mut put = |label: &str, value: Option<&str>| {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            let _ = writeln!(out, " {label}: {v}");
        }
    };
    put("Kernel Version", f.kernel.as_deref());
    put("Operating System", f.os.as_deref());
    put("OSType", Some("linux"));
    put("Architecture", Some(std::env::consts::ARCH));
    let _ = writeln!(out, " CPUs: {}", f.cpus);
    if let Some(m) = f.memory {
        let _ = writeln!(out, " Total Memory: {}", super::commands::binary_size(m));
    }
    let mut put = |label: &str, value: Option<&str>| {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            let _ = writeln!(out, " {label}: {v}");
        }
    };
    put("Name", f.name.as_deref());
    let _ = writeln!(out, " Docker Root Dir: {}", f.root);
    out.push_str(" Debug Mode: false\n Experimental: false\n Live Restore Enabled: false\n\n");
    out
}

/// The facts for a client on a colour terminal, by section.
fn sheet(f: &Facts) -> shards_ipc::Sheet {
    let mut sheet = shards_ipc::Sheet::new("info");
    let mut put = |section: &str, key: &str, value: String| {
        sheet.record(&[("section", section.into()), ("key", key.into()), ("value", value)]);
    };
    put("microVMs", "running", f.running.to_string());
    put("microVMs", "paused", f.paused.to_string());
    put("microVMs", "stopped", f.stopped.to_string());
    put("images", "images", f.images.to_string());
    put("images", "stored in", f.root.clone());
    put("shards", "version", env!("CARGO_PKG_VERSION").into());
    if let Some(k) = &f.kernel {
        put("shards", "guest kernel", format!("Linux {k}"));
    }
    put("shards", "guest init", "shards-init".into());
    if let Some(os) = &f.os {
        put("host", "system", os.clone());
    }
    put("host", "architecture", std::env::consts::ARCH.into());
    put("host", "CPUs", f.cpus.to_string());
    if let Some(m) = f.memory {
        put("host", "memory", super::commands::binary_size(m));
    }
    if let Some(n) = &f.name {
        put("host", "name", n.clone());
    }
    sheet
}

/// The release a kernel image says it is (`Linux version 6.18.48 …`, the banner every
/// kernel carries, init/version.c), where it is not compressed.
fn kernel_version(kernel: &Path) -> Option<String> {
    let image = std::fs::read(kernel).ok()?;
    let banner = b"Linux version ";
    let at = image.windows(banner.len()).position(|w| w == banner)? + banner.len();
    let rest = image.get(at..)?;
    let end = rest.iter().position(|&b| b == b' ' || b == 0)?;
    std::str::from_utf8(rest.get(..end)?).ok().map(str::to_string)
}

/// The host's operating system: macOS's product name and version, or os-release's
/// PRETTY_NAME, as dockerd names its own (operatingsystem.GetOperatingSystem).
fn os_name() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut buf = [0u8; 64];
        let mut len = buf.len();
        // SAFETY: sysctlbyname(3) writes at most `len` bytes into `buf`.
        let rc = unsafe {
            libc::sysctlbyname(
                c"kern.osproductversion".as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return None;
        }
        let version = std::ffi::CStr::from_bytes_until_nul(&buf).ok()?.to_str().ok()?;
        Some(format!("macOS {version}"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let text = std::fs::read_to_string("/etc/os-release").ok()?;
        let line = text.lines().find_map(|l| l.strip_prefix("PRETTY_NAME="))?;
        Some(line.trim_matches('"').to_string())
    }
}

/// The host's name.
fn host_name() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname(2) writes at most the buffer's length.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    std::str::from_utf8(buf.get(..end)?).ok().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kernels_release_is_read_from_its_banner() {
        let dir = std::env::temp_dir().join(format!("shards-banner-{}", std::process::id()));
        std::fs::write(
            &dir,
            b"\x00\x01junk Linux version 6.18.48 (builder@ci) #1 SMP\x00more",
        )
        .unwrap();
        assert_eq!(kernel_version(&dir), Some("6.18.48".to_string()));
        std::fs::write(&dir, b"no banner").unwrap();
        assert_eq!(kernel_version(&dir), None);
        let _ = std::fs::remove_file(&dir);
    }
}
