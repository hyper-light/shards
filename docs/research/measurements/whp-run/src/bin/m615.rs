//! Windows' AF_UNIX parent-path controls in a child made in an AppContainer as D141 makes the
//! VM process (PM M615; kiln's r735, ROOT's r769 and r777). At 0e7773c19c a VM booted under the
//! user's own `LOCALAPPDATA` (r777); under a stand-in, `C:\Users\corph\.cache\w1-long\` then 40
//! a's, 40 b's and `AppData\Local`, its VM process ended at its first socket, refused
//! WSAENETDOWN (10050; r769), as a bind is where its directory is no directory (M614's
//! UTF-16 case). What differs with the stand-in, told apart here:
//!
//! - **The folder.** CreateAppContainerProfile makes a profile's folders where
//!   SHGetKnownFolderPath reaches them (Microsoft's documentation), and Windows points a
//!   confined child's `TEMP` at `<LOCALAPPDATA>\Packages\<profile>\AC\Temp` of the
//!   `LOCALAPPDATA` it is given (M208; r735's refusal named it). For each `LOCALAPPDATA` (this
//!   process's own, a short stand-in and one as long as r769's), taken as this process's own
//!   first, as r769's spawner had it, a profile is made and where its folder is said:
//!   GetAppContainerFolderPath's answer, and whether `Packages\<profile>\AC\Temp` is beneath the
//!   `LOCALAPPDATA` given and beneath the user's own (CreateEnvironmentBlock's, as Windows makes
//!   a user's environment); and again once the profile is deleted.
//! - **The length.** A child in that profile (its Job and D141's mitigations, given
//!   `LOCALAPPDATA` and `SystemRoot` as shards gives them) says its variables and temporary
//!   directory, and whether that is there; makes a pair there as shards' pairs are made (each
//!   socket's directory set through the controls as UTF-8, its address the file name), and with
//!   the whole path in `sun_path` where it fits, as before 0e7773c19c; binds where no directory
//!   is; and sweeps the directory's length from 100 to 300 bytes in its container's own `Temp`
//!   (GetAppContainerFolderPath's). This process sweeps the same lengths unconfined.
//!
//! The child is this binary, and loads what M208's did: no shell32 or ole32, whose user32 a
//! process without Win32k may not start with (D141's mitigations). So GetAppContainerFolderPath's
//! string is left unfreed: CoTaskMemFree is ole32's.
//!
//! Run on the Windows host, from this directory: `cargo run --release --bin m615`.

use std::ffi::{OsStr, OsString, c_void};
use std::io::Read as _;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::io::FromRawHandle as _;
use std::path::{Path, PathBuf};

use whp_run::confine::{Container, job, spawn_with_env};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows_sys::Win32::Networking::WinSock::{
    AF_UNIX, INVALID_SOCKET, SIO_AF_UNIX_SETBINDPARENTPATH, SIO_AF_UNIX_SETCONNPARENTPATH, SOCK_STREAM,
    SOCKADDR_UN, SOCKET, WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSADATA, WSAGetLastError, WSAIoctl,
    WSASocketW, WSAStartup, accept, bind, closesocket, connect, listen,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::Isolation::{DeleteAppContainerProfile, GetAppContainerFolderPath};
use windows_sys::Win32::Security::{PSID, TOKEN_QUERY};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, INFINITE, OpenProcessToken, ResumeThread, WaitForSingleObject,
};

/// The directory lengths swept, in bytes: the 100 to 260 and past asked for, r769's VM
/// process's Temp (160), and the bytes about MAX_PATH's 260.
const SWEEP: [usize; 12] = [100, 140, 160, 180, 200, 220, 240, 250, 256, 260, 280, 300];

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

/// A socket made as shards_afunix makes its own: overlapped, inherited by no child.
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

/// Sets `code` on `s`, its input `input`, as shards_afunix sets its directory.
fn control(s: &Sock, code: u32, input: &[u8]) -> Result<(), i32> {
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

/// `dir` as shards_afunix gives it to the controls: absolute, without a separator at its end.
fn placed(dir: &Path) -> PathBuf {
    std::path::absolute(dir.join("x"))
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| dir.to_path_buf())
}

