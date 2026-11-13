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
use std::num::NonZeroU64;
use std::time::{Duration, Instant};

use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};
use shards_api::image::{
    BootStop, Captured, Channel, Cli, CredentialGrant, ImageGuest, ImagePools, ImageRefusal, ImageShards,
    ImageShare, ImageSpec, Images as _, Keep, LeaseRefusal, Leased, Lessor, MemberEnd, MemberFile,
    MemberKind, MemberName, MemberReach, MemberSpec, Narrowing, PlugRefusal, Proto, Publish, RunEnd,
    RunLessor, SaveRefusal, Secret, SignInGrant, SignInRefusal, SignInTokens, StartedFrom, StatsScope,
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
    let built = run_shards_env(&["build", "image"], &["-t", tag, ctx.to_str().unwrap()], &env);
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
        lessor: None,
        shares: Vec::new(),
        nesting: false,
        ready_by: None,
        binary: Some(common::shards().to_path_buf()),
        env: host_env(home),
        control: None,
        gpu_devices: Vec::new(),
        sign_ins: Vec::new(),
    }
}

fn codex() -> MemberName {
    MemberName::harness("codex")
}

/// Where the diagnostics of the run hosts a test boots in `home` are kept whole
/// (`ImageGuest::keep_diagnostics`), each appended after the last: a `.stderr` of the home's,
/// which a failing test shows as its home goes (`common::TempDir`).
fn diagnostics(home: &TempDir) -> std::path::PathBuf {
    home.join("run-hosts.stderr")
}

/// `s` booted, its run host's diagnostics kept whole in [`diagnostics`] from its start.
fn boot(home: &TempDir, s: &ImageSpec) -> Box<dyn ImageGuest> {
    let guest = ImageShards::new().boot(s, TIMEOUT).unwrap();
    let kept = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(diagnostics(home))
        .unwrap();
    guest.keep_diagnostics(kept).unwrap();
    guest
}

/// The harness's end, held to `expected`; where it is not, the run's own end is said beside
/// it, which tells a run that ended first (`WithRun`) as stopped, failed in its host's words or
/// exited (CI run 43303186680: `WithRun` where `Exited(3)` was expected, its cause unsaid),
/// and the home shows its run hosts' diagnostics, their VMs' among them, as it goes ([`boot`]:
/// HVF at 2b89e8504, `WithRun` and `Exited(125)`, nothing more said).
fn assert_ended(guest: &dyn ImageGuest, expected: MemberEnd) {
    let ended = guest.wait_member(&codex(), TIMEOUT).unwrap();
    assert_eq!(
        ended,
        Some(expected),
        "the run's end: {:?}",
        guest.wait_for(Duration::ZERO)
    );
}

/// Reads `channel` of the harness to its end, within the test's bound.
fn read_all(guest: &dyn ImageGuest, channel: Channel) -> Vec<u8> {
    read_within(guest, channel).expect("the member's channel did not end")
}

/// What `channel` of the harness says to its end; none where it does not end within the
/// test's bound.
fn read_within(guest: &dyn ImageGuest, channel: Channel) -> Option<Vec<u8>> {
    let mut pipe = guest.take_output(&codex(), channel).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        let _ = tx.send(bytes);
    });
    rx.recv_timeout(TIMEOUT).ok()
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
        args: Vec::new(),
        mcp: false,
    }];
    let guest = boot(&home, &s);
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
    assert_ended(&*guest, MemberEnd::Exited(3));
    // Nothing of the secret lies in the host's home: it went on the channel alone.
    let found = holding(&home, b"t0p-secret");
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
        args: Vec::new(),
        mcp: false,
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
    let guest = boot(&home, &s);
    let said = String::from_utf8(read_all(&*guest, Channel::Stdout)).unwrap();
    assert_eq!(said, "== /work/hello.txt\nfrom the host\n");
    assert_ended(&*guest, MemberEnd::Exited(0));
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

/// The CPU rate an orchestrator holds its run's vCPUs to (D247) reaches its VM through its
/// host: a run spinning its one vCPU, held to a quarter of its CPU, runs its guest a quarter
/// of the time passing, but what a period may overrun and a period's share where the window
/// and the periods do not line up, and is let go each period; held to its vCPU's whole CPU,
/// nothing holds it, and the answer says so; told none, its hold is lifted. The period it is
/// held over is the one its host said as the run started, its build's hold's.
#[test]
fn the_orchestrators_rate_holds_its_run_to_it() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-rate-home");
    build(&home, "api-rate:1", &["spin"], "");
    let guest = ImageShards::new()
        .boot(&spec(&home, "api-rate:1"), TIMEOUT)
        .unwrap();
    let period = guest.rate_period();
    assert_eq!(period, shards_vmm::vm::hold_period());
    let quarter = NonZeroU64::new(250).unwrap();
    assert_eq!(guest.rate(Some(quarter), TIMEOUT).unwrap(), Some(quarter));
    let ran = || {
        let stats = guest.stats(StatsScope::Run, TIMEOUT).unwrap();
        stats.microvm.unwrap().guest.unwrap().total_ns()
    };
    let (start, before) = (Instant::now(), ran());
    std::thread::sleep(Duration::from_secs(2));
    let (used, wall) = (ran() - before, u64::try_from(start.elapsed().as_nanos()).unwrap());
    // A quarter of each period the share, and a period's whole share and what one vCPU
    // overruns before its governor reads at the window's two ends: two periods.
    let most = wall / 4 + 2 * u64::try_from(period.as_nanos()).unwrap();
    assert!(
        used <= most,
        "{used} ns of guest in {wall} ns, held to a quarter CPU"
    );
    assert!(
        used >= wall / 20,
        "{used} ns of guest in {wall} ns: held, and never let go"
    );
    assert_eq!(
        guest.rate(NonZeroU64::new(1000), TIMEOUT).unwrap(),
        None,
        "its one vCPU's whole CPU holds nothing"
    );
    assert_eq!(guest.rate(None, TIMEOUT).unwrap(), None);
    guest.stop().unwrap();
    assert_eq!(guest.wait_for(TIMEOUT).unwrap(), Some(RunEnd::Stopped));
}

/// A run that keeps nothing of what the image grants its harness past the microVM, as Hex's
/// image class asks by default (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 12), boots
/// and its harness runs to its end, its network process letting nothing past (D231).
#[test]
fn a_run_keeping_nothing_the_image_grants_boots_and_its_harness_runs() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-nothing-home");
    build(
        &home,
        "api-nothing:1",
        &["exit", "0"],
        "NETWORK --egress=api.example:443 provider FOR codex\n\
         EXPOSE --harnesses=codex 443 AS egress FOR provider\nCONNECT codex WITH codex ON provider\n",
    );
    let mut s = spec(&home, "api-nothing:1");
    s.egress = Narrowing {
        egress: Keep::Only(Vec::new()),
        mcp: Keep::Only(Vec::new()),
        destinations: Keep::Only(Vec::new()),
        dns: false,
        members: None,
    };
    let guest = boot(&home, &s);
    assert_ended(&*guest, MemberEnd::Exited(0));
    guest.stop().unwrap();
}

/// The example harness image (docs/examples/harness-codex), built in `home` as `tag`: its
/// Agentfile and harness.json as the repository holds them, FROM its pinned alpine, which
/// the tests' own registry serves (`common::hub_bases`), and the test guest as its `codex`,
/// as OpenAI's binaries are no part of the repository (its .gitignore).
fn build_example(home: &TempDir, tag: &str) {
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/harness-codex");
    let ctx = TempDir::new("api-example-ctx");
    std::fs::create_dir_all(ctx.join("codex/bin")).unwrap();
    let agentfile = std::fs::read_to_string(example.join("Agentfile")).unwrap();
    std::fs::write(ctx.join("Agentfile"), common::hub_bases(&agentfile)).unwrap();
    std::fs::copy(example.join("codex/harness.json"), ctx.join("codex/harness.json")).unwrap();
    std::fs::copy(common::test_guest(), ctx.join("codex/bin/codex")).unwrap();
    // A context's files keep their modes on a Unix host, as fsutil walks them; a Windows
    // host's context gives every file 0755.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            ctx.join("codex/bin/codex"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let built = run_shards_env(&["build", "image"], &["-t", tag, ctx.to_str().unwrap()], &env);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
}

