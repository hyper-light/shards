//! MicroVMs moved to another machine as they run (D176 part 6), end to end on real
//! microVMs: `shards move vm NAME CONTEXT` of a home's microVM to another home whose daemon
//! listens, reached through a context (D180), as another machine's is. On this machine, two
//! homes stand for two machines: nothing of the one is the other's but what the move sends
//! (its directory, snapshot, volumes and the files its snapshot restores against) and what
//! the other machine pulls itself (its image, by the digest it was made of). Between two
//! machines, `a_microvm_moves_to_another_host_saved_and_live` moves microVMs to the machine
//! whose invitation `SHARDS_MOVE_INVITE` names, and the harness measures moves there
//! (docs/research/measurements/migrate/two-hosts.sh).

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

mod common;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Run, TempDir, cannot_pool, cannot_run_vms, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(120);

/// `shards ARGS` in `home`, whose runs boot the test kernel and init.
fn shards_in(home: &Path, args: &[&str]) -> Run {
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    run_shards_env(&[], args, &env, TIMEOUT)
}

/// A home whose daemon listens for another machine's clients, stopped as this drops.
struct Listening(PathBuf);

impl Drop for Listening {
    fn drop(&mut self) {
        let _ = shards_in(&self.0, &["daemon", "stop"]);
    }
}

