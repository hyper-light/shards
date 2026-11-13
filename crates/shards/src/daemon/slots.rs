//! The home's request slots (docs/design/architecture.md D185): how the daemon's own clients
//! reach it. A slot's session is bridged to the daemon's handling of a client: a connection
//! of its own, which the daemon takes as a client's, and whose other end the bridge holds.
//! The request's descriptors are the slot's streams; the client's messages go on to the
//! daemon, and the daemon's back to the client, without descriptors, each side's end the
//! other's.

use std::collections::HashMap;
use std::io;
#[cfg(unix)]
use std::os::fd::AsRawFd as _;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use shards_ipc::Stream as UnixStream;
use shards_ipc::fd::{AsFd, BorrowedFd, OwnedFd};
use shards_ipc::kind;
use shards_ipc::slots::{self, Asked, Frame, Requested, Requests, Session, Slot, Stopper, Waited};

use super::{Daemon, Threads, lock, log};
use crate::containers::Disk;

/// How long a slot a client took may go without its request before a sweep looks it over:
/// one taken by a client whose process ended in between is removed at the second sweep
/// that finds it so, and its place offered anew. Only such a client's slot waits so long;
/// a live one writes as it opens its slot. A sweep is due only while a slot is taken and
/// its request not read: an idle daemon's maker sleeps until something happens.
const SWEEP: Duration = Duration::from_secs(60);

/// What the daemon has seen of its clients, which its waits on them follow (D185's
/// admission), never a figure picked:
/// - **The rest of a request** comes with its first byte: a client writes its request in one
///   write (`Client::ask`), so its rest is there as the daemon reads it, and waits on no
///   client: it teaches nothing, as an answer's write the FIFO takes at once does not. Only a
///   rest its client was waited on for (`Asked::waited`: a request larger than a FIFO holds,
///   coming in parts, or a client paused past its first byte) teaches how long a client goes
///   on with one. A client that sends nothing past its first byte is waited for as long as
///   the slowest such rest seen whole, and then cut: told RESTART, or finding its slot's
///   `say` gone as it writes (BrokenPipe), it asks again through the next slot, its request
///   begun nowhere. Before any, the wait follows the client alone (R03's cold start): its
///   end, its FIFO's, or the daemon ending its clients. Before, every rest taught, and the
///   first request's, read 2.334 µs after its first byte on a KVM x86_64 host, cut each
///   client after it that paused there (D185). A request whose rest has begun is waited
///   for until it is whole or its client ends: one larger than a FIFO holds comes in parts,
///   each as the daemon reads and its client is next run, a gap no whole request's rest
///   measures, which a window learned from them would cut again each time it was asked
///   (PM M545).
/// - **A client taking its answer** goes on as it takes: its answer is written as its FIFO
///   takes it, and only where the FIFO takes none of what is left is its client waited on,
///   until it makes room (`slots::write_all_waiting`). Each such wait that ends is a take,
///   and the slowest is what bounds a stall: a closed daemon lets go of a client that has
///   waited longer than the slowest take seen with nothing taken, and holds one that takes,
///   however slowly the answer goes as a whole. What the FIFO takes at once waits on no
///   client and teaches nothing. Before any take, the daemon follows the client alone:
///   its end, its FIFO's. On Windows a named pipe says nothing of its room before a write,
///   so its answer goes in parts no larger than its line's outbound quota, and a part whose
///   write pends is the take, from its pend to its completion (`slots::named`; D185). It was
///   twice the longest a piece took, at least TCP's R2 (100 s); and on Windows, no take,
///   a closed daemon holding a client that took nothing until it ended (PM M549).
#[derive(Debug, Default)]
pub(crate) struct Seen {
    /// The slowest a request's rest was read whole in, from its first byte, of those its
    /// client was waited on for; none before any.
    slowest_rest: Option<Duration>,
    /// The slowest a client took to make room for its answer's next part; none before any.
    slowest_take: Option<Duration>,
}

impl Seen {
    /// How long a request's rest is waited for while none of it has come: the slowest rest
    /// its client was waited on for that has come whole; none before any, the client's end
    /// alone then.
    pub(super) fn request_stall(&self) -> Option<Duration> {
        self.slowest_rest
    }

    /// A request its client was waited on for came whole `took` after its first byte.
    pub(super) fn whole(&mut self, took: Duration) {
        self.slowest_rest = Some(self.slowest_rest.map_or(took, |slowest| slowest.max(took)));
    }