/// Hex's HEX-IMG-3 (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 12): an orchestrator's
/// run of the example harness image with a command of its own, keeping nothing the image
/// grants past the microVM (D250) and holding its members apart with no reach (D246), ends
/// with its command's status, as `shards run` of the same command does; a command that never
/// started ends the run failed in its guest's words (D249), which the assertion says.
#[test]
fn an_images_own_command_ends_its_run_with_its_status() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-own-home");
    build_example(&home, "api-own:1");
    let mut s = spec(&home, "api-own:1");
    s.command = Some(["/bin/sh", "-c", "exit 3"].map(String::from).to_vec());
    s.egress = Narrowing {
        egress: Keep::Only(Vec::new()),
        mcp: Keep::Only(Vec::new()),
        destinations: Keep::Only(Vec::new()),
        dns: false,
        members: Some(Vec::new()),
    };
    let guest = ImageShards::new().boot(&s, TIMEOUT).unwrap();
    assert_eq!(guest.wait_for(TIMEOUT).unwrap(), Some(RunEnd::Exited(3)));
}

/// D253: an orchestrator's run restores its image's template, as a daemon's run does. In a
/// home whose guest is recorded (D28), the image's first run boots, saving its template, and
/// says so; the next restores it, and its command ends as the first's did. A template that no
/// longer restores (its snapshot's state emptied) is replaced, and its run told so; a run of a
/// guest its caller gave has none. Where the host's hypervisor restores no snapshot (Windows
/// Server 2022, PM M345), each run boots.
#[test]
fn an_images_next_run_restores_the_template_its_first_saved() {
    if cannot_run_vms() {
        return;
    }
    let home = common::home_with("api-tmpl-home");
    build_example(&home, "api-tmpl:1");
    let mut given = spec(&home, "api-tmpl:1");
    given.command = Some(["/bin/sh", "-c", "exit 3"].map(String::from).to_vec());
    given.egress = Narrowing {
        egress: Keep::Only(Vec::new()),
        mcp: Keep::Only(Vec::new()),
        destinations: Keep::Only(Vec::new()),
        dns: false,
        members: Some(Vec::new()),
    };
    let mut stored = given.clone();
    stored
        .env
        .retain(|(name, _)| name != "SHARDS_KERNEL" && name != "SHARDS_INIT");
    let run = |s: &ImageSpec| {
        let guest = ImageShards::new().boot(s, TIMEOUT).unwrap();
        let from = guest.started_from();
        assert_eq!(guest.wait_for(TIMEOUT).unwrap(), Some(RunEnd::Exited(3)));
        from
    };
    if !shards_vmm::vm::restores_here() {
        assert_eq!(run(&stored), StartedFrom::Booted);
        return;
    }
    assert_eq!(run(&stored), StartedFrom::Saved);
    assert_eq!(run(&stored), StartedFrom::Restored);
    let templates: Vec<_> = std::fs::read_dir(home.join("templates"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(templates.len(), 1, "{templates:?}");
    // Its snapshot's state emptied: a template that is there and no longer restores. Its
    // generation found as the store finds it (`common::snapshot_file`): on Windows its pointer
    // is the newest `current.~N`, never `current` itself (platform `write_in`, PM M345), and
    // reading that name failed NotFound on WHP at 6c3cb7137.
    std::fs::write(common::snapshot_file(&templates[0], "state"), b"").unwrap();
    assert_eq!(run(&stored), StartedFrom::Replaced);
    assert_eq!(run(&stored), StartedFrom::Restored);
    assert_eq!(run(&given), StartedFrom::Booted);
}

/// D253's pools: an orchestrator's pools give an image's next run a warm run host, its
/// microVM restored and ready before the run asked. The pool's first run starts a host of its
/// own, nothing being warm; the pool then starts one ahead, which a later run takes (`Warm`),
/// its command ending as the first's did. A run of another microVM's (another size) starts a
/// host of its own.
#[test]
fn an_images_next_run_takes_a_warm_host_of_its_pool() {
    if cannot_run_vms() {
        return;
    }
    let home = common::home_with("api-pool-home");
    build_example(&home, "api-pool:1");
    let mut s = spec(&home, "api-pool:1");
    s.command = Some(["/bin/sh", "-c", "exit 3"].map(String::from).to_vec());
    s.egress = Narrowing {
        egress: Keep::Only(Vec::new()),
        mcp: Keep::Only(Vec::new()),
        destinations: Keep::Only(Vec::new()),
        dns: false,
        members: Some(Vec::new()),
    };
    s.env
        .retain(|(name, _)| name != "SHARDS_KERNEL" && name != "SHARDS_INIT");
    let pools = ImagePools::new(2, ImagePools::KEEP).unwrap();
    let run = |s: &ImageSpec| {
        let guest = pools.boot(s, TIMEOUT).unwrap();
        let from = guest.started_from();
        assert_eq!(guest.wait_for(TIMEOUT).unwrap(), Some(RunEnd::Exited(3)));
        from
    };
    assert_ne!(
        run(&s),
        StartedFrom::Warm,
        "nothing is warm before a pool's first run"
    );
    // The warm host its first run's pool started is taken by the first run that comes once
    // it is ready: each run before that starts a host of its own.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if run(&s) == StartedFrom::Warm {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no run took a warm host within {TIMEOUT:?}"
        );
    }
    let other = ImageSpec {
        vcpus: 2,
        ..s.clone()
    };
    assert_ne!(run(&other), StartedFrom::Warm, "another microVM's first run");
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

/// A run's VM process held as it starts, until its run host is ended (D231's stop of a
/// boot under way). On Unix its VM process is a wrapper (`SHARDS_VM_BINARY`) that tells its
/// pid on a FIFO as it starts, then waits for a writer of a second that never comes, before
/// it would become this build's. Windows has no exec, and a VM process opens nothing but
/// what its run host opens for it (D30): there the run's init is a copy held under an
/// oplock (`common::Oplocked`), which the run host's open of it for its VM breaks, and then
/// waits on.
struct Gate {
    dir: TempDir,
    /// What the test reads: read and written on Unix, so that a writer's open never waits.
    #[cfg(unix)]
    told: std::fs::File,
    #[cfg(windows)]
    held: Option<common::Oplocked>,
    /// Set as the boot returns ([`Returning`]).
    #[cfg(windows)]
    returned: std::os::windows::io::OwnedHandle,
}

/// What the boot's thread says as it returns, which ends a wait for the gate
/// ([`Gate::reached`]): a line on the gate's FIFO on Unix, its event set on Windows.
struct Returning {
    #[cfg(unix)]
    told: std::fs::File,
    #[cfg(windows)]
    returned: std::os::windows::io::OwnedHandle,
}

impl Returning {
    fn say(self) {
        #[cfg(unix)]
        {
            use std::io::Write as _;
            let _ = (&self.told).write_all(b"returned\n");
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            // SAFETY: an event this holds.
            unsafe { windows_sys::Win32::System::Threading::SetEvent(self.returned.as_raw_handle()) };
        }
    }
}

impl Gate {
    #[cfg(unix)]
    fn new() -> Gate {
        let dir = TempDir::new("api-gate");
        for name in ["seen", "shut"] {
            let c = std::ffi::CString::new(dir.join(name).into_os_string().into_encoded_bytes()).unwrap();
            // SAFETY: mkfifo(3) of a NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        }
        // Linux and macOS open a FIFO read and written at once, as its reader and a writer.
        let told = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join("seen"))
            .unwrap();
        // Exec'd by the run host as its VM process, in its process group.
        common::program(
            &dir.join("shards-vm"),
            &format!(
                "#!/bin/sh\necho \"$$\" >'{seen}'\nexec 3<'{shut}'\nexec '{vm}' \"$@\"\n",
                seen = dir.join("seen").display(),
                shut = dir.join("shut").display(),
                vm = common::shards_vm().display()
            ),
        );
        Gate { dir, told }
    }

    #[cfg(windows)]
    fn new() -> Gate {
        use std::os::windows::io::FromRawHandle as _;
        use windows_sys::Win32::System::Threading::CreateEventW;
        let dir = TempDir::new("api-gate");
        std::fs::copy(guest_init(), dir.join("init")).unwrap();
        let held = common::Oplocked::new(&dir.join("init"), TIMEOUT);
        // SAFETY: an unnamed manual-reset event, owned from here.
        let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        assert!(!raw.is_null(), "an event: {}", std::io::Error::last_os_error());
        Gate {
            dir,
            held: Some(held),
            // SAFETY: the event just made, which nothing else owns.
            returned: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw) },
        }
    }

    /// `s` booted through the gate.
    fn hold(&self, s: &mut ImageSpec) {
        #[cfg(unix)]
        s.env
            .push(("SHARDS_VM_BINARY".into(), self.dir.join("shards-vm").into()));
        #[cfg(windows)]
        for (name, value) in &mut s.env {
            if name.as_os_str() == "SHARDS_INIT" {
                *value = self.dir.join("init").into();
            }
        }
    }

    /// What its boot's thread says as it returns.
    fn returning(&self) -> Returning {
        Returning {
            #[cfg(unix)]
            told: self.told.try_clone().unwrap(),
            #[cfg(windows)]
            returned: self.returned.try_clone().unwrap(),
        }
    }

    /// Whether the run's VM process reached the gate, waited for until it has or its boot
    /// returned first ([`Returning`]).
    fn reached(&self) -> bool {
        #[cfg(unix)]
        {
            use std::io::BufRead as _;
            let mut line = String::new();
            std::io::BufReader::new(&self.told).read_line(&mut line).unwrap();
            line.trim().parse::<u32>().is_ok()
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
            use windows_sys::Win32::System::Threading::{INFINITE, WaitForMultipleObjects};
            let breaking = self.held.as_ref().unwrap().breaking().as_raw_handle();
            let either = [breaking, self.returned.as_raw_handle()];
            // SAFETY: two events this holds, alive through the wait.
            let woke = unsafe { WaitForMultipleObjects(2, either.as_ptr(), 0, INFINITE) };
            woke == WAIT_OBJECT_0
        }
    }

    /// Lets go of what the run host waits on, once it has been ended: on Windows its open of
    /// the init waits in the kernel on the oplock, which may hold its end up. On Unix a
    /// signal ends a wait for a FIFO's writer.
    fn let_go(&mut self) {
        #[cfg(windows)]
        drop(self.held.take());
    }
}

