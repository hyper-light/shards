//! virtio-blk backed by a host file (virtio 1.3 §5.2), or a join disk: the ranges the
//! images of runs joining the microVM were given (D119), and a builder's cache layers
//! and the layers its steps write (D134).
//!
//! Data moves directly between the file and guest memory: the device passes raw guest
//! pointers to positional reads and writes, so no Rust reference ever aliases memory the guest
//! may modify concurrently.

use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use super::limit::Limiter;
use super::queue::{Chain, Descriptor, Queue};
use super::worker::Worker;
use super::{Activation, DeviceInterrupt, VirtioDevice, feature};
use crate::memory::{Access, GuestMemory};
use crate::snapshot::codec::{self, Reader, Writer};
use crate::{debug, platform};

pub const DEVICE_ID: u32 = 2;
const SECTOR: u64 = 512;
const QUEUE_SIZE: u16 = 256;
/// Data segments per request advertised to the driver (VIRTIO_BLK_F_SEG_MAX).
const SEG_MAX: u32 = 254;
const ID_BYTES: usize = 20;

const F_SEG_MAX: u64 = 1 << 2;
const F_RO: u64 = 1 << 5;
const F_BLK_SIZE: u64 = 1 << 6;
const F_FLUSH: u64 = 1 << 9;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

/// Where a join disk's ranges start: each on a boundary of this many bytes.
const RANGE_ALIGN: u64 = 1 << 20;

/// A disk's backing store and identity.
#[derive(Debug)]
struct Backend {
    store: Store,
    read_only: bool,
    id: [u8; ID_BYTES],
}

/// What a disk's sectors are.
#[derive(Debug)]
enum Store {
    /// One host file, of the capacity it had when it was opened, in sectors, or that it
    /// grew to as the machine ran ([`DiskSize::grow`]).
    File {
        file: File,
        capacity: AtomicU64,
        /// How many times it grew: what its configuration changed by on its own.
        grown: AtomicU32,
        /// The device's interrupt while the driver has it, to say the capacity grew, as a
        /// join disk's ([`Join`]).
        interrupt: Mutex<Option<Arc<DeviceInterrupt>>>,
    },
    /// A join disk's ranges.
    Join(&'static Join),
}

impl Backend {
    /// The disk's capacity in sectors, as the driver reads it now.
    fn capacity(&self) -> u64 {
        match &self.store {
            Store::File { capacity, .. } => capacity.load(Ordering::Acquire),
            Store::Join(join) => join.capacity(),
        }
    }
}

/// A file disk's size as its machine runs: grown online, its driver told (virtio 1.3
/// §5.2.4: the capacity may change, and the device then sends a configuration change
/// notification), as Linux's virtio_blk then reads the new capacity and says so
/// (virtblk_config_changed_work, set_capacity_and_notify: a uevent with `RESIZE=1`).
#[derive(Debug, Clone)]
pub struct DiskSize(Arc<Backend>);

impl DiskSize {
    /// Takes its file's length now as the disk's capacity, where it grew, and tells the
    /// driver: what its host made it, `set_len`, before. Returns its capacity in bytes. An
    /// error, changing nothing, where the file shrank, which would take sectors from under
    /// a guest's file system, or is not whole sectors.
    pub fn grow(&self) -> Result<u64, String> {
        let Store::File {
            file,
            capacity,
            grown,
            interrupt,
        } = &self.0.store
        else {
            return Err("a join disk grows as it is given files".into());
        };
        let bytes = file.metadata().map_err(|e| format!("its file: {e}"))?.len();
        if bytes % SECTOR != 0 {
            return Err(format!(
                "its file of {bytes} bytes: not whole sectors of {SECTOR}"
            ));
        }
        let now = capacity.load(Ordering::Acquire);
        let sectors = bytes / SECTOR;
        if sectors < now {
            return Err(format!(
                "its file shrank to {bytes} bytes from {}: a disk only grows under its guest",
                now * SECTOR
            ));
        }
        if sectors > now {
            capacity.store(sectors, Ordering::Release);
            grown.fetch_add(1, Ordering::AcqRel);
            if let Some(interrupt) = interrupt.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                interrupt.config_change();
            }
        }
        Ok(bytes)
    }

    /// Its capacity in bytes.
    pub fn bytes(&self) -> u64 {
        self.0.capacity() * SECTOR
    }
}

/// A disk whose sectors are the files its VM process is given (D119): the images of the
/// runs that joined the microVM, a builder's cache layers, and the files its steps
/// write theirs to (D134), each a range of its own from a [`RANGE_ALIGN`] boundary, in the
/// order they came; the gaps between them read as zeros. A range is written only where it
/// was given as writable; every other write is an I/O error. It is empty, of no capacity,
/// until it is given a file, and only grows: a range let go reads and writes as an I/O
/// error, and its place is never given again, so no range's offset ever names another file.
///
/// A VM process runs one VM: its join disk is a `static` of that process's, which its
/// device and the process's join requests both reach, with no count of holders kept.
#[derive(Debug)]
pub struct Join {
    ranges: RwLock<Vec<Range>>,
    /// The device's interrupt while the driver has it, to say the capacity grew: the
    /// transport's, which it makes for each activation, so a disk that outlives them holds
    /// it while one lasts, taken once as the driver activates the device and let go at its
    /// reset, never on a request's path.
    interrupt: Mutex<Option<Arc<DeviceInterrupt>>>,
    /// Whether the guest may write the ranges given writable: a builder's disk (D134). A
    /// run's join disk is read-only, and its device says so.
    writable: AtomicBool,
}

impl Default for Join {
    fn default() -> Join {
        Join::new()
    }
}

#[derive(Debug)]
struct Range {
    /// Where it starts, in bytes.
    start: u64,
    /// Its length in bytes, a whole number of sectors: its file's.
    len: u64,
    /// Its file, until it is let go.
    file: Option<File>,
    /// Whether the guest may write it.
    writable: bool,
    /// Who reads and writes its file for the guest, where not this process: a process of
    /// its builder's outside the budget that holds the builder ([`Through`]).
    through: Option<Through>,
}

/// A request to a [`Through`]'s process: read (`THROUGH_READ`) or write (`THROUGH_WRITE`)
/// what follows its 13 bytes: the operation, the offset in the range's file (8 bytes) and
/// how many bytes (4), big-endian, at most [`THROUGH_MAX`]; a write's bytes after it. Its
/// answer: an error's number (4 bytes, 0 for none) and how many bytes it read or wrote (4),
/// a read's bytes after it.
pub const THROUGH_READ: u8 = 1;
pub const THROUGH_WRITE: u8 = 2;
/// The most bytes one request moves.
pub const THROUGH_MAX: usize = 1 << 20;

/// The process that reads and writes a range's file for its guest, over a connection of
/// its own, as this process would have, from and into the guest's memory: a build held to
/// a budget has its builder's cache layers and their changes (D134) read and written by
/// its writer outside the budget (D188), so that the page cache they fault is charged to
/// the writer, where the budget's reclaim does not take it before the build's merge of
/// the cache reads it again (PM M442). Unix's: the connection is a socket.
#[derive(Debug)]
pub struct Through {
    #[cfg(unix)]
    conn: Mutex<std::os::unix::net::UnixStream>,
}

#[cfg(unix)]
impl Through {
    /// Reads and writes through the process at the other end of `conn`.
    pub fn new(conn: std::os::unix::net::UnixStream) -> Through {
        Through {
            conn: Mutex::new(conn),
        }
    }

