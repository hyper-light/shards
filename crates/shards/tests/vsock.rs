//! vsock between the host and real guests: connections both ways, many streams at once,
//! refusals, and restored copies of a guest that listens or holds a connection. On Windows
//! through a VM process confined as D141 confines it: its socket bound and its host ports
//! dialled by its spawner (PM M203).

#![allow(
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

mod common;

use shards_vmm::platform::{UnixListener, UnixStream};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::{TempDir, Vm, cannot_run_vms, cannot_snapshot, echo, kernel, test_guest, vsock_connect, within};

const TIMEOUT: Duration = Duration::from_secs(60);
/// The most a test's streams take, past which it fails saying what its VM said.
const STREAMING: Duration = Duration::from_secs(300);
const ECHO_PORT: u32 = 1234;

fn guest_args(mode: &str, vsock: &Path) -> Vec<std::ffi::OsString> {
    vec![
        "run".into(),
        "--kernel".into(),
        kernel().into(),
        "--init".into(),
        test_guest().into(),
        "--memory".into(),
        "256".into(),
        "--cmdline".into(),
        format!("console=ttyS0 quiet shards_test={mode}").into(),
        "--vsock".into(),
        vsock.into(),
    ]
}

fn os(args: &[std::ffi::OsString]) -> Vec<&std::ffi::OsStr> {
    args.iter().map(|a| a.as_os_str()).collect()
}

/// Boots the `vsock` guest, serves its connection to host port 5000, and waits until it
/// listens.
fn boot_echo_guest(dir: &Path) -> (Vm, PathBuf) {
    let sock = dir.join("v.sock");
    let host = UnixListener::bind(dir.join("v.sock_5000")).unwrap();
    // Its device's debug lines, which a failure prints with what the VM said.
    let mut vm = Vm::spawn_env(&os(&guest_args("vsock", &sock)), &[("SHARDS_LOG", "debug")]);
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| {
            let (mut s, _) = host.accept().map_err(|e| e.to_string())?;
            s.set_read_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;
            let mut line = String::new();
            BufReader::new(&s)
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            s.write_all(b"hello from the host\n").map_err(|e| e.to_string())?;
            Ok::<String, String>(line)
        })();
        let _ = tx.send(result);
    });
    vm.wait_for("SHARDS-TEST READY", TIMEOUT);
    let greeting = rx.recv_timeout(TIMEOUT).unwrap().unwrap();
    assert_eq!(greeting, "hello from the guest\n");
    (vm, sock)
}

#[test]
fn host_and_guest_connect_both_ways_and_stream_in_parallel() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("vsock-both");
    let (vm, sock) = boot_echo_guest(&dir);
    // Eight streams at once, each far past every credit window and socket buffer.
    within("eight streams through the echo", STREAMING, &vm, move || {
        let streams: Vec<_> = (0..8u64)
            .map(|salt| {
                let sock = sock.clone();
                thread::spawn(move || echo(&sock, ECHO_PORT, salt, 8 << 20, None).unwrap())
            })
            .collect();
        for s in streams {
            s.join().unwrap();
        }
    });
}

#[test]
fn refused_ports_and_bad_handshakes_close_the_host_socket() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("vsock-refuse");
    let (vm, sock) = boot_echo_guest(&dir);
    within("refusals, then an echo", STREAMING, &vm, move || {
        assert!(
            vsock_connect(&sock, 4321, TIMEOUT).is_err(),
            "a port nobody listens on was accepted"
        );
        let mut s = UnixStream::connect(&sock).unwrap();
        s.set_read_timeout(Some(TIMEOUT)).unwrap();
        s.write_all(b"HELLO 1234\n").unwrap();
        let mut rest = Vec::new();
        assert_eq!(
            s.read_to_end(&mut rest).unwrap(),
            0,
            "a bad handshake got {rest:?}"
        );
        // The device still serves after both.
        echo(&sock, ECHO_PORT, 99, 1 << 20, None).unwrap();
    });
}

