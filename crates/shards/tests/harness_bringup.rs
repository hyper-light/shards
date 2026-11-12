//! The publishers' own Codex CLI and Claude Code, each started in a microVM as a harness by
//! its root `harness.json` (architecture.md D54; the managed-runtime plan's G24 and P10),
//! from the example build contexts in docs/examples/harness-*: no RUN or VOLUME installs
//! either, and no credential is in an image.
//!
//! Ignored: each test needs its publisher's verified Linux build for this host's
//! architecture, in the directory `SHARDS_HARNESS_CODEX` or `SHARDS_HARNESS_CLAUDE` names
//! (docs/research/measurements/harness-bringup-v1/fetch.sh leaves each harness's binaries
//! in one), which becomes the harness's `bin`; those that reach a provider need the network.
//! A provider's refusal, in the harness's own output exactly as the run's owner reads it
//! from its private endpoint, is the proof that the harness ran and reached the provider
//! over the network its Agentfile grants. Where a key is needed for a request to be made at
//! all, it is a placeholder given at run time, never in the image and never a real one.
//! `SHARDS_HARNESS_RECORDS`, an absolute directory, keeps what each test saw.
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

#[path = "common/managed_capture.rs"]
mod managed_capture;

use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, workspace};
use managed_capture::{Ask, Event, Metadata, Observer};
use shards_abi::run::managed::{Channel, Event as CaptureEvent, Record, StreamState};
use shards_remote::identity::Identity;
use shards_remote::net::Command;

// Test patience for a base image's pull, a build of a few hundred megabytes, a boot and a
// provider's answer on a loaded host; never runtime policy.
const WITHIN: Duration = Duration::from_secs(600);

/// A key no provider issued, given at run time where a harness needs one to send a request.
const PLACEHOLDER: &str = "sk-shards-placeholder-not-a-key";

/// The base image of the examples, and of the probes below.
const BASE: &str = "alpine:3.24.4@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf";

/// One bring-up: what to build and run, and how far to read.
#[derive(Default)]
struct Case<'a> {
    /// The harness's name: its example is `docs/examples/harness-<name>`.
    name: &'a str,
    /// The variable naming the directory of its publisher's binaries, its `bin`; none for
    /// a probe that runs the base image's own programs.
    variable: Option<&'a str>,
    /// Where it keeps what it saw, under `SHARDS_HARNESS_RECORDS`.
    record: &'a str,
    /// The Agentfile and harness.json, where not the example's.
    agentfile: Option<String>,
    descriptor: Option<String>,
    /// How the kernel's record starts the harness's own exec, where not with
    /// `/harness/<name>/bin/<name> `.
    exec: Option<&'a str>,
    /// `shards run` options before the image.
    run: Vec<String>,
    /// Stop reading once stdout says this, the harness still running; else read both
    /// channels to their ends.
    until_said: Option<&'a str>,
}

/// What a harness's microVM showed of it.
struct Seen {
    /// Its stdout and stderr, each exactly as its owner endpoint delivered it.
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// How each channel's reading ended: `End` at its EOF, `Aborted <errno>`, `Said` where
    /// stdout said what was awaited, `transport <event>` or `Disconnected` where its
    /// subscription ended first, or why it was not read.
    ends: [String; 2],
    /// How long each channel was read, from its subscription to its reading's end.
    read_for: [Duration; 2],
    /// How each subscription's transport ended, where not well.
    drivers: Vec<String>,
    /// `shards read processes`: the kernel's record of the microVM's processes.
    processes: String,
}

impl Seen {
    fn harness(&self, program: &str, event: &str) -> Option<(String, String)> {
        row(&self.processes, program, event)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// In `shards read processes`' table (TIME, MEMBER, PID, EVENT, WHAT), the row of `event`
/// by the process whose exec's command line starts with `program`, and what it says:
/// (pid, what).
fn row(processes: &str, program: &str, event: &str) -> Option<(String, String)> {
    let pid = processes.lines().find_map(|line| {
        let (head, what) = line.split_once("  exec   ")?;
        what.starts_with(program)
            .then(|| head.split_whitespace().last().map(str::to_string))?
    })?;
    processes.lines().find_map(|line| {
        let (head, what) = line.split_once(&format!("  {event}   "))?;
        (head.split_whitespace().last() == Some(pid.as_str())).then(|| (pid.clone(), what.to_string()))
    })
}

/// Where a test keeps what it saw: `SHARDS_HARNESS_RECORDS/<record>`, or nowhere.
fn records(record: &str) -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("SHARDS_HARNESS_RECORDS")?);
    assert!(root.is_absolute(), "SHARDS_HARNESS_RECORDS is absolute");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join(record);
    std::fs::create_dir(&dir).unwrap();
    Some(dir)
}

