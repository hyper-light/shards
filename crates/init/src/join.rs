//! A container joining the workload's network (`--network container:NAME`, D119): its
//! image a range of the microVM's join disk, its root that image under a tmpfs overlay, its
//! mount, PID, IPC, UTS and cgroup namespaces its own, its network namespace the
//! workload's, as Docker's container network mode gives one container another's network
//! namespace and nothing else (moby daemon/oci_linux.go). It takes the workload's hostname
//! and its `/etc/hostname`, `/etc/hosts` and `/etc/resolv.conf`, the same files, as dockerd
//! gives a joiner the other's (daemon/container_operations.go initializeNetworkingPaths).

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

/// Where a joiner's image lies on the join disk, the workload whose network it joins, and
/// where its writable layer from before lies on the disk, if it has one to put back (D37).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Join {
    pub offset: u64,
    pub len: u64,
    pub workload: libc::pid_t,
    pub layer: Option<(u64, u64)>,
}

/// An image's user database, `/etc/passwd` and `/etc/group`, where it has them.
pub type Users = (Option<Vec<u8>>, Option<Vec<u8>>);

/// A joiner's layers, its image's and its writable one, each a directory's `O_PATH`
/// descriptor kept from before its root moved over them, as init keeps the workload's
/// (changes.rs, `keep`): what its diff, size and commit read.
#[derive(Debug)]
pub struct Kept {
    pub lower: OwnedFd,
    pub upper: OwnedFd,
}

/// What a joiner's child built: its image's users, and its layers kept.
pub type Built = (Users, Kept);

/// The setup entry that makes an exec a joiner: `join=OFFSET,LEN`, its image's bytes on
/// the join disk, which the host gave it.
pub const ENTRY: &[u8] = b"join=";

/// The setup entry naming a joiner's writable layer from before on the join disk:
/// `join-layer=OFFSET,LEN`, an OCI layer as its last run left it (layer.rs), to put back
/// over its root before its command runs again.
pub const LAYER_ENTRY: &[u8] = b"join-layer=";

/// The range `setup`'s join entry names, if it has one; a malformed one is refused.
pub fn range(setup: &[Vec<u8>]) -> Option<Result<(u64, u64), String>> {
    entry_range(setup, ENTRY, "join")
}

/// The range `setup`'s join layer entry names, if it has one; a malformed one is refused.
pub fn layer_range(setup: &[Vec<u8>]) -> Option<Result<(u64, u64), String>> {
    entry_range(setup, LAYER_ENTRY, "join layer")
}

fn entry_range(setup: &[Vec<u8>], prefix: &[u8], what: &str) -> Option<Result<(u64, u64), String>> {
    let entry = setup.iter().find_map(|e| e.strip_prefix(prefix))?;
    let parsed = std::str::from_utf8(entry)
        .ok()
        .and_then(|e| e.split_once(','))
        .and_then(|(o, l)| Some((o.parse::<u64>().ok()?, l.parse::<u64>().ok()?)))
        .filter(|&(o, l)| l > 0 && o.checked_add(l).is_some());
    Some(parsed.ok_or_else(|| format!("a malformed {what} entry")))
}

/// The cgroup of the joiner init knows as exec `id`: beside the workload's, its own, as a
/// container's is.
pub fn cgroup(id: u32) -> String {
    format!("/sys/fs/cgroup/join-{id}")
}

/// Makes joiner `id`'s cgroup, under Docker's device rules, as the workload's is.
pub fn make_cgroup(id: u32) -> Result<String, String> {
    let dir = cgroup(id);
    match std::fs::create_dir(&dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(format!("{dir}: {e}")),
        _ => {}
    }
    crate::devices::confine_joiner(&dir)?;
    Ok(dir)
}

/// Removes joiner `id`'s cgroup once its processes have gone: the kernel refuses while
/// any is left, and that cgroup is let be.
pub fn remove_cgroup(id: u32) {
    let _ = std::fs::remove_dir(cgroup(id));
}

