//! What a workload's namespaces are given before it starts (shards_abi::run::Spec::setup),
//! as runc v1.5.1 gives a container's: its tmpfs mounts and `/dev/shm` (libcontainer
//! rootfs_linux.go, after specconv's parseMountOptions), its rlimits (setupRlimits), the
//! root read-only (`readonly`), done by the standby in its own mount namespace; and its
//! sysctls (internal/sys WriteSysctls), done by init through a `/proc/sys` it opened
//! before `/proc/sys` was made read-only, as runc writes through an unmasked procfs.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::sync::OnceLock;

/// `/proc/sys`, opened before it is made read-only (run.rs `masked`).
static PROC_SYS: OnceLock<OwnedFd> = OnceLock::new();

/// Opens `/proc/sys` for [`write_sysctl`], before it is made read-only.
pub fn keep_proc_sys() -> io::Result<()> {
    // SAFETY: open(2) of a NUL-terminated literal.
    let fd = unsafe {
        libc::open(
            c"/proc/sys".as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    let _ = PROC_SYS.set(unsafe { OwnedFd::from_raw_fd(fd) });
    Ok(())
}

fn errno_words(e: &io::Error) -> String {
    shards_cmdline::go::linux_error(e.raw_os_error().unwrap_or(libc::EIO))
}

/// Sets sysctl `key` to `value`, in runc's words when it cannot.
pub fn write_sysctl(key: &str, value: &str) -> Result<(), String> {
    let path = key.replace('.', "/");
    let shown = format!("/proc/sys/{path}");
    let dir = PROC_SYS
        .get()
        .ok_or_else(|| format!("open sysctl {key} file: open {shown}: no such file or directory"))?;
    let c = CString::new(path).map_err(|_| format!("open sysctl {key} file: a NUL in its name"))?;
    // SAFETY: openat(2) of a NUL-terminated path below our own descriptor.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c.as_ptr(),
            libc::O_WRONLY | libc::O_TRUNC | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let e = io::Error::last_os_error();
        return Err(format!(
            "open sysctl {key} file: open {shown}: {}",
            errno_words(&e)
        ));
    }
    // SAFETY: a fresh descriptor nothing else owns.
    let file = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: write(2) of a live buffer, of its length.
    let n = unsafe { libc::write(file.as_raw_fd(), value.as_ptr().cast(), value.len()) };
    if n < 0 {
        let e = io::Error::last_os_error();
        return Err(format!(
            "failed to write sysctl {key} = {}: write {shown}: {}",
            shards_cmdline::go::quote(value),
            errno_words(&e)
        ));
    }
    Ok(())
}

/// A tmpfs's options as runc's parseMountOptions reads them: mount flags, propagation
/// flags, and the rest as the filesystem's data.
pub struct MountOptions {
    pub flags: libc::c_ulong,
    pub propagation: Vec<libc::c_ulong>,
    pub data: String,
}

/// runc's mountFlags and mountPropagationMapping, for what MergeTmpfsOptions leaves.
pub fn mount_options(options: &str) -> MountOptions {
    let mut m = MountOptions {
        flags: 0,
        propagation: Vec::new(),
        data: String::new(),
    };
    let mut data: Vec<&str> = Vec::new();
    for o in options.split(',').filter(|o| !o.is_empty()) {
        let flag = |clear: bool, f: libc::c_ulong| Some((clear, f));
        let set = match o {
            "async" => flag(true, libc::MS_SYNCHRONOUS),
            "atime" => flag(true, libc::MS_NOATIME),
            "bind" => flag(false, libc::MS_BIND),
            "dev" => flag(true, libc::MS_NODEV),
            "diratime" => flag(true, libc::MS_NODIRATIME),
            "dirsync" => flag(false, libc::MS_DIRSYNC),
            "exec" => flag(true, libc::MS_NOEXEC),
            "mand" => flag(false, libc::MS_MANDLOCK),
            "noatime" => flag(false, libc::MS_NOATIME),
            "nodev" => flag(false, libc::MS_NODEV),
            "nodiratime" => flag(false, libc::MS_NODIRATIME),
            "noexec" => flag(false, libc::MS_NOEXEC),
            "nomand" => flag(true, libc::MS_MANDLOCK),
            "norelatime" => flag(true, libc::MS_RELATIME),
            "nostrictatime" => flag(true, libc::MS_STRICTATIME),
            "nosuid" => flag(false, libc::MS_NOSUID),
            "rbind" => flag(false, libc::MS_BIND | libc::MS_REC),
            "relatime" => flag(false, libc::MS_RELATIME),
            "remount" => flag(false, libc::MS_REMOUNT),
            "ro" => flag(false, libc::MS_RDONLY),
            "rw" => flag(true, libc::MS_RDONLY),
            "strictatime" => flag(false, libc::MS_STRICTATIME),
            "suid" => flag(true, libc::MS_NOSUID),
            "sync" => flag(false, libc::MS_SYNCHRONOUS),
            _ => None,
        };
        let propagation = match o {
            "rprivate" => Some(libc::MS_PRIVATE | libc::MS_REC),
            "private" => Some(libc::MS_PRIVATE),
            "rslave" => Some(libc::MS_SLAVE | libc::MS_REC),
            "slave" => Some(libc::MS_SLAVE),
            "rshared" => Some(libc::MS_SHARED | libc::MS_REC),
            "shared" => Some(libc::MS_SHARED),
            "runbindable" => Some(libc::MS_UNBINDABLE | libc::MS_REC),
            "unbindable" => Some(libc::MS_UNBINDABLE),
            _ => None,
        };
        match (set, propagation) {
            (Some((true, f)), _) => m.flags &= !f,
            (Some((false, f)), _) => m.flags |= f,
            (None, Some(p)) => m.propagation.push(p),
            (None, None) => data.push(o),
        }
    }
    m.data = data.join(",");
    m
}

/// runc's stringifyMountFlags.
fn flag_names(flags: libc::c_ulong) -> String {
    let names: [(&str, libc::c_ulong); 22] = [
        ("MS_RDONLY", libc::MS_RDONLY),
        ("MS_NOSUID", libc::MS_NOSUID),
        ("MS_NODEV", libc::MS_NODEV),
        ("MS_NOEXEC", libc::MS_NOEXEC),
        ("MS_SYNCHRONOUS", libc::MS_SYNCHRONOUS),
        ("MS_REMOUNT", libc::MS_REMOUNT),
        ("MS_MANDLOCK", libc::MS_MANDLOCK),
        ("MS_DIRSYNC", libc::MS_DIRSYNC),
        ("MS_NOSYMFOLLOW", 256),
        ("MS_NOATIME", libc::MS_NOATIME),
        ("MS_NODIRATIME", libc::MS_NODIRATIME),
        ("MS_BIND", libc::MS_BIND),
        ("MS_MOVE", libc::MS_MOVE),
        ("MS_REC", libc::MS_REC),
        ("MS_SILENT", libc::MS_SILENT),
        ("MS_POSIXACL", libc::MS_POSIXACL),
        ("MS_UNBINDABLE", libc::MS_UNBINDABLE),
        ("MS_PRIVATE", libc::MS_PRIVATE),
        ("MS_SLAVE", libc::MS_SLAVE),
        ("MS_SHARED", libc::MS_SHARED),
        ("MS_RELATIME", libc::MS_RELATIME),
        ("MS_STRICTATIME", libc::MS_STRICTATIME),
    ];
    let set: Vec<&str> = names
        .iter()
        .filter(|(_, b)| flags & b == *b)
        .map(|(n, _)| *n)
        .collect();
    set.join("|")
}

/// An rlimit's resource as each C library types it.
#[cfg(target_env = "gnu")]
type Resource = libc::__rlimit_resource_t;
#[cfg(not(target_env = "gnu"))]
type Resource = libc::c_int;

/// An rlimit's resource number, by go-units' name.
pub fn rlimit(name: &str) -> Option<Resource> {
    Some(match name {
        "core" => libc::RLIMIT_CORE,
        "cpu" => libc::RLIMIT_CPU,
        "data" => libc::RLIMIT_DATA,
        "fsize" => libc::RLIMIT_FSIZE,
        "locks" => libc::RLIMIT_LOCKS,
        "memlock" => libc::RLIMIT_MEMLOCK,
        "msgqueue" => libc::RLIMIT_MSGQUEUE,
        "nice" => libc::RLIMIT_NICE,
        "nofile" => libc::RLIMIT_NOFILE,
        "nproc" => libc::RLIMIT_NPROC,
        "rss" => libc::RLIMIT_RSS,
        "rtprio" => libc::RLIMIT_RTPRIO,
        "rttime" => libc::RLIMIT_RTTIME,
        "sigpending" => libc::RLIMIT_SIGPENDING,
        "stack" => libc::RLIMIT_STACK,
        _ => return None,
    })
}

/// `name=soft:hard`'s resource and limits, -1 as unlimited.
fn ulimit(spec: &str) -> Option<(Resource, libc::rlimit)> {
    let (name, limits) = spec.split_once('=')?;
    let (soft, hard) = limits.split_once(':')?;
    let limit = |s: &str| -> Option<libc::rlim_t> {
        let n: i64 = s.parse().ok()?;
        // runc takes the uint64 of the int64 (moby withRlimits): -1 is RLIM_INFINITY.
        #[allow(clippy::cast_sign_loss)]
        Some(n as libc::rlim_t)
    };
    Some((
        rlimit(name)?,
        libc::rlimit {
            rlim_cur: limit(soft)?,
            rlim_max: limit(hard)?,
        },
    ))
}

fn c(s: &str) -> Option<CString> {
    CString::new(s).ok()
}

/// Makes `path` and its parents, as runc's MkdirAllInRoot makes a mount's destination.
fn mkdir_all(path: &str) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(path)
}

