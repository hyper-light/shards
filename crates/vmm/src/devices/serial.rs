//! NS16550A UART: the boot and debug console.
//!
//! Every register access is a VM exit (~0.8 µs on HVF), so this is for diagnostics, not
//! throughput; production VMs boot without it (docs/research/boot-latency.md).
//! Register semantics follow the NS16550A datasheet, including loopback, which Linux's
//! 8250 driver exercises while probing.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Condvar, Mutex};

use super::{Interrupt, MmioDevice};
use crate::snapshot::codec::{self, Reader, Writer};
use crate::sync::{lock, wait};

const RBR_THR_DLL: u64 = 0;
const IER_DLM: u64 = 1;
const IIR_FCR: u64 = 2;
const LCR: u64 = 3;
const MCR: u64 = 4;
const LSR: u64 = 5;
const MSR: u64 = 6;
const SCR: u64 = 7;

const IER_RDI: u8 = 1 << 0;
const IER_THRI: u8 = 1 << 1;
const IER_RLSI: u8 = 1 << 2;
const IER_MSI: u8 = 1 << 3;

const IIR_NONE: u8 = 0x01;
const IIR_THRI: u8 = 0x02;
const IIR_RDI: u8 = 0x04;
const IIR_RLSI: u8 = 0x06;
const IIR_FIFO_ENABLED: u8 = 0xc0;

const LCR_DLAB: u8 = 1 << 7;
const MCR_DTR: u8 = 1 << 0;
const MCR_RTS: u8 = 1 << 1;
const MCR_OUT1: u8 = 1 << 2;
const MCR_OUT2: u8 = 1 << 3;
const MCR_LOOP: u8 = 1 << 4;
const LSR_DR: u8 = 1 << 0;
const LSR_OE: u8 = 1 << 1;
const LSR_THRE: u8 = 1 << 5;
const LSR_TEMT: u8 = 1 << 6;
const MSR_CTS: u8 = 1 << 4;
const MSR_DSR: u8 = 1 << 5;
const MSR_RI: u8 = 1 << 6;
const MSR_DCD: u8 = 1 << 7;

/// The receive FIFO's depth in FIFO mode (FCR bit 0 set): the NS16550A's 16 bytes
/// (PC16550D datasheet, "In the FIFO mode, transmitter and receiver are each buffered with
/// 16 byte FIFOs"; Linux 8250_port.c `uart_config[PORT_16550A].fifo_size`, which the
/// guest's driver sizes its reads by). In 16450 mode, the receiver buffer register's one
/// byte ([`rx_capacity`]). Host input waits for room rather than overrun it
/// ([`Serial::input`]), as Firecracker reads stdin only as far as its FIFO has room
/// (src/vmm/src/devices/legacy/serial.rs `recv_bytes`), so what the guest has not drained
/// stays in the host's own buffers, the input's pipe or terminal.
const RX_FIFO: usize = 16;
/// What is kept of the console's output, its last bytes ([`Serial::said`]): a page. A
/// kernel's panic and the errors before it wrote 1,129 bytes to a console told to be
/// `quiet`, 858 of them from its panic's line to its reset (6.18 on arm64, a root it could
/// not mount).
const SAID: usize = 4096;

struct State {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    fifo_enabled: bool,
    /// THR-empty interrupt is pending until IIR reports it or THR is written.
    thri_pending: bool,
    overrun: bool,
    /// Received bytes the guest has not read, at most [`rx_capacity`] but for a snapshot's
    /// of an older build, which the guest drains first.
    rx: VecDeque<u8>,
    out: Box<dyn Write + Send>,
    /// The console's last bytes, at most [`SAID`].
    said: VecDeque<u8>,
    level: bool,
}

pub struct Serial {
    state: Mutex<State>,
    /// Signalled as the receive buffer gains room, for host input waiting on it.
    room: Condvar,
    irq: Arc<dyn Interrupt>,
}

