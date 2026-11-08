//! A section handed to a confined VM process by a process that did not spawn it (PM M343), before the virtio-fs
//! device's DAX window is built on Windows for a share another server serves (D189, D38). The server answers a
//! mapping with a section of the file (slates' Shards route; on Unix a descriptor, which Windows cannot pass on a
//! stream, M340), and M341 found the window itself works with any section placed in its placeholder. Can such a server
//! put the section in the VM process at all?
//!
//! - **The hand-off.** The server is a same-user process that did not spawn the VM. Knowing only its pid, can it open
//!   a VM process confined as D141 confines one (an AppContainer holding VidExo's capabilities, in a Job of one process,
//!   with the mitigations, M201) for PROCESS_DUP_HANDLE, and duplicate a section into it? Can the VM process map the
//!   section, read what the server wrote, and write what the server then reads? What does a duplication cost?
//! - **The other way.** Can the VM process open the server likewise? It must not.
//! - **Who the server is.** A server such as slates' daemon runs as a standard user at medium integrity, while the
//!   runner's own account is an administrator. So the hand-off runs again as a standard user where the workflow makes
//!   one (`SHARDS_PROBE_USER`, as M200 runs one), and each run says whether its account is elevated.

use std::ffi::c_void;
use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use whp_run::confine::{Container, job, spawn};
use whp_run::summary;
use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_CLOSE_SOURCE, DuplicateHandle, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    LocalFree, SetHandleInformation,
};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, GetAce, GetLengthSid, GetTokenInformation,
    PSECURITY_DESCRIPTOR, PSID, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL, ReadFile, WriteFile,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE,
    UnmapViewOfFile,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessWithLogonW, GetCurrentProcess, GetExitCodeProcess, LOGON_WITH_PROFILE, OpenProcess,
    OpenProcessToken, PROCESS_DUP_HANDLE, PROCESS_INFORMATION, ResumeThread, STARTUPINFOW,
    WaitForSingleObject,
};

/// A section's size: one DAX range (fs/fuse/dax.c's 2 MiB).
const RANGE: usize = 2 << 20;
/// What the server writes into the section before the VM maps it, and what the VM writes back.
const SERVER_WROTE: &[u8] = b"written by the server";
const VM_WROTE: &[u8] = b"written by the VM....";
/// The duplications timed.
const DUPLICATIONS: usize = 200;
/// How long the VM child may take.
const CHILD_WAIT_MS: u32 = 30_000;

