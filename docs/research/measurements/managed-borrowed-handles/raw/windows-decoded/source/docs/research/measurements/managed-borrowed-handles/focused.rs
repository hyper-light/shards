//! ROOT-only Windows component fixture. No VM, Guest, cross-process socket copy,
//! lifecycle owner, warm-up or handle-count tolerance is represented here.

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;
use std::os::windows::io::{AsRawSocket as _, IntoRawSocket as _, OwnedSocket};
use std::sync::mpsc::{self, Receiver, SyncSender};
use windows_sys::Win32::Foundation::{GetLastError, HANDLE_FLAG_INHERIT};
use windows_sys::Win32::Networking::WinSock::{
    POLLERR, POLLHUP, POLLNVAL, POLLRDNORM, POLLWRNORM, SOCKET, WSAEWOULDBLOCK, WSAGetLastError, WSAPOLLFD,
    WSAPoll, closesocket,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

const PORT: u32 = u32::MAX;
const CHALLENGE: u8 = 0xa5;
const ANSWER_BYTE: u8 = 0x5a;

#[derive(Clone, Copy, Debug)]
enum Mode {
    Success,
    WrongStart,
    Withheld,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::WrongStart => "wrong-start",
            Self::Withheld => "withheld",
        }
    }
}

#[derive(Debug)]
struct Count {
    phase: &'static str,
    iteration: Option<usize>,
    returned: i32,
    value: Option<u32>,
    error: Option<u32>,
}

impl Count {
    fn read(phase: &'static str, iteration: Option<usize>) -> Self {
        let mut value = 0;
        // SAFETY: the current-process pseudo handle is borrowed, never closed;
        // the DWORD output is valid storage. Failure never manufactures a count.
        let returned = unsafe { GetProcessHandleCount(GetCurrentProcess(), &raw mut value) };
        let error = if returned == 0 {
            // SAFETY: snapshot this thread's error immediately after failure.
            Some(unsafe { GetLastError() })
        } else {
            None
        };
        Self {
            phase,
            iteration,
            returned,
            value: (returned != 0).then_some(value),
            error,
        }
    }
}

#[derive(Debug)]
struct Limits {
    repetitions: usize,
    call: Duration,
    cleanup: Duration,
    whole: Instant,
    operations: Instant,
    path: PathBuf,
}

impl Limits {
    fn read(began: Instant) -> Result<Self, String> {
        let number = |name| -> Result<u64, String> {
            let n = std::env::var(name)
                .map_err(|e| format!("{name}: {e}"))?
                .parse::<u64>()
                .map_err(|e| format!("{name}: {e}"))?;
            if n == 0 {
                return Err(format!("{name}: positive caller input required"));
            }
            Ok(n)
        };
        let repetitions =
            usize::try_from(number("SHARDS_BORROWED_HANDLES_REPETITIONS")?).map_err(|e| e.to_string())?;
        let case_ms = number("SHARDS_BORROWED_HANDLES_CASE_MS")?;
        let call_ms = number("SHARDS_BORROWED_HANDLES_CALL_MS")?;
        let cleanup_ms = number("SHARDS_BORROWED_HANDLES_CLEANUP_MS")?;
        // A single finite WSAPoll uses a signed millisecond argument. Reject
        // larger fixture inputs rather than invent a periodic wait interval.
        if case_ms > i32::MAX as u64 || cleanup_ms >= case_ms || call_ms > case_ms - cleanup_ms {
            return Err("fixture budgets exceed the finite WSAPoll/case/cleanup envelope".into());
        }
        let whole = began
            .checked_add(Duration::from_millis(case_ms))
            .ok_or("whole deadline overflow")?;
        let cleanup = Duration::from_millis(cleanup_ms);
        let operations = whole
            .checked_sub(cleanup)
            .ok_or("cleanup reservation underflow")?;
        let path =
            PathBuf::from(std::env::var_os("SHARDS_BORROWED_HANDLES_SOCKET").ok_or("socket input missing")?);
        Ok(Self {
            repetitions,
            call: Duration::from_millis(call_ms),
            cleanup,
            whole,
            operations,
            path,
        })
    }

    fn call_until(&self) -> Result<Instant, String> {
        let at = Instant::now()
            .checked_add(self.call)
            .ok_or("call deadline overflow")?
            .min(self.operations);
        left(at)?;
        Ok(at)
    }

    fn cleanup_until(&self) -> Instant {
        Instant::now()
            .checked_add(self.cleanup)
            .unwrap_or(self.whole)
            .min(self.whole)
    }
}

