//! vCPU power states for PSCI (Arm DEN0022 PSCI 1.1). KVM keeps these in the kernel;
//! Hypervisor.framework leaves them to the VMM (ground-truth doc §5 row 4).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

use crate::arch::aarch64::state::Power;
use crate::arch::aarch64::{Entry, psci};
use crate::sync::{lock, wait};

#[derive(Debug)]
struct Slot {
    mpidr: u64,
    power: Mutex<Power>,
    /// A kick that has not yet returned `Canceled` to the caller. The run loop reads it
    /// before every entry to the guest, without a lock.
    kicked: AtomicBool,
    wake: Condvar,
}

/// What a vCPU does next, once it may run.
#[derive(Debug, PartialEq, Eq)]
pub enum Wake {
    /// Keep running where it is.
    Running,
    /// Enter the guest here first (a boot or CPU_ON).
    Start(Entry),
    /// A kick arrived while it was parked.
    Kicked,
}

#[derive(Debug)]
pub struct Table {
    slots: Vec<Slot>,
    /// Guest RAM ranges `(start, end)`: CPU_ON refuses entry points outside them.
    ram: Mutex<Vec<(u64, u64)>>,
}

impl Table {
    /// Every vCPU starts powered off.
    pub fn new(mpidrs: &[u64]) -> Table {
        Table {
            slots: mpidrs
                .iter()
                .map(|&mpidr| Slot {
                    mpidr,
                    power: Mutex::new(Power::Off),
                    kicked: AtomicBool::new(false),
                    wake: Condvar::new(),
                })
                .collect(),
            ram: Mutex::new(Vec::new()),
        }
    }

    pub fn add_ram(&self, start: u64, end: u64) {
        lock(&self.ram).push((start, end));
    }

    /// Powers `index` on at `entry` (the boot vCPU).
    pub fn boot(&self, index: usize, entry: Entry) {
        if let Some(slot) = self.slots.get(index) {
            *lock(&slot.power) = Power::Pending(entry);
        }
    }

    /// Parks vCPU `index` while it is off, like KVM_RUN on a vCPU with
    /// `KVM_ARM_VCPU_POWER_OFF`, until CPU_ON or a kick.
    pub fn park(&self, index: usize) -> Wake {
        let Some(slot) = self.slots.get(index) else {
            return Wake::Kicked;
        };
        let mut power = lock(&slot.power);
        loop {
            if slot.kicked.swap(false, Ordering::AcqRel) {
                return Wake::Kicked;
            }
            match *power {
                Power::On => return Wake::Running,
                Power::Pending(entry) => {
                    *power = Power::On;
                    return Wake::Start(entry);
                }
                Power::Off => power = wait(&slot.wake, power),
            }
        }
    }

    /// vCPU `index`'s power state, for a snapshot.
    pub fn state(&self, index: usize) -> Option<Power> {
        self.slots.get(index).map(|slot| *lock(&slot.power))
    }

    /// Sets vCPU `index`'s power state, on restore.
    pub fn set_state(&self, index: usize, power: Power) {
        if let Some(slot) = self.slots.get(index) {
            *lock(&slot.power) = power;
        }
    }

    /// PSCI CPU_OFF by the calling vCPU.
    pub fn off(&self, index: usize) {
        if let Some(slot) = self.slots.get(index) {
            *lock(&slot.power) = Power::Off;
        }
    }

    /// Records a kick and wakes the vCPU if it is parked. The caller cancels its run
    /// after this (hv_vcpus_exit), so a vCPU checking before it enters the guest, or
    /// leaving the guest for another reason as the cancel arrives, still sees the kick.
    pub fn kick(&self, index: usize) {
        if let Some(slot) = self.slots.get(index) {
            slot.kicked.store(true, Ordering::Release);
            // Taking the lock orders the wakeup after a parked vCPU's look at `kicked`.
            drop(lock(&slot.power));
            slot.wake.notify_all();
        }
    }

    /// Takes a kick not yet delivered: the run loop's check before each entry.
    pub fn take_kick(&self, index: usize) -> bool {
        self.slots
            .get(index)
            .is_some_and(|slot| slot.kicked.swap(false, Ordering::AcqRel))
    }

    /// The vCPU returned `Canceled` from the guest, which delivers any pending kick.
    pub fn kick_delivered(&self, index: usize) {
        if let Some(slot) = self.slots.get(index) {
            slot.kicked.store(false, Ordering::Release);
        }
    }

    pub fn cpu_on(&self, target: u64, entry: u64, context: u64) -> i64 {
        if target & !psci::MPIDR_AFFINITY_MASK != 0 {
            return psci::INVALID_PARAMETERS;
        }
        let Some(slot) = self.slots.iter().find(|s| s.mpidr == target) else {
            return psci::INVALID_PARAMETERS;
        };
        let in_ram = lock(&self.ram)
            .iter()
            .any(|&(start, end)| entry >= start && entry.saturating_add(4) <= end);
        if !in_ram {
            return psci::INVALID_ADDRESS;
        }
        let mut power = lock(&slot.power);
        match *power {
            Power::On => psci::ALREADY_ON,
            Power::Pending(_) => psci::ON_PENDING,
            Power::Off => {
                *power = Power::Pending(Entry {
                    pc: entry,
                    x0: context,
                });
                slot.wake.notify_all();
                psci::SUCCESS
            }
        }
    }

    pub fn affinity_info(&self, target: u64) -> i64 {
        match self.slots.iter().find(|s| s.mpidr == target) {
            None => psci::INVALID_PARAMETERS,
            Some(slot) => match *lock(&slot.power) {
                Power::Off => psci::AFF_OFF,
                Power::Pending(_) => psci::AFF_ON_PENDING,
                Power::On => psci::AFF_ON,
            },
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn cpu_on_releases_a_parked_vcpu_once() {
        let t = Arc::new(Table::new(&[0, 1]));
        t.add_ram(0x8000_0000, 0x9000_0000);
        let parked = {
            let t = t.clone();
            std::thread::spawn(move || t.park(1))
        };
        assert_eq!(t.affinity_info(1), psci::AFF_OFF);
        assert_eq!(t.cpu_on(1, 0x7000_0000, 0), psci::INVALID_ADDRESS);
        assert_eq!(t.cpu_on(1 << 40, 0x8000_0000, 0), psci::INVALID_PARAMETERS);
        assert_eq!(t.cpu_on(7, 0x8000_0000, 0), psci::INVALID_PARAMETERS);
        assert_eq!(t.cpu_on(1, 0x8000_1000, 42), psci::SUCCESS);
        assert_eq!(
            parked.join().unwrap(),
            Wake::Start(Entry {
                pc: 0x8000_1000,
                x0: 42
            })
        );
        assert_eq!(t.cpu_on(1, 0x8000_1000, 42), psci::ALREADY_ON);
        assert_eq!(t.affinity_info(1), psci::AFF_ON);
        t.off(1);
        assert_eq!(t.affinity_info(1), psci::AFF_OFF);
    }

    #[test]
    fn a_kick_wakes_a_parked_vcpu_and_is_consumed() {
        let t = Arc::new(Table::new(&[0]));
        let parked = {
            let t = t.clone();
            std::thread::spawn(move || t.park(0))
        };
        t.kick(0);
        assert_eq!(parked.join().unwrap(), Wake::Kicked);
        t.boot(
            0,
            Entry {
                pc: 0x8000_0000,
                x0: 0,
            },
        );
        assert!(matches!(t.park(0), Wake::Start(_)));
        assert_eq!(t.park(0), Wake::Running);
    }
}
