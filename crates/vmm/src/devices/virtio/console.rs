//! virtio-console (VIRTIO 1.3 §5.3): the guest's console, `hvc0`, one port and no
//! multiport control queue. Its output comes in buffers, one notification each, where a
//! UART traps every byte; its input goes into the buffers the guest posted, so a guest that
//! has posted none holds the host's input back in the host (its pipe or terminal), and
//! nothing is lost or held by the device. A snapshot keeps its queues alone.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use super::queue::{Chain, Queue, QueueError, serve_round};
use super::{Activation, DeviceInterrupt, QueueState, VirtioDevice, feature};
use crate::devices::said::{Kept, Said};
use crate::memory::GuestMemory;
use crate::sync::{lock, wait};
use crate::{debug, warn};

pub const DEVICE_ID: u32 = 3;
/// The receive ring: Linux's driver fills it with buffers of a page each, as many as it
/// holds (drivers/char/virtio_console.c `fill_queue`, l. 1290-1311), so each slot is a page
/// of input the guest holds unread. One, the least ring: the rest of the host's input waits
/// in the host, as it did behind the UART's FIFO.
const RECEIVE_SIZE: u16 = 1;
/// The transmit ring: Linux's console sends one buffer at a time and spins until the device
/// has used it before the next, under the port's lock (virtio_console.c `put_chars`,
/// l. 1118-1123, and `__send_to_port`, l. 596-640), so one is ever outstanding.
const TRANSMIT_SIZE: u16 = 1;
/// The most host input one receive buffer takes: Linux's driver posts a page each
/// (`fill_queue`, `alloc_buf(.., PAGE_SIZE, ..)`), and shards' guest kernels have 4 KiB
/// pages (x86-64's one size; arm64's `CONFIG_ARM64_4K_PAGES=y` in resources/kernel's base
/// configuration). A reader of host input need read no more at once: the rest waits in the
/// host.
pub const INPUT_AT_ONCE: usize = 4 << 10;
/// receiveq 0 and transmitq 1, port 0's (§5.3.2): no MULTIPORT, so no control queues.
const QUEUES: [u16; 2] = [RECEIVE_SIZE, TRANSMIT_SIZE];
const RECEIVEQ: usize = 0;
const TRANSMITQ: usize = 1;

/// A queue as it is served, with what serving it needs.
type Served = (Queue, Arc<GuestMemory>, Arc<DeviceInterrupt>);

/// The receive side, which host input drives.
#[derive(Default)]
struct Receive {
    /// The receive queue while the device runs, with what serving it needs; none before
    /// activation, while paused, after a reset, or once its ring was found malformed.
    queue: Option<Served>,
}

/// The console's host side, shared by the device and its VM's handle.
pub struct Port {
    /// What it keeps of its output to quote (D12).
    kept: Mutex<Kept>,
    /// The one buffer taken from the guest that its writer has not delivered.
    slot: Mutex<Slot>,
    /// Signalled as the slot is filled, or the device goes.
    filled: Condvar,
    /// Signalled as the writer has delivered what the slot held.
    drained: Condvar,
    receive: Mutex<Receive>,
    /// Signalled as the guest posts receive buffers, or the receive queue comes or goes.
    posted: Condvar,
    /// Whether its guest's kernel said it panicked (`devices::pvpanic`).
    panicked: AtomicBool,
}

impl std::fmt::Debug for Port {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Port").finish_non_exhaustive()
    }
}

impl Port {
    /// The whole lines of the last bytes its guest wrote to the console, as many as it keeps.
    ///
    /// What the guest sent before is delivered first: its writer is waited for while it
    /// holds a buffer, so a reset's last lines (a panic's) are among them.
    pub fn said(&self) -> Said {
        let mut s = lock(&self.slot);
        while (s.bytes.is_some() || s.writing) && !s.ended {
            s = wait(&self.drained, s);
        }
        drop(s);
        lock(&self.kept).said()
    }

    /// How many of its last bytes it keeps to quote.
    pub fn keep(&self) -> usize {
        lock(&self.kept).keep()
    }