/// Two homes, two machines: `there`, whose daemon listens on loopback, and `here`, joined to
/// it as context `lab`; the test image pulled here alone.
fn machines(name: &str) -> Option<(TempDir, TempDir, Listening, String)> {
    if cannot_run_vms() || cannot_pool() {
        return None;
    }
    let (image, _) = served();
    let here = TempDir::new(&format!("{name}-h"));
    let there = TempDir::new(&format!("{name}-t"));
    let started = shards_in(
        &there,
        &["run", "daemon", "--detached", "--listen", "127.0.0.1:0"],
    );
    assert_eq!(started.status, Some(0), "{started}");
    let listening = Listening(there.to_path_buf());
    let invite = there.join("here.invite");
    let invited = shards_in(
        &there,
        &[
            "invite",
            "client",
            "--name",
            "here",
            "--output",
            invite.to_str().unwrap(),
        ],
    );
    assert_eq!(invited.status, Some(0), "{invited}");
    let joined = shards_in(
        &here,
        &[
            "create",
            "context",
            "lab",
            "--invite-file",
            invite.to_str().unwrap(),
        ],
    );
    assert_eq!(joined.status, Some(0), "{joined}");
    let pulled = shards_in(&here, &["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    Some((here, there, listening, image))
}

fn logs(home: &Path, name: &str) -> String {
    let logs = shards_in(home, &["logs", name]);
    assert_eq!(logs.status, Some(0), "{logs}");
    logs.stdout
}

fn counts(logs: &str) -> Vec<u64> {
    logs.lines().filter_map(|l| l.parse().ok()).collect()
}

fn until(what: &str, done: &mut dyn FnMut() -> bool, said: &dyn Fn() -> String) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "{what}: {}", said());
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The end of a home's daemon log, for a failing test to say.
fn daemon_log(home: &Path) -> String {
    let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
    let lines: Vec<&str> = log.lines().collect();
    lines[lines.len().saturating_sub(40)..].join("\n")
}

/// The counts in a file a guest appends to, a line each: the lines whole, and not the last
/// where its write is not yet.
fn appended(file: &Path) -> Vec<u64> {
    let text = std::fs::read_to_string(file).unwrap_or_default();
    let whole = text.rsplit_once('\n').map_or("", |(whole, _)| whole);
    whole.lines().filter_map(|l| l.parse().ok()).collect()
}

/// The CPU every machine of this one and context `peer` has told this home's microVMs, where
/// `SHARDS_MOVE_BASELINE` asks: for a pair neither of which has every feature of the other
/// (D176 part 6, PM M462).
fn baseline_with_peer(here: &Path) {
    if std::env::var_os("SHARDS_MOVE_BASELINE").is_some() {
        let said = shards_in(here, &["configure", "cpu", "--baseline", "peer"]);
        assert_eq!(said.status, Some(0), "{said}");
    }
}

/// What a guest's /proc/cpuinfo, as testguest's `files` prints it, says of its CPUs: their
/// numbers, identity and features, the lines of those keys alone (not their clocks).
fn cpu_identity(said: &str) -> Vec<String> {
    const KEYS: &[&str] = &[
        "processor",
        "vendor_id",
        "cpu family",
        "model",
        "model name",
        "stepping",
        "flags",
        "Features",
        "CPU implementer",
        "CPU architecture",
        "CPU variant",
        "CPU part",
        "CPU revision",
    ];
    said.lines()
        .filter(|l| l.split_once(':').is_some_and(|(k, _)| KEYS.contains(&k.trim())))
        .map(str::to_string)
        .collect()
}

/// A running microVM moved to another machine goes on there where it was: its counter
/// counts on from the count it had, each count once, its log moved with it; its image found
/// there by its digest, pulled from the registry it came from; and here it is no more. Its
/// volume moves with it, a copy of it there, which its restored server serves by its paths:
/// the guest appends on to the file it holds open, from the directory it works in.
#[test]
fn a_running_microvm_moves_to_another_machine_and_goes_on_there() {
    let Some((here, there, _listening, image)) = machines("moved") else {
        return;
    };
    let run = shards_in(
        &here,
        &[
            "run",
            "-d",
            "--pull",
            "never",
            "--name",
            "counter",
            "-u",
            "0",
            "-v",
            "kept:/kept",
            &image,
            "appender",
            "/kept/counts",
            "20",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let file_here = here.join("volumes/kept/_data/counts");
    let written = |file: &Path| -> Vec<u64> {
        std::fs::read_to_string(file)
            .unwrap_or_default()
            .lines()
            .map(|l| l.parse().unwrap())
            .collect()
    };
    let said_here = || daemon_log(&here);
    until("appending", &mut || written(&file_here).len() >= 10, &said_here);
    let moved = shards_in(&here, &["move", "vm", "counter", "lab"]);
    assert_eq!(
        (moved.status, moved.stdout.as_str()),
        (Some(0), "counter\n"),
        "{moved}\nhere:\n{}\nthere:\n{}",
        daemon_log(&here),
        daemon_log(&there)
    );
    // Gone from here, its volume's copy here as it was at the move.
    let gone = shards_in(&here, &["inspect", "counter"]);
    assert_eq!(gone.status, Some(1), "{gone}");
    let at_move = written(&file_here);
    // Going on there, appending to its volume's copy there.
    let file_there = there.join("volumes/kept/_data/counts");
    let said_there = || format!("{}\n{}", daemon_log(&there), logs(&there, "counter"));
    until(
        "appending there",
        &mut || written(&file_there).len() >= at_move.len() + 10,
        &said_there,
    );
    let counts_there = written(&file_there);
    let wanted: Vec<u64> = (1..=counts_there.len() as u64).collect();
    assert_eq!(
        counts_there,
        wanted,
        "each count once, in order: {}",
        said_there()
    );
    let ps = shards_in(
        &there,
        &["ps", "--filter", "name=counter", "--format", "{{.Status}}"],
    );
    assert!(ps.stdout.starts_with("Up "), "{ps}");
    assert_eq!(shards_in(&there, &["rm", "-f", "counter"]).status, Some(0));
}

/// A microVM's output goes on in its log there from where it was, each count once: its log
/// moved with it. An image no registry the other machine reaches holds goes there too,
/// saved here and loaded there, by its digest.
#[test]
fn a_moved_microvms_log_and_image_go_with_it() {
    let Some((here, there, _listening, image)) = machines("moved-log") else {
        return;
    };
    // A name no registry holds: the other machine cannot pull it.
    let local = "127.0.0.1:1/only/here:v1";
    assert_eq!(shards_in(&here, &["tag", &image, local]).status, Some(0));
    let run = shards_in(
        &here,
        &[
            "run", "-d", "--pull", "never", "--name", "counter", local, "counter", "20",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let log_here = || logs(&here, "counter");
    until("counting", &mut || counts(&log_here()).len() >= 10, &log_here);
    let moved = shards_in(&here, &["move", "vm", "counter", "lab"]);
    assert_eq!(
        (moved.status, moved.stdout.as_str()),
        (Some(0), "counter\n"),
        "{moved}\nhere:\n{}\nthere:\n{}",
        daemon_log(&here),
        daemon_log(&there)
    );
    let log_there = || logs(&there, "counter");
    let n = counts(&log_there()).len();
    until(
        "counting there",
        &mut || counts(&log_there()).len() >= n + 10,
        &log_there,
    );
    let all = counts(&log_there());
    assert_eq!(
        all,
        (1..=all.len() as u64).collect::<Vec<_>>(),
        "each count once, from here on to there"
    );
    // Its image is there now, by its name here.
    let inspected = shards_in(&there, &["image", "inspect", "-f", "{{.Id}}", local]);
    let id_here = shards_in(&here, &["image", "inspect", "-f", "{{.Id}}", local]);
    assert_eq!(inspected.stdout, id_here.stdout, "{inspected}");
    assert_eq!(shards_in(&there, &["rm", "-f", "counter"]).status, Some(0));
}

/// A move the other machine cannot take leaves the microVM here as it was, running on:
/// another microVM there has its name.
#[test]
fn a_move_refused_leaves_the_microvm_running_here() {
    let Some((here, there, _listening, image)) = machines("unmoved") else {
        return;
    };
    let taken = shards_in(&there, &["create", "--name", "counter", &image, "counter", "20"]);
    assert_eq!(taken.status, Some(0), "{taken}");
    let run = shards_in(
        &here,
        &[
            "run", "-d", "--pull", "never", "--name", "counter", &image, "counter", "20",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let log = || logs(&here, "counter");
    until("counting", &mut || counts(&log()).len() >= 5, &log);
    let refused = shards_in(&here, &["move", "vm", "counter", "lab"]);
    assert_eq!(refused.status, Some(1), "{refused}");
    assert!(
        refused.stderr.contains("a microVM named counter is here already")
            && refused.stderr.contains("counter goes on here"),
        "{refused}"
    );
    let n = counts(&log()).len();
    until("counting on here", &mut || counts(&log()).len() >= n + 5, &log);
    let all = counts(&log());
    assert_eq!(all, (1..=all.len() as u64).collect::<Vec<_>>());
    assert_eq!(shards_in(&here, &["rm", "-f", "counter"]).status, Some(0));
}

/// A microVM moved saved keeps what its move checks, each as D176 part 6 has it:
///
/// - its CPU: the other machine's has every feature of this one's (here, the same CPU), and
///   its guest's CPUs are there what they were here;
/// - its image, there by the digest it was made of, pulled from its registry by that
///   digest, though its name there tags another image;
/// - its files: its directory with the snapshot it moves as and none other (one it was
///   saved as before stays here), and of the files that snapshot restores against those the
///   other machine lacked, each kept in its directory there, and no other;
/// - its connections, dialed again by its init there: an exec runs in it, and a stop's
///   SIGTERM ends it, well inside its timeout.
#[test]
fn a_microvm_moved_saved_keeps_its_cpu_image_files_and_connections() {
    let Some((here, there, _listening, image)) = machines("moved-checks") else {
        return;
    };
    let field = |home: &Path, args: &[&str]| {
        let said = shards_in(home, args);
        assert_eq!(said.status, Some(0), "{args:?}: {said}");
        said.stdout.trim().to_string()
    };
    // There, its image's name tags another image.
    let other = common::served_variant(b"another");
    let pulled = shards_in(&there, &["pull", "-q", &other]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let tagged = shards_in(&there, &["tag", &other, &image]);
    assert_eq!(tagged.status, Some(0), "{tagged}");
    let other_id = field(&there, &["image", "inspect", "-f", "{{.Id}}", &image]);
    // Under --init, so that its command ends at a stop's SIGTERM.
    let run = shards_in(
        &here,
        &[
            "run", "-d", "--init", "--pull", "never", "--name", "mover", &image, "counter", "20",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let image_id = field(&here, &["inspect", "-f", "{{.Image}}", "mover"]);
    assert_ne!(image_id, other_id);
    let log_here = || logs(&here, "mover");
    until("counting", &mut || counts(&log_here()).len() >= 10, &log_here);
    let cpu = |home: &Path| {
        cpu_identity(&field(
            home,
            &["exec", "mover", "/bin/testguest", "files", "/proc/cpuinfo"],
        ))
    };
    let cpu_here = cpu(&here);
    assert!(
        cpu_here.iter().any(|l| l.starts_with("processor")),
        "{cpu_here:?}"
    );
    let early = shards_in(&here, &["save", "vm", "mover", "early", "--leave-running"]);
    assert_eq!(early.status, Some(0), "{early}");

    let (kernel, init) = (kernel(), guest_init());
    let timed: [(&str, &OsStr); 4] = [
        ("SHARDS_HOME", here.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
        ("SHARDS_MOVE_TIMES", OsStr::new("1")),
    ];
    let moved = run_shards_env(&[], &["move", "vm", "mover", "lab"], &timed, TIMEOUT);
    assert_eq!(
        (moved.status, moved.stdout.as_str()),
        (Some(0), "mover\n"),
        "{moved}\nhere:\n{}\nthere:\n{}",
        daemon_log(&here),
        daemon_log(&there)
    );
    let gone = shards_in(&here, &["inspect", "mover"]);
    assert_eq!(gone.status, Some(1), "{gone}");
    // Counting on there, each count once, its log with it.
    let log_there = || logs(&there, "mover");
    let n = counts(&log_there()).len();
    until(
        "counting there",
        &mut || counts(&log_there()).len() >= n + 10,
        &log_there,
    );
    let all = counts(&log_there());
    assert_eq!(
        all,
        (1..=all.len() as u64).collect::<Vec<_>>(),
        "each count once, from here on to there"
    );

    // Its image, by its digest.
    assert_eq!(field(&there, &["inspect", "-f", "{{.Image}}", "mover"]), image_id);
    assert_eq!(
        field(&there, &["image", "inspect", "-f", "{{.Id}}", &image_id]),
        image_id,
        "pulled there by its digest"
    );

    // Its snapshot, the one it moved as alone; the files there lacked, and only those.
    let listed = field(&there, &["list", "snapshots", "mover"]);
    let names: Vec<&str> = listed
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    assert!(names.len() == 1 && names[0] != "early", "{listed}");
    let (sent, files) = moved
        .stderr
        .lines()
        .find_map(|l| {
            let (sent, rest) = l.strip_prefix("move: ")?.split_once(" of its ")?;
            let (files, _) = rest.split_once(" files sent")?;
            Some((sent.parse::<usize>().ok()?, files.parse::<usize>().ok()?))
        })
        .unwrap_or_else(|| panic!("no files said: {moved}"));
    assert!(files >= 1 && sent <= files, "{moved}");
    let id = field(&there, &["inspect", "-f", "{{.Id}}", "mover"]);
    let backing = there.join("containers").join(&id).join("backing");
    let kept = std::fs::read_dir(&backing).map_or(0, Iterator::count);
    assert_eq!(kept, sent, "{}: {moved}", backing.display());

    // Its connections, dialed again: an exec, and a stop's signal.
    assert_eq!(cpu(&there), cpu_here, "its CPUs, as its guest sees them");
    let asked = Instant::now();
    let stopped = shards_in(&there, &["stop", "-t", "30", "mover"]);
    let took = asked.elapsed();
    assert_eq!(stopped.status, Some(0), "{stopped}");
    let code = field(&there, &["inspect", "-f", "{{.State.ExitCode}}", "mover"]);
    assert_eq!(
        (code.as_str(), took < Duration::from_secs(30)),
        ("143", true),
        "stopped in {took:?}\n{}",
        daemon_log(&there)
    );
    assert_eq!(shards_in(&there, &["rm", "-f", "mover"]).status, Some(0));
}

/// A volume two microVMs here share moves with the first moved, a copy of it there that it
/// appends on to, each count once, the other's file in it as it was; and the other stays:
/// its move refused there, which has a volume of the name now, it appends on here to the
/// volume here, each count once.
#[test]
fn a_volume_shared_here_moves_with_one_and_the_other_goes_on_here() {
    let Some((here, there, _listening, image)) = machines("moved-shared") else {
        return;
    };
    for name in ["first", "second"] {
        let file = format!("/kept/{name}");
        let run = shards_in(
            &here,
            &[
                "run",
                "-d",
                "--pull",
                "never",
                "--name",
                name,
                "-u",
                "0",
                "-v",
                "shared:/kept",
                &image,
                "appender",
                &file,
                "20",
            ],
        );
        assert_eq!(run.status, Some(0), "{run}");
    }
    let data = |home: &Path, file: &str| home.join("volumes/shared/_data").join(file);
    let said_here = || daemon_log(&here);
    until(
        "appending",
        &mut || appended(&data(&here, "first")).len() >= 10 && appended(&data(&here, "second")).len() >= 10,
        &said_here,
    );
    let moved = shards_in(&here, &["move", "vm", "first", "lab"]);
    assert_eq!(
        (moved.status, moved.stdout.as_str()),
        (Some(0), "first\n"),
        "{moved}\nhere:\n{}\nthere:\n{}",
        daemon_log(&here),
        daemon_log(&there)
    );
    let at_move = appended(&data(&here, "first")).len();
    until(
        "appending there",
        &mut || appended(&data(&there, "first")).len() >= at_move + 10,
        &|| daemon_log(&there),
    );
    let first = appended(&data(&there, "first"));
    assert_eq!(
        first,
        (1..=first.len() as u64).collect::<Vec<_>>(),
        "each count once, in order"
    );
    assert!(
        data(&there, "second").is_file(),
        "the second's file, moved with the volume"
    );

    let refused = shards_in(&here, &["move", "vm", "second", "lab"]);
    assert_eq!(refused.status, Some(1), "{refused}");
    assert!(
        refused.stderr.contains("a volume named shared is here already")
            && refused.stderr.contains("second goes on here"),
        "{refused}"
    );
    let n = appended(&data(&here, "second")).len();
    until(
        "appending on here",
        &mut || appended(&data(&here, "second")).len() >= n + 10,
        &said_here,
    );
    let second = appended(&data(&here, "second"));
    assert_eq!(
        second,
        (1..=second.len() as u64).collect::<Vec<_>>(),
        "each count once, in order"
    );
    assert_eq!(
        appended(&data(&here, "first")).len(),
        at_move,
        "the first's file here as it was moved"
    );
    let kept = shards_in(&here, &["volume", "inspect", "shared"]);
    assert_eq!(kept.status, Some(0), "{kept}");
    assert_eq!(shards_in(&here, &["rm", "-f", "second"]).status, Some(0));
    assert_eq!(shards_in(&there, &["rm", "-f", "first"]).status, Some(0));
}

/// A microVM's published port goes with it (D176 part 6): its guest serving on port 7000,
/// published on a host port its home picks (`-p 7000`), answers on the other home at the port
/// that home bound for it as its restore started it, as it answered here. Two homes of one
/// machine share its host ports, so each picks its own here; a fixed one is the two-host
/// test's.
#[test]
fn a_moved_microvms_published_port_answers_there() {
    use std::io::{Read as _, Write as _};
    let Some((here, there, _listening, image)) = machines("ports") else {
        return;
    };
    let run = shards_in(
        &here,
        &[
            "run", "-d", "--pull", "never", "--name", "web", "-p", "7000", &image, "serve", "7000", "2",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let log_here = || logs(&here, "web");
    until("serving", &mut || log_here().contains("ready"), &log_here);
    let ask = |home: &Path| {
        let listed = shards_in(home, &["port", "web", "7000"]);
        let port: u16 = listed
            .stdout
            .lines()
            .find_map(|l| l.strip_prefix("0.0.0.0:"))
            .and_then(|p| p.trim().parse().ok())
            .unwrap_or_else(|| panic!("{listed}"));
        let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.set_read_timeout(Some(TIMEOUT)).unwrap();
        c.write_all(b"hello").unwrap();
        c.shutdown(std::net::Shutdown::Write).unwrap();
        let mut got = String::new();
        c.read_to_string(&mut got).unwrap();
        got
    };
    let answered = ask(&here);
    assert!(
        answered.starts_with("from ") && answered.ends_with("\nhello"),
        "{answered}"
    );
    let moved = shards_in(&here, &["move", "vm", "web", "lab"]);
    assert_eq!(
        (moved.status, moved.stdout.as_str()),
        (Some(0), "web\n"),
        "{moved}\nhere:\n{}\nthere:\n{}",
        daemon_log(&here),
        daemon_log(&there)
    );
    let answered = ask(&there);
    assert!(
        answered.starts_with("from ") && answered.ends_with("\nhello"),
        "{answered}"
    );
    assert_eq!(shards_in(&there, &["rm", "-f", "web"]).status, Some(0));
}

/// `shards move vm --live NAME lab` with what it reports of its rounds and its downtime
/// (`SHARDS_MOVE_TIMES`): the run, and the downtime in µs where it said one.
fn move_live(here: &Path, name: &str) -> (Run, Option<u64>) {
    move_live_to(here, name, "lab")
}

/// [`move_live`], to context `to`.
fn move_live_to(here: &Path, name: &str, to: &str) -> (Run, Option<u64>) {
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 4] = [
        ("SHARDS_HOME", here.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
        ("SHARDS_MOVE_TIMES", OsStr::new("1")),
    ];
    let moved = run_shards_env(&[], &["move", "vm", "--live", name, to], &env, TIMEOUT);
    let downtime = moved.stderr.lines().find_map(|l| {
        l.strip_prefix("move: downtime ")?
            .strip_suffix(" us")?
            .parse()
            .ok()
    });
    (moved, downtime)
}

/// A running microVM moved live goes on there where it was, as it ran on here while its
/// memory was copied: its counter counts on, each count once, through rounds that sent what
/// it wrote as it ran; held here only for its last save, the time it ran nowhere said. A
/// move the other machine cannot take leaves it running here, its last save let go.
#[test]
fn a_running_microvm_moves_live_and_goes_on_there() {
    let Some((here, there, _listening, image)) = machines("live") else {
        return;
    };
    let run = shards_in(
        &here,
        &[
            "run", "-d", "--pull", "never", "--name", "counter", &image, "counter", "20",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let log_here = || logs(&here, "counter");
    until("counting", &mut || counts(&log_here()).len() >= 10, &log_here);
    let (moved, downtime) = move_live(&here, "counter");
    assert_eq!(
        (moved.status, moved.stdout.as_str()),
        (Some(0), "counter\n"),
        "{moved}\nhere:\n{}\nthere:\n{}",
        daemon_log(&here),
        daemon_log(&there)
    );
    assert!(moved.stderr.contains("move: round 0: "), "{moved}");
    let downtime = downtime.unwrap_or_else(|| panic!("no downtime said: {moved}"));
    eprintln!("downtime {downtime} us");
    let gone = shards_in(&here, &["inspect", "counter"]);
    assert_eq!(gone.status, Some(1), "{gone}");
    let log_there = || logs(&there, "counter");
    let n = counts(&log_there()).len();
    until(
        "counting there",
        &mut || counts(&log_there()).len() >= n + 10,
        &log_there,
    );
    let all = counts(&log_there());
    assert_eq!(
        all,
        (1..=all.len() as u64).collect::<Vec<_>>(),
        "each count once, from here on to there"
    );
    assert_eq!(shards_in(&there, &["rm", "-f", "counter"]).status, Some(0));

    // Refused there: it runs on here, as it was.
    let taken = shards_in(&there, &["create", "--name", "busy", &image, "counter", "20"]);
    assert_eq!(taken.status, Some(0), "{taken}");
    let run = shards_in(
        &here,
        &[
            "run", "-d", "--pull", "never", "--name", "busy", &image, "counter", "20",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let log = || logs(&here, "busy");
    until("counting", &mut || counts(&log()).len() >= 5, &log);
    let (refused, _) = move_live(&here, "busy");
    assert_eq!(refused.status, Some(1), "{refused}");
    assert!(refused.stderr.contains("busy goes on here"), "{refused}");
    let n = counts(&log()).len();
    until("counting on here", &mut || counts(&log()).len() >= n + 5, &log);
    assert_eq!(shards_in(&here, &["rm", "-f", "busy"]).status, Some(0));
}

/// A microVM writing its memory as fast as its workload stamps it moves live with none of
/// it lost: every page holds its last write there, its guest checking each after every
/// sweep (testguest's `scribble`), as the rounds chased its writes and its last save held
/// what they had not.
#[test]
fn a_microvm_writing_its_memory_moves_live_with_none_of_it_lost() {
    let Some((here, there, _listening, image)) = machines("live-mem") else {
        return;
    };
    let run = shards_in(
        &here,
        &[
            "run",
            "-d",
            "--pull",
            "never",
            "--name",
            "scribbler",
            &image,
            "scribble",
            "64",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let log_here = || logs(&here, "scribbler");
    until("sweeping", &mut || counts(&log_here()).len() >= 2, &log_here);
    let (moved, downtime) = move_live(&here, "scribbler");
    assert_eq!(
        (moved.status, moved.stdout.as_str()),
        (Some(0), "scribbler\n"),
        "{moved}\nhere:\n{}\nthere:\n{}",
        daemon_log(&here),
        daemon_log(&there)
    );
    eprintln!("downtime {downtime:?} us: {}", moved.stderr);
    let log_there = || logs(&there, "scribbler");
    let n = counts(&log_there()).len();
    until(
        "sweeping there",
        &mut || counts(&log_there()).len() >= n + 3 || log_there().contains("BAD"),
        &log_there,
    );
    let said = log_there();
    assert!(!said.contains("BAD"), "a page lost its last write: {said}");
    let all = counts(&said);
    assert_eq!(all, (1..=all.len() as u64).collect::<Vec<_>>(), "{said}");
    assert_eq!(shards_in(&there, &["rm", "-f", "scribbler"]).status, Some(0));
}

/// What a test moved to the other machine, removed there as it drops, the test passing or
/// failing: microVMs and volumes of names its own.
struct LeftThere<'a> {
    here: &'a Path,
    context: &'a str,
    microvms: Vec<String>,
    volumes: Vec<String>,
}

impl Drop for LeftThere<'_> {
    fn drop(&mut self) {
        for name in &self.microvms {
            let _ = shards_in(self.here, &["--context", self.context, "rm", "-f", name]);
        }
        for name in &self.volumes {
            let _ = shards_in(
                self.here,
                &["--context", self.context, "volume", "rm", "-f", name],
            );
        }
    }
}

/// The CPU baseline a home's microVMs are told (D176 part 6) is all a guest finds, and goes
/// with it: told ID_AA64PFR1_EL1 of nothing, a guest finds no SSBS and no BTI, whatever its
/// host has; moved, it finds the same there, the guest's CPU, not that machine's.
#[cfg(target_arch = "aarch64")]
#[test]
fn a_cpu_baseline_is_all_its_guest_finds_and_goes_with_it() {
    let Some((here, there, _listening, image)) = machines("baseline") else {
        return;
    };
    let field = |home: &Path, args: &[&str]| {
        let said = shards_in(home, args);
        assert_eq!(said.status, Some(0), "{args:?}: {said}");
        said.stdout.trim().to_string()
    };
    let cpu = |home: &Path, name: &str| {
        cpu_identity(&field(
            home,
            &["exec", name, "/bin/testguest", "files", "/proc/cpuinfo"],
        ))
    };
    let features = |cpu: &[String]| {
        cpu.iter()
            .find_map(|l| l.strip_prefix("Features"))
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let start = |name: &str| {
        let run = shards_in(
            &here,
            &[
                "run", "-d", "--pull", "never", "--name", name, &image, "counter", "20",
            ],
        );
        assert_eq!(run.status, Some(0), "{run}");
        let log_here = || logs(&here, name);
        until("counting", &mut || counts(&log_here()).len() >= 3, &log_here);
    };
    start("host");
    let hosts = features(&cpu(&here, "host"));
    assert_eq!(shards_in(&here, &["rm", "-f", "host"]).status, Some(0));
    // Hypervisor.framework shows a guest no WFxT or RPRES, ID_AA64ISAR2_EL1's, whatever
    // macOS names (PM M499): a move checks neither by name (daemon/moves.rs `host_cpu`).
    if cfg!(target_os = "macos") {
        for hidden in ["wfxt", "rpres"] {
            assert!(!hosts.iter().any(|f| f == hidden), "{hidden}: {hosts:?}");
        }
    }

    std::fs::write(here.join("cpu-baseline"), "c021=0000000000000000\nlab\n").unwrap();
    let inspected = field(&here, &["inspect", "cpu"]);
    assert!(inspected.contains("c021=0000000000000000"), "{inspected}");
    start("told");
    let told = cpu(&here, "told");
    let found = features(&told);
    assert!(!found.is_empty(), "{told:?}");
    for masked in ["ssbs", "bti"] {
        assert!(!found.iter().any(|f| f == masked), "{masked}: {found:?}");
    }
    // Where this host has either, the baseline took it away.
    if hosts.iter().any(|f| f == "ssbs" || f == "bti") {
        assert_ne!(found, hosts);
    }
    let (kernel, init) = (kernel(), guest_init());
    let timed: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", here.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    let moved = run_shards_env(&[], &["move", "vm", "told", "lab"], &timed, TIMEOUT);
    assert_eq!(moved.status, Some(0), "{moved}");
    assert_eq!(cpu(&there, "told"), told);
    assert_eq!(shards_in(&there, &["rm", "-f", "told"]).status, Some(0));
}

/// The CPU baseline a home's microVMs are told on x86-64 (D176 part 6, D245): CPUID's
/// feature leaves, which `configure cpu` meets across the machines its contexts reach, and
/// which go with a microVM moved. Told this machine's own CPU less AVX2 and SHA (leaf 7
/// EBX bits 5 and 29), a guest's /proc/cpuinfo names neither, whatever its host has; moved,
/// it finds the same there. KVM's: WHP tells no baseline (hv::whp restores a snapshot only
/// on the CPU it was taken on).
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
#[test]
fn an_x86_cpu_baseline_is_all_its_guest_finds_and_goes_with_it() {
    let Some((here, there, _listening, image)) = machines("base86") else {
        return;
    };
    let field = |home: &Path, args: &[&str]| {
        let said = shards_in(home, args);
        assert_eq!(said.status, Some(0), "{args:?}: {said}");
        said.stdout.trim().to_string()
    };
    let cpu = |home: &Path, name: &str| {
        cpu_identity(&field(
            home,
            &["exec", name, "/bin/testguest", "files", "/proc/cpuinfo"],
        ))
    };
    let flags = |cpu: &[String]| {
        cpu.iter()
            .find_map(|l| l.strip_prefix("flags"))
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let start = |name: &str| {
        let run = shards_in(
            &here,
            &[
                "run", "-d", "--pull", "never", "--name", name, &image, "counter", "20",
            ],
        );
        assert_eq!(run.status, Some(0), "{run}");
        let log_here = || logs(&here, name);
        until("counting", &mut || counts(&log_here()).len() >= 3, &log_here);
    };
    start("host");
    let hosts = flags(&cpu(&here, "host"));
    assert!(!hosts.is_empty(), "a guest's CPU names its features");
    assert_eq!(shards_in(&here, &["rm", "-f", "host"]).status, Some(0));

    // This machine's CPU met with the other home's, its own: then less AVX2 and SHA.
    let _ = field(&here, &["configure", "cpu", "--baseline", "lab"]);
    let file = here.join("cpu-baseline");
    let text = std::fs::read_to_string(&file).unwrap();
    let (line, of) = text.split_once('\n').unwrap();
    assert_eq!(of.trim(), "lab", "{text}");
    assert!(line.starts_with("0.0="), "leaf 0, its vendor, first: {line}");
    let told_line: Vec<String> = line
        .split(',')
        .map(|leaf| match leaf.strip_prefix("7.0=") {
            Some(regs) => {
                let mut r: Vec<u32> = regs
                    .split(':')
                    .map(|v| u32::from_str_radix(v, 16).unwrap())
                    .collect();
                r[1] &= !(1 << 5 | 1 << 29);
                format!("7.0={:08x}:{:08x}:{:08x}:{:08x}", r[0], r[1], r[2], r[3])
            }
            None => leaf.to_string(),
        })
        .collect();
    let told_line = told_line.join(",");
    std::fs::write(&file, format!("{told_line}\nlab\n")).unwrap();
    let inspected = field(&here, &["inspect", "cpu"]);
    assert!(inspected.contains(&told_line), "{inspected}");
    start("told");
    let told = cpu(&here, "told");
    let found = flags(&told);
    assert!(!found.is_empty(), "{told:?}");
    for masked in ["avx2", "sha_ni"] {
        assert!(!found.iter().any(|f| f == masked), "{masked}: {found:?}");
    }
    // Where this host has either, the baseline took it away; the rest are the host's.
    if hosts.iter().any(|f| f == "avx2" || f == "sha_ni") {
        assert_ne!(found, hosts);
    }
    assert!(
        found.iter().all(|f| hosts.contains(f)),
        "{found:?} within {hosts:?}"
    );
    let (kernel, init) = (kernel(), guest_init());
    let timed: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", here.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    let moved = run_shards_env(&[], &["move", "vm", "told", "lab"], &timed, TIMEOUT);
    assert_eq!(moved.status, Some(0), "{moved}");
    assert_eq!(cpu(&there, "told"), told);
    assert_eq!(shards_in(&there, &["rm", "-f", "told"]).status, Some(0));
}

/// A microVM moved to another machine, the one whose invitation `SHARDS_MOVE_INVITE` names
/// (a file: two-hosts.sh `invite`, run there, makes one), as context `peer`; the test image
/// pulled here alone, from this machine's own loopback registry, which the other cannot
/// reach: its image goes with the move, saved here and loaded there, and so do the files its
/// snapshot restores against that the other has not alike (its kernel and init among them,
/// where that machine has none at their paths).
///
/// Saved: a counter appending to a file it holds open in a volume goes on there from its
/// count, each count once, in order, its volume and log with it, and is gone from here.
/// Live: a guest stamping 16 MiB page by page goes on sweeping there, every page holding its
/// last write, the time it ran nowhere said. 16, not 64 as between homes: it rewrites them
/// at 100 MiB/s, which a network between machines may not carry, so that its rounds end as
/// they no longer shrink (`live::Stop`), and what they send then is the size of what it
/// stamps.
#[test]
fn a_microvm_moves_to_another_host_saved_and_live() {
    let Some(invite) = std::env::var_os("SHARDS_MOVE_INVITE") else {
        eprintln!("SKIP: SHARDS_MOVE_INVITE names no other machine's invitation (two-hosts.sh invite)");
        return;
    };
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let (image, _) = served();
    let here = TempDir::new("peer");
    let invite = PathBuf::from(invite);
    let joined = shards_in(
        &here,
        &[
            "create",
            "context",
            "peer",
            "--invite-file",
            invite.to_str().unwrap(),
        ],
    );
    assert_eq!(joined.status, Some(0), "{joined}");
    baseline_with_peer(&here);
    let pulled = shards_in(&here, &["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let id = std::process::id();
    let (counter, volume, scribbler) = (format!("cold{id}"), format!("kept{id}"), format!("live{id}"));
    let _left = LeftThere {
        here: &here,
        context: "peer",
        microvms: vec![counter.clone(), scribbler.clone()],
        volumes: vec![volume.clone()],
    };
    let logs_there = |name: &str| shards_in(&here, &["--context", "peer", "logs", name]).stdout;
    let moved_ok = |moved: &Run, name: &str| {
        assert_eq!(
            (moved.status, moved.stdout.as_str()),
            (Some(0), format!("{name}\n").as_str()),
            "{moved}\nhere:\n{}",
            daemon_log(&here)
        );
        let gone = shards_in(&here, &["inspect", name]);
        assert_eq!(gone.status, Some(1), "{gone}");
    };

    // Saved.
    let mount = format!("{volume}:/kept");
    let run = shards_in(
        &here,
        &[
            "run",
            "-d",
            "--pull",
            "never",
            "--name",
            &counter,
            "-u",
            "0",
            "-v",
            &mount,
            &image,
            "appender",
            "/kept/counts",
            "20",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let log_here = || logs(&here, &counter);
    until("appending", &mut || counts(&log_here()).len() >= 10, &log_here);
    let (kernel, init) = (kernel(), guest_init());
    let timed: [(&str, &OsStr); 4] = [
        ("SHARDS_HOME", here.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
        ("SHARDS_MOVE_TIMES", OsStr::new("1")),
    ];
    let moved = run_shards_env(&[], &["move", "vm", &counter, "peer"], &timed, TIMEOUT);
    moved_ok(&moved, &counter);
    eprintln!("saved move: {}", moved.stderr);
    let n = counts(&logs_there(&counter)).len();
    until(
        "appending there",
        &mut || counts(&logs_there(&counter)).len() >= n + 10,
        &|| logs_there(&counter),
    );
    let all = counts(&logs_there(&counter));
    assert_eq!(
        all,
        (1..=all.len() as u64).collect::<Vec<_>>(),
        "each count once, in order, from here on to there"
    );
    let ps = shards_in(
        &here,
        &[
            "--context",
            "peer",
            "ps",
            "--filter",
            &format!("name={counter}"),
            "--format",
            "{{.Status}}",
        ],
    );
    assert!(ps.stdout.starts_with("Up "), "{ps}");

    // Live.
    let run = shards_in(
        &here,
        &[
            "run", "-d", "--pull", "never", "--name", &scribbler, &image, "scribble", "16",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let log_here = || logs(&here, &scribbler);
    until("sweeping", &mut || counts(&log_here()).len() >= 2, &log_here);
    let (moved, downtime) = move_live_to(&here, &scribbler, "peer");
    moved_ok(&moved, &scribbler);
    assert!(moved.stderr.contains("move: round 0: "), "{moved}");
    let downtime = downtime.unwrap_or_else(|| panic!("no downtime said: {moved}"));
    eprintln!("live move: downtime {downtime} us; {}", moved.stderr);
    let n = counts(&logs_there(&scribbler)).len();
    until(
        "sweeping there",
        &mut || {
            let said = logs_there(&scribbler);
            counts(&said).len() >= n + 3 || said.contains("BAD")
        },
        &|| logs_there(&scribbler),
    );
    let said = logs_there(&scribbler);
    assert!(!said.contains("BAD"), "a page lost its last write: {said}");
    let all = counts(&said);
    assert_eq!(all, (1..=all.len() as u64).collect::<Vec<_>>(), "{said}");
}

/// A volume's guest owners move with it (D176 part 6; the parity audit's row 14), whatever
/// hosts the two machines are, each keeping them its own way (D38's owner attribute on
/// Unix, WSL's metadata on Windows, D197): files a guest gave owners and groups, modes (one
/// of 0), an extended attribute, a symlink, a device node and a name no Windows name holds
/// show a guest there what they showed one here, as `tree` lists them. To the machine
/// whose invitation `SHARDS_MOVE_INVITE` names (a Windows one, or a Unix one from Windows),
/// as context `peer`; without one, between two homes here.
#[test]
fn a_volumes_guest_owners_move_with_it() {
    let invite = std::env::var_os("SHARDS_MOVE_INVITE");
    let (here, _two, image, context) = match &invite {
        Some(invite) => {
            if cannot_run_vms() || cannot_pool() {
                return;
            }
            let (image, _) = served();
            let here = TempDir::new("owners");
            let joined = shards_in(
                &here,
                &[
                    "create",
                    "context",
                    "peer",
                    "--invite-file",
                    invite.to_str().unwrap(),
                ],
            );
            assert_eq!(joined.status, Some(0), "{joined}");
            baseline_with_peer(&here);
            let pulled = shards_in(&here, &["pull", "-q", &image]);
            assert_eq!(pulled.status, Some(0), "{pulled}");
            (here, None, image, "peer")
        }
        None => {
            let Some((here, there, listening, image)) = machines("owners") else {
                return;
            };
            (here, Some((there, listening)), image, "lab")
        }
    };
    let id = std::process::id();
    let (vm, volume) = (format!("own{id}"), format!("owners{id}"));
    let _left = LeftThere {
        here: &here,
        context,
        microvms: vec![vm.clone()],
        volumes: vec![volume.clone()],
    };
    let mount = format!("{volume}:/kept");
    let run = shards_in(
        &here,
        &[
            "run", "-d", "--pull", "never", "--name", &vm, "-u", "0", "-v", &mount, &image, "sleep",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let made = shards_in(
        &here,
        &[
            "exec",
            &vm,
            "/bin/testguest",
            "fs",
            "mkdir:/kept/d",
            "write:/kept/d/f=hi",
            "chown:1000:1001:/kept/d/f",
            "chmod:640:/kept/d/f",
            "xattr:user.k=v:/kept/d/f",
            "write:/kept/d/zero=",
            "chmod:0:/kept/d/zero",
            "write:/kept/d/a:b=colon",
            "symlink:f:/kept/d/l",
            "mknod:c:1:3:/kept/d/null",
            "chown:1234:5678:/kept/d",
            "chmod:750:/kept/d",
        ],
    );
    assert_eq!(made.status, Some(0), "{made}");
    let tree = |args: &[&str]| {
        let mut all: Vec<&str> = args.to_vec();
        all.extend(["exec", vm.as_str(), "/bin/testguest", "tree", "/kept"]);
        let listed = shards_in(&here, &all);
        assert_eq!(listed.status, Some(0), "{listed}");
        listed.stdout
    };
    let before = tree(&[]);
    for line in [
        "d dir 750 1234:5678",
        "d/f file 640 1000:1001 2 = hi",
        "d/zero file 0 0:0",
        "d/null char",
        "d/l symlink",
        "d/a:b file",
    ] {
        assert!(before.contains(line), "{line}: {before}");
    }
    let moved = shards_in(&here, &["move", "vm", &vm, context]);
    assert_eq!(
        (moved.status, moved.stdout.as_str()),
        (Some(0), format!("{vm}\n").as_str()),
        "{moved}\nhere:\n{}",
        daemon_log(&here)
    );
    assert_eq!(
        tree(&["--context", context]),
        before,
        "the volume there, as its guest sees it"
    );
}

/// The downtime of live moves, measured (PM, D176 part 7): `SHARDS_MOVES` microVMs, each
/// writing memory as it counts, moved live from here to there and back, one at a time; each
/// move's downtime, from its last save asked to its restore there said done, by this
/// machine's clock, in µs, with n, p50, p90, p99 and max. Ignored: run by the harness
/// (docs/research/measurements/migrate).
#[test]
#[ignore = "a measurement, run by docs/research/measurements/migrate"]
fn live_move_downtime() {
    let Some((here, there, _listening, image)) = machines("live-n") else {
        return;
    };
    let n: usize = std::env::var("SHARDS_MOVES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(20);
    let mut downtimes = Vec::new();
    for i in 0..n {
        let name = format!("m{i}");
        let run = shards_in(
            &here,
            &[
                "run", "-d", "--pull", "never", "--name", &name, &image, "counter", "20",
            ],
        );
        assert_eq!(run.status, Some(0), "{run}");
        let log = || logs(&here, &name);
        until("counting", &mut || counts(&log()).len() >= 5, &log);
        let (moved, downtime) = move_live(&here, &name);
        assert_eq!(moved.status, Some(0), "{moved}");
        let rounds: Vec<&str> = moved
            .stderr
            .lines()
            .filter(|l| l.starts_with("move: ") && !l.starts_with("move: saved at"))
            .collect();
        eprintln!("move {i}: {downtime:?} us; {rounds:?}");
        downtimes.extend(downtime);
        assert_eq!(shards_in(&there, &["rm", "-f", &name]).status, Some(0));
    }
    downtimes.sort_unstable();
    let at = |q: f64| downtimes[((downtimes.len() as f64 - 1.0) * q).round() as usize];
    eprintln!(
        "live move downtime (us): n {} p50 {} p90 {} p99 {} max {}",
        downtimes.len(),
        at(0.5),
        at(0.9),
        at(0.99),
        downtimes.last().copied().unwrap_or(0)
    );
}
