//! vCPU threads and the VM lifecycle, on any backend and architecture: boot or restore,
//! run, snapshot, stop.

use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, mpsc};
use std::thread::JoinHandle;

use super::machine::{self, Machine, Start};
use super::{AfterSnapshot, Config, ExitReason, RestoreConfig, SnapshotPolicy};
use crate::devices::control::Control;
use crate::devices::power::PowerEvent;
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
        warn!(
            "kvm-stats recorded {} pages, {} written",
            pages.len(),
            pages.iter().filter(|t| t.written).count()
        );
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

/// A snapshot in progress: vCPUs park here with their state captured.
#[derive(Debug, Default)]
struct Pause {
    requested: bool,
    /// Per vCPU, while parked: its state and its guest counter when it stopped.
    captured: Vec<Option<machine::Captured>>,
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
            pause: Mutex::new(Pause {
                captured: (0..vcpus).map(|_| None).collect(),
                ..Pause::default()
            }),
            paused: Condvar::new(),
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
        r.done = true;
        r.counter_offset = machine::release_offset(&self.start);
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
        let captured = machine::capture(vcpu, index)?;
        let mut p = lock(&self.pause);
        if let Some(slot) = p.captured.get_mut(index) {
            *slot = Some(captured);
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
    /// Devices own memory the VM maps (virtio-pmem regions): they go after the VM, as
    /// guest RAM does.
    _bus: Arc<machine::Bus>,
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
        // Diagnostic (branch kvm-ws-diag): the RAM pages this VM mapped, and wrote.
        #[cfg(target_os = "linux")]
        if std::env::var_os("SHARDS_KVM_STATS").is_some() {
            let (mut mapped, mut written) = (0, 0);
            for (_, host, len) in self._memory.regions() {
                if let Ok(pages) = crate::platform::mapped_pages(host, len) {
                    mapped += pages.len();
                    written += pages.iter().filter(|(_, w)| *w).count();
                }
            }
            warn!("kvm-stats ram mapped={mapped} written={written}");
        }
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
    let working_set = if cfg.prefetch {
        snapshot::read_working_set(&cfg.dir, machine::PAGE).unwrap_or_else(|e| {
            warn!("{e}; restoring without prefetching it");
            None
        })
    } else {
        None
    };
    let machine = machine::restore(
        &snap,
        &memory_file,
        cfg.console,
        cfg.vsock.as_deref(),
        working_set.unwrap_or_default(),
    )?;
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
    Ok((
        Handle {
            shared,
            serial,
            control,
        },
        Running {
            vm: Some(vm),
            threads,
            _bus: bus,
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
    bus: Arc<machine::Bus>,
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
            let captured: Option<Vec<machine::Captured>> = p.captured.iter_mut().map(Option::take).collect();
            drop(p);
            let t0 = crate::log::uptime_us();
            let written = match captured {
                Some(captured) => self.write(captured),
                None => Err("a vCPU parked without its state".into()),
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
                    if self.policy.working_set {
                        self.record();
                    }
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

    /// Starts recording the working set from the snapshot just written, with every vCPU
    /// parked, unless one is being recorded already. Without one, restores just run.
    fn record(&self) {
        let mut recording = lock(&self.sh.recording);
        if recording.is_some() {
            return;
        }
        let started = File::open(&self.policy.dir)
            .map_err(|e| format!("{}: {e}", self.policy.dir.display()))
            .and_then(|dir| {
                let recorder = machine::record(&self.vm, &self.memory, &self.bus, &self.policy.dir)?;
                Ok(recorder.map(|r| (r, dir)))
            });
        match started {
            Ok(r) => *recording = r,
            Err(e) => warn!("not recording a working set: {e}"),
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
    fn write(&self, captured: Vec<machine::Captured>) -> Result<(), String> {
        self.bus.pause();
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
