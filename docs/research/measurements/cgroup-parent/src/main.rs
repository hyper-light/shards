//! M248 (docs/research/platform-measurements.md): what placing a process in a slice of the
//! user's service manager costs the daemon (`shards_bus::place`, the daemon's own code),
//! and where the process's memory is charged once it is there.
//!
//! - `place N SLICE BUSY`: N fresh children, each placed as the one process of a scope of
//!   its own in SLICE, 20 ms apart, with BUSY threads spinning meanwhile. Each, once
//!   placed, touches 64 MiB, and its scope's `memory.current` is read before and after.
//!   Prints each placement's times (connecting to the bus, the placement to its end) and
//!   their n, p50, p90, p99 and max.
//! - `phases N SLICE BUSY`: as `place`, each placement timed by its steps: connecting to
//!   the bus, the match rule, the start's reply (and whether the child had moved by then),
//!   and the start's end.
//! - `delegated N SLICE BUSY`: this process placed once, as the one process of a delegated
//!   scope in SLICE; then N children, each moved into a cgroup of its own beneath it
//!   (made, written its pid, and removed once it has gone), as a spawner of the daemon's
//!   placed so would its microVMs'.
//! - `moved N SLICE`: N children that touch their 64 MiB first and are placed after: what
//!   the scope then holds of them (cgroup-v2.rst, "Memory Ownership": charges stay where
//!   they were made).

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MIB: usize = 64;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or_default();
    let result = match arg(0) {
        "place" => busy(arg(3).parse().unwrap_or(0), || run_places(arg(1).parse().unwrap_or(100), arg(2))),
        "phases" => busy(arg(3).parse().unwrap_or(0), || phases(arg(1).parse().unwrap_or(100), arg(2))),
        "delegated" => busy(arg(3).parse().unwrap_or(0), || delegated(arg(1).parse().unwrap_or(100), arg(2))),
        "moved" => moved(arg(1).parse().unwrap_or(10), arg(2)),
        "child" => child(arg(1) == "first"),
        _ => Err("usage: place|phases|delegated N SLICE BUSY | moved N SLICE".to_string()),
    };
    if let Err(e) = result {
        let _ = writeln!(std::io::stderr(), "cgroup-parent: {e}");
        std::process::exit(1);
    }
}

/// A child of this binary: it touches its memory first (`first`) or once told to, says
/// so, then holds it until its stdin ends.
fn child(first: bool) -> Result<(), String> {
    let mut stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let mut byte = [0u8; 1];
    // Its stdin ended before the word: it was placed and let go of untouched (`phases`).
    if !first && stdin.read_exact(&mut byte).is_err() {
        return Ok(());
    }
    let held = vec![1u8; MIB << 20];
    writeln!(out, "touched").and_then(|()| out.flush()).map_err(|e| e.to_string())?;
    let mut rest = Vec::new();
    let _ = stdin.read_to_end(&mut rest);
    std::hint::black_box(&held);
    Ok(())
}

fn spawn(first: bool) -> Result<(Child, BufReader<std::process::ChildStdout>), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut c = Command::new(exe)
        .args(["child", if first { "first" } else { "after" }])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let out = c.stdout.take().ok_or("no stdout")?;
    Ok((c, BufReader::new(out)))
}

fn said(out: &mut BufReader<std::process::ChildStdout>) -> Result<String, String> {
    let mut line = String::new();
    out.read_line(&mut line).map_err(|e| e.to_string())?;
    Ok(line.trim_end().to_string())
}

/// The cgroup `pid` is in, by its path under /sys/fs/cgroup.
fn cgroup_of(pid: u32) -> Result<String, String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).map_err(|e| e.to_string())?;
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(str::to_string)
        .ok_or_else(|| format!("no cgroup v2 line for {pid}: {text}"))
}

fn memory_current(cgroup: &str) -> Option<u64> {
    std::fs::read_to_string(format!("/sys/fs/cgroup{cgroup}/memory.current"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn uid() -> u32 {
    // SAFETY: getuid(2) takes nothing and cannot fail.
    unsafe { libc::getuid() }
}

/// Places `pid` in a new scope of `slice`: the time to the bus, and to the placement's end.
fn placed(pid: u32, unit: &str, slice: &str) -> Result<(Duration, Duration), String> {
    let t0 = Instant::now();
    let mut bus = shards_bus::user_bus(uid()).map_err(|e| e.to_string())?;
    let t1 = Instant::now();
    shards_bus::place(&mut bus, unit, slice, pid, "shards cgroup-parent probe", false).map_err(|e| e.to_string())?;
    let t2 = Instant::now();
    Ok((t1 - t0, t2 - t1))
}

/// `f`, with `threads` threads spinning meanwhile.
fn busy(threads: usize, f: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            });
        }
        let ran = f();
        stop.store(true, Ordering::Relaxed);
        ran
    })
}

