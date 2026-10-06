//! A container's devices (D44), as dockerd and runc give a container its host's: each
//! `--device` found (moby daemon/oci_linux.go WithDevices, daemon/pkg/oci DevicesFromPath),
//! here among the VM's own, which `/sys/dev` lists; its rules and each
//! `--device-cgroup-rule` (AppendDevicePermissionsFromCgroupRules); the CDI devices, which
//! no VM has a spec for; then the nodes made in the container's `/dev` (runc
//! createDevices) and its cgroup confined to its devices by runc's eBPF program
//! (opencontainers/cgroups devices, `shards-devcgroup`). Init does it all, outside the
//! workload's cgroup, before the standby runs anything there.

use std::ffi::CString;
use std::io;
use std::os::fd::OwnedFd;

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
/// container's `--device` and `--privileged` take nodes from, and a host's are so.
const UDEV_MODES: [(&str, u32); 5] = [
    ("/dev/fuse", 0o666),
    ("/dev/net/tun", 0o666),
    ("/dev/vsock", 0o666),
    ("/dev/vfio/vfio", 0o666),
    ("/dev/rfkill", 0o664),
];

/// The VM's devices, as the kernel names their nodes (each `/sys/dev/{char,block}/M:N`'s
/// uevent: DEVNAME, and DEVMODE, DEVUID and DEVGID where they are not devtmpfs's 0600 and
/// root's), with udev's modes where it has them, sorted as filepath.WalkDir visits a tree: by each path's names in turn.
pub fn vm_devices() -> Vec<Node> {
    let mut out = Vec::new();
    for (dir, kind) in [("/sys/dev/char", Kind::Char), ("/sys/dev/block", Kind::Block)] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(uevent) = std::fs::read_to_string(entry.path().join("uevent")) else {
                continue;
            };
            let field = |k: &str| uevent.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('='));
            let number = |k: &str| field(k).and_then(|v| v.parse::<u32>().ok());
            let (Some(major), Some(minor), Some(name)) = (number("MAJOR"), number("MINOR"), field("DEVNAME"))
            else {
                continue;
            };
            out.push(Node {
                path: format!("/dev/{name}"),
                kind,
                major,
                minor,
                mode: field("DEVMODE")
                    .and_then(|m| u32::from_str_radix(m, 8).ok())
                    .unwrap_or(0o600),
                uid: number("DEVUID").unwrap_or(0),
                gid: number("DEVGID").unwrap_or(0),
            });
        }
    }
    for n in &mut out {
        if let Some((_, mode)) = UDEV_MODES.iter().find(|(p, _)| *p == n.path) {
            n.mode = *mode;
        }
    }
    out.sort_by(|a, b| a.path.split('/').cmp(b.path.split('/')));
    out
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
    devices: Vec<[&'a str; 3]>,
    rules: Vec<&'a str>,
    cdi: Vec<&'a str>,
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
        devices: Vec::new(),
        rules: Vec::new(),
        cdi: Vec::new(),
        io: Vec::new(),
    };
    while let Some(tag) = words.next() {
        match tag {
            "d" => a.devices.push([words.next()?, words.next()?, words.next()?]),
            "r" => a.rules.push(words.next()?),
            "i" => a.cdi.push(words.next()?),
            "w" | "rb" | "wb" | "ri" | "wi" => a.io.push([tag, words.next()?, words.next()?]),
            _ => return None,
        }
    }
    Some(a)
}

/// What a container's devices come to before its cgroup is set: its I/O limits by
/// device number, and the rules of its device filter (none where it is privileged).
pub struct Prepared {
    weights: Vec<String>,
    throttles: Vec<Vec<u8>>,
    rules: Option<Vec<Rule>>,
}

/// What setup entry `entry` asks, as dockerd makes a spec of it (WithResources, then
/// WithDevices) and runc prepares the container's root (createDevices): the devices'
/// numbers found, the CDI devices refused, and the nodes made in the `/dev` init shares
/// with the workload. What failed, as dockerd or runc says it.
pub fn prepare(entry: &[u8]) -> Result<Prepared, String> {
    let a = asked(entry).ok_or("a malformed devices entry")?;
    // Read only where a path is named.
    let vm = if a.devices.is_empty() && a.io.is_empty() {
        Vec::new()
    } else {
        vm_devices()
    };
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
    for [host, container, perms] in &a.devices {
        // A privileged container has the VM's devices where the VM has them already.
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
    for node in &nodes {
        make(node).map_err(|e| format!("error creating device nodes: {e}"))?;
    }
    Ok(Prepared {
        weights,
        throttles,
        rules,
    })
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
        crate::run::write_cgroup(&self.throttles)
    }

    /// runc's setDevices: the filter of its rules compiled, loaded and attached to
    /// `cgroup` in place of Docker's default one, which it leaves where they are the
    /// same; none for a privileged container, which has the default taken away.
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
        _ => libc::S_IFCHR,
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

/// Docker's default filter, as boot attached it: its program, and its descriptor, by which
/// a run's own replaces it.
static DEFAULT: std::sync::OnceLock<(Vec<u8>, OwnedFd)> = std::sync::OnceLock::new();

/// Confines `cgroup` to Docker's default devices (moby's rules and runc's own), as boot
/// does before any snapshot: every run with those rules, the most, then has them already.
pub fn confine_by_default(cgroup: &str) -> Result<(), String> {
    let program = dc::compile(&dc::container(&[], &[], false))?;
    let fd = attach(&program, cgroup, None)?;
    DEFAULT
        .set((program, fd))
        .map_err(|_| "the default device filter, twice".to_string())
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
        let a = asked(b"-\0d\0/dev/fuse\0/dev/x\0r\0r\0c 1:3 r\0i\0v.com/g=0\0").unwrap();
        assert!(!a.privileged);
        assert_eq!(a.devices, [["/dev/fuse", "/dev/x", "r"]]);
        assert_eq!((a.rules, a.cdi), (vec!["c 1:3 r"], vec!["v.com/g=0"]));
        assert!(asked(b"p\0d\0/dev/fuse\0").is_none());
    }
}