    /// Whether its guest's kernel said it panicked, through its machine's pvpanic device.
    pub fn panicked(&self) -> bool {
        self.panicked.load(Ordering::Acquire)
    }

    /// Sets whether its guest's kernel panicked; whether it had already said so.
    pub(crate) fn tell_panicked(&self, panicked: bool) -> bool {
        self.panicked.swap(panicked, Ordering::AcqRel)
    }

    /// Gives host input to the guest, in the buffers it posted, waiting while it has posted
    /// none: nothing is lost, and what the guest has not taken waits in the caller.
    pub fn input(&self, mut bytes: &[u8]) {
        let mut r = lock(&self.receive);
        while !bytes.is_empty() {
            let took = match deliver(&mut r, bytes) {
                Ok(took) => took,
                Err(e) => {
                    // A malformed ring: the device needs reset (virtio 1.3 §2.1.2), after
                    // which the driver posts buffers anew.
                    if let Some((_, _, interrupt)) = r.queue.take() {
                        if interrupt.fail() {
                            warn!("virtio-console: {e}; device needs reset");
                        } else {
                            debug!("virtio-console: {e}; device needs reset");
                        }
                    }
                    0
                }
            };
            match took {
                0 => r = wait(&self.posted, r),
                n => bytes = bytes.get(n..).unwrap_or_default(),
            }
        }
    }
}

/// Puts as much of `bytes` as the next buffer the guest posted holds into it; how much.
/// None where it has posted none, its notifications then re-armed.
fn deliver(r: &mut Receive, bytes: &[u8]) -> Result<usize, QueueError> {
    let Some((queue, memory, interrupt)) = &mut r.queue else {
        return Ok(0);
    };
    let access = memory.access()?;
    loop {
        let Some(chain) = queue.pop(&access)? else {
            if queue.enable_notification(&access)? {
                continue;
            }
            return Ok(0);
        };
        let mut took = 0usize;
        for d in chain.writable() {
            let rest = bytes.get(took..).unwrap_or_default();
            let n = rest.len().min(d.len as usize);
            access.write(d.addr, rest.get(..n).unwrap_or_default())?;
            took = took.saturating_add(n);
            if took == bytes.len() {
                break;
            }
        }
        queue.add_used(&access, chain.head, u32::try_from(took).unwrap_or(u32::MAX))?;
        if queue.needs_interrupt(&access)? {
            interrupt.used_buffer();
        }
        if took > 0 {
            return Ok(took);
        }
    }
}

/// A buffer's bytes, read out of guest memory: none where it cannot be read or held.
fn read_chain(chain: &Chain, memory: &GuestMemory) -> Option<Vec<u8>> {
    let len = chain
        .readable()
        .map(|d| d.len as usize)
        .fold(0usize, usize::saturating_add);
    let mut bytes = Vec::new();
    if bytes.try_reserve(len).is_err() {
        debug!("virtio-console: no memory for a buffer of {len} bytes");
        return None;
    }
    let access = memory.access().ok()?;
    for d in chain.readable() {
        let at = bytes.len();
        bytes.resize(at.saturating_add(d.len as usize), 0);
        if let Err(e) = access.read(d.addr, bytes.get_mut(at..).unwrap_or_default()) {
            debug!("virtio-console: a buffer at {:#x}: {e}", d.addr);
            return None;
        }
    }
    Some(bytes)
}

/// The writer's one buffer.
#[derive(Default)]
struct Slot {
    /// Taken from the guest and not yet delivered.
    bytes: Option<Vec<u8>>,
    /// The writer is delivering what it took.
    writing: bool,
    /// The device is gone: the writer ends once it has delivered what it holds.
    ended: bool,
}

