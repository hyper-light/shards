//! The home's request slots (docs/design/architecture.md D185): how the daemon's own clients
//! reach it. A slot's session is bridged to the daemon's handling of a client: a connection
//! of its own, which the daemon takes as a client's, and whose other end the bridge holds.
//! The request's descriptors are the slot's streams; the client's messages go on to the
//! daemon, and the daemon's back to the client, without descriptors, each side's end the
//! other's.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use shards_ipc::kind;
use shards_ipc::slots::{self, Asked, Frame, Requested, Requests, Session, Slot, Stopper, Waited};

use super::{Daemon, RETRY, Threads, lock, log};
use crate::containers::Disk;

/// How long a slot a client took may go without its request before a sweep looks it over:
/// one taken by a client whose process ended in between is removed at the second sweep
/// that finds it so, and its place offered anew. Only such a client's slot waits so long;
/// a live one writes as it opens its slot. A sweep is due only while a slot is taken and
/// its request not read: an idle daemon's maker sleeps until something happens.
const SWEEP: Duration = Duration::from_secs(60);

/// The most of an answer a slot writes to its client at once: what a Linux FIFO holds (PM
/// M261), so that each piece's write ends once its client has taken some of what it was
/// given, and a client taking a long answer slowly is seen to go on, as one that takes
/// nothing is not ([`Daemon::slots_hold`]).
const PIECE: usize = 64 << 10;

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
    /// When the piece of an answer being written to its client began ([`PIECE`]): what it
    /// waits on its client from.
    pub(super) writing: Option<Instant>,
    /// When it last went on, or was offered.
    went_on: Instant,
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
    /// client opened holds it. Once it has closed, each holds it until its serving ends, or
    /// until its client has had the time a request has (`request_timeout`) and not gone on:
    /// not taken the piece of its answer being written to it, since that piece's write
    /// began, or not gone on at all since its last step or the close, the later. A client
    /// reading its answer takes each piece as it comes, however slowly it reads; one
    /// stopped, or held by a pager that waits, takes nothing, and keeps no daemon from
    /// exiting nor the next from starting.
    pub(super) fn slots_hold(&self) -> (bool, Option<Instant>) {
        let closed_at = *lock(&self.closed_at);
        let now = Instant::now();
        let mut held = false;
        let mut lets_go: Option<Instant> = None;
        for live in lock(&self.live).values() {
            let Some(closed_at) = closed_at else {
                held |= live.session;
                continue;
            };
            let since = live.writing.unwrap_or_else(|| live.went_on.max(closed_at));
            match since.checked_add(self.request_timeout) {
                Some(at) if at <= now => {}
                at => {
                    held = true;
                    lets_go = lets_go.into_iter().chain(at).min();
                }
            }
        }
        (held, lets_go)
    }

    /// Says which slots' clients the daemon exits without: each holds it no longer
    /// ([`slots_hold`](Self::slots_hold)), and none has ended.
    pub(super) fn leave_slots(&self) {
        let closed_at = *lock(&self.closed_at);
        for (n, live) in lock(&self.live).iter() {
            let (since, what) = match live.writing {
                Some(since) => (since, "took no more of its answer"),
                None => (
                    closed_at.map_or(live.went_on, |at| live.went_on.max(at)),
                    "went no further",
                ),
            };
            log(format!(
                "request slot {n}: its client {what} in {:?}; exiting without it",
                since.elapsed()
            ));
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
        // Out of descriptors since when: said once, and looked again every RETRY, a slot
        // being FIFOs and their ends.
        let mut starved_since: Option<Instant> = None;
        // A slot it could not offer otherwise: said once, and tried again every RETRY, as
        // nothing else may wake it while every client waits on a list it left short.
        let mut failing_since: Option<Instant> = None;
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
                {
                    match self.offer_slot(threads, &mut requests, tell, streams) {
                        Ok((n, made)) => {
                            offered.insert(n, made);
                            if let Some(since) = starved_since.take() {
                                log(format!("offering slots again, after {:?}", since.elapsed()));
                                self.have_room();
                            }
                            if let Some(since) = failing_since.take() {
                                log(format!("offering slots again, after {:?}", since.elapsed()));
                            }
                        }
                        Err(e) if matches!(e.raw_os_error(), Some(libc::EMFILE | libc::ENFILE)) => {
                            if starved_since.is_none() {
                                log(format!(
                                    "offering a request's slot: {e}; clients wait until the daemon has room"
                                ));
                                starved_since = Some(Instant::now());
                                self.starving.store(true, Ordering::SeqCst);
                            }
                            break;
                        }
                        Err(e) => {
                            if failing_since.is_none() {
                                log(format!("offering a request's slot: {e}; trying again"));
                                failing_since = Some(Instant::now());
                            }
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
            let wait = match (starved_since.or(failing_since), due) {
                (Some(_), _) => Some(RETRY),
                (None, Some(due)) => Some(due.saturating_duration_since(Instant::now())),
                (None, None) => None,
            };
            let heard = match wait {
                Some(wait) => heard.recv_timeout(wait),
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
        // The rest of its request, whole within the time a socket's client had to send its
        // own, and no longer waited for once the daemon ends its clients.
        let request = |opened: slots::Opened| {
            opened.request_within(Some(self.request_timeout), Some(self.requests_end.0.as_fd()))
        };
        // A daemon that has closed takes no more: its client asks the next.
        let Some(counted) = self.in_hand() else {
            if let Ok(Requested::Asked(Asked { session, .. }) | Requested::Ended(session)) = request(opened) {
                let _ = session.send(kind::RESTART, &[]);
            }
            return;
        };
        match request(opened) {
            Ok(Requested::Asked(Asked {
                session,
                fds,
                request,
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
            Ok(Requested::Late(_)) => log(format!("a client sent no request in {:?}", self.request_timeout)),
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
        let (ours, theirs) = UnixStream::pair()?;
        // The daemon's handling reads the request as it is written, so it starts first: a
        // request past the pair's buffer (a run's labels: 208 KiB on Linux, net.core's
        // wmem_default, 8 KiB on macOS) would otherwise wait for ever on a reader yet to
        // come. Ended before it is sent, the request is none, and its count goes with it.
        counted.hand(threads, theirs);
        let null = File::open("/dev/null")?;
        {
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
            shards_ipc::send(&ours, request.kind, &request.payload, &fds)?;
        }
        drop(null);
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
        std::thread::scope(|inner| {
            let (ours, say) = (&ours, &say);
            // The client's messages, on to the daemon; its end, the connection's.
            let onward = std::thread::Builder::new()
                .name("slot-say".into())
                .spawn_scoped(inner, move || {
                    while let Ok(Some(f)) = slots::read_frame(say) {
                        if shards_ipc::send(ours, f.kind, &f.payload, &[]).is_err() {
                            break;
                        }
                    }
                    shards_ipc::end(ours);
                });
            if onward.is_err() {
                shards_ipc::end(ours);
            }
            // The daemon's messages, back to the client.
            while let Ok(Some(m)) = shards_ipc::recv(ours) {
                streams = None;
                // A microVM given a host's devices sends its process's pidfd with EXIT
                // (D154): its run is over once that process has ended.
                if m.kind == kind::EXIT
                    && let Some(pidfd) = m.fds.first()
                {
                    ended(pidfd);
                }
                if living.answer(&answer, m.kind, &m.payload).is_err() {
                    break;
                }
            }
            drop(streams.take());
            // The client's end of the session: its reads end.
            drop(answer);
            // All the daemon will say is the client's to read, there whether or not the
            // daemon is (a FIFO keeps what was written as its writer goes, pipe(7)): the
            // slot holds the daemon no longer, though the client's messages may still come
            // until it ends its own.
            living.settled();
        });
        Ok(())
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
            writing: None,
            went_on: Instant::now(),
        };
        lock(&daemon.live).insert(n, live);
        Living { daemon, n }
    }

    /// Its client opened it.
    fn opened(&self) {
        self.step(|live| live.session = true);
    }

    /// Its client asked for the daemon's stop, whose session the daemon holds until it
    /// exits: no longer among the live slots, which would hold it.
    fn forget(&self) {
        lock(&self.daemon.live).remove(&self.n);
    }

    /// Writes `kind` and `payload` to its client, waiting on that client meanwhile: a
    /// [`PIECE`] at a time, each a step on as its client takes it.
    pub(super) fn answer(&self, to: &File, kind: u8, payload: &[u8]) -> io::Result<()> {
        let message = slots::frame(kind, payload)?;
        let written = message.chunks(PIECE).try_for_each(|piece| {
            self.step(|live| live.writing = Some(Instant::now()));
            (&*to).write_all(piece)
        });
        self.step(|live| live.writing = None);
        written
    }

    /// It goes on: `step` to its record, and when.
    fn step(&self, step: impl FnOnce(&mut Live)) {
        if let Some(live) = lock(&self.daemon.live).get_mut(&self.n) {
            step(live);
            live.went_on = Instant::now();
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

/// Waits for the process whose pidfd `pidfd` is to end: it polls readable then
/// (pidfd_open(2)). Whatever stops the wait short, it is over.
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
