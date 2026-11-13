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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use sha2::Digest as _;
use shards_image::erofs::{self, DataRef, Dir, Kind, Meta, Node, NodeId, Source, Tree};

/// Records in `home` of a host whose restarts go over a kept layer quicker than they pack
/// it from `from` bytes, and pack it quicker below (D190's `layers::Restarts`, the home's
/// `restarts`): a thousand restarts each way, of layers holding 0 to 64 MiB evenly, on two
/// exact lines meeting at `from`, packing's the steeper, so that the few a test's own
/// restarts add move neither. The home's daemon is stopped, as the next reads them.
pub fn restarts_cross_at(home: &Path, from: u64, timeout: Duration) {
    let mib = f64::from(1u32 << 20);
    let (n, top) = (1000.0f64, 64.0 * mib);
    let (mean, xx) = (top / 2.0, (top / (n - 1.0)).powi(2) * n * (n * n - 1.0) / 12.0);
    let from = from as f64;
    // Packing 50 ms and 10 a MiB; going over the same at `from`, 2 a MiB, in µs.
    let (packed, over) = (
        (50e3, 10e3 / mib),
        (50e3 + 10e3 / mib * from - 2e3 / mib * from, 2e3 / mib),
    );
    let line = |(at_zero, slope): (f64, f64)| {
        format!(
            "{n} {mean} {} {xx} {} {}",
            at_zero + slope * mean,
            slope * xx,
            slope * slope * xx
        )
    };
    std::fs::write(
        home.join("restarts"),
        format!("packed {}\nover {}\n", line(packed), line(over)),
    )
    .unwrap();
    let env = [("SHARDS_HOME", home.as_os_str())];
    let _ = run_shards_env(&["daemon"], &["stop"], &env, timeout);
}

/// Which file is at `path`: its device and inode on Unix, its volume's serial number and
/// its index on that volume on Windows (`platform::file_id`), so that a test tells the same
/// file from another put in its place.
pub fn file_identity(path: &Path) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let meta = std::fs::metadata(path).unwrap();
        (meta.dev(), meta.ino())
    }
    #[cfg(windows)]
    {
        let file = shards_vmm::platform::open_for_metadata(path).unwrap();
        shards_vmm::platform::file_id(&file).unwrap()
    }
}

/// A datagram from `socket`, a signal's interruption (EINTR) waited out: `recv_from`
/// returns it where `read_exact` would not.
pub fn recv_from(socket: &std::net::UdpSocket, buf: &mut [u8]) -> io::Result<(usize, std::net::SocketAddr)> {
    loop {
        match socket.recv_from(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            got => return got,
        }
    }
}

/// `command` spawned, though its program was written a moment ago: a child another
/// thread forked meanwhile holds the file open to write until it execs, and exec says
/// ETXTBSY until then (Linux; the fork window of every multithreaded process).
pub fn spawn(command: &mut std::process::Command) -> io::Result<std::process::Child> {
    let mut tries = 0;
    loop {
        match command.spawn() {
            Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy && tries < 1000 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            got => return got,
        }
    }
}

/// The program `text` at `path`, written by a process of its own: a file this one held open
/// to write, a child another test's thread forked meanwhile would hold until it execs, and
/// an exec of it, by this process or any other, would fail with ETXTBSY until then (Linux).
#[cfg(unix)]
pub fn program(path: &Path, text: &str) {
    let mut writer = Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"cat > "$0" && chmod 755 "$0""#)
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    writer.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    assert!(writer.wait().unwrap().success(), "{} not written", path.display());
}

/// The program at `from`, copied to `to` as [`program`] writes one: by a process of its own
/// on Unix, so that no child another test's thread forks holds it open to write.
pub fn program_copy(from: &Path, to: &Path) {
    #[cfg(unix)]
    assert!(
        Command::new("cp").arg(from).arg(to).status().unwrap().success(),
        "{} not copied",
        from.display()
    );
    #[cfg(windows)]
    std::fs::copy(from, to).unwrap();
}

/// A home whose guest is the test kernel and init, recorded before its daemon starts.
pub fn home_with(name: &str, timeout: Duration) -> TempDir {
    let home = TempDir::new(name);
    let args = [
        "use".as_ref(),
        "--kernel".as_ref(),
        kernel().as_os_str(),
        "--init".as_ref(),
        guest_init().as_os_str(),
    ];
    let recorded = run_shards_env(&["guest"], &args, &[("SHARDS_HOME", home.as_os_str())], timeout);
    assert_eq!(recorded.status, Some(0), "{}", recorded.stderr);
    home
}

/// The pids of the processes whose command line mentions `needle`.
pub fn processes_with(needle: &str) -> Vec<u32> {
    commands_with(needle).into_iter().map(|(pid, _)| pid).collect()
}

/// The processes whose command line mentions `needle`, and the command lines.
pub fn commands_with(needle: &str) -> Vec<(u32, String)> {
    commands()
        .into_iter()
        .filter(|(_, args)| args.contains(needle))
        .collect()
}

/// Every process and its command line: from /proc on Linux, whose `ps` may be busybox's,
/// which takes no `-o args`; from `ps` elsewhere.
#[cfg(not(windows))]
fn commands() -> Vec<(u32, String)> {
    if cfg!(target_os = "linux") {
        return std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|e| {
                let e = e.ok()?;
                let pid: u32 = e.file_name().to_str()?.parse().ok()?;
                let cmdline = std::fs::read(e.path().join("cmdline")).ok()?;
                Some((pid, String::from_utf8_lossy(&cmdline).replace('\0', " ")))
            })
            .collect();
    }
    let out = Command::new("ps").args(["-axo", "pid=,args="]).output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (pid, args) = l.trim_start().split_once(' ')?;
            Some((pid.parse().ok()?, args.to_string()))
        })
        .collect()
}

/// Every process and its command line, on Windows from the system's own table of them
/// ([`command_lines`]).
#[cfg(windows)]
fn commands() -> Vec<(u32, String)> {
    command_lines()
        .into_iter()
        .map(|process| (process.pid, process.command))
        .collect()
}

pub fn workspace() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    // Windows' canonical form is verbatim (`\\?\D:\...`), which every path under it then
    // is, and the POSIX shell that runs the guests' C compiler reads its backslashes as
    // escapes: the plain form names the same directory.
    match path.to_str().and_then(|p| p.strip_prefix(r"\\?\")) {
        Some(plain) if cfg!(windows) => PathBuf::from(plain),
        _ => path,
    }
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

/// Snapshots restore where this build has a backend that takes them (vm::SNAPSHOTS) and
/// the host's hypervisor sets a vCPU's state back (vm::restores_here: not Windows Server
/// 2022's, PM M345); tests of them skip elsewhere with a SKIP line, even where
/// `SHARDS_REQUIRE_VMS=1` says VMs must run: such a host runs VMs all the same.
pub fn cannot_snapshot() -> bool {
    if !shards_vmm::vm::SNAPSHOTS {
        let _ = writeln!(
            std::io::stderr(),
            "SKIP: snapshots are not supported on {ARCH} yet"
        );
        return true;
    }
    if !shards_vmm::vm::restores_here() {
        let _ = writeln!(
            std::io::stderr(),
            "SKIP: this host's hypervisor sets no vCPU's state back, so no snapshot restores here (Windows Server 2022's WHP sets no APIC page back, PM M345)"
        );
        return true;
    }
    false
}

/// Whether the daemon here keeps the snapshots it keeps, its templates, warm pools and saves
/// (D25, D26, D176): wherever snapshots restore ([`cannot_snapshot`]).
pub fn pools() -> bool {
    shards_vmm::vm::restores_here()
}

/// Whether tests of the snapshots a daemon keeps skip here ([`pools`]), saying why.
pub fn cannot_pool() -> bool {
    cannot_snapshot()
}

/// Whether a test that must reach Docker Hub itself skips here, saying why: unless
/// `SHARDS_TESTS_DOCKER_HUB=1` says it runs, as CI's test step does. Docker Hub counts
/// anonymous pulls by address, and every machine behind one shares them: 100 an hour for
/// this LAN's whole IPv6 /64, which tests that pulled each run spent (CI cluster 13). What
/// such a test reads is Docker Hub's own (`what`: attestation chains and signatures, tags as
/// they move, other CPUs' images and the emulator image), which the tests' own registry
/// ([`hub`]) does not serve.
pub fn cannot_reach_docker_hub(what: &str) -> bool {
    if std::env::var_os("SHARDS_TESTS_DOCKER_HUB").is_some_and(|v| v == "1") {
        return false;
    }
    let _ = writeln!(
        std::io::stderr(),
        "SKIP: {what} is Docker Hub's, whose anonymous pulls every machine behind this address shares: SHARDS_TESTS_DOCKER_HUB=1 runs it"
    );
    true
}

/// The kernel shards pins for its guests (src/kernel.rs), which the tests boot too.
#[path = "../../src/kernel.rs"]
pub mod pinned;

#[cfg(unix)]
pub mod terminal;
#[cfg(windows)]
#[path = "terminal_windows.rs"]
pub mod terminal;

/// The file `name` of the snapshot generation `dir` points at, as the store reads both:
/// its pointer, and the file's newest version, which on Windows a file written after its
/// generation was committed (its working set) lands as (platform `write_in`); the name
/// itself where none has.
pub fn snapshot_file(dir: &Path, name: &str) -> PathBuf {
    let generation = shards_vmm::snapshot::generation(dir).unwrap();
    let newest = shards_vmm::platform::open_dir(&generation)
        .and_then(|d| shards_vmm::platform::newest_in(&d, name))
        .unwrap_or_else(|_| name.to_string());
    generation.join(newest)
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

/// `CONFIG_NR_CPUS` of this architecture's pinned kernel overlay. Embed the config so
/// copied E2E executables need no source tree; do not copy a count that a new pin changes.
/// A guest brings up at most this many vCPUs and refuses the rest.
pub fn kernel_nr_cpus() -> u32 {
    let config = match ARCH {
        "x86_64" => include_str!("../../../../resources/kernel/shards-x86_64.config"),
        "aarch64" => include_str!("../../../../resources/kernel/shards-aarch64.config"),
        other => panic!("no pinned kernel config for {other}"),
    };
    let mut values = config
        .lines()
        .filter_map(|line| line.strip_prefix("CONFIG_NR_CPUS="));
    let cpus = values
        .next()
        .expect("the pinned kernel overlay sets CONFIG_NR_CPUS");
    assert!(
        values.next().is_none(),
        "the pinned kernel overlay sets CONFIG_NR_CPUS once"
    );
    cpus.parse().expect("CONFIG_NR_CPUS is a u32")
}

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
    // `--retry` alone retries timeouts and some HTTP statuses (curl(1)). Eight times, its
    // waits doubling from a second, some four minutes in all: GitHub's release downloads
    // answered 500 past three tries (CI run 38252923745).
    let ok = Command::new("curl")
        .args(["-fsSL", "--retry", "8", "--retry-all-errors", "-o"])
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

/// docker compose's own client for Linux on this architecture, v5.6.0 of docker/compose's
/// releases, pinned by content (the release's own SHA-256 of each asset): what a
/// microVM's command runs against its own engine (D181).
pub fn compose_linux() -> Option<PathBuf> {
    let artifact = match ARCH {
        "aarch64" => Artifact {
            name: "docker-compose-5.6.0-linux-aarch64",
            url: "https://github.com/docker/compose/releases/download/v5.6.0/docker-compose-linux-aarch64",
            sha256: "733ec76717ceb59052a9609b9dadfb523b2df8eab57a54212872d10a58078ea2",
        },
        "x86_64" => Artifact {
            name: "docker-compose-5.6.0-linux-x86_64",
            url: "https://github.com/docker/compose/releases/download/v5.6.0/docker-compose-linux-x86_64",
            sha256: "40343e21ca777173e69cff5dbafeb37c6f81f3b0d57d9e597f036e95eb63e76a",
        },
        _ => return None,
    };
    Some(fetch(&artifact))
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

/// The pinned kernel's `gpu` variant (D144), for a guest given this Mac's GPU: arm64's
/// alone (src/kernel.rs).
pub fn gpu_kernel() -> Option<&'static Path> {
    static K: OnceLock<Option<PathBuf>> = OnceLock::new();
    K.get_or_init(|| {
        pinned::KERNEL_GPU.map(|k| {
            fetch(&Artifact {
                name: k.name,
                url: k.url,
                sha256: k.sha256,
            })
        })
    })
    .as_deref()
}

/// Whether the `shards` under test carries the GPU's VM process (D144): built where the
/// renderer's libraries were given (SHARDS_GPU_LIBS), an empty file elsewhere (build.rs).
pub fn carries_gpu_vm() -> bool {
    std::fs::metadata(carried().join("shards-vm-gpu")).is_ok_and(|m| m.len() > 0)
}

/// The kernel of a guest that is itself a KVM host (`--nested`, D132): the `kvm` variant
/// ([`kvm_kernel`]), arm64's alone, as an x86_64 host gives no guest virtualization of its
/// own yet.
pub fn nested_kernel() -> Option<&'static Path> {
    (ARCH == "aarch64").then(kvm_kernel).flatten()
}

/// The pinned kernel's `kvm` variant (resources/kernel/kvm.config): KVM, VFIO and
/// virtio-iommu, for a guest that takes a PCI function it was given through VFIO (D154) or
/// runs VMs of its own, of the pinned kernels' own release, or the one
/// SHARDS_TEST_NESTED_KERNEL names, as SHARDS_TEST_KERNEL names another kernel for every
/// other guest. None on Windows, whose guests boot the `hyperv` variant.
pub fn kvm_kernel() -> Option<&'static Path> {
    static K: OnceLock<Option<PathBuf>> = OnceLock::new();
    K.get_or_init(|| {
        if let Some(path) = std::env::var_os("SHARDS_TEST_NESTED_KERNEL") {
            return Some(PathBuf::from(path));
        }
        let pinned = match ARCH {
            _ if cfg!(windows) => return None,
            "aarch64" => Artifact {
                name: "Image-6.18.48-aarch64-kvm-54fba99afaee",
                url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-54fba99afaee/Image-6.18.48-aarch64-kvm",
                sha256: "ed1e6dabf2f4088b37e3405a40b12962eb5eff76afe4303730e9d76ee1a7a3ba",
            },
            "x86_64" => Artifact {
                name: "vmlinux-6.18.48-x86_64-kvm-54fba99afaee",
                url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-54fba99afaee/vmlinux-6.18.48-x86_64-kvm",
                sha256: "71b5de5f7c9b4abfc543e230ad09bc1736e2ca15f7e76cbd28efbc546e6fa89e",
            },
            _ => return None,
        };
        Some(fetch(&pinned))
    })
    .as_deref()
}

/// Builds guest package `name` (static musl, `guest` profile) and returns its binary.
fn guest_binary(name: &str) -> PathBuf {
    guest_binary_in(name, name, "target/guest", &[])
}

/// shards itself for this architecture's Linux (static musl), for a guest that is itself
/// a KVM host to run as a Linux host runs it (D154): of the tests' profile, stripped and
/// without debug information, which a guest holds in its memory.
pub fn guest_shards() -> &'static Path {
    static S: OnceLock<PathBuf> = OnceLock::new();
    S.get_or_init(|| {
        guest_binary_with(
            "shards",
            "shards",
            "target/guest-shards",
            "test-release",
            &[
                ("CARGO_PROFILE_TEST_RELEASE_DEBUG", "false"),
                ("CARGO_PROFILE_TEST_RELEASE_STRIP", "true"),
            ],
        )
    })
}

/// shards' VM process for this architecture's Linux (static musl), for a guest that is
/// itself a KVM host to run VMs of its own with (D132).
pub fn guest_vm_process() -> &'static Path {
    static V: OnceLock<PathBuf> = OnceLock::new();
    V.get_or_init(|| guest_binary_in("shards-vm-process", "shards-vm", "target/guest", &[]))
}

/// shards-init built as if for another shards: its contract's identity is not this build's
/// (shards_abi::IDENTITY, crates/abi/build.rs).
pub fn foreign_init() -> &'static Path {
    static F: OnceLock<PathBuf> = OnceLock::new();
    F.get_or_init(|| {
        guest_binary_in(
            "shards-init",
            "shards-init",
            "target/guest-foreign",
            &[("SHARDS_ABI_IDENTITY", "1")],
        )
    })
}

/// The signal whose frame [`lossy_init`] takes for a malformed one: SIGRTMAX, which no test
/// sends for its own sake.
pub const LOSES_SIGNALS_ON: &str = "64";

/// shards-init built to take a signal frame of [`LOSES_SIGNALS_ON`] for a malformed one,
/// losing its signal connection as a malformed frame loses it, unsaid: a loss of the
/// workload's control that tests provoke (D16).
pub fn lossy_init() -> &'static Path {
    static L: OnceLock<PathBuf> = OnceLock::new();
    L.get_or_init(|| {
        guest_binary_in(
            "shards-init",
            "shards-init",
            "target/guest-lossy",
            &[("SHARDS_TEST_LOSE_SIGNALS_ON", LOSES_SIGNALS_ON)],
        )
    })
}

/// Builds guest package `package`'s binary `name` into `target_dir` with `env` added.
fn guest_binary_in(package: &str, name: &str, target_dir: &str, env: &[(&str, &str)]) -> PathBuf {
    guest_binary_with(package, name, target_dir, "guest", env)
}

