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
            ],
        );
        restores(&firecracker, kernel, &initrd, &cpus, &memory, runs);
    }

    /// The restore comparison (above): each VMM's snapshot of the beating guest, restored
    /// by fresh processes, interleaved.
    fn restores(firecracker: &Path, kernel: &Path, initrd: &Path, cpus: &str, memory: &str, runs: usize) {
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
            .args(["--cpus", cpus, "--memory", memory, "--cmdline"])
            .arg(format!("{BEAT_CMDLINE} shards_snapshot={SNAPSHOT_AFTER}"))
            .arg("--snapshot-dir")
            .arg(&snapshot)
            .output()
            .unwrap();
        assert!(
            saved.status.success() && snapshot.join("state").exists(),
            "shards' snapshot: {}",
            String::from_utf8_lossy(&saved.stderr)
        );
        // Diagnostic (branch restore-diag): the same snapshot with its memory written one
        // page per pwrite, as shards wrote it before e9ed44d, restored beside it.
        let perpage = dir.join("shards-perpage");
        std::fs::create_dir_all(&perpage).unwrap();
        for entry in std::fs::read_dir(&snapshot).unwrap() {
            let entry = entry.unwrap();
            let to = perpage.join(entry.file_name());
            if entry.file_name() == "memory" {
                use std::os::unix::fs::FileExt;
                let bytes = std::fs::read(entry.path()).unwrap();
                let f = std::fs::File::create(&to).unwrap();
                for (i, page) in bytes.chunks(4096).enumerate() {
                    if page.iter().any(|&b| b != 0) {
                        f.write_all_at(page, (i * 4096) as u64).unwrap();
                    }
                }
                f.set_len(bytes.len() as u64).unwrap();
            } else {
                std::fs::copy(entry.path(), &to).unwrap();
            }
        }
        // Diagnostic (branch restore-diag): the guest's mitigations under each VMM.
        for line in String::from_utf8_lossy(&saved.stdout)
            .lines()
            .filter(|l| l.contains("vuln "))
        {
            println!("shards-guest {line}");
        }

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
        let (beating, fc_out) = first_beat(booted.stdout.take().unwrap());
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
        for line in String::from_utf8_lossy(&fc_out.join().unwrap())
            .lines()
            .filter(|l| l.contains("vuln "))
        {
            println!("fc-guest {line}");
        }

        let load = format!(
            "{{\"snapshot_path\":{},\"mem_backend\":{{\"backend_type\":\"File\",\"backend_path\":{}}},\"resume_vm\":true}}",
            json_string(&fc_state.to_string_lossy()),
            json_string(&fc_memory.to_string_lossy())
        );
        let (mut shards, mut fc) = (Vec::with_capacity(runs), Vec::with_capacity(runs));
        let mut per_page = Vec::with_capacity(runs);
        for i in 0..WARMUP + runs {
            // Diagnostic (branch restore-diag): the per-page copy, first or last in turn.
            let mut sample_per_page = || {
                let mut c = Command::new(common::shards_vm());
                c.arg("restore").arg(&perpage).env("SHARDS_LOG", "debug");
                println!("per-page restore:");
                let s = restore_sample(&mut c, guest_bytes, || {});
                if i >= WARMUP {
                    per_page.push(s);
                }
            };
            if i % 4 < 2 {
                sample_per_page();
            }
            let order = if i % 2 == 0 {
                [Vmm::Shards, Vmm::Firecracker]
            } else {
                [Vmm::Firecracker, Vmm::Shards]
            };
            for vmm in order {
                let s = match vmm {
                    Vmm::Shards => {
                        let mut c = Command::new(common::shards_vm());
                        c.arg("restore").arg(&snapshot).env("SHARDS_LOG", "debug");
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
            if i % 4 >= 2 {
                sample_per_page();
            }
        }
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
                support::stats("perpage_to_beat", "us", to_beat(&per_page)),
                support::stats("fc_to_beat", "us", to_beat(&fc)),
                support::stats("shards_overhead", "MiB", mib(&shards, |s| s.overhead_bytes)),
                support::stats("fc_overhead", "MiB", mib(&fc, |s| s.overhead_bytes)),
                support::stats("shards_peak_rss", "MiB", mib(&shards, |s| s.peak_rss_bytes)),
                support::stats("fc_peak_rss", "MiB", mib(&fc, |s| s.peak_rss_bytes)),
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
        // Diagnostic (branch restore-diag): KVM's exits and entries, traced from the spawn.
        let _ = Command::new("sudo")
            .args(["sh", "-c", "cd /sys/kernel/tracing && echo 0 > tracing_on && echo > trace && echo 8192 > buffer_size_kb && echo 1 > events/kvm/kvm_exit/enable && echo 1 > events/kvm/kvm_entry/enable && echo 1 > events/kvm/kvm_userspace_exit/enable && echo 1 > tracing_on"])
            .status();
        let start = Instant::now();
        let mut child = command.spawn().expect("spawning the VMM");
        let pid = child.id() as libc::pid_t;
        let (beat, out) = first_beat(child.stdout.take().unwrap());
        let stderr = child.stderr.take().unwrap();
        // Diagnostic (branch restore-diag): each stderr line with its arrival, from spawn.
        let err = thread::spawn(move || {
            let mut all = String::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                all.push_str(&format!(
                    "[+{:>8.0} us] {line}\n",
                    start.elapsed().as_secs_f64() * 1e6
                ));
            }
            all
        });
        after_spawn();
        let beat = beat.recv_timeout(TIMEOUT);
        // Diagnostic (branch restore-diag): the time from each exit to the next entry, by
        // reason, until the first beat; and what the guest printed before it.
        if beat.is_ok() {
            let summary = Command::new("sudo")
                .args(["sh", "-c"])
                .arg(concat!(
                    "cd /sys/kernel/tracing && echo 0 > tracing_on && cat trace | awk '",
                    r#"function ts(  i) { for (i = 1; i <= NF; i++) if ($i ~ /^[0-9]+\.[0-9]+:$/) return substr($i, 1, length($i) - 1); return 0 }
function after(w,  i) { for (i = 1; i < NF; i++) if ($i == w) return $(i + 1); return "?" }
/kvm_exit:/ { t = ts(); if ($1 in ran) { guest += (t - ran[$1]) * 1e6; runs++; delete ran[$1] } last[$1] = t; why[$1] = after("reason"); next }
/kvm_userspace_exit:/ { if ($1 in why) why[$1] = why[$1] "/user:" after("reason"); next }
/kvm_entry:/ { t = ts(); if ($1 in last) { r = why[$1]; n[r]++; us[r] += (t - last[$1]) * 1e6; delete last[$1] } ran[$1] = t }
END { printf "guest n=%d us=%.0f\n", runs, guest; for (r in n) printf "%s n=%d us=%.0f\n", r, n[r], us[r] }"#,
                    "' | sort -t= -k3 -n -r | head -25; echo 0 > events/kvm/enable 2>/dev/null; echo > trace"
                ))
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            println!("kvm-trace {:?}\n{summary}", command.get_program());
        }
        if beat.is_ok() {
            let dump = Command::new("sudo")
                .args(["sh", "-c"])
                .arg(format!("cd /sys/kernel/debug/kvm && for f in {pid}-*/* {pid}-*/vcpu0/*; do [ -f \"$f\" ] && v=$(cat \"$f\" 2>/dev/null) && [ \"$v\" != 0 ] && echo \"${{f##*/}}=$v\"; done | tr '\\n' ' '"))
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            println!("kvm-debugfs {:?} {dump}", command.get_program());
        }
        let overhead = beat.is_ok().then(|| {
            (0..READINGS)
                .map(|_| {
                    thread::sleep(READING_PERIOD);
                    overhead_bytes(pid, guest_bytes)
                })
                .max()
                .unwrap_or(0)
        });
        let peak = peak_bytes(pid);
        // SAFETY: the child is not yet reaped, so `pid` still names it.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        child.wait().unwrap();
        let (out, err) = (out.join().unwrap(), err.join().unwrap());
        if let Ok(at) = beat {
            let shown = String::from_utf8_lossy(&out);
            let before = shown.split("..").next().unwrap_or_default();
            println!(
                "restore-timeline {:?} first beat at +{:.0} us, after {:?}\n{err}",
                command.get_program(),
                at.duration_since(start).as_secs_f64() * 1e6,
                before.get(before.len().saturating_sub(300)..).unwrap_or(before)
            );
        }
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
