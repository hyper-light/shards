//! A connection whose bytes go through an engine ring (D230): what its owner's cue turns
//! it into, and how it moves its bytes once it is.
//!
//! Its states (R04): `Plain`, vsock as ever; `Ringable`, a ring port's connection whose
//! owner may yet offer one; `Refused`, its reply owed the guest, then `Plain`; `Ringed`,
//! attached, its reply owed until sent, every byte the guest sends after a doorbell, every
//! byte the host sends into the ring for the guest. The host socket's reads fill that ring
//! as the socket is readable, not as the guest's buffers come, so a turn that rings no
//! doorbell takes none of them; the guest's doorbells, and the socket's room, drain the
//! ring for the host into the socket. Each ring direction moves at most a ring's length a
//! turn, so that one connection's stream leaves the device's other connections their
//! turns, as a packet's 64 KiB at most does today.

use std::io::{self, Read as _};

use shards_abi::ring::{self as abi, Reply};

use super::super::Span;
use super::super::packet::{Header, op, shutdown};
use super::super::ring::{Broken, CUE, Ring, RingHandle};
use super::{Conn, Phase};
use crate::debug;
use crate::memory::GuestMemory;

/// Where a connection stands with a ring.
pub(super) enum Ringing {
    /// Plain vsock: not a ring port's connection, or its chance at a ring has passed.
    Plain,
    /// A ring port's connection whose owner may yet offer one.
    Ringable(RingHandle),
    /// Its offer refused: the reply owed, plain vsock after.
    Refused(Reply, RingHandle),
    /// Attached.
    Ringed(Box<Ringed>),
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

/// What a turn of the host socket into the ring for the guest came to.
enum Filled {
    /// As much as there was, or a turn's worth.
    Moved,
    /// The host socket's end.
    End,
}

impl Conn {
    /// Makes this a ring port's connection, whose owner may offer a ring through `handle`.
    pub fn ringable(&mut self, handle: RingHandle) {
        self.ringing = Ringing::Ringable(handle);
    }

    /// Whether a ring's reply or doorbell is owed the guest now.
    pub(super) fn ring_owes(&self) -> bool {
        match &self.ringing {
            Ringing::Refused(..) => true,
            Ringing::Ringed(r) => !r.replied || (r.doorbell && self.guest_credit() > 0),
            Ringing::Plain | Ringing::Ringable(_) => false,
        }
    }

    /// The host socket's events an attached connection waits for, where it is one.
    pub(super) fn ring_interest(&self) -> Option<(bool, bool)> {
        let Ringing::Ringed(r) = &self.ringing else {
            return None;
        };
        let read = !self.killed
            && self.phase == Phase::Established
            && !self.host_eof
            && !self.guest_rcv_closed
            && !r.full;
        Some((read, !self.killed && r.blocked))
    }