/// [`guest_binary_in`], of cargo's profile `profile`. Where `SHARDS_TEST_GUESTS` names a
/// directory, the binary built beforehand into it, as `target_dir`'s last part and `name`
/// (`guest/shards-testguest`): for test binaries run where no cargo builds them, a guest
/// of the host's that is itself a KVM host (boot.rs,
/// `snapshot_tests_pass_on_a_kvm_host_of_this_architecture`).
fn guest_binary_with(
    package: &str,
    name: &str,
    target_dir: &str,
    profile: &str,
    env: &[(&str, &str)],
) -> PathBuf {
    if let Some(built) = std::env::var_os("SHARDS_TEST_GUESTS") {
        let variant = Path::new(target_dir).file_name().unwrap_or_default();
        return Path::new(&built).join(variant).join(name);
    }
    let target_dir = workspace().join(target_dir);
    let guest_target = format!("{ARCH}-unknown-linux-musl");
    // One build at a time, and this process's own copy of what it made: cargo puts its
    // output in place again on every build, up to date or not, and another test process
    // reading it meanwhile finds it missing.
    std::fs::create_dir_all(&target_dir).unwrap();
    let lock = std::fs::File::create(target_dir.join(".tests.lock")).unwrap();
    lock.lock().unwrap();
    let st = guest_cargo(&guest_target)
        .envs(env.iter().copied())
        .args([
            "build",
            "-p",
            package,
            "--profile",
            profile,
            "--target",
            &guest_target,
        ])
        .arg("--target-dir")
        .arg(&target_dir)
        .status()
        .unwrap();
    assert!(st.success(), "building {name}");
    let built = target_dir.join(&guest_target).join(profile).join(name);
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

/// cargo for this architecture's Linux (static musl), `guest_target`, from the workspace:
/// through the rustup proxy on PATH (not $CARGO, the bare cargo binary), without the dyld
/// paths cargo injects into test processes, as the proxy's environment is what lets
/// rust-lld find the toolchain's libLLVM; linked as build.rs links the init shardsd
/// carries, whatever linker the environment names for the host's own musl builds; its C
/// (AWS-LC) by zig, as build.rs compiles init's (scripts/zig-cc).
fn guest_cargo(guest_target: &str) -> Command {
    let linker = format!(
        "CARGO_TARGET_{}_LINKER",
        guest_target.to_ascii_uppercase().replace('-', "_")
    );
    let var = guest_target.replace('-', "_");
    let mut cc: Vec<(String, String)> = Vec::new();
    if std::env::var_os(format!("CC_{var}")).is_none() {
        let tool = |name: &str| {
            let script = workspace().join("scripts").join(name);
            if cfg!(windows) {
                // As a POSIX shell on Windows (Git's, MSYS) takes a path: forward slashes,
                // and no verbatim prefix (`\\?\`), whose backslashes it reads as escapes.
                let text = script.display().to_string();
                format!(
                    "sh {}",
                    text.strip_prefix(r"\\?\").unwrap_or(&text).replace('\\', "/")
                )
            } else {
                script.display().to_string()
            }
        };
        cc.push((format!("CC_{var}"), tool("zig-cc")));
        cc.push((format!("AR_{var}"), tool("zig-ar")));
        cc.push(("ZIG_TARGET".to_string(), format!("{ARCH}-linux-musl")));
    }
    let mut cargo = Command::new("cargo");
    cargo
        .env_remove("DYLD_FALLBACK_LIBRARY_PATH")
        .env_remove("DYLD_LIBRARY_PATH")
        .env(linker, "rust-lld")
        .envs(cc)
        .current_dir(workspace());
    cargo
}

/// shards' E2E test binaries of `tests` (their targets' names), built for this
/// architecture's Linux (static musl) with the tests' profile, without debug information,
/// for a guest that is itself a KVM host to run as such a host runs them (boot.rs,
/// `snapshot_tests_pass_on_a_kvm_host_of_this_architecture`).
pub struct LinuxTests {
    /// Each test's binary, by its target's name.
    pub binaries: Vec<(String, PathBuf)>,
    /// What the binaries reach by the paths their build gave them: `shards`
    /// (CARGO_BIN_EXE_shards), the init build.rs made (OUT_DIR) and the helpers it carries
    /// (SHARDS_HELPERS_CARRIED).
    pub files: Vec<PathBuf>,
}

pub fn linux_tests(tests: &[&str]) -> LinuxTests {
    let target_dir = workspace().join("target/l1");
    let guest_target = format!("{ARCH}-unknown-linux-musl");
    std::fs::create_dir_all(&target_dir).unwrap();
    let lock = std::fs::File::create(target_dir.join(".tests.lock")).unwrap();
    lock.lock().unwrap();
    let mut cargo = guest_cargo(&guest_target);
    cargo
        .env("CARGO_PROFILE_TEST_RELEASE_DEBUG", "false")
        .args(["test", "-p", "shards", "--profile", "test-release", "--no-run"])
        .args([
            "--message-format=json-render-diagnostics",
            "--target",
            &guest_target,
        ])
        .arg("--target-dir")
        .arg(&target_dir);
    for test in tests {
        cargo.args(["--test", test]);
    }
    let out = cargo.stdin(Stdio::null()).output().unwrap();
    assert!(
        out.status.success(),
        "building the tests for Linux: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    drop(lock);
    let (mut binaries, mut shards) = (Vec::new(), None);
    // Each package's build script's output directory, and the helpers it carries, if any.
    let mut scripts: Vec<(String, PathBuf, Option<PathBuf>)> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(m) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let text = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();
        let package = text(&m["package_id"]);
        match m["reason"].as_str() {
            Some("compiler-artifact") => {
                let (Some(kinds), Some(executable)) =
                    (m["target"]["kind"].as_array(), m["executable"].as_str())
                else {
                    continue;
                };
                let name = text(&m["target"]["name"]);
                if kinds.iter().any(|k| k == "test") {
                    binaries.push((name, PathBuf::from(executable)));
                } else if kinds.iter().any(|k| k == "bin") && name == "shards" {
                    shards = Some((package, PathBuf::from(executable)));
                }
            }
            Some("build-script-executed") => {
                let carried = m["env"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|kv| kv[0] == "SHARDS_HELPERS_CARRIED")
                    .and_then(|kv| kv[1].as_str())
                    .map(PathBuf::from);
                scripts.push((package, PathBuf::from(text(&m["out_dir"])), carried));
            }
            _ => {}
        }
    }
    let (package, shards) = shards.expect("cargo built no shards for Linux");
    let (_, out_dir, carried) = scripts
        .into_iter()
        .find(|(p, ..)| *p == package)
        .expect("no build script ran for shards");
    let carried = carried.expect("shards' build carries no helpers");
    LinuxTests {
        binaries,
        files: vec![
            shards,
            out_dir.join("shards-init"),
            carried.join("shards-vm"),
            carried.join("shards-net"),
        ],
    }
}

/// The production guest init (PID 1): the one `shardsd` carries, as build.rs made it.
pub fn guest_init() -> &'static Path {
    static INIT: OnceLock<PathBuf> = OnceLock::new();
    // Resolve the build's path on this host: a Unix cross-build's rooted path needs a
    // drive on Windows before the daemon can use it as an absolute init path.
    INIT.get_or_init(|| {
        Path::new(concat!(env!("OUT_DIR"), "/shards-init"))
            .canonicalize()
            .unwrap()
    })
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

/// A `shards` command as tests run it ([`as_tests_run`]).
pub fn command() -> Command {
    let mut cmd = Command::new(shards());
    as_tests_run(&mut cmd);
    cmd
}

/// `cmd`, which runs `shards` itself or a program that runs it, as every test runs it:
/// publishing the microVMs it makes to no local image store (pull.rs `publish`), so that no
/// test writes to the Docker engine of the host it runs on (a test of publishing names an
/// engine of its own), and given the tests' own docker config ([`docker_config`]), so that
/// none reads this host's or runs its credential helpers, and their own daemon.json and
/// certs.d in place of the host's Docker's (`SHARDS_DAEMON_JSON`, D194; `SHARDS_CERTS_D`,
/// D23), so that no mirror, insecure registry or certificate of the host's reaches one. No default platform of the host's user's reaches it.
pub fn as_tests_run(cmd: &mut Command) -> &mut Command {
    cmd.env("SHARDS_LOCAL_STORE", "none")
        .env("DOCKER_CONFIG", docker_config())
        .env("SHARDS_DAEMON_JSON", docker_config().join("daemon.json"))
        .env("SHARDS_CERTS_D", docker_config().join("certs.d"))
        .env_remove("DOCKER_AUTH_CONFIG")
        // A platform this host's user prefers is no test's: a test names its own.
        .env_remove("SHARDS_DEFAULT_PLATFORM")
        .env_remove("DOCKER_DEFAULT_PLATFORM")
}

/// The tests' own docker config, which every command is given unless a test gives its
/// own: no test, nor any daemon one starts, reads this host's docker config or its
/// credential helpers. Its `auths` name a host no test reaches, so that no platform helper
/// is looked for either (D184: a config that holds credentials keeps them itself). Beside it,
/// the tests' own daemon.json, which names no registry, and certs.d, which holds nothing
/// ([`as_tests_run`]).
pub fn docker_config() -> &'static Path {
    static CONFIG: OnceLock<TempDir> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let dir = TempDir::new("docker-config");
        std::fs::write(
            dir.join("config.json"),
            r#"{"auths":{"shards-tests.invalid":{}}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("daemon.json"), "{}").unwrap();
        std::fs::create_dir(dir.join("certs.d")).unwrap();
        dir
    })
}

/// KVM's counters of a VM as `name value` lines (each file of its debugfs directory,
/// /sys/kernel/debug/kvm/PID-FD/, which sums its vCPUs'), summed per name over every
/// directory listed; a file of several values (a histogram) is left out, and a counter not
/// listed is absent, not zero.
pub fn kvm_counters_in(text: &str) -> std::collections::BTreeMap<String, u64> {
    let mut sums = std::collections::BTreeMap::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if let (Some(name), Some(value), None) = (words.next(), words.next(), words.next())
            && let Ok(value) = value.parse::<u64>()
        {
            *sums.entry(name.to_string()).or_insert(0) += value;
        }
    }
    sums
}

/// KVM's counters of VM process `pid` ([`kvm_counters_in`]), from debugfs, which only root
/// reads: directly where this process may, as root on a guest that is a KVM host of its own
/// (boot.rs, `snapshot_tests_pass_on_a_kvm_host_of_this_architecture`), else through
/// `sudo -n`, as CI's runners allow; none where they cannot be read.
pub fn kvm_counters(pid: u32) -> Option<std::collections::BTreeMap<String, u64>> {
    let mine = format!("{pid}-");
    let direct: String = std::fs::read_dir("/sys/kernel/debug/kvm")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|vm| vm.file_name().to_string_lossy().starts_with(&mine))
        .flat_map(|vm| std::fs::read_dir(vm.path()).into_iter().flatten().flatten())
        .filter_map(|f| {
            let value = std::fs::read_to_string(f.path()).ok()?;
            Some(format!(
                "{} {}\n",
                f.file_name().to_string_lossy(),
                value.replace('\n', " ")
            ))
        })
        .collect();
    let counters = kvm_counters_in(&direct);
    if !counters.is_empty() {
        return Some(counters);
    }
    let script = format!(
        "for f in /sys/kernel/debug/kvm/{pid}-*/*; do [ -f \"$f\" ] && printf '%s %s\\n' \"${{f##*/}}\" \"$(tr '\\n' ' ' < \"$f\")\"; done; true"
    );
    let out = Command::new("sudo")
        .args(["-n", "sh", "-c", &script])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let counters = kvm_counters_in(&String::from_utf8_lossy(&out.stdout));
    (out.status.success() && !counters.is_empty()).then_some(counters)
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

#[cfg(unix)]
pub fn alive(pid: i32) -> bool {
    // SAFETY: kill(2) with signal 0 only asks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// The IDs of the VM processes `parent` started: its children named shards-vm. On Windows
/// `shards run --kernel` and `shards restore` start the VM process as a child, confined,
/// and answer what it asks for, where Unix's become it.
#[cfg(windows)]
pub fn vm_processes(parent: u32) -> Vec<u32> {
    processes()
        .into_iter()
        .filter(|(_, ppid, name)| *ppid == parent && name.eq_ignore_ascii_case("shards-vm.exe"))
        .map(|(pid, _, _)| pid)
        .collect()
}

/// Every process of the system's, as its snapshot of them says (Toolhelp32): its ID, its
/// parent's ([`live_parents`]), and its executable's name.
#[cfg(windows)]
pub fn processes() -> Vec<(u32, u32, String)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    // SAFETY: a snapshot of every process, closed below.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    assert_ne!(snap, INVALID_HANDLE_VALUE);
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut found = Vec::new();
    // SAFETY: the snapshot, and an entry of its own size.
    let mut more = unsafe { Process32FirstW(snap, &raw mut entry) } != 0;
    while more {
        let end = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        found.push((
            entry.th32ProcessID,
            entry.th32ParentProcessID,
            String::from_utf16_lossy(&entry.szExeFile[..end]),
        ));
        // SAFETY: as above.
        more = unsafe { Process32NextW(snap, &raw mut entry) } != 0;
    }
    // SAFETY: the snapshot taken above, closed once.
    unsafe { CloseHandle(snap) };
    live_parents(found, started)
}

/// `listed`, each process's parent kept only where that parent started no later than it
/// did (`started`), else 0. Windows keeps the ID of a parent that has ended in its orphans'
/// entries (th32ParentProcessID) and gives the ID to a process started later, whose
/// children the orphans then seem: a ping the daemon tests' sleeper left, under a direct
/// boot's processes on windows-2025 (PM M528). A parent that started after its child is
/// that later process, as psutil's `Process.children` reads it. Where either start is
/// unknown, the parent is kept as listed.
#[cfg(windows)]
pub fn live_parents(
    listed: Vec<(u32, u32, String)>,
    started: impl Fn(u32) -> Option<u64>,
) -> Vec<(u32, u32, String)> {
    let when: std::collections::HashMap<u32, u64> = listed
        .iter()
        .filter_map(|&(pid, ..)| Some((pid, started(pid)?)))
        .collect();
    listed
        .into_iter()
        .map(|(pid, parent, name)| {
            let later = matches!((when.get(&parent), when.get(&pid)), (Some(p), Some(c)) if p > c);
            (pid, if later { 0 } else { parent }, name)
        })
        .collect()
}

/// When process `pid` started, in 100 ns since 1601 (GetProcessTimes' creation time),
/// asked through a handle of the least right; none where it does not open.
#[cfg(windows)]
fn started(pid: u32) -> Option<u64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: a process's handle of one right, closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
    // SAFETY: the handle opened above, and four times for the call to fill.
    let asked = unsafe {
        GetProcessTimes(
            process,
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    };
    // SAFETY: the handle opened above, closed once.
    unsafe { CloseHandle(process) };
    (asked != 0).then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// A process as [`command_lines`] lists it.
#[cfg(windows)]
pub struct Listed {
    pub pid: u32,
    pub parent: u32,
    /// Its executable's name, as Toolhelp32's snapshot gives it.
    pub name: String,
    /// Its command line, empty where it could not be read.
    pub command: String,
}

/// Every process of the system's ([`processes`]), each with its command line, read
/// through the process's own handle ([`command_line`]), not WMI: CIM's Win32_Process
/// answers a standard user logged on over SSH, a network logon, "Access denied"
/// (0x80041003; root\cimv2 grants remote enable to administrators alone), and tests run
/// so (step 10's arm64 Windows host) found no warm or restored VM while the daemon's log
/// said they ran. A process whose command line cannot be read is kept, its command line
/// empty, and one of shards' is said on stderr, captured with a failing test's output.
#[cfg(windows)]
#[allow(clippy::print_stderr)]
pub fn command_lines() -> Vec<Listed> {
    processes()
        .into_iter()
        .map(|(pid, parent, name)| {
            let command = command_line(pid).unwrap_or_else(|e| {
                if name.to_ascii_lowercase().starts_with("shards") {
                    eprintln!("the command line of {name} ({pid}) unread: {e}");
                }
                String::new()
            });
            Listed {
                pid,
                parent,
                name,
                command,
            }
        })
        .collect()
}

/// Process `pid`'s command line, as the process was started with it: NtQueryInformationProcess's
/// ProcessCommandLineInformation (60; PROCESSINFOCLASS, as the WDK declares it and
/// windows-sys's Wdk_System_Threading carries it), asked through a handle of the least
/// right, PROCESS_QUERY_LIMITED_INFORMATION, which [`alive`] asks too. It answers as
/// ProcessImageFileName does, as Microsoft's page of the function has it: a UNICODE_STRING
/// followed by its text, and STATUS_INFO_LENGTH_MISMATCH with the size needed in
/// ReturnLength when the buffer is too small
/// (<https://learn.microsoft.com/en-us/windows/win32/api/winternl/nf-winternl-ntqueryinformationprocess>).
#[cfg(windows)]
pub fn command_line(pid: u32) -> Result<String, String> {
    use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation};
    use windows_sys::Win32::Foundation::{
        CloseHandle, STATUS_BUFFER_OVERFLOW, STATUS_BUFFER_TOO_SMALL, STATUS_INFO_LENGTH_MISMATCH,
        UNICODE_STRING,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: a process's handle of one right, closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(format!("OpenProcess: {}", io::Error::last_os_error()));
    }
    // In u64s, which align the UNICODE_STRING at its start as its pointer needs.
    let mut buffer = vec![0u64; 256];
    let read = loop {
        let size = u32::try_from(buffer.len() * size_of::<u64>()).unwrap_or(u32::MAX);
        let mut needed = 0u32;
        // SAFETY: a buffer of `size` bytes, and the handle opened above.
        let status = unsafe {
            NtQueryInformationProcess(
                process,
                ProcessCommandLineInformation,
                buffer.as_mut_ptr().cast(),
                size,
                &raw mut needed,
            )
        };
        let short = [
            STATUS_INFO_LENGTH_MISMATCH,
            STATUS_BUFFER_TOO_SMALL,
            STATUS_BUFFER_OVERFLOW,
        ];
        if short.contains(&status) && needed > size {
            buffer.resize(usize::try_from(needed).unwrap().div_ceil(size_of::<u64>()), 0);
            continue;
        }
        if status < 0 {
            break Err(format!(
                "NtQueryInformationProcess: NTSTATUS {:#010x}",
                status.cast_unsigned()
            ));
        }
        // SAFETY: the call succeeded, so the buffer starts with the UNICODE_STRING it wrote.
        let text = unsafe { buffer.as_ptr().cast::<UNICODE_STRING>().read() };
        if text.Buffer.is_null() || text.Length == 0 {
            break Ok(String::new());
        }
        // SAFETY: the string's text, Length bytes in the buffer after it, which lives on.
        let units = unsafe { std::slice::from_raw_parts(text.Buffer, usize::from(text.Length) / 2) };
        break Ok(String::from_utf16_lossy(units));
    };
    // SAFETY: the handle opened above, closed once.
    unsafe { CloseHandle(process) };
    read
}

/// A handle to process `pid` that may give it handles (PROCESS_DUP_HANDLE), as a VM's
/// spawner holds one: what `shards_ipc::Stream::gives_to` takes.
#[cfg(windows)]
pub fn giver(pid: u32) -> std::os::windows::io::OwnedHandle {
    use std::os::windows::io::FromRawHandle as _;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_DUP_HANDLE};
    // SAFETY: a process handle of the one right asked, owned from here on.
    let raw = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, pid) };
    assert!(!raw.is_null(), "process {pid}: {}", io::Error::last_os_error());
    // SAFETY: the handle just opened, which nothing else owns.
    unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw) }
}

/// Whether process `pid` has yet to end: its exit code still STILL_ACTIVE
/// (GetExitCodeProcess), which a process that ended with that code would be taken for.
#[cfg(windows)]
pub fn alive(pid: i32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let Ok(pid) = u32::try_from(pid) else {
        return false;
    };
    // SAFETY: a process's handle opened, read and closed here.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let mut code = 0u32;
        let read = GetExitCodeProcess(process, &raw mut code) != 0;
        CloseHandle(process);
        read && code == STILL_ACTIVE.cast_unsigned()
    }
}

/// Ends process `pid` outright: SIGKILL on Unix; on Windows TerminateProcess, its status
/// 128 and 9 as a Unix child's.
pub fn kill_now(pid: i32) {
    #[cfg(unix)]
    // SAFETY: kill(2) of a process this test started.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
        let pid = u32::try_from(pid).unwrap();
        // SAFETY: a process's handle opened, used and closed here.
        unsafe {
            let process = OpenProcess(PROCESS_TERMINATE, 0, pid);
            assert!(!process.is_null(), "process {pid} could not be opened");
            TerminateProcess(process, 128 + 9);
            CloseHandle(process);
        }
    }
}

/// This host's physical memory, in bytes: sysconf(3)'s pages on Unix; on Windows
/// GlobalMemoryStatusEx's total.
pub fn host_memory() -> u64 {
    #[cfg(unix)]
    // SAFETY: sysconf(3) with constant arguments.
    unsafe {
        u64::try_from(libc::sysconf(libc::_SC_PHYS_PAGES)).unwrap()
            * u64::try_from(libc::sysconf(libc::_SC_PAGESIZE)).unwrap()
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut status = MEMORYSTATUSEX {
            dwLength: u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>()).unwrap(),
            ..MEMORYSTATUSEX::default()
        };
        // SAFETY: a structure on this stack, its length set as the call asks.
        assert_ne!(unsafe { GlobalMemoryStatusEx(&raw mut status) }, 0);
        status.ullTotalPhys
    }
}

/// This host's name, as the daemon reads it (daemon/info.rs `host_name`): gethostname(2)
/// on Unix; on Windows its DNS host name (GetComputerNameExW,
/// ComputerNamePhysicalDnsHostname), as Go's `os.Hostname` reads it.
pub fn host_name() -> String {
    #[cfg(unix)]
    {
        let mut name = [0u8; 256];
        // SAFETY: gethostname(2) into our own buffer, of its length.
        let got = unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) };
        assert_eq!(got, 0);
        String::from_utf8_lossy(name.split(|&b| b == 0).next().unwrap()).into_owned()
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{
            ComputerNamePhysicalDnsHostname, GetComputerNameExW,
        };
        let mut name = [0u16; 256];
        let mut len = u32::try_from(name.len()).unwrap();
        // SAFETY: at most `len` units written into `name`, and its length back.
        let got =
            unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, name.as_mut_ptr(), &raw mut len) };
        assert_ne!(got, 0, "{}", std::io::Error::last_os_error());
        String::from_utf16(&name[..usize::try_from(len).unwrap()]).unwrap()
    }
}

/// Writes `bytes` into `file` at `at`, in place, as a host's program writes one.
pub fn write_at(file: &std::fs::File, bytes: &[u8], at: u64) {
    #[cfg(unix)]
    std::os::unix::fs::FileExt::write_all_at(file, bytes, at).unwrap();
    #[cfg(windows)]
    assert_eq!(
        std::os::windows::fs::FileExt::seek_write(file, bytes, at).unwrap(),
        bytes.len()
    );
}