fn example(name: &str, file: &str) -> String {
    std::fs::read_to_string(workspace().join(format!("docs/examples/harness-{name}/{file}"))).unwrap()
}

/// Builds the case's context: the example's, or its Agentfile and harness.json, with the
/// binaries of the directory its variable names as its harness's `bin`. Runs it detached,
/// the base image's `sleep` as its command; reads the harness's stdout, then its stderr,
/// from the run's owner endpoint; then the kernel's record of the microVM's processes. None
/// where this host runs no VMs.
fn bring_up(case: &Case<'_>) -> Option<Seen> {
    if cannot_run_vms() {
        return None;
    }
    let name = case.name;
    let until = Instant::now() + WITHIN;
    let kept = records(case.record);
    let save = |file: &str, bytes: &[u8]| {
        if let Some(dir) = &kept {
            std::fs::write(dir.join(file), bytes).unwrap();
        }
    };
    let ctx = TempDir::new(&format!("harness-{name}-ctx"));
    let file = case
        .agentfile
        .clone()
        .unwrap_or_else(|| example(name, "Agentfile"));
    std::fs::write(ctx.join("Agentfile"), &file).unwrap();
    save("Agentfile", file.as_bytes());
    std::fs::create_dir_all(ctx.join(name)).unwrap();
    let descriptor = case
        .descriptor
        .clone()
        .unwrap_or_else(|| example(name, &format!("{name}/harness.json")));
    std::fs::write(ctx.join(name).join("harness.json"), &descriptor).unwrap();
    save("harness.json", descriptor.as_bytes());
    if let Some(variable) = case.variable {
        let binaries = PathBuf::from(std::env::var_os(variable).unwrap_or_else(|| {
            panic!("supply {variable}: the publisher's verified binaries (harness-bringup-v1/fetch.sh)")
        }));
        std::fs::create_dir(ctx.join(name).join("bin")).unwrap();
        let mut placed = Vec::new();
        for entry in std::fs::read_dir(&binaries).unwrap() {
            let from = entry.unwrap().path();
            let to = ctx.join(name).join("bin").join(from.file_name().unwrap());
            std::fs::copy(&from, &to).unwrap();
            set_mode(&to, 0o755);
            placed.push(to.file_name().unwrap().to_string_lossy().into_owned());
        }
        placed.sort();
        save("bin.txt", placed.join("\n").as_bytes());
        assert!(
            placed.iter().any(|p| p == name),
            "no {name} in {binaries:?}: {placed:?}"
        );
    }

    let home = TempDir::new(&format!("harness-{name}-home"));
    let owner = Identity::load_or_create(&home.join("identity")).unwrap();
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| {
        let left = until.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "within the test's patience");
        run_shards_env(&[], args, &env, left)
    };
    let image = format!("harness-{name}:1");
    let built = shards(&[
        "build",
        "image",
        "--progress=plain",
        "-t",
        &image,
        ctx.to_str().unwrap(),
    ]);
    save("build.log", built.to_string().as_bytes());
    assert_eq!(built.status, Some(0), "{built}");
    let mut args = vec!["run", "vm", "-d"];
    args.extend(case.run.iter().map(String::as_str));
    args.extend(["--name", name, &image, "sleep", "600"]);
    let started = shards(&args);
    save(
        "run.log",
        format!("shards {}\n{started}", args.join(" ")).as_bytes(),
    );
    assert_eq!(started.status, Some(0), "{started}");
    let id = started.stdout.trim().to_string();
    let published =
        std::fs::read_to_string(home.join("containers").join(&id).join("managed-output")).unwrap();
    let metadata = Metadata::parse(&published).unwrap();
    assert_eq!(metadata.owner, owner.fingerprint());
    // The harness is the image's one member: member 0, its stdout and its stderr.
    assert_eq!(
        metadata.endpoints.keys().copied().collect::<Vec<_>>(),
        [(0, 1), (0, 2)]
    );

    // Each channel is subscribed as it is read: an owner's subscription that hears nothing
    // for its transport's idle bound ends (README, finding H1), and stderr may say nothing
    // while stdout is read.
    let subscribe = |channel: Channel| {
        Observer::start(
            &owner,
            &metadata,
            metadata.scope(0, channel),
            metadata.endpoints[&(0, channel as u8)],
            until,
        )
    };
    let label = format!("harness {name}");
    let reading = Instant::now();
    let mut observers = vec![subscribe(Channel::Stdout)];
    let (stdout, out_end) = read(&observers[0], &label, until, case.until_said);
    let out_for = reading.elapsed();
    save("stdout.bin", &stdout);
    let ended = out_end == "End";
    let reading = Instant::now();
    let (stderr, err_end) = if ended {
        observers.push(subscribe(Channel::Stderr));
        read(&observers[1], &label, until, None)
    } else {
        (Vec::new(), "not read: its stdout did not end".into())
    };
    let err_for = reading.elapsed();
    save("stderr.bin", &stderr);
    save(
        "ends.txt",
        format!("stdout {out_end} after {out_for:?}\nstderr {err_end} after {err_for:?}\n").as_bytes(),
    );

    // The record holds the harness's exit once its process has ended, which its output's
    // ends come before: read again until it does, or the patience runs out.
    let program = case
        .exec
        .map_or_else(|| format!("/harness/{name}/bin/{name} "), str::to_string);
    let processes = loop {
        let read = shards(&["read", "processes", name]);
        assert_eq!(read.status, Some(0), "{read}");
        if !ended || row(&read.stdout, &program, "exit").is_some() || Instant::now() >= until {
            break read.stdout;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    save("processes.log", processes.as_bytes());
    let ports = shards(&["read", "ports", name]);
    save("ports.log", ports.to_string().as_bytes());

    for observer in &observers {
        observer.sender.request_stop();
    }
    let removed = shards(&["rm", "-f", name]);
    let stopped = shards(&["stop", "daemon"]);
    let mut drivers = Vec::new();
    for observer in &mut observers {
        if let Err(error) = observer.join(until) {
            drivers.push(error);
        }
    }
    save("drivers.txt", drivers.join("\n").as_bytes());
    assert_eq!(removed.status, Some(0), "{removed}");
    assert_eq!(stopped.status, Some(0), "{stopped}");
    Some(Seen {
        stdout,
        stderr,
        ends: [out_end, err_end],
        read_for: [out_for, err_for],
        drivers,
        processes,
    })
}

/// One channel's bytes, each record acknowledged as it comes: to its end (`End`, or
/// `Aborted <errno>`), until they say `said` (`Said`), or until its subscription ends
/// (`transport <event>`, or `Disconnected` where its driver is gone).
fn read(observer: &Observer, label: &str, until: Instant, said: Option<&str>) -> (Vec<u8>, String) {
    let mut state = StreamState::New;
    let (mut wire, mut bytes) = (0u64, Vec::new());
    loop {
        let event = match observer
            .events
            .recv_timeout(until.saturating_duration_since(Instant::now()))
        {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => panic!(
                "past the test's patience; the harness said:\n{}",
                String::from_utf8_lossy(&bytes)
            ),
            Err(RecvTimeoutError::Disconnected) => return (bytes, "Disconnected".into()),
        };
        let Event::Record(offset, chunk) = event else {
            return (bytes, format!("transport {event:?}"));
        };
        assert_eq!(offset, wire, "contiguous transport offset");
        wire = wire.checked_add(u64::try_from(chunk.len()).unwrap()).unwrap();
        let record = Record::decode(&chunk).unwrap();
        assert_eq!((record.member, record.channel), (0, observer.scope.channel));
        state.accept(record).unwrap();
        let end = match record.event {
            CaptureEvent::Open(actual) => {
                assert_eq!(actual, label);
                None
            }
            CaptureEvent::Data(data) => {
                bytes.extend_from_slice(data);
                None
            }
            CaptureEvent::End => Some("End".to_string()),
            CaptureEvent::Aborted(errno) => Some(format!("Aborted {errno}")),
        };
        observer.sender.send(Command::Handler(Ask::Ack(wire))).unwrap();
        if let Some(end) = end {
            return (bytes, end);
        }
        if said.is_some_and(|s| String::from_utf8_lossy(&bytes).contains(s)) {
            return (bytes, "Said".into());
        }
    }
}

/// What a test saw, printed for its record and kept for its failure message.
fn shown(seen: &Seen) -> String {
    let all = format!(
        "--- stdout ({}, read for {:?})\n{}\n--- stderr ({}, read for {:?})\n{}\n--- drivers\n{}\n--- processes\n{}",
        seen.ends[0],
        seen.read_for[0],
        seen.text(),
        seen.ends[1],
        seen.read_for[1],
        String::from_utf8_lossy(&seen.stderr),
        seen.drivers.join("\n"),
        seen.processes
    );
    let _ = std::io::Write::write_all(&mut std::io::stdout(), all.as_bytes());
    all
}

/// Each line of `seen`'s stdout as JSON, as the harness wrote it.
fn json_lines(seen: &Seen, all: &str) -> Vec<serde_json::Value> {
    let text = String::from_utf8(seen.stdout.clone()).unwrap_or_else(|e| panic!("{e}\n{all}"));
    assert!(text.ends_with('\n'), "{all}");
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line:?}\n{all}")))
        .collect()
}

