//! virtio-iommu (virtio 1.3 §5.13; include/uapi/linux/virtio_iommu.h): the IOMMU of the
//! functions the VMM emulates on a machine's PCI bus (D154), between their DMA and guest
//! memory as a host's IOMMU stands between a device's and its memory. The guest's driver
//! attaches each endpoint, a function's requester ID, to a domain, and maps the domain's
//! I/O virtual addresses to guest-physical ones; a function's DMA reaches guest memory at
//! what its endpoint's domain maps ([`Translation::translate`]), and nothing else. An
//! endpoint attached to no domain bypasses translation until the driver says otherwise
//! (the configuration's `bypass`, 1 from reset, as QEMU's `boot-bypass`), so a guest with
//! no driver for the device keeps DMA by guest-physical address. On x86_64 the local
//! APICs' MSI window is a reserved region of subtype MSI (PROBE), so MSIs need no mapping,
//! as virtio 1.3 §5.13.3 asks there. On arm64 none is: Linux's MSI parents, GICv2m's and
//! the ITS's, have the IOMMU map their doorbell for the device's domain
//! (drivers/iommu/iommu.c, iommu_dma_prepare_msi), which VFIO's domains do only with the
//! software MSI window Linux's driver adds where the device reports no MSI region
//! (virtio-iommu.c, viommu_get_resv_regions); a function's MSIs are then writes to an I/O
//! address, translated as its DMA is.
//!
//! A function passed through (D154) can sit behind it too: its DMA is the host IOMMU's to
//! translate, by its group's container, which a [`Follower`] keeps mapping what the
//! function's endpoint sees here, as Cloud Hypervisor's external DMA mappings follow its
//! virtio-iommu (virtio-devices/src/iommu.rs); and where the endpoint bypasses
//! translation, all of guest RAM, as virtio 1.3 §5.13 has a bypassing endpoint's DMA go
//! untranslated.
//!
//! A machine's hot-plug slots can sit behind it too (D214): their requester IDs are
//! endpoints its firmware names from boot (the devicetree's `iommu-map`, ACPI's VIOT), as
//! a guest's IOMMU core finds a function's IOMMU by its firmware as the function is added,
//! but each is one it translates only while a function is plugged there
//! ([`Translation::plugged`]); vacant, it is no endpoint of its (S_NOENT), as one whose
//! device is gone. A function the VMM emulates there (a disk, a shared directory) has its
//! MSIs land where its endpoint's domain maps their address ([`Msis`]), as Linux has an
//! arm64 function's doorbell mapped in its domain whatever its driver's DMA does.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use super::queue::{Chain, Queue};
use super::worker::Worker;
use super::{Activation, DeviceInterrupt, VirtioDevice, feature};
use crate::memory::{Access, GuestMemory};

pub const DEVICE_ID: u32 = 23;
/// The request ring.
const QUEUE_SIZE: u16 = 64;
/// The event ring: the device reports no faults, so its driver's event buffers are never
/// written, and the least ring holds them; Linux's driver posts one for each of its free
/// entries (drivers/iommu/virtio-iommu.c l. 1131-1138, `viommu_fill_evtq`; Linux
/// 1590cf03), which a longer ring would only have it allocate more of.
const EVENT_QUEUE_SIZE: u16 = 1;
/// Features: the input and domain ranges in the configuration, MAP and UNMAP, PROBE,
/// mappings of device memory, and the configuration's bypass. Linux maps an MSI doorbell
/// as device memory (IOMMU_MMIO), and its driver refuses a mapping of a flag the device
/// does not offer (virtio-iommu.c, viommu_map_pages), so arm64's MSIs need F_MMIO.
const F_INPUT_RANGE: u64 = 1 << 0;
const F_DOMAIN_RANGE: u64 = 1 << 1;
const F_MAP_UNMAP: u64 = 1 << 2;
const F_PROBE: u64 = 1 << 4;
const F_MMIO: u64 = 1 << 5;
const F_BYPASS_CONFIG: u64 = 1 << 6;
/// Requests, statuses and flags (virtio_iommu.h).
const T_ATTACH: u8 = 1;
const T_DETACH: u8 = 2;
const T_MAP: u8 = 3;
const T_UNMAP: u8 = 4;
const T_PROBE: u8 = 5;
const S_OK: u8 = 0;
const S_UNSUPP: u8 = 2;
const S_DEVERR: u8 = 3;
const S_INVAL: u8 = 4;
const S_RANGE: u8 = 5;
const S_NOENT: u8 = 6;
const S_NOMEM: u8 = 8;
const ATTACH_F_BYPASS: u32 = 1 << 0;
pub(crate) const MAP_F_READ: u32 = 1 << 0;
pub(crate) const MAP_F_WRITE: u32 = 1 << 1;
const MAP_F_MMIO: u32 = 1 << 2;
const PROBE_T_RESV_MEM: u16 = 1;
const RESV_MEM_T_MSI: u8 = 1;
/// A reserved region's property (struct virtio_iommu_probe_resv_mem): its head (type and
/// the length after it, 2 bytes each), subtype, three reserved bytes, and the region's
/// first and last address (virtio 1.3 §5.13.6; include/uapi/linux/virtio_iommu.h
/// l. 123-137, Linux 1590cf03).
const RESV_MEM_BYTES: u16 = 2 + 2 + 1 + 3 + 8 + 8;
/// The buffer PROBE writes an endpoint's properties into: the one property the device
/// gives, the MSI window as a reserved region, which fills it. Linux reads properties until
/// a NONE or the buffer's end (drivers/iommu/virtio-iommu.c l. 565-582); one without the
/// window reads a NONE at once, its bytes all zeros.
const PROBE_SIZE: u32 = RESV_MEM_BYTES as u32;
/// The granule of a mapping: its addresses and length are whole 4 KiB pages.
const PAGE: u64 = 1 << 12;
/// Each request's device-readable part, as virtio 1.3 §5.13.6 lays it out
/// (include/uapi/linux/virtio_iommu.h l. 62-150): its 4-byte head, then ATTACH's domain,
/// endpoint, flags and 4 reserved bytes; DETACH's domain, endpoint and 8 reserved; MAP's
/// domain, start, end, physical start and flags; UNMAP's domain, start, end and 4 reserved;
/// PROBE's endpoint and 64 reserved, before the properties the device writes.
const ATTACH_READABLE: usize = 4 + 4 + 4 + 4 + 4;
const DETACH_READABLE: usize = 4 + 4 + 4 + 8;
const MAP_READABLE: usize = 4 + 4 + 8 + 8 + 8 + 4;
const UNMAP_READABLE: usize = 4 + 4 + 8 + 8 + 4;
const PROBE_READABLE: usize = 4 + 4 + 64;
/// The longest of them: PROBE's, 72.
const READABLE_MAX: usize = longer(
    longer(
        longer(ATTACH_READABLE, DETACH_READABLE),
        longer(MAP_READABLE, UNMAP_READABLE),
    ),
    PROBE_READABLE,
);

const fn longer(a: usize, b: usize) -> usize {
    if a > b { a } else { b }
}

/// A translation as the device and its endpoints' DMA share it.
pub type Shared = Arc<Mutex<Translation>>;

/// What the guest's driver set up: each endpoint's domain, each domain's mappings, and
/// whether an endpoint attached to none bypasses translation.
#[derive(Debug)]
pub struct Translation {
    /// The endpoints the IOMMU translates: the functions' requester IDs, its slots'
    /// among them.
    known: Vec<u32>,
    /// The endpoints of the machine's hot-plug slots (D214), which its firmware names from
    /// boot; and of them, those that hold no function, translated by none
    /// ([`Translation::translates`]).
    slots: Vec<u32>,
    vacant: Vec<u32>,
    attached: BTreeMap<u32, u32>,
    domains: BTreeMap<u32, Domain>,
    bypass: bool,
    /// Whether it offers bypass at all: the configuration's `bypass` and bypass domains
    /// (VIRTIO_IOMMU_F_BYPASS_CONFIG).
    offers_bypass: bool,
    /// The MSI window, its first and last address, never translated (a reserved region of
    /// subtype MSI, Linux's IOMMU_RESV_MSI), where the machine has one: x86_64's.
    msi: Option<(u64, u64)>,
    /// What follows the views of the endpoints whose DMA the host translates: each
    /// follower, the endpoints that share it, and the one whose view it shows.
    followers: Vec<Follows>,
}

