//! What each vCPU of a machine ran of its guest, read as it runs (`Handle::guest_times`), on
//! this host's hypervisor: machines of 1, 2 and as many vCPUs as the host has CPUs, booted
//! in this process one after another (Hypervisor.framework allows one at once), the test
//! guest spinning on every CPU, then idle, then powering off. Each sample's vCPU times never
//! fall, nor does their machine's total, which keeps those gone as the machine is taken
//! apart; and the total is never more than this process's CPU time read after it, past the
//! steps that clock is read in.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::print_stderr
)]

mod common;

use std::time::{Duration, Instant};

use common::{cannot_run_vms, kernel, kernel_nr_cpus, test_guest};
use shards_vmm::vm::{self, Config, Console, ExitReason, GuestTimes};

/// How long the guest spins every CPU, then idles: long enough for each vCPU's guest time to
/// take many of Linux's 10 ms steps (shards_vmm::platform::ThreadGuestTime), and for its
/// idle to be read many times.
const SPIN_MS: u64 = 2_000;
const IDLE_MS: u64 = 1_000;
/// How often its guest time is read: twenty times a second.
const EVERY: Duration = Duration::from_millis(50);
/// How long a machine may take, past which it is waited on while its guest says something
/// new within `common::DAEMON_QUIET`.
const TIMEOUT: Duration = Duration::from_secs(120);

#[test]
fn each_vcpus_guest_time_only_grows_and_stays_within_its_processs_cpu_time() {
    if cannot_run_vms() {
        return;
    }
    let host = std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX));
    let most = kernel_nr_cpus().min(vm::max_vcpus().unwrap());
    let mut counts: Vec<u32> = [1, 2, host].into_iter().map(|n| n.min(most)).collect();
    counts.sort_unstable();
    counts.dedup();
    let step = cpu_step();
    for vcpus in counts {
        watch(vcpus, step);
    }
}

/// The steps this process's CPU time is read in (`process_usage`): the least it is seen to
/// grow by as this thread spins, twenty growths at most or a second. Linux's /proc says it
/// in clock ticks (proc(5)), Windows' times count by its clock's interval, and macOS's are
/// its threads' own, read to about the nanosecond.
fn cpu_step() -> u64 {
    let me = std::process::id();
    let cpu = || shards_vmm::platform::process_usage(me).unwrap().cpu_ns;
    let until = Instant::now() + Duration::from_secs(1);
    let (mut last, mut least, mut grew) = (cpu(), u64::MAX, 0);
    while grew < 20 && Instant::now() < until {
        let now = cpu();
        if now > last {
            least = least.min(now - last);
            (last, grew) = (now, grew + 1);
        }
    }
    assert!(
        grew > 0,
        "this process's CPU time did not grow in a second of spinning"
    );
    least
}