#[test]
fn restored_copies_listen_on_their_own_sockets() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let dir = TempDir::new("vsock-restore");
    let snap = dir.join("snap");
    let mut args = guest_args("vsock_snapshot", &dir.join("original.sock"));
    args.extend(["--snapshot-dir".into(), snap.clone().into()]);
    let mut original = Vm::spawn(&os(&args));
    assert_eq!(
        original.wait_exit_following(TIMEOUT),
        Some(0),
        "{}",
        original.seen.join("\n")
    );

    let restore = |name: &str| {
        let sock = dir.join(name);
        let vm = Vm::spawn(&[
            "restore".as_ref(),
            snap.as_os_str(),
            "--vsock".as_ref(),
            sock.as_os_str(),
        ]);
        (vm, sock)
    };
    let (mut a, sock_a) = restore("a.sock");
    let (mut b, sock_b) = restore("b.sock");
    a.wait_for("SHARDS-TEST READY", TIMEOUT);
    b.wait_for("SHARDS-TEST READY", TIMEOUT);
    let (ta, tb) = (
        thread::spawn(move || echo(&sock_a, ECHO_PORT, 1, 4 << 20, None).unwrap()),
        thread::spawn(move || echo(&sock_b, ECHO_PORT, 2, 4 << 20, None).unwrap()),
    );
    ta.join().unwrap();
    tb.join().unwrap();

    let mut bare = Vm::spawn(&["restore".as_ref(), snap.as_os_str()]);
    assert_eq!(bare.wait_exit_following(TIMEOUT), Some(1));
    assert!(
        bare.seen.iter().any(|l| l.contains("--vsock")),
        "{}",
        bare.seen.join("\n")
    );
}

/// Serves one guest connection to host port 5000 at `sock`: reads the guest's line,
/// answers, and holds the connection until the guest's side closes.
fn greet_once(sock: &Path) -> thread::JoinHandle<String> {
    let mut path = sock.as_os_str().to_owned();
    path.push("_5000");
    let host = UnixListener::bind(path).unwrap();
    thread::spawn(move || {
        let (s, _) = host.accept().unwrap();
        s.set_read_timeout(Some(TIMEOUT)).unwrap();
        let mut reader = BufReader::new(&s);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        (&s).write_all(b"hello from the host\n").unwrap();
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        line
    })
}

/// A snapshot taken while the guest holds a connection to the host: each restored copy
/// finds that connection closed (its host is gone), dials its own host, and serves.
#[test]
fn restored_copies_find_held_connections_closed() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let dir = TempDir::new("vsock-held");
    let snap = dir.join("snap");
    let original_sock = dir.join("original.sock");
    let greeted = greet_once(&original_sock);
    let mut args = guest_args("vsock_snapshot_held", &original_sock);
    args.extend(["--snapshot-dir".into(), snap.clone().into()]);
    let mut original = Vm::spawn(&os(&args));
    assert_eq!(
        original.wait_exit_following(TIMEOUT),
        Some(0),
        "{}",
        original.seen.join("\n")
    );
    assert_eq!(greeted.join().unwrap(), "hello from the guest\n");

    for (salt, name) in [(1, "a.sock"), (2, "b.sock")] {
        let sock = dir.join(name);
        let greeted = greet_once(&sock);
        let mut copy = Vm::spawn(&[
            "restore".as_ref(),
            snap.as_os_str(),
            "--vsock".as_ref(),
            sock.as_os_str(),
        ]);
        copy.wait_for("SHARDS-TEST READY", TIMEOUT);
        assert_eq!(greeted.join().unwrap(), "hello from the guest\n");
        echo(&sock, ECHO_PORT, salt, 1 << 20, None).unwrap();
    }
}

/// The port the test guest's `vsock_bench` serves on.
const BENCH_PORT: u32 = 1240;

