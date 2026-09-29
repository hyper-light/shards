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
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use sha2::Digest as _;
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

/// Snapshots exist where this build has a backend (vm::SNAPSHOTS); tests of them skip
/// elsewhere with a SKIP line.
pub fn cannot_snapshot() -> bool {
    if shards_vmm::vm::SNAPSHOTS {
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

/// The `shards` command, beside the `shardsd` it runs (src/bin/shards). Copies of this
/// build's two binaries share a directory named by their SHA-256, so the test processes of
/// one build share them; on macOS `shardsd` is signed with the hypervisor entitlement,
/// without which Hypervisor.framework refuses the process. macOS assesses each new signed
/// binary when it first runs, which would otherwise delay every process's first VM and
/// load the host while tests and benchmarks run.
pub fn shards() -> &'static Path {
    static V: OnceLock<PathBuf> = OnceLock::new();
    V.get_or_init(|| binaries().join(format!("shards{}", std::env::consts::EXE_SUFFIX)))
}

/// The `shardsd` beside [`shards`]: the daemon, pulls and the guest.
pub fn shardsd() -> &'static Path {
    static V: OnceLock<PathBuf> = OnceLock::new();
    V.get_or_init(|| binaries().join(format!("shardsd{}", std::env::consts::EXE_SUFFIX)))
}

/// The `shards-vm` beside [`shards`], for what runs microVMs without the command in front.
pub fn shards_vm() -> &'static Path {
    static V: OnceLock<PathBuf> = OnceLock::new();
    V.get_or_init(|| binaries().join(format!("shards-vm{}", std::env::consts::EXE_SUFFIX)))
}

