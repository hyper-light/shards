//! What a build's builders give back as their host runs short of memory (D188 for D133's
//! builders): the build's process watches its host's memory pressure as the daemon does
//! ([`MemoryPressure`]), and at each word of it asks each builder it has to give back what
//! it holds cold of its files and what it holds plugged past twice what it uses
//! (build::GIVE_PORT, [`Giver`]): every builder asked at once, one ask out to each at a
//! time, an answer not come within [`PATIENCE`] said to have had none and that builder
//! asked again only once it has answered; then, where the build's cgroup is held to a budget
//! (Linux: a `memory.high` or `memory.max` on it or above it), each builder told its share of
//! the room the budget leaves (`ROOM`), which it holds its build within, as the daemon tells
//! its runs theirs (D188): its guest then keeps its own page cache within the budget, where
//! a give-back alone left a held build's host throttling it between words (Fedora, apt 105 s
//! held against 19 s free, 46 MiB given back in 55 asks). What they gave back is said once a
//! minute at most, as the daemon says its microVMs'. Windows watches no pressure yet (D188):
//! there, its builders are asked nothing.

use std::io;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use shards_vmm::platform::MemoryPressure;

use super::builder::{Answer, Giver};

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
/// is said on the build's stderr, where its plain display's lines go. A build held to a
/// budget tells its builder the budget's room first, so that its guest holds itself within
/// it from its first step, which waits for no answer.
pub fn watch(mut giver: Giver, plain: bool) {
    if let Some(r) = room() {
        let _ = giver.tell_first(r);
    }
    if let Some(sender) = WATCHER.get_or_init(|| start(plain)) {
        let _ = sender.send(giver);
    }
}

/// The room the budget over this process's cgroup leaves its builders, as the daemon works
/// out a run's (`daemon::budget::room`, nothing of theirs evicted): none where no cgroup it
/// is in has a budget, or none is to be read (macOS).
fn room() -> Option<i64> {
    let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = own.lines().find_map(|l| l.strip_prefix("0::"))?;
    let root = std::path::Path::new(crate::daemon::budget::ROOT);
    let dir = root.join(path.trim_start_matches('/'));
    crate::daemon::budget::room(&dir, root, &mut |_| 0).map(|(room, _)| room)
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
            &room,
        );
        if let (Some(said), true) = (said, plain) {
            use std::io::Write as _;
            let _ = writeln!(io::stderr(), "{said}");
        }
    }
}

/// One word of pressure, heard `now`: each builder in `held` asked at once (one whose ask
/// before is still out is asked nothing more), each answer heard by `until`, a builder whose
/// connection has ended let go of; then, where `room` finds a budget, each whose answer has
/// come told its share of the room it leaves after, and heard by `until` likewise. What
/// they gave back counted, and said where it is due.
fn pass(
    held: &mut Vec<Giver>,
    unsaid: &mut Unsaid,
    until: Instant,
    now: Instant,
    room: &dyn Fn() -> Option<i64>,
) -> Option<String> {
    held.retain_mut(|g| g.ask().is_ok());
    unsaid.since.get_or_insert(now);
    unsaid.passes += 1;
    held.retain_mut(|g| hear(g, until, unsaid));
    if let Some(r) = room() {
        let share = r / i64::try_from(held.len().max(1)).unwrap_or(i64::MAX);
        // Those told alone heard: one whose give-back is still out was counted unanswered.
        let mut told = Vec::with_capacity(held.len());
        held.retain_mut(|g| g.tell(Some(share)).map(|went| told.push(went)).is_ok());
        let mut told = told.into_iter();
        held.retain_mut(|g| !told.next().unwrap_or(false) || hear(g, until, unsaid));
    }
    unsaid.say(now)
}

