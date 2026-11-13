//! A run's members' MCP connections (D254), each paired with its controller's ask: held as its
//! VM hands it over (`LANE_ACCEPTED`) until an `Mcp` ask for its member takes it, its harness's
//! side ends, or its run ends; then carried as that ask's two streams, step 5's, the harness's
//! bytes its output and the controller's its input, as an exec's stdio is carried (execs.rs).
//! The daemon's runs and an orchestrator's run host (api_run.rs) hold theirs alike.
//!
//! - **Held, in the order they came.** Each connection's first bytes are read ahead on a thread
//!   of its own ([`Bridges::watch`]), at most a record of the lane that takes it, so that one
//!   whose harness's side ends before any ask takes it is let go, never paired.
//! - **Its bound.** Each is charged to its run's budget for what it holds: its connection's
//!   kernel queues, as an exec's connection is (`ipc_buffers`), the record its first bytes are
//!   read into, and its entry. One the budget has no room for is let go, which ends its
//!   member's bridge.
//! - **Its ask** ([`carry`]). Paired with its member's oldest connection held, waited for until
//!   one comes, its lane is let go, or its run ends; refused `Refused` at once for a member its
//!   run gave no bridge. Told `Started` once its connection's first read is done.
//! - **Its end.** Told once the harness's side has ended and every byte of it is taken
//!   (`Exited(0)`); a read that fails (its VM gone) aborts its output, as an exec's lost end
//!   does. The controller's end of what it sends half-closes the connection, and an abort ends
//!   it both ways. Its lane let go hangs it up: the connection ended both ways, which ends the
//!   member's bridge.

use std::io::{self, Read as _, Write as _};
use std::mem::size_of;
use std::net::Shutdown;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use shards_abi::run::managed::InputEnd;
use shards_ipc::Stream;
use shards_remote::control::exec::{self, Reply, Start, Unstarted};
use shards_remote::managed::input;

use super::execs::{self, Held, Pumped};
use super::{Charge, ipc_buffers, lock};

/// A run's members' MCP connections: the members its run gave a bridge, and each connection
/// held, charged to its run's budget.
pub(crate) struct Bridges {
    /// The members the run gave a bridge (`member-mcp=`).
    members: Vec<u32>,
    /// The most a connection's first read takes: a record of the lane that takes it, the
    /// output record its run admits for an exec's streams.
    record: usize,
    charge: Arc<Charge>,
    held: Mutex<Waiting>,
    changed: Condvar,
}

/// What a run holds (R04): each connection come and not yet carried, in the order it came;
/// and whether the run is over, after which none is held and every wait ends.
struct Waiting {
    connections: Vec<Connection>,
    over: bool,
}

/// A connection held: its VM's number for it, its member, the connection, its first read,
/// whether an ask has taken it, and what it costs its run.
struct Connection {
    number: u64,
    member: u32,
    conn: Stream,
    first: First,
    taken: bool,
    charged: u64,
}

/// A held connection's first read (R04).
enum First {
    /// Its watcher reads: nothing has come.
    Awaited,
    /// Its harness's first bytes, which go first to whoever takes it.
    Came(Vec<u8>),
    /// Its harness's side ended, or its read failed, before a byte came.
    Ended,
}

/// Why a connection was not held: let go as it came, which ends its member's bridge.
#[derive(Debug)]
pub(crate) enum Unheld {
    /// Its member's run gave it no bridge.
    NoBridge,
    /// The run is over.
    Over,
    /// The run's budget has no room for what it holds.
    Capacity,
    /// What it holds could not be measured.
    Unmeasured(io::Error),
    /// Its watcher's thread could not be started.
    Unwatched(io::Error),
}

impl std::fmt::Display for Unheld {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unheld::NoBridge => out.write_str("its run gave it no MCP bridge"),
            Unheld::Over => out.write_str("its run is over"),
            Unheld::Capacity => out.write_str("its run's budget has no room for it"),
            Unheld::Unmeasured(e) => write!(out, "what it holds: {e}"),
            Unheld::Unwatched(e) => write!(out, "its watcher's thread: {e}"),
        }
    }
}

/// Why an ask took no connection (R04).
enum Unpaired {
    /// Its member's run gave it no bridge.
    NoBridge,
    /// Its lane was let go.
    Gone,
    /// The run is over.
    Over,
}

/// A connection an ask took: the connection, its first read, and its charge to the run.
struct Paired<'a> {
    conn: Stream,
    first: First,
    _charged: Held<'a>,
}

