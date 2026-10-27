//! vCPU threads and the VM lifecycle, on any backend and architecture: boot or restore,
//! run, snapshot, stop.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak, mpsc};
use std::thread::JoinHandle;

use super::barrier::Barrier;
use super::hold::Hold;
use super::machine::{self, Machine};
use super::{AfterSnapshot, Config, ExitReason, RestoreConfig, Save, SnapshotPolicy};
use crate::devices::control::Control;
use crate::devices::power::PowerEvent;
use crate::devices::serial::Serial;
use crate::devices::virtio::block::Join;
use crate::devices::virtio::{head, pmem};
use crate::hv;
use crate::memory::GuestMemory;
use crate::snapshot::{self, MachineConfig, Snapshot, codec::Writer};
use crate::sync::{lock, wait};
use crate::{debug, info, platform, warn};

/// Ok when this host can run VMs; otherwise, why not.
pub fn check_host() -> Result<(), String> {
    hv::check_host()
}

/// The most vCPUs one VM can have on this host.
pub fn max_vcpus() -> Result<u32, String> {
    hv::max_vcpus().map_err(|e| e.to_string())
}

/// A running VM's handle for host-side control.
#[derive(Debug, Clone)]
pub struct Handle {
    shared: Arc<Shared>,
    serial: Arc<Serial>,
    control: Arc<Control>,
    memory: Arc<GuestMemory>,
}

impl Handle {
    /// The bytes of its memory to plug whose pages this process holds (D133): pages the
    /// guest touched and has not given back. None where it has none, or the host cannot
    /// say which pages it holds.
    pub fn hotplug_held(&self) -> Option<u64> {
        self.memory.hotplug_held().ok().flatten()
    }

    /// The bytes its guest has reported free (D151), given back or not.
    pub fn reported(&self) -> u64 {
        self.memory.reported()
    }

    /// Sets the most memory its guest may have plugged, in bytes: a run's own, as a VM
    /// restored from a template takes it (D148). An error where it has none to plug.
    pub fn set_plug_most(&self, bytes: u64) -> Result<(), String> {
        if self.control.set_plug_most(bytes) {
            Ok(())
        } else {
            Err("the VM has no memory to plug".into())
        }
    }

    /// Has its writable layer's file read ahead again (D190), as its run's command runs
    /// ([`Layer::read_ahead`](crate::devices::virtio::pmem::Layer::read_ahead)): nothing
    /// where it has none.
    pub fn layer_reads_ahead(&self) {
        if let Some(layer) = self.control.layer() {
            layer.read_ahead();
        }
    }

    /// Holds its vCPUs together to `cpus` CPUs' time of the host's each period, its run's
    /// limit (D191), whatever its guest does with its own cgroups; 0, or as many CPUs as
    /// it has vCPUs, lets them be. Its governor's thread is made with the first limit.
    pub fn hold_cpus(&self, cpus: u32) -> Result<(), String> {
        let sh = &self.shared;
        sh.hold.set(cpus);
        if cpus == 0 {
            return Ok(());
        }
        let mut governor = lock(&sh.governor);
        // Under the lock the teardown takes it by, once the machine stops: none is made
        // after.
        if governor.is_none() && !sh.exiting() {
            let shared = sh.clone();
            let made = std::thread::Builder::new()
                .name("hold".into())
                .spawn(move || shared.hold.govern(&|| shared.kick_all()))
                .map_err(|e| format!("starting the hold on its vCPUs: {e}"))?;
            *governor = Some(made);
        }
        Ok(())
    }

    /// Maps `file`, a run's swap, into the guest's swap window (D159), writable by the guest;
    /// returns its length, or nothing where a run's is mapped already, which the guest's
    /// members share. An error where the VM has no swap window.
    pub fn attach_swap(&self, file: std::fs::File) -> Result<Option<u64>, String> {
        self.control
            .swap()
            .ok_or("the microVM has no swap window")?
            .attach(file)
    }

    /// Maps `file` back into the guest's swap window, a copy of the run's swap a snapshot of
    /// the run kept (D176), before a held restore's vCPUs run: the length it kept alone.
    pub fn restore_swap(&self, file: std::fs::File) -> Result<(), String> {
        self.control
            .swap()
            .ok_or("the microVM has no swap window")?
            .restore_run(Some(file))
    }

    /// Saves the running machine into `save`'s files, a generation its spawner prepared and
    /// commits once this returns (D176), with its vCPUs parked and its devices quiet, as a
    /// template is saved. A machine that goes on runs again before this returns; one that
    /// stops waits, parked, until the [`Saved`] returned goes, so that its spawner hears of
    /// the save before it ends. A save that fails leaves the machine running as it was.
    pub fn save(&self, save: Save) -> Result<Saved, String> {
        let sh = &self.shared;
        if !super::SNAPSHOTS {
            return Err("this host's microVMs take no snapshots yet".into());
        }
        let parts = sh.parts.get().ok_or("the microVM has not started")?;
        if sh.released_at_us.get().is_none() {
            return Err("the microVM has not started".into());
        }
        // A template's snapshot, which the guest asks for, is the only other: its
        // coordinator would take this one's request for it.
        if sh.template.load(Ordering::Acquire) {
            return Err("the microVM has its template still to save".into());
        }
        {
            let mut saving = lock(&sh.saving);
            if sh.exiting() {
                return Err("the microVM has stopped".into());
            }
            if *saving {
                return Err("the microVM is being saved already".into());
            }
            *saving = true;
        }
        let saved = self.save_parked(parts, save);
        *lock(&sh.saving) = false;
        sh.saved.notify_all();
        saved
    }