// include/uapi/linux/loop.h and include/uapi/linux/major.h.
const LOOP_SET_FD: libc::c_ulong = 0x4C00;
const LOOP_SET_STATUS64: libc::c_ulong = 0x4C04;
const LOOP_CTL_GET_FREE: libc::c_ulong = 0x4C82;
const LO_FLAGS_READ_ONLY: u32 = 1;
const LO_FLAGS_AUTOCLEAR: u32 = 4;
const LOOP_MAJOR: u32 = 7;
const MISC_MAJOR: u32 = 10;
const LOOP_CTRL_MINOR: u32 = 237;

#[repr(C)]
struct LoopInfo64 {
    lo_device: u64,
    lo_inode: u64,
    lo_rdevice: u64,
    lo_offset: u64,
    lo_sizelimit: u64,
    lo_number: u32,
    lo_encrypt_type: u32,
    lo_encrypt_key_size: u32,
    lo_flags: u32,
    lo_file_name: [u8; 64],
    lo_crypt_name: [u8; 64],
    lo_encrypt_key: [u8; 32],
    lo_init: [u64; 2],
}

/// How long the join disk may take to show its new capacity: the guest's driver reads it
/// on the config-change interrupt, on a workqueue (drivers/block/virtio_blk.c,
/// virtblk_config_changed_work), which the host raised before it gave the joiner's range.
const CAPACITY_WAIT: Duration = Duration::from_secs(5);

/// Where a joiner's child keeps what only it sees as it builds its root: a tmpfs over
/// `/dev` in its own mount namespace.
const SCRATCH: &str = "/dev";

/// The joiner's root, its image's lower layer and its writable one, under [`SCRATCH`].
const ROOT: &str = "/dev/join/root";
const LOWER: &str = "/dev/join/lower";
const RW: &str = "/dev/join/rw";

fn c(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| format!("{s:?} contains NUL"))
}

fn last(what: &str) -> String {
    format!("{what}: {}", io::Error::last_os_error())
}

fn mount(source: &str, target: &str, fstype: &str, flags: libc::c_ulong, data: &str) -> Result<(), String> {
    let (s, t, f, d) = (c(source)?, c(target)?, c(fstype)?, c(data)?);
    // SAFETY: NUL-terminated strings that outlive the call.
    if unsafe { libc::mount(s.as_ptr(), t.as_ptr(), f.as_ptr(), flags, d.as_ptr().cast()) } != 0 {
        return Err(last(&format!("mounting {fstype} on {target}")));
    }
    Ok(())
}

fn mkdir(path: &str) -> Result<(), String> {
    match std::fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) => Err(format!("mkdir {path}: {e}")),
    }
}

fn mknod(path: &str, kind: libc::mode_t, major: u32, minor: u32) -> Result<(), String> {
    let p = c(path)?;
    // SAFETY: a NUL-terminated path.
    if unsafe { libc::mknod(p.as_ptr(), kind | 0o600, libc::makedev(major, minor)) } != 0 {
        return Err(last(&format!("mknod {path}")));
    }
    Ok(())
}

/// The setns(2) of `pid`'s namespace `ns` (`/proc/<pid>/ns/<ns>`), as `kind`.
fn enter(pid: libc::pid_t, ns: &str, kind: libc::c_int) -> Result<(), String> {
    let file = File::open(format!("/proc/{pid}/ns/{ns}"))
        .map_err(|e| format!("the workload's {ns} namespace: {e}"))?;
    // SAFETY: setns(2) on a descriptor this process holds.
    if unsafe { libc::setns(file.as_raw_fd(), kind) } != 0 {
        return Err(last(&format!("joining the workload's {ns} namespace")));
    }
    Ok(())
}

