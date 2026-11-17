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
#[track_caller]
pub fn restarts_cross_at(home: &Path, from: u64) {
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
    let _ = run_shards_env(&["daemon"], &["stop"], &env);
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

/// A home whose guest is the test kernel and init, recorded before its daemon starts.
#[track_caller]
pub fn home_with(name: &str) -> TempDir {
    let home = TempDir::new(name);
    let args = [
        "use".as_ref(),
        "--kernel".as_ref(),
        kernel().as_os_str(),
        "--init".as_ref(),
        guest_init().as_os_str(),
    ];
    let recorded = run_shards_env(&["guest"], &args, &[("SHARDS_HOME", home.as_os_str())]);
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

/// The most a window's vCPU guest time, read twice, may say past what its vCPU ran between
/// the reads: one step of its count, measured as its VM process says it, `read` of a guest
/// whose vCPU spins unheld: read until it has moved twice, the second move's size, as a thread
/// clock's step is measured (`granule::clock_step`). A count's unit is not its step: WHP's
/// runtime counters count 100 ns (`TotalRuntime100ns`) but moved every ~10 ms on hex-winvm,
/// 9.4 ms of guest each (p50, n 200; D265); KVM's gtime moves in clock ticks
/// (fs/proc/array.c); Hypervisor.framework's as its vCPU leaves its guest to be read
/// (docs/research/measurements/guest-cpu-time-v1).
pub fn guest_time_step(mut read: impl FnMut() -> u64) -> Duration {
    let moved = |read: &mut dyn FnMut() -> u64, from: u64| loop {
        let now = read();
        if now != from {
            return now;
        }
    };
    let first = read();
    let once = moved(&mut read, first);
    let twice = moved(&mut read, once);
    Duration::from_nanos(twice.saturating_sub(once))
}

/// Whether this host cannot run VMs: no backend for it yet, or no hardware virtualization
/// (e.g. a CI runner that is itself a VM). VM tests then return early with a SKIP line,
/// or fail where `SHARDS_REQUIRE_VMS=1` says they must run;
/// `this_host_has_its_hypervisor_backend` pins which hosts must have a backend.
pub fn cannot_run_vms() -> bool {
    match shards_vmm::vm::check_host() {
        Ok(()) => false,
        Err(why) => skip(&why.to_string()),
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

/// Whether `said`, what a run printed or a daemon's log, tells of a vCPU entry its host
/// refused that D261's gate takes for its host's (ROOT's ruling of 2026-11-17,
/// `shards_vmm::vm::limits::the_hosts`): VMX's error 7, on a host that is itself a VM, its
/// guest having run, or another VM process whose guest had run refused within its host model's
/// burst width. Then a SKIP line says the refusal, with its evidence, and its witness, even
/// where `SHARDS_REQUIRE_VMS=1` says VMs must run: such a host runs VMs. Any other refusal is
/// said with why the gate does not take it, and its test's failure stands. Its witnesses are
/// read from the records its VM process kept it in, and from the tests' own ([`vcpu_limits`]),
/// where the machines a test boots in its own process keep theirs. The burst width is waited
/// out from the refusal's moment first, so that a witness kept just after it is read.
pub fn host_refused(said: &str) -> bool {
    use shards_vmm::vm::limits;
    let Some(refused) = limits::Said::find(said) else {
        return false;
    };
    let records: Vec<limits::Limits> = refused
        .file
        .clone()
        .map(|file| limits::Limits::kept_in(file, limits::HostModel::here()))
        .into_iter()
        .chain([vcpu_limits()])
        .collect();
    let width = || {
        records
            .iter()
            .map(limits::Limits::burst_width_us)
            .max()
            .unwrap_or_else(limits::measured_width_us)
    };
    let wait = refused
        .refusal
        .at_us
        .saturating_add(width())
        .saturating_sub(limits::now_us());
    std::thread::sleep(Duration::from_micros(wait));
    let kept: Vec<limits::Refusal> = records.iter().flat_map(limits::Limits::refusals).collect();
    match limits::the_hosts(&refused, &kept, width()) {
        Ok(witness) => {
            let _ = writeln!(
                std::io::stderr(),
                "SKIP: {}: the host's, by D261's gate: {witness}",
                refused.line
            );
            true
        }
        Err(not) => {
            let _ = writeln!(
                std::io::stderr(),
                "a refused entry D261's gate does not take for its host's ({not}): {}",
                refused.line
            );
            false
        }
    }
}

/// [`host_refused`] of what a daemon's `log` says of the VM a run's error, `said`, names as
/// ended (`VM N ended`): its refusal, not another VM's the same log holds.
pub fn vm_host_refused(log: &str, said: &str) -> bool {
    let of_vm: Vec<&str> = log
        .lines()
        .filter(|line| {
            shards_vmm::vm::limits::Said::find(line)
                .is_some_and(|refused| said.contains(&format!("VM {} ended", refused.refusal.machine)))
        })
        .collect();
    host_refused(&of_vm.join("\n"))
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

/// Plan step 33's leak scan (docs/research/measurements/s33-leak-scan), on what it scans:
/// what `ImageGuest::save` writes, which is its owner's alone (D259).
/// Python 3 runs it: `python3`, on Windows `python`.
fn leak_scan(secrets: &Path, paths: &[&Path]) -> (Option<i32>, String) {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/research/measurements/s33-leak-scan/scan.py");
    let python = if cfg!(windows) { "python" } else { "python3" };
    let out = Command::new(python)
        .arg("-I")
        .arg(script)
        .arg("--secrets")
        .arg(secrets)
        .args(paths)
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code(), said)
}

/// api_image's leak-scan test judged (D259): `saved`, a run's save, scanned with its harness's
/// environment as given (`keys/env`), first for the values the harness was given
/// (`keys/given.json`), then for one never given (`keys/never.json`). Both values given are
/// found in the save's memory, each by its name, and the environment's in its file (exit 3);
/// the one never given nowhere. Where the test runs, or on L1's outer host
/// ([`leave_leak_scan`]).
pub fn leak_scan_judged(saved: &Path, keys: &Path) {
    let memory = snapshot_file(saved, "memory").display().to_string();
    let env = keys.join("env");
    let (code, said) = leak_scan(&keys.join("given.json"), &[saved, &env]);
    assert_eq!(code, Some(3), "{said}");
    for name in ["given.json:env_token", "given.json:file_token"] {
        assert!(
            said.lines()
                .any(|l| l.starts_with(&format!("FOUND {memory} offset ")) && l.ends_with(name)),
            "{name} in the save's memory: {said}"
        );
    }
    assert!(
        said.contains("harness-codex offset 19 length 64: a run of given.json:env_token"),
        "{said}"
    );
    // Never given, never found: what else credential-shaped its memory holds is listed (exit 4)
    // and is no secret's.
    let (code, said) = leak_scan(&keys.join("never.json"), &[saved, &env]);
    assert!(matches!(code, Some(0 | 4)), "{said}");
    assert!(said.contains(" 0 secret runs found"), "{said}");
}

/// Where in the outer host's share a test under L1 leaves a run's save to scan.
const LEAK_SCAN_LEFT: &str = "leak-scan";

/// `saved`, a run's save, and `keys`, what its scan is given ([`leak_scan_judged`]), left in
/// `out`, the outer host's share under L1 (SHARDS_L1_OUT, boot.rs), for that host to scan
/// once the guest is gone ([`judge_left_leak_scan`]): an L1 guest has no Python.
pub fn leave_leak_scan(out: &Path, saved: &Path, keys: &Path) {
    let left = out.join(LEAK_SCAN_LEFT);
    copy_used_tree(saved, &left.join("saved"));
    copy_used_tree(keys, &left.join("keys"));
}

/// The save a test under L1 left in `out`, this host's side of its share
/// ([`leave_leak_scan`]), judged as the test judges one in place; nothing where none was left.
pub fn judge_left_leak_scan(out: &Path) {
    let left = out.join(LEAK_SCAN_LEFT);
    if left.exists() {
        leak_scan_judged(&left.join("saved"), &left.join("keys"));
    }
}

/// Directory `from` copied to `to`: its directories, and its regular files, each with only
/// what its file system says holds data and is not all zeros (`platform::copy_used`), so that a
/// save's sparse files take no more of `to`'s disk than they did of `from`'s, and synced, as `to`
/// may be a share whose host reads it once this guest is gone. No link is copied: the scan
/// follows none.
fn copy_used_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let (path, copy) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            copy_used_tree(&path, &copy);
        } else if kind.is_file() {
            let source = std::fs::File::open(&path).unwrap();
            let len = source.metadata().unwrap().len();
            let made = std::fs::File::create(&copy).unwrap();
            shards_vmm::platform::copy_used(&source, &made, len).unwrap();
            made.sync_all().unwrap();
        }
    }
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
                name: "Image-6.18.48-aarch64-kvm-836568f8ad2c",
                url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-836568f8ad2c/Image-6.18.48-aarch64-kvm",
                sha256: "ff68fc5ede1ef64f9f8c9323c9429dcfbec065e579aa98a4028bb8a269b55d08",
            },
            "x86_64" => Artifact {
                name: "vmlinux-6.18.48-x86_64-kvm-836568f8ad2c",
                url: "https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-836568f8ad2c/vmlinux-6.18.48-x86_64-kvm",
                sha256: "759273d6df9d6b3800ab1ba8a792be8c3bf8f956d5a72d7ea850b831bc6ecf76",
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
/// through the process's own handle ([`keyed_line`]), not WMI: CIM's Win32_Process
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

/// What a home learned its builders' guest needs to boot, in bytes (D133, memory-15): the
/// `boot IDENTITY BYTES` line of its `builder-memory`, which a builder adds as it ends. One
/// guest (one kernel, init and CPU count) per test home, so one line; None where the home
/// has learned none, as before its first builder ended: its next builder boots cold, with
/// all it may have.
pub fn learned_boot(home: &Path) -> Option<u64> {
    let said = std::fs::read_to_string(home.join("builder-memory")).ok()?;
    let boots: Vec<u64> = said
        .lines()
        .filter_map(|l| l.strip_prefix("boot "))
        .filter_map(|rest| rest.split_whitespace().nth(1)?.parse().ok())
        .collect();
    assert!(
        boots.len() <= 1,
        "more than one guest learned in one home: {said}"
    );
    boots.first().copied()
}

/// What `/proc/sys/vm/pluggable_kbytes` read in each step that `testguest stat`ed it, in
/// order, in KiB (D149): the memory its builder may still plug, which its init reports
/// where it has memory to plug and the kernel lowers as it is plugged; 0 in a builder that
/// booted with all it may have (D133, memory-15).
pub fn pluggable_said(stderr: &str) -> Vec<u64> {
    let mut said = Vec::new();
    let mut lines = stderr.lines();
    while let Some(line) = lines.next() {
        if !line.contains("/proc/sys/vm/pluggable_kbytes file") {
            continue;
        }
        // `#N SECS = VALUE\n`, the file's text as `stat` prints it, its newline escaped.
        let value = lines
            .find_map(|l| l.split_once(" = ").map(|(_, v)| v.to_string()))
            .unwrap_or_default();
        let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
        said.push(
            digits
                .parse()
                .unwrap_or_else(|_| panic!("pluggable_kbytes read {value:?}")),
        );
    }
    said
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
/// end, as the kernel closes the dead process's descriptors one by one. That is its host's
/// work, no party's of the test's: waited out however long it takes, each look paced as
/// D215 paces a wait's looks (`shards_api::pause`).
pub fn until_unserved(home: &Path) {
    let began = Instant::now();
    loop {
        let look = Instant::now();
        if !shards_ipc::slots::served(home) {
            return;
        }
        std::thread::sleep(shards_api::pause(look.elapsed(), began.elapsed(), Duration::MAX));
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
    minted_port(port)
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
                launched(&dir.join(format!("shards{}", std::env::consts::EXE_SUFFIX)));
                return dir;
            }
        }
    })
}

