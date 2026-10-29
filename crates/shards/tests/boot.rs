//! Boots real Linux guests on the host hypervisor, with each architecture's expectations
//! of what the guest finds (PSCI and GICv3 on arm64; ACPI and the IOAPIC on x86_64).

#![allow(clippy::panic)]

mod common;

use std::time::Duration;

use common::{
    KERNEL_NR_CPUS, cannot_run_vms, cannot_snapshot, guest_init, kernel, run_shards, test_guest, vm_run,
};

const TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_RESET: i32 = 3;

/// Kernel log lines that show the machine as each architecture's firmware describes it.
fn machine_lines() -> &'static [&'static str] {
    match common::ARCH {
        "aarch64" => &[
            "Machine model: linux,dummy-virt",
            "psci: PSCIv1.1 detected in firmware",
            "GICv3: CPU0: found redistributor 0 region 0:0x00000000080a0000",
            "arch_timer: cp15 timer running at 24.00MHz (virt)",
        ],
        // ACPI is how the x86 guest finds its CPUs and interrupt controller.
        "x86_64" => &["ACPI: RSDP 0x00000000000E0000", "IOAPIC[0]: apic_id 0"],
        other => panic!("no machine expectations for {other}"),
    }
}

#[test]
fn boots_to_root_mount_without_a_root_device() {
    if cannot_run_vms() {
        return;
    }
    let k = kernel().to_str().unwrap();
    let r = vm_run(
        &[
            "--kernel",
            k,
            "--memory",
            "256",
            "--cmdline",
            "console=ttyS0 panic=-1",
        ],
        TIMEOUT,
    );
    assert_eq!(r.status, Some(EXIT_RESET), "{r}");
    for line in machine_lines() {
        assert!(r.stdout.contains(line), "missing {line:?}: {r}");
    }
    assert!(
        r.stdout
            .contains("Kernel panic - not syncing: VFS: Unable to mount root fs"),
        "{r}"
    );
}

/// A DAX window's page structures are its head's, not its guest's memory, and its linear
/// map is in blocks (D189, shards' kernel patches 0008 and 0010, PM M300): a guest with a
/// 4 GiB window has as much memory free as one with none, but for the spread of boots,
/// where its page structures took a 64th of the window, 64 MiB, and on arm64 its linear
/// map's page tables 8.3 MiB more, a 512th of head and window in base pages. Free memory
/// is MemFree and the pages its CPUs' free lists hold, which held up to 16 MiB as a boot
/// began (M300); each the most of three boots.
// Shared directories, and so DAX windows, are Unix hosts' alone.
#[cfg(unix)]
#[test]
fn a_dax_windows_page_structures_take_none_of_its_guests_memory() {
    if cannot_run_vms() {
        return;
    }
    let (k, guest) = (kernel().to_str().unwrap(), test_guest().to_str().unwrap());
    let most_free = |window: &str| -> u64 {
        (0..3)
            .map(|_| {
                let r = vm_run(
                    &[
                        "--kernel",
                        k,
                        "--init",
                        guest,
                        "--memory",
                        "512",
                        "--shares",
                        "1",
                        "--share-window",
                        window,
                        "--cmdline",
                        "console=ttyS0 panic=-1 shards_test=memory",
                    ],
                    TIMEOUT,
                );
                assert_eq!(r.status, Some(0), "{r}");
                // `SHARDS-TEST MEMORY <MemTotal> <MemFree> <MemAvailable> <uptime> <listed>`
                let line = r.stdout.lines().find(|l| l.contains("SHARDS-TEST MEMORY"));
                let field =
                    |n: usize| -> Option<u64> { line?.split_whitespace().nth(n)?.trim().parse().ok() };
                field(3)
                    .zip(field(6))
                    .map(|(free, listed)| free + listed)
                    .unwrap_or_else(|| panic!("no free memory said: {r}"))
            })
            .max()
            .unwrap()
    };
    let none = most_free("0");
    let with = most_free("4096");
    let spread = 2 << 20;
    let taken = none.saturating_sub(with);
    assert!(
        taken <= spread,
        "a 4 GiB window took {} KiB of its guest's free memory, past {} KiB",
        taken >> 10,
        spread >> 10
    );
}

/// A DAX window costs its boot what its guest maps of it (D189, shards' kernel patches
/// 0011 and 0012, PM M306): its page structures are made as each range is first mapped,
/// and its head's own pages' never, so a 64 GiB window takes its VM process less than a
/// 4,096th of the window (16 MiB), what its head's own pages' structures alone would,
/// more memory than none: where its page structures took a 64th of it (1,044 MiB on the
/// Mac and on Fedora), and with them made but its head's own pages' 20 MiB; with neither,
/// 0.1 MiB on the Mac and 0.3 on Fedora (2.1, 2.8 and 8.2 on an x86_64 CI runner while
/// a device's reset wrote its window's whole held record, PM M307). Each the least of
/// three boots' peaks.
// Shared directories, and so DAX windows, are Unix hosts' alone.
#[cfg(unix)]
#[test]
fn a_long_dax_window_costs_its_boot_next_to_nothing() {
    if cannot_run_vms() {
        return;
    }
    let (k, guest) = (kernel().to_str().unwrap(), test_guest().to_str().unwrap());
    let least_peak = |window: &str| -> u128 {
        (0..3)
            .map(|_| {
                let r = common::run_shards_env(
                    &["run"],
                    &[
                        "--kernel",
                        k,
                        "--init",
                        guest,
                        "--memory",
                        "512",
                        "--shares",
                        "1",
                        "--share-window",
                        window,
                        "--cmdline",
                        "console=ttyS0 panic=-1 shards_test=memory",
                    ],
                    &[("SHARDS_TIMING", std::ffi::OsStr::new("1"))],
                    TIMEOUT,
                );
                assert_eq!(r.status, Some(0), "{r}");
                r.rss_kib().unwrap_or_else(|| panic!("no peak said: {r}"))
            })
            .min()
            .unwrap()
    };
    let none = least_peak("0");
    let long = least_peak("65536");
    let bound = ((64u128 << 30) / 4096) >> 10;
    let more = long.saturating_sub(none);
    assert!(
        more < bound,
        "a 64 GiB window's boot took {more} KiB of its VM process's memory more than none's, past {bound} KiB"
    );
}

/// A PCI bus (D153): the guest's pci-host-generic finds the test function, QEMU edu's IDs
/// at 00:01.0, and the test guest reaches its BAR's registers, mapped from sysfs.
#[test]
fn a_pci_bus_reaches_its_function() {
    if cannot_run_vms() {
        return;
    }
    let (k, g) = (kernel().to_str().unwrap(), test_guest().to_str().unwrap());
    let r = vm_run(
        &[
            "--kernel",
            k,
            "--init",
            g,
            "--pci-test",
            "--cmdline",
            "console=ttyS0 panic=-1 shards_test=pci",
        ],
        TIMEOUT,
    );
    assert!(r.stdout.contains("SHARDS-TEST PASS"), "{r}");
    assert_eq!(r.status, Some(0), "{r}");
}

/// Secondary CPUs come up through the firmware: PSCI CPU_ON on arm64, INIT/SIPI on x86.
#[test]
fn brings_up_every_vcpu() {
    if cannot_run_vms() {
        return;
    }
    let k = kernel().to_str().unwrap();
    for n in [2u32, 7, 16] {
        let cpus = n.to_string();
        let r = vm_run(
            &[
                "--kernel",
                k,
                "--cpus",
                &cpus,
                "--memory",
                "512",
                "--cmdline",
                "console=ttyS0 panic=-1",
            ],
            TIMEOUT,
        );
        assert!(
            r.stdout.contains(&format!("smp: Brought up 1 node, {n} CPUs")),
            "{n} vCPUs: {r}"
        );
    }
}

/// Two vCPUs booted in this process, unconfined, to their end: the kernel panics for want
/// of a root and resets, as `panic=-1` has it. The control for the SMP boots above, which
/// run in a confined VM process. The one VM this binary makes in its own process
/// (Hypervisor.framework allows one a process).
#[test]
fn two_vcpus_boot_to_their_end_in_this_process() {
    use shards_vmm::vm::{self, Config, Console, ExitReason};
    if cannot_run_vms() {
        return;
    }
    let mut cfg = Config::new(kernel().to_path_buf(), None);
    cfg.vcpus = 2;
    cfg.memory_mib = 512;
    cfg.cmdline = "console=ttyS0 panic=-1".into();
    cfg.console = Console::Discard;
    let (handle, running) = vm::start(&cfg).unwrap_or_else(|e| panic!("{e}"));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(running.wait());
    });
    match rx.recv_timeout(TIMEOUT) {
        Ok(reason) => assert_eq!(reason, ExitReason::Reset),
        Err(_) => {
            handle.stop();
            panic!("two vCPUs did not reach their end in {TIMEOUT:?} in this process");
        }
    }
}