/// A follower, the endpoints that share it, and the one of them whose view it shows.
/// Endpoints that a host's IOMMU translates as one, the functions of one IOMMU group,
/// share one: a type 1 container maps one set of I/O addresses for all of its groups'
/// devices (Documentation/driver-api/vfio.rst), and the host could not keep them from
/// each other's DMA anyway, which is why they are one group. The guest is shown them as
/// one group too (the machine's bus puts them in one device, `vm::vfio_places`), whose
/// endpoints its IOMMU core attaches to a domain together, one after another
/// (iommu.c, __iommu_group_set_domain): the follower shows the view of the last of them
/// attached or detached. A guest that kept them in domains apart would have the last
/// one's view for all of them: of its own RAM still, all that a container is given.
#[derive(Debug)]
struct Follows {
    endpoints: Vec<u32>,
    follower: Arc<dyn Follower>,
    shows: u32,
}

/// What follows an endpoint's view of guest memory where the endpoint's DMA is not the
/// VMM's to translate: a function passed through, whose DMA the host's IOMMU translates by
/// its group's container (D154). It is shown the view as it changes: all of guest RAM at
/// the addresses the guest knows it by where the endpoint bypasses translation, nothing
/// where it is blocked, and its domain's mappings of guest RAM otherwise.
pub trait Follower: Send + Sync + std::fmt::Debug {
    /// All of guest RAM at its own addresses, in place of whatever was mapped.
    fn identity(&self) -> io::Result<()>;
    /// Nothing mapped.
    fn clear(&self) -> io::Result<()>;
    /// `len` bytes of guest RAM at `gpa`, mapped at `iova`, for the device to read where
    /// `read` and to write where `write`.
    fn map(&self, iova: u64, gpa: u64, len: u64, read: bool, write: bool) -> io::Result<()>;
    /// The mapping of `len` bytes at `iova`, gone.
    fn unmap(&self, iova: u64, len: u64) -> io::Result<()>;
}

/// What an endpoint sees of guest memory.
enum View<'a> {
    /// All of it, untranslated.
    Identity,
    /// Nothing.
    Blocked,
    /// What its domain maps.
    Mapped(&'a BTreeMap<u64, (u64, u64, u32)>),
}

/// Shows `follower` `view`, in place of what it showed.
fn show(follower: &dyn Follower, view: View<'_>) -> io::Result<()> {
    match view {
        View::Identity => follower.identity(),
        View::Blocked => follower.clear(),
        View::Mapped(mappings) => {
            follower.clear()?;
            for (&first, &(last, phys, flags)) in mappings {
                mirror(follower, first, last, phys, flags)?;
            }
            Ok(())
        }
    }
}

/// A domain's mapping as its endpoints' followers take it: of guest RAM alone, which the
/// device reaches. One of device memory is an MSI doorbell's, whose writes the host's own
/// interrupt remapping takes (VFIO's), and one of no access maps nothing.
fn mirror(follower: &dyn Follower, first: u64, last: u64, phys: u64, flags: u32) -> io::Result<()> {
    let (read, write) = (flags & MAP_F_READ != 0, flags & MAP_F_WRITE != 0);
    if flags & MAP_F_MMIO != 0 || !(read || write) {
        return Ok(());
    }
    follower.map(first, phys, span(first, last)?, read, write)
}

/// [`mirror`]'s mapping, gone.
fn unmirror(follower: &dyn Follower, first: u64, last: u64, flags: u32) -> io::Result<()> {
    if flags & MAP_F_MMIO != 0 || flags & (MAP_F_READ | MAP_F_WRITE) == 0 {
        return Ok(());
    }
    follower.unmap(first, span(first, last)?)
}

/// The bytes from `first` to `last`: none past what 64 bits count.
fn span(first: u64, last: u64) -> io::Result<u64> {
    last.checked_sub(first)
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a mapping of every address"))
}

/// The status of a request a follower could not carry out: no memory where the host had
/// none to pin, a range where the guest's is not RAM, else the device's error.
fn status_of(e: &io::Error) -> u8 {
    match e.kind() {
        io::ErrorKind::OutOfMemory => S_NOMEM,
        io::ErrorKind::InvalidInput => S_RANGE,
        _ => S_DEVERR,
    }
}

/// A domain: whether it bypasses translation (attached with BYPASS), the endpoints
/// attached to it, and its mappings by their first address, each to its last address,
/// the guest-physical address of its first and its flags.
#[derive(Debug, Default)]
struct Domain {
    bypass: bool,
    endpoints: usize,
    mappings: BTreeMap<u64, (u64, u64, u32)>,
}

impl Translation {
    /// The translation of `endpoints`, as from reset: nothing attached, bypass on; with the
    /// MSI window `msi`, its first and last address, where the machine has one.
    pub fn new(endpoints: Vec<u32>, msi: Option<(u64, u64)>) -> Translation {
        Translation {
            known: endpoints,
            slots: Vec::new(),
            vacant: Vec::new(),
            attached: BTreeMap::new(),
            domains: BTreeMap::new(),
            bypass: true,
            offers_bypass: true,
            msi,
            followers: Vec::new(),
        }
    }

    /// Whether it bypasses translation: by default it offers bypass, on from the start for
    /// an endpoint attached to no domain, as QEMU's `boot-bypass`, so a guest without the
    /// driver keeps DMA by guest-physical address. Off, it offers none (virtio 1.3
    /// §5.13.3: no VIRTIO_IOMMU_F_BYPASS_CONFIG, so no bypass domains either): what every
    /// endpoint passed through sees is what the guest maps for it alone, which its
    /// container then maps and guest memory keeps pinned, and nothing until the guest's
    /// driver attaches it. Neither a whole of RAM that memory plugged later would outgrow,
    /// nor RAM that free page reporting would take from under the device (D154).
    pub fn bypassing(mut self, on: bool) -> Translation {
        self.bypass = on;
        self.offers_bypass = on;
        self
    }

    /// The translation with the endpoints of the machine's hot-plug slots, `slots`, as
    /// well (D214): known to the guest's firmware from boot, each vacant, translated by
    /// none, until a function is plugged there ([`plugged`](Self::plugged)).
    pub fn with_slots(mut self, slots: &[u32]) -> Translation {
        for &s in slots {
            if !self.known.contains(&s) {
                self.known.push(s);
            }
        }
        self.slots = slots.to_vec();
        self.vacant = slots.to_vec();
        self
    }

    /// Whether `endpoint` is one it translates now: one of its own, but a vacant slot's.
    pub(crate) fn translates(&self, endpoint: u32) -> bool {
        self.known.contains(&endpoint) && !self.vacant.contains(&endpoint)
    }

