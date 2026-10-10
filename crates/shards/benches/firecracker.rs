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
//! - `rss_anon`, `rss_file`: the VMM's anonymous and file-backed resident memory at the
//!   same moment, `RssAnon` and `RssFile` in /proc/PID/status.
//! - `pss`, `pss_file`: its proportional set size then, all and file-backed, `Pss` and
//!   `Pss_File` in /proc/PID/smaps_rollup: shared pages charged in proportion to their
//!   sharers, what a fleet pays per VM. A restored guest's memory is
//!   a private mapping of its snapshot file: the pages it only reads are the file's, in the
//!   page cache every VM restored from that snapshot shares, and a page it writes becomes
//!   an anonymous copy of its own.
//!
//! Firecracker is its pinned release binary, run without the jailer as its getting-started
//! guide runs it: `--no-api --config-file`, default seccomp filters. Each iteration
//! alternates which VMM goes first.
//!
//! Then restores. Each VMM snapshots the test guest in `beat` mode, which waits for the
//! kernel's crypto self-tests to end, as shards-init does before a template (PM M38),
//! then prints a `.` every millisecond: shards when the guest asks (`shards_snapshot=N`,
//! as init asks for a template's), Firecracker through its API (`PATCH /vm` Paused,
//! `PUT /snapshot/create`, Full). Every sample is a fresh VMM process restoring it, as each does it: `shards-vm
//! restore DIR`, and `firecracker --api-sock` then `PUT /snapshot/load` with its memory
//! file mapped (`File`) and `resume_vm`.
//!
//! - `to_beat`: the host's clock from spawn until the guest's first beat after the restore
//!   reaches the VMM's stdout: the guest running again, what a user waits for.
//! - `overhead` and `peak_rss`, as above, while the guest beats.
//!
//! Then density (audit D14): `--fleet K` (16) restored VMs of one VMM held at once, all
//! beating, then the other's, in rounds S F F S S F F S. Each VM's row pairs with the other
//! VMM's VM of the same index in the matching round.
//!
//! - `fleet_pss`, `fleet_private`: each VM's proportional set size, and its private pages
//!   (`Private_Clean` and `Private_Dirty` in smaps_rollup), with its fleet running.
//! - `fleet_pte`: its page tables, `VmPTE` in /proc/PID/status.
//! - `fleet_host`: the host's `MemAvailable` given up per VM, from before the round's
//!   first spawn to all of it beating: what process accounting leaves out (KVM's and the
//!   kernel's own memory) with what it counts, and none of the page cache, which stays
//!   reclaimable.

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
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::UnixStream;
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
    /// The restore comparison's guest: the test guest beating.
    const BEAT_CMDLINE: &str = "console=ttyS0 quiet panic=-1 shards_test=beat";
    /// The beats shards' guest runs before it asks for its snapshot.
    const SNAPSHOT_AFTER: u32 = 20;

    #[derive(Clone, Copy)]
    enum Vmm {
        Shards,
        Firecracker,
    }

    struct Sample {
        to_ready_us: f64,
        overhead_bytes: u64,
        peak_rss_bytes: u64,
        rss_anon_bytes: u64,
        rss_file_bytes: u64,
        pss_bytes: u64,
        pss_file_bytes: u64,
        anon_huge_bytes: u64,
    }

    /// The walking guest's command line (testguest `walk`, 512 MiB).
    const WALK_CMDLINE: &str = "console=ttyS0 quiet panic=-1 shards_test=walk shards_walk_mib=512";

    /// THP's A/B (`--thp-ab all,never`, PM M157): shards alone, its guest memory on huge
    /// pages as shards advises it (MADV_HUGEPAGE, `all`), or on none (`never`: the VM
    /// process's THP disabled), interleaved, each order alternating. Boot: the idle guest's
    /// `to_ready` and the VMM's memory at ready, as above, with `AnonHugePages`. Work: a
    /// 1 GiB guest that writes a byte to each page of 512 MiB and reads it at random
    /// (testguest `walk`).
    fn thp_ab(arms: &str, runs: usize) {
        let arms: Vec<String> = arms.split(',').map(str::to_string).collect();
        assert_eq!(arms.len(), 2, "--thp-ab A,B");
        let kernel = common::kernel();
        let initrd = idle_initrd();
        for arm in &arms {
            assert!(
                arm == "all" || arm == "never",
                "--thp-ab: all or never, not {arm}"
            );
        }
        let command = |arm: &str, memory: &str, cmdline: &str| {
            let mut c = Command::new(common::shards_vm());
            c.args(["run", "--kernel"])
                .arg(kernel)
                .arg("--initrd")
                .arg(&initrd)
                .args(["--cpus", "1", "--memory", memory, "--cmdline", cmdline]);
            if arm == "never" {
                // No huge page for any of the VM process's memory, its MADV_HUGEPAGE guest
                // memory too: PR_SET_THP_DISABLE holds across exec (prctl(2)).
                // SAFETY: prctl(2) of the child itself, between fork and exec.
                unsafe {
                    std::os::unix::process::CommandExt::pre_exec(&mut c, || {
                        if libc::prctl(libc::PR_SET_THP_DISABLE, 1, 0, 0, 0) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
            c
        };
        let mut boots: Vec<Vec<Sample>> = vec![Vec::new(), Vec::new()];
        let mut walks: Vec<Vec<[f64; 4]>> = vec![Vec::new(), Vec::new()];
        for i in 0..WARMUP + runs {
            let order = if i % 2 == 0 { [0usize, 1] } else { [1, 0] };
            for a in order {
                let boot = sample(&mut command(&arms[a], "128", CMDLINE), 128 << 20);
                let walk = walk_sample(&mut command(&arms[a], "1024", WALK_CMDLINE));
                if i >= WARMUP {
                    boots[a].push(boot);
                    walks[a].push(walk);
                }
            }
        }
        let label = |a: usize| arms[a].replace('=', "");
        let mut rows = Vec::new();
        for (a, b) in boots.iter().enumerate() {
            let l = label(a);
            let mib = |f: fn(&Sample) -> u64| b.iter().map(|s| f(s) as f64 / MIB).collect::<Vec<_>>();
            rows.push(support::stats(
                &format!("{l}_to_ready"),
                "us",
                b.iter().map(|s| s.to_ready_us).collect(),
            ));
            rows.push(support::stats(
                &format!("{l}_rss_anon"),
                "MiB",
                mib(|s| s.rss_anon_bytes),
            ));
            rows.push(support::stats(
                &format!("{l}_peak_rss"),
                "MiB",
                mib(|s| s.peak_rss_bytes),
            ));
            rows.push(support::stats(
                &format!("{l}_anon_huge"),
                "MiB",
                mib(|s| s.anon_huge_bytes),
            ));
        }
        support::report(
            "thp_boot",
            &[("n", runs.to_string()), ("arms", arms.join(","))],
            &rows,
        );
        let mut rows = Vec::new();
        for (a, w) in walks.iter().enumerate() {
            let l = label(a);
            let col = |i: usize| w.iter().map(|v| v[i]).collect::<Vec<_>>();
            rows.push(support::stats(&format!("{l}_touch"), "ms", col(0)));
            rows.push(support::stats(&format!("{l}_read"), "ms", col(1)));
            rows.push(support::stats(&format!("{l}_rss_anon"), "MiB", col(2)));
            rows.push(support::stats(&format!("{l}_anon_huge"), "MiB", col(3)));
        }
        support::report(
            "thp_walk",
            &[("n", runs.to_string()), ("arms", arms.join(","))],
            &rows,
        );
    }

    /// One walking guest: its touch and read times in ms, and the VMM's RssAnon and
    /// AnonHugePages in MiB once it has walked.
    #[allow(clippy::zombie_processes)] // reaped by waitpid, not Child::wait
    fn walk_sample(command: &mut Command) -> [f64; 4] {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("spawning the VMM");
        let pid = child.id() as libc::pid_t;
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        let out = thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(rest) = line.split_once("SHARDS-TEST WALK ").map(|(_, r)| r.to_string()) {
                    let _ = tx.send(rest);
                }
            }
        });
        let walked = rx
            .recv_timeout(Duration::from_secs(300))
            .expect("the guest's walk");
        let [anon, huge] = ["RssAnon", "AnonHugePages"].map(|f| {
            if f == "AnonHugePages" {
                rollup_bytes(pid, f)
            } else {
                status_bytes(pid, f)
            }
        });
        // SAFETY: the child is not yet reaped, so `pid` still names it.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let mut status = 0;
        // SAFETY: waits for our own child; `status` is a valid out-parameter.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        let _ = out.join();
        let ns: Vec<f64> = walked.split_whitespace().filter_map(|v| v.parse().ok()).collect();
        [
            ns.first().copied().unwrap_or(0.0) / 1e6,
            ns.get(1).copied().unwrap_or(0.0) / 1e6,
            anon.unwrap_or(0) as f64 / MIB,
            huge.unwrap_or(0) as f64 / MIB,
        ]
    }

    pub fn main() {
        let runs: usize = support::option("--runs").map_or(30, |v| v.parse().expect("--runs N"));
        let cpus = support::option("--cpus").unwrap_or_else(|| "1".into());
        let memory = support::option("--memory").unwrap_or_else(|| "128".into());
        if common::cannot_run_vms() {
            return;
        }
        if let Some(arms) = support::option("--thp-ab") {
            return thp_ab(&arms, runs);
        }
        let guest_bytes = memory.parse::<u64>().expect("--memory MIB") << 20;
        let firecracker = firecracker();
        let kernel = common::kernel();
        let initrd = idle_initrd();
        let config = firecracker_config(kernel, &initrd, &cpus, &memory, CMDLINE, "firecracker-idle.json");

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
                support::stats("shards_rss_anon", "MiB", mib(&shards, |s| s.rss_anon_bytes)),
                support::stats("fc_rss_anon", "MiB", mib(&fc, |s| s.rss_anon_bytes)),
                support::stats("shards_rss_file", "MiB", mib(&shards, |s| s.rss_file_bytes)),
                support::stats("fc_rss_file", "MiB", mib(&fc, |s| s.rss_file_bytes)),
                support::stats("shards_pss", "MiB", mib(&shards, |s| s.pss_bytes)),
                support::stats("fc_pss", "MiB", mib(&fc, |s| s.pss_bytes)),
                support::stats("shards_pss_file", "MiB", mib(&shards, |s| s.pss_file_bytes)),
                support::stats("fc_pss_file", "MiB", mib(&fc, |s| s.pss_file_bytes)),
            ],
        );
        let fleet: usize = support::option("--fleet").map_or(16, |v| v.parse().expect("--fleet K"));
        restores(&firecracker, kernel, &initrd, &cpus, &memory, runs, fleet);
    }

    /// The restore comparison (above): each VMM's snapshot of the beating guest, restored
    /// by fresh processes, interleaved.
    fn restores(
        firecracker: &Path,
        kernel: &Path,
        initrd: &Path,
        cpus: &str,
        memory: &str,
        runs: usize,
        fleet: usize,
    ) {
        let guest_bytes = memory.parse::<u64>().expect("--memory MIB") << 20;
        let dir = common::workspace().join(format!("target/bench/fc-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // shards: the guest asks for its snapshot, and the VM stops once it is written.
        let snapshot = dir.join("shards");
        let saved = Command::new(common::shards_vm())
            .args(["run", "--kernel"])
            .arg(kernel)
            .arg("--initrd")
            .arg(initrd)
            .args(["--cpus", cpus, "--memory", memory, "--no-console", "--cmdline"])
            .arg(format!("{BEAT_CMDLINE} shards_snapshot={SNAPSHOT_AFTER}"))
            .arg("--snapshot-dir")
            .arg(&snapshot)
            .output()
            .unwrap();
        assert!(
            saved.status.success() && shards_vmm::snapshot::exists(&snapshot),
            "shards' snapshot: {}",
            String::from_utf8_lossy(&saved.stderr)
        );

        // Firecracker: booted from its config, paused and snapshotted through its API.
        let (fc_state, fc_memory) = (dir.join("fc.state"), dir.join("fc.memory"));
        let config = firecracker_config(
            kernel,
            initrd,
            cpus,
            memory,
            BEAT_CMDLINE,
            "firecracker-beat.json",
        );
        let socket = dir.join("fc-save.sock");
        let mut booted = Command::new(firecracker)
            .arg("--api-sock")
            .arg(&socket)
            .arg("--config-file")
            .arg(&config)
            .args(["--level", "error"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let (beating, _) = first_beat(booted.stdout.take().unwrap());
        beating
            .recv_timeout(TIMEOUT)
            .expect("Firecracker's guest never beat");
        api(&socket, "PATCH", "/vm", r#"{"state":"Paused"}"#);
        api(
            &socket,
            "PUT",
            "/snapshot/create",
            &format!(
                "{{\"snapshot_type\":\"Full\",\"snapshot_path\":{},\"mem_file_path\":{}}}",
                json_string(&fc_state.to_string_lossy()),
                json_string(&fc_memory.to_string_lossy())
            ),
        );
        booted.kill().unwrap();
        booted.wait().unwrap();

        let load = format!(
            "{{\"snapshot_path\":{},\"mem_backend\":{{\"backend_type\":\"File\",\"backend_path\":{}}},\"resume_vm\":true}}",
            json_string(&fc_state.to_string_lossy()),
            json_string(&fc_memory.to_string_lossy())
        );
        let (mut shards, mut fc) = (Vec::with_capacity(runs), Vec::with_capacity(runs));
        for i in 0..WARMUP + runs {
            let order = if i % 2 == 0 {
                [Vmm::Shards, Vmm::Firecracker]
            } else {
                [Vmm::Firecracker, Vmm::Shards]
            };
            for vmm in order {
                let s = match vmm {
                    Vmm::Shards => {
                        let mut c = Command::new(common::shards_vm());
                        c.arg("restore").arg(&snapshot);
                        restore_sample(&mut c, guest_bytes, || {})
                    }
                    Vmm::Firecracker => {
                        let socket = dir.join(format!("fc-{i}.sock"));
                        let mut c = Command::new(firecracker);
                        c.arg("--api-sock").arg(&socket).args(["--level", "error"]);
                        restore_sample(&mut c, guest_bytes, || {
                            api(&socket, "PUT", "/snapshot/load", &load);
                        })
                    }
                };
                if i >= WARMUP {
                    match vmm {
                        Vmm::Shards => shards.push(s),
                        Vmm::Firecracker => fc.push(s),
                    }
                }
            }
        }
        density(firecracker, &snapshot, &dir, &load, fleet, cpus, memory);
        let _ = std::fs::remove_dir_all(&dir);
        let to_beat = |v: &[Sample]| v.iter().map(|s| s.to_ready_us).collect::<Vec<_>>();
        let mib =
            |v: &[Sample], f: fn(&Sample) -> u64| v.iter().map(|s| f(s) as f64 / MIB).collect::<Vec<_>>();
        support::report(
            "firecracker_restore",
            &[
                ("n", runs.to_string()),
                ("cpus", cpus.to_string()),
                ("memory_mib", memory.to_string()),
                ("kernel", common::kernel_artifact().name.to_string()),
                ("firecracker", FIRECRACKER.to_string()),
            ],
            &[
                support::stats("shards_to_beat", "us", to_beat(&shards)),
                support::stats("fc_to_beat", "us", to_beat(&fc)),
                support::stats("shards_overhead", "MiB", mib(&shards, |s| s.overhead_bytes)),
                support::stats("fc_overhead", "MiB", mib(&fc, |s| s.overhead_bytes)),
                support::stats("shards_peak_rss", "MiB", mib(&shards, |s| s.peak_rss_bytes)),
                support::stats("fc_peak_rss", "MiB", mib(&fc, |s| s.peak_rss_bytes)),
                support::stats("shards_rss_anon", "MiB", mib(&shards, |s| s.rss_anon_bytes)),
                support::stats("fc_rss_anon", "MiB", mib(&fc, |s| s.rss_anon_bytes)),
                support::stats("shards_rss_file", "MiB", mib(&shards, |s| s.rss_file_bytes)),
                support::stats("fc_rss_file", "MiB", mib(&fc, |s| s.rss_file_bytes)),
                support::stats("shards_pss", "MiB", mib(&shards, |s| s.pss_bytes)),
                support::stats("fc_pss", "MiB", mib(&fc, |s| s.pss_bytes)),
                support::stats("shards_pss_file", "MiB", mib(&shards, |s| s.pss_file_bytes)),
                support::stats("fc_pss_file", "MiB", mib(&fc, |s| s.pss_file_bytes)),
            ],
        );
    }

    /// A restored VM held running: its process, and the threads that drain its output.
    struct Held {
        child: std::process::Child,
        pid: libc::pid_t,
        out: thread::JoinHandle<Vec<u8>>,
        err: thread::JoinHandle<String>,
    }

    impl Held {
        /// `command` spawned, `after_spawn` done (Firecracker's load), and its guest beating.
        fn start(command: &mut Command, after_spawn: impl FnOnce()) -> Held {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().expect("spawning the VMM");
            let pid = child.id() as libc::pid_t;
            let (beat, out) = first_beat(child.stdout.take().unwrap());
            let mut stderr = child.stderr.take().unwrap();
            let err = thread::spawn(move || {
                let mut all = String::new();
                let _ = stderr.read_to_string(&mut all);
                all
            });
            after_spawn();
            let held = Held { child, pid, out, err };
            if beat.recv_timeout(TIMEOUT).is_err() {
                let (out, err) = held.end();
                panic!(
                    "a held restore never beat\n--- stdout\n{}\n--- stderr\n{err}",
                    String::from_utf8_lossy(&out)
                );
            }
            held
        }

        fn end(mut self) -> (Vec<u8>, String) {
            // SAFETY: the child is not yet reaped, so `pid` still names it.
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
            self.child.wait().unwrap();
            (self.out.join().unwrap(), self.err.join().unwrap())
        }
    }

    /// /proc/meminfo's `field`, in bytes.
    fn meminfo_bytes(field: &str) -> u64 {
        let info = std::fs::read_to_string("/proc/meminfo").unwrap();
        info.lines()
            .find_map(|l| l.strip_prefix(field)?.strip_prefix(':'))
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .expect("a /proc/meminfo field")
            * 1024
    }

    /// The density rounds (above): `fleet` restores of each VMM's snapshot held at once.
    fn density(
        firecracker: &Path,
        snapshot: &Path,
        dir: &Path,
        load: &str,
        fleet: usize,
        cpus: &str,
        memory: &str,
    ) {
        let rounds = [
            Vmm::Shards,
            Vmm::Firecracker,
            Vmm::Firecracker,
            Vmm::Shards,
            Vmm::Shards,
            Vmm::Firecracker,
            Vmm::Firecracker,
            Vmm::Shards,
        ];
        // Per VMM: each VM's PSS, private pages and page tables, and each round's host share.
        let mut rows: [[Vec<f64>; 4]; 2] = Default::default();
        for (round, vmm) in rounds.into_iter().enumerate() {
            let before = meminfo_bytes("MemAvailable");
            let held: Vec<Held> = (0..fleet)
                .map(|k| match vmm {
                    Vmm::Shards => {
                        let mut c = Command::new(common::shards_vm());
                        c.arg("restore").arg(snapshot);
                        Held::start(&mut c, || {})
                    }
                    Vmm::Firecracker => {
                        let socket = dir.join(format!("fleet-{round}-{k}.sock"));
                        let mut c = Command::new(firecracker);
                        c.arg("--api-sock").arg(&socket).args(["--level", "error"]);
                        Held::start(&mut c, || api(&socket, "PUT", "/snapshot/load", load))
                    }
                })
                .collect();
            let after = meminfo_bytes("MemAvailable");
            let row = &mut rows[matches!(vmm, Vmm::Firecracker) as usize];
            for h in &held {
                let private = ["Private_Clean", "Private_Dirty"]
                    .map(|f| rollup_bytes(h.pid, f).expect("smaps_rollup"))
                    .iter()
                    .sum::<u64>();
                row[0].push(rollup_bytes(h.pid, "Pss").expect("Pss") as f64 / MIB);
                row[1].push(private as f64 / MIB);
                row[2].push(status_bytes(h.pid, "VmPTE").expect("VmPTE") as f64 / MIB);
            }
            row[3].push(before.saturating_sub(after) as f64 / fleet as f64 / MIB);
            for h in held {
                h.end();
            }
        }
        let [shards, fc] = rows;
        let [s_pss, s_private, s_pte, s_host] = shards;
        let [f_pss, f_private, f_pte, f_host] = fc;
        support::report(
            "firecracker_density",
            &[
                ("fleet", fleet.to_string()),
                ("rounds", "S F F S S F F S".to_string()),
                ("cpus", cpus.to_string()),
                ("memory_mib", memory.to_string()),
                ("kernel", common::kernel_artifact().name.to_string()),
                ("firecracker", FIRECRACKER.to_string()),
            ],
            &[
                support::stats("shards_fleet_pss", "MiB", s_pss),
                support::stats("fc_fleet_pss", "MiB", f_pss),
                support::stats("shards_fleet_private", "MiB", s_private),
                support::stats("fc_fleet_private", "MiB", f_private),
                support::stats("shards_fleet_pte", "MiB", s_pte),
                support::stats("fc_fleet_pte", "MiB", f_pte),
                support::stats("shards_fleet_host", "MiB", s_host),
                support::stats("fc_fleet_host", "MiB", f_host),
            ],
        );
    }

    /// Watches `stdout` for the guest's first beat, two `.` in a row (a log line may hold
    /// one): the channel gets the time the first of them arrived, and the thread returns
    /// everything read, for errors.
    fn first_beat(
        mut stdout: impl Read + Send + 'static,
    ) -> (mpsc::Receiver<Instant>, thread::JoinHandle<Vec<u8>>) {
        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            let (mut all, mut buf) = (Vec::new(), [0u8; 4096]);
            let (mut tx, mut last_dot) = (Some(tx), None::<Instant>);
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let now = Instant::now();
                for &b in &buf[..n] {
                    if b == b'.' {
                        if let (Some(at), Some(sender)) = (last_dot, tx.as_ref()) {
                            let _ = sender.send(at);
                            tx = None;
                        }
                        last_dot = last_dot.or(Some(now));
                    } else {
                        last_dot = None;
                    }
                }
                all.extend_from_slice(&buf[..n]);
            }
            all
        });
        (rx, reader)
    }

    /// One restore: `command` spawned, then `after_spawn` (Firecracker's load), then the
    /// time to the guest's first beat, the overhead readings, and the peak.
    fn restore_sample(command: &mut Command, guest_bytes: u64, after_spawn: impl FnOnce()) -> Sample {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let start = Instant::now();
        let mut child = command.spawn().expect("spawning the VMM");
        let pid = child.id() as libc::pid_t;
        let (beat, out) = first_beat(child.stdout.take().unwrap());
        let mut stderr = child.stderr.take().unwrap();
        let err = thread::spawn(move || {
            let mut all = String::new();
            let _ = stderr.read_to_string(&mut all);
            all
        });
        after_spawn();
        let beat = beat.recv_timeout(TIMEOUT);
        let overhead = beat.is_ok().then(|| {
            (0..READINGS)
                .map(|_| {
                    thread::sleep(READING_PERIOD);
                    overhead_bytes(pid, guest_bytes)
                })
                .max()
                .unwrap_or(0)
        });
        let [peak, anon, file] = ["VmHWM", "RssAnon", "RssFile"].map(|field| status_bytes(pid, field));
        let [pss, pss_file, anon_huge] =
            ["Pss", "Pss_File", "AnonHugePages"].map(|field| rollup_bytes(pid, field));
        describe_mappings(
            pid,
            &format!("{} restore", command.get_program().to_string_lossy()),
        );
        // SAFETY: the child is not yet reaped, so `pid` still names it.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        child.wait().unwrap();
        let (out, err) = (out.join().unwrap(), err.join().unwrap());
        let (Ok(beat), Some(overhead_bytes)) = (beat, overhead) else {
            panic!(
                "the restored guest never beat\n--- stdout\n{}\n--- stderr\n{err}",
                String::from_utf8_lossy(&out)
            );
        };
        Sample {
            to_ready_us: beat.duration_since(start).as_secs_f64() * 1e6,
            overhead_bytes,
            peak_rss_bytes: peak.expect("the VMM's VmHWM"),
            rss_anon_bytes: anon.expect("the VMM's RssAnon"),
            rss_file_bytes: file.expect("the VMM's RssFile"),
            pss_bytes: pss.expect("the VMM's Pss"),
            pss_file_bytes: pss_file.expect("the VMM's Pss_File"),
            anon_huge_bytes: anon_huge.unwrap_or(0),
        }
    }

    /// Firecracker's API over its socket: `method path` with a JSON `body`, which must
    /// succeed (204). A load's socket appears only once Firecracker has started, so the
    /// connection is retried until then.
    fn api(socket: &Path, method: &str, path: &str, body: &str) {
        let deadline = Instant::now() + TIMEOUT;
        let mut conn = loop {
            match UnixStream::connect(socket) {
                Ok(c) => break c,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_micros(100)),
                Err(e) => panic!("{}: {e}", socket.display()),
            }
        };
        write!(
            conn,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = Vec::new();
        let mut buf = [0u8; 4096];
        while !response.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = conn.read(&mut buf).unwrap();
            assert!(n > 0, "Firecracker closed its API connection");
            response.extend_from_slice(&buf[..n]);
        }
        let head = String::from_utf8_lossy(&response);
        assert!(head.starts_with("HTTP/1.1 204"), "{method} {path}: {head}");
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

    /// Firecracker's configuration for the guest with `cmdline`, written to `name`.
    fn firecracker_config(
        kernel: &Path,
        initrd: &Path,
        cpus: &str,
        memory: &str,
        cmdline: &str,
        name: &str,
    ) -> PathBuf {
        // shards' x86_64 machine appends these (vm/x86_64.rs, MACHINE_CMDLINE).
        let cmdline = if common::ARCH == "x86_64" {
            format!("{cmdline} reboot=k pci=off")
        } else {
            cmdline.to_string()
        };
        let json = format!(
            "{{\"boot-source\":{{\"kernel_image_path\":{},\"initrd_path\":{},\"boot_args\":{}}},\
             \"drives\":[],\"machine-config\":{{\"vcpu_count\":{cpus},\"mem_size_mib\":{memory}}}}}",
            json_string(&kernel.to_string_lossy()),
            json_string(&initrd.to_string_lossy()),
            json_string(&cmdline),
        );
        let path = common::workspace().join("target/artifacts").join(name);
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
        let [peak, anon, file] = ["VmHWM", "RssAnon", "RssFile"].map(|field| status_bytes(pid, field));
        let [pss, pss_file, anon_huge] =
            ["Pss", "Pss_File", "AnonHugePages"].map(|field| rollup_bytes(pid, field));
        describe_mappings(pid, &format!("{} boot", command.get_program().to_string_lossy()));
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
            rss_anon_bytes: anon.expect("the VMM's RssAnon"),
            rss_file_bytes: file.expect("the VMM's RssFile"),
            pss_bytes: pss.expect("the VMM's Pss"),
            pss_file_bytes: pss_file.expect("the VMM's Pss_File"),
            anon_huge_bytes: anon_huge.unwrap_or(0),
        }
    }

    /// A size in `pid`'s /proc/PID/status (proc(5)): `VmHWM`, its peak resident set so
    /// far, or `RssAnon` and `RssFile`, its anonymous and file-backed resident memory now.
    /// A field of /proc/PID/smaps_rollup, in bytes: `Pss` charges each shared page to the
    /// processes that map it in proportion (proc(5)), so pages a VM shares through the page
    /// cache with others restored from the same snapshot cost it its share of them, not all.
    fn rollup_bytes(pid: libc::pid_t, field: &str) -> Option<u64> {
        let rollup = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
        let kib: u64 = rollup
            .lines()
            .find_map(|l| l.strip_prefix(field)?.strip_prefix(':'))?
            .trim()
            .strip_suffix("kB")?
            .trim()
            .parse()
            .ok()?;
        Some(kib * 1024)
    }

    fn status_bytes(pid: libc::pid_t, field: &str) -> Option<u64> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let kib: u64 = status
            .lines()
            .find_map(|l| l.strip_prefix(field)?.strip_prefix(':'))?
            .trim()
            .strip_suffix("kB")?
            .trim()
            .parse()
            .ok()?;
        Some(kib * 1024)
    }

    /// Once per VMM and phase: each mapping's resident bytes at the reading, the largest
    /// first, by what backs it, on stderr. What the totals are made of.
    fn describe_mappings(pid: libc::pid_t, vmm: &str) {
        static SHOWN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let key = vmm.to_string();
        {
            let mut shown = SHOWN.lock().unwrap();
            if shown.contains(&key) {
                return;
            }
            shown.push(key);
        }
        let smaps = std::fs::read_to_string(format!("/proc/{pid}/smaps")).unwrap_or_default();
        let mut rows: Vec<(String, u64, u64, u64)> = Vec::new();
        let (mut name, mut size) = (String::new(), 0);
        for line in smaps.lines() {
            let mut fields = line.split_whitespace();
            let Some(first) = fields.next() else { continue };
            if let Some((start, end)) = first.split_once('-')
                && let (Ok(start), Ok(end)) = (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
            {
                size = end - start;
                name = line
                    .split_whitespace()
                    .nth(5)
                    .unwrap_or("[anonymous]")
                    .to_string();
            } else if first == "Rss:" {
                let kib: u64 = fields.next().unwrap().parse().unwrap();
                rows.push((name.clone(), size, kib * 1024, 0));
            } else if first == "Anonymous:" {
                let kib: u64 = fields.next().unwrap().parse().unwrap();
                if let Some(last) = rows.last_mut() {
                    last.3 = kib * 1024;
                }
            }
        }
        rows.sort_by_key(|r| std::cmp::Reverse(r.2));
        let mut out = format!("{vmm} (pid {pid}) mappings by resident bytes:\n");
        for (name, size, rss, anon) in rows.iter().take(12) {
            out.push_str(&format!(
                "  rss {:>9} anon {:>9} size {:>11}  {name}\n",
                rss, anon, size
            ));
        }
        let _ = std::io::Write::write_all(&mut std::io::stderr(), out.as_bytes());
    }

    /// Resident bytes of `pid` outside guest memory, by Firecracker's rule
    /// (tests/host_tools/memory.py, `MemoryMonitor`): sum `Rss` over every mapping except
    /// those of guest memory ([`guest_mappings`]).
    fn overhead_bytes(pid: libc::pid_t, guest_bytes: u64) -> u64 {
        let smaps = std::fs::read_to_string(format!("/proc/{pid}/smaps"))
            .unwrap_or_else(|e| panic!("reading /proc/{pid}/smaps: {e}"));
        // Each mapping's start, end and resident bytes, in address order.
        let mut maps: Vec<(u64, u64, u64)> = Vec::new();
        for line in smaps.lines() {
            let mut fields = line.split_whitespace();
            let Some(first) = fields.next() else { continue };
            if let Some((start, end)) = first.split_once('-')
                && let (Ok(start), Ok(end)) = (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
            {
                maps.push((start, end, 0));
            } else if first == "Rss:"
                && let Some(last) = maps.last_mut()
            {
                let kib: u64 = fields.next().unwrap().parse().unwrap();
                last.2 = kib * 1024;
            }
        }
        let spans: Vec<(u64, u64)> = maps.iter().map(|&(s, e, _)| (s, e)).collect();
        guest_mappings(&spans, guest_bytes)
            .iter()
            .zip(&maps)
            .filter(|(guest, _)| !**guest)
            .map(|(_, &(_, _, rss))| rss)
            .sum()
    }

    /// Which of `maps`, each a (start, end) in address order, are guest memory: one alone
    /// by Firecracker's rule ([`is_guest_memory`]), or contiguous ones whose sizes add up
    /// to exactly a piece of it, as advice on part of guest memory splits its mapping
    /// (shards' first 2 MiB on small pages, PM M157).
    fn guest_mappings(maps: &[(u64, u64)], guest: u64) -> Vec<bool> {
        let mut found = vec![false; maps.len()];
        for a in 0..maps.len() {
            let mut size = 0;
            for b in a..maps.len() {
                if b > a && maps[b].0 != maps[b - 1].1 {
                    break;
                }
                size += maps[b].1 - maps[b].0;
                let fits = if b == a {
                    is_guest_memory(size, guest)
                } else {
                    guest_pieces(guest).contains(&size)
                };
                if fits {
                    found[a..=b].iter_mut().for_each(|f| *f = true);
                    break;
                }
            }
        }
        found
    }

    /// Firecracker's `is_guest_mem_x86` and `is_guest_mem_arch64`: a mapping is guest
    /// memory when it is at least as large as guest memory, or has the size of one of the
    /// pieces the architecture's memory gaps split guest memory into.
    fn is_guest_memory(size: u64, guest: u64) -> bool {
        size >= guest || guest_pieces(guest).contains(&size)
    }

    /// The sizes guest memory comes in: whole, or each piece the architecture's memory gaps
    /// split it into.
    fn guest_pieces(guest: u64) -> Vec<u64> {
        const GIB: u64 = 1 << 30;
        let pieces = match common::ARCH {
            "x86_64" => vec![
                Some(guest),
                Some(3 * GIB),
                guest.checked_sub(3 * GIB),
                Some(256 * GIB - 3 * GIB - GIB),
                (guest + GIB).checked_sub(256 * GIB),
            ],
            _ => vec![
                Some(guest),
                Some(256 * GIB - 2 * GIB),
                (guest + 2 * GIB).checked_sub(256 * GIB),
            ],
        };
        pieces.into_iter().flatten().collect()
    }
}
