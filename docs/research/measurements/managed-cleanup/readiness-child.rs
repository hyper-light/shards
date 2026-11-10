//! Private native test artifact, never installed or carried by Shards.
//! Protocol matches readiness-child.py's MODE stdin controls, without an interpreter.

#![deny(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

use std::ffi::OsStr;
use std::io::{self, Read as _, Write as _};
use std::process::ExitCode;

enum Mode {
    Silent,
    Early,
    Partial,
    Normal,
}

fn run() -> io::Result<ExitCode> {
    let mut args = std::env::args_os().skip(1);
    let mode = match args.next().as_deref() {
        Some(value) if value == "silent" => Mode::Silent,
        Some(value) if value == "early" => Mode::Early,
        Some(value) if value == "partial" => Mode::Partial,
        Some(value) if value == "normal" => Mode::Normal,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected silent, early, partial or normal",
            ));
        }
    };
    if args.next().as_deref() != Some(OsStr::new("stdin")) || args.next().is_some() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "expected MODE stdin"));
    }
    let mut stderr = io::stderr().lock();
    writeln!(stderr, "CLEANUP_CHILD_PID {}", std::process::id())?;
    stderr.flush()?;
    drop(stderr);
    let mut stdout = io::stdout().lock();
    match mode {
        Mode::Early => return Ok(ExitCode::from(7)),
        Mode::Partial => stdout.write_all(b"rea")?,
        Mode::Normal => stdout.write_all(b"ready\n")?,
        Mode::Silent => {}
    }
    // Write bytes, including literal LF, without platform text newline conversion.
    stdout.flush()?;
    let mut release = [0];
    match io::stdin().lock().read_exact(&mut release) {
        Ok(()) if release == *b"r" => {}
        Ok(()) => return Ok(ExitCode::from(8)),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(ExitCode::from(8)),
        Err(error) => return Err(error),
    }
    stdout.write_all(b"released\n")?;
    stdout.flush()?;
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    match run() {
        Ok(status) => status,
        Err(error) => {
            let _ = writeln!(io::stderr(), "readiness child {:?}: {error}", error.kind());
            ExitCode::FAILURE
        }
    }
}