fn left(until: Instant) -> Result<Duration, String> {
    let remaining = until.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err("original fixture deadline expired".into())
    } else {
        Ok(remaining)
    }
}

fn receive<T>(receiver: &Receiver<T>, until: Instant) -> Result<T, String> {
    receiver
        .recv_timeout(left(until)?)
        .map_err(|e| format!("bounded receipt: {e}"))
}

fn command(sender: &SyncSender<Command>, value: Command) -> Result<(), String> {
    sender
        .try_send(value)
        .map_err(|e| format!("bounded command handoff: {e}"))
}

#[derive(Debug)]
enum Command {
    Step { iteration: usize, until: Instant },
    Release { iteration: usize, until: Instant },
}

#[derive(Debug)]
struct Close {
    returned: i32,
    error: Option<i32>,
}

#[derive(Debug)]
enum Receipt {
    Ready,
    StepReady {
        iteration: usize,
    },
    Request {
        iteration: usize,
    },
    Closed {
        iteration: usize,
        exchange: Result<(), String>,
        close: Close,
        inheritance: Result<u32, i32>,
    },
    NoDial {
        iteration: usize,
        error: io::ErrorKind,
        code: Option<i32>,
    },
}

fn inheritance(socket: SOCKET) -> Result<u32, i32> {
    let mut flags = 0;
    // SAFETY: this is a currently owned socket; GetHandleInformation explicitly
    // accepts sockets. Query only: no inheritance/security setting is changed.
    if unsafe { windows_sys::Win32::Foundation::GetHandleInformation(socket as _, &raw mut flags) } == 0 {
        // SAFETY: immediate native error snapshot after the failed query.
        Err(unsafe { GetLastError() } as i32)
    } else {
        Ok(flags)
    }
}

fn close_accepted(peer: shards_afunix::UnixStream) -> Close {
    // The existing conversion transfers sole ownership without copying/duplicating
    // the socket. All peer I/O has ended before this function is entered.
    let owned: OwnedSocket = peer.into();
    let raw = owned.into_raw_socket();
    // SAFETY: sole owned accepted socket, consumed exactly once. A failure is
    // retained as unknown resource state, never retried/probed or called closed.
    let returned = unsafe { closesocket(raw as SOCKET) };
    let error = if returned != 0 {
        // SAFETY: immediate Winsock error snapshot after the failed close.
        Some(unsafe { WSAGetLastError() })
    } else {
        None
    };
    Close { returned, error }
}

fn ready(socket: SOCKET, write: bool, until: Instant) -> Result<(), String> {
    let millis = i32::try_from(left(until)?.as_nanos().div_ceil(1_000_000)).map_err(|e| e.to_string())?;
    let wanted = if write { POLLWRNORM } else { POLLRDNORM };
    let mut fd = WSAPOLLFD {
        fd: socket,
        events: wanted,
        revents: 0,
    };
    // SAFETY: one currently owned socket, one valid WSAPOLLFD, finite positive
    // caller-derived timeout. This is a readiness wait, not periodic probing.
    let returned = unsafe { WSAPoll(&raw mut fd, 1, millis) };
    if returned < 0 {
        // SAFETY: immediate Winsock error snapshot.
        return Err(format!("WSAPoll returned {returned}, native {}", unsafe {
            WSAGetLastError()
        }));
    }
    left(until)?;
    if returned == 0 || fd.revents & (POLLERR | POLLNVAL) != 0 || fd.revents & (wanted | POLLHUP) == 0 {
        return Err(format!("WSAPoll returned {returned}, revents {}", fd.revents));
    }
    Ok(())
}

fn read_bytes(peer: &mut shards_afunix::UnixStream, bytes: &mut [u8], until: Instant) -> Result<(), String> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        ready(peer.raw(), false, until)?;
        let n = peer.read(remaining).map_err(|e| format!("server read {e:?}"))?;
        left(until)?;
        if n == 0 {
            return Err("server EOF before complete fixed request/byte".into());
        }
        remaining = remaining.get_mut(n..).ok_or("server read length bounds")?;
    }
    Ok(())
}

fn write_bytes(peer: &mut shards_afunix::UnixStream, bytes: &[u8], until: Instant) -> Result<(), String> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        ready(peer.raw(), true, until)?;
        let n = peer.write(remaining).map_err(|e| format!("server write {e:?}"))?;
        left(until)?;
        if n == 0 {
            return Err("server write made no progress".into());
        }
        remaining = remaining.get(n..).ok_or("server write length bounds")?;
    }
    Ok(())
}

