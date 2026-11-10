//! Borrowed-observer loss with an exact observed cursor and later independent production.
use super::super::*;
use super::RetainedRecord;

struct Supplied {
    base: Inputs,
    work_ms: u64,
    cleanup_ms: u64,
}

impl Supplied {
    fn read() -> Self {
        let required = |key: &str| {
            std::env::var(format!("SHARDS_TENANT_RECONNECT_{key}"))
                .unwrap_or_else(|_| panic!("supply SHARDS_TENANT_RECONNECT_{key}"))
        };
        let port = required("PORT").parse::<u16>().unwrap();
        let patience = Duration::from_millis(required("DEADLINE_MS").parse().unwrap());
        let work_ms = required("WORK_MS").parse::<u64>().unwrap();
        let cleanup_ms = required("CLEANUP_MS").parse::<u64>().unwrap();
        assert!(
            port > 0 && !patience.is_zero() && work_ms > 0 && cleanup_ms > 0,
            "explicit positive finite inputs"
        );
        assert!(
            Instant::now()
                .checked_add(patience)
                .and_then(|t| t.checked_add(Duration::from_millis(cleanup_ms)))
                .is_some()
        );
        assert!(
            Instant::now()
                .checked_add(Duration::from_millis(work_ms))
                .is_some()
        );
        let records = PathBuf::from(required("RECORDS"));
        assert!(records.is_absolute(), "new absolute record directory");
        std::fs::create_dir(&records).unwrap();
        Self {
            base: Inputs {
                port,
                patience,
                records,
            },
            work_ms,
            cleanup_ms,
        }
    }
}

struct Trace<'a> {
    inputs: &'a Inputs,
    sequence: u64,
}

impl Trace<'_> {
    fn next(&mut self, observer: &Observer, phase: &str, until: Instant) -> Event {
        let sequence = self.sequence;
        self.sequence = sequence.checked_add(1).unwrap();
        let event = observer.next(until);
        let name = format!(
            "{sequence}-{phase}-{}-{}",
            observer.scope.member, observer.scope.channel as u8
        );
        let observed = match &event {
            Event::Record(offset, bytes) => {
                std::fs::write(self.inputs.records.join(format!("{name}.bin")), bytes).unwrap();
                json!({"event":"Record","wire_start":offset,"bytes":bytes.len()})
            }
            Event::End => json!({"event":"TransportEnd"}),
            Event::Lost(code) => json!({"event":"Lost","code":code}),
            Event::Failed(error) => json!({"event":"Failed","error":error}),
        };
        self.inputs.write(&format!("{name}.json"), &observed);
        event
    }
}

enum Accepted {
    Open,
    Data,
    End,
}

fn accept(
    capture: &mut Capture,
    identity: (u32, Channel),
    offset: u64,
    bytes: &[u8],
    expected: &[u8],
) -> Accepted {
    assert!(!bytes.is_empty(), "nonempty canonical record");
    assert_eq!(offset, capture.wire, "actual contiguous transport cursor");
    let record = Record::decode(bytes).unwrap();
    assert_eq!(record.member, identity.0, "declared member identity");
    assert_eq!(record.channel, identity.1, "separate fd1/fd2 identity");
    capture.state.accept(record).unwrap();
    capture.wire = offset.checked_add(u64::try_from(bytes.len()).unwrap()).unwrap();
    match record.event {
        CaptureEvent::Open(label) => {
            assert_eq!(label, declared_label(identity.0));
            Accepted::Open
        }
        CaptureEvent::Data(data) => {
            capture.bytes.extend_from_slice(data);
            assert!(
                expected.starts_with(&capture.bytes),
                "exact controlled bytes without skipped/duplicate payload"
            );
            Accepted::Data
        }
        CaptureEvent::End => {
            assert_eq!(capture.bytes, expected, "End after the exact payload tail");
            assert_eq!(
                capture.state,
                StreamState::Ended {
                    offset: u64::try_from(expected.len()).unwrap()
                }
            );
            Accepted::End
        }
        CaptureEvent::Aborted(errno) => panic!("normal/reconnected capture aborted: {errno}"),
    }
}

