//! An image booted for an orchestrator (D231), through shards-api's `ImageShards` and this
//! build's `shards api-run`, with no daemon: real microVMs on this host's hypervisor.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::time::Duration;

use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};
use shards_api::image::{
    Channel, ImageGuest, ImageRefusal, ImageShards, ImageShare, ImageSpec, Images as _, Keep, MemberEnd,
    MemberFile, MemberKind, MemberName, MemberReach, MemberSpec, Narrowing, PlugRefusal, Proto, Publish,
    RunEnd, Secret, StatsScope,
};

const TIMEOUT: Duration = Duration::from_secs(120);

/// The variables a test's run host is given: its home, this build's guest, and the tests'
/// own docker config and local store (`common::as_tests_run`'s).
fn host_env(home: &TempDir) -> Vec<(OsString, OsString)> {
    vec![
        ("SHARDS_HOME".into(), home.as_os_str().into()),
        ("SHARDS_KERNEL".into(), kernel().as_os_str().into()),
        ("SHARDS_INIT".into(), guest_init().as_os_str().into()),
        ("SHARDS_LOCAL_STORE".into(), "none".into()),
        ("DOCKER_CONFIG".into(), common::docker_config().as_os_str().into()),
    ]
}

/// Builds `agentfile` (its `{image}` the test image) in a context holding a harness
/// archive whose `codex` is the test guest started with `options`, and an agent of files
/// alone; tagged `tag` in `home`.
fn build(home: &TempDir, tag: &str, options: &[&str], agentfile: &str) {
    let (image, _) = served();
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let ctx = TempDir::new("api-image-ctx");
    std::fs::write(ctx.join("Dockerfile"), format!("FROM {image}\n")).unwrap();
    std::fs::create_dir(ctx.join("helper")).unwrap();
    std::fs::write(ctx.join("helper/notes.txt"), "files alone\n").unwrap();
    let binary = std::fs::read(common::test_guest()).unwrap();
    let options: Vec<String> = options.iter().map(|o| format!("{o:?}")).collect();
    let descriptor = format!(r#"{{"entrypoint":"codex","options":[{}]}}"#, options.join(","));
    std::fs::write(
        ctx.join("harness.tar"),
        common::tar(&[
            ("bin", 0o755, 0, None),
            ("bin/codex", 0o755, 0, Some(&binary)),
            ("harness.json", 0o644, 0, Some(descriptor.as_bytes())),
        ]),
    )
    .unwrap();
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nAGENT helper FROM ./helper\nHARNESS codex FROM ./harness.tar\n{agentfile}"),
    )
    .unwrap();
    let built = run_shards_env(
        &["build", "image"],
        &["-t", tag, ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    assert_eq!(built.status, Some(0), "{}", built.stderr);
}

fn spec(home: &TempDir, image: &str) -> ImageSpec {
    ImageSpec {
        image: image.into(),
        vcpus: 1,
        memory_mib: 512,
        command: None,
        members: Vec::new(),
        publish: Vec::new(),
        egress: Narrowing::default(),
        credentials: Vec::new(),
        shares: Vec::new(),
        nesting: false,
        ready_by: None,
        binary: Some(common::shards().to_path_buf()),
        env: host_env(home),
    }
}

fn codex() -> MemberName {
    MemberName::harness("codex")
}

/// Reads `channel` of the harness to its end, within the test's bound.
fn read_all(guest: &dyn ImageGuest, channel: Channel) -> Vec<u8> {
    let mut pipe = guest.take_output(&codex(), channel).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        let _ = tx.send(bytes);
    });
    rx.recv_timeout(TIMEOUT)
        .expect("the member's channel did not end")
}

