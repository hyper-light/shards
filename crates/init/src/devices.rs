//! A microVM's devices (D44), as dockerd and runc give a container its host's: each
//! `--device` found (moby daemon/oci_linux.go WithDevices, daemon/pkg/oci DevicesFromPath),
//! here among the VM's own, which its devtmpfs holds; its rules and each
//! `--device-cgroup-rule` (AppendDevicePermissionsFromCgroupRules); the CDI devices, which
//! no VM has a spec for; then the nodes made in the workload's `/dev` (runc
//! createDevices) and its cgroup confined to its devices by runc's eBPF program
//! (opencontainers/cgroups devices, `shards-devcgroup`). Init does it all, outside the
//! workload's cgroup, before the standby runs anything there.

use std::ffi::{CString, OsStr};
use std::io::{self, Write as _};
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;

use shards_devcgroup::{self as dc, Kind, Rule};

/// A device node: where, which, and its mode and owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub path: String,
    pub kind: Kind,
    pub major: u32,
    pub minor: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// The modes systemd-udev's default rules give the devices a guest can have
/// (systemd v257 rules.d/50-udev-default.rules.in), over devtmpfs's: the VM is the host a
/// microVM's `--device` and `--privileged` take nodes from, and a host's are so.
const UDEV_MODES: [(&str, u32); 6] = [
    // A GPU's render node: `KERNEL=="renderD*"`, MODE group-render-mode, 0666 by default
    // (meson_options.txt); a guest has one GPU at most (D144).
    ("/dev/dri/renderD128", 0o666),
    ("/dev/fuse", 0o666),
    ("/dev/net/tun", 0o666),
    ("/dev/vsock", 0o666),
    ("/dev/vfio/vfio", 0o666),
    ("/dev/rfkill", 0o664),
];

/// The mode udev's default rules give the VM's device at `path`, where they give one.
pub fn udev_mode(path: &str) -> Option<u32> {
    UDEV_MODES.iter().find(|(p, _)| *p == path).map(|&(_, mode)| mode)
}

/// The VM's devtmpfs, as boot mounted it at `/dev`, before a run's root took that place:
/// its nodes are the VM's devices, as a host's `/dev` holds its own ([`keep_devtmpfs`]).
static DEVTMPFS: std::sync::OnceLock<OwnedFd> = std::sync::OnceLock::new();

/// Keeps the devtmpfs boot mounted at `/dev`, before any snapshot, for [`vm_devices`]: a
/// run's devices are found there by name, where reading every device's uevent cost each
/// run given one 0.8 ms (PM M223).
pub fn keep_devtmpfs() -> io::Result<()> {
    // SAFETY: open(2) of a NUL-terminated literal.
    let fd = unsafe {
        libc::open(
            c"/dev".as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    let _ = DEVTMPFS.set(unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) });
    Ok(())
}

/// A sysfs of init's own, attached nowhere, made at boot where the microVM was booted for
/// the host's VFIO groups (`shards_vfio`, D154), before a run's root mounts its own
/// read-only: Linux's sysfs is one superblock for each network namespace (kernfs,
/// sysfs_init_fs_context), read-only where its first mount is, as the root's would be, and
/// every later mount of it then too. Through it init gives the functions to vfio-pci.
static SYSFS: std::sync::OnceLock<OwnedFd> = std::sync::OnceLock::new();

/// Keeps [`SYSFS`], where the kernel's command line asks (`shards_vfio`).
pub fn keep_sysfs() -> io::Result<()> {
    if std::env::var_os("shards_vfio").is_some() {
        let _ = SYSFS.set(crate::domains::detached(c"sysfs", &[])?);
    }
    Ok(())
}

/// The VM's devices at `path` and beneath it ([`devices_at`] of its devtmpfs); none where
/// boot kept none.
pub fn vm_devices(path: &str) -> Vec<Node> {
    DEVTMPFS
        .get()
        .map(|dev| devices_at(dev, path))
        .unwrap_or_default()
}

/// The devices at `path` and beneath it in `dev`, a `/dev`, as runc finds a host's
/// (DeviceFromPath, and a directory walked as DevicesFromPath's filepath.WalkDir walks
/// one): each node with its numbers, mode and owner, udev's modes over devtmpfs's, sorted
/// as WalkDir visits a tree, by each path's names in turn. None where `path` is no path of
/// `/dev` as the kernel names its nodes: no `.`, `..` or empty name in it.
fn devices_at(dev: &OwnedFd, path: &str) -> Vec<Node> {
    let mut out = Vec::new();
    let rel = match path.trim_end_matches('/').strip_prefix("/dev") {
        Some("") => "",
        Some(rest) => match rest.strip_prefix('/') {
            Some(rel) if rel.split('/').all(|n| !n.is_empty() && n != "." && n != "..") => rel,
            _ => return out,
        },
        None => return out,
    };
    visit(dev, rel, &mut out);
    for n in &mut out {
        if let Some(mode) = udev_mode(&n.path) {
            n.mode = mode;
        }
    }
    out.sort_by(|a, b| a.path.split('/').cmp(b.path.split('/')));
    out
}

