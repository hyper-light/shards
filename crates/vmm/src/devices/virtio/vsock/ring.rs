//! An engine ring attached to a vsock connection (D230): the device's end of the rings a
//! guest's relay offered in its hello, in the guest's RAM, whose bytes the device moves to
//! and from the connection's host socket by `writev` and `readv`, as it moves packets.
//!
//! The guest is hostile. Its offer is checked against guest RAM and pinned
//! ([`GuestMemory::pin`]: neither unplugged nor given back while attached); its positions
//! are read once a turn, with acquire, and believed only where they could be; offsets are
//! positions modulo the ring, so nothing it writes reaches past the slot's pages; the
//! rings' bytes are moved by system calls and never parsed here. No Rust reference ever
//! covers ring memory: the control page's words are reached as atomics, the rings by the
//! kernel.

use std::io;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};
use std::sync::{Arc, Mutex};

use shards_abi::ring::{self as abi, Offer, Refusal, field};

use super::{Span, readv, writev};
use crate::memory::GuestMemory;
use crate::platform::UnixStream;

/// What a ring port's owner offers a connection's ring through (D230), given with the
/// connection ([`crate::vm::ServedPort::Ringed`]). The owner reads the guest's hello,
/// checks its token, then [`offer`](RingHandle::offer)s what it offered and writes one byte
/// of its own, [`CUE`], before anything else of the connection: the device takes that byte
/// as its cue, attaches the ring or refuses it, and answers the guest
/// ([`shards_abi::ring::Reply`]) before anything the host sends after.
#[derive(Clone)]
pub struct RingHandle {
    memory: Arc<GuestMemory>,
    offer: Arc<Mutex<Option<Offered>>>,
}

/// What the owner made of the guest's offer: the slot it names, or why its bytes name none.
pub(super) type Offered = Result<Offer, Refusal>;

impl std::fmt::Debug for RingHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RingHandle").finish_non_exhaustive()
    }
}

/// The owner's cue: the byte it writes after its offer, which the device takes and drops.
pub const CUE: u8 = 0;

impl RingHandle {
    /// A connection's handle, whose rings are attached in `memory`: the muxer's, or a test's.
    pub fn new(memory: Arc<GuestMemory>) -> RingHandle {
        RingHandle {
            memory,
            offer: Arc::new(Mutex::new(None)),
        }
    }

    /// Has the device attach `offer` as [`CUE`], which the owner writes next, reaches it.
    pub fn offer(&self, offer: Offer) {
        self.put(Ok(offer));
    }

    /// Has the device refuse the guest's offer, for `why`, as [`CUE`] reaches it: an offer
    /// whose bytes the owner could not read as one is answered all the same, so that the
    /// guest's relay, which waits for an answer, has one.
    pub fn refuse(&self, why: Refusal) {
        self.put(Err(why));
    }

    pub(super) fn put(&self, offered: Offered) {
        *crate::sync::lock(&self.offer) = Some(offered);
    }

    /// What the owner offered, once: the device's, as the cue reaches it.
    pub(super) fn take(&self) -> Option<Offered> {
        crate::sync::lock(&self.offer).take()
    }

    pub(super) fn memory(&self) -> &Arc<GuestMemory> {
        &self.memory
    }
}

/// The most iovecs one `readv` or `writev` takes ([`abi::MOST_IOVECS`]): a turn of the
/// largest ring, its pages each apart, needs 513, so every turn is one call
/// ([`abi::MAX_RING`]'s derivation).
const MOST_SPANS: usize = abi::MOST_IOVECS as usize;

/// Why a ring's peer cannot be believed, or its host socket failed: the connection ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Broken {
    /// The guest's head of the ring for the host is behind what the device took of it.
    HeadBehind,
    /// It is more than a ring ahead of it.
    HeadAhead,
    /// The guest's tail of the ring for the guest is past what the device put in it.
    TailAhead,
    /// It went back.
    TailBehind,
    /// The host socket failed.
    Socket(io::ErrorKind),
}

/// What a turn of one direction did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Turn {
    /// Bytes moved.
    pub moved: usize,
    /// The guest waits on a doorbell, which the turn's caller owes it: its flag was set and
    /// is cleared.
    pub ring: bool,
    /// The host socket's end was read (from the host only).
    pub eof: bool,
}

