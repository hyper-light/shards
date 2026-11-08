//! A host's PCI function its hypervisor assigns to the VM (D154): Windows' Discrete Device
//! Assignment, through the Windows Hypervisor Platform's virtual PCI devices (Microsoft
//! Learn, "Virtual PCI Data Types", and each function's page; hv/whp/vpci.rs). Its
//! configuration space the device's, read and written through its hypervisor
//! (WHvReadVpciDeviceRegister, WHvWriteVpciDeviceRegister), but for what the VMM keeps, as
//! of a function VFIO passes through (vfio.rs): its BARs, placed by the VMM; its expansion
//! ROM, none; its interrupt line and pin, none, as WHP maps a function's MSI and MSI-X
//! alone (WHvMapVpciDeviceInterrupt); its MSI and MSI-X capabilities, each message the
//! guest writes mapped to the vCPUs it names and the device given the message WHP returns
//! for it, as that function's page says to; and its power state, which WHP sets
//! (WHvSetVpciDevicePowerState), the function's BARs this process's in D0 alone
//! (WHvMapVpciDeviceMmioRanges), as Microsoft's own VMM routes a guest's power management
//! there (OpenVMM's vmm_core/virt_whp/src/device.rs, 58d8ab84, `AssignedPciDevice`). Its
//! DMA the host's IOMMU remaps to the partition's guest-physical addresses as the device is
//! made (WHvCreateVpciDevice's remarks): all of guest RAM, as a VFIO function's outside the
//! guest's IOMMU. A function of its own on its device: Windows has no IOMMU groups to keep
//! together.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::passed::{self, in_bars, kept, windows_of};
use super::{Bar, Header, Identity, Interrupts, MsiCap, MsixCap, MsixTable, PciFunction, reg};
use crate::devices::{MmioDevice, get_le, put_le};
use crate::snapshot::codec::{self, Reader, Writer};

/// A space of the function's registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Space {
    Config,
    Bar(usize),
}

/// Part of a BAR its hypervisor maps into this process while the function is in D0: `len`
/// bytes at `offset` of BAR `bar`, at `host` here, readable and writable as it says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub bar: usize,
    pub offset: u64,
    pub len: u64,
    pub host: usize,
    pub read: bool,
    pub write: bool,
}

/// The function as its hypervisor gives it.
pub trait Device: Send + Sync {
    /// How its host names it.
    fn name(&self) -> &str;
    fn read(&self, space: Space, offset: u64, data: &mut [u8]) -> Result<(), String>;
    fn write(&self, space: Space, offset: u64, data: &[u8]) -> Result<(), String>;
    /// Puts it in power state D`state`, 0 to 3.
    fn power(&self, state: u8) -> Result<(), String>;
    /// Maps its BARs into this process, which it must be in D0 for: each part of each.
    fn map_ranges(&self) -> Result<Vec<Range>, String>;
    /// Unmaps what [`Device::map_ranges`] mapped.
    fn unmap_ranges(&self) -> Result<(), String>;
    /// Maps the guest's MSI of `address` and `data` as its interrupt `index`: the message
    /// the device is to send for it.
    fn map_interrupt(&self, index: u32, address: u64, data: u32) -> Result<(u64, u32), String>;
    fn unmap_interrupt(&self, index: u32) -> Result<(), String>;
}

/// Where the parts of its BARs the guest reaches without exits are put into the guest.
pub trait Windows: Send + Sync {
    /// Maps `len` bytes at `host` at `gpa`, writable by the guest where `writable`.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped until unmapped here or the VM is destroyed.
    unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize, writable: bool) -> Result<(), String>;
    fn unmap(&self, gpa: u64, len: usize) -> Result<(), String>;
}

/// An MSI-X vector as it is at the device: the guest's message WHP maps for it, and
/// whether it is unmasked there.
#[derive(Debug, Clone, Copy, Default)]
struct Vector {
    mapped: Option<(u64, u32)>,
    unmasked: bool,
}

#[derive(Debug, Default)]
struct State {
    /// Its power state, D0 (0) to D3hot (3), as the guest last set it.
    power: u8,
    /// Whether its memory decoding is on, as the guest last wrote its command register.
    decoding: bool,
    /// Its BARs' parts in this process, while it is in D0.
    ranges: Vec<Range>,
    /// Its BARs' parts in the guest now: each where, and how long.
    mapped: Vec<(u64, usize)>,
    /// The guest's MSI, which WHP maps as interrupt 0, where MSI is on at the device.
    msi: Option<(u64, u32)>,
    /// MSI-X's table, the guest's.
    table: Option<MsixTable>,
    /// Where MSI-X is on at the device: whether its function is masked there.
    msix: Option<bool>,
    /// Each MSI-X vector, which WHP maps as interrupt 1 + its number.
    vectors: Vec<Vector>,
}

/// A function its hypervisor assigns to the VM, as the guest sees it on its bus.
pub struct AssignedFunction {
    device: Box<dyn Device>,
    windows: Box<dyn Windows>,
    kept: Header,
    /// Its MSI and MSI-X capabilities' places and lengths.
    caps: Vec<(u16, u16)>,
    msi: Option<MsiCap>,
    msix: Option<MsixCap>,
    /// Its power management control/status register's place, where it has one.
    pm: Option<u16>,
    page: u64,
    state: Mutex<State>,
}

impl std::fmt::Debug for AssignedFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssignedFunction")
            .field("name", &self.device.name())
            .field("bars", &self.kept.bars())
            .finish_non_exhaustive()
    }
}

