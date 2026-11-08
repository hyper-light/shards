//! What a run's `--device` costs init, inside a microVM (PM M223). Each of N rounds times,
//! in microseconds:
//!
//! - `walk`: the lookup init did (devices.rs `vm_devices`): each
//!   /sys/dev/{char,block}/M:N's uevent read for its device's numbers, name, mode and owner;
//! - `stat`: one fstatat(2) of PATH's node in a devtmpfs mounted once and held open (O_PATH);
//! - `compile`: the run's device filter, Docker's defaults and PATH's rule, compiled by
//!   shards-devcgroup as init compiles it;
//! - `load`: that program loaded (BPF_PROG_LOAD);
//! - `attach`: it attached to a cgroup of the harness's own in place of the default filter
//!   (BPF_F_REPLACE), as init attaches a run's, and the default put back;
//!
//! each printed as n, p50, p90, p99 and max. Run as root of a privileged run, which may
//! mount a devtmpfs, load programs and make a cgroup:
//!
//!     device-setup N PATH

use std::ffi::CString;
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::time::Instant;

use shards_devcgroup::{self as dc, Kind, Rule};

#[repr(C)]
#[derive(Default)]
struct ProgLoad {
    prog_type: u32,
    insn_cnt: u32,
    insns: u64,
    license: u64,
    log_level: u32,
    log_size: u32,
    log_buf: u64,
    kern_version: u32,
    prog_flags: u32,
    prog_name: [u8; 16],
    prog_ifindex: u32,
    expected_attach_type: u32,
}

#[repr(C)]
#[derive(Default)]
struct ProgAttach {
    target_fd: u32,
    attach_bpf_fd: u32,
    attach_type: u32,
    attach_flags: u32,
    replace_bpf_fd: u32,
}

const BPF_PROG_LOAD: libc::c_long = 5;
const BPF_PROG_ATTACH: libc::c_long = 8;
const BPF_PROG_TYPE_CGROUP_DEVICE: u32 = 15;
const BPF_CGROUP_DEVICE: u32 = 6;
const BPF_F_ALLOW_MULTI: u32 = 2;
const BPF_F_REPLACE: u32 = 4;

fn main() {
    if let Err(e) = run() {
        let _ = writeln!(io::stderr(), "device-setup: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let (Some(n), Some(path)) = (args.get(1).and_then(|n| n.parse::<usize>().ok()), args.get(2)) else {
        return Err("usage: device-setup N PATH".into());
    };
    let rel = path
        .strip_prefix("/dev/")
        .ok_or_else(|| format!("{path} is not under /dev"))?;
    let devtmpfs = devtmpfs()?;
    let rel = CString::new(rel).map_err(|_| "a NUL in the path")?;
    let cgroup = "/sys/fs/cgroup/device-setup";
    std::fs::create_dir_all(cgroup).map_err(|e| format!("{cgroup}: {e}"))?;
    let default = dc::compile(&dc::container(&[], &[], false))?;
    let mut default_fd = attach(&default, cgroup, None)?;

    let mut times: [Vec<f64>; 5] = Default::default();
    let mut found = 0;
    for _ in 0..n {
        let t = Instant::now();
        found = walk().len();
        times[0].push(t.elapsed().as_secs_f64() * 1e6);

        let t = Instant::now();
        // SAFETY: fstatat(2) of a NUL-terminated path under a descriptor held open, into a
        // stat buffer it fills.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::fstatat(devtmpfs.as_raw_fd(), rel.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
        times[1].push(t.elapsed().as_secs_f64() * 1e6);
        if r != 0 {
            return Err(format!("{path}: {}", io::Error::last_os_error()));
        }

        let t = Instant::now();
        let rule = Rule {
            kind: if st.st_mode & libc::S_IFMT == libc::S_IFBLK { Kind::Block } else { Kind::Char },
            major: i64::from(libc::major(st.st_rdev)),
            minor: i64::from(libc::minor(st.st_rdev)),
            perms: dc::RWM,
            allow: true,
        };
        let given = [dc::Given { path: path.clone(), rule }];
        let program = dc::compile(&dc::container(&given, &[], false))?;
        times[2].push(t.elapsed().as_secs_f64() * 1e6);

        let t = Instant::now();
        let prog = load(&program)?;
        times[3].push(t.elapsed().as_secs_f64() * 1e6);

        let t = Instant::now();
        attach_loaded(&prog, cgroup, Some(&default_fd))?;
        times[4].push(t.elapsed().as_secs_f64() * 1e6);
        // The default back in its place, off the clock.
        default_fd = attach(&default, cgroup, Some(&prog))?;
    }
    drop(default_fd);
    let _ = std::fs::remove_dir(cgroup);
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "devices walked {found}");
    for (name, v) in ["walk", "stat", "compile", "load", "attach"].iter().zip(times.iter_mut()) {
        v.sort_by(f64::total_cmp);
        let q = |p: usize| v.get((v.len() * p / 100).min(v.len().saturating_sub(1))).copied().unwrap_or(0.0);
        let _ = writeln!(
            out,
            "{name:8} n={} p50={:.1} p90={:.1} p99={:.1} max={:.1} us",
            v.len(),
            q(50),
            q(90),
            q(99),
            v.last().copied().unwrap_or(0.0)
        );
    }
    Ok(())
}

