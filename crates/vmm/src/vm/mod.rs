//! A microVM: configuration, lifecycle, and exit reasons, independent of the backend
//! that runs it.

#[cfg(hv)]
use std::path::Path;
use std::path::PathBuf;
#[cfg(hv)]
use std::sync::Arc;
#[cfg(hv)]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(hv)]
use crate::memory::{FileSpan, GuestMemory};

#[cfg(hv = "hvf")]
use crate::devices::virtio::gpu;
#[cfg(hv)]
use crate::devices::virtio::{head, mem, pmem, vsock};

#[cfg(all(hv, target_arch = "aarch64"))]
mod aarch64;
#[cfg(all(hv, target_arch = "aarch64"))]
use aarch64 as machine;
#[cfg(all(hv, target_arch = "x86_64"))]
mod x86_64;
#[cfg(all(hv, target_arch = "x86_64"))]
use x86_64 as machine;
/// Working sets from the pages a restored machine's host maps: KVM's, and the x86_64
/// machine's on any backend.
#[cfg(any(hv = "kvm", all(hv, target_arch = "x86_64")))]
mod mapped;
/// Whether this build's VMs can be snapshotted and restored: HVF's and KVM's can, on
/// x86_64 and arm64 (D154), WHP's not yet (D140).
pub const SNAPSHOTS: bool = cfg!(any(hv = "hvf", hv = "kvm"));
/// Whether a VM resumed from its snapshot records its working set, for restores to
/// prefetch: HVF's at stage 2 (hv::hvf::Watch), KVM's from the pages its host maps
/// (vm::mapped).
pub const WORKING_SETS: bool = SNAPSHOTS;
/// Which run records it. HVF's: the one that saves the template, whose command recording
/// slows sixfold (PM M30), on a run that boots anyway. KVM's: the first warm restore
/// without one, for which recording costs nothing, and whose pages later restores touch,
/// where the saving run's they barely do (PM M33).
pub const RESTORES_RECORD: bool = cfg!(hv = "kvm");

#[cfg(any(hv, test))]
mod barrier;
#[cfg(any(hv, test))]
mod hold;
#[cfg(hv)]
mod runtime;
#[cfg(hv)]
pub use runtime::{
    Handle, Running, Saved, accept_working_set, check_host, keep_working_set, max_vcpus, restore,
    restore_from, start, working_set_limit,
};

#[cfg(not(hv))]
mod unsupported;
#[cfg(not(hv))]
pub use unsupported::{
    Handle, Running, Saved, accept_working_set, check_host, keep_working_set, max_vcpus, restore,
    restore_from, start, working_set_limit,
};

/// The host sides of a VM's devices that live outside it.
#[cfg(hv)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Hosts<'a> {
    pub vsock: Option<&'a VsockHost>,
    pub net: Option<&'a crate::devices::virtio::net::NetHost>,
    /// Each virtio-fs device's slot, in order (D38).
    pub shares: &'a [crate::devices::virtio::fs::Share],
    /// The join disk's ranges, which its VM process gives joining containers' images
    /// (D119).
    pub join: Option<&'static crate::devices::virtio::block::Join>,
    /// The join share's slot (D119), which a server of its VM's joiners' volumes fills.
    #[cfg(unix)]
    pub join_share: Option<&'static crate::devices::virtio::fs::Slot>,
    /// The microVM's I/O limits (D178), which its disks and shared directories charge.
    pub io: Option<&'static crate::devices::virtio::limit::Limiter>,
    /// The run's writable layer (D190), where the machine has one: a boot's, or a
    /// restore's, its head the snapshot's and its file system the restore's own copy.
    pub layer: Option<&'a Arc<pmem::Layer>>,
    /// The heads of its DAX windows a snapshot kept (D189), which a restore maps privately;
    /// none for a boot's, whose are its own memory.
    pub window_heads: Option<&'a std::fs::File>,
}

/// A snapshot's vsock and network devices, and the restore's host sides for them, come
/// together or not at all.
#[cfg(hv)]
fn check_hosts(snap: &crate::snapshot::Snapshot, hosts: Hosts<'_>) -> Result<(), String> {
    match (snap.config.vsock, hosts.vsock) {
        (true, None) => {
            return Err(
                "the snapshot has a vsock device: give the restored VM its own socket with --vsock PATH"
                    .into(),
            );
        }
        (false, Some(_)) => return Err("the snapshot has no vsock device for --vsock".into()),
        _ => {}
    }
    match (snap.config.net, hosts.net) {
        (Some(_), None) => {
            return Err(
                "the snapshot has a network device: give the restored VM its own network process".into(),
            );
        }
        (None, Some(_)) => return Err("the snapshot has no network device for a network process".into()),
        _ => {}
    }
    if snap.config.shares as usize != hosts.shares.len() {
        return Err(format!(
            "the snapshot has {} shared directories; the restore gives {}",
            snap.config.shares,
            hosts.shares.len()
        ));
    }
    match (snap.config.layer_mib, hosts.layer) {
        (0, Some(_)) => return Err("the snapshot has no writable layer for the restore's".into()),
        (mib, None) if mib > 0 => {
            return Err("the snapshot has a writable layer: give the restored VM its own copy of it".into());
        }
        (mib, Some(layer)) if layer.data_len() != mib << 20 => {
            return Err(format!(
                "a writable layer of {} bytes for a snapshot whose was {mib} MiB",
                layer.data_len()
            ));
        }
        _ => {}
    }
    match (has_windows(&snap.config), hosts.window_heads) {
        (true, None) => return Err("the snapshot has DAX windows: give the restored VM their heads".into()),
        (false, Some(_)) => return Err("the snapshot has no DAX windows for the restore's heads".into()),
        _ => {}
    }
    match (snap.config.join, hosts.join) {
        (true, None) => return Err("the snapshot has a join disk: give the restored VM its own".into()),
        (false, Some(_)) => return Err("the snapshot has no join disk for the restore's".into()),
        _ => {}
    }
    #[cfg(unix)]
    match (snap.config.join_share, hosts.join_share) {
        (true, None) => return Err("the snapshot has a join share: give the restored VM its own".into()),
        (false, Some(_)) => return Err("the snapshot has no join share for the restore's".into()),
        _ => {}
    }
    Ok(())
}

/// pmem regions, each with the guest address it maps at.
#[cfg(hv)]
type Regions = Vec<(Arc<pmem::Region>, u64)>;

/// `config`'s pmem files, opened, each at the guest address it maps at: from `first`, in
/// order, each right after the one before; and the address past the last. Opened before
/// the VM, so that on every early return they outlive it.
#[cfg(hv)]
fn pmem_regions(config: &crate::snapshot::MachineConfig, first: u64) -> Result<(Regions, u64), String> {
    let mut regions = Vec::with_capacity(config.pmem.len());
    let mut next = first;
    for path in &config.pmem {
        let region = Arc::new(pmem::Region::open(path)?);
        let gpa = next;
        next = gpa
            .checked_add(region.len() as u64)
            .ok_or("pmem regions overflow the address space")?;
        regions.push((region, gpa));
    }
    Ok((regions, next))
}

/// A writable layer and the guest address its device is at (D190).
#[cfg(hv)]
type Placed = (Arc<pmem::Layer>, u64);

/// Where the machine's writable layer goes (D190), if it has one: at the first GiB boundary
/// at or past `top`, a section's start ([`pmem::SECTION`]), as its head is sized for; and
/// the address past it, or `top` where it has none.
#[cfg(hv)]
fn layer_layout(layer: Option<&Arc<pmem::Layer>>, top: u64) -> Result<(Option<Placed>, u64), String> {
    let Some(layer) = layer else {
        return Ok((None, top));
    };
    const GIB: u64 = 1 << 30;
    let overflow = || "the writable layer overflows the address space".to_string();
    let at = top.checked_next_multiple_of(GIB).ok_or_else(overflow)?;
    let end = at.checked_add(layer.len()).ok_or_else(overflow)?;
    Ok((Some((layer.clone(), at)), end))
}

/// Maps the writable layer at `gpa` into `vm` (D190): its head and its file system, both
/// writable by the guest; the layer is told where, and given the hypervisor's stage 2 to
/// map its head back through ([`pmem::Layer::remap_head`]).
#[cfg(hv)]
fn map_layer(vm: &crate::hv::Vm, layer: &pmem::Layer, gpa: u64) -> Result<(), String> {
    for (at, host, len) in layer.mappings(gpa)? {
        // SAFETY: the layer outlives the VM: its device sits on the bus, which `Machine`
        // and `Running` drop after the VM.
        unsafe { vm.map_device_memory(host, at, len, true) }.map_err(|e| e.to_string())?;
    }
    layer.placed(gpa, Box::new(vm.stage2()));
    Ok(())
}

/// The run's writable layer `cfg` boots with (D190): its file opened for writing, its head
/// the VM's own memory.
#[cfg(hv)]
fn boot_layer(cfg: &Config) -> Result<Option<Arc<pmem::Layer>>, String> {
    let Some(path) = &cfg.layer else {
        return Ok(None);
    };
    let file = crate::platform::open_input(path, true).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some(Arc::new(pmem::Layer::boot(file)?)))
}

/// The memory `config` may have plugged (D133), `(most, region)` in bytes: its
/// `hotplug_mib`, whole blocks of [`mem::BLOCK`], and the region that holds it, whole
/// blocks of Linux's memory ([`mem::ALIGN`]); none where it has none.
#[cfg(hv)]
fn hotplug(config: &crate::snapshot::MachineConfig) -> Result<Option<(u64, u64)>, String> {
    if config.hotplug_mib == 0 {
        return Ok(None);
    }
    let too_much = || format!("{} MiB of memory to plug", config.hotplug_mib);
    let most = config.hotplug_mib.checked_mul(1 << 20).ok_or_else(too_much)?;
    if !most.is_multiple_of(mem::BLOCK) {
        return Err(format!(
            "memory to plug must be whole blocks of {} MiB",
            mem::BLOCK >> 20
        ));
    }
    let region = most.checked_next_multiple_of(mem::ALIGN).ok_or_else(too_much)?;
    Ok(Some((most, region)))
}

/// A boot's guest RAM, `ranges`, on small pages but its kernel's image, `image`, whose whole
/// huge pages the VMM writes as it copies it in: its kernel's boot touches other huge
/// pages in part, which a huge page would hold resident whole (D29, PM M279). The rest
/// goes on huge pages once its PID 1 runs ([`huge_pages_when_init_runs`]). Where the host
/// puts every process's memory on huge pages (THP `always`), Firecracker's too, a boot's
/// stays on them throughout: there small pages cost a boot more time than their memory
/// saves against what its host gives every process (PM M279).
#[cfg(hv)]
fn boot_pages(
    memory: &GuestMemory,
    ranges: &[(u64, usize)],
    image: std::ops::Range<u64>,
) -> Result<(), String> {
    let Some(huge) = crate::platform::advised_huge_page() else {
        return Ok(());
    };
    if crate::platform::thp_always() {
        return Ok(());
    }
    for &(gpa, len) in ranges {
        memory
            .small_pages(gpa, len)
            .map_err(|e| format!("guest RAM: {e}"))?;
    }
    let huge = huge as u64;
    let (start, end) = (image.start.next_multiple_of(huge), image.end / huge * huge);
    if end > start {
        let len = usize::try_from(end - start).map_err(|_| format!("a kernel of {} bytes", end - start))?;
        memory
            .huge_pages(start, len)
            .map_err(|e| format!("guest RAM: {e}"))?;
    }
    Ok(())
}

/// Has a boot's guest RAM put on huge pages past its boot as its PID 1 first runs
/// (`GuestMemory::huge_pages_past_boot`), on a thread of its own: the vCPU that says so
/// goes on. SHARDS_THP_ADVICE=boot leaves it out, the boot's advice alone, for measuring
/// what each buys (PM M279).
#[cfg(hv)]
fn huge_pages_when_init_runs(control: &crate::devices::control::Control, memory: &Arc<GuestMemory>) {
    if crate::platform::advised_huge_page().is_none()
        || crate::platform::thp_always()
        || std::env::var_os("SHARDS_THP_ADVICE").is_some_and(|a| a == "boot")
    {
        return;
    }
    let memory = Arc::clone(memory);
    control.on_init_started(Box::new(move || {
        let spawned = std::thread::Builder::new()
            .name("huge-pages".into())
            .spawn(move || {
                if let Err(e) = memory.huge_pages_past_boot() {
                    crate::debug!("huge pages past its boot: {e}");
                }
            });
        if let Err(e) = spawned {
            crate::debug!("huge pages past its boot: {e}");
        }
    }));
}

/// Guest RAM of `ranges`, and the memory `config` may have plugged at `at`, withheld until
/// plugged (D133).
#[cfg(hv)]
fn guest_memory(
    ranges: &[(u64, usize)],
    config: &crate::snapshot::MachineConfig,
    at: u64,
) -> Result<crate::memory::GuestMemory, String> {
    let memory = match hotplug(config)? {
        None => crate::memory::GuestMemory::anonymous(ranges),
        Some((_, region)) => {
            let len = usize::try_from(region).map_err(|_| format!("{region} bytes of memory to plug"))?;
            crate::memory::GuestMemory::with_hotplug(ranges, at, len, mem::BLOCK)
        }
    };
    memory.map_err(|e| format!("guest RAM: {e}"))
}