/// A run of one ring's bytes: where in the ring, at what guest address, mapped where here.
#[derive(Debug, Clone, Copy)]
struct Run {
    at: u64,
    gpa: u64,
    ptr: *mut u8,
    len: u64,
}

/// One direction's ring: its runs in the ring's order.
#[derive(Debug)]
struct Pages {
    runs: Vec<Run>,
}

impl Pages {
    /// The spans of ring bytes `from..from + len`, `from` a ring offset, wrapping at the
    /// ring's end, at most [`MOST_SPANS`] of them.
    fn spans(&self, size: u64, from: u64, len: u64) -> Vec<Span> {
        let mut out = Vec::new();
        let (first, second) = (len.min(size - from), len.saturating_sub(size - from));
        for (start, n) in [(from, first), (0, second)] {
            let end = start + n;
            for r in &self.runs {
                let (lo, hi) = (r.at.max(start), (r.at + r.len).min(end));
                if lo >= hi || out.len() == MOST_SPANS {
                    continue;
                }
                let skip = lo - r.at;
                out.push(Span {
                    gpa: r.gpa + skip,
                    // SAFETY: `skip` lies within the run, which lies within its pinned
                    // extent's host mapping.
                    ptr: unsafe { r.ptr.add(skip as usize) },
                    len: (hi - lo) as usize,
                });
            }
        }
        out
    }
}

/// A connection's attached rings.
#[derive(Debug)]
pub struct Ring {
    memory: Arc<GuestMemory>,
    /// The control page's host address, within the slot's first, pinned, extent.
    control: *mut u8,
    control_gpa: u64,
    size: u64,
    to_host: Pages,
    to_guest: Pages,
    /// Bytes of the ring for the host the device has passed on: its own, of which the
    /// control page's tail is a copy.
    taken: u64,
    /// Bytes the device has put in the ring for the guest: its own, of which the control
    /// page's head is a copy.
    given: u64,
    /// The guest's tail of the ring for the guest as last believed.
    freed: u64,
    /// The slot's extents, pinned while attached.
    pinned: Vec<(u64, u64)>,
}

// SAFETY: the raw pointers are into guest memory this ring holds pinned and an `Arc` of;
// the ring is used by one thread at a time (the device's worker), and reaches that memory
// only through atomics and system calls.
unsafe impl Send for Ring {}

impl Ring {
    /// Attaches the slot `offer` names: its extents all guest RAM (or memory plugged), none
    /// pinned already (another connection's ring, a device's DMA), each then pinned. Its
    /// own positions start where the control page's are, which the relay zeroed.
    pub fn attach(memory: &Arc<GuestMemory>, offer: &Offer) -> Result<Ring, Refusal> {
        let size = u64::from(offer.ring);
        // No page named twice: two of the slot's places would be one.
        let mut sorted: Vec<(u64, u64)> = offer.extents.iter().map(|e| (e.gpa, e.len)).collect();
        sorted.sort_unstable();
        if sorted
            .windows(2)
            .any(|w| matches!(w, [(a, n), (b, _)] if a.saturating_add(*n) > *b))
        {
            return Err(Refusal::Malformed);
        }
        let mut at = 0u64;
        let (mut control, mut to_host, mut to_guest) = (None, Vec::new(), Vec::new());
        for e in &offer.extents {
            if memory.pinned(e.gpa, e.len) {
                return Err(Refusal::InUse);
            }
            let len = usize::try_from(e.len).map_err(|_| Refusal::NotRam)?;
            let host = memory.host_ptr(e.gpa, len).map_err(|_| Refusal::NotRam)?;
            // The slot's bytes this extent holds, cut at the control page's and the rings'
            // boundaries.
            let end = at.checked_add(e.len).ok_or(Refusal::Malformed)?;
            let mut piece = at;
            while piece < end {
                let (region_start, region_end) = if piece < abi::CONTROL {
                    (0, abi::CONTROL)
                } else if piece < abi::CONTROL + size {
                    (abi::CONTROL, abi::CONTROL + size)
                } else {
                    (abi::CONTROL + size, abi::CONTROL + 2 * size)
                };
                let piece_end = end.min(region_end);
                let skip = piece - at;
                let run = Run {
                    at: piece - region_start,
                    gpa: e.gpa + skip,
                    // SAFETY: `skip` lies within the extent `host_ptr` mapped whole.
                    ptr: unsafe { host.add(skip as usize) },
                    len: piece_end - piece,
                };
                match region_start {
                    0 => {
                        // The control page lies in one extent: its first page.
                        if run.at != 0 || run.len != abi::CONTROL {
                            return Err(Refusal::Malformed);
                        }
                        control = Some((run.ptr, run.gpa));
                    }
                    s if s == abi::CONTROL => to_host.push(run),
                    _ => to_guest.push(run),
                }
                piece = piece_end;
            }
            at = end;
        }
        let (control, control_gpa) = control.ok_or(Refusal::Malformed)?;
        if at != abi::slot_len(offer.ring) {
            return Err(Refusal::Malformed);
        }
        let pinned: Vec<(u64, u64)> = offer.extents.iter().map(|e| (e.gpa, e.len)).collect();
        for &(gpa, len) in &pinned {
            memory.pin(gpa, len);
        }
        let ring = Ring {
            memory: memory.clone(),
            control,
            control_gpa,
            size,
            to_host: Pages { runs: to_host },
            to_guest: Pages { runs: to_guest },
            taken: 0,
            given: 0,
            freed: 0,
            pinned,
        };
        // Its own positions are where the relay's offer left the page: at its start. And it
        // waits for the relay's first bytes: the relay rings only a side that says it waits
        // (Dekker's handshake), and nothing else has the device look at the ring for the
        // host before a doorbell, so a device that said nothing never heard the first
        // request (logs/r1-red-first-request.txt of docs/research/measurements/ring-pins-v1).
        ring.store_u64(field::TO_HOST_TAIL, 0);
        ring.store_u64(field::TO_GUEST_HEAD, 0);
        ring.memory
            .device_wrote(ring.control_gpa + field::HOST_WAITS_DATA, 4);
        ring.flag(field::HOST_WAITS_DATA).store(1, Ordering::SeqCst);
        Ok(ring)
    }