/// `path`'s DACL on Windows, as SDDL writes it, and what it is where the file is its user's
/// alone, as `shards_ipc::create_private` makes one: protected from its directory's, one
/// ACE, allowing this process's user all of a file (`FA`), and no one else anything, which
/// NTFS keeps of a file made with `D:P(A;;GA;;;USER)` (jupiter, Windows 11 26200). Each as
/// SDDL writes a descriptor back, its user's SID an alias where it has one: `LA`, a
/// machine's administrator (RID 500), as CI's runners run as.
#[cfg(windows)]
pub fn dacl_and_private(path: &Path) -> (String, String) {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetTokenInformation, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: a NUL-terminated path, and a descriptor for the call to fill, freed below.
    let got = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut sd,
        )
    };
    assert_eq!(got, 0, "{}", path.display());
    let dacl = dacl_text(sd);
    // SAFETY: what the call above allocated, freed once.
    unsafe { LocalFree(sd) };
    // This process's user, as SDDL names it.
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: this process's own token, for querying, never closed: the test's life.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) };
    assert_ne!(opened, 0);
    let mut buf = vec![0u8; 256];
    let mut needed = 0u32;
    // SAFETY: a buffer of its own size for the token's user.
    let read = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast(),
            u32::try_from(buf.len()).unwrap(),
            &raw mut needed,
        )
    };
    assert_ne!(read, 0, "{}", std::io::Error::last_os_error());
    // SAFETY: TokenUser's TOKEN_USER, at the start of the buffer it filled.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut named: *mut u16 = std::ptr::null_mut();
    // SAFETY: the SID the token holds, and a string for the call to fill, freed below.
    let converted = unsafe { ConvertSidToStringSidW(sid, &raw mut named) };
    assert_ne!(converted, 0);
    // SAFETY: a NUL-terminated string the call made.
    let user = unsafe {
        let n = (0..).take_while(|&i| *named.add(i) != 0).count();
        let user = String::from_utf16_lossy(std::slice::from_raw_parts(named, n));
        LocalFree(named.cast());
        user
    };
    let private: Vec<u16> = format!("D:P(A;;FA;;;{user})").encode_utf16().chain([0]).collect();
    let mut made: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: a NUL-terminated SDDL string, and a descriptor for the call to fill, freed
    // below.
    let parsed = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            private.as_ptr(),
            SDDL_REVISION_1,
            &raw mut made,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(parsed, 0, "{}", std::io::Error::last_os_error());
    let private = dacl_text(made);
    // SAFETY: what the call above allocated, freed once.
    unsafe { LocalFree(made) };
    (dacl, private)
}

/// The DACL of the descriptor `sd` as SDDL writes it.
#[cfg(windows)]
fn dacl_text(sd: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR) -> String {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    let mut text: *mut u16 = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: a descriptor its caller holds, and a string for the call to fill, freed below.
    let converted = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &raw mut text,
            &raw mut len,
        )
    };
    assert_ne!(converted, 0, "{}", std::io::Error::last_os_error());
    // SAFETY: the `len` characters the call wrote.
    let dacl = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len as usize) });
    // SAFETY: what the call allocated, freed once.
    unsafe { LocalFree(text.cast()) };
    dacl.trim_end_matches('\0').to_string()
}

/// `cmd` started so that [`interrupt`] reaches it: on Windows in a process group of its own,
/// which a console's events reach by its leader's id (CREATE_NEW_PROCESS_GROUP).
pub fn interruptible(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
    }
    cmd
}

/// Interrupts `child`, started [`interruptible`], as a terminal's interrupt does: SIGINT on
/// Unix; on Windows CTRL_BREAK_EVENT to its group, the one console event a process can send
/// one group (GenerateConsoleCtrlEvent), which the client takes as Go's runtime takes it,
/// for SIGINT.
pub fn interrupt(child: &std::process::Child) {
    #[cfg(unix)]
    // SAFETY: kill(2) of a child of this test's.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGINT);
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
        // SAFETY: an event sent to the group the child leads.
        let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) };
        assert_ne!(sent, 0, "CTRL_BREAK_EVENT: {}", std::io::Error::last_os_error());
    }
}

/// Ends `child`, started [`interruptible`], as a signal it takes no hold of ends a client:
/// SIGTERM on Unix; on Windows its interrupt, which the console's own handler takes where
/// the client took none, ending it with STATUS_CONTROL_C_EXIT ([`TERMINATED`]).
pub fn terminate(child: &std::process::Child) {
    #[cfg(unix)]
    // SAFETY: kill(2) of a child of this test's.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(windows)]
    interrupt(child);
}

/// The status of a client [`terminate`] ended: none on Unix, where a signal ended it; on
/// Windows STATUS_CONTROL_C_EXIT (0xC000013A), the console's handler's.
pub const TERMINATED: Option<i32> = if cfg!(windows) {
    Some(0xC000_013A_u32.cast_signed())
} else {
    None
};

/// The signal a client that proxies its signals passes on as [`terminate`] ends it, by
/// name and Linux's number: SIGTERM on Unix; on Windows, where its interrupt is the event,
/// SIGINT.
pub const PROXIED: (&str, u8) = if cfg!(windows) { ("INT", 2) } else { ("TERM", 15) };

