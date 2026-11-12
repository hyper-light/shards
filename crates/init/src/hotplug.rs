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
//! - `share ADDR TAG DST [ro] [member N]`: the shared directory at ADDR, virtio-fs by TAG,
//!   at DST; with `member N`, in the N-th domain's mount namespace instead of the
//!   workload's (D235), DST beneath one of the run's plug roots, its mount point made there,
//!   and the host's files the domain's.
//! - `unmount ADDR DST [member N]`: DST unmounted in the workload, or the N-th domain,
//!   refused while anything there is in use, then the function let go (its `remove`), for
//!   its host to unplug.
//! - `grow ADDR DST SECTORS`: once the disk at ADDR reads SECTORS, its ext4 file system at
//!   DST grown to fill it (EXT4_IOC_RESIZE_FS, online). Answers its size in bytes.
//! - `function ADDR group|node HOST`: a host's function at ADDR, HOST as its daemon named
//!   it, given to vfio-pci, and its group's node, or its own VFIO device node, made in the
//!   workload's `/dev` with the VM's numbers and mode (D44, D154), owned by the user and
//!   group the workload runs as. Answers the node's path and its numbers, `PATH c MAJ:MIN`.
//!   A function by its device node is taken through iommufd too: the workload's
//!   `/dev/iommu` its user's and group's as well, made where it has none; answered after,
//!   `/dev/iommu c MAJ:MIN BEFORE`, BEFORE `made` or its owner before, `UID:GID`.
//! - `allow [c MAJ:MIN rwm]...`: the workload's device filter, its run's rules and these,
//!   the nodes of every function plugged in. Init's own process does it, as it keeps the
//!   filter (`devices::allow`); not here.
//! - `release ADDR [PATH [iommu BEFORE]]`: the function at ADDR let go of, refused while
//!   anything in the microVM holds its node at PATH or its VFIO device open, its node
//!   taken out of the workload's `/dev` (and `/dev/iommu` made as BEFORE says it was, for
//!   the last function had through it), then its `remove` written, for its host to unplug.

use std::ffi::CString;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::OnceLock;

use shards_abi::run::plug;

use crate::devices::Uevents;

/// The run's plug roots, as [`plug_roots`] read them, for what is plugged into its domains
/// as they run.
static ROOTS: OnceLock<Vec<CString>> = OnceLock::new();

/// The run's plug roots, from its setup entries (`plug-root=/Users`, D235), each by its
/// name (`Users`), and kept for what is plugged into its domains as they run. Each domain
/// is given each as it starts (domains.rs): made where its image lacks it, and Landlock's
/// every right granted beneath it, which grants nothing until something is plugged there,
/// as the domain's root is read-only. One that is not a single top-level directory, or is
/// one of init's own (`plug::REFUSED`), is refused.
pub fn plug_roots(setup: &[Vec<u8>]) -> Result<Vec<CString>, String> {
    let mut roots: Vec<CString> = Vec::new();
    for entry in setup.iter().filter_map(|e| e.strip_prefix(plug::ROOT_KEY)) {
        let name = plug::root_name(entry)
            .and_then(|n| CString::new(n).ok())
            .ok_or_else(|| {
                format!(
                    "plug root {:?}: not a top-level directory a domain may be given paths beneath",
                    String::from_utf8_lossy(entry)
                )
            })?;
        if !roots.contains(&name) {
            roots.push(name);
        }
    }
    let _ = ROOTS.set(roots.clone());
    Ok(roots)
}

/// The components of `dst`, where it lies strictly beneath one of `roots` (D235): absolute,
/// each component plain, and its first a root. A mount at a root itself would hide the
/// directory Landlock's rule for it is tied to, which a path walk then skips
/// (security/landlock/fs.c), so a plug lies beneath one. Refused naming the roots there are.
fn beneath_root<'a>(dst: &'a str, roots: &[CString]) -> Result<Vec<&'a str>, String> {
    let named = || {
        let all: Vec<String> = roots
            .iter()
            .map(|r| format!("/{}", r.to_string_lossy()))
            .collect();
        if all.is_empty() {
            "the run has no plug roots".to_string()
        } else {
            format!("the run's plug roots are {}", all.join(", "))
        }
    };
    let parts: Vec<&str> = dst
        .strip_prefix('/')
        .ok_or_else(|| format!("{dst}: not an absolute path"))?
        .split('/')
        .collect();
    if parts.iter().any(|p| matches!(*p, "" | "." | "..")) {
        return Err(format!("{dst}: not a plain absolute path"));
    }
    let root = parts.first().copied().unwrap_or_default();
    if parts.len() < 2 || !roots.iter().any(|r| r.as_bytes() == root.as_bytes()) {
        return Err(format!("{dst}: not beneath a plug root: {}", named()));
    }
    Ok(parts)
}

