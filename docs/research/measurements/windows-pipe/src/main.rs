//! A Windows named pipe as the stream of a share another server serves (PM M340), before
//! the virtio-fs device is built on Windows over one. slates' `slates shards VOLUME -- shards
//! run …` hands its VM the stream its daemon serves a volume on, as `--slates-fs-fd` does on
//! Unix, and its daemon writes nothing to disk outside a granted landing, so the pair must be
//! made without a path in the filesystem:
//!
//! - whether an AF_UNIX socket bound at an abstract address (`sun_path` starting with NUL)
//!   can be connected to, which a pair made without a path would need;
//! - a duplex pipe pair made under a fresh name, its first instance only, one instance,
//!   remote clients refused: its cost, and whether a second client or a second server
//!   instance of the name is refused once it is connected;
//! - the name each handle to an end reports (GetFileInformationByHandleEx, FileNameInfo):
//!   the server end's, the client end's, a duplicate's and a child's inherited copy;
//! - what a reader gets after its writer closes its end, and after its writer's process
//!   exits holding it (M199: an AF_UNIX peer loses what it had not read);
//! - throughput (1 GiB in 64 KiB writes, through 64 KiB and 1 MiB pipe buffers) and a
//!   one-byte round trip's latency, beside loopback TCP's.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_BROKEN_PIPE, ERROR_PIPE_BUSY,
    ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, HANDLE_FLAG_INHERIT,
    INVALID_HANDLE_VALUE, SetHandleInformation,
};
use windows_sys::Win32::Networking::WinSock::{
    AF_UNIX, INVALID_SOCKET, SOCK_STREAM, SOCKADDR, SOCKADDR_UN, WSA_FLAG_NO_HANDLE_INHERIT,
    WSA_FLAG_OVERLAPPED, WSADATA, WSAGetLastError, WSASocketW, WSAStartup, bind, closesocket, connect,
    listen,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_NAME_INFO, FileNameInfo, GetFileInformationByHandleEx,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const ROUNDS: usize = 10_000;
const PAIRS: usize = 1_000;
const VOLUME: usize = 1 << 30;
const CHUNK: usize = 64 << 10;
const BIG_BUFFER: u32 = 1 << 20;
const EXIT_BYTES: usize = 1 << 20;
const REPEATS: usize = 3;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn last(what: &str) -> String {
    // SAFETY: no arguments.
    format!("{what}: error {}", unsafe { GetLastError() })
}

fn summary(v: &mut [Duration]) -> String {
    v.sort();
    let n = v.len();
    if n == 0 {
        return "n=0".into();
    }
    let rank = |p: usize| v[(n * p).div_ceil(100).clamp(1, n) - 1].as_secs_f64() * 1e6;
    format!(
        "n={n} p50={:.2} p90={:.2} p99={:.2} max={:.2}",
        rank(50),
        rank(90),
        rank(99),
        rank(100)
    )
}

// ---------------------------------------------------------------------------------------
// An AF_UNIX socket at an abstract address

fn abstract_unix() -> Result<(), String> {
    let mut data = WSADATA::default();
    // SAFETY: a WSADATA for the call to fill.
    let started = unsafe { WSAStartup(0x0202, &raw mut data) };
    if started != 0 {
        return Err(format!("WSAStartup: {started}"));
    }
    let name = format!("shards-m340-{}", std::process::id());
    for (form, whole) in [
        ("the length covers the name", false),
        ("the length is the whole sockaddr_un", true),
    ] {
        let mut address = SOCKADDR_UN {
            sun_family: AF_UNIX,
            sun_path: [0; 108],
        };
        for (slot, byte) in address.sun_path.iter_mut().skip(1).zip(name.bytes()) {
            *slot = byte as _;
        }
        let length = if whole {
            std::mem::size_of::<SOCKADDR_UN>()
        } else {
            std::mem::size_of::<u16>() + 1 + name.len()
        } as i32;
        let flags = WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT;
        let at = (&raw const address).cast::<SOCKADDR>();
        // SAFETY: plain arguments.
        let listener = unsafe { WSASocketW(i32::from(AF_UNIX), SOCK_STREAM, 0, std::ptr::null(), 0, flags) };
        if listener == INVALID_SOCKET {
            // SAFETY: no arguments.
            return Err(format!("socket: WSA error {}", unsafe { WSAGetLastError() }));
        }
        // SAFETY: the address and its length, as built above.
        let bound = unsafe { bind(listener, at, length) };
        // SAFETY: no arguments.
        let bind_error = if bound == 0 {
            0
        } else {
            unsafe { WSAGetLastError() }
        };
        // SAFETY: a socket of this process.
        let listened = if bound == 0 {
            unsafe { listen(listener, 8) }
        } else {
            -1
        };
        // SAFETY: no arguments.
        let listen_error = if listened == 0 {
            0
        } else {
            unsafe { WSAGetLastError() }
        };
        // SAFETY: plain arguments.
        let client = unsafe { WSASocketW(i32::from(AF_UNIX), SOCK_STREAM, 0, std::ptr::null(), 0, flags) };
        // SAFETY: the address and its length, as built above.
        let connected = unsafe { connect(client, at, length) };
        // SAFETY: no arguments.
        let connect_error = if connected == 0 {
            0
        } else {
            unsafe { WSAGetLastError() }
        };
        println!(
            "abstract address, {form} (addrlen {length}): bind {bound} (error {bind_error}), listen {listened} (error {listen_error}), connect {connected} (error {connect_error})"
        );
        // SAFETY: sockets of this process, closed once each.
        unsafe {
            closesocket(client);
            closesocket(listener);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// A duplex pipe pair

/// A connected pair, closed as it drops.
struct Pair {
    server: HANDLE,
    client: HANDLE,
    name: String,
}

impl Drop for Pair {
    fn drop(&mut self) {
        for handle in [self.server, self.client] {
            if !handle.is_null() {
                // SAFETY: a handle this value owns, closed once.
                unsafe { CloseHandle(handle) };
            }
        }
    }
}

fn fresh_name(n: usize) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(r"\\.\pipe\shards-m340-{}-{n}-{nanos:x}", std::process::id())
}

fn open_client(name: &[u16]) -> HANDLE {
    // SAFETY: a NUL-terminated name; plain arguments.
    unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    }
}

/// A pair as a server would make one for a peer it hands the client end to: the server end
/// the name's first instance and its only one, remote clients refused; the client end opened
/// by name; the server's connect answered at once (ERROR_PIPE_CONNECTED).
fn pair(n: usize, buffer: u32) -> Result<Pair, String> {
    let name = fresh_name(n);
    let wide_name = wide(&name);
    // SAFETY: a NUL-terminated name; plain arguments.
    let server = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            buffer,
            buffer,
            0,
            std::ptr::null(),
        )
    };
    if server == INVALID_HANDLE_VALUE {
        return Err(last("CreateNamedPipeW"));
    }
    let mut made = Pair {
        server,
        client: std::ptr::null_mut(),
        name,
    };
    let client = open_client(&wide_name);
    if client == INVALID_HANDLE_VALUE {
        return Err(last("CreateFileW"));
    }
    made.client = client;
    // SAFETY: the server end of this pair, synchronous.
    if unsafe { ConnectNamedPipe(made.server, std::ptr::null_mut()) } == 0 {
        // SAFETY: no arguments.
        let error = unsafe { GetLastError() };
        if error != ERROR_PIPE_CONNECTED {
            return Err(format!("ConnectNamedPipe: error {error}"));
        }
    }
    Ok(made)
}

