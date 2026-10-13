//! virtio-balloon (VIRTIO 1.3 §5.5) for its free page reporting alone (D151): the guest
//! hands the host memory it holds free, in whole blocks of its free lists
//! (`page_reporting_order`, its pageblock: 2 MiB with 4 KiB pages, mm/page_reporting.c),
//! and the host gives their pages back, so that a run's memory, once its workload frees
//! it, is the host's again, as a container's is. The balloon itself never inflates: the
//! device asks for no pages (`num_pages` 0), and its inflate and deflate queues stay idle.
//!
//! A reported range stays guest RAM, the guest's: its pages go back to the host and fresh
//! ones, zero-filled as touched, take their place before the report is answered, after
//! which the guest may use them again ([`crate::memory::Access::discard`]). Where the
//! hypervisor's stage 2 holds what it maps, the range is unmapped from the guest
//! meanwhile ([`Mapper`], PM M193).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::mem::Mapper;
use super::queue::{Chain, Queue};
use super::worker::Worker;
use super::{Activation, DeviceInterrupt, QueueState, VirtioDevice, feature};
use crate::memory::GuestMemory;

pub const DEVICE_ID: u32 = 5;
/// VIRTIO_BALLOON_F_REPORTING (include/uapi/linux/virtio_balloon.h).
const REPORTING: u64 = 1 << 5;
/// Linux reports at most PAGE_REPORTING_CAPACITY (32) ranges at once, each a descriptor of
/// one chain, and takes a reporting queue no smaller (virtio_balloon.c `virtballoon_probe`).
const QUEUE_SIZE: u16 = 64;
/// The queues as Linux's driver finds them with free page reporting alone: inflate,
/// deflate, then reporting, as it gives indexes only to the queues it names
/// (virtio_balloon.c `init_vqs`, virtio_mmio.c `vm_find_vqs`).
const QUEUES: [u16; 3] = [QUEUE_SIZE; 3];
const REPORTING_QUEUE: usize = 2;
/// `struct virtio_balloon_config`: le32 num_pages, actual, free_page_hint_cmd_id,
/// poison_val.
const CONFIG_LEN: usize = 16;
/// `actual` in it, the one field the driver writes.
const ACTUAL: usize = 4;

pub struct Balloon<M> {
    stage2: M,
    /// `actual`: the pages the driver says the balloon holds, which it writes.
    actual: [u8; 4],
    /// What a worker needs besides its queue, from activation until reset.
    context: Option<(Arc<GuestMemory>, Arc<DeviceInterrupt>)>,
    /// The inflate and deflate queues, which no worker serves.
    idle: Vec<Queue>,
    worker: Option<Worker>,
    /// The reporting queue while paused.
    paused: Option<Queue>,
    /// Whether a report the device could not give back has been said: once a device.
    said: Arc<AtomicBool>,
}

impl<M> std::fmt::Debug for Balloon<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Balloon").finish_non_exhaustive()
    }
}

