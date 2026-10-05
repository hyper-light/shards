//! E2E fixtures: pinned real kernels, real guest binaries, a signed VMM, bounded runs.
//! Test-support code: failing loudly is the point, so the no-panic lints are off here.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

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
use shards_image::erofs::{self, DataRef, Dir, Kind, Meta, Node, NodeId, Source, Tree};

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

/// Whether VM tests must run here: `SHARDS_REQUIRE_VMS=1`, which CI sets on runners that
/// offer their backend's hypervisor, so a test that would skip fails instead.
fn vms_required() -> bool {
    std::env::var_os("SHARDS_REQUIRE_VMS").is_some_and(|v| v == "1")
}

/// A VM test that cannot run here: a SKIP line, or with `SHARDS_REQUIRE_VMS=1` a failure.
fn skip(why: &str) -> bool {
    assert!(
        !vms_required(),
        "SHARDS_REQUIRE_VMS=1, and this VM test cannot run: {why}"
    );
    // Straight to stderr: libtest captures `eprintln!`, and a passing test's captured
    // output is never shown.
    let _ = writeln!(std::io::stderr(), "SKIP: {why}");
    true
}

/// Whether this host cannot run VMs: no backend for it yet, or no hardware virtualization
/// (e.g. a CI runner that is itself a VM). VM tests then return early with a SKIP line,
/// or fail where `SHARDS_REQUIRE_VMS=1` says they must run;
/// `this_host_has_its_hypervisor_backend` pins which hosts must have a backend.
pub fn cannot_run_vms() -> bool {
    match shards_vmm::vm::check_host() {
        Ok(()) => false,
        Err(why) => skip(&why),
    }
}

/// Snapshots exist where this build has a backend (vm::SNAPSHOTS); tests of them skip
/// elsewhere with a SKIP line, as [`cannot_run_vms`] does.
pub fn cannot_snapshot() -> bool {
    if shards_vmm::vm::SNAPSHOTS {
        return false;
    }
    skip(&format!("snapshots are not supported on {ARCH} yet"))
}

/// The kernel shards pins for its guests (src/kernel.rs), which the tests boot too.
#[path = "../../src/kernel.rs"]
pub mod pinned;

/// The file `name` of the snapshot generation `dir` points at (vmm snapshot/mod.rs).
pub fn snapshot_file(dir: &Path, name: &str) -> PathBuf {
    let current = std::fs::read_to_string(dir.join("current")).unwrap();
    dir.join(current.trim_end()).join(name)
}

/// shards' guest kernel for the host architecture: Linux 6.18.48 with Firecracker's
/// microVM config and ours (resources/kernel), built reproducibly by CI.
pub fn kernel_artifact() -> Artifact {
    let Some(k) = pinned::KERNEL else {
        panic!("no pinned guest kernel for {ARCH} yet")
    };
    Artifact {
        name: k.name,
        url: k.url,
        sha256: k.sha256,
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
    // Every failure is retried, a connection reset in the TLS handshake as well: curl's
    // `--retry` alone retries timeouts and some HTTP statuses (curl(1)).
    let ok = Command::new("curl")
        .args(["-fsSL", "--retry", "3", "--retry-all-errors", "-o"])
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
    guest_binary_in(name, "target/guest", &[])
}

/// shards-init built as if for another shards: its contract's identity is not this build's
/// (shards_abi::IDENTITY, crates/abi/build.rs).
pub fn foreign_init() -> &'static Path {
    static F: OnceLock<PathBuf> = OnceLock::new();
    F.get_or_init(|| {
        guest_binary_in(
            "shards-init",
            "target/guest-foreign",
            &[("SHARDS_ABI_IDENTITY", "1")],
        )
    })
}

/// Builds guest package `name` into `target_dir` with `env` added.
fn guest_binary_in(name: &str, target_dir: &str, env: &[(&str, &str)]) -> PathBuf {
    let target_dir = workspace().join(target_dir);
    let guest_target = format!("{ARCH}-unknown-linux-musl");
    // Go through the rustup proxy on PATH (not $CARGO, the bare cargo binary) and drop
    // the dyld paths cargo injects into test processes: the proxy's environment is
    // what lets rust-lld find the toolchain's libLLVM.
    // Linked as build.rs links the init shardsd carries, whatever linker the environment
    // names for the host's own musl builds.
    let linker = format!(
        "CARGO_TARGET_{}_LINKER",
        guest_target.to_ascii_uppercase().replace('-', "_")
    );
    // One build at a time, and this process's own copy of what it made: cargo puts its
    // output in place again on every build, up to date or not, and another test process
    // reading it meanwhile finds it missing.
    std::fs::create_dir_all(&target_dir).unwrap();
    let lock = std::fs::File::create(target_dir.join(".tests.lock")).unwrap();
    lock.lock().unwrap();
    let st = Command::new("cargo")
        .env_remove("DYLD_FALLBACK_LIBRARY_PATH")
        .env_remove("DYLD_LIBRARY_PATH")
        .env(linker, "rust-lld")
        .envs(env.iter().copied())
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
    let built = target_dir.join(&guest_target).join("guest").join(name);
    let mine = target_dir.join("by-process");
    // Copies of test processes that have ended go.
    for e in std::fs::read_dir(&mine).into_iter().flatten().flatten() {
        #[cfg(unix)]
        let gone = e
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<libc::pid_t>().ok())
            // SAFETY: kill(2) with signal 0 only asks whether the process exists.
            .is_some_and(|pid| unsafe { libc::kill(pid, 0) } != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH));
        // Where processes cannot be asked after so, copies stay until the next clean.
        #[cfg(not(unix))]
        let gone = false;
        if gone {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    let own = mine.join(std::process::id().to_string());
    std::fs::create_dir_all(&own).unwrap();
    let copy = own.join(name);
    std::fs::copy(&built, &copy).unwrap();
    drop(lock);
    copy
}