    /// [`save`](Handle::save)'s snapshot: what it holds of the machine goes before the
    /// machine may be taken apart.
    fn save_parked(&self, parts: &Parts, save: Save) -> Result<Saved, String> {
        let sh = &self.shared;
        let (Some(vm), Some(bus)) = (parts.vm.upgrade(), parts.bus.upgrade()) else {
            return Err("the microVM has stopped".into());
        };
        let t0 = crate::log::uptime_us();
        if !sh.snapshot.request() {
            return Err("the microVM is being saved already".into());
        }
        sh.kick_all();
        let cut = Cut {
            sh,
            vm: &vm,
            bus: &bus,
            memory: &self.memory,
            config: &parts.config,
            join: parts.join,
            swap: self.control.swap().map(|w| &**w),
        };
        if !sh.snapshot.wait_parked(&|| sh.exiting()) {
            return Err("the microVM has stopped".into());
        }
        let snap = match cut.take(true) {
            Ok(Some(snap)) => snap,
            Ok(None) => return Err("the microVM has stopped".into()),
            Err(e) => {
                cut.go_on();
                return Err(e);
            }
        };
        let swap = cut.swap.filter(|w| w.attached());
        let written = snapshot::fill(&save.name, &snap, &self.memory, &save.memory, &save.state)
            .and_then(|()| match (swap, &save.swap) {
                (None, _) => Ok(()),
                (Some(window), Some(file)) => window.save_run(file).map(|_| ()),
                (Some(_), None) => Err("its run's swap has nowhere to go".into()),
            })
            .and_then(|()| match (self.control.layer(), &save.layer) {
                (None, _) => Ok(()),
                (Some(layer), Some((head, data))) => layer.save(head, data),
                (Some(_), None) => Err("its writable layer has nowhere to go".into()),
            })
            .and_then(|()| match (self.control.window_heads(), &save.window_heads) {
                (None, _) => Ok(()),
                (Some(heads), Some(file)) => heads
                    .save(file)
                    .map_err(|e| format!("its DAX windows' heads: {e}")),
                (Some(_), None) => Err("its DAX windows' heads have nowhere to go".into()),
            });
        if let Err(e) = written {
            // One whose guest lost a mapping taken from it for the save stops.
            if self.control.lost() {
                sh.stop(ExitReason::Error(e.clone()));
            } else {
                cut.go_on();
            }
            return Err(e);
        }
        info!(
            "saved for the host in {} us",
            crate::log::uptime_us().saturating_sub(t0)
        );
        Ok(match save.then {
            AfterSnapshot::Resume => {
                cut.go_on();
                Saved(None)
            }
            AfterSnapshot::Stop => Saved(Some(self.shared.clone())),
        })
    }

    /// Guest markers as `(marker, µs since VMM start)`.
    pub fn markers(&self) -> Vec<(u32, u128)> {
        self.control.markers()
    }

    /// The contract the guest's init announced (`shards_abi::control::ABI`), if it has.
    pub fn guest_abi(&self) -> Option<u64> {
        self.control.guest_abi()
    }

    /// Microseconds since VMM start at which the boot vCPU first entered the guest.
    pub fn entered_at_us(&self) -> Option<u128> {
        self.shared.entered_at_us.get().copied()
    }

    /// Microseconds since VMM start at which the guest exited (once it has).
    pub fn exited_at_us(&self) -> Option<u128> {
        *lock(&self.shared.exited_at_us)
    }

    pub fn stop(&self) {
        self.shared.stop(ExitReason::Stopped);
    }

    /// Waits until no snapshot is being taken: one the guest asked for is durable and in
    /// use when this returns, though the guest ran again before it was (PM M63). Returns
    /// at once if none is, or the VM is stopping.
    pub fn wait_for_snapshot(&self) {
        let mut committing = lock(&self.shared.committing);
        while *committing && !self.shared.exiting() {
            committing = wait(&self.shared.committed, committing);
        }
    }

    /// Starts the vCPUs of a VM restored with `hold`. Everything else is ready, so this
    /// is all a start request costs. A guest whose snapshot kept its run's swap runs only
    /// with a copy of it mapped back ([`restore_swap`](Handle::restore_swap), D176): one
    /// without stops, rather than find zeros where it swapped.
    pub fn release(&self) {
        if self.control.swap().is_some_and(|w| w.wants_run()) {
            return self.shared.stop(ExitReason::Error(
                "its snapshot kept its run's swap, which the restore was not given".into(),
            ));
        }
        self.shared.release_vcpus();
    }

    /// Microseconds since VMM start at which the vCPUs were released.
    pub fn released_at_us(&self) -> Option<u128> {
        self.shared.released_at_us.get().copied()
    }

    /// Feeds bytes to the guest console as if typed.
    pub fn console_input(&self, bytes: &[u8]) {
        self.serial.enqueue_input(bytes);
    }

    /// Whether the VM is recording a working set ([`SnapshotPolicy::working_set`]).
    pub fn recording(&self) -> bool {
        lock(&self.shared.recording).is_some()
    }

    /// Stops recording the working set, and keeps what it recorded for
    /// [`take_working_set`](Handle::take_working_set). The VM writes nothing of it: its
    /// snapshot is a template other VMs restore, and no VM may write one (D30); the daemon
    /// takes it and writes it ([`accept_working_set`]).
    pub fn end_recording(&self) -> Result<(), String> {
        let Some((recorder, name)) = lock(&self.shared.recording).take() else {
            return Ok(());
        };
        let pages = machine::recorded(&recorder)?;
        if !pages.is_empty() {
            *lock(&self.shared.recorded) = Some((name, snapshot::encode_working_set(&pages, machine::PAGE)));
        }
        Ok(())
    }

    /// The working set recorded, encoded, and the name of the generation it goes with;
    /// `None` if nothing was. Recording ends first, if it has not.
    pub fn take_working_set(&self) -> Result<Option<(String, Vec<u8>)>, String> {
        self.end_recording()?;
        Ok(lock(&self.shared.recorded).take())
    }

    /// How many pages of its snapshot's working set this VM prefetched before it ran.
    pub fn prefetched(&self) -> usize {
        self.shared.kept.get().map_or(0, machine::prefetched)
    }
}

