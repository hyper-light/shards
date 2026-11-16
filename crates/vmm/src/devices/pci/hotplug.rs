//! Functions plugged into a running machine's PCI bus, and taken off it (hot-plug).
//!
//! A plugged function takes a free slot of bus 0, one the machine kept for it as it booted,
//! with the MSIs of that place's requester ID; its BARs are where the guest places them in
//! the bus's 32-bit window, as Linux assigns a function it finds by a rescan addresses of
//! its own (drivers/pci/setup-bus.c, pci_assign_unassigned_bus_resources). The guest finds
//! it as its init writes `/sys/bus/pci/rescan`, and lets it go as its init writes the
//! function's `remove`: no hot-plug controller is emulated, as Linux's native one waits
//! 120 ms for each function it adds (pciehp's link wait, pcie_wait_for_link_delay) and its
//! attention button 5 s (pciehp_ctrl.c, pciehp_handle_button_press), and ACPI's is x86's
//! alone (D214).
//!
//! Where the machine's slots sit behind its IOMMU, as a microVM booted to take a host's
//! functions has them (D214), a slot's endpoint is translated from the plug to the unplug
//! of what is plugged there (`iommu::Translation::plugged`).

use shards_ipc::device::Refusal;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use super::{Ecam, Msi, PciFunction};
use crate::devices::{MmioBus, MmioDevice};
use crate::snapshot::codec::{self, Reader, Writer};

/// A function plugged into a running machine: its configuration space, and its BARs as the
/// guest placed them.
pub trait Plugged: PciFunction {
    /// The BAR an access of `len` bytes at guest address `addr` reaches, and its offset
    /// there, where the function decodes memory and one of its BARs holds the access.
    fn claims(&self, addr: u64, len: u64) -> Option<(usize, u64)>;
    fn bar_read(&self, bar: usize, offset: u64, data: &mut [u8]);
    fn bar_write(&self, bar: usize, offset: u64, data: &[u8]);
    /// Why it may not be taken off the bus yet, where it may not: its guest's driver still
    /// drives it.
    fn held(&self) -> Option<String>;
    /// Stops what it runs at a request's boundary, as a machine's devices pause.
    fn pause(&self);
    fn resume(&self) -> Result<(), String>;
    /// Stops what it runs for good, once it is off the bus.
    fn unplugged(&self);
    /// Its state for a snapshot (D270 slice 5): what its guest set of it and its device's own.
    /// Saved paused. A function no snapshot can bring back (a host's, a disk's, a share's) is
    /// never asked: its machine refuses to save while it is plugged ([`Window::saves`]).
    fn save(&self, _: &mut Writer) {}
    /// Puts back what [`save`](Self::save) kept, the function plugged back where it was first.
    fn restore(&self, _: &mut Reader<'_>) -> codec::Result<()> {
        Ok(())
    }
}

/// The bus's 32-bit window, which plugged functions' BARs go in wherever the guest places
/// them: the BARs the VMM placed as the machine was made (`fixed`), and the plugged
/// functions', each reached as its function decodes it. An access no BAR claims reads as
/// all ones, as one no function claims completes (PCI Express Base 6.0 §2.3.2).
pub struct Window {
    base: u64,
    fixed: MmioBus,
    plugged: RwLock<Vec<(u8, Arc<dyn Plugged>)>>,
    /// The places of the plugged functions a snapshot keeps (D270 slice 5): its components',
    /// which a restore plugs back from their images before their state is put back.
    saved: RwLock<std::collections::BTreeSet<u8>>,
}

impl std::fmt::Debug for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Window")
            .field("base", &self.base)
            .field("fixed", &self.fixed)
            .finish_non_exhaustive()
    }
}

impl Window {
    /// The window at `base`, with the BARs placed in it, `fixed`, at their guest addresses.
    pub fn new(base: u64, fixed: MmioBus) -> Window {
        Window {
            base,
            fixed,
            plugged: RwLock::new(Vec::new()),
            saved: RwLock::new(std::collections::BTreeSet::new()),
        }
    }

    /// The function at `devfn` a snapshot keeps, as long as it is plugged (D270 slice 5): a
    /// component's, which a restore plugs back.
    pub fn saves(&self, devfn: u8) {
        self.saved
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(devfn);
    }