/// The vsock path's measurement (docs/research/measurements/vsock-path-v1). The test guest's
/// `vsock_bench` mode serves every arm. Each of `VSOCK_BENCH_ROUNDS` rounds (default 11)
/// moves `VSOCK_BENCH_BYTES` (default 256 MiB) host to guest (`K`), guest to host (`S`) and
/// both ways through the echo (`E`): directly over vsock, and through the engine relay's
/// pump in the guest (lower case). Then `VSOCK_BENCH_PINGS` (default 2,000) round trips of
/// 64 bytes, direct and relayed. `VSOCK_BENCH_CONFIGS` boots one VM per configuration,
/// `label:NAME=VALUE,...;label:...` (default one, `base`, with nothing set), the VM process
/// given those variables, and interleaves every configuration and arm each round in a
/// rotated order. A configuration's `SHARDS_VSOCK_SOCKBUF` also sizes this test's sockets,
/// as the VM process sizes both ends of the pairs it makes for its served ports. Prints n,
/// p50, p90, p99 and max, and on Linux the CPU time of each VM's processes.
/// `VSOCK_BENCH_CPUS` sets the guests' vCPUs.
#[test]
#[ignore = "a measurement, run on purpose: docs/research/measurements/vsock-path-v1"]
fn vsock_path_measurement() {
    if cannot_run_vms() {
        return;
    }
    let env = |name: &str, default: u64| -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let rounds = env("VSOCK_BENCH_ROUNDS", 11) as usize;
    let bytes = env("VSOCK_BENCH_BYTES", 256 << 20);
    let pings = env("VSOCK_BENCH_PINGS", 2000) as usize;
    let configs: Vec<(String, Vec<(String, String)>)> = std::env::var("VSOCK_BENCH_CONFIGS")
        .unwrap_or_else(|_| "base:".into())
        .split(';')
        .filter(|c| !c.trim().is_empty())
        .map(|c| {
            let (label, vars) = c.split_once(':').unwrap_or((c, ""));
            let vars = vars
                .split(',')
                .filter(|v| !v.is_empty())
                .map(|v| {
                    let (k, v) = v.split_once('=').unwrap();
                    (k.trim().to_string(), v.trim().to_string())
                })
                .collect();
            (label.trim().to_string(), vars)
        })
        .collect();
    let dir = TempDir::new("vsock-bench");
    let mut vms = Vec::new();
    for (i, (label, vars)) in configs.iter().enumerate() {
        let sock = dir.join(format!("v{i}.sock"));
        let mut args = guest_args("vsock_bench", &sock);
        if let Ok(cpus) = std::env::var("VSOCK_BENCH_CPUS") {
            args.extend(["--cpus".into(), cpus.into()]);
        }
        let vars: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let mut vm = Vm::spawn_env(&os(&args), &vars);
        vm.wait_for("SHARDS-TEST READY", TIMEOUT);
        let sockbuf = vars
            .iter()
            .find(|(k, _)| *k == "SHARDS_VSOCK_SOCKBUF")
            .and_then(|(_, v)| v.parse::<i32>().ok());
        println!("vsock-path: config {label} {vars:?} on VM process {}", vm.id());
        vms.push((label.clone(), vm, sock, sockbuf));
    }
    println!(
        "vsock-path: {rounds} rounds of {bytes} bytes, {pings} pings; host {}; load {}",
        host_line(),
        load_line()
    );
    let arms = *b"KkSsEe";
    type Bulk = Vec<(Duration, Option<[Duration; 3]>)>;
    let mut bulk: std::collections::BTreeMap<(usize, u8), Bulk> = Default::default();
    let mut rtts: std::collections::BTreeMap<(usize, u8), Vec<Duration>> = Default::default();
    for round in 0..rounds {
        for c in 0..vms.len() {
            let v = (c + round) % vms.len();
            let (_, vm, sock, sockbuf) = &vms[v];
            let guest_buf = configs[v]
                .1
                .iter()
                .find(|(k, _)| k == "VSOCK_GUEST_BUF")
                .and_then(|(_, v)| v.parse().ok())
                .unwrap_or(0);
            GUEST_BUF.with(|b| b.set(guest_buf));
            for i in 0..arms.len() {
                let op = arms[(i + round) % arms.len()];
                let cpu = vm_cpu(vm.id());
                eprintln!("vsock-path: round {round} config {} arm {}", vms[v].0, op as char);
                let took = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    bench_bulk(sock, op, bytes, *sockbuf)
                })) {
                    Ok(took) => took,
                    Err(panic) => {
                        let log = vm.log();
                        let lines = log.lock().unwrap_or_else(|e| e.into_inner());
                        let from = lines.len().saturating_sub(80);
                        eprintln!(
                            "vsock-path: VM {} said, last {} lines:",
                            vms[v].0,
                            lines.len() - from
                        );
                        for line in &lines[from..] {
                            eprintln!("  {line}");
                        }
                        std::panic::resume_unwind(panic);
                    }
                };
                eprintln!("vsock-path: took {took:?}");
                let used = vm_cpu(vm.id())
                    .zip(cpu)
                    .map(|(after, before)| [0, 1, 2].map(|i| after[i].saturating_sub(before[i])));
                bulk.entry((v, op)).or_default().push((took, used));
            }
            for op in if round % 2 == 0 {
                [b'E', b'e']
            } else {
                [b'e', b'E']
            } {
                rtts.entry((v, op))
                    .or_default()
                    .extend(bench_pings(sock, op, pings, *sockbuf));
            }
        }
    }
    println!("vsock-path: load after {}", load_line());
    let name = |op: u8| match op {
        b'K' => "host->guest direct",
        b'k' => "host->guest relayed",
        b'S' => "guest->host direct",
        b's' => "guest->host relayed",
        b'E' => "echo direct",
        b'e' => "echo relayed",
        _ => "?",
    };
    println!(
        "| config | arm | n | p50 | p90 | p99 | max | MiB/s at p50 | VM CPU p50 (vsock worker, vCPUs) |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    for (v, (label, ..)) in vms.iter().enumerate() {
        for op in arms {
            let samples = &bulk[&(v, op)];
            let mut wall: Vec<Duration> = samples.iter().map(|s| s.0).collect();
            let [p50, p90, p99, max] = quantiles(&mut wall);
            let mib = bytes as f64 / (1 << 20) as f64 / p50.as_secs_f64();
            let part = |i: usize| {
                let mut v: Vec<Duration> = samples.iter().filter_map(|s| s.1.map(|c| c[i])).collect();
                (!v.is_empty()).then(|| quantiles(&mut v)[0].as_secs_f64() * 1e3)
            };
            let cpu = match (part(0), part(1), part(2)) {
                (Some(all), Some(worker), Some(vcpus)) => {
                    format!("{all:.0} ms ({worker:.0}, {vcpus:.0})")
                }
                _ => "n/a".to_string(),
            };
            println!(
                "| {label} | {} | {} | {:.1} ms | {:.1} | {:.1} | {:.1} | {mib:.0} | {cpu} |",
                name(op),
                wall.len(),
                p50.as_secs_f64() * 1e3,
                p90.as_secs_f64() * 1e3,
                p99.as_secs_f64() * 1e3,
                max.as_secs_f64() * 1e3
            );
        }
    }
    println!("| config | 64 B round trip | n | p50 | p90 | p99 | max |");
    println!("|---|---|---|---|---|---|---|");
    for (v, (label, ..)) in vms.iter().enumerate() {
        for op in *b"Ee" {
            let samples = rtts.get_mut(&(v, op)).unwrap();
            let [p50, p90, p99, max] = quantiles(samples);
            println!(
                "| {label} | {} | {} | {:.1} µs | {:.1} | {:.1} | {:.1} |",
                name(op),
                samples.len(),
                p50.as_secs_f64() * 1e6,
                p90.as_secs_f64() * 1e6,
                p99.as_secs_f64() * 1e6,
                max.as_secs_f64() * 1e6
            );
        }
    }
}