fn last() -> String {
    std::io::Error::last_os_error().to_string()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

// ---------------------------------------------------------------------------------------
// Who can hand a VM process a section

/// The capability SIDs VidExo's DACL allows to read and write it (M200), which D141's VM process holds (as M203
/// finds them).
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

/// The bytes at the head of `view`, as text where they are.
fn head(view: *const u8, count: usize) -> String {
    // SAFETY: a mapped view of at least `count` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(view, count) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// The server: told only the VM process's pid. Opens it for PROCESS_DUP_HANDLE, makes a section, writes into it,
/// times DUPLICATIONS duplications into the VM (each closed there again), then duplicates one to keep and prints its
/// value; once told to check, prints what the section holds.
fn server(pid: u32) {
    let say = |line: String| {
        println!("{line}");
        let _ = std::io::stdout().flush();
    };
    let t = Instant::now();
    // SAFETY: plain arguments; the result is checked.
    let process = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, pid) };
    let opened = t.elapsed();
    if process.is_null() {
        say(format!("open: refused, {}", last()));
        return;
    }
    say(format!(
        "open: PROCESS_DUP_HANDLE granted in {:.2} us",
        opened.as_secs_f64() * 1e6
    ));
    // SAFETY: a pagefile-backed section of RANGE bytes; the result is checked.
    let section = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            RANGE as u32,
            std::ptr::null(),
        )
    };
    if section.is_null() {
        say(format!("section: {}", last()));
        return;
    }
    // SAFETY: the section just made, mapped whole.
    let view = unsafe { MapViewOfFile(section, FILE_MAP_ALL_ACCESS, 0, 0, RANGE) };
    if view.Value.is_null() {
        say(format!("view: {}", last()));
        return;
    }
    let bytes = view.Value.cast::<u8>();
    // SAFETY: a view of RANGE bytes, longer than what is written.
    unsafe { std::ptr::copy_nonoverlapping(SERVER_WROTE.as_ptr(), bytes, SERVER_WROTE.len()) };
    // SAFETY: no arguments.
    let me = unsafe { GetCurrentProcess() };
    let mut costs = Vec::with_capacity(DUPLICATIONS);
    for _ in 0..DUPLICATIONS {
        let mut value: HANDLE = std::ptr::null_mut();
        let t = Instant::now();
        // SAFETY: our section, into the process opened above; the value is the VM's.
        let ok = unsafe {
            DuplicateHandle(
                me,
                section,
                process,
                &raw mut value,
                FILE_MAP_READ | FILE_MAP_WRITE,
                0,
                0,
            )
        };
        costs.push(t.elapsed());
        if ok == 0 {
            say(format!("duplicate: refused, {}", last()));
            return;
        }
        // SAFETY: the value just made in the VM's table, closed there (the source closed, no target).
        unsafe {
            DuplicateHandle(
                process,
                value,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                0,
                DUPLICATE_CLOSE_SOURCE,
            )
        };
    }
    say(format!(
        "a section duplicated into the VM process (us): {}",
        summary(&mut costs)
    ));
    let mut value: HANDLE = std::ptr::null_mut();
    // SAFETY: as above; this one is kept for the VM.
    if unsafe {
        DuplicateHandle(
            me,
            section,
            process,
            &raw mut value,
            FILE_MAP_READ | FILE_MAP_WRITE,
            0,
            0,
        )
    } == 0
    {
        say(format!("duplicate: refused, {}", last()));
        return;
    }
    say(format!("value={}", value as usize));
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    say(format!(
        "server: the section now begins {:?}",
        head(bytes, SERVER_WROTE.len())
    ));
    // SAFETY: our view, section and process handle, each closed once.
    unsafe {
        UnmapViewOfFile(view);
        CloseHandle(section);
        CloseHandle(process);
    }
}

/// The VM process: its instructions on the pipe it was handed (`section=` a handle value, `server=` a pid). Maps the
/// section, reads it, writes back, and tries to open the server.
fn vm(pipe: usize) {
    let say = |line: String| {
        println!("vm: {line}");
        let _ = std::io::stdout().flush();
    };
    let mut buf = vec![0u8; 4096];
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
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0)
    };
    let section = field("section") as HANDLE;
    // SAFETY: the value the server duplicated into this process, mapped whole.
    let view = unsafe { MapViewOfFile(section, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, RANGE) };
    if section.is_null() {
        say("no section was given".into());
    } else if view.Value.is_null() {
        say(format!("map: refused, {}", last()));
    } else {
        let bytes = view.Value.cast::<u8>();
        say(format!(
            "mapped the section; it begins {:?}",
            head(bytes, SERVER_WROTE.len())
        ));
        // SAFETY: a view of RANGE bytes, longer than what is written.
        unsafe { std::ptr::copy_nonoverlapping(VM_WROTE.as_ptr(), bytes, VM_WROTE.len()) };
        say(format!("wrote {:?}", String::from_utf8_lossy(VM_WROTE)));
        // SAFETY: our view.
        unsafe { UnmapViewOfFile(view) };
    }
    let server = field("server") as u32;
    // SAFETY: plain arguments; the result is checked.
    let back = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, server) };
    if back.is_null() {
        say(format!(
            "opening the server for PROCESS_DUP_HANDLE: refused, {}",
            last()
        ));
    } else {
        say("opening the server for PROCESS_DUP_HANDLE: granted".into());
        // SAFETY: the handle just opened.
        unsafe { CloseHandle(back) };
    }
}

