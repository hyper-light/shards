//! What a VM's network process costs its host as the connections it holds grow (D31, D14;
//! docs/design/architecture.md, E2): its memory and its VM process's, beside N connections
//! a builder's `RUN` step opens to a server of this bench's and holds (`testguest conns`),
//! each fresh or after it moved B bytes each way, measured once they are idle; and a round
//! trip's time beside them, as the step measures it, which shows whether a wait costs what
//! is ready or what is watched (review 2.14, PM M106).
//!
//! The step reaches this host's address on its default route through the builder's
//! network process, as the build tests' steps do: its connections are TCP's, each a
//! record of the network process's and a socket of the host's, as a run's are whichever
//! side opened them.
//!
//! Memory, as each host counts it: Linux, each process's resident, proportional and
//! private pages (`/proc/PID/smaps_rollup`), and the host's slab (`/proc/meminfo`), where
//! both ends of each connection are this host's sockets; macOS, resident size and
//! physical footprint (`proc_pid_rusage`); Windows, the working set and private bytes
//! (`GetProcessMemoryInfo`).
//!
//! `cargo bench -p shards --bench net_memory [-- --counts 0,1000,10000 --bytes 0,1048576
//! --rounds 3 --trips 2000]`

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout
)]

#[path = "../tests/common/mod.rs"]
mod common;
mod support;

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long a case may take, its build included: 10,000 connections that move 1 MiB each
/// way take a minute and more under load.
const CASE: Duration = Duration::from_secs(900);

fn list(name: &str, default: &[u64]) -> Vec<u64> {
    support::option(name).map_or_else(
        || default.to_vec(),
        |v| v.split(',').map(|n| n.parse().expect("a number")).collect(),
    )
}

fn main() {
    if common::cannot_run_vms() {
        return;
    }
    let counts = list("--counts", &[0, 1_000, 10_000]);
    let sizes = list("--bytes", &[0, 1 << 20]);
    let rounds: usize = support::option("--rounds").map_or(3, |v| v.parse().expect("--rounds N"));
    let trips: usize = support::option("--trips").map_or(2_000, |v| v.parse().expect("--trips N"));
    // This host's address on its default route: a UDP connect sends nothing.
    let host = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").map(|()| s))
        .and_then(|s| s.local_addr());
    let Ok(host) = host else {
        println!("SKIP: this host has no route to give a guest an address of it");
        return;
    };
    let listener = TcpListener::bind("0.0.0.0:0").unwrap();
    let to = format!("{}:{}", host.ip(), listener.local_addr().unwrap().port());
    let (image, _) = common::served();
    let home = common::TempDir::new("nm-home");
    let server = Server {
        listener,
        idle: Mutex::new(Vec::new()),
    };
    let (held_tx, held_rx) = mpsc::channel::<mpsc::Sender<()>>();
    let mut rows = Vec::new();
    std::thread::scope(|s| {
        let serving = s.spawn(|| server.serve(&held_tx));
        // A case that fails ends the server too, which would otherwise hold the scope.
        let measured = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for &bytes in &sizes {
                for &n in &counts {
                    let mut cases = Vec::new();
                    for round in 0..rounds {
                        let c = case(&home, &image, &to, n, bytes, trips, &held_rx);
                        // Each case as it ends, which a case that fails after it keeps.
                        let p50 = |v: &[f64]| {
                            let mut v = v.to_vec();
                            v.sort_by(f64::total_cmp);
                            v.get(v.len() / 2).copied().unwrap_or(0.0)
                        };
                        println!(
                            "case n={n} b={bytes} round={round}: {:.1} s, connect p50 {:.0} us, rtt p50 {:.0} us, net {:?} MiB",
                            c.secs,
                            p50(&c.connect),
                            p50(&c.rtt),
                            c.net
                        );
                        cases.push(c);
                    }
                    let name = |what: &str| format!("{what} n={n} b={bytes}");
                    for (i, (metric, _)) in cases[0].net.iter().enumerate() {
                        let v = cases.iter().map(|c| c.net[i].1).collect();
                        rows.push(support::stats(&name(&format!("net {metric}")), "MiB", v));
                    }
                    for (i, (metric, _)) in cases[0].vm.iter().enumerate() {
                        let v = cases.iter().map(|c| c.vm[i].1).collect();
                        rows.push(support::stats(&name(&format!("vm {metric}")), "MiB", v));
                    }
                    if cases.iter().all(|c| c.slab.is_some()) {
                        let v = cases.iter().map(|c| c.slab.unwrap_or_default()).collect();
                        rows.push(support::stats(&name("host slab"), "MiB", v));
                    }
                    if n > 0 {
                        let connect = cases.iter().flat_map(|c| c.connect.iter().copied()).collect();
                        rows.push(support::stats(&name("connect"), "us", connect));
                    }
                    let rtt = cases.iter().flat_map(|c| c.rtt.iter().copied()).collect();
                    rows.push(support::stats(&name("rtt"), "us", rtt));
                }
            }
        }));
        // The server's last connection: it ends its loop.
        let _ = TcpStream::connect(&to).and_then(|mut c| c.write_all(b"q"));
        serving.join().unwrap();
        if let Err(failed) = measured {
            std::panic::resume_unwind(failed);
        }
    });
    support::report(
        "net_memory",
        &[
            ("counts", format!("{counts:?}")),
            ("bytes", format!("{sizes:?}")),
            ("rounds", rounds.to_string()),
            ("trips", trips.to_string()),
        ],
        &rows,
    );
}

