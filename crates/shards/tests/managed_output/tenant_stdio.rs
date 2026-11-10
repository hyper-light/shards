//! Actor output EOF with causally later actor control and sibling/main production.
use super::*;
use std::io;

#[path = "current_tenant_stdio.rs"]
mod current_tenant_stdio;

struct Supplied {
    base: Inputs,
    actor: u16,
    work_ms: u64,
    cleanup_ms: u64,
    report_bytes: u64,
    challenge: Vec<u8>,
}

impl Supplied {
    fn read() -> Self {
        let text = |key: &str| {
            std::env::var(format!("SHARDS_TENANT_STDIO_{key}"))
                .unwrap_or_else(|_| panic!("supply SHARDS_TENANT_STDIO_{key}"))
        };
        let number = |key: &str| text(key).parse::<u64>().unwrap();
        let port = u16::try_from(number("PORT")).unwrap();
        let actor = u16::try_from(number("ACTOR_PORT")).unwrap();
        let patience = Duration::from_millis(number("DEADLINE_MS"));
        let work_ms = number("WORK_MS");
        let cleanup_ms = number("CLEANUP_MS");
        assert!(
            port > 0 && actor > 0 && port != actor,
            "two distinct explicit guest ports"
        );
        assert!(
            !patience.is_zero() && work_ms > 0 && cleanup_ms > 0,
            "positive finite deadlines"
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
        let hex = text("CHALLENGE_HEX");
        assert!(
            !hex.is_empty() && hex.len().is_multiple_of(2),
            "explicit nonempty byte challenge"
        );
        let challenge: Vec<_> = hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        let count = u32::try_from(challenge.len()).unwrap();
        let report_bytes = number("REPORT_BYTES");
        let required = payload("agent", Channel::Stdout)
            .len()
            .checked_add(payload("agent", Channel::Stderr).len())
            .unwrap()
            .checked_add(
                (size_of::<u8>() + size_of::<u32>() + usize::try_from(count).unwrap())
                    .checked_mul(2)
                    .unwrap(),
            )
            .unwrap();
        assert!(
            u64::try_from(required).unwrap() <= report_bytes,
            "bounded capture plus owner request/reply"
        );
        let records = PathBuf::from(text("RECORDS"));
        assert!(records.is_absolute(), "new absolute evidence directory");
        std::fs::create_dir(&records).unwrap();
        Self {
            base: Inputs {
                port,
                patience,
                records,
            },
            actor,
            work_ms,
            cleanup_ms,
            report_bytes,
            challenge,
        }
    }
}

fn remaining(until: Instant) -> io::Result<Duration> {
    let left = until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(io::ErrorKind::TimedOut.into())
    } else {
        Ok(left)
    }
}

fn exchange(fixture: &Fixture, input: &Supplied, until: Instant) {
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
    input.base.write("actor-control.json", &json!({"state":"OwnerChallengeAfterBothActorOutputEofs","sent":sent,"received":received,"complete":result.is_ok(),"error":result.as_ref().err().map(ToString::to_string),"cause_known_to_host":"transport IO only; actor IO failure phase/history can remain Unknown"}));
    result.unwrap();
    assert_eq!(sent, frame);
    assert_eq!(received, frame, "fresh actor response after output EOF");
}