    /// The plugged function an access reaches, its BAR and the offset there.
    fn find(&self, addr: u64, len: usize) -> Option<(Arc<dyn Plugged>, usize, u64)> {
        let plugged = self.plugged.read().unwrap_or_else(PoisonError::into_inner);
        plugged.iter().find_map(|(_, f)| {
            let (bar, offset) = f.claims(addr, len as u64)?;
            Some((f.clone(), bar, offset))
        })
    }

    fn functions(&self) -> Vec<Arc<dyn Plugged>> {
        let plugged = self.plugged.read().unwrap_or_else(PoisonError::into_inner);
        plugged.iter().map(|(_, f)| f.clone()).collect()
    }
}

impl MmioDevice for Window {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let Some(addr) = self.base.checked_add(offset) else {
            data.fill(0xff);
            return;
        };
        if self.fixed.read(addr, data) {
            return;
        }
        match self.find(addr, data.len()) {
            Some((f, bar, at)) => f.bar_read(bar, at, data),
            None => data.fill(0xff),
        }
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let Some(addr) = self.base.checked_add(offset) else {
            return;
        };
        if self.fixed.write(addr, data) {
            return;
        }
        if let Some((f, bar, at)) = self.find(addr, data.len()) {
            f.bar_write(bar, at, data);
        }
    }

    fn pause(&self) {
        self.fixed.pause();
        self.functions().iter().for_each(|f| f.pause());
    }

    fn resume(&self) -> Result<(), String> {
        self.fixed.resume()?;
        self.functions().iter().try_for_each(|f| f.resume())
    }

    /// Each plugged function's state, by its place (D270 slice 5): only those it keeps are
    /// plugged when it saves ([`unsaved`](MmioDevice::unsaved)).
    fn save(&self, w: &mut Writer) {
        let mut plugged: Vec<(u8, Arc<dyn Plugged>)> = self
            .plugged
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        plugged.sort_by_key(|(devfn, _)| *devfn);
        w.seq(&plugged, |w, (devfn, f)| {
            let mut own = Writer::default();
            f.save(&mut own);
            w.u8(*devfn);
            w.bytes(&own.into_bytes());
        });
    }

    /// Puts its state back on each function plugged back, by its place: refused unless each
    /// one plugged back has a state kept, and every state kept has one plugged back, which
    /// reads it whole.
    fn restore(&self, r: &mut Reader<'_>) -> codec::Result<()> {
        let plugged: Vec<(u8, Arc<dyn Plugged>)> = self
            .plugged
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        // At most a state for each function plugged back: its place, then its length.
        let kept = r.seq(plugged.len(), 1 + 4, |r| Ok((r.u8()?, r.bytes(usize::MAX)?)))?;
        for (devfn, f) in &plugged {
            let state = kept
                .iter()
                .find(|(at, _)| at == devfn)
                .map(|(_, state)| *state)
                .ok_or_else(|| {
                    codec::DecodeError(format!(
                        "the snapshot keeps no state of the function plugged back at {devfn:#04x}"
                    ))
                })?;
            let mut own = Reader::new(state);
            f.restore(&mut own)?;
            own.finish()?;
        }
        Ok(())
    }

    fn unsaved(&self) -> Option<String> {
        self.fixed.unsaved().or_else(|| {
            let saved = self.saved.read().unwrap_or_else(PoisonError::into_inner);
            let plugged = self.plugged.read().unwrap_or_else(PoisonError::into_inner);
            plugged.iter().any(|(devfn, _)| !saved.contains(devfn)).then(|| {
                "it has PCI functions plugged into it that no snapshot brings back (a disk's, a share's, a host's)"
                    .to_string()
            })
        })
    }
}

/// A free place on bus 0 for a function to be plugged into: its device and function
/// (`devfn`), and where the MSIs of its requester ID go.
#[derive(Debug, Clone)]
pub struct Slot {
    pub devfn: u8,
    pub msi: Arc<dyn Msi>,
    /// How a function passed through there interrupts its guest, by irqfds.
    #[cfg(target_os = "linux")]
    pub route: Option<Arc<dyn super::vfio::Route>>,
    /// The machine's IOMMU, where the slot sits behind it (D214): its requester ID an
    /// endpoint of the IOMMU's while a function is plugged there.
    pub iommu: Option<crate::devices::virtio::iommu::Shared>,
}

