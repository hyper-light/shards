//! Actual native wire/credit boundaries; canonical causes are owner-supplied.
use super::*;
use shards_remote::managed::{Control, Endpoint, Producer, State};

#[derive(Clone, Copy, Debug)]
enum TerminalControl {
    DuplicateAck,
    Resubscribe,
    EmptyFinish,
}

#[derive(Clone, Copy, Debug)]
enum NativeCase {
    Terminal(TerminalControl),
    AfterOpenEmptyFinish,
}

struct NativeInput {
    directory: PathBuf,
    run: String,
    generation: u64,
    channel: Channel,
    errno: Option<i32>,
    case: NativeCase,
    until: Instant,
    cleanup: Instant,
}

impl NativeInput {
    fn read(began: Instant, case: NativeCase) -> Self {
        let required = |suffix| {
            let key = format!("SHARDS_ADVERSARIAL_NATIVE_{suffix}");
            std::env::var(&key).unwrap_or_else(|_| panic!("supply {key}"))
        };
        let phase_ms = required("PHASE_MS").parse().unwrap();
        let limits = Limits {
            phase_ms,
            work_ms: phase_ms,
            cleanup_ms: required("CLEANUP_MS").parse().unwrap(),
            outer_ms: required("OUTER_MS").parse().unwrap(),
        };
        let deadlines = limits.deadlines(began).unwrap();
        let channel = match required("CHANNEL").as_str() {
            "stdout" => Channel::Stdout,
            "stderr" => Channel::Stderr,
            _ => panic!("select stdout or stderr"),
        };
        let (errno, selection, work) = match case {
            NativeCase::Terminal(control) => {
                let errno = required("ERRNO").parse().unwrap();
                assert!(errno > 0, "positive supplied canonical cause");
                if matches!(control, TerminalControl::EmptyFinish) {
                    assert_eq!(channel, Channel::Stdout);
                    assert_eq!(errno, 5, "assigned owner-supplied cause");
                }
                (
                    Some(errno),
                    format!("{control:?}"),
                    "three canonical records; one selected terminal control; one returned postterminal payload",
                )
            }
            NativeCase::AfterOpenEmptyFinish => {
                assert_eq!(channel, Channel::Stdout);
                (
                    None,
                    "AfterOpenEmptyFinish".into(),
                    "one canonical Open; empty FIN without application ACK; one authorized replay and exact ACK; owner abort and returned owned payload",
                )
            }
        };
        let run = required("RUN");
        assert!(!run.is_empty());
        let generation = required("GENERATION").parse().unwrap();
        assert!(generation > 0);
        let source = required("SOURCE_REVISION");
        assert!(source.len() == 40 && source.bytes().all(|b| b.is_ascii_hexdigit()));
        let directory = PathBuf::from(required("RECORDS"));
        assert!(directory.is_absolute());
        std::fs::create_dir(&directory).unwrap();
        let input = Self {
            directory,
            run,
            generation,
            channel,
            errno,
            case,
            until: deadlines.work,
            cleanup: deadlines.cleanup,
        };
        input.write("inputs.json", &json!({"scope":"actual native authenticated Endpoint; no daemon/VM metadata or observed guest read error","supplied_errno":errno,"source":source,"phase_ms":limits.phase_ms,"cleanup_ms":limits.cleanup_ms,"outer_ms":limits.outer_ms,"outer_enforcement":"direct caller required","run":input.run,"generation":generation,"channel":channel as u8,"work":work,"control":selection,"fixture_sha256":common::sha256_digest(&std::fs::read(std::env::current_exe().unwrap()).unwrap()),"compiled_leaf_sha256":common::sha256_digest(include_bytes!("native_terminal.rs"))}));
        input
    }

    fn write(&self, name: &str, value: &Value) {
        write_json(&self.directory, name, value);
    }
}

fn snapshot(control: &Control) -> Value {
    let state = control.state();
    let at = state.progress();
    json!({"state":format!("{state:?}"),"accepted":at.accepted,"consumed":at.consumed,"transport_drained":"Unknown; local stop/join is a separate witness"})
}

