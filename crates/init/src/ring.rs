//! The relay's end of an engine ring (D230): slots of the relay's own memory, pinned for
//! the VM's life, offered in a connection's hello; and the pump that moves a connection's
//! bytes between its client and a slot's rings, its doorbells and its end on the
//! connection's vsock socket.
//!
//! A slot is a memory file the relay maps alone, registered as a fixed buffer of an
//! io_uring instance that init made before it forked the relay and holds until the VM
//! ends: io_uring pins a fixed buffer's pages long-term (`FOLL_LONGTERM`), so they stay at
//! the guest-physical addresses the host was given, and since init's descriptor keeps the
//! instance, a relay that dies leaves them pinned rather than handing them to the guest's
//! allocator while the host may still write them. The host is trusted with nothing here:
//! the relay never reaches past a slot whatever positions it reads, and a host that writes
//! nonsense spoils only the connection's own stream.

use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

use shards_abi::ring::{self as abi, Extent, Offer, Reply, field};
use shards_abi::run;

/// io_uring's register opcodes and flag this needs (include/uapi/linux/io_uring.h).
const IORING_REGISTER_BUFFERS2: libc::c_uint = 15;
const IORING_REGISTER_BUFFERS_UPDATE: libc::c_uint = 16;
const IORING_RSRC_REGISTER_SPARSE: u32 = 1;

/// `struct io_uring_rsrc_register`.
#[repr(C)]
struct RsrcRegister {
    nr: u32,
    flags: u32,
    resv2: u64,
    data: u64,
    tags: u64,
}

/// `struct io_uring_rsrc_update2`.
#[repr(C)]
struct RsrcUpdate2 {
    offset: u32,
    resv: u32,
    data: u64,
    tags: u64,
    nr: u32,
    resv2: u32,
}

