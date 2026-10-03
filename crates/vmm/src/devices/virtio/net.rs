//! virtio-net (virtio 1.3 §5.1): a VM's network device, its frames crossing to the VM's
//! network process in shared memory (docs/design/architecture.md D31, PM M83). The
//! network process terminates every flow at L4 and never sees guest memory: this device
//! copies each frame between the virtqueues and the frame ring, once.
//!
//! The link is a pipe between two of shards' own processes, not a wire: checksums are
//! offered both ways and never computed (the network process terminates TCP and UDP, and
//! marks what it sends as checked), large segments come from the guest whole (TSO), and
//! the MTU is 65520, so that a frame carries as much as the ring's records hold
//! (networking.md R1; RootlessKit's pasta went from 0.24 to 31.9 Gbps with the MTU alone).

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use shards_netring::{Consumer, Producer, Region};

use super::queue::{Chain, Queue, QueueError, with};
use super::{Activation, DeviceInterrupt, VirtioDevice, feature};
use crate::memory::GuestMemory;
use crate::warn;

pub const DEVICE_ID: u32 = 1;
const QUEUE_SIZE: u16 = 256;
const RX: usize = 0;
const TX: usize = 1;
/// The virtio-net header, as VERSION_1 makes it whatever else is negotiated (§5.1.6).
pub const HEADER: usize = 12;
/// The MTU the device offers: 65535, less the room a VLAN tag takes, as pasta offers it.
pub const MTU: u16 = 65520;

const F_CSUM: u64 = 1 << 0;
const F_GUEST_CSUM: u64 = 1 << 1;
const F_MTU: u64 = 1 << 3;
const F_MAC: u64 = 1 << 5;
const F_HOST_TSO4: u64 = 1 << 11;
const F_HOST_TSO6: u64 = 1 << 12;
const F_MRG_RXBUF: u64 = 1 << 15;
const F_STATUS: u64 = 1 << 16;
const S_LINK_UP: u16 = 1;

/// A VM's network host side: the frame ring its network process shares, and the two
/// doorbells, this side's to sleep on and the network process's to ring.
#[derive(Debug, Clone)]
pub struct NetHost {
    pub region: Arc<OwnedFd>,
    pub wake_me: Arc<OwnedFd>,
    pub wake_peer: Arc<OwnedFd>,
    pub mac: [u8; 6],
}

fn dup(fd: &OwnedFd) -> io::Result<OwnedFd> {
    fd.try_clone()
}

/// A self-pipe for the queues' notifications.
#[derive(Debug)]
struct Waker {
    read: OwnedFd,
    write: OwnedFd,
}

impl Waker {
    fn new() -> io::Result<Waker> {
        let (read, write) = shards_netring::doorbell()?;
        Ok(Waker { read, write })
    }

