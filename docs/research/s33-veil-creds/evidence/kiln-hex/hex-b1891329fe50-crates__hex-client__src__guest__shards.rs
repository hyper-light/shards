//! The guest's ports through Shards (docs/design/shards-guest.md §2, Appendix A "Hex's side"): a
//! [`Link`] whose first dial is `shards_api::Guest::connect_before` and every later one
//! `connect_once_before`, Firecracker's `CONNECT <port>` on the guest's vsock, and a [`Duplex`] over the
//! stream it returns. Hex never names Shards' stream type: on
//! Windows it is Shards' overlapped AF_UNIX socket, whose timeouts poll first, behind the same trait.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hex_api::{ConnectFailure, ConnectPhase, ConnectReply, IoKind, Refusal};

use super::{Duplex, DuplexOwner, Link, own_duplex};

/// The slowest recorded value of each wait that seeds a bound, by host class
/// (bench/native/guest/records.tsv). A class with no row for a measure seeds from twice the slowest row of
/// any class ([`seed_in`]); adding its own row, once its record is taken, makes that its seed, with no code
/// change.
pub const RECORDS: &str = include_str!("../../../../bench/native/guest/records.tsv");

/// This host's class, as the records name it: the hypervisor Shards boots its guests with here.
pub const CLASS: &str = if cfg!(target_os = "macos") {
    "macOS-HVF"
} else if cfg!(target_os = "windows") {
    "Windows-WHP"
} else {
    "Linux-KVM"
};

/// The slowest value `table` records for `measure` on host class `class`: each line `measure`, `class`,
/// microseconds and its record, tab-separated; `#` lines are comments. `None` where no row names both.
pub fn slowest_in(table: &str, measure: &str, class: &str) -> Option<Duration> {
    table
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| line.split('\t'))
        .find_map(|mut row| {
            (row.next()? == measure && row.next()? == class)
                .then(|| row.next()?.parse().ok().map(Duration::from_micros))?
        })
}

/// The seed `table` gives `measure` on host class `class`: its own slowest row where it has one; else, by the
/// lead's seed rule (2026-11-07), twice the slowest row of any class, a class with no record of its own
/// starting from the slowest the others have seen with room to spare, then rising with its node's own (the
/// learned bounds take the max with what the node sees). `None` where no class records the measure.
pub fn seed_in(table: &str, measure: &str, class: &str) -> Option<Duration> {
    if let Some(own) = slowest_in(table, measure, class) {
        return Some(own);
    }
    table
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let mut row = line.split('\t');
            if row.next()? != measure {
                return None;
            }
            row.next()?;
            row.next()?.parse().ok().map(Duration::from_micros)
        })
        .max()
        .map(|slowest| slowest.saturating_mul(2))
}

/// The seed for `measure` on this host's class ([`seed_in`] over records.tsv), or the refusal naming the
/// record to take where no class records it.
pub fn recorded(measure: &str) -> Result<Duration, Refusal> {
    seed_in(RECORDS, measure, CLASS).ok_or_else(|| Refusal::Platform {
        what: "the oci class's guest",
        platform: std::env::consts::OS,
        why: format!(
            "no {measure} record for any host class yet (bench/native/guest/records.tsv): record one first"
        ),
    })
}

/// The seed of the first dial's bound before this node has seen a boot: the slowest boot to ready on record
/// for this host's class, or with none, twice the slowest of any class (bench/native/guest/boot-ready/
/// RECORD.md, its rows in records.tsv).
pub fn recorded_boot() -> Result<Duration, Refusal> {
    recorded("boot_ready")
}

/// A connection to one of the guest's ports, as Shards gives it.
pub struct ShardsStream(pub(crate) Box<dyn shards_api::Stream>);

impl Read for ShardsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for ShardsStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Duplex for ShardsStream {
    fn try_clone(&self) -> io::Result<DuplexOwner> {
        own_duplex(Self(self.0.try_clone()?))
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.0.shutdown(how)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_write_timeout(timeout)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_read_timeout(timeout)
    }
    fn handle(&self) -> super::Handle<'_> {
        self.0.handle()
    }

    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.0.set_nonblocking(on)
    }
}

/// A guest's ports. Each dial has a finite absolute deadline, the caller's or else `bound` from the
/// dial's start. Until a dial has reached the guest's relay it is one `connect_before`, within which
/// Shards' client asks again while the guest doesn't listen there yet. The relay binds every port it
/// serves before it serves any (`hex guest`), so after that a refused port has no relay behind it:
/// each later dial is one `connect_once_before`, refused at once if nothing listens and by its
/// deadline if nothing answers. `bound` is the caller's learned bound on a boot to ready
/// (`hex_runtime::guest::GuestRuntime::first_dial_bound`), whose ready includes a whole CONNECT/OK
/// exchange. Never a relative patience: Shards' client awaits a zero or `Duration::MAX` patience's
/// answer without a bound (HEX-GUEST-33).
pub struct ShardsLink {
    guest: ShardsGuest,
    bound: Duration,
    listened: AtomicBool,
}

enum ShardsGuest {
    Owned(Arc<dyn shards_api::Guest>),
    Managed(shards_api::ManagedGuest),
}

impl ShardsGuest {
    fn guest(&self) -> &dyn shards_api::Guest {
        match self {
            Self::Owned(guest) => guest.as_ref(),
            Self::Managed(guest) => guest,
        }
    }
}

impl ShardsLink {
    pub fn new(guest: Arc<dyn shards_api::Guest>, bound: Duration) -> Self {
        Self {
            guest: ShardsGuest::Owned(guest),
            bound,
            listened: AtomicBool::new(false),
        }
    }

