//! Devices plugged into the running microVM (D214), as its daemon asks (`run::builtin::
//! DEVICE`): found by a rescan of the PCI bus, mounted where the run's workload asked, and
//! let go again; a disk grown online, its file system with it.
//!
//! Each is done in a child of init's, as every built-in is: the PCI bus rescanned through
//! init's own sysfs ([`crate::devices::sysfs`]), as the run's root's is read-only; the
//! device's file system mounted attached nowhere (the new mount API), then attached in the
//! workload's mount namespace at its path, so that nothing of it is in init's.
//!
//! - `disk ADDR DST [ro]`: the disk at PCI address ADDR, its file system (ext4, XFS,
//!   Btrfs, FAT or EROFS, as the kernel recognizes it) at DST. Answers its block device's
//!   name.
//! - `share ADDR TAG DST [ro]`: the shared directory at ADDR, virtio-fs by TAG, at DST.
//! - `unmount ADDR DST`: DST unmounted in the workload, refused while anything there is
//!   in use, then the function let go (its `remove`), for its host to unplug.
//! - `grow ADDR DST SECTORS`: once the disk at ADDR reads SECTORS, its ext4 file system at
//!   DST grown to fill it (EXT4_IOC_RESIZE_FS, online). Answers its size in bytes.

use std::ffi::CString;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

/// How long a function plugged in is waited for, its driver probing it.
const FOUND_WITHIN: Duration = Duration::from_secs(10);

/// The file systems a plugged disk is tried as, in order.
const FILE_SYSTEMS: [&std::ffi::CStr; 5] = [c"ext4", c"xfs", c"btrfs", c"vfat", c"erofs"];

/// EXT4_IOC_RESIZE_FS (fs/ext4/ext4.h: _IOW('f', 16, __u64)).
const EXT4_IOC_RESIZE_FS: libc::c_ulong = 0x4008_6610;

/// `fsmount`'s read-only attribute, and `move_mount`'s flag for a mount by its descriptor
/// (include/uapi/linux/mount.h).
const MOUNT_ATTR_RDONLY: u64 = 0x1;
const MOVE_MOUNT_F_EMPTY_PATH: u32 = 0x4;

