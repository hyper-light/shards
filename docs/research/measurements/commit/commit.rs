//! What restored VMs are charged against Linux's commit limit (audit D04;
//! docs/research/platform-measurements.md M56). A restore maps its template's memory file
//! privately and writable over the RAM it reserved, and Linux accounts a private writable
//! mapping at its whole size, whether or not a page of it is ever written
//! (Documentation/mm/overcommit-accounting.rst).
//!
//! One template of the `resume` test guest is saved, then restored with `--hold` again and
//! again, each held VM kept, until `COMMIT_VMS` are held or a restore fails. After each,
//! `Committed_AS` is read from /proc/meminfo; of each held VM, the mappings of the
//! template's files in /proc/PID/smaps: their size, resident and private pages, and
//! `VmFlags` (`ac`: accounted; `nr`: not reserved). The overcommit mode is the host's: the
//! workflow sets `vm.overcommit_memory` to 0, 1 and 2 in turn.
//!
//! Crates/shards/tests/commit.rs includes it as an ignored test, for the helpers E2E tests
//! use:
//!
//!     COMMIT_VMS=64 COMMIT_MEMORY=256 cargo test --release -p shards --test commit -- \
//!         --ignored --nocapture

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use super::common::{TempDir, cannot_run_vms, cannot_snapshot, kernel, shards_vm, test_guest};

const TIMEOUT: Duration = Duration::from_secs(120);

fn setting(name: &str, default: usize) -> usize {
    std::env::var(name).map_or(default, |v| v.parse().expect(name))
}

/// /proc/meminfo's `key`, in KiB.
fn meminfo(key: &str) -> u64 {
    let info = std::fs::read_to_string("/proc/meminfo").unwrap();
    info.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap_or_else(|| panic!("{key} in /proc/meminfo"))
}

fn sysctl(name: &str) -> String {
    std::fs::read_to_string(format!("/proc/sys/vm/{name}"))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// A restore held before its start request: ready, or what it said as it failed.
fn hold(snapshot: &Path) -> Result<Child, String> {
    let mut child = Command::new(shards_vm())
        .args([
            "restore".as_ref(),
            snapshot.as_os_str(),
            "--no-console".as_ref(),
            "--hold".as_ref(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        // At the limit, what fails first is whichever asks first: this process's fork too.
        .map_err(|e| format!("spawning the restore: {e}"))?;
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    // A small stack: under mode 2, near the limit, a restore's RAM is refused well before
    // this thread's stack would be.
    let reader = std::thread::Builder::new().stack_size(64 << 10).spawn(move || {
        let mut said = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line == "shards-ready" {
                let _ = tx.send(Ok(()));
                // Held from here: what it says later is read, and dropped.
                said.clear();
                continue;
            }
            said.push_str(&line);
            said.push('\n');
        }
        let _ = tx.send(Err(said));
    });
    if let Err(e) = reader {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("its reader thread: {e}"));
    }
    match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(())) => Ok(child),
        failed => {
            let _ = child.kill();
            let status = child.wait().unwrap();
            let said = match failed {
                Ok(Err(said)) => said,
                _ => "no shards-ready in time".into(),
            };
            Err(format!("{status}: {}", said.trim()))
        }
    }
}

/// A mapping of one of `dir`'s files: its size, Rss and Private_* in KiB, and VmFlags.
#[derive(Default)]
struct Mapping {
    path: String,
    size: u64,
    rss: u64,
    private: u64,
    flags: String,
}

fn mappings(pid: u32, dir: &Path) -> Vec<Mapping> {
    let smaps = std::fs::read_to_string(format!("/proc/{pid}/smaps")).unwrap();
    let dir = dir.to_str().unwrap();
    let mut out: Vec<Mapping> = Vec::new();
    let mut current: Option<Mapping> = None;
    for line in smaps.lines() {
        let kib = |l: &str| -> u64 {
            l.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse().ok())
                .unwrap_or(0)
        };
        if let Some(flags) = line.strip_prefix("VmFlags:") {
            if let Some(mut m) = current.take() {
                m.flags = flags.trim().to_string();
                out.push(m);
            }
        } else if line.starts_with("Size:") {
            if let Some(m) = current.as_mut() {
                m.size = kib(line);
            }
        } else if line.starts_with("Rss:") {
            if let Some(m) = current.as_mut() {
                m.rss = kib(line);
            }
        } else if line.starts_with("Private_Clean:") || line.starts_with("Private_Dirty:") {
            if let Some(m) = current.as_mut() {
                m.private += kib(line);
            }
        } else if !line.contains(':') || line.split_whitespace().next().is_some_and(|f| f.contains('-')) {
            // A mapping's header: range, perms, offset, device, inode, path.
            let path = line.split_whitespace().nth(5).unwrap_or("");
            current = path.starts_with(dir).then(|| Mapping {
                path: path.trim_start_matches(dir).to_string(),
                ..Mapping::default()
            });
        }
    }
    out
}

