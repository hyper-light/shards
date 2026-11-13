//! A connection whose bytes go through an engine ring (D230): what its owner's cue turns
//! it into, how it moves its bytes once it is, and how it lets the ring go.
//!
//! Its states (R04):
//! - `Plain`: vsock as ever.
//! - `Ringable`: a ring port's connection whose owner may yet offer one.
//! - `Refused`: a refusal owed the guest (of the hello's offer, or of a swap's), then `Plain`.
//! - `Ringed`: attached, its reply owed until sent. Every byte the guest sends is a control
//!   byte ([`abi::DOORBELL`], [`abi::END`], [`abi::DETACH`], [`abi::SWAP`]); every byte the
//!   host sends goes into the ring for the guest. The host socket's reads fill that ring as
//!   the socket is readable, not as the guest's buffers come, so a turn that rings no doorbell
//!   takes none of them; the guest's doorbells, and the socket's room, drain the ring for the
//!   host into the socket. Each direction moves at most a ring's length a turn, so that one
//!   connection's stream leaves the device's other connections their turns, as a packet's 64
//!   KiB at most does.
//! - `Detached`: the slot let go, its pins dropped as it went, nothing of it written since.
//!   The device's [`abi::DETACH`] is owed until sent (after the old ring's reply, where that
//!   was still owed), and the guest's bytes are control bytes until its own `DETACH` (then
//!   plain) or `SWAP` (then a new slot's offer, answered once the device's `DETACH` has gone:
//!   `Ringed` again, or `Refused`).
//!
//! A ring is let go as the guest asks (`DETACH`, `SWAP`), as the host's socket ends (whose
//! shutdown then waits for the guest's `DETACH`, so that a `SWAP` crossing it is answered),
//! and as a broken rule or a failed socket would have ended it, which is then ended by an
//! RST after the device's `DETACH`, where the guest's credit has room for it. Whatever the
//! cause, the guest hears `DETACH` only once the ring is gone.

use std::io::{self, Read as _};
use std::sync::Arc;

use shards_abi::ring::{self as abi, Offer, Refusal, Reply};

use super::super::Span;
use super::super::packet::{Header, op, shutdown};
use super::super::ring::{Broken, CUE, Ring, RingHandle};
use super::{Conn, HostWrites, Phase};
use crate::debug;
use crate::memory::GuestMemory;

/// Where a connection stands with a ring.
pub(super) enum Ringing {
    /// Plain vsock: not a ring port's connection, or its chance at a ring has passed.
    Plain,
    /// A ring port's connection whose owner may yet offer one.
    Ringable(RingHandle),
    /// An offer refused: the reply owed, plain vsock after.
    Refused(Box<Refused>),
    /// Attached.
    Ringed(Box<Ringed>),
    /// Its ring let go.
    Detached(Box<Detached>),
}

pub(super) struct Refused {
    reply: Reply,
    memory: Arc<GuestMemory>,
    /// The host's socket had ended: its shutdown follows the reply.
    host_ended: bool,
}

pub(super) struct Ringed {
    ring: Ring,
    /// Whether the guest has been told it is attached.
    replied: bool,
    /// A doorbell owed the guest.
    doorbell: bool,
    /// The host socket took none of what the ring for the host holds, or a turn's length
    /// of it: drained again once it is writable.
    blocked: bool,
    /// The ring for the guest is full: the host socket is read no more until the guest
    /// rings.
    full: bool,
    /// The guest's client ended its writes ([`abi::END`]): once the ring for the host is
    /// drained, the host socket's writes end.
    ended: bool,
}

pub(super) struct Detached {
    memory: Arc<GuestMemory>,
    why: Why,
    /// The let-go ring's reply, owed still: it goes before the device's `DETACH`.
    reply: bool,
    /// The device's `DETACH` sent.
    said: bool,
    guest: Guest,
}

/// What let a ring go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    /// The guest's `DETACH` or `SWAP`.
    Asked,
    /// The host's socket ended.
    HostEnded,
    /// A broken rule or a failed host socket: the connection ends.
    Broken,
}

/// What the guest's bytes are after a ring is let go.
enum Guest {
    /// Its `DETACH` or `SWAP` not heard yet: control bytes.
    Unheard,
    /// Its `DETACH` heard: plain.
    Plain,
    /// Its `SWAP` heard: the new slot's offer, as much as has come.
    Offering(Vec<u8>),
    /// Its `SWAP`'s offer whole: the ring it named attached, or why not, told once the
    /// device's `DETACH` has gone.
    Answered(Result<Ring, Refusal>),
}

/// What a turn of the host socket into the ring for the guest came to.
enum Filled {
    /// As much as there was, or a turn's worth.
    Moved,
    /// The host socket's end.
    End,
}

/// The longest control packet: a reply.
const REPLY_LEN: u32 = abi::REPLY as u32;

impl Conn {
    /// Makes this a ring port's connection, whose owner may offer a ring through `handle`.
    pub fn ringable(&mut self, handle: RingHandle) {
        self.ringing = Ringing::Ringable(handle);
    }

    /// Whether a ring's reply, `DETACH`, refusal or doorbell is owed the guest now, as its
    /// credit allows.
    pub(super) fn ring_owes(&self) -> bool {
        let credit = self.guest_credit();
        match &self.ringing {
            Ringing::Refused(_) => credit >= REPLY_LEN,
            Ringing::Ringed(r) => (!r.replied && credit >= REPLY_LEN) || (r.doorbell && credit > 0),
            Ringing::Detached(d) => {
                (d.reply && credit >= REPLY_LEN)
                    || (!d.reply && !d.said && (credit > 0 || d.why == Why::Broken))
            }
            Ringing::Plain | Ringing::Ringable(_) => false,
        }
    }

    /// The host socket's events a ring's connection waits for, where they are not plain
    /// vsock's.
    pub(super) fn ring_interest(&self) -> Option<(bool, bool)> {
        match &self.ringing {
            Ringing::Ringed(r) => {
                let read = !self.killed
                    && self.phase == Phase::Established
                    && !self.host_eof
                    && !self.guest_rcv_closed
                    && !r.full;
                Some((read, !self.killed && r.blocked))
            }
            // The host's bytes wait for a swap's answer, or for the end the guest hears.
            Ringing::Detached(d) if !matches!(d.guest, Guest::Plain) => Some((false, false)),
            _ => None,
        }
    }