/// The answer to `g`'s ask out, counted in `unsaid`, by `until`: whether its builder is
/// kept, which one whose connection has ended is not.
fn hear(g: &mut Giver, until: Instant, unsaid: &mut Unsaid) -> bool {
    match g.answer(until) {
        Ok(Some(Answer::Gave(freed, unplugging))) => {
            unsaid.freed = unsaid.freed.saturating_add(freed);
            unsaid.unplugging = unsaid.unplugging.saturating_add(unplugging);
            unsaid.answered += 1;
            true
        }
        Ok(Some(Answer::Held(within))) => {
            unsaid.held = within;
            true
        }
        Ok(Some(Answer::Refused(why))) => {
            unsaid.refused += 1;
            unsaid.why = Some(why);
            true
        }
        Ok(None) => {
            unsaid.unanswered += 1;
            true
        }
        Err(_) => false,
    }
}

/// What builders gave back since it was last said: since when, how much of their files and
/// of the memory they held plugged, in how many passes, how many answered, how many did not
/// in time and how many said why not; and when it was last said.
#[derive(Debug, Default)]
struct Unsaid {
    since: Option<Instant>,
    freed: u64,
    unplugging: u64,
    passes: u32,
    answered: u32,
    unanswered: u32,
    /// What its builder was last held within past what it uses, by its budget's room.
    held: Option<u64>,
    /// How many answers said why their builder gave nothing back or held nothing, and the
    /// last one's words.
    refused: u32,
    why: Option<String>,
    said: Option<Instant>,
}