/// Nearest-rank p50, p90, p99 and max of `samples`.
fn quantiles(samples: &mut [Duration]) -> [Duration; 4] {
    samples.sort();
    let at = |q: f64| samples[((samples.len() as f64 * q).ceil() as usize).clamp(1, samples.len()) - 1];
    [at(0.5), at(0.9), at(0.99), samples[samples.len() - 1]]
}

/// A bench connection to the guest asking for measurement `op` of `bytes` bytes.
fn bench_open(sock: &Path, op: u8, bytes: u64, sockbuf: Option<i32>) -> UnixStream {
    let mut s = vsock_connect(sock, BENCH_PORT, TIMEOUT).unwrap();
    let stall = std::env::var("VSOCK_BENCH_STALL_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(STREAMING, Duration::from_secs);
    s.set_read_timeout(Some(stall)).unwrap();
    #[cfg(unix)]
    if let Some(size) = sockbuf {
        use std::os::fd::AsRawFd as _;
        for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
            // SAFETY: setsockopt(2) of an int option on this test's own socket.
            let rc = unsafe {
                libc::setsockopt(
                    s.as_raw_fd(),
                    libc::SOL_SOCKET,
                    option,
                    (&raw const size).cast(),
                    std::mem::size_of::<i32>() as libc::socklen_t,
                )
            };
            assert_eq!(rc, 0, "setsockopt: {}", std::io::Error::last_os_error());
        }
    }
    #[cfg(not(unix))]
    let _ = sockbuf;
    let mut head = [0u8; 17];
    head[0] = op;
    head[1..9].copy_from_slice(&bytes.to_le_bytes());
    head[9..].copy_from_slice(&GUEST_BUF.with(|b| b.get()).to_le_bytes());
    s.write_all(&head).unwrap();
    s
}