#[derive(Debug)]
struct Shared {
    kickers: Vec<OnceLock<hv::Kicker>>,
    exiting: AtomicBool,
    exit: Mutex<Option<ExitReason>>,
    exited_at_us: Mutex<Option<u128>>,
    exited: Condvar,
    /// When the boot vCPU first entered the guest (µs since VMM start).
    entered_at_us: OnceLock<u128>,
    /// Where vCPUs park, one phase at a time, while a snapshot is taken.
    snapshot: Barrier<machine::Captured>,
    /// vCPUs wait here after creation until the machine is complete (and, for a held
    /// restore, until the request arrives).
    released: Mutex<Release>,
    release: Condvar,
    /// What the machine's start leaves it, once every vCPU is set up: a restored guest's
    /// counter at its snapshot, and what was prefetched. The rest of the start is dropped
    /// then (audit D03).
    kept: OnceLock<machine::Kept>,
    /// When the vCPUs were released (µs since VMM start).
    released_at_us: OnceLock<u128>,
    /// While a working set is recorded: what records it, and the directory of the
    /// snapshot it goes with, held open since the daemon may rename it.
    recording: Mutex<Option<(machine::Recorder, String)>>,
    /// What a recording that ended recorded, for the daemon: the generation's name and the
    /// encoded working set.
    recorded: Mutex<Option<(String, Vec<u8>)>>,
    /// Whether a snapshot is being taken, from its vCPUs parking to its commit: the guest
    /// runs again before the commit ends (PM M63).
    committing: Mutex<bool>,
    committed: Condvar,
    /// What a snapshot the host asks for reads, once the machine is launched (D176).
    parts: OnceLock<Parts>,
    /// Whether a template's snapshot is still to be taken, its coordinator waiting.
    template: AtomicBool,
    /// Whether a snapshot the host asked for is being taken: one at a time, and the
    /// machine is taken apart only once it is done (`Running::teardown`).
    saving: Mutex<bool>,
    saved: Condvar,
    /// The host's hold on the vCPUs' CPU time, a run's limit (D191), and its governor's
    /// thread, made as the first limit comes and joined as the machine is taken apart.
    hold: Hold,
    governor: Mutex<Option<JoinHandle<()>>>,
}

/// What a snapshot the host asks for reads of the machine (D176): its VM and devices by
/// weak references, which `Running` alone owns, so that one held never keeps them past
/// the machine's teardown.
#[derive(Debug)]
struct Parts {
    vm: Weak<hv::Vm>,
    bus: Weak<machine::Bus>,
    config: MachineConfig,
    join: Option<&'static Join>,
}

/// A save the host asked for, done ([`Handle::save`]): a machine to stop after it stops as
/// this goes, its vCPUs parked until then.
#[derive(Debug)]
#[must_use = "a machine to stop after its save stops as this goes"]
pub struct Saved(Option<Arc<Shared>>);

impl Drop for Saved {
    fn drop(&mut self) {
        if let Some(sh) = self.0.take() {
            sh.stop(ExitReason::Snapshotted);
        }
    }
}

#[derive(Debug, Default)]
struct Release {
    done: bool,
    /// For a restore: the counter offset every vCPU applies, taken at release so the
    /// guest counter continues from the snapshot with no jump.
    counter_offset: Option<u64>,
}

impl Shared {
    fn new(vcpus: u32) -> Shared {
        Shared {
            kickers: (0..vcpus).map(|_| OnceLock::new()).collect(),
            exiting: AtomicBool::new(false),
            exit: Mutex::new(None),
            exited_at_us: Mutex::new(None),
            exited: Condvar::new(),
            entered_at_us: OnceLock::new(),
            snapshot: Barrier::new(vcpus as usize),
            released: Mutex::new(Release::default()),
            release: Condvar::new(),
            kept: OnceLock::new(),
            released_at_us: OnceLock::new(),
            recording: Mutex::new(None),
            recorded: Mutex::new(None),
            committing: Mutex::new(false),
            committed: Condvar::new(),
            parts: OnceLock::new(),
            template: AtomicBool::new(false),
            saving: Mutex::new(false),
            saved: Condvar::new(),
            hold: Hold::new(vcpus as usize),
            governor: Mutex::new(None),
        }
    }

    /// Waits until no save the host asked for reads the machine.
    fn await_save(&self) {
        let mut saving = lock(&self.saving);
        while *saving {
            saving = wait(&self.saved, saving);
        }
    }

    /// Lets every created vCPU run (once).
    fn release_vcpus(&self) {
        let mut r = lock(&self.released);
        if r.done {
            return;
        }
        let Some(kept) = self.kept.get() else {
            drop(r);
            return self.stop(ExitReason::Error("released before its vCPUs were set up".into()));
        };
        match machine::release_offset(kept) {
            Ok(offset) => r.counter_offset = offset,
            Err(e) => {
                drop(r);
                return self.stop(ExitReason::Error(format!("releasing the vCPUs: {e}")));
            }
        }
        r.done = true;
        let _ = self.released_at_us.set(crate::log::uptime_us());
        drop(r);
        self.release.notify_all();
    }

    /// Blocks a created vCPU until released; returns the counter offset to apply, if
    /// any. Returns at once if the VM is stopping.
    fn wait_release(&self) -> Option<u64> {
        let mut r = lock(&self.released);
        while !r.done && !self.exiting() {
            r = wait(&self.release, r);
        }
        r.counter_offset
    }

    fn kick_all(&self) {
        for kicker in self.kickers.iter().filter_map(OnceLock::get) {
            kicker.kick();
        }
        // vCPUs held out of their guest look again at why they might go on.
        self.hold.wake();
    }

    /// Records the first exit reason and interrupts every vCPU so it can wind down.
    fn stop(&self, reason: ExitReason) {
        {
            let mut exit = lock(&self.exit);
            if exit.is_none() {
                *exit = Some(reason);
                *lock(&self.exited_at_us) = Some(crate::log::uptime_us());
            }
            self.exiting.store(true, Ordering::Release);
        }
        self.exited.notify_all();
        // Taking each lock orders the wakeup after any waiter's check of `exiting`.
        self.snapshot.wake();
        self.hold.end();
        drop(lock(&self.released));
        self.release.notify_all();
        self.kick_all();
    }

    fn wait_exit(&self) -> ExitReason {
        let mut exit = lock(&self.exit);
        loop {
            if let Some(r) = exit.clone() {
                return r;
            }
            exit = wait(&self.exited, exit);
        }
    }

    fn exiting(&self) -> bool {
        self.exiting.load(Ordering::Acquire)
    }

    /// The guest asked for a snapshot (on a vCPU thread, inside its MMIO write): every
    /// vCPU is kicked out of the guest, this one right after its store completes.
    fn request_snapshot(&self) {
        if self.snapshot.request() {
            self.kick_all();
        }
    }

