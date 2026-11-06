//! What every [`Guests`] does, run against the simulator in this crate's tests and against
//! [`Shards`](crate::Shards) on each backend: a spec past the bounds is refused before boot;
//! a served port carries bytes both ways and a half-close; an unserved port is refused as
//! `Channel` within its patience, one off the allowlist at once; `stop` ends a guest as
//! `Stopped`, and every later `connect` is refused `Ended`; a guest that powers off ends as
//! `PowerOff`; and the hosts without a backend are refused naming them.

use std::io::{Read as _, Write as _};
use std::net::Shutdown;
use std::time::{Duration, Instant};

use crate::{Backend, End, Guest, GuestSpec, Guests, Refusal, backend_of, max_vcpus, min_memory_mib};

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
    /// How long a guest may take to boot and listen, or to end.
    pub patience: Duration,
}

impl std::fmt::Debug for Subject<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subject")
            .field("serving", &self.serving)
            .field("port", &self.port)
            .field("unserved", &self.unserved)
            .field("powering_off", &self.powering_off)
            .field("patience", &self.patience)
            .finish_non_exhaustive()
    }
}

/// A case that failed, and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{case}: {why}")]
pub struct Failure {
    pub case: &'static str,
    pub why: String,
}

fn fail(case: &'static str, why: impl Into<String>) -> Failure {
    Failure { case, why: why.into() }
}

/// Every case, in order; on a host without a backend, the ones that boot nothing.
pub fn run(s: &Subject<'_>) -> Result<(), Failure> {
    hosts_without_a_backend_are_named(s)?;
    bounds_are_refused_before_boot(s)?;
    if s.guests.backend().is_err() {
        return Ok(());
    }
    a_served_port_carries_bytes_and_a_half_close(s)?;
    an_unserved_port_is_refused(s)?;
    stop_ends_it_and_later_connects_are_refused(s)?;
    a_guest_that_powers_off_ends_as_power_off(s)
}

