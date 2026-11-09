//! What a build's builders give back as their host runs short of memory (D188 for D133's
//! builders): the build's process watches its host's memory pressure as the daemon does
//! ([`Pressure`]), and at each word of it asks each builder it has to give back what
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
//! each told its share. What they gave back is said as a pressure begins and then as it has
//! gone on as long again (`pressure::Saying`), as the daemon says its microVMs'. While a budget holds the build, its builders are told its room again
//! each pressure window where it has changed: their guests hold the build within what they
//! were last told, past what it used then, and what their host gets back of them outside the
//! build (memory they unplug, free pages reported late) the build's growth never shows, so
//! that a build told 44 MiB of room as its builder's unplug of 128 MiB was still under way
//! was held within it for 200 s, its host 140 MiB short of its budget throughout (Fedora,
//! apt 219 s held against 31.5 s free, PM M442). On Windows its host's word is the system's
//! low memory notification, as the daemon's is (D188).

use std::io;
use std::sync::OnceLock;
use std::sync::mpsc;
#[cfg(target_os = "linux")]
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::pressure::{Pressure, Saying};

use super::builder::{Answer, Giver};

/// How long builders' answers are waited for at a word of pressure: until the next word
/// can come, a pressure window on (the window the host's pressure is measured over, which a
/// pressure that goes on is heard again after: `platform::PRESSURE_WINDOW`). One not come by
/// then is said to have had none, heard as it comes after ([`late`], looked for once a
/// window), and its builder asked again only once it has answered: a pass never holds up the
/// next word's.
const PATIENCE: Duration = shards_vmm::platform::PRESSURE_WINDOW;
/// Where it names a FIFO (on Windows, a named pipe), a build's memory pressure is what is
/// written to it, each line the host's word that it is short of memory, in place of its
/// kernel's: so that a test tells it where no unprivileged process may raise the system's
/// pressure (macOS, Windows).
const TOLD: &str = "SHARDS_BUILD_PRESSURE";

/// The watcher's channels: each builder's giver sent down the first as the builder starts,
/// and its coming said down the second, which wakes the watcher; started with the first,
/// none where it could not start.
static WATCHER: OnceLock<Option<(mpsc::Sender<Giver>, mpsc::Sender<Woke>)>> = OnceLock::new();

/// Has the build's memory pressure watcher ask `giver`'s builder to give back at its host's
/// word, starting the watcher with the first builder; `plain`, whether what they gave back
/// is said on the build's stderr, where its plain display's lines go. A build held to a
/// budget tells its builder the budget's room first, so that its guest holds itself within
/// it from its first step, which waits for no answer.
pub fn watch(mut giver: Giver, plain: bool) {
    if let Some(r) = room() {
        let _ = giver.tell_first(r);
    }
    if let Some((sender, word)) = WATCHER.get_or_init(|| start(plain))
        && sender.send(giver).is_ok()
    {
        let _ = word.send(Woke::Builder);
    }
}

/// This process's cgroup's directory in the cgroup v2 hierarchy: none where it has none to
/// be read (macOS).
fn own_cgroup() -> Option<std::path::PathBuf> {
    let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = own.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(std::path::Path::new(crate::daemon::budget::ROOT).join(path.trim_start_matches('/')))
}

/// The room the budget over this process's cgroup leaves its builders, as its kernel
/// charges it (`daemon::budget::charged_room`: the file pages their processes map counted
/// where charged to it, not wherever, as a run's are): none where no cgroup it is in has a
/// budget, or none is to be read (macOS).
fn room() -> Option<i64> {
    let root = std::path::Path::new(crate::daemon::budget::ROOT);
    crate::daemon::budget::charged_room(&own_cgroup()?, root).map(|(room, _)| room)
}

/// What wakes the watcher: its host's word that it is short of memory, a budget over the
/// build written, or a builder come, told its budget's room as it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Woke {
    Pressure,
    /// Linux's alone, whose budgets are its cgroups' files.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Budget,
    Builder,
}