/// The host's most vCPUs: the guest brings up as many as its kernel was built for, and is
/// shown the rest, which its kernel names as it refuses them. Watched until its CPUs are
/// up, then stopped: the rest of a boot of 64 vCPUs on a host of 4 took a CI runner's
/// nested WHP 40 s and more.
#[test]
fn boots_the_hosts_maximum_vcpu_count() {
    if cannot_run_vms() {
        return;
    }
    let max = shards_vmm::vm::max_vcpus().unwrap();
    let k = kernel();
    let cpus = max.to_string();
    let mut vm = common::Vm::spawn(&[
        "run".as_ref(),
        "--kernel".as_ref(),
        k.as_os_str(),
        "--cpus".as_ref(),
        cpus.as_ref(),
        "--memory".as_ref(),
        "1024".as_ref(),
        "--cmdline".as_ref(),
        "console=ttyS0 panic=-1".as_ref(),
    ]);
    let up = max.min(KERNEL_NR_CPUS);
    let plural = if up == 1 { "" } else { "s" };
    let brought = format!("smp: Brought up 1 node, {up} CPU{plural}");
    // A count other than the one asked for fails at once, with what the guest said.
    let said = vm.wait_for_any(&["smp: Brought up"], TIMEOUT);
    vm.kill();
    let seen = vm.seen.join("\n");
    assert!(said.contains(&brought), "{seen}");
    if max > KERNEL_NR_CPUS {
        let refused = match common::ARCH {
            // arch/x86/kernel/cpu/topology.c
            "x86_64" => format!("CPU topo: Rejected CPUs {}", max - KERNEL_NR_CPUS),
            // arch/arm64/kernel/smp.c
            _ => format!("Number of cores ({max}) exceeds configured maximum of {KERNEL_NR_CPUS}"),
        };
        assert!(seen.contains(&refused), "{seen}");
    }
}

#[test]
fn runs_init_as_pid_1_and_powers_off() {
    if cannot_run_vms() {
        return;
    }
    let (k, init) = (kernel().to_str().unwrap(), guest_init().to_str().unwrap());
    let r = vm_run(
        &[
            "--kernel",
            k,
            "--init",
            init,
            "--cmdline",
            "console=ttyS0 quiet panic=-1",
        ],
        TIMEOUT,
    );
    assert_eq!(r.status, Some(0), "{r}");
    assert!(r.stdout.contains("shards-init: pid 1 running"), "{r}");
    let (init_us, exit_us) = (
        r.marker_us(1).expect("init marker"),
        r.exit_us().expect("exit time"),
    );
    assert!(init_us < exit_us, "{r}");
}

#[test]
fn survives_back_to_back_boots() {
    if cannot_run_vms() {
        return;
    }
    let (k, init) = (kernel().to_str().unwrap(), guest_init().to_str().unwrap());
    for i in 0..20 {
        let r = vm_run(
            &[
                "--kernel",
                k,
                "--init",
                init,
                "--cmdline",
                "quiet panic=-1",
                "--no-console",
            ],
            TIMEOUT,
        );
        assert_eq!(r.status, Some(0), "boot {i}: {r}");
        assert!(r.marker_us(1).is_some(), "boot {i}: {r}");
    }
}

fn not_a_kernel() -> &'static str {
    if common::ARCH == "x86_64" {
        "not an x86_64 vmlinux"
    } else {
        "not an arm64 Image"
    }
}

#[test]
fn rejects_invalid_vm_configuration() {
    if cannot_run_vms() {
        return;
    }
    let (k, init) = (kernel().to_str().unwrap(), guest_init().to_str().unwrap());
    let cases: &[(&[&str], &str)] = &[
        (&["--kernel", k, "--cpus", "0"], "at least one vCPU"),
        (&["--kernel", k, "--cpus", "100000"], "vCPUs requested"),
        (&["--kernel", k, "--memory", "63"], "even number of MiB"),
        (&["--kernel", k, "--memory", "65"], "even number of MiB"),
        // Named whole, as the VM process resolved it: Windows gives it a drive.
        (
            &["--kernel", "/nonexistent/kernel"],
            if cfg!(windows) {
                r"\nonexistent\kernel"
            } else {
                "/nonexistent/kernel"
            },
        ),
        (&["--kernel", init], not_a_kernel()),
        (
            &["--kernel", k, "--init", init, "--initrd", init],
            "mutually exclusive",
        ),
    ];
    for (args, message) in cases {
        let r = vm_run(args, TIMEOUT);
        assert_ne!(r.status, Some(0), "{args:?} should fail: {r}");
        assert!(r.stderr.contains(message), "{args:?}: expected {message:?}: {r}");
    }
}

/// Argument errors are reported the same way on every host, backend or not.
#[test]
fn rejects_malformed_vm_arguments() {
    let cases: &[(&[&str], &str)] = &[
        (&["--kernel", "k", "--bogus"], "unknown argument"),
        (&["--kernel"], "--kernel needs a value"),
        (&["--kernel", "k", "--cpus", "two"], "--cpus"),
    ];
    for (args, message) in cases {
        let r = vm_run(args, TIMEOUT);
        assert_eq!(r.status, Some(2), "{args:?}: {r}");
        assert!(r.stderr.contains(message), "{args:?}: expected {message:?}: {r}");
    }
}

/// The platform matrix (docs/design/architecture.md D13): the hosts whose backend has
/// landed must have it, so VM tests can never skip there.
#[test]
fn this_host_has_its_hypervisor_backend() {
    let expected = match (std::env::consts::OS, common::ARCH) {
        ("macos", "aarch64") => Some("hvf"),
        ("linux", "x86_64" | "aarch64") => Some("kvm"),
        ("windows", "x86_64") => Some("whp"),
        _ => None,
    };
    assert_eq!(shards_vmm::hv::BACKEND, expected);
}

/// Where VMs cannot run, `vm run` fails with the reason instead of crashing.
#[test]
fn explains_when_this_host_cannot_run_vms() {
    let Err(why) = shards_vmm::vm::check_host() else {
        return;
    };
    let r = vm_run(&["--kernel", "k"], TIMEOUT);
    assert_eq!(r.status, Some(1), "{r}");
    assert!(r.stderr.contains(&why), "{r}");
}

#[test]
fn cli_reports_usage_and_rejects_unknown_commands() {
    let none: [&str; 0] = [];
    let help = run_shards(&["--help"], &none, TIMEOUT);
    assert_eq!(help.status, Some(0), "{help}");
    assert!(help.stdout.contains("\n  run "), "{help}");
    // As docker/cli refuses one: status 1, its words.
    let unknown = run_shards(&["frobnicate"], &none, TIMEOUT);
    assert_eq!(unknown.status, Some(1), "{unknown}");
    assert!(
        unknown.stderr.contains("unknown command: shards frobnicate"),
        "{unknown}"
    );
    // Piped, as docker/cli lays its version out; the server is this shards too.
    let version = run_shards(&["version"], &none, TIMEOUT);
    assert!(version.stdout.starts_with("Client:\n Version:"), "{version}");
    assert!(
        version.stdout.contains("\nServer: shards\n Engine:\n"),
        "{version}"
    );
    let short = run_shards(&["--version"], &none, TIMEOUT);
    assert!(short.stdout.starts_with("shards version "), "{short}");
    let server = run_shards(&["version", "-f", "{{.Server.Platform.Name}}"], &none, TIMEOUT);
    assert_eq!(server.stdout, "shards\n", "{server}");
}