/// Boots a machine of `vcpus` vCPUs whose guest spins every CPU, then idles, reading its
/// guest time as it goes and once it has ended.
fn watch(vcpus: u32, step: u64) {
    let mut cfg = Config::new(kernel().to_path_buf(), Some(test_guest().to_path_buf()));
    cfg.vcpus = vcpus;
    cfg.memory_mib = 512;
    cfg.cmdline = format!(
        "console=ttyS0 panic=-1 quiet shards_test=spin shards_spin_ms={SPIN_MS} shards_idle_ms={IDLE_MS}"
    );
    cfg.console = Console::Discard;
    // Its whole console kept: this test reads it as it goes (D12).
    cfg.said = usize::MAX;
    let (handle, running) = vm::start(&cfg).unwrap_or_else(|e| panic!("{vcpus} vCPUs: {e}"));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(running.wait());
    });
    let me = std::process::id();
    let cpu = || shards_vmm::platform::process_usage(me).unwrap().cpu_ns;
    // Each vCPU thread's guest time may be read up to a step ahead of what this process's
    // CPU time says of it, and that time is read a step behind at most.
    let slack = (u64::from(vcpus) + 1).saturating_mul(step);
    let deadline = Instant::now() + TIMEOUT;
    let mut heard = common::Heard::console();
    let mut console = handle.console_said();
    let (mut last, mut spun, mut samples) = (None::<GuestTimes>, None::<GuestTimes>, 0u32);
    let ended = loop {
        let t = handle.guest_times();
        let used = cpu();
        check(vcpus, &t, last.as_ref(), used, slack);
        samples += 1;
        let now = handle.console_said();
        heard.said(now != console);
        console = now;
        if spun.is_none() && String::from_utf8_lossy(&console.lines).contains("SHARDS-SPIN SPUN") {
            spun = Some(t.clone());
        }
        last = Some(t);
        match rx.recv_timeout(EVERY) {
            Ok(reason) => break reason,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => assert!(
                Instant::now() < deadline || heard.lately(true),
                "{vcpus} vCPUs: no end in {TIMEOUT:?}, nor anything new said in {:?}: {}",
                common::DAEMON_QUIET,
                String::from_utf8_lossy(&console.lines)
            ),
            Err(e) => panic!("{vcpus} vCPUs: {e}"),
        }
    };
    let said = String::from_utf8_lossy(&handle.console_said().lines).into_owned();
    assert_eq!(ended, ExitReason::PowerOff, "{vcpus} vCPUs: {said}");
    assert!(said.contains("SHARDS-TEST PASS"), "{vcpus} vCPUs: {said}");
    // A line of its own, whatever ends it ("\n", or "\r\n" where a console adds the return):
    // on WHP the line was there and a match on "\n" failed (jupiter, 2026-11-12).
    let spun_line = format!("SHARDS-SPIN SPUN {vcpus}");
    assert!(
        said.lines().any(|l| l == spun_line),
        "{vcpus} vCPUs, each online in its guest: {said}"
    );
    // Every vCPU there, by its index, as its guest idled after its spin, each having run it.
    let spun = spun.unwrap_or_else(|| panic!("{vcpus} vCPUs: no sample after its spin: {said}"));
    let indexes: Vec<u32> = spun.vcpus.iter().map(|v| v.index).collect();
    assert_eq!(indexes, (0..vcpus).collect::<Vec<_>>(), "{spun:?}");
    for v in &spun.vcpus {
        assert!(
            v.guest_ns > 0,
            "vCPU {} ran none of its guest's spin: {spun:?}",
            v.index
        );
    }
    // Its machine gone: every vCPU's time kept as its machine's.
    let last = last.unwrap_or_default();
    let end = handle.guest_times();
    let used = cpu();
    assert!(end.vcpus.is_empty(), "{vcpus} vCPUs, its machine gone: {end:?}");
    assert!(end.departed_ns >= last.total_ns(), "{last:?} then {end:?}");
    assert!(
        end.total_ns() <= used.saturating_add(slack),
        "{vcpus} vCPUs: {} ns in their guest, {used} ns of this process's CPU, read in {step} ns steps",
        end.total_ns()
    );
    eprintln!(
        "{vcpus} vCPUs: {samples} samples; {} ns in their guest of this process's {used} ns of CPU \
         (read in {step} ns steps); each vCPU's at its spin's end (ns): {:?}",
        end.total_ns(),
        spun.vcpus.iter().map(|v| v.guest_ns).collect::<Vec<_>>(),
    );
}

/// A sample `t` of a machine of `vcpus` vCPUs, after `before`, read before this process's
/// CPU time was `used`: its vCPUs by index, each read, none less than it was nor the total;
/// and its total within `used` and `slack`. vCPUs leave as their machine ends.
fn check(vcpus: u32, t: &GuestTimes, before: Option<&GuestTimes>, used: u64, slack: u64) {
    let indexes: Vec<u32> = t.vcpus.iter().map(|v| v.index).collect();
    assert!(
        indexes.windows(2).all(|w| w[0] < w[1]) && indexes.iter().all(|&i| i < vcpus),
        "{vcpus} vCPUs: {t:?}"
    );
    for v in &t.vcpus {
        assert_eq!(v.unread, None, "vCPU {} unread: {t:?}", v.index);
    }
    if let Some(before) = before {
        for v in &t.vcpus {
            if let Some(b) = before.vcpus.iter().find(|b| b.index == v.index) {
                assert!(
                    v.guest_ns >= b.guest_ns,
                    "vCPU {}'s time fell: {before:?} then {t:?}",
                    v.index
                );
            }
        }
        assert!(
            t.total_ns() >= before.total_ns(),
            "the total fell: {before:?} then {t:?}"
        );
        assert!(t.departed_ns >= before.departed_ns, "{before:?} then {t:?}");
    }
    assert!(
        t.total_ns() <= used.saturating_add(slack),
        "{vcpus} vCPUs: {} ns in their guest, {used} ns of this process's CPU after, {slack} ns of steps: {t:?}",
        t.total_ns()
    );
}
