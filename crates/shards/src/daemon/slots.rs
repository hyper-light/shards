//! The home's request slots (docs/design/architecture.md D185): how the daemon's own clients
//! reach it. A slot's session is bridged to the daemon's handling of a client: a connection
//! of its own, which the daemon takes as a client's, and whose other end the bridge holds.
//! The request's descriptors are the slot's streams; the client's messages go on to the
//! daemon, and the daemon's back to the client, without descriptors, each side's end the
//! other's.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use shards_ipc::kind;
use shards_ipc::slots::{self, Asked, Frame, Requested, Requests, Session, Slot, Stopper, Waited};

use super::{Daemon, RETRY, Threads, lock, log};
use crate::containers::Disk;

/// How often the slots offered are looked over for those a client took and never wrote
/// to, its process ended in between: removed, and offered anew. Only such a client's slot
/// waits so long; a live one writes as it opens its slot.
const SWEEP: Duration = Duration::from_secs(60);

/// How often the maker looks whether its lists are still there, where nothing else wakes
/// it: someone may remove them as the daemon serves, as a removal of the home does, and a
/// client that finds none starts a daemon, which waits for the lock up to `TAKEOVER` (20 s)
/// for its holder to serve. A look is a stat of each list.
pub(super) const LOOK: Duration = Duration::from_secs(1);

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

    /// The slots' maker: keeps [`slots::OFFERED`] slots of each kind offered, without streams
    /// and with: one for the next client, and one for a client that comes as the first's
    /// replacement is made (making and removing a slot of five FIFOs is 317 µs of the
    /// daemon's CPU on a loaded Mac, of two 125 µs, PM M255), so that none is made ahead
    /// that no client asked for; clients past them wait on their list, in turn, a making
    /// each. As long as the daemon has room for another client (its most in hand,
    /// `max_clients`: past that, clients wait on their list for room); and looks them over
    /// every [`SWEEP`].
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
        let mut swept = Instant::now();
        loop {
            // Its lists removed as it serves: made again, as a removed socket was bound
            // again, and what was offered on them let go of. Under `socket_at`, which
            // `close` holds as it closes and unoffers them: read apart, a close between the
            // two looks would have its lists made again, a daemon told to stop offering
            // again (as one listened again, PM M188).
            {
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
                while offered.values().filter(|o| o.streams == streams).count() < slots::OFFERED
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
                            log(format!("offering a request's slot: {e}"));
                            break;
                        }
                    }
                }
            }
            let wait = if starved_since.is_some() { RETRY } else { LOOK };
            match heard.recv_timeout(wait) {
                Ok(Event::Asked(n) | Event::Ended(n)) => {
                    offered.remove(&n);
                    requests.remove(n);
                }
                Ok(Event::Room) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Ok(Event::Close) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if swept.elapsed() >= SWEEP {
                sweep(&requests, &mut offered);
                swept = Instant::now();
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
        let served = std::thread::Builder::new()
            .name("slot".into())
            .spawn_scoped(threads, move || self.serve_slot(threads, slot, &tell));
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
    /// bridges its session to a connection the daemon takes as a client's.
    fn serve_slot<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, slot: Slot, tell: &mpsc::Sender<Event>) {
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
                if let Err(e) = self.bridge(threads, session, &request, &fds, counted) {
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
    /// either side ends.
    fn bridge<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        session: Session,
        request: &Frame,
        named: &[u8],
        counted: InHand<'s, D>,
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
                if slots::write_frame(&answer, m.kind, &m.payload).is_err() {
                    break;
                }
            }
            drop(streams.take());
            // The client's end of the session: its reads end.
            drop(answer);
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
