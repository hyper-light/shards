//! Coordinator-only normal member completion with causally later neighbor output.
use super::*;
use serde_json::{Value, json};
use std::io::Read as _;

#[path = "tenant_reconnect.rs"]
mod reconnect;

struct Inputs {
    port: u16,
    patience: Duration,
    records: PathBuf,
}

impl Inputs {
    fn supplied() -> Self {
        Self::supplied_for("SHARDS_TENANT_EXIT")
    }

    fn supplied_for(prefix: &str) -> Self {
        let required = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("supply {name}"));
        let port = required(&format!("{prefix}_PORT")).parse().unwrap();
        let patience = Duration::from_millis(required(&format!("{prefix}_DEADLINE_MS")).parse().unwrap());
        assert_ne!(port, 0, "explicit guest fixture port required");
        assert!(
            !patience.is_zero(),
            "explicit positive finite phase deadline required"
        );
        assert!(
            Instant::now().checked_add(patience).is_some(),
            "representable deadline"
        );
        let records = PathBuf::from(required(&format!("{prefix}_RECORDS")));
        assert!(records.is_absolute(), "absolute new evidence directory required");
        std::fs::create_dir(&records).unwrap();
        Self {
            port,
            patience,
            records,
        }
    }

    fn write(&self, name: &str, value: &Value) {
        std::fs::write(self.records.join(name), serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }

    fn command(&self, fixture: &Fixture, name: &str, args: &[&str], until: Instant) -> common::Run {
        let result = fixture.command(args, until.saturating_duration_since(Instant::now()));
        self.write(
            name,
            &json!({"args":args,"status":result.status,"stdout":result.stdout,"stderr":result.stderr}),
        );
        assert_eq!(result.status, Some(0), "{result}");
        result
    }
}

#[derive(Default)]
struct Capture {
    wire: u64,
    bytes: Vec<u8>,
    state: StreamState,
}

impl Capture {
    fn receive(
        &mut self,
        observer: &Observer,
        expected: &[u8],
        terminal: bool,
        inputs: &Inputs,
        until: Instant,
    ) {
        loop {
            let Event::Record(offset, chunk) = observer.next(until) else {
                panic!("expected canonical member record before terminal");
            };
            inputs.write(
                &format!(
                    "wire-{}-{}-{offset}.json",
                    observer.scope.member, observer.scope.channel as u8
                ),
                &json!({"offset":offset,"raw":chunk}),
            );
            assert!(!chunk.is_empty(), "empty member record");
            assert_eq!(offset, self.wire, "contiguous transport offset");
            self.wire = self
                .wire
                .checked_add(u64::try_from(chunk.len()).unwrap())
                .unwrap();
            let record = Record::decode(&chunk).unwrap();
            assert_eq!(record.member, observer.scope.member, "stable declared member");
            assert_eq!(
                record.channel, observer.scope.channel,
                "separate fd1/fd2 identity"
            );
            self.state.accept(record).unwrap();
            match record.event {
                CaptureEvent::Open(label) => {
                    assert_eq!(label, declared_label(observer.scope.member))
                }
                CaptureEvent::Data(bytes) => {
                    self.bytes.extend_from_slice(bytes);
                    assert!(expected.starts_with(&self.bytes), "exact controlled member bytes");
                }
                CaptureEvent::End => {
                    assert!(terminal, "continuing neighbor ended before owner stop");
                    assert_eq!(self.bytes, expected);
                    assert_eq!(
                        self.state,
                        StreamState::Ended {
                            offset: u64::try_from(expected.len()).unwrap()
                        },
                        "EOF is at the exact payload tail"
                    );
                }
                CaptureEvent::Aborted(errno) => panic!("normal member completion aborted: {errno}"),
            }
            observer
                .sender
                .send(Command::Handler(Ask::Ack(self.wire)))
                .unwrap();
            if terminal && self.state.terminal() {
                assert!(
                    matches!(observer.next(until), Event::End),
                    "transport EOF after consumed End"
                );
                return;
            }
            if !terminal && self.bytes == expected {
                return;
            }
        }
    }

