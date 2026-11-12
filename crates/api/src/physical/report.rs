//! One private report, consumed under the original operation deadline. The same
//! Shared owner retains its channel through native observation and cancellation.

use std::io;
use std::time::{Duration, Instant};

use super::{GpuReport, PhysicalGpu};
use crate::{GpuProbeError, Refusal};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix::Reader;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows::Reader;

#[derive(Debug)]
pub(crate) struct Request {
    pub scope: [u8; 16],
    pub selected: Vec<PhysicalGpu>,
    pub deadline: Instant,
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
            deadline: request.deadline(),
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum Transfer {
    Available,
    Taken,
}

#[derive(Debug)]
pub(crate) struct Receiver {
    request: Request,
    transfer: Transfer,
    reader: Reader,
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
    pub fn new(request: Request) -> Result<(Self, io::PipeWriter), Refusal> {
        if request.scope == [0; 16] {
            return Err(Refusal::GpuProbe(GpuProbeError::Scope));
        }
        super::check_selection(&request.selected).map_err(Refusal::Gpu)?;
        if request.selected.is_empty() {
            return Err(Refusal::GpuProbe(GpuProbeError::Empty));
        }
        if Instant::now() >= request.deadline {
            return Err(pending());
        }
        // The extra byte distinguishes an overlong frame before parsing or
        // allocating any reported device/name storage.
        let bytes = super::report_bytes(request.selected.len())
            .map_err(Refusal::GpuProbe)?
            .checked_add(1)
            .ok_or(Refusal::GpuProbe(GpuProbeError::Count))?;
        let (reader, writer) = Reader::new(bytes, request.scope)?;
        Ok((
            Self {
                request,
                transfer: Transfer::Available,
                reader,
            },
            writer,
        ))
    }

    pub fn take(&mut self, patience: Duration) -> Result<GpuReport, Refusal> {
        if matches!(self.transfer, Transfer::Taken) {
            return Err(Refusal::GpuProbe(GpuProbeError::ReportTaken));
        }
        self.transfer = Transfer::Taken;
        let now = Instant::now();
        let deadline = now
            .checked_add(patience)
            .map_or(self.request.deadline, |caller| caller.min(self.request.deadline));
        if now >= deadline {
            return Err(pending());
        }
        let bytes = self.reader.frame(deadline)?;
        if Instant::now() >= deadline {
            return Err(pending());
        }
        if bytes.len() > super::report_bytes(self.request.selected.len()).map_err(Refusal::GpuProbe)? {
            return Err(Refusal::GpuProbe(GpuProbeError::Count));
        }
        let text = std::str::from_utf8(bytes).map_err(|_| Refusal::GpuProbe(GpuProbeError::Syntax))?;
        let report = GpuReport::parse(text, self.request.scope, &self.request.selected);
        if Instant::now() >= deadline {
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

    fn fixture() -> (Request, GpuReport) {
        let selected = PhysicalGpu::MetalRegistry(7);
        let scope = [3; 16];
        let request = Request {
            scope,
            selected: vec![selected],
            deadline: Instant::now().checked_add(Duration::from_secs(1)).unwrap(),
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
        let (mut receiver, mut writer) = Receiver::new(request).unwrap();
        writer.write_all(text.as_bytes()).unwrap();
        drop(writer);
        assert_eq!(receiver.take(Duration::from_secs(1)).unwrap(), expected);
        assert!(matches!(
            receiver.take(Duration::ZERO),
            Err(Refusal::GpuProbe(GpuProbeError::ReportTaken))
        ));
        receiver.finish().unwrap();
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
            let (mut receiver, mut writer) = Receiver::new(request).unwrap();
            writer.write_all(&bytes).unwrap();
            drop(writer);
            assert!(receiver.take(Duration::from_secs(1)).is_err());
            receiver.finish().unwrap();
        }
    }

    /// A zero remaining caller budget cannot replace the original create budget
    /// or wait for a writer that still exists. Cleanup owns cancellation separately.
    #[test]
    fn private_report_expired_caller_budget_does_not_wait_or_retry() {
        let (request, _) = fixture();
        let (mut receiver, writer) = Receiver::new(request).unwrap();
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