impl AssignedFunction {
    /// `device`, its BARs at `bars` (of [`passed::sized_bars`]' sizes), the parts of them it
    /// maps put into the guest through `windows` in pages of `page` bytes: in D0 from the
    /// start, as a guest's kernel finds a function its firmware left on.
    pub fn new(
        device: Box<dyn Device>,
        bars: Vec<Bar>,
        windows: Box<dyn Windows>,
        page: u64,
    ) -> Result<AssignedFunction, String> {
        let config = |at: u16, len: usize| -> u32 {
            let mut raw = [0u8; 4];
            let bytes = raw.get_mut(..len.min(4)).unwrap_or_default();
            match device.read(Space::Config, u64::from(at), bytes) {
                Ok(()) => get_le(bytes) as u32,
                Err(_) => 0,
            }
        };
        let msi = passed::msi_cap_of(&config);
        // MSI-X where its table and pending bits lie in the device's BARs, as they must for
        // the VMM to keep its table.
        let msix = passed::msix_cap_of(&config).filter(|cap| {
            let size = u64::from(cap.size);
            let fits = in_bars(&bars, cap.table, size * reg::MSIX_ENTRY)
                && in_bars(&bars, cap.pba, size.div_ceil(64) * 8);
            if !fits {
                crate::warn!(
                    "{}: its MSI-X table or pending bits lie past its BARs",
                    device.name()
                );
            }
            fits
        });
        if msi.is_none() && msix.is_none() {
            crate::warn!(
                "{}: no MSI or MSI-X: WHP maps a function's MSI and MSI-X alone (WHvMapVpciDeviceInterrupt), so none of its interrupts reaches the guest",
                device.name()
            );
        }
        let pm = passed::cap_of(&config, passed::CAP_PM).map(|at| at + passed::PM_CONTROL);
        let kept = Header::with_interrupts(
            Identity {
                vendor: 0,
                device: 0,
                class: 0,
                revision: 0,
                subsystem_vendor: 0,
                subsystem: 0,
            },
            bars,
            Interrupts {
                pin: 0,
                msi: msi.map(|(cap, _)| cap),
                msix,
            },
        )?;
        let mut caps: Vec<(u16, u16)> = msi.map(|(cap, len)| (cap.at, len)).into_iter().collect();
        caps.extend(msix.map(|cap| (cap.at, reg::MSIX_LEN)));
        device
            .power(0)
            .map_err(|e| format!("{}: powering it on (D0): {e}", device.name()))?;
        let ranges = device
            .map_ranges()
            .map_err(|e| format!("{}: mapping its BARs: {e}", device.name()))?;
        let vectors = msix.map_or(0, |cap| usize::from(cap.size));
        Ok(AssignedFunction {
            device,
            windows,
            kept,
            caps,
            msi: msi.map(|(cap, _)| cap),
            msix,
            pm,
            page,
            state: Mutex::new(State {
                ranges,
                table: msix.map(|cap| MsixTable::new(cap.size)),
                vectors: vec![Vector::default(); vectors],
                ..State::default()
            }),
        })
    }

