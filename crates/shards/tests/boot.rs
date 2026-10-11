//! Boots real Linux guests on the host hypervisor, with each architecture's expectations
//! of what the guest finds (PSCI and GICv3 on arm64; ACPI and the IOAPIC on x86_64).

#![allow(clippy::panic)]

mod common;

use std::time::Duration;

use common::{KERNEL_NR_CPUS, cannot_run_vms, guest_init, kernel, run_shards, test_guest, vm_run};

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
    let refused = vm_run(
        &[
            "--kernel",
            kernel().to_str().unwrap(),
            "--memory-max",
            "1024",
            "--snapshot-dir",
            "nowhere",
        ],
        TIMEOUT,
    );
    assert_eq!(refused.status, Some(2), "{refused}");
    assert!(
        refused.stderr.contains("--memory-max with --snapshot-dir"),
        "{refused}"
    );
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
