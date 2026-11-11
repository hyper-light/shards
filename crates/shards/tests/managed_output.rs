//! Supplied AGENT/HARNESS output through the daemon's private owner endpoint.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod common;

#[path = "common/managed_capture.rs"]
mod managed_capture;
use managed_capture::{Ask, Event, Metadata, Observer};

#[path = "managed_output/tenant_exit.rs"]
mod tenant_exit;

#[path = "managed_output/broker_effects.rs"]
mod broker_effects;

#[path = "managed_output/metrics.rs"]
mod metrics;

#[path = "common/managed_alloc.rs"]
mod managed_alloc;

#[global_allocator]
static MANAGED_ALLOC: managed_alloc::ManagedAlloc = managed_alloc::ManagedAlloc::new();

use std::collections::BTreeMap;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use shards_abi::run::managed::{Channel, Event as CaptureEvent, Record, StreamState};
use shards_remote::identity::Identity;
use shards_remote::managed::{Reason, Scope};
use shards_remote::net::Command;

// Test patience, matching the neighboring real-VM build fixtures; never runtime policy.
const WITHIN: Duration = Duration::from_secs(120);

fn payload(member: &str, channel: Channel) -> Vec<u8> {
    let tail: &[u8] = match channel {
        Channel::Stdout => b"\0stdout\n\xff\x1b[2Jout-tail",
        Channel::Stderr => b"\0stderr\n\x80\xfeerr-tail",
    };
    [member.as_bytes(), tail].concat()
}

struct Fixture {
    home: common::TempDir,
    context: common::TempDir,
    owner: Identity,
    metadata: Option<Metadata>,
    record: Option<PathBuf>,
    observers: Vec<Observer>,
    observed: BTreeMap<(u32, u8), (u64, u64)>,
    /// Every socket this fixture's observers opened, checked returned at shutdown.
    observer_sockets: Vec<managed_capture::Held>,
    /// A whole-process descriptor baseline, compared at shutdown: valid only for a caller that
    /// runs alone in its process, as the coordinator's cost campaign does, which sets it. The
    /// binary's other tests run beside each other, so the fixture never sets it.
    initial_fds: Option<usize>,
}

impl Fixture {
    fn new() -> Self {
        let home = common::TempDir::new("owner-output");
        let owner = Identity::load_or_create(&home.join("identity")).unwrap();
        Self {
            home,
            context: common::TempDir::new("owner-context"),
            owner,
            metadata: None,
            record: None,
            observers: Vec::new(),
            observed: BTreeMap::new(),
            observer_sockets: Vec::new(),
            initial_fds: None,
        }
    }

    fn command(&self, args: &[&str], within: Duration) -> common::Run {
        common::run_shards_env(
            &[],
            args,
            &[
                ("SHARDS_HOME", self.home.as_os_str()),
                ("SHARDS_KERNEL", common::kernel().as_os_str()),
                ("SHARDS_INIT", common::guest_init().as_os_str()),
            ],
            within,
        )
    }

    fn start(&mut self) {
        self.start_with_options(
            &["managed-capture", "agent"],
            &["managed-capture", "harness"],
            "",
            None,
            None,
            Instant::now() + WITHIN,
        );
    }