fn encoded(scope: Scope, offset: u64, event: CaptureEvent<'_>) -> Vec<u8> {
    let record = Record {
        member: scope.member,
        channel: scope.channel,
        offset,
        event,
    };
    let mut bytes = vec![0; record.encoded_len().unwrap()];
    assert_eq!(record.encode(&mut bytes).unwrap(), bytes.len());
    bytes
}

fn check_empty_finish(receipts: &[ControlReceipt]) {
    let mut finishes = 0usize;
    for receipt in receipts {
        match receipt {
            ControlReceipt::Finish { error, pending } => {
                assert!(error.is_none(), "actual empty-pending FIN failure: {error:?}");
                assert_eq!(*pending, 0, "actual pending buffer at finish");
                finishes = finishes.checked_add(1).unwrap();
            }
            ControlReceipt::Read { .. } => {}
            ControlReceipt::Write { .. } | ControlReceipt::WriteError(_) => {
                panic!("empty control must not submit stream bytes: {receipt:?}");
            }
        }
    }
    assert_eq!(finishes, 1, "one actual successful FIN receipt");
}

struct Worker {
    producer: Producer,
    control: Control,
    credits: mpsc::Receiver<()>,
    ended: mpsc::Receiver<Result<(), managed::Error>>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn held(&self, start: u64, end: u64) {
        assert!(
            matches!(self.control.state(), State::Pending(at) if at.accepted==end && at.consumed==start),
            "the observed record must retain its exact unacknowledged interval"
        );
        assert_eq!(self.producer.capacity(), 0, "no returned record credit");
    }

    fn ready(&self, cursor: u64, until: Instant) {
        loop {
            match self.control.state() {
                State::Ready(at) => {
                    assert_eq!((at.accepted, at.consumed), (cursor, cursor));
                    return;
                }
                State::Finished(_) | State::Aborted(_) => panic!("native data admission became terminal"),
                State::Pending(_) => {}
            }
            self.credits.recv_timeout(left(until)).unwrap();
        }
    }
}

#[derive(Default)]
struct Native {
    worker: Option<Worker>,
    observer: Option<Observer>,
    after_open_receipts: Option<mpsc::Receiver<ControlReceipt>>,
}

