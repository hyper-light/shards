//! What a VM process confined as D141 confines it (M201) can be handed by its spawner, before
//! shards' Windows grants are built on it (PM M203):
//!
//! - a listening AF_UNIX socket the spawner bound, in a directory whose DACL does not grant
//!   the child's AppContainer (a user's `--vsock` directory), given by DuplicateHandle and
//!   by WSADuplicateSocketW, which Winsock's documentation names for sharing sockets: does
//!   the child's WSAEventSelect see a connection, its WSAPoll report it, its accept take it?
//! - connected sockets so given (the connections a spawner dials for its VM to host ports),
//!   one by WSADuplicateSocketW whose spawner closed its own copy before the child made its
//!   own: does the child's socket work, and does its end of the stream reach the peer?
//! - a file so given: does GetFinalPathNameByHandleW name it, normalized and as opened?
//! - a dial through the spawner, asked over a socket and answered by each way, timed against
//!   a dial an unconfined process makes itself;
//! - the child's own binary in a directory only its user, SYSTEM and Administrators may
//!   read, as `%LOCALAPPDATA%`'s are: does an AppContainer start from it?

// Windows' constants, matched by their own names.
#![allow(non_upper_case_globals)]

use std::ffi::c_void;
use std::io::Write as _;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, GENERIC_ALL, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Networking::WinSock::*;
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GetSecurityInfo, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW,
    SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, CreateWellKnownSid, DACL_SECURITY_INFORMATION, GetAce, GetLengthSid,
    GetTokenInformation, NO_INHERITANCE, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_QUERY, TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid,
    WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_NAME_NORMALIZED, FILE_NAME_OPENED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetFinalPathNameByHandleW, OPEN_EXISTING, READ_CONTROL, ReadFile, WriteFile,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, INFINITE, OpenProcessToken, ResumeThread,
    WaitForSingleObject,
};

/// Dials through the spawner, each way.
const DIALS: usize = 200;
/// WSAPROTOCOL_INFOW's size, which a dial's answer carries whole.
const INFO: usize = std::mem::size_of::<WSAPROTOCOL_INFOW>();

fn wsa() -> String {
    // SAFETY: no arguments.
    let e = unsafe { WSAGetLastError() };
    format!("WSA error {e} ({})", std::io::Error::from_raw_os_error(e))
}

fn startup() {
    let mut data = WSADATA::default();
    // SAFETY: a WSADATA for the call to fill.
    unsafe { WSAStartup(0x0202, &raw mut data) };
}

fn unix_addr(path: &Path) -> SOCKADDR_UN {
    let mut a = SOCKADDR_UN {
        sun_family: AF_UNIX,
        sun_path: [0; 108],
    };
    for (d, b) in a.sun_path.iter_mut().zip(path.display().to_string().bytes()) {
        *d = b as i8;
    }
    a
}

fn socket() -> Result<SOCKET, String> {
    // SAFETY: plain arguments; no child inherits it.
    let s = unsafe { WSASocketW(i32::from(AF_UNIX), SOCK_STREAM, 0, std::ptr::null(), 0, WSA_FLAG_NO_HANDLE_INHERIT) };
    if s == INVALID_SOCKET {
        return Err(format!("WSASocketW: {}", wsa()));
    }
    Ok(s)
}

fn listen_at(path: &Path) -> Result<SOCKET, String> {
    let s = socket()?;
    let a = unix_addr(path);
    // SAFETY: an address of its own length; our socket.
    unsafe {
        if bind(s, (&raw const a).cast(), std::mem::size_of::<SOCKADDR_UN>() as i32) != 0 {
            return Err(format!("bind {}: {}", path.display(), wsa()));
        }
        if listen(s, 16) != 0 {
            return Err(format!("listen: {}", wsa()));
        }
    }
    Ok(s)
}

fn dial(path: &Path) -> Result<SOCKET, String> {
    let s = socket()?;
    let a = unix_addr(path);
    // SAFETY: an address of its own length.
    if unsafe { connect(s, (&raw const a).cast(), std::mem::size_of::<SOCKADDR_UN>() as i32) } != 0 {
        let e = wsa();
        // SAFETY: our own socket.
        unsafe { closesocket(s) };
        return Err(e);
    }
    Ok(s)
}