/// The file systems a plugged disk is tried as, in order.
const FILE_SYSTEMS: [&std::ffi::CStr; 5] = [c"ext4", c"xfs", c"btrfs", c"vfat", c"erofs"];

/// EXT4_IOC_RESIZE_FS (fs/ext4/ext4.h: _IOW('f', 16, __u64)).
const EXT4_IOC_RESIZE_FS: libc::c_ulong = 0x4008_6610;

/// `fsmount`'s and `mount_setattr`'s attributes, and `move_mount`'s flag for a mount by its
/// descriptor (include/uapi/linux/mount.h).
const MOUNT_ATTR_RDONLY: u64 = 0x1;
const MOUNT_ATTR_NOSUID: u64 = 0x2;
const MOUNT_ATTR_NODEV: u64 = 0x4;
const MOVE_MOUNT_F_EMPTY_PATH: u32 = 0x4;

/// Does what `args` ask in the microVM whose workload is process `workload` and whose
/// domains are `domains`, saying the answer on `out`.
pub fn device(
    args: &[Vec<u8>],
    workload: libc::pid_t,
    domains: &[crate::domains::Started],
    out: &mut impl Write,
) -> io::Result<()> {
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
            // A domain's beneath one of the run's plug roots (D235), refused before its
            // function is taken.
            let member = member_of(rest, domains)?;
            let parts = match member {
                Some(_) => Some(
                    beneath_root(dst, ROOTS.get().map_or(&[][..], Vec::as_slice))
                        .map_err(io::Error::other)?,
                ),
                None => None,
            };
            bound(addr)?;
            let tag = CString::new(*tag).map_err(io::Error::other)?;
            // The tag is virtio-fs's once its driver has the function: virtio_fs_probe adds
            // it before the probe returns, and a driver is bound only after (drivers/base/
            // dd.c `really_probe`, then `driver_bound`). Mounted once: a refusal is the
            // kernel's word.
            let (Some((id, cgroup)), Some(parts)) = (member, parts) else {
                let mounted = fs_mount(c"virtiofs", &tag, read_only, 0)?;
                return attach(mounted, workload, dst);
            };
            // A domain's, as a volume given it is (D109): `nosuid` and `nodev`, and the
            // files the host made there its user's, which it is told while the mount is
            // still writable, as a read-only mount takes no attribute.
            let mounted = fs_mount(c"virtiofs", &tag, false, MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV)?;
            crate::setup::name_owner_as(&format!("/proc/self/fd/{}", mounted.as_raw_fd()), id, id);
            if read_only {
                set_attrs(&mounted, MOUNT_ATTR_RDONLY)?;
            }
            in_member(cgroup, || {
                mount_point(&parts)?;
                move_to(&mounted, dst)
            })
        }
        ["unmount", addr, dst, rest @ ..] => {
            let unmount = || {
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
            };
            match member_of(rest, domains)? {
                Some((_, cgroup)) => in_member(cgroup, unmount)?,
                None => in_workload(workload, unmount)?,
            }
            crate::devices::sysfs_write(&format!("bus/pci/devices/{addr}/remove"), "1")
        }
        ["grow", addr, dst, sectors] => {
            // Heard from before the disk is found: its driver reads the new size as the
            // host's word of it comes (virtio_blk.c `virtblk_config_changed_work`), and says
            // so (`set_capacity_and_notify`'s KOBJ_CHANGE, RESIZE=1) once the size reads it.
            let events = Uevents::open()?;
            let name = block_device(addr)?;
            let size = format!("/sys/block/{name}/size");
            let want = sectors.to_string();
            events
                .arrival(|| {
                    crate::devices::sysfs_read(&format!("class/block/{name}/size"))
                        .filter(|s| s.trim() == want)
                })
                .map_err(|e| {
                    io::Error::other(format!("waiting for {size} to read {sectors} sectors: {e}"))
                })?;
            let sectors: u64 = sectors.parse().map_err(io::Error::other)?;
            in_workload(workload, || grow_ext4(dst, sectors * 512))?;
            write!(out, "{}", sectors * 512)
        }
        ["function", addr, kind, host] => {
            rescan(addr)?;
            let mut node = crate::devices::vfio_function(addr, kind, host).map_err(io::Error::other)?;
            // The workload's to open: the VM's devtmpfs makes a group's node, and a VFIO
            // device's, root's and 0600 (vfio_devnode and vfio_device_devnode set no mode;
            // devtmpfs.c's default), which a workload of a user other than root cannot
            // open (EACCES); its mode kept, so that it is the workload's user's alone.
            let ids = workload_ids(workload)?;
            (node.uid, node.gid) = ids;
            // A function by its device node is taken through iommufd as well (vfio.rst,
            // "Device cdev Example"): the workload's /dev/iommu its user's too.
            let iommu = match *kind {
                "node" => Some(iommu_node()?),
                _ => None,
            };
            let before = in_workload(workload, || {
                let before = iommu.as_ref().map(|vm| give_iommu(vm, ids)).transpose()?;
                if let Err(e) = crate::devices::make_nodes(std::slice::from_ref(&node)) {
                    if let Some(b) = &before {
                        let _ = give_back_iommu(b);
                    }
                    return Err(io::Error::other(e));
                }
                Ok(before)
            })?;
            write!(out, "{} c {}:{}", node.path, node.major, node.minor)?;
            match (&iommu, before) {
                (Some(vm), Some(before)) => write!(out, " {IOMMU} c {}:{} {before}", vm.major, vm.minor),
                _ => Ok(()),
            }
        }
        ["release", addr, rest @ ..] => {
            if let Some(path) = rest.first().filter(|p| !p.is_empty()) {
                let iommu = match rest.get(1..) {
                    Some(["iommu", before]) => Some(*before),
                    Some([]) => None,
                    other => return Err(io::Error::other(format!("no release step {other:?}"))),
                };
                if let Some(node) = crate::devices::vm_devices(path)
                    .into_iter()
                    .find(|n| n.path == *path)
                    && let Some(who) = crate::devices::holder(&node, addr)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::ResourceBusy,
                        format!("{path}: {who} in the microVM has it open: it closes it first"),
                    ));
                }
                in_workload(workload, || {
                    // The last function had through it gone: /dev/iommu as it was.
                    if let Some(before) = iommu {
                        give_back_iommu(before)?;
                    }
                    let c = CString::new(*path).map_err(io::Error::other)?;
                    // SAFETY: unlink(2) of a NUL-terminated path.
                    if unsafe { libc::unlink(c.as_ptr()) } != 0 {
                        let e = io::Error::last_os_error();
                        if e.raw_os_error() != Some(libc::ENOENT) {
                            return Err(io::Error::new(e.kind(), format!("{path}: {e}")));
                        }
                    }
                    Ok(())
                })?;
            }
            // Let go of where its guest still has it: its driver's remove, vfio-pci's.
            match crate::devices::sysfs_list(&format!("bus/pci/devices/{addr}")) {
                Some(_) => crate::devices::sysfs_write(&format!("bus/pci/devices/{addr}/remove"), "1"),
                None => Ok(()),
            }
        }
        other => Err(io::Error::other(format!("no device step {other:?}"))),
    }
}