/// A listener at `name` in `dir` and a connect to it, each socket's directory set through the
/// controls as UTF-8 with its NUL and its address the file name, as shards_afunix makes them,
/// then an accept: what became of them, in words.
fn pair_in(dir: &Path, name: &str) -> String {
    let Some(text) = dir.to_str() else {
        return "not UTF-8".into();
    };
    let parent: Vec<u8> = text.bytes().chain([0]).collect();
    let Some(a) = address(name.as_bytes()) else {
        return "its name past sun_path".into();
    };
    let step = |what: &str, r: Result<(), i32>| r.map_err(|e| format!("{what} refused {e}"));
    let made = (|| -> Result<(), String> {
        let l = socket().map_err(|e| format!("socket refused {e}"))?;
        step(
            "the listener's control",
            control(&l, SIO_AF_UNIX_SETBINDPARENTPATH, &parent),
        )?;
        step("bind", bind_to(&l, &a))?;
        step("listen", listen_on(&l))?;
        let c = socket().map_err(|e| format!("socket refused {e}"))?;
        step(
            "the client's control",
            control(&c, SIO_AF_UNIX_SETCONNPARENTPATH, &parent),
        )?;
        step("connect", connect_to(&c, &a))?;
        accept_on(&l).map(drop).map_err(|e| format!("accept refused {e}"))
    })();
    match made {
        Ok(()) => format!(
            "bound, connected and accepted, its file there: {}",
            dir.join(name).exists()
        ),
        Err(e) => e,
    }
}

/// A directory beneath `base` of `len` bytes, its last name padded to land on it; `base` itself
/// where that is as long already.
fn nested(base: &Path, len: usize) -> PathBuf {
    let mut dir = base.to_path_buf();
    while dir.as_os_str().len() + 1 < len {
        let room = len - dir.as_os_str().len() - 1;
        dir.push("d".repeat(room.min(48)));
    }
    dir
}

/// At each length of [`SWEEP`], a directory made beneath `base` and a pair through it.
fn sweep(base: &Path, who: &str) {
    for len in SWEEP {
        let dir = nested(base, len);
        let said = match std::fs::create_dir_all(&dir) {
            Ok(()) => pair_in(&dir, &format!("s{len}.sock")),
            Err(e) => format!("its directory not made: {e}"),
        };
        println!("  {who}: sweep, {} bytes: {said}", dir.as_os_str().len());
    }
}

/// What the confined child says, a line each.
fn child(base: &Path) {
    let mut data = WSADATA::default();
    // SAFETY: a WSADATA for the call to fill.
    let started = unsafe { WSAStartup(0x0202, &raw mut data) };
    println!("  child: WSAStartup {started}");
    for k in ["LOCALAPPDATA", "TEMP", "TMP"] {
        let v = std::env::var_os(k).unwrap_or_default();
        println!("  child: {k}={}", v.to_string_lossy());
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "a confined child's temporary directory, the measurement"
    )]
    let tmp = placed(&std::env::temp_dir());
    println!(
        "  child: temp_dir {} ({} bytes), a directory there: {}",
        tmp.display(),
        tmp.as_os_str().len(),
        tmp.is_dir()
    );
    println!(
        "  child: a pair there, as shards' are made: {}",
        pair_in(&tmp, "t.sock")
    );
    let whole = tmp.join("w.sock");
    let said = match whole.to_str().and_then(|t| address(t.as_bytes())) {
        None => "past sun_path".to_string(),
        Some(a) => match socket() {
            Ok(l) => bind_to(&l, &a).map_or_else(|e| format!("bind refused {e}"), |()| "bound".into()),
            Err(e) => format!("socket refused {e}"),
        },
    };
    println!(
        "  child: a bind there with its whole path in sun_path, as before 0e7773c19c ({} bytes): {said}",
        whole.as_os_str().len()
    );
    let absent = base.join("absent");
    println!(
        "  child: a pair where no directory is ({}): {}",
        absent.display(),
        pair_in(&absent, "a.sock")
    );
    println!(
        "  child: the sweep's base, its container's own Temp {} ({} bytes), a directory there: {}",
        base.display(),
        base.as_os_str().len(),
        base.is_dir()
    );
    sweep(base, "child");
}

/// `vars` as a Unicode environment block, in the order given: `NAME=VALUE` and its NUL each,
/// and a NUL more.
fn block(vars: &[(&str, &OsStr)]) -> Vec<u16> {
    let mut b = Vec::new();
    for (k, v) in vars {
        b.extend(k.encode_utf16());
        b.push(u16::from(b'='));
        b.extend(v.encode_wide());
        b.push(0);
    }
    b.push(0);
    b
}

/// The NUL-terminated wide string at `at`, as a path.
///
/// # Safety
/// `at` is a NUL-terminated wide string, alive for the call.
unsafe fn wide_path(at: *const u16) -> PathBuf {
    // SAFETY: within the string, up to its NUL, as the caller vouches.
    let len = (0usize..).take_while(|&i| unsafe { *at.add(i) } != 0).count();
    // SAFETY: its `len` units, read above.
    PathBuf::from(OsString::from_wide(unsafe {
        std::slice::from_raw_parts(at, len)
    }))
}

