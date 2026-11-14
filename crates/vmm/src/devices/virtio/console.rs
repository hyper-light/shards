//! virtio-console (VIRTIO 1.3 §5.3): the guest's console, `hvc0`, one port and no
//! multiport control queue. Its output comes in buffers, one notification each, where a
//! UART traps every byte; its input goes into the buffers the guest posted, so a guest that
//! has posted none holds the host's input back in the host (its pipe or terminal), and
//! nothing is lost or held by the device. A snapshot keeps its queues alone.

use std::io::Write;
use std::sync::{Arc, Condvar, Mutex};

use super::queue::{Chain, Queue, QueueError};
use super::worker::Worker;
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
/// receiveq 0 and transmitq 1, port 0's (§5.3.2): no MULTIPORT, so no control queues.
const QUEUES: [u16; 2] = [RECEIVE_SIZE, TRANSMIT_SIZE];
const RECEIVEQ: usize = 0;
const TRANSMITQ: usize = 1;

/// Where its output goes, and what it keeps of it to quote (D12).
struct Out {
    to: Box<dyn Write + Send>,
    kept: Kept,
    /// A buffer's bytes, read out of guest memory before they are written out: no access
    /// to guest memory is held while the host's output blocks.
    scratch: Vec<u8>,
}

/// The receive side, which host input drives.
#[derive(Default)]
struct Receive {
    /// The receive queue while the device runs, with what serving it needs; none before
    /// activation, while paused, after a reset, or once its ring was found malformed.
    queue: Option<(Queue, Arc<GuestMemory>, Arc<DeviceInterrupt>)>,
}

/// The console's host side, shared by the device and its VM's handle.
pub struct Port {
    out: Mutex<Out>,
    receive: Mutex<Receive>,
    /// Signalled as the guest posts receive buffers, or the receive queue comes or goes.
    posted: Condvar,
}

impl std::fmt::Debug for Port {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Port").finish_non_exhaustive()
    }
}