/// Waits until no daemon serves `home`'s slots: a daemon killed holds its lists until the
/// kernel has closed all its descriptors, which may be after a client sees its own slot
/// end, as the kernel closes the dead process's descriptors one by one.
pub fn until_unserved(home: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while shards_ipc::slots::served(home) {
        assert!(
            std::time::Instant::now() < deadline,
            "{} is still served",
            home.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// The processes under the daemon whose log is `log` (its first line names its PID), each
/// as `ps` shows it, or on Windows as [`command_lines`] lists it (ID, parent's ID, name,
/// command line): whether a run's VM and network processes still live, for a failure to say.
pub fn processes_under(log: &str) -> String {
    let Some(daemon) = log.lines().find_map(|l| {
        l.strip_prefix("shards daemon ")?
            .split(':')
            .next()?
            .parse::<u32>()
            .ok()
    }) else {
        return "(no daemon named in its log)".into();
    };
    #[cfg(windows)]
    let listed: Vec<(u32, u32, String)> = command_lines()
        .into_iter()
        .map(|p| {
            let line = format!("{} {} {} {}", p.pid, p.parent, p.name, p.command);
            (p.pid, p.parent, line)
        })
        .collect();
    #[cfg(not(windows))]
    let listed: Vec<(u32, u32, String)> = Command::new("ps")
        .args(["-axo", "pid=,ppid=,stat=,etime=,command="])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((
                f.next()?.parse().ok()?,
                f.next()?.parse().ok()?,
                l.trim().to_string(),
            ))
        })
        .collect();
    let mut under = vec![daemon];
    loop {
        let more: Vec<u32> = listed
            .iter()
            .filter(|(pid, ppid, _)| under.contains(ppid) && !under.contains(pid))
            .map(|(pid, ..)| *pid)
            .collect();
        if more.is_empty() {
            break;
        }
        under.extend(more);
    }
    let shown: Vec<&str> = listed
        .iter()
        .filter(|(pid, ..)| under.contains(pid))
        .map(|(.., line)| line.as_str())
        .collect();
    if shown.is_empty() {
        format!("(daemon {daemon} is gone)")
    } else {
        shown.join("\n")
    }
}

/// Docker's default bridge as the daemon and the builder elect it on this host
/// (shards_net::bridge): its gateway and guest addresses are what a run sees.
pub fn bridge() -> shards_net::bridge::Bridge {
    shards_net::bridge::elected_here(&mut |note| panic!("{note}")).expect("a subnet for the default bridge")
}

/// A TCP port free now at every IPv4 address, below every system's ephemeral range
/// (Linux's from 32768, net.ipv4.ip_local_port_range; macOS's and Windows' from 49152, as
/// RFC 6335 §6 has it): one no other test's connection is given meanwhile, as a port a
/// daemon picked from that range may be the moment it is free. Tried from a place of the
/// process's own, so that test processes running beside each other seldom try the same.
pub fn fixed_port() -> u16 {
    const FIRST: u32 = 20_000;
    const SPAN: u32 = 12_000;
    // A port handed out is not handed out again in this process: its probe lets it go at
    // once, and tests running beside each other would otherwise be given the same one.
    static HANDED: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
    let mut handed = HANDED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let start = std::process::id() % SPAN;
    let port = (0..SPAN)
        .filter_map(|i| u16::try_from(FIRST + (start + i) % SPAN).ok())
        .filter(|port| !handed.contains(port))
        .find(|&port| port_free(port))
        .expect("a free port below the ephemeral ranges");
    handed.push(port);
    port
}

/// Whether TCP port `port` can be bound at every IPv4 address now, asked of a process of
/// its own that spawns nothing ([`port_probe`]). A socket a test process makes, however
/// briefly, stays open in each child another of its threads spawns meanwhile until that
/// child execs, so a probe made here would hold the very port it found free from the
/// program that binds it next: measured on macOS under load, a listener dropped beside
/// four spawning threads was still bound for up to 180 ms, with std's spawns and with
/// posix_spawn's CLOEXEC_DEFAULT alike (project-port-free-flake).
pub fn port_free(port: u16) -> bool {
    port_probe_process(port, false)
        .and_then(|mut probe| {
            let said = probe.said();
            let _ = probe.child.wait();
            said
        })
        .is_some_and(|said| said == "port free")
}

/// `port` held at every IPv4 address by a process of its own, as another program holds it,
/// until the holder is dropped: then its socket closes with it, held by no child of this
/// process meanwhile.
pub fn hold_port(port: u16) -> PortHolder {
    let mut probe = port_probe_process(port, true).expect("a process to hold the port");
    match probe.said() {
        Some(said) if said == "port held" => probe,
        said => panic!("{port}: not held: {said:?}"),
    }
}

/// A process of this test binary's running [`port_probe`] alone.
pub struct PortHolder {
    child: std::process::Child,
    lines: BufReader<std::process::ChildStdout>,
}

impl PortHolder {
    /// The probe's word: `port free`, `port held` or `port taken …`.
    fn said(&mut self) -> Option<String> {
        let mut line = String::new();
        while self.lines.read_line(&mut line).ok()? > 0 {
            // After libtest's `test common::port_probe ... `, on its line.
            if let Some((_, said)) = line.trim_end().split_once("PORT_PROBE ") {
                return Some(said.to_string());
            }
            line.clear();
        }
        None
    }
}

impl Drop for PortHolder {
    fn drop(&mut self) {
        // Its stdin closed, the holder lets go and ends.
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

fn port_probe_process(port: u16, hold: bool) -> Option<PortHolder> {
    let mut command = Command::new(std::env::current_exe().ok()?);
    command
        .args([
            "common::port_probe",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("SHARDS_PORT_PROBE", port.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if hold {
        command.env("SHARDS_PORT_HOLD", "1");
    }
    let mut child = command.spawn().ok()?;
    let lines = BufReader::new(child.stdout.take()?);
    Some(PortHolder { child, lines })
}

/// The process [`port_free`] and [`hold_port`] ask: it binds the port named by
/// `SHARDS_PORT_PROBE` and says whether it could, holding it, with `SHARDS_PORT_HOLD`, until
/// its stdin closes. Nothing else of the test binary runs in it.
#[test]
#[ignore = "a process of its own for port_free and hold_port, which run it"]
fn port_probe() {
    let Some(port) = std::env::var("SHARDS_PORT_PROBE")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
    else {
        return;
    };
    match std::net::TcpListener::bind(("0.0.0.0", port)) {
        Ok(held) if std::env::var_os("SHARDS_PORT_HOLD").is_some() => {
            println!("PORT_PROBE port held");
            let _ = io::stdin().read(&mut [0u8; 1]);
            drop(held);
        }
        Ok(_) => println!("PORT_PROBE port free"),
        Err(e) => println!("PORT_PROBE port taken {e}"),
    }
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
    /// The signal that ended it, where one did: a status of `None` says no more.
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

impl fmt::Display for Run {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "status {:?}{} after {:?}\n--- stdout\n{}\n--- stderr\n{}",
            self.status,
            self.signal
                .map(|s| format!(", ended by signal {s}"))
                .unwrap_or_default(),
            self.elapsed,
            self.stdout,
            self.stderr
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

    /// The VMM process's own memory as it reported, in KiB, as memory per VM is counted.
    pub fn footprint_kib(&self) -> Option<u128> {
        self.timing_field("footprint_kib")
    }

    /// The VMM process's peak commit charge as it reported, in KiB: Windows' alone, which
    /// charges memory as it is committed, touched or not (PM M413); 0 elsewhere.
    pub fn peak_commit_kib(&self) -> Option<u128> {
        self.timing_field("peak_commit_kib")
    }

    /// What the VM's guest reported free, given back or not (D151), in KiB.
    pub fn reported_kib(&self) -> Option<u128> {
        self.timing_field("reported_kib")
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

/// `shards <args>` (`run` or `start -a`) of microVM `container`, whose command waits for
/// SIGUSR1 (the test guest's `await`): the command sees none of its microVM's agents, its
/// PID namespace being its own (D115), so the host watches them in `shards top container
/// -o fields`, each process a row of its columns, until `ready` says of the rows that it
/// is, and then sends the command the signal. What the run said, as [`run_shards_env`]
/// says it.
///
/// The signal goes once the command catches it, as `top` shows (`caught`): without
/// `--init` the command is its PID namespace's first process, of which the kernel drops a
/// signal it neither handles nor blocks (kernel/signal.c, `sig_task_ignored`), and with it
/// the reaper passes it on to a command that it would end. (`blocked` cannot say: the
/// kernel takes a signal waited for out of the blocked set while it waits, into
/// `real_blocked`.) `ready`, once it says so, is asked no more: what it kept of the rows
/// is of that moment.
pub fn run_watching(
    env: &[(&str, &std::ffi::OsStr)],
    args: &[&str],
    container: &str,
    fields: &str,
    ready: &mut dyn FnMut(&[Vec<String>]) -> bool,
    timeout: Duration,
) -> Run {
    // `shards top`'s table needs a `pid` column to key its rows (procps ps), so every
    // field list leads with it; then the caught signals and the name, which say whether
    // the command awaits; a row is those, then the fields asked.
    let with_pid = format!("pid,caught,comm,{fields}");
    let start = Instant::now();
    std::thread::scope(|scope| {
        let ran = scope.spawn(|| run_shards_env(&[], args, env, timeout));
        let mut last = String::new();
        let mut signalled = false;
        let mut was_ready = false;
        while !ran.is_finished() && start.elapsed() < timeout {
            let top = run_shards_env(
                &["top"],
                &[container, "-o", &with_pid],
                env,
                Duration::from_secs(30),
            );
            let all: Vec<Vec<String>> = top
                .stdout
                .lines()
                .skip(1)
                .map(|l| l.split_whitespace().skip(1).map(str::to_string).collect())
                .collect();
            // The test guest awaiting: SIGUSR1 (10) among the signals it catches, the mask
            // in hex (`<` before its low 32 bits where higher ones are set too).
            let awaiting = all.iter().any(|r| {
                r.get(1).is_some_and(|c| c == "testguest")
                    && r.first()
                        .and_then(|m| u64::from_str_radix(m.trim_start_matches('<'), 16).ok())
                        .is_some_and(|m| (m & (1 << (10 - 1))) != 0)
            });
            // Each row its fields, in the order asked.
            let rows: Vec<Vec<String>> = all.into_iter().map(|r| r.into_iter().skip(2).collect()).collect();
            was_ready = was_ready || (top.status == Some(0) && ready(&rows));
            if was_ready && awaiting {
                let sent = run_shards_env(
                    &["kill"],
                    &["-s", "USR1", container],
                    env,
                    Duration::from_secs(30),
                );
                assert_eq!(sent.status, Some(0), "{sent}");
                signalled = true;
                break;
            }
            // What the host last saw, for a handshake that never completes (libtest shows
            // a test's output only when it fails).
            last = format!(
                "top status={:?} ready={was_ready} awaiting={awaiting} rows={rows:?} stderr={:?}",
                top.status,
                top.stderr.trim()
            );
            // Each look is a whole `shards top`: the client, the daemon, and an exec in the
            // microVM that reads its /proc. Agents take hundreds of milliseconds to come up,
            // so a look every 250 ms costs little and loads no parallel test's daemon or VM.
            std::thread::sleep(Duration::from_millis(250));
        }
        if !signalled {
            // Straight to stderr: a helper is not a `#[test]`, where clippy allows the
            // print macros, so it writes as `show_why` does.
            let _ = writeln!(io::stderr(), "run_watching {container} never ready: {last}");
        }
        ran.join().expect("the run's thread")
    })
}

/// [`run_watching`] until `ready`'s count of processes named `ready`'s name run: the test
/// guest's `await NAME COUNT [THEN]`.
pub fn run_awaiting(
    env: &[(&str, &std::ffi::OsStr)],
    args: &[&str],
    container: &str,
    ready: (&str, usize),
    timeout: Duration,
) -> Run {
    let (name, count) = ready;
    let mut seen = |rows: &[Vec<String>]| {
        rows.iter()
            .filter(|r| r.first().is_some_and(|c| c == name))
            .count()
            >= count
    };
    run_watching(env, args, container, "comm", &mut seen, timeout)
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

/// `cmd`'s output, as [`Command::output`] gives it, within `timeout`: a client of shards'
/// (docker, Compose) that hangs fails its test with what it said, rather than hold the
/// suite. Past `timeout` it is sent SIGQUIT first, on which a Go program writes where each
/// of its goroutines waits and exits, then killed.
pub fn output_within(cmd: &mut Command, timeout: Duration) -> std::process::Output {
    let start = Instant::now();
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning a client");
    // Each read on a thread of its own, handed over as it comes: a process the client
    // started may hold its pipes past it, and what came before is kept.
    let collect = |mut r: Box<dyn Read + Send>| {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut b = vec![0u8; 64 * 1024];
            while let Ok(n @ 1..) = r.read(&mut b) {
                if tx.send(b[..n].to_vec()).is_err() {
                    return;
                }
            }
        });
        rx
    };
    let out = collect(Box::new(child.stdout.take().unwrap()));
    let err = collect(Box::new(child.stderr.take().unwrap()));
    let took = |rx: &std::sync::mpsc::Receiver<Vec<u8>>| {
        let until = Instant::now() + Duration::from_secs(10);
        let mut got = Vec::new();
        while let Ok(chunk) = rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            got.extend_from_slice(&chunk);
        }
        got
    };
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > timeout {
            #[cfg(unix)]
            {
                // SAFETY: kill(2) of the child this holds, not yet waited for.
                unsafe { libc::kill(i32::try_from(child.id()).unwrap(), libc::SIGQUIT) };
                let quit = Instant::now();
                while child.try_wait().unwrap().is_none() && quit.elapsed() < Duration::from_secs(5) {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "{cmd:?} did not exit within {timeout:?}\n--- stdout\n{}\n--- stderr\n{}",
                String::from_utf8_lossy(&took(&out)),
                String::from_utf8_lossy(&took(&err))
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    std::process::Output {
        status,
        stdout: took(&out),
        stderr: took(&err),
    }
}

fn run_shards_with<S: AsRef<std::ffi::OsStr>>(
    command: &[&str],
    args: &[S],
    env: &[(&str, &std::ffi::OsStr)],
    dir: Option<&Path>,
    timeout: Duration,
) -> Run {
    // What it was asked, for the failure that names it.
    let asked: Vec<String> = std::iter::once("shards".to_string())
        .chain(command.iter().map(|c| (*c).to_string()))
        .chain(args.iter().map(|a| a.as_ref().to_string_lossy().into_owned()))
        .collect();
    let mut cmd = self::command();
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    cmd.args(command).args(args).envs(env.iter().copied());
    run_command(cmd, &asked, timeout)
}

/// What a client that never ended shows before it is killed: its home's runs as its daemon
/// has them (`ps -a`), whether the daemon took its run at all, and what each running one's
/// guest runs ([`guests_processes`]); and on Windows its threads' stacks and its daemon's,
/// where this host has Microsoft's console debugger (cdb, the Windows SDK's Debugging
/// Tools), attached without stopping them (`-pv`).
fn stuck(pid: u32, home: Option<&Path>) -> String {
    stacks_of(&[("its client", pid)], home)
}

/// What `cmd` said by `timeout`, its stdout then its stderr; a word that it said nothing
/// where it never ended, so that the failure this adds to is never lost to one of its own.
fn said_within(cmd: &mut Command, timeout: Duration) -> String {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| output_within(cmd, timeout))) {
        Ok(out) => format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(_) => format!("(it did not end within {timeout:?})"),
    }
}

/// What each microVM running in `home` runs, as `top` lays its processes out (init's dump
/// of the guest's `/proc`), with each one's state and the kernel function it waits in
/// (`stat`, `wchan`): where in its guest a client that never ended, or a command that said
/// nothing more, is held.
pub fn guests_processes(home: &Path) -> String {
    let mut ids = command();
    ids.args(["ps", "-q", "--no-trunc"])
        .env("SHARDS_HOME", home)
        .stdin(Stdio::null());
    let listed = said_within(&mut ids, Duration::from_secs(20));
    let mut said = String::new();
    for id in listed
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        let mut top = command();
        top.args(["top", id, "-o", "pid,ppid,stat,wchan:32,args"])
            .env("SHARDS_HOME", home)
            .stdin(Stdio::null());
        said.push_str(&format!(
            "\n--- the processes of its microVM {}\n{}",
            id.get(..12).unwrap_or(id),
            said_within(&mut top, Duration::from_secs(20))
        ));
    }
    said
}

/// [`stuck`]'s view of processes `pids`, each named, as a test that found one of them stuck
/// shows them: its home's runs and their guests' processes, and on Windows their stacks and
/// its daemon's.
pub fn stacks_of(pids: &[(&str, u32)], home: Option<&Path>) -> String {
    let mut said = String::new();
    if let Some(home) = home {
        let mut ps = command();
        ps.args(["ps", "-a", "--no-trunc"])
            .env("SHARDS_HOME", home)
            .stdin(Stdio::null());
        said.push_str(&format!(
            "\n--- ps -a in its home\n{}",
            said_within(&mut ps, Duration::from_secs(20))
        ));
        said.push_str(&guests_processes(home));
        // What its daemon said last, of its runs and their groups' budgets: a run held or
        // stopped says why there alone.
        let log = std::fs::read(home.join("daemon.log")).unwrap_or_default();
        let tail = &log[log.len().saturating_sub(16 << 10)..];
        said.push_str(&format!(
            "\n--- the end of its daemon.log\n{}",
            String::from_utf8_lossy(tail)
        ));
    }
    #[cfg(windows)]
    {
        let cdb = Path::new(r"C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe");
        if cdb.is_file() {
            let daemon = home
                .and_then(|h| std::fs::read_to_string(h.join("daemon.pid")).ok())
                .and_then(|p| p.split_ascii_whitespace().next()?.parse::<u32>().ok());
            // shards' symbols: its PDB beside the build's own binary, which the copy these
            // tests run names (the test binary's deps directory and the one above it).
            let deps = std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(Path::to_path_buf));
            let symbols = deps
                .iter()
                .flat_map(|d| [d.clone()].into_iter().chain(d.parent().map(Path::to_path_buf)))
                .fold("srv*".to_string(), |path, d| format!("{path};{}", d.display()));
            for &(who, pid) in pids.iter().chain(daemon.map(|d| ("its daemon", d)).as_ref()) {
                let mut stacks = Command::new(cdb);
                stacks
                    .args([
                        "-pv",
                        "-p",
                        &pid.to_string(),
                        "-y",
                        &symbols,
                        "-c",
                        ".reload /f; ~*kn 40; qd",
                    ])
                    .stdin(Stdio::null());
                said.push_str(&format!(
                    "\n--- {who} ({pid}): its threads' stacks (cdb)\n{}",
                    said_within(&mut stacks, Duration::from_secs(120))
                ));
            }
        } else {
            said.push_str("\n(no cdb on this host for the stacks)");
        }
    }
    // Its stacks are Windows' alone here: Linux CI's hang watcher takes them there.
    #[cfg(not(windows))]
    let _ = pids;
    said
}

/// How long a home's daemon may say nothing of a client's subject while the client is past
/// its deadline before the client is taken for stuck ([`Said`]): the daemon's own
/// `READY_TIMEOUT` (daemon.rs), the longest it waits on a starting microVM that runs nothing
/// of its start. While one runs its start, however slowly its host runs it, the daemon says
/// so each `READY_STALL` (10 s), and says why it gave up on one when it does. What it says
/// each as long of a microVM a client waits on as it runs its workload ([`HEARTBEAT`],
/// D209) counts by what the microVM did ([`Floor`]).
pub const DAEMON_QUIET: Duration = Duration::from_secs(60);

/// The daemon's heartbeat of a microVM a client waits on (daemon.rs `log_progress`, D209),
/// in each of its lines: said each `READY_STALL` while the microVM's CPU runs, whatever it
/// runs, the same line again with its numbers grown. A nesting test whose runc hung waited
/// on it to Linux CI's 56-minute limit, its guest idle: it counts by its deltas alone, as
/// [`Floor`] judges them (ROOT's rulings, 2026-11-13).
pub const HEARTBEAT: &str = " runs as its client waits: ";

/// What this test process has learned of a microVM's work, as D215's judge learns a kind of
/// answer in its process's memory (shards_api's `Record`): the least CPU rate at which its run
/// still answered, its floor (ROOT's ruling, 2026-11-13). A slice of its work counts as
/// progress during its cold start, before its run has answered, by liveness alone (as a wait
/// begun before its kind's first answer follows its party's liveness); past it, only where
/// its CPU rate is not below that floor, compared by cross-multiplication (`Rate::below`):
/// an idle microVM's timer ticks are no progress of a test's subject, and a workload's CPU,
/// past what its run answered at, is. A window its run answered within teaches its record,
/// from its last teaching to the slice that closes it, as an answer teaches a wait's.
pub struct Floor {
    record: shards_api::Record,
    /// Its CPU time, and the wall time it was had over, since its record last learned.
    since: (Duration, Duration),
    /// Whether its run has answered since then.
    answered: bool,
}

/// The floors of every microVM this process's tests have followed, by its home and its run's
/// container and VM process (a daemon's heartbeats, [`Said`]) or its VM process ([`Ran`]).
static FLOORS: std::sync::Mutex<std::collections::BTreeMap<String, Floor>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

fn floors() -> std::sync::MutexGuard<'static, std::collections::BTreeMap<String, Floor>> {
    FLOORS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Floor {
    /// A microVM first seen, its run having answered meanwhile or not.
    pub fn new(answered: bool) -> Floor {
        Floor {
            record: shards_api::Record::new(),
            since: (Duration::ZERO, Duration::ZERO),
            answered,
        }
    }

    /// A slice of its work, `cpu` over `over`, `live` as its follower's liveness judges it:
    /// whether it is progress, judged by what its record learned before it; then, where its
    /// run answered within the window this slice closes, that window taught.
    pub fn slice(&mut self, cpu: Duration, over: Duration, live: bool) -> bool {
        let progressed = match self.record.seen() {
            shards_api::Seen::Cold => live,
            shards_api::Seen::Learned { least, .. } => !shards_api::Rate { cpu, over }.below(&least),
        };
        self.since = (
            self.since.0.saturating_add(cpu),
            self.since.1.saturating_add(over),
        );
        if std::mem::take(&mut self.answered) {
            let (work, wall) = std::mem::take(&mut self.since);
            self.record.teach(wall, work);
        }
        progressed
    }

    /// Its run answered: the window its next slice closes teaches its record.
    pub fn answered(&mut self) {
        self.answered = true;
    }
}

/// A heartbeat's microVM, by its run's container and its VM process, and the CPU time it ran
/// over the slice it says (`+N ms in Ss`, platform::Progress's `Look`).
fn heartbeat(line: &str) -> Option<(&str, Duration, Duration)> {
    let (vm, look) = line.split_once(HEARTBEAT)?;
    let vm = vm.split_once(": container ").map_or(vm, |(_, vm)| vm);
    let slice = look.split_once("(+")?.1.split_once(')')?.0;
    let (ms, secs) = slice.split_once(" ms in ")?;
    let secs = secs.strip_suffix('s')?;
    Some((
        vm,
        Duration::from_millis(ms.parse().ok()?),
        Duration::from_secs(secs.parse().ok()?),
    ))
}

/// What the daemon of a home has said in its log since this began watching: a client past
/// its deadline is waited on while its daemon says something of it within [`DAEMON_QUIET`],
/// as it does while it waits on a microVM its host boots slowly (the parity plan's step 10:
/// boots past a minute on an arm64 Windows host nested under QEMU's TCG): each line but its
/// [`HEARTBEAT`], and that by what its microVM did ([`Floor`]). A test's deadline then holds
/// where its host is fast, and follows the daemon's own judgment of a start, and its
/// microVMs' own work, where it is slow: never a fixed guess at the host's speed.
pub struct Said {
    log: PathBuf,
    /// Its microVMs' floors' prefix in [`FLOORS`]: its home's.
    home: String,
    /// How far the log has been read: to the end of the last whole line in it.
    len: u64,
    /// When it last said something of its subject, since this began watching, to within a
    /// second.
    at: Option<Instant>,
    /// When it was last read: once a second at most.
    read: Option<Instant>,
    /// Whether its run has answered since it was last read, and since this began watching.
    answers: (bool, bool),
}

impl Said {
    pub fn of(home: &Path) -> Said {
        let log = home.join("daemon.log");
        let len = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
        Said {
            log,
            home: format!("{}|", home.display()),
            len,
            at: None,
            read: None,
            answers: (false, false),
        }
    }

    /// Its run answered, as its client's own output says ([`run_command`]'s client): the
    /// window each of its microVMs' next heartbeat closes teaches its floor.
    pub fn answered(&mut self) {
        self.answers = (true, true);
    }

    /// Whether the daemon has said something but its [`HEARTBEAT`] within [`DAEMON_QUIET`]:
    /// a microVM's start said slowly among it (daemon.rs `ready`: liveness governs a cold
    /// start, which the daemon gives up on itself past its `READY_TIMEOUT`). Asked as often
    /// as its caller likes, it reads the log once a second.
    pub fn lately(&mut self) -> bool {
        if self.read.is_none_or(|r| r.elapsed() >= Duration::from_secs(1)) {
            self.read = Some(Instant::now());
            if self.said_anew() {
                self.at = Some(Instant::now());
            }
        }
        self.at.is_some_and(|at| at.elapsed() < DAEMON_QUIET)
    }

    /// Whether the whole lines the log gained since its last read say anything of its
    /// subject, read to the end of the last of them: each but its heartbeat, and that where
    /// its microVM's floor takes it for progress; a log begun again is read from its start.
    fn said_anew(&mut self) -> bool {
        use std::io::Seek as _;
        let Ok(mut log) = std::fs::File::open(&self.log) else {
            return false;
        };
        let size = log.metadata().map(|m| m.len()).unwrap_or(0);
        if size < self.len {
            self.len = 0;
        }
        let mut new = Vec::new();
        if size == self.len
            || log.seek(io::SeekFrom::Start(self.len)).is_err()
            || log.take(size - self.len).read_to_end(&mut new).is_err()
        {
            return false;
        }
        let Some(last) = new.iter().rposition(|&b| b == b'\n') else {
            return false;
        };
        self.len += last as u64 + 1;
        let mut floors = floors();
        if std::mem::take(&mut self.answers.0) {
            for (_, floor) in floors
                .range_mut(self.home.clone()..)
                .take_while(|(k, _)| k.starts_with(&self.home))
            {
                floor.answered();
            }
        }
        let mut said = false;
        for line in new[..last].split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            let line = String::from_utf8_lossy(line);
            match line.contains(HEARTBEAT).then(|| heartbeat(&line)) {
                // One whose slice cannot be read says nothing of what its microVM did.
                Some(None) => {}
                Some(Some((vm, cpu, over))) => {
                    let floor = floors
                        .entry(format!("{}{vm}", self.home))
                        .or_insert_with(|| Floor::new(self.answers.1));
                    // The daemon says it only where its microVM ran past an idle one
                    // (`progressed`): its liveness.
                    said |= floor.slice(cpu, over, true);
                }
                None => {
                    // A run's own line answers for its microVMs.
                    if let Some(id) = line.split_once("container ").and_then(|(_, r)| r.split_once(':')) {
                        let of = format!("{}{}", self.home, id.0);
                        for (_, floor) in floors
                            .range_mut(of.clone()..)
                            .take_while(|(k, _)| k.starts_with(&of))
                        {
                            floor.answered();
                        }
                    }
                    said = true;
                }
            }
        }
        said
    }
}

/// The slice a microVM's progress is judged over, each as long: the daemon's `READY_STALL`
/// (daemon.rs), over which it judges a starting microVM, and one a client waits on, and
/// logs it.
pub const PROGRESS_SLICE: Duration = Duration::from_secs(10);

/// What the microVMs of a client's own have run: [`Said`]'s counterpart for one whose
/// microVM is no daemon's, which says nothing of it. `shards run --kernel` and `shards
/// restore` are the VM process on Unix and start it on Windows, and `shards build`'s
/// builders are its children (builds run in its process). Each VM process under it is
/// judged each [`PROGRESS_SLICE`] as the daemon judges one, by its CPU time against the
/// most an idle microVM takes in as long (`shards_vmm::platform::progressed`, PM M357): a
/// client past its deadline is waited on while one of them ran within [`DAEMON_QUIET`], as
/// a guest's workload runs at its host's speed (the parity plan's step 10: a 64-vCPU storm
/// and a 3 GiB build step on an arm64 Windows host nested under QEMU's TCG). One that runs
/// nothing for as long fails it as before. Not caught: a guest that spins for ever, as
/// D209 says of a start, which its CI job's limit ends.
pub struct Ran {
    client: u32,
    followed: std::collections::HashMap<u32, shards_vmm::platform::Progress>,
    /// When its processes were last looked at.
    looked: Instant,
    /// When one of them last ran, since this began watching.
    at: Option<Instant>,
    /// Whether its client's run has answered since this began watching.
    answered: bool,
}

impl Ran {
    pub fn of(client: u32) -> Ran {
        Ran {
            client,
            followed: std::collections::HashMap::new(),
            looked: Instant::now(),
            at: None,
            answered: false,
        }
    }

    /// Its client's run answered, as its output says ([`run_command`]): the window each of
    /// its VM processes' next look closes teaches its floor.
    pub fn answered(&mut self) {
        self.answered = true;
        let mut floors = floors();
        for pid in self.followed.keys() {
            if let Some(floor) = floors.get_mut(&format!("vm process {pid}")) {
                floor.answered();
            }
        }
    }

    /// Whether a VM process under its client ran within [`DAEMON_QUIET`], as its [`Floor`]
    /// judges what it ran: during its cold start, past an idle microVM's
    /// (`platform::progressed`); past it, not below its floor. Asked as often as its caller
    /// likes, it looks once each [`PROGRESS_SLICE`]; one found anew is judged from the next
    /// look on.
    pub fn lately(&mut self) -> bool {
        if self.looked.elapsed() >= PROGRESS_SLICE {
            self.looked = Instant::now();
            let vms = vm_processes_under(self.client);
            self.followed.retain(|pid, _| vms.contains(pid));
            let mut floors = floors();
            for pid in vms {
                let floor = floors
                    .entry(format!("vm process {pid}"))
                    .or_insert_with(|| Floor::new(self.answered));
                match self.followed.entry(pid) {
                    std::collections::hash_map::Entry::Occupied(mut p) => {
                        let look = p.get_mut().look();
                        let ran = look
                            .before
                            .zip(look.now)
                            .map(|(b, n)| Duration::from_nanos(n.cpu_ns.saturating_sub(b.cpu_ns)));
                        if let Some(ran) = ran
                            && floor.slice(ran, look.over, look.ran)
                        {
                            self.at = Some(Instant::now());
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(v) => {
                        v.insert(shards_vmm::platform::Progress::of(pid));
                    }
                }
            }
        }
        self.at.is_some_and(|at| at.elapsed() < DAEMON_QUIET)
    }
}

/// How long a test waits on a state a home's daemon or its microVMs come to (a pool
/// filled, a restored run's VM up, a VM gone): `timeout`, as before, or past it for as long
/// as they progress, as [`run_command`] waits on a client: while the home's daemon says
/// something within [`DAEMON_QUIET`] ([`Said`]), or a VM process under it, or under this
/// test's process (its clients' own, `run --kernel` and `restore`'s), ran within as long
/// ([`Ran`]). Fast hosts keep `timeout`; a slow one (the parity plan's step 10, an arm64
/// Windows host nested under QEMU's TCG, whose template saves took 20 s and pool restores
/// a further minute) is waited on while it works, and a stuck one fails a minute after its
/// last progress. The look that fails a wait begins past it: one that takes long itself,
/// as a PowerShell's look at Windows' processes does, is never what the wait runs out in.
pub struct Patience {
    start: Instant,
    timeout: Duration,
    home: Option<PathBuf>,
    said: Option<Said>,
    /// The VM processes under this test's process.
    own: Ran,
    /// The home's daemon, and the VM processes under it, followed anew as it changes.
    daemon: Option<(u32, Ran)>,
    /// When the daemon's pid was last read: once a second at most.
    read: Option<Instant>,
    /// Whether patience has run out, the look after it still to come.
    out: bool,
}

impl Patience {
    /// Patience with the daemon of `home` and its microVMs, and this test's own.
    pub fn of(home: &Path, timeout: Duration) -> Patience {
        Patience::with(Some(home), timeout)
    }

    /// Patience with this test's own microVMs, where no home's daemon serves them.
    pub fn own(timeout: Duration) -> Patience {
        Patience::with(None, timeout)
    }

    fn with(home: Option<&Path>, timeout: Duration) -> Patience {
        Patience {
            start: Instant::now(),
            timeout,
            home: home.map(Path::to_path_buf),
            said: home.map(Said::of),
            own: Ran::of(std::process::id()),
            daemon: None,
            read: None,
            out: false,
        }
    }

    /// Whether to look again: within its timeout, or past it while the daemon or a microVM
    /// progresses; once neither does, once more, the look after this one, and then no more.
    /// Ask it after each look that found the state not yet come, as often as they come.
    pub fn holds(&mut self) -> bool {
        let due = self.read.is_none_or(|r| r.elapsed() >= Duration::from_secs(1));
        if let Some(home) = self.home.as_ref().filter(|_| due) {
            self.read = Some(Instant::now());
            let pid = std::fs::read_to_string(home.join("daemon.pid"))
                .ok()
                .and_then(|p| p.split_ascii_whitespace().next()?.parse::<u32>().ok());
            if pid != self.daemon.as_ref().map(|(d, _)| *d) {
                self.daemon = pid.map(|d| (d, Ran::of(d)));
            }
        }
        // Each asked each time, so that each sees when it last progressed.
        let said = self.said.as_mut().is_some_and(Said::lately);
        let ran = self.own.lately();
        let daemons = self.daemon.as_mut().is_some_and(|(_, r)| r.lately());
        if self.start.elapsed() < self.timeout || said || ran || daemons {
            self.out = false;
            return true;
        }
        !std::mem::replace(&mut self.out, true)
    }
}

/// The VM processes `client` is, or started, as far down as its descendants go: shards-vm's
/// and the GPU's (shards-vm-gpu), from Windows' table of processes or `ps`.
pub fn vm_processes_under(client: u32) -> Vec<u32> {
    let listed = processes();
    let is_vm = |name: &str| {
        let name = name
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(name)
            .to_ascii_lowercase();
        let name = name.strip_suffix(".exe").unwrap_or(&name);
        name == "shards-vm" || name == "shards-vm-gpu"
    };
    let mut under = vec![client];
    let mut i = 0;
    while let Some(&parent) = under.get(i) {
        for (pid, ppid, _) in &listed {
            if *ppid == parent && !under.contains(pid) {
                under.push(*pid);
            }
        }
        i += 1;
    }
    listed
        .iter()
        .filter(|(pid, _, name)| under.contains(pid) && is_vm(name))
        .map(|(pid, ..)| *pid)
        .collect()
}

/// Every process, its parent and its command's name, from `ps`.
#[cfg(unix)]
pub fn processes() -> Vec<(u32, u32, String)> {
    Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,comm="])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|line| {
                    let mut w = line.split_whitespace();
                    Some((
                        w.next()?.parse().ok()?,
                        w.next()?.parse().ok()?,
                        w.collect::<Vec<_>>().join(" "),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Runs `cmd` as built, `shards` itself ([`run_shards_with`]) or a program that runs it
/// (given [`as_tests_run`]'s environment), its output collected; killed, failing the test,
/// after `timeout`, `asked` naming it then, or, where its home's daemon still says
/// something then, once it has said nothing for [`DAEMON_QUIET`] ([`Said`]), or where a
/// microVM of its own still runs then, once none has run for as long ([`Ran`]).
pub fn run_command(cmd: Command, asked: &[String], timeout: Duration) -> Run {
    run_ending(cmd, asked, timeout, Leftovers::Ended).unwrap_or_else(|ended| panic!("{}", ended.said))
}

/// What a run ended at its deadline leaves of the processes under it ([`end_run`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Leftovers {
    /// Ended by the harness with it, on Windows: a test that fails at its deadline leaves
    /// no process, whatever the run started (a daemon among them).
    Ended,
    /// Waited for, as the run's own end should end them: those still running then are
    /// [`Ended::left`], and only then ended by the harness.
    Awaited,
}

/// A run [`run_until`] ended at its deadline: what it had said and shown ([`stuck`]), the
/// processes it had started (those under it, each named), and those of them still running
/// once it was ended and they were waited for.
pub struct Ended {
    pub said: String,
    pub started: Vec<Process>,
    pub left: Vec<Process>,
}

/// A process by its ID and its executable's name.
pub type Process = (u32, String);

/// [`run_command`]'s run, ended at its deadline as [`Ended`] says rather than failing the
/// test there, for a test of what an ended run leaves: the processes under it are left to
/// end as its own end ends them ([`Leftovers::Awaited`]).
pub fn run_until(cmd: Command, asked: &[String], timeout: Duration) -> Result<Run, Ended> {
    run_ending(cmd, asked, timeout, Leftovers::Awaited)
}

/// [`run_until`]'s run, the processes under it at its deadline as `leftovers` says.
fn run_ending(
    mut cmd: Command,
    asked: &[String],
    timeout: Duration,
    leftovers: Leftovers,
) -> Result<Run, Ended> {
    let start = Instant::now();
    // Its home, for what is shown of a run that never ends ([`stuck`]).
    let home: Option<PathBuf> = cmd
        .get_envs()
        .find(|(k, _)| *k == "SHARDS_HOME")
        .and_then(|(_, v)| v.map(PathBuf::from));
    let mut child = cmd
        .env("SHARDS_TIMING", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning shards");
    // Read on threads of their own and handed over by channel as it comes, so that a pipe
    // another process still holds (a VM given the client's stdio) costs the test its
    // deadline, never a hang, and what came before it is kept: a command that never ends
    // shows how far it got.
    let collect = |mut r: Box<dyn Read + Send>| {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut b = vec![0u8; 64 * 1024];
            while let Ok(n @ 1..) = r.read(&mut b) {
                if tx.send(b[..n].to_vec()).is_err() {
                    return;
                }
            }
        });
        rx
    };
    // All it wrote, to its end, after what had come of it (`got`); or what came by the
    // deadline, where it never ends. Lossily: a step's output is printed as it came, bytes
    // that are no UTF-8 included, which a strict conversion would drop the whole of.
    let took = |rx: &std::sync::mpsc::Receiver<Vec<u8>>, mut got: Vec<u8>| {
        let until = Instant::now()
            .max(start + timeout)
            .max(Instant::now() + Duration::from_secs(1));
        loop {
            match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(chunk) => got.extend_from_slice(&chunk),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break String::from_utf8_lossy(&got).into_owned();
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    break format!(
                        "{}\n(still held open by another process)",
                        String::from_utf8_lossy(&got)
                    );
                }
            }
        }
    };
    let out = collect(Box::new(child.stdout.take().unwrap()));
    let err = collect(Box::new(child.stderr.take().unwrap()));
    let (mut out_got, mut err_got) = (Vec::new(), Vec::new());
    // What came of it since the look before: its run's answers, which teach its microVMs'
    // floors ([`Floor`]).
    let came = |rx: &std::sync::mpsc::Receiver<Vec<u8>>, got: &mut Vec<u8>| {
        let mut came = false;
        while let Ok(chunk) = rx.try_recv() {
            got.extend_from_slice(&chunk);
            came = true;
        }
        came
    };
    let mut said = home.as_deref().map(Said::of);
    let mut ran = Ran::of(child.id());
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if came(&out, &mut out_got) | came(&err, &mut err_got) {
            if let Some(said) = said.as_mut() {
                said.answered();
            }
            ran.answered();
        }
        // Each asked each time, so that it sees when the daemon last said something, and
        // when a microVM of the client's own last ran.
        let said_lately = said.as_mut().is_some_and(Said::lately);
        let ran_lately = ran.lately();
        if start.elapsed() > timeout && !said_lately && !ran_lately {
            let seen = stuck(child.id(), home.as_deref());
            let (started, left) = end_run(&mut child, timeout, leftovers);
            return Err(Ended {
                said: format!(
                    "{} did not exit within {timeout:?}\n--- stdout\n{}\n--- stderr\n{}{seen}",
                    asked.join(" "),
                    took(&out, std::mem::take(&mut out_got)),
                    took(&err, std::mem::take(&mut err_got))
                ),
                started,
                left,
            });
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal = None;
    Ok(Run {
        status: status.code(),
        signal,
        stdout: took(&out, out_got),
        stderr: took(&err, err_got),
        elapsed: start.elapsed(),
    })
}

/// Ends `child`, a run past its deadline, and gives the processes under it, each named,
/// with any still running `within` after: read from one snapshot while it still runs, so
/// that each is the run's by its parent. With [`Leftovers::Ended`] the harness ends those
/// on Windows as it ends the run (on Unix a run's share process ends with its VM, the
/// client itself: cli/shares.rs); with [`Leftovers::Awaited`] it ends none of them, which
/// the run's own end must end (D141: on Windows the job its client alone holds), and only
/// those left past `within` afterwards, for the test to fail on, not to outlive it.
fn end_run(
    child: &mut std::process::Child,
    within: Duration,
    leftovers: Leftovers,
) -> (Vec<Process>, Vec<Process>) {
    if cfg!(unix) && leftovers == Leftovers::Ended {
        let _ = child.kill();
        let _ = child.wait();
        return (Vec::new(), Vec::new());
    }
    let under = under(child.id());
    let _ = child.kill();
    let _ = child.wait();
    if leftovers == Leftovers::Ended {
        for p in &under {
            p.end();
        }
    }
    let until = Instant::now() + within;
    let started = under.iter().map(|p| (p.pid, p.name.clone())).collect();
    let left: Vec<&Under> = under.iter().filter(|p| !p.ended_by(until)).collect();
    for p in &left {
        p.end();
    }
    (started, left.iter().map(|p| (p.pid, p.name.clone())).collect())
}

/// A process under a run, opened while the run still ran: on Windows a handle to end it
/// and wait on; on Unix its ID, which names it until it is reaped.
struct Under {
    pid: u32,
    name: String,
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
}

impl Under {
    /// Whether it has ended by `until`.
    fn ended_by(&self, until: Instant) -> bool {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
            use windows_sys::Win32::System::Threading::WaitForSingleObject;
            let ms = u32::try_from(until.saturating_duration_since(Instant::now()).as_millis())
                .unwrap_or(u32::MAX);
            // SAFETY: a process handle `under` opened, waited on until `until`.
            unsafe { WaitForSingleObject(self.handle.as_raw_handle(), ms) == WAIT_OBJECT_0 }
        }
        // Its parent gone, it is reaped by the one it was given to as it ends: kill(2)
        // finds no process then.
        #[cfg(unix)]
        loop {
            let Ok(pid) = libc::pid_t::try_from(self.pid) else {
                return true;
            };
            // SAFETY: kill(2) with signal 0, which only asks whether the process is there.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Ends it.
    fn end(&self) {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            // SAFETY: TerminateProcess of a process handle `under` opened with the right to.
            unsafe {
                windows_sys::Win32::System::Threading::TerminateProcess(self.handle.as_raw_handle(), 1)
            };
        }
        #[cfg(unix)]
        if let Ok(pid) = libc::pid_t::try_from(self.pid) {
            // SAFETY: kill(2) of a process this run started, found under it.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// The processes under `root` (its children, theirs and so on), as one snapshot of them
/// lists them ([`processes`]): a pass over the snapshot for each generation, at most as
/// many as it lists. Each is opened, on Windows, to be ended and waited for, while `root`
/// still runs; named by its executable's file name.
fn under(root: u32) -> Vec<Under> {
    let all = processes();
    let mut tree = vec![root];
    for _ in 0..all.len() {
        let grew: Vec<u32> = all
            .iter()
            .filter(|(pid, ppid, _)| tree.contains(ppid) && !tree.contains(pid))
            .map(|(pid, _, _)| *pid)
            .collect();
        if grew.is_empty() {
            break;
        }
        tree.extend(grew);
    }
    all.into_iter()
        .filter(|(pid, _, _)| *pid != root && tree.contains(pid))
        .filter_map(|(pid, _, name)| {
            let name = name.rsplit(['/', '\\']).next().unwrap_or(&name).to_string();
            Under::opened(pid, name)
        })
        .collect()
}

impl Under {
    /// Process `pid`, opened on Windows to be ended and waited for: none where it does not
    /// open (it has ended).
    #[cfg(windows)]
    fn opened(pid: u32, name: String) -> Option<Under> {
        use std::os::windows::io::FromRawHandle as _;
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE};
        // SAFETY: a handle to a process the snapshot lists, owned below or not at all.
        let raw = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: the handle just opened, which nothing else owns.
        let handle = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw) };
        Some(Under { pid, name, handle })
    }

    /// Process `pid`, named by its ID until it is reaped.
    #[cfg(unix)]
    #[allow(clippy::unnecessary_wraps)]
    fn opened(pid: u32, name: String) -> Option<Under> {
        Some(Under { pid, name })
    }
}

/// A fresh directory, short enough for sockaddr_un paths, removed with everything in it
/// (sockets, snapshots) when dropped (shards_testdir: a killed test's, as the next test
/// process starts). Declare it before the VMs that use it.
pub struct TempDir(shards_testdir::TempDir);

impl TempDir {
    pub fn new(name: &str) -> TempDir {
        TempDir(shards_testdir::TempDir::new(name).unwrap())
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
        // directory: shown first, straight to stderr, which libtest does not capture. The
        // directory goes after, as the field drops.
        if std::thread::panicking() {
            show_why(&self.0);
        }
    }
}

/// What a home's runs wrote to stderr (`*.stderr`), the stacks the daemon sampled of a VM
/// that stalled before its first request for access (`vm-PID.sample`, daemon.rs `grant`),
/// and its daemon's log's last lines: shown before the home goes, with them.
fn show_why(home: &Path) {
    let mut err = std::io::stderr().lock();
    let mut said: Vec<PathBuf> = std::fs::read_dir(home)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "stderr" || x == "sample"))
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
    registry_serving_at(std::net::Ipv4Addr::LOCALHOST.into(), image, stalls)
}

/// [`registry`], listening at `ip`: a registry off loopback where `ip` is this host's on
/// its route out ([`routable_v4`]).
pub fn registry_at(ip: std::net::IpAddr, manifest: Vec<u8>, blobs: Vec<Vec<u8>>) -> (u16, Arc<AtomicUsize>) {
    let image = Arc::new(std::sync::Mutex::new((manifest, blobs)));
    let (port, served, _) = registry_serving_at(ip, image, None);
    (port, served)
}

/// This host's IPv4 address on its route out, where it has one.
pub fn routable_v4() -> Option<std::net::Ipv4Addr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // TEST-NET-1 (RFC 5737): a route is chosen, nothing is sent.
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => Some(v4),
        _ => None,
    }
}

fn registry_serving_at(
    ip: std::net::IpAddr,
    image: Served,
    stalls: Option<(Arc<AtomicUsize>, Arc<AtomicUsize>)>,
) -> (u16, Arc<AtomicUsize>, Served) {
    let listener = TcpListener::bind((ip, 0)).unwrap();
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
                        } else if has(doc, b"application/vnd.docker.distribution.manifest.list.v2+json") {
                            "application/vnd.docker.distribution.manifest.list.v2+json"
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

/// The test image as `docker save` writes an archive of an image named `name` (`NAME:TAG`):
/// an OCI layout (`oci-layout`, an `index.json` that names it, its blobs) with Docker's
/// `manifest.json` beside, as Docker 25 and later write one.
pub fn image_archive(name: &str) -> Vec<u8> {
    let (manifest, blobs) = test_image();
    let [config, layer] = &blobs[..] else {
        panic!("the test image is a config and a layer");
    };
    let path = |b: &[u8]| format!("blobs/sha256/{}", sha256_digest(b).trim_start_matches("sha256:"));
    let tag = name.rsplit_once(':').map_or("latest", |(_, tag)| tag);
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{}","size":{},"annotations":{{"io.containerd.image.name":"docker.io/library/{name}","org.opencontainers.image.ref.name":"{tag}"}}}}]}}"#,
        sha256_digest(&manifest),
        manifest.len()
    );
    let archive_manifest = format!(
        r#"[{{"Config":"{}","RepoTags":["{name}"],"Layers":["{}"]}}]"#,
        path(config),
        path(layer)
    );
    let (manifest_at, config_at, layer_at) = (path(&manifest), path(config), path(layer));
    tar(&[
        ("blobs", 0o755, 0, None),
        ("blobs/sha256", 0o755, 0, None),
        (&config_at, 0o644, 0, Some(config)),
        (&layer_at, 0o644, 0, Some(layer)),
        (&manifest_at, 0o644, 0, Some(&manifest)),
        ("index.json", 0o644, 0, Some(index.as_bytes())),
        ("manifest.json", 0o644, 0, Some(archive_manifest.as_bytes())),
        ("oci-layout", 0o644, 0, Some(br#"{"imageLayoutVersion":"1.0.0"}"#)),
    ])
}

/// The test image behind an index, as multi-platform images are served: its manifest
/// for this host's platform, one for another architecture that is never fetched, and an
/// attestation for ours, as D72's builds write one. Returns the index and its blobs, the
/// manifests among them: the image's config, layer and manifest first, then the
/// attestation's manifest, config and statement, then the other platform's attestation.
pub fn test_index() -> (Vec<u8>, Vec<Vec<u8>>) {
    let (manifest, mut blobs) = test_image();
    let (arch, other) = match std::env::consts::ARCH {
        "aarch64" => ("arm64", "amd64"),
        _ => ("amd64", "arm64"),
    };
    // An attestation as D72's builds write one: an empty config, and an in-toto statement.
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

/// A static program for `arch` (`amd64` or `arm64`) that does nothing but meet an
/// undefined instruction (x86-64's `ud2`, AArch64's `udf #0`): it dies of SIGILL, a fault,
/// natively or emulated alike. One ELF64 program header loads the file whole.
pub fn crashing_program(arch: &str) -> Vec<u8> {
    let code: &[u8] = match arch {
        "amd64" => &[0x0f, 0x0b],
        "arm64" => &[0, 0, 0, 0],
        other => panic!("no crashing program for {other}"),
    };
    elf_program(arch, code)
}

/// A static program for `arch` (`amd64` or `arm64`) that writes `text` to its stdout and
/// exits 0, with Linux's own system calls alone: what an image of that CPU runs, natively
/// or emulated.
pub fn printing_program(arch: &str, text: &[u8]) -> Vec<u8> {
    let len = u32::try_from(text.len()).unwrap();
    let mut code = Vec::new();
    match arch {
        "amd64" => {
            // write(1, msg, len); exit(0). `msg` follows the code, 16 bytes past the
            // `lea`'s end.
            code.extend_from_slice(&[0xb8, 1, 0, 0, 0, 0xbf, 1, 0, 0, 0, 0x48, 0x8d, 0x35]);
            code.extend_from_slice(&16u32.to_le_bytes());
            code.push(0xba);
            code.extend_from_slice(&len.to_le_bytes());
            code.extend_from_slice(&[0x0f, 0x05, 0xb8, 60, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05]);
        }
        "arm64" => {
            assert!(len < 1 << 16, "a short text");
            // mov x0, #1; adr x1, msg (28 bytes on); mov x2, #len; mov x8, #64 (write);
            // svc #0; mov x0, #0; mov x8, #93 (exit); svc #0.
            for word in [
                0xd280_0020u32,
                0x1000_00e1,
                0xd280_0002 | (len << 5),
                0xd280_0808,
                0xd400_0001,
                0xd280_0000,
                0xd280_0ba8,
                0xd400_0001,
            ] {
                code.extend_from_slice(&word.to_le_bytes());
            }
        }
        other => panic!("no printing program for {other}"),
    }
    code.extend_from_slice(text);
    elf_program(arch, &code)
}

/// A static ELF64 program for `arch` whose entry is `code`: one program header loads the
/// file whole.
fn elf_program(arch: &str, code: &[u8]) -> Vec<u8> {
    let machine: u16 = match arch {
        "amd64" => 0x3e,
        "arm64" => 0xb7,
        other => panic!("no program for {other}"),
    };
    let base: u64 = 0x40_0000;
    let entry: u64 = 0x78;
    let total = entry + code.len() as u64;
    let mut elf = b"\x7fELF\x02\x01\x01\x00".to_vec();
    elf.extend_from_slice(&[0; 8]);
    elf.extend_from_slice(&2u16.to_le_bytes());
    elf.extend_from_slice(&machine.to_le_bytes());
    elf.extend_from_slice(&1u32.to_le_bytes());
    for word in [base + entry, 0x40, 0] {
        elf.extend_from_slice(&word.to_le_bytes());
    }
    elf.extend_from_slice(&0u32.to_le_bytes());
    for half in [0x40u16, 0x38, 1, 0, 0, 0] {
        elf.extend_from_slice(&half.to_le_bytes());
    }
    // PT_LOAD, readable and executable: the file at `base`.
    elf.extend_from_slice(&1u32.to_le_bytes());
    elf.extend_from_slice(&5u32.to_le_bytes());
    for word in [0, base, base, total, total, 0x1000] {
        elf.extend_from_slice(&word.to_le_bytes());
    }
    elf.extend_from_slice(code);
    elf
}

/// Serves the test image at `127.0.0.1:<port>/test/image:v1`: one registry for every test
/// of the binary, started by the first that asks, so that no test leaves a server of its
/// own behind (each had one, its thread blocked in accept(2) until the binary ended). What
/// it counts is every test's requests; a test that counts its own has [`served_counting`].
pub fn served() -> (String, Arc<AtomicUsize>) {
    static SHARED: OnceLock<(String, Arc<AtomicUsize>)> = OnceLock::new();
    SHARED.get_or_init(served_counting).clone()
}

/// [`served`], from a registry of the caller's own, whose count is its requests alone.
pub fn served_counting() -> (String, Arc<AtomicUsize>) {
    let (manifest, blobs) = test_image();
    let (port, served) = registry(manifest, blobs);
    (format!("127.0.0.1:{port}/test/image:v1"), served)
}

/// A remote registry's image `reference` (Docker Hub's, ghcr.io's), pinned by its digest,
/// served by a registry of the tests' own as `127.0.0.1:<port>/test/image@<digest>`: the
/// same image, never pulled from its registry as a test runs. Docker Hub counts its
/// anonymous pulls by address, here the whole LAN's IPv6 /64, 100 an hour, and test homes
/// that pulled their base from it each run spent them for every machine there (CI cluster
/// 13: api_image.rs's builds, refused from its eighth round of 30 on); ghcr.io limits its own.
/// What it serves is what a pull of the image for this host's platform fetches, pulled once
/// per checkout ([`hub_seed`]); one registry serves each image to every test of the binary.
pub fn hub(reference: &str, timeout: Duration) -> String {
    static SERVED: OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> = OnceLock::new();
    let mut served = SERVED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(at) = served.get(reference) {
        return at.clone();
    }
    let (top, blobs) = hub_seed(reference, timeout);
    let digest = sha256_digest(&top);
    let (port, _) = registry(top, blobs);
    let at = format!("127.0.0.1:{port}/test/image@{digest}");
    served.insert(reference.to_string(), at.clone());
    at
}

/// `reference` as a test pulls it: a remote registry's image pinned by its digest from the
/// tests' own registry ([`hub`]), any other (a loopback registry's, a tag) as it is.
pub fn hubless(reference: &str, timeout: Duration) -> String {
    match shards_image::reference::Reference::parse(reference) {
        Ok(r) if remote(&r) && r.digest.is_some() => hub(reference, timeout),
        _ => reference.to_string(),
    }
}

/// Whether `r` names a registry off this machine: not `localhost`, nor a loopback address.
fn remote(r: &shards_image::reference::Reference) -> bool {
    let domain = r.domain.as_str();
    let host = match domain.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or(bracketed),
        None => domain.split(':').next().unwrap_or(domain),
    };
    host != "localhost" && !host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// `agentfile` with each base it takes `FROM` a remote registry by a digest taken from the
/// tests' own registry instead ([`hubless`]): the same image, every other word as it was.
pub fn hub_bases(agentfile: &str, timeout: Duration) -> String {
    agentfile
        .split_inclusive('\n')
        .map(|line| {
            let base = line
                .strip_prefix("FROM ")
                .and_then(|rest| rest.split_whitespace().find(|w| !w.starts_with("--")));
            match base {
                Some(base) => line.replacen(base, &hubless(base, timeout), 1),
                None => line.to_string(),
            }
        })
        .collect()
}

/// What [`hub`] serves of `reference`: the document its digest names, then each manifest,
/// config and layer of it a pull for this host's platform fetched (the platform's manifest
/// and its attestations), from the store of target/artifacts/hub, a home of the tests' own
/// that a build pulls each into from its registry the first time it is asked for, in
/// process, as a build pulls its base (no daemon): once per checkout, as the pinned kernel
/// is downloaded once into target/artifacts ([`fetch`]), one test process at a time.
fn hub_seed(reference: &str, timeout: Duration) -> (Vec<u8>, Vec<Vec<u8>>) {
    let pinned = shards_image::reference::Reference::parse(reference).unwrap();
    assert!(
        remote(&pinned) && pinned.digest.is_some(),
        "{reference}: a remote registry's image pinned by its digest"
    );
    let home = workspace().join("target/artifacts/hub");
    shards_vmm::platform::create_private_dir(&home).unwrap();
    let _seeding = shards_image::store::Locked::exclusive(&home.join("seeding.lock")).unwrap();
    if let Ok(seed) = hub_held(&home, &pinned) {
        return seed;
    }
    let ctx = TempDir::new("hub-seed");
    std::fs::write(ctx.join("Dockerfile"), format!("FROM {reference}\n")).unwrap();
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let built = run_shards_env(&["build"], &[ctx.as_os_str()], &env, timeout);
    assert_eq!(
        built.status,
        Some(0),
        "{reference}, pulled from its registry once into {}: {}",
        home.display(),
        built.stderr
    );
    hub_held(&home, &pinned).unwrap_or_else(|missing| panic!("{reference}, pulled once: {missing}"))
}

/// What `home`'s store holds of `pinned`, found by its digest as content is, whoever's
/// record holds it: a pull of an index holds its platform's manifest by its own digest, under
/// the index's name alone, and a seed's build that finds a digest held pulls nothing (D78).
/// The document the digest names; then, of an index, this host's platform's manifest and
/// its attestations, which a pull of it fetches, and of each manifest its config and
/// layers. Where one is not there whole, what is missing.
pub fn hub_held(
    home: &Path,
    pinned: &shards_image::reference::Reference,
) -> Result<(Vec<u8>, Vec<Vec<u8>>), String> {
    let store = shards_image::store::Store::open(&home.join("images")).map_err(|e| e.to_string())?;
    let read = |what: &str, d: &str| {
        let path = shards_image::reference::Digest::parse(d)
            .map(|digest| store.blob_path(&digest))
            .map_err(|e| format!("{pinned}: {what} {d}: {e}"))?;
        match std::fs::read(&path) {
            Ok(bytes) if sha256_digest(&bytes) == d => Ok(bytes),
            Ok(_) => Err(format!(
                "{pinned}: {what} {d}: {} is not what its digest says",
                path.display()
            )),
            Err(e) => Err(format!("{pinned}: {what} {d}: {}: {e}", path.display())),
        }
    };
    let parse = |what: &str, bytes: &[u8]| {
        serde_json::from_slice::<serde_json::Value>(bytes).map_err(|e| format!("{pinned}: {what}: {e}"))
    };
    let digest = pinned
        .digest
        .as_ref()
        .ok_or_else(|| format!("{pinned}: pinned by no digest"))?
        .to_string();
    let top = read("the document", &digest)?;
    let doc = parse("the document", &top)?;
    let manifests = match doc["manifests"].as_array() {
        None => vec![top.clone()],
        Some(listed) => {
            let arch = match ARCH {
                "aarch64" => "arm64",
                "x86_64" => "amd64",
                other => other,
            };
            let ours = listed
                .iter()
                .find(|m| m["platform"]["os"] == "linux" && m["platform"]["architecture"] == arch)
                .and_then(|m| m["digest"].as_str())
                .ok_or_else(|| format!("{pinned}: its index lists no linux/{arch} manifest"))?;
            let mut held = vec![read(&format!("its linux/{arch} manifest"), ours)?];
            for attestation in listed
                .iter()
                .filter(|m| m["annotations"]["vnd.docker.reference.digest"] == ours)
            {
                let d = attestation["digest"].as_str().unwrap_or_default();
                held.push(read("an attestation of it", d)?);
            }
            held
        }
    };
    let mut blobs = Vec::new();
    for manifest in manifests {
        let parsed = parse("a manifest", &manifest)?;
        let layers = parsed["layers"].as_array().map(Vec::as_slice).unwrap_or_default();
        for part in std::iter::once(&parsed["config"]).chain(layers) {
            blobs.push(read(
                "a manifest's part",
                part["digest"].as_str().unwrap_or_default(),
            )?);
        }
        if doc["manifests"].is_array() {
            blobs.push(manifest);
        }
    }
    Ok((top, blobs))
}

/// A running `shards` whose output lines arrive on a channel, killed when dropped.
pub struct Vm {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
    pub seen: Vec<String>,
    /// Every line it has printed, read or not: what a test that stalls says.
    log: Log,
}

/// Lines a process printed, as they came.
pub type Log = Arc<std::sync::Mutex<Vec<String>>>;

/// Runs `f`, failing with what `vm` said where it does not end within `within`, nor its
/// guest progress for [`DAEMON_QUIET`] past it: a line more from it, or its VM process's
/// CPU time ([`Heard::running`]). A test that stalls says where, and what its VM said,
/// rather than holding the whole binary; one its host runs slowly is waited on as it runs.
pub fn within<T: Send + 'static>(
    what: &str,
    within: Duration,
    vm: &Vm,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    let start = Instant::now();
    let lines = || vm.log.lock().map_or(0, |l| l.len());
    let mut heard = Heard::running(vm.id());
    let mut said = lines();
    loop {
        let now = lines();
        heard.said(now != said);
        said = now;
        // Asked each pass, so that each slice of its VM process's is judged as it comes.
        let progressing = heard.lately(true);
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(v) => return v,
            // Ended without sending: it panicked, and says why, after what the VM said.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let said = vm.log.lock().map(|l| l.join("\n")).unwrap_or_default();
                let _ = writeln!(std::io::stderr(), "{what}: failed; the VM said:\n{said}");
                match worker.join() {
                    Err(panic) => std::panic::resume_unwind(panic),
                    Ok(()) => panic!("{what}: ended with no result"),
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if start.elapsed() < within || progressing => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let said = vm.log.lock().map(|l| l.join("\n")).unwrap_or_default();
                panic!(
                    "{what}: not done in {within:?}, nor its guest progressed for {DAEMON_QUIET:?} after; the VM said:\n{said}"
                )
            }
        }
    }
}

impl Vm {
    pub fn spawn<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> Vm {
        Vm::spawn_env(args, &[])
    }

    /// [`Vm::spawn`], with `env` added to shards' environment.
    pub fn spawn_env<S: AsRef<std::ffi::OsStr>>(args: &[S], env: &[(&str, &str)]) -> Vm {
        let mut child = command()
            .envs(env.iter().copied())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        let log = Log::default();
        let (out, err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        for stream in [Box::new(out) as Box<dyn Read + Send>, Box::new(err)] {
            let (tx, log) = (tx.clone(), log.clone());
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    if let Ok(mut log) = log.lock() {
                        log.push(line.clone());
                    }
                    let _ = tx.send(line);
                }
            });
        }
        Vm {
            child,
            lines,
            seen: Vec::new(),
            log,
        }
    }

    /// Every line it has printed so far, and those to come.
    pub fn log(&self) -> Log {
        self.log.clone()
    }

    /// The `shards` process's ID.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Kills the `shards` process, and waits for it to go.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Waits for an output line containing `text` ([`wait_for_any`](Self::wait_for_any)).
    pub fn wait_for(&mut self, text: &str, timeout: Duration) {
        self.wait_for_any(&[text], timeout);
    }

    /// Waits for an output line containing any of `texts`, and returns it: within
    /// `timeout`, or past it while its guest says something new or its VM process runs,
    /// within [`DAEMON_QUIET`] ([`Heard::running`]), for a guest of fewer vCPUs than its
    /// host has CPUs. One that does neither for as long past it fails, with what it said.
    /// A guest of more vCPUs than its host's CPUs is waited on by
    /// [`wait_for_any_within`](Self::wait_for_any_within), whose CPU time counts only as it
    /// starts.
    pub fn wait_for_any(&mut self, texts: &[&str], timeout: Duration) -> String {
        let heard = Heard::running(self.child.id());
        self.wait_heard(texts, timeout, heard)
    }

    /// [`wait_for_any`](Self::wait_for_any), its guest's progress what [`Heard::of`] takes
    /// for it: a line it has not said before, or more of `streamed`'s stream, within
    /// [`DAEMON_QUIET`], and its VM process's CPU time only before its first line; for a
    /// guest of more vCPUs than its host has CPUs, which show CPU time for ever. One that
    /// does neither for as long past `timeout` fails as before.
    pub fn wait_for_any_within(
        &mut self,
        texts: &[&str],
        timeout: Duration,
        streamed: Option<&AtomicU64>,
    ) -> String {
        let heard = Heard::of(self.child.id(), streamed);
        self.wait_heard(texts, timeout, heard)
    }

    /// Waits for a line containing any of `texts` within `timeout`, or past it while
    /// `heard` takes its guest to progress.
    fn wait_heard(&mut self, texts: &[&str], timeout: Duration, mut heard: Heard<'_>) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            // Asked each pass, so that each slice of its microVM's is judged as it comes.
            let progressing = heard.lately(!self.seen.is_empty());
            match self.lines.recv_timeout(Duration::from_secs(1)) {
                Ok(line) => {
                    let found = texts.iter().any(|t| line.contains(t));
                    self.hear(line.clone(), &mut heard);
                    if found {
                        return line;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    if Instant::now() < deadline || progressing => {}
                Err(_) => panic!(
                    "no {texts:?} from the VM in {timeout:?}, nor progress of its guest's for {DAEMON_QUIET:?} after; it said:\n{}",
                    self.seen.join("\n")
                ),
            }
        }
    }

    /// Keeps a line it printed, which [`Heard`] takes for progress where it is new.
    fn hear(&mut self, line: String, heard: &mut Heard<'_>) {
        heard.said(!self.seen.contains(&line));
        self.seen.push(line);
    }

    /// Waits for the process to exit, keeping everything it printed.
    pub fn wait_exit(&mut self) -> Option<i32> {
        let code = self.child.wait().unwrap().code();
        // Its pipes are closed now, so the readers end and the channel with them.
        self.seen.extend(self.lines.iter());
        code
    }

    /// [`wait_exit`](Self::wait_exit), failing, with what it printed, if the process runs
    /// past `timeout` and its guest has said nothing new and its VM process run nothing
    /// for [`DAEMON_QUIET`] ([`Heard::running`]): for a guest of fewer vCPUs than its host
    /// has CPUs, as [`wait_for_any`](Self::wait_for_any) waits on one.
    pub fn wait_exit_following(&mut self, timeout: Duration) -> Option<i32> {
        let heard = Heard::running(self.child.id());
        self.wait_exit_heard(timeout, heard)
    }

    /// [`wait_exit`](Self::wait_exit), failing, with what it printed, if the process runs
    /// past `timeout` and its guest has said nothing new, nor carried more of `streamed`'s
    /// stream, for [`DAEMON_QUIET`] ([`Heard::of`]): a guest whose workload its host runs
    /// slower than `timeout` allows (the parity plan's step 10, a 64-vCPU storm on 4 TCG
    /// CPUs) is waited on while it gets on with it, and one that does not fails as before.
    pub fn wait_exit_within(&mut self, timeout: Duration, streamed: Option<&AtomicU64>) -> Option<i32> {
        let heard = Heard::of(self.child.id(), streamed);
        self.wait_exit_heard(timeout, heard)
    }

    /// Waits for the process to exit within `timeout`, or past it while `heard` takes its
    /// guest to progress.
    fn wait_exit_heard(&mut self, timeout: Duration, mut heard: Heard<'_>) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        while self.child.try_wait().unwrap().is_none() {
            // Asked each pass, so that each slice of its microVM's and each byte of its
            // stream is judged as it comes.
            let progressing = heard.lately(!self.seen.is_empty());
            if Instant::now() > deadline && !progressing {
                let _ = self.child.kill();
                self.wait_exit();
                panic!(
                    "still running after {timeout:?}, and no progress of its guest's for {DAEMON_QUIET:?} after; it said:\n{}",
                    self.seen.join("\n")
                );
            }
            match self.lines.recv_timeout(Duration::from_millis(5)) {
                Ok(line) => self.hear(line, &mut heard),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                // Its pipes closed as it goes: it is waited for alone.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(Duration::from_millis(5))
                }
            }
        }
        self.wait_exit()
    }
}

/// What a wait on a guest past its deadline takes for its progress (D209): a line it has
/// not printed before, or bytes more of a stream a test carries through it (`streamed`, as
/// [`Following`] counts them); and its microVM's CPU time ([`Ran`]), throughout
/// ([`Heard::running`]), or only before its first line, while it starts ([`Heard::of`]),
/// or never, for a machine of this process's own ([`Heard::console`]). CPU time is
/// progress throughout for a guest of fewer vCPUs than its host has CPUs, whose idle vCPUs
/// take less than an idle microVM does ([`shards_vmm::platform::IDLE_VM_CPU`]): a guest
/// onlining a gigabyte it plugged says nothing as it does, past the test's minute on an
/// arm64 Windows host nested under QEMU's TCG (the parity plan's step 10). It is no
/// progress once a guest of more vCPUs than that has its own words: a 64-vCPU guest on 4
/// TCG CPUs shows it for ever, its stream stopped or not (step 10: a storm waited on for
/// 2 h 16 min, its VM process at 31,324 CPU-seconds), and a guest that repeats a line says
/// nothing new.
pub struct Heard<'a> {
    /// Its VM processes, where their CPU time counts.
    ran: Option<Ran>,
    /// Whether their CPU time counts past its guest's first line.
    throughout: bool,
    streamed: Option<&'a AtomicU64>,
    bytes: u64,
    /// When it last progressed, since this began watching.
    at: Option<Instant>,
}

