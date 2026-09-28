//! Boots real Linux guests on the host hypervisor.

mod common;

use std::time::Duration;

use common::{guest_init, kernel, run_shards, vm_run};

const TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_RESET: i32 = 3;

#[test]
fn boots_to_root_mount_without_a_root_device() {
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
    assert!(r.stdout.contains("Machine model: linux,dummy-virt"), "{r}");
    assert!(r.stdout.contains("psci: PSCIv1.1 detected in firmware"), "{r}");
    assert!(
        r.stdout
            .contains("GICv3: CPU0: found redistributor 0 region 0:0x00000000080a0000"),
        "{r}"
    );
    assert!(
        r.stdout
            .contains("arch_timer: cp15 timer running at 24.00MHz (virt)"),
        "{r}"
    );
    assert!(
        r.stdout
            .contains("Kernel panic - not syncing: VFS: Unable to mount root fs"),
        "{r}"
    );
}

#[test]
fn brings_up_every_vcpu_through_psci() {
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
    let max = shards_vmm::hvf::max_vcpus().unwrap();
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
    assert!(
        r.stdout.contains(&format!("smp: Brought up 1 node, {max} CPUs")),
        "{r}"
    );
}

#[test]
fn runs_init_as_pid_1_and_powers_off() {
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

#[test]
fn rejects_invalid_configuration() {
    let (k, init) = (kernel().to_str().unwrap(), guest_init().to_str().unwrap());
    let cases: &[(&[&str], &str)] = &[
        (&["--kernel", k, "--cpus", "0"], "at least one vCPU"),
        (&["--kernel", k, "--cpus", "100000"], "vCPUs requested"),
        (&["--kernel", k, "--memory", "63"], "even number of MiB"),
        (&["--kernel", k, "--memory", "65"], "even number of MiB"),
        (&["--kernel", "/nonexistent/kernel"], "/nonexistent/kernel"),
        (&["--kernel", init], "not an arm64 Image"),
        (
            &["--kernel", k, "--init", init, "--initrd", init],
            "mutually exclusive",
        ),
        (&["--cpus", "1"], "--kernel is required"),
        (&["--kernel", k, "--bogus"], "unknown argument"),
    ];
    for (args, message) in cases {
        let r = vm_run(args, TIMEOUT);
        assert_ne!(r.status, Some(0), "{args:?} should fail: {r}");
        assert!(r.stderr.contains(message), "{args:?}: expected {message:?}: {r}");
    }
}

#[test]
fn cli_reports_usage_and_rejects_unknown_commands() {
    let none: [&str; 0] = [];
    let help = run_shards(&["--help"], &none, TIMEOUT);
    assert_eq!(help.status, Some(0), "{help}");
    assert!(help.stdout.contains("vm run"), "{help}");
    let unknown = run_shards(&["frobnicate"], &none, TIMEOUT);
    assert_eq!(unknown.status, Some(2), "{unknown}");
    assert!(unknown.stderr.contains("unknown command"), "{unknown}");
    let version = run_shards(&["version"], &none, TIMEOUT);
    assert!(version.stdout.starts_with("shards "), "{version}");
}
