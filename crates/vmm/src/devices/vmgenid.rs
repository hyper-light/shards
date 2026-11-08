//! Virtual Machine Generation ID (Microsoft's VMGenID spec, as Linux implements it in
//! drivers/virt/vmgenid.c): 16 bytes of guest memory holding a random ID, and an edge
//! interrupt. At boot the driver mixes the ID into its entropy pool. Each new ID, with
//! the interrupt, makes the guest reseed its CRNG (`add_vmfork_randomness`), so clones of
//! one snapshot never share RNG state (docs/research/snapshot-restore-memory.md §2.4).

use std::sync::Arc;

use super::Interrupt;
use crate::memory::GuestMemory;
use crate::platform;

pub const SIZE: usize = 16;

pub struct VmGenId {
    memory: Arc<GuestMemory>,
    addr: u64,
    irq: Arc<dyn Interrupt>,
}

impl std::fmt::Debug for VmGenId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmGenId")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl VmGenId {
    pub fn new(memory: Arc<GuestMemory>, addr: u64, irq: Arc<dyn Interrupt>) -> VmGenId {
        VmGenId { memory, addr, irq }
    }

    /// Writes a fresh random ID. The guest maps it as a device, so the bytes are
    /// written back from the data cache.
    pub fn write_new_id(&self) -> Result<(), String> {
        let mut id = [0u8; SIZE];
        platform::fill_random(&mut id).map_err(|e| format!("VMGenID entropy: {e}"))?;
        let host = self
            .memory
            .host_ptr(self.addr, SIZE)
            .map_err(|e| format!("VMGenID: {e}"))?;
        self.memory
            .access()
            .map_err(|e| format!("VMGenID: {e}"))?
            .write(self.addr, &id)
            .map_err(|e| format!("VMGenID: {e}"))?;
        // SAFETY: `host` addresses the 16 guest bytes just written.
        unsafe { platform::clean_dcache(host, SIZE) };
        Ok(())
    }

    /// Starts a new generation: a fresh ID, then the interrupt telling the guest to
    /// reseed. Called for every restore, before its vCPUs run.
    pub fn new_generation(&self) -> Result<(), String> {
        self.write_new_id()?;
        self.irq.set_level(true);
        Ok(())
    }
}