fn phases(n: usize, slice: &str) -> Result<(), String> {
    let me = std::process::id();
    let mut out = std::io::stdout();
    let _ = writeln!(out, "from {}", cgroup_of(me)?);
    let (mut connects, mut matches, mut replies, mut ends) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut moved_by_reply = 0;
    for i in 0..n {
        let (mut c, _said_by) = spawn(false)?;
        let pid = c.id();
        let unit = format!("shardsprobe-{me}-p{i}.scope");
        let t0 = Instant::now();
        let mut bus = shards_bus::user_bus(uid()).map_err(|e| e.to_string())?;
        let t1 = Instant::now();
        shards_bus::watch_jobs(&mut bus).map_err(|e| e.to_string())?;
        let t2 = Instant::now();
        let job = shards_bus::start_scope(&mut bus, &unit, slice, pid, "shards cgroup-parent probe", false)
            .map_err(|e| e.to_string())?;
        let t3 = Instant::now();
        if cgroup_of(pid)?.ends_with(&format!("/{unit}")) {
            moved_by_reply += 1;
        }
        shards_bus::started(&mut bus, &job, &unit).map_err(|e| e.to_string())?;
        let t4 = Instant::now();
        let _ = writeln!(
            out,
            "{i} connect_us {} match_us {} reply_us {} ended_us {}",
            (t1 - t0).as_micros(),
            (t2 - t1).as_micros(),
            (t3 - t2).as_micros(),
            (t4 - t3).as_micros()
        );
        connects.push(t1 - t0);
        matches.push(t2 - t1);
        replies.push(t3 - t2);
        ends.push(t4 - t3);
        drop(c.stdin.take());
        let _ = c.wait();
        std::thread::sleep(Duration::from_millis(20));
    }
    for (name, v) in [
        ("connect", &mut connects),
        ("match", &mut matches),
        ("reply", &mut replies),
        ("ended", &mut ends),
    ] {
        let _ = writeln!(out, "{name}: {}", summary(v));
    }
    let _ = writeln!(out, "moved by the start's reply: {moved_by_reply} of {n}");
    Ok(())
}