/// Of `pids`, those still running: on Unix not yet exited (a zombie, which its parent has
/// yet to reap, has), and any process of process group `group` likewise; on Windows not
/// yet ended (`common::alive`).
fn still_running(pids: &[u32], group: Option<u32>) -> Vec<u32> {
    #[cfg(unix)]
    {
        let listed = std::process::Command::new("ps")
            .args(["-A", "-o", "pid=,pgid=,stat="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&listed.stdout)
            .lines()
            .filter_map(|line| {
                let mut w = line.split_whitespace();
                let (pid, pgid, stat): (u32, u32, &str) =
                    (w.next()?.parse().ok()?, w.next()?.parse().ok()?, w.next()?);
                (!stat.starts_with('Z') && (pids.contains(&pid) || Some(pgid) == group)).then_some(pid)
            })
            .collect()
    }
    #[cfg(windows)]
    {
        let _ = group;
        pids.iter()
            .copied()
            .filter(|&pid| common::alive(pid.cast_signed()))
            .collect()
    }
}

/// D231: a stop of a boot under way, once its run host has started its VM process and
/// before its run starts, ends the boot `Ended(Stopped)`, its run host and every process of
/// its run with it. Do boot an image whose VM process is held as it starts ([`Gate`]), and
/// stop the boot from this thread as it reaches the gate. Expect: `Ended(Stopped)`; none of
/// the processes the run host had started by then running, nor, on Unix, any of its
/// process group, a VM, network or share process it started after among them; each
/// record of a run the home keeps (a writable layer's file in `layers`, named by the pid of
/// the run host that made it) naming no process that runs; and no file of its run host's
/// left in `layers` at all, its run host having ended before its microVM held its layer.
#[test]
fn a_stop_mid_boot_ends_the_run_host_and_every_process_of_its_run() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-bstop-home");
    build_example(&home, "api-bstop:1");
    let mut gate = Gate::new();
    let mut s = spec(&home, "api-bstop:1");
    gate.hold(&mut s);
    let stop = BootStop::new();
    let layers = home.join("layers");
    // The pids its records name: a build's that ended among them.
    let records = || -> Vec<u32> {
        std::fs::read_dir(&layers)
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let name = e.ok()?.file_name();
                name.to_str()?.split('-').next()?.parse().ok()
            })
            .collect()
    };
    let returning = gate.returning();
    // Whatever is seen, the boot is stopped before it is held to anything: a failing test
    // leaves no run host waiting out its patience on the gate.
    let (reached, hosts, run, booted) = std::thread::scope(|scope| {
        let booting = scope.spawn(|| {
            let booted = ImageShards::new().boot_stoppable(&s, TIMEOUT, &stop).map(drop);
            returning.say();
            booted
        });
        let reached = gate.reached();
        // Its run host, by the record it made before it started its VM process; and what
        // runs under it now, as far down as its descendants go.
        let hosts = still_running(&records(), None);
        let mut run = hosts.clone();
        let listed = common::processes();
        let mut i = 0;
        while let Some(&parent) = run.get(i) {
            run.extend(
                listed
                    .iter()
                    .filter(|(pid, ppid, _)| *ppid == parent && !run.contains(pid))
                    .map(|(pid, ..)| *pid)
                    .collect::<Vec<_>>(),
            );
            i += 1;
        }
        stop.stop();
        gate.let_go();
        (reached, hosts, run, booting.join().unwrap())
    });
    assert!(
        reached,
        "the boot returned before its VM process started: {booted:?}"
    );
    let [host] = hosts[..] else {
        panic!("the home's layers name {hosts:?} running");
    };
    assert!(run.len() > 1, "its run host {host} ran no VM process: {run:?}");
    assert_eq!(booted, Err(ImageRefusal::Ended(RunEnd::Stopped)));
    let left = still_running(&run, Some(host));
    assert!(left.is_empty(), "left running of its run {run:?}: {left:?}");
    let named = records();
    let running = still_running(&named, None);
    assert!(running.is_empty(), "the home's layers name {running:?}, running");
    assert!(
        !named.contains(&host),
        "its run host {host}'s layer outlived it: the home's layers name {named:?}"
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
        args: Vec::new(),
        mcp: false,
    }];
    let guest = boot(&home, &s);
    let mut input = Vec::new();
    input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n");
    input.extend_from_slice(b"\0\xff\x80\x1b[2Jno-newline\xc3\x28");
    input.extend((0..300 * 1024u32).map(|i| (i % 251) as u8));
    let mut writer = guest.take_input(&codex()).unwrap();
    assert!(guest.take_input(&codex()).is_err(), "taken once");
    let echoed = std::thread::scope(|scope| {
        let reading = scope.spawn(|| read_all(guest.as_ref(), Channel::Stdout));
        let written = writer.write_all(&input);
        drop(writer);
        // Its stdin taking no more: how far the harness read it (its stderr says so as its
        // stdin ends), and how it and the run ended.
        if let Err(e) = written {
            let said = read_within(guest.as_ref(), Channel::Stderr);
            panic!(
                "its stdin took no more ({e}): the harness said {:?} and ended {:?}; the run's end {:?}",
                said.as_deref().map(String::from_utf8_lossy),
                guest.wait_member(&codex(), TIMEOUT),
                guest.wait_for(Duration::ZERO)
            );
        }
        reading.join().unwrap()
    });
    assert_eq!(echoed.len(), input.len());
    assert!(
        echoed == input,
        "the harness echoed other bytes than its stdin took"
    );
    let said = read_all(guest.as_ref(), Channel::Stderr);
    assert_eq!(said, format!("codex\0echo-end {}", input.len()).into_bytes());
    assert_ended(&*guest, MemberEnd::Exited(0));
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