/// Guest RAM of `ranges` from a snapshot's memory `file`, and, as boot made it, the memory
/// `config` may have plugged at `at`, none of it plugged yet: what the snapshot's machine
/// had plugged is plugged again once the VM exists ([`replug`]).
#[cfg(hv)]
fn restored_memory(
    ranges: &[(u64, usize)],
    file: &std::fs::File,
    config: &crate::snapshot::MachineConfig,
    at: u64,
) -> Result<crate::memory::GuestMemory, String> {
    let memory =
        crate::memory::GuestMemory::from_file(ranges, file).map_err(|e| format!("snapshot memory: {e}"))?;
    match hotplug(config)? {
        None => Ok(memory),
        Some((_, region)) => {
            let len = usize::try_from(region).map_err(|_| format!("{region} bytes of memory to plug"))?;
            memory
                .with_plug_region(at, len, mem::BLOCK)
                .map_err(|e| format!("snapshot memory: {e}"))
        }
    }
}

/// The memory `snap`'s machine had plugged, plugged again in `memory` and read back from
/// the snapshot's memory `file` (D176), given to the guest through `vm`'s stage 2 as a plug
/// gives it: once the VM exists, and before any vCPU runs.
#[cfg(hv)]
fn replug(
    memory: &crate::memory::GuestMemory,
    snap: &crate::snapshot::Snapshot,
    file: &std::fs::File,
    vm: &crate::hv::Vm,
) -> Result<(), String> {
    if snap.plugged.is_empty() {
        return Ok(());
    }
    let stage2 = vm.stage2();
    memory
        .restore_plugged(&snap.plugged, file, |host, gpa, len| {
            // SAFETY: the memory to plug stays mapped in this process as long as the guest
            // memory does, which outlives the VM.
            unsafe { mem::Mapper::map(&stage2, host, gpa, len) }
        })
        .map_err(|e| format!("snapshot memory: {e}"))
}

#[cfg(hv)]
impl mem::Mapper for crate::hv::Stage2 {
    unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize) -> Result<(), String> {
        // SAFETY: forwarded caller contract.
        unsafe { crate::hv::Stage2::map(self, host, gpa, len) }.map_err(|e| e.to_string())
    }

    fn unmap(&self, gpa: u64, len: usize) -> Result<(), String> {
        crate::hv::Stage2::unmap(self, gpa, len).map_err(|e| e.to_string())
    }
}

#[cfg(hv)]
impl crate::devices::virtio::fs::window::Stage2 for crate::hv::Stage2 {
    unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize, writable: bool) -> Result<(), String> {
        // SAFETY: forwarded caller contract.
        unsafe { crate::hv::Stage2::map_file(self, host, gpa, len, writable) }.map_err(|e| e.to_string())
    }

    fn unmap(&self, gpa: u64, len: usize) -> Result<(), String> {
        crate::hv::Stage2::unmap(self, gpa, len).map_err(|e| e.to_string())
    }
}

#[cfg(hv)]
impl pmem::Slots for crate::hv::DeviceSlots {
    unsafe fn map(&self, i: u32, host: *mut u8, gpa: u64, len: usize, writable: bool) -> Result<(), String> {
        // SAFETY: forwarded caller contract.
        unsafe { crate::hv::DeviceSlots::map(self, i, host, gpa, len, writable) }.map_err(|e| e.to_string())
    }

    fn unmap(&self, i: u32, gpa: u64, len: usize) -> Result<(), String> {
        crate::hv::DeviceSlots::unmap(self, i, gpa, len).map_err(|e| e.to_string())
    }
}

/// The virtio-mem device of `memory`'s memory to plug, and the sizes the control page
/// shares with it (D133); none where it has none. The region is the VM's from here on,
/// mapped as the hypervisor maps it ([`crate::hv::Vm::add_hotplug`]).
#[cfg(hv)]
#[expect(
    clippy::type_complexity,
    reason = "the device and the sizes the control page shares with it, made together"
)]
fn hotplug_device(
    vm: &crate::hv::Vm,
    memory: &crate::memory::GuestMemory,
    config: &crate::snapshot::MachineConfig,
) -> Result<Option<(Box<dyn crate::devices::virtio::VirtioDevice>, Arc<mem::Sizes>)>, String> {
    let (Some((gpa, host, len)), Some((most, _))) = (memory.hotplug(), hotplug(config)?) else {
        return Ok(None);
    };
    // SAFETY: `memory` outlives the VM: `Machine` and `Running` drop the VM first.
    let stage2 = unsafe { vm.add_hotplug(host, gpa, len) }.map_err(|e| e.to_string())?;
    let sizes = Arc::new(mem::Sizes::new(gpa, len as u64, most));
    Ok(Some((Box::new(mem::Mem::new(sizes.clone(), stage2)), sizes)))
}

/// Where `config`'s swap window goes (D159), if it has one: at the first GiB boundary at or
/// past `top`, the end of everything else the guest is given, and no longer than the
/// guest's addresses reach (`limit`, the first past them): `((gpa, len), end)`, `end` past
/// the window, or `top` where there is none. A window cut short holds less swap than a
/// run's limits allow, as a host's swap device smaller than a container's allowance does;
/// the run's file is cut to it (`pmem::SwapWindow::attach`).
#[cfg(hv)]
fn swap_layout(
    config: &crate::snapshot::MachineConfig,
    top: u64,
    limit: u64,
) -> Result<(Option<(u64, u64)>, u64), String> {
    const GIB: u64 = 1 << 30;
    if config.swap_mib == 0 {
        return Ok((None, top));
    }
    let overflow = || {
        format!(
            "a swap window of {} MiB overflows the address space",
            config.swap_mib
        )
    };
    let asked = config.swap_mib.checked_mul(1 << 20).ok_or_else(overflow)?;
    let at = top.checked_next_multiple_of(GIB).ok_or_else(overflow)?;
    let room = limit.saturating_sub(at) / pmem::ALIGN * pmem::ALIGN;
    let len = asked.min(room);
    if len < asked {
        crate::warn!(
            "a swap window of {} MiB where the guest's addresses leave {} MiB",
            asked >> 20,
            room >> 20
        );
    }
    if len < pmem::ALIGN {
        return Ok((None, top));
    }
    let end = at.checked_add(len).ok_or_else(overflow)?;
    Ok((Some((at, len)), end))
}

/// Whether `config`'s machine has DAX windows (D189): a length for them, and a shared
/// directory or a join share to give one.
#[cfg(hv)]
pub(crate) fn has_windows(config: &crate::snapshot::MachineConfig) -> bool {
    config.share_window_mib > 0 && (config.shares > 0 || config.join_share)
}

/// A shared directory's DAX window's place in its guest (D189), `(guest address, length)`
/// of each part: its head ([`head::window_head_len`]), at a GiB boundary, a section's start,
/// the window right after it, and its held record right after that
/// ([`head::window_held_len`]).
#[cfg(hv)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowAt {
    head: (u64, u64),
    window: (u64, u64),
    held: (u64, u64),
}

/// Each shared directory's DAX window (D189), its share's first and the join share's last,
/// each at a GiB boundary from `top` on, its head first, within `limit`; where the last
/// ends; and their length in MiB. Each as long as `config` asks where all of them fit
/// within `limit`, else, unless they are `kept` (a snapshot's, its heads kept for that
/// length), as long as all of them fit, whole 2 MiB ranges, as the swap window takes what
/// the guest's addresses leave it (`swap_layout`); none where not one range each fits.
/// None where `config` gives no window.
#[cfg(hv)]
fn share_windows(
    config: &crate::snapshot::MachineConfig,
    top: u64,
    limit: u64,
    kept: bool,
) -> Result<(Vec<WindowAt>, u64, u64), String> {
    const GIB: u64 = 1 << 30;
    let count = config.shares as usize + usize::from(config.join_share);
    if config.share_window_mib == 0 || count == 0 {
        return Ok((Vec::new(), top, 0));
    }
    let overflow = || {
        format!(
            "{count} DAX windows of {} MiB overflow the address space",
            config.share_window_mib
        )
    };
    let len = config
        .share_window_mib
        .checked_mul(1 << 20)
        .ok_or_else(overflow)?;
    // Whole 2 MiB ranges: what the guest maps a file's ranges in (fs/fuse/dax.c,
    // FUSE_DAX_SZ), and the subsections it describes device memory's pages in
    // (devm_memremap_pages, with SPARSEMEM_VMEMMAP).
    if !len.is_multiple_of(pmem::ALIGN) {
        return Err(format!(
            "a DAX window of {} MiB: whole {} MiB",
            config.share_window_mib,
            pmem::ALIGN >> 20
        ));
    }
    // `count` windows of `len` bytes each, and where the last ends, where they fit.
    let place = |len: u64| -> Result<Option<(Vec<WindowAt>, u64)>, String> {
        let head = head::window_head_len(len)?;
        let held = head::window_held_len(len)?;
        let mut windows = Vec::with_capacity(count);
        let mut next = top;
        for _ in 0..count {
            let Some((at, window, record, end)) = next.checked_next_multiple_of(GIB).and_then(|at| {
                let window = at.checked_add(head)?;
                let record = window.checked_add(len)?;
                Some((at, window, record, record.checked_add(held)?))
            }) else {
                return Ok(None);
            };
            windows.push(WindowAt {
                head: (at, head),
                window: (window, len),
                held: (record, held),
            });
            next = end;
        }
        Ok((next <= limit).then_some((windows, next)))
    };
    if let Some((windows, end)) = place(len)? {
        return Ok((windows, end, config.share_window_mib));
    }
    if kept {
        return Err(overflow());
    }
    // The most whole ranges each that fit, `fits` (0, none) to `over`, which do not: where
    // the windows end only grows with their length (head and window whole sections, the
    // held record whole ranges, each at its GiB).
    let (mut fits, mut over) = (0, len / pmem::ALIGN);
    while over - fits > 1 {
        let mid = fits + (over - fits) / 2;
        match place(mid * pmem::ALIGN)? {
            Some(_) => fits = mid,
            None => over = mid,
        }
    }
    let fitted = match fits {
        0 => None,
        n => place(n * pmem::ALIGN)?.map(|(windows, end)| (windows, end, n * pmem::ALIGN)),
    };
    let Some((windows, end, fitted)) = fitted else {
        crate::warn!(
            "no DAX window for its {count} shared directories: the guest's addresses leave none a 2 MiB range"
        );
        return Ok((Vec::new(), top, 0));
    };
    crate::warn!(
        "{count} DAX windows of {} MiB each where {} MiB were asked: the guest's addresses leave no more",
        fitted >> 20,
        config.share_window_mib
    );
    Ok((windows, end, fitted >> 20))
}

/// The heads of `windows` (D189), one after another in one head: a snapshot's, `kept`,
/// mapped privately, or a boot's, memory of the VM's own; none where it has no windows.
#[cfg(hv)]
fn window_heads(
    windows: &[WindowAt],
    kept: Option<&std::fs::File>,
) -> Result<Option<Arc<head::WindowHeads>>, String> {
    let Some(first) = windows.first() else {
        return Ok(None);
    };
    let at = |e: String| format!("its DAX windows' heads: {e}");
    let each = usize::try_from(first.head.1).map_err(|_| at(format!("a head of {} bytes", first.head.1)))?;
    let len = each
        .checked_mul(windows.len())
        .ok_or_else(|| at(format!("{} heads of {each} bytes", windows.len())))?;
    let head = match kept {
        Some(kept) => head::Head::restore(kept.try_clone().map_err(|e| at(e.to_string()))?, len),
        None => head::Head::new(len).inspect(|head| {
            // On the host's base pages, as a restored head is: its guest writes a range's
            // page structures as it first maps the range (shards' kernel patch 0011), 32 KiB
            // of a 2 MiB block, which a huge page would hold whole (2 MiB for the one page
            // structure a boot writes, on an x86_64 CI runner with THP always; PM M307).
            crate::platform::small_pages(head.host(), head.len());
        }),
    }
    .map_err(at)?;
    let heads = head::WindowHeads::new(head, windows.iter().map(|w| w.head.0).collect(), each).map_err(at)?;
    Ok(Some(Arc::new(heads)))
}

/// Maps the DAX windows' heads into `vm` (D189), writable by the guest, each at its
/// window's head; the heads are given the hypervisor's stage 2 to be mapped back through
/// ([`head::WindowHeads::remap`]).
#[cfg(hv)]
fn map_window_heads(vm: &crate::hv::Vm, heads: &head::WindowHeads) -> Result<(), String> {
    for (gpa, host, len) in heads.mappings() {
        // SAFETY: the heads outlive the VM: the control page holds them, which sits on the
        // bus, and `Machine` and `Running` drop the bus after the VM.
        unsafe { vm.map_device_memory(host, gpa, len, true) }.map_err(|e| e.to_string())?;
    }
    heads.placed(Box::new(vm.stage2()));
    Ok(())
}

