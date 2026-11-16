//! The host's word that it is short of memory, as the daemon and a build hear it (D188): its
//! kernel's ([`MemoryPressure`]), or, where a variable names a FIFO (on Windows, a named
//! pipe), each line written to it, in place of its kernel's, so that a test tells it where
//! no unprivileged process may raise the system's pressure (macOS, Windows). A told line may
//! say instead what memory its host has available, which every charge of a run's buffers is
//! then held to in place of its kernel's count ([`available`], D252): no test may set that
//! either.

use std::ffi::{OsStr, OsString};
use std::io;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use shards_vmm::platform::{MemoryPressure, PressureWatches};

/// The variable naming the pipe a test tells the daemon its host's word through, and the
/// file or pipe whose first line a run host is told by (api_run.rs): D188, D252.
pub const TOLD: &str = "SHARDS_PRESSURE";

/// A told line: its host's word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Word {
    /// It is short of memory: an empty line (D188).
    Short,
    /// It has this many bytes of memory available, in place of its kernel's count: a line
    /// `available BYTES`, BYTES in decimal digits alone (D252).
    Available(u64),
}

/// What a told line of available memory begins with, before its bytes.
const AVAILABLE: &[u8] = b"available ";
/// The longest told line, its newline apart: [`AVAILABLE`] and the most decimal digits a u64
/// has (20, `u64::MAX`'s). A line past it is no word, refused as it reaches this length rather
/// than held.
const WORD_MOST: usize = AVAILABLE.len() + (u64::MAX.ilog10() as usize + 1);

/// What its host has available as a told line said it last ([`Word::Available`]); none until
/// one has. One for the process, as its kernel's count is.
static TOLD_AVAILABLE: Mutex<Option<u64>> = Mutex::new(None);

/// What this host has available of its memory, which every charge of a run's buffers is held
/// to (daemon/managed.rs `Charge`, the daemon's runs' and a run host's alike, D252): what a
/// told line said last, where one has ([`Word::Available`]); else its kernel's count
/// (`platform::host_available`), none where that cannot be read.
pub fn available() -> Option<u64> {
    let told = *TOLD_AVAILABLE.lock().unwrap_or_else(PoisonError::into_inner);
    told.or_else(shards_vmm::platform::host_available)
}

/// Where variable `told` names a FIFO, a named pipe or a file, the word its first line says,
/// an `available` one told at once ([`available`]): a run host's (api_run.rs), told once,
/// before its run is made, as its run's charges are made as it starts (D252). A pipe nothing
/// has written to yet is waited on. None where `told` names nothing.
pub fn first(told: &str) -> io::Result<Option<Word>> {
    let Some(path) = std::env::var_os(told) else {
        return Ok(None);
    };
    heard(&mut open(&path)?).map(Some)
}

/// What `path` names, read and written, so that a pipe's writer's going is no end: reads
/// wait for the next.
fn open(path: &OsStr) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new().read(true).write(true).open(path)
}

/// `from`'s next line's word ([`read_word`]), an `available` one told at once.
fn heard(from: &mut impl io::Read) -> io::Result<Word> {
    let word = read_word(from)?;
    if let Word::Available(bytes) = word {
        *TOLD_AVAILABLE.lock().unwrap_or_else(PoisonError::into_inner) = Some(bytes);
    }
    Ok(word)
}

/// Reads `from`'s next line as a word: refused `InvalidData` where it says none or is longer
/// than any ([`WORD_MOST`]), `UnexpectedEof` where it ends before its newline.
fn read_word(from: &mut impl io::Read) -> io::Result<Word> {
    let mut line = [0u8; WORD_MOST];
    let mut len = 0;
    loop {
        let mut byte = [0u8; 1];
        from.read_exact(&mut byte)?;
        if byte == *b"\n" {
            break;
        }
        let [b] = byte;
        let at = line.get_mut(len).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a told line past {WORD_MOST} bytes, longer than any word"),
            )
        })?;
        *at = b;
        len += 1;
    }
    let line = line.get(..len).unwrap_or_default();
    if line.is_empty() {
        return Ok(Word::Short);
    }
    line.strip_prefix(AVAILABLE)
        .filter(|digits| !digits.is_empty() && digits.iter().all(u8::is_ascii_digit))
        .and_then(|digits| std::str::from_utf8(digits).ok()?.parse::<u64>().ok())
        .map(Word::Available)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{:?} is no told word: an empty line, or `available BYTES`",
                    String::from_utf8_lossy(line)
                ),
            )
        })
}

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
            let pipe = open(&path)?;
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

    /// Waits for its host's next word: its kernel's, that it is short of memory; a told pipe's
    /// next line, an `available` one told at once ([`available`]).
    pub fn wait(&mut self) -> io::Result<Word> {
        match self {
            Pressure::Kernel(pressure) => pressure.wait().map(|()| Word::Short),
            Pressure::Told(pipe, _) => heard(pipe),
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

#[cfg(test)]
mod words {
    use super::*;

    /// A told line is an empty one, its host short of memory, or `available BYTES` in decimal
    /// digits alone, as far as a u64 counts; one after another from one pipe. Any other line, a
    /// line past the longest word and one cut off are refused. Read alone, never told: what
    /// [`available`] answers is the process's, which its other tests' charges read.
    #[test]
    fn a_told_line_is_short_or_what_its_host_has_available() {
        let read = |mut text: &[u8]| read_word(&mut text).map_err(|e| e.kind());
        assert_eq!(read(b"\n"), Ok(Word::Short));
        assert_eq!(read(b"available 0\n"), Ok(Word::Available(0)));
        assert_eq!(
            read(b"available 18446744073709551615\n"),
            Ok(Word::Available(u64::MAX))
        );
        for no_word in [
            &b"available 18446744073709551616\n"[..],
            b"available +5\n",
            b"available \n",
            b"available 5 \n",
            b"available 5\r\n",
            b"available\n",
            b"x\n",
            b"available 000000000000000000005\n",
        ] {
            assert_eq!(
                read(no_word),
                Err(io::ErrorKind::InvalidData),
                "{}",
                String::from_utf8_lossy(no_word)
            );
        }
        assert_eq!(read(b"available 5"), Err(io::ErrorKind::UnexpectedEof));
        assert_eq!(read(b""), Err(io::ErrorKind::UnexpectedEof));
        let mut pipe: &[u8] = b"available 7\n\navailable 9\n";
        let words: Vec<_> = (0..3)
            .map(|_| read_word(&mut pipe).map_err(|e| e.kind()))
            .collect();
        assert_eq!(
            words,
            [Ok(Word::Available(7)), Ok(Word::Short), Ok(Word::Available(9))]
        );
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
        assert_eq!(told.wait().unwrap(), Word::Short);
        assert_eq!(told.wait().unwrap(), Word::Short);
        let (heard, hear) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| {
                assert_eq!(told.wait().unwrap(), Word::Short);
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
