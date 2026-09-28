//! virtio-mmio transport, version 2 (virtio 1.3 §4.2).

use std::sync::{Arc, Mutex};

use super::queue::{Queue, QueueConfig, QueueState};
use super::{Activation, DeviceInterrupt, VirtioDevice, feature, status};
use crate::devices::{Interrupt, MmioDevice, get_le, put_le};
use crate::memory::GuestMemory;
use crate::snapshot::codec::{self, Reader, Writer};
use crate::sync::lock;
use crate::warn;

const MAGIC: u64 = 0x000;
const VERSION: u64 = 0x004;
const DEVICE_ID: u64 = 0x008;
const VENDOR_ID: u64 = 0x00c;
const DEVICE_FEATURES: u64 = 0x010;
const DEVICE_FEATURES_SEL: u64 = 0x014;
const DRIVER_FEATURES: u64 = 0x020;
const DRIVER_FEATURES_SEL: u64 = 0x024;
const QUEUE_SEL: u64 = 0x030;
const QUEUE_NUM_MAX: u64 = 0x034;
const QUEUE_NUM: u64 = 0x038;
const QUEUE_READY: u64 = 0x044;
const QUEUE_NOTIFY: u64 = 0x050;
const INTERRUPT_STATUS: u64 = 0x060;
const INTERRUPT_ACK: u64 = 0x064;
const STATUS: u64 = 0x070;
const QUEUE_DESC_LOW: u64 = 0x080;
const QUEUE_DESC_HIGH: u64 = 0x084;
const QUEUE_DRIVER_LOW: u64 = 0x090;
const QUEUE_DRIVER_HIGH: u64 = 0x094;
const QUEUE_DEVICE_LOW: u64 = 0x0a0;
const QUEUE_DEVICE_HIGH: u64 = 0x0a4;
const SHM_LEN_LOW: u64 = 0x0b0;
const SHM_LEN_HIGH: u64 = 0x0b4;
const CONFIG_GENERATION: u64 = 0x0fc;
const CONFIG: u64 = 0x100;

const MAGIC_VALUE: u32 = 0x7472_6976; // "virt"
/// "SHRD" read as a little-endian u32.
const VENDOR: u32 = 0x4452_4853;
/// Bytes of MMIO window each transport occupies.
pub const WINDOW: u64 = 0x200;

struct State {
    device: Box<dyn VirtioDevice>,
    status: u32,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    queues: Vec<QueueConfig>,
    config_generation: u32,
    /// Each queue's progress while paused, for a snapshot.
    paused: Vec<QueueState>,
}

/// One virtio device behind an MMIO register window.
pub struct MmioTransport {
    state: Mutex<State>,
    interrupt: Arc<DeviceInterrupt>,
    memory: Arc<GuestMemory>,
}

impl std::fmt::Debug for MmioTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MmioTransport").finish_non_exhaustive()
    }
}

fn low(v: u64) -> u32 {
    v as u32
}

fn high(v: u64) -> u32 {
    (v >> 32) as u32
}

fn set_low(v: &mut u64, x: u32) {
    *v = (*v & !0xffff_ffff) | u64::from(x);
}

fn set_high(v: &mut u64, x: u32) {
    *v = (*v & 0xffff_ffff) | (u64::from(x) << 32);
}

impl MmioTransport {
    pub fn new(
        device: Box<dyn VirtioDevice>,
        memory: Arc<GuestMemory>,
        line: Arc<dyn Interrupt>,
    ) -> MmioTransport {
        let queues = vec![QueueConfig::default(); device.queue_max_sizes().len()];
        MmioTransport {
            state: Mutex::new(State {
                device,
                status: 0,
                device_features_sel: 0,
                driver_features_sel: 0,
                driver_features: 0,
                queue_sel: 0,
                queues,
                config_generation: 0,
                paused: Vec::new(),
            }),
            interrupt: Arc::new(DeviceInterrupt::new(line)),
            memory,
        }
    }

    fn reset(&self, s: &mut State) {
        s.device.reset();
        s.status = 0;
        s.device_features_sel = 0;
        s.driver_features_sel = 0;
        s.driver_features = 0;
        s.queue_sel = 0;
        s.queues.iter_mut().for_each(|q| *q = QueueConfig::default());
        self.interrupt.clear();
    }

