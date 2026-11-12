//! The guest's side, in a Linux microVM.
//!
//! - Arm g: `control-end guest quic --controller HEX`, a daemon run's command under
//!   shards-init: a QUIC endpoint of its own on UDP [`QUIC_PORT`], its key made here.
//! - Arm h: PID 1 of a directly booted microVM (`control_end=vsock` on the kernel's command
//!   line): a plain listener on vsock port [`VSOCK_PORT`], one thread per stream the host's
//!   endpoint carries on.
//! - Their baselines, g0 and h0: the same program and microVMs, holding no endpoint
//!   (`guest idle`, `control_end=idle`).

use std::io::Write as _;
use std::time::Duration;

use crate::proto::{self, Opts};

pub fn main(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("quic") => quic(args.get(1..).unwrap_or_default()),
        Some("idle") => idle(),
        // A first run of the image, which boots its template and stores the home's kernel.
        Some("check") => {
            let _ = writeln!(std::io::stdout(), "control-end ok");
            Ok(())
        }
        _ => Err("guest quic --controller HEX | guest idle | guest check".into()),
    }
}

/// Arm g's endpoint: says its key's SHA-256 on stdout, which its run's log keeps, then serves
/// for as long as its microVM runs.
fn quic(args: &[String]) -> Result<(), String> {
    let opts = Opts::parse(args, &["controller"])?;
    let controller = proto::fingerprint(opts.need("controller")?)?;
    let at = std::net::SocketAddr::from(([0, 0, 0, 0], proto::QUIC_PORT));
    let (mut driver, fingerprint) = bind_quic(at, controller)?;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "control-end ready port={} fingerprint={}",
        proto::QUIC_PORT,
        proto::hex(&fingerprint)
    );
    let _ = out.flush();
    drop(out);
    driver.run().map_err(|e| e.to_string())
}

/// Arm g's endpoint on `at`, its key made here, serving the controller whose key's SHA-256
/// is `controller`; and its own key's SHA-256.
pub fn bind_quic(
    at: std::net::SocketAddr,
    controller: [u8; 32],
) -> Result<(shards_remote::net::Driver<crate::serve::Serve>, [u8; 32]), String> {
    let identity = shards_remote::identity::Identity::generate().map_err(|e| e.to_string())?;
    let config = shards_remote::tls::server(&identity).map_err(|e| e.to_string())?;
    let serve = crate::serve::Serve::new(controller, process_cpu_us());
    let (driver, _sender) =
        shards_remote::net::Driver::bind(at, Some(config), serve).map_err(|e| e.to_string())?;
    Ok((driver, identity.fingerprint()))
}

/// One stream of arm h's guest, past its header: its operation answered on `stream`. Its
/// caller ends the stream's writing side after.
pub fn answer<S: std::io::Read + std::io::Write>(stream: &mut S, extra: &str) -> Result<(), String> {
    let mut head = [0u8; proto::HEADER];
    stream.read_exact(&mut head).map_err(|e| e.to_string())?;
    let header = proto::Header::decode(&head)?;
    let mut buf = vec![0u8; proto::CHUNK];
    match header.op {
        proto::ECHO => loop {
            let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                return Ok(());
            }
            stream
                .write_all(buf.get(..n).unwrap_or_default())
                .map_err(|e| e.to_string())?;
        },
        proto::UPLOAD => {
            let mut left = header.count;
            while left > 0 {
                let want = usize::try_from(left).unwrap_or(proto::CHUNK).min(proto::CHUNK);
                let n = stream
                    .read(buf.get_mut(..want).unwrap_or_default())
                    .map_err(|e| e.to_string())?;
                if n == 0 {
                    return Err(format!("the upload ended {left} bytes short"));
                }
                left = left.saturating_sub(n as u64);
            }
            stream
                .write_all(&header.count.to_be_bytes())
                .map_err(|e| e.to_string())
        }
        proto::DOWNLOAD => {
            let mut left = header.count;
            while left > 0 {
                let n = usize::try_from(left).unwrap_or(proto::CHUNK).min(proto::CHUNK);
                stream
                    .write_all(proto::ZEROS.get(..n).unwrap_or_default())
                    .map_err(|e| e.to_string())?;
                left = left.saturating_sub(n as u64);
            }
            Ok(())
        }
        _ => stream
            .write_all(stats_line(extra).as_bytes())
            .map_err(|e| e.to_string()),
    }
}