impl Bridges {
    /// The connections of a run that gave `members` a bridge, each read ahead at most a
    /// `record`, charged to `charge`.
    pub(crate) fn new(members: Vec<u32>, record: usize, charge: Arc<Charge>) -> Bridges {
        Bridges {
            members,
            record,
            charge,
            held: Mutex::new(Waiting {
                connections: Vec::new(),
                over: false,
            }),
            changed: Condvar::new(),
        }
    }

    /// Whether the run gave member `member` a bridge.
    pub(crate) fn bridged(&self, member: u32) -> bool {
        self.members.contains(&member)
    }

    /// Holds `conn`, its VM's connection `number` for member `member`, charged to its run's
    /// budget, until an ask takes it, its harness's side ends, or the run ends. Its watcher
    /// ([`Bridges::watch`]) is its caller's to start. Refused, typed, its connection let go,
    /// where the member has no bridge, the run is over or its budget has no room.
    pub(crate) fn hold(&self, number: u64, member: u32, conn: Stream) -> Result<(), Unheld> {
        let refused = |why: Unheld| {
            shards_ipc::end(&conn);
            Err(why)
        };
        if !self.bridged(member) {
            return refused(Unheld::NoBridge);
        }
        let charged = match self.cost(&conn) {
            Ok(bytes) => bytes,
            Err(e) => return refused(Unheld::Unmeasured(e)),
        };
        if self.charge.grow(charged).is_err() {
            return refused(Unheld::Capacity);
        }
        let mut held = lock(&self.held);
        let room = held.connections.try_reserve(1);
        if held.over || room.is_err() {
            drop(held);
            let _ = self.charge.release(charged);
            return refused(if room.is_err() {
                Unheld::Capacity
            } else {
                Unheld::Over
            });
        }
        held.connections.push(Connection {
            number,
            member,
            conn,
            first: First::Awaited,
            taken: false,
            charged,
        });
        drop(held);
        self.changed.notify_all();
        Ok(())
    }

    /// What a held connection costs its run: its kernel queues, as an exec's connection is
    /// measured; the record its first read takes; and its entry.
    fn cost(&self, conn: &Stream) -> io::Result<u64> {
        let overflow = || io::Error::other("a held connection's charge overflows");
        let record = u64::try_from(self.record).map_err(|_| overflow())?;
        let entry = u64::try_from(size_of::<Connection>()).map_err(|_| overflow())?;
        ipc_buffers(conn)?
            .checked_add(record)
            .and_then(|n| n.checked_add(entry))
            .ok_or_else(overflow)
    }

    /// Connection `number`'s watcher: reads its harness's first bytes, at most a record, and
    /// keeps them for whoever takes it. One whose harness's side ends, or whose read fails,
    /// before a byte comes is let go if no ask has taken it, and said ended to the one that has.
    pub(crate) fn watch(&self, number: u64) {
        let conn = {
            let held = lock(&self.held);
            held.connections
                .iter()
                .find(|c| c.number == number)
                .map(|c| c.conn.try_clone())
        };
        let read = match conn {
            None => return,
            Some(Ok(conn)) => first_read(&conn, self.record),
            Some(Err(e)) => Err(e),
        };
        let mut held = lock(&self.held);
        let Some(at) = held.connections.iter().position(|c| c.number == number) else {
            return;
        };
        let first = match read {
            Ok(bytes) if !bytes.is_empty() => First::Came(bytes),
            _ => First::Ended,
        };
        let taken = held.connections.get(at).is_some_and(|c| c.taken);
        if taken || matches!(first, First::Came(_)) {
            if let Some(c) = held.connections.get_mut(at) {
                c.first = first;
            }
            drop(held);
        } else {
            let c = held.connections.remove(at);
            drop(held);
            self.let_go(&c);
        }
        self.changed.notify_all();
    }

    /// Connection `number`, held, let go: its watcher could not be started.
    pub(crate) fn release(&self, number: u64) {
        let mut held = lock(&self.held);
        let at = held.connections.iter().position(|c| c.number == number);
        let c = at.map(|at| held.connections.remove(at));
        drop(held);
        if let Some(c) = c {
            self.let_go(&c);
        }
        self.changed.notify_all();
    }

    /// Ends a connection no ask will take, its member's bridge with it, and gives back what
    /// it cost.
    fn let_go(&self, c: &Connection) {
        shards_ipc::end(&c.conn);
        let _ = self.charge.release(c.charged);
    }