/// How many bytes the receiver holds: its FIFO, or in 16450 mode its one register.
fn rx_capacity(s: &State) -> usize {
    if s.fifo_enabled { RX_FIFO } else { 1 }
}

/// The most host input the receiver takes at once, its FIFO ([`Serial::input`]): what a
/// reader of the host's input need read at a time, the rest waiting in the host's pipe or
/// terminal rather than in the reader.
pub const INPUT_AT_ONCE: usize = RX_FIFO;

impl std::fmt::Debug for Serial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Serial").finish_non_exhaustive()
    }
}

impl Serial {
    pub fn new(out: Box<dyn Write + Send>, irq: Arc<dyn Interrupt>) -> Serial {
        Serial {
            state: Mutex::new(State {
                ier: 0,
                lcr: 0x03, // 8N1
                mcr: MCR_OUT2,
                scr: 0,
                dll: 12, // 9600 baud at 1.8432 MHz
                dlm: 0,
                fifo_enabled: false,
                thri_pending: false,
                overrun: false,
                rx: VecDeque::new(),
                out,
                said: VecDeque::new(),
                level: false,
            }),
            room: Condvar::new(),
            irq,
        }
    }

    /// The whole lines of the last [`SAID`] bytes its guest wrote to the console: what it
    /// said last, as a kernel writes why it panics there.
    pub fn said(&self) -> Vec<u8> {
        let s = lock(&self.state);
        let mut said: Vec<u8> = s.said.iter().copied().collect();
        if said.len() == SAID {
            let cut = said
                .iter()
                .position(|&b| b == b'\n')
                .map_or(said.len(), |at| at + 1);
            said.drain(..cut);
        }
        said
    }

    /// Gives host input to the guest, waiting while its receiver has no room: as a line
    /// with flow control, nothing is lost, and what the guest has not taken waits in the
    /// caller.
    pub fn input(&self, mut bytes: &[u8]) {
        let mut s = lock(&self.state);
        loop {
            let took = Self::enqueue(&mut s, bytes);
            bytes = bytes.get(took..).unwrap_or_default();
            if took > 0 {
                self.update_irq(&mut s);
            }
            if bytes.is_empty() {
                return;
            }
            s = wait(&self.room, s);
        }
    }

    /// Queues as much of `bytes` as the receiver has room for; returns how many it took.
    fn enqueue(s: &mut State, bytes: &[u8]) -> usize {
        let room = rx_capacity(s).saturating_sub(s.rx.len());
        let took = bytes.len().min(room);
        s.rx.extend(bytes.iter().take(took));
        took
    }

    fn pending(s: &State) -> u8 {
        if s.ier & IER_RLSI != 0 && s.overrun {
            IIR_RLSI
        } else if s.ier & IER_RDI != 0 && !s.rx.is_empty() {
            IIR_RDI
        } else if s.ier & IER_THRI != 0 && s.thri_pending {
            IIR_THRI
        } else {
            IIR_NONE
        }
    }

    fn update_irq(&self, s: &mut State) {
        let level = Self::pending(s) != IIR_NONE;
        if level != s.level {
            s.level = level;
            self.irq.set_level(level);
        }
    }

    fn msr(s: &State) -> u8 {
        if s.mcr & MCR_LOOP != 0 {
            // Loopback: modem inputs mirror the modem outputs.
            let m = s.mcr;
            (if m & MCR_RTS != 0 { MSR_CTS } else { 0 })
                | (if m & MCR_DTR != 0 { MSR_DSR } else { 0 })
                | (if m & MCR_OUT1 != 0 { MSR_RI } else { 0 })
                | (if m & MCR_OUT2 != 0 { MSR_DCD } else { 0 })
        } else {
            MSR_DCD | MSR_DSR | MSR_CTS
        }
    }
}

