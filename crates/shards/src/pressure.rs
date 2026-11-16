//! The host's word that it is short of memory, as the daemon and a build hear it (D188): its
//! kernel's ([`MemoryPressure`]), or, where a variable names a FIFO (on Windows, a named
//! pipe), each line written to it, in place of its kernel's, so that a test tells it where
//! no unprivileged process may raise the system's pressure (macOS, Windows).

use std::ffi::OsString;
use std::io;
use std::time::{Duration, Instant};

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

    /// Its listener's act on the last word took `took`: what its kernel's source learns a
    /// pressure, and its pace, from (`platform::MemoryPressure::acted`); nothing for a pipe.
    pub fn acted(&mut self, took: Duration) {
        if let Pressure::Kernel(pressure) = self {
            pressure.acted(took);
        }
    }

    /// The most a pressure that goes on waits between its words now, as its kernel's source
    /// says it (`platform::MemoryPressure::repeat`); none for a pipe, whose lines are each
    /// a pressure of their own, or a source not raised.
    pub fn repeat(&self) -> Option<Duration> {
        match self {
            Pressure::Kernel(pressure) => pressure.repeat(),
            Pressure::Told(..) => None,
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

/// When what microVMs or builders gave back under a pressure is said (D188), without a fixed
/// while: as the pressure begins, then each time it has gone on as long again as it had when
/// last said (one repeat at the least), so that a pressure that goes on for a time `t` is
/// said about log2(t / repeat) times, its lines farther apart the longer it goes on (a
/// doubling, as a search past what is known: Baeza-Yates, Culberson and Rawlins, "Searching
/// in the plane", Inf. Comput. 106(2), 1993). A pressure that goes on is heard at most a
/// repeat apart, its source's own ([`Pressure::repeat`]: a trigger's window, or macOS's and
/// Windows' pace as they say it), so a word that comes two repeats after the last, a whole
/// repeat having passed with none, begins another, said at once; a source with no repeat (a
/// told pipe's line, a source not raised) has each word begin one.
#[derive(Debug, Default, Clone, Copy)]
pub struct Saying {
    /// When the pressure under way began, its last word, when it was last said, and the
    /// repeat its source said as its last word came.
    began: Option<Instant>,
    last: Option<Instant>,
    said: Option<Instant>,
    repeat: Option<Duration>,
}

impl Saying {
    /// A word of pressure heard at `now`, its source saying words of a pressure that goes on
    /// come at most `repeat` apart.
    pub fn heard(&mut self, now: Instant, repeat: Option<Duration>) {
        let goes_on = match (self.last, self.repeat) {
            (Some(last), Some(was)) => now.saturating_duration_since(last) <= was.saturating_mul(2),
            _ => false,
        };
        if !goes_on {
            self.began = Some(now);
            self.said = None;
        }
        self.last = Some(now);
        self.repeat = repeat;
    }

    /// Whether it is due to be said at `now`: never said in this pressure, or it has gone on
    /// as long again since it was last said as it had then.
    pub fn due(&self, now: Instant) -> bool {
        let (Some(began), Some(said)) = (self.began, self.said) else {
            return true;
        };
        let gone_on = said
            .saturating_duration_since(began)
            .max(self.repeat.unwrap_or_default());
        now.saturating_duration_since(said) >= gone_on
    }

    /// It was said at `now`.
    pub fn said(&mut self, now: Instant) {
        self.began.get_or_insert(now);
        self.said = Some(now);
    }
}

#[cfg(test)]
mod saying {
    use std::time::Duration;

    use super::*;

    /// A pressure that goes on is said as it begins, then at one repeat, two, four and eight
    /// into it; one heard again two repeats after a word begins again, said at once; and a
    /// source with no repeat has each word begin a pressure.
    #[test]
    fn a_pressure_is_said_as_it_doubles() {
        let w = Duration::from_millis(700);
        let t0 = Instant::now();
        let mut s = Saying::default();
        let mut said = Vec::new();
        for n in 0..=16u32 {
            let now = t0 + w * n;
            s.heard(now, Some(w));
            if s.due(now) {
                s.said(now);
                said.push(n);
            }
        }
        assert_eq!(said, [0, 1, 2, 4, 8, 16]);
        // Three repeats without a word: the next begins another pressure.
        let again = t0 + w * 19;
        s.heard(again, Some(w));
        assert!(s.due(again));
        s.said(again);
        assert!(!s.due(again + w / 2));
        assert!(s.due(again + w + Duration::from_millis(1)));
        // A pipe's words, no repeat: each its own pressure, said at once.
        let mut told = Saying::default();
        told.heard(t0, None);
        told.said(t0);
        let next = t0 + Duration::from_millis(1);
        told.heard(next, None);
        assert!(told.due(next));
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