/// The production guest init (PID 1): the one `shardsd` carries, as build.rs made it.
pub fn guest_init() -> &'static Path {
    Path::new(concat!(env!("OUT_DIR"), "/shards-init"))
}

/// The E2E test agent (PID 1 of test VMs).
pub fn test_guest() -> &'static Path {
    static T: OnceLock<PathBuf> = OnceLock::new();
    T.get_or_init(|| guest_binary("shards-testguest"))
}

/// `shards`, the one binary: every command, the daemon included. Copies of this build's
/// binaries share a directory named by their SHA-256, so the test processes of one build
/// share them; on macOS `shards` is signed with the hypervisor entitlement, as releases
/// are. macOS assesses each new signed
/// binary when it first runs, which would otherwise delay every process's first VM and
/// load the host while tests and benchmarks run.
pub fn shards() -> &'static Path {
    static V: OnceLock<PathBuf> = OnceLock::new();
    V.get_or_init(|| binaries().join(format!("shards{}", std::env::consts::EXE_SUFFIX)))
}

/// The VM process, for what runs microVMs without the command in front.
pub fn shards_vm() -> &'static Path {
    static V: OnceLock<PathBuf> = OnceLock::new();
    V.get_or_init(|| carried().join(format!("shards-vm{}", std::env::consts::EXE_SUFFIX)))
}

/// The VM and network processes `shards` carries, as build.rs signed and embedded them:
/// for what starts one without `shards` in front.
fn carried() -> PathBuf {
    PathBuf::from(env!("SHARDS_HELPERS_CARRIED"))
}

/// Waits until no daemon listens in `home`: a daemon killed accepts connections until
/// its listening socket closes, which may be after a client sees its own connection
/// close, as the kernel closes the dead process's descriptors one by one. A client that
/// connects meanwhile hears the daemon hang up.
#[cfg(unix)]
pub fn until_unserved(home: &Path) {
    let socket = home.join("daemon.sock");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "{} is still served",
            socket.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Docker's default bridge as the daemon and the builder elect it on this host
/// (shards_net::bridge): its gateway and guest addresses are what a run sees.
#[cfg(unix)]
pub fn bridge() -> shards_net::bridge::Bridge {
    shards_net::bridge::elected_here(&mut |note| panic!("{note}")).expect("a subnet for the default bridge")
}

/// A TCP port free now at every IPv4 address, below every system's ephemeral range
/// (Linux's from 32768, net.ipv4.ip_local_port_range; macOS's and Windows' from 49152, as
/// RFC 6335 §6 has it): one no other test's connection is given meanwhile, as a port a
/// daemon picked from that range may be the moment it is free. Tried from a place of the
/// process's own, so that tests running beside each other seldom try the same.
pub fn fixed_port() -> u16 {
    const FIRST: u32 = 20_000;
    const SPAN: u32 = 12_000;
    let start = std::process::id() % SPAN;
    (0..SPAN)
        .filter_map(|i| u16::try_from(FIRST + (start + i) % SPAN).ok())
        .find(|&port| std::net::TcpListener::bind(("0.0.0.0", port)).is_ok())
        .expect("a free port below the ephemeral ranges")
}

/// The network process: each networked VM's.
pub fn shards_net() -> &'static Path {
    static V: OnceLock<PathBuf> = OnceLock::new();
    V.get_or_init(|| carried().join(format!("shards-net{}", std::env::consts::EXE_SUFFIX)))
}

/// The directory holding this build's `shards`.
fn binaries() -> &'static Path {
    static V: OnceLock<PathBuf> = OnceLock::new();
    V.get_or_init(|| {
        let built = [("shards", Path::new(env!("CARGO_BIN_EXE_shards")))];
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
        loop {
            if !dir.exists() {
                place(&root, &name, &built);
            }
            // Held while this process lives; another build's copies go once nothing holds
            // them (every mutant's and past build's copy stayed: 3.4 GiB by 2026-10-03).
            if hold(&dir) {
                prune(&root, &name);
                return dir;
            }
        }
    })
}