/// The block device of the virtio disk at PCI address `addr`, once its driver has it.
fn block_device(addr: &str) -> io::Result<String> {
    let events = rescan(addr)?;
    let dir = format!("bus/pci/devices/{addr}");
    events
        .arrival(|| {
            let virtio = crate::devices::sysfs_list(&dir)?
                .into_iter()
                .find(|n| n.starts_with("virtio"))?;
            crate::devices::sysfs_list(&format!("{dir}/{virtio}/block"))?
                .into_iter()
                .next()
        })
        .map_err(|e| io::Error::other(format!("waiting for the disk at {addr}: {e}")))
}

/// Waits for the function at `addr` to have its driver.
fn bound(addr: &str) -> io::Result<()> {
    let events = rescan(addr)?;
    events
        .arrival(|| crate::devices::sysfs_link(&format!("bus/pci/devices/{addr}/driver")))
        .map(|_| ())
        .map_err(|e| io::Error::other(format!("waiting for a driver to take {addr}: {e}")))
}

/// Has the kernel scan the PCI bus for functions plugged in (drivers/pci/pci-sysfs.c,
/// rescan_store; pci_rescan_bus), and says whether it found the function at `addr`: the
/// scan adds what it finds before the write returns, so one not there then is not plugged
/// in. Its driver is the kernel's to bind, which it does in the same write unless it
/// defers it or runs it apart (drivers/base/dd.c `__device_attach`): the kernel's device
/// events, heard from before the scan and returned, say when.
fn rescan(addr: &str) -> io::Result<Uevents> {
    let events = Uevents::open()?;
    crate::devices::sysfs_write("bus/pci/rescan", "1")?;
    if crate::devices::sysfs_list(&format!("bus/pci/devices/{addr}")).is_none() {
        return Err(io::Error::other(format!(
            "no function at {addr} after a rescan of the PCI bus"
        )));
    }
    Ok(events)
}