    /// The host socket is readable: the owner's cue attaches a ring, or its bytes fill the
    /// one attached. Whether it was a ring's to handle.
    pub(super) fn ring_on_readable(&mut self) -> bool {
        if self.killed || self.phase != Phase::Established {
            return false;
        }
        match &self.ringing {
            Ringing::Plain | Ringing::Refused(_) => false,
            Ringing::Detached(d) => !matches!(d.guest, Guest::Plain),
            Ringing::Ringable(handle) => {
                let handle = handle.clone();
                self.cue(handle)
            }
            Ringing::Ringed(_) => {
                match self.fill() {
                    Ok(Filled::Moved) => {}
                    Ok(Filled::End) => {
                        // What the host sent is all in the ring, whose head says it: the
                        // ring goes, and the guest is told once it has said its own DETACH.
                        self.host_eof = true;
                        self.let_go(Why::HostEnded, Guest::Unheard);
                    }
                    Err(e) => self.broken(e),
                }
                true
            }
        }
    }

    /// The owner's offer, as its cue arrives: attached, or refused.
    fn cue(&mut self, handle: RingHandle) -> bool {
        let Some(offered) = handle.take() else {
            // The host sent something before any offer: none is coming.
            self.ringing = Ringing::Plain;
            return false;
        };
        let mut byte = [0u8; 1];
        match (&self.stream).read(&mut byte) {
            Ok(1) if byte == [CUE] => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {
                handle.put(offered);
                return true;
            }
            Ok(0) => {
                // The owner went before its cue: the connection ends as plain.
                self.ringing = Ringing::Plain;
                return false;
            }
            Ok(_) | Err(_) => {
                debug!("vsock: port {}: no cue after a ring's offer", self.local_port);
                self.kill();
                return true;
            }
        }
        self.ringing = match offered.and_then(|offer| Ring::attach(handle.memory(), &offer)) {
            Ok(ring) => self.attached(ring),
            Err(refusal) => {
                debug!("vsock: port {}: a ring refused: {refusal:?}", self.local_port);
                refused(Reply::Refused(refusal), handle.memory().clone(), false)
            }
        };
        true
    }

    fn attached(&self, ring: Ring) -> Ringing {
        debug!(
            "vsock: port {}: an engine ring attached, {} bytes pinned",
            self.local_port,
            ring.pinned()
        );
        Ringing::Ringed(Box::new(Ringed {
            ring,
            replied: false,
            doorbell: false,
            blocked: false,
            full: false,
            ended: false,
        }))
    }

    /// Lets the attached ring go, for `why`: its pins are dropped here, and nothing more of
    /// the slot is written, before anything is owed the guest that says so.
    fn let_go(&mut self, why: Why, guest: Guest) {
        if !matches!(self.ringing, Ringing::Ringed(_)) {
            return;
        }
        let Ringing::Ringed(r) = std::mem::replace(&mut self.ringing, Ringing::Plain) else {
            return;
        };
        let (memory, pinned, reply) = (r.ring.memory().clone(), r.ring.pinned(), !r.replied);
        drop(r);
        debug!(
            "vsock: port {}: an engine ring let go, {pinned} bytes unpinned ({why:?})",
            self.local_port
        );
        self.ringing = Ringing::Detached(Box::new(Detached {
            memory,
            why,
            reply,
            said: false,
            guest,
        }));
    }

    /// Moves a let-go ring's connection on as far as what has been said lets it: plain once
    /// both sides' `DETACH`es are said (the host's end then said, where it came), a swap's
    /// answer once the device's has, the RST of a broken one once its `DETACH` has gone.
    fn settle(&mut self) {
        let next = match &self.ringing {
            Ringing::Detached(d) if d.said => match (&d.guest, d.why) {
                (_, Why::Broken) => Next::Reset,
                // A guest that has ended its sends says nothing more, its DETACH among it.
                (Guest::Unheard, why) if self.guest_send_closed => Next::Plain {
                    host_ended: why == Why::HostEnded,
                },
                (Guest::Plain, why) => Next::Plain {
                    host_ended: why == Why::HostEnded,
                },
                (Guest::Answered(_), why) => Next::Answer {
                    host_ended: why == Why::HostEnded,
                },
                (Guest::Unheard | Guest::Offering(_), _) => return,
            },
            _ => return,
        };
        match next {
            Next::Reset => self.kill(),
            Next::Plain { host_ended } => {
                if host_ended {
                    self.pending.shutdown |= shutdown::SEND;
                }
                self.ringing = Ringing::Plain;
            }
            Next::Answer { host_ended } => {
                let Ringing::Detached(d) = std::mem::replace(&mut self.ringing, Ringing::Plain) else {
                    return;
                };
                let Detached { memory, guest, .. } = *d;
                let Guest::Answered(answer) = guest else {
                    return;
                };
                self.ringing = match answer {
                    Ok(ring) => self.attached(ring),
                    Err(refusal) => {
                        debug!("vsock: port {}: a swap refused: {refusal:?}", self.local_port);
                        refused(Reply::Refused(refusal), memory, host_ended)
                    }
                };
            }
        }
    }

    /// Fills the ring for the guest from the host socket, a ring's length at most.
    fn fill(&mut self) -> Result<Filled, Broken> {
        let Ringing::Ringed(r) = &mut self.ringing else {
            return Ok(Filled::Moved);
        };
        let quantum = r.ring.size();
        let mut moved = 0usize;
        loop {
            if r.ring.room_to_guest()? == 0 {
                if r.ring.wait_for_room()? {
                    continue;
                }
                r.full = true;
                return Ok(Filled::Moved);
            }
            let turn = r.ring.from_host(&self.stream)?;
            r.doorbell |= turn.ring;
            if turn.eof {
                return Ok(Filled::End);
            }
            moved = moved.saturating_add(turn.moved);
            if turn.moved == 0 || moved >= quantum {
                return Ok(Filled::Moved);
            }
        }
    }

