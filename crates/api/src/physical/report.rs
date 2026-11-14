//! One private report, consumed under the original operation's budget: its deadline, or none
//! but its VM process's end, which a take waits on beside the report itself through the
//! process as its kernel holds it ([`Vm`]). The same Shared owner retains its channel through
//! native observation and cancellation.

use std::io;
use std::time::{Duration, Instant};

use super::{GpuReport, PhysicalGpu};
use crate::{GpuProbeError, ProcessFailure, Refusal};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix::Reader;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows::Reader;

#[cfg(windows)]
use crate::client::borrowed_windows::Held;
#[cfg(unix)]
use crate::taken::Held;

#[derive(Debug)]
pub(crate) struct Request {
    pub scope: [u8; 16],
    pub selected: Vec<PhysicalGpu>,
    pub until: crate::Until,
}

impl Request {
    pub fn copy_from(request: crate::GpuReportRequest<'_>) -> Result<Self, Refusal> {
        let mut selected = Vec::new();
        selected
            .try_reserve_exact(request.selected().len())
            .map_err(|_| Refusal::GpuProbe(GpuProbeError::Allocation))?;
        selected.extend_from_slice(request.selected());
        Ok(Self {
            scope: request.scope(),
            selected,
            until: request.until(),
        })
    }
}

/// What a reader's wait for its frame came to: the frame, its writer closed after it; or its
/// VM process's end ([`Vm`]) before the writer closed, or a pipe its watcher let go.
#[derive(Debug)]
pub(super) enum Read<'a> {
    Frame(&'a [u8]),
    Ended,
}

/// The VM process a report's take waits on beside its pipe (D233): its launch's own process,
/// held by its pid and start ([`Identity`](crate::Identity)) as the launch returns, so that
/// the kernel says its end, whatever the guest's native receipt and watcher do: a pidfd
/// (Linux), a kqueue with its exit registered (macOS), a process handle (Windows).
#[derive(Debug)]
pub(crate) enum Vm {
    Held(Held),
    /// It had ended before it was held: no process has its pid (Unix), or the one that has
    /// it now is another (its start another's), or it is held exited.
    Ended,
}

impl Vm {
    /// Process `pid`, started at `started`, held; or its end, where it has none.
    pub fn of(pid: u32, started: u64) -> Result<Self, ProcessFailure> {
        match Held::find(pid, started) {
            Ok(held) => Ok(Self::Held(held)),
            Err(ProcessFailure::Exited | ProcessFailure::DifferentStart { .. }) => Ok(Self::Ended),
            // pidfd_open(2) and kevent(2)'s EVFILT_PROC refuse a pid no process has, ESRCH:
            // one that ended and was reaped since its launch named it.
            #[cfg(unix)]
            Err(ProcessFailure::Os {
                operation: crate::ProcessOperation::PidfdOpen | crate::ProcessOperation::RegisterExit,
                code: libc::ESRCH,
            }) => Ok(Self::Ended),
            Err(failure) => Err(failure),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Transfer {
    Available,
    Taken,
}

/// Where `take` rechecks the original deadline after its own work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recheck {
    Frame,
    Parse,
}

/// The clock of `take`'s rechecks. A test makes one recheck see the deadline
/// pass exactly there; production always reads the monotonic clock.
#[derive(Debug, Default)]
struct Clock {
    #[cfg(test)]
    expire_at: Option<Recheck>,
}

impl Clock {
    fn expired(
        &self,
        deadline: Option<Instant>,
        #[cfg_attr(
            not(test),
            expect(unused_variables, reason = "only a test chooses the recheck that expires")
        )]
        recheck: Recheck,
    ) -> bool {
        #[cfg(test)]
        if self.expire_at == Some(recheck) {
            return true;
        }
        deadline.is_some_and(|deadline| Instant::now() >= deadline)
    }
}