/// The join disk's name under /sys/block, found by its serial, and its device number.
fn join_disk() -> Result<(String, u32, u32), String> {
    let serial = shards_abi::JOIN_DISK_SERIAL.as_bytes();
    for entry in std::fs::read_dir("/sys/block").map_err(|e| format!("/sys/block: {e}"))? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("vd") {
            continue;
        }
        let said = std::fs::read(format!("/sys/block/{name}/serial")).unwrap_or_default();
        if said.trim_ascii_end() != serial {
            continue;
        }
        let dev =
            std::fs::read_to_string(format!("/sys/block/{name}/dev")).map_err(|e| format!("{name}: {e}"))?;
        let (major, minor) = dev
            .trim()
            .split_once(':')
            .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
            .ok_or_else(|| format!("{name}: a device number {dev:?}"))?;
        return Ok((name, major, minor));
    }
    Err("this microVM has no join disk".into())
}

/// Waits until the join disk `name` holds `end` bytes: its capacity, as the driver read
/// it, past the joiner's range.
fn await_capacity(name: &str, end: u64) -> Result<(), String> {
    let deadline = Instant::now() + CAPACITY_WAIT;
    loop {
        let sectors: u64 = std::fs::read_to_string(format!("/sys/block/{name}/size"))
            .map_err(|e| format!("{name}'s size: {e}"))?
            .trim()
            .parse()
            .map_err(|_| format!("{name}'s size is no number"))?;
        if sectors.saturating_mul(512) >= end {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the join disk holds {} bytes, not the {end} its image needs, after {CAPACITY_WAIT:?}",
                sectors.saturating_mul(512)
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A loop device over `len` bytes of the disk at `disk` from `offset`, read-only and let go
/// with its last user (`LO_FLAGS_AUTOCLEAR`): the joiner's root, which ends with its mount
/// namespace. Its node made at `/dev/join/loop`, and returned open: the device lets go of
/// its disk at its last close, so it is held until its mount holds it. With LOOP_SET_FD and
/// LOOP_SET_STATUS64, which every kernel has, rather than LOOP_CONFIGURE (Linux 5.8).
fn loop_device(disk: &str, offset: u64, len: u64) -> Result<(String, File), String> {
    mknod(
        "/dev/join/loop-control",
        libc::S_IFCHR,
        MISC_MAJOR,
        LOOP_CTRL_MINOR,
    )?;
    let control = File::open("/dev/join/loop-control").map_err(|e| format!("loop-control: {e}"))?;
    let backing = File::open(disk).map_err(|e| format!("the join disk: {e}"))?;
    // Another joiner starting may take the free device first: try again.
    for _ in 0..64 {
        // SAFETY: ioctl(2) on loop-control, which takes no argument.
        let n = unsafe { libc::ioctl(control.as_raw_fd(), LOOP_CTL_GET_FREE as _) };
        if n < 0 {
            return Err(last("a free loop device"));
        }
        let n = u32::try_from(n).map_err(|_| "a loop device number out of range")?;
        let path = format!("/dev/join/loop{n}");
        let _ = std::fs::remove_file(&path);
        mknod(&path, libc::S_IFBLK, LOOP_MAJOR, n)?;
        let dev = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .map_err(|e| format!("{path}: {e}"))?;
        // SAFETY: ioctl(2) on the loop device with the backing file's descriptor.
        if unsafe { libc::ioctl(dev.as_raw_fd(), LOOP_SET_FD as _, backing.as_raw_fd()) } != 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EBUSY) {
                continue;
            }
            return Err(last("attaching the join disk to a loop device"));
        }
        let info = LoopInfo64 {
            lo_device: 0,
            lo_inode: 0,
            lo_rdevice: 0,
            lo_offset: offset,
            lo_sizelimit: len,
            lo_number: 0,
            lo_encrypt_type: 0,
            lo_encrypt_key_size: 0,
            lo_flags: LO_FLAGS_READ_ONLY | LO_FLAGS_AUTOCLEAR,
            lo_file_name: [0; 64],
            lo_crypt_name: [0; 64],
            lo_encrypt_key: [0; 32],
            lo_init: [0; 2],
        };
        // SAFETY: ioctl(2) reading `info`, which outlives it.
        if unsafe { libc::ioctl(dev.as_raw_fd(), LOOP_SET_STATUS64 as _, &raw const info) } != 0 {
            return Err(last("setting the joiner's range on its loop device"));
        }
        return Ok((path, dev));
    }
    Err("no loop device stayed free".into())
}

/// Builds joiner `join`'s root and namespaces in this process, a standby init forked as PID
/// 1 of a PID namespace of its own, in init's cgroup: a mount namespace of its own, private;
/// the workload's hostname in a UTS namespace of its own; the workload's network namespace;
/// an IPC namespace of its own; its image's range of the join disk on a loop device, under
/// a tmpfs overlay, its root; the workload's `/etc/hostname`, `/etc/hosts` and
/// `/etc/resolv.conf` over its own; then its cgroup, `cgroup`, and a cgroup namespace
/// rooted there; then the mounts Docker gives a container. Its writable layer from before,
/// if it has one, is put back over its root first. Returns its user database,
/// `/etc/passwd` and `/etc/group`, which init resolves its user against.
pub fn build(join: Join, cgroup: &str) -> Result<Built, String> {
    let end = [Some((join.offset, join.len)), join.layer]
        .into_iter()
        .flatten()
        .try_fold(0u64, |end, (offset, len)| Some(end.max(offset.checked_add(len)?)))
        .ok_or("a join range past the end")?;
    // SAFETY: unshare(2) and mount(2) of NUL-terminated literals, in a single-threaded
    // fork of init's.
    unsafe {
        if libc::unshare(libc::CLONE_NEWNS) != 0 {
            return Err(last("a mount namespace of its own"));
        }
        if libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        ) != 0
        {
            return Err(last("its mounts made private"));
        }
    }
    // The workload's hostname, in a namespace of its own, as Docker's joiner takes the
    // other's (initializeNetworking).
    enter(join.workload, "uts", libc::CLONE_NEWUTS)?;
    enter(join.workload, "net", libc::CLONE_NEWNET)?;
    // SAFETY: unshare(2), in a single-threaded fork of init's.
    if unsafe { libc::unshare(libc::CLONE_NEWUTS | libc::CLONE_NEWIPC) } != 0 {
        return Err(last("its UTS and IPC namespaces"));
    }
    // What only it sees as it builds its root.
    mount("tmpfs", SCRATCH, "tmpfs", libc::MS_NOSUID, "mode=0700")?;
    mkdir("/dev/join")?;
    let (name, major, minor) = join_disk()?;
    await_capacity(&name, end)?;
    mknod("/dev/join/disk", libc::S_IFBLK, major, minor)?;
    // Held open until its mount holds it (`loop_device`).
    let (device, _held) = loop_device("/dev/join/disk", join.offset, join.len)?;
    for dir in [LOWER, RW, ROOT] {
        mkdir(dir)?;
    }
    mount(&device, LOWER, "erofs", libc::MS_RDONLY, "")?;
    mount("tmpfs", RW, "tmpfs", 0, "mode=0755")?;
    for dir in ["/dev/join/rw/upper", "/dev/join/rw/work"] {
        mkdir(dir)?;
    }
    mount(
        "overlay",
        ROOT,
        "overlay",
        0,
        "lowerdir=/dev/join/lower,upperdir=/dev/join/rw/upper,workdir=/dev/join/rw/work,volatile",
    )?;
    // What it changed before, over its image's files, as a run's (D37): before its users
    // are read, which it may have changed, and before anything is mounted in its root.
    if let Some((offset, len)) = join.layer {
        put_back("/dev/join/disk", offset, len)?;
    }
    // Its users, read before anything is mounted over its /etc.
    let users = (
        std::fs::read(format!("{ROOT}/etc/passwd")).ok(),
        std::fs::read(format!("{ROOT}/etc/group")).ok(),
    );
    // Its layers, kept before its root moves over them.
    let open_path = |path: &str| -> Result<OwnedFd, String> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
            .open(path)
            .map(OwnedFd::from)
            .map_err(|e| format!("{path}: {e}"))
    };
    let kept = Kept {
        lower: open_path(LOWER)?,
        upper: open_path("/dev/join/rw/upper")?,
    };
    // The workload's files over its own, the same files: what the workload's run writes in
    // them, it sees.
    etc_dir()?;
    for file in ["hostname", "hosts", "resolv.conf"] {
        let target = format!("{ROOT}/etc/{file}");
        match std::fs::symlink_metadata(&target) {
            Ok(m) if m.is_file() => {}
            Ok(_) => {
                let _ = std::fs::remove_dir_all(&target).or_else(|_| std::fs::remove_file(&target));
                File::create(&target).map_err(|e| format!("{target}: {e}"))?;
            }
            Err(_) => {
                File::create(&target).map_err(|e| format!("{target}: {e}"))?;
            }
        }
        mount(&format!("/etc/{file}"), &target, "", libc::MS_BIND, "")?;
    }
    // Its cgroup, once nothing more is opened outside it: its device rules hold from here.
    std::fs::OpenOptions::new()
        .write(true)
        .open(format!("{cgroup}/cgroup.procs"))
        .and_then(|mut f| f.write_all(b"0"))
        .map_err(|e| format!("its cgroup: {e}"))?;
    // SAFETY: unshare(2), in a single-threaded fork of init's.
    if unsafe { libc::unshare(libc::CLONE_NEWCGROUP) } != 0 {
        return Err(last("its cgroup namespace"));
    }
    pivot()?;
    crate::run::container_mounts().map_err(|f| f.message)?;
    // Its cgroup, read-only, as Docker's containers see theirs.
    mount(
        "cgroup2",
        "/sys/fs/cgroup",
        "cgroup2",
        libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV,
        "",
    )?;
    let mtab = "/etc/mtab";
    let _ = std::fs::remove_file(mtab);
    std::os::unix::fs::symlink("/proc/mounts", mtab).map_err(|e| format!("{mtab}: {e}"))?;
    crate::run::masked().map_err(|f| f.message)?;
    Ok((users, kept))
}