fn accept_one(l: SOCKET) -> Result<SOCKET, String> {
    // SAFETY: a listening socket; no address asked.
    let a = unsafe { accept(l, std::ptr::null_mut(), std::ptr::null_mut()) };
    if a == INVALID_SOCKET {
        return Err(format!("accept: {}", wsa()));
    }
    Ok(a)
}

/// Gives up a read after 10 s, so that nothing here waits for ever.
fn timeout(s: SOCKET) {
    let ms: u32 = 10_000;
    // SAFETY: an option of its own size.
    unsafe { setsockopt(s, SOL_SOCKET, SO_RCVTIMEO, (&raw const ms).cast(), 4) };
}

/// Back to blocking, with no events selected.
fn blocking(s: SOCKET) {
    let mut off = 0u32;
    // SAFETY: our socket.
    unsafe {
        WSAEventSelect(s, 0, 0);
        ioctlsocket(s, FIONBIO, &raw mut off);
    }
}

fn send_all(s: SOCKET, bytes: &[u8]) -> Result<(), String> {
    let mut sent = 0;
    while sent < bytes.len() {
        // SAFETY: the rest of our bytes.
        let n = unsafe { send(s, bytes[sent..].as_ptr(), (bytes.len() - sent) as i32, 0) };
        if n <= 0 {
            return Err(format!("send: {}", wsa()));
        }
        sent += n as usize;
    }
    Ok(())
}

fn recv_exact(s: SOCKET, buf: &mut [u8]) -> Result<(), String> {
    let mut got = 0;
    while got < buf.len() {
        // SAFETY: the rest of our buffer.
        let n = unsafe { recv(s, buf[got..].as_mut_ptr(), (buf.len() - got) as i32, 0) };
        if n <= 0 {
            return Err(format!("recv {n} after {got}: {}", wsa()));
        }
        got += n as usize;
    }
    Ok(())
}

/// A line, up to its newline.
fn recv_line(s: SOCKET) -> Result<String, String> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while line.last() != Some(&b'\n') {
        recv_exact(s, &mut b)?;
        line.push(b[0]);
    }
    Ok(String::from_utf8_lossy(&line).trim_end().to_string())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// A socket made from what WSADuplicateSocketW wrote for this process.
