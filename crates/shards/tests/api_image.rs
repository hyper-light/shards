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
use std::io::{BufRead as _, BufReader, PipeReader, Read as _, Write as _};
use std::num::NonZeroU64;
use std::time::{Duration, Instant};

/// A lane's ask, its answer waited for as long as it takes (`Duration::MAX`), as a caller
/// with no bound of its own waits: what it answered, or why not.
macro_rules! answered {
    ($ask:expr) => {
        $ask.and_then(|mut ask| ask.wait(std::time::Duration::MAX))
            .map(|got| got.expect("Duration::MAX waits until the answer"))
    };
}

use common::{
    Party, Patience, TempDir, ask, booted, cannot_run_vms, guest_init, kernel, run_shards_env, served,
};
use shards_api::image::{
    BootStop, Captured, Channel, Cli, ComponentAt, CredentialGrant, EgressGrant, FinalUse, ImageGuest,
    ImagePools, ImageRefusal, ImageShards, ImageShare, ImageSpec, Images as _, Keep, LeaseRefusal, Leased,
    Lessor, MemberCeiling, MemberEnd, MemberFile, MemberKind, MemberName, MemberReach, MemberSpec, Narrowing,
    NetRefusal, Plug, PlugFor, PlugRefusal, PlugSource, Proto, Publish, Reach, RunEnd, RunEnded, RunLessor,
    SaveRefusal, Secret, SignInGrant, SignInRefusal, SignInTokens, StartedFrom, StatsScope, SwapRefusal,
    SwapWhen, Unused, UpstreamRoot,
};
use shards_api::volumes::{ClearCause, VolumeRefusal};

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

/// Builds `agentfile` (its `{image}` the test image) in [`context`]'s context, tagged `tag` in
/// `home`.
#[track_caller]
fn build(home: &TempDir, tag: &str, options: &[&str], agentfile: &str) {
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let ctx = context(options, agentfile);
    let built = run_shards_env(&["build", "image"], &["-t", tag, ctx.to_str().unwrap()], &env);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
}

/// A context holding a harness archive whose `codex` is the test guest started with
/// `options`, an agent of files alone, and an Agentfile of both over the test image, then
/// `agentfile`.
fn context(options: &[&str], agentfile: &str) -> TempDir {
    let (image, _) = served();
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
    ctx
}

/// P4.4 (D263): an Agentfile built through shards-api's `Builds` into the caller's home, its
/// progress told, is pinned by its image's ID, and that ID boots as an `ImageSpec`'s image
/// with no tag named.
#[test]
fn an_image_built_through_the_api_boots_by_its_digest() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-built-home");
    let ctx = context(&["managed-capture", "codex", "echo"], "");
    let builds = shards_api::Builds {
        binary: Some(common::shards().to_path_buf()),
        home: Some(home.to_path_buf()),
        env: host_env(&home),
    };
    let asked = shards_api::BuildSpec {
        context: ctx.to_path_buf(),
        ..shards_api::BuildSpec::default()
    };
    let mut events = Vec::new();
    let pinned = builds.build(&asked, &mut |e| events.push(e)).unwrap();
    assert!(pinned.digest.starts_with("sha256:"), "{pinned:?}");
    assert!(
        events.iter().any(|e| matches!(
            e,
            shards_api::BuildEvent::Ended {
                how: shards_api::StepEnd::Done,
                ..
            }
        )),
        "{events:?}"
    );
    let guest = boot(&home, &spec(&home, &pinned.digest));
    guest.stop().unwrap();
}

/// The longest life a lease is told, its wire's (a u64 of nanoseconds: shards-api's
/// image/session.rs), as far as this host's clock reaches: a lease of the test's broker
/// that does not lapse while its test runs.
fn longest_lease() -> Instant {
    let now = Instant::now();
    let mut life = Duration::from_nanos(u64::MAX);
    loop {
        if let Some(until) = now.checked_add(life) {
            return until;
        }
        life /= 2;
    }
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
        upstream_roots: Vec::new(),
        plug_roots: Vec::new(),
        egress_ceiling: Vec::new(),
        gpu_report: None,
    }
}

fn codex() -> MemberName {
    MemberName::harness("codex")
}

/// Where the diagnostics of a run host a test boots in `home` are kept whole
/// (`ImageGuest::keep_diagnostics`): a `.stderr` of the home's for each, numbered in boot order,
/// which a failing test shows as its home goes (`common::TempDir`). One file each, made to
/// write in place: a file opened to append alone takes no cut on Windows, which a bounded keep
/// asks of it, and so is refused.
fn diagnostics(home: &TempDir) -> std::path::PathBuf {
    static BOOTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = BOOTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    home.join(format!("run-host-{n}.stderr"))
}

/// `s` booted, its run host's diagnostics kept whole in [`diagnostics`] from its start.
#[track_caller]
fn boot(home: &TempDir, s: &ImageSpec) -> Box<dyn ImageGuest> {
    let guest = booted(&ImageShards::new(), s).unwrap();
    let kept = std::fs::File::create(diagnostics(home)).unwrap();
    guest.keep_diagnostics(kept, u64::MAX).unwrap();
    guest
}

/// The harness's end, held to `expected`; where it is not, the run's own end is said beside
/// it, which tells a run that ended first (`WithRun`) as stopped, failed in its host's words or
/// exited (CI run 43303186680: `WithRun` where `Exited(3)` was expected, its cause unsaid),
/// and the home shows its run hosts' diagnostics, their VMs' among them, as it goes ([`boot`]:
/// HVF at 2b89e8504, `WithRun` and `Exited(125)`, nothing more said).
#[track_caller]
fn assert_ended(guest: &dyn ImageGuest, expected: MemberEnd) {
    let ended = ask(guest, |g| g.wait_member(&codex(), 0, Duration::MAX)).unwrap();
    assert_eq!(
        ended,
        Some(expected),
        "the run's end: {:?}",
        guest.wait_for(Duration::ZERO)
    );
}

/// Reads `channel` of the harness to its end ([`read_within`]).
#[track_caller]
fn read_all(guest: &dyn ImageGuest, channel: Channel) -> Vec<u8> {
    read_within(guest, channel)
        .unwrap_or_else(|refused| panic!("the member's channel did not end: {refused}"))
}

/// What `channel` of the harness says to its end, read for as long as its run goes on: a
/// wait judged as the tests' waits are (`common::judged`), its kind its call site. Refused,
/// its run is stopped, which ends the channel, and what its wait said of itself comes back.
/// The channel of a member that has ended already is read as one: no answer waited for,
/// which teaches its kind nothing (`common::judged_there`; #77: such reads taught a W* of
/// 21 ms, R* 0 ns, that refused a read waiting on an outside sign-in).
#[track_caller]
fn read_within(guest: &dyn ImageGuest, channel: Channel) -> Result<Vec<u8>, String> {
    let mut pipe = guest.take_output(&codex(), 0, channel).unwrap();
    let ended = matches!(guest.wait_member(&codex(), 0, Duration::ZERO), Ok(Some(_)));
    let stop = || {
        let _ = guest.stop();
    };
    let read = move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    };
    if ended {
        common::judged_there(Party::started(None), "a member's channel", stop, read)
    } else {
        common::judged(Party::started(None), "a member's channel", stop, read)
    }
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
    assert_eq!(
        ask(&*guest, |g| g.wait_for(Duration::MAX))
            .unwrap()
            .map(|e| e.end),
        Some(RunEnd::Stopped)
    );
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
    let refused = |s: ImageSpec| match booted(&ImageShards::new(), &s) {
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
    // A deadline of any length: what is refused is its asking, not its length.
    s.ready_by = Some(Duration::ZERO);
    assert_eq!(
        refused(s),
        ImageRefusal::Widening(shards_abi::run::nesting::undeclared())
    );
    let mut s = spec(&home, "api-refused:1");
    s.ready_by = Some(Duration::ZERO);
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
        exec: true,
        members: None,
    }];
    let guest = boot(&home, &s);
    let said = String::from_utf8(read_all(&*guest, Channel::Stdout)).unwrap();
    assert_eq!(said, "== /work/hello.txt\nfrom the host\n");
    assert_ended(&*guest, MemberEnd::Exited(0));
}

/// A project shared at its host path, writable, with its harness (D277), as kiln launches one
/// (its `isolated.rs`): beneath the host's own per-user temporary directory (macOS's
/// `/var/folders/…`, a path through its `/var` link), its harness's working directory, beside a
/// plugin folder at `/plugins/p`, read-only, given its harness too, its person's home shared at
/// the image's `VOLUME` for its harness and given its harness (a share's members narrowing what
/// a `VOLUME` gives, never widening it), and its run's plug roots its project's top-level
/// directory, the home's and `/volumes` (D271). Its harness starts in its project, which it
/// reads at its host path, its files its own (D38), and reads its plugin. Where its host path
/// is no guest's ([`guest_at`]: Linux's tests keep theirs in `/tmp`, Windows' a `C:\` path),
/// it is shared beneath `/work` by its name, as kiln shares a Windows project.
#[test]
fn a_project_given_its_harness_is_where_it_works() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-given-home");
    let person = TempDir::new("api-given-person");
    let plugin = TempDir::new("api-given-plugin");
    let proj = HostPathDir::new("api-given-proj");
    let proj = proj.path();
    std::fs::write(proj.join("hello.txt"), "from the project\n").unwrap();
    std::fs::write(plugin.join("plugin.md"), "a plugin\n").unwrap();
    let at = guest_at(&proj);
    let file = format!("{at}/hello.txt");
    build(
        &home,
        "api-given:1",
        &["stat", "hello.txt", &file, "/plugins/p/plugin.md", "/tmp"],
        "VOLUME --target-kind=harness /home/stub FOR codex\n",
    );
    let mut s = spec(&home, "api-given:1");
    let to_codex = Some(PlugFor::Members(vec![codex()]));
    s.shares = vec![
        ImageShare {
            host: proj.clone(),
            at: at.clone(),
            read_only: false,
            exec: true,
            members: to_codex.clone(),
        },
        ImageShare {
            host: plugin.to_path_buf(),
            at: "/plugins/p".into(),
            read_only: true,
            exec: true,
            members: to_codex,
        },
        ImageShare {
            host: person.to_path_buf(),
            at: "/home/stub".into(),
            read_only: false,
            exec: true,
            members: Some(PlugFor::Members(vec![codex()])),
        },
    ];
    // Its plug roots as kiln's are: its project's top-level directory, the home's and
    // `/volumes`, each once (a root granted twice is refused).
    let mut roots: Vec<String> = Vec::new();
    for root in plug_root_of(&at)
        .into_iter()
        .chain([String::from("/home"), String::from("/volumes")])
    {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    s.plug_roots = roots;
    s.members = vec![MemberSpec {
        member: codex(),
        env: Vec::new(),
        files: Vec::new(),
        workdir: Some(at.clone()),
        stdin: false,
        terminal: false,
        args: Vec::new(),
        mcp: false,
    }];
    let guest = boot(&home, &s);
    let said = String::from_utf8(read_all(&*guest, Channel::Stdout)).unwrap();
    assert_ended(&*guest, MemberEnd::Exited(0));
    // Each path's kind, mode, owner and size, and a file's bytes: `stat` in the test guest.
    let owner = |path: &str| {
        said.lines()
            .find(|l| l.starts_with(&format!("{path} ")))
            .and_then(|l| l.split(' ').nth(3))
            .map(str::to_owned)
    };
    assert!(
        said.contains("hello.txt file") && said.contains("= from the project\\n"),
        "{said}"
    );
    assert!(
        said.contains(&format!("{file} file")),
        "its project at its host path: {said}"
    );
    assert!(
        said.contains("/plugins/p/plugin.md file") && said.contains("= a plugin\\n"),
        "{said}"
    );
    assert_eq!(
        owner("hello.txt"),
        owner("/tmp"),
        "its project's files its own, as its scratch is: {said}"
    );
}

/// A share given no member stays the run's own root's (D277; a member's default deny, D59):
/// its harness, given none, finds nothing at the share's path, its host path where a guest's
/// may be it ([`guest_at`]).
#[test]
fn a_share_given_no_member_is_the_runs_alone() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-ungiven-home");
    let proj = TempDir::new("api-ungiven-proj");
    let real = std::fs::canonicalize(proj.to_path_buf()).unwrap();
    std::fs::write(real.join("hello.txt"), "from the project\n").unwrap();
    let at = guest_at(&real);
    let file = format!("{at}/hello.txt");
    build(&home, "api-ungiven:1", &["files", &file], "");
    let mut s = spec(&home, "api-ungiven:1");
    s.shares = vec![ImageShare {
        host: real,
        at,
        read_only: false,
        exec: true,
        members: None,
    }];
    let guest = boot(&home, &s);
    let said = String::from_utf8(read_all(&*guest, Channel::Stdout)).unwrap();
    assert_eq!(said, format!("== {file}\nerrno 2\n"));
    assert_ended(&*guest, MemberEnd::Exited(0));
}

/// P6.1 (D277): a member whose working directory its guest cannot enter as it starts is
/// refused by its guest's own step (init's `entering its working directory`), typed `Workdir`
/// to its orchestrator as its members start, naming the directory and the errno: its project
/// shared and given no member, given at another path, or not shared at all. A working
/// directory its image holds (`/etc`) starts as it did before P6.1, no share given it. Its
/// shared control, the project given its harness, starts in it:
/// `a_project_given_its_harness_is_where_it_works`.
#[test]
fn a_members_workdir_its_guest_cannot_enter_is_refused_typed() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-unheld-home");
    let proj = TempDir::new("api-unheld-proj");
    let real = std::fs::canonicalize(proj.to_path_buf()).unwrap();
    let at = real.to_str().unwrap().to_owned();
    build(&home, "api-unheld:1", &["files", "hello.txt"], "");
    let started_in = |shares: Vec<ImageShare>, workdir: &str| {
        let mut s = spec(&home, "api-unheld:1");
        s.shares = shares;
        s.members = vec![MemberSpec {
            member: codex(),
            env: Vec::new(),
            files: Vec::new(),
            workdir: Some(workdir.into()),
            stdin: false,
            terminal: false,
            args: Vec::new(),
            mcp: false,
        }];
        boot(&home, &s)
    };
    let share = |members| ImageShare {
        host: real.clone(),
        at: at.clone(),
        read_only: false,
        exec: true,
        members,
    };
    for (shares, workdir) in [
        (vec![share(None)], at.clone()),
        (
            vec![share(Some(PlugFor::Members(vec![codex()])))],
            format!("{at}-elsewhere"),
        ),
        (Vec::new(), at.clone()),
    ] {
        let guest = started_in(shares, &workdir);
        match ask(&*guest, |g| g.wait_member(&codex(), 0, Duration::MAX)) {
            Err(ImageRefusal::Workdir(why)) => {
                assert!(why.contains(&workdir) && why.contains("errno 2"), "{why}");
            }
            other => panic!("{workdir}: {other:?}"),
        }
    }
    let guest = started_in(Vec::new(), "/etc");
    assert_ended(&*guest, MemberEnd::Exited(0));
}