    /// Its BARs, where the VMM placed them.
    pub fn bars(&self) -> &[Bar] {
        self.kept.bars()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn name(&self) -> &str {
        self.device.name()
    }

    /// Writes `bytes` at `at` of its configuration space, at the device.
    fn put(&self, at: u16, bytes: &[u8]) -> Result<(), String> {
        self.device.write(Space::Config, u64::from(at), bytes)
    }

    /// Its own register at `at` of `space`, of 2 or 4 bytes.
    fn get(&self, space: Space, at: u64, len: usize) -> Result<u32, String> {
        let mut raw = [0u8; 4];
        let bytes = raw.get_mut(..len.min(4)).unwrap_or_default();
        self.device.read(space, at, bytes)?;
        Ok(get_le(bytes) as u32)
    }

    /// Puts its BARs' parts into the guest (`on`), or takes them out: as the guest turns its
    /// memory decoding on in D0, and before it turns it off or leaves D0. Out, accesses
    /// there exit to the VMM ([`AssignedBar`]).
    fn map_windows(&self, state: &mut State, on: bool) {
        if !on {
            let taken = state.mapped.len();
            for (gpa, len) in state.mapped.drain(..) {
                if let Err(e) = self.windows.unmap(gpa, len) {
                    crate::warn!("{}: a BAR's window at {gpa:#x}: {e}", self.device.name());
                }
            }
            crate::debug!("{}: BAR windows taken from the guest: {taken}", self.name());
            return;
        }
        if state.power != 0 || !state.mapped.is_empty() {
            return;
        }
        let page = self.page;
        for bar in self.kept.bars() {
            for (offset, len) in windows_of(bar.index, bar.size, self.msix, page) {
                for r in state.ranges.iter().filter(|r| r.bar == bar.index && r.read) {
                    let start = offset.max(r.offset);
                    let (Some(end), Some(r_end)) = (offset.checked_add(len), r.offset.checked_add(r.len))
                    else {
                        continue;
                    };
                    let end = end.min(r_end);
                    let host = usize::try_from(start - r.offset)
                        .ok()
                        .and_then(|o| r.host.checked_add(o));
                    let (Some(host), Some(gpa), Ok(len)) = (
                        host,
                        bar.addr.checked_add(start),
                        usize::try_from(end.saturating_sub(start)),
                    ) else {
                        continue;
                    };
                    // Whole pages alone: what is left of a page stays trapped.
                    if len == 0
                        || !start.is_multiple_of(page)
                        || !end.is_multiple_of(page)
                        || !(host as u64).is_multiple_of(page)
                    {
                        continue;
                    }
                    // SAFETY: the range stays mapped in this process while the function is
                    // in D0, and its windows leave the guest before it leaves D0 or before
                    // its ranges are unmapped (`set_power`, `Drop`).
                    match unsafe { self.windows.map(host as *mut u8, gpa, len, r.write) } {
                        Ok(()) => state.mapped.push((gpa, len)),
                        Err(e) => crate::warn!("{}: a BAR's window at {gpa:#x}: {e}", self.name()),
                    }
                }
            }
        }
        crate::debug!(
            "{}: BAR windows mapped into the guest: {}",
            self.name(),
            state.mapped.len()
        );
    }

    /// Puts the function in power state D`to`, as the guest asked through its control
    /// register: its windows and ranges let go of before it leaves D0, and taken again as
    /// it comes back.
    fn set_power(&self, state: &mut State, to: u8) {
        if to == state.power {
            return;
        }
        if to != 0 {
            if state.power == 0 {
                self.map_windows(state, false);
                if let Err(e) = self.device.unmap_ranges() {
                    crate::warn!("{}: unmapping its BARs: {e}", self.name());
                }
                state.ranges.clear();
            }
            match self.device.power(to) {
                Ok(()) => state.power = to,
                Err(e) => crate::warn!("{}: to D{to}: {e}", self.name()),
            }
            return;
        }
        if let Err(e) = self.device.power(0) {
            crate::warn!("{}: to D0: {e}", self.name());
            return;
        }
        state.power = 0;
        match self.device.map_ranges() {
            Ok(ranges) => state.ranges = ranges,
            Err(e) => crate::warn!("{}: mapping its BARs: {e}", self.name()),
        }
        if state.decoding {
            self.map_windows(state, true);
        }
    }

    /// Brings its interrupts at the device to what the guest set (§6.8): MSI-X where it
    /// enabled MSI-X, else MSI where it enabled MSI, one of them on at a time, what is no
    /// longer wanted first.
    fn sync(&self, state: &mut State) {
        let msix = self.kept.msix().filter(|m| m.enabled);
        let msi = if msix.is_some() { None } else { self.kept.msi() };
        if msix.is_none() {
            self.msix_off(state);
        }
        if state.msi != msi {
            self.msi_off(state);
            if let Some((address, data)) = msi {
                self.msi_on(state, address, data);
            }
        }
        if let Some(m) = msix {
            self.msix_on(state, m.masked);
        }
    }

    /// MSI on at the device with the message WHP maps the guest's to (§6.8.1): one
    /// message, as the guest is offered.
    fn msi_on(&self, state: &mut State, address: u64, data: u32) {
        let Some(cap) = self.msi else {
            return;
        };
        let (sent, with) = match self.device.map_interrupt(0, address, data) {
            Ok(m) => m,
            Err(e) => {
                crate::warn!("{}: MSI to {address:#x} with {data:#x}: {e}", self.name());
                return;
            }
        };
        let on = self.msi_message(cap, sent, with);
        match on {
            Ok(()) => state.msi = Some((address, data)),
            Err(e) => {
                crate::warn!("{}: MSI to {address:#x} with {data:#x}: {e}", self.name());
                if let Err(e) = self.device.unmap_interrupt(0) {
                    crate::warn!("{}: MSI's mapping: {e}", self.name());
                }
            }
        }
    }

    /// Gives the device's MSI capability the message `sent` with `with`, and enables it,
    /// its Multiple Message Enable (bits 6:4) 0: one message.
    fn msi_message(&self, cap: MsiCap, sent: u64, with: u32) -> Result<(), String> {
        if !cap.is64 && sent >> 32 != 0 {
            return Err(format!("its 32-bit MSI given address {sent:#x}"));
        }
        self.put(cap.at + reg::MSI_ADDRESS, &(sent as u32).to_le_bytes())?;
        if cap.is64 {
            self.put(
                cap.at + reg::MSI_ADDRESS_HIGH,
                &((sent >> 32) as u32).to_le_bytes(),
            )?;
        }
        let data_at = if cap.is64 { reg::MSI_DATA } else { reg::MSI_DATA_32 };
        self.put(cap.at + data_at, &(with as u16).to_le_bytes())?;
        let control = self.get(Space::Config, u64::from(cap.at + reg::MSI_CONTROL), 2)? as u16;
        let control = (control & !0x70) | reg::MSI_ENABLE;
        self.put(cap.at + reg::MSI_CONTROL, &control.to_le_bytes())
    }

    fn msi_off(&self, state: &mut State) {
        let (Some(cap), Some(_)) = (self.msi, state.msi.take()) else {
            return;
        };
        let off = self
            .get(Space::Config, u64::from(cap.at + reg::MSI_CONTROL), 2)
            .and_then(|control| {
                let control = control as u16 & !reg::MSI_ENABLE;
                self.put(cap.at + reg::MSI_CONTROL, &control.to_le_bytes())
            });
        if let Err(e) = off {
            crate::warn!("{}: MSI off: {e}", self.name());
        }
        if let Err(e) = self.device.unmap_interrupt(0) {
            crate::warn!("{}: MSI's mapping: {e}", self.name());
        }
    }

    /// Writes MSI-X's Enable and Function Mask at the device, as `bits` has them.
    fn msix_control(&self, cap: MsixCap, bits: u16) -> Result<(), String> {
        let at = cap.at + reg::MSIX_CONTROL;
        let control = self.get(Space::Config, u64::from(at), 2)? as u16;
        let control = (control & !(reg::MSIX_ENABLE | reg::MSIX_MASK)) | bits;
        self.put(at, &control.to_le_bytes())
    }

    /// MSI-X on at the device (§6.8.2), its function masked there until each vector is as
    /// the guest left it, every vector masked there from reset (§6.8.2.9); then its function
    /// masked as the guest's is.
    fn msix_on(&self, state: &mut State, masked: bool) {
        let Some(cap) = self.msix else {
            return;
        };
        if state.msix.is_none() {
            if let Err(e) = self.msix_control(cap, reg::MSIX_ENABLE | reg::MSIX_MASK) {
                crate::warn!("{}: MSI-X on: {e}", self.name());
                return;
            }
            state.msix = Some(true);
        }
        for k in 0..usize::from(cap.size) {
            self.vector(state, k);
        }
        if state.msix != Some(masked) {
            let bits = reg::MSIX_ENABLE | if masked { reg::MSIX_MASK } else { 0 };
            match self.msix_control(cap, bits) {
                Ok(()) => state.msix = Some(masked),
                Err(e) => crate::warn!("{}: MSI-X's function mask: {e}", self.name()),
            }
        }
    }

    fn msix_off(&self, state: &mut State) {
        let (Some(cap), Some(_)) = (self.msix, state.msix.take()) else {
            return;
        };
        if let Err(e) = self.msix_control(cap, 0) {
            crate::warn!("{}: MSI-X off: {e}", self.name());
        }
        for (k, vector) in state.vectors.iter_mut().enumerate() {
            if vector.unmasked {
                if let Err(e) = self.entry_mask(cap, k, true) {
                    crate::warn!("{}: MSI-X vector {k}: {e}", self.name());
                }
                vector.unmasked = false;
            }
            if vector.mapped.take().is_some()
                && let Err(e) = self.device.unmap_interrupt(k as u32 + 1)
            {
                crate::warn!("{}: MSI-X vector {k}'s mapping: {e}", self.name());
            }
        }
    }

    /// Where vector `k`'s entry is in MSI-X's table at the device, and in which BAR.
    fn entry(cap: MsixCap, k: usize) -> (Space, u64) {
        (
            Space::Bar(usize::from(cap.table.0)),
            u64::from(cap.table.1) + k as u64 * reg::MSIX_ENTRY,
        )
    }

    /// Masks vector `k` at the device, or unmasks it, its vector control's other bits as
    /// they were (§6.8.2.9).
    fn entry_mask(&self, cap: MsixCap, k: usize, masked: bool) -> Result<(), String> {
        let (space, entry) = AssignedFunction::entry(cap, k);
        let control = self.get(space, entry + 12, 4)?;
        let control = if masked {
            control | reg::MSIX_ENTRY_MASKED
        } else {
            control & !reg::MSIX_ENTRY_MASKED
        };
        self.device.write(space, entry + 12, &control.to_le_bytes())
    }

    /// MSI-X vector `k` at the device as the guest left it, while MSI-X is on there: where
    /// the guest unmasked it, the message WHP maps the guest's to, mapped again only as
    /// the guest's changes, written to its entry, masked while it changes, and the vector
    /// unmasked; else masked, its pending bit the device's to set (§6.8.2.10).
    fn vector(&self, state: &mut State, k: usize) {
        let (Some(cap), Some(_)) = (self.msix, state.msix) else {
            return;
        };
        let message = state.table.as_ref().and_then(|t| t.message(k));
        let Some(vector) = state.vectors.get_mut(k) else {
            return;
        };
        if let Some((address, data)) = message
            && vector.mapped != message
        {
            if vector.unmasked {
                match self.entry_mask(cap, k, true) {
                    Ok(()) => vector.unmasked = false,
                    Err(e) => {
                        crate::warn!("{}: MSI-X vector {k}: {e}", self.name());
                        return;
                    }
                }
            }
            if vector.mapped.take().is_some()
                && let Err(e) = self.device.unmap_interrupt(k as u32 + 1)
            {
                crate::warn!("{}: MSI-X vector {k}'s mapping: {e}", self.name());
            }
            let index = k as u32 + 1;
            let written = self
                .device
                .map_interrupt(index, address, data)
                .and_then(|(sent, with)| {
                    let (space, entry) = AssignedFunction::entry(cap, k);
                    let written = self
                        .device
                        .write(space, entry, &(sent as u32).to_le_bytes())
                        .and_then(|()| {
                            self.device
                                .write(space, entry + 4, &((sent >> 32) as u32).to_le_bytes())
                        })
                        .and_then(|()| self.device.write(space, entry + 8, &with.to_le_bytes()));
                    if written.is_err() {
                        let _ = self.device.unmap_interrupt(index);
                    }
                    written
                });
            match written {
                Ok(()) => vector.mapped = message,
                Err(e) => crate::warn!(
                    "{}: MSI-X vector {k} to {address:#x} with {data:#x}: {e}",
                    self.name()
                ),
            }
        }
        let unmasked = message.is_some() && vector.mapped.is_some();
        if vector.unmasked != unmasked {
            match self.entry_mask(cap, k, !unmasked) {
                Ok(()) => vector.unmasked = unmasked,
                Err(e) => crate::warn!("{}: MSI-X vector {k}: {e}", self.name()),
            }
        }
    }

    /// Where an access at `offset` of BAR `bar` lands in MSI-X's table, which the VMM
    /// keeps, if it does.
    fn in_table(&self, bar: usize, offset: u64) -> Option<u64> {
        let cap = self.msix?;
        let at = offset.checked_sub(u64::from(cap.table.1))?;
        (usize::from(cap.table.0) == bar && at < u64::from(cap.size) * reg::MSIX_ENTRY).then_some(at)
    }

    /// The range that maps `len` bytes at `offset` of BAR `bar` into this process for a
    /// write (`write`) or a read, and where they are there.
    fn host_of(state: &State, bar: usize, offset: u64, len: usize, write: bool) -> Option<usize> {
        state.ranges.iter().find_map(|r| {
            let from = offset.checked_sub(r.offset)?;
            let fits = from.checked_add(len as u64)? <= r.len;
            (r.bar == bar && fits && if write { r.write } else { r.read })
                .then(|| r.host.checked_add(usize::try_from(from).ok()?))
                .flatten()
        })
    }

    /// Reads BAR `bar` at `offset`: all ones where the function decodes nothing (out of D0,
    /// or its memory decoding off), as an access no function claims (PCI 3.0 §6.2.2);
    /// MSI-X's table the guest's; the rest the device's, through this process's mapping
    /// where an access of its width can be made there, else through WHP.
    fn bar_read(&self, bar: usize, offset: u64, data: &mut [u8]) {
        let state = self.state();
        if state.power != 0 || !state.decoding {
            data.fill(0xff);
            return;
        }
        if let Some(at) = self.in_table(bar, offset) {
            if let Some(table) = &state.table {
                table.read(at, data);
            }
            return;
        }
        if let Some(host) = AssignedFunction::host_of(&state, bar, offset, data.len(), false)
            && volatile_read(host, data)
        {
            return;
        }
        if self.device.read(Space::Bar(bar), offset, data).is_err() {
            data.fill(0xff);
        }
    }

    /// Writes BAR `bar` at `offset`: nowhere where the function decodes nothing; a write
    /// of MSI-X's table brings its vector at the device to what the guest left in it; the
    /// rest the device's.
    fn bar_write(&self, bar: usize, offset: u64, data: &[u8]) {
        let mut state = self.state();
        if state.power != 0 || !state.decoding {
            return;
        }
        if let Some(at) = self.in_table(bar, offset) {
            if let Some(k) = state.table.as_mut().and_then(|table| table.write(at, data)) {
                self.vector(&mut state, k);
            }
            return;
        }
        if let Some(host) = AssignedFunction::host_of(&state, bar, offset, data.len(), true)
            && volatile_write(host, data)
        {
            return;
        }
        if let Err(e) = self.device.write(Space::Bar(bar), offset, data) {
            crate::warn!("{}: BAR {bar} at {offset:#x}: {e}", self.name());
        }
    }
}

/// Reads `data` from `at` in this process as one access of its width, aligned to it; false
/// where none can be made.
fn volatile_read(at: usize, data: &mut [u8]) -> bool {
    if !at.is_multiple_of(data.len().max(1)) {
        return false;
    }
    // SAFETY: `at` lies in a range the hypervisor maps into this process, `data.len()`
    // bytes of it (`host_of`), aligned to the access.
    let value = unsafe {
        match data.len() {
            1 => u64::from((at as *const u8).read_volatile()),
            2 => u64::from((at as *const u16).read_volatile()),
            4 => u64::from((at as *const u32).read_volatile()),
            8 => (at as *const u64).read_volatile(),
            _ => return false,
        }
    };
    put_le(data, value);
    true
}

/// Writes `data` at `at` in this process as one access of its width, aligned to it; false
/// where none can be made.
fn volatile_write(at: usize, data: &[u8]) -> bool {
    if !at.is_multiple_of(data.len().max(1)) {
        return false;
    }
    let value = get_le(data);
    // SAFETY: as `volatile_read`'s, the range writable.
    unsafe {
        match data.len() {
            1 => (at as *mut u8).write_volatile(value as u8),
            2 => (at as *mut u16).write_volatile(value as u16),
            4 => (at as *mut u32).write_volatile(value as u32),
            8 => (at as *mut u64).write_volatile(value),
            _ => return false,
        }
    }
    true
}

/// The byte of `data`, read or written at `offset`, that is register byte `at`, if one is.
fn byte_at(offset: u16, data: &mut [u8], at: u16) -> Option<&mut u8> {
    data.get_mut(usize::from(at.checked_sub(offset)?))
}

impl PciFunction for AssignedFunction {
    fn config_read(&self, offset: u16, data: &mut [u8]) {
        if kept(offset, &self.caps) {
            self.kept.read(offset, data);
        } else if self.device.read(Space::Config, u64::from(offset), data).is_err() {
            data.fill(0xff);
        }
        // A single function: its device on the guest's bus has no others (the header
        // type's bit 7, PCIe 6.0 §7.5.1.1.9).
        if let Some(header) = byte_at(offset, data, reg::HEADER_TYPE) {
            *header &= 0x7f;
        }
        if let Some(pm) = self.pm
            && let Some(control) = byte_at(offset, data, pm)
        {
            *control = (*control & !passed::PM_STATE) | self.state().power;
        }
    }

