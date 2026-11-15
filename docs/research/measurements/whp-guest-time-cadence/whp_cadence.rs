//! Scratch probe (g121 whp lane, never committed): how often a --host-managed guest's vCPU
//! guest time, as USE reads it, moves while its one vCPU spins: the intervals between the
//! reads that saw it move, and each read's own latency, n p50 p90 p99 max.
#![allow(clippy::panic, clippy::unwrap_used)]
mod common;
use std::io::{Read as _, Write as _};
use std::time::{Duration, Instant};

fn summary(name: &str, v: &mut [u64]) {
    v.sort_unstable();
    let at = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
    if v.is_empty() {
        println!("CADENCE {name}: none");
        return;
    }
    println!(
        "CADENCE {name}: n {} p50 {} p90 {} p99 {} max {} (us)",
        v.len(), at(0.5) / 1000, at(0.9) / 1000, at(0.99) / 1000, v[v.len() - 1] / 1000
    );
}

#[test]
fn whp_cadence_of_guest_time() {
    let dir = common::TempDir::new("cadence");
    let socket = dir.join("v");
    let mut child = common::command()
        .args(["run", "--kernel", common::kernel().to_str().unwrap()])
        .args(["--init", common::test_guest().to_str().unwrap()])
        .args(["--cpus", "1", "--memory", "256", "--no-console", "--vsock"])
        .arg(&socket)
        .arg("--host-managed")
        .arg("--cmdline")
        .arg("console=hvc0 panic=-1 quiet shards_test=spin shards_spin_ms=8000 shards_idle_ms=0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let began = Instant::now();
    loop {
        if socket.exists() && shards_vmm::platform::UnixStream::connect(&socket).is_ok() {
            break;
        }
        assert!(child.try_wait().unwrap().is_none(), "ended before it listened");
        assert!(began.elapsed() < Duration::from_secs(60), "never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    let guest = || {
        let mut s = shards_vmm::platform::UnixStream::connect(&socket).unwrap();
        s.write_all(shards_abi::rate::USE_ASK).unwrap();
        let mut a = Vec::new();
        s.read_to_end(&mut a).unwrap();
        shards_abi::rate::read_used(&a).unwrap().guest.unwrap().total_ns()
    };
    // Past the guest's boot, into its spin.
    std::thread::sleep(Duration::from_millis(1500));
    let (mut asks, mut moves, mut steps) = (Vec::new(), Vec::new(), Vec::new());
    let first = Instant::now();
    let (mut last_g, mut last_move) = (guest(), Instant::now());
    println!("CADENCE first ask took {} us", first.elapsed().as_micros());
    let second = Instant::now();
    let _ = guest();
    println!("CADENCE second ask took {} us", second.elapsed().as_micros());
    let t0 = Instant::now();
    let mut reads = 0u64;
    while t0.elapsed() < Duration::from_secs(2) {
        let asked = Instant::now();
        let g = guest();
        let done = Instant::now();
        reads += 1;
        asks.push(u64::try_from((done - asked).as_nanos()).unwrap());
        if g != last_g {
            moves.push(u64::try_from((done - last_move).as_nanos()).unwrap());
            steps.push(g - last_g);
            (last_g, last_move) = (g, done);
        }
    }
    println!("CADENCE reads {reads} in {} ms", t0.elapsed().as_millis());
    summary("ask latency", &mut asks);
    summary("interval between moves", &mut moves);
    summary("guest ns per move", &mut steps);
    let _ = child.kill();
    let _ = child.wait();
}
