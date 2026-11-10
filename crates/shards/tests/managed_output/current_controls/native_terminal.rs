//! Actual native wire/credit boundary with an owner-supplied canonical errno.
use super::*;
use shards_remote::managed::{Control, Endpoint, Producer, State};

#[derive(Clone, Copy, Debug)]
enum TerminalControl {
    DuplicateAck,
    Resubscribe,
}

struct NativeInput {
    directory: PathBuf,
    run: String,
    generation: u64,
    channel: Channel,
    errno: i32,
    control: TerminalControl,
    until: Instant,
    cleanup: Instant,
}

impl NativeInput {
    fn read(began: Instant, control: TerminalControl) -> Self {
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
        let errno = required("ERRNO").parse().unwrap();
        assert!(errno > 0, "positive supplied canonical cause");
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
            control,
            until: deadlines.work,
            cleanup: deadlines.cleanup,
        };
        input.write("inputs.json", &json!({"scope":"actual native authenticated Endpoint; no daemon/VM metadata or observed guest read error","supplied_errno":errno,"source":source,"phase_ms":limits.phase_ms,"cleanup_ms":limits.cleanup_ms,"outer_ms":limits.outer_ms,"outer_enforcement":"direct caller required","run":input.run,"generation":generation,"channel":channel as u8,"work":"three canonical records; one selected terminal control; one returned postterminal payload","control":format!("{control:?}"),"fixture_sha256":common::sha256_digest(&std::fs::read(std::env::current_exe().unwrap()).unwrap()),"compiled_leaf_sha256":common::sha256_digest(include_bytes!("native_terminal.rs"))}));
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

struct Worker {
    producer: Producer,
    control: Control,
    credits: mpsc::Receiver<()>,
    ended: mpsc::Receiver<Result<(), managed::Error>>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
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
        let data = payload("agent", input.channel);
        let records = [
            encoded(scope, 0, CaptureEvent::Open(declared_label(1))),
            encoded(scope, 0, CaptureEvent::Data(&data)),
            encoded(
                scope,
                u64::try_from(data.len()).unwrap(),
                CaptureEvent::Aborted(input.errno),
            ),
        ];
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
                errno: input.errno
            }
        );
        assert!(
            matches!(worker.control.state(), State::Finished(at) if at.accepted==cursor && at.consumed==cursor)
        );
        input.write("canonical.json", &json!({"canonical_state":format!("{state:?}"),"control":snapshot(&worker.control),"actual_transport_end":true}));
        let (command, bytes, receipts) = match input.control {
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
        };
        std::fs::write(input.directory.join("postterminal-control.bin"), &bytes).unwrap();
        observer.sender.send(Command::Handler(command)).unwrap();
        let event = trace.next(observer, "native-control", input.until);
        let Event::Lost(Some(code)) = event else {
            panic!("selected terminal control must report typed refusal");
        };
        assert_eq!(
            Reason::try_from(u32::try_from(code).unwrap()),
            Ok(Reason::Protocol)
        );
        assert!(
            matches!(worker.control.state(), State::Finished(at) if at.accepted==cursor && at.consumed==cursor)
        );
        let refused = encoded(
            scope,
            u64::try_from(data.len()).unwrap(),
            CaptureEvent::Data(&data),
        );
        let Err(managed::Rejected::Closed(returned)) = worker.producer.submit(refused.clone()) else {
            panic!("postterminal payload must return with typed Closed");
        };
        assert_eq!(returned, refused);
        input.write("control-refusal.json", &json!({"selected_control":format!("{:?}",input.control),"submitted_bytes":bytes.len(),"reason":format!("{:?}",Reason::Protocol),"control":snapshot(&worker.control),"canonical_state":format!("{state:?}"),"returned_payload_sha256":common::sha256_digest(&returned),"returned_payload_bytes":returned.len()}));
        observer.sender.request_stop();
        let observer = self.observer.as_mut().unwrap();
        let joined = observer.join(input.until);
        let remaining: Vec<_> = observer.events.try_iter().collect();
        for event in &remaining {
            trace.record(observer, "native-after-join", event);
        }
        if let Some(receipts) = receipts {
            let receipts: Vec<_> = receipts.try_iter().collect();
            input.write("io-receipts.json", &json!({"actual":receipts.iter().map(receipt_value).collect::<Vec<_>>(),"driver_joined":joined.is_ok(),"submitted_bytes":bytes.len(),"finish_requested":false,"meaning":"actual local acceptance/read receipts; not peer ACK or daemon metadata"}));
            check_receipts(&receipts, bytes.len(), false);
        }
        input.write("observer-stop.json", &json!({"joined":joined.is_ok(),"error":joined.as_ref().err(),"post_join_records":remaining.iter().filter(|e|matches!(e,Event::Record(..))).count(),"owner_control":snapshot(&worker.control)}));
        joined.unwrap();
        assert!(!remaining.iter().any(|event| matches!(event, Event::Record(..))));
        assert!(
            matches!(worker.control.state(), State::Finished(at) if at.accepted==cursor && at.consumed==cursor)
        );
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
        errors
    }
}

fn exercise(control: TerminalControl) {
    let began = Instant::now();
    let input = NativeInput::read(began, control);
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
