#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use common::{TempDir, run_shards_env};
use shards_testguest::limits::{Budget, Cause, Command, Phase, Report};
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::time::{Duration, Instant};

fn input(name: &str) -> u32 {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("set explicit fixture input {name}"))
        .parse()
        .unwrap_or_else(|_| panic!("{name} must be a positive u32"))
}

fn publish(directory: &Path, name: &str, bytes: &[u8]) {
    let pending = directory.join(format!("{name}.pending"));
    let mut file = std::fs::File::create(&pending).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
    std::fs::rename(pending, directory.join(name)).unwrap();
}

fn published(directory: &Path, name: &str, until: Instant) -> Vec<u8> {
    let root = shards_vmm::platform::open_dir(directory).unwrap();
    let changed = shards_vmm::platform::FileWatch::new(&root).unwrap();
    loop {
        changed.clear();
        if let Ok(bytes) = std::fs::read(directory.join("failure")) {
            panic!("guest fixture failure: {:?}", Report::decode(&bytes));
        }
        match std::fs::read(directory.join(name)) {
            Ok(bytes) => return bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("reading {name}: {e}"),
        }
        let left = until.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "no {name} before caller deadline");
        #[cfg(unix)]
        shards_ipc::await_any_within(&[changed.fd()], Some(left)).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
            use windows_sys::Win32::System::Threading::WaitForSingleObject;
            let ms = u32::try_from(left.as_millis().max(1)).unwrap();
            // SAFETY: the FileWatch owns this live event throughout the wait.
            let status = unsafe { WaitForSingleObject(changed.fd().as_raw_handle(), ms) };
            assert!(matches!(status, WAIT_OBJECT_0 | WAIT_TIMEOUT));
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Process {
    pid: u32,
    parent: u32,
    uid: u32,
    threads: u32,
}

fn process_rows(text: &str) -> Result<Vec<Process>, &'static str> {
    text.lines()
        .skip(1)
        .map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let [pid, parent, uid, threads] = fields.as_slice() else {
                return Err("invalid process row");
            };
            let number = |s: &str| s.parse().map_err(|_| "invalid process number");
            Ok(Process {
                pid: number(pid)?,
                parent: number(parent)?,
                uid: number(uid)?,
                threads: number(threads)?,
            })
        })
        .collect()
}

fn tree(rows: &[Process], expected_reserved: u32, held: u32) -> u32 {
    let actors = rows
        .iter()
        .filter(|p| p.uid == shards_abi::DOMAIN_FIRST_ID)
        .collect::<Vec<_>>();
    let actor = actors
        .iter()
        .find(|actor| rows.iter().any(|p| p.pid == actor.parent && p.uid == 0))
        .expect("no actor below a root-owned reaper");
    let reaper = rows.iter().find(|p| p.pid == actor.parent).unwrap();
    assert_eq!(actor.threads, 1);
    assert_eq!(reaper.threads, 1);
    let children = actors
        .iter()
        .filter(|p| p.parent == actor.pid)
        .collect::<Vec<_>>();
    assert_eq!(u32::try_from(children.len()).unwrap(), held);
    assert_eq!(actors.len(), children.len() + 1);
    assert!(children.iter().all(|p| p.threads == 1));
    let server = rows
        .iter()
        .filter(|p| p.uid == shards_abi::SERVER_FIRST_ID)
        .collect::<Vec<_>>();
    assert_eq!(server.len(), 2, "server/helper reservation differs: {rows:?}");
    assert!(server.iter().all(|p| p.threads == 1));
    assert!(
        server
            .iter()
            .any(|p| server.iter().any(|helper| helper.parent == p.pid)),
        "no server/helper tree"
    );
    let observed_reserved = actor.threads + reaper.threads + server.iter().map(|p| p.threads).sum::<u32>();
    assert_eq!(
        observed_reserved, expected_reserved,
        "source reservation prediction falsified"
    );
    actor.pid
}

struct Run<'a> {
    env: &'a [(&'a str, &'a std::ffi::OsStr)],
    patience: Duration,
    running: bool,
}
impl Run<'_> {
    fn command(&self, words: &[&str]) -> common::Run {
        run_shards_env(&[], words, self.env, self.patience)
    }
    fn snapshot(&self, phase: &str, budget: Budget, held: u32) {
        let result = self.command(&["top", "managed-limits", "-o", "pid,ppid,uid,nlwp"]);
        eprintln!("{phase} trusted process snapshot:\n{result}");
        assert_eq!(result.status, Some(0));
        tree(&process_rows(&result.stdout).unwrap(), budget.reserved, held);
    }
}
impl Drop for Run<'_> {
    fn drop(&mut self) {
        if self.running {
            let _ = self.command(&["stop", "vm", "-t", "0", "managed-limits"]);
        }
        let result = self.command(&["stop", "daemon"]);
        eprintln!("private daemon teardown: {result}");
        if let Some((_, home)) = self.env.iter().find(|(key, _)| *key == "SHARDS_HOME") {
            eprintln!(
                "private daemon log:\n{}",
                std::fs::read_to_string(Path::new(home).join("daemon.log")).unwrap_or_default()
            );
        }
    }
}