/// Codex's JSONL events, from its thread's start to its turn's failure: that failure's
/// message.
fn codex_failure(seen: &Seen, all: &str) -> String {
    let events = json_lines(seen, all);
    assert_eq!(
        events.first().and_then(|e| e["type"].as_str()),
        Some("thread.started"),
        "{all}"
    );
    let last = events.last().unwrap();
    assert_eq!(last["type"], "turn.failed", "{all}");
    last["error"]["message"].as_str().unwrap().to_string()
}

/// Claude Code's stream-json, from its `system` `init` to its `result`: all of it.
fn claude_events(seen: &Seen, all: &str) -> Vec<serde_json::Value> {
    let events = json_lines(seen, all);
    let init = events.first().unwrap();
    assert_eq!(
        (init["type"].as_str(), init["subtype"].as_str()),
        (Some("system"), Some("init")),
        "{all}"
    );
    assert_eq!(events.last().unwrap()["type"], "result", "{all}");
    events
}

/// Both channels read to their ends, and both subscriptions ended well.
fn whole(seen: &Seen, all: &str) {
    assert_eq!(seen.ends, ["End".to_string(), "End".to_string()], "{all}");
    assert!(seen.drivers.is_empty(), "{all}");
}

/// The example's Agentfile with `lines` after it.
fn example_with(name: &str, lines: &str) -> String {
    example(name, "Agentfile") + lines
}