    /// The host socket is readable: the owner's cue attaches a ring, or its bytes fill the
    /// one attached. Whether it was a ring's to handle.
    pub(super) fn ring_on_readable(&mut self) -> bool {
        if self.killed || self.phase != Phase::Established {
            return false;
        }
        match &self.ringing {
            Ringing::Plain | Ringing::Refused(..) => false,
            Ringing::Ringable(handle) => {
                let handle = handle.clone();
                self.cue(handle)
            }
            Ringing::Ringed(_) => {
                match self.fill() {
                    Ok(Filled::Moved) => {}
                    Ok(Filled::End) => {
                        // The guest drains the ring as it reads the end, which tells it all.
                        self.host_eof = true;
                        if let Ringing::Ringed(r) = &mut self.ringing {
                            r.doorbell = false;
                        }
                        self.pending.shutdown |= shutdown::SEND;
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
            Ok(ring) => Ringing::Ringed(Box::new(Ringed {
                ring,
                replied: false,
                doorbell: false,
                blocked: false,
                full: false,
                ended: false,
            })),
            Err(refusal) => {
                debug!("vsock: port {}: a ring refused: {refusal:?}", self.local_port);
                Ringing::Refused(Reply::Refused(refusal), handle)
            }
        };
        true
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
    /// is attached.
    pub(super) fn ring_drained(&mut self) -> io::Result<bool> {
        let Ringing::Ringed(r) = &mut self.ringing else {
            return Ok(true);
        };
        let send_closed = self.guest_send_closed || r.ended;
        let quantum = r.ring.size();
        let mut moved = 0usize;
        let drained = loop {
            let pending = r.ring.pending_to_host().map_err(broken)?;
            if pending == 0 {
                // After the guest's shutdown nothing more comes; before it, the device
                // says it waits, and looks once more.
                if send_closed || !r.ring.wait_for_data().map_err(broken)? {
                    break true;
                }
                continue;
            }
            let turn = r.ring.to_host(&self.stream).map_err(broken)?;
            r.doorbell |= turn.ring;
            moved = moved.saturating_add(turn.moved);
            if turn.moved == 0 || moved >= quantum {
                break false;
            }
        };
        r.blocked = !drained;
        // The guest's client ended its writes: they end at the host too, after its last.
        if drained && r.ended && !self.host_write_shut {
            self.host_write_shut = true;
            let _ = self.stream.shutdown(std::net::Shutdown::Write);
        }
        Ok(drained)
    }

    /// What the guest sent on an attached connection: doorbells, and once its client's
    /// end; any other byte a broken rule. Whether it was a ring's to handle.
    pub(super) fn ring_doorbells(&mut self, h: &Header, payload: &[Span], mem: &GuestMemory) -> bool {
        let Ringing::Ringed(r) = &mut self.ringing else {
            return false;
        };
        // The guest has rung: room may have come in the ring for the guest.
        r.full = false;
        match signals(payload, mem) {
            Some(ended) => r.ended |= ended,
            None => {
                debug!("vsock: port {}: a byte that is no doorbell", self.local_port);
                self.kill();
                return true;
            }
        }
        self.fwd_cnt += h.len;
        if self.guest_needs_credit() {
            self.pending.credit_update = true;
        }
        if let Err(e) = self.flush() {
            debug!("vsock: host socket for port {}: {e}", self.local_port);
            self.kill();
        }
        true
    }

    /// A ring's packet owed the guest, written into `space`: its reply, then doorbells.
    pub(super) fn ring_next_rx(&mut self, space: &[Span]) -> Option<Header> {
        let credit = self.guest_credit();
        let (bytes, memory) = match &mut self.ringing {
            Ringing::Refused(reply, handle) => (reply.encode().to_vec(), handle.memory().clone()),
            Ringing::Ringed(r) if !r.replied => {
                r.replied = true;
                (Reply::Attached.encode().to_vec(), r.ring.memory().clone())
            }
            Ringing::Ringed(r) if r.doorbell && credit > 0 => {
                r.doorbell = false;
                (vec![abi::DOORBELL], r.ring.memory().clone())
            }
            _ => return None,
        };
        if matches!(self.ringing, Ringing::Refused(..)) {
            self.ringing = Ringing::Plain;
        }
        if !put(&memory, space, &bytes) {
            debug!("vsock: port {}: an RX buffer too short for a ring's packet", self.local_port);
            self.kill();
            self.pending = Default::default();
            return Some(self.header(op::RST));
        }
        let len = bytes.len() as u32;
        self.rx_cnt += len;
        let mut h = self.header(op::RW);
        h.len = len;
        Some(h)
    }

    /// Ends a connection whose ring broke.
    fn broken(&mut self, e: Broken) {
        debug!("vsock: port {}: its ring: {e:?}", self.local_port);
        self.kill();
    }
}

fn broken(e: Broken) -> io::Error {
    io::Error::other(format!("its ring: {e:?}"))
}

/// What the guest's bytes say, where each is a doorbell or its client's end: whether one is
/// its end. None where any is neither.
fn signals(payload: &[Span], mem: &GuestMemory) -> Option<bool> {
    let access = mem.access().ok()?;
    let mut ended = false;
    let mut chunk = [0u8; 64];
    for s in payload {
        let mut at = 0usize;
        while at < s.len {
            let n = (s.len - at).min(chunk.len());
            let buf = chunk.get_mut(..n)?;
            access.read(s.gpa + at as u64, buf).ok()?;
            for &b in buf.iter() {
                match b {
                    abi::DOORBELL => {}
                    abi::END => ended = true,
                    _ => return None,
                }
            }
            at += n;
        }
    }
    Some(ended)
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
            self.m.on_guest_packet(&header(op::SHUTDOWN, 0, flags), &[], &self.mem);
        }

        fn word(&self, at: u64) -> &'static AtomicU64 {
            // SAFETY: an aligned u64 of the test's guest memory, which outlives its uses.
            unsafe { AtomicU64::from_ptr(self.mem.host_ptr(SLOT + at, 8).unwrap().cast()) }
        }

        fn flag(&self, at: u64) -> &'static AtomicU32 {
            // SAFETY: as in `word`.
            unsafe { AtomicU32::from_ptr(self.mem.host_ptr(SLOT + at, 4).unwrap().cast()) }
        }

        /// The relay puts `bytes` in the ring for the host from `head`.
        fn put(&self, head: u64, bytes: &[u8]) {
            for (i, b) in bytes.iter().enumerate() {
                let at = abi::CONTROL + (head + i as u64) % u64::from(RING);
                self.mem.access().unwrap().write(SLOT + at, &[*b]).unwrap();
            }
            self.word(field::TO_HOST_HEAD)
                .store(head + bytes.len() as u64, Ordering::Release);
        }

        /// The relay takes what the ring for the guest holds from `tail`.
        fn take(&self, tail: u64) -> Vec<u8> {
            let head = self.word(field::TO_GUEST_HEAD).load(Ordering::Acquire);
            let ring = u64::from(RING);
            let mut out = vec![0u8; (head - tail) as usize];
            for (i, b) in out.iter_mut().enumerate() {
                let at = abi::CONTROL + ring + (tail + i as u64) % ring;
                let mut one = [0u8];
                self.mem.access().unwrap().read(SLOT + at, &mut one).unwrap();
                *b = one[0];
            }
            self.word(field::TO_GUEST_TAIL).store(head, Ordering::Release);
            out
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
        assert_eq!(g.word(field::TO_HOST_TAIL).load(Ordering::Acquire), up.len() as u64);
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
    fn a_byte_that_is_no_doorbell_resets_the_connection_and_lets_its_ring_go() {
        let mut g = Guest::connect();
        assert_eq!(g.offer(whole(SLOT)), Reply::Attached);
        g.send(b"x");
        // Let go as it is killed, before its RST has gone or the guest gave a buffer for it.
        assert!(!g.mem.pinned(SLOT, abi::slot_len(RING)));
        let (h, _) = g.next().unwrap();
        assert_eq!(h.op, op::RST);
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
        // The host's last bytes, then its end: in the ring before the guest is told.
        (&g.owner).write_all(b"goodbye").unwrap();
        g.owner.shutdown(std::net::Shutdown::Write).unwrap();
        g.turn();
        g.turn();
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
        assert!(g.next().is_none(), "no reset: a doorbell after the end is the relay's right");
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
        assert_eq!(g.word(field::TO_GUEST_HEAD).load(Ordering::Acquire), u64::from(RING));
        assert_eq!(g.flag(field::HOST_WAITS_ROOM).load(Ordering::SeqCst), 1);
        let mut got = g.take(0);
        // Room made, the relay rings, and the rest comes.
        g.send(&[abi::DOORBELL]);
        g.turn();
        got.extend(g.take(u64::from(RING)));
        assert_eq!(got, big);
    }
}