fn run(fixture: &mut Fixture, input: &Supplied, until: Instant) -> Vec<(u32, Channel, Capture)> {
    let port = input.base.port.to_string();
    let actor = input.actor.to_string();
    let work = input.work_ms.to_string();
    let count = input.challenge.len().to_string();
    let envelope = input.report_bytes.to_string();
    let directives = format!(
        "NETWORK --ingress={actor} --ingress={port} control\nEXPOSE --agents=helper {actor} AS ingress FOR control\nEXPOSE --harnesses=driver {port} AS ingress FOR control\nCONNECT helper WITH helper ON control\nCONNECT driver WITH driver ON control\n"
    );
    fixture.start_with_options(
        &[
            "managed-capture",
            "agent",
            "close-stdio",
            &actor,
            &work,
            &count,
            &envelope,
        ],
        &["managed-capture", "harness", "step", &port, &work],
        &directives,
        Some(&[input.actor, input.base.port]),
        until,
    );
    std::fs::copy(
        fixture.context.join("Agentfile"),
        input.base.records.join("Agentfile"),
    )
    .unwrap();
    std::fs::copy(
        fixture.record.as_ref().unwrap(),
        input.base.records.join("managed-output.ready"),
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
                trace: Some(input.base.records.clone()),
                ..Capture::default()
            },
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
            fixture.observers[*index]
                .sender
                .send(Command::Handler(Ask::Cancel))
                .unwrap();
            fixture.observers[*index].join(until).unwrap();
        }
    }
    input.base.write("actor-eof.json", &json!({"state":"BothActorChannelsCanonicalEndAckAndTransportEof","channels":channels.iter().map(|(m,c,_,capture)|capture.record(*m,*c)).collect::<Vec<_>>(),"lifecycle":"EOF is output evidence only; actor response must now follow"}));
    std::fs::copy(
        fixture.record.as_ref().unwrap(),
        input.base.records.join("managed-output.actor-eof"),
    )
    .unwrap();
    fixture.main_progress(remaining(until).unwrap());
    exchange(fixture, input, until);
    fixture.main_progress(remaining(until).unwrap());
    release_neighbor(fixture, &input.base, until);
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
    fixture.main_progress(remaining(until).unwrap());
    let live = input.base.command(
        fixture,
        "actor-live-after-eof.json",
        &["top", "owner-output", "-o", "pid,ppid,uid,stat,comm"],
        until,
    );
    let actor_uid = shards_abi::DOMAIN_FIRST_ID.checked_add(1).unwrap().to_string();
    assert!(
        live.stdout.lines().skip(1).any(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            fields.get(2) == Some(&actor_uid.as_str())
                && fields
                    .get(3)
                    .is_some_and(|state| !state.starts_with('Z') && !state.starts_with('X'))
        }),
        "actor has a visible non-zombie row after output EOF and all subsequent progress: {live}"
    );
    input.base.write("subsequent-progress.json", &json!({"state":"ActorChallengeAndNewSiblingFd1Fd2AndIndependentMainAfterActorOutputEof","channels":channels.iter().map(|(m,c,_,capture)|capture.record(*m,*c)).collect::<Vec<_>>(),"provenance":"controlled actor/member data and visible actor row only; no arbitrary exact descendant/tool attribution or population proof"}));
    std::fs::copy(
        fixture.record.as_ref().unwrap(),
        input.base.records.join("managed-output.before-stop"),
    )
    .unwrap();
    channels
        .into_iter()
        .map(|(m, c, _, capture)| (m, c, capture))
        .collect()
}

#[test]
#[ignore = "coordinator-only real VM; explicit two ports/challenge/byte envelope/work+cleanup/evidence"]
fn actor_stdio_eof_preserves_actor_control_and_neighbor_lifetime() {
    let input = Supplied::read();
    assert!(!common::cannot_run_vms(), "requires a real supported VM host");
    let until = Instant::now().checked_add(input.base.patience).unwrap();
    let cleanup_until = until
        .checked_add(Duration::from_millis(input.cleanup_ms))
        .unwrap();
    let artifact = |path: &std::path::Path| json!({"path":path,"sha256":common::sha256_digest(&std::fs::read(path).unwrap())});
    input.base.write("artifacts.json", &json!({"fixture":artifact(&std::env::current_exe().unwrap()),"cli":artifact(common::shards()),"testguest":artifact(common::test_guest()),"kernel":artifact(common::kernel()),"init":artifact(common::guest_init())}));
    input.base.write("inputs.json", &json!({"actor_port":input.actor,"sibling_port":input.base.port,"actor_work_ms":input.work_ms,"host_work_ms":input.base.patience.as_millis(),"cleanup_ms":input.cleanup_ms,"report_bytes":input.report_bytes,"challenge":input.challenge,"source":option_env!("SHARDS_TENANT_STDIO_SOURCE_REVISION"),"lifetime_owner":"retained Fixture owner; output observers and EOF are not lifecycle ownership"}));
    let mut fixture = Fixture::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut fixture, &input, until)));
    if let Some(path) = &fixture.record {
        let _ = std::fs::copy(path, input.base.records.join("managed-output.before-cleanup"));
    }
    let removed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        remove_run(&fixture, &input.base, cleanup_until)
    }));
    let stopped = fixture.shutdown(cleanup_until);
    input.base.write("teardown.json", &json!({"protocol_complete":result.is_ok(),"run_removed":removed.is_ok(),"owner_stop_observer_join":{"complete":stopped.is_ok(),"error":stopped.as_ref().err()},"observer_fd_baseline":fixture.initial_fds,"full_population_reaping_kernel_return":"Unverified; ROOT independent held-watch/resource gates owed"}));
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
    stopped.unwrap();
    let metadata = removed.unwrap_or_else(|error| std::panic::resume_unwind(error));
    assert_terminal_metadata(&metadata, &result.unwrap());
}