    /// A client made room for its answer's next part `took` after it was waited on.
    pub(super) fn taken(&mut self, took: Duration) {
        self.slowest_take = Some(self.slowest_take.map_or(took, |slowest| slowest.max(took)));
    }

    /// How long a closed daemon waits on a client that takes nothing of its answer: the
    /// slowest take seen; none before any, the client's end alone then.
    pub(super) fn answer_wait(&self) -> Option<Duration> {
        self.slowest_take
    }
}

/// What the slots' maker hears.
#[derive(Debug)]
pub(super) enum Event {
    /// Slot `n`'s client began its request: its names go, and another is offered in its
    /// place at once, whether or not the rest of its request ever comes.
    Asked(u64),
    /// Slot `n`'s wait ended without a request.
    Ended(u64),
    /// A client left: there may be room to offer what back-pressure held back.
    Room,
    /// The lists may have gone: a name of the home came or went, as a removal of the home's
    /// takes `requests/`, or a daemon waiting for the lock found none (`take_lock`), as one
    /// removed by hand leaves the next client. The maker looks, and makes them again.
    Look,
    /// The daemon closed: no slot is offered from here on.
    Close,
}

/// A slot offered: whether it has streams, what ends its wait, and whether a sweep has found
/// it taken.
struct Offered {
    streams: bool,
    stopper: Stopper,
    taken: bool,
}

/// A slot the daemon offered, from its offer until its serving ends ([`Daemon::live`]):
/// whatever client took it is owed its answer, or RESTART, before the daemon exits
/// ([`Daemon::slots_hold`]).
#[derive(Debug, Clone, Copy)]
pub(super) struct Live {
    /// Opened by its client: its request is being read, answered, or passed on.
    session: bool,
    /// Its request went on to the daemon's handling, whose answer it passes back.
    passed: bool,
    /// Since when its answer's writer has waited on its client to make room, its FIFO
    /// taking nothing (`slots::write_all_waiting`): none while it writes, or has nothing to
    /// write.
    pub(super) waiting: Option<Instant>,
    /// Its answer's last word (`EXIT`, `END`, `RESTART`) is its client's: there in its FIFO
    /// whether or not the daemon is (pipe(7)), and nothing its client reads comes after it.
    answered: bool,
}