impl Slot {
    /// Its requester ID: the endpoint its IOMMU knows it by.
    pub fn endpoint(&self) -> u32 {
        u32::from(self.devfn)
    }

    /// Where the MSIs of a function the VMM emulates there go: through its IOMMU's
    /// translation, where it sits behind one (`iommu::Msis`), else as they are.
    pub fn emulated_msi(&self) -> Arc<dyn Msi> {
        match &self.iommu {
            Some(t) => Arc::new(crate::devices::virtio::iommu::Msis::new(
                t.clone(),
                self.endpoint(),
                self.msi.clone(),
            )) as Arc<dyn Msi>,
            None => self.msi.clone(),
        }
    }

    /// Its endpoint translated by its IOMMU, where it sits behind one: a function is
    /// plugged there.
    fn occupy(&self) -> Result<(), String> {
        match &self.iommu {
            Some(t) => t
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .plugged(self.endpoint())
                .map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }

    /// Its endpoint vacant again, its domain and what followed it let go of.
    fn vacate(&self) {
        if let Some(t) = &self.iommu {
            t.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .unplugged(self.endpoint());
        }
    }
}

/// A running machine's hot-plug: the bus's configuration space, its 32-bit window, and the
/// slots the machine kept for functions to come.
pub struct Hotplug {
    ecam: Arc<Ecam>,
    window: Arc<Window>,
    slots: Vec<Slot>,
    /// Held across a plug or an unplug, so that two never take one slot.
    busy: Mutex<()>,
}

impl std::fmt::Debug for Hotplug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hotplug")
            .field("slots", &self.slots.iter().map(|s| s.devfn).collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Hotplug {
    pub fn new(ecam: Arc<Ecam>, window: Arc<Window>, slots: Vec<Slot>) -> Hotplug {
        Hotplug {
            ecam,
            window,
            slots,
            busy: Mutex::new(()),
        }
    }

    /// Plugs the function `make` makes, given the first free slot, into it: its BARs
    /// reached first, then its configuration space, which the guest finds it by. Returns
    /// its `devfn`.
    pub fn plug(&self, make: impl FnOnce(&Slot) -> Result<Arc<dyn Plugged>, String>) -> Result<u8, Refusal> {
        let _busy = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
        let taken: Vec<u8> = {
            let plugged = self.window.plugged.read().unwrap_or_else(PoisonError::into_inner);
            plugged.iter().map(|(devfn, _)| *devfn).collect()
        };
        let slot = self
            .slots
            .iter()
            .find(|s| !taken.contains(&s.devfn))
            .ok_or(Refusal::NoSlots {
                slots: u64::try_from(self.slots.len()).unwrap_or(u64::MAX),
            })?;
        self.plug_in(slot, make)
    }

    /// Plugs what `make` makes into `slot`, free: its BARs reached first, then its
    /// configuration space.
    fn plug_in(
        &self,
        slot: &Slot,
        make: impl FnOnce(&Slot) -> Result<Arc<dyn Plugged>, String>,
    ) -> Result<u8, Refusal> {
        slot.occupy().map_err(Refusal::Host)?;
        let function = match make(slot) {
            Ok(function) => function,
            Err(e) => {
                slot.vacate();
                return Err(Refusal::Host(e));
            }
        };
        self.window
            .plugged
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push((slot.devfn, function.clone()));
        if let Err(e) = self.ecam.plug(slot.devfn, function.clone()) {
            self.take(slot.devfn);
            function.unplugged();
            slot.vacate();
            return Err(Refusal::Host(e));
        }
        Ok(slot.devfn)
    }

    /// [`plug`](Self::plug) at the slot of `devfn`, as a restore plugs a component back where it
    /// was (D270 slice 5): refused where the machine kept no slot there or one is taken.
    pub fn plug_at(
        &self,
        devfn: u8,
        make: impl FnOnce(&Slot) -> Result<Arc<dyn Plugged>, String>,
    ) -> Result<u8, Refusal> {
        let _busy = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
        if self
            .window
            .plugged
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|(at, _)| *at == devfn)
        {
            return Err(Refusal::Host(format!("its slot {devfn:#04x} is taken")));
        }
        let slot = self
            .slots
            .iter()
            .find(|s| s.devfn == devfn)
            .ok_or_else(|| Refusal::Host(format!("this machine kept no slot at {devfn:#04x}")))?;
        self.plug_in(slot, make)
    }

    /// The function at `devfn` a snapshot keeps ([`Window::saves`]).
    pub fn saves(&self, devfn: u8) {
        self.window.saves(devfn);
    }

    /// Takes the function at `devfn` off the bus, once its guest has let it go: an error,
    /// changing nothing, where none is plugged there or its driver still drives it.
    pub fn unplug(&self, devfn: u8) -> Result<(), Refusal> {
        let _busy = self.busy.lock().unwrap_or_else(PoisonError::into_inner);
        let function = {
            let plugged = self.window.plugged.read().unwrap_or_else(PoisonError::into_inner);
            plugged
                .iter()
                .find(|(f, _)| *f == devfn)
                .map(|(_, f)| f.clone())
                .ok_or(Refusal::NoDevice { devfn })?
        };
        if let Some(reason) = function.held() {
            return Err(Refusal::StillDriven { devfn, reason });
        }
        self.ecam.unplug(devfn);
        self.take(devfn);
        function.unplugged();
        if let Some(slot) = self.slots.iter().find(|s| s.devfn == devfn) {
            slot.vacate();
        }
        Ok(())
    }

    /// Whether its slots sit behind the machine's IOMMU (D214).
    pub fn slots_behind_iommu(&self) -> bool {
        self.slots.iter().any(|s| s.iommu.is_some())
    }

    /// The places of the functions plugged, in the order they were.
    pub fn plugged(&self) -> Vec<u8> {
        let plugged = self.window.plugged.read().unwrap_or_else(PoisonError::into_inner);
        plugged.iter().map(|(devfn, _)| *devfn).collect()
    }

    /// The function plugged at `devfn`, where one is.
    pub fn function(&self, devfn: u8) -> Option<Arc<dyn Plugged>> {
        let plugged = self.window.plugged.read().unwrap_or_else(PoisonError::into_inner);
        plugged.iter().find(|(f, _)| *f == devfn).map(|(_, f)| f.clone())
    }

    fn take(&self, devfn: u8) {
        self.window
            .plugged
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(f, _)| *f != devfn);
        self.window
            .saved
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&devfn);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A function of one BAR of a page at `at`, held while `held`.
    struct Fake {
        at: u64,
        held: AtomicBool,
        gone: AtomicBool,
    }

