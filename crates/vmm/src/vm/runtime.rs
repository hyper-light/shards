//! vCPU threads and the VM lifecycle, on any backend and architecture: boot or restore,
//! run, snapshot, stop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, mpsc};
use std::thread::JoinHandle;

use super::machine::{self, Machine, Start};
use super::{AfterSnapshot, Config, ExitReason, RestoreConfig, SnapshotPolicy};
use crate::arch::aarch64::state::VcpuState;
use crate::devices::MmioBus;
use crate::devices::control::Control;
use crate::devices::serial::Serial;
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
}

/// A snapshot in progress: vCPUs park here with their state captured.
#[derive(Debug, Default)]
struct Pause {
    requested: bool,
    /// Per vCPU, while parked: its state and its guest counter when it stopped.
    captured: Vec<Option<(VcpuState, u64)>>,
    cpu_id: Option<Vec<(u16, u64)>>,
    /// Bumped to release parked vCPUs.
    epoch: u64,
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
    pause: Mutex<Pause>,
    paused: Condvar,
    /// vCPUs wait here after creation until the machine is complete (and, for a held
    /// restore, until the request arrives).
    released: Mutex<Release>,
    release: Condvar,
    /// A restored guest's counter at its snapshot.
    resume_counter: Option<u64>,
    /// When the vCPUs were released (µs since VMM start).
    released_at_us: OnceLock<u128>,
}

#[derive(Debug, Default)]
struct Release {
    done: bool,
    /// For a restore: the counter offset every vCPU applies, taken at release so the
    /// guest counter continues from the snapshot with no jump.
    counter_offset: Option<u64>,
}

impl Shared {
    fn new(vcpus: u32, resume_counter: Option<u64>) -> Shared {
        Shared {
            kickers: (0..vcpus).map(|_| OnceLock::new()).collect(),
            exiting: AtomicBool::new(false),
            exit: Mutex::new(None),
            exited_at_us: Mutex::new(None),
            exited: Condvar::new(),
            entered_at_us: OnceLock::new(),
            pause: Mutex::new(Pause {
                captured: (0..vcpus).map(|_| None).collect(),
                ..Pause::default()
            }),
            paused: Condvar::new(),
            released: Mutex::new(Release::default()),
            release: Condvar::new(),
            resume_counter,
            released_at_us: OnceLock::new(),
        }
    }

    /// Lets every created vCPU run (once).
    fn release_vcpus(&self) {
        let mut r = lock(&self.released);
        if r.done {
            return;
        }
        r.done = true;
        r.counter_offset = self.resume_counter.map(|c| hv::host_counter().wrapping_sub(c));
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
        drop(lock(&self.pause));
        self.paused.notify_all();
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
        let mut p = lock(&self.pause);
        if p.requested {
            return;
        }
        p.requested = true;
        drop(p);
        self.paused.notify_all();
        self.kick_all();
    }

    /// After a kick: if a snapshot is pending, capture this vCPU's state on its own
    /// thread (HVF requires it) and park until the snapshot is written.
    fn park_for_snapshot(&self, index: usize, vcpu: &hv::Vcpu) -> Result<(), String> {
        if !lock(&self.pause).requested {
            return Ok(());
        }
        let e = |e: hv::Error| e.to_string();
        let state = vcpu.save_state().map_err(e)?;
        let counter = vcpu.guest_counter().map_err(e)?;
        let cpu_id = if index == 0 {
            Some(vcpu.cpu_id().map_err(e)?)
        } else {
            None
        };
        let mut p = lock(&self.pause);
        if let Some(slot) = p.captured.get_mut(index) {
            *slot = Some((state, counter));
        }
        if cpu_id.is_some() {
            p.cpu_id = cpu_id;
        }
        let epoch = p.epoch;
        self.paused.notify_all();
        while p.epoch == epoch && !self.exiting() {
            p = wait(&self.paused, p);
        }
        Ok(())
    }
}

/// Joins the vCPU threads and tears the VM down once the guest exits.
#[derive(Debug)]
pub struct Running {
    vm: Option<Arc<hv::Vm>>,
    threads: Vec<JoinHandle<()>>,
    _memory: Arc<GuestMemory>,
}