fn name_of(handle: HANDLE) -> String {
    let mut buffer = vec![0u8; 4096];
    // SAFETY: a buffer of the size given, for the call to fill.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileNameInfo,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
        )
    };
    if ok == 0 {
        return last("GetFileInformationByHandleEx");
    }
    let info = buffer.as_ptr().cast::<FILE_NAME_INFO>();
    // SAFETY: the call wrote a FILE_NAME_INFO at the buffer's head, its name within the buffer.
    let units = unsafe {
        let length = ((*info).FileNameLength as usize / 2).min(buffer.len() / 2);
        std::slice::from_raw_parts(std::ptr::addr_of!((*info).FileName).cast::<u16>(), length)
    };
    String::from_utf16_lossy(units)
}

fn refusals_and_names() -> Result<(), String> {
    let made = pair(0, CHUNK as u32)?;
    let wide_name = wide(&made.name);
    let second = open_client(&wide_name);
    if second == INVALID_HANDLE_VALUE {
        // SAFETY: no arguments.
        let error = unsafe { GetLastError() };
        let named = if error == ERROR_PIPE_BUSY {
            " (ERROR_PIPE_BUSY)"
        } else {
            ""
        };
        println!("a second client of the connected pair: refused, error {error}{named}");
    } else {
        println!("a second client of the connected pair: opened");
        // SAFETY: the handle just opened, closed once.
        unsafe { CloseHandle(second) };
    }
    // SAFETY: a NUL-terminated name; plain arguments.
    let another = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            255,
            CHUNK as u32,
            CHUNK as u32,
            0,
            std::ptr::null(),
        )
    };
    if another == INVALID_HANDLE_VALUE {
        println!(
            "another server instance of the name: refused, {}",
            last("CreateNamedPipeW")
        );
    } else {
        println!("another server instance of the name: made");
        // SAFETY: the handle just made, closed once.
        unsafe { CloseHandle(another) };
    }
    println!("the pipe: {}", made.name);
    println!("the server end's name: {}", name_of(made.server));
    println!("the client end's name: {}", name_of(made.client));
    let mut duplicate: HANDLE = std::ptr::null_mut();
    // SAFETY: no arguments.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: a handle of this process duplicated within it, for the call to fill.
    let ok = unsafe {
        DuplicateHandle(
            process,
            made.client,
            process,
            &raw mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        return Err(last("DuplicateHandle"));
    }
    println!("a duplicate of the client end's name: {}", name_of(duplicate));
    // SAFETY: the duplicate just made, closed once.
    unsafe { CloseHandle(duplicate) };
    Ok(())
}

