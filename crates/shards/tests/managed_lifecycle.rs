#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use common::{Following, Patience, TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};

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
    let port = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
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
        let socket = if directory == "healthy" {
            format!(r#","{port}""#)
        } else {
            String::new()
        };
        std::fs::write(
            ctx.join(directory).join(format!("{kind}.json")),
            format!(r#"{{"entrypoint":"probe","options":["{mode}","{path}"{socket}]}}"#),
        )
        .unwrap();
    }
    std::fs::write(ctx.join("Agentfile"), format!(
        "FROM {image}\nAGENT faulty FROM ./faulty\nHARNESS healthy FROM ./healthy\nVOLUME --chown=faulty /bad FOR faulty\nVOLUME --chown=healthy /probe FOR healthy\nNETWORK --ingress={port} challenge\nEXPOSE --harnesses=healthy {port} AS ingress FOR challenge\nCONNECT healthy WITH healthy ON challenge\n"
    )).unwrap();
    let built = run_shards_env(
        &["build", "image"],
        &["-t", "tenant-fault:1", ctx.to_str().unwrap()],
        &env,
    );
    assert_eq!(built.status, Some(0), "{built}");
    let report_bind = format!("{}:/probe", reports.display());
    let faulty_bind = format!("{}:/bad", faulty.display());
    let publish = format!("127.0.0.1::{port}");
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
            "-p",
            &publish,
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
    challenge_healthy(&env, &home, &reports, port);
    let facts = std::fs::read_to_string(faulty.join("result")).expect("faulty actor never ran");
    eprintln!("post-quarantine exact challenge answered; exiting actor facts:\n{facts}");
    let top = run_shards_env(&["top"], &["tenant-fault", "-o", "pid,comm"], &env);
    assert_eq!(top.status, Some(0), "{top}");
    assert!(
        top.stdout.contains("testguest") && top.stdout.contains("lifecycle-ready"),
        "{top}"
    );
    eprintln!(
        "post-quarantine challenge answered; remaining processes:\n{}",
        top.stdout
    );
    let stopped = run_shards_env(&["stop", "vm"], &["-t", "0", "tenant-fault"], &env);
    assert_eq!(stopped.status, Some(0), "{stopped}");
    vm.wait_exit_following();
}

#[test]
fn repeated_orphan_crashes_are_reaped_while_a_mixed_neighbor_progresses() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("reaping-home");
    let reports = TempDir::new("reaping-report");
    let healthy = TempDir::new("reaping-healthy");
    let ctx = TempDir::new("reaping-context");
    let port = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let rounds = 20;
    for (kind, name, options) in [
        (
            "agent",
            "reaper",
            serde_json::json!(["managed-reap", "/probe/result", rounds.to_string()]),
        ),
        (
            "harness",
            "healthy",
            serde_json::json!(["managed-lifecycle-challenge", "/healthy", port.to_string()]),
        ),
    ] {
        std::fs::create_dir(ctx.join(name)).unwrap();
        common::program_copy(common::test_guest(), &ctx.join(name).join("probe"));
        let manifest = serde_json::json!({"entrypoint": "probe", "options": options});
        std::fs::write(ctx.join(name).join(format!("{kind}.json")), manifest.to_string()).unwrap();
    }
    std::fs::write(ctx.join("Agentfile"), format!(
        "FROM {image}\nAGENT reaper FROM ./reaper\nHARNESS healthy FROM ./healthy\nVOLUME --chown=reaper /probe FOR reaper\nVOLUME --chown=healthy /healthy FOR healthy\nNETWORK --ingress={port} challenge\nEXPOSE --harnesses=healthy {port} AS ingress FOR challenge\nCONNECT healthy WITH healthy ON challenge\n"
    )).unwrap();
    let built = run_shards_env(
        &["build", "image"],
        &["-t", "reaping:1", ctx.to_str().unwrap()],
        &env,
    );
    assert_eq!(built.status, Some(0), "{built}");
    let report_bind = format!("{}:/probe", reports.display());
    let healthy_bind = format!("{}:/healthy", healthy.display());
    let publish = format!("127.0.0.1::{port}");
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
            &healthy_bind,
            "-p",
            &publish,
            "reaping:1",
            "sleep",
        ],
        &env_text,
    );
    assert_eq!(published(&home, &reports, "result.started"), b"started");
    let report = String::from_utf8(published(&home, &reports, "result")).unwrap();
    eprintln!("actual orphan-reaping results:\n{report}");
    assert!(!report.contains("failure"), "{report}");
    assert_eq!(report.lines().count(), rounds, "{report}");
    for (round, line) in report.lines().enumerate() {
        assert!(line.starts_with(&format!("round {round} pid ")), "{report}");
        assert!(line.contains(" adopted 1 reaped true fds "), "{report}");
    }
    let after = run_shards_env(
        &["exec", "vm"],
        &["tenant-fault", "/bin/testguest", "report"],
        &env,
    );
    assert_eq!(after.status, Some(0), "{after}");
    challenge_healthy(&env, &home, &healthy, port);
    let stopped = run_shards_env(&["stop", "vm"], &["-t", "0", "tenant-fault"], &env);
    assert_eq!(stopped.status, Some(0), "{stopped}");
    vm.wait_exit_following();
}