/// The user's own `LOCALAPPDATA`, as Windows makes the user's environment
/// (CreateEnvironmentBlock for this process's token, none of this process's variables in it).
fn users_local_app_data() -> Result<PathBuf, String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: this process's pseudo-handle; a handle for the call to fill, closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(format!("OpenProcessToken: {}", std::io::Error::last_os_error()));
    }
    let mut env: *mut c_void = std::ptr::null_mut();
    // SAFETY: the token opened above; a pointer for the call to fill, the block destroyed below.
    let made = unsafe { CreateEnvironmentBlock(&raw mut env, token, 0) };
    let error = std::io::Error::last_os_error();
    // SAFETY: the token opened above, closed once.
    unsafe { CloseHandle(token) };
    if made == 0 {
        return Err(format!("CreateEnvironmentBlock: {error}"));
    }
    let mut found = None;
    let mut at = env.cast::<u16>().cast_const();
    loop {
        // SAFETY: within the block, whose entries each end in a NUL, an empty one its end.
        let entry = unsafe { wide_path(at) };
        let len = entry.as_os_str().encode_wide().count();
        if len == 0 {
            break;
        }
        if let Some((k, v)) = entry.to_str().and_then(|e| e.split_once('='))
            && k.eq_ignore_ascii_case("LOCALAPPDATA")
        {
            found = Some(PathBuf::from(v));
        }
        // SAFETY: past the entry and its NUL, still within the block.
        at = unsafe { at.add(len + 1) };
    }
    // SAFETY: the block CreateEnvironmentBlock made, destroyed once.
    unsafe { DestroyEnvironmentBlock(env) };
    found.ok_or_else(|| "no LOCALAPPDATA in the user's environment block".into())
}