/// Copies `built` into `root`, as `name`: `shards`, which carries the rest, the VM process
/// signed inside it as a release signs it (build.rs).
fn place(root: &Path, name: &str, built: &[(&str, &Path)]) {
    let dir = root.join(name);
    let temp = root.join(format!("{name}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).unwrap();
    for &(bin, path) in built {
        let copy = temp.join(format!("{bin}{}", std::env::consts::EXE_SUFFIX));
        std::fs::copy(path, &copy).unwrap();
    }
    // Another process may have placed the same build meanwhile: either copy serves.
    if std::fs::rename(&temp, &dir).is_err() {
        assert!(dir.exists(), "{} could not be placed", dir.display());
        let _ = std::fs::remove_dir_all(&temp);
    }
}

/// Holds `dir` shared for as long as this process lives, as long as it is still there
/// once held: one pruned meanwhile is placed again. Not on Windows, which runs no E2E.
fn hold(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::MetadataExt as _;
        static HELD: std::sync::Mutex<Vec<std::fs::File>> = std::sync::Mutex::new(Vec::new());
        let Ok(opened) = std::fs::File::open(dir) else {
            return false;
        };
        // SAFETY: flock(2) on a descriptor this function owns.
        if unsafe { libc::flock(opened.as_raw_fd(), libc::LOCK_SH) } != 0 {
            return false;
        }
        let same = match (std::fs::metadata(dir), opened.metadata()) {
            (Ok(now), Ok(held)) => now.ino() == held.ino() && now.dev() == held.dev(),
            _ => false,
        };
        if same {
            HELD.lock().unwrap().push(opened);
        }
        same
    }
    #[cfg(not(unix))]
    {
        dir.exists()
    }
}

/// Removes the builds' copies in `root` but `keep` that no process holds, and copies left
/// half-placed by processes that are gone.
fn prune(root: &Path, keep: &str) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("shards-") || name == keep {
                continue;
            }
            let path = entry.path();
            if let Some(pid) = name
                .strip_suffix(".tmp")
                .and_then(|rest| rest.rsplit('.').next())
                .and_then(|pid| pid.parse::<libc::pid_t>().ok())
            {
                // SAFETY: kill(2) with signal 0 only asks whether the process exists.
                if pid > 0 && unsafe { libc::kill(pid, 0) } != 0 {
                    let _ = std::fs::remove_dir_all(&path);
                }
                continue;
            }
            let Ok(opened) = std::fs::File::open(&path) else {
                continue;
            };
            // SAFETY: flock(2) on a descriptor this function owns.
            if unsafe { libc::flock(opened.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (root, keep);
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

/// Runs `shards run <args>`, `<args>` starting `--kernel`, killing it (and failing) after `timeout`.
pub fn vm_run<S: AsRef<std::ffi::OsStr>>(args: &[S], timeout: Duration) -> Run {
    run_shards(&["run"], args, timeout)
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
    run_shards_with(command, args, env, None, timeout)
}

/// [`run_shards_env`], in the working directory `dir`.
pub fn run_shards_env_in<S: AsRef<std::ffi::OsStr>>(
    dir: &Path,
    command: &[&str],
    args: &[S],
    env: &[(&str, &std::ffi::OsStr)],
    timeout: Duration,
) -> Run {
    run_shards_with(command, args, env, Some(dir), timeout)
}

/// [`run_shards`], in the working directory `dir`.
pub fn run_shards_in<S: AsRef<std::ffi::OsStr>>(
    dir: &Path,
    command: &[&str],
    args: &[S],
    timeout: Duration,
) -> Run {
    run_shards_with(command, args, &[], Some(dir), timeout)
}

fn run_shards_with<S: AsRef<std::ffi::OsStr>>(
    command: &[&str],
    args: &[S],
    env: &[(&str, &std::ffi::OsStr)],
    dir: Option<&Path>,
    timeout: Duration,
) -> Run {
    let start = Instant::now();
    let mut cmd = Command::new(shards());
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    let mut child = cmd
        .args(command)
        .args(args)
        .envs(env.iter().copied())
        .env("SHARDS_TIMING", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning shards");
    // Read on threads of their own and handed over by channel, so that a pipe another
    // process still holds (a VM given the client's stdio) costs the test its deadline,
    // never a hang.
    let collect = |mut r: Box<dyn Read + Send>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // Lossily: a step's output is printed as it came, bytes that are no UTF-8
            // included, which read_to_string would drop the whole of.
            let mut b = Vec::new();
            let _ = r.read_to_end(&mut b);
            let _ = tx.send(String::from_utf8_lossy(&b).into_owned());
        });
        rx
    };
    let took = |rx: &std::sync::mpsc::Receiver<String>| {
        let left = timeout
            .saturating_sub(start.elapsed())
            .max(Duration::from_secs(1));
        rx.recv_timeout(left)
            .unwrap_or_else(|_| "(still held open by another process)".into())
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
                took(&out),
                took(&err)
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    Run {
        status: status.code(),
        stdout: took(&out),
        stderr: took(&err),
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
        // A failing test's daemon and clients said why in files that go with the
        // directory: shown first, straight to stderr, which libtest does not capture.
        if std::thread::panicking() {
            show_why(&self.0);
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What a home's runs wrote to stderr (`*.stderr`), and its daemon's log's last lines.
fn show_why(home: &Path) {
    let mut err = std::io::stderr().lock();
    let mut said: Vec<PathBuf> = std::fs::read_dir(home)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "stderr"))
        .collect();
    said.sort();
    for path in said {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = writeln!(err, "--- {}\n{text}", path.display());
    }
    let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
    let tail: Vec<&str> = log.lines().rev().take(100).collect();
    let _ = writeln!(
        err,
        "--- {}, its last {} lines",
        home.join("daemon.log").display(),
        tail.len()
    );
    for line in tail.iter().rev() {
        let _ = writeln!(err, "{line}");
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
        let kind = Kind::Dir(Dir::default());
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
    // What real images leave where Docker puts files of its own: the name of the
    // container they were built in, amazonlinux's empty /etc/mtab, and an /etc/hosts that
    // is a link, which a run must replace and never write through.
    file(&mut tree, etc, "hostname", 0o644, b"buildkitsandbox\n".to_vec());
    file(&mut tree, etc, "mtab", 0o644, Vec::new());
    file(&mut tree, etc, "hosts.image", 0o644, b"image hosts\n".to_vec());
    tree.insert(
        etc,
        b"hosts",
        Node {
            kind: Kind::Symlink(Box::from(&b"hosts.image"[..])),
            meta: meta(0o777, 0),
        },
    )
    .unwrap();
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

/// Where a home's image store keeps the root filesystems this build makes.
pub fn rootfs_dir() -> String {
    format!("images/rootfs/v{}", shards_image::store::ROOTFS_VERSION)
}

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

/// A ustar header of a symlink at `path` to `target`, to go into what [`tar`] makes.
pub fn tar_symlink(path: &str, target: &str) -> Vec<u8> {
    let mut h = [0u8; 512];
    h[..path.len()].copy_from_slice(path.as_bytes());
    h[100..108].copy_from_slice(b"0000777\0");
    h[108..116].copy_from_slice(b"0000000\0");
    h[116..124].copy_from_slice(b"0000000\0");
    h[124..136].copy_from_slice(b"00000000000\0");
    h[136..148].copy_from_slice(b"14500000000\0");
    h[148..156].copy_from_slice(b"        ");
    h[156] = b'2';
    h[157..157 + target.len()].copy_from_slice(target.as_bytes());
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    h.to_vec()
}

/// An image a registry serves: its manifest, and its blobs.
pub type Served = Arc<std::sync::Mutex<(Vec<u8>, Vec<Vec<u8>>)>>;

/// A registry that serves `test/image:v1` (manifest, config, layer) over plain HTTP, and
/// counts the requests it answers.
pub fn registry(manifest: Vec<u8>, blobs: Vec<Vec<u8>>) -> (u16, Arc<AtomicUsize>) {
    let (port, served, _) = registry_of(Arc::new(std::sync::Mutex::new((manifest, blobs))));
    (port, served)
}

/// [`registry`], serving what `image` holds whenever it is asked: a test may replace it,
/// and the tag names another image.
pub fn registry_of(image: Served) -> (u16, Arc<AtomicUsize>, Served) {
    registry_serving(image, None)
}

/// [`registry`], never answering a blob's GET: what a pull waits on. Counts the blob
/// GETs begun, and those whose connection then ended.
pub fn registry_stalling_blobs(
    manifest: Vec<u8>,
    blobs: Vec<Vec<u8>>,
) -> (u16, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let stalls = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let image = Arc::new(std::sync::Mutex::new((manifest, blobs)));
    let (port, _, _) = registry_serving(image, Some(stalls.clone()));
    (port, stalls.0, stalls.1)
}

/// [`registry_of`], holding each blob's GET unanswered, counted, with `stalls`.
/// What Go's net/http writes to a connection whose request it cannot read.
pub const GO_BAD_REQUEST: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n400 Bad Request";

fn registry_serving(
    image: Served,
    stalls: Option<(Arc<AtomicUsize>, Arc<AtomicUsize>)>,
) -> (u16, Arc<AtomicUsize>, Served) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::new(AtomicUsize::new(0));
    let count = served.clone();
    let serving = image.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (image, count, stalls) = (serving.clone(), count.clone(), stalls.clone());
            std::thread::spawn(move || {
                // Asked for TLS, it answers as Go's net/http, and so a registry, answers a
                // request it cannot read (server.go, publicErr).
                let mut first = [0u8; 1];
                if stream.peek(&mut first).is_ok_and(|n| n == 1) && first[0] == 0x16 {
                    let _ = (&stream).write_all(GO_BAD_REQUEST);
                    return;
                }
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
                    let (manifest, blobs) = image.lock().unwrap().clone();
                    let mut parts = line.split(' ');
                    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                    if let Some((begun, ended)) = &stalls
                        && path.starts_with("/v2/test/image/blobs/")
                    {
                        begun.fetch_add(1, Ordering::SeqCst);
                        // Never answered: held until the client lets it go.
                        let mut byte = [0u8; 1];
                        while matches!(reader.read(&mut byte), Ok(1)) {}
                        ended.fetch_add(1, Ordering::SeqCst);
                        return;
                    }
                    // A document's own media type: an index's, or a manifest's, OCI's or
                    // Docker's v2.
                    let has = |doc: &[u8], what: &[u8]| doc.windows(what.len()).any(|w| w == what);
                    let kind = |doc: &[u8]| {
                        if has(doc, b"image.index") {
                            "application/vnd.oci.image.index.v1+json"
                        } else if has(doc, b"application/vnd.docker.distribution.manifest.v2+json") {
                            "application/vnd.docker.distribution.manifest.v2+json"
                        } else {
                            "application/vnd.oci.image.manifest.v1+json"
                        }
                    };
                    let body = if path == "/v2/test/image/manifests/v1"
                        || path == format!("/v2/test/image/manifests/{}", sha256_digest(&manifest))
                    {
                        Some((manifest.clone(), kind(&manifest)))
                    } else if let Some(d) = path.strip_prefix("/v2/test/image/manifests/") {
                        // The manifests an index names, kept among the blobs.
                        blobs
                            .iter()
                            .find(|b| sha256_digest(b) == d)
                            .map(|b| (b.clone(), kind(b)))
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
    (port, served, image)
}

/// Serves `body` over plain HTTP on loopback at `/file`, and a redirect to it at
/// `/moved`, as GitHub serves release assets; any other path is a 404. Counts requests.
pub fn serve_file(body: Vec<u8>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::new(AtomicUsize::new(0));
    let count = served.clone();
    let body = Arc::new(body);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (body, count) = (body.clone(), count.clone());
            std::thread::spawn(move || {
                // Asked for TLS, it answers as Go's net/http, and so a registry, answers a
                // request it cannot read (server.go, publicErr).
                let mut first = [0u8; 1];
                if stream.peek(&mut first).is_ok_and(|n| n == 1) && first[0] == 0x16 {
                    let _ = (&stream).write_all(GO_BAD_REQUEST);
                    return;
                }
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
                    let response = match line.split(' ').nth(1).unwrap_or("") {
                        "/file" => {
                            let mut r = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
                                .into_bytes();
                            r.extend_from_slice(&body);
                            r
                        }
                        "/moved" => {
                            b"HTTP/1.1 302 Found\r\nLocation: /file\r\nContent-Length: 0\r\n\r\n".to_vec()
                        }
                        _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
                    };
                    if out.write_all(&response).is_err() {
                        return;
                    }
                }
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), served)
}

/// The test image: the test guest as its entrypoint, with `report` as its command, run as
/// `app` in `/work`, with its own environment. Returns its manifest and blobs.
pub fn test_image() -> (Vec<u8>, Vec<Vec<u8>>) {
    test_image_with(None)
}

/// The test image, with `variant` in `/etc/variant` if given: a layer, and a template, of
/// its own.
pub fn test_image_with(variant: Option<&[u8]>) -> (Vec<u8>, Vec<Vec<u8>>) {
    let guest = std::fs::read(test_guest()).unwrap();
    let passwd = b"root:x:0:0:root:/root:/bin/sh\napp:x:1000:1000:app:/home/app:/bin/sh\n";
    let group = b"root:x:0:\napp:x:1000:\nstaff:x:50:app\n";
    let mut files = vec![
        ("bin", 0o755, 0, None),
        ("bin/testguest", 0o755, 0, Some(&guest[..])),
        ("etc", 0o755, 0, None),
        ("etc/passwd", 0o644, 0, Some(&passwd[..])),
        ("etc/group", 0o644, 0, Some(&group[..])),
        ("home", 0o755, 0, None),
        ("home/app", 0o755, 1000, None),
        ("tmp", 0o1777, 0, None),
        ("work", 0o755, 1000, None),
    ];
    if let Some(v) = variant {
        files.push(("etc/variant", 0o644, 0, Some(v)));
    }
    let layer = tar(&files);
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

/// The test image behind an index, as multi-platform images are served: its manifest
/// for this host's platform, one for another architecture that is never fetched, and an
/// attestation for ours, as BuildKit writes one. Returns the index and its blobs, the
/// manifests among them: the image's config, layer and manifest first, then the
/// attestation's manifest, config and statement, then the other platform's attestation.
pub fn test_index() -> (Vec<u8>, Vec<Vec<u8>>) {
    let (manifest, mut blobs) = test_image();
    let (arch, other) = match std::env::consts::ARCH {
        "aarch64" => ("arm64", "amd64"),
        _ => ("amd64", "arm64"),
    };
    // An attestation as BuildKit writes one: an empty config, and an in-toto statement.
    let (att_config, statement) = (
        b"{}".to_vec(),
        br#"{"_type":"https://in-toto.io/Statement/v0.1"}"#.to_vec(),
    );
    let attestation = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.in-toto+json","digest":"{}","size":{},"annotations":{{"in-toto.io/predicate-type":"https://spdx.dev/Document"}}}}]}}"#,
        sha256_digest(&att_config),
        att_config.len(),
        sha256_digest(&statement),
        statement.len()
    )
    .into_bytes();
    // The other platform's attestation, which a pull for ours never fetches.
    let theirs = attestation.iter().copied().chain(*b" ").collect::<Vec<u8>>();
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{}","size":{},"platform":{{"architecture":"{arch}","os":"linux"}}}},{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:{}","size":1234,"platform":{{"architecture":"{other}","os":"linux"}}}},{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{}","size":{},"annotations":{{"vnd.docker.reference.digest":"{}","vnd.docker.reference.type":"attestation-manifest"}},"platform":{{"architecture":"unknown","os":"unknown"}}}},{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{}","size":{},"annotations":{{"vnd.docker.reference.digest":"sha256:{}","vnd.docker.reference.type":"attestation-manifest"}},"platform":{{"architecture":"unknown","os":"unknown"}}}}]}}"#,
        sha256_digest(&manifest),
        manifest.len(),
        "1".repeat(64),
        sha256_digest(&attestation),
        attestation.len(),
        sha256_digest(&manifest),
        sha256_digest(&theirs),
        theirs.len(),
        "1".repeat(64),
    )
    .into_bytes();
    blobs.push(manifest);
    blobs.extend([attestation, att_config, statement, theirs]);
    (index, blobs)
}

