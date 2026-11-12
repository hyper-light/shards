//! The exec port ([`run::EXEC_PORT`]): the guest dials it for each exec, each restored
//! run's connections (D176) and its capture, and each connection's HELLO names, by its
//! token, what waits for it. What waits ([`register`]) is matched by token, of whatever
//! kind; a connection is never refused because another thread holds a lock.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use shards_abi::run::{self, kind};
use shards_vmm::{
    platform::UnixStream,
    vm::{Admission, ServedPort},
};

use super::Target;

enum Listener {
    Dormant,
    Live,
    Closed,
}

/// What waits for a connection: the token its HELLO will say, and what it is for.
struct Waiting {
    token: [u8; run::TOKEN],
    target: Target,
}

/// What waits, and whether the port listens: the exec threads', the acceptor's and the
/// workload's lock, which the vsock device thread never takes.
struct Waiters {
    listener: Listener,
    waiting: Vec<Waiting>,
}

static WAITERS: Mutex<Waiters> = Mutex::new(Waiters {
    listener: Listener::Dormant,
    waiting: Vec::new(),
});

/// How many wait, as [`WAITERS`] last said (stored under its lock): what admission reads in
/// its place. A registration precedes the frame that asks the guest for its connection,
/// and the device thread carries that frame to the guest before the guest can dial, so it
/// reads the registration's count before the connection comes.
static WAITING: AtomicUsize = AtomicUsize::new(0);

/// Where admitted connections wait for the acceptor, and how it is woken: the vsock device
/// thread's hand-off (D8). Its lock is held only to push, take or close, never across a
/// wait, and no other lock is taken under it, so the device thread waits on it no longer
/// than that, and on nothing that waits for the device thread.
struct Door {
    /// Its write end; none before the port listens, or once it has closed.
    wake: Option<UnixStream>,
    /// None of them any one waiter's until its HELLO says whose.
    arrived: Vec<UnixStream>,
}

static DOOR: Mutex<Door> = Mutex::new(Door {
    wake: None,
    arrived: Vec::new(),
});

fn waiters() -> MutexGuard<'static, Waiters> {
    WAITERS.lock().unwrap_or_else(PoisonError::into_inner)
}

fn door() -> MutexGuard<'static, Door> {
    DOOR.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Waiters {
    /// The first waiter `matches` finds, no longer waiting.
    fn take(&mut self, matches: impl Fn(&Waiting) -> bool) -> Option<Target> {
        let i = self.waiting.iter().position(matches)?;
        let target = self.waiting.swap_remove(i).target;
        WAITING.store(self.waiting.len(), Ordering::Release);
        Some(target)
    }
}

/// `target` waits for the connection whose HELLO will say `token`, registered before the
/// guest is asked for it; refused once the port has closed, as nothing waiting would hear.
pub(super) fn register(token: [u8; run::TOKEN], target: Target) -> Result<(), String> {
    let mut w = waiters();
    if !matches!(w.listener, Listener::Live) {
        return Err("owned HELLO listener is closed".into());
    }
    if w.waiting.iter().any(|x| super::same_token(&x.token, &token)) {
        return Err("a duplicate owned HELLO token".into());
    }
    w.waiting
        .try_reserve(1)
        .map_err(|e| format!("reserving an owned HELLO slot: {e}"))?;
    w.waiting.push(Waiting { token, target });
    WAITING.store(w.waiting.len(), Ordering::Release);
    Ok(())
}

/// What waited for the connection whose HELLO says `token`, no longer waiting.
fn claim(token: &[u8; run::TOKEN]) -> Option<Target> {
    waiters().take(|w| super::same_token(&w.token, token))
}

/// What exec `id` waited with, no longer waiting: its guest has said it has no connection.
pub(super) fn fail(id: u32) -> Option<Target> {
    waiters().take(|w| w.target.id() == Some(id))
}

/// `token`'s wait, forgotten: the guest was never asked for its connection.
pub(super) fn forget(token: &[u8; run::TOKEN]) {
    drop(claim(token));
}

/// The port closes as its workload ends: what waits is let go of, which tells each that its
/// connection will not come, nothing more is admitted, and the acceptor ends.
pub(super) fn close() {
    let waiting = {
        let mut w = waiters();
        w.listener = Listener::Closed;
        WAITING.store(0, Ordering::Release);
        std::mem::take(&mut w.waiting)
    };
    let (wake, arrived) = {
        let mut d = door();
        (d.wake.take(), std::mem::take(&mut d.arrived))
    };
    drop((waiting, wake, arrived));
}

pub struct Acceptor {
    woken: UnixStream,
}

pub fn exec_port() -> Result<(ServedPort, Acceptor), String> {
    let (wake, woken) = UnixStream::pair().map_err(|e| e.to_string())?;
    wake.set_nonblocking(true).map_err(|e| e.to_string())?;
    woken.set_nonblocking(true).map_err(|e| e.to_string())?;
    let mut w = waiters();
    if !matches!(w.listener, Listener::Dormant) {
        return Err("owned HELLO listener already initialized or closed".into());
    }
    // The door opens before anything may wait. WAITERS, then DOOR: nothing takes WAITERS
    // under DOOR.
    door().wake = Some(wake);
    w.listener = Listener::Live;
    Ok((ServedPort::Admitted(admit), Acceptor { woken }))
}

