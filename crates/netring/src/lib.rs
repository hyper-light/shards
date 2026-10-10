//! Frames between a VM process and its network process (docs/design/architecture.md
//! D31): two rings of bytes in memory the two share, one each way, each with a doorbell a
//! side waiting for it sleeps on (PM M83). Each frame is a record: its length, then its
//! bytes, eight-aligned; a record that would run past the end of the ring is preceded by
//! padding to the end, so a record is always whole.
//!
//! Each side treats the other as hostile, since a network process parses what the
//! Internet sends and a VM process what a guest does. A side keeps its own position to
//! itself, and only writes the shared copy for the peer: where it reads and writes never
//! comes from memory the peer can change. The peer's position, and every length, is
//! checked against it before it is used, and no reference into the shared memory is ever
//! made, only copies through raw pointers. A peer that writes nonsense spoils its own
//! frames, and is cut off, but reaches nothing of this side's.

#![cfg(unix)]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

/// The largest frame a record holds: a virtio-net header and a 64 KiB Ethernet frame
/// (an MTU of 65535 and its 14-byte header), as the device carries them.
pub const MAX_FRAME: usize = 12 + 14 + 65535;
/// Bytes of each direction's ring: room for 16 of the largest frames, and many more small
/// ones (PM M83 ran 64 slots; a byte ring holds as many frames as their sizes allow).
pub const RING: usize = 1 << 20;
const HEADER: usize = 8;
/// A record's kind, beside its length: a frame, or padding to the ring's end.
const FRAME: u32 = 0;
const PAD: u32 = 1;

/// One direction's positions and flags, each on a cache line of its own.
#[repr(C, align(64))]
struct Line<T>(T);

#[repr(C)]
struct Control {
    /// Bytes produced and consumed so far; the ring holds `head - tail`.
    head: Line<AtomicU64>,
    tail: Line<AtomicU64>,
    /// A consumer asleep for a frame, and a producer asleep for room.
    consumer_waits: Line<AtomicU32>,
    producer_waits: Line<AtomicU32>,
}

/// The controls' page: 64 KiB, the largest page size of the systems shards runs on
/// (arm64 Linux may use 64 KiB pages, Apple silicon 16 KiB), so that the region is whole
/// pages everywhere; macOS rounds a shared memory object up to them.
const CONTROL: usize = 64 * 1024;

/// The shared memory: both directions' controls in the first page, then each
/// direction's ring.
#[derive(Debug)]
pub struct Region {
    base: *mut u8,
    len: usize,
    fd: OwnedFd,
}

// SAFETY: the region is a mapping both sides reach only through atomics and raw-pointer
// copies, never references; moving it between threads changes nothing of that.
unsafe impl Send for Region {}
// SAFETY: as above: shared, the region is reached through atomics and raw-pointer copies
// alone, by one producer and one consumer of each direction.
unsafe impl Sync for Region {}

/// The region's size: the controls' page and two rings.
pub const SIZE: usize = CONTROL + 2 * RING;

impl Region {
    /// Maps the region a peer made and handed over as `fd`. Its size must be [`SIZE`].
    pub fn map(fd: OwnedFd) -> io::Result<Region> {
        // SAFETY: fstat(2) into a zeroed buffer, for a descriptor we own.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if usize::try_from(st.st_size).ok() != Some(SIZE) {
            return Err(io::Error::other("a frame region of the wrong size"));
        }
        // SAFETY: a shared mapping of the whole object, which this region owns until drop.
        let p = unsafe {
            libc::mmap(
                ptr::null_mut(),
                SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Region {
            base: p.cast(),
            len: SIZE,
            fd,
        })
    }

    /// The descriptor to hand the peer.
    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Direction `d` (0 or 1)'s control block and ring.
    fn direction(&self, d: usize) -> (*const Control, *mut u8) {
        // SAFETY: both offsets lie within the mapping: the controls' page holds two
        // Controls (4 lines of 64 bytes each), and the rings follow it.
        unsafe {
            (
                self.base
                    .add(d * std::mem::size_of::<Control>())
                    .cast::<Control>(),
                self.base.add(CONTROL + d * RING),
            )
        }
    }