impl<'a> Heard<'a> {
    /// The guest of the VM process `vm` is or started, its CPU time counted only before
    /// its first line.
    fn of(vm: u32, streamed: Option<&'a AtomicU64>) -> Heard<'a> {
        Heard {
            ran: Some(Ran::of(vm)),
            throughout: false,
            bytes: streamed.map_or(0, |s| s.load(Ordering::Relaxed)),
            streamed,
            at: None,
        }
    }

    /// The guest of the VM process `vm` is or started (`run --kernel`, `restore`), its CPU
    /// time counted throughout: for a guest of fewer vCPUs than its host has CPUs.
    pub fn running(vm: u32) -> Heard<'static> {
        Heard {
            ran: Some(Ran::of(vm)),
            throughout: true,
            streamed: None,
            bytes: 0,
            at: None,
        }
    }

    /// The guest of a machine this process runs (`vm::start`), by what it says alone: its
    /// vCPUs are threads of this process, whose CPU time is its other tests' too, and its
    /// own polling's.
    pub fn console() -> Heard<'static> {
        Heard {
            ran: None,
            throughout: false,
            streamed: None,
            bytes: 0,
            at: None,
        }
    }

    /// Something it said, `new` where it had not said it before.
    pub fn said(&mut self, new: bool) {
        if new {
            self.at = Some(Instant::now());
        }
    }

    /// Whether it progressed within [`DAEMON_QUIET`]; `started`, whether it has printed a
    /// line, past which its CPU time counts only where it counts throughout.
    pub fn lately(&mut self, started: bool) -> bool {
        if let Some(s) = self.streamed {
            let bytes = s.load(Ordering::Relaxed);
            if bytes != self.bytes {
                self.bytes = bytes;
                self.at = Some(Instant::now());
            }
        }
        let ran = (self.throughout || !started) && self.ran.as_mut().is_some_and(Ran::lately);
        ran || self.at.is_some_and(|at| at.elapsed() < DAEMON_QUIET)
    }
}

