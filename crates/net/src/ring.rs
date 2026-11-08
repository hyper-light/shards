//! A connection's queue of bytes: a ring over memory of its own from the OS (mmap(2), or
//! VirtualAlloc on Windows), handed back as the connection lets it go (tcp.rs, `IDLE`). The
//! C library's allocator keeps what is freed for itself, dirty: 1,000 queues of 1 MiB each
//! left a process's footprint at 1,006 MiB once freed, macOS's malloc_zone_pressure_relief
//! freeing none of it (PM M210), so a queue on the heap held its memory past its
//! connection's use of it.
//!
//! Its capacity is a power of two, of whole pages, doubling as bytes come; the bytes in it
//! lie in at most two parts, the ring's end and its start, as a VecDeque's do.
//!
//! Memory let go of is kept a while for the next queue that grows to its size, whose bytes
//! then go into pages the OS has made already, as a queue on the heap's did: one made
//! afresh for each connection's queue cost bulk transfer 2.9% at p50 in zeroing its pages
//! (PM M210). What has gone unused for [`crate::tcp::IDLE`] goes back to the OS
//! ([`expire`]).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::ptr::NonNull;
use std::time::Instant;

/// Memory let go of of one size, oldest first, with when it was.
type Kept = VecDeque<(Instant, Mapping)>;

thread_local! {
    /// Memory let go of, by its size.
    static SPARE: RefCell<Vec<(usize, Kept)>> = const { RefCell::new(Vec::new()) };
}

/// The newest memory kept of `len` bytes, if any.
fn spare(len: usize) -> Option<Mapping> {
    SPARE.with_borrow_mut(|kept| {
        let (_, of_len) = kept.iter_mut().find(|(l, _)| *l == len)?;
        of_len.pop_back().map(|(_, m)| m)
    })
}

/// Keeps `m` for the next queue that grows to its size.
fn keep(m: Mapping) {
    SPARE.with_borrow_mut(|kept| {
        let now = Instant::now();
        match kept.iter_mut().find(|(l, _)| *l == m.len) {
            Some((_, of_len)) => of_len.push_back((now, m)),
            None => kept.push((m.len, VecDeque::from([(now, m)]))),
        }
    });
}

/// Hands back to the OS the memory kept that has gone unused for an RTO by `now`; says
/// when the next of it will have.
pub fn expire(now: Instant) -> Option<Instant> {
    SPARE.with_borrow_mut(|kept| {
        let mut next: Option<Instant> = None;
        for (_, of_len) in kept.iter_mut() {
            while of_len
                .front()
                .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= crate::tcp::IDLE)
            {
                of_len.pop_front();
            }
            if let Some((at, _)) = of_len.front() {
                let due = *at + crate::tcp::IDLE;
                next = Some(next.map_or(due, |n| n.min(due)));
            }
        }
        next
    })
}

/// Memory from the OS, `len` bytes, handed back as it drops.
struct Mapping {
    at: NonNull<u8>,
    len: usize,
}

// SAFETY: memory this value alone holds, and moves with it.
unsafe impl Send for Mapping {}

/// The OS's page: what a mapping is made of.
fn page() -> usize {
    static PAGE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *PAGE.get_or_init(|| {
        #[cfg(unix)]
        {
            // SAFETY: sysconf(3) of a constant name.
            usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096)
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
            let mut info = SYSTEM_INFO::default();
            // SAFETY: an info for the call to fill.
            unsafe { GetSystemInfo(&raw mut info) };
            usize::try_from(info.dwPageSize).unwrap_or(4096)
        }
    })
}