/// D256: a member's run-time arguments follow its image's command on its command line, each
/// whole and in its order. The harness, the test guest writing its command line word by word,
/// each with its NUL, says its program as found in its own `bin`, its image's options, then its
/// run's `--resume`, `s 1` and an empty one: none split, joined or dropped. An argument holding
/// a NUL, which no command line exec'd holds, is refused by its place, never its bytes; and
/// arguments past the channel's bound are refused whole, never cut; each before any run host
/// starts.
#[test]
fn a_members_arguments_follow_its_images_command_in_order() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-args-home");
    build(&home, "api-args:1", &["argv", "--from-image"], "");
    let given = |args: Vec<String>| {
        let mut s = spec(&home, "api-args:1");
        s.members = vec![MemberSpec {
            member: codex(),
            env: Vec::new(),
            files: Vec::new(),
            workdir: None,
            stdin: false,
            terminal: false,
            args,
            mcp: false,
        }];
        s
    };
    // Its run host's diagnostics kept ([`boot`]): its guest stopped before its command's end
    // with nothing said of why (CI 43496145972, x86_64 gnu).
    let guest = boot(&home, &given(["--resume", "s 1", ""].map(String::from).to_vec()));
    let said = read_all(&*guest, Channel::Stdout);
    let (program, rest) = said.split_at(said.iter().position(|&b| b == 0).unwrap_or(said.len()));
    assert!(
        program.starts_with(b"/") && program.ends_with(b"/bin/codex"),
        "its command line: {:?}",
        String::from_utf8_lossy(&said)
    );
    assert_eq!(
        rest,
        b"\0argv\0--from-image\0--resume\0s 1\0\0",
        "its command line: {:?}",
        String::from_utf8_lossy(&said)
    );
    assert_ended(&*guest, MemberEnd::Exited(0));
    guest.stop().unwrap();
    assert_eq!(guest.wait_for(TIMEOUT).unwrap(), Some(RunEnd::Stopped));
    let refused = |s: ImageSpec| match ImageShards::new().boot(&s, TIMEOUT) {
        Err(e) => e,
        Ok(_) => panic!("booted"),
    };
    match refused(given(["--resume", "s-1\0--model"].map(String::from).to_vec())) {
        ImageRefusal::Spec { what, why } => {
            assert_eq!(what, "a member's argument");
            assert!(why.contains("argument 1") && !why.contains("s-1"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    let half = shards_abi::run::MAX_PAYLOAD as usize / 2;
    assert!(matches!(
        refused(given(vec!["x".repeat(half), "x".repeat(half)])),
        ImageRefusal::Spec { what: "the spec", .. }
    ));
}

/// D238's member lanes through an orchestrator's run (plan step 11; the Hex lead's ask 3, which
/// makes kiln's interrupt and stop real signals): shards-api boots an image of two harnesses
/// under a control ask of the orchestrator's key, `codex` with an MCP bridge (D254) its harness
/// writes MCP's `initialize` into, `claude` asleep; its controller dials the run's endpoint
/// subscribed to exec lanes.
/// - **The MCP connection.** `Execs::mcp(codex)` carries the harness's `initialize`, exactly.
/// - **The signals.** TERM to `codex`'s command ends it `Signaled(15)`, never `Exited(143)`;
///   KILL to `claude`'s, `Signaled(9)`: on its lane (`exec::End`) and on the image channel
///   (`MemberEnd`) alike.
/// - **After.** A steer of an ended member is refused `Ended`; its lane asked anew says how it
///   ended.
///
/// Each wait on the guest is judged (`common::within_own`, D209); none is fixed.
#[test]
fn an_orchestrators_members_are_signaled_and_their_mcp_connections_reached_on_its_endpoint() {
    use shards_api::control::{
        Controller, Fault, Got, Identity, McpConnection, Member, MemberControl, Subscriptions, Unsteered,
        exec, exec_admissions,
    };
    use shards_api::image::ControlAsk;
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-lanes-home");
    build(&home, "api-lanes:1", &[], "HARNESS claude FROM ./harness.tar\n");
    let hex = Identity::generate().unwrap();
    let claude = MemberName::harness("claude");
    let member = |member: MemberName, mode: &str, mcp: bool| MemberSpec {
        member,
        env: Vec::new(),
        files: Vec::new(),
        workdir: None,
        stdin: false,
        terminal: false,
        args: vec![mode.into()],
        mcp,
    };
    let mut s = spec(&home, "api-lanes:1");
    s.members = vec![
        member(codex(), "mcp-hello", true),
        member(claude.clone(), "sleep", false),
    ];
    s.control = Some(ControlAsk {
        controller: hex.fingerprint(),
        sockets: None,
    });
    let guest = boot(&home, &s);
    let index = |name: &MemberName| {
        guest
            .members()
            .iter()
            .find(|m| m.kind == name.kind && m.name == name.name)
            .map(|m| m.index)
            .unwrap()
    };
    let (codex_at, claude_at) = (index(&codex()), index(&claude));
    let at = *guest.control().expect("the run's agent-control endpoint");
    let (controller, dialed) = Controller::dial(
        &hex,
        at.vm,
        at.address,
        Subscriptions {
            outputs: &[],
            inputs: &[],
            execs: Some(exec_admissions()),
            sockets: None,
        },
    )
    .unwrap();
    let driver = std::thread::spawn(move || controller.run());
    let execs = dialed.execs.expect("exec lanes subscribed");
    // The harness's connection to its bridge, held for the controller until it asks.
    let said = common::within_own("codex's MCP connection", &home, || {
        let McpConnection { mut from, .. } = execs.mcp(codex_at).unwrap();
        let mut said = Vec::new();
        while said.len() < shards_testguest::MCP_INITIALIZE.len() {
            match from.next_record().unwrap() {
                Got::Record { bytes, .. } => said.extend(bytes),
                Got::Finished => break,
            }
        }
        said
    });
    assert_eq!(
        String::from_utf8_lossy(&said),
        String::from_utf8_lossy(shards_testguest::MCP_INITIALIZE)
    );
    // `claude` asleep, as its own word says, before it is signalled.
    let mut asleep = guest.take_output(&claude, Channel::Stdout).unwrap();
    let mut line = [0; 6];
    common::within_own("claude's ready", &home, || asleep.read_exact(&mut line)).unwrap();
    assert_eq!(&line, b"ready\n");
    let signalled = |at: u32, signal: u32| {
        common::within_own("a member's signal and its end", &home, || {
            let Member::Running(MemberControl {
                mut steering,
                mut end,
            }) = execs.member(at).unwrap()
            else {
                panic!("member {at} runs");
            };
            let sent = steering.signal(signal);
            let ended = end.wait();
            let after = steering.signal(signal);
            (sent, ended, after)
        })
    };
    let (sent, ended, after) = signalled(codex_at, 15);
    assert_eq!(sent, Ok(()));
    assert_eq!(ended, Ok(exec::End::Signaled(15)), "TERM, never an exit of 143");
    assert_eq!(after, Err(Fault::Unsteered(Unsteered::Ended)));
    let (sent, ended, _) = signalled(claude_at, 9);
    assert_eq!(sent, Ok(()));
    assert_eq!(ended, Ok(exec::End::Signaled(9)));
    assert_eq!(
        guest.wait_member(&codex(), TIMEOUT).unwrap(),
        Some(MemberEnd::Signaled(15))
    );
    assert_eq!(
        guest.wait_member(&claude, TIMEOUT).unwrap(),
        Some(MemberEnd::Signaled(9))
    );
    let again = common::within_own("an ended member's lane", &home, || execs.member(codex_at));
    assert!(
        matches!(again, Ok(Member::Ended(exec::End::Signaled(15)))),
        "{again:?}"
    );
    guest.stop().unwrap();
    assert_eq!(guest.wait_for(TIMEOUT).unwrap(), Some(RunEnd::Stopped));
    let _ = driver.join().unwrap();
}

/// A value the guest's memory holds, unique to this test's run: a SHA-256 of `label`, this
/// process and the time, in hex.
fn canary(label: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seed = format!("{label} {} {nanos}", std::process::id());
    <sha2::Sha256 as sha2::Digest>::digest(seed.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Plan step 33's leak scan (docs/research/measurements/s33-leak-scan), on what it scans:
/// what `ImageGuest::save` writes, which is its owner's alone (D259).
/// Python 3 runs it: `python3`, on Windows `python`.
fn leak_scan(secrets: &std::path::Path, paths: &[&std::path::Path]) -> (Option<i32>, String) {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/research/measurements/s33-leak-scan/scan.py");
    let python = if cfg!(windows) { "python" } else { "python3" };
    let out = std::process::Command::new(python)
        .arg("-I")
        .arg(script)
        .arg("--secrets")
        .arg(secrets)
        .args(paths)
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code(), said)
}

/// D259, and plan step 33's leak scan checked on a real microVM: a run's memory saved by its
/// owner holds what the run's harness was given, and the scan finds it there, while a value
/// never given is found nowhere. Do: give the harness (the test guest echoing its stdin, so that
/// it runs on) a value in its environment and one in a staged file; ask a save outside the run's
/// home, then one in it; echo a line after; scan the save, and the harness's environment as
/// given, first for the values given, then for one never given. Expect: the first save refused
/// `Destination` and nothing made; the second a snapshot directory, its generation's memory
/// written; the harness still echoing; both values found in the save's memory, each by its name,
/// and the environment's in its file (exit 3); the value never given found nowhere.
#[test]
fn a_saved_runs_memory_holds_what_its_harness_was_given_and_the_scan_finds_it() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-save-home");
    build(&home, "api-save:1", &["managed-capture", "codex", "echo"], "");
    let (in_env, in_file, never) = (canary("env"), canary("file"), canary("never"));
    let mut s = spec(&home, "api-save:1");
    s.members = vec![MemberSpec {
        member: codex(),
        env: vec![("SHARDS_LEAK_CANARY".into(), Secret::from(in_env.as_str()))],
        files: vec![MemberFile {
            path: ".leak-canary".into(),
            mode: 0o600,
            bytes: Secret(in_file.clone().into_bytes()),
        }],
        workdir: None,
        stdin: true,
        terminal: false,
        args: Vec::new(),
        mcp: false,
    }];
    let guest = boot(&home, &s);
    let mut input = guest.take_input(&codex()).unwrap();
    let mut output = guest.take_output(&codex(), Channel::Stdout).unwrap();
    let mut echo = |line: &[u8]| {
        input.write_all(line).unwrap();
        let mut echoed = vec![0; line.len()];
        output.read_exact(&mut echoed).unwrap();
        assert_eq!(echoed, line);
    };
    echo(b"before\n");
    let outside = TempDir::new("api-save-out");
    match guest.save(&outside.join("s1"), TIMEOUT) {
        Err(ImageRefusal::Save(SaveRefusal::Destination, why)) => {
            assert!(why.contains("outside the run's home"), "{why}")
        }
        other => panic!("{other:?}"),
    }
    assert!(!outside.join("s1").exists());
    let saved = home.join("leak-save");
    guest.save(&saved, TIMEOUT).unwrap();
    // Its memory as the store reads it, through the generation its pointer names: on Windows
    // that pointer is the newest version of `current` (platform `write_in`, PM M345), never a
    // file of that name (WHP at 5af644eab).
    let memory = common::snapshot_file(&saved, "memory");
    assert!(std::fs::metadata(&memory).unwrap().len() > 0);
    echo(b"after\n");
    let keys = TempDir::new("api-save-keys");
    let given = keys.join("given.json");
    std::fs::write(
        &given,
        format!(r#"{{"env_token":"{in_env}","file_token":"{in_file}"}}"#),
    )
    .unwrap();
    let not_given = keys.join("never.json");
    std::fs::write(&not_given, format!(r#"{{"never_token":"{never}"}}"#)).unwrap();
    let env = keys.join("env");
    std::fs::create_dir(&env).unwrap();
    std::fs::write(
        env.join("harness-codex"),
        format!("SHARDS_LEAK_CANARY={in_env}\0"),
    )
    .unwrap();
    let (code, said) = leak_scan(&given, &[&saved, &env]);
    assert_eq!(code, Some(3), "{said}");
    let memory = memory.display().to_string();
    for name in ["given.json:env_token", "given.json:file_token"] {
        assert!(
            said.lines()
                .any(|l| l.starts_with(&format!("FOUND {memory} offset ")) && l.ends_with(name)),
            "{name} in the save's memory: {said}"
        );
    }
    assert!(
        said.contains("harness-codex offset 19 length 64: a run of given.json:env_token"),
        "{said}"
    );
    // Never given, never found: what else credential-shaped its memory holds is listed (exit 4)
    // and is no secret's.
    let (code, said) = leak_scan(&not_given, &[&saved, &env]);
    assert!(matches!(code, Some(0 | 4)), "{said}");
    assert!(said.contains(" 0 secret runs found"), "{said}");
    guest.stop().unwrap();
}

// Plan step 33's steps 7 and 8 on a real microVM (D242): a credential filled in at the run's
// egress toward a host speaking HTTP/2, a lease its orchestrator refuses, and a sign-in's tokens
// taken outside over HTTP/2. The host is this test's own, on this host's loopback, which a run
// reaches as `host.docker.internal`.

/// A credential's placeholder, as an orchestrator mints one (a b64token, `credential::
/// placeholder`), and its value, which the microVM never holds.
const PLACEHOLDER: &str =
    "shards-test-placeholder.5f0c2d7e9a14b3c86e21f0a9d4c7b2e15a3f8c06d9e2b7a41c5f3e8d0b6a9c27";
const VALUE: &str = "sk-test-the-real-value-filled-at-egress-7c1e";
const REFERENCE: &str = "test/api-key";
/// A sign-in's tokens as its provider (the test's host) gives them, then as its refresh does,
/// and the reference and placeholders its capture gives.
const REAL_ACCESS: &str = "test-access-token-from-the-provider-0001";
const REAL_REFRESH: &str = "test-refresh-token-from-the-provider-0001";
const REAL_ACCESS_2: &str = "test-access-token-refreshed-outside-0002";
const REAL_REFRESH_2: &str = "test-refresh-token-refreshed-outside-0002";
const ACCESS_REFERENCE: &str = "test/codex/access";
const ACCESS_PLACEHOLDER: &str =
    "shards-test-access.0d9e2b7a41c5f3e8d0b6a9c275f0c2d7e9a14b3c86e21f0a9d4c7b2e15a3f8c6";
const REFRESH_PLACEHOLDER: &str =
    "shards-test-refresh.a41c5f3e8d0b6a9c275f0c2d7e9a14b3c86e21f0a9d4c7b2e15a3f8c60d9e2b7";

/// What `value` comes back to the guest as where it echoes (inject.rs' `Mask`): its
/// placeholder's bytes, as many as the value has.
fn masked(value: &str, placeholder: &str) -> String {
    placeholder
        .bytes()
        .cycle()
        .take(value.len())
        .map(char::from)
        .collect()
}

/// The files under `root` holding `needle`.
fn holding(root: &std::path::Path, needle: &[u8]) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => dirs.push(path),
                Ok(t) if t.is_file() => {
                    let bytes = std::fs::read(&path).unwrap_or_default();
                    if bytes.windows(needle.len()).any(|w| w == needle) {
                        found.push(path);
                    }
                }
                _ => {}
            }
        }
    }
    found
}

/// Whether a run's network process trusts this test's roots as its host's. On Linux its
/// verifier reads the system's bundle as it starts (rustls-platform-verifier through
/// rustls-native-certs), which `SSL_CERT_FILE` in the run host's environment names; on macOS
/// (trustd) and Windows (CryptoAPI) the OS verifies (shards-verify) and nothing a test sets
/// adds a root, so these runs are owed there until a run's injector can be given a test's.
fn injector_trusts_test_roots() -> bool {
    if cfg!(target_os = "linux") {
        return true;
    }
    let _ = writeln!(
        std::io::stderr(),
        "SKIP: a run's injector here trusts only the OS's roots, and no test root can be added to them"
    );
    false
}

/// A request the test's host was asked: over HTTP/2 or not, each field as it arrived (with
/// its HPACK indexing, over HTTP/2), and its body.
#[derive(Debug)]
struct Asked {
    h2: bool,
    method: String,
    path: String,
    fields: Vec<(String, Vec<u8>, Option<shards_net::hpack::Indexing>)>,
    body: Vec<u8>,
}

impl Asked {
    fn field(&self, name: &str) -> Option<(&[u8], Option<shards_net::hpack::Indexing>)> {
        self.fields
            .iter()
            .find(|(n, ..)| n == name)
            .map(|(_, v, i)| (v.as_slice(), *i))
    }
}

/// The test's TLS host: its port on this host's loopback, its CA's certificate written for
/// the run's host, and each request it answered, sent as it came, before its answer.
struct TlsHost {
    port: u16,
    roots: std::path::PathBuf,
    asked: std::sync::mpsc::Receiver<Asked>,
}

type ServerTls = rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream>;

/// The test's TLS host, under a CA of its own, for `names`, offering `h2` and `http/1.1`: `/echo`
/// answers the request's credential field in `x-echo` and its body; `/oauth/token` answers a
/// code's exchange with [`REAL_ACCESS`] and [`REAL_REFRESH`], as Codex's provider would, and
/// anything else there 400.
fn tls_host(home: &TempDir, names: &[&str]) -> TlsHost {
    let mut ca = rcgen::CertificateParams::new(Vec::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name = rcgen::DistinguishedName::new();
    ca.distinguished_name
        .push(rcgen::DnType::CommonName, "shards credentials test CA");
    ca.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
    ];
    let ca = rcgen::CertifiedIssuer::self_signed(ca, rcgen::KeyPair::generate().unwrap()).unwrap();
    let mut leaf =
        rcgen::CertificateParams::new(names.iter().map(|n| (*n).to_string()).collect::<Vec<_>>()).unwrap();
    leaf.distinguished_name = rcgen::DistinguishedName::new();
    leaf.distinguished_name.push(rcgen::DnType::CommonName, names[0]);
    leaf.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = leaf.signed_by(&key, &ca).unwrap();
    let roots = home.join("test-roots.pem");
    std::fs::write(&roots, ca.pem()).unwrap();
    let mut config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.der().clone()],
        rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
            key.serialize_der(),
        )),
    )
    .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let config = std::sync::Arc::new(config);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, asked) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let (config, tx) = (config.clone(), tx.clone());
            std::thread::spawn(move || {
                let conn = rustls::ServerConnection::new(config).unwrap();
                let mut tls = rustls::StreamOwned::new(conn, sock);
                let _ = serve(&mut tls, &tx);
            });
        }
    });
    TlsHost { port, roots, asked }
}