    /// Drains the ring for the host into the host socket, a ring's length at most: whether
    /// nothing is left of it, so that the guest's shutdown may follow. True where no ring
    /// is attached; false where the ring broke, which is let go.
    pub(super) fn ring_drained(&mut self) -> io::Result<bool> {
        let drained = match self.drain() {
            Ok(drained) => drained,
            Err(e) => {
                self.broken(e);
                return Ok(false);
            }
        };
        let Ringing::Ringed(r) = &mut self.ringing else {
            // A guest's shutdown may settle a let-go ring's connection.
            self.settle();
            return Ok(true);
        };
        r.blocked = !drained;
        // The guest's client ended its writes: they end at the host too, after its last.
        if drained && r.ended && self.host_writes == HostWrites::Open {
            self.host_writes = HostWrites::Shut;
            let _ = self.stream.shutdown(std::net::Shutdown::Write);
        }
        Ok(drained)
    }

    fn drain(&mut self) -> Result<bool, Broken> {
        let Ringing::Ringed(r) = &mut self.ringing else {
            return Ok(true);
        };
        let send_closed = self.guest_send_closed || r.ended;
        let quantum = r.ring.size();
        let mut moved = 0usize;
        loop {
            let pending = r.ring.pending_to_host()?;
            if pending == 0 {
                // After the guest's shutdown nothing more comes; before it, the device
                // says it waits, and looks once more.
                if send_closed || !r.ring.wait_for_data()? {
                    return Ok(true);
                }
                continue;
            }
            let turn = r.ring.to_host(&self.stream)?;
            r.doorbell |= turn.ring;
            moved = moved.saturating_add(turn.moved);
            if turn.moved == 0 || moved >= quantum {
                return Ok(false);
            }
        }
    }

    /// What the guest sent on a ring's connection: control bytes, and after its `DETACH`
    /// plain bytes, which go on as plain vsock's. Whether all of it was a ring's to handle.
    pub(super) fn ring_doorbells(&mut self, h: &Header, payload: &[Span], mem: &GuestMemory) -> bool {
        let kind = match &self.ringing {
            Ringing::Ringed(_) => Bytes::Control,
            Ringing::Detached(d) => match &d.guest {
                Guest::Plain => return false,
                Guest::Unheard if d.why == Why::Broken => Bytes::Ignored,
                Guest::Unheard => Bytes::Unheard,
                Guest::Offering(_) => Bytes::Offer,
                Guest::Answered(_) => Bytes::Early,
            },
            Ringing::Plain | Ringing::Ringable(_) | Ringing::Refused(_) => return false,
        };
        let total = h.len as usize;
        let taken = match kind {
            Bytes::Control => self.ringed_bytes(payload, mem),
            Bytes::Unheard => self.unheard_bytes(payload, mem),
            Bytes::Offer => self.offer_bytes(payload, mem),
            Bytes::Ignored => Some(total),
            Bytes::Early => {
                debug!("vsock: port {}: bytes before a swap's answer", self.local_port);
                None
            }
        };
        let Some(taken) = taken else {
            self.broken(Broken::Socket(io::ErrorKind::InvalidData));
            return true;
        };
        // The control bytes are taken here; what follows the guest's `DETACH` is plain.
        let taken = taken.min(total);
        self.fwd_cnt += taken as u32;
        if taken < total
            && !self.killed
            && let Err(e) = self.send_host(&skip(payload, taken), mem)
        {
            debug!("vsock: guest data for port {}: {e}", self.local_port);
            self.kill();
            return true;
        }
        if self.guest_needs_credit() {
            self.pending.credit_update = true;
        }
        if let Err(e) = self.flush() {
            debug!("vsock: host socket for port {}: {e}", self.local_port);
            self.kill();
        }
        self.settle();
        true
    }

    /// An attached ring's control bytes, all of the payload's, and a `SWAP`'s offer after
    /// them: how many bytes they were, or None for a broken rule.
    fn ringed_bytes(&mut self, payload: &[Span], mem: &GuestMemory) -> Option<usize> {
        let mut last = 0u8;
        let mut ended = false;
        let seen = scan(payload, mem, |b| {
            last = b;
            match b {
                abi::DOORBELL => true,
                abi::END => {
                    ended = true;
                    true
                }
                _ => false,
            }
        })?;
        if let Ringing::Ringed(r) = &mut self.ringing {
            // The guest has rung: room may have come in the ring for the guest.
            r.full = false;
            r.ended |= ended;
        }
        let total: usize = payload.iter().map(|s| s.len).sum();
        if seen == total && matches!(last, abi::DOORBELL | abi::END) {
            return Some(total);
        }
        match last {
            // The guest's last byte for this slot: nothing of its may follow it until the
            // device's DETACH, for which it waits.
            abi::DETACH if seen == total => {
                self.let_go(Why::Asked, Guest::Plain);
                Some(total)
            }
            abi::SWAP => {
                self.let_go(Why::Asked, Guest::Offering(Vec::new()));
                let offer = self.offer_bytes(&skip(payload, seen), mem)?;
                Some(seen + offer)
            }
            _ => {
                debug!("vsock: port {}: a byte that is no control byte", self.local_port);
                None
            }
        }
    }

    /// A let-go ring's guest's control bytes before its own `DETACH` or `SWAP`: how many of
    /// the payload's they are, those after a `DETACH` plain; None for a broken rule.
    fn unheard_bytes(&mut self, payload: &[Span], mem: &GuestMemory) -> Option<usize> {
        let mut last = 0u8;
        // Said before the guest heard the device's DETACH: nothing to do. Its client's end
        // it says again over plain vsock, after what the ring held.
        let seen = scan(payload, mem, |b| {
            last = b;
            matches!(b, abi::DOORBELL | abi::END)
        })?;
        let total: usize = payload.iter().map(|s| s.len).sum();
        if seen == total && matches!(last, abi::DOORBELL | abi::END) {
            return Some(total);
        }
        let Ringing::Detached(d) = &mut self.ringing else {
            return Some(total);
        };
        match last {
            abi::DETACH => {
                d.guest = Guest::Plain;
                Some(seen)
            }
            abi::SWAP => {
                d.guest = Guest::Offering(Vec::new());
                let offer = self.offer_bytes(&skip(payload, seen), mem)?;
                Some(seen + offer)
            }
            _ => {
                debug!("vsock: port {}: a byte that is no control byte", self.local_port);
                None
            }
        }
    }

