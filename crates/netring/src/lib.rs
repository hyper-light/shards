//! Frames between a VM process and its network process (docs/design/architecture.md
//! D31): two rings of bytes in memory the two share, one each way, each with a doorbell a
//! side waiting for it sleeps on (PM M83). Each frame is a record: its length, then its
//! bytes, eight-aligned; a record that would run past the end of the ring is preceded by
//! padding to the end, so a record is always whole.
//!
//! Each side treats the other as hostile, since a network process parses what the
//! Internet sends and a VM process what a guest does: every position and length read from
//! the shared memory is checked before it is used, and no reference into it is ever made,
//! only copies through raw pointers. A peer that writes nonsense spoils its own frames,
//! and is cut off, but reaches nothing of the reader's.

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
/// How many times a side checks again before it sleeps on its doorbell (PM M83).
const SPINS: u32 = 2000;

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

/// The region's size: the controls' page and two rings.
pub const SIZE: usize = CONTROL + 2 * RING;

impl Region {
    /// A new region, zeroed: shared memory that only its descriptor names.
    pub fn create() -> io::Result<Region> {
        let fd = platform::shared_memory(SIZE)?;
        Region::map(fd)
    }

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

    /// The producer end of direction `d`, ringing `doorbell` for its consumer.
    pub fn producer(&self, d: usize, doorbell: OwnedFd, waits_on: OwnedFd) -> Producer<'_> {
        let (control, ring) = self.direction(d & 1);
        Producer {
            _region: self,
            control,
            ring,
            doorbell,
            waits_on,
        }
    }

    /// The consumer end of direction `d`, ringing `doorbell` for its producer.
    pub fn consumer(&self, d: usize, doorbell: OwnedFd, waits_on: OwnedFd) -> Consumer<'_> {
        let (control, ring) = self.direction(d & 1);
        Consumer {
            _region: self,
            control,
            ring,
            doorbell,
            waits_on,
        }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `map`, unmapped once.
        unsafe { libc::munmap(self.base.cast(), self.len) };
    }
}

/// A doorbell: a pipe's two ends, the read end for the side that sleeps on it, the write
/// end for the side that wakes it. Both nonblocking: a ring already rung needs no more.
pub fn doorbell() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: an array of two descriptors.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: two descriptors just made, owned from here on.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
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