impl Mapping {
    fn new(len: usize) -> io::Result<Mapping> {
        #[cfg(unix)]
        // SAFETY: an anonymous private mapping of `len` bytes, unmapped as this drops.
        let at = unsafe {
            let at = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            );
            if at == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            at
        };
        #[cfg(windows)]
        // SAFETY: `len` bytes reserved and committed, released as this drops.
        let at = unsafe {
            use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
            VirtualAlloc(std::ptr::null(), len, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE)
        };
        let at = NonNull::new(at.cast::<u8>()).ok_or_else(io::Error::last_os_error)?;
        Ok(Mapping { at, len })
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping's `len` bytes, readable and initialized (zeroed by the OS).
        unsafe { std::slice::from_raw_parts(self.at.as_ptr(), self.len) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as `bytes`, and this value alone holds them.
        unsafe { std::slice::from_raw_parts_mut(self.at.as_ptr(), self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        #[cfg(unix)]
        // SAFETY: the mapping `new` made, of its length.
        unsafe {
            libc::munmap(self.at.as_ptr().cast(), self.len);
        }
        #[cfg(windows)]
        // SAFETY: the allocation `new` made, released whole.
        unsafe {
            use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
            VirtualFree(self.at.as_ptr().cast(), 0, MEM_RELEASE);
        }
    }
}

/// A queue of bytes over memory of its own: none until bytes come.
#[derive(Default)]
pub struct Ring {
    mem: Option<Mapping>,
    /// Where the first byte lies, and how many there are.
    head: usize,
    len: usize,
}

impl std::fmt::Debug for Ring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Ring({} of {})", self.len, self.capacity())
    }
}