impl Port {
    /// The whole lines of the last bytes its guest wrote to the console, as many as it keeps.
    pub fn said(&self) -> Said {
        lock(&self.out).kept.said()
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

    fn transmit(&self, chain: &Chain, memory: &GuestMemory) {
        let mut out = lock(&self.out);
        let Out { to, kept, scratch } = &mut *out;
        for d in chain.readable() {
            let len = d.len as usize;
            scratch.clear();
            if scratch.try_reserve(len).is_err() {
                debug!("virtio-console: no memory for a buffer of {len} bytes");
                return;
            }
            scratch.resize(len, 0);
            let read = memory
                .access()
                .map_err(|e| e.to_string())
                .and_then(|a| a.read(d.addr, scratch).map_err(|e| e.to_string()));
            if let Err(e) = read {
                debug!("virtio-console: a buffer at {:#x}: {e}", d.addr);
                return;
            }
            // Console output is best effort: a closed stdout must not stop the guest.
            let _ = to.write_all(scratch).and_then(|()| to.flush());
            kept.push(scratch);
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

pub struct Console {
    port: Arc<Port>,
    /// What the transmit worker needs besides its queue, from activation until reset.
    context: Option<(Arc<GuestMemory>, Arc<DeviceInterrupt>)>,
    worker: Option<Worker>,
    /// The transmit queue while paused.
    transmit: Option<Queue>,
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
    /// port, for its VM's handle.
    pub fn new(out: Box<dyn Write + Send>, keep: usize) -> (Console, Arc<Port>) {
        let port = Arc::new(Port {
            out: Mutex::new(Out {
                to: out,
                kept: Kept::new(keep),
                scratch: Vec::new(),
            }),
            receive: Mutex::new(Receive::default()),
            posted: Condvar::new(),
        });
        let console = Console {
            port: port.clone(),
            context: None,
            worker: None,
            transmit: None,
            receive: None,
        };
        (console, port)
    }

    /// Starts the transmit worker on `queue`; if it cannot, the queue waits paused.
    fn start(&mut self, queue: Queue) -> Result<(), String> {
        let Some((memory, interrupt)) = self.context.clone() else {
            self.transmit = Some(queue);
            return Err("virtio-console started before activation".into());
        };
        let (port, mem) = (self.port.clone(), memory.clone());
        let answering = move |chain: &Chain| {
            port.transmit(chain, &mem);
            0
        };
        match Worker::start("virtio-console", queue, memory, interrupt, answering) {
            Ok(worker) => {
                self.worker = Some(worker);
                Ok(())
            }
            Err((e, queue)) => {
                self.transmit = Some(queue);
                Err(e)
            }
        }
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
        // An activation that fails leaves its queue: the driver sets the device up anew.
        self.start(transmit).inspect_err(|_| self.transmit = None)
    }

    /// Host input waits for receive buffers; output is written out on the device's worker,
    /// never on the notifying vCPU's thread.
    fn notify(&self, queue: u16) {
        match usize::from(queue) {
            RECEIVEQ => {
                let _held = lock(&self.port.receive);
                self.port.posted.notify_all();
            }
            TRANSMITQ => {
                if let Some(w) = &self.worker {
                    w.notify();
                }
            }
            _ => {}
        }
    }

    fn reset(&mut self) {
        if let Some(w) = self.worker.take() {
            w.stop();
        }
        self.take_input();
        self.transmit = None;
        self.receive = None;
        self.context = None;
    }

    /// Output stops after the buffer it is writing out, input after the one it is filling;
    /// each queue's progress, receive then transmit.
    fn pause(&mut self) -> Vec<QueueState> {
        if let Some(queue) = self.worker.take().and_then(Worker::stop) {
            self.transmit = Some(queue);
        }
        if let Some(queue) = self.take_input() {
            self.receive = Some(queue);
        }
        match (&self.receive, &self.transmit) {
            (Some(r), Some(t)) => vec![r.state(), t.state()],
            _ => Vec::new(),
        }
    }

    fn resume(&mut self) -> Result<(), String> {
        if let Some(queue) = self.receive.take() {
            self.serve_input(queue);
        }
        match self.transmit.take() {
            Some(queue) => self.start(queue),
            None => Ok(()),
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        if let Some(w) = self.worker.take() {
            w.stop();
        }
        self.take_input();
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

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
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
        let mem = Arc::new(GuestMemory::anonymous(&[(BASE, 1 << 20)]).unwrap());
        let sink = Sink::default();
        let (mut console, port) = Console::new(Box::new(sink.clone()), keep);
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
        (console, port, mem, sink)
    }

    /// A buffer the guest sends is written out whole, its notification its one exit, and
    /// kept to quote in whole lines (D12); the device used it, so the guest's spin ends.
    #[test]
    fn a_buffer_sent_is_written_out_and_kept() {
        let (console, port, mem, sink) = active(1000);
        let text = b"[    0.000000] Linux version 6.18.48\nKernel panic - not syncing\n";
        publish(&mem, 1, text, text.len() as u32, false);
        console.notify(1);
        until("the buffer used", || used_of(&mem, 1).0 == 1);
        assert_eq!(*sink.0.lock().unwrap(), text);
        assert_eq!(port.said().lines, text);
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

    /// No feature past VERSION_1, its two queues of one buffer each, and a configuration of
    /// zeros that ignores writes.
    #[test]
    fn the_device_is_one_port_of_two_queues() {
        let (mut c, _) = Console::new(Box::new(std::io::sink()), 0);
        assert_eq!((c.device_id(), c.features()), (3, feature::VERSION_1));
        assert_eq!(c.queue_max_sizes(), [1, 1]);
        c.write_config(0, &[9; 12]);
        let mut config = [0xffu8; 12];
        c.read_config(0, &mut config);
        assert_eq!(config, [0; 12]);
    }
}