fn receive(
    capture: &mut Capture,
    observer: &Observer,
    expected: &[u8],
    goal: ReadGoal,
    trace: &mut Trace<'_>,
    until: Instant,
) {
    loop {
        let Event::Record(offset, bytes) = trace.next(observer, "capture", until) else {
            panic!("canonical record required")
        };
        let accepted = accept(
            capture,
            (observer.scope.member, observer.scope.channel),
            offset,
            &bytes,
            expected,
        );
        if matches!(accepted, Accepted::End) {
            assert!(
                matches!(goal, ReadGoal::CanonicalEnd),
                "surviving member ended before owner stop"
            );
        }
        if matches!(goal, ReadGoal::FinalPayload) && capture.bytes == expected {
            return;
        }
        observer
            .sender
            .send(Command::Handler(Ask::Ack(capture.wire)))
            .unwrap();
        if matches!(accepted, Accepted::End) {
            assert!(
                matches!(trace.next(observer, "terminal", until), Event::End),
                "transport EOF after canonical End ACK"
            );
            return;
        }
        if matches!(goal, ReadGoal::InitialPayload) && capture.bytes == expected {
            return;
        }
    }
}

struct Held {
    start: u64,
    witness: RetainedRecord,
    observed: Capture,
}

fn hold_initial(observer: &Observer, expected: &[u8], trace: &mut Trace<'_>, until: Instant) -> Held {
    let mut observed = Capture::default();
    loop {
        let Event::Record(offset, bytes) = trace.next(observer, "before-disconnect", until) else {
            panic!("initial member record")
        };
        let before = observed.state;
        match accept(
            &mut observed,
            (observer.scope.member, observer.scope.channel),
            offset,
            &bytes,
            expected,
        ) {
            Accepted::End => panic!("surviving member ended before observer disconnect"),
            Accepted::Data if observed.bytes == expected => {
                let witness = RetainedRecord::hold(
                    observer.scope.member,
                    observer.scope.channel,
                    offset,
                    bytes,
                    before,
                )
                .unwrap();
                assert_ne!(offset, 0, "retained cursor observed after Open");
                return Held {
                    start: offset,
                    witness,
                    observed,
                };
            }
            Accepted::Open | Accepted::Data => observer
                .sender
                .send(Command::Handler(Ask::Ack(observed.wire)))
                .unwrap(),
        }
    }
}