    /// Each ring's length in bytes.
    pub fn size(&self) -> usize {
        // The offer's ring is at most `MAX_RING`, a u32.
        self.size as usize
    }

    /// The bytes of the guest's memory it holds pinned: its slot's.
    pub fn pinned(&self) -> u64 {
        self.pinned.iter().map(|&(_, len)| len).sum()
    }

    pub(super) fn memory(&self) -> &Arc<GuestMemory> {
        &self.memory
    }

    fn word<T>(&self, offset: u64) -> *mut T {
        // SAFETY: every field lies within the control page (shards_abi::ring's test
        // `the_control_pages_fields_fit_it_each_on_a_line_of_its_own`), which is mapped,
        // pinned and page-aligned, so each field is aligned.
        unsafe { self.control.add(offset as usize).cast() }
    }

    fn load_u64(&self, offset: u64) -> u64 {
        // SAFETY: an aligned u64 of the pinned control page, reached only atomically here;
        // the guest's accesses are the hardware's, as a virtqueue index's are.
        unsafe { AtomicU64::from_ptr(self.word(offset)) }.load(Ordering::Acquire)
    }

    fn store_u64(&self, offset: u64, value: u64) {
        self.memory.device_wrote(self.control_gpa + offset, 8);
        // SAFETY: as in `load_u64`.
        unsafe { AtomicU64::from_ptr(self.word(offset)) }.store(value, Ordering::Release);
    }

    fn flag(&self, offset: u64) -> &AtomicU32 {
        // SAFETY: as in `load_u64`, an aligned u32 of the control page; the reference
        // covers the atomic alone, which every access to it is.
        unsafe { AtomicU32::from_ptr(self.word(offset)) }
    }

    /// Clears the guest's waiting flag at `offset`, after a position was published and
    /// fenced: whether it was set, so that the caller rings it.
    fn take_flag(&self, offset: u64) -> bool {
        fence(Ordering::SeqCst);
        self.memory.device_wrote(self.control_gpa + offset, 4);
        self.flag(offset).swap(0, Ordering::SeqCst) != 0
    }

    /// Bytes the guest has put in the ring for the host and the device has not taken, if
    /// its head can be believed.
    pub fn pending_to_host(&self) -> Result<u64, Broken> {
        let head = self.load_u64(field::TO_HOST_HEAD);
        let pending = head.checked_sub(self.taken).ok_or(Broken::HeadBehind)?;
        if pending > self.size {
            return Err(Broken::HeadAhead);
        }
        Ok(pending)
    }