/// The watcher, on a thread of its own: its channel, none where its host's pressure cannot
/// be watched or its threads cannot start, its builders then asked nothing. Its host's
/// pressure is heard on a thread of its own, and a budget's writes by the process's watch of
/// its files, each a word down one channel.
fn start(plain: bool) -> Option<(mpsc::Sender<Giver>, mpsc::Sender<Woke>)> {
    let mut pressure = Pressure::new(TOLD).ok()?.0;
    let (sender, givers) = mpsc::channel();
    let (word, woke) = mpsc::channel();
    let heard = word.clone();
    let come = word.clone();
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
    Some((sender, come))
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
/// ([`late`]); while a budget holds them, it looks at the room it leaves them once a window
/// too ([`again`]).
fn watch_words(woke: &mpsc::Receiver<Woke>, givers: &mpsc::Receiver<Giver>, plain: bool) {
    let mut held: Vec<Giver> = Vec::new();
    let mut unsaid = Unsaid::default();
    loop {
        let why = if held.iter().any(Giver::awaiting) || unsaid.told.is_some() {
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
            // Told its room as it started (`watch`), the room it then had.
            Some(Woke::Builder) => {
                if unsaid.told.is_none() {
                    unsaid.told = room();
                }
                None
            }
            None => late(&mut held, &mut unsaid, until, now, &room)
                .or_else(|| again(&mut held, &mut unsaid, until, now, &room)),
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
    unsaid.budgeted = true;
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
    unsaid.saying.heard(now);
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

/// The room a budget leaves its builders, looked at again a pressure window after it was
/// last told them, `now`: where it is short, a word of pressure ([`pass`]), as a budget
/// written short is; where it has changed, each builder told its share of it, heard by
/// `until`. A guest holds its build within what it was last told past what the build used
/// then, and its host's getting back what it holds outside the build (blocks it unplugs,
/// free pages reported after its answer) makes room the build's own growth never shows:
/// told 44 MiB as its unplug of 128 MiB was under way, a builder held its build within it
/// for 200 s while its budget had 140 MiB to spare (PM M442). Nothing where no budget held
/// them when last told.
fn again(
    held: &mut Vec<Giver>,
    unsaid: &mut Unsaid,
    until: Instant,
    now: Instant,
    room: &dyn Fn() -> Option<i64>,
) -> Option<String> {
    let was = unsaid.told?;
    match room() {
        Some(r) if r < 0 => pass(held, unsaid, until, now, room),
        Some(r) if r != was => {
            tell(held, Some(r), until, unsaid);
            unsaid.say(now)
        }
        // The budget gone is heard as it is written (`budget_written`), the room unchanged
        // has nothing to say.
        _ => None,
    }
}

/// Tells each builder in `held` its share of the room `r` a budget leaves them, none where
/// no budget holds them, each answer heard by `until`: those told alone heard, as one whose
/// give-back is still out was counted unanswered. The room told kept in `unsaid`.
fn tell(held: &mut Vec<Giver>, r: Option<i64>, until: Instant, unsaid: &mut Unsaid) {
    unsaid.told = r;
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
    /// Whether a budget over the build was written since it was last said: what its builders
    /// give back under it is said at once, where a word of its host's pressure alone, said
    /// a moment before the budget came, held it back (then a minute), past the build's end (CI
    /// 40925092313).
    budgeted: bool,
    /// When it is due to be said, as the pressure goes on.
    saying: Saying,
    /// The room a budget left its builders as they were last told it, all of them together:
    /// none where no budget held them. Kept past what is said.
    told: Option<i64>,
}

impl Unsaid {
    /// What builders gave back since it was last said, where they gave back anything, or
    /// any went unanswered or said why not, as the pressure is due to be said ([`Saying`]);
    /// counted afresh after.
    fn say(&mut self, now: Instant) -> Option<String> {
        if !self.late && !self.budgeted && !self.saying.due(now) {
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
            said.push_str(&format!("answering past the {:?} their pass waited", PATIENCE));
        } else {
            said.push_str(&format!("in {} passes over {over:?}", self.passes));
        }
        if self.unanswered > 0 {
            said.push_str(&format!(
                "; {} asks had no answer within {:?}",
                self.unanswered, PATIENCE
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
        let mut saying = self.saying;
        saying.said(now);
        *self = Unsaid {
            saying,
            told: self.told,
            ..Unsaid::default()
        };
        Some(said)
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
    /// What they gave back is said at once, then no sooner than the pressure has gone on as
    /// long again (a window, as it began), with
    /// everything given back since.
    #[test]
    fn each_word_asks_every_builder_once_and_says_the_sum_as_the_pressure_doubles() {
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
        assert!(
            said.contains(&format!("1 asks had no answer within {PATIENCE:?}")),
            "{said}"
        );
        // The second word: the late one answers; it is asked nothing until it has, the first
        // asked again; within the window, nothing said.
        gave(&two_end, &format!("{} 0", 7 * MIB));
        gave(&one_end, &format!("{} 0", MIB));
        assert_eq!(
            pass(&mut held, &mut unsaid, soon(), t0 + PATIENCE / 2, &|| None),
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
        let said = pass(&mut held, &mut unsaid, soon(), t0 + PATIENCE, &|| None).unwrap();
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
                t0 + PATIENCE + Duration::from_millis(1),
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
                t0 + PATIENCE + Duration::from_millis(2),
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

    /// What builders give back under a budget written as the build goes is said at once,
    /// whatever was said a moment before: a word of its host's pressure heard before a test
    /// set its budget said what little was given back then, and the give-back under the
    /// budget went unsaid for the minute after, past the build's end (CI 40925092313's
    /// a_build_held_short_of_memory_has_its_builder_give_back).
    #[test]
    fn what_is_given_back_under_a_budget_written_is_said_at_once() {
        let read_room = |end: &UnixStream| {
            let mut h = [0u8; run::HEADER];
            (&*end).read_exact(&mut h).unwrap();
            let (kind, len) = run::parse_header(h).unwrap();
            assert_eq!(kind, build::kind::ROOM);
            let mut room = vec![0u8; len as usize];
            (&*end).read_exact(&mut room).unwrap();
        };
        let answer_held = |end: &UnixStream, within: &str| {
            let len = u32::try_from(within.len()).unwrap();
            (&*end).write_all(&run::header(build::kind::HELD, len)).unwrap();
            (&*end).write_all(within.as_bytes()).unwrap();
        };
        let (one, end) = builder();
        let mut held = vec![one];
        let mut unsaid = Unsaid::default();
        let t0 = Instant::now();
        // Its host's word, no budget yet: what little was given back, said.
        gave(&end, &format!("{} 0", MIB / 2));
        let said = pass(&mut held, &mut unsaid, soon(), t0, &|| None).unwrap();
        assert!(was_asked(&end));
        assert!(
            said.contains("gave back 0 MiB") && !said.contains("held within"),
            "{said}"
        );
        // A budget written a second on, leaving room: its share told, nothing said.
        let roomy = i64::try_from(64 * MIB).unwrap();
        answer_held(&end, "67108864");
        let after = t0 + Duration::from_secs(1);
        assert_eq!(
            budget_written(&mut held, &mut unsaid, soon(), after, &|| Some(roomy)),
            None
        );
        read_room(&end);
        // Its host's next word, under it: what was given back said at once, with its room.
        gave(&end, &format!("{} 0", 100 * MIB));
        answer_held(&end, "67108864");
        let after = t0 + Duration::from_secs(2);
        let said = pass(&mut held, &mut unsaid, soon(), after, &|| Some(roomy)).unwrap();
        assert!(was_asked(&end));
        read_room(&end);
        assert!(said.contains("gave back 100 MiB"), "{said}");
        assert!(said.contains("held within 64 MiB past what they use"), "{said}");
        // And it is not said again until the pressure has gone on as long again.
        gave(&end, &format!("{} 0", MIB));
        answer_held(&end, "67108864");
        let after = t0 + Duration::from_secs(3);
        assert_eq!(pass(&mut held, &mut unsaid, soon(), after, &|| Some(roomy)), None);
        assert!(was_asked(&end));
        read_room(&end);
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
        assert!(
            said.contains(&format!("1 asks had no answer within {PATIENCE:?}")),
            "{said}"
        );
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
        assert!(
            said.contains(&format!("1 asks had no answer within {PATIENCE:?}")),
            "{said}"
        );
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
            said.contains(&format!("answering past the {PATIENCE:?} their pass waited")),
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

    /// While a budget holds its builders, the room it leaves is looked at again each window:
    /// told them where it has changed since they were last told it, a word of pressure where
    /// it is short, and nothing where it is unchanged or no budget held them (PM M442).
    #[test]
    fn a_budgets_room_is_told_again_where_it_changed() {
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
        let nothing_sent = |end: &UnixStream| {
            end.set_nonblocking(true).unwrap();
            let mut more = [0u8; 1];
            assert_eq!(
                (&*end).read(&mut more).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            end.set_nonblocking(false).unwrap();
        };
        let (one, end) = builder();
        let mut held = vec![one];
        let mut unsaid = Unsaid::default();
        let mib = |n: i64| n * i64::try_from(MIB).unwrap();
        // No budget held it when last told: nothing looked at.
        assert_eq!(
            again(&mut held, &mut unsaid, soon(), Instant::now(), &|| Some(mib(64))),
            None
        );
        nothing_sent(&end);
        // Told 44 MiB; its host then got back what it unplugged: told the room it has now.
        unsaid.told = Some(mib(44));
        answer_held(&end, &mib(140).to_string());
        assert_eq!(
            again(&mut held, &mut unsaid, soon(), Instant::now(), &|| Some(mib(140))),
            None
        );
        assert_eq!(read_room(&end), mib(140).to_string().as_bytes());
        assert_eq!(unsaid.told, Some(mib(140)));
        // Unchanged: nothing told.
        assert_eq!(
            again(&mut held, &mut unsaid, soon(), Instant::now(), &|| Some(mib(140))),
            None
        );
        nothing_sent(&end);
        // Short: asked to give back first, then told the room after.
        gave(&end, &format!("{} 0", 9 * MIB));
        answer_held(&end, "0");
        let said = again(&mut held, &mut unsaid, soon(), Instant::now(), &|| Some(-mib(1))).unwrap();
        assert!(was_asked(&end), "asked to give back");
        assert_eq!(read_room(&end), (-mib(1)).to_string().as_bytes());
        assert!(said.contains("gave back 9 MiB"), "{said}");
        assert_eq!(unsaid.told, Some(-mib(1)), "kept past what is said");
        // The budget gone, as its write told them: looked at no more.
        answer_held(&end, "");
        budget_written(&mut held, &mut unsaid, soon(), Instant::now(), &|| None);
        assert_eq!(read_room(&end), b"");
        assert_eq!(unsaid.told, None);
        assert_eq!(
            again(&mut held, &mut unsaid, soon(), Instant::now(), &|| Some(mib(64))),
            None
        );
        nothing_sent(&end);
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
}
