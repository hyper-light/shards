//! vCPU threads and the VM lifecycle, on any backend and architecture.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::thread::JoinHandle;

use super::machine::{self, Machine};
use super::{Config, ExitReason};
use crate::devices::control::Control;
use crate::devices::serial::Serial;
use crate::hv;
use crate::memory::GuestMemory;
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
    /// Guest boot markers as `(marker, µs since VMM start)`.
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

    /// Feeds bytes to the guest console as if typed.
    pub fn console_input(&self, bytes: &[u8]) {
        self.serial.enqueue_input(bytes);
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
}

impl Shared {
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
        for kicker in self.kickers.iter().filter_map(OnceLock::get) {
            kicker.kick();
        }
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

pub fn start(cfg: &Config) -> Result<(Handle, Running), String> {
    check_host()?;
    if cfg.vcpus == 0 {
        return Err("at least one vCPU is required".into());
    }
    let max = max_vcpus()?;
    if cfg.vcpus > max {
        return Err(format!("{} vCPUs requested; this host supports {max}", cfg.vcpus));
    }
    let Machine {
        vm,
        memory,
        bus,
        serial,
        control,
        boot,
    } = machine::build(cfg)?;
    let vm = Arc::new(vm);
    let bus = Arc::new(bus);
    let shared = Arc::new(Shared {
        kickers: (0..cfg.vcpus).map(|_| OnceLock::new()).collect(),
        exiting: AtomicBool::new(false),
        exit: Mutex::new(None),
        exited_at_us: Mutex::new(None),
        exited: Condvar::new(),
        entered_at_us: OnceLock::new(),
    });

    // vCPUs are created strictly in index order (D5).
    let mut threads = Vec::with_capacity(cfg.vcpus as usize);
    for index in 0..cfg.vcpus as usize {
        let (created_tx, created_rx) = mpsc::channel();
        let (vm, sh, bus) = (vm.clone(), shared.clone(), bus.clone());
        let boot = (index == 0).then_some(boot);
        let spawned = std::thread::Builder::new()
            .name(format!("vcpu{index}"))
            .spawn(move || vcpu_thread(&vm, &sh, &*bus, index, boot, &created_tx));
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
    info!(
        "started {} vCPU(s), {} MiB; ready to run after {} us",
        cfg.vcpus,
        cfg.memory_mib,
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

fn vcpu_thread(
    vm: &hv::Vm,
    sh: &Shared,
    io: &dyn hv::Io,
    index: usize,
    boot: Option<machine::Boot>,
    created: &mpsc::Sender<Result<(), String>>,
) {
    if let Err(e) = platform::prioritize_vcpu_thread()
        && index == 0
    {
        warn!("vCPU threads run without real-time policy (coarser guest timers): {e}");
    }
    let mut vcpu = match vm.create_vcpu(index) {
        Ok(v) => v,
        Err(e) => {
            let _ = created.send(Err(e.to_string()));
            return;
        }
    };
    if let Some(slot) = sh.kickers.get(index) {
        let _ = slot.set(vcpu.kicker());
    }
    if let Some(entry) = boot {
        vcpu.boot(entry);
    }
    let _ = created.send(Ok(()));
    if index == 0 {
        let _ = sh.entered_at_us.set(crate::log::uptime_us());
    }
    loop {
        if sh.exiting() {
            return;
        }
        match vcpu.run(io) {
            Ok(hv::Exit::Canceled) => {}
            Ok(hv::Exit::Shutdown) => return sh.stop(ExitReason::PowerOff),
            Ok(hv::Exit::Reset) => return sh.stop(ExitReason::Reset),
            Err(e) => return sh.stop(ExitReason::Error(format!("vCPU {index}: {e}"))),
        }
    }
}