fn run(fixture: &mut Fixture, input: &Supplied, until: Instant) -> Vec<(u32, Channel, Capture)> {
    let base = &input.base;
    let port = base.port.to_string();
    let work = input.work_ms.to_string();
    let directives = format!(
        "NETWORK --ingress={port} control\nEXPOSE --harnesses=driver {port} AS ingress FOR control\nCONNECT driver WITH driver ON control\n"
    );
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
    let mut trace = Trace {
        inputs: base,
        sequence: 0,
    };
    let mut terminal = Vec::new();
    for channel in [Channel::Stdout, Channel::Stderr] {
        let index = fixture.observe(1, channel, None, false, until);
        let mut capture = Capture::default();
        receive(
            &mut capture,
            &fixture.observers[index],
            &payload("agent", channel),
            ReadGoal::CanonicalEnd,
            &mut trace,
            until,
        );
        fixture.observers[index]
            .sender
            .send(Command::Handler(Ask::Cancel))
            .unwrap();
        fixture.observers[index].join(until).unwrap();
        fixture
            .observed
            .insert((1, channel as u8), (capture.wire, capture.wire));
        terminal.push((1, channel, capture));
    }
    base.write("normal-ended.json",&json!({"state":"Member1BothCanonicalEndAckAndTransportEof","channels":terminal.iter().map(|(m,c,capture)|capture.record(*m,*c)).collect::<Vec<_>>() }));
    let mut retained: Vec<_> = [Channel::Stdout, Channel::Stderr]
        .into_iter()
        .map(|channel| {
            let index = fixture.observe(2, channel, None, false, until);
            let held = hold_initial(
                &fixture.observers[index],
                &payload("harness", channel),
                &mut trace,
                until,
            );
            (channel, index, held)
        })
        .collect();
    base.write("retained-before-disconnect.json",&json!({"state":"LastObservedInitialDataUnacknowledged","channels":retained.iter().map(|(c,_,held)|json!({"channel":*c as u8,"wire_start":held.start,"observed":held.observed.record(2,*c)})).collect::<Vec<_>>() }));
    for (_, index, _) in &retained {
        fixture.observers[*index]
            .sender
            .send(Command::Handler(Ask::Cancel))
            .unwrap();
        fixture.observers[*index].join(until).unwrap();
    }
    base.write("borrowed-observers-joined.json",&json!({"state":"BothBorrowedObserversLocallyJoinedOwnerRetained","remote_close_processing":"Unverified; local join is not a remote barrier"}));
    fixture.main_progress(until.saturating_duration_since(Instant::now()));
    std::fs::copy(
        fixture.record.as_ref().unwrap(),
        base.records.join("managed-output.after-disconnect"),
    )
    .unwrap();
    for (channel, index, held) in &mut retained {
        *index = fixture.observe_at(2, *channel, None, false, held.start, until);
        let Event::Record(offset, bytes) = trace.next(&fixture.observers[*index], "retained-replay", until)
        else {
            panic!("retained canonical record must replay")
        };
        let (end, state) = held.witness.replay(offset, &bytes).unwrap();
        assert_eq!(end, held.observed.wire, "same actually observed wire tail");
        assert_eq!(
            state, held.observed.state,
            "replay does not append initial payload twice"
        );
        fixture.observers[*index]
            .sender
            .send(Command::Handler(Ask::Ack(end)))
            .unwrap();
    }
    base.write("retained-replayed.json",&json!({"state":"ExactRetainedRecordAtObservedCursorOnBothChannels","ack":"enqueued only; later new data proves consumption, not this enqueue"}));
    // Existing gate release follows both exact replays. New suffix cannot come from replayed credit.
    release_neighbor(fixture, base, until);
    for (channel, index, mut held) in retained {
        let expected = [
            payload("harness", channel),
            payload("harness-after-exit", channel),
        ]
        .concat();
        receive(
            &mut held.observed,
            &fixture.observers[index],
            &expected,
            ReadGoal::FinalPayload,
            &mut trace,
            until,
        );
        fixture.observers[index].ack_and_finish(held.observed.wire, until);
        fixture
            .observed
            .insert((2, channel as u8), (held.observed.wire, held.observed.wire));
        terminal.push((2, channel, held.observed));
    }
    fixture.main_progress(until.saturating_duration_since(Instant::now()));
    base.write("later-progress.json",&json!({"state":"FreshSiblingFd1Fd2AfterReplayAndGatePlusIndependentMain","channels":terminal.iter().map(|(m,c,capture)|capture.record(*m,*c)).collect::<Vec<_>>(),"population_reaping":"Unverified; observer disconnect/replay is not lifecycle ownership"}));
    std::fs::copy(
        fixture.record.as_ref().unwrap(),
        base.records.join("managed-output.before-stop"),
    )
    .unwrap();
    terminal
}

#[test]
#[ignore = "coordinator-only real VM; explicit port/work/cleanup/evidence"]
fn observer_disconnect_replays_retained_record_without_losing_owner() {
    let input = Supplied::read();
    let base = &input.base;
    assert!(!common::cannot_run_vms(), "requires a supported real VM host");
    let until = Instant::now().checked_add(base.patience).unwrap();
    let cleanup_until = until
        .checked_add(Duration::from_millis(input.cleanup_ms))
        .unwrap();
    let artifact = |path: &std::path::Path| json!({"path":path,"sha256":common::sha256_digest(&std::fs::read(path).unwrap())});
    base.write("artifacts.json",&json!({"fixture":artifact(&std::env::current_exe().unwrap()),"cli":artifact(common::shards()),"testguest":artifact(common::test_guest()),"kernel":artifact(common::kernel()),"init":artifact(common::guest_init())}));
    base.write("inputs.json",&json!({"guest_port":base.port,"host_work_ms":base.patience.as_millis(),"helper_work_ms":input.work_ms,"cleanup_ms":input.cleanup_ms,"source":option_env!("SHARDS_TENANT_RECONNECT_SOURCE_REVISION"),"owner":"retained Fixture; observers are borrowed","cursor":"actual unACKed record start; not arbitrary history"}));
    let mut fixture = Fixture::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut fixture, &input, until)));
    if let Some(path) = &fixture.record {
        let _ = std::fs::copy(path, base.records.join("managed-output.before-cleanup"));
    }
    let removed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        remove_run(&fixture, base, cleanup_until)
    }));
    let stopped = fixture.shutdown(cleanup_until);
    base.write("teardown.json",&json!({"protocol_complete":result.is_ok(),"run_removed":removed.is_ok(),"owner_stop_observer_join":{"complete":stopped.is_ok(),"error":stopped.as_ref().err()},"observer_fd_baseline":fixture.initial_fds,"full_population_kernel_return":"Unverified; ROOT independent cleanup gates owed"}));
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
    stopped.unwrap();
    let metadata = removed.unwrap_or_else(|error| std::panic::resume_unwind(error));
    assert_terminal_metadata(&metadata, &result.unwrap());
}