    fn start_with_options(
        &mut self,
        agent: &[&str],
        harness: &[&str],
        directives: &str,
        agent_tasks: Option<u32>,
        publish_ports: Option<&[u16]>,
        until: Instant,
    ) {
        let processes = agent_tasks
            .map(|tasks| {
                assert!(
                    tasks > 0,
                    "AGENT --processes numeric grammar requires positive u32"
                );
                format!("--processes={tasks} ")
            })
            .unwrap_or_default();
        let publication = publish_ports.map(|ports| {
            assert!(!ports.is_empty(), "explicit guest fixture ports required");
            ports
                .iter()
                .map(|port| {
                    assert_ne!(*port, 0, "explicit guest fixture port required");
                    format!("127.0.0.1::{port}")
                })
                .collect::<Vec<_>>()
        });
        let (image, _) = common::served();
        std::fs::create_dir(self.context.join("files")).unwrap();
        std::fs::write(self.context.join("files/asset"), b"files-only").unwrap();
        std::fs::create_dir(self.context.join("agent")).unwrap();
        std::fs::copy(common::test_guest(), self.context.join("agent/member")).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            self.context.join("agent/member"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::write(
            self.context.join("agent/agent.json"),
            serde_json::to_vec(&serde_json::json!({"entrypoint":"member","options":agent})).unwrap(),
        )
        .unwrap();
        let binary = std::fs::read(common::test_guest()).unwrap();
        let manifest =
            serde_json::to_vec(&serde_json::json!({"entrypoint":"member","options":harness})).unwrap();
        std::fs::write(
            self.context.join("harness.tar"),
            common::tar(&[
                ("bin", 0o755, 0, None),
                ("bin/member", 0o755, 0, Some(&binary)),
                ("harness.json", 0o644, 0, Some(&manifest)),
            ]),
        )
        .unwrap();
        std::fs::write(self.context.join("Agentfile"), format!("FROM {image}\nAGENT assets FROM ./files\nAGENT {processes}helper FROM ./agent\nHARNESS driver FROM ./harness.tar\n{directives}")).unwrap();
        let built = self.command(
            &[
                "build",
                "image",
                "-t",
                "owner-output:1",
                self.context.to_str().unwrap(),
            ],
            until.saturating_duration_since(Instant::now()),
        );
        assert_eq!(built.status, Some(0), "{built}");
        let run = |args: &[&str]| self.command(args, until.saturating_duration_since(Instant::now()));
        let started = match publication.as_ref() {
            Some(bindings) => {
                let mut args = vec!["run", "vm", "-d", "--name", "owner-output"];
                for binding in bindings {
                    args.extend(["-p", binding.as_str()]);
                }
                args.extend(["owner-output:1", "sleep"]);
                run(&args)
            }
            None => run(&[
                "run",
                "vm",
                "-d",
                "--name",
                "owner-output",
                "owner-output:1",
                "sleep",
            ]),
        };
        assert_eq!(started.status, Some(0), "{started}");
        let id = started.stdout.trim();
        assert!(
            !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "unexpected detached run identity"
        );
        let record = self.home.join("containers").join(id).join("managed-output");
        let metadata = Metadata::parse(&std::fs::read_to_string(&record).unwrap())
            .unwrap_or_else(|| panic!("published owner metadata"));
        assert_eq!(metadata.run, id);
        assert_eq!(metadata.server, self.owner.fingerprint());
        assert_eq!(metadata.owner, self.owner.fingerprint());
        assert_eq!(
            metadata.endpoints.keys().copied().collect::<Vec<_>>(),
            [(1, 1), (1, 2), (2, 1), (2, 2)],
            "files-only member retains its declaration hole"
        );
        self.record = Some(record);
        self.metadata = Some(metadata);
    }

    fn observe(
        &mut self,
        member: u32,
        channel: Channel,
        offered: Option<Scope>,
        outsider: bool,
        until: Instant,
    ) -> usize {
        let metadata = self.metadata.as_ref().unwrap();
        let _ = writeln!(
            std::io::stdout(),
            "managed-owner subscribe member={member} channel={} outsider={outsider}",
            channel as u8
        );
        let stranger = outsider.then(|| Identity::generate().unwrap());
        let observer = Observer::start(
            stranger.as_ref().unwrap_or(&self.owner),
            metadata,
            offered.unwrap_or_else(|| metadata.scope(member, channel)),
            metadata.endpoints[&(member, channel as u8)],
            until,
        );
        self.observer_sockets.extend_from_slice(&observer.held);
        self.observers.push(observer);
        self.observers.len() - 1
    }

    fn main_progress(&self, within: Duration) {
        let _ = writeln!(std::io::stdout(), "managed-owner main exec");
        let progress = self.command(
            &[
                "exec",
                "owner-output",
                "/bin/testguest",
                "streams",
                "main-progress",
                "main-error",
            ],
            within,
        );
        assert_eq!(progress.status, Some(0), "{progress}");
        assert_eq!(progress.stdout, "main-progress\n", "{progress}");
        let stderr = progress
            .stderr
            .strip_prefix("main-error\n")
            .unwrap_or_else(|| panic!("independent main stderr"));
        assert!(
            stderr.is_empty() || stderr.starts_with("shards-timing "),
            "unexpected main diagnostic"
        );
    }

    fn consume(&mut self, member: u32, label: &str, channel: Channel) {
        let observer = self.observe(member, channel, None, false, Instant::now() + WITHIN);
        let expected = payload(label, channel);
        let (_, wire) =
            self.observers[observer].exact(declared_label(member), &expected, Instant::now() + WITHIN);
        self.observers[observer].ack_and_finish(wire, Instant::now() + WITHIN);
        self.observed.insert((member, channel as u8), (wire, wire));
        let _ = writeln!(
            std::io::stdout(),
            "managed-owner consumed member={member} channel={} bytes={}",
            channel as u8,
            expected.len()
        );
    }

    fn shutdown(&mut self, until: Instant) -> Result<(), String> {
        let _ = writeln!(std::io::stdout(), "managed-owner owner stop");
        let mut errors = Vec::new();
        for observer in &self.observers {
            observer.sender.request_stop();
        }
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.command(
                &["stop", "daemon"],
                until.saturating_duration_since(Instant::now()),
            )
        })) {
            Ok(stopped) if stopped.status != Some(0) => errors.push(format!("owner stop: {stopped}")),
            Err(_) => errors.push("owner-stop CLI helper panicked".into()),
            _ => {}
        }
        for observer in &mut self.observers {
            if let Err(error) = observer.join(until) {
                errors.push(error);
            }
        }
        self.observers.clear();
        let open = managed_capture::not_returned(&self.observer_sockets);
        let _ = writeln!(
            std::io::stdout(),
            "managed-owner observer_sockets opened={} not_returned={}",
            self.observer_sockets.len(),
            open.len()
        );
        if !open.is_empty() {
            errors.push(format!("observer sockets not returned: {open:?}"));
        }
        if let Some(before) = self.initial_fds {
            match std::fs::read_dir("/proc/self/fd") {
                Ok(entries) => {
                    let after = entries.count();
                    let _ = writeln!(
                        std::io::stdout(),
                        "managed-owner observer_fds before={before} after={after}"
                    );
                    if after != before {
                        errors.push(format!("observer FD return {before} -> {after}"));
                    }
                }
                Err(error) => errors.push(format!("observer FD observation: {error}")),
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn terminal(&self, member: u32, channel: Channel) -> (u64, u64) {
        let record = std::fs::read_to_string(self.record.as_ref().unwrap()).unwrap();
        let prefix = format!("member {member} channel {} state ", channel as u8);
        let terminal = record
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("terminal channel metadata"));
        let fields: Vec<_> = terminal.split_whitespace().collect();
        let [
            "aborted",
            "accepted",
            accepted,
            "consumed",
            consumed,
            "tail",
            "unknown",
        ] = fields.as_slice()
        else {
            panic!("unexpected terminal metadata");
        };
        let cursors = (accepted.parse().unwrap(), consumed.parse().unwrap());
        let _ = writeln!(
            std::io::stdout(),
            "managed-owner terminal member={member} channel={} accepted={} consumed={} tail=unknown",
            channel as u8,
            cursors.0,
            cursors.1
        );
        cursors
    }
}

