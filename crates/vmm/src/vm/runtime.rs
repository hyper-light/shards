//! vCPU threads and the VM lifecycle, on any backend and architecture: boot or restore,
//! run, snapshot, stop.

use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::thread::JoinHandle;

use super::barrier::Barrier;
use super::machine::{self, Machine, Start};
use super::{AfterSnapshot, Config, ExitReason, RestoreConfig, SnapshotPolicy};
use crate::devices::control::Control;
use crate::devices::power::PowerEvent;
use crate::devices::serial::Serial;
use crate::devices::virtio::pmem;
use crate::hv;
use crate::memory::GuestMemory;
use crate::snapshot::{self, MachineConfig, Snapshot, codec::Writer};
use crate::sync::{lock, wait};
use crate::{info, platform, warn};

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

    /// Stops recording the working set, and saves it with the snapshot the VM resumed
    /// from. Returns how many pages it holds: 0 when nothing was being recorded.
    pub fn save_working_set(&self) -> Result<usize, String> {
        let Some((recorder, dir)) = lock(&self.shared.recording).take() else {
            return Ok(0);
        };
        let pages = machine::recorded(&recorder)?;
        if !pages.is_empty() {
            snapshot::write_working_set(&dir, &pages, machine::PAGE)?;
        }
        Ok(pages.len())
    }

    /// How many pages of its snapshot's working set this VM prefetched before it ran.
    pub fn prefetched(&self) -> usize {
        machine::prefetched(&self.shared.start)
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
    /// A restored guest's counter at its snapshot.
    start: Arc<Start>,
    /// When the vCPUs were released (µs since VMM start).
    released_at_us: OnceLock<u128>,
    /// While a working set is recorded: what records it, and the directory of the
    /// snapshot it goes with, held open since the daemon may rename it.
    recording: Mutex<Option<(machine::Recorder, File)>>,
}

#[derive(Debug, Default)]
struct Release {
    done: bool,
    /// For a restore: the counter offset every vCPU applies, taken at release so the
    /// guest counter continues from the snapshot with no jump.
    counter_offset: Option<u64>,
}