/// Puts back the joiner's writable layer from before, `len` bytes of the join disk at
/// `disk` from `offset`, over its root ([`ROOT`]), as go-archive's ApplyLayer puts a
/// container's back (layer.rs, `apply`). Applied with [`ROOT`] as this process's root, so
/// that no path or link the layer holds resolves past it (chroot(2)); the root it had is
/// taken back after, from a descriptor of it held meanwhile. What it reads of the disk is
/// let go of from the page cache as it goes: read once, it is the joiner's files after.
fn put_back(disk: &str, offset: u64, len: u64) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let disk = File::open(disk).map_err(|e| format!("the join disk: {e}"))?;
    let own = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
        .open("/")
        .map_err(|e| format!("its root: {e}"))?;
    let root = c(ROOT)?;
    // SAFETY: chroot(2) and chdir(2) of NUL-terminated paths, in a single-threaded fork
    // of init's.
    if unsafe { libc::chroot(root.as_ptr()) } != 0 || unsafe { libc::chdir(c"/".as_ptr()) } != 0 {
        return Err(last("entering its root to put back its files"));
    }
    let mut layer = DiskRange {
        disk: &disk,
        at: offset,
        left: len,
    };
    let applied = shards_archive::apply_layer(
        &mut layer,
        std::path::Path::new("/"),
        &shards_archive::UnpackOptions::default(),
    );
    // SAFETY: fchdir(2) to the root this process had, which it holds, then chroot(2) and
    // chdir(2) of literals.
    let back = unsafe {
        libc::fchdir(own.as_raw_fd()) == 0
            && libc::chroot(c".".as_ptr()) == 0
            && libc::chdir(c"/".as_ptr()) == 0
    };
    if !back {
        return Err(last("leaving its root after putting back its files"));
    }
    applied
        .map(drop)
        .map_err(|e| format!("putting back the container's files: {e}"))
}