    /// AC-T7.4.1: retain the API's existing native/protocol receipt directly. The
    /// caller uses ManagedGuest::try_clone when it also retains a boot handle;
    /// adapting that handle needs no second Arc allocation or lifecycle owner.
    pub fn managed(guest: shards_api::ManagedGuest, bound: Duration) -> Self {
        Self {
            guest: ShardsGuest::Managed(guest),
            bound,
            listened: AtomicBool::new(false),
        }
    }
}

impl Link for ShardsLink {
    fn identity(&self) -> Result<shards_api::Identity, Refusal> {
        self.guest
            .guest()
            .identity()
            .map_err(|why| refusal("naming the guest's stream endpoint", &why))
    }

    /// The learned bound on a boot to ready, whose ready includes the CONNECT/OK exchange and the
    /// relay's token after it: a connect with no caller deadline spends it once on both.
    fn setup_bound(&self) -> Option<Duration> {
        Some(self.bound)
    }

    fn dial(&self, port: u32) -> Result<DuplexOwner, Refusal> {
        self.dial_before(port, None)
    }

    fn dial_before(&self, port: u32, deadline: Option<Instant>) -> Result<DuplexOwner, Refusal> {
        let at = match deadline {
            Some(at) => at,
            None => (!self.bound.is_zero())
                .then(|| Instant::now().checked_add(self.bound))
                .flatten()
                .ok_or_else(|| Refusal::Invalid {
                    what: "guest dial bound",
                    why: "it must be positive and fit a monotonic deadline".to_owned(),
                })?,
        };
        let cap = crate::calls::ReplyDeadline::caller_before(at)?;
        cap.remaining("connecting to the guest within the original operation deadline")?;
        let guest = self.guest.guest();
        let stream = if self.listened.load(Ordering::Acquire) {
            guest.connect_once_before(port, at)
        } else {
            guest.connect_before(port, at)
        }
        .map_err(|why| refusal("connecting to the guest", &why))?;
        self.listened.store(true, Ordering::Release);
        let stream = own_duplex(ShardsStream(stream))?;
        cap.remaining("retaining the guest connection within the original operation deadline")?;
        Ok(stream)
    }
}

/// Shards' refusal as Hex's: a missing backend is the platform's, a bound is a spec's, a CONNECT's
/// failure keeps its port, phase and cause (Shards R04), an owned guest's or a borrowed one's; the
/// rest is the guest's channel or its end, which Hex reads as an operating-system failure naming
/// what it did.
pub fn refusal(op: &'static str, why: &shards_api::Refusal) -> Refusal {
    match why {
        shards_api::Refusal::NoBackend { os, arch } => Refusal::Platform {
            what: "the oci class",
            platform: os,
            why: format!(
                "Shards has no backend on {os} {arch} to boot the Linux guest the class runs in"
            ),
        },
        shards_api::Refusal::Bounds { what, why } => Refusal::Invalid {
            what: "the node's guest",
            why: format!("its {what}: {why}"),
        },
        shards_api::Refusal::Gpu(why) => {
            use shards_api::PhysicalGpuError;
            let message = match why {
                PhysicalGpuError::Syntax => "malformed native GPU selector",
                PhysicalGpuError::PciAddress => "GPU PCI address fields are out of range",
                PhysicalGpuError::MetalRegistry => "Metal registry identity is unavailable",
                PhysicalGpuError::Count => "GPU count exceeds the native device address space",
                PhysicalGpuError::Duplicate => "GPU selection repeats a native identity",
                PhysicalGpuError::Allocation => {
                    return Refusal::Exhausted {
                        what: "physical GPU selection storage",
                        bound: shards_api::physical::MAX_PHYSICAL_GPUS,
                    };
                }
            };
            let mut reason = String::new();
            if reason.try_reserve_exact(message.len()).is_err() {
                return Refusal::Exhausted {
                    what: "physical GPU refusal text",
                    bound: message.len(),
                };
            }
            reason.push_str(message);
            Refusal::Invalid {
                what: "physical GPU selection",
                why: reason,
            }
        }
        shards_api::Refusal::Connect { port, failure }
        | shards_api::Refusal::Borrowed {
            port,
            failure: shards_api::BorrowFailure::Channel(failure),
        } => Refusal::Connect {
            port: *port,
            failure: connect_failure(failure),
        },
        other => Refusal::os(op, other),
    }
}

fn connect_failure(failure: &shards_api::HandshakeError) -> ConnectFailure {
    use shards_api::{HandshakeError, ReplyError};
    match failure {
        HandshakeError::Expired { phase } => ConnectFailure::Expired {
            phase: connect_phase(*phase),
        },
        HandshakeError::Io { phase, kind, code } => ConnectFailure::Io {
            phase: connect_phase(*phase),
            kind: IoKind::new(*kind),
            code: *code,
        },
        HandshakeError::Closed => ConnectFailure::Closed,
        HandshakeError::Reply(ReplyError::TooLong) => ConnectFailure::Reply(ConnectReply::TooLong),
        HandshakeError::Reply(ReplyError::NotOk) => ConnectFailure::Reply(ConnectReply::NotOk),
    }
}

fn connect_phase(phase: shards_api::HandshakePhase) -> ConnectPhase {
    match phase {
        shards_api::HandshakePhase::Dial => ConnectPhase::Dial,
        shards_api::HandshakePhase::Write => ConnectPhase::Write,
        shards_api::HandshakePhase::Read => ConnectPhase::Read,
        shards_api::HandshakePhase::ClearTimeouts => ConnectPhase::ClearTimeouts,
    }
}

#[cfg(test)]
mod connect_original_tests;