    fn config_write(&self, offset: u16, data: &[u8]) {
        // Its power state, which WHP sets: the guest's writes of its control register go
        // there alone (its PME bits unused, as the guest is offered no PME).
        if let Some(pm) = self.pm
            && (pm..pm + 4).contains(&offset)
        {
            if offset == pm
                && let Some(&control) = data.first()
            {
                self.set_power(&mut self.state(), control & passed::PM_STATE);
            }
            return;
        }
        if !kept(offset, &self.caps) {
            // The command register's memory decoding, where this writes it.
            let decoding = usize::from(reg::COMMAND)
                .checked_sub(usize::from(offset))
                .and_then(|i| data.get(i))
                .map(|&b| u16::from(b) & reg::COMMAND_MEMORY != 0);
            let mut state = self.state();
            if decoding == Some(false) {
                self.map_windows(&mut state, false);
            }
            if let Err(e) = self.device.write(Space::Config, u64::from(offset), data) {
                crate::warn!("{}: its configuration at {offset:#x}: {e}", self.name());
            }
            if let Some(on) = decoding {
                state.decoding = on;
                if on {
                    self.map_windows(&mut state, true);
                }
            }
            return;
        }
        self.kept.write(offset, data);
        if self
            .caps
            .iter()
            .any(|&(at, len)| (at..at + len).contains(&offset))
        {
            self.sync(&mut self.state());
        }
    }
}

impl Drop for AssignedFunction {
    /// Its windows out of the guest before its device lets go of the ranges they map.
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        let mut state = std::mem::take(state);
        self.map_windows(&mut state, false);
    }
}