fn peer_exchange(
    peer: &mut shards_afunix::UnixStream,
    mode: Mode,
    iteration: usize,
    until: Instant,
    commands: &Receiver<Command>,
    receipts: &SyncSender<Receipt>,
    whole: Instant,
) -> Result<(), String> {
    peer.set_nonblocking(true)
        .map_err(|e| format!("server nonblocking {e:?}"))?;
    let expected = format!("CONNECT {PORT}\n");
    let mut request = vec![0; expected.len()];
    read_bytes(peer, &mut request, until)?;
    if request != expected.as_bytes() {
        return Err(format!("actual request bytes {request:?}"));
    }
    match mode {
        Mode::Success => {
            write_bytes(peer, b"OK 4294967295\n", until)?;
            let mut byte = [0];
            read_bytes(peer, &mut byte, until)?;
            if byte != [CHALLENGE] {
                return Err(format!("actual challenge {byte:?}"));
            }
            write_bytes(peer, &[ANSWER_BYTE], until)?;
        }
        Mode::Withheld => receipts
            .try_send(Receipt::Request { iteration })
            .map_err(|e| format!("request receipt {e}"))?,
        Mode::WrongStart => return Err("wrong-start unexpectedly entered exchange".into()),
    }
    // Success releases after local byte exchange + stream drop; withheld releases
    // only after the actual typed connector refusal. No timer/sleep releases it.
    let Command::Release {
        iteration: released,
        until: cleanup,
    } = receive(commands, whole)?
    else {
        return Err("expected owned-connection release event".into());
    };
    if released != iteration {
        return Err("release iteration mismatch".into());
    }
    ready(peer.raw(), false, cleanup)?;
    let n = peer
        .read(&mut [0])
        .map_err(|e| format!("actual server EOF read {e:?}"))?;
    left(cleanup)?;
    if n != 0 {
        return Err(format!("server observed unexpected post-drop bytes {n}"));
    }
    Ok(())
}

fn server(
    listener: shards_afunix::UnixListener,
    mode: Mode,
    commands: Receiver<Command>,
    receipts: SyncSender<Receipt>,
    whole: Instant,
) -> Result<(), String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("listener nonblocking {e:?}"))?;
    receipts.try_send(Receipt::Ready).map_err(|e| e.to_string())?;
    loop {
        let step = match commands.recv_timeout(left(whole)?) {
            Ok(step) => step,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            Err(error) => return Err(format!("server command deadline {error}")),
        };
        let Command::Step { iteration, until } = step else {
            return Err("unexpected release outside step".into());
        };
        receipts
            .try_send(Receipt::StepReady { iteration })
            .map_err(|e| e.to_string())?;
        if matches!(mode, Mode::WrongStart) {
            // Wait for the caller's actual refusal before the one bounded probe.
            let Command::Release {
                iteration: released, ..
            } = receive(&commands, whole)?
            else {
                return Err("wrong-start probe release missing".into());
            };
            if released != iteration {
                return Err("probe iteration mismatch".into());
            }
            match listener.accept() {
                Err(error) => {
                    let receipt = Receipt::NoDial {
                        iteration,
                        error: error.kind(),
                        code: error.raw_os_error(),
                    };
                    let refused = error.kind() == io::ErrorKind::WouldBlock
                        && error.raw_os_error() == Some(WSAEWOULDBLOCK);
                    receipts.try_send(receipt).map_err(|e| e.to_string())?;
                    if !refused {
                        return Err("nonblocking accept did not observe WouldBlock".into());
                    }
                }
                Ok((peer, _)) => {
                    let flags = inheritance(peer.raw());
                    let close = close_accepted(peer);
                    receipts
                        .try_send(Receipt::Closed {
                            iteration,
                            exchange: Err("actual unexpected dial".into()),
                            close,
                            inheritance: flags,
                        })
                        .map_err(|e| e.to_string())?;
                    return Err("wrong-start dial actually accepted".into());
                }
            }
            continue;
        }
        ready(listener.raw(), false, until)?;
        let (mut peer, _) = listener.accept().map_err(|e| format!("server accept {e:?}"))?;
        let flags = inheritance(peer.raw());
        let exchange = peer_exchange(&mut peer, mode, iteration, until, &commands, &receipts, whole);
        let close = close_accepted(peer);
        let clean = exchange.is_ok() && close.returned == 0;
        receipts
            .try_send(Receipt::Closed {
                iteration,
                exchange,
                close,
                inheritance: flags,
            })
            .map_err(|e| e.to_string())?;
        if !clean {
            return Err("server exchange/close failed; resource state may be unknown".into());
        }
    }
}

