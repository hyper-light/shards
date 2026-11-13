//! D238's log lanes (#72, plan step 11; goal 58's step 7) on a real microVM: a controller of a
//! run's agent-control endpoint reads the logs the run keeps, its command's (D161) and every
//! member's, owned or drained (D240), each record at its place, followed to its source's end,
//! and goes on from where it got to; refused, typed, where the run keeps no such log; and
//! told what the run's one log cap let go. Written for ROOT to run (real-VM suites are the
//! coordinator's): `cargo test -p shards --profile test-release --test managed_output log_lanes`.
use super::*;
use std::sync::mpsc;

use sha2::{Digest as _, Sha256};
use shards_remote::control::exec::{
    Admissions, LogChannels, LogCursor, LogPos, LogRecord, LogSource, Unlogged,
};
use shards_remote::control::{Controller, Dialed, Execs, Fault, Got, Log, Logged, Output, Subscriptions};
use shards_remote::identity::Fingerprint;
use shards_remote::managed::{self as wire, Admitted, RunId};

/// Agent `a`, owned by the run's controller (`shards.output=agent:a`), and agent `b`, which the
/// daemon drains: declared in that order, so indexed 0 and 1.
const A: u32 = 0;
const B: u32 = 1;

/// One test's home: its daemon's, whose identity is the owner every run of it names, and so
/// the controller its runs' agent-control endpoints pin.
struct Home {
    home: common::TempDir,
    owner: Identity,
}

/// What a run's managed metadata says of its agent-control endpoint (daemon/managed.rs): its
/// scope, each owned stream's record, where it listens and the run's own key.
struct Control {
    run: RunId,
    generation: u64,
    capacity: usize,
    address: SocketAddr,
    vm: Fingerprint,
}

/// A run started detached: its directory and its endpoint.
struct Started {
    dir: PathBuf,
    control: Control,
}

fn unhex32(text: &str) -> Option<Fingerprint> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0; 32];
    for (pair, byte) in bytes.as_chunks::<2>().0.iter().zip(&mut out) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

impl Control {
    /// The run's metadata at `record`, whose owner asked for its agent-control endpoint: its
    /// head, then `control ADDRESS` and `vm KEY` in place of each stream's own endpoint.
    fn read(record: &std::path::Path, owner: Fingerprint) -> Control {
        let text = std::fs::read_to_string(record).unwrap();
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name))
                .unwrap_or_else(|| panic!("no `{name}` in the run's managed metadata:\n{text}"))
        };
        assert_eq!(text.lines().next(), Some("shards managed output 1"), "{text}");
        assert_eq!(unhex32(field("owner ")), Some(owner), "{text}");
        assert!(
            !text.lines().any(|line| line.starts_with("member ")),
            "an agent-control run published a stream's own endpoint:\n{text}"
        );
        let address: SocketAddr = field("control ").parse().unwrap();
        assert!(address.ip().is_loopback(), "{text}");
        Control {
            run: RunId(Sha256::digest(field("run ").as_bytes()).into()),
            generation: field("generation ").parse().unwrap(),
            capacity: field("capacity ").parse().unwrap(),
            address,
            vm: unhex32(field("vm ")).unwrap(),
        }
    }

    /// What the endpoint admits each stream, as the daemon binds it (daemon/managed.rs
    /// `start_managed_capture`, `exec_admissions`): an owned member's output its record of
    /// `capacity`; an exec's, and so a log lane's, a guest's output frame (`run::CHUNK`); an
    /// input's, its one record and what its endpoint says it took. RFC 8446's Handshake length
    /// (a uint24 and the four-byte head) bounds each handshake.
    fn admitted(&self) -> (Admitted, Admissions) {
        let handshake_bytes = (1 << 24) - 1 + 4;
        let output = Admitted {
            record_bytes: self.capacity,
            transport_bytes: u64::try_from(self.capacity + wire::DATA_HEAD).unwrap(),
            handshake_bytes,
            datagram_bytes: wire::DATAGRAM_BYTES,
        };
        let chunk = shards_abi::run::CHUNK;
        let execs = Admissions {
            output: Admitted {
                record_bytes: chunk,
                transport_bytes: u64::try_from(chunk + wire::DATA_HEAD).unwrap(),
                ..output
            },
            input: Admitted {
                record_bytes: chunk,
                transport_bytes: u64::try_from(wire::input::TAKEN_BYTES).unwrap(),
                handshake_bytes,
                datagram_bytes: wire::DATAGRAM_BYTES,
            },
        };
        (output, execs)
    }

    fn scope(&self, member: u32, channel: Channel) -> Scope {
        Scope {
            run: self.run,
            generation: self.generation,
            member,
            channel,
        }
    }

    /// Dials the endpoint as the run's owner, subscribed to agent `a`'s stdout and stderr (the
    /// streams it owns, which also name the run its asks are of), with execs and so log lanes.
    fn dial(&self, owner: &Identity) -> (Controller, Dialed) {
        let (output, execs) = self.admitted();
        let outputs = [
            (self.scope(A, Channel::Stdout), output, 0),
            (self.scope(A, Channel::Stderr), output, 0),
        ];
        Controller::dial(
            owner,
            self.vm,
            self.address,
            Subscriptions {
                outputs: &outputs,
                inputs: &[],
                execs: Some(execs),
                sockets: None,
            },
        )
        .unwrap()
    }
}