/// Applies setup `entry` in this process's namespaces: in the standby, the single-threaded
/// fork of init, before it execs. The errno of what failed.
pub fn apply(entry: &[u8]) -> Result<(), i32> {
    let last = || io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
    // The process's own, done last, as it execs (run.rs `child`).
    if entry.starts_with(b"seccomp=") || entry == b"nnp" {
        return Ok(());
    }
    // Init's, for the domains it starts (D59), and the volumes it gives them alone (D111).
    if entry.starts_with(b"domains-seccomp=")
        || entry.starts_with(b"domains-seccomp-none=")
        || entry.starts_with(b"domain-volume=")
    {
        return Ok(());
    }
    let text = std::str::from_utf8(entry).map_err(|_| libc::EINVAL)?;
    let mount = |src: &str, dst: &str, fstype: &str, flags: libc::c_ulong, data: &str| -> Result<(), i32> {
        let (src, dst, fstype, data) = (
            c(src).ok_or(libc::EINVAL)?,
            c(dst).ok_or(libc::EINVAL)?,
            c(fstype).ok_or(libc::EINVAL)?,
            c(data).ok_or(libc::EINVAL)?,
        );
        let data_ptr = if data.as_bytes().is_empty() {
            std::ptr::null()
        } else {
            data.as_ptr().cast()
        };
        // SAFETY: mount(2) on NUL-terminated strings that outlive the call.
        if unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), fstype.as_ptr(), flags, data_ptr) } != 0 {
            return Err(last());
        }
        Ok(())
    };
    if let Some(rest) = text.strip_prefix("tmpfs=") {
        let (dest, options) = rest.split_once('\0').ok_or(libc::EINVAL)?;
        mkdir_all(dest).map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
        let m = mount_options(options);
        mount("tmpfs", dest, "tmpfs", m.flags, &m.data)?;
        for p in m.propagation {
            mount("", dest, "", p, "")?;
        }
    } else if let Some(size) = text.strip_prefix("shm=") {
        let flags = libc::MS_REMOUNT | libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV;
        mount(
            "shm",
            "/dev/shm",
            "tmpfs",
            flags,
            &format!("mode=1777,size={size}"),
        )?;
    } else if let Some(spec) = text.strip_prefix("ulimit=") {
        let (resource, limit) = ulimit(spec).ok_or(libc::EINVAL)?;
        // SAFETY: setrlimit(2) of a resource with a limit on our stack.
        if unsafe { libc::setrlimit(resource, &limit) } != 0 {
            return Err(last());
        }
    } else if let Some(rest) = text.strip_prefix("volume=") {
        volume(rest).map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
    } else if let Some(n) = text.strip_prefix("oom=") {
        // runc sets the container's process's own (setupOOMScoreAdj, before exec).
        std::fs::write("/proc/self/oom_score_adj", n).map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
    } else if text == "privileged" {
        privileged().map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
    } else if text == "unmasked" {
        // `--security-opt systempaths=unconfined`: none masked or made read-only.
        unmask();
    } else if text == "cgroups-rw" {
        // `--security-opt writable-cgroups=true`: the cgroup hierarchy writable.
        let flags = libc::MS_REMOUNT | libc::MS_BIND | libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV;
        mount("", "/sys/fs/cgroup", "", flags, "")?;
    } else if text == "readonly" {
        // runc's remount of the root: this mount namespace's, read-only.
        mount(
            "",
            "/",
            "",
            libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY,
            "",
        )?;
    }
    Ok(())
}