/// How a machine this process runs (`vm::start`) ends, as `ended` says (a thread's
/// `running.wait()`): within `timeout`, or past it while its guest writes something new to
/// its console within [`DAEMON_QUIET`] ([`Heard::console`]); `None` where it does neither,
/// its machine then stopped.
pub fn machine_ended(
    handle: &shards_vmm::vm::Handle,
    ended: &std::sync::mpsc::Receiver<shards_vmm::vm::ExitReason>,
    timeout: Duration,
) -> Option<shards_vmm::vm::ExitReason> {
    let deadline = Instant::now() + timeout;
    let mut heard = Heard::console();
    let mut console = handle.console_said();
    loop {
        let now = handle.console_said();
        heard.said(now != console);
        console = now;
        let progressing = heard.lately(true);
        match ended.recv_timeout(Duration::from_millis(100)) {
            Ok(reason) => return Some(reason),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline || progressing => {}
            Err(_) => {
                handle.stop();
                return None;
            }
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Opens a stream to guest vsock port `port` through the VM's socket `sock`: `Ok` once
/// the guest accepted (the `OK` line was read), `Err` with what came back otherwise.
pub fn vsock_connect(
    sock: &Path,
    port: u32,
    timeout: Duration,
) -> Result<shards_vmm::platform::UnixStream, String> {
    let s =
        shards_vmm::platform::UnixStream::connect(sock).map_err(|e| format!("{}: {e}", sock.display()))?;
    s.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    vsock_handshake(&s, &mut &s, port)?;
    Ok(s)
}

/// Asks the VM's socket `s` for guest port `port`, reading its answer through `r`.
fn vsock_handshake(
    mut s: &shards_vmm::platform::UnixStream,
    r: &mut impl Read,
    port: u32,
) -> Result<(), String> {
    s.write_all(format!("CONNECT {port}\n").as_bytes())
        .map_err(|e| format!("CONNECT: {e}"))?;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.last() != Some(&b'\n') {
        match r.read(&mut byte) {
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
    Ok(())
}

/// A guest's stream, read for as long as it carries bytes: each read waits up to
/// [`DAEMON_QUIET`] for one, so that a stream that carries anything goes on however slowly
/// its host runs its guest (the parity plan's step 10: a 64-vCPU storm's echo on 4 TCG
/// CPUs, 196,608 bytes in 120 s), and one that carries nothing for as long fails, with the
/// stream's own timeout error, whatever its microVM's CPU does (a guest on more vCPUs than
/// its host's CPUs runs them all for ever). What it reads is added to `carried`, where a
/// wait on its guest follows it ([`Vm::wait_exit_within`]).
pub struct Following<'a> {
    stream: &'a shards_vmm::platform::UnixStream,
    carried: Option<&'a AtomicU64>,
}

impl<'a> Following<'a> {
    /// Reads of `stream`, which reads with [`DAEMON_QUIET`]'s timeout from now on.
    pub fn of(
        stream: &'a shards_vmm::platform::UnixStream,
        carried: Option<&'a AtomicU64>,
    ) -> io::Result<Following<'a>> {
        stream.set_read_timeout(Some(DAEMON_QUIET))?;
        Ok(Following { stream, carried })
    }
}

impl Read for Following<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.stream.read(buf) {
                Ok(n) => {
                    if let Some(c) = self.carried {
                        c.fetch_add(n as u64, Ordering::Relaxed);
                    }
                    return Ok(n);
                }
                // A timeout says WouldBlock through SO_RCVTIMEO, TimedOut on Windows.
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    return Err(io::Error::new(
                        e.kind(),
                        format!("{e}: nothing for {DAEMON_QUIET:?}"),
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

/// Streams `len` bytes of pattern `salt` through the guest's echo on `port` and checks
/// what comes back, byte for byte, after the half-close: each read waits a minute for its
/// next byte ([`Following`]), and what comes back is added to `carried`.
pub fn echo(
    sock: &Path,
    port: u32,
    salt: u64,
    len: usize,
    carried: Option<&AtomicU64>,
) -> Result<(), String> {
    let s =
        shards_vmm::platform::UnixStream::connect(sock).map_err(|e| format!("{}: {e}", sock.display()))?;
    let mut r = Following::of(&s, carried).map_err(|e| e.to_string())?;
    vsock_handshake(&s, &mut r, port)?;
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

/// What a fake S3 holds: each object by its path (`/BUCKET/KEY`, path-style, decoded), with
/// when it was last written, and each request.
#[derive(Debug, Default)]
pub struct Buckets {
    pub objects: std::collections::BTreeMap<String, (Vec<u8>, std::time::SystemTime)>,
    /// Each request: its method and path, `copy` after a copy, and the access key that
    /// signed it.
    pub log: Vec<String>,
}

/// An S3 on loopback, path-style, as far as the S3 cache backend asks one: PutObject (its
/// body checked against `x-amz-content-sha256` where signed), CopyObject onto a key,
/// GetObject and HeadObject, with S3's `NoSuchKey`. A request not signed with SigV4
/// is refused, as S3's `AccessDenied`.
pub fn fake_s3() -> (u16, Arc<std::sync::Mutex<Buckets>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let buckets = Arc::new(std::sync::Mutex::new(Buckets::default()));
    let held = buckets.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let buckets = held.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut out = stream;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut headers = std::collections::HashMap::new();
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).unwrap_or(0) <= 2 {
                            break;
                        }
                        let (name, value) = header.split_once(':').unwrap_or_default();
                        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                    }
                    let length: usize = headers
                        .get("content-length")
                        .and_then(|l| l.parse().ok())
                        .unwrap_or(0);
                    let mut body = vec![0u8; length];
                    reader.read_exact(&mut body).unwrap();
                    let mut parts = line.split(' ');
                    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                    let path = percent_decode(target.split('?').next().unwrap_or(""));
                    let signer = headers
                        .get("authorization")
                        .and_then(|a| a.strip_prefix("AWS4-HMAC-SHA256 Credential="))
                        .and_then(|c| c.split('/').next())
                        .unwrap_or("")
                        .to_string();
                    let payload = headers.get("x-amz-content-sha256").cloned().unwrap_or_default();
                    let error = |status: &str, code: &str| {
                        (
                            status.to_string(),
                            vec![],
                            format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>{code}</Code><Message>{code}</Message></Error>")
                                .into_bytes(),
                        )
                    };
                    let mut buckets = buckets.lock().unwrap();
                    let copy = headers
                        .get("x-amz-copy-source")
                        .map(|s| format!("/{}", percent_decode(s)));
                    buckets.log.push(format!(
                        "{method} {path}{} {signer}",
                        if copy.is_some() { " copy" } else { "" }
                    ));
                    let (status, extra, reply): (String, Vec<String>, Vec<u8>) = if signer.is_empty() {
                        error("403 Forbidden", "AccessDenied")
                    } else if method == "PUT" {
                        match copy {
                            Some(from) => match buckets.objects.get(&from).cloned() {
                                Some((b, _)) => {
                                    buckets.objects.insert(path, (b, std::time::SystemTime::now()));
                                    (
                                        "200 OK".into(),
                                        vec![],
                                        b"<CopyObjectResult><ETag>\"e\"</ETag></CopyObjectResult>".to_vec(),
                                    )
                                }
                                None => error("404 Not Found", "NoSuchKey"),
                            },
                            None if payload != "UNSIGNED-PAYLOAD"
                                && payload != sha256_digest(&body).trim_start_matches("sha256:") =>
                            {
                                error("400 Bad Request", "XAmzContentSHA256Mismatch")
                            }
                            None => {
                                buckets.objects.insert(path, (body, std::time::SystemTime::now()));
                                ("200 OK".into(), vec!["ETag: \"e\"".into()], vec![])
                            }
                        }
                    } else {
                        match buckets.objects.get(&path) {
                            Some((b, at)) => (
                                "200 OK".into(),
                                vec![format!("Last-Modified: {}", http_date(*at))],
                                if method == "HEAD" {
                                    b.len().to_string().into_bytes()
                                } else {
                                    b.clone()
                                },
                            ),
                            None if method == "HEAD" => ("404 Not Found".into(), vec![], vec![]),
                            None => error("404 Not Found", "NoSuchKey"),
                        }
                    };
                    drop(buckets);
                    let mut head = format!("HTTP/1.1 {status}\r\n");
                    for h in extra {
                        head.push_str(&format!("{h}\r\n"));
                    }
                    // A HEAD's length is the object's; it has no body.
                    let (length, reply) = if method == "HEAD" {
                        (String::from_utf8(reply).unwrap_or_default(), vec![])
                    } else {
                        (reply.len().to_string(), reply)
                    };
                    head.push_str(&format!(
                        "Content-Length: {}\r\n\r\n",
                        if length.is_empty() { "0" } else { &length }
                    ));
                    if out
                        .write_all(head.as_bytes())
                        .and_then(|()| out.write_all(&reply))
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    (port, buckets)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if let Some(v) = s
            .get(i + 1..i + 3)
            .filter(|_| b[i] == b'%')
            .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

/// `at` as HTTP dates it (IMF-fixdate, RFC 9110 §5.6.7).
fn http_date(at: std::time::SystemTime) -> String {
    let secs = at.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let t = time::OffsetDateTime::from_unix_timestamp(secs).unwrap();
    let day =
        ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][t.weekday().number_days_from_monday() as usize];
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][u8::from(t.month()) as usize - 1];
    format!(
        "{day}, {:02} {month} {} {:02}:{:02}:{:02} GMT",
        t.day(),
        t.year(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// What a fake Blob Storage holds: containers, each blob by its path (`/ACCOUNT/CONTAINER/
/// NAME`, the name decoded), and each request.
#[derive(Debug, Default)]
pub struct Blobs {
    pub containers: std::collections::BTreeSet<String>,
    pub blobs: std::collections::BTreeMap<String, Vec<u8>>,
    /// Blocks staged, by blob and block ID.
    pub blocks: std::collections::BTreeMap<(String, String), Vec<u8>>,
    /// Each request: its method, its path and `comp=` if any, and whether it was signed
    /// with a shared key.
    pub log: Vec<String>,
}

/// Blob Storage on loopback, as far as the azblob cache backend asks it: a container's
/// properties and its making, a blob's properties, download and upload (`If-None-Match:
/// *` kept), and blocks staged and committed. A request not signed with a shared key is
/// refused, as Blob Storage's `NoAuthenticationInformation`.
pub fn fake_azblob() -> (u16, Arc<std::sync::Mutex<Blobs>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let held = Arc::new(std::sync::Mutex::new(Blobs::default()));
    let shared = held.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let blobs = shared.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut out = stream;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut headers = std::collections::HashMap::new();
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).unwrap_or(0) <= 2 {
                            break;
                        }
                        let (name, value) = header.split_once(':').unwrap_or_default();
                        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                    }
                    let length: usize = headers
                        .get("content-length")
                        .and_then(|l| l.parse().ok())
                        .unwrap_or(0);
                    let mut body = vec![0u8; length];
                    reader.read_exact(&mut body).unwrap();
                    let mut parts = line.split(' ');
                    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                    let (raw, query) = target.split_once('?').unwrap_or((target, ""));
                    let path = percent_decode(raw);
                    let param = |k: &str| {
                        query
                            .split('&')
                            .find_map(|p| p.strip_prefix(&format!("{k}=")))
                            .map(percent_decode)
                    };
                    let signed = headers
                        .get("authorization")
                        .is_some_and(|a| a.starts_with("SharedKey devstoreaccount1:"));
                    let mut blobs = blobs.lock().unwrap();
                    blobs.log.push(format!(
                        "{method} {path}{}{}",
                        param("comp").map(|c| format!(" {c}")).unwrap_or_default(),
                        if signed { "" } else { " unsigned" }
                    ));
                    let error = |status: &str, code: &str| {
                        (
                            status.to_string(),
                            vec![format!("x-ms-error-code: {code}")],
                            Vec::new(),
                        )
                    };
                    let absent = headers.get("if-none-match").is_some_and(|v| v == "*");
                    let (status, extra, reply): (String, Vec<String>, Vec<u8>) = if !signed {
                        error("403 Forbidden", "NoAuthenticationInformation")
                    } else if param("restype").as_deref() == Some("container") {
                        if method == "PUT" {
                            if blobs.containers.insert(path.clone()) {
                                ("201 Created".into(), vec![], vec![])
                            } else {
                                error("409 Conflict", "ContainerAlreadyExists")
                            }
                        } else if blobs.containers.contains(&path) {
                            ("200 OK".into(), vec![], vec![])
                        } else {
                            error("404 Not Found", "ContainerNotFound")
                        }
                    } else if param("comp").as_deref() == Some("block") {
                        let id = param("blockid").unwrap_or_default();
                        blobs.blocks.insert((path, id), body);
                        ("201 Created".into(), vec![], vec![])
                    } else if param("comp").as_deref() == Some("blocklist") {
                        if absent && blobs.blobs.contains_key(&path) {
                            error("409 Conflict", "BlobAlreadyExists")
                        } else {
                            let list = String::from_utf8(body).unwrap();
                            let mut whole = Vec::new();
                            for id in list.split("<Latest>").skip(1) {
                                let id = id.split("</Latest>").next().unwrap().to_string();
                                whole.extend(blobs.blocks[&(path.clone(), id)].clone());
                            }
                            blobs.blobs.insert(path, whole);
                            ("201 Created".into(), vec![], vec![])
                        }
                    } else if method == "PUT" {
                        if absent && blobs.blobs.contains_key(&path) {
                            error("409 Conflict", "BlobAlreadyExists")
                        } else {
                            blobs.blobs.insert(path, body);
                            ("201 Created".into(), vec![], vec![])
                        }
                    } else {
                        match blobs.blobs.get(&path) {
                            Some(b) if method == "HEAD" => {
                                ("200 OK".into(), vec![], b.len().to_string().into_bytes())
                            }
                            Some(b) => ("200 OK".into(), vec![], b.clone()),
                            None => error("404 Not Found", "BlobNotFound"),
                        }
                    };
                    drop(blobs);
                    let mut head = format!("HTTP/1.1 {status}\r\n");
                    for h in extra {
                        head.push_str(&format!("{h}\r\n"));
                    }
                    let (length, reply) = if method == "HEAD" {
                        (String::from_utf8(reply).unwrap_or_default(), vec![])
                    } else {
                        (reply.len().to_string(), reply)
                    };
                    let length = if length.is_empty() {
                        "0".to_string()
                    } else {
                        length
                    };
                    head.push_str(&format!("Content-Length: {length}\r\n\r\n"));
                    if out
                        .write_all(head.as_bytes())
                        .and_then(|()| out.write_all(&reply))
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    (port, held)
}

/// What a fake GitHub Actions cache holds: each committed entry by key, and each request
/// (method, path, header names and, for the service's own calls, the JSON body).
#[derive(Debug, Default)]
pub struct Actions {
    pub entries: std::collections::BTreeMap<String, Vec<u8>>,
    reserved: std::collections::BTreeSet<String>,
    staged: std::collections::BTreeMap<String, Vec<u8>>,
    ids: Vec<String>,
    pub log: Vec<(String, String, Vec<String>, String)>,
}

/// GitHub's cache service on loopback, as go-actions-cache asks it: v2 (twirp, entries
/// uploaded to and downloaded from signed blob URLs here, single or in blocks) and the
/// legacy v1 (`_apis/artifactcache`, chunks by `Content-Range`). A key is taken once; a
/// lookup answers the newest key beginning with the first restore key any begins with.
/// The service's calls without `Authorization: Bearer TOKEN` are refused.
pub fn fake_gha(token: String) -> (u16, Arc<std::sync::Mutex<Actions>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let held = Arc::new(std::sync::Mutex::new(Actions::default()));
    let shared = held.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let actions = shared.clone();
            let token = token.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut out = stream;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut headers = std::collections::BTreeMap::new();
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).unwrap_or(0) <= 2 {
                            break;
                        }
                        let (name, value) = header.split_once(':').unwrap_or_default();
                        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                    }
                    let length: usize = headers
                        .get("content-length")
                        .and_then(|l| l.parse().ok())
                        .unwrap_or(0);
                    let mut body = vec![0u8; length];
                    reader.read_exact(&mut body).unwrap();
                    let mut parts = line.split(' ');
                    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                    let (raw, query) = target.split_once('?').unwrap_or((target, ""));
                    let path = percent_decode(raw);
                    let param = |k: &str| {
                        query
                            .split('&')
                            .find_map(|p| p.strip_prefix(&format!("{k}=")))
                            .map(percent_decode)
                    };
                    let service = path.starts_with("/twirp/") || path.starts_with("/_apis/");
                    let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                    let mut a = actions.lock().unwrap();
                    a.log.push((
                        method.to_string(),
                        path.clone(),
                        headers.keys().cloned().collect(),
                        if service {
                            String::from_utf8_lossy(&body).into_owned()
                        } else {
                            String::new()
                        },
                    ));
                    let ok = |v: serde_json::Value| ("200 OK".to_string(), serde_json::to_vec(&v).unwrap());
                    let newest = |a: &Actions, keys: &[String]| {
                        keys.iter().find_map(|p| {
                            a.entries
                                .keys()
                                .filter(|k| k.starts_with(p.as_str()))
                                .max()
                                .cloned()
                        })
                    };
                    let base = format!("http://127.0.0.1:{port}");
                    let escaped = |k: &str| k.replace('%', "%25").replace('#', "%23");
                    let (status, reply): (String, Vec<u8>) = if service
                        && headers.get("authorization").map(String::as_str)
                            != Some(&format!("Bearer {token}"))
                    {
                        (
                            "401 Unauthorized".into(),
                            br#"{"code":"unauthenticated","msg":"no token"}"#.to_vec(),
                        )
                    } else if path
                        == "/twirp/github.actions.results.api.v1.CacheService/GetCacheEntryDownloadURL"
                    {
                        let keys: Vec<String> =
                            serde_json::from_value(json["restore_keys"].clone()).unwrap_or_default();
                        match newest(&a, &keys) {
                            Some(k) => ok(
                                serde_json::json!({"ok": true, "matched_key": k, "signed_download_url": format!("{base}/blob/{}?sig=read", escaped(&k))}),
                            ),
                            None => ok(serde_json::json!({"ok": false})),
                        }
                    } else if path == "/twirp/github.actions.results.api.v1.CacheService/CreateCacheEntry" {
                        let key = json["key"].as_str().unwrap_or_default().to_string();
                        if a.reserved.insert(key.clone()) {
                            ok(
                                serde_json::json!({"ok": true, "signed_upload_url": format!("{base}/blob/{}?sig=write", escaped(&key))}),
                            )
                        } else {
                            ("409 Conflict".into(), br#"{"code":"already_exists","msg":"cache entry with the same key, version, and scope already exists"}"#.to_vec())
                        }
                    } else if path
                        == "/twirp/github.actions.results.api.v1.CacheService/FinalizeCacheEntryUpload"
                    {
                        let key = json["key"].as_str().unwrap_or_default().to_string();
                        let b = a.staged.remove(&key).unwrap_or_default();
                        assert_eq!(b.len() as u64, json["size_bytes"].as_u64().unwrap(), "{key}");
                        a.entries.insert(key, b);
                        ok(serde_json::json!({"ok": true, "entry_id": "1"}))
                    } else if path == "/_apis/artifactcache/cache" {
                        let keys: Vec<String> = param("keys")
                            .unwrap_or_default()
                            .split(',')
                            .map(str::to_string)
                            .collect();
                        match newest(&a, &keys) {
                            Some(k) => ok(
                                serde_json::json!({"cacheKey": k, "scope": "refs/heads/main", "archiveLocation": format!("{base}/blob/{}", escaped(&k))}),
                            ),
                            None => ("204 No Content".into(), vec![]),
                        }
                    } else if path == "/_apis/artifactcache/caches" {
                        let key = json["key"].as_str().unwrap_or_default().to_string();
                        if a.reserved.insert(key.clone()) {
                            a.ids.push(key);
                            ok(serde_json::json!({"cacheId": a.ids.len()}))
                        } else {
                            ("409 Conflict".into(), br#"{"message":"Cache already exists.","typeName":"x","typeKey":"ArtifactCacheItemAlreadyExistsException","errorCode":0}"#.to_vec())
                        }
                    } else if let Some(n) = path.strip_prefix("/_apis/artifactcache/caches/") {
                        let key = a.ids[n.parse::<usize>().unwrap() - 1].clone();
                        if method == "PATCH" {
                            let range = headers["content-range"]
                                .trim_start_matches("bytes ")
                                .trim_end_matches("/*")
                                .to_string();
                            let (from, to) = range.split_once('-').unwrap();
                            let (from, to): (usize, usize) = (from.parse().unwrap(), to.parse().unwrap());
                            let b = a.staged.entry(key).or_default();
                            if b.len() < to + 1 {
                                b.resize(to + 1, 0);
                            }
                            b[from..=to].copy_from_slice(&body);
                        } else {
                            let b = a.staged.remove(&key).unwrap_or_default();
                            assert_eq!(b.len() as u64, json["size"].as_u64().unwrap(), "{key}");
                            a.entries.insert(key, b);
                        }
                        ("204 No Content".into(), vec![])
                    } else if let Some(key) = path.strip_prefix("/blob/") {
                        let key = key.to_string();
                        match (method, param("comp").as_deref()) {
                            ("PUT", Some("block")) => {
                                let id = param("blockid").unwrap_or_default();
                                a.staged.insert(format!("{key}\u{0}{id}"), body);
                                ("201 Created".into(), vec![])
                            }
                            ("PUT", Some("blocklist")) => {
                                let list = String::from_utf8(body).unwrap();
                                let mut whole = Vec::new();
                                for id in list.split("<Latest>").skip(1) {
                                    let id = id.split("</Latest>").next().unwrap();
                                    whole.extend(a.staged.remove(&format!("{key}\u{0}{id}")).unwrap());
                                }
                                a.staged.insert(key, whole);
                                ("201 Created".into(), vec![])
                            }
                            ("PUT", _) => {
                                a.staged.insert(key, body);
                                ("201 Created".into(), vec![])
                            }
                            _ => match a.entries.get(&key) {
                                Some(b) => ("200 OK".into(), b.clone()),
                                None => ("404 Not Found".into(), vec![]),
                            },
                        }
                    } else {
                        ("404 Not Found".into(), vec![])
                    };
                    drop(a);
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        reply.len()
                    );
                    if out
                        .write_all(head.as_bytes())
                        .and_then(|()| out.write_all(&reply))
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    (port, held)
}

/// A runtime token as the cache service's: its scopes (`ac`), in force from 2020 to 2100,
/// its signature the service's own to check.
pub fn actions_token(scopes: &str) -> String {
    use base64::Engine as _;
    let enc = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let claims = serde_json::json!({"ac": scopes, "exp": 4102444800i64, "nbf": 1577836800i64});
    format!(
        "{}.{}.{}",
        enc(br#"{"alg":"HS256","typ":"JWT"}"#),
        enc(&serde_json::to_vec(&claims).unwrap()),
        enc(b"signature")
    )
}

/// What a writable test registry holds, by repository: blobs by digest, and manifests by
/// tag and by digest, each with its media type.
#[derive(Debug, Default)]
pub struct Repos {
    pub blobs: std::collections::HashMap<String, std::collections::HashMap<String, Vec<u8>>>,
    pub manifests: std::collections::HashMap<String, std::collections::HashMap<String, (String, Vec<u8>)>>,
    /// Each request's method and path, in order.
    pub log: Vec<String>,
    /// Whether it serves the referrers API as zot does (D116): a manifest with a subject
    /// put is answered `OCI-Subject`, and `referrers/<digest>` lists those referring to
    /// it; else it has none, as distribution has none, and answers 404 there.
    pub referrers_api: bool,
}

/// The referrers of `subject` among `manifests`, as the referrers API lists them: each
/// manifest whose subject it is, its artifact type (else its config's type) and its
/// annotations.
fn referrers_of(manifests: &std::collections::HashMap<String, (String, Vec<u8>)>, subject: &str) -> Vec<u8> {
    let mut seen = std::collections::BTreeMap::new();
    for (reference, (kind, body)) in manifests {
        if !reference.starts_with("sha256:") {
            continue;
        }
        let Ok(m) = serde_json::from_slice::<serde_json::Value>(body) else {
            continue;
        };
        if m["subject"]["digest"].as_str() != Some(subject) {
            continue;
        }
        let artifact_type = m["artifactType"]
            .as_str()
            .or(m["config"]["mediaType"].as_str())
            .unwrap_or_default();
        seen.insert(
            reference.clone(),
            serde_json::json!({
                "mediaType": kind,
                "size": body.len(),
                "digest": reference,
                "artifactType": artifact_type,
                "annotations": m["annotations"].clone(),
            }),
        );
    }
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": seen.into_values().collect::<Vec<_>>(),
    }))
    .unwrap()
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
                    } else if path == "/v2/_catalog" {
                        // As distribution lists it: its repositories sorted, `n` a page
                        // after `last`, a `Link` to the next where more remain.
                        let mut names: Vec<String> = repos
                            .manifests
                            .keys()
                            .chain(repos.blobs.keys())
                            .cloned()
                            .collect();
                        names.sort();
                        names.dedup();
                        let last = param("last").map(|l| l.replace("%2F", "/")).unwrap_or_default();
                        let n: usize = param("n").and_then(|n| n.parse().ok()).unwrap_or(100);
                        let page: Vec<String> = names.into_iter().filter(|r| *r > last).collect();
                        let more = page.len() > n;
                        let page: Vec<String> = page.into_iter().take(n).collect();
                        let mut headers = vec!["Content-Type: application/json".to_string()];
                        if more && let Some(end) = page.last() {
                            headers.push(format!(
                                "Link: </v2/_catalog?last={}&n={n}>; rel=\"next\"",
                                end.replace('/', "%2F")
                            ));
                        }
                        (
                            "200 OK",
                            headers,
                            serde_json::to_vec(&serde_json::json!({ "repositories": page })).unwrap(),
                        )
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
                    } else if let Some((repo, subject)) =
                        rest.split_once("/referrers/").filter(|_| repos.referrers_api)
                    {
                        let listed =
                            referrers_of(repos.manifests.get(repo).unwrap_or(&Default::default()), subject);
                        (
                            "200 OK",
                            vec!["Content-Type: application/vnd.oci.image.index.v1+json".to_string()],
                            listed,
                        )
                    } else if let Some((repo, reference)) = rest.split_once("/manifests/") {
                        let repo = repo.to_string();
                        if method == "PUT" {
                            let d = sha256_digest(&body);
                            let subject = serde_json::from_slice::<serde_json::Value>(&body)
                                .ok()
                                .and_then(|m| m["subject"]["digest"].as_str().map(String::from))
                                .filter(|_| repos.referrers_api);
                            let m = repos.manifests.entry(repo).or_default();
                            m.insert(reference.to_string(), (kind.clone(), body.clone()));
                            m.insert(d.clone(), (kind.clone(), body));
                            let mut headers = vec![format!("Docker-Content-Digest: {d}")];
                            headers.extend(subject.map(|s| format!("OCI-Subject: {s}")));
                            ("201 Created", headers, vec![])
                        } else if method == "DELETE" {
                            // The manifest, by its digest, and every tag naming it.
                            let m = repos.manifests.entry(repo).or_default();
                            let before = m.len();
                            m.retain(|r, (_, b)| r != reference && sha256_digest(b) != reference);
                            if m.len() == before {
                                ("404 Not Found", vec![], vec![])
                            } else {
                                ("202 Accepted", vec![], vec![])
                            }
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

/// The test image behind an index of two platforms: this host's, and another
/// architecture's whose one layer holds `/arch`, its name. The index, its blobs, and that
/// layer's digest.
pub fn two_platform_image() -> (Vec<u8>, Vec<Vec<u8>>, String) {
    let (ours, mut blobs) = test_image();
    let (arch, other) = if cfg!(target_arch = "aarch64") {
        ("arm64", "amd64")
    } else {
        ("amd64", "arm64")
    };
    let layer = tar(&[("arch", 0o644, 0, Some(other.as_bytes()))]);
    let config = format!(
        r#"{{"architecture":"{other}","os":"linux","config":{{"Env":["PATH=/bin"]}},"rootfs":{{"type":"layers","diff_ids":["{}"]}}}}"#,
        sha256_digest(&layer)
    )
    .into_bytes();
    let theirs = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":{}}}]}}"#,
        sha256_digest(&config),
        config.len(),
        sha256_digest(&layer),
        layer.len()
    )
    .into_bytes();
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{}","size":{},"platform":{{"architecture":"{arch}","os":"linux"}}}},{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{}","size":{},"platform":{{"architecture":"{other}","os":"linux"}}}}]}}"#,
        sha256_digest(&ours),
        ours.len(),
        sha256_digest(&theirs),
        theirs.len(),
    )
    .into_bytes();
    let layer_digest = sha256_digest(&layer);
    blobs.push(ours);
    blobs.extend([config, layer, theirs]);
    (index, blobs, layer_digest)
}

