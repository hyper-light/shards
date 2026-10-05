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
    pub(super) fn info(&self, asker: &Asker, reply: &Reply<'_>) -> u8 {
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
        if asker.styled() {
            reply.sheet(&sheet(&facts));
            return 0;
        }
        let _ = reply.bytes(LOG_STDOUT, pretty(&facts).as_bytes());
        0
    }
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