fn from_info(bytes: &[u8]) -> Result<SOCKET, String> {
    if bytes.len() != INFO {
        return Err(format!("{} bytes of protocol info", bytes.len()));
    }
    let mut info = WSAPROTOCOL_INFOW::default();
    // SAFETY: the structure's own bytes, as the spawner's WSADuplicateSocketW wrote them.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), (&raw mut info).cast::<u8>(), INFO) };
    // SAFETY: the info, as Winsock's documentation has a target use it.
    let s = unsafe {
        WSASocketW(
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            &raw const info,
            0,
            WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if s == INVALID_SOCKET {
        return Err(format!("WSASocketW(FROM_PROTOCOL_INFO): {}", wsa()));
    }
    Ok(s)
}

/// The info by which process `pid` makes its own socket of `s`.
fn info_for(s: SOCKET, pid: u32) -> Result<Vec<u8>, String> {
    let mut info = WSAPROTOCOL_INFOW::default();
    // SAFETY: our socket; a structure for the call to fill.
    if unsafe { WSADuplicateSocketW(s, pid, &raw mut info) } != 0 {
        return Err(format!("WSADuplicateSocketW: {}", wsa()));
    }
    // SAFETY: the structure's own bytes.
    Ok(unsafe { std::slice::from_raw_parts((&raw const info).cast::<u8>(), INFO) }.to_vec())
}

/// `s` in `process`, by DuplicateHandle: its value there.
fn dup_into(h: HANDLE, process: HANDLE) -> Result<usize, String> {
    let mut out: HANDLE = std::ptr::null_mut();
    // SAFETY: our handle, duplicated into a process we made, of the same access.
    if unsafe { DuplicateHandle(GetCurrentProcess(), h, process, &raw mut out, 0, 0, DUPLICATE_SAME_ACCESS) } == 0 {
        return Err(format!("DuplicateHandle: {}", last()));
    }
    Ok(out as usize)
}

use whp_run::confine::{Container, job, last, spawn, wide};

// ---- The child ----

/// A listener given the child: its connection seen by an event, then by a poll, accepted,
/// read and answered.
fn child_listener(name: &str, s: SOCKET) -> String {
    // SAFETY: an event of our own, closed below.
    let event = unsafe { WSACreateEvent() };
    // SAFETY: our socket and event.
    if unsafe { WSAEventSelect(s, event, FD_ACCEPT as i32) } != 0 {
        return format!("{name}: WSAEventSelect: {}", wsa());
    }
    let events = [event as HANDLE];
    // SAFETY: one event of ours.
    let waited = unsafe { WSAWaitForMultipleEvents(1, events.as_ptr(), 0, 10_000, 0) };
    let mut fd = WSAPOLLFD {
        fd: s,
        events: POLLRDNORM as i16,
        revents: 0,
    };
    // SAFETY: one entry; no wait.
    let polled = unsafe { WSAPoll(&raw mut fd, 1, 0) };
    let mut accepted = INVALID_SOCKET;
    for _ in 0..200 {
        // SAFETY: a listening socket; no address asked.
        accepted = unsafe { accept(s, std::ptr::null_mut(), std::ptr::null_mut()) };
        if accepted != INVALID_SOCKET {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // SAFETY: our event.
    unsafe { WSACloseEvent(event) };
    if accepted == INVALID_SOCKET {
        return format!("{name}: event wait {waited}, poll {polled} revents {:#x}, accept: {}", fd.revents, wsa());
    }
    blocking(accepted);
    timeout(accepted);
    let line = recv_line(accepted);
    let answered = send_all(accepted, format!("the child accepted on {name}\n").as_bytes());
    format!(
        "{name}: event wait {waited} (0 is the event), poll {polled} revents {:#x}, accepted, read {line:?}, answered {answered:?}",
        fd.revents
    )
}

/// A connected socket given the child: written, then its answer seen by an event and read.
fn child_connected(name: &str, s: SOCKET) -> String {
    let wrote = send_all(s, format!("hello from the child on {name}\n").as_bytes());
    // SAFETY: an event of our own, closed below.
    let event = unsafe { WSACreateEvent() };
    // SAFETY: our socket and event.
    let selected = unsafe { WSAEventSelect(s, event, (FD_READ | FD_CLOSE) as i32) };
    let events = [event as HANDLE];
    // SAFETY: one event of ours.
    let waited = unsafe { WSAWaitForMultipleEvents(1, events.as_ptr(), 0, 10_000, 0) };
    // SAFETY: our event.
    unsafe { WSACloseEvent(event) };
    blocking(s);
    timeout(s);
    format!(
        "{name}: wrote {wrote:?}, event select {selected} wait {waited}, read {:?}",
        recv_line(s)
    )
}

fn final_path(file: HANDLE, flags: u32) -> String {
    let mut buf = vec![0u16; 1024];
    // SAFETY: a handle given us; a buffer of its length.
    let n = unsafe { GetFinalPathNameByHandleW(file, buf.as_mut_ptr(), buf.len() as u32, flags) } as usize;
    if n == 0 || n >= buf.len() {
        return format!("error: {}", last());
    }
    String::from_utf16_lossy(&buf[..n])
}

fn child(pipe: usize) {
    startup();
    let say = |line: String| {
        let _ = writeln!(std::io::stdout(), "child: {line}");
    };
    let mut buf = vec![0u8; 16 << 10];
    let mut got = 0usize;
    loop {
        let mut n = 0u32;
        // SAFETY: the pipe's read end, given us; the rest of our buffer.
        let ok = unsafe {
            ReadFile(
                pipe as HANDLE,
                buf[got..].as_mut_ptr(),
                (buf.len() - got) as u32,
                &raw mut n,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || n == 0 {
            break;
        }
        got += n as usize;
    }
    let text = String::from_utf8_lossy(&buf[..got]).into_owned();
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{name}=")))
            .unwrap_or("")
            .to_string()
    };
    let handle = |name: &str| field(name).parse::<usize>().unwrap_or(0);
    let info = |name: &str| unhex(&field(name)).unwrap_or_default();

    say(child_listener("listener by DuplicateHandle", handle("l1") as SOCKET));
    match from_info(&info("l2")) {
        Ok(s) => say(child_listener("listener by WSADuplicateSocketW", s)),
        Err(e) => say(format!("listener by WSADuplicateSocketW: {e}")),
    }
    say(child_connected("connected by DuplicateHandle", handle("c1") as SOCKET));
    for (key, label) in [
        ("c2", "connected by WSADuplicateSocketW"),
        ("c3", "connected by WSADuplicateSocketW, the spawner's copy closed first"),
    ] {
        match from_info(&info(key)) {
            Ok(s) => say(child_connected(label, s)),
            Err(e) => say(format!("{label}: {e}")),
        }
    }
    let file = handle("file") as HANDLE;
    say(format!(
        "a file's path, normalized: {}; as opened: {}",
        final_path(file, FILE_NAME_NORMALIZED),
        final_path(file, FILE_NAME_OPENED)
    ));

    // Dials through the spawner: a byte asks; the answer is a socket's protocol info, or
    // a handle's value.
    let link = handle("link") as SOCKET;
    for (ask, way) in [(b'D', "WSADuplicateSocketW"), (b'H', "DuplicateHandle")] {
        let mut took = Vec::with_capacity(DIALS);
        let mut failed = None;
        for _ in 0..DIALS {
            let t = Instant::now();
            let made = send_all(link, &[ask]).and_then(|()| {
                if ask == b'D' {
                    let mut bytes = vec![0u8; INFO];
                    recv_exact(link, &mut bytes)?;
                    let s = from_info(&bytes)?;
                    send_all(link, b"T")?;
                    Ok(s)
                } else {
                    let mut value = [0u8; 8];
                    recv_exact(link, &mut value)?;
                    Ok(u64::from_le_bytes(value) as SOCKET)
                }
            });
            match made {
                Ok(s) => {
                    took.push(t.elapsed());
                    // One byte through it, which the host side counts.
                    let _ = send_all(s, b"x");
                    // SAFETY: our socket.
                    unsafe { closesocket(s) };
                }
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        say(format!(
            "a dial through the spawner by {way}: {} of {DIALS}, {}{}",
            took.len(),
            whp_run::summary(&mut took),
            failed.map(|e| format!(", then {e}")).unwrap_or_default()
        ));
    }
    let _ = send_all(link, b"Q");
}

// ---- The spawner ----

/// The capability SIDs VidExo's DACL allows to read and write it (M200), which D141's VM
/// process holds.
fn videxo_capabilities() -> Result<Vec<Vec<u8>>, String> {
    let name = wide("\\\\?\\VidExo");
    // SAFETY: a NUL-terminated name; READ_CONTROL alone.
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(format!("VidExo: {}", last()));
    }
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: our handle; the descriptor freed below.
    let r = unsafe {
        GetSecurityInfo(
            h,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut dacl,
            std::ptr::null_mut(),
            &raw mut sd,
        )
    };
    // SAFETY: our handle.
    unsafe { CloseHandle(h) };
    if r != 0 || dacl.is_null() {
        return Err(format!("VidExo's DACL: {r}"));
    }
    const READ_WRITE: u32 = 0x0012_0089 | 0x0012_0116;
    let mut found = Vec::new();
    // SAFETY: the DACL read above, its ACEs read in place.
    unsafe {
        for i in 0..u32::from((*dacl).AceCount) {
            let mut ace: *mut c_void = std::ptr::null_mut();
            if GetAce(dacl, i, &raw mut ace) == 0 {
                continue;
            }
            let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
            if (*allowed).Header.AceType != 0 || (*allowed).Mask & READ_WRITE != READ_WRITE {
                continue;
            }
            let sid: PSID = (&raw mut (*allowed).SidStart).cast();
            let bytes = std::slice::from_raw_parts(sid.cast::<u8>(), GetLengthSid(sid) as usize);
            if bytes.len() > 12 && bytes[7] == 15 && bytes[8..12] == 3u32.to_le_bytes() {
                found.push(bytes.to_vec());
            }
        }
        LocalFree(sd.cast());
    }
    Ok(found)
}

/// Serves the child's dials on `link`, dialing `host` for each, until it says it is done.
fn serve_dials(link: SOCKET, host: &Path, process: HANDLE, pid: u32) -> Result<(usize, usize), String> {
    let mut held: Vec<SOCKET> = Vec::new();
    let (mut by_info, mut by_handle) = (0, 0);
    loop {
        let mut ask = [0u8; 1];
        recv_exact(link, &mut ask)?;
        match ask[0] {
            b'D' => {
                let s = dial(host)?;
                send_all(link, &info_for(s, pid)?)?;
                held.push(s);
                by_info += 1;
            }
            // The child made its own: ours goes.
            b'T' => {
                for s in held.drain(..) {
                    // SAFETY: our socket.
                    unsafe { closesocket(s) };
                }
            }
            b'H' => {
                let s = dial(host)?;
                let value = dup_into(s as HANDLE, process)?;
                // SAFETY: our copy; the child has its own handle.
                unsafe { closesocket(s) };
                send_all(link, &(value as u64).to_le_bytes())?;
                by_handle += 1;
            }
            b'Q' => return Ok((by_info, by_handle)),
            other => return Err(format!("the child asked {other:#x}")),
        }
    }
}

/// The current user's SID, as bytes.
fn user_sid() -> Result<Vec<u8>, String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: our own process's token, for querying, closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(format!("OpenProcessToken: {}", last()));
    }
    let mut buf = vec![0usize; 64];
    let mut len = 0u32;
    // SAFETY: a buffer of its length, aligned for TOKEN_USER.
    let ok = unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &raw mut len) };
    // SAFETY: our token.
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(format!("GetTokenInformation: {}", last()));
    }
    // SAFETY: TOKEN_USER at the buffer's start, its SID within the buffer.
    unsafe {
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        let sid = user.User.Sid;
        Ok(std::slice::from_raw_parts(sid.cast::<u8>(), GetLengthSid(sid) as usize).to_vec())
    }
}

fn well_known(kind: i32) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; 68];
    let mut len = buf.len() as u32;
    // SAFETY: a buffer of its length.
    if unsafe { CreateWellKnownSid(kind, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &raw mut len) } == 0 {
        return Err(format!("CreateWellKnownSid: {}", last()));
    }
    buf.truncate(len as usize);
    Ok(buf)
}