impl<M: Mapper + Clone + 'static> Balloon<M> {
    /// A device whose reported ranges `stage2` takes from the guest and gives back.
    pub fn new(stage2: M) -> Balloon<M> {
        Balloon {
            stage2,
            actual: [0; 4],
            context: None,
            idle: Vec::new(),
            worker: None,
            paused: None,
            said: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Starts the worker on the reporting `queue`; if it cannot, the queue waits paused,
    /// as a pause leaves it, for a resume or a reset.
    fn start(&mut self, queue: Queue) -> Result<(), String> {
        let Some((memory, interrupt)) = self.context.clone() else {
            self.paused = Some(queue);
            return Err("virtio-balloon started before activation".into());
        };
        let (mem, stage2, said) = (memory.clone(), self.stage2.clone(), self.said.clone());
        let answering = move |chain: &Chain| answer(chain, &mem, &stage2, &said);
        match Worker::start("virtio-balloon", queue, memory, interrupt, answering) {
            Ok(worker) => {
                self.worker = Some(worker);
                Ok(())
            }
            Err((e, queue)) => {
                self.paused = Some(queue);
                Err(e)
            }
        }
    }

    /// Stops the worker after the report it is answering; returns its queue.
    fn stop(&mut self) -> Option<Queue> {
        self.worker.take()?.stop()
    }
}

impl<M: Mapper + Clone + 'static> VirtioDevice for Balloon<M> {
    fn device_id(&self) -> u32 {
        DEVICE_ID
    }

    fn features(&self) -> u64 {
        feature::VERSION_1 | REPORTING
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUES
    }

    /// No pages asked for, and `actual` as the driver wrote it.
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut config = [0u8; CONFIG_LEN];
        if let Some(actual) = config.get_mut(ACTUAL..ACTUAL + 4) {
            actual.copy_from_slice(&self.actual);
        }
        let at = usize::try_from(offset).ok();
        for (i, b) in data.iter_mut().enumerate() {
            *b = at
                .and_then(|at| at.checked_add(i))
                .and_then(|i| config.get(i))
                .copied()
                .unwrap_or(0);
        }
    }

    /// The driver writes `actual` alone (§5.5.6); the rest is the device's.
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        let Ok(at) = usize::try_from(offset) else {
            return;
        };
        for (i, b) in data.iter().enumerate() {
            if let Some(byte) = at
                .checked_add(i)
                .and_then(|i| i.checked_sub(ACTUAL))
                .and_then(|i| self.actual.get_mut(i))
            {
                *byte = *b;
            }
        }
    }

    fn activate(&mut self, activation: Activation) -> Result<(), String> {
        let Activation {
            memory,
            mut queues,
            interrupt,
            features,
            ..
        } = activation;
        if features & REPORTING == 0 || queues.len() != QUEUES.len() {
            // Nothing to serve: a driver that reports nothing has nothing to give back.
            self.idle = queues;
            return Ok(());
        }
        let reporting = queues.remove(REPORTING_QUEUE);
        self.idle = queues;
        self.context = Some((memory, interrupt));
        // An activation that fails leaves its queue: the driver sets the device up anew.
        self.start(reporting).inspect_err(|_| self.paused = None)
    }

    /// Reports are answered on the device's worker, which gives their memory back: never
    /// on the notifying vCPU's thread. The inflate and deflate queues are never used: the
    /// device asks for no pages.
    fn notify(&self, queue: u16) {
        if usize::from(queue) == REPORTING_QUEUE
            && let Some(w) = &self.worker
        {
            w.notify();
        }
    }

    fn reset(&mut self) {
        self.stop();
        self.paused = None;
        self.idle.clear();
        self.context = None;
        self.actual = [0; 4];
    }

    /// The worker stops after the report it is answering; each queue's progress, in
    /// order: inflate and deflate as they are, then reporting.
    fn pause(&mut self) -> Vec<QueueState> {
        if let Some(queue) = self.stop() {
            self.paused = Some(queue);
        }
        let Some(reporting) = &self.paused else {
            return self.idle.iter().map(Queue::state).collect();
        };
        self.idle
            .iter()
            .chain(std::iter::once(reporting))
            .map(Queue::state)
            .collect()
    }

    fn resume(&mut self) -> Result<(), String> {
        match self.paused.take() {
            Some(queue) => self.start(queue),
            None => Ok(()),
        }
    }
}

impl<M> Drop for Balloon<M> {
    fn drop(&mut self) {
        if let Some(w) = self.worker.take() {
            w.stop();
        }
    }
}