/// `left` bytes of the join disk from `at`, read in order; each read let go of from the
/// page cache once read (posix_fadvise(2), POSIX_FADV_DONTNEED).
struct DiskRange<'a> {
    disk: &'a File,
    at: u64,
    left: u64,
}

impl io::Read for DiskRange<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use std::os::unix::fs::FileExt;
        let want = usize::try_from(self.left).unwrap_or(usize::MAX).min(buf.len());
        let Some(to) = buf.get_mut(..want).filter(|b| !b.is_empty()) else {
            return Ok(0);
        };
        let n = self.disk.read_at(to, self.at)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the join disk ended before the layer",
            ));
        }
        let (start, read) = (self.at, n as u64);
        // SAFETY: posix_fadvise(2) on a descriptor this process holds; advice only.
        unsafe {
            libc::posix_fadvise(
                self.disk.as_raw_fd(),
                i64::try_from(start).unwrap_or(i64::MAX),
                i64::try_from(read).unwrap_or(0),
                libc::POSIX_FADV_DONTNEED,
            )
        };
        self.at = self.at.saturating_add(read);
        self.left = self.left.saturating_sub(read);
        Ok(n)
    }
}

/// Makes the joiner's `/etc` a directory, as Docker's init layer does over an image that
/// has anything else there.
fn etc_dir() -> Result<(), String> {
    let etc = format!("{ROOT}/etc");
    match std::fs::symlink_metadata(&etc) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => {
            std::fs::remove_file(&etc).map_err(|e| format!("replacing {etc}: {e}"))?;
            mkdir(&etc)
        }
        Err(_) => mkdir(&etc),
    }
}

