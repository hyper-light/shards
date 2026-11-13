//! What every [`Guests`] does, run against the simulator in this crate's tests and against
//! [`Shards`](crate::Shards) on each backend's real guests (`crates/shards/tests/boot.rs`
//! `shards_api_conforms_on_real_guests`): a spec past the bounds is refused before boot,
//! and one past the hypervisor's vCPUs as `Bounds`; a served port carries bytes both ways
//! and a half-close; an unserved port is refused as `Channel` once its patience is out, never
//! before, one off the allowlist at once; `stop` ends a guest as `Stopped`, and every later
//! connect is refused `Ended`; a guest that powers off ends as `PowerOff`; and the hosts
//! without a backend are refused naming them.
//!
//! Every wait for a guest's answer, a served port's `OK` or a guest's end, follows its VM
//! process ([`Guest::connect_while_runs`], [`Guest::wait_while_runs`]): no clock, and no
//! number of ours (R03). The one patience the suite gives, an unserved port's, is this
//! host's own: what one refused attempt there took.

use std::io::{Read as _, Write as _};
use std::net::Shutdown;
use std::time::Instant;

#[cfg(unix)]
use crate::ProcessOperation;
use crate::{
    Backend, End, Guest, GuestSpec, Guests, Identity, ProcessFailure, Refusal, backend_of, min_memory_mib,
};

/// What the suite runs against.
pub struct Subject<'a> {
    pub guests: &'a dyn Guests,
    /// A guest that serves `port`, echoing what it reads until the host half-closes, then
    /// half-closing itself; that serves nothing on `unserved`; and runs until stopped. Its
    /// `ports` allow both.
    pub serving: GuestSpec,
    pub port: u32,
    pub unserved: u32,
    /// A guest that powers off by itself.
    pub powering_off: GuestSpec,
}

impl std::fmt::Debug for Subject<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subject")
            .field("serving", &self.serving)
            .field("port", &self.port)
            .field("unserved", &self.unserved)
            .field("powering_off", &self.powering_off)
            .finish_non_exhaustive()
    }
}

/// A case that failed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub case: &'static str,
    pub why: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.case, self.why)
    }
}

impl std::error::Error for Failure {}

fn fail(case: &'static str, why: impl Into<String>) -> Failure {
    Failure {
        case,
        why: why.into(),
    }
}

/// Every case, in order; on a host without a backend, the ones that boot nothing.
pub fn run(s: &Subject<'_>) -> Result<(), Failure> {
    hosts_without_a_backend_are_named(s)?;
    bounds_are_refused_before_boot(s)?;
    if s.guests.backend().is_err() {
        return Ok(());
    }
    more_vcpus_than_the_hypervisor_runs_are_refused_as_bounds(s)?;
    a_served_port_carries_bytes_and_a_half_close(s)?;
    an_unserved_port_is_refused(s)?;
    a_single_attempt_is_refused_at_once_where_the_guest_does_not_listen(s)?;
    stop_ends_it_and_later_connects_are_refused(s)?;
    a_guest_that_powers_off_ends_as_power_off(s)?;
    a_guest_its_client_lost_is_taken_back_by_attach(s)
}

/// `backend()` is this host's, and macOS x86_64, outside shards' scope, has none, by name.
pub fn hosts_without_a_backend_are_named(s: &Subject<'_>) -> Result<(), Failure> {
    const CASE: &str = "hosts without a backend are named";
    let (os, arch) = ("macos", "x86_64");
    match backend_of(os, arch) {
        Err(Refusal::NoBackend { os: o, arch: a }) if (o, a) == (os, arch) => {}
        other => return Err(fail(CASE, format!("{os} {arch}: {other:?}"))),
    }
    let here: Result<Backend, Refusal> = backend_of(std::env::consts::OS, std::env::consts::ARCH);
    let said = s.guests.backend();
    if said != here {
        return Err(fail(
            CASE,
            format!("backend() is {said:?}; this host's is {here:?}"),
        ));
    }
    Ok(())
}