impl Home {
    fn new(name: &str) -> Home {
        let home = common::TempDir::new(name);
        let owner = Identity::load_or_create(&home.join("identity")).unwrap();
        Home { home, owner }
    }

    /// `shards ARGS` in its home, waited on as long as it goes on (`common::run_command`):
    /// `_until`, its case's end, bounds it no more.
    fn command(&self, args: &[&str], _until: Instant) -> common::Run {
        common::run_shards_env(
            &[],
            args,
            &[
                ("SHARDS_HOME", self.home.as_os_str()),
                ("SHARDS_KERNEL", common::kernel().as_os_str()),
                ("SHARDS_INIT", common::guest_init().as_os_str()),
            ],
        )
    }

    /// Builds `tag`: the tests' own image (never a registry's beyond this host) with agents
    /// `a` and `b`, the test guest each, run with `a_options` and `b_options`.
    fn image(&self, tag: &str, a_options: &[&str], b_options: &[&str], until: Instant) {
        let context = common::TempDir::new("lanes-ctx");
        let (image, _) = common::served();
        for (name, options) in [("a", a_options), ("b", b_options)] {
            let dir = context.join(name);
            std::fs::create_dir(&dir).unwrap();
            std::fs::copy(common::test_guest(), dir.join("member")).unwrap();
            // A context's files keep their modes on a Unix host, as fsutil walks them; a
            // Windows host's context gives every file 0755 (tests/member_exec.rs).
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(dir.join("member"), std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            std::fs::write(
                dir.join("agent.json"),
                serde_json::to_vec(&serde_json::json!({"entrypoint":"member","options":options})).unwrap(),
            )
            .unwrap();
        }
        std::fs::write(
            context.join("Agentfile"),
            format!("FROM {image}\nAGENT a FROM ./a\nAGENT b FROM ./b\n"),
        )
        .unwrap();
        let built = self.command(&["build", "image", "-t", tag, context.to_str().unwrap()], until);
        assert_eq!(built.status, Some(0), "{built}");
    }

    /// Starts `image` detached as `name`, its agent-control endpoint asked for and agent `a`
    /// owned, with `options` beside, its command `command`.
    fn run(&self, name: &str, options: &[&str], image: &str, command: &[&str], until: Instant) -> Started {
        let mut args = vec![
            "run",
            "vm",
            "-d",
            "--name",
            name,
            "--annotation",
            "shards.control=true",
            "--annotation",
            "shards.output=agent:a",
        ];
        args.extend_from_slice(options);
        args.push(image);
        args.extend_from_slice(command);
        let started = self.command(&args, until);
        assert_eq!(started.status, Some(0), "{started}");
        let id = started.stdout.trim();
        assert!(
            !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "{started}"
        );
        let dir = self.home.join("containers").join(id);
        let control = Control::read(&dir.join("managed-output"), self.owner.fingerprint());
        Started { dir, control }
    }

    /// Stops its daemon, which ends its runs, and checks it let go of its home.
    fn stop(&self, until: Instant) -> Result<(), String> {
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.command(&["stop", "daemon"], until)
        }));
        match stopped {
            Ok(stopped) if stopped.status != Some(0) => Err(format!("stop daemon: {stopped}")),
            Ok(_) if !daemon_gone(&self.home) => {
                Err("`stop daemon` returned while its daemon still held its home's lock".into())
            }
            Ok(_) => Ok(()),
            Err(_) => Err("the stop's CLI helper panicked".into()),
        }
    }

    /// Runs `test` on this home, then stops its daemon whatever happened; a failure of the
    /// test is the one reported, the stop's beside it.
    fn with(name: &str, test: impl FnOnce(&Home)) {
        if common::cannot_run_vms() {
            return;
        }
        let home = Home::new(name);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test(&home)));
        let stopped = home.stop(Instant::now() + WITHIN);
        if let Err(error) = result {
            let _ = writeln!(std::io::stderr(), "log lanes cleanup: {stopped:?}");
            std::panic::resume_unwind(error);
        }
        stopped.unwrap();
    }
}