/// The spawner: the VM process confined as D141 confines one (or, with `plain`, an ordinary child, for a host that
/// keeps no AppContainer profiles), the server an ordinary child told only the VM's pid, and the value relayed.
fn handed(plain: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let (mut pr, mut pw): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: two handles for the call to fill.
    if unsafe { CreatePipe(&raw mut pr, &raw mut pw, std::ptr::null(), 0) } == 0 {
        return Err(format!("CreatePipe: {}", last()));
    }
    let container;
    let (process, thread, pid) = if plain {
        // The ordinary child inherits the pipe's read end (and only it: the write end stays this process's), and
        // blocks reading it until the instructions are written and the write end closed.
        // SAFETY: our pipe's read end.
        if unsafe { SetHandleInformation(pr, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(format!("SetHandleInformation: {}", last()));
        }
        let child = Command::new(&exe)
            .arg("vm-plain")
            .arg((pr as usize).to_string())
            .spawn()
            .map_err(|e| format!("spawn: {e}"))?;
        container = None;
        (None, None, child.id())
    } else {
        let caps = videxo_capabilities()?;
        let made = Container::new("m343", "vm")?;
        let job = job()?;
        let pi = spawn(&exe, &format!("vm {}", pr as usize), made.sid, &caps, job, &[pr])?;
        container = Some((made, job));
        (Some(pi.hProcess), Some(pi.hThread), pi.dwProcessId)
    };
    let mut server = Command::new(&exe)
        .arg("server")
        .arg(pid.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn the server: {e}"))?;
    let server_pid = server.id();
    let out = server.stdout.take().ok_or("the server's output")?;
    let mut lines = BufReader::new(out).lines();
    let mut value = None;
    for line in lines.by_ref() {
        let line = line.map_err(|e| format!("the server's output: {e}"))?;
        if let Some(v) = line.strip_prefix("value=") {
            value = Some(v.to_string());
            break;
        }
        println!("{line}");
        if line.contains("refused") {
            break;
        }
    }
    let given = format!(
        "section={}\nserver={server_pid}\n",
        value.as_deref().unwrap_or("0")
    );
    // SAFETY: our pipe ends; the write end closed so the VM's read ends.
    unsafe {
        let mut written = 0u32;
        WriteFile(
            pw,
            given.as_ptr(),
            given.len() as u32,
            &raw mut written,
            std::ptr::null_mut(),
        );
        CloseHandle(pw);
        pw = std::ptr::null_mut();
        if let Some(thread) = thread {
            ResumeThread(thread);
        }
    }
    let _ = pw;
    if let Some(process) = process {
        // SAFETY: the VM process's handle, which CreateProcessW gave.
        let waited = unsafe { WaitForSingleObject(process, CHILD_WAIT_MS) };
        let mut code = 0u32;
        // SAFETY: as above.
        unsafe { GetExitCodeProcess(process, &raw mut code) };
        println!("the VM process: wait {waited}, exit code {code}");
    } else {
        std::thread::sleep(Duration::from_millis(500));
    }
    if let Some(input) = server.stdin.as_mut() {
        let _ = input.write_all(b"check\n");
    }
    for line in lines {
        match line {
            Ok(line) => println!("{line}"),
            Err(_) => break,
        }
    }
    let _ = server.wait();
    // SAFETY: our pipe's read end.
    unsafe { CloseHandle(pr) };
    pr = std::ptr::null_mut();
    let _ = pr;
    if let Some((made, job)) = container {
        // SAFETY: the job ends what is left of the child.
        unsafe { CloseHandle(job) };
        drop(made);
    }
    Ok(())
}

/// Whether this process's token is elevated (an administrator's, as GitHub's runners run), where it says.
fn elevated() -> Option<bool> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: this process's token, for query; closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return None;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut written = 0u32;
    // SAFETY: a TOKEN_ELEVATION of its own size, for the call to fill.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &raw mut written,
        )
    };
    // SAFETY: the token opened above.
    unsafe { CloseHandle(token) };
    (ok != 0).then_some(elevation.TokenIsElevated != 0)
}