impl Running {
    pub fn wait(mut self, handle: Handle) -> ExitReason {
        let reason = handle.shared.wait_exit();
        handle.shared.stop(reason.clone());
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        // Every vCPU was destroyed on its own thread above; the VM goes before its memory.
        self.vm.take();
        reason
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

/// Resumes the VM a snapshot holds, in this process.
pub fn restore(cfg: &RestoreConfig) -> Result<(Handle, Running), String> {
    check_host()?;
    let (snap, memory_file) = snapshot::read(&cfg.dir)?;
    check_vcpus(snap.config.vcpus)?;
    let machine = machine::restore(&snap, &memory_file, cfg.console)?;
    info!(
        "restored a {} MiB guest from {}",
        snap.config.memory_mib,
        cfg.dir.display()
    );
    launch(machine, cfg.snapshot.clone(), cfg.hold)
}

/// Starts the machine's vCPUs; with `hold`, they wait for [`Handle::release`].
fn launch(m: Machine, snapshots: Option<SnapshotPolicy>, hold: bool) -> Result<(Handle, Running), String> {
    let Machine {
        vm,
        memory,
        bus,
        serial,
        control,
        start,
        config,
    } = m;
    let vcpus = config.vcpus;
    let vm = Arc::new(vm);
    let bus = Arc::new(bus);
    let resume_counter = match &start {
        Start::Restore(r) => Some(r.counter),
        Start::Boot(_) => None,
    };
    let start = Arc::new(start);
    let shared = Arc::new(Shared::new(vcpus, resume_counter));

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
        match machine::finish(&vm, &bus, &start) {
            Ok(()) if !hold => shared.release_vcpus(),
            Ok(()) => {}
            Err(e) => shared.stop(ExitReason::Error(e)),
        }
    }
    info!(
        "{vcpus} vCPU(s) ready to run after {} us",
        crate::log::uptime_us()
    );
    Ok((
        Handle {
            shared,
            serial,
            control,
        },
        Running {
            vm: Some(vm),
            threads,
            _memory: memory,
        },
    ))
}

/// Creates vCPU `index` and puts it where `start` says: the boot entry, parked for
/// PSCI CPU_ON, or its restored state.
fn setup_vcpu(vm: &hv::Vm, index: usize, start: &Start) -> Result<hv::Vcpu, String> {
    let e = |e: hv::Error| e.to_string();
    let mut vcpu = vm.create_vcpu(index).map_err(e)?;
    match start {
        Start::Boot(entry) => {
            if index == 0 {
                vcpu.boot(*entry);
            }
        }
        Start::Restore(r) => {
            if index == 0 {
                let here = vcpu.cpu_id().map_err(e)?;
                if here != r.cpu_id {
                    return Err(format!(
                        "this CPU is not the one the snapshot was taken on: {here:x?} vs {:x?}",
                        r.cpu_id
                    ));
                }
            }
            let state = r
                .vcpus
                .get(index)
                .ok_or_else(|| format!("the snapshot has no vCPU {index}"))?;
            vcpu.restore_state(state).map_err(e)?;
        }
    }
    Ok(vcpu)
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
    let mut vcpu = match setup_vcpu(vm, index, start) {
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
        && let Err(e) = vcpu.set_counter_offset(offset)
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
            Ok(hv::Exit::Canceled) => {
                if let Err(e) = sh.park_for_snapshot(index, &vcpu) {
                    return sh.stop(ExitReason::Error(format!("vCPU {index} snapshot: {e}")));
                }
            }
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
    bus: Arc<MmioBus>,
    memory: Arc<GuestMemory>,
    config: MachineConfig,
    policy: SnapshotPolicy,
}

impl Coordinator {
    fn run(self) {
        loop {
            let Some(mut p) = self.wait_until_parked() else {
                return;
            };
            let captured: Option<Vec<(VcpuState, u64)>> = p.captured.iter_mut().map(Option::take).collect();
            let cpu_id = p.cpu_id.take();
            drop(p);
            let t0 = crate::log::uptime_us();
            let written = match (captured, cpu_id) {
                (Some(captured), Some(cpu_id)) => self.write(captured, cpu_id),
                _ => Err("a vCPU parked without its state".into()),
            };
            if let Err(e) = written {
                return self.sh.stop(ExitReason::Error(format!("snapshot: {e}")));
            }
            info!(
                "snapshot written to {} in {} us",
                self.policy.dir.display(),
                crate::log::uptime_us().saturating_sub(t0)
            );
            match self.policy.then {
                AfterSnapshot::Stop => return self.sh.stop(ExitReason::Snapshotted),
                AfterSnapshot::Resume => {
                    if let Err(e) = self.bus.resume() {
                        return self.sh.stop(ExitReason::Error(format!("resuming devices: {e}")));
                    }
                    let mut p = lock(&self.sh.pause);
                    p.requested = false;
                    p.epoch = p.epoch.wrapping_add(1);
                    drop(p);
                    self.sh.paused.notify_all();
                }
            }
        }
    }

    /// Waits for a request and for every vCPU to park; `None` once the VM is exiting.
    fn wait_until_parked(&self) -> Option<MutexGuard<'_, Pause>> {
        let mut p = lock(&self.sh.pause);
        loop {
            if self.sh.exiting() {
                return None;
            }
            if p.requested && p.captured.iter().all(Option::is_some) {
                return Some(p);
            }
            p = wait(&self.sh.paused, p);
        }
    }

    /// Quiesces devices, then saves interrupt controller, devices and memory.
    fn write(&self, captured: Vec<(VcpuState, u64)>, cpu_id: Vec<(u16, u64)>) -> Result<(), String> {
        self.bus.pause();
        let counter = captured.iter().map(|&(_, c)| c).max().unwrap_or(0);
        let states = captured.into_iter().map(|(s, _)| s).collect();
        let arch = machine::encode_state(&self.vm, states, counter, cpu_id)?;
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