/// A baseline's program: holds nothing, does nothing, wakes once an hour.
fn idle() -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "control-end idle");
    let _ = out.flush();
    drop(out);
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// This process's CPU time, user and system, in microseconds.
pub fn process_cpu_us() -> u64 {
    #[cfg(unix)]
    {
        // SAFETY: an all-zero rusage is a valid value for getrusage(2) to fill.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: RUSAGE_SELF and a valid, writable rusage.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) } != 0 {
            return 0;
        }
        let us = |t: libc::timeval| {
            u64::try_from(t.tv_sec)
                .unwrap_or(0)
                .saturating_mul(1_000_000)
                .saturating_add(u64::try_from(t.tv_usec).unwrap_or(0))
        };
        us(usage.ru_utime).saturating_add(us(usage.ru_stime))
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// One line of what this program holds and has used, as `key=value` words: its pid, its
/// resident anonymous, file and shared memory and its proportional set size (Linux
/// proc_pid_status(5), smaps_rollup), the guest's MemTotal and MemAvailable (proc_meminfo(5)),
/// its CPU time, and `extra`.
pub fn stats_line(extra: &str) -> String {
    let field = |text: &str, name: &str| -> u64 {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
            .unwrap_or(0)
    };
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    format!(
        "pid={} rss_anon_kib={} rss_file_kib={} rss_shmem_kib={} pss_kib={} mem_total_kib={} mem_available_kib={} cpu_us={} {extra}\n",
        std::process::id(),
        field(&status, "RssAnon:"),
        field(&status, "RssFile:"),
        field(&status, "RssShmem:"),
        field(&rollup, "Pss:"),
        field(&meminfo, "MemTotal:"),
        field(&meminfo, "MemAvailable:"),
        process_cpu_us(),
    )
}

/// PID 1 of arm h's and h0's microVMs: their mode from the kernel's command line, which the
/// kernel gives init as its environment. PID 1 never returns: a failure is said on the
/// console and the guest waits to be stopped.
#[cfg(target_os = "linux")]
pub fn pid1() -> ! {
    let said = mount_pseudo().and_then(|()| match std::env::var("control_end").as_deref() {
        Ok("vsock") => linux::serve_vsock(proto::VSOCK_PORT),
        Ok("idle") => idle(),
        other => Err(format!("control_end={other:?}: neither vsock nor idle")),
    });
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "control-end: {}", said.err().unwrap_or_default());
    drop(err);
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// /dev, /proc and /sys, which a directly booted guest's initramfs leaves unmounted.
#[cfg(target_os = "linux")]
fn mount_pseudo() -> Result<(), String> {
    for (src, target, fs) in [
        ("devtmpfs", "/dev", "devtmpfs"),
        ("proc", "/proc", "proc"),
        ("sysfs", "/sys", "sysfs"),
    ] {
        let c = |s: &str| std::ffi::CString::new(s).map_err(|e| e.to_string());
        let (src, target, fs) = (c(src)?, c(target)?, c(fs)?);
        let _ = std::fs::create_dir_all(target.to_string_lossy().as_ref());
        // SAFETY: NUL-terminated strings, no data argument.
        if unsafe { libc::mount(src.as_ptr(), target.as_ptr(), fs.as_ptr(), 0, std::ptr::null()) } != 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EBUSY) {
                return Err(format!("mount {target:?}: {e}"));
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{Read as _, Write as _};
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    /// Arm h's guest: every stream the host's endpoint opens on `port`, served on a thread of
    /// its own. Says so on the console once it listens.
    pub fn serve_vsock(port: u32) -> Result<(), String> {
        // SAFETY: socket(2) with constant arguments.
        let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(format!("vsock socket: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let listener = unsafe { std::fs::File::from_raw_fd(fd) };
        // SAFETY: an all-zero sockaddr_vm is a valid value.
        let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_cid = libc::VMADDR_CID_ANY;
        addr.svm_port = port;
        let len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
        // SAFETY: a valid sockaddr_vm of `len` bytes.
        if unsafe { libc::bind(listener.as_raw_fd(), (&raw const addr).cast(), len) } != 0 {
            return Err(format!("vsock bind {port}: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: listen(2) on our own socket.
        if unsafe { libc::listen(listener.as_raw_fd(), libc::SOMAXCONN) } != 0 {
            return Err(format!("vsock listen: {}", std::io::Error::last_os_error()));
        }
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "control-end ready vsock={port}");
        let _ = out.flush();
        drop(out);
        loop {
            // SAFETY: accept4(2) on our listening socket, without the peer's address.
            let fd = unsafe {
                libc::accept4(
                    listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };
            if fd < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(format!("vsock accept: {e}"));
            }
            // SAFETY: a fresh descriptor accept4 gave this process alone.
            let stream = unsafe { std::fs::File::from_raw_fd(fd) };
            let spawned = std::thread::Builder::new()
                .name("stream".into())
                .spawn(move || {
                    let _ = serve_stream(stream);
                });
            if let Err(e) = spawned {
                let mut err = std::io::stderr().lock();
                let _ = writeln!(err, "control-end: a stream's thread: {e}");
            }
        }
    }

    /// One stream: its operation answered, then its writing side shut.
    fn serve_stream(mut stream: std::fs::File) -> Result<(), String> {
        let answered = super::answer(&mut stream, "arm=h");
        // SAFETY: shutdown(2) of our own socket's writing side: the host's endpoint reads
        // its end.
        unsafe { libc::shutdown(stream.as_raw_fd(), libc::SHUT_WR) };
        // What the endpoint still sends (its end) is read until it closes.
        let mut buf = [0u8; 4096];
        while stream.read(&mut buf).is_ok_and(|n| n > 0) {}
        answered
    }
}
