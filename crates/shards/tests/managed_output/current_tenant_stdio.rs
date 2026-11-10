//! Owned descendants retain inherited capture writers after each parent closes its own.
use super::*;

#[test]
#[ignore = "assigned bounded real VM; explicit normal work/cleanup/port/evidence"]
fn normal_exit_preserves_current_neighbor_and_main() {
    let inputs = Inputs::supplied();
    let cleanup_ms = std::env::var("SHARDS_TENANT_STDIO_CLEANUP_MS")
        .expect("supply SHARDS_TENANT_STDIO_CLEANUP_MS")
        .parse::<u64>()
        .unwrap();
    assert!(cleanup_ms > 0, "explicit finite cleanup deadline");
    assert!(!common::cannot_run_vms(), "requires a supported real VM host");
    let until = Instant::now().checked_add(inputs.patience).unwrap();
    let cleanup_until = until.checked_add(Duration::from_millis(cleanup_ms)).unwrap();
    let artifact = |path: &std::path::Path| json!({"path":path,"sha256":common::sha256_digest(&std::fs::read(path).unwrap())});
    inputs.write("inputs.json", &json!({"port":inputs.port,"work_ms":inputs.patience.as_millis(),"cleanup_ms":cleanup_ms,"source":option_env!("SHARDS_TENANT_EXIT_SOURCE_REVISION"),"fixture":artifact(&std::env::current_exe().unwrap()),"cli":artifact(common::shards()),"testguest":artifact(common::test_guest()),"kernel":artifact(common::kernel()),"init":artifact(common::guest_init())}));
    let mut fixture = Fixture::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        super::super::run(&mut fixture, &inputs, until)
    }));
    if let Some(path) = &fixture.record {
        let _ = std::fs::copy(path, inputs.records.join("managed-output.before-cleanup"));
    }
    let removed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        remove_run(&fixture, &inputs, cleanup_until)
    }));
    let stopped = fixture.shutdown(cleanup_until);
    inputs.write("teardown.json", &json!({"protocol_complete":result.is_ok(),"run_removed":removed.is_ok(),"owner_stop_observer_join":{"complete":stopped.is_ok(),"error":stopped.as_ref().err()},"observer_fd_baseline":fixture.initial_fds,"parent_child_waitstatus_and_full_population_return":"Unknown; EOF/stop/host FD return are not independent whole-population/kernel evidence"}));
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
    stopped.unwrap();
    let metadata = removed.unwrap_or_else(|error| std::panic::resume_unwind(error));
    assert_terminal_metadata(&metadata, &result.unwrap());
}

struct InheritedInputs {
    common: Supplied,
    tasks: u32,
    depth: Option<u32>,
}

#[derive(Clone, Copy, Debug)]
enum InheritedFinish {
    Challenge,
    GuestExpiry { work_ms: u64 },
}

impl InheritedFinish {
    fn actor_work_ms(self, sibling_work_ms: u64) -> u64 {
        match self {
            Self::Challenge => sibling_work_ms,
            Self::GuestExpiry { work_ms } => work_ms,
        }
    }
}

fn inherited_report_bytes(challenge_bytes: usize) -> Option<usize> {
    let capture = ["agent", "agent-inherited-ready", "agent-inherited-after-release"]
        .into_iter()
        .flat_map(|member| [payload(member, Channel::Stdout), payload(member, Channel::Stderr)])
        .try_fold(0usize, |n, bytes| n.checked_add(bytes.len()))?;
    challenge_bytes
        .checked_add(size_of::<u8>() + size_of::<u32>())?
        .checked_mul(2)?
        .checked_add(capture)
}

impl InheritedInputs {
    fn read(depth: Option<u32>) -> Self {
        let common = Supplied::read();
        let tasks = std::env::var("SHARDS_TENANT_STDIO_TASKS")
            .unwrap_or_else(|_| panic!("supply SHARDS_TENANT_STDIO_TASKS"))
            .parse::<u32>()
            .unwrap();
        assert!(tasks > 0, "positive u32 matches actual AGENT --processes grammar");
        if let Some(depth) = depth {
            assert!(depth >= 2, "explicit finite descendant depth below the actor");
            // Existing fixture processes: server + sandbox + reaper + actor, then D descendants.
            let required = depth.checked_add(4).unwrap();
            assert!(tasks >= required, "caller task ask covers the controlled chain");
        }
        let wire = inherited_report_bytes(common.challenge.len()).unwrap();
        assert!(
            u64::try_from(wire).unwrap() <= common.report_bytes,
            "parent/child capture plus control request+reply envelope"
        );
        Self { common, tasks, depth }
    }
}