fn pair_cost() -> Result<(), String> {
    let mut costs = Vec::with_capacity(PAIRS);
    for n in 1..=PAIRS {
        let t = Instant::now();
        let made = pair(n, CHUNK as u32)?;
        costs.push(t.elapsed());
        drop(made);
    }
    println!("a pair, made and connected (us): {}", summary(&mut costs));
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Throughput and round trips

fn write_all(handle: HANDLE, mut bytes: &[u8]) -> Result<(), String> {
    while !bytes.is_empty() {
        let mut written = 0u32;
        // SAFETY: the bytes and their length; synchronous.
        let ok = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr(),
                bytes.len() as u32,
                &raw mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last("WriteFile"));
        }
        bytes = bytes.get(written as usize..).unwrap_or_default();
    }
    Ok(())
}

/// What a read got: bytes, or the error that ended it.
fn read_some(handle: HANDLE, buffer: &mut [u8]) -> Result<usize, u32> {
    let mut read = 0u32;
    // SAFETY: the buffer and its length; synchronous.
    let ok = unsafe {
        ReadFile(
            handle,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            &raw mut read,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        // SAFETY: no arguments.
        return Err(unsafe { GetLastError() });
    }
    Ok(read as usize)
}

fn pipe_throughput(buffer: u32) -> Result<(), String> {
    let made = pair(PAIRS + 1, buffer)?;
    let writer = made.client as usize;
    let t = Instant::now();
    let thread = std::thread::spawn(move || -> Result<(), String> {
        let chunk = vec![7u8; CHUNK];
        for _ in 0..VOLUME / CHUNK {
            write_all(writer as HANDLE, &chunk)?;
        }
        Ok(())
    });
    let mut landing = vec![0u8; CHUNK];
    let mut total = 0;
    while total < VOLUME {
        total += read_some(made.server, &mut landing).map_err(|e| format!("ReadFile: error {e}"))?;
    }
    thread
        .join()
        .map_err(|_| "the writer ended abnormally".to_string())??;
    println!(
        "pipe throughput, {buffer}-byte pipe buffers: {:.0} MB/s",
        VOLUME as f64 / t.elapsed().as_secs_f64() / 1e6
    );
    Ok(())
}

fn pipe_round_trips() -> Result<(), String> {
    let made = pair(PAIRS + 2, CHUNK as u32)?;
    let echo = made.client as usize;
    let thread = std::thread::spawn(move || -> Result<(), String> {
        let mut byte = [0u8; 1];
        for _ in 0..ROUNDS {
            read_some(echo as HANDLE, &mut byte).map_err(|e| format!("echo ReadFile: error {e}"))?;
            write_all(echo as HANDLE, &byte)?;
        }
        Ok(())
    });
    let mut samples = Vec::with_capacity(ROUNDS);
    let mut byte = [1u8; 1];
    for _ in 0..ROUNDS {
        let t = Instant::now();
        write_all(made.server, &byte)?;
        read_some(made.server, &mut byte).map_err(|e| format!("ReadFile: error {e}"))?;
        samples.push(t.elapsed());
    }
    thread
        .join()
        .map_err(|_| "the echo ended abnormally".to_string())??;
    println!("pipe one-byte round trip (us): {}", summary(&mut samples));
    Ok(())
}

fn tcp_pair() -> Result<(std::net::TcpStream, std::net::TcpStream), String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind: {e}"))?;
    let address = listener.local_addr().map_err(|e| format!("local_addr: {e}"))?;
    let client = std::net::TcpStream::connect(address).map_err(|e| format!("connect: {e}"))?;
    let (server, _) = listener.accept().map_err(|e| format!("accept: {e}"))?;
    for stream in [&client, &server] {
        stream
            .set_nodelay(true)
            .map_err(|e| format!("TCP_NODELAY: {e}"))?;
    }
    Ok((server, client))
}