/// Specs past the bounds are refused as `Bounds`, before anything boots: their kernel is
/// none, which a boot would refuse as `Boot`.
pub fn bounds_are_refused_before_boot(s: &Subject<'_>) -> Result<(), Failure> {
    const CASE: &str = "bounds are refused before boot";
    let none = GuestSpec {
        kernel: "/shards-api/conformance/no-such-kernel".into(),
        ..s.serving.clone()
    };
    let least = min_memory_mib(std::env::consts::ARCH);
    let past = [
        (
            "no vCPU",
            GuestSpec {
                vcpus: 0,
                ..none.clone()
            },
        ),
        (
            "odd memory",
            GuestSpec {
                memory_mib: least.saturating_add(1),
                ..none.clone()
            },
        ),
        (
            "too little memory",
            GuestSpec {
                memory_mib: least.saturating_sub(2),
                ..none.clone()
            },
        ),
    ];
    for (what, spec) in past {
        match s.guests.boot(&spec) {
            Err(Refusal::Bounds { .. }) => {}
            Err(other) => return Err(fail(CASE, format!("{what}: {other}"))),
            Ok(_) => return Err(fail(CASE, format!("{what}: booted"))),
        }
    }
    Ok(())
}

/// More vCPUs than any hypervisor runs are refused as `Bounds` naming the vCPUs: by `boot`,
/// or, where the hypervisor's own limit is read as the VM starts (shards-vmm's
/// `check_vcpus`), by the `connect` that finds the guest gone. An input: u32's most.
pub fn more_vcpus_than_the_hypervisor_runs_are_refused_as_bounds(s: &Subject<'_>) -> Result<(), Failure> {
    const CASE: &str = "more vCPUs than the hypervisor runs are refused as bounds";
    let spec = GuestSpec {
        vcpus: u32::MAX,
        ..s.serving.clone()
    };
    let refused = match s.guests.boot(&spec) {
        Err(refused) => refused,
        Ok(guest) => match guest.connect_while_runs(s.port) {
            Err(refused) => refused,
            Ok(_) => return Err(fail(CASE, "connected")),
        },
    };
    match refused {
        Refusal::Bounds { what: "vcpus", .. } => Ok(()),
        other => Err(fail(CASE, other.to_string())),
    }
}

fn boot(case: &'static str, s: &Subject<'_>, spec: &GuestSpec) -> Result<Box<dyn Guest>, Failure> {
    s.guests.boot(spec).map_err(|e| fail(case, format!("boot: {e}")))
}

/// A served port answers: bytes go both ways, the host's half-close reaches the guest, and
/// the guest's reaches the host.
pub fn a_served_port_carries_bytes_and_a_half_close(s: &Subject<'_>) -> Result<(), Failure> {
    const CASE: &str = "a served port carries bytes and a half-close";
    let guest = boot(CASE, s, &s.serving)?;
    let mut conn = guest
        .connect_while_runs(s.port)
        .map_err(|e| fail(CASE, format!("connect: {e}")))?;
    // Its echo, read to the guest's half-close, waited for as such: no read timeout.
    conn.set_read_timeout(None)
        .map_err(|e| fail(CASE, format!("its read timeout: {e}")))?;
    // An input, sized to exercise a bound: one byte more than shards' device holds of a
    // connection's (`BUF_ALLOC`, 64 KiB, `crates/vmm/src/devices/virtio/vsock/conn.rs:21`),
    // so the echo goes through only as the device's credit updates let it. 251, a prime, so
    // no power-of-two-sized piece repeats another.
    let sent: Vec<u8> = (0..64 * 1024 + 1u32).map(|i| (i % 251) as u8).collect();
    let mut writer = conn
        .try_clone()
        .map_err(|e| fail(CASE, format!("a clone: {e}")))?;
    // Written on a thread of its own while this one reads: the echo writes back as it reads,
    // and neither side's buffers hold all of it.
    let written = std::thread::scope(|scope| {
        let wrote = std::thread::Builder::new()
            .name("conformance-writer".into())
            .spawn_scoped(scope, || {
                writer.write_all(&sent)?;
                writer.shutdown(Shutdown::Write)
            });
        let wrote = match wrote {
            Ok(wrote) => wrote,
            Err(e) => return Err(format!("a writer thread: {e}")),
        };
        let mut got = Vec::with_capacity(sent.len());
        let read = conn.read_to_end(&mut got).map(|_| got);
        match (wrote.join(), read) {
            (Ok(Ok(())), Ok(got)) => Ok(got),
            (Ok(Err(e)), _) => Err(format!("writing: {e}")),
            (Err(_), _) => Err("the writer panicked".into()),
            (_, Err(e)) => Err(format!("reading to the guest's half-close: {e}")),
        }
    });
    match written {
        Ok(got) if got == sent => {}
        Ok(got) => {
            return Err(fail(
                CASE,
                format!(
                    "sent {} bytes, read back {} different ones",
                    sent.len(),
                    got.len()
                ),
            ));
        }
        Err(why) => return Err(fail(CASE, why)),
    }
    let _ = guest.stop();
    Ok(())
}