/// An io_uring instance whose fixed buffers pin a relay's slots: init's, made before it
/// forks the relay, which inherits it. None where the guest's kernel has no io_uring, or
/// has it turned off (`kernel.io_uring_disabled`): its relay then offers no ring.
pub fn pinner() -> Option<OwnedFd> {
    // `struct io_uring_params`, 120 bytes, all zero: the kernel's defaults.
    let mut params = [0u64; 15];
    // SAFETY: io_uring_setup(2) with one entry and a zeroed params struct it fills.
    let fd = unsafe { libc::syscall(libc::SYS_io_uring_setup, 1 as libc::c_long, params.as_mut_ptr()) };
    let fd = RawFd::try_from(fd).ok().filter(|fd| *fd >= 0)?;
    // SAFETY: a descriptor io_uring_setup just made, close-on-exec, which nothing else owns.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The most slots a relay pins: a stream through a ring keeps two of the guest's threads
/// busy (its client's and the relay's thread for it), so no more streams than half its
/// CPUs move bytes at once (D230).
fn most_slots() -> usize {
    // SAFETY: sysconf(3) of a constant name.
    let cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    usize::try_from(cpus).map_or(1, |n| (n / 2).max(1))
}

/// A slot: the control page and both rings, mapped here, pinned, and named by the
/// guest-physical addresses of its pages.
pub struct Slot {
    base: NonNull<u8>,
    len: usize,
    extents: Vec<Extent>,
    _memory: OwnedFd,
}

// SAFETY: the mapping is this slot's own, reached only through atomics and system calls;
// moving it between the relay's threads changes nothing of that.
unsafe impl Send for Slot {}
// SAFETY: as above: shared, its control page is reached through atomics alone and its
// rings through the kernel's copies, never through a reference.
unsafe impl Sync for Slot {}

impl Drop for Slot {
    fn drop(&mut self) {
        // SAFETY: the mapping `make` made, unmapped once; its pages stay pinned.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.len) };
    }
}

impl Slot {
    /// The offer that names it.
    pub fn offer(&self) -> Offer {
        Offer {
            ring: abi::MAX_RING,
            extents: self.extents.clone(),
        }
    }

    fn word(&self, offset: u64) -> &AtomicU64 {
        // SAFETY: an aligned u64 of the slot's control page, mapped while the slot lives,
        // reached only atomically, by this relay and by the host.
        unsafe { AtomicU64::from_ptr(self.base.as_ptr().add(offset as usize).cast()) }
    }

    fn flag(&self, offset: u64) -> &AtomicU32 {
        // SAFETY: as in `word`, an aligned u32.
        unsafe { AtomicU32::from_ptr(self.base.as_ptr().add(offset as usize).cast()) }
    }

    /// Every position and flag back to zero, as an offer starts.
    fn reset(&self) {
        for f in [
            field::TO_HOST_HEAD,
            field::TO_HOST_TAIL,
            field::TO_GUEST_HEAD,
            field::TO_GUEST_TAIL,
        ] {
            self.word(f).store(0, Ordering::Relaxed);
        }
        for f in [
            field::HOST_WAITS_DATA,
            field::GUEST_WAITS_ROOM,
            field::GUEST_WAITS_DATA,
            field::HOST_WAITS_ROOM,
        ] {
            self.flag(f).store(0, Ordering::Relaxed);
        }
        fence(Ordering::SeqCst);
    }

    /// The ring for the host's bytes `from..from + len`, `from` a ring offset, as at most
    /// two runs wrapping at its end.
    fn to_host(&self, from: u64, len: u64) -> [libc::iovec; 2] {
        self.runs(abi::CONTROL, from, len)
    }

    fn to_guest(&self, from: u64, len: u64) -> [libc::iovec; 2] {
        self.runs(abi::CONTROL + u64::from(abi::MAX_RING), from, len)
    }

    fn runs(&self, ring_at: u64, from: u64, len: u64) -> [libc::iovec; 2] {
        let size = u64::from(abi::MAX_RING);
        let first = len.min(size - from);
        // SAFETY: both runs lie within the ring at `ring_at`, within the mapping.
        let at = |offset: u64| unsafe { self.base.as_ptr().add((ring_at + offset) as usize) };
        [
            libc::iovec {
                iov_base: at(from).cast(),
                iov_len: first as usize,
            },
            libc::iovec {
                iov_base: at(0).cast(),
                iov_len: (len - first) as usize,
            },
        ]
    }
}

/// The relay's slots: made as connections need them, up to [`most_slots`], kept for reuse.
pub struct Pool {
    pinner: OwnedFd,
    pagemap: Option<File>,
    most: usize,
    made: usize,
    registered: bool,
    free: Vec<Slot>,
    /// What made the first slot fail, after which none is tried: the guest cannot pin, or
    /// hides frame numbers.
    broken: Option<io::Error>,
}

static POOL: Mutex<Option<Pool>> = Mutex::new(None);

fn pool() -> std::sync::MutexGuard<'static, Option<Pool>> {
    POOL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The relay's pool, pinned on `pinner`.
pub fn start(pinner: OwnedFd) {
    *pool() = Some(Pool {
        pinner,
        pagemap: None,
        most: most_slots(),
        made: 0,
        registered: false,
        free: Vec::new(),
        broken: None,
    });
}

/// A free slot, or a new one where the relay has fewer than its most; none where it has
/// no pool, its slots are all in use, or the guest cannot make one.
pub fn take() -> Option<Slot> {
    let mut guard = pool();
    let p = guard.as_mut()?;
    if let Some(slot) = p.free.pop() {
        return Some(slot);
    }
    if p.broken.is_some() || p.made >= p.most {
        return None;
    }
    match p.make() {
        Ok(slot) => {
            p.made += 1;
            Some(slot)
        }
        Err(e) => {
            let _ = writeln!(
                io::stderr(),
                "shards-init: engine rings: {e}; plain vsock from here"
            );
            p.broken = Some(e);
            None
        }
    }
}

/// Gives a slot back for the next connection.
pub fn give(slot: Slot) {
    if let Some(p) = pool().as_mut() {
        p.free.push(slot);
    }
}

impl Pool {
    fn make(&mut self) -> io::Result<Slot> {
        let index = u32::try_from(self.made).map_err(|_| io::Error::other("slot index"))?;
        let len =
            usize::try_from(abi::slot_len(abi::MAX_RING)).map_err(|_| io::Error::other("slot length"))?;
        // SAFETY: memfd_create(2) of a NUL-terminated name.
        let fd = unsafe { libc::memfd_create(c"shards-engine-ring".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor just made, which nothing else owns.
        let memory = unsafe { OwnedFd::from_raw_fd(fd) };
        let size = libc::off_t::try_from(len).map_err(|_| io::Error::other("slot length"))?;
        // SAFETY: ftruncate(2) of our own memory file.
        if unsafe { libc::ftruncate(memory.as_raw_fd(), size) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a shared mapping of the whole file, populated, owned by the slot.
        let at = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                memory.as_raw_fd(),
                0,
            )
        };
        if at == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let base = NonNull::new(at.cast::<u8>()).ok_or_else(|| io::Error::other("a null mapping"))?;
        let mut slot = Slot {
            base,
            len,
            extents: Vec::new(),
            _memory: memory,
        };
        // No process init forks later gets it.
        // SAFETY: madvise(2) of the slot's own mapping.
        if unsafe { libc::madvise(at, len, libc::MADV_DONTFORK) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.pin(index, at, len)?;
        let frames = self.frames(at as usize, len)?;
        slot.extents = abi::extents(&frames, page()?).ok_or_else(|| io::Error::other("frame numbers"))?;
        Ok(slot)
    }

    /// Pins `len` bytes at `at` as fixed buffer `index`: the table registered once, sparse,
    /// for the most slots, each slot's entry filled as it is made.
    fn pin(&mut self, index: u32, at: *mut libc::c_void, len: usize) -> io::Result<()> {
        if !self.registered {
            let table = RsrcRegister {
                nr: u32::try_from(self.most).map_err(|_| io::Error::other("slot count"))?,
                flags: IORING_RSRC_REGISTER_SPARSE,
                resv2: 0,
                data: 0,
                tags: 0,
            };
            register(&self.pinner, IORING_REGISTER_BUFFERS2, &table)?;
            self.registered = true;
        }
        let iov = libc::iovec {
            iov_base: at,
            iov_len: len,
        };
        let update = RsrcUpdate2 {
            offset: index,
            resv: 0,
            data: (&raw const iov) as u64,
            tags: 0,
            nr: 1,
            resv2: 0,
        };
        register(&self.pinner, IORING_REGISTER_BUFFERS_UPDATE, &update)
    }

    /// The frame numbers of the pages at `at..at + len`, pinned, from /proc/self/pagemap,
    /// which shows them to CAP_SYS_ADMIN alone (fs/proc/task_mmu.c): bits 0-54 the frame,
    /// bit 63 present (Documentation/admin-guide/mm/pagemap.rst).
    fn frames(&mut self, at: usize, len: usize) -> io::Result<Vec<u64>> {
        if self.pagemap.is_none() {
            self.pagemap = Some(File::open("/proc/self/pagemap")?);
        }
        let pagemap = self.pagemap.as_ref().ok_or_else(|| io::Error::other("pagemap"))?;
        let page = page()?;
        let first = (at as u64) / page;
        let count = (len as u64) / page;
        let mut raw = vec![0u8; usize::try_from(count * 8).map_err(|_| io::Error::other("pagemap length"))?];
        std::os::unix::fs::FileExt::read_exact_at(pagemap, &mut raw, first * 8)?;
        let (entries, []) = raw.as_chunks::<8>() else {
            return Err(io::Error::other("pagemap entries"));
        };
        entries
            .iter()
            .map(|e| {
                let e = u64::from_le_bytes(*e);
                let frame = e & ((1 << 55) - 1);
                if e >> 63 == 1 && frame != 0 {
                    Ok(frame)
                } else {
                    Err(io::Error::other("pagemap shows no frame numbers"))
                }
            })
            .collect()
    }
}

fn register<T>(ring: &OwnedFd, opcode: libc::c_uint, arg: &T) -> io::Result<()> {
    let size = libc::c_uint::try_from(std::mem::size_of::<T>())
        .map_err(|_| io::Error::other("io_uring argument"))?;
    // SAFETY: io_uring_register(2) of our instance, with an argument of the size it reads.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_io_uring_register,
            libc::c_long::from(ring.as_raw_fd()),
            libc::c_long::from(opcode),
            std::ptr::from_ref(arg),
            libc::c_long::from(size),
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn page() -> io::Result<u64> {
    // SAFETY: sysconf(3) of a constant name.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(page)
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| io::Error::other("the page size"))
}

/// What became of a connection's offer.
pub enum Offered {
    /// Its relay has no slot: a plain hello is said.
    Plain,
    /// The device took it: the connection ran through the ring to its end.
    Ringed,
    /// The device refused it: its hello is said, the connection goes on over plain vsock.
    Refused,
}

/// Says the connection's hello on `host`, with a slot's offer where the relay has one, and
/// runs the connection through the slot's rings where the device takes it.
pub fn hello(conn: &UnixStream, host: &File, token: &[u8; run::TOKEN]) -> io::Result<Offered> {
    let Some(slot) = take() else {
        return Ok(Offered::Plain);
    };
    slot.reset();
    let offer = slot.offer().encode();
    let said = (|| {
        let len = u32::try_from(run::TOKEN + offer.len()).map_err(|_| io::Error::other("offer length"))?;
        let hello = [&run::header(run::kind::HELLO, len)[..], token, &offer].concat();
        (&*host).write_all(&hello)?;
        let mut reply = [0u8; abi::REPLY];
        (&*host).read_exact(&mut reply)?;
        Reply::parse(reply).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not a ring's reply"))
    })();
    let offered = match said {
        Ok(Reply::Attached) => {
            if let Err(e) = pump(conn, host, &slot) {
                let _ = writeln!(io::stderr(), "shards-init: engine ring: {e}");
                let _ = conn.shutdown(std::net::Shutdown::Both);
            }
            Ok(Offered::Ringed)
        }
        Ok(Reply::Refused(why)) => {
            let _ = writeln!(io::stderr(), "shards-init: engine ring refused: {why:?}");
            Ok(Offered::Refused)
        }
        Err(e) => Err(e),
    };
    give(slot);
    offered
}

/// One direction's state, as the relay's plain pump keeps its halves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Up {
    /// The client's bytes go into the ring for the host.
    Reading,
    /// The client ended its writes; the device has been told ([`abi::END`]).
    Ended,
}

/// The connection's bytes through `slot`'s rings until the host ends it: the client's
/// into the ring for the host, the ring for the guest's to the client, each side rung
/// only where it said it waits. The host's end ends the client's both ways, after every
/// byte the host sent (D181's engine end).
fn pump(conn: &UnixStream, host: &File, slot: &Slot) -> io::Result<()> {
    set_nonblocking(host.as_raw_fd())?;
    set_nonblocking(conn.as_raw_fd())?;
    let size = u64::from(abi::MAX_RING);
    let (mut head, mut tail) = (0u64, 0u64);
    let mut up = Up::Reading;
    let mut host_ended = false;
    // Signals owed the host that its socket had no room for.
    let mut owed = Owed::default();
    loop {
        let mut progressed = false;
        // The client's bytes, into the ring for the host.
        let taken = slot.word(field::TO_HOST_TAIL).load(Ordering::Acquire);
        let room = size.saturating_sub(head.saturating_sub(taken));
        if up == Up::Reading && !host_ended && room > 0 {
            let iov = slot.to_host(head % size, room);
            match recv(conn.as_raw_fd(), &iov) {
                Ok(0) => {
                    up = Up::Ended;
                    owed.end = true;
                    progressed = true;
                }
                Ok(n) => {
                    head += n as u64;
                    slot.word(field::TO_HOST_HEAD).store(head, Ordering::Release);
                    owed.doorbell |= rung(slot, field::HOST_WAITS_DATA);
                    progressed = true;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        // The ring for the guest's bytes, to the client.
        let given = slot.word(field::TO_GUEST_HEAD).load(Ordering::Acquire);
        let ready = given.saturating_sub(tail).min(size);
        let mut client_full = false;
        if ready > 0 {
            let iov = slot.to_guest(tail % size, ready);
            match send(conn.as_raw_fd(), &iov) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    tail += n as u64;
                    slot.word(field::TO_GUEST_TAIL).store(tail, Ordering::Release);
                    owed.doorbell |= rung(slot, field::HOST_WAITS_ROOM);
                    progressed = true;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => client_full = true,
                Err(e) => return Err(e),
            }
        } else if host_ended {
            // Every byte the host sent has reached the client: its end ends the client's.
            let _ = conn.shutdown(std::net::Shutdown::Both);
            return Ok(());
        }
        owed.send(host.as_raw_fd())?;
        // The host's doorbells, and its end.
        match drain_signals(host.as_raw_fd())? {
            Signals::Ended => {
                host_ended = true;
                progressed = true;
            }
            Signals::Rung => progressed = true,
            Signals::Quiet => {}
        }
        if progressed {
            continue;
        }
        // Nothing moved: say what this side waits for, look once more, then sleep.
        let want_room = up == Up::Reading && !host_ended && room == 0;
        let want_data = ready == 0;
        if want_room {
            slot.flag(field::GUEST_WAITS_ROOM).store(1, Ordering::SeqCst);
        }
        if want_data {
            slot.flag(field::GUEST_WAITS_DATA).store(1, Ordering::SeqCst);
        }
        fence(Ordering::SeqCst);
        let came = (want_room && slot.word(field::TO_HOST_TAIL).load(Ordering::Acquire) != taken)
            || (want_data && slot.word(field::TO_GUEST_HEAD).load(Ordering::Acquire) != given);
        if !came {
            let client = (if up == Up::Reading && !host_ended && room > 0 {
                libc::POLLIN
            } else {
                0
            }) | (if client_full { libc::POLLOUT } else { 0 });
            let host_events = libc::POLLIN | if owed.any() { libc::POLLOUT } else { 0 };
            wait(conn.as_raw_fd(), client, host.as_raw_fd(), host_events)?;
        }
        slot.flag(field::GUEST_WAITS_ROOM).store(0, Ordering::SeqCst);
        slot.flag(field::GUEST_WAITS_DATA).store(0, Ordering::SeqCst);
    }
}

/// Whether the host said it waits at `flag`, which is cleared: after this side published
/// and fenced (Dekker's handshake: the host sets its flag, fences, then looks again).
fn rung(slot: &Slot, flag: u64) -> bool {
    fence(Ordering::SeqCst);
    slot.flag(flag).swap(0, Ordering::SeqCst) != 0
}

/// Signals the host is owed and its socket had no room for yet.
#[derive(Default)]
struct Owed {
    doorbell: bool,
    end: bool,
}

impl Owed {
    fn any(&self) -> bool {
        self.doorbell || self.end
    }

    /// Sends what is owed, a doorbell before the end, as far as the socket takes them.
    fn send(&mut self, host: RawFd) -> io::Result<()> {
        for (owed, byte) in [(&mut self.doorbell, abi::DOORBELL), (&mut self.end, abi::END)] {
            if !*owed {
                continue;
            }
            // SAFETY: send(2) of one byte from a local, never blocking, never raising SIGPIPE.
            let n = unsafe {
                libc::send(
                    host,
                    (&raw const byte).cast(),
                    1,
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            match n {
                1 => *owed = false,
                _ => {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::WouldBlock {
                        return Ok(());
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}

/// What the host's side of the connection said since it was last read.
enum Signals {
    Quiet,
    Rung,
    /// Its end: the host's socket ended its writes, or the connection went.
    Ended,
}

/// Reads every doorbell the host sent: any other byte is a broken rule.
fn drain_signals(host: RawFd) -> io::Result<Signals> {
    let mut said = Signals::Quiet;
    let mut buf = [0u8; 64];
    loop {
        // SAFETY: recv(2) into a local buffer, never blocking.
        let n = unsafe { libc::recv(host, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
        match usize::try_from(n) {
            Ok(0) => return Ok(Signals::Ended),
            Ok(n) => {
                if buf.get(..n).is_none_or(|b| b.iter().any(|&b| b != abi::DOORBELL)) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "a byte that is no doorbell",
                    ));
                }
                said = Signals::Rung;
            }
            Err(_) => {
                let e = io::Error::last_os_error();
                return match e.kind() {
                    io::ErrorKind::WouldBlock => Ok(said),
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe => Ok(Signals::Ended),
                    _ => Err(e),
                };
            }
        }
    }
}

fn recv(fd: RawFd, iov: &[libc::iovec; 2]) -> io::Result<usize> {
    msg(fd, iov, false)
}

fn send(fd: RawFd, iov: &[libc::iovec; 2]) -> io::Result<usize> {
    msg(fd, iov, true)
}

/// One recvmsg(2) or sendmsg(2) of `iov`, never blocking, never raising SIGPIPE.
fn msg(fd: RawFd, iov: &[libc::iovec; 2], out: bool) -> io::Result<usize> {
    let used = if iov.get(1).is_none_or(|v| v.iov_len == 0) {
        1
    } else {
        2
    };
    // SAFETY: an all-zero msghdr is valid; its iovecs are set below.
    let mut m: libc::msghdr = unsafe { std::mem::zeroed() };
    m.msg_iov = iov.as_ptr().cast_mut();
    m.msg_iovlen = used as _;
    loop {
        // SAFETY: the iovecs lie within the slot's mapping, which outlives the call; the
        // kernel copies to or from them without Rust aliasing them.
        let n = unsafe {
            if out {
                libc::sendmsg(fd, &raw const m, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL)
            } else {
                libc::recvmsg(fd, &raw mut m, libc::MSG_DONTWAIT)
            }
        };
        if let Ok(n) = usize::try_from(n) {
            return Ok(n);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl(2) of our own descriptor's flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    // SAFETY: as above.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sleeps until the client or the host has what this side waits for.
fn wait(
    client: RawFd,
    client_events: libc::c_short,
    host: RawFd,
    host_events: libc::c_short,
) -> io::Result<()> {
    let mut fds = [
        libc::pollfd {
            fd: if client_events == 0 { -1 } else { client },
            events: client_events,
            revents: 0,
        },
        libc::pollfd {
            fd: host,
            events: host_events,
            revents: 0,
        },
    ];
    // SAFETY: poll(2) of two initialized entries; a negative descriptor is skipped.
    if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const SIZE: u64 = abi::MAX_RING as u64;

    impl Slot {
        /// A slot as `make` maps it, unpinned and unnamed: what the pump alone needs.
        fn unpinned() -> Slot {
            let len = abi::slot_len(abi::MAX_RING) as usize;
            // SAFETY: as in `make`.
            let fd = unsafe { libc::memfd_create(c"test-ring".as_ptr(), libc::MFD_CLOEXEC) };
            assert!(fd >= 0);
            // SAFETY: a descriptor just made.
            let memory = unsafe { OwnedFd::from_raw_fd(fd) };
            // SAFETY: as in `make`.
            assert_eq!(unsafe { libc::ftruncate(fd, len as libc::off_t) }, 0);
            // SAFETY: as in `make`.
            let at = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    0,
                )
            };
            assert_ne!(at, libc::MAP_FAILED);
            Slot {
                base: NonNull::new(at.cast()).unwrap(),
                len,
                extents: Vec::new(),
                _memory: memory,
            }
        }

        fn byte(&self, at: u64) -> *mut u8 {
            // SAFETY: tests keep `at` within the slot.
            unsafe { self.base.as_ptr().add(at as usize) }
        }
    }

    /// What the device stand-in did: the bytes it took from the ring for the host, and
    /// whether the relay said its client's end.
    struct Device {
        took: Vec<u8>,
        ended: bool,
    }

    /// The device's side of `slot`, on `socket` (the vsock connection's host end): takes
    /// what the relay puts in the ring for the host, puts `answer` in the ring for the
    /// guest, and ends its writes once it has, `after_end` the relay's end or not; it
    /// sleeps as the device does, its flags set, until a doorbell.
    fn device(slot: &Slot, socket: &UnixStream, answer: &[u8], after_end: bool) -> Device {
        let deadline = Instant::now() + Duration::from_secs(20);
        let (mut taken, mut given) = (0u64, 0u64);
        let mut out = Device {
            took: Vec::new(),
            ended: false,
        };
        let mut eof = false;
        socket.set_nonblocking(true).unwrap();
        loop {
            assert!(Instant::now() < deadline, "the device stand-in waited too long");
            let mut moved = false;
            let head = slot.word(field::TO_HOST_HEAD).load(Ordering::Acquire);
            while taken < head {
                // SAFETY: a byte of the ring the relay published.
                out.took
                    .push(unsafe { slot.byte(abi::CONTROL + taken % SIZE).read() });
                taken += 1;
                moved = true;
            }
            slot.word(field::TO_HOST_TAIL).store(taken, Ordering::Release);
            if rung(slot, field::GUEST_WAITS_ROOM) {
                (&*socket).write_all(&[abi::DOORBELL]).unwrap();
            }
            let freed = slot.word(field::TO_GUEST_TAIL).load(Ordering::Acquire);
            let mut room = SIZE - (given - freed);
            while (given as usize) < answer.len() && room > 0 {
                // SAFETY: a byte of the ring for the guest with room.
                unsafe {
                    slot.byte(abi::CONTROL + SIZE + given % SIZE)
                        .write(answer[given as usize])
                };
                given += 1;
                room -= 1;
                moved = true;
            }
            slot.word(field::TO_GUEST_HEAD).store(given, Ordering::Release);
            if rung(slot, field::GUEST_WAITS_DATA) {
                (&*socket).write_all(&[abi::DOORBELL]).unwrap();
            }
            if !eof && given as usize == answer.len() && (out.ended || !after_end) {
                socket.shutdown(std::net::Shutdown::Write).unwrap();
                eof = true;
            }
            let mut signal = [0u8; 16];
            match (&*socket).read(&mut signal) {
                Ok(0) => return out,
                Ok(n) => {
                    for b in &signal[..n] {
                        match *b {
                            abi::DOORBELL => {}
                            abi::END => out.ended = true,
                            other => panic!("the relay sent {other}"),
                        }
                    }
                    moved = true;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return out,
            }
            if moved {
                continue;
            }
            slot.flag(field::HOST_WAITS_DATA).store(1, Ordering::SeqCst);
            slot.flag(field::HOST_WAITS_ROOM)
                .store(u32::from(room == 0), Ordering::SeqCst);
            fence(Ordering::SeqCst);
            if slot.word(field::TO_HOST_HEAD).load(Ordering::Acquire) == taken
                && slot.word(field::TO_GUEST_TAIL).load(Ordering::Acquire) == freed
            {
                let mut fds = [libc::pollfd {
                    fd: socket.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                }];
                // SAFETY: poll(2) of one entry. The relay rings a device that said it waits,
                // so a long silence is a lost wakeup.
                let woken = unsafe { libc::poll(fds.as_mut_ptr(), 1, 5000) };
                assert!(woken != 0, "the relay never rang the waiting device");
            }
            slot.flag(field::HOST_WAITS_DATA).store(0, Ordering::SeqCst);
            slot.flag(field::HOST_WAITS_ROOM).store(0, Ordering::SeqCst);
        }
    }

    fn pattern(n: usize, salt: u8) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(7).wrapping_add(salt))
            .collect()
    }

    /// A client's request and the engine's answer, each bigger than a ring, through the
    /// relay's pump and the device stand-in.
    fn exchange(request: usize, answer: usize, after_end: bool) -> (Vec<u8>, Device, Vec<u8>) {
        let slot = Slot::unpinned();
        let (client, relay_side) = UnixStream::pair().unwrap();
        let (vsock_guest, vsock_host) = UnixStream::pair().unwrap();
        let sent = pattern(request, 3);
        let answer = pattern(answer, 9);
        let (got, device) = std::thread::scope(|s| {
            let pump = s.spawn(|| {
                let host = File::from(OwnedFd::from(vsock_guest));
                pump(&relay_side, &host, &slot)
            });
            // Its socket moves in, so that a stand-in that fails ends the connection with it.
            let (slot, answer) = (&slot, &answer);
            let device = s.spawn(move || device(slot, &vsock_host, answer, after_end));
            let writer = s.spawn(|| {
                (&client).write_all(&sent).unwrap();
                client.shutdown(std::net::Shutdown::Write).unwrap();
            });
            let mut got = Vec::new();
            (&client).read_to_end(&mut got).unwrap();
            writer.join().unwrap();
            pump.join().unwrap().unwrap();
            (got, device.join().unwrap())
        });
        (got, device, sent)
    }

    #[test]
    fn a_requests_and_an_answers_bytes_cross_whole_through_the_rings() {
        let (got, device, sent) = exchange(3 * SIZE as usize + 17, 2 * SIZE as usize + 5, true);
        assert_eq!(device.took, sent);
        assert!(device.ended, "the client's end reached the device");
        assert_eq!(got, pattern(2 * SIZE as usize + 5, 9));
    }

    #[test]
    fn the_hosts_end_ends_the_client_after_its_last_byte() {
        // The engine answers and ends before the client has ended its writes.
        let (got, _, _) = exchange(10, SIZE as usize + 3, false);
        assert_eq!(got, pattern(SIZE as usize + 3, 9));
    }

    #[test]
    fn a_host_byte_that_is_no_doorbell_ends_the_pump() {
        let slot = Slot::unpinned();
        let (_client, relay_side) = UnixStream::pair().unwrap();
        let (vsock_guest, vsock_host) = UnixStream::pair().unwrap();
        (&vsock_host).write_all(&[0x7f]).unwrap();
        let host = File::from(OwnedFd::from(vsock_guest));
        let e = pump(&relay_side, &host, &slot).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_fresh_slot_is_reset_before_its_offer() {
        let slot = Slot::unpinned();
        slot.word(field::TO_GUEST_HEAD).store(99, Ordering::Relaxed);
        slot.flag(field::HOST_WAITS_ROOM).store(1, Ordering::Relaxed);
        slot.reset();
        assert_eq!(slot.word(field::TO_GUEST_HEAD).load(Ordering::Relaxed), 0);
        assert_eq!(slot.flag(field::HOST_WAITS_ROOM).load(Ordering::Relaxed), 0);
    }
}