    /// Room in the ring for the guest, if its tail can be believed.
    pub fn room_to_guest(&mut self) -> Result<u64, Broken> {
        let tail = self.load_u64(field::TO_GUEST_TAIL);
        if tail > self.given {
            return Err(Broken::TailAhead);
        }
        if tail < self.freed {
            return Err(Broken::TailBehind);
        }
        self.freed = tail;
        Ok(self.size - (self.given - tail))
    }

    /// Passes what the guest put in the ring for the host on to `stream`, as much as it
    /// takes at once.
    pub fn to_host(&mut self, stream: &UnixStream) -> Result<Turn, Broken> {
        let pending = self.pending_to_host()?;
        if pending == 0 {
            return Ok(Turn::default());
        }
        let from = self.taken % self.size;
        let spans = self.to_host.spans(self.size, from, pending);
        let moved = match writev(stream, &spans, pending as usize) {
            Ok(n) => n.min(pending as usize),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => 0,
            Err(e) => return Err(Broken::Socket(e.kind())),
        };
        if moved == 0 {
            return Ok(Turn::default());
        }
        self.taken += moved as u64;
        self.store_u64(field::TO_HOST_TAIL, self.taken);
        Ok(Turn {
            moved,
            ring: self.take_flag(field::GUEST_WAITS_ROOM),
            eof: false,
        })
    }

    /// Fills the ring for the guest from `stream`, as much as it holds and there is room
    /// for.
    pub fn from_host(&mut self, stream: &UnixStream) -> Result<Turn, Broken> {
        let room = self.room_to_guest()?;
        if room == 0 {
            return Ok(Turn::default());
        }
        let from = self.given % self.size;
        let spans = self.to_guest.spans(self.size, from, room);
        let moved = match readv(stream, &spans, room as usize) {
            Ok(0) => {
                return Ok(Turn {
                    eof: true,
                    ..Turn::default()
                });
            }
            Ok(n) => n.min(room as usize),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(Turn::default()),
            Err(e) => return Err(Broken::Socket(e.kind())),
        };
        let mut left = moved;
        for s in &spans {
            if left == 0 {
                break;
            }
            let n = s.len.min(left);
            self.memory.device_wrote(s.gpa, n as u64);
            left -= n;
        }
        self.given += moved as u64;
        self.store_u64(field::TO_GUEST_HEAD, self.given);
        Ok(Turn {
            moved,
            ring: self.take_flag(field::GUEST_WAITS_DATA),
            eof: false,
        })
    }

