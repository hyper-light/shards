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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use shards_netring::{Consumer, Producer, Region};

use super::queue::{Chain, Queue, QueueError, with};
use super::{Activation, DeviceInterrupt, VirtioDevice, feature};
use crate::memory::GuestMemory;
use crate::sync::{lock, wait_timeout};
use crate::{debug, warn};

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
/// doorbells, this side's to sleep on and the network process's to ring; and the flush
/// its owner asks of the device as the guest's command ends.
#[derive(Debug, Clone)]
pub struct NetHost {
    pub region: Arc<OwnedFd>,
    pub wake_me: Arc<OwnedFd>,
    pub wake_peer: Arc<OwnedFd>,
    pub mac: [u8; 6],
    pub flush: TxFlush,
}

/// Waits until every frame the guest has given the device to send is in the frame ring:
/// what a VM's owner asks as its guest's command ends, before the run's ports close and
/// the VM goes, so that a datagram a command sends just before it exits reaches its
/// network process, as a container's reaches its host, rather than going with the VM.
#[derive(Debug, Clone, Default)]
pub struct TxFlush(Arc<Flushes>);

#[derive(Debug, Default)]
struct Flushes {
    /// How many flushes have been asked for.
    asked: AtomicU64,
    /// The most asked for when the worker last found the guest's frames all in the ring.
    answered: Mutex<u64>,
    changed: Condvar,
    /// The worker's waker, while a worker runs.
    waker: Mutex<Option<Arc<Waker>>>,
}

impl TxFlush {
    /// True once every frame the guest had given the device when this was asked is in the
    /// ring, or at once where no worker runs (no session, or paused); false if `deadline`
    /// came first.
    pub fn flush(&self, deadline: Instant) -> bool {
        let Some(waker) = lock(&self.0.waker).clone() else {
            return true;
        };
        let n = self.0.asked.fetch_add(1, Ordering::AcqRel).saturating_add(1);
        waker.wake();
        let mut answered = lock(&self.0.answered);
        while *answered < n {
            let Some(left) = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
            else {
                return false;
            };
            answered = wait_timeout(&self.0.changed, answered, left);
        }
        true
    }

    /// The worker found every frame given before it read `asked` in the ring.
    fn answer(&self, asked: u64) {
        let mut answered = lock(&self.0.answered);
        if asked > *answered {
            *answered = asked;
            drop(answered);
            self.0.changed.notify_all();
        }
    }
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
    /// The chain of the guest frame taken from TX last; held while the ring was full, and
    /// sent first next time.
    tx: Chain,
    tx_held: bool,
    /// Whether the last round took every frame the guest had given to send.
    tx_drained: bool,
    /// RX buffers: the first `rx_held` taken for a frame that needs more of them than the
    /// guest gave yet. Chains and `rx_lens`, what each held of the last frame, keep their
    /// memory from one frame to the next (review 2.22).
    rx: Vec<Chain>,
    rx_held: usize,
    rx_lens: Vec<u32>,
}

impl Session {
    fn new(queues: Vec<Queue>) -> Session {
        Session {
            queues,
            tx: Chain::default(),
            tx_held: false,
            tx_drained: false,
            rx: Vec::new(),
            rx_held: 0,
            rx_lens: Vec::new(),
        }
    }
}

struct Worker {
    thread: JoinHandle<Option<Session>>,
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

    /// Starts the worker on `session`; if it cannot, the session waits paused, as a
    /// snapshot's pause leaves it, for a resume or a reset.
    fn start(&mut self, session: Session) -> Result<(), String> {
        let Some((memory, interrupt)) = self.context.clone() else {
            self.paused = Some(session);
            return Err("virtio-net started before activation".into());
        };
        let stop = Arc::new(AtomicBool::new(false));
        let (flag, waker, region) = (stop.clone(), self.waker.clone(), self.region.clone());
        let flush = self.host.flush.clone();
        // A worker before this one may have ended answering everything: this one answers
        // what is asked from now on.
        *lock(&flush.0.answered) = flush.0.asked.load(Ordering::Acquire);
        let ends = match (
            dup(&self.host.wake_peer),
            dup(&self.host.wake_peer),
            dup(&self.host.wake_me),
        ) {
            (Ok(a), Ok(b), Ok(c)) => (a, b, c),
            (Err(e), ..) | (_, Err(e), _) | (.., Err(e)) => {
                self.paused = Some(session);
                return Err(format!("virtio-net: {e}"));
            }
        };
        let spawned = super::worker::spawn("virtio-net", session, move |session| {
            // The device's frames go one way, the network process's come the other.
            let producer = region.producer(0, ends.0);
            let consumer = region.consumer(1, ends.1, ends.2);
            let session = run(
                session, producer, consumer, &memory, &interrupt, &waker, &flag, &flush,
            );
            // Nothing takes the guest's frames now: no flush waits for it.
            flush.answer(u64::MAX);
            Some(session)
        });
        match spawned {
            Ok(thread) => {
                self.worker = Some(Worker { thread, stop });
                *lock(&self.host.flush.0.waker) = Some(self.waker.clone());
                Ok(())
            }
            Err((e, session)) => {
                self.paused = Some(session);
                Err(e)
            }
        }
    }