/// A port off the allowlist is refused as `Channel` at once; one on it that the guest does
/// not serve, as `Channel` once its patience is out, never before. That patience is this
/// host's own: what one refused single attempt to the unserved port took. How long past it
/// the refusal may come has no bound here: a bound on it would be a multiple of some time of
/// this host's, a number of ours (R03).
pub fn an_unserved_port_is_refused(s: &Subject<'_>) -> Result<(), Failure> {
    const CASE: &str = "an unserved port is refused";
    let began = Instant::now();
    let guest = boot(CASE, s, &s.serving)?;
    // Ready first: a boot's time is not the refusal's.
    drop(
        guest
            .connect_while_runs(s.port)
            .map_err(|e| fail(CASE, format!("connect to the served port: {e}")))?,
    );
    // A spec allows its ports alone, so one of the first of them plus one is off it.
    let off = (0..=u32::MAX)
        .take(s.serving.ports.len().saturating_add(1))
        .find(|p| !s.serving.ports.contains(p))
        .ok_or_else(|| fail(CASE, "every port is allowed"))?;
    match guest.connect_while_runs(off) {
        Err(Refusal::Channel { port, .. }) if port == off => {}
        other => {
            return Err(fail(
                CASE,
                format!("port {off}, off the allowlist: {:?}", other.err()),
            ));
        }
    }
    // A single attempt there, bounded by this boot's own time to answer, which holds a dial
    // and more: what it took is the patience the retrying form is asked with.
    let at = Instant::now()
        .checked_add(began.elapsed())
        .ok_or_else(|| fail(CASE, "this boot's time is past a deadline"))?;
    let once = Instant::now();
    match guest.connect_once_before(s.unserved, at) {
        Err(Refusal::Connect { port, .. }) if port == s.unserved => {}
        other => {
            return Err(fail(
                CASE,
                format!("a single attempt to port {}: {:?}", s.unserved, other.err()),
            ));
        }
    }
    let patience = once.elapsed();
    let start = Instant::now();
    let refused = guest.connect(s.unserved, patience);
    let took = start.elapsed();
    match refused {
        Err(Refusal::Channel { port, .. }) if port == s.unserved => {}
        other => return Err(fail(CASE, format!("port {}: {:?}", s.unserved, other.err()))),
    }
    if took < patience {
        return Err(fail(
            CASE,
            format!("refused after {took:?}, before its patience {patience:?}"),
        ));
    }
    let _ = guest.stop();
    Ok(())
}

/// A single attempt (`connect_once_before`) to a port the guest serves connects; one to a port it
/// doesn't serve is refused at once, closed unanswered, never asked again until its deadline.
pub fn a_single_attempt_is_refused_at_once_where_the_guest_does_not_listen(
    s: &Subject<'_>,
) -> Result<(), Failure> {
    const CASE: &str = "a single attempt is refused at once where the guest does not listen";
    let began = Instant::now();
    let guest = boot(CASE, s, &s.serving)?;
    // Ready first: waiting for a boot is the retrying form's.
    drop(
        guest
            .connect_while_runs(s.port)
            .map_err(|e| fail(CASE, format!("connect to the served port: {e}")))?,
    );
    // A deadline as long as this boot took to answer, which holds a dial and more: what a
    // single attempt is refused within, at once, is far inside it.
    let at = Instant::now()
        .checked_add(began.elapsed())
        .ok_or_else(|| fail(CASE, "this boot's time is past a deadline"))?;
    drop(
        guest
            .connect_once_before(s.port, at)
            .map_err(|e| fail(CASE, format!("a single attempt to the served port: {e}")))?,
    );
    match guest.connect_once_before(s.unserved, at) {
        Err(Refusal::Connect {
            port,
            failure: crate::HandshakeError::Closed,
        }) if port == s.unserved => {}
        other => return Err(fail(CASE, format!("port {}: {:?}", s.unserved, other.err()))),
    }
    if Instant::now() >= at {
        return Err(fail(CASE, "refused only at its deadline, not at once"));
    }
    let _ = guest.stop();
    Ok(())
}

/// `stop` ends a guest as `Stopped`, and a `connect` after it is refused `Ended(Stopped)`.
pub fn stop_ends_it_and_later_connects_are_refused(s: &Subject<'_>) -> Result<(), Failure> {
    const CASE: &str = "stop ends it and later connects are refused";
    let guest = boot(CASE, s, &s.serving)?;
    drop(
        guest
            .connect_while_runs(s.port)
            .map_err(|e| fail(CASE, format!("connect: {e}")))?,
    );
    guest.stop().map_err(|e| fail(CASE, format!("stop: {e}")))?;
    match guest.wait_while_runs() {
        Ok(End::Stopped) => {}
        other => return Err(fail(CASE, format!("its end: {other:?}"))),
    }
    match guest.connect_while_runs(s.port) {
        Err(Refusal::Ended(End::Stopped)) => Ok(()),
        other => Err(fail(CASE, format!("connect after stop: {:?}", other.err()))),
    }
}

