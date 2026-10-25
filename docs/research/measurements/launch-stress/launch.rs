//! Launches the E2E tests' placed copy of `shards` again and again, as they launch it
//! (`std::process::Command`, its output piped and read), with commands that start and end
//! at once, from several threads beside whatever else the host runs (a full gate's load,
//! the flakes plan's step 8), and says how each launch ended: its exit status, or the
//! signal that ended it, and how long it took. A launch a signal ended is said whole, with
//! its output, as it happens. The copy is held as the tests hold it (a shared flock(2) on
//! its directory, tests/common `hold`), so that no test prunes it meanwhile.
//!
//!     rustc -O --edition 2024 docs/research/measurements/launch-stress/launch.rs -o LAUNCH
//!     LAUNCH DIR LAUNCHES THREADS [HOME]    (DIR: target/e2e/shards-DIGEST)
//!
//! With HOME, each launch is a client of the daemon in that home (SHARDS_HOME, which the
//! first starts), asking it what it holds, as a test's `shards ps` or `pull` does; a
//! DOCKER_CONFIG of its own, empty, as the tests give theirs.
//!
//! Prints, at the end: launches, how many exited 0, exited otherwise, and were ended by a
//! signal (each signal's count), and n, p50, p90, p99 and max of a launch's milliseconds.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Commands that start and end at once, each in its own process: no daemon, no home.
const COMMANDS: [&[&str]; 4] = [&["--version"], &["-h"], &["run", "-h"], &["pull", "-h"]];
/// A daemon's clients that end at once, its answer had (with HOME).
const CLIENTS: [&[&str]; 3] = [&["ps", "-q"], &["images", "-q"], &["ps", "-aq"]];

unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}
const LOCK_SH: i32 = 1;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(dir), Some(launches), Some(threads)) = (args.get(1), args.get(2), args.get(3)) else {
        eprintln!("usage: launch DIR LAUNCHES THREADS");
        std::process::exit(2);
    };
    let dir = PathBuf::from(dir);
    let launches: u64 = launches.parse().expect("LAUNCHES");
    let threads: u64 = threads.parse().expect("THREADS");
    let home = args.get(4).map(PathBuf::from);
    let config = home.as_ref().map(|h| {
        let c = h.join("docker-config");
        std::fs::create_dir_all(&c).expect("the docker config");
        c
    });
    let commands: &[&[&str]] = if home.is_some() { &CLIENTS } else { &COMMANDS };
    let held = std::fs::File::open(&dir).expect("the copy's directory");
    // SAFETY: flock(2) on a descriptor this process owns, held for its life.
    assert_eq!(unsafe { flock(held.as_raw_fd(), LOCK_SH) }, 0, "flock");
    let shards = dir.join("shards");
    let next = AtomicU64::new(0);
    let ended: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());
    let times: Mutex<Vec<f64>> = Mutex::new(Vec::new());
    let started = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= launches {
                        return;
                    }
                    let command = commands[(i % commands.len() as u64) as usize];
                    let t0 = Instant::now();
                    let mut cmd = Command::new(&shards);
                    if let (Some(home), Some(config)) = (&home, &config) {
                        cmd.env("SHARDS_HOME", home)
                            .env("DOCKER_CONFIG", config)
                            .env("SHARDS_LOCAL_STORE", "none");
                    }
                    let out = cmd
                        .args(command)
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .output();
                    let ms = t0.elapsed().as_secs_f64() * 1000.0;
                    let how = match &out {
                        Err(e) => format!("not launched: {e}"),
                        Ok(o) => match (o.status.code(), o.status.signal()) {
                            (Some(0), _) => "exit 0".to_string(),
                            (Some(code), _) => format!("exit {code}"),
                            (None, Some(signal)) => format!("signal {signal}"),
                            (None, None) => "neither".to_string(),
                        },
                    };
                    if how != "exit 0" {
                        let (stdout, stderr) = out.as_ref().map_or((String::new(), String::new()), |o| {
                            (
                                String::from_utf8_lossy(&o.stdout).into_owned(),
                                String::from_utf8_lossy(&o.stderr).into_owned(),
                            )
                        });
                        let mut err = std::io::stderr().lock();
                        let _ = writeln!(
                            err,
                            "launch {i} ({command:?}) {how} after {ms:.1} ms at {:.1} s\n--- stdout\n{stdout}\n--- stderr\n{stderr}",
                            started.elapsed().as_secs_f64()
                        );
                    }
                    *ended.lock().unwrap().entry(how).or_insert(0) += 1;
                    times.lock().unwrap().push(ms);
                }
            });
        }
    });
    let mut times = times.into_inner().unwrap();
    times.sort_by(f64::total_cmp);
    let at = |q: f64| times[((times.len() - 1) as f64 * q).round() as usize];
    println!(
        "copy {} | {} launches on {threads} threads in {:.1} s",
        shards.display(),
        times.len(),
        started.elapsed().as_secs_f64()
    );
    for (how, n) in ended.into_inner().unwrap() {
        println!("  {how}: {n}");
    }
    println!(
        "  ms: n {} p50 {:.2} p90 {:.2} p99 {:.2} max {:.2}",
        times.len(),
        at(0.5),
        at(0.9),
        at(0.99),
        at(1.0)
    );
}