/// Does what `args` ask in the microVM whose workload is process `workload`, saying the
/// answer on `out`.
pub fn device(args: &[Vec<u8>], workload: libc::pid_t, out: &mut impl Write) -> io::Result<()> {
    let text: Vec<String> = args
        .iter()
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    let words: Vec<&str> = text.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["disk", addr, dst, rest @ ..] => {
            let read_only = rest.contains(&"ro");
            let name = block_device(addr)?;
            let mounted = disk_mount(&name, read_only)?;
            if !read_only {
                give_empty(&mounted, workload)?;
            }
            attach(mounted, workload, dst)?;
            write!(out, "{name}")
        }
        ["share", addr, tag, dst, rest @ ..] => {
            let read_only = rest.contains(&"ro");
            bound(addr)?;
            let tag = CString::new(*tag).map_err(io::Error::other)?;
            // The tag's device comes as its driver probes it: tried a while.
            let mounted = retried(|| fs_mount(c"virtiofs", &tag, read_only))?;
            attach(mounted, workload, dst)
        }
        ["unmount", addr, dst] => {
            in_workload(workload, || {
                let path = CString::new(*dst).map_err(io::Error::other)?;
                // SAFETY: umount2(2) of a NUL-terminated path.
                if unsafe { libc::umount2(path.as_ptr(), 0) } != 0 {
                    let e = io::Error::last_os_error();
                    return Err(io::Error::new(
                        e.kind(),
                        format!(
                            "unmounting {dst}: {e}{}",
                            if e.raw_os_error() == Some(libc::EBUSY) {
                                ": something in the microVM uses it"
                            } else {
                                ""
                            }
                        ),
                    ));
                }
                Ok(())
            })?;
            crate::devices::sysfs_write(&format!("bus/pci/devices/{addr}/remove"), "1")
        }
        ["grow", addr, dst, sectors] => {
            let name = block_device(addr)?;
            let size = format!("/sys/block/{name}/size");
            let want = sectors.to_string();
            let end = Instant::now() + FOUND_WITHIN;
            while crate::devices::sysfs_read(&format!("class/block/{name}/size"))
                .is_none_or(|s| s.trim() != want)
            {
                if Instant::now() > end {
                    return Err(io::Error::other(format!(
                        "{size} does not read {sectors} sectors"
                    )));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let sectors: u64 = sectors.parse().map_err(io::Error::other)?;
            in_workload(workload, || grow_ext4(dst, sectors * 512))?;
            write!(out, "{}", sectors * 512)
        }
        other => Err(io::Error::other(format!("no device step {other:?}"))),
    }
}

/// The block device of the virtio disk at PCI address `addr`, once its driver has it.
fn block_device(addr: &str) -> io::Result<String> {
    rescan()?;
    let dir = format!("bus/pci/devices/{addr}");
    wait(|| {
        let virtio = crate::devices::sysfs_list(&dir)?
            .into_iter()
            .find(|n| n.starts_with("virtio"))?;
        crate::devices::sysfs_list(&format!("{dir}/{virtio}/block"))?
            .into_iter()
            .next()
    })
    .ok_or_else(|| io::Error::other(format!("no disk at {addr} after a rescan of the PCI bus")))
}

/// Waits for the function at `addr` to have its driver.
fn bound(addr: &str) -> io::Result<()> {
    rescan()?;
    wait(|| crate::devices::sysfs_link(&format!("bus/pci/devices/{addr}/driver")))
        .map(|_| ())
        .ok_or_else(|| io::Error::other(format!("no driver took {addr} after a rescan of the PCI bus")))
}

/// Has the kernel scan the PCI bus for functions plugged in (drivers/pci/pci-sysfs.c,
/// rescan_store; pci_rescan_bus).
fn rescan() -> io::Result<()> {
    crate::devices::sysfs_write("bus/pci/rescan", "1")
}

/// What `found` finds, asked again every millisecond for [`FOUND_WITHIN`].
fn wait<T>(mut found: impl FnMut() -> Option<T>) -> Option<T> {
    let end = Instant::now() + FOUND_WITHIN;
    loop {
        if let Some(t) = found() {
            return Some(t);
        }
        if Instant::now() > end {
            return None;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// `make`'s mount, tried again every millisecond for [`FOUND_WITHIN`] while it fails.
fn retried(mut make: impl FnMut() -> io::Result<OwnedFd>) -> io::Result<OwnedFd> {
    let end = Instant::now() + FOUND_WITHIN;
    loop {
        match make() {
            Ok(fd) => return Ok(fd),
            Err(e) if Instant::now() > end => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

/// Block device `name`'s file system, mounted attached nowhere: the first of
/// [`FILE_SYSTEMS`] the kernel takes it as.
fn disk_mount(name: &str, read_only: bool) -> io::Result<OwnedFd> {
    let node = crate::devices::device_handle(name)?;
    let source = CString::new(format!("/proc/self/fd/{}", node.as_raw_fd())).map_err(io::Error::other)?;
    let mut tried = Vec::new();
    for fs in FILE_SYSTEMS {
        match fs_mount(fs, &source, read_only) {
            Ok(fd) => return Ok(fd),
            Err(e) => tried.push(format!("{}: {e}", fs.to_string_lossy())),
        }
    }
    Err(io::Error::other(format!(
        "{name} holds no file system this microVM mounts ({})",
        tried.join("; ")
    )))
}

/// A new mount of `fstype` from `source`, read-only (the `ro` flag, and the mount's
/// attribute) where asked, attached nowhere (fsopen(2), fsconfig(2), fsmount(2)).
fn fs_mount(fstype: &std::ffi::CStr, source: &std::ffi::CStr, read_only: bool) -> io::Result<OwnedFd> {
    const FSOPEN_CLOEXEC: u32 = 0x1;
    const FSCONFIG_SET_FLAG: u32 = 0;
    const FSCONFIG_SET_STRING: u32 = 1;
    const FSCONFIG_CMD_CREATE: u32 = 6;
    const FSMOUNT_CLOEXEC: u32 = 0x1;
    let null = std::ptr::null::<libc::c_char>();
    // SAFETY: fsopen(2) of a NUL-terminated name.
    let fs = unsafe { libc::syscall(libc::SYS_fsopen, fstype.as_ptr(), FSOPEN_CLOEXEC) };
    if fs < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor fsopen just made, owned here alone.
    let fs = unsafe { OwnedFd::from_raw_fd(fs as i32) };
    let config = |cmd: u32, key: *const libc::c_char, value: *const libc::c_char| {
        // SAFETY: fsconfig(2) of NUL-terminated strings, or none, on the context above.
        if unsafe { libc::syscall(libc::SYS_fsconfig, fs.as_raw_fd(), cmd, key, value, 0) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    config(FSCONFIG_SET_STRING, c"source".as_ptr(), source.as_ptr())?;
    if read_only {
        config(FSCONFIG_SET_FLAG, c"ro".as_ptr(), null)?;
    }
    config(FSCONFIG_CMD_CREATE, null, null)?;
    let attrs = if read_only { MOUNT_ATTR_RDONLY } else { 0 };
    // SAFETY: fsmount(2) of the context made above.
    let mounted = unsafe { libc::syscall(libc::SYS_fsmount, fs.as_raw_fd(), FSMOUNT_CLOEXEC, attrs) };
    if mounted < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor fsmount just made, owned here alone.
    Ok(unsafe { OwnedFd::from_raw_fd(mounted as i32) })
}

/// Gives the top of `mounted`, a file system holding nothing but `lost+found`, a fresh
/// disk's, to the user and group the workload, process `workload`, runs as, as a volume's
/// top is its run's user's where it is made empty (D38): so that its command writes there.
fn give_empty(mounted: &OwnedFd, workload: libc::pid_t) -> io::Result<()> {
    // SAFETY: openat(2) of the mount's top, by its descriptor (an O_PATH one, which reads
    // no directory), for a directory stream of its own.
    let dup = unsafe {
        libc::openat(
            mounted.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fdopendir(3) takes the descriptor just made.
    let dir = unsafe { libc::fdopendir(dup) };
    if dir.is_null() {
        let e = io::Error::last_os_error();
        // SAFETY: the descriptor openat made, which fdopendir did not take.
        unsafe { libc::close(dup) };
        return Err(e);
    }
    let mut empty = true;
    loop {
        // SAFETY: readdir(3) of the stream opened above.
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
            break;
        }
        // SAFETY: a NUL-terminated name readdir just gave.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if !matches!(name, b"." | b".." | b"lost+found") {
            empty = false;
            break;
        }
    }
    // SAFETY: closedir(3) of the stream opened above, and its descriptor with it.
    unsafe { libc::closedir(dir) };
    if !empty {
        return Ok(());
    }
    let status = std::fs::read_to_string(format!("/proc/{workload}/status"))?;
    let id = |key: &str| -> io::Result<u32> {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|ids| ids.split_whitespace().nth(1))
            .and_then(|effective| effective.parse().ok())
            .ok_or_else(|| io::Error::other(format!("the workload's {key}")))
    };
    let (uid, gid) = (id("Uid:")?, id("Gid:")?);
    // SAFETY: fchownat(2) of the mount's own top, by its descriptor.
    if unsafe { libc::fchownat(mounted.as_raw_fd(), c"".as_ptr(), uid, gid, libc::AT_EMPTY_PATH) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Attaches `mounted` at `dst` in the mount namespace of process `workload`, made a
/// directory there first where it is none.
fn attach(mounted: OwnedFd, workload: libc::pid_t, dst: &str) -> io::Result<()> {
    in_workload(workload, || {
        std::fs::create_dir_all(dst).map_err(|e| io::Error::new(e.kind(), format!("{dst}: {e}")))?;
        let path = CString::new(dst).map_err(io::Error::other)?;
        // SAFETY: move_mount(2) of a mount this process holds onto a NUL-terminated path.
        let moved = unsafe {
            libc::syscall(
                libc::SYS_move_mount,
                mounted.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_FDCWD,
                path.as_ptr(),
                MOVE_MOUNT_F_EMPTY_PATH,
            )
        };
        if moved != 0 {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(e.kind(), format!("mounting at {dst}: {e}")));
        }
        Ok(())
    })
}

/// Runs `step` in the mount namespace of process `workload`, then comes back to this
/// process's own.
fn in_workload<T>(workload: libc::pid_t, step: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let open = |path: &str| -> io::Result<OwnedFd> {
        let c = CString::new(path).map_err(io::Error::other)?;
        // SAFETY: open(2) of a NUL-terminated path.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor open just made, owned here alone.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    };
    let ours = open("/proc/self/ns/mnt")?;
    let root = open("/")?;
    let theirs = open(&format!("/proc/{workload}/ns/mnt"))
        .map_err(|e| io::Error::new(e.kind(), format!("the workload's mount namespace: {e}")))?;
    let enter = |ns: &OwnedFd| {
        // SAFETY: setns(2) on a descriptor this process holds, a single-threaded child.
        if unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNS) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    };
    enter(&theirs)?;
    let done = step();
    enter(&ours)?;
    // SAFETY: fchdir(2) and chroot(2) back to this process's own root, held open above.
    unsafe {
        libc::fchdir(root.as_raw_fd());
        libc::chroot(c".".as_ptr());
    }
    done
}

/// Grows the ext4 file system mounted at `dst` to `bytes` (EXT4_IOC_RESIZE_FS, online,
/// as resize2fs does a mounted one); another kind of file system is left as it is.
fn grow_ext4(dst: &str, bytes: u64) -> io::Result<()> {
    let path = CString::new(dst).map_err(io::Error::other)?;
    // SAFETY: an all-zero statfs is a valid out-parameter, which statfs(2) fills.
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: statfs(2) of a NUL-terminated path into a struct it owns.
    if unsafe { libc::statfs(path.as_ptr(), &raw mut fs) } != 0 {
        return Err(io::Error::last_os_error());
    }
    const EXT4_SUPER_MAGIC: u64 = 0xef53;
    #[allow(
        clippy::useless_conversion,
        reason = "statfs's f_type is i64 on some targets, u64 on others"
    )]
    if u64::try_from(fs.f_type).ok() != Some(EXT4_SUPER_MAGIC) {
        return Ok(());
    }
    #[allow(
        clippy::useless_conversion,
        reason = "statfs's f_bsize is i64 on some targets, u64 on others"
    )]
    let block = u64::try_from(fs.f_bsize).unwrap_or(4096).max(1024);
    let blocks = bytes / block;
    // SAFETY: open(2) of a NUL-terminated path.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor open just made, owned here alone.
    let dir = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: ioctl(2) EXT4_IOC_RESIZE_FS with a u64 it reads.
    if unsafe { libc::ioctl(dir.as_raw_fd(), EXT4_IOC_RESIZE_FS as _, &raw const blocks) } != 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(
            e.kind(),
            format!("growing the file system at {dst}: {e}"),
        ));
    }
    Ok(())
}
