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
//!   A template's save and its first two runs are not samples: the pool restored those
//!   VMs before the save's run had recorded the working set they would prefetch (PM M30).
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

#[cfg(unix)]
fn main() {
    use support::{report, rss_mib, run_env, stats, us, wall_us};

    const WARMUP: usize = 3;
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
            run_env(template_args, false, &env); // the pool's first two runs, restored
            run_env(template_args, false, &env); // before the working set existed
        }
        for _ in 0..runs / templates + usize::from(t < runs % templates) {
            cold.push(run_env(cold_args, false, &cold_env));
            if templated {
                template.push(run_env(template_args, false, &env));
            }
        }
    }
    run_env(&stop, false, &env);
    // Diagnostic (branch kvm-ws-diag): the home stays, for its daemon.log, and each
    // templated run's statistics, which its VM wrote to the client's stderr.
    for (i, s) in template.iter().enumerate() {
        for line in s
            .run
            .stderr
            .lines()
            .filter(|l| l.contains("kvm-stats") || l.contains("shards-timing"))
        {
            println!("run {i}: {line}");
        }
    }
    let vm_rss_mib = |samples: &[support::Sample]| -> Vec<f64> {
        us(samples, |r| r.rss_kib())
            .into_iter()
            .map(|kib| kib / 1024.0)
            .collect()
    };
    let mut rows = vec![
        stats("run_cold", "us", wall_us(&cold)),
        stats("run_cold_rss", "MiB", vm_rss_mib(&cold)),
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
            stats("run_template_rss", "MiB", vm_rss_mib(&template)),
        ]);
    }
    let clients: Vec<support::Sample> = cold.into_iter().chain(template).collect();
    rows.push(stats("client_rss", "MiB", rss_mib(&clients)));
    report(
        "image",
        &[("runs", runs.to_string()), ("templates", templates.to_string())],
        &rows,
    );
}