/// The example's Agentfile without its network directives: nothing past the microVM.
fn airgapped(name: &str) -> String {
    let file = example(name, "Agentfile");
    let kept: Vec<&str> = file
        .lines()
        .filter(|line| {
            !["NETWORK ", "EXPOSE ", "CONNECT "]
                .iter()
                .any(|d| line.starts_with(d))
        })
        .collect();
    assert_eq!(
        file.lines().count() - kept.len(),
        3,
        "the example's three grants: {file}"
    );
    kept.join("\n") + "\n"
}

/// A host directory to give a harness at run time, `files` in it, `mode` its own, inside a
/// directory only this host's user may enter.
fn given(files: &[(&str, &str, u32)], mode: u32) -> (TempDir, PathBuf) {
    let outer = TempDir::new("harness-given");
    let dir = outer.join("share");
    std::fs::create_dir(&dir).unwrap();
    for (file, text, file_mode) in files {
        std::fs::write(dir.join(file), text).unwrap();
        set_mode(&dir.join(file), *file_mode);
    }
    set_mode(&dir, mode);
    set_mode(&outer, 0o700);
    (outer, dir)
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap();
    #[cfg(not(unix))]
    let _ = (path, mode);
}

/// OpenAI's Codex CLI, `codex exec --json`, started by its harness.json: the kernel saw
/// the very command line it declares, with no shell between, and the run's owner reads its
/// JSONL events, each line JSON as Codex wrote it, to the provider's refusal: a 401 from
/// api.openai.com, with the request id only OpenAI's API gives.
#[test]
#[ignore = "needs SHARDS_HARNESS_CODEX and the network to reach OpenAI"]
fn a_real_codex_starts_as_a_harness_and_reaches_its_provider() {
    let Some(seen) = bring_up(&Case {
        name: "codex",
        variable: Some("SHARDS_HARNESS_CODEX"),
        record: "codex",
        ..Case::default()
    }) else {
        return;
    };
    let all = shown(&seen);
    let (_, exec) = seen
        .harness("/harness/codex/bin/codex ", "exec")
        .unwrap_or_else(|| panic!("{all}"));
    assert_eq!(
        exec,
        r#"/harness/codex/bin/codex exec --json --skip-git-repo-check --ephemeral "Reply with the single word: ready""#,
        "{all}"
    );
    whole(&seen, &all);
    let (_, exit) = seen
        .harness("/harness/codex/bin/codex ", "exit")
        .unwrap_or_else(|| panic!("{all}"));
    assert_eq!(exit, "code 1", "{all}");
    let failure = codex_failure(&seen, &all);
    for said in [
        "unexpected status 401 Unauthorized: Missing bearer or basic authentication in header",
        "url: https://api.openai.com/v1/responses",
        "request id: req_",
    ] {
        assert!(failure.contains(said), "no {said:?} in {failure:?}\n{all}");
    }
}

