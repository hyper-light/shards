//! Coordinator-only native experiment, never a product reporting implementation.

use std::fmt::{self, Display};
use std::io::Write as _;

use managed_reporting_prototype::{ReportError, report};

#[path = "../../../managed-borrowed/allocation.rs"]
mod faults;

struct Broken;
impl Display for Broken {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        Err(fmt::Error)
    }
}

struct HeldRust {
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::mpsc::Receiver<()>,
}
impl Display for HeldRust {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RUST-BEGIN")?;
        self.entered.send(()).map_err(|_| fmt::Error)?;
        self.release.recv().map_err(|_| fmt::Error)?;
        f.write_str("RUST-END\n")
    }
}

fn main() {
    let mode = std::env::args().nth(1).expect("explicit coordinator mode");
    match mode.as_str() {
        "cold" => {
            // SAFETY: this private process never closes/replaces its stderr FILE.
            let result = faults::fail_after(0, || unsafe {
                report(format_args!("α 😀 count={} hex={:#x}", u64::MAX, u64::MAX))
            });
            assert!(result.is_ok(), "native cold reporter returned {result:?}");
        }
        "formatter-cold" => {
            // SAFETY: the controlled formatter leaves the process FILE unchanged.
            let result = faults::fail_after(0, || unsafe { report(Broken) });
            assert!(matches!(result, Err(ReportError::Formatter(_))));
            println!("FORMATTER_RETURNED");
        }
        "closed-cold" => {
            // SAFETY: only this private child's integer fd2 is closed. No FILE or
            // borrowed Rust descriptor is closed or held by this controlled case.
            assert_eq!(unsafe { libc::close(libc::STDERR_FILENO) }, 0);
            // SAFETY: closing fd2 leaves the live C FILE object/lock unchanged.
            let result = faults::fail_after(0, || unsafe { report("closed") });
            assert!(matches!(result, Err(ReportError::Io(ref e)) if e.raw_os_error() == Some(libc::EBADF)));
            println!("CLOSED_IO_RETURNED");
        }
        "mixed" => {
            // Zero capacity makes each gate a rendezvous, not a buffered queue.
            let (entered, wait_entered) = std::sync::mpsc::sync_channel(0);
            let (release, wait_release) = std::sync::mpsc::sync_channel(0);
            let rust = std::thread::spawn(move || {
                std::io::stderr().write_fmt(format_args!(
                    "{}",
                    HeldRust {
                        entered,
                        release: wait_release
                    }
                ))
            });
            wait_entered.recv().unwrap();
            // Rust's writer is inside its formatter with its own lock held.
            // SAFETY: neither controlled thread mutates the process C FILE.
            let result = unsafe { report("NATIVE") };
            release.send(()).unwrap();
            result.unwrap();
            rust.join().unwrap().unwrap();
            println!("NATIVE_COMPLETED_BEFORE_RUST_RELEASE");
        }
        _ => panic!("unknown private coordinator mode"),
    }
}