impl Native {
    fn run(&mut self, input: &NativeInput) {
        let owner = Identity::generate().unwrap();
        let server = Identity::generate().unwrap();
        let scope = Scope {
            run: RunId(Sha256::digest(input.run.as_bytes()).into()),
            generation: input.generation,
            member: 1,
            channel: input.channel,
        };
        let (data, records) = match input.case {
            NativeCase::Terminal(_) => {
                let data = payload("agent", input.channel);
                let records = vec![
                    encoded(scope, 0, CaptureEvent::Open(declared_label(1))),
                    encoded(scope, 0, CaptureEvent::Data(&data)),
                    encoded(
                        scope,
                        u64::try_from(data.len()).unwrap(),
                        CaptureEvent::Aborted(input.errno.unwrap()),
                    ),
                ];
                (data, records)
            }
            NativeCase::AfterOpenEmptyFinish => (
                Vec::new(),
                vec![encoded(scope, 0, CaptureEvent::Open(declared_label(1)))],
            ),
        };
        let capacity = records.iter().map(Vec::capacity).max().unwrap();
        let admitted = Admitted {
            record_bytes: capacity,
            transport_bytes: u64::try_from(capacity.checked_add(managed::DATA_HEAD).unwrap()).unwrap(),
            // RFC8446 uint24 handshake length plus its four-byte header, as Observer.
            handshake_bytes: (1 << 24) - 1 + 4,
            datagram_bytes: managed::DATAGRAM_BYTES,
            patience: left(input.until),
        };
        let (endpoint, producer, control) =
            Endpoint::bind(&server, owner.fingerprint(), scope, admitted).unwrap();
        let metadata = Metadata {
            server: endpoint.fingerprint,
            owner: owner.fingerprint(),
            run: input.run.clone(),
            generation: input.generation,
            capacity,
            endpoints: BTreeMap::from([((scope.member, scope.channel as u8), endpoint.address)]),
        };
        assert_eq!(metadata.scope(scope.member, scope.channel), scope);
        let address = endpoint.address;
        let (wake, credits) = mpsc::sync_channel(1);
        producer.on_credit(move || {
            let _ = wake.try_send(());
        });
        let (finished, ended) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("adversarial-native-owner".into())
            .spawn(move || {
                let result = endpoint.run();
                let _ = finished.send(result);
            })
            .unwrap();
        self.worker = Some(Worker {
            producer,
            control,
            credits,
            ended,
            thread: Some(thread),
        });
        self.observer = Some(Observer::start(&owner, &metadata, scope, address, input.until));
        input.write("bound.json", &json!({"address":address,"capacity":capacity,"transport_bytes":admitted.transport_bytes,"handshake_bytes":admitted.handshake_bytes,"metadata_provenance":"constructed from this actual Endpoint; no daemon append","control":snapshot(&self.worker.as_ref().unwrap().control)}));
        let control = match input.case {
            NativeCase::Terminal(control) => control,
            NativeCase::AfterOpenEmptyFinish => {
                return self.after_open_empty_finish(
                    input,
                    &owner,
                    &metadata,
                    scope,
                    records.first().unwrap(),
                );
            }
        };
        let worker = self.worker.as_ref().unwrap();
        let observer = self.observer.as_ref().unwrap();
        let mut trace = Trace {
            directory: &input.directory,
            next: 0,
        };
        let mut state = StreamState::New;
        let mut cursor = 0u64;
        for (index, submitted) in records.iter().enumerate() {
            worker.ready(cursor, input.until);
            worker.producer.submit(submitted.clone()).unwrap();
            let event = trace.next(observer, "native-record", input.until);
            let Event::Record(start, bytes) = event else {
                panic!("canonical native record required");
            };
            assert_eq!(start, cursor);
            assert_eq!(&bytes, submitted);
            state.accept(Record::decode(&bytes).unwrap()).unwrap();
            cursor = cursor.checked_add(u64::try_from(bytes.len()).unwrap()).unwrap();
            if index.checked_add(1).unwrap() == records.len() {
                worker.control.finish().unwrap();
            }
            observer.sender.send(Command::Handler(Ask::Ack(cursor))).unwrap();
        }
        assert!(matches!(
            trace.next(observer, "native-eof", input.until),
            Event::End
        ));
        assert_eq!(
            state,
            StreamState::Aborted {
                offset: u64::try_from(data.len()).unwrap(),
                errno: input.errno.unwrap()
            }
        );
        assert!(
            matches!(worker.control.state(), State::Finished(at) if at.accepted==cursor && at.consumed==cursor)
        );
        input.write("canonical.json", &json!({"canonical_state":format!("{state:?}"),"control":snapshot(&worker.control),"actual_transport_end":true}));
        let (command, bytes, receipts) = match control {
            TerminalControl::DuplicateAck => (Ask::Ack(cursor), managed::consumed(cursor).to_vec(), None),
            TerminalControl::Resubscribe => {
                let bytes = managed::subscribe(scope, cursor).to_vec();
                assert_eq!(bytes.len(), managed::CONTROL_BYTES);
                let (sender, receipts) = mpsc::channel();
                (
                    Ask::RawControl {
                        bytes: bytes.clone(),
                        finish: false,
                        receipts: sender,
                    },
                    bytes,
                    Some(receipts),
                )
            }
            TerminalControl::EmptyFinish => {
                let (sender, receipts) = mpsc::channel();
                (
                    Ask::RawControl {
                        bytes: Vec::new(),
                        finish: true,
                        receipts: sender,
                    },
                    Vec::new(),
                    Some(receipts),
                )
            }
        };
        let reason = match control {
            TerminalControl::EmptyFinish => Reason::Closed,
            TerminalControl::DuplicateAck | TerminalControl::Resubscribe => Reason::Protocol,
        };
        std::fs::write(input.directory.join("postterminal-control.bin"), &bytes).unwrap();
        observer.sender.send(Command::Handler(command)).unwrap();
        let event = trace.next(observer, "native-control", input.until);
        let Event::Lost(Some(code)) = event else {
            panic!("selected terminal control must report typed refusal");
        };
        assert_eq!(Reason::try_from(u32::try_from(code).unwrap()), Ok(reason));
        assert!(
            matches!(worker.control.state(), State::Finished(at) if at.accepted==cursor && at.consumed==cursor)
        );
        let refused = encoded(
            scope,
            u64::try_from(data.len()).unwrap(),
            CaptureEvent::Data(&data),
        );
        let candidate = refused.clone();
        let allocation = candidate.as_ptr();
        let capacity = candidate.capacity();
        let Err(managed::Rejected::Closed(returned)) = worker.producer.submit(candidate) else {
            panic!("postterminal payload must return with typed Closed");
        };
        assert_eq!(returned, refused);
        if matches!(control, TerminalControl::EmptyFinish) {
            assert_eq!(returned.as_ptr(), allocation);
            assert_eq!(returned.capacity(), capacity);
            assert_eq!(worker.producer.capacity(), 0);
            std::fs::write(input.directory.join("postterminal-candidate.bin"), &refused).unwrap();
            std::fs::write(input.directory.join("postterminal-returned.bin"), &returned).unwrap();
        }
        input.write("control-refusal.json", &json!({"selected_control":format!("{control:?}"),"submitted_bytes":bytes.len(),"reason":format!("{reason:?}"),"control":snapshot(&worker.control),"canonical_state":format!("{state:?}"),"returned_payload_sha256":common::sha256_digest(&returned),"returned_payload_bytes":returned.len(),"same_allocation":returned.as_ptr()==allocation,"capacity_submitted":capacity,"capacity_returned":returned.capacity(),"producer_capacity":worker.producer.capacity()}));
        observer.sender.request_stop();
        let observer = self.observer.as_mut().unwrap();
        let joined = observer.join(input.until);
        let remaining: Vec<_> = observer.events.try_iter().collect();
        for event in &remaining {
            trace.record(observer, "native-after-join", event);
        }
        if let Some(receipts) = receipts {
            let receipts: Vec<_> = receipts.try_iter().collect();
            input.write("io-receipts.json", &json!({"actual":receipts.iter().map(receipt_value).collect::<Vec<_>>(),"driver_joined":joined.is_ok(),"submitted_bytes":bytes.len(),"finish_requested":matches!(control,TerminalControl::EmptyFinish),"meaning":"actual local acceptance/read/finish receipts; not peer ACK or daemon metadata; empty FIN is not an Io::write return0"}));
            if matches!(control, TerminalControl::EmptyFinish) {
                assert!(bytes.is_empty());
                check_empty_finish(&receipts);
            } else {
                check_receipts(&receipts, bytes.len(), false);
            }
        }
        input.write("observer-stop.json", &json!({"joined":joined.is_ok(),"error":joined.as_ref().err(),"post_join_records":remaining.iter().filter(|e|matches!(e,Event::Record(..))).count(),"owner_control":snapshot(&worker.control)}));
        joined.unwrap();
        assert!(!remaining.iter().any(|event| matches!(event, Event::Record(..))));
        assert!(
            matches!(worker.control.state(), State::Finished(at) if at.accepted==cursor && at.consumed==cursor)
        );
        if matches!(control, TerminalControl::EmptyFinish) {
            assert_eq!(worker.producer.capacity(), 0);
        }
    }