thread_local! {
    /// The guest socket buffer the bench's next connections ask for (0: the guest's default).
    static GUEST_BUF: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How long measurement `op` took to move `bytes` bytes, from its request to its last byte.
fn bench_bulk(sock: &Path, op: u8, bytes: u64, sockbuf: Option<i32>) -> Duration {
    let mut buf = vec![0xa5u8; 64 << 10];
    let start = std::time::Instant::now();
    let mut s = bench_open(sock, op, bytes, sockbuf);
    match op.to_ascii_uppercase() {
        b'K' => {
            let mut left = bytes;
            while left > 0 {
                let n = (buf.len() as u64).min(left) as usize;
                s.write_all(&buf[..n]).unwrap();
                left -= n as u64;
            }
            let mut answer = [0u8; 8];
            s.read_exact(&mut answer).unwrap();
            assert_eq!(u64::from_le_bytes(answer), bytes);
        }
        b'S' => read_bytes(&mut s, &mut buf, bytes),
        _ => {
            let mut w = s.try_clone().unwrap();
            let writer = thread::spawn(move || {
                let buf = vec![0xa5u8; 64 << 10];
                let mut left = bytes;
                while left > 0 {
                    let n = (buf.len() as u64).min(left) as usize;
                    w.write_all(&buf[..n]).unwrap();
                    left -= n as u64;
                }
            });
            read_bytes(&mut s, &mut buf, bytes);
            writer.join().unwrap();
        }
    }
    let took = start.elapsed();
    // The guest's end: it ends its writes, then waits for ours.
    s.shutdown(std::net::Shutdown::Write).unwrap();
    let mut rest = Vec::new();
    s.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty(), "{} bytes after the measurement", rest.len());
    took
}

fn read_bytes(s: &mut UnixStream, buf: &mut [u8], bytes: u64) {
    let mut left = bytes;
    while left > 0 {
        let n = (buf.len() as u64).min(left) as usize;
        let got = s
            .read(&mut buf[..n])
            .unwrap_or_else(|e| panic!("stalled with {left} of {bytes} bytes left: {e}"));
        assert_ne!(got, 0, "the guest ended {left} bytes early");
        left -= got as u64;
    }
}

/// `n` round trips of 64 bytes through the echo `op` (`E` direct, `e` relayed).
fn bench_pings(sock: &Path, op: u8, n: usize, sockbuf: Option<i32>) -> Vec<Duration> {
    let size = 64u64;
    let mut s = bench_open(sock, op, size * n as u64, sockbuf);
    let out = [0x3cu8; 64];
    let mut back = [0u8; 64];
    let mut took = Vec::with_capacity(n);
    for _ in 0..n {
        let start = std::time::Instant::now();
        s.write_all(&out).unwrap();
        s.read_exact(&mut back).unwrap();
        took.push(start.elapsed());
    }
    s.shutdown(std::net::Shutdown::Write).unwrap();
    let mut rest = Vec::new();
    s.read_to_end(&mut rest).unwrap();
    took
}