/// Serves the test image with `variant` ([`test_image_with`]) at
/// `127.0.0.1:<port>/test/image:v1`.
pub fn served_variant(variant: &[u8]) -> String {
    let (manifest, blobs) = test_image_with(Some(variant));
    let (port, _) = registry(manifest, blobs);
    format!("127.0.0.1:{port}/test/image:v1")
}

/// Serves the test image at `127.0.0.1:<port>/test/image:v1`.
pub fn served() -> (String, Arc<AtomicUsize>) {
    let (manifest, blobs) = test_image();
    let (port, served) = registry(manifest, blobs);
    (format!("127.0.0.1:{port}/test/image:v1"), served)
}

/// A running `shards` whose output lines arrive on a channel, killed when dropped.
#[cfg(unix)]
pub struct Vm {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
    pub seen: Vec<String>,
}

#[cfg(unix)]
impl Vm {
    pub fn spawn<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> Vm {
        let mut child = Command::new(shards())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        let (out, err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        for stream in [Box::new(out) as Box<dyn Read + Send>, Box::new(err)] {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    let _ = tx.send(line);
                }
            });
        }
        Vm {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// Waits up to `timeout` for an output line containing `text`.
    pub fn wait_for(&mut self, text: &str, timeout: Duration) {
        self.wait_for_any(&[text], timeout);
    }

    /// Waits up to `timeout` for an output line containing any of `texts`; returns it.
    pub fn wait_for_any(&mut self, texts: &[&str], timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    let found = texts.iter().any(|t| line.contains(t));
                    self.seen.push(line.clone());
                    if found {
                        return line;
                    }
                }
                Err(_) => panic!("no {texts:?} from the VM; it said:\n{}", self.seen.join("\n")),
            }
        }
    }

    /// Waits for the process to exit, keeping everything it printed.
    pub fn wait_exit(&mut self) -> Option<i32> {
        let code = self.child.wait().unwrap().code();
        // Its pipes are closed now, so the readers end and the channel with them.
        self.seen.extend(self.lines.iter());
        code
    }

    /// [`wait_exit`](Self::wait_exit), failing, with what it printed, if the process runs
    /// past `timeout`.
    pub fn wait_exit_within(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        while self.child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                let _ = self.child.kill();
                self.wait_exit();
                panic!(
                    "still running after {timeout:?}; it said:\n{}",
                    self.seen.join("\n")
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.wait_exit()
    }
}