fn serve(tls: &mut ServerTls, tx: &std::sync::mpsc::Sender<Asked>) -> std::io::Result<()> {
    while tls.conn.is_handshaking() {
        tls.conn.complete_io(&mut tls.sock)?;
    }
    if tls.conn.alpn_protocol() == Some(b"h2") {
        serve_h2(tls, tx)
    } else {
        serve_http1(tls, tx)
    }
}

/// An answer of the test's host: its status, fields and body.
type Answer = (u16, Vec<(&'static str, Vec<u8>)>, Vec<u8>);

/// The test host's answer to `asked`.
fn answer(asked: &Asked) -> Answer {
    match asked.path.as_str() {
        "/echo" => {
            let credential = asked
                .field("authorization")
                .or_else(|| asked.field("x-api-key"))
                .map(|(v, _)| v.to_vec())
                .unwrap_or_default();
            (200, vec![("x-echo", credential.clone())], credential)
        }
        "/oauth/token" if asked.method == "POST" && asked.body.starts_with(b"grant_type=authorization_code") => (
            200,
            vec![("content-type", b"application/json".to_vec())],
            format!(
                r#"{{"access_token":"{REAL_ACCESS}","refresh_token":"{REAL_REFRESH}","expires_in":3600,"token_type":"Bearer"}}"#
            )
            .into_bytes(),
        ),
        "/oauth/token" => (
            400,
            vec![("content-type", b"application/json".to_vec())],
            br#"{"error":"invalid_grant"}"#.to_vec(),
        ),
        _ => (404, Vec::new(), Vec::new()),
    }
}

fn serve_http1(tls: &mut ServerTls, tx: &std::sync::mpsc::Sender<Asked>) -> std::io::Result<()> {
    loop {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if tls.read(&mut byte)? == 0 {
                return Ok(());
            }
            head.push(byte[0]);
        }
        let text = String::from_utf8_lossy(&head).into_owned();
        let mut lines = text.split("\r\n");
        let mut first = lines.next().unwrap_or_default().split(' ');
        let (method, path) = (
            first.next().unwrap_or_default().to_string(),
            first.next().unwrap_or_default().to_string(),
        );
        let fields: Vec<_> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().as_bytes().to_vec(), None))
            .collect();
        let length = fields
            .iter()
            .find(|(n, ..)| n == "content-length")
            .and_then(|(_, v, _)| std::str::from_utf8(v).ok()?.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        tls.read_exact(&mut body)?;
        let asked = Asked {
            h2: false,
            method,
            path,
            fields,
            body,
        };
        let (status, fields, body) = answer(&asked);
        let _ = tx.send(asked);
        let mut out = format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\n", body.len()).into_bytes();
        for (name, value) in fields {
            out.extend_from_slice(format!("{name}: ").as_bytes());
            out.extend_from_slice(&value);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&body);
        tls.write_all(&out)?;
        tls.flush()?;
    }
}