/// `backend()` is this host's, and macOS x86_64 and Windows arm64 have none, by name.
pub fn hosts_without_a_backend_are_named(s: &Subject<'_>) -> Result<(), Failure> {
    const CASE: &str = "hosts without a backend are named";
    for (os, arch) in [("macos", "x86_64"), ("windows", "aarch64")] {
        match backend_of(os, arch) {
            Err(Refusal::NoBackend { os: o, arch: a }) if (o, a) == (os, arch) => {}
            other => return Err(fail(CASE, format!("{os} {arch}: {other:?}"))),
        }
    }
    let here: Result<Backend, Refusal> = backend_of(std::env::consts::OS, std::env::consts::ARCH);
    let said = s.guests.backend();
    if said != here {
        return Err(fail(CASE, format!("backend() is {said:?}; this host's is {here:?}")));
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
        ("no vCPU", GuestSpec { vcpus: 0, ..none.clone() }),
        (
            "more vCPUs than the host's",
            GuestSpec {
                vcpus: max_vcpus().saturating_add(1),
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

fn boot(case: &'static str, s: &Subject<'_>, spec: &GuestSpec) -> Result<Box<dyn Guest>, Failure> {
    s.guests.boot(spec).map_err(|e| fail(case, format!("boot: {e}")))
}

/// A served port answers: bytes go both ways, the host's half-close reaches the guest, and
/// the guest's reaches the host.
pub fn a_served_port_carries_bytes_and_a_half_close(s: &Subject<'_>) -> Result<(), Failure> {
    const CASE: &str = "a served port carries bytes and a half-close";
    let guest = boot(CASE, s, &s.serving)?;
    let mut conn = guest
        .connect(s.port, s.patience)
        .map_err(|e| fail(CASE, format!("connect: {e}")))?;
    conn.set_read_timeout(Some(s.patience))
        .map_err(|e| fail(CASE, format!("its read timeout: {e}")))?;
    let sent: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 251) as u8).collect();
    let mut writer = conn
        .try_clone()
        .map_err(|e| fail(CASE, format!("a clone: {e}")))?;
    // Written on a thread of its own while this one reads: the echo writes back as it reads,
    // and neither side's buffers hold all of it.
    let written = std::thread::scope(|scope| {
        let wrote = scope.spawn(|| {
            writer.write_all(&sent)?;
            writer.shutdown(Shutdown::Write)
        });
        let mut got = Vec::with_capacity(sent.len());
        let read = conn.read_to_end(&mut got).map(|_| got);
        (wrote.join(), read)
    });
    match written {
        (Ok(Ok(())), Ok(got)) if got == sent => {}
        (Ok(Ok(())), Ok(got)) => {
            return Err(fail(
                CASE,
                format!("sent {} bytes, read back {} different ones", sent.len(), got.len()),
            ));
        }
        (Ok(Err(e)), _) => return Err(fail(CASE, format!("writing: {e}"))),
        (Err(_), _) => return Err(fail(CASE, "the writer panicked")),
        (_, Err(e)) => return Err(fail(CASE, format!("reading to the guest's half-close: {e}"))),
    }
    let _ = guest.stop();
    Ok(())
}

/// A port off the allowlist is refused as `Channel` at once; one on it that the guest does
/// not serve, as `Channel` once its patience is out, and not long after.
pub fn an_unserved_port_is_refused(s: &Subject<'_>) -> Result<(), Failure> {
    const CASE: &str = "an unserved port is refused";
    let guest = boot(CASE, s, &s.serving)?;
    // Ready first: a boot's time is not the refusal's.
    drop(
        guest
            .connect(s.port, s.patience)
            .map_err(|e| fail(CASE, format!("connect to the served port: {e}")))?,
    );
    let off = (0..=u32::MAX)
        .take(4096)
        .find(|p| !s.serving.ports.contains(p))
        .ok_or_else(|| fail(CASE, "every port is allowed"))?;
    match guest.connect(off, s.patience) {
        Err(Refusal::Channel { port, .. }) if port == off => {}
        other => return Err(fail(CASE, format!("port {off}, off the allowlist: {:?}", other.err()))),
    }
    let patience = Duration::from_millis(200);
    let start = Instant::now();
    let refused = guest.connect(s.unserved, patience);
    let took = start.elapsed();
    match refused {
        Err(Refusal::Channel { port, .. }) if port == s.unserved => {}
        other => return Err(fail(CASE, format!("port {}: {:?}", s.unserved, other.err()))),
    }
    // A handshake under way when patience runs out ends at it; a second is past any.
    if took < patience || took > patience.saturating_add(Duration::from_secs(1)) {
        return Err(fail(CASE, format!("refused after {took:?}, with patience {patience:?}")));
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
            .connect(s.port, s.patience)
            .map_err(|e| fail(CASE, format!("connect: {e}")))?,
    );
    guest.stop().map_err(|e| fail(CASE, format!("stop: {e}")))?;
    match guest.wait_for(s.patience) {
        Ok(Some(End::Stopped)) => {}
        other => return Err(fail(CASE, format!("its end: {other:?}"))),
    }
    match guest.connect(s.port, s.patience) {
        Err(Refusal::Ended(End::Stopped)) => Ok(()),
        other => Err(fail(CASE, format!("connect after stop: {:?}", other.err()))),
    }
}

/// A guest that powers off ends as `PowerOff`.
pub fn a_guest_that_powers_off_ends_as_power_off(s: &Subject<'_>) -> Result<(), Failure> {
    const CASE: &str = "a guest that powers off ends as PowerOff";
    let guest = boot(CASE, s, &s.powering_off)?;
    match guest.wait_for(s.patience) {
        Ok(Some(End::PowerOff)) => Ok(()),
        other => Err(fail(CASE, format!("its end: {other:?}"))),
    }
}