impl<D: Disk> Daemon<D> {
    /// Offers the slots of `requests`, the home's, and serves each slot's client on a
    /// thread of `threads`'s, until the daemon closes.
    pub(super) fn serve_slots<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, requests: Requests) {
        let (tell, heard) = mpsc::channel();
        *lock(&self.slot_events) = Some(tell.clone());
        let spawned = std::thread::Builder::new()
            .name("slots".into())
            .spawn_scoped(threads, move || self.make_slots(threads, requests, &tell, &heard));
        if let Err(e) = spawned {
            log(format!("the requests' slots: {e}; none is offered"));
        }
    }

    /// Tells the slots' maker `event`, where it serves.
    pub(super) fn tell_slots(&self, event: Event) {
        if let Some(tell) = lock(&self.slot_events).as_ref() {
            let _ = tell.send(event);
        }
    }

    /// Whether a slot holds the daemon from being quiet, and so from exiting; and, once it
    /// has closed, when the first that does lets go of it. While it serves, a slot its
    /// client opened holds it. Once it has closed, each its client opened holds it until
    /// its serving ends, its answer's last word is its client's, or its client has taken
    /// nothing of its answer for longer than the slowest take seen (`Seen::answer_wait`),
    /// its answer's writer waiting on it that long; before any take, until it ends. A client
    /// reading its answer makes room as it reads, however slowly; one stopped, or held by a
    /// pager that waits, takes nothing, and keeps no daemon from exiting nor the next from
    /// starting past that wait. One whose request went on, with nothing of its answer to
    /// take yet, takes nothing it could: it holds the daemon while the daemon has yet to
    /// answer, as an attached run its stop ends after its timeout says nothing meanwhile (PM
    /// M331). One whose request has not gone on holds it until its serving ends: its
    /// request's wait is its own (`serve_slot`: the requests' record, and none once the
    /// daemon has closed), never the answers'. A slot no client opened has no client.
    pub(super) fn slots_hold(&self) -> (bool, Option<Instant>) {
        let closed = self.closed.load(Ordering::SeqCst);
        let wait = lock(&self.seen).answer_wait();
        let now = Instant::now();
        let mut held = false;
        let mut lets_go: Option<Instant> = None;
        for live in lock(&self.live).values() {
            if !closed {
                held |= live.session;
                continue;
            }
            let since = match (live.waiting, live.passed, live.answered) {
                (Some(since), _, _) => since,
                (None, true, _) => {
                    held = true;
                    continue;
                }
                (None, false, true) => continue,
                (None, false, false) => {
                    held |= live.session;
                    continue;
                }
            };
            // No take seen yet: its client's end alone lets it go.
            let Some(wait) = wait else {
                held = true;
                continue;
            };
            match since.checked_add(wait) {
                Some(at) if at <= now => {}
                at => {
                    held = true;
                    lets_go = lets_go.into_iter().chain(at).min();
                }
            }
        }
        (held, lets_go)
    }

    /// Says which slots' clients the daemon exits without: those its answer's writer waits
    /// on, each past the slowest take seen ([`slots_hold`](Self::slots_hold)). The other
    /// slots it exits with are no client's it leaves: their answer's last word is their
    /// client's, or no client opened them.
    pub(super) fn leave_slots(&self) {
        for (n, live) in lock(&self.live).iter() {
            if let Some(since) = live.waiting {
                log(format!(
                    "request slot {n}: its client took no more of its answer in {:?}; exiting without it",
                    since.elapsed()
                ));
            }
        }
    }

    /// The slots' maker: keeps [`slots::OFFERED`] slots of each kind offered, without streams
    /// and with: one for the next client, and one for a client that comes as the first's
    /// replacement is made (making and removing a slot of five FIFOs is 317 µs of the
    /// daemon's CPU on a loaded Mac, of two 125 µs, PM M255), so that none is made ahead
    /// that no client asked for; clients past them wait on their list, in turn, a making
    /// each. As long as the daemon has room for another client (its most in hand,
    /// `max_clients`: past that, clients wait on their list for room). It wakes only for
    /// what it hears, for a retry while out of descriptors, and for a sweep while a slot
    /// is taken with no request read ([`SWEEP`]): never on a timer of its own.
    fn make_slots<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        mut requests: Requests,
        tell: &mpsc::Sender<Event>,
        heard: &mpsc::Receiver<Event>,
    ) {
        let mut offered: HashMap<u64, Offered> = HashMap::new();
        // Out of descriptors since when, a slot being FIFOs and their ends: said once, and
        // offered again as the daemon is told descriptors were let go of (`Event::Room`, as a
        // client, a run or a connection ends), never on a clock. Clients meanwhile wait on
        // their lists, a backlog that keeps nothing of the daemon's ready, as a socket's
        // client left in its listener's backlog does (`refuse_api`).
        let mut starved_since: Option<Instant> = None;
        // Since when a slot could not be offered otherwise (its FIFOs not made, its thread not
        // started), and when it is tried again: said once, and tried again after a random
        // share of how long the trouble has lasted so far (D222's `Trouble`, full jitter: no
        // more lost past its end than it had lasted), as nothing else may wake it while every
        // client waits on a list it left short.
        let mut failing: Option<(Instant, Instant)> = None;
        // Since when a slot has been taken with no request read: what the sweep's due by.
        let mut taken_since: Option<Instant> = None;
        // Whether to look at the lists: at first, and as the listener asks ([`Event::Look`]).
        let mut look = true;
        loop {
            // Its lists removed as it serves: made again, as a removed socket was bound
            // again, and what was offered on them let go of. Under `socket_at`, which
            // `close` holds as it closes and unoffers them: read apart, a close between the
            // two looks would have its lists made again, a daemon told to stop offering
            // again (as one listened again, PM M188).
            if std::mem::take(&mut look) {
                let _at = lock(&self.socket_at);
                if !self.closed.load(Ordering::SeqCst) && !requests.listed() {
                    let_go(&requests, &mut offered);
                    match requests.renew() {
                        Ok(()) => log(format!(
                            "{} was removed; offering slots there again",
                            self.home.join(slots::DIR).display()
                        )),
                        Err(e) => log(format!("{}: {e}", self.home.join(slots::DIR).display())),
                    }
                }
            }
            for streams in [false, true] {
                // As many as its list holds untaken: a slot taken by a client that died
                // before its request holds no client's place until the sweep removes it.
                while untaken(&requests, &offered, streams) < slots::OFFERED
                    && self.busy.load(Ordering::SeqCst) < self.max_clients
                    && !self.closed.load(Ordering::SeqCst)
                    && failing.is_none_or(|(_, again)| Instant::now() >= again)
                {
                    let tried = Instant::now();
                    match self.offer_slot(threads, &mut requests, tell, streams) {
                        Ok((n, made)) => {
                            offered.insert(n, made);
                            if let Some(since) = starved_since.take() {
                                log(format!("offering slots again, after {:?}", since.elapsed()));
                                self.have_room();
                            }
                            if let Some((since, _)) = failing.take() {
                                log(format!("offering slots again, after {:?}", since.elapsed()));
                            }
                        }
                        Err(e) if crate::os::out_of_descriptors(&e) => {
                            if starved_since.is_none() {
                                log(format!(
                                    "offering a request's slot: {e}; clients wait until the daemon has room"
                                ));
                                starved_since = Some(Instant::now());
                            }
                            // Said again at each failure: a wake may have said there was room.
                            self.starving.store(true, Ordering::SeqCst);
                            break;
                        }
                        Err(e) => {
                            let since = failing.map_or(tried, |(since, _)| since);
                            if failing.is_none() {
                                log(format!("offering a request's slot: {e}; trying again"));
                            }
                            failing = Some((since, again_after(since)));
                            break;
                        }
                    }
                }
            }
            // A slot taken whose request has not come: a sweep is due SWEEP after it was first
            // seen so; with none, nothing is.
            let taken = [false, true].into_iter().any(|streams| {
                let mine = offered.values().filter(|o| o.streams == streams).count();
                untaken(&requests, &offered, streams) < mine
            });
            taken_since = if taken {
                taken_since.or_else(|| Some(Instant::now()))
            } else {
                None
            };
            let due = taken_since.and_then(|since| since.checked_add(SWEEP));
            // Starved, it waits to hear of room; failing otherwise, for its next try.
            let wait = due
                .into_iter()
                .chain(failing.map(|(_, again)| again))
                .min()
                .map(|at| at.saturating_duration_since(Instant::now()));
            let heard = match wait {
                Some(wait) => super::timed(Some(wait), || heard.recv_timeout(wait)),
                None => heard.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
            };
            match heard {
                Ok(Event::Asked(n) | Event::Ended(n)) => {
                    offered.remove(&n);
                    requests.remove(n);
                }
                Ok(Event::Look) => look = true,
                Ok(Event::Room) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Ok(Event::Close) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if due.is_some_and(|due| Instant::now() >= due) {
                sweep(&requests, &mut offered);
                // What it marked, it removes at the next, a sweep later.
                taken_since = None;
            }
        }
        // Closed: what is offered goes, its names and the lists with `requests`, so that a
        // client waiting for a slot hears there is no daemon, and asks the next. A slot a
        // client took meanwhile is still served: it hears RESTART (`serve_slot`).
        let_go(&requests, &mut offered);
        lock(&self.slot_events).take();
    }

    /// Makes a slot, with `streams` or without, offers it, and serves it on a thread of its
    /// own.
    fn offer_slot<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        requests: &mut Requests,
        tell: &mpsc::Sender<Event>,
        streams: bool,
    ) -> io::Result<(u64, Offered)> {
        let slot = requests.make(streams)?;
        let n = slot.number();
        let stopper = slot.stopper();
        let stopper = match stopper {
            Ok(stopper) => stopper,
            Err(e) => {
                requests.remove(n);
                return Err(e);
            }
        };
        let tell = tell.clone();
        // Among the daemon's live slots from here: its serving's end, or a thread never
        // started, lets go of it.
        let living = Living::new(self, n);
        let served = std::thread::Builder::new()
            .name("slot".into())
            .spawn_scoped(threads, move || self.serve_slot(threads, slot, &tell, &living));
        if let Err(e) = served {
            requests.remove(n);
            return Err(e);
        }
        if let Err(e) = requests.offer(n, streams) {
            requests.remove(n);
            let _ = stopper.stop();
            return Err(e);
        }
        Ok((
            n,
            Offered {
                streams,
                stopper,
                taken: false,
            },
        ))
    }

    /// Serves slot `slot`'s client: waits for it to begin its request, from which it counts
    /// among the clients in hand (past the most, it waits its turn there, as a socket's
    /// client waits in the listener's backlog); then takes the rest of its request, and
    /// bridges its session to a connection the daemon takes as a client's. `living` is the
    /// slot among the daemon's live ones.
    fn serve_slot<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        slot: Slot,
        tell: &mpsc::Sender<Event>,
        living: &Living<'s, D>,
    ) {
        let n = slot.number();
        let opened = match slot.wait() {
            Ok(Waited::Opened(opened)) => opened,
            Ok(Waited::Stopped | Waited::Gone) => {
                let _ = tell.send(Event::Ended(n));
                return;
            }
            Err(e) => {
                log(format!("request slot {n}: {e}"));
                let _ = tell.send(Event::Ended(n));
                return;
            }
        };
        living.opened();
        let _ = tell.send(Event::Asked(n));
        // The rest of its request, waited for while its client goes on with it: none of it
        // past the slowest rest seen whole that waited on its client, its client's end alone
        // before any (`Seen`); and no longer once the daemon ends its clients. One there as
        // it was read waited on no client, and teaches nothing.
        let stall = lock(&self.seen).request_stall();
        let request = |opened: slots::Opened| {
            let began = Instant::now();
            let requested = opened.request_within(stall, Some(&self.requests_end));
            if let Ok(Requested::Asked(Asked { waited: true, .. })) = &requested {
                lock(&self.seen).whole(began.elapsed());
            }
            requested
        };
        // A daemon that has closed takes no more: its client asks the next.
        let Some(counted) = self.in_hand() else {
            if let Ok(
                Requested::Asked(Asked { session, .. })
                | Requested::Ended(session)
                | Requested::Late(session),
            ) = request(opened)
            {
                let _ = session.send(kind::RESTART, &[]);
            }
            return;
        };
        match request(opened) {
            Ok(Requested::Asked(Asked {
                session,
                fds,
                request,
                ..
            })) => {
                // A stop's session the daemon holds until its exit itself, which is how its
                // client learns of it: it holds no quiet.
                if request.kind == kind::STOP {
                    living.forget();
                }
                if let Err(e) = self.bridge(threads, session, &request, &fds, counted, living) {
                    log(format!("request slot {n}: {e}"));
                }
            }
            // Ended before its request was whole: it is gone, and its count with it.
            Ok(Requested::Gone) => {}
            // Cut: told to ask again, its request begun nowhere (`Seen`). One still writing
            // finds its `say` gone (BrokenPipe), and asks again.
            Ok(Requested::Late(session)) => {
                log(format!(
                    "request slot {n}: nothing past its request's first byte in {:?}; its client asks again",
                    stall.unwrap_or_default()
                ));
                let _ = session.send(kind::RESTART, &[]);
            }
            // Its daemon ending its clients, it asks the next, as one whose request no
            // thread had read (`end_clients`).
            Ok(Requested::Ended(session)) => {
                let _ = session.send(kind::RESTART, &[]);
            }
            Err(e) => log(format!("request slot {n}: {e}")),
        }
    }

    /// Bridges `session` to a connection the daemon takes as a client's: `request` sent on
    /// it first, the descriptors it stands for (`named`, of [`slots::fds`]) the session's
    /// streams; then the client's messages on to the daemon and the daemon's back, until
    /// either side ends, each written to the client timed by `living`.
    fn bridge<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        session: Session,
        request: &Frame,
        named: &[u8],
        counted: InHand<'s, D>,
        living: &Living<'s, D>,
    ) -> io::Result<()> {
        // What its serving takes besides what the slot holds, taken before it is handed on.
        // Where no descriptor is left for them, its client is told to ask again (RESTART),
        // on the slot's own FIFO, which holds its descriptors from its offer: a client taken
        // is always answered, as a socket's client the daemon could not serve is refused
        // with its spare (`refuse_api`). It asks again through the next slot, offered as
        // there is room (`make_slots`).
        let taken = UnixStream::pair().and_then(|(ours, theirs)| Ok((ours, theirs, crate::os::null()?)));
        let (ours, theirs, null) = match taken {
            Ok(taken) => taken,
            Err(e) if crate::os::out_of_descriptors(&e) => {
                log(format!("a request's slot: {e}; its client asks again"));
                drop(counted);
                return session.send(kind::RESTART, &[]);
            }
            Err(e) => return Err(e),
        };
        // The daemon's handling reads the request as it is written, so it starts first: a
        // request past the pair's buffer (a run's labels: 208 KiB on Linux, net.core's
        // wmem_default, 8 KiB on macOS) would otherwise wait for ever on a reader yet to
        // come. Ended before it is sent, the request is none, and its count goes with it.
        counted.hand(threads, theirs);
        // Where the daemon ended the connection before the request could go (its clients
        // ended as it steps aside, `end_clients`), what it said on it still reaches the
        // client: its RESTART, so that the client asks the next daemon rather than take
        // the end for its command's.
        let sent = {
            let stream = |f: u8| -> BorrowedFd<'_> {
                let given = match f {
                    slots::fds::IN => session.input.as_ref(),
                    slots::fds::OUT => session.output.as_ref(),
                    slots::fds::ERR => session.errors.as_ref(),
                    _ => None,
                };
                given.map_or_else(|| null.as_fd(), AsFd::as_fd)
            };
            let fds: Vec<BorrowedFd<'_>> = named.iter().map(|&f| stream(f)).collect();
            shards_ipc::send(&ours, request.kind, &request.payload, &fds)
        };
        drop(null);
        self.pass_on(sent, request.kind, &ours, session, living)
    }

    /// Passes a slot's session on through `ours`, the connection the daemon took as the
    /// client's, once its request (of kind `asked`) was `sent` there or could not be: the
    /// client's messages on to the daemon, and the daemon's back, until either side ends.
    pub(super) fn pass_on(
        &self,
        sent: io::Result<()>,
        asked: u8,
        ours: &UnixStream,
        session: Session,
        living: &Living<'_, D>,
    ) -> io::Result<()> {
        let Session {
            say,
            answer,
            input,
            output,
            errors,
            ..
        } = session;
        // The streams' ends here, held until the daemon has read the request they came
        // with, which its first message says: a descriptor in flight is the sender's to
        // keep until then (M24).
        let mut streams = Some((input, output, errors));
        let mut relayed = 0usize;
        if sent.is_ok() {
            living.passed();
        }
        // On Windows `say` and `answer` are one pipe instance, whose client hears its end
        // only once each handle of it here has closed: what the client says stops being
        // read once all the daemon will say has been passed on, so that `say` closes with
        // `answer`. A run whose VM was killed left its client waiting for ever before.
        #[cfg(windows)]
        let said_all = shards_ipc::Event::new()?;
        std::thread::scope(|inner| {
            let say = &say;
            #[cfg(windows)]
            let said_all = &said_all;
            // The client's messages, on to the daemon; its end, the connection's. None
            // where the request never went: the connection may have ended there, and ended
            // here both ways before its last messages are read, XNU would drop them
            // (soshutdown's sorflush), so it ends only its writing, which the daemon's
            // handling reads as a request that never came, and waits for no longer.
            if sent.is_err() {
                let _ = ours.shutdown(std::net::Shutdown::Write);
            }
            if sent.is_ok() {
                let onward =
                    std::thread::Builder::new()
                        .name("slot-say".into())
                        .spawn_scoped(inner, move || {
                            loop {
                                #[cfg(unix)]
                                let said = slots::read_frame(say);
                                #[cfg(windows)]
                                let said = slots::read_frame_until(say, said_all);
                                let Ok(Some(f)) = said else {
                                    break;
                                };
                                if shards_ipc::send(ours, f.kind, &f.payload, &[]).is_err() {
                                    break;
                                }
                            }
                            shards_ipc::end(ours);
                        });
                if onward.is_err() {
                    shards_ipc::end(ours);
                }
            }
            // The daemon's messages, back to the client, until the daemon's side ends (true)
            // or the client takes no more (false).
            let mut answered = false;
            let ended_here = loop {
                let Ok(Some(m)) = shards_ipc::recv(ours) else {
                    break true;
                };
                relayed += 1;
                streams = None;
                // A microVM given a host's devices sends its process's pidfd with EXIT
                // (D154): its run is over once that process has ended.
                if m.kind == kind::EXIT
                    && let Some(pidfd) = m.fds.first()
                {
                    ended(pidfd);
                }
                if living.answer(&answer, m.kind, &m.payload).is_err() {
                    break false;
                }
                // Its answer's last word passed on: what the daemon's side holds after it,
                // a VM ending, holds a closed daemon no longer than a request's time.
                if matches!(m.kind, kind::EXIT | kind::END | kind::RESTART) {
                    answered = true;
                    living.answered();
                }
            };
            // A run's, an exec's or an attach's side that ended with no last word is said to
            // have (UNANSWERED): its client tells that end, its microVM's, from the end of a
            // slot whose daemon itself ended, which says nothing (PM M530).
            if ended_here
                && !answered
                && sent.is_ok()
                && matches!(asked, kind::START | kind::EXEC | kind::ATTACH)
            {
                let _ = living.answer(&answer, kind::UNANSWERED, &[]);
            }
            drop(streams.take());
            // The client's end of the session: its reads end.
            drop(answer);
            #[cfg(windows)]
            said_all.set();
            // All the daemon will say is the client's to read, there whether or not the
            // daemon is (a FIFO keeps what was written as its writer goes, pipe(7)): the
            // slot holds the daemon no longer, though the client's messages may still come
            // until it ends its own.
            living.settled();
        });
        // A request that never went is the log's only where nothing came back for its
        // client either.
        match sent {
            Err(e) if relayed == 0 => Err(e),
            _ => Ok(()),
        }
    }
}