/// What an orchestrator stages into its harness at create reaches it alone, exactly, never
/// in the image: its variable on its stdout and its file on its stderr come back byte for
/// byte, each on its own channel, then its status; the members the image declares are
/// listed, the files-alone agent running nothing (D231).
#[test]
fn staged_credentials_reach_the_harness_and_its_output_and_end_come_back() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-image-home");
    build(
        &home,
        "api-staged:1",
        &["staged", "OPENAI_API_KEY", "/tmp/.codex/auth.json", "3"],
        "",
    );
    let token = b"{\"token\":\"t0p-secret\"}\n\x00\xff no newline";
    let mut s = spec(&home, "api-staged:1");
    s.members = vec![MemberSpec {
        member: codex(),
        env: vec![("OPENAI_API_KEY".into(), Secret::from("sk-orchestrated-0"))],
        files: vec![MemberFile {
            path: ".codex/auth.json".into(),
            mode: 0o600,
            bytes: Secret(token.to_vec()),
        }],
        workdir: None,
        stdin: false,
        terminal: false,
    }];
    let guest = ImageShards::new().boot(&s, TIMEOUT).unwrap();
    let members: Vec<(MemberKind, &str, bool)> = guest
        .members()
        .iter()
        .map(|m| (m.kind, m.name.as_str(), m.runs))
        .collect();
    assert_eq!(
        members,
        [
            (MemberKind::Agent, "helper", false),
            (MemberKind::Harness, "codex", true)
        ]
    );
    assert_eq!(read_all(&*guest, Channel::Stdout), b"sk-orchestrated-0");
    assert_eq!(read_all(&*guest, Channel::Stderr), token);
    assert_eq!(
        guest.wait_member(&codex(), TIMEOUT).unwrap(),
        Some(MemberEnd::Exited(3))
    );
    // Nothing of the secret lies in the host's home: it went on the channel alone.
    let mut found = Vec::new();
    let mut dirs = vec![home.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => dirs.push(path),
                Ok(t) if t.is_file() => {
                    let bytes = std::fs::read(&path).unwrap_or_default();
                    if bytes.windows(10).any(|w| w == b"t0p-secret") {
                        found.push(path);
                    }
                }
                _ => {}
            }
        }
    }
    assert!(found.is_empty(), "the secret is in {found:?}");
    guest.stop().unwrap();
    assert_eq!(guest.wait_for(TIMEOUT).unwrap(), Some(RunEnd::Stopped));
}

/// What would widen the image's grants, or names a member it does not declare or one of
/// files alone, is refused by type before anything boots: members held apart (D246) that
/// would reach other than asked among them, the nesting grant (D234) asked of an image
/// declaring no NESTING, in the daemon's words; and a deadline without the grant, the
/// request's.
#[test]
fn what_the_image_does_not_grant_is_refused_before_boot() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-refused-home");
    build(&home, "api-refused:1", &["exit", "0"], "");
    let staged = |member: MemberName| MemberSpec {
        member,
        env: vec![("K".into(), Secret::from("v"))],
        files: Vec::new(),
        workdir: None,
        stdin: false,
        terminal: false,
    };
    let refused = |s: ImageSpec| match ImageShards::new().boot(&s, TIMEOUT) {
        Err(e) => e,
        Ok(_) => panic!("booted"),
    };
    let mut s = spec(&home, "api-refused:1");
    s.members = vec![staged(MemberName::harness("nobody"))];
    assert!(matches!(refused(s), ImageRefusal::Image(_)));
    let mut s = spec(&home, "api-refused:1");
    s.members = vec![staged(MemberName::agent("helper"))];
    assert!(matches!(refused(s), ImageRefusal::Image(_)));
    let mut s = spec(&home, "api-refused:1");
    s.egress = Narrowing {
        egress: Keep::Only(vec!["22/tcp".into()]),
        ..Narrowing::default()
    };
    assert!(matches!(refused(s), ImageRefusal::Widening(_)));
    // A destination the image grants none of (D236).
    let mut s = spec(&home, "api-refused:1");
    s.egress = Narrowing {
        destinations: Keep::Only(vec!["host.docker.internal:7070".into()]),
        ..Narrowing::default()
    };
    assert!(matches!(refused(s), ImageRefusal::Widening(_)));
    // Members held apart (D246), of an image whose harness alone reaches a destination:
    // that destination kept with the harness let reach nothing, or with the agent of files
    // alone let reach it too, would let a member reach other than asked; and a member the
    // image declares not.
    build(
        &home,
        "api-refused:2",
        &["exit", "0"],
        "NETWORK --egress=api.example:443 provider FOR codex\n\
         EXPOSE --harnesses=codex 443 AS egress FOR provider\nCONNECT codex WITH codex ON provider\n",
    );
    let apart = |members: Vec<MemberReach>| Narrowing {
        egress: Keep::Only(Vec::new()),
        mcp: Keep::Only(Vec::new()),
        destinations: Keep::Only(vec!["api.example:443".into()]),
        dns: false,
        members: Some(members),
    };
    let reach = |kind, name: &str, destinations: &[&str]| MemberReach {
        kind,
        name: name.into(),
        destinations: destinations.iter().map(|d| (*d).to_string()).collect(),
    };
    for members in [
        Vec::new(),
        vec![
            reach(MemberKind::Harness, "codex", &["api.example:443"]),
            reach(MemberKind::Agent, "helper", &["api.example:443"]),
        ],
    ] {
        let mut s = spec(&home, "api-refused:2");
        s.egress = apart(members);
        assert!(matches!(refused(s), ImageRefusal::Widening(_)));
    }
    let mut s = spec(&home, "api-refused:2");
    s.egress = apart(vec![reach(MemberKind::Harness, "nobody", &[])]);
    assert!(matches!(refused(s), ImageRefusal::Image(_)));
    let mut s = spec(&home, "api-refused:1");
    s.publish = vec![Publish {
        guest_port: 9000,
        proto: Proto::Tcp,
        host: Some("127.0.0.1".parse().unwrap()),
        host_port: 0,
    }];
    assert!(matches!(refused(s), ImageRefusal::Widening(_)));
    let mut s = spec(&home, "api-refused:1");
    s.nesting = true;
    s.ready_by = Some(Duration::from_secs(60));
    assert_eq!(
        refused(s),
        ImageRefusal::Widening(shards_abi::run::nesting::undeclared())
    );
    let mut s = spec(&home, "api-refused:1");
    s.ready_by = Some(Duration::from_secs(60));
    assert_eq!(
        refused(s),
        ImageRefusal::Spec {
            what: "the request",
            why: shards_abi::run::nesting::Error::Ungranted("ready-by=").to_string(),
        }
    );
}

