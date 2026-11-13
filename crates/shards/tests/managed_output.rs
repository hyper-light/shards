//! Supplied AGENT/HARNESS output through the daemon's private owner endpoint.
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

#[path = "managed_output/member_stdin.rs"]
mod member_stdin;

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
    #[cfg(unix)]
    observer_sockets: Vec<managed_capture::Held>,
    /// A whole-process descriptor baseline, compared at shutdown: valid only for a caller that
    /// runs alone in its process, as the coordinator's cost campaign does, which sets it. The
    /// binary's other tests run beside each other, so the fixture never sets it.
    initial_fds: Option<usize>,
    /// The members the run gives an input (`--annotation shards.stdin=…`), where it gives any.
    stdin: Option<&'static str>,
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
            #[cfg(unix)]
            observer_sockets: Vec::new(),
            initial_fds: None,
            stdin: None,
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
        // A context's files keep their modes on a Unix host, as fsutil walks them, and a
        // copied file is a program only if made one; a Windows host's context gives every
        // file 0755 (tests/member_exec.rs).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                self.context.join("agent/member"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
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
        let stdin = self.stdin.map(|members| format!("shards.stdin={members}"));
        // Its owner reads both members' output (D240): the daemon drains none of it.
        let mut args = vec![
            "run",
            "vm",
            "-d",
            "--name",
            "owner-output",
            "--annotation",
            "shards.output=agent:helper,harness:driver",
        ];
        for binding in publication.iter().flatten() {
            args.extend(["-p", binding.as_str()]);
        }
        if let Some(annotation) = &stdin {
            args.extend(["--annotation", annotation.as_str()]);
        }
        args.extend(["owner-output:1", "sleep"]);
        let started = run(&args);
        assert_eq!(started.status, Some(0), "{started}");
        let id = started.stdout.trim();
        assert!(
            !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "unexpected detached run identity"
        );
        let record = self.home.join("containers").join(id).join("managed-output");
        // Public fingerprints and loopback endpoints alone: shown whole where it is not one.
        let text = std::fs::read_to_string(&record).unwrap();
        let metadata = Metadata::parse(&text).unwrap_or_else(|| panic!("published owner metadata:\n{text}"));
        assert_eq!(metadata.run, id);
        assert_eq!(metadata.server, self.owner.fingerprint());
        assert_eq!(metadata.owner, self.owner.fingerprint());
        let expected: &[(u32, u8)] = if self.stdin.is_some() {
            &[(1, 1), (1, 2), (2, 0), (2, 1), (2, 2)]
        } else {
            &[(1, 1), (1, 2), (2, 1), (2, 2)]
        };
        assert_eq!(
            metadata.endpoints.keys().copied().collect::<Vec<_>>(),
            expected,
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
        #[cfg(unix)]
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
            // A stop returns once its daemon has exited (cli/client.rs `stop`), which lets go
            // of its home's lock last (daemon.rs `finish`), its runs and their managed records
            // ended before. HVF at 4a426c367 found those records unwritten after a stop.
            Ok(_) if !daemon_gone(&self.home) => errors.push(
                "`stop daemon` returned while its daemon still held its home's lock: its runs, and their \
                 managed records, not yet ended"
                    .into(),
            ),
            Err(_) => errors.push("owner-stop CLI helper panicked".into()),
            _ => {}
        }
        for observer in &mut self.observers {
            if let Err(error) = observer.join(until) {
                errors.push(error);
            }
        }
        self.observers.clear();
        #[cfg(unix)]
        {
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
        }
        #[cfg(windows)]
        let _ = writeln!(
            std::io::stdout(),
            "managed-owner observer_sockets unverified: {}",
            managed_capture::UNIDENTIFIED
        );
        if let Some(before) = self.initial_fds {
            match descriptors() {
                Ok(after) => {
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
            .unwrap_or_else(|| {
                panic!(
                    "terminal metadata of member {member} channel {}:\n{record}",
                    channel as u8
                )
            });
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

    /// Its observers' sockets as evidence records them: each one's descriptor and identity
    /// (managed_capture::Held), or why Windows names none.
    fn sockets_record(&self) -> serde_json::Value {
        #[cfg(unix)]
        {
            self.observer_sockets
                .iter()
                .map(|socket| serde_json::json!({"fd":socket.fd,"id":socket.id}))
                .collect::<Vec<_>>()
                .into()
        }
        #[cfg(windows)]
        {
            serde_json::json!({"state":"Unverified","why":managed_capture::UNIDENTIFIED})
        }
    }
}

fn declared_label(member: u32) -> &'static str {
    match member {
        1 => "agent helper",
        2 => "harness driver",
        _ => panic!("fixture has no runnable member {member}"),
    }
}

/// Whether `home`'s daemon has let go of it: its lock (`daemon.lock`), which a daemon holds
/// from its start until the last of its exit (daemon.rs `finish`), free.
fn daemon_gone(home: &std::path::Path) -> bool {
    // A home no daemon took has no lock to hold.
    let Ok(lock) = std::fs::File::open(home.join("daemon.lock")) else {
        return true;
    };
    let free = lock.try_lock().is_ok();
    // Let go of before its file closes (M244).
    let _ = lock.unlock();
    free
}

/// This process's open descriptors, counted as the coordinator's cost campaign counts the
/// baseline it sets ([`Fixture::initial_fds`]): the entries of Linux's /proc/self/fd, or of
/// macOS's /dev/fd (fdescfs), the listing's own among them; Windows' handles
/// (GetProcessHandleCount, platform::open_descriptors). A whole-process count, which this
/// binary's tests running beside each other move: compared only by a caller that runs alone.
fn descriptors() -> std::io::Result<usize> {
    #[cfg(unix)]
    {
        let listing = if cfg!(target_os = "linux") {
            "/proc/self/fd"
        } else {
            "/dev/fd"
        };
        Ok(std::fs::read_dir(listing)?.count())
    }
    #[cfg(windows)]
    {
        shards_vmm::platform::open_descriptors()
            .and_then(|count| usize::try_from(count).ok())
            .ok_or_else(std::io::Error::last_os_error)
    }
}

/// Stops an observer as the thread holding this unwinds: its reader's wait then ends at once,
/// with its driver's end, not at its deadline.
struct StopOnPanic<'a>(&'a shards_remote::net::Sender<Ask>);

impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.request_stop();
        }
    }
}

/// Runs `help` on a thread of its own while `wait` reads `observer` here. A helper that
/// panics stops the observer as it unwinds, which ends `wait`'s read at once, and its own
/// panic, the cause of the read cut short, is the one this fails with, its thread joined.
/// The KVM run at d5afadbc8 waited out a 120 s deadline behind a writer's panic instead,
/// then was ended in its cleanup.
fn beside<H: Send, W>(
    observer: &Observer,
    help: impl FnOnce() -> H + Send,
    wait: impl FnOnce() -> W,
) -> (H, W) {
    let sender = &observer.sender;
    std::thread::scope(|scope| {
        let helper = scope.spawn(move || {
            let _stop = StopOnPanic(sender);
            help()
        });
        let waited = std::panic::catch_unwind(std::panic::AssertUnwindSafe(wait));
        let helped = helper
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (
            helped,
            waited.unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
        )
    })
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

/// The record as the daemon goes on writing it (daemon/managed.rs): each channel's end
/// appended after the endpoints, an output's `canonical` errno and `state`, an input's
/// `state`. A member that ends at once has its input's end there before its owner reads the
/// record, which was then no record (HVF at 4a426c367: "published owner metadata").
#[test]
fn metadata_keeps_its_endpoints_beside_the_ends_appended_after_them() {
    let key = "01".repeat(32);
    let header =
        format!("shards managed output 1\nserver {key}\nowner {key}\nrun abc\ngeneration 9\ncapacity 64\n");
    let endpoints =
        "member 2 channel 0 address 127.0.0.1:12345\nmember 2 channel 1 address 127.0.0.1:12346\n";
    let ends = "member 2 channel 0 state aborted written 0 errno 32\n\
        member 2 channel 1 canonical aborted errno 5\n\
        member 2 channel 1 state aborted accepted 20 consumed 20 tail unknown\n";
    let metadata = Metadata::parse(&format!("{header}{endpoints}{ends}")).unwrap();
    assert_eq!(
        metadata.endpoints.keys().copied().collect::<Vec<_>>(),
        [(2, 0), (2, 1)]
    );
    // An end of a channel it never published, an endpoint after an end, a word the daemon
    // never writes, and an end that says nothing: none is a record of its.
    for wrong in [
        "member 1 channel 1 state ended accepted 7 consumed 7 tail complete\n",
        "member 2 channel 0 state closed written 4 errno 0\nmember 2 channel 2 address 127.0.0.1:12347\n",
        "member 2 channel 1 torn\n",
        "member 2 channel 1 state\n",
    ] {
        assert!(
            Metadata::parse(&format!("{header}{endpoints}{wrong}")).is_none(),
            "{wrong:?}"
        );
    }
}

/// An observer whose endpoint never answers: a UDP socket on loopback, told apart from the
/// observer's own as theirs are, by its whole address. 127.0.0.1, which every host's loopback
/// serves: macOS's lo0 serves no other address of 127.0.0.0/8 without an alias its root adds.
fn unanswered_observer() -> (std::net::UdpSocket, Observer) {
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
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

/// What a caught panic said: a message with arguments is a String, a literal a &'static str.
fn said(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|said| (*said).to_owned()))
        .unwrap_or_default()
}