impl<D: Disk> Daemon<D> {
    /// Counts a slot's client among those in hand once the daemon has room for it, its
    /// fewer than its most, waiting until then; `None` where the daemon has closed.
    fn in_hand(&self) -> Option<InHand<'_, D>> {
        let (held, room) = &self.room;
        let mut held = lock(held);
        while self.busy.load(Ordering::SeqCst) >= self.max_clients && !self.closed.load(Ordering::SeqCst) {
            held = room.wait(held).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if self.closed.load(Ordering::SeqCst) {
            return None;
        }
        let now = self.busy.fetch_add(1, Ordering::SeqCst).saturating_add(1);
        if now == self.max_clients {
            self.said_at_most();
        }
        Some(InHand {
            daemon: self,
            handed: false,
        })
    }
}

impl<D: Disk> Daemon<D> {
    /// Says that its clients in hand have reached its most, so that the next waits its
    /// turn: once a minute at most, however often it reaches it.
    fn said_at_most(&self) {
        let mut last = lock(&self.at_most_said);
        if last.is_none_or(|at| at.elapsed() >= Duration::from_secs(60)) {
            *last = Some(Instant::now());
            log(format!(
                "{} clients in hand, the most it takes at once: the next waits its turn",
                self.max_clients
            ));
        }
    }
}