    /// After a kick: if a snapshot is pending, park, capture this vCPU's state on its own
    /// thread (HVF requires it) once every vCPU is out and the devices are quiet, and wait
    /// until the snapshot is written (the barrier module).
    fn park_for_snapshot(&self, index: usize, vcpu: &hv::Vcpu) {
        self.snapshot
            .park(index, &|| self.exiting(), || machine::capture(vcpu, index));
    }
}

/// The running machine, which it owns: waiting for it, or dropping it, stops it and tears
/// it down in order (audit A04).
#[derive(Debug)]
pub struct Running {
    shared: Arc<Shared>,
    vm: Option<Arc<hv::Vm>>,
    threads: Vec<JoinHandle<()>>,
    /// Devices own memory the VM maps (virtio-pmem regions): they go after the VM, as
    /// guest RAM does.
    bus: Arc<machine::Bus>,
    /// Its writable layer (D190), let go of as soon as the VM is gone.
    control: Arc<Control>,
    _memory: Arc<GuestMemory>,
}

impl Running {
    /// Waits for the guest to exit, then tears the machine down.
    pub fn wait(self) -> ExitReason {
        self.wait_then(|| {})
    }

    /// [`wait`](Self::wait), calling `free` as soon as nothing of the machine writes its
    /// writable layer's file (D190), so that its host may read it without waiting on this
    /// process: once every vCPU has stopped and the device workers are quiet, or, where
    /// letting go of a file written through a shared mapping writes it back as its readers
    /// wait ([`platform::LET_GO_WRITES_BACK`], PM M278), once the layer is let go of too.
    pub fn wait_then(mut self, free: impl FnOnce()) -> ExitReason {
        let reason = self.shared.wait_exit();
        self.teardown(reason.clone(), free);
        reason
    }

    /// Stops the machine, if it runs, and takes it apart in order: every vCPU and the
    /// snapshot coordinator joined, so each vCPU was destroyed on its own thread; the
    /// device workers stopped; the VM destroyed; and only then, as the fields drop, the
    /// devices and guest memory it mapped. Again, it does nothing.
    fn teardown(&mut self, reason: ExitReason, free: impl FnOnce()) {
        self.shared.stop(reason);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        if let Some(t) = lock(&self.shared.governor).take() {
            let _ = t.join();
        }
        // A save the host asked for holds the VM and its devices until it is done.
        self.shared.await_save();
        let vm = self.vm.take();
        if vm.is_some() {
            // The device workers go quiet before the VM they raise interrupts in goes.
            self.bus.pause();
        }
        // Nothing of the machine runs now: its writable layer is what its guest left.
        let mut free = Some(free);
        if !platform::LET_GO_WRITES_BACK
            && let Some(free) = free.take()
        {
            free();
        }
        if let Some(vm) = vm {
            drop(vm);
            // Its writable layer is its file's alone from here on (D190).
            if let Some(layer) = self.control.layer() {
                // SAFETY: the VM that mapped it is gone, and nothing maps it again.
                unsafe { layer.release() };
            }
        }
        if let Some(free) = free.take() {
            free();
        }
    }
}

impl Drop for Running {
    /// A machine dropped without [`wait`](Running::wait) is stopped, not left running
    /// with nothing to own its memory (audit A04).
    fn drop(&mut self) {
        self.teardown(ExitReason::Stopped, || {});
    }
}

fn check_vcpus(vcpus: u32) -> Result<(), String> {
    if vcpus == 0 {
        return Err("at least one vCPU is required".into());
    }
    let max = max_vcpus()?;
    if vcpus > max {
        return Err(format!("{vcpus} vCPUs requested; this host supports {max}"));
    }
    Ok(())
}

/// Boots a VM.
pub fn start(cfg: &Config) -> Result<(Handle, Running), String> {
    check_host()?;
    check_vcpus(cfg.vcpus)?;
    launch(machine::build(cfg)?, cfg.snapshot.clone(), false, cfg.join)
}

/// The most stage-2 pages `snap`'s guest can have, RAM and pmem regions (each its file
/// rounded up to [`pmem::ALIGN`]): the bound on its working set. A pmem file's size is
/// its granted descriptor's where it has one, as a VM in App Sandbox reaches it alone,
/// where a look up of its path may be denied and left its pages out (review 1.11). One
/// that cannot be read adds nothing; the restore reports it.
fn guest_pages(snap: &snapshot::Snapshot) -> u64 {
    let pmem: u64 = snap
        .config
        .pmem
        .iter()
        .filter_map(|path| platform::input_metadata(path).ok())
        .map(|m| m.len().checked_next_multiple_of(pmem::ALIGN).unwrap_or(u64::MAX))
        .fold(0, u64::saturating_add);
    // Its writable layer's, head and file system (D190).
    let layer = match snap.config.layer_mib.checked_mul(1 << 20) {
        Some(0) => 0,
        Some(data) => pmem::layer_head(data).map_or(u64::MAX, |(head, _)| head.saturating_add(data)),
        None => u64::MAX,
    };
    // Its DAX windows' heads (D189).
    let windows = u64::from(snap.config.shares) + u64::from(snap.config.join_share);
    let heads = match snap.config.share_window_mib.checked_mul(1 << 20) {
        _ if !super::has_windows(&snap.config) => 0,
        Some(len) => head::window_head_len(len).map_or(u64::MAX, |each| each.saturating_mul(windows)),
        None => u64::MAX,
    };
    snap.config
        .memory_mib
        .saturating_mul(1 << 20)
        .saturating_add(pmem)
        .saturating_add(layer)
        .saturating_add(heads)
        .div_ceil(machine::PAGE)
}

/// Resumes the VM a snapshot holds, in this process.
pub fn restore(cfg: &RestoreConfig) -> Result<(Handle, Running), String> {
    check_host()?;
    restore_from(cfg, snapshot::read(&cfg.dir)?)
}