fn delegated(n: usize, slice: &str) -> Result<(), String> {
    let me = std::process::id();
    let mut out = std::io::stdout();
    let _ = writeln!(out, "from {}", cgroup_of(me)?);
    let unit = format!("shardsprobe-{me}-d.scope");
    let t0 = Instant::now();
    let mut bus = shards_bus::user_bus(uid()).map_err(|e| e.to_string())?;
    shards_bus::place(&mut bus, &unit, slice, me, "shards cgroup-parent probe", true).map_err(|e| e.to_string())?;
    let _ = writeln!(out, "placed itself in a delegated scope: {} us", t0.elapsed().as_micros());
    let root = format!("/sys/fs/cgroup{}", cgroup_of(me)?);
    // Itself into a leaf of its own: a cgroup that gives its children controllers holds no
    // process (cgroup-v2.rst, "No Internal Process Constraint").
    let write = |path: &str, what: &str| std::fs::write(path, what).map_err(|e| format!("{path}: {e}"));
    std::fs::create_dir(format!("{root}/keeper")).map_err(|e| e.to_string())?;
    write(&format!("{root}/keeper/cgroup.procs"), &me.to_string())?;
    let controllers = std::fs::read_to_string(format!("{root}/cgroup.controllers")).map_err(|e| e.to_string())?;
    let enable: Vec<String> = controllers.split_whitespace().map(|c| format!("+{c}")).collect();
    write(&format!("{root}/cgroup.subtree_control"), &enable.join(" "))?;
    let _ = writeln!(out, "its controllers given to its children: {}", controllers.trim());
    let (mut moves, mut removes) = (Vec::new(), Vec::new());
    let mut charged = Vec::new();
    for i in 0..n {
        let (mut c, mut said_by) = spawn(false)?;
        let leaf = format!("{root}/vm-{i}");
        let t0 = Instant::now();
        std::fs::create_dir(&leaf).map_err(|e| e.to_string())?;
        write(&format!("{leaf}/cgroup.procs"), &c.id().to_string())?;
        let moved = t0.elapsed();
        let cg = cgroup_of(c.id())?;
        if !cg.ends_with(&format!("/vm-{i}")) {
            return Err(format!("moved to vm-{i}, but the child is in {cg}"));
        }
        let before = memory_current(&cg);
        c.stdin.as_mut().ok_or("no stdin")?.write_all(b"g").map_err(|e| e.to_string())?;
        if said(&mut said_by)? != "touched" {
            return Err("the child said nothing".to_string());
        }
        if let (Some(b), Some(a)) = (before, memory_current(&cg)) {
            charged.push(a.saturating_sub(b));
        }
        drop(c.stdin.take());
        let _ = c.wait();
        let t1 = Instant::now();
        std::fs::remove_dir(&leaf).map_err(|e| e.to_string())?;
        let removed = t1.elapsed();
        let _ = writeln!(out, "{i} remove_us {} move_us {}", removed.as_micros(), moved.as_micros());
        moves.push(moved);
        removes.push(removed);
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = writeln!(out, "move (mkdir, cgroup.procs): {}", summary(&mut moves));
    let _ = writeln!(out, "remove (rmdir): {}", summary(&mut removes));
    charged.sort_unstable();
    let _ = writeln!(
        out,
        "charged after moving, of {MIB} MiB touched: min {} KiB, max {} KiB (n={})",
        charged.first().copied().unwrap_or(0) >> 10,
        charged.last().copied().unwrap_or(0) >> 10,
        charged.len()
    );
    Ok(())
}

fn run_places(n: usize, slice: &str) -> Result<(), String> {
    let me = std::process::id();
    let mut out = std::io::stdout();
    let _ = writeln!(out, "from {}", cgroup_of(me)?);
    let (mut connects, mut places, mut totals) = (Vec::new(), Vec::new(), Vec::new());
    let mut charged = Vec::new();
    for i in 0..n {
        let (mut c, mut said_by) = spawn(false)?;
        let unit = format!("shardsprobe-{me}-{i}.scope");
        let (connect, placing) = placed(c.id(), &unit, slice)?;
        let cg = cgroup_of(c.id())?;
        if !cg.ends_with(&format!("/{slice}/{unit}")) {
            return Err(format!("{unit} placed, but the child is in {cg}"));
        }
        let before = memory_current(&cg);
        c.stdin.as_mut().ok_or("no stdin")?.write_all(b"g").map_err(|e| e.to_string())?;
        if said(&mut said_by)? != "touched" {
            return Err("the child said nothing".to_string());
        }
        let after = memory_current(&cg);
        if let (Some(b), Some(a)) = (before, after) {
            charged.push(a.saturating_sub(b));
        }
        drop(c.stdin.take());
        let _ = c.wait();
        let _ = writeln!(
            out,
            "{i} connect_us {} place_us {} total_us {}",
            connect.as_micros(),
            placing.as_micros(),
            (connect + placing).as_micros()
        );
        connects.push(connect);
        places.push(placing);
        totals.push(connect + placing);
        std::thread::sleep(Duration::from_millis(20));
    }
    for (name, v) in [("connect", &mut connects), ("place", &mut places), ("total", &mut totals)] {
        let _ = writeln!(out, "{name}: {}", summary(v));
    }
    if charged.is_empty() {
        let _ = writeln!(out, "charged: the scopes have no memory.current");
    } else {
        charged.sort_unstable();
        let _ = writeln!(
            out,
            "charged after placing, of {MIB} MiB touched: min {} KiB, max {} KiB (n={})",
            charged.first().copied().unwrap_or(0) >> 10,
            charged.last().copied().unwrap_or(0) >> 10,
            charged.len()
        );
    }
    Ok(())
}

fn moved(n: usize, slice: &str) -> Result<(), String> {
    let me = std::process::id();
    let mut out = std::io::stdout();
    let from = cgroup_of(me)?;
    let _ = writeln!(out, "from {from}");
    for i in 0..n {
        let (mut c, mut said_by) = spawn(true)?;
        if said(&mut said_by)? != "touched" {
            return Err("the child said nothing".to_string());
        }
        let unit = format!("shardsprobe-{me}-m{i}.scope");
        placed(c.id(), &unit, slice)?;
        let cg = cgroup_of(c.id())?;
        let _ = writeln!(
            out,
            "{i} touched {MIB} MiB, then placed: its scope holds {} KiB",
            memory_current(&cg).map_or_else(|| "no memory.current".to_string(), |b| (b >> 10).to_string())
        );
        drop(c.stdin.take());
        let _ = c.wait();
    }
    Ok(())
}

/// n, p50, p90, p99 and max, nearest-rank, in µs.
fn summary(v: &mut [Duration]) -> String {
    v.sort_unstable();
    let at = |q: f64| {
        let rank = ((q * v.len() as f64).ceil() as usize).clamp(1, v.len().max(1));
        v.get(rank - 1).map_or(0, Duration::as_micros)
    };
    format!(
        "n={} p50={} p90={} p99={} max={} us",
        v.len(),
        at(0.50),
        at(0.90),
        at(0.99),
        v.last().map_or(0, Duration::as_micros)
    )
}