    fn after_open_empty_finish(
        &mut self,
        input: &NativeInput,
        owner: &Identity,
        metadata: &Metadata,
        scope: Scope,
        submitted: &[u8],
    ) {
        let worker = self.worker.as_ref().unwrap();
        let observer = self.observer.as_ref().unwrap();
        let mut trace = Trace {
            directory: &input.directory,
            next: 0,
        };
        let initial = worker.control.progress();
        assert_eq!(initial.accepted, initial.consumed);
        worker.ready(initial.consumed, input.until);
        std::fs::write(input.directory.join("only-submitted-open.bin"), submitted).unwrap();
        let candidate = submitted.to_vec();
        let submitted_capacity = candidate.capacity();
        assert!(submitted_capacity <= metadata.capacity);
        worker.producer.submit(candidate).unwrap();
        // Observer::next records bytes without Observer::exact's automatic ACK.
        let Event::Record(start, bytes) = trace.next(observer, "open-without-ack", input.until) else {
            panic!("one actual canonical Open required without application ACK");
        };
        assert_eq!(start, initial.consumed);
        assert_eq!(bytes, submitted);
        let record = Record::decode(&bytes).unwrap();
        assert_eq!((record.member, record.channel), (scope.member, scope.channel));
        assert_eq!(record.offset, 0);
        assert!(matches!(record.event, CaptureEvent::Open(label) if label==declared_label(1)));
        let mut canonical = StreamState::New;
        canonical.accept(record).unwrap();
        assert_eq!(canonical, StreamState::Open { offset: 0 });
        let end = start.checked_add(u64::try_from(bytes.len()).unwrap()).unwrap();
        assert!(end > start);
        worker.held(start, end);
        input.write("held-before-fin.json", &json!({"wire_start":start,"wire_end":end,"encoded_bytes":bytes.len(),"encoded_sha256":common::sha256_digest(&bytes),"submitted_capacity":submitted_capacity,"admitted_record_bytes":metadata.capacity,"canonical_state":format!("{canonical:?}"),"application_ack_sent":false,"producer_capacity":worker.producer.capacity(),"control":snapshot(&worker.control)}));

        let (sender, receipts) = mpsc::channel();
        self.after_open_receipts = Some(receipts);
        std::fs::write(input.directory.join("empty-control.bin"), []).unwrap();
        observer
            .sender
            .send(Command::Handler(Ask::RawControl {
                bytes: Vec::new(),
                finish: true,
                receipts: sender,
            }))
            .unwrap();
        let finish = self
            .after_open_receipts
            .as_ref()
            .unwrap()
            .recv_timeout(left(input.until))
            .unwrap();
        input.write("finish-before-peer-event.json", &receipt_value(&finish));
        check_empty_finish(std::slice::from_ref(&finish));
        let Event::Lost(Some(code)) = trace.next(observer, "empty-fin-peer-close", input.until) else {
            panic!("empty FIN must receive typed peer Closed before local cancellation");
        };
        assert_eq!(Reason::try_from(u32::try_from(code).unwrap()), Ok(Reason::Closed));
        worker.held(start, end);
        input.write("held-after-peer-close.json", &json!({"peer_reason":format!("{:?}",Reason::Closed),"before_local_cancel":true,"canonical_state":format!("{canonical:?}"),"control":snapshot(&worker.control),"producer_capacity":worker.producer.capacity()}));
        observer.sender.request_stop();
        let observer = self.observer.as_mut().unwrap();
        let joined = observer.join(input.until);
        let remaining: Vec<_> = observer.events.try_iter().collect();
        for event in &remaining {
            trace.record(observer, "first-observer-after-join", event);
        }
        let mut receipts = vec![finish];
        receipts.extend(self.after_open_receipts.as_ref().unwrap().try_iter());
        input.write("io-receipts.json", &json!({"actual":receipts.iter().map(receipt_value).collect::<Vec<_>>(),"driver_joined":joined.is_ok(),"submitted_bytes":0,"finish_requested":true,"meaning":"actual local empty-pending FIN receipt; not Io::write return0, application ACK or server connection-removal proof"}));
        input.write("first-observer-stop.json", &json!({"joined":joined.is_ok(),"error":joined.as_ref().err(),"post_join_records":remaining.iter().filter(|e|matches!(e,Event::Record(..))).count(),"control":snapshot(&worker.control)}));
        joined.unwrap();
        check_empty_finish(&receipts);
        assert!(!remaining.iter().any(|event| matches!(event, Event::Record(..))));
        worker.held(start, end);

        // The existing Client subscribes at wire origin; no replacement cursor/API.
        assert_eq!(start, 0, "retained first record starts at the native wire origin");
        assert_eq!(metadata.owner, owner.fingerprint());
        assert_eq!(metadata.scope(scope.member, scope.channel), scope);
        let address = *metadata
            .endpoints
            .get(&(scope.member, scope.channel as u8))
            .unwrap();
        input.write("one-replay-attempt.json", &json!({"retained_start":start,"attempts":1,"same_owner":metadata.owner==owner.fingerprint(),"same_scope":metadata.scope(scope.member,scope.channel)==scope,"same_endpoint":address,"control":snapshot(&worker.control),"admission_uncertainty":"peer Closed and local observer join do not prove server QUIC connection removal; retain failure without retry"}));
        self.observer = Some(Observer::start(owner, metadata, scope, address, input.until));
        let observer = self.observer.as_ref().unwrap();
        let Event::Record(replay_start, replay) = trace.next(observer, "authorized-replay", input.until)
        else {
            panic!("the one authorized subscription must replay the exact held Open");
        };
        assert_eq!(replay_start, start);
        assert_eq!(replay, bytes);
        let mut replay_state = StreamState::New;
        replay_state.accept(Record::decode(&replay).unwrap()).unwrap();
        assert_eq!(replay_state, canonical);
        worker.held(start, end);
        input.write("replay-before-ack.json", &json!({"wire_start":replay_start,"wire_end":replay_start.checked_add(u64::try_from(replay.len()).unwrap()).unwrap(),"encoded_sha256":common::sha256_digest(&replay),"canonical_state":format!("{replay_state:?}"),"control":snapshot(&worker.control),"producer_capacity":worker.producer.capacity()}));
        observer.sender.send(Command::Handler(Ask::Ack(end))).unwrap();
        worker.ready(end, input.until);
        assert_eq!(worker.producer.capacity(), metadata.capacity);
        input.write("actual-ack-credit.json", &json!({"application_ack":end,"consumed_interval_start":start,"consumed_interval_end":end,"consumed_interval_bytes":end.checked_sub(start).unwrap(),"admitted_record_bytes":metadata.capacity,"producer_capacity":worker.producer.capacity(),"canonical_state":format!("{canonical:?}"),"control":snapshot(&worker.control),"meaning":"actual Ready predicate after application ACK; no allocator/kernel/durability assertion"}));

        observer.sender.request_stop();
        let observer = self.observer.as_mut().unwrap();
        let joined = observer.join(input.until);
        let remaining: Vec<_> = observer.events.try_iter().collect();
        for event in &remaining {
            trace.record(observer, "replay-observer-after-join", event);
        }
        input.write("replay-observer-stop.json", &json!({"joined":joined.is_ok(),"error":joined.as_ref().err(),"post_join_records":remaining.iter().filter(|e|matches!(e,Event::Record(..))).count(),"control":snapshot(&worker.control)}));
        joined.unwrap();
        assert!(!remaining.iter().any(|event| matches!(event, Event::Record(..))));
        assert!(
            matches!(worker.control.state(),State::Ready(at)|State::Pending(at) if at.accepted==end && at.consumed==end)
        );
        worker.control.abort();
        assert!(matches!(worker.control.finish(), Err(managed::Error::Closed)));
        assert!(matches!(worker.control.state(),State::Aborted(at) if at.accepted==end && at.consumed==end));
        assert_eq!(worker.producer.capacity(), 0);
        let candidate = submitted.to_vec();
        let allocation = candidate.as_ptr();
        let capacity = candidate.capacity();
        let Err(managed::Rejected::Closed(returned)) = worker.producer.submit(candidate) else {
            panic!("owner-aborted producer must return the owned Open with typed Closed");
        };
        assert_eq!(returned, submitted);
        assert_eq!(returned.as_ptr(), allocation);
        assert_eq!(returned.capacity(), capacity);
        assert!(matches!(worker.control.state(),State::Aborted(at) if at.accepted==end && at.consumed==end));
        assert_eq!(worker.producer.capacity(), 0);
        std::fs::write(input.directory.join("owner-aborted-returned.bin"), &returned).unwrap();
        input.write("owner-abort-owned-return.json", &json!({"canonical_state":format!("{canonical:?}"),"canonical_terminal_record_submitted":false,"control":snapshot(&worker.control),"finish_after_abort":"Closed","returned_reason":"Closed","returned_payload_bytes":returned.len(),"returned_payload_sha256":common::sha256_digest(&returned),"same_allocation":returned.as_ptr()==allocation,"capacity_submitted":capacity,"capacity_returned":returned.capacity(),"producer_capacity":worker.producer.capacity(),"writer_population_death":"Unknown; no guest or actor exists in this native fixture"}));
    }