/// A report's frame, `bytes` as its VM process wrote them, read against the scope and the
/// selection its client named (D232, D233): a managed launch's from its private pipe
/// ([`Receiver::take`]), an image run's as its run host relayed them
/// (`ImageGuest::take_gpu_report`). More bytes than the selection's report holds are `Count`,
/// before any is parsed; text that is not UTF-8 `Syntax`; none at all `Unreported`; a refusal's
/// record the refusal its renderer typed.
pub(crate) fn read(
    bytes: &[u8],
    scope: [u8; 16],
    selected: &[PhysicalGpu],
) -> Result<GpuReport, GpuProbeError> {
    if bytes.len() > super::report_bytes(selected.len())? {
        return Err(GpuProbeError::Count);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| GpuProbeError::Syntax)?;
    GpuReport::parse(text, scope, selected)
}

/// A report's pipe, made before its VM process is launched with its writer: a [`Receiver`]
/// once it follows that process ([`follow`](Opened::follow)).
#[derive(Debug)]
pub(crate) struct Opened {
    request: Request,
    reader: Reader,
}

impl Opened {
    /// The report's pipe, and its writer for the VM process.
    pub fn new(request: Request) -> Result<(Self, io::PipeWriter), Refusal> {
        if request.scope == [0; 16] {
            return Err(Refusal::GpuProbe(GpuProbeError::Scope));
        }
        super::check_selection(&request.selected).map_err(Refusal::Gpu)?;
        if request.selected.is_empty() {
            return Err(Refusal::GpuProbe(GpuProbeError::Empty));
        }
        if request
            .until
            .instant()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(pending());
        }
        // The extra byte distinguishes an overlong frame before parsing or
        // allocating any reported device/name storage.
        let bytes = super::report_bytes(request.selected.len())
            .map_err(Refusal::GpuProbe)?
            .checked_add(1)
            .ok_or(Refusal::GpuProbe(GpuProbeError::Count))?;
        let (reader, writer) = Reader::new(bytes)?;
        Ok((Self { request, reader }, writer))
    }

    pub fn follow(self, vm: Vm) -> Receiver {
        Receiver {
            request: self.request,
            transfer: Transfer::Available,
            reader: self.reader,
            vm,
            clock: Clock::default(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Receiver {
    request: Request,
    transfer: Transfer,
    reader: Reader,
    vm: Vm,
    clock: Clock,
}

pub(super) fn io_error(operation: &'static str, error: io::Error) -> Refusal {
    Refusal::ManagedIo {
        operation,
        kind: error.kind(),
        code: error.raw_os_error(),
    }
}

pub(super) fn pending() -> Refusal {
    Refusal::GpuProbe(GpuProbeError::Pending)
}

#[cfg(unix)]
pub(super) fn buffer(bytes: usize) -> Result<Vec<u8>, Refusal> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(bytes)
        .map_err(|_| Refusal::GpuProbe(GpuProbeError::Allocation))?;
    buffer.resize(bytes, 0);
    Ok(buffer)
}

impl Receiver {
    /// The report, once, by the earlier of the create's deadline and `patience` from now
    /// (`Duration::MAX`, or one past the clock's range: none of the caller's). With neither, it
    /// waits until its writer closes or its VM process ends ([`Vm`]): then, the pipe read once
    /// more, `GpuProbe(Ended)`.
    pub fn take(&mut self, patience: Duration) -> Result<GpuReport, Refusal> {
        if matches!(self.transfer, Transfer::Taken) {
            return Err(Refusal::GpuProbe(GpuProbeError::ReportTaken));
        }
        self.transfer = Transfer::Taken;
        let now = Instant::now();
        let deadline = match (now.checked_add(patience), self.request.until.instant()) {
            (Some(caller), Some(create)) => Some(caller.min(create)),
            (caller, create) => caller.or(create),
        };
        if deadline.is_some_and(|deadline| now >= deadline) {
            return Err(pending());
        }
        let exit = match &self.vm {
            Vm::Held(held) => Some(held.exit()),
            Vm::Ended => None,
        };
        let bytes = match self.reader.frame(deadline, exit)? {
            Read::Frame(bytes) => bytes,
            Read::Ended => return Err(Refusal::GpuProbe(GpuProbeError::Ended)),
        };
        if self.clock.expired(deadline, Recheck::Frame) {
            return Err(pending());
        }
        let report = read(bytes, self.request.scope, &self.request.selected);
        if self.clock.expired(deadline, Recheck::Parse) {
            return Err(pending());
        }
        report.map_err(Refusal::GpuProbe)
    }

    /// Called by the existing native observer on success OR observation error.
    /// Completion of this private I/O is never native scope release evidence.
    pub fn finish(&mut self) -> Result<(), Refusal> {
        self.reader.finish()
    }

    pub fn wait_for_completion(&mut self) -> Result<(), Refusal> {
        self.reader.wait_for_completion()
    }

    #[cfg(all(test, windows))]
    pub fn fail_cancel_calls_for_test(&self, calls: usize) {
        self.reader.fail_cancel_calls_for_test(calls);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::physical::{GpuProbe, ResolvedGpu};
    use std::io::Write as _;

    /// The receiver of `request`, its VM process this test process, which outlives its tests:
    /// no take here sees its VM process end.
    fn followed(request: Request) -> (Receiver, io::PipeWriter) {
        let (opened, writer) = Opened::new(request).unwrap();
        let pid = std::process::id();
        let vm = Vm::of(pid, crate::taken::started(pid).unwrap()).unwrap();
        assert!(matches!(vm, Vm::Held(_)));
        (opened.follow(vm), writer)
    }

    fn fixture() -> (Request, GpuReport) {
        let selected = PhysicalGpu::MetalRegistry(7);
        let scope = [3; 16];
        let request = Request {
            scope,
            selected: vec![selected],
            until: crate::Until::At(Instant::now().checked_add(Duration::from_secs(1)).unwrap()),
        };
        let report = GpuReport {
            scope,
            probe: GpuProbe {
                devices: vec![ResolvedGpu {
                    physical: selected,
                    uuid: [4; 16],
                    pci: None,
                    luid: Some(7u64.to_be_bytes()),
                    name: None,
                    device_local_bytes: None,
                }],
                host_visible_bytes: 1,
            },
        };
        (request, report)
    }

    /// D232: only the private writer's complete scoped frame transfers;
    /// console framing is unrelated and a second consumer gets no second report.
    #[test]
    fn private_report_transfers_exactly_once_after_writer_eof() {
        let (request, expected) = fixture();
        let text = expected.encode().unwrap();
        let (mut receiver, mut writer) = followed(request);
        writer.write_all(text.as_bytes()).unwrap();
        drop(writer);
        assert_eq!(receiver.take(Duration::from_secs(1)).unwrap(), expected);
        assert!(matches!(
            receiver.take(Duration::ZERO),
            Err(Refusal::GpuProbe(GpuProbeError::ReportTaken))
        ));
        receiver.finish().unwrap();
    }

    /// D232, PM M533: reports of one scope at once each transfer their own frame, one
    /// beside an earlier report's writer too, which a VM process can hold past its
    /// client's receiver. A scope names a frame, not its channel: Windows' pipe once took
    /// the scope's name, its one instance still live, and refused these ERROR_PIPE_BUSY.
    #[test]
    fn reports_of_one_scope_at_once_each_transfer_their_own_frame() {
        let (request, expected) = fixture();
        let (earlier, lingering) = followed(request);
        drop(earlier);
        let mut reports = Vec::new();
        for _ in 0..2 {
            let (receiver, mut writer) = followed(fixture().0);
            writer.write_all(expected.encode().unwrap().as_bytes()).unwrap();
            drop(writer);
            reports.push(receiver);
        }
        for receiver in &mut reports {
            assert_eq!(receiver.take(Duration::from_secs(1)).unwrap(), expected);
            receiver.finish().unwrap();
        }
        drop(lingering);
    }

    /// D232: an empty/malformed/oversized private stream is uncertainty,
    /// and never a successful renderer observation or release receipt.
    #[test]
    fn private_report_refuses_missing_torn_and_over_bound_bytes() {
        for bytes in [
            Vec::new(),
            b"shards-vmm-gpu 1 torn\n".to_vec(),
            vec![b'x'; super::super::report_bytes(1).unwrap() + 1],
        ] {
            let (request, _) = fixture();
            let (mut receiver, mut writer) = followed(request);
            writer.write_all(&bytes).unwrap();
            drop(writer);
            assert!(receiver.take(Duration::from_secs(1)).is_err());
            receiver.finish().unwrap();
        }
    }

    /// D232: a selection its VM process refused reaches the client as that refusal, where it
    /// read as a malformed receipt; and a report its writer closed unwritten as no report.
    #[test]
    fn a_refused_or_unwritten_report_is_said_as_such() {
        use crate::physical::{GpuRefusal, JoinError};
        let (request, _) = fixture();
        let (mut receiver, mut writer) = followed(request);
        let frame = format!(
            "shards-vmm-gpu 1 {}\nshards-gpu-refusal 1\njoin missing\n",
            "03".repeat(16)
        );
        writer.write_all(frame.as_bytes()).unwrap();
        drop(writer);
        assert_eq!(
            receiver.take(Duration::from_secs(1)),
            Err(Refusal::GpuProbe(GpuProbeError::Refused(GpuRefusal::Join(
                JoinError::Missing
            ))))
        );
        receiver.finish().unwrap();
        let (request, _) = fixture();
        let (mut receiver, writer) = followed(request);
        drop(writer);
        assert_eq!(
            receiver.take(Duration::from_secs(1)),
            Err(Refusal::GpuProbe(GpuProbeError::Unreported))
        );
        receiver.finish().unwrap();
    }

    /// Write one complete frame and take it, the deadline seen to pass at `late`
    /// alone. The writer's EOF precedes the take, so the read itself never waits.
    fn take_expiring_at(late: Option<Recheck>) -> Result<GpuReport, Refusal> {
        let (request, expected) = fixture();
        let (mut receiver, mut writer) = followed(request);
        receiver.clock.expire_at = late;
        writer.write_all(expected.encode().unwrap().as_bytes()).unwrap();
        drop(writer);
        let taken = receiver.take(Duration::from_secs(1));
        receiver.finish().unwrap();
        taken
    }

    /// D232: take a complete frame whose read ends as the original deadline
    /// passes; expect Pending, never the frame.
    #[test]
    fn private_report_frame_read_ending_at_the_deadline_is_pending() {
        assert!(matches!(
            take_expiring_at(Some(Recheck::Frame)),
            Err(Refusal::GpuProbe(GpuProbeError::Pending))
        ));
    }

    /// D232: take a valid frame whose parse ends as the original deadline
    /// passes; expect Pending, never the parsed report.
    #[test]
    fn private_report_parse_ending_at_the_deadline_is_pending() {
        assert!(matches!(
            take_expiring_at(Some(Recheck::Parse)),
            Err(Refusal::GpuProbe(GpuProbeError::Pending))
        ));
    }

    /// D232: take the same frame with neither recheck expired; expect the
    /// report, so both Pending results above come from their recheck alone.
    #[test]
    fn private_report_neither_recheck_expired_transfers_the_frame() {
        assert_eq!(take_expiring_at(None).unwrap(), fixture().1);
    }

    /// A zero remaining caller budget cannot replace the original create budget
    /// or wait for a writer that still exists. Cleanup owns cancellation separately.
    #[test]
    fn private_report_expired_caller_budget_does_not_wait_or_retry() {
        let (request, _) = fixture();
        let (mut receiver, writer) = followed(request);
        assert!(matches!(
            receiver.take(Duration::ZERO),
            Err(Refusal::GpuProbe(GpuProbeError::Pending))
        ));
        receiver.finish().unwrap();
        assert!(matches!(
            receiver.take(Duration::from_secs(1)),
            Err(Refusal::GpuProbe(GpuProbeError::ReportTaken))
        ));
        drop(writer);
    }
}