/// Why the port refused a connection.
#[derive(Clone, Copy)]
enum Refusal {
    /// Its workload, or its acceptor, has ended.
    Closed,
    /// Nothing waits for one: a process of the microVM's dialing the port itself.
    Unwaited,
    /// No memory to hold it.
    Unheld,
}

impl Refusal {
    /// Said in the VM process's log the first time for each reason, where a refused exec's
    /// cause is looked for; every one is the muxer's to say at debug, as a workload may dial
    /// the port as often as it likes.
    fn say(self) {
        static CLOSED: AtomicBool = AtomicBool::new(false);
        static UNWAITED: AtomicBool = AtomicBool::new(false);
        static UNHELD: AtomicBool = AtomicBool::new(false);
        let (said, why) = match self {
            Refusal::Closed => (&CLOSED, "the port has closed, its workload ended"),
            Refusal::Unwaited => (&UNWAITED, "no exec waits for a connection"),
            Refusal::Unheld => (&UNHELD, "no memory to hold it"),
        };
        if !said.swap(true, Ordering::Relaxed) {
            shards_vmm::warn!("the exec port refused a guest's connection: {why} (later ones at debug)");
        }
    }
}

impl Door {
    fn admit(&mut self, conn: UnixStream) -> Result<(), (Refusal, UnixStream)> {
        let Some(wake) = &self.wake else {
            return Err((Refusal::Closed, conn));
        };
        if WAITING.load(Ordering::Acquire) == 0 {
            return Err((Refusal::Unwaited, conn));
        }
        if self.arrived.try_reserve(1).is_err() {
            return Err((Refusal::Unheld, conn));
        }
        loop {
            match (&*wake).write(&[1]) {
                Ok(_) => break,
                // A wake the acceptor has yet to read wakes it for this one too.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // Its read end gone: the acceptor has ended.
                Err(_) => return Err((Refusal::Closed, conn)),
            }
        }
        self.arrived.push(conn);
        Ok(())
    }
}

/// Admits a guest's connection, on the vsock device thread: handed to the acceptor while
/// anything waits for one.
fn admit(conn: UnixStream) -> Admission {
    let admitted = door().admit(conn);
    match admitted {
        Ok(()) => Admission::Accepted,
        Err((refusal, conn)) => {
            drop(conn);
            refusal.say();
            Admission::Closed
        }
    }
}

struct Candidate {
    conn: UnixStream,
    hello: [u8; run::HEADER + run::TOKEN],
    got: usize,
    /// When it last said anything, or arrived.
    heard: Instant,
}

impl Candidate {
    fn new(conn: UnixStream, heard: Instant) -> Candidate {
        Candidate {
            conn,
            hello: [0; run::HEADER + run::TOKEN],
            got: 0,
            heard,
        }
    }

    fn read(&mut self) -> io::Result<bool> {
        let room = self
            .hello
            .get_mut(self.got..)
            .ok_or_else(|| io::Error::other("owned HELLO cursor"))?;
        let n = match (&self.conn).read(room) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => n,
            Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {
                return Ok(false);
            }
            Err(e) => return Err(e),
        };
        self.got = self
            .got
            .checked_add(n)
            .ok_or_else(|| io::Error::other("owned HELLO cursor overflow"))?;
        self.heard = Instant::now();
        if self.got >= run::HEADER
            && self
                .hello
                .first_chunk::<{ run::HEADER }>()
                .copied()
                .and_then(run::parse_header)
                != Some((kind::HELLO, run::TOKEN as u32))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid owned HELLO header",
            ));
        }
        Ok(self.got == self.hello.len())
    }
}

/// Reads what each candidate has said, never past its HELLO: one whose HELLO is whole goes
/// to what its token names, and one that ended, failed or said anything else is closed.
/// How many went to what waited for them.
fn read_candidates(active: &mut Vec<Candidate>) -> usize {
    let mut claimed = 0;
    for i in (0..active.len()).rev() {
        let Some(candidate) = active.get_mut(i) else {
            continue;
        };
        match candidate.read() {
            Ok(false) => {}
            Ok(true) => {
                let candidate = active.swap_remove(i);
                if let Some(token) = candidate.hello.last_chunk::<{ run::TOKEN }>()
                    && heard(candidate.conn, token)
                {
                    claimed += 1;
                }
            }
            Err(_) => {
                active.swap_remove(i);
            }
        }
    }
    claimed
}

/// A connection whose HELLO has come whole, saying `token`: to what waits for it, or
/// closed; whether anything did.
fn heard(conn: UnixStream, token: &[u8; run::TOKEN]) -> bool {
    let Some(target) = claim(token) else {
        return false;
    };
    target.deliver(match conn.set_nonblocking(false) {
        Ok(()) => Ok(conn),
        Err(e) => Err(format!("its connection: {e}")),
    });
    true
}