/// Gives `dir` a DACL of its own, inheriting none: full access to the current user,
/// SYSTEM and Administrators alone, as a profile's `AppData\Local` has.
fn private_dir(dir: &Path) -> Result<(), String> {
    let sids = [user_sid()?, well_known(WinLocalSystemSid)?, well_known(WinBuiltinAdministratorsSid)?];
    let entries: Vec<EXPLICIT_ACCESS_W> = sids
        .iter()
        .map(|sid| EXPLICIT_ACCESS_W {
            grfAccessPermissions: GENERIC_ALL,
            grfAccessMode: SET_ACCESS,
            grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: sid.as_ptr().cast_mut().cast(),
            },
        })
        .collect();
    let _ = NO_INHERITANCE;
    let mut acl: *mut ACL = std::ptr::null_mut();
    // SAFETY: the entries, with no old DACL; the new one freed below.
    let r = unsafe { SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), std::ptr::null(), &raw mut acl) };
    if r != 0 {
        return Err(format!("SetEntriesInAclW: {r}"));
    }
    let name = wide(&dir.display().to_string());
    // SAFETY: a NUL-terminated name and the DACL made above.
    let r = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        )
    };
    // SAFETY: what SetEntriesInAclW allocated.
    unsafe { LocalFree(acl.cast()) };
    if r != 0 {
        return Err(format!("SetNamedSecurityInfoW: {r}"));
    }
    Ok(())
}