    /// The slot of endpoint `endpoint` holds a function now (D214): translated from here
    /// on, attached to no domain, as a function found as its guest rescans is. An error
    /// where it is no slot's, or one that holds a function already.
    pub fn plugged(&mut self, endpoint: u32) -> io::Result<()> {
        if !self.slots.contains(&endpoint) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("endpoint {endpoint:#x} is no hot-plug slot's of this IOMMU"),
            ));
        }
        if !self.vacant.contains(&endpoint) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("endpoint {endpoint:#x}'s slot holds a function already"),
            ));
        }
        self.vacant.retain(|&e| e != endpoint);
        Ok(())
    }

    /// The function in the slot of endpoint `endpoint` gone (D214): the endpoint taken
    /// from its domain, which goes once no endpoint is attached to it, and from what
    /// follows it, which unmaps what it mapped once it follows no endpoint, else shows
    /// another's view; then vacant again. Nothing where it is no slot's.
    pub fn unplugged(&mut self, endpoint: u32) {
        if !self.slots.contains(&endpoint) {
            return;
        }
        self.detach(endpoint);
        if let Some(at) = self
            .followers
            .iter()
            .position(|f| f.endpoints.contains(&endpoint))
        {
            let alone = self.followers.get(at).is_some_and(|f| f.endpoints.len() == 1);
            if alone {
                let f = self.followers.remove(at);
                if let Err(e) = f.follower.clear() {
                    crate::warn!("virtio-iommu: endpoint {endpoint:#x}'s host mappings, unplugged: {e}");
                }
            } else if let Some(f) = self.followers.get_mut(at) {
                f.endpoints.retain(|&e| e != endpoint);
                if f.shows == endpoint
                    && let Some(&other) = f.endpoints.first()
                {
                    f.shows = other;
                }
            }
            if !alone
                && let Some(f) = self.followers.get(at)
                && let Err(e) = show(&*f.follower, self.view(f.shows))
            {
                crate::warn!("virtio-iommu: endpoint {:#x}'s host mappings: {e}", f.shows);
            }
        }
        if !self.vacant.contains(&endpoint) {
            self.vacant.push(endpoint);
        }
    }

    /// A device reset: no endpoint attached, and bypass as it was, which a device reset
    /// leaves (virtio 1.3 §5.13.4.2).
    fn reset(&mut self) {
        self.attached.clear();
        self.domains.clear();
        for f in &self.followers {
            if let Err(e) = show(&*f.follower, self.view(f.shows)) {
                crate::warn!(
                    "virtio-iommu: endpoint {:#x}'s host mappings at reset: {e}",
                    f.shows
                );
            }
        }
    }

    /// Has `follower` follow the view of `endpoint`, one this IOMMU translates, from the
    /// view it has now; or, where it follows other endpoints already, with theirs, as one
    /// ([`Follows`]).
    pub fn follow(&mut self, endpoint: u32, follower: Arc<dyn Follower>) -> io::Result<()> {
        if !self.translates(endpoint) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("endpoint {endpoint:#x} is not one this IOMMU translates"),
            ));
        }
        if self.follows(endpoint).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("endpoint {endpoint:#x} is followed already"),
            ));
        }
        if let Some(f) = self
            .followers
            .iter_mut()
            .find(|f| Arc::ptr_eq(&f.follower, &follower))
        {
            f.endpoints.push(endpoint);
            return Ok(());
        }
        show(&*follower, self.view(endpoint))?;
        self.followers.push(Follows {
            endpoints: vec![endpoint],
            follower,
            shows: endpoint,
        });
        Ok(())
    }

    /// What follows `endpoint`, where something does.
    fn follows(&self, endpoint: u32) -> Option<&Follows> {
        self.followers.iter().find(|f| f.endpoints.contains(&endpoint))
    }

    /// `endpoint`'s view, the one its follower shows now.
    fn shown_by(&mut self, endpoint: u32) {
        if let Some(f) = self
            .followers
            .iter_mut()
            .find(|f| f.endpoints.contains(&endpoint))
        {
            f.shows = endpoint;
        }
    }

    /// What `endpoint` sees now.
    fn view(&self, endpoint: u32) -> View<'_> {
        match self.attached.get(&endpoint).and_then(|id| self.domains.get(id)) {
            Some(d) if d.bypass => View::Identity,
            Some(d) => View::Mapped(&d.mappings),
            None if self.bypass => View::Identity,
            None => View::Blocked,
        }
    }

    /// The followers showing `domain`, each once, with the endpoint whose view they show.
    fn followers_of(&self, domain: u32) -> Vec<(u32, Arc<dyn Follower>)> {
        self.followers
            .iter()
            .filter(|f| self.attached.get(&f.shows) == Some(&domain))
            .map(|f| (f.shows, f.follower.clone()))
            .collect()
    }

    /// The status of a request `endpoint`'s follower could not carry out, its view shown it
    /// again as it was, which the request then leaves as it found it.
    fn refused(&self, endpoint: u32, e: &io::Error) -> u8 {
        crate::warn!("virtio-iommu: endpoint {endpoint:#x}'s host mappings: {e}");
        if let Some(f) = self.follows(endpoint)
            && let Err(e) = show(&*f.follower, self.view(f.shows))
        {
            crate::warn!("virtio-iommu: endpoint {endpoint:#x}'s host mappings, as they were: {e}");
        }
        status_of(e)
    }

    /// The configuration's bypass set to `on`: each endpoint attached to no domain then
    /// bypasses translation, or is blocked, its follower shown so.
    pub(crate) fn set_bypass(&mut self, on: bool) {
        // A field it does not offer: the driver writes it only where it does (§5.13.4.1).
        if self.bypass == on || !self.offers_bypass {
            return;
        }
        self.bypass = on;
        for f in &self.followers {
            if !self.attached.contains_key(&f.shows)
                && let Err(e) = show(&*f.follower, self.view(f.shows))
            {
                crate::warn!("virtio-iommu: endpoint {:#x}'s host mappings: {e}", f.shows);
            }
        }
    }

    /// Where an MSI `endpoint` aims at `address` lands in guest memory: at `address` within
    /// the MSI window, which is never translated, else where the endpoint's domain maps it
    /// for a write. None where it maps nothing there: the write faults.
    pub fn msi_target(&self, endpoint: u32, address: u64) -> Option<u64> {
        if self
            .msi
            .is_some_and(|(first, last)| (first..=last).contains(&address))
        {
            return Some(address);
        }
        self.translate(endpoint, address, 4, true)?
            .first()
            .map(|&(gpa, _)| gpa)
    }

    /// Where `len` bytes at I/O virtual address `iova` are in guest memory for a DMA of
    /// `endpoint`'s, a write where `write`: each part's guest-physical address and length,
    /// as its domain maps them; none where a byte is not mapped for the access, a fault
    /// that ends the DMA before it moves anything.
    pub fn translate(&self, endpoint: u32, iova: u64, len: u64, write: bool) -> Option<Vec<(u64, u64)>> {
        let end = iova.checked_add(len)?;
        let domain = match self.attached.get(&endpoint) {
            Some(id) => self.domains.get(id)?,
            None if self.bypass => return Some(vec![(iova, len)]),
            None => return None,
        };
        if domain.bypass {
            return Some(vec![(iova, len)]);
        }
        let want = if write { MAP_F_WRITE } else { MAP_F_READ };
        let mut parts = Vec::new();
        let mut at = iova;
        while at < end {
            let (&first, &(last, phys, flags)) = domain.mappings.range(..=at).next_back()?;
            if last < at || flags & want == 0 {
                return None;
            }
            // MAP's end is inclusive (§5.13.6.6), and may be u64::MAX. Bound
            // the span by the nonempty request before adding its final byte.
            let take = (last - at).min(end - at - 1) + 1;
            parts.push((phys + (at - first), take));
            at += take;
        }
        Some(parts)
    }

    /// Takes `endpoint` from the domain it is attached to, which goes, with its mappings,
    /// once no endpoint is (virtio 1.3 §5.13.6.4, as Linux's driver expects it:
    /// drivers/iommu/virtio-iommu.c, viommu_attach_dev).
    fn detach(&mut self, endpoint: u32) {
        if let Some(id) = self.attached.remove(&endpoint)
            && let Some(domain) = self.domains.get_mut(&id)
        {
            domain.endpoints = domain.endpoints.saturating_sub(1);
            if domain.endpoints == 0 {
                self.domains.remove(&id);
            }
        }
    }

    pub(crate) fn attach(&mut self, domain: u32, endpoint: u32, flags: u32) -> u8 {
        if !self.translates(endpoint) {
            return S_NOENT;
        }
        // A flag it does not offer is one it does not recognize (§5.13.6.3.2).
        if flags & !ATTACH_F_BYPASS != 0 || (flags & ATTACH_F_BYPASS != 0 && !self.offers_bypass) {
            return S_INVAL;
        }
        let bypass = flags & ATTACH_F_BYPASS != 0;
        if self.domains.get(&domain).is_some_and(|d| d.bypass != bypass) {
            return S_INVAL;
        }
        if self.attached.get(&endpoint) == Some(&domain) {
            // Attached there already: its follower shows its view, where it showed another's.
            if let Some(f) = self.follows(endpoint)
                && f.shows != endpoint
            {
                if let Err(e) = show(&*f.follower, self.view(endpoint)) {
                    return self.refused(endpoint, &e);
                }
                self.shown_by(endpoint);
            }
            return S_OK;
        }
        if let Some(f) = self.follows(endpoint) {
            // What the endpoint will see: the domain as it is, or a new one, empty.
            let empty = BTreeMap::new();
            let view = match self.domains.get(&domain) {
                Some(d) if d.bypass => View::Identity,
                Some(d) => View::Mapped(&d.mappings),
                None if bypass => View::Identity,
                None => View::Mapped(&empty),
            };
            if let Err(e) = show(&*f.follower, view) {
                return self.refused(endpoint, &e);
            }
        }
        self.detach(endpoint);
        let d = self.domains.entry(domain).or_default();
        d.bypass = bypass;
        d.endpoints += 1;
        self.attached.insert(endpoint, domain);
        self.shown_by(endpoint);
        S_OK
    }

    fn detach_from(&mut self, domain: u32, endpoint: u32) -> u8 {
        if self.attached.get(&endpoint) != Some(&domain) {
            return S_INVAL;
        }
        if let Some(f) = self.follows(endpoint) {
            let view = if self.bypass {
                View::Identity
            } else {
                View::Blocked
            };
            if let Err(e) = show(&*f.follower, view) {
                return self.refused(endpoint, &e);
            }
        }
        self.detach(endpoint);
        self.shown_by(endpoint);
        S_OK
    }

    pub(crate) fn map(&mut self, domain: u32, first: u64, last: u64, phys: u64, flags: u32) -> u8 {
        let followers = self.followers_of(domain);
        let Some(d) = self.domains.get_mut(&domain) else {
            return S_NOENT;
        };
        if d.bypass || flags & !(MAP_F_READ | MAP_F_WRITE | MAP_F_MMIO) != 0 || last < first {
            return S_INVAL;
        }
        if !first.is_multiple_of(PAGE)
            || !phys.is_multiple_of(PAGE)
            || !last.wrapping_add(1).is_multiple_of(PAGE)
        {
            return S_RANGE;
        }
        if phys.checked_add(last - first).is_none() {
            return S_RANGE;
        }
        // A mapping that overlaps one the domain has is refused (§5.13.6.6).
        let before = d.mappings.range(..=last).next_back();
        if before.is_some_and(|(_, &(end, _, _))| end >= first) {
            return S_INVAL;
        }
        // Each follower of the domain's endpoints maps it as well, or the domain does not.
        for (i, (endpoint, f)) in followers.iter().enumerate() {
            if let Err(e) = mirror(&**f, first, last, phys, flags) {
                crate::warn!("virtio-iommu: endpoint {endpoint:#x}'s host mapping of {first:#x}: {e}");
                for (_, mapped) in followers.iter().take(i) {
                    if let Err(e) = unmirror(&**mapped, first, last, flags) {
                        crate::warn!("virtio-iommu: a host mapping of {first:#x}, undone: {e}");
                    }
                }
                return status_of(&e);
            }
        }
        d.mappings.insert(first, (last, phys, flags));
        S_OK
    }

    fn unmap(&mut self, domain: u32, first: u64, last: u64) -> u8 {
        let followers = self.followers_of(domain);
        let Some(d) = self.domains.get_mut(&domain) else {
            return S_NOENT;
        };
        if last < first {
            return S_INVAL;
        }
        // Every mapping the range touches goes, but where one would be split none does
        // (§5.13.6.7).
        let touched: Vec<u64> = d
            .mappings
            .range(..=last)
            .filter(|(_, mapping)| mapping.0 >= first)
            .map(|(&start, _)| start)
            .collect();
        let whole = touched
            .iter()
            .all(|start| *start >= first && d.mappings.get(start).is_some_and(|&(end, _, _)| end <= last));
        if !whole {
            return S_RANGE;
        }
        // Each goes from the host first: one a follower keeps stays here too, as the
        // device can still reach it.
        for start in touched {
            if let Some(&(end, _, flags)) = d.mappings.get(&start) {
                for (endpoint, f) in &followers {
                    if let Err(e) = unmirror(&**f, start, end, flags) {
                        crate::warn!(
                            "virtio-iommu: endpoint {endpoint:#x}'s host mapping of {start:#x}, kept: {e}"
                        );
                        return status_of(&e);
                    }
                }
            }
            d.mappings.remove(&start);
        }
        S_OK
    }
}