/// A share's DAX window (D189) at `at`: its host side reserved, and on KVM one memslot over
/// it, as memory to plug's (`add_hotplug`), which each range its guest maps is placed in;
/// on HVF each range mapped at stage 2 as it is placed; on WHP each range mapped at stage 2
/// as it is first placed and left standing, as stage 2 follows the host's view there (PM
/// M341). Its held record is mapped right after it, writable.
#[cfg(hv)]
fn dax_window(
    vm: Option<&crate::hv::Vm>,
    at: &WindowAt,
) -> Result<Arc<crate::devices::virtio::fs::window::Window>, String> {
    let vm = vm.ok_or("a DAX window with no VM to map it into")?;
    let (gpa, len) = at.window;
    let size = usize::try_from(len).map_err(|_| format!("a DAX window of {len} bytes"))?;
    let window = crate::devices::virtio::fs::window::Window::new(gpa, len, |host| {
        // SAFETY: the window's reservation stays mapped until the window drops, which its
        // device holds, after the VM (`Machine` and `Running` drop the VM first).
        let stage2 = unsafe { vm.add_hotplug(host, gpa, size) }.map_err(|e| e.to_string())?;
        Ok(Box::new(stage2) as Box<dyn crate::devices::virtio::fs::window::Stage2>)
    })?;
    let (host, held) = window.held();
    if held as u64 != at.held.1 {
        return Err(format!(
            "a DAX window's record of {held} bytes for {} given it",
            at.held.1
        ));
    }
    // SAFETY: as the window's own, the record stays mapped until the window drops.
    unsafe { vm.map_device_memory(host, at.held.0, held, true) }.map_err(|e| e.to_string())?;
    Ok(Arc::new(window))
}

/// The swap window of `layout` (D159), its head mapped through slots of its own, which the
/// control page shares with its device; none where it has none.
#[cfg(hv)]
fn swap_window(
    vm: &crate::hv::Vm,
    layout: Option<(u64, u64)>,
) -> Result<Option<Arc<pmem::SwapWindow>>, String> {
    let Some((gpa, len)) = layout else {
        return Ok(None);
    };
    let slots = Box::new(vm.device_slots(pmem::SWAP_SLOTS));
    Ok(Some(Arc::new(pmem::SwapWindow::new(gpa, len, slots)?)))
}

/// The virtio-balloon device that gives the host back the memory the guest reports free
/// (D151), where `config` has one; it takes guest RAM from the guest and gives it back
/// through the VM's stage 2.
#[cfg(hv)]
fn reporting_device(
    vm: &crate::hv::Vm,
    config: &crate::snapshot::MachineConfig,
) -> Option<Box<dyn crate::devices::virtio::VirtioDevice>> {
    config.reporting.then(|| {
        Box::new(crate::devices::virtio::balloon::Balloon::new(vm.stage2()))
            as Box<dyn crate::devices::virtio::VirtioDevice>
    })
}

/// `config`'s virtio devices in the guest's probe order, which each machine gives the
/// next of its MMIO windows and interrupt lines: its disks, its join disk, its pmem
/// `regions`, its writable `layer` and where it is, its vsock device, its network device, its shared directories, its join share,
/// and the machine's own devices, `last`: its GPU, its memory to plug, then its balloon (last of
/// all: no other device's window moves for them); at most `max` of them. The devices share the regions:
/// `regions`, made before the VM, holds them until after it on every early return, the
/// hypervisor mapping them until its destroy.
#[cfg(hv)]
#[allow(clippy::too_many_arguments)]
fn virtio_devices(
    config: &crate::snapshot::MachineConfig,
    regions: &[(Arc<pmem::Region>, u64)],
    layer: Option<&Placed>,
    // The shares' windows, which only Unix hosts build, and the VM they map into.
    #[cfg_attr(not(unix), allow(unused_variables))] windows: &[WindowAt],
    #[cfg_attr(not(unix), allow(unused_variables))] vm: Option<&crate::hv::Vm>,
    hosts: Hosts<'_>,
    last: Vec<Box<dyn crate::devices::virtio::VirtioDevice>>,
    max: u64,
) -> Result<Vec<Box<dyn crate::devices::virtio::VirtioDevice>>, String> {
    let slots = config.disks.len()
        + usize::from(config.join)
        + regions.len()
        + usize::from(layer.is_some())
        + usize::from(config.vsock)
        + usize::from(config.net.is_some())
        + config.shares as usize
        + usize::from(config.join_share)
        + last.len();
    if slots as u64 > max {
        return Err(format!("at most {max} virtio devices are supported"));
    }
    let mut devices: Vec<Box<dyn crate::devices::virtio::VirtioDevice>> = Vec::with_capacity(slots);
    for (i, (path, read_only)) in config.disks.iter().enumerate() {
        devices.push(Box::new(
            crate::devices::virtio::block::Block::open(path, *read_only, &format!("shards-disk{i}"))?
                .with_limit(hosts.io),
        ));
    }
    // Its join disk, the virtio-blk device after its disks, which init finds by its
    // serial (D119).
    if config.join {
        let disk = hosts
            .join
            .ok_or("the machine has a join disk but no host side for it")?;
        devices.push(Box::new(
            crate::devices::virtio::block::Block::join(disk, shards_abi::JOIN_DISK_SERIAL)
                .with_limit(hosts.io),
        ));
    }
    for (region, gpa) in regions {
        devices.push(Box::new(pmem::Pmem::new(region.clone(), *gpa)));
    }
    // Its writable layer, the pmem device after its files (D190), which init finds by its
    // address (`control::LAYER`).
    if let Some((layer, gpa)) = layer {
        devices.push(Box::new(pmem::Pmem::layer(layer.clone(), *gpa)));
    }
    if config.vsock {
        let host = hosts
            .vsock
            .ok_or("the machine has a vsock device but no host side for it")?;
        devices.push(Box::new(vsock::Vsock::new(host.clone(), vsock::GUEST_CID)?));
    }
    if let Some(mac) = config.net {
        let mut host = hosts
            .net
            .ok_or("the machine has a network device but no network process for it")?
            .clone();
        // The machine's own MAC, a snapshot's included: its guest was set up with it.
        host.mac = mac;
        devices.push(Box::new(crate::devices::virtio::net::Net::new(host)?));
    }
    // Then its shared directories, each mounted by its tag, `shards0` and on.
    for i in 0..config.shares as usize {
        let slot = hosts
            .shares
            .get(i)
            .ok_or("the machine has a shared directory but no slot for it")?
            .clone();
        let window = windows
            .get(i)
            .map(|w| dax_window(vm, w).map(|window| (window, w.head, w.held)))
            .transpose()?;
        devices.push(Box::new(
            crate::devices::virtio::fs::Fs::new(&format!("shards{i}"), slot)?
                .with_limit(hosts.io)
                .with_window(window),
        ));
    }
    // Its join share, after them, which init mounts by its tag once a joiner brings
    // volumes (D119).
    #[cfg(unix)]
    if config.join_share {
        let slot = hosts
            .join_share
            .ok_or("the machine has a join share but no slot for it")?;
        let window = windows
            .get(config.shares as usize)
            .map(|w| dax_window(vm, w).map(|window| (window, w.head, w.held)))
            .transpose()?;
        devices.push(Box::new(
            crate::devices::virtio::fs::Fs::join(shards_abi::JOIN_SHARE_TAG, slot)?
                .with_limit(hosts.io)
                .with_window(window),
        ));
    }
    devices.extend(last);
    Ok(devices)
}

/// A GPU's renderer, and the length of the window its blobs are mapped into: what the
/// host's GPU says it has of memory the host can map, which Venus tells the guest, the most
/// a guest can have mapped at once (D144).
#[cfg(all(hv = "hvf", feature = "gpu"))]
fn gpu_renderer() -> Result<(Box<dyn gpu::Renderer>, u64), String> {
    let venus = gpu::virgl::Venus::new()?;
    let len = venus.host_visible();
    Ok((Box::new(venus), len))
}

#[cfg(all(hv = "hvf", not(feature = "gpu")))]
fn gpu_renderer() -> Result<(Box<dyn gpu::Renderer>, u64), String> {
    Err("this VM process renders no GPU: shards' GPU VM process does, on macOS (D144)".into())
}

#[cfg(hv = "hvf")]
impl gpu::Window for crate::hv::Window {
    unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize) -> Result<(), String> {
        // SAFETY: forwarded caller contract.
        unsafe { crate::hv::Window::map(self, host, gpa, len) }.map_err(|e| e.to_string())
    }

    fn unmap(&self, gpa: u64, len: usize) -> Result<(), String> {
        crate::hv::Window::unmap(self, gpa, len).map_err(|e| e.to_string())
    }
}

/// Where the serial console's output goes.
#[cfg(hv)]
fn console_out(console: Console) -> Result<Box<dyn std::io::Write + Send>, String> {
    Ok(match console {
        Console::Stdout => Box::new(crate::platform::stdout_file().map_err(|e| format!("console: {e}"))?),
        Console::Discard => Box::new(std::io::sink()),
    })
}

/// `cfg`'s machine, its backing files resolved once to absolute paths: a snapshot of it
/// names the files the machine had, wherever it is restored from (audit A18).
#[cfg(hv)]
fn machine_config(cfg: &Config) -> Result<crate::snapshot::MachineConfig, String> {
    let resolve = |path: &std::path::Path| {
        crate::platform::input_path(path).map_err(|e| format!("{}: {e}", path.display()))
    };
    Ok(crate::snapshot::MachineConfig {
        vcpus: cfg.vcpus,
        memory_mib: cfg.memory_mib,
        disks: cfg
            .disks
            .iter()
            .map(|d| Ok((resolve(&d.path)?, d.read_only)))
            .collect::<Result<_, String>>()?,
        pmem: cfg
            .pmem
            .iter()
            .map(|p| resolve(p))
            .collect::<Result<_, String>>()?,
        vsock: cfg.vsock.is_some(),
        net: cfg.net.as_ref().map(|n| n.mac),
        shares: u32::try_from(cfg.shares.len()).map_err(|_| "too many shared directories")?,
        join: cfg.join.is_some(),
        #[cfg(unix)]
        join_share: cfg.join_share.is_some(),
        #[cfg(not(unix))]
        join_share: false,
        nested: cfg.nested,
        hotplug_mib: cfg.hotplug_mib,
        reporting: cfg.reporting,
        // Its window's length, once the machine has its renderer (aarch64.rs `build`).
        gpu: None,
        swap_mib: cfg.swap_mib,
        share_window_mib: cfg.share_window_mib,
        // Its file system's length, once its layer is open (`boot_layer`).
        layer_mib: 0,
    })
}