/// init's walk (devices.rs `vm_devices`): each device's uevent, its fields parsed.
fn walk() -> Vec<(String, u32, u32, u32, u32, u32)> {
    let mut out = Vec::new();
    for dir in ["/sys/dev/char", "/sys/dev/block"] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(uevent) = std::fs::read_to_string(entry.path().join("uevent")) else {
                continue;
            };
            let field = |k: &str| uevent.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('='));
            let number = |k: &str| field(k).and_then(|v| v.parse::<u32>().ok());
            let (Some(major), Some(minor), Some(name)) = (number("MAJOR"), number("MINOR"), field("DEVNAME")) else {
                continue;
            };
            out.push((
                format!("/dev/{name}"),
                major,
                minor,
                field("DEVMODE").and_then(|m| u32::from_str_radix(m, 8).ok()).unwrap_or(0o600),
                number("DEVUID").unwrap_or(0),
                number("DEVGID").unwrap_or(0),
            ));
        }
    }
    out.sort_by(|a, b| a.0.split('/').cmp(b.0.split('/')));
    out
}

/// A devtmpfs, mounted, opened (O_PATH) and detached: only the descriptor reaches it.
fn devtmpfs() -> Result<OwnedFd, String> {
    let at = "/device-setup-dev";
    std::fs::create_dir_all(at).map_err(|e| format!("{at}: {e}"))?;
    let c = CString::new(at).map_err(|_| "a NUL")?;
    // SAFETY: mount(2), open(2) and umount2(2) of NUL-terminated strings.
    unsafe {
        if libc::mount(c"devtmpfs".as_ptr(), c.as_ptr(), c"devtmpfs".as_ptr(), libc::MS_NOSUID | libc::MS_NOEXEC, std::ptr::null()) != 0 {
            return Err(format!("mounting a devtmpfs: {}", io::Error::last_os_error()));
        }
        let fd = libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC);
        let opened = io::Error::last_os_error();
        libc::umount2(c.as_ptr(), libc::MNT_DETACH);
        let _ = std::fs::remove_dir(at);
        if fd < 0 {
            return Err(format!("opening the devtmpfs: {opened}"));
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

fn load(program: &[u8]) -> Result<OwnedFd, String> {
    let license = CString::new(dc::LICENSE).map_err(|_| "a NUL in the license")?;
    let load = ProgLoad {
        prog_type: BPF_PROG_TYPE_CGROUP_DEVICE,
        insn_cnt: u32::try_from(program.len() / 8).map_err(|_| "a program too long")?,
        insns: program.as_ptr() as u64,
        license: license.as_ptr() as u64,
        ..ProgLoad::default()
    };
    // SAFETY: bpf(2) BPF_PROG_LOAD of a program and license that outlive the call.
    let prog = unsafe { libc::syscall(libc::SYS_bpf, BPF_PROG_LOAD, &raw const load, std::mem::size_of::<ProgLoad>()) };
    if prog < 0 {
        return Err(format!("load program: {}", io::Error::last_os_error()));
    }
    let prog = i32::try_from(prog).map_err(|_| "a descriptor out of range")?;
    // SAFETY: the descriptor bpf(2) just returned, ours alone.
    Ok(unsafe { OwnedFd::from_raw_fd(prog) })
}

fn attach(program: &[u8], cgroup: &str, replacing: Option<&OwnedFd>) -> Result<OwnedFd, String> {
    let prog = load(program)?;
    attach_loaded(&prog, cgroup, replacing)?;
    Ok(prog)
}

fn attach_loaded(prog: &OwnedFd, cgroup: &str, replacing: Option<&OwnedFd>) -> Result<(), String> {
    let dir = std::fs::File::open(cgroup).map_err(|e| format!("{cgroup}: {e}"))?;
    let at = ProgAttach {
        target_fd: u32::try_from(dir.as_raw_fd()).map_err(|_| "a descriptor out of range")?,
        attach_bpf_fd: u32::try_from(prog.as_raw_fd()).map_err(|_| "a descriptor out of range")?,
        attach_type: BPF_CGROUP_DEVICE,
        attach_flags: BPF_F_ALLOW_MULTI | replacing.map_or(0, |_| BPF_F_REPLACE),
        replace_bpf_fd: match replacing {
            Some(fd) => u32::try_from(fd.as_raw_fd()).map_err(|_| "a descriptor out of range")?,
            None => 0,
        },
    };
    // SAFETY: bpf(2) BPF_PROG_ATTACH of descriptors held open over the call.
    let attached = unsafe { libc::syscall(libc::SYS_bpf, BPF_PROG_ATTACH, &raw const at, std::mem::size_of::<ProgAttach>()) };
    if attached != 0 {
        return Err(format!("BPF_PROG_ATTACH: {}", io::Error::last_os_error()));
    }
    Ok(())
}
