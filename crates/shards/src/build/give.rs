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
//! held against 19 s free, 46 MiB given back in 55 asks). A budget over the build set or
//! changed as it goes is heard as it is written, as the daemon hears its microVMs' (Linux:
//! `memory.high` and `memory.max` of its cgroup and each above it): one that leaves its
//! builders no room is a word of pressure, which a host whose builders idle never says
//! (CI's ubuntu-24.04: a build idling under a budget set 256 MiB short gave back nothing
//! for 126 s, and its read after crawled, throttled, 18 minutes); one that leaves them room,
//! each told its share. What they gave back is said once a minute at most, as the daemon
//! says its microVMs'. Windows watches no pressure yet (D188): there, its builders are
//! asked nothing.

use std::io;
use std::sync::OnceLock;
use std::sync::mpsc;
#[cfg(target_os = "linux")]
use std::sync::{Mutex, PoisonError};
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

/// This process's cgroup's directory in the cgroup v2 hierarchy: none where it has none to
/// be read (macOS).
fn own_cgroup() -> Option<std::path::PathBuf> {
    let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = own.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(std::path::Path::new(crate::daemon::budget::ROOT).join(path.trim_start_matches('/')))
}

/// The room the budget over this process's cgroup leaves its builders, as the daemon works
/// out a run's (`daemon::budget::room`, nothing of theirs evicted): none where no cgroup it
/// is in has a budget, or none is to be read (macOS).
fn room() -> Option<i64> {
    let root = std::path::Path::new(crate::daemon::budget::ROOT);
    crate::daemon::budget::room(&own_cgroup()?, root, &mut |_| 0).map(|(room, _)| room)
}

/// What wakes the watcher: its host's word that it is short of memory, or a budget over the
/// build written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Woke {
    Pressure,
    /// Linux's alone, whose budgets are its cgroups' files.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Budget,
}

/// The watcher, on a thread of its own: its channel, none where its host's pressure cannot
/// be watched or its threads cannot start, its builders then asked nothing. Its host's
/// pressure is heard on a thread of its own, and a budget's writes by the process's watch of
/// its files, each a word down one channel.
fn start(plain: bool) -> Option<mpsc::Sender<Giver>> {
    let mut pressure = Pressure::new().ok()?;
    let (sender, givers) = mpsc::channel();
    let (word, woke) = mpsc::channel();
    let heard = word.clone();
    std::thread::Builder::new()
        .name("pressure".into())
        .spawn(
            move || {
                while pressure.wait().is_ok() && heard.send(Woke::Pressure).is_ok() {}
            },
        )
        .ok()?;
    #[cfg(target_os = "linux")]
    watch_budgets(word);
    #[cfg(not(target_os = "linux"))]
    drop(word);
    std::thread::Builder::new()
        .name("give back".into())
        .spawn(move || watch_words(&woke, &givers, plain))
        .ok()?;
    Some(sender)
}

/// Where a budget's writes are told the watcher, once it has started ([`watch_budgets`]).
#[cfg(target_os = "linux")]
static WRITTEN: OnceLock<Mutex<mpsc::Sender<Woke>>> = OnceLock::new();

/// A budget's file written, as the process's watch of it says ([`watch_budgets`]): told the
/// watcher, on the watch's own thread, which this keeps no longer than a send.
#[cfg(target_os = "linux")]
fn written(_: libc::c_int) {
    if let Some(word) = WRITTEN.get() {
        let _ = word
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(Woke::Budget);
    }
}

/// Watches the budgets over this process's cgroup as the daemon watches its microVMs'
/// (daemon::budget `watch_chain`): a write to the `memory.high` or `memory.max` of its
/// cgroup or of one above it is said down `word`, with the process's one inotify instance
/// (PM M257). A file that cannot be watched holds the build to its budget by its host's
/// pressure alone.
#[cfg(target_os = "linux")]
fn watch_budgets(word: mpsc::Sender<Woke>) {
    if WRITTEN.set(Mutex::new(word)).is_err() {
        return;
    }
    let Some(dir) = own_cgroup() else {
        return;
    };
    let root = std::path::Path::new(crate::daemon::budget::ROOT);
    let mut at = dir.as_path();
    while at.starts_with(root) && at != root {
        for file in ["memory.high", "memory.max"] {
            if let Ok(f) = std::fs::File::open(at.join(file)) {
                let _ = shards_vmm::platform::watch_writes(&f, written);
            }
        }
        let Some(up) = at.parent() else {
            return;
        };
        at = up;
    }
}