/// Moves this process's root to the joiner's ([`ROOT`]), and lets the workload's go.
fn pivot() -> Result<(), String> {
    let old = format!("{ROOT}/.old-root");
    mkdir(&old)?;
    let (new, put) = (c(ROOT)?, c(&old)?);
    // SAFETY: pivot_root(2) of NUL-terminated paths, then chdir(2), umount2(2) and rmdir(2)
    // of literals, in a single-threaded fork of init's.
    unsafe {
        if libc::syscall(libc::SYS_pivot_root, new.as_ptr(), put.as_ptr()) != 0 {
            return Err(last("entering its root"));
        }
        if libc::chdir(c"/".as_ptr()) != 0 {
            return Err(last("entering its root"));
        }
        if libc::umount2(c"/.old-root".as_ptr(), libc::MNT_DETACH) != 0 {
            return Err(last("leaving the workload's root"));
        }
        libc::rmdir(c"/.old-root".as_ptr());
    }
    Ok(())
}

/// What a joiner's child says to init once its root is built: its image's user database
/// and its layers' descriptors, or why it has no root. Its length first: the child, forked
/// from init, holds init's own end of the channel until it execs, so the channel's end is
/// no end of the report. The descriptors come with its first byte (`SCM_RIGHTS`).
pub fn report(to: OwnedFd, built: &Result<Built, String>) {
    let mut body = Vec::new();
    match built {
        Ok(((passwd, group), _)) => {
            body.push(1);
            for file in [passwd, group] {
                match file {
                    Some(b) => {
                        body.push(1);
                        body.extend(u32::try_from(b.len()).unwrap_or(u32::MAX).to_be_bytes());
                        body.extend(b.get(..u32::MAX as usize).unwrap_or(b));
                    }
                    None => body.push(0),
                }
            }
        }
        Err(why) => {
            body.push(0);
            body.extend(why.as_bytes());
        }
    }
    let mut bytes = u32::try_from(body.len())
        .unwrap_or(u32::MAX)
        .to_be_bytes()
        .to_vec();
    bytes.extend(body);
    let fds = match built {
        Ok((_, kept)) => vec![kept.lower.as_raw_fd(), kept.upper.as_raw_fd()],
        Err(_) => Vec::new(),
    };
    let _ = send_with(&to, &bytes, &fds);
}