/// [`restore`] of `pinned`, the snapshot in `cfg.dir` already read, on a host its caller
/// has checked ([`check_host`]): a restore that read it to learn its backing files reads
/// and decodes it once, and resumes the very generation it checked (review 1.12).
pub fn restore_from(cfg: &RestoreConfig, pinned: snapshot::Pinned) -> Result<(Handle, Running), String> {
    let snapshot::Pinned {
        snapshot: snap,
        memory: memory_file,
        path: generation,
        name,
    } = pinned;
    check_vcpus(snap.config.vcpus)?;
    let mut working_set = if cfg.prefetch {
        snapshot::read_working_set(&generation, machine::PAGE, guest_pages(&snap)).unwrap_or_else(|e| {
            warn!("{e}; restoring without prefetching it");
            None
        })
    } else {
        None
    };
    if let Some(ws) = &mut working_set {
        let cleared = hv::budget_writes(ws, machine::PAGE, hv::PREFETCH_PRIVATE);
        if cleared > 0 {
            debug!("{cleared} written pages past the prefetch's private budget are read instead");
        }
    }
    let recording = cfg.record && working_set.is_none();
    // Its writable layer (D190): the head as the snapshot kept it, and the restore's own
    // copy of its file system, this very generation's: the clone of it the restore was
    // given, or a copy made here into the file it was given, empty or a clone of another
    // generation's. One the snapshot and the restore do not agree on is refused as the
    // machine is checked (`check_hosts`).
    let layer = match &cfg.layer {
        Some(path) if snap.config.layer_mib > 0 => {
            let at = |e: std::io::Error| format!("{}: its writable layer: {e}", generation.display());
            let head = platform::open_beneath_input(&generation, &[snapshot::LAYER_HEAD]).map_err(at)?;
            let kept = platform::open_beneath_input(&generation, &[snapshot::LAYER]).map_err(at)?;
            let file = platform::open_input(path, true).map_err(|e| format!("{}: {e}", path.display()))?;
            let len = kept.metadata().map_err(at)?.len();
            let given = file.metadata().map_err(at)?.len();
            let copies = copies_layer(given, len, cfg.layer_cloned.as_deref(), &name)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            if copies {
                platform::copy_used(&kept, &file, len).map_err(at)?;
                // Written back as it restores, ahead of its run, rather than as the VM lets
                // go of the file, where its host waits to read what its run left (macOS, PM
                // M278), on a thread of its own: the restore goes on.
                platform::write_back_ahead(&file).map_err(at)?;
            }
            Some(Arc::new(pmem::Layer::restore(head, file)?))
        }
        Some(path) => {
            return Err(format!(
                "{}: a writable layer for a snapshot that has none",
                path.display()
            ));
        }
        None => None,
    };
    // Its DAX windows' heads (D189), as the snapshot kept them.
    let window_heads = super::has_windows(&snap.config)
        .then(|| {
            platform::open_beneath_input(&generation, &[snapshot::WINDOW_HEADS])
                .map_err(|e| format!("{}: its DAX windows' heads: {e}", generation.display()))
        })
        .transpose()?;
    let machine = machine::restore(
        &snap,
        &memory_file,
        cfg.console,
        super::Hosts {
            vsock: cfg.vsock.as_ref(),
            net: cfg.net.as_ref(),
            shares: &cfg.shares,
            join: cfg.join,
            #[cfg(unix)]
            join_share: cfg.join_share,
            io: cfg.io_limit,
            layer: layer.as_ref(),
            window_heads: window_heads.as_ref(),
        },
        working_set.unwrap_or_default(),
    )?;
    // The run's swap a snapshot of a run kept (D176), back where its guest had it before
    // any vCPU runs.
    let swap = cfg
        .swap
        .as_ref()
        .map(std::fs::File::try_clone)
        .transpose()
        .map_err(|e| format!("its swap file: {e}"))?;
    // Held, it may be given later, before its vCPUs run (`Handle::restore_swap`).
    match machine.control.swap() {
        Some(window) if swap.is_some() || !cfg.hold => window.restore_run(swap)?,
        Some(_) => {}
        None if swap.is_some() => return Err("a run's swap for a microVM without a swap window".into()),
        None => {}
    }
    // A working set is saved into the generation it was recorded from.
    let recorder = if recording {
        machine::recorder(&machine).map(|r| (r, name))
    } else {
        None
    };
    info!(
        "restored a {} MiB guest from {}",
        snap.config.memory_mib,
        cfg.dir.display()
    );
    let (handle, running) = launch(machine, cfg.snapshot.clone(), cfg.hold, cfg.join)?;
    if recorder.is_some() {
        *lock(&handle.shared.recording) = recorder;
    }
    Ok((handle, running))
}

/// The most bytes a working set of the snapshot in `dir` can take encoded: its header and
/// 8 bytes for each page its guest has (`guest_pages`). The daemon takes no more of one.
pub fn working_set_limit(dir: &Path) -> Result<u64, String> {
    let pinned = snapshot::read(dir)?;
    guest_pages(&pinned.snapshot)
        .checked_mul(8)
        .and_then(|b| b.checked_add(snapshot::WORKING_SET_HEADER))
        .ok_or_else(|| format!("{}: a guest too large to bound", dir.display()))
}

/// Whether a restore of generation `pinned` copies its writable layer, `len` bytes, into
/// the file it was given, `given` bytes long (D190): not where the file is a clone of that
/// very generation's layer (`cloned`); where it is empty, or a clone of another
/// generation's, which a template's newer one replaced before the restore read it. A file
/// that is neither is refused: it might hold a run's own.
fn copies_layer(given: u64, len: u64, cloned: Option<&str>, pinned: &str) -> Result<bool, String> {
    match cloned {
        Some(of) if of == pinned && given == len => Ok(false),
        Some(_) => Ok(true),
        None if given == 0 => Ok(true),
        None => Err("a writable layer to copy its snapshot's into must be empty".into()),
    }
}

/// Writes the working set a VM recorded from generation `name` of the snapshot in `dir`,
/// for the daemon (`snapshot::accept_working_set`): at this host's page size, within the
/// snapshot's guest (`guest_pages`).
pub fn accept_working_set(dir: &Path, name: &str, bytes: &[u8]) -> Result<usize, String> {
    snapshot::accept_working_set(dir, name, bytes, machine::PAGE, guest_pages)
}

/// Keeps the working set a restore handed its spawner (`--working-set FD`) beside the
/// generation of the snapshot in `dir` it names, as [`accept_working_set`] keeps a warm
/// VM's (`snapshot::split_handed`): the pages kept, 0 where that generation is no longer
/// the snapshot's.
pub fn keep_working_set(dir: &Path, handed: &[u8]) -> Result<usize, String> {
    let (name, bytes) = snapshot::split_handed(handed)?;
    accept_working_set(dir, name, bytes)
}