    fn record(&self, member: u32, channel: Channel) -> Value {
        json!({"member":member,"channel":channel as u8,"wire_consumed":self.wire,"payload_offset":self.bytes.len(),"payload_hex":self.bytes.iter().map(|b|format!("{b:02x}")).collect::<String>(),"state":format!("{:?}",self.state)})
    }
}

fn release_neighbor(fixture: &Fixture, inputs: &Inputs, until: Instant) {
    let ports = inputs.command(fixture, "ports.json", &["read", "ports", "owner-output"], until);
    let endpoint = ports
        .stdout
        .lines()
        .find(|line| line.starts_with(&format!("{}/tcp", inputs.port)))
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_else(|| panic!("published owner fixture gate: {ports}"));
    let port = endpoint.rsplit(':').next().unwrap().parse().unwrap();
    let left = until.saturating_duration_since(Instant::now());
    assert!(!left.is_zero(), "owner release before phase deadline");
    let mut release =
        std::net::TcpStream::connect_timeout(&SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)), left)
            .unwrap();
    release
        .set_write_timeout(Some(until.saturating_duration_since(Instant::now())))
        .unwrap();
    release.write_all(&[1]).unwrap();
    release.shutdown(std::net::Shutdown::Write).unwrap();
    release
        .set_read_timeout(Some(until.saturating_duration_since(Instant::now())))
        .unwrap();
    let mut byte = [0];
    assert_eq!(
        release.read(&mut byte).unwrap(),
        0,
        "guest releases its gate connection"
    );
    inputs.write("release.json", &json!({"state":"OwnerReleasedNeighborAfterBothMember1Eofs","guest_port":inputs.port,"release_bytes":[1],"gate_peer_eof":true}));
}

fn run(fixture: &mut Fixture, inputs: &Inputs, until: Instant) -> Vec<(u32, Channel, Capture)> {
    run_member(
        fixture,
        inputs,
        until,
        &["managed-capture", "agent", "exit"],
        None,
        [
            payload("agent", Channel::Stdout),
            payload("agent", Channel::Stderr),
        ],
    )
}