    /// Says the device waits for bytes in the ring for the host, then looks again: whether
    /// some came meanwhile, so that it need not wait (Dekker's handshake: the guest
    /// publishes its head, fences, then looks at this flag).
    pub fn wait_for_data(&self) -> Result<bool, Broken> {
        self.memory
            .device_wrote(self.control_gpa + field::HOST_WAITS_DATA, 4);
        self.flag(field::HOST_WAITS_DATA).store(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let came = self.pending_to_host()? > 0;
        if came {
            self.flag(field::HOST_WAITS_DATA).store(0, Ordering::SeqCst);
        }
        Ok(came)
    }

    /// Says the device waits for room in the ring for the guest, then looks again, as
    /// [`wait_for_data`](Self::wait_for_data) does.
    pub fn wait_for_room(&mut self) -> Result<bool, Broken> {
        self.memory
            .device_wrote(self.control_gpa + field::HOST_WAITS_ROOM, 4);
        self.flag(field::HOST_WAITS_ROOM).store(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let came = self.room_to_guest()? > 0;
        if came {
            self.flag(field::HOST_WAITS_ROOM).store(0, Ordering::SeqCst);
        }
        Ok(came)
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        for &(gpa, len) in &self.pinned {
            self.memory.unpin(gpa, len);
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use shards_abi::ring::Extent;
    use std::io::{Read as _, Write as _};

    const RAM: u64 = 64 << 20;
    const RING: u32 = 64 << 10;

    fn memory() -> Arc<GuestMemory> {
        Arc::new(GuestMemory::anonymous(&[(0, RAM as usize)]).unwrap())
    }

    /// A slot at `base` whose pages are scattered: each page `stride` pages from the last.
    fn scattered(base: u64, stride: u64) -> Offer {
        let pages = abi::most_extents(RING);
        Offer {
            ring: RING,
            extents: (0..pages)
                .map(|i| Extent {
                    gpa: base + i * stride * abi::PAGE,
                    len: abi::PAGE,
                })
                .collect(),
        }
    }

    fn whole(base: u64) -> Offer {
        Offer {
            ring: RING,
            extents: vec![Extent {
                gpa: base,
                len: abi::slot_len(RING),
            }],
        }
    }

    /// The guest's side: the bytes at slot offset `at` of `offer`'s pages.
    fn slot_ptr(memory: &GuestMemory, offer: &Offer, at: u64) -> *mut u8 {
        let mut start = 0;
        for e in &offer.extents {
            if at < start + e.len {
                return memory.host_ptr(e.gpa + (at - start), 1).unwrap();
            }
            start += e.len;
        }
        panic!("{at} past the slot");
    }

    fn guest_u64(memory: &GuestMemory, offer: &Offer, field: u64) -> &'static AtomicU64 {
        // SAFETY: an aligned u64 of the test's guest memory, which outlives the test's uses.
        unsafe { AtomicU64::from_ptr(slot_ptr(memory, offer, field).cast()) }
    }

    fn guest_u32(memory: &GuestMemory, offer: &Offer, field: u64) -> &'static AtomicU32 {
        // SAFETY: as in `guest_u64`.
        unsafe { AtomicU32::from_ptr(slot_ptr(memory, offer, field).cast()) }
    }

    /// The guest writes `bytes` into the ring for the host from position `head`.
    fn guest_put(memory: &GuestMemory, offer: &Offer, head: u64, bytes: &[u8]) {
        for (i, b) in bytes.iter().enumerate() {
            let at = abi::CONTROL + (head + i as u64) % u64::from(RING);
            // SAFETY: a byte of the test's guest memory, as the guest writes it.
            unsafe { slot_ptr(memory, offer, at).write(*b) };
        }
        guest_u64(memory, offer, field::TO_HOST_HEAD).store(head + bytes.len() as u64, Ordering::Release);
    }

    /// The guest reads `len` bytes of the ring for the guest from position `tail`.
    fn guest_take(memory: &GuestMemory, offer: &Offer, tail: u64, len: u64) -> Vec<u8> {
        let ring = u64::from(RING);
        let out = (0..len)
            // SAFETY: a byte of the test's guest memory, as the guest reads it.
            .map(|i| unsafe { slot_ptr(memory, offer, abi::CONTROL + ring + (tail + i) % ring).read() })
            .collect();
        guest_u64(memory, offer, field::TO_GUEST_TAIL).store(tail + len, Ordering::Release);
        out
    }

    fn pattern(n: usize, salt: u8) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(salt))
            .collect()
    }

    #[test]
    fn an_offer_of_memory_the_guest_lacks_or_a_ring_holds_is_refused() {
        let memory = memory();
        let past = whole(RAM);
        assert_eq!(Ring::attach(&memory, &past).unwrap_err(), Refusal::NotRam);
        let straddling = whole(RAM - abi::PAGE);
        assert_eq!(Ring::attach(&memory, &straddling).unwrap_err(), Refusal::NotRam);
        let held = Ring::attach(&memory, &whole(1 << 20)).unwrap();
        // Any page of the held slot, in any extent of another offer, is refused.
        let mut overlapping = scattered(8 << 20, 1);
        overlapping.extents[7].gpa = (1 << 20) + 3 * abi::PAGE;
        assert_eq!(Ring::attach(&memory, &overlapping).unwrap_err(), Refusal::InUse);
        drop(held);
        // Let go, its pages are free for the next.
        assert!(Ring::attach(&memory, &whole(1 << 20)).is_ok());
    }

    /// The device waits for the relay's first bytes from its attach: the relay rings only
    /// a side that says it waits.
    #[test]
    fn an_attached_ring_says_it_waits_for_the_relays_first_bytes() {
        let memory = memory();
        let offer = whole(2 << 20);
        let ring = Ring::attach(&memory, &offer).unwrap();
        assert_eq!(
            guest_u32(&memory, &offer, field::HOST_WAITS_DATA).load(Ordering::SeqCst),
            1
        );
        assert_eq!(ring.pinned(), abi::slot_len(RING));
    }