impl Shared {
    fn new(vcpus: u32, start: Arc<Start>) -> Shared {
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
            start,
            released_at_us: OnceLock::new(),
            recording: Mutex::new(None),
        }
    }

    /// Lets every created vCPU run (once).
    fn release_vcpus(&self) {
        let mut r = lock(&self.released);
        if r.done {
            return;
        }
        match machine::release_offset(&self.start) {
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
/// rounded up to [`pmem::ALIGN`]): the bound on its working set. A pmem file that cannot
/// be read adds nothing; the restore reports it.
fn guest_pages(snap: &snapshot::Snapshot) -> u64 {
    let pmem: u64 = snap
        .config
        .pmem
        .iter()
        .filter_map(|path| std::fs::metadata(path).ok())
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
    let snapshot::Pinned {
        snapshot: snap,
        memory: memory_file,
        generation,
    } = snapshot::read(&cfg.dir)?;
    check_vcpus(snap.config.vcpus)?;
    let working_set = if cfg.prefetch {
        snapshot::read_working_set(&generation, machine::PAGE, guest_pages(&snap)).unwrap_or_else(|e| {
            warn!("{e}; restoring without prefetching it");
            None
        })
    } else {
        None
    };
    let recording = cfg.record && working_set.is_none();
    let machine = machine::restore(
        &snap,
        &memory_file,
        cfg.console,
        cfg.vsock.as_deref(),
        working_set.unwrap_or_default(),
    )?;
    // A working set is saved into the generation it was recorded from.
    let recorder = if recording {
        machine::recorder(&machine).map(|r| (r, generation))
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
    let vm = Arc::new(vm);
    let bus = Arc::new(bus);
    let start = Arc::new(start);
    let shared = Arc::new(Shared::new(vcpus, start.clone()));
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
        control.on_snapshot(Box::new(move || sh.request_snapshot()));
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
    for index in 0..vcpus as usize {
        let (created_tx, created_rx) = mpsc::channel();
        let (vm, sh, bus, start) = (vm.clone(), shared.clone(), bus.clone(), start.clone());
        let spawned = std::thread::Builder::new()
            .name(format!("vcpu{index}"))
            .spawn(move || vcpu_thread(&vm, &sh, &*bus, index, &start, &created_tx));
        match spawned {
            Ok(t) => threads.push(t),
            Err(e) => {
                shared.stop(ExitReason::Error(format!("spawning vCPU {index}: {e}")));
                break;
            }
        }
        match created_rx.recv() {
            Ok(Ok(())) => {}
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
        match machine::finish(&vm, &bus, &finish, &start) {
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
    start: &Start,
    created: &mpsc::Sender<Result<(), String>>,
) {
    if let Err(e) = platform::prioritize_vcpu_thread()
        && index == 0
    {
        warn!("vCPU threads run without real-time policy (coarser guest timers): {e}");
    }
    let mut vcpu = match machine::setup_vcpu(vm, index, start) {
        Ok(v) => v,
        Err(e) => {
            let _ = created.send(Err(e));
            return;
        }
    };
    if let Some(slot) = sh.kickers.get(index) {
        let _ = slot.set(vcpu.kicker());
    }
    let _ = created.send(Ok(()));
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

/// Writes a snapshot whenever the guest asks, once every vCPU has parked.
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
        loop {
            if !self.sh.snapshot.wait_parked(&stopping) {
                return;
            }
            let t0 = crate::log::uptime_us();
            // Every vCPU is out of the guest: the devices go quiet, and only then does each
            // vCPU capture its state.
            self.bus.pause();
            let captured = match self.sh.snapshot.capture(&stopping) {
                None => return,
                Some(Err(e)) => return self.sh.stop(ExitReason::Error(format!("snapshot: {e}"))),
                Some(Ok(captured)) => captured,
            };
            let generation = match self.write(captured) {
                Ok(generation) => generation,
                Err(e) => return self.sh.stop(ExitReason::Error(format!("snapshot: {e}"))),
            };
            info!(
                "snapshot written to {} in {} us",
                self.policy.dir.display(),
                crate::log::uptime_us().saturating_sub(t0)
            );
            match self.policy.then {
                AfterSnapshot::Stop => return self.sh.stop(ExitReason::Snapshotted),
                AfterSnapshot::Resume => {
                    if self.policy.working_set {
                        self.record(generation);
                    }
                    if let Err(e) = self.bus.resume() {
                        return self.sh.stop(ExitReason::Error(format!("resuming devices: {e}")));
                    }
                    self.sh.snapshot.release();
                }
            }
        }
    }

    /// Starts recording the working set of `generation`, the snapshot just written, with
    /// every vCPU parked, unless one is being recorded already. Without one, restores just
    /// run.
    fn record(&self, generation: File) {
        let mut recording = lock(&self.sh.recording);
        if recording.is_some() {
            return;
        }
        match machine::record(&self.vm).map(|r| r.map(|r| (r, generation))) {
            Ok(r) => *recording = r,
            Err(e) => warn!("not recording a working set: {e}"),
        }
    }

    /// Saves the interrupt controller, the devices and memory, of a machine whose vCPUs
    /// have parked with their state captured and whose devices are quiet. Returns the new
    /// generation's directory, held open.
    fn write(&self, captured: Vec<machine::Captured>) -> Result<File, String> {
        let arch = machine::encode_state(&self.vm, captured)?;
        let mut w = Writer::default();
        self.bus.save(&mut w);
        let snap = Snapshot {
            config: self.config.clone(),
            arch,
            devices: w.into_bytes(),
        };
        snapshot::write(&self.policy.dir, &snap, &self.memory)
    }
}