/// The MSIs of a function the VMM emulates in a hot-plug slot behind the IOMMU (D214), a
/// disk's or a shared directory's: each lands where its endpoint's domain maps its address
/// for a write, or in the MSI window untranslated ([`Translation::msi_target`]), as a
/// passed-through function's do (`vfio::Translated`); one its domain maps nothing at
/// faults, and goes nowhere. Linux has an arm64 function's MSI doorbell mapped in its
/// domain (iommu.c, iommu_dma_prepare_msi) whether or not its driver's DMA goes through
/// the IOMMU, which a virtio driver's does not without VIRTIO_F_ACCESS_PLATFORM.
pub struct Msis {
    translation: Shared,
    endpoint: u32,
    to: Arc<dyn crate::devices::pci::Msi>,
}

impl std::fmt::Debug for Msis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Msis")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl Msis {
    /// The MSIs of `endpoint`, translated by `translation`, sent on by `to`.
    pub fn new(translation: Shared, endpoint: u32, to: Arc<dyn crate::devices::pci::Msi>) -> Msis {
        Msis {
            translation,
            endpoint,
            to,
        }
    }
}

impl crate::devices::pci::Msi for Msis {
    fn send(&self, address: u64, data: u32) {
        let target = self
            .translation
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .msi_target(self.endpoint, address);
        match target {
            Some(to) => self.to.send(to, data),
            None => crate::debug!(
                "virtio-iommu: endpoint {:#x}'s MSI to {address:#x}, which its domain does not map",
                self.endpoint
            ),
        }
    }
}

/// The values a request reads from its device-readable part at `at`, little-endian.
fn le32(req: &[u8], at: usize) -> Option<u32> {
    req.get(at..at + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
}

fn le64(req: &[u8], at: usize) -> Option<u64> {
    req.get(at..at + 8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
}

/// Answers request `req`, its device-readable part: its status, and for PROBE the
/// endpoint's properties, `PROBE_SIZE` bytes: the MSI window `msi`, its first and last
/// address, as a reserved region of subtype MSI, where there is one, and then none.
fn answer_request(state: &mut Translation, req: &[u8]) -> (u8, Vec<u8>) {
    let short = (S_INVAL, Vec::new());
    let Some(&kind) = req.first() else {
        return short;
    };
    match kind {
        T_ATTACH => match (le32(req, 4), le32(req, 8), le32(req, 12)) {
            (Some(domain), Some(endpoint), Some(flags)) => {
                (state.attach(domain, endpoint, flags), Vec::new())
            }
            _ => short,
        },
        T_DETACH => match (le32(req, 4), le32(req, 8)) {
            (Some(domain), Some(endpoint)) => (state.detach_from(domain, endpoint), Vec::new()),
            _ => short,
        },
        T_MAP => match (
            le32(req, 4),
            le64(req, 8),
            le64(req, 16),
            le64(req, 24),
            le32(req, 32),
        ) {
            (Some(domain), Some(first), Some(last), Some(phys), Some(flags)) => {
                (state.map(domain, first, last, phys, flags), Vec::new())
            }
            _ => short,
        },
        T_UNMAP => match (le32(req, 4), le64(req, 8), le64(req, 16)) {
            (Some(domain), Some(first), Some(last)) => (state.unmap(domain, first, last), Vec::new()),
            _ => short,
        },
        T_PROBE => match le32(req, 4) {
            Some(endpoint) if state.translates(endpoint) => {
                let mut props = vec![0u8; PROBE_SIZE as usize];
                // struct virtio_iommu_probe_resv_mem: its head (type, the length after
                // it), subtype, three reserved bytes, and the region's first and last.
                if let Some((first, last)) = state.msi {
                    let mut resv = Vec::with_capacity(usize::from(RESV_MEM_BYTES));
                    resv.extend_from_slice(&PROBE_T_RESV_MEM.to_le_bytes());
                    resv.extend_from_slice(&(RESV_MEM_BYTES - 4).to_le_bytes());
                    resv.extend_from_slice(&[RESV_MEM_T_MSI, 0, 0, 0]);
                    resv.extend_from_slice(&first.to_le_bytes());
                    resv.extend_from_slice(&last.to_le_bytes());
                    if let Some(at) = props.get_mut(..resv.len()) {
                        at.copy_from_slice(&resv);
                    }
                }
                (S_OK, props)
            }
            Some(_) => (S_NOENT, Vec::new()),
            None => short,
        },
        _ => (S_UNSUPP, Vec::new()),
    }
}

/// Serves one request of `chain` from guest memory `mem`: reads its device-readable part,
/// then writes its device-writable part, PROBE's properties at its start and the tail,
/// the status and three reserved bytes, at its end. Returns the bytes written: all of
/// that part.
fn answer(chain: &Chain, mem: &Access<'_>, state: &Mutex<Translation>) -> u32 {
    let mut req = [0u8; READABLE_MAX];
    let mut len = 0usize;
    for d in chain.readable() {
        let take = (d.len as usize).min(READABLE_MAX - len);
        let Some(to) = req.get_mut(len..len + take) else {
            break;
        };
        if mem.read(d.addr, to).is_err() {
            return 0;
        }
        len += take;
    }
    // Used lengths are u32 (virtio §2.7.8). Validate every writable span before
    // allocating or applying a request: an unreachable reply must change no domain.
    let total = chain.writable().try_fold(0u32, |n, d| {
        if d.len != 0 && mem.memory().io_ptr(d.addr, d.len as usize).is_err() {
            return None;
        }
        n.checked_add(d.len)
    });
    let Some(total) = total.filter(|n| *n >= 4) else {
        return 0;
    };
    // One host page bounds staging independently of the guest's offered capacity.
    // Page size is queried from this host, not a selected transfer-size constant.
    let Ok(page) = crate::platform::page_size() else {
        return 0;
    };
    let chunk = page.min(total as usize);
    if chunk == 0 {
        return 0;
    }
    let mut out = Vec::new();
    if out.try_reserve_exact(chunk).is_err() {
        return 0;
    }
    out.resize(chunk, 0u8);
    let (status, props) = {
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        let (status, props) = answer_request(&mut state, req.get(..len).unwrap_or_default());
        if props.len() > total as usize - 4 {
            (S_DEVERR, Vec::new())
        } else {
            (status, props)
        }
    };
    // struct virtio_iommu_req_tail (§5.13.6): status and three reserved bytes.
    let tail = [status, 0, 0, 0];
    let mut at = 0usize;
    for d in chain.writable() {
        let mut done = 0usize;
        while done < d.len as usize {
            let n = (d.len as usize - done).min(out.len());
            out.fill(0);
            for (start, bytes) in [(0, props.as_slice()), (total as usize - 4, tail.as_slice())] {
                let first = at.max(start);
                let end = (at + n).min(start + bytes.len());
                if first < end {
                    let Some(to) = out.get_mut(first - at..end - at) else {
                        return 0;
                    };
                    let Some(from) = bytes.get(first - start..end - start) else {
                        return 0;
                    };
                    to.copy_from_slice(from);
                }
            }
            let Some(part) = out.get(..n) else {
                return 0;
            };
            // The whole descriptor was validated above; checked addition also guards
            // this step if the range-validation implementation changes.
            let Some(addr) = d.addr.checked_add(done as u64) else {
                return 0;
            };
            if mem.write(addr, part).is_err() {
                return 0;
            }
            done += n;
            at += n;
        }
    }
    total
}

/// The device: its translation, which the functions' DMA shares, and its request queue's
/// worker, the event queue held unused (it reports no faults).
pub struct Iommu {
    state: Shared,
    context: Option<(Arc<GuestMemory>, Arc<DeviceInterrupt>)>,
    worker: Option<Worker>,
    events: Option<Queue>,
    paused: Option<Queue>,
}

impl std::fmt::Debug for Iommu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Iommu").finish_non_exhaustive()
    }
}