    impl PciFunction for Fake {
        fn config_read(&self, _: u16, data: &mut [u8]) {
            data.fill(0x42);
        }
        fn config_write(&self, _: u16, _: &[u8]) {}
    }

    impl Plugged for Fake {
        fn claims(&self, addr: u64, len: u64) -> Option<(usize, u64)> {
            let offset = addr.checked_sub(self.at)?;
            (offset + len <= 0x1000).then_some((0, offset))
        }
        fn bar_read(&self, _: usize, offset: u64, data: &mut [u8]) {
            data.fill(offset as u8);
        }
        fn bar_write(&self, _: usize, _: u64, _: &[u8]) {}
        fn held(&self) -> Option<String> {
            self.held.load(Ordering::Acquire).then(|| "driven".to_string())
        }
        fn pause(&self) {}
        fn resume(&self) -> Result<(), String> {
            Ok(())
        }
        fn unplugged(&self) {
            self.gone.store(true, Ordering::Release);
        }
    }

    struct NoMsi;

    impl Msi for NoMsi {
        fn send(&self, _: u64, _: u32) {}
    }

    fn read(d: &dyn MmioDevice, offset: u64) -> u32 {
        let mut b = [0u8; 4];
        d.read(offset, &mut b);
        u32::from_le_bytes(b)
    }