/// The directory holding this build's `shards`, `shardsd` and `shards-vm`.
fn binaries() -> &'static Path {
    static V: OnceLock<PathBuf> = OnceLock::new();
    V.get_or_init(|| {
        let built = [
            ("shards", Path::new(env!("CARGO_BIN_EXE_shards"))),
            ("shardsd", Path::new(env!("CARGO_BIN_EXE_shardsd"))),
            ("shards-vm", Path::new(env!("CARGO_BIN_EXE_shards-vm"))),
        ];
        let digest: String = {
            use sha2::Digest;
            let mut hash = sha2::Sha256::new();
            for (_, path) in &built {
                hash.update(std::fs::read(path).unwrap());
            }
            hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
        };
        let name = format!("shards-{}", digest.get(..16).unwrap());
        let root = workspace().join("target/e2e");
        let dir = root.join(&name);
        if dir.exists() {
            return dir;
        }
        let temp = root.join(format!("{name}.{}.tmp", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        for (bin, path) in built {
            let copy = temp.join(format!("{bin}{}", std::env::consts::EXE_SUFFIX));
            std::fs::copy(path, &copy).unwrap();
            if cfg!(target_os = "macos") && bin == "shards-vm" {
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
        }
        // Another process may have placed the same build meanwhile: either copy serves.
        if std::fs::rename(&temp, &dir).is_err() {
            assert!(dir.exists(), "{} could not be placed", dir.display());
            let _ = std::fs::remove_dir_all(&temp);
        }
        dir
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

    /// The client's own peak RSS, in KiB, which it adds to the timing line.
    pub fn client_rss_kib(&self) -> Option<u128> {
        self.timing_field("client_rss_kib")
    }

    /// The VMM process's peak RSS when it reported, in KiB.
    pub fn rss_kib(&self) -> Option<u128> {
        self.timing_field("rss_kib")
    }

    /// How many pages of its template's working set the VM prefetched before it ran.
    pub fn prefetched(&self) -> Option<u128> {
        self.timing_field("prefetched")
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
    run_shards_env(command, args, &[], timeout)
}

/// [`run_shards`], with `env` added to shards' environment.
pub fn run_shards_env<S: AsRef<std::ffi::OsStr>>(
    command: &[&str],
    args: &[S],
    env: &[(&str, &std::ffi::OsStr)],
    timeout: Duration,
) -> Run {
    let start = Instant::now();
    let mut child = Command::new(shards())
        .args(command)
        .args(args)
        .envs(env.iter().copied())
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

// A registry image for `shards run`: the test guest, served over loopback HTTP.

/// `sha256:<hex>` of `bytes`.
pub fn sha256_digest(bytes: &[u8]) -> String {
    let hex: String = sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

/// A ustar archive of `(path, mode, uid, contents)`, where no contents means a directory.
pub fn tar(entries: &[(&str, u32, u32, Option<&[u8]>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (path, mode, uid, contents) in entries {
        let mut h = [0u8; 512];
        let name = if contents.is_none() {
            format!("{path}/")
        } else {
            path.to_string()
        };
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..108].copy_from_slice(format!("{mode:07o}\0").as_bytes());
        h[108..116].copy_from_slice(format!("{uid:07o}\0").as_bytes());
        h[116..124].copy_from_slice(format!("{uid:07o}\0").as_bytes());
        let size = contents.map_or(0, <[u8]>::len);
        h[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
        h[136..148].copy_from_slice(b"14500000000\0");
        h[148..156].copy_from_slice(b"        ");
        h[156] = if contents.is_none() { b'5' } else { b'0' };
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        out.extend_from_slice(&h);
        if let Some(data) = contents {
            out.extend_from_slice(data);
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
    }
    out.resize(out.len() + 1024, 0);
    out
}

/// A registry that serves `test/image:v1` (manifest, config, layer) over plain HTTP, and
/// counts the requests it answers.
pub fn registry(manifest: Vec<u8>, blobs: Vec<Vec<u8>>) -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::new(AtomicUsize::new(0));
    let count = served.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (manifest, blobs, count) = (manifest.clone(), blobs.clone(), count.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut out = stream;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut header = String::new();
                    while reader.read_line(&mut header).unwrap_or(0) > 2 {
                        header.clear();
                    }
                    count.fetch_add(1, Ordering::SeqCst);
                    let mut parts = line.split(' ');
                    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                    let body = if path == "/v2/test/image/manifests/v1"
                        || path == format!("/v2/test/image/manifests/{}", sha256_digest(&manifest))
                    {
                        Some((manifest.clone(), "application/vnd.oci.image.manifest.v1+json"))
                    } else {
                        path.strip_prefix("/v2/test/image/blobs/")
                            .and_then(|d| blobs.iter().find(|b| sha256_digest(b) == d))
                            .map(|b| (b.clone(), "application/octet-stream"))
                    };
                    let response = match body {
                        Some((bytes, kind)) => {
                            let mut r = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nDocker-Content-Digest: {}\r\nContent-Length: {}\r\n\r\n",
                                sha256_digest(&bytes),
                                bytes.len()
                            )
                            .into_bytes();
                            if method != "HEAD" {
                                r.extend(bytes);
                            }
                            r
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
                    };
                    if out.write_all(&response).is_err() {
                        return;
                    }
                }
            });
        }
    });
    (port, served)
}

/// The test image: the test guest as its entrypoint, with `report` as its command, run as
/// `app` in `/work`, with its own environment. Returns its manifest and blobs.
pub fn test_image() -> (Vec<u8>, Vec<Vec<u8>>) {
    let guest = std::fs::read(test_guest()).unwrap();
    let passwd = b"root:x:0:0:root:/root:/bin/sh\napp:x:1000:1000:app:/home/app:/bin/sh\n";
    let group = b"root:x:0:\napp:x:1000:\nstaff:x:50:app\n";
    let layer = tar(&[
        ("bin", 0o755, 0, None),
        ("bin/testguest", 0o755, 0, Some(&guest)),
        ("etc", 0o755, 0, None),
        ("etc/passwd", 0o644, 0, Some(passwd)),
        ("etc/group", 0o644, 0, Some(group)),
        ("home", 0o755, 0, None),
        ("home/app", 0o755, 1000, None),
        ("tmp", 0o1777, 0, None),
        ("work", 0o755, 1000, None),
    ]);
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    let config = format!(
        r#"{{"architecture":"{arch}","os":"linux","config":{{"User":"app","Env":["FROM_IMAGE=yes","PATH=/bin"],"Entrypoint":["/bin/testguest"],"Cmd":["report"],"WorkingDir":"/work"}},"rootfs":{{"type":"layers","diff_ids":["{}"]}}}}"#,
        sha256_digest(&layer)
    )
    .into_bytes();
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":{}}}]}}"#,
        sha256_digest(&config),
        config.len(),
        sha256_digest(&layer),
        layer.len()
    )
    .into_bytes();
    (manifest, vec![config, layer])
}

/// Serves the test image at `127.0.0.1:<port>/test/image:v1`.
pub fn served() -> (String, Arc<AtomicUsize>) {
    let (manifest, blobs) = test_image();
    let (port, served) = registry(manifest, blobs);
    (format!("127.0.0.1:{port}/test/image:v1"), served)
}
