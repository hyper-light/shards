//! Named pipes for the Windows host side of vsock connections, measured before any of it is
//! built (ROOT's S0, to supersede D140's Windows half; PM M603):
//!
//! - (a) whether a process confined as D141 confines a VM process (`shards_ipc`'s
//!   AppContainer, its mitigations and Job) can open a named pipe its user made, granted its
//!   user alone (shards' pipes today, pipe.rs `attributes`) or ALL APPLICATION PACKAGES too;
//!   can make a pipe's server, under `\\.\pipe\` or `\\.\pipe\LOCAL\`, that its user then
//!   opens; and serves a client over a server instance its spawner made and handed in;
//! - (b) what one such instance costs its spawner: CreateNamedPipeW alone, and with its
//!   handing in (DuplicateHandle, `Suspended::give`), n p50 p90 p99 max;
//! - (c) when an overlapped write of 1 MiB to a pipe of 64 KiB completes while its reader
//!   takes 64 KiB every 10 ms, and what a CancelIoEx partway leaves its reader;
//! - (d) a half-close over a message-type pipe, as `shards_ipc`'s pipes make one (a
//!   zero-byte message): what a client reading in byte mode, as a host program's does, reads
//!   of "abc", a zero-byte message and "def"; whether it writes on after; and what it reads and
//!   writes once the server has closed;
//! - (e) vsock's pattern: bytes, then a zero-byte message, and nothing more that way, read by
//!   `std::fs::File` in byte mode on a thread of its own (each read's result waited for two
//!   seconds at most, a read still waiting said so): whether it reads the bytes, then
//!   `Ok(0)`, and then writes on;
//! - (f) (d) and (e) again, read by `std::fs::File` once its end is put in message read mode
//!   (SetNamedPipeHandleState, PIPE_READMODE_MESSAGE), as a client could set it at its open:
//!   whether "abc", a zero-byte message and "def" read as `Ok(3)`, `Ok(0)`, `Ok(3)`, and the
//!   vsock pattern's zero-byte message as `Ok(0)` with the client writing on after;
//! - (g) in that mode, what a read of a message larger than its buffer gives through std
//!   (ReadFile's ERROR_MORE_DATA), and what of the message is left to read after it.
//!
//! Run as `windows-pipes-s0` (the parent); it starts itself as `windows-pipes-s0 child …`.

#[cfg(not(windows))]
fn main() {
    println!("windows-pipes-s0: Windows only");
}

