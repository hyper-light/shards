#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};
use std::time::Duration;

// The existing supplied-startup fixture's build/run guard (tests/build.rs, D54).
const TIMEOUT: Duration = Duration::from_secs(120);

/// Coordinator source-fault campaign; the committed production build has no fault switch.
#[test]
#[ignore = "requires the retained tenant-events-directory source patch"]
fn a_tenant_observation_fault_leaves_a_mixed_sibling_and_workload_alive() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("tenant-fault-home");
    let reports = TempDir::new("tenant-fault-report");
    let faulty = TempDir::new("tenant-fault-bad");
    let ctx = TempDir::new("tenant-fault-context");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    for (kind, directory, mode) in [
        ("agent", "faulty", "managed-lifecycle-exit"),
        ("harness", "healthy", "managed-lifecycle-challenge"),
    ] {
        std::fs::create_dir(ctx.join(directory)).unwrap();
        common::program_copy(common::test_guest(), &ctx.join(directory).join("probe"));
        let path = if directory == "faulty" {
            "/bad/result"
        } else {
            "/probe"
        };
        std::fs::write(
            ctx.join(directory).join(format!("{kind}.json")),
            format!(r#"{{"entrypoint":"probe","options":["{mode}","{path}"]}}"#),
        )
        .unwrap();
    }
    std::fs::write(ctx.join("Agentfile"), format!(
        "FROM {image}\nAGENT faulty FROM ./faulty\nHARNESS healthy FROM ./healthy\nVOLUME --chown=faulty /bad FOR faulty\nVOLUME --chown=healthy /probe FOR healthy\n"
    )).unwrap();
    let built = run_shards_env(
        &["build", "image"],
        &["-t", "tenant-fault:1", ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    assert_eq!(built.status, Some(0), "{built}");
    let report_bind = format!("{}:/probe", reports.display());
    let faulty_bind = format!("{}:/bad", faulty.display());
    let env_text = [
        ("SHARDS_HOME", home.to_str().unwrap()),
        ("SHARDS_KERNEL", kernel().to_str().unwrap()),
        ("SHARDS_INIT", guest_init().to_str().unwrap()),
    ];
    let mut vm = common::Vm::spawn_env(
        &[
            "run",
            "vm",
            "--rm",
            "--name",
            "tenant-fault",
            "-v",
            &report_bind,
            "-v",
            &faulty_bind,
            "tenant-fault:1",
            "sleep",
        ],
        &env_text,
    );
    let observed = fault_observed(&home);
    eprintln!("tenant fault observation: {observed}");
    let after_fault = run_shards_env(
        &["exec", "vm"],
        &["tenant-fault", "/bin/testguest", "report"],
        &env,
        TIMEOUT,
    );
    eprintln!("main namespace execution after fault: {after_fault}");
    assert_eq!(
        after_fault.status,
        Some(0),
        "tenant fault ended sibling execution: {after_fault}"
    );
    assert!(
        observed.contains("agent faulty: quarantined after population observation errno 21"),
        "{observed}"
    );
    let challenge = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .to_be_bytes();
    std::fs::write(reports.join("go.pending"), challenge).unwrap();
    std::fs::rename(reports.join("go.pending"), reports.join("go")).unwrap();
    vm.wait_for("tenant challenge answered", TIMEOUT);
    assert_eq!(std::fs::read(reports.join("answer")).unwrap(), challenge);
    assert!(faulty.join("result").exists(), "faulty actor never ran");
    let top = run_shards_env(&["top"], &["tenant-fault", "-o", "comm"], &env, TIMEOUT);
    assert_eq!(top.status, Some(0), "{top}");
    assert!(
        top.stdout.contains("testguest") && top.stdout.contains("lifecycle-ready"),
        "{top}"
    );
    eprintln!(
        "post-quarantine challenge answered; remaining processes:\n{}",
        top.stdout
    );
    let stopped = run_shards_env(&["stop", "vm"], &["-t", "0", "tenant-fault"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{stopped}");
    vm.wait_exit_following(TIMEOUT);
}

fn fault_observed(home: &std::path::Path) -> String {
    use std::io::Read as _;
    let directory = shards_vmm::platform::open_dir(home).unwrap();
    let mut changed = shards_vmm::platform::FileWatch::new(&directory).unwrap();
    let mut log = None;
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut observed = String::new();
    loop {
        changed.clear();
        if log.is_none() {
            match std::fs::File::open(home.join("daemon.log")) {
                Ok(file) => {
                    changed.file(&file).unwrap();
                    log = Some(file);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("opening the console log: {error}"),
            }
        }
        if let Some(log) = &mut log {
            log.read_to_string(&mut observed).unwrap();
        }
        let complete = observed.rsplit_once('\n').map_or("", |(complete, _)| complete);
        if let Some(line) = complete.lines().find(|line| {
            line.contains("quarantined after population observation") || line.contains("shared boundary:")
        }) {
            return line.to_string();
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !left.is_zero(),
            "no tenant fault observation in daemon console log:\n{observed}"
        );
        #[cfg(unix)]
        shards_ipc::await_any_within(&[changed.fd()], Some(left)).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
            use windows_sys::Win32::System::Threading::WaitForSingleObject;
            let millis = u32::try_from(left.as_millis().max(1)).unwrap();
            // SAFETY: the borrowed event stays owned by this live FileWatch.
            let status = unsafe { WaitForSingleObject(changed.fd().as_raw_handle(), millis) };
            assert!(matches!(status, WAIT_OBJECT_0 | WAIT_TIMEOUT));
        }
    }
}

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