/// Launches the tests' copy of `shards` once, by each test process before any of its tests
/// runs it: a freshly written executable's first exec waits on its host's assessment of it
/// (macOS: XProtect and syspolicyd, 1.19-1.34 s for a test-release `shards` against 4-10 ms
/// for its next, its process running nothing of its own meanwhile, PM M611), which a judged
/// command would take for a stall (the hot-swap land gate's first api_image test, refused
/// 380 ms into its `shards build harness`). What it answers is nothing: a copy that cannot run
/// fails the test that runs it.
fn launched(shards: &Path) {
    let mut cmd = Command::new(shards);
    as_tests_run(&mut cmd);
    let _ = cmd
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Copies `built` into `root`, as `name`: `shards`, which carries the rest, the VM process
/// signed inside it as a release signs it (build.rs). Each copy written by a process of its
/// own (`shards_testdir::program_copy`), as every program a test runs is: none of this one's
/// descriptors ever writes it, so no exec of it fails ETXTBSY.
fn place(root: &Path, name: &str, built: &[(&str, &Path)]) {
    let dir = root.join(name);
    let temp = root.join(format!("{name}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).unwrap();
    for &(bin, path) in built {
        let copy = temp.join(format!("{bin}{}", std::env::consts::EXE_SUFFIX));
        shards_testdir::program_copy(path, &copy).unwrap();
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

/// Runs `shards run <args>`, `<args>` starting `--kernel`, as [`run_command`] waits on it.
#[track_caller]
pub fn vm_run<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> Run {
    run_shards(&["run"], args)
}

/// Runs `shards <command...> <args...>`, as [`run_command`] waits on it.
#[track_caller]
pub fn run_shards<S: AsRef<std::ffi::OsStr>>(command: &[&str], args: &[S]) -> Run {
    run_shards_env(command, args, &[])
}

/// [`run_shards`], with `env` added to shards' environment.
#[track_caller]
pub fn run_shards_env<S: AsRef<std::ffi::OsStr>>(
    command: &[&str],
    args: &[S],
    env: &[(&str, &std::ffi::OsStr)],
) -> Run {
    run_shards_with(command, args, env, None)
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
/// is of that moment. It watches for as long as the run goes on, whose own wait is judged
/// ([`run_command`]): no bound of its own.
pub fn run_watching(
    env: &[(&str, &std::ffi::OsStr)],
    args: &[&str],
    container: &str,
    fields: &str,
    ready: &mut dyn FnMut(&[Vec<String>]) -> bool,
) -> Run {
    // `shards top`'s table needs a `pid` column to key its rows (procps ps), so every
    // field list leads with it; then the caught signals and the name, which say whether
    // the command awaits; a row is those, then the fields asked.
    let with_pid = format!("pid,caught,comm,{fields}");
    let start = Instant::now();
    std::thread::scope(|scope| {
        let ran = scope.spawn(|| run_shards_env(&[], args, env));
        let mut last = String::new();
        let mut signalled = false;
        let mut was_ready = false;
        while !ran.is_finished() {
            let look = Instant::now();
            let top = run_shards_env(&["top"], &[container, "-o", &with_pid], env);
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
                let sent = run_shards_env(&["kill"], &["-s", "USR1", container], env);
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
            // microVM that reads its /proc. Paced as D215 paces a wait's looks, by a look's
            // own cost against the wait so far (`shards_api::pause`: sqrt(2 c w)).
            std::thread::sleep(shards_api::pause(look.elapsed(), start.elapsed(), Duration::MAX));
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
) -> Run {
    let (name, count) = ready;
    let mut seen = |rows: &[Vec<String>]| {
        rows.iter()
            .filter(|r| r.first().is_some_and(|c| c == name))
            .count()
            >= count
    };
    run_watching(env, args, container, "comm", &mut seen)
}

/// [`run_shards_env`], in the working directory `dir`.
#[track_caller]
pub fn run_shards_env_in<S: AsRef<std::ffi::OsStr>>(
    dir: &Path,
    command: &[&str],
    args: &[S],
    env: &[(&str, &std::ffi::OsStr)],
) -> Run {
    run_shards_with(command, args, env, Some(dir))
}

/// [`run_shards`], in the working directory `dir`.
#[track_caller]
pub fn run_shards_in<S: AsRef<std::ffi::OsStr>>(dir: &Path, command: &[&str], args: &[S]) -> Run {
    run_shards_with(command, args, &[], Some(dir))
}

/// `cmd`'s output, as [`Command::output`] gives it: a client of shards' (docker, Compose) or
/// a host's tool, waited on as [`run_command`] waits on a command ([`Wait`], D215's judge),
/// its kind its call site, its party the client and what descends from it, and `home`'s
/// daemon where it talks to one. A client its judge refuses fails its test with what it
/// said, rather than hold the suite: it is sent SIGQUIT first, on which a Go program writes
/// where each of its goroutines waits and exits, its exit waited on as judged, then killed.
/// A stream another process holds open past the client's end is what came of it by its
/// judge's look.
#[track_caller]
pub fn client_output(cmd: &mut Command, home: Option<&Path>) -> std::process::Output {
    let kind = WaitKind::command(
        std::panic::Location::caller(),
        &command_words(cmd),
        &keyed_line(cmd),
    );
    let asked = format!("{cmd:?}");
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning a client");
    let (stdout, stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let (out_probe, err_probe) = (probe(&stdout), probe(&stderr));
    let out = collect(Box::new(stdout));
    let err = collect(Box::new(stderr));
    let (mut out_got, mut err_got) = ((Vec::new(), false), (Vec::new(), false));
    let mut wait = Wait::begin(kind, child.id(), home, &asked);
    let mut status = None;
    loop {
        if status.is_none()
            && let Some(st) = child.try_wait().unwrap()
        {
            if st.success() {
                wait.answered();
            } else {
                wait.unserved();
            }
            status = Some(st);
        }
        let output = more(&out, &mut out_got) | more(&err, &mut err_got);
        if let Some(status) = status.filter(|_| out_got.1 && err_got.1) {
            break std::process::Output {
                status,
                stdout: out_got.0,
                stderr: err_got.0,
            };
        }
        match (wait.look(output), status) {
            (Ok(()), _) => {}
            // What came, or the whole of a stream no process holds any more ([`taken_whole`]).
            (Err(_), Some(status)) => {
                taken_whole(&out, &out_probe, &mut out_got);
                taken_whole(&err, &err_probe, &mut err_got);
                break std::process::Output {
                    status,
                    stdout: out_got.0,
                    stderr: err_got.0,
                };
            }
            (Err(stall), None) => {
                let said = wait.refused(&stall);
                quit(&mut child, home);
                more(&out, &mut out_got);
                more(&err, &mut err_got);
                panic!(
                    "{said}\n--- stdout\n{}\n--- stderr\n{}",
                    String::from_utf8_lossy(&out_got.0),
                    String::from_utf8_lossy(&err_got.0)
                );
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A refused client ended: on Unix sent SIGQUIT, on which a Go program writes where each of
/// its goroutines waits and exits, its exit waited on as judged (its kind this call site's);
/// then killed.
#[track_caller]
fn quit(child: &mut std::process::Child, home: Option<&Path>) {
    #[cfg(unix)]
    {
        let kind = WaitKind::at(std::panic::Location::caller());
        let mut wait = Wait::begin(kind, child.id(), home, "its exit on SIGQUIT");
        // SAFETY: kill(2) of the child this holds, not yet waited for.
        unsafe { libc::kill(i32::try_from(child.id()).unwrap(), libc::SIGQUIT) };
        while child.try_wait().unwrap().is_none() && wait.look(false).is_ok() {
            std::thread::sleep(Duration::from_millis(2));
        }
        if child.try_wait().unwrap().is_some() {
            wait.answered();
        }
    }
    #[cfg(not(unix))]
    let _ = home;
    let _ = child.kill();
    let _ = child.wait();
}

/// A second handle of a client's pipe's read end, by which [`taken_whole`] tells whether any
/// process still holds its write end; none where it could not be made.
#[cfg(unix)]
pub type Probe = Option<std::os::fd::OwnedFd>;

/// A second handle of a client's pipe's read end, by which [`taken_whole`] tells whether any
/// process still holds its write end; none where it could not be made.
#[cfg(windows)]
pub type Probe = Option<std::os::windows::io::OwnedHandle>;

/// [`Probe`] of `end`, a pipe's read end.
#[cfg(unix)]
pub fn probe(end: &impl std::os::fd::AsFd) -> Probe {
    end.as_fd().try_clone_to_owned().ok()
}

/// [`Probe`] of `end`, a pipe's read end.
#[cfg(windows)]
pub fn probe(end: &impl std::os::windows::io::AsHandle) -> Probe {
    end.as_handle().try_clone_to_owned().ok()
}

/// Whether no process holds the write end of the pipe `probe` reads, so that its stream's end
/// is certain: poll(2)'s POLLHUP of its read end, which a pipe says once its last writer has
/// closed (Linux fs/pipe.c `pipe_poll`; XNU's poll(2), from `filt_piperead`'s EV_EOF).
#[cfg(unix)]
fn writers_gone(probe: &Probe) -> bool {
    use std::os::fd::AsRawFd as _;
    let Some(end) = probe else {
        return false;
    };
    let mut look = libc::pollfd {
        fd: end.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll(2) of one descriptor this holds, with no wait.
    let ready = unsafe { libc::poll(&raw mut look, 1, 0) };
    ready == 1 && look.revents & libc::POLLHUP != 0
}

/// Whether no process holds the write end of the pipe `probe` reads: PeekNamedPipe's
/// ERROR_BROKEN_PIPE, which Windows says only once the pipe is empty too, so that a pipe with
/// bytes still to read says nothing and its reader is waited for no more than before.
#[cfg(windows)]
fn writers_gone(probe: &Probe) -> bool {
    use std::os::windows::io::AsRawHandle as _;
    let Some(end) = probe else {
        return false;
    };
    let mut waiting = 0u32;
    // SAFETY: a peek of no bytes at a pipe's read end this holds, its count on this stack.
    let peeked = unsafe {
        windows_sys::Win32::System::Pipes::PeekNamedPipe(
            end.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &raw mut waiting,
            std::ptr::null_mut(),
        )
    };
    peeked == 0 && io::Error::last_os_error().kind() == io::ErrorKind::BrokenPipe
}

/// What came of a stream [`collect`] reads, taken whole where no process holds its pipe any
/// more ([`writers_gone`]): its end certain, its reader thread is waited for to it, however late
/// it runs. A stream a process still holds is taken as it came, its end not waited for. Taken
/// as it came alone, a client that ended at once on a busy host had both its streams said held
/// open, their reader threads yet to hand over what their pipes held (CI 44164945050, x86_64
/// musl: `shards --version`, its status 0 after 40 ms, its kind's W* its own run).
pub fn taken_whole(rx: &std::sync::mpsc::Receiver<Vec<u8>>, probe: &Probe, got: &mut (Vec<u8>, bool)) {
    more(rx, got);
    if got.1 || !writers_gone(probe) {
        return;
    }
    for chunk in rx.iter() {
        got.0.extend_from_slice(&chunk);
    }
    got.1 = true;
}

/// A stream read on a thread of its own and handed over by channel as it comes, so that a
/// pipe another process still holds (a VM given a client's stdio) never holds a wait, and
/// what came before it is kept.
pub fn collect(mut r: Box<dyn Read + Send>) -> std::sync::mpsc::Receiver<Vec<u8>> {
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
}

/// Whether more came of a stream [`collect`] reads since the call before, kept in `got`
/// with whether it has ended, its every writer gone.
fn more(rx: &std::sync::mpsc::Receiver<Vec<u8>>, got: &mut (Vec<u8>, bool)) -> bool {
    let mut came = false;
    loop {
        match rx.try_recv() {
            Ok(chunk) => {
                got.0.extend_from_slice(&chunk);
                came = true;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                got.1 = true;
                return came;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => return came,
        }
    }
}

#[track_caller]
fn run_shards_with<S: AsRef<std::ffi::OsStr>>(
    command: &[&str],
    args: &[S],
    env: &[(&str, &std::ffi::OsStr)],
    dir: Option<&Path>,
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
    run_command(cmd, &asked)
}

/// What a client that never ended shows before it is killed: its home's runs as its daemon
/// has them (`ps -a`), whether the daemon took its run at all, and what each running one's
/// guest runs ([`guests_processes`]); and on Windows its threads' stacks and its daemon's,
/// where this host has Microsoft's console debugger (cdb, the Windows SDK's Debugging
/// Tools), attached without stopping them (`-pv`).
fn stuck(pid: u32, home: Option<&Path>) -> String {
    stacks_of(&[("its client", pid)], home)
}

/// What `cmd` said, its stdout then its stderr, waited on as [`client_output`] waits, `home`'s
/// daemon in its party where it has one; a word that it said nothing where its wait was
/// refused, so that the failure this adds to is never lost to one of its own.
fn said_by(cmd: &mut Command, home: Option<&Path>) -> String {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| client_output(cmd, home))) {
        Ok(out) => format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(_) => "(its wait was refused: it made no progress past the slowest end of its kind)".to_string(),
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
    let listed = said_by(&mut ids, Some(home));
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
            said_by(&mut top, Some(home))
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
            said_by(&mut ps, Some(home))
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
                    said_by(&mut stacks, home)
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

/// What the daemon of a home has said in its log since this began watching, a [`Party`]'s
/// words: each whole line it gained, its heartbeat of a microVM a client waits on
/// (daemon.rs `log_progress`) among them, which says only that its microVM ran, as the
/// party's own look at that microVM's process already reads (D209: no rate a run answered
/// at bounds what counts as progress, R* being a refusal's diagnosis alone).
pub struct Said {
    log: PathBuf,
    /// How far the log has been read: to the end of the last whole line in it.
    len: u64,
}

impl Said {
    pub fn of(home: &Path) -> Said {
        let log = home.join("daemon.log");
        let len = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
        Said { log, len }
    }

    /// Whether the daemon has said a whole line since the call before, read to the end of
    /// the last of them: a [`Wait`]'s look at its party, which its judge paces. A log begun
    /// again is read from its start.
    pub fn said(&mut self) -> bool {
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
        true
    }
}

/// A test's wait on a state a home's daemon or its microVMs come to (a pool filled, a
/// restored run's VM up, a VM gone), judged as [`run_command`]'s wait on a command is
/// ([`Wait`], D215's judge; the person's ruling of 2026-11-13, D209): its kind is its call
/// site (`#[track_caller]` through the helpers that make one), whose answers ([`done`]) teach
/// W*, the slowest. Its party ([`Party`]) is its home's daemon and what descends from it,
/// this test's own VM processes and theirs, and the daemon's words ([`Said`]). Cold (no
/// answer of its kind on this host), it follows its party's liveness alone; learned, it is
/// looked at each W* past its start and refused at a look that finds its party made no
/// progress at all since the look before, its host not holding it. Cold or learned, it is
/// refused at a look that finds its party gone: none of it running and its daemon saying
/// nothing, its liveness ended. Its caller gives it no bound (R03).
///
/// [`done`]: Patience::done
pub struct Patience {
    wait: Wait,
    state: Patient,
}

/// Where a [`Patience`] is: each refusal said once, and followed by one look more, so that a
/// state that came as its party ended or stalled is still seen.
enum Patient {
    /// Waiting.
    Holds,
    /// Refused, what it said of itself kept for its look after.
    Refused(String),
    /// Its look after the refusal spent: no more.
    Spent,
}

/// Waits, judged as a state's wait is ([`Patience`]: its party `home`'s daemon, no clock),
/// until `home`'s daemon has done the work its clients' request slots left it (D185): a slot
/// a client took is replaced and its names removed as its session ends, work the daemon does
/// after its client has gone (slots.rs' `make_slots`: 125 to 317 µs of its CPU on a loaded
/// Mac, PM M255), which a test's wait that begins at once counts as its party's progress
/// (ROOT's batch 22, HVF: harness.rs' `a_wait_is_refused_though_its_own_polls_outlast_w_star`,
/// its daemon started by a `shards ps`, "its last progress its daemon ran"; the daemon
/// measured running 65 to 73 µs of CPU 5.3 to 5.9 ms after that `ps` returned in 2 of 6
/// starts with every CPU of the host busy, in none of 3 without). Settled, on Unix: each
/// list offers [`OFFERED`](shards_ipc::slots::OFFERED) slots again, that many records unread
/// in its FIFO (FIONREAD, as the daemon's own `Requests::waiting` counts them, none read), and
/// the directory names no slot past them, a taken one's names removed. On Windows a slot is
/// a pipe's instance, which no listing shows (slots/named.rs): unread there. Then none of its
/// daemon's threads runnable ([`daemon_at_rest`]): slots seen as offered are the work's last
/// effect, not its end, which comes as the thread that made them sleeps (CI 44007524337,
/// aarch64-apple-darwin, the same test: settled, its wait's first window found "its daemon
/// 3186 ran", and none after, so it was refused a look late).
#[track_caller]
pub fn settled(home: &Path) {
    let mut patience = Patience::of(home);
    while !(slots_settled(home) && daemon_at_rest(home)) {
        assert!(
            patience.holds(),
            "{}'s request slots never settled",
            home.display()
        );
        std::thread::sleep(patience.next_look());
    }
    patience.done();
}

/// Whether `home`'s request slots are as its daemon offers them with no client's work left
/// ([`settled`]).
#[cfg(unix)]
fn slots_settled(home: &Path) -> bool {
    use shards_ipc::slots::{DIR, FREE, FREE_STREAMS, OFFERED, RECORD};
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::io::AsRawFd as _;
    let dir = home.join(DIR);
    let offering = |list: &str| {
        // A reader of its own, which reads nothing: the records stay the clients'.
        let Ok(fifo) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(dir.join(list))
        else {
            return false;
        };
        let mut bytes: libc::c_int = 0;
        // SAFETY: FIONREAD of a FIFO this holds open, into an int on this stack.
        let read = unsafe { libc::ioctl(fifo.as_raw_fd(), libc::FIONREAD, &raw mut bytes) };
        read == 0 && usize::try_from(bytes).is_ok_and(|bytes| bytes / RECORD == OFFERED)
    };
    if !(offering(FREE) && offering(FREE_STREAMS)) {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    let slots: std::collections::BTreeSet<u64> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.split_once('.')?.0.parse().ok())
        .collect();
    slots.len() == 2 * OFFERED
}

#[cfg(not(unix))]
fn slots_settled(_: &Path) -> bool {
    true
}

/// Whether none of `home`'s daemon's threads is runnable, running or waiting for a CPU, as
/// the judge reads them (`shards_api::runnable_threads`): the work its last client left it
/// done, its CPU counted to its threads as each slept (XNU adds a thread's run to its task's
/// as it leaves its CPU). True with no daemon, and where threads are not read (Windows).
fn daemon_at_rest(home: &Path) -> bool {
    shards_ipc::Named::read(&home.join(shards_ipc::PID))
        .filter(shards_ipc::Named::lives)
        .and_then(|daemon| shards_api::runnable_threads(daemon.pid))
        .is_none_or(|(runnable, _)| runnable == 0)
}

impl Patience {
    /// Patience with the daemon of `home` and its microVMs, and this test's own.
    #[track_caller]
    pub fn of(home: &Path) -> Patience {
        Patience::at(home, std::panic::Location::caller())
    }

    /// [`of`](Self::of) as the wait at `site`: for a `#[track_caller]` helper that waits on a
    /// thread of its own, which takes its caller's site before it spawns it, as a closure
    /// is no `#[track_caller]` and its own site would be every caller's one kind (log
    /// lanes' `controlled`: a quick read's W* refused a run's bulk write 83 ms in).
    pub fn at(home: &Path, site: &'static std::panic::Location<'static>) -> Patience {
        Patience::with(Some(home), site)
    }

    /// [`at`](Self::at) waiting for `awaited`, a kind kept apart from the site's other waits
    /// ([`WaitKind::awaiting`]; [`STATE`] is `at`'s own): an answer's body, which comes when
    /// what it answers comes (a container's end, an event stream's end), read after its
    /// head, which comes at once and would teach the body's wait a W* of one read (CI
    /// 43968984258's x86_64 musl job: engine.rs' waits for a container's end and for an
    /// events stream's last chunk refused 4 to 16 ms into a stillness past their heads').
    pub fn awaiting(home: &Path, site: &'static std::panic::Location<'static>, awaited: &str) -> Patience {
        Patience {
            wait: Wait::on(WaitKind::awaiting(site, awaited), Party::own(Some(home)), awaited),
            state: Patient::Holds,
        }
    }

    /// Patience with client `client`, what descends from it, and the daemon of `home`: a
    /// wait on what a command brings about, which starts its home's daemon, or waits out one
    /// of another build stepping aside, before any daemon is there to follow.
    #[track_caller]
    pub fn client(client: u32, home: &Path) -> Patience {
        Patience::on(Party::client(client, Some(home)), std::panic::Location::caller())
    }

    /// [`client`](Self::client) of several clients: a wait on what they bring about
    /// together, as a run's client writes what a follower of its log reads.
    #[track_caller]
    pub fn clients(clients: &[u32], home: &Path) -> Patience {
        Patience::on(
            Party::with(Members::Clients(clients.to_vec()), Some(home)),
            std::panic::Location::caller(),
        )
    }

    /// Patience with this test's own microVMs, where no home's daemon serves them.
    #[track_caller]
    pub fn own() -> Patience {
        Patience::with(None, std::panic::Location::caller())
    }

    /// Patience with every process this test's process started ([`Party::started`]): the run
    /// hosts shards-api's `ImageShards` starts and their VM processes, where no daemon serves
    /// them.
    #[track_caller]
    pub fn started() -> Patience {
        Patience::on(Party::started(None), std::panic::Location::caller())
    }

    /// Patience with every process of process groups `groups`, wherever it is listed
    /// ([`Party::groups`]): run hosts whose holder has ended, none of them this test's
    /// process's own since, each leading its group, its VM's among it.
    #[cfg(unix)]
    #[track_caller]
    pub fn groups(groups: &[u32]) -> Patience {
        Patience::on(Party::groups(groups), std::panic::Location::caller())
    }

    fn with(home: Option<&Path>, site: &'static std::panic::Location<'static>) -> Patience {
        Patience::on(Party::own(home), site)
    }

    fn on(party: Party, site: &'static std::panic::Location<'static>) -> Patience {
        Patience {
            wait: Wait::on(WaitKind::awaiting(site, STATE), party, STATE),
            state: Patient::Holds,
        }
    }

    /// It as a wait on the delivery of a run's log to `to` ([`Wait::delivery_to`]): a log
    /// server's wait for a run's lines.
    pub fn delivery_to(mut self, to: &str) -> Patience {
        self.wait.party.delivery = Some(to.to_string());
        self
    }

    /// Whether to look again: while its judge holds it and its party lives; once either
    /// refuses it, once more, the look after this one, and then no more, its account said on
    /// stderr. Ask it after each look that found the state not yet come, as often as they
    /// come: it looks at its party only as its pace says.
    pub fn holds(&mut self) -> bool {
        match std::mem::replace(&mut self.state, Patient::Spent) {
            Patient::Holds => {
                let refused = match self.wait.look(false) {
                    Err(stall) => Some(self.wait.refused(&stall)),
                    Ok(()) if self.wait.gone() => Some(format!(
                        "its party is gone, its liveness ended\n--- its wait\n{}",
                        self.wait.account()
                    )),
                    Ok(()) => None,
                };
                self.state = refused.map_or(Patient::Holds, Patient::Refused);
                true
            }
            Patient::Refused(said) => {
                let _ = writeln!(io::stderr(), "{said}");
                false
            }
            Patient::Spent => false,
        }
    }

    /// How long until its next look is due: what a caller that blocks on its state (a read,
    /// a channel) may block for before it asks [`holds`](Self::holds) again.
    pub fn next_look(&self) -> Duration {
        self.wait.next_look()
    }

    /// The state came: its kind's answer, which teaches its record, kept on this host. A
    /// state seen only at the look after a refusal teaches nothing.
    pub fn done(mut self) {
        if matches!(self.state, Patient::Holds) {
            self.wait.answered();
        }
    }

    /// What its kind has learned on this host.
    pub fn seen(&self) -> shards_api::Seen {
        self.wait.kind.record().seen()
    }

    /// Its wait's account ([`Wait::account`]): what its last look counted as progress.
    pub fn account(&self) -> String {
        self.wait.account()
    }

    /// The window a stillness of its party is refused over as its looks so far say it: W*, or
    /// what its looks resolve where that is longer (a counter's tick, a look's own time). None
    /// while it is not judged.
    pub fn window(&self) -> Option<Duration> {
        self.wait.judge.stillness().map(|(_, _, window)| window)
    }
}

/// What [`requests_end`]'s waits await, and key their kind by: the next end of the requests
/// their party serves.
pub const NEXT_END: &str = "a request's next end";

/// Waits for `n` more ends of requests as `ended` brings them, judged by their own progress,
/// their ends (D209 Part 11), as [`guest_says`] judges a guest by its lines: benches/arrival.rs'
/// drain of each rate's requests once all of them have arrived.
/// - Each end begins a wait for the next, judged as D215's judge judges ([`Wait`]), of one kind
///   at its call site ([`NEXT_END`]), whichever rate it drains. Its party is the requests still
///   outstanding by their clients (`clients`, less each end's own, `client_of`), what descends
///   from them, and `home`'s daemon, whose CPU is its liveness alone ([`Cpu::Liveness`]): the
///   daemon and its pools' warm microVMs run whether or not a request ends, and a drain judged
///   by its party's CPU was never refused (CI 44007524337). Its daemon's words are progress
///   still, among them its heartbeat of a microVM a client waits on that runs (daemon.rs
///   `log_progress`).
/// - Its W* is the slowest stretch this host has shown with requests outstanding and none
///   ending: `stretches`, those its caller saw before it, taught first; what this host kept of
///   its kind; and the slowest wait for a next end since. With none, its party's liveness alone.
/// - Only a pass keeps what it was taught on this host: once all `n` came. A wait refused, or
///   its party gone, keeps nothing, and an end that came only as it was refused teaches nothing.
///
/// The ends in the order they came; refused, those that came and what its wait said.
#[track_caller]
pub fn requests_end<E>(
    ended: &std::sync::mpsc::Receiver<E>,
    n: usize,
    clients: &[u32],
    client_of: impl Fn(&E) -> Option<u32>,
    stretches: &[Duration],
    home: &Path,
) -> Result<Vec<E>, (Vec<E>, String)> {
    let kind = WaitKind::awaiting(std::panic::Location::caller(), NEXT_END);
    for &stretch in stretches {
        kind.record().teach_unrated(stretch);
    }
    let party = |outstanding: &[u32]| {
        let mut party = Party::with(Members::Clients(outstanding.to_vec()), Some(home));
        party.read_cpu(Cpu::Liveness);
        party
    };
    let mut outstanding = clients.to_vec();
    let mut wait = Wait::on(kind, party(&outstanding), NEXT_END);
    let (mut came, mut slowest) = (Vec::with_capacity(n), None::<Duration>);
    while came.len() < n {
        match ended.recv_timeout(wait.next_look()) {
            Ok(end) => {
                if wait.awaited() {
                    let waited = wait.began.elapsed();
                    slowest = Some(slowest.map_or(waited, |s| s.max(waited)));
                }
                if let Some(client) = client_of(&end) {
                    outstanding.retain(|&c| c != client);
                }
                came.push(end);
                wait = Wait::on(kind, party(&outstanding), NEXT_END);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let refused = match wait.look(false) {
                    Err(stall) => wait.refused(&stall),
                    Ok(()) if wait.gone() => format!(
                        "its party is gone, its liveness ended\n--- its wait\n{}",
                        wait.account()
                    ),
                    Ok(()) => continue,
                };
                // One look more, as a refused Patience's: the ends that came as it was refused.
                came.extend(ended.try_iter());
                return if came.len() < n {
                    Err((came, refused))
                } else {
                    Ok(came)
                };
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let said = format!(
                    "its ends' channel closed with {} of {n} come\n--- its wait\n{}",
                    came.len(),
                    wait.account()
                );
                return Err((came, said));
            }
        }
    }
    if let Some(waited) = slowest {
        kind.record().teach_unrated(waited);
    }
    if slowest.is_some() || !stretches.is_empty() {
        kind.keep();
    }
    Ok(came)
}

/// The least a guest gives back at once: one block of its free lists, which free page
/// reporting hands the host whole (`page_reporting_order`, its pageblock: 2 MiB with 4 KiB
/// pages, the least a page is, and more with larger pages; mm/page_reporting.c,
/// virtio/balloon.rs, D151). A dip of less is no give-back's, and taken for its first fall it
/// ends a cold settle before the guest has reported: CI 43968984258's x86_64 musl job took a
/// microVM "idle" at 871706624 bytes after a 1 GiB free, and its 512 MiB read then grew it
/// 29 MiB, a baseline taken before its give-back had settled.
pub const REPORTED: u64 = 2 << 20;

/// A give-back followed (ROOT's ruling, D209 Part 4): a VM process's own resident memory
/// (`Usage::footprint`) while it falls. A give-back is progress while that falls, as the
/// daemon's judge of an end counts it (daemon/ends.rs `goes_on`), so this is a judged
/// [`Wait`] whose progress is a fall alone, its CPU its liveness only (an idle guest's ticks
/// give nothing back), and whose kind, its caller's site, learns W*, the slowest give-back
/// seen: from the wait's start to its last fall. Learned, its judge's refusal, a whole W*
/// with no fall and its host not holding it, is the give-back's end; W* spans each gap
/// between falls of a give-back as long as the slowest. Cold, none seen on this host, the
/// give-back so far is the slowest seen: it ends once no fall has come for as long as from
/// its start to its last fall, its host's holds not counted, and none ends before its first
/// fall, which it waits on for as long as the process lives. Begun before what it gives
/// back is freed, its first fall spans the free and its guest's whole wait before it
/// reports (mm/page_reporting.c `PAGE_REPORTING_DELAY`, D151), which each later report
/// only waits again. A fall is one report's at least, [`REPORTED`] below the level its last
/// fall left or its memory rose to since: less is its process's own memory moving.
pub struct Falling {
    pid: u32,
    wait: Wait,
    /// The level a fall is counted from: its memory at its last fall, or the most it has
    /// held since.
    held: u64,
    /// Its last fall, and the last time a look found its host holding it since.
    fell: Option<Instant>,
    quiet: Option<Instant>,
}

impl Falling {
    /// Follows VM process `pid`'s own memory from now.
    #[track_caller]
    pub fn begin(pid: u32) -> Falling {
        let kind = WaitKind::at(std::panic::Location::caller());
        let mut party = Party::client(pid, None);
        party.read_cpu(Cpu::Liveness);
        Falling {
            pid,
            wait: Wait::on(kind, party, "its memory to stop falling"),
            held: Falling::footprint(pid),
            fell: None,
            quiet: None,
        }
    }

    fn footprint(pid: u32) -> u64 {
        match shards_vmm::platform::process_usage(pid) {
            Ok(usage) => usage.footprint,
            Err(e) => panic!("VM process {pid}'s memory unread: {e}"),
        }
    }

    /// What its kind has learned on this host.
    pub fn seen(&self) -> shards_api::Seen {
        self.wait.kind.record().seen()
    }

    /// What it holds of its own once its give-back has ended, which teaches its kind how
    /// long the give-back took. A process gone fails the test.
    pub fn settled(mut self) -> u64 {
        loop {
            let now = Falling::footprint(self.pid);
            let fell = now.saturating_add(REPORTED) <= self.held;
            if fell || now > self.held {
                self.held = now;
            }
            if fell {
                (self.fell, self.quiet) = (Some(Instant::now()), Some(Instant::now()));
            }
            let judged = self.wait.look(fell);
            assert!(
                !self.wait.gone(),
                "VM process {} gone as it gave back\n{}",
                self.pid,
                self.wait.account()
            );
            if self.wait.holding.is_some() && self.fell.is_some() {
                self.quiet = Some(Instant::now());
            }
            let ended = match (judged, self.fell, self.quiet) {
                (Err(_), ..) => true,
                (Ok(()), Some(fell), Some(quiet)) if !self.wait.judged() => {
                    quiet.elapsed() >= fell.saturating_duration_since(self.wait.began)
                }
                (Ok(()), ..) => false,
            };
            if ended {
                if let Some(fell) = self.fell {
                    let took = fell.saturating_duration_since(self.wait.began);
                    self.wait.read_through();
                    if self.wait.read == Some(true) {
                        self.wait.kind.record().teach(took, self.wait.work);
                    } else {
                        self.wait.kind.record().teach_unrated(took);
                    }
                    self.wait.kind.keep();
                }
                let _ = writeln!(
                    io::stderr(),
                    "VM process {}'s give-back settled at {now} bytes, {:?} in, its last fall {:?} in\n{}",
                    self.pid,
                    self.wait.began.elapsed(),
                    self.fell
                        .map(|fell| fell.saturating_duration_since(self.wait.began)),
                    self.wait.account()
                );
                return now;
            }
            std::thread::sleep(self.wait.next_look());
        }
    }
}

/// A claim of speed taken out of a test's pass or fail (D209, #77's ruling: a time that no
/// later event of the test's can be ordered against is a measurement, not a bound): `times`
/// told on the test's output as n, p50, p90, p99 and max, by nearest rank, with `what` they
/// time and this host.
pub fn measured(what: &str, times: &[Duration]) {
    let mut times = times.to_vec();
    times.sort_unstable();
    let Some(&max) = times.last() else {
        let _ = writeln!(io::stdout(), "MEASURED {what}: n 0");
        return;
    };
    // The nearest rank: the least time that many of the times are within.
    let at = |p: usize| times[(times.len() * p).div_ceil(100).max(1) - 1];
    let revision = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(workspace())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or_else(
            || "revision unknown".to_string(),
            |o| String::from_utf8_lossy(&o.stdout).trim().to_string(),
        );
    let _ = writeln!(
        io::stdout(),
        "MEASURED {what}: n {} p50 {:?} p90 {:?} p99 {:?} max {max:?} (host {} {}, {revision})",
        times.len(),
        at(50),
        at(90),
        at(99),
        host_name(),
        host_class(),
    );
}

/// The VM processes this test's process started itself, by the IDs their starts gave
/// ([`OwnVm`]): a [`Party::own`]'s, with what descends from them. Each is known by its
/// identity, never its name: a Linux helper started through its descriptor was named by it
/// (`5`), and a party found by the name `shards-vm` had none (D209).
static OWN_VMS: std::sync::Mutex<std::collections::BTreeSet<u32>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// A VM process this test's process started (`run --kernel` or `restore`, which becomes it on
/// Unix and starts it on Windows; a warm VM; a stand-in for one), one of this test's own
/// ([`Party::own`]) until this drops, which its owner does once it has ended it.
pub struct OwnVm(u32);

impl OwnVm {
    /// Process `pid`, just started by this test's process.
    pub fn started(pid: u32) -> OwnVm {
        OWN_VMS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(pid);
        OwnVm(pid)
    }
}

impl Drop for OwnVm {
    fn drop(&mut self) {
        OWN_VMS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// This test's own VM processes now ([`OwnVm`]).
fn own_vms() -> Vec<u32> {
    OWN_VMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .copied()
        .collect()
}

/// Every process, its parent and its name, the kernel's own (its executable's file name as
/// its exec cut it: Linux's `comm`, 15 bytes; macOS's `p_comm`, 16), read in this process
/// with no process started: on Linux from `/proc/PID/stat` (proc(5)), on macOS from
/// libproc (proc_listallpids(3), PROC_PIDT_SHORTBSDINFO). Every look at a wait's party
/// lists them ([`Party::look`]), and a `ps` spawned for it took 16 to 25 ms on adacachy2
/// (1,200 processes), more than a W* of 8 ms: the looks crowded out the reads they were
/// between (D209). A process gone as it is read is left out.
#[cfg(target_os = "linux")]
pub fn processes() -> Vec<(u32, u32, String)> {
    let Ok(listed) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    listed
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            // `PID (COMM) STATE PPID ...`: COMM may hold spaces and parentheses, so it ends at
            // the last `)`.
            let (head, rest) = stat.rsplit_once(')')?;
            let name = head.split_once('(')?.1.to_string();
            let ppid = rest.split_whitespace().nth(1)?.parse().ok()?;
            Some((pid, ppid, name))
        })
        .collect()
}

/// [`processes`] on macOS: every process ID (proc_listallpids(3), asked again while its
/// reply fills what it was given, which processes started meanwhile may have cut short),
/// each one's parent and name from its short BSD info.
#[cfg(target_os = "macos")]
pub fn processes() -> Vec<(u32, u32, String)> {
    let pids = loop {
        // SAFETY: no buffer: the call says how many processes there are.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        let Some(count) = usize::try_from(count).ok().filter(|count| *count > 0) else {
            return Vec::new();
        };
        let mut pids: Vec<libc::pid_t> = vec![0; count + 1];
        let Ok(room) = libc::c_int::try_from(size_of_val(pids.as_slice())) else {
            return Vec::new();
        };
        // SAFETY: a buffer of `room` bytes for the call to fill.
        let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), room) };
        let filled = usize::try_from(filled).unwrap_or(0);
        if filled < pids.len() {
            pids.truncate(filled);
            break pids;
        }
    };
    let size = libc::c_int::try_from(size_of::<libc::proc_bsdshortinfo>()).unwrap_or(0);
    pids.into_iter()
        .filter_map(|pid| {
            // SAFETY: proc_bsdshortinfo is plain C data, for which all zeroes is a value.
            let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
            // SAFETY: a buffer of `size` bytes for the call to fill.
            let filled = unsafe {
                libc::proc_pidinfo(pid, libc::PROC_PIDT_SHORTBSDINFO, 0, (&raw mut info).cast(), size)
            };
            if filled != size {
                return None;
            }
            let comm: Vec<u8> = info
                .pbsi_comm
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as u8)
                .collect();
            Some((
                info.pbsi_pid,
                info.pbsi_ppid,
                String::from_utf8_lossy(&comm).into_owned(),
            ))
        })
        .collect()
}

/// The processes of `listed` ([`processes`]) in one of process groups `groups`, as
/// getpgid(2) reads each now (POSIX.1-2024, XSH getpgid): one gone since it was listed is
/// left out.
#[cfg(unix)]
pub fn grouped<'a>(
    listed: &'a [(u32, u32, String)],
    groups: &'a [u32],
) -> impl Iterator<Item = &'a (u32, u32, String)> + 'a {
    listed.iter().filter(|(pid, ..)| {
        let Ok(id) = libc::pid_t::try_from(*pid) else {
            return false;
        };
        // SAFETY: getpgid(2) of a process ID: it reads the kernel's record alone.
        let group = unsafe { libc::getpgid(id) };
        u32::try_from(group).is_ok_and(|group| groups.contains(&group))
    })
}

/// Runs `cmd` as built, `shards` itself ([`run_shards_with`]) or a program that runs it
/// (given [`as_tests_run`]'s environment), its output collected, for as long as it goes on:
/// a wait on its party ([`Party`]) judged by its kind's record ([`WaitKind`]: its caller's
/// call site), as D215's judge judges a wait ([`Wait`]). Its kind cold, it follows its
/// client's liveness alone; once its kind has an answer, it is looked at each W* past its
/// start, and the test fails, `asked` naming the command, at a look that finds its party
/// made no progress at all since the look before, its host not holding it (the person's
/// ruling of 2026-11-13, D209). Run as its thread unwinds a test's failure (a cleanup's
/// `Drop`), a refusal is said on stderr and comes back as a run of no status, its account
/// its stderr: a panic there would abort the whole test binary, every other test of it
/// with it (#77: one of context.rs' did).
#[track_caller]
pub fn run_command(cmd: Command, asked: &[String]) -> Run {
    let start = Instant::now();
    match run_ending(cmd, asked, None, Leftovers::Ended) {
        Ok(run) => run,
        Err(ended) if std::thread::panicking() => {
            let _ = writeln!(io::stderr(), "{} (as its test unwound)", ended.said);
            Run {
                status: None,
                signal: None,
                stdout: String::new(),
                stderr: ended.said,
                elapsed: start.elapsed(),
            }
        }
        Err(ended) => panic!("{}", ended.said),
    }
}

/// Whether child `pid` has ended, left unreaped for a look to read its CPU through its end:
/// waitid(2) with WNOWAIT (POSIX.1-2024, XSH waitid), which leaves it waitable.
#[cfg(unix)]
fn ended_unreaped(pid: u32) -> bool {
    let id = libc::id_t::from(pid);
    // SAFETY: siginfo_t is plain C data, for which all zeroes is a value.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: waitid(2) of this process's own child into a siginfo_t of ours; WNOWAIT leaves
    // it to be reaped by its `Child`.
    let waited = unsafe {
        libc::waitid(
            libc::P_PID,
            id,
            &raw mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    // With WNOHANG and no child ended, waitid(2) leaves si_pid 0.
    #[cfg(target_os = "linux")]
    // SAFETY: the si_pid of a siginfo_t waitid(2) filled for a child's end.
    let ended = unsafe { info.si_pid() } != 0;
    #[cfg(not(target_os = "linux"))]
    let ended = info.si_pid != 0;
    waited == 0 && ended
}

/// What a run ended at its end or its stall leaves of the processes under it ([`end_run`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Leftovers {
    /// Ended by the harness with it, on Windows: a test that fails on a stall leaves no
    /// process, whatever the run started (a daemon among them).
    Ended,
    /// Waited for, as the run's own end should end them: those still running then are
    /// [`Ended::left`], and only then ended by the harness.
    Awaited,
}

/// A run [`run_until`] ended at its end, or one ended on its stall: what it had said and
/// shown ([`stuck`]), the processes it had started (those under it, each named), and those
/// of them still running once it was ended and they were waited for.
pub struct Ended {
    pub said: String,
    pub started: Vec<Process>,
    pub left: Vec<Process>,
}

/// A process by its ID and its executable's name.
pub type Process = (u32, String);

/// [`run_command`]'s run, ended at `end` past its start, a time of its test's own (boot.rs:
/// this host's own time for its guest to answer), as [`Ended`] says rather than failing the
/// test there, for a test of what an ended run leaves: the processes under it are left to
/// end as its own end ends them ([`Leftovers::Awaited`]). Until then its wait is
/// [`run_command`]'s.
#[track_caller]
pub fn run_until(cmd: Command, asked: &[String], end: Duration) -> Result<Run, Ended> {
    run_ending(cmd, asked, Some(end), Leftovers::Awaited)
}

/// [`run_command`]'s wait, ended at `end` where its test gave one ([`run_until`]) or at its
/// stall, the processes under it then as `leftovers` says. Its output is read to its end, as
/// long as the wait goes on: a stream another process holds past the client (a VM given the
/// client's stdio), which the wait finds doing nothing of its own past the slowest answer
/// of its kind, is said held open, with what came of it.
#[track_caller]
fn run_ending(
    mut cmd: Command,
    asked: &[String],
    end: Option<Duration>,
    leftovers: Leftovers,
) -> Result<Run, Ended> {
    let kind = WaitKind::command(
        std::panic::Location::caller(),
        &command_words(&cmd),
        &keyed_line(&cmd),
    );
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
    // Read as it comes ([`collect`]): a command that never ends shows how far it got. Each
    // pipe probed by a second handle of its read end ([`taken_whole`]).
    let (stdout, stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let (out_probe, err_probe) = (probe(&stdout), probe(&stderr));
    let out = collect(Box::new(stdout));
    let err = collect(Box::new(stderr));
    // What it wrote, each stream with whether it has ended, its every writer gone.
    let (mut out_got, mut err_got) = ((Vec::new(), false), (Vec::new(), false));
    let came = more;
    // Lossily: a step's output is printed as it came, bytes that are no UTF-8 included,
    // which a strict conversion would drop the whole of.
    let text = |got: &(Vec<u8>, bool)| {
        let said = String::from_utf8_lossy(&got.0).into_owned();
        if got.1 {
            said
        } else {
            format!("{said}\n(still held open by another process)")
        }
    };
    let mut wait = Wait::begin(kind, child.id(), home.as_deref(), &asked.join(" "));
    // Its answer, its client's end, is missing as it begins: its client started just now,
    // so an end its first check finds before any look is its party's answer, which teaches
    // its kind, never one there before it.
    wait.absent();
    let mut status = None;
    loop {
        // Its client read as it ends, before it is reaped, for its work through its answer:
        // on Unix while it is a zombie, on Windows through the handle its watch holds.
        #[cfg(unix)]
        if status.is_none() && ended_unreaped(child.id()) {
            wait.read_through();
        }
        if status.is_none()
            && let Some(st) = child.try_wait().unwrap()
        {
            #[cfg(windows)]
            wait.read_through();
            // Its client's own end: its kind's answer, taught where it served.
            if st.success() {
                wait.answered();
            } else {
                wait.unserved();
            }
            status = Some(st);
        }
        let output = came(&out, &mut out_got) | came(&err, &mut err_got);
        if let Some(status) = status.filter(|_| out_got.1 && err_got.1) {
            break Ok(finished(status, text(&out_got), text(&err_got), start));
        }
        if status.is_none()
            && let Some(end) = end.filter(|end| start.elapsed() >= *end)
        {
            let seen = stuck(child.id(), home.as_deref());
            let (started, left) = end_run(&mut child, end, leftovers);
            came(&out, &mut out_got);
            came(&err, &mut err_got);
            return Err(Ended {
                said: format!(
                    "{} did not exit within {end:?}, its end\n--- stdout\n{}\n--- stderr\n{}{seen}",
                    asked.join(" "),
                    text(&out_got),
                    text(&err_got)
                ),
                started,
                left,
            });
        }
        match (wait.look(output), status) {
            (Ok(()), _) => {}
            // Its streams' end never came, their holder doing nothing of its own past the
            // slowest answer of its kind: what came is what it said, but the whole of a stream
            // no process holds any more, however late its reader ([`taken_whole`]).
            (Err(_), Some(status)) => {
                taken_whole(&out, &out_probe, &mut out_got);
                taken_whole(&err, &err_probe, &mut err_got);
                break Ok(finished(status, text(&out_got), text(&err_got), start));
            }
            (Err(stall), None) => {
                let said = wait.refused(&stall);
                let seen = stuck(child.id(), home.as_deref());
                let (started, left) = end_run(&mut child, stall.slowest(), leftovers);
                came(&out, &mut out_got);
                came(&err, &mut err_got);
                return Err(Ended {
                    said: format!(
                        "{said}\n--- stdout\n{}\n--- stderr\n{}{seen}",
                        text(&out_got),
                        text(&err_got)
                    ),
                    started,
                    left,
                });
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A run that ended with `status`, its streams as they came.
fn finished(status: std::process::ExitStatus, stdout: String, stderr: String, start: Instant) -> Run {
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal = None;
    Run {
        status: status.code(),
        signal,
        stdout,
        stderr,
        elapsed: start.elapsed(),
    }
}

/// The words that name `cmd`'s command, which key its wait's kind beside its call site
/// ([`WaitKind::of`]), so that one call site's quick refusal never bounds its slow build or
/// push (#77: build.rs' `policy_eval_answers` and `builds_push_what_they_build` ran many
/// commands from one closure, a W* of 13.4 ms and 3.49 ms learned from quick ones judging
/// the rest). They are its program's file stem, then its subcommand's words, each a plain
/// word (ASCII lowercase letters and `-`, no flag) up to the first that is not:
/// - its first argument;
/// - its second, only where its first names no command of the CLI's that takes arguments
///   itself (shards-cmdline's catalog: `run`, `push`, `build`, whose next word is an image,
///   a name or a path), or shards' grammar reads the two as ACTION THING (`run vm`, `stop
///   daemon`, `grammar::form`): `image ls`, `compose up`, `buildx build`, `system df`.
///
/// So `shards run --rm alpine` is `shards run`, `docker push localhost:5000/x` is `docker
/// push`, `shards stop daemon` is `shards stop daemon`, and `docker compose up -d` is `docker
/// compose up`. None of a run's own names, paths or tags is ever a word: each run of a call
/// site keys the same kinds, whose records it keeps.
pub fn command_words(cmd: &Command) -> String {
    let program = Path::new(cmd.get_program())
        .file_stem()
        .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
    let plain = |word: &&std::ffi::OsStr| {
        word.to_str().is_some_and(|w| {
            !w.is_empty() && !w.starts_with('-') && w.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
        })
    };
    let mut args = cmd.get_args();
    let mut words = vec![program];
    let Some(first) = args.next().filter(plain).and_then(std::ffi::OsStr::to_str) else {
        return words.join(" ");
    };
    words.push(first.to_string());
    let takes_arguments = shards_cmdline::catalog::TOP
        .iter()
        .flat_map(|group| group.entries)
        .any(|entry| entry.name == first && entry.command.is_some());
    if let Some(second) = args.next().filter(plain).and_then(std::ffi::OsStr::to_str)
        && (!takes_arguments
            || shards_cmdline::grammar::form(&[first.to_string(), second.to_string()]).is_some())
    {
        words.push(second.to_string());
    }
    words.join(" ")
}

/// A value this run minted of its own, which a command's line carries (a directory, a port,
/// a name made of this process's ID), and the placeholder its kind's key says it as
/// ([`keyed_line`]): each registered by the helper that mints it ([`minted`]), so that
/// every run of a call site keys its commands as the run before did (ROOT's ruling (a),
/// 2026-11-13, #77).
struct Minted {
    value: String,
    placeholder: String,
    /// A number (a port, a process ID), found only where no digit is beside it.
    number: bool,
}

/// What this run has minted, longest first, so that a directory goes before the root it is
/// under and a value inside another is said as the outer one's placeholder.
static MINTED: std::sync::Mutex<Vec<Minted>> = std::sync::Mutex::new(Vec::new());

fn minted_values() -> std::sync::MutexGuard<'static, Vec<Minted>> {
    MINTED.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Registers `value`, minted by this run, as `placeholder` in its commands' keys.
pub fn minted(value: &str, placeholder: &str) {
    if value.is_empty() {
        return;
    }
    let mut minted = minted_values();
    if minted.iter().any(|m| m.value == value) {
        return;
    }
    minted.push(Minted {
        value: value.to_string(),
        placeholder: placeholder.to_string(),
        number: value.bytes().all(|b| b.is_ascii_digit()),
    });
    minted.sort_by_key(|m| std::cmp::Reverse(m.value.len()));
}

/// No longer minted: a directory gone with its [`TempDir`].
fn unminted(value: &str) {
    minted_values().retain(|m| m.value != value);
}

/// `port`, a port this run was handed (a listener bound at port 0, [`fixed_port`]),
/// registered as `{port}` in its commands' keys ([`minted`]).
pub fn minted_port(port: u16) -> u16 {
    minted(&port.to_string(), "{port}");
    port
}

/// `cmd`'s whole command line as its wait's kind is keyed by it beside its call site
/// ([`WaitKind::of`]): its program's file stem and every argument, each value this run
/// minted said as its placeholder ([`minted`]): a [`TempDir`] as `{dir NAME}`, the
/// directory they are all under as `{testdir}`, a port a helper was handed as `{port}`, and
/// this process's ID, which names the slices, builders and pipes tests make, as `{pid}`.
/// So one call site's quick refused command is kept apart from its real one (build.rs'
/// `build --push CTX` and `build --push -t localhost:PORT/team/built:1 CTX`), and each run
/// keys its commands as the run before did, adding no record of its own.
pub fn keyed_line(cmd: &Command) -> String {
    minted(&std::process::id().to_string(), "{pid}");
    let program = Path::new(cmd.get_program())
        .file_stem()
        .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
    let line: Vec<String> = std::iter::once(program)
        .chain(cmd.get_args().map(|arg| generic(&arg.to_string_lossy())))
        .collect();
    format!("{line:?}")
}

/// `text`, each value this run minted said as its placeholder.
fn generic(text: &str) -> String {
    let mut said = text.to_string();
    for minted in minted_values().iter() {
        said = if minted.number {
            number_said(&said, &minted.value, &minted.placeholder)
        } else {
            said.replace(&minted.value, &minted.placeholder)
        };
    }
    said
}

/// `text`, each occurrence of the number `value` with no digit beside it said as
/// `placeholder`.
fn number_said(text: &str, value: &str, placeholder: &str) -> String {
    let mut said = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(value) {
        let (before, from) = rest.split_at(at);
        let after = from.get(value.len()..).unwrap_or_default();
        let digit_before = before
            .chars()
            .next_back()
            .or_else(|| said.chars().next_back())
            .is_some_and(|c| c.is_ascii_digit());
        let alone = !digit_before && !after.starts_with(|c: char| c.is_ascii_digit());
        said.push_str(before);
        said.push_str(if alone { placeholder } else { value });
        rest = after;
    }
    said.push_str(rest);
    said
}

/// `text`, a guest's line a wait awaits and what it was asked for it ([`Heard::console`]),
/// as its wait's kind is keyed by it beside its call site: each value this run minted said
/// as its placeholder ([`generic`]), then each run of digits as `{n}`. One call site's waits for different stages are kept apart by their words (#77,
/// ROOT's KVM run c33: hotplug.rs' `ask` waited for `SHARDS-PLUG WAIT 1`, a grow, and then
/// `WAIT 2`, a rescan and a disk's first read, as one kind, whose W* the grow taught); the
/// numbers a stage carries (its step, a PCI address, a size, a salt) say no other work, and
/// a run of more rounds keys its stages as a run of fewer did.
pub fn guest_text(text: &str) -> String {
    let mut said = String::with_capacity(text.len());
    let mut digits = false;
    for c in generic(text).chars() {
        let digit = c.is_ascii_digit();
        if !digit {
            said.push(c);
        } else if !digits {
            said.push_str("{n}");
        }
        digits = digit;
    }
    said
}

/// A kind of the tests' own waits: the call site that waits ([`run_command`]'s caller,
/// `#[track_caller]` through its wrappers) and, for a wait on a command, its command's words
/// ([`command_words`]), with what its answers have taught D215's record:
/// W*, the slowest, from a wait's start to its client's own end, and R*, the least rate its
/// party ran at meanwhile, a refusal's diagnosis and never its bound. None on a host until
/// its first answer there; then kept under `target/judge-records`, by the host and the call
/// site, so that a later run on the host begins from what it learned (ROOT, 2026-11-13: a
/// record learned on the host is data, not a seed), and taught again by each answer: a
/// command's by each served one alone ([`Wait::unserved`], [`SERVED`]). The
/// person's ruling of 2026-11-13 holds each of them: "No record seeds. The cold start, and
/// every wait after a run-out, follows the party's liveness and progress. After that the
/// bound is the slowest answer seen. No ×2."
pub struct WaitKind {
    site: &'static std::panic::Location<'static>,
    words: String,
    line: String,
    taught: Taught,
    record: shards_api::Record,
    file: PathBuf,
}

impl WaitKind {
    /// The kind of `site`, a wait on no command: its record taught what this host kept of it
    /// the first time a wait of it begins in this process.
    pub fn at(site: &'static std::panic::Location<'static>) -> &'static WaitKind {
        WaitKind::of(site, "", "")
    }

    /// The kind of `site` waiting for `awaited` ([`STATE`], [`PROGRESS`], [`STREAM`]): kept
    /// apart from its call site's other waits, as a `#[track_caller]` helper's waits share
    /// their caller's site (#77: managed_lifecycle.rs' `challenge_healthy`, whose
    /// `published` state, there before its wait began, taught its stream's wait a W* of
    /// 3.527 µs).
    pub fn awaiting(site: &'static std::panic::Location<'static>, awaited: &str) -> &'static WaitKind {
        WaitKind::of(site, awaited, "")
    }

    /// The kind of `site` waiting on the command whose words are `words` ([`command_words`])
    /// and whose whole line is `line` ([`keyed_line`]), kept apart from its call site's
    /// other commands'. Its record is kept beside the call site's own
    /// (`FILE-LINE-COLUMN-WORDS-HASH`, HASH the first 16 hex digits of the line's SHA-256, a
    /// name of bounded length whatever the line): one a host kept before commands were told
    /// apart, keyed by its call site alone, is read by waits on no command still, and a
    /// command's kind is cold on a host until its own first answer there.
    pub fn command(
        site: &'static std::panic::Location<'static>,
        words: &str,
        line: &str,
    ) -> &'static WaitKind {
        WaitKind::taught(site, words, line, Taught::Served)
    }

    /// The kind of `site` waiting for `awaited` keyed by `line` (a guest's text,
    /// [`guest_text`]), or for nothing where both are empty: taught by every answer.
    pub fn of(site: &'static std::panic::Location<'static>, words: &str, line: &str) -> &'static WaitKind {
        WaitKind::taught(site, words, line, Taught::Every)
    }

    fn taught(
        site: &'static std::panic::Location<'static>,
        words: &str,
        line: &str,
        taught: Taught,
    ) -> &'static WaitKind {
        type Kind = (&'static str, u32, u32, String, String, Taught);
        static KINDS: std::sync::Mutex<std::collections::BTreeMap<Kind, &'static WaitKind>> =
            std::sync::Mutex::new(std::collections::BTreeMap::new());
        let mut kinds = KINDS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        kinds
            .entry((
                site.file(),
                site.line(),
                site.column(),
                words.to_string(),
                line.to_string(),
                taught,
            ))
            .or_insert_with(|| {
                let kind: &'static WaitKind = Box::leak(Box::new(WaitKind {
                    site,
                    words: words.to_string(),
                    line: line.to_string(),
                    taught,
                    record: shards_api::Record::new(),
                    file: WaitKind::kept_at(site, words, line),
                }));
                kind.learn_kept();
                kind
            })
    }

    /// Where this host keeps the record of [`of`](Self::of)'s kind: a test of a cold kind's
    /// waits removes it before that kind's first wait in its process.
    pub fn kept_at(site: &std::panic::Location<'_>, words: &str, line: &str) -> PathBuf {
        let mut name = format!("{}-{}-{}", site.file(), site.line(), site.column());
        if !words.is_empty() {
            name.push('-');
            name.push_str(words);
        }
        if !line.is_empty() {
            let hash = sha2::Sha256::digest(line.as_bytes());
            name.push('-');
            name.extend(hash.iter().take(8).map(|b| format!("{b:02x}")));
        }
        let name: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        workspace()
            .join("target/judge-records")
            .join(host_class())
            .join(name)
    }

    /// What its waits have taught it.
    pub fn record(&self) -> &shards_api::Record {
        &self.record
    }

    /// Teaches its record what this host kept of it, W*, and R*, C* and the count of answers
    /// that taught them where they were rated ([`RATED`]), each as it was kept
    /// (`shards_api::Record::merge`). Nothing where
    /// nothing whole is kept (one kept before answers were rated, W* and R* alone, leaves its
    /// kind cold until its next answer rewrites it: its R* may be of none read, 0 ns), and,
    /// for a command's kind, nothing that does not say it is of served answers alone
    /// ([`SERVED`]).
    fn learn_kept(&self) {
        let kept = std::fs::read_to_string(&self.file).unwrap_or_default();
        let mut words = kept.split_whitespace();
        if self.served_only() && words.next() != Some(SERVED) {
            return;
        }
        let ns = |word: Option<&str>| word.and_then(|n| n.parse().ok()).map(Duration::from_nanos);
        let Some(slowest) = ns(words.next()) else {
            return;
        };
        let rated = match words.next() {
            None => None,
            Some(RATED) => {
                let (cpu, over, most) = (ns(words.next()), ns(words.next()), ns(words.next()));
                // One kept before answers were counted is one answer's, until its next rated
                // answer counts it two (D215, ROOT's ruling of 2026-11-17).
                let answers = match words.next() {
                    None => Some(1),
                    Some(n) => n.parse::<u64>().ok().filter(|&n| n > 0),
                };
                match (cpu, over, most, answers, words.next()) {
                    (Some(cpu), Some(over), Some(most), Some(answers), None) => Some(shards_api::Rated {
                        least: shards_api::Rate { cpu, over },
                        most,
                        answers,
                    }),
                    _ => return,
                }
            }
            Some(_) => return,
        };
        self.record.merge(shards_api::Seen::Learned { slowest, rated });
    }

    /// Whether it is a command's kind, taught by its served answers alone ([`Wait::unserved`]).
    fn served_only(&self) -> bool {
        self.taught == Taught::Served
    }

    /// A kind of its own whose record is kept at `file`: two of them stand for two
    /// processes' kinds of one call site, for the tests of [`keep`](Self::keep).
    #[track_caller]
    pub fn apart(file: PathBuf) -> WaitKind {
        WaitKind {
            site: std::panic::Location::caller(),
            words: String::new(),
            line: String::new(),
            taught: Taught::Every,
            record: shards_api::Record::new(),
            file,
        }
    }

    /// Keeps its record on this host: what another process kept of it since taught in
    /// first, then written whole and renamed into place, the whole of it under the record's
    /// lock (waits-11), so that no other keep reads between this one's read and its rename
    /// and then renames a merge that lacks this one's answer over it. A keep that fails is
    /// the next answer's to make: what this process learned stays in its memory meanwhile.
    pub fn keep(&self) {
        let _ = self.keep_between(true, || {});
    }

    /// [`keep`](Self::keep), `between` called once what was kept is read and merged, before
    /// its merge is written; where `wait` is false, nothing kept and `false` returned while
    /// another keep holds the record.
    pub fn keep_between(&self, wait: bool, between: impl FnOnce()) -> std::io::Result<bool> {
        let (Some(dir), Some(name)) = (self.file.parent(), self.file.file_name()) else {
            return Ok(false);
        };
        std::fs::create_dir_all(dir)?;
        let mut locked = std::ffi::OsString::from(".");
        locked.push(name);
        locked.push(".lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(locked))?;
        if wait {
            lock.lock()?;
        } else {
            match lock.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => return Ok(false),
                Err(std::fs::TryLockError::Error(e)) => return Err(e),
            }
        }
        let kept = self.merged_into_place(dir, name, between);
        // Let go of before its file closes: a lock is its open file's, and a program another
        // thread starts meanwhile would hold it until its exec (M244).
        let unlocked = lock.unlock();
        kept.and(unlocked).map(|()| true)
    }

    /// What another process kept taught in, `between` called, and the merge written whole and
    /// renamed into place: by a name of its own, this process's and this keep's, as two of
    /// its threads may keep their own kinds at once.
    fn merged_into_place(
        &self,
        dir: &Path,
        name: &std::ffi::OsStr,
        between: impl FnOnce(),
    ) -> std::io::Result<()> {
        static KEEPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        self.learn_kept();
        let shards_api::Seen::Learned { slowest, rated } = self.record.seen() else {
            return Ok(());
        };
        between();
        let ns = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        let mut temporary = std::ffi::OsString::from(".");
        temporary.push(name);
        temporary.push(format!(
            ".{}.{}",
            std::process::id(),
            KEEPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let temporary = dir.join(temporary);
        let tag = if self.served_only() {
            format!("{SERVED} ")
        } else {
            String::new()
        };
        std::fs::write(
            &temporary,
            match rated {
                Some(rated) => format!(
                    "{tag}{} {RATED} {} {} {} {}\n",
                    ns(slowest),
                    ns(rated.least.cpu),
                    ns(rated.least.over),
                    ns(rated.most),
                    rated.answers
                ),
                None => format!("{tag}{}\n", ns(slowest)),
            },
        )?;
        std::fs::rename(&temporary, &self.file)
    }
}

/// The first word of a command's kept record ([`WaitKind::keep`]): its W*, R* and C* are of
/// served answers alone ([`Wait::unserved`]). A record kept without it, before a refusal
/// taught nothing, may hold a refusal's answer (adacachy2's `shards run` record of
/// containers.rs' `a_logs_history_is_what_its_user_set`, W* 5.4 ms), and is read as none:
/// its kind cold until its next served answer rewrites it.
const SERVED: &str = "served";

/// The word before a kept record's R*, C* and the count of the answers that taught them
/// ([`WaitKind::keep`]): of answers whose work was read through their end alone
/// (`shards_api::Rated`). A record kept without it has none; one kept with no count, before
/// answers were counted, is one answer's (D215, ROOT's ruling of 2026-11-17).
const RATED: &str = "rated";

/// Which answers teach a [`WaitKind`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Taught {
    /// Each: a state, a line, a stream's bytes, a call's return.
    Every,
    /// A command's served ones alone, its client's status 0 ([`Wait::unserved`], [`SERVED`]).
    Served,
}

/// What a [`Patience`] awaits, and keys its kind by ([`WaitKind::awaiting`]).
pub const STATE: &str = "a state its party comes to";
/// What a [`Heard`] awaits.
pub const PROGRESS: &str = "its guest's progress";
/// What a [`Following`] awaits.
pub const STREAM: &str = "a guest's stream";

/// This host model's vCPU counts and what each came to (D261, `shards_vmm::vm::limits`),
/// kept beside the judge's records (`target/judge-records`, under this host's class), which CI
/// keeps from run to run: each its CPU's model and what it runs under, so that one image's
/// runners of two models (CI's x86_64 runners: AMD on Azure, Intel on EC2) keep theirs apart.
pub fn vcpu_limits() -> shards_vmm::vm::limits::Limits {
    shards_vmm::vm::limits::Limits::at(
        workspace()
            .join("target/judge-records")
            .join(host_class())
            .join("vcpus"),
        shards_vmm::vm::limits::HostModel::here(),
    )
}

/// The most vCPUs a machine is tried at here, of `most` (D261): `most`, unless this host
/// model has only refused that many, then its known-good count, or, while it knows none, one
/// less than the least count it has only refused.
pub fn vcpus_admitted(most: u32) -> u32 {
    let limits = vcpu_limits();
    match limits.admit(most) {
        Ok(()) => most,
        Err(only) => limits
            .known_good()
            .unwrap_or_else(|| only.from.saturating_sub(1))
            .min(most)
            .max(1),
    }
}

/// This host, as the records of its waits are kept for it ([`WaitKind`]): its OS, its
/// architecture and its logical CPUs, which tell two machines' own target directories apart
/// and which every runner of one CI image shares, whose cached target a CI cache restores.
fn host_class() -> String {
    format!(
        "{}-{}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism().map_or(0, std::num::NonZero::get)
    )
}

/// What a wait on a command follows, the ruling's party: what the command prints (its
/// caller's to tell, [`Wait::look`]), what its home's daemon says ([`Said`]), and the
/// processes that serve it ([`Members`]), each followed by D215's readers
/// (`shards_api::Watch`) from the look that first finds it on, their CPU read as its
/// [`Cpu`] says.
pub struct Party {
    members: Members,
    home: Option<PathBuf>,
    said: Option<Said>,
    watched: std::collections::BTreeMap<u32, shards_api::Watch>,
    cpu: Cpu,
    /// Whether it has been looked at: its first look has nothing before it.
    looked: bool,
    /// A machine of this process's: what this process's threads beside its waiter ran.
    beside: Option<Beside>,
    /// What this process's serving threads had run at the look before ([`Server`]).
    served: Option<u64>,
    /// Each member's parent and name as its last look's listing gave them ([`processes`]),
    /// for its account: which of them a refusal's last progress was (b24: a run's VM
    /// process, its daemon's spawner's child, told from the client's by its name alone).
    kin: std::collections::BTreeMap<u32, (u32, String)>,
    /// The destination of the delivery of a run's log its wait reads, as the run's options
    /// name it ([`Wait::delivery_to`]); none for a wait on anything else.
    delivery: Option<String>,
}

/// Process `pid`'s status as its OS gives it: Linux its `/proc/<pid>/stat` state (field 3,
/// proc(5)), macOS its `pbsi_status` (`PROC_PIDT_SHORTBSDINFO`, sys/proc.h's `SIDL` to
/// `SZOMB`; `SRUN` for any live process, sleeping or not: HVF's accounts said "running
/// (status 2), 0 of 12 threads runnable" of an idle daemon), each said with its letter or
/// number; none where it cannot be read (Windows,
/// whose process has no such state).
/// Where the threads of process `pid` wait, each by its kernel's name for the place it
/// sleeps (`/proc/<pid>/task/<tid>/wchan`, proc(5)), with how many: for a refusal's account,
/// which said a party's threads all slept but not on what (CI 43968984258, x86_64 musl:
/// engine.rs' refused create and `/system/df`, each daemon's 17 threads `S`, its looks on
/// time). None where none is read, and off Linux.
fn waits_in(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let mut counts = std::collections::BTreeMap::<String, usize>::new();
        for task in std::fs::read_dir(format!("/proc/{pid}/task")).ok()?.flatten() {
            let Ok(wchan) = std::fs::read_to_string(task.path().join("wchan")) else {
                continue;
            };
            let wchan = wchan.trim();
            // "0": a thread on a CPU, or a name its reader may not see.
            if !wchan.is_empty() && wchan != "0" {
                *counts.entry(wchan.to_string()).or_default() += 1;
            }
        }
        (!counts.is_empty()).then(|| {
            counts
                .iter()
                .map(|(wchan, n)| format!("{wchan} x{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

#[cfg(target_os = "linux")]
fn process_status(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let state = stat.rsplit_once(')')?.1.split_whitespace().next()?.to_string();
    let word = match state.as_str() {
        "R" => "running or runnable",
        "S" => "sleeping",
        "D" => "in an uninterruptible wait",
        "Z" => "a zombie",
        "T" => "stopped",
        "t" => "traced",
        "X" => "dead",
        "I" => "idle",
        _ => "in a state proc(5) does not name",
    };
    Some(format!("{word} ({state})"))
}

#[cfg(target_os = "macos")]
fn process_status(pid: u32) -> Option<String> {
    let id = libc::c_int::try_from(pid).ok()?;
    let size = libc::c_int::try_from(size_of::<libc::proc_bsdshortinfo>()).ok()?;
    // SAFETY: proc_bsdshortinfo is plain C data, for which all zeroes is a value.
    let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    // SAFETY: a buffer of `size` bytes for the call to fill.
    let filled =
        unsafe { libc::proc_pidinfo(id, libc::PROC_PIDT_SHORTBSDINFO, 0, (&raw mut info).cast(), size) };
    if filled != size {
        return None;
    }
    let word = match info.pbsi_status {
        libc::SIDL => "being created",
        // XNU keeps SRUN for every live process, a sleeping one too: its threads' run
        // states, said beside it, tell whether one runs.
        libc::SRUN => "live",
        libc::SSLEEP => "sleeping",
        libc::SSTOP => "stopped",
        libc::SZOMB => "a zombie",
        _ => "in a status sys/proc.h does not name",
    };
    Some(format!("{word} (status {})", info.pbsi_status))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_status(_: u32) -> Option<String> {
    None
}

/// The name a VM process runs as (`shards-vm`, `shards-vm-gpu`): a daemon's descendant of
/// another name is its helper (its spawner, its network process), whose run for a waiter's
/// own clients is the daemon's ([`Looked::daemon_ran`]; ROOT's batch, KVM: the polled test's
/// party progressed by a process of the daemon's serving its polls, 13 of 30 whole binaries
/// never refused).
#[cfg(unix)]
const VM_PROCESS: &str = "shards-vm";

/// Whose processes a [`Party`]'s are, besides its home's daemon and what descends from it.
#[derive(Clone)]
enum Members {
    /// Clients and what descends from them: a command [`run_command`] runs, or a `run
    /// --kernel` or `restore` (the VM process on Unix, its starter on Windows).
    Clients(Vec<u32>),
    /// This test's own VM processes, by the IDs their starts gave ([`OwnVm`]: `run
    /// --kernel`'s and `restore`'s [`Vm`], warm.rs' warm VM) and what descends from them: a
    /// [`Patience`]'s, and a guest's stream's ([`Following`]).
    Own,
    /// None: a machine this test's process runs (`vm::start`), whose vCPUs and devices are
    /// threads of this process, as is the host's work its guest waits on, and whose liveness
    /// is its caller's (its `running.wait()`): its words ([`Heard::console`]), and what this
    /// process's threads beside its waiter ran ([`Beside`]).
    Machine,
    /// Every process this test's process started and what descends from them: the run host
    /// shards-api's `ImageShards` starts and its VM process, which a call of the API's waits
    /// on ([`judged`]), the host's work before its VM starts among them.
    Started,
    /// Every process of these process groups, wherever it is listed ([`grouped`]): a pool's
    /// warm run hosts, each leading a group of its own with its VM, network and share
    /// processes in it (shards-api session.rs `start_host`), which their holder's end leaves
    /// orphans of no process of this test's.
    #[cfg(unix)]
    Groups(Vec<u32>),
}

/// What a [`Party`]'s processes' CPU time says of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cpu {
    /// Progress: any CPU at all, or an end under way (D215's look); a thread held from
    /// running by its host (runnable on no CPU, stalled on memory) its host's hold.
    Progress,
    /// Its liveness alone: a guest of more vCPUs than its host has CPUs runs them for ever,
    /// its workload stopped or not (the parity plan's step 10: a 64-vCPU storm on 4 TCG CPUs
    /// waited on 2 h 16 min, its VM process at 31,324 CPU-seconds). Only its words and bytes
    /// are its progress; its host's hold is still a hold where it ran nothing at all since
    /// the look before (D215's look finds `Held` only of a process that did not run), or
    /// was stopped or suspended.
    Liveness,
}

/// What a look at a party found since the look before ([`Party::look`]).
pub struct Looked {
    pub found: shards_api::Found,
    /// What showed it.
    pub what: String,
    /// The CPU its processes ran since the look before.
    pub cpu: Duration,
    /// The coarsest resolution of the counters it read (`shards_api::Watch::resolution`).
    pub resolution: Duration,
    /// Its home's daemon ran ([`Cpu::Progress`]), said apart from `found`: the work of its
    /// waiter's own clients where its waiter asked the daemon meanwhile ([`Wait::look`]).
    pub daemon_ran: Option<String>,
    /// Whether it saw its party's own processes run or rest unheld: none of them held from
    /// running, stopped, starved of a CPU (`shards_api::Watch::starved`), ending or found
    /// anew, its daemon within no deadline of its own that holds it ([`Party::graced`]: a
    /// run's stop grace, a log's next try to the destination it reads), its daemon saying
    /// nothing (words are
    /// progress its CPU does not show), and no progress of this process's servers', which it
    /// may be waiting on. Only such looks' windows are its judge's rate's
    /// (`shards_api::Span::work`, D215).
    pub vouched: bool,
    /// Whether a process of it had gone, or could not be read, before this look read its
    /// last CPU: what it ran since the look before is lost to its work.
    pub lost: bool,
}

impl Party {
    /// The party of client `client`, whose home is `home` where it has one.
    pub fn client(client: u32, home: Option<&Path>) -> Party {
        Party::with(Members::Clients(vec![client]), home)
    }

    /// The party of this test's own VM processes, and of `home`'s daemon where it has one.
    pub fn own(home: Option<&Path>) -> Party {
        Party::with(Members::Own, home)
    }

    /// The party of a machine this test's process runs: its words, and the CPU of this
    /// process's threads beside the one that looks ([`Beside`]).
    pub fn machine() -> Party {
        let mut party = Party::with(Members::Machine, None);
        party.beside = Some(Beside::default());
        party
    }

    /// The party of every process this test's process started, and of `home`'s daemon where
    /// it has one.
    pub fn started(home: Option<&Path>) -> Party {
        Party::with(Members::Started, home)
    }

    /// The party of every process of process groups `groups` ([`Members::Groups`]).
    #[cfg(unix)]
    pub fn groups(groups: &[u32]) -> Party {
        Party::with(Members::Groups(groups.to_vec()), None)
    }

    fn with(members: Members, home: Option<&Path>) -> Party {
        Party {
            members,
            home: home.map(Path::to_path_buf),
            said: home.map(Said::of),
            watched: std::collections::BTreeMap::new(),
            cpu: Cpu::Progress,
            looked: false,
            beside: None,
            served: None,
            kin: std::collections::BTreeMap::new(),
            delivery: None,
        }
    }

    /// Its processes' CPU read as `cpu` says from its next look on.
    pub fn read_cpu(&mut self, cpu: Cpu) {
        self.cpu = cpu;
    }

    /// Its home's daemon now, as its pid file names it by its ID and its start
    /// (`shards_ipc::Named`, D185): none where the process with that ID is not the one that
    /// started then, a crashed daemon's ID reused (#77 waits-09).
    fn daemon(&self) -> Option<u32> {
        let named = shards_ipc::Named::read(&self.home.as_ref()?.join(shards_ipc::PID))?;
        named.lives().then_some(named.pid)
    }

    /// The processes it is now: its client (or this test's own VM processes) and its
    /// daemon, and on Unix what descends from them, from one listing of the system's
    /// ([`processes`]); on Windows each is followed with what descends from it (shards-api's
    /// tree of a process, Toolhelp32). None for a machine of this process's.
    fn members(&mut self) -> (Vec<u32>, std::collections::BTreeSet<u32>) {
        let listed = match &self.members {
            Members::Machine => Vec::new(),
            Members::Clients(_) if cfg!(windows) => Vec::new(),
            Members::Clients(_) | Members::Own | Members::Started => processes(),
            #[cfg(unix)]
            Members::Groups(_) => processes(),
        };
        let own = match &self.members {
            Members::Clients(clients) => clients.clone(),
            Members::Own => own_vms(),
            Members::Machine => Vec::new(),
            Members::Started => listed
                .iter()
                .filter(|(_, ppid, _)| *ppid == std::process::id())
                .map(|(pid, ..)| *pid)
                .collect(),
            #[cfg(unix)]
            Members::Groups(groups) => grouped(&listed, groups).map(|(pid, ..)| *pid).collect(),
        };
        let daemon = self.daemon();
        let roots: Vec<u32> = own.into_iter().chain(daemon).collect();
        #[cfg(windows)]
        return (roots, std::collections::BTreeSet::new());
        #[cfg(unix)]
        {
            // Each member, and whether it is its daemon's helper: under the daemon, with no
            // VM process between them.
            let mut tree: Vec<(u32, bool)> = roots.into_iter().map(|pid| (pid, false)).collect();
            let mut i = 0;
            while let Some(&(parent, parent_helps)) = tree.get(i) {
                let under_daemon = parent_helps || Some(parent) == daemon;
                for (pid, ppid, name) in &listed {
                    if *ppid == parent && !tree.iter().any(|(member, _)| member == pid) {
                        tree.push((*pid, under_daemon && !name.starts_with(VM_PROCESS)));
                    }
                }
                i += 1;
            }
            let helpers = tree
                .iter()
                .filter(|(_, helps)| *helps)
                .map(|(pid, _)| *pid)
                .collect();
            self.kin = listed
                .into_iter()
                .filter(|(pid, ..)| tree.iter().any(|(member, _)| member == pid))
                .map(|(pid, ppid, name)| (pid, (ppid, name)))
                .collect();
            (tree.into_iter().map(|(pid, _)| pid).collect(), helpers)
        }
    }

    /// What it did since the look before: progressed where its daemon said something or a
    /// process of it ran (D215's look: any CPU at all, or an end under way), or a serving
    /// thread of this process did ([`Server`]: a test registry's, which its daemon may wait
    /// on), its CPU read as [`Cpu::Progress`]; else held where its host held one of those
    /// from running (runnable on no CPU,
    /// stalled on its memory, and so running nothing), or stopped or suspended one, which
    /// D215's look finds `Still` on Linux and a test's wait counts as its host's hold: a kiln
    /// runner's headroom guard suspends a run's processes as it paces them (ROOT,
    /// 2026-11-13); or, a process of it live, held where its daemon is within a deadline of
    /// its own that holds it, a run of its home in its stop grace or, for a wait on the
    /// delivery of a run's log, that run's shipper paused until its next try there
    /// ([`graced`](Self::graced)); else still; and gone where no process of it runs and its daemon said
    /// nothing, its liveness ended. A process found anew is judged from the look after. A
    /// machine of this process's is never gone: its liveness is its caller's. Its daemon's own
    /// run is said apart ([`Looked::daemon_ran`]), its waiter's to weigh. A look that found
    /// nothing else, being its first or one that found a process of it anew, is based
    /// (`Found::Based`): it has nothing before it to show stillness by (#77). A process once
    /// found is of it until it has gone or ended, wherever it is listed since: a daemon's
    /// microVMs as they end after it, a client's children it leaves as it exits (#77
    /// waits-10). One found ended (exited, not reaped: `shards_api::Watch::ended`) is not
    /// live, and its end is progress once, at the look that first finds it under way, never
    /// again (#77 waits-09: a zombie's every look says its end under way).
    pub fn look(&mut self) -> Looked {
        use shards_api::Found;
        let (members, helpers) = self.members();
        let daemon = self.daemon();
        let mut looked = Looked {
            found: Found::Still,
            what: String::new(),
            cpu: Duration::ZERO,
            resolution: Duration::ZERO,
            daemon_ran: None,
            vouched: true,
            lost: false,
        };
        let mut based = !std::mem::replace(&mut self.looked, true);
        if self.said.as_mut().is_some_and(Said::said) {
            looked.vouched = false;
            looked.found = Found::Progressed;
            looked.what = "its daemon said something".into();
        }
        if let Some(beside) = self.beside.as_mut() {
            let ran = beside.look();
            looked.resolution = looked.resolution.max(beside.uncertainty);
            match ran {
                Ran::Based => based = true,
                Ran::Still => {}
                Ran::Held => {
                    looked.vouched = false;
                    if looked.found == Found::Still {
                        looked.found = Found::Held;
                        looked.what =
                            "its host held a thread of this process beside its waiter from running".into();
                    }
                }
                Ran::Others(cpu) => {
                    looked.cpu = looked.cpu.saturating_add(cpu);
                    if self.cpu == Cpu::Progress && looked.found != Found::Progressed {
                        looked.found = Found::Progressed;
                        looked.what = "this process's threads beside its waiter ran".into();
                    }
                }
            }
        }
        // This process's serving threads: what any party may be waiting on (D209 Part 8).
        let (served, serving) = servers_ran();
        let mut server_held = false;
        if let Some(before) = self.served.replace(served)
            && self.cpu == Cpu::Progress
        {
            if served > before && looked.found != Found::Progressed {
                looked.found = Found::Progressed;
                looked.what = "this process's serving threads ran".into();
            }
            server_held = served == before && serving;
            looked.vouched &= served == before && !server_held;
        }
        // Whether a process of it was found live: one found anew, or one still to be read.
        let mut live = false;
        let mut found_anew = Vec::new();
        for pid in members {
            if self.watched.contains_key(&pid) {
                continue;
            }
            let watch = shards_api::Followed::of_pid(pid)
                .ok()
                .flatten()
                .and_then(|followed| shards_api::Watch::of(followed).ok().flatten());
            // Its daemon named again once read: the ID its file names is still the process
            // that started then, so the one read is it.
            let Some(watch) =
                watch.filter(|w| !w.ended() && (daemon != Some(pid) || self.daemon() == daemon))
            else {
                continue;
            };
            looked.resolution = looked.resolution.max(watch.resolution());
            self.watched.insert(pid, watch);
            found_anew.push(pid);
            live = true;
            based = true;
        }
        let mut gone = Vec::new();
        for (&pid, watch) in &mut self.watched {
            if found_anew.contains(&pid) {
                continue;
            }
            looked.resolution = looked.resolution.max(watch.resolution());
            let before = watch.cpu();
            let was_ended = watch.ended();
            let found = watch.look();
            if matches!(found, Ok(Found::Gone)) || (was_ended && watch.ended()) {
                gone.push(pid);
                looked.vouched = false;
                looked.lost |= matches!(found, Ok(Found::Gone));
                continue;
            }
            looked.lost |= found.is_err();
            looked.cpu = looked.cpu.saturating_add(watch.cpu().saturating_sub(before));
            live |= matches!(found, Ok(Found::Progressed | Found::Held | Found::Still));
            // A party whose CPU is its liveness alone ran, some threads of it, while the one
            // its answer waits on may have sat runnable on no CPU: its host's hold
            // (`shards_api::Watch::starved`), read at each look so its window is theirs.
            // Read under either CPU, as the rate's window needs it (D215).
            let starved = watch.starved();
            looked.vouched &= found
                .as_ref()
                .is_ok_and(|&found| watch.vouched_by(found, starved));
            match (found, self.cpu) {
                (Ok(Found::Progressed | Found::Still), Cpu::Liveness)
                    if starved && looked.found == Found::Still =>
                {
                    looked.found = Found::Held;
                    looked.what = format!("its host held a thread of process {pid} from any CPU");
                }
                (Ok(Found::Progressed), Cpu::Progress) if daemon == Some(pid) => {
                    looked.daemon_ran = Some(format!("its daemon {pid} ran"));
                }
                (Ok(Found::Progressed), Cpu::Progress) if helpers.contains(&pid) => {
                    looked
                        .daemon_ran
                        .get_or_insert_with(|| format!("its daemon's process {pid} ran"));
                }
                (Ok(Found::Progressed), Cpu::Progress) if looked.found != Found::Progressed => {
                    looked.found = Found::Progressed;
                    looked.what = format!("process {pid} ran");
                }
                (Ok(Found::Held), _) if looked.found == Found::Still => {
                    looked.found = Found::Held;
                    looked.what = format!("its host held process {pid} from running");
                }
                (Ok(Found::Still), _) if watch.stopped() && looked.found == Found::Still => {
                    looked.found = Found::Held;
                    looked.what = format!("process {pid} stopped or suspended");
                }
                _ => {}
            }
        }
        for pid in gone {
            self.watched.remove(&pid);
        }
        if !found_anew.is_empty() {
            looked.vouched = false;
        }
        if server_held && looked.found == Found::Still {
            looked.found = Found::Held;
            looked.what = "its host held a serving thread of this process from running".into();
        }
        // A VM process of it in its launch, the host's work before its own code (PM M166):
        // its one thread waits on amfid's, syspolicyd's and secinitd's work asleep, so it is
        // followed by its liveness alone until its launch is over, as the daemon follows one
        // before its first request (daemon.rs `launch`). Read only of a look that would find
        // it still.
        #[cfg(target_os = "macos")]
        if live
            && looked.found == Found::Still
            && let Some(pid) = processes().into_iter().find_map(|(pid, _, name)| {
                (self.watched.contains_key(&pid)
                    && name.starts_with(VM_PROCESS)
                    && shards_vmm::platform::in_launch(pid) == Some(true))
                .then_some(pid)
            })
        {
            looked.found = Found::Held;
            looked.what = format!("its VM process {pid} in its launch, the host's to admit (PM M166)");
        }
        if live && let Some((run, what, left)) = self.graced() {
            looked.vouched = false;
            if looked.found == Found::Still {
                looked.found = Found::Held;
                looked.what = format!("its daemon in run {run}'s {what}, {left:?} more");
            }
        }
        if !live && looked.found == Found::Still && !matches!(self.members, Members::Machine) {
            looked.found = Found::Gone;
            looked.what = "none of it runs".into();
        } else if based && looked.found == Found::Still {
            looked.found = Found::Based;
            looked.what = "its first look at a process of it".into();
        }
        looked
    }

    /// The deadline of its home's daemon that holds it and has longest to go: the run it is
    /// of, what it is, and how long. The daemon keeps each in its run's directory while it
    /// holds, and before it nothing need run, so none is a stall (D209): a run's stop grace
    /// (`shards_ipc::GRACE`), its SIGKILL, before which the daemon and that run may run
    /// nothing at all, as a command that ignores its stop signal keeps them, which holds
    /// every wait on the daemon; and a run's log's shipper's next try (`shards_ipc::RETRY`),
    /// before which the shipper sleeps, its receiver having failed it, as a server not yet
    /// up does, which holds up the delivery to the destination it names alone, and so holds
    /// only a wait on that delivery ([`delivery`](Self::delivery), Part 10). None past every
    /// one, nor where none is kept.
    fn graced(&self) -> Option<(String, String, Duration)> {
        let now = std::time::SystemTime::now();
        std::fs::read_dir(self.home.as_ref()?.join("containers"))
            .ok()?
            .filter_map(Result::ok)
            .flat_map(|run| {
                let dir = run.path();
                let grace = shards_ipc::deadline_in(&dir, shards_ipc::GRACE)
                    .map(|(due, _)| (due, "stop grace".to_string()));
                let retry = shards_ipc::deadline_in(&dir, shards_ipc::RETRY).and_then(|(due, to)| {
                    let to = to.filter(|to| self.delivery.as_ref() == Some(to))?;
                    Some((due, format!("log's pause before its next try to {to}")))
                });
                let name = run.file_name().to_string_lossy().into_owned();
                [grace, retry]
                    .into_iter()
                    .flatten()
                    .filter_map(move |(due, what)| Some((name.clone(), what, due.duration_since(now).ok()?)))
            })
            .max_by_key(|(_, _, left)| *left)
    }

    /// What each process it follows is doing now, as its OS gives it, for a wait's account of
    /// itself, so that a refusal names its own premise (ROOT, 2026-11-14: api_image.rs:1379's
    /// refusal named no member's state, whether one was ending among them): its status
    /// ([`process_status`]), how many of its threads are runnable (`shards_api::
    /// runnable_threads`), where its threads wait ([`waits_in`]), and its end under way or its
    /// stop, as its look found them.
    fn states(&self) -> String {
        if self.watched.is_empty() {
            return "none followed".into();
        }
        let states: Vec<String> = self
            .watched
            .iter()
            .map(|(&pid, watch)| {
                let kin = self
                    .kin
                    .get(&pid)
                    .map_or_else(String::new, |(parent, name)| format!(" {name} (parent {parent})"));
                let mut said = format!(
                    "{pid}{kin} {}",
                    process_status(pid).unwrap_or_else(|| "unread".into())
                );
                if let Some((runnable, threads)) = shards_api::runnable_threads(pid) {
                    said.push_str(&format!(", {runnable} of {threads} threads runnable"));
                }
                if let Some(waits) = waits_in(pid) {
                    said.push_str(&format!(", its threads waiting in {waits}"));
                }
                if watch.ended() {
                    said.push_str(", its end under way");
                }
                if watch.stopped() {
                    said.push_str(", stopped");
                }
                said
            })
            .collect();
        states.join("; ")
    }

    /// Who it is, for a wait's account of itself.
    fn named(&self) -> String {
        let who = match &self.members {
            Members::Clients(c) => match c.as_slice() {
                [one] => format!("client {one}"),
                many => format!("clients {many:?}"),
            },
            Members::Own => "this test's own VM processes".to_string(),
            Members::Machine => "a machine of this test's process".to_string(),
            Members::Started => "the processes this test's process started".to_string(),
            #[cfg(unix)]
            Members::Groups(groups) => format!("the processes of process groups {groups:?}"),
        };
        let who = match self.cpu {
            Cpu::Progress => who,
            Cpu::Liveness => format!("{who}, its CPU its liveness alone"),
        };
        match (&self.home, self.daemon()) {
            (Some(home), Some(daemon)) => format!("{who}, daemon {daemon} of {}", home.display()),
            // One its home's pid file still names, the same process, no longer living: as it
            // is now, its threads among it. A killed daemon's main thread may end before its
            // others (Linux's delayed group leader, kernel/exit.c), so that it reads as a
            // zombie while they still hold its files open (fs/file.c `put_files_struct`), its
            // clients' slots among them, which a client waiting on its slot then waits on (CI
            // 44077430171, x86_64 musl, `a_container_outlives_a_daemon_that_dies`).
            (Some(home), None) => match shards_ipc::Named::read(&home.join(shards_ipc::PID)) {
                Some(named) if shards_ipc::started(named.pid).is_ok_and(|start| start == named.start) => {
                    format!(
                        "{who}, its daemon {} no longer living in {}: {}",
                        named.pid,
                        home.display(),
                        ending(named.pid)
                    )
                }
                Some(named) => format!("{who}, its daemon {} gone from {}", named.pid, home.display()),
                None => format!("{who}, no daemon yet in {}", home.display()),
            },
            (None, _) => who,
        }
    }
}

/// Process `pid` as a wait's account says it once it no longer lives: its status
/// ([`process_status`]) and its threads (`shards_api::runnable_threads`), each where it could
/// be read.
fn ending(pid: u32) -> String {
    let threads = shards_api::runnable_threads(pid).map_or_else(String::new, |(runnable, threads)| {
        format!(", {runnable} of {threads} threads runnable")
    });
    match process_status(pid) {
        Some(status) => format!("{status}{threads}"),
        None => format!("its state unread{threads}"),
    }
}

/// What this process's threads beside the one that looks ran since the look before: the CPU
/// of a machine this process runs (`Members::Machine`), whose vCPUs and devices are threads
/// of it, as is the host's work its guest waits on (a plug, a grow, a disk's reads), and
/// never its waiter's own polling (D209: a wait's own polls are no progress of its party's;
/// #77, ROOT's KVM run c33, hotplug.rs refused 1.09 ms into its guest's rescan, its party its
/// words alone). Each look reads the looking thread's CPU counter, this process's, and the
/// looking thread's again: the waiter ran at most what its counter grew by from before the
/// look before's reading of this process to after this one's, so what this process's counter
/// grew by past that, and past what either counter is uncertain by (its resolution, as its
/// OS says it), is the other threads'. A look made by another thread than the look before
/// has nothing before it. Read as each OS keeps them:
/// - Unix: this process through D215's `shards_api::Watch` (Linux's CPU clock to the
///   nanosecond, macOS' proc_pid_rusage(2) in Mach units), the thread through
///   clock_gettime(2)'s CLOCK_THREAD_CPUTIME_ID, each counter's resolution its clock's
///   (`Watch::resolution`, clock_getres(2)).
/// - Linux, beside that: each other thread's own CPU clock. The process's clock brings only
///   its reader's thread up to date: a thread running on another CPU is in it as of that
///   CPU's last tick or scheduler event (kernel/sched/cputime.c `thread_group_cputime`), so
///   between ticks it grows by its waiter's run alone (CI's aarch64 run 43575280954: refused
///   at W* 239 µs, 2.6 ms after the process's clock last grew, a thread spinning throughout).
///   A thread's own clock brings a running thread up to date as it is read
///   (kernel/sched/core.c `task_sched_runtime`). The process's clock still counts a thread
///   that ran and exited between two looks; either showing a run is its progress.
/// - macOS, beside that: each other thread's own times (task_threads(2) and
///   thread_info(THREAD_BASIC_INFO), as [`ServerThread`] reads one). proc_pid_rusage(2) did
///   not show a thread spinning beside its waiter within 450 µs windows on HVF
///   (`a_judged_calls_own_work_is_its_progress`' first form, and ROOT's batch: this file's
///   `a_machines_host_work_is_its_progress` refused under 14 busy loops), where its own
///   times did.
/// - Linux and macOS: whether another thread is runnable as it is read (Linux its state `R`,
///   macOS `TH_STATE_RUNNING`, each running or waiting for a CPU): one whose others ran
///   nothing since the look before while one of them was runnable was held by its host
///   ([`Ran::Held`]), as a busy host keeps a spinning thread from its CPU.
/// - Windows: QueryProcessCycleTime and QueryThreadCycleTime, cycle counts, exact (their
///   time undocumented: what they ran is said as no CPU time, R* a refusal's diagnosis).
///
/// Other tests of its process run beside it too: a binary whose tests run two machines at
/// once has each machine's CPU the other's progress, a stall shown only once the other ends.
#[derive(Default)]
struct Beside {
    /// The look before: the thread that made it, and what it read.
    last: Option<(std::thread::ThreadId, Reading)>,
    /// What a difference of two readings is uncertain by: the two counters' resolutions.
    uncertainty: Duration,
    #[cfg(unix)]
    watch: Option<shards_api::Watch>,
}

/// One look's reading, in its OS's units (Unix nanoseconds, Windows cycles): the looking
/// thread's counter, this process's, then the looking thread's again; on Linux and macOS
/// each other thread's own counter too, by its thread ID (macOS: its port's name), and
/// whether one of them was runnable.
struct Reading {
    before: u64,
    process: u64,
    after: u64,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    others: Others,
}

/// This process's threads but the calling one, each by its ID and its own CPU time, and
/// whether one was runnable as it was read.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Others {
    cpu: std::collections::BTreeMap<u32, u64>,
    runnable: bool,
}

/// What [`Beside::look`] found.
enum Ran {
    /// Nothing before it to compare with: its first look, one by another thread, or one that
    /// could not read a counter.
    Based,
    /// Nothing beyond its waiter's own run and the counters' uncertainty.
    Still,
    /// Nothing, while one of its other threads was runnable: its host held it.
    Held,
    /// Its other threads ran, this much (none said on Windows, whose cycles are no time).
    Others(Duration),
}

impl Beside {
    fn look(&mut self) -> Ran {
        let thread = std::thread::current().id();
        let Some(now) = self.read() else {
            self.last = None;
            return Ran::Based;
        };
        let others = match self.last.take() {
            Some((by, last)) if by == thread => Some(self.others(&last, &now)),
            _ => None,
        };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let held = now.others.runnable;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let held = false;
        self.last = Some((thread, now));
        match others {
            None => Ran::Based,
            Some(0) if held => Ran::Held,
            Some(0) => Ran::Still,
            Some(ns) if cfg!(unix) => Ran::Others(Duration::from_nanos(ns)),
            Some(_) => Ran::Others(Duration::ZERO),
        }
    }

    /// What this process's threads beside its waiter ran from `last` to `now`, in its OS's
    /// units: the process's counter past its waiter's run and the counters' uncertainty, or,
    /// on Linux, the other threads' own counters' growth, whichever shows more.
    fn others(&self, last: &Reading, now: &Reading) -> u64 {
        let waiter = now.after.saturating_sub(last.before);
        let uncertain = if cfg!(windows) {
            0
        } else {
            u64::try_from(self.uncertainty.as_nanos()).unwrap_or(u64::MAX)
        };
        let process = now
            .process
            .saturating_sub(last.process)
            .saturating_sub(waiter)
            .saturating_sub(uncertain);
        process.max(threads_ran(last, now))
    }

    #[cfg(unix)]
    fn read(&mut self) -> Option<Reading> {
        let before = thread_cpu(libc::clock_gettime)?;
        let process = match self.watch.as_mut() {
            Some(watch) => {
                watch.look().ok()?;
                watch.cpu()
            }
            None => {
                let watch = shards_api::Followed::of_pid(std::process::id())
                    .ok()
                    .flatten()
                    .and_then(|followed| shards_api::Watch::of(followed).ok().flatten())?;
                let cpu = watch.cpu();
                self.uncertainty = watch.resolution().saturating_add(thread_cpu(libc::clock_getres)?);
                self.watch = Some(watch);
                cpu
            }
        };
        let after = thread_cpu(libc::clock_gettime)?;
        let ns = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        Some(Reading {
            before: ns(before),
            process: ns(process),
            after: ns(after),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            others: other_threads()?,
        })
    }

    #[cfg(windows)]
    fn read(&mut self) -> Option<Reading> {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread};
        use windows_sys::Win32::System::WindowsProgramming::{QueryProcessCycleTime, QueryThreadCycleTime};
        let (mut before, mut process, mut after) = (0u64, 0u64, 0u64);
        // SAFETY: this thread's and this process's pseudo-handles, which need no closing, and
        // a count each to write.
        let read = unsafe {
            QueryThreadCycleTime(GetCurrentThread(), &mut before) != 0
                && QueryProcessCycleTime(GetCurrentProcess(), &mut process) != 0
                && QueryThreadCycleTime(GetCurrentThread(), &mut after) != 0
        };
        read.then_some(Reading {
            before,
            process,
            after,
        })
    }
}

/// What the other threads' own counters grew by from `last` to `now` (Linux and macOS; none
/// read elsewhere). A thread not read at the look before began since: all it ran is since.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn threads_ran(last: &Reading, now: &Reading) -> u64 {
    now.others.cpu.iter().fold(0u64, |ran, (tid, &cpu)| {
        ran.saturating_add(cpu.saturating_sub(last.others.cpu.get(tid).copied().unwrap_or(0)))
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn threads_ran(_: &Reading, _: &Reading) -> u64 {
    0
}

/// `read` (clock_gettime(2) or clock_getres(2)) of the calling thread's CPU clock.
#[cfg(unix)]
fn thread_cpu(
    read: unsafe extern "C" fn(libc::clockid_t, *mut libc::timespec) -> libc::c_int,
) -> Option<Duration> {
    cpu_clock(libc::CLOCK_THREAD_CPUTIME_ID, read)
}

/// The CPU time of each thread of this process but the calling one, by its ID, each from
/// its own CPU clock: the scheduler's (`CPUCLOCK_SCHED`, 2) of the thread its ID names
/// (`CPUCLOCK_PERTHREAD_MASK`, 4), `(~tid << 3) | 6` (include/linux/posix-timers.h
/// `make_thread_cpuclock`; glibc's and musl's `pthread_getcpuclockid` make the same). A
/// thread that ended between this process's listing and its read is left out.
#[cfg(target_os = "linux")]
fn other_threads() -> Option<Others> {
    // SAFETY: gettid(2) takes nothing and always succeeds.
    let me = unsafe { libc::gettid() };
    let mut others = Others {
        cpu: std::collections::BTreeMap::new(),
        runnable: false,
    };
    for entry in std::fs::read_dir("/proc/self/task").ok()? {
        let Some(tid) = entry
            .ok()
            .and_then(|entry| entry.file_name().to_str()?.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        if tid == me {
            continue;
        }
        let Some(clock) = (!tid).checked_shl(3).map(|id| id | 6) else {
            continue;
        };
        let Some(cpu) = cpu_clock(clock, libc::clock_gettime) else {
            continue;
        };
        let (Ok(tid), Ok(ns)) = (u32::try_from(tid), u64::try_from(cpu.as_nanos())) else {
            continue;
        };
        others.cpu.insert(tid, ns);
        // Its state (`/proc/self/task/<tid>/stat` field 3, after its command's closing
        // parenthesis, proc(5)).
        others.runnable |= std::fs::read_to_string(format!("/proc/self/task/{tid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')
                    .map(|(_, rest)| rest.trim_start().starts_with('R'))
            })
            .unwrap_or(false);
    }
    Some(others)
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// This task's port, which the SDK's `mach_task_self()` reads (mach/mach_init.h), as
    /// shards-vmm's `platform` binds it.
    static mach_task_self_: libc::mach_port_t;
    /// Releases one reference of right `name` in `task`'s port space (mach/mach_port.h;
    /// libSystem), which libc binds no name for.
    fn mach_port_deallocate(task: libc::mach_port_t, name: libc::mach_port_t) -> libc::kern_return_t;
}

/// This process's threads but the calling one, each by its port's name, its user and system
/// time (thread_info(THREAD_BASIC_INFO), as [`ServerThread`] reads one), and whether one is
/// `TH_STATE_RUNNING` (running or waiting for a CPU). A thread that ended between the list
/// and its read is left out.
#[cfg(target_os = "macos")]
fn other_threads() -> Option<Others> {
    // SAFETY: this thread's own pthread, whose port it returns without a new reference.
    let me = unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) };
    let mut list: libc::thread_act_array_t = std::ptr::null_mut();
    let mut count: libc::mach_msg_type_number_t = 0;
    // SAFETY: a read of the task's port, which the system sets before main and never
    // changes.
    let task = unsafe { mach_task_self_ };
    // SAFETY: task_threads(2) of this task, into a list and a count it allocates and fills;
    // each port it names carries a send right of ours, released below.
    if unsafe { libc::task_threads(task, &raw mut list, &raw mut count) } != libc::KERN_SUCCESS {
        return None;
    }
    let ports: Vec<libc::thread_act_t> = (0..usize::try_from(count).unwrap_or(0))
        // SAFETY: `count` ports the call wrote at `list`.
        .map(|i| unsafe { *list.add(i) })
        .collect();
    let mut others = Others {
        cpu: std::collections::BTreeMap::new(),
        runnable: false,
    };
    for &port in &ports {
        if port != me
            && let Some((ns, runnable)) = thread_times(port)
        {
            others.cpu.insert(port, ns);
            others.runnable |= runnable;
        }
        // SAFETY: the send right task_threads gave of this port, released once.
        unsafe { mach_port_deallocate(task, port) };
    }
    let bytes = usize::try_from(count)
        .ok()
        .and_then(|n| n.checked_mul(std::mem::size_of::<libc::thread_act_t>()))
        .unwrap_or(0);
    // SAFETY: the list task_threads allocated in this task, of `count` ports, freed once.
    unsafe { libc::vm_deallocate(task, list as libc::vm_address_t, bytes as libc::vm_size_t) };
    Some(others)
}

/// Thread `port`'s user and system time in nanoseconds, and whether it is
/// `TH_STATE_RUNNING`; none where it cannot be read (it ended).
#[cfg(target_os = "macos")]
fn thread_times(port: libc::thread_act_t) -> Option<(u64, bool)> {
    // SAFETY: plain data, for thread_info to fill.
    let mut info: libc::thread_basic_info = unsafe { std::mem::zeroed() };
    let flavor = libc::thread_flavor_t::try_from(libc::THREAD_BASIC_INFO).ok()?;
    let mut count = libc::THREAD_BASIC_INFO_COUNT;
    // SAFETY: THREAD_BASIC_INFO of a thread port of ours into a thread_basic_info of its
    // count's size (mach/thread_info.h).
    let code = unsafe { libc::thread_info(port, flavor, (&raw mut info).cast(), &raw mut count) };
    if code != libc::KERN_SUCCESS {
        return None;
    }
    let us = |t: libc::time_value_t| {
        u64::try_from(t.seconds)
            .ok()?
            .checked_mul(1_000_000)?
            .checked_add(u64::try_from(t.microseconds).ok()?)
    };
    let ns = us(info.user_time)?
        .checked_add(us(info.system_time)?)?
        .checked_mul(1000)?;
    Some((ns, info.run_state == libc::TH_STATE_RUNNING))
}

/// `read` (clock_gettime(2) or clock_getres(2)) of CPU clock `clock`.
#[cfg(unix)]
fn cpu_clock(
    clock: libc::clockid_t,
    read: unsafe extern "C" fn(libc::clockid_t, *mut libc::timespec) -> libc::c_int,
) -> Option<Duration> {
    let mut at = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a CPU clock read into a local timespec.
    if unsafe { read(clock, &raw mut at) } != 0 {
        return None;
    }
    Some(Duration::new(
        u64::try_from(at.tv_sec).ok()?,
        u32::try_from(at.tv_nsec).ok()?,
    ))
}

/// The threads of this process that serve other processes' requests: a test registry's
/// connections ([`registry_serving_at`]). A command a test judges waits on them (a pull's
/// or a build's daemon on the blob it asked for), yet they are threads of the test's own
/// process, which no party's members by parent include (D209 Part 8): each wait's party
/// counts what they run as its progress, and one held from running as its host's hold
/// ([`Party::look`]). Each thread is followed from its [`Server::begin`] to its end; what
/// it ran is kept as it ends, so that the total never falls.
struct Servers {
    live: std::collections::BTreeMap<u64, ServerThread>,
    /// What the threads that ended ran, in their OS's units ([`ServerThread::ran`]).
    ended: u64,
    next: u64,
}

static SERVERS: std::sync::Mutex<Servers> = std::sync::Mutex::new(Servers {
    live: std::collections::BTreeMap::new(),
    ended: 0,
    next: 0,
});

fn servers() -> std::sync::MutexGuard<'static, Servers> {
    SERVERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A serving thread of this process, followed as [`Servers`] says while it lives: made by the
/// thread itself, its end as it drops.
pub struct Server(u64);

impl Server {
    /// The calling thread, followed from now until this drops on it.
    pub fn begin() -> Server {
        let mut servers = servers();
        let id = servers.next;
        servers.next = id.wrapping_add(1);
        servers.live.insert(id, ServerThread::this());
        Server(id)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let mut servers = servers();
        if let Some(mut thread) = servers.live.remove(&self.0) {
            let ran = thread.ran();
            servers.ended = servers.ended.saturating_add(ran);
        }
    }
}

/// What this process's serving threads have run in all, in their OS's units, and whether one
/// is runnable as it is read (Linux: its state `R`; macOS: `TH_STATE_RUNNING`, running or
/// waiting for a CPU): one that ran nothing since a look before is held by its host.
fn servers_ran() -> (u64, bool) {
    let mut servers = servers();
    let mut ran = servers.ended;
    let mut runnable = false;
    for thread in servers.live.values_mut() {
        ran = ran.saturating_add(thread.ran());
        runnable |= thread.runnable();
    }
    (ran, runnable)
}

/// A thread of this process as its OS lets another thread of its process read it, and what
/// it was last read to have run: a read that fails counts that, so the total never falls. A
/// serving thread ([`Server`]), or a judged call's own ([`judging`]).
struct ServerThread {
    #[cfg(target_os = "linux")]
    tid: libc::pid_t,
    /// Its Mach port (pthread_mach_thread_np(3)): held by its thread, valid while it lives,
    /// which it does while it is followed.
    #[cfg(target_os = "macos")]
    port: libc::mach_port_t,
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
    last: u64,
}

// SAFETY: a thread's handle is a kernel object's, usable from any thread of its process.
#[cfg(windows)]
unsafe impl Send for ServerThread {}

impl ServerThread {
    #[cfg(target_os = "linux")]
    fn this() -> ServerThread {
        // SAFETY: gettid(2) takes nothing and always succeeds.
        let tid = unsafe { libc::gettid() };
        ServerThread { tid, last: 0 }
    }

    /// Its own CPU clock, `make_thread_cpuclock`'s (see [`other_threads`]), in
    /// nanoseconds.
    #[cfg(target_os = "linux")]
    fn read(&self) -> Option<u64> {
        let clock = (!self.tid).checked_shl(3)? | 6;
        u64::try_from(cpu_clock(clock, libc::clock_gettime)?.as_nanos()).ok()
    }

    /// Its state (`/proc/self/task/<tid>/stat` field 3, after its command's closing
    /// parenthesis, proc(5)) is `R`.
    #[cfg(target_os = "linux")]
    fn runnable(&self) -> bool {
        std::fs::read_to_string(format!("/proc/self/task/{}/stat", self.tid))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')
                    .map(|(_, rest)| rest.trim_start().starts_with('R'))
            })
            .unwrap_or(false)
    }

    #[cfg(target_os = "macos")]
    fn this() -> ServerThread {
        // SAFETY: this thread's own pthread, whose port it returns without a new reference.
        let port = unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) };
        ServerThread { port, last: 0 }
    }

    #[cfg(target_os = "macos")]
    fn info(&self) -> Option<libc::thread_basic_info> {
        // SAFETY: plain data, for thread_info to fill.
        let mut info: libc::thread_basic_info = unsafe { std::mem::zeroed() };
        let flavor = libc::thread_flavor_t::try_from(libc::THREAD_BASIC_INFO).ok()?;
        let mut count = libc::THREAD_BASIC_INFO_COUNT;
        // SAFETY: THREAD_BASIC_INFO of a live thread's port into a thread_basic_info of its
        // count's size (mach/thread_info.h).
        let code = unsafe { libc::thread_info(self.port, flavor, (&raw mut info).cast(), &raw mut count) };
        (code == libc::KERN_SUCCESS).then_some(info)
    }

    /// Its user and system time (`thread_basic_info`), in nanoseconds.
    #[cfg(target_os = "macos")]
    fn read(&self) -> Option<u64> {
        let info = self.info()?;
        let us = |t: libc::time_value_t| {
            u64::try_from(t.seconds)
                .ok()?
                .checked_mul(1_000_000)?
                .checked_add(u64::try_from(t.microseconds).ok()?)
        };
        us(info.user_time)?
            .checked_add(us(info.system_time)?)?
            .checked_mul(1000)
    }

    #[cfg(target_os = "macos")]
    fn runnable(&self) -> bool {
        self.info()
            .is_some_and(|info| info.run_state == libc::TH_STATE_RUNNING)
    }

    #[cfg(windows)]
    fn this() -> ServerThread {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThreadId, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: a handle of this thread's own, closed as it drops.
        let handle = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, GetCurrentThreadId()) };
        ServerThread { handle, last: 0 }
    }

    /// Its cycles (QueryThreadCycleTime), exact, their time undocumented (as [`Beside`]'s).
    #[cfg(windows)]
    fn read(&self) -> Option<u64> {
        let mut cycles = 0u64;
        // SAFETY: a thread handle of ours, or null, which the call refuses; a count to write.
        let read = unsafe {
            windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime(self.handle, &mut cycles)
        };
        (read != 0).then_some(cycles)
    }

    /// Unread on Windows, which keeps a thread's state only in its whole-system listing.
    #[cfg(windows)]
    fn runnable(&self) -> bool {
        false
    }

    fn ran(&mut self) -> u64 {
        if let Some(now) = self.read() {
            self.last = self.last.max(now);
        }
        self.last
    }
}

#[cfg(windows)]
impl Drop for ServerThread {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: the handle `this` opened, closed once.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
        }
    }
}

/// A wait of the tests' own on a party ([`Party`]), judged by its kind's record ([`WaitKind`])
/// as D215's judge judges a wait (`shards_api::Judge`): cold, its party's liveness alone,
/// which its caller holds (its client's end); learned, looked at each W* past its start and
/// refused at a look that finds its party made no progress at all since the look before,
/// its host not holding it. Looked at between those too, paced as D215 paces looks
/// (`shards_api::pause`: a look's own cost against the wait so far), so that what it last
/// did is known, for its account of itself at a refusal or as the test process is ended
/// ([`dump_live_waits`]).
pub struct Wait {
    kind: &'static WaitKind,
    judge: shards_api::Judge<'static>,
    party: Party,
    asked: String,
    began: Instant,
    /// When the next look is due, before the judge's own.
    next: Instant,
    /// What showed progress since the judge's look before, and its host's hold.
    progressed: Option<String>,
    held: Option<String>,
    /// Its last progress, and when.
    last: (Instant, String),
    /// Whether its last look found its host holding it, and what showed it.
    holding: Option<String>,
    /// Whether its last look found its party gone: nothing of it running, nor its daemon
    /// saying anything.
    gone: bool,
    /// Its party's CPU since it began.
    work: Duration,
    /// Whether every look since its judge's look before saw its party run or rest unheld
    /// ([`Looked::vouched`]): only then is its work its judge's rate's (D215).
    vouched: bool,
    /// Whether its work was read through its answer ([`read_through`](Wait::read_through)):
    /// none until a read at its answer, which only then teaches its kind R* and C*.
    read: Option<bool>,
    /// The clients this thread had begun against its party's home as its judge's window
    /// began ([`asked_of`]), and whether its daemon ran in the window: its daemon's progress
    /// only where none began since, as its own clients' work is no progress of its party's.
    polls: u64,
    daemon_ran: Option<String>,
    live: Live,
    stage: Stage,
    /// Whether it waits on a command of its thread's own ([`Wait::begin`]): its span is its
    /// thread's own work ([`OWN`]) as it ends.
    command: bool,
    /// How far into it each due look came that found its party still, since one last found
    /// it progressed, held or based: what its account says its judge saw.
    still: Vec<Duration>,
}

/// Where a [`Wait`] is, by what its caller has found of its answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    /// Its answer not yet found missing: one found now was there as it began, which it
    /// waited on no party for, and teaches nothing (#77 waits-03, as D185's and D209's
    /// answers already there teach nothing).
    Begun,
    /// Its answer found missing ([`Wait::look`], [`Wait::absent`]): it waits on its party,
    /// and its answer teaches its kind.
    Awaited,
    /// Answered ([`Wait::answered`]): what it still reads (a command's streams another
    /// process holds open past its end) is judged by its kind's record as that answer left
    /// it, from the answer on, a look that finds its party gone finding it still: no process
    /// of it that could write runs (#77 waits-10: a cold wait kept past its client's end
    /// followed a silent holder's liveness for ever). It teaches nothing more.
    Answered,
}

thread_local! {
    /// The clients each thread began against each home ([`Wait::begin`]): a waiter's own
    /// asks of its home's daemon, its polls among them.
    static ASKED: std::cell::RefCell<std::collections::BTreeMap<PathBuf, u64>> =
        const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

thread_local! {
    /// The spans this thread waited on its own commands ([`Wait::begin`]'s waits, each kept as
    /// it ends), and how many of its waits live: a look of another of its waits that its
    /// commands made late was late for its own work, no hold of its host's (D209 Part 8).
    /// Let go of as its last live wait ends.
    static OWN: std::cell::RefCell<(usize, Vec<(Instant, Instant)>)> =
        const { std::cell::RefCell::new((0, Vec::new())) };
}

/// The time from `from` to `to` this thread spent waiting on its own commands ([`OWN`]).
fn own_work(from: Instant, to: Instant) -> Duration {
    OWN.with(|own| {
        own.borrow()
            .1
            .iter()
            .rev()
            .take_while(|(_, ended)| *ended > from)
            .fold(Duration::ZERO, |sum, (began, ended)| {
                sum.saturating_add((*ended).min(to).saturating_duration_since((*began).max(from)))
            })
    })
}

/// The clients this thread has begun against `home`, none where it has none.
fn asked_of(home: Option<&Path>) -> u64 {
    home.map_or(0, |home| {
        ASKED.with(|asked| asked.borrow().get(home).copied().unwrap_or(0))
    })
}

impl Wait {
    /// A wait beginning now on client `client`'s party, `asked` naming its command.
    pub fn begin(kind: &'static WaitKind, client: u32, home: Option<&Path>, asked: &str) -> Wait {
        if let Some(home) = home {
            ASKED.with(|asked| {
                let mut asked = asked.borrow_mut();
                let n = asked.entry(home.to_path_buf()).or_insert(0);
                *n = n.saturating_add(1);
            });
        }
        let mut wait = Wait::on(kind, Party::client(client, home), asked);
        wait.command = true;
        wait
    }

    /// It as a wait on the delivery of a run's log to `to`, the destination as the run's
    /// options name it (`logging::destination`: `syslog-address=tcp://127.0.0.1:514`): a
    /// pause of that run's shipper before its next try there holds it, and holds no other
    /// wait (`shards_ipc::RETRY`, D209 Part 10).
    pub fn delivery_to(mut self, to: &str) -> Wait {
        self.party.delivery = Some(to.to_string());
        self
    }

    /// A wait beginning now on `party`, `asked` naming what it waits for.
    fn on(kind: &'static WaitKind, party: Party, asked: &str) -> Wait {
        let began = Instant::now();
        let polls = asked_of(party.home.as_deref());
        OWN.with(|own| {
            let mut own = own.borrow_mut();
            own.0 = own.0.saturating_add(1);
        });
        Wait {
            kind,
            judge: shards_api::Judge::begin(&kind.record),
            party,
            asked: asked.to_string(),
            began,
            next: began,
            progressed: None,
            held: None,
            last: (began, "its start".into()),
            holding: None,
            gone: false,
            work: Duration::ZERO,
            vouched: false,
            read: None,
            polls,
            daemon_ran: None,
            live: Live::new(),
            stage: Stage::Begun,
            command: false,
            still: Vec::new(),
        }
    }

    /// Its answer came (its client's own end, the line or the state it waited for): its
    /// kind's answer, which teaches its record, kept on this host, where it was found
    /// missing first ([`Stage::Awaited`]). What it reads after is judged anew
    /// ([`Stage::Answered`]).
    pub fn answered(&mut self) {
        self.ended(true);
    }

    /// Its command's client ended unserved, by a status other than 0 or a signal: its
    /// answer, which teaches its kind nothing. A command's words and line are its kind, and
    /// only a served answer says it did their work: one refused stops short of it, at a
    /// check before or along it, in what may be a fraction of the work's time (ROOT's batch
    /// 22, KVM, f385b8dac: containers.rs' `shards run --rm --pull never IMAGE exit 0`,
    /// refused in 5.4 ms for a settings file that did not read, taught its line W* 5.4 ms,
    /// which refused the same line's real run, a microVM's boot, 300 ms in). What it reads
    /// after is judged anew, as after any answer.
    pub fn unserved(&mut self) {
        self.ended(false);
    }

    /// Reads its party's CPU now, at its answer, for its work through it: read through where
    /// none of its processes was lost before its last CPU was read (its command's client
    /// read as it ended, before it was reaped). Once.
    pub fn read_through(&mut self) {
        if self.read.is_none() {
            let looked = self.party.look();
            self.work = self.work.saturating_add(looked.cpu);
            self.read = Some(!looked.lost);
        }
    }

    /// Its answer came, taught to its kind where `teaches` and it was found missing first:
    /// its R* and C* too where its work was read through it ([`read_through`](Self::read_through)).
    fn ended(&mut self, teaches: bool) {
        if teaches && self.stage == Stage::Awaited {
            self.read_through();
            if self.read == Some(true) {
                self.judge.answered(self.work);
            } else {
                self.judge.answered_unrated();
            }
            self.kind.keep();
        }
        if self.stage != Stage::Answered {
            self.stage = Stage::Answered;
            self.judge = shards_api::Judge::begin(&self.kind.record);
            (self.progressed, self.held, self.vouched) = (None, None, false);
            self.still.clear();
        }
    }

    /// What its call's own thread did since it was read before ([`judging`]): progress where
    /// it ran, its host's hold where it ran nothing yet was runnable.
    fn call_looked(&mut self, ran: bool, runnable: bool) {
        if ran {
            // Its CPU is no part of its party's work: no window of its rate (D215).
            self.vouched = false;
            self.last = (Instant::now(), "its call's own thread ran".into());
            self.progressed
                .get_or_insert_with(|| "its call's own thread ran".into());
        } else if runnable {
            self.vouched = false;
            self.held
                .get_or_insert_with(|| "its host held its call's own thread from running".into());
        }
    }

    /// Its caller found its answer missing: it waits on its party from here.
    pub fn absent(&mut self) {
        if self.stage == Stage::Begun {
            self.stage = Stage::Awaited;
        }
    }

    /// Whether its caller has found its answer missing.
    pub fn awaited(&self) -> bool {
        self.stage != Stage::Begun
    }

    /// Looks at its party where a look is due, `output` whether its command printed since
    /// the call before: refused where its judge refuses it. A caller looks only once it has
    /// found its answer missing, which a look says ([`absent`](Self::absent)): one that may
    /// find it there as it begins checks first ([`Following`]'s read, engine.rs' `Patient`).
    pub fn look(&mut self, output: bool) -> Result<(), shards_api::Stall> {
        self.absent();
        self.looked(output)
    }

    /// [`look`](Self::look), its answer not said missing: for a wait whose caller knows its
    /// answer is there already ([`judged_there`]).
    fn looked(&mut self, output: bool) -> Result<(), shards_api::Stall> {
        use shards_api::Found;
        let now = Instant::now();
        if output {
            // Words are progress its CPU does not show: no window of its rate (D215).
            self.vouched = false;
            self.last = (now, "its output".into());
            self.progressed.get_or_insert_with(|| "its output".into());
        }
        let due = self.judge.due();
        if now < self.next && due.is_none_or(|due| now < due) {
            return Ok(());
        }
        let own = self
            .judge
            .late_from()
            .map_or(Duration::ZERO, |from| own_work(from, now));
        let mut looked = self.party.look();
        if self.stage == Stage::Answered && looked.found == Found::Gone {
            looked.found = Found::Still;
        }
        self.work = self.work.saturating_add(looked.cpu);
        self.vouched &= looked.vouched && matches!(looked.found, Found::Progressed | Found::Still);
        let span = shards_api::Span {
            began: now,
            ended: Instant::now(),
            resolution: looked.resolution,
            own,
            work: None,
        };
        if self.daemon_ran.is_none() {
            self.daemon_ran = looked.daemon_ran;
        }
        self.holding = None;
        self.gone = looked.found == Found::Gone;
        match looked.found {
            Found::Progressed => {
                self.last = (now, looked.what.clone());
                self.progressed.get_or_insert(looked.what);
            }
            Found::Held => {
                self.holding = Some(looked.what.clone());
                self.held.get_or_insert(looked.what);
            }
            Found::Still | Found::Gone | Found::Based => {}
        }
        let mut judged = Ok(());
        if due.is_some_and(|due| now >= due) {
            // Its daemon's run is its party's progress only in a window in which this thread
            // began no client of its home (ROOT's ruling, 2026-11-13, D209). The window and not
            // the client's span: the daemon's work for a client is not bound to the client's
            // life, as its slot-maker thread makes slots again as they are taken and let go of
            // (slots.rs `make_slots`).
            let polls = asked_of(self.party.home.as_deref());
            if let Some(ran) = self.daemon_ran.take()
                && polls == self.polls
            {
                self.last = (now, ran.clone());
                self.progressed.get_or_insert(ran);
            }
            self.polls = polls;
            // A window's findings are its own: its progress outranks its hold, and neither
            // carries into the next window. Taken one at a time, a hold its call's spawn
            // recorded outlived the window its call's first run progressed, and judged the
            // next one held: every idle call's refusal came a window late, at the fourth
            // due look, on arm64 Windows and x86_64 alike (judge-trace 44163626733, PM M618).
            let (progressed, held) = (self.progressed.take(), self.held.take());
            let found = if progressed.is_some() {
                Found::Progressed
            } else if held.is_some() {
                Found::Held
            } else if looked.found == Found::Based {
                Found::Based
            } else {
                Found::Still
            };
            // Its rate's window is this one where every look in it vouched, and the next
            // begins vouched.
            let work = std::mem::replace(&mut self.vouched, true).then_some(self.work);
            judged = self.judge.found_over(found, shards_api::Span { work, ..span });
            if found == Found::Still {
                self.still.push(self.began.elapsed());
            } else {
                self.still.clear();
            }
        } else if looked.found == Found::Based {
            // Its evidence begins at this look's end, due or not: a based look refuses nothing.
            judged = self.judge.found_over(Found::Based, span);
            self.still.clear();
        }
        self.live.set(self.account(), self.party.home.as_deref());
        let left = self
            .judge
            .due()
            .map_or(Duration::MAX, |due| due.saturating_duration_since(Instant::now()));
        self.next = Instant::now() + shards_api::pause(now.elapsed(), self.began.elapsed(), left);
        judged
    }

    /// Its account: its call site and command, its party, its last progress, its host's
    /// hold at its last look, and its kind's record.
    pub fn account(&self) -> String {
        let record = match self.kind.record.seen() {
            shards_api::Seen::Cold => "none yet on this host: its party's liveness alone".to_string(),
            shards_api::Seen::Learned { slowest, rated: None } => {
                format!("W* {slowest:?}, no answer's work read through its end")
            }
            shards_api::Seen::Learned {
                slowest,
                rated: Some(rated),
            } => format!(
                "W* {slowest:?}, R* {:?} of CPU in {:?}, C* {:?} of CPU",
                rated.least.cpu, rated.least.over, rated.most
            ),
        };
        let kind = if self.kind.line.is_empty() {
            self.kind.site.to_string()
        } else {
            format!("{} {}", self.kind.site, self.kind.line)
        };
        format!(
            "{kind}: {} ({}), {:?} in: its last progress {} {:?} ago; {}; its record {record}{}{}; its members now: {}",
            self.asked,
            self.party.named(),
            self.began.elapsed(),
            self.last.1,
            self.last.0.elapsed(),
            self.holding.as_deref().map_or_else(
                || "its host not holding it".to_string(),
                |held| format!("held: {held}")
            ),
            self.judge.stillness().map_or_else(String::new, |(still, held, window)| format!(
                "; still {still:?} of its {window:?} window as its last look saw, {held:?} of its looks' lateness out"
            )),
            if self.still.is_empty() {
                String::new()
            } else {
                format!(
                    "; its due looks that found its party still since: {} of them, {:?} in",
                    self.still.len(),
                    self.still
                )
            },
            self.party.states(),
        )
    }

    /// How long until its next look is due, paced or its judge's: what a caller that blocks
    /// on its answer (a read, a channel) may block for before it calls [`look`](Self::look)
    /// again.
    pub fn next_look(&self) -> Duration {
        self.next.saturating_duration_since(Instant::now())
    }

    /// Whether its judge looks at it: its kind had an answer as it began, and its party has
    /// not gone since. One not judged follows its party's liveness alone.
    pub fn judged(&self) -> bool {
        self.judge.due().is_some()
    }

    /// Whether its last look found its party gone ([`Party::look`]).
    pub fn gone(&self) -> bool {
        self.gone
    }

    /// What a refusal says of it.
    pub fn refused(&self, stall: &shards_api::Stall) -> String {
        if let (Some(rate), Some(least)) = (stall.rate(), stall.least()) {
            return format!(
                "{} ran too slowly to be answering: {:?} of CPU in the {:?} it was seen to run unheld, below the least rate any answer of its kind came at on this host (R* {:?} in {:?}) and more than any such answer took (C* {:?}), past the slowest such answer ({:?}), {:?} into its wait\n--- its wait\n{}",
                self.asked,
                rate.cpu,
                rate.over,
                least.cpu,
                least.over,
                stall.most().unwrap_or_default(),
                stall.slowest(),
                stall.waited(),
                self.account()
            );
        }
        format!(
            "{} made no progress for {:?}, past the slowest answer of its kind seen on this host ({:?}), {:?} into its wait\n--- its wait\n{}",
            self.asked,
            stall.over(),
            stall.slowest(),
            stall.waited(),
            self.account()
        )
    }
}

impl Drop for Wait {
    fn drop(&mut self) {
        let span = (self.began, Instant::now());
        OWN.with(|own| {
            let mut own = own.borrow_mut();
            own.0 = own.0.saturating_sub(1);
            if own.0 == 0 {
                own.1.clear();
            } else if self.command {
                own.1.push(span);
            }
        });
    }
}

/// Each live wait's account as its last look gave it, by a number of its own: what
/// [`dump_live_waits`] says as the test process is ended.
static LIVE: std::sync::Mutex<std::collections::BTreeMap<u64, (String, Option<PathBuf>)>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

fn live_waits() -> std::sync::MutexGuard<'static, std::collections::BTreeMap<u64, (String, Option<PathBuf>)>>
{
    LIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A wait's place among [`LIVE`]'s, let go of as it ends.
struct Live(u64);

impl Live {
    fn new() -> Live {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        dump_at_the_end();
        Live(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    fn set(&self, account: String, home: Option<&Path>) {
        live_waits().insert(self.0, (account, home.map(Path::to_path_buf)));
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        live_waits().remove(&self.0);
    }
}

/// Says each live wait's account on stderr: as the test process is ended by SIGINT or
/// SIGTERM (CI's job limit, a person's ^C), or by Windows' console control event, so that a
/// wait that never ends, cold or not, leaves what it knew (ROOT, 2026-11-13): its members,
/// each by its name, parent and state at its last look, and what each home its waits were
/// on wrote ([`show_why`]: its runs' stderr, its daemon's samples, its daemon's log's tail),
/// which go with the home as the process ends (b24's 62-minute wait was ended with its home
/// and no word of which process its last progress was, nor what its daemon said).
fn dump_live_waits(why: &str) {
    let live = live_waits();
    let mut said = format!("shards tests: ended by {why}, {} wait(s) under way", live.len());
    for (account, _) in live.values() {
        said.push_str("\n  ");
        said.push_str(account);
    }
    let _ = writeln!(io::stderr(), "{said}");
    let homes: std::collections::BTreeSet<&PathBuf> =
        live.values().filter_map(|(_, home)| home.as_ref()).collect();
    for home in homes {
        show_why(home);
    }
}

/// The write end of the pipe on which [`ended_by`] tells [`dump_at_the_end`]'s thread a
/// signal came, and the process it is of.
#[cfg(unix)]
static TOLD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
#[cfg(unix)]
static OWNER: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// SIGINT's and SIGTERM's handler: the signal's number written to [`dump_at_the_end`]'s
/// thread by this process alone (a child between its fork and its exec runs it too), with
/// getpid(2) and write(2), each async-signal-safe (POSIX.1-2024, XSH 2.4.3).
#[cfg(unix)]
extern "C" fn ended_by(signal: libc::c_int) {
    let to = TOLD.load(Ordering::Relaxed);
    let byte = u8::try_from(signal).unwrap_or(u8::MAX);
    // SAFETY: getpid(2), then write(2) of one byte of ours to a descriptor of ours.
    unsafe {
        if to >= 0 && libc::getpid() == OWNER.load(Ordering::Relaxed) {
            libc::write(to, (&raw const byte).cast(), 1);
        }
    }
}

/// Has this process say its live waits as it is ended ([`dump_live_waits`]), once: on Unix a
/// thread that SIGINT's and SIGTERM's handler wakes, which says them, then gives the signal
/// its default action and raises it again, so that the process ends as it would have; on
/// Windows a console control handler that says them and lets the system's own end it.
#[cfg(unix)]
fn dump_at_the_end() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: pipe(2) into two descriptors of ours.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return;
        }
        for fd in fds {
            // SAFETY: fcntl(2) of a descriptor just made, which no child is given.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        let [from, to] = fds;
        TOLD.store(to, Ordering::SeqCst);
        // SAFETY: getpid(2).
        OWNER.store(unsafe { libc::getpid() }, Ordering::SeqCst);
        let spawned = std::thread::Builder::new()
            .name("waits at the end".into())
            .spawn(move || {
                let mut signal = 0u8;
                loop {
                    // SAFETY: read(2) of one byte into a local.
                    match unsafe { libc::read(from, (&raw mut signal).cast(), 1) } {
                        1 => break,
                        -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
                        _ => return,
                    }
                }
                let signal = libc::c_int::from(signal);
                dump_live_waits(if signal == libc::SIGINT {
                    "SIGINT"
                } else {
                    "SIGTERM"
                });
                // SAFETY: signal(2) back to the default action, then raise(3) of it.
                unsafe {
                    libc::signal(signal, libc::SIG_DFL);
                    libc::raise(signal);
                }
            });
        if spawned.is_err() {
            return;
        }
        for signal in [libc::SIGINT, libc::SIGTERM] {
            // SAFETY: a sigaction of ours, all its fields set, for a handler that is
            // async-signal-safe ([`ended_by`]).
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = ended_by as *const () as libc::sighandler_t;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&raw mut action.sa_mask);
                libc::sigaction(signal, &raw const action, std::ptr::null_mut());
            }
        }
    });
}

#[cfg(windows)]
fn dump_at_the_end() {
    /// HandlerRoutine: the live waits said, then FALSE, so that the next handler, the
    /// system's own, ends the process as it would have.
    unsafe extern "system" fn ended_by(event: u32) -> windows_sys::core::BOOL {
        dump_live_waits(&format!("console control event {event}"));
        0
    }
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        // SAFETY: a handler of this process's own, added to its list for its life.
        unsafe { windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(ended_by), 1) };
    });
}

/// Ends `child`, a run at its end or its stall, and gives the processes under it, each named,
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
    /// A directory named for `name`, registered as `{dir NAME}` in its commands' keys, and
    /// the directory every one of this run's is under as `{testdir}` ([`minted`]), each as
    /// its path and as that path made canonical (macOS' `/private/var`).
    pub fn new(name: &str) -> TempDir {
        let dir = TempDir(shards_testdir::TempDir::new(name).unwrap());
        let placeholder = format!("{{dir {name}}}");
        for path in dir.minted() {
            minted(&path, &placeholder);
        }
        if let Some(root) = dir.0.parent() {
            minted(&root.to_string_lossy(), "{testdir}");
            if let Ok(root) = root.canonicalize() {
                minted(&root.to_string_lossy(), "{testdir}");
            }
        }
        dir
    }

    /// Its path, and that path made canonical.
    fn minted(&self) -> Vec<String> {
        let path: &Path = &self.0;
        std::iter::once(path.to_path_buf())
            .chain(path.canonicalize())
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
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
        for path in self.minted() {
            unminted(&path);
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

/// The image a registry serves, which a test may replace: each request takes the image as
/// it is then, shared, never copied, its lock held only to take it (D209 Part 8: a clone of
/// the whole image under one lock for each request had each request wait on the others').
#[derive(Clone)]
pub struct Served(Arc<std::sync::Mutex<Arc<Image>>>);

/// A served image: its manifest and its blobs, each with its digest.
struct Image {
    manifest: Vec<u8>,
    digest: String,
    blobs: Vec<(String, Vec<u8>)>,
}

impl Served {
    /// Serving `manifest` and `blobs`.
    pub fn new((manifest, blobs): (Vec<u8>, Vec<Vec<u8>>)) -> Served {
        Served(Arc::new(std::sync::Mutex::new(Image::of(manifest, blobs))))
    }

    /// Serving `manifest` and `blobs` from the next request on.
    pub fn set(&self, (manifest, blobs): (Vec<u8>, Vec<Vec<u8>>)) {
        *self.0.lock().unwrap() = Image::of(manifest, blobs);
    }

    fn now(&self) -> Arc<Image> {
        self.0.lock().unwrap().clone()
    }
}

impl Image {
    fn of(manifest: Vec<u8>, blobs: Vec<Vec<u8>>) -> Arc<Image> {
        Arc::new(Image {
            digest: sha256_digest(&manifest),
            manifest,
            blobs: blobs.into_iter().map(|b| (sha256_digest(&b), b)).collect(),
        })
    }

    fn blob(&self, digest: &str) -> Option<&[u8]> {
        self.blobs
            .iter()
            .find(|(d, _)| d == digest)
            .map(|(_, b)| b.as_slice())
    }
}

/// A registry that serves `test/image:v1` (manifest, config, layer) over plain HTTP, and
/// counts the requests it answers.
pub fn registry(manifest: Vec<u8>, blobs: Vec<Vec<u8>>) -> (u16, Arc<AtomicUsize>) {
    let (port, served, _) = registry_of(Served::new((manifest, blobs)));
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
    let (port, _, _) = registry_serving(Served::new((manifest, blobs)), Some(stalls.clone()));
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
    let (port, served, _) = registry_serving_at(ip, Served::new((manifest, blobs)), None);
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
    let port = minted_port(listener.local_addr().unwrap().port());
    let served = Arc::new(AtomicUsize::new(0));
    let count = served.clone();
    let serving = image.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (image, count, stalls) = (serving.clone(), count.clone(), stalls.clone());
            std::thread::spawn(move || {
                let _serving = Server::begin();
                // Asked for TLS, it answers as Go's net/http, and so a registry, answers a
                // request it cannot read (server.go, publicErr).
                let mut first = [0u8; 1];
                if stream.peek(&mut first).is_ok_and(|n| n == 1) && first[0] == 0x16 {
                    let _ = (&stream).write_all(GO_BAD_REQUEST);
                    return;
                }
                // A head and its body go as two writes: unsent, its head would wait for an
                // ACK the client delays (Nagle's algorithm, RFC 896; RFC 1122 4.2.3.2).
                let _ = stream.set_nodelay(true);
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
                    let image = image.now();
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
                    let body = if let Some(d) = path.strip_prefix("/v2/test/image/manifests/") {
                        if d == "v1" || d == image.digest {
                            Some((
                                image.digest.as_str(),
                                image.manifest.as_slice(),
                                kind(&image.manifest),
                            ))
                        } else {
                            // The manifests an index names, kept among the blobs.
                            image.blob(d).map(|b| (d, b, kind(b)))
                        }
                    } else {
                        path.strip_prefix("/v2/test/image/blobs/")
                            .and_then(|d| Some((d, image.blob(d)?, "application/octet-stream")))
                    };
                    let sent = match body {
                        Some((digest, bytes, kind)) => {
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nDocker-Content-Digest: {digest}\r\nContent-Length: {}\r\n\r\n",
                                bytes.len()
                            );
                            out.write_all(head.as_bytes()).and_then(|()| {
                                if method == "HEAD" {
                                    Ok(())
                                } else {
                                    out.write_all(bytes)
                                }
                            })
                        }
                        None => out.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"),
                    };
                    if sent.is_err() {
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
    let port = minted_port(listener.local_addr().unwrap().port());
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
#[track_caller]
pub fn hub(reference: &str) -> String {
    static SERVED: OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> = OnceLock::new();
    let mut served = SERVED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(at) = served.get(reference) {
        return at.clone();
    }
    let (top, blobs) = hub_seed(reference);
    let digest = sha256_digest(&top);
    let (port, _) = registry(top, blobs);
    let at = format!("127.0.0.1:{port}/test/image@{digest}");
    served.insert(reference.to_string(), at.clone());
    at
}

/// `reference` as a test pulls it: a remote registry's image pinned by its digest from the
/// tests' own registry ([`hub`]), any other (a loopback registry's, a tag) as it is.
pub fn hubless(reference: &str) -> String {
    match shards_image::reference::Reference::parse(reference) {
        Ok(r) if remote(&r) && r.digest.is_some() => hub(reference),
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
pub fn hub_bases(agentfile: &str) -> String {
    agentfile
        .split_inclusive('\n')
        .map(|line| {
            let base = line
                .strip_prefix("FROM ")
                .and_then(|rest| rest.split_whitespace().find(|w| !w.starts_with("--")));
            match base {
                Some(base) => line.replacen(base, &hubless(base), 1),
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
#[track_caller]
fn hub_seed(reference: &str) -> (Vec<u8>, Vec<Vec<u8>>) {
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
    let built = run_shards_env(&["build"], &[ctx.as_os_str()], &env);
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
    /// One of this test's own VM processes until it drops, after its child is ended.
    _own: OwnVm,
    lines: std::sync::mpsc::Receiver<String>,
    pub seen: Vec<String>,
    /// Every line it has printed, read or not: what a test that stalls says.
    log: Log,
    /// Its guest's size (its vCPUs), which keys its streamed waits' kinds ([`Vm::sized`]).
    size: String,
}

/// Lines a process printed, as they came.
pub type Log = Arc<std::sync::Mutex<Vec<String>>>;

/// Runs `f`, failing with what `vm` said where its guest stops progressing before it ends: a
/// wait judged as [`run_command`]'s is ([`Heard::running`]), its kind its call site, a line
/// more from the VM or its VM process's CPU its progress. A test that stalls says where, and
/// what its VM said, rather than holding the whole binary; one its host runs slowly is waited
/// on as it runs.
#[track_caller]
pub fn within<T: Send + 'static>(what: &str, vm: &Vm, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut heard = Heard::running(vm.id());
    let worker = std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    let lines = || vm.log.lock().map_or(0, |l| l.len());
    let mut said = lines();
    loop {
        match rx.recv_timeout(heard.next_look()) {
            Ok(v) => {
                heard.answered();
                return v;
            }
            // Ended without sending: it panicked, and says why, after what the VM said.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let said = vm.log.lock().map(|l| l.join("\n")).unwrap_or_default();
                let _ = writeln!(std::io::stderr(), "{what}: failed; the VM said:\n{said}");
                match worker.join() {
                    Err(panic) => std::panic::resume_unwind(panic),
                    Ok(()) => panic!("{what}: ended with no result"),
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        let now = lines();
        heard.said(now != said);
        said = now;
        if let Err(refused) = heard.look(true) {
            let said = vm.log.lock().map(|l| l.join("\n")).unwrap_or_default();
            panic!("{what}: {refused}\nthe VM said:\n{said}")
        }
    }
}

/// A call that blocks until its answer or its party's end (shards-api's `ImageShards` and
/// `ImageGuest` calls given `Duration::MAX`, until it answers or its run ends; a read of a
/// pipe its run holds), judged as [`run_command`]'s wait is ([`Wait`], D215's judge), its
/// kind its call site: run on a thread of its own, its return both its answer and its
/// liveness, `party`'s processes and words its progress. Cold, its return alone ends it;
/// learned, it is refused at a look past W* that finds `party` made no progress since the
/// look before, its host not holding it. Refused, `stop` ends the call (a boot's
/// `BootStop::stop`, a guest's `stop`), which is waited for, and what its wait says of
/// itself comes back.
#[track_caller]
pub fn judged<T: Send>(
    party: Party,
    asked: &str,
    stop: impl FnOnce(),
    call: impl FnOnce() -> T + Send,
) -> Result<T, String> {
    let wait = Wait::on(WaitKind::at(std::panic::Location::caller()), party, asked);
    judging(wait, Stage::Awaited, asked, stop, call, None).0
}

/// [`judged`], and its wait's account as it ended, before its answer taught its kind: what
/// a test that expects a refusal says where none came (harness.rs
/// `a_judged_calls_own_work_is_its_progress`, unrefused on CI's arm64 Windows runner with no
/// account kept, run 44077430171).
#[track_caller]
pub fn judged_accounted<T: Send>(
    party: Party,
    asked: &str,
    stop: impl FnOnce(),
    call: impl FnOnce() -> T + Send,
) -> (Result<T, String>, String) {
    let wait = Wait::on(WaitKind::at(std::panic::Location::caller()), party, asked);
    judging(wait, Stage::Awaited, asked, stop, call, None)
}

/// [`judged_accounted`], its call told once its judge's looks have outlasted their rule: on the
/// channel its call takes, the due looks that found its party still since it last progressed,
/// held or based, each as far into the wait as it came, once they are more than
/// [`shards_api::LOOKS_TO_REFUSE`] and none refused it; the channel closing as its wait ends,
/// refused or not. Its call's thread is its party's member, so it hears nothing before: each
/// word wakes it, which its next look takes for progress ([`judging`]). What a test that bounds
/// a wait on a still party by its judge's own looks reads, where a span of time would be cut
/// short by a host that holds the looks late (harness.rs
/// `a_judged_calls_own_work_is_its_progress`).
#[track_caller]
pub fn judged_watching<T: Send>(
    party: Party,
    asked: &str,
    call: impl FnOnce(std::sync::mpsc::Receiver<Vec<Duration>>) -> T + Send,
) -> (Result<T, String>, String) {
    let wait = Wait::on(WaitKind::at(std::panic::Location::caller()), party, asked);
    let (still, looks) = std::sync::mpsc::channel();
    judging(
        wait,
        Stage::Awaited,
        asked,
        || {},
        move || call(looks),
        Some(still),
    )
}

/// [`judged`], of a call whose caller knows its answer is there already (what a member that
/// has ended said, read to its end): judged as [`judged`] is, should it stall, its answer
/// teaching its kind nothing, as it waited on no party (#77 waits-03: api_image.rs' reads of
/// an ended member's channel taught their kind a W* of 21 ms over which its party ran no CPU
/// at all, R* 0 ns, which refused a read that waited on an outside sign-in's round trip).
#[track_caller]
pub fn judged_there<T: Send>(
    party: Party,
    asked: &str,
    stop: impl FnOnce(),
    call: impl FnOnce() -> T + Send,
) -> Result<T, String> {
    let wait = Wait::on(WaitKind::at(std::panic::Location::caller()), party, asked);
    judging(wait, Stage::Begun, asked, stop, call, None).0
}

/// Where a judged call's own thread is ([`judging`]).
enum Calling {
    /// Spawned, not yet at its first line: its OS has it runnable, and it has run none of
    /// its call.
    Spawned,
    /// Running its call, followed by its own clock.
    Calls(ServerThread),
    /// Past its call, its clock let go of; its answer on its way.
    Called,
}

/// [`judged`]'s wait, its looks saying its answer missing where `stage` is
/// [`Stage::Awaited`], and never where it is [`Stage::Begun`]. `call` runs on a thread of
/// this process, shards-api's client, whose work (a spec's checks, an image's reads, a
/// request's writes) no process this test started shows: that thread is its party's member
/// too ([`ServerThread`]: its own CPU clock), so a call that works on it is progress and one
/// idle there is not (ROOT's batch 22, f385b8dac: api_image.rs' `refused` calls, a refusal in
/// 206 µs on HVF and one in 28.6 ms on KVM at 0 ns of the started processes' CPU, taught
/// their call sites a W* that refused the next call as it worked). The thread names itself
/// as it starts and is let go of before it returns, under one lock, so its clock is read
/// only while it lives ([`Calling`]).
fn judging<T: Send>(
    mut wait: Wait,
    stage: Stage,
    asked: &str,
    stop: impl FnOnce(),
    call: impl FnOnce() -> T + Send,
    mut watch: Option<std::sync::mpsc::Sender<Vec<Duration>>>,
) -> (Result<T, String>, String) {
    let calling = std::sync::Mutex::new(Calling::Spawned);
    let thread = || calling.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = scope.spawn(move || {
            *thread() = Calling::Calls(ServerThread::this());
            let answer = call();
            *thread() = Calling::Called;
            let _ = tx.send(answer);
        });
        let refused = loop {
            match rx.recv_timeout(wait.next_look()) {
                Ok(answer) => {
                    let account = wait.account();
                    wait.answered();
                    return (Ok(answer), account);
                }
                // Ended without sending: it panicked, and says why.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => match worker.join() {
                    Err(panic) => std::panic::resume_unwind(panic),
                    Ok(()) => return (Err(format!("{asked}: ended with no answer")), wait.account()),
                },
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
            match &mut *thread() {
                // Runnable and none of its call run: its host's hold, as a thread of its
                // that is runnable and not running is (CI 43968984258, aarch64 musl: a call
                // spinning for 200 ms, its thread not yet named 2.06 ms into its wait beside
                // the busy stand-ins of the tests around it, refused as idle, "its last
                // progress its start").
                Calling::Spawned => wait.call_looked(false, true),
                Calling::Calls(thread) => {
                    let before = thread.last;
                    wait.call_looked(thread.ran() > before, thread.runnable());
                }
                Calling::Called => {}
            }
            let looked = if stage == Stage::Awaited {
                wait.look(false)
            } else {
                wait.looked(false)
            };
            if let Err(stall) = looked {
                break wait.refused(&stall);
            }
            // Its watcher is told once, as its due looks have found its party still more times
            // than its judge's rule refuses within, none refusing it. Told of each change, its
            // call's own thread woke to hear it, and that run, its party's member's, was its
            // next due look's progress: its party never stayed still, so its wait was neither
            // refused nor told it had outlasted its rule (CI 44164945050 at 95247a79c, harness.rs
            // `a_judged_calls_own_work_is_its_progress`, to the Tests step's limit on every
            // Linux and macOS job and on arm64 Windows).
            if wait.still.len() > shards_api::LOOKS_TO_REFUSE as usize
                && let Some(still) = watch.take()
            {
                let _ = still.send(wait.still.clone());
            }
        };
        // Its watcher hears its wait end, refused, before its call is stopped.
        drop(watch.take());
        stop();
        let _ = worker.join();
        (Err(refused), wait.account())
    })
}

/// `s` booted by `images`, waited for until its run starts or its host ends (`Duration::MAX`)
/// and judged as the tests' waits are ([`judged`]), its kind its call site, its party
/// what this test's process started: refused, its boot is stopped.
#[track_caller]
#[allow(
    clippy::panic_in_result_fn,
    reason = "a refused wait fails its test; the boot's own refusal is its caller's"
)]
pub fn booted(
    images: &dyn shards_api::image::Images,
    s: &shards_api::image::ImageSpec,
) -> Result<Box<dyn shards_api::image::ImageGuest>, shards_api::image::ImageRefusal> {
    let stop = shards_api::image::BootStop::new();
    judged(
        Party::started(None),
        "a boot",
        || stop.stop(),
        || images.boot_stoppable(s, Duration::MAX, &stop),
    )
    .unwrap_or_else(|refused| panic!("{refused}"))
}

/// What `call` of `guest`'s answers, given `Duration::MAX` (until it answers or its run ends)
/// and judged as the tests' waits are ([`judged`]), its kind its call site, its party
/// what this test's process started: refused, its run is stopped, which ends the call.
#[track_caller]
pub fn ask<T: Send>(
    guest: &dyn shards_api::image::ImageGuest,
    call: impl FnOnce(&dyn shards_api::image::ImageGuest) -> T + Send,
) -> T {
    judged(
        Party::started(None),
        "a call of its run's",
        || {
            let _ = guest.stop();
        },
        || call(guest),
    )
    .unwrap_or_else(|refused| panic!("{refused}"))
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
            _own: OwnVm::started(child.id()),
            child,
            lines,
            seen: Vec::new(),
            log,
            size: String::new(),
        }
    }

    /// Its guest's size (its vCPUs, say), which keys its streamed waits' kinds beside their
    /// call sites ([`Heard::streaming_sized`]): one site's waits on guests of different sizes
    /// are kinds apart.
    pub fn sized(mut self, size: String) -> Vm {
        self.size = size;
        self
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
    #[track_caller]
    pub fn wait_for(&mut self, text: &str) {
        self.wait_for_any(&[text]);
    }

    /// Waits for an output line containing any of `texts`, and returns it, for as long as
    /// its guest says something new or its VM process runs ([`Heard::running`]), for a guest
    /// of fewer vCPUs than its host has CPUs: a wait judged as [`run_command`]'s is, its kind
    /// its call site. One refused fails, with what it said. A guest of more vCPUs than its
    /// host's CPUs is waited on by [`wait_for_any_streamed`](Self::wait_for_any_streamed),
    /// whose CPU time counts only as it starts.
    #[track_caller]
    pub fn wait_for_any(&mut self, texts: &[&str]) -> String {
        let heard = Heard::running(self.child.id());
        self.wait_lines(texts, heard)
    }

    /// [`wait_for_any`](Self::wait_for_any), its guest's progress what [`Heard::streaming`]
    /// takes for it: a line it has not said before, or more of `streamed`'s stream, and its
    /// VM process's CPU time only before its first line; for a guest of more vCPUs than its
    /// host has CPUs, which show CPU time for ever.
    #[track_caller]
    pub fn wait_for_any_streamed(&mut self, texts: &[&str], streamed: Option<&AtomicU64>) -> String {
        let heard = Heard::streaming_sized(self.child.id(), streamed, &self.size);
        self.wait_lines(texts, heard)
    }

    /// Waits for a line containing any of `texts` while `heard` holds its wait. The lines
    /// already come are read first: one of them is no answer it waited for, and teaches
    /// nothing (#77 waits-03).
    fn wait_lines(&mut self, texts: &[&str], mut heard: Heard<'_>) -> String {
        while let Ok(line) = self.lines.try_recv() {
            let found = texts.iter().any(|t| line.contains(t));
            self.hear(line.clone(), &mut heard);
            if found {
                return line;
            }
        }
        heard.wait.absent();
        loop {
            match self.lines.recv_timeout(heard.next_look()) {
                Ok(line) => {
                    let found = texts.iter().any(|t| line.contains(t));
                    self.hear(line.clone(), &mut heard);
                    if found {
                        heard.answered();
                        return line;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!(
                    "no {texts:?} from the VM before its output ended; it said:\n{}",
                    self.seen.join("\n")
                ),
            }
            if let Err(refused) = heard.look(!self.seen.is_empty()) {
                panic!(
                    "no {texts:?} from the VM: {refused}\nit said:\n{}",
                    self.seen.join("\n")
                );
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

    /// [`wait_exit`](Self::wait_exit), failing, with what it printed, if its wait is refused:
    /// its guest said nothing new and its VM process ran nothing over a whole window past
    /// the slowest end of its kind ([`Heard::running`]), for a guest of fewer vCPUs than its
    /// host has CPUs, as [`wait_for_any`](Self::wait_for_any) waits on one.
    #[track_caller]
    pub fn wait_exit_following(&mut self) -> Option<i32> {
        let heard = Heard::running(self.child.id());
        self.wait_end(heard)
    }

    /// [`wait_exit`](Self::wait_exit), failing, with what it printed, if its wait is refused:
    /// its guest said nothing new, nor carried more of `streamed`'s stream, over a whole
    /// window past the slowest end of its kind ([`Heard::streaming`]): a guest whose workload
    /// its host runs slowly (the parity plan's step 10, a 64-vCPU storm on 4 TCG CPUs) is
    /// waited on while it gets on with it.
    #[track_caller]
    pub fn wait_exit_streamed(&mut self, streamed: Option<&AtomicU64>) -> Option<i32> {
        let heard = Heard::streaming_sized(self.child.id(), streamed, &self.size);
        self.wait_end(heard)
    }

    /// Waits for the process to exit while `heard` holds its wait.
    fn wait_end(&mut self, mut heard: Heard<'_>) -> Option<i32> {
        while self.child.try_wait().unwrap().is_none() {
            if let Err(refused) = heard.look(!self.seen.is_empty()) {
                let _ = self.child.kill();
                self.wait_exit();
                panic!("still running: {refused}\nit said:\n{}", self.seen.join("\n"));
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
        heard.answered();
        self.wait_exit()
    }
}

/// A test's wait on a guest (D209), judged as [`run_command`]'s wait on a command is
/// ([`Wait`], D215's judge; the person's ruling of 2026-11-13): its kind is its call site
/// (`#[track_caller]` through the helpers that make one), whose answers
/// ([`answered`](Heard::answered)) teach W*, the slowest. Cold (no answer of its kind on this
/// host), it follows its guest's liveness alone, which its caller holds (its VM process's
/// end, its machine's); learned, it is looked at each W* past its start and refused at a look
/// that finds its guest made no progress at all since the look before, its host not holding
/// it. Its progress: a line it has not printed before, or bytes more of a stream a test
/// carries through it (`streamed`, as [`Following`] counts them); and its VM processes' CPU
/// time ([`Party`]), throughout ([`Heard::running`]), or only before its first line, while it
/// starts ([`Heard::streaming`]); for a machine of this process's own ([`Heard::console`]),
/// the CPU time of this process's threads beside the one that waits, throughout. CPU time is progress throughout for a guest of fewer vCPUs than its
/// host has CPUs: a guest onlining a gigabyte it plugged says nothing as it does (the parity
/// plan's step 10). It is only liveness once a guest of more vCPUs than that has its own
/// words ([`Cpu::Liveness`]): a 64-vCPU guest on 4 TCG CPUs shows it for ever, its stream
/// stopped or not, and a guest that repeats a line says nothing new.
pub struct Heard<'a> {
    wait: Wait,
    guest: Guest,
    streamed: Option<&'a AtomicU64>,
    bytes: u64,
    /// Whether it said something new since its last look.
    new: bool,
}

/// What a [`Heard`]'s guest's CPU time counts for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Guest {
    /// Fewer vCPUs than its host has CPUs: progress throughout.
    Running,
    /// More: progress until its first line, its liveness after.
    Streaming,
    /// A machine of this process's: this process's threads beside its waiter's, progress
    /// throughout ([`Party::machine`]); or, of more vCPUs than its host has CPUs, its
    /// liveness alone, its lines its progress ([`guest_says`]).
    Machine,
}

impl<'a> Heard<'a> {
    /// A wait at `site` for `awaited` ([`guest_text`]), or for its guest's progress alone
    /// where `awaited` is empty (an end, a state, a call's return).
    fn with(
        site: &'static std::panic::Location<'static>,
        party: Party,
        guest: Guest,
        streamed: Option<&'a AtomicU64>,
        awaited: &str,
    ) -> Heard<'a> {
        let kind = if awaited.is_empty() {
            WaitKind::awaiting(site, PROGRESS)
        } else {
            WaitKind::of(site, PROGRESS, &guest_text(awaited))
        };
        Heard::keyed(kind, party, guest, streamed)
    }

    fn keyed(
        kind: &'static WaitKind,
        party: Party,
        guest: Guest,
        streamed: Option<&'a AtomicU64>,
    ) -> Heard<'a> {
        Heard {
            wait: Wait::on(kind, party, PROGRESS),
            guest,
            bytes: streamed.map_or(0, |s| s.load(Ordering::Relaxed)),
            streamed,
            new: false,
        }
    }

    /// The guest of the VM process `vm` is or started (`run --kernel`, `restore`), its CPU
    /// time counted only before its first line, and `streamed`'s bytes as it carries them.
    #[track_caller]
    pub fn streaming(vm: u32, streamed: Option<&'a AtomicU64>) -> Heard<'a> {
        Heard::streaming_sized(vm, streamed, "")
    }

    /// [`streaming`](Self::streaming), its guest of size `size` (its vCPUs), which keys its
    /// kind beside its call site: one site's waits on guests of different sizes are kinds
    /// apart, so that a 64-vCPU storm is not judged by a 2-vCPU one's W* (HVF at ddb88cd47:
    /// "still running" refused at a W* of 49.9 ms and of 753 ms learned of smaller storms).
    #[track_caller]
    pub fn streaming_sized(vm: u32, streamed: Option<&'a AtomicU64>, size: &str) -> Heard<'a> {
        let kind = WaitKind::of(std::panic::Location::caller(), PROGRESS, size);
        Heard::keyed(kind, Party::client(vm, None), Guest::Streaming, streamed)
    }

    /// The guest of the VM process `vm` is or started (`run --kernel`, `restore`), its CPU
    /// time counted throughout: for a guest of fewer vCPUs than its host has CPUs.
    #[track_caller]
    pub fn running(vm: u32) -> Heard<'static> {
        Heard::with(
            std::panic::Location::caller(),
            Party::client(vm, None),
            Guest::Running,
            None,
            "",
        )
    }

    /// The guest of a machine this process runs (`vm::start`), waiting for `awaited` (a line,
    /// with what it was asked for it; empty for its end), its kind its call site's and
    /// `awaited`'s ([`guest_text`]): by what it says, and by the CPU of this process's threads
    /// beside the one that waits ([`Party::machine`]), its vCPUs and the host's work for them
    /// among them.
    #[track_caller]
    pub fn console(awaited: &str) -> Heard<'static> {
        Heard::with(
            std::panic::Location::caller(),
            Party::machine(),
            Guest::Machine,
            None,
            awaited,
        )
    }

    /// A wait of `kind` for a line of a guest's ([`guest_says`]), `asked` naming which: by its
    /// lines, its caller beginning another wait as each comes, and by the CPU of this
    /// process's threads beside the one that waits read as `cpu` says ([`Party::machine`]).
    fn lines(kind: &'static WaitKind, asked: &str, cpu: Cpu) -> Heard<'static> {
        let mut party = Party::machine();
        party.read_cpu(cpu);
        Heard {
            wait: Wait::on(kind, party, asked),
            guest: Guest::Machine,
            streamed: None,
            bytes: 0,
            new: false,
        }
    }

    /// Something it said, `new` where it had not said it before.
    pub fn said(&mut self, new: bool) {
        self.new |= new;
    }

    /// Looks at its guest where a look is due; `started`, whether it has printed a line,
    /// past which a streaming guest's CPU time is its liveness alone. Refused, what its wait
    /// says of itself.
    pub fn look(&mut self, started: bool) -> Result<(), String> {
        if let Some(s) = self.streamed {
            let bytes = s.load(Ordering::Relaxed);
            if bytes != self.bytes {
                self.bytes = bytes;
                self.new = true;
            }
        }
        if started && self.guest == Guest::Streaming {
            self.wait.party.read_cpu(Cpu::Liveness);
        }
        let new = std::mem::take(&mut self.new);
        self.wait.look(new).map_err(|stall| self.wait.refused(&stall))
    }

    /// How long until its next look is due ([`Wait::next_look`]).
    pub fn next_look(&self) -> Duration {
        self.wait.next_look()
    }

    /// What it waited for came: its kind's answer.
    pub fn answered(&mut self) {
        self.wait.answered();
    }

    /// Its caller found what it waits for missing ([`Wait::absent`]): a party's end it
    /// started just now, which no check finds there before it.
    pub fn absent(&mut self) {
        self.wait.absent();
    }

    /// What its kind has learned on this host.
    pub fn seen(&self) -> shards_api::Seen {
        self.wait.kind.record().seen()
    }
}

/// How a machine this process runs (`vm::start`) ends, as `ended` says (a thread's
/// `running.wait()`), waited on while its guest writes something new to its console
/// ([`Heard::console`]): refused, its machine stopped and what its wait says of itself.
#[track_caller]
pub fn machine_ended(
    handle: &shards_vmm::vm::Handle,
    ended: &std::sync::mpsc::Receiver<shards_vmm::vm::ExitReason>,
) -> Result<shards_vmm::vm::ExitReason, String> {
    let mut heard = Heard::console("");
    let mut console = handle.console_said();
    loop {
        match ended.recv_timeout(heard.next_look()) {
            Ok(reason) => {
                heard.answered();
                return Ok(reason);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                handle.stop();
                return Err("its machine's thread ended without its reason".into());
            }
        }
        let now = handle.console_said();
        heard.said(now != console);
        console = now;
        if let Err(refused) = heard.look(true) {
            handle.stop();
            return Err(refused);
        }
    }
}

/// What [`guest_says`] awaits first, and keys that wait's kind by: its guest's first line,
/// from its wait's start.
pub const FIRST_LINE: &str = "its guest's first line";
/// What each of [`guest_says`]' waits after its first awaits, and keys their kind by: its
/// guest's next line, from the line before.
pub const NEXT_LINE: &str = "its guest's next line";

/// What a guest said as [`guest_says`] read it: each line as a read first found it, and the
/// time from the wait's start at which that read began.
pub struct Lines {
    began: Instant,
    seen: Vec<(Duration, String)>,
    known: std::collections::BTreeSet<String>,
}

impl Lines {
    fn new() -> Lines {
        Lines {
            began: Instant::now(),
            seen: Vec::new(),
            known: std::collections::BTreeSet::new(),
        }
    }

    /// Takes in a read of its guest, `said` its whole lines: each line it had not said before
    /// is kept, and said on stderr after `tag` and its time where a tag is given, which CI's
    /// log keeps whatever ends the test, a step's limit among them. Returns how many lines it
    /// had before: those past them are new.
    fn heard(&mut self, said: &[u8], tag: Option<&str>) -> usize {
        let (before, at) = (self.seen.len(), self.began.elapsed());
        for line in String::from_utf8_lossy(said).lines() {
            if self.known.insert(line.to_string()) {
                if let Some(tag) = tag {
                    let _ = writeln!(io::stderr(), "{tag} {at:?}: {line}");
                }
                self.seen.push((at, line.to_string()));
            }
        }
        before
    }

    /// Each line, after the time from the wait's start at which the read that first found
    /// it began.
    pub fn seen(&self) -> &[(Duration, String)] {
        &self.seen
    }

    /// Every line, as its guest said them.
    pub fn text(&self) -> String {
        self.seen
            .iter()
            .map(|(_, line)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every line, after the time a read first found it: what a failure says.
    pub fn timed(&self) -> String {
        self.seen
            .iter()
            .map(|(at, line)| format!("{at:.3?} {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// How [`guest_says`]' wait ended.
#[derive(Debug)]
pub enum Saying<E> {
    /// Its guest said a line with its text: the first such, and its machine's end where that
    /// came before the read that found it.
    Said { line: String, ended: Option<E> },
    /// Its machine ended and its guest had not said it: its end, none where the channel that
    /// tells it closed without one.
    Ended(Option<E>),
    /// Its wait was refused: what its wait said of itself.
    Refused(String),
}

/// Whether a guest of `vcpus` vCPUs has more than this host has CPUs, and so runs them for
/// ever, its workload stopped or not ([`Cpu::Liveness`]).
fn wider_than_host(vcpus: u32) -> bool {
    let cpus = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    usize::try_from(vcpus).is_ok_and(|vcpus| vcpus > cpus)
}

/// Waits for a guest to say a line containing `text`, each read of its whole lines `read`'s,
/// until `ended`, its machine's end, comes; judged by its guest's own progress, its lines
/// (D261): each line it had not said before.
/// - Each wait is judged as D215's judge judges ([`Heard`]), its party this process's threads
///   beside the one that waits ([`Party::machine`]), whose CPU is the liveness alone of a
///   guest of more vCPUs than its host has CPUs (`vcpus`), and its progress otherwise.
/// - Its first line's wait is a kind of its own ([`FIRST_LINE`]), beside its call site and
///   `size`. Each line that comes begins a wait for the next, of another ([`NEXT_LINE`]), whose
///   W* is the slowest gap between two lines seen on this host, so that a guest silent for
///   longer than that is refused there, however long its boot.
/// - Only a pass teaches them: once it has found its text, what its waits took (the time to its
///   first line, where it was found missing first; the slowest gap after it) is taught to their
///   kinds and kept on this host. A guest that stalls, or a wait refused, teaches nothing.
/// - Its guest is read as its read's own cost weighs against the wait so far (`shards_api::
///   pause`, D215's pace), and before each look its judge makes, which sees what it found.
/// - Its machine's end ends it, what its guest said by then read first.
#[track_caller]
pub fn guest_says<E>(
    mut read: impl FnMut() -> Vec<u8>,
    ended: &std::sync::mpsc::Receiver<E>,
    text: &str,
    size: &str,
    vcpus: u32,
    tag: Option<&str>,
) -> (Saying<E>, Lines) {
    let site = std::panic::Location::caller();
    let (first, next) = (
        WaitKind::of(site, FIRST_LINE, size),
        WaitKind::of(site, NEXT_LINE, size),
    );
    let cpu = if wider_than_host(vcpus) {
        Cpu::Liveness
    } else {
        Cpu::Progress
    };
    let mut lines = Lines::new();
    let mut heard = Heard::lines(first, FIRST_LINE, cpu);
    // When its last lines were read, from its start; what its first line's wait took, and
    // the slowest wait for a next line since, each where its wait found it missing first.
    let (mut last, mut first_came, mut slowest) = (Duration::ZERO, None, None::<Duration>);
    let mut read_at = Instant::now();
    loop {
        if Instant::now() >= read_at {
            let began = Instant::now();
            let from = lines.heard(&read(), tag);
            let cost = began.elapsed();
            if let Some(&(at, _)) = lines.seen.get(from) {
                if heard.wait.awaited() {
                    let waited = at.saturating_sub(last);
                    if from == 0 {
                        first_came = Some(waited);
                    } else {
                        slowest = Some(slowest.map_or(waited, |s| s.max(waited)));
                    }
                }
                last = at;
                if let Some(line) = lines.seen[from..].iter().find(|(_, l)| l.contains(text)) {
                    let line = line.1.clone();
                    for (kind, waited) in [(first, first_came), (next, slowest)] {
                        if let Some(waited) = waited {
                            kind.record().teach_unrated(waited);
                            kind.keep();
                        }
                    }
                    return (Saying::Said { line, ended: None }, lines);
                }
                heard = Heard::lines(next, NEXT_LINE, cpu);
            }
            let left = heard
                .wait
                .judge
                .due()
                .map_or(Duration::MAX, |due| due.saturating_duration_since(Instant::now()));
            read_at = Instant::now() + shards_api::pause(cost, heard.wait.began.elapsed(), left);
        }
        if let Err(refused) = heard.look(true) {
            return (Saying::Refused(refused), lines);
        }
        let pause = read_at
            .saturating_duration_since(Instant::now())
            .min(heard.next_look());
        let end = match ended.recv_timeout(pause) {
            Ok(end) => Some(end),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
        };
        let from = lines.heard(&read(), tag);
        let saying = match lines.seen[from..].iter().find(|(_, l)| l.contains(text)) {
            Some((_, line)) => Saying::Said {
                line: line.clone(),
                ended: end,
            },
            None => Saying::Ended(end),
        };
        return (saying, lines);
    }
}

/// [`guest_says`] of a machine this process runs, `handle`, whose end `ended` says (a
/// thread's `running.wait()`): its lines those of its console, or, while its console has
/// carried nothing, of its kernel's printk ring, read from its memory
/// ([`Handle::console_said`](shards_vmm::vm::Handle::console_said), D261: hvc0 registers only
/// at device_initcall, and its ring is found from subsys_initcall on, as VMCOREINFO is
/// published). Once it has said it, or its wait is refused, its machine is stopped and its
/// end waited for ([`machine_ended`]), so that none of it runs once this returns. The line
/// and what it said; or what failed, and what it said, each line after when a read first
/// found it.
#[track_caller]
pub fn machine_says(
    handle: &shards_vmm::vm::Handle,
    ended: &std::sync::mpsc::Receiver<shards_vmm::vm::ExitReason>,
    text: &str,
    size: &str,
    vcpus: u32,
    tag: Option<&str>,
) -> Result<(String, Lines), String> {
    use shards_vmm::vm::ExitReason;
    let (saying, lines) = guest_says(|| handle.console_said().lines, ended, text, size, vcpus, tag);
    let stop = match saying {
        Saying::Said { ended: None, .. } | Saying::Refused(_) => {
            handle.stop();
            machine_ended(handle, ended).err()
        }
        Saying::Said { ended: Some(_), .. } | Saying::Ended(_) => None,
    };
    let failed = match saying {
        Saying::Said { line, .. } if stop.is_none() => return Ok((line, lines)),
        Saying::Said { .. } => format!("its guest said {text:?}"),
        // Its end's own line, a vCPU's "VM entry failed" among them (D261's A/B), on a line
        // of its own.
        Saying::Ended(Some(ExitReason::Error(e))) => {
            format!("no {text:?} from its guest: its machine ended:\n{e}")
        }
        // The host's refusal to enter a vCPU (D261), said as the host's.
        Saying::Ended(Some(ExitReason::Refused(refused))) => {
            format!("no {text:?} from its guest: its machine ended:\n{refused}")
        }
        Saying::Ended(end) => format!("no {text:?} from its guest: its machine ended: {end:?}"),
        Saying::Refused(refused) => format!("no {text:?} from its guest: {refused}"),
    };
    let stop = stop.map_or_else(String::new, |refused| {
        format!("\nstopped, its machine did not end: {refused}")
    });
    Err(format!(
        "{failed}{stop}\nwhat it said, each line after when a read first found it:\n{}",
        lines.timed()
    ))
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A process this test started, killed and reaped as it drops, as [`Vm`] is: as its test ends,
/// and as a failing test's panic unwinds past it, which leaves a bare `std::process::Child`
/// running (CI 43968984258, x86_64 gnu: boot.rs' rate test failed past its boot, and its guest,
/// told to spin for ten minutes, ran 6.5 minutes past the test binary's end). On Unix `run
/// --kernel` becomes the VM process; on Windows its VM process is in a job that ends with it.
pub struct Reaped(pub std::process::Child);

impl std::ops::Deref for Reaped {
    type Target = std::process::Child;
    fn deref(&self) -> &std::process::Child {
        &self.0
    }
}

impl std::ops::DerefMut for Reaped {
    fn deref_mut(&mut self) -> &mut std::process::Child {
        &mut self.0
    }
}

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A host program's end of a guest's vsock stream: a Unix stream socket; on Windows its two
/// pipes (D272, [`PipeStream`]).
#[cfg(unix)]
pub type VsockStream = shards_vmm::platform::UnixStream;
#[cfg(windows)]
pub type VsockStream = PipeStream;

/// A dial of the VM's socket `sock`, no handshake said: a Unix socket's connect; on Windows
/// the pair of pipes its file names, opened its way down first, waited for while another
/// client holds them (D272).
pub fn vsock_dial(sock: &Path) -> io::Result<VsockStream> {
    #[cfg(unix)]
    return shards_vmm::platform::UnixStream::connect(sock);
    #[cfg(windows)]
    PipeStream::connect(sock)
}

/// A host port's listener at `path` (`<sock>_<port>`), which the guest's dials reach: a Unix
/// socket; on Windows a pair of pipes named at the path (D272).
#[cfg(unix)]
pub type HostPort = shards_vmm::platform::UnixListener;
#[cfg(windows)]
pub type HostPort = shards_ipc::VsockListener;

/// Listens at `path` for the guest's dials to a host port.
pub fn host_port(path: impl AsRef<Path>) -> HostPort {
    #[cfg(unix)]
    return shards_vmm::platform::UnixListener::bind(path).unwrap();
    #[cfg(windows)]
    shards_ipc::VsockListener::bind(path.as_ref()).unwrap()
}

/// The guest's next dial of `port`'s host port.
pub fn host_port_accept(port: &mut HostPort) -> io::Result<VsockStream> {
    #[cfg(unix)]
    return port.accept().map(|(s, _)| s);
    #[cfg(windows)]
    port.accept().map(PipeStream::served)
}

/// A host program's end of a guest's vsock stream on Windows (D272): the pipe it reads and
/// the one it writes, as `shards_api`'s client holds a guest connection and a host port's
/// listener its client's; its read timeout its own, its reads overlapped calls waited for to
/// it; its writes as `std` writes a file, each waited for to its end, a refused one's error
/// the OS's own (ERROR_NO_DATA, 232, once its reader has closed its end).
#[cfg(windows)]
#[derive(Debug)]
pub struct PipeStream {
    reads: shards_ipc::Pipe,
    /// None once its writes are shut down, which its peer reads as their end.
    writes: std::sync::Mutex<Option<std::fs::File>>,
    timeout: std::sync::Mutex<Option<Duration>>,
}

#[cfg(windows)]
impl PipeStream {
    /// A client's end of the pair `sock`'s file names: its way down read, its way up written.
    pub fn connect(sock: &Path) -> io::Result<PipeStream> {
        let (down, up) = shards_ipc::vsock_open(sock, None)?;
        Ok(PipeStream {
            reads: shards_ipc::Pipe::from_handle(down),
            writes: std::sync::Mutex::new(Some(std::fs::File::from(up))),
            timeout: std::sync::Mutex::new(None),
        })
    }

    /// A listener's end of its client's connection: its client's way up read, its way down
    /// written.
    pub fn served(conn: shards_ipc::VsockConn) -> PipeStream {
        let shards_ipc::VsockConn { down, up } = conn;
        PipeStream {
            reads: up,
            writes: std::sync::Mutex::new(Some(std::fs::File::from(
                std::os::windows::io::OwnedHandle::from(down),
            ))),
            timeout: std::sync::Mutex::new(None),
        }
    }

    /// Ends its writes (`Write`, `Both`): its way written closed, which its peer reads as
    /// their end after every byte. Its reads go on: a pipe has no half-close of its own.
    pub fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        if how != std::net::Shutdown::Read {
            *self.writes.lock().unwrap() = None;
        }
        Ok(())
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *self.timeout.lock().unwrap() = timeout;
        Ok(())
    }
}

#[cfg(windows)]
impl Timed for PipeStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        PipeStream::set_read_timeout(self, timeout)
    }

    /// What it holds, read without waiting: its end where its writers have gone and it holds
    /// nothing (`Pipe::peek`'s None), as a socket's is ([`recv_now`]); WouldBlock where nothing
    /// is yet.
    fn read_now(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self.reads.peek()? {
            None => Ok(0),
            Some(0) => Err(io::ErrorKind::WouldBlock.into()),
            Some(there) => {
                let len = buf.len().min(there);
                match self.reads.read_by(&mut buf[..len], Some(Instant::now())) {
                    Err(e) if e.kind() == io::ErrorKind::TimedOut => Err(io::ErrorKind::WouldBlock.into()),
                    read => read,
                }
            }
        }
    }
}

#[cfg(windows)]
impl Read for &PipeStream {
    /// A read to its timeout, `TimedOut` past it, as a Windows socket's says.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let timeout = *self.timeout.lock().unwrap();
        let deadline = timeout.and_then(|t| Instant::now().checked_add(t));
        self.reads.read_by(buf, deadline)
    }
}

#[cfg(windows)]
impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buf)
    }
}

#[cfg(windows)]
impl Write for &PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.writes.lock().unwrap().as_mut() {
            Some(file) => file.write(buf),
            None => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Opens a stream to guest vsock port `port` through the VM's socket `sock`: `Ok` once
/// the guest accepted (the `OK` line was read), `Err` with what came back otherwise. Its
/// handshake is read through [`Following`], its wait's kind its call site; the stream comes
/// back reading with no timeout, its caller's to set.
#[track_caller]
pub fn vsock_connect(sock: &Path, port: u32) -> Result<VsockStream, String> {
    let s = vsock_dial(sock).map_err(|e| format!("{}: {e}", sock.display()))?;
    let mut r = Following::of(&s, None);
    vsock_handshake(&s, &mut r, port)?;
    r.answered();
    s.set_read_timeout(None).map_err(|e| e.to_string())?;
    Ok(s)
}

/// Asks the VM's socket `s` for guest port `port`, reading its answer through `r`.
fn vsock_handshake(mut s: impl Write, r: &mut impl Read, port: u32) -> Result<(), String> {
    s.write_all(format!("CONNECT {port}\n").as_bytes())
        .map_err(|e| format!("CONNECT: {e}"))?;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.last() != Some(&b'\n') {
        match r.read(&mut byte) {
            Ok(1) => line.push(byte[0]),
            Ok(_) => return Err(format!("refused after {:?}", String::from_utf8_lossy(&line))),
            Err(e) => {
                return Err(format!(
                    "no answer after {:?}: {e}",
                    String::from_utf8_lossy(&line)
                ));
            }
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

/// A guest's stream, read for as long as it carries bytes: a wait judged as
/// [`run_command`]'s is ([`Wait`]), its kind its call site, its end (the stream's close) its
/// answer, and its bytes its progress, whatever its microVM's CPU does (a guest on more vCPUs
/// than its host's CPUs runs them all for ever): its party is this test's own VM processes,
/// their CPU their progress only until its first byte, while its peer starts it (a
/// handshake's answer, a guest's greeting: the vsock device and the guest's kernel at work,
/// with no byte to show for it), and their liveness alone after ([`Cpu::Liveness`]), as a
/// streaming guest's is past its first line ([`Heard::streaming`]); a stop or suspension of
/// theirs its host's hold. Cold, it reads with no timeout, its peer's liveness its own (the VM process
/// that holds the stream's other end closes it as it ends); learned, each read waits until
/// its judge's look, which refuses a stream that carried nothing over a whole W* past the
/// look before. What it reads is added to `carried`, where a wait on its guest follows it
/// ([`Vm::wait_exit_streamed`]). A stream a home's daemon carries to its guest (a published
/// port's) is read with that daemon in its party ([`Following::of_home`]).
pub struct Following<'a, S: Timed = shards_vmm::platform::UnixStream> {
    stream: &'a S,
    carried: Option<&'a AtomicU64>,
    wait: Wait,
}

/// A stream whose reads wait at most a time of its caller's: a guest's vsock stream, or a
/// TCP stream to a port its daemon publishes.
pub trait Timed {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    /// A read of what is there now, waiting for nothing (WouldBlock where nothing is), that
    /// leaves its descriptor's mode as it was. A non-blocking mode is its open socket's,
    /// which every clone of it shares: `echo`'s writer, writing through one while a read
    /// had set it, was refused (EAGAIN) as its peer stopped reading and ended, and the
    /// stream stood still for good (HVF at f385b8dac, storm through its snapshot, D209).
    fn read_now(&self, buf: &mut [u8]) -> io::Result<usize>;
}

/// [`Timed::read_now`] of socket `fd`: recv(2) with `MSG_DONTWAIT`, this call's alone.
#[cfg(unix)]
fn recv_now(fd: std::os::fd::RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: recv(2) into `buf`, of its length, from a socket its caller holds open.
    let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
    usize::try_from(n).map_err(|_| io::Error::last_os_error())
}

/// [`Timed::read_now`] of socket `socket` on Windows, which has no `MSG_DONTWAIT`: as many
/// bytes as are there (ioctlsocket `FIONREAD`), read by `read`, which cannot wait for them.
/// Where none are, its end may be, which FIONREAD counts nothing of and WSAPoll sees (as
/// afunix's reads see a peer's end): then `read`, which returns at once, its end or what came
/// since, as Unix's recv(2) with `MSG_DONTWAIT` reads its end. An end there read as absent
/// made its reader's wait one of a look, which taught its kind (CI's arm64 Windows runner,
/// run 44007524337: harness.rs `an_answer_already_there_teaches_nothing`, W* 8.86 ms). Else
/// WouldBlock.
#[cfg(windows)]
fn recv_now(
    socket: std::os::windows::io::BorrowedSocket<'_>,
    buf: &mut [u8],
    read: impl FnOnce(&mut [u8]) -> io::Result<usize>,
) -> io::Result<usize> {
    use std::os::windows::io::AsRawSocket as _;
    use windows_sys::Win32::Networking::WinSock::{
        FIONREAD, POLLRDNORM, SOCKET, WSAPOLLFD, WSAPoll, ioctlsocket,
    };
    let mut there: u32 = 0;
    // SAFETY: FIONREAD's count into a local, of a socket its caller holds open.
    if unsafe { ioctlsocket(socket.as_raw_socket() as SOCKET, FIONREAD, &raw mut there) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let there = usize::try_from(there).unwrap_or(usize::MAX);
    if there == 0 {
        let mut fd = WSAPOLLFD {
            fd: socket.as_raw_socket() as SOCKET,
            events: POLLRDNORM,
            revents: 0,
        };
        // SAFETY: one entry, a socket its caller holds open, polled without waiting.
        return match unsafe { WSAPoll(&raw mut fd, 1, 0) } {
            n if n < 0 => Err(io::Error::last_os_error()),
            0 => Err(io::ErrorKind::WouldBlock.into()),
            _ => read(buf),
        };
    }
    let len = buf.len().min(there);
    read(buf.get_mut(..len).unwrap_or_default())
}

/// A read timeout set, or none needed: macOS refuses SO_RCVTIMEO with EINVAL on a socket both
/// of whose ways are shut, as its peer's close leaves a Unix stream's (XNU bsd/kern/uipc_socket.c,
/// `sosetoptlock`: SS_CANTRCVMORE and SS_CANTSENDMORE both set; on macOS 26.2 a pair whose peer
/// wrote and closed refused it, a half-closed one took it, and the bytes written were still read
/// whole). A read of such a socket waits for nothing: it returns what is left, then its end.
fn timed(set: io::Result<()>) -> io::Result<()> {
    match set {
        #[cfg(target_os = "macos")]
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        set => set,
    }
}

impl Timed for shards_vmm::platform::UnixStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        timed(shards_vmm::platform::UnixStream::set_read_timeout(self, timeout))
    }
    fn read_now(&self, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        return recv_now(std::os::fd::AsRawFd::as_raw_fd(self), buf);
        #[cfg(windows)]
        return recv_now(std::os::windows::io::AsSocket::as_socket(self), buf, |b| {
            let mut s = self;
            s.read(b)
        });
    }
}

impl Timed for std::net::TcpStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        timed(std::net::TcpStream::set_read_timeout(self, timeout))
    }
    fn read_now(&self, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        return recv_now(std::os::fd::AsRawFd::as_raw_fd(self), buf);
        #[cfg(windows)]
        return recv_now(std::os::windows::io::AsSocket::as_socket(self), buf, |b| {
            let mut s = self;
            s.read(b)
        });
    }
}

impl<'a, S: Timed> Following<'a, S>
where
    &'a S: Read,
{
    /// Reads of `stream` from now on.
    #[track_caller]
    pub fn of(stream: &'a S, carried: Option<&'a AtomicU64>) -> Following<'a, S> {
        Following::with(stream, carried, None, std::panic::Location::caller())
    }

    /// Reads of `stream`, which `home`'s daemon carries to its guest, from now on: that
    /// daemon's words and its holds its party's too.
    #[track_caller]
    pub fn of_home(stream: &'a S, home: &Path) -> Following<'a, S> {
        Following::with(stream, None, Some(home), std::panic::Location::caller())
    }

    fn with(
        stream: &'a S,
        carried: Option<&'a AtomicU64>,
        home: Option<&Path>,
        site: &'static std::panic::Location<'static>,
    ) -> Following<'a, S> {
        Following::keyed(stream, carried, home, WaitKind::awaiting(site, STREAM))
    }

    /// Reads of `stream` from now on, from a guest of size `size` (its vCPUs), which keys
    /// its kind beside its call site: one site's streams from guests of different sizes are
    /// kinds apart, so that a 64-vCPU storm's stream is not judged by a 2-vCPU one's W*
    /// (HVF at ddb88cd47: "64 vCPUs: stream 6", refused 35 s past a W* of 17.6 s).
    #[track_caller]
    pub fn sized(stream: &'a S, carried: Option<&'a AtomicU64>, size: &str) -> Following<'a, S> {
        let kind = WaitKind::of(std::panic::Location::caller(), STREAM, size);
        Following::keyed(stream, carried, None, kind)
    }

    fn keyed(
        stream: &'a S,
        carried: Option<&'a AtomicU64>,
        home: Option<&Path>,
        kind: &'static WaitKind,
    ) -> Following<'a, S> {
        Following {
            stream,
            carried,
            wait: Wait::on(kind, Party::own(home), STREAM),
        }
    }

    /// A read of what is there now, waiting for nothing: WouldBlock where nothing is.
    fn read_now(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read_now(buf)
    }

    /// It read `n` bytes: `carried` counts them, and from its first its party's CPU is its
    /// liveness alone.
    fn took(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.wait.party.read_cpu(Cpu::Liveness);
        if let Some(c) = self.carried {
            c.fetch_add(n as u64, Ordering::Relaxed);
        }
    }

    /// What it read to came: its kind's answer.
    pub fn answered(&mut self) {
        self.wait.answered();
    }
}

impl<'a, S: Timed> Read for Following<'a, S>
where
    &'a S: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Until a read finds nothing there yet, what it reads was there as its wait began:
        // read without waiting, its end then no answer it waited for (#77 waits-03).
        if !self.wait.awaited() {
            match self.read_now(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.wait.absent(),
                Ok(n) => {
                    self.took(n);
                    return Ok(n);
                }
                Err(e) => return Err(e),
            }
        }
        loop {
            let left = self.wait.next_look();
            // Every look comes after a read, without waiting where the look is due now: an
            // answer there is read, never judged missing. A look that costs more than the
            // judge's W* leaves none to wait (`pause` at most what is left), and a loop of
            // looks with no read between them refused answers that had come (KVM at
            // cf13e49e6: each look's `ps` 16 to 25 ms against a W* of 8 ms; a guest's close
            // there 128 ms unread, its VM process idle waiting on the host's next word).
            let read = if left.is_zero() {
                self.read_now(buf)
            } else {
                let mut stream = self.stream;
                stream
                    .set_read_timeout(self.wait.judged().then_some(left))
                    .and_then(|()| stream.read(buf))
            };
            match read {
                Ok(0) => {
                    self.wait.answered();
                    return Ok(0);
                }
                Ok(n) => {
                    self.took(n);
                    self.wait.look(true).map_err(|stall| {
                        io::Error::new(io::ErrorKind::TimedOut, self.wait.refused(&stall))
                    })?;
                    return Ok(n);
                }
                // A timeout says WouldBlock through SO_RCVTIMEO, TimedOut on Windows.
                Err(e)
                    if !matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                    ) =>
                {
                    return Err(e);
                }
                Err(_) => {}
            }
            self.wait
                .look(false)
                .map_err(|stall| io::Error::new(io::ErrorKind::TimedOut, self.wait.refused(&stall)))?;
        }
    }
}

/// Streams `len` bytes of pattern `salt` through the guest's echo on `port` and checks
/// what comes back, byte for byte, after the half-close: read through [`Following`], its
/// kind its call site, and what comes back is added to `carried`.
#[track_caller]
pub fn echo(
    sock: &Path,
    port: u32,
    salt: u64,
    len: usize,
    carried: Option<&AtomicU64>,
) -> Result<(), String> {
    echo_of(sock, port, salt, len, carried, "")
}

/// [`echo`] through the echo of a guest of size `size` (its vCPUs), which keys its stream's
/// kind ([`Following::sized`]).
#[track_caller]
pub fn echo_of(
    sock: &Path,
    port: u32,
    salt: u64,
    len: usize,
    carried: Option<&AtomicU64>,
    size: &str,
) -> Result<(), String> {
    let s = vsock_dial(sock).map_err(|e| format!("{}: {e}", sock.display()))?;
    let mut r = Following::sized(&s, carried, size);
    vsock_handshake(&s, &mut r, port)?;
    // The writer shares the stream: a pipe's end is one handle, whose close alone is its end
    // for its peer (D272), as a socket's shutdown is its own whatever holds it.
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| -> Result<(), String> {
            let mut w = &s;
            let mut buf = vec![0u8; 256 * 1024];
            let mut sent = 0;
            while sent < len {
                let n = buf.len().min(len - sent);
                shards_testguest::fill(salt, sent as u64, &mut buf[..n]);
                w.write_all(&buf[..n]).map_err(|e| format!("sending: {e}"))?;
                sent += n;
            }
            s.shutdown(std::net::Shutdown::Write).map_err(|e| e.to_string())
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
    })
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
    let port = minted_port(listener.local_addr().unwrap().port());
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
    let port = minted_port(listener.local_addr().unwrap().port());
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
    let port = minted_port(listener.local_addr().unwrap().port());
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
    let port = minted_port(listener.local_addr().unwrap().port());
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
        self.word(b"\n");
    }

    /// Its host's word that it has `bytes` of memory available, in place of its kernel's
    /// count (D252): every charge its daemon makes of a run's buffers is held to it once its
    /// daemon has heard it, which it says in its log (`told the host has N bytes`), as a
    /// microVM runs (D188: its watcher listens then alone).
    pub fn available(&mut self, bytes: u64) {
        self.word(format!("available {bytes}\n").as_bytes());
    }

    fn word(&mut self, line: &[u8]) {
        #[cfg(unix)]
        std::fs::OpenOptions::new()
            .write(true)
            .open(&self.path)
            .unwrap()
            .write_all(line)
            .unwrap();
        #[cfg(windows)]
        self.server.write_all(line).unwrap();
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
    /// `path` held so, waiting out another's handle that shares nothing: a scanner's
    /// (Defender's), its host's hold, waited out however long it holds, each attempt paced
    /// as D215 paces a wait's looks (`shards_api::pause`: its own cost against the wait so
    /// far).
    pub fn new(path: &Path) -> Oplocked {
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
        let began = Instant::now();
        let file = loop {
            let tried = Instant::now();
            match std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .custom_flags(FILE_FLAG_OVERLAPPED)
                .open(path)
            {
                Ok(file) => break file,
                Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION.cast_signed()) => {
                    std::thread::sleep(shards_api::pause(tried.elapsed(), began.elapsed(), Duration::MAX));
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