/// Where Windows says the profile of `sid` keeps its local data (GetAppContainerFolderPath),
/// `...\Packages\<profile>\AC`. Its string is left unfreed: CoTaskMemFree is ole32's.
fn container_folder(sid: PSID) -> Result<PathBuf, String> {
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: the profile's SID; a string for the call to fill, freed below.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 {
        return Err(format!(
            "ConvertSidToStringSidW: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut path: *mut u16 = std::ptr::null_mut();
    // SAFETY: the SID as a NUL-terminated string; a string for the call to fill.
    let hr = unsafe { GetAppContainerFolderPath(text, &raw mut path) };
    // SAFETY: the string ConvertSidToStringSidW made, freed once.
    unsafe { LocalFree(text as HLOCAL) };
    if hr < 0 {
        return Err(format!("GetAppContainerFolderPath: {:#010x}", hr as u32));
    }
    // SAFETY: the NUL-terminated string the call made.
    Ok(unsafe { wide_path(path) })
}

fn shown(r: &Result<PathBuf, String>) -> String {
    match r {
        Ok(p) => format!("{} ({} bytes)", p.display(), p.as_os_str().len()),
        Err(e) => e.clone(),
    }
}

/// `Packages\<profile>\AC\Temp` beneath `root`.
fn temp_beneath(root: &Path, profile: &str) -> PathBuf {
    root.join("Packages").join(profile).join("AC").join("Temp")
}

/// One `LOCALAPPDATA`, this process's own now: a profile made under it, said where its folder
/// is, a child in it, and the profile deleted. The profile's name, for a look after.
fn case(
    exe: &Path,
    n: usize,
    local: &Path,
    users: &Result<PathBuf, String>,
    system_root: &OsStr,
) -> Option<Vec<u16>> {
    let container = match Container::new("m615", &n.to_string()) {
        Ok(c) => c,
        Err(e) => {
            println!("  {e}");
            return None;
        }
    };
    let profile = String::from_utf16_lossy(
        container
            .name
            .strip_suffix(&[0])
            .unwrap_or(container.name.as_slice()),
    );
    let folder = container_folder(container.sid);
    println!(
        "  profile {profile}: GetAppContainerFolderPath {}",
        shown(&folder)
    );
    println!(
        "  Packages\\{profile}\\AC\\Temp beneath the LOCALAPPDATA given: {}",
        temp_beneath(local, &profile).is_dir()
    );
    if let Ok(u) = users {
        println!(
            "  Packages\\{profile}\\AC\\Temp beneath the user's own: {}",
            temp_beneath(u, &profile).is_dir()
        );
    }
    let base = match (&folder, users) {
        (Ok(f), _) => f.join("Temp"),
        (Err(_), Ok(u)) => temp_beneath(u, &profile),
        (Err(_), Err(_)) => temp_beneath(local, &profile),
    };
    let job = match job() {
        Ok(j) => j,
        Err(e) => {
            println!("  {e}");
            return Some(container.name.clone());
        }
    };
    let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: two handles for the call to fill.
    if unsafe { CreatePipe(&raw mut read, &raw mut write, std::ptr::null(), 0) } == 0 {
        println!("  CreatePipe: {}", std::io::Error::last_os_error());
        // SAFETY: the job made above.
        unsafe { CloseHandle(job) };
        return Some(container.name.clone());
    }
    // As shards gives a confined child its environment (windows.rs KEPT): this process's
    // LOCALAPPDATA and SystemRoot, sorted by name.
    let env = block(&[("LOCALAPPDATA", local.as_os_str()), ("SystemRoot", system_root)]);
    let args = format!("child \"{}\"", base.display());
    let made = spawn_with_env(
        exe,
        &args,
        container.sid,
        &[],
        job,
        &[],
        write,
        Some(env.as_slice()),
    );
    // SAFETY: our pipe's write end, which the child has its own of now.
    unsafe { CloseHandle(write) };
    // SAFETY: our pipe's read end, owned by the file from here on.
    let mut from = unsafe { std::fs::File::from_raw_handle(read) };
    match made {
        Err(e) => println!("  {e}"),
        Ok(pi) => {
            let mut said = String::new();
            let mut code = 0u32;
            // SAFETY: the child's handles: its thread let run, its output read to its end, the
            // child waited for and its handles closed once.
            unsafe {
                ResumeThread(pi.hThread);
                let _ = from.read_to_string(&mut said);
                WaitForSingleObject(pi.hProcess, INFINITE);
                GetExitCodeProcess(pi.hProcess, &raw mut code);
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
            }
            print!("{said}");
            println!("  child's exit code: {code:#x}");
        }
    }
    // SAFETY: the job made above.
    unsafe { CloseHandle(job) };
    let name = container.name.clone();
    drop(container);
    println!(
        "  deleted under it: Temp beneath the LOCALAPPDATA given {}, beneath the user's own {}",
        temp_beneath(local, &profile).is_dir(),
        users.as_ref().is_ok_and(|u| temp_beneath(u, &profile).is_dir())
    );
    Some(name)
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "child") {
        child(&args.get(2).map(PathBuf::from).unwrap_or_default());
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        println!("no current_exe");
        return;
    };
    let mut data = WSADATA::default();
    // SAFETY: a WSADATA for the call to fill.
    let started = unsafe { WSAStartup(0x0202, &raw mut data) };
    println!("M615 this process {}: WSAStartup {started}", std::process::id());
    let ours = std::env::var_os("LOCALAPPDATA").unwrap_or_default();
    let system_root = std::env::var_os("SystemRoot").unwrap_or_default();
    let users = users_local_app_data();
    println!("M615 this process's LOCALAPPDATA: {}", ours.to_string_lossy());
    println!("M615 the user's own (CreateEnvironmentBlock): {}", shown(&users));
    let home = PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default());
    let stand_ins = home.join(".cache").join(format!("m615-{}", std::process::id()));
    let cases = [
        ("this process's own", PathBuf::from(&ours)),
        (
            "a short stand-in",
            stand_ins.join("s").join("AppData").join("Local"),
        ),
        (
            "a stand-in as long as r769's",
            stand_ins
                .join("a".repeat(40))
                .join("b".repeat(40))
                .join("AppData")
                .join("Local"),
        ),
    ];
    let mut made = Vec::new();
    for (n, (label, local)) in cases.iter().enumerate() {
        println!(
            "M615 LOCALAPPDATA {label}: {} ({} bytes)",
            local.display(),
            local.as_os_str().len()
        );
        if let Err(e) = std::fs::create_dir_all(local) {
            println!("  not made: {e}");
            continue;
        }
        // As r769's spawner had it: its own LOCALAPPDATA the one it makes profiles under and
        // gives its children.
        // SAFETY: this process's one thread; nothing else reads its environment meanwhile.
        unsafe { std::env::set_var("LOCALAPPDATA", local) };
        if let Some(name) = case(&exe, n, local, &users, &system_root) {
            made.push((name, local.clone()));
        }
        // SAFETY: as above.
        unsafe { std::env::set_var("LOCALAPPDATA", &ours) };
    }
    // Each profile deleted again under this process's own LOCALAPPDATA, and looked for.
    for (name, local) in &made {
        // SAFETY: a NUL-terminated name.
        let hr = unsafe { DeleteAppContainerProfile(name.as_ptr()) };
        let profile = String::from_utf16_lossy(name.strip_suffix(&[0]).unwrap_or(name.as_slice()));
        println!(
            "M615 {profile} deleted again under this process's own LOCALAPPDATA ({:#010x}): Temp beneath the LOCALAPPDATA it was made under {}, beneath the user's own {}",
            hr as u32,
            temp_beneath(local, &profile).is_dir(),
            users.as_ref().is_ok_and(|u| temp_beneath(u, &profile).is_dir())
        );
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "this process's own temporary directory, the unconfined sweep's"
    )]
    let base = placed(&std::env::temp_dir()).join(format!("m615-{}", std::process::id()));
    println!(
        "M615 unconfined, beneath {} ({} bytes):",
        base.display(),
        base.as_os_str().len()
    );
    sweep(&base, "unconfined");
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&stand_ins);
}