/// A guest given virtualization of its own (`--nested`, D132) boots at EL2 on a Mac that
/// gives one, its secondary CPUs brought up by PSCI over SMC, and its KVM comes up as a
/// hypervisor: the pinned kernel's `kvm` variant. A snapshot of such a guest is refused up
/// front; an x86_64 host refuses `--nested` until it is built there.
#[test]
fn a_nested_guest_is_a_kvm_host() {
    if cannot_run_vms() {
        return;
    }
    let refused = vm_run(
        &[
            "--kernel",
            kernel().to_str().unwrap(),
            "--nested",
            "--snapshot-dir",
            "nowhere",
        ],
        TIMEOUT,
    );
    assert_eq!(refused.status, Some(2), "{refused}");
    assert!(
        refused.stderr.contains("--nested with --snapshot-dir"),
        "{refused}"
    );
    let Some(nested) = common::nested_kernel() else {
        let r = vm_run(&["--kernel", kernel().to_str().unwrap(), "--nested"], TIMEOUT);
        assert!(
            r.stderr
                .contains("virtualization of a guest's own is not given on x86_64 yet"),
            "{r}"
        );
        return;
    };
    let r = vm_run(
        &[
            "--kernel",
            nested.to_str().unwrap(),
            "--init",
            test_guest().to_str().unwrap(),
            "--nested",
            "--cpus",
            "2",
            "--memory",
            "512",
            "--cmdline",
            "console=ttyS0 panic=-1 shards_test=kmsg",
        ],
        TIMEOUT,
    );
    if r.stderr.contains("this Mac gives a guest no EL2") {
        eprintln!("SKIP: this Mac gives a guest no EL2");
        return;
    }
    assert_eq!(r.status, Some(0), "{r}");
    for line in [
        "psci: SMC Calling Convention",
        "smp: Brought up 1 node, 2 CPUs",
        "CPU: All CPU(s) started at EL2",
        "kvm [1]: vgic interrupt IRQ9",
        "mode initialized successfully",
        "SHARDS-TEST PASS",
    ] {
        assert!(r.stdout.contains(line), "missing {line:?}: {r}");
    }
}

/// What the test guest says of the interrupts it took (`pci-irq`).
const INTERRUPTS: &str = "SHARDS-TEST INFO interrupts: INTx 2, MSI 1, MSI-X 3, 2 of them held";

/// A guest's VFIO takes a group with an IOMMU, and its iommufd a function, only where
/// their MSIs are isolated, or where told that they need not be (vfio_iommu_type1.c;
/// iommufd/device.c, iommufd_device_bind; each's `allow_unsafe_interrupts`): the tests'
/// guests are told so where the machine isolates none (vm::ISOLATED_MSIS), and on arm64
/// KVM, whose ITS isolates them, told nothing.
fn unsafe_interrupts() -> &'static str {
    if shards_vmm::vm::ISOLATED_MSIS {
        ""
    } else {
        " vfio_iommu_type1.allow_unsafe_interrupts=1 iommufd.allow_unsafe_interrupts=1"
    }
}

/// A PCI function's interrupts reach the guest (D154): the test function, taken by vfio-pci
/// in the guest's own userspace, raises INTx twice, acknowledged and unmasked between,
/// then one MSI, then MSI-X vectors three times, two of them held pending while masked and
/// sent as unmasked, each signalled on an eventfd. On arm64, INTx on an SPI of the
/// device's own and the MSIs through the GIC's ITS (KVM's) or its GICv2m frame
/// (Hypervisor.framework's); on x86_64, INTx on the bus's GSI as the DSDT's _PRT says, and
/// the MSIs to the local APICs.
#[test]
fn a_pci_functions_interrupts_reach_the_guest() {
    if cannot_run_vms() {
        return;
    }
    let Some(kernel) = common::kvm_kernel() else {
        eprintln!("SKIP: no pinned kernel with VFIO for {}", common::ARCH);
        return;
    };
    let r = vm_run(
        &[
            "--kernel",
            kernel.to_str().unwrap(),
            "--init",
            test_guest().to_str().unwrap(),
            "--pci-test",
            "--cmdline",
            &format!(
                "console=ttyS0 panic=-1 vfio.enable_unsafe_noiommu_mode=1{} shards_test=pci-irq",
                unsafe_interrupts()
            ),
        ],
        TIMEOUT,
    );
    assert_eq!(r.status, Some(0), "{r}");
    assert!(r.stdout.contains(INTERRUPTS), "{r}");
    assert!(
        r.stdout.lines().any(|l| l.trim_end() == "SHARDS-TEST PASS"),
        "{r}"
    );
}

/// A PCI function's DMA goes through the machine's IOMMU (D154): the test guest, its
/// kernel virtio-iommu's driver, takes the test function through vfio-pci in a group with
/// an IOMMU, maps two pages of its own at I/O addresses of its choosing, and has the
/// function's DMA engine move a page of a pattern from one into its buffer and back to the
/// other, then to an address it never mapped, which moves nothing.
#[test]
fn a_pci_functions_dma_goes_through_its_iommu() {
    if cannot_run_vms() {
        return;
    }
    let Some(kernel) = common::kvm_kernel() else {
        eprintln!("SKIP: no pinned kernel with VFIO for {}", common::ARCH);
        return;
    };
    let r = vm_run(
        &[
            "--kernel",
            kernel.to_str().unwrap(),
            "--init",
            test_guest().to_str().unwrap(),
            "--pci-test",
            "--cmdline",
            &format!(
                "console=ttyS0 panic=-1{} shards_test=pci-dma",
                unsafe_interrupts()
            ),
        ],
        TIMEOUT,
    );
    assert_eq!(r.status, Some(0), "{r}");
    assert!(r.stdout.contains(DMA_THROUGH_IOMMU), "{r}");
    assert!(
        r.stdout.lines().any(|l| l.trim_end() == "SHARDS-TEST PASS"),
        "{r}"
    );
}

/// What the test guest says of its DMA (`pci-dma`): through an IOMMU, or by guest-physical
/// address where its group has none.
const DMA_THROUGH_IOMMU: &str =
    "SHARDS-TEST INFO dma: 4096 bytes into the function and back, through its IOMMU";
const DMA_BY_ADDRESS: &str =
    "SHARDS-TEST INFO dma: 4096 bytes into the function and back, through guest-physical addresses";