    /// The producer end of direction `d`, ringing `doorbell` for its consumer; its ring
    /// starts empty.
    pub fn producer(&self, d: usize, doorbell: OwnedFd) -> Producer<'_> {
        let (control, ring) = self.direction(d & 1);
        let producer = Producer {
            _region: self,
            control,
            ring,
            head: 0,
            doorbell,
        };
        producer.control().head.0.store(0, Ordering::Release);
        producer
    }

    /// The consumer end of direction `d`, ringing `doorbell` for its producer; it starts
    /// at the ring's start.
    pub fn consumer(&self, d: usize, doorbell: OwnedFd, waits_on: OwnedFd) -> Consumer<'_> {
        let (control, ring) = self.direction(d & 1);
        let consumer = Consumer {
            _region: self,
            control,
            ring,
            tail: 0,
            doorbell,
            waits_on,
        };
        consumer.control().tail.0.store(0, Ordering::Release);
        consumer
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `map`, unmapped once.
        unsafe { libc::munmap(self.base.cast(), self.len) };
    }
}

/// Shared memory for a region, zeroed and unmapped: for a spawner that hands it to the
/// two processes that map it.
pub fn memory() -> io::Result<OwnedFd> {
    platform::shared_memory(SIZE)
}

/// A doorbell: a pipe's two ends, the read end for the side that sleeps on it, the write
/// end for the side that wakes it. Both nonblocking: a ring already rung needs no more.
pub fn doorbell() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // Close-on-exec from its making where the kernel can (Linux): the daemon making it
    // spawns VMs on other threads, which would otherwise have it. macOS cannot, and
    // spawns with POSIX_SPAWN_CLOEXEC_DEFAULT.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: an array of two descriptors.
    let made = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    // SAFETY: an array of two descriptors.
    let made = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if made != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: two descriptors just made, owned from here on.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    for fd in [&r, &w] {
        // SAFETY: fcntl(2) on descriptors we own.
        unsafe {
            let fl = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    Ok((r, w))
}

fn ring_bell(fd: &OwnedFd) {
    let b = [1u8];
    // SAFETY: a one-byte write; a full pipe has a wakeup pending already.
    unsafe { libc::write(fd.as_raw_fd(), b.as_ptr().cast(), 1) };
}

/// Why a ring was given up on: the peer broke its rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broken(pub &'static str);

impl std::fmt::Display for Broken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the frame ring's peer broke it: {}", self.0)
    }
}

/// The side that writes frames.
#[derive(Debug)]
pub struct Producer<'r> {
    _region: &'r Region,
    control: *const Control,
    ring: *mut u8,
    /// Bytes produced so far: this side's own, of which the shared `head` is a copy.
    head: u64,
    /// Rung for the consumer. A producer short of room is rung on its process's own
    /// doorbell, which its consumer of the other direction sleeps on.
    doorbell: OwnedFd,
}

// SAFETY: as Region's: atomics and raw-pointer copies alone.
unsafe impl Send for Producer<'_> {}

fn padded(n: usize) -> usize {
    (HEADER + n + 7) & !7
}