/// A memory BAR of an assigned function: each access the device's, but for MSI-X's table,
/// which the VMM keeps.
#[derive(Debug)]
pub struct AssignedBar {
    function: Arc<AssignedFunction>,
    index: usize,
}

impl AssignedBar {
    pub fn new(function: Arc<AssignedFunction>, index: usize) -> AssignedBar {
        AssignedBar { function, index }
    }
}

impl MmioDevice for AssignedBar {
    fn read(&self, offset: u64, data: &mut [u8]) {
        self.function.bar_read(self.index, offset, data);
    }

    fn write(&self, offset: u64, data: &[u8]) {
        self.function.bar_write(self.index, offset, data);
    }

    fn pause(&self) {}

    fn resume(&self) -> Result<(), String> {
        Ok(())
    }

    fn save(&self, _: &mut Writer) {}

    fn restore(&self, _: &mut Reader<'_>) -> codec::Result<()> {
        Ok(())
    }

    fn unsaved(&self) -> Option<String> {
        Some("an assigned device's state is its own".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::pci::BarKind;

    /// What the fake device saw, and what it holds: its configuration space and one BAR.
    #[derive(Debug)]
    struct Seen {
        config: Vec<u8>,
        bar: Box<Page>,
        power: Vec<u8>,
        mapped: Vec<(u32, u64, u32)>,
        unmapped: Vec<u32>,
        ranges: usize,
        unranged: usize,
    }

    /// A function of 8 KiB BAR 0, MSI (64-bit) at 0x50, MSI-X of 4 vectors at 0x60 (its
    /// table at 0x1000 of BAR 0, its pending bits at 0x1800), Power Management at 0x70.
    #[derive(Debug, Clone)]
    struct Fake(Arc<Mutex<Seen>>);

    const BAR: u64 = 0x2000;

    /// Its BAR, page-aligned, as a hypervisor maps one.
    #[derive(Debug)]
    #[repr(C, align(4096))]
    struct Page([u8; BAR as usize]);

    impl Fake {
        fn new() -> Fake {
            let mut config = vec![0u8; 4096];
            config[0..4].copy_from_slice(&0x1234_8086u32.to_le_bytes());
            config[0x0e] = 0x80;
            config[usize::from(reg::STATUS)] = reg::STATUS_CAPABILITIES as u8;
            config[usize::from(reg::CAPABILITIES)] = 0x50;
            config[0x50..0x54].copy_from_slice(&[reg::CAP_MSI, 0x60, 0x80, 0x00]);
            config[0x60..0x64].copy_from_slice(&[reg::CAP_MSIX, 0x70, 0x03, 0x00]);
            config[0x64..0x68].copy_from_slice(&0x1000u32.to_le_bytes());
            config[0x68..0x6c].copy_from_slice(&0x1800u32.to_le_bytes());
            config[0x70..0x72].copy_from_slice(&[passed::CAP_PM, 0x00]);
            config[0x3d] = 1;
            let mut bar = Box::new(Page([0u8; BAR as usize]));
            bar.0[0..4].copy_from_slice(&0xfeed_f00du32.to_le_bytes());
            // Every MSI-X vector masked from reset.
            for k in 0..4 {
                bar.0[0x1000 + 16 * k + 12] = 1;
            }
            Fake(Arc::new(Mutex::new(Seen {
                config,
                bar,
                power: Vec::new(),
                mapped: Vec::new(),
                unmapped: Vec::new(),
                ranges: 0,
                unranged: 0,
            })))
        }

        fn seen(&self) -> std::sync::MutexGuard<'_, Seen> {
            self.0.lock().unwrap()
        }

        fn config(&self, at: usize, len: usize) -> u64 {
            get_le(&self.seen().config[at..at + len])
        }

        fn bar(&self, at: usize, len: usize) -> u64 {
            get_le(&self.seen().bar.0[at..at + len])
        }
    }

    impl Device for Fake {
        fn name(&self) -> &str {
            "PCIP\\FAKE"
        }

        fn read(&self, space: Space, offset: u64, data: &mut [u8]) -> Result<(), String> {
            let seen = self.seen();
            let from: &[u8] = match space {
                Space::Config => &seen.config,
                Space::Bar(0) => &seen.bar.0,
                Space::Bar(n) => return Err(format!("BAR {n}")),
            };
            let at = offset as usize;
            data.copy_from_slice(&from[at..at + data.len()]);
            Ok(())
        }

        fn write(&self, space: Space, offset: u64, data: &[u8]) -> Result<(), String> {
            let mut seen = self.seen();
            let to: &mut [u8] = match space {
                Space::Config => &mut seen.config,
                Space::Bar(0) => &mut seen.bar.0,
                Space::Bar(n) => return Err(format!("BAR {n}")),
            };
            let at = offset as usize;
            to[at..at + data.len()].copy_from_slice(data);
            Ok(())
        }

        fn power(&self, state: u8) -> Result<(), String> {
            self.seen().power.push(state);
            Ok(())
        }

        fn map_ranges(&self) -> Result<Vec<Range>, String> {
            let mut seen = self.seen();
            seen.ranges += 1;
            Ok(vec![Range {
                bar: 0,
                offset: 0,
                len: BAR,
                host: seen.bar.0.as_ptr() as usize,
                read: true,
                write: true,
            }])
        }

        fn unmap_ranges(&self) -> Result<(), String> {
            self.seen().unranged += 1;
            Ok(())
        }

        fn map_interrupt(&self, index: u32, address: u64, data: u32) -> Result<(u64, u32), String> {
            self.seen().mapped.push((index, address, data));
            Ok((0xfee0_0000 | u64::from(index) << 12, 0x4000 | index))
        }

        fn unmap_interrupt(&self, index: u32) -> Result<(), String> {
            self.seen().unmapped.push(index);
            Ok(())
        }
    }

    /// The guest's windows: each mapped and unmapped, where and how long.
    #[derive(Debug, Clone, Default)]
    struct Guest(Arc<Mutex<Vec<(bool, u64, usize)>>>);

    impl Windows for Guest {
        unsafe fn map(&self, _: *mut u8, gpa: u64, len: usize, _: bool) -> Result<(), String> {
            self.0.lock().unwrap().push((true, gpa, len));
            Ok(())
        }

        fn unmap(&self, gpa: u64, len: usize) -> Result<(), String> {
            self.0.lock().unwrap().push((false, gpa, len));
            Ok(())
        }
    }

    const AT: u64 = 0x1000_0000;

    fn function() -> (AssignedFunction, Fake, Guest) {
        let (fake, guest) = (Fake::new(), Guest::default());
        let bars = passed::sized_bars([!(BAR as u32 - 1), 0, 0, 0, 0, 0])
            .into_iter()
            .map(|(index, size, kind)| Bar {
                index,
                addr: AT,
                size,
                kind,
            })
            .collect();
        let f = AssignedFunction::new(Box::new(fake.clone()), bars, Box::new(guest.clone()), 0x1000).unwrap();
        (f, fake, guest)
    }

    fn read(f: &AssignedFunction, offset: u16, len: usize) -> u64 {
        let mut b = vec![0u8; len];
        f.config_read(offset, &mut b);
        get_le(&b)
    }

    /// The guest reads the device's configuration but for what the VMM keeps: its BAR
    /// where the VMM placed it, no INTx pin, a single function; it starts in D0, its BAR
    /// mapped into this process.
    #[test]
    fn the_guest_reads_the_devices_configuration_but_what_the_vmm_keeps() {
        let (f, fake, _) = function();
        assert_eq!(read(&f, 0, 4), 0x1234_8086);
        assert_eq!(read(&f, reg::BAR0, 4), AT);
        assert_eq!(read(&f, reg::INTERRUPT_PIN, 1), 0, "WHP maps no INTx");
        assert_eq!(read(&f, reg::HEADER_TYPE, 1), 0, "a single function");
        assert_eq!(f.bars()[0].kind, BarKind::Memory32);
        assert_eq!(f.bars()[0].size, BAR);
        assert_eq!(fake.seen().power, vec![0]);
        assert_eq!(fake.seen().ranges, 1);
        // The guest's BAR register written: the device's is not.
        f.config_write(reg::BAR0, &u32::MAX.to_le_bytes());
        assert_eq!(read(&f, reg::BAR0, 4), u64::from(!(BAR as u32 - 1)));
        assert_eq!(fake.config(0x10, 4), 0);
    }

    /// MSI the guest enables is mapped by WHP as interrupt 0, and the device sends the
    /// message WHP returns, enabled; disabled, it is unmapped and off at the device.
    #[test]
    fn msi_is_mapped_and_the_device_given_whps_message() {
        let (f, fake, _) = function();
        f.config_write(0x54, &0xfee0_1000u32.to_le_bytes());
        f.config_write(0x58, &0u32.to_le_bytes());
        f.config_write(0x5c, &0x41u16.to_le_bytes());
        assert!(fake.seen().mapped.is_empty(), "not before it is enabled");
        f.config_write(0x52, &1u16.to_le_bytes());
        assert_eq!(fake.seen().mapped, vec![(0, 0xfee0_1000, 0x41)]);
        assert_eq!(fake.config(0x54, 4), 0xfee0_0000);
        assert_eq!(fake.config(0x58, 4), 0);
        assert_eq!(fake.config(0x5c, 2), 0x4000);
        assert_eq!(fake.config(0x52, 2) & 1, 1, "enabled at the device");
        f.config_write(0x52, &0u16.to_le_bytes());
        assert_eq!(fake.seen().unmapped, vec![0]);
        assert_eq!(fake.config(0x52, 2) & 1, 0);
    }

    /// MSI-X: a vector the guest unmasks with MSI-X on is mapped by WHP as interrupt 1 + its
    /// number, the device's entry given WHP's message and unmasked; masked again, it is
    /// masked at the device, its mapping kept; a new message is mapped anew, the old one
    /// let go of. Its table is the guest's, its pending bits the device's. Off, every
    /// vector is masked and unmapped and MSI-X off at the device.
    #[test]
    fn msix_vectors_are_mapped_as_the_guest_unmasks_them() {
        let (f, fake, _) = function();
        f.config_write(reg::COMMAND, &reg::COMMAND_MEMORY.to_le_bytes());
        f.config_write(0x62, &reg::MSIX_ENABLE.to_le_bytes());
        assert_eq!(
            fake.config(0x62, 2) as u16 & (reg::MSIX_ENABLE | reg::MSIX_MASK),
            reg::MSIX_ENABLE,
            "on, its function unmasked"
        );
        let bar = AssignedBar::new(Arc::new(f), 0);
        let entry = 0x1000 + 16 * 2;
        bar.write(entry, &0xfee0_2000u32.to_le_bytes());
        bar.write(entry + 4, &0u32.to_le_bytes());
        bar.write(entry + 8, &0x42u32.to_le_bytes());
        assert!(fake.seen().mapped.is_empty(), "masked yet");
        bar.write(entry + 12, &0u32.to_le_bytes());
        assert_eq!(fake.seen().mapped, vec![(3, 0xfee0_2000, 0x42)]);
        let device = entry as usize;
        assert_eq!(fake.bar(device, 4), 0xfee0_3000);
        assert_eq!(fake.bar(device + 8, 4), 0x4003);
        assert_eq!(fake.bar(device + 12, 4), 0, "unmasked at the device");
        let mut read = [0u8; 4];
        bar.read(entry, &mut read);
        assert_eq!(u32::from_le_bytes(read), 0xfee0_2000, "the guest's table");
        bar.read(0, &mut read);
        assert_eq!(u32::from_le_bytes(read), 0xfeed_f00d, "the device's register");
        bar.write(entry + 12, &1u32.to_le_bytes());
        assert_eq!(fake.bar(device + 12, 4), 1, "masked at the device");
        assert!(fake.seen().unmapped.is_empty(), "its mapping kept");
        bar.write(entry + 8, &0x43u32.to_le_bytes());
        bar.write(entry + 12, &0u32.to_le_bytes());
        assert_eq!(fake.seen().unmapped, vec![3]);
        assert_eq!(fake.seen().mapped.last(), Some(&(3, 0xfee0_2000, 0x43)));
        assert_eq!(fake.bar(device + 12, 4), 0);
        bar.function.config_write(0x62, &0u16.to_le_bytes());
        assert_eq!(fake.bar(device + 12, 4), 1, "masked as MSI-X goes off");
        assert_eq!(fake.seen().unmapped, vec![3, 3]);
        assert_eq!(fake.config(0x62, 2) as u16 & reg::MSIX_ENABLE, 0);
    }

    /// Its BAR in the guest while its memory decoding is on, but for the page of MSI-X's
    /// table and pending bits; off, out, and its accesses read all ones. Out of D0 its
    /// BAR's ranges are let go of, and taken again as it comes back.
    #[test]
    fn its_bar_reaches_the_guest_while_it_decodes_in_d0() {
        let (f, fake, guest) = function();
        let bar = AssignedBar::new(Arc::new(f), 0);
        let mut read = [0u8; 4];
        bar.read(0, &mut read);
        assert_eq!(read, [0xff; 4], "decoding off");
        bar.function
            .config_write(reg::COMMAND, &reg::COMMAND_MEMORY.to_le_bytes());
        assert_eq!(*guest.0.lock().unwrap(), vec![(true, AT, 0x1000)]);
        bar.read(0, &mut read);
        assert_eq!(u32::from_le_bytes(read), 0xfeed_f00d);
        // To D3hot through its control register: out of the guest, its ranges let go of.
        bar.function.config_write(0x74, &[3]);
        assert_eq!(guest.0.lock().unwrap().last(), Some(&(false, AT, 0x1000)));
        assert_eq!(fake.seen().power, vec![0, 3]);
        assert_eq!(fake.seen().unranged, 1);
        assert_eq!(read_config(&bar, 0x74) & 3, 3);
        bar.read(0, &mut read);
        assert_eq!(read, [0xff; 4], "out of D0");
        bar.function.config_write(0x74, &[0]);
        assert_eq!(fake.seen().power, vec![0, 3, 0]);
        assert_eq!(fake.seen().ranges, 2);
        assert_eq!(guest.0.lock().unwrap().last(), Some(&(true, AT, 0x1000)));
        bar.function.config_write(reg::COMMAND, &0u16.to_le_bytes());
        assert_eq!(guest.0.lock().unwrap().last(), Some(&(false, AT, 0x1000)));
        assert_eq!(
            fake.config(usize::from(reg::COMMAND), 2),
            0,
            "the device's command register"
        );
    }

    fn read_config(bar: &AssignedBar, offset: u16) -> u64 {
        read(&bar.function, offset, 1)
    }
}
