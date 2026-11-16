//! Scratch probe (g121 whp lane, never committed): a --host-managed guest's vCPU guest time
//! (USE) against its VM process's CPU time and wall time, while its one vCPU spins and then
//! halts, and the charge its VM process learned (D265).
#![allow(clippy::panic, clippy::unwrap_used)]
mod common;
use std::io::{Read as _, Write as _};
use std::time::{Duration, Instant};

#[test]
fn whp_probe_guest_time_against_process_cpu() {
    let dir = common::TempDir::new("probe");
    let socket = dir.join("v");
    let mut child = common::command()
        .args(["run", "--kernel", common::kernel().to_str().unwrap()])
        .args(["--init", common::test_guest().to_str().unwrap()])
        .args(["--cpus", "1", "--memory", "256", "--no-console", "--vsock"])
        .arg(&socket)
        .arg("--host-managed")
        .arg("--cmdline")
        .arg("console=hvc0 panic=-1 quiet shards_test=spin shards_spin_ms=5000 shards_idle_ms=6000")
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
    let used = || {
        let mut s = shards_vmm::platform::UnixStream::connect(&socket).unwrap();
        s.write_all(shards_abi::rate::USE_ASK).unwrap();
        let mut a = Vec::new();
        s.read_to_end(&mut a).unwrap();
        let u = shards_abi::rate::read_used(&a).unwrap();
        let host = u.host.unwrap();
        (u.guest.unwrap().total_ns(), host.cpu_ns, host.charged)
    };
    let t0 = Instant::now();
    let (mut pw, (mut pg, mut ph, c0)) = (0u64, used());
    println!("PROBE listening after {} ms; guest {} ms, cpu {} ms, charged {c0:?} at t0", began.elapsed().as_millis(), pg / 1_000_000, ph / 1_000_000);
    while t0.elapsed() < Duration::from_secs(12) {
        std::thread::sleep(Duration::from_millis(500));
        if child.try_wait().unwrap().is_some() {
            println!("PROBE the VM ended");
            break;
        }
        let w = u64::try_from(t0.elapsed().as_nanos()).unwrap();
        let (g, h, c) = used();
        let dw = (w - pw).max(1);
        println!(
            "PROBE wall {:>6} ms | guest {:>6} ms, +{:>4} ms = {:>5.1}% of wall | cpu {:>6} ms, +{:>4} ms = {:>5.1}% of wall | charged {c:?}",
            w / 1_000_000, g / 1_000_000, (g - pg) / 1_000_000, (g - pg) as f64 * 100.0 / dw as f64,
            h / 1_000_000, (h.saturating_sub(ph)) / 1_000_000, h.saturating_sub(ph) as f64 * 100.0 / dw as f64
        );
        (pw, pg, ph) = (w, g, h);
    }
    let _ = child.kill();
    let _ = child.wait();
}