#[derive(Debug)]
struct Operation {
    iteration: usize,
    api: &'static str,
    refusal: Option<Refusal>,
    client_flags: Option<Result<u32, i32>>,
    receipt: Option<Receipt>,
}

#[derive(Debug)]
struct Report {
    mode: Mode,
    counts: Vec<Count>,
    operations: Vec<Operation>,
    errors: Vec<String>,
    pid: u32,
    started: Option<u64>,
    listener_flags: Option<Result<u32, i32>>,
    joined: bool,
}

fn client_bytes(stream: &mut dyn Stream, until: Instant) -> Result<(), String> {
    stream
        .set_write_timeout(Some(left(until)?))
        .map_err(|e| format!("client timeout {e:?}"))?;
    stream
        .write_all(&[CHALLENGE])
        .map_err(|e| format!("client challenge {e:?}"))?;
    left(until)?;
    stream
        .set_read_timeout(Some(left(until)?))
        .map_err(|e| format!("client timeout {e:?}"))?;
    let mut byte = [0];
    stream
        .read_exact(&mut byte)
        .map_err(|e| format!("client answer {e:?}"))?;
    left(until)?;
    if byte != [ANSWER_BYTE] {
        return Err(format!("actual client answer {byte:?}"));
    }
    Ok(())
}

fn operations(
    report: &mut Report,
    limits: &Limits,
    identity: &Identity,
    commands: &SyncSender<Command>,
    receipts: &Receiver<Receipt>,
) -> Result<(), String> {
    receive(receipts, limits.operations).and_then(|r| match r {
        Receipt::Ready => Ok(()),
        _ => Err("server readiness mismatch".into()),
    })?;
    report.counts.push(Count::read("setup", None));
    for iteration in 0..limits.repetitions {
        let until = limits.call_until()?;
        command(commands, Command::Step { iteration, until })?;
        if !matches!(receive(receipts, until)?, Receipt::StepReady { iteration: i } if i == iteration) {
            return Err("server step readiness mismatch".into());
        }
        let mut requested = identity.clone();
        if matches!(report.mode, Mode::WrongStart) {
            requested.started = identity
                .started
                .checked_add(1)
                .or_else(|| identity.started.checked_sub(1))
                .ok_or("distinct start unavailable")?;
        }
        let result = connect_borrowed(&requested, PORT, left(until)?);
        let mut operation = Operation {
            iteration,
            api: "unexpected",
            refusal: None,
            client_flags: None,
            receipt: None,
        };
        let expected = match result {
            Ok(mut stream) => {
                operation.api = "connected";
                operation.client_flags = Some(inheritance(stream.handle().as_raw_socket() as SOCKET));
                let bytes = if matches!(report.mode, Mode::Success) {
                    client_bytes(&mut *stream, until)
                } else {
                    Err("unexpected successful connector".into())
                };
                drop(stream);
                bytes
            }
            Err(refusal) => {
                let expected = match (&report.mode, &refusal) {
                    (
                        Mode::WrongStart,
                        Refusal::Process {
                            pid,
                            expected_started,
                            failure: crate::ProcessFailure::DifferentStart { observed },
                        },
                    ) if *pid == requested.pid
                        && *expected_started == requested.started
                        && *observed == identity.started =>
                    {
                        operation.api = "different-start";
                        true
                    }
                    (
                        Mode::Withheld,
                        Refusal::Borrowed {
                            port: PORT,
                            failure:
                                BorrowFailure::Channel(HandshakeError::Expired {
                                    phase: HandshakePhase::Read,
                                }),
                        },
                    ) => {
                        operation.api = "read-expired";
                        true
                    }
                    _ => false,
                };
                operation.refusal = Some(refusal);
                if expected {
                    Ok(())
                } else {
                    Err("actual connector outcome differs from selected mode".into())
                }
            }
        };
        if matches!(report.mode, Mode::Withheld) {
            let observed = receive(receipts, limits.cleanup_until());
            if !matches!(observed, Ok(Receipt::Request { iteration: i }) if i == iteration) {
                report.operations.push(operation);
                return Err(format!("actual accepted-request receipt missing: {observed:?}"));
            }
        }
        let cleanup = limits.cleanup_until();
        command(
            commands,
            Command::Release {
                iteration,
                until: cleanup,
            },
        )?;
        let receipt = receive(receipts, cleanup)?;
        let clean = match &receipt {
            Receipt::Closed {
                iteration: i,
                exchange,
                close,
                ..
            } => *i == iteration && exchange.is_ok() && close.returned == 0,
            Receipt::NoDial {
                iteration: i,
                error,
                code,
            } => *i == iteration && *error == io::ErrorKind::WouldBlock && *code == Some(WSAEWOULDBLOCK),
            _ => false,
        };
        operation.receipt = Some(receipt);
        report.operations.push(operation);
        if !clean {
            return Err("server cleanup unconfirmed; no closed-operation sample or further iteration".into());
        }
        report
            .counts
            .push(Count::read("after-operation", Some(iteration)));
        expected?;
    }
    report.counts.push(Count::read("after-operations", None));
    Ok(())
}