/// A slot among the daemon's live ones ([`Live`]), from its offer: let go of as its serving
/// ends, the listener woken where that may leave the daemon quiet.
pub(super) struct Living<'a, D: Disk> {
    daemon: &'a Daemon<D>,
    n: u64,
}

impl<'a, D: Disk> Living<'a, D> {
    pub(super) fn new(daemon: &'a Daemon<D>, n: u64) -> Self {
        let live = Live {
            session: false,
            passed: false,
            waiting: None,
            answered: false,
        };
        lock(&daemon.live).insert(n, live);
        Living { daemon, n }
    }

    /// Its client opened it.
    pub(super) fn opened(&self) {
        self.step(|live| live.session = true);
    }

    /// Its request went on to the daemon's handling: it holds a closed daemon while that
    /// has yet to answer ([`Daemon::slots_hold`]).
    pub(super) fn passed(&self) {
        self.step(|live| live.passed = true);
    }

    /// Its answer's last word is its client's: from here it holds a closed daemon no more
    /// ([`Daemon::slots_hold`]).
    pub(super) fn answered(&self) {
        self.step(|live| {
            live.passed = false;
            live.answered = true;
        });
    }

    /// Its client asked for the daemon's stop, whose session the daemon holds until it
    /// exits: no longer among the live slots, which would hold it.
    fn forget(&self) {
        lock(&self.daemon.live).remove(&self.n);
    }

