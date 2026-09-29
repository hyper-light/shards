//! Image benchmark: `shards run IMAGE COMMAND` as a user runs it, the host's wall clock
//! around the client process, from its spawn to its exit. The image is the E2E tests'
//! registry image (tests/common, `served`), pulled once from a loopback registry; the
//! command is `exit 0`, so the test guest exits at once. Every run goes through the
//! daemon (docs/design/architecture.md D26), which the first run starts.
//!
//! - `run_cold`: with SHARDS_KERNEL and SHARDS_INIT set, the daemon boots a VM for every
//!   run.
//! - `run_template`: with the guest recorded (`shards guest use`), the daemon serves each
//!   run from its pool of warm VMs of the image's template (D25, D26). Only where this
//!   build can snapshot. Restores cost more for some templates than others, so samples
//!   come from `--templates T` of them (default 5), each saved afresh under a new daemon.
//!   A template's save and its first runs are not samples: the pool restored their VMs
//!   before the working set they would prefetch was recorded, by the save's run (HVF: two
//!   runs) or the first warm restore (KVM: three) (PM M30, M33).
//!   Its phases:
//!   - `template_command`: the command sent → its exit status read (the VM's clock): the
//!     command's run in the guest.
//!   - `template_outside`: the rest of the wall clock: the client launched, the request
//!     handed to a warm VM, the status back, the client gone.
//! - `*_rss`: the VM process's peak RSS when it answered, including the guest memory it
//!   touched. `client_rss`: the client process's.
//!
//! Cold and templated samples alternate, after three cold warm-up runs. Between runs the
//! daemon refills its pool, as it would between a user's runs.
//!
//! `cargo bench -p shards --bench image [-- --runs N --templates T]`

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout
)]

#[cfg(unix)]
#[path = "../tests/common/mod.rs"]
mod common;
#[cfg(unix)]
mod support;

#[cfg(not(unix))]
fn main() {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "SKIP: the image benchmark needs a Unix host");
}

/// Diagnostic (branch pool-diag): every KVM event, and the vCPU and vsock threads'
/// wakeups and switches, on CLOCK_MONOTONIC.
#[cfg(unix)]
const TRACE_ON: &str = r#"cd /sys/kernel/tracing && echo 0 > tracing_on && echo > trace && echo mono > trace_clock && echo 32768 > buffer_size_kb && echo 'comm ~ "vcpu*" || comm == "virtio-vsock"' > events/sched/sched_wakeup/filter && echo 'prev_comm ~ "vcpu*" || next_comm ~ "vcpu*" || prev_comm == "virtio-vsock" || next_comm == "virtio-vsock"' > events/sched/sched_switch/filter && echo 1 > events/sched/sched_wakeup/enable && echo 1 > events/sched/sched_switch/enable && echo 1 > events/kvm/enable && echo 1 > tracing_on"#;
#[cfg(unix)]
const TRACE_DUMP: &str = r#"cd /sys/kernel/tracing && echo 0 > tracing_on && grep -v '^#' trace; echo 0 > events/enable; echo > trace"#;