    /// D270 slice 5: a machine whose plugged functions are each one a snapshot keeps saves,
    /// each one's state by its place, and one with a function it keeps not refuses to. A restore
    /// puts each state back on the function plugged back at its place (`plug_at`), refused for
    /// a state whose place has none, or a function plugged back whose state it keeps none of;
    /// `plug_at` refuses a place taken or no slot's.
    #[test]
    fn a_window_saves_the_functions_it_keeps_by_their_places_and_puts_them_back() {
        struct Stateful(std::sync::Mutex<u64>);
        impl PciFunction for Stateful {
            fn config_read(&self, _: u16, data: &mut [u8]) {
                data.fill(0);
            }
            fn config_write(&self, _: u16, _: &[u8]) {}
        }
        impl Plugged for Stateful {
            fn claims(&self, _: u64, _: u64) -> Option<(usize, u64)> {
                None
            }
            fn bar_read(&self, _: usize, _: u64, data: &mut [u8]) {
                data.fill(0);
            }
            fn bar_write(&self, _: usize, _: u64, _: &[u8]) {}
            fn held(&self) -> Option<String> {
                None
            }
            fn pause(&self) {}
            fn resume(&self) -> Result<(), String> {
                Ok(())
            }
            fn unplugged(&self) {}
            fn save(&self, w: &mut Writer) {
                w.u64(*self.0.lock().unwrap());
            }
            fn restore(&self, r: &mut Reader<'_>) -> codec::Result<()> {
                *self.0.lock().unwrap() = r.u64()?;
                Ok(())
            }
        }
        let stateful = |v: u64| Arc::new(Stateful(std::sync::Mutex::new(v)));
        let machine = || {
            let window = Arc::new(Window::new(0x1000_0000, MmioBus::default()));
            let slots = [2u8, 3]
                .map(|d| Slot {
                    devfn: d << 3,
                    msi: Arc::new(NoMsi),
                    #[cfg(target_os = "linux")]
                    route: None,
                    iommu: None,
                })
                .to_vec();
            let hotplug = Hotplug::new(Arc::new(Ecam::new(Vec::new())), window.clone(), slots);
            (window, hotplug)
        };
        let (window, hotplug) = machine();
        let at = hotplug.plug(|_| Ok(stateful(7) as Arc<dyn Plugged>)).unwrap();
        assert!(window.unsaved().is_some(), "a function no snapshot keeps");
        hotplug.saves(at);
        assert!(window.unsaved().is_none(), "every plugged function one it keeps");
        let mut w = Writer::default();
        window.save(&mut w);
        let saved = w.into_bytes();
        let (back, again) = machine();
        let put = stateful(0);
        let p = put.clone();
        assert_eq!(again.plug_at(at, move |_| Ok(p as Arc<dyn Plugged>)).unwrap(), at);
        back.restore(&mut Reader::new(&saved)).unwrap();
        assert_eq!(*put.0.lock().unwrap(), 7, "its state put back on it");
        let (empty, _) = machine();
        assert!(
            empty.restore(&mut Reader::new(&saved)).is_err(),
            "no function plugged back at its place"
        );
        let (more, plugs) = machine();
        for d in [at, 3 << 3] {
            plugs.plug_at(d, |_| Ok(stateful(0) as Arc<dyn Plugged>)).unwrap();
        }
        assert!(
            more.restore(&mut Reader::new(&saved)).is_err(),
            "a function plugged back whose state it keeps none of"
        );
        assert!(
            again
                .plug_at(at, |_| Ok(stateful(0) as Arc<dyn Plugged>))
                .is_err(),
            "its place taken"
        );
        assert!(
            again
                .plug_at(5 << 3, |_| Ok(stateful(0) as Arc<dyn Plugged>))
                .is_err(),
            "no slot kept there"
        );
        // Kept as the runtime keeps a component it plugs back; taken off, kept no more.
        again.saves(at);
        assert!(back.saved.read().unwrap().contains(&at));
        again.unplug(at).unwrap();
        assert!(back.saved.read().unwrap().is_empty());
    }