/// Mounts a shared directory (D38), `TAG\0DEST\0FLAGS\0NAME`: by its virtio-fs tag at
/// DEST, read-only with `ro`; with `copy`, the image's files at DEST copied into it first,
/// where it is empty (moby daemon/create_unix.go populateVolume); with a NAME, only that
/// file of it, bound at DEST, as a file bind mount is.
fn volume(spec: &str) -> io::Result<()> {
    let mut parts = spec.split('\0');
    let (Some(tag), Some(dest), Some(flags), Some(name)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    };
    let file = (!name.is_empty()).then_some(name);
    let ro = flags.split(',').any(|f| f == "ro");
    let copy = flags.split(',').any(|f| f == "copy");
    let base = if ro { libc::MS_RDONLY } else { 0 };
    let mount = |src: &str, dst: &str, fstype: Option<&str>, flags: libc::c_ulong| -> io::Result<()> {
        let (src, dst) = (
            c(src).ok_or(io::ErrorKind::InvalidInput)?,
            c(dst).ok_or(io::ErrorKind::InvalidInput)?,
        );
        let fstype = fstype.and_then(c);
        let ty = fstype.as_ref().map_or(std::ptr::null(), |t| t.as_ptr());
        // SAFETY: mount(2) of NUL-terminated strings that outlive the call.
        if unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), ty, flags, std::ptr::null()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    };
    let staging = format!("/dev/.shards-{tag}");
    match file {
        Some(name) => {
            std::fs::create_dir_all(&staging)?;
            mount(tag, &staging, Some("virtiofs"), base)?;
            if let Some(parent) = std::path::Path::new(dest).parent() {
                mkdir_all(&parent.to_string_lossy())?;
            }
            if std::fs::symlink_metadata(dest).is_err() {
                std::fs::File::create(dest)?;
            }
            mount(&format!("{staging}/{name}"), dest, None, libc::MS_BIND)?;
            if ro {
                mount("", dest, None, libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY)?;
            }
            detach(&staging);
            let _ = std::fs::remove_dir(&staging);
        }
        None if copy => {
            mkdir_all(dest)?;
            std::fs::create_dir_all(&staging)?;
            mount(tag, &staging, Some("virtiofs"), 0)?;
            let empty = std::fs::read_dir(&staging)?.next().is_none();
            if empty {
                copy_tree(std::path::Path::new(dest), std::path::Path::new(&staging))?;
            }
            mount(&staging, dest, None, libc::MS_MOVE)?;
            if ro {
                mount("", dest, None, libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY)?;
            }
            let _ = std::fs::remove_dir(&staging);
        }
        None => {
            mkdir_all(dest)?;
            mount(tag, dest, Some("virtiofs"), base)?;
        }
    }
    Ok(())
}