/// Asks its builders to give back each time its host is short of memory, and as a budget
/// over the build is written ([`budget_written`]), until neither can be heard; while an
/// answer is out past its pass's patience, it looks for it once a pressure window
/// ([`late`]).
fn watch_words(woke: &mpsc::Receiver<Woke>, givers: &mpsc::Receiver<Giver>, plain: bool) {
    let mut held: Vec<Giver> = Vec::new();
    let mut unsaid = Unsaid::default();
    loop {
        let why = if held.iter().any(Giver::awaiting) {
            match woke.recv_timeout(shards_vmm::platform::PRESSURE_WINDOW) {
                Ok(why) => Some(why),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        } else {
            let Ok(why) = woke.recv() else {
                return;
            };
            Some(why)
        };
        held.extend(givers.try_iter());
        let now = Instant::now();
        let until = now.checked_add(PATIENCE).unwrap_or(now);
        let said = match why {
            Some(Woke::Pressure) => pass(&mut held, &mut unsaid, until, now, &room),
            Some(Woke::Budget) => budget_written(&mut held, &mut unsaid, until, now, &room),
            None => late(&mut held, &mut unsaid, until, now, &room),
        };
        if let (Some(said), true) = (said, plain) {
            use std::io::Write as _;
            let _ = writeln!(io::stderr(), "{said}");
        }
    }
}

/// A budget over the build written, heard `now`: one that leaves its builders no room is a
/// word of pressure ([`pass`]), what they hold cold given back first, as their host must
/// take it back; one that leaves them room, or none left (a budget gone), each told its
/// share of it, nothing asked back, by `until`.
fn budget_written(
    held: &mut Vec<Giver>,
    unsaid: &mut Unsaid,
    until: Instant,
    now: Instant,
    room: &dyn Fn() -> Option<i64>,
) -> Option<String> {
    let left = room();
    if left.is_some_and(|r| r < 0) {
        return pass(held, unsaid, until, now, room);
    }
    tell(held, left, until, unsaid);
    unsaid.say(now)
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
        tell(held, Some(r), until, unsaid);
    }
    unsaid.say(now)
}

/// The answers out past their pass's patience, as far as they have come by `now`: each
/// counted as its pass would have counted it, a builder whose connection has ended let go
/// of; and where a give-back came, each builder told its share of the room a budget leaves,
/// as its pass would have told it, heard by `until`. A builder held by its host's throttle
/// answered in 9 s and more, past its pass (CI's ubuntu-24.04, PM M350). Said at once,
/// whatever was said a moment before: their pass's line went out without them.
fn late(
    held: &mut Vec<Giver>,
    unsaid: &mut Unsaid,
    until: Instant,
    now: Instant,
    room: &dyn Fn() -> Option<i64>,
) -> Option<String> {
    let mut came = false;
    let mut gave = false;
    held.retain_mut(|g| {
        if !g.awaiting() {
            return true;
        }
        match g.answer(now) {
            Ok(Some(answer)) => {
                came = true;
                gave |= matches!(answer, Answer::Gave(..));
                count(unsaid, answer);
                true
            }
            Ok(None) => true,
            Err(_) => false,
        }
    });
    if !came {
        return None;
    }
    if gave && let Some(r) = room() {
        tell(held, Some(r), until, unsaid);
    }
    unsaid.late = true;
    unsaid.say(now)
}

/// Tells each builder in `held` its share of the room `r` a budget leaves them, none where
/// no budget holds them, each answer heard by `until`: those told alone heard, as one whose
/// give-back is still out was counted unanswered.
fn tell(held: &mut Vec<Giver>, r: Option<i64>, until: Instant, unsaid: &mut Unsaid) {
    let n = i64::try_from(held.len().max(1)).unwrap_or(i64::MAX);
    let share = r.map(|r| r / n);
    let mut told = Vec::with_capacity(held.len());
    held.retain_mut(|g| g.tell(share).map(|went| told.push(went)).is_ok());
    let mut told = told.into_iter();
    held.retain_mut(|g| !told.next().unwrap_or(false) || hear(g, until, unsaid));
}

/// The answer to `g`'s ask out, counted in `unsaid`, by `until`: whether its builder is
/// kept, which one whose connection has ended is not.
fn hear(g: &mut Giver, until: Instant, unsaid: &mut Unsaid) -> bool {
    match g.answer(until) {
        Ok(Some(answer)) => {
            count(unsaid, answer);
            true
        }
        Ok(None) => {
            unsaid.unanswered += 1;
            true
        }
        Err(_) => false,
    }
}