/// Starts the machine's vCPUs; with `hold`, they wait for [`Handle::release`].
fn launch(
    m: Machine,
    snapshots: Option<SnapshotPolicy>,
    hold: bool,
    join: Option<&'static crate::devices::virtio::block::Join>,
) -> Result<(Handle, Running), String> {
    let Machine {
        vm,
        memory,
        bus,
        serial,
        control,
        power,
        finish,
        start,
        config,
        // What a restore's recorder read, which it took before the launch.
        #[cfg(all(hv = "kvm", target_arch = "aarch64"))]
            pmem: _,
    } = m;
    let vcpus = config.vcpus;
    // Each vCPU is moved its own part of the start, and the machine keeps the rest: none
    // of it is shared, and each part goes as soon as its owner is done with it.
    let start_bytes = machine::start_bytes(&start);
    let (starts, mut rest) = machine::split(start, vcpus as usize)?;
    // The bus before the VM, so that an early return drops the VM first: the hypervisor
    // maps the bus's device memory until the VM is destroyed (Machine's own order).
    let bus = Arc::new(bus);
    let vm = Arc::new(vm);
    let shared = Arc::new(Shared::new(vcpus));
    let sh = shared.clone();
    power.on_event(Box::new(move |event| {
        sh.stop(match event {
            PowerEvent::Off => ExitReason::PowerOff,
            PowerEvent::Reset => ExitReason::Reset,
        })
    }));

    let _ = shared.parts.set(Parts {
        vm: Arc::downgrade(&vm),
        bus: Arc::downgrade(&bus),
        config: config.clone(),
        join,
    });
    let mut threads = Vec::with_capacity(vcpus as usize + 1);
    if let Some(policy) = snapshots {
        shared.template.store(true, Ordering::Release);
        let sh = shared.clone();
        // One snapshot per VM, a template's: a guest's later requests are passed by,
        // rather than each parking every vCPU and writing all of memory again.
        let asked = std::sync::atomic::AtomicBool::new(false);
        control.on_snapshot(Box::new(move || {
            if !asked.swap(true, std::sync::atomic::Ordering::Relaxed) {
                sh.request_snapshot();
            }
        }));
        let job = Coordinator {
            sh: shared.clone(),
            vm: vm.clone(),
            bus: bus.clone(),
            memory: memory.clone(),
            control: control.clone(),
            config,
            policy,
            join,
        };
        match std::thread::Builder::new()
            .name("snapshot".into())
            .spawn(move || job.run())
        {
            Ok(t) => threads.push(t),
            Err(e) => return Err(format!("spawning the snapshot thread: {e}")),
        }
    }

    // vCPUs are created strictly in index order (D5).
    let mut first = None;
    // Each one's setup, its thread's spawn to its answer: where a slow start's time goes.
    let (setups, mut slowest) = (crate::log::uptime_us(), (0, 0u128));
    for (index, start) in starts.into_iter().enumerate() {
        let began = crate::log::uptime_us();
        let (created_tx, created_rx) = mpsc::channel();
        let (vm, sh, bus) = (vm.clone(), shared.clone(), bus.clone());
        let spawned = std::thread::Builder::new()
            .name(format!("vcpu{index}"))
            .spawn(move || vcpu_thread(&vm, &sh, &*bus, index, start, &created_tx));
        match spawned {
            Ok(t) => threads.push(t),
            Err(e) => {
                shared.stop(ExitReason::Error(format!("spawning vCPU {index}: {e}")));
                break;
            }
        }
        match created_rx.recv() {
            Ok(Ok(set_up)) => {
                if index == 0 {
                    first = Some(set_up);
                }
                let took = crate::log::uptime_us().saturating_sub(began);
                if took > slowest.1 {
                    slowest = (index, took);
                }
            }
            Ok(Err(e)) => {
                shared.stop(ExitReason::Error(format!("vCPU {index}: {e}")));
                break;
            }
            Err(_) => {
                shared.stop(ExitReason::Error(format!(
                    "vCPU {index} thread died during setup"
                )));
                break;
            }
        }
    }
    if !shared.exiting() {
        let finishing = crate::log::uptime_us();
        debug!(
            "vCPUs set up in {} us, the slowest vCPU {} in {} us",
            finishing.saturating_sub(setups),
            slowest.0,
            slowest.1
        );
        let finished = machine::finish(&vm, &bus, &finish, rest.as_mut());
        debug!(
            "the machine's start finished in {} us",
            crate::log::uptime_us().saturating_sub(finishing)
        );
        let _ = shared
            .kept
            .set(machine::keep(rest.as_mut(), &first.unwrap_or_default()));
        // The vCPUs dropped theirs once set up: the snapshot's device state goes now, where
        // a warm VM held it for its whole life.
        drop(rest);
        if let Some([vcpus, devices, working_set]) = start_bytes {
            info!(
                "dropped the restore's state: {vcpus} bytes of vCPU state, {devices} of \
                 devices, {working_set} of working set"
            );
        }
        match finished {
            Ok(()) if !hold => shared.release_vcpus(),
            Ok(()) => {}
            Err(e) => shared.stop(ExitReason::Error(e)),
        }
    }
    info!(
        "{vcpus} vCPU(s) ready to run after {} us",
        crate::log::uptime_us()
    );
    let handle_shared = shared.clone();
    let control_running = control.clone();
    Ok((
        Handle {
            shared,
            serial,
            control,
            memory: memory.clone(),
        },
        Running {
            shared: handle_shared,
            vm: Some(vm),
            threads,
            bus,
            control: control_running,
            _memory: memory,
        },
    ))
}