/// The control: the same image less its NETWORK, EXPOSE and CONNECT directives. Its
/// harness runs as before, but nothing past the microVM exists for it (default deny): it
/// finds no name, and then says it waits for a network, which it does for as long as it
/// runs. No provider answers: no status, no request id, no edge.
#[test]
#[ignore = "needs SHARDS_HARNESS_CODEX"]
fn a_real_codex_without_its_grants_reaches_no_provider() {
    let Some(seen) = bring_up(&Case {
        name: "codex",
        variable: Some("SHARDS_HARNESS_CODEX"),
        record: "codex-airgapped",
        agentfile: Some(airgapped("codex")),
        until_said: Some("waiting for network"),
        ..Case::default()
    }) else {
        return;
    };
    let all = shown(&seen);
    assert!(
        seen.harness("/harness/codex/bin/codex ", "exec").is_some(),
        "{all}"
    );
    assert_eq!(seen.ends[0], "Said", "{all}");
    let text = seen.text();
    assert!(text.contains("failed to lookup address information"), "{all}");
    for unsaid in ["unexpected status", "request id", "cf-ray"] {
        assert!(!text.contains(unsaid), "{unsaid:?} from a provider\n{all}");
    }
}

/// A run's environment is its command's: `-e` reaches no harness, whose environment is its
/// own and its image's (D54, domains.rs). Codex reads `CODEX_API_KEY` in `codex exec` and
/// `OPENAI_API_KEY`; given both by `-e`, it still sends no key.
#[test]
#[ignore = "needs SHARDS_HARNESS_CODEX and the network to reach OpenAI"]
fn a_runs_environment_reaches_no_harness() {
    let Some(seen) = bring_up(&Case {
        name: "codex",
        variable: Some("SHARDS_HARNESS_CODEX"),
        record: "codex-run-env",
        run: vec![
            "-e".into(),
            format!("CODEX_API_KEY={PLACEHOLDER}"),
            "-e".into(),
            format!("OPENAI_API_KEY={PLACEHOLDER}"),
        ],
        ..Case::default()
    }) else {
        return;
    };
    let all = shown(&seen);
    whole(&seen, &all);
    let failure = codex_failure(&seen, &all);
    assert!(
        failure.contains("401 Unauthorized: Missing bearer or basic authentication in header"),
        "{all}"
    );
}

/// Codex's own login cache, given at run time: a volume the Agentfile declares for the
/// harness alone, `CODEX_HOME` pointing at it from the full OSI form of its harness.json (a
/// path, no secret), and `shards run -v` standing a host directory in for it, holding the
/// `auth.json` Codex's `login --with-api-key` writes, of a placeholder key, as Codex writes
/// it (0600) in a directory only the host's user may enter. Codex sends that key, and
/// OpenAI refuses it by its masked name: the credential came from the run, not the image,
/// and reached the provider. Native delegation (G18): the harness reads its key.
#[test]
#[ignore = "needs SHARDS_HARNESS_CODEX and the network to reach OpenAI"]
fn a_real_codex_takes_its_login_from_a_volume_given_at_run_time() {
    let auth = format!("{{\n  \"auth_mode\": \"apikey\",\n  \"OPENAI_API_KEY\": \"{PLACEHOLDER}\"\n}}");
    let (_outer, share) = given(&[("auth.json", &auth, 0o600)], 0o700);
    let Some(seen) = bring_up(&Case {
        name: "codex",
        variable: Some("SHARDS_HARNESS_CODEX"),
        record: "codex-login-volume",
        agentfile: Some(example_with(
            "codex",
            "VOLUME --target-kind=harness /run/codex-home FOR codex\n",
        )),
        descriptor: Some(
            r#"{"schemaVersion":1,"name":"codex","run":{"command":["codex","exec","--json","--skip-git-repo-check","--ephemeral","Reply with the single word: ready"],"env":["CODEX_HOME=/run/codex-home"]}}"#
                .to_string(),
        ),
        run: vec!["-v".into(), format!("{}:/run/codex-home", share.display())],
        ..Case::default()
    }) else {
        return;
    };
    let all = shown(&seen);
    whole(&seen, &all);
    let failure = codex_failure(&seen, &all);
    for said in [
        "unexpected status 401 Unauthorized: Incorrect API key provided: sk-shard",
        "url: https://api.openai.com/v1/responses",
        "request id: req_",
        "auth error code: invalid_api_key",
    ] {
        assert!(failure.contains(said), "no {said:?} in {failure:?}\n{all}");
    }
    assert!(
        !seen.text().contains(PLACEHOLDER),
        "the key itself is not echoed\n{all}"
    );
}