/// Runs `exe child-hello` confined, and says how it ended.
fn hello_from(exe: &Path, caps: &[Vec<u8>]) -> String {
    let container = match Container::new("m203", "hello") {
        Ok(c) => c,
        Err(e) => return e,
    };
    let job = match job() {
        Ok(j) => j,
        Err(e) => return e,
    };
    let result = match spawn(exe, "child-hello", container.sid, caps, job, &[]) {
        Ok(pi) => {
            let mut code = 0u32;
            // SAFETY: the child we made, waited on, then its handles closed.
            unsafe {
                ResumeThread(pi.hThread);
                WaitForSingleObject(pi.hProcess, 20_000);
                GetExitCodeProcess(pi.hProcess, &raw mut code);
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
            }
            format!("exit code {code:#x}")
        }
        Err(e) => e,
    };
    // SAFETY: our job.
    unsafe { CloseHandle(job) };
    result
}

fn spawner() -> Result<(), String> {
    startup();
    let caps = videxo_capabilities().unwrap_or_else(|e| {
        println!("VidExo's capabilities: {e}");
        Vec::new()
    });
    let base = std::env::current_dir().map_err(|e| e.to_string())?.join("m203");
    let _ = std::fs::remove_dir_all(&base);
    let vsock = base.join("vsock");
    std::fs::create_dir_all(&vsock).map_err(|e| e.to_string())?;
    let (l1_path, l2_path, host) = (vsock.join("l1.sock"), vsock.join("l2.sock"), vsock.join("h.sock"));
    let l1 = listen_at(&l1_path)?;
    let l2 = listen_at(&l2_path)?;
    let h = listen_at(&host)?;
    let mut ends = Vec::new();
    let mut ours = Vec::new();
    for _ in 0..3 {
        ends.push(dial(&host)?);
        ours.push(accept_one(h)?);
    }
    let link_path = vsock.join("link.sock");
    let link_listener = listen_at(&link_path)?;
    let theirs = dial(&link_path)?;
    let link = accept_one(link_listener)?;
    let input = vsock.join("input.txt");
    std::fs::write(&input, "given by handle").map_err(|e| e.to_string())?;
    let file = std::fs::File::open(&input).map_err(|e| e.to_string())?;

    // Unconfined, for comparison: a dial of our own.
    let mut direct = Vec::with_capacity(DIALS);
    for _ in 0..DIALS {
        let t = Instant::now();
        let s = dial(&host)?;
        direct.push(t.elapsed());
        // SAFETY: our sockets.
        unsafe {
            closesocket(s);
            closesocket(accept_one(h)?);
        }
    }
    println!("a dial of the spawner's own: {}", whp_run::summary(&mut direct));

    let container = Container::new("m203", "vm")?;
    let job = job()?;
    let (mut pr, mut pw): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: two handles for the call to fill.
    if unsafe { CreatePipe(&raw mut pr, &raw mut pw, std::ptr::null(), 0) } == 0 {
        return Err(format!("CreatePipe: {}", last()));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let pi = spawn(&exe, &format!("child {}", pr as usize), container.sid, &caps, job, &[pr])?;
    let t = Instant::now();
    let given = (|| -> Result<String, String> {
        let l2_info = info_for(l2, pi.dwProcessId)?;
        let c2_info = info_for(ends[1], pi.dwProcessId)?;
        let c3_info = info_for(ends[2], pi.dwProcessId)?;
        Ok(format!(
            "l1={}\nl2={}\nc1={}\nc2={}\nc3={}\nlink={}\nfile={}\n",
            dup_into(l1 as HANDLE, pi.hProcess)?,
            hex(&l2_info),
            dup_into(ends[0] as HANDLE, pi.hProcess)?,
            hex(&c2_info),
            hex(&c3_info),
            dup_into(theirs as HANDLE, pi.hProcess)?,
            dup_into(file.as_raw_handle() as HANDLE, pi.hProcess)?,
        ))
    })();
    let given = match given {
        Ok(g) => g,
        Err(e) => {
            // SAFETY: the job ends the child.
            unsafe { CloseHandle(job) };
            return Err(e);
        }
    };
    println!("given in {} us", t.elapsed().as_micros());
    // c3's spawner copy goes before the child runs.
    // SAFETY: our sockets and pipe ends.
    unsafe {
        closesocket(ends[2]);
        closesocket(theirs);
        let mut written = 0u32;
        WriteFile(pw, given.as_ptr(), given.len() as u32, &raw mut written, std::ptr::null_mut());
        CloseHandle(pw);
        CloseHandle(pr);
        ResumeThread(pi.hThread);
    }
    // The listeners' clients.
    for (path, name) in [(&l1_path, "l1"), (&l2_path, "l2")] {
        let reply = dial(path).and_then(|s| {
            timeout(s);
            send_all(s, format!("to {name}\n").as_bytes())?;
            let line = recv_line(s);
            // SAFETY: our socket.
            unsafe { closesocket(s) };
            line
        });
        println!("spawner: dialed {name}: {reply:?}");
    }
    // The connected sockets' far ends.
    for (i, s) in ours.iter().enumerate() {
        timeout(*s);
        let heard = recv_line(*s);
        let answered = send_all(*s, format!("back to c{}\n", i + 1).as_bytes());
        println!("spawner: c{}'s far end heard {heard:?}, answered {answered:?}", i + 1);
    }
    // Every dialed connection's byte is counted as it is accepted, until the dials are
    // over and none has come for 200 ms.
    let done = std::sync::atomic::AtomicBool::new(false);
    let (served, bytes) = std::thread::scope(|scope| {
        let counter = scope.spawn(|| {
            let mut on = 1u32;
            // SAFETY: our listener, polled from here on.
            unsafe { ioctlsocket(h, FIONBIO, &raw mut on) };
            let (mut accepted, mut bytes) = (0usize, 0usize);
            let mut quiet = Instant::now();
            loop {
                // SAFETY: a listening socket; no address asked.
                let a = unsafe { accept(h, std::ptr::null_mut(), std::ptr::null_mut()) };
                if a == INVALID_SOCKET {
                    if done.load(std::sync::atomic::Ordering::Relaxed) && quiet.elapsed() > Duration::from_millis(200) {
                        return (accepted, bytes);
                    }
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                quiet = Instant::now();
                accepted += 1;
                blocking(a);
                timeout(a);
                let mut b = [0u8; 1];
                if recv_exact(a, &mut b).is_ok() {
                    bytes += 1;
                }
                // SAFETY: our socket.
                unsafe { closesocket(a) };
            }
        });
        let served = serve_dials(link, &host, pi.hProcess, pi.dwProcessId);
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        (served, counter.join())
    });
    println!("spawner: dials served {served:?}; (accepted, bytes) through them {bytes:?}");
    let mut code = 0u32;
    // SAFETY: the child's handles.
    unsafe {
        WaitForSingleObject(pi.hProcess, INFINITE);
        GetExitCodeProcess(pi.hProcess, &raw mut code);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        CloseHandle(job);
    }
    println!("spawner: the child exited {code:#x}");
    drop(container);

    // The child's binary where only its user, SYSTEM and Administrators may read.
    let private = base.join("private");
    std::fs::create_dir_all(&private).map_err(|e| e.to_string())?;
    let copy = private.join("m203.exe");
    std::fs::copy(&exe, &copy).map_err(|e| e.to_string())?;
    println!("from the probe's own directory: {}", hello_from(&exe, &caps));
    match private_dir(&private) {
        Ok(()) => println!("from a directory of the user's alone: {}", hello_from(&copy, &caps)),
        Err(e) => println!("a directory of the user's alone: {e}"),
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("child") => child(args.get(2).and_then(|v| v.parse().ok()).unwrap_or(0)),
        Some("child-hello") => println!("child-hello: running"),
        _ => {
            if let Err(e) = spawner() {
                println!("spawner: {e}");
            }
        }
    }
}