fn vcpu_thread(
    vm: &hv::Vm,
    sh: &Shared,
    io: &dyn hv::Io,
    index: usize,
    start: machine::VcpuStart,
    created: &mpsc::Sender<Result<machine::SetUp, String>>,
) {
    // Held while the vCPU runs.
    let _prioritized = match platform::prioritize_vcpu_thread() {
        Ok(prioritized) => Some(prioritized),
        Err(e) => {
            if index == 0 {
                warn!("vCPU threads run without real-time policy (coarser guest timers): {e}");
            }
            None
        }
    };
    // Its part of the start goes with its setup.
    let (mut vcpu, set_up) = match machine::setup_vcpu(vm, index, start) {
        Ok(v) => v,
        Err(e) => {
            let _ = created.send(Err(e));
            return;
        }
    };
    if let Some(slot) = sh.kickers.get(index) {
        let _ = slot.set(vcpu.kicker());
    }
    // Its clock, by which the host holds the vCPUs to their run's CPUs (D191).
    if let Err(e) = sh.hold.register(index) {
        warn!("vCPU {index}: its CPU time unread, so not held to its run's CPUs: {e}");
    }
    let _ = created.send(Ok(set_up));
    if let Some(offset) = sh.wait_release()
        && let Err(e) = machine::set_counter_offset(&mut vcpu, offset)
    {
        return sh.stop(ExitReason::Error(format!("vCPU {index}: {e}")));
    }
    if index == 0 {
        let _ = sh.entered_at_us.set(crate::log::uptime_us());
    }
    loop {
        if sh.exiting() {
            return;
        }
        // Out of the guest while its run's CPU time is spent (D191), or until a snapshot
        // needs it parked.
        sh.hold.wait(&|| sh.exiting() || sh.snapshot.requested());
        match vcpu.run(io) {
            Ok(hv::Exit::Canceled) => sh.park_for_snapshot(index, &vcpu),
            Ok(hv::Exit::Shutdown) => return sh.stop(ExitReason::PowerOff),
            Ok(hv::Exit::Reset) => return sh.stop(ExitReason::Reset),
            Err(e) => return sh.stop(ExitReason::Error(format!("vCPU {index}: {e}"))),
        }
    }
}

/// Writes the snapshot the guest asks for, once every vCPU has parked.
struct Coordinator {
    sh: Arc<Shared>,
    vm: Arc<hv::Vm>,
    bus: Arc<machine::Bus>,
    memory: Arc<GuestMemory>,
    control: Arc<Control>,
    config: MachineConfig,
    policy: SnapshotPolicy,
    /// The machine's join disk (D119), which a snapshot keeps only empty.
    join: Option<&'static Join>,
}

impl Coordinator {
    fn run(self) {
        let stopping = || self.sh.exiting();
        let parked = self.sh.snapshot.wait_parked(&stopping);
        self.sh.template.store(false, Ordering::Release);
        if !parked {
            return;
        }
        let t0 = crate::log::uptime_us();
        *lock(&self.sh.committing) = true;
        self.take(t0);
        *lock(&self.sh.committing) = false;
        self.sh.committed.notify_all();
    }

    fn cut(&self) -> Cut<'_> {
        Cut {
            sh: &self.sh,
            vm: &self.vm,
            bus: &self.bus,
            memory: &self.memory,
            config: &self.config,
            join: self.join,
            swap: self.control.swap().map(|w| &**w),
        }
    }

    /// Takes the snapshot, its vCPUs parked. A VM that resumes after it serves its run
    /// whatever becomes of the snapshot: one that fails is only a template lost, which
    /// the daemon finds missing; a VM that only saves one ends with why.
    fn take(&self, t0: u128) {
        let resume = matches!(self.policy.then, AfterSnapshot::Resume);
        let cut = self.cut();
        // `parked`: whether the guest still waits for the snapshot.
        let failed = |e: String, parked: bool| {
            if !resume {
                return self.sh.stop(ExitReason::Error(e));
            }
            // One whose guest lost a mapping taken from it for the save stops.
            if self.control.lost() {
                return self.sh.stop(ExitReason::Error(e));
            }
            warn!("{e}; the VM goes on without its snapshot");
            if parked {
                cut.go_on();
            }
        };
        let snap = match cut.take(false) {
            Ok(Some(snap)) => snap,
            Ok(None) => return,
            Err(e) => return failed(format!("snapshot: {e}"), true),
        };
        let beside = snapshot::Beside {
            layer: self.control.layer().map(|l| &**l),
            window_heads: self.control.window_heads().map(|h| &**h),
        };
        let staged = match snapshot::stage(&self.policy.dir, &snap, &self.memory, beside) {
            Ok(staged) => staged,
            Err(e) => return failed(format!("snapshot: {e}"), true),
        };
        info!(
            "snapshot written to {} in {} us",
            self.policy.dir.display(),
            crate::log::uptime_us().saturating_sub(t0)
        );
        if resume {
            // Before a working set is recorded, which takes the head from the guest too.
            match self.remap_head(&staged) {
                Ok(()) => {
                    if self.policy.working_set {
                        self.record(staged.name().to_string());
                    }
                    cut.go_on();
                }
                Err(e) => self.sh.stop(ExitReason::Error(e)),
            }
        }
        // Durable, and in use, with the guest running again: its files hold all of it
        // already (PM M63). The process ends only once this thread has.
        let t1 = crate::log::uptime_us();
        if let Err(e) = staged.commit() {
            return failed(format!("snapshot: {e}"), false);
        }
        info!(
            "snapshot made durable in {} us",
            crate::log::uptime_us().saturating_sub(t1)
        );
        if !resume {
            self.sh.stop(ExitReason::Snapshotted);
        }
    }

    /// Maps the writable layer's head back from the copy the snapshot just kept of it, with
    /// every vCPU parked, for the machine that goes on: a template's, its image's first run,
    /// which holds of its head from then on what its guest writes alone (D190,
    /// [`pmem::Layer::remap_head`]). One that fails leaves the head the guest's own memory,
    /// and is said; an error is a head its guest cannot be given back.
    fn remap_head(&self, staged: &snapshot::Staged) -> Result<(), String> {
        self.remap_window_heads(staged)?;
        let (Some(layer), Some(kept)) = (self.control.layer(), staged.layer_head()) else {
            return Ok(());
        };
        let t0 = crate::log::uptime_us();
        // SAFETY: every vCPU is parked and the devices quiet since the head was saved into
        // `kept`.
        match unsafe { layer.remap_head(kept) } {
            Ok(()) => info!(
                "its writable layer's head mapped back from its snapshot in {} us",
                crate::log::uptime_us().saturating_sub(t0)
            ),
            Err(e) if layer.lost() => return Err(format!("its writable layer's head: {e}")),
            Err(e) => warn!("its writable layer's head stays its own memory where not mapped back: {e}"),
        }
        Ok(())
    }

    /// Maps the DAX windows' heads back from the copy the snapshot just kept of them, as
    /// [`remap_head`](Self::remap_head) does the writable layer's (D189,
    /// [`WindowHeads::remap`](crate::devices::virtio::head::WindowHeads::remap)).
    fn remap_window_heads(&self, staged: &snapshot::Staged) -> Result<(), String> {
        let (Some(heads), Some(kept)) = (self.control.window_heads(), staged.window_heads()) else {
            return Ok(());
        };
        let t0 = crate::log::uptime_us();
        // SAFETY: every vCPU is parked and the devices quiet since the heads were saved
        // into `kept`.
        match unsafe { heads.remap(kept) } {
            Ok(parts) => info!(
                "its DAX windows' heads mapped back from its snapshot in {} us: {parts}",
                crate::log::uptime_us().saturating_sub(t0)
            ),
            Err(e) if heads.lost() => return Err(format!("its DAX windows' heads: {e}")),
            Err(e) => warn!("its DAX windows' heads stay its own memory where not mapped back: {e}"),
        }
        Ok(())
    }

    /// Starts recording the working set of `generation`, the snapshot just written, with
    /// every vCPU parked, unless one is being recorded already. Without one, restores just
    /// run.
    fn record(&self, generation: String) {
        let mut recording = lock(&self.sh.recording);
        if recording.is_some() {
            return;
        }
        match machine::record(&self.vm).map(|r| r.map(|r| (r, generation))) {
            Ok(r) => *recording = r,
            Err(e) => warn!("not recording a working set: {e}"),
        }
    }
}