fn run_member(
    fixture: &mut Fixture,
    inputs: &Inputs,
    until: Instant,
    agent: &[&str],
    agent_tasks: Option<u32>,
    expected: [Vec<u8>; 2],
) -> Vec<(u32, Channel, Capture)> {
    let port = inputs.port.to_string();
    let deadline = inputs.patience.as_millis().to_string();
    // Existing declared NETWORK/EXPOSE grammar, also used by managed_limits.
    let directives = format!(
        "NETWORK --ingress={port} control\nEXPOSE --harnesses=driver {port} AS ingress FOR control\nCONNECT driver WITH driver ON control\n"
    );
    fixture.start_with_options(
        agent,
        &["managed-capture", "harness", "step", &port, &deadline],
        &directives,
        agent_tasks,
        until,
    );
    std::fs::copy(
        fixture.context.join("Agentfile"),
        inputs.records.join("Agentfile"),
    )
    .unwrap();
    std::fs::copy(
        fixture.record.as_ref().unwrap(),
        inputs.records.join("managed-output.ready"),
    )
    .unwrap();
    let mut channels: Vec<_> = [
        (1, Channel::Stdout),
        (1, Channel::Stderr),
        (2, Channel::Stdout),
        (2, Channel::Stderr),
    ]
    .into_iter()
    .map(|(member, channel)| {
        (
            member,
            channel,
            fixture.observe(member, channel, None, false, until),
            Capture::default(),
        )
    })
    .collect();
    for (member, channel, index, capture) in &mut channels {
        let harness_payload = payload("harness", *channel);
        capture.receive(
            &fixture.observers[*index],
            if *member == 1 {
                match channel {
                    Channel::Stdout => &expected[0],
                    Channel::Stderr => &expected[1],
                }
            } else {
                &harness_payload
            },
            *member == 1,
            inputs,
            until,
        );
        if *member == 1 {
            // End was ACKed before transport EOF. Finished cursors do not accept
            // another consumed control; ordinary observer cancel releases this client.
            fixture.observers[*index]
                .sender
                .send(Command::Handler(Ask::Cancel))
                .unwrap();
            fixture.observers[*index].join(until).unwrap();
        }
    }
    inputs.write("member-ended.json", &json!({"state":"Member1CanonicalEndAndTransportEofOnBothChannels","channels":channels.iter().map(|(m,c,_,capture)|capture.record(*m,*c)).collect::<Vec<_>>(),"member0":"inactive files-only declaration remains absent; runnable IDs1/2 checked by Fixture::start_with_options"}));
    std::fs::copy(
        fixture.record.as_ref().unwrap(),
        inputs.records.join("managed-output.member-ended"),
    )
    .unwrap();
    let before = inputs.command(
        fixture,
        "processes-after-member1.json",
        &["top", "owner-output", "-o", "pid,ppid,uid,nlwp,stat,comm"],
        until,
    );
    let ended_uid = shards_abi::DOMAIN_FIRST_ID.checked_add(1).unwrap().to_string();
    assert!(
        before
            .stdout
            .lines()
            .skip(1)
            .all(|line| line.split_whitespace().nth(2) != Some(ended_uid.as_str())),
        "ended member has no visible actor rows: {before}"
    );
    fixture.main_progress(until.saturating_duration_since(Instant::now()));
    release_neighbor(fixture, inputs, until);
    for (member, channel, index, capture) in &mut channels {
        if *member == 2 {
            let expected = [
                payload("harness", *channel),
                payload("harness-after-exit", *channel),
            ]
            .concat();
            capture.receive(&fixture.observers[*index], &expected, false, inputs, until);
            fixture.observers[*index].ack_and_finish(capture.wire, until);
        }
        fixture
            .observed
            .insert((*member, *channel as u8), (capture.wire, capture.wire));
    }
    fixture.main_progress(until.saturating_duration_since(Instant::now()));
    inputs.write("neighbor-progress.json",&json!({"state":"ExactSiblingFd1Fd2ProducedAfterMember1BothEofAndIndependentMainExec","channels":channels.iter().map(|(m,c,_,capture)|capture.record(*m,*c)).collect::<Vec<_>>(),"guest_reaping":"visible actor rows absent only; wait status/full population remain coordinator-owed"}));
    std::fs::copy(
        fixture.record.as_ref().unwrap(),
        inputs.records.join("managed-output.before-stop"),
    )
    .unwrap();
    channels
        .into_iter()
        .map(|(member, channel, _, capture)| (member, channel, capture))
        .collect()
}

fn remove_run(fixture: &Fixture, inputs: &Inputs, until: Instant) -> String {
    inputs.command(
        fixture,
        "stop.json",
        &["stop", "vm", "-t", "0", "owner-output"],
        until,
    );
    let state = inputs.command(
        fixture,
        "stopped-state.json",
        &[
            "inspect",
            "vm",
            "-f",
            "{{.State.Running}} {{.State.Pid}}",
            "owner-output",
        ],
        until,
    );
    assert_eq!(state.stdout.trim(), "false 0", "{state}");
    let metadata = std::fs::read_to_string(fixture.record.as_ref().unwrap()).unwrap();
    std::fs::write(inputs.records.join("managed-output.stopped"), &metadata).unwrap();
    inputs.command(fixture, "remove.json", &["remove", "vm", "owner-output"], until);
    if let Some(record) = &fixture.record {
        assert!(
            matches!(std::fs::symlink_metadata(record), Err(e) if e.kind()==std::io::ErrorKind::NotFound),
            "private run metadata removed by daemon"
        );
    }
    inputs.write("run-cleanup.json", &json!({"state":"StoppedStatePid0AndPrivateRunMetadataRemoved","guest_reaping":"visible actor rows absent only; wait status/full population remain Unverified"}));
    metadata
}

