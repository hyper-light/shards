//! vCPU threads and the VM lifecycle, on any backend and architecture: boot or restore,
//! run, snapshot, stop.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::thread::JoinHandle;

use super::barrier::Barrier;
use super::machine::{self, Machine};
use super::{AfterSnapshot, Config, ExitReason, RestoreConfig, SnapshotPolicy};
use crate::devices::control::Control;
use crate::devices::power::PowerEvent;
use crate::devices::serial::Serial;
use crate::devices::virtio::pmem;
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
}

impl Handle {
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
    /// is all a start request costs.
    pub fn release(&self) {
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
    _memory: Arc<GuestMemory>,
}

impl Running {
    /// Waits for the guest to exit, then tears the machine down.
    pub fn wait(mut self) -> ExitReason {
        let reason = self.shared.wait_exit();
        self.teardown(reason.clone());
        reason
    }

    /// Stops the machine, if it runs, and takes it apart in order: every vCPU and the
    /// snapshot coordinator joined, so each vCPU was destroyed on its own thread; the
    /// device workers stopped; the VM destroyed; and only then, as the fields drop, the
    /// devices and guest memory it mapped. Again, it does nothing.
    fn teardown(&mut self, reason: ExitReason) {
        self.shared.stop(reason);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        if let Some(vm) = self.vm.take() {
            // The device workers go quiet before the VM they raise interrupts in goes.
            self.bus.pause();
            drop(vm);
        }
    }
}

impl Drop for Running {
    /// A machine dropped without [`wait`](Running::wait) is stopped, not left running
    /// with nothing to own its memory (audit A04).
    fn drop(&mut self) {
        self.teardown(ExitReason::Stopped);
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
    launch(machine::build(cfg)?, cfg.snapshot.clone(), false)
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
    snap.config
        .memory_mib
        .saturating_mul(1 << 20)
        .saturating_add(pmem)
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
    let machine = machine::restore(
        &snap,
        &memory_file,
        cfg.console,
        super::Hosts {
            vsock: cfg.vsock.as_ref(),
            #[cfg(unix)]
            net: cfg.net.as_ref(),
        },
        working_set.unwrap_or_default(),
    )?;
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
    let (handle, running) = launch(machine, cfg.snapshot.clone(), cfg.hold)?;
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

/// Writes the working set a VM recorded from generation `name` of the snapshot in `dir`,
/// for the daemon (`snapshot::accept_working_set`): at this host's page size, within the
/// snapshot's guest (`guest_pages`).
pub fn accept_working_set(dir: &Path, name: &str, bytes: &[u8]) -> Result<usize, String> {
    snapshot::accept_working_set(dir, name, bytes, machine::PAGE, guest_pages)
}

/// Starts the machine's vCPUs; with `hold`, they wait for [`Handle::release`].
fn launch(m: Machine, snapshots: Option<SnapshotPolicy>, hold: bool) -> Result<(Handle, Running), String> {
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
    } = m;
    let vcpus = config.vcpus;
    // Each vCPU is moved its own part of the start, and the machine keeps the rest: none
    // of it is shared, and each part goes as soon as its owner is done with it.
    let start_bytes = machine::start_bytes(&start);
    let (starts, rest) = machine::split(start, vcpus as usize)?;
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

    let mut threads = Vec::with_capacity(vcpus as usize + 1);
    if let Some(policy) = snapshots {
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
            config,
            policy,
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
    for (index, start) in starts.into_iter().enumerate() {
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
        let finished = machine::finish(&vm, &bus, &finish, rest.as_ref());
        let _ = shared
            .kept
            .set(machine::keep(rest.as_ref(), &first.unwrap_or_default()));
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
    Ok((
        Handle {
            shared,
            serial,
            control,
        },
        Running {
            shared: handle_shared,
            vm: Some(vm),
            threads,
            bus,
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
    if let Err(e) = platform::prioritize_vcpu_thread()
        && index == 0
    {
        warn!("vCPU threads run without real-time policy (coarser guest timers): {e}");
    }
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
    config: MachineConfig,
    policy: SnapshotPolicy,
}

impl Coordinator {
    fn run(self) {
        let stopping = || self.sh.exiting();
        if !self.sh.snapshot.wait_parked(&stopping) {
            return;
        }
        let t0 = crate::log::uptime_us();
        *lock(&self.sh.committing) = true;
        self.take(t0);
        *lock(&self.sh.committing) = false;
        self.sh.committed.notify_all();
    }

    /// Takes the snapshot, its vCPUs parked. A VM that resumes after it serves its run
    /// whatever becomes of the snapshot: one that fails is only a template lost, which
    /// the daemon finds missing; a VM that only saves one ends with why.
    fn take(&self, t0: u128) {
        let stopping = || self.sh.exiting();
        let resume = matches!(self.policy.then, AfterSnapshot::Resume);
        let go_on = || match self.bus.resume() {
            Ok(()) => self.sh.snapshot.release(),
            Err(e) => self.sh.stop(ExitReason::Error(format!("resuming devices: {e}"))),
        };
        // `parked`: whether the guest still waits for the snapshot.
        let failed = |e: String, parked: bool| {
            if !resume {
                return self.sh.stop(ExitReason::Error(e));
            }
            warn!("{e}; the VM goes on without its snapshot");
            if parked {
                go_on();
            }
        };
        // Every vCPU is out of the guest: the devices go quiet, and only then does each
        // vCPU capture its state.
        self.bus.pause();
        let captured = match self.sh.snapshot.capture(&stopping) {
            None => return,
            Some(Err(e)) => return failed(format!("snapshot: {e}"), true),
            Some(Ok(captured)) => captured,
        };
        let staged = match self.stage(captured) {
            Ok(staged) => staged,
            Err(e) => return failed(format!("snapshot: {e}"), true),
        };
        info!(
            "snapshot written to {} in {} us",
            self.policy.dir.display(),
            crate::log::uptime_us().saturating_sub(t0)
        );
        if resume {
            if self.policy.working_set {
                self.record(staged.name().to_string());
            }
            go_on();
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

    /// Stages the interrupt controller, the devices and memory, of a machine whose vCPUs
    /// have parked with their state captured and whose devices are quiet.
    fn stage(&self, captured: Vec<machine::Captured>) -> Result<snapshot::Staged, String> {
        let arch = machine::encode_state(&self.vm, captured)?;
        let mut w = Writer::default();
        self.bus.save(&mut w);
        let snap = Snapshot {
            config: self.config.clone(),
            arch,
            devices: w.into_bytes(),
        };
        snapshot::stage(&self.policy.dir, &snap, &self.memory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pmem file's pages count whether the process reaches its path or only the
    /// descriptor granted for it, as a VM in App Sandbox does (review 1.11): each file
    /// at its size rounded up to the alignment pmem maps it at, beside the RAM.
    #[test]
    fn a_granted_pmem_files_pages_count() {
        let file = std::env::temp_dir().join(format!("shards-guest-pages-{}", std::process::id()));
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
            },
            arch: Vec::new(),
            devices: Vec::new(),
        };
        assert_eq!(pmem::ALIGN, 2 << 20);
        assert_eq!(guest_pages(&snap), ((16 + 4 + 4) << 20) / machine::PAGE);
        let _ = std::fs::remove_file(&file);
    }
}
