#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};
use std::time::Duration;

// The existing supplied-startup fixture's build/run guard (tests/build.rs, D54).
const TIMEOUT: Duration = Duration::from_secs(120);

#[test]
fn a_supplied_agent_runs_below_a_protected_native_reaper() {
    supplied_agent(None, Admission::ProtectedActor);
}

#[test]
fn a_scope_below_its_required_task_budget_is_refused() {
    supplied_agent(Some(1), Admission::Refused);
}

enum Admission {
    ProtectedActor,
    Refused,
}

fn supplied_agent(processes: Option<u64>, admission: Admission) {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("managed-life-home");
    let reports = TempDir::new("managed-life-report");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let ctx = TempDir::new("managed-life-context");
    std::fs::create_dir(ctx.join("agent")).unwrap();
    common::program_copy(common::test_guest(), &ctx.join("agent/probe"));
    std::fs::write(
        ctx.join("agent/agent.json"),
        br#"{"entrypoint":"probe","options":["managed-lifecycle","/probe/result"]}"#,
    )
    .unwrap();
    let process_limit = processes.map_or(String::new(), |n| format!("--processes={n} "));
    std::fs::write(
        ctx.join("Agentfile"),
        format!(
            "FROM {image}\nAGENT {process_limit}probe FROM ./agent\nVOLUME --chown=probe /probe FOR probe\n"
        ),
    )
    .unwrap();
    let built = run_shards_env(
        &["build", "image"],
        &["-t", "managed-life:1", ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    assert_eq!(built.status, Some(0), "{built}");
    let volume = format!("{}:/probe", reports.display());
    let ran = common::run_awaiting(
        &env,
        &[
            "run",
            "vm",
            "--rm",
            "--name",
            "managed-life",
            "-v",
            &volume,
            "managed-life:1",
            "await",
            "lifecycle-ready",
            "1",
        ],
        "managed-life",
        ("lifecycle-ready", 1),
        TIMEOUT,
    );
    if matches!(admission, Admission::Refused) {
        assert_eq!(ran.status, Some(125), "{ran}");
        assert!(
            ran.stderr.contains("starting agent probe")
                && ran
                    .stderr
                    .contains("Resource temporarily unavailable (os error 11)"),
            "{ran}"
        );
        assert!(
            !reports.join("result").exists(),
            "actor ran beyond its task budget"
        );
        return;
    }
    assert_eq!(ran.status, Some(0), "{ran}");
    let report = std::fs::read_to_string(reports.join("result")).unwrap();
    eprintln!("managed lifecycle facts:\n{report}");
    let field = |name: &str| {
        report
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap_or_else(|| panic!("missing {name}: {report}"))
            .parse::<u32>()
            .unwrap()
    };
    assert_ne!(field("actor_pid="), 1, "the actor is namespace PID1: {report}");
    assert_eq!(field("actor_ppid="), 1, "{report}");
    assert_eq!(field("namespace_init_owner="), 0, "{report}");
    assert_eq!(field("namespace_init_signal_errno="), 1, "{report}");
    for name in [
        "namespace_init_memory_errno=",
        "namespace_init_descriptors_errno=",
    ] {
        assert!(matches!(field(name), 1 | 13), "{report}");
    }
}
