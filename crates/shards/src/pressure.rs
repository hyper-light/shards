//! The host's word that it is short of memory, as the daemon and a build hear it (D188): its
//! kernel's ([`MemoryPressure`]), or, where a variable names a FIFO (on Windows, a named
//! pipe), each line written to it, in place of its kernel's, so that a test tells it where
//! no unprivileged process may raise the system's pressure (macOS, Windows).

use std::ffi::OsString;
use std::io;

use shards_vmm::platform::{MemoryPressure, PressureWatches};

/// What tells its listener its host is short of memory: its kernel, or the pipe a variable
/// names, by its path.
#[derive(Debug)]
pub enum Pressure {
    Kernel(MemoryPressure),
    Told(std::fs::File, OsString),
}

impl Pressure {
    /// The pipe variable `told` names, where it names one; else its kernel's pressure, with
    /// what has it watch more (a Linux cgroup's).
    pub fn new(told: &str) -> io::Result<(Pressure, Option<PressureWatches>)> {
        if let Some(path) = std::env::var_os(told) {
            // Read and written, so that a writer's going is no end: reads wait for the next.
            let pipe = std::fs::OpenOptions::new().read(true).write(true).open(&path)?;
            return Ok((Pressure::Told(pipe, path), None));
        }
        MemoryPressure::new().map(|(pressure, watches)| (Pressure::Kernel(pressure), Some(watches)))
    }

    /// What it hears, for a log.
    pub fn describe(&self) -> String {
        match self {
            Pressure::Kernel(pressure) => pressure.describe(),
            Pressure::Told(_, path) => format!(
                "each line written to {}, in place of its kernel's word",
                path.to_string_lossy()
            ),
        }
    }

    /// Waits for the next word that its host is short of memory.
    pub fn wait(&mut self) -> io::Result<()> {
        match self {
            Pressure::Kernel(pressure) => pressure.wait(),
            Pressure::Told(pipe, _) => {
                use std::io::Read as _;
                let mut byte = [0u8; 1];
                loop {
                    if pipe.read(&mut byte)? == 0 {
                        return Err(io::ErrorKind::UnexpectedEof.into());
                    }
                    if byte == *b"\n" {
                        return Ok(());
                    }
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::io::Write as _;
    use std::time::Duration;

    use super::*;

    /// A FIFO's lines are its words of pressure, each heard once, and a writer that goes is
    /// no end of them. A FIFO is Unix's: Windows' named pipe is build_windows.rs's and
    /// containers.rs's.
    #[test]
    fn a_fifo_tells_its_pressure_a_line_at_a_time() {
        let dir = shards_testdir::TempDir::new("told").unwrap();
        let path = dir.join("p");
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: mkfifo(3) of a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let fifo = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut told = Pressure::Told(fifo, path.clone().into_os_string());
        {
            let mut w = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            w.write_all(b"\n\n").unwrap();
        }
        told.wait().unwrap();
        told.wait().unwrap();
        let (heard, hear) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| {
                told.wait().unwrap();
                heard.send(()).unwrap();
            });
            assert!(
                hear.recv_timeout(Duration::from_millis(100)).is_err(),
                "no third word yet"
            );
            let mut w = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            w.write_all(b"\n").unwrap();
            hear.recv_timeout(Duration::from_secs(10)).unwrap();
        });
    }
}