/// The transmit side, shared by the device's exits and its writer. Every console's output
/// takes one path: the notifying vCPU's exit only takes the guest's buffer into the slot,
/// uses it and returns; the writer, a thread of the device's own, delivers it to the output
/// and to what is kept to quote (D12), and then takes the guest's next. Linux's console
/// spins with interrupts off until the device has used each buffer (virtio_console.c
/// `__send_to_port`, l. 611-636): used in the exit, it runs on at once. The host holds at
/// most the one buffer being delivered; the guest's next waits in its ring of one
/// (`TRANSMIT_SIZE`), the guest spinning on it, so an output that blocks (a terminal paused,
/// a pipe nobody reads) holds the guest back as a UART did, and holds no lock its other
/// exits, or a pause or a snapshot of the device, need (D261).
struct Tx {
    port: Arc<Port>,
    /// The transmit queue while it is served, with what serving it needs: none before
    /// activation, while paused, after a reset, or once its ring was found malformed.
    queue: Mutex<Option<Served>>,
}

impl Tx {
    /// Takes the guest's next buffer into the slot, where it is free; the writer comes back
    /// for the guest's next as it is done. A malformed ring needs reset (virtio 1.3 §2.1.2).
    fn serve(&self) {
        let mut held = lock(&self.queue);
        let Some((queue, memory, interrupt)) = held.as_mut() else {
            return;
        };
        let busy = {
            let s = lock(&self.port.slot);
            s.bytes.is_some() || s.writing
        };
        if busy {
            return;
        }
        let never = AtomicBool::new(false);
        let mut taken = None;
        let mut answer = |chain: &Chain| {
            taken = read_chain(chain, memory);
            0
        };
        // A ring of one: a round takes at most the one buffer the slot holds.
        let round = serve_round(queue, memory, interrupt, &never, &mut answer);
        if let Some(bytes) = taken {
            lock(&self.port.slot).bytes = Some(bytes);
            self.port.filled.notify_all();
        }
        if let Err(e) = round {
            if interrupt.fail() {
                warn!("virtio-console: {e}; device needs reset");
            } else {
                debug!("virtio-console: {e}; device needs reset");
            }
            *held = None;
        }
    }

    /// The writer: each buffer taken delivered, with no lock held but the slot's and what is
    /// kept's, then the guest's next taken.
    fn write_out(&self, mut to: Box<dyn Write + Send>) {
        let port = &self.port;
        loop {
            let bytes = {
                let mut s = lock(&port.slot);
                loop {
                    if let Some(bytes) = s.bytes.take() {
                        s.writing = true;
                        break bytes;
                    }
                    if s.ended {
                        return;
                    }
                    s = wait(&port.filled, s);
                }
            };
            // Console output is best effort: a closed stdout must not stop the guest.
            let _ = to.write_all(&bytes).and_then(|()| to.flush());
            lock(&port.kept).push(&bytes);
            lock(&port.slot).writing = false;
            port.drained.notify_all();
            self.serve();
        }
    }
}

pub struct Console {
    port: Arc<Port>,
    tx: Arc<Tx>,
    /// What serving its queues needs besides them, from activation until reset.
    context: Option<(Arc<GuestMemory>, Arc<DeviceInterrupt>)>,
    /// The transmit queue while paused.
    transmit_paused: Option<Queue>,
    /// The receive queue while paused.
    receive: Option<Queue>,
}

impl std::fmt::Debug for Console {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Console").finish_non_exhaustive()
    }
}

impl Console {
    /// A console writing to `out`, that keeps its last `keep` bytes ([`Port::said`]); its
    /// port, for its VM's handle. A writer thread the system refuses is an error.
    pub fn new(out: Box<dyn Write + Send>, keep: usize) -> Result<(Console, Arc<Port>), String> {
        let port = Arc::new(Port {
            kept: Mutex::new(Kept::new(keep)),
            slot: Mutex::new(Slot::default()),
            filled: Condvar::new(),
            drained: Condvar::new(),
            receive: Mutex::new(Receive::default()),
            posted: Condvar::new(),
            panicked: AtomicBool::new(false),
        });
        let tx = Arc::new(Tx {
            port: port.clone(),
            queue: Mutex::new(None),
        });
        let writing = tx.clone();
        // Not joined as the device drops: a write blocked on its output would hold the
        // machine's teardown. It ends once what it holds is delivered.
        std::thread::Builder::new()
            .name("console-out".into())
            .spawn(move || writing.write_out(out))
            .map_err(|e| format!("the console's writer: {e}"))?;
        let console = Console {
            port: port.clone(),
            tx,
            context: None,
            transmit_paused: None,
            receive: None,
        };
        Ok((console, port))
    }

