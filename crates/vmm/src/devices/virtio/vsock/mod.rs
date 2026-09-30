//! virtio-vsock (virtio 1.3 §5.10): the host↔guest control plane (docs/design/architecture.md
//! D12), with guest ports mapped to host Unix sockets as Firecracker maps them
//! (docs/research/virtio-io-exits.md R7).
//!
//! One worker thread per device moves packets between the virtqueues and the host sockets.
//! Payloads go straight between sockets and guest memory, through readv/writev on raw
//! guest pointers, so no Rust reference aliases memory the guest may be changing.

mod conn;
mod muxer;
pub mod packet;
mod poll;

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use super::queue::{Chain, Queue, QueueError, with};
use super::{Activation, DeviceInterrupt, VirtioDevice, feature};
use crate::memory::{Access, GuestMemory};
use crate::snapshot::codec::{self, Reader, Writer};
use crate::{debug, warn};
use muxer::{Muxer, Saved, Token};
use packet::{HEADER_LEN, Header, MAX_PAYLOAD};
use poll::{Interest, Ready};

pub const DEVICE_ID: u32 = 19;
/// The guest's vsock address. Each VM's device is its own namespace, so every VM can
/// use the first address guests may have (VMADDR_CID_HOST is 2).
pub const GUEST_CID: u64 = 3;
const QUEUE_SIZE: u16 = 256;
const RX: usize = 0;
const TX: usize = 1;

/// A run of guest memory: its guest address and where it is mapped in this process.
#[derive(Debug, Clone, Copy)]
pub struct Span {
    pub gpa: u64,
    pub ptr: *mut u8,
    pub len: usize,
}

fn iovecs(spans: &[Span], max: usize) -> Vec<libc::iovec> {
    let mut left = max;
    let mut out = Vec::with_capacity(spans.len());
    for s in spans {
        if left == 0 {
            break;
        }
        let n = s.len.min(left);
        out.push(libc::iovec {
            iov_base: s.ptr.cast(),
            iov_len: n,
        });
        left -= n;
    }
    out
}