/// A share given a member the image declares none of, or one of files alone, is refused by its
/// run host, by what (D277), before any member starts; where the image's own `VOLUME` lies, a
/// share may narrow whom it reaches (its harness, which the `VOLUME` is for), and is refused as
/// a widening for a member the `VOLUME`'s `FOR` names not.
#[test]
fn a_share_given_no_member_of_its_image_is_refused() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-misgiven-home");
    let proj = TempDir::new("api-misgiven-proj");
    let real = std::fs::canonicalize(proj.to_path_buf()).unwrap();
    build(
        &home,
        "api-misgiven:1",
        &["files", "/proc/self/cgroup"],
        "VOLUME --target-kind=harness /work FOR codex\nVOLUME --target-kind=agent /theirs FOR helper\n",
    );
    let given = |at: &str, member: MemberName| {
        let mut s = spec(&home, "api-misgiven:1");
        s.shares = vec![ImageShare {
            host: real.clone(),
            at: at.into(),
            read_only: false,
            exec: true,
            members: Some(PlugFor::Members(vec![member])),
        }];
        booted(&ImageShards::new(), &s).map(|_| ())
    };
    match given("/proj", MemberName::harness("nope")) {
        Err(ImageRefusal::Image(why)) => assert!(why.contains("declares no such member"), "{why}"),
        other => panic!("{other:?}"),
    }
    match given("/proj", MemberName::agent("helper")) {
        Err(ImageRefusal::Image(why)) => assert!(why.contains("files alone"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(
        given("/work", codex()).is_ok(),
        "its harness, whom the VOLUME there is for"
    );
    match given("/theirs", codex()) {
        Err(ImageRefusal::Widening(why)) => assert!(why.contains("VOLUME") && why.contains("codex"), "{why}"),
        other => panic!("{other:?}"),
    }
}

/// A test's directory where the host's users keep their temporary files as the OS names it:
/// on macOS `DARWIN_USER_TEMP_DIR`, beneath `/var/folders`, not where `TMPDIR` may point, made
/// and removed here; elsewhere the test's own (`TempDir`).
enum HostPathDir {
    Os(std::path::PathBuf),
    Test(TempDir),
}

impl HostPathDir {
    fn new(name: &str) -> Self {
        if !cfg!(target_os = "macos") {
            return Self::Test(TempDir::new(name));
        }
        let out = std::process::Command::new("getconf")
            .arg("DARWIN_USER_TEMP_DIR")
            .output()
            .unwrap();
        let base = std::path::PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
        let dir = base.join(format!("{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self::Os(dir)
    }

    fn path(&self) -> std::path::PathBuf {
        match self {
            Self::Os(dir) => dir.clone(),
            Self::Test(dir) => dir.to_path_buf(),
        }
    }
}

impl Drop for HostPathDir {
    fn drop(&mut self) {
        if let Self::Os(dir) = self {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Where a run's microVM sees host directory `host`, as kiln shares a project
/// (`isolated::guest_path`): at its own path where that is a Unix one beneath a root a
/// run's domains may be given things under ([`plug_root_of`]); else beneath `/work` by its
/// own name. A Windows path names no Linux one; and a path beneath a directory init keeps
/// its own is none a share may be at: Linux's tests keep theirs in `/tmp`, each domain's
/// scratch, at or under which init refuses a share (D139), and a plug root there is refused
/// (`plug::root_name`; CI run 43876891053: "\"/tmp\": a plug root is one top-level
/// directory").
fn guest_at(host: &std::path::Path) -> String {
    match host.to_str() {
        Some(path) if plug_root_of(path).is_some() => path.to_owned(),
        _ => format!("/work/{}", host.file_name().unwrap().to_string_lossy()),
    }
}

/// The plug root guest path `at` lies beneath, as kiln's `isolated::plug_root_of` takes it:
/// its top-level directory, where a root may be one (`plug::root_name`, D271).
fn plug_root_of(at: &str) -> Option<String> {
    let root = format!("/{}", at.strip_prefix('/')?.split('/').next()?);
    shards_abi::run::plug::root_name(root.as_bytes())
        .is_some()
        .then_some(root)
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
    let guest = booted(&ImageShards::new(), &s).unwrap();
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
    // The listener comes up once its member runs: asked until it answers, while its run
    // goes on (`Patience` of what this test started), each answer read as a stream its run
    // carries (`common::Following`).
    let mut patience = Patience::started();
    let answer = loop {
        if let Ok(mut c) = std::net::TcpStream::connect(bound.host) {
            let _ = c.write_all(b"hello\n");
            let mut got = Vec::new();
            let _ = common::Following::of(&c, None).read_to_end(&mut got);
            if !got.is_empty() {
                break got;
            }
        }
        assert!(patience.holds(), "no answer on {}", bound.host);
        std::thread::sleep(patience.next_look());
    };
    patience.done();
    assert!(!answer.is_empty());
    guest.stop().unwrap();
}

/// D279: a share or a plug whose files may not be run refuses its programs, and one whose may
/// runs them. The harness is given a share at `/proj` as it starts (D277) and a plug at
/// `/plugs/p` once it runs (D271), each holding the test guest; its `run-when` runs each once
/// it is there. Not runnable, exec(2) refuses both (EACCES, 13: the mounts are `noexec`);
/// runnable, both run and exit 0.
#[test]
fn a_share_or_plug_whose_files_may_not_be_run_refuses_its_programs() {
    if cannot_run_vms() {
        return;
    }
    for exec in [false, true] {
        let home = TempDir::new("api-exec-home");
        let (shared, plugged) = (TempDir::new("api-exec-share"), TempDir::new("api-exec-plug"));
        for dir in [&shared, &plugged] {
            std::fs::create_dir_all(dir.join("bin")).unwrap();
            std::fs::write(dir.join("bin/tg"), std::fs::read(common::test_guest()).unwrap()).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(dir.join("bin/tg"), std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        build(
            &home,
            "api-exec:1",
            &["run-when", "/proj/bin/tg", "/plugs/p/bin/tg"],
            "",
        );
        let mut s = spec(&home, "api-exec:1");
        s.shares = vec![ImageShare {
            host: shared.to_path_buf(),
            at: "/proj".into(),
            read_only: true,
            exec,
            members: Some(PlugFor::Members(vec![codex()])),
        }];
        s.plug_roots = vec!["/plugs".into()];
        s.members = vec![MemberSpec {
            member: codex(),
            env: Vec::new(),
            files: Vec::new(),
            workdir: None,
            stdin: false,
            terminal: false,
            args: Vec::new(),
            mcp: false,
        }];
        let guest = boot(&home, &s);
        let said = if exec { "ran 0\n" } else { "run errno 13\n" };
        let stdout = BufReader::new(guest.take_output(&codex(), 0, Channel::Stdout).unwrap());
        let (stdout, line) = line_within(&*guest, stdout);
        assert_eq!(line, said, "its share's program, exec {exec}");
        let plug = Plug {
            source: PlugSource::Host(plugged.to_path_buf()),
            to: PlugFor::Members(vec![codex()]),
            at: "/plugs/p".into(),
            read_only: true,
            exec,
        };
        ask(&*guest, |g| g.plug(&plug, Duration::from_secs(60))).unwrap();
        let (_, line) = line_within(&*guest, stdout);
        assert_eq!(line, said, "its plug's program, exec {exec}");
        guest.stop().unwrap();
    }
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
        exec: true,
        members: None,
    }];
    let guest = booted(&ImageShards::new(), &s).unwrap();
    let plug = |to: &MemberName, at: &str| Plug {
        source: PlugSource::Host(proj.clone()),
        to: PlugFor::Members(vec![to.clone()]),
        at: at.into(),
        read_only: false,
        exec: true,
    };
    match ask(&*guest, |g| {
        g.plug(&plug(&codex(), "/outside/proj"), Duration::MAX)
    }) {
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
            ask(&*guest, |g| g.plug(&plug(&helper, &dest), Duration::MAX)),
            Err(ImageRefusal::Plug(PlugRefusal::Member, _))
        ),
        "a member of files alone"
    );
    let barrier = std::sync::Barrier::new(2);
    let attempt = || {
        barrier.wait();
        ask(&*guest, |g| g.plug(&plug(&codex(), &dest), Duration::MAX))
    };
    let results = std::thread::scope(|scope| {
        let a = scope.spawn(attempt);
        let b = scope.spawn(attempt);
        [a.join().unwrap(), b.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1, "{results:?}");
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(ImageRefusal::Plug(PlugRefusal::Taken, _))))
            .count(),
        1,
        "{results:?}"
    );
    let seen_at = proj.join("seen");
    let mut patience = Patience::started();
    let seen = loop {
        if let Ok(seen) = std::fs::read_to_string(&seen_at) {
            break seen;
        }
        assert!(patience.holds(), "the harness wrote no {}", seen_at.display());
        std::thread::sleep(patience.next_look());
    };
    patience.done();
    for root in &roots {
        let line = format!("root {root} mkdir errno 30 create errno 30");
        assert!(seen.lines().any(|l| l == line), "no {line:?} in\n{seen}");
    }
    assert!(seen.lines().any(|l| l == "from-host host"), "{seen}");
    assert_eq!(
        std::fs::read_to_string(proj.join("from-member")).unwrap(),
        "member\n"
    );
    ask(&*guest, |g| g.unplug(&dest, Duration::MAX)).unwrap();
    assert!(matches!(
        ask(&*guest, |g| g.unplug(&dest, Duration::MAX)),
        Err(ImageRefusal::Plug(PlugRefusal::Unknown, _))
    ));
    guest.stop().unwrap();
    assert_eq!(
        ask(&*guest, |g| g.wait_for(Duration::MAX))
            .unwrap()
            .map(|e| e.end),
        Some(RunEnd::Stopped)
    );
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
    let guest = booted(&ImageShards::new(), &spec(&home, "api-stats:1")).unwrap();
    let run = ask(&*guest, |g| g.stats(StatsScope::Run, Duration::MAX)).unwrap();
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
    let member = ask(&*guest, |g| g.stats(StatsScope::Member(1), Duration::MAX)).unwrap();
    assert!(member.pids.is_some_and(|p| p >= 1), "{member:?}");
    assert_eq!(member.microvm, None, "a member's scope is inside the microVM");
    assert!(ask(&*guest, |g| g.stats(StatsScope::Member(9), Duration::MAX)).is_err());
    guest.stop().unwrap();
    assert_eq!(
        ask(&*guest, |g| g.wait_for(Duration::MAX))
            .unwrap()
            .map(|e| e.end),
        Some(RunEnd::Stopped)
    );
}

/// The host's wall clock, in nanoseconds since the Unix epoch.
fn now_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// A run paused by its orchestrator (D267) runs no guest and goes on where it stopped: do boot
/// a harness that prints its count and its clock every 50 ms, pause it, read its stats across
/// a window of 3 s while a judged read of its next line waits, ask its guest a room and a
/// member's stats, hold its rate, then resume it. Expect: its vCPUs' guest time flat across
/// the window (but a step of its count), its paused time grown by the window and said paused,
/// its guest's lines what its init last said before the pause; no line while paused; the room and the member's stats refused `Paused` at once, the rate
/// answered; the judged read not refused, its next line the count after the last before the
/// pause, its clock the host's (within the host's reads around it); every count in order, none
/// lost; and its paused time in its end.
#[test]
fn a_paused_run_runs_no_guest_and_goes_on_where_it_stopped() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-pause-home");
    build(&home, "api-pause:1", &["ticks", "50"], "");
    let guest = boot(&home, &spec(&home, "api-pause:1"));
    let pipe = guest.take_output(&codex(), 0, Channel::Stdout).unwrap();
    let (lines, heard) = std::sync::mpsc::channel::<(u128, String)>();
    let heard = std::sync::Mutex::new(heard);
    let reader = std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::BufReader::new(pipe).lines().map_while(Result::ok) {
            if lines.send((now_ns(), line)).is_err() {
                break;
            }
        }
    });
    let count = |line: &str| -> (u64, u128) {
        let (n, ns) = line.split_once(' ').unwrap_or_else(|| panic!("{line:?}"));
        (n.parse().unwrap(), ns.parse().unwrap())
    };
    let next = |what: &str| {
        common::judged(
            Party::started(None),
            what,
            || {
                let _ = guest.stop();
            },
            || heard.lock().unwrap().recv().ok(),
        )
        .unwrap_or_else(|refused| panic!("{refused}"))
        .unwrap_or_else(|| panic!("{what}: its stdout ended"))
    };
    assert_eq!(next("its harness's start").1, "ready");
    let mut seen = Vec::new();
    while seen.len() < 5 {
        seen.push(count(&next("a tick").1).0);
    }
    let guest_ns =
        |stats: &shards_api::image::Stats| stats.microvm.as_ref().unwrap().guest.as_ref().unwrap().total_ns();
    // The step its guest time is read in, measured as it runs, before its pause.
    let step = u64::try_from(
        common::guest_time_step(|| {
            guest_ns(&ask(&*guest, |g| g.stats(StatsScope::Run, Duration::MAX)).unwrap())
        })
        .as_nanos(),
    )
    .unwrap();
    // Its init's last answer before the pause, which its stats say while it is paused (its run
    // host keeps each answer as its last): read after the step's reads, each an answer too.
    let before = ask(&*guest, |g| g.stats(StatsScope::Run, Duration::MAX)).unwrap();
    assert!(
        before.cpu_ns.is_some() && before.paused.is_some_and(|p| !p.now),
        "{before:?}"
    );
    let paused = ask(&*guest, |g| g.pause(Duration::MAX)).unwrap();
    assert!(paused.now, "{paused:?}");
    // What its guest printed before its vCPUs were held, still on its way.
    let first = ask(&*guest, |g| g.stats(StatsScope::Run, Duration::MAX)).unwrap();
    while let Ok((_, line)) = heard.lock().unwrap().try_recv() {
        seen.push(count(&line).0);
    }
    let last = *seen.last().unwrap();
    let window = Duration::from_secs(3);
    // A failure while its run is paused would leave the reading below waiting for a tick that
    // never comes, the scope with it: stopped as the failure unwinds, its stdout ends.
    struct StopOnUnwind<'a>(&'a dyn ImageGuest);
    impl Drop for StopOnUnwind<'_> {
        fn drop(&mut self) {
            if std::thread::panicking() {
                let _ = self.0.stop();
            }
        }
    }
    let resuming = std::thread::scope(|scope| {
        let _stop = StopOnUnwind(&*guest);
        // Read on while it is paused: a wait judged by what this test's processes run, which
        // its pause holds, so it is never refused for its stillness.
        let reading = scope.spawn(|| next("its first tick past the pause"));
        let started = Instant::now();
        assert_eq!(
            ask(&*guest, |g| g.room(Some(64 << 20), Duration::MAX)),
            Err(ImageRefusal::Paused { what: "room" })
        );
        assert_eq!(
            ask(&*guest, |g| g.stats(StatsScope::Member(1), Duration::MAX)).map(|_| ()),
            Err(ImageRefusal::Paused { what: "stats" })
        );
        let quarter = NonZeroU64::new(250).unwrap();
        assert_eq!(
            ask(&*guest, |g| g.rate(Some(quarter), Duration::MAX)).unwrap(),
            Some(quarter)
        );
        assert_eq!(ask(&*guest, |g| g.rate(None, Duration::MAX)).unwrap(), None);
        std::thread::sleep(window.saturating_sub(started.elapsed()));
        let later = ask(&*guest, |g| g.stats(StatsScope::Run, Duration::MAX)).unwrap();
        assert!(
            guest_ns(&later) <= guest_ns(&first) + step,
            "its guest ran {} ns while paused, read in {step} ns steps",
            guest_ns(&later) - guest_ns(&first)
        );
        let (at_first, at_later) = (first.paused.unwrap(), later.paused.unwrap());
        assert!(at_first.now && at_later.now, "{at_first:?} {at_later:?}");
        assert!(
            at_later.total >= at_first.total + window - Duration::from_millis(50),
            "{at_later:?}"
        );
        // Its guest's lines while paused: what its init last said before the pause.
        assert_eq!(
            (first.cpu_ns, first.memory_bytes),
            (before.cpu_ns, before.memory_bytes)
        );
        assert_eq!(
            (later.cpu_ns, later.memory_bytes),
            (before.cpu_ns, before.memory_bytes)
        );
        let before_resume = now_ns();
        let resumed = ask(&*guest, |g| g.resume(Duration::MAX)).unwrap();
        assert!(!resumed.now && resumed.total >= window, "{resumed:?}");
        let again = ask(&*guest, |g| g.resume(Duration::MAX)).unwrap();
        assert!(!again.now, "a resume of a running run is done already");
        (before_resume, reading.join().unwrap())
    });
    let (before_resume, (heard_at, line)) = resuming;
    let (n, realtime) = count(&line);
    assert_eq!(n, last + 1, "it went on from {last}: {line}");
    assert!(
        heard_at >= before_resume,
        "a line came while it was paused: {line}"
    );
    assert!(
        (before_resume..=heard_at).contains(&realtime),
        "its clock {realtime} past the pause, the host's {before_resume}..={heard_at}"
    );
    seen.push(n);
    assert!(
        seen.windows(2).all(|w| w[1] == w[0] + 1),
        "counts lost or out of order: {seen:?}"
    );
    guest.stop().unwrap();
    let ended = ask(&*guest, |g| g.wait_for(Duration::MAX)).unwrap().unwrap();
    assert_eq!(ended.end, RunEnd::Stopped);
    assert!(ended.paused >= window, "{ended:?}");
    assert_eq!(
        ask(&*guest, |g| g.pause(Duration::MAX)).map(|_| ()),
        Err(ImageRefusal::Ended(ended))
    );
    reader.join().unwrap();
}