/// Block device `name`'s file system, mounted attached nowhere: the first of
/// [`FILE_SYSTEMS`] the kernel takes it as.
fn disk_mount(name: &str, read_only: bool) -> io::Result<OwnedFd> {
    let node = crate::devices::device_handle(name)?;
    let source = CString::new(format!("/proc/self/fd/{}", node.as_raw_fd())).map_err(io::Error::other)?;
    let mut tried = Vec::new();
    for fs in FILE_SYSTEMS {
        match fs_mount(fs, &source, read_only, 0) {
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
/// attribute) where asked, with mount attributes `attrs` besides, attached nowhere
/// (fsopen(2), fsconfig(2), fsmount(2)).
fn fs_mount(
    fstype: &std::ffi::CStr,
    source: &std::ffi::CStr,
    read_only: bool,
    attrs: u64,
) -> io::Result<OwnedFd> {
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
    let attrs = attrs | if read_only { MOUNT_ATTR_RDONLY } else { 0 };
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
    let (uid, gid) = workload_ids(workload)?;
    // SAFETY: fchownat(2) of the mount's own top, by its descriptor.
    if unsafe { libc::fchownat(mounted.as_raw_fd(), c"".as_ptr(), uid, gid, libc::AT_EMPTY_PATH) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The user and group process `workload` runs as: its effective IDs.
fn workload_ids(workload: libc::pid_t) -> io::Result<(u32, u32)> {
    let status = std::fs::read_to_string(format!("/proc/{workload}/status"))?;
    effective_ids(&status).ok_or_else(|| io::Error::other("the workload's Uid: and Gid:"))
}

/// The effective user and group a process's `status` says: the second of its `Uid:` and
/// `Gid:` lines' IDs, which are real, effective, saved and filesystem in turn (proc(5)).
fn effective_ids(status: &str) -> Option<(u32, u32)> {
    let id = |key: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|ids| ids.split_whitespace().nth(1))
            .and_then(|effective| effective.parse().ok())
    };
    Some((id("Uid:")?, id("Gid:")?))
}

/// Where a workload has iommufd: a misc device of a dynamic minor, its node `iommu`, mode
/// 0660 (drivers/iommu/iommufd/main.c, `iommu_misc_dev`).
const IOMMU: &str = "/dev/iommu";

/// The VM's `/dev/iommu`, its numbers and mode.
fn iommu_node() -> io::Result<crate::devices::Node> {
    crate::devices::vm_devices(IOMMU)
        .into_iter()
        .find(|n| n.path == IOMMU)
        .ok_or_else(|| {
            io::Error::other(format!(
                "the microVM has no {IOMMU}, which a function by its device node is taken through: its kernel has no IOMMUFD"
            ))
        })
}

/// The workload's `/dev/iommu` given the user and group `ids`, in its mount namespace:
/// made with the VM's numbers and mode where it has none, else its own, iommufd's,
/// given them. What it was before, which [`give_back_iommu`] makes it again: `made`, or
/// its owner, `UID:GID`.
fn give_iommu(vm: &crate::devices::Node, ids: (u32, u32)) -> io::Result<String> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    let named = |e: io::Error| io::Error::new(e.kind(), format!("{IOMMU}: {e}"));
    match std::fs::symlink_metadata(IOMMU) {
        Ok(m) => {
            if !m.file_type().is_char_device() || m.rdev() != libc::makedev(vm.major, vm.minor) {
                return Err(io::Error::other(format!(
                    "the workload's {IOMMU} is another device than iommufd's {}:{}",
                    vm.major, vm.minor
                )));
            }
            std::os::unix::fs::lchown(IOMMU, Some(ids.0), Some(ids.1)).map_err(named)?;
            Ok(format!("{}:{}", m.uid(), m.gid()))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut node = vm.clone();
            (node.uid, node.gid) = ids;
            crate::devices::make_nodes(std::slice::from_ref(&node)).map_err(io::Error::other)?;
            Ok("made".into())
        }
        Err(e) => Err(named(e)),
    }
}

/// The workload's `/dev/iommu` as it was before [`give_iommu`], `before` its answer:
/// taken out where it was made, else given its owner again. In its mount namespace.
fn give_back_iommu(before: &str) -> io::Result<()> {
    let named = |e: io::Error| io::Error::new(e.kind(), format!("{IOMMU}: {e}"));
    match owner_before(before) {
        Some(None) => match std::fs::remove_file(IOMMU) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(named(e)),
            _ => Ok(()),
        },
        Some(Some((uid, gid))) => std::os::unix::fs::lchown(IOMMU, Some(uid), Some(gid)).map_err(named),
        None => Err(io::Error::other(format!("{IOMMU} was {before:?} before"))),
    }
}

/// What [`give_iommu`] said `/dev/iommu` was: none (`made`), or its owner (`UID:GID`).
fn owner_before(before: &str) -> Option<Option<(u32, u32)>> {
    if before == "made" {
        return Some(None);
    }
    let (uid, gid) = before.split_once(':')?;
    Some(Some((uid.parse().ok()?, gid.parse().ok()?)))
}

/// Attaches `mounted` at `dst` in the mount namespace of process `workload`, made a
/// directory there first where it is none.
fn attach(mounted: OwnedFd, workload: libc::pid_t, dst: &str) -> io::Result<()> {
    in_workload(workload, || {
        std::fs::create_dir_all(dst).map_err(|e| io::Error::new(e.kind(), format!("{dst}: {e}")))?;
        move_to(&mounted, dst)
    })
}

/// Moves `mounted`, a mount attached nowhere, onto `dst` in this process's mount namespace.
fn move_to(mounted: &OwnedFd, dst: &str) -> io::Result<()> {
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
}

/// `path` opened to be held, not read (`O_RDONLY`, close-on-exec).
fn open_held(path: &str) -> io::Result<OwnedFd> {
    let c = CString::new(path).map_err(io::Error::other)?;
    // SAFETY: open(2) of a NUL-terminated path.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor open just made, owned here alone.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Runs `step` in the mount namespace of process `workload`, then comes back to this
/// process's own.
fn in_workload<T>(workload: libc::pid_t, step: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let theirs = open_held(&format!("/proc/{workload}/ns/mnt"))
        .map_err(|e| io::Error::new(e.kind(), format!("the workload's mount namespace: {e}")))?;
    in_namespace(&theirs, step)
}

/// Runs `step` in the mount namespace of the domain whose cgroup is `cgroup` (domains.rs
/// `Started::cgroup`), then comes back to this process's own (D235).
fn in_member<T>(cgroup: &str, step: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let theirs = member_process(cgroup)?;
    in_namespace(&theirs, step)
}

/// Runs `step` in the mount namespace `theirs` names (a namespace's descriptor, or a pidfd,
/// which setns(2) takes since Linux 5.8), then comes back to this process's own.
fn in_namespace<T>(theirs: &OwnedFd, step: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let ours = open_held("/proc/self/ns/mnt")?;
    let root = open_held("/")?;
    let enter = |ns: &OwnedFd| {
        // SAFETY: setns(2) on a descriptor this process holds, a single-threaded child.
        if unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNS) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    };
    enter(theirs)?;
    let done = step();
    enter(&ours)?;
    // SAFETY: fchdir(2) and chroot(2) back to this process's own root, held open above.
    unsafe {
        libc::fchdir(root.as_raw_fd());
        libc::chroot(c".".as_ptr());
    }
    done
}

/// The domain `rest` names (`member N`, the N-th domain's, D235), by its uid and its cgroup:
/// none where it names none; refused where N started no domain.
fn member_of<'a>(
    rest: &[&str],
    domains: &'a [crate::domains::Started],
) -> io::Result<Option<(u32, &'a str)>> {
    let mut words = rest.iter().skip_while(|w| **w != "member");
    if words.next().is_none() {
        return Ok(None);
    }
    let n: u32 = words
        .next()
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| io::Error::other("member takes a member's index"))?;
    let started = domains
        .iter()
        .find(|d| d.member == n)
        .ok_or_else(|| io::Error::other(format!("member {n} did not start")))?;
    let id = crate::domains::FIRST_ID
        .checked_add(n)
        .ok_or_else(|| io::Error::other(format!("member {n}: past the domains' IDs")))?;
    Ok(Some((id, started.cgroup.as_str())))
}