impl Producer<'_> {
    fn control(&self) -> &Control {
        // SAFETY: the control block lies in the mapping the region keeps; only its atomics
        // are touched.
        unsafe { &*self.control }
    }

    /// Room for a frame of `n` bytes now, the padding a wrap needs included; or why the
    /// consumer's position cannot be believed: past this side's, or more than a ring
    /// behind it.
    fn room(&self, n: usize) -> Result<bool, Broken> {
        let tail = self.control().tail.0.load(Ordering::Acquire);
        let used = self
            .head
            .checked_sub(tail)
            .ok_or(Broken("its tail is past the head"))?;
        if used > RING as u64 {
            return Err(Broken("it consumed more than was written"));
        }
        // This side's head is eight-aligned: `at` leaves room for a header.
        let at = (self.head % RING as u64) as usize;
        let need = padded(n) + if at + padded(n) > RING { RING - at } else { 0 };
        Ok(RING as u64 - used >= need as u64)
    }

    /// Writes one frame of `n` bytes if there is room now: `fill` gets where to copy them
    /// and copies all `n`. `None` if there is no room, the consumer asked to ring this
    /// side's doorbell once it makes some; `Some(false)` if `n` is past [`MAX_FRAME`].
    /// Never waits: a device thread that must also drain the other way cannot.
    pub fn try_push_with(&mut self, n: usize, fill: impl FnOnce(*mut u8)) -> Result<Option<bool>, Broken> {
        if n > MAX_FRAME {
            return Ok(Some(false));
        }
        if !self.room(n)? {
            let c = self.control();
            c.producer_waits.0.store(1, Ordering::SeqCst);
            // The consumer stores its tail, fences, then looks at the flag; this side
            // stores the flag, fences, then looks at the tail: one of the two sees the
            // other.
            fence(Ordering::SeqCst);
            if !self.room(n)? {
                return Ok(None);
            }
            c.producer_waits.0.store(0, Ordering::SeqCst);
        }
        let mut head = self.head;
        let mut at = (head % RING as u64) as usize;
        if at + padded(n) > RING {
            self.write_header(at, (RING - at - HEADER) as u32, PAD);
            head += (RING - at) as u64;
            at = 0;
        }
        self.write_header(at, n as u32, FRAME);
        // SAFETY: `at + HEADER .. + n` lies in the ring: the record fits before its end.
        fill(unsafe { self.ring.add(at + HEADER) });
        self.head = head + padded(n) as u64;
        let c = self.control();
        c.head.0.store(self.head, Ordering::Release);
        fence(Ordering::SeqCst);
        if c.consumer_waits.0.swap(0, Ordering::SeqCst) == 1 {
            ring_bell(&self.doorbell);
        }
        Ok(Some(true))
    }

    /// Writes one frame of `parts`, one after another, if there is room now, as
    /// [`Producer::try_push_with`] does: a frame's headers and its payload, each where it
    /// lies, copied once.
    pub fn try_push(&mut self, parts: &[&[u8]]) -> Result<Option<bool>, Broken> {
        let n = parts.iter().fold(0usize, |n, part| n.saturating_add(part.len()));
        self.try_push_with(n, |dst| {
            let mut off = 0;
            for part in parts {
                // SAFETY: `dst` has room for all `n` bytes, of which these are the next.
                unsafe { ptr::copy_nonoverlapping(part.as_ptr(), dst.add(off), part.len()) };
                off += part.len();
            }
        })
    }

    fn write_header(&self, at: usize, len: u32, kind: u32) {
        let [a, b, c, d] = len.to_le_bytes();
        let [e, f, g, i] = kind.to_le_bytes();
        let h = [a, b, c, d, e, f, g, i];
        // SAFETY: `at` leaves room for a header: records are eight-aligned and the ring's
        // size a multiple of eight.
        unsafe { ptr::copy_nonoverlapping(h.as_ptr(), self.ring.add(at), HEADER) };
    }
}

/// The side that reads frames.
#[derive(Debug)]
pub struct Consumer<'r> {
    _region: &'r Region,
    control: *const Control,
    ring: *mut u8,
    /// Bytes consumed so far: this side's own, of which the shared `tail` is a copy.
    tail: u64,
    /// Rung for the producer once room is made; slept on for frames.
    doorbell: OwnedFd,
    waits_on: OwnedFd,
}

// SAFETY: as Region's.
unsafe impl Send for Consumer<'_> {}