    #[test]
    fn an_offer_that_names_a_page_twice_is_refused() {
        let memory = memory();
        let mut twice = scattered(4 << 20, 1);
        twice.extents[9].gpa = twice.extents[2].gpa;
        assert_eq!(Ring::attach(&memory, &twice).unwrap_err(), Refusal::Malformed);
        assert!(!memory.pinned(4 << 20, abi::slot_len(RING)));
    }

    #[test]
    fn an_attached_slots_pages_are_pinned_until_it_goes() {
        let memory = memory();
        let offer = scattered(4 << 20, 3);
        let ring = Ring::attach(&memory, &offer).unwrap();
        for e in &offer.extents {
            assert!(memory.pinned(e.gpa, e.len));
        }
        drop(ring);
        for e in &offer.extents {
            assert!(!memory.pinned(e.gpa, e.len));
        }
    }

    #[test]
    fn bytes_cross_each_way_through_scattered_pages_and_round_the_rings_end() {
        for offer in [whole(2 << 20), scattered(4 << 20, 2)] {
            let memory = memory();
            let mut ring = Ring::attach(&memory, &offer).unwrap();
            let (host, daemon) = UnixStream::pair().unwrap();
            host.set_nonblocking(true).unwrap();
            // Guest to host: three rings' worth in turns of five ninths of a ring, so that
            // each turn's span wraps somewhere new.
            let sent = pattern(3 * RING as usize, 7);
            let mut head = 0u64;
            let mut got = Vec::new();
            for chunk in sent.chunks(RING as usize * 5 / 9) {
                guest_put(&memory, &offer, head, chunk);
                head += chunk.len() as u64;
                while ring.pending_to_host().unwrap() > 0 {
                    let turn = ring.to_host(&host).unwrap();
                    let mut buf = vec![0u8; turn.moved];
                    (&daemon).read_exact(&mut buf).unwrap();
                    got.extend_from_slice(&buf);
                }
                assert_eq!(
                    guest_u64(&memory, &offer, field::TO_HOST_TAIL).load(Ordering::Acquire),
                    head
                );
            }
            assert_eq!(got, sent);
            // Host to guest, the same.
            let sent = pattern(3 * RING as usize, 11);
            let mut tail = 0u64;
            let mut got = Vec::new();
            for chunk in sent.chunks(RING as usize * 5 / 9) {
                (&daemon).write_all(chunk).unwrap();
                let mut left = chunk.len();
                while left > 0 {
                    let turn = ring.from_host(&host).unwrap();
                    left -= turn.moved;
                    let head = guest_u64(&memory, &offer, field::TO_GUEST_HEAD).load(Ordering::Acquire);
                    got.extend(guest_take(&memory, &offer, tail, head - tail));
                    tail = head;
                }
            }
            assert_eq!(got, sent);
        }
    }

    #[test]
    fn a_full_ring_takes_no_more_and_the_host_socket_waits() {
        let memory = memory();
        let offer = whole(2 << 20);
        let mut ring = Ring::attach(&memory, &offer).unwrap();
        let (host, daemon) = UnixStream::pair().unwrap();
        host.set_nonblocking(true).unwrap();
        (&daemon).write_all(&pattern(RING as usize + 100, 3)).unwrap();
        let mut moved = 0;
        loop {
            let turn = ring.from_host(&host).unwrap();
            if turn.moved == 0 {
                break;
            }
            moved += turn.moved;
        }
        assert_eq!(moved, RING as usize);
        assert_eq!(ring.room_to_guest().unwrap(), 0);
        // Full, it waits; the guest takes some, and it does not.
        assert!(!ring.wait_for_room().unwrap());
        assert_eq!(
            guest_u32(&memory, &offer, field::HOST_WAITS_ROOM).load(Ordering::SeqCst),
            1
        );
        guest_take(&memory, &offer, 0, 100);
        assert!(ring.wait_for_room().unwrap());
        assert_eq!(
            guest_u32(&memory, &offer, field::HOST_WAITS_ROOM).load(Ordering::SeqCst),
            0
        );
        assert_eq!(ring.from_host(&host).unwrap().moved, 100);
    }