fn assert_terminal_metadata(metadata: &str, channels: &[(u32, Channel, Capture)]) {
    for (member, channel, capture) in channels {
        let prefix = format!("member {member} channel {} state ", *channel as u8);
        let records: Vec<_> = metadata
            .lines()
            .filter_map(|line| line.strip_prefix(&prefix))
            .collect();
        let [terminal] = records.as_slice() else {
            panic!("exactly one terminal metadata record per channel");
        };
        let fields: Vec<_> = terminal.split_whitespace().collect();
        let [state, "accepted", accepted, "consumed", consumed, "tail", tail] = fields.as_slice() else {
            panic!("terminal metadata fields");
        };
        assert_eq!(accepted.parse::<u64>().unwrap(), capture.wire);
        assert_eq!(consumed.parse::<u64>().unwrap(), capture.wire);
        if *member == 1 {
            assert_eq!(
                (*state, *tail),
                ("ended", "complete"),
                "normal canonical EOF must remain complete across observer cancel and owner stop"
            );
        } else {
            assert_eq!(
                (*state, *tail),
                ("aborted", "unknown"),
                "continuing sibling ended by owner stop"
            );
        }
    }
}

#[test]
#[ignore = "coordinator-only real VM; explicit owner gate port/deadline/evidence directory"]
fn member_normal_exit_preserves_sibling_and_main_progress() {
    let inputs = Inputs::supplied();
    assert!(
        !common::cannot_run_vms(),
        "fixture requires a supported real VM host"
    );
    let until = Instant::now().checked_add(inputs.patience).unwrap();
    let artifact = |path: &std::path::Path| json!({"path":path,"sha256":common::sha256_digest(&std::fs::read(path).unwrap())});
    inputs.write("inputs.json",&json!({"port":inputs.port,"phase_deadline_ms":inputs.patience.as_millis(),"source":option_env!("SHARDS_TENANT_EXIT_SOURCE_REVISION"),"fixture":artifact(&std::env::current_exe().unwrap()),"cli":artifact(common::shards()),"testguest":artifact(common::test_guest()),"kernel":artifact(common::kernel()),"init":artifact(common::guest_init()),"topology":"EXPERIMENTAL2N; not selected final design"}));
    let mut fixture = Fixture::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut fixture, &inputs, until)));
    let cleanup_until = Instant::now().checked_add(inputs.patience).unwrap();
    let removed = result.as_ref().ok().map(|_| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            remove_run(&fixture, &inputs, cleanup_until)
        }))
    });
    let stopped = fixture.shutdown(cleanup_until);
    inputs.write("teardown.json",&json!({"owner_stop_and_observer_join":{"completed":stopped.is_ok(),"error":stopped.as_ref().err()},"protocol_completed":result.is_ok(),"private_run_cleanup_completed":matches!(&removed,Some(Ok(_))),"observer_fd_baseline":fixture.initial_fds,"host_exit_reaping_kernel_return":"Unverified; coordinator held-watches/independent cleanup campaign owed"}));
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
    stopped.unwrap();
    let metadata = match removed {
        Some(Ok(metadata)) => metadata,
        Some(Err(error)) => std::panic::resume_unwind(error),
        None => panic!("run cleanup not prepared"),
    };
    // Owned cleanup and durable evidence precede the strict terminal contract check.
    // A red retains the independent canonical End and transport EOF witnesses.
    assert_terminal_metadata(&metadata, &result.unwrap());
}

struct CrashInputs {
    count: u32,
    work_ms: u64,
    cleanup_ms: u64,
    report_bytes: u64,
    tasks: u32,
}