/// Lets go of the candidates silent longest while more are held than `waiting`, each read of
/// all it has said: every waiter's connection has its waiter, so past their number at least
/// one candidate is no waiter's (a process of the microVM's dialing the port), and a
/// waiter's guest, whose HELLO follows its connect, is heard from first. Whether any went.
fn bound(active: &mut Vec<Candidate>, waiting: usize) -> bool {
    let mut evicted = false;
    while active.len() > waiting {
        let Some(oldest) = active
            .iter()
            .enumerate()
            .min_by_key(|(_, c)| c.heard)
            .map(|(i, _)| i)
        else {
            break;
        };
        active.swap_remove(oldest);
        evicted = true;
    }
    evicted
}

/// Reads every wake written: false once the port has closed, its write end gone.
fn drain(woken: &UnixStream) -> io::Result<bool> {
    let mut byte = [0];
    loop {
        match (&*woken).read(&mut byte) {
            Ok(0) => return Ok(false),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

impl Acceptor {
    pub fn run(self) {
        if let Err(e) = self.drive() {
            shards_vmm::warn!("the exec port's acceptor ended, and what waits on it fails: {e}");
        }
    }

    /// Each round: the connections admitted since the last, every candidate read, and no
    /// more held than wait. The waiters are counted before the reads, less those the reads
    /// hand over: a waiter gone meanwhile, whose candidate the next round reads to its end,
    /// costs no other its own. Wakes are read before the next round takes what came.
    fn drive(&self) -> io::Result<()> {
        static EVICTED: AtomicBool = AtomicBool::new(false);
        let mut active: Vec<Candidate> = Vec::new();
        loop {
            let arrived = std::mem::take(&mut door().arrived);
            active.try_reserve(arrived.len()).map_err(io::Error::other)?;
            let now = Instant::now();
            active.extend(arrived.into_iter().map(|conn| Candidate::new(conn, now)));
            let waiting = waiters().waiting.len();
            let claimed = read_candidates(&mut active);
            if bound(&mut active, waiting.saturating_sub(claimed)) && !EVICTED.swap(true, Ordering::Relaxed) {
                shards_vmm::warn!(
                    "the exec port let go of the guest's connection silent longest, more held than wait (later ones unsaid)"
                );
            }
            super::await_hellos(&self.woken, active.iter().map(|c| &c.conn))?;
            if !drain(&self.woken)? {
                return Ok(());
            }
        }
    }
}

impl Drop for Acceptor {
    fn drop(&mut self) {
        close();
    }
}

/// A fresh port for the next test: each test's ends with its own.
#[cfg(test)]
pub(super) fn reopen() {
    close();
    waiters().listener = Listener::Dormant;
}

/// One test of the exec port's at a time: they share its state, as a VM process's execs do.
#[cfg(test)]
pub(super) static TESTS: Mutex<()> = Mutex::new(());

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A guest's exec connection is admitted while an exec thread holds the waiters' lock,
    /// as each does while it registers (the listener's state is under it too): refused as
    /// full, the muxer reset it, and its exec waited for ever (ROOT's 64 execs at once,
    /// 2026-11-12). Admission is the device thread's alone, under a lock of its own.
    #[test]
    fn admission_takes_a_connection_while_the_waiters_lock_is_held() {
        let _serial = TESTS.lock().unwrap_or_else(PoisonError::into_inner);
        reopen();
        let (port, _acceptor) = exec_port().unwrap();
        let (to, _gets) = std::sync::mpsc::sync_channel(1);
        register([31; run::TOKEN], Target::Exec { id: 31, to }).unwrap();
        let busy = waiters();
        let (_guest, host) = UnixStream::pair().unwrap();
        let admitted = port.accept(host);
        drop(busy);
        assert_eq!(admitted, Admission::Accepted);
        close();
    }

    /// Past the waiters' number, the candidate silent longest goes, each read first: one
    /// that has said part of its HELLO since stays.
    #[test]
    fn past_the_waiters_the_one_silent_longest_goes() {
        let pair = || {
            let (peer, conn) = UnixStream::pair().unwrap();
            conn.set_nonblocking(true).unwrap();
            (peer, conn)
        };
        let start = Instant::now();
        let (_silent_peer, silent) = pair();
        let (talking_peer, talking) = pair();
        let mut active = vec![Candidate::new(silent, start), Candidate::new(talking, start)];
        (&talking_peer)
            .write_all(&run::header(kind::HELLO, run::TOKEN as u32))
            .unwrap();
        assert_eq!(read_candidates(&mut active), 0);
        assert!(bound(&mut active, 1));
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].got, run::HEADER, "the one that spoke stays");
        assert!(!bound(&mut active, 1));
        assert!(bound(&mut active, 0));
        assert!(active.is_empty());
    }
}