#[cfg(unix)]
impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Opens a stream to guest vsock port `port` through the VM's socket `sock`: `Ok` once
/// the guest accepted (the `OK` line was read), `Err` with what came back otherwise.
#[cfg(unix)]
pub fn vsock_connect(
    sock: &Path,
    port: u32,
    timeout: Duration,
) -> Result<std::os::unix::net::UnixStream, String> {
    let mut s =
        std::os::unix::net::UnixStream::connect(sock).map_err(|e| format!("{}: {e}", sock.display()))?;
    s.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    s.write_all(format!("CONNECT {port}\n").as_bytes())
        .map_err(|e| format!("CONNECT: {e}"))?;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.last() != Some(&b'\n') {
        match s.read(&mut byte) {
            Ok(1) => line.push(byte[0]),
            _ => return Err(format!("refused after {:?}", String::from_utf8_lossy(&line))),
        }
    }
    let text = String::from_utf8_lossy(&line);
    let host: u32 = text
        .strip_prefix("OK ")
        .and_then(|p| p.trim().parse().ok())
        .ok_or_else(|| format!("handshake answered {text:?}"))?;
    if host < 1 << 30 {
        return Err(format!("host port {host} outside [2^30, 2^31)"));
    }
    Ok(s)
}

/// Streams `len` bytes of pattern `salt` through the guest's echo on `port` and checks
/// what comes back, byte for byte, after the half-close.
#[cfg(unix)]
pub fn echo(sock: &Path, port: u32, salt: u64, len: usize) -> Result<(), String> {
    let s = vsock_connect(sock, port, Duration::from_secs(60))?;
    let mut w = s.try_clone().map_err(|e| e.to_string())?;
    let writer = std::thread::spawn(move || -> Result<(), String> {
        let mut buf = vec![0u8; 256 * 1024];
        let mut sent = 0;
        while sent < len {
            let n = buf.len().min(len - sent);
            shards_testguest::fill(salt, sent as u64, &mut buf[..n]);
            w.write_all(&buf[..n]).map_err(|e| format!("sending: {e}"))?;
            sent += n;
        }
        w.shutdown(std::net::Shutdown::Write).map_err(|e| e.to_string())
    });
    let mut r = s;
    let mut got = 0usize;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = r
            .read(&mut buf)
            .map_err(|e| format!("stream {salt}: after {got} bytes: {e}"))?;
        if n == 0 {
            break;
        }
        if let Some(i) = shards_testguest::first_mismatch(salt, got as u64, &buf[..n]) {
            return Err(format!("stream {salt}: byte {} came back wrong", got + i));
        }
        got += n;
    }
    writer.join().map_err(|_| "the writer panicked".to_string())??;
    if got != len {
        return Err(format!("stream {salt}: echoed {got} of {len} bytes"));
    }
    Ok(())
}