/// The node at `rel` in `dev` into `out`, or each beneath it where it is a directory; what
/// cannot be read passed over, as DevicesFromPath's WalkDir passes it over.
fn visit(dev: &OwnedFd, rel: &str, out: &mut Vec<Node>) {
    use std::os::fd::AsRawFd as _;
    let Ok(at) = CString::new(if rel.is_empty() { "." } else { rel }) else {
        return;
    };
    // SAFETY: a zeroed stat(2) buffer, for fstatat(2) to fill.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstatat(2) of a NUL-terminated path beneath a descriptor held open, into a
    // buffer of its own.
    if unsafe { libc::fstatat(dev.as_raw_fd(), at.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return;
    }
    let kind = match st.st_mode & libc::S_IFMT {
        libc::S_IFCHR => Kind::Char,
        libc::S_IFBLK => Kind::Block,
        libc::S_IFDIR => {
            for name in names(dev, &at) {
                let child = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                visit(dev, &child, out);
            }
            return;
        }
        _ => return,
    };
    out.push(Node {
        path: format!("/dev/{rel}"),
        kind,
        major: libc::major(st.st_rdev),
        minor: libc::minor(st.st_rdev),
        mode: st.st_mode & 0o7777,
        uid: st.st_uid,
        gid: st.st_gid,
    });
}

/// The names in directory `at` of `dev`, but `.` and `..`; none where it cannot be read.
fn names(dev: &OwnedFd, at: &std::ffi::CStr) -> Vec<String> {
    use std::os::fd::AsRawFd as _;
    // SAFETY: openat(2) of a NUL-terminated path beneath a descriptor held open.
    let fd = unsafe {
        libc::openat(
            dev.as_raw_fd(),
            at.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Vec::new();
    }
    // SAFETY: fdopendir(3) takes the fresh descriptor, closed with the stream; close(2) of
    // it where no stream took it.
    let dir = unsafe { libc::fdopendir(fd) };
    if dir.is_null() {
        // SAFETY: close(2) of the descriptor this function opened.
        unsafe { libc::close(fd) };
        return Vec::new();
    }
    let mut out = Vec::new();
    loop {
        // SAFETY: readdir(3) of the stream this function holds; each entry's name read
        // before the next call.
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
            break;
        }
        // SAFETY: an entry readdir(3) returned, whose name is NUL-terminated.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        if let Ok(name) = name.to_str()
            && name != "."
            && name != ".."
        {
            out.push(name.to_string());
        }
    }
    // SAFETY: closedir(3) of the stream this function holds, and its descriptor.
    unsafe { libc::closedir(dir) };
    out
}

/// dockerd's words for a device it could not gather, `host` as the run named it.
fn gathering(host: &str, e: &str) -> String {
    format!("error gathering device information while adding custom device \"{host}\": {e}")
}

/// The guest's IOMMU group of its PCI functions at `addresses` (comma-separated, as
/// `0000:00:01.0`): the functions of a host's VFIO group, `host` as the run named it, that
/// the microVM was given behind its own IOMMU, one group of the guest's as of the host's
/// (`vm::vfio_places`). Each is taken from any driver the guest's kernel gave it and
/// given to vfio-pci (sysfs's `driver_override` and `drivers_probe`,
/// Documentation/ABI/testing/sysfs-bus-pci), as the host's were before the run was given
/// their group, through `sys`, a sysfs.
fn vfio_group(sys: BorrowedFd<'_>, host: &str, addresses: &str) -> Result<String, String> {
    let mut group: Option<String> = None;
    for address in addresses.split(',') {
        if !pci_address(address) {
            return Err(gathering(
                host,
                &format!("{address:?} is no PCI function's address"),
            ));
        }
        let dir = format!("bus/pci/devices/{address}");
        let failed = |what: &str, e: io::Error| gathering(host, &format!("{address}: {what}: {e}"));
        let driver = |sys| link_at(sys, &format!("{dir}/driver")).ok();
        let vfio =
            |d: &Option<PathBuf>| d.as_ref().and_then(|d| d.file_name()) == Some(OsStr::new("vfio-pci"));
        let bound = driver(sys);
        if !vfio(&bound) {
            if bound.is_some() {
                write_at(sys, &format!("{dir}/driver/unbind"), address)
                    .map_err(|e| failed("taking it from its driver", e))?;
            }
            write_at(sys, &format!("{dir}/driver_override"), "vfio-pci")
                .map_err(|e| failed("its driver_override", e))?;
            // A probe that no driver takes the function in succeeds too (bus.c,
            // drivers_probe_store): whether vfio-pci did is its driver's link.
            write_at(sys, "bus/pci/drivers_probe", address)
                .map_err(|e| failed("giving it to vfio-pci", e))?;
            if !vfio(&driver(sys)) {
                return Err(gathering(
                    host,
                    &format!("{address}: the guest's vfio-pci did not take it"),
                ));
            }
        }
        let link = link_at(sys, &format!("{dir}/iommu_group")).map_err(|e| {
            failed(
                "its IOMMU group, which the microVM's IOMMU gives it where the guest's kernel has VIRTIO_IOMMU",
                e,
            )
        })?;
        let this = link
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| gathering(host, &format!("{address}: an IOMMU group without a number")))?
            .to_string();
        match &group {
            Some(other) if *other != this => {
                return Err(gathering(
                    host,
                    &format!("its functions are in the guest's groups {other} and {this}, not one"),
                ));
            }
            _ => group = Some(this),
        }
    }
    group.ok_or_else(|| gathering(host, "a group of no functions"))
}