/// `working_set` if every page of it, `page` bytes long, lies in one of `ranges`, the
/// `(guest address, length)` of the guest's RAM and pmem. Else nothing: the restore goes
/// without it rather than prefetch what the guest does not have (audit A16).
#[cfg(hv)]
fn usable_working_set(
    working_set: Vec<crate::hv::Touch>,
    page: u64,
    ranges: &[(u64, u64)],
) -> Vec<crate::hv::Touch> {
    let inside = |gpa: u64| {
        gpa.checked_add(page).is_some_and(|end| {
            ranges
                .iter()
                .any(|&(start, len)| gpa >= start && start.checked_add(len).is_some_and(|stop| end <= stop))
        })
    };
    match working_set.iter().find(|t| !inside(t.gpa)) {
        None => working_set,
        Some(t) => {
            crate::warn!(
                "the working set names {:#x}, outside the guest's memory; restoring without prefetching it",
                t.gpa
            );
            Vec::new()
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub kernel: PathBuf,
    /// A prebuilt initramfs image.
    pub initrd: Option<PathBuf>,
    /// A guest executable to run as PID 1 from a generated initramfs (exclusive with
    /// `initrd`).
    pub init: Option<PathBuf>,
    pub cmdline: String,
    pub vcpus: u32,
    pub memory_mib: u64,
    /// Where guest console (UART) output goes.
    pub console: Console,
    /// virtio-blk disks, in the order the guest enumerates them (vda, vdb, ...).
    pub disks: Vec<Disk>,
    /// What to do when the guest asks for a snapshot; without it, requests are ignored.
    pub snapshot: Option<SnapshotPolicy>,
    /// Read-only virtio-pmem devices backed by these files, in guest order (pmem0, ...).
    pub pmem: Vec<PathBuf>,
    /// A virtio-vsock device, and where its host side is.
    pub vsock: Option<VsockHost>,
    /// A virtio-net device, and the network process's side of it (D31).
    pub net: Option<crate::devices::virtio::net::NetHost>,
    /// virtio-fs devices, each a slot its directory's server is put in (D38).
    pub shares: Vec<crate::devices::virtio::fs::Share>,
    /// A join disk, after the disks: empty until the VM process gives a joining
    /// container's image a range of it (D119).
    pub join: Option<&'static crate::devices::virtio::block::Join>,
    /// Virtualization for the guest itself (nested): on arm64 its vCPUs have EL2, so
    /// that its kernel may be a hypervisor (KVM). Refused where the host cannot give it,
    /// and its snapshot refused, its EL2 state not saved.
    pub nested: bool,
    /// Memory the guest may have plugged beyond `memory_mib`, in MiB, as it asks for it
    /// through the control page: a virtio-mem device (D133), last of its devices; 0 for
    /// none. Its snapshot keeps none of it plugged (D148).
    pub hotplug_mib: u64,
    /// A virtio-balloon device that gives the host back the memory the guest reports free
    /// (D151), after the memory to plug: last of its devices.
    pub reporting: bool,
    /// The join share's slot (D119), which a server of its VM's joiners' volumes fills.
    #[cfg(unix)]
    pub join_share: Option<&'static crate::devices::virtio::fs::Slot>,
    /// Its I/O limits (D178), which its disks and shared directories charge: its process's,
    /// given their rates with its run.
    pub io_limit: Option<&'static crate::devices::virtio::limit::Limiter>,
    /// A virtio-gpu device, Vulkan through Venus (D144), rendered in this process: a VM
    /// process built with the renderer (`shards-vm-gpu`) has one. Its snapshot is refused
    /// once the guest has made a context or a resource, which are its host driver's and no
    /// snapshot holds; a template is saved before any (D150).
    pub gpu: bool,
    /// A swap window of this many MiB (D159): a virtio-pmem device, last of its devices, a
    /// run's swap file mapped into it as it is given one; 0 for none. Hidden from the guest
    /// until its init shows it (`control::SHOW_SWAP`).
    pub swap_mib: u64,
    /// A DAX window of this many MiB for each shared directory (D189): virtio-fs's cache, a
    /// shared memory region its guest maps the files it reads into; 0 for none.
    pub share_window_mib: u64,
    /// The run's writable layer (D190): its ext4 file system's file, which the guest
    /// writes, a pmem device after the pmem files.
    pub layer: Option<PathBuf>,
    /// A PCI bus with test functions on it, QEMU's edu's identification and liveness
    /// registers (D153), at each of these places on bus 0, `(device, function)`: what
    /// tests read to show that the bus reaches the VMM. A machine with a PCI bus saves no
    /// snapshot.
    pub pci_test: Vec<(u8, u8)>,
    /// Host PCI functions passed through by VFIO, Linux's (D154), on the PCI bus after the
    /// test function. A machine given one outside its own IOMMU has no memory to plug and
    /// no balloon: the function's DMA reaches all of guest RAM through mappings the host's
    /// IOMMU holds pinned, which neither may take from under it (as QEMU's
    /// `ram_block_discard_disable`). Behind the guest's IOMMU ([`Vfio::iommu`]), it reaches
    /// what the guest maps for it alone, pinned as it is mapped, which guest memory keeps
    /// from being given back meanwhile ([`crate::memory::GuestMemory::pin`]).
    pub vfio: Vec<Vfio>,
}

/// A host PCI function a VM is given (D154): its name as the host has it (`0000:01:00.0`)
/// and its IOMMU group's VFIO device, found before the VM process is confined
/// ([`Vfio::of`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vfio {
    pub name: String,
    pub group: PathBuf,
    /// Whether the group has no IOMMU (vfio's `enable_unsafe_noiommu_mode`): its function
    /// may make no DMA, and guest RAM is mapped for none.
    pub noiommu: bool,
    /// Its 64-bit memory BARs' sizes, in their order: what they take of the machine's
    /// 64-bit window.
    pub bars64: Vec<u64>,
    /// Whether it sits behind the guest's own IOMMU, which the guest's VFIO needs to give
    /// it to the guest's own processes: its group then has a container of its own, which
    /// maps what the guest's IOMMU shows its endpoint (D154). Otherwise all of guest RAM is
    /// its, untranslated, as a guest's kernel driving it needs nothing else.
    pub iommu: bool,
    /// Its VFIO device node, where it is taken by that and iommufd, not its group
    /// (`/dev/vfio/devices/vfioN`; vfio.rst, "VFIO Device cdev"): `group` then names its
    /// IOMMU group, whose node it needs not.
    pub cdev: Option<PathBuf>,
}

impl Vfio {
    /// The host's function `name`, by the VFIO group it is in: it must be bound to
    /// vfio-pci, as must every other function in its IOMMU group.
    pub fn of(name: &str) -> Result<Vfio, String> {
        #[cfg(target_os = "linux")]
        {
            use crate::devices::pci::vfio;
            let (group, noiommu) = vfio::group_of(name).map_err(|e| format!("{name}: {e}"))?;
            let bars64 = vfio::bars64_of(name).map_err(|e| format!("{name}: its BARs: {e}"))?;
            Ok(Vfio {
                name: name.to_string(),
                group,
                noiommu,
                bars64,
                iommu: false,
                cdev: None,
            })
        }
        #[cfg(not(target_os = "linux"))]
        Err(format!(
            "{name}: a host's PCI devices are passed through by VFIO, Linux's (D154)"
        ))
    }

    /// The host's function whose VFIO device node is `cdev` (`/dev/vfio/devices/vfioN`),
    /// taken by that and iommufd (D154): it must be bound to vfio-pci, as must every other
    /// function in its IOMMU group.
    pub fn of_cdev(cdev: &std::path::Path) -> Result<Vfio, String> {
        #[cfg(target_os = "linux")]
        {
            use crate::devices::pci::vfio;
            let at = |e: std::io::Error| format!("{}: {e}", cdev.display());
            let name = vfio::cdev_function(cdev).map_err(at)?;
            let group = vfio::group_number(&name).map_err(|e| format!("{name}: its IOMMU group: {e}"))?;
            let bars64 = vfio::bars64_of(&name).map_err(|e| format!("{name}: its BARs: {e}"))?;
            Ok(Vfio {
                name,
                group: PathBuf::from(format!("/dev/vfio/{group}")),
                noiommu: false,
                bars64,
                iommu: false,
                cdev: Some(cdev.to_path_buf()),
            })
        }
        #[cfg(not(target_os = "linux"))]
        Err(format!(
            "{}: a host's PCI devices are passed through by VFIO, Linux's (D154)",
            cdev.display()
        ))
    }
}

/// The PCI functions a machine is given (D153, D154).
#[cfg(hv)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Pci<'a> {
    /// The test functions' places, `(device, function)`.
    pub test: &'a [(u8, u8)],
    pub vfio: &'a [Vfio],
}

#[cfg(hv)]
impl Pci<'_> {
    /// What its functions' 64-bit BARs take of the machine's 64-bit window, placed as the
    /// bus places them, the test function's and then each passed-through function's in
    /// order, each at a multiple of its size: the window's length, and the alignment of its
    /// start, the largest BAR's; none where no BAR is 64-bit.
    pub fn mem64(&self) -> Result<Option<(u64, u64)>, String> {
        let sizes = self
            .test
            .iter()
            .map(|_| crate::devices::pci::edu::SCRATCH_BAR_SIZE)
            .chain(self.vfio.iter().flat_map(|f| f.bars64.iter().copied()));
        let (mut len, mut align) = (0u64, 0u64);
        for size in sizes {
            len = len
                .checked_next_multiple_of(size)
                .and_then(|at| at.checked_add(size))
                .ok_or("the 64-bit BARs overflow the address space")?;
            align = align.max(size);
        }
        Ok((len > 0).then_some((len, align)))
    }
}

/// How a machine takes its PCI functions' interrupts (D154): the line of each device's
/// INTx pin, which its firmware tables route there, and where the MSIs of the functions
/// the VMM emulates go.
#[cfg(hv)]
pub(crate) trait PciInterrupts {
    /// The INTx line of device `device` on bus 0.
    fn intx(&self, device: u8) -> Arc<dyn crate::devices::Interrupt>;
    /// Where the function of requester ID `requester` (its device and function on bus 0,
    /// `device << 3 | function`) sends its MSIs.
    fn msi(&self, requester: u32) -> Arc<dyn crate::devices::pci::Msi>;
    /// How the function passed through at requester ID `requester` interrupts the guest,
    /// by irqfds; none where the machine routes it no interrupt.
    #[cfg(target_os = "linux")]
    fn route(&self, requester: u32) -> Option<Arc<dyn crate::devices::pci::vfio::Route>>;
}

/// A machine's PCI bus (D153, D154): each device that raises INTx, and its pin, for the
/// machine's firmware tables to route to [`PciInterrupts::intx`]'s line.
#[cfg(hv)]
#[derive(Debug, Default)]
pub(crate) struct PciBus {
    pub intx: Vec<(u8, u8)>,
}

/// What a machine gives its PCI bus: its VM, and its guest memory, which its functions
/// reach by DMA; how their interrupts reach the guest, where it routes them; and its
/// IOMMU's translation, which the test function's DMA goes through, where it has one.
#[cfg(hv)]
pub(crate) struct PciMachine<'a> {
    #[cfg_attr(
        not(target_os = "linux"),
        expect(dead_code, reason = "read by VFIO, Linux's alone")
    )]
    pub vm: &'a crate::hv::Vm,
    pub memory: &'a Arc<crate::memory::GuestMemory>,
    pub irqs: Option<&'a dyn PciInterrupts>,
    pub iommu: Option<crate::devices::virtio::iommu::Shared>,
}

/// Whether a machine's PCI functions' MSIs are isolated from each other's, as a guest's
/// VFIO needs to give a function to a process without being told it may anyway
/// (vfio_iommu_type1.c, `allow_unsafe_interrupts`; D154): on arm64 KVM, whose GIC's ITS
/// translates each MSI by the ID of the function that wrote it (Linux's irq-gic-v3-its.c
/// marks its MSIs IRQ_DOMAIN_FLAG_ISOLATED_MSI). Not yet on x86_64, whose guests have no
/// interrupt remapping, nor on Hypervisor.framework, whose GIC has a GICv2m frame alone.
pub const ISOLATED_MSIS: bool = cfg!(all(target_os = "linux", target_arch = "aarch64"));

/// The test function's device on bus 0 (D153) where a machine has one (`--pci-test`), and
/// so its requester ID, `device << 3`, which the machine's IOMMU translates (D154): the
/// first device a function takes.
pub const TEST_DEVICE: u8 = 1;

/// The first device of bus 0 after the test functions at `test` (D153).
fn after_tests(test: &[(u8, u8)]) -> u8 {
    test.iter()
        .map(|&(device, _)| device.saturating_add(1))
        .max()
        .unwrap_or(TEST_DEVICE)
}

/// Whether the functions `vfio` can be given a machine together (D154): each once, none
/// without a host's IOMMU behind the guest's, and each group's taken one way, through the
/// group or each by its device node, as VFIO gives a group one way at a time
/// (vfio_device_block_group), and all behind the guest's IOMMU or all outside it, as a
/// group's functions share the host's mappings of their DMA.
pub fn vfio_given(vfio: &[Vfio]) -> Result<(), String> {
    let way = |f: &Vfio| match f.cdev {
        Some(_) => "by its device node",
        None => "through its group",
    };
    let behind = |f: &Vfio| match f.iommu {
        true => "behind the guest's IOMMU",
        false => "outside the guest's IOMMU",
    };
    for (i, f) in vfio.iter().enumerate() {
        let named = |e: String| format!("{}: {e}", f.name);
        let before = vfio.get(..i).unwrap_or_default();
        if before.iter().any(|g| g.name == f.name) {
            return Err(named("given twice".into()));
        }
        if f.iommu && f.noiommu {
            return Err(named("a group without an IOMMU cannot follow the guest's".into()));
        }
        let Some(g) = before.iter().find(|g| g.group == f.group) else {
            continue;
        };
        if g.cdev.is_some() != f.cdev.is_some() {
            return Err(named(format!(
                "taken {}, and {} of its group {} {}: VFIO gives a group's functions one way at a time",
                way(f),
                g.name,
                f.group.display(),
                way(g)
            )));
        }
        if g.iommu != f.iommu {
            return Err(named(format!(
                "{}, and {} of its group {} {}: a group's functions share the host's mappings of their DMA",
                behind(f),
                g.name,
                f.group.display(),
                behind(g)
            )));
        }
    }
    Ok(())
}

/// Where each function of `vfio` sits on bus 0, `(device, function)`, after the test
/// functions at `test` (D153). The functions of one host IOMMU group
/// share a device, which nothing isolates from each other ([`passed_functions`]), so that
/// the guest takes them as one group too, as Linux groups a multi-function device's
/// functions (iommu.c, pci_device_group, get_pci_function_alias_group): its IOMMU core
/// then keeps them in one domain, as the host's container does, and a workload given
/// `/dev/vfio/N` finds them all in it. Where they are one host slot's with its function 0,
/// each keeps its function number; else they are numbered from 0 in the order of their
/// addresses, as a guest scans a device's other functions only where it has function 0.
/// None past the bus's 32 devices, nor for a group's functions past eight, nor for a
/// function number past 7.
pub fn vfio_places(vfio: &[Vfio], test: &[(u8, u8)]) -> Vec<Option<(u8, u8)>> {
    let slot = |name: &str| name.rsplit_once('.').map(|(slot, _)| slot.to_string());
    let number = |name: &str| {
        name.rsplit_once('.')
            .and_then(|(_, f)| u8::from_str_radix(f, 16).ok())
            .filter(|&f| f < 8)
    };
    let mut groups: Vec<&std::path::Path> = Vec::new();
    for f in vfio {
        if !groups.contains(&f.group.as_path()) {
            groups.push(&f.group);
        }
    }
    let first = after_tests(test);
    vfio.iter()
        .map(|f| {
            number(&f.name)?;
            let device = groups
                .iter()
                .position(|g| *g == f.group)
                .and_then(|i| u8::try_from(i).ok())
                .and_then(|i| first.checked_add(i))
                .filter(|&d| d < 32)?;
            // Its group's functions, by their addresses, which sysfs writes in fixed-width
            // lowercase hex: in their numbers' order.
            let mut members: Vec<&str> = vfio
                .iter()
                .filter(|g| g.group == f.group)
                .map(|g| g.name.as_str())
                .collect();
            members.sort_unstable();
            members.dedup();
            let one_slot = members.iter().all(|m| slot(m) == slot(&f.name));
            let function = if one_slot && members.iter().any(|m| number(m) == Some(0)) {
                number(&f.name)?
            } else {
                u8::try_from(members.iter().position(|m| *m == f.name)?)
                    .ok()
                    .filter(|&n| n < 8)?
            };
            Some((device, function))
        })
        .collect()
}