/// HTTP/2's frame types and flags the test's host reads or writes (RFC 9113 §6).
mod h2 {
    pub const DATA: u8 = 0x0;
    pub const HEADERS: u8 = 0x1;
    pub const SETTINGS: u8 = 0x4;
    pub const PING: u8 = 0x6;
    pub const GOAWAY: u8 = 0x7;
    pub const WINDOW_UPDATE: u8 = 0x8;
    pub const CONTINUATION: u8 = 0x9;
    pub const END_STREAM: u8 = 0x1;
    pub const ACK: u8 = 0x1;
    pub const END_HEADERS: u8 = 0x4;
    pub const PADDED: u8 = 0x8;
    pub const PRIORITY: u8 = 0x20;
    pub const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
    /// SETTINGS_HEADER_TABLE_SIZE as the protocol starts it (§6.5.2).
    pub const TABLE: usize = 4096;

    pub fn frame(out: &mut Vec<u8>, kind: u8, flags: u8, stream: u32, payload: &[u8]) {
        out.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes()[1..]);
        out.push(kind);
        out.push(flags);
        out.extend_from_slice(&stream.to_be_bytes());
        out.extend_from_slice(payload);
    }

    /// A frame's payload without its padding, and a HEADERS frame's priority (§6.1, §6.2).
    pub fn unpad(payload: &[u8], flags: u8, headers: bool) -> &[u8] {
        let (mut p, mut pad) = (payload, 0);
        if flags & PADDED != 0 {
            pad = usize::from(p[0]);
            p = &p[1..];
        }
        if headers && flags & PRIORITY != 0 {
            p = &p[5..];
        }
        &p[..p.len() - pad]
    }
}

fn serve_h2(tls: &mut ServerTls, tx: &std::sync::mpsc::Sender<Asked>) -> std::io::Result<()> {
    use shards_net::hpack::{Decoded, Decoder, Encoder, Field, Indexing};
    let mut preface = [0u8; 24];
    tls.read_exact(&mut preface)?;
    if preface != h2::PREFACE {
        return Err(std::io::Error::other("no client's preface"));
    }
    let mut out = Vec::new();
    h2::frame(&mut out, h2::SETTINGS, 0, 0, &[]);
    tls.write_all(&out)?;
    let (mut decoder, mut encoder) = (Decoder::new(h2::TABLE), Encoder::new(h2::TABLE, h2::TABLE));
    /// An open stream's header block so far, its fields once whole, and its body.
    type Open = (Vec<u8>, Vec<Field>, Vec<u8>);
    let mut streams: std::collections::HashMap<u32, Open> = Default::default();
    loop {
        let mut head = [0u8; 9];
        match tls.read_exact(&mut head) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let len = usize::from(head[0]) << 16 | usize::from(head[1]) << 8 | usize::from(head[2]);
        let (kind, flags) = (head[3], head[4]);
        let id = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & 0x7fff_ffff;
        let mut payload = vec![0u8; len];
        tls.read_exact(&mut payload)?;
        let mut out = Vec::new();
        let mut ended = false;
        match kind {
            h2::SETTINGS if flags & h2::ACK == 0 => h2::frame(&mut out, h2::SETTINGS, h2::ACK, 0, &[]),
            h2::PING if flags & h2::ACK == 0 => h2::frame(&mut out, h2::PING, h2::ACK, 0, &payload),
            h2::GOAWAY => return Ok(()),
            h2::HEADERS | h2::CONTINUATION => {
                let s = streams.entry(id).or_default();
                s.0.extend_from_slice(h2::unpad(&payload, flags, kind == h2::HEADERS));
                if flags & h2::END_HEADERS != 0 {
                    let Ok(Decoded::Fields(fields)) = decoder.decode(&s.0, usize::MAX) else {
                        return Err(std::io::Error::other(
                            "a header block the test's host cannot decode",
                        ));
                    };
                    s.1.extend(fields);
                    s.0.clear();
                }
                ended = flags & h2::END_STREAM != 0;
            }
            h2::DATA => {
                streams
                    .entry(id)
                    .or_default()
                    .2
                    .extend_from_slice(h2::unpad(&payload, flags, false));
                if !payload.is_empty() {
                    let credit = u32::try_from(payload.len()).unwrap().to_be_bytes();
                    h2::frame(&mut out, h2::WINDOW_UPDATE, 0, 0, &credit);
                    h2::frame(&mut out, h2::WINDOW_UPDATE, 0, id, &credit);
                }
                ended = flags & h2::END_STREAM != 0;
            }
            _ => {}
        }
        if ended {
            let (_, fields, body) = streams.remove(&id).unwrap_or_default();
            let pseudo = |name: &[u8]| {
                fields
                    .iter()
                    .find(|f| f.name == name)
                    .map(|f| String::from_utf8_lossy(&f.value).into_owned())
                    .unwrap_or_default()
            };
            let asked = Asked {
                h2: true,
                method: pseudo(b":method"),
                path: pseudo(b":path"),
                fields: fields
                    .iter()
                    .filter(|f| !f.name.starts_with(b":"))
                    .map(|f| {
                        (
                            String::from_utf8_lossy(&f.name).into_owned(),
                            f.value.to_vec(),
                            Some(f.indexing),
                        )
                    })
                    .collect(),
                body,
            };
            let (status, fields, body) = answer(&asked);
            let _ = tx.send(asked);
            let mut answer = vec![Field::new(
                b":status",
                status.to_string().as_bytes(),
                Indexing::Without,
            )];
            answer.extend(
                fields
                    .iter()
                    .map(|(n, v)| Field::new(n.as_bytes(), v, Indexing::Without)),
            );
            answer.push(Field::new(
                b"content-length",
                body.len().to_string().as_bytes(),
                Indexing::Without,
            ));
            let mut block = Vec::new();
            encoder.encode(&answer, &mut block);
            let flags = h2::END_HEADERS | if body.is_empty() { h2::END_STREAM } else { 0 };
            h2::frame(&mut out, h2::HEADERS, flags, id, &block);
            if !body.is_empty() {
                h2::frame(&mut out, h2::DATA, h2::END_STREAM, id, &body);
            }
        }
        if !out.is_empty() {
            tls.write_all(&out)?;
            tls.flush()?;
        }
    }
}