#[cfg(test)]
mod reader_tests {
    use super::*;

    fn encoded(channel: Channel, offset: u64, event: CaptureEvent<'_>) -> Vec<u8> {
        let record = Record {
            member: 2,
            channel,
            offset,
            event,
        };
        let mut bytes = vec![0; record.encoded_len().unwrap()];
        assert_eq!(record.encode(&mut bytes).unwrap(), bytes.len());
        bytes
    }

    fn consume(capture: &mut Capture, channel: Channel, bytes: &[u8], expected: &[u8]) -> Accepted {
        let wire = capture.wire;
        accept(capture, (2, channel), wire, bytes, expected)
    }

    #[test]
    fn fragmented_initial_replay_then_new_data_preserves_exact_capture_and_distinct_wire_tail() {
        for channel in [Channel::Stdout, Channel::Stderr] {
            let initial = payload("harness", channel);
            let suffix = payload("harness-after-exit", channel);
            let expected = [initial.as_slice(), suffix.as_slice()].concat();
            let (head, tail) = initial.split_at(initial.len() / 2);
            let mut capture = Capture::default();
            let open = encoded(channel, 0, CaptureEvent::Open("harness driver"));
            assert!(matches!(
                consume(&mut capture, channel, &open, &initial),
                Accepted::Open
            ));
            let first = encoded(channel, 0, CaptureEvent::Data(head));
            assert!(matches!(
                consume(&mut capture, channel, &first, &initial),
                Accepted::Data
            ));
            let start = capture.wire;
            let before = capture.state;
            let last = encoded(
                channel,
                u64::try_from(head.len()).unwrap(),
                CaptureEvent::Data(tail),
            );
            assert!(matches!(
                consume(&mut capture, channel, &last, &initial),
                Accepted::Data
            ));
            let held = RetainedRecord::hold(2, channel, start, last.clone(), before).unwrap();
            let (wire, state) = held.replay(start, &last).unwrap();
            assert_eq!((wire, state), (capture.wire, capture.state));
            assert_eq!(
                capture.bytes, initial,
                "replayed record remains one logical payload copy"
            );
            assert_ne!(
                wire,
                u64::try_from(initial.len()).unwrap(),
                "wire cursor includes canonical framing"
            );
            let later = encoded(
                channel,
                u64::try_from(initial.len()).unwrap(),
                CaptureEvent::Data(&suffix),
            );
            assert!(matches!(
                consume(&mut capture, channel, &later, &expected),
                Accepted::Data
            ));
            let ended = encoded(channel, u64::try_from(expected.len()).unwrap(), CaptureEvent::End);
            assert!(matches!(
                consume(&mut capture, channel, &ended, &expected),
                Accepted::End
            ));
            assert_eq!(capture.bytes, expected);
            let wire_len = [&open, &first, &last, &later, &ended]
                .into_iter()
                .map(|bytes| u64::try_from(bytes.len()).unwrap())
                .sum::<u64>();
            assert_eq!(capture.wire, wire_len);
        }
    }

    #[test]
    fn shifted_observed_wire_start_refuses_before_changing_capture_state_or_payload() {
        let channel = Channel::Stdout;
        let initial = payload("harness", channel);
        let mut capture = Capture::default();
        let open = encoded(channel, 0, CaptureEvent::Open("harness driver"));
        consume(&mut capture, channel, &open, &initial);
        let before = (capture.wire, capture.state, capture.bytes.clone());
        let data = encoded(channel, 0, CaptureEvent::Data(&initial));
        let shifted = capture.wire.checked_add(1).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            accept(&mut capture, (2, channel), shifted, &data, &initial)
        }));
        assert!(
            result.is_err(),
            "a shifted transport cursor cannot advance the fixture"
        );
        assert_eq!((capture.wire, capture.state, capture.bytes), before);
    }
}
