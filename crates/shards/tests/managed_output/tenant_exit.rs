//! Coordinator-only normal member completion with causally later neighbor output.
use super::*;
use serde_json::{Value, json};
use std::io::Read as _;

struct Inputs {
    port: u16,
    patience: Duration,
    records: PathBuf,
}

impl Inputs {
    fn supplied() -> Self {
        let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("supply {name}"));
        let port = required("SHARDS_TENANT_EXIT_PORT").parse().unwrap();
        let patience = Duration::from_millis(required("SHARDS_TENANT_EXIT_DEADLINE_MS").parse().unwrap());
        assert_ne!(port, 0, "explicit guest fixture port required");
        assert!(
            !patience.is_zero(),
            "explicit positive finite phase deadline required"
        );
        assert!(
            Instant::now().checked_add(patience).is_some(),
            "representable deadline"
        );
        let records = PathBuf::from(required("SHARDS_TENANT_EXIT_RECORDS"));
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

#[derive(Clone, Copy)]
enum ReadGoal {
    CanonicalEnd,
    InitialPayload,
    FinalPayload,
}

impl Capture {
    fn receive(&mut self, observer: &Observer, expected: &[u8], goal: ReadGoal, until: Instant) {
        loop {
            let Event::Record(offset, chunk) = observer.next(until) else {
                panic!("expected canonical member record before terminal");
            };
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
                CaptureEvent::Open(label) => assert_eq!(label, declared_label(observer.scope.member)),
                CaptureEvent::Data(bytes) => {
                    self.bytes.extend_from_slice(bytes);
                    assert!(expected.starts_with(&self.bytes), "exact controlled member bytes");
                }
                CaptureEvent::End => {
                    assert!(
                        matches!(goal, ReadGoal::CanonicalEnd),
                        "continuing neighbor ended before owner stop"
                    );
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
            if matches!(goal, ReadGoal::FinalPayload) && self.bytes == expected {
                // ack_and_finish owns this final record's single ACK and FIN handoff.
                return;
            }
            observer
                .sender
                .send(Command::Handler(Ask::Ack(self.wire)))
                .unwrap();
            if matches!(goal, ReadGoal::CanonicalEnd) && self.state.terminal() {
                assert!(
                    matches!(observer.next(until), Event::End),
                    "transport EOF after consumed End"
                );
                return;
            }
            if matches!(goal, ReadGoal::InitialPayload) && self.bytes == expected {
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
    let port = inputs.port.to_string();
    let deadline = inputs.patience.as_millis().to_string();
    // Existing declared NETWORK/EXPOSE grammar, also used by managed_limits.
    let directives = format!(
        "NETWORK --ingress={port} control\nEXPOSE --harnesses=driver {port} AS ingress FOR control\nCONNECT driver WITH driver ON control\n"
    );
    fixture.start_with_options(
        &["managed-capture", "agent", "exit"],
        &["managed-capture", "harness", "step", &port, &deadline],
        &directives,
        Some(inputs.port),
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
        capture.receive(
            &fixture.observers[*index],
            &payload(if *member == 1 { "agent" } else { "harness" }, *channel),
            if *member == 1 {
                ReadGoal::CanonicalEnd
            } else {
                ReadGoal::InitialPayload
            },
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
            capture.receive(
                &fixture.observers[*index],
                &expected,
                ReadGoal::FinalPayload,
                until,
            );
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