/// Init's side of [`report`]: the joiner's user database and its layers, or why it has no
/// root.
pub fn reported(from: OwnedFd) -> Result<Built, String> {
    let (bytes, mut fds) = receive_with(&from).map_err(|e| format!("joining: {e}"))?;
    let (len, body) = bytes
        .split_first_chunk::<4>()
        .ok_or("joining: its process ended before its root was built")?;
    let body = body
        .get(..u32::from_be_bytes(*len) as usize)
        .ok_or("joining: a truncated report")?;
    let (&ok, mut rest) = body.split_first().ok_or("joining: an empty report")?;
    if ok == 0 {
        return Err(format!("joining: {}", String::from_utf8_lossy(rest)));
    }
    let mut take = || -> Result<Option<Vec<u8>>, String> {
        let (&has, after) = rest.split_first().ok_or("joining: a truncated report")?;
        if has == 0 {
            rest = after;
            return Ok(None);
        }
        let (len, after) = after
            .split_first_chunk::<4>()
            .ok_or("joining: a truncated report")?;
        let len = u32::from_be_bytes(*len) as usize;
        let (file, after) = after.split_at_checked(len).ok_or("joining: a truncated report")?;
        rest = after;
        Ok(Some(file.to_vec()))
    };
    let passwd = take()?;
    let group = take()?;
    let (upper, lower) = (fds.pop(), fds.pop());
    let (Some(lower), Some(upper), true) = (lower, upper, fds.is_empty()) else {
        return Err("joining: its layers did not come with its report".into());
    };
    Ok(((passwd, group), Kept { lower, upper }))
}

/// A channel for [`report`]: init's end to read, the child's to write.
pub fn report_channel() -> Result<(OwnedFd, OwnedFd), String> {
    let mut fds = [0; 2];
    // SAFETY: socketpair(2) filling a two-int array.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    } != 0
    {
        return Err(last("a channel for joining"));
    }
    let [r, w] = fds;
    // SAFETY: fresh descriptors nothing else owns.
    Ok(unsafe { (OwnedFd::from_raw_fd(r), OwnedFd::from_raw_fd(w)) })
}

/// The most descriptors a report carries: a joiner's two layers.
const MAX_FDS: usize = 2;

