//! E2E fixtures: pinned real kernels, real guest binaries, a signed VMM, bounded runs.
//! Test-support code: failing loudly is the point, so the no-panic lints are off here.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use shards_image::erofs::{self, DataRef, Kind, Meta, Node, NodeId, Source, Tree};

pub fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// A downloadable test artifact pinned by content hash.
pub struct Artifact {
    pub name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
}

/// The guest architecture: hardware virtualization runs guests of the host's own ISA.
pub const ARCH: &str = std::env::consts::ARCH;

/// Whether this host cannot run VMs: no backend for it yet, or no hardware virtualization
/// (e.g. a CI runner that is itself a VM). VM tests then return early with a SKIP line;
/// `this_host_has_its_hypervisor_backend` pins which hosts must have a backend.
pub fn cannot_run_vms() -> bool {
    match shards_vmm::vm::check_host() {
        Ok(()) => false,
        Err(why) => {
            // Straight to stderr: libtest captures `eprintln!`, and a passing test's
            // captured output is never shown.
            let _ = writeln!(std::io::stderr(), "SKIP: {why}");
            true
        }
    }
}

/// Snapshots exist on arm64 (HVF) so far; tests of them skip elsewhere with a SKIP line.
pub fn cannot_snapshot() -> bool {
    if ARCH == "aarch64" {
        return false;
    }
    let _ = writeln!(
        std::io::stderr(),
        "SKIP: snapshots are not supported on {ARCH} yet"
    );
    true
}

/// Firecracker CI's guest kernel for the host architecture (uncompressed, virtio built in).
/// shards' guest kernel for the host architecture: Linux 6.18.48 with Firecracker's
/// microVM config and ours (resources/kernel), built reproducibly by CI.
pub fn kernel_artifact() -> Artifact {
    match ARCH {
        "aarch64" => Artifact {
            name: "Image-6.18.48-aarch64-1bff175d35cb",
            url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-1bff175d35cb/Image-6.18.48-aarch64",
            sha256: "ed7fb50d27b59e29e9e6c9f57f02c4bb82f8c3f5ecd51bd8083741f77597913b",
        },
        "x86_64" => Artifact {
            name: "vmlinux-6.18.48-x86_64-1bff175d35cb",
            url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-1bff175d35cb/vmlinux-6.18.48-x86_64",
            sha256: "136a182b7013fa32d852a7f227b91f6c113d9ad9dbe7a9b9d4baac7153ddd59c",
        },
        other => panic!("no pinned guest kernel for {other} yet"),
    }
}

/// `CONFIG_NR_CPUS` of both pinned kernels (resources/kernel/firecracker-*-6.18.config,
/// which shards.config leaves alone). A guest brings up at most this many vCPUs and refuses
/// the rest.
pub const KERNEL_NR_CPUS: u32 = 64;

fn sha256(path: &Path) -> String {
    use sha2::Digest;
    let bytes = std::fs::read(path).unwrap();
    sha2::Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Returns the artifact's cached path, downloading and verifying it if needed.
pub fn fetch(a: &Artifact) -> PathBuf {
    let dir = workspace().join("target/artifacts");
    let path = dir.join(a.name);
    if path.exists() && sha256(&path) == a.sha256 {
        return path;
    }
    std::fs::create_dir_all(&dir).unwrap();
    let part = dir.join(format!("{}.{}.part", a.name, std::process::id()));
    let ok = Command::new("curl")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(&part)
        .arg(a.url)
        .status()
        .unwrap();
    assert!(ok.success(), "downloading {}", a.url);
    let got = sha256(&part);
    assert_eq!(got, a.sha256, "{} does not match its pinned hash", a.url);
    std::fs::rename(&part, &path).unwrap();
    path
}

/// The guest kernel: `SHARDS_TEST_KERNEL` if set (to try a kernel before pinning it),
/// else the pinned one.
pub fn kernel() -> &'static Path {
    static K: OnceLock<PathBuf> = OnceLock::new();
    K.get_or_init(|| match std::env::var_os("SHARDS_TEST_KERNEL") {
        Some(path) => PathBuf::from(path),
        None => fetch(&kernel_artifact()),
    })
}

