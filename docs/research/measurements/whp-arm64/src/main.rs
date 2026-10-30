//! A WHP partition run on an arm64 Windows host (PM M414), before shards' arm64 WHP backend
//! is written: what the platform offers there (its capabilities, and whether its arm64 API
//! is the released one), whether a partition of two vCPUs runs under the platform's GICv3 at
//! shards' places (arch/aarch64 layout: QEMU virt's), and, from a guest of its own
//! (`m414_guest`, below): an MMIO write and read as a VMM sees them (their syndrome, and what
//! an exit costs, timed from the host, n of them); an SPI asked for of the GIC and taken by the
//! guest; PSCI as the guest's HVC reaches the hypervisor (PSCI_VERSION, CPU_ON for vCPU 1,
//! SYSTEM_OFF), answered by the hypervisor or handed to the VMM; and what a snapshot's GIC and
//! timer rest on (each vCPU's interrupt controller state and the partition's global one, read
//! and set back, and the virtual counter read and set). Each line says what was found.

#[cfg(all(windows, target_arch = "aarch64"))]
mod ffi;

#[cfg(not(all(windows, target_arch = "aarch64")))]
fn main() {
    println!("whp-arm64 runs on arm64 Windows");
}

#[cfg(all(windows, target_arch = "aarch64"))]
fn main() {
    match probe::run() {
        Ok(()) => {}
        Err(e) => {
            println!("FAILED: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(all(windows, target_arch = "aarch64"))]
mod probe {
    use std::ffi::c_void;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
    use windows_sys::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
    };

    use crate::ffi::*;

    // The guest's code: shards' arm64 layout (crates/vmm/src/arch/aarch64/mod.rs).
    const GICD: u64 = 0x0800_0000;
    const GITS: u64 = 0x0808_0000;
    const GICR: u64 = 0x080a_0000;
    const GICR_STRIDE: u64 = 0x2_0000;
    const MMIO: u64 = 0x0a00_0000;
    const RAM: u64 = 0x8000_0000;
    const RAM_SIZE: usize = 2 << 20;
    /// INTIDs of the virtual timer and the PMU, as shards' devicetree gives them.
    const PPI_VTIMER: u32 = 27;
    const PPI_PMU: u32 = 23;
    /// The SPI the guest is sent: SPI 1, INTID 33.
    const SPI: u32 = 33;
    const WATCHDOG: Duration = Duration::from_secs(30);
    /// SCTLR_EL1 at reset with the MMU and caches off (its RES1 bits), and PSTATE at EL1h
    /// with DAIF masked: shards' boot values (arch/aarch64 reg).
    const SCTLR_RESET: u64 = 0x30d0_0800;
    const PSTATE_EL1H: u64 = 0x3c5;

    std::arch::global_asm!(
        r#"
        .section .text.m414,"xr"
        .p2align 12
        .globl m414_guest
    m414_guest:
        // vCPU 0, at EL1h with the MMU off, from the start of RAM.
        movz x20, #0x0a00, lsl #16          // the probe's MMIO page, which nothing maps
        movz x0, #0x1234
        str x0, [x20]                       // 0x00: a write, its value 0x1234
        ldr x2, [x20, #0x08]                // 0x08: a read, answered 0x5678
        str x2, [x20, #0x10]                // 0x10: the read's value
        movz x3, #1000
    1:  str x3, [x20, #0x18]                // 0x18: the writes timed
        subs x3, x3, #1
        b.ne 1b
        // The GIC at shards' places: its distributor, and vCPU 0's redistributor awake.
        movz x21, #0x0800, lsl #16
        movz x22, #0x080a, lsl #16
        ldr w12, [x22, #0x14]               // GICR_WAKER
        and w12, w12, #0xfffffffd           // ProcessorSleep off
        str w12, [x22, #0x14]
    2:  ldr w12, [x22, #0x14]
        tbnz w12, #2, 2b                    // until ChildrenAsleep clears
        movz w12, #0x13
        str w12, [x21]                      // GICD_CTLR: ARE, Group 1 and 0 enabled
        movz w12, #2
        str w12, [x21, #0x84]               // GICD_IGROUPR1: INTID 33 in group 1
        movz w12, #0x80
        strb w12, [x21, #0x421]             // GICD_IPRIORITYR33
        str xzr, [x21, #0x6108]             // GICD_IROUTER33: to vCPU 0 (affinity 0)
        movz w12, #2
        str w12, [x21, #0x104]              // GICD_ISENABLER1: INTID 33 enabled
        ldr w12, [x21, #0x4]                // GICD_TYPER: the INTIDs it has, and more
        str x12, [x20, #0x70]               // 0x70: GICD_TYPER
        ldr x13, [x22, #0x8]                // GICR_TYPER of vCPU 0's redistributor
        str x13, [x20, #0x78]               // 0x78: GICR_TYPER
        mrs x12, S3_0_C12_C12_5             // ICC_SRE_EL1
        orr x12, x12, #1
        msr S3_0_C12_C12_5, x12
        isb
        movz x12, #0xff
        msr S3_0_C4_C6_0, x12               // ICC_PMR_EL1: every priority
        movz x12, #1
        msr S3_0_C12_C12_7, x12             // ICC_IGRPEN1_EL1
        isb
        adr x12, m414_vectors
        msr vbar_el1, x12
        isb
        msr daifclr, #2                     // IRQs taken
        str xzr, [x20, #0x50]               // 0x50: ready for INTID 33
    3:  ldr x4, m414_irq_seen
        cbnz x4, 4f
        wfi
        b 3b
    4:  msr daifset, #2
        // PSCI, as SMCCC's HVC conduit.
        movz x0, #0x8400, lsl #16           // PSCI_VERSION
        hvc #0
        str x0, [x20, #0x28]                // 0x28: its answer
        movz x0, #0x0003
        movk x0, #0xc400, lsl #16           // CPU_ON, 64-bit
        movz x1, #1                         // vCPU 1: MPIDR affinity 0.0.0.1
        adr x2, m414_secondary
        movz x3, #0x42                      // its context ID
        hvc #0
        str x0, [x20, #0x20]                // 0x20: CPU_ON's answer
    5:  ldr x4, m414_secondary_word
        cbz x4, 5b
        str x4, [x20, #0x30]                // 0x30: vCPU 1's word, read by vCPU 0
        movz x0, #0x0008
        movk x0, #0x8400, lsl #16           // SYSTEM_OFF
        hvc #0
        str x0, [x20, #0x58]                // 0x58: SYSTEM_OFF returned, as it must not
    6:  b 6b

    m414_secondary:
        movz x20, #0x0a00, lsl #16
        str x0, [x20, #0x38]                // 0x38: vCPU 1's X0, its context ID
        mrs x5, mpidr_el1
        str x5, [x20, #0x40]                // 0x40: its MPIDR
        movz x4, #1
        adr x6, m414_secondary_word
        str x4, [x6]
    7:  wfi
        b 7b

        .p2align 3
    m414_irq_seen:
        .quad 0
    m414_secondary_word:
        .quad 0

        .p2align 11
    m414_vectors:
        .rept 4
        b m414_unexpected
        .p2align 7
        .endr
        b m414_unexpected                   // current EL with SPx: synchronous
        .p2align 7
        b m414_irq                          // IRQ
        .p2align 7
        b m414_unexpected                   // FIQ
        .p2align 7
        b m414_unexpected                   // SError
        .p2align 7
        .rept 8
        b m414_unexpected
        .p2align 7
        .endr
    m414_irq:
        mrs x9, S3_0_C12_C12_0              // ICC_IAR1_EL1
        str x9, [x20, #0x48]                // 0x48: the INTID taken
        msr S3_0_C12_C12_1, x9              // ICC_EOIR1_EL1
        adr x8, m414_irq_seen
        movz x7, #1
        str x7, [x8]
        eret
    m414_unexpected:
        mrs x9, esr_el1
        str x9, [x20, #0x60]                // 0x60: an exception not expected, its ESR
        mrs x9, elr_el1
        str x9, [x20, #0x68]                // 0x68: and its ELR
    8:  b 8b
        .globl m414_guest_end
    m414_guest_end:
        .text
        "#
    );

    unsafe extern "C" {
        static m414_guest: u8;
        static m414_guest_end: u8;
    }

    type Result<T> = std::result::Result<T, String>;

    fn hr(op: &str, hr: Hresult) -> Result<()> {
        if hr < 0 {
            Err(format!("{op}: HRESULT {:#010x}", hr as u32))
        } else {
            Ok(())
        }
    }

    fn registry(key: &str, value: &str, dword: bool) -> String {
        let key: Vec<u16> = key.encode_utf16().chain([0]).collect();
        let name: Vec<u16> = value.encode_utf16().chain([0]).collect();
        let mut buf = [0u16; 256];
        let mut size = size_of_val(&buf) as u32;
        let flags = if dword { RRF_RT_REG_DWORD } else { RRF_RT_REG_SZ };
        // SAFETY: NUL-terminated names and a buffer of `size` bytes.
        let r = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                name.as_ptr(),
                flags,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &raw mut size,
            )
        };
        if r != 0 {
            return format!("(error {r})");
        }
        if dword {
            let v = u32::from(buf[0]) | (u32::from(buf[1]) << 16);
            return v.to_string();
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..n])
    }

    fn capability(code: u32) -> Result<[u64; 8]> {
        let mut buf = [0u64; 8];
        let mut written = 0u32;
        // SAFETY: a buffer of the union's size, which the call fills.
        hr("WHvGetCapability", unsafe {
            WHvGetCapability(
                code,
                buf.as_mut_ptr().cast(),
                size_of_val(&buf) as u32,
                &raw mut written,
            )
        })?;
        Ok(buf)
    }

    struct Vm {
        partition: Partition,
    }

    // SAFETY: a partition handle is the platform's, callable from any thread.
    unsafe impl Sync for Vm {}

    impl Drop for Vm {
        fn drop(&mut self) {
            // SAFETY: the partition this value made.
            unsafe { WHvDeletePartition(self.partition) };
        }
    }

    impl Vm {
        fn set<T>(&self, code: u32, value: &T) -> Result<()> {
            // SAFETY: a property of its own size.
            hr("WHvSetPartitionProperty", unsafe {
                WHvSetPartitionProperty(
                    self.partition,
                    code,
                    (value as *const T).cast(),
                    size_of::<T>() as u32,
                )
            })
        }

        fn get(&self, vp: u32, names: &[u32]) -> Result<Vec<u64>> {
            let mut values = vec![Value::default(); names.len()];
            // SAFETY: as many names as values.
            hr("WHvGetVirtualProcessorRegisters", unsafe {
                WHvGetVirtualProcessorRegisters(
                    self.partition,
                    vp,
                    names.as_ptr(),
                    names.len() as u32,
                    values.as_mut_ptr(),
                )
            })?;
            Ok(values.iter().map(|v| v.0[0]).collect())
        }

        fn put(&self, vp: u32, names: &[u32], values: &[u64]) -> Result<()> {
            let values: Vec<Value> = values.iter().map(|&v| Value([v, 0])).collect();
            // SAFETY: as many names as values.
            hr("WHvSetVirtualProcessorRegisters", unsafe {
                WHvSetVirtualProcessorRegisters(
                    self.partition,
                    vp,
                    names.as_ptr(),
                    names.len() as u32,
                    values.as_ptr(),
                )
            })
        }

        fn cancel(&self, vp: u32) {
            // SAFETY: the partition's vCPU.
            unsafe { WHvCancelRunVirtualProcessor(self.partition, vp, 0) };
        }

        fn interrupt(&self, intid: u32, asserted: bool) -> Result<()> {
            let control = InterruptControl {
                control: u64::from(INTERRUPT_TYPE_FIXED) | if asserted { INTERRUPT_ASSERTED } else { 0 },
                requested_vector: intid,
                ..InterruptControl::default()
            };
            // SAFETY: a control of its own size.
            hr("WHvRequestInterrupt", unsafe {
                WHvRequestInterrupt(
                    self.partition,
                    &raw const control,
                    size_of::<InterruptControl>() as u32,
                )
            })
        }

        /// A state of `kind` of vCPU `vp` (or the partition's, for WHV_ANY_VP), into a page,
        /// and set back as read: how many bytes it took, and what setting it back said.
        fn state(&self, vp: u32, kind: u32) -> String {
            // SAFETY: a page this process owns, committed.
            let page = unsafe {
                VirtualAlloc(
                    std::ptr::null(),
                    1 << 16,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_READWRITE,
                )
            };
            if page.is_null() {
                return "no page".into();
            }
            let mut written = 0u32;
            // SAFETY: 64 KiB of committed memory.
            let got = unsafe {
                WHvGetVirtualProcessorState(self.partition, vp, kind, page, 1 << 16, &raw mut written)
            };
            if got < 0 {
                return format!("get {:#010x}", got as u32);
            }
            // SAFETY: as read.
            let set = unsafe { WHvSetVirtualProcessorState(self.partition, vp, kind, page, written) };
            format!("{written} bytes, set back {:#010x}", set as u32)
        }
    }

    fn percentile(sorted: &[Duration], p: f64) -> Duration {
        if sorted.is_empty() {
            return Duration::ZERO;
        }
        let i = ((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len()) - 1;
        sorted[i]
    }

    /// What the guest's MMIO said, by offset in its page.
    #[derive(Default)]
    struct Said {
        writes: Vec<(u32, u32, u64)>,
        exits: Vec<Instant>,
        syndromes: Vec<String>,
        psci_by_vmm: Vec<String>,
        done: bool,
        reset: Option<u32>,
        secondary_started: bool,
    }

    pub fn run() -> Result<()> {
        println!("== host");
        let nt = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion";
        println!(
            "{} {} build {}.{}",
            registry(nt, "ProductName", false),
            registry(nt, "DisplayVersion", false),
            registry(nt, "CurrentBuild", false),
            registry(nt, "UBR", true)
        );
        println!(
            "{}",
            registry(
                "HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0",
                "ProcessorNameString",
                false
            )
        );

        println!("== capabilities");
        if !loaded() {
            return Err(
                "WinHvPlatform.dll is not here: the optional feature HypervisorPlatform is off".into(),
            );
        }
        let present = capability(CAP_HYPERVISOR_PRESENT)?[0] & 1;
        println!("HypervisorPresent {present}");
        let features = capability(CAP_FEATURES)?[0];
        println!(
            "Features {features:#x}: Arm64Support {}",
            u64::from(features & FEATURE_ARM64_SUPPORT != 0)
        );
        for (name, code) in [
            ("ExtendedVmExits", CAP_EXTENDED_VM_EXITS),
            ("ProcessorVendor", CAP_PROCESSOR_VENDOR),
            ("ProcessorFeatures", CAP_PROCESSOR_FEATURES),
            ("ProcessorClockFrequency", CAP_PROCESSOR_CLOCK_FREQUENCY),
            (
                "SyntheticProcessorFeaturesBanks",
                CAP_SYNTHETIC_PROCESSOR_FEATURES_BANKS,
            ),
            ("PhysicalAddressWidth", CAP_PHYSICAL_ADDRESS_WIDTH),
            ("GicLpiIntIdBits", CAP_GIC_LPI_INT_ID_BITS),
            ("MaxSveVectorLength", CAP_MAX_SVE_VECTOR_LENGTH),
        ] {
            match capability(code) {
                Ok(v) => println!("{name} {:#x} {:#x}", v[0], v[1]),
                Err(e) => println!("{name}: {e}"),
            }
        }
        if present == 0 {
            return Err("no hypervisor runs on this host".into());
        }

        println!("== partition");
        let mut partition: Partition = std::ptr::null_mut();
        // SAFETY: a handle the call fills.
        hr("WHvCreatePartition", unsafe {
            WHvCreatePartition(&raw mut partition)
        })?;
        let vm = Vm { partition };
        vm.set(PROP_PROCESSOR_COUNT, &2u32)?;
        let ic = IcParameters {
            emulation_mode: IC_EMULATION_GICV3,
            gic_v3: GicV3Parameters {
                gicd_base: GICD,
                gits_translater_base: GITS,
                gic_lpi_int_id_bits: 0,
                ppi_cntv_overflow: PPI_VTIMER,
                ppi_performance_monitors: PPI_PMU,
                ..GicV3Parameters::default()
            },
            ..IcParameters::default()
        };
        vm.set(PROP_ARM64_IC_PARAMETERS, &ic)?;
        // SAFETY: the partition, its properties set.
        hr("WHvSetupPartition", unsafe { WHvSetupPartition(vm.partition) })?;
        println!("set up: 2 vCPUs, GICv3 at {GICD:#x}, ITS at {GITS:#x}");

        // SAFETY: memory this process owns for the partition's life, committed.
        let ram = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                RAM_SIZE,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if ram.is_null() {
            return Err("VirtualAlloc of the guest's RAM failed".into());
        }
        // SAFETY: the guest's code, between its two symbols in this binary.
        let code = unsafe {
            let start = &raw const m414_guest;
            let len = (&raw const m414_guest_end as usize).saturating_sub(start as usize);
            std::slice::from_raw_parts(start, len)
        };
        if code.len() > RAM_SIZE {
            return Err("the guest's code is larger than its RAM".into());
        }
        // SAFETY: `code.len()` bytes into the RAM just committed.
        unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), ram.cast::<u8>(), code.len()) };
        // SAFETY: the RAM stays mapped for the partition's life.
        hr("WHvMapGpaRange", unsafe {
            WHvMapGpaRange(
                vm.partition,
                ram.cast_const(),
                RAM,
                RAM_SIZE as u64,
                MAP_READ | MAP_WRITE | MAP_EXECUTE,
            )
        })?;
        println!(
            "RAM: {} KiB at {RAM:#x}, the guest's code {} bytes",
            RAM_SIZE >> 10,
            code.len()
        );

        for vp in 0..2u32 {
            // SAFETY: the partition, set up.
            hr("WHvCreateVirtualProcessor", unsafe {
                WHvCreateVirtualProcessor(vm.partition, vp, 0)
            })?;
            vm.put(vp, &[GICR_BASE_GPA], &[GICR + GICR_STRIDE * u64::from(vp)])?;
        }
        for vp in 0..2u32 {
            let ids = vm.get(
                vp,
                &[
                    MIDR_EL1,
                    MPIDR_EL1,
                    SCTLR_EL1,
                    PSTATE,
                    CNTVCT_EL0,
                    INTERNAL_ACTIVITY_STATE,
                ],
            )?;
            println!(
                "vCPU {vp} as made: MIDR {:#x} MPIDR {:#x} SCTLR {:#x} PSTATE {:#x} CNTVCT {:#x} activity {:#x}",
                ids[0], ids[1], ids[2], ids[3], ids[4], ids[5]
            );
        }
        let global = vm.get(
            ANY_VP,
            &[
                ID_AA64PFR0_EL1,
                ID_AA64PFR1_EL1,
                ID_AA64ISAR0_EL1,
                ID_AA64MMFR0_EL1,
            ],
        );
        match global {
            Ok(v) => println!(
                "ID_AA64PFR0 {:#x} PFR1 {:#x} ISAR0 {:#x} MMFR0 {:#x}",
                v[0], v[1], v[2], v[3]
            ),
            Err(e) => println!("ID registers: {e}"),
        }
        vm.put(0, &[PC, PSTATE, SCTLR_EL1], &[RAM, PSTATE_EL1H, SCTLR_RESET])?;
        // vCPU 1 held until a CPU_ON starts it, as QEMU holds a vCPU powered off
        // (WHV_INTERNAL_ACTIVITY_REGISTER's StartupSuspend).
        vm.put(1, &[INTERNAL_ACTIVITY_STATE], &[1])?;

        println!("== run");
        let said = Mutex::new(Said::default());
        let stop = AtomicBool::new(false);
        let started = Instant::now();
        let outcome: Result<()> = std::thread::scope(|scope| {
            let secondary = scope.spawn(|| run_vcpu(&vm, 1, &said, &stop));
            let watchdog = scope.spawn(|| {
                while !stop.load(Ordering::SeqCst) && started.elapsed() < WATCHDOG {
                    std::thread::sleep(Duration::from_millis(50));
                }
                if !stop.load(Ordering::SeqCst) {
                    println!("watchdog: {WATCHDOG:?} passed, the vCPUs cancelled");
                    stop.store(true, Ordering::SeqCst);
                }
                for vp in 0..2 {
                    vm.cancel(vp);
                }
            });
            let first = run_vcpu(&vm, 0, &said, &stop);
            stop.store(true, Ordering::SeqCst);
            let second = secondary
                .join()
                .map_err(|_| "vCPU 1's thread panicked".to_string())?;
            let _ = watchdog.join();
            first.and(second)
        });
        if let Err(e) = &outcome {
            println!("run: {e}");
        }
        println!("== limits");
        limits();

        let said = said.into_inner().map_err(|_| "poisoned".to_string())?;
        println!("== found");
        for s in &said.syndromes {
            println!("{s}");
        }
        for (vp, offset, value) in said.writes.iter().filter(|w| w.1 != 0x18) {
            println!("vCPU {vp} wrote {value:#x} at {offset:#x}");
        }
        if let Some((_, _, typer)) = said.writes.iter().find(|w| w.1 == 0x70) {
            // GICD_TYPER.ITLinesNumber (bits 4:0): 32 * (N + 1) INTIDs; IDbits (23:19).
            println!(
                "GICD_TYPER {typer:#x}: INTIDs up to {}, ID bits {}",
                32 * ((typer & 0x1f) + 1) - 1,
                ((typer >> 19) & 0x1f) + 1
            );
        }
        for p in &said.psci_by_vmm {
            println!("{p}");
        }
        match said.reset {
            Some(RESET_POWER_OFF) => println!("SYSTEM_OFF: an Arm64Reset exit, PowerOff"),
            Some(RESET_REBOOT) => println!("SYSTEM_OFF: an Arm64Reset exit, Reboot"),
            Some(t) => println!("SYSTEM_OFF: an Arm64Reset exit of type {t}"),
            None => println!("SYSTEM_OFF: no Arm64Reset exit"),
        }
        println!("vCPU 1 started: {}", said.secondary_started);
        let mut gaps: Vec<Duration> = said.exits.windows(2).map(|w| w[1] - w[0]).collect();
        gaps.sort();
        if !gaps.is_empty() {
            let us = |d: Duration| d.as_secs_f64() * 1e6;
            println!(
                "an MMIO write's round trip through its exit: n={} p50={:.2} p90={:.2} p99={:.2} max={:.2} µs",
                gaps.len(),
                us(percentile(&gaps, 0.50)),
                us(percentile(&gaps, 0.90)),
                us(percentile(&gaps, 0.99)),
                us(percentile(&gaps, 1.0))
            );
        }
        outcome
    }

    /// Runs vCPU `vp` until `stop`, or its guest is done, answering its MMIO and, where the
    /// hypervisor hands them over, its PSCI calls.
    fn run_vcpu(vm: &Vm, vp: u32, said: &Mutex<Said>, stop: &AtomicBool) -> Result<()> {
        let lock = || said.lock().map_err(|_| "poisoned".to_string());
        let mut exits = 0u64;
        while !stop.load(Ordering::SeqCst) {
            let mut exit = ExitContext::default();
            // SAFETY: an exit context of its own size, 16-aligned.
            hr("WHvRunVirtualProcessor", unsafe {
                WHvRunVirtualProcessor(
                    vm.partition,
                    vp,
                    (&raw mut exit).cast::<c_void>(),
                    size_of::<ExitContext>() as u32,
                )
            })?;
            let at = Instant::now();
            exits += 1;
            match exit.exit_reason {
                EXIT_UNMAPPED_GPA | EXIT_GPA_INTERCEPT => {
                    let access = exit.memory_access();
                    let s = access.syndrome;
                    let isv = s & (1 << 24) != 0;
                    let size = 1u32 << ((s >> 22) & 3);
                    let srt = ((s >> 16) & 0x1f) as u32;
                    let write = s & (1 << 6) != 0;
                    let offset = (access.gpa.wrapping_sub(MMIO)) as u32;
                    if !isv || access.gpa < MMIO || access.gpa >= MMIO + 0x1000 {
                        return Err(format!(
                            "vCPU {vp}: an access the probe does not answer: GPA {:#x} syndrome {s:#x} PC {:#x}",
                            access.gpa, access.header.pc
                        ));
                    }
                    {
                        let mut said = lock()?;
                        if said.syndromes.len() < 3 && offset <= 0x10 {
                            said.syndromes.push(format!(
                                "vCPU {vp} {}: GPA {:#x}, syndrome {s:#x} (ISV {}, {size} bytes, X{srt}), PC {:#x}, instruction length {}, access type {}, exit {:#x}",
                                if write { "write" } else { "read" },
                                access.gpa,
                                u8::from(isv),
                                access.header.pc,
                                access.header.instruction_length,
                                access.header.intercept_access_type,
                                exit.exit_reason
                            ));
                        }
                    }
                    if write {
                        let value = if srt == 31 { 0 } else { vm.get(vp, &[X0 + srt])?[0] };
                        let mut said = lock()?;
                        if offset == 0x18 {
                            said.exits.push(at);
                        } else {
                            said.writes.push((vp, offset, value));
                        }
                        match offset {
                            0x38 => said.secondary_started = true,
                            0x50 => {
                                drop(said);
                                vm.interrupt(SPI, true)?;
                            }
                            0x48 => {
                                drop(said);
                                vm.interrupt(SPI, false)?;
                            }
                            0x30 => {
                                said.psci_by_vmm.push(format!(
                                    "interrupt controller state: vCPU 0 {}; vCPU 1 {}; global {}",
                                    vm.state(0, STATE_INTERRUPT_CONTROLLER),
                                    vm.state(1, STATE_INTERRUPT_CONTROLLER),
                                    vm.state(ANY_VP, STATE_GLOBAL_INTERRUPT)
                                ));
                                drop(said);
                                counter(vm)?;
                                time(vm)?;
                            }
                            0x60 | 0x68 | 0x58 => {
                                said.done = true;
                            }
                            _ => {}
                        }
                    } else {
                        let value = match offset {
                            0x08 => 0x5678,
                            _ => 0,
                        };
                        if srt != 31 {
                            vm.put(vp, &[X0 + srt], &[value])?;
                        }
                    }
                    vm.put(vp, &[PC], &[access.header.pc + 4])?;
                    if lock()?.done {
                        return Ok(());
                    }
                }
                EXIT_HYPERCALL => {
                    let call = exit.hypercall();
                    let function = call.x[0] as u32;
                    let pc = vm.get(vp, &[PC])?[0];
                    lock()?.psci_by_vmm.push(format!(
                        "vCPU {vp}: HVC #{} handed to the VMM: X0 {:#x} X1 {:#x} X2 {:#x} X3 {:#x}, header PC {:#x}, PC {pc:#x}",
                        call.immediate, call.x[0], call.x[1], call.x[2], call.x[3], call.header.pc
                    ));
                    let answer: u64 = match function {
                        0x8400_0000 => 0x1_0001,
                        0xc400_0003 => {
                            vm.put(
                                1,
                                &[PC, X0, PSTATE, SCTLR_EL1],
                                &[call.x[2], call.x[3], PSTATE_EL1H, SCTLR_RESET],
                            )?;
                            vm.put(1, &[INTERNAL_ACTIVITY_STATE], &[0])?;
                            0
                        }
                        0x8400_0008 => {
                            let mut said = lock()?;
                            said.done = true;
                            said.reset = Some(u32::MAX);
                            return Ok(());
                        }
                        _ => u64::MAX,
                    };
                    vm.put(vp, &[X0], &[answer])?;
                    if pc == call.header.pc {
                        vm.put(vp, &[PC], &[pc + 4])?;
                    }
                }
                EXIT_ARM64_RESET => {
                    let mut said = lock()?;
                    said.reset = Some(exit.reset().reset_type);
                    said.done = true;
                    return Ok(());
                }
                EXIT_CANCELED => {
                    if stop.load(Ordering::SeqCst) || lock()?.done {
                        return Ok(());
                    }
                }
                other => {
                    let h = exit.header();
                    return Err(format!(
                        "vCPU {vp}: exit {other:#x} at PC {:#x}, after {exits} exits",
                        h.pc
                    ));
                }
            }
        }
        Ok(())
    }

    /// The partition's reference time (100 ns units), its virtual counter as vCPU 0 has it,
    /// and both after 100 ms with the partition's time suspended and after it resumes: what a
    /// snapshot's time rests on.
    fn time(vm: &Vm) -> Result<()> {
        let reference = || -> Result<u64> {
            let mut t = 0u64;
            let mut written = 0u32;
            // SAFETY: a u64, the property's own type.
            hr("WHvGetPartitionProperty(ReferenceTime)", unsafe {
                WHvGetPartitionProperty(
                    vm.partition,
                    PROP_REFERENCE_TIME,
                    (&raw mut t).cast(),
                    8,
                    &raw mut written,
                )
            })?;
            Ok(t)
        };
        let both = || -> Result<(u64, u64)> { Ok((reference()?, vm.get(0, &[CNTVCT_EL0])?[0])) };
        let (r0, c0) = both()?;
        // SAFETY: the partition, its vCPUs out of the guest here (vCPU 1 may run: the call
        // says whether that is refused).
        let suspended = unsafe { WHvSuspendPartitionTime(vm.partition) };
        std::thread::sleep(Duration::from_millis(100));
        let (r1, c1) = both()?;
        // SAFETY: as above.
        let resumed = unsafe { WHvResumePartitionTime(vm.partition) };
        std::thread::sleep(Duration::from_millis(100));
        let (r2, c2) = both()?;
        println!(
            "reference time {r0} then, its time suspended ({:#010x}), {r1} 100 ms later, resumed ({:#010x}), {r2} 100 ms after",
            suspended as u32, resumed as u32
        );
        println!(
            "CNTVCT_EL0 {c0:#x}, {c1:#x}, {c2:#x}: {} and {} ticks",
            c1.wrapping_sub(c0),
            c2.wrapping_sub(c1)
        );
        Ok(())
    }

    /// The most vCPUs a partition takes: each of 64 to 1,024 set up, under the GIC at
    /// shards' places, and its last vCPU made.
    fn limits() {
        for n in [64u32, 128, 256, 512, 1024] {
            let mut partition: Partition = std::ptr::null_mut();
            // SAFETY: a handle the call fills.
            if hr("WHvCreatePartition", unsafe {
                WHvCreatePartition(&raw mut partition)
            })
            .is_err()
            {
                println!("{n} vCPUs: no partition");
                return;
            }
            let vm = Vm { partition };
            let ic = IcParameters {
                emulation_mode: IC_EMULATION_GICV3,
                gic_v3: GicV3Parameters {
                    gicd_base: GICD,
                    gits_translater_base: GITS,
                    ppi_cntv_overflow: PPI_VTIMER,
                    ppi_performance_monitors: PPI_PMU,
                    ..GicV3Parameters::default()
                },
                ..IcParameters::default()
            };
            let set = vm
                .set(PROP_PROCESSOR_COUNT, &n)
                .and_then(|()| vm.set(PROP_ARM64_IC_PARAMETERS, &ic));
            // SAFETY: the partition, its properties set.
            let setup =
                set.and_then(|()| hr("WHvSetupPartition", unsafe { WHvSetupPartition(vm.partition) }));
            // SAFETY: the partition, set up.
            let last = setup.and_then(|()| {
                hr("WHvCreateVirtualProcessor", unsafe {
                    WHvCreateVirtualProcessor(vm.partition, n - 1, 0)
                })
            });
            match last {
                Ok(()) => println!("{n} vCPUs: set up, its vCPU {} made", n - 1),
                Err(e) => {
                    println!("{n} vCPUs: {e}");
                    return;
                }
            }
        }
    }

    /// The virtual counter as vCPU 0 has it, and set a second back: what a restore's time
    /// rests on.
    fn counter(vm: &Vm) -> Result<()> {
        let before = vm.get(0, &[CNTVCT_EL0])?[0];
        let set = vm.put(0, &[CNTVCT_EL0], &[before.saturating_sub(1)]);
        let after = vm.get(0, &[CNTVCT_EL0])?[0];
        let other = vm.get(1, &[CNTVCT_EL0])?[0];
        println!(
            "CNTVCT_EL0: vCPU 0 {before:#x}, set one tick back: {}, then {after:#x}; vCPU 1 {other:#x}",
            match set {
                Ok(()) => "taken".to_string(),
                Err(e) => e,
            }
        );
        Ok(())
    }
}