/// Writes `value` to the attribute at `path` beneath `dir`, a sysfs, in one write, as an
/// attribute takes a value.
fn write_at(dir: BorrowedFd<'_>, path: &str, value: &str) -> io::Result<()> {
    let c = CString::new(path).map_err(io::Error::other)?;
    // SAFETY: openat(2) of a NUL-terminated path beneath a descriptor of init's.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor openat just made, owned here alone.
    let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    (&file).write_all(value.as_bytes())
}

/// What the link at `path` beneath `dir` holds.
fn link_at(dir: BorrowedFd<'_>, path: &str) -> io::Result<PathBuf> {
    let c = CString::new(path).map_err(io::Error::other)?;
    let mut buf = [0u8; 4096];
    // SAFETY: readlinkat(2) of a NUL-terminated path into a buffer of the length given.
    let n = unsafe { libc::readlinkat(dir.as_raw_fd(), c.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    let n = usize::try_from(n).map_err(|_| io::Error::last_os_error())?;
    Ok(PathBuf::from(OsStr::from_bytes(buf.get(..n).unwrap_or_default())))
}

/// Whether `a` is a PCI function's address as sysfs names it: `DDDD:BB:SS.F`, in
/// lowercase hex, the function 0 to 7.
fn pci_address(a: &str) -> bool {
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let mut parts = a.splitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(domain), Some(bus), Some(rest)) => match rest.split_once('.') {
            Some((slot, function)) => {
                hex(domain, 4)
                    && hex(bus, 2)
                    && hex(slot, 2)
                    && slot <= "1f"
                    && matches!(function, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7")
            }
            None => false,
        },
        _ => false,
    }
}

/// DevicesFromPath: the device at `host`, or each under it where it is a directory, at
/// `container` instead, with `perms`; or dockerd's refusal. The VM has no files but its
/// devices: a path that names none and holds none is not there.
fn devices_from_path(
    vm: &[Node],
    host: &str,
    container: &str,
    perms: &str,
) -> Result<Vec<(Node, Rule)>, String> {
    let rule = |n: &Node| Rule {
        kind: n.kind,
        major: i64::from(n.major),
        minor: i64::from(n.minor),
        perms: perms_of(perms),
        allow: true,
    };
    let given = host;
    let host = host.trim_end_matches('/');
    if let Some(n) = vm.iter().find(|n| n.path == host) {
        let node = Node {
            path: container.to_string(),
            ..n.clone()
        };
        let r = rule(&node);
        return Ok(vec![(node, r)]);
    }
    let under: Vec<(Node, Rule)> = vm
        .iter()
        .filter_map(|n| {
            let rest = n.path.strip_prefix(host)?.strip_prefix('/')?;
            let node = Node {
                path: format!("{container}/{rest}"),
                ..n.clone()
            };
            let r = rule(&node);
            Some((node, r))
        })
        .collect();
    if under.is_empty() {
        return Err(format!(
            "error gathering device information while adding custom device \"{given}\": no such file or directory"
        ));
    }
    Ok(under)
}

/// devices.Permissions' set.
fn perms_of(s: &str) -> u8 {
    s.chars().fold(0, |acc, c| {
        acc | match c {
            'r' => dc::READ,
            'w' => dc::WRITE,
            'm' => dc::MKNOD,
            _ => 0,
        }
    })
}

/// AppendDevicePermissionsFromCgroupRules' reading of one `--device-cgroup-rule`, and runc
/// specconv's of the rule it makes.
fn cgroup_rule(text: &str) -> Result<Rule, String> {
    let invalid = || format!("invalid device cgroup rule format: '{text}'");
    let number = |s: &str| s == "*" || (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
    let (kind, rest) = text.split_once(' ').ok_or_else(invalid)?;
    let (numbers, access) = rest.split_once(' ').ok_or_else(invalid)?;
    let (major, minor) = numbers.split_once(':').ok_or_else(invalid)?;
    let kind = match kind {
        "a" => Kind::All,
        "b" => Kind::Block,
        "c" => Kind::Char,
        _ => return Err(invalid()),
    };
    if !number(major)
        || !number(minor)
        || !(1..=3).contains(&access.len())
        || !access.bytes().all(|b| matches!(b, b'r' | b'w' | b'm'))
    {
        return Err(invalid());
    }
    let value = |s: &str, which: &str| {
        if s == "*" {
            Ok(-1)
        } else {
            s.parse::<i64>()
                .map_err(|_| format!("invalid {which} value in device cgroup rule format: '{text}'"))
        }
    };
    Ok(Rule {
        kind,
        major: value(major, "major")?,
        minor: value(minor, "minor")?,
        perms: perms_of(access),
        allow: true,
    })
}

/// What a `devices=` setup entry asks for (shards' setup.rs `devices`).
struct Asked<'a> {
    privileged: bool,
    /// The host's VFIO groups the microVM was given (D154): each its path on the host, and
    /// its functions' addresses on the microVM's bus, comma-separated.
    vfio: Vec<[&'a str; 2]>,
    devices: Vec<[&'a str; 3]>,
    rules: Vec<&'a str>,
    cdi: Vec<&'a str>,
    /// CDI's nodes (D146): each its host path, path, cgroup access, kind, mode, owner's
    /// user and group, `-` where its spec gives none.
    cdi_nodes: Vec<[&'a str; 7]>,
    /// `--blkio-weight-device`'s and the four throttles', in that order: each its tag
    /// (`w`, `rb`, `wb`, `ri`, `wi`), path and number.
    io: Vec<[&'a str; 3]>,
}

fn asked(entry: &[u8]) -> Option<Asked<'_>> {
    let text = std::str::from_utf8(entry).ok()?;
    let mut words = text.strip_suffix('\0')?.split('\0');
    let privileged = match words.next()? {
        "p" => true,
        "-" => false,
        _ => return None,
    };
    let mut a = Asked {
        privileged,
        vfio: Vec::new(),
        devices: Vec::new(),
        rules: Vec::new(),
        cdi: Vec::new(),
        cdi_nodes: Vec::new(),
        io: Vec::new(),
    };
    while let Some(tag) = words.next() {
        match tag {
            "d" => a.devices.push([words.next()?, words.next()?, words.next()?]),
            "v" => a.vfio.push([words.next()?, words.next()?]),
            "r" => a.rules.push(words.next()?),
            "i" => a.cdi.push(words.next()?),
            "n" => {
                let mut node = [""; 7];
                for word in &mut node {
                    *word = words.next()?;
                }
                a.cdi_nodes.push(node);
            }
            "w" | "rb" | "wb" | "ri" | "wi" => a.io.push([tag, words.next()?, words.next()?]),
            _ => return None,
        }
    }
    Some(a)
}

/// What a microVM's devices come to before its cgroup is set: its I/O limits by
/// device number, and the rules of its device filter (none where it is privileged); and
/// the nodes still to make, a joiner's (D119), in its own `/dev`.
pub struct Prepared {
    weights: Vec<String>,
    throttles: Vec<Vec<u8>>,
    rules: Option<Vec<Rule>>,
    nodes: Vec<Node>,
    /// CDI's nodes whose spec gives no owner (D146): each its index in `nodes`, and
    /// whether its user and its group are the process's.
    unowned: Vec<(usize, bool, bool)>,
}

/// What setup entry `entry` asks, as dockerd makes a spec of it (WithResources, then
/// WithDevices) and runc prepares a container's root (createDevices): the devices'
/// numbers found, the CDI devices refused, and the nodes made in the `/dev` init shares
/// with the workload. What failed, as dockerd or runc says it.
///
/// `owner` is the process's user and group, which CDI gives a node whose spec gives it none.
pub fn prepare(entry: &[u8], owner: &dyn Fn() -> Result<(u32, u32), String>) -> Result<Prepared, String> {
    let mut prepared = prepare_with(entry)?;
    prepared.own(owner)?;
    make_nodes(&prepared.nodes)?;
    prepared.nodes.clear();
    Ok(prepared)
}

/// [`prepare`] for a joiner (D119), whose nodes are made in its own `/dev`, in its mount
/// namespace ([`Prepared::nodes`]), not in the one init shares with the workload.
pub fn prepare_joiner(entry: &[u8]) -> Result<Prepared, String> {
    prepare_with(entry)
}

fn prepare_with(entry: &[u8]) -> Result<Prepared, String> {
    let a = asked(entry).ok_or("a malformed devices entry")?;
    // Each host VFIO group the microVM was given a group of the guest's now (D154): its
    // functions given to vfio-pci here, as the host's were, through the sysfs boot kept,
    // the run's root's being read-only. A device that names the group's host path, by
    // `--device` or a CDI node, is the group's node in the guest, as the guest's /sys
    // numbers it (Kata's agent does the same, vfio_device_handler.rs), and there in the
    // container too where its path there is the host's.
    let mut groups: Vec<(&str, String)> = Vec::with_capacity(a.vfio.len());
    for [host, addresses] in &a.vfio {
        let sys = SYSFS.get().ok_or_else(|| {
            gathering(
                host,
                "no sysfs to give its functions to vfio-pci in: the microVM was not booted for VFIO (shards_vfio)",
            )
        })?;
        let group = vfio_group(sys.as_fd(), host, addresses)?;
        // A function by its device node (/dev/vfio/devices/vfioN): the guest's own of it,
        // which the guest's vfio-pci made as it took it (sysfs's `vfio-dev`; vfio.rst,
        // "Device cdev Example").
        let node = if host.starts_with("/dev/vfio/devices/") {
            let address = addresses.split(',').next().unwrap_or_default();
            let at = CString::new(format!("bus/pci/devices/{address}/vfio-dev"))
                .map_err(|e| gathering(host, &e.to_string()))?;
            match names(sys, &at).as_slice() {
                [one] => format!("/dev/vfio/devices/{one}"),
                _ => {
                    return Err(gathering(
                        host,
                        &format!(
                            "{address}: no VFIO device node of its own, which the guest's kernel makes where it has VFIO_DEVICE_CDEV"
                        ),
                    ));
                }
            }
        } else {
            format!("/dev/vfio/{group}")
        };
        groups.push((host, node));
    }
    let mapped = |host: &str, container: &str| -> (String, String) {
        let clean = host.trim_end_matches('/');
        match groups.iter().find(|(h, _)| *h == clean) {
            Some((_, node)) if container.trim_end_matches('/') == clean => (node.clone(), node.clone()),
            Some((_, node)) => (node.clone(), container.to_string()),
            None => (host.to_string(), container.to_string()),
        }
    };
    let devices: Vec<[String; 3]> = a
        .devices
        .iter()
        .map(|[host, container, perms]| {
            let (host, container) = mapped(host, container);
            [host, container, (*perms).to_string()]
        })
        .collect();
    let cdi_nodes: Vec<[String; 7]> = a
        .cdi_nodes
        .iter()
        .map(|[host, path, perms, kind, mode, uid, gid]| {
            let (host, path) = mapped(host, path);
            [
                host,
                path,
                (*perms).to_string(),
                (*kind).to_string(),
                (*mode).to_string(),
                (*uid).to_string(),
                (*gid).to_string(),
            ]
        })
        .collect();
    // Read where a path is named, at it and beneath it.
    let mut vm: Vec<Node> = devices
        .iter()
        .map(|[host, ..]| host.as_str())
        .chain(a.io.iter().map(|[_, path, _]| *path))
        .chain(cdi_nodes.iter().map(|[host, ..]| host.as_str()))
        .flat_map(vm_devices)
        .collect();
    vm.sort_by(|a, b| a.path.split('/').cmp(b.path.split('/')));
    vm.dedup_by(|a, b| a.path == b.path);
    // getBlkioWeightDevices and getBlkioThrottleDevices: each path stat(2)ed for its
    // numbers, a directory's 0:0.
    let (mut weights, mut throttles) = (Vec::new(), Vec::new());
    for [tag, path, number] in &a.io {
        let (major, minor) = match vm.iter().find(|n| n.path == *path) {
            Some(n) => (n.major, n.minor),
            None if vm
                .iter()
                .any(|n| n.path.strip_prefix(*path).is_some_and(|r| r.starts_with('/'))) =>
            {
                (0, 0)
            }
            None => return Err(format!("stat {path}: no such file or directory")),
        };
        match *tag {
            "w" => weights.push(format!("{major}:{minor} {number}")),
            t => {
                let name = match t {
                    "rb" => "rbps",
                    "wb" => "wbps",
                    "ri" => "riops",
                    _ => "wiops",
                };
                throttles.push((t, format!("io.max={major}:{minor} {name}={number}").into_bytes()));
            }
        }
    }
    // runc writes them a kind at a time: reads' bytes, writes', reads' operations, writes'.
    let order = |t: &str| ["rb", "wb", "ri", "wi"].iter().position(|k| *k == t);
    throttles.sort_by_key(|(t, _)| order(t));
    let throttles = throttles.into_iter().map(|(_, e)| e).collect();
    let mut nodes = Vec::new();
    let mut given = Vec::new();
    for d in &devices {
        let [host, container, perms] = d.each_ref().map(String::as_str);
        // A privileged microVM's workload has the VM's devices where the VM has them already.
        if a.privileged && host == container {
            continue;
        }
        let perms = if a.privileged { "rwm" } else { perms };
        for (node, rule) in devices_from_path(&vm, host, container, perms)? {
            given.push(dc::Given {
                path: node.path.clone(),
                rule,
            });
            nodes.push(node);
        }
    }
    let mut unowned = Vec::new();
    // CDI's nodes (D146), as CDI's Apply makes them (fillMissingInfo, toOCI): the guest's
    // own device at the host path, a host's numbers meaning nothing in a guest, of the kind
    // the spec says; its mode and owner the spec's, else the device's mode and the
    // process's owner, but root's. A FIFO is made as one, outside the device filter.
    for n in &cdi_nodes {
        let [host, path, perms, kind, mode, uid, gid] = n.each_ref().map(String::as_str);
        let number = |w: &str, radix: u32| -> Result<Option<u32>, String> {
            match w {
                "-" => Ok(None),
                _ => u32::from_str_radix(w, radix)
                    .map(Some)
                    .map_err(|_| format!("CDI device {path}: {w} is no number")),
            }
        };
        let (mode, uid, gid) = (number(mode, 8)?, number(uid, 10)?, number(gid, 10)?);
        // The process's owner, given by `own` once its user is known.
        if uid.is_none() || gid.is_none() {
            unowned.push((nodes.len(), uid.is_none(), gid.is_none()));
        }
        if kind == "p" {
            nodes.push(Node {
                path: path.to_string(),
                kind: Kind::All,
                major: 0,
                minor: 0,
                mode: mode.unwrap_or(0o666),
                uid: uid.unwrap_or(0),
                gid: gid.unwrap_or(0),
            });
            continue;
        }
        let mut found = devices_from_path(&vm, host, path, perms)?;
        let (mut node, rule) = match (found.pop(), found.is_empty()) {
            (Some(one), true) => one,
            _ => {
                return Err(format!(
                    "CDI device {path}: {host} is a directory of devices, not one"
                ));
            }
        };
        match (kind, node.kind) {
            ("-" | "c" | "u", Kind::Char) | ("-" | "b", Kind::Block) => {}
            (k, found) => {
                return Err(format!(
                    "CDI device {path}: specified device type {k:?} does not match host device type {:?}",
                    found.letter().to_string()
                ));
            }
        }
        node.mode = mode.unwrap_or(node.mode);
        node.uid = uid.unwrap_or(0);
        node.gid = gid.unwrap_or(0);
        given.push(dc::Given {
            path: node.path.clone(),
            rule,
        });
        nodes.push(node);
    }
    let rules = if a.privileged {
        None
    } else {
        let asked = a
            .rules
            .iter()
            .map(|r| cgroup_rule(r))
            .collect::<Result<Vec<_>, _>>()?;
        Some(dc::container(&given, &asked, false))
    };
    if !a.cdi.is_empty() {
        return Err(format!(
            "CDI device injection failed: unresolvable CDI devices {}",
            a.cdi.join(", ")
        ));
    }
    Ok(Prepared {
        weights,
        throttles,
        rules,
        nodes,
        unowned,
    })
}

/// Makes `nodes` in the root this process is in, as runc makes a container's.
pub fn make_nodes(nodes: &[Node]) -> Result<(), String> {
    for node in nodes {
        make(node).map_err(|e| format!("error creating device nodes: {e}"))?;
    }
    Ok(())
}

impl Prepared {
    /// runc's setIo, of the devices' limits: each device's weight where BFQ takes one
    /// (bfqDeviceWeightSupported: its weight file says more than a number), then the
    /// throttles to io.max.
    pub fn write_io(&self, cgroup: &str) -> Result<(), String> {
        if !self.weights.is_empty() {
            let bfq = format!("{cgroup}/io.bfq.weight");
            let said = std::fs::read(&bfq).unwrap_or_default();
            let said = String::from_utf8_lossy(said.get(..32).unwrap_or(&said));
            let device_weights = std::fs::metadata(&bfq).is_ok() && said.trim().parse::<i64>().is_err();
            if device_weights {
                use std::io::Write as _;
                let mut f = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&bfq)
                    .map_err(|e| format!("open /sys/fs/cgroup/io.bfq.weight: {}", errno_text(&e)))?;
                for w in &self.weights {
                    f.write_all(format!("{w}\n").as_bytes()).map_err(|e| {
                        format!(
                            "setting device weight {}: write /sys/fs/cgroup/io.bfq.weight: {}",
                            shards_cmdline::go::quote(w),
                            errno_text(&e)
                        )
                    })?;
                }
            }
        }
        crate::run::write_cgroup_at(cgroup, &self.throttles)
    }

    /// The nodes still to make: a joiner's (D119), in its own `/dev`.
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// CDI's nodes whose spec gives no owner (D146), given the process's user and group,
    /// which `owner` says, asked once and only where a node has none: root's where the
    /// process is root's, as CDI leaves such a node to its runtime's default.
    pub fn own(&mut self, owner: &dyn Fn() -> Result<(u32, u32), String>) -> Result<(), String> {
        if self.unowned.is_empty() {
            return Ok(());
        }
        let (uid, gid) = owner()?;
        for &(i, user, group) in &self.unowned {
            if let Some(n) = self.nodes.get_mut(i) {
                if user {
                    n.uid = uid;
                }
                if group {
                    n.gid = gid;
                }
            }
        }
        self.unowned.clear();
        Ok(())
    }

    /// [`attach`](Self::attach) for a joiner's cgroup (D119), whose own filter, `default`,
    /// the joiner's rules take the place of, or which a privileged joiner has taken away;
    /// left where they are the same.
    pub fn attach_joiner(&self, cgroup: &str, default: &Filter) -> Result<(), String> {
        let (bytes, fd) = default;
        let Some(rules) = &self.rules else {
            return detach(fd, cgroup);
        };
        let program = dc::compile(rules)?;
        if program == *bytes {
            return Ok(());
        }
        attach(&program, cgroup, Some(fd)).map(drop)
    }

    /// runc's setDevices: the filter of its rules compiled, loaded and attached to
    /// `cgroup` in place of Docker's default one, which it leaves where they are the
    /// same; none for a privileged microVM, which has the default taken away.
    pub fn attach(&self, cgroup: &str) -> Result<(), String> {
        let default = DEFAULT.get();
        let Some(rules) = &self.rules else {
            return match default {
                Some((_, fd)) => detach(fd, cgroup),
                None => Ok(()),
            };
        };
        let program = dc::compile(rules)?;
        match default {
            Some((bytes, _)) if *bytes == program => Ok(()),
            Some((_, fd)) => attach(&program, cgroup, Some(fd)).map(drop),
            None => attach(&program, cgroup, None).map(drop),
        }
    }
}

fn errno_text(e: &io::Error) -> String {
    shards_cmdline::go::linux_error(e.raw_os_error().unwrap_or(libc::EIO))
}

/// runc's createDeviceNode: its parents made, the node made with its mode and owner; one
/// there already kept, but for a node of runc's own defaults, which a device given at its
/// path takes the place of.
fn make(node: &Node) -> Result<(), String> {
    let err = |e: i32| shards_cmdline::go::linux_error(e);
    if node.path == "/dev/ptmx" {
        return Ok(());
    }
    if let Some(parent) = std::path::Path::new(&node.path).parent() {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(parent)
            .map_err(|e| {
                format!(
                    "mkdir parent of device inode \"{}\": {}",
                    node.path,
                    err(e.raw_os_error().unwrap_or(libc::EIO))
                )
            })?;
    }
    let path = CString::new(node.path.as_str()).map_err(|_| format!("{}: a NUL in its path", node.path))?;
    if dc::RUNC_PATHS.contains(&node.path.as_str()) {
        // SAFETY: unlink(2) of a NUL-terminated path.
        unsafe { libc::unlink(path.as_ptr()) };
    }
    let kind = match node.kind {
        Kind::Block => libc::S_IFBLK,
        Kind::Char => libc::S_IFCHR,
        // A CDI device's FIFO (D146), which no device filter's kind names.
        Kind::All => libc::S_IFIFO,
    };
    // SAFETY: mknod(2), chmod(2) and lchown(2) of a NUL-terminated path.
    unsafe {
        if libc::mknod(
            path.as_ptr(),
            kind | node.mode,
            libc::makedev(node.major, node.minor),
        ) != 0
        {
            let e = io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
            if e == libc::EEXIST {
                return Ok(());
            }
            return Err(format!("mknodat {}: {}", node.path, err(e)));
        }
        if libc::chmod(path.as_ptr(), node.mode) != 0 {
            let e = io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
            return Err(format!(
                "update new {} device inode {} file mode: {}",
                node.kind.letter(),
                node.path,
                err(e)
            ));
        }
        if libc::lchown(path.as_ptr(), node.uid, node.gid) != 0 {
            let e = io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
            return Err(format!(
                "update new {} device inode {} owner: {}",
                node.kind.letter(),
                node.path,
                err(e)
            ));
        }
    }
    Ok(())
}

/// A device filter as attached: its program, and its descriptor, by which another takes
/// its place.
pub type Filter = (Vec<u8>, OwnedFd);

/// Docker's default filter, as boot attached it: its program, and its descriptor, by which
/// a run's own replaces it.
static DEFAULT: std::sync::OnceLock<Filter> = std::sync::OnceLock::new();

/// Confines `cgroup` to Docker's default devices (moby's rules and runc's own), as boot
/// does before any snapshot: every run with those rules, the most, then has them already.
pub fn confine_by_default(cgroup: &str) -> Result<(), String> {
    let program = dc::compile(&dc::container(&[], &[], false))?;
    let fd = attach(&program, cgroup, None)?;
    DEFAULT
        .set((program, fd))
        .map_err(|_| "the default device filter, twice".to_string())
}

/// Confines a joiner's cgroup (D119) to Docker's default devices, as the workload's is:
/// the program and its descriptor, by which the joiner's own devices take its place
/// ([`Prepared::attach_joiner`]).
pub fn confine_joiner(cgroup: &str) -> Result<Filter, String> {
    let program = dc::compile(&dc::container(&[], &[], false))?;
    let fd = attach(&program, cgroup, None)?;
    Ok((program, fd))
}

/// `union bpf_attr` for BPF_PROG_LOAD, as far as runc's load sets it.
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

/// `union bpf_attr` for BPF_PROG_ATTACH.
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
const BPF_PROG_DETACH: libc::c_long = 9;
const BPF_F_ALLOW_MULTI: u32 = 2;
const BPF_F_REPLACE: u32 = 4;

/// runc's loadAttachCgroupDeviceFilter: `program` loaded and attached to `cgroup`, others
/// allowed beneath it (BPF_F_ALLOW_MULTI), in place of `replacing` where one is
/// (BPF_F_REPLACE, as runc replaces its one old program), as cilium/ebpf and runc say it
/// fails. Its descriptor.
fn attach(program: &[u8], cgroup: &str, replacing: Option<&OwnedFd>) -> Result<OwnedFd, String> {
    let err = |what: &str| {
        let e = io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
        format!("{what}: {}", shards_cmdline::go::linux_error(e))
    };
    let license = CString::new(dc::LICENSE).map_err(|_| "a NUL in the license")?;
    let load = ProgLoad {
        prog_type: BPF_PROG_TYPE_CGROUP_DEVICE,
        insn_cnt: u32::try_from(program.len() / 8).map_err(|_| "a program too long")?,
        insns: program.as_ptr() as u64,
        license: license.as_ptr() as u64,
        ..ProgLoad::default()
    };
    // SAFETY: bpf(2) BPF_PROG_LOAD of a program and license that outlive the call; the
    // kernel reads `size_of::<ProgLoad>()` bytes of the attribute.
    let prog = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_PROG_LOAD,
            &raw const load,
            std::mem::size_of::<ProgLoad>(),
        )
    };
    if prog < 0 {
        return Err(err("load program"));
    }
    let prog = i32::try_from(prog).map_err(|_| "a descriptor out of range")?;
    // SAFETY: the descriptor bpf(2) just returned, ours alone.
    let prog = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(prog) };
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(cgroup)
        .map_err(|_| format!("cannot get dir FD for {cgroup}"))?;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;
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
    let attached = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_PROG_ATTACH,
            &raw const at,
            std::mem::size_of::<ProgAttach>(),
        )
    };
    if attached != 0 {
        return Err(err(
            "failed to call BPF_PROG_ATTACH (BPF_CGROUP_DEVICE, BPF_F_ALLOW_MULTI)",
        ));
    }
    Ok(prog)
}