#[cfg(unix)]
fn main() {
    use support::{peak_rss_mib, report, run_env, stats, us, wall_us};

    const WARMUP: usize = 3;
    // A template's runs whose VMs its pool restored before the working set existed: the
    // pool's first two, and where a warm restore records it, the one restored as the
    // recording run took its VM.
    const UNPREFETCHED: usize = if shards_vmm::vm::RESTORES_RECORD { 3 } else { 2 };
    let runs: usize = support::option("--runs").map_or(50, |v| v.parse().expect("--runs N"));
    let templates: usize = support::option("--templates")
        .map_or(5, |v| v.parse().expect("--templates T"))
        .max(1);
    if common::cannot_run_vms() {
        return;
    }
    let home = common::workspace().join(format!("target/bench/image-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let env = [("SHARDS_HOME", home.as_os_str())];
    let strings = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let (image, _) = common::served();
    let (kernel, init) = (
        common::kernel().to_str().unwrap(),
        common::guest_init().to_str().unwrap(),
    );
    // Pulled once; every measured run finds the image stored.
    run_env(&strings(&["pull", "-q", &image]), false, &env);
    run_env(
        &strings(&["guest", "use", "--kernel", kernel, "--init", init]),
        false,
        &env,
    );
    let run_args = strings(&["run", "--pull", "never", &image, "exit", "0"]);
    let (cold_args, template_args) = (&run_args, &run_args);
    let cold_env: Vec<(&str, &std::ffi::OsStr)> = env
        .iter()
        .copied()
        .chain([("SHARDS_KERNEL", kernel.as_ref()), ("SHARDS_INIT", init.as_ref())])
        .collect();
    for _ in 0..WARMUP {
        run_env(cold_args, false, &cold_env);
    }
    let templated = shards_vmm::vm::SNAPSHOTS;
    let stop = strings(&["daemon", "stop"]);
    let (mut cold, mut template) = (Vec::with_capacity(runs), Vec::with_capacity(runs));
    for t in 0..templates {
        if templated {
            // A new daemon: its pool holds no VM of the last template.
            run_env(&stop, false, &env);
            let _ = std::fs::remove_dir_all(home.join("templates"));
            run_env(template_args, false, &env); // saves the template
            // The runs whose VMs were restored before the working set existed.
            for _ in 0..UNPREFETCHED {
                run_env(template_args, false, &env);
            }
        }
        for i in 0..runs / templates + usize::from(t < runs % templates) {
            cold.push(run_env(cold_args, false, &cold_env));
            if templated {
                // Diagnostic (branch pool-diag): KVM's events and the scheduling of vCPU
                // and vsock threads through a templated run, raw, for its first runs.
                let traced = t == 0 && i < 4;
                if traced {
                    let _ = std::process::Command::new("sudo")
                        .args(["sh", "-c", TRACE_ON])
                        .status();
                }
                let sample = run_env(template_args, false, &env);
                if traced {
                    let dump = std::process::Command::new("sudo")
                        .args(["sh", "-c", TRACE_DUMP])
                        .output()
                        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                        .unwrap_or_default();
                    println!(
                        "trace-run {i} wall {:.0} us",
                        sample.run.elapsed.as_secs_f64() * 1e6
                    );
                    for line in sample
                        .run
                        .stderr
                        .lines()
                        .filter(|l| l.starts_with("shards-timing"))
                    {
                        println!("trace-timing {line}");
                    }
                    for line in dump.lines() {
                        println!("trace| {line}");
                    }
                }
                template.push(sample);
            }
        }
    }
    run_env(&stop, false, &env);
    let _ = std::fs::remove_dir_all(&home);
    let mut rows = vec![
        stats("run_cold", "us", wall_us(&cold)),
        stats("run_cold_rss", "MiB", peak_rss_mib(&cold)),
    ];
    if templated {
        let command = |r: &common::Run| r.answered_us()?.checked_sub(r.request_us()?);
        let outside: Vec<f64> = template
            .iter()
            .map(|s| s.run.elapsed.as_secs_f64() * 1e6 - command(&s.run).expect("timing field") as f64)
            .collect();
        rows.extend([
            stats("run_template", "us", wall_us(&template)),
            stats("template_command", "us", us(&template, command)),
            stats("template_outside", "us", outside),
            stats("run_template_rss", "MiB", peak_rss_mib(&template)),
        ]);
    }
    let clients: Vec<support::Sample> = cold.into_iter().chain(template).collect();
    // The client's own peak, which it adds to the VM's timing line.
    let client_mib = us(&clients, |r| r.client_rss_kib())
        .into_iter()
        .map(|kib| kib / 1024.0)
        .collect();
    rows.push(stats("client_rss", "MiB", client_mib));
    report(
        "image",
        &[("runs", runs.to_string()), ("templates", templates.to_string())],
        &rows,
    );
}