/// A process's PID in a domain's PID namespace, a child of init's, from its `status`
/// (proc(5)): the last of its two `NSpid:` numbers; none for a process of init's namespace
/// (one number), as each domain's in-VM server instance is, in its domain's cgroup.
fn domain_pid(status: &str) -> Option<u32> {
    let ids: Vec<&str> = status
        .lines()
        .find_map(|l| l.strip_prefix("NSpid:"))?
        .split_whitespace()
        .collect();
    match ids.as_slice() {
        [_, inner] => inner.parse().ok(),
        _ => None,
    }
}

/// A pidfd of a process in the command's mount namespace of the domain whose cgroup is
/// `cgroup`: one of its PID namespace's but that namespace's init, which keeps a mount
/// namespace of its own (managed.rs), its command (PID 2 there) first. Each is held before
/// it is checked to be the domain's still, so that a PID another process took meanwhile is
/// never entered: one that ended once held fails setns(2), and the next is tried.
fn member_process(cgroup: &str) -> io::Result<OwnedFd> {
    let procs = std::fs::read_to_string(format!("{cgroup}/cgroup.procs"))
        .map_err(|e| io::Error::new(e.kind(), format!("{cgroup}: {e}")))?;
    let group = cgroup.strip_prefix("/sys/fs/cgroup").unwrap_or(cgroup);
    let side = |pid: &str| {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let in_group = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
            .is_ok_and(|c| c.lines().any(|l| l.strip_prefix("0::") == Some(group)));
        domain_pid(&status).filter(|inner| in_group && *inner != 1)
    };
    let mut candidates: Vec<(u32, &str)> = procs
        .lines()
        .filter_map(|pid| side(pid).map(|inner| (inner, pid)))
        .collect();
    candidates.sort_by_key(|(inner, _)| *inner != 2);
    for (_, pid) in candidates {
        let Ok(pid) = pid.parse::<libc::pid_t>() else {
            continue;
        };
        // SAFETY: pidfd_open(2) of a PID, with no flags.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
        let Ok(fd) = libc::c_int::try_from(fd) else {
            continue;
        };
        if fd < 0 {
            continue;
        }
        // SAFETY: the descriptor pidfd_open just made, owned here alone.
        let held = unsafe { OwnedFd::from_raw_fd(fd) };
        if side(&pid.to_string()).is_some() {
            return Ok(held);
        }
    }
    Err(io::Error::other(
        "it runs no process of its own: it has ended, or not yet started",
    ))
}

