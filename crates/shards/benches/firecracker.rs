//! Firecracker comparison: shards and Firecracker boot the same guest on the same host,
//! interleaved, and each is measured the same way. Linux with KVM only.
//!
//! `cargo bench -p shards --bench firecracker [-- --runs N --cpus N --memory MIB]`
//!
//! Both VMMs boot the pinned kernel with the same initrd (the test guest in `idle` mode as
//! `/init`) and the same kernel command line. Every sample is a fresh VMM process:
//!
//! - `to_ready`: the host's clock from spawn until the guest's ready line reaches the VMM's
//!   stdout. It covers exec, VMM setup, the kernel and init: what a user waits for.
//! - `overhead`: the VMM's resident memory outside guest memory, by Firecracker's own rule
//!   (tests/host_tools/memory.py): every mapping in /proc/PID/smaps counts except those
//!   sized like guest memory. The highest of 20 readings, 10 ms apart, once the guest idles.
//! - `peak_rss`: the VMM's peak resident set, guest memory included: `VmHWM` in
//!   /proc/PID/status, read once the readings are done. Not wait4(2)'s `ru_maxrss`, which
//!   starts from this harness's own peak: exec keeps the peak of the address space it
//!   replaces, the spawner's under `posix_spawn` (fs/exec.c `exec_mmap`).
//!
//! Firecracker is its pinned release binary, run without the jailer as its getting-started
//! guide runs it: `--no-api --config-file`, default seccomp filters. Each iteration
//! alternates which VMM goes first.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout
)]

#[cfg(target_os = "linux")]
#[path = "../tests/common/mod.rs"]
mod common;
#[cfg(target_os = "linux")]
mod support;

#[cfg(not(target_os = "linux"))]
fn main() {
    use std::io::Write;
    let _ = writeln!(
        std::io::stderr(),
        "SKIP: the Firecracker comparison runs on Linux with KVM"
    );
}

#[cfg(target_os = "linux")]
fn main() {
    compare::main();
}

#[cfg(target_os = "linux")]
mod compare {
    use std::io::{BufRead, BufReader, Read};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{common, support};

    const FIRECRACKER: &str = "v1.17.0";
    const READY: &str = "SHARDS-TEST READY";
    /// The guest's command line. shards' x86_64 machine appends `reboot=k pci=off`, so
    /// Firecracker gets the same there.
    const CMDLINE: &str = "console=ttyS0 quiet panic=-1 shards_test=idle";
    const WARMUP: usize = 3;
    const TIMEOUT: Duration = Duration::from_secs(60);
    const READINGS: usize = 20;
    const READING_PERIOD: Duration = Duration::from_millis(10);
    const MIB: f64 = 1024.0 * 1024.0;

    #[derive(Clone, Copy)]
    enum Vmm {
        Shards,
        Firecracker,
    }

    struct Sample {
        to_ready_us: f64,
        overhead_bytes: u64,
        peak_rss_bytes: u64,
    }