/// A machine's parts, which a snapshot reads once every vCPU has parked: the guest's or
/// the host's (D176).
struct Cut<'a> {
    sh: &'a Shared,
    vm: &'a hv::Vm,
    bus: &'a machine::Bus,
    memory: &'a GuestMemory,
    config: &'a MachineConfig,
    /// The machine's join disk (D119), which a snapshot keeps only empty.
    join: Option<&'static Join>,
    /// Its swap window (D159), where it has one.
    swap: Option<&'a pmem::SwapWindow>,
}

impl Cut<'_> {
    /// Quiets the devices, refuses what a snapshot cannot hold, and takes the machine's
    /// state, each vCPU's captured on its own thread; none once the machine stops. With
    /// `run`, a run's snapshot, which keeps the run's swap beside it; a template keeps
    /// none. The devices stay quiet and the vCPUs parked until [`go_on`](Cut::go_on).
    fn take(&self, run: bool) -> Result<Option<Snapshot>, String> {
        // Every vCPU is out of the guest: the devices go quiet, and only then does each
        // vCPU capture its state.
        self.bus.pause();
        // A snapshot keeps no image of a join disk's: a container's image is the joining
        // run's, and every restore of a snapshot is a microVM no container has joined. It
        // keeps the ranges once let go, as the joiners ended (D176).
        if self.join.is_some_and(Join::attached) {
            return Err(
                "a container joined to this microVM still has its image attached, which a snapshot cannot keep"
                    .into(),
            );
        }
        if !run && self.swap.is_some_and(pmem::SwapWindow::attached) {
            return Err(
                "a run's swap is mapped into the microVM's swap window, which a template keeps none of"
                    .into(),
            );
        }
        // What a paused device cannot save refuses the snapshot, not lost in it (D148).
        if let Some(why) = self.bus.unsaved() {
            return Err(why);
        }
        let captured = match self.sh.snapshot.capture(&|| self.sh.exiting()) {
            None => return Ok(None),
            Some(captured) => captured?,
        };
        let arch = machine::encode_state(self.vm, captured)?;
        let mut w = Writer::default();
        self.bus.save(&mut w);
        Ok(Some(Snapshot {
            config: self.config.clone(),
            plugged: self.memory.plugged_runs(),
            arch,
            devices: w.into_bytes(),
        }))
    }

    /// Lets the machine run again after its snapshot.
    fn go_on(&self) {
        match self.bus.resume() {
            Ok(()) => self.sh.snapshot.release(),
            Err(e) => self.sh.stop(ExitReason::Error(format!("resuming devices: {e}"))),
        }
    }
}

// Of granted inputs, which only Unix hosts have.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A restore copies its generation's writable layer into an empty file or into a clone
    /// of another generation's, takes a clone of its own generation's as it is, and refuses
    /// a file that is neither (D190).
    #[test]
    fn a_restore_copies_its_layer_unless_given_its_own_generations_clone() {
        assert_eq!(copies_layer(0, 8 << 20, None, "g-2"), Ok(true));
        assert_eq!(copies_layer(8 << 20, 8 << 20, Some("g-2"), "g-2"), Ok(false));
        // Its template saved again between the clone and the restore's read.
        assert_eq!(copies_layer(8 << 20, 8 << 20, Some("g-1"), "g-2"), Ok(true));
        // No whole clone: made again.
        assert_eq!(copies_layer(4 << 20, 8 << 20, Some("g-2"), "g-2"), Ok(true));
        assert!(copies_layer(4096, 8 << 20, None, "g-2").is_err());
    }

    /// A pmem file's pages count whether the process reaches its path or only the
    /// descriptor granted for it, as a VM in App Sandbox does (review 1.11): each file
    /// at its size rounded up to the alignment pmem maps it at, beside the RAM.
    #[test]
    fn a_granted_pmem_files_pages_count() {
        let file_dir = shards_testdir::TempDir::new("guest-pages").unwrap();
        let file = file_dir.join("guest-pages");
        std::fs::write(&file, vec![0u8; 3 << 20]).unwrap();
        let granted = std::path::PathBuf::from(format!("/nowhere/shards-{}.erofs", std::process::id()));
        platform::grant_input(granted.clone(), Some(std::fs::File::open(&file).unwrap().into()));
        let snap = Snapshot {
            config: MachineConfig {
                vcpus: 1,
                memory_mib: 16,
                disks: Vec::new(),
                pmem: vec![granted, file.clone()],
                vsock: false,
                net: None,
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
            },
            plugged: Vec::new(),
            arch: Vec::new(),
            devices: Vec::new(),
        };
        assert_eq!(pmem::ALIGN, 2 << 20);
        assert_eq!(guest_pages(&snap), ((16 + 4 + 4) << 20) / machine::PAGE);
        let _ = std::fs::remove_file(&file);
    }
}