/// A builder's answer counted in `unsaid`.
fn count(unsaid: &mut Unsaid, answer: Answer) {
    match answer {
        Answer::Gave(freed, unplugging) => {
            unsaid.freed = unsaid.freed.saturating_add(freed);
            unsaid.unplugging = unsaid.unplugging.saturating_add(unplugging);
            unsaid.answered += 1;
        }
        Answer::Held(within) => unsaid.held = within,
        Answer::Refused(why) => {
            unsaid.refused += 1;
            unsaid.why = Some(why);
        }
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
    /// Whether what it holds came past its pass's patience ([`late`]).
    late: bool,
    said: Option<Instant>,
}

impl Unsaid {
    /// What builders gave back since it was last said, where they gave back anything, or
    /// any went unanswered or said why not, at most once in [`SAY_EVERY`]; counted afresh
    /// after.
    fn say(&mut self, now: Instant) -> Option<String> {
        if !self.late
            && self
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
            "the host is short of memory: the build's builders gave back {} MiB they held cold of their files, and asked to unplug {} MiB they held plugged past twice what they use and their peak since, ",
            self.freed >> 20,
            self.unplugging >> 20,
        );
        if self.passes == 0 {
            said.push_str(&format!(
                "answering past the {} s their pass waited",
                PATIENCE.as_secs()
            ));
        } else {
            said.push_str(&format!("in {} passes over {over:?}", self.passes));
        }
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

    /// A budget written as the build goes: one that leaves no room is a word of pressure, its
    /// builders asked to give back first and then told their share of it; one that leaves
    /// room, each told its share alone, nothing asked back and nothing said; one gone, each
    /// told it has none.
    #[test]
    fn a_budget_written_asks_back_only_what_it_leaves_no_room_for() {
        let read_room = |end: &UnixStream| -> Vec<u8> {
            let mut h = [0u8; run::HEADER];
            (&*end).read_exact(&mut h).unwrap();
            let (kind, len) = run::parse_header(h).unwrap();
            assert_eq!(kind, build::kind::ROOM);
            let mut room = vec![0u8; len as usize];
            (&*end).read_exact(&mut room).unwrap();
            room
        };
        let answer_held = |end: &UnixStream, within: &str| {
            let len = u32::try_from(within.len()).unwrap();
            (&*end).write_all(&run::header(build::kind::HELD, len)).unwrap();
            (&*end).write_all(within.as_bytes()).unwrap();
        };
        let (one, end) = builder();
        let mut held = vec![one];
        let mut unsaid = Unsaid::default();
        let short = -i64::try_from(MIB).unwrap();
        gave(&end, &format!("{} 0", 5 * MIB));
        answer_held(&end, "0");
        let said = budget_written(&mut held, &mut unsaid, soon(), Instant::now(), &|| Some(short)).unwrap();
        assert!(was_asked(&end), "asked to give back first");
        assert_eq!(read_room(&end), short.to_string().as_bytes());
        assert!(said.contains("gave back 5 MiB"), "{said}");
        let roomy = i64::try_from(64 * MIB).unwrap();
        answer_held(&end, "67108864");
        let said = budget_written(&mut held, &mut unsaid, soon(), Instant::now(), &|| Some(roomy));
        assert_eq!(read_room(&end), roomy.to_string().as_bytes());
        assert!(!was_asked(&end), "nothing asked back");
        assert_eq!(said, None);
        answer_held(&end, "");
        budget_written(&mut held, &mut unsaid, soon(), Instant::now(), &|| None);
        assert_eq!(read_room(&end), b"", "no budget");
        assert!(!was_asked(&end));
        assert_eq!(held.len(), 1);
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

    /// An answer past its pass's patience is heard as it comes, counted as its pass would
    /// have counted it and said at once, however lately its pass's line went out, its
    /// builder then told its share of the room, as its pass would have told it (PM M350).
    #[test]
    fn a_late_answer_is_heard_as_it_comes_and_said_at_once() {
        let (slow, slow_end) = builder();
        let mut held = vec![slow];
        let mut unsaid = Unsaid::default();
        let short = || Some(-(64 * MIB as i64));
        let said = pass(&mut held, &mut unsaid, soon(), Instant::now(), &short).unwrap();
        assert!(said.contains("gave back 0 MiB"), "{said}");
        assert!(said.contains("1 asks had no answer within 10 s"), "{said}");
        assert!(was_asked(&slow_end));
        assert!(held[0].awaiting());
        assert_eq!(
            late(&mut held, &mut unsaid, soon(), Instant::now(), &short),
            None,
            "nothing come, nothing said"
        );
        gave(&slow_end, &format!("{} 0", 503 * MIB));
        (&slow_end).write_all(&run::header(build::kind::HELD, 7)).unwrap();
        (&slow_end).write_all(b"3407872").unwrap();
        let said = late(&mut held, &mut unsaid, soon(), Instant::now(), &short).unwrap();
        assert!(said.contains("gave back 503 MiB"), "{said}");
        assert!(
            said.contains("answering past the 10 s their pass waited"),
            "{said}"
        );
        assert!(said.contains("held within 3 MiB past what they use"), "{said}");
        let mut h = [0u8; run::HEADER];
        (&slow_end).read_exact(&mut h).unwrap();
        let (kind, len) = run::parse_header(h).unwrap();
        assert_eq!(kind, build::kind::ROOM, "told its room once it answered");
        let mut room = vec![0u8; len as usize];
        (&slow_end).read_exact(&mut room).unwrap();
        assert_eq!(room, (-(64 * MIB as i64)).to_string().as_bytes());
        assert!(!held[0].awaiting());
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