impl Unsaid {
    /// What builders gave back since it was last said, where they gave back anything, or
    /// any went unanswered or said why not, at most once in [`SAY_EVERY`]; counted afresh
    /// after.
    fn say(&mut self, now: Instant) -> Option<String> {
        if self
            .said
            .is_some_and(|at| now.saturating_duration_since(at) < SAY_EVERY)
        {
            return None;
        }
        if self.freed == 0 && self.unplugging == 0 && self.unanswered == 0 && self.refused == 0 {
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
        if let Some(why) = &self.why {
            said.push_str(&format!("; {} asks were refused, the last: {why}", self.refused));
        }
        if let Some(held) = self.held {
            said.push_str(&format!(
                "; held within {} MiB past what they use, by their budget's room",
                held >> 20
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

    /// A giver and the builder's end of its connection, whose reads fail rather than wait
    /// past 5 s for a frame that never comes.
    fn builder() -> (Giver, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        theirs.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
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
    /// connection ended is let go of, whether as it is asked or as its answer is waited for.
    /// What they gave back is said at once, then no sooner than a minute after, with
    /// everything given back since.
    #[test]
    fn each_word_asks_every_builder_once_and_says_the_sum_once_a_minute() {
        let (one, one_end) = builder();
        let (two, two_end) = builder();
        let mut held = vec![one, two];
        let mut unsaid = Unsaid::default();
        let t0 = Instant::now();
        // The first word: one answers in time, the other not.
        gave(&one_end, &format!("{} {}", 30 * MIB, 128 * MIB));
        let said = pass(&mut held, &mut unsaid, soon(), t0, &|| None).unwrap();
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
            pass(
                &mut held,
                &mut unsaid,
                soon(),
                t0 + Duration::from_secs(5),
                &|| None
            ),
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
        let said = pass(
            &mut held,
            &mut unsaid,
            soon(),
            t0 + Duration::from_secs(61),
            &|| None,
        )
        .unwrap();
        assert_eq!(held.len(), 1, "the ended builder let go of");
        assert!(was_asked(&two_end));
        assert!(
            said.contains("gave back 10 MiB") && said.contains("in 2 passes"),
            "{said}"
        );
        // A builder whose connection ends while its ask is out is let go of as its answer is
        // waited for: asked, it answers nothing in time; then its connection ends.
        assert_eq!(
            pass(
                &mut held,
                &mut unsaid,
                soon(),
                t0 + Duration::from_secs(62),
                &|| None
            ),
            None
        );
        assert!(was_asked(&two_end));
        drop(two_end);
        assert_eq!(
            pass(
                &mut held,
                &mut unsaid,
                soon(),
                t0 + Duration::from_secs(63),
                &|| None
            ),
            None
        );
        assert!(
            held.is_empty(),
            "the builder whose connection ended while asked let go of"
        );
    }

    /// Held to a budget, a pass asks its builders to give back first, then tells each its
    /// share of the room the budget leaves after, which it holds its build within: what that
    /// holds said with what they gave back.
    #[test]
    fn a_budgeted_pass_tells_each_builder_its_share_of_the_room() {
        let (one, one_end) = builder();
        let (two, two_end) = builder();
        let mut held = vec![one, two];
        let mut unsaid = Unsaid::default();
        gave(&one_end, &format!("{} 0", 3 * MIB));
        gave(&two_end, "0 0");
        for end in [&one_end, &two_end] {
            (&*end).write_all(&run::header(build::kind::HELD, 8)).unwrap();
            (&*end).write_all(b"33554432").unwrap();
        }
        let said = pass(&mut held, &mut unsaid, soon(), Instant::now(), &|| {
            Some(64 * MIB as i64)
        })
        .unwrap();
        for end in [&one_end, &two_end] {
            assert!(was_asked(end), "asked to give back first");
            let mut h = [0u8; run::HEADER];
            (&*end).read_exact(&mut h).unwrap();
            let (kind, len) = run::parse_header(h).unwrap();
            assert_eq!(kind, build::kind::ROOM);
            let mut room = vec![0u8; len as usize];
            (&*end).read_exact(&mut room).unwrap();
            assert_eq!(room, (32 * MIB).to_string().as_bytes(), "half the room each");
        }
        assert!(said.contains("gave back 3 MiB"), "{said}");
        assert!(said.contains("held within 32 MiB past what they use"), "{said}");
    }

    /// A builder whose give-back has no answer in time is told no room at that word, and its
    /// ask counted unanswered once; one that answers why it gave nothing back is said with
    /// its words, kept and asked again at the next word.
    #[test]
    fn a_late_or_refused_answer_keeps_its_builder_and_is_said_once() {
        let (late, late_end) = builder();
        let (refusing, refusing_end) = builder();
        let mut held = vec![late, refusing];
        let mut unsaid = Unsaid::default();
        gave(&refusing_end, "nothing given back: no cgroup");
        (&refusing_end)
            .write_all(&run::header(build::kind::HELD, 0))
            .unwrap();
        let said = pass(&mut held, &mut unsaid, soon(), Instant::now(), &|| {
            Some(64 * MIB as i64)
        })
        .unwrap();
        assert_eq!(held.len(), 2, "both kept");
        assert!(said.contains("1 asks had no answer within 10 s"), "{said}");
        assert!(
            said.contains("; 1 asks were refused, the last: nothing given back: no cgroup"),
            "{said}"
        );
        assert!(was_asked(&late_end));
        late_end.set_nonblocking(true).unwrap();
        let mut more = [0u8; 1];
        assert_eq!(
            (&late_end).read(&mut more).unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "the late one told no room"
        );
        late_end.set_nonblocking(false).unwrap();
        assert!(was_asked(&refusing_end));
        let mut h = [0u8; run::HEADER];
        (&refusing_end).read_exact(&mut h).unwrap();
        assert_eq!(
            run::parse_header(h).map(|(kind, _)| kind),
            Some(build::kind::ROOM)
        );
        let mut room = vec![0u8; run::parse_header(h).unwrap().1 as usize];
        (&refusing_end).read_exact(&mut room).unwrap();
        assert_eq!(room, (32 * MIB).to_string().as_bytes(), "half the room");
        gave(&refusing_end, "0 0");
        pass(&mut held, &mut unsaid, soon(), Instant::now(), &|| None);
        assert!(was_asked(&refusing_end), "the refusing one asked again");
    }

    /// A refusal alone, nothing given back and no ask late, is said all the same.
    #[test]
    fn a_refusal_alone_is_said() {
        let (one, one_end) = builder();
        let mut held = vec![one];
        let mut unsaid = Unsaid::default();
        gave(
            &one_end,
            "nothing given back: memory.reclaim: No such file or directory",
        );
        let said = pass(&mut held, &mut unsaid, soon(), Instant::now(), &|| None).unwrap();
        assert!(
            said.contains("gave back 0 MiB")
                && said.contains("1 asks were refused, the last: nothing given back: memory.reclaim"),
            "{said}"
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
                Instant::now(),
                &|| None
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
