//! The home's request slots (docs/design/architecture.md D185), served beside the daemon's
//! socket while clients move to them. A slot's session is bridged to the daemon's handling
//! of a client: a connection of its own, which the daemon takes as it takes a socket's
//! client, and whose other end the bridge holds, so that what serves a socket's client
//! serves a slot's. The request's descriptors are the slot's streams; the client's
//! messages go on to the daemon, and the daemon's back to the client, without descriptors,
//! each side's end the other's.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use shards_ipc::kind;
use shards_ipc::slots::{self, Frame, Requests, Session, Slot, Stopper, Waited};

use super::{Daemon, Threads, lock, log};
use crate::containers::Disk;

/// How often the slots offered are looked over for those a client took and never wrote
/// to, its process ended in between: removed, and offered anew. Only such a client's slot
/// waits so long; a live one writes as it opens its slot.
const SWEEP: Duration = Duration::from_secs(60);

/// The slots kept offered: one for the next client, and one for a client that comes as the
/// first one's replacement is made, which takes 291 µs at p50 and 569 at p99 on a loaded
/// Mac (making and removing five FIFOs, 317 µs of CPU). Clients past them wait on `free`,
/// in turn, a making each; none is made ahead that no client asked for.
const SPARE: usize = 2;

/// What the slots' maker hears.
#[derive(Debug)]
pub(super) enum Event {
    /// Slot `n`'s client asked: its names go, and another is offered in its place.
    Asked(u64),
    /// Slot `n`'s wait ended without a request.
    Ended(u64),
    /// A client left: there may be room to offer what back-pressure held back.
    Room,
    /// The daemon closed: no slot is offered from here on.
    Close,
}

/// A slot offered: what ends its wait, and whether a sweep has found it taken.
struct Offered {
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

    /// The slots' maker: keeps [`SPARE`] slots offered, as long as the daemon has room for
    /// another client (its most in hand, `max_clients`: past that, clients wait on `free`
    /// for room); and looks them over every [`SWEEP`].
    fn make_slots<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        mut requests: Requests,
        tell: &mpsc::Sender<Event>,
        heard: &mpsc::Receiver<Event>,
    ) {
        let mut offered: HashMap<u64, Offered> = HashMap::new();
        loop {
            while offered.len() < SPARE
                && self.busy.load(Ordering::SeqCst) < self.max_clients
                && !self.closed.load(Ordering::SeqCst)
            {
                match self.offer_slot(threads, &mut requests, tell) {
                    Ok((n, made)) => {
                        offered.insert(n, made);
                    }
                    Err(e) => {
                        log(format!("offering a request's slot: {e}"));
                        break;
                    }
                }
            }
            match heard.recv_timeout(SWEEP) {
                Ok(Event::Asked(n) | Event::Ended(n)) => {
                    offered.remove(&n);
                    requests.remove(n);
                }
                Ok(Event::Room) => {}
                Ok(Event::Close) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => sweep(&requests, &mut offered),
            }
        }
        // Closed: what is offered goes, and `free` with `requests`, so that a client
        // waiting for a slot hears there is no daemon, and asks the next.
        for (n, slot) in &offered {
            requests.remove(*n);
            let _ = slot.stopper.stop();
        }
        lock(&self.slot_events).take();
    }

    /// Makes a slot, offers it, and serves it on a thread of its own.
    fn offer_slot<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        requests: &mut Requests,
        tell: &mpsc::Sender<Event>,
    ) -> io::Result<(u64, Offered)> {
        let slot = requests.make()?;
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
        if let Err(e) = requests.offer(n) {
            requests.remove(n);
            let _ = stopper.stop();
            return Err(e);
        }
        Ok((
            n,
            Offered {
                stopper,
                taken: false,
            },
        ))
    }

    /// Serves slot `slot`'s client: waits for its request, then bridges its session to a
    /// connection the daemon takes as a client's.
    fn serve_slot<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, slot: Slot, tell: &mpsc::Sender<Event>) {
        let n = slot.number();
        let (session, request, named) = match slot.wait() {
            Ok(Waited::Asked {
                session,
                request,
                fds,
            }) => {
                let _ = tell.send(Event::Asked(n));
                (session, request, fds)
            }
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
        // A client past the daemon's most in hand waits its turn, as a socket's waits in
        // the listener's backlog.
        self.await_room();
        // A daemon that has closed takes no more: its client asks the next.
        if self.closed.load(Ordering::SeqCst) {
            let _ = session.send(kind::RESTART, &[]);
            return;
        }
        if let Err(e) = self.bridge(threads, session, &request, &named) {
            log(format!("request slot {n}: {e}"));
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
    ) -> io::Result<()> {
        let (ours, theirs) = UnixStream::pair()?;
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
        self.take(threads, theirs);
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
    /// Waits until the daemon has fewer than its most clients in hand, or has closed.
    fn await_room(&self) {
        let (held, room) = &self.room;
        let mut held = lock(held);
        while self.busy.load(Ordering::SeqCst) >= self.max_clients && !self.closed.load(Ordering::SeqCst) {
            held = room.wait(held).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

/// Looks the slots offered over: those no client has taken are offered again; one a
/// client took and has not written to since the last sweep is its dead client's, ended
/// and removed (its maker offers another in its place).
fn sweep(requests: &Requests, offered: &mut HashMap<u64, Offered>) {
    let untaken = match requests.untaken() {
        Ok(untaken) => untaken,
        Err(e) => {
            log(format!("reading back the slots no client has taken: {e}"));
            return;
        }
    };
    for n in &untaken {
        if offered.contains_key(n)
            && let Err(e) = requests.offer(*n)
        {
            log(format!("offering request slot {n} again: {e}"));
        }
    }
    for (n, slot) in offered.iter_mut() {
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