    /// Writes `kind` and `payload` to its client as its FIFO takes them, waiting on that
    /// client only where the FIFO takes none of what is left: until it makes room, a take
    /// (`Seen`), each wait's start and end a step on.
    pub(super) fn answer(&self, to: &slots::Line, kind: u8, payload: &[u8]) -> io::Result<()> {
        let message = slots::frame(kind, payload)?;
        let written = slots::write_all_waiting(to, &message, |wait| match wait {
            slots::Wait::Began(began) => self.step(|live| live.waiting = Some(began)),
            slots::Wait::Ended(took) => {
                lock(&self.daemon.seen).taken(took);
                self.step(|live| live.waiting = None);
            }
        });
        self.step(|live| live.waiting = None);
        written
    }

    /// It goes on: `step` to its record.
    fn step(&self, step: impl FnOnce(&mut Live)) {
        if let Some(live) = lock(&self.daemon.live).get_mut(&self.n) {
            step(live);
        }
    }

    /// Its client has all the daemon will say to it: no longer among the live slots.
    fn settled(&self) {
        let was = lock(&self.daemon.live).remove(&self.n);
        // Its client's, or a closed daemon's: the listener looks whether it is quiet now.
        if was.is_some_and(|live| live.session) || self.daemon.closed.load(Ordering::SeqCst) {
            self.daemon.wake_listener();
        }
    }
}