/// A guest's watchdogs take its run's pause for its host's hold, not a lockup (D267): on KVM
/// x86-64 each vCPU held through the pause tells its guest's kvmclock it was stopped as it goes
/// on (`KVM_KVMCLOCK_CTRL`), which the soft-lockup and RCU stall watchdogs read before they
/// report (kernel/watchdog.c, kernel/rcu/tree_stall.h, `kvm_check_and_clear_guest_paused`). Do
/// run an image's own command (`watchdogs`, the test guest's) that says its guest's
/// `watchdog_thresh`, the kernel's taint and its count of RCU stalls; pause the run past the
/// soft-lockup threshold that follows (twice `watchdog_thresh`) by one of the watchdog's sample
/// periods (a fifth of the threshold), the kernel's own rule, so that its look after the pause
/// is past it whatever its phase; resume; and read the taint and the count once the watchdog
/// has looked since. Expect: its soft-lockup watchdog on, and neither moved: no `L` taint (a
/// soft lockup's report sets it, as it prints `BUG: soft lockup`) and no RCU stall counted (as
/// `rcu_sched self-detected stall` is printed). A member of an Agentfile's image reads no
/// kernel log (D115: `dmesg_restrict` 1, no CAP_SYSLOG) and the run's console is discarded, so
/// these are what the kernel keeps of those reports that its guest may read. Red with
/// `Vcpu::went_on` emptied on KVM x86-64. Elsewhere there is nothing to tell (D267).
#[test]
fn a_paused_runs_guest_takes_its_pause_for_its_hosts_on_kvm_x86_64() {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        eprintln!(
            "SKIP: kvmclock's stopped flag is KVM x86-64's alone; arm64 KVM, Hypervisor.framework and WHP \
             give their guests no flag to read (D267)"
        );
        return;
    }
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-dog-home");
    build(&home, "api-dog:1", &["sleep-for", "600000"], "");
    let out = TempDir::new("api-dog-out");
    let mut s = spec(&home, "api-dog:1");
    s.command = Some(["/bin/testguest", "watchdogs", "/out"].map(String::from).to_vec());
    s.shares = vec![ImageShare {
        host: out.to_path_buf(),
        at: "/out".into(),
        read_only: false,
        exec: true,
        members: None,
    }];
    let guest = boot(&home, &s);
    let said = |name: &str| -> std::collections::BTreeMap<String, String> {
        let at = out.join(name);
        let mut patience = Patience::started();
        let text = loop {
            if let Ok(text) = std::fs::read_to_string(&at) {
                break text;
            }
            assert!(patience.holds(), "its command wrote no {}", at.display());
            std::thread::sleep(patience.next_look());
        };
        patience.done();
        let words: Vec<&str> = text.split_whitespace().collect();
        words
            .chunks(2)
            .map(|kv| {
                (
                    kv[0].to_string(),
                    kv.get(1).copied().unwrap_or_default().to_string(),
                )
            })
            .collect()
    };
    let before = said("before");
    assert_eq!(before["soft"], "1", "its soft-lockup watchdog is off: {before:?}");
    let thresh: u64 = before["thresh"].parse().unwrap();
    // kernel/watchdog.c: the soft-lockup threshold is twice `watchdog_thresh`, looked at five
    // times within it (`get_softlockup_thresh`, `NUM_SAMPLE_PERIODS`).
    let threshold = Duration::from_secs(thresh * 2);
    let sample = threshold / 5;
    assert!(ask(&*guest, |g| g.pause(Duration::MAX)).unwrap().now);
    std::thread::sleep(threshold + sample);
    assert!(!ask(&*guest, |g| g.resume(Duration::MAX)).unwrap().now);
    // Its command's sleep across the pause ends as its vCPU goes on; one sample later the
    // watchdog has looked, and its command says so.
    std::thread::sleep(sample);
    let after = said("after");
    assert_eq!(
        (&after["tainted"], &after["stalls"]),
        (&before["tainted"], &before["stalls"]),
        "its guest took a pause of {:?} for a lockup (taint `L` is 1 << 14): before {before:?}, after {after:?}",
        threshold + sample
    );
    assert_eq!(
        ask(&*guest, |g| g.wait_for(Duration::MAX))
            .unwrap()
            .map(|e| e.end),
        Some(RunEnd::Exited(0))
    );
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
    let guest = booted(&ImageShards::new(), &spec(&home, "api-room:1")).unwrap();
    let room: u64 = 64 << 20;
    let held = ask(&*guest, |g| {
        g.room(Some(i64::try_from(room).unwrap()), Duration::MAX)
    })
    .unwrap()
    .expect("held within the room told");
    // Whole pages, as `memory.high` and `memory.current` are kept: exactly the room told.
    assert_eq!(held, room);
    let least = ask(&*guest, |g| g.room(Some(0), Duration::MAX))
        .unwrap()
        .expect("held within the least room");
    assert!(least > 0 && least < held, "held within {least}, short of {held}");
    assert_eq!(ask(&*guest, |g| g.room(None, Duration::MAX)).unwrap(), None);
    guest.stop().unwrap();
    assert_eq!(
        ask(&*guest, |g| g.wait_for(Duration::MAX))
            .unwrap()
            .map(|e| e.end),
        Some(RunEnd::Stopped)
    );
}