/// An SSH agent of this test's own on Windows, at a named pipe, as Windows' OpenSSH serves
/// its own (`\\.\pipe\openssh-ssh-agent`), whose agent is a service of the machine's that no
/// test may take over: it holds one ed25519 key, which it lists and signs with
/// (draft-miller-ssh-agent: SSH_AGENTC_REQUEST_IDENTITIES, SSH_AGENTC_SIGN_REQUEST), and
/// refuses all else, counting each removal it is asked for. Served for the test's life.
#[cfg(windows)]
pub struct SshAgent {
    /// What `SSH_AUTH_SOCK` names it by: its pipe.
    pub pipe: String,
    /// The removals it was asked for, which it refused.
    pub removals: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// `bytes` as the agent protocol's string: its length, then itself.
#[cfg(windows)]
fn ssh_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_be_bytes());
    out.extend_from_slice(bytes);
}

/// An ed25519 public key as SSH writes its blob: its type's name, then its 32 bytes.
#[cfg(windows)]
fn ed25519_blob(public: &[u8]) -> Vec<u8> {
    let mut blob = Vec::new();
    ssh_string(&mut blob, b"ssh-ed25519");
    ssh_string(&mut blob, public);
    blob
}

/// Writes at `path` the ed25519 key of `seed`, with `comment`, as `ssh-keygen -N ""` writes
/// one: an unencrypted OpenSSH private key (openssh-key-v1, OpenSSH's PROTOCOL.key).
#[cfg(windows)]
pub fn ssh_key_file(path: &Path, seed: &[u8; 32], comment: &str) {
    use base64::Engine as _;
    let pair = aws_lc_rs::signature::Ed25519KeyPair::from_seed_unchecked(seed).unwrap();
    let public = aws_lc_rs::signature::KeyPair::public_key(&pair).as_ref().to_vec();
    let mut private = Vec::new();
    // Its check, any word written twice.
    let check = 0x5348_4152u32.to_be_bytes();
    private.extend_from_slice(&check);
    private.extend_from_slice(&check);
    ssh_string(&mut private, b"ssh-ed25519");
    ssh_string(&mut private, &public);
    let secret: Vec<u8> = seed.iter().chain(&public).copied().collect();
    ssh_string(&mut private, &secret);
    ssh_string(&mut private, comment.as_bytes());
    let mut pad = 1u8;
    while !private.len().is_multiple_of(8) {
        private.push(pad);
        pad += 1;
    }
    let mut body = b"openssh-key-v1\0".to_vec();
    ssh_string(&mut body, b"none");
    ssh_string(&mut body, b"none");
    ssh_string(&mut body, b"");
    body.extend_from_slice(&1u32.to_be_bytes());
    ssh_string(&mut body, &ed25519_blob(&public));
    ssh_string(&mut body, &private);
    let encoded = base64::engine::general_purpose::STANDARD.encode(&body);
    let mut text = String::from("-----BEGIN OPENSSH PRIVATE KEY-----\n");
    for line in encoded.as_bytes().chunks(70) {
        text.push_str(std::str::from_utf8(line).unwrap());
        text.push('\n');
    }
    text.push_str("-----END OPENSSH PRIVATE KEY-----\n");
    std::fs::write(path, text).unwrap();
}

