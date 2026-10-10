//! A container joining the workload's network (`--network container:NAME`, D119): its
//! image a range of the microVM's join disk, its root that image under a tmpfs overlay, its
//! mount, PID, IPC, UTS and cgroup namespaces its own, its network namespace the
//! workload's, as Docker's container network mode gives one container another's network
//! namespace and nothing else (moby daemon/oci_linux.go). It takes the workload's hostname
//! and its `/etc/hostname`, `/etc/hosts` and `/etc/resolv.conf`, the same files, as dockerd
//! gives a joiner the other's (daemon/container_operations.go initializeNetworkingPaths).

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

/// Where a joiner's image lies on the join disk, and the workload whose network it joins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Join {
    pub offset: u64,
    pub len: u64,
    pub workload: libc::pid_t,
}

/// An image's user database, `/etc/passwd` and `/etc/group`, where it has them.
pub type Users = (Option<Vec<u8>>, Option<Vec<u8>>);

/// The setup entry that makes an exec a joiner: `join=OFFSET,LEN`, its image's bytes on
/// the join disk, which the host gave it.
pub const ENTRY: &[u8] = b"join=";

/// The range `setup`'s join entry names, if it has one; a malformed one is refused.
pub fn range(setup: &[Vec<u8>]) -> Option<Result<(u64, u64), String>> {
    let entry = setup.iter().find_map(|e| e.strip_prefix(ENTRY))?;
    let parsed = std::str::from_utf8(entry)
        .ok()
        .and_then(|e| e.split_once(','))
        .and_then(|(o, l)| Some((o.parse::<u64>().ok()?, l.parse::<u64>().ok()?)))
        .filter(|&(o, l)| l > 0 && o.checked_add(l).is_some());
    Some(parsed.ok_or_else(|| "a malformed join entry".to_string()))
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
/// rooted there; then the mounts Docker gives a container. Returns its image's user
/// database, `/etc/passwd` and `/etc/group`, which init resolves its user against.
pub fn build(join: Join, cgroup: &str) -> Result<Users, String> {
    let end = join
        .offset
        .checked_add(join.len)
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
    // Its image's users, read before anything is mounted over its /etc.
    let users = (
        std::fs::read(format!("{ROOT}/etc/passwd")).ok(),
        std::fs::read(format!("{ROOT}/etc/group")).ok(),
    );
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
    Ok(users)
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

/// What a joiner's child says to init once its root is built: its image's user database,
/// or why it has no root. Its length first: the child, forked from init, holds init's own
/// copy of the pipe's other end until it execs, so its end is no end of the report.
pub fn report(to: OwnedFd, built: &Result<Users, String>) {
    let mut out = File::from(to);
    let mut bytes = Vec::new();
    match built {
        Ok((passwd, group)) => {
            bytes.push(1);
            for file in [passwd, group] {
                match file {
                    Some(b) => {
                        bytes.push(1);
                        bytes.extend(u32::try_from(b.len()).unwrap_or(u32::MAX).to_be_bytes());
                        bytes.extend(b.get(..u32::MAX as usize).unwrap_or(b));
                    }
                    None => bytes.push(0),
                }
            }
        }
        Err(why) => {
            bytes.push(0);
            bytes.extend(why.as_bytes());
        }
    }
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes();
    let _ = out.write_all(&len).and_then(|()| out.write_all(&bytes));
}

/// Init's side of [`report`]: the joiner's user database, or why it has no root.
pub fn reported(from: OwnedFd) -> Result<Users, String> {
    let mut from = File::from(from);
    let mut len = [0u8; 4];
    from.read_exact(&mut len)
        .map_err(|_| "joining: its process ended before its root was built".to_string())?;
    let mut bytes = vec![0u8; u32::from_be_bytes(len) as usize];
    from.read_exact(&mut bytes)
        .map_err(|e| format!("joining: a truncated report: {e}"))?;
    let (&ok, mut rest) = bytes
        .split_first()
        .ok_or("joining: its process ended before its root was built")?;
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
    Ok((passwd, group))
}

/// A pipe for [`report`]: init's end to read, the child's to write.
pub fn report_pipe() -> Result<(OwnedFd, OwnedFd), String> {
    let mut fds = [0; 2];
    // SAFETY: pipe2(2) filling a two-int array.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(last("a pipe for joining"));
    }
    let [r, w] = fds;
    // SAFETY: fresh descriptors nothing else owns.
    Ok(unsafe { (OwnedFd::from_raw_fd(r), OwnedFd::from_raw_fd(w)) })
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
        for bad in [
            "join=",
            "join=1",
            "join=a,b",
            "join=1,0",
            "join=18446744073709551615,2",
        ] {
            assert!(range(&setup(bad)).unwrap().is_err(), "{bad}");
        }
    }

    #[test]
    fn a_report_says_the_users_or_why() {
        for built in [
            Ok((Some(b"root:x:0:0::/root:/bin/sh\n".to_vec()), None)),
            Ok((None, Some(b"root:x:0:\n".to_vec()))),
            Ok((None, None)),
            Err("mounting erofs on /dev/join/lower: Invalid argument".to_string()),
        ] {
            let (r, w) = report_pipe().unwrap();
            report(w, &built);
            let back = reported(r);
            match built {
                Ok(users) => assert_eq!(back.unwrap(), users),
                Err(why) => assert_eq!(back.unwrap_err(), format!("joining: {why}")),
            }
        }
    }
}