impl CrashInputs {
    fn supplied() -> Self {
        let number = |suffix| {
            std::env::var(format!("SHARDS_TENANT_CRASH_{suffix}"))
                .unwrap_or_else(|_| panic!("supply SHARDS_TENANT_CRASH_{suffix}"))
                .parse::<u64>()
                .unwrap()
        };
        let inputs = Self {
            count: u32::try_from(number("COUNT")).unwrap(),
            work_ms: number("WORK_MS"),
            cleanup_ms: number("CLEANUP_MS"),
            report_bytes: number("REPORT_BYTES"),
            tasks: u32::try_from(number("TASKS")).unwrap(),
        };
        assert!(
            inputs.count > 0 && inputs.tasks > 0,
            "positive u32 count and actual AGENT task grammar"
        );
        assert!(
            inputs.work_ms > 0 && inputs.cleanup_ms > 0,
            "explicit positive helper deadlines"
        );
        assert!(
            Instant::now()
                .checked_add(Duration::from_millis(inputs.work_ms))
                .and_then(|until| until.checked_add(Duration::from_millis(inputs.cleanup_ms)))
                .is_some(),
            "representable immutable helper work and cleanup deadlines"
        );
        // Each successful child contributes a pair and one fixed actual-wait report.
        // The longest round spelling bounds every round before allocating or starting a VM.
        let child = format!("agent-child-{}", inputs.count);
        let wait_bytes = b"\0W1\0".len()
            + size_of::<u32>()
            + size_of::<i32>()
            + 2 * (size_of::<u8>() + size_of::<i32>())
            + size_of::<u8>();
        let error_bytes = b"\0E1\0".len()
            + size_of::<u8>()
            + 2 * (2 * size_of::<u8>() + size_of::<i32>())
            + size_of::<u8>()
            + size_of::<i32>();
        let pair_len = |name| {
            payload(name, Channel::Stdout)
                .len()
                .checked_add(payload(name, Channel::Stderr).len())
                .unwrap()
        };
        let required = pair_len(&child)
            .checked_add(wait_bytes)
            .unwrap()
            .checked_mul(usize::try_from(inputs.count).unwrap())
            .unwrap()
            .checked_add(pair_len("agent"))
            .unwrap()
            .checked_add(pair_len("agent-after-crashes"))
            .unwrap()
            .checked_add(error_bytes.checked_mul(2).unwrap())
            .unwrap();
        assert!(
            u64::try_from(required).unwrap() <= inputs.report_bytes,
            "supplied controlled-byte envelope covers both channels and two failure reports"
        );
        inputs
    }

    fn expected(&self) -> [Vec<u8>; 2] {
        [Channel::Stdout, Channel::Stderr].map(|channel| {
            let mut bytes = Vec::new();
            bytes
                .try_reserve(usize::try_from(self.report_bytes).unwrap())
                .unwrap();
            bytes.extend(payload("agent", channel));
            for round in 0..self.count {
                bytes.extend(payload(&format!("agent-child-{round}"), channel));
                if channel == Channel::Stdout {
                    bytes.extend(b"\0W1\0");
                    bytes.extend(round.to_be_bytes());
                    // Linux wait status: signal with no core bit/normal exit byte.
                    bytes.extend(libc::SIGSEGV.to_be_bytes());
                    bytes.push(1);
                    bytes.extend(libc::SIGSEGV.to_be_bytes());
                    bytes.push(0);
                    bytes.extend(0i32.to_be_bytes());
                    bytes.push(0);
                }
            }
            bytes.extend(payload("agent-after-crashes", channel));
            bytes
        })
    }
}

fn preserve_history(fixture: &Fixture, inputs: &Inputs, phase: &str, until: Instant) {
    let destination = inputs.records.join(format!("history-{phase}"));
    std::fs::create_dir(&destination).unwrap();
    let copy = || -> std::io::Result<Vec<String>> {
        let parent = fixture
            .record
            .as_ref()
            .and_then(|p| p.parent())
            .ok_or(std::io::ErrorKind::NotFound)?;
        let mut names = Vec::new();
        for entry in std::fs::read_dir(parent)? {
            let entry = entry?;
            let name = entry.file_name();
            if name
                .to_str()
                .and_then(|n| n.strip_prefix("processes."))
                .and_then(|n| n.parse::<u64>().ok())
                .is_some()
            {
                if !entry.file_type()?.is_file() {
                    return Err(std::io::ErrorKind::InvalidData.into());
                }
                std::fs::copy(entry.path(), destination.join(&name))?;
                names.push(name.to_string_lossy().into_owned());
            }
        }
        Ok(names)
    };
    let segments = copy();
    // History is independent evidence, never authority or an assumed complete population.
    let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fixture.command(
            &["read", "processes", "owner-output"],
            until.saturating_duration_since(Instant::now()),
        )
    }));
    let shown = match read {
        Ok(run) => json!({"status":run.status,"stdout":run.stdout,"stderr":run.stderr}),
        Err(_) => json!({"state":"Unknown","cause":"read-processes helper panicked"}),
    };
    inputs.write(&format!("history-{phase}.json"), &json!({"read":shown,"segments":segments.as_ref().ok(),"copy_error":segments.as_ref().err().map(ToString::to_string),"completeness":"Unknown: rotation, unsupported/lost/short-lived records and omitted threads remain possible","correlation":"Only actual member/exec round-marker/fork/exit records may correlate; no namespace-to-VM PID mapping or exact output-writer claim"}));
}