fn hex(value: impl std::fmt::Debug) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    for byte in format!("{value:?}").bytes() {
        write!(text, "{byte:02x}").unwrap();
    }
    text
}

fn emit(report: &Report) {
    let mut stdout = io::stdout().lock();
    // Serialization is after teardown; no stdout/stderr warm-up is excluded from
    // the raw cold/setup/operation snapshots by a hidden preliminary call.
    writeln!(
        stdout,
        "BORROWED_HANDLES {{\"kind\":\"identity\",\"mode\":\"{}\",\"pid\":{},\"started\":{}}}",
        report.mode.name(),
        report.pid,
        report.started.map_or("null".into(), |n| n.to_string())
    )
    .unwrap();
    for count in &report.counts {
        writeln!(stdout,
            "BORROWED_HANDLES {{\"kind\":\"count\",\"phase\":\"{}\",\"iteration\":{},\"returned\":{},\"count\":{},\"error\":{}}}",
            count.phase,
            count.iteration.map_or("null".into(), |n| n.to_string()),
            count.returned,
            count.value.map_or("null".into(), |n| n.to_string()),
            count.error.map_or("null".into(), |n| n.to_string())
        ).unwrap();
    }
    writeln!(
        stdout,
        "BORROWED_HANDLES {{\"kind\":\"listener-flags\",\"raw_debug_hex\":\"{}\"}}",
        hex(report.listener_flags)
    )
    .unwrap();
    for operation in &report.operations {
        let refusal = operation
            .refusal
            .as_ref()
            .map_or("null".into(), |cause| format!("\"{}\"", hex(cause)));
        let client_flags = hex(operation.client_flags);
        let server_flags = match &operation.receipt {
            Some(Receipt::Closed { inheritance, .. }) => hex(inheritance),
            _ => hex(Option::<u32>::None),
        };
        let (receipt, closed, close_returned, close_error, no_dial_error) = match &operation.receipt {
            Some(Receipt::Closed { exchange, close, .. }) => (
                "closed",
                exchange.is_ok() && close.returned == 0,
                Some(close.returned),
                close.error,
                None,
            ),
            Some(Receipt::NoDial { code, .. }) => ("no-dial", false, None, None, *code),
            _ => ("unconfirmed", false, None, None, None),
        };
        writeln!(stdout,
            "BORROWED_HANDLES {{\"kind\":\"operation\",\"iteration\":{},\"api\":\"{}\",\"server_receipt\":\"{}\",\"server_eof_and_close_confirmed\":{},\"close_returned\":{},\"close_error\":{},\"no_dial_error\":{},\"raw_debug_hex\":\"{}\"}}",
            operation.iteration,
            operation.api,
            receipt,
            closed,
            close_returned.map_or("null".into(), |n| n.to_string()),
            close_error.map_or("null".into(), |n| n.to_string()),
            no_dial_error.map_or("null".into(), |n| n.to_string()),
            hex(operation)
        ).unwrap();
        writeln!(stdout,
            "BORROWED_HANDLES {{\"kind\":\"operation-detail\",\"iteration\":{},\"refusal_debug_hex\":{},\"client_flags_debug_hex\":\"{}\",\"server_flags_debug_hex\":\"{}\"}}",
            operation.iteration, refusal, client_flags, server_flags
        ).unwrap();
    }
    for error in &report.errors {
        writeln!(
            stdout,
            "BORROWED_HANDLES {{\"kind\":\"failure\",\"raw_debug_hex\":\"{}\"}}",
            hex(error)
        )
        .unwrap();
    }
    let counts_known = report.counts.iter().all(|c| c.value.is_some());
    let listener_inherit = match report.listener_flags {
        Some(Ok(flags)) => (flags & HANDLE_FLAG_INHERIT != 0).to_string(),
        _ => "null".into(),
    };
    writeln!(
        stdout,
        "BORROWED_HANDLES {{\"kind\":\"listener-inheritance\",\"inheritable\":{listener_inherit}}}"
    )
    .unwrap();
    writeln!(stdout,
        "BORROWED_HANDLES {{\"kind\":\"result\",\"mode\":\"{}\",\"functional_pass\":{},\"server_joined\":{},\"listener_close_rc\":null,\"counts_known\":{},\"handle_stability\":\"raw-point-counts-only\"}}",
        report.mode.name(),
        report.errors.is_empty() && report.joined && counts_known,
        report.joined,
        counts_known
    ).unwrap();
}