/// The connections' far end: each says what it is first (`testguest conns`).
struct Server {
    listener: TcpListener,
    /// The connections a step holds idle, closed once its case is done.
    idle: Mutex<Vec<TcpStream>>,
}

impl Server {
    /// Serves each connection as it comes: `i` answered; `b` and a length read whole, then
    /// as many bytes sent back; `t` echoed until it says `H`, when the case's measurement
    /// is told and the connection held until it says to let go; `q`, the bench's end.
    fn serve(&self, held: &mpsc::Sender<mpsc::Sender<()>>) {
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok((mut c, _)) = self.listener.accept() {
            c.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
            let mut kind = [0u8; 1];
            if c.read_exact(&mut kind).is_err() {
                continue;
            }
            match kind[0] {
                b'i' => {
                    if c.write_all(b"i").is_ok() {
                        self.idle.lock().unwrap().push(c);
                    }
                }
                b'b' => {
                    let mut len = [0u8; 8];
                    let moved = c.read_exact(&mut len).and_then(|()| {
                        let total = u64::from_be_bytes(len);
                        let mut left = total;
                        while left > 0 {
                            let m = left.min(buf.len() as u64) as usize;
                            c.read_exact(&mut buf[..m])?;
                            left -= m as u64;
                        }
                        buf.fill(0);
                        let mut left = total;
                        while left > 0 {
                            let m = left.min(buf.len() as u64) as usize;
                            c.write_all(&buf[..m])?;
                            left -= m as u64;
                        }
                        Ok(())
                    });
                    if moved.is_ok() {
                        self.idle.lock().unwrap().push(c);
                    }
                }
                b't' => {
                    let _ = c.set_nodelay(true);
                    if c.write_all(b"t").is_err() {
                        continue;
                    }
                    // Echoed until the step says it holds its connections.
                    let said = loop {
                        match c.read(&mut buf) {
                            Ok(0) | Err(_) => break false,
                            Ok(got) if &buf[..got] == b"H" => break true,
                            Ok(got) => {
                                if c.write_all(&buf[..got]).is_err() {
                                    break false;
                                }
                            }
                        }
                    };
                    if said {
                        let (go_tx, go_rx) = mpsc::channel();
                        if held.send(go_tx).is_ok() {
                            let _ = go_rx.recv();
                        }
                    }
                    // The held ones closed from this side first, then the talking one, which
                    // ends the step: each of the network process's sockets is closed by its
                    // peer first, and keeps no port in TIME_WAIT. On macOS a host address has
                    // 16,384 ports for its connections, which TIME_WAIT holds for 30 s
                    // whatever their peer (connect: EADDRNOTAVAIL, as cases followed one
                    // another), where Linux reuses one for another peer.
                    self.idle.lock().unwrap().clear();
                    drop(c);
                }
                _ => return,
            }
        }
    }
}