/// A guest whose client's handle is lost without its drop (a crashed client's: nothing of it ran
/// on) is taken back by `attach` of its identity: the attached guest serves its port and stops as
/// any guest does; once it has ended, attach of it is refused `Ended` or `Gone`, and attach of an
/// identity whose start time differs is refused with its observed start (or simulator `Gone`). On
/// every host: on Windows the guest's process is the `shards` that booted its VM, whose Job ends
/// the VM as it ends (D141), and it outlives its client as on Unix (D215).
pub fn a_guest_its_client_lost_is_taken_back_by_attach(s: &Subject<'_>) -> Result<(), Failure> {
    const CASE: &str = "a guest its client lost is taken back by attach";
    let guest = boot(CASE, s, &s.serving)?;
    let identity = guest
        .identity()
        .map_err(|e| fail(CASE, format!("identity: {e}")))?;
    // The client's crash: its handle goes without its drop, which would end the guest.
    std::mem::forget(guest);
    let other = Identity {
        started: identity.started.wrapping_add(1),
        ..identity.clone()
    };
    match s.guests.attach(&other, &s.serving) {
        Err(Refusal::Gone { pid, .. }) if pid == other.pid => {}
        Err(Refusal::Process {
            pid,
            expected_started,
            failure: ProcessFailure::DifferentStart { observed },
        }) if pid == other.pid && expected_started == other.started && observed == identity.started => {}
        other => {
            return Err(fail(
                CASE,
                format!("attach of a start time that isn't its: {:?}", other.err()),
            ));
        }
    }
    let back = s
        .guests
        .attach(&identity, &s.serving)
        .map_err(|e| fail(CASE, format!("attach: {e}")))?;
    let mut conn = back
        .connect_while_runs(s.port)
        .map_err(|e| fail(CASE, format!("connect after attach: {e}")))?;
    conn.set_read_timeout(None)
        .map_err(|e| fail(CASE, format!("its read timeout: {e}")))?;
    conn.write_all(b"back")
        .and_then(|()| conn.shutdown(Shutdown::Write))
        .map_err(|e| fail(CASE, format!("writing after attach: {e}")))?;
    let mut got = Vec::new();
    conn.read_to_end(&mut got)
        .map_err(|e| fail(CASE, format!("reading after attach: {e}")))?;
    if got != b"back" {
        return Err(fail(CASE, format!("echoed {got:?} after attach")));
    }
    back.stop().map_err(|e| fail(CASE, format!("stop: {e}")))?;
    match back.wait_while_runs() {
        Ok(End::Stopped) => {}
        other => return Err(fail(CASE, format!("its end: {other:?}"))),
    }
    match s.guests.attach(&identity, &s.serving) {
        Err(Refusal::Ended(_) | Refusal::Gone { .. }) => Ok(()),
        Err(Refusal::Process {
            pid,
            expected_started,
            failure: ProcessFailure::Exited,
        }) if pid == identity.pid && expected_started == identity.started => Ok(()),
        Err(Refusal::Process {
            pid,
            expected_started,
            failure: ProcessFailure::DifferentStart { observed },
        }) if pid == identity.pid && expected_started == identity.started && observed != identity.started => {
            Ok(())
        }
        #[cfg(unix)]
        Err(Refusal::Process {
            pid,
            expected_started,
            failure:
                ProcessFailure::Os {
                    operation: ProcessOperation::PidfdOpen | ProcessOperation::RegisterExit,
                    code: libc::ESRCH,
                },
        }) if pid == identity.pid && expected_started == identity.started => Ok(()),
        other => Err(fail(
            CASE,
            format!(
                "attach of the independently stopped/awaited guest: {:?}",
                other.err()
            ),
        )),
    }
}

/// A guest that powers off ends as `PowerOff`.
pub fn a_guest_that_powers_off_ends_as_power_off(s: &Subject<'_>) -> Result<(), Failure> {
    const CASE: &str = "a guest that powers off ends as PowerOff";
    let guest = boot(CASE, s, &s.powering_off)?;
    match guest.wait_while_runs() {
        Ok(End::PowerOff) => Ok(()),
        other => Err(fail(CASE, format!("its end: {other:?}"))),
    }
}
