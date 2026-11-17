//! What Windows' AF_UNIX takes where a pair's directory is longer than `sun_path` holds, before
//! shards' pairs are built on any of it (kiln goal 3cd82843, r735: every VM process in an
//! AppContainer under a long LOCALAPPDATA ended making its first pair, its rendezvous at
//! `<LOCALAPPDATA>\Packages\<container>\AC\Temp\shards-pair-<pid>-<n>.sock`, 134 bytes):
//!
//! - a pathname listener in a directory past 107 bytes: refused before it is asked, as now;
//! - a relative name, bound and connected with the process's current directory set to that long
//!   directory, where its file lands, and a connect of the same name from another directory;
//! - afunix.h's parent-path controls (`SIO_AF_UNIX_SETBINDPARENTPATH`,
//!   `SIO_AF_UNIX_SETCONNPARENTPATH`), which Microsoft documents by their names alone, each given
//!   the long directory as UTF-8 and as UTF-16 (microsoft/WSL#9455 reports WSAEOPNOTSUPP);
//! - an abstract address's bind, listen and connect, which M340 found refused (WSAEINVAL);
//! - `SIO_AF_UNIX_GETPEERPID` on an accepted connection (afunix.h: "Returns ULONG PID of the
//!   connected peer process").
//!
//! Run on the Windows host, from this directory: `cargo run --release`. Each step prints one
//! line, `<case> <step>: ok` or `<case> <step>: refused <WSA error> `, then the summary.

use std::path::{Path, PathBuf};

use windows_sys::Win32::Networking::WinSock::{
    AF_UNIX, INVALID_SOCKET, SIO_AF_UNIX_GETPEERPID, SIO_AF_UNIX_SETBINDPARENTPATH,
    SIO_AF_UNIX_SETCONNPARENTPATH, SOCK_STREAM, SOCKADDR_UN, SOCKET, WSA_FLAG_NO_HANDLE_INHERIT,
    WSA_FLAG_OVERLAPPED, WSADATA, WSAGetLastError, WSAIoctl, WSASocketW, WSAStartup, accept, bind,
    closesocket, connect, listen,
};

/// An AF_UNIX stream socket, closed as it drops.
struct Sock(SOCKET);

impl Drop for Sock {
    fn drop(&mut self) {
        // SAFETY: a socket this value owns.
        unsafe { closesocket(self.0) };
    }
}

fn last() -> i32 {
    // SAFETY: no arguments.
    unsafe { WSAGetLastError() }
}