#[cfg(windows)]
fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let done = if args.get(1).is_some_and(|a| a == "child") {
        win::child(&args)
    } else {
        win::parent()
    };
    if let Err(e) = done {
        println!("S0 failed: {e}");
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::{OsStr, OsString, c_void};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::ptr::{null, null_mut};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_IO_INCOMPLETE, ERROR_MORE_DATA, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE,
        GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
        ReadFile, WriteFile,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_READMODE_MESSAGE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
        PeekNamedPipe, SetNamedPipeHandleState,
    };
    use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentProcess, OpenProcessToken};

    /// A pipe's buffer each way, as `shards_ipc`'s own pipes ask for one of a window's size.
    const BUFFER: u32 = 64 << 10;

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }

    fn last() -> u32 {
        // SAFETY: reads this thread's last error.
        unsafe { GetLastError() }
    }

    /// This process's user's SID, in its string form.
    fn user_sid() -> Result<String, String> {
        let mut token: HANDLE = null_mut();
        // SAFETY: this process's own token, for querying.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
            return Err(format!("OpenProcessToken: {}", last()));
        }
        let mut len = 0u32;
        // SAFETY: asks the size alone.
        unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &raw mut len) };
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: a buffer of `len` bytes or more, aligned for TOKEN_USER.
        let ok = unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &raw mut len) };
        // SAFETY: the token opened above, closed once.
        unsafe { CloseHandle(token) };
        if ok == 0 {
            return Err(format!("GetTokenInformation: {}", last()));
        }
        // SAFETY: the buffer GetTokenInformation filled with a TOKEN_USER.
        let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
        let mut text: *mut u16 = null_mut();
        // SAFETY: the SID in the buffer, its string allocated by the call.
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut text) } == 0 {
            return Err(format!("ConvertSidToStringSidW: {}", last()));
        }
        let mut n = 0;
        // SAFETY: a NUL-terminated string the call allocated.
        while unsafe { *text.add(n) } != 0 {
            n += 1;
        }
        // SAFETY: as above, `n` characters long.
        let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, n) });
        // SAFETY: the string the call allocated, freed once.
        unsafe { LocalFree(text.cast()) };
        Ok(s)
    }

    /// A security descriptor made of SDDL, freed as it drops.
    struct Sd(PSECURITY_DESCRIPTOR);

    impl Drop for Sd {
        fn drop(&mut self) {
            // SAFETY: the descriptor the conversion allocated, freed once.
            unsafe { LocalFree(self.0) };
        }
    }

    fn sd(sddl: &str) -> Result<Sd, String> {
        let w = wide(sddl);
        let mut p: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: a NUL-terminated SDDL string; the descriptor is allocated by the call.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w.as_ptr(),
                SDDL_REVISION_1,
                &raw mut p,
                null_mut(),
            )
        } == 0
        {
            return Err(format!("SDDL {sddl}: {}", last()));
        }
        Ok(Sd(p))
    }

    /// A pipe's server end named `name`, in byte mode, any number of instances.
    fn server(name: &str, sd: Option<&Sd>, overlapped: bool, first: bool) -> Result<OwnedHandle, u32> {
        let w = wide(name);
        let sa = sd.map(|sd| SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: 0,
        });
        let mode = PIPE_ACCESS_DUPLEX
            | if overlapped { FILE_FLAG_OVERLAPPED } else { 0 }
            | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        // SAFETY: a NUL-terminated name, and attributes that outlive the call where given.
        let h = unsafe {
            CreateNamedPipeW(
                w.as_ptr(),
                mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER,
                BUFFER,
                0,
                sa.as_ref().map_or(null(), |s| s as *const SECURITY_ATTRIBUTES),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(last());
        }
        // SAFETY: a handle the call made, owned from here on.
        Ok(unsafe { OwnedHandle::from_raw_handle(h as RawHandle) })
    }

    /// A pipe's client end, opened by its name.
    fn client(name: &str) -> Result<OwnedHandle, u32> {
        let w = wide(name);
        // SAFETY: a NUL-terminated name.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(last());
        }
        // SAFETY: a handle the call made, owned from here on.
        Ok(unsafe { OwnedHandle::from_raw_handle(h as RawHandle) })
    }

    fn write(h: &OwnedHandle, b: &[u8]) -> Result<u32, u32> {
        let mut n = 0u32;
        // SAFETY: `b.len()` bytes of `b`, a handle opened for synchronous I/O.
        if unsafe {
            WriteFile(
                h.as_raw_handle(),
                b.as_ptr(),
                b.len() as u32,
                &raw mut n,
                null_mut(),
            )
        } == 0
        {
            return Err(last());
        }
        Ok(n)
    }

    fn read(h: &OwnedHandle, b: &mut [u8]) -> Result<u32, u32> {
        let mut n = 0u32;
        // SAFETY: `b.len()` bytes of `b`, a handle opened for synchronous I/O.
        if unsafe {
            ReadFile(
                h.as_raw_handle(),
                b.as_mut_ptr(),
                b.len() as u32,
                &raw mut n,
                null_mut(),
            )
        } == 0
        {
            return Err(last());
        }
        Ok(n)
    }

    /// What a pipe holds for its reader now.
    fn available(h: &OwnedHandle) -> u32 {
        let mut n = 0u32;
        // SAFETY: a pipe handle; no bytes copied, its count of available ones read.
        unsafe {
            PeekNamedPipe(
                h.as_raw_handle(),
                null_mut(),
                0,
                null_mut(),
                &raw mut n,
                null_mut(),
            )
        };
        n
    }

    fn summary(name: &str, v: &mut [Duration]) {
        v.sort_unstable();
        let at = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize].as_micros();
        println!(
            "S0 cost {name}: n {} p50 {} p90 {} p99 {} max {} (us)",
            v.len(),
            at(0.5),
            at(0.9),
            at(0.99),
            v[v.len() - 1].as_micros()
        );
    }

    /// `f` until it answers, or `within` passes: its last error then.
    fn retry<T>(mut f: impl FnMut() -> Result<T, u32>, within: Duration) -> Result<T, u32> {
        let began = Instant::now();
        loop {
            match f() {
                Ok(t) => return Ok(t),
                Err(e) if began.elapsed() >= within => return Err(e),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    fn tag() -> String {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u32(std::process::id());
        format!("{}-{:016x}", std::process::id(), h.finish())
    }

    pub fn parent() -> Result<(), String> {
        let tag = tag();
        let user = user_sid()?;
        let only_user = sd(&format!("D:P(A;;GA;;;{user})"))?;
        let with_packages = sd(&format!("D:P(A;;GA;;;{user})(A;;GA;;;AC)"))?;
        let user_name = format!(r"\\.\pipe\s0-user-{tag}");
        let all_name = format!(r"\\.\pipe\s0-all-{tag}");
        let given_name = format!(r"\\.\pipe\s0-given-{tag}");
        let global_name = format!(r"\\.\pipe\s0-child-{tag}");
        let local_name = format!(r"\\.\pipe\LOCAL\s0-child-{tag}");
        let _user_server =
            server(&user_name, Some(&only_user), false, true).map_err(|e| format!("its user's pipe: {e}"))?;
        let _all_server = server(&all_name, Some(&with_packages), false, true)
            .map_err(|e| format!("a packages' pipe: {e}"))?;
        let given_server =
            server(&given_name, Some(&only_user), false, true).map_err(|e| format!("the given pipe: {e}"))?;
        let records = std::env::temp_dir().join(format!("pipes-s0-{}", std::process::id()));
        std::fs::create_dir_all(&records).map_err(|e| format!("{}: {e}", records.display()))?;
        let container = shards_ipc::AppContainer::new("pipes-s0", &records)
            .map_err(|e| format!("its AppContainer: {e}"))?;
        let exe = std::env::current_exe().map_err(|e| format!("this program: {e}"))?;
        let argv: Vec<OsString> = vec![
            "child".into(),
            user_name.clone().into(),
            all_name.clone().into(),
            global_name.clone().into(),
            local_name.clone().into(),
        ];
        let args: Vec<&OsStr> = argv.iter().map(OsString::as_os_str).collect();
        let (stdout, stderr) = (std::io::stdout(), std::io::stderr());
        let given = [shards_ipc::Given {
            name: "given",
            handle: given_server.as_handle(),
        }];
        let spawn = shards_ipc::Spawn {
            program: &exe,
            args: &args,
            grants_at: args.len(),
            given: &given,
            stdin: None,
            stdout: stdout.as_handle(),
            stderr: stderr.as_handle(),
            sockets: &[],
            env: &[],
            container: Some(&container),
            holds: shards_ipc::Holds::Nothing,
            group: None,
            graphics: false,
        };
        let suspended = shards_ipc::spawn_suspended(&spawn).map_err(|e| format!("its child: {e}"))?;
        // (b) An instance made here, alone and handed in, a thousand times each.
        let (mut alone, mut handed) = (Vec::new(), Vec::new());
        for _ in 0..1000 {
            let t = Instant::now();
            let h = server(
                &format!(r"\\.\pipe\s0-cost-{tag}-a"),
                Some(&only_user),
                true,
                false,
            )
            .map_err(|e| format!("an instance: {e}"))?;
            alone.push(t.elapsed());
            drop(h);
            let t = Instant::now();
            let h = server(
                &format!(r"\\.\pipe\s0-cost-{tag}-b"),
                Some(&only_user),
                true,
                false,
            )
            .map_err(|e| format!("an instance: {e}"))?;
            suspended
                .give(h.as_handle())
                .map_err(|e| format!("handing it in: {e}"))?;
            handed.push(t.elapsed());
            drop(h);
        }
        summary("an instance made (CreateNamedPipeW)", &mut alone);
        summary("an instance made and handed in (+ DuplicateHandle)", &mut handed);
        let child = suspended
            .resume()
            .map_err(|e| format!("resuming its child: {e}"))?;
        // (a) Its given instance served: this process its client.
        let mut buf = [0u8; 16];
        let given_client = retry(|| client(&given_name), Duration::from_secs(10));
        match given_client {
            Ok(c) => {
                let wrote = write(&c, b"hello");
                let got =
                    read(&c, &mut buf).map(|n| String::from_utf8_lossy(&buf[..n as usize]).into_owned());
                println!("S0 parent: its given instance, opened here: wrote {wrote:?}, read back {got:?}");
            }
            Err(e) => println!("S0 parent: its given instance would not open here: {e}"),
        }
        // (a) The child's own servers, opened by name here.
        for name in [&global_name, &local_name] {
            let opened = retry(|| client(name), Duration::from_secs(3)).map(drop);
            println!("S0 parent: opening the child's {name} here: {opened:?}");
        }
        let code = child.wait().map_err(|e| format!("its child's end: {e}"))?;
        println!("S0 parent: its child exited {code}");
        pended(&tag)?;
        half_close(&tag)?;
        half_close_last(&tag, false)?;
        message_mode(&tag)?;
        half_close_last(&tag, true)?;
        drop(container);
        let _ = std::fs::remove_dir_all(&records);
        Ok(())
    }

    pub fn child(args: &[OsString]) -> Result<(), String> {
        let arg = |i: usize| -> Result<String, String> {
            args.get(i)
                .and_then(|a| a.to_str())
                .map(str::to_string)
                .ok_or(format!("argument {i}"))
        };
        let (user_name, all_name, global_name, local_name) = (arg(2)?, arg(3)?, arg(4)?, arg(5)?);
        let pipe = args
            .iter()
            .position(|a| a == "--grants")
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.to_str()?.parse::<usize>().ok())
            .ok_or("no --grants")?;
        let given = shards_ipc::given(pipe).map_err(|e| format!("its grants: {e}"))?;
        println!("S0 child: in an AppContainer: {}", shards_ipc::in_app_container());
        println!(
            "S0 child: opening a pipe granted its user alone: {:?}",
            client(&user_name).map(drop)
        );
        println!(
            "S0 child: opening one granted ALL APPLICATION PACKAGES too: {:?}",
            client(&all_name).map(drop)
        );
        let global = server(&global_name, None, false, true);
        println!(
            "S0 child: making the server {global_name}: {:?}",
            global.as_ref().map(|_| ())
        );
        let local = server(&local_name, None, false, true);
        println!(
            "S0 child: making the server {local_name}: {:?}",
            local.as_ref().map(|_| ())
        );
        match given.iter().find(|(n, _)| n == "given") {
            Some((_, shards_ipc::Handed::Handle(v))) => {
                // SAFETY: the server end its spawner duplicated into it, owned from here on.
                let h = unsafe { OwnedHandle::from_raw_handle(*v as RawHandle) };
                // SAFETY: a pipe server's handle, for synchronous I/O.
                let connected = unsafe { ConnectNamedPipe(h.as_raw_handle(), null_mut()) };
                let connected = if connected != 0 || last() == ERROR_PIPE_CONNECTED {
                    Ok(())
                } else {
                    Err(last())
                };
                let mut buf = [0u8; 16];
                let got =
                    read(&h, &mut buf).map(|n| String::from_utf8_lossy(&buf[..n as usize]).into_owned());
                let wrote = write(&h, b"reply");
                println!(
                    "S0 child: its given instance: connected {connected:?}, read {got:?}, wrote {wrote:?}"
                );
            }
            _ => println!("S0 child: no given instance"),
        }
        // Held while its spawner tries to open them.
        std::thread::sleep(Duration::from_secs(4));
        drop((global, local));
        Ok(())
    }

    /// A message-type pipe's server end named `name`, one instance.
    fn message_server(name: &str) -> Result<OwnedHandle, String> {
        let w = wide(name);
        // SAFETY: a NUL-terminated name; default security.
        let h = unsafe {
            CreateNamedPipeW(
                w.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                BUFFER,
                BUFFER,
                0,
                null(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(format!("its server: {}", last()));
        }
        // SAFETY: a handle the call made, owned from here on.
        Ok(unsafe { OwnedHandle::from_raw_handle(h as RawHandle) })
    }

    /// What a std read said: its bytes, or its error's kind and code.
    fn said(r: std::io::Result<usize>, buf: &[u8]) -> String {
        match r {
            Ok(n) => format!("Ok({n}) {:?}", String::from_utf8_lossy(&buf[..n])),
            Err(e) => format!("Err({:?}, os {:?})", e.kind(), e.raw_os_error()),
        }
    }

    /// (d) A half-close over a message-type pipe, as `shards_ipc`'s pipes make one: read by a
    /// client in byte mode through ReadFile itself, then through `std::fs::File`, as a host
    /// program reads; a message larger than the reader's buffer among them.
    fn half_close(tag: &str) -> Result<(), String> {
        use std::io::{Read as _, Write as _};
        let long = [b'x'; 100];
        // Through ReadFile itself.
        let name = format!(r"\\.\pipe\s0-half-{tag}");
        let server = message_server(&name)?;
        let client = client(&name).map_err(|e| format!("its client: {e}"))?;
        println!(
            "S0 half: server writes \"abc\" {:?}, a zero-byte message {:?}, \"def\" {:?}, 100 bytes {:?}",
            write(&server, b"abc"),
            write(&server, b""),
            write(&server, b"def"),
            write(&server, &long)
        );
        let mut buf = [0u8; 16];
        for i in 0..6 {
            let got =
                read(&client, &mut buf).map(|n| String::from_utf8_lossy(&buf[..n as usize]).into_owned());
            println!("S0 half: ReadFile, byte mode, read {i}: {got:?}");
        }
        let mut rest = 0usize;
        while available(&client) > 0 {
            rest += read(&client, &mut buf).map_err(|e| format!("reading: {e}"))? as usize;
        }
        println!("S0 half: ReadFile: {rest} more bytes after them");
        println!(
            "S0 half: ReadFile's client writes on: {:?}",
            write(&client, b"up")
        );
        let got = read(&server, &mut buf).map(|n| String::from_utf8_lossy(&buf[..n as usize]).into_owned());
        println!("S0 half: the server reads it: {got:?}");
        drop(server);
        println!(
            "S0 half: once the server has closed, ReadFile's client reads {:?} and writes {:?}",
            read(&client, &mut buf),
            write(&client, b"late")
        );
        // Through std::fs::File, opened by the pipe's name for reading and writing.
        let name = format!(r"\\.\pipe\s0-half-std-{tag}");
        let server = message_server(&name)?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name)
            .map_err(|e| format!("std's open: {e}"))?;
        let wrote = (
            write(&server, b"abc"),
            write(&server, b""),
            write(&server, b"def"),
            write(&server, &long),
        );
        println!("S0 half std: server writes {wrote:?}");
        for i in 0..6 {
            let r = file.read(&mut buf);
            println!("S0 half std: File::read {i}: {}", said(r, &buf));
        }
        let mut rest = Vec::new();
        while available_client(&file) > 0 {
            let r = file.read(&mut buf).map_err(|e| format!("reading: {e}"))?;
            rest.extend_from_slice(&buf[..r]);
        }
        println!("S0 half std: {} more bytes after them", rest.len());
        println!(
            "S0 half std: File writes on: {:?}",
            file.write(b"up").map_err(|e| e.kind())
        );
        let got = read(&server, &mut buf).map(|n| String::from_utf8_lossy(&buf[..n as usize]).into_owned());
        println!("S0 half std: the server reads it: {got:?}");
        drop(server);
        let r = file.read(&mut buf);
        println!(
            "S0 half std: once the server has closed, File::read {} and File::write {:?}",
            said(r, &buf),
            file.write(b"late").map_err(|e| (e.kind(), e.raw_os_error()))
        );
        Ok(())
    }

    /// (e) Bytes, then a zero-byte message, and nothing more that way; read in byte mode, or
    /// (f) with the client's end in message read mode.
    fn half_close_last(tag: &str, message: bool) -> Result<(), String> {
        use std::io::{Read as _, Write as _};
        let label = if message { "S0 msg last" } else { "S0 last" };
        let name = format!(r"\\.\pipe\s0-last-{}-{tag}", u8::from(message));
        let server = message_server(&name)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name)
            .map_err(|e| format!("std's open: {e}"))?;
        if message {
            println!(
                "{label}: the client's end in message read mode: {:?}",
                read_mode_message(&file)
            );
        }
        let mut writer = file.try_clone().map_err(|e| format!("its clone: {e}"))?;
        println!(
            "{label}: server writes \"abc\" {:?}, then a zero-byte message {:?}, then nothing",
            write(&server, b"abc"),
            write(&server, b"")
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let mut reader = file;
        let reading = std::thread::spawn(move || {
            let mut buf = [0u8; 16];
            for _ in 0..3 {
                let r = reader.read(&mut buf);
                let end = !matches!(r, Ok(n) if n > 0);
                if tx.send(said(r, &buf)).is_err() || end {
                    return;
                }
            }
        });
        for i in 0..3 {
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(r) => println!("{label}: File::read {i}: {r}"),
                Err(_) => {
                    println!("{label}: File::read {i}: still waiting after 2 s");
                    break;
                }
            }
        }
        println!(
            "{label}: the client writes on: {:?}",
            writer.write(b"up").map_err(|e| e.kind())
        );
        let mut buf = [0u8; 16];
        let got = read(&server, &mut buf).map(|n| String::from_utf8_lossy(&buf[..n as usize]).into_owned());
        println!("{label}: the server reads it: {got:?}");
        // A read still waiting ends with the server's close.
        drop(server);
        while let Ok(r) = rx.recv_timeout(Duration::from_secs(2)) {
            println!("{label}: once the server has closed: File::read {r}");
        }
        let _ = reading.join();
        Ok(())
    }

    /// Puts a client's end of a pipe in message read mode, as a client could at its open.
    fn read_mode_message(file: &std::fs::File) -> Result<(), u32> {
        let mode = PIPE_READMODE_MESSAGE;
        // SAFETY: a pipe handle the File owns; the mode read from a local, the other two left as they are.
        if unsafe { SetNamedPipeHandleState(file.as_raw_handle(), &raw const mode, null(), null()) } == 0 {
            Err(last())
        } else {
            Ok(())
        }
    }

    /// (f) (d)'s messages read by `std::fs::File` in message read mode, and (g) a message larger
    /// than its buffer among them.
    fn message_mode(tag: &str) -> Result<(), String> {
        use std::io::{Read as _, Write as _};
        let long = [b'x'; 100];
        let name = format!(r"\\.\pipe\s0-msg-{tag}");
        let server = message_server(&name)?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name)
            .map_err(|e| format!("std's open: {e}"))?;
        println!(
            "S0 msg: the client's end in message read mode: {:?}",
            read_mode_message(&file)
        );
        let wrote = (
            write(&server, b"abc"),
            write(&server, b""),
            write(&server, b"def"),
            write(&server, &long),
        );
        println!("S0 msg: server writes {wrote:?}");
        let mut buf = [0u8; 16];
        for i in 0..6 {
            let r = file.read(&mut buf);
            println!("S0 msg: File::read {i}: {}", said(r, &buf));
        }
        let mut rest = Vec::new();
        let mut more = 0u32;
        let mut refused = None;
        while available_client(&file) > 0 {
            match file.read(&mut buf) {
                Ok(n) => rest.extend_from_slice(&buf[..n]),
                Err(e) if e.raw_os_error() == Some(ERROR_MORE_DATA as i32) => more += 1,
                Err(e) => {
                    refused = Some((e.kind(), e.raw_os_error()));
                    break;
                }
            }
        }
        println!(
            "S0 msg: {} more bytes after them as Ok, {more} reads answered ERROR_MORE_DATA, {} left unread{}",
            rest.len(),
            available_client(&file),
            refused
                .map(|r| format!(", a read refused {r:?}"))
                .unwrap_or_default()
        );
        println!(
            "S0 msg: File writes on: {:?}",
            file.write(b"up").map_err(|e| e.kind())
        );
        let got = read(&server, &mut buf).map(|n| String::from_utf8_lossy(&buf[..n as usize]).into_owned());
        println!("S0 msg: the server reads it: {got:?}");
        drop(server);
        let r = file.read(&mut buf);
        println!(
            "S0 msg: once the server has closed, File::read {} and File::write {:?}",
            said(r, &buf),
            file.write(b"late").map_err(|e| (e.kind(), e.raw_os_error()))
        );
        Ok(())
    }

    /// What a std File's pipe holds for it now.
    fn available_client(file: &std::fs::File) -> u32 {
        let mut n = 0u32;
        // SAFETY: a pipe handle the File owns; no bytes copied, its count read.
        unsafe {
            PeekNamedPipe(
                file.as_raw_handle(),
                null_mut(),
                0,
                null_mut(),
                &raw mut n,
                null_mut(),
            )
        };
        n
    }

    /// (c) A pended write: 1 MiB to a pipe of 64 KiB, its reader taking 64 KiB every 10 ms;
    /// then the same cancelled once its reader has taken three of them.
    fn pended(tag: &str) -> Result<(), String> {
        let name = format!(r"\\.\pipe\s0-pended-{tag}");
        let server = server(&name, None, true, true).map_err(|e| format!("its server: {e}"))?;
        let client = client(&name).map_err(|e| format!("its client: {e}"))?;
        // SAFETY: an unnamed manual-reset event.
        let event = unsafe { CreateEventW(null(), 1, 0, null()) };
        if event.is_null() {
            return Err(format!("CreateEventW: {}", last()));
        }
        // SAFETY: the event made above, owned from here on.
        let event = unsafe { OwnedHandle::from_raw_handle(event as RawHandle) };
        let data = vec![0x5au8; 1 << 20];
        let mut buf = vec![0u8; BUFFER as usize];
        for cancel in [false, true] {
            // SAFETY: an all-zero OVERLAPPED is a valid one; its event set below.
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.hEvent = event.as_raw_handle() as *mut c_void;
            let t = Instant::now();
            // SAFETY: `data` and `ov` outlive the write, waited out below.
            let r = unsafe {
                WriteFile(
                    server.as_raw_handle(),
                    data.as_ptr(),
                    data.len() as u32,
                    null_mut(),
                    &raw mut ov,
                )
            };
            let e = if r == 0 { last() } else { 0 };
            println!(
                "S0 pended (cancel {cancel}): WriteFile of 1 MiB to a pipe of 64 KiB returned {r} (error {e})"
            );
            let mut taken = 0usize;
            loop {
                let mut n = 0u32;
                // SAFETY: the write's OVERLAPPED; not waited on here.
                let done =
                    unsafe { GetOverlappedResult(server.as_raw_handle(), &raw const ov, &raw mut n, 0) };
                if done != 0 {
                    println!(
                        "S0 pended (cancel {cancel}): completed at {} ms, its reader having taken {taken}, {n} transferred",
                        t.elapsed().as_millis()
                    );
                    break;
                }
                let err = last();
                if err != ERROR_IO_INCOMPLETE {
                    println!("S0 pended (cancel {cancel}): failed {err} at {taken} taken, {n} transferred");
                    break;
                }
                if cancel && taken >= 3 * BUFFER as usize {
                    // SAFETY: the write's OVERLAPPED, still pending.
                    unsafe { CancelIoEx(server.as_raw_handle(), &raw const ov) };
                    let mut n = 0u32;
                    // SAFETY: as above, waited out.
                    let r =
                        unsafe { GetOverlappedResult(server.as_raw_handle(), &raw const ov, &raw mut n, 1) };
                    let err = if r == 0 { last() } else { 0 };
                    let mut after = 0usize;
                    while available(&client) > 0 {
                        after += read(&client, &mut buf).map_err(|e| format!("reading: {e}"))? as usize;
                    }
                    println!(
                        "S0 pended (cancel true): cancelled at {taken} taken: GetOverlappedResult {r} (error {err}), {n} transferred; its reader then took {after} more, {} in all",
                        taken + after
                    );
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
                taken += read(&client, &mut buf).map_err(|e| format!("reading: {e}"))? as usize;
            }
            // What the write left in the pipe, taken before the next.
            let mut rest = 0usize;
            while available(&client) > 0 {
                rest += read(&client, &mut buf).map_err(|e| format!("reading: {e}"))? as usize;
            }
            println!(
                "S0 pended (cancel {cancel}): {rest} bytes left in the pipe after it, {} read in all",
                taken + rest
            );
        }
        Ok(())
    }
}