/// CPU time of process `root` and its descendants, from /proc (Linux), as [all of it, the
/// vsock device's worker thread (`virtio-vsock`), the guest's vCPU threads (`vcpuN`)];
/// None elsewhere.
fn vm_cpu(root: u32) -> Option<[Duration; 3]> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: sysconf(3) of a constant.
        let tick = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        let stat = |path: String| -> Option<(String, u32, u64)> {
            let text = std::fs::read_to_string(path).ok()?;
            let (head, rest) = text.rsplit_once(')')?;
            let comm = head.split_once('(')?.1.to_string();
            // Fields after the command: state, ppid, ... utime (14th of stat), stime (15th).
            let fields: Vec<&str> = rest.split_whitespace().collect();
            let ppid = fields.get(1)?.parse().ok()?;
            let ticks = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
            Some((comm, ppid, ticks))
        };
        let mut all = Vec::new();
        for entry in std::fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|p| p.parse::<u32>().ok()) else {
                continue;
            };
            if let Some((_, ppid, _)) = stat(format!("/proc/{pid}/stat")) {
                all.push((pid, ppid));
            }
        }
        let mut tree = vec![root];
        loop {
            let grew: Vec<u32> = all
                .iter()
                .filter(|(pid, ppid)| tree.contains(ppid) && !tree.contains(pid))
                .map(|(pid, _)| *pid)
                .collect();
            if grew.is_empty() {
                break;
            }
            tree.extend(grew);
        }
        let mut split = [0u64; 3];
        for pid in tree {
            let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
                continue;
            };
            for task in tasks.flatten() {
                let Some((comm, _, ticks)) = stat(format!("{}/stat", task.path().display())) else {
                    continue;
                };
                split[0] += ticks;
                if comm == "virtio-vsock" {
                    split[1] += ticks;
                } else if comm.starts_with("vcpu") {
                    split[2] += ticks;
                }
            }
        }
        Some(split.map(|t| Duration::from_secs_f64(t as f64 / tick)))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = root;
        None
    }
}

fn load_line() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|l| l.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|_| {
            std::process::Command::new("sysctl")
                .args(["-n", "vm.loadavg"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default()
        })
}

fn host_line() -> String {
    let uname = std::process::Command::new("uname")
        .arg("-srm")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    format!(
        "{uname}, {} CPUs",
        thread::available_parallelism().map_or(0, |n| n.get())
    )
}