    /// A swap's offer, as its bytes come, its own fixed part saying how long it is: read
    /// whole, it is answered. How many bytes it took, which are all the payload's; None for
    /// a fixed part that says no offer, or bytes past it, which the guest may not send
    /// before its answer.
    fn offer_bytes(&mut self, payload: &[Span], mem: &GuestMemory) -> Option<usize> {
        let Ringing::Detached(d) = &mut self.ringing else {
            return None;
        };
        let Guest::Offering(got) = &mut d.guest else {
            return None;
        };
        let total: usize = payload.iter().map(|s| s.len).sum();
        let mut taken = 0usize;
        while taken < total {
            // Its fixed part first, then as much as it says: never past `MAX_OFFER`, which
            // the fixed part's check bounds every offer by.
            let want = match got.first_chunk::<{ abi::OFFER_HEADER }>() {
                Some(head) => abi::offer_len(head).ok()?,
                None => abi::OFFER_HEADER,
            };
            let room = want.checked_sub(got.len())?;
            if room == 0 {
                // Bytes past a whole offer.
                return None;
            }
            let n = room.min(total - taken);
            let at = got.len();
            got.try_reserve(n).ok()?;
            got.resize(at + n, 0);
            read_into(&skip(payload, taken), mem, got.get_mut(at..)?)?;
            taken += n;
        }
        let whole = got
            .first_chunk::<{ abi::OFFER_HEADER }>()
            .and_then(|head| abi::offer_len(head).ok())
            .is_some_and(|len| len == got.len());
        if whole {
            let offer = std::mem::take(got);
            let answer = if d.why == Why::HostEnded {
                Err(Refusal::Ended)
            } else {
                Offer::parse(&offer)
                    .map_err(|_| Refusal::Malformed)
                    .and_then(|offer| Ring::attach(&d.memory, &offer))
            };
            d.guest = Guest::Answered(answer);
        }
        Some(total)
    }

    /// A ring's packet owed the guest, written into `space`: its reply, its doorbells, its
    /// `DETACH`, a refusal.
    pub(super) fn ring_next_rx(&mut self, space: &[Span], spare: u32) -> Option<Header> {
        let credit = self.guest_credit();
        let owed = match &self.ringing {
            Ringing::Refused(r) if credit >= REPLY_LEN => Owed::Reply(r.reply),
            Ringing::Ringed(r) if !r.replied && credit >= REPLY_LEN => Owed::Reply(Reply::Attached),
            Ringing::Ringed(r) if r.doorbell && credit > 0 => Owed::Doorbell,
            Ringing::Detached(d) if d.reply && credit >= REPLY_LEN => Owed::Reply(Reply::Attached),
            Ringing::Detached(d) if !d.reply && !d.said && credit > 0 => Owed::Detach,
            // A broken one with no credit for its DETACH ends at once, unsaid.
            Ringing::Detached(d) if !d.reply && !d.said && d.why == Why::Broken => Owed::Reset,
            _ => return None,
        };
        let memory = match &self.ringing {
            Ringing::Refused(r) => r.memory.clone(),
            Ringing::Ringed(r) => r.ring.memory().clone(),
            Ringing::Detached(d) => d.memory.clone(),
            Ringing::Plain | Ringing::Ringable(_) => return None,
        };
        let bytes = match owed {
            Owed::Reply(reply) => reply.encode().to_vec(),
            Owed::Doorbell => vec![abi::DOORBELL],
            Owed::Detach => vec![abi::DETACH],
            Owed::Reset => {
                self.kill();
                self.pending = Default::default();
                return Some(self.header(op::RST, spare));
            }
        };
        match &mut self.ringing {
            Ringing::Ringed(r) if owed == Owed::Doorbell => r.doorbell = false,
            Ringing::Ringed(r) => r.replied = true,
            Ringing::Detached(d) if owed == Owed::Detach => d.said = true,
            Ringing::Detached(d) => d.reply = false,
            Ringing::Refused(r) => {
                if r.host_ended {
                    self.pending.shutdown |= shutdown::SEND;
                }
                self.ringing = Ringing::Plain;
            }
            Ringing::Plain | Ringing::Ringable(_) => {}
        }
        if !put(&memory, space, &bytes) {
            debug!(
                "vsock: port {}: an RX buffer too short for a ring's packet",
                self.local_port
            );
            self.kill();
            self.pending = Default::default();
            return Some(self.header(op::RST, spare));
        }
        let len = bytes.len() as u32;
        self.rx_cnt += len;
        let mut h = self.header(op::RW, spare);
        h.len = len;
        self.settle();
        Some(h)
    }

    /// Lets go of a connection's ring that broke, then ends it: an RST after the device's
    /// `DETACH`.
    fn broken(&mut self, e: Broken) {
        debug!("vsock: port {}: its ring: {e:?}", self.local_port);
        match &mut self.ringing {
            Ringing::Ringed(_) => self.let_go(Why::Broken, Guest::Unheard),
            // Let go already, its DETACH perhaps unsaid: the RST follows it.
            Ringing::Detached(d) => {
                d.why = Why::Broken;
                d.guest = Guest::Unheard;
                self.settle();
            }
            Ringing::Plain | Ringing::Ringable(_) | Ringing::Refused(_) => self.kill(),
        }
    }
}

fn refused(reply: Reply, memory: Arc<GuestMemory>, host_ended: bool) -> Ringing {
    Ringing::Refused(Box::new(Refused {
        reply,
        memory,
        host_ended,
    }))
}

/// A ring's packet owed the guest ([`Conn::ring_next_rx`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owed {
    Reply(Reply),
    Doorbell,
    Detach,
    /// A broken one's RST, its DETACH unsaid for want of credit.
    Reset,
}

/// Where a let-go ring's connection goes next ([`Conn::settle`]).
enum Next {
    /// An RST: it broke.
    Reset,
    /// Plain vsock, the host's end said first where it came.
    Plain { host_ended: bool },
    /// A swap's answer.
    Answer { host_ended: bool },
}

/// How a ring's packet's bytes are read, as its connection stands.
enum Bytes {
    /// An attached ring's control bytes.
    Control,
    /// A let-go ring's guest's, before its own `DETACH` or `SWAP`.
    Unheard,
    /// A swap's offer.
    Offer,
    /// A broken one's, which ends at its `DETACH`.
    Ignored,
    /// Before a swap's answer, where the guest waits for it: a broken rule.
    Early,
}

