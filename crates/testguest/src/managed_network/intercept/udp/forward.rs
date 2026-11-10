//! Finite one-flow payload custody comparison, not a production relay or budget.
use super::*;

pub(in super::super) struct Slots {
    request: Vec<u8>,
    reply: Vec<u8>,
    provided: usize,
}
#[derive(Debug)]
pub(in super::super) enum ReserveError {
    Limit { provided: usize, required: usize },
    Overflow,
    Io(io::Error),
}
impl ReserveError {
    pub(in super::super) fn into_io(self) -> io::Error {
        let mut out = io::stdout().lock();
        match &self {
            Self::Limit { provided, required } => {
                let _ = writeln!(
                    out,
                    "managed-sock-addr {{\"type\":\"PayloadRefusal\",\"cause\":\"Limit\",\"provided\":{provided},\"required\":{required}}}"
                );
            }
            Self::Overflow => {
                let _ = writeln!(
                    out,
                    "managed-sock-addr {{\"type\":\"PayloadRefusal\",\"cause\":\"Overflow\"}}"
                );
            }
            Self::Io(error) => {
                let _ = writeln!(
                    out,
                    "managed-sock-addr {{\"type\":\"PayloadRefusal\",\"cause\":\"Allocation\",\"kind\":\"{:?}\"}}",
                    error.kind()
                );
            }
        }
        match self {
            Self::Io(error) => error,
            _ => invalid("retained payload slots exceed caller envelope"),
        }
    }
}
impl Slots {
    pub(in super::super) fn reserve(provided: usize) -> Result<Self, ReserveError> {
        let required = SENT
            .len()
            .checked_add(Reply::Backend.bytes().len())
            .ok_or(ReserveError::Overflow)?;
        if required > provided {
            return Err(ReserveError::Limit { provided, required });
        }
        let request = buffer(SENT.len()).map_err(ReserveError::Io)?;
        let reply = buffer(Reply::Backend.bytes().len()).map_err(ReserveError::Io)?;
        let usable = request
            .capacity()
            .checked_add(reply.capacity())
            .ok_or(ReserveError::Overflow)?;
        if usable > provided {
            return Err(ReserveError::Limit {
                provided,
                required: usable,
            });
        }
        Ok(Self {
            request,
            reply,
            provided,
        })
    }
    pub(in super::super) fn record(&self) -> io::Result<()> {
        let mut out = io::stdout().lock();
        writeln!(
            out,
            "managed-sock-addr {{\"type\":\"PayloadSlots\",\"provided\":{},\"request_len\":{},\"reply_len\":{},\"request_capacity\":{},\"reply_capacity\":{},\"charge_scope\":\"PayloadCapacityOnly\",\"allocator_kernel_console\":\"Unaccounted\"}}",
            self.provided,
            self.request.len(),
            self.reply.len(),
            self.request.capacity(),
            self.reply.capacity()
        )?;
        out.flush()
    }
}
pub(in super::super) struct Inputs {
    pub(in super::super) ingress: UdpSocket,
    pub(in super::super) upstream: UdpSocket,
    pub(in super::super) backend: SocketAddr,
    pub(in super::super) slots: Slots,
    pub(in super::super) control: Control,
}
pub(in super::super) enum Control {
    Ordinary,
    ReceiveEntry(OwnedFd),
    BeforeUpstreamSend {
        ready: OwnedFd,
        release: OwnedFd,
        notice: BoundaryNotice,
    },
}
#[derive(Clone, Copy, Debug)]
enum Stage {
    IngressReceive,
    RecordIngress,
    ValidateIngress,
    BeforeUpstreamSend,
    UpstreamSend,
    UpstreamReceive,
    RecordReply,
    ValidateReply,
    ClientSend,
    ExchangeComplete,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientPeer {
    Unbound,
    Bound(SocketAddr),
}
impl ClientPeer {
    fn observe(&mut self, peer: SocketAddr) -> Result<(), Fault> {
        match *self {
            Self::Unbound => {
                *self = Self::Bound(peer);
                Ok(())
            }
            Self::Bound(expected) if expected == peer => Ok(()),
            _ => Err(Fault::Tuple),
        }
    }
}
#[derive(Clone, Copy, Debug)]
enum UpstreamProgress {
    NotAttempted,
    Attempted,
    KernelAccepted,
    ReplyValidated,
}
#[derive(Default)]
pub(in super::super) struct Report {
    pub(in super::super) ingress: usize,
    pub(in super::super) upstream_sent: usize,
    pub(in super::super) upstream_received: usize,
    pub(in super::super) client_sent: usize,
    pub(in super::super) empty_ingress: usize,
    pub(in super::super) empty_reply: usize,
}
pub(in super::super) struct Completion {
    pub(in super::super) report: Report,
    step: Exchange,
    stage: Stage,
    progress: UpstreamProgress,
    pub(in super::super) result: Result<Terminal, Fault>,
}
impl Completion {
    pub(in super::super) fn expired_before_upstream_send(&self) -> bool {
        matches!(&self.result, Err(Fault::Io(error)) if error.kind() == io::ErrorKind::TimedOut)
            && matches!(self.step, Exchange::Binary)
            && matches!(self.stage, Stage::BeforeUpstreamSend)
            && matches!(self.progress, UpstreamProgress::NotAttempted)
            && self.report.ingress == 1
            && self.report.upstream_sent == 0
            && self.report.upstream_received == 0
            && self.report.client_sent == 0
            && self.report.empty_ingress == 0
            && self.report.empty_reply == 0
    }
    pub(in super::super) fn cancelled_before_upstream_send(&self) -> bool {
        matches!(self.result, Ok(Terminal::Cancelled))
            && matches!(self.step, Exchange::Binary)
            && matches!(self.stage, Stage::BeforeUpstreamSend | Stage::UpstreamSend)
            && matches!(self.progress, UpstreamProgress::NotAttempted)
            && self.report.ingress == 1
            && self.report.upstream_sent == 0
            && self.report.upstream_received == 0
            && self.report.client_sent == 0
            && self.report.empty_ingress == 0
            && self.report.empty_reply == 0
    }
    pub(in super::super) fn cancelled_upstream_receive(&self) -> bool {
        matches!(self.result, Ok(Terminal::Cancelled))
            && matches!(self.step, Exchange::Binary)
            && matches!(self.stage, Stage::UpstreamReceive)
            && matches!(self.progress, UpstreamProgress::KernelAccepted)
            && self.report.ingress == 1
            && self.report.upstream_sent == 1
            && self.report.upstream_received == 0
            && self.report.client_sent == 0
            && self.report.empty_ingress == 0
            && self.report.empty_reply == 0
    }
}
fn increment(value: &mut usize) -> Result<(), Fault> {
    *value = value.checked_add(1).ok_or(Fault::CountOverflow)?;
    Ok(())
}
fn validate_received(
    received: UdpReceived,
    bytes: &[u8],
    expected: &[u8],
    local: SocketAddr,
    peer: Option<SocketAddr>,
) -> Result<UdpMetadata, Fault> {
    if received.reported != expected.len()
        || received.flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
        || bytes.get(..received.copied) != Some(expected)
    {
        return Err(Fault::Payload);
    }
    let metadata = received.metadata.map_err(Fault::Metadata)?;
    if metadata.original != Some(local) || peer.is_some_and(|peer| peer != metadata.peer) {
        return Err(Fault::Tuple);
    }
    Ok(metadata)
}
pub(super) fn run(mut input: Inputs, deadline: &Deadline, cancel: &OwnedFd) -> Completion {
    let mut report = Report::default();
    let mut step = Exchange::Binary;
    let mut stage = Stage::IngressReceive;
    let mut peer = ClientPeer::Unbound;
    let mut progress = UpstreamProgress::NotAttempted;
    let result = (|| {
        let local = input.ingress.local_addr().map_err(Fault::Io)?;
        let upstream_local = input.upstream.local_addr().map_err(Fault::Io)?;
        for exchange in Exchange::SCRIPT {
            step = exchange;
            progress = UpstreamProgress::NotAttempted;
            stage = Stage::IngressReceive;
            let received =
                udp_arrival_into(&input.ingress, &mut input.slots.request, deadline).map_err(Fault::Io)?;
            increment(&mut report.ingress)?;
            if received.reported == 0 {
                increment(&mut report.empty_ingress)?;
            }
            stage = Stage::RecordIngress;
            arrival_record(
                "ForwardIngress",
                step,
                report.ingress,
                local,
                received.view(&input.slots.request).map_err(Fault::Io)?,
            )
            .map_err(Fault::Io)?;
            let length = received.copied;
            stage = Stage::ValidateIngress;
            let metadata = validate_received(received, &input.slots.request, step.request(), local, None)?;
            peer.observe(metadata.peer)?;
            if let Control::BeforeUpstreamSend {
                ready,
                release,
                notice,
            } = &input.control
            {
                stage = Stage::BeforeUpstreamSend;
                let mut out = io::stdout().lock();
                writeln!(out, "managed-intercept {{\"type\":\"IngressValidated\",\"stage\":\"{stage:?}\",\"step\":\"{step:?}\",\"upstream_progress\":\"{progress:?}\",\"ingress_received\":{}}}", report.ingress).map_err(Fault::Io)?;
                out.flush().map_err(Fault::Io)?;
                drop(out);
                notify_boundary(ready, *notice, deadline).map_err(Fault::Io)?;
                // This trusted barrier retains the original deadline. Ordinary
                // pre-send control delivers pending cancel to the real send;
                // negative notices leave release unwritten until original expiry.
                let barrier_deadline = Deadline {
                    at: deadline.at,
                    cancel: None,
                };
                notified(release, &barrier_deadline).map_err(Fault::Io)?;
            }
            let request = input.slots.request.get(..length).ok_or(Fault::Payload)?;
            stage = Stage::UpstreamSend;
            let sent = deadline
                .io(input.upstream.as_raw_fd(), libc::POLLOUT, || {
                    progress = UpstreamProgress::Attempted;
                    input.upstream.send(request)
                })
                .map_err(Fault::Io)?;
            if sent != length {
                return Err(Fault::PartialSend);
            }
            progress = UpstreamProgress::KernelAccepted;
            increment(&mut report.upstream_sent)?;
            stage = Stage::UpstreamReceive;
            if let Control::ReceiveEntry(entered) = &input.control {
                let mut out = io::stdout().lock();
                writeln!(out, "managed-intercept {{\"type\":\"ReceiveStageEntered\",\"stage\":\"{stage:?}\",\"step\":\"{step:?}\",\"upstream_progress\":\"{progress:?}\",\"kernel_sleep\":\"Unobserved\"}}")
                    .map_err(Fault::Io)?;
                out.flush().map_err(Fault::Io)?;
                drop(out);
                notify(entered, deadline).map_err(Fault::Io)?;
            }
            let received =
                udp_arrival_into(&input.upstream, &mut input.slots.reply, deadline).map_err(Fault::Io)?;
            increment(&mut report.upstream_received)?;
            if received.reported == 0 {
                increment(&mut report.empty_reply)?;
            }
            stage = Stage::RecordReply;
            arrival_record(
                "ForwardUpstreamReply",
                step,
                report.upstream_received,
                upstream_local,
                received.view(&input.slots.reply).map_err(Fault::Io)?,
            )
            .map_err(Fault::Io)?;
            let length = received.copied;
            stage = Stage::ValidateReply;
            validate_received(
                received,
                &input.slots.reply,
                step.response(Reply::Backend),
                upstream_local,
                Some(input.backend),
            )?;
            progress = UpstreamProgress::ReplyValidated;
            let reply = input.slots.reply.get(..length).ok_or(Fault::Payload)?;
            stage = Stage::ClientSend;
            let sent = deadline
                .io(input.ingress.as_raw_fd(), libc::POLLOUT, || {
                    input.ingress.send_to(reply, metadata.peer)
                })
                .map_err(Fault::Io)?;
            if sent != length {
                return Err(Fault::PartialSend);
            }
            increment(&mut report.client_sent)?;
            stage = Stage::ExchangeComplete;
            let mut out = io::stdout().lock();
            writeln!(out,"managed-intercept {{\"type\":\"ForwardExchange\",\"step\":\"{step:?}\",\"client\":\"{}\",\"ingress\":\"{local}\",\"upstream\":\"{upstream_local}\",\"backend\":\"{}\",\"progress\":\"{progress:?}\",\"source_preservation\":\"{}\",\"stage\":\"{stage:?}\"}}",metadata.peer,input.backend,if upstream_local==metadata.peer {"Same"} else {"Changed"}).map_err(Fault::Io)?;
            out.flush().map_err(Fault::Io)?;
        }
        Ok(Terminal::ScriptComplete)
    })();
    let result = match result {
        Err(Fault::Io(error))
            if error.kind() == io::ErrorKind::ConnectionAborted && cancellation_observed(cancel) =>
        {
            Ok(Terminal::Cancelled)
        }
        other => other,
    };
    Completion {
        report,
        step,
        stage,
        progress,
        result,
    }
}
pub(super) fn record(completed: &Completion) -> io::Result<()> {
    let mut out = io::stdout().lock();
    let terminal = match &completed.result {
        Ok(Terminal::ScriptComplete) => "ScriptComplete",
        Ok(Terminal::Cancelled) => "Cancelled",
        Err(_) => "Error",
    };
    writeln!(
        out,
        "managed-intercept {{\"type\":\"ForwardCompletion\",\"joined\":true,\"step\":\"{:?}\",\"stage\":\"{:?}\",\"upstream_progress\":\"{:?}\",\"ingress_received\":{},\"upstream_sent\":{},\"upstream_received\":{},\"client_sent\":{},\"empty_ingress\":{},\"empty_reply\":{},\"terminal\":\"{terminal}\",\"external_commit\":\"UnverifiedBeyondIndependentBackendReceipts\"}}",
        completed.step,
        completed.stage,
        completed.progress,
        completed.report.ingress,
        completed.report.upstream_sent,
        completed.report.upstream_received,
        completed.report.client_sent,
        completed.report.empty_ingress,
        completed.report.empty_reply
    )?;
    if let Err(fault) = &completed.result {
        write!(
            out,
            "managed-intercept {{\"type\":\"ForwardError\",\"step\":\"{:?}\",\"stage\":\"{:?}\",\"progress\":\"{:?}\",\"cause\":\"{}\",\"kind\":",
            completed.step,
            completed.stage,
            completed.progress,
            fault.tag()
        )?;
        match fault.io() {
            Some(error) => write!(out, "\"{:?}\"", error.kind())?,
            None => write!(out, "null")?,
        }
        write!(out, ",\"errno\":")?;
        match fault.io().and_then(io::Error::raw_os_error) {
            Some(errno) => write!(out, "{errno}")?,
            None => write!(out, "null")?,
        }
        writeln!(out, "}}")?;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn payload_envelope_and_single_peer_are_explicit() {
        let required = SENT.len() + Reply::Backend.bytes().len();
        assert!(matches!(
            Slots::reserve(required - 1),
            Err(ReserveError::Limit { .. })
        ));
        let slots = Slots::reserve(required).unwrap();
        assert!(slots.request.capacity() + slots.reply.capacity() <= required);
        let mut peer = ClientPeer::Unbound;
        let first = "127.0.0.1:41".parse().unwrap();
        peer.observe(first).unwrap();
        peer.observe(first).unwrap();
        assert!(matches!(
            peer.observe("127.0.0.1:42".parse().unwrap()),
            Err(Fault::Tuple)
        ));
    }
    #[test]
    fn before_send_expectation_rejects_any_observed_attempt_or_send() {
        let mut done = Completion {
            report: Report {
                ingress: 1,
                ..Report::default()
            },
            step: Exchange::Binary,
            stage: Stage::UpstreamSend,
            progress: UpstreamProgress::NotAttempted,
            result: Ok(Terminal::Cancelled),
        };
        assert!(done.cancelled_before_upstream_send());
        done.progress = UpstreamProgress::Attempted;
        assert!(!done.cancelled_before_upstream_send());
        done.progress = UpstreamProgress::NotAttempted;
        done.report.upstream_sent = 1;
        assert!(!done.cancelled_before_upstream_send());
        done.report.upstream_sent = 0;
        done.result = Err(Fault::Io(io::ErrorKind::TimedOut.into()));
        assert!(!done.cancelled_before_upstream_send());
        assert_eq!(done.report.ingress, 1);
    }
    #[test]
    fn notification_expiry_keeps_ingress_and_is_not_successful_cancellation() {
        let mut done = Completion {
            report: Report {
                ingress: 1,
                ..Report::default()
            },
            step: Exchange::Binary,
            stage: Stage::BeforeUpstreamSend,
            progress: UpstreamProgress::NotAttempted,
            result: Err(Fault::Io(io::ErrorKind::TimedOut.into())),
        };
        assert!(done.expired_before_upstream_send());
        assert!(!done.cancelled_before_upstream_send());
        done.result = Ok(Terminal::Cancelled);
        assert!(!done.expired_before_upstream_send());
        done.result = Err(Fault::Io(io::ErrorKind::InvalidInput.into()));
        assert!(!done.expired_before_upstream_send());
        done.result = Err(Fault::Io(io::ErrorKind::TimedOut.into()));
        done.progress = UpstreamProgress::Attempted;
        assert!(!done.expired_before_upstream_send());
        done.progress = UpstreamProgress::NotAttempted;
        done.report.upstream_sent = 1;
        assert!(!done.expired_before_upstream_send());
        assert_eq!((done.report.ingress, done.report.upstream_sent), (1, 1));
    }
    #[test]
    fn cancellation_witness_keeps_send_facts_and_rejects_expiry_or_wrong_stage() {
        let mut done = Completion {
            report: Report {
                ingress: 1,
                upstream_sent: 1,
                ..Report::default()
            },
            step: Exchange::Binary,
            stage: Stage::UpstreamReceive,
            progress: UpstreamProgress::KernelAccepted,
            result: Ok(Terminal::Cancelled),
        };
        assert!(done.cancelled_upstream_receive());
        done.result = Err(Fault::Io(io::ErrorKind::TimedOut.into()));
        assert!(!done.cancelled_upstream_receive());
        assert_eq!((done.report.ingress, done.report.upstream_sent), (1, 1));
        done.result = Ok(Terminal::Cancelled);
        done.stage = Stage::IngressReceive;
        assert!(!done.cancelled_upstream_receive());
        done.stage = Stage::UpstreamReceive;
        done.report.upstream_received = 1;
        assert!(!done.cancelled_upstream_receive());
    }
    #[test]
    fn empty_and_truncated_borrowed_messages_stay_distinct() {
        let local = "127.0.0.1:41".parse().unwrap();
        let peer = "127.0.0.1:42".parse().unwrap();
        let message = |reported, flags| UdpReceived {
            reported,
            flags,
            copied: 0,
            metadata: Ok(UdpMetadata {
                peer,
                original: Some(local),
            }),
        };
        assert!(validate_received(message(0, 0), &[], &[], local, Some(peer)).is_ok());
        assert!(matches!(
            validate_received(message(1, libc::MSG_TRUNC), &[], &[], local, Some(peer)),
            Err(Fault::Payload)
        ));
    }
}