/// kiln boots every harness at shards-api's least envelope (`envelope(ARCH, 0, 0)`: 1 vCPU,
/// and 64 MiB on arm64, 128 on x86-64) and leaves its members' memory to D148's plug. An
/// orchestrator's run may plug what a daemon's run without a limit may, its host's memory, and
/// its members' share counts what it may still plug as their domains start (D148, D149): a
/// harness holding four times what its microVM booted with, a page at a time, is given it as
/// it grows and ends 0, where init ended kiln's `alloc 256` at 64 MiB's `MemAvailable` (the
/// agents' memory "past their share, 20873216"). Its kernel reports what its run may have,
/// its host's memory; and one allocation past all of that, which nothing its run may plug
/// could hold, is refused by its kernel at once, none of it touched.
#[test]
fn a_member_at_the_least_envelope_grows_into_the_memory_its_run_may_plug() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-share-home");
    build(&home, "api-share:1", &[], "");
    let (vcpus, memory_mib) = shards_api::envelope(std::env::consts::ARCH, 0, 0).unwrap();
    // The harness given `args` (D256) in a run at the least envelope: what it said, its end.
    let ran = |args: &[&str]| {
        let mut s = spec(&home, "api-share:1");
        (s.vcpus, s.memory_mib) = (vcpus, memory_mib);
        s.members = vec![MemberSpec {
            member: codex(),
            env: Vec::new(),
            files: Vec::new(),
            workdir: None,
            stdin: false,
            terminal: false,
            args: args.iter().map(ToString::to_string).collect(),
            mcp: false,
        }];
        let guest = boot(&home, &s);
        let said = String::from_utf8_lossy(&read_all(&*guest, Channel::Stdout)).into_owned();
        let ended = ask(&*guest, |g| g.wait_member(&codex(), 0, Duration::MAX)).unwrap();
        guest.stop().unwrap();
        (said, ended)
    };
    let grown = 4 * memory_mib;
    let alloc = ran(&["alloc", &grown.to_string()]);
    let (meminfo, _) = ran(&["meminfo", "MemTotal", "SwapTotal"]);
    let kib = |field: &str| -> u64 {
        meminfo
            .lines()
            .find_map(|l| l.strip_prefix(field))
            .and_then(|v| v.trim().strip_suffix("kB"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(|| panic!("no {field} in {meminfo:?}"))
    };
    let (total, swap) = (kib("MemTotal:"), kib("SwapTotal:"));
    assert_eq!(
        alloc,
        (format!("allocated {grown}\n"), Some(MemberEnd::Exited(0))),
        "its kernel's MemTotal {total} kB"
    );
    let host = common::host_memory() / 1024;
    assert!(
        total > host / 2 && total <= host,
        "MemTotal {total} kB, the host's {host} kB"
    );
    // A MiB past all the memory its kernel counts, its swap's with it.
    let past = (total + swap) / 1024 + 1;
    let (said, ended) = ran(&["alloc-once", &past.to_string()]);
    assert!(
        said.starts_with(&format!("alloc-once {past}: memory allocation failed")),
        "{said:?}, MemTotal {total} kB, SwapTotal {swap} kB"
    );
    assert_eq!(ended, Some(MemberEnd::Exited(1)));
}

/// The CPU rate an orchestrator holds its run's vCPUs to (D247) reaches its VM through its
/// host: a run spinning its one vCPU, held to a quarter of its CPU, runs its guest a quarter
/// of the time passing, but what a period may overrun, a period's share where the window and
/// the periods do not line up, and one step of its guest time's count (`guest_time_step`),
/// and is let go each period; held to its vCPU's whole CPU,
/// nothing holds it, and the answer says so; told none, its hold is lifted. The period it is
/// held over is the one its VM said with the rate's answer, D191's for one vCPU at a quarter
/// CPU: 4g, g its VM process's own and never under the governor's least wait.
#[test]
fn the_orchestrators_rate_holds_its_run_to_it() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-rate-home");
    build(&home, "api-rate:1", &["spin"], "");
    let guest = booted(&ImageShards::new(), &spec(&home, "api-rate:1")).unwrap();
    assert_eq!(guest.rate_period(), None, "no rate answered yet");
    let ran = || {
        let stats = ask(&*guest, |g| g.stats(StatsScope::Run, Duration::MAX)).unwrap();
        stats.microvm.unwrap().guest.unwrap().total_ns()
    };
    // The step its guest time is read in, measured as its vCPU spins unheld.
    let step = u64::try_from(common::guest_time_step(ran).as_nanos()).unwrap();
    let quarter = NonZeroU64::new(250).unwrap();
    assert_eq!(
        ask(&*guest, |g| g.rate(Some(quarter), Duration::MAX)).unwrap(),
        Some(quarter)
    );
    let period = guest.rate_period().unwrap();
    assert!(
        period >= 4 * shards_vmm::platform::granule::LEAST_WAIT,
        "{period:?}"
    );
    let (start, before) = (Instant::now(), ran());
    std::thread::sleep(Duration::from_secs(2));
    let (used, wall) = (ran() - before, u64::try_from(start.elapsed().as_nanos()).unwrap());
    // A quarter of each period the share, and a period's whole share and what one vCPU
    // overruns before its governor reads at the window's two ends: two periods. Read twice,
    // its guest time may say a step of its count more than ran between the reads.
    let most = wall / 4 + 2 * u64::try_from(period.as_nanos()).unwrap() + step;
    assert!(
        used <= most,
        "{used} ns of guest in {wall} ns, held to a quarter CPU over {period:?}, read in {step} ns steps"
    );
    assert!(
        used >= wall / 20,
        "{used} ns of guest in {wall} ns: held, and never let go"
    );
    assert_eq!(
        ask(&*guest, |g| g.rate(NonZeroU64::new(1000), Duration::MAX)).unwrap(),
        None,
        "its one vCPU's whole CPU holds nothing"
    );
    assert_eq!(ask(&*guest, |g| g.rate(None, Duration::MAX)).unwrap(), None);
    guest.stop().unwrap();
    assert_eq!(
        ask(&*guest, |g| g.wait_for(Duration::MAX))
            .unwrap()
            .map(|e| e.end),
        Some(RunEnd::Stopped)
    );
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
#[track_caller]
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
/// started ends the run failed in its guest's words (D249), which the assertion says. Its end
/// carries what its microVM used in all (D260's final use), read once its machine was down:
/// each vCPU's guest time by its index, none departed apart from them, and its VM process's
/// CPU time, which holds theirs; its stats after the end are refused with that end.
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
    let guest = booted(&ImageShards::new(), &s).unwrap();
    let ended = ask(&*guest, |g| g.wait_for(Duration::MAX)).unwrap().unwrap();
    assert_eq!(ended.end, RunEnd::Exited(3), "{ended:?}");
    let FinalUse::Read(used) = &ended.final_use else {
        panic!("no final use with its end: {ended:?}");
    };
    let times = used.guest.as_ref().unwrap();
    let host = used.host.as_ref().unwrap();
    let indices: Vec<u32> = times.vcpus.iter().map(|v| v.index).collect();
    assert_eq!(indices, (0..s.vcpus).collect::<Vec<_>>(), "{used:?}");
    assert_eq!(times.departed_ns, 0, "{used:?}");
    assert!(times.vcpus[0].guest_ns > 0, "its boot vCPU ran nothing: {used:?}");
    assert!(host.cpu_ns >= times.total_ns(), "{used:?}");
    assert_eq!(used.vmm_ns(), Some(host.cpu_ns - times.total_ns()));
    assert_eq!(
        guest.stats(StatsScope::Run, Duration::ZERO),
        Err(ImageRefusal::Ended(ended.clone()))
    );
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
        let guest = booted(&ImageShards::new(), s).unwrap();
        let from = guest.started_from();
        assert_eq!(
            ask(&*guest, |g| g.wait_for(Duration::MAX))
                .unwrap()
                .map(|e| e.end),
            Some(RunEnd::Exited(3))
        );
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

/// kiln boots every harness at shards-api's least envelope (`envelope(ARCH, 0, 0)`: 1 vCPU and,
/// on arm64, 64 MiB) and leaves its workload's memory to D148's plug (D274). At that envelope
/// the image's first run saves its template, which holds memory its guest plugged before the
/// save, and the next restores it; each run's own command holds 64 MiB, which its microVM's
/// boot memory has no room for beside its kernel, on that plugged memory, and ends 0.
#[test]
fn the_least_envelope_saves_and_restores_its_template_and_grows_past_its_boot_memory() {
    if cannot_run_vms() {
        return;
    }
    let home = common::home_with("api-least-home");
    build(&home, "api-least:1", &["exit", "0"], "");
    let (vcpus, memory_mib) = shards_api::envelope(std::env::consts::ARCH, 0, 0).unwrap();
    let mut s = spec(&home, "api-least:1");
    (s.vcpus, s.memory_mib) = (vcpus, memory_mib);
    s.command = Some(["/bin/testguest", "alloc", "64"].map(String::from).to_vec());
    // The guest its home recorded, as kiln's runs boot Shards' own: a guest its caller gives
    // saves no template.
    s.env
        .retain(|(name, _)| name != "SHARDS_KERNEL" && name != "SHARDS_INIT");
    let restores = shards_vmm::vm::restores_here();
    for from in [StartedFrom::Saved, StartedFrom::Restored] {
        let guest = boot(&home, &s);
        let expected = if restores { from } else { StartedFrom::Booted };
        assert_eq!(guest.started_from(), expected);
        let ended = ask(&*guest, |g| g.wait_for(Duration::MAX)).unwrap();
        assert_eq!(ended.map(|e| e.end), Some(RunEnd::Exited(0)));
    }
    if restores {
        let templates: Vec<_> = std::fs::read_dir(home.join("templates"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.is_dir())
            .collect();
        assert_eq!(templates.len(), 1, "{templates:?}");
        let (_, plugged) = shards_vmm::snapshot::layout(&templates[0]).unwrap();
        // arm64's least envelope, 64 MiB, has no room for its guest's boot: its guest plugs
        // before its template point (PM M604), and that memory is what D274 gives back as RAM.
        // x86-64's, 128 MiB, holds the boot (KVM, PM M605): its template holds nothing plugged,
        // and each run's 64 MiB is plugged after its restore.
        if cfg!(target_arch = "aarch64") {
            assert!(!plugged.is_empty(), "its guest plugged nothing before its save");
        }
    }
}

/// D233 (Hex goal 39): a GPU run's renderer report, its VM process's own, reaches its
/// orchestrator as a managed launch's does. Given a host GPU by its identity in D232's grammar
/// (`SHARDS_TEST_GPU`: `pci:DOMAIN:BUS:DEVICE.FUNCTION`, `metal:HEX` or `luid:HEX`; skipped
/// without one), the image's first run boots and saves its template and its next restores it
/// (where this host restores), each run's report of its own scope naming exactly that GPU, on
/// Windows with its current boot's LUID, and taken once; a GPU no host has is refused as its
/// renderer typed it.
#[test]
fn a_gpu_runs_renderer_report_reaches_its_orchestrator() {
    use shards_api::{GpuProbeError, PhysicalGpu};
    if cannot_run_vms() {
        return;
    }
    let Ok(named) = std::env::var("SHARDS_TEST_GPU") else {
        let _ = writeln!(std::io::stderr(), "SKIP: SHARDS_TEST_GPU names no host GPU");
        return;
    };
    let gpu: PhysicalGpu = named.parse().unwrap();
    let home = common::home_with("api-gpu-home");
    build_example(&home, "api-gpu:1");
    let mut s = spec(&home, "api-gpu:1");
    s.env
        .retain(|(name, _)| name != "SHARDS_KERNEL" && name != "SHARDS_INIT");
    s.command = Some(["/bin/sh", "-c", "exit 0"].map(String::from).to_vec());
    s.gpu_devices = vec![gpu];
    let mut froms = Vec::new();
    for scope in [[3u8; 16], [4u8; 16]] {
        s.gpu_report = Some(scope);
        let guest = booted(&ImageShards::new(), &s).unwrap();
        let report = guest.take_gpu_report(Duration::MAX).unwrap();
        assert_eq!(report.scope, scope);
        let physical: Vec<PhysicalGpu> = report.probe.devices.iter().map(|d| d.physical).collect();
        assert_eq!(physical, [gpu], "{report:?}");
        #[cfg(windows)]
        assert!(report.probe.devices[0].luid.is_some(), "{report:?}");
        assert_eq!(
            guest.take_gpu_report(Duration::MAX),
            Err(ImageRefusal::GpuProbe(GpuProbeError::ReportTaken))
        );
        froms.push(guest.started_from());
        assert_eq!(
            ask(&*guest, |g| g.wait_for(Duration::MAX))
                .unwrap()
                .map(|e| e.end),
            Some(RunEnd::Exited(0))
        );
    }
    if shards_vmm::vm::restores_here() {
        assert_eq!(froms, [StartedFrom::Saved, StartedFrom::Restored]);
    }
    // An identity of this host's kind that no GPU of it has.
    #[cfg(target_os = "macos")]
    let absent = PhysicalGpu::MetalRegistry(u64::MAX);
    #[cfg(windows)]
    let absent = PhysicalGpu::Luid(u64::MAX);
    #[cfg(not(any(target_os = "macos", windows)))]
    let absent = PhysicalGpu::Pci {
        domain: 0xffff,
        bus: 0xff,
        device: 0x1f,
        function: 7,
    };
    s.gpu_devices = vec![absent];
    s.gpu_report = Some([5; 16]);
    let refused = ImageShards::new().boot(&s, Duration::MAX).err();
    assert!(
        matches!(refused, Some(ImageRefusal::GpuProbe(GpuProbeError::Refused(_)))),
        "{refused:?}"
    );
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
    let pools = ImagePools::new(RoomOfTwo::default(), ImagePools::KEEP).unwrap();
    let run = |s: &ImageSpec| {
        let guest = booted(&pools, s).unwrap();
        let from = guest.started_from();
        assert_eq!(
            ask(&*guest, |g| g.wait_for(Duration::MAX))
                .unwrap()
                .map(|e| e.end),
            Some(RunEnd::Exited(3))
        );
        from
    };
    assert_ne!(
        run(&s),
        StartedFrom::Warm,
        "nothing is warm before a pool's first run"
    );
    // The warm host its first run's pool started is taken by the first run that comes once
    // it is ready: each run before that starts a host of its own. Asked while the pool's
    // hosts go on (`Patience` of what this test started).
    let mut patience = Patience::started();
    while run(&s) != StartedFrom::Warm {
        assert!(patience.holds(), "no run took a warm host");
    }
    patience.done();
    let other = ImageSpec {
        vcpus: 2,
        ..s.clone()
    };
    assert_ne!(run(&other), StartedFrom::Warm, "another microVM's first run");
}

/// The orchestrator's pools the measured warm starts come through: one construction, which
/// changes as `ImagePools::new` does.
fn warm_pools() -> ImagePools {
    ImagePools::new(RoomOfTwo::default(), ImagePools::KEEP).unwrap()
}

/// One measured start of an image's run: how it started, and its three spans from the boot's
/// call: its answer (`boot`, as Hex's `hex.image.boot` is bounded: `boot_stoppable` to its
/// answer, the run host's diagnostics kept), its harness's first output read (`usable`), and
/// its stop, from its call to its answer (`stop`). `diagnostics` is where its run host's were
/// kept; `called_unix_us` is the wall clock as the boot was called, in microseconds since the
/// Unix epoch, which its run host's and VM's timed lines are lined up with.
struct Start {
    from: StartedFrom,
    called_unix_us: u128,
    boot: Duration,
    usable: Duration,
    stop: Duration,
    diagnostics: std::path::PathBuf,
}

/// `s` started through `images`, its harness's first line read (the test guest's `sleep`
/// prints `ready`, then waits for its end), then stopped: each span timed ([`Start`]).
#[track_caller]
fn timed_start(images: &dyn shards_api::image::Images, home: &TempDir, s: &ImageSpec) -> Start {
    // Made before the boot is asked, as Hex makes its file before its boot's thread starts.
    let path = diagnostics(home);
    let kept = std::fs::File::create(&path).unwrap();
    let stop = BootStop::new();
    let (guest, t0, called_unix_us, boot) = common::judged(
        Party::started(None),
        "a measured boot",
        || stop.stop(),
        || {
            let t0 = Instant::now();
            let called_unix_us = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_micros();
            let guest = images
                .boot_stoppable(s, Duration::MAX, &stop)
                .and_then(|g| g.keep_diagnostics(kept, u64::MAX).map(|()| g));
            (guest, t0, called_unix_us, t0.elapsed())
        },
    )
    .unwrap_or_else(|refused| panic!("{refused}"));
    let guest = guest.unwrap_or_else(|refused| {
        panic!(
            "the boot: {refused}; its run host said: {}",
            std::fs::read_to_string(&path).unwrap_or_default()
        )
    });
    let from = guest.started_from();
    let mut out = guest.take_output(&codex(), 0, Channel::Stdout).unwrap();
    let (line, usable) = common::judged(
        Party::started(None),
        "a measured start's first output",
        || {
            let _ = guest.stop();
        },
        move || {
            let mut line = [0u8; 6];
            out.read_exact(&mut line).map(|()| (line, t0.elapsed()))
        },
    )
    .unwrap_or_else(|refused| panic!("{refused}"))
    .unwrap_or_else(|e| {
        panic!(
            "its harness's first output: {e}; the run's end: {:?}",
            guest.wait_for(Duration::ZERO)
        )
    });
    assert_eq!(&line, b"ready\n");
    let (stopped, stop) = ask(&*guest, |g| {
        let t = Instant::now();
        (g.stop(), t.elapsed())
    });
    stopped.unwrap();
    Start {
        from,
        called_unix_us,
        boot,
        usable,
        stop,
        diagnostics: path,
    }
}

/// The ways an image's run starts (D253), each asked of [`image_start_costs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartKind {
    Booted,
    Saved,
    Restored,
    Warm,
}

impl StartKind {
    const ALL: [StartKind; 4] = [
        StartKind::Booted,
        StartKind::Saved,
        StartKind::Restored,
        StartKind::Warm,
    ];

    fn started_from(self) -> StartedFrom {
        match self {
            StartKind::Booted => StartedFrom::Booted,
            StartKind::Saved => StartedFrom::Saved,
            StartKind::Restored => StartedFrom::Restored,
            StartKind::Warm => StartedFrom::Warm,
        }
    }
}

/// The homes and specs [`image_start_costs`] starts each kind with: one home whose guest is
/// recorded (D28), its template saved once and then restored, by `ImageShards` (`Restored`)
/// and by a pool's warm hosts (`Warm`), and a run of a guest given (`Booted`); and another,
/// whose templates are removed before each start, which then saves one (`Saved`). The image
/// is the tests' own, its harness the test guest's `sleep`.
struct CostRig {
    home: TempDir,
    saved_home: TempDir,
    booted: ImageSpec,
    saved: ImageSpec,
    restored: ImageSpec,
    pools: ImagePools,
}

impl CostRig {
    fn new() -> CostRig {
        let home = common::home_with("api-cost-home");
        let saved_home = common::home_with("api-cost-saved");
        let tag = "api-cost:1";
        build(&home, tag, &["sleep"], "");
        build(&saved_home, tag, &["sleep"], "");
        let stored = |mut s: ImageSpec| {
            s.env
                .retain(|(name, _)| name != "SHARDS_KERNEL" && name != "SHARDS_INIT");
            s
        };
        CostRig {
            booted: spec(&home, tag),
            saved: stored(spec(&saved_home, tag)),
            restored: stored(spec(&home, tag)),
            pools: warm_pools(),
            home,
            saved_home,
        }
    }

    /// Its run hosts say each step with its time from now on (`SHARDS_TIMING`), and shards-api
    /// its own, each process's clock said on the wall's; where `debug`, their VMs log at debug
    /// too (`SHARDS_LOG`), each step of a VM's start said with its time. A warm host's pool is
    /// then another (its environment).
    fn traced(&mut self, debug: bool) {
        for s in [&mut self.booted, &mut self.saved, &mut self.restored] {
            s.env
                .retain(|(name, _)| name != "SHARDS_LOG" && name != "SHARDS_TIMING");
            s.env.push(("SHARDS_TIMING".into(), "1".into()));
            if debug {
                s.env.push(("SHARDS_LOG".into(), "debug".into()));
            }
        }
    }

    /// A start of `kind`, held to it: one that started otherwise fails the measurement.
    #[track_caller]
    fn start(&self, kind: StartKind) -> Start {
        let started = match kind {
            StartKind::Booted => timed_start(&ImageShards::new(), &self.home, &self.booted),
            StartKind::Saved => {
                let templates = self.saved_home.join("templates");
                if templates.exists() {
                    for entry in std::fs::read_dir(&templates).unwrap() {
                        let path = entry.unwrap().path();
                        if path.is_dir() {
                            std::fs::remove_dir_all(&path).unwrap();
                        } else {
                            std::fs::remove_file(&path).unwrap();
                        }
                    }
                }
                timed_start(&ImageShards::new(), &self.saved_home, &self.saved)
            }
            StartKind::Restored => timed_start(&ImageShards::new(), &self.home, &self.restored),
            StartKind::Warm => timed_start(&self.pools, &self.home, &self.restored),
        };
        assert_eq!(
            started.from,
            kind.started_from(),
            "a start asked as {kind:?} started {:?}{}; its run host said: {}",
            started.from,
            if kind == StartKind::Warm {
                " (its pool had no warm host ready)"
            } else {
                ""
            },
            std::fs::read_to_string(&started.diagnostics).unwrap_or_default()
        );
        started
    }

    /// What comes before the measured starts: the image's files read once by a start of
    /// each way, its template saved in the restoring home, and its pool warm.
    fn warm_up(&self) {
        self.start(StartKind::Booted);
        self.start(StartKind::Saved);
        let first = timed_start(&ImageShards::new(), &self.home, &self.restored);
        assert!(
            matches!(first.from, StartedFrom::Saved | StartedFrom::Restored),
            "{:?}",
            first.from
        );
        self.start(StartKind::Restored);
        // A start through the pools makes its pool; a start is tried only once the pool says a
        // host of it is warm, so no start races its pool's refill.
        let mut patience = Patience::started();
        timed_start(&self.pools, &self.home, &self.restored);
        loop {
            if self.pools.held().iter().any(|p| p.warm > 0)
                && timed_start(&self.pools, &self.home, &self.restored).from == StartedFrom::Warm
            {
                break;
            }
            assert!(patience.holds(), "no start took a warm host");
            std::thread::yield_now();
        }
        patience.done();
    }
}

/// This host's OS and its release, as the measurement's line names it.
fn os_release() -> String {
    #[cfg(unix)]
    let out = std::process::Command::new("uname").arg("-sr").output();
    #[cfg(windows)]
    let out = std::process::Command::new("cmd").args(["/c", "ver"]).output();
    out.ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::env::consts::OS.to_string())
}

/// Every timed line of a start's diagnostics on the caller's clock, in order: microseconds
/// from the boot's call (`called_unix_us`, the wall clock then), its run host's lines
/// (`shards api-run: +US us TEXT`, microseconds since its `main`) and each VM process's
/// (`[PID US LEVEL] TEXT`, microseconds since its log began), each lined up by the wall clock
/// its process said its clock began at (`... began at unix US us`, `SHARDS_TIMING`); and
/// shards-api's own steps on this side of the channel (`shards-api: at unix US us TEXT`, the
/// wall clock itself), said `caller`. A warm host's lines from before the call come first,
/// negative. Lines of a process whose clock was not said are none of it.
fn timeline(diagnostics: &str, called_unix_us: u128) -> Vec<(i128, String)> {
    let began = |text: &str| -> Option<i128> {
        text.split_once("began at unix ")?
            .1
            .strip_suffix(" us")?
            .parse()
            .ok()
    };
    let called = i128::try_from(called_unix_us).unwrap();
    let mut host: Option<i128> = None;
    let mut vms: std::collections::HashMap<u32, i128> = std::collections::HashMap::new();
    let mut lines = Vec::new();
    for line in diagnostics.lines() {
        // shards-api's own steps (session.rs `Shared::step`), each on the wall clock.
        if let Some(said) = line.strip_prefix("shards-api: at unix ") {
            if let Some((us, text)) = said.split_once(" us ")
                && let Ok(us) = us.parse::<i128>()
            {
                lines.push((us - called, format!("caller    {text}")));
            }
            continue;
        }
        if let Some(said) = line.strip_prefix("shards api-run: +") {
            let Some((us, text)) = said.split_once(" us ") else {
                continue;
            };
            let Ok(us) = us.parse::<i128>() else { continue };
            host = host.or_else(|| began(text));
            if let Some(began) = host {
                lines.push((began + us - called, format!("run host  {text}")));
            }
            continue;
        }
        let Some((head, text)) = line.strip_prefix('[').and_then(|l| l.split_once("] ")) else {
            continue;
        };
        let mut fields = head.split_whitespace();
        let (Some(Ok(pid)), Some(Some(Ok(us)))) = (
            fields.next().map(str::parse::<u32>),
            fields
                .next()
                .map(|us| us.strip_suffix("us").map(str::parse::<i128>)),
        ) else {
            continue;
        };
        if let Some(b) = began(text) {
            vms.entry(pid).or_insert(b);
        }
        if let Some(began) = vms.get(&pid) {
            lines.push((began + us - called, format!("vm {pid:<6} {text}")));
        }
    }
    // Stable: lines of one instant keep their order in the diagnostics.
    lines.sort_by_key(|(at, _)| *at);
    lines
}