impl MmioDevice for Serial {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let mut s = lock(&self.state);
        let dlab = s.lcr & LCR_DLAB != 0;
        let v = match offset {
            RBR_THR_DLL if dlab => s.dll,
            RBR_THR_DLL => match s.rx.pop_front() {
                Some(b) => {
                    self.room.notify_all();
                    b
                }
                None => 0,
            },
            IER_DLM if dlab => s.dlm,
            IER_DLM => s.ier,
            IIR_FCR => {
                let id = Self::pending(&s);
                if id == IIR_THRI {
                    s.thri_pending = false; // reading IIR clears a THRE interrupt
                }
                id | if s.fifo_enabled { IIR_FIFO_ENABLED } else { 0 }
            }
            LCR => s.lcr,
            MCR => s.mcr,
            LSR => {
                let v = LSR_THRE
                    | LSR_TEMT
                    | if s.rx.is_empty() { 0 } else { LSR_DR }
                    | if s.overrun { LSR_OE } else { 0 };
                s.overrun = false; // error bits clear on read
                v
            }
            MSR => Self::msr(&s),
            SCR => s.scr,
            _ => 0,
        };
        data.fill(0);
        if let Some(first) = data.first_mut() {
            *first = v;
        }
        self.update_irq(&mut s);
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let Some(&v) = data.first() else {
            return;
        };
        let mut s = lock(&self.state);
        let dlab = s.lcr & LCR_DLAB != 0;
        match offset {
            RBR_THR_DLL if dlab => s.dll = v,
            RBR_THR_DLL => {
                if s.mcr & MCR_LOOP != 0 {
                    if s.rx.len() < rx_capacity(&s) {
                        s.rx.push_back(v);
                    } else {
                        s.overrun = true;
                    }
                } else {
                    // Console output is best effort: a closed stdout must not stop the guest.
                    let _ = s.out.write_all(&[v]).and_then(|()| s.out.flush());
                    if s.said.len() == SAID {
                        s.said.pop_front();
                    }
                    s.said.push_back(v);
                }
                // Transmission is instantaneous, so THR is empty again at once.
                s.thri_pending = true;
            }
            IER_DLM if dlab => s.dlm = v,
            IER_DLM => {
                let was = s.ier;
                s.ier = v & (IER_RDI | IER_THRI | IER_RLSI | IER_MSI);
                // Enabling THRI while THR is empty raises the interrupt immediately.
                if s.ier & IER_THRI != 0 && was & IER_THRI == 0 {
                    s.thri_pending = true;
                }
            }
            IIR_FCR => {
                s.fifo_enabled = v & 1 != 0;
                if v & 0x02 != 0 {
                    s.rx.clear();
                }
                self.room.notify_all();
            }
            LCR => s.lcr = v,
            MCR => s.mcr = v & 0x1f,
            SCR => s.scr = v,
            _ => {}
        }
        self.update_irq(&mut s);
    }

    fn pause(&self) {}

    fn resume(&self) -> Result<(), String> {
        Ok(())
    }

    fn save(&self, w: &mut Writer) {
        let s = lock(&self.state);
        for v in [s.ier, s.lcr, s.mcr, s.scr, s.dll, s.dlm] {
            w.u8(v);
        }
        w.bool(s.fifo_enabled);
        w.bool(s.thri_pending);
        w.bool(s.overrun);
        let (a, b) = s.rx.as_slices();
        w.u32((a.len() + b.len()) as u32);
        a.iter().chain(b).for_each(|&byte| w.u8(byte));
    }

    /// The restored GIC starts with every line low, so a pending interrupt is raised
    /// again here.
    fn restore(&self, r: &mut Reader<'_>) -> codec::Result<()> {
        let mut s = lock(&self.state);
        s.ier = r.u8()? & (IER_RDI | IER_THRI | IER_RLSI | IER_MSI);
        s.lcr = r.u8()?;
        s.mcr = r.u8()? & 0x1f;
        s.scr = r.u8()?;
        s.dll = r.u8()?;
        s.dlm = r.u8()?;
        s.fifo_enabled = r.bool()?;
        s.thri_pending = r.bool()?;
        s.overrun = r.bool()?;
        // As many bytes as the snapshot holds: an older build's held up to 4096, which the
        // guest drains before host input finds room again.
        let rx = r.bytes(u32::MAX as usize)?;
        s.rx.clear();
        s.rx.extend(rx);
        self.room.notify_all();
        s.level = false;
        self.update_irq(&mut s);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct Line(AtomicBool);
    impl Interrupt for Line {
        fn set_level(&self, level: bool) {
            self.0.store(level, Ordering::SeqCst);
        }
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

    fn rd(s: &Serial, off: u64) -> u8 {
        let mut b = [0];
        s.read(off, &mut b);
        b[0]
    }

    /// What the console said last is kept, its last page in whole lines; what loops back
    /// to the guest is not said.
    #[test]
    fn the_last_page_said_is_kept_in_whole_lines() {
        let s = Serial::new(Box::new(std::io::sink()), Arc::new(Line::default()));
        let say = |bytes: &[u8]| bytes.iter().for_each(|b| s.write(RBR_THR_DLL, &[*b]));
        assert!(s.said().is_empty());
        say(b"first\nsecond\n");
        s.write(MCR, &[MCR_LOOP]);
        say(b"looped");
        s.write(MCR, &[MCR_OUT2]);
        assert_eq!(s.said(), b"first\nsecond\n");
        let line: Vec<u8> = [b'x'; 99].into_iter().chain(*b"\n").collect();
        for _ in 0..SAID / line.len() + 2 {
            say(&line);
        }
        say(b"panic");
        let said = s.said();
        assert!(
            said.len() > SAID - line.len() && said.len() <= SAID,
            "{}",
            said.len()
        );
        assert!(said.starts_with(&line), "{:?}", &said[..8]);
        assert!(said.ends_with(b"\npanic"));
    }

    #[test]
    fn transmit_receive_and_interrupts() {
        let sink = Sink::default();
        let line = Arc::new(Line::default());
        let s = Serial::new(Box::new(sink.clone()), line.clone());
        for b in b"ok\n" {
            s.write(RBR_THR_DLL, &[*b]);
        }
        assert_eq!(&*sink.0.lock().unwrap(), b"ok\n");
        assert_eq!(rd(&s, LSR) & (LSR_THRE | LSR_TEMT), LSR_THRE | LSR_TEMT);

        // RX interrupt only once enabled; clears when the FIFO drains.
        s.write(IIR_FCR, &[1]);
        s.input(b"ab");
        assert!(!line.0.load(Ordering::SeqCst));
        s.write(IER_DLM, &[IER_RDI]);
        assert!(line.0.load(Ordering::SeqCst));
        assert_eq!(rd(&s, IIR_FCR) & 0x0f, IIR_RDI);
        assert_eq!(rd(&s, LSR) & LSR_DR, LSR_DR);
        assert_eq!((rd(&s, RBR_THR_DLL), rd(&s, RBR_THR_DLL)), (b'a', b'b'));
        assert!(!line.0.load(Ordering::SeqCst));

        // THRE interrupt: raised on enable, cleared by reading IIR, re-raised by a write.
        s.write(IER_DLM, &[IER_THRI]);
        assert!(line.0.load(Ordering::SeqCst));
        assert_eq!(rd(&s, IIR_FCR) & 0x0f, IIR_THRI);
        assert!(!line.0.load(Ordering::SeqCst));
        s.write(RBR_THR_DLL, b"x");
        assert!(line.0.load(Ordering::SeqCst));
    }

    #[test]
    fn divisor_latch_and_loopback() {
        let sink = Sink::default();
        let s = Serial::new(Box::new(sink.clone()), Arc::new(Line::default()));
        s.write(LCR, &[LCR_DLAB | 3]);
        s.write(RBR_THR_DLL, &[0x01]);
        s.write(IER_DLM, &[0x02]);
        assert_eq!((rd(&s, RBR_THR_DLL), rd(&s, IER_DLM)), (0x01, 0x02));
        s.write(LCR, &[3]);
        assert_eq!(rd(&s, IER_DLM), 0); // IER, not DLM

        s.write(MCR, &[MCR_LOOP | MCR_RTS | MCR_OUT2]);
        assert_eq!(rd(&s, MSR), MSR_CTS | MSR_DCD);
        s.write(RBR_THR_DLL, &[0x5a]);
        assert!(sink.0.lock().unwrap().is_empty()); // looped back, not transmitted
        assert_eq!(rd(&s, RBR_THR_DLL), 0x5a);
    }

    /// A loopback write past the receiver's room overruns it, as the datasheet's does.
    #[test]
    fn overrun_is_reported_and_cleared() {
        let s = Serial::new(Box::new(Sink::default()), Arc::new(Line::default()));
        s.write(IIR_FCR, &[1]);
        s.write(MCR, &[MCR_LOOP]);
        for _ in 0..=RX_FIFO {
            s.write(RBR_THR_DLL, &[0x5a]);
        }
        assert_eq!(rd(&s, LSR) & LSR_OE, LSR_OE);
        assert_eq!(rd(&s, LSR) & LSR_OE, 0);
    }

    /// A snapshot of an older build, whose receiver held more than the FIFO's depth,
    /// restores whole, and the guest reads it all.
    #[test]
    fn an_older_snapshots_longer_receive_buffer_restores() {
        let mut w = Writer::default();
        for v in [0, 3, MCR_OUT2, 0, 12, 0] {
            w.u8(v);
        }
        w.bool(true);
        w.bool(false);
        w.bool(false);
        let held: Vec<u8> = (0..=u8::MAX).cycle().take(4096).collect();
        w.u32(held.len() as u32);
        held.iter().for_each(|&b| w.u8(b));
        let bytes = w.into_bytes();
        let s = Serial::new(Box::new(Sink::default()), Arc::new(Line::default()));
        let mut r = Reader::new(&bytes);
        s.restore(&mut r).unwrap();
        r.finish().unwrap();
        let read: Vec<u8> = held.iter().map(|_| rd(&s, RBR_THR_DLL)).collect();
        assert_eq!(read, held);
        assert_eq!(rd(&s, LSR) & LSR_DR, 0);
    }

    /// Host input fills the receiver to its depth, the FIFO's or in 16450 mode its one
    /// byte, and waits there; each byte the guest reads makes room for the next, and none
    /// is lost or overruns.
    #[test]
    fn host_input_waits_for_room_and_loses_nothing() {
        let s = Serial::new(Box::new(Sink::default()), Arc::new(Line::default()));
        let typed: Vec<u8> = (0..=u8::MAX).cycle().take(5 * RX_FIFO + 3).collect();
        let mut read = Vec::new();
        std::thread::scope(|scope| {
            scope.spawn(|| s.input(&typed));
            // In 16450 mode the receiver holds one byte; then the FIFO, once enabled.
            while rd(&s, LSR) & LSR_DR == 0 {
                std::thread::yield_now();
            }
            assert_eq!(lock(&s.state).rx.len(), 1);
            read.push(rd(&s, RBR_THR_DLL));
            s.write(IIR_FCR, &[1]);
            while read.len() < typed.len() {
                let held = lock(&s.state).rx.len();
                assert!(held <= RX_FIFO, "{held}");
                if rd(&s, LSR) & LSR_DR != 0 {
                    read.push(rd(&s, RBR_THR_DLL));
                } else {
                    std::thread::yield_now();
                }
            }
        });
        assert_eq!(read, typed);
        assert_eq!(rd(&s, LSR) & LSR_OE, 0, "no overrun");
    }
}