impl<D: Disk> Drop for Living<'_, D> {
    fn drop(&mut self) {
        self.settled();
    }
}

/// A slot's client counted among those in hand: handed to the daemon's handling with its
/// count ([`InHand::hand`]), or, dropped first, its count let go of.
struct InHand<'a, D: Disk> {
    daemon: &'a Daemon<D>,
    handed: bool,
}

impl<'a, D: Disk> InHand<'a, D> {
    /// Hands `conn` to the daemon's handling of a client, counted already.
    fn hand<'s, 'e>(mut self, threads: &'s Threads<'s, 'e>, conn: UnixStream)
    where
        'a: 's,
    {
        self.handed = true;
        self.daemon.take_counted(threads, conn);
    }
}

impl<D: Disk> Drop for InHand<'_, D> {
    fn drop(&mut self) {
        if !self.handed {
            self.daemon.busy.fetch_sub(1, Ordering::SeqCst);
            self.daemon.wake_listener();
        }
    }
}

/// When a slot that could not be offered since `since` is tried again: after a random share
/// of how long that trouble has lasted, from its first try's start (D222's `Trouble`, full
/// jitter: the time lost past the trouble's end at most what it had lasted). The tries come
/// as soon as the trouble is young and further apart as it lasts, a share of its length.
fn again_after(since: Instant) -> Instant {
    let lasted = since.elapsed();
    let now = Instant::now();
    now.checked_add(lasted.mul_f64(super::uniform())).unwrap_or(now)
}

