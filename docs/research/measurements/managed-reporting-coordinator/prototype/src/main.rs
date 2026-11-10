//! Controlled native cases, launched only as private subprocesses by the tests.

use std::fmt::{self, Display};
use std::io::Write as _;
use std::process::ExitCode;

use managed_reporting_prototype::{ReportError, report};

struct Recursive;

impl Display for Recursive {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: this controlled process never closes or replaces its C FILE.
        unsafe { report("inner") }.map_err(|_| fmt::Error)?;
        formatter.write_str("outer")
    }
}

struct BrokenFormatter;

impl Display for BrokenFormatter {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        Err(fmt::Error)
    }
}

fn observed(bytes: &[u8]) -> ExitCode {
    match std::io::stdout().write_all(bytes) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

fn main() -> ExitCode {
    let mode = std::env::args_os().nth(1);
    // SAFETY: every controlled case keeps the process stderr FILE alive; only
    // Closed closes the integer fd2, which report does not assume is open.
    let outcome = unsafe {
        match mode.as_deref().and_then(|mode| mode.to_str()) {
            Some("static") => report("literal"),
            Some("unicode") => report(format_args!("α 😀 count={} hex={:#x}", u64::MAX, u64::MAX)),
            Some("recursive") => report(Recursive),
            Some("formatter") => match report(BrokenFormatter) {
                Err(ReportError::Formatter(_)) => return observed(b"formatter\n"),
                _ => return ExitCode::FAILURE,
            },
            Some("closed") => {
                if libc::close(libc::STDERR_FILENO) != 0 {
                    return ExitCode::FAILURE;
                }
                match report("closed") {
                    Err(ReportError::Io(error)) if error.raw_os_error() == Some(libc::EBADF) => {
                        return observed(b"io:EBADF\n");
                    }
                    _ => return ExitCode::FAILURE,
                }
            }
            _ => return ExitCode::FAILURE,
        }
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