fn tcp_throughput() -> Result<(), String> {
    let (mut server, mut client) = tcp_pair()?;
    let t = Instant::now();
    let thread = std::thread::spawn(move || -> Result<(), String> {
        let chunk = vec![7u8; CHUNK];
        for _ in 0..VOLUME / CHUNK {
            client.write_all(&chunk).map_err(|e| format!("send: {e}"))?;
        }
        Ok(())
    });
    let mut landing = vec![0u8; CHUNK];
    let mut total = 0;
    while total < VOLUME {
        total += server.read(&mut landing).map_err(|e| format!("recv: {e}"))?;
    }
    thread
        .join()
        .map_err(|_| "the writer ended abnormally".to_string())??;
    println!(
        "loopback TCP throughput: {:.0} MB/s",
        VOLUME as f64 / t.elapsed().as_secs_f64() / 1e6
    );
    Ok(())
}

fn tcp_round_trips() -> Result<(), String> {
    let (mut server, mut client) = tcp_pair()?;
    let thread = std::thread::spawn(move || -> Result<(), String> {
        let mut byte = [0u8; 1];
        for _ in 0..ROUNDS {
            client
                .read_exact(&mut byte)
                .map_err(|e| format!("echo recv: {e}"))?;
            client.write_all(&byte).map_err(|e| format!("echo send: {e}"))?;
        }
        Ok(())
    });
    let mut samples = Vec::with_capacity(ROUNDS);
    let mut byte = [1u8; 1];
    for _ in 0..ROUNDS {
        let t = Instant::now();
        server.write_all(&byte).map_err(|e| format!("send: {e}"))?;
        server.read_exact(&mut byte).map_err(|e| format!("recv: {e}"))?;
        samples.push(t.elapsed());
    }
    thread
        .join()
        .map_err(|_| "the echo ended abnormally".to_string())??;
    println!("loopback TCP one-byte round trip (us): {}", summary(&mut samples));
    Ok(())
}