/// Each process's timed lines land on the caller's clock: a run host's by the clock it said, a
/// VM's by its log's, shards-api's by the wall clock each of its lines says; a process that said
/// no clock gives none, and the lines come in order.
#[test]
fn a_starts_timeline_lines_every_process_up_on_the_callers_clock() {
    let said = "\
shards api-run: +5 us its clock began at unix 1000 us
shards api-run: +40 us its request read
[77 3us DEBUG] its log began at unix 1100 us
[77 10us INFO] guest marker 2 at 3 us
shards-api: at unix 1060 us its start taken
[88 5us DEBUG] a line of a VM that said no clock
shards api-run: not timed
";
    assert_eq!(
        timeline(said, 1000),
        [
            (5, "run host  its clock began at unix 1000 us".to_string()),
            (40, "run host  its request read".to_string()),
            (60, "caller    its start taken".to_string()),
            (103, "vm 77     its log began at unix 1100 us".to_string()),
            (110, "vm 77     guest marker 2 at 3 us".to_string()),
        ]
    );
}

/// Where a shards-api image start's time goes, for each way it starts (D253): from the boot's
/// call to its answer (Hex's `hex.image.boot`: `boot_stoppable` and the diagnostics' keep), to
/// its harness's first output, and its stop, `SHARDS_IMAGE_COST_N` starts of each, the kinds
/// taken round-robin, each round starting one kind later, so that the host's drift and a
/// pool's refill after its warm start fall on every kind alike (PM M29). Each start's kind is
/// held to the one asked. Then two passes of one start of each kind, every timed line on the
/// caller's clock, from the boot's call, in order: `timed`, its run host's and shards-api's
/// steps alone (`SHARDS_TIMING`): its run host's spawn, preparation and launches, its grants
/// answered, the way from `READY` through `TAKEN` and `STARTED`, shards-api's own steps to its
/// answer, and its first output; then `debug`, its VM's log at debug too (`SHARDS_LOG`), which
/// slows it: its VM's spawn, exec and App Sandbox entry to its `main`, its own steps and its
/// guest's markers. Its VM's lines share one pipe with its run host's, so where its reader
/// lags a write waits on it, and the line after shows the wait: `timed`'s gaps are its run
/// host's own.
#[test]
#[ignore = "a measurement: SHARDS_IMAGE_COST_N starts of each kind; ROOT runs it"]
fn image_start_costs() {
    if cannot_run_vms() {
        return;
    }
    let n: usize = std::env::var("SHARDS_IMAGE_COST_N")
        .expect("SHARDS_IMAGE_COST_N names the starts of each kind")
        .parse()
        .expect("SHARDS_IMAGE_COST_N is a count");
    let kinds: &[StartKind] = if shards_vmm::vm::restores_here() {
        &StartKind::ALL
    } else {
        let _ = writeln!(
            std::io::stderr(),
            "SKIP: Saved, Restored and Warm: this host's hypervisor restores no snapshot (PM M345)"
        );
        &StartKind::ALL[..1]
    };
    let host = format!(
        "backend {}, {}",
        shards_vmm::hv::BACKEND.unwrap_or("none"),
        os_release()
    );
    let mut rig = CostRig::new();
    rig.warm_up();
    let mut times: Vec<[Vec<Duration>; 3]> = kinds.iter().map(|_| Default::default()).collect();
    for round in 0..n {
        for i in 0..kinds.len() {
            let at = (round + i) % kinds.len();
            let kind = kinds[at];
            let s = rig.start(kind);
            let _ = writeln!(
                std::io::stdout(),
                "IMAGE-START-SAMPLE round {round} {kind:?} from {:?} boot_us {} usable_us {} stop_us {}",
                s.from,
                s.boot.as_micros(),
                s.usable.as_micros(),
                s.stop.as_micros()
            );
            times[at][0].push(s.boot);
            times[at][1].push(s.usable);
            times[at][2].push(s.stop);
        }
    }
    for (kind, [boot, usable, stop]) in kinds.iter().zip(&times) {
        common::measured(
            &format!("IMAGE START {kind:?} boot, hex.image.boot's span ({host})"),
            boot,
        );
        common::measured(&format!("IMAGE START {kind:?} to first output ({host})"), usable);
        common::measured(&format!("IMAGE START {kind:?} stop ({host})"), stop);
    }
    // One start of each kind with its steps timed, apart from the measured starts: its run
    // host's and shards-api's lines alone, then its VM's at debug too.
    for (pass, debug) in [("timed", false), ("debug", true)] {
        rig.traced(debug);
        // The room holds two warm hosts: the last pool's give theirs back for this one's.
        rig.pools.release(u64::MAX).unwrap();
        rig.warm_up();
        for &kind in kinds {
            let s = rig.start(kind);
            let said = std::fs::read_to_string(&s.diagnostics).unwrap();
            let _ = writeln!(
                std::io::stdout(),
                "IMAGE-START-PHASES {pass} {kind:?} from {:?}: boot {} us, first output {} us, stop {} us; every timed line in us from the boot's call:",
                s.from,
                s.boot.as_micros(),
                s.usable.as_micros(),
                s.stop.as_micros()
            );
            let mut lines = timeline(&said, s.called_unix_us);
            let caller = |d: Duration, what: &str| {
                (
                    i128::try_from(d.as_micros()).unwrap(),
                    format!("caller    {what}"),
                )
            };
            lines.push(caller(s.boot, "the boot answered"));
            lines.push(caller(s.usable, "its harness's first output read"));
            lines.sort_by_key(|(at, _)| *at);
            let mut before: Option<i128> = None;
            for (at, text) in lines {
                let gap = before.map_or(String::new(), |b| format!("+{}", at - b));
                before = Some(at);
                let _ = writeln!(
                    std::io::stdout(),
                    "IMAGE-START-PHASES {pass} {kind:?} {at:>9} {gap:>9}  {text}"
                );
            }
        }
    }
}

/// Stop ends the run at once, whatever its members do, and its end says so.
#[test]
fn stop_ends_the_run_at_once() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-stop-home");
    build(&home, "api-stop:1", &["sleep-for", "600000"], "");
    let guest = booted(&ImageShards::new(), &spec(&home, "api-stop:1")).unwrap();
    assert_eq!(guest.wait_member(&codex(), 0, Duration::ZERO).unwrap(), None);
    guest.stop().unwrap();
    // Killed at once: what its microVM used went with it, typed, never a zero (D260).
    assert_eq!(
        guest.wait_for(Duration::ZERO).unwrap(),
        Some(RunEnded::unused(RunEnd::Stopped, Unused::Killed))
    );
    assert_eq!(
        guest.wait_member(&codex(), 0, Duration::ZERO).unwrap(),
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
        shards_testdir::program(
            &dir.join("shards-vm"),
            &format!(
                "#!/bin/sh\necho \"$$\" >'{seen}'\nexec 3<'{shut}'\nexec '{vm}' \"$@\"\n",
                seen = dir.join("seen").display(),
                shut = dir.join("shut").display(),
                vm = common::shards_vm().display()
            ),
        )
        .unwrap();
        Gate { dir, told }
    }

    #[cfg(windows)]
    fn new() -> Gate {
        use std::os::windows::io::FromRawHandle as _;
        use windows_sys::Win32::System::Threading::CreateEventW;
        let dir = TempDir::new("api-gate");
        std::fs::copy(guest_init(), dir.join("init")).unwrap();
        let held = common::Oplocked::new(&dir.join("init"));
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
            // No patience of its own: the test's stop below ends it, whatever is seen.
            let booted = ImageShards::new()
                .boot_stoppable(&s, Duration::MAX, &stop)
                .map(drop);
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
    assert_eq!(
        booted,
        Err(ImageRefusal::Ended(RunEnded::unused(
            RunEnd::Stopped,
            Unused::NotStarted
        )))
    );
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
    let mut writer = guest.take_input(&codex(), 0).unwrap();
    assert!(guest.take_input(&codex(), 0).is_err(), "taken once");
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
                ask(&*guest, |g| g.wait_member(&codex(), 0, Duration::MAX)),
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
    let guest = booted(&ImageShards::new(), &spec(&home, "api-nostdin:1")).unwrap();
    assert!(guest.take_input(&codex(), 0).is_err());
    guest.stop().unwrap();
}