/// Gives `each` the guest's bytes of `payload` in turn until it says to stop: how many it
/// was given, the one it stopped at among them. None where they cannot be read. A byte a
/// read: the relay says each control byte with a send of its own, so that a packet holds
/// one (a swap's offer, after its byte, is read whole by [`read_into`]).
fn scan(payload: &[Span], mem: &GuestMemory, mut each: impl FnMut(u8) -> bool) -> Option<usize> {
    let access = mem.access().ok()?;
    let mut seen = 0usize;
    for s in payload {
        for at in 0..s.len {
            let mut byte = [0u8; 1];
            access.read(s.gpa + at as u64, &mut byte).ok()?;
            seen += 1;
            let [b] = byte;
            if !each(b) {
                return Some(seen);
            }
        }
    }
    Some(seen)
}

/// Reads the first `out.len()` bytes of `payload` into `out`.
fn read_into(payload: &[Span], mem: &GuestMemory, out: &mut [u8]) -> Option<()> {
    let access = mem.access().ok()?;
    let mut at = 0usize;
    for s in payload {
        if at == out.len() {
            break;
        }
        let n = s.len.min(out.len() - at);
        access.read(s.gpa, out.get_mut(at..at + n)?).ok()?;
        at += n;
    }
    (at == out.len()).then_some(())
}

/// `payload` past its first `n` bytes.
fn skip(payload: &[Span], mut n: usize) -> Vec<Span> {
    let mut out = Vec::new();
    for s in payload {
        if n >= s.len {
            n -= s.len;
            continue;
        }
        out.push(Span {
            gpa: s.gpa + n as u64,
            // SAFETY: `n` lies within the span, whose host mapping covers it whole.
            ptr: unsafe { s.ptr.add(n) },
            len: s.len - n,
        });
        n = 0;
    }
    out
}

