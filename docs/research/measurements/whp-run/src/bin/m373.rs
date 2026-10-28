//! What Windows' TCP says of the bytes its receiver has not acknowledged (PM M373, D163's
//! log drivers on Windows), as M233 measured Linux's (SIOCOUTQ, TCP_INFO) and macOS's
//! (TCP_CONNECTION_INFO): SIO_TCP_INFO's TCP_INFO_v1 of a loopback connection's sender,
//! beside the bytes it wrote. Its documentation says BytesOut is "the total number of bytes
//! sent" and BytesInFlight "the current number of sent bytes that are unacknowledged", but
//! not whether BytesOut counts bytes sent again, nor whether either counts a FIN. As
//! acked.py's steps:
//!
//! 1. 1,000 bytes written to a receiver that reads none;
//! 2. its receive buffer set to 4,096 bytes (SO_RCVBUF), written on until the sender takes
//!    no more: what is held, unsent, where the window closes;
//! 3. the window held shut for 30 s, read every 5 s: the persist timer's probes, and what
//!    they add to BytesOut and BytesRetrans;
//! 4. all read, the receiver's buffer grown again;
//! 5. the sender's shutdown(SD_SEND): its FIN, at once and 200 ms on;
//! 6. the receiver's close, 200 ms on.
//!
//! Then again with the receiver's buffer 1 byte from its listener on (`full`), so that it
//! has no room for the persist timer's probes: a probe dropped is sent again, what an
//! unacknowledged byte sent again adds to BytesOut and to BytesRetrans, which a loopback
//! connection otherwise never shows.

use std::io::{Read as _, Write as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::windows::io::AsRawSocket as _;
use std::time::Duration;

use windows_sys::Win32::Networking::WinSock::{
    SIO_TCP_INFO, SO_RCVBUF, SOCKET, SOL_SOCKET, TCP_INFO_v1, WSAIoctl, setsockopt,
};

fn info(s: &TcpStream) -> Result<TCP_INFO_v1, String> {
    let version: u32 = 1;
    let mut info = TCP_INFO_v1::default();
    let mut got = 0u32;
    // SAFETY: SIO_TCP_INFO of version 1 into `info`, its size given; synchronous.
    let r = unsafe {
        WSAIoctl(
            s.as_raw_socket() as SOCKET,
            SIO_TCP_INFO,
            (&raw const version).cast(),
            4,
            (&raw mut info).cast(),
            size_of::<TCP_INFO_v1>() as u32,
            &raw mut got,
            std::ptr::null_mut(),
            None,
        )
    };
    if r != 0 {
        return Err(format!("SIO_TCP_INFO: {}", std::io::Error::last_os_error()));
    }
    Ok(info)
}

fn say(step: &str, written: u64, s: &TcpStream) {
    let line = match info(s) {
        Ok(i) => format!(
            "written {written}; state {} in flight {} out {} retrans {} timeouts {} fast {} rtt {} us (min {}); \
             sndwnd {} cwnd {}; limited by rwin/cwnd/snd {}/{}/{} bytes; \
             written-(out-inflight) {} written-(out-retrans-inflight) {}",
            i.State,
            i.BytesInFlight,
            i.BytesOut,
            i.BytesRetrans,
            i.TimeoutEpisodes,
            i.FastRetrans,
            i.RttUs,
            i.MinRttUs,
            i.SndWnd,
            i.Cwnd,
            i.SndLimBytesRwin,
            i.SndLimBytesCwnd,
            i.SndLimBytesSnd,
            written as i64 - (i.BytesOut as i64 - i64::from(i.BytesInFlight)),
            written as i64 - (i.BytesOut as i64 - i64::from(i.BytesRetrans) - i64::from(i.BytesInFlight)),
        ),
        Err(e) => e,
    };
    println!("m373 {step}: {line}");
}

fn rcvbuf(s: &TcpStream, bytes: i32) {
    // SAFETY: setsockopt of an int.
    unsafe {
        setsockopt(
            s.as_raw_socket() as SOCKET,
            SOL_SOCKET,
            SO_RCVBUF,
            (&raw const bytes).cast(),
            4,
        )
    };
}

fn main() {
    let run = |full: bool| -> std::io::Result<()> {
        println!("m373 the receiver's buffer {}", if full { "1 byte from the start" } else { "as Windows sizes it" });
        let listener = TcpListener::bind("127.0.0.1:0")?;
        if full {
            // SAFETY: setsockopt of an int on the listener, which its connections take.
            unsafe {
                let one: i32 = 1;
                setsockopt(listener.as_raw_socket() as SOCKET, SOL_SOCKET, SO_RCVBUF, (&raw const one).cast(), 4)
            };
        }
        let mut c = TcpStream::connect(listener.local_addr()?)?;
        let (mut r, _) = listener.accept()?;
        let mut written = 0u64;
        c.write_all(&[b'x'; 1000])?;
        written += 1000;
        std::thread::sleep(Duration::from_millis(200));
        say("1. 1000 written, none read", written, &c);

        if !full {
            rcvbuf(&r, 4096);
        }
        c.set_nonblocking(true)?;
        let chunk = vec![b'y'; 65536];
        loop {
            match c.write(&chunk) {
                Ok(n) => written += n as u64,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        std::thread::sleep(Duration::from_millis(200));
        say("2. written until it took no more", written, &c);
        for i in 1..=6 {
            std::thread::sleep(Duration::from_secs(5));
            say(&format!("3. the window shut {} s", i * 5), written, &c);
        }

        rcvbuf(&r, 1 << 22);
        r.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut got = 0u64;
        let mut buf = vec![0u8; 1 << 20];
        while got < written {
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            got += n as u64;
        }
        std::thread::sleep(Duration::from_millis(200));
        say(&format!("4. {got} read"), written, &c);

        c.set_nonblocking(false)?;
        c.shutdown(Shutdown::Write)?;
        say("5. shut, at once", written, &c);
        std::thread::sleep(Duration::from_millis(200));
        say("5. shut, 200 ms on", written, &c);
        drop(r);
        std::thread::sleep(Duration::from_millis(200));
        say("6. its receiver closed, 200 ms on", written, &c);
        Ok(())
    };
    for full in [false, true] {
        if let Err(e) = run(full) {
            println!("m373: {e}");
        }
    }
}