fn declared_label(member: u32) -> &'static str {
    match member {
        1 => "agent helper",
        2 => "harness driver",
        _ => panic!("fixture has no runnable member {member}"),
    }
}

fn with_fixture(test: impl FnOnce(&mut Fixture)) -> Option<Fixture> {
    if common::cannot_run_vms() {
        return None;
    }
    let mut fixture = Fixture::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fixture.start();
        test(&mut fixture);
    }));
    let stopped = fixture.shutdown(Instant::now() + WITHIN);
    if let Err(error) = result {
        let _ = writeln!(std::io::stderr(), "owner cleanup: {stopped:?}");
        std::panic::resume_unwind(error);
    }
    stopped.unwrap();
    Some(fixture)
}

#[test]
fn supplied_owner_output_keeps_bytes_ack_cancel_and_neighbor_progress_independent() {
    let Some(fixture) = with_fixture(|fixture| {
        let held = fixture.observe(1, Channel::Stdout, None, false, Instant::now() + WITHIN);
        let (start, end) = fixture.observers[held].exact(
            declared_label(1),
            &payload("agent", Channel::Stdout),
            Instant::now() + WITHIN,
        );
        fixture.observed.insert((1, Channel::Stdout as u8), (end, start));
        fixture.consume(1, "agent", Channel::Stderr);
        fixture.main_progress(WITHIN);
        fixture.observers[held]
            .sender
            .send(Command::Handler(Ask::Cancel))
            .unwrap();
        fixture.observers[held].join(Instant::now() + WITHIN).unwrap();
        let _ = writeln!(std::io::stdout(), "managed-owner local cancellation complete");
        fixture.main_progress(WITHIN);
        // The sibling receives its first output credit only after observer loss.
        fixture.consume(2, "harness", Channel::Stdout);
        fixture.consume(2, "harness", Channel::Stderr);
    }) else {
        return;
    };
    // Cancellation preserves a record awaiting ACK instead of manufacturing
    // consumption or holding owner teardown behind its stalled observer.
    for (&(member, channel), &expected) in &fixture.observed {
        assert_eq!(
            fixture.terminal(member, Channel::try_from(channel).unwrap()),
            expected
        );
    }
}