/// How many of `places` share each place's device: a device that holds more than one
/// function is a multi-function device to the guest.
#[cfg(all(hv, any(test, target_os = "linux")))]
fn passed_functions(places: &[Option<(u8, u8)>], device: u8) -> usize {
    places.iter().flatten().filter(|(d, _)| *d == device).count()
}

/// The guest's name of the PCI function at `place` ([`vfio_places`]), as sysfs has it.
pub fn guest_address((device, function): (u8, u8)) -> String {
    format!("0000:00:{device:02x}.{function}")
}

/// The IOMMU of the machine's PCI functions (virtio-iommu, D154), where any sits behind
/// it: the test function, whose DMA the VMM translates, and each function passed through
/// behind it ([`Vfio::iommu`]), whose group's container follows its endpoint. Its
/// translation of their requester IDs, with the MSI window `msi`, its first and last
/// address, where the machine has one; the device; and the requester IDs.
#[cfg(hv)]
fn iommu_of(
    pci: Pci<'_>,
    msi: Option<(u64, u64)>,
) -> Option<(
    crate::devices::virtio::iommu::Shared,
    Box<dyn crate::devices::virtio::VirtioDevice>,
    Vec<u32>,
)> {
    use crate::devices::virtio::iommu;
    let places = vfio_places(pci.vfio, pci.test);
    let behind = pci
        .vfio
        .iter()
        .zip(places)
        .filter(|(f, _)| f.iommu)
        .filter_map(|(_, place)| place);
    let endpoints: Vec<u32> = pci
        .test
        .iter()
        .copied()
        .chain(behind)
        .map(|(device, function)| u32::from(device) << 3 | u32::from(function))
        .collect();
    if endpoints.is_empty() {
        return None;
    }
    // Bypassing from the start where the test function is behind it, whose tests take a
    // guest without the driver; blocked where only functions passed through are, whose
    // guest needs the driver to give them to a process anyway.
    let translation = Arc::new(std::sync::Mutex::new(
        iommu::Translation::new(endpoints.clone(), msi).bypassing(!pci.test.is_empty()),
    ));
    let device =
        Box::new(iommu::Iommu::new(translation.clone())) as Box<dyn crate::devices::virtio::VirtioDevice>;
    Some((translation, device, endpoints))
}

/// Puts the PCI bus `pci` asks for on `bus` (D153, D154), where it asks for one: its ECAM
/// window at `ecam`, and its functions' BARs in the memory window `mem32`, each at the
/// next multiple of its size, but for 64-bit BARs, in `mem64` ([`Pci::mem64`]); QEMU's edu
/// at each of `pci.test`'s places, then each function passed through, on the devices after.
/// Their interrupts go where `machine.irqs` says; with none, the functions the VMM
/// emulates have none.
#[cfg(hv)]
fn pci_bus(
    bus: &mut crate::devices::MmioBus,
    ecam: u64,
    mem32: (u64, u64),
    mem64: Option<(u64, u64)>,
    pci: Pci<'_>,
    config: &crate::snapshot::MachineConfig,
    machine: PciMachine<'_>,
) -> Result<Option<PciBus>, String> {
    use crate::devices::pci;
    let irqs = machine.irqs;
    if pci.test.is_empty() && pci.vfio.is_empty() {
        return Ok(None);
    }
    let mut routed = PciBus::default();
    let mut window = BarWindow::of(mem32)?;
    let mut window64 = mem64.map(BarWindow::of).transpose()?;
    let mut functions: Vec<pci::Function> = Vec::new();
    for &(device, function) in pci.test {
        // Of a device of several, a multi-function device's function, its function 0
        // alone with the device's INTx pin.
        let role = pci::edu::Role {
            multi_function: pci.test.iter().filter(|(d, _)| *d == device).count() > 1,
            intx: function == 0,
        };
        let at = window.take(pci::edu::BAR_SIZE)?;
        let lines = irqs.map(|irqs| pci::Lines {
            intx: irqs.intx(device),
            msi: irqs.msi(u32::from(device << 3 | function)),
        });
        if lines.is_some() && role.intx {
            routed.intx.push((device, pci::edu::PIN));
        }
        let places = pci::edu::Places {
            registers: at,
            msix: window.take(pci::edu::MSIX_BAR_SIZE)?,
            scratch: window64
                .as_mut()
                .ok_or("the test function's BAR 4 has no 64-bit window")?
                .take(pci::edu::SCRATCH_BAR_SIZE)?,
        };
        let devfn = device << 3 | function;
        let dma = pci::edu::Dma {
            memory: machine.memory.clone(),
            iommu: machine.iommu.clone().map(|t| (t, u32::from(devfn))),
        };
        let (edu, bars) = pci::edu::Edu::of(places, lines, Some(dma), role)?;
        bus.insert(places.registers, pci::edu::BAR_SIZE, bars.registers)?;
        bus.insert(places.msix, pci::edu::MSIX_BAR_SIZE, bars.msix)?;
        bus.insert(places.scratch, pci::edu::SCRATCH_BAR_SIZE, bars.scratch)?;
        functions.push((devfn, edu));
    }
    if !pci.vfio.is_empty() {
        // All of guest RAM mapped for a function outside the guest's IOMMU, pinned: what
        // memory plugged later would outgrow, and what reporting would take from under it.
        if pci.vfio.iter().any(|f| !f.iommu) && (config.hotplug_mib > 0 || config.reporting) {
            return Err("a VM given a host's PCI device outside its own IOMMU has no memory to plug and no balloon: the device's DMA reaches all of guest RAM through mappings the host holds pinned (D154)".into());
        }
        functions.extend(passed_through(
            bus,
            [
                &mut window,
                window64.as_mut().unwrap_or(&mut BarWindow::default()),
            ],
            pci.vfio,
            pci.test,
            &machine,
            &mut routed,
        )?);
    }
    bus.insert(ecam, pci::ECAM_SIZE, Arc::new(pci::Ecam::new(functions)))?;
    Ok(Some(routed))
}

/// What is left of a PCI memory window, BARs taken from its start.
#[cfg(hv)]
#[derive(Debug, Default)]
struct BarWindow {
    next: u64,
    end: u64,
}

#[cfg(hv)]
impl BarWindow {
    /// The window `len` bytes at `base`, all of it left.
    fn of((base, len): (u64, u64)) -> Result<BarWindow, String> {
        Ok(BarWindow {
            next: base,
            end: base
                .checked_add(len)
                .ok_or("the PCI window overflows the address space")?,
        })
    }

    /// The place of a BAR of `size` bytes, a power of two: the next multiple of its size
    /// (PCI Local Bus 3.0 §6.2.5.1).
    fn take(&mut self, size: u64) -> Result<u64, String> {
        let at = self
            .next
            .checked_next_multiple_of(size)
            .filter(|at| at.checked_add(size).is_some_and(|end| end <= self.end))
            .ok_or_else(|| {
                format!(
                    "a BAR of {size:#x} bytes: the PCI window has {:#x} left",
                    self.end.saturating_sub(self.next)
                )
            })?;
        self.next = at + size;
        Ok(at)
    }
}

/// Each function of `vfio` opened (its DMA mapped to the machine's memory), its BARs
/// placed in `windows`, the 32-bit one and the 64-bit one for 64-bit BARs, and put on
/// `bus` where [`vfio_places`] places it, after the test function where `test`.
#[cfg(all(hv, target_os = "linux"))]
fn passed_through(
    bus: &mut crate::devices::MmioBus,
    mut windows: [&mut BarWindow; 2],
    vfio: &[Vfio],
    test: &[(u8, u8)],
    machine: &PciMachine<'_>,
    routed: &mut PciBus,
) -> Result<Vec<crate::devices::pci::Function>, String> {
    use crate::devices::pci::{self, vfio as v};
    let PciMachine { vm, memory, irqs, .. } = *machine;
    let mut functions: Vec<pci::Function> = Vec::new();
    // One container for the functions whose groups have an IOMMU, and one for those whose
    // have none; and one of its own for each group whose functions sit behind the guest's
    // IOMMU, with what follows their endpoints into it, which they share, as a type 1
    // container maps one set of I/O addresses for all of a group's devices.
    let (mut iommu, mut noiommu) = (None, None);
    let mut followed: Vec<(PathBuf, v::Container, Arc<v::Followed>)> = Vec::new();
    // The functions taken by their device nodes: iommufd, opened once; an I/O address
    // space for each group behind the guest's IOMMU, with what follows its endpoints into
    // it; and one that maps all of guest RAM for those outside it.
    let mut iommufd: Option<v::Iommufd> = None;
    let mut spaces: Vec<(PathBuf, v::Ioas, Arc<v::Followed>)> = Vec::new();
    let mut whole: Option<v::Ioas> = None;
    vfio_given(vfio)?;
    if !vfio.is_empty() {
        v::raise_memlock();
    }
    let places = vfio_places(vfio, test);
    for (f, place) in vfio.iter().zip(&places) {
        let named = |e: String| format!("{}: {e}", f.name);
        let &Some((device, function)) = place else {
            return Err(named(format!(
                "no place on the guest's bus, which has room for {} devices, each the functions of one IOMMU group, eight at most",
                32u8.saturating_sub(after_tests(test))
            )));
        };
        let (opened, follower) = match &f.cdev {
            Some(cdev) => {
                let fd = match &iommufd {
                    Some(fd) => fd,
                    None => iommufd.insert(v::Iommufd::open().map_err(named)?),
                };
                if f.iommu {
                    let at = match spaces.iter().position(|(group, ..)| *group == f.group) {
                        Some(at) => at,
                        None => {
                            let ioas = fd.ioas().map_err(named)?;
                            let follower = Arc::new(ioas.follower(memory.clone()));
                            spaces.push((f.group.clone(), ioas, follower));
                            spaces.len() - 1
                        }
                    };
                    let (_, ioas, follower) = spaces
                        .get(at)
                        .ok_or_else(|| named("its group's address space".into()))?;
                    (
                        v::Device::open_cdev(&f.name, cdev, ioas, vm)?,
                        Some(follower.clone()),
                    )
                } else {
                    // Mapped once its first function is attached, as iommufd pins what it
                    // maps for a device then.
                    let fresh = whole.is_none();
                    let ioas = match &whole {
                        Some(ioas) => ioas,
                        None => whole.insert(fd.ioas().map_err(named)?),
                    };
                    let opened = v::Device::open_cdev(&f.name, cdev, ioas, vm)?;
                    if fresh {
                        ioas.map_ram(memory).map_err(named)?;
                    }
                    (opened, None)
                }
            }
            None => {
                let (container, follower) = if f.iommu {
                    let at = match followed.iter().position(|(group, ..)| *group == f.group) {
                        Some(at) => at,
                        None => {
                            let container = v::Container::followed().map_err(named)?;
                            let follower = Arc::new(container.follower(memory.clone()));
                            followed.push((f.group.clone(), container, follower));
                            followed.len() - 1
                        }
                    };
                    let (_, container, follower) = followed
                        .get_mut(at)
                        .ok_or_else(|| named("its group's container".into()))?;
                    (container, Some(follower.clone()))
                } else {
                    let slot = if f.noiommu { &mut noiommu } else { &mut iommu };
                    let container = match slot {
                        Some(c) => c,
                        None => slot.insert(v::Container::open(f.noiommu).map_err(named)?),
                    };
                    (container, None)
                };
                (
                    v::Device::open(&f.name, &f.group, container, memory, vm)?,
                    follower,
                )
            }
        };
        // Behind the guest's IOMMU: its group's container maps what its endpoint sees
        // there, and its MSIs land where the endpoint's domain maps their addresses.
        let endpoint = u32::from(device) << 3 | u32::from(function);
        let behind = match follower {
            Some(follower) => {
                let translation = machine
                    .iommu
                    .clone()
                    .ok_or_else(|| named("behind the guest's IOMMU, where the machine has none".into()))?;
                translation
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .follow(endpoint, follower)
                    .map_err(|e| named(format!("its DMA mappings: {e}")))?;
                Some(translation)
            }
            None => None,
        };
        let mut bars = Vec::new();
        let [window, window64] = &mut windows;
        for (index, size, kind) in opened.bars() {
            let window = if kind == pci::BarKind::Memory64 {
                &mut **window64
            } else {
                &mut **window
            };
            let addr = window
                .take(size)
                .map_err(|e| format!("{}: BAR {index}: {e}", f.name))?;
            bars.push(pci::Bar {
                index,
                addr,
                size,
                kind,
            });
        }
        let mut route = irqs.and_then(|irqs| irqs.route(endpoint));
        let mut send = irqs.map(|irqs| irqs.msi(endpoint));
        if let Some(translation) = behind {
            let translated = Arc::new(v::Translated::new(
                translation,
                endpoint,
                route.clone(),
                send.clone(),
            ));
            route = route.map(|_| translated.clone() as Arc<dyn v::Route>);
            send = send.map(|_| translated as Arc<dyn pci::Msi>);
        }
        let together = passed_functions(&places, device) > 1;
        let passed = Arc::new(v::VfioFunction::new(opened, bars, route, send, vm, together)?);
        for bar in passed.bars() {
            bus.insert(
                bar.addr,
                bar.size,
                Arc::new(v::VfioBar::new(passed.clone(), bar.index)),
            )?;
        }
        if passed.pin() != 0 {
            routed.intx.push((device, passed.pin()));
        }
        functions.push((device << 3 | function, passed));
    }
    Ok(functions)
}