    fn cleanup(&mut self, input: &NativeInput) -> Vec<String> {
        let mut errors = Vec::new();
        if let Some(observer) = &self.observer {
            observer.sender.request_stop();
        }
        if let Some(worker) = &self.worker {
            worker.control.abort();
            let state = worker.control.state();
            let finish = worker.control.finish();
            input.write(
                "owner-stop.json",
                &json!({"control":snapshot(&worker.control),"finish_after_abort":format!("{finish:?}")}),
            );
            if !matches!(state, State::Aborted(_)) || !matches!(finish, Err(managed::Error::Closed)) {
                errors.push("owner abort must remain typed Aborted/Closed".into());
            }
        }
        if let Some(observer) = &mut self.observer
            && let Err(error) = observer.join(input.cleanup)
        {
            errors.push(error);
        }
        if matches!(input.case, NativeCase::AfterOpenEmptyFinish) {
            if let Some(observer) = &self.observer {
                let mut trace = Trace {
                    directory: &input.directory,
                    next: 0,
                };
                for event in observer.events.try_iter() {
                    trace.record(observer, "cleanup-queued-event", &event);
                }
            }
            if let Some(receipts) = &self.after_open_receipts {
                input.write("cleanup-io-tail.json", &json!({"actual":receipts.try_iter().map(|receipt|receipt_value(&receipt)).collect::<Vec<_>>(),"meaning":"actual unconsumed receipt tail after cleanup join; the earlier Finish has its own record"}));
            }
        }
        if let Some(worker) = &mut self.worker {
            match worker.ended.recv_timeout(left(input.cleanup)) {
                Ok(result) => {
                    if let Err(error) = result {
                        errors.push(error.to_string());
                    }
                    if worker.thread.take().unwrap().join().is_err() {
                        errors.push("native endpoint panicked".into());
                    }
                }
                Err(error) => errors.push(format!("native endpoint join not established: {error}")),
            }
        }
        self.observer = None;
        self.worker = None;
        self.after_open_receipts = None;
        errors
    }
}

