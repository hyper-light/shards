//! What a build's builders give back as their host runs short of memory (D188 for D133's
//! builders): the build's process watches its host's memory pressure as the daemon does
//! ([`MemoryPressure`]), and at each word of it asks each builder it has to give back what
//! it holds cold of its files and what it holds plugged past twice what it uses
//! (build::GIVE_PORT, [`Giver`]): every builder asked at once, one ask out to each at a
//! time, an answer not come within [`PATIENCE`] said to have had none and that builder
//! asked again only once it has answered; what they gave back said once a minute at most,
//! as the daemon says its microVMs'. Windows watches no pressure yet (D188): there, its
//! builders are asked nothing.

use std::io;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use shards_vmm::platform::MemoryPressure;

use super::builder::Giver;

/// How long builders' answers are waited for at a word of pressure: one not come by then is
/// said to have had none, and its builder is asked again only once it has answered.
const PATIENCE: Duration = Duration::from_secs(10);
/// How often what builders gave back is said, at most, as the daemon says its microVMs'
/// (daemon::reclaim's `SAY_EVERY`): a pressure that goes on says it once in this long.
const SAY_EVERY: Duration = Duration::from_secs(60);
/// Where it names a FIFO, a build's memory pressure is what is written to it, each line the
/// host's word that it is short of memory, in place of its kernel's: so that a test tells it
/// where no unprivileged process may raise the system's pressure (macOS).
const TOLD: &str = "SHARDS_BUILD_PRESSURE";

/// The watcher's channel, down which each builder's giver is sent as the builder starts:
/// started with the first, none where it could not start.
static WATCHER: OnceLock<Option<mpsc::Sender<Giver>>> = OnceLock::new();

/// Has the build's memory pressure watcher ask `giver`'s builder to give back at its host's
/// word, starting the watcher with the first builder; `plain`, whether what they gave back
/// is said on the build's stderr, where its plain display's lines go.
pub fn watch(giver: Giver, plain: bool) {
    if let Some(sender) = WATCHER.get_or_init(|| start(plain)) {
        let _ = sender.send(giver);
    }
}

/// The watcher, on a thread of its own: its channel, none where its host's pressure cannot
/// be watched or its thread cannot start, its builders then asked nothing.
fn start(plain: bool) -> Option<mpsc::Sender<Giver>> {
    let pressure = Pressure::new().ok()?;
    let (sender, givers) = mpsc::channel();
    std::thread::Builder::new()
        .name("give back".into())
        .spawn(move || watch_pressure(pressure, &givers, plain))
        .ok()?;
    Some(sender)
}

/// Asks its builders to give back each time its host is short of memory, until the
/// pressure can no longer be heard.
fn watch_pressure(mut pressure: Pressure, givers: &mpsc::Receiver<Giver>, plain: bool) {
    let mut held: Vec<Giver> = Vec::new();
    let mut unsaid = Unsaid::default();
    while pressure.wait().is_ok() {
        held.extend(givers.try_iter());
        let now = Instant::now();
        let said = pass(
            &mut held,
            &mut unsaid,
            now.checked_add(PATIENCE).unwrap_or(now),
            now,
        );
        if let (Some(said), true) = (said, plain) {
            use std::io::Write as _;
            let _ = writeln!(io::stderr(), "{said}");
        }
    }
}

/// One word of pressure, heard `now`: each builder in `held` asked at once (one whose ask
/// before is still out is asked nothing more), each answer heard by `until`, a builder whose
/// connection has ended let go of; what they gave back counted, and said where it is due.
fn pass(held: &mut Vec<Giver>, unsaid: &mut Unsaid, until: Instant, now: Instant) -> Option<String> {
    held.retain_mut(|g| g.ask().is_ok());
    unsaid.since.get_or_insert(now);
    unsaid.passes += 1;
    held.retain_mut(|g| match g.answer(until) {
        Ok(Some((freed, unplugging))) => {
            unsaid.freed = unsaid.freed.saturating_add(freed);
            unsaid.unplugging = unsaid.unplugging.saturating_add(unplugging);
            unsaid.answered += 1;
            true
        }
        Ok(None) => {
            unsaid.unanswered += 1;
            true
        }
        Err(_) => false,
    });
    unsaid.say(now)
}

/// What builders gave back since it was last said: since when, how much of their files and
/// of the memory they held plugged, in how many passes, how many answered and how many did
/// not in time; and when it was last said.
#[derive(Debug, Default)]
struct Unsaid {
    since: Option<Instant>,
    freed: u64,
    unplugging: u64,
    passes: u32,
    answered: u32,
    unanswered: u32,
    said: Option<Instant>,
}

impl Unsaid {
    /// What builders gave back since it was last said, where they gave back anything or any
    /// went unanswered, at most once in [`SAY_EVERY`]; counted afresh after.
    fn say(&mut self, now: Instant) -> Option<String> {
        if self
            .said
            .is_some_and(|at| now.saturating_duration_since(at) < SAY_EVERY)
        {
            return None;
        }
        if self.freed == 0 && self.unplugging == 0 && self.unanswered == 0 {
            return None;
        }
        let over = self
            .since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        let mut said = format!(
            "the host is short of memory: the build's builders gave back {} MiB they held cold of their files, and asked to unplug {} MiB they held plugged past twice what they use and their peak since, in {} passes over {over:?}",
            self.freed >> 20,
            self.unplugging >> 20,
            self.passes,
        );
        if self.unanswered > 0 {
            said.push_str(&format!(
                "; {} asks had no answer within {} s",
                self.unanswered,
                PATIENCE.as_secs()
            ));
        }
        *self = Unsaid {
            said: Some(now),
            ..Unsaid::default()
        };
        Some(said)
    }
}