/// Writes `bytes` into the guest's buffer `space`, in its order: whether all fit.
fn put(memory: &GuestMemory, space: &[Span], bytes: &[u8]) -> bool {
    let Ok(access) = memory.access() else {
        return false;
    };
    let mut rest = bytes;
    for s in space {
        if rest.is_empty() {
            break;
        }
        let Some((now, later)) = rest.split_at_checked(rest.len().min(s.len)) else {
            return false;
        };
        if access.write(s.gpa, now).is_err() {
            return false;
        }
        rest = later;
    }
    rest.is_empty()
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::io::Write as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::mpsc::Receiver;

    use shards_abi::ring::{Extent, Offer, Refusal, field};

    use super::super::super::muxer::Muxer;
    use super::super::super::packet::{HOST_CID, TYPE_STREAM};
    use super::*;
    use crate::platform::UnixStream;
    use crate::vm::{ServedPort, VsockHost};

    const PORT: u32 = 1029;
    const FROM: u32 = 49_152;
    const RING: u32 = 64 << 10;
    /// Where the slot is, where the guest's packets' payloads are, and its RX buffer.
    const SLOT: u64 = 1 << 20;
    const TX: u64 = 8 << 20;
    const RX: u64 = 9 << 20;

    struct Guest {
        mem: Arc<GuestMemory>,
        m: Muxer,
        owner: UnixStream,
        handle: RingHandle,
        fwd: u32,
    }

    fn header(op: u16, len: u32, flags: u32) -> Header {
        Header {
            src_cid: 3,
            dst_cid: HOST_CID,
            src_port: FROM,
            dst_port: PORT,
            len,
            kind: TYPE_STREAM,
            op,
            flags,
            buf_alloc: 1 << 18,
            fwd_cnt: 0,
        }
    }

    fn whole(gpa: u64) -> Offer {
        Offer {
            ring: RING,
            extents: vec![Extent {
                gpa,
                len: abi::slot_len(RING),
            }],
        }
    }

    impl Guest {
        /// A guest connected to a ring port, its owner holding the host's end.
        fn connect() -> Guest {
            let (sender, arrived): (_, Receiver<(UnixStream, RingHandle)>) = std::sync::mpsc::channel();
            let host = VsockHost {
                path: None,
                ports: Vec::new(),
                every: vec![(PORT, ServedPort::Ringed(sender))],
                rate: None,
            };
            let mut m = Muxer::new(host, 3).unwrap();
            let mem = Arc::new(GuestMemory::anonymous(&[(0, 16 << 20)]).unwrap());
            m.set_memory(mem.clone());
            m.on_guest_packet(&header(op::REQUEST, 0, 0), &[], &mem);
            assert_eq!(m.next_rx(&[]).map(|h| h.op), Some(op::RESPONSE));
            let (owner, handle) = arrived.try_recv().unwrap();
            owner.set_nonblocking(true).unwrap();
            Guest {
                mem,
                m,
                owner,
                handle,
                fwd: 0,
            }
        }

        /// The host's sockets all ready: what the device makes of them.
        fn turn(&mut self) {
            let mut interests = Vec::new();
            self.m.interests(&mut interests);
            let ready: Vec<_> = interests
                .iter()
                .map(|i| super::super::super::poll::Ready {
                    token: i.token,
                    read: i.read,
                    write: i.write,
                })
                .collect();
            self.m.on_events(&ready);
        }

        /// The next packet for the guest, and its payload.
        fn next(&mut self) -> Option<(Header, Vec<u8>)> {
            let space = [Span {
                gpa: RX,
                ptr: self.mem.host_ptr(RX, 4096).unwrap(),
                len: 4096,
            }];
            let h = self.m.next_rx(&space)?;
            let mut payload = vec![0u8; h.len as usize];
            self.mem.access().unwrap().read(RX, &mut payload).unwrap();
            Some((h, payload))
        }

        /// The guest sends `bytes`.
        fn send(&mut self, bytes: &[u8]) {
            self.mem.access().unwrap().write(TX, bytes).unwrap();
            let span = [Span {
                gpa: TX,
                ptr: self.mem.host_ptr(TX, bytes.len()).unwrap(),
                len: bytes.len(),
            }];
            let h = header(op::RW, bytes.len() as u32, 0);
            self.m.on_guest_packet(&h, &span, &self.mem);
            self.fwd += bytes.len() as u32;
        }

        fn shutdown(&mut self, flags: u32) {
            self.m
                .on_guest_packet(&header(op::SHUTDOWN, 0, flags), &[], &self.mem);
        }

        fn word(&self, at: u64) -> &'static AtomicU64 {
            self.word_of(SLOT, at)
        }

        fn word_of(&self, slot: u64, at: u64) -> &'static AtomicU64 {
            // SAFETY: an aligned u64 of the test's guest memory, which outlives its uses.
            unsafe { AtomicU64::from_ptr(self.mem.host_ptr(slot + at, 8).unwrap().cast()) }
        }

        fn flag(&self, at: u64) -> &'static AtomicU32 {
            self.flag_of(SLOT, at)
        }

        fn flag_of(&self, slot: u64, at: u64) -> &'static AtomicU32 {
            // SAFETY: as in `word_of`.
            unsafe { AtomicU32::from_ptr(self.mem.host_ptr(slot + at, 4).unwrap().cast()) }
        }

        /// The relay puts `bytes` in the ring for the host from `head`.
        fn put(&self, head: u64, bytes: &[u8]) {
            self.put_in(SLOT, head, bytes);
        }

        fn put_in(&self, slot: u64, head: u64, bytes: &[u8]) {
            for (i, b) in bytes.iter().enumerate() {
                let at = abi::CONTROL + (head + i as u64) % u64::from(RING);
                self.mem.access().unwrap().write(slot + at, &[*b]).unwrap();
            }
            self.word_of(slot, field::TO_HOST_HEAD)
                .store(head + bytes.len() as u64, Ordering::Release);
        }

        /// The relay takes what the ring for the guest holds from `tail`.
        fn take(&self, tail: u64) -> Vec<u8> {
            self.take_from(SLOT, tail)
        }

        fn take_from(&self, slot: u64, tail: u64) -> Vec<u8> {
            let head = self.word_of(slot, field::TO_GUEST_HEAD).load(Ordering::Acquire);
            let ring = u64::from(RING);
            let mut out = vec![0u8; (head - tail) as usize];
            for (i, b) in out.iter_mut().enumerate() {
                let at = abi::CONTROL + ring + (tail + i as u64) % ring;
                let mut one = [0u8];
                self.mem.access().unwrap().read(slot + at, &mut one).unwrap();
                *b = one[0];
            }
            self.word_of(slot, field::TO_GUEST_TAIL)
                .store(head, Ordering::Release);
            out
        }

        /// The next packet, which must be one control byte.
        fn control(&mut self) -> u8 {
            let (h, said) = self.next().expect("a packet");
            assert_eq!((h.op, said.len()), (op::RW, 1), "{said:?}");
            said[0]
        }

        /// The next packet, which must be a reply.
        fn reply(&mut self) -> Reply {
            let (h, said) = self.next().expect("a packet");
            assert_eq!((h.op, h.len), (op::RW, 8));
            Reply::parse(said.try_into().unwrap()).unwrap()
        }

        /// The owner offers `offer` and cues the device, which answers.
        fn offer(&mut self, offer: Offer) -> Reply {
            self.handle.offer(offer);
            (&self.owner).write_all(&[CUE]).unwrap();
            self.turn();
            let (h, said) = self.next().unwrap();
            assert_eq!((h.op, h.len), (op::RW, 8));
            Reply::parse(said.try_into().unwrap()).unwrap()
        }

        fn read_owner(&self, want: usize) -> Vec<u8> {
            let mut got = vec![0u8; want];
            let mut at = 0;
            while at < want {
                match (&self.owner).read(&mut got[at..]) {
                    Ok(0) => break,
                    Ok(n) => at += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => panic!("{at} of {want} bytes came"),
                    Err(e) => panic!("{e}"),
                }
            }
            got.truncate(at);
            got
        }
    }

    #[test]
    fn an_attached_connections_bytes_cross_through_its_ring_and_its_doorbells() {
        let mut g = Guest::connect();
        assert_eq!(g.offer(whole(SLOT)), Reply::Attached);
        assert!(g.mem.pinned(SLOT, abi::slot_len(RING)));
        // Guest to host: the relay's bytes, then its doorbell, reach the owner's socket.
        let up: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
        g.put(0, &up);
        g.send(&[abi::DOORBELL]);
        assert_eq!(g.read_owner(up.len()), up);
        assert_eq!(
            g.word(field::TO_HOST_TAIL).load(Ordering::Acquire),
            up.len() as u64
        );
        // Host to guest: what the owner writes fills the ring for the guest, and a relay
        // that waits is rung once, with one doorbell byte.
        g.flag(field::GUEST_WAITS_DATA).store(1, Ordering::SeqCst);
        (&g.owner).write_all(b"the engine's answer").unwrap();
        g.turn();
        assert_eq!(g.take(0), b"the engine's answer");
        let (h, said) = g.next().unwrap();
        assert_eq!((h.op, said), (op::RW, vec![abi::DOORBELL]));
        assert!(g.next().is_none(), "one doorbell for one wait");
        // The device found the ring for the host empty, and says it waits.
        assert_eq!(g.flag(field::HOST_WAITS_DATA).load(Ordering::SeqCst), 1);
    }

    /// The relay rings a side only where that side said it waits (Dekker's handshake), so a
    /// device that attaches without saying it waits for the relay's first bytes is never
    /// told of them, and a connection's first request never reaches the host.
    #[test]
    fn a_relays_first_bytes_reach_the_host_though_it_rings_only_a_waiting_device() {
        let mut g = Guest::connect();
        assert_eq!(g.offer(whole(SLOT)), Reply::Attached);
        let request = b"GET /_ping HTTP/1.1\r\nHost: api\r\n\r\n";
        g.put(0, request);
        // As the relay does: a doorbell only where the device said it waits.
        if g.flag(field::HOST_WAITS_DATA).swap(0, Ordering::SeqCst) != 0 {
            g.send(&[abi::DOORBELL]);
        }
        g.turn();
        assert_eq!(g.read_owner(request.len()), request);
    }

    #[test]
    fn a_refused_offer_leaves_the_connection_plain() {
        let mut g = Guest::connect();
        assert_eq!(g.offer(whole(64 << 20)), Reply::Refused(Refusal::NotRam));
        g.send(b"plain bytes");
        assert_eq!(g.read_owner(11), b"plain bytes");
        (&g.owner).write_all(b"plain answer").unwrap();
        g.turn();
        let (h, said) = g.next().unwrap();
        assert_eq!((h.op, said), (op::RW, b"plain answer".to_vec()));
    }

    #[test]
    fn an_offer_the_owner_could_not_read_is_answered_refused() {
        let mut g = Guest::connect();
        g.handle.refuse(Refusal::Malformed);
        (&g.owner).write_all(&[CUE]).unwrap();
        g.turn();
        let (h, said) = g.next().unwrap();
        assert_eq!(h.op, op::RW);
        assert_eq!(
            Reply::parse(said.try_into().unwrap()),
            Some(Reply::Refused(Refusal::Malformed))
        );
        g.send(b"plain after");
        assert_eq!(g.read_owner(11), b"plain after");
    }

    #[test]
    fn a_connection_whose_owner_offers_nothing_is_plain() {
        let mut g = Guest::connect();
        (&g.owner).write_all(b"no ring").unwrap();
        g.turn();
        let (h, said) = g.next().unwrap();
        assert_eq!((h.op, said), (op::RW, b"no ring".to_vec()));
    }

    #[test]
    fn a_byte_that_is_no_doorbell_resets_the_connection_after_its_detach() {
        let mut g = Guest::connect();
        assert_eq!(g.offer(whole(SLOT)), Reply::Attached);
        g.send(b"x");
        // Let go at once, before anything has gone or the guest gave a buffer for it; then
        // its DETACH, so that the relay may free the slot, then the RST.
        assert!(!g.mem.pinned(SLOT, abi::slot_len(RING)));
        assert_eq!(g.control(), abi::DETACH);
        let (h, _) = g.next().unwrap();
        assert_eq!(h.op, op::RST);
    }

    /// The guard of the slot's freeing (D230): the device's DETACH is said only after the
    /// ring's pins are gone and nothing more of the slot is written; then plain vsock.
    #[test]
    fn a_detach_is_answered_once_the_pins_are_gone_and_plain_vsock_follows() {
        let mut g = Guest::connect();
        assert_eq!(g.offer(whole(SLOT)), Reply::Attached);
        // What the relay put and the device passed on, and what the host had sent.
        g.put(0, b"asked");
        g.send(&[abi::DOORBELL]);
        assert_eq!(g.read_owner(5), b"asked");
        (&g.owner).write_all(b"answered").unwrap();
        g.turn();
        g.send(&[abi::DETACH]);
        assert!(!g.mem.pinned(SLOT, abi::slot_len(RING)), "pinned as DETACH came");
        let head = g.word(field::TO_GUEST_HEAD).load(Ordering::Acquire);
        assert_eq!(g.control(), abi::DETACH);
        // Its last positions stay: nothing more of the slot is written.
        (&g.owner).write_all(b" more").unwrap();
        g.turn();
        assert_eq!(g.word(field::TO_GUEST_HEAD).load(Ordering::Acquire), head);
        assert_eq!(g.take(0), b"answered");
        // Plain vsock both ways from here.
        g.send(b"plain up");
        assert_eq!(g.read_owner(8), b"plain up");
        let (h, said) = g.next().unwrap();
        assert_eq!((h.op, said), (op::RW, b" more".to_vec()));
    }

    /// The host's end lets the ring go: the device's DETACH, then, once the relay has said
    /// its own (with what the ring for the host still held, as plain bytes after it), the
    /// host's end as vsock's shutdown. What the relay said meanwhile is not the host's.
    #[test]
    fn the_hosts_end_lets_the_ring_go_and_its_shutdown_waits_for_the_relays_detach() {
        let mut g = Guest::connect();
        assert_eq!(g.offer(whole(SLOT)), Reply::Attached);
        (&g.owner).write_all(b"goodbye").unwrap();
        g.owner.shutdown(std::net::Shutdown::Write).unwrap();
        g.turn();
        g.turn();
        assert!(!g.mem.pinned(SLOT, abi::slot_len(RING)));
        assert_eq!(g.control(), abi::DETACH);
        assert_eq!(g.take(0), b"goodbye");
        assert!(g.next().is_none(), "no shutdown before the relay's DETACH");
        // The relay's doorbell and end, sent before it heard: control bytes still.
        g.send(&[abi::DOORBELL, abi::END]);
        assert!(g.next().is_none());
        // Its DETACH, then the bytes the device had not taken from the ring, plain.
        g.send(&[abi::DETACH, b'l', b'a', b's', b't']);
        assert_eq!(g.read_owner(4), b"last");
        let (h, _) = g.next().unwrap();
        assert_eq!((h.op, h.flags), (op::SHUTDOWN, shutdown::SEND));
    }

    /// A swap: the old slot let go and said so, then the new one attached, pinned, waiting
    /// for the relay's first bytes, and answered.
    #[test]
    fn a_swap_lets_the_old_slot_go_and_takes_the_new_one() {
        const NEW: u64 = 4 << 20;
        let mut g = Guest::connect();
        assert_eq!(g.offer(whole(SLOT)), Reply::Attached);
        let swap = [&[abi::SWAP][..], &whole(NEW).encode()].concat();
        g.send(&swap);
        assert!(!g.mem.pinned(SLOT, abi::slot_len(RING)));
        assert!(g.mem.pinned(NEW, abi::slot_len(RING)));
        assert_eq!(g.control(), abi::DETACH);
        assert_eq!(g.reply(), Reply::Attached);
        assert_eq!(g.flag_of(NEW, field::HOST_WAITS_DATA).load(Ordering::SeqCst), 1);
        // Bytes cross through the new slot, rung as the relay rings.
        g.put_in(NEW, 0, b"through the new ring");
        if g.flag_of(NEW, field::HOST_WAITS_DATA).swap(0, Ordering::SeqCst) != 0 {
            g.send(&[abi::DOORBELL]);
        }
        assert_eq!(g.read_owner(20), b"through the new ring");
        (&g.owner).write_all(b"back").unwrap();
        g.turn();
        assert_eq!(g.take_from(NEW, 0), b"back");
    }

    /// A swap's offer may come in pieces: it is answered once whole.
    #[test]
    fn a_swaps_offer_in_pieces_is_answered_once_whole() {
        const NEW: u64 = 4 << 20;
        let mut g = Guest::connect();
        assert_eq!(g.offer(whole(SLOT)), Reply::Attached);
        let offer = whole(NEW).encode();
        g.send(&[abi::SWAP, offer[0], offer[1]]);
        assert_eq!(g.control(), abi::DETACH);
        assert!(g.next().is_none(), "no answer before the offer is whole");
        g.send(&offer[2..20]);
        g.send(&offer[20..]);
        assert_eq!(g.reply(), Reply::Attached);
        assert!(g.mem.pinned(NEW, abi::slot_len(RING)));
    }

    /// A swap that crosses the host's end is refused, Ended, and the end follows the answer.
    #[test]
    fn a_swap_crossing_the_hosts_end_is_refused_and_the_end_follows() {
        const NEW: u64 = 4 << 20;
        let mut g = Guest::connect();
        assert_eq!(g.offer(whole(SLOT)), Reply::Attached);
        g.owner.shutdown(std::net::Shutdown::Write).unwrap();
        g.turn();
        let swap = [&[abi::SWAP][..], &whole(NEW).encode()].concat();
        g.send(&swap);
        assert_eq!(g.control(), abi::DETACH);
        assert_eq!(g.reply(), Reply::Refused(Refusal::Ended));
        assert!(!g.mem.pinned(NEW, abi::slot_len(RING)));
        let (h, _) = g.next().unwrap();
        assert_eq!((h.op, h.flags), (op::SHUTDOWN, shutdown::SEND));
    }

    /// Bytes past a swap's offer, which the guest may not send before its answer, end the
    /// connection: after the device's DETACH, as every end it makes.
    #[test]
    fn bytes_past_a_swaps_offer_end_the_connection_after_its_detach() {
        const NEW: u64 = 4 << 20;
        let mut g = Guest::connect();
        assert_eq!(g.offer(whole(SLOT)), Reply::Attached);
        let swap = [&[abi::SWAP][..], &whole(NEW).encode(), &[abi::DOORBELL]].concat();
        g.send(&swap);
        assert!(!g.mem.pinned(SLOT, abi::slot_len(RING)));
        assert_eq!(g.control(), abi::DETACH);
        let (h, _) = g.next().unwrap();
        assert_eq!(h.op, op::RST);
    }

    /// A DETACH with bytes after it in its packet breaks the rule that the guest waits for
    /// the device's DETACH before its plain bytes (they would pass what the ring held).
    #[test]
    fn bytes_after_a_relays_detach_before_the_devices_end_the_connection() {
        let mut g = Guest::connect();
        assert_eq!(g.offer(whole(SLOT)), Reply::Attached);
        g.send(&[abi::DETACH, b'x']);
        assert_eq!(g.control(), abi::DETACH);
        let (h, _) = g.next().unwrap();
        assert_eq!(h.op, op::RST);
        assert!(g.read_owner(0).is_empty());
    }

    #[test]
    fn each_side_ends_its_writes_after_its_last_bytes() {
        let mut g = Guest::connect();
        assert_eq!(g.offer(whole(SLOT)), Reply::Attached);
        // The relay's last bytes, then its end: the owner reads them, then end-of-stream.
        g.put(0, b"last words");
        g.shutdown(shutdown::SEND);
        let mut got = Vec::new();
        g.owner.set_nonblocking(false).unwrap();
        (&g.owner).read_to_end(&mut got).unwrap();
        assert_eq!(got, b"last words");
        g.owner.set_nonblocking(true).unwrap();
        // The host's last bytes, then its end: in the ring before the guest is told, which
        // the device's DETACH does; a guest that has ended its sends says no DETACH of its
        // own, and the shutdown follows at once.
        (&g.owner).write_all(b"goodbye").unwrap();
        g.owner.shutdown(std::net::Shutdown::Write).unwrap();
        g.turn();
        g.turn();
        assert_eq!(g.control(), abi::DETACH);
        assert_eq!(g.take(0), b"goodbye");
        let (h, _) = g.next().unwrap();
        assert_eq!((h.op, h.flags), (op::SHUTDOWN, shutdown::SEND));
    }

    #[test]
    fn a_clients_end_reaches_the_host_after_its_last_bytes_and_doorbells_go_on() {
        let mut g = Guest::connect();
        assert_eq!(g.offer(whole(SLOT)), Reply::Attached);
        g.put(0, b"request body");
        g.send(&[abi::END]);
        g.owner.set_nonblocking(false).unwrap();
        let mut got = Vec::new();
        (&g.owner).read_to_end(&mut got).unwrap();
        assert_eq!(got, b"request body");
        g.owner.set_nonblocking(true).unwrap();
        // The answer still comes, and the relay still rings for it.
        (&g.owner).write_all(b"answer").unwrap();
        g.turn();
        assert_eq!(g.take(0), b"answer");
        g.send(&[abi::DOORBELL]);
        assert!(
            g.next().is_none(),
            "no reset: a doorbell after the end is the relay's right"
        );
    }

    #[test]
    fn a_full_ring_stops_the_host_socket_until_the_guest_rings() {
        let mut g = Guest::connect();
        assert_eq!(g.offer(whole(SLOT)), Reply::Attached);
        let big: Vec<u8> = (0..(RING as usize + 5000)).map(|i| (i % 251) as u8).collect();
        let mut written = 0;
        while written < big.len() {
            match (&g.owner).write(&big[written..]) {
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => g.turn(),
                Err(e) => panic!("{e}"),
            }
        }
        g.turn();
        g.turn();
        assert_eq!(
            g.word(field::TO_GUEST_HEAD).load(Ordering::Acquire),
            u64::from(RING)
        );
        assert_eq!(g.flag(field::HOST_WAITS_ROOM).load(Ordering::SeqCst), 1);
        let mut got = g.take(0);
        // Room made, the relay rings, and the rest comes.
        g.send(&[abi::DOORBELL]);
        g.turn();
        got.extend(g.take(u64::from(RING)));
        assert_eq!(got, big);
    }
}