    /// Hands the receive queue to host input, and wakes input waiting for it.
    fn serve_input(&mut self, queue: Queue) {
        let mut r = lock(&self.port.receive);
        if let Some((memory, interrupt)) = self.context.clone() {
            r.queue = Some((queue, memory, interrupt));
        }
        self.port.posted.notify_all();
    }

    /// Takes the receive queue back from host input, after the buffer it is filling.
    fn take_input(&mut self) -> Option<Queue> {
        let mut r = lock(&self.port.receive);
        r.queue.take().map(|(queue, _, _)| queue)
    }

    /// Hands the transmit queue to the device's exits and its writer, and takes what the
    /// guest sent before.
    fn serve_output(&mut self, queue: Queue) {
        if let Some((memory, interrupt)) = self.context.clone() {
            *lock(&self.tx.queue) = Some((queue, memory, interrupt));
            self.tx.serve();
        }
    }
}

impl VirtioDevice for Console {
    fn device_id(&self) -> u32 {
        DEVICE_ID
    }

    fn features(&self) -> u64 {
        feature::VERSION_1
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUES
    }

    /// `struct virtio_console_config` (§5.3.4): cols, rows, max_nr_ports and emerg_wr,
    /// each valid only with a feature this device does not offer: zeros.
    fn read_config(&self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }

    /// emerg_wr is the one field a driver writes, with VIRTIO_CONSOLE_F_EMERG_WRITE, which
    /// this device does not offer.
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn activate(&mut self, activation: Activation) -> Result<(), String> {
        let Activation {
            memory,
            mut queues,
            interrupt,
            ..
        } = activation;
        if queues.len() != QUEUES.len() {
            return Err(format!(
                "virtio-console has {} queues, not {}",
                QUEUES.len(),
                queues.len()
            ));
        }
        let transmit = queues.remove(TRANSMITQ);
        let receive = queues.remove(RECEIVEQ);
        self.context = Some((memory, interrupt));
        self.serve_input(receive);
        self.serve_output(transmit);
        Ok(())
    }