    /// Asks for `op` of up to `len` bytes at `offset` of the file, from or into `ptr`:
    /// how many bytes it moved.
    ///
    /// # Safety
    ///
    /// `ptr .. ptr + len` lies in guest memory, as `GuestMemory::io_ptr` gives it.
    unsafe fn ask(&self, op: u8, ptr: *mut u8, len: usize, offset: u64) -> std::io::Result<usize> {
        use std::io::{Read as _, Write as _};
        use std::os::fd::AsRawFd as _;
        let n = len.min(THROUGH_MAX);
        let mut conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut head = vec![op];
        head.extend_from_slice(&offset.to_be_bytes());
        head.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_be_bytes());
        conn.write_all(&head)?;
        if op == THROUGH_WRITE {
            // SAFETY: the caller's: `ptr .. ptr + n` lies in guest memory, which write(2)
            // reads, never a Rust reference.
            unsafe { raw(conn.as_raw_fd(), ptr, n, true) }?;
        }
        let mut code = [0u8; 4];
        let mut moved = [0u8; 4];
        conn.read_exact(&mut code)?;
        conn.read_exact(&mut moved)?;
        let code = i32::from_be_bytes(code);
        let moved = u32::from_be_bytes(moved) as usize;
        if code != 0 {
            return Err(std::io::Error::from_raw_os_error(code));
        }
        if moved > n {
            return Err(std::io::Error::other(
                "a range's process moved more than it was asked",
            ));
        }
        if op == THROUGH_READ {
            // SAFETY: the caller's, `moved` within it: read(2) writes guest memory.
            unsafe { raw(conn.as_raw_fd(), ptr, moved, false) }?;
        }
        Ok(moved)
    }

    /// Asks for `op` of all `n` bytes the `iov` describe, at most [`THROUGH_MAX`], at
    /// `offset` of the file, in one round trip: a request's segments are the pages of its
    /// guest's buffer, 4 KiB each, and a round trip for each cost more than the writes: 85 MiB
    /// took 0.5 to 0.7 s longer than the same writes made here (PM M442).
    ///
    /// # Safety
    ///
    /// Each of `iov` lies in guest memory, as `GuestMemory::io_ptr` gives it.
    unsafe fn ask_all(&self, op: u8, iov: &mut [libc::iovec], n: usize, offset: u64) -> std::io::Result<()> {
        use std::io::{Read as _, Write as _};
        use std::os::fd::AsRawFd as _;
        let mut conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut head = vec![op];
        head.extend_from_slice(&offset.to_be_bytes());
        head.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_be_bytes());
        conn.write_all(&head)?;
        if op == THROUGH_WRITE {
            // SAFETY: the caller's: guest memory, which writev(2) reads.
            unsafe { raw_all(conn.as_raw_fd(), iov, true) }?;
        }
        let mut code = [0u8; 4];
        let mut moved = [0u8; 4];
        conn.read_exact(&mut code)?;
        conn.read_exact(&mut moved)?;
        let code = i32::from_be_bytes(code);
        if code != 0 {
            return Err(std::io::Error::from_raw_os_error(code));
        }
        let moved = u32::from_be_bytes(moved) as usize;
        if moved != n {
            // A read cut short (its file shorter than its range) has its bytes after the
            // answer: taken off the connection, which the next request reads from.
            if op == THROUGH_READ && moved < n {
                std::io::copy(&mut (&mut *conn).take(moved as u64), &mut std::io::sink())?;
            }
            return Err(std::io::Error::other(
                "a range's process moved other than it was asked",
            ));
        }
        if op == THROUGH_READ {
            // SAFETY: the caller's: guest memory, which readv(2) writes.
            unsafe { raw_all(conn.as_raw_fd(), iov, false) }?;
        }
        Ok(())
    }
}