/// A guest that is itself a KVM host runs VMs of its own with shards' VM process for its
/// Linux (D132, D154), on arm64 with shards' KVM backend: the inner guest boots on KVM's
/// GICv3, its second vCPU brought up by KVM's PSCI, and passes its test; its PCI bus
/// reaches the test function, whose interrupts reach it, INTx and MSI, through the VMM's
/// GICv2m frame; and a PCI function the outer guest was given, bound to vfio-pci there,
/// reaches the inner guest passed through by VFIO, its configuration space and its BAR the
/// outer function's, and its INTx and MSI by KVM's irqfds; behind the inner machine's own
/// IOMMU, its MSIs land where the inner guest's IOMMU maps their addresses, and its DMA
/// reaches the inner guest's pages at the inner guest's own I/O addresses, which the outer
/// VM process's container maps as the inner IOMMU does. Taken by its VFIO device node and
/// iommufd instead, its interrupts are the same, and its DMA reaches all of the inner
/// guest's RAM, which an I/O address space of the outer guest's maps. An x86_64 host
/// gives no guest virtualization of its own yet.
#[test]
fn a_nested_guest_runs_vms_of_its_own() {
    if cannot_run_vms() {
        return;
    }
    let Some(nested) = common::nested_kernel() else {
        eprintln!("SKIP: no nested guest kernel for {}", common::ARCH);
        return;
    };
    let dir = common::TempDir::new("nest");
    let initrd = dir.join("l1.cpio");
    let read = |p: &std::path::Path| std::fs::read(p).unwrap();
    let mut cpio = shards_vmm::initramfs::Cpio::new();
    cpio.dir("dev", 0o755)
        .char_dev("dev/console", 0o600, 5, 1)
        .dir("proc", 0o555)
        .dir("sys", 0o555)
        .file("init", 0o755, &read(test_guest()))
        .file("vm", 0o755, &read(common::guest_vm_process()))
        // The inner guests' kernel has VFIO too, which takes their PCI functions'
        // interrupts.
        .file("l2", 0o644, &read(nested));
    std::fs::write(&initrd, cpio.finish()).unwrap();
    let outer = |args: &[&str], cmdline: &str| {
        let cmdline = format!("console=ttyS0 panic=-1 shards_test=host {cmdline}");
        let mut all = vec![
            "--kernel",
            nested.to_str().unwrap(),
            "--initrd",
            initrd.to_str().unwrap(),
            "--nested",
            "--cpus",
            "2",
            "--memory",
            "1024",
            "--cmdline",
            &cmdline,
        ];
        all.extend_from_slice(args);
        vm_run(&all, TIMEOUT * 3)
    };
    let passed = |r: &common::Run| {
        let line = |want: &str| r.stdout.lines().any(|l| l.trim_end() == want);
        assert!(line("L2: SHARDS-TEST PASS"), "the inner guest: {r}");
        assert!(line("SHARDS-TEST PASS"), "the outer guest: {r}");
    };
    let booted = outer(&[], "shards_l2=kmsg");
    if booted.stderr.contains("gives a guest no EL2") {
        eprintln!("SKIP: this host gives a guest no EL2");
        return;
    }
    assert_eq!(booted.status, Some(0), "{booted}");
    passed(&booted);
    for line in [
        "smp: Brought up 1 node, 2 CPUs",
        "GICv3: CPU1: found redistributor 1",
    ] {
        assert!(
            booted
                .stdout
                .lines()
                .any(|l| l.starts_with("L2: ") && l.contains(line)),
            "the inner guest lacks {line:?}: {booted}"
        );
    }
    let interrupted = |r: &common::Run| {
        assert!(
            r.stdout
                .lines()
                .any(|l| l.trim_end() == format!("L2: {INTERRUPTS}")),
            "the inner guest's interrupts: {r}"
        );
    };
    let dma = |r: &common::Run, how: &str| {
        assert!(
            r.stdout.lines().any(|l| l.trim_end() == format!("L2: {how}")),
            "the inner guest's DMA: {r}"
        );
    };
    for test in ["pci", "pci-irq", "pci-dma"] {
        let bus = outer(&[], &format!("shards_l2={test} shards_l2_args=--pci-test"));
        assert_eq!(bus.status, Some(0), "{bus}");
        passed(&bus);
        if test == "pci-irq" {
            interrupted(&bus);
        }
        // The inner guest's own bus, behind the outer VM process's IOMMU.
        if test == "pci-dma" {
            dma(&bus, DMA_THROUGH_IOMMU);
        }
        let passed_through = outer(
            &["--pci-test"],
            &format!(
                "shards_l2={test} shards_l1_vfio=0000:00:01.0 shards_l2_args=--vfio,0000:00:01.0 vfio.enable_unsafe_noiommu_mode=1 vfio_iommu_type1.allow_unsafe_interrupts=1 shards_l1_log=debug"
            ),
        );
        assert_eq!(passed_through.status, Some(0), "{passed_through}");
        passed(&passed_through);
        if test == "pci-irq" {
            interrupted(&passed_through);
        }
        // The inner guest has no IOMMU, and gives the function its pages' guest-physical
        // addresses: the I/O addresses the outer VM process mapped its RAM at, through
        // the outer guest's IOMMU, which the outermost VMM's function's DMA goes through.
        if test == "pci-dma" {
            dma(&passed_through, DMA_BY_ADDRESS);
        }
        // By its VFIO device node and the outer guest's iommufd (`--vfio
        // /dev/vfio/devices/vfio0`, the outer guest's one VFIO device), not its group: all
        // of the inner guest's RAM mapped in an I/O address space once the function is
        // attached to it, its interrupts as through its group; told nothing of type 1.
        if test != "pci" {
            let by_node = outer(
                &["--pci-test"],
                &format!(
                    "shards_l2={test} shards_l1_vfio=0000:00:01.0 shards_l2_args=--vfio,/dev/vfio/devices/vfio0 iommufd.allow_unsafe_interrupts=1 shards_l1_log=debug"
                ),
            );
            assert_eq!(by_node.status, Some(0), "{by_node}");
            passed(&by_node);
            if test == "pci-irq" {
                interrupted(&by_node);
            } else {
                dma(&by_node, DMA_BY_ADDRESS);
            }
        }
        // Behind the inner machine's own IOMMU (`--vfio-iommu`): the inner guest's VFIO
        // gives the function I/O addresses of its own, 0x10_0000 and 0x20_0000, where the
        // inner guest has no RAM (it starts at 2 GiB), so its pages come back only where
        // the outer VM process's container maps what the inner IOMMU maps; and on arm64
        // the inner guest maps its GICv2m doorbell there for the function's MSIs. With
        // memory to plug and free page reporting beside it, which take nothing the
        // function's DMA holds pinned.
        if test != "pci" {
            let more = if test == "pci-dma" {
                ",--memory-max,1024,--reporting"
            } else {
                ""
            };
            let behind = outer(
                &["--pci-test"],
                &format!(
                    "shards_l2={test} shards_l1_vfio=0000:00:01.0 shards_l2_args=--vfio-iommu,0000:00:01.0{more} vfio_iommu_type1.allow_unsafe_interrupts=1"
                ),
            );
            assert_eq!(behind.status, Some(0), "{behind}");
            passed(&behind);
            if test == "pci-irq" {
                interrupted(&behind);
            } else {
                dma(&behind, DMA_THROUGH_IOMMU);
            }
        }
        // BARs 0 and 4 in the inner guest's memory as the guest enables the function's
        // memory space, reached without exits to the outer VM process; BAR 2, all MSI-X,
        // trapped. Disabled, they leave (`pci`), and come back.
        let logged = |what: &str| {
            passed_through
                .stdout
                .lines()
                .filter(|l| {
                    l.starts_with("L2 stderr: ")
                        && l.ends_with(&format!("0000:00:01.0: BAR windows {what} the guest: 2"))
                })
                .count()
        };
        assert!(logged("mapped into") >= 1, "BARs never mapped: {passed_through}");
        if test == "pci" {
            assert!(
                logged("taken from") >= 1,
                "BARs never taken out: {passed_through}"
            );
            assert!(logged("mapped into") >= 2, "BARs never back: {passed_through}");
        }
    }
}

/// A run given a host's VFIO group (`shards run --device /dev/vfio/N`, D154), on a host
/// that has one to give: a nested guest given test functions, a device of one and a
/// device of two (as a GPU with its audio function), runs shards as a Linux host does.
/// The two functions, one IOMMU group of the outer guest's, its group 1, it gives
/// vfio-pci, and a run is given their group, by its node, by a CDI device whose node it
/// is and by the directory of VFIO's nodes, then each function by its VFIO device node and
/// iommufd, and by the directory of those; whose microVM
/// shards' daemon boots with them behind the microVM's own IOMMU, one device there, which
/// its guest takes as its own group 0. Each run's workload, given the group's node, or
/// the functions', where its guest's sysfs puts them, takes each function through VFIO as
/// DPDK finds one, by its address in sysfs, and moves a page through each by DMA, one
/// container's or address space's mappings for both. An x86_64 host gives no guest
/// virtualization of its own yet.
#[test]
fn a_run_is_given_a_hosts_vfio_group() {
    if cannot_run_vms() {
        return;
    }
    let Some(nested) = common::nested_kernel() else {
        eprintln!("SKIP: no nested guest kernel for {}", common::ARCH);
        return;
    };
    let dir = common::TempDir::new("nest-run");
    let read = |p: &std::path::Path| std::fs::read(p).unwrap();
    let base = |cpio: &mut shards_vmm::initramfs::Cpio| {
        cpio.dir("dev", 0o755)
            .char_dev("dev/console", 0o600, 5, 1)
            .dir("proc", 0o555)
            .dir("sys", 0o555)
            .file("init", 0o755, &read(test_guest()));
    };
    let outer = |initrd: &std::path::Path, cmdline: &str| {
        let cmdline = format!("console=ttyS0 panic=-1 {cmdline}");
        vm_run(
            &[
                "--kernel",
                nested.to_str().unwrap(),
                "--initrd",
                initrd.to_str().unwrap(),
                "--nested",
                "--pci-tests",
                "01.0,02.0,02.1",
                "--cpus",
                "2",
                "--memory",
                "2048",
                "--cmdline",
                &cmdline,
            ],
            TIMEOUT * 3,
        )
    };
    // Whether this host gives a guest EL2, before shards is built for one.
    let probe = dir.join("probe.cpio");
    let mut cpio = shards_vmm::initramfs::Cpio::new();
    base(&mut cpio);
    std::fs::write(&probe, cpio.finish()).unwrap();
    let probed = outer(&probe, "shards_test=kmsg");
    if probed.stderr.contains("gives a guest no EL2") {
        eprintln!("SKIP: this host gives a guest no EL2");
        return;
    }
    assert_eq!(probed.status, Some(0), "{probed}");
    let initrd = dir.join("l1.cpio");
    let mut cpio = shards_vmm::initramfs::Cpio::new();
    base(&mut cpio);
    cpio.file("shards", 0o755, &read(common::guest_shards()))
        .file("shards-init", 0o755, &read(common::guest_init()))
        // The run's kernel, with VFIO and virtio-iommu.
        .file("l2", 0o644, &read(nested))
        .file("image.tar", 0o644, &common::image_archive("vfio-test:latest"));
    std::fs::write(&initrd, cpio.finish()).unwrap();
    // The outer guest is a host whose IOMMU isolates no MSIs (GICv2m's), whose VFIO and
    // iommufd its administrator allows them.
    let ran = outer(
        &initrd,
        "shards_test=daemon shards_l1_vfio=0000:00:02.0,0000:00:02.1 shards_l2=vfio-dma,0000:00:01.0,0000:00:01.1 vfio_iommu_type1.allow_unsafe_interrupts=1 iommufd.allow_unsafe_interrupts=1 shards_l1_log=debug",
    );
    let lines = |want: &str| ran.stdout.lines().filter(|l| l.trim_end() == want).count();
    assert!(
        !ran.stdout.contains("L1: no VFIO device nodes"),
        "the nested kernel makes no VFIO device nodes: {ran}"
    );
    assert_eq!(
        lines(&format!("L2: {DMA_THROUGH_IOMMU}")),
        10,
        "the runs' DMA: {ran}"
    );
    assert_eq!(lines("SHARDS-TEST PASS"), 1, "the outer guest: {ran}");
    assert_eq!(ran.status, Some(0), "{ran}");
}