    fn stop(&mut self) -> Option<Session> {
        let w = self.worker.take()?;
        // A flush asked from here on finds no worker, and does not wait for one.
        *lock(&self.host.flush.0.waker) = None;
        w.stop.store(true, Ordering::Release);
        self.waker.wake();
        match w.thread.join() {
            Ok(s) => s,
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
        // An activation that fails leaves its queues: the driver sets the device up anew.
        self.start(Session::new(a.queues))
            .inspect_err(|_| self.paused = None)
    }

    fn notify(&self, _queue: u16) {
        self.waker.wake();
    }

    fn reset(&mut self) {
        self.stop();
        self.paused = None;
        self.context = None;
    }

    /// The queues' states as a restore resumes them: before the chains the session holds
    /// unused, a TX frame waiting for room in the ring and RX buffers gathered for a frame
    /// larger than they are, which a restore takes again from the driver's rings; a
    /// session resumed here goes on with them (review 2.20).
    fn pause(&mut self) -> Vec<super::queue::QueueState> {
        if let Some(s) = self.stop() {
            self.paused = Some(s);
        }
        // Paused already, as after a resume that failed: the same states.
        let Some(s) = &self.paused else {
            return Vec::new();
        };
        let mut held = [0u16; 2];
        held[RX] = u16::try_from(s.rx_held).unwrap_or(u16::MAX);
        held[TX] = u16::from(s.tx_held);
        s.queues
            .iter()
            .enumerate()
            .map(|(i, q)| q.state_before(held.get(i).copied().unwrap_or(0)))
            .collect()
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

/// Moves frames until stopped, answering each flush once a round has taken every frame
/// the guest had given before it. A malformed ring marks the device as needing reset; a
/// broken frame ring, the network process's fault, cuts the guest's network off alone.
#[allow(clippy::too_many_arguments)]
fn run(
    mut s: Session,
    mut tx: Producer<'_>,
    mut rx: Consumer<'_>,
    mem: &GuestMemory,
    irq: &DeviceInterrupt,
    waker: &Waker,
    stop: &AtomicBool,
    flush: &TxFlush,
) -> Session {
    let mut broken = false;
    while !stop.load(Ordering::Acquire) {
        // Read before the round: every frame given before a flush asked is the round's.
        let asked = flush.0.asked.load(Ordering::Acquire);
        let stepped = step(&mut s, &mut tx, &mut rx, mem, irq, broken);
        if s.tx_drained {
            flush.answer(asked);
        }
        let more = match stepped {
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
                if irq.fail() {
                    warn!("virtio-net: {e}; device needs reset");
                } else {
                    debug!("virtio-net: {e}; device needs reset");
                }
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
    s.tx_drained = false;
    let [rxq, txq] = s.queues.as_mut_slice() else {
        s.tx_drained = true;
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
            if !s.tx_held && !with(mem, |a| txq.pop_into(a, &mut s.tx))? {
                break;
            }
            s.tx_held = false;
            if !broken {
                match push(tx, &s.tx, mem) {
                    Ok(true) => {}
                    Ok(false) => {
                        // No room: the frame waits, and the network process rings once it
                        // drains the ring.
                        s.tx_held = true;
                        break 'tx;
                    }
                    Err(why) => return Ok(Step::Broken(why)),
                }
            }
            with(mem, |a| txq.add_used(a, s.tx.head, 0))?;
            used[TX] = true;
            sent += 1;
        }
        if !with(mem, |a| txq.enable_notification(a))? {
            // Nothing came while it was taking: every frame given is in the ring.
            s.tx_drained = true;
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
        let mut room: usize = s.rx.iter().take(s.rx_held).map(writable).sum();
        while room < n {
            if s.rx.len() == s.rx_held {
                s.rx.push(Chain::default());
            }
            let Some(chain) = s.rx.get_mut(s.rx_held) else {
                break;
            };
            if !with(mem, |a| rxq.pop_into(a, chain))? {
                break;
            }
            // A chain with no room for any of a frame goes back at once, empty (review
            // 2.23): held, it would count among the frame's buffers, and first among them
            // leave its header's `num_buffers` unwritten.
            let cap = writable(chain);
            if cap == 0 {
                with(mem, |a| rxq.add_used(a, chain.head, 0))?;
                used[RX] = true;
                continue;
            }
            room += cap;
            s.rx_held += 1;
        }
        if room < n {
            // Too few buffers: wait for the driver to add some.
            if with(mem, |a| rxq.enable_notification(a))? {
                continue;
            }
            starved = true;
            break;
        }
        let chains = s.rx.get(..s.rx_held).unwrap_or_default();
        if let Err(why) = deliver(rx, chains, &mut s.rx_lens, mem) {
            return Ok(Step::Broken(why));
        }
        // The frame's buffers go back together: a driver that saw some used and not the
        // rest would drop the frame and take what follows for them (review 2.34, PM M104).
        let heads = chains.iter().map(|c| c.head);
        with(mem, |a| rxq.add_used_all(a, heads.zip(s.rx_lens.iter().copied())))?;
        s.rx_held = 0;
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
    let mut n = 0usize;
    for d in chain.readable() {
        if mem.host_ptr(d.addr, d.len as usize).is_err() {
            return Ok(true);
        }
        n = n.saturating_add(d.len as usize);
    }
    match tx.try_push_with(n, |dst| {
        let mut off = 0;
        for d in chain.readable() {
            // Checked above, in a memory map that does not change while a device runs.
            if let Ok(ptr) = mem.host_ptr(d.addr, d.len as usize) {
                // SAFETY: guest memory checked by host_ptr, into the record's `n` bytes,
                // of which these are the next.
                unsafe { std::ptr::copy_nonoverlapping(ptr as *const u8, dst.add(off), d.len as usize) };
            }
            off += d.len as usize;
        }
    }) {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(b) => Err(b.to_string()),
    }
}

/// The next frame into `chains`, its header's `num_buffers` the count of them; each
/// chain's bytes written into `lens`.
fn deliver(
    rx: &mut Consumer<'_>,
    chains: &[Chain],
    lens: &mut Vec<u32>,
    mem: &GuestMemory,
) -> Result<(), String> {
    lens.clear();
    lens.resize(chains.len(), 0);
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
            && let Some(at) = first.addr.checked_add(10)
            && let Ok(p) = mem.host_ptr(at, 2)
        {
            let count = u16::try_from(used).unwrap_or(u16::MAX).to_le_bytes();
            // SAFETY: two bytes of guest memory checked by host_ptr.
            unsafe { std::ptr::copy_nonoverlapping(count.as_ptr(), p, 2) };
        }
    })
    .map_err(|b| b.to_string())?;
    // Chains that held none of the frame still go back, empty.
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::super::queue::QueueConfig;
    use super::*;
    use crate::devices::Interrupt;

    const BASE: u64 = 0x8000_0000;
    const SIZE: u16 = 8;
    const RX_DESC: u64 = BASE;
    const RX_AVAIL: u64 = BASE + 0x1000;
    const RX_USED: u64 = BASE + 0x2000;
    const TX_DESC: u64 = BASE + 0x3000;
    const TX_AVAIL: u64 = BASE + 0x4000;
    const TX_USED: u64 = BASE + 0x5000;
    const DATA: u64 = BASE + 0x8000;
    /// VIRTQ_DESC_F_WRITE (virtio 1.2 §2.7.5): a buffer the device writes.
    const DESC_WRITE: u16 = 2;

    struct Line;

    impl Interrupt for Line {
        fn set_level(&self, _: bool) {}
    }

    /// An activation, and a resume after a snapshot's pause, whose worker the system
    /// refuses fail alone: the paused session, with the chains it holds, stays, and the
    /// next activation or resume runs (review 1.14).
    #[test]
    fn a_refused_worker_leaves_the_session_paused() {
        let (_net_waits, device_rings) = shards_netring::doorbell().unwrap();
        let (device_waits, _net_rings) = shards_netring::doorbell().unwrap();
        let host = NetHost {
            region: Arc::new(shards_netring::memory().unwrap()),
            wake_me: Arc::new(device_waits),
            wake_peer: Arc::new(device_rings),
            mac: [2, 0, 0, 0, 0, 1],
            flush: TxFlush::default(),
        };
        let mut net = Net::new(host).unwrap();
        let page = crate::platform::page_size().unwrap();
        let mem = Arc::new(GuestMemory::anonymous(&[(BASE, 16 * page)]).unwrap());
        let irq = Arc::new(DeviceInterrupt::new(Arc::new(Line)));
        let activation = || {
            let queue = |desc, avail, used| {
                let cfg = QueueConfig {
                    size: SIZE,
                    desc,
                    avail,
                    used,
                    ready: true,
                };
                Queue::new(cfg, SIZE, &mem, feature::VERSION_1).unwrap()
            };
            Activation {
                memory: mem.clone(),
                queues: vec![
                    queue(RX_DESC, RX_AVAIL, RX_USED),
                    queue(TX_DESC, TX_AVAIL, TX_USED),
                ],
                interrupt: irq.clone(),
                features: feature::VERSION_1,
                restored: false,
            }
        };
        super::super::worker::REFUSE.set(true);
        let refused = net.activate(activation());
        super::super::worker::REFUSE.set(false);
        assert!(refused.unwrap_err().starts_with("spawning the virtio-net worker"));
        // Its queues gone with it.
        assert!(net.worker.is_none() && net.paused.is_none());
        net.activate(activation()).unwrap();
        assert_eq!(net.pause().len(), 2);
        super::super::worker::REFUSE.set(true);
        assert!(net.resume().is_err());
        super::super::worker::REFUSE.set(false);
        assert!(net.worker.is_none());
        assert_eq!(net.pause().len(), 2, "paused still");
        net.resume().unwrap();
        assert!(net.worker.is_some() && net.paused.is_none());
    }

    /// A flush returns once the frame the guest gave is in the ring, though the guest never
    /// rang for it: what a run's end relies on before its ports close (the published UDP
    /// flake, 2026-10-10). A device with no worker has nothing to flush.
    #[test]
    fn a_flush_returns_once_the_guests_frames_are_in_the_ring() {
        let (net_waits, device_rings) = shards_netring::doorbell().unwrap();
        let (device_waits, _net_rings) = shards_netring::doorbell().unwrap();
        let region = Arc::new(shards_netring::memory().unwrap());
        let host = NetHost {
            region: region.clone(),
            wake_me: Arc::new(device_waits),
            wake_peer: Arc::new(device_rings),
            mac: [2, 0, 0, 0, 0, 1],
            flush: TxFlush::default(),
        };
        let flush = host.flush.clone();
        let soon = || Instant::now() + std::time::Duration::from_secs(10);
        let mut net = Net::new(host).unwrap();
        let began = Instant::now();
        assert!(flush.flush(soon()), "no worker, nothing to wait for");
        assert!(began.elapsed() < std::time::Duration::from_secs(1));
        let page = crate::platform::page_size().unwrap();
        let mem = Arc::new(GuestMemory::anonymous(&[(BASE, 16 * page)]).unwrap());
        let queue = |desc, avail, used| {
            let cfg = QueueConfig {
                size: SIZE,
                desc,
                avail,
                used,
                ready: true,
            };
            Queue::new(cfg, SIZE, &mem, feature::VERSION_1).unwrap()
        };
        net.activate(Activation {
            memory: mem.clone(),
            queues: vec![
                queue(RX_DESC, RX_AVAIL, RX_USED),
                queue(TX_DESC, TX_AVAIL, TX_USED),
            ],
            interrupt: Arc::new(DeviceInterrupt::new(Arc::new(Line))),
            features: feature::VERSION_1,
            restored: false,
        })
        .unwrap();
        // The worker's first round finds nothing and sleeps.
        std::thread::sleep(std::time::Duration::from_millis(100));
        // A frame given, the device never notified.
        let f = frame(60);
        let a = mem.access().unwrap();
        a.write(DATA, &f).unwrap();
        a.write_obj(TX_DESC, DATA).unwrap();
        a.write_obj(TX_DESC + 8, u32::try_from(f.len()).unwrap()).unwrap();
        a.write_obj(TX_DESC + 12, 0u16).unwrap();
        a.write_obj(TX_DESC + 14, 0u16).unwrap();
        a.write_obj(TX_AVAIL + 4, 0u16).unwrap();
        a.write_obj(TX_AVAIL + 2, 1u16).unwrap();
        // While guest memory is this thread's, the worker cannot take the frame: the flush
        // is not answered, however promptly the worker wakes.
        let shortly = Instant::now() + std::time::Duration::from_millis(300);
        assert!(!flush.flush(shortly), "answered before the frame was taken");
        drop(a);
        assert!(flush.flush(soon()));
        let ring = Region::map(region.try_clone().unwrap()).unwrap();
        let (_nobody, rings) = shards_netring::doorbell().unwrap();
        let mut from_guest = ring.consumer(0, rings, net_waits);
        let mut got = vec![0u8; f.len()];
        let n = from_guest
            .pop(|n, copy| {
                copy(0, got.as_mut_ptr(), n.min(f.len()));
                n
            })
            .unwrap();
        assert_eq!(n, Some(f.len()), "the frame is in the ring as the flush returns");
        assert_eq!(got, f);
        // Stopped, the device's flush waits for nothing.
        net.reset();
        assert!(flush.flush(soon()));
    }

    /// A device's session on guest memory, and the network process's ends of its frame
    /// ring: what `step` runs against, without a worker.
    struct Rig {
        mem: Arc<GuestMemory>,
        session: Session,
        region: Region,
        rx_published: u16,
        irq: DeviceInterrupt,
    }

    impl Rig {
        fn new() -> Rig {
            let page = crate::platform::page_size().unwrap();
            let mem = Arc::new(GuestMemory::anonymous(&[(BASE, 16 * page)]).unwrap());
            let queue = |desc, avail, used| {
                let cfg = QueueConfig {
                    size: SIZE,
                    desc,
                    avail,
                    used,
                    ready: true,
                };
                Queue::new(cfg, SIZE, &mem, feature::VERSION_1).unwrap()
            };
            let queues = vec![
                queue(RX_DESC, RX_AVAIL, RX_USED),
                queue(TX_DESC, TX_AVAIL, TX_USED),
            ];
            Rig {
                session: Session::new(queues),
                mem,
                region: Region::map(shards_netring::memory().unwrap()).unwrap(),
                rx_published: 0,
                irq: DeviceInterrupt::new(Arc::new(Line)),
            }
        }

        /// Gives the driver's RX descriptor `i`, one buffer, as a chain of its own.
        fn rx_buffer(&mut self, i: u16, addr: u64, len: u32, flags: u16) {
            let a = self.mem.access().unwrap();
            let d = RX_DESC + 16 * u64::from(i);
            a.write_obj(d, addr).unwrap();
            a.write_obj(d + 8, len).unwrap();
            a.write_obj(d + 12, flags).unwrap();
            a.write_obj(d + 14, 0u16).unwrap();
            a.write_obj(RX_AVAIL + 4 + 2 * u64::from(self.rx_published % SIZE), i)
                .unwrap();
            self.rx_published = self.rx_published.wrapping_add(1);
            a.write_obj(RX_AVAIL + 2, self.rx_published).unwrap();
        }

        /// One round, with `frames` waiting from the network process.
        fn step(&mut self, frames: &[&[u8]]) -> Step {
            // Each doorbell's other end, kept for the round: ringing one closed fails.
            let (_net_waits, device_rings) = shards_netring::doorbell().unwrap();
            let (device_waits, net_rings) = shards_netring::doorbell().unwrap();
            let (_tx_waits, tx_ring) = shards_netring::doorbell().unwrap();
            // Each round's ends start the ring afresh: the consumer's first, so that the
            // last round's tail is not taken for this one's.
            let mut rx = self.region.consumer(1, device_rings, device_waits);
            let mut from_net = self.region.producer(1, net_rings);
            for f in frames {
                assert_eq!(
                    from_net.try_push_with(f.len(), |dst| {
                        // SAFETY: `dst` has room for the frame.
                        unsafe { std::ptr::copy_nonoverlapping(f.as_ptr(), dst, f.len()) }
                    }),
                    Ok(Some(true))
                );
            }
            let mut tx = self.region.producer(0, tx_ring);
            step(&mut self.session, &mut tx, &mut rx, &self.mem, &self.irq, false).unwrap()
        }

        /// The used ring's entries so far: each chain's head and the bytes written.
        fn rx_used(&self) -> Vec<(u32, u32)> {
            let a = self.mem.access().unwrap();
            let n = a.read_obj::<u16>(RX_USED + 2).unwrap();
            (0..n)
                .map(|k| {
                    let e = RX_USED + 4 + 8 * u64::from(k % SIZE);
                    (a.read_obj::<u32>(e).unwrap(), a.read_obj::<u32>(e + 4).unwrap())
                })
                .collect()
        }
    }

    /// A frame of `len` bytes, its virtio header zeroed.
    fn frame(len: usize) -> Vec<u8> {
        (0..len).map(|i| if i < HEADER { 0 } else { i as u8 }).collect()
    }

    /// A chain with no room for the frame goes back at once, empty, and the frame goes to
    /// the next, whose header says it is the one buffer the frame takes (review 2.23).
    #[test]
    fn a_chain_with_no_room_goes_back_empty() {
        let mut r = Rig::new();
        r.rx_buffer(0, DATA, 64, 0);
        r.rx_buffer(1, DATA + 0x100, 256, DESC_WRITE);
        let f = frame(40);
        r.step(&[&f]);
        assert_eq!(r.rx_used(), [(0, 0), (1, 40)]);
        assert_eq!(r.session.rx_held, 0);
        let a = r.mem.access().unwrap();
        assert_eq!(a.read_obj::<u16>(DATA + 0x100 + 10).unwrap(), 1, "num_buffers");
        assert_eq!(a.read_obj::<u8>(DATA + 0x100 + 39).unwrap(), 39);
    }

    /// A frame spread over buffers is seen used all at once (virtio 1.2 §5.1.6.4.1): a
    /// driver polling the used ring while frames are delivered never sees some of a
    /// frame's buffers used and not the rest. A Linux guest counts one it does in
    /// rx_length_errors, drops it, and takes the next buffer for a frame's start.
    #[test]
    fn a_frames_buffers_are_used_together() {
        let mut r = Rig::new();
        // Frames of 150 bytes in buffers of 64: three each, two frames a round.
        let f = frame(150);
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            // The used index as a vCPU reads it: straight from guest memory, without the
            // host threads' lock, which would keep it from ever seeing a frame half used.
            let index = r.mem.host_ptr(RX_USED + 2, 2).unwrap() as usize;
            let done = &done;
            let observer = scope.spawn(move || {
                // SAFETY: two bytes of the rig's guest memory, aligned (the used ring is),
                // which outlives the scope; the device stores them atomically too.
                let used = unsafe { std::sync::atomic::AtomicU16::from_ptr(index as *mut u16) };
                let mut seen = 0u64;
                while !done.load(Ordering::Acquire) {
                    assert_eq!(
                        used.load(Ordering::Acquire) % 3,
                        0,
                        "a frame's buffers seen used apart"
                    );
                    seen += 1;
                }
                seen
            });
            // The observer stops however the rounds end.
            struct Stop<'a>(&'a AtomicBool);
            impl Drop for Stop<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }
            let stop = Stop(done);
            for _ in 0..2000 {
                for i in 0..6 {
                    r.rx_buffer(i, DATA + 0x100 * u64::from(i), 64, DESC_WRITE);
                }
                r.step(&[&f, &f]);
            }
            drop(stop);
            assert!(observer.join().unwrap() > 0);
        });
        assert_eq!(r.rx_used().len(), 12_000);
    }

    /// A buffer whose address is at the top of the guest's address space is no frame's,
    /// and its header is not written past it (review 2.23).
    #[test]
    fn a_buffer_at_the_top_of_the_address_space_takes_nothing() {
        let mut r = Rig::new();
        r.rx_buffer(0, u64::MAX - 7, 64, DESC_WRITE);
        r.step(&[&frame(HEADER)]);
        assert_eq!(r.rx_used(), [(0, 0)]);
    }

    /// A frame larger than the buffers given so far holds them until more come: those are
    /// what a pause saves as not yet taken (review 2.20).
    #[test]
    fn buffers_held_for_a_larger_frame_are_saved_as_not_taken() {
        let mut r = Rig::new();
        r.rx_buffer(0, DATA, 64, DESC_WRITE);
        assert!(matches!(r.step(&[&frame(100)]), Step::Starved));
        assert_eq!(r.session.rx_held, 1);
        let held = u16::try_from(r.session.rx_held).unwrap();
        assert_eq!(r.session.queues[RX].state_before(held).next_avail, 0);
        assert_eq!(r.session.queues[RX].state().next_avail, 1);
    }
}