/// Anthropic's Claude Code, `claude -p` with stream-json output, started by its
/// harness.json: the kernel saw the command line it declares, and the run's owner reads
/// its stream-json. Without a credential it refuses before any request: its own
/// `Not logged in`, no API time, no status.
#[test]
#[ignore = "needs SHARDS_HARNESS_CLAUDE"]
fn a_real_claude_code_starts_as_a_harness_and_refuses_without_a_login() {
    let Some(seen) = bring_up(&Case {
        name: "claude",
        variable: Some("SHARDS_HARNESS_CLAUDE"),
        record: "claude",
        ..Case::default()
    }) else {
        return;
    };
    let all = shown(&seen);
    let (_, exec) = seen
        .harness("/harness/claude/bin/claude ", "exec")
        .unwrap_or_else(|| panic!("{all}"));
    assert_eq!(
        exec,
        r#"/harness/claude/bin/claude -p --output-format stream-json --verbose "Reply with the single word: ready""#,
        "{all}"
    );
    whole(&seen, &all);
    let (_, exit) = seen
        .harness("/harness/claude/bin/claude ", "exit")
        .unwrap_or_else(|| panic!("{all}"));
    assert_eq!(exit, "code 1", "{all}");
    let events = claude_events(&seen, &all);
    let result = events.last().unwrap();
    assert_eq!(events[0]["apiKeySource"], "none", "{all}");
    assert_eq!(result["result"], "Not logged in · Please run /login", "{all}");
    assert!(result["api_error_status"].is_null(), "{all}");
    assert!(!events.iter().any(|e| e["subtype"] == "api_retry"), "{all}");
}

/// Claude Code given a placeholder key at run time: a volume declared for the harness
/// alone, `--settings` naming a file in it (a path, no secret), whose `apiKeyHelper` reads
/// the key beside it, and `shards run -v` standing a host directory in for the volume,
/// read-only, the key in it as a person keeps one (0600, in a directory only its owner may
/// enter). Claude Code sends the key and Anthropic's API refuses it, each try a 401, until
/// Claude Code gives up: the key came from the run, not the image, and reached the provider
/// over the granted network. Native delegation (G18): the harness runs the helper and
/// holds the key.
#[test]
#[ignore = "needs SHARDS_HARNESS_CLAUDE and the network to reach Anthropic"]
fn a_real_claude_code_takes_a_key_from_a_volume_given_at_run_time() {
    let (_outer, share) = given(
        &[
            (
                "settings.json",
                r#"{"apiKeyHelper":"cat /run/claude-auth/api-key"}"#,
                0o600,
            ),
            ("api-key", PLACEHOLDER, 0o600),
        ],
        0o700,
    );
    let Some(seen) = bring_up(&Case {
        name: "claude",
        variable: Some("SHARDS_HARNESS_CLAUDE"),
        record: "claude-key-volume",
        agentfile: Some(example_with(
            "claude",
            "VOLUME --target-kind=harness /run/claude-auth FOR claude\n",
        )),
        descriptor: Some(
            r#"{"entrypoint":"claude -p","options":["--settings","/run/claude-auth/settings.json","--output-format","stream-json","--verbose","Reply with the single word: ready"]}"#
                .to_string(),
        ),
        run: vec!["-v".into(), format!("{}:/run/claude-auth:ro", share.display())],
        ..Case::default()
    }) else {
        return;
    };
    let all = shown(&seen);
    whole(&seen, &all);
    let events = claude_events(&seen, &all);
    let result = events.last().unwrap();
    assert_eq!(events[0]["apiKeySource"], "apiKeyHelper", "{all}");
    assert!(
        events
            .iter()
            .any(|e| e["subtype"] == "api_retry" && e["error_status"] == 401),
        "{all}"
    );
    assert_eq!(result["api_error_status"], 401, "{all}");
    assert_eq!(
        result["result"], "Invalid API key · Fix external API key",
        "{all}"
    );
    assert!(
        !seen.text().contains(PLACEHOLDER),
        "the key itself is not echoed\n{all}"
    );
}