    fn wake(&self) {
        // SAFETY: one byte from a valid buffer to our own non-blocking descriptor.
        unsafe { libc::write(self.write.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
    }

    fn drain(&self) {
        let mut buf = [0u8; 64];
        // SAFETY: into a local buffer, from our own non-blocking descriptor.
        while unsafe { libc::read(self.read.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
    }
}

/// What the worker holds while it runs.
struct Session {
    queues: Vec<Queue>,
    /// A guest frame taken from TX while the ring was full, sent first next time.
    pending_tx: Option<Chain>,
    /// RX buffers taken for a frame that needs more of them than the guest gave yet.
    pending_rx: Vec<Chain>,
}

struct Worker {
    thread: JoinHandle<Session>,
    stop: Arc<AtomicBool>,
}

pub struct Net {
    host: NetHost,
    region: Arc<Region>,
    waker: Arc<Waker>,
    context: Option<(Arc<GuestMemory>, Arc<DeviceInterrupt>)>,
    worker: Option<Worker>,
    paused: Option<Session>,
}

impl std::fmt::Debug for Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Net")
            .field("mac", &self.host.mac)
            .finish_non_exhaustive()
    }
}

impl Net {
    pub fn new(host: NetHost) -> Result<Net, String> {
        let region = Region::map(dup(&host.region).map_err(|e| format!("virtio-net: {e}"))?)
            .map_err(|e| format!("virtio-net: the frame ring: {e}"))?;
        Ok(Net {
            host,
            region: Arc::new(region),
            waker: Arc::new(Waker::new().map_err(|e| format!("virtio-net: {e}"))?),
            context: None,
            worker: None,
            paused: None,
        })
    }

    fn start(&mut self, session: Session) -> Result<(), String> {
        let (memory, interrupt) = self
            .context
            .clone()
            .ok_or("virtio-net started before activation")?;
        let stop = Arc::new(AtomicBool::new(false));
        let (flag, waker, region) = (stop.clone(), self.waker.clone(), self.region.clone());
        let fds = |e: io::Error| format!("virtio-net: {e}");
        let ends = (
            dup(&self.host.wake_peer).map_err(fds)?,
            dup(&self.host.wake_peer).map_err(fds)?,
            dup(&self.host.wake_me).map_err(fds)?,
        );
        let thread = thread::Builder::new()
            .name("virtio-net".into())
            .spawn(move || {
                // The device's frames go one way, the network process's come the other.
                let producer = region.producer(0, ends.0);
                let consumer = region.consumer(1, ends.1, ends.2);
                run(session, producer, consumer, &memory, &interrupt, &waker, &flag)
            })
            .map_err(|e| format!("spawning the virtio-net worker: {e}"))?;
        self.worker = Some(Worker { thread, stop });
        Ok(())
    }

    fn stop(&mut self) -> Option<Session> {
        let w = self.worker.take()?;
        w.stop.store(true, Ordering::Release);
        self.waker.wake();
        match w.thread.join() {
            Ok(s) => Some(s),
            Err(_) => {
                warn!("the virtio-net worker ended abnormally");
                None
            }
        }
    }
}

impl VirtioDevice for Net {
    fn device_id(&self) -> u32 {
        DEVICE_ID
    }

    fn features(&self) -> u64 {
        feature::VERSION_1
            | feature::EVENT_IDX
            | feature::INDIRECT_DESC
            | F_CSUM
            | F_GUEST_CSUM
            | F_MTU
            | F_MAC
            | F_HOST_TSO4
            | F_HOST_TSO6
            | F_MRG_RXBUF
            | F_STATUS
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE, QUEUE_SIZE]
    }

    /// mac, status, max_virtqueue_pairs, mtu (§5.1.4).
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut config = [0u8; 12];
        config[..6].copy_from_slice(&self.host.mac);
        config[6..8].copy_from_slice(&S_LINK_UP.to_le_bytes());
        config[8..10].copy_from_slice(&1u16.to_le_bytes());
        config[10..12].copy_from_slice(&MTU.to_le_bytes());
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        for (i, b) in data.iter_mut().enumerate() {
            *b = start
                .checked_add(i)
                .and_then(|at| config.get(at))
                .copied()
                .unwrap_or(0);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn activate(&mut self, a: Activation) -> Result<(), String> {
        self.stop();
        self.context = Some((a.memory, a.interrupt));
        self.start(Session {
            queues: a.queues,
            pending_tx: None,
            pending_rx: Vec::new(),
        })
    }

    fn notify(&self, _queue: u16) {
        self.waker.wake();
    }

    fn reset(&mut self) {
        self.stop();
        self.paused = None;
        self.context = None;
    }

    fn pause(&mut self) -> Vec<super::queue::QueueState> {
        let Some(s) = self.stop() else {
            return Vec::new();
        };
        let states = s.queues.iter().map(Queue::state).collect();
        self.paused = Some(s);
        states
    }

    fn resume(&mut self) -> Result<(), String> {
        match self.paused.take() {
            Some(s) => self.start(s),
            None => Ok(()),
        }
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Moves frames until stopped. A malformed ring marks the device as needing reset; a
/// broken frame ring, the network process's fault, cuts the guest's network off alone.
fn run(
    mut s: Session,
    mut tx: Producer<'_>,
    mut rx: Consumer<'_>,
    mem: &GuestMemory,
    irq: &DeviceInterrupt,
    waker: &Waker,
    stop: &AtomicBool,
) -> Session {
    let mut broken = false;
    while !stop.load(Ordering::Acquire) {
        let more = match step(&mut s, &mut tx, &mut rx, mem, irq, broken) {
            Ok(Step::More) => true,
            // Asked to be rung for the next frame: one that came before the ask rang
            // nothing, so it is taken now, not slept past.
            // A broken ring's frames are never taken.
            Ok(Step::Idle) => !broken && matches!(rx.arm(), Ok(true)),
            // The ring's frames wait for the driver's buffers, whose notice wakes this.
            Ok(Step::Starved) => false,
            Ok(Step::Broken(why)) => {
                warn!("virtio-net: {why}; the guest's network is cut off");
                broken = true;
                false
            }
            Err(e) => {
                warn!("virtio-net: {e}; device needs reset");
                irq.fail();
                return s;
            }
        };
        if more {
            continue;
        }
        // Wait for a notification or a frame: the network process rings this side for a
        // frame, or for room once it has drained a full ring.
        let mut fds = [
            libc::pollfd {
                fd: waker.read.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: rx.waits_on(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two pollfds.
        unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        waker.drain();
        let mut buf = [0u8; 64];
        // SAFETY: draining our own non-blocking doorbell into a local buffer.
        while unsafe { libc::read(rx.waits_on(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
    }
    s
}

enum Step {
    More,
    /// Nothing to do until the guest or the network process says so.
    Idle,
    /// Frames wait in the ring for buffers the driver has not given yet.
    Starved,
    Broken(String),
}

/// One round: guest frames to the ring, ring frames to the guest, a ring's worth each at
/// most, then an interrupt if the driver wants one.
fn step(
    s: &mut Session,
    tx: &mut Producer<'_>,
    rx: &mut Consumer<'_>,
    mem: &GuestMemory,
    irq: &DeviceInterrupt,
    broken: bool,
) -> Result<Step, QueueError> {
    let [rxq, txq] = s.queues.as_mut_slice() else {
        return Ok(Step::Idle);
    };
    let mut used = [false; 2];
    let mut more = false;
    let mut starved = false;
    // Guest → network process.
    let mut sent = 0;
    'tx: loop {
        with(mem, |a| txq.disable_notification(a))?;
        loop {
            if sent == usize::from(txq.size()) {
                more = true;
                break 'tx;
            }
            let chain = match s.pending_tx.take() {
                Some(c) => c,
                None => match with(mem, |a| txq.pop(a))? {
                    Some(c) => c,
                    None => break,
                },
            };
            if !broken {
                match push(tx, &chain, mem) {
                    Ok(true) => {}
                    Ok(false) => {
                        // No room: the frame waits, and the network process rings once it
                        // drains the ring.
                        s.pending_tx = Some(chain);
                        break 'tx;
                    }
                    Err(why) => return Ok(Step::Broken(why)),
                }
            }
            with(mem, |a| txq.add_used(a, chain.head, 0))?;
            used[TX] = true;
            sent += 1;
        }
        if !with(mem, |a| txq.enable_notification(a))? {
            break;
        }
    }
    // Network process → guest, while there are frames and buffers for them.
    let mut received = 0;
    // A broken ring takes no more frames; the guest's TX still drains, its frames
    // dropped.
    while !broken && received <= usize::from(rxq.size()) {
        if received == usize::from(rxq.size()) {
            more = true;
            break;
        }
        let n = match rx.peek_len() {
            Ok(Some(n)) => n,
            Ok(None) => break,
            Err(b) => return Ok(Step::Broken(b.to_string())),
        };
        if n < HEADER {
            return Ok(Step::Broken("a frame shorter than its header".into()));
        }
        // Enough buffers for the frame, merged as MRG_RXBUF lets them be.
        let mut room: usize = s.pending_rx.iter().map(writable).sum();
        while room < n {
            let Some(chain) = with(mem, |a| rxq.pop(a))? else {
                break;
            };
            room += writable(&chain);
            s.pending_rx.push(chain);
        }
        if room < n {
            // Too few buffers: wait for the driver to add some.
            if with(mem, |a| rxq.enable_notification(a))? {
                continue;
            }
            starved = true;
            break;
        }
        let chains = std::mem::take(&mut s.pending_rx);
        let lens = match deliver(rx, &chains, mem) {
            Ok(lens) => lens,
            Err(why) => return Ok(Step::Broken(why)),
        };
        for (chain, len) in chains.iter().zip(lens) {
            with(mem, |a| rxq.add_used(a, chain.head, len))?;
        }
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
    Ok(if more {
        Step::More
    } else if starved {
        Step::Starved
    } else {
        Step::Idle
    })
}

fn writable(c: &Chain) -> usize {
    c.writable().map(|d| d.len as usize).sum()
}

/// A guest frame, header and all, into the ring if there is room: false if there is
/// none. A frame past the ring's largest, or one outside guest memory, is dropped, as a
/// NIC drops what it cannot send.
fn push(tx: &mut Producer<'_>, chain: &Chain, mem: &GuestMemory) -> Result<bool, String> {
    let n: usize = chain.readable().map(|d| d.len as usize).sum();
    let mut spans = Vec::with_capacity(chain.descriptors.len());
    for d in chain.readable() {
        let Ok(ptr) = mem.host_ptr(d.addr, d.len as usize) else {
            return Ok(true);
        };
        spans.push((ptr, d.len as usize));
    }
    match tx.try_push_with(n, |dst| {
        let mut off = 0;
        for (ptr, len) in &spans {
            // SAFETY: guest memory checked by host_ptr, into the record's `n` bytes.
            unsafe { std::ptr::copy_nonoverlapping(*ptr as *const u8, dst.add(off), *len) };
            off += len;
        }
    }) {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(b) => Err(b.to_string()),
    }
}

/// The next frame into `chains`, its header's `num_buffers` the count of them; each
/// chain's bytes written.
fn deliver(rx: &mut Consumer<'_>, chains: &[Chain], mem: &GuestMemory) -> Result<Vec<u32>, String> {
    let mut lens = vec![0u32; chains.len()];
    let used = chains.len();
    rx.pop(|n, copy| {
        let mut done = 0usize;
        for (i, chain) in chains.iter().enumerate() {
            for d in chain.writable() {
                if done == n {
                    break;
                }
                let take = (d.len as usize).min(n - done);
                let Ok(ptr) = mem.host_ptr(d.addr, take) else {
                    continue;
                };
                copy(done, ptr, take);
                done += take;
                if let Some(l) = lens.get_mut(i) {
                    *l += take as u32;
                }
            }
        }
        // num_buffers, the header's last field (§5.1.6): how many chains hold the frame.
        if let Some(first) = chains.first().and_then(|c| c.writable().next())
            && first.len as usize >= HEADER
            && let Ok(p) = mem.host_ptr(first.addr + 10, 2)
        {
            let count = u16::try_from(used).unwrap_or(u16::MAX).to_le_bytes();
            // SAFETY: two bytes of guest memory checked by host_ptr.
            unsafe { std::ptr::copy_nonoverlapping(count.as_ptr(), p, 2) };
        }
    })
    .map_err(|b| b.to_string())?;
    // Chains that held none of the frame still go back, empty.
    Ok(lens)
}