    /// Member `member`'s oldest connection held, once its first read is done: waited for until
    /// one comes, `gone` says its ask was let go ([`Bridges::wake`]), or the run is over. One
    /// taken whose ask is let go before its first read is done is the next ask's.
    fn take(&self, member: u32, gone: &AtomicBool) -> Result<Paired<'_>, Unpaired> {
        if !self.bridged(member) {
            return Err(Unpaired::NoBridge);
        }
        let mut held = lock(&self.held);
        let number = loop {
            if held.over {
                return Err(Unpaired::Over);
            }
            if gone.load(Ordering::Acquire) {
                return Err(Unpaired::Gone);
            }
            if let Some(c) = held
                .connections
                .iter_mut()
                .find(|c| c.member == member && !c.taken)
            {
                c.taken = true;
                break c.number;
            }
            held = self
                .changed
                .wait(held)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        };
        loop {
            // Let go meanwhile: the run is over, or its watcher never started.
            let Some(at) = held.connections.iter().position(|c| c.number == number) else {
                return Err(Unpaired::Over);
            };
            let waits = held
                .connections
                .get(at)
                .is_some_and(|c| matches!(c.first, First::Awaited));
            if !waits {
                let c = held.connections.remove(at);
                return Ok(Paired {
                    conn: c.conn,
                    first: c.first,
                    _charged: Held::of(&self.charge, c.charged),
                });
            }
            if gone.load(Ordering::Acquire) {
                if let Some(c) = held.connections.get_mut(at) {
                    c.taken = false;
                }
                drop(held);
                self.changed.notify_all();
                return Err(Unpaired::Gone);
            }
            held = self
                .changed
                .wait(held)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Wakes every wait, for an ask whose lane was let go to see it.
    fn wake(&self) {
        let _held = lock(&self.held);
        self.changed.notify_all();
    }

    /// The run is over: every connection held let go, and every wait ends.
    pub(crate) fn close(&self) {
        let mut held = lock(&self.held);
        held.over = true;
        let connections = std::mem::take(&mut held.connections);
        drop(held);
        for c in &connections {
            self.let_go(c);
        }
        self.changed.notify_all();
    }
}

/// A connection's first read: what its harness sent first, at most `record` bytes; none where
/// its side ended first.
fn first_read(conn: &Stream, record: usize) -> io::Result<Vec<u8>> {
    let mut bytes = execs::buffer(record).ok_or_else(|| io::Error::other("a first read's record"))?;
    let n = shards_ipc::Polled(conn).read(&mut bytes)?;
    bytes.truncate(n);
    Ok(bytes)
}

/// One MCP ask (D254), from its ask to its end: refused where its run gave its member no
/// bridge, charged as an exec of one output and a stdin is, paired with its member's next
/// connection, told started, carried both ways, and its end told.
pub(super) fn carry(start: Start, hosting: &execs::Hosting<'_>) {
    let Start {
        ask: exec::Ask { member, .. },
        exec: number,
        stdout,
        stdin,
        steer,
        replies,
        ..
    } = start;
    let say = |what: &str| {
        (hosting.say)(&format!(
            "agent-control MCP connection {number} of member {member}: {what}"
        ))
    };
    let refuse = |why: Unstarted, what: &str| {
        say(what);
        replies.tell(Reply::Unstarted(why));
    };
    let Some(bridges) = hosting.bridges.filter(|b| b.bridged(member)) else {
        return refuse(Unstarted::Refused, "its run gave it no MCP bridge");
    };
    let Some((relay, input)) = stdin else {
        return refuse(Unstarted::Failed, "an ask with no stdin");
    };
    let cost = exec::lane_bytes(hosting.admissions, false, true)
        .zip(u64::try_from(size_of::<Start>()).ok())
        .zip(u64::try_from(stdout.0.storage_capacity()).ok())
        .and_then(|((lanes, host), record)| {
            lanes
                .checked_add(host)?
                .checked_add(record.checked_mul(2)?)?
                .checked_add(u64::from(input.room()))
        });
    let Some(bytes) = cost else {
        return refuse(Unstarted::Failed, "its charge overflows");
    };
    let _held = match Held::new(hosting.charge, bytes) {
        Ok(held) => held,
        Err(e) => return refuse(Unstarted::Capacity, &format!("{bytes} bytes: {e:?}")),
    };
    let Some(mut buffer) = execs::buffer(stdout.0.storage_capacity()) else {
        return refuse(Unstarted::Failed, "its output's buffer");
    };
    let Ok(steered) = replies.try_clone() else {
        return refuse(Unstarted::Failed, "its steering");
    };
    let gone = AtomicBool::new(false);
    // Its connection once paired, for its steering's close to hang up.
    let hang: Mutex<Option<Stream>> = Mutex::new(None);
    std::thread::scope(|s| {
        let (gone, hang) = (&gone, &hang);
        // Its lane let go is seen while it waits for its connection and once it carries one:
        // an MCP connection takes no steering, each steer let go.
        let steering = std::thread::Builder::new()
            .name("mcp-steer".into())
            .spawn_scoped(s, move || {
                while steer.recv().is_ok() {
                    steered.took();
                }
                let hung = lock(hang);
                gone.store(true, Ordering::Release);
                if let Some(conn) = hung.as_ref() {
                    shards_ipc::end(conn);
                }
                drop(hung);
                bridges.wake();
            });
        if let Err(e) = steering {
            return refuse(Unstarted::Failed, &format!("its steering's thread: {e}"));
        }
        let Paired {
            conn,
            first,
            _charged,
        } = match bridges.take(member, gone) {
            Ok(paired) => paired,
            Err(Unpaired::NoBridge) => return refuse(Unstarted::Refused, "its run gave it no MCP bridge"),
            Err(Unpaired::Gone) => return,
            Err(Unpaired::Over) => return refuse(Unstarted::Failed, "its run is over"),
        };
        let writes = {
            let mut hung = lock(hang);
            let writes = conn.try_clone();
            *hung = conn.try_clone().ok();
            if gone.load(Ordering::Acquire) {
                shards_ipc::end(&conn);
            }
            writes
        };
        let Ok(writes) = writes else {
            shards_ipc::end(&conn);
            return refuse(Unstarted::Failed, "its connection's writes");
        };
        let (relay, input) = (&relay, &input);
        let feeder = std::thread::Builder::new()
            .name("mcp-to".into())
            .spawn_scoped(s, move || feed(relay, input, &writes));
        if let Err(e) = feeder {
            shards_ipc::end(&conn);
            return refuse(Unstarted::Failed, &format!("its input's thread: {e}"));
        }
        replies.tell(Reply::Started);
        let first = match first {
            First::Came(bytes) => bytes,
            First::Awaited | First::Ended => Vec::new(),
        };
        let pumped = execs::pump(
            &stdout.0,
            io::Cursor::new(first).chain(shards_ipc::Polled(&conn)),
            &mut buffer,
        );
        match pumped {
            // The harness's side ended, every byte of it taken: told once its output has
            // finished, its input, where still open, ended as no longer read.
            Pumped::Ended => {
                let _ = stdout.1.finish();
                if let input::State::Open { at } = input.state() {
                    input.ended(at, InputEnd::Aborted(execs::EPIPE));
                }
                replies.tell(Reply::Exited(0));
            }
            Pumped::Failed => {
                say("its connection failed: its VM gone");
                stdout.1.abort();
            }
            Pumped::Unread => {}
        }
        shards_ipc::end(&conn);
    });
    // Its steering closed, every helper ended: its streams' controls go now.
    drop((stdout, relay, input));
}

/// Writes the controller's bytes to the harness's connection as they come, each record told
/// taken once the connection has it all: its end the connection's half-close, its abort the
/// connection's end both ways, as a socket lane's is (D251). A write the connection refuses
/// ends the input, `EPIPE` where its reader is gone, else `EIO`.
fn feed(relay: &input::Relay, control: &input::Control, conn: &Stream) {
    let mut open = true;
    let mut written = 0u64;
    loop {
        match relay.next() {
            input::Next::Record { bytes, .. } => {
                let wrote = match open {
                    true => (&*conn).write_all(&bytes),
                    false => Err(io::ErrorKind::BrokenPipe.into()),
                };
                match wrote {
                    Ok(()) => {
                        written = written.saturating_add(bytes.len() as u64);
                        relay.delivered(bytes);
                    }
                    Err(e) => {
                        open = false;
                        let errno = if e.kind() == io::ErrorKind::BrokenPipe {
                            execs::EPIPE
                        } else {
                            execs::EIO
                        };
                        control.ended(written, InputEnd::Aborted(errno));
                    }
                }
            }
            input::Next::End => {
                open = false;
                let _ = conn.shutdown(Shutdown::Write);
                control.ended(written, InputEnd::Closed);
            }
            input::Next::Abort(errno) => {
                open = false;
                shards_ipc::end(conn);
                control.ended(written, InputEnd::Aborted(errno));
            }
            input::Next::Gone => return,
        }
    }
}
