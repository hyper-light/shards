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

fn agentfile(base: &str, budget: Budget, actor_port: u16, healthy_port: u16) -> String {
    let text = format!(
        "FROM {base}\nAGENT --processes={} limited FROM ./limited\nHARNESS healthy FROM ./healthy\nVOLUME --chown=limited /limits FOR limited\nVOLUME --chown=healthy /healthy FOR healthy\nNETWORK --ingress={actor_port} --ingress={healthy_port} control\nEXPOSE --agents=limited {actor_port} AS ingress FOR control\nEXPOSE --harnesses=healthy {healthy_port} AS ingress FOR control\nCONNECT limited WITH limited ON control\nCONNECT healthy WITH healthy ON control\n",
        budget.tasks
    );
    shards_agentfile::agentfile::connections(&parsed_directives(&text)).unwrap();
    text
}

fn parsed_directives(text: &str) -> Vec<shards_agentfile::agentfile::Directive> {
    let parsed =
        shards_agentfile::parser::parse_as(text.as_bytes(), shards_agentfile::parser::Dialect::Agentfile)
            .unwrap();
    let instructions =
        shards_agentfile::instructions::parse(&parsed, &shards_agentfile::lint::Linter::default()).unwrap();
    shards_agentfile::agentfile::check(&instructions).unwrap();
    let directives = instructions.stages[0]
        .commands
        .iter()
        .filter_map(|command| match &command.kind {
            shards_agentfile::instructions::Kind::Agentfile(directive) => Some(directive.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    shards_agentfile::agentfile::resolve_targets(&directives).unwrap()
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

fn tree(rows: &[Process], held: u32) -> Process {
    let actors = rows
        .iter()
        .filter(|p| p.uid == shards_abi::DOMAIN_FIRST_ID)
        .collect::<Vec<_>>();
    let roots = actors
        .iter()
        .filter(|actor| !actors.iter().any(|p| p.pid == actor.parent))
        .collect::<Vec<_>>();
    let [actor] = roots.as_slice() else {
        panic!("expected exactly one visible actor root: {rows:?}");
    };
    assert_ne!(actor.pid, 0);
    assert_ne!(actor.parent, 0);
    assert_ne!(actor.parent, actor.pid);
    assert_eq!(actor.threads, 1);
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
    let visible_idle_tasks = actor.threads + server.iter().map(|p| p.threads).sum::<u32>();
    let visible_child_tasks = children.iter().map(|p| p.threads).sum::<u32>();
    let _ = writeln!(
        std::io::stderr(),
        "visible actor {actor:?}; visible actor+server/helper tasks {visible_idle_tasks}; visible held-child tasks {visible_child_tasks}. Reaper row/threads, total tenant tasks and cgroup membership are unobserved."
    );
    ***actor
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
    fn snapshot(&self, phase: &str, budget: Budget, held: u32) -> Process {
        let result = self.command(&["top", "managed-limits", "-o", "pid,ppid,uid,nlwp"]);
        let _ = writeln!(std::io::stderr(), "{phase} trusted process snapshot:\n{result}");
        assert_eq!(result.status, Some(0));
        let _ = writeln!(
            std::io::stderr(),
            "declared tasks {}; source-predicted reserved {} (not an observed total or admission authority)",
            budget.tasks,
            budget.reserved
        );
        tree(&process_rows(&result.stdout).unwrap(), held)
    }
    fn stop(&mut self, last: Report) {
        let _ = writeln!(
            std::io::stderr(),
            "whole-run stop requested; last confirmed guest observation {last:?}. This is not a terminal reaping result."
        );
        let stopped = self.command(&["stop", "vm", "-t", "0", "managed-limits"]);
        assert_eq!(stopped.status, Some(0), "{stopped}");
        self.running = false;
        let state = self.command(&[
            "inspect",
            "vm",
            "--format",
            "{{.State.Running}} {{.State.Pid}}",
            "managed-limits",
        ]);
        assert_eq!(state.status, Some(0), "{state}");
        assert_eq!(state.stdout.trim(), "false 0", "{state}");
        let removed = self.command(&["remove", "vm", "managed-limits"]);
        assert_eq!(removed.status, Some(0), "{removed}");
        let _ = writeln!(
            std::io::stderr(),
            "stop returned; daemon state not-running/PID0; private run removed. Host/kernel resource campaign remains coordinator-owned."
        );
    }
}
impl Drop for Run<'_> {
    fn drop(&mut self) {
        if self.running {
            let _ = self.command(&["stop", "vm", "-t", "0", "managed-limits"]);
        }
        let result = self.command(&["stop", "daemon"]);
        let _ = writeln!(std::io::stderr(), "private daemon teardown: {result}");
        if let Some((_, home)) = self.env.iter().find(|(key, _)| *key == "SHARDS_HOME") {
            let _ = writeln!(
                std::io::stderr(),
                "private daemon log:\n{}",
                std::fs::read_to_string(Path::new(home).join("daemon.log")).unwrap_or_default()
            );
        }
    }
}

fn report(root: &Path, name: &str, phase: Phase, budget: Budget, until: Instant) -> Report {
    let report = Report::decode(&published(root, name, until)).unwrap();
    let _ = writeln!(std::io::stderr(), "{name} actor observation: {report:?}");
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
        .unwrap_or_else(|| panic!("no published fixture control port {port}/tcp: {ports}"));
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
    let _ = writeln!(
        std::io::stderr(),
        "fresh HARNESS and main nonce answered after {phase}: {hex}"
    );
}

#[derive(Debug)]
enum EndAt {
    Recovered,
    RefusedHeld,
}

#[test]
#[ignore = "coordinator real-VM campaign; requires explicit MANAGED_LIMIT_* inputs"]
fn a_finite_tenant_task_refusal_recovers_without_blocking_its_neighbors() {
    campaign(EndAt::Recovered);
}

#[test]
#[ignore = "coordinator real-VM cancellation; requires explicit MANAGED_LIMIT_* inputs"]
fn a_finite_held_tenant_population_can_be_cancelled_after_neighbor_progress() {
    campaign(EndAt::RefusedHeld);
}

fn campaign(end_at: EndAt) {
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
    std::fs::write(
        context.join("Agentfile"),
        agentfile(&base, budget, actor_port, healthy_port),
    )
    .unwrap();
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
    let actor_publish = format!("127.0.0.1::{actor_port}");
    let healthy_publish = format!("127.0.0.1::{healthy_port}");
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
        "-p",
        &actor_publish,
        "-p",
        &healthy_publish,
        "managed-limits:1",
        "sleep",
    ]);
    assert_eq!(started.status, Some(0), "{started}");
    let ready = report(&actor, "ready", Phase::Ready, budget, until);
    assert_eq!(published(&healthy, "ready", until), [1]);
    let actor_observation = run.snapshot("before fill", budget, 0);
    let mut actor_control = connect(&run, actor_port, until);
    let mut healthy_control = connect(&run, healthy_port, until);
    actor_control.write_all(&Command::Fill.encode()).unwrap();
    let refused = report(&actor, "refused", Phase::Refused, budget, until);
    assert_eq!(
        (refused.held, refused.reaped, refused.cause, refused.errno),
        (
            budget.children,
            0,
            Cause::ForkRefused,
            shards_testguest::limits::LINUX_EAGAIN
        )
    );
    assert_eq!(
        Report::decode(&receive::<{ Report::BYTES }>(&mut actor_control, until)).unwrap(),
        refused
    );
    assert_eq!(
        run.snapshot("after actual EAGAIN", budget, budget.children),
        actor_observation
    );
    neighbor(&run, &mut healthy_control, "refused", until);
    match end_at {
        EndAt::RefusedHeld => {
            assert_eq!(
                run.snapshot("before stopping with children held", budget, budget.children),
                actor_observation
            );
            // Both control streams stay owned through stop. No Release or observer
            // EOF can start guest cleanup before this whole-run cancellation.
            run.stop(refused);
            match std::fs::read(actor.join("recovered")) {
                Ok(bytes) => panic!(
                    "recovery was published without Release: {:?}",
                    Report::decode(&bytes)
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("reading optional recovery publication: {error}"),
            }
            match std::fs::read(actor.join("failure")) {
                Ok(bytes) => {
                    let terminal = Report::decode(&bytes).unwrap();
                    assert_eq!(terminal.phase, Phase::Failed);
                    let _ = writeln!(
                        std::io::stderr(),
                        "guest failure publication retained after stop: {terminal:?}"
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let _ = writeln!(
                        std::io::stderr(),
                        "no guest terminal publication after stop; last confirmed observation remains Refused/held. Host/kernel resource return is unobserved."
                    );
                }
                Err(error) => panic!("reading optional terminal publication: {error}"),
            }
            return;
        }
        EndAt::Recovered => {}
    }
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
    assert_eq!(
        run.snapshot("after replacement reaped", budget, 0),
        actor_observation
    );
    neighbor(&run, &mut healthy_control, "recovered", until);
    run.stop(recovered);
}

#[test]
fn fixture_synthetic_held_children_belong_to_one_visible_actor() {
    let rows =
        process_rows("PID PPID UID NLWP\n11 10 200000 1\n12 1 4394304 1\n13 12 4394304 1\n14 11 200000 1\n")
            .unwrap();
    assert_eq!(
        tree(&rows, 1),
        Process {
            pid: 11,
            parent: 10,
            uid: 200000,
            threads: 1
        }
    );
    assert!(process_rows("PID PPID UID NLWP\n1 2 x 1\n").is_err());
    assert!(process_rows("PID PPID UID NLWP\n1 2 3 1 extra\n").is_err());
}

#[test]
fn fixture_actual_r222_rows_do_not_require_an_unlisted_reaper() {
    let rows = process_rows(include_str!(
        "../../../docs/research/measurements/managed-limits/root-r222-before-fill/top.txt"
    ))
    .unwrap();
    assert_eq!(
        tree(&rows, 0),
        Process {
            pid: 935,
            parent: 933,
            uid: 200000,
            threads: 1
        }
    );
}

#[test]
#[should_panic(expected = "expected exactly one visible actor root")]
fn fixture_ambiguous_actor_roots_are_rejected() {
    let mut rows = process_rows(include_str!(
        "../../../docs/research/measurements/managed-limits/root-r222-before-fill/top.txt"
    ))
    .unwrap();
    rows.push(Process {
        pid: 2000,
        parent: 1999,
        uid: shards_abi::DOMAIN_FIRST_ID,
        threads: 1,
    });
    tree(&rows, 0);
}

#[test]
fn fixture_agentfile_preserves_the_finite_task_declaration_and_grants() {
    use shards_agentfile::agentfile::{Directive, Processes};
    let budget = Budget::new(6, 4, 2).unwrap();
    let text = agentfile("scratch", budget, 7100, 7101);
    let directives = parsed_directives(&text);
    shards_agentfile::agentfile::connections(&directives).unwrap();
    assert!(
        matches!(&directives[0], Directive::Agent(domain) if domain.processes == Processes::AtMost(budget.tasks))
    );
    assert!(matches!(&directives[1], Directive::Harness(domain) if domain.processes == Processes::Unbounded));
    assert_eq!(
        shards_agentfile::agentfile::boundary(&directives, b"control", false),
        [(6, 7100, 7100), (6, 7101, 7101)],
        "fixture must grant both actual ingress ports"
    );
    assert!(shards_agentfile::agentfile::boundary(&directives, b"control", true).is_empty());
    let _ = writeln!(
        std::io::stderr(),
        "checked fixture normalized declaration: {}",
        String::from_utf8(shards_agentfile::agentfile::spec(&directives)).unwrap()
    );
}

#[test]
fn fixture_connections_rejects_the_original_comma_port_request() {
    let valid = agentfile("scratch", Budget::new(6, 4, 2).unwrap(), 7100, 7101);
    let original = valid.replace("--ingress=7100 --ingress=7101", "--ingress=7100,7101");
    let directives = parsed_directives(&original);
    // Parse/check alone are partial. Build planning and runtime both call this
    // existing validator before using grants (plan.rs:5120; shards/agentfile.rs:200).
    let Err(refusal) = shards_agentfile::agentfile::connections(&directives) else {
        panic!("connections accepted the original malformed comma port request");
    };
    let _ = writeln!(
        std::io::stderr(),
        "existing connections rejected original comma request: {}",
        String::from_utf8(refusal).unwrap()
    );
}