/// All the bytes the `iov` describe written to `fd` (`out`), or read from it into them. The
/// `iov` are used up.
///
/// # Safety
///
/// Each of `iov` is valid memory, for reads where `out`, for writes where not.
#[cfg(unix)]
unsafe fn raw_all(fd: std::os::fd::RawFd, iov: &mut [libc::iovec], out: bool) -> std::io::Result<()> {
    let mut at = 0usize;
    while let Some(rest) = iov.get(at..).filter(|r| !r.is_empty()) {
        let count = libc::c_int::try_from(rest.len().min(1024)).unwrap_or(1);
        // SAFETY: the caller's: each of `rest` is valid.
        let n = unsafe {
            if out {
                libc::writev(fd, rest.as_ptr(), count)
            } else {
                libc::readv(fd, rest.as_ptr(), count)
            }
        };
        let mut left = match n {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n if n > 0 => n.unsigned_abs(),
            _ => {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
        };
        while let Some(v) = iov.get_mut(at) {
            if left < v.iov_len {
                // SAFETY: within the segment, `left` bytes being less than its length.
                v.iov_base = unsafe { v.iov_base.cast::<u8>().add(left) }.cast();
                v.iov_len -= left;
                break;
            }
            left -= v.iov_len;
            at += 1;
        }
    }
    Ok(())
}

/// All `len` bytes at `ptr` written to `fd` (`out`), or read from it into them.
///
/// # Safety
///
/// `ptr .. ptr + len` is valid memory, for reads where `out`, for writes where not.
#[cfg(unix)]
unsafe fn raw(fd: std::os::fd::RawFd, ptr: *mut u8, len: usize, out: bool) -> std::io::Result<()> {
    let mut done = 0usize;
    while done < len {
        // SAFETY: the caller's: `ptr + done .. ptr + len` is valid.
        let n = unsafe {
            let p = ptr.add(done).cast::<libc::c_void>();
            if out {
                libc::write(fd, p, len - done)
            } else {
                libc::read(fd, p, len - done)
            }
        };
        match n {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n if n > 0 => done += n.unsigned_abs(),
            _ => {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

impl Join {
    /// An empty join disk, for a `static`.
    pub const fn new() -> Join {
        Join {
            ranges: RwLock::new(Vec::new()),
            interrupt: Mutex::new(None),
            writable: AtomicBool::new(false),
        }
    }

    /// Lets the guest write the ranges given writable (D134): before its device is made,
    /// which then says the disk is writable.
    pub fn take_writes(&self) {
        self.writable.store(true, Ordering::Release);
    }

    /// Gives `file`, an image whose size is a whole number of sectors, a range after the
    /// last, read-only, and tells the driver the disk's new capacity. Returns where the
    /// range starts and its length, in bytes.
    pub fn attach(&self, file: File) -> Result<(u64, u64), String> {
        self.give(file, false, None)
    }

    /// Gives `file` a range as [`attach`](Self::attach) does, or one the guest may write as
    /// [`attach_writable`](Self::attach_writable) does, whose reads and writes `through`
    /// makes.
    pub fn attach_through(&self, file: File, writable: bool, through: Through) -> Result<(u64, u64), String> {
        if writable && !self.writable.load(Ordering::Acquire) {
            return Err("a writable range of a join disk that takes no writes".into());
        }
        self.give(file, writable, Some(through))
    }

    /// Gives `file`, whose size is a whole number of sectors, a range after the last that
    /// the guest may write (D134), as [`attach`](Self::attach) gives an image one: only on
    /// a disk that takes writes ([`take_writes`](Self::take_writes)).
    pub fn attach_writable(&self, file: File) -> Result<(u64, u64), String> {
        if !self.writable.load(Ordering::Acquire) {
            return Err("a writable range of a join disk that takes no writes".into());
        }
        self.give(file, true, None)
    }

    fn give(&self, file: File, writable: bool, through: Option<Through>) -> Result<(u64, u64), String> {
        let len = file
            .metadata()
            .map_err(|e| format!("a file for the join disk: {e}"))?
            .len();
        if len == 0 || len % SECTOR != 0 {
            return Err(format!(
                "a file of {len} bytes: a join disk takes whole sectors of {SECTOR}"
            ));
        }
        let start = {
            let mut ranges = self.ranges.write().unwrap_or_else(PoisonError::into_inner);
            let end = ranges.last().map_or(Some(0), |r| r.start.checked_add(r.len));
            let start = end
                .and_then(|e| e.checked_next_multiple_of(RANGE_ALIGN))
                .filter(|s| s.checked_add(len).is_some())
                .ok_or("the join disk is full")?;
            ranges.push(Range {
                start,
                len,
                file: Some(file),
                writable,
                through,
            });
            start
        };
        // Once its range is there: a driver that reads the new capacity reads the image.
        if let Some(interrupt) = &*self.interrupt.lock().unwrap_or_else(PoisonError::into_inner) {
            interrupt.config_change();
        }
        Ok((start, len))
    }

    /// Lets the range that starts at `start` go: its file is closed, and the driver's
    /// reads of it fail from here on.
    pub fn detach(&self, start: u64) -> Result<(), String> {
        let mut ranges = self.ranges.write().unwrap_or_else(PoisonError::into_inner);
        let range = ranges
            .iter_mut()
            .find(|r| r.start == start)
            .ok_or_else(|| format!("no range of the join disk starts at {start}"))?;
        range.file = None;
        range.through = None;
        Ok(())
    }

    /// Whether a range still has its file: a joined run's image the guest may still
    /// read, which a snapshot keeps nothing of (D176).
    pub fn attached(&self) -> bool {
        self.ranges
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|r| r.file.is_some())
    }

    /// Its ranges as a snapshot keeps them, each let go (D176): where each starts, its
    /// length, and whether the guest could write it. Asked once none has its file.
    fn save(&self, w: &mut Writer) {
        let ranges = self.ranges.read().unwrap_or_else(PoisonError::into_inner);
        w.seq(&ranges, |w, r| {
            w.u64(r.start);
            w.u64(r.len);
            w.bool(r.writable);
        });
    }

    /// The ranges a snapshot kept, each let go: a restored disk has the capacity its guest
    /// knows, and gives none of their places again.
    fn restore(&self, r: &mut Reader<'_>) -> codec::Result<()> {
        let kept = r.seq(u32::MAX as usize, 17, |r| {
            Ok(Range {
                start: r.u64()?,
                len: r.u64()?,
                file: None,
                writable: r.bool()?,
                through: None,
            })
        })?;
        let mut end = 0u64;
        for k in &kept {
            if k.start < end
                || !k.start.is_multiple_of(RANGE_ALIGN)
                || k.len == 0
                || !k.len.is_multiple_of(SECTOR)
            {
                return Err(codec::DecodeError(format!(
                    "a join disk's range of {} bytes at {}, after one that ends at {end}",
                    k.len, k.start
                )));
            }
            end = k.start.checked_add(k.len).ok_or_else(|| {
                codec::DecodeError(format!("a join disk's range at {} past the disk's end", k.start))
            })?;
        }
        let mut ranges = self.ranges.write().unwrap_or_else(PoisonError::into_inner);
        if !ranges.is_empty() {
            return Err(codec::DecodeError(
                "a join disk restored with ranges of its own".into(),
            ));
        }
        *ranges = kept;
        Ok(())
    }

    /// Whether no joiner's image was ever given a range.
    pub fn is_empty(&self) -> bool {
        self.ranges
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }

    /// Its capacity in sectors: its last range's end.
    fn capacity(&self) -> u64 {
        let ranges = self.ranges.read().unwrap_or_else(PoisonError::into_inner);
        ranges
            .last()
            .and_then(|r| r.start.checked_add(r.len))
            .map_or(0, |end| end / SECTOR)
    }

    /// Reads up to `len` bytes at `offset` into the guest's memory at `ptr`, which holds
    /// that many: from the range there, or zeros between ranges. Returns how many it
    /// read, or why it read none.
    ///
    /// # Safety
    ///
    /// `ptr .. ptr + len` lies in guest memory, as `GuestMemory::io_ptr` gives it.
    unsafe fn read_at(&self, ptr: *mut u8, len: usize, offset: u64) -> Result<usize, u8> {
        let ranges = self.ranges.read().unwrap_or_else(PoisonError::into_inner);
        // The last range that starts at or before `offset`.
        let at = ranges.partition_point(|r| r.start <= offset);
        let next = ranges.get(at).map(|r| r.start);
        if let Some(r) = at.checked_sub(1).and_then(|i| ranges.get(i))
            && offset < r.start.saturating_add(r.len)
        {
            let file = r.file.as_ref().ok_or(S_IOERR)?;
            let left = r.start.saturating_add(r.len) - offset;
            let n = usize::try_from(left).map_or(len, |left| left.min(len));
            #[cfg(unix)]
            if let Some(through) = &r.through {
                // SAFETY: the caller's: `ptr .. ptr + n` lies in guest memory.
                return match unsafe { through.ask(THROUGH_READ, ptr, n, offset - r.start) } {
                    Ok(0) | Err(_) => Err(S_IOERR),
                    Ok(n) => Ok(n),
                };
            }
            // SAFETY: the caller's: `ptr .. ptr + n` lies in guest memory, which the
            // kernel's read reaches.
            return match unsafe { platform::read_at(file, ptr, n, offset - r.start) } {
                Ok(0) | Err(_) => Err(S_IOERR),
                Ok(n) => Ok(n),
            };
        }
        // Between ranges, or before the first: zeros, up to the next range.
        let gap = next.map_or(len, |s| usize::try_from(s - offset).map_or(len, |g| g.min(len)));
        // SAFETY: the caller's: `ptr .. ptr + gap` lies in guest memory.
        unsafe { zeros(ptr, gap) }?;
        Ok(gap)
    }

    /// Whether the range at `offset` is read and written through a [`Through`].
    #[cfg(unix)]
    fn goes_through(&self, offset: u64) -> bool {
        let ranges = self.ranges.read().unwrap_or_else(PoisonError::into_inner);
        let at = ranges.partition_point(|r| r.start <= offset);
        at.checked_sub(1)
            .and_then(|i| ranges.get(i))
            .is_some_and(|r| offset < r.start.saturating_add(r.len) && r.through.is_some())
    }

    /// One request's `segments`, guest memory, read or written at `offset` through its
    /// range's [`Through`], in round trips of at most [`THROUGH_MAX`]; none (`None`) where
    /// they are not all of one such range, and its segments go one at a time.
    ///
    /// # Safety
    ///
    /// Each of `segments` lies in guest memory, as `GuestMemory::io_ptr` gives it.
    #[cfg(unix)]
    unsafe fn through_request(
        &self,
        segments: &[(*mut u8, usize)],
        offset: u64,
        write: bool,
    ) -> Option<Result<(), u8>> {
        let ranges = self.ranges.read().unwrap_or_else(PoisonError::into_inner);
        let at = ranges.partition_point(|r| r.start <= offset);
        let r = at.checked_sub(1).and_then(|i| ranges.get(i))?;
        let through = r.through.as_ref()?;
        r.file.as_ref()?;
        let total = segments.iter().map(|&(_, len)| len as u64).sum::<u64>();
        if offset.checked_add(total)? > r.start.saturating_add(r.len) || (write && !r.writable) {
            return None;
        }
        let op = if write { THROUGH_WRITE } else { THROUGH_READ };
        let mut at = offset - r.start;
        let mut iov = Vec::with_capacity(segments.len());
        let mut gathered = 0usize;
        for &(ptr, len) in segments {
            let mut done = 0usize;
            while done < len {
                let take = (len - done).min(THROUGH_MAX - gathered);
                iov.push(libc::iovec {
                    // SAFETY: the caller's: `ptr + done` lies in the segment.
                    iov_base: unsafe { ptr.add(done) }.cast(),
                    iov_len: take,
                });
                done += take;
                gathered += take;
                if gathered == THROUGH_MAX {
                    // SAFETY: the caller's: each of `iov` lies in guest memory.
                    if unsafe { through.ask_all(op, &mut iov, gathered, at) }.is_err() {
                        return Some(Err(S_IOERR));
                    }
                    at += gathered as u64;
                    gathered = 0;
                    iov.clear();
                }
            }
        }
        // SAFETY: the caller's: each of `iov` lies in guest memory.
        if gathered > 0 && unsafe { through.ask_all(op, &mut iov, gathered, at) }.is_err() {
            return Some(Err(S_IOERR));
        }
        Some(Ok(()))
    }

    /// Writes up to `len` bytes from the guest's memory at `ptr` at `offset`: into the
    /// writable range there, within it. Returns how many it wrote, or why it wrote none: a
    /// range given read-only, let go, or none at all.
    ///
    /// # Safety
    ///
    /// `ptr .. ptr + len` lies in guest memory, as `GuestMemory::io_ptr` gives it.
    unsafe fn write_at(&self, ptr: *const u8, len: usize, offset: u64) -> Result<usize, u8> {
        let ranges = self.ranges.read().unwrap_or_else(PoisonError::into_inner);
        let at = ranges.partition_point(|r| r.start <= offset);
        let r = at
            .checked_sub(1)
            .and_then(|i| ranges.get(i))
            .filter(|r| r.writable && offset < r.start.saturating_add(r.len))
            .ok_or(S_IOERR)?;
        let file = r.file.as_ref().ok_or(S_IOERR)?;
        let left = r.start.saturating_add(r.len) - offset;
        let n = usize::try_from(left).map_or(len, |left| left.min(len));
        #[cfg(unix)]
        if let Some(through) = &r.through {
            // SAFETY: the caller's: `ptr .. ptr + n` lies in guest memory, which write(2)
            // here copies from; the process at the other end never reaches it.
            return match unsafe { through.ask(THROUGH_WRITE, ptr.cast_mut(), n, offset - r.start) } {
                Ok(0) | Err(_) => Err(S_IOERR),
                Ok(n) => Ok(n),
            };
        }
        // SAFETY: the caller's: `ptr .. ptr + n` lies in guest memory, which the kernel's
        // write reaches.
        match unsafe { platform::write_at(file, ptr, n, offset - r.start) } {
            Ok(0) | Err(_) => Err(S_IOERR),
            Ok(n) => Ok(n),
        }
    }
}

/// The worker thread returns its queue when stopped, or nothing if a malformed ring
/// failed the device.
pub struct Block {
    backend: Arc<Backend>,
    /// What a worker needs besides its queue, from activation until reset.
    context: Option<(Arc<GuestMemory>, Arc<DeviceInterrupt>)>,
    worker: Option<Worker>,
    /// The queue while paused.
    paused: Option<Queue>,
    /// Its microVM's I/O limits (D178), and whether this device holds them paused.
    limit: Option<&'static Limiter>,
    holds_limit: bool,
}

impl std::fmt::Debug for Block {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Block")
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

impl Block {
    /// Opens `path` as a disk. Its size must be a whole number of 512-byte sectors.
    pub fn open(path: &Path, read_only: bool, id: &str) -> Result<Block, String> {
        let file =
            crate::platform::open_input(path, !read_only).map_err(|e| format!("{}: {e}", path.display()))?;
        Block::of_file(file, read_only, id).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// `file` as a disk, opened for writing unless `read_only`: a disk plugged into a
    /// running machine (D214), whose VM process is given its file open. Its size must be a
    /// whole number of 512-byte sectors.
    pub fn of_file(file: File, read_only: bool, id: &str) -> Result<Block, String> {
        let bytes = file.metadata().map_err(|e| e.to_string())?.len();
        if bytes % SECTOR != 0 {
            return Err(format!("size {bytes} is not a multiple of {SECTOR}"));
        }
        Ok(Block::of(Backend {
            store: Store::File {
                file,
                capacity: AtomicU64::new(bytes / SECTOR),
                grown: AtomicU32::new(0),
                interrupt: Mutex::new(None),
            },
            read_only,
            id: serial(id),
        }))
    }

    /// What grows it online, for a file disk; none for a join disk.
    pub fn size(&self) -> Option<DiskSize> {
        matches!(self.backend.store, Store::File { .. }).then(|| DiskSize(self.backend.clone()))
    }

    /// A join disk (D119), its serial `id`: empty until `disk` is given a range, and
    /// read-only unless it takes writes (D134).
    pub fn join(disk: &'static Join, id: &str) -> Block {
        Block::of(Backend {
            store: Store::Join(disk),
            read_only: !disk.writable.load(Ordering::Acquire),
            id: serial(id),
        })
    }

    /// Its reads and writes charged to its microVM's I/O limits (D178).
    pub fn with_limit(mut self, limit: Option<&'static Limiter>) -> Block {
        self.limit = limit;
        self
    }

    /// Lets go of its microVM's limits, where its pause holds them.
    fn release_limit(&mut self) {
        if let Some(limit) = self.limit.filter(|_| self.holds_limit) {
            limit.resume();
            self.holds_limit = false;
        }
    }

    fn of(backend: Backend) -> Block {
        Block {
            backend: Arc::new(backend),
            context: None,
            worker: None,
            paused: None,
            limit: None,
            holds_limit: false,
        }
    }

    /// Starts the worker on `queue`; if it cannot, the queue waits paused, as a snapshot's
    /// pause leaves it, for a resume or a reset.
    fn start(&mut self, queue: Queue) -> Result<(), String> {
        let Some((memory, interrupt)) = self.context.clone() else {
            self.paused = Some(queue);
            return Err("virtio-blk started before activation".into());
        };
        let (backend, mem, limit) = (self.backend.clone(), memory.clone(), self.limit);
        let answer = move |chain: &Chain| handle(chain, &mem, &backend, limit);
        match Worker::start("virtio-blk", queue, memory, interrupt, answer) {
            Ok(worker) => {
                self.worker = Some(worker);
                Ok(())
            }
            Err((e, queue)) => {
                self.paused = Some(queue);
                Err(e)
            }
        }
    }

    /// Stops the worker after the request it is executing; returns its queue.
    fn stop(&mut self) -> Option<Queue> {
        self.worker.take()?.stop()
    }

    /// The device's interrupt, given at activation and taken at reset: who tells the
    /// driver its capacity grew.
    fn join_interrupt(&self, interrupt: Option<Arc<DeviceInterrupt>>) {
        let held = match &self.backend.store {
            Store::Join(join) => &join.interrupt,
            Store::File { interrupt, .. } => interrupt,
        };
        *held.lock().unwrap_or_else(PoisonError::into_inner) = interrupt;
    }
}

/// `id` as a virtio-blk serial: its first [`ID_BYTES`] bytes, NUL-padded.
fn serial(id: &str) -> [u8; ID_BYTES] {
    let mut serial = [0u8; ID_BYTES];
    for (d, s) in serial.iter_mut().zip(id.bytes()) {
        *d = s;
    }
    serial
}

impl VirtioDevice for Block {
    fn device_id(&self) -> u32 {
        DEVICE_ID
    }

    fn features(&self) -> u64 {
        let ro = if self.backend.read_only { F_RO } else { 0 };
        feature::VERSION_1
            | feature::EVENT_IDX
            | feature::INDIRECT_DESC
            | F_SEG_MAX
            | F_BLK_SIZE
            | F_FLUSH
            | ro
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // struct virtio_blk_config (§5.2.4): capacity @0, size_max @8, seg_max @12,
        // geometry @16, blk_size @20; everything else reads as zero.
        let mut config = [0u8; 24];
        config[0..8].copy_from_slice(&self.backend.capacity().to_le_bytes());
        config[12..16].copy_from_slice(&SEG_MAX.to_le_bytes());
        config[20..24].copy_from_slice(&(SECTOR as u32).to_le_bytes());
        let start = offset as usize;
        for (i, b) in data.iter_mut().enumerate() {
            *b = start
                .checked_add(i)
                .and_then(|at| config.get(at))
                .copied()
                .unwrap_or(0);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    /// A file disk's growths (virtio 1.3 §4.2.2.1): a driver reading its capacity in two
    /// accesses sees whether it grew between them.
    fn config_generation(&self) -> u32 {
        match &self.backend.store {
            Store::File { grown, .. } => grown.load(Ordering::Acquire),
            Store::Join(_) => 0,
        }
    }

    fn activate(&mut self, activation: Activation) -> Result<(), String> {
        let Activation {
            memory,
            mut queues,
            interrupt,
            ..
        } = activation;
        let queue = queues.pop().ok_or("virtio-blk activated without a queue")?;
        self.join_interrupt(Some(interrupt.clone()));
        self.context = Some((memory, interrupt));
        // An activation that fails leaves its queue: the driver sets the device up anew.
        self.start(queue).inspect_err(|_| self.paused = None)
    }

    fn notify(&self, _queue: u16) {
        if let Some(w) = &self.worker {
            w.notify();
        }
    }

    fn reset(&mut self) {
        self.stop();
        self.paused = None;
        self.context = None;
        self.join_interrupt(None);
        self.release_limit();
    }

    /// The worker stops after the request it is answering. Whatever the driver published
    /// and it did not answer stays in the ring, for the worker that resumes.
    fn pause(&mut self) -> Vec<super::QueueState> {
        // Its limits paused first (D178): a worker waiting on them answers the request in
        // hand at once, and stops after it.
        if let Some(limit) = self.limit.filter(|_| !self.holds_limit) {
            limit.pause();
            self.holds_limit = true;
        }
        if let Some(queue) = self.stop() {
            self.paused = Some(queue);
        }
        self.paused.iter().map(Queue::state).collect()
    }

    /// The new worker drains first, so requests published meanwhile are served.
    fn resume(&mut self) -> Result<(), String> {
        self.release_limit();
        match self.paused.take() {
            Some(queue) => self.start(queue),
            None => Ok(()),
        }
    }

    /// A join disk's ranges, each let go (D176); nothing of any other disk's.
    fn save(&self, w: &mut Writer) {
        if let Store::Join(join) = &self.backend.store {
            join.save(w);
        }
    }

    fn restore(&mut self, r: &mut Reader<'_>) -> codec::Result<()> {
        match &self.backend.store {
            Store::Join(join) => join.restore(r),
            _ => Ok(()),
        }
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        self.reset();
    }
}

/// Executes one request; returns the bytes written into the driver's buffers.
fn handle(chain: &Chain, mem: &GuestMemory, backend: &Backend, limit: Option<&Limiter>) -> u32 {
    // The status byte is the last byte of the last writable descriptor.
    let Some(last) = chain.writable().last().copied() else {
        debug!("virtio-blk: request without a status byte");
        return 0;
    };
    if last.len == 0 {
        return 0;
    }
    let Some(status_at) = last.addr.checked_add(u64::from(last.len) - 1) else {
        return 0;
    };
    let (status, data_written) = match execute(chain, last, mem, backend, limit) {
        Ok(n) => (S_OK, n),
        Err(status) => (status, 0),
    };
    let stored = mem.access().map(|a| a.write_obj(status_at, status));
    if !matches!(stored, Ok(Ok(()))) {
        return 0;
    }
    data_written.saturating_add(1)
}

/// Returns bytes of data written into guest buffers, or a status code on failure.
fn execute(
    chain: &Chain,
    status: Descriptor,
    mem: &GuestMemory,
    backend: &Backend,
    limit: Option<&Limiter>,
) -> Result<u32, u8> {
    let mut header = [0u8; 16];
    let body = gather_header(chain, &mem.access().map_err(|_| S_IOERR)?, &mut header)?;
    let kind = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let sector = u64::from_le_bytes([
        header[8], header[9], header[10], header[11], header[12], header[13], header[14], header[15],
    ]);
    // Writable buffers except the trailing status byte.
    let out: Vec<Descriptor> = writable_data(chain, status);
    // A read's or a write's bytes, charged to its microVM's I/O limits first (D178).
    if let Some(limit) = limit
        && matches!(kind, T_IN | T_OUT)
    {
        let data = if kind == T_IN { &out } else { &body };
        limit.take(data.iter().map(|d| u64::from(d.len)).sum());
    }
    match kind {
        T_IN => transfer(&out, sector, mem, backend, Direction::Read),
        T_OUT if backend.read_only => Err(S_IOERR),
        T_OUT => transfer(&body, sector, mem, backend, Direction::Write).map(|_| 0),
        // Durable on stable storage, not just in the host page cache (platform::sync_durable).
        // A join disk's ranges are images and the files of a build's steps, which the host
        // reads back as they are: nothing to make durable.
        T_FLUSH => match &backend.store {
            Store::File { file, .. } => platform::sync_durable(file).map(|()| 0).map_err(|_| S_IOERR),
            Store::Join(_) => Ok(0),
        },
        T_GET_ID => {
            let mut written = 0u32;
            let mut id = backend.id.as_slice();
            let a = mem.access().map_err(|_| S_IOERR)?;
            for d in &out {
                let n = id.len().min(d.len as usize);
                let (chunk, rest) = id.split_at(n);
                a.write(d.addr, chunk).map_err(|_| S_IOERR)?;
                written += n as u32;
                id = rest;
            }
            Ok(written)
        }
        _ => Err(S_UNSUPP),
    }
}

/// Copies the 16-byte request header out of the readable descriptors and returns the
/// readable data that follows it (framing is not assumed, per VIRTIO 1.x).
fn gather_header(chain: &Chain, mem: &Access<'_>, header: &mut [u8; 16]) -> Result<Vec<Descriptor>, u8> {
    let mut filled = 0usize;
    let mut body = Vec::new();
    for d in chain.readable() {
        let take = (header.len() - filled).min(d.len as usize);
        if take > 0 {
            let dst = header.get_mut(filled..filled + take).ok_or(S_IOERR)?;
            mem.read(d.addr, dst).map_err(|_| S_IOERR)?;
            filled += take;
        }
        let rest = d.len - take as u32;
        if rest > 0 {
            body.push(Descriptor {
                addr: d.addr.checked_add(take as u64).ok_or(S_IOERR)?,
                len: rest,
                writable: false,
            });
        }
    }
    if filled == header.len() {
        Ok(body)
    } else {
        Err(S_IOERR)
    }
}

fn writable_data(chain: &Chain, status: Descriptor) -> Vec<Descriptor> {
    let mut out: Vec<Descriptor> = chain.writable().copied().collect();
    if let Some(last) = out.last_mut()
        && *last == status
    {
        last.len -= 1;
    }
    out.retain(|d| d.len > 0);
    out
}

#[derive(Clone, Copy)]
enum Direction {
    Read,
    Write,
}

/// Writes `len` zeros at `ptr`, in guest memory as `GuestMemory::io_ptr` gives it: by
/// [`platform::copy_checked`], which a page of device memory that is gone fails, not the
/// VMM's process.
///
/// # Safety
/// `ptr .. ptr + len` lies in guest memory, as `GuestMemory::io_ptr` gives it.
unsafe fn zeros(ptr: *mut u8, len: usize) -> Result<(), u8> {
    static ZEROS: [u8; 4096] = [0; 4096];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(ZEROS.len());
        // SAFETY: the caller's: `ptr + done .. ptr + done + n` lies in guest memory, apart
        // from `ZEROS`.
        unsafe { platform::copy_checked(ZEROS.as_ptr(), ptr.add(done), n) }.map_err(|_| S_IOERR)?;
        done += n;
    }
    Ok(())
}

fn transfer(
    bufs: &[Descriptor],
    sector: u64,
    mem: &GuestMemory,
    backend: &Backend,
    dir: Direction,
) -> Result<u32, u8> {
    let total: u64 = bufs.iter().map(|d| u64::from(d.len)).sum();
    let start = sector.checked_mul(SECTOR).ok_or(S_IOERR)?;
    let end = start.checked_add(total).ok_or(S_IOERR)?;
    if end > backend.capacity().saturating_mul(SECTOR) {
        return Err(S_IOERR);
    }
    #[cfg(unix)]
    if let Store::Join(join) = &backend.store
        && join.goes_through(start)
    {
        let segments = bufs
            .iter()
            .map(|d| {
                Ok((
                    mem.io_ptr(d.addr, d.len as usize).map_err(|_| S_IOERR)?,
                    d.len as usize,
                ))
            })
            .collect::<Result<Vec<_>, u8>>()?;
        // SAFETY: each lies inside guest memory (io_ptr checked it), which the kernel reads
        // or writes, with no Rust reference to it.
        if let Some(done) = unsafe { join.through_request(&segments, start, matches!(dir, Direction::Write)) }
        {
            done?;
            return u32::try_from(total).map_err(|_| S_IOERR);
        }
    }
    let mut offset = start;
    for d in bufs {
        // Guest RAM, or device memory: a page of a file the guest maps (D189), which a
        // system call reaches as RAM, and answers EFAULT where its page is gone.
        let ptr = mem.io_ptr(d.addr, d.len as usize).map_err(|_| S_IOERR)?;
        let mut done = 0usize;
        while done < d.len as usize {
            let remaining = d.len as usize - done;
            // SAFETY: `ptr + done .. ptr + len` lies inside guest memory (io_ptr checked
            // the whole descriptor); the kernel copies to/from it without Rust aliasing.
            let n = unsafe {
                let p = ptr.add(done);
                match (&backend.store, dir) {
                    (Store::File { file, .. }, Direction::Read) => {
                        platform::read_at(file, p, remaining, offset).map_err(|_| S_IOERR)
                    }
                    (Store::File { file, .. }, Direction::Write) => {
                        platform::write_at(file, p, remaining, offset).map_err(|_| S_IOERR)
                    }
                    (Store::Join(join), Direction::Read) => join.read_at(p, remaining, offset),
                    (Store::Join(join), Direction::Write) => join.write_at(p, remaining, offset),
                }
            };
            match n {
                Ok(0) | Err(_) => return Err(S_IOERR), // 0: unexpected end of file
                Ok(n) => {
                    done += n;
                    offset += n as u64;
                }
            }
        }
    }
    u32::try_from(total).map_err(|_| S_IOERR)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::super::queue::QueueConfig;
    use super::super::worker;
    use super::*;

    struct Line;
    impl crate::devices::Interrupt for Line {
        fn set_level(&self, _: bool) {}
    }

    /// An activation, and a resume after a snapshot's pause, whose worker the system
    /// refuses fail alone: the paused queue stays, and the next activation or resume runs
    /// (review 1.14). Block's and pmem's workers are one.
    #[test]
    fn a_refused_worker_leaves_the_queue_paused() {
        let path_dir = shards_testdir::TempDir::new("blk-refused").unwrap();
        let path = path_dir.join("blk-refused");
        std::fs::write(&path, vec![0u8; 8 * 512]).unwrap();
        let mut b = Block::open(&path, true, "disk0").unwrap();
        const BASE: u64 = 0x8000_0000;
        let mem = Arc::new(GuestMemory::anonymous(&[(BASE, 1 << 16)]).unwrap());
        let irq = Arc::new(DeviceInterrupt::new(Arc::new(Line)));
        let activation = || {
            let cfg = QueueConfig {
                size: 8,
                desc: BASE,
                avail: BASE + 0x1000,
                used: BASE + 0x2000,
                ready: true,
            };
            Activation {
                memory: mem.clone(),
                queues: vec![Queue::new(cfg, 8, &mem, feature::VERSION_1).unwrap()],
                interrupt: irq.clone(),
                features: feature::VERSION_1,
                restored: false,
            }
        };
        worker::REFUSE.set(true);
        let refused = b.activate(activation());
        worker::REFUSE.set(false);
        assert!(refused.unwrap_err().starts_with("spawning the virtio-blk worker"));
        // Its queue gone with it.
        assert!(b.worker.is_none() && b.paused.is_none());
        b.activate(activation()).unwrap();
        assert_eq!(b.pause().len(), 1);
        worker::REFUSE.set(true);
        assert!(b.resume().is_err());
        worker::REFUSE.set(false);
        assert!(b.worker.is_none());
        assert_eq!(b.pause().len(), 1, "paused still");
        b.resume().unwrap();
        assert!(b.worker.is_some() && b.paused.is_none());
        drop(b);
        let _ = std::fs::remove_file(&path);
    }

    /// A join disk (D119): of no capacity until an image is given a range; each range from
    /// a 1 MiB boundary, read as its file reads, the gaps as zeros, one let go as an I/O
    /// error; the driver told each time the capacity grows, while it has the device.
    #[test]
    fn a_join_disk_reads_each_image_at_its_range() {
        let image = |name: &str, len: usize, byte: u8| {
            let path_dir = shards_testdir::TempDir::new(&format!("join-{name}")).unwrap();
            let path = path_dir.join("it");
            std::fs::write(&path, vec![byte; len]).unwrap();
            let file = File::open(&path).unwrap();
            std::fs::remove_file(&path).unwrap();
            file
        };
        static DISK: Join = Join::new();
        let disk = &DISK;
        let mut b = Block::join(disk, "shards-join");
        let capacity = |b: &Block| {
            let mut c = [0u8; 8];
            b.read_config(0, &mut c);
            u64::from_le_bytes(c)
        };
        assert!(disk.is_empty());
        assert_eq!(capacity(&b), 0);
        assert_ne!(b.features() & F_RO, 0);
        const BASE: u64 = 0x8000_0000;
        let mem = Arc::new(GuestMemory::anonymous(&[(BASE, 1 << 16)]).unwrap());
        let irq = Arc::new(DeviceInterrupt::new(Arc::new(Line)));
        let cfg = QueueConfig {
            size: 8,
            desc: BASE,
            avail: BASE + 0x1000,
            used: BASE + 0x2000,
            ready: true,
        };
        b.activate(Activation {
            memory: mem.clone(),
            queues: vec![Queue::new(cfg, 8, &mem, feature::VERSION_1).unwrap()],
            interrupt: irq.clone(),
            features: feature::VERSION_1,
            restored: false,
        })
        .unwrap();
        assert_eq!(disk.attach(image("a", 4096, 0xaa)).unwrap(), (0, 4096));
        assert_eq!(capacity(&b), 8);
        assert_ne!(irq.status() & super::super::INT_CONFIG_CHANGE, 0);
        irq.ack(super::super::INT_CONFIG_CHANGE);
        assert_eq!(disk.attach(image("b", 8192, 0xbb)).unwrap(), (1 << 20, 8192));
        assert_eq!(capacity(&b), ((1 << 20) + 8192) / SECTOR);
        assert_ne!(irq.status() & super::super::INT_CONFIG_CHANGE, 0);
        assert!(!disk.is_empty());
        let read = |at: u64, len: usize| {
            let mut buf = vec![0x55u8; len];
            // SAFETY: `buf` holds `len` bytes.
            let n = unsafe { disk.read_at(buf.as_mut_ptr(), len, at) };
            n.map(|n| buf[..n].to_vec())
        };
        assert_eq!(read(0, 4096).unwrap(), vec![0xaa; 4096]);
        // Past a's end, zeros up to b's start, never past it.
        assert_eq!(read(4096, 4096).unwrap(), vec![0; 4096]);
        assert_eq!(read((1 << 20) - 512, 1024).unwrap(), vec![0; 512]);
        assert_eq!(read(1 << 20, 8192).unwrap(), vec![0xbb; 8192]);
        assert_eq!(read((1 << 20) + 4096, 8192).unwrap(), vec![0xbb; 4096]);
        disk.detach(0).unwrap();
        assert_eq!(read(0, 512), Err(S_IOERR));
        assert!(disk.detach(512).is_err());
        // A range let go keeps its place: the next comes after the last.
        assert_eq!(disk.attach(image("c", 512, 0xcc)).unwrap(), (2 << 20, 512));
        for bad in [0, 100] {
            assert!(disk.attach(image("bad", bad, 0)).is_err());
        }
        // Reset: the driver no longer has the device, and is told nothing.
        b.reset();
        irq.ack(super::super::INT_CONFIG_CHANGE);
        disk.attach(image("d", 512, 0xdd)).unwrap();
        assert_eq!(irq.status() & super::super::INT_CONFIG_CHANGE, 0);
        // A snapshot keeps its ranges once each is let go (D176), and a restored disk has the
        // capacity its guest knew, reads none of them, and gives none of their places again.
        assert!(disk.attached());
        for start in [1 << 20, 2 << 20, 3 << 20] {
            disk.detach(start).unwrap();
        }
        assert!(!disk.attached());
        let mut w = Writer::default();
        b.save(&mut w);
        let saved = w.into_bytes();
        static RESTORED: Join = Join::new();
        let mut again = Block::join(&RESTORED, "shards-join");
        let mut r = Reader::new(&saved);
        again.restore(&mut r).unwrap();
        r.finish().unwrap();
        assert_eq!(capacity(&again), capacity(&b));
        let mut buf = vec![0u8; 512];
        // SAFETY: `buf` holds 512 bytes.
        let read = unsafe { RESTORED.read_at(buf.as_mut_ptr(), 512, 0) };
        assert_eq!(read, Err(S_IOERR));
        assert_eq!(RESTORED.attach(image("e", 512, 0xee)).unwrap(), (4 << 20, 512));
        // Only into an empty disk, and only of ranges in order.
        let mut r = Reader::new(&saved);
        assert!(again.restore(&mut r).is_err());
        let mut w = Writer::default();
        w.seq(&[(2u64 << 20, 512u64), (1 << 20, 512)], |w, (start, len)| {
            w.u64(*start);
            w.u64(*len);
            w.bool(false);
        });
        static DISORDERED: Join = Join::new();
        let mut bad = Block::join(&DISORDERED, "shards-join");
        assert!(bad.restore(&mut Reader::new(&w.into_bytes())).is_err());
    }

    /// A builder's join disk (D134): writes land in a range given as writable alone, and
    /// within it, at its file's offsets, which the host reads back; a read-only range, a
    /// gap, a range let go, and past a range's end are I/O errors; the device says it is
    /// writable.
    #[test]
    fn a_join_disk_writes_only_its_writable_ranges() {
        let dir = shards_testdir::TempDir::new("join-writable").unwrap();
        let file = |name: &str, len: usize, byte: u8| {
            let path = dir.join(name);
            std::fs::write(&path, vec![byte; len]).unwrap();
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap()
        };
        static DISK: Join = Join::new();
        let disk = &DISK;
        assert!(
            disk.attach_writable(file("refused", 512, 0)).is_err(),
            "no writes taken yet"
        );
        disk.take_writes();
        let b = Block::join(disk, "shards-join");
        assert_eq!(b.features() & F_RO, 0);
        assert_eq!(disk.attach(file("layer", 4096, 0xaa)).unwrap(), (0, 4096));
        let (out_at, out_len) = disk.attach_writable(file("out", 8192, 0)).unwrap();
        assert_eq!((out_at, out_len), (1 << 20, 8192));
        let write = |at: u64, bytes: &[u8]| {
            // SAFETY: `bytes` holds its length.
            unsafe { disk.write_at(bytes.as_ptr(), bytes.len(), at) }
        };
        assert_eq!(write(out_at + 512, &[0x5a; 1024]), Ok(1024));
        // Only to the range's end.
        assert_eq!(write(out_at + 8192 - 512, &[0x77; 1024]), Ok(512));
        assert_eq!(write(0, &[1; 512]), Err(S_IOERR), "a read-only range");
        assert_eq!(write(8192, &[1; 512]), Err(S_IOERR), "a gap");
        assert_eq!(write(out_at + 8192, &[1; 512]), Err(S_IOERR), "past its end");
        let mut back = vec![0u8; 8192];
        std::io::Read::read_exact(&mut File::open(dir.join("out")).unwrap(), &mut back).unwrap();
        assert_eq!(back[..512], [0; 512]);
        assert_eq!(back[512..1536], [0x5a; 1024]);
        assert_eq!(back[8192 - 512..], [0x77; 512]);
        assert_eq!(
            std::fs::read(dir.join("layer")).unwrap(),
            vec![0xaa; 4096],
            "the read-only range's file unwritten"
        );
        disk.detach(out_at).unwrap();
        assert_eq!(write(out_at, &[1; 512]), Err(S_IOERR), "let go");
    }

    /// A request to a range given a [`Through`] goes to its process whole, in round trips
    /// of at most [`THROUGH_MAX`] however its segments fall (PM M442): its bytes written
    /// and read back as they were. A read its process cuts short fails alone, the next
    /// request on the connection answered as asked.
    #[cfg(unix)]
    #[test]
    fn a_through_range_moves_a_request_whole() {
        use std::io::{Read as _, Write as _};
        use std::os::unix::fs::FileExt as _;
        let dir = shards_testdir::TempDir::new("join-through").unwrap();
        let path = dir.join("cache");
        const LEN: usize = 3 << 20;
        std::fs::write(&path, vec![0u8; LEN]).unwrap();
        let open = || {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap()
        };
        let (mine, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        // The range's process, as the writer's serve_range answers.
        let served = open();
        let trips = std::thread::spawn(move || {
            let mut conn = theirs;
            let mut trips = Vec::new();
            let mut head = [0u8; 13];
            while conn.read_exact(&mut head).is_ok() {
                let offset = u64::from_be_bytes(head[1..9].try_into().unwrap());
                let n = u32::from_be_bytes(head[9..].try_into().unwrap()) as usize;
                trips.push(n);
                let mut buf = vec![0u8; n];
                let moved = if head[0] == THROUGH_WRITE {
                    conn.read_exact(&mut buf).unwrap();
                    served.write_all_at(&buf, offset).unwrap();
                    n
                } else {
                    served.read_at(&mut buf, offset).unwrap()
                };
                let mut answer = 0i32.to_be_bytes().to_vec();
                answer.extend_from_slice(&(moved as u32).to_be_bytes());
                if head[0] == THROUGH_READ {
                    answer.extend_from_slice(&buf[..moved]);
                }
                conn.write_all(&answer).unwrap();
            }
            trips
        });
        let disk = Join::new();
        disk.take_writes();
        let (at, len) = disk.attach_through(open(), true, Through::new(mine)).unwrap();
        assert_eq!(len, LEN as u64);
        // 2.5 MiB in segments of 4 KiB and one of 3 KiB, the first written 512 bytes in.
        let mut out: Vec<u8> = (0..(5 << 19)).map(|i: usize| (i * 7 + i / 4096) as u8).collect();
        let mut cuts: Vec<usize> = (0..out.len() / 4096).map(|_| 4096).collect();
        cuts[0] = 1024;
        cuts.push(3072);
        let segments = |buf: &mut [u8]| {
            let mut segs = Vec::new();
            let mut ptr = buf.as_mut_ptr();
            for &cut in &cuts {
                segs.push((ptr, cut));
                // SAFETY: the cuts lie within the buffer, summing to its length.
                ptr = unsafe { ptr.add(cut) };
            }
            segs
        };
        let total: usize = cuts.iter().sum();
        assert_eq!(total, out.len());
        // SAFETY: each segment lies in `out`.
        let wrote = unsafe { disk.through_request(&segments(&mut out), at + 512, true) };
        assert_eq!(wrote, Some(Ok(())));
        let file = std::fs::read(&path).unwrap();
        assert_eq!(file[..512], [0; 512]);
        assert!(file[512..512 + total] == out[..], "written as the guest gave it");
        let mut back = vec![0u8; total];
        // SAFETY: each segment lies in `back`.
        let read = unsafe { disk.through_request(&segments(&mut back), at + 512, false) };
        assert_eq!(read, Some(Ok(())));
        assert!(back == out, "read back as written");
        // Its file cut short under it: that read fails, and the next is answered whole.
        open().set_len((LEN - 4096) as u64).unwrap();
        let mut tail = vec![0u8; 8192];
        let tail_ptr = tail.as_mut_ptr();
        // SAFETY: the segment is `tail`.
        let short = unsafe { disk.through_request(&[(tail_ptr, 8192)], at + LEN as u64 - 8192, false) };
        assert_eq!(short, Some(Err(S_IOERR)));
        let mut again = vec![0u8; 4096];
        let again_ptr = again.as_mut_ptr();
        // SAFETY: the segment is `again`.
        let read = unsafe { disk.through_request(&[(again_ptr, 4096)], at + 512, false) };
        assert_eq!(read, Some(Ok(())));
        assert!(again[..] == out[..4096], "the connection kept in step");
        assert_eq!(
            // SAFETY: the segment is `again`.
            unsafe { disk.through_request(&[(again_ptr, 4096)], at + LEN as u64, false) },
            None,
            "past its range: not this range's to move"
        );
        disk.detach(at).unwrap();
        drop(disk);
        let trips = trips.join().unwrap();
        // 2.5 MiB each way in trips of at most a MiB, then the short read and the one after.
        assert_eq!(
            trips,
            [1 << 20, 1 << 20, 1 << 19, 1 << 20, 1 << 20, 1 << 19, 8192, 4096]
        );
    }

    /// A transfer reaches device memory, a page of a file its guest maps (D189), as it
    /// reaches RAM: read into and written from. One whose page is gone, its file cut short,
    /// is an I/O error, a join disk's zeros between its ranges too, never the end of the
    /// VMM's process.
    #[cfg(unix)]
    #[test]
    fn a_transfer_reaches_device_memory_and_its_lost_pages_fail_it() {
        use std::os::fd::AsRawFd as _;
        const BASE: u64 = 0x8000_0000;
        const DEVICE: u64 = 0x2_0000_0000;
        let dir = shards_testdir::TempDir::new("blk-device").unwrap();
        let disk: Vec<u8> = (0..8 * 512u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("disk"), &disk).unwrap();
        let b = Block::open(&dir.join("disk"), false, "disk0").unwrap();
        let p = crate::platform::page_size().unwrap();
        std::fs::write(dir.join("mapped"), vec![0u8; 2 * p]).unwrap();
        let mapped = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join("mapped"))
            .unwrap();
        // SAFETY: a shared mapping of the file's two pages, unmapped at the test's end.
        let host = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                2 * p,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                mapped.as_raw_fd(),
                0,
            )
        };
        assert_ne!(host, libc::MAP_FAILED);
        let mem = GuestMemory::anonymous(&[(BASE, 1 << 16)]).unwrap();
        // SAFETY: the mapping above, which outlives every use of `mem`'s.
        unsafe { mem.give_devices([(DEVICE, host.cast(), 2 * p)]) }.unwrap();
        let at = |addr: u64, writable: bool| Descriptor {
            addr,
            len: 1024,
            writable,
        };
        let (read, write) = (Direction::Read, Direction::Write);
        assert_eq!(
            transfer(&[at(DEVICE + 512, true)], 1, &mem, &b.backend, read),
            Ok(1024)
        );
        assert_eq!(
            std::fs::read(dir.join("mapped")).unwrap()[512..1536],
            disk[512..1536]
        );
        assert_eq!(
            transfer(&[at(DEVICE + 512, false)], 4, &mem, &b.backend, write),
            Ok(1024)
        );
        assert_eq!(
            std::fs::read(dir.join("disk")).unwrap()[2048..3072],
            disk[512..1536]
        );
        // Cut short under its mapping: each way an I/O error.
        mapped.set_len(p as u64).unwrap();
        let lost = DEVICE + p as u64;
        assert_eq!(
            transfer(&[at(lost, true)], 0, &mem, &b.backend, read),
            Err(S_IOERR)
        );
        assert_eq!(
            transfer(&[at(lost, false)], 0, &mem, &b.backend, write),
            Err(S_IOERR)
        );
        static JOIN: Join = Join::new();
        let ptr = mem.io_ptr(lost, 512).unwrap();
        // SAFETY: `ptr` is guest memory as io_ptr gives it, 512 bytes.
        assert_eq!(unsafe { JOIN.read_at(ptr, 512, 0) }, Err(S_IOERR));
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(host, 2 * p) };
    }

    #[test]
    fn config_space_reports_capacity_segments_and_block_size() {
        let dir_guard = shards_testdir::TempDir::new("blk").unwrap();
        let dir = dir_guard.join("blk");
        std::fs::write(&dir, vec![0u8; 8 * 512]).unwrap();
        let b = Block::open(&dir, true, "disk0").unwrap();
        let mut cap = [0u8; 8];
        b.read_config(0, &mut cap);
        assert_eq!(u64::from_le_bytes(cap), 8);
        let mut seg = [0u8; 4];
        b.read_config(12, &mut seg);
        assert_eq!(u32::from_le_bytes(seg), SEG_MAX);
        let mut beyond = [0xffu8; 4];
        b.read_config(4096, &mut beyond);
        assert_eq!(beyond, [0; 4]);
        assert_ne!(b.features() & F_RO, 0);
        std::fs::write(&dir, vec![0u8; 513]).unwrap();
        assert!(Block::open(&dir, true, "x").is_err());
        let _ = std::fs::remove_file(&dir);
    }
}