/// Sends `bytes` on `sock`, `fds` with their first byte (`SCM_RIGHTS`, unix(7)).
fn send_with(sock: &OwnedFd, bytes: &[u8], fds: &[std::os::fd::RawFd]) -> io::Result<()> {
    let mut sent = 0;
    while sent < bytes.len() {
        let rest = bytes.get(sent..).unwrap_or_default();
        let mut iov = libc::iovec {
            iov_base: rest.as_ptr().cast_mut().cast(),
            iov_len: rest.len(),
        };
        let mut control = [0u64; 8];
        // SAFETY: an all-zero msghdr, then filled with buffers of ours that outlive the call.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        if sent == 0 && !fds.is_empty() && fds.len() <= MAX_FDS {
            let data = std::mem::size_of_val(fds);
            msg.msg_control = control.as_mut_ptr().cast();
            // SAFETY: CMSG_SPACE, CMSG_LEN and CMSG_FIRSTHDR of a control buffer large enough
            // for MAX_FDS descriptors, whose header and data are written in it.
            unsafe {
                msg.msg_controllen = libc::CMSG_SPACE(u32::try_from(data).unwrap_or(u32::MAX)) as _;
                let c = libc::CMSG_FIRSTHDR(&raw const msg);
                if c.is_null() {
                    return Err(io::Error::other("no room for the layers' descriptors"));
                }
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN(u32::try_from(data).unwrap_or(u32::MAX)) as _;
                std::ptr::copy_nonoverlapping(fds.as_ptr().cast::<u8>(), libc::CMSG_DATA(c), data);
            }
        }
        // SAFETY: sendmsg(2) of the message just built.
        let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &raw const msg, libc::MSG_NOSIGNAL) };
        match usize::try_from(n) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(n) => sent += n,
            Err(_) => {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

/// Reads `sock` to its end: the bytes, and the descriptors that came with them.
fn receive_with(sock: &OwnedFd) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
    let (mut bytes, mut fds) = (Vec::new(), Vec::new());
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        let mut control = [0u64; 8];
        // SAFETY: as in `send_with`.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&control) as _;
        // SAFETY: recvmsg(2) into the buffers just described, all of ours and alive; with
        // MSG_CMSG_CLOEXEC, what it brings is closed on exec.
        let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &raw mut msg, libc::MSG_CMSG_CLOEXEC) };
        let n = match usize::try_from(n) {
            Ok(n) => n,
            Err(_) => {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
        };
        // SAFETY: CMSG_FIRSTHDR and CMSG_NXTHDR walk the control buffer recvmsg filled; an
        // SCM_RIGHTS header's data is the descriptors it brought, now this process's.
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&raw const msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let len = ((*c).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
                    let count = len / std::mem::size_of::<std::os::fd::RawFd>();
                    let data = libc::CMSG_DATA(c).cast::<std::os::fd::RawFd>();
                    for i in 0..count {
                        let fd = std::ptr::read_unaligned(data.add(i));
                        fds.push(OwnedFd::from_raw_fd(fd));
                    }
                }
                c = libc::CMSG_NXTHDR(&raw const msg, c);
            }
        }
        if n == 0 {
            return Ok((bytes, fds));
        }
        bytes.extend_from_slice(buf.get(..n).unwrap_or_default());
        // The whole report, by its length: init's copy of the child's end may stay open.
        if let Some((len, body)) = bytes.split_first_chunk::<4>()
            && body.len() >= u32::from_be_bytes(*len) as usize
        {
            return Ok((bytes, fds));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_join_entry_names_its_range() {
        let setup = |e: &str| vec![b"caps=1".to_vec(), e.as_bytes().to_vec()];
        assert_eq!(range(&setup("join=1048576,4096")), Some(Ok((1 << 20, 4096))));
        assert_eq!(range(&[b"caps=1".to_vec()]), None);
        // Its layer from before, its own entry, which the image's is not.
        let both = [b"join=0,512".to_vec(), b"join-layer=2097152,1024".to_vec()];
        assert_eq!(range(&both), Some(Ok((0, 512))));
        assert_eq!(layer_range(&both), Some(Ok((2 << 20, 1024))));
        assert_eq!(layer_range(&setup("join=1048576,4096")), None);
        for bad in [
            "join=",
            "join=1",
            "join=a,b",
            "join=1,0",
            "join=18446744073709551615,2",
        ] {
            assert!(range(&setup(bad)).unwrap().is_err(), "{bad}");
            let layer = bad.replacen("join=", "join-layer=", 1);
            assert!(layer_range(&setup(&layer)).unwrap().is_err(), "{layer}");
        }
    }

    #[test]
    fn a_report_says_the_users_and_layers_or_why() {
        let dir = |name: &str| {
            use std::os::unix::fs::OpenOptionsExt;
            let path = std::env::temp_dir().join(format!("shards-join-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            let fd: OwnedFd = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
                .open(&path)
                .unwrap()
                .into();
            (path, fd)
        };
        for users in [
            (Some(b"root:x:0:0::/root:/bin/sh\n".to_vec()), None),
            (None, Some(b"root:x:0:\n".to_vec())),
            (None, None),
        ] {
            let ((lower_path, lower), (upper_path, upper)) = (dir("lower"), dir("upper"));
            let (r, w) = report_channel().unwrap();
            report(w, &Ok((users.clone(), Kept { lower, upper })));
            let (back, kept) = reported(r).unwrap();
            assert_eq!(back, users);
            // The descriptors that came name the directories sent.
            let named =
                |fd: &OwnedFd| std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap();
            assert_eq!((named(&kept.lower), named(&kept.upper)), (lower_path, upper_path));
        }
        let (r, w) = report_channel().unwrap();
        let why = "mounting erofs on /dev/join/lower: Invalid argument".to_string();
        report(w, &Err(why.clone()));
        assert_eq!(reported(r).unwrap_err(), format!("joining: {why}"));
    }
}
