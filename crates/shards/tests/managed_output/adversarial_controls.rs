//! Assigned adversarial authenticated control cases; one fresh run per selection.
use super::*;

#[path = "adversarial_controls/contract.rs"]
mod contract;
use contract::{AckCase, ControlCase, Limits, Selection, TerminalCase};

#[path = "adversarial_controls/control_inputs.rs"]
mod control_inputs;
#[path = "adversarial_controls/native_terminal.rs"]
mod native_terminal;
#[path = "adversarial_controls/write_receipts.rs"]
mod write_receipts;

struct Supplied {
    base: Inputs,
    case: Selection,
    channel: Channel,
    limits: Limits,
    source: String,
}

impl Supplied {
    fn read() -> Self {
        let required = |key: &str| {
            std::env::var(format!("SHARDS_ADVERSARIAL_{key}"))
                .unwrap_or_else(|_| panic!("supply SHARDS_ADVERSARIAL_{key}"))
        };
        let number = |key: &str| required(key).parse::<u64>().unwrap();
        let case = Selection::parse(&required("CASE")).unwrap();
        let channel = match required("CHANNEL").as_str() {
            "stdout" => Channel::Stdout,
            "stderr" => Channel::Stderr,
            _ => panic!("select stdout or stderr"),
        };
        let port = u16::try_from(number("PORT")).unwrap();
        assert_ne!(port, 0, "explicit guest gate port");
        let limits = Limits {
            phase_ms: number("PHASE_MS"),
            work_ms: number("WORK_MS"),
            cleanup_ms: number("CLEANUP_MS"),
            outer_ms: number("OUTER_MS"),
        };
        let source = required("SOURCE_REVISION");
        assert!(source.len() == 40 && source.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let records = PathBuf::from(required("RECORDS"));
        assert!(records.is_absolute(), "new absolute records directory");
        std::fs::create_dir(&records).unwrap();
        Self {
            base: Inputs {
                port,
                patience: Duration::from_millis(limits.phase_ms),
                records,
            },
            case,
            channel,
            limits,
            source,
        }
    }
}

fn left(until: Instant) -> Duration {
    until
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .unwrap_or_else(|| panic!("original adversarial phase deadline"))
}

fn panic_text(error: &(dyn std::any::Any + Send)) -> &str {
    error
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| error.downcast_ref::<&str>().copied())
        .unwrap_or("non-string test panic")
}

struct Trace<'a> {
    directory: &'a std::path::Path,
    next: u64,
}

impl Trace<'_> {
    fn record(&mut self, observer: &Observer, phase: &str, event: &Event) {
        let index = self.next;
        self.next = index.checked_add(1).unwrap();
        let stem = format!(
            "{index}-{phase}-{}-{}",
            observer.scope.member, observer.scope.channel as u8
        );
        let value = match event {
            Event::Record(offset, bytes) => {
                std::fs::write(self.directory.join(format!("{stem}.bin")), bytes).unwrap();
                json!({"kind":"Record","wire_start":offset,"bytes":bytes.len()})
            }
            Event::Lost(code) => {
                json!({"kind":"Lost","code":code,"reason":code.and_then(|n|u32::try_from(n).ok()).and_then(|n|Reason::try_from(n).ok()).map(|reason|format!("{reason:?}"))})
            }
            Event::End => json!({"kind":"TransportEnd"}),
            Event::Failed(error) => json!({"kind":"Failed","error":error}),
        };
        write_json(self.directory, &format!("{stem}.json"), &value);
    }

    fn next(&mut self, observer: &Observer, phase: &str, until: Instant) -> Event {
        left(until);
        let event = observer.next(until);
        self.record(observer, phase, &event);
        event
    }
}