/// Builds guest package `name` (static musl, `guest` profile) and returns its binary.
fn guest_binary(name: &str) -> PathBuf {
    let target_dir = workspace().join("target/guest");
    let guest_target = format!("{ARCH}-unknown-linux-musl");
    // Go through the rustup proxy on PATH (not $CARGO, the bare cargo binary) and drop
    // the dyld paths cargo injects into test processes: the proxy's environment is
    // what lets rust-lld find the toolchain's libLLVM.
    let st = Command::new("cargo")
        .env_remove("DYLD_FALLBACK_LIBRARY_PATH")
        .env_remove("DYLD_LIBRARY_PATH")
        .current_dir(workspace())
        .args([
            "build",
            "-p",
            name,
            "--profile",
            "guest",
            "--target",
            &guest_target,
        ])
        .arg("--target-dir")
        .arg(&target_dir)
        .status()
        .unwrap();
    assert!(st.success(), "building {name}");
    target_dir.join(guest_target).join("guest").join(name)
}

/// The production guest init (PID 1).
pub fn guest_init() -> &'static Path {
    static I: OnceLock<PathBuf> = OnceLock::new();
    I.get_or_init(|| guest_binary("shards-init"))
}

/// The E2E test agent (PID 1 of test VMs).
pub fn test_guest() -> &'static Path {
    static T: OnceLock<PathBuf> = OnceLock::new();
    T.get_or_init(|| guest_binary("shards-testguest"))
}

/// A private copy of the `shards` binary; on macOS, ad-hoc signed with the hypervisor
/// entitlement, without which Hypervisor.framework refuses the process.
pub fn shards() -> &'static Path {
    static V: OnceLock<PathBuf> = OnceLock::new();
    V.get_or_init(|| {
        let dir = workspace().join("target/e2e");
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir
            .join(format!("shards-{}", std::process::id()))
            .with_extension(std::env::consts::EXE_EXTENSION);
        std::fs::copy(env!("CARGO_BIN_EXE_shards"), &copy).unwrap();
        if cfg!(target_os = "macos") {
            let st = Command::new("codesign")
                .arg("--entitlements")
                .arg(workspace().join("resources/hvf.entitlements"))
                .args(["--force", "-s", "-"])
                .arg(&copy)
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(st.success(), "codesign");
        }
        copy
    })
}

pub struct Run {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

impl fmt::Display for Run {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "status {:?} after {:?}\n--- stdout\n{}\n--- stderr\n{}",
            self.status, self.elapsed, self.stdout, self.stderr
        )
    }
}

impl Run {
    /// Parses `shards-timing {"exit_us":N,"markers":[[m,t],...]}` from stderr.
    fn timing(&self) -> Option<&str> {
        self.stderr.lines().find_map(|l| l.strip_prefix("shards-timing "))
    }

    pub fn released_us(&self) -> Option<u128> {
        self.timing_field("released_us")
    }

    pub fn entry_us(&self) -> Option<u128> {
        self.timing_field("entry_us")
    }

    pub fn exit_us(&self) -> Option<u128> {
        self.timing_field("exit_us")
    }

    /// When shards sent a workload its command: the request, for a warm VM.
    pub fn request_us(&self) -> Option<u128> {
        self.timing_field("request_us")
    }

    /// When shards read a workload's exit status.
    pub fn answered_us(&self) -> Option<u128> {
        self.timing_field("answered_us")
    }

    fn timing_field(&self, name: &str) -> Option<u128> {
        let t = self.timing()?;
        let v = t.split(&format!("\"{name}\":")).nth(1)?.split(',').next()?;
        v.parse().ok()
    }

    pub fn marker_us(&self, marker: u32) -> Option<u128> {
        let t = self.timing()?;
        let list = t.split("\"markers\":").nth(1)?;
        list.split("],[").find_map(|pair| {
            let pair = pair.trim_matches(|c| c == '[' || c == ']' || c == '}');
            let (m, at) = pair.split_once(',')?;
            (m.parse::<u32>().ok()? == marker)
                .then(|| at.parse().ok())
                .flatten()
        })
    }
}