/// A probe harness, no binary of its own, running the base image's `sh`: a host directory
/// given it at run time, only the host's user's to enter (0700) and its file only theirs to
/// read (0600), is shown the harness as its own (uid and gid 200000, the first domain's),
/// the host's modes kept, so the harness reads it: the host's modes keep the host's other
/// users out, and the volume's `FOR` keeps every other member out.
#[test]
#[ignore = "a measurement: what a run-time volume shows a harness"]
fn a_volume_given_at_run_time_is_the_harness_own() {
    let (_outer, share) = given(&[("secret", PLACEHOLDER, 0o600)], 0o700);
    let Some(seen) = bring_up(&Case {
        name: "probe",
        record: "probe-volume",
        agentfile: Some(format!(
            "FROM {BASE}\nHARNESS probe FROM ./probe\nVOLUME --target-kind=harness /run/probe FOR probe\n"
        )),
        descriptor: Some(
            r#"{"entrypoint":"sh -c","options":["id; ls -lan /run/probe; if cat /run/probe/secret >/dev/null; then echo read; else echo unread; fi"]}"#
                .to_string(),
        ),
        exec: Some("/bin/sh -c "),
        run: vec!["-v".into(), format!("{}:/run/probe:ro", share.display())],
        ..Case::default()
    }) else {
        return;
    };
    let all = shown(&seen);
    whole(&seen, &all);
    let text = seen.text();
    let lines: Vec<Vec<&str>> = text.lines().map(|l| l.split_whitespace().collect()).collect();
    assert_eq!(
        lines.first().and_then(|l| l.first()),
        Some(&"uid=200000"),
        "{all}"
    );
    let listed = |name: &str| {
        lines
            .iter()
            .find(|l| l.last() == Some(&name))
            .map(|l| [l[0], l[2], l[3]])
    };
    assert_eq!(listed("."), Some(["drwx------", "200000", "200000"]), "{all}");
    assert_eq!(
        listed("secret"),
        Some(["-rw-------", "200000", "200000"]),
        "{all}"
    );
    assert_eq!(text.lines().last(), Some("read"), "{all}");
}

/// A probe harness that says nothing for `silence` seconds, then one line, read by its
/// owner from its first instruction.
fn quiet_for(silence: u64, record: &str) -> Option<Seen> {
    bring_up(&Case {
        name: "probe",
        record,
        agentfile: Some(format!("FROM {BASE}\nHARNESS probe FROM ./probe\n")),
        descriptor: Some(format!(
            r#"{{"entrypoint":"sh -c","options":["sleep {silence}; echo after-quiet"]}}"#
        )),
        exec: Some("/bin/sh -c "),
        ..Case::default()
    })
}

/// Finding H1, red: an owner's subscription outlives a harness that writes nothing for
/// longer than the owner endpoint's idle bound (`shards_remote::tls::QUIET`, which its
/// transport's `max_idle_timeout` is) by a keep-alive interval (a quarter of it, as
/// `tls.rs` keeps its other connections alive). A model's long turn is such a silence.
#[test]
#[ignore = "red: finding H1, the owner endpoint's transport keeps no quiet subscription alive"]
fn an_owners_subscription_outlives_a_quiet_harness() {
    let quiet = shards_remote::tls::QUIET.as_secs();
    let Some(seen) = quiet_for(quiet + quiet / 4, "probe-quiet") else {
        return;
    };
    let all = shown(&seen);
    whole(&seen, &all);
    assert_eq!(seen.text(), "after-quiet\n", "{all}");
}

/// H1's twin: a silence a keep-alive interval short of the idle bound, which the same
/// subscription outlives today.
#[test]
#[ignore = "H1's control: a silence within the owner endpoint's idle bound"]
fn an_owners_subscription_outlives_a_silence_within_its_idle_bound() {
    let quiet = shards_remote::tls::QUIET.as_secs();
    let Some(seen) = quiet_for(quiet - quiet / 4, "probe-within") else {
        return;
    };
    let all = shown(&seen);
    whole(&seen, &all);
    assert_eq!(seen.text(), "after-quiet\n", "{all}");
}

/// The path of a private copy of a signed-in harness's home or config directory, which the
/// caller made of this machine's user's own login
/// (docs/research/measurements/harness-bringup-v2/signed-in.sh). Never read here: only
/// given to the harness.
fn signed_in(variable: &str) -> String {
    let path = PathBuf::from(std::env::var_os(variable).unwrap_or_else(|| {
        panic!("supply {variable}: a private copy of a signed-in login (harness-bringup-v2/signed-in.sh)")
    }));
    assert!(
        path.is_absolute() && path.is_dir(),
        "{variable} is an absolute directory"
    );
    path.display().to_string()
}

/// The files of the project a signed-in harness works on: names it can only learn by
/// running a tool.
const PROJECT: [&str; 2] = ["shards-turn-alpha.txt", "shards-turn-beta.md"];

fn project() -> TempDir {
    let dir = TempDir::new("harness-project");
    for name in PROJECT {
        std::fs::write(dir.join(name), format!("{name}\n")).unwrap();
    }
    dir
}