/// shards' snapshot tests pass on a KVM host of this architecture's Linux (D154): a guest
/// given virtualization of its own (`--nested`, D132), its kernel the pinned `kvm`
/// variant's, runs them as such a host runs them, built for it (`common::linux_tests`),
/// with real VMs of its own, KVM's: snapshots taken and restored, cold and from warm
/// pools, templates and their working sets, microVMs saved and restored. Every VM test
/// must run there (SHARDS_REQUIRE_VMS). A host that gives a guest no EL2 (a Mac before
/// the M3; x86_64, not yet) skips, before building anything.
#[test]
fn snapshot_tests_pass_on_a_kvm_host_of_this_architecture() {
    if cannot_run_vms() {
        return;
    }
    let Some(nested) = common::nested_kernel() else {
        eprintln!("SKIP: no nested guest kernel for {}", common::ARCH);
        return;
    };
    let dir = common::TempDir::new("l1-e2e");
    let read = |p: &std::path::Path| std::fs::read(p).unwrap();
    let outer = |initrd: &std::path::Path, cpus: &str, memory: &str, cmdline: &str, timeout| {
        vm_run(
            &[
                "--kernel",
                nested.to_str().unwrap(),
                "--initrd",
                initrd.to_str().unwrap(),
                "--nested",
                "--cpus",
                cpus,
                "--memory",
                memory,
                "--cmdline",
                &format!("console=ttyS0 panic=-1 {cmdline}"),
            ],
            timeout,
        )
    };
    // Whether this host gives a guest EL2, before the tests are built for one.
    let probe = dir.join("probe.cpio");
    std::fs::write(&probe, shards_vmm::initramfs::with_init(&read(test_guest()))).unwrap();
    let probed = outer(&probe, "1", "256", "shards_test=kmsg", TIMEOUT);
    if probed.stderr.contains("gives a guest no EL2") {
        eprintln!("SKIP: this host gives a guest no EL2");
        return;
    }
    assert_eq!(probed.status, Some(0), "{probed}");
    // Each test binary, and what it is given: whole files, and of the rest the tests of
    // snapshots alone (`cannot_snapshot`), one at a time: a nested guest's exits each go
    // through this host's hypervisor, its KVM's nVHE world switches among them, and two of
    // the daemon's tests together, some 20 VMs of eight vCPUs on its eight, had restored
    // guests stall 30 s between resuming and dialing their host, their runs past 60 s, in
    // 11 of 24 rounds, where the one that failed passed 9 of 9 alone (PM M346).
    // SHARDS_L1_TESTS names others instead (`binary` or `binary:filter`, comma-separated),
    // SHARDS_L1_LOG their SHARDS_LOG, and SHARDS_TEST_LOG_SHOWN how much of a failing
    // test's logs they show.
    let runs: Vec<(String, String)> = match std::env::var("SHARDS_L1_TESTS") {
        Ok(chosen) => chosen
            .split(',')
            .map(|run| match run.split_once(':') {
                Some((test, filter)) => (test.to_string(), filter.to_string()),
                None => (run.to_string(), String::new()),
            })
            .collect(),
        Err(_) => [
            ("snapshot", ""),
            ("restore", ""),
            ("warm", ""),
            ("saved", ""),
            ("daemon", ""),
            ("run", "templates_restore_into_runs_of_their_own"),
            ("vsock", "restored_copies"),
            ("pmem", "restored_copies"),
            ("boot", "a_snapshot_keeps"),
        ]
        .map(|(test, filter)| (test.to_string(), filter.to_string()))
        .to_vec(),
    };
    let tests: Vec<&str> = runs.iter().map(|(t, _)| t.as_str()).collect();
    let built = common::linux_tests(&tests);
    // The host's CA certificates, as every Linux host has a bundle of them, where Linux's
    // TLS libraries look (rustls-native-certs, openssl-probe): the registry client trusts
    // the system's roots, and on Linux builds no verifier without them (registry tls.rs).
    let roots = [
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/cert.pem",
    ]
    .iter()
    .find_map(|path| std::fs::read(path).ok())
    .expect("no CA bundle on this host");
    let mut cpio = shards_vmm::initramfs::Cpio::new();
    cpio.dir("dev", 0o755)
        .char_dev("dev/console", 0o600, 5, 1)
        .dir("proc", 0o555)
        .dir("sys", 0o555)
        .dir("tmp", 0o1777)
        .dir("etc", 0o755)
        .dir("etc/ssl", 0o755)
        .dir("etc/ssl/certs", 0o755)
        .file("etc/ssl/certs/ca-certificates.crt", 0o644, &roots)
        .file("init", 0o755, &read(test_guest()))
        .dir("e2e", 0o755)
        .dir("e2e/bin", 0o755)
        .dir("e2e/tests", 0o755)
        .dir("e2e/guests", 0o755)
        .dir("e2e/guests/guest", 0o755)
        .file("e2e/guests/guest/shards-testguest", 0o755, &read(test_guest()));
    // The files the binaries name by their build's paths, the kernel their VMs boot among
    // them, each under the directories that hold it; and the checkout's, which they
    // name as their workspace.
    let mut dirs = std::collections::BTreeSet::new();
    let mut place = |cpio: &mut shards_vmm::initramfs::Cpio, path: &std::path::Path, data: Option<&[u8]>| {
        let relative = path.strip_prefix("/").unwrap();
        for ancestor in relative.ancestors().skip(1).collect::<Vec<_>>().into_iter().rev() {
            let name = ancestor.to_str().unwrap();
            if !name.is_empty() && dirs.insert(name.to_string()) {
                cpio.dir(name, 0o755);
            }
        }
        match data {
            Some(bytes) => {
                cpio.file(relative.to_str().unwrap(), 0o755, bytes);
            }
            None => {
                if dirs.insert(relative.to_str().unwrap().to_string()) {
                    cpio.dir(relative.to_str().unwrap(), 0o755);
                }
            }
        }
    };
    for file in built
        .files
        .iter()
        .map(std::path::PathBuf::as_path)
        .chain([kernel()])
    {
        place(&mut cpio, file, Some(&read(file)));
    }
    place(&mut cpio, &common::workspace().join("crates/shards"), None);
    let (mut run, mut names) = (String::new(), Vec::new());
    for (test, args) in &runs {
        let (_, binary) = built
            .binaries
            .iter()
            .find(|(name, _)| name == test)
            .unwrap_or_else(|| panic!("no test binary {test} built"));
        let name = format!("e2e/tests/{test}");
        cpio.file(&name, 0o755, &read(binary));
        run.push_str(&format!("/{name} --test-threads=1 {args}\n"));
        names.push(name);
    }
    let mut env = String::from(
        "HOME=/tmp\nPATH=/e2e/bin\nTMPDIR=/tmp\nXDG_CACHE_HOME=/tmp/cache\nRUST_BACKTRACE=1\nSHARDS_REQUIRE_VMS=1\nSHARDS_TEST_GUESTS=/e2e/guests\n",
    );
    if let Ok(level) = std::env::var("SHARDS_L1_LOG") {
        env.push_str(&format!("SHARDS_LOG={level}\n"));
    }
    if let Ok(bytes) = std::env::var("SHARDS_TEST_LOG_SHOWN") {
        env.push_str(&format!("SHARDS_TEST_LOG_SHOWN={bytes}\n"));
    }
    cpio.file("e2e/run", 0o644, run.as_bytes())
        .file("e2e/env", 0o644, env.as_bytes());
    let initrd = dir.join("l1.cpio");
    std::fs::write(&initrd, cpio.finish()).unwrap();
    let ran = outer(
        &initrd,
        "8",
        "24576",
        "shards_test=e2e",
        Duration::from_secs(45 * 60),
    );
    // SHARDS_L1_CONSOLE names a file its console is written to, passing or failing, for
    // what the tests in it say of themselves.
    if let Some(console) = std::env::var_os("SHARDS_L1_CONSOLE") {
        std::fs::write(console, ran.to_string()).unwrap();
    }
    for name in &names {
        assert!(
            ran.stdout
                .lines()
                .any(|l| l.trim_end() == format!("E2E STATUS /{name} exit status: 0")),
            "/{name} failed on Linux: {ran}"
        );
    }
    assert!(!ran.stdout.contains("SKIP: snapshots are not supported"), "{ran}");
    assert!(
        ran.stdout.lines().any(|l| l.trim_end() == "SHARDS-TEST PASS"),
        "{ran}"
    );
    assert_eq!(ran.status, Some(0), "{ran}");
}