fn exercise(control: TerminalControl) {
    exercise_case(NativeCase::Terminal(control));
}

fn exercise_case(case: NativeCase) {
    let began = Instant::now();
    let input = NativeInput::read(began, case);
    let before = std::fs::read_dir("/proc/self/fd").unwrap().count();
    let mut native = Native::default();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| native.run(&input)));
    let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| native.cleanup(&input)));
    let after = std::fs::read_dir("/proc/self/fd").unwrap().count();
    input.write("teardown.json", &json!({"protocol_complete":result.is_ok(),"protocol_failure":result.as_ref().err().map(|e|panic_text(e.as_ref())),"cleanup_completed":matches!(&cleanup,Ok(errors) if errors.is_empty()),"cleanup_errors":cleanup.as_ref().ok(),"cleanup_panic":cleanup.as_ref().err().map(|e|panic_text(e.as_ref())),"observer_fds":{"before":before,"after":after},"population_kernel_return":"Unknown; this fixture has no guest or daemon lifecycle"}));
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
    let errors = cleanup.unwrap_or_else(|error| std::panic::resume_unwind(error));
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(after, before);
}

#[test]
#[ignore = "assigned native actual endpoint; explicit positive phase/cleanup/outer, scope/errno and new records"]
fn canonical_aborted_duplicate_ack_preserves_errno_and_terminal_credit() {
    exercise(TerminalControl::DuplicateAck);
}

#[test]
#[ignore = "assigned native actual endpoint; one owned Subscribe after Aborted, explicit bounds and new records"]
fn canonical_aborted_resubscribe_preserves_errno_and_terminal_credit() {
    exercise(TerminalControl::Resubscribe);
}

#[test]
#[ignore = "assigned native stdout/errno5; one real empty-pending FIN after Aborted/ACK/End, explicit bounds and fresh records"]
fn canonical_aborted_empty_finish_preserves_errno_credit_and_owned_return() {
    exercise(TerminalControl::EmptyFinish);
}

#[test]
#[ignore = "preparation only: ROOT review required; one native stdout Open without ACK, empty FIN and one authorized replay; explicit bounds and fresh records"]
fn canonical_open_empty_finish_retains_unacked_record_until_authorized_replay() {
    exercise_case(NativeCase::AfterOpenEmptyFinish);
}