/// Whether the kernel saw the harness `name` exec `program` itself (its command line's
/// first word, bare or a path), whatever shell started it.
fn ran_in_domain(seen: &Seen, name: &str, program: &str) -> bool {
    seen.processes.lines().any(|line| {
        line.split_once("  exec   ").is_some_and(|(head, what)| {
            let first = what.split_whitespace().next().unwrap_or_default();
            head.contains(&format!("harness {name} "))
                && (first == program || first.ends_with(&format!("/{program}")))
        })
    })
}

/// Codex signed in with this machine's user's own login, given at run time as a private
/// copy of its home (CODEX_HOME, from the full OSI form of harness.json) on a volume for
/// the harness alone: one real turn, in which it runs `ls` in the project volume and
/// answers with what it found. Its JSONL holds the command, its output and the answer;
/// the kernel saw the command exec in the harness's domain.
#[test]
#[ignore = "needs SHARDS_HARNESS_CODEX, a signed-in SHARDS_HARNESS_CODEX_HOME copy, and OpenAI"]
fn a_real_codex_signed_in_runs_a_tool_and_answers() {
    let home = signed_in("SHARDS_HARNESS_CODEX_HOME");
    let project = project();
    let Some(seen) = bring_up(&Case {
        name: "codex",
        variable: Some("SHARDS_HARNESS_CODEX"),
        record: "codex-signed-in",
        agentfile: Some(example("codex-signed-in", "Agentfile")),
        descriptor: Some(example("codex-signed-in", "codex/harness.json")),
        run: vec![
            "-v".into(),
            format!("{home}:/run/codex-home"),
            "-v".into(),
            format!("{}:/work:ro", project.display()),
        ],
        ..Case::default()
    }) else {
        return;
    };
    let all = shown(&seen);
    whole(&seen, &all);
    let events = json_lines(&seen, &all);
    assert_eq!(events.last().unwrap()["type"], "turn.completed", "{all}");
    let items: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["type"] == "item.completed")
        .map(|e| &e["item"])
        .collect();
    let ran = items
        .iter()
        .find(|i| i["type"] == "command_execution" && i["command"].as_str().is_some_and(|c| c.contains("ls")))
        .unwrap_or_else(|| panic!("no ls ran\n{all}"));
    assert_eq!(ran["exit_code"], 0, "{all}");
    let answer = items
        .iter()
        .rev()
        .find(|i| i["type"] == "agent_message")
        .unwrap_or_else(|| panic!("no answer\n{all}"));
    for file in PROJECT {
        assert!(ran["aggregated_output"].as_str().unwrap().contains(file), "{all}");
        assert!(answer["text"].as_str().unwrap().contains(file), "{all}");
    }
    assert!(ran_in_domain(&seen, "codex", "ls"), "{all}");
}

/// Claude Code signed in with this machine's user's own login, given at run time as a
/// private copy of its config directory (CLAUDE_CONFIG_DIR, from the full OSI form of
/// harness.json) on a volume for the harness alone: one real turn, in which it runs `ls`
/// in the project volume through its Bash tool and answers with what it found.
#[test]
#[ignore = "needs SHARDS_HARNESS_CLAUDE, a signed-in SHARDS_HARNESS_CLAUDE_HOME copy, and Anthropic"]
fn a_real_claude_code_signed_in_answers() {
    let home = signed_in("SHARDS_HARNESS_CLAUDE_HOME");
    let project = project();
    let Some(seen) = bring_up(&Case {
        name: "claude",
        variable: Some("SHARDS_HARNESS_CLAUDE"),
        record: "claude-signed-in",
        agentfile: Some(example("claude-signed-in", "Agentfile")),
        descriptor: Some(example("claude-signed-in", "claude/harness.json")),
        run: vec![
            "-v".into(),
            format!("{home}:/run/claude-home"),
            "-v".into(),
            format!("{}:/work:ro", project.display()),
        ],
        ..Case::default()
    }) else {
        return;
    };
    let all = shown(&seen);
    whole(&seen, &all);
    let events = claude_events(&seen, &all);
    let result = events.last().unwrap();
    assert_eq!(result["is_error"], false, "{all}");
    assert!(
        events.iter().any(|e| e["type"] == "assistant"
            && e["message"]["content"]
                .as_array()
                .is_some_and(|c| c.iter().any(|p| p["type"] == "tool_use" && p["name"] == "Bash"))),
        "{all}"
    );
    let answer = result["result"].as_str().unwrap();
    for file in PROJECT {
        assert!(answer.contains(file), "{all}");
    }
    assert!(ran_in_domain(&seen, "claude", "ls"), "{all}");
}
