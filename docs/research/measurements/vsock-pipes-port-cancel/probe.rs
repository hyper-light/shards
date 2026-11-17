//! Does a cancelled overlapped read's end reach its OVERLAPPED on a named pipe's server end that
//! is associated with an I/O completion port, before anyone takes its packet from the port?
//! Three cases, each a byte-mode duplex instance opened for overlapped calls, its client opened
//! as `std` opens a file, a read under way on the server end with an OVERLAPPED, then
//! CancelIoEx and a GetOverlappedResultEx waited for 3 s: no port; a port, the OVERLAPPED
//! naming no event; a port, the OVERLAPPED naming a manual-reset event. Then whether the port
//! holds the read's packet. D272, whp-verify (2026-11-16): run as README.md says.
//!
//! Build and run on a Windows host: `rustc -O --edition 2024 probe.rs -o probe.exe`, `./probe.exe`.

#![allow(non_snake_case)]
use std::ffi::c_void;

type H = *mut c_void;

#[repr(C)]
struct Ov {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: H,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateNamedPipeW(n: *const u16, om: u32, pm: u32, max: u32, ob: u32, ib: u32, to: u32, sa: *const c_void) -> H;
    fn CreateFileW(n: *const u16, acc: u32, share: u32, sa: *const c_void, disp: u32, flags: u32, t: H) -> H;
    fn ReadFile(h: H, buf: *mut u8, n: u32, read: *mut u32, ov: *mut Ov) -> i32;
    fn CancelIoEx(h: H, ov: *mut Ov) -> i32;
    fn GetOverlappedResultEx(h: H, ov: *mut Ov, n: *mut u32, ms: u32, alertable: i32) -> i32;
    fn CreateIoCompletionPort(h: H, port: H, key: usize, threads: u32) -> H;
    fn GetQueuedCompletionStatus(port: H, n: *mut u32, key: *mut usize, ov: *mut *mut Ov, ms: u32) -> i32;
    fn CreateEventW(sa: *const c_void, manual: i32, initial: i32, name: *const u16) -> H;
    fn GetLastError() -> u32;
}

const PIPE_ACCESS_DUPLEX: u32 = 3;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn run(case: &str, attach: bool, event: bool) {
    // SAFETY: a probe: each handle and buffer below lives to the process's end.
    unsafe {
        let b = char::from(92u8);
        let name = wide(&format!("{b}{b}.{b}pipe{b}vsock-port-cancel-{}-{case}", std::process::id()));
        let s = CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            0,
            1,
            65536,
            65536,
            0,
            std::ptr::null(),
        );
        let made = GetLastError();
        let _client = CreateFileW(name.as_ptr(), GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut());
        let mut port = std::ptr::null_mut();
        if attach {
            port = CreateIoCompletionPort(-1isize as H, std::ptr::null_mut(), 0, 1);
            CreateIoCompletionPort(s, port, 7, 0);
        }
        let ended = if event {
            CreateEventW(std::ptr::null(), 1, 0, std::ptr::null())
        } else {
            std::ptr::null_mut()
        };
        let ov = Box::into_raw(Box::new(Ov {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            event: ended,
        }));
        let mut buf = vec![0u8; 4096];
        let read = ReadFile(s, buf.as_mut_ptr(), 4096, std::ptr::null_mut(), ov);
        let read_error = GetLastError();
        let cancelled = CancelIoEx(s, ov);
        let waited = std::time::Instant::now();
        let mut moved = 0u32;
        let got = GetOverlappedResultEx(s, ov, &mut moved, 3000, 0);
        let got_error = GetLastError();
        let took = waited.elapsed();
        let internal = (*ov).internal;
        let mut packet = String::new();
        if attach {
            let (mut n, mut key, mut pov) = (0u32, 0usize, std::ptr::null_mut());
            let q = GetQueuedCompletionStatus(port, &mut n, &mut key, &mut pov, 0);
            packet = format!(" port packet={q}/{} key={key}", GetLastError());
        }
        println!(
            "{case}: made {made} read={read}/{read_error} cancel={cancelled} wait(3s)={got}/{got_error} after {took:?} internal={internal:#x}{packet}"
        );
    }
}

fn main() {
    run("plain", false, false);
    run("port", true, false);
    run("port-event", true, true);
}
