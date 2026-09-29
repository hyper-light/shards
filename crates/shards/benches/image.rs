//! Image benchmark: `shards run IMAGE COMMAND` as a user runs it, the host's wall clock
//! around the whole process. The image is the E2E tests' registry image (tests/common,
//! `test_image`), pulled once from a loopback registry; the command is `exit 0`, so the
//! test guest exits at once.
//!
//! - `run_cold`: with `--kernel` and `--init` named, every run boots the image.
//! - `run_template`: with the guest recorded (`shards guest use`), the first run saves a
//!   template of the booted, mounted image, and each later run restores it
//!   (docs/design/architecture.md D25). Only where this build can snapshot. Restores cost
//!   more for some templates than others, so samples come from `--templates T` of them
//!   (default 5), each saved afresh. A template's save and its first restore, the first
//!   process to map its just-written memory, are not samples. Its phases:
//!   - `template_restore`: shards' `main` → the restored vCPUs released (the VMM's clock):
//!     the image looked up in the store, the template named and restored.
//!   - `template_command`: → the VM stopped: the command sent, run and answered.
//!   - `template_process`: the rest of the wall clock, outside `main`: the process
//!     launched (exec, dyld, frameworks) and torn down.
//!
//! Peak RSS includes the guest memory the process touched.
//!
//! Cold and templated samples alternate, after three cold warm-up runs.
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
    let cold_args = strings(&[
        "run", "--kernel", kernel, "--init", init, "--pull", "never", &image, "exit", "0",
    ]);
    let template_args = strings(&["run", "--pull", "never", &image, "exit", "0"]);
    for _ in 0..WARMUP {
        run_env(&cold_args, false, &env);
    }
    let templated = shards_vmm::vm::SNAPSHOTS;
    let (mut cold, mut template) = (Vec::with_capacity(runs), Vec::with_capacity(runs));
    for t in 0..templates {
        if templated {
            let _ = std::fs::remove_dir_all(home.join("templates"));
            run_env(&template_args, false, &env); // saves the template
            run_env(&template_args, false, &env); // its first restore
        }
        for _ in 0..runs / templates + usize::from(t < runs % templates) {
            cold.push(run_env(&cold_args, false, &env));
            if templated {
                template.push(run_env(&template_args, false, &env));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&home);
    let mut rows = vec![
        stats("run_cold", "us", wall_us(&cold)),
        stats("run_cold_rss", "MiB", rss_mib(&cold)),
    ];
    if templated {
        rows.extend([
            stats("run_template", "us", wall_us(&template)),
            stats("template_restore", "us", us(&template, |r| r.released_us())),
            stats(
                "template_command",
                "us",
                us(&template, |r| r.exit_us()?.checked_sub(r.released_us()?)),
            ),
            stats(
                "template_process",
                "us",
                us(&template, |r| r.elapsed.as_micros().checked_sub(r.exit_us()?)),
            ),
            stats("run_template_rss", "MiB", rss_mib(&template)),
        ]);
    }
    report(
        "image",
        &[("runs", runs.to_string()), ("templates", templates.to_string())],
        &rows,
    );
}