/// Drives a dialed controller while `body` runs with its execs and agent `a`'s outputs, then
/// hangs it up. Should `until` pass first, it is hung up then, so that every read of its lanes
/// ends with the connection's end and the test fails at that read rather than waiting on.
fn controlled<T>(
    controller: Controller,
    dialed: Dialed,
    until: Instant,
    body: impl FnOnce(&Execs, Vec<Output>) -> T,
) -> T {
    let Dialed {
        outputs,
        execs,
        hangup,
        ..
    } = dialed;
    let execs = execs.unwrap();
    let (done, finished) = mpsc::channel::<()>();
    std::thread::scope(|scope| {
        let driver = scope.spawn(move || controller.run());
        scope.spawn(move || {
            let left = until.saturating_duration_since(Instant::now());
            if let Err(mpsc::RecvTimeoutError::Timeout) = finished.recv_timeout(left) {
                let _ = writeln!(
                    std::io::stderr(),
                    "log lanes: the test's patience passed; its controller hung up"
                );
            }
            drop(hangup);
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(&execs, outputs)));
        drop(done);
        drop(execs);
        let _ = driver.join();
        result.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

/// An owned output read to its end: its data's bytes. Its endpoint carries the guest's capture
/// records whole (`run::managed::Record`, as every owned output's reader decodes them), each at
/// the wire offset its last left, held to its stream's transitions, the last its end.
fn drained(mut output: Output) -> Vec<u8> {
    let mut wire = 0u64;
    let mut state = StreamState::New;
    let mut bytes = Vec::new();
    loop {
        match output.next_record() {
            Ok(Got::Record { at, bytes: record }) => {
                assert_eq!(at, wire, "an owned output's record out of place");
                wire += record.len() as u64;
                let record = Record::decode(&record).unwrap();
                state.accept(record).unwrap();
                match record.event {
                    CaptureEvent::Data(data) => bytes.extend_from_slice(data),
                    CaptureEvent::Open(_) | CaptureEvent::End => {}
                    CaptureEvent::Aborted(errno) => panic!("an owned output aborted: errno {errno}"),
                }
            }
            Ok(Got::Finished) => {
                assert!(state.terminal(), "an owned output finished before its end record");
                return bytes;
            }
            Err(fault) => panic!("an owned output ended without its end: {fault:?}"),
        }
    }
}

fn channel_at(channel: Channel) -> usize {
    match channel {
        Channel::Stdout => 0,
        Channel::Stderr => 1,
    }
}

/// What a log lane read: each channel's bytes, its records checked at their places as they
/// came, and the places it was told its retention let go to.
#[derive(Default)]
struct Read {
    bytes: [Vec<u8>; 2],
    last: [Option<(LogPos, u64)>; 2],
    rotated: Vec<LogPos>,
}

impl Read {
    /// Takes `record` of a lane asked from `from`. A channel's first record is at its place in
    /// `from` or past it, or a record's start where it was asked from the oldest; each next one
    /// goes on inside its record, where its last stopped, or starts a later record.
    fn take(&mut self, record: &LogRecord, from: LogCursor) {
        match record {
            LogRecord::Output {
                channel, pos, bytes, ..
            } => {
                let at = channel_at(*channel);
                match self.last[at] {
                    Some((last, len)) if (pos.seq, pos.no) == (last.seq, last.no) => {
                        assert_eq!(Some(*pos), last.after(len), "a record's next piece out of place");
                    }
                    Some((last, _)) => assert!(
                        (pos.seq, pos.no) > (last.seq, last.no) && pos.byte == 0,
                        "a record not at its place: {pos:?} after {last:?}"
                    ),
                    None => match from.of(*channel) {
                        Some(start) => assert!(
                            *pos >= start,
                            "a record before its lane's place: {pos:?} < {start:?}"
                        ),
                        None => assert_eq!(pos.byte, 0, "a lane from the oldest began inside a record"),
                    },
                }
                self.last[at] = Some((*pos, bytes.len() as u64));
                self.bytes[at].extend_from_slice(bytes);
            }
            LogRecord::Gap { .. } => panic!("a gap in a log that lost nothing: {record:?}"),
            LogRecord::Rotated { oldest } => self.rotated.push(*oldest),
        }
    }
}

/// A lane read to its end, from where it was asked, `from`.
fn read_whole(log: &mut Log, from: LogCursor) -> Read {
    let mut read = Read::default();
    loop {
        match log.next_record() {
            Ok(Logged::Record(record)) => read.take(&record, from),
            Ok(Logged::Finished) => return read,
            Err(fault) => panic!("a log lane ended without its end: {fault:?}"),
        }
    }
}

/// What `managed-capture NAME` writes (the test guest's `managed_capture_bytes`): stdout's,
/// then stderr's.
fn written(name: &str) -> [Vec<u8>; 2] {
    [payload(name, Channel::Stdout), payload(name, Channel::Stderr)]
}

/// One run, its command and two agents writing what the test guest's `managed-capture` writes,
/// `a` owned by its controller and `b` drained:
/// 1. the command's log read whole from the oldest, each record at its place;
/// 2. each agent's log followed from the oldest, read as written and ended with its channels;
/// 3. a lane from where another got to reads only what came after;
/// 4. a member the run lacks refused `Unknown`;
/// 5. `shards read logs` shows each agent's lines after its label.
#[test]
fn log_lanes_read_a_runs_command_and_each_agents_log_at_its_place_and_go_on_from_a_cursor() {
    Home::with("lanes", |home| {
        let until = Instant::now() + WITHIN;
        home.image(
            "lanes:1",
            &["managed-capture", "a", "exit"],
            &["managed-capture", "b", "exit"],
            until,
        );
        let run = home.run("lanes", &[], "lanes:1", &["managed-capture", "cmd"], until);
        let (controller, dialed) = run.control.dial(&home.owner);
        controlled(controller, dialed, until, |execs, outputs| {
            std::thread::scope(|scope| {
                // Agent `a`'s output is its owner's to read: its channels end as it is read.
                let owned: Vec<_> = outputs
                    .into_iter()
                    .map(|output| scope.spawn(move || drained(output)))
                    .collect();

                // The command's log, followed from the oldest until all it writes is there: its
                // cursor after its first record is where the lane of step 3 goes on.
                let command = written("cmd");
                let mut lane = execs
                    .log(LogSource::Command, LogChannels::BOTH, LogCursor::OLDEST, true)
                    .unwrap();
                let mut followed = Read::default();
                let mut first = None;
                while followed.bytes != command {
                    match lane.next_record() {
                        Ok(Logged::Record(record)) => {
                            followed.take(&record, LogCursor::OLDEST);
                            first.get_or_insert((record, lane.cursor()));
                        }
                        other => panic!("the command's followed log ended before its output: {other:?}"),
                    }
                    for (had, all) in followed.bytes.iter().zip(&command) {
                        assert!(
                            all.starts_with(had),
                            "the command's log holds what it did not write"
                        );
                    }
                }
                drop(lane);
                let (first, mid) = first.unwrap();

                // 1. Read whole, unfollowed, from the oldest: every byte, each at its place.
                let whole = read_whole(
                    &mut execs
                        .log(LogSource::Command, LogChannels::BOTH, LogCursor::OLDEST, false)
                        .unwrap(),
                    LogCursor::OLDEST,
                );
                assert_eq!(whole.bytes, command, "the command's log read whole");
                assert!(whole.rotated.is_empty(), "{:?}", whole.rotated);

                // 3. From the first lane's cursor: only what came after its first record.
                let LogRecord::Output {
                    channel, bytes: had, ..
                } = &first
                else {
                    panic!("the command's log began with no output: {first:?}");
                };
                let mut after = command.clone();
                after[channel_at(*channel)].drain(..had.len());
                let rest = read_whole(
                    &mut execs
                        .log(LogSource::Command, LogChannels::BOTH, mid, false)
                        .unwrap(),
                    mid,
                );
                assert_eq!(rest.bytes, after, "a lane from a cursor read what came before it");

                // 2. Each agent's log, followed from the oldest: as it wrote it, ended with its
                // channels, the owned one's as the drained one's.
                for (member, name) in [(B, "b"), (A, "a")] {
                    let read = read_whole(
                        &mut execs
                            .log(
                                LogSource::Member(member),
                                LogChannels::BOTH,
                                LogCursor::OLDEST,
                                true,
                            )
                            .unwrap(),
                        LogCursor::OLDEST,
                    );
                    assert_eq!(read.bytes, written(name), "agent {name}'s log");
                    assert!(read.rotated.is_empty(), "{:?}", read.rotated);
                }
                let owned: Vec<_> = owned.into_iter().map(|reader| reader.join().unwrap()).collect();
                assert_eq!(owned, written("a"), "agent a's owned output");

                // 3, again: past every record of a lane read to its end, nothing.
                let mut lane = execs
                    .log(LogSource::Member(A), LogChannels::BOTH, LogCursor::OLDEST, false)
                    .unwrap();
                read_whole(&mut lane, LogCursor::OLDEST);
                let end = lane.cursor();
                drop(lane);
                let nothing = read_whole(
                    &mut execs
                        .log(LogSource::Member(A), LogChannels::BOTH, end, false)
                        .unwrap(),
                    end,
                );
                assert_eq!(
                    nothing.bytes,
                    [Vec::<u8>::new(), Vec::new()],
                    "a lane past its log's end"
                );

                // 4. A member the run lacks: the first index past its agents.
                assert_eq!(
                    execs
                        .log(
                            LogSource::Member(B + 1),
                            LogChannels::BOTH,
                            LogCursor::OLDEST,
                            false
                        )
                        .map(|_| ()),
                    Err(Fault::Unlogged(Unlogged::Unknown))
                );
            });
        });

        // 5. `shards read logs`: the command's own, then each agent's lines after its label,
        // the owned agent's as the drained one's.
        let logs = home.command(&["read", "logs", "lanes"], until);
        assert_eq!(logs.status, Some(0), "{logs}");
        let shown = format!("{}{}", logs.stdout, logs.stderr);
        for line in [
            "cmd\0stdout\n",
            "cmd\0stderr\n",
            "[agent a] a\0stdout\n",
            "[agent a] a\0stderr\n",
            "[agent b] b\0stdout\n",
            "[agent b] b\0stderr\n",
        ] {
            assert!(shown.contains(line), "`read logs` showed no {line:?}:\n{logs}");
        }
    });
}

/// A run that keeps no log refuses every log lane `Unkept`; one whose driver ships its log and
/// keeps no copy (`cache-disabled=true`) refuses its command's `Uncached`, as `logs` refuses
/// both.
#[test]
fn log_lanes_are_refused_typed_where_a_run_keeps_no_log_or_no_copy() {
    Home::with("unlogged", |home| {
        let until = Instant::now() + WITHIN;
        home.image(
            "unlogged:1",
            &["managed-capture", "a", "exit"],
            &["managed-capture", "b", "exit"],
            until,
        );
        // Where the shipped log goes: a socket of the test's, on loopback, that nothing reads.
        let syslog = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = format!("syslog-address=udp://{}", syslog.local_addr().unwrap());
        // Each run: its name, its log options, and each source's refusal.
        type Case<'a> = (&'a str, Vec<&'a str>, &'a [(LogSource, Unlogged)]);
        let cases: [Case<'_>; 2] = [
            (
                "unkept",
                vec!["--log-driver", "none"],
                &[
                    (LogSource::Command, Unlogged::Unkept),
                    (LogSource::Member(A), Unlogged::Unkept),
                    (LogSource::Member(B), Unlogged::Unkept),
                ],
            ),
            (
                "uncached",
                vec![
                    "--log-driver",
                    "syslog",
                    "--log-opt",
                    address.as_str(),
                    "--log-opt",
                    "cache-disabled=true",
                ],
                &[(LogSource::Command, Unlogged::Uncached)],
            ),
        ];
        for (name, options, refused) in cases {
            let run = home.run(name, &options, "unlogged:1", &["managed-capture", "cmd"], until);
            let (controller, dialed) = run.control.dial(&home.owner);
            controlled(controller, dialed, until, |execs, _| {
                for (source, why) in refused {
                    for follow in [false, true] {
                        assert_eq!(
                            execs
                                .log(*source, LogChannels::BOTH, LogCursor::OLDEST, follow)
                                .map(|_| ()),
                            Err(Fault::Unlogged(*why)),
                            "{name}: {source:?}, followed {follow}"
                        );
                    }
                }
            });
        }
    });
}

/// The run's one log cap (D240): a run whose `--log-opt max-size` and `max-file` its agents
/// write past.
/// - `logs` says what the cap let go of whose log;
/// - a lane of a log it cut, from the oldest, begins `Rotated` with the oldest place kept, and
///   one asked from a place before that is refused `Rotated`, naming it;
/// - the run's logs hold no more output than its bound and one segment.
#[test]
fn the_runs_one_log_cap_is_said_by_logs_and_by_a_lane_from_the_oldest_and_holds_its_bound() {
    // A segment of a guest's output frame (`run::CHUNK`), so that a frame the daemon keeps fills
    // one segment at most; two files, the fewest a log keeps that it rotates; and each agent
    // writing a segment past that bound alone, so that the two together pass it as many times.
    let segment = shards_abi::run::CHUNK as u64;
    let files = 2u64;
    let bound = segment * files;
    let each = bound + segment;
    Home::with("capped", |home| {
        let until = Instant::now() + WITHIN;
        let (a_bytes, b_bytes) = (each.to_string(), each.to_string());
        home.image(
            "capped:1",
            &["bulk", a_bytes.as_str(), "1"],
            &["bulk", b_bytes.as_str(), "2"],
            until,
        );
        let (size, count) = (format!("max-size={segment}"), format!("max-file={files}"));
        let run = home.run(
            "capped",
            &[
                "--log-opt",
                size.as_str(),
                "--log-opt",
                count.as_str(),
                "--log-opt",
                "compress=false",
            ],
            "capped:1",
            &["managed-capture", "cmd"],
            until,
        );
        let (controller, dialed) = run.control.dial(&home.owner);
        let capped = controlled(controller, dialed, until, |execs, outputs| {
            // Both agents' output written whole: `a`'s read by its owner to its end, `b`'s
            // followed in its log to its channels' end.
            let owned: usize = std::thread::scope(|scope| {
                let readers: Vec<_> = outputs
                    .into_iter()
                    .map(|output| scope.spawn(move || drained(output).len()))
                    .collect();
                let mut lane = execs
                    .log(LogSource::Member(B), LogChannels::BOTH, LogCursor::OLDEST, true)
                    .unwrap();
                loop {
                    match lane.next_record() {
                        Ok(Logged::Record(_)) => {}
                        Ok(Logged::Finished) => break,
                        Err(fault) => panic!("agent b's followed log ended without its end: {fault:?}"),
                    }
                }
                readers.into_iter().map(|reader| reader.join().unwrap()).sum()
            });
            assert_eq!(owned as u64, each, "agent a's owned output");

            // Whose logs the cap cut, by its record of each (`log.capped`).
            let capped: Vec<u32> = [A, B]
                .into_iter()
                .filter(|member| {
                    run.dir
                        .join("members")
                        .join(member.to_string())
                        .join("log.capped")
                        .exists()
                })
                .collect();
            assert!(
                !capped.is_empty(),
                "agents writing {} bytes past a bound of {bound} lost none of it to the cap",
                2 * each
            );
            for &member in &capped {
                let mut lane = execs
                    .log(
                        LogSource::Member(member),
                        LogChannels::BOTH,
                        LogCursor::OLDEST,
                        false,
                    )
                    .unwrap();
                let Ok(Logged::Record(LogRecord::Rotated { oldest })) = lane.next_record() else {
                    panic!("member {member}'s lane from the oldest did not begin with what the cap let go");
                };
                drop(lane);
                let before = LogCursor {
                    stdout: Some(LogPos::default()),
                    stderr: Some(LogPos::default()),
                };
                assert_eq!(
                    execs
                        .log(LogSource::Member(member), LogChannels::BOTH, before, false)
                        .map(|_| ()),
                    Err(Fault::Unlogged(Unlogged::Rotated { oldest })),
                    "member {member}: a place before the oldest kept"
                );
            }
            capped
        });

        // The output its logs hold, as the cap weighs it: each segment's log as written, its
        // index not output; none compressed, as the run's options say.
        let mut total = 0u64;
        let dirs = std::iter::once(run.dir.clone())
            .chain([A, B].map(|m| run.dir.join("members").join(m.to_string())));
        for dir in dirs {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy().into_owned();
                assert!(
                    !name.ends_with(".zst"),
                    "{}: compressed against its options",
                    dir.display()
                );
                let segment = name == "log"
                    || name
                        .strip_prefix("log.")
                        .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()));
                if segment {
                    total += entry.metadata().unwrap().len();
                }
            }
        }
        assert!(
            total <= bound + segment,
            "the run's logs hold {total} bytes of output, past its bound {bound} and one segment {segment}"
        );

        // `logs` says the cap's losses first, each of whose log it cut.
        let logs = home.command(&["read", "logs", "capped"], until);
        assert_eq!(logs.status, Some(0), "{}", logs.stderr);
        for member in capped {
            let whose = format!(
                "shards: capped: [agent {}]'s older output went to its run's one log cap",
                if member == A { "a" } else { "b" }
            );
            assert!(
                logs.stderr.contains(&whose),
                "no notice {whose:?}:\n{}",
                logs.stderr
            );
        }
    });
}