#[test]
fn private_owner_output_refuses_wrong_owner_stale_generation_and_cross_member_scope() {
    let Some(fixture) = with_fixture(|fixture| {
        let outsider = fixture.observe(1, Channel::Stdout, None, true, Instant::now() + WITHIN);
        // The current public Driver erases the typed TLS cause. A peer close without
        // an application code is observed; local errors/deadlines/data are failures.
        fixture.observers[outsider].refused(None, Instant::now() + WITHIN);
        let mut stale = fixture.metadata.as_ref().unwrap().scope(1, Channel::Stderr);
        stale.generation = stale.generation.checked_add(1).unwrap();
        let observer = fixture.observe(1, Channel::Stderr, Some(stale), false, Instant::now() + WITHIN);
        fixture.observers[observer].refused(Some(Reason::Stale), Instant::now() + WITHIN);
        let other = fixture.metadata.as_ref().unwrap().scope(1, Channel::Stdout);
        let observer = fixture.observe(2, Channel::Stdout, Some(other), false, Instant::now() + WITHIN);
        fixture.observers[observer].refused(Some(Reason::Scope), Instant::now() + WITHIN);
        fixture.consume(2, "harness", Channel::Stderr);
        fixture.main_progress(WITHIN);
    }) else {
        return;
    };
    for (member, channel) in [(1, Channel::Stdout), (1, Channel::Stderr), (2, Channel::Stdout)] {
        assert_eq!(fixture.terminal(member, channel), (0, 0));
    }
    assert_eq!(
        fixture.terminal(2, Channel::Stderr),
        fixture.observed[&(2, Channel::Stderr as u8)]
    );
}

#[test]
fn metadata_requires_unique_scoped_loopback_endpoints() {
    let key = "01".repeat(32);
    let header =
        format!("shards managed output 1\nserver {key}\nowner {key}\nrun abc\ngeneration 9\ncapacity 64\n");
    let endpoint = "member 2 channel 1 address 127.0.0.1:12345\n";
    let metadata = Metadata::parse(&format!("{header}{endpoint}")).unwrap();
    assert_eq!(metadata.scope(2, Channel::Stdout).member, 2);
    assert!(Metadata::parse(&format!("{header}{endpoint}{endpoint}")).is_none());
    assert!(Metadata::parse(&format!("{header}member 2 channel 1 address 192.0.2.1:12345\n")).is_none());
    assert!(Metadata::parse(&format!("{header}member 2 channel 3 address 127.0.0.1:12345\n")).is_none());
}