#[test]
fn a_lost_observer_stream_says_how_its_driver_ended() {
    let lost = |observer: &Observer| {
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            observer.next(Instant::now() + WITHIN)
        }))
        .unwrap_err();
        said(panic.as_ref())
    };
    // A driver that ends normally: its observer's own cancel.
    let (_silent, mut cancelled) = unanswered_observer();
    admit(&cancelled, Ask::Cancel);
    let said = lost(&cancelled);
    assert!(said.contains("driver ended Ok"), "{said}");
    cancelled.join(Instant::now() + WITHIN).unwrap();
    // A driver whose thread panics, as it is asked to.
    let (_silent, mut panicked) = unanswered_observer();
    admit(&panicked, Ask::Panic);
    let said = lost(&panicked);
    assert!(said.contains("panicked"), "{said}");
    assert!(panicked.join(Instant::now() + WITHIN).is_err());
}

/// A helper that panics while the test reads an observer beside it ends that read at once,
/// before its deadline, and the helper's own panic is what fails: the read is never left to
/// wait it out, as member_stdin's was behind its writer at d5afadbc8.
#[test]
fn a_helpers_panic_ends_the_read_beside_it_and_is_what_fails() {
    fn helper() {
        panic!("the helper's own failure");
    }
    let (_silent, mut observer) = unanswered_observer();
    let began = Instant::now();
    let until = began + WITHIN;
    let read = std::cell::Cell::new(None);
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        beside(&observer, helper, || {
            let next = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer.next(until)));
            let how = match next {
                Ok(event) => format!("{event:?}"),
                Err(panic) => said(panic.as_ref()),
            };
            read.set(Some((Instant::now(), how)));
        })
    }))
    .unwrap_err();
    assert_eq!(said(failed.as_ref()), "the helper's own failure");
    // Ended by the stop the helper's panic sent, its driver's end: not by the observer's own
    // deadline (its Failed) nor the read's, which come WITHIN after it began.
    let (ended, how) = read.take().unwrap();
    assert!(
        how.contains("driver ended Ok"),
        "the read ended {:?} after it began, by {how}",
        ended.duration_since(began)
    );
    observer.join(until).unwrap();
}

/// The binary's tests share this process: another test's descriptors come and go while an
/// observer lives. Its own sockets, told apart by their kernel identity (managed_capture's
/// Held), are what its shutdown checks. Unix's alone: Windows names a socket by nothing a
/// test can read again (managed_capture::UNIDENTIFIED), and has no dup(2).
#[cfg(unix)]
#[test]
fn observer_sockets_are_checked_by_identity_beside_other_tests_descriptors() {
    use std::os::fd::FromRawFd as _;
    let count = || descriptors().unwrap();
    // As another test's would: a descriptor open across the baseline, closed in the window.
    let neighbor = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
    let before = count();
    let (silent, mut observer) = unanswered_observer();
    let held = observer.held.clone();
    assert_eq!(
        managed_capture::not_returned(&held),
        held,
        "both open while it lives"
    );
    // A leak: a second descriptor for one of its sockets, kept past the observer's end. It is
    // close-on-exec (F_DUPFD_CLOEXEC), as std makes its own: std starts a process with each
    // descriptor that is not (posix_spawn, no POSIX_SPAWN_CLOEXEC_DEFAULT), and dup(2) clears
    // the flag, so a process another test started meanwhile held the socket past the copy's
    // end (HVF at 4a426c367).
    let leaked = held[0];
    // SAFETY: fcntl(2) duplicating a descriptor this live observer holds.
    let copy = unsafe { libc::fcntl(leaked.fd, libc::F_DUPFD_CLOEXEC, 0) };
    assert!(copy >= 0, "{}", std::io::Error::last_os_error());
    // SAFETY: the descriptor fcntl(2) just returned, owned here once.
    let copy = unsafe { std::os::fd::OwnedFd::from_raw_fd(copy) };
    // As another test's would: a process started while the copy lives, until its stdin ends.
    let mut started = std::process::Command::new("cat")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
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
    drop(started.stdin.take());
    started.wait().unwrap();
    assert!(returned.is_empty(), "{returned:?}");
}

// The coordinator's cost campaign is Linux's: it reads each process's /proc (stat, status,
// smaps_rollup, cgroup, descriptors and UDP tables) and holds its exit watches as pidfds, whose
// count its descriptor check adds. macOS's and Windows' twins of those (libproc, kqueue, process
// handles) it does not have yet; its parsers read Linux's formats alone.
#[cfg(target_os = "linux")]
#[path = "../../../docs/research/measurements/managed-output-cost/fixture.rs"]
mod cost;