/// How many slots on one list no client has taken: the records its FIFO still holds
/// ([`Requests::waiting`]), or every slot of that kind offered, where those cannot be read.
fn untaken(requests: &Requests, offered: &HashMap<u64, Offered>, streams: bool) -> usize {
    requests
        .waiting(streams)
        .unwrap_or_else(|_| offered.values().filter(|o| o.streams == streams).count())
}

/// Lets go of the slots `offered`: their names go, and the waits of those no client has
/// taken, read back from their lists, end through their own pipes. One a client took is
/// still served as its client writes, its record no longer on its list, so no stop races
/// a request.
fn let_go(requests: &Requests, offered: &mut HashMap<u64, Offered>) {
    for streams in [false, true] {
        match requests.untaken(streams) {
            Ok(untaken) => {
                for n in untaken {
                    if let Some(o) = offered.get(&n) {
                        let _ = o.stopper.stop();
                    }
                }
            }
            Err(e) => log(format!("reading back the slots no client has taken: {e}")),
        }
    }
    for (n, _) in offered.drain() {
        requests.remove(n);
    }
}

/// Looks the slots offered over, each list in turn: those no client has taken are offered
/// again; one a client took and has not written to since the last sweep is its dead
/// client's, removed and its wait ended through its own pipe (its maker offers another in
/// its place).
fn sweep(requests: &Requests, offered: &mut HashMap<u64, Offered>) {
    for streams in [false, true] {
        let untaken = match requests.untaken(streams) {
            Ok(untaken) => untaken,
            Err(e) => {
                log(format!("reading back the slots no client has taken: {e}"));
                continue;
            }
        };
        for n in &untaken {
            if offered.contains_key(n)
                && let Err(e) = requests.offer(*n, streams)
            {
                log(format!("offering request slot {n} again: {e}"));
            }
        }
        for (n, slot) in offered.iter_mut().filter(|(_, o)| o.streams == streams) {
            if untaken.contains(n) {
                slot.taken = false;
            } else if slot.taken {
                requests.remove(*n);
                let _ = slot.stopper.stop();
            } else {
                slot.taken = true;
            }
        }
    }
}

/// Waits for the process whose handle `process` is to end: it is signaled then. Whatever
/// stops the wait short, it is over.
#[cfg(windows)]
fn ended(process: &OwnedFd) {
    use std::os::windows::io::AsRawHandle as _;
    // SAFETY: a process handle this function is lent, waited for.
    unsafe {
        windows_sys::Win32::System::Threading::WaitForSingleObject(
            process.as_raw_handle(),
            windows_sys::Win32::System::Threading::INFINITE,
        )
    };
}

/// Waits for the process whose pidfd `pidfd` is to end: it polls readable then
/// (pidfd_open(2)). Whatever stops the wait short, it is over.
#[cfg(unix)]
fn ended(pidfd: &OwnedFd) {
    let mut poll = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: poll(2) of one pollfd this function owns, with no timeout.
        let r = unsafe { libc::poll(&raw mut poll, 1, -1) };
        if r >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return;
        }
    }
}