fn exchange_inherited(fixture: &Fixture, input: &Supplied, until: Instant) {
    let listed = input.base.command(
        fixture,
        "actor-ports.json",
        &["read", "ports", "owner-output"],
        until,
    );
    let wanted = format!("{}/tcp", input.actor);
    let rows: Vec<_> = listed
        .stdout
        .lines()
        .filter(|line| line.split_whitespace().next() == Some(wanted.as_str()))
        .collect();
    let [row] = rows.as_slice() else {
        panic!("one actually published actor TCP port: {listed}");
    };
    let fields: Vec<_> = row.split_whitespace().collect();
    let [_, address, "agent", "helper"] = fields.as_slice() else {
        panic!("actor receiver: {row}");
    };
    let address: SocketAddr = address.parse().unwrap();
    assert_eq!(
        address.ip(),
        std::net::Ipv4Addr::LOCALHOST,
        "explicit owner loopback bind"
    );
    assert_ne!(address.port(), 0, "actual OS-assigned host port");
    let frame = [
        &[1][..],
        &u32::try_from(input.challenge.len()).unwrap().to_be_bytes(),
        &input.challenge,
    ]
    .concat();
    let mut sent = Vec::new();
    let mut received = Vec::new();
    let result = (|| -> io::Result<()> {
        let mut peer = std::net::TcpStream::connect_timeout(&address, remaining(until)?)?;
        let mut rest = frame.as_slice();
        while !rest.is_empty() {
            peer.set_write_timeout(Some(remaining(until)?))?;
            match peer.write(rest) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    sent.extend_from_slice(rest.get(..n).ok_or(io::ErrorKind::InvalidData)?);
                    rest = rest.get(n..).ok_or(io::ErrorKind::InvalidData)?;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        peer.shutdown(std::net::Shutdown::Write)?;
        let mut reply = Vec::new();
        reply.try_reserve_exact(frame.len()).map_err(io::Error::other)?;
        reply.resize(frame.len(), 0);
        let mut rest = reply.as_mut_slice();
        while !rest.is_empty() {
            peer.set_read_timeout(Some(remaining(until)?))?;
            match peer.read(rest) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    received.extend_from_slice(rest.get(..n).ok_or(io::ErrorKind::InvalidData)?);
                    rest = rest.get_mut(n..).ok_or(io::ErrorKind::InvalidData)?;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        loop {
            peer.set_read_timeout(Some(remaining(until)?))?;
            let mut byte = [0];
            match peer.read(&mut byte) {
                Ok(0) => break,
                Ok(n) => {
                    received.extend_from_slice(byte.get(..n).ok_or(io::ErrorKind::InvalidData)?);
                    return Err(io::ErrorKind::InvalidData.into());
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    })();
    input.base.write("actor-control.json", &json!({"state":"OwnerChallengeAfterParentWritersClosedAndNewSiblingBytes","sent":sent,"received":received,"complete":result.is_ok(),"error":result.as_ref().err().map(ToString::to_string),"cause_known_to_host":"transport IO only; actor IO failure phase/history can remain Unknown"}));
    result.unwrap();
    assert_eq!(sent, frame);
    assert_eq!(
        received, frame,
        "fresh child response after parent writer closure"
    );
}

fn finish_inherited_actor(
    fixture: &mut Fixture,
    shared: &Supplied,
    channels: &mut [(u32, Channel, usize, Capture)],
    finish: InheritedFinish,
    until: Instant,
) {
    for (member, channel, index, capture) in channels {
        if *member == 1 {
            let expected = match finish {
                InheritedFinish::Challenge => [
                    payload("agent", *channel),
                    payload("agent-inherited-ready", *channel),
                    payload("agent-inherited-after-release", *channel),
                ]
                .concat(),
                InheritedFinish::GuestExpiry { .. } => [
                    payload("agent", *channel),
                    payload("agent-inherited-ready", *channel),
                ]
                .concat(),
            };
            capture.receive(
                &fixture.observers[*index],
                &expected,
                ReadGoal::CanonicalEnd,
                until,
            );
            fixture.observers[*index]
                .sender
                .send(Command::Handler(Ask::Cancel))
                .unwrap();
            fixture.observers[*index].join(until).unwrap();
        }
    }
    shared.base.write("actor-channel-ends.json", &json!({"completion":format!("{finish:?}"),"provenance":"canonical stream End/ACK/EOF only; not independent death/wait/population proof"}));
}

fn run(
    fixture: &mut Fixture,
    input: &InheritedInputs,
    finish: InheritedFinish,
    until: Instant,
) -> Vec<(u32, Channel, Capture)> {
    let shared = &input.common;
    let actor = shared.actor.to_string();
    let sibling = shared.base.port.to_string();
    let work = shared.work_ms.to_string();
    let actor_work = finish.actor_work_ms(shared.work_ms).to_string();
    let cleanup = shared.cleanup_ms.to_string();
    let count = shared.challenge.len().to_string();
    let report = shared.report_bytes.to_string();
    let depth = input.depth.map(|depth| depth.to_string());
    let directives = format!(
        "NETWORK --ingress={actor} --ingress={sibling} control\nEXPOSE --agents=helper {actor} AS ingress FOR control\nEXPOSE --harnesses=driver {sibling} AS ingress FOR control\nCONNECT helper WITH helper ON control\nCONNECT driver WITH driver ON control\n"
    );
    let mut agent_args = vec![
        "managed-capture",
        "agent",
        "inherit-stdio",
        &actor,
        &actor_work,
        &cleanup,
        &count,
        &report,
    ];
    if let Some(depth) = &depth {
        agent_args.push(depth);
    }
    fixture.start_with_options(
        &agent_args,
        &["managed-capture", "harness", "step", &sibling, &work],
        &directives,
        Some(input.tasks),
        Some(&[shared.actor, shared.base.port]),
        until,
    );
    std::fs::copy(
        fixture.context.join("Agentfile"),
        shared.base.records.join("Agentfile"),
    )
    .unwrap();
    std::fs::copy(
        fixture.record.as_ref().unwrap(),
        shared.base.records.join("managed-output.ready"),
    )
    .unwrap();
    let mut channels: Vec<_> = [
        (1, Channel::Stdout),
        (1, Channel::Stderr),
        (2, Channel::Stdout),
        (2, Channel::Stderr),
    ]
    .into_iter()
    .map(|(m, c)| {
        (
            m,
            c,
            fixture.observe(m, c, None, false, until),
            Capture {
                trace: Some(shared.base.records.clone()),
                ..Capture::default()
            },
        )
    })
    .collect();
    for (member, channel, index, capture) in &mut channels {
        let expected = if *member == 1 {
            [
                payload("agent", *channel),
                payload("agent-inherited-ready", *channel),
            ]
            .concat()
        } else {
            payload("harness", *channel)
        };
        capture.receive(
            &fixture.observers[*index],
            &expected,
            ReadGoal::InitialPayload,
            until,
        );
    }
    shared.base.write("inherited-ready.json", &json!({"state":"ChildListenerReadyAndInheritedBytesAfterParentBothWritersClosed","channels":channels.iter().map(|(m,c,_,capture)|capture.record(*m,*c)).collect::<Vec<_>>(),"provenance":"controlled member/channel data; not arbitrary exact descendant attribution"}));
    if input.depth.is_some() {
        shared.base.command(
            fixture,
            "processes-at-descendant-ready.json",
            &["top", "owner-output", "-o", "pid,ppid,uid,stat,comm"],
            until,
        );
    }
    if let InheritedFinish::GuestExpiry { work_ms } = finish {
        for (member, channel, _, capture) in &channels {
            if *member == 1 {
                let expected = [
                    payload("agent", *channel),
                    payload("agent-inherited-ready", *channel),
                ]
                .concat();
                assert_eq!(capture.bytes, expected, "exact initial inherited bytes");
                assert_eq!(
                    capture.state,
                    StreamState::Open {
                        offset: u64::try_from(expected.len()).unwrap()
                    },
                    "initial inherited output remains Open before waiting for expiry"
                );
            }
        }
        shared.base.write("expiry-control-withheld.json", &json!({"state":"NoLeafChallengeConnectionOrBytesSent","actor_work_ms":work_ms,"sibling_work_ms":shared.work_ms,"host_work_ms":shared.base.patience.as_millis(),"channels":channels.iter().map(|(m,c,_,capture)|capture.record(*m,*c)).collect::<Vec<_>>() }));
        finish_inherited_actor(fixture, shared, &mut channels, finish, until);
        let host_start = until.checked_sub(shared.base.patience).unwrap();
        let elapsed = Instant::now().saturating_duration_since(host_start);
        shared.base.write("expiry-end-before-neighbor.json", &json!({"state":"BothActorCanonicalEndsAckAndTransportEofBeforeNeighborRelease","host_elapsed_ms":elapsed.as_millis(),"actor_work_ms":work_ms,"channels":channels.iter().map(|(m,c,_,capture)|capture.record(*m,*c)).collect::<Vec<_>>(),"direct_wait_population":"Unknown: stream End is not death or an independent wait receipt"}));
        assert!(
            elapsed >= Duration::from_millis(work_ms),
            "no early-End pass before the caller's guest work duration; host elapsed is not serialized guest time"
        );
    }
    fixture.main_progress(remaining(until).unwrap());
    release_neighbor(fixture, &shared.base, until);
    let release_evidence = match finish {
        InheritedFinish::Challenge => {
            "shared release caption is not an actor End witness; exact child reply and later suffix/End remain required"
        }
        InheritedFinish::GuestExpiry { .. } => {
            "both canonical actor channel Ends/ACK/EOF observed before gate release; no challenge sent; End is not death"
        }
    };
    shared.base.write("release-causality.json", &json!({"gate_peer_eof":true,"actor_last_consumed_states":channels.iter().filter(|(m,_,_,_)|*m==1).map(|(m,c,_,capture)|capture.record(*m,*c)).collect::<Vec<_>>(),"evidence":release_evidence}));
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
    }
    fixture.main_progress(remaining(until).unwrap());
    match finish {
        InheritedFinish::Challenge => {
            shared.base.write("sibling-before-child-release.json", &json!({"state":"NewSiblingFd1Fd2AndMainProgressAfterParentStdioClosureBeforeChildRelease","channels":channels.iter().map(|(m,c,_,capture)|capture.record(*m,*c)).collect::<Vec<_>>() }));
            exchange_inherited(fixture, shared, until);
            finish_inherited_actor(fixture, shared, &mut channels, finish, until);
        }
        InheritedFinish::GuestExpiry { .. } => {
            shared.base.write("sibling-after-expiry-end.json", &json!({"state":"NewSiblingFd1Fd2AndMainProgressAfterBothActorChannelEnds","channels":channels.iter().map(|(m,c,_,capture)|capture.record(*m,*c)).collect::<Vec<_>>() }));
        }
    }
    for (member, channel, _, capture) in &channels {
        fixture
            .observed
            .insert((*member, *channel as u8), (capture.wire, capture.wire));
    }
    fixture.main_progress(remaining(until).unwrap());
    shared.base.command(
        fixture,
        "processes-after-inherited-eof.json",
        &["top", "owner-output", "-o", "pid,ppid,uid,stat,comm"],
        until,
    );
    let state = match finish {
        InheritedFinish::Challenge => {
            "ExactInheritedChildSuffixThenCanonicalEndAckAndTransportEofOnBothChannels"
        }
        InheritedFinish::GuestExpiry { .. } => {
            "InheritedGuestExpiryThenCanonicalEndAckAndTransportEofOnBothChannels"
        }
    };
    shared.base.write("inherited-end.json", &json!({"state":state,"channels":channels.iter().map(|(m,c,_,capture)|capture.record(*m,*c)).collect::<Vec<_>>(),"actual_parent_waitstatus":"not independently exported after parent stdio closure; ROOT held lifecycle witness owed","population_reaping":"Unverified; EOF is not whole population completion"}));
    std::fs::copy(
        fixture.record.as_ref().unwrap(),
        shared.base.records.join("managed-output.before-stop"),
    )
    .unwrap();
    channels
        .into_iter()
        .map(|(m, c, _, capture)| (m, c, capture))
        .collect()
}

#[test]
#[ignore = "assigned bounded real VM; explicit tasks/two ports/challenge/work+cleanup/report envelope/evidence"]
fn inherited_child_writers_survive_parent_stdio_closure() {
    run_inherited_case(InheritedInputs::read(None), InheritedFinish::Challenge);
}

#[test]
#[ignore = "assigned bounded real VM; explicit finite depth/tasks/ports/challenge/work+cleanup/report/evidence"]
fn inherited_descendant_depth_preserves_neighbor_and_main() {
    let depth = std::env::var("SHARDS_TENANT_STDIO_DEPTH")
        .unwrap_or_else(|_| panic!("supply SHARDS_TENANT_STDIO_DEPTH"))
        .parse::<u32>()
        .unwrap();
    run_inherited_case(InheritedInputs::read(Some(depth)), InheritedFinish::Challenge);
}

#[test]
#[ignore = "one assigned D2 guest expiry; explicit half host work/unchanged sibling/cleanup/resources/evidence"]
fn inherited_d2_guest_expiry_preserves_subsequent_neighbor_and_main() {
    let depth = std::env::var("SHARDS_TENANT_STDIO_DEPTH")
        .unwrap_or_else(|_| panic!("supply SHARDS_TENANT_STDIO_DEPTH"))
        .parse::<u32>()
        .unwrap();
    let input = InheritedInputs::read(Some(depth));
    let work_ms = std::env::var("SHARDS_TENANT_STDIO_ACTOR_WORK_MS")
        .unwrap_or_else(|_| panic!("supply SHARDS_TENANT_STDIO_ACTOR_WORK_MS"))
        .parse::<u64>()
        .unwrap();
    let host_work = u64::try_from(input.common.base.patience.as_millis()).unwrap();
    assert_eq!(depth, 2, "one explicitly approved D2 case");
    assert_eq!(input.tasks, depth.checked_add(4).unwrap());
    assert_eq!(input.common.work_ms, host_work, "unchanged sibling work budget");
    assert!(work_ms > 0);
    assert_eq!(
        Some(work_ms),
        host_work.checked_div(2),
        "ROOT caller budget split, not a runtime default"
    );
    run_inherited_case(input, InheritedFinish::GuestExpiry { work_ms });
}

fn run_inherited_case(input: InheritedInputs, finish: InheritedFinish) {
    let shared = &input.common;
    assert!(!common::cannot_run_vms(), "requires a supported real VM host");
    let until = Instant::now().checked_add(shared.base.patience).unwrap();
    let cleanup_until = until
        .checked_add(Duration::from_millis(shared.cleanup_ms))
        .unwrap();
    let artifact = |path: &std::path::Path| json!({"path":path,"sha256":common::sha256_digest(&std::fs::read(path).unwrap())});
    shared.base.write("artifacts.json", &json!({"fixture":artifact(&std::env::current_exe().unwrap()),"cli":artifact(common::shards()),"testguest":artifact(common::test_guest()),"kernel":artifact(common::kernel()),"init":artifact(common::guest_init())}));
    shared.base.write("inputs.json", &json!({"actor_port":shared.actor,"sibling_port":shared.base.port,"agent_tasks":input.tasks,"descendant_depth":input.depth,"private_handoff_bytes":input.depth.unwrap_or(1),"completion":format!("{finish:?}"),"actor_work_ms":finish.actor_work_ms(shared.work_ms),"work_ms":shared.work_ms,"host_work_ms":shared.base.patience.as_millis(),"cleanup_ms":shared.cleanup_ms,"report_bytes":shared.report_bytes,"challenge":shared.challenge,"source":option_env!("SHARDS_TENANT_STDIO_SOURCE_REVISION"),"lifetime_owner":"retained Fixture owner; each direct parent retains one private Child; observer/EOF is not owner","deadline_provenance":"depth mode: first actor creates one guest CLOCK_MONOTONIC expiry; unchanged across exec; cleanup derives from that expiry plus original duration; host deadline is independent"}));
    let mut fixture = Fixture::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&mut fixture, &input, finish, until)
    }));
    if let Some(path) = &fixture.record {
        let _ = std::fs::copy(path, shared.base.records.join("managed-output.before-cleanup"));
        let history = (|| -> io::Result<Vec<String>> {
            let parent = path.parent().ok_or(io::ErrorKind::NotFound)?;
            let destination = shared.base.records.join("history-before-cleanup");
            std::fs::create_dir(&destination)?;
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
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                    std::fs::copy(entry.path(), destination.join(&name))?;
                    names.push(name.to_string_lossy().into_owned());
                }
            }
            Ok(names)
        })();
        let processes = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fixture.command(
                &["read", "processes", "owner-output"],
                cleanup_until.saturating_duration_since(Instant::now()),
            )
        }));
        let read = match processes {
            Ok(run) => json!({"status":run.status,"stdout":run.stdout,"stderr":run.stderr}),
            Err(_) => json!({"state":"Unknown","cause":"read-processes helper panicked"}),
        };
        shared.base.write("history-before-cleanup.json", &json!({"read":read,"segments":history.as_ref().ok(),"copy_error":history.as_ref().err().map(ToString::to_string),"completeness":"Unknown: asynchronous/lost/rotated records and omitted threads remain possible; namespace PIDs are not VM PIDs"}));
    }
    let removed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        remove_run(&fixture, &shared.base, cleanup_until)
    }));
    let stopped = fixture.shutdown(cleanup_until);
    shared.base.write("teardown.json", &json!({"protocol_complete":result.is_ok(),"run_removed":removed.is_ok(),"owner_stop_observer_join":{"complete":stopped.is_ok(),"error":stopped.as_ref().err()},"observer_fd_baseline":fixture.initial_fds,"parent_child_waitstatus_and_full_population_return":"Unverified; ROOT held watches owed; cleanup Unknown retains Child until outer owner stop"}));
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
    stopped.unwrap();
    let metadata = removed.unwrap_or_else(|error| std::panic::resume_unwind(error));
    assert_terminal_metadata(&metadata, &result.unwrap());
}