/// Measurement only (docs/research/measurements/vsock-path-v1): the ceiling of a byte ring
/// both sides map. The test guest's `dax_ring` maps `ring`, a file of an 8 MiB share, through
/// the share's DAX window; this test maps the same file, so both load and store the same host
/// pages. Each of `VSOCK_BENCH_ROUNDS` rounds (default 11) moves `VSOCK_BENCH_BYTES` (default
/// 256 MiB) guest to host and host to guest through its 4 MiB ring, both sides spinning.
#[test]
#[ignore = "a measurement, run on purpose: docs/research/measurements/vsock-path-v1"]
fn dax_ring_measurement() {
    use std::sync::atomic::{AtomicU64, Ordering};
    if cannot_run_vms() {
        return;
    }
    const RING_AT: usize = 4 << 20;
    const RING: usize = 4 << 20;
    let env = |name: &str, default: u64| -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let rounds = env("VSOCK_BENCH_ROUNDS", 11) as usize;
    let bytes = env("VSOCK_BENCH_BYTES", 256 << 20);
    let dir = TempDir::new("dax-ring");
    let share = dir.join("share");
    std::fs::create_dir(&share).unwrap();
    let path = share.join("ring");
    std::fs::write(&path, vec![0u8; RING_AT + RING]).unwrap();
    let home = dir.join("home");
    let args: Vec<std::ffi::OsString> = vec![
        "run".into(),
        "--kernel".into(),
        kernel().into(),
        "--init".into(),
        test_guest().into(),
        "--memory".into(),
        "256".into(),
        "--cmdline".into(),
        "console=ttyS0 quiet shards_test=dax_ring".into(),
        "--share".into(),
        share.clone().into(),
        "--share-window".into(),
        "64".into(),
    ];
    let home_str = home.to_str().unwrap().to_owned();
    let mut vm = Vm::spawn_env(&os(&args), &[("SHARDS_HOME", home_str.as_str())]);
    vm.wait_for("SHARDS-TEST READY", TIMEOUT);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    #[cfg(unix)]
    let base = {
        use std::os::fd::AsRawFd as _;
        // SAFETY: a shared mapping of the whole file, which outlives every use below.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                RING_AT + RING,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(p, libc::MAP_FAILED, "mmap: {}", std::io::Error::last_os_error());
        p.cast::<u8>()
    };
    #[cfg(not(unix))]
    let base: *mut u8 = {
        let _ = &file;
        eprintln!("SKIP: the ring measurement maps the file with mmap(2)");
        return;
    };
    // SAFETY: 8-aligned words in the mapped first page, used only atomically by both sides.
    let word = |at: usize| unsafe { &*base.add(at).cast::<AtomicU64>() };
    let (tail, head, command, count, done) = (word(0), word(128), word(256), word(264), word(272));
    // SAFETY: the ring's 4 MiB inside the mapping.
    let ring = unsafe { base.add(RING_AT) };
    let source = vec![0xa5u8; 64 << 10];
    let mut sink = vec![0u8; 64 << 10];
    let mut seq = 0u64;
    let mut took: [Vec<Duration>; 2] = [Vec::new(), Vec::new()];
    println!(
        "dax-ring: {rounds} rounds of {bytes} bytes; host {}; load {}",
        host_line(),
        load_line()
    );
    for round in 0..rounds {
        for k in 0..2u64 {
            let op = 1 + (k + round as u64) % 2;
            tail.store(0, Ordering::Release);
            head.store(0, Ordering::Release);
            count.store(bytes, Ordering::Release);
            seq += 1;
            let cmd = (seq << 8) | op;
            let start = std::time::Instant::now();
            command.store(cmd, Ordering::Release);
            let mut at = 0u64;
            let mut moved = 0u64;
            let deadline = start + Duration::from_secs(60);
            if op == 1 {
                // Guest to host: this side consumes.
                while moved < bytes {
                    let ready = tail.load(Ordering::Acquire) - at;
                    if ready == 0 {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "the guest's ring stalled at {moved}"
                        );
                        std::hint::spin_loop();
                        continue;
                    }
                    let off = (at % RING as u64) as usize;
                    let len = (sink.len() as u64).min(ready).min((RING - off) as u64) as usize;
                    // SAFETY: published bytes inside the mapped ring.
                    unsafe { std::ptr::copy_nonoverlapping(ring.add(off), sink.as_mut_ptr(), len) };
                    at += len as u64;
                    moved += len as u64;
                    head.store(at, Ordering::Release);
                }
            } else {
                // Host to guest: this side produces.
                while moved < bytes {
                    let free = RING as u64 - (at - head.load(Ordering::Acquire));
                    if free == 0 {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "the guest's ring stalled at {moved}"
                        );
                        std::hint::spin_loop();
                        continue;
                    }
                    let off = (at % RING as u64) as usize;
                    let len = (source.len() as u64)
                        .min(free)
                        .min(bytes - moved)
                        .min((RING - off) as u64) as usize;
                    // SAFETY: free bytes inside the mapped ring, published below.
                    unsafe { std::ptr::copy_nonoverlapping(source.as_ptr(), ring.add(off), len) };
                    at += len as u64;
                    moved += len as u64;
                    tail.store(at, Ordering::Release);
                }
            }
            while done.load(Ordering::Acquire) != cmd {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the guest never finished {cmd:#x}"
                );
                std::hint::spin_loop();
            }
            took[(op - 1) as usize].push(start.elapsed());
        }
    }
    let _ = &mut sink;
    println!("| ring (4 MiB, both sides spinning) | n | p50 | p90 | p99 | max | MiB/s at p50 |");
    println!("|---|---|---|---|---|---|---|");
    for (name, samples) in ["guest->host", "host->guest"].iter().zip(took.iter_mut()) {
        let [p50, p90, p99, max] = quantiles(samples);
        println!(
            "| {name} | {} | {:.1} ms | {:.1} | {:.1} | {:.1} | {:.0} |",
            samples.len(),
            p50.as_secs_f64() * 1e3,
            p90.as_secs_f64() * 1e3,
            p99.as_secs_f64() * 1e3,
            max.as_secs_f64() * 1e3,
            bytes as f64 / (1 << 20) as f64 / p50.as_secs_f64()
        );
    }
}