/// Reads at most `max` bytes from `fd` into `spans`, in one readv(2).
fn readv(fd: RawFd, spans: &[Span], max: usize) -> io::Result<usize> {
    let iov = iovecs(spans, max);
    if iov.is_empty() {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    loop {
        // SAFETY: every iovec lies in guest RAM, checked by host_ptr when its span was
        // made; the kernel writes there without Rust aliasing it.
        let n = unsafe { libc::readv(fd, iov.as_ptr(), iov.len() as libc::c_int) };
        if let Ok(n) = usize::try_from(n) {
            return Ok(n);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Writes at most `max` bytes of `spans` to `fd`, in one writev(2).
fn writev(fd: RawFd, spans: &[Span], max: usize) -> io::Result<usize> {
    let iov = iovecs(spans, max);
    if iov.is_empty() {
        return Ok(0);
    }
    loop {
        // SAFETY: as in `readv`; the kernel only reads guest memory here.
        let n = unsafe { libc::writev(fd, iov.as_ptr(), iov.len() as libc::c_int) };
        if let Ok(n) = usize::try_from(n) {
            return Ok(n);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// A pipe, close-on-exec and non-blocking: its read end, then its write end. Linux makes
/// it so at once with pipe2(2), which the seccomp filter allows where musl's pipe() would
/// be pipe(2), which it does not.
#[cfg(target_os = "linux")]
fn nonblocking_pipe() -> io::Result<[OwnedFd; 2]> {
    let mut fds = [0; 2];
    // SAFETY: pipe2(2) fills both descriptors on success.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh descriptors that nothing else owns.
    Ok(fds.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }))
}

/// A pipe, close-on-exec and non-blocking: macOS has no pipe2(2), so its flags are set
/// after.
#[cfg(not(target_os = "linux"))]
fn nonblocking_pipe() -> io::Result<[OwnedFd; 2]> {
    let mut fds = [0; 2];
    // SAFETY: pipe(2) fills both descriptors on success.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh descriptors that nothing else owns.
    let owned = fds.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
    for fd in fds {
        // SAFETY: fcntl(2) on our own descriptors.
        let ok = unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) == 0
                && libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) == 0
        };
        if !ok {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(owned)
}

/// A self-pipe: vCPU threads write a byte to wake the worker out of poll(2).
#[derive(Debug)]
struct Waker {
    read: OwnedFd,
    write: OwnedFd,
}

impl Waker {
    fn new() -> io::Result<Waker> {
        let [read, write] = nonblocking_pipe()?;
        Ok(Waker { read, write })
    }

    /// Cheap enough for a vCPU thread. A full pipe already holds a wakeup.
    fn wake(&self) {
        // SAFETY: writes one byte from a valid buffer to our own descriptor.
        unsafe { libc::write(self.write.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
    }

    fn drain(&self) {
        let mut buf = [0u8; 64];
        // SAFETY: reads into a local buffer from our own non-blocking descriptor.
        while unsafe { libc::read(self.read.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
    }
}

/// What the worker owns while it runs, handed back when it stops.
struct Session {
    queues: Vec<Queue>,
    muxer: Muxer,
}

struct Worker {
    thread: JoinHandle<Session>,
    stop: Arc<AtomicBool>,
}

pub struct Vsock {
    guest_cid: u64,
    waker: Arc<Waker>,
    /// Held while no worker runs.
    muxer: Option<Muxer>,
    context: Option<(Arc<GuestMemory>, Arc<DeviceInterrupt>)>,
    worker: Option<Worker>,
    /// The queues while paused.
    paused: Option<Vec<Queue>>,
    /// The streams a snapshot's guest may hold, from `restore` until activation resets
    /// them.
    saved: Option<Saved>,
}

impl std::fmt::Debug for Vsock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vsock")
            .field("guest_cid", &self.guest_cid)
            .field("muxer", &self.muxer)
            .finish_non_exhaustive()
    }
}

impl Vsock {
    /// A device whose host side listens at `path`.
    pub fn new(path: &Path, guest_cid: u64) -> Result<Vsock, String> {
        Ok(Vsock {
            guest_cid,
            waker: Arc::new(Waker::new().map_err(|e| format!("vsock: {e}"))?),
            muxer: Some(Muxer::bind(path, guest_cid).map_err(|e| format!("vsock: {e}"))?),
            context: None,
            worker: None,
            paused: None,
            saved: None,
        })
    }

    fn start(&mut self, session: Session) -> Result<(), String> {
        let (memory, interrupt) = self
            .context
            .clone()
            .ok_or("virtio-vsock started before activation")?;
        let stop = Arc::new(AtomicBool::new(false));
        let (flag, waker) = (stop.clone(), self.waker.clone());
        let thread = thread::Builder::new()
            .name("virtio-vsock".into())
            .spawn(move || run(session, &memory, &interrupt, &waker, &flag))
            .map_err(|e| format!("spawning virtio-vsock worker: {e}"))?;
        self.worker = Some(Worker { thread, stop });
        Ok(())
    }

    /// Stops the worker at a boundary between packets and takes back its session.
    fn stop(&mut self) -> Option<Session> {
        let w = self.worker.take()?;
        w.stop.store(true, Ordering::Release);
        self.waker.wake();
        match w.thread.join() {
            Ok(s) => Some(s),
            Err(_) => {
                warn!("virtio-vsock worker ended abnormally");
                None
            }
        }
    }
}

impl VirtioDevice for Vsock {
    fn device_id(&self) -> u32 {
        DEVICE_ID
    }

    fn features(&self) -> u64 {
        feature::VERSION_1 | feature::EVENT_IDX | feature::INDIRECT_DESC
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE; 3]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // struct virtio_vsock_config: le64 guest_cid.
        let config = self.guest_cid.to_le_bytes();
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

    fn activate(&mut self, activation: Activation) -> Result<(), String> {
        let Activation {
            memory,
            queues,
            interrupt,
            restored,
            ..
        } = activation;
        if queues.len() != 3 {
            return Err(format!("virtio-vsock activated with {} queues", queues.len()));
        }
        let mut muxer = self.muxer.take().ok_or("virtio-vsock activated twice")?;
        if restored {
            // The guest's connections belong to the snapshot's host, not to this one.
            muxer.reset();
            muxer.restore(self.saved.take().unwrap_or_default());
        }
        self.context = Some((memory, interrupt));
        self.start(Session { queues, muxer })
    }

    fn notify(&self, _queue: u16) {
        self.waker.wake();
    }

    fn reset(&mut self) {
        let session = self.stop();
        let paused = self.paused.take();
        if let Some(mut s) = session {
            s.muxer.reset();
            self.muxer = Some(s.muxer);
        }
        if paused.is_some()
            && let Some(m) = &mut self.muxer
        {
            m.reset();
        }
        self.context = None;
        self.saved = None;
    }

    /// The worker stops between packets, so every popped buffer has been returned and
    /// the queues match guest memory. Connections stay open.
    fn pause(&mut self) -> Vec<super::QueueState> {
        if let Some(s) = self.stop() {
            self.paused = Some(s.queues);
            self.muxer = Some(s.muxer);
        }
        self.paused
            .iter()
            .flat_map(|queues| queues.iter().map(Queue::state))
            .collect()
    }

    fn resume(&mut self) -> Result<(), String> {
        let Some(queues) = self.paused.take() else {
            return Ok(());
        };
        let muxer = self
            .muxer
            .take()
            .ok_or("virtio-vsock resumed without its sockets")?;
        self.start(Session { queues, muxer })
    }

    /// The streams the guest holds, which a restored copy resets. Paused, the muxer is
    /// back here; a device that never started holds none.
    fn save(&self, w: &mut Writer) {
        self.muxer.as_ref().map(Muxer::saved).unwrap_or_default().write(w);
    }

    fn restore(&mut self, r: &mut Reader<'_>) -> codec::Result<()> {
        self.saved = Some(Saved::read(r)?);
        Ok(())
    }
}

impl Drop for Vsock {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The worker: moves packets until stopped. A malformed ring marks the device as needing
/// reset and ends processing (virtio 1.3 §2.1.2).
fn run(
    mut s: Session,
    mem: &GuestMemory,
    irq: &DeviceInterrupt,
    waker: &Waker,
    stop: &AtomicBool,
) -> Session {
    let mut interests: Vec<Interest<Option<Token>>> = Vec::new();
    let mut ready: Vec<Ready<Option<Token>>> = Vec::new();
    let mut poller: Option<poll::Poller> = None;
    while !stop.load(Ordering::Acquire) {
        let more = match step(&mut s, mem, irq) {
            Ok(more) => more,
            Err(e) => {
                warn!("virtio-vsock: {e}; device needs reset");
                irq.fail();
                return s;
            }
        };
        interests.clear();
        interests.push(Interest {
            fd: waker.read.as_raw_fd(),
            read: true,
            write: false,
            token: None,
        });
        s.muxer.interests(&mut interests);
        // With packets left for the next round, the sockets are only looked at: they get
        // their turn, and so does stopping, between rounds.
        let timeout = if more {
            Some(std::time::Duration::ZERO)
        } else {
            s.muxer
                .next_deadline()
                .map(|t| t.saturating_duration_since(Instant::now()))
        };
        ready.clear();
        let waited = match &mut poller {
            Some(p) => p.wait(&interests, timeout, &mut ready),
            None => poll::Poller::new().and_then(|p| poller.insert(p).wait(&interests, timeout, &mut ready)),
        };
        if let Err(e) = waited {
            // Unexpected (out of memory or descriptors): back off rather than spin.
            warn!("virtio-vsock: waiting for sockets: {e}");
            thread::sleep(std::time::Duration::from_millis(10));
        }
        waker.drain();
        s.muxer.on_events(&ready);
        s.muxer.expire(Instant::now());
    }
    s
}

/// One round: guest packets to the host, host packets to the guest, a ring's worth of
/// each at most, then one interrupt if the driver wants one. Whether packets may be left
/// for the next round: a driver refilling TX from another CPU cannot keep RX, the host's
/// sockets or a stop from their turn (audit A09). The event queue is never used: a
/// restore resets the streams its snapshot held on RX instead (muxer.rs).
fn step(s: &mut Session, mem: &GuestMemory, irq: &DeviceInterrupt) -> Result<bool, QueueError> {
    let [rxq, txq, _] = s.queues.as_mut_slice() else {
        return Ok(false);
    };
    let mut used = [false; 2];
    // Guest → host. First in every round: the driver stops taking RX packets while too
    // many of its replies wait in TX (Linux virtio_transport_more_replies).
    // Each packet is parsed under an access, and its payload moved to or from the host
    // socket with none held.
    let mut sent = 0;
    let tx_more = 'tx: loop {
        with(mem, |a| txq.disable_notification(a))?;
        loop {
            if sent == usize::from(txq.size()) {
                // TX's notifications stay off: the next round comes back for the rest.
                break 'tx true;
            }
            let Some((packet, chain)) = with(mem, |a| Ok(txq.pop(a)?.map(|c| (parse_tx(&c, a), c))))? else {
                break;
            };
            if let Some((h, payload)) = packet {
                s.muxer.on_guest_packet(&h, &payload, mem);
            }
            with(mem, |a| txq.add_used(a, chain.head, 0))?;
            used[TX] = true;
            sent += 1;
        }
        if !with(mem, |a| txq.enable_notification(a))? {
            break false;
        }
    };
    // Host → guest, while there are packets and buffers for them.
    let mut received = 0;
    let mut rx_more = false;
    while s.muxer.has_pending_rx() {
        if received == usize::from(rxq.size()) {
            rx_more = true;
            break;
        }
        let Some(chain) = with(mem, |a| rxq.pop(a))? else {
            // Out of buffers: ask the driver to notify when it adds some.
            if with(mem, |a| rxq.enable_notification(a))? {
                continue;
            }
            break;
        };
        let written = fill_rx(&chain, mem, &mut s.muxer);
        with(mem, |a| rxq.add_used(a, chain.head, written))?;
        used[RX] = true;
        received += 1;
    }
    let mut interrupt = false;
    for (q, used) in [(&mut *rxq, used[RX]), (&mut *txq, used[TX])] {
        if used {
            interrupt |= with(mem, |a| q.needs_interrupt(a))?;
        }
    }
    if interrupt {
        irq.used_buffer();
    }
    Ok(tx_more || rx_more)
}

/// The spans of a chain's readable or writable descriptors, skipping the first `skip`
/// bytes. None if a descriptor lies outside guest RAM.
fn spans(chain: &Chain, mem: &GuestMemory, writable: bool, mut skip: usize) -> Option<Vec<Span>> {
    let mut out = Vec::new();
    for d in chain.descriptors.iter().filter(|d| d.writable == writable) {
        let len = d.len as usize;
        if skip >= len {
            skip -= len;
            continue;
        }
        let gpa = d.addr.checked_add(skip as u64)?;
        let len = len - skip;
        skip = 0;
        let ptr = mem.host_ptr(gpa, len).ok()?;
        out.push(Span { gpa, ptr, len });
    }
    Some(out)
}

/// A guest packet: its header and payload spans. Malformed packets are dropped, as
/// Linux drops them in the other direction.
fn parse_tx(chain: &Chain, mem: &Access<'_>) -> Option<(Header, Vec<Span>)> {
    let mut raw = [0u8; HEADER_LEN];
    let mut filled = 0;
    for d in chain.readable() {
        let dst = raw.get_mut(filled..)?;
        let take = dst.len().min(d.len as usize);
        if take == 0 {
            break;
        }
        mem.read(d.addr, dst.get_mut(..take)?).ok()?;
        filled += take;
    }
    if filled < HEADER_LEN {
        debug!("virtio-vsock: TX packet shorter than its header");
        return None;
    }
    let h = Header::decode(&raw);
    if h.len > MAX_PAYLOAD {
        debug!("virtio-vsock: TX payload of {} bytes", h.len);
        return None;
    }
    let mut payload = spans(chain, mem.memory(), false, HEADER_LEN)?;
    let mut need = h.len as usize;
    payload.retain_mut(|s| {
        s.len = s.len.min(need);
        need -= s.len;
        s.len > 0
    });
    if need > 0 {
        debug!("virtio-vsock: TX packet carries less than its {} bytes", h.len);
        return None;
    }
    Some((h, payload))
}

/// Writes the next host packet into an RX buffer. Returns the bytes written: 0 if the
/// buffer is unusable or nothing was due after all (Linux drops such buffers).
fn fill_rx(chain: &Chain, mem: &GuestMemory, muxer: &mut Muxer) -> u32 {
    let (Some(head), Some(space)) = (spans(chain, mem, true, 0), spans(chain, mem, true, HEADER_LEN)) else {
        return 0;
    };
    if head.iter().map(|s| s.len).sum::<usize>() < HEADER_LEN {
        debug!("virtio-vsock: RX buffer smaller than a header");
        return 0;
    }
    let Some(h) = muxer.next_rx(&space) else {
        return 0;
    };
    let wrote = mem.access().map_or(0, |a| write_span(chain, &a, &h.encode()));
    wrote + h.len
}

/// Copies `bytes` into the chain's writable descriptors from the start; returns the
/// bytes copied.
fn write_span(chain: &Chain, mem: &Access<'_>, bytes: &[u8]) -> u32 {
    let mut rest = bytes;
    let mut wrote = 0u32;
    for d in chain.writable() {
        if rest.is_empty() {
            break;
        }
        let n = rest.len().min(d.len as usize);
        let (now, later) = rest.split_at(n);
        if mem.write(d.addr, now).is_err() {
            break;
        }
        wrote += n as u32;
        rest = later;
    }
    wrote
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use super::packet::{HOST_CID, TYPE_STREAM, op};
    use super::*;
    use crate::devices::virtio::queue::QueueConfig;

    const BASE: u64 = 0x8000_0000;
    const SIZE: u16 = 16;
    const RX: u64 = 0;
    const TX: u64 = 1;

    fn desc(q: u64) -> u64 {
        BASE + 0x1000 * q
    }
    fn avail(q: u64) -> u64 {
        BASE + 0x4000 + 0x100 * q
    }
    fn used(q: u64) -> u64 {
        BASE + 0x5000 + 0x200 * q
    }
    fn buffer(q: u64, i: u16) -> u64 {
        BASE + 0x10_000 + 0x10_000 * q + 0x1000 * u64::from(i)
    }

    struct Line;
    impl crate::devices::Interrupt for Line {
        fn set_level(&self, _: bool) {}
    }

    /// Queue `q`'s descriptor `i`, one buffer of `len` bytes, and published.
    fn publish(mem: &GuestMemory, q: u64, i: u16, len: u32, writable: bool) {
        let a = mem.access().unwrap();
        let mut d = [0u8; 16];
        d[..8].copy_from_slice(&buffer(q, i).to_le_bytes());
        d[8..12].copy_from_slice(&len.to_le_bytes());
        d[12..14].copy_from_slice(&(if writable { 2u16 } else { 0 }).to_le_bytes());
        a.write(desc(q) + 16 * u64::from(i), &d).unwrap();
        let idx = a.load_u16(avail(q) + 2, Ordering::Acquire).unwrap();
        a.write_obj(avail(q) + 4 + 2 * u64::from(idx % SIZE), i).unwrap();
        a.store_u16(avail(q) + 2, idx.wrapping_add(1), Ordering::Release)
            .unwrap();
    }

    /// Puts back every buffer of queue `q` the device has used since `seen`; how many.
    fn put_back(a: &Access<'_>, q: u64, seen: &mut u16) -> usize {
        let used_idx = a.load_u16(used(q) + 2, Ordering::Acquire).unwrap();
        let mut n = 0;
        while *seen != used_idx {
            let slot = u64::from(*seen % SIZE);
            let head = a.read_obj::<u32>(used(q) + 4 + 8 * slot).unwrap() as u16;
            let idx = a.load_u16(avail(q) + 2, Ordering::Acquire).unwrap();
            a.write_obj(avail(q) + 4 + 2 * u64::from(idx % SIZE), head)
                .unwrap();
            a.store_u16(avail(q) + 2, idx.wrapping_add(1), Ordering::Release)
                .unwrap();
            *seen = seen.wrapping_add(1);
            n += 1;
        }
        n
    }

    /// A session whose TX ring is full of requests for a host port nothing listens on,
    /// which the device answers with resets on RX, and whose RX ring is full of buffers.
    fn flooded(tag: &str) -> (Arc<GuestMemory>, Session, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("shards-vsock-{tag}-{}", std::process::id()));
        let mem = Arc::new(GuestMemory::anonymous(&[(BASE, 1 << 20)]).unwrap());
        let queue = |q: u64| {
            let cfg = QueueConfig {
                size: SIZE,
                desc: desc(q),
                avail: avail(q),
                used: used(q),
                ready: true,
            };
            Queue::new(cfg, SIZE, &mem, feature::VERSION_1).unwrap()
        };
        for i in 0..SIZE {
            let request = Header {
                src_cid: 3,
                dst_cid: HOST_CID,
                src_port: 1000 + u32::from(i),
                dst_port: 9,
                len: 0,
                kind: TYPE_STREAM,
                op: op::REQUEST,
                flags: 0,
                buf_alloc: 1 << 16,
                fwd_cnt: 0,
            };
            mem.access()
                .unwrap()
                .write(buffer(TX, i), &request.encode())
                .unwrap();
            publish(&mem, TX, i, HEADER_LEN as u32, false);
            publish(&mem, RX, i, 4096, true);
        }
        let session = Session {
            queues: vec![queue(RX), queue(TX), queue(2)],
            muxer: Muxer::bind(&dir, 3).unwrap(),
        };
        (mem, session, dir)
    }

    /// A driver that puts every buffer of queues `qs` the device uses straight back, until
    /// `flooding` is cleared, counting TX's in `recycled`. It notifies through `notify`
    /// whenever the device has not asked it not to, as Linux's `virtqueue_kick_prepare`
    /// decides without EVENT_IDX.
    fn driver(
        mem: Arc<GuestMemory>,
        qs: &'static [u64],
        flooding: Arc<AtomicBool>,
        recycled: Arc<AtomicUsize>,
        notify: Option<Arc<Waker>>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut seen = [0u16; 2];
            while flooding.load(Ordering::Acquire) {
                let a = mem.access().unwrap();
                for &q in qs {
                    let n = put_back(&a, q, &mut seen[q as usize]);
                    if q == TX {
                        recycled.fetch_add(n, Ordering::SeqCst);
                    }
                    // The index must be visible before the device's flags are read.
                    std::sync::atomic::fence(Ordering::SeqCst);
                    if n > 0
                        && let Some(waker) = &notify
                        && a.read_obj::<u16>(used(q)).unwrap() & 1 == 0
                    {
                        waker.wake();
                    }
                }
                drop(a);
                std::thread::yield_now();
            }
        })
    }

    /// A guest flooding TX, which its driver refills as fast as the device uses it, keeps
    /// neither the round from ending nor RX from its turn: the host's replies reach the
    /// guest meanwhile. Before, TX was drained until empty, and was never (audit A09).
    #[test]
    fn a_flooded_tx_queue_leaves_rx_its_turn() {
        let (mem, mut session, dir) = flooded("flood");
        let irq = DeviceInterrupt::new(Arc::new(Line));
        let flooding = Arc::new(AtomicBool::new(true));
        let rounds = Arc::new(AtomicUsize::new(0));
        let tx_driver = driver(
            mem.clone(),
            &[TX],
            flooding.clone(),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        // The device, round after round, on a thread of its own: an old device would
        // never come back from its first.
        let device = {
            let (mem, rounds) = (mem.clone(), rounds.clone());
            std::thread::spawn(move || {
                let tx_used = || {
                    mem.access()
                        .unwrap()
                        .load_u16(used(TX) + 2, Ordering::Acquire)
                        .unwrap()
                };
                // The ring starts full, so the first round spends its budget on TX, and
                // says packets may be left; later rounds find what the driver put back,
                // a ring's worth at most, however fast it puts them back.
                for round in 0..20 {
                    let before = tx_used();
                    let more = step(&mut session, &mem, &irq).unwrap();
                    let sent = tx_used().wrapping_sub(before);
                    assert!(sent <= SIZE, "round {round} took {sent} TX packets");
                    if round == 0 {
                        assert!(more, "a round that spent its budget left nothing for the next");
                    }
                    rounds.fetch_add(1, Ordering::SeqCst);
                }
            })
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while !device.is_finished() {
            assert!(
                Instant::now() < deadline,
                "a round never ended ({} did)",
                rounds.load(Ordering::SeqCst)
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        device.join().unwrap();
        flooding.store(false, Ordering::Release);
        tx_driver.join().unwrap();
        let rx_used = mem
            .access()
            .unwrap()
            .load_u16(used(RX) + 2, Ordering::Acquire)
            .unwrap();
        assert_eq!(
            rx_used, SIZE,
            "the guest's RX buffers never got the host's replies"
        );
        let _ = std::fs::remove_file(&dir);
    }

    /// However many replies wait, and however fast the driver puts RX buffers back, one
    /// round delivers a ring's worth at most (audit A09).
    #[test]
    fn a_round_delivers_a_ring_of_rx_at_most() {
        let (mem, mut session, dir) = flooded("rx");
        let irq = DeviceInterrupt::new(Arc::new(Line));
        // Four rings of requests, each answered with a reset: the first ring's fill the
        // RX buffers, and the rest wait.
        let mut seen_tx = 0;
        for _ in 0..4 {
            step(&mut session, &mem, &irq).unwrap();
            put_back(&mem.access().unwrap(), TX, &mut seen_tx);
        }
        let before = mem
            .access()
            .unwrap()
            .load_u16(used(RX) + 2, Ordering::Acquire)
            .unwrap();
        assert_eq!(before, SIZE);
        assert!(session.muxer.has_pending_rx());
        let flooding = Arc::new(AtomicBool::new(true));
        let rx_driver = driver(
            mem.clone(),
            &[RX],
            flooding.clone(),
            Arc::new(AtomicUsize::new(0)),
            None,
        );
        // Every buffer back first, so the round has a ring's worth to fill, and more
        // coming back as it fills them.
        let deadline = Instant::now() + Duration::from_secs(20);
        while mem
            .access()
            .unwrap()
            .load_u16(avail(RX) + 2, Ordering::Acquire)
            .unwrap()
            != before.wrapping_add(SIZE)
        {
            assert!(Instant::now() < deadline, "the driver never put RX buffers back");
            std::thread::sleep(Duration::from_millis(1));
        }
        step(&mut session, &mem, &irq).unwrap();
        flooding.store(false, Ordering::Release);
        rx_driver.join().unwrap();
        let delivered = mem
            .access()
            .unwrap()
            .load_u16(used(RX) + 2, Ordering::Acquire)
            .unwrap()
            .wrapping_sub(before);
        assert_eq!(delivered, SIZE, "one round delivered {delivered} packets");
        let _ = std::fs::remove_file(&dir);
    }

    /// The worker keeps up with a driver that floods TX, coming back by itself for what a
    /// round left, with the driver's notifications off, and woken by them once it has
    /// caught up; and a stop ends it at once, flood or not (audit A09). RX's buffers run
    /// out and stay out, so nothing else wakes it.
    #[test]
    fn a_flooded_worker_keeps_up_and_stops() {
        let (mem, session, dir) = flooded("worker");
        let irq = Arc::new(DeviceInterrupt::new(Arc::new(Line)));
        let waker = Arc::new(Waker::new().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let flooding = Arc::new(AtomicBool::new(true));
        let recycled = Arc::new(AtomicUsize::new(0));
        let driver = driver(
            mem.clone(),
            &[TX],
            flooding.clone(),
            recycled.clone(),
            Some(waker.clone()),
        );
        let worker = {
            let (mem, irq, waker, stop) = (mem.clone(), irq.clone(), waker.clone(), stop.clone());
            std::thread::spawn(move || run(session, &mem, &irq, &waker, &stop))
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while recycled.load(Ordering::SeqCst) < 20 * usize::from(SIZE) {
            assert!(
                Instant::now() < deadline,
                "the worker stopped coming back after {} packets",
                recycled.load(Ordering::SeqCst)
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let t0 = Instant::now();
        stop.store(true, Ordering::Release);
        waker.wake();
        while !worker.is_finished() {
            assert!(t0.elapsed() < Duration::from_secs(2), "the worker did not stop");
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(worker.join().unwrap());
        flooding.store(false, Ordering::Release);
        driver.join().unwrap();
        let _ = std::fs::remove_file(&dir);
    }
}
