//! Private test registration is appended to exact client.rs; production bytes are
//! unchanged. This is a bounded allocation-refusal correctness case, not a VM or
//! allocation profile. The outer driver holds the test process unreaped while it
//! terminates that newly created session and observes its one stand-in child.
use super::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::AtomicBool;

thread_local! {
    static REFUSE: Cell<bool> = const { Cell::new(false) };
    static REPORTED: Cell<bool> = const { Cell::new(false) };
}
static TRACE: AtomicBool = AtomicBool::new(false);
struct Refusing;
#[global_allocator]
static ALLOCATOR: Refusing = Refusing;

unsafe extern "C" {
    fn backtrace(frames: *mut *mut libc::c_void, count: libc::c_int) -> libc::c_int;
    fn backtrace_symbols_fd(frames: *const *mut libc::c_void, count: libc::c_int, fd: libc::c_int);
}

fn refused(size: usize) -> bool {
    if !REFUSE.get() {
        return false;
    }
    if !REPORTED.replace(true) {
        let mut digits = [0u8; usize::MAX.ilog10() as usize + 1];
        let mut remaining = size;
        let mut at = digits.len();
        loop {
            at -= 1;
            digits[at] = b'0' + (remaining % 10) as u8;
            remaining /= 10;
            if remaining == 0 {
                break;
            }
        }
        // SAFETY: constant and stack byte slices are valid for these writes.
        // Best-effort diagnostics never change the null allocation result.
        unsafe {
            let prefix = b"COLD_REFUSED_BYTES ";
            let _ = libc::write(2, prefix.as_ptr().cast(), prefix.len());
            let _ = libc::write(2, digits[at..].as_ptr().cast(), digits.len() - at);
            let _ = libc::write(2, b"\n".as_ptr().cast(), 1);
            if TRACE.load(Ordering::Relaxed) {
                // Explicit diagnostic frame envelope only, matching the retained
                // attribution experiment. C unwinding may incur its own costs.
                let mut frames = [std::ptr::null_mut(); 32];
                let count = backtrace(frames.as_mut_ptr(), frames.len() as libc::c_int);
                if count > 0 {
                    backtrace_symbols_fd(frames.as_ptr(), count, 2);
                }
            }
        }
    }
    true
}

// SAFETY: non-refused requests and every deallocation use the same System
// allocator and original layout. Refusal is thread-local and returns null.
unsafe impl GlobalAlloc for Refusing {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if refused(layout.size()) {
            std::ptr::null_mut()
        } else {
            // SAFETY: caller supplies GlobalAlloc's valid layout.
            unsafe { System.alloc(layout) }
        }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if refused(layout.size()) {
            std::ptr::null_mut()
        } else {
            // SAFETY: caller supplies GlobalAlloc's valid layout.
            unsafe { System.alloc_zeroed(layout) }
        }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if refused(size) {
            std::ptr::null_mut()
        } else {
            // SAFETY: original allocation/layout and new size come from caller.
            unsafe { System.realloc(ptr, layout, size) }
        }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: every successful allocation above came from System.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[test]
fn owned_connection_cold_sync_refusal_current() {
    let dir = PathBuf::from(std::env::var_os("SHARDS_COLD_CASE_DIR").unwrap());
    let millis: u64 = std::env::var("SHARDS_COLD_CASE_MS").unwrap().parse().unwrap();
    let deadline = Instant::now().checked_add(Duration::from_millis(millis)).unwrap();
    let guest = start(&GuestSpec {
        kernel: dir.join("unused-kernel"),
        init: Init::Executable(dir.join("unused-init")),
        cmdline: String::new(),
        vcpus: 1,
        memory_mib: crate::min_memory_mib(std::env::consts::ARCH),
        disks: Vec::new(),
        shares: Vec::new(),
        ports: vec![1024],
        binary: Some(dir.join("standin")),
    })
    .unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&guest.vsock).unwrap();
    let marker = guest.vsock.parent().unwrap().join("cold-sync-owned");
    let mut ownership = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(marker)
        .unwrap();
    ownership
        .write_all(std::env::var("SHARDS_COLD_OWNER").unwrap().as_bytes())
        .unwrap();
    drop(ownership);
    TRACE.store(std::env::var_os("SHARDS_COLD_TRACE").is_some(), Ordering::Relaxed);
    // No identity/stop/wait/connect call before the guard: State stays cold.
    println!("COLD_READY {} {}", guest.pid, guest.vsock.display());
    std::io::stdout().flush().unwrap();
    let mut release = [0u8; 1];
    std::io::stdin().read_exact(&mut release).unwrap();
    assert_eq!(release, [b'R']);
    assert!(Instant::now() < deadline);
    REFUSE.set(true);
    let result = guest.connect(1024, deadline.saturating_duration_since(Instant::now()));
    REFUSE.set(false);
    #[cfg(has_allocation_refusal)]
    assert!(
        matches!(result, Err(Refusal::Allocation { .. })),
        "expected typed allocation refusal"
    );
    println!("COLD_RETURNED_REFUSAL {}", result.is_err());
    drop(result);
    drop(listener);
    guest.stop().unwrap();
    assert_eq!(
        guest
            .wait_for(deadline.saturating_duration_since(Instant::now()))
            .unwrap(),
        Some(End::Stopped)
    );
    let mut status = 0;
    // SAFETY: this test is the stand-in's actual parent. WNOHANG never waits;
    // ECHILD independently checks that the API already reaped this owned child.
    let waited = unsafe { libc::waitpid(guest.pid as libc::pid_t, &mut status, libc::WNOHANG) };
    let errno = std::io::Error::last_os_error().raw_os_error();
    assert_eq!((waited, errno), (-1, Some(libc::ECHILD)));
    println!("COLD_NATIVE_REAP_ECHILD");
    println!("COLD_OWNED_REAPED");
}