    /// A function plugged takes the first free slot, its configuration space on the bus and
    /// its BAR in the window where it placed it, beside the window's fixed BARs; the window
    /// reads all ones where nothing is. It is not taken off while held, and once taken off
    /// its place reads as no function's and is free again.
    #[test]
    fn a_plugged_function_takes_a_free_slot_and_leaves_once_let_go() {
        let base = 0x1000_0000;
        let mut fixed = MmioBus::default();
        struct Fixed;
        impl MmioDevice for Fixed {
            fn read(&self, _: u64, data: &mut [u8]) {
                data.fill(7);
            }
            fn write(&self, _: u64, _: &[u8]) {}
            fn pause(&self) {}
            fn resume(&self) -> Result<(), String> {
                Ok(())
            }
            fn save(&self, _: &mut Writer) {}
            fn restore(&self, _: &mut Reader<'_>) -> codec::Result<()> {
                Ok(())
            }
        }
        fixed.insert(base, 0x1000, Arc::new(Fixed)).unwrap();
        let window = Arc::new(Window::new(base, fixed));
        let ecam = Arc::new(Ecam::new(Vec::new()));
        let slots = [2u8, 3]
            .map(|d| Slot {
                devfn: d << 3,
                msi: Arc::new(NoMsi),
                #[cfg(target_os = "linux")]
                route: None,
                iommu: None,
            })
            .to_vec();
        let hotplug = Hotplug::new(ecam.clone(), window.clone(), slots);
        let fake = Arc::new(Fake {
            at: base + 0x4000,
            held: AtomicBool::new(true),
            gone: AtomicBool::new(false),
        });
        let f = fake.clone();
        let devfn = hotplug.plug(move |_| Ok(f as Arc<dyn Plugged>)).unwrap();
        assert_eq!(devfn, 2 << 3);
        assert_eq!(read(&*ecam, 2 << 15), 0x4242_4242, "on the bus");
        assert_eq!(read(&*window, 0), 0x0707_0707, "a fixed BAR");
        assert_eq!(read(&*window, 0x4010), 0x1010_1010, "its BAR, where placed");
        assert_eq!(read(&*window, 0x8000), u32::MAX, "nothing's");
        assert!(window.unsaved().is_some());
        assert_eq!(
            hotplug.unplug(devfn).unwrap_err(),
            Refusal::StillDriven {
                devfn,
                reason: "driven".into()
            }
        );
        assert_eq!(hotplug.plugged(), [devfn]);
        fake.held.store(false, Ordering::Release);
        hotplug.unplug(devfn).unwrap();
        assert!(fake.gone.load(Ordering::Acquire));
        assert_eq!(read(&*ecam, 2 << 15), u32::MAX, "off the bus");
        assert_eq!(read(&*window, 0x4010), u32::MAX);
        assert_eq!(hotplug.unplug(devfn).unwrap_err(), Refusal::NoDevice { devfn });
        assert!(window.unsaved().is_none());
        // Both slots taken, a third is refused.
        for _ in 0..2 {
            let f = Arc::new(Fake {
                at: 0,
                held: AtomicBool::new(false),
                gone: AtomicBool::new(false),
            });
            hotplug.plug(move |_| Ok(f as Arc<dyn Plugged>)).unwrap();
        }
        assert_eq!(hotplug.plugged(), [2 << 3, 3 << 3]);
        let f = Arc::new(Fake {
            at: 0,
            held: AtomicBool::new(false),
            gone: AtomicBool::new(false),
        });
        assert_eq!(
            hotplug.plug(move |_| Ok(f as Arc<dyn Plugged>)).unwrap_err(),
            Refusal::NoSlots { slots: 2 }
        );
    }

    /// A slot behind the machine's IOMMU (D214): its endpoint translated from the plug of
    /// a function to its unplug, and vacant again where the function could not be made.
    #[test]
    fn a_slot_behind_the_iommu_is_its_endpoint_while_plugged() {
        use crate::devices::virtio::iommu::{Shared, Translation};
        let t: Shared = Arc::new(Mutex::new(
            Translation::new(Vec::new(), None).with_slots(&[2 << 3]),
        ));
        let slots = vec![Slot {
            devfn: 2 << 3,
            msi: Arc::new(NoMsi),
            #[cfg(target_os = "linux")]
            route: None,
            iommu: Some(t.clone()),
        }];
        let window = Arc::new(Window::new(0, MmioBus::default()));
        let hotplug = Hotplug::new(Arc::new(Ecam::new(Vec::new())), window, slots);
        let translates = || t.lock().unwrap().translates(2 << 3);
        assert!(!translates(), "vacant");
        assert_eq!(
            hotplug.plug(|_| Err("no function".to_string())).unwrap_err(),
            Refusal::Host("no function".into())
        );
        assert!(!translates(), "vacant again");
        let f = Arc::new(Fake {
            at: 0,
            held: AtomicBool::new(false),
            gone: AtomicBool::new(false),
        });
        let devfn = hotplug.plug(move |_| Ok(f as Arc<dyn Plugged>)).unwrap();
        assert!(translates(), "plugged");
        hotplug.unplug(devfn).unwrap();
        assert!(!translates(), "unplugged");
    }
}