/// An observer whose endpoint never answers: a UDP socket on 127.0.0.2, apart from the
/// 127.0.0.1 sockets observers open.
fn unanswered_observer() -> (std::net::UdpSocket, Observer) {
    let silent = std::net::UdpSocket::bind("127.0.0.2:0").unwrap();
    let key = "01".repeat(32);
    let metadata = Metadata::parse(&format!(
        "shards managed output 1\nserver {key}\nowner {key}\nrun unanswered\ngeneration 1\ncapacity 64\nmember 1 channel 1 address {}\n",
        silent.local_addr().unwrap()
    ))
    .unwrap();
    let observer = Observer::start(
        &Identity::generate().unwrap(),
        &metadata,
        metadata.scope(1, Channel::Stdout),
        metadata.endpoints[&(1, 1)],
        Instant::now() + WITHIN,
    );
    (silent, observer)
}

/// Sends `ask` once the observer's one admitted command slot is free: its driver frees it as
/// it takes the Connect `Observer::start` sent.
fn admit(observer: &Observer, ask: Ask) {
    let until = Instant::now() + WITHIN;
    let mut command = Command::Handler(ask);
    while let Err(std::sync::mpsc::TrySendError::Full(back)) = observer.sender.try_send(command) {
        assert!(Instant::now() < until, "observer driver took no command");
        command = back;
        std::thread::yield_now();
    }
}

#[test]
fn a_lost_observer_stream_says_how_its_driver_ended() {
    let lost = |observer: &Observer| {
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            observer.next(Instant::now() + WITHIN)
        }))
        .unwrap_err();
        // A message with arguments is a String, a literal a &'static str.
        panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|said| (*said).to_owned()))
            .unwrap_or_default()
    };
    // A driver that ends normally: its observer's own cancel.
    let (_silent, mut cancelled) = unanswered_observer();
    admit(&cancelled, Ask::Cancel);
    let said = lost(&cancelled);
    assert!(said.contains("driver ended Ok"), "{said}");
    cancelled.join(Instant::now() + WITHIN).unwrap();
    // A driver whose thread panics: a second Connect finds its client config taken.
    let (silent, mut panicked) = unanswered_observer();
    admit(&panicked, Ask::Connect(silent.local_addr().unwrap()));
    let said = lost(&panicked);
    assert!(said.contains("panicked"), "{said}");
    assert!(panicked.join(Instant::now() + WITHIN).is_err());
}

/// The binary's tests share this process: another test's descriptors come and go while an
/// observer lives. Its own sockets, told apart by inode, are what its shutdown checks.
#[test]
fn observer_sockets_are_checked_by_identity_beside_other_tests_descriptors() {
    use std::os::fd::FromRawFd as _;
    let count = || std::fs::read_dir("/proc/self/fd").unwrap().count();
    // As another test's would: a descriptor open across the baseline, closed in the window.
    let neighbor = std::fs::File::open("/proc/self/stat").unwrap();
    let before = count();
    let (silent, mut observer) = unanswered_observer();
    let held = observer.held.clone();
    assert_eq!(
        managed_capture::not_returned(&held),
        held,
        "both open while it lives"
    );
    // A leak: a second descriptor for one of its sockets, kept past the observer's end.
    let leaked = held[0];
    // SAFETY: dup(2) of a descriptor this live observer holds.
    let copy = unsafe { libc::dup(leaked.fd) };
    assert!(copy >= 0, "{}", std::io::Error::last_os_error());
    // SAFETY: the descriptor dup(2) just returned, owned here once.
    let copy = unsafe { std::os::fd::OwnedFd::from_raw_fd(copy) };
    admit(&observer, Ask::Cancel);
    observer.join(Instant::now() + WITHIN).unwrap();
    drop((silent, observer, neighbor));
    let after = count();
    // Alone in its process this prints equal counts beside the leak; beside the binary's other
    // tests the count also moves with theirs. Neither is asserted.
    let _ = writeln!(
        std::io::stdout(),
        "process descriptors before={before} after={after} equal={}",
        before == after
    );
    assert_eq!(managed_capture::not_returned(&held), [leaked]);
    drop(copy);
    let returned = managed_capture::not_returned(&held);
    assert!(returned.is_empty(), "{returned:?}");
}

#[path = "../../../docs/research/measurements/managed-output-cost/fixture.rs"]
mod cost;