impl Ring {
    pub const fn new() -> Ring {
        Ring {
            mem: None,
            head: 0,
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The memory it holds: none once let go.
    pub fn capacity(&self) -> usize {
        self.mem.as_ref().map_or(0, |m| m.len)
    }

    /// Its bytes, in order: from the first to the ring's end, then from its start.
    pub fn as_slices(&self) -> (&[u8], &[u8]) {
        let Some(mem) = &self.mem else {
            return (&[], &[]);
        };
        let bytes = mem.bytes();
        let first = self.len.min(mem.len - self.head);
        (
            bytes.get(self.head..self.head + first).unwrap_or_default(),
            bytes.get(..self.len - first).unwrap_or_default(),
        )
    }

    /// Adds `data` after its bytes, growing to the next power of two of pages they all fit
    /// in; an error where the OS gives no memory, its bytes then as they were.
    pub fn extend(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let need = self
            .len
            .checked_add(data.len())
            .ok_or(io::ErrorKind::OutOfMemory)?;
        if need > self.capacity() {
            let len = need
                .checked_next_power_of_two()
                .ok_or(io::ErrorKind::OutOfMemory)?
                .max(page());
            let mut grown = match spare(len) {
                Some(kept) => kept,
                None => Mapping::new(len)?,
            };
            let (a, b) = self.as_slices();
            let to = grown.bytes_mut();
            to.get_mut(..a.len()).unwrap_or_default().copy_from_slice(a);
            to.get_mut(a.len()..a.len() + b.len())
                .unwrap_or_default()
                .copy_from_slice(b);
            if let Some(outgrown) = self.mem.replace(grown) {
                keep(outgrown);
            }
            self.head = 0;
        }
        let Some(mem) = &mut self.mem else {
            return Err(io::ErrorKind::OutOfMemory.into());
        };
        let cap = mem.len;
        let tail = (self.head + self.len) & (cap - 1);
        let first = data.len().min(cap - tail);
        let (a, b) = data.split_at(first);
        let to = mem.bytes_mut();
        to.get_mut(tail..tail + a.len())
            .unwrap_or_default()
            .copy_from_slice(a);
        to.get_mut(..b.len()).unwrap_or_default().copy_from_slice(b);
        self.len = need;
        Ok(())
    }

    /// Takes away its first `n` bytes, or all of them where it holds fewer.
    pub fn drain(&mut self, n: usize) {
        let n = n.min(self.len);
        self.len -= n;
        self.head = match self.len {
            0 => 0,
            _ => (self.head + n) & (self.capacity() - 1),
        };
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.head = 0;
    }

    /// Lets go of its memory, and its bytes with it: kept for a queue that grows to its size
    /// until it has gone unused for an RTO ([`expire`]).
    pub fn release(&mut self) {
        self.clear();
        if let Some(m) = self.mem.take() {
            keep(m);
        }
    }

    /// A ring holding `end` at its memory's end and then `start` at its start, as a queue
    /// that has wrapped holds them: for tests of what reads one.
    #[cfg(test)]
    pub fn wrapped(end: &[u8], start: &[u8]) -> io::Result<Ring> {
        let len = end.len() + start.len();
        let mut mem = Mapping::new(len.next_power_of_two().max(page()))?;
        let head = mem.len - end.len();
        let to = mem.bytes_mut();
        to.get_mut(head..).unwrap_or_default().copy_from_slice(end);
        to.get_mut(..start.len())
            .unwrap_or_default()
            .copy_from_slice(start);
        Ok(Ring {
            mem: Some(mem),
            head,
            len,
        })
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn contents(r: &Ring) -> Vec<u8> {
        let (a, b) = r.as_slices();
        [a, b].concat()
    }

    /// Bytes come out in the order they went in, across the ring's end and as it grows,
    /// its capacity a power of two of whole pages; released, it holds nothing.
    #[test]
    fn a_ring_keeps_its_bytes_in_order_as_it_wraps_and_grows() {
        let mut r = Ring::new();
        assert_eq!(r.capacity(), 0);
        assert_eq!(r.as_slices(), (&[][..], &[][..]));
        let page = page();
        let data: Vec<u8> = (0..3 * page as u32).map(|i| (i % 251) as u8).collect();
        r.extend(&data[..page - 10]).unwrap();
        assert_eq!(r.capacity(), page);
        r.drain(page - 20);
        // Past the end: the 10 bytes left and 10 new ones before it, then the new ones
        // from its start.
        r.extend(&data[..100]).unwrap();
        assert_eq!(r.capacity(), page);
        let (a, b) = r.as_slices();
        assert_eq!((a.len(), b.len()), (20, 90));
        let mut want = data[page - 20..page - 10].to_vec();
        want.extend_from_slice(&data[..100]);
        assert_eq!(contents(&r), want);
        // Grown while wrapped: the bytes in order, from the new ring's start.
        r.extend(&data).unwrap();
        assert_eq!(r.capacity(), (120 + data.len()).next_power_of_two().max(page));
        want.extend_from_slice(&data);
        assert_eq!(contents(&r), want);
        r.drain(usize::MAX);
        assert!(r.is_empty());
        assert!(r.capacity() > 0);
        r.release();
        assert_eq!(r.capacity(), 0);
        r.extend(b"again").unwrap();
        assert_eq!(contents(&r), b"again");
    }

    /// Memory let go of goes to the next queue that grows to its size, its pages already
    /// made, and back to the OS once it has gone unused for an RTO.
    #[test]
    fn memory_let_go_of_is_taken_again_or_handed_back() {
        expire(Instant::now() + crate::tcp::IDLE * 4);
        let page = page();
        let mut a = Ring::new();
        a.extend(&vec![1u8; page]).unwrap();
        let at = a.mem.as_ref().map(|m| m.at);
        a.release();
        assert!(expire(Instant::now()).is_some(), "kept");
        let mut b = Ring::new();
        b.extend(b"next").unwrap();
        assert_eq!(b.mem.as_ref().map(|m| m.at), at, "the same pages again");
        assert_eq!(contents(&b), b"next");
        assert_eq!(expire(Instant::now()), None, "taken");
        drop(b);
        let kept = expire(Instant::now()).unwrap();
        assert!(kept > Instant::now());
        assert_eq!(expire(kept), None, "handed back once unused for an RTO");
        assert_eq!(spare(page).map(|m| m.len), None);
    }
}