/// The orchestrator's lessor as these tests play it: [`VALUE`] leased for [`REFERENCE`] after
/// refusing the first `refuse` asks, and a sign-in's tokens kept as its capture and refresh
/// give them; each ask, release, capture and refresh recorded.
#[derive(Default)]
struct TestLessor {
    refuse: usize,
    asks: std::sync::Mutex<Vec<String>>,
    released: std::sync::Mutex<Vec<String>>,
    /// The sign-in's access token as the lessor holds it now.
    access: std::sync::Mutex<String>,
    captured: std::sync::Mutex<Vec<SignInTokens>>,
    refreshed: std::sync::Mutex<usize>,
}

impl TestLessor {
    fn refusing(refuse: usize) -> std::sync::Arc<TestLessor> {
        std::sync::Arc::new(TestLessor {
            refuse,
            ..TestLessor::default()
        })
    }

    fn given(&self) -> usize {
        self.asks.lock().unwrap().len().saturating_sub(self.refuse)
    }
}

impl Lessor for TestLessor {
    fn lease(&self, reference: &str, _key: Option<[u8; 32]>) -> Result<Leased, LeaseRefusal> {
        let mut asks = self.asks.lock().unwrap();
        asks.push(reference.into());
        let n = asks.len();
        if n <= self.refuse {
            return Err(LeaseRefusal::Denied("the test's broker refuses this ask".into()));
        }
        let value = match reference {
            ACCESS_REFERENCE => self.access.lock().unwrap().clone(),
            _ => VALUE.to_string(),
        };
        Ok(Leased {
            lease: format!("lease-{n}"),
            value: Secret::from(value.as_str()),
            until: Instant::now() + TIMEOUT,
        })
    }

    fn release(&self, lease: &str) {
        self.released.lock().unwrap().push(lease.into());
    }

    fn capture(
        &self,
        _sign_in: &str,
        tokens: SignInTokens,
        _key: Option<[u8; 32]>,
    ) -> Result<Captured, SignInRefusal> {
        *self.access.lock().unwrap() = String::from_utf8(tokens.access.0.clone()).unwrap();
        self.captured.lock().unwrap().push(tokens);
        Ok(Captured {
            access: ACCESS_REFERENCE.into(),
            access_placeholder: ACCESS_PLACEHOLDER.into(),
            refresh_placeholder: Some(REFRESH_PLACEHOLDER.into()),
        })
    }

    fn refresh(&self, _sign_in: &str, _key: Option<[u8; 32]>) -> Result<Secret, SignInRefusal> {
        *self.refreshed.lock().unwrap() += 1;
        *self.access.lock().unwrap() = REAL_ACCESS_2.into();
        Ok(Secret::from(
            format!(
                r#"{{"access_token":"{REAL_ACCESS_2}","refresh_token":"{REAL_REFRESH_2}","expires_in":3600,"token_type":"Bearer"}}"#
            )
            .as_str(),
        ))
    }
}

/// An image whose `codex` (the test guest's `tls-ask`) asks the test's host at `port`, as
/// `host.docker.internal`, for server name `sni`: its harness reaches that destination alone,
/// and `auth.openai.com:443` where `sign_in`, so that Codex's token host is one its egress
/// reaches (D236); the guest's flows to it still go to the test's host, as it dials them.
fn build_asking(home: &TempDir, tag: &str, port: u16, sni: &str, sign_in: bool) {
    let host = format!("host.docker.internal:{port}");
    let (token, ports) = if sign_in {
        (" --egress=auth.openai.com:443", format!("{port} 443"))
    } else {
        ("", port.to_string())
    };
    build(
        home,
        tag,
        &["tls-ask", &host, sni],
        &format!(
            "NETWORK --egress={host}{token} provider FOR codex\n\
             EXPOSE --harnesses=codex {ports} AS egress FOR provider\nCONNECT codex WITH codex ON provider\n"
        ),
    );
}

/// The spec of a run of `image` whose harness asks `steps`, holding [`PLACEHOLDER`] in
/// `API_KEY`, its host trusting the test's roots, its leases `lessor`'s.
fn asking_spec(
    home: &TempDir,
    image: &str,
    host: &TlsHost,
    lessor: &std::sync::Arc<TestLessor>,
    steps: &[&str],
) -> ImageSpec {
    let mut s = spec(home, image);
    s.env
        .push(("SSL_CERT_FILE".into(), host.roots.as_os_str().into()));
    s.lessor = Some(RunLessor(lessor.clone()));
    s.members = vec![MemberSpec {
        member: codex(),
        env: vec![("API_KEY".into(), Secret::from(PLACEHOLDER))],
        files: Vec::new(),
        workdir: None,
        stdin: false,
        terminal: false,
        args: steps.iter().map(|s| (*s).to_string()).collect(),
        mcp: false,
    }];
    s
}

/// [`PLACEHOLDER`] granted toward the host itself, by [`REFERENCE`].
fn granted() -> Vec<CredentialGrant> {
    vec![CredentialGrant {
        reference: REFERENCE.into(),
        toward: vec!["host.docker.internal".into()],
        placeholder: PLACEHOLDER.into(),
    }]
}

/// The harness's answers, one line for each of its requests, read to its end.
fn answers(guest: &dyn ImageGuest) -> Vec<String> {
    let out = String::from_utf8(read_all(guest, Channel::Stdout)).unwrap();
    out.lines().map(str::to_string).collect()
}

/// The run stopped, and every lease its lessor gave released by its end (D242).
fn stop_releasing(guest: &dyn ImageGuest, lessor: &TestLessor) {
    guest.stop().unwrap();
    assert_eq!(guest.wait_for(TIMEOUT).unwrap(), Some(RunEnd::Stopped));
    assert_eq!(
        lessor.released.lock().unwrap().len(),
        lessor.given(),
        "a lease the run was given was not released at its end"
    );
}

/// Plan step 33 step 7 on a real microVM. Do: grant [`PLACEHOLDER`] toward the host itself and
/// have the harness send it, over HTTP/2, as a bearer token on one stream and as `x-api-key`
/// with a body on the next, to the test's host speaking `h2` alone of what it offers. Expect:
/// the host sees both over HTTP/2 with [`VALUE`] where the placeholder stood, each field never
/// indexed (RFC 7541 §7.1.3); the harness's echoes, in a field and in DATA, come back masked;
/// the value nowhere in what the harness saw nor in the run's home; each lease released at the
/// run's end.
#[test]
fn an_h2_request_to_a_granted_host_is_filled_never_indexed_and_its_echo_comes_back_masked() {
    if cannot_run_vms() || !injector_trusts_test_roots() {
        return;
    }
    let home = TempDir::new("api-cred-h2");
    let host = tls_host(&home, &["host.docker.internal"]);
    build_asking(&home, "api-cred-h2:1", host.port, "host.docker.internal", false);
    let lessor = TestLessor::refusing(0);
    let mut s = asking_spec(
        &home,
        "api-cred-h2:1",
        &host,
        &lessor,
        &[
            "--",
            "h2",
            "GET",
            "/echo",
            "-H",
            "authorization:Bearer {env:API_KEY}",
            "--",
            "h2",
            "POST",
            "/echo",
            "-H",
            "x-api-key:{env:API_KEY}",
            "-d",
            "hello",
        ],
    );
    s.credentials = granted();
    let guest = boot(&home, &s);
    let said = answers(&*guest);
    assert_ended(&*guest, MemberEnd::Exited(0));
    let bearer = format!("Bearer {}", masked(VALUE, PLACEHOLDER));
    let key = masked(VALUE, PLACEHOLDER);
    assert_eq!(
        said,
        [
            format!("answer 200 echo={bearer} body={bearer}"),
            format!("answer 200 echo={key} body={key}"),
        ]
    );
    let asked: Vec<Asked> = host.asked.try_iter().collect();
    assert_eq!(asked.len(), 2, "{asked:?}");
    let never = Some(shards_net::hpack::Indexing::Never);
    assert!(asked.iter().all(|a| a.h2), "{asked:?}");
    assert_eq!(
        asked[0].field("authorization"),
        Some((format!("Bearer {VALUE}").as_bytes(), never)),
        "the bearer token was not filled, or was sent indexable"
    );
    assert_eq!(
        asked[1].field("x-api-key"),
        Some((VALUE.as_bytes(), never)),
        "x-api-key was not filled, or was sent indexable"
    );
    assert_eq!(asked[1].body, b"hello");
    assert!(lessor.asks.lock().unwrap().iter().all(|r| r == REFERENCE));
    assert!(lessor.given() >= 1);
    stop_releasing(&*guest, &lessor);
    assert_eq!(holding(&home, VALUE.as_bytes()), Vec::<std::path::PathBuf>::new());
}

