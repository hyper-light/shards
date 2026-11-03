//! A stopped container's writable layer kept in its file (D190), gone on over as it is:
//! its host readies the file to go under the microVM's layer device (vmm `Layer::stage`)
//! and says so ([`kind::OVER`](shards_abi::run::kind::OVER)) in place of the archive it put
//! back before. The microVM's root, the image under the layer its template mounted, is
//! every process's and holds that layer's file system; so the root moves aside first:
//!
//! 1. A tmpfs, mounted where the template's root has room for it, becomes every process's
//!    root (pivot_root(2), which moves each process of the namespace rooted at the old
//!    root, the memory's grower among them), the old root's mounts (`/proc`, `/sys`,
//!    `/dev` and theirs) moved onto it, the old root detached.
//! 2. The layer's file system, unmounted and nothing of it open, is let go of: its device
//!    opened exclusively says so (a mounted block device is claimed by its file system,
//!    block/bdev.c, bd_prepare_to_claim), and the host puts the container's file under it
//!    (`control::LAYER_SWAP`), read back to have been.
//! 3. The device's file system, the container's, is mounted, the image under it, the mounts
//!    moved onto that, and it becomes every process's root, the tmpfs detached: the root
//!    a boot makes, over the container's layer as it left it.
//!
//! The template's standby, in a mount namespace of its own whose copy of the old root holds
//! the layer too, is ended first and born again after (run.rs). What the container's file
//! held of the template's own making is its last run's: the run writes its files again as
//! it starts.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};

use shards_abi::control;

/// Where the template's root has room for the tmpfs that is every process's root
/// meanwhile: a directory of its own, made in the layer it lets go of.
const ASIDE: &str = "/.shards-over";