fn report(root: &Path, name: &str, phase: Phase, budget: Budget, until: Instant) -> Report {
    let report = Report::decode(&published(root, name, until)).unwrap();
    eprintln!("{name} actor observation: {report:?}");
    assert_eq!(report.phase, phase);
    assert_eq!(report.budget, budget);
    assert_eq!(
        (
            report.actor,
            report.parent,
            report.uid,
            report.reaper_uid,
            report.threads
        ),
        (2, 1, shards_abi::DOMAIN_FIRST_ID, 0, 1)
    );
    report
}

fn connect(run: &Run<'_>, port: u16, until: Instant) -> std::net::TcpStream {
    let ports = run.command(&["read", "ports", "managed-limits"]);
    assert_eq!(ports.status, Some(0), "{ports}");
    let host_port: u16 = ports
        .stdout
        .lines()
        .find(|line| line.starts_with(&format!("{port}/tcp")))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|address| address.rsplit(':').next())
        .and_then(|port| port.parse().ok())
        .expect("no published fixture control port");
    let left = until.saturating_duration_since(Instant::now());
    assert!(!left.is_zero());
    let stream = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, host_port)),
        left,
    )
    .unwrap();
    stream.set_read_timeout(Some(left)).unwrap();
    stream.set_write_timeout(Some(left)).unwrap();
    stream
}

fn receive<const N: usize>(stream: &mut std::net::TcpStream, until: Instant) -> [u8; N] {
    let mut output = [0; N];
    let mut offset = 0;
    while offset < N {
        let left = until.saturating_duration_since(Instant::now());
        assert!(!left.is_zero());
        stream.set_read_timeout(Some(left)).unwrap();
        let n = stream.read(&mut output[offset..]).unwrap();
        assert_ne!(n, 0, "control EOF");
        offset += n;
    }
    output
}

fn neighbor(run: &Run<'_>, control: &mut std::net::TcpStream, phase: &str, until: Instant) {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let bytes = nonce.to_be_bytes();
    control
        .set_write_timeout(Some(until.saturating_duration_since(Instant::now())))
        .unwrap();
    control.write_all(&bytes).unwrap();
    assert_eq!(receive::<16>(control, until), bytes);
    let hex = format!("{nonce:032x}");
    let result = run.command(&[
        "exec",
        "vm",
        "managed-limits",
        "/bin/testguest",
        "managed-limits-nonce",
        &hex,
    ]);
    assert_eq!(result.status, Some(0), "{result}");
    assert_eq!(result.stdout, hex, "{result}");
    eprintln!("fresh HARNESS and main nonce answered after {phase}: {hex}");
}