// ---------------------------------------------------------------------------------------
// End of stream

/// Everything readable from `handle`, and the error that ended it (0 for a read of nothing).
fn drain(handle: HANDLE) -> (usize, u32) {
    let mut landing = vec![0u8; CHUNK];
    let mut total = 0;
    loop {
        match read_some(handle, &mut landing) {
            Ok(0) => return (total, 0),
            Ok(n) => total += n,
            Err(error) => return (total, error),
        }
    }
}

fn ended(error: u32) -> &'static str {
    if error == ERROR_BROKEN_PIPE {
        " (ERROR_BROKEN_PIPE)"
    } else {
        ""
    }
}

fn end_of_stream() -> Result<(), String> {
    let mut made = pair(PAIRS + 3, CHUNK as u32)?;
    let writer = made.client as usize;
    made.client = std::ptr::null_mut();
    let thread = std::thread::spawn(move || -> Result<(), String> {
        let written = write_all(writer as HANDLE, &vec![9u8; EXIT_BYTES]);
        // SAFETY: the client end, moved here and closed once.
        unsafe { CloseHandle(writer as HANDLE) };
        written
    });
    let (read, error) = drain(made.server);
    thread
        .join()
        .map_err(|_| "the writer ended abnormally".to_string())??;
    println!(
        "its writer closed: read {read} of {EXIT_BYTES} bytes, then error {error}{}",
        ended(error)
    );

    let mut made = pair(PAIRS + 4, CHUNK as u32)?;
    // SAFETY: the client end of this pair, made inheritable for the child.
    if unsafe { SetHandleInformation(made.client, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
        return Err(last("SetHandleInformation"));
    }
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let child = std::process::Command::new(exe)
        .arg("child-exit")
        .arg((made.client as usize).to_string())
        .arg(&made.name)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    // SAFETY: this process's copy of the client end, closed once; the child holds its own.
    unsafe { CloseHandle(made.client) };
    made.client = std::ptr::null_mut();
    let (read, error) = drain(made.server);
    let output = child.wait_with_output().map_err(|e| format!("wait: {e}"))?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    println!(
        "its writer's process exited holding its end: read {read} of {EXIT_BYTES} bytes, then error {error}{}, the child {}",
        ended(error),
        output.status
    );
    Ok(())
}

/// The child: the name its inherited end reports, then `EXIT_BYTES` written and an exit
/// that closes nothing first.
fn child(value: Option<&String>, name: Option<&String>) {
    let Some(handle) = value.and_then(|v| v.parse::<usize>().ok()) else {
        println!("the child: no handle value");
        std::process::exit(2);
    };
    let handle = handle as HANDLE;
    println!(
        "a child's inherited end's name: {} (the pipe: {})",
        name_of(handle),
        name.map_or("?", String::as_str)
    );
    let _ = std::io::stdout().flush();
    let written = write_all(handle, &vec![9u8; EXIT_BYTES]);
    std::process::exit(if written.is_ok() { 0 } else { 1 });
}

fn report(what: &str, outcome: Result<(), String>) {
    if let Err(e) = outcome {
        println!("M340 {what} failed: {e}");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("child-exit") {
        child(args.get(2), args.get(3));
        return;
    }
    report("abstract AF_UNIX", abstract_unix());
    report("refusals and names", refusals_and_names());
    report("pair cost", pair_cost());
    report("end of stream", end_of_stream());
    for round in 1..=REPEATS {
        println!("round {round} of {REPEATS}");
        report("pipe throughput", pipe_throughput(CHUNK as u32));
        report("pipe throughput, 1 MiB buffers", pipe_throughput(BIG_BUFFER));
        report("TCP throughput", tcp_throughput());
        report("pipe round trips", pipe_round_trips());
        report("TCP round trips", tcp_round_trips());
    }
}