/// `include/uapi/linux/openat2.h`'s `struct open_how` and its resolve flags, and
/// `open_tree`'s (include/uapi/linux/mount.h).
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
const RESOLVE_NO_XDEV: u64 = 0x01;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_IN_ROOT: u64 = 0x10;
const OPEN_TREE_CLONE: u32 = 1;

/// Sets `set` on `mounted`'s attributes, and clears `clear` (mount_setattr(2)).
fn mount_attrs(mounted: &OwnedFd, set: u64, clear: u64) -> io::Result<()> {
    let attr: [u64; 4] = [set, clear, 0, 0];
    // SAFETY: mount_setattr(2) of a mount this process holds, with a struct mount_attr it owns.
    if unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            mounted.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            attr.as_ptr(),
            std::mem::size_of_val(&attr),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sets `set` on `mounted`'s attributes.
fn set_attrs(mounted: &OwnedFd, set: u64) -> io::Result<()> {
    mount_attrs(mounted, set, 0)
}

/// Makes `parts`, a path beneath a plug root (D235), in this process's root, a domain's:
/// through a copy of its root's mount, attached nowhere and made writable, the domain's own
/// being read-only, so that each directory is made in the writable layer of the domain's
/// root, which init alone writes (domains.rs `image_root`). The root is found as the
/// domain's own walk finds it, its image's symlinks followed within it; beneath it no
/// symlink is followed, so that nothing is made outside it.
fn mount_point(parts: &[&str]) -> io::Result<()> {
    // SAFETY: open_tree(2) of a NUL-terminated path, a copy of its mount alone.
    let copy = unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            libc::AT_FDCWD,
            c"/".as_ptr(),
            OPEN_TREE_CLONE | libc::O_CLOEXEC as u32,
        )
    };
    let copy = match libc::c_int::try_from(copy) {
        Ok(fd) if fd >= 0 => fd,
        _ => {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(
                e.kind(),
                format!("a copy of the domain's root: {e}"),
            ));
        }
    };
    // SAFETY: the descriptor open_tree just made, owned here alone.
    let mut at = unsafe { OwnedFd::from_raw_fd(copy) };
    mount_attrs(&at, 0, MOUNT_ATTR_RDONLY)?;
    for (i, part) in parts.iter().enumerate() {
        let name = CString::new(*part).map_err(io::Error::other)?;
        // SAFETY: mkdirat(2) of a NUL-terminated name beneath a descriptor held here.
        if unsafe { libc::mkdirat(at.as_raw_fd(), name.as_ptr(), 0o755) } != 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EEXIST) {
                return Err(io::Error::new(
                    e.kind(),
                    format!("making /{}: {e}", parts.get(..=i).unwrap_or_default().join("/")),
                ));
            }
        }
        let how = OpenHow {
            flags: (libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64,
            mode: 0,
            resolve: RESOLVE_NO_XDEV
                | if i == 0 {
                    RESOLVE_IN_ROOT
                } else {
                    RESOLVE_NO_SYMLINKS
                },
        };
        // SAFETY: openat2(2) of a NUL-terminated name beneath a descriptor held here, with an
        // open_how of the size given.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                at.as_raw_fd(),
                name.as_ptr(),
                &raw const how,
                std::mem::size_of::<OpenHow>(),
            )
        };
        let fd = libc::c_int::try_from(fd)
            .ok()
            .filter(|fd| *fd >= 0)
            .ok_or_else(|| {
                let e = io::Error::last_os_error();
                io::Error::new(
                    e.kind(),
                    format!("/{}: {e}", parts.get(..=i).unwrap_or_default().join("/")),
                )
            })?;
        // SAFETY: the descriptor openat2 just made, owned here alone.
        at = unsafe { OwnedFd::from_raw_fd(fd) };
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A plugged function's node, and a fresh disk's top, are given the user and group the
    /// workload runs as, its effective IDs, not its real or saved ones (proc(5)'s order);
    /// a status that does not name both is no owner.
    #[test]
    fn the_workloads_user_is_its_effective_ids() {
        let status =
            "Name:\ttestguest\nUmask:\t0022\nUid:\t0\t1000\t0\t1000\nGid:\t5\t1001\t5\t1001\nGroups:\t\n";
        assert_eq!(effective_ids(status), Some((1000, 1001)));
        assert_eq!(
            effective_ids("Uid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\n"),
            Some((0, 0))
        );
        assert_eq!(effective_ids("Uid:\t1000\t1000\t1000\t1000\n"), None, "no Gid:");
        assert_eq!(effective_ids("Uid:\t1000\nGid:\t1000\n"), None, "no effective ID");
    }

    /// A plug root is one top-level directory, named once however often the host names it;
    /// one of init's own, a deeper path, or none at all is refused, so nothing is granted
    /// beneath it.
    #[test]
    fn plug_roots_are_top_level_directories_not_inits() {
        let setup = |entries: &[&str]| -> Vec<Vec<u8>> {
            entries
                .iter()
                .map(|e| e.as_bytes().to_vec())
                .chain([b"volume=shards0\0/data\0domains\0".to_vec()])
                .collect()
        };
        let named = |roots: Vec<std::ffi::CString>| -> Vec<String> {
            roots
                .into_iter()
                .map(|r| r.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            named(
                plug_roots(&setup(&[
                    "plug-root=/Users",
                    "plug-root=/Volumes",
                    "plug-root=/Users"
                ]))
                .unwrap()
            ),
            ["Users", "Volumes"]
        );
        assert!(plug_roots(&setup(&[])).unwrap().is_empty());
        for wrong in [
            "plug-root=Users",
            "plug-root=/",
            "plug-root=/Users/ada",
            "plug-root=/Users/",
            "plug-root=/..",
            "plug-root=/.",
            "plug-root=/tmp",
            "plug-root=/run",
            "plug-root=/proc",
            "plug-root=/sys",
            "plug-root=/dev",
            "plug-root=/etc",
            "plug-root=/a\0b",
        ] {
            assert!(plug_roots(&setup(&[wrong])).is_err(), "{wrong:?}");
        }
    }

    /// What is plugged into a domain lies strictly beneath one of the run's plug roots, by a
    /// plain absolute path; anywhere else is refused naming the roots there are, so that no
    /// plug is mounted where Landlock grants the domain nothing, or covers a root's rule.
    #[test]
    fn a_domains_plug_lies_strictly_beneath_a_root() {
        let roots = [
            std::ffi::CString::new("Users").unwrap(),
            std::ffi::CString::new("Volumes").unwrap(),
        ];
        assert_eq!(
            beneath_root("/Users/ada/proj", &roots).unwrap(),
            ["Users", "ada", "proj"]
        );
        assert_eq!(beneath_root("/Volumes/x", &roots).unwrap(), ["Volumes", "x"]);
        let refused = beneath_root("/home/ada", &roots).unwrap_err();
        assert!(refused.contains("/Users, /Volumes"), "{refused}");
        for wrong in [
            "/Users",
            "/",
            "Users/ada",
            "/Users/../etc",
            "/Users/./x",
            "/Users//x",
            "/Users/x/",
            "/tmp/x",
        ] {
            assert!(beneath_root(wrong, &roots).is_err(), "{wrong:?}");
        }
        assert!(
            beneath_root("/Users/ada", &[])
                .unwrap_err()
                .contains("no plug roots")
        );
    }

    /// A domain's processes are found by the PID namespace they run in: two `NSpid:`
    /// numbers, the inner one the domain's; a process of init's own namespace, as each
    /// domain's in-VM server instance is, or of one nested deeper, is none of them.
    #[test]
    fn a_domains_process_is_known_by_its_pid_namespace() {
        assert_eq!(
            domain_pid("Name:\tsh\nNSpid:\t1234\t2\nNSgid:\t1234\t2\n"),
            Some(2)
        );
        assert_eq!(domain_pid("NSpid:\t77\t1\n"), Some(1));
        assert_eq!(domain_pid("NSpid:\t1234\n"), None);
        assert_eq!(domain_pid("NSpid:\t1\t2\t3\n"), None);
        assert_eq!(domain_pid("Name:\tsh\n"), None);
    }

    /// `/dev/iommu` is given back as it was before a function by its device node had it:
    /// taken out where it was made for it, else its owner again; anything else said of it
    /// is refused, so nothing else is given.
    #[test]
    fn iommu_is_given_back_as_it_was() {
        assert_eq!(owner_before("made"), Some(None));
        assert_eq!(owner_before("0:0"), Some(Some((0, 0))));
        assert_eq!(owner_before("1000:50"), Some(Some((1000, 50))));
        for wrong in ["", "root", "0", "0:", ":0", "-1:0", "0:0:0", "made "] {
            assert_eq!(owner_before(wrong), None, "{wrong}");
        }
    }
}