/// A host directory shared at the path of a `VOLUME` the image gives its harness reaches
/// the harness, read-only as asked (D109): its file read whole, and a write refused.
#[test]
fn a_share_at_a_granted_volume_reaches_the_harness() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-share-home");
    build(
        &home,
        "api-share:1",
        &["files", "/work/hello.txt"],
        "VOLUME --target-kind=harness /work FOR codex\n",
    );
    let shared = TempDir::new("api-share-dir");
    std::fs::write(shared.join("hello.txt"), "from the host\n").unwrap();
    let mut s = spec(&home, "api-share:1");
    s.shares = vec![ImageShare {
        host: shared.to_path_buf(),
        at: "/work".into(),
        read_only: true,
    }];
    let guest = ImageShards::new().boot(&s, TIMEOUT).unwrap();
    let said = String::from_utf8(read_all(&*guest, Channel::Stdout)).unwrap();
    assert_eq!(said, "== /work/hello.txt\nfrom the host\n");
    assert_eq!(
        guest.wait_member(&codex(), TIMEOUT).unwrap(),
        Some(MemberEnd::Exited(0))
    );
}

/// A guest port the image gives its harness coming in, published on a host port the
/// orchestrator lets the OS pick, reaches the harness (D122).
#[test]
fn a_published_ingress_port_reaches_its_member() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-port-home");
    build(
        &home,
        "api-port:1",
        &["confined", "listen", "7100"],
        "NETWORK --ingress=7100 front\n\
         EXPOSE --harnesses=codex 7100 AS ingress FOR front\n\
         CONNECT --target-kind=harness codex WITH codex ON front\n",
    );
    let mut s = spec(&home, "api-port:1");
    s.publish = vec![Publish {
        guest_port: 7100,
        proto: Proto::Tcp,
        host: Some("127.0.0.1".parse().unwrap()),
        host_port: 0,
    }];
    let guest = ImageShards::new().boot(&s, TIMEOUT).unwrap();
    let bound = guest.published().first().cloned().expect("a bound port");
    assert_eq!((bound.guest_port, bound.proto), (7100, Proto::Tcp));
    // It names whom it reaches: the harness, at its own port.
    let [to] = bound.to.as_slice() else {
        panic!("{bound:?}");
    };
    let member = guest.members().iter().find(|m| m.index == to.member).unwrap();
    assert_eq!(
        (member.kind, member.name.as_str(), to.port),
        (MemberKind::Harness, "codex", 7100)
    );
    // The listener comes up once its member runs: asked until it answers, within the bound.
    let deadline = std::time::Instant::now() + TIMEOUT;
    let answer = loop {
        if let Ok(mut c) = std::net::TcpStream::connect(bound.host) {
            c.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let _ = c.write_all(b"hello\n");
            let mut got = Vec::new();
            let _ = c.read_to_end(&mut got);
            if !got.is_empty() {
                break got;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no answer on {}",
            bound.host
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(!answer.is_empty());
    guest.stop().unwrap();
}

/// A project plugged into the running harness through its run's own channel (D235), as
/// an orchestrator plugs one: refused outside the run's plug roots, and for a member that
/// runs nothing, each by its kind; then at its host path, read and written by the harness,
/// its writes the host's, with nothing made beneath a root before; then unplugged, and
/// refused unplugged again.
#[test]
fn a_project_is_plugged_into_the_running_harness() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-plug-home");
    let files = TempDir::new("api-plug-files");
    for dir in ["proj", "bound"] {
        std::fs::create_dir(files.join(dir)).unwrap();
    }
    let proj = std::fs::canonicalize(files.join("proj")).unwrap();
    let bound = std::fs::canonicalize(files.join("bound")).unwrap();
    std::fs::write(proj.join("from-host"), "host\n").unwrap();
    let dest = common::at_host_path(&proj);
    let roots = common::plug_roots(&bound);
    let mut options = vec!["plugged", dest.as_str()];
    options.extend(roots.iter().map(String::as_str));
    build(&home, "api-plug:1", &options, "");
    let mut s = spec(&home, "api-plug:1");
    s.shares = vec![ImageShare {
        host: bound.clone(),
        at: "/bound".into(),
        read_only: true,
    }];
    let guest = ImageShards::new().boot(&s, TIMEOUT).unwrap();
    match guest.plug(&codex(), &proj, "/outside/proj", false, TIMEOUT) {
        Err(ImageRefusal::Plug(PlugRefusal::Outside, why)) => {
            assert!(why.contains(roots[0].as_str()), "{why}");
        }
        other => panic!("{other:?}"),
    }
    let helper = MemberName {
        kind: MemberKind::Agent,
        name: "helper".into(),
    };
    assert!(
        matches!(
            guest.plug(&helper, &proj, &dest, false, TIMEOUT),
            Err(ImageRefusal::Plug(PlugRefusal::Member, _))
        ),
        "a member of files alone"
    );
    guest.plug(&codex(), &proj, &dest, false, TIMEOUT).unwrap();
    let seen_at = proj.join("seen");
    let started = std::time::Instant::now();
    let seen = loop {
        if let Ok(seen) = std::fs::read_to_string(&seen_at) {
            break seen;
        }
        assert!(
            started.elapsed() < TIMEOUT,
            "the harness wrote no {}",
            seen_at.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    for root in &roots {
        let line = format!("root {root} mkdir errno 30 create errno 30");
        assert!(seen.lines().any(|l| l == line), "no {line:?} in\n{seen}");
    }
    assert!(seen.lines().any(|l| l == "from-host host"), "{seen}");
    assert_eq!(
        std::fs::read_to_string(proj.join("from-member")).unwrap(),
        "member\n"
    );
    guest.unplug(&dest, TIMEOUT).unwrap();
    assert!(matches!(
        guest.unplug(&dest, TIMEOUT),
        Err(ImageRefusal::Plug(PlugRefusal::Unknown, _))
    ));
    guest.stop().unwrap();
    assert_eq!(guest.wait_for(TIMEOUT).unwrap(), Some(RunEnd::Stopped));
}

/// Stats read the run's cgroup and a running member's, typed; a member that does not run
/// is refused by its guest.
#[test]
fn stats_read_the_run_and_a_member() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-stats-home");
    build(&home, "api-stats:1", &["sleep-for", "60000"], "");
    let guest = ImageShards::new()
        .boot(&spec(&home, "api-stats:1"), TIMEOUT)
        .unwrap();
    let run = guest.stats(StatsScope::Run, TIMEOUT).unwrap();
    assert!(run.memory_bytes.is_some_and(|m| m > 0), "{run:?}");
    assert!(run.cpu_ns.is_some(), "{run:?}");
    // The run's microVM beside its guest's cgroup (D241): its VM process's use, and what its
    // one vCPU has run of its guest, by its index.
    let microvm = run.microvm.as_ref().expect("the run's microVM");
    let host = microvm.host.as_ref().expect("its VM process's use");
    assert!(host.cpu_ns > 0 && host.footprint_bytes > 0, "{host:?}");
    let times = microvm.guest.as_ref().expect("its vCPUs' guest time");
    assert_eq!(
        times
            .vcpus
            .iter()
            .map(|v| (v.index, v.unread.is_none()))
            .collect::<Vec<_>>(),
        [(0, true)],
        "{times:?}"
    );
    let member = guest.stats(StatsScope::Member(1), TIMEOUT).unwrap();
    assert!(member.pids.is_some_and(|p| p >= 1), "{member:?}");
    assert_eq!(member.microvm, None, "a member's scope is inside the microVM");
    assert!(guest.stats(StatsScope::Member(9), TIMEOUT).is_err());
    guest.stop().unwrap();
    assert_eq!(guest.wait_for(TIMEOUT).unwrap(), Some(RunEnd::Stopped));
}

/// The room an orchestrator's budget leaves its run (D241) reaches the run's init through
/// its host: told a room past the least a run goes on in, the run is held within it, in
/// whole pages; told none left, within the least, short of what it had; told no budget,
/// its hold is lifted and nothing is answered.
#[test]
fn the_orchestrators_room_holds_its_run_within_it() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-room-home");
    build(&home, "api-room:1", &["sleep-for", "60000"], "");
    let guest = ImageShards::new()
        .boot(&spec(&home, "api-room:1"), TIMEOUT)
        .unwrap();
    let room: u64 = 64 << 20;
    let held = guest
        .room(Some(i64::try_from(room).unwrap()), TIMEOUT)
        .unwrap()
        .expect("held within the room told");
    // Whole pages, as `memory.high` and `memory.current` are kept: exactly the room told.
    assert_eq!(held, room);
    let least = guest
        .room(Some(0), TIMEOUT)
        .unwrap()
        .expect("held within the least room");
    assert!(least > 0 && least < held, "held within {least}, short of {held}");
    assert_eq!(guest.room(None, TIMEOUT).unwrap(), None);
    guest.stop().unwrap();
    assert_eq!(guest.wait_for(TIMEOUT).unwrap(), Some(RunEnd::Stopped));
}

/// Stop ends the run at once, whatever its members do, and its end says so.
#[test]
fn stop_ends_the_run_at_once() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-stop-home");
    build(&home, "api-stop:1", &["sleep-for", "600000"], "");
    let guest = ImageShards::new()
        .boot(&spec(&home, "api-stop:1"), TIMEOUT)
        .unwrap();
    assert_eq!(guest.wait_member(&codex(), Duration::ZERO).unwrap(), None);
    guest.stop().unwrap();
    assert_eq!(guest.wait_for(Duration::ZERO).unwrap(), Some(RunEnd::Stopped));
    assert_eq!(
        guest.wait_member(&codex(), Duration::ZERO).unwrap(),
        Some(MemberEnd::WithRun)
    );
}

