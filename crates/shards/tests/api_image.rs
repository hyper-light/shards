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
    Channel, ImageGuest, ImagePools, ImageRefusal, ImageShards, ImageShare, ImageSpec, Images as _, Keep,
    MemberEnd, MemberFile, MemberKind, MemberName, MemberReach, MemberSpec, Narrowing, PlugRefusal, Proto,
    Publish, RunEnd, Secret, StartedFrom, StatsScope,
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
        lessor: None,
        shares: Vec::new(),
        nesting: false,
        ready_by: None,
        binary: Some(common::shards().to_path_buf()),
        env: host_env(home),
        control: None,
        gpu_devices: Vec::new(),
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
        args: Vec::new(),
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
    std::fs::write(ctx.join("Agentfile"), common::hub_bases(&agentfile, TIMEOUT)).unwrap();
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
    let built = run_shards_env(
        &["build", "image"],
        &["-t", tag, ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
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
    let home = common::home_with("api-tmpl-home", TIMEOUT);
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
    let home = common::home_with("api-pool-home", TIMEOUT);
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
        }];
        s
    };
    let guest = ImageShards::new()
        .boot(
            &given(["--resume", "s 1", ""].map(String::from).to_vec()),
            TIMEOUT,
        )
        .unwrap();
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
