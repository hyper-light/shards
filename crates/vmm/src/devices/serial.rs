//! NS16550A UART: the boot and debug console.
//!
//! Every register access is a VM exit (~0.8 µs on HVF), so this is for diagnostics, not
//! throughput; production VMs boot without it (docs/research/boot-latency.md).
//! Register semantics follow the NS16550A datasheet, including loopback, which Linux's
//! 8250 driver exercises while probing.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{Interrupt, MmioDevice};
use crate::snapshot::codec::{self, Reader, Writer};
use crate::sync::lock;

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

/// Receive buffer: generous so pasted input isn't dropped while the guest drains it.
const RX_CAPACITY: usize = 4096;

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
    rx: VecDeque<u8>,
    out: Box<dyn Write + Send>,
    level: bool,
}

pub struct Serial {
    state: Mutex<State>,
    irq: Arc<dyn Interrupt>,
}

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
                level: false,
            }),
            irq,
        }
    }

    /// Queues host input for the guest. Bytes beyond the buffer set the overrun flag.
    pub fn enqueue_input(&self, bytes: &[u8]) {
        let mut s = lock(&self.state);
        for &b in bytes {
            if s.rx.len() < RX_CAPACITY {
                s.rx.push_back(b);
            } else {
                s.overrun = true;
            }
        }
        self.update_irq(&mut s);
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
            RBR_THR_DLL => s.rx.pop_front().unwrap_or(0),
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
                    if s.rx.len() < RX_CAPACITY {
                        s.rx.push_back(v);
                    } else {
                        s.overrun = true;
                    }
                } else {
                    // Console output is best effort: a closed stdout must not stop the guest.
                    let _ = s.out.write_all(&[v]).and_then(|()| s.out.flush());
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
        let n = r.u32()? as usize;
        if n > RX_CAPACITY {
            return Err(codec::DecodeError(format!("UART receive buffer of {n}")));
        }
        s.rx.clear();
        for _ in 0..n {
            s.rx.push_back(r.u8()?);
        }
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
        s.enqueue_input(b"ab");
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

    #[test]
    fn overrun_is_reported_and_cleared() {
        let s = Serial::new(Box::new(Sink::default()), Arc::new(Line::default()));
        s.enqueue_input(&vec![0u8; RX_CAPACITY + 1]);
        assert_eq!(rd(&s, LSR) & LSR_OE, LSR_OE);
        assert_eq!(rd(&s, LSR) & LSR_OE, 0);
    }
}