    pub fn main() {
        let runs: usize = support::option("--runs").map_or(30, |v| v.parse().expect("--runs N"));
        let cpus = support::option("--cpus").unwrap_or_else(|| "1".into());
        let memory = support::option("--memory").unwrap_or_else(|| "128".into());
        if common::cannot_run_vms() {
            return;
        }
        let guest_bytes = memory.parse::<u64>().expect("--memory MIB") << 20;
        let firecracker = firecracker();
        let kernel = common::kernel();
        let initrd = idle_initrd();
        let config = firecracker_config(kernel, &initrd, &cpus, &memory);

        let command = |vmm: Vmm| match vmm {
            Vmm::Shards => {
                let mut c = Command::new(common::shards_vm());
                c.args(["run", "--kernel"])
                    .arg(kernel)
                    .arg("--initrd")
                    .arg(&initrd)
                    .args(["--cpus", &cpus, "--memory", &memory, "--cmdline", CMDLINE]);
                c
            }
            Vmm::Firecracker => {
                let mut c = Command::new(&firecracker);
                c.args(["--no-api", "--config-file"])
                    .arg(&config)
                    .args(["--level", "error"]);
                c
            }
        };

        let (mut shards, mut fc) = (Vec::with_capacity(runs), Vec::with_capacity(runs));
        for i in 0..WARMUP + runs {
            let order = if i % 2 == 0 {
                [Vmm::Shards, Vmm::Firecracker]
            } else {
                [Vmm::Firecracker, Vmm::Shards]
            };
            for vmm in order {
                let s = sample(&mut command(vmm), guest_bytes);
                if i >= WARMUP {
                    match vmm {
                        Vmm::Shards => shards.push(s),
                        Vmm::Firecracker => fc.push(s),
                    }
                }
            }
        }

        let to_ready = |v: &[Sample]| v.iter().map(|s| s.to_ready_us).collect::<Vec<_>>();
        let mib =
            |v: &[Sample], f: fn(&Sample) -> u64| v.iter().map(|s| f(s) as f64 / MIB).collect::<Vec<_>>();
        support::report(
            "firecracker",
            &[
                ("n", runs.to_string()),
                ("cpus", cpus.clone()),
                ("memory_mib", memory.clone()),
                ("kernel", common::kernel_artifact().name.to_string()),
                ("firecracker", FIRECRACKER.to_string()),
            ],
            &[
                support::stats("shards_to_ready", "us", to_ready(&shards)),
                support::stats("fc_to_ready", "us", to_ready(&fc)),
                support::stats("shards_overhead", "MiB", mib(&shards, |s| s.overhead_bytes)),
                support::stats("fc_overhead", "MiB", mib(&fc, |s| s.overhead_bytes)),
                support::stats("shards_peak_rss", "MiB", mib(&shards, |s| s.peak_rss_bytes)),
                support::stats("fc_peak_rss", "MiB", mib(&fc, |s| s.peak_rss_bytes)),
            ],
        );
    }

    /// The pinned Firecracker release binary, downloaded, verified and unpacked once.
    fn firecracker() -> PathBuf {
        let (url, sha256) = match common::ARCH {
            "x86_64" => (
                "https://github.com/firecracker-microvm/firecracker/releases/download/v1.17.0/firecracker-v1.17.0-x86_64.tgz",
                "06094a1108ae9e82aa4c23a775aa92758f53f1175d422270d9d6162cb9ade558",
            ),
            "aarch64" => (
                "https://github.com/firecracker-microvm/firecracker/releases/download/v1.17.0/firecracker-v1.17.0-aarch64.tgz",
                "e351ebe4f7a16b5873bbd51005d2e6767103cff4d5ebc829df2d3f95a93e2256",
            ),
            other => panic!("no Firecracker release for {other}"),
        };
        let name = url.rsplit('/').next().unwrap();
        let archive = common::fetch(&common::Artifact { name, url, sha256 });
        let dir = archive.with_extension("");
        let arch = common::ARCH;
        let bin = dir.join(format!(
            "release-{FIRECRACKER}-{arch}/firecracker-{FIRECRACKER}-{arch}"
        ));
        if !bin.exists() {
            std::fs::create_dir_all(&dir).unwrap();
            let ok = Command::new("tar")
                .arg("-xzf")
                .arg(&archive)
                .arg("-C")
                .arg(&dir)
                .status()
                .unwrap();
            assert!(ok.success(), "unpacking {}", archive.display());
        }
        bin
    }

    /// The test guest in `idle` mode, packed as `/init` the way `shards vm run --init` packs
    /// it, so both VMMs load the same bytes.
    fn idle_initrd() -> PathBuf {
        let path = common::workspace().join("target/artifacts/idle.initrd");
        let init = std::fs::read(common::test_guest()).unwrap();
        std::fs::write(&path, shards_vmm::initramfs::with_init(&init)).unwrap();
        path
    }

    fn firecracker_config(kernel: &Path, initrd: &Path, cpus: &str, memory: &str) -> PathBuf {
        let cmdline = if common::ARCH == "x86_64" {
            format!("{CMDLINE} reboot=k pci=off")
        } else {
            CMDLINE.to_string()
        };
        let json = format!(
            "{{\"boot-source\":{{\"kernel_image_path\":{},\"initrd_path\":{},\"boot_args\":{}}},\
             \"drives\":[],\"machine-config\":{{\"vcpu_count\":{cpus},\"mem_size_mib\":{memory}}}}}",
            json_string(&kernel.to_string_lossy()),
            json_string(&initrd.to_string_lossy()),
            json_string(&cmdline),
        );
        let path = common::workspace().join("target/artifacts/firecracker-idle.json");
        std::fs::write(&path, json).unwrap();
        path
    }

