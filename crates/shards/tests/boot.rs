//! Boots real Linux guests on the host hypervisor, with each architecture's expectations
//! of what the guest finds (PSCI and GICv3 on arm64; ACPI and the IOAPIC on x86_64).

#![allow(clippy::panic)]

mod common;

use std::time::Duration;

use common::{KERNEL_NR_CPUS, cannot_run_vms, guest_init, kernel, run_shards, vm_run};

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

#[test]
fn boots_the_hosts_maximum_vcpu_count() {
    if cannot_run_vms() {
        return;
    }
    let max = shards_vmm::vm::max_vcpus().unwrap();
    let k = kernel().to_str().unwrap();
    let cpus = max.to_string();
    let r = vm_run(
        &[
            "--kernel",
            k,
            "--cpus",
            &cpus,
            "--memory",
            "1024",
            "--cmdline",
            "console=ttyS0 panic=-1",
        ],
        TIMEOUT,
    );
    // The guest runs as many as its kernel was built for. It must have been shown the
    // rest: the kernel names the vCPUs it refused.
    let up = max.min(KERNEL_NR_CPUS);
    let plural = if up == 1 { "" } else { "s" };
    assert!(
        r.stdout
            .contains(&format!("smp: Brought up 1 node, {up} CPU{plural}")),
        "{r}"
    );
    if max > KERNEL_NR_CPUS {
        let refused = match common::ARCH {
            // arch/x86/kernel/cpu/topology.c
            "x86_64" => format!("CPU topo: Rejected CPUs {}", max - KERNEL_NR_CPUS),
            // arch/arm64/kernel/smp.c
            _ => format!("Number of cores ({max}) exceeds configured maximum of {KERNEL_NR_CPUS}"),
        };
        assert!(r.stdout.contains(&refused), "{r}");
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
        (&["--kernel", "/nonexistent/kernel"], "/nonexistent/kernel"),
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
        ("linux", "x86_64") => Some("kvm"),
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