/// Answers one report: each range it names, guest RAM the guest holds free, given back
/// to the host, ranges that meet taken as one. A range that is no RAM of the guest's, or
/// that the host could not give back, is left as it is, and the first is said. Writes
/// nothing.
fn answer(chain: &Chain, memory: &GuestMemory, stage2: &impl Mapper, said: &AtomicBool) -> u32 {
    let Ok(access) = memory.access() else {
        return 0;
    };
    for (gpa, len) in merged(chain.writable().map(|d| (d.addr, u64::from(d.len)))) {
        let unmap = |gpa, size| stage2.unmap(gpa, size);
        let map = |host, gpa, size| {
            // SAFETY: guest RAM stays mapped in this process as long as the guest memory
            // does, which outlives the VM.
            unsafe { stage2.map(host, gpa, size) }
        };
        if let Err(e) = access.discard(gpa, len, unmap, map)
            && !said.swap(true, Ordering::Relaxed)
        {
            crate::warn!("virtio-balloon: memory reported free at {gpa:#x}+{len:#x} is kept: {e}");
        }
    }
    0
}

/// `ranges`, those that meet joined, in the order they came.
fn merged(ranges: impl Iterator<Item = (u64, u64)>) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (gpa, len) in ranges {
        match out.last_mut() {
            Some((at, n)) if at.checked_add(*n) == Some(gpa) => *n = n.saturating_add(len),
            _ => out.push((gpa, len)),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::devices::virtio::queue::Descriptor;
    use crate::platform::page_size;
    use std::sync::Mutex;

    const RAM: u64 = 0x8000_0000;
    const BLOCK: u64 = 2 << 20;

    /// Records the ranges it unmapped and mapped again, as Hypervisor.framework's stage 2
    /// would; refuses maps once told to.
    #[derive(Debug, Clone, Default)]
    struct Fake {
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Mapper for Fake {
        unsafe fn map(&self, _host: *mut u8, gpa: u64, len: usize) -> Result<(), String> {
            self.log.lock().unwrap().push(format!("map {gpa:#x}+{len:#x}"));
            Ok(())
        }

        fn unmap(&self, gpa: u64, len: usize) -> Result<(), String> {
            self.log.lock().unwrap().push(format!("unmap {gpa:#x}+{len:#x}"));
            Ok(())
        }
    }

    fn chain(ranges: &[(u64, u32)]) -> Chain {
        Chain {
            head: 0,
            descriptors: ranges
                .iter()
                .map(|&(addr, len)| Descriptor {
                    addr,
                    len,
                    writable: true,
                })
                .collect(),
        }
    }

    /// A reported range comes back zeroed, the rest of RAM as it was; ranges that meet are
    /// given back as one; the stage 2 unmaps each before its pages go and maps it after.
    #[test]
    fn reported_memory_comes_back_empty() {
        let memory = GuestMemory::anonymous(&[(RAM, 8 * BLOCK as usize)]).unwrap();
        let fill = vec![0xab; 8 * BLOCK as usize];
        memory.access().unwrap().write(RAM, &fill).unwrap();
        let fake = Fake::default();
        let said = AtomicBool::new(false);
        let report = chain(&[
            (RAM + BLOCK, BLOCK as u32),
            (RAM + 2 * BLOCK, BLOCK as u32),
            (RAM + 5 * BLOCK, BLOCK as u32),
        ]);
        assert_eq!(answer(&report, &memory, &fake, &said), 0);
        let mut back = vec![0u8; 8 * BLOCK as usize];
        memory.access().unwrap().read(RAM, &mut back).unwrap();
        let block = |i: u64| &back[(i * BLOCK) as usize..((i + 1) * BLOCK) as usize];
        for i in 0..8 {
            let want = if matches!(i, 1 | 2 | 5) { 0 } else { 0xab };
            assert!(block(i).iter().all(|b| *b == want), "block {i}");
        }
        assert_eq!(
            *fake.log.lock().unwrap(),
            [
                format!("unmap {:#x}+{:#x}", RAM + BLOCK, 2 * BLOCK),
                format!("map {:#x}+{:#x}", RAM + BLOCK, 2 * BLOCK),
                format!("unmap {:#x}+{:#x}", RAM + 5 * BLOCK, BLOCK),
                format!("map {:#x}+{:#x}", RAM + 5 * BLOCK, BLOCK),
            ]
        );
        assert!(!said.load(Ordering::Relaxed));
    }

    /// A range that is no RAM of the guest's is left alone, and said once; a range of part
    /// of a host page gives back only the host's whole pages within it.
    #[test]
    fn a_range_past_ram_or_in_part_of_a_page_keeps_what_is_not_wholly_its() {
        let p = page_size().unwrap() as u64;
        let memory = GuestMemory::anonymous(&[(RAM, 4 * p as usize)]).unwrap();
        memory
            .access()
            .unwrap()
            .write(RAM, &vec![0xcd; 4 * p as usize])
            .unwrap();
        let fake = Fake::default();
        let said = AtomicBool::new(false);
        let report = chain(&[(RAM + 3 * p, (2 * p) as u32), (RAM + p / 2, (2 * p) as u32)]);
        assert_eq!(answer(&report, &memory, &fake, &said), 0);
        assert!(said.load(Ordering::Relaxed), "a range past RAM is said");
        let mut back = vec![0u8; 4 * p as usize];
        memory.access().unwrap().read(RAM, &mut back).unwrap();
        let page = |i: u64| &back[(i * p) as usize..((i + 1) * p) as usize];
        assert!(page(0).iter().all(|b| *b == 0xcd), "a page in part kept");
        assert!(page(1).iter().all(|b| *b == 0), "the whole page given back");
        assert!(page(2).iter().all(|b| *b == 0xcd), "a page in part kept");
        assert!(page(3).iter().all(|b| *b == 0xcd), "past RAM: none given back");
    }

    /// A reported range whose pages the host holds none of, never touched, is left alone:
    /// nothing to give back, and the stage 2 untouched. Where the host cannot say which it
    /// never touched (Windows, `platform::untouched`), each range is given back.
    #[test]
    fn a_range_the_host_holds_nothing_of_is_left_alone() {
        let memory = GuestMemory::anonymous(&[(RAM, 4 * BLOCK as usize)]).unwrap();
        memory.access().unwrap().write(RAM, &[1]).unwrap();
        let fake = Fake::default();
        let said = AtomicBool::new(false);
        let report = chain(&[(RAM, BLOCK as u32), (RAM + 2 * BLOCK, BLOCK as u32)]);
        assert_eq!(answer(&report, &memory, &fake, &said), 0);
        let untouched = RAM + 2 * BLOCK;
        let host = memory.host_ptr(untouched, BLOCK as usize).unwrap();
        // SAFETY: guest RAM of `memory`, whole host pages of it.
        let says = unsafe {
            crate::platform::untouched(host, BLOCK as usize, crate::platform::page_size().unwrap())
        }
        .unwrap()
        .is_some();
        let mut given = vec![
            format!("unmap {RAM:#x}+{BLOCK:#x}"),
            format!("map {RAM:#x}+{BLOCK:#x}"),
        ];
        if !says {
            given.extend([
                format!("unmap {untouched:#x}+{BLOCK:#x}"),
                format!("map {untouched:#x}+{BLOCK:#x}"),
            ]);
        }
        assert_eq!(*fake.log.lock().unwrap(), given, "the touched block alone");
    }

    /// The configuration asks for no pages and holds what the driver writes of `actual`
    /// alone.
    #[test]
    fn the_device_asks_for_nothing_and_keeps_actual() {
        let mut b = Balloon::new(Fake::default());
        b.write_config(0, &[9; CONFIG_LEN]);
        let mut config = [0xffu8; CONFIG_LEN];
        b.read_config(0, &mut config);
        assert_eq!(config, [0, 0, 0, 0, 9, 9, 9, 9, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(b.features(), feature::VERSION_1 | REPORTING);
        assert_eq!(b.queue_max_sizes().len(), 3);
    }
}