impl Consumer<'_> {
    fn control(&self) -> &Control {
        // SAFETY: as the producer's.
        unsafe { &*self.control }
    }

    /// The descriptor this side sleeps on, to wait for it among others.
    pub fn waits_on(&self) -> RawFd {
        self.waits_on.as_raw_fd()
    }

    /// The producer's position, if it can be believed: not behind this side's, and not
    /// more than a ring ahead of it.
    fn head(&self) -> Result<u64, Broken> {
        let head = self.control().head.0.load(Ordering::Acquire);
        let used = head
            .checked_sub(self.tail)
            .ok_or(Broken("its head is behind the tail"))?;
        if used > RING as u64 {
            return Err(Broken("it wrote more than the ring holds"));
        }
        Ok(head)
    }

    /// Whether a frame is ready.
    pub fn ready(&self) -> Result<bool, Broken> {
        Ok(self.head()? != self.tail)
    }

    /// Asks to be rung when a frame comes, then whether one came meanwhile: if not, the
    /// caller may sleep on [`waits_on`](Self::waits_on).
    pub fn arm(&self) -> Result<bool, Broken> {
        self.control().consumer_waits.0.store(1, Ordering::SeqCst);
        // The producer stores its head, fences, then looks at the flag; this side stores
        // the flag, fences, then looks at the head: one of the two sees the other.
        fence(Ordering::SeqCst);
        let ready = self.ready()?;
        if ready {
            self.control().consumer_waits.0.store(0, Ordering::SeqCst);
        }
        Ok(ready)
    }

    /// The next record past any padding, given the producer's `head`: where its frame
    /// starts, its length, and this side's position after its padding.
    fn next(&self, head: u64) -> Result<(usize, usize, u64), Broken> {
        let mut tail = self.tail;
        // This side's tail is eight-aligned: `at` leaves room for a header.
        let mut at = (tail % RING as u64) as usize;
        let (mut len, mut kind) = self.read_header(at);
        if kind == PAD {
            let skip = RING - at;
            if len as usize != skip - HEADER || head - tail <= skip as u64 {
                return Err(Broken("its padding is not to the end"));
            }
            tail += skip as u64;
            at = 0;
            (len, kind) = self.read_header(at);
        }
        let n = len as usize;
        if kind != FRAME || n > MAX_FRAME || at + padded(n) > RING || head - tail < padded(n) as u64 {
            return Err(Broken("a frame's length runs past what was written"));
        }
        Ok((at, n, tail))
    }

    /// The next frame's length, if one is ready, without taking it.
    pub fn peek_len(&self) -> Result<Option<usize>, Broken> {
        let head = self.head()?;
        if head == self.tail {
            return Ok(None);
        }
        Ok(Some(self.next(head)?.1))
    }

    /// Takes the next frame, if any, giving `take` its length and a function that copies
    /// a range of its bytes out; what `take` returns, once the frame is consumed.
    pub fn pop<R>(
        &mut self,
        take: impl FnOnce(usize, &dyn Fn(usize, *mut u8, usize)) -> R,
    ) -> Result<Option<R>, Broken> {
        self.pop_frame(|frame, n| {
            let copy = move |from: usize, to: *mut u8, count: usize| {
                if from <= n && count <= n - from {
                    // SAFETY: `from .. from + count` lies within the frame's `n` bytes;
                    // `to` is the caller's.
                    unsafe { ptr::copy_nonoverlapping(frame.add(from), to, count) };
                }
            };
            take(n, &copy)
        })
    }

    /// Takes the next frame, if any, giving `take` where it lies in the ring and its length:
    /// its `n` bytes there hold still until `take` returns, for a caller that copies them by
    /// accesses of its own, as the VMM's device does into guest memory (D29).
    pub fn pop_frame<R>(&mut self, take: impl FnOnce(*const u8, usize) -> R) -> Result<Option<R>, Broken> {
        let head = self.head()?;
        if head == self.tail {
            return Ok(None);
        }
        let (at, n, tail) = self.next(head)?;
        // SAFETY: `at + HEADER .. + n` lies within the ring, as `next` checked.
        let frame = unsafe { self.ring.add(at + HEADER) };
        let r = take(frame.cast_const(), n);
        self.tail = tail + padded(n) as u64;
        let c = self.control();
        c.tail.0.store(self.tail, Ordering::Release);
        fence(Ordering::SeqCst);
        if c.producer_waits.0.swap(0, Ordering::SeqCst) == 1 {
            ring_bell(&self.doorbell);
        }
        Ok(Some(r))
    }

    fn read_header(&self, at: usize) -> (u32, u32) {
        let mut h = [0u8; HEADER];
        // SAFETY: `at` is eight-aligned within the ring, with room for a header.
        unsafe { ptr::copy_nonoverlapping(self.ring.add(at), h.as_mut_ptr(), HEADER) };
        let [a, b, c, d, e, f, g, i] = h;
        (u32::from_le_bytes([a, b, c, d]), u32::from_le_bytes([e, f, g, i]))
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::io;
    use std::os::fd::{FromRawFd, OwnedFd};

    /// An anonymous memory file of `size` bytes, zeroed.
    pub fn shared_memory(size: usize) -> io::Result<OwnedFd> {
        // SAFETY: memfd_create(2) with a NUL-terminated name; the descriptor is owned.
        let fd = unsafe { libc::memfd_create(c"shards-frames".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor just made.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: ftruncate(2) on it.
        if unsafe { libc::ftruncate(std::os::fd::AsRawFd::as_raw_fd(&fd), size as libc::off_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    /// A POSIX shared memory object of `size` bytes, zeroed, its name unlinked at once, so
    /// that only its descriptor reaches it: macOS has no memfd.
    pub fn shared_memory(size: usize) -> io::Result<OwnedFd> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        for _ in 0..16 {
            // Short: macOS bounds these names at 31 bytes (PSHMNAMLEN).
            let name = format!("/sf{}.{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
            let c = std::ffi::CString::new(name).map_err(|_| io::Error::other("a name with NUL"))?;
            // SAFETY: shm_open(2) with a NUL-terminated name.
            let fd =
                unsafe { libc::shm_open(c.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600) };
            if fd < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EEXIST) {
                    continue;
                }
                return Err(e);
            }
            // SAFETY: shm_unlink(2) of the name just made; the object lives on through fd.
            unsafe { libc::shm_unlink(c.as_ptr()) };
            // SAFETY: a descriptor just made.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            // SAFETY: fcntl(2) and ftruncate(2) on it.
            unsafe {
                libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
                if libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            return Ok(fd);
        }
        Err(io::Error::other("no free shared memory name"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn region() -> Region {
        Region::map(memory().unwrap()).unwrap()
    }

    /// Sleeps on `fd` until it rings, and drains it.
    fn sleep(fd: RawFd) {
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd.
        unsafe { libc::poll(&mut p, 1, -1) };
        let mut buf = [0u8; 64];
        // SAFETY: a buffer of the length given, from a nonblocking descriptor.
        while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
    }

    /// Writes one frame of `parts`, sleeping on `wait` while there is no room.
    fn push(p: &mut Producer<'_>, wait: &OwnedFd, parts: &[&[u8]]) -> Result<bool, Broken> {
        loop {
            match p.try_push(parts)? {
                Some(fit) => return Ok(fit),
                None => sleep(wait.as_raw_fd()),
            }
        }
    }

    fn frame(seq: u32, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (seq as usize).wrapping_mul(31).wrapping_add(i) as u8)
            .collect()
    }

    fn take(c: &mut Consumer<'_>) -> Option<Vec<u8>> {
        c.pop(|n, copy| {
            let mut v = vec![0u8; n];
            copy(0, v.as_mut_ptr(), n);
            v
        })
        .unwrap()
    }

    /// Frames of every size cross in order, through every wrap of the ring, a thread on
    /// each side, each sleeping when it must, as the devices' loops do.
    #[test]
    fn frames_cross_in_order_through_every_wrap() {
        let region = region();
        let (c_wait, p_ring) = doorbell().unwrap();
        let (p_wait, c_ring) = doorbell().unwrap();
        let mut producer = region.producer(0, p_ring);
        let mut consumer = region.consumer(0, c_ring, c_wait);
        let sizes = [0usize, 1, 7, 8, 9, 60, 1514, 9014, MAX_FRAME, 4093];
        let n = 20_000u32;
        std::thread::scope(|s| {
            s.spawn(|| {
                for i in 0..n {
                    // In three parts, as a frame's headers and the two halves of a
                    // queue its payload wraps in come.
                    let f = frame(i, sizes[i as usize % sizes.len()]);
                    let (a, rest) = f.split_at(f.len() / 3);
                    let (b, c) = rest.split_at(rest.len() / 2);
                    assert!(push(&mut producer, &p_wait, &[a, b, c]).unwrap());
                }
            });
            for i in 0..n {
                loop {
                    if let Some(f) = take(&mut consumer) {
                        assert_eq!(f, frame(i, sizes[i as usize % sizes.len()]), "frame {i}");
                        break;
                    }
                    if !consumer.arm().unwrap() {
                        sleep(consumer.waits_on());
                    }
                }
            }
        });
        assert!(take(&mut consumer).is_none());
        assert_eq!(producer.try_push_with(MAX_FRAME + 1, |_| ()), Ok(Some(false)));
    }

    /// A peer that writes nonsense into its shared position or a length is caught, and
    /// its nonsense never leads the reader outside the ring.
    #[test]
    fn a_peer_that_breaks_the_ring_is_caught() {
        let region = region();
        let (cw, pr) = doorbell().unwrap();
        let (pw, cr) = doorbell().unwrap();
        let mut producer = region.producer(0, pr);
        let mut consumer = region.consumer(0, cr, cw);
        push(&mut producer, &pw, &[b"hello"]).unwrap();
        // A length past what was written.
        // SAFETY: the test plays the hostile peer, writing the shared header directly.
        unsafe { ptr::copy_nonoverlapping(u32::MAX.to_le_bytes().as_ptr(), consumer.ring, 4) };
        assert!(consumer.pop(|_, _| ()).is_err());
        // A head beyond the tail by more than the ring.
        consumer.control().head.0.store(u64::MAX, Ordering::SeqCst);
        assert!(consumer.ready().is_err());
        // A tail past the head.
        producer.control().tail.0.store(u64::MAX, Ordering::SeqCst);
        assert!(push(&mut producer, &pw, &[b"x"]).is_err());
    }

    /// A peer that writes this side's own shared position moves nothing of this side's:
    /// a hostile VM process that puts the network process's head four bytes short of the
    /// last ring's end, its own tail there too, would have it write a header past the
    /// mapping's end were that head believed.
    #[test]
    fn a_peer_cannot_move_this_sides_position() {
        let region = region();
        let (cw, pr) = doorbell().unwrap();
        let (pw, cr) = doorbell().unwrap();
        let mut producer = region.producer(1, pr);
        let mut consumer = region.consumer(1, cr, cw);
        let short = (RING - 4) as u64;
        producer.control().head.0.store(short, Ordering::SeqCst);
        producer.control().tail.0.store(short, Ordering::SeqCst);
        assert_eq!(
            producer.try_push_with(5, |_| ()),
            Err(Broken("its tail is past the head"))
        );
        // The tail put back, the ring goes on from where this side was, not from where
        // its peer said.
        producer.control().tail.0.store(0, Ordering::SeqCst);
        assert!(push(&mut producer, &pw, &[b"after"]).unwrap());
        assert_eq!(take(&mut consumer).unwrap(), b"after");
        // The consumer's own position, moved by its peer, moves nothing of its either.
        assert!(push(&mut producer, &pw, &[b"again"]).unwrap());
        consumer.control().tail.0.store(short, Ordering::SeqCst);
        assert_eq!(take(&mut consumer).unwrap(), b"again");
        assert_eq!(
            consumer.control().tail.0.load(Ordering::SeqCst),
            2 * padded(5) as u64
        );
    }

    /// A measurement: a 64-byte frame's round trip, and frames' throughput one way, each
    /// side sleeping on its own doorbell whenever its ring is empty, as the VM's device
    /// and the network process do: no spin (review 2.17, PM M105). Threads stand for the
    /// two processes; the wake is the same pipe either way.
    ///
    ///     cargo test -p shards-netring --release -- --ignored --nocapture frames_cost
    #[test]
    #[ignore = "a measurement"]
    fn frames_cost() {
        let region = region();
        // Each side's own doorbell: the read end it sleeps on, the write end its peer rings.
        let (a_sleeps, rings_a) = doorbell().unwrap();
        let (b_sleeps, rings_b) = doorbell().unwrap();
        let mut a_out = region.producer(0, rings_b.try_clone().unwrap());
        let mut a_in = region.consumer(1, rings_b, a_sleeps);
        let mut b_out = region.producer(1, rings_a.try_clone().unwrap());
        let mut b_in = region.consumer(0, rings_a, b_sleeps);
        // The production wait: take a frame, else ask to be rung and sleep.
        let next = |c: &mut Consumer<'_>| loop {
            if let Some(f) = take(c) {
                return f;
            }
            if !c.arm().unwrap() {
                sleep(c.waits_on());
            }
        };
        let send = |p: &mut Producer<'_>, f: &[u8]| loop {
            match p.try_push(&[f]).unwrap() {
                Some(true) => return,
                Some(false) => panic!("a frame past the largest"),
                None => std::thread::yield_now(),
            }
        };
        let n = 20_000;
        let mut rtt = Vec::with_capacity(n);
        std::thread::scope(|s| {
            s.spawn(|| {
                for _ in 0..n {
                    let f = next(&mut b_in);
                    send(&mut b_out, &f);
                }
            });
            let f = [7u8; 64];
            for _ in 0..n {
                let t = std::time::Instant::now();
                send(&mut a_out, &f);
                let _ = next(&mut a_in);
                rtt.push(t.elapsed().as_nanos() as f64 / 1e3);
            }
        });
        rtt.sort_by(f64::total_cmp);
        let at = |q: f64| rtt[((rtt.len() - 1) as f64 * q) as usize];
        println!(
            "64-byte round trip, n={n}: p50 {:.1} us p90 {:.1} p99 {:.1} max {:.1}",
            at(0.5),
            at(0.9),
            at(0.99),
            rtt[rtt.len() - 1]
        );
        for size in [1514usize, 9014, MAX_FRAME] {
            let frames = (1usize << 30) / size;
            let f = vec![1u8; size];
            let t = std::time::Instant::now();
            std::thread::scope(|s| {
                s.spawn(|| {
                    for _ in 0..frames {
                        let _ = next(&mut b_in);
                    }
                });
                for _ in 0..frames {
                    send(&mut a_out, &f);
                }
            });
            let gbit = (frames * size) as f64 * 8.0 / t.elapsed().as_secs_f64() / 1e9;
            println!("{size}-byte frames one way, 1 GiB: {gbit:.1} Gbit/s");
        }
    }

    /// A region handed over by its descriptor is the same memory.
    #[test]
    fn a_region_mapped_from_its_descriptor_is_shared() {
        let a = region();
        // SAFETY: dup(2) of a descriptor the region owns.
        let fd = unsafe { OwnedFd::from_raw_fd(libc::dup(a.fd())) };
        let b = Region::map(fd).unwrap();
        let (cw, pr) = doorbell().unwrap();
        let (pw, cr) = doorbell().unwrap();
        let mut producer = a.producer(1, pr);
        let mut consumer = b.consumer(1, cr, cw);
        push(&mut producer, &pw, &[b"across"]).unwrap();
        assert_eq!(take(&mut consumer).unwrap(), b"across");
    }
}