fn write_json(directory: &std::path::Path, name: &str, value: &Value) {
    std::fs::write(directory.join(name), serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn observed_record(
    observer: &Observer,
    state: &mut StreamState,
    wire: &mut u64,
    trace: &mut Trace<'_>,
    until: Instant,
) -> (u64, u64) {
    let Event::Record(start, bytes) = trace.next(observer, "target", until) else {
        panic!("real canonical target record required before invalid control");
    };
    assert_eq!(start, *wire, "contiguous observed wire cursor");
    let record = Record::decode(&bytes).unwrap();
    assert_eq!(
        (record.member, record.channel),
        (observer.scope.member, observer.scope.channel)
    );
    match record.event {
        CaptureEvent::Open(label) => assert_eq!(label, declared_label(record.member)),
        CaptureEvent::Data(data) => assert!(payload("agent", record.channel).starts_with(data)),
        other => panic!("target must still hold an unacknowledged nonterminal record: {other:?}"),
    }
    state.accept(record).unwrap();
    *wire = start.checked_add(u64::try_from(bytes.len()).unwrap()).unwrap();
    (start, *wire)
}

#[derive(Clone, Copy, Debug)]
enum ExpectedTarget {
    Held { accepted: u64, consumed: u64 },
    Ended { wire: u64 },
}

impl ExpectedTarget {
    fn cursors(self) -> (u64, u64) {
        match self {
            Self::Held { accepted, consumed } => (accepted, consumed),
            Self::Ended { wire } => (wire, wire),
        }
    }
}

struct Outcome {
    target: ExpectedTarget,
    other: Vec<(u32, Channel, Capture)>,
}

struct Attempt {
    target: ExpectedTarget,
    bytes: Vec<u8>,
    finish: bool,
    reason: Reason,
    command: Ask,
    receipts: Option<mpsc::Receiver<ControlReceipt>>,
}

impl Attempt {
    fn ack(target: ExpectedTarget, cursor: u64, finish: bool) -> Self {
        Self {
            target,
            bytes: managed::consumed(cursor).to_vec(),
            finish,
            reason: Reason::Protocol,
            command: if finish {
                Ask::AckAndFinish(cursor)
            } else {
                Ask::Ack(cursor)
            },
            receipts: None,
        }
    }

    fn raw(target: ExpectedTarget, case: ControlCase, wire: u64) -> Self {
        let (bytes, finish, reason) = case.wire(wire);
        let (sender, receipts) = mpsc::channel();
        Self {
            target,
            command: Ask::RawControl {
                bytes: bytes.clone(),
                finish,
                receipts: sender,
            },
            bytes,
            finish,
            reason,
            receipts: Some(receipts),
        }
    }
}

fn receipt_value(receipt: &ControlReceipt) -> Value {
    match receipt {
        ControlReceipt::Write { accepted, pending } => {
            json!({"kind":"Write","accepted":accepted,"pending":pending})
        }
        ControlReceipt::Read { bytes, ended } => json!({"kind":"Read","bytes":bytes,"ended":ended}),
        ControlReceipt::Finish { error } => json!({"kind":"Finish","error":error}),
        ControlReceipt::WriteError(error) => json!({"kind":"WriteError","error":error}),
    }
}

fn check_receipts(receipts: &[ControlReceipt], submitted: usize, finish: bool) {
    let mut writes = write_receipts::Writes::new(submitted).unwrap();
    let mut finishes = 0usize;
    for receipt in receipts {
        match receipt {
            ControlReceipt::Write { accepted, pending } => writes.accept(*accepted, *pending).unwrap(),
            ControlReceipt::Read { .. } => {}
            ControlReceipt::Finish { error } => {
                assert!(error.is_none(), "actual send-side FIN failure: {error:?}");
                finishes = finishes.checked_add(1).unwrap();
            }
            ControlReceipt::WriteError(error) => panic!("actual raw-control write failure: {error}"),
        }
    }
    assert_eq!(
        writes.complete().unwrap(),
        submitted,
        "all actual input bytes accepted"
    );
    assert_eq!(
        finishes,
        usize::from(finish),
        "exact requested send-side FIN receipt"
    );
}

fn main_progress(fixture: &Fixture, base: &Inputs, name: &str, until: Instant) {
    left(until);
    let progress = base.command(
        fixture,
        &format!("{name}.json"),
        &[
            "exec",
            "owner-output",
            "/bin/testguest",
            "streams",
            "main-progress",
            "main-error",
        ],
        until,
    );
    assert_eq!(progress.stdout, "main-progress\n", "{progress}");
    let stderr = progress
        .stderr
        .strip_prefix("main-error\n")
        .unwrap_or_else(|| panic!("independent main stderr"));
    assert!(
        stderr.is_empty() || stderr.starts_with("shards-timing "),
        "unexpected main diagnostic"
    );
    std::fs::write(
        base.records.join(format!("{name}.stdout.bin")),
        progress.stdout.as_bytes(),
    )
    .unwrap();
    std::fs::write(
        base.records.join(format!("{name}.stderr.bin")),
        progress.stderr.as_bytes(),
    )
    .unwrap();
}

fn run(fixture: &mut Fixture, input: &Supplied, until: Instant) -> Outcome {
    let base = &input.base;
    let port = base.port.to_string();
    let work = input.limits.work_ms.to_string();
    let directives = format!(
        "NETWORK --ingress={port} control\nEXPOSE --harnesses=driver {port} AS ingress FOR control\nCONNECT driver WITH driver ON control\n"
    );
    left(until);
    fixture.start_with_options(
        &["managed-capture", "agent", "exit"],
        &["managed-capture", "harness", "step", &port, &work],
        &directives,
        Some(&[base.port]),
        until,
    );
    std::fs::copy(fixture.context.join("Agentfile"), base.records.join("Agentfile")).unwrap();
    std::fs::copy(
        fixture.record.as_ref().unwrap(),
        base.records.join("managed-output.ready"),
    )
    .unwrap();
    let index = fixture.observe(1, input.channel, None, false, until);
    let observer = &fixture.observers[index];
    let mut trace = Trace {
        directory: &base.records,
        next: 0,
    };
    let mut state = StreamState::New;
    let mut wire = 0;
    let mut interval = observed_record(observer, &mut state, &mut wire, &mut trace, until);
    assert!(
        matches!(state, StreamState::Open { offset: 0 }),
        "first record is Open"
    );
    let attempt = match input.case {
        Selection::Ack(case) => {
            let last_valid_ack = if case == AckCase::Replay {
                observer
                    .sender
                    .send(Command::Handler(Ask::Ack(interval.1)))
                    .unwrap();
                let acknowledged = interval.1;
                interval = observed_record(observer, &mut state, &mut wire, &mut trace, until);
                assert!(matches!(state, StreamState::Open { offset } if offset > 0));
                acknowledged
            } else {
                0
            };
            let cursor = case.cursor(interval.0, interval.1, last_valid_ack).unwrap();
            Attempt::ack(
                ExpectedTarget::Held {
                    accepted: interval.1,
                    consumed: last_valid_ack,
                },
                cursor,
                false,
            )
        }
        Selection::Control(case) => Attempt::raw(
            ExpectedTarget::Held {
                accepted: wire,
                consumed: 0,
            },
            case,
            wire,
        ),
        Selection::Terminal(case) => {
            observer.sender.send(Command::Handler(Ask::Ack(wire))).unwrap();
            let mut capture = Capture {
                wire,
                state,
                trace: Some(base.records.clone()),
                ..Capture::default()
            };
            capture.receive(
                observer,
                &payload("agent", input.channel),
                ReadGoal::CanonicalEnd,
                until,
            );
            base.write("target-ended.json", &capture.record(1, input.channel));
            std::fs::copy(
                fixture.record.as_ref().unwrap(),
                base.records.join("managed-output.target-ended"),
            )
            .unwrap();
            Attempt::ack(
                ExpectedTarget::Ended { wire: capture.wire },
                case.cursor(capture.wire).unwrap(),
                case == TerminalCase::Finish,
            )
        }
    };
    let target = attempt.target;
    let (accepted, last_valid_ack) = target.cursors();
    std::fs::write(base.records.join("invalid-control.bin"), &attempt.bytes).unwrap();
    base.write("attempt.json", &json!({"case":format!("{:?}",input.case),"member":1,"channel":input.channel as u8,"observed_wire_start":interval.0,"observed_wire_end":interval.1,"observed_cursor_before_attempt":accepted,"expected_target":format!("{target:?}"),"last_valid_ack":last_valid_ack,"control_bytes":attempt.bytes.len(),"finish":attempt.finish,"expected_reason":format!("{:?}",attempt.reason),"authentication":"existing owner; actual record already observed","endpoint_reuse":false}));
    left(until);
    let enqueued = observer.sender.send(Command::Handler(attempt.command));
    base.write("enqueue.json", &json!({"accepted":enqueued.is_ok(),"error":enqueued.as_ref().err().map(ToString::to_string),"meaning":"command enqueue only; no assumed peer consumption"}));
    enqueued.unwrap();
    let refusal = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let event = trace.next(observer, "refusal", until);
        let Event::Lost(Some(code)) = event else {
            panic!("expected typed application protocol refusal, actual={event:?}");
        };
        assert_eq!(Reason::try_from(u32::try_from(code).unwrap()), Ok(attempt.reason));
    }));
    // Local cancellation is owned cleanup, not a remote connection-drain barrier.
    fixture.observers[index].sender.request_stop();
    let joined = fixture.observers[index].join(until);
    let remaining: Vec<_> = fixture.observers[index].events.try_iter().collect();
    for event in &remaining {
        trace.record(&fixture.observers[index], "after-join", event);
    }
    let receipts = attempt
        .receipts
        .as_ref()
        .map(|receiver| receiver.try_iter().collect::<Vec<_>>());
    if let Some(receipts) = &receipts {
        base.write("io-receipts.json", &json!({"actual":receipts.iter().map(receipt_value).collect::<Vec<_>>(),"driver_joined":joined.is_ok(),"submitted_bytes":attempt.bytes.len(),"finish_requested":attempt.finish,"meaning":"local transport write/read/FIN receipts; peer consumption proved only by actual typed refusal and final metadata"}));
    }
    base.write("refusal.json", &json!({"matched_expected_refusal":refusal.is_ok(),"assertion_failure":refusal.as_ref().err().map(|e|panic_text(e.as_ref())),"borrowed_driver_join":{"complete":joined.is_ok(),"error":joined.as_ref().err()},"remote_connection_drained":"Unknown; endpoint is never reused","post_join_records":remaining.iter().filter(|e|matches!(e,Event::Record(..))).count()}));
    if let Err(error) = refusal {
        std::panic::resume_unwind(error);
    }
    joined.unwrap();
    if let Some(receipts) = &receipts {
        check_receipts(receipts, attempt.bytes.len(), attempt.finish);
    }
    assert!(
        !remaining.iter().any(|event| matches!(event, Event::Record(..))),
        "no future target payload after invalid ACK"
    );
    main_progress(fixture, base, "main-after-refusal-raw", until);
    base.write(
        "main-after-refusal.json",
        &json!({"state":"ExactIndependentMainStdoutStderrAfterTypedRefusalAndJoin"}),
    );

    let other_channel = match input.channel {
        Channel::Stdout => Channel::Stderr,
        Channel::Stderr => Channel::Stdout,
    };
    let mut channels: Vec<_> = [(1, other_channel), (2, Channel::Stdout), (2, Channel::Stderr)]
        .into_iter()
        .map(|(member, channel)| {
            left(until);
            (
                member,
                channel,
                fixture.observe(member, channel, None, false, until),
                Capture {
                    trace: Some(base.records.clone()),
                    ..Capture::default()
                },
            )
        })
        .collect();
    for (member, channel, index, capture) in &mut channels {
        left(until);
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
            fixture.observers[*index]
                .sender
                .send(Command::Handler(Ask::Cancel))
                .unwrap();
            fixture.observers[*index].join(until).unwrap();
        }
    }
    base.write("before-release.json", &json!({"state":"OtherMember1ChannelEndAckEofAndNeighborBothInitialPayloads","channels":channels.iter().map(|(m,c,_,capture)|capture.record(*m,*c)).collect::<Vec<_>>()}));
    left(until);
    release_neighbor(fixture, base, until);
    for (member, channel, index, capture) in &mut channels {
        if *member == 2 {
            let expected = [
                payload("harness", *channel),
                payload("harness-after-exit", *channel),
            ]
            .concat();
            left(until);
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
    main_progress(fixture, base, "main-after-neighbor-raw", until);
    base.write("progress.json", &json!({"state":"ExactNewNeighborFd1Fd2AfterTypedRefusalAndGatePlusIndependentMain","channels":channels.iter().map(|(m,c,_,capture)|capture.record(*m,*c)).collect::<Vec<_>>(),"target":{"accepted_must_equal":accepted,"consumed_must_equal":last_valid_ack},"population_kernel_cost":"Unknown; no ownership inference from observer or metadata"}));
    Outcome {
        target,
        other: channels
            .into_iter()
            .map(|(m, c, _, capture)| (m, c, capture))
            .collect(),
    }
}

#[test]
#[ignore = "assigned live VM; one selected control case/channel, new records, explicit phase/work/cleanup/outer bounds"]
fn invalid_application_ack_after_record_preserves_credit_neighbor_and_main() {
    let began = Instant::now();
    let input = Supplied::read();
    let deadlines = input.limits.deadlines(began).unwrap();
    assert!(deadlines.cleanup <= deadlines.outer);
    let base = &input.base;
    assert!(!common::cannot_run_vms(), "requires a supported real VM host");
    let artifact = |path: &std::path::Path| json!({"path":path,"sha256":common::sha256_digest(&std::fs::read(path).unwrap())});
    base.write("artifacts.json", &json!({"fixture":artifact(&std::env::current_exe().unwrap()),"cli":artifact(common::shards()),"testguest":artifact(common::test_guest()),"kernel":artifact(common::kernel()),"init":artifact(common::guest_init()),"leaf_sha256":common::sha256_digest(include_bytes!("adversarial_controls.rs")),"contract_sha256":common::sha256_digest(include_bytes!("adversarial_controls/contract.rs")),"control_inputs_sha256":common::sha256_digest(include_bytes!("adversarial_controls/control_inputs.rs")),"write_receipts_sha256":common::sha256_digest(include_bytes!("adversarial_controls/write_receipts.rs"))}));
    base.write("inputs.json", &json!({"case":format!("{:?}",input.case),"channel":input.channel as u8,"port":base.port,"phase_ms":input.limits.phase_ms,"work_ms":input.limits.work_ms,"cleanup_ms":input.limits.cleanup_ms,"outer_ms":input.limits.outer_ms,"caller_declared_source":input.source,"outer_enforcement":"admitted caller enforces supplied OUTER_MS on the fixture process","owner":"fresh retained Fixture; one target subscription; no endpoint reuse"}));
    let mut fixture = Fixture::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&mut fixture, &input, deadlines.work)
    }));
    let copied = fixture
        .record
        .as_ref()
        .map(|path| std::fs::copy(path, base.records.join("managed-output.before-cleanup")));
    let removed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        remove_run(&fixture, base, deadlines.cleanup)
    }));
    let stopped = fixture.shutdown(deadlines.cleanup);
    let fds = std::fs::read_dir("/proc/self/fd").map(|entries| entries.count());
    base.write("teardown.json", &json!({"protocol_complete":result.is_ok(),"protocol_failure":result.as_ref().err().map(|e|panic_text(e.as_ref())),"metadata_copy":copied.as_ref().map(|copy|json!({"complete":copy.is_ok(),"error":copy.as_ref().err().map(ToString::to_string)})),"run_removed":removed.is_ok(),"remove_failure":removed.as_ref().err().map(|e|panic_text(e.as_ref())),"owner_stop_observer_join":{"complete":stopped.is_ok(),"error":stopped.as_ref().err()},"observer_fds":{"before":fixture.initial_fds,"after":fds.as_ref().ok(),"error":fds.as_ref().err().map(ToString::to_string)},"population_reaping_kernel_return":"Unknown; stop/PID0/FD return do not prove full return"}));
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
    stopped.unwrap();
    let metadata = removed.unwrap_or_else(|error| std::panic::resume_unwind(error));
    let outcome = result.unwrap();
    assert_terminal_metadata(&metadata, &outcome.other);
    let prefix = format!("member 1 channel {} state ", input.channel as u8);
    let rows: Vec<_> = metadata
        .lines()
        .filter(|line| line.starts_with(&prefix))
        .collect();
    let (state, tail) = match outcome.target {
        ExpectedTarget::Held { .. } => ("aborted", "unknown"),
        ExpectedTarget::Ended { .. } => ("ended", "complete"),
    };
    let (accepted, consumed) = outcome.target.cursors();
    assert_eq!(
        rows,
        [format!(
            "{prefix}{state} accepted {accepted} consumed {consumed} tail {tail}"
        )],
        "invalid control must preserve held credit and already observed canonical completion"
    );
}