fn socket() -> Result<Sock, i32> {
    // SAFETY: plain arguments.
    let s = unsafe {
        WSASocketW(
            i32::from(AF_UNIX),
            SOCK_STREAM,
            0,
            std::ptr::null(),
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if s == INVALID_SOCKET {
        Err(last())
    } else {
        Ok(Sock(s))
    }
}

/// `bytes` as `sun_path`, NUL-terminated: none where they don't fit.
fn address(bytes: &[u8]) -> Option<SOCKADDR_UN> {
    let mut a = SOCKADDR_UN {
        sun_family: AF_UNIX,
        sun_path: [0; 108],
    };
    if bytes.len() >= a.sun_path.len() {
        return None;
    }
    for (d, b) in a.sun_path.iter_mut().zip(bytes) {
        *d = *b as i8;
    }
    Some(a)
}

const LEN: i32 = std::mem::size_of::<SOCKADDR_UN>() as i32;

fn bind_to(s: &Sock, a: &SOCKADDR_UN) -> Result<(), i32> {
    // SAFETY: an address of its own length.
    if unsafe { bind(s.0, (a as *const SOCKADDR_UN).cast(), LEN) } != 0 {
        Err(last())
    } else {
        Ok(())
    }
}

fn listen_on(s: &Sock) -> Result<(), i32> {
    // SAFETY: a bound socket.
    if unsafe { listen(s.0, 1) } != 0 {
        Err(last())
    } else {
        Ok(())
    }
}

fn connect_to(s: &Sock, a: &SOCKADDR_UN) -> Result<(), i32> {
    // SAFETY: an address of its own length.
    if unsafe { connect(s.0, (a as *const SOCKADDR_UN).cast(), LEN) } != 0 {
        Err(last())
    } else {
        Ok(())
    }
}

fn accept_on(s: &Sock) -> Result<Sock, i32> {
    // SAFETY: a listening socket; no address asked.
    let c = unsafe { accept(s.0, std::ptr::null_mut(), std::ptr::null_mut()) };
    if c == INVALID_SOCKET {
        Err(last())
    } else {
        Ok(Sock(c))
    }
}

fn ioctl_in(s: &Sock, code: u32, input: &[u8]) -> Result<(), i32> {
    let mut returned = 0u32;
    // SAFETY: an input buffer of its own length, no output, synchronous.
    let r = unsafe {
        WSAIoctl(
            s.0,
            code,
            input.as_ptr().cast(),
            u32::try_from(input.len()).unwrap_or(u32::MAX),
            std::ptr::null_mut(),
            0,
            &raw mut returned,
            std::ptr::null_mut(),
            None,
        )
    };
    if r != 0 { Err(last()) } else { Ok(()) }
}

fn peer_pid(s: &Sock) -> Result<u32, i32> {
    let (mut pid, mut returned) = (0u32, 0u32);
    // SAFETY: a ULONG for the call to fill, synchronous.
    let r = unsafe {
        WSAIoctl(
            s.0,
            SIO_AF_UNIX_GETPEERPID,
            std::ptr::null(),
            0,
            (&raw mut pid).cast(),
            4,
            &raw mut returned,
            std::ptr::null_mut(),
            None,
        )
    };
    if r != 0 { Err(last()) } else { Ok(pid) }
}

fn say<T: std::fmt::Debug>(case: &str, step: &str, r: &Result<T, i32>) {
    match r {
        Ok(v) => println!("{case} {step}: ok {v:?}"),
        Err(code) => println!("{case} {step}: refused {code}"),
    }
}

/// A directory past what `sun_path` holds, made under the user's temporary directory.
fn long_dir() -> std::io::Result<PathBuf> {
    let mut dir = std::env::temp_dir().join(format!("afunix-long-paths-{}", std::process::id()));
    while dir.as_os_str().len() <= 120 {
        dir = dir.join("abcdefghijklmnopqrstuvwxyz");
    }
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A listener at `name` and a connect to it, each through `bind_with` and `connect_with` (which set
/// what each needs first), then an accept, and the peer's pid as the accepted end reads it.
fn pair(
    case: &str,
    name: &[u8],
    bind_with: impl Fn(&Sock) -> Result<(), i32>,
    connect_with: impl Fn(&Sock) -> Result<(), i32>,
) {
    let Some(a) = address(name) else {
        println!("{case} address: {} bytes, past sun_path", name.len());
        return;
    };
    let Ok(l) = socket() else {
        return say::<()>(case, "socket", &Err(last()));
    };
    let set = bind_with(&l);
    say(case, "listener's control", &set);
    let bound = bind_to(&l, &a);
    say(case, "bind", &bound);
    if bound.is_err() {
        return;
    }
    say(case, "listen", &listen_on(&l));
    let Ok(c) = socket() else {
        return say::<()>(case, "socket", &Err(last()));
    };
    say(case, "client's control", &connect_with(&c));
    let connected = connect_to(&c, &a);
    say(case, "connect", &connected);
    if connected.is_err() {
        return;
    }
    let accepted = accept_on(&l);
    say(case, "accept", &accepted.as_ref().map(|_| ()).map_err(|e| *e));
    if let Ok(s) = accepted {
        say(case, "peer pid (this process's is shown after)", &peer_pid(&s));
    }
}

fn main() -> std::io::Result<()> {
    let mut data = WSADATA::default();
    // SAFETY: a WSADATA for the call to fill.
    let started = unsafe { WSAStartup(0x0202, &raw mut data) };
    println!("WSAStartup: {started}; this process: {}", std::process::id());
    let dir = long_dir()?;
    let shown = dir.display().to_string();
    println!("long directory: {} bytes: {shown}", shown.len());
    let none = |_: &Sock| Ok(());

    // As shards' pair is made now: a pathname there.
    pair(
        "absolute",
        dir.join("a.sock").to_string_lossy().as_bytes(),
        none,
        none,
    );

    // A relative name, the process's current directory the long one.
    let home = std::env::current_dir()?;
    std::env::set_current_dir(&dir)?;
    pair("relative", b"r.sock", none, none);
    std::env::set_current_dir(&home)?;
    println!(
        "relative file in the long directory: {}",
        Path::new(&dir).join("r.sock").exists()
    );

    // A relative bind there, and a connect of the same name from elsewhere.
    std::env::set_current_dir(&dir)?;
    if let (Ok(l), Some(a)) = (socket(), address(b"x.sock")) {
        let bound = bind_to(&l, &a).and_then(|()| listen_on(&l));
        say(
            "relative-elsewhere",
            "bind and listen in the long directory",
            &bound,
        );
        std::env::set_current_dir(&home)?;
        if let Ok(c) = socket() {
            say(
                "relative-elsewhere",
                "connect from another directory",
                &connect_to(&c, &a),
            );
        }
    }
    std::env::set_current_dir(&home)?;

    // afunix.h's parent-path controls, the parent as UTF-8 and as UTF-16, each NUL-terminated.
    let utf8: Vec<u8> = shown.bytes().chain([0]).collect();
    let utf16: Vec<u8> = shown
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect();
    for (case, parent, name) in [
        ("parent-utf8", &utf8, &b"p8.sock"[..]),
        ("parent-utf16", &utf16, &b"p16.sock"[..]),
    ] {
        pair(
            case,
            name,
            |s| ioctl_in(s, SIO_AF_UNIX_SETBINDPARENTPATH, parent),
            |s| ioctl_in(s, SIO_AF_UNIX_SETCONNPARENTPATH, parent),
        );
        let file = String::from_utf8_lossy(name).into_owned();
        println!("{case} file in the long directory: {}", dir.join(&file).exists());
    }

    // An abstract address: a NUL, then its name (M340: its connect refused WSAEINVAL).
    pair("abstract", b"\0afunix-long-paths-abstract", none, none);

    let _ = std::fs::remove_dir_all(
        std::env::temp_dir().join(format!("afunix-long-paths-{}", std::process::id())),
    );
    Ok(())
}