/// Plan step 33 step 7's refused lease on a real microVM. Do: the orchestrator's lessor
/// refuses the first two asks of [`REFERENCE`]; the harness sends its placeholder once over
/// HTTP/1.1, once on an HTTP/2 stream, then on a later stream. Expect: one 503 for each of the
/// first two, nothing of either reaching the host; the third asked anew (three asks in all),
/// filled and echoed masked, the host seeing it alone, over HTTP/2, never indexed.
#[test]
fn a_refused_lease_is_answered_503_over_http_1_1_and_h2_and_a_later_request_asks_again() {
    if cannot_run_vms() || !injector_trusts_test_roots() {
        return;
    }
    let home = TempDir::new("api-cred-503");
    let host = tls_host(&home, &["host.docker.internal"]);
    build_asking(&home, "api-cred-503:1", host.port, "host.docker.internal", false);
    let lessor = TestLessor::refusing(2);
    let mut s = asking_spec(
        &home,
        "api-cred-503:1",
        &host,
        &lessor,
        &[
            "--",
            "http/1.1",
            "GET",
            "/echo",
            "-H",
            "authorization:Bearer {env:API_KEY}",
            "--",
            "h2",
            "GET",
            "/echo",
            "-H",
            "authorization:Bearer {env:API_KEY}",
            "--",
            "h2",
            "GET",
            "/echo",
            "-H",
            "authorization:Bearer {env:API_KEY}",
        ],
    );
    s.credentials = granted();
    let guest = boot(&home, &s);
    let said = answers(&*guest);
    assert_ended(&*guest, MemberEnd::Exited(0));
    assert_eq!(said.len(), 3, "{said:?}");
    assert!(said[0].starts_with("answer 503 "), "HTTP/1.1: {said:?}");
    assert!(said[1].starts_with("answer 503 "), "HTTP/2: {said:?}");
    let bearer = format!("Bearer {}", masked(VALUE, PLACEHOLDER));
    assert_eq!(said[2], format!("answer 200 echo={bearer} body={bearer}"));
    assert_eq!(
        *lessor.asks.lock().unwrap(),
        [REFERENCE; 3],
        "each request after a refusal asks anew, once"
    );
    let asked: Vec<Asked> = host.asked.try_iter().collect();
    assert_eq!(asked.len(), 1, "a refused request reached the host: {asked:?}");
    assert!(asked[0].h2);
    assert_eq!(
        asked[0].field("authorization"),
        Some((
            format!("Bearer {VALUE}").as_bytes(),
            Some(shards_net::hpack::Indexing::Never)
        ))
    );
    stop_releasing(&*guest, &lessor);
    assert_eq!(holding(&home, VALUE.as_bytes()), Vec::<std::path::PathBuf>::new());
}

/// Plan step 33 step 8 over HTTP/2 on a real microVM, Codex's sign-in played by the test's
/// host as `auth.openai.com` (no vendor is asked: the guest dials the test's host). Do: the
/// harness exchanges a code at the token endpoint over HTTP/2, then refreshes with the refresh
/// token it was given, then sends the access token it was given to the host itself. Expect: the
/// exchange reaches the host, its tokens are captured by the lessor and the harness holds the
/// capture's placeholders alone; the refresh never reaches the host, answered by the lessor
/// with placeholders again; the access token's use filled with the refreshed token, never
/// indexed, its echo masked; no token in what the harness saw nor in the run's home.
#[test]
fn an_h2_sign_in_gives_the_harness_placeholders_and_its_refresh_is_answered_outside() {
    if cannot_run_vms() || !injector_trusts_test_roots() {
        return;
    }
    let home = TempDir::new("api-signin-h2");
    let host = tls_host(&home, &["host.docker.internal", "auth.openai.com"]);
    build_asking(&home, "api-signin-h2:1", host.port, "auth.openai.com", true);
    let lessor = TestLessor::refusing(0);
    let mut s = asking_spec(
        &home,
        "api-signin-h2:1",
        &host,
        &lessor,
        &[
            "--",
            "h2",
            "POST",
            "/oauth/token",
            "-H",
            "content-type:application/x-www-form-urlencoded",
            "-d",
            "grant_type=authorization_code&code=test-code&code_verifier=test-verifier&client_id=app_test",
            "--",
            "h2",
            "POST",
            "/oauth/token",
            "-H",
            "content-type:application/json",
            "-d",
            r#"{"grant_type":"refresh_token","refresh_token":"{refresh_token}","client_id":"app_test"}"#,
            "--",
            "h2",
            "GET",
            "/echo",
            "-s",
            "host.docker.internal",
            "-H",
            "authorization:Bearer {access_token}",
        ],
    );
    s.sign_ins = vec![SignInGrant {
        sign_in: "test-codex".into(),
        cli: Cli::Codex,
        toward: vec!["host.docker.internal".into()],
        captured: None,
    }];
    let guest = boot(&home, &s);
    let said = answers(&*guest);
    assert_ended(&*guest, MemberEnd::Exited(0));
    assert_eq!(said.len(), 3, "{said:?}");
    for (i, line) in said.iter().take(2).enumerate() {
        assert!(line.starts_with("answer 200 "), "step {i}: {line}");
        assert!(
            line.contains(ACCESS_PLACEHOLDER),
            "step {i} gave no access placeholder: {line}"
        );
        assert!(
            line.contains(REFRESH_PLACEHOLDER),
            "step {i} gave no refresh placeholder: {line}"
        );
    }
    let bearer = format!("Bearer {}", masked(REAL_ACCESS_2, ACCESS_PLACEHOLDER));
    assert_eq!(said[2], format!("answer 200 echo={bearer} body={bearer}"));
    for token in [REAL_ACCESS, REAL_REFRESH, REAL_ACCESS_2, REAL_REFRESH_2] {
        assert!(
            said.iter().all(|l| !l.contains(token)),
            "the harness saw a token: {said:?}"
        );
    }
    let captured = lessor.captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].access, Secret::from(REAL_ACCESS));
    assert_eq!(captured[0].refresh, Some(Secret::from(REAL_REFRESH)));
    drop(captured);
    assert_eq!(
        *lessor.refreshed.lock().unwrap(),
        1,
        "the refresh was not answered outside"
    );
    let asked: Vec<Asked> = host.asked.try_iter().collect();
    let paths: Vec<(&str, &str, bool)> = asked
        .iter()
        .map(|a| (a.method.as_str(), a.path.as_str(), a.h2))
        .collect();
    assert_eq!(
        paths,
        [("POST", "/oauth/token", true), ("GET", "/echo", true)],
        "the refresh reached the host, or the exchange did not"
    );
    assert_eq!(
        asked[0].field("accept-encoding").map(|(v, _)| v),
        Some(&b"identity"[..])
    );
    assert_eq!(
        asked[1].field("authorization"),
        Some((
            format!("Bearer {REAL_ACCESS_2}").as_bytes(),
            Some(shards_net::hpack::Indexing::Never)
        ))
    );
    stop_releasing(&*guest, &lessor);
    for token in [REAL_ACCESS, REAL_REFRESH, REAL_ACCESS_2, REAL_REFRESH_2] {
        assert_eq!(
            holding(&home, token.as_bytes()),
            Vec::<std::path::PathBuf>::new(),
            "{token}"
        );
    }
}