/// The program `fd` taken off `cgroup` (runc's closer: BPF_PROG_DETACH).
fn detach(fd: &OwnedFd, cgroup: &str) -> Result<(), String> {
    use std::os::fd::AsRawFd as _;
    let dir = std::fs::File::open(cgroup).map_err(|_| format!("cannot get dir FD for {cgroup}"))?;
    let at = ProgAttach {
        target_fd: u32::try_from(dir.as_raw_fd()).map_err(|_| "a descriptor out of range")?,
        attach_bpf_fd: u32::try_from(fd.as_raw_fd()).map_err(|_| "a descriptor out of range")?,
        attach_type: BPF_CGROUP_DEVICE,
        ..ProgAttach::default()
    };
    // SAFETY: bpf(2) BPF_PROG_DETACH of descriptors held open over the call.
    let detached = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_PROG_DETACH,
            &raw const at,
            std::mem::size_of::<ProgAttach>(),
        )
    };
    if detached != 0 {
        let e = io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
        return Err(format!(
            "failed to call BPF_PROG_DETACH (BPF_CGROUP_DEVICE): {}",
            shards_cmdline::go::linux_error(e)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(path: &str, kind: Kind, major: u32, minor: u32) -> Node {
        Node {
            path: path.into(),
            kind,
            major,
            minor,
            mode: 0o666,
            uid: 0,
            gid: 0,
        }
    }

    /// DevicesFromPath over the VM's devices: a device, a directory's, none.
    #[test]
    fn devices_are_found_as_dockerd_finds_a_hosts() {
        let vm = vec![
            node("/dev/fuse", Kind::Char, 10, 229),
            node("/dev/net/tun", Kind::Char, 10, 200),
            node("/dev/net2", Kind::Char, 1, 1),
        ];
        let found = devices_from_path(&vm, "/dev/fuse", "/dev/x", "r").unwrap();
        assert_eq!(found[0].0.path, "/dev/x");
        assert_eq!(found[0].1.perms, dc::READ);
        let found = devices_from_path(&vm, "/dev/net", "/n", "rwm").unwrap();
        assert_eq!(
            found.iter().map(|f| f.0.path.as_str()).collect::<Vec<_>>(),
            ["/n/tun"]
        );
        assert_eq!(
            devices_from_path(&vm, "/dev/nope", "/dev/nope", "rwm").unwrap_err(),
            "error gathering device information while adding custom device \"/dev/nope\": no such file or directory"
        );
    }

    /// The devices at a path of `/dev` and beneath it, from a descriptor of it, as runc's
    /// walk of a host's finds them, in WalkDir's order; none at a path that is no path of
    /// `/dev` as the kernel names its nodes, a link, or nothing.
    #[test]
    fn devices_are_found_in_dev_by_name() {
        // SAFETY: open(2) of a NUL-terminated literal.
        let fd = unsafe {
            libc::open(
                c"/dev".as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        assert!(fd >= 0, "{}", io::Error::last_os_error());
        // SAFETY: a fresh descriptor nothing else owns.
        let dev = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let null = devices_at(&dev, "/dev/null");
        assert_eq!(null.len(), 1, "{null:?}");
        assert_eq!(
            (null[0].path.as_str(), null[0].kind, null[0].major, null[0].minor),
            ("/dev/null", Kind::Char, 1, 3)
        );
        assert_eq!(devices_at(&dev, "/dev/null/"), null);
        let pts = devices_at(&dev, "/dev/pts");
        assert!(
            !pts.is_empty() && pts.iter().all(|n| n.path.starts_with("/dev/pts/")),
            "{pts:?}"
        );
        let all = devices_at(&dev, "/dev");
        assert!(
            all.contains(&null[0]) && pts.iter().all(|n| all.contains(n)),
            "{all:?}"
        );
        let mut walked = all.clone();
        walked.sort_by(|a, b| a.path.split('/').cmp(b.path.split('/')));
        assert_eq!(all, walked);
        for none in [
            "/dev/./null",
            "/dev/../dev/null",
            "/dev//null",
            "/devnull",
            "/etc/passwd",
            "/dev/stdin",
            "/dev/nope",
        ] {
            assert_eq!(devices_at(&dev, none), Vec::new(), "{none}");
        }
    }

    /// AppendDevicePermissionsFromCgroupRules' refusals.
    #[test]
    fn cgroup_rules_are_read_as_dockerd_reads_them() {
        assert_eq!(cgroup_rule("c 1:3 r").unwrap().perms, dc::READ);
        assert_eq!(cgroup_rule("b *:* rwm").unwrap().major, -1);
        assert_eq!(
            cgroup_rule("c 1:3").unwrap_err(),
            "invalid device cgroup rule format: 'c 1:3'"
        );
        assert_eq!(
            cgroup_rule("c 99999999999999999999:1 r").unwrap_err(),
            "invalid major value in device cgroup rule format: 'c 99999999999999999999:1 r'"
        );
    }

    #[test]
    fn entries_are_read_as_setup_writes_them() {
        let a = asked(
            b"-\0v\0/dev/vfio/42\x000000:00:01.0,0000:00:01.1\0d\0/dev/fuse\0/dev/x\0r\0r\0c 1:3 r\0i\0v.com/g=0\0",
        )
        .unwrap();
        assert!(!a.privileged);
        assert_eq!(a.vfio, [["/dev/vfio/42", "0000:00:01.0,0000:00:01.1"]]);
        assert_eq!(a.devices, [["/dev/fuse", "/dev/x", "r"]]);
        assert_eq!((a.rules, a.cdi), (vec!["c 1:3 r"], vec!["v.com/g=0"]));
        assert!(asked(b"p\0d\0/dev/fuse\0").is_none());
        assert!(asked(b"-\0v\0/dev/vfio/42\0").is_none(), "a word short");
    }

    /// A function's address is sysfs's: domain, bus and slot in lowercase hex, the slot
    /// one of 32, the function one of 8.
    #[test]
    fn a_function_is_named_as_sysfs_names_it() {
        for good in ["0000:00:01.0", "0001:3b:1f.7", "ffff:ff:00.3"] {
            assert!(pci_address(good), "{good}");
        }
        for bad in [
            "0000:00:01",
            "0000:00:01.8",
            "0000:00:20.0",
            "0000:00:01.0,",
            "000:00:01.0",
            "0000:0:01.0",
            "0000:00:0A.0",
            "0000:00:01.0:0",
            "../../x",
            "",
        ] {
            assert!(!pci_address(bad), "{bad}");
        }
    }
}