fn detach(path: &str) {
    if let Some(p) = c(path) {
        // SAFETY: umount2(2) of a NUL-terminated path.
        unsafe { libc::umount2(p.as_ptr(), libc::MNT_DETACH) };
    }
}

/// Copies directory `src`'s contents into `dst`, and its own owner, mode and times, as
/// continuity's CopyDir does (fs/copy.go): directories, files, symlinks, hard links as
/// links, devices and FIFOs, each with its owner, mode, times and extended attributes,
/// those `dst` does not take left.
pub fn copy_tree(src: &std::path::Path, dst: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let mut links: std::collections::HashMap<(u64, u64), std::path::PathBuf> =
        std::collections::HashMap::new();
    let meta = std::fs::symlink_metadata(src)?;
    copy_tree_into(src, dst, &mut links)?;
    std::os::unix::fs::lchown(dst, Some(meta.uid()), Some(meta.gid()))?;
    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    copy_times(&meta, dst);
    Ok(())
}

fn copy_tree_into(
    src: &std::path::Path,
    dst: &std::path::Path,
    links: &mut std::collections::HashMap<(u64, u64), std::path::PathBuf>,
) -> io::Result<()> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        let meta = std::fs::symlink_metadata(&from)?;
        let kind = meta.file_type();
        if kind.is_dir() {
            std::fs::create_dir(&to)?;
            copy_tree_into(&from, &to, links)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&from)?, &to)?;
        } else if meta.nlink() > 1
            && let Some(first) = links.get(&(meta.dev(), meta.ino()))
        {
            std::fs::hard_link(first, &to)?;
            continue;
        } else if kind.is_file() {
            std::fs::copy(&from, &to)?;
            if meta.nlink() > 1 {
                links.insert((meta.dev(), meta.ino()), to.clone());
            }
        } else if kind.is_fifo() || kind.is_char_device() || kind.is_block_device() || kind.is_socket() {
            let p = c(&to.to_string_lossy()).ok_or(io::ErrorKind::InvalidInput)?;
            // SAFETY: mknod(2) of a NUL-terminated path, with the source's type and number.
            if unsafe {
                libc::mknod(
                    p.as_ptr(),
                    meta.mode() as libc::mode_t,
                    meta.rdev() as libc::dev_t,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        copy_xattrs(&from, &to);
        std::os::unix::fs::lchown(&to, Some(meta.uid()), Some(meta.gid()))?;
        if !kind.is_symlink() {
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(meta.mode() & 0o7777))?;
        }
        copy_times(&meta, &to);
    }
    Ok(())
}