    #[test]
    fn a_guest_waiting_is_told_once_and_its_flag_cleared() {
        let memory = memory();
        let offer = whole(2 << 20);
        let mut ring = Ring::attach(&memory, &offer).unwrap();
        let (host, daemon) = UnixStream::pair().unwrap();
        host.set_nonblocking(true).unwrap();
        // Waiting for data: the host's bytes ring it once.
        guest_u32(&memory, &offer, field::GUEST_WAITS_DATA).store(1, Ordering::SeqCst);
        (&daemon).write_all(b"hello").unwrap();
        assert!(ring.from_host(&host).unwrap().ring);
        assert_eq!(
            guest_u32(&memory, &offer, field::GUEST_WAITS_DATA).load(Ordering::SeqCst),
            0
        );
        (&daemon).write_all(b"again").unwrap();
        assert!(!ring.from_host(&host).unwrap().ring);
        // Waiting for room: what the device takes rings it.
        guest_put(&memory, &offer, 0, b"up");
        guest_u32(&memory, &offer, field::GUEST_WAITS_ROOM).store(1, Ordering::SeqCst);
        let turn = ring.to_host(&host).unwrap();
        assert_eq!((turn.moved, turn.ring), (2, true));
        // And the device's own wait, for data, sees what came meanwhile.
        assert!(!ring.wait_for_data().unwrap());
        guest_put(&memory, &offer, 2, b"more");
        assert!(ring.wait_for_data().unwrap());
    }

    #[test]
    fn positions_the_guest_could_not_have_reached_end_the_ring() {
        let memory = memory();
        let offer = whole(2 << 20);
        let mut ring = Ring::attach(&memory, &offer).unwrap();
        let (host, daemon) = UnixStream::pair().unwrap();
        host.set_nonblocking(true).unwrap();
        let head = guest_u64(&memory, &offer, field::TO_HOST_HEAD);
        head.store(u64::from(RING) + 1, Ordering::Release);
        assert_eq!(ring.to_host(&host), Err(Broken::HeadAhead));
        head.store(u64::MAX, Ordering::Release);
        assert_eq!(ring.to_host(&host), Err(Broken::HeadAhead));
        guest_put(&memory, &offer, 0, b"abc");
        ring.to_host(&host).unwrap();
        head.store(1, Ordering::Release);
        assert_eq!(ring.to_host(&host), Err(Broken::HeadBehind));
        let tail = guest_u64(&memory, &offer, field::TO_GUEST_TAIL);
        tail.store(1, Ordering::Release);
        assert_eq!(ring.from_host(&host), Err(Broken::TailAhead));
        tail.store(0, Ordering::Release);
        (&daemon).write_all(b"xyz").unwrap();
        ring.from_host(&host).unwrap();
        tail.store(2, Ordering::Release);
        ring.room_to_guest().unwrap();
        tail.store(1, Ordering::Release);
        assert_eq!(ring.room_to_guest(), Err(Broken::TailBehind));
    }

    #[test]
    fn the_host_sockets_end_is_said_and_its_errors_end_the_ring() {
        let memory = memory();
        let offer = whole(2 << 20);
        let mut ring = Ring::attach(&memory, &offer).unwrap();
        let (host, daemon) = UnixStream::pair().unwrap();
        host.set_nonblocking(true).unwrap();
        assert_eq!(ring.from_host(&host).unwrap(), Turn::default());
        daemon.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(ring.from_host(&host).unwrap().eof);
        drop(daemon);
        guest_put(&memory, &offer, 0, b"to no one");
        assert!(matches!(ring.to_host(&host), Err(Broken::Socket(_))));
    }

    #[test]
    fn what_the_device_writes_is_noted_while_a_move_watches() {
        let memory = memory();
        let offer = whole(2 << 20);
        let mut ring = Ring::attach(&memory, &offer).unwrap();
        let (host, daemon) = UnixStream::pair().unwrap();
        host.set_nonblocking(true).unwrap();
        memory.watch_device_writes();
        (&daemon).write_all(&pattern(3 * abi::PAGE as usize, 1)).unwrap();
        ring.from_host(&host).unwrap();
        let written = memory.device_written();
        // The control page, and the three pages of the ring for the guest written.
        let ring_start = (2 << 20) + abi::CONTROL + u64::from(RING);
        for page in [
            2 << 20,
            ring_start,
            ring_start + abi::PAGE,
            ring_start + 2 * abi::PAGE,
        ] {
            assert!(written.contains(&page), "{page:#x} not in {written:x?}");
        }
    }
}