#[cfg(all(hv, not(target_os = "linux")))]
fn passed_through(
    _bus: &mut crate::devices::MmioBus,
    _windows: [&mut BarWindow; 2],
    _vfio: &[Vfio],
    _test: &[(u8, u8)],
    _machine: &PciMachine<'_>,
    _routed: &mut PciBus,
) -> Result<Vec<crate::devices::pci::Function>, String> {
    Err("a host's PCI devices are passed through by VFIO, Linux's (D154)".into())
}

/// KVM as the host of the functions VFIO passes through (D154): its VFIO device told of
/// their groups' or their own files, and memory slots of each function's own for its BARs.
#[cfg(all(hv, target_os = "linux"))]
impl crate::devices::pci::vfio::Host for crate::hv::Vm {
    fn add_file(&self, file: std::os::fd::RawFd) -> Result<(), String> {
        self.add_vfio_file(file).map_err(|e| e.to_string())
    }

    fn slots(&self, count: u32) -> Box<dyn crate::devices::pci::vfio::BarSlots> {
        Box::new(self.device_slots(count))
    }
}

#[cfg(all(hv, target_os = "linux"))]
impl crate::devices::pci::vfio::BarSlots for crate::hv::DeviceSlots {
    unsafe fn map(&self, i: u32, host: *mut u8, gpa: u64, len: usize) -> Result<(), String> {
        // SAFETY: forwarded caller contract.
        unsafe { crate::hv::DeviceSlots::map(self, i, host, gpa, len, true) }.map_err(|e| e.to_string())
    }

    fn unmap(&self, i: u32) -> Result<(), String> {
        crate::hv::DeviceSlots::unmap(self, i, 0, 0).map_err(|e| e.to_string())
    }
}

/// The host side of a VM's virtio-vsock device.
#[derive(Debug, Clone, Default)]
pub struct VsockHost {
    /// A Unix socket host clients dial (`CONNECT <port>`), and the prefix of the sockets
    /// `<path>_<port>` that guest connections to other host ports reach, as Firecracker's.
    pub path: Option<PathBuf>,
    /// Host ports this process serves itself. A guest connection to one arrives on its
    /// sender as one end of a socket pair, with no socket file anyone else could reach,
    /// dial first or need to be granted (D30). Each takes the guest's first connection
    /// only.
    pub ports: Vec<(u32, std::sync::mpsc::Sender<crate::platform::UnixStream>)>,
    /// Host ports this process serves for every guest connection, as [`ports`]' each
    /// first: their owner tells its own connections from any other by what each says
    /// first (shards' exec sessions, which name a token the guest's init alone was sent).
    ///
    /// [`ports`]: VsockHost::ports
    pub every: Vec<(u32, std::sync::mpsc::Sender<crate::platform::UnixStream>)>,
}

impl VsockHost {
    /// A host side at `path` alone.
    pub fn at(path: PathBuf) -> VsockHost {
        VsockHost {
            path: Some(path),
            ports: Vec::new(),
            every: Vec::new(),
        }
    }
}

/// The memory a VM has unless it is given more or less, in MiB.
pub const MEMORY_MIB: u64 = 256;

impl Config {
    /// A VM with shards' defaults: 1 CPU, [`MEMORY_MIB`], and a console on stdout.
    pub fn new(kernel: PathBuf, init: Option<PathBuf>) -> Config {
        Config {
            kernel,
            initrd: None,
            init,
            cmdline: "console=ttyS0 earlycon panic=-1".into(),
            vcpus: 1,
            memory_mib: MEMORY_MIB,
            console: Console::Stdout,
            disks: Vec::new(),
            snapshot: None,
            pmem: Vec::new(),
            vsock: None,
            net: None,
            shares: Vec::new(),
            join: None,
            #[cfg(unix)]
            join_share: None,
            io_limit: None,
            nested: false,
            hotplug_mib: 0,
            reporting: false,
            gpu: false,
            swap_mib: 0,
            share_window_mib: 0,
            layer: None,
            pci_test: Vec::new(),
            vfio: Vec::new(),
        }
    }
}

/// Starts a VM from a snapshot instead of booting one.
#[derive(Debug)]
pub struct RestoreConfig {
    /// The snapshot directory.
    pub dir: PathBuf,
    pub console: Console,
    /// For snapshots the restored guest asks for.
    pub snapshot: Option<SnapshotPolicy>,
    /// Prepare everything, then wait for [`Handle::release`]: a warm VM whose start
    /// request costs only the release.
    pub hold: bool,
    /// This VM's vsock device's host side. A snapshot with a vsock device needs one: the
    /// original VM may still hold its own path.
    pub vsock: Option<VsockHost>,
    /// This VM's network process's side of its network device. A snapshot with one needs
    /// one: a network process serves one VM alone.
    pub net: Option<crate::devices::virtio::net::NetHost>,
    /// The slots of the snapshot's shared directories, one each (D38).
    pub shares: Vec<crate::devices::virtio::fs::Share>,
    /// The restored VM's own join disk, where the snapshot has one (D119): empty, as the
    /// snapshot's was.
    pub join: Option<&'static crate::devices::virtio::block::Join>,
    /// The join share's slot (D119), which a server of its VM's joiners' volumes fills.
    #[cfg(unix)]
    pub join_share: Option<&'static crate::devices::virtio::fs::Slot>,
    /// Its I/O limits (D178), as [`Config::io_limit`].
    pub io_limit: Option<&'static crate::devices::virtio::limit::Limiter>,
    /// Prefetch the snapshot's working set, if it has one, before the guest runs: for a
    /// restore ahead of its request, which it moves off the request's path (PM M30).
    pub prefetch: bool,
    /// Record a working set for the snapshot, where it has none and this backend records
    /// restores' ([`RESTORES_RECORD`]), until [`Handle::take_working_set`] takes it.
    pub record: bool,
    /// A copy of the run's swap a snapshot of a run kept (D176), the restore's own, mapped
    /// where its guest had it before any vCPU runs.
    pub swap: Option<std::fs::File>,
    /// The restore's own file for the writable layer its snapshot kept (D190), which the
    /// guest writes; one the snapshot has a layer needs. Empty, which the restore fills
    /// with a copy of the layer of the generation it resumes; or a copy-on-write clone of
    /// generation `layer_cloned`'s, taken as it is where that is the generation it resumes,
    /// and filled as an empty one is where it is not.
    pub layer: Option<PathBuf>,
    /// The snapshot generation whose layer `layer` is a clone of, where its spawner made
    /// one (APFS, btrfs, XFS).
    pub layer_cloned: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SnapshotPolicy {
    /// Where to write the snapshot when the guest asks for one.
    pub dir: PathBuf,
    pub then: AfterSnapshot,
    /// Once resumed from its snapshot, record the pages the guest touches, until
    /// [`Handle::take_working_set`] takes them, for the snapshot.
    pub working_set: bool,
}

/// A snapshot the host asks of a running machine (D176), into the files of a generation
/// its spawner prepared (`snapshot::prepare`): no VM opens a path of its own (D30).
#[derive(Debug)]
pub struct Save {
    /// The generation's name, which its state records.
    pub name: String,
    pub memory: std::fs::File,
    pub state: std::fs::File,
    /// Where the run's swap goes (D159), where the machine maps one.
    pub swap: Option<std::fs::File>,
    /// Where its writable layer's head and file system go (D190), where it has one.
    pub layer: Option<(std::fs::File, std::fs::File)>,
    /// Where its DAX windows' heads go (D189), where it has windows.
    pub window_heads: Option<std::fs::File>,
    pub then: AfterSnapshot,
}

/// What the VM does once its snapshot is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterSnapshot {
    Resume,
    Stop,
}

#[derive(Debug, Clone)]
pub struct Disk {
    pub path: PathBuf,
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Console {
    Stdout,
    Discard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    /// The guest powered off (PSCI SYSTEM_OFF on arm64).
    PowerOff,
    /// The guest requested a reset (Linux does on reboot and, with `panic=-1`, on panic).
    Reset,
    /// Stopped by the host.
    Stopped,
    /// Stopped after writing a snapshot ([`AfterSnapshot::Stop`]).
    Snapshotted,
    Error(String),
}

/// Boots a VM and waits for it to exit.
pub fn run(cfg: &Config) -> Result<ExitReason, String> {
    let (_, running) = start(cfg)?;
    Ok(running.wait())
}

/// The initrd a machine boots: `--initrd`'s file, or an initramfs holding `--init`'s,
/// opened once its length is seen to fit the `room` bytes of RAM there are for it. Its
/// file is read only as it is copied in, straight into guest memory, an init into its
/// place in the archive there (audit D13, [`Initrd::place`]).
#[cfg(hv)]
fn initrd(cfg: &Config, room: u64) -> Result<Option<Initrd>, String> {
    let open = |p: &PathBuf| -> Result<(std::fs::File, usize), String> {
        let at = |e: std::io::Error| format!("{}: {e}", p.display());
        let file = crate::platform::open_input(p, false).map_err(at)?;
        let len = file.metadata().map_err(at)?.len();
        let len = usize::try_from(len).map_err(|_| format!("{}: {len} bytes", p.display()))?;
        Ok((file, len))
    };
    let fits = |p: &PathBuf, len: usize| {
        if len as u64 > room {
            return Err(format!(
                "{}: {len} bytes, more than the {room} the guest's RAM has room for",
                p.display()
            ));
        }
        Ok(())
    };
    match (&cfg.initrd, &cfg.init) {
        (Some(_), Some(_)) => Err("--initrd and --init are mutually exclusive".into()),
        // Only the length that was seen to fit is copied, though the file grow meanwhile;
        // one that shrinks is an error as it is copied.
        (Some(p), None) => {
            let (file, len) = open(p)?;
            fits(p, len)?;
            Ok(Some(Initrd {
                file,
                path: p.clone(),
                file_len: len,
                before: Vec::new(),
                after: Vec::new(),
            }))
        }
        (None, Some(p)) => {
            let (file, len) = open(p)?;
            fits(p, crate::initramfs::archive_len(len))?;
            let (before, after) = crate::initramfs::frame(len);
            Ok(Some(Initrd {
                file,
                path: p.clone(),
                file_len: len,
                before,
                after,
            }))
        }
        (None, None) => Ok(None),
    }
}

/// A boot's initrd before it is copied in: the file whose bytes it holds, and around them
/// the bytes of the archive an init is given in (`--init`, [`initramfs::frame`]); a
/// `--initrd`'s file is the initrd whole.
///
/// [`initramfs::frame`]: crate::initramfs::frame
#[cfg(hv)]
#[derive(Debug)]
pub(crate) struct Initrd {
    file: std::fs::File,
    path: PathBuf,
    file_len: usize,
    before: Vec<u8>,
    after: Vec<u8>,
}

#[cfg(hv)]
impl Initrd {
    /// The guest memory it takes.
    pub(crate) fn len(&self) -> u64 {
        (self.before.len() + self.file_len + self.after.len()) as u64
    }

    /// Writes its archive's own bytes at `start` in `memory`, around where its file's go:
    /// the piece left to copy in.
    pub(crate) fn place(&self, memory: &GuestMemory, start: u64) -> Result<Piece<'_>, String> {
        let overflows = || "the initrd overflows the address space".to_string();
        let data = start
            .checked_add(self.before.len() as u64)
            .ok_or_else(overflows)?;
        let after = data.checked_add(self.file_len as u64).ok_or_else(overflows)?;
        let access = memory.access().map_err(|e| e.to_string())?;
        for (at, bytes) in [(start, &self.before), (after, &self.after)] {
            if !bytes.is_empty() {
                access.write(at, bytes).map_err(|e| e.to_string())?;
            }
        }
        Ok(Piece {
            span: FileSpan {
                gpa: data,
                len: self.file_len,
                offset: 0,
            },
            file: &self.file,
            path: &self.path,
        })
    }
}

/// A span of guest memory and the file that fills it (`path` names it in errors): what a
/// boot copies in of its kernel and initrd.
#[cfg(hv)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Piece<'f> {
    pub span: FileSpan,
    pub file: &'f std::fs::File,
    pub path: &'f Path,
}

/// The unit a boot's copy is shared out in: a huge page of guest memory, which one thread
/// alone then faults in (THP's, PM M157).
#[cfg(hv)]
const COPY_UNIT: u64 = 2 << 20;

/// `pieces`, cut where each [`COPY_UNIT`] of guest memory begins.
#[cfg(hv)]
fn chunks<'f>(pieces: &[Piece<'f>]) -> Vec<Piece<'f>> {
    let mut out = Vec::new();
    for p in pieces {
        let FileSpan {
            mut gpa,
            len,
            mut offset,
        } = p.span;
        let mut left = len as u64;
        while left > 0 {
            let len = left.min(COPY_UNIT - gpa % COPY_UNIT);
            out.push(Piece {
                span: FileSpan {
                    gpa,
                    len: len as usize,
                    offset,
                },
                ..*p
            });
            // Within the piece, whose own sums were checked as it was planned.
            gpa = gpa.saturating_add(len);
            offset = offset.saturating_add(len);
            left -= len;
        }
    }
    out
}