/// The hand-off again as a standard user, where the workflow made one (`SHARDS_PROBE_USER`, as M200 runs one): the
/// runner's own account is an administrator, and a server such as slates' daemon runs as a standard user at medium
/// integrity. The probe is copied where that user may run it, and its output is gathered from a file there.
fn handed_as_user(user: &str, password: &str) -> Result<(), String> {
    let public = std::path::PathBuf::from(r"C:\Users\Public\shards-m343");
    let exe = public.join("m343.exe");
    let out = public.join("out.txt");
    std::fs::create_dir_all(&public).map_err(|e| format!("{}: {e}", public.display()))?;
    let copied = std::env::current_exe()
        .and_then(|from| std::fs::copy(from, &exe).map(drop))
        .map_err(|e| format!("copy: {e}"));
    let ran = copied.and_then(|()| {
        let mut cmd = wide(&format!(
            "cmd.exe /c \"\"{}\" handed > \"{}\" 2>&1\"",
            exe.display(),
            out.display()
        ));
        let (user_w, domain, password_w, dir) = (
            wide(user),
            wide("."),
            wide(password),
            wide(&public.display().to_string()),
        );
        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        // SAFETY: names, a command line and a directory we own; startup info alive.
        let made = unsafe {
            CreateProcessWithLogonW(
                user_w.as_ptr(),
                domain.as_ptr(),
                password_w.as_ptr(),
                LOGON_WITH_PROFILE,
                std::ptr::null(),
                cmd.as_mut_ptr(),
                0,
                std::ptr::null(),
                dir.as_ptr(),
                &raw const si,
                &raw mut pi,
            )
        };
        if made == 0 {
            return Err(format!("CreateProcessWithLogonW: {}", last()));
        }
        let mut code = 0u32;
        // SAFETY: the child's handles, closed once it has ended.
        let waited = unsafe {
            let waited = WaitForSingleObject(pi.hProcess, CHILD_WAIT_MS * 2);
            GetExitCodeProcess(pi.hProcess, &raw mut code);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            waited
        };
        Ok((waited, code))
    });
    match ran {
        Ok((waited, code)) => {
            println!("standard user {user}: wait {waited}, exit code {code}");
            for line in std::fs::read_to_string(&out).unwrap_or_default().lines() {
                println!("standard user {user}: {line}");
            }
        }
        Err(e) => println!("standard user {user}: {e}"),
    }
    let _ = std::fs::remove_dir_all(&public);
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("server") => return server(args.get(2).and_then(|v| v.parse().ok()).unwrap_or(0)),
        Some("vm") | Some("vm-plain") => return vm(args.get(2).and_then(|v| v.parse().ok()).unwrap_or(0)),
        Some("handed") => {
            println!("this account elevated: {:?}", elevated());
            if let Err(e) = handed(false) {
                println!("M343 handed failed: {e}");
            }
            return;
        }
        _ => {}
    }
    let plain = args.iter().any(|a| a == "--plain");
    println!("M343: a section handed to the VM process by a process that did not spawn it");
    println!("this account elevated: {:?}", elevated());
    if let Err(e) = handed(plain) {
        println!("M343 handed failed: {e}");
    }
    if let (Ok(user), Ok(password)) = (
        std::env::var("SHARDS_PROBE_USER"),
        std::env::var("SHARDS_PROBE_PASSWORD"),
    ) && let Err(e) = handed_as_user(&user, &password)
    {
        println!("M343 handed as a standard user failed: {e}");
    }
}