/// One case's numbers: each process's memory by what its host counts, in MiB; the host's
/// slab where it says (Linux); the step's connects and round trips, in µs; and how long
/// the case took, its build's start to its end.
struct Case {
    net: Vec<(&'static str, f64)>,
    vm: Vec<(&'static str, f64)>,
    slab: Option<f64>,
    connect: Vec<f64>,
    rtt: Vec<f64>,
    secs: f64,
}

/// A build whose one step holds `n` connections to `to`, each moving `bytes` each way
/// first; measured once it says it holds them, then let go.
fn case(
    home: &common::TempDir,
    image: &str,
    to: &str,
    n: u64,
    bytes: u64,
    trips: usize,
    held: &mpsc::Receiver<mpsc::Sender<()>>,
) -> Case {
    let ctx = common::TempDir::new("nm-ctx");
    std::fs::write(
        ctx.join("Dockerfile"),
        format!(
            "FROM {image}\nRUN [\"/bin/testguest\", \"conns\", \"{to}\", \"{n}\", \"{trips}\", \"{bytes}\"]\n"
        ),
    )
    .unwrap();
    // A step starts with the guest kernel's 1,024 descriptors of 4,096; the connections,
    // their talking one and its standard streams need more.
    let nofile = format!("nofile={0}:{0}", n + 64);
    let mut child = common::command()
        .args(["build", "--progress=plain", "--no-cache", "--ulimit", &nofile])
        .arg(ctx.as_os_str())
        .env("SHARDS_HOME", home.as_os_str())
        .env("SHARDS_KERNEL", common::kernel())
        .env("SHARDS_INIT", common::guest_init())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawning shards build");
    let read = |mut r: Box<dyn std::io::Read + Send>| {
        std::thread::spawn(move || {
            let mut all = Vec::new();
            let _ = r.read_to_end(&mut all);
            String::from_utf8_lossy(&all).into_owned()
        })
    };
    let out = read(Box::new(child.stdout.take().unwrap()));
    let err = read(Box::new(child.stderr.take().unwrap()));
    let start = Instant::now();
    let go = loop {
        match held.recv_timeout(Duration::from_millis(100)) {
            Ok(go) => break go,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => panic!("the server ended: {e}"),
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "the build ended ({status}) before its step held its connections\n--- stdout\n{}\n--- stderr\n{}",
                out.join().unwrap(),
                err.join().unwrap()
            );
        }
        if start.elapsed() > CASE {
            let _ = child.kill();
            panic!("no step held its connections within {CASE:?}");
        }
    };
    // Measured once every connection is idle as the network process counts it, past one
    // RTO (200 ms) with nothing to send, when it lets go of what their queues held: five
    // times that after the step says it holds them.
    std::thread::sleep(Duration::from_secs(1));
    let under = descendants(child.id());
    let only = |name: &str| -> u32 {
        let found: Vec<u32> = under
            .iter()
            .filter(|(_, n)| n.trim_end_matches(".exe") == name)
            .map(|(pid, _)| *pid)
            .collect();
        assert_eq!(found.len(), 1, "the build's {name} processes: {under:?}");
        found[0]
    };
    let (net, vm) = (only("shards-net"), only("shards-vm"));
    let measured = Case {
        net: memory(net),
        vm: memory(vm),
        slab: slab_mib(),
        connect: Vec::new(),
        rtt: Vec::new(),
        secs: 0.0,
    };
    let _ = go.send(());
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > CASE {
            let _ = child.kill();
            panic!("the build did not end within {CASE:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (out, err) = (out.join().unwrap(), err.join().unwrap());
    assert!(
        status.success(),
        "the build failed ({status})\n--- stdout\n{out}\n--- stderr\n{err}"
    );
    let rtt = err
        .lines()
        .find_map(|l| l.split_once(" conns rtt-us ").map(|(_, v)| v))
        .unwrap_or_else(|| panic!("no round trips reported\n{err}"))
        .split_whitespace()
        .map(|v| v.parse().unwrap())
        .collect();
    let connect: Vec<f64> = err
        .lines()
        .filter_map(|l| l.split_once(" conns connect-us ").map(|(_, v)| v))
        .flat_map(str::split_whitespace)
        .map(|v| v.parse().unwrap())
        .collect();
    assert_eq!(
        u64::try_from(connect.len()).ok(),
        Some(n),
        "each connect's time\n{err}"
    );
    Case {
        connect,
        rtt,
        secs: start.elapsed().as_secs_f64(),
        ..measured
    }
}

/// Every process under `root`, at any depth, by pid with its name.
fn descendants(root: u32) -> Vec<(u32, String)> {
    let all = processes();
    let mut under = Vec::new();
    let mut parents = vec![root];
    while let Some(parent) = parents.pop() {
        for (pid, ppid, name) in &all {
            if *ppid == parent && *pid != parent {
                under.push((*pid, name.clone()));
                parents.push(*pid);
            }
        }
    }
    under
}

/// Every process of this host: its pid, its parent's, and its name.
#[cfg(target_os = "linux")]
fn processes() -> Vec<(u32, u32, String)> {
    let mut all = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        // `PID (COMM) STATE PPID ...`: the name may hold spaces and parentheses.
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let (Some(name), Some(rest)) = (stat.get(open + 1..close), stat.get(close + 1..)) else {
            continue;
        };
        let ppid = rest
            .split_whitespace()
            .nth(1)
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        all.push((pid, ppid, name.to_string()));
    }
    all
}

#[cfg(target_os = "macos")]
fn processes() -> Vec<(u32, u32, String)> {
    // SAFETY: a count of every pid, with no buffer.
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let mut pids = vec![0 as libc::pid_t; usize::try_from(n).unwrap() + 64];
    let bytes = i32::try_from(pids.len() * size_of::<libc::pid_t>()).unwrap();
    // SAFETY: a buffer of `bytes` bytes.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(usize::try_from(n).unwrap());
    let mut all = Vec::new();
    for pid in pids {
        // SAFETY: proc_bsdinfo is plain data, for which all zeroes is a value.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = size_of::<libc::proc_bsdinfo>() as i32;
        // SAFETY: a proc_bsdinfo of its own size.
        let got = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
        if got != size {
            continue;
        }
        // Its name, else its command's, which is cut to 16 bytes.
        let named = if info.pbi_name[0] != 0 {
            &info.pbi_name[..]
        } else {
            &info.pbi_comm[..]
        };
        let name: Vec<u8> = named.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
        all.push((
            pid as u32,
            info.pbi_ppid,
            String::from_utf8_lossy(&name).into_owned(),
        ));
    }
    all
}

#[cfg(windows)]
fn processes() -> Vec<(u32, u32, String)> {
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
    let mut all = Vec::new();
    // SAFETY: the snapshot, and an entry of its own size.
    let mut more = unsafe { Process32FirstW(snap, &raw mut entry) } != 0;
    while more {
        let end = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        all.push((
            entry.th32ProcessID,
            entry.th32ParentProcessID,
            String::from_utf16_lossy(&entry.szExeFile[..end]),
        ));
        // SAFETY: as above.
        more = unsafe { Process32NextW(snap, &raw mut entry) } != 0;
    }
    // SAFETY: the snapshot made above.
    unsafe { CloseHandle(snap) };
    all
}

const MIB: f64 = 1024.0 * 1024.0;

/// A process's memory, by what this host counts, in MiB.
#[cfg(target_os = "linux")]
fn memory(pid: u32) -> Vec<(&'static str, f64)> {
    let rollup = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).unwrap();
    let kib = |field: &str| -> f64 {
        rollup
            .lines()
            .find_map(|l| l.strip_prefix(field))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<f64>().ok())
            .unwrap_or_else(|| panic!("{field} in {rollup}"))
    };
    vec![
        ("rss", kib("Rss:") * 1024.0 / MIB),
        ("pss", kib("Pss:") * 1024.0 / MIB),
        (
            "private",
            (kib("Private_Clean:") + kib("Private_Dirty:")) * 1024.0 / MIB,
        ),
    ]
}