/// Copies in the chunks `next` hands out until none is left: what each thread a boot's
/// copy has runs, however many.
#[cfg(hv)]
fn copy_in(memory: &GuestMemory, chunks: &[Piece<'_>], next: &AtomicUsize) -> Result<(), String> {
    while let Some(c) = chunks.get(next.fetch_add(1, Ordering::Relaxed)) {
        memory
            .read_file(c.span.gpa, c.span.len, c.file, c.span.offset)
            .map_err(|e| format!("{}: {e}", c.path.display()))?;
    }
    Ok(())
}

/// Runs `own`, the machine's making, while a thread of its own copies `pieces` into
/// `memory`, the most of a cold boot's setup (PM M226), which the VM, its devices and its
/// firmware tables need none of, nor touch any of; then this thread takes its share of
/// what is left of the copy (PM M227). Where no thread can be made, or two pieces meet in
/// memory, this one copies all of it after `own`, in the pieces' order: where two meet,
/// the later one's bytes are the ones there, as a copy of one piece after another leaves
/// them.
#[cfg(hv)]
fn beside_copy<T>(
    memory: &GuestMemory,
    pieces: &[Piece<'_>],
    own: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let chunks = chunks(pieces);
    let next = AtomicUsize::new(0);
    let copy = || copy_in(memory, &chunks, &next);
    let mut spans: Vec<&FileSpan> = pieces.iter().map(|p| &p.span).collect();
    spans.sort_unstable_by_key(|s| s.gpa);
    let meet = spans
        .windows(2)
        .any(|w| matches!(w, [a, b] if a.gpa.saturating_add(a.len as u64) > b.gpa));
    std::thread::scope(|s| {
        // None where pieces meet, or where no thread can be made.
        let loader = (!meet)
            .then(|| {
                std::thread::Builder::new()
                    .name("shards-load".into())
                    .spawn_scoped(s, copy)
                    .ok()
            })
            .flatten();
        let made = own();
        let mine = match made {
            Ok(_) => copy(),
            // No machine to boot: what is left of the copy is left.
            Err(_) => {
                next.store(chunks.len(), Ordering::Relaxed);
                Ok(())
            }
        };
        let theirs = match loader {
            Some(loader) => loader
                .join()
                .map_err(|_| "the kernel's loader ended abnormally".to_string())?,
            None => Ok(()),
        };
        let made = made?;
        mine.and(theirs)?;
        Ok(made)
    })
}

#[cfg(all(test, hv))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::hv::Touch;
    use crate::snapshot::MachineConfig;
    #[cfg(unix)]
    use crate::snapshot::Snapshot;

    /// BARs are placed from the window's start, each at the next multiple of its size,
    /// and one past the window's end is refused, the window as it was.
    #[test]
    fn bars_are_placed_at_multiples_of_their_sizes() {
        let mut w = BarWindow {
            next: 0x1000_0000,
            end: 0x1080_0000,
        };
        assert_eq!(w.take(0x1000).unwrap(), 0x1000_0000);
        assert_eq!(w.take(0x10_0000).unwrap(), 0x1010_0000);
        assert_eq!(w.take(0x10).unwrap(), 0x1020_0000);
        assert_eq!(w.take(0x20_0000).unwrap(), 0x1040_0000);
        // 4 MiB fits from 0x10800000 alone, past the end.
        assert!(w.take(0x40_0000).is_err(), "{w:?}");
        assert_eq!(w.next, 0x1060_0000);
        assert_eq!(w.take(0x20_0000).unwrap(), 0x1060_0000);
        assert!(w.take(0x10).is_err(), "{w:?}");
    }

    fn machine(vsock: bool, net: Option<[u8; 6]>) -> MachineConfig {
        MachineConfig {
            vcpus: 1,
            memory_mib: 16,
            disks: Vec::new(),
            pmem: Vec::new(),
            vsock,
            net,
            shares: 0,
            join: false,
            join_share: false,
            nested: false,
            hotplug_mib: 0,
            reporting: false,
            gpu: None,
            swap_mib: 0,
            share_window_mib: 0,
            layer_mib: 0,
        }
    }

    #[cfg(unix)]
    fn net_host() -> (crate::devices::virtio::net::NetHost, [std::os::fd::OwnedFd; 2]) {
        let (net_waits, device_rings) = shards_netring::doorbell().unwrap();
        let (device_waits, net_rings) = shards_netring::doorbell().unwrap();
        let host = crate::devices::virtio::net::NetHost {
            region: Arc::new(shards_netring::memory().unwrap()),
            wake_me: Arc::new(device_waits),
            wake_peer: Arc::new(device_rings),
            mac: [2, 0, 0, 0, 0, 9],
            flush: crate::devices::virtio::net::TxFlush::default(),
        };
        (host, [net_waits, net_rings])
    }

    /// A restore's host sides match its snapshot's devices: a vsock device and a socket
    /// for it, a network device and a network process for it, together or not at all.
    #[cfg(unix)]
    #[test]
    fn a_restores_hosts_match_its_snapshots_devices() {
        let vsock = VsockHost::at(PathBuf::from("/nowhere"));
        let (net, _ends) = net_host();
        let snap = |vsock, net| Snapshot {
            config: machine(vsock, net),
            plugged: Vec::new(),
            arch: Vec::new(),
            devices: Vec::new(),
        };
        let hosts = |v: bool, n: bool| Hosts {
            vsock: v.then_some(&vsock),
            net: n.then_some(&net),
            shares: &[],
            join: None,
            #[cfg(unix)]
            join_share: None,
            io: None,
            layer: None,
            window_heads: None,
        };
        let mac = Some([2, 0, 0, 0, 0, 1]);
        for (has_vsock, has_net) in [(false, false), (true, false), (false, true), (true, true)] {
            for (give_vsock, give_net) in [(false, false), (true, false), (false, true), (true, true)] {
                let checked = check_hosts(
                    &snap(has_vsock, has_net.then_some(mac).flatten()),
                    hosts(give_vsock, give_net),
                );
                let matched = has_vsock == give_vsock && has_net == give_net;
                assert_eq!(
                    checked.is_ok(),
                    matched,
                    "{has_vsock} {has_net} {give_vsock} {give_net}: {checked:?}"
                );
            }
        }
        let e = check_hosts(&snap(true, None), hosts(false, false)).unwrap_err();
        assert!(e.contains("--vsock PATH"), "{e}");
        let e = check_hosts(&snap(false, mac), hosts(false, false)).unwrap_err();
        assert!(e.contains("its own network process"), "{e}");
    }

    /// Each share's DAX window and the join share's (D189), at GiB boundaries from where
    /// the machine's addresses end, whole 2 MiB ranges, within its address limit: as long
    /// as asked where they fit, else as long as all of them fit, and none where not one
    /// range each does; a snapshot's, whose heads are kept for their length, that long or
    /// refused.
    #[test]
    fn share_windows_go_one_after_another() {
        const GIB: u64 = 1 << 30;
        let mut config = machine(false, None);
        assert_eq!(
            share_windows(&config, 5 * GIB, u64::MAX, false).unwrap(),
            (Vec::new(), 5 * GIB, 0)
        );
        config.shares = 2;
        config.join_share = true;
        assert_eq!(
            share_windows(&config, 5 * GIB, u64::MAX, false).unwrap(),
            (Vec::new(), 5 * GIB, 0)
        );
        config.share_window_mib = 1536;
        let (windows, end, mib) = share_windows(&config, 5 * GIB + 1, u64::MAX, false).unwrap();
        assert_eq!(mib, 1536);
        // Each window's head at its GiB, a section's start, its 13 sections' page
        // structures in one section, and the window right after it.
        let head = head::window_head_len(1536 << 20).unwrap();
        assert_eq!(head, pmem::SECTION);
        // Its held record, a bit for each of 384 Ki pages, 48 KiB, in a range of its own.
        let held = head::window_held_len(1536 << 20).unwrap();
        assert_eq!(held, pmem::ALIGN);
        let at = |gib: u64| WindowAt {
            head: (gib * GIB, head),
            window: (gib * GIB + head, 1536 << 20),
            held: (gib * GIB + head + (1536 << 20), held),
        };
        assert_eq!(windows, [at(6), at(8), at(10)]);
        assert_eq!(end, 10 * GIB + head + (1536 << 20) + held);
        assert_eq!(share_windows(&config, 5 * GIB + 1, end, true).unwrap().2, 1536);
        // A byte short: each the most whole ranges that fit, ending within the limit, one
        // range more past it.
        let (fitted, short, mib) = share_windows(&config, 5 * GIB + 1, end - 1, false).unwrap();
        assert_eq!(fitted.len(), 3);
        assert!(mib < 1536 && mib % 2 == 0, "{mib}");
        assert!(short < end, "{short:#x}");
        let mut longer = config.clone();
        longer.share_window_mib = mib + 2;
        assert!(share_windows(&longer, 5 * GIB + 1, end - 1, true).is_err());
        // Kept, their heads a snapshot's: that long, or refused.
        assert!(share_windows(&config, 5 * GIB + 1, end - 1, true).is_err());
        // No room for a range each: none.
        assert_eq!(
            share_windows(&config, 5 * GIB + 1, 6 * GIB, false).unwrap(),
            (Vec::new(), 5 * GIB + 1, 0)
        );
        config.share_window_mib = 3;
        assert!(share_windows(&config, 5 * GIB, u64::MAX, false).is_err());
    }

    #[test]
    fn pmem_regions_go_one_after_another() {
        let dir = shards_testdir::TempDir::new("pmem-regions").unwrap();
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::write(&a, vec![0u8; 4096]).unwrap();
        std::fs::write(&b, vec![0u8; 3 << 20]).unwrap();
        let mut config = machine(false, None);
        let (none, end) = pmem_regions(&config, 1 << 32).unwrap();
        assert!(none.is_empty());
        assert_eq!(end, 1 << 32);
        config.pmem = vec![a, b];
        let (regions, end) = pmem_regions(&config, 1 << 32).unwrap();
        let at: Vec<(u64, u64)> = regions.iter().map(|(r, gpa)| (*gpa, r.len() as u64)).collect();
        let (first, second) = (at[0], at[1]);
        assert_eq!(first.0, 1 << 32);
        assert_eq!(second.0, first.0 + first.1);
        assert_eq!(end, second.0 + second.1);
        assert!(first.1 >= 4096 && second.1 >= 3 << 20);
        let e = pmem_regions(&config, u64::MAX - 4096).unwrap_err();
        assert_eq!(e, "pmem regions overflow the address space");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Maps nothing: the order test's memory to plug is never plugged.
    #[cfg(unix)]
    #[derive(Debug, Clone)]
    struct Unmapped;

    #[cfg(unix)]
    impl mem::Mapper for Unmapped {
        unsafe fn map(&self, _host: *mut u8, _gpa: u64, _len: usize) -> Result<(), String> {
            Ok(())
        }

        fn unmap(&self, _gpa: u64, _len: usize) -> Result<(), String> {
            Ok(())
        }
    }

    /// A machine's virtio devices come in the guest's probe order, disks, pmem, vsock,
    /// network and memory to plug last, no more than the machine has room for; the network
    /// device takes the machine's own MAC; a device without its host side is refused.
    #[cfg(unix)]
    #[test]
    fn virtio_devices_come_in_probe_order() {
        let dir = shards_testdir::TempDir::new("virtio-devices").unwrap();
        let disk = dir.join("disk");
        std::fs::write(&disk, vec![0u8; 8 * 512]).unwrap();
        let image = dir.join("image");
        std::fs::write(&image, vec![0u8; 4096]).unwrap();
        let mut config = machine(true, Some([2, 0, 0, 0, 0, 1]));
        config.disks = vec![(disk.clone(), true), (disk, false)];
        config.pmem = vec![image];
        let (regions, _) = pmem_regions(&config, 1 << 32).unwrap();
        let vsock = VsockHost::at(dir.join("vsock"));
        let (net, _ends) = net_host();
        let hosts = Hosts {
            vsock: Some(&vsock),
            net: Some(&net),
            shares: &[],
            join: None,
            #[cfg(unix)]
            join_share: None,
            io: None,
            layer: None,
            window_heads: None,
        };
        let e = virtio_devices(&config, &regions, None, &[], None, hosts, Vec::new(), 4)
            .err()
            .unwrap();
        assert_eq!(e, "at most 4 virtio devices are supported");
        let plugging = || {
            let sizes = Arc::new(mem::Sizes::new(1 << 32, mem::ALIGN, mem::ALIGN));
            vec![Box::new(mem::Mem::new(sizes, Unmapped)) as Box<dyn crate::devices::virtio::VirtioDevice>]
        };
        let e = virtio_devices(&config, &regions, None, &[], None, hosts, plugging(), 5)
            .err()
            .unwrap();
        assert_eq!(e, "at most 5 virtio devices are supported");
        let ids: Vec<u32> = virtio_devices(&config, &regions, None, &[], None, hosts, plugging(), 6)
            .unwrap()
            .iter()
            .map(|d| d.device_id())
            .collect();
        assert_eq!(ids, [2, 2, 27, 19, 1, mem::DEVICE_ID]);
        let devices = virtio_devices(&config, &regions, None, &[], None, hosts, Vec::new(), 5).unwrap();
        let ids: Vec<u32> = devices.iter().map(|d| d.device_id()).collect();
        assert_eq!(ids, [2, 2, 27, 19, 1]);
        let mut mac = [0u8; 6];
        devices[4].read_config(0, &mut mac);
        assert_eq!(
            mac,
            [2, 0, 0, 0, 0, 1],
            "the machine's MAC, not its network process's"
        );
        drop(devices);
        let missing = Hosts {
            vsock: None,
            net: Some(&net),
            shares: &[],
            join: None,
            #[cfg(unix)]
            join_share: None,
            io: None,
            layer: None,
            window_heads: None,
        };
        let e = virtio_devices(&config, &regions, None, &[], None, missing, Vec::new(), 5)
            .err()
            .unwrap();
        assert!(e.contains("no host side"), "{e}");
        let missing = Hosts {
            vsock: Some(&vsock),
            net: None,
            shares: &[],
            join: None,
            #[cfg(unix)]
            join_share: None,
            io: None,
            layer: None,
            window_heads: None,
        };
        let e = virtio_devices(&config, &regions, None, &[], None, missing, Vec::new(), 5)
            .err()
            .unwrap();
        assert!(e.contains("no network process"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Backing files are resolved when the machine is built, so a snapshot of it names
    /// them wherever it is restored from (audit A18). Tests run in their package's
    /// directory, which holds `Cargo.toml`.
    #[test]
    fn a_machines_backing_files_resolve_to_absolute_paths() {
        let mut cfg = Config::new(PathBuf::from("kernel"), None);
        cfg.disks = vec![Disk {
            path: PathBuf::from("Cargo.toml"),
            read_only: true,
        }];
        cfg.pmem = vec![PathBuf::from("./src/../Cargo.toml")];
        let machine = machine_config(&cfg).unwrap();
        let absolute = std::fs::canonicalize("Cargo.toml").unwrap();
        assert!(absolute.is_absolute());
        assert_eq!(machine.disks, vec![(absolute.clone(), true)]);
        assert_eq!(machine.pmem, vec![absolute]);
        cfg.pmem = vec![PathBuf::from("no-such-file")];
        assert!(machine_config(&cfg).unwrap_err().starts_with("no-such-file: "));
    }

    /// An init or initrd is taken only if it fits where it goes: one longer than the room
    /// is refused, saying so; one that fits is the archive, or the initrd, whole, once
    /// placed and copied in, across a huge page's edge too (audit D13). `Cargo.toml`
    /// stands in for either.
    #[test]
    fn an_initrd_is_taken_only_if_it_fits() {
        let bytes = std::fs::read("Cargo.toml").unwrap();
        let len = bytes.len();
        let memory = GuestMemory::anonymous(&[(0, 8 << 20)]).unwrap();
        let copied = |i: &Initrd, start: u64| {
            let piece = i.place(&memory, start).unwrap();
            copy_in(&memory, &chunks(&[piece]), &AtomicUsize::new(0)).unwrap();
            let mut back = vec![0u8; i.len() as usize];
            memory.access().unwrap().read(start, &mut back).unwrap();
            back
        };
        let mut cfg = Config::new(PathBuf::from("kernel"), Some(PathBuf::from("Cargo.toml")));
        let archive = crate::initramfs::archive_len(len) as u64;
        let e = initrd(&cfg, archive - 1).unwrap_err();
        assert!(e.contains("room for"), "{e}");
        let framed = initrd(&cfg, archive).unwrap().unwrap();
        assert_eq!(framed.len(), archive);
        assert_eq!(copied(&framed, 0x1000), crate::initramfs::with_init(&bytes));
        cfg.init = None;
        cfg.initrd = Some(PathBuf::from("Cargo.toml"));
        assert!(initrd(&cfg, len as u64 - 1).unwrap_err().contains("room for"));
        let whole = initrd(&cfg, len as u64).unwrap().unwrap();
        assert_eq!(copied(&whole, COPY_UNIT - 7), bytes);
        cfg.init = Some(PathBuf::from("Cargo.toml"));
        assert!(initrd(&cfg, u64::MAX).is_err(), "both at once");
    }

    /// A boot's copy is cut where each 2 MiB of guest memory begins, every byte in one
    /// chunk alone, each read from where its own bytes lie in the file.
    #[test]
    fn a_boots_copy_is_cut_at_each_huge_page() {
        let file = std::fs::File::open("Cargo.toml").unwrap();
        let path = Path::new("Cargo.toml");
        let piece = |gpa, len, offset| Piece {
            span: FileSpan { gpa, len, offset },
            file: &file,
            path,
        };
        let cut = chunks(&[piece(0x1f_f000, 0x40_2000, 7), piece(0x80_0000, 5, 0)]);
        let spans: Vec<FileSpan> = cut.iter().map(|p| p.span).collect();
        let span = |gpa, len, offset| FileSpan { gpa, len, offset };
        assert_eq!(
            spans,
            vec![
                span(0x1f_f000, 0x1000, 7),
                span(0x20_0000, 0x20_0000, 0x1007),
                span(0x40_0000, 0x20_0000, 0x20_1007),
                span(0x60_0000, 0x1000, 0x40_1007),
                span(0x80_0000, 5, 0),
            ]
        );
    }

    /// Pieces that meet in memory are copied in their order, the later one's bytes where
    /// they meet, as a copy of one after another leaves them.
    #[test]
    fn pieces_that_meet_are_copied_in_their_order() {
        let dir = shards_testdir::TempDir::new("meet").unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, vec![b'a'; 3 << 20]).unwrap();
        std::fs::write(&b, vec![b'b'; 1 << 20]).unwrap();
        let (fa, fb) = (std::fs::File::open(&a).unwrap(), std::fs::File::open(&b).unwrap());
        let memory = GuestMemory::anonymous(&[(0, 8 << 20)]).unwrap();
        let piece = |file, path, gpa, len| Piece {
            span: FileSpan { gpa, len, offset: 0 },
            file,
            path,
        };
        let pieces = [
            piece(&fa, a.as_path(), 0x10_0000, 3 << 20),
            piece(&fb, b.as_path(), 0x20_0000, 1 << 20),
        ];
        beside_copy(&memory, &pieces, || Ok(())).unwrap();
        let mut back = vec![0u8; 3 << 20];
        memory.access().unwrap().read(0x10_0000, &mut back).unwrap();
        let (before, rest) = back.split_at(1 << 20);
        let (met, after) = rest.split_at(1 << 20);
        assert!(before.iter().all(|&x| x == b'a'));
        assert!(
            met.iter().all(|&x| x == b'b'),
            "the later piece's bytes where they meet"
        );
        assert!(after.iter().all(|&x| x == b'a'));
    }

    /// Threads sharing one boot's copy copy every byte where it goes; a file shorter than
    /// its plan is an error naming it, not a short copy.
    #[test]
    fn a_boots_copy_is_shared_by_its_threads() {
        let dir = shards_testdir::TempDir::new("copy").unwrap();
        let path = dir.path().join("image");
        let bytes: Vec<u8> = (0..(5 << 20) + 3).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let memory = GuestMemory::anonymous(&[(0, 16 << 20)]).unwrap();
        let at = 0x10_0009;
        let piece = |len| Piece {
            span: FileSpan {
                gpa: at,
                len,
                offset: 0,
            },
            file: &file,
            path: &path,
        };
        let plan = chunks(&[piece(bytes.len())]);
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            let other = s.spawn(|| copy_in(&memory, &plan, &next));
            copy_in(&memory, &plan, &next).unwrap();
            other.join().unwrap().unwrap();
        });
        let mut back = vec![0u8; bytes.len()];
        memory.access().unwrap().read(at, &mut back).unwrap();
        assert_eq!(back, bytes);
        let short = chunks(&[piece(bytes.len() + 1)]);
        let e = copy_in(&memory, &short, &AtomicUsize::new(0)).unwrap_err();
        assert!(e.contains("image"), "{e}");
    }

    /// Functions passed through sit on bus 0 after the test function where the machine has
    /// it, a host IOMMU group's together in one device: with their own function numbers
    /// where they are one slot's with its function 0, else numbered from 0 in their
    /// addresses' order, a group of slots' too; none past bus 0's room, nor past a
    /// device's eight functions.
    #[test]
    fn a_groups_functions_are_passed_through_as_one_device() {
        let f = |name: &str, group: u32| Vfio {
            name: name.into(),
            group: PathBuf::from(format!("/dev/vfio/{group}")),
            noiommu: false,
            bars64: Vec::new(),
            iommu: false,
            cdev: None,
        };
        let vfio = [
            f("0000:01:00.0", 7),
            f("0000:02:00.0", 9),
            f("0000:01:00.1", 7),
            f("0000:03:00.3", 4),
            f("0000:03:00.1", 4),
            // Two slots behind a switch that keeps neither from the other's DMA (no ACS).
            f("0000:06:00.0", 5),
            f("0000:05:00.0", 5),
            f("0000:04:00.9", 6),
        ];
        assert_eq!(
            vfio_places(&vfio, &[]),
            vec![
                Some((1, 0)),
                Some((2, 0)),
                Some((1, 1)),
                Some((3, 1)),
                Some((3, 0)),
                Some((4, 1)),
                Some((4, 0)),
                None
            ]
        );
        let places = vfio_places(&vfio, &[]);
        assert_eq!(
            (passed_functions(&places, 1), passed_functions(&places, 2)),
            (2, 1)
        );
        assert_eq!(
            vfio_places(&vfio[..1], &[(1, 0)]),
            vec![Some((2, 0))],
            "after edu"
        );
        assert_eq!(
            vfio_places(&vfio[..1], &[(1, 0), (3, 0), (3, 1)]),
            vec![Some((4, 0))],
            "after the last test device"
        );
        let many: Vec<Vfio> = (0..32).map(|i| f(&format!("0000:{i:02x}:00.0"), i)).collect();
        let placed = vfio_places(&many, &[]);
        assert_eq!(placed[30], Some((31, 0)));
        assert_eq!(placed[31], None, "past bus 0's 32 devices");
        let wide: Vec<Vfio> = (0..9).map(|i| f(&format!("0000:{i:02x}:00.0"), 3)).collect();
        assert_eq!(vfio_places(&wide, &[])[7], Some((1, 7)));
        assert_eq!(
            vfio_places(&wide, &[])[8],
            None,
            "past a device's eight functions"
        );
        assert_eq!(guest_address((3, 1)), "0000:00:03.1");
    }

    /// Functions are given a machine once each, and a group's functions one way: all
    /// through the group or all by their device nodes, all behind the guest's IOMMU or all
    /// outside it.
    #[test]
    fn a_groups_functions_are_taken_one_way() {
        let f = |name: &str, group: u32, cdev: bool, iommu: bool| Vfio {
            name: name.into(),
            group: PathBuf::from(format!("/dev/vfio/{group}")),
            noiommu: false,
            bars64: Vec::new(),
            iommu,
            cdev: cdev.then(|| PathBuf::from(format!("/dev/vfio/devices/vfio{group}"))),
        };
        let fine = [
            f("0000:01:00.0", 7, true, true),
            f("0000:01:00.1", 7, true, true),
            f("0000:02:00.0", 9, false, false),
            f("0000:03:00.0", 4, false, true),
            f("0000:03:00.1", 4, false, true),
        ];
        assert_eq!(vfio_given(&fine), Ok(()));
        assert_eq!(vfio_given(&[]), Ok(()));
        let twice = [fine[0].clone(), fine[2].clone(), fine[0].clone()];
        assert_eq!(vfio_given(&twice), Err("0000:01:00.0: given twice".into()));
        let ways = [fine[0].clone(), f("0000:01:00.1", 7, false, true)];
        assert_eq!(
            vfio_given(&ways),
            Err("0000:01:00.1: taken through its group, and 0000:01:00.0 of its group /dev/vfio/7 by its device node: VFIO gives a group's functions one way at a time".into())
        );
        let views = [fine[3].clone(), f("0000:03:00.1", 4, false, false)];
        assert_eq!(
            vfio_given(&views),
            Err("0000:03:00.1: outside the guest's IOMMU, and 0000:03:00.0 of its group /dev/vfio/4 behind the guest's IOMMU: a group's functions share the host's mappings of their DMA".into())
        );
        let unsafe_follow = [Vfio {
            noiommu: true,
            ..f("0000:05:00.0", 2, false, true)
        }];
        assert_eq!(
            vfio_given(&unsafe_follow),
            Err("0000:05:00.0: a group without an IOMMU cannot follow the guest's".into())
        );
    }

    /// A working set naming a page the guest does not have is not prefetched at all
    /// (audit A16).
    #[test]
    fn a_working_set_outside_the_guest_is_dropped() {
        let page = 0x4000;
        let ranges = [(0x8000_0000, 0x10_0000), (0x1_0000_0000, 0x20_0000)];
        let touch = |gpa| Touch { gpa, written: false };
        let inside = vec![touch(0x8000_0000), touch(0x800f_c000), touch(0x1_001f_c000)];
        assert_eq!(usable_working_set(inside.clone(), page, &ranges), inside);
        for outside in [0x8010_0000, 0x7fff_c000, 0x1_0020_0000, !(page - 1)] {
            let set = vec![touch(0x8000_0000), touch(outside)];
            assert!(usable_working_set(set, page, &ranges).is_empty(), "{outside:#x}");
        }
    }
}