/// D256: a member's run-time arguments follow its image's command on its command line, each
/// whole and in its order. The harness, the test guest writing its command line word by word,
/// each with its NUL, says its program as its image's command names it, its image's options, then its
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
    // Its program as its image's command names it: the member's own search finds its file
    // (confine-04, `run::exec_found`) and its argv[0] stays as given, as runc's execve(2)
    // passes a container's command.
    assert_eq!(
        program,
        b"codex",
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
    assert_eq!(
        ask(&*guest, |g| g.wait_for(Duration::MAX))
            .unwrap()
            .map(|e| e.end),
        Some(RunEnd::Stopped)
    );
    let refused = |s: ImageSpec| match booted(&ImageShards::new(), &s) {
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

/// What `call` on a run's agent-control endpoint gives (a lane's answer, a member's end, a read
/// of a member's channel), judged as a call of its run's is (`common::judged`, as [`ask`]): its
/// party what this test's process started. Refused, the run is stopped, which ends its
/// endpoint and every lane with it.
#[track_caller]
fn on_endpoint<T: Send>(guest: &dyn ImageGuest, what: &str, call: impl FnOnce() -> T + Send) -> T {
    common::judged(
        Party::started(None),
        what,
        || {
            let _ = guest.stop();
        },
        call,
    )
    .unwrap_or_else(|refused| panic!("{refused}"))
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
/// Each wait on the guest is judged (`common::judged`, D209), the run stopped where one is
/// refused; none is fixed.
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
        stdio: false,
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
    let said = on_endpoint(&*guest, "codex's MCP connection", || {
        // Its end held while its records are read: an ExecControl let go before its exit is
        // heard hangs its connection up (D254, d7ff8a094), which a controller does only
        // when it means to end it.
        let McpConnection { mut from, end, .. } = answered!(execs.mcp(codex_at)).unwrap();
        let mut said = Vec::new();
        while said.len() < shards_testguest::MCP_INITIALIZE.len() {
            match from.next_record().unwrap() {
                Got::Record { bytes, .. } => said.extend(bytes),
                Got::Finished => break,
            }
        }
        drop(end);
        said
    });
    assert_eq!(
        String::from_utf8_lossy(&said),
        String::from_utf8_lossy(shards_testguest::MCP_INITIALIZE)
    );
    // `claude` asleep, as its own word says, before it is signalled.
    let mut asleep = guest.take_output(&claude, 0, Channel::Stdout).unwrap();
    let mut line = [0; 6];
    on_endpoint(&*guest, "claude's ready", || asleep.read_exact(&mut line)).unwrap();
    assert_eq!(&line, b"ready\n");
    let signalled = |at: u32, signal: u32| {
        on_endpoint(&*guest, "a member's signal and its end", || {
            let Member::Running(MemberControl {
                mut steering,
                mut end,
            }) = answered!(execs.member(at)).unwrap()
            else {
                panic!("member {at} runs");
            };
            let sent = steering.signal(signal, Duration::MAX);
            let ended = end.wait();
            let after = steering.signal(signal, Duration::MAX);
            (sent, ended, after)
        })
    };
    let (sent, ended, after) = signalled(codex_at, 15);
    assert_eq!(sent, Ok(Some(())));
    assert_eq!(ended, Ok(exec::End::Signaled(15)), "TERM, never an exit of 143");
    assert_eq!(after, Err(Fault::Unsteered(Unsteered::Ended)));
    let (sent, ended, _) = signalled(claude_at, 9);
    assert_eq!(sent, Ok(Some(())));
    assert_eq!(ended, Ok(exec::End::Signaled(9)));
    assert_eq!(
        ask(&*guest, |g| g.wait_member(&codex(), 0, Duration::MAX)).unwrap(),
        Some(MemberEnd::Signaled(15))
    );
    assert_eq!(
        ask(&*guest, |g| g.wait_member(&claude, 0, Duration::MAX)).unwrap(),
        Some(MemberEnd::Signaled(9))
    );
    let again = on_endpoint(&*guest, "an ended member's lane", || {
        answered!(execs.member(codex_at))
    });
    assert!(
        matches!(again, Ok(Member::Ended(exec::End::Signaled(15)))),
        "{again:?}"
    );
    guest.stop().unwrap();
    assert_eq!(
        ask(&*guest, |g| g.wait_for(Duration::MAX))
            .unwrap()
            .map(|e| e.end),
        Some(RunEnd::Stopped)
    );
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
    let mut input = guest.take_input(&codex(), 0).unwrap();
    let mut output = guest.take_output(&codex(), 0, Channel::Stdout).unwrap();
    let mut echo = |line: &[u8]| {
        input.write_all(line).unwrap();
        let mut echoed = vec![0; line.len()];
        output.read_exact(&mut echoed).unwrap();
        assert_eq!(echoed, line);
    };
    echo(b"before\n");
    let outside = TempDir::new("api-save-out");
    match ask(&*guest, |g| g.save(&outside.join("s1"), Duration::MAX)) {
        Err(ImageRefusal::Save(SaveRefusal::Destination, why)) => {
            assert!(why.contains("outside the run's home"), "{why}")
        }
        other => panic!("{other:?}"),
    }
    assert!(!outside.join("s1").exists());
    let saved = home.join("leak-save");
    ask(&*guest, |g| g.save(&saved, Duration::MAX)).unwrap();
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

/// The test's TLS host: its port on this host's loopback, its CA's certificate in PEM, which a
/// run names as its upstream root (D242), and each request it answered, sent as it came, before
/// its answer.
struct TlsHost {
    port: u16,
    root: String,
    asked: std::sync::mpsc::Receiver<Asked>,
}

type ServerTls = rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream>;

/// The test's TLS host, under a CA of its own, for `names`, offering `h2` and `http/1.1`: `/echo`
/// answers the request's credential field in `x-echo` and its body; `/oauth/token` answers a
/// code's exchange with [`REAL_ACCESS`] and [`REAL_REFRESH`], as Codex's provider would, and
/// anything else there 400.
fn tls_host(names: &[&str]) -> TlsHost {
    // A day's validity, within what every platform's verifier takes of a server's certificate
    // (shards-verify's and the injector's tests give theirs the same).
    let now = time::OffsetDateTime::now_utc();
    let day = |p: &mut rcgen::CertificateParams| {
        p.not_before = now - time::Duration::hours(1);
        p.not_after = now + time::Duration::days(1);
    };
    let mut ca = rcgen::CertificateParams::new(Vec::new()).unwrap();
    day(&mut ca);
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
    day(&mut leaf);
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = leaf.signed_by(&key, &ca).unwrap();
    let root = ca.pem();
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
    let port = crate::common::minted_port(listener.local_addr().unwrap().port());
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
    TlsHost { port, root, asked }
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
            until: longest_lease(),
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
/// Its trust bundle, at the first of the paths D242 lays the run's CA over, holds no root of
/// its own: a granted member is refused where its image has none (init's trust.rs), and with
/// none of the image's, the run's CA laid over it is the one root the harness's client trusts.
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
            "COPY <<EOF /etc/ssl/certs/ca-certificates.crt\nno roots of the image's own\nEOF\n\
             NETWORK --egress={host}{token} provider FOR codex\n\
             EXPOSE --harnesses=codex {ports} AS egress FOR provider\nCONNECT codex WITH codex ON provider\n"
        ),
    );
}

/// The spec of a run of `image` whose harness asks `steps`, holding [`PLACEHOLDER`] in
/// `API_KEY`, the test's root its upstream root (D242), its leases `lessor`'s.
fn asking_spec(
    home: &TempDir,
    image: &str,
    host: &TlsHost,
    lessor: &std::sync::Arc<TestLessor>,
    steps: &[&str],
) -> ImageSpec {
    let mut s = spec(home, image);
    s.upstream_roots = vec![UpstreamRoot::Pem(host.root.clone())];
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
#[track_caller]
fn answers(guest: &dyn ImageGuest) -> Vec<String> {
    let out = String::from_utf8(read_all(guest, Channel::Stdout)).unwrap();
    out.lines().map(str::to_string).collect()
}

/// The run stopped, and every lease its lessor gave released by its end (D242).
#[track_caller]
fn stop_releasing(guest: &dyn ImageGuest, lessor: &TestLessor) {
    guest.stop().unwrap();
    assert_eq!(
        ask(guest, |g| g.wait_for(Duration::MAX)).unwrap().map(|e| e.end),
        Some(RunEnd::Stopped)
    );
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
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-cred-h2");
    let host = tls_host(&["host.docker.internal"]);
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
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-cred-503");
    let host = tls_host(&["host.docker.internal"]);
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
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-signin-h2");
    let host = tls_host(&["host.docker.internal", "auth.openai.com"]);
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

/// A room of two warm hosts of the pool tests' size (D253's `WarmRoom`), counted atomically,
/// as a node's room may be: it waits on no lock of its owner's.
#[derive(Debug, Default)]
struct RoomOfTwo(std::sync::atomic::AtomicUsize);

impl shards_api::image::WarmRoom for RoomOfTwo {
    fn claim(&self, _: &shards_api::image::WarmHost) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        self.0
            .fetch_update(SeqCst, SeqCst, |n| (n < 2).then_some(n + 1))
            .is_ok()
    }

    fn give(&self, _: &shards_api::image::WarmHost, _: shards_api::image::Gave) {
        use std::sync::atomic::Ordering::SeqCst;
        let _ = self.0.fetch_update(SeqCst, SeqCst, |n| n.checked_sub(1));
    }
}

/// The pool test's spec in `home`, its run host `binary`: a command that exits 3, granted
/// nothing past its microVM, booting this build's pinned guest.
fn pooled_spec(home: &std::path::Path, image: &str, binary: &std::path::Path) -> ImageSpec {
    ImageSpec {
        image: image.into(),
        vcpus: 1,
        memory_mib: 512,
        command: Some(["/bin/sh", "-c", "exit 3"].map(String::from).to_vec()),
        members: Vec::new(),
        publish: Vec::new(),
        egress: Narrowing {
            egress: Keep::Only(Vec::new()),
            mcp: Keep::Only(Vec::new()),
            destinations: Keep::Only(Vec::new()),
            dns: false,
            members: Some(Vec::new()),
        },
        credentials: Vec::new(),
        lessor: None,
        shares: Vec::new(),
        nesting: false,
        ready_by: None,
        binary: Some(binary.to_path_buf()),
        env: vec![
            ("SHARDS_HOME".into(), home.as_os_str().into()),
            ("SHARDS_LOCAL_STORE".into(), "none".into()),
            ("DOCKER_CONFIG".into(), common::docker_config().as_os_str().into()),
        ],
        control: None,
        gpu_devices: Vec::new(),
        sign_ins: Vec::new(),
        upstream_roots: Vec::new(),
        plug_roots: Vec::new(),
        egress_ceiling: Vec::new(),
        gpu_report: None,
    }
}

/// D253's pools end with their holder, by whatever death: a warm run host reads its
/// orchestrator's channel for its run (`CREATE`), and its end, as the holder's process goes,
/// ends it and its microVM (api_run.rs `warm`: none read, `end`). The holder, this test run
/// again (`SHARDS_TEST_POOL_HOLDER`), keeps a pool with a warm host, its run host a copy of
/// `shards` of its own, which names its hosts; SIGKILLed, every host's process group is gone.
#[cfg(unix)]
#[test]
fn warm_hosts_end_with_the_process_holding_their_pools() {
    if let Some(home) = std::env::var_os("SHARDS_TEST_POOL_HOLDER") {
        let binary = std::env::var_os("SHARDS_TEST_POOL_BINARY").unwrap();
        let s = pooled_spec(home.as_ref(), "api-orphan:1", binary.as_ref());
        let pools = ImagePools::new(RoomOfTwo::default(), ImagePools::KEEP).unwrap();
        let guest = booted(&pools, &s).unwrap();
        ask(&*guest, |g| g.wait_for(Duration::MAX)).unwrap();
        drop(guest);
        let mut patience = Patience::started();
        while pools.held().iter().map(|h| h.warm).sum::<usize>() == 0 {
            assert!(patience.holds(), "no host warm");
            std::thread::sleep(patience.next_look());
        }
        patience.done();
        let _ = writeln!(std::io::stdout(), "warm");
        let _ = std::io::stdout().flush();
        // Held until killed.
        let _ = std::io::stdin().read(&mut [0]);
        std::process::abort();
    }
    if cannot_run_vms() {
        return;
    }
    let home = common::home_with("api-orphan-home");
    build_example(&home, "api-orphan:1");
    let named = TempDir::new("orphan");
    let binary = named.join("shards-pool-holder");
    std::fs::copy(common::shards(), &binary).unwrap();
    let mut holder = std::process::Command::new(std::env::current_exe().unwrap());
    holder
        .args([
            "warm_hosts_end_with_the_process_holding_their_pools",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("SHARDS_TEST_POOL_HOLDER", home.as_os_str())
        .env("SHARDS_TEST_POOL_BINARY", &binary)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    common::as_tests_run(&mut holder);
    let mut holder = holder.spawn().unwrap();
    let mut out = holder.stdout.take().unwrap();
    let mut said = Vec::new();
    let mut byte = [0];
    while !said.ends_with(b"warm\n") {
        assert_eq!(
            out.read(&mut byte).unwrap(),
            1,
            "the holder ended: {}",
            String::from_utf8_lossy(&said)
        );
        said.push(byte[0]);
    }
    // Listed from /proc on Linux, where Alpine's BusyBox `ps` has no `command` column and
    // listed nothing (CI's musl jobs).
    let named = binary.to_string_lossy().into_owned();
    let hosts: Vec<i32> = common::commands_with(&named)
        .into_iter()
        .filter(|(_, args)| args.contains("api-run"))
        .filter_map(|(pid, _)| i32::try_from(pid).ok())
        .collect();
    assert!(!hosts.is_empty(), "the holder's warm hosts are listed");
    holder.kill().unwrap();
    holder.wait().unwrap();
    // Each host led a process group of its own, its VM's among it (`start_host`): gone whole.
    let gone = |pid: i32| {
        // SAFETY: kill(2) with signal 0 sends none; it only says whether the group exists.
        let sent = unsafe { libc::kill(-pid, 0) };
        sent == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    };
    // Its party is those groups' own processes, the holder's orphans now, none of them this
    // process's: a wait on the processes this process started followed none of them, and was
    // refused 322.5 µs in with host 75603 live (ROOT's batch 32, HVF).
    let groups: Vec<u32> = hosts.iter().filter_map(|&pid| u32::try_from(pid).ok()).collect();
    let mut patience = Patience::groups(&groups);
    while !hosts.iter().all(|&pid| gone(pid)) {
        assert!(
            patience.holds(),
            "warm hosts {hosts:?} outlived their pools' holder, their groups' processes now {:?}",
            common::grouped(&common::processes(), &groups)
                .cloned()
                .collect::<Vec<_>>()
        );
        std::thread::sleep(patience.next_look());
    }
    patience.done();
}

/// D253's pools: a warm host's footprint is its VM process's, as a running run's stats read it
/// (`USE`, answered `USED`): asked as `held()` looks, and listed by a later look, never waited
/// for.
#[test]
fn a_warm_hosts_footprint_is_its_vm_processs_reading() {
    if cannot_run_vms() {
        return;
    }
    let home = common::home_with("api-footprint-home");
    build_example(&home, "api-footprint:1");
    let s = pooled_spec(&home, "api-footprint:1", common::shards());
    let pools = ImagePools::new(RoomOfTwo::default(), ImagePools::KEEP).unwrap();
    let guest = booted(&pools, &s).unwrap();
    ask(&*guest, |g| g.wait_for(Duration::MAX)).unwrap();
    drop(guest);
    let mut patience = Patience::started();
    let footprints = loop {
        let held = pools.held();
        let footprints: Vec<u64> = held.iter().flat_map(|h| h.footprints.iter().copied()).collect();
        let warm: usize = held.iter().map(|h| h.warm).sum();
        if warm > 0 && footprints.len() == warm {
            break footprints;
        }
        assert!(patience.holds(), "no warm host's footprint: {held:?}");
        std::thread::sleep(patience.next_look());
    };
    patience.done();
    assert!(footprints.iter().all(|&f| f > 0), "{footprints:?}");
}

/// The Agentfile lines of an image whose harness takes port 7100 coming in (D122), which links
/// it past its microVM, so that its run has a network with no port published at its start.
const INGRESS: &str = "NETWORK --ingress=7100 front\n\
     EXPOSE --harnesses=codex 7100 AS ingress FOR front\n\
     CONNECT --target-kind=harness codex WITH codex ON front\n";

/// The VM processes of `home`'s runs: those whose command line names the home (their image's
/// root, or their template's directory) and the VM process's own binary.
fn vm_processes(home: &TempDir) -> Vec<u32> {
    let home = home.to_string_lossy().into_owned();
    // The VM binary by its command line, or on Linux by its name: a run host starts it through
    // its own descriptor there (`/proc/PID/fd/N`, helpers.rs `Program::path`), which names no
    // binary, and the binary names itself after its file (shards-ipc `name_after_executable`).
    let mut vms: Vec<u32> = common::commands_with(&home)
        .into_iter()
        .filter(|(pid, args)| {
            args.contains("shards-vm")
                || std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim() == "shards-vm")
        })
        .map(|(pid, _)| pid)
        .collect();
    vms.sort_unstable();
    vms
}

/// Every file beneath `home`'s templates, with its length: a save, or a template made or
/// replaced, changes it (D253).
fn templates_of(home: &TempDir) -> Vec<(std::path::PathBuf, u64)> {
    let mut files = Vec::new();
    let mut dirs = vec![home.join("templates")];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                dirs.push(entry.path());
            } else {
                files.push((entry.path(), meta.len()));
            }
        }
    }
    files.sort();
    files
}

/// D269, P6b.1 AC1: a port published as the run runs answers from the host, reaching the member
/// its image gives it coming in, and once let go of answers no more; all of it in the running
/// microVM in place: its start (`started_from`) and its one VM process the same throughout, its
/// template's files untouched, and its image's next run restoring that template where this host
/// restores. Refused typed: a port let go of twice (`Unknown`), and one the image gives no member
/// on a run with no command of its own (`Widening`).
#[test]
fn a_port_published_live_answers_and_is_gone_after_unpublish() {
    if cannot_run_vms() {
        return;
    }
    let home = common::home_with("api-live-port-home");
    build(&home, "api-live-port:1", &["confined", "listen", "7100"], INGRESS);
    let mut s = spec(&home, "api-live-port:1");
    s.env
        .retain(|(name, _)| name != "SHARDS_KERNEL" && name != "SHARDS_INIT");
    let guest = boot(&home, &s);
    let from = guest.started_from();
    let vms = vm_processes(&home);
    assert_eq!(vms.len(), 1, "{vms:?}");
    let templates = templates_of(&home);
    assert!(guest.published().is_empty(), "{:?}", guest.published());
    let asked = |guest_port| Publish {
        guest_port,
        proto: Proto::Tcp,
        host: Some("127.0.0.1".parse().unwrap()),
        host_port: 0,
    };
    let bound = ask(&*guest, |g| g.publish(asked(7100), Duration::MAX)).unwrap();
    let [bound] = bound.as_slice() else {
        panic!("{bound:?}");
    };
    let [to] = bound.to.as_slice() else {
        panic!("{bound:?}");
    };
    let member = guest.members().iter().find(|m| m.index == to.member).unwrap();
    assert_eq!(
        (member.kind, member.name.as_str(), to.port),
        (MemberKind::Harness, "codex", 7100)
    );
    assert_eq!(guest.published(), vec![bound.clone()]);
    let mut patience = Patience::started();
    loop {
        if let Ok(mut c) = std::net::TcpStream::connect(bound.host) {
            let _ = c.write_all(b"hello\n");
            let mut got = Vec::new();
            let _ = common::Following::of(&c, None).read_to_end(&mut got);
            if !got.is_empty() {
                break;
            }
        }
        assert!(patience.holds(), "no answer on {}", bound.host);
        std::thread::sleep(patience.next_look());
    }
    patience.done();
    ask(&*guest, |g| g.unpublish(Proto::Tcp, bound.host, Duration::MAX)).unwrap();
    assert!(guest.published().is_empty(), "{:?}", guest.published());
    assert!(
        std::net::TcpStream::connect(bound.host).is_err(),
        "{} answered once let go of",
        bound.host
    );
    assert!(matches!(
        ask(&*guest, |g| g.unpublish(Proto::Tcp, bound.host, Duration::MAX)),
        Err(ImageRefusal::Net(NetRefusal::Unknown, _))
    ));
    assert!(matches!(
        ask(&*guest, |g| g.publish(asked(7200), Duration::MAX)),
        Err(ImageRefusal::Net(NetRefusal::Widening, _))
    ));
    // In place: the same start, the same VM process, no template saved or replaced.
    assert_eq!(guest.started_from(), from);
    assert_eq!(vm_processes(&home), vms);
    assert_eq!(templates_of(&home), templates);
    guest.stop().unwrap();
    if shards_vmm::vm::restores_here() {
        assert_eq!(from, StartedFrom::Saved);
        let next = boot(&home, &s);
        assert_eq!(next.started_from(), StartedFrom::Restored);
        next.stop().unwrap();
    }
}