fn run(mode: Mode) {
    let began = Instant::now();
    let cold = Count::read("cold", None);
    let mut report = Report {
        mode,
        counts: vec![cold],
        operations: Vec::new(),
        errors: Vec::new(),
        pid: std::process::id(),
        started: None,
        listener_flags: None,
        joined: false,
    };
    let setup = (|| -> Result<_, String> {
        let limits = Limits::read(began)?;
        report
            .operations
            .try_reserve_exact(limits.repetitions)
            .map_err(|e| e.to_string())?;
        report
            .counts
            .try_reserve_exact(
                limits
                    .repetitions
                    .checked_add(3)
                    .ok_or("sample length overflow")?,
            )
            .map_err(|e| e.to_string())?;
        // SAFETY: existing producer borrows the current-process pseudo handle;
        // this does not create an owned handle, warm Held or acquire lifecycle.
        let started = created_handle(unsafe { GetCurrentProcess() })
            .map_err(|e| format!("actual identity producer {e:?}"))?;
        report.started = Some(started);
        let identity = Identity {
            pid: report.pid,
            started,
            vsock: limits.path.clone(),
        };
        let listener = shards_afunix::UnixListener::bind(&limits.path)
            .map_err(|e| format!("actual listener bind {e:?}"))?;
        report.listener_flags = Some(inheritance(listener.raw()));
        let (send, commands) = mpsc::sync_channel(1);
        let (receipts, receive) = mpsc::sync_channel(1);
        let (done, returned) = mpsc::sync_channel(1);
        let whole = limits.whole;
        let worker = std::thread::Builder::new()
            .name("borrowed-handle-muxer".into())
            .spawn(move || {
                // server owns/drops listener before its done receipt. Its Drop's
                // closesocket return is unobserved and never serialized as close0.
                let result = server(listener, mode, commands, receipts, whole);
                let _ = done.try_send(result);
            })
            .map_err(|e| format!("server thread {e:?}"))?;
        Ok((limits, identity, send, receive, returned, worker))
    })();
    match setup {
        Err(error) => report.errors.push(error),
        Ok((limits, identity, send, receipts, returned, worker)) => {
            if let Err(error) = operations(&mut report, &limits, &identity, &send, &receipts) {
                report.errors.push(error);
            }
            drop(send); // cooperative disconnect releases pending command waits.
            match receive(&returned, limits.cleanup_until()) {
                Ok(result) => {
                    if let Err(error) = result {
                        report.errors.push(error);
                    }
                    match worker.join() {
                        Ok(()) => {
                            report.joined = true;
                            report.counts.push(Count::read("post-join", None));
                        }
                        Err(error) => report.errors.push(format!("actual server panic {error:?}")),
                    }
                }
                Err(error) => {
                    report.errors.push(format!("cleanup pending/unknown: {error}"));
                    drop(worker);
                }
            }
        }
    }
    emit(&report);
    assert!(
        report.errors.is_empty() && report.joined && report.counts.iter().all(|c| c.value.is_some()),
        "functional/count-observation failure; raw facts retained, no handle-stability inference"
    );
}

#[test]
#[ignore = "ROOT native Windows repeated-handle component case; finite caller inputs required"]
fn native_success_handles() {
    run(Mode::Success);
}

#[test]
#[ignore = "ROOT native Windows repeated-handle component case; finite caller inputs required"]
fn native_wrong_start_handles() {
    run(Mode::WrongStart);
}

#[test]
#[ignore = "ROOT native Windows repeated-handle component case; finite caller inputs required"]
fn native_withheld_reply_handles() {
    run(Mode::Withheld);
}