/// Goes on over the container's layer its host readied (module docs). An error leaves the
/// microVM unable to run the container: its run fails.
pub fn go_over() -> Result<(), String> {
    let os = |what: &str| format!("{what}: {}", io::Error::last_os_error());
    // Moves between mounts of a shared tree are refused (fs/namespace.c, do_move_mount):
    // private until the new root is, then shared again as a boot leaves it (run.rs
    // `cgroups`). The template's standby, its slave, has gone.
    mount(None, c"/", None, libc::MS_REC | libc::MS_PRIVATE, None)
        .map_err(|e| format!("making the root's mounts private: {e}"))?;
    mkdir(ASIDE)?;
    mount(
        Some(c"tmpfs"),
        &path(ASIDE)?,
        Some(c"tmpfs"),
        0,
        Some(c"mode=0700"),
    )
    .map_err(|e| format!("a tmpfs at {ASIDE}: {e}"))?;
    mkdir(&format!("{ASIDE}/old"))?;
    let (aside, old) = (path(ASIDE)?, path(&format!("{ASIDE}/old"))?);
    // SAFETY: pivot_root(2) of NUL-terminated paths.
    if unsafe { libc::syscall(libc::SYS_pivot_root, aside.as_ptr(), old.as_ptr()) } != 0 {
        return Err(os("pivot_root onto the tmpfs"));
    }
    chdir(c"/")?;
    // The old root's own mounts, by where they are beneath it, before any moves.
    let held = mounts_beneath("/old")?;
    for rel in held
        .iter()
        .rev()
        .filter(|r| *r != "/proc")
        .chain(held.iter().filter(|r| *r == "/proc"))
    {
        move_mount(&format!("/old{rel}"), rel)?;
    }
    umount(&path("/old")?, libc::MNT_DETACH).map_err(|e| format!("detaching the template's root: {e}"))?;
    let _ = std::fs::remove_dir("/old");
    // The layer's own mount, unreachable now but through what init holds of it, and then
    // what it holds: each descriptor the tmpfs's root, until it is the new layer's.
    let slots = [
        crate::pmem::layer_fd(),
        crate::setup::rw_fd(),
        crate::changes::upper_fd(),
    ];
    if let Some(rw) = crate::setup::rw_fd() {
        umount(&path(&format!("/proc/self/fd/{rw}"))?, libc::MNT_DETACH)
            .map_err(|e| format!("detaching the template's layer: {e}"))?;
    }
    for fd in slots.into_iter().flatten() {
        reopen_as(fd, c"/", libc::O_PATH | libc::O_DIRECTORY)
            .map_err(|e| format!("letting go of the template's layer: {e}"))?;
    }
    let dev = crate::pmem::layer_device().ok_or("no writable layer of its own")?;
    // SAFETY: mknod(2) of a NUL-terminated path, the layer's block device.
    if unsafe { libc::mknod(c"/layer".as_ptr(), libc::S_IFBLK | 0o600, dev) } != 0 {
        return Err(os("its layer's device"));
    }
    // Nothing of the template's layer holds its device: else its host's file would go
    // under a file system still mounted.
    // SAFETY: open(2) of a NUL-terminated path; the descriptor is owned below.
    let claimed = unsafe {
        libc::open(
            c"/layer".as_ptr(),
            libc::O_RDONLY | libc::O_EXCL | libc::O_CLOEXEC,
        )
    };
    if claimed < 0 {
        return Err(os("its template's layer is still held"));
    }
    // SAFETY: a fresh descriptor nothing else owns.
    drop(unsafe { OwnedFd::from_raw_fd(claimed) });
    crate::linux::control_write(control::LAYER_SWAP, control::SWAP_LAYER)
        .and_then(|()| crate::linux::control_read64(control::LAYER_SWAP))
        .map_err(|e| format!("the control page: {e}"))
        .and_then(|swapped| match swapped {
            1 => Ok(()),
            _ => Err("its host did not put the container's layer under its device".to_string()),
        })?;
    mkdir("/rw")?;
    crate::pmem::mount_ext4(c"/layer", c"/rw")?;
    for dir in ["/rw/upper", "/rw/work"] {
        mkdir(dir)?;
    }
    for (fd, at, flags) in [
        (
            crate::pmem::layer_fd(),
            c"/rw",
            libc::O_RDONLY | libc::O_DIRECTORY,
        ),
        (crate::setup::rw_fd(), c"/rw", libc::O_PATH | libc::O_DIRECTORY),
        (
            crate::changes::upper_fd(),
            c"/rw/upper",
            libc::O_PATH | libc::O_DIRECTORY,
        ),
    ] {
        if let Some(fd) = fd {
            reopen_as(fd, at, flags).map_err(|e| format!("{}: {e}", at.to_string_lossy()))?;
        }
    }
    // The image, as the template's root had it, through what init holds of it.
    mkdir("/lower")?;
    let lower = crate::changes::lower_path().ok_or("the image's root was not kept")?;
    mount(Some(&path(&lower)?), c"/lower", None, libc::MS_BIND, None)
        .map_err(|e| format!("the image: {e}"))?;
    mkdir("/new")?;
    let options = path(&format!(
        "lowerdir=/lower,upperdir=/rw/upper,workdir=/rw/work,{}",
        crate::run::XINO
    ))?;
    mount(Some(c"overlay"), c"/new", Some(c"overlay"), 0, Some(&options))
        .map_err(|e| format!("its root: {e}"))?;
    for rel in held
        .iter()
        .rev()
        .filter(|r| *r != "/proc")
        .chain(held.iter().filter(|r| *r == "/proc"))
    {
        move_mount(rel, &format!("/new{rel}"))?;
    }
    // The new root every process's, the tmpfs under it detached (pivot_root(2), NOTES).
    chdir(c"/new")?;
    // SAFETY: pivot_root(2) of NUL-terminated paths.
    if unsafe { libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) } != 0 {
        return Err(os("pivot_root onto the container's root"));
    }
    umount(c".", libc::MNT_DETACH).map_err(|e| format!("detaching the tmpfs: {e}"))?;
    chdir(c"/")?;
    mount(None, c"/", None, libc::MS_REC | libc::MS_SHARED, None)
        .map_err(|e| format!("sharing the root's mounts: {e}"))
}