/// The lines a running member writes on `channel`, as they come, read on a thread of their own
/// until its channel ends (its run's end): each sent on the receiver this returns.
fn lines_of(guest: &dyn ImageGuest, channel: Channel) -> std::sync::mpsc::Receiver<String> {
    let pipe = guest.take_output(&codex(), 0, channel).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::BufReader::new(pipe).lines() {
            let Ok(line) = line else { return };
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    rx
}

/// Waits, as long as its run goes on (`Patience` of what this test started), for the member's
/// lines on `rx` to hold each of `wanted`: every line read meanwhile.
#[track_caller]
fn said_all(rx: &std::sync::mpsc::Receiver<String>, wanted: &[String]) -> Vec<String> {
    let mut seen = Vec::new();
    let mut patience = Patience::started();
    while !wanted.iter().all(|w| seen.contains(w)) {
        match rx.recv_timeout(patience.next_look()) {
            Ok(line) => seen.push(line),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        assert!(patience.holds(), "the member said {seen:?}, short of {wanted:?}");
    }
    patience.done();
    seen
}

/// D271, P6b.2: a host directory plugged read-write into a running member is read there and
/// written back; one plugged read-only refuses the member's writes (EROFS); a volume of the
/// home's store, made by its plug, is read and written alike, its files kept in the home; each
/// beneath the plug root the run was granted at its start (`ImageSpec::plug_roots`), and one
/// outside every root refused typed (`Outside`). Unplugging one the member holds busy (its
/// working directory there) is refused typed (`Busy`) and leaves it plugged; the others let go
/// of, and refused unplugged again (`Unknown`). All of it in the running microVM in place: its
/// start (`started_from`) and its one VM process the same throughout, its template's files
/// untouched.
#[test]
fn volumes_plugged_live_are_read_and_written_as_asked_and_a_busy_one_stays() {
    if cannot_run_vms() {
        return;
    }
    let home = common::home_with("api-live-vol-home");
    let files = TempDir::new("api-live-vol-files");
    let mut dirs = Vec::new();
    for (name, text) in [("rw", "rw host"), ("ro", "ro host")] {
        std::fs::create_dir(files.join(name)).unwrap();
        let dir = std::fs::canonicalize(files.join(name)).unwrap();
        std::fs::write(dir.join("from-host"), format!("{text}\n")).unwrap();
        dirs.push(dir);
    }
    let (rw, ro) = (dirs[0].clone(), dirs[1].clone());
    let volume = "api-live-vol";
    build(
        &home,
        "api-live-vol:1",
        &["plugged-volumes", "/session/rw", "/session/ro", "/session/vol"],
        "",
    );
    let mut s = spec(&home, "api-live-vol:1");
    s.env
        .retain(|(name, _)| name != "SHARDS_KERNEL" && name != "SHARDS_INIT");
    s.plug_roots = vec!["/session".into()];
    let guest = boot(&home, &s);
    let from = guest.started_from();
    let vms = vm_processes(&home);
    assert_eq!(vms.len(), 1, "{vms:?}");
    let templates = templates_of(&home);
    let lines = lines_of(&*guest, Channel::Stdout);
    let plug = |source, to, at: &str, read_only| Plug {
        source,
        to,
        at: at.into(),
        read_only,
        exec: true,
    };
    match ask(&*guest, |g| {
        g.plug(
            &plug(
                PlugSource::Host(rw.clone()),
                PlugFor::Every,
                "/elsewhere/rw",
                false,
            ),
            Duration::MAX,
        )
    }) {
        Err(ImageRefusal::Plug(PlugRefusal::Outside, why)) => {
            assert!(why.contains("/session"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    ask(&*guest, |g| {
        g.plug(
            &plug(PlugSource::Host(rw.clone()), PlugFor::Every, "/session/rw", false),
            Duration::MAX,
        )
    })
    .unwrap();
    ask(&*guest, |g| {
        g.plug(
            &plug(
                PlugSource::Host(ro.clone()),
                PlugFor::Members(vec![codex()]),
                "/session/ro",
                true,
            ),
            Duration::MAX,
        )
    })
    .unwrap();
    ask(&*guest, |g| {
        g.plug(
            &plug(
                PlugSource::Volume(volume.into()),
                PlugFor::Every,
                "/session/vol",
                false,
            ),
            Duration::MAX,
        )
    })
    .unwrap();
    let data = home.join("volumes").join(volume).join("_data");
    std::fs::write(data.join("from-host"), "vol host\n").unwrap();
    let wanted: Vec<String> = [
        "/session/rw from-host rw host",
        "/session/rw from-member ok",
        "/session/ro from-host ro host",
        "/session/ro from-member errno 30",
        "/session/vol from-host vol host",
        "/session/vol from-member ok",
        "holding /session/rw",
    ]
    .map(String::from)
    .to_vec();
    said_all(&lines, &wanted);
    assert_eq!(
        std::fs::read_to_string(rw.join("from-member")).unwrap(),
        "member\n"
    );
    assert!(!ro.join("from-member").exists(), "a read-only plug took a write");
    assert_eq!(
        std::fs::read_to_string(data.join("from-member")).unwrap(),
        "member\n"
    );
    match ask(&*guest, |g| g.unplug("/session/rw", Duration::MAX)) {
        Err(ImageRefusal::Plug(PlugRefusal::Busy, why)) => assert!(why.contains("codex"), "{why}"),
        other => panic!("{other:?}"),
    }
    for at in ["/session/ro", "/session/vol"] {
        ask(&*guest, |g| g.unplug(at, Duration::MAX)).unwrap();
        assert!(matches!(
            ask(&*guest, |g| g.unplug(at, Duration::MAX)),
            Err(ImageRefusal::Plug(PlugRefusal::Unknown, _))
        ));
    }
    // In place: the same start, the same VM process, no template saved or replaced.
    assert_eq!(guest.started_from(), from);
    assert_eq!(vm_processes(&home), vms);
    assert_eq!(templates_of(&home), templates);
    guest.stop().unwrap();
}

/// P6.2 (D281): an agent's volume of the home's store, plugged into one session and written
/// there, is the next session's on that machine: what the first session left in it (the host's
/// file, written as it ran, and its member's) is there for the next, whose member reads it. It
/// is listed with the bytes its files take (`shards_api::volumes::volumes`), and cleared with
/// its files (`clear_volume`) once no session holds it: refused `InUse` while one runs, then
/// gone from the list and from the home, and asked again, refused `Unknown`.
#[test]
fn an_agents_volume_outlives_its_session_and_is_listed_and_cleared() {
    if cannot_run_vms() {
        return;
    }
    let home = common::home_with("api-agent-vol-home");
    let volume = "agent-a.cache";
    build(&home, "api-agent-vol:1", &["plugged-volumes", "/session/vol"], "");
    let mut s = spec(&home, "api-agent-vol:1");
    s.env
        .retain(|(name, _)| name != "SHARDS_KERNEL" && name != "SHARDS_INIT");
    s.plug_roots = vec!["/session".into()];
    let plug = Plug {
        source: PlugSource::Volume(volume.into()),
        to: PlugFor::Members(vec![codex()]),
        at: "/session/vol".into(),
        read_only: false,
        exec: true,
    };
    let data = home.join("volumes").join(volume).join("_data");
    let wanted: Vec<String> = [
        "/session/vol from-host first session",
        "/session/vol from-member ok",
        "holding /session/vol",
    ]
    .map(String::from)
    .to_vec();
    // Its first session: the volume made by its plug, the host's file and its member's written.
    let first = boot(&home, &s);
    let lines = lines_of(&*first, Channel::Stdout);
    ask(&*first, |g| g.plug(&plug, Duration::MAX)).unwrap();
    std::fs::write(data.join("from-host"), "first session\n").unwrap();
    said_all(&lines, &wanted);
    first.stop().unwrap();
    drop(first);
    assert_eq!(
        std::fs::read_to_string(data.join("from-member")).unwrap(),
        "member\n"
    );
    // Its next: the same volume, read by its member as the first session left it, and held by
    // it: its clearing refused while it runs, its files kept (D281).
    let next = boot(&home, &s);
    let lines = lines_of(&*next, Channel::Stdout);
    ask(&*next, |g| g.plug(&plug, Duration::MAX)).unwrap();
    said_all(&lines, &wanted);
    assert!(matches!(
        shards_api::volumes::clear_volume(s.binary.as_deref(), &s.env, volume),
        Err(VolumeRefusal::Clear(ClearCause::InUse, _))
    ));
    assert!(data.join("from-member").exists());
    next.stop().unwrap();
    drop(next);
    let binary = s.binary.as_deref();
    let listed = shards_api::volumes::volumes(binary, &s.env).unwrap();
    let found = listed.iter().find(|v| v.name == volume).unwrap();
    let bytes = u64::try_from("first session\n".len() + "member\n".len()).unwrap();
    assert_eq!((found.bytes, found.anonymous), (Some(bytes), false));
    shards_api::volumes::clear_volume(binary, &s.env, volume).unwrap();
    assert!(!data.exists());
    let left = shards_api::volumes::volumes(binary, &s.env).unwrap();
    assert!(left.iter().all(|v| v.name != volume), "{left:?}");
    assert!(matches!(
        shards_api::volumes::clear_volume(binary, &s.env, volume),
        Err(VolumeRefusal::Clear(ClearCause::Unknown, _))
    ));
}

/// D273, P6b.1 AC2: a destination its image grants, reached by the running harness, revoked
/// as the run runs refuses the next connection to it, and every one after; granted back, it is
/// reached again. All of it in the running microVM in place: its start (`started_from`) and its
/// one VM process the same throughout, its template's files untouched. A grant its image does
/// not give is refused typed (`Widening`).
#[test]
fn an_egress_destination_revoked_refuses_the_next_connection_and_granted_back_reaches_again() {
    if cannot_run_vms() {
        return;
    }
    let host = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = host.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for taken in host.incoming() {
            drop(taken);
        }
    });
    let to = format!("host.docker.internal:{port}");
    let home = common::home_with("api-live-egress-home");
    build(
        &home,
        "api-live-egress:1",
        &["reach-each", &to],
        &format!(
            "NETWORK --egress={to} provider FOR codex\n\
             EXPOSE --harnesses=codex {port} AS egress FOR provider\n\
             CONNECT codex WITH codex ON provider\n"
        ),
    );
    let mut s = spec(&home, "api-live-egress:1");
    s.env
        .retain(|(name, _)| name != "SHARDS_KERNEL" && name != "SHARDS_INIT");
    let guest = boot(&home, &s);
    let from = guest.started_from();
    let vms = vm_processes(&home);
    assert_eq!(vms.len(), 1, "{vms:?}");
    let templates = templates_of(&home);
    let lines = lines_of(&*guest, Channel::Stdout);
    said_all(&lines, &["reach ok".to_string()]);
    let destination = EgressGrant::Destination {
        at: to.clone(),
        members: Vec::new(),
    };
    let revoked = ask(&*guest, |g| g.revoke_egress(&destination, Duration::MAX)).unwrap();
    assert!(
        matches!(&revoked.destinations, Keep::Only(kept) if !kept.contains(&to)),
        "{revoked:?}"
    );
    assert_eq!(guest.egress(), revoked);
    // What was said before the revoke went may still come: from its first refusal on, every
    // connection is refused.
    let mut refused = 0;
    let mut patience = Patience::started();
    while refused < 3 {
        match lines.recv_timeout(patience.next_look()) {
            Ok(line) if line.starts_with("reach errno") => refused += 1,
            Ok(line) => assert_eq!(refused, 0, "{line} after a refusal"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => panic!("{e}"),
        }
        assert!(patience.holds(), "no refusal once revoked");
    }
    patience.done();
    let granted = ask(&*guest, |g| g.grant_egress(&destination, Duration::MAX)).unwrap();
    assert!(
        matches!(&granted.destinations, Keep::Only(kept) if kept.contains(&to)),
        "{granted:?}"
    );
    said_all(&lines, &["reach ok".to_string()]);
    assert!(matches!(
        ask(&*guest, |g| g
            .grant_egress(&EgressGrant::Ports("22/tcp".into()), Duration::MAX)),
        Err(ImageRefusal::Net(NetRefusal::Widening, _))
    ));
    // In place: the same start, the same VM process, no template saved or replaced.
    assert_eq!(guest.started_from(), from);
    assert_eq!(vm_processes(&home), vms);
    assert_eq!(templates_of(&home), templates);
    guest.stop().unwrap();
}

/// A line of a member's generation's channel, `reader`, or what it says to its end where no
/// line ends first, read as [`read_within`] reads it: its wait judged, its kind its call site.
#[track_caller]
fn line_within(guest: &dyn ImageGuest, mut reader: BufReader<PipeReader>) -> (BufReader<PipeReader>, String) {
    let stop = || {
        let _ = guest.stop();
    };
    let read = move || {
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        (reader, line)
    };
    common::judged(Party::started(None), "a member's channel", stop, read)
        .unwrap_or_else(|refused| panic!("the member's channel said no line: {refused}"))
}

/// What a member's generation's channel, `reader`, says to its end, read as [`line_within`]
/// reads.
#[track_caller]
fn rest_within(guest: &dyn ImageGuest, mut reader: BufReader<PipeReader>) -> String {
    let stop = || {
        let _ = guest.stop();
    };
    let read = move || {
        let mut rest = String::new();
        let _ = reader.read_to_string(&mut rest);
        rest
    };
    common::judged(Party::started(None), "a member's channel", stop, read)
        .unwrap_or_else(|refused| panic!("the member's channel did not end: {refused}"))
}

/// D270 slice 3b: a member started again in its running microVM by its orchestrator. Its
/// first generation, the test guest's `trap TERM`, says it is ready; its restart's SIGTERM ends
/// it gracefully (`got 15`, exited 0), its end told before its restart answers; its next,
/// generation 1, says it is ready on output of its own and runs until it too is asked to end,
/// by a second restart, to generation 2. The run's VM processes, how it started and its
/// templates stay as they were: no microVM was made or remade.
#[test]
fn a_member_restarted_goes_on_in_its_microvm_with_output_and_end_of_its_own() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-restart-home");
    build(&home, "api-restart:1", &["trap", "TERM"], "");
    let mut s = spec(&home, "api-restart:1");
    s.members = vec![MemberSpec {
        member: codex(),
        env: Vec::new(),
        files: Vec::new(),
        workdir: None,
        stdin: false,
        terminal: false,
        args: Vec::new(),
        mcp: false,
    }];
    let guest = boot(&home, &s);
    let (from, vms, templates) = (guest.started_from(), vm_processes(&home), templates_of(&home));
    let stdout =
        |generation| BufReader::new(guest.take_output(&codex(), generation, Channel::Stdout).unwrap());
    let (first, line) = line_within(&*guest, stdout(0));
    assert_eq!(line, "ready\n", "its first generation waits for its SIGTERM");
    let patience = Duration::from_secs(60);
    let restarted = ask(&*guest, |g| g.restart(&codex(), &[], patience));
    assert_eq!(restarted, Ok(1));
    assert_eq!(
        guest.wait_member(&codex(), 0, Duration::ZERO),
        Ok(Some(MemberEnd::Exited(0))),
        "its first generation's end, told before its restart answered"
    );
    assert_eq!(rest_within(&*guest, first), "got 15\n", "it ended on its SIGTERM");
    let (next, line) = line_within(&*guest, stdout(1));
    assert_eq!(line, "ready\n", "its next generation, on output of its own");
    assert_eq!(
        guest.wait_member(&codex(), 1, Duration::ZERO),
        Ok(None),
        "its next generation runs"
    );
    assert!(
        guest.take_output(&codex(), 2, Channel::Stdout).is_err(),
        "a generation not started has no output"
    );
    let restarted = ask(&*guest, |g| g.restart(&codex(), &[], patience));
    assert_eq!(restarted, Ok(2));
    assert_eq!(
        guest.wait_member(&codex(), 1, Duration::ZERO),
        Ok(Some(MemberEnd::Exited(0)))
    );
    assert_eq!(rest_within(&*guest, next), "got 15\n");
    assert_eq!(guest.started_from(), from);
    assert_eq!(vm_processes(&home), vms);
    assert_eq!(templates_of(&home), templates);
    guest.stop().unwrap();
}

/// D278 b3: a member given, as its run runs, a destination its ceiling allows and its image
/// does not grant reaches it; given its image's grants back, its next connection is refused and
/// every one after; a reach past its ceiling is refused typed (`Widening`), and one of a member
/// that runs nothing (`Image`). Its image links it past its microVM on a network that opens no
/// port out, so it reaches nothing until its orchestrator gives it something: all of it in the
/// running microVM in place, its start (`started_from`), its one VM process and its template's
/// files the same throughout.
#[test]
fn a_members_own_reach_given_and_taken_back_holds_in_its_running_microvm() {
    if cannot_run_vms() {
        return;
    }
    let host = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = host.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for taken in host.incoming() {
            drop(taken);
        }
    });
    let to = format!("host.docker.internal:{port}");
    let home = common::home_with("api-member-egress-home");
    build(
        &home,
        "api-member-egress:1",
        &["reach-each", &to],
        "NETWORK provider FOR codex\nCONNECT codex WITH codex ON provider\n",
    );
    let mut s = spec(&home, "api-member-egress:1");
    s.env
        .retain(|(name, _)| name != "SHARDS_KERNEL" && name != "SHARDS_INIT");
    let reach = Reach {
        destinations: vec![to.clone()],
        ..Reach::default()
    };
    s.egress_ceiling = vec![MemberCeiling {
        kind: MemberKind::Harness,
        name: "codex".into(),
        reach: reach.clone(),
    }];
    let guest = boot(&home, &s);
    let from = guest.started_from();
    let vms = vm_processes(&home);
    assert_eq!(vms.len(), 1, "{vms:?}");
    let templates = templates_of(&home);
    let lines = lines_of(&*guest, Channel::Stdout);
    // Refused until it is given anything: every connection, from the first.
    let refused_from_now = |lines: &std::sync::mpsc::Receiver<String>, after_ok: bool| {
        let mut refused = 0;
        let mut patience = Patience::started();
        while refused < 3 {
            match lines.recv_timeout(patience.next_look()) {
                Ok(line) if line.starts_with("reach errno") => refused += 1,
                Ok(line) => assert!(after_ok && refused == 0, "{line} after a refusal"),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(e) => panic!("{e}"),
            }
            assert!(patience.holds(), "no refusal");
        }
        patience.done();
    };
    refused_from_now(&lines, false);
    ask(&*guest, |g| {
        g.member_egress(&codex(), Some(&reach), Duration::MAX)
    })
    .unwrap();
    said_all(&lines, &["reach ok".to_string()]);
    ask(&*guest, |g| g.member_egress(&codex(), None, Duration::MAX)).unwrap();
    // What was said before its image's grants came back may still come: from its first refusal
    // on, every connection is refused.
    refused_from_now(&lines, true);
    let past = Reach {
        ports: vec!["22/tcp".into()],
        ..Reach::default()
    };
    assert!(matches!(
        ask(&*guest, |g| g.member_egress(&codex(), Some(&past), Duration::MAX)),
        Err(ImageRefusal::Net(NetRefusal::Widening, _))
    ));
    let helper = MemberName::agent("helper");
    assert!(matches!(
        ask(&*guest, |g| g.member_egress(&helper, Some(&reach), Duration::MAX)),
        Err(ImageRefusal::Image(_))
    ));
    // In place: the same start, the same VM process, no template saved or replaced.
    assert_eq!(guest.started_from(), from);
    assert_eq!(vm_processes(&home), vms);
    assert_eq!(templates_of(&home), templates);
    guest.stop().unwrap();
}

/// D278 b3: a member's open TCP connection past its microVM, its reach taken back as its run
/// runs, is reset rather than left to stall: its read ends at once, aborted (ECONNABORTED, 103,
/// as sock_diag's SOCK_DESTROY ends it in its own network namespace), and it says so. The host
/// holds the connection open and writes nothing, so nothing but the reset ends the member's
/// read. The member dials as it starts, before its grant: TCP retransmits its SYN (RFC 6298), so
/// it connects once its grant opens its way.
#[test]
fn a_members_open_connection_is_reset_as_its_reach_is_taken_back() {
    if cannot_run_vms() {
        return;
    }
    let host = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = host.local_addr().unwrap().port();
    let (taken_tx, taken_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for taken in host.incoming().flatten() {
            // Held open and written nothing: the member's read waits on it.
            if taken_tx.send(taken).is_err() {
                break;
            }
        }
    });
    let to = format!("host.docker.internal:{port}");
    let home = common::home_with("api-member-reset-home");
    build(
        &home,
        "api-member-reset:1",
        &["tcp", &to],
        "NETWORK provider FOR codex\nCONNECT codex WITH codex ON provider\n",
    );
    let mut s = spec(&home, "api-member-reset:1");
    s.env
        .retain(|(name, _)| name != "SHARDS_KERNEL" && name != "SHARDS_INIT");
    let reach = Reach {
        destinations: vec![to.clone()],
        ..Reach::default()
    };
    s.egress_ceiling = vec![MemberCeiling {
        kind: MemberKind::Harness,
        name: "codex".into(),
        reach: reach.clone(),
    }];
    let guest = boot(&home, &s);
    let (from, vms, templates) = (guest.started_from(), vm_processes(&home), templates_of(&home));
    let lines = lines_of(&*guest, Channel::Stdout);
    ask(&*guest, |g| {
        g.member_egress(&codex(), Some(&reach), Duration::MAX)
    })
    .unwrap();
    let stop = || {
        let _ = guest.stop();
    };
    let held = common::judged(Party::started(None), "a member's connection", stop, move || {
        taken_rx.recv()
    })
    .unwrap_or_else(|refused| panic!("the member's connection never came: {refused}"))
    .unwrap();
    ask(&*guest, |g| g.member_egress(&codex(), None, Duration::MAX)).unwrap();
    let stop = || {
        let _ = guest.stop();
    };
    let said = common::judged(
        Party::started(None),
        "a member's connection's end",
        stop,
        move || lines.recv(),
    )
    .unwrap_or_else(|refused| panic!("the member's read never ended: {refused}"))
    .unwrap();
    assert!(
        said.starts_with("tcp error") && said.contains("os error 103"),
        "{said}"
    );
    drop(held);
    // In place: the same start, the same VM process, no template saved or replaced.
    assert_eq!(guest.started_from(), from);
    assert_eq!(vm_processes(&home), vms);
    assert_eq!(templates_of(&home), templates);
    guest.stop().unwrap();
}

/// An OSI artifact of `kind` (`agent`, `harness`) built of the test guest (`bin/codex`), a
/// `marker.txt` saying `marker` and its config (`<kind>.json`, its asks `asks`, JSON), tagged
/// `tag` in `home`'s store as `shards build harness` makes one: its manifest's digest (D270
/// slice 4).
#[track_caller]
fn component(home: &TempDir, kind: &str, tag: &str, marker: &str, asks: &str) -> String {
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let dir = TempDir::new("api-component");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(
        dir.join("bin/codex"),
        std::fs::read(common::test_guest()).unwrap(),
    )
    .unwrap();
    // An artifact's files keep their modes on a Unix host; a Windows host's context gives every file 0755.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir.join("bin/codex"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(dir.join("marker.txt"), format!("{marker}\n")).unwrap();
    std::fs::write(
        dir.join(format!("{kind}.json")),
        format!(r#"{{"name":"codex","run":{{"command":["bin/codex"]}},"asks":{asks}}}"#),
    )
    .unwrap();
    let built = run_shards_env(&["build", kind], &[dir.to_str().unwrap(), "-t", tag], &env);
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let shown = run_shards_env(&["inspect", kind], &[tag], &env);
    assert_eq!(shown.status, Some(0), "{}", shown.stderr);
    let said: serde_json::Value = serde_json::from_str(&shown.stdout).unwrap();
    said[0]["Digest"].as_str().unwrap().to_string()
}

/// The image of the swap tests (D270 slice 4): its harness the test guest saying what its
/// directory's `marker.txt` says, `none` in the image's own, then trapping SIGTERM; booted with
/// its harness alone.
#[track_caller]
fn swap_run(home: &TempDir, tag: &str) -> Box<dyn ImageGuest> {
    build(home, tag, &["say-trap", "/harness/codex/marker.txt", "TERM"], "");
    let mut s = spec(home, tag);
    s.members = vec![MemberSpec {
        member: codex(),
        env: Vec::new(),
        files: Vec::new(),
        workdir: None,
        stdin: false,
        terminal: false,
        args: Vec::new(),
        mcp: false,
    }];
    boot(home, &s)
}

/// D270 slice 4: a harness swapped now, in its running microVM, runs its component: its
/// generation before ends on its SIGTERM, its next finds the component's files at its path;
/// restored now, its image's own come back. Its components said as its host holds them; its
/// VM processes, how it started and its templates as they were: no microVM made or remade.
#[test]
fn a_harness_swapped_now_runs_its_component_and_restored_runs_its_images_own() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-swap-home");
    let digest = component(&home, "harness", "local/codex-v2:1", "v2", "{}");
    let guest = swap_run(&home, "api-swap:1");
    let (from, vms, templates) = (guest.started_from(), vm_processes(&home), templates_of(&home));
    let stdout =
        |generation| BufReader::new(guest.take_output(&codex(), generation, Channel::Stdout).unwrap());
    let (first, line) = line_within(&*guest, stdout(0));
    assert_eq!(line, "none\n", "its image's own: no marker");
    let patience = Duration::from_secs(60);
    let swapped = ask(&*guest, |g| {
        g.swap("/harness/codex", &digest, SwapWhen::Now, patience)
    })
    .unwrap();
    assert_eq!(swapped.started.map(|(_, g)| g), Some(1));
    let at = |desired: Option<&str>, held: Option<&str>| ComponentAt {
        at: "/harness/codex".into(),
        desired: desired.map(str::to_string),
        held: held.map(str::to_string),
    };
    assert_eq!(swapped.components, vec![at(Some(&digest), Some(&digest))]);
    assert_eq!(guest.components(), swapped.components);
    // `say-trap` says its marker, then that it is ready, then the signal it got.
    assert_eq!(rest_within(&*guest, first), "ready\ngot 15\n");
    let (next, line) = line_within(&*guest, stdout(1));
    assert_eq!(line, "v2\n", "its component, laid at its path");
    let restored = ask(&*guest, |g| g.restore("/harness/codex", SwapWhen::Now, patience)).unwrap();
    assert_eq!(restored.started.map(|(_, g)| g), Some(2));
    assert_eq!(
        restored.components,
        Vec::new(),
        "nothing wanted or held, its component let go"
    );
    assert_eq!(rest_within(&*guest, next), "ready\ngot 15\n");
    let (_, line) = line_within(&*guest, stdout(2));
    assert_eq!(line, "none\n", "its image's own again");
    assert_eq!(guest.started_from(), from);
    assert_eq!(vm_processes(&home), vms);
    assert_eq!(templates_of(&home), templates);
    guest.stop().unwrap();
}

/// D270 slice 5: a run holding a component is saved whole, where a run with anything plugged
/// was refused ("it has PCI functions plugged into it"): its snapshot keeps the component by
/// its key (its function's place and its range of the window beside it), and its member runs
/// on its component after the save as before it.
#[test]
fn a_run_holding_a_component_is_saved_with_it() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-swap-save-home");
    let digest = component(&home, "harness", "local/codex-v4:1", "v4", "{}");
    let guest = swap_run(&home, "api-swap-save:1");
    let stdout =
        |generation| BufReader::new(guest.take_output(&codex(), generation, Channel::Stdout).unwrap());
    let (first, line) = line_within(&*guest, stdout(0));
    assert_eq!(line, "none\n");
    let patience = Duration::from_secs(60);
    let swapped = ask(&*guest, |g| {
        g.swap("/harness/codex", &digest, SwapWhen::Now, patience)
    })
    .unwrap();
    assert_eq!(swapped.started.map(|(_, g)| g), Some(1));
    assert_eq!(rest_within(&*guest, first), "ready\ngot 15\n");
    let (_, line) = line_within(&*guest, stdout(1));
    assert_eq!(line, "v4\n", "its component, laid at its path");
    let saved = home.join("component-save");
    ask(&*guest, |g| g.save(&saved, Duration::MAX)).unwrap();
    let state = std::fs::read(common::snapshot_file(&saved, "state")).unwrap();
    let key = digest.strip_prefix("sha256:").unwrap();
    assert!(
        state.windows(key.len()).any(|w| w == key.as_bytes()),
        "its snapshot keeps the component by its key"
    );
    assert_eq!(
        guest.wait_member(&codex(), 1, Duration::ZERO),
        Ok(None),
        "its member runs on its component after the save"
    );
    guest.stop().unwrap();
}