/// Runs `shards vm run <args>`, killing it (and failing) after `timeout`.
pub fn vm_run<S: AsRef<std::ffi::OsStr>>(args: &[S], timeout: Duration) -> Run {
    run_shards(&["vm", "run"], args, timeout)
}

/// Runs `shards <command...> <args...>`, killing it (and failing) after `timeout`.
pub fn run_shards<S: AsRef<std::ffi::OsStr>>(command: &[&str], args: &[S], timeout: Duration) -> Run {
    let start = Instant::now();
    let mut child = Command::new(shards())
        .args(command)
        .args(args)
        .env("SHARDS_TIMING", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning shards");
    let collect = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = r.read_to_string(&mut s);
            s
        })
    };
    let out = collect(Box::new(child.stdout.take().unwrap()));
    let err = collect(Box::new(child.stderr.take().unwrap()));
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "shards did not exit within {timeout:?}\n--- stdout\n{}\n--- stderr\n{}",
                out.join().unwrap(),
                err.join().unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    Run {
        status: status.code(),
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap(),
        elapsed: start.elapsed(),
    }
}

/// A fresh directory, short enough for sockaddr_un paths, removed with everything in it
/// (sockets, snapshots) when dropped. Declare it before the VMs that use it.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(name: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!("shards-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\napp:x:1000:1000:app:/home/app:/bin/sh\n";
const GROUP: &str = "root:x:0:\napp:x:1000:\nstaff:x:50:app\n";

struct Files(Vec<Vec<u8>>);

impl Source for Files {
    fn read_at(&mut self, data: DataRef, at: u64, buf: &mut [u8]) -> io::Result<()> {
        let bytes = &self.0[data.source as usize];
        let start = (data.offset + at) as usize;
        buf.copy_from_slice(&bytes[start..start + buf.len()]);
        Ok(())
    }
}

/// A minimal image for workloads: the test guest as /bin/testguest, users, and nothing
/// else. There is no /proc, /sys or /dev: init must make them.
pub fn workload_image(dir: &Path) -> PathBuf {
    let meta = |mode: u16, owner: u32| Meta {
        mode,
        uid: owner,
        gid: owner,
        mtime: 1_700_000_000,
        ..Meta::default()
    };
    let mut tree = Tree::new(meta(0o755, 0));
    let mut files = Files(Vec::new());
    let mut file = |tree: &mut Tree, at: NodeId, name: &str, mode: u16, bytes: Vec<u8>| {
        let data = DataRef {
            source: files.0.len() as u32,
            offset: 0,
        };
        let size = bytes.len() as u64;
        files.0.push(bytes);
        let kind = Kind::File { size, data };
        tree.insert(
            at,
            name.as_bytes(),
            Node {
                kind,
                meta: meta(mode, 0),
            },
        )
        .unwrap();
    };
    let dir_node = |tree: &mut Tree, at: NodeId, name: &str, mode: u16, owner: u32| {
        let kind = Kind::Dir(BTreeMap::new());
        tree.insert(
            at,
            name.as_bytes(),
            Node {
                kind,
                meta: meta(mode, owner),
            },
        )
        .unwrap()
    };
    let bin = dir_node(&mut tree, Tree::ROOT, "bin", 0o755, 0);
    file(
        &mut tree,
        bin,
        "testguest",
        0o755,
        std::fs::read(test_guest()).unwrap(),
    );
    let etc = dir_node(&mut tree, Tree::ROOT, "etc", 0o755, 0);
    file(&mut tree, etc, "passwd", 0o644, PASSWD.into());
    file(&mut tree, etc, "group", 0o644, GROUP.into());
    let home = dir_node(&mut tree, Tree::ROOT, "home", 0o755, 0);
    dir_node(&mut tree, home, "app", 0o755, 1000);
    dir_node(&mut tree, Tree::ROOT, "tmp", 0o1777, 0);
    let path = dir.join("image.erofs");
    let mut out = io::BufWriter::new(std::fs::File::create(&path).unwrap());
    erofs::write(&tree, &mut files, &mut out).unwrap();
    out.flush().unwrap();
    path
}