impl Iommu {
    /// The IOMMU of `state`'s endpoints.
    pub fn new(state: Shared) -> Iommu {
        Iommu {
            state,
            context: None,
            worker: None,
            events: None,
            paused: None,
        }
    }

    fn start(&mut self, queue: Queue) -> Result<(), String> {
        let Some((memory, interrupt)) = self.context.clone() else {
            self.paused = Some(queue);
            return Err("virtio-iommu started before activation".into());
        };
        let (mem, state) = (memory.clone(), self.state.clone());
        let answering = move |chain: &Chain| mem.access().map_or(0, |a| answer(chain, &a, &state));
        match Worker::start("virtio-iommu", queue, memory, interrupt, answering) {
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

    fn stop(&mut self) -> Option<Queue> {
        self.worker.take()?.stop()
    }
}

impl VirtioDevice for Iommu {
    fn device_id(&self) -> u32 {
        DEVICE_ID
    }

    fn features(&self) -> u64 {
        let offers_bypass = self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .offers_bypass;
        let bypass = if offers_bypass { F_BYPASS_CONFIG } else { 0 };
        feature::VERSION_1 | F_INPUT_RANGE | F_DOMAIN_RANGE | F_MAP_UNMAP | F_PROBE | F_MMIO | bypass
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE, EVENT_QUEUE_SIZE]
    }

    /// struct virtio_iommu_config: the page sizes mappings take (every power of two from
    /// 4 KiB), the input range (every address), the domains (every ID), PROBE's buffer,
    /// and whether an endpoint attached to no domain bypasses translation.
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let bypass = self.state.lock().unwrap_or_else(PoisonError::into_inner).bypass;
        let mut config = Vec::with_capacity(40);
        config.extend_from_slice(&(!(PAGE - 1)).to_le_bytes());
        config.extend_from_slice(&0u64.to_le_bytes());
        config.extend_from_slice(&u64::MAX.to_le_bytes());
        config.extend_from_slice(&0u32.to_le_bytes());
        config.extend_from_slice(&u32::MAX.to_le_bytes());
        config.extend_from_slice(&PROBE_SIZE.to_le_bytes());
        config.extend_from_slice(&[u8::from(bypass), 0, 0, 0]);
        let at = offset as usize;
        for (i, b) in data.iter_mut().enumerate() {
            *b = at
                .checked_add(i)
                .and_then(|i| config.get(i))
                .copied()
                .unwrap_or(0);
        }
    }

    /// The driver writes `bypass` alone, at 36.
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        if let Some(&b) = 36u64
            .checked_sub(offset)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| data.get(i))
        {
            self.state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .set_bypass(b & 1 != 0);
        }
    }

    fn activate(&mut self, activation: Activation) -> Result<(), String> {
        let Activation {
            memory,
            mut queues,
            interrupt,
            ..
        } = activation;
        if queues.len() != 2 {
            return Err("virtio-iommu activated without its two queues".into());
        }
        self.events = queues.pop();
        let requests = queues
            .pop()
            .ok_or("virtio-iommu activated without its request queue")?;
        self.context = Some((memory, interrupt));
        self.start(requests).inspect_err(|_| self.paused = None)
    }

    /// Requests are answered on the device's worker, never on the notifying vCPU's thread.
    fn notify(&self, queue: u16) {
        if queue == 0
            && let Some(w) = &self.worker
        {
            w.notify();
        }
    }

    fn reset(&mut self) {
        self.stop();
        self.paused = None;
        self.events = None;
        self.context = None;
        self.state.lock().unwrap_or_else(PoisonError::into_inner).reset();
    }

    fn pause(&mut self) -> Vec<super::QueueState> {
        if let Some(queue) = self.stop() {
            self.paused = Some(queue);
        }
        self.paused.iter().chain(&self.events).map(Queue::state).collect()
    }

    fn resume(&mut self) -> Result<(), String> {
        match self.paused.take() {
            Some(queue) => self.start(queue),
            None => Ok(()),
        }
    }

    /// A machine with a PCI bus saves no snapshot (D153), nor so its IOMMU's domains.
    fn unsaved(&self) -> Option<String> {
        Some("an IOMMU's domains and mappings".into())
    }
}