/// D270 slice 4: a harness swapped at its next start keeps running its image's own until it
/// starts again; its restart lays the component; its components say it wanted, then held.
#[test]
fn a_harness_swapped_at_its_next_start_runs_its_component_once_restarted() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-swap-next-home");
    let digest = component(&home, "harness", "local/codex-v3:1", "v3", "{}");
    let guest = swap_run(&home, "api-swap-next:1");
    let stdout =
        |generation| BufReader::new(guest.take_output(&codex(), generation, Channel::Stdout).unwrap());
    let (first, line) = line_within(&*guest, stdout(0));
    assert_eq!(line, "none\n");
    let patience = Duration::from_secs(60);
    let swapped = ask(&*guest, |g| {
        g.swap("/harness/codex", &digest, SwapWhen::NextStart, patience)
    })
    .unwrap();
    assert_eq!(swapped.started, None, "nothing restarted");
    assert_eq!(
        swapped.components,
        vec![ComponentAt {
            at: "/harness/codex".into(),
            desired: Some(digest.clone()),
            held: None,
        }]
    );
    assert_eq!(
        guest.wait_member(&codex(), 0, Duration::ZERO),
        Ok(None),
        "its generation runs on"
    );
    assert_eq!(ask(&*guest, |g| g.restart(&codex(), &[], patience)), Ok(1));
    assert_eq!(rest_within(&*guest, first), "ready\ngot 15\n");
    let (_, line) = line_within(&*guest, stdout(1));
    assert_eq!(line, "v3\n", "laid at its next start");
    assert_eq!(
        guest.components(),
        swapped.components,
        "its components told as the swap answered them: a restart tells none"
    );
    guest.stop().unwrap();
}

/// D270 slice 4: what cannot be swapped is refused typed, its member running on: a digest
/// this host's store has not got (`Artifact`), an agent's artifact at a harness's path
/// (`Kind`), a path no member runs at (`Unknown`), and a component asking a port its member
/// is granted not (`Asks`).
#[test]
fn a_swap_that_cannot_be_made_is_refused_typed_and_its_member_runs_on() {
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("api-swap-refused-home");
    let agent = component(&home, "agent", "local/an-agent:1", "agent", "{}");
    let asking = component(
        &home,
        "harness",
        "local/codex-asks:1",
        "asks",
        r#"{"network":["22/tcp"]}"#,
    );
    let guest = swap_run(&home, "api-swap-refused:1");
    let (_, line) = line_within(
        &*guest,
        BufReader::new(guest.take_output(&codex(), 0, Channel::Stdout).unwrap()),
    );
    assert_eq!(line, "none\n");
    let patience = Duration::from_secs(60);
    let refused = |at: &str, digest: &str| ask(&*guest, |g| g.swap(at, digest, SwapWhen::Now, patience));
    let absent = format!("sha256:{}", "0".repeat(64));
    for (at, digest, why) in [
        ("/harness/codex", absent.as_str(), SwapRefusal::Artifact),
        ("/harness/codex", agent.as_str(), SwapRefusal::Kind),
        ("/harness/nobody", agent.as_str(), SwapRefusal::Unknown),
        ("/harness/codex", asking.as_str(), SwapRefusal::Asks),
    ] {
        match refused(at, digest) {
            Err(ImageRefusal::Swap(cause, words)) => assert_eq!(cause, why, "{at} {digest}: {words}"),
            other => panic!("{at} {digest}: {other:?}"),
        }
    }
    assert_eq!(guest.components(), Vec::new());
    assert_eq!(
        guest.wait_member(&codex(), 0, Duration::ZERO),
        Ok(None),
        "its member runs on"
    );
    guest.stop().unwrap();
}
