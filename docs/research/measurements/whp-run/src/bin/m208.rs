//! What environment a child made in an AppContainer, as shards makes its VM and network
//! processes (D141, D142), must be given, and what it has then (PM M208). shards gives its
//! Unix children `SHARDS_LOG` and `SHARDS_TIMING` alone, and on Windows `SystemRoot`
//! besides, which Winsock needs; CreateProcessW refused every such child
//! (ERROR_ENVVAR_NOT_FOUND, 203; CI run 38256632140). So: a child made in an AppContainer
//! of its own, in a Job, under D141's mitigations, given each of several environments
//! (this process's own, none, and `SystemRoot` with each of `LOCALAPPDATA`, `USERPROFILE`,
//! `TEMP` and `TMP` and without them), says whether it was made, and if it was, every
//! variable it has, its temporary directory and whether it may write a file there, whether
//! Winsock starts in it, and whether it makes an AF_UNIX and an AF_INET socket.

use std::io::{Read as _, Write as _};
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::FromRawHandle as _;
use std::time::Duration;

use whp_run::confine::{Container, job, spawn_with_env};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_UNIX, INVALID_SOCKET, SOCK_STREAM, WSA_FLAG_OVERLAPPED, WSADATA, WSAGetLastError, WSASocketW,
    WSAStartup, closesocket,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{ResumeThread, TerminateProcess, WaitForSingleObject};

/// What the child says of itself, a line each.
fn child() {
    let mut out = std::io::stdout();
    let mut vars: Vec<(String, String)> = std::env::vars_os()
        .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
        .collect();
    vars.sort();
    for (k, v) in vars {
        let _ = writeln!(out, "  var {k}={v}");
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "what a child's temporary directory is, the measurement"
    )]
    let tmp = std::env::temp_dir();
    let wrote = std::fs::write(tmp.join(format!("m208-{}", std::process::id())), b"x")
        .map(|()| "written".to_string())
        .unwrap_or_else(|e| e.to_string());
    let _ = writeln!(out, "  temp_dir {} ({wrote})", tmp.display());
    let mut data = WSADATA::default();
    // SAFETY: a WSADATA for the call to fill.
    let started = unsafe { WSAStartup(0x0202, &raw mut data) };
    let _ = writeln!(out, "  WSAStartup {started}");
    // A socket of each family shards makes: its provider is loaded as one is made.
    for (name, family) in [("AF_UNIX", AF_UNIX), ("AF_INET", AF_INET)] {
        // SAFETY: plain arguments; the socket closed below.
        let s = unsafe {
            WSASocketW(
                i32::from(family),
                SOCK_STREAM,
                0,
                std::ptr::null(),
                0,
                WSA_FLAG_OVERLAPPED,
            )
        };
        if s == INVALID_SOCKET {
            // SAFETY: no arguments.
            let _ = writeln!(out, "  socket {name}: WSA error {}", unsafe { WSAGetLastError() });
        } else {
            let _ = writeln!(out, "  socket {name}: made");
            // SAFETY: the socket just made.
            unsafe { closesocket(s) };
        }
    }
}

/// An environment given a child: each variable's name and value; `None`, this process's own.
type Given = Option<Vec<(&'static str, std::ffi::OsString)>>;

/// `vars` as a Unicode environment block: `NAME=VALUE` and its NUL each, and a NUL more.
fn block(vars: &[(&str, std::ffi::OsString)]) -> Vec<u16> {
    let mut b = Vec::new();
    for (k, v) in vars {
        b.extend(k.encode_utf16());
        b.push(u16::from(b'='));
        b.extend(v.encode_wide());
        b.push(0);
    }
    if b.is_empty() {
        b.push(0);
    }
    b.push(0);
    b
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("child") {
        child();
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        println!("no current_exe");
        return;
    };
    let ours = |k: &str| std::env::var_os(k).unwrap_or_default();
    let with = |names: &[&'static str]| -> Vec<(&'static str, std::ffi::OsString)> {
        names.iter().map(|&k| (k, ours(k))).collect()
    };
    let cases: Vec<(&str, Given)> = vec![
        ("this process's", None),
        ("empty", Some(Vec::new())),
        ("SystemRoot", Some(with(&["SystemRoot"]))),
        (
            "SystemRoot LOCALAPPDATA",
            Some(with(&["SystemRoot", "LOCALAPPDATA"])),
        ),
        (
            "SystemRoot USERPROFILE",
            Some(with(&["SystemRoot", "USERPROFILE"])),
        ),
        (
            "SystemRoot LOCALAPPDATA USERPROFILE",
            Some(with(&["SystemRoot", "LOCALAPPDATA", "USERPROFILE"])),
        ),
        (
            "SystemRoot LOCALAPPDATA USERPROFILE TEMP TMP",
            Some(with(&[
                "SystemRoot",
                "LOCALAPPDATA",
                "USERPROFILE",
                "TEMP",
                "TMP",
            ])),
        ),
        (
            "LOCALAPPDATA USERPROFILE",
            Some(with(&["LOCALAPPDATA", "USERPROFILE"])),
        ),
        ("LOCALAPPDATA", Some(with(&["LOCALAPPDATA"]))),
    ];
    for (n, (label, vars)) in cases.into_iter().enumerate() {
        println!("M208 {label}:");
        let container = match Container::new("m208", &n.to_string()) {
            Ok(c) => c,
            Err(e) => {
                println!("  {e}");
                continue;
            }
        };
        let job = match job() {
            Ok(j) => j,
            Err(e) => {
                println!("  {e}");
                continue;
            }
        };
        let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: two handles for the call to fill.
        if unsafe { CreatePipe(&raw mut read, &raw mut write, std::ptr::null(), 0) } == 0 {
            println!("  CreatePipe: {}", std::io::Error::last_os_error());
            continue;
        }
        let env = vars.as_deref().map(block);
        let made = spawn_with_env(&exe, "child", container.sid, &[], job, &[], write, env.as_deref());
        // SAFETY: our pipe's write end, which the child has its own of now.
        unsafe { CloseHandle(write) };
        // SAFETY: our pipe's read end, owned by the file from here on.
        let mut from = unsafe { std::fs::File::from_raw_handle(read) };
        match made {
            Err(e) => println!("  {e}"),
            Ok(pi) => {
                let reader = std::thread::spawn(move || {
                    let mut said = String::new();
                    let _ = from.read_to_string(&mut said);
                    said
                });
                // SAFETY: the child's handles; it runs, is waited for 10 s at most, and
                // ended if it has not.
                unsafe {
                    ResumeThread(pi.hThread);
                    if WaitForSingleObject(pi.hProcess, 10_000) != 0 {
                        println!("  (killed: still running after 10 s)");
                        TerminateProcess(pi.hProcess, 1);
                    }
                    CloseHandle(pi.hThread);
                    CloseHandle(pi.hProcess);
                }
                print!("{}", reader.join().unwrap_or_default());
            }
        }
        // SAFETY: the job made above.
        unsafe { CloseHandle(job) };
        std::thread::sleep(Duration::from_millis(50));
        drop(container);
    }
}