    /// Host input waits for receive buffers; output is taken here, in the notifying vCPU's
    /// exit, as the guest spins until it is (`Tx::serve`).
    fn notify(&self, queue: u16) {
        match usize::from(queue) {
            RECEIVEQ => {
                let _held = lock(&self.port.receive);
                self.port.posted.notify_all();
            }
            TRANSMITQ => self.tx.serve(),
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.take_input();
        *lock(&self.tx.queue) = None;
        self.transmit_paused = None;
        self.receive = None;
        self.context = None;
    }

    /// Output stops after the buffer it is writing out, input after the one it is filling;
    /// each queue's progress, receive then transmit.
    fn pause(&mut self) -> Vec<QueueState> {
        if let Some((queue, _, _)) = lock(&self.tx.queue).take() {
            self.transmit_paused = Some(queue);
        }
        if let Some(queue) = self.take_input() {
            self.receive = Some(queue);
        }
        match (&self.receive, &self.transmit_paused) {
            (Some(r), Some(t)) => vec![r.state(), t.state()],
            _ => Vec::new(),
        }
    }

    fn resume(&mut self) -> Result<(), String> {
        if let Some(queue) = self.receive.take() {
            self.serve_input(queue);
        }
        if let Some(queue) = self.transmit_paused.take() {
            self.serve_output(queue);
        }
        Ok(())
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.take_input();
        *lock(&self.tx.queue) = None;
        lock(&self.port.slot).ended = true;
        self.port.filled.notify_all();
        self.port.drained.notify_all();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::super::queue::QueueConfig;
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    const BASE: u64 = 0x10_0000;
    const SIZE: u16 = 1;

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

    /// An output that keeps what it is given, and which threads gave it.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>, Arc<Mutex<Vec<std::thread::ThreadId>>>);
    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            self.1.lock().unwrap().push(std::thread::current().id());
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Queue `q`'s descriptor 0, one buffer of `len` bytes holding `bytes`, published.
    fn publish(mem: &GuestMemory, q: u64, bytes: &[u8], len: u32, writable: bool) {
        let a = mem.access().unwrap();
        a.write(buffer(q, 0), bytes).unwrap();
        let mut d = [0u8; 16];
        d[..8].copy_from_slice(&buffer(q, 0).to_le_bytes());
        d[8..12].copy_from_slice(&len.to_le_bytes());
        d[12..14].copy_from_slice(&(if writable { 2u16 } else { 0 }).to_le_bytes());
        a.write(desc(q), &d).unwrap();
        let idx = a.load_u16(avail(q) + 2, Ordering::Acquire).unwrap();
        a.write_obj(avail(q) + 4, 0u16).unwrap();
        a.store_u16(avail(q) + 2, idx.wrapping_add(1), Ordering::Release)
            .unwrap();
    }

    /// The used ring's index of queue `q`, and the length of its one entry (a ring of one).
    fn used_of(mem: &GuestMemory, q: u64) -> (u16, u32) {
        let a = mem.access().unwrap();
        let idx = a.load_u16(used(q) + 2, Ordering::Acquire).unwrap();
        (idx, a.read_obj::<u32>(used(q) + 8).unwrap())
    }

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let began = Instant::now();
        while !done() {
            assert!(began.elapsed() < Duration::from_secs(10), "{what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn active(keep: usize) -> (Console, Arc<Port>, Arc<GuestMemory>, Sink) {
        let sink = Sink::default();
        let (console, port, mem) = active_to(Box::new(sink.clone()), keep);
        (console, port, mem, sink)
    }

    fn active_to(out: Box<dyn Write + Send>, keep: usize) -> (Console, Arc<Port>, Arc<GuestMemory>) {
        let mem = Arc::new(GuestMemory::anonymous(&[(BASE, 1 << 20)]).unwrap());
        let (mut console, port) = Console::new(out, keep).unwrap();
        let queues = (0..2)
            .map(|q| {
                let cfg = QueueConfig {
                    size: SIZE,
                    desc: desc(q),
                    avail: avail(q),
                    used: used(q),
                    ready: true,
                };
                Queue::new(cfg, QUEUES[q as usize], &mem, feature::VERSION_1).unwrap()
            })
            .collect();
        console
            .activate(Activation {
                memory: mem.clone(),
                queues,
                interrupt: Arc::new(DeviceInterrupt::new(Arc::new(Line))),
                features: feature::VERSION_1,
                restored: false,
            })
            .unwrap();
        (console, port, mem)
    }

    /// A buffer the guest sends is used before its notification's exit returns, so the
    /// guest's spin ends at once, with nothing written in the exit: the writer delivers it
    /// whole, to the output and to what is kept to quote in whole lines (D12), which waits
    /// for it.
    #[test]
    fn a_buffer_sent_is_used_in_its_exit_and_delivered_by_the_writer() {
        let (console, port, mem, sink) = active(1000);
        let text = b"[    0.000000] Linux version 6.18.48\nKernel panic - not syncing\n";
        publish(&mem, 1, text, text.len() as u32, false);
        console.notify(1);
        assert_eq!(used_of(&mem, 1).0, 1, "used within the notify");
        assert_eq!(port.said().lines, text);
        assert_eq!(*sink.0.lock().unwrap(), text);
        let me = std::thread::current().id();
        assert!(
            sink.1.lock().unwrap().iter().all(|t| *t != me),
            "written in the exit"
        );
    }

    /// Host input waits for a buffer the guest posted, losing nothing: more than a buffer
    /// holds goes in buffer after buffer, as the guest posts them.
    #[test]
    fn host_input_waits_for_posted_buffers_and_loses_nothing() {
        let (console, port, mem, _) = active(0);
        let console = Mutex::new(console);
        std::thread::scope(|s| {
            let typing = s.spawn(|| port.input(b"hello, guest"));
            std::thread::sleep(Duration::from_millis(50));
            assert!(!typing.is_finished(), "input waits while no buffer is posted");
            let mut got = Vec::new();
            for n in 1..=3u16 {
                publish(&mem, 0, &[0; 5], 5, true);
                lock(&console).notify(0);
                until("the buffer filled", || used_of(&mem, 0).0 == n);
                let len = used_of(&mem, 0).1 as usize;
                let mut b = vec![0u8; len];
                mem.access().unwrap().read(buffer(0, 0), &mut b).unwrap();
                got.extend_from_slice(&b);
            }
            typing.join().unwrap();
            assert_eq!(got, b"hello, guest");
        });
    }

    /// A paused console takes no input: its receive queue is the snapshot's, untouched,
    /// until it resumes; each queue's progress is given back, receive then transmit.
    #[test]
    fn a_paused_console_holds_input_until_it_resumes() {
        let (mut console, port, mem, _) = active(0);
        let states = console.pause();
        assert_eq!(states.len(), 2);
        publish(&mem, 0, &[0; 8], 8, true);
        std::thread::scope(|s| {
            let typing = s.spawn(|| port.input(b"x"));
            std::thread::sleep(Duration::from_millis(50));
            assert_eq!(used_of(&mem, 0).0, 0, "nothing delivered while paused");
            console.resume().unwrap();
            typing.join().unwrap();
        });
        assert_eq!(used_of(&mem, 0), (1, 1));
    }

    /// A stdout that holds each write until it is let go: a terminal paused, a pipe nobody
    /// reads.
    struct Held {
        open: Arc<(Mutex<bool>, std::sync::Condvar)>,
        got: Arc<Mutex<Vec<u8>>>,
    }
    impl Write for Held {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            let (open, opened) = &*self.open;
            let mut o = open.lock().unwrap();
            while !*o {
                o = opened.wait(o).unwrap();
            }
            self.got.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A stdout that blocks holds no exit and no pause: the guest's buffer is used as its
    /// exit takes it, the next waits in its ring while the first is written, a pause lands
    /// while the write is blocked, and once the stdout drains both are written in order.
    #[test]
    fn a_blocked_stdout_holds_no_exit_and_no_pause() {
        let open = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let got = Arc::new(Mutex::new(Vec::new()));
        let held = Held {
            open: open.clone(),
            got: got.clone(),
        };
        let (mut console, _port, mem) = active_to(Box::new(held), 0);
        let within = |what: &str, f: &mut (dyn FnMut() + Send)| {
            std::thread::scope(|s| {
                let (done, finished) = std::sync::mpsc::channel();
                s.spawn(move || {
                    f();
                    let _ = done.send(());
                });
                assert!(
                    finished.recv_timeout(Duration::from_secs(5)).is_ok(),
                    "{what} blocked"
                );
            });
        };
        publish(&mem, 1, b"first\n", 6, false);
        within("the first kick", &mut || console.notify(1));
        assert_eq!(used_of(&mem, 1).0, 1, "taken within its exit");
        until("the writer blocked", || {
            got.lock().unwrap().is_empty() && used_of(&mem, 1).0 == 1
        });
        publish(&mem, 1, b"second\n", 7, false);
        within("the second kick", &mut || console.notify(1));
        assert_eq!(used_of(&mem, 1).0, 1, "the second waits in its ring");
        let mut states = Vec::new();
        within("a pause", &mut || states = console.pause());
        assert_eq!(states.len(), 2);
        console.resume().unwrap();
        *open.0.lock().unwrap() = true;
        open.1.notify_all();
        until("both written", || {
            got.lock().unwrap().as_slice() == b"first\nsecond\n"
        });
        assert_eq!(used_of(&mem, 1).0, 2);
    }

    /// No feature past VERSION_1, its two queues of one buffer each, and a configuration of
    /// zeros that ignores writes.
    #[test]
    fn the_device_is_one_port_of_two_queues() {
        let (mut c, _) = Console::new(Box::new(std::io::sink()), 0).unwrap();
        assert_eq!((c.device_id(), c.features()), (3, feature::VERSION_1));
        assert_eq!(c.queue_max_sizes(), [1, 1]);
        c.write_config(0, &[9; 12]);
        let mut config = [0xffu8; 12];
        c.read_config(0, &mut config);
        assert_eq!(config, [0; 12]);
    }
}