#[test]
#[ignore = "coordinator-only real VM; explicit count/tasks/byte envelope/deadlines/port/evidence"]
fn owned_child_crashes_preserve_sibling_and_main_progress() {
    let crash = CrashInputs::supplied();
    let expected = crash.expected();
    let inputs = Inputs::supplied_for("SHARDS_TENANT_CRASH");
    assert!(
        !common::cannot_run_vms(),
        "fixture requires a supported real VM host"
    );
    let until = Instant::now().checked_add(inputs.patience).unwrap();
    let cleanup_until = until
        .checked_add(Duration::from_millis(crash.cleanup_ms))
        .unwrap();
    let artifact = |path: &std::path::Path| json!({"path":path,"sha256":common::sha256_digest(&std::fs::read(path).unwrap())});
    inputs.write("artifacts.json", &json!({"fixture":artifact(&std::env::current_exe().unwrap()),"cli":artifact(common::shards()),"testguest":artifact(common::test_guest()),"kernel":artifact(common::kernel()),"init":artifact(common::guest_init())}));
    inputs.write("inputs.json", &json!({"count":crash.count,"tasks":crash.tasks,"controlled_report_bytes":crash.report_bytes,"helper_work_ms":crash.work_ms,"helper_cleanup_ms":crash.cleanup_ms,"host_work_ms":inputs.patience.as_millis(),"port":inputs.port,"source":option_env!("SHARDS_TENANT_CRASH_SOURCE_REVISION"),"population_bound":"at most one owned unreaped child; caller tasks are not hard aggregate/parent/kernel proof","provenance":"member/channel inherited bytes; child round claims are controlled-fixture data, actual wait results reported by fixture parent"}));
    let args = [
        crash.count.to_string(),
        crash.work_ms.to_string(),
        crash.cleanup_ms.to_string(),
        crash.report_bytes.to_string(),
    ];
    let options = [
        "managed-capture",
        "agent",
        "crashes",
        &args[0],
        &args[1],
        &args[2],
        &args[3],
    ];
    let mut fixture = Fixture::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let channels = run_member(
            &mut fixture,
            &inputs,
            until,
            &options,
            Some(crash.tasks),
            expected,
        );
        preserve_history(&fixture, &inputs, "after-progress", until);
        channels
    }));
    // Preserve any failing raw member bytes/metadata/history before owned teardown.
    if let Some(record) = &fixture.record {
        let _ = std::fs::copy(record, inputs.records.join("managed-output.before-cleanup"));
    }
    let removed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        inputs.command(
            &fixture,
            "stop-before-history.json",
            &["stop", "vm", "-t", "0", "owner-output"],
            cleanup_until,
        );
        preserve_history(&fixture, &inputs, "after-stop", cleanup_until);
        remove_run(&fixture, &inputs, cleanup_until)
    }));
    let stopped = fixture.shutdown(cleanup_until);
    inputs.write("teardown.json", &json!({"protocol_completed":result.is_ok(),"private_run_cleanup_completed":removed.is_ok(),"owner_stop_and_observer_join":{"completed":stopped.is_ok(),"error":stopped.as_ref().err()},"observer_fd_baseline":fixture.initial_fds,"host_exit_reaping_kernel_return":"Unverified; ROOT independent held-watch/population/resource evidence owed"}));
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
    stopped.unwrap();
    let metadata = removed.unwrap_or_else(|error| std::panic::resume_unwind(error));
    assert_terminal_metadata(&metadata, &result.unwrap());
}