/// A snapshot of a guest with memory to plug, none of it plugged (D148): the VM that
/// booted and each VM restored from it plug memory as their guest asks, and write it.
#[test]
fn a_snapshot_keeps_memory_to_plug() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let dir = common::TempDir::new("snap-plug");
    let snapshot = dir.join("s");
    let cmdline = "console=ttyS0 quiet panic=-1 shards_test=snapshot-plug shards_plug_mib=512 memhp_default_state=online_movable";
    let booted = run_shards(
        &["run"],
        &[
            "--kernel",
            kernel().to_str().unwrap(),
            "--init",
            test_guest().to_str().unwrap(),
            "--memory",
            "256",
            "--memory-max",
            "1024",
            "--cmdline",
            cmdline,
            "--snapshot-dir",
            snapshot.to_str().unwrap(),
            "--snapshot-then",
            "resume",
        ],
        TIMEOUT,
    );
    for run in [booted]
        .into_iter()
        .chain((0..2).map(|_| run_shards(&["restore"], &[snapshot.to_str().unwrap()], TIMEOUT)))
    {
        assert!(run.stdout.contains("SHARDS-TEST PLUGGED "), "{run}");
        assert!(run.stdout.contains("SHARDS-TEST PASS"), "{run}");
    }
}

/// A snapshot of a guest with memory plugged keeps it (D176): the guest plugs 512 MiB,
/// writes 448 MiB, more than it booted with, and only then asks for a snapshot; the VM that
/// booted and each VM restored from it find all it wrote.
#[test]
fn a_snapshot_keeps_the_memory_its_guest_plugged() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let dir = common::TempDir::new("plug-snap");
    let snapshot = dir.join("s");
    let cmdline = "console=ttyS0 quiet panic=-1 shards_test=plug-snapshot shards_plug_mib=512 memhp_default_state=online_movable";
    let booted = run_shards(
        &["run"],
        &[
            "--kernel",
            kernel().to_str().unwrap(),
            "--init",
            test_guest().to_str().unwrap(),
            "--memory",
            "256",
            "--memory-max",
            "1024",
            "--cmdline",
            cmdline,
            "--snapshot-dir",
            snapshot.to_str().unwrap(),
            "--snapshot-then",
            "resume",
        ],
        TIMEOUT,
    );
    for run in [booted]
        .into_iter()
        .chain((0..2).map(|_| run_shards(&["restore"], &[snapshot.to_str().unwrap()], TIMEOUT)))
    {
        assert!(run.stdout.contains("SHARDS-TEST KEPT 448"), "{run}");
        assert!(run.stdout.contains("SHARDS-TEST PASS"), "{run}");
    }
}

/// Memory plugged as the guest asks for it (`--memory-max`, D133). A guest booted with 256
/// MiB asks for 1 GiB more, its kernel brings it all online, and the guest fills seven
/// eighths of it, which the VM process then holds; it frees it and asks for none, its kernel
/// gives it all back, the VM process's own memory falls by it, and the VM process holds
/// none of the pages of the memory it gave back as it ends (`hotplug_held_kib`: on macOS a
/// page out of the hypervisor's mappings is out of the footprint, held or not, PM M193; a
/// block the guest's driver is plugging as it ends, asked for everything and then nothing,
/// is the guest's, and none of what it gave back). Asked for more
/// than it may have, it is given what it may. On the hosts whose VMs it measures the
/// memory of (`process_usage`): Windows runs none yet.
#[cfg(unix)]
#[test]
fn a_guest_grows_and_gives_back_its_memory() {
    use std::io::{BufRead, Write};
    if cannot_run_vms() {
        return;
    }
    let mut child = common::command()
        .env("SHARDS_TIMING", "1")
        .args(["run", "--kernel", kernel().to_str().unwrap()])
        .args(["--init", test_guest().to_str().unwrap()])
        .args(["--memory", "256", "--memory-max", "1280", "--cmdline"])
        .arg("console=ttyS0 panic=-1 shards_test=hotplug shards_plug_mib=1024 memhp_default_state=online_movable")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // `shards run --kernel` becomes the VM process: the same pid.
    let pid = child.id();
    let mut stdin = child.stdin.take().unwrap();
    // Its stderr drained as it runs, so that what it writes there never waits on the test.
    let mut stderr = child.stderr.take().unwrap();
    let err = std::thread::spawn(move || {
        let mut err = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut err);
        err
    });
    let (tx, lines) = std::sync::mpsc::channel();
    let out = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let start = std::time::Instant::now();
    let mut said = String::new();
    let mut until = |marker: &str| -> String {
        loop {
            let left = TIMEOUT.saturating_sub(start.elapsed());
            let Ok(line) = lines.recv_timeout(left) else {
                let _ = child.kill();
                panic!("no {marker:?} within {TIMEOUT:?}:\n{said}");
            };
            said.push_str(&line);
            said.push('\n');
            if line.contains("SHARDS-TEST FAIL") {
                let _ = child.kill();
                panic!("{said}");
            }
            if let Some((_, rest)) = line.split_once(marker) {
                return rest.trim().to_string();
            }
        }
    };
    let footprint = || shards_vmm::platform::process_usage(pid).unwrap().footprint;
    let plugged_ms: u64 = until("SHARDS-TEST PLUGGED").parse().unwrap();
    let full = footprint();
    stdin.write_all(b"\n").unwrap();
    let unplugged_ms: u64 = until("SHARDS-TEST UNPLUGGED").parse().unwrap();
    let given_back = footprint();
    stdin.write_all(b"\n").unwrap();
    until("SHARDS-TEST PASS");
    eprintln!(
        "1 GiB plugged in {plugged_ms} ms, unplugged in {unplugged_ms} ms; the VM process held {} MiB, then {} MiB",
        full >> 20,
        given_back >> 20
    );
    // The guest wrote 896 MiB of what was plugged: the host gave that, and has it back.
    assert!(full >= 896 << 20, "the VM process held {full} bytes");
    assert!(
        full.saturating_sub(given_back) >= 768 << 20,
        "the VM process held {full} bytes, then {given_back}"
    );
    let status = child.wait().unwrap();
    let err = err.join().unwrap();
    assert!(status.success(), "{err}");
    let timing: serde_json::Value = err
        .lines()
        .find_map(|l| l.strip_prefix("shards-timing "))
        .map(|t| serde_json::from_str(t).unwrap())
        .unwrap_or_else(|| panic!("no timing line: {err}"));
    let held = timing["hotplug_held_kib"].as_u64().unwrap();
    assert!(
        held < 16 << 10,
        "the VM process held {held} KiB of the memory it gave back"
    );
}

/// What cold boots at once cost on a Windows host (PM M376): guests of as many vCPUs as the
/// host has CPUs, as the daemon boots a run's, one, two, four and eight at once, then guests
/// of one vCPU, eight and sixteen at once; each its console's lines as they come and the
/// guest's own times on them, until its kernel, given no root, panics and resets; each VM
/// process's CPU time, user and kernel, read once it has ended. Under CI's whole suite a
/// run's cold boot took past 20 s, its VM process at 0.6 of a host CPU throughout (CI
/// 40715919365, windows-2022): whether a guest's vCPUs waiting on one another make a boot
/// cost more as more boot at once, which guests of one vCPU would not.
#[cfg(windows)]
#[test]
#[ignore = "a measurement: budget-e2e.yml windows:boot, ignored:cold_boots_at_once_cost"]
fn cold_boots_at_once_cost() {
    let host = std::thread::available_parallelism().unwrap().get();
    boots_at_once(&[(host, 1), (host, 2), (host, 4), (host, 8), (1, 8), (1, 16)].map(|(v, n)| (v, n, "")));
}