/// What a writable test registry holds, by repository: blobs by digest, and manifests by
/// tag and by digest, each with its media type.
#[derive(Debug, Default)]
pub struct Repos {
    pub blobs: std::collections::HashMap<String, std::collections::HashMap<String, Vec<u8>>>,
    pub manifests: std::collections::HashMap<String, std::collections::HashMap<String, (String, Vec<u8>)>>,
    /// Each request's method and path, in order.
    pub log: Vec<String>,
}

/// A registry on loopback that takes pushes as the distribution spec has them: a blob's
/// HEAD; an upload begun by a POST, or a mount from another repository, done with a PUT
/// whose digest is checked; a manifest PUT by tag or digest; and the GETs a pull makes.
pub fn writable_registry() -> (u16, Arc<std::sync::Mutex<Repos>>) {
    writable_registry_requiring(None)
}

/// [`writable_registry`], answering every request without `authorization`, when given,
/// as its `Authorization` field, with a Basic challenge.
pub fn writable_registry_requiring(authorization: Option<String>) -> (u16, Arc<std::sync::Mutex<Repos>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let repos = Arc::new(std::sync::Mutex::new(Repos::default()));
    let held = repos.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let repos = held.clone();
            let wanted = authorization.clone();
            std::thread::spawn(move || {
                // Asked for TLS, it answers as Go's net/http, and so a registry, answers a
                // request it cannot read (server.go, publicErr).
                let mut first = [0u8; 1];
                if stream.peek(&mut first).is_ok_and(|n| n == 1) && first[0] == 0x16 {
                    let _ = (&stream).write_all(GO_BAD_REQUEST);
                    return;
                }
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut out = stream;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut length = 0usize;
                    let mut kind = String::new();
                    let mut given = String::new();
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).unwrap_or(0) <= 2 {
                            break;
                        }
                        let (name, value) = header.split_once(':').unwrap_or_default();
                        match name.to_ascii_lowercase().as_str() {
                            "content-length" => length = value.trim().parse().unwrap_or(0),
                            "content-type" => kind = value.trim().to_string(),
                            "authorization" => given = value.trim().to_string(),
                            _ => {}
                        }
                    }
                    let mut body = vec![0u8; length];
                    reader.read_exact(&mut body).unwrap();
                    let mut parts = line.split(' ');
                    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                    let (path, query) = target.split_once('?').unwrap_or((target, ""));
                    let param = |key: &str| {
                        query
                            .split('&')
                            .find_map(|p| p.strip_prefix(&format!("{key}=")))
                            .map(|v| v.replace("%3A", ":"))
                    };
                    if wanted.as_ref().is_some_and(|w| *w != given) {
                        repos.lock().unwrap().log.push(format!("401 {method} {path}"));
                        let _ = out.write_all(
                            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\n\r\n",
                        );
                        continue;
                    }
                    let mut repos = repos.lock().unwrap();
                    repos.log.push(format!("{method} {path}"));
                    let rest = path.strip_prefix("/v2/").unwrap_or("");
                    let (status, headers, reply): (&str, Vec<String>, Vec<u8>) = if path == "/v2/" {
                        ("200 OK", vec![], vec![])
                    } else if let Some((repo, upload)) = rest.split_once("/blobs/uploads/") {
                        let repo = repo.to_string();
                        if method == "POST" {
                            let mounted = param("mount").zip(param("from")).and_then(|(d, from)| {
                                repos
                                    .blobs
                                    .get(&from)
                                    .and_then(|b| b.get(&d))
                                    .cloned()
                                    .map(|b| (d, b))
                            });
                            match mounted {
                                Some((d, b)) => {
                                    repos.blobs.entry(repo.clone()).or_default().insert(d.clone(), b);
                                    (
                                        "201 Created",
                                        vec![format!("Location: /v2/{repo}/blobs/{d}")],
                                        vec![],
                                    )
                                }
                                None => (
                                    "202 Accepted",
                                    vec![format!("Location: /v2/{repo}/blobs/uploads/u{}", repos.log.len())],
                                    vec![],
                                ),
                            }
                        } else if method == "PUT" && !upload.is_empty() {
                            let d = param("digest").unwrap_or_default();
                            if sha256_digest(&body) == d {
                                repos.blobs.entry(repo).or_default().insert(d, body);
                                ("201 Created", vec![], vec![])
                            } else {
                                (
                                    "400 Bad Request",
                                    vec![],
                                    br#"{"errors":[{"code":"DIGEST_INVALID","message":"digest mismatch"}]}"#
                                        .to_vec(),
                                )
                            }
                        } else {
                            ("405 Method Not Allowed", vec![], vec![])
                        }
                    } else if let Some((repo, d)) = rest.split_once("/blobs/") {
                        match repos.blobs.get(repo).and_then(|b| b.get(d)) {
                            Some(b) => (
                                "200 OK",
                                vec![format!("Docker-Content-Digest: {d}")],
                                if method == "HEAD" {
                                    b.len().to_string().into_bytes()
                                } else {
                                    b.clone()
                                },
                            ),
                            None => ("404 Not Found", vec![], vec![]),
                        }
                    } else if let Some((repo, reference)) = rest.split_once("/manifests/") {
                        let repo = repo.to_string();
                        if method == "PUT" {
                            let d = sha256_digest(&body);
                            let m = repos.manifests.entry(repo).or_default();
                            m.insert(reference.to_string(), (kind.clone(), body.clone()));
                            m.insert(d.clone(), (kind.clone(), body));
                            ("201 Created", vec![format!("Docker-Content-Digest: {d}")], vec![])
                        } else {
                            match repos.manifests.get(&repo).and_then(|m| m.get(reference)) {
                                Some((kind, b)) => (
                                    "200 OK",
                                    vec![
                                        format!("Content-Type: {kind}"),
                                        format!("Docker-Content-Digest: {}", sha256_digest(b)),
                                    ],
                                    if method == "HEAD" {
                                        b.len().to_string().into_bytes()
                                    } else {
                                        b.clone()
                                    },
                                ),
                                None => ("404 Not Found", vec![], vec![]),
                            }
                        }
                    } else {
                        ("404 Not Found", vec![], vec![])
                    };
                    drop(repos);
                    // A HEAD says the length it would send, and sends nothing.
                    let (length, sent) = if method == "HEAD" && status.starts_with("200") {
                        (String::from_utf8(reply).unwrap(), Vec::new())
                    } else {
                        (reply.len().to_string(), reply)
                    };
                    let mut response = format!("HTTP/1.1 {status}\r\nContent-Length: {length}\r\n");
                    for h in headers {
                        response.push_str(&h);
                        response.push_str("\r\n");
                    }
                    response.push_str("\r\n");
                    let mut bytes = response.into_bytes();
                    bytes.extend(sent);
                    if out.write_all(&bytes).is_err() {
                        return;
                    }
                }
            });
        }
    });
    (port, repos)
}