/// An instance of the pipe `name`, waiting for its client; the first of its name, which no
/// other process may already hold, where `first`.
#[cfg(windows)]
fn agent_instance(name: &[u16], first: bool) -> std::fs::File {
    use std::os::windows::io::FromRawHandle as _;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    let flags = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
    // SAFETY: a NUL-terminated name and plain arguments; checked below.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            flags,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            1 << 16,
            1 << 16,
            0,
            std::ptr::null(),
        )
    };
    assert_ne!(
        handle,
        INVALID_HANDLE_VALUE,
        "{}",
        std::io::Error::last_os_error()
    );
    // SAFETY: the instance just made, owned from here on.
    unsafe { std::fs::File::from_raw_handle(handle) }
}

/// Starts [`SshAgent`]: the ed25519 key of `seed`, its comment `comment`.
#[cfg(windows)]
pub fn ssh_agent_windows(seed: [u8; 32], comment: &str) -> SshAgent {
    use std::os::windows::io::AsRawHandle as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use windows_sys::Win32::Foundation::ERROR_PIPE_CONNECTED;
    use windows_sys::Win32::System::Pipes::ConnectNamedPipe;
    static MADE: AtomicUsize = AtomicUsize::new(0);
    let pipe = format!(
        r"\\.\pipe\shards-test-agent-{}-{}",
        std::process::id(),
        MADE.fetch_add(1, Ordering::Relaxed)
    );
    let name: Vec<u16> = pipe.encode_utf16().chain([0]).collect();
    let removals = std::sync::Arc::new(AtomicUsize::new(0));
    // Its first instance made now: a client may connect as soon as this returns.
    let first = agent_instance(&name, true);
    let (counted, comment) = (removals.clone(), comment.to_string());
    std::thread::spawn(move || {
        let mut next = Some(first);
        loop {
            let conn = next.take().unwrap_or_else(|| agent_instance(&name, false));
            // SAFETY: an instance this thread holds, waited on to its client.
            let connected = unsafe { ConnectNamedPipe(conn.as_raw_handle(), std::ptr::null_mut()) } != 0
                || std::io::Error::last_os_error().raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32);
            if connected {
                let (counted, comment) = (counted.clone(), comment.clone());
                std::thread::spawn(move || serve_agent(conn, &seed, &comment, &counted));
            }
        }
    });
    SshAgent { pipe, removals }
}

/// Answers one client of [`SshAgent`] until it goes.
#[cfg(windows)]
fn serve_agent(
    mut conn: std::fs::File,
    seed: &[u8; 32],
    comment: &str,
    removals: &std::sync::atomic::AtomicUsize,
) {
    use std::io::{Read as _, Write as _};
    const FAILURE: u8 = 5;
    const IDENTITIES: u8 = 11;
    const IDENTITIES_ANSWER: u8 = 12;
    const SIGN: u8 = 13;
    const SIGN_RESPONSE: u8 = 14;
    const REMOVE: u8 = 18;
    let pair = aws_lc_rs::signature::Ed25519KeyPair::from_seed_unchecked(seed).unwrap();
    let public = aws_lc_rs::signature::KeyPair::public_key(&pair).as_ref().to_vec();
    let blob = ed25519_blob(&public);
    loop {
        let mut len = [0u8; 4];
        if conn.read_exact(&mut len).is_err() {
            return;
        }
        let mut message = vec![0u8; u32::from_be_bytes(len) as usize];
        if conn.read_exact(&mut message).is_err() {
            return;
        }
        let mut answer = Vec::new();
        match message.first() {
            Some(&IDENTITIES) => {
                answer.push(IDENTITIES_ANSWER);
                answer.extend_from_slice(&1u32.to_be_bytes());
                ssh_string(&mut answer, &blob);
                ssh_string(&mut answer, comment.as_bytes());
            }
            Some(&SIGN) => {
                // Its key's blob, then the data to sign, then its flags.
                let rest = &message[1..];
                let at = 4 + u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
                let asked = &rest[4..at];
                let n = u32::from_be_bytes(rest[at..at + 4].try_into().unwrap()) as usize;
                let data = &rest[at + 4..at + 4 + n];
                if asked == blob.as_slice() {
                    let mut signature = Vec::new();
                    ssh_string(&mut signature, b"ssh-ed25519");
                    ssh_string(&mut signature, pair.sign(data).as_ref());
                    answer.push(SIGN_RESPONSE);
                    ssh_string(&mut answer, &signature);
                } else {
                    answer.push(FAILURE);
                }
            }
            Some(&REMOVE) => {
                removals.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                answer.push(FAILURE);
            }
            _ => answer.push(FAILURE),
        }
        let mut framed = u32::try_from(answer.len()).unwrap().to_be_bytes().to_vec();
        framed.extend_from_slice(&answer);
        if conn.write_all(&framed).is_err() {
            return;
        }
    }
}

/// An SSH agent at `sock`, which goes as this test's process does, however it ends:
/// ssh-agent given a command runs it as its parent and exits once that has gone
/// (OpenSSH ssh-agent.c, check_parent_exists), the command a wait on this process. The
/// child is that command; killing it ends the agent too.
#[cfg(unix)]
pub fn ssh_agent(sock: &Path) -> std::process::Child {
    let me = std::process::id();
    std::process::Command::new("ssh-agent")
        .arg("-a")
        .arg(sock)
        .args([
            "sh",
            "-c",
            &format!("while kill -0 {me} 2>/dev/null; do sleep 1; done"),
        ])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

/// `mib` MiB of xorshift64's bytes at `path`, which no file system keeps as holes or shares.
pub fn noise(path: &Path, mib: usize) {
    use std::io::Write as _;
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut block = vec![0u8; 1 << 20];
    for _ in 0..mib {
        for word in block.as_chunks_mut::<8>().0 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *word = x.to_le_bytes();
        }
        file.write_all(&block).unwrap();
    }
    file.flush().unwrap();
}

/// The pipe a test tells its daemon its host is short of memory through
/// (`SHARDS_PRESSURE`, D188), each line a word: a FIFO on Unix, a named pipe on Windows,
/// there before the daemon opens it as it starts. One never said keeps a daemon from its
/// host's own word (engine_ring.rs's shrinking room). Each is its own, as many at once as a
/// test process's tests make: a named pipe of one instance, named by the process alone,
/// would refuse a second.
pub struct Told {
    pub path: std::ffi::OsString,
    #[cfg(windows)]
    server: std::fs::File,
}

impl Told {
    pub fn new(home: &Path) -> Told {
        #[cfg(unix)]
        {
            let path = home.join("pressure");
            let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: mkfifo(3) of a NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            Told {
                path: path.into_os_string(),
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::{FromRawHandle as _, OwnedHandle};
            use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
            use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
            use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_WAIT};
            let _ = home;
            static MADE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = MADE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let pipe = format!(r"\\.\pipe\shards-test-pressure-{}-{n}", std::process::id());
            let name: Vec<u16> = pipe.encode_utf16().chain(Some(0)).collect();
            // SAFETY: a NUL-terminated name and plain arguments.
            let server = unsafe {
                CreateNamedPipeW(
                    name.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_WAIT,
                    1,
                    4096,
                    4096,
                    0,
                    std::ptr::null(),
                )
            };
            assert_ne!(
                server,
                INVALID_HANDLE_VALUE,
                "{pipe}: {}",
                std::io::Error::last_os_error()
            );
            Told {
                path: pipe.into(),
                // SAFETY: the pipe just made, ours alone.
                server: std::fs::File::from(unsafe { OwnedHandle::from_raw_handle(server) }),
            }
        }
    }

    /// Its host's word that it is short of memory, once.
    pub fn say(&mut self) {
        #[cfg(unix)]
        std::fs::OpenOptions::new()
            .write(true)
            .open(&self.path)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        #[cfg(windows)]
        self.server.write_all(b"\n").unwrap();
    }
}

/// Where a host directory is plugged into a member at its host path (D235): its own path
/// where its top-level directory may be a plug root; a Windows drive's beneath `/mnt` as
/// WSL has it (`C:\x` at `/mnt/c/x`); and beneath `/srv`, a Linux host's root, where its
/// top is `/tmp`, which no root is and in which a Linux host's test directories lie.
pub fn at_host_path(host: &Path) -> String {
    let text = host.to_string_lossy().replace('\\', "/");
    let text = text.strip_prefix("//?/").unwrap_or(&text).to_string();
    match text.split_once(":/") {
        Some((drive, rest)) if drive.len() == 1 => format!("/mnt/{}/{rest}", drive.to_ascii_lowercase()),
        _ if text.starts_with("/tmp/") => format!("/srv/plugm{text}"),
        _ => text,
    }
}

/// The plug roots a run given host directory `bound` as it starts has (D235), as `shards`
/// names them (setup.rs `plug_roots`): its host OS's own, and on Unix `bound`'s top-level
/// directory, where that may be one.
pub fn plug_roots(bound: &Path) -> Vec<String> {
    let mut roots: Vec<String> = if cfg!(target_os = "macos") {
        vec!["/Users".into(), "/Volumes".into()]
    } else if cfg!(windows) {
        vec!["/mnt".into()]
    } else {
        vec!["/home".into(), "/media".into(), "/mnt".into(), "/srv".into()]
    };
    let top = bound
        .to_string_lossy()
        .strip_prefix('/')
        .and_then(|rest| rest.split('/').next().map(|t| format!("/{t}")));
    if let Some(top) = top.filter(|t| cfg!(unix) && t != "/tmp" && !roots.contains(t)) {
        roots.push(top);
    }
    roots
}

/// A file held open sharing nothing under a Read-Handle oplock (FSCTL_REQUEST_OPLOCK): an
/// open of it by another handle that reads, writes or deletes it, which its sharing
/// refuses, breaks the oplock and waits for the break to be acknowledged, which closing
/// the oplock's handle does as this drops. The wait has no end: "the system would wait
/// forever to receive the acknowledgment as there's no timeout" ("Breaking Oplocks"); "the
/// open operation is blocked and the Read-Handle oplock breaks. The application that owns
/// the oplock may acknowledge the break by closing the handle" ("Types of Opportunistic
/// Locks"). An open that reads its attributes alone shares nothing and waits for nothing.
#[cfg(windows)]
pub struct Oplocked {
    file: Option<std::fs::File>,
    // Written as the request completes, which may be after it is made: boxed, not to move,
    // and kept until it has.
    overlapped: Box<windows_sys::Win32::System::IO::OVERLAPPED>,
    _answer: Box<windows_sys::Win32::System::Ioctl::REQUEST_OPLOCK_OUTPUT_BUFFER>,
    event: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl Oplocked {
    /// `path` held so, waiting out at most `timeout` another's handle that shares nothing.
    pub fn new(path: &Path, timeout: Duration) -> Oplocked {
        use std::os::windows::fs::OpenOptionsExt as _;
        use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
        use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, ERROR_SHARING_VIOLATION};
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;
        use windows_sys::Win32::System::IO::{DeviceIoControl, OVERLAPPED};
        use windows_sys::Win32::System::Ioctl::{
            FSCTL_REQUEST_OPLOCK, OPLOCK_LEVEL_CACHE_HANDLE, OPLOCK_LEVEL_CACHE_READ,
            REQUEST_OPLOCK_CURRENT_VERSION, REQUEST_OPLOCK_INPUT_BUFFER, REQUEST_OPLOCK_INPUT_FLAG_REQUEST,
            REQUEST_OPLOCK_OUTPUT_BUFFER,
        };
        use windows_sys::Win32::System::Threading::CreateEventW;
        // A handle that shares nothing is refused while another reads the file: Defender's,
        // scanning the copy just written, is waited out.
        let deadline = Instant::now() + timeout;
        let file = loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .custom_flags(FILE_FLAG_OVERLAPPED)
                .open(path)
            {
                Ok(file) => break file,
                Err(e)
                    if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION.cast_signed())
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("{}: {e}", path.display()),
            }
        };
        // SAFETY: an unnamed manual-reset event, owned from here.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        assert!(!event.is_null(), "an event: {}", std::io::Error::last_os_error());
        // SAFETY: the event just made, this alone holds.
        let event = unsafe { OwnedHandle::from_raw_handle(event) };
        // SAFETY: OVERLAPPED and REQUEST_OPLOCK_OUTPUT_BUFFER are plain data, zeroes valid
        // values of them.
        let (mut overlapped, mut answer): (Box<OVERLAPPED>, Box<REQUEST_OPLOCK_OUTPUT_BUFFER>) =
            unsafe { (Box::new(std::mem::zeroed()), Box::new(std::mem::zeroed())) };
        overlapped.hEvent = event.as_raw_handle();
        let version = u16::try_from(REQUEST_OPLOCK_CURRENT_VERSION).unwrap();
        answer.StructureVersion = version;
        answer.StructureLength = u16::try_from(size_of::<REQUEST_OPLOCK_OUTPUT_BUFFER>()).unwrap();
        let asked = REQUEST_OPLOCK_INPUT_BUFFER {
            StructureVersion: version,
            StructureLength: u16::try_from(size_of::<REQUEST_OPLOCK_INPUT_BUFFER>()).unwrap(),
            RequestedOplockLevel: OPLOCK_LEVEL_CACHE_READ | OPLOCK_LEVEL_CACHE_HANDLE,
            Flags: REQUEST_OPLOCK_INPUT_FLAG_REQUEST,
        };
        // SAFETY: the request is copied in as it is made (METHOD_BUFFERED), and its answer
        // and OVERLAPPED live until it has completed, which dropping this waits for.
        let done = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_REQUEST_OPLOCK,
                (&raw const asked).cast(),
                u32::try_from(size_of::<REQUEST_OPLOCK_INPUT_BUFFER>()).unwrap(),
                (&raw mut *answer).cast(),
                u32::try_from(size_of::<REQUEST_OPLOCK_OUTPUT_BUFFER>()).unwrap(),
                std::ptr::null_mut(),
                &raw mut *overlapped,
            )
        };
        // Granted, the request pends until the oplock breaks.
        let e = std::io::Error::last_os_error();
        assert!(
            done == 0 && e.raw_os_error() == Some(ERROR_IO_PENDING.cast_signed()),
            "an oplock on {}: {e}",
            path.display()
        );
        Oplocked {
            file: Some(file),
            overlapped,
            _answer: answer,
            event,
        }
    }
}

#[cfg(windows)]
impl Oplocked {
    /// The event its request signals as it completes: as an open of the file breaks the
    /// oplock, its opener then waiting until this drops.
    pub fn breaking(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        use std::os::windows::io::AsHandle as _;
        self.event.as_handle()
    }
}

#[cfg(windows)]
impl Drop for Oplocked {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult};
        let Some(file) = self.file.take() else {
            return;
        };
        // The request, still pending or completed by a break, ended and waited for before
        // its buffers go; then the handle is closed, which acknowledges a break, and an open
        // that waits on it goes on.
        // SAFETY: the request made on this handle with this OVERLAPPED.
        unsafe {
            CancelIoEx(file.as_raw_handle(), &raw const *self.overlapped);
            let mut moved = 0;
            GetOverlappedResult(
                file.as_raw_handle(),
                &raw const *self.overlapped,
                &raw mut moved,
                1,
            );
        }
        drop(file);
    }
}