fn copy_times(meta: &std::fs::Metadata, to: &std::path::Path) {
    use std::os::unix::fs::MetadataExt as _;
    let times = [
        libc::timespec {
            tv_sec: meta.atime() as _,
            tv_nsec: meta.atime_nsec() as _,
        },
        libc::timespec {
            tv_sec: meta.mtime() as _,
            tv_nsec: meta.mtime_nsec() as _,
        },
    ];
    if let Some(p) = c(&to.to_string_lossy()) {
        // SAFETY: utimensat(2) of a NUL-terminated path with two times, not following it.
        unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                p.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
    }
}

/// The extended attributes of `from` on `to`, those it takes (ignoreUnsupportedXAttrs).
fn copy_xattrs(from: &std::path::Path, to: &std::path::Path) {
    let (Some(f), Some(t)) = (c(&from.to_string_lossy()), c(&to.to_string_lossy())) else {
        return;
    };
    let mut names = vec![0u8; 4096];
    // SAFETY: llistxattr(2) into a buffer of its length.
    let n = unsafe { libc::llistxattr(f.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    let Ok(n) = usize::try_from(n) else {
        return;
    };
    for name in names
        .get(..n)
        .unwrap_or_default()
        .split(|&b| b == 0)
        .filter(|n| !n.is_empty())
    {
        let Ok(name) = CString::new(name) else {
            continue;
        };
        let mut value = vec![0u8; 65536];
        // SAFETY: lgetxattr(2) into a buffer of its length.
        let len =
            unsafe { libc::lgetxattr(f.as_ptr(), name.as_ptr(), value.as_mut_ptr().cast(), value.len()) };
        let Ok(len) = usize::try_from(len) else {
            continue;
        };
        // SAFETY: lsetxattr(2) of a buffer of `len` bytes.
        unsafe { libc::lsetxattr(t.as_ptr(), name.as_ptr(), value.as_ptr().cast(), len, 0) };
    }
}

/// The masked and read-only paths, undone (each mount over one detached).
fn unmask() {
    for p in crate::defaults::MASKED
        .iter()
        .chain(crate::defaults::READONLY.iter())
    {
        if let Some(c) = c(p) {
            // SAFETY: umount2(2) of a NUL-terminated path; one not mounted is left.
            unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
        }
    }
}

/// The seccomp filter a setup names (`seccomp=`: its seccomp(2) flags, then its program's
/// instructions as `struct sock_filter`s), if it names one.
pub fn filter(entries: &[Vec<u8>]) -> Option<(u32, Vec<libc::sock_filter>)> {
    filter_named(entries, b"seccomp=")
}

/// The filter of the entry `name` names: `domains-seccomp=`, the domains' (D59).
pub fn filter_named(entries: &[Vec<u8>], name: &[u8]) -> Option<(u32, Vec<libc::sock_filter>)> {
    let raw = entries.iter().find_map(|e| e.strip_prefix(name))?;
    let (flags, insns) = raw.split_first_chunk::<4>()?;
    let program = insns
        .as_chunks::<8>()
        .0
        .iter()
        .map(|&[c0, c1, jt, jf, k0, k1, k2, k3]| libc::sock_filter {
            code: u16::from_ne_bytes([c0, c1]),
            jt,
            jf,
            k: u32::from_ne_bytes([k0, k1, k2, k3]),
        })
        .collect();
    Some((u32::from_le_bytes(*flags), program))
}

/// What a privileged container has of the host's, of the VM's (moby daemon/oci_linux.go,
/// WithDevices, WithMounts and masked and read-only paths left out): no masked or
/// read-only paths, `/sys` and its cgroup writable, and every device the VM has, made in
/// `/dev` as the VM has them (devices.rs), as runc makes a host's devices.
fn privileged() -> io::Result<()> {
    unmask();
    let remount = |path: &std::ffi::CStr, flags: libc::c_ulong| -> io::Result<()> {
        // SAFETY: mount(2) remounting a NUL-terminated path.
        if unsafe {
            libc::mount(
                std::ptr::null(),
                path.as_ptr(),
                std::ptr::null(),
                flags,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    };
    let base = libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV;
    // sysfs is read-only as mounted, its superblock and its mount both.
    remount(c"/sys", libc::MS_REMOUNT | base)?;
    remount(c"/sys", libc::MS_REMOUNT | libc::MS_BIND | base)?;
    remount(c"/sys/fs/cgroup", libc::MS_REMOUNT | libc::MS_BIND | base)?;
    for n in crate::devices::vm_devices() {
        if std::fs::symlink_metadata(&n.path).is_ok() {
            continue;
        }
        if let Some(parent) = std::path::Path::new(&n.path).parent() {
            let _ = mkdir_all(&parent.to_string_lossy());
        }
        let Some(p) = c(&n.path) else {
            continue;
        };
        let kind = match n.kind {
            shards_devcgroup::Kind::Block => libc::S_IFBLK,
            _ => libc::S_IFCHR,
        };
        // SAFETY: mknod(2), chmod(2) and lchown(2) of a NUL-terminated path.
        unsafe {
            if libc::mknod(p.as_ptr(), kind | n.mode, libc::makedev(n.major, n.minor)) == 0 {
                libc::chmod(p.as_ptr(), n.mode);
                libc::lchown(p.as_ptr(), n.uid, n.gid);
            }
        }
    }
    Ok(())
}

/// What runc says when setup `entry` fails with `errno`.
pub fn failed(entry: &[u8], errno: i32) -> String {
    let err = shards_cmdline::go::linux_error(errno);
    let text = String::from_utf8_lossy(entry);
    if let Some(rest) = text.strip_prefix("tmpfs=") {
        let (dest, options) = rest.split_once('\0').unwrap_or((rest, ""));
        let m = mount_options(options);
        let mut said =
            format!("error mounting \"tmpfs\" to rootfs at \"{dest}\": mount src=tmpfs, dst={dest}");
        if m.flags != 0 {
            said.push_str(&format!(", flags={}", flag_names(m.flags)));
        }
        if !m.data.is_empty() {
            said.push_str(&format!(", data={}", m.data));
        }
        return format!("{said}: {err}");
    }
    if let Some(size) = text.strip_prefix("shm=") {
        return format!(
            "error mounting \"shm\" to rootfs at \"/dev/shm\": mount src=shm, dst=/dev/shm, flags=MS_NOSUID|MS_NODEV|MS_NOEXEC|MS_REMOUNT, data=mode=1777,size={size}: {err}"
        );
    }
    if let Some(spec) = text.strip_prefix("ulimit=") {
        let n = spec
            .split_once('=')
            .and_then(|(name, _)| rlimit(name))
            .unwrap_or_default();
        return format!("error setting rlimit type {n}: {err}");
    }
    if text.starts_with("oom=") {
        return format!("failed to write oom_score_adj: write /proc/self/oom_score_adj: {err}");
    }
    if text == "privileged" {
        return format!("making the container privileged: {err}");
    }
    if text == "cgroups-rw" {
        return format!("error remounting the cgroup hierarchy writable: {err}");
    }
    if let Some(rest) = text.strip_prefix("volume=") {
        let dest = rest.split('\0').nth(1).unwrap_or_default();
        return format!("error mounting a shared directory to rootfs at \"{dest}\": {err}");
    }
    format!("error remounting the root read-only: {err}")
}
