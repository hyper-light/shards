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

/// A self-pipe: vCPU threads write a byte to wake the worker out of poll(2).
#[derive(Debug)]
struct Waker {
    read: OwnedFd,
    write: OwnedFd,
}

impl Waker {
    fn new() -> io::Result<Waker> {
        let mut fds = [0; 2];
        // SAFETY: pipe(2) fills both descriptors on success.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let [r, w] = fds;
        // SAFETY: fresh descriptors that nothing else owns.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(r), OwnedFd::from_raw_fd(w)) };
        for fd in [r, w] {
            // SAFETY: fcntl(2) on our own descriptors.
            let ok = unsafe {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) == 0
                    && libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) == 0
            };
            if !ok {
                return Err(io::Error::last_os_error());
            }
        }
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
    while !stop.load(Ordering::Acquire) {
        if let Err(e) = step(&mut s, mem, irq) {
            warn!("virtio-vsock: {e}; device needs reset");
            irq.fail();
            return s;
        }
        interests.clear();
        interests.push(Interest {
            fd: waker.read.as_raw_fd(),
            read: true,
            write: false,
            token: None,
        });
        s.muxer.interests(&mut interests);
        let timeout = s
            .muxer
            .next_deadline()
            .map(|t| t.saturating_duration_since(Instant::now()));
        ready.clear();
        if let Err(e) = poll::wait(&interests, timeout, &mut ready) {
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

/// One round: guest packets to the host, host packets to the guest, then one interrupt
/// if the driver wants one. The event queue is never used: a restore resets the streams
/// its snapshot held on RX instead (muxer.rs).
fn step(s: &mut Session, mem: &GuestMemory, irq: &DeviceInterrupt) -> Result<(), QueueError> {
    let [rxq, txq, _] = s.queues.as_mut_slice() else {
        return Ok(());
    };
    let mut used = [false; 2];
    // Guest → host. Always drained first: the driver stops taking RX packets while too
    // many of its replies wait in TX (Linux virtio_transport_more_replies).
    // Each packet is parsed under an access, and its payload moved to or from the host
    // socket with none held.
    loop {
        with(mem, |a| txq.disable_notification(a))?;
        while let Some((packet, chain)) = with(mem, |a| Ok(txq.pop(a)?.map(|c| (parse_tx(&c, a), c))))? {
            if let Some((h, payload)) = packet {
                s.muxer.on_guest_packet(&h, &payload, mem);
            }
            with(mem, |a| txq.add_used(a, chain.head, 0))?;
            used[TX] = true;
        }
        if !with(mem, |a| txq.enable_notification(a))? {
            break;
        }
    }
    // Host → guest, while there are packets and buffers for them.
    while s.muxer.has_pending_rx() {
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
    Ok(())
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