/// Plan step 5 through the orchestrator's path: a harness whose stdin its spec asks for
/// reads exactly what the orchestrator writes into its pipe, JSON lines, bytes no UTF-8
/// holds, no final newline and more than a pipe holds, which it echoes to its stdout; the
/// pipe's close is its end, after which its stdout ends with the same bytes and its stderr
/// says how many it read. A harness whose spec does not ask for it has no stdin to take.
#[test]
fn a_harness_reads_its_orchestrators_stdin_exactly_and_its_close_is_its_end() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-stdin-home");
    build(&home, "api-stdin:1", &["managed-capture", "codex", "echo"], "");
    let mut s = spec(&home, "api-stdin:1");
    s.members = vec![MemberSpec {
        member: codex(),
        env: Vec::new(),
        files: Vec::new(),
        workdir: None,
        stdin: true,
        terminal: false,
    }];
    let guest = ImageShards::new().boot(&s, TIMEOUT).unwrap();
    let mut input = Vec::new();
    input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n");
    input.extend_from_slice(b"\0\xff\x80\x1b[2Jno-newline\xc3\x28");
    input.extend((0..300 * 1024u32).map(|i| (i % 251) as u8));
    let mut writer = guest.take_input(&codex()).unwrap();
    assert!(guest.take_input(&codex()).is_err(), "taken once");
    let echoed = std::thread::scope(|scope| {
        let reading = scope.spawn(|| read_all(guest.as_ref(), Channel::Stdout));
        writer.write_all(&input).unwrap();
        drop(writer);
        reading.join().unwrap()
    });
    assert_eq!(echoed.len(), input.len());
    assert!(
        echoed == input,
        "the harness echoed other bytes than its stdin took"
    );
    let said = read_all(guest.as_ref(), Channel::Stderr);
    assert_eq!(said, format!("codex\0echo-end {}", input.len()).into_bytes());
    assert_eq!(
        guest.wait_member(&codex(), TIMEOUT).unwrap(),
        Some(MemberEnd::Exited(0))
    );
    guest.stop().unwrap();
    // Not asked for: no stdin.
    let home = TempDir::new("api-nostdin-home");
    build(&home, "api-nostdin:1", &["managed-capture", "codex", "echo"], "");
    let guest = ImageShards::new()
        .boot(&spec(&home, "api-nostdin:1"), TIMEOUT)
        .unwrap();
    assert!(guest.take_input(&codex()).is_err());
    guest.stop().unwrap();
}