/// The same, each case beside its kernel told `quiet`, which writes only warnings and worse
/// to its console: whether a boot's console lines, each byte two port exits, are what it
/// spends (PM M376).
#[cfg(windows)]
#[test]
#[ignore = "a measurement: budget-e2e.yml windows:boot, ignored:cold_boots_at_once_cost_quiet"]
fn cold_boots_at_once_cost_quiet() {
    let host = std::thread::available_parallelism().unwrap().get();
    boots_at_once(&[
        (host, 1, ""),
        (host, 4, ""),
        (host, 1, " quiet"),
        (host, 2, " quiet"),
        (host, 4, " quiet"),
        (host, 8, " quiet"),
        (1, 8, " quiet"),
        (1, 16, " quiet"),
    ]);
}

/// Boots each case's guests at once, as [`cold_boots_at_once_cost`] says: of its vCPUs,
/// as many at once, its kernel told the words past its console.
#[cfg(windows)]
#[allow(clippy::print_stderr, clippy::unwrap_used, clippy::indexing_slicing)]
fn boots_at_once(cases: &[(usize, usize, &str)]) {
    use std::io::{BufRead as _, BufReader};
    use std::process::Stdio;
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if cannot_run_vms() {
        return;
    }
    // The child of `cli` (the VM process), opened so that its times outlive it.
    let open_vm = |cli: u32| -> HANDLE {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            // SAFETY: a snapshot of every process, closed below.
            let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
            assert_ne!(snap, INVALID_HANDLE_VALUE);
            let mut entry = PROCESSENTRY32W {
                dwSize: size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut vm = None;
            // SAFETY: the snapshot, and an entry of its own size.
            let mut more = unsafe { Process32FirstW(snap, &raw mut entry) } != 0;
            while more {
                if entry.th32ParentProcessID == cli {
                    vm = Some(entry.th32ProcessID);
                }
                // SAFETY: as above.
                more = unsafe { Process32NextW(snap, &raw mut entry) } != 0;
            }
            // SAFETY: the snapshot taken above.
            unsafe { CloseHandle(snap) };
            if let Some(vm) = vm {
                // SAFETY: a process handle of the one right asked, closed by the caller.
                let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, vm) };
                if !h.is_null() {
                    return h;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("no VM process of {cli} in 10 s");
    };
    // Its CPU time in seconds, user and kernel; the handle closed.
    let times = |h: HANDLE| -> (f64, f64) {
        let mut t = [FILETIME::default(); 4];
        let [c, e, k, u] = &mut t;
        // SAFETY: the process, and four times to fill.
        let ok = unsafe { GetProcessTimes(h, c, e, k, u) };
        // SAFETY: the handle `open_vm` opened.
        unsafe { CloseHandle(h) };
        assert_ne!(ok, 0);
        let s =
            |t: &FILETIME| ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) as f64 / 1e7;
        (s(u), s(k))
    };
    // A console line's guest time, `[    1.529486] ...`.
    let guest_at = |line: &str| -> Option<f64> {
        let (at, _) = line.strip_prefix('[')?.split_once(']')?;
        at.trim().parse().ok()
    };
    let host = std::thread::available_parallelism().unwrap().get();
    let k = kernel();
    eprintln!("M376 host CPUs {host}");
    for &(vcpus, at_once, told) in cases {
        let t0 = Instant::now();
        let (tx, rx) = std::sync::mpsc::channel::<(usize, Option<(Duration, String)>)>();
        let mut vms: Vec<(std::process::Child, HANDLE)> = Vec::new();
        for i in 0..at_once {
            let mut child = common::command()
                .args(["run", "--kernel"])
                .arg(k)
                .args(["--cpus", &vcpus.to_string(), "--memory", "512"])
                .args(["--cmdline", &format!("console=ttyS0 earlycon panic=-1{told}")])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let out = child.stdout.take().unwrap();
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    let _ = tx.send((i, Some((t0.elapsed(), line))));
                }
                let _ = tx.send((i, None));
            });
            let h = open_vm(child.id());
            vms.push((child, h));
        }
        drop(tx);
        let mut lines: Vec<Vec<(Duration, String)>> = vec![Vec::new(); at_once];
        let mut ended = 0;
        let deadline = t0 + Duration::from_secs(240);
        while ended < at_once {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok((i, Some(line))) => lines[i].push(line),
                Ok((_, None)) => ended += 1,
                Err(_) => break,
            }
        }
        let mut summary = Vec::new();
        for (i, (mut child, h)) in vms.into_iter().enumerate() {
            let _ = child.kill();
            let _ = child.wait();
            let (user, kernel) = times(h);
            let said = &lines[i];
            let end = said.iter().find(|(_, l)| l.contains("Kernel panic"));
            let first = said.first().map(|(at, _)| at.as_secs_f64());
            let guest_end = said.iter().rev().find_map(|(_, l)| guest_at(l));
            let mut gaps: Vec<(f64, &str)> = said
                .windows(2)
                .filter_map(|w| Some((guest_at(&w[1].1)? - guest_at(&w[0].1)?, w[1].1.as_str())))
                .collect();
            gaps.sort_by(|a, b| b.0.total_cmp(&a.0));
            let gaps: Vec<String> = gaps
                .iter()
                .take(4)
                .map(|(g, l)| format!("+{g:.3}s {}", l.chars().take(110).collect::<String>()))
                .collect();
            let end_at = end.map(|(at, _)| at.as_secs_f64());
            eprintln!(
                "M376 vcpus={vcpus} at_once={at_once}{told} vm={i}: first {first:.2?} s, panic {end_at:.2?} s, guest {guest_end:.2?} s, cpu user {user:.2} s kernel {kernel:.2} s, {} lines; longest guest gaps: {gaps:#?}",
                said.len()
            );
            summary.push((end_at.unwrap_or(f64::INFINITY), user + kernel));
        }
        let mut ends: Vec<f64> = summary.iter().map(|s| s.0).collect();
        let mut cpus: Vec<f64> = summary.iter().map(|s| s.1).collect();
        ends.sort_by(f64::total_cmp);
        cpus.sort_by(f64::total_cmp);
        eprintln!(
            "M376 SUMMARY vcpus={vcpus} at_once={at_once}{told}: panic at p50 {:.2} s max {:.2} s; CPU a VM p50 {:.2} s max {:.2} s; wall {:.2} s",
            ends[ends.len() / 2],
            ends[ends.len() - 1],
            cpus[cpus.len() / 2],
            cpus[cpus.len() - 1],
            t0.elapsed().as_secs_f64()
        );
    }
}