#[test]
#[ignore = "coordinator real-VM campaign; requires explicit MANAGED_LIMIT_* inputs"]
fn a_finite_tenant_task_refusal_recovers_without_blocking_its_neighbors() {
    if common::cannot_run_vms() {
        return;
    }
    let budget = Budget::new(
        input("MANAGED_LIMIT_TASKS"),
        input("MANAGED_LIMIT_RESERVED"),
        input("MANAGED_LIMIT_CHILDREN"),
    )
    .unwrap();
    let patience = Duration::from_millis(u64::from(input("MANAGED_LIMIT_PATIENCE_MS")));
    assert!(!patience.is_zero());
    assert_eq!(
        input("MANAGED_LIMIT_ROUNDS"),
        1,
        "this fixture owns one finite cycle; repeat campaigns belong to coordinator"
    );
    let (base, _) = common::served();
    let home = TempDir::new("limits-home");
    let actor = TempDir::new("limits-actor");
    let healthy = TempDir::new("limits-neighbor");
    let context = TempDir::new("limits-context");
    // Keep both OS-assigned port reservations until context construction is done.
    let ports = [
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap(),
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap(),
    ];
    let actor_port = ports[0].local_addr().unwrap().port();
    let healthy_port = ports[1].local_addr().unwrap().port();
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", common::kernel().as_os_str()),
        ("SHARDS_INIT", common::guest_init().as_os_str()),
    ];
    let mut run = Run {
        env: &env,
        patience,
        running: false,
    };
    for (kind, name, options) in [
        (
            "agent",
            "limited",
            serde_json::json!([
                "managed-limits",
                "/limits",
                budget.tasks.to_string(),
                budget.reserved.to_string(),
                budget.children.to_string(),
                patience.as_millis().to_string(),
                actor_port.to_string()
            ]),
        ),
        (
            "harness",
            "healthy",
            serde_json::json!([
                "managed-limits-healthy",
                "/healthy",
                patience.as_millis().to_string(),
                healthy_port.to_string()
            ]),
        ),
    ] {
        std::fs::create_dir(context.join(name)).unwrap();
        common::program_copy(common::test_guest(), &context.join(name).join("probe"));
        std::fs::write(
            context.join(name).join(format!("{kind}.json")),
            serde_json::json!({"entrypoint":"probe", "options":options}).to_string(),
        )
        .unwrap();
    }
    std::fs::write(context.join("Agentfile"), format!("FROM {base}\nAGENT --processes={} limited FROM ./limited\nHARNESS healthy FROM ./healthy\nVOLUME --chown=limited /limits FOR limited\nVOLUME --chown=healthy /healthy FOR healthy\nNETWORK --ingress={actor_port},{healthy_port} control\nEXPOSE --agents=limited {actor_port} AS ingress FOR control\nEXPOSE --harnesses=healthy {healthy_port} AS ingress FOR control\nCONNECT limited WITH limited ON control\nCONNECT healthy WITH healthy ON control\n", budget.tasks)).unwrap();
    let built = run.command(&[
        "build",
        "image",
        "-t",
        "managed-limits:1",
        context.to_str().unwrap(),
    ]);
    assert_eq!(built.status, Some(0), "{built}");
    let actor_bind = format!("{}:/limits", actor.display());
    let healthy_bind = format!("{}:/healthy", healthy.display());
    drop(ports);
    let until = Instant::now().checked_add(patience).unwrap();
    run.running = true;
    let started = run.command(&[
        "run",
        "vm",
        "-d",
        "--name",
        "managed-limits",
        "-v",
        &actor_bind,
        "-v",
        &healthy_bind,
        "managed-limits:1",
        "sleep",
    ]);
    assert_eq!(started.status, Some(0), "{started}");
    let ready = report(&actor, "ready", Phase::Ready, budget, until);
    assert_eq!(published(&healthy, "ready", until), [1]);
    run.snapshot("before fill", budget, 0);
    let mut actor_control = connect(&run, actor_port, until);
    let mut healthy_control = connect(&run, healthy_port, until);
    actor_control.write_all(&Command::Fill.encode()).unwrap();
    let refused = report(&actor, "refused", Phase::Refused, budget, until);
    assert_eq!(
        (refused.held, refused.reaped, refused.cause, refused.errno),
        (budget.children, 0, Cause::ForkRefused, libc::EAGAIN)
    );
    assert_eq!(
        Report::decode(&receive::<{ Report::BYTES }>(&mut actor_control, until)).unwrap(),
        refused
    );
    run.snapshot("after actual EAGAIN", budget, budget.children);
    neighbor(&run, &mut healthy_control, "refused", until);
    actor_control.write_all(&Command::Release.encode()).unwrap();
    let recovered = report(&actor, "recovered", Phase::Recovered, budget, until);
    assert_eq!(
        (recovered.held, recovered.reaped, recovered.cause, recovered.errno),
        (0, budget.children.checked_add(1).unwrap(), Cause::None, 0)
    );
    assert!(recovered.replacement > recovered.actor);
    assert_eq!(
        Report::decode(&receive::<{ Report::BYTES }>(&mut actor_control, until)).unwrap(),
        recovered
    );
    assert_eq!(recovered.descriptors, ready.descriptors.checked_add(1).unwrap());
    assert_eq!(
        refused.descriptors,
        recovered
            .descriptors
            .checked_add(budget.children.checked_mul(2).unwrap())
            .unwrap()
    );
    run.snapshot("after replacement reaped", budget, 0);
    neighbor(&run, &mut healthy_control, "recovered", until);
    let stopped = run.command(&["stop", "vm", "-t", "0", "managed-limits"]);
    assert_eq!(stopped.status, Some(0), "{stopped}");
    run.running = false;
    let state = run.command(&[
        "inspect",
        "vm",
        "--format",
        "{{.State.Running}} {{.State.Pid}}",
        "managed-limits",
    ]);
    assert_eq!(state.status, Some(0), "{state}");
    assert_eq!(state.stdout.trim(), "false 0", "{state}");
    let removed = run.command(&["remove", "vm", "managed-limits"]);
    assert_eq!(removed.status, Some(0), "{removed}");
    eprintln!(
        "stop returned; daemon state not-running/PID0; private run removed. Host/kernel resource campaign remains coordinator-owned."
    );
}

#[test]
fn process_observation_checks_threads_and_parentage() {
    let rows = process_rows(
        "PID PPID UID NLWP\n10 1 0 1\n11 10 200000 1\n12 1 4394304 1\n13 12 4394304 1\n14 11 200000 1\n",
    )
    .unwrap();
    assert_eq!(tree(&rows, 4, 1), 11);
    assert!(process_rows("PID PPID UID NLWP\n1 2 x 1\n").is_err());
    assert!(process_rows("PID PPID UID NLWP\n1 2 3 1 extra\n").is_err());
}
