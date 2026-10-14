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

/// A PCI function's interrupts reach the guest (D154): the test function, taken by vfio-pci
/// in the guest's own userspace (a no-IOMMU group), raises INTx twice, acknowledged and
/// unmasked between, then one MSI, then MSI-X vectors three times, two of them held
/// pending while masked and sent as unmasked, each signalled on an eventfd. On arm64, INTx
/// on an SPI of the device's own and the MSIs through the GIC's GICv2m frame; on x86_64,
/// INTx on the bus's GSI as the DSDT's _PRT says, and the MSIs to the local APICs.
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
            "console=ttyS0 panic=-1 vfio.enable_unsafe_noiommu_mode=1 vfio_iommu_type1.allow_unsafe_interrupts=1 shards_test=pci-irq",
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

/// A guest's GICv2m frame isolates no MSIs, and its IOMMU remaps no interrupts, so VFIO's
/// type 1 takes a group of it only where told that a function's DMA may reach the frame
/// (vfio_iommu_type1.c, `allow_unsafe_interrupts`): the tests' guests are told so.
///
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
            "console=ttyS0 panic=-1 vfio_iommu_type1.allow_unsafe_interrupts=1 shards_test=pci-dma",
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
/// VM process's container maps as the inner IOMMU does. An x86_64 host gives no guest
/// virtualization of its own yet.
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
/// vfio-pci, and a run is given their group, whose microVM shards' daemon boots with them
/// behind the microVM's own IOMMU, one device there, which its guest takes as its own group
/// 0. The run's workload, given the group's node where its guest's sysfs puts the group,
/// takes each function through VFIO as DPDK finds one, by its address's group in sysfs,
/// and moves a page through each by DMA, one container's mappings for both. An x86_64
/// host gives no guest virtualization of its own yet.
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
    // The outer guest is a host whose IOMMU isolates no MSIs (GICv2m's), whose VFIO its
    // administrator allows them.
    let ran = outer(
        &initrd,
        "shards_test=daemon shards_l1_vfio=0000:00:02.0,0000:00:02.1 shards_l2=vfio-dma,0000:00:01.0,0000:00:01.1 vfio_iommu_type1.allow_unsafe_interrupts=1 shards_l1_log=debug",
    );
    let lines = |want: &str| ran.stdout.lines().filter(|l| l.trim_end() == want).count();
    assert_eq!(
        lines(&format!("L2: {DMA_THROUGH_IOMMU}")),
        2,
        "the run's DMA: {ran}"
    );
    assert_eq!(lines("SHARDS-TEST PASS"), 1, "the outer guest: {ran}");
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

/// Memory plugged as the guest asks for it (`--memory-max`, D133). A guest booted with 256
/// MiB asks for 1 GiB more, its kernel brings it all online, and the guest fills seven
/// eighths of it, which the VM process then holds; it frees it and asks for none, its kernel
/// gives it all back, the VM process's own memory falls by it, and the VM process holds
/// none of the pages the guest wrote as it ends (`hotplug_held_kib`: on macOS a page out of
/// the hypervisor's mappings is out of the footprint, held or not, PM M193). Asked for more
/// than it may have, it is given what it may. A snapshot of such a guest is refused up
/// front. On the hosts whose VMs it measures the memory of (`process_usage`): Windows runs
/// none yet.
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