/// The mount points of the mounts right beneath the one at `at` (its children), as
/// /proc/self/mountinfo names them relative to this process's root, each made relative to
/// `at`, in the order they were mounted: those a move takes with them, theirs included.
fn mounts_beneath(at: &str) -> Result<Vec<String>, String> {
    let info = std::fs::read_to_string(format!("{at}/proc/self/mountinfo"))
        .or_else(|_| std::fs::read_to_string("/proc/self/mountinfo"))
        .map_err(|e| format!("its mounts: {e}"))?;
    // mountinfo's fields: ID, parent ID, major:minor, root, mount point (octal escapes).
    let lines: Vec<(&str, &str, String)> = info
        .lines()
        .filter_map(|l| {
            let mut f = l.split(' ');
            let (id, parent) = (f.next()?, f.next()?);
            Some((id, parent, unescape(f.nth(2)?)))
        })
        .collect();
    let id = lines
        .iter()
        .rev()
        .find(|(_, _, point)| point == at)
        .map(|(id, _, _)| *id)
        .ok_or_else(|| format!("no mount at {at}"))?;
    Ok(lines
        .iter()
        .filter(|(i, parent, _)| *parent == id && *i != id)
        .filter_map(|(_, _, point)| point.strip_prefix(at).map(str::to_string))
        .filter(|rel| rel.starts_with('/'))
        .collect())
}

/// A mount point in mountinfo's escapes (`\040` for a space), as it is.
fn unescape(field: &str) -> String {
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        let octal = b
            .get(i + 1..i + 4)
            .filter(|d| c == b'\\' && d.iter().all(|x| (b'0'..=b'7').contains(x)));
        match octal {
            Some(d) => {
                out.push(
                    d.iter()
                        .fold(0u8, |n, x| n.wrapping_mul(8).wrapping_add(x - b'0')),
                );
                i += 4;
            }
            None => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Makes `at` a place to move the mount at `like` onto: a directory where it is one, else
/// an empty file, its parents directories.
fn mount_point(like: &str, at: &str) -> Result<(), String> {
    let dir = std::fs::metadata(like).map(|m| m.is_dir()).unwrap_or(true);
    let made = if dir {
        std::fs::create_dir_all(at)
    } else {
        std::path::Path::new(at)
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::File::create(at).map(drop))
    };
    made.map_err(|e| format!("{at}: {e}"))
}

fn move_mount(from: &str, to: &str) -> Result<(), String> {
    if std::fs::symlink_metadata(to).is_err() {
        mount_point(from, to)?;
    }
    mount(Some(&path(from)?), &path(to)?, None, libc::MS_MOVE, None)
        .map_err(|e| format!("moving {from} to {to}: {e}"))
}

/// Opens `at` with `flags` as descriptor `fd`, in place of what it was: whoever reads the
/// number reads the new.
fn reopen_as(fd: RawFd, at: &CStr, flags: libc::c_int) -> io::Result<()> {
    // SAFETY: open(2) of a NUL-terminated path.
    let new = unsafe { libc::open(at.as_ptr(), flags | libc::O_CLOEXEC) };
    if new < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    let new = unsafe { OwnedFd::from_raw_fd(new) };
    // SAFETY: dup3(2) onto a descriptor init holds for its life, which it replaces whole.
    if unsafe { libc::dup3(new.as_raw_fd(), fd, libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn path(p: &str) -> Result<CString, String> {
    CString::new(p).map_err(|_| format!("{p:?}: a NUL in it"))
}

fn mkdir(p: &str) -> Result<(), String> {
    match std::fs::create_dir(p) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(format!("mkdir {p}: {e}")),
        _ => Ok(()),
    }
}

fn chdir(p: &CStr) -> Result<(), String> {
    // SAFETY: chdir(2) of a NUL-terminated path.
    if unsafe { libc::chdir(p.as_ptr()) } != 0 {
        return Err(format!(
            "entering {}: {}",
            p.to_string_lossy(),
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn mount(
    source: Option<&CStr>,
    target: &CStr,
    fstype: Option<&CStr>,
    flags: libc::c_ulong,
    data: Option<&CStr>,
) -> io::Result<()> {
    let ptr = |s: Option<&CStr>| s.map_or(std::ptr::null(), CStr::as_ptr);
    // SAFETY: mount(2) of NUL-terminated strings, or nulls, that outlive the call.
    if unsafe { libc::mount(ptr(source), target.as_ptr(), ptr(fstype), flags, ptr(data).cast()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn umount(target: &CStr, flags: libc::c_int) -> io::Result<()> {
    // SAFETY: umount2(2) of a NUL-terminated path.
    if unsafe { libc::umount2(target.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