#[cfg(target_os = "macos")]
fn memory(pid: u32) -> Vec<(&'static str, f64)> {
    // SAFETY: rusage_info_v4 is plain data, for which all zeroes is a value.
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    // SAFETY: an rusage_info_v4 for the flavor that fills one.
    let got = unsafe { libc::proc_pid_rusage(pid as i32, libc::RUSAGE_INFO_V4, (&raw mut info).cast()) };
    assert_eq!(
        got,
        0,
        "proc_pid_rusage {pid}: {}",
        std::io::Error::last_os_error()
    );
    vec![
        ("resident", info.ri_resident_size as f64 / MIB),
        ("footprint", info.ri_phys_footprint as f64 / MIB),
    ]
}

#[cfg(windows)]
fn memory(pid: u32) -> Vec<(&'static str, f64)> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: a handle to query, closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    assert!(
        !process.is_null(),
        "OpenProcess {pid}: {}",
        std::io::Error::last_os_error()
    );
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: counters of their own size.
    let got = unsafe { GetProcessMemoryInfo(process, (&raw mut counters).cast(), counters.cb) };
    let error = std::io::Error::last_os_error();
    // SAFETY: the handle opened above.
    unsafe { CloseHandle(process) };
    assert_ne!(got, 0, "GetProcessMemoryInfo {pid}: {error}");
    vec![
        ("working set", counters.WorkingSetSize as f64 / MIB),
        ("private", counters.PrivateUsage as f64 / MIB),
    ]
}

/// The kernel's slab, in MiB, where the host says (Linux): both ends of each connection
/// are this host's sockets.
fn slab_mib() -> Option<f64> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    meminfo
        .lines()
        .find_map(|l| l.strip_prefix("Slab:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<f64>().ok())
        .map(|kib| kib * 1024.0 / MIB)
}