/// Where a cold boot's CPU time goes in its VM process (PM M376): one guest of as many
/// vCPUs as the host has CPUs booted to its kernel's panic, left there (`panic=0`), and as
/// its panic is said each of the VM process's threads' CPU time, user and kernel, by its
/// name (`vcpu0`, `vcpu1`, ...), the largest first; plain and told `quiet`. A guest of 32
/// vCPUs used 7.7 s of CPU time in a boot of 1 s on jupiter, one of 1 vCPU 0.2 s.
#[cfg(windows)]
#[test]
#[ignore = "a measurement: budget-e2e.yml windows:boot, ignored:cold_boot_threads_cost"]
fn cold_boot_threads_cost() {
    use std::io::{BufRead as _, BufReader};
    use std::process::Stdio;

    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
        TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{
        GetThreadDescription, GetThreadTimes, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
    };

    if cannot_run_vms() {
        return;
    }
    let s = |t: &FILETIME| ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) as f64 / 1e7;
    let host = std::thread::available_parallelism().unwrap().get();
    for (vcpus, told) in [(host, ""), (host, " quiet"), (4.min(host), ""), (1, "")] {
        let mut child = common::command()
            .args(["run", "--kernel"])
            .arg(kernel())
            .args(["--cpus", &vcpus.to_string(), "--memory", "512"])
            .args(["--cmdline", &format!("console=ttyS0 earlycon panic=0{told}")])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        let start = std::time::Instant::now();
        let mut panicked = false;
        for line in out.lines().map_while(Result::ok) {
            if line.contains("Kernel panic") {
                panicked = true;
                break;
            }
        }
        let wall = start.elapsed();
        // The VM process: the child of the one started.
        // SAFETY: a snapshot of every process and thread, closed below.
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS | TH32CS_SNAPTHREAD, 0) };
        assert_ne!(snap, INVALID_HANDLE_VALUE);
        let mut p = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut vm = None;
        // SAFETY: the snapshot, and an entry of its own size.
        let mut more = unsafe { Process32FirstW(snap, &raw mut p) } != 0;
        while more {
            if p.th32ParentProcessID == child.id() {
                vm = Some(p.th32ProcessID);
            }
            // SAFETY: as above.
            more = unsafe { Process32NextW(snap, &raw mut p) } != 0;
        }
        let vm = vm.unwrap();
        let mut t = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut threads: Vec<(f64, f64, String)> = Vec::new();
        // SAFETY: as above.
        let mut more = unsafe { Thread32First(snap, &raw mut t) } != 0;
        while more {
            if t.th32OwnerProcessID == vm {
                // SAFETY: a thread handle of the one right asked, closed below.
                let h = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, t.th32ThreadID) };
                if !h.is_null() {
                    let mut f = [FILETIME::default(); 4];
                    let [c, e, k, u] = &mut f;
                    // SAFETY: the thread, and four times to fill.
                    let ok = unsafe { GetThreadTimes(h, c, e, k, u) } != 0;
                    let mut name = std::ptr::null_mut();
                    // SAFETY: the thread; its description, freed by LocalFree's owner.
                    let described = unsafe { GetThreadDescription(h, &raw mut name) } >= 0 && !name.is_null();
                    let mut said = String::new();
                    if described {
                        // SAFETY: a NUL-terminated wide string the call returned.
                        let len = (0..).take_while(|&i| unsafe { *name.add(i) } != 0).count();
                        // SAFETY: as above, `len` units long.
                        said = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(name, len) });
                        // SAFETY: the string GetThreadDescription allocated.
                        unsafe { windows_sys::Win32::Foundation::LocalFree(name.cast()) };
                    }
                    if ok {
                        threads.push((s(u), s(k), format!("{said}#{}", t.th32ThreadID)));
                    }
                    // SAFETY: the handle opened above.
                    unsafe { CloseHandle(h) };
                }
            }
            // SAFETY: as above.
            more = unsafe { Thread32Next(snap, &raw mut t) } != 0;
        }
        // SAFETY: the snapshot taken above.
        unsafe { CloseHandle(snap) };
        let _ = child.kill();
        let _ = child.wait();
        threads.sort_by(|a, b| (b.0 + b.1).total_cmp(&(a.0 + a.1)));
        let total: f64 = threads.iter().map(|t| t.0 + t.1).sum();
        let vcpu0 = threads
            .iter()
            .find(|t| t.2.starts_with("vcpu0#"))
            .map_or(0.0, |t| t.0 + t.1);
        let aps: f64 = threads
            .iter()
            .filter(|t| t.2.starts_with("vcpu") && !t.2.starts_with("vcpu0#"))
            .map(|t| t.0 + t.1)
            .sum();
        let top: Vec<String> = threads
            .iter()
            .take(6)
            .map(|(u, k, n)| format!("{n} user {u:.3} kernel {k:.3}"))
            .collect();
        eprintln!(
            "M376 THREADS vcpus={vcpus}{told}: panicked {panicked} at {:.2} s; {} threads, CPU {total:.2} s: vcpu0 {vcpu0:.2} s, the other vCPUs {aps:.2} s; most: {top:?}",
            wall.as_secs_f64(),
            threads.len()
        );
    }
}

/// Where a cold boot's CPUs spend their ticks as more boot at once (PM M376): guests of as
/// many vCPUs as the host has CPUs, one alone and then eight at once, each booting the test
/// guest with its kernel's profiler on (`profile=4`) and quiet, the test guest reporting
/// where its kernel's ticks went from boot to its start (`profile`), summed over the case's
/// guests by symbol, the most first. Guests of the host's vCPUs boot past linearly slower as
/// more boot at once, where guests of one vCPU do not: what their CPUs do meanwhile.
#[test]
#[ignore = "a measurement: budget-e2e.yml windows:boot, ignored:cold_boots_profiled"]
fn cold_boots_profiled() {
    use std::collections::HashMap;
    if cannot_run_vms() {
        return;
    }
    let (k, guest) = (kernel(), test_guest());
    let host = std::thread::available_parallelism().unwrap().get();
    // Each beside its kernel told to bring up its boot CPU alone (`maxcpus=1`), its test
    // guest bringing the rest online before it reports.
    for (vcpus, at_once, told) in [
        (host, 1, ""),
        (host, 8, ""),
        (host, 1, " maxcpus=1"),
        (host, 8, " maxcpus=1"),
        (1, 8, ""),
    ] {
        let started = std::time::Instant::now();
        let runs: Vec<_> = (0..at_once)
            .map(|_| {
                let (k, guest) = (k.to_path_buf(), guest.to_path_buf());
                std::thread::spawn(move || {
                    vm_run(
                        &[
                            "--kernel",
                            k.to_str().unwrap(),
                            "--init",
                            guest.to_str().unwrap(),
                            "--cpus",
                            &vcpus.to_string(),
                            "--memory",
                            "512",
                            "--cmdline",
                            &format!(
                                "console=ttyS0 panic=-1 quiet profile=4 initcall_debug log_buf_len=1M{told} shards_test=profile"
                            ),
                        ],
                        Duration::from_secs(240),
                    )
                })
            })
            .collect();
        let mut by: HashMap<String, u64> = HashMap::new();
        let mut calls: HashMap<String, (u64, u64)> = HashMap::new();
        let (mut total, mut uptimes, mut gaps) = (0u64, Vec::new(), Vec::new());
        let (mut offered, mut steps) = (std::collections::BTreeSet::new(), Vec::new());
        let mut onlined = Vec::new();
        for (i, run) in runs.into_iter().enumerate() {
            let r = run.join().unwrap();
            for line in r.stdout.lines() {
                if let Some(rest) = line.split_once("SHARDS-TEST INITCALL ").map(|(_, r)| r) {
                    if let Some((us, name)) = rest.split_once(' ') {
                        let us = us.parse::<u64>().unwrap_or(0);
                        let c = calls.entry(name.to_string()).or_default();
                        *c = (c.0 + us, c.1.max(us));
                    }
                } else if let Some(rest) = line.split_once("SHARDS-TEST ONLINE ").map(|(_, r)| r) {
                    onlined.push(rest.to_string());
                } else if let Some(rest) = line.split_once("SHARDS-TEST HV ").map(|(_, r)| r) {
                    offered.insert(rest.to_string());
                } else if let Some(rest) = line.split_once("SHARDS-TEST AT ").map(|(_, r)| r) {
                    steps.push((i, rest.to_string()));
                } else if let Some(rest) = line.split_once("SHARDS-TEST GAP ").map(|(_, r)| r) {
                    if gaps.iter().filter(|(vm, _)| *vm == i).count() < 3 {
                        gaps.push((i, rest.to_string()));
                    }
                } else if let Some(rest) = line.split_once("SHARDS-TEST PROFILE ").map(|(_, r)| r) {
                    if let Some((ticks, name)) = rest.split_once(' ') {
                        *by.entry(name.to_string()).or_default() += ticks.parse::<u64>().unwrap_or(0);
                    }
                } else if let Some(rest) = line.split_once("SHARDS-TEST PROFILE-TOTAL ").map(|(_, r)| r) {
                    let mut parts = rest.split_whitespace();
                    total += parts.next().and_then(|t| t.parse::<u64>().ok()).unwrap_or(0);
                    uptimes.push(parts.next().unwrap_or("?").to_string());
                }
            }
        }
        let mut ranked: Vec<(String, u64)> = by.into_iter().collect();
        ranked.sort_by_key(|r| std::cmp::Reverse(r.1));
        let top: Vec<String> = ranked
            .iter()
            .take(25)
            .map(|(name, ticks)| {
                format!(
                    "{ticks} ({:.1}%) {name}",
                    100.0 * *ticks as f64 / total.max(1) as f64
                )
            })
            .collect();
        let mut slowest: Vec<(String, (u64, u64))> = calls.into_iter().collect();
        slowest.sort_by_key(|c| std::cmp::Reverse(c.1.0));
        let slowest: Vec<String> = slowest
            .iter()
            .take(15)
            .map(|(name, (sum, max))| {
                format!(
                    "{:.1} ms a guest, at most {:.1}: {name}",
                    *sum as f64 / 1e3 / at_once as f64,
                    *max as f64 / 1e3
                )
            })
            .collect();
        eprintln!(
            "M376 PROFILE vcpus={vcpus} at_once={at_once}{told}: {:.2} s, CPUs brought online (count, us) {onlined:?}, {total} ticks, guests up {uptimes:?} s at their start: {top:#?}\nM376 INITCALLS vcpus={vcpus} at_once={at_once}{told}: {slowest:#?}\nM376 GAPS vcpus={vcpus} at_once={at_once}{told} (us, each guest's longest three): {gaps:#?}\nM376 STEPS vcpus={vcpus} at_once={at_once}{told} (us): {steps:#?}\nM376 HYPER-V vcpus={vcpus} at_once={at_once}{told}: {offered:#?}",
            started.elapsed().as_secs_f64()
        );
    }
}