    fn set_status(&self, s: &mut State, new: u32) {
        if new == 0 {
            self.reset(s);
            return;
        }
        // Status bits are only ever added (virtio 1.3 §3.1.1); clearing requires reset.
        if new & s.status != s.status {
            warn!(
                "virtio: driver cleared status bits {:#x} -> {new:#x} without reset",
                s.status
            );
            return;
        }
        let added = new & !s.status;
        if added & status::FEATURES_OK != 0 {
            let offered = s.device.features();
            let accepted = s.driver_features;
            if accepted & !offered != 0 || accepted & feature::VERSION_1 == 0 {
                // Leave FEATURES_OK clear: the driver reads it back and gives up.
                s.status = new & !status::FEATURES_OK;
                return;
            }
        }
        s.status = new;
        if added & status::DRIVER_OK != 0
            && let Err(e) = self.activate(s, &[])
        {
            warn!("virtio device {}: {e}", s.device.device_id());
            s.status |= status::DEVICE_NEEDS_RESET;
            self.interrupt.config_change();
        }
    }

    /// Starts the device on the driver's queues, continuing from `progress` where a
    /// snapshot recorded it (empty: from the beginning).
    fn activate(&self, s: &mut State, progress: &[QueueState]) -> Result<(), String> {
        if s.status & status::FEATURES_OK == 0 {
            return Err("DRIVER_OK before FEATURES_OK".into());
        }
        let max = s.device.queue_max_sizes().to_vec();
        let mut queues = Vec::with_capacity(s.queues.len());
        for (i, (cfg, max)) in s.queues.iter().zip(max).enumerate() {
            if !cfg.ready {
                return Err(format!("queue {i} not ready at DRIVER_OK"));
            }
            let mut queue = Queue::new(*cfg, max, &self.memory, s.driver_features)
                .map_err(|e| format!("queue {i}: {e}"))?;
            if let Some(&st) = progress.get(i) {
                queue.set_state(st);
            }
            queues.push(queue);
        }
        s.device.activate(Activation {
            memory: self.memory.clone(),
            queues,
            interrupt: self.interrupt.clone(),
            features: s.driver_features,
        })
    }

    fn selected_queue(s: &mut State) -> Option<&mut QueueConfig> {
        s.queues.get_mut(s.queue_sel as usize)
    }
}