impl Drop for Iommu {
    fn drop(&mut self) {
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MSI: Option<(u64, u64)> = Some((0xfee0_0000, 0xfeef_ffff));

    /// Each request's readable part is its structure's less its 4-byte tail (or, PROBE's,
    /// its properties), and the longest is PROBE's.
    #[test]
    fn the_longest_readable_part_is_probes() {
        assert_eq!(
            [
                ATTACH_READABLE,
                DETACH_READABLE,
                MAP_READABLE,
                UNMAP_READABLE,
                PROBE_READABLE
            ],
            [20, 20, 36, 28, 72]
        );
        assert_eq!(READABLE_MAX, 72);
        assert_eq!(attach(1, 2, 0).len(), ATTACH_READABLE);
    }

    fn attach(domain: u32, endpoint: u32, flags: u32) -> Vec<u8> {
        let mut r = vec![T_ATTACH, 0, 0, 0];
        for v in [domain, endpoint, flags, 0] {
            r.extend_from_slice(&v.to_le_bytes());
        }
        r
    }

    fn detach(domain: u32, endpoint: u32) -> Vec<u8> {
        let mut r = vec![T_DETACH, 0, 0, 0];
        for v in [domain, endpoint, 0, 0] {
            r.extend_from_slice(&v.to_le_bytes());
        }
        r
    }

    fn map(domain: u32, first: u64, last: u64, phys: u64, flags: u32) -> Vec<u8> {
        let mut r = vec![T_MAP, 0, 0, 0];
        r.extend_from_slice(&domain.to_le_bytes());
        for v in [first, last, phys] {
            r.extend_from_slice(&v.to_le_bytes());
        }
        r.extend_from_slice(&flags.to_le_bytes());
        r
    }

    fn unmap(domain: u32, first: u64, last: u64) -> Vec<u8> {
        let mut r = vec![T_UNMAP, 0, 0, 0];
        r.extend_from_slice(&domain.to_le_bytes());
        for v in [first, last] {
            r.extend_from_slice(&v.to_le_bytes());
        }
        r.extend_from_slice(&0u32.to_le_bytes());
        r
    }

    fn status(t: &mut Translation, req: &[u8]) -> u8 {
        answer_request(t, req).0
    }

    #[test]
    fn invalid_response_is_rejected_before_applying_the_request() {
        use crate::devices::virtio::queue::Descriptor;
        let page = crate::platform::page_size().unwrap();
        let mem = GuestMemory::anonymous(&[(0x8000_0000, page)]).unwrap();
        let a = mem.access().unwrap();
        let req = attach(1, 8, 0);
        a.write(0x8000_0000, &req).unwrap();
        let state = Mutex::new(Translation::new(vec![8], MSI));
        let chain = Chain {
            head: 0,
            descriptors: vec![
                Descriptor {
                    addr: 0x8000_0000,
                    len: req.len() as u32,
                    writable: false,
                },
                Descriptor {
                    addr: 0x8000_0000 + page as u64 - 4,
                    len: 8,
                    writable: true,
                },
            ],
        };
        assert_eq!(answer(&chain, &a, &state), 0);
        assert!(!state.lock().unwrap().attached.contains_key(&8));
    }

    #[test]
    fn padded_probe_response_streams_across_descriptors() {
        use crate::devices::virtio::queue::Descriptor;
        let page = crate::platform::page_size().unwrap();
        let base = 0x8000_0000;
        let mem = GuestMemory::anonymous(&[(base, 4 * page)]).unwrap();
        let a = mem.access().unwrap();
        let mut req = vec![T_PROBE, 0, 0, 0];
        req.extend_from_slice(&8u32.to_le_bytes());
        req.resize(PROBE_READABLE, 0);
        a.write(base, &req).unwrap();
        let first = base + page as u64;
        let next = base + 2 * page as u64;
        let chain = Chain {
            head: 0,
            descriptors: vec![
                Descriptor {
                    addr: base,
                    len: req.len() as u32,
                    writable: false,
                },
                Descriptor {
                    addr: first,
                    len: 1,
                    writable: true,
                },
                Descriptor {
                    addr: next,
                    len: page as u32 + 3,
                    writable: true,
                },
            ],
        };
        let state = Mutex::new(Translation::new(vec![8], MSI));
        let total = page + 4;
        let (_, props) = answer_request(&mut state.lock().unwrap(), &req);
        a.write(next, &vec![0xff; page + 3]).unwrap();
        assert_eq!(answer(&chain, &a, &state), total as u32);
        let mut response = vec![0u8; total];
        a.read(first, &mut response[..1]).unwrap();
        a.read(next, &mut response[1..]).unwrap();
        assert_eq!(&response[..props.len()], props);
        assert!(response[props.len()..total - 4].iter().all(|b| *b == 0));
        assert_eq!(&response[total - 4..], &[S_OK, 0, 0, 0]);
    }

    #[test]
    fn full_span_mapping_translates_an_msi_without_overflow() {
        let mut t = Translation::new(vec![8], MSI);
        assert_eq!(status(&mut t, &attach(1, 8, 0)), S_OK);
        assert_eq!(
            status(
                &mut t,
                &map(1, 0, u64::MAX, 0, MAP_F_READ | MAP_F_WRITE | MAP_F_MMIO)
            ),
            S_OK
        );
        assert_eq!(t.translate(8, 0, 4, true), Some(vec![(0, 4)]));
        assert_eq!(t.msi_target(8, 0), Some(0));
        assert_eq!(
            t.translate(8, u64::MAX - 4, 4, true),
            Some(vec![(u64::MAX - 4, 4)])
        );
    }

    /// An endpoint attached to no domain bypasses translation until the driver turns it
    /// off; attached to a domain, its DMA reaches what the domain maps, read or written as
    /// each mapping allows, and a byte the domain does not map faults the whole access.
    #[test]
    fn dma_reaches_what_its_domain_maps() {
        let mut t = Translation::new(vec![8], MSI);
        assert_eq!(
            t.translate(8, 0x1000, 16, true),
            Some(vec![(0x1000, 16)]),
            "bypass from reset"
        );
        t.bypass = false;
        assert_eq!(t.translate(8, 0x1000, 16, true), None);
        assert_eq!(
            status(&mut t, &attach(1, 9, 0)),
            S_NOENT,
            "an endpoint it does not translate"
        );
        assert_eq!(status(&mut t, &attach(1, 8, 0)), S_OK);
        assert_eq!(
            status(&mut t, &map(1, 0x10_0000, 0x10_1fff, 0x8000_0000, MAP_F_READ)),
            S_OK
        );
        assert_eq!(
            status(
                &mut t,
                &map(1, 0x10_2000, 0x10_2fff, 0x9000_0000, MAP_F_READ | MAP_F_WRITE)
            ),
            S_OK
        );
        // Across two mappings, contiguous in I/O addresses, apart in guest memory.
        assert_eq!(
            t.translate(8, 0x10_1ff0, 0x20, false),
            Some(vec![(0x8000_1ff0, 0x10), (0x9000_0000, 0x10)])
        );
        assert_eq!(
            t.translate(8, 0x10_1ff0, 0x20, true),
            None,
            "the first is read-only"
        );
        assert_eq!(t.translate(8, 0x10_2ff0, 0x20, false), None, "past the last");
        assert_eq!(t.translate(9, 0x10_0000, 8, false), None, "no such endpoint");
    }

    /// MAP refuses what is not whole pages, overlaps a mapping, or names a domain that
    /// does not exist; UNMAP removes what the range covers whole, and nothing where it
    /// would split a mapping.
    #[test]
    fn mappings_are_whole_pages_and_go_whole() {
        let mut t = Translation::new(vec![8], MSI);
        assert_eq!(status(&mut t, &map(1, 0, 0xfff, 0, MAP_F_READ)), S_NOENT);
        assert_eq!(status(&mut t, &attach(1, 8, 0)), S_OK);
        assert_eq!(status(&mut t, &map(1, 0x800, 0xfff, 0, MAP_F_READ)), S_RANGE);
        assert_eq!(status(&mut t, &map(1, 0, 0x7ff, 0, MAP_F_READ)), S_RANGE);
        assert_eq!(status(&mut t, &map(1, 0, 0xfff, 0x800, MAP_F_READ)), S_RANGE);
        assert_eq!(
            status(&mut t, &map(1, 0, 0xfff, 0, 1 << 3)),
            S_INVAL,
            "a flag it has not"
        );
        assert_eq!(
            status(&mut t, &map(1, 0x1000, 0x2fff, 0x1_0000, MAP_F_READ)),
            S_OK
        );
        assert_eq!(
            status(&mut t, &map(1, 0x2000, 0x3fff, 0x2_0000, MAP_F_READ)),
            S_INVAL,
            "overlaps"
        );
        assert_eq!(
            status(&mut t, &map(1, 0x3000, 0x3fff, 0x2_0000, MAP_F_READ)),
            S_OK
        );
        assert_eq!(status(&mut t, &unmap(1, 0x1000, 0x1fff)), S_RANGE, "would split");
        assert!(t.translate(8, 0x1000, 0x3000, false).is_some(), "nothing went");
        assert_eq!(status(&mut t, &unmap(1, 0, 0x2fff)), S_OK);
        assert_eq!(t.translate(8, 0x1000, 8, false), None);
        assert_eq!(t.translate(8, 0x3000, 8, false), Some(vec![(0x2_0000, 8)]));
        assert_eq!(
            status(&mut t, &unmap(1, 0x10_0000, 0x10_0fff)),
            S_OK,
            "nothing there"
        );
    }

    /// An endpoint attached elsewhere leaves its domain, which goes with its mappings once
    /// no endpoint is attached; a bypass domain maps nothing and passes everything; a
    /// domain is one kind or the other.
    #[test]
    fn domains_go_with_their_last_endpoint() {
        let mut t = Translation::new(vec![8, 16], MSI);
        assert_eq!(status(&mut t, &attach(1, 8, 0)), S_OK);
        assert_eq!(status(&mut t, &attach(1, 16, 0)), S_OK);
        assert_eq!(status(&mut t, &map(1, 0, 0xfff, 0x5000, MAP_F_READ)), S_OK);
        assert_eq!(status(&mut t, &attach(2, 8, ATTACH_F_BYPASS)), S_OK);
        assert_eq!(t.translate(8, 0x1234, 4, true), Some(vec![(0x1234, 4)]), "bypass");
        assert_eq!(status(&mut t, &map(2, 0, 0xfff, 0, MAP_F_READ)), S_INVAL);
        assert_eq!(status(&mut t, &attach(2, 16, 0)), S_INVAL, "a bypass domain");
        assert_eq!(
            t.translate(16, 0, 4, false),
            Some(vec![(0x5000, 4)]),
            "domain 1 keeps 16"
        );
        assert_eq!(status(&mut t, &detach(1, 8)), S_INVAL, "8 is in domain 2");
        assert_eq!(status(&mut t, &detach(1, 16)), S_OK);
        assert!(
            !t.domains.contains_key(&1),
            "its last endpoint gone, domain 1 goes"
        );
        assert_eq!(status(&mut t, &attach(1, 16, 0)), S_OK);
        t.bypass = false;
        assert_eq!(t.translate(16, 0, 4, false), None, "its mappings went with it");
    }

    /// What a follower was shown, as a host's IOMMU would hold it: all of RAM, or each
    /// mapping by its I/O address (its guest-physical address, length, read, write); every
    /// map refused for want of memory while `refusing`.
    #[derive(Debug, Default)]
    struct Shown {
        identity: Mutex<bool>,
        mapped: Mutex<BTreeMap<u64, (u64, u64, bool, bool)>>,
        refusing: Mutex<bool>,
    }

    impl Shown {
        fn whole(&self) -> bool {
            *self.identity.lock().unwrap()
        }

        fn mappings(&self) -> Vec<(u64, (u64, u64, bool, bool))> {
            self.mapped
                .lock()
                .unwrap()
                .iter()
                .map(|(&k, &v)| (k, v))
                .collect()
        }

        fn refuse(&self, on: bool) {
            *self.refusing.lock().unwrap() = on;
        }
    }

    impl Follower for Shown {
        fn identity(&self) -> io::Result<()> {
            self.clear()?;
            *self.identity.lock().unwrap() = true;
            Ok(())
        }

        fn clear(&self) -> io::Result<()> {
            *self.identity.lock().unwrap() = false;
            self.mapped.lock().unwrap().clear();
            Ok(())
        }

        fn map(&self, iova: u64, gpa: u64, len: u64, read: bool, write: bool) -> io::Result<()> {
            if *self.refusing.lock().unwrap() {
                return Err(io::Error::from(io::ErrorKind::OutOfMemory));
            }
            // As VFIO refuses a mapping where one is (vfio_dma_do_map: EEXIST).
            let mut mapped = self.mapped.lock().unwrap();
            if mapped.contains_key(&iova) {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            mapped.insert(iova, (gpa, len, read, write));
            Ok(())
        }

        fn unmap(&self, iova: u64, _len: u64) -> io::Result<()> {
            self.mapped.lock().unwrap().remove(&iova);
            Ok(())
        }
    }

    /// A followed endpoint's host mappings are what it sees: all of RAM while it bypasses
    /// translation, as it does from the start by default and in a bypass domain; nothing
    /// while it is blocked, as after a device reset that left bypass off; and its
    /// domain's mappings of RAM, replayed as it attaches, then each MAP and UNMAP as they
    /// come, an MSI doorbell's (device memory) not among them. A request the host cannot
    /// carry out is refused, and leaves the domain and the endpoint as they were.
    #[test]
    fn a_followed_endpoint_maps_what_it_sees() {
        let mut t = Translation::new(vec![8, 16], None);
        let host = Arc::new(Shown::default());
        t.follow(8, host.clone()).unwrap();
        assert!(host.whole(), "bypass from reset");
        assert!(
            t.follow(24, host.clone()).is_err(),
            "an endpoint it does not translate"
        );
        // Domain 1 maps a page before 8 attaches to it; 16, which nothing follows, is in it.
        assert_eq!(status(&mut t, &attach(1, 16, 0)), S_OK);
        assert_eq!(
            status(&mut t, &map(1, 0x10_0000, 0x10_0fff, 0x8000_0000, MAP_F_READ)),
            S_OK
        );
        assert_eq!(status(&mut t, &attach(1, 8, 0)), S_OK);
        assert!(!host.whole());
        assert_eq!(
            host.mappings(),
            vec![(0x10_0000, (0x8000_0000, 0x1000, true, false))],
            "replayed"
        );
        let both = MAP_F_READ | MAP_F_WRITE;
        assert_eq!(
            status(&mut t, &map(1, 0x20_0000, 0x20_1fff, 0x9000_0000, both)),
            S_OK
        );
        assert_eq!(
            status(
                &mut t,
                &map(1, 0x30_0000, 0x30_0fff, 0x0802_0000, MAP_F_WRITE | MAP_F_MMIO)
            ),
            S_OK
        );
        assert_eq!(host.mappings().len(), 2, "the doorbell is not the host's to map");
        assert_eq!(status(&mut t, &unmap(1, 0x10_0000, 0x10_0fff)), S_OK);
        assert_eq!(
            host.mappings(),
            vec![(0x20_0000, (0x9000_0000, 0x2000, true, true))]
        );
        // A host with no memory to pin: the MAP refused, the domain without it.
        host.refuse(true);
        assert_eq!(
            status(&mut t, &map(1, 0x40_0000, 0x40_0fff, 0xa000_0000, MAP_F_READ)),
            S_NOMEM
        );
        assert_eq!(t.translate(16, 0x40_0000, 4, false), None, "nor for 16");
        host.refuse(false);
        // Detached, it bypasses; with bypass off, it is blocked.
        assert_eq!(status(&mut t, &detach(1, 8)), S_OK);
        assert!(host.whole());
        t.set_bypass(false);
        assert!(!host.whole() && host.mappings().is_empty(), "blocked");
        // An attach the host cannot carry out leaves it where it was: blocked.
        host.refuse(true);
        assert_eq!(status(&mut t, &attach(1, 8, 0)), S_NOMEM);
        assert!(!host.whole() && host.mappings().is_empty());
        assert_eq!(t.translate(8, 0x20_0000, 4, false), None, "not attached");
        host.refuse(false);
        // A bypass domain: all of RAM; and from reset, bypass again.
        assert_eq!(status(&mut t, &attach(2, 8, ATTACH_F_BYPASS)), S_OK);
        assert!(host.whole());
        assert_eq!(status(&mut t, &attach(1, 8, 0)), S_OK);
        assert!(!host.whole());
        // A device reset detaches it and leaves bypass off: blocked.
        t.reset();
        assert!(!host.whole() && host.mappings().is_empty());
        t.set_bypass(true);
        assert!(host.whole());
        // An IOMMU of passed-through functions alone offers no bypass: blocked from the
        // start, pinning nothing; no bypass domain, nor a bypass the driver writes.
        let mut blocked = Translation::new(vec![8], None).bypassing(false);
        let pinned = Arc::new(Shown::default());
        blocked.follow(8, pinned.clone()).unwrap();
        assert!(!pinned.whole() && pinned.mappings().is_empty());
        assert_eq!(status(&mut blocked, &attach(1, 8, ATTACH_F_BYPASS)), S_INVAL);
        blocked.set_bypass(true);
        assert!(!pinned.whole(), "a field it does not offer");
        assert_eq!(status(&mut blocked, &attach(1, 8, 0)), S_OK);
        let both = MAP_F_READ | MAP_F_WRITE;
        assert_eq!(status(&mut blocked, &map(1, 0, 0xfff, 0x8000_0000, both)), S_OK);
        assert_eq!(pinned.mappings(), vec![(0, (0x8000_0000, 0x1000, true, true))]);
    }

    /// The endpoints of one host group share a follower, its container: it shows the view
    /// of the last of them attached or detached, and each MAP and UNMAP of the domain it
    /// shows reaches it once; one already followed is refused another follower.
    #[test]
    fn a_groups_endpoints_share_their_follower() {
        let mut t = Translation::new(vec![8, 9, 16], None).bypassing(false);
        let group = Arc::new(Shown::default());
        t.follow(8, group.clone()).unwrap();
        t.follow(9, group.clone()).unwrap();
        assert!(
            t.follow(9, Arc::new(Shown::default())).is_err(),
            "followed already"
        );
        assert!(!group.whole() && group.mappings().is_empty(), "blocked");
        // The guest attaches the group to domain 1, one endpoint after the other.
        assert_eq!(status(&mut t, &attach(1, 8, 0)), S_OK);
        let both = MAP_F_READ | MAP_F_WRITE;
        assert_eq!(
            status(&mut t, &map(1, 0x10_0000, 0x10_0fff, 0x8000_0000, both)),
            S_OK
        );
        assert_eq!(status(&mut t, &attach(1, 9, 0)), S_OK);
        assert_eq!(
            status(&mut t, &map(1, 0x20_0000, 0x20_0fff, 0x8000_1000, MAP_F_READ)),
            S_OK,
            "mapped once, not once for each endpoint"
        );
        assert_eq!(
            group.mappings(),
            vec![
                (0x10_0000, (0x8000_0000, 0x1000, true, true)),
                (0x20_0000, (0x8000_1000, 0x1000, true, false))
            ]
        );
        assert_eq!(status(&mut t, &unmap(1, 0x10_0000, 0x10_0fff)), S_OK);
        assert_eq!(group.mappings().len(), 1);
        // Then to domain 2: the follower shows it from its first endpoint's attach, and a
        // MAP of domain 1, which 9 is still in, is the guest's alone.
        assert_eq!(status(&mut t, &attach(2, 8, 0)), S_OK);
        assert!(group.mappings().is_empty(), "domain 2 maps nothing yet");
        assert_eq!(
            status(&mut t, &map(1, 0x30_0000, 0x30_0fff, 0x8000_2000, MAP_F_READ)),
            S_OK
        );
        assert!(group.mappings().is_empty());
        assert_eq!(t.translate(9, 0x30_0000, 4, false), Some(vec![(0x8000_2000, 4)]));
        assert_eq!(status(&mut t, &attach(2, 9, 0)), S_OK);
        assert_eq!(status(&mut t, &map(2, 0, 0xfff, 0x8000_3000, both)), S_OK);
        assert_eq!(group.mappings(), vec![(0, (0x8000_3000, 0x1000, true, true))]);
        // Detached, the last of them blocked: and attached again to the domain the other
        // is in, its view again.
        assert_eq!(status(&mut t, &detach(2, 8)), S_OK);
        assert!(group.mappings().is_empty());
        assert_eq!(status(&mut t, &attach(2, 9, 0)), S_OK, "9 is in domain 2 already");
        assert_eq!(group.mappings(), vec![(0, (0x8000_3000, 0x1000, true, true))]);
        // An endpoint followed alone keeps its own.
        let alone = Arc::new(Shown::default());
        t.follow(16, alone.clone()).unwrap();
        assert_eq!(status(&mut t, &attach(2, 16, 0)), S_OK);
        assert_eq!(alone.mappings(), group.mappings());
        assert_eq!(status(&mut t, &map(2, 0x1000, 0x1fff, 0x8000_4000, both)), S_OK);
        assert_eq!((alone.mappings().len(), group.mappings().len()), (2, 2));
    }

    /// An MSI lands in the MSI window untranslated, as Linux's IOMMU_RESV_MSI has it, and
    /// elsewhere where its endpoint's domain maps the address for a write; where the
    /// domain maps nothing there, nowhere.
    #[test]
    fn msis_land_untranslated_in_the_window_and_translated_elsewhere() {
        let mut t = Translation::new(vec![8], MSI);
        assert_eq!(status(&mut t, &attach(1, 8, 0)), S_OK);
        assert_eq!(t.msi_target(8, 0xfee0_1000), Some(0xfee0_1000), "the window");
        assert_eq!(t.msi_target(8, 0x800_0040), None, "unmapped");
        assert_eq!(
            status(
                &mut t,
                &map(1, 0x800_0000, 0x800_0fff, 0x0802_0000, MAP_F_WRITE | MAP_F_MMIO)
            ),
            S_OK
        );
        assert_eq!(t.msi_target(8, 0x800_0040), Some(0x0802_0040), "its doorbell");
    }

    /// A hot-plug slot's endpoint (D214) is known from boot but translated only while a
    /// function is plugged there: vacant, PROBE and ATTACH find no endpoint and nothing may
    /// follow it; plugged, it is attached and followed as any; unplugged, it leaves its
    /// domain, which goes with its last endpoint, and what followed it alone unmaps what
    /// it mapped, where one shared by another endpoint shows that one's view; then it is
    /// vacant again, and plugged afresh, attached to nothing.
    #[test]
    fn a_slots_endpoint_is_translated_while_a_function_is_plugged_there() {
        let mut t = Translation::new(vec![8], MSI)
            .bypassing(false)
            .with_slots(&[16, 24]);
        let mut probe = vec![T_PROBE, 0, 0, 0];
        probe.extend_from_slice(&16u32.to_le_bytes());
        probe.extend_from_slice(&[0; 64]);
        assert_eq!(answer_request(&mut t, &probe).0, S_NOENT, "vacant");
        assert_eq!(status(&mut t, &attach(1, 16, 0)), S_NOENT);
        let shown = Arc::new(Shown::default());
        assert!(
            t.follow(16, shown.clone()).is_err(),
            "nothing follows a vacant slot"
        );
        assert!(t.plugged(8).is_err(), "no slot's");
        t.plugged(16).unwrap();
        assert!(t.plugged(16).is_err(), "plugged already");
        assert_eq!(answer_request(&mut t, &probe).0, S_OK);
        t.follow(16, shown.clone()).unwrap();
        assert!(!shown.whole(), "blocked until attached");
        assert_eq!(status(&mut t, &attach(1, 16, 0)), S_OK);
        assert_eq!(
            status(&mut t, &map(1, 0x1000, 0x1fff, 0x8000, MAP_F_READ | MAP_F_WRITE)),
            S_OK
        );
        assert_eq!(shown.mappings(), [(0x1000, (0x8000, 0x1000, true, true))]);
        // The test endpoint in the domain too, so that it outlives the slot's.
        assert_eq!(status(&mut t, &attach(1, 8, 0)), S_OK);
        t.unplugged(16);
        assert!(shown.mappings().is_empty(), "its follower unmapped all it mapped");
        assert_eq!(answer_request(&mut t, &probe).0, S_NOENT, "vacant again");
        assert_eq!(
            t.translate(8, 0x1000, 8, false),
            Some(vec![(0x8000, 8)]),
            "the domain stays"
        );
        assert_eq!(status(&mut t, &detach(1, 8)), S_OK);
        t.plugged(16).unwrap();
        assert_eq!(t.translate(16, 0x1000, 8, false), None, "attached to nothing");

        // Two endpoints sharing a follower: the one left is what it shows.
        let mut t = Translation::new(Vec::new(), MSI)
            .bypassing(false)
            .with_slots(&[16, 24]);
        let shared = Arc::new(Shown::default());
        for e in [16, 24] {
            t.plugged(e).unwrap();
            t.follow(e, shared.clone()).unwrap();
        }
        assert_eq!(status(&mut t, &attach(2, 24, 0)), S_OK);
        assert_eq!(status(&mut t, &map(2, 0x4000, 0x4fff, 0x9000, MAP_F_READ)), S_OK);
        assert_eq!(status(&mut t, &attach(1, 16, 0)), S_OK);
        assert!(shared.mappings().is_empty(), "it shows 16's empty domain");
        t.unplugged(16);
        assert_eq!(
            shared.mappings(),
            [(0x4000, (0x9000, 0x1000, true, false))],
            "24's"
        );
        t.unplugged(24);
        assert!(shared.mappings().is_empty());
        t.unplugged(8);
    }

    /// A function the VMM emulates in a slot behind the IOMMU sends its MSIs where its
    /// domain maps them, the window's untranslated; one its domain does not map goes
    /// nowhere.
    #[test]
    fn an_emulated_slots_msis_go_where_its_domain_maps_them() {
        #[derive(Default)]
        struct Sent(Mutex<Vec<(u64, u32)>>);
        impl crate::devices::pci::Msi for Sent {
            fn send(&self, address: u64, data: u32) {
                self.0.lock().unwrap().push((address, data));
            }
        }
        let t: Shared = Arc::new(Mutex::new(Translation::new(Vec::new(), MSI).with_slots(&[16])));
        t.lock().unwrap().plugged(16).unwrap();
        let sent = Arc::new(Sent::default());
        let msis = Msis::new(t.clone(), 16, sent.clone());
        use crate::devices::pci::Msi as _;
        msis.send(0xfee0_0000, 1);
        assert_eq!(status(&mut t.lock().unwrap(), &attach(1, 16, 0)), S_OK);
        msis.send(0x800_0040, 2);
        assert_eq!(
            status(
                &mut t.lock().unwrap(),
                &map(1, 0x800_0000, 0x800_0fff, 0x0802_0000, MAP_F_WRITE | MAP_F_MMIO)
            ),
            S_OK
        );
        msis.send(0x800_0040, 3);
        assert_eq!(*sent.0.lock().unwrap(), [(0xfee0_0000, 1), (0x0802_0040, 3)]);
    }

    /// PROBE answers an endpoint's reserved regions: the MSI window, of subtype MSI, where
    /// the machine has one (x86_64's), which fills its buffer, or nothing; an endpoint it
    /// does not translate has none.
    #[test]
    fn probe_gives_the_msi_doorbell() {
        assert_eq!(PROBE_SIZE, 24, "struct virtio_iommu_probe_resv_mem, whole");
        let mut t = Translation::new(vec![8], MSI);
        let mut probe = vec![T_PROBE, 0, 0, 0];
        probe.extend_from_slice(&8u32.to_le_bytes());
        probe.extend_from_slice(&[0; 64]);
        let (s, props) = answer_request(&mut t, &probe);
        assert_eq!(s, S_OK);
        assert_eq!(props.len(), PROBE_SIZE as usize);
        assert_eq!(&props[..8], &[1, 0, 20, 0, RESV_MEM_T_MSI, 0, 0, 0]);
        assert_eq!(&props[8..16], &0xfee0_0000u64.to_le_bytes());
        assert_eq!(&props[16..24], &0xfeef_ffffu64.to_le_bytes());
        assert!(props[24..].iter().all(|&b| b == 0), "then none");
        let (s, props) = answer_request(&mut Translation::new(vec![8], None), &probe);
        assert_eq!(s, S_OK);
        assert!(props.iter().all(|&b| b == 0), "arm64's: no window, none");
        probe[4] = 9;
        assert_eq!(answer_request(&mut t, &probe).0, S_NOENT);
        assert_eq!(answer_request(&mut t, &[T_ATTACH, 0, 0, 0]).0, S_INVAL, "short");
        assert_eq!(answer_request(&mut t, &[9, 0, 0, 0]).0, S_UNSUPP);
    }
}