/// n, p50, p90, p99 and max of `v`, nearest rank.
fn summary(v: &mut [f64]) -> String {
    if v.is_empty() {
        return "n 0".into();
    }
    v.sort_by(f64::total_cmp);
    let at = |p: f64| v[(((p / 100.0) * v.len() as f64).ceil().max(1.0) as usize).min(v.len()) - 1];
    format!(
        "n {} | p50 {:.1} | p90 {:.1} | p99 {:.1} | max {:.1}",
        v.len(),
        at(50.0),
        at(90.0),
        at(99.0),
        v[v.len() - 1]
    )
}

#[test]
#[ignore = "a measurement: COMMIT_VMS=… cargo test --test commit -- --ignored --nocapture"]
fn commit_accounting() {
    if !cfg!(target_os = "linux") {
        println!("SKIP: commit accounting is Linux's");
        return;
    }
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let vms = setting("COMMIT_VMS", 64);
    let memory = setting("COMMIT_MEMORY", 256);
    let dir = TempDir::new("commit");
    let snapshot = dir.join("snapshot");
    let saved = Command::new(shards_vm())
        .args(["run", "--kernel"])
        .arg(kernel())
        .arg("--init")
        .arg(test_guest())
        .args(["--memory", &memory.to_string()])
        .args([
            "--cmdline",
            "quiet panic=-1 shards_test=resume",
            "--no-console",
            "--snapshot-dir",
        ])
        .arg(&snapshot)
        .status()
        .unwrap();
    assert!(saved.success(), "saving the template: {saved}");

    println!(
        "overcommit_memory {} | overcommit_ratio {} | CommitLimit {} MiB | MemTotal {} MiB | \
         guest RAM {memory} MiB",
        sysctl("overcommit_memory"),
        sysctl("overcommit_ratio"),
        meminfo("CommitLimit") / 1024,
        meminfo("MemTotal") / 1024,
    );
    let base = meminfo("Committed_AS");
    let mut held = Vec::new();
    let mut deltas = Vec::new();
    let (mut rss, mut private) = (Vec::new(), Vec::new());
    let mut last = base;
    let mut failure = None;
    for i in 0..vms {
        match hold(&snapshot) {
            Ok(child) => {
                let now = meminfo("Committed_AS");
                deltas.push((now as f64 - last as f64) / 1024.0);
                last = now;
                let maps = mappings(child.id(), &snapshot);
                if i == 0 {
                    for m in &maps {
                        println!(
                            "mapping {} | size {} KiB | rss {} KiB | private {} KiB | VmFlags {}",
                            m.path, m.size, m.rss, m.private, m.flags
                        );
                    }
                }
                rss.push(maps.iter().map(|m| m.rss).sum::<u64>() as f64 / 1024.0);
                private.push(maps.iter().map(|m| m.private).sum::<u64>() as f64 / 1024.0);
                held.push(child);
            }
            Err(e) => {
                failure = Some((i, e));
                break;
            }
        }
    }
    println!("held {} of {vms}", held.len());
    println!(
        "Committed_AS {} MiB before, {} MiB held ({:+} MiB)",
        base / 1024,
        last / 1024,
        (last as i64 - base as i64) / 1024
    );
    println!("commit per VM (MiB)         {}", summary(&mut deltas));
    println!("template mappings rss (MiB) {}", summary(&mut rss));
    println!("  of it private (MiB)       {}", summary(&mut private));
    match failure {
        Some((i, e)) => println!("restore {} failed: {e}", i + 1),
        None => println!("no restore failed"),
    }
    for mut child in held {
        let _ = child.kill();
        let _ = child.wait();
    }
}
