//! A virtual machine on Hypervisor.framework: construction, vCPU threads, exit handling.

use std::fs::File;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};

use crate::arch::aarch64::{self, boot, esr, layout, psci, sysreg};
use crate::devices::control::Control;
use crate::devices::get_le;
use crate::devices::rtc::Pl031;
use crate::devices::serial::Serial;
use crate::devices::{Interrupt, MmioBus};
use crate::hvf::{self, Exit, Gic, GicLayout, Granule, Perms, Reg, Vcpu, VcpuKicker};
use crate::memory::GuestMemory;
use crate::sync::{lock, wait};
use crate::{debug, info, initramfs, warn};

const MIB: u64 = 1 << 20;
/// Smallest guest that can hold a kernel, its early allocations and the DTB window.
const MIN_MEMORY_MIB: u64 = 64;
const SPI_INTID_BASE: u32 = 32;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Console {
    Stdout,
    Discard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    /// PSCI SYSTEM_OFF.
    PowerOff,
    /// PSCI SYSTEM_RESET (Linux issues it on reboot and, with `panic=-1`, on panic).
    Reset,
    /// Stopped by the host.
    Stopped,
    Error(String),
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

struct GicLine {
    gic: Gic,
    intid: u32,
}

impl Interrupt for GicLine {
    fn set_level(&self, level: bool) {
        if let Err(e) = self.gic.set_spi(self.intid, level) {
            warn!("interrupt {}: {e}", self.intid);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Start {
    entry: u64,
    x0: u64,
}

#[derive(Debug, Clone, Copy)]
enum Power {
    Off,
    Pending(Start),
    On,
}

#[derive(Debug)]
struct CpuSlot {
    mpidr: u64,
    power: Mutex<Power>,
    wake: Condvar,
    kicker: OnceLock<VcpuKicker>,
}

struct Shared {
    memory: Arc<GuestMemory>,
    bus: MmioBus,
    cpus: Vec<CpuSlot>,
    ipa_bits: u32,
    redist_size: u64,
    exiting: AtomicBool,
    exit: Mutex<Option<ExitReason>>,
    exited_at_us: Mutex<Option<u128>>,
    exited: Condvar,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("cpus", &self.cpus.len())
            .field("bus", &self.bus)
            .finish_non_exhaustive()
    }
}

impl Shared {
    /// Records the first exit reason and wakes every vCPU so it can wind down.
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
        for cpu in &self.cpus {
            drop(lock(&cpu.power));
            cpu.wake.notify_all();
            if let Some(k) = cpu.kicker.get() {
                // A vCPU that already exited reports an error here; nothing to do then.
                let _ = k.kick();
            }
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

    /// Parks an off vCPU until PSCI CPU_ON targets it; `None` once the VM is stopping.
    fn wait_power_on(&self, cpu: &CpuSlot) -> Option<Start> {
        let mut p = lock(&cpu.power);
        loop {
            if self.exiting() {
                return None;
            }
            if let Power::Pending(start) = *p {
                *p = Power::On;
                return Some(start);
            }
            p = wait(&cpu.wake, p);
        }
    }

    fn cpu_on(&self, target: u64, entry: u64, context: u64) -> i64 {
        if target & !psci::MPIDR_AFFINITY_MASK != 0 {
            return psci::INVALID_PARAMETERS;
        }
        let Some(cpu) = self.cpus.iter().find(|c| c.mpidr == target) else {
            return psci::INVALID_PARAMETERS;
        };
        if self.memory.host_ptr(entry, 4).is_err() {
            return psci::INVALID_ADDRESS;
        }
        let mut p = lock(&cpu.power);
        match *p {
            Power::On => psci::ALREADY_ON,
            Power::Pending(_) => psci::ON_PENDING,
            Power::Off => {
                *p = Power::Pending(Start { entry, x0: context });
                cpu.wake.notify_all();
                psci::SUCCESS
            }
        }
    }

    fn affinity_info(&self, target: u64) -> i64 {
        match self.cpus.iter().find(|c| c.mpidr == target) {
            None => psci::INVALID_PARAMETERS,
            Some(cpu) => match *lock(&cpu.power) {
                Power::Off => psci::AFF_OFF,
                Power::Pending(_) => psci::AFF_ON_PENDING,
                Power::On => psci::AFF_ON,
            },
        }
    }
}

fn architected_ipa_bits(ram_end: u64) -> Result<u32, String> {
    let needed = 64 - ram_end.saturating_sub(1).leading_zeros();
    let max = hvf::max_ipa_bits().map_err(|e| e.to_string())?;
    [36, 40, 42, 44, 48]
        .into_iter()
        .find(|&b| b >= needed && b <= max)
        .ok_or_else(|| {
            format!("guest RAM ending at {ram_end:#x} needs {needed} address bits; host allows {max}")
        })
}

/// Boots a VM and runs it on the calling thread's behalf until it exits.
pub fn run(cfg: &Config) -> Result<ExitReason, String> {
    let (handle, join) = start(cfg)?;
    Ok(join.wait(handle))
}

/// Joins the vCPU threads and tears the VM down once the guest exits.
#[derive(Debug)]
pub struct Running {
    vm: Option<hvf::Vm>,
    threads: Vec<std::thread::JoinHandle<()>>,
    _memory: Arc<GuestMemory>,
}

impl Running {
    pub fn wait(mut self, handle: Handle) -> ExitReason {
        let reason = handle.shared.wait_exit();
        handle.shared.stop(reason.clone());
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        // Every vCPU is destroyed on its own thread before this point, as HVF requires.
        self.vm.take();
        reason
    }
}

pub fn start(cfg: &Config) -> Result<(Handle, Running), String> {
    if cfg.vcpus == 0 {
        return Err("at least one vCPU is required".into());
    }
    let max_vcpus = hvf::max_vcpus().map_err(|e| e.to_string())?;
    if cfg.vcpus > max_vcpus {
        return Err(format!(
            "{} vCPUs requested; this host supports {max_vcpus}",
            cfg.vcpus
        ));
    }
    if cfg.memory_mib < MIN_MEMORY_MIB || !cfg.memory_mib.is_multiple_of(2) {
        return Err(format!(
            "guest memory must be an even number of MiB, at least {MIN_MEMORY_MIB}"
        ));
    }
    let ram = cfg.memory_mib * MIB;
    let ipa_bits = architected_ipa_bits(layout::DRAM_BASE + ram)?;

    // Declared before the VM so it outlives it: HVF must stop mapping it first.
    let memory = Arc::new(
        GuestMemory::anonymous(&[(layout::DRAM_BASE, ram as usize)])
            .map_err(|e| format!("guest RAM: {e}"))?,
    );
    let vm = hvf::Vm::new(hvf::VmConfig {
        ipa_bits: (ipa_bits > 36).then_some(ipa_bits),
        granule: Granule::K16,
    })
    .map_err(|e| e.to_string())?;
    for (gpa, host, len) in memory.regions() {
        // SAFETY: `memory` is an owned mmap region kept alive in `Running` until after
        // the VM is destroyed.
        unsafe { vm.map(host, gpa, len, Perms::RWX) }.map_err(|e| e.to_string())?;
    }

    let gp = hvf::gic_params().map_err(|e| e.to_string())?;
    let redist_total = gp.redist_size * cfg.vcpus as u64;
    if !layout::GIC_DIST.is_multiple_of(gp.dist_align)
        || !layout::GIC_REDIST.is_multiple_of(gp.redist_align)
        || layout::GIC_DIST + gp.dist_size > layout::GIC_MSI
        || layout::GIC_REDIST + redist_total > layout::GIC_REDIST_MAX_END
    {
        return Err(format!(
            "host GIC geometry {gp:?} does not fit the guest memory map"
        ));
    }
    let gic = vm
        .create_gic(&GicLayout {
            dist_base: layout::GIC_DIST,
            redist_base: layout::GIC_REDIST,
            msi: None,
        })
        .map_err(|e| e.to_string())?;

    let kernel_file = File::open(&cfg.kernel).map_err(|e| format!("{}: {e}", cfg.kernel.display()))?;
    let kernel = boot::load_kernel(&memory, &kernel_file, ram).map_err(|e| e.to_string())?;
    let fdt_addr = layout::DRAM_BASE + ram - boot::FDT_MAX;
    let read = |p: &PathBuf| std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
    let initrd_bytes = match (&cfg.initrd, &cfg.init) {
        (Some(_), Some(_)) => return Err("--initrd and --init are mutually exclusive".into()),
        (Some(p), None) => Some(read(p)?),
        (None, Some(p)) => Some(initramfs::with_init(&read(p)?)),
        (None, None) => None,
    };
    let initrd = match initrd_bytes {
        None => None,
        Some(bytes) => {
            Some(boot::load_initrd(&memory, &bytes, kernel.end, fdt_addr).map_err(|e| e.to_string())?)
        }
    };
    let mpidrs: Vec<u64> = (0..cfg.vcpus).map(aarch64::mpidr).collect();
    let mut rng_seed = [0u8; 64];
    // SAFETY: getentropy writes at most 256 bytes into the provided buffer.
    if unsafe { libc::getentropy(rng_seed.as_mut_ptr().cast(), rng_seed.len()) } != 0 {
        return Err(format!("getentropy: {}", std::io::Error::last_os_error()));
    }
    let fdt = boot::build_fdt(&boot::Machine {
        mpidrs: &mpidrs,
        ram_size: ram,
        cmdline: &cfg.cmdline,
        initrd,
        gic_dist: (layout::GIC_DIST, gp.dist_size),
        gic_redist: (layout::GIC_REDIST, redist_total),
        virtio: &[],
        rng_seed,
    })
    .map_err(|e| e.to_string())?;
    memory.write(fdt_addr, &fdt).map_err(|e| e.to_string())?;
    debug!(
        "kernel entry {:#x} end {:#x}; dtb at {fdt_addr:#x} ({} bytes)",
        kernel.entry,
        kernel.end,
        fdt.len()
    );

    let out: Box<dyn Write + Send> = match cfg.console {
        Console::Stdout => {
            // SAFETY: dup has no memory-safety preconditions.
            let fd = unsafe { libc::dup(1) };
            if fd < 0 {
                return Err(format!("dup(stdout): {}", std::io::Error::last_os_error()));
            }
            // SAFETY: `fd` is a fresh descriptor that the File owns from here on.
            Box::new(unsafe { File::from_raw_fd(fd) })
        }
        Console::Discard => Box::new(std::io::sink()),
    };
    let serial = Arc::new(Serial::new(
        out,
        Arc::new(GicLine {
            gic,
            intid: SPI_INTID_BASE + layout::SPI_UART,
        }),
    ));
    let mut bus = MmioBus::default();
    bus.insert(layout::UART, 0x1000, serial.clone())?;
    bus.insert(layout::RTC, 0x1000, Arc::new(Pl031::default()))?;
    let control = Arc::new(Control::default());
    bus.insert(layout::CONTROL, 0x1000, control.clone())?;

    let shared = Arc::new(Shared {
        memory: memory.clone(),
        bus,
        cpus: mpidrs
            .iter()
            .enumerate()
            .map(|(i, &mpidr)| CpuSlot {
                mpidr,
                power: Mutex::new(if i == 0 { Power::On } else { Power::Off }),
                wake: Condvar::new(),
                kicker: OnceLock::new(),
            })
            .collect(),
        ipa_bits,
        redist_size: gp.redist_size,
        exiting: AtomicBool::new(false),
        exit: Mutex::new(None),
        exited_at_us: Mutex::new(None),
        exited: Condvar::new(),
    });

    // vCPUs are created strictly in index order: HVF assigns redistributor frames by
    // creation order (platform-measurements.md M13).
    let mut threads = Vec::with_capacity(cfg.vcpus as usize);
    for i in 0..cfg.vcpus as usize {
        let (created_tx, created_rx) = mpsc::channel();
        let sh = shared.clone();
        let boot_start = (i == 0).then_some(Start {
            entry: kernel.entry,
            x0: fdt_addr,
        });
        let spawned = std::thread::Builder::new()
            .name(format!("vcpu{i}"))
            .spawn(move || vcpu_thread(sh, i, boot_start, created_tx));
        match spawned {
            Ok(t) => threads.push(t),
            Err(e) => {
                shared.stop(ExitReason::Error(format!("spawning vCPU {i}: {e}")));
                break;
            }
        }
        match created_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                shared.stop(ExitReason::Error(format!("vCPU {i}: {e}")));
                break;
            }
            Err(_) => {
                shared.stop(ExitReason::Error(format!("vCPU {i} thread died during setup")));
                break;
            }
        }
    }
    info!(
        "started {} vCPU(s), {} MiB, IPA {} bits",
        cfg.vcpus, cfg.memory_mib, ipa_bits
    );
    info!("VM ready to run after {} us", crate::log::uptime_us());
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
    sh: Arc<Shared>,
    index: usize,
    boot_start: Option<Start>,
    created: mpsc::Sender<Result<(), String>>,
) {
    if let Err(e) = crate::thread::make_current_realtime()
        && index == 0
    {
        warn!("vCPU threads run without real-time policy (coarser guest timers): {e}");
    }
    let setup = sh
        .cpus
        .get(index)
        .ok_or_else(|| format!("no CPU slot for vCPU {index}"))
        .and_then(|slot| init_vcpu(&sh, slot, index).map(|vcpu| (slot, vcpu)));
    let (slot, mut vcpu) = match setup {
        Ok(v) => v,
        Err(e) => {
            let _ = created.send(Err(e));
            return;
        }
    };
    let _ = created.send(Ok(()));
    drop(created);

    let mut next = boot_start;
    loop {
        let Some(start) = next.take().or_else(|| sh.wait_power_on(slot)) else {
            return;
        };
        if let Err(e) = enter_at(&mut vcpu, start) {
            sh.stop(ExitReason::Error(format!("vCPU {index}: {e}")));
            return;
        }
        match run_vcpu(&mut vcpu, &sh, index) {
            Stop::CpuOff => *lock(&slot.power) = Power::Off,
            Stop::Vm => return,
        }
    }
}

fn init_vcpu(sh: &Shared, slot: &CpuSlot, index: usize) -> Result<Vcpu, String> {
    let mut vcpu = Vcpu::new().map_err(|e| e.to_string())?;
    let e = |e: hvf::Error| e.to_string();
    vcpu.set_sys_reg(sysreg::MPIDR_EL1, slot.mpidr).map_err(e)?;

    // The redistributor must be the index-th frame, or GIC routing and the DT disagree.
    let expected = (index as u64)
        .checked_mul(sh.redist_size)
        .and_then(|off| off.checked_add(layout::GIC_REDIST))
        .ok_or("redistributor offset overflows")?;
    let actual = vcpu.redistributor_base().map_err(e)?;
    if actual != expected {
        return Err(format!(
            "redistributor at {actual:#x}, expected {expected:#x} (vCPU creation order)"
        ));
    }

    // Advertise the physical address width the stage-2 actually covers.
    let parange = aarch64::parange_for_bits(sh.ipa_bits)
        .ok_or_else(|| format!("no PARange encoding for {} IPA bits", sh.ipa_bits))?;
    let mmfr0 = vcpu.sys_reg(sysreg::ID_AA64MMFR0_EL1).map_err(e)?;
    let want = (mmfr0 & !0xf) | parange;
    if want != mmfr0 {
        vcpu.set_sys_reg(sysreg::ID_AA64MMFR0_EL1, want).map_err(e)?;
        let got = vcpu.sys_reg(sysreg::ID_AA64MMFR0_EL1).map_err(e)?;
        if got != want && index == 0 {
            warn!(
                "ID_AA64MMFR0_EL1.PARange stays {:#x}; guest may probe beyond {} IPA bits",
                got & 0xf,
                sh.ipa_bits
            );
        }
    }
    slot.kicker
        .set(vcpu.kicker())
        .map_err(|_| format!("vCPU {index} registered twice"))?;
    Ok(vcpu)
}

/// Architectural entry state for the boot CPU and for PSCI CPU_ON.
fn enter_at(vcpu: &mut Vcpu, start: Start) -> Result<(), hvf::Error> {
    vcpu.set_reg(Reg::CPSR, sysreg::PSTATE_EL1H_DAIF)?;
    vcpu.set_sys_reg(sysreg::SCTLR_EL1, sysreg::SCTLR_EL1_RESET)?;
    vcpu.set_x(0, start.x0)?;
    for n in 1..=30 {
        vcpu.set_x(n, 0)?;
    }
    vcpu.set_reg(Reg::PC, start.entry)
}

enum Stop {
    CpuOff,
    Vm,
}

fn run_vcpu(vcpu: &mut Vcpu, sh: &Shared, index: usize) -> Stop {
    let fail = |msg: String| {
        sh.stop(ExitReason::Error(format!("vCPU {index}: {msg}")));
        Stop::Vm
    };
    loop {
        if sh.exiting() {
            return Stop::Vm;
        }
        let exit = match vcpu.run() {
            Ok(exit) => exit,
            Err(e) => return fail(e.to_string()),
        };
        let (esr, ipa) = match exit {
            Exit::Canceled => continue,
            Exit::Exception { syndrome, ipa, .. } => (syndrome, ipa),
            Exit::VtimerActivated => {
                debug!("vCPU {index}: unexpected VTIMER_ACTIVATED with the in-kernel GIC");
                continue;
            }
            Exit::Unknown(r) => return fail(format!("unknown exit reason {r}")),
        };
        let handled = match esr::ec(esr) {
            esr::EC_DABT_LOW => mmio(vcpu, sh, esr, ipa),
            esr::EC_HVC64 | esr::EC_SMC64 => {
                // HVC exits with PC already past the instruction; a trapped SMC does not.
                if esr::ec(esr) == esr::EC_SMC64
                    && let Err(e) = advance_pc(vcpu, esr)
                {
                    return fail(e);
                }
                match firmware_call(vcpu, sh) {
                    Ok(None) => Ok(()),
                    Ok(Some(stop)) => return stop,
                    Err(e) => Err(e),
                }
            }
            esr::EC_SYS64 => sysreg_trap(vcpu, esr),
            esr::EC_WFX => advance_pc(vcpu, esr),
            ec => Err(describe_fault(vcpu, ec, esr, ipa)),
        };
        if let Err(e) = handled {
            return fail(e);
        }
    }
}

fn advance_pc(vcpu: &mut Vcpu, esr: u64) -> Result<(), String> {
    let pc = vcpu.reg(Reg::PC).map_err(|e| e.to_string())?;
    vcpu.set_reg(Reg::PC, pc.wrapping_add(esr::instr_len(esr)))
        .map_err(|e| e.to_string())
}

fn mmio(vcpu: &mut Vcpu, sh: &Shared, esr: u64, ipa: u64) -> Result<(), String> {
    let Some(da) = esr::data_abort(esr) else {
        return Err(describe_fault(vcpu, esr::EC_DABT_LOW, esr, ipa));
    };
    let e = |e: hvf::Error| e.to_string();
    let bad_size = || format!("MMIO access of {} bytes at {ipa:#x}", da.size);
    if da.write {
        let value = vcpu.x(da.reg).map_err(e)?;
        let bytes = value.to_le_bytes();
        let data = bytes.get(..da.size).ok_or_else(bad_size)?;
        if !sh.bus.write(ipa, data) {
            debug!("unclaimed MMIO write {ipa:#x} <- {value:#x} ({} bytes)", da.size);
        }
    } else {
        let mut raw = [0u8; 8];
        let data = raw.get_mut(..da.size).ok_or_else(bad_size)?;
        if !sh.bus.read(ipa, data) {
            debug!("unclaimed MMIO read {ipa:#x} ({} bytes)", da.size);
        }
        vcpu.set_x(da.reg, da.load_value(get_le(data))).map_err(e)?;
    }
    advance_pc(vcpu, esr)
}

/// Handles PSCI/SMCCC. Returns `Some` when the vCPU must stop running.
fn firmware_call(vcpu: &mut Vcpu, sh: &Shared) -> Result<Option<Stop>, String> {
    let e = |e: hvf::Error| e.to_string();
    let x = [
        vcpu.x(0).map_err(e)?,
        vcpu.x(1).map_err(e)?,
        vcpu.x(2).map_err(e)?,
        vcpu.x(3).map_err(e)?,
    ];
    let ret = match psci::decode(x) {
        psci::Call::Immediate(v) => v,
        psci::Call::CpuSuspend => psci::SUCCESS,
        psci::Call::CpuOff => return Ok(Some(Stop::CpuOff)),
        psci::Call::CpuOn {
            target,
            entry,
            context,
        } => sh.cpu_on(target, entry, context),
        psci::Call::AffinityInfo { target } => sh.affinity_info(target),
        psci::Call::SystemOff => {
            sh.stop(ExitReason::PowerOff);
            return Ok(Some(Stop::Vm));
        }
        psci::Call::SystemReset => {
            sh.stop(ExitReason::Reset);
            return Ok(Some(Stop::Vm));
        }
    };
    vcpu.set_x(0, ret as u64).map_err(e)?;
    Ok(None)
}

/// Trapped system-register accesses read as zero and ignore writes. HVF traps only a
/// few debug/OS-lock registers here (e.g. OSLAR_EL1, MDCCINT_EL1 on macOS 26).
fn sysreg_trap(vcpu: &mut Vcpu, esr: u64) -> Result<(), String> {
    let a = esr::sysreg_access(esr);
    debug!(
        "trapped {} of sysreg {:#06x}",
        if a.read { "read" } else { "write" },
        a.encoding
    );
    if a.read {
        vcpu.set_x(a.reg, 0).map_err(|e| e.to_string())?;
    }
    advance_pc(vcpu, esr)
}

fn describe_fault(vcpu: &Vcpu, ec: u32, esr: u64, ipa: u64) -> String {
    let pc = vcpu.reg(Reg::PC).unwrap_or(0);
    format!("unhandled guest exception: EC {ec:#04x}, ESR {esr:#x}, IPA {ipa:#x}, PC {pc:#x}")
}