/// The file `name` in `directory` once a guest of `home`'s daemon has published it, waited
/// for while that daemon and its microVMs go on ([`Patience`], its kind its call site): a
/// refusal says what the guest's report had said by then (`name` with `.pending`).
#[track_caller]
fn published(home: &std::path::Path, directory: &std::path::Path, name: &str) -> Vec<u8> {
    let root = shards_vmm::platform::open_dir(directory).unwrap();
    let changed = shards_vmm::platform::FileWatch::new(&root).unwrap();
    let mut patience = Patience::of(home);
    loop {
        changed.clear();
        match std::fs::read(directory.join(name)) {
            Ok(bytes) => {
                patience.done();
                return bytes;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("reading {name}: {error}"),
        }
        assert!(
            patience.holds(),
            "no publication of {name}; its report said {:?}",
            std::fs::read_to_string(directory.join(format!("{name}.pending")))
        );
        wait_changed(&changed, patience.next_look());
    }
}

#[track_caller]
fn fault_observed(home: &std::path::Path) -> String {
    use std::io::Read as _;
    let directory = shards_vmm::platform::open_dir(home).unwrap();
    let mut changed = shards_vmm::platform::FileWatch::new(&directory).unwrap();
    let mut log = None;
    let mut patience = Patience::of(home);
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
            line.contains("quarantined after population observation")
                || line.contains("shared boundary:")
                || line.contains("reboot: Power down")
        }) {
            patience.done();
            return line.to_string();
        }
        assert!(
            patience.holds(),
            "no tenant fault observation in daemon console log:\n{observed}"
        );
        wait_changed(&changed, patience.next_look());
    }
}

/// Waits for a change `changed` sees, at most `left`.
fn wait_changed(changed: &shards_vmm::platform::FileWatch, left: std::time::Duration) {
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

#[track_caller]
fn supplied_agent(processes: Option<u64>, admission: Admission) {
    use std::io::Write as _;
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
    let _ = writeln!(std::io::stderr(), "managed lifecycle facts:\n{report}");
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

/// The healthy harness's echo of a challenge through the port its run publishes, its answer
/// read as a stream `home`'s daemon carries to its guest ([`Following::of_home`]).
#[track_caller]
fn challenge_healthy(
    env: &[(&str, &std::ffi::OsStr)],
    home: &std::path::Path,
    reports: &std::path::Path,
    port: u16,
) {
    use std::io::{Read as _, Write as _};
    let challenge = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .to_be_bytes();
    assert_eq!(published(home, reports, "ready"), b"ready");
    let ports = run_shards_env(&["read", "ports"], &["tenant-fault"], env);
    assert_eq!(ports.status, Some(0), "{ports}");
    let host_port: u16 = ports
        .stdout
        .lines()
        .find(|line| line.starts_with(&format!("{port}/tcp")))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|address| address.rsplit(':').next())
        .and_then(|port| port.parse().ok())
        .unwrap_or_else(|| panic!("no published challenge port: {ports}"));
    let address = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, host_port));
    let mut socket = std::net::TcpStream::connect(address).unwrap();
    socket.write_all(&challenge).unwrap();
    socket.shutdown(std::net::Shutdown::Write).unwrap();
    let mut reply = [0u8; size_of::<u128>()];
    let mut answer = Following::of_home(&socket, home);
    answer.read_exact(&mut reply).unwrap();
    answer.answered();
    assert_eq!(reply, challenge);
    assert_eq!(std::fs::read(reports.join("answer")).unwrap(), challenge);
}