    fn json_string(s: &str) -> String {
        let mut out = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    /// One boot: spawn, wait for the guest's ready line, read the VMM's memory while the
    /// guest idles, then kill the VMM and reap it for its peak RSS.
    #[allow(clippy::zombie_processes)] // reaped by wait4, not Child::wait
    fn sample(command: &mut Command, guest_bytes: u64) -> Sample {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let start = Instant::now();
        let mut child = command.spawn().expect("spawning the VMM");
        let pid = child.id() as libc::pid_t;
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let out = thread::spawn(move || {
            let mut all = String::new();
            let mut ready_tx = Some(ready_tx);
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.contains(READY)
                    && let Some(tx) = ready_tx.take()
                {
                    let _ = tx.send(Instant::now());
                }
                all.push_str(&line);
                all.push('\n');
            }
            all
        });
        let err = thread::spawn(move || {
            let mut all = String::new();
            let _ = stderr.read_to_string(&mut all);
            all
        });
        let ready = ready_rx.recv_timeout(TIMEOUT);
        let overhead = ready.is_ok().then(|| {
            (0..READINGS)
                .map(|_| {
                    thread::sleep(READING_PERIOD);
                    overhead_bytes(pid, guest_bytes)
                })
                .max()
                .unwrap_or(0)
        });
        // The guest has idled through the readings: its peak so far is its peak.
        let peak = peak_bytes(pid);
        // SAFETY: the child is not yet reaped, so `pid` still names it.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let mut status = 0;
        // SAFETY: waits for our own child; `status` is a valid out-parameter.
        let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
        assert_eq!(reaped, pid, "waitpid: {}", std::io::Error::last_os_error());
        let (out, err) = (out.join().unwrap(), err.join().unwrap());
        let (Ok(ready), Some(overhead_bytes)) = (ready, overhead) else {
            panic!("the guest never reported ready\n--- stdout\n{out}\n--- stderr\n{err}");
        };
        Sample {
            to_ready_us: ready.duration_since(start).as_secs_f64() * 1e6,
            overhead_bytes,
            peak_rss_bytes: peak.expect("the VMM's VmHWM"),
        }
    }

    /// `pid`'s peak resident set so far: `VmHWM` in /proc/PID/status (proc(5)).
    fn peak_bytes(pid: libc::pid_t) -> Option<u64> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let kib: u64 = status
            .lines()
            .find_map(|l| l.strip_prefix("VmHWM:"))?
            .trim()
            .strip_suffix("kB")?
            .trim()
            .parse()
            .ok()?;
        Some(kib * 1024)
    }

    /// Resident bytes of `pid` outside guest memory, by Firecracker's rule
    /// (tests/host_tools/memory.py, `MemoryMonitor`): sum `Rss` over every mapping except
    /// those sized like guest memory.
    fn overhead_bytes(pid: libc::pid_t, guest_bytes: u64) -> u64 {
        let smaps = std::fs::read_to_string(format!("/proc/{pid}/smaps"))
            .unwrap_or_else(|e| panic!("reading /proc/{pid}/smaps: {e}"));
        let (mut size, mut total) = (0, 0);
        for line in smaps.lines() {
            let mut fields = line.split_whitespace();
            let Some(first) = fields.next() else { continue };
            if let Some((start, end)) = first.split_once('-')
                && let (Ok(start), Ok(end)) = (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
            {
                size = end - start;
            } else if first == "Rss:" && !is_guest_memory(size, guest_bytes) {
                let kib: u64 = fields.next().unwrap().parse().unwrap();
                total += kib * 1024;
            }
        }
        total
    }

    /// Firecracker's `is_guest_mem_x86` and `is_guest_mem_arch64`: a mapping is guest
    /// memory when it is at least as large as guest memory, or has the size of one of the
    /// pieces the architecture's memory gaps split guest memory into.
    fn is_guest_memory(size: u64, guest: u64) -> bool {
        const GIB: u64 = 1 << 30;
        let pieces: &[Option<u64>] = match common::ARCH {
            "x86_64" => &[
                Some(guest),
                Some(3 * GIB),
                guest.checked_sub(3 * GIB),
                Some(256 * GIB - 3 * GIB - GIB),
                (guest + GIB).checked_sub(256 * GIB),
            ],
            _ => &[
                Some(guest),
                Some(256 * GIB - 2 * GIB),
                (guest + 2 * GIB).checked_sub(256 * GIB),
            ],
        };
        size >= guest || pieces.contains(&Some(size))
    }
}
