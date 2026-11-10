//! Harness-only exact Hex patch handshake, not production or an adopted patch.
//! Extracted after applying the reference to private temporary source copies.
use super::super::{ANSWER, Attempt, Duration, Instant, Path, Socket, Stream, io, stream};
use std::io::{Read as _, Write as _};
pub(super) fn handshake(
    vsock: &Path,
    port: u32,
    wait: Option<Duration>,
    cost: &mut Duration,
) -> Result<Socket, Attempt> {
    let began = Instant::now();
    // One handshake's budget, including a partial request or answer, rather than a fresh timeout per byte.
    let deadline = wait.and_then(|wait| began.checked_add(wait));
    let left = || match deadline {
        Some(deadline) => {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                Err(Attempt::Again("the handshake's patience elapsed".into()))
            } else {
                Ok(Some(remaining))
            }
        }
        None => Ok(None),
    };
    // Its own work, the connect and the ask, as far as it got: not its wait for the answer.
    let done = |cost: &mut Duration| *cost = began.elapsed();
    let mut s = stream::connect(vsock, left()?).map_err(|e| {
        done(cost);
        match e.kind() {
            // Not yet bound, or bound and not yet listening: the VM is starting. Or its
            // muxer's queue is full (Linux's EAGAIN) or the connect still under way at its
            // wait's end: the VM takes no connection now.
            io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut => Attempt::Again(format!("dialing: {e}")),
            // Windows' full queue, its EAGAIN (PM M205).
            #[cfg(windows)]
            _ if e.raw_os_error() == Some(windows_sys::Win32::Networking::WinSock::WSAENOBUFS) => {
                Attempt::Again(format!("dialing: {e}"))
            }
            _ => Attempt::Refused(format!("{}: {e}", vsock.display())),
        }
    })?;
    let request = format!("CONNECT {port}\n");
    let asked = (|| {
        let mut unsent = request.as_bytes();
        while !unsent.is_empty() {
            Stream::set_write_timeout(&s, left()?)
                .map_err(|e| Attempt::Refused(format!("its write timeout: {e}")))?;
            match s.write(unsent) {
                Ok(0) => return Err(Attempt::Again("closed while asking".into())),
                Ok(n) => unsent = unsent.get(n..).unwrap_or_default(),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Attempt::Again(format!("asking: {e}"))),
            }
        }
        Ok(())
    })();
    done(cost);
    asked?;
    let mut line = Vec::with_capacity(ANSWER);
    let mut byte = [0u8; 1];
    while line.last() != Some(&b'\n') {
        if line.len() >= ANSWER {
            return Err(Attempt::Refused(format!(
                "answered {:?}, no line",
                String::from_utf8_lossy(&line)
            )));
        }
        Stream::set_read_timeout(&s, left()?)
            .map_err(|e| Attempt::Refused(format!("its read timeout: {e}")))?;
        match s.read(&mut byte) {
            // The guest did not accept: its device closes the connection.
            Ok(0) => {
                return Err(Attempt::Again(
                    "closed unanswered (the guest does not listen there)".into(),
                ));
            }
            Ok(_) => line.extend_from_slice(&byte),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Attempt::Again(format!("awaiting its answer: {e}"))),
        }
    }
    left()?;
    let answer = String::from_utf8_lossy(&line);
    let ok = answer
        .strip_prefix("OK ")
        .is_some_and(|n| n.trim_end().parse::<u32>().is_ok());
    if !ok {
        return Err(Attempt::Refused(format!("answered {answer:?}, not OK")));
    }
    let cleared = Stream::set_write_timeout(&s, None).and_then(|()| Stream::set_read_timeout(&s, None));
    cleared.map_err(|e| Attempt::Refused(format!("its timeouts: {e}")))?;
    Ok(s)
}