/// What tells the watcher its host is short of memory: its kernel ([`MemoryPressure`]), or
/// a FIFO [`TOLD`] names.
enum Pressure {
    Kernel(MemoryPressure),
    Told(std::fs::File),
}

impl Pressure {
    fn new() -> io::Result<Pressure> {
        if let Some(path) = std::env::var_os(TOLD) {
            // Read and written, so that a writer's going is no end: reads wait for the next.
            let fifo = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
            return Ok(Pressure::Told(fifo));
        }
        MemoryPressure::new().map(|(pressure, _)| Pressure::Kernel(pressure))
    }

    /// Waits for the next word that its host is short of memory.
    fn wait(&mut self) -> io::Result<()> {
        match self {
            Pressure::Kernel(pressure) => pressure.wait(),
            Pressure::Told(fifo) => {
                use std::io::Read as _;
                let mut byte = [0u8; 1];
                loop {
                    if fifo.read(&mut byte)? == 0 {
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::io::{Read as _, Write as _};

    use shards_abi::{build, run};
    use shards_vmm::platform::UnixStream;

    use super::*;

    const MIB: u64 = 1 << 20;

    /// The time a pass waits for answers by: 50 ms on.
    fn soon() -> Instant {
        Instant::now() + Duration::from_millis(50)
    }

    /// A giver and the builder's end of its connection.
    fn builder() -> (Giver, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        (Giver::new(ours), theirs)
    }

    /// Whether the builder's end has an ask waiting: read if so.
    fn was_asked(theirs: &UnixStream) -> bool {
        theirs.set_nonblocking(true).unwrap();
        let mut h = [0u8; run::HEADER];
        let asked = match (&*theirs).read(&mut h) {
            Ok(n) => {
                theirs.set_nonblocking(false).unwrap();
                (&*theirs).read_exact(&mut h[n..]).unwrap();
                assert_eq!(run::parse_header(h), Some((build::kind::GIVE_BACK, 0)));
                true
            }
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
                false
            }
        };
        theirs.set_nonblocking(false).unwrap();
        asked
    }

    fn gave(theirs: &UnixStream, said: &str) {
        let len = u32::try_from(said.len()).unwrap();
        (&*theirs)
            .write_all(&run::header(build::kind::GAVE, len))
            .unwrap();
        (&*theirs).write_all(said.as_bytes()).unwrap();
    }

    /// At each word every builder is asked at once, one whose ask is still out asked nothing
    /// more; an answer late for one word is counted at the next, and a builder whose
    /// connection ended is let go of, whether as it is asked or as its answer is waited for. What they gave back is said at once, then no sooner
    /// than a minute after, with everything given back since.
    #[test]
    fn each_word_asks_every_builder_once_and_says_the_sum_once_a_minute() {
        let (one, one_end) = builder();
        let (two, two_end) = builder();
        let mut held = vec![one, two];
        let mut unsaid = Unsaid::default();
        let t0 = Instant::now();
        // The first word: one answers in time, the other not.
        gave(&one_end, &format!("{} {}", 30 * MIB, 128 * MIB));
        let said = pass(&mut held, &mut unsaid, soon(), t0).unwrap();
        assert!(was_asked(&one_end) && was_asked(&two_end));
        assert!(
            said.contains("gave back 30 MiB")
                && said.contains("unplug 128 MiB")
                && said.contains("in 1 passes"),
            "{said}"
        );
        assert!(said.contains("1 asks had no answer within 10 s"), "{said}");
        // The second word: the late one answers; it is asked nothing until it has, the first
        // asked again; within the minute, nothing said.
        gave(&two_end, &format!("{} 0", 7 * MIB));
        gave(&one_end, &format!("{} 0", MIB));
        assert_eq!(
            pass(&mut held, &mut unsaid, soon(), t0 + Duration::from_secs(5)),
            None
        );
        assert!(was_asked(&one_end), "the first, answered, asked again");
        assert!(
            !was_asked(&two_end),
            "the second, its ask out, asked nothing more"
        );
        // The first's connection ends: let go of at the next word, which says the sum since.
        drop(one_end);
        gave(&two_end, &format!("{} 0", 2 * MIB));
        let said = pass(&mut held, &mut unsaid, soon(), t0 + Duration::from_secs(61)).unwrap();
        assert_eq!(held.len(), 1, "the ended builder let go of");
        assert!(was_asked(&two_end));
        assert!(
            said.contains("gave back 10 MiB") && said.contains("in 2 passes"),
            "{said}"
        );
        // A builder whose connection ends while its ask is out is let go of as its answer is
        // waited for: asked, it answers nothing in time; then its connection ends.
        assert_eq!(
            pass(&mut held, &mut unsaid, soon(), t0 + Duration::from_secs(62)),
            None
        );
        assert!(was_asked(&two_end));
        drop(two_end);
        assert_eq!(
            pass(&mut held, &mut unsaid, soon(), t0 + Duration::from_secs(63)),
            None
        );
        assert!(
            held.is_empty(),
            "the builder whose connection ended while asked let go of"
        );
    }

    /// Nothing given back and every ask answered is said nowhere.
    #[test]
    fn nothing_given_back_is_not_said() {
        let (one, one_end) = builder();
        let mut held = vec![one];
        let mut unsaid = Unsaid::default();
        gave(&one_end, "0 0");
        assert_eq!(
            pass(
                &mut held,
                &mut unsaid,
                Instant::now() + Duration::from_secs(10),
                Instant::now()
            ),
            None
        );
    }

    /// A FIFO's lines are its words of pressure, each heard once, and a writer that goes is
    /// no end of them.
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
        let mut told = Pressure::Told(fifo);
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