/// Waits on a doorbell until it rings or `deadline_ms` passes (-1 for ever), and drains
/// it.
fn sleep(fd: &OwnedFd, deadline_ms: i32) {
    let mut p = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd.
    unsafe { libc::poll(&mut p, 1, deadline_ms) };
    let mut buf = [0u8; 64];
    // SAFETY: a buffer of the length given, from a nonblocking descriptor we own.
    while unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
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
    /// Rung for the consumer; slept on for room.
    doorbell: OwnedFd,
    waits_on: OwnedFd,
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
    /// peer's positions cannot be believed.
    fn room(&self, n: usize) -> Result<bool, Broken> {
        let c = self.control();
        let head = c.head.0.load(Ordering::Relaxed);
        let tail = c.tail.0.load(Ordering::Acquire);
        let used = head
            .checked_sub(tail)
            .ok_or(Broken("its tail is past the head"))?;
        if used > RING as u64 {
            return Err(Broken("it consumed more than was written"));
        }
        let at = (head % RING as u64) as usize;
        let need = padded(n) + if at + padded(n) > RING { RING - at } else { 0 };
        Ok(RING as u64 - used >= need as u64)
    }

    /// Writes one frame, its `parts` gathered, waiting for room while the consumer
    /// drains; false if `parts` are longer than [`MAX_FRAME`].
    pub fn push(&mut self, parts: &[&[u8]]) -> Result<bool, Broken> {
        let n: usize = parts.iter().map(|p| p.len()).sum();
        if n > MAX_FRAME {
            return Ok(false);
        }
        let mut spins = 0u32;
        loop {
            if self.room(n)? {
                break;
            }
            spins += 1;
            if spins < SPINS {
                std::hint::spin_loop();
                continue;
            }
            let c = self.control();
            c.producer_waits.0.store(1, Ordering::SeqCst);
            if self.room(n)? {
                c.producer_waits.0.store(0, Ordering::SeqCst);
                break;
            }
            sleep(&self.waits_on, -1);
            c.producer_waits.0.store(0, Ordering::SeqCst);
            spins = 0;
        }
        let c = self.control();
        let mut head = c.head.0.load(Ordering::Relaxed);
        let mut at = (head % RING as u64) as usize;
        if at + padded(n) > RING {
            // Padding to the end, so that the record is whole.
            self.write_header(at, (RING - at - HEADER) as u32, PAD);
            head += (RING - at) as u64;
            at = 0;
        }
        self.write_header(at, n as u32, FRAME);
        let mut off = at + HEADER;
        for p in parts {
            // SAFETY: `off..off + p.len()` lies in the ring: the record fits before its end.
            unsafe { ptr::copy_nonoverlapping(p.as_ptr(), self.ring.add(off), p.len()) };
            off += p.len();
        }
        c.head.0.store(head + padded(n) as u64, Ordering::Release);
        fence(Ordering::SeqCst);
        if c.consumer_waits.0.swap(0, Ordering::SeqCst) == 1 {
            ring_bell(&self.doorbell);
        }
        Ok(true)
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

    /// Whether a frame is ready.
    pub fn ready(&self) -> Result<bool, Broken> {
        let c = self.control();
        let tail = c.tail.0.load(Ordering::Relaxed);
        let head = c.head.0.load(Ordering::Acquire);
        let used = head
            .checked_sub(tail)
            .ok_or(Broken("its head is behind the tail"))?;
        if used > RING as u64 {
            return Err(Broken("it wrote more than the ring holds"));
        }
        Ok(used > 0)
    }

    /// Asks to be rung when a frame comes, then whether one came meanwhile: if not, the
    /// caller may sleep on [`waits_on`](Self::waits_on).
    pub fn arm(&self) -> Result<bool, Broken> {
        self.control().consumer_waits.0.store(1, Ordering::SeqCst);
        let ready = self.ready()?;
        if ready {
            self.control().consumer_waits.0.store(0, Ordering::SeqCst);
        }
        Ok(ready)
    }

    /// Takes the next frame, if any, giving `take` its length and a function that copies
    /// a range of its bytes out; what `take` returns, once the frame is consumed.
    pub fn pop<R>(
        &mut self,
        take: impl FnOnce(usize, &dyn Fn(usize, *mut u8, usize)) -> R,
    ) -> Result<Option<R>, Broken> {
        if !self.ready()? {
            return Ok(None);
        }
        let c = self.control();
        let mut tail = c.tail.0.load(Ordering::Relaxed);
        let head = c.head.0.load(Ordering::Acquire);
        let mut at = (tail % RING as u64) as usize;
        let (mut len, mut kind) = self.read_header(at);
        if kind == PAD {
            let skip = RING - at;
            if len as usize != skip - HEADER || head - tail < skip as u64 {
                return Err(Broken("its padding is not to the end"));
            }
            tail += skip as u64;
            at = 0;
            if head == tail {
                return Err(Broken("padding with no frame after it"));
            }
            (len, kind) = self.read_header(at);
        }
        let n = len as usize;
        if kind != FRAME || n > MAX_FRAME || at + padded(n) > RING || head - tail < padded(n) as u64 {
            return Err(Broken("a frame's length runs past what was written"));
        }
        let start = at + HEADER;
        let ring = self.ring;
        let copy = move |from: usize, to: *mut u8, count: usize| {
            if from <= n && count <= n - from {
                // SAFETY: `start + from .. + count` lies within the frame, checked above
                // to lie within the ring; `to` is the caller's.
                unsafe { ptr::copy_nonoverlapping(ring.add(start + from), to, count) };
            }
        };
        let r = take(n, &copy);
        c.tail.0.store(tail + padded(n) as u64, Ordering::Release);
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

    /// Waits for a frame: spins, then sleeps on the doorbell, at most `ms` (-1 for ever).
    pub fn wait(&self, ms: i32) -> Result<(), Broken> {
        for _ in 0..SPINS {
            if self.ready()? {
                return Ok(());
            }
            std::hint::spin_loop();
        }
        if !self.arm()? {
            sleep(&self.waits_on, ms);
            self.control().consumer_waits.0.store(0, Ordering::SeqCst);
        }
        Ok(())
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
    /// each side, each sleeping when it must.
    #[test]
    fn frames_cross_in_order_through_every_wrap() {
        let region = Region::create().unwrap();
        let (c_wait, p_ring) = doorbell().unwrap();
        let (p_wait, c_ring) = doorbell().unwrap();
        let mut producer = region.producer(0, p_ring, p_wait);
        let mut consumer = region.consumer(0, c_ring, c_wait);
        let sizes = [0usize, 1, 7, 8, 9, 60, 1514, 9014, MAX_FRAME, 4093];
        let n = 20_000u32;
        std::thread::scope(|s| {
            s.spawn(|| {
                for i in 0..n {
                    let f = frame(i, sizes[i as usize % sizes.len()]);
                    let (a, b) = f.split_at(f.len() / 2);
                    assert!(producer.push(&[a, b]).unwrap());
                }
            });
            for i in 0..n {
                loop {
                    if let Some(f) = take(&mut consumer) {
                        assert_eq!(f, frame(i, sizes[i as usize % sizes.len()]), "frame {i}");
                        break;
                    }
                    consumer.wait(-1).unwrap();
                }
            }
        });
        assert!(take(&mut consumer).is_none());
        assert!(!producer.push(&[&vec![0u8; MAX_FRAME + 1]]).unwrap());
    }

    /// A peer that writes nonsense into the shared positions or lengths is caught, and its
    /// nonsense never leads the reader outside the ring.
    #[test]
    fn a_peer_that_breaks_the_ring_is_caught() {
        let region = Region::create().unwrap();
        let (cw, pr) = doorbell().unwrap();
        let (pw, cr) = doorbell().unwrap();
        let mut producer = region.producer(0, pr, pw);
        let mut consumer = region.consumer(0, cr, cw);
        producer.push(&[b"hello"]).unwrap();
        // A length past what was written.
        // SAFETY: the test plays the hostile peer, writing the shared header directly.
        unsafe { ptr::copy_nonoverlapping(u32::MAX.to_le_bytes().as_ptr(), consumer.ring, 4) };
        assert!(consumer.pop(|_, _| ()).is_err());
        // A head beyond the tail by more than the ring.
        consumer.control().head.0.store(u64::MAX, Ordering::SeqCst);
        assert!(consumer.ready().is_err());
        assert!(producer.push(&[b"x"]).is_err());
    }

    /// A region handed over by its descriptor is the same memory.
    #[test]
    fn a_region_mapped_from_its_descriptor_is_shared() {
        let a = Region::create().unwrap();
        // SAFETY: dup(2) of a descriptor the region owns.
        let fd = unsafe { OwnedFd::from_raw_fd(libc::dup(a.fd())) };
        let b = Region::map(fd).unwrap();
        let (cw, pr) = doorbell().unwrap();
        let (pw, cr) = doorbell().unwrap();
        let mut producer = a.producer(1, pr, pw);
        let mut consumer = b.consumer(1, cr, cw);
        producer.push(&[b"across"]).unwrap();
        assert_eq!(take(&mut consumer).unwrap(), b"across");
    }
}