impl MmioDevice for MmioTransport {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let mut s = lock(&self.state);
        if offset >= CONFIG {
            s.device.read_config(offset - CONFIG, data);
            return;
        }
        if data.len() != 4 {
            // Registers below the config space are 32-bit only (virtio 1.3 §4.2.2.2).
            data.fill(0);
            return;
        }
        let v: u32 = match offset {
            MAGIC => MAGIC_VALUE,
            VERSION => 2,
            DEVICE_ID => s.device.device_id(),
            VENDOR_ID => VENDOR,
            DEVICE_FEATURES => match s.device_features_sel {
                0 => low(s.device.features()),
                1 => high(s.device.features()),
                _ => 0,
            },
            QUEUE_NUM_MAX => {
                let sel = s.queue_sel as usize;
                s.device.queue_max_sizes().get(sel).copied().map_or(0, u32::from)
            }
            QUEUE_READY => Self::selected_queue(&mut s).map_or(0, |q| u32::from(q.ready)),
            INTERRUPT_STATUS => self.interrupt.status(),
            STATUS if self.interrupt.failed() => s.status | status::DEVICE_NEEDS_RESET,
            STATUS => s.status,
            // No shared memory regions: length reads as all-ones (§4.2.2).
            SHM_LEN_LOW | SHM_LEN_HIGH => u32::MAX,
            CONFIG_GENERATION => s.config_generation,
            _ => 0,
        };
        put_le(data, u64::from(v));
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let mut s = lock(&self.state);
        if offset >= CONFIG {
            s.device.write_config(offset - CONFIG, data);
            return;
        }
        if data.len() != 4 {
            return;
        }
        let v = get_le(data) as u32;
        // Queue and feature layout may only change before the device goes live.
        let configurable = s.status & status::DRIVER_OK == 0;
        match offset {
            DEVICE_FEATURES_SEL => s.device_features_sel = v,
            DRIVER_FEATURES_SEL => s.driver_features_sel = v,
            DRIVER_FEATURES if configurable && s.status & status::FEATURES_OK == 0 => {
                match s.driver_features_sel {
                    0 => set_low(&mut s.driver_features, v),
                    1 => set_high(&mut s.driver_features, v),
                    _ => {}
                }
            }
            QUEUE_SEL => s.queue_sel = v,
            QUEUE_NUM if configurable => {
                if let Some(q) = Self::selected_queue(&mut s) {
                    q.size = v as u16;
                }
            }
            QUEUE_READY if configurable => {
                if let Some(q) = Self::selected_queue(&mut s) {
                    q.ready = v == 1;
                }
            }
            QUEUE_DESC_LOW | QUEUE_DESC_HIGH | QUEUE_DRIVER_LOW | QUEUE_DRIVER_HIGH | QUEUE_DEVICE_LOW
            | QUEUE_DEVICE_HIGH
                if configurable =>
            {
                if let Some(q) = Self::selected_queue(&mut s) {
                    match offset {
                        QUEUE_DESC_LOW => set_low(&mut q.desc, v),
                        QUEUE_DESC_HIGH => set_high(&mut q.desc, v),
                        QUEUE_DRIVER_LOW => set_low(&mut q.avail, v),
                        QUEUE_DRIVER_HIGH => set_high(&mut q.avail, v),
                        QUEUE_DEVICE_LOW => set_low(&mut q.used, v),
                        _ => set_high(&mut q.used, v),
                    }
                }
            }
            QUEUE_NOTIFY if s.status & status::DRIVER_OK != 0 => s.device.notify(v as u16),
            INTERRUPT_ACK => self.interrupt.ack(v),
            STATUS => self.set_status(&mut s, v),
            _ => {}
        }
    }

    fn pause(&self) {
        let mut s = lock(&self.state);
        s.paused = s.device.pause();
    }

    fn resume(&self) -> Result<(), String> {
        lock(&self.state).device.resume()
    }

    fn save(&self, w: &mut Writer) {
        let s = lock(&self.state);
        for v in [
            s.status,
            s.device_features_sel,
            s.driver_features_sel,
            s.queue_sel,
            s.config_generation,
            self.interrupt.status(),
        ] {
            w.u32(v);
        }
        w.u64(s.driver_features);
        w.bool(self.interrupt.failed());
        w.seq(&s.queues, |w, q| {
            w.u16(q.size);
            w.u64(q.desc);
            w.u64(q.avail);
            w.u64(q.used);
            w.bool(q.ready);
        });
        w.seq(&s.paused, |w, p| {
            w.u16(p.next_avail);
            w.u16(p.next_used);
            w.bool(p.signalled_used.is_some());
            w.u16(p.signalled_used.unwrap_or(0));
        });
    }

    /// A device that was live resumes on its queues at the saved progress, so requests
    /// published before the snapshot but not yet consumed are served.
    fn restore(&self, r: &mut Reader<'_>) -> codec::Result<()> {
        let mut s = lock(&self.state);
        let [status_reg, dev_sel, drv_sel, queue_sel, generation, interrupt] =
            [r.u32()?, r.u32()?, r.u32()?, r.u32()?, r.u32()?, r.u32()?];
        let driver_features = r.u64()?;
        let failed = r.bool()?;
        let queues = r.seq(s.queues.len(), |r| {
            Ok(QueueConfig {
                size: r.u16()?,
                desc: r.u64()?,
                avail: r.u64()?,
                used: r.u64()?,
                ready: r.bool()?,
            })
        })?;
        if queues.len() != s.queues.len() {
            return Err(codec::DecodeError(format!(
                "virtio snapshot has {} queues; the device has {}",
                queues.len(),
                s.queues.len()
            )));
        }
        let progress = r.seq(s.queues.len(), |r| {
            let (next_avail, next_used, signalled) = (r.u16()?, r.u16()?, r.bool()?);
            let at = r.u16()?;
            Ok(QueueState {
                next_avail,
                next_used,
                signalled_used: signalled.then_some(at),
            })
        })?;
        s.status = status_reg;
        s.device_features_sel = dev_sel;
        s.driver_features_sel = drv_sel;
        s.queue_sel = queue_sel;
        s.config_generation = generation;
        s.driver_features = driver_features;
        s.queues = queues;
        self.interrupt.set_state(interrupt, failed);
        let live = status::DRIVER_OK;
        let dead = status::DEVICE_NEEDS_RESET | status::FAILED;
        if s.status & live != 0 && s.status & dead == 0 && !failed {
            self.activate(&mut s, &progress)
                .map_err(|e| codec::DecodeError(format!("restoring virtio device: {e}")))?;
        }
        Ok(())
    }
}
