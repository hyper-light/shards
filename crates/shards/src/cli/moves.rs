//! `shards move vm NAME CONTEXT` (D176 part 6): a microVM moved to the machine a context
//! reaches (D180), driven here as `shards copy vm` is, each step asked of this machine's daemon
//! or of the other's through the context (daemon/moves.rs):
//!
//! 1. saved whole here, stopped where it is (D176): its plan;
//! 2. the other machine asked whether it can take it, and what it lacks; its image, where
//!    that machine has none and pulls none by its digest, saved here and loaded there;
//! 3. its directory, snapshot, volumes and the files the other lacks streamed from this
//!    daemon to that one, through no file;
//! 4. resumed there from its snapshot, where it was saved;
//! 5. removed here.
//!
//! What fails before it runs there leaves it here as it was, resumed; what was made there
//! of it is removed.

use std::io::{self, Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use shards_cmdline::flags::Parsed;
use shards_remote::frame::Request;
use shards_remote::net::Seen;

use crate::cli::cp::Ask;

/// What a step on the other machine said: its status, its stdout and its stderr.
type There = (u8, Vec<u8>, Vec<u8>);

/// A step asked of the other machine, as `context::exec` says it: answered
/// (`Ok(Ok)`); refused there, in its daemon's words (`Ok(Err)`); or unanswered, its
/// connection ended first or never made (`Err`).
type Asked = Result<Result<There, String>, String>;

/// A step asked of the other machine: its arguments and what it reads, if anything.
type AskThere<'a> = dyn Fn(Vec<String>, Option<Box<dyn Read + Send>>) -> Result<There, String> + 'a;

/// What a step writes, sent to its taker as it comes, taken whole once the step is through.
struct Taken(std::sync::mpsc::Sender<Vec<u8>>);

impl Write for Taken {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let _ = self.0.send(bytes.to_vec());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn taken(rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
    rx.try_iter().flatten().collect()
}

/// The fields of a plan the client reads (daemon/moves.rs `Plan`).
#[derive(serde::Deserialize)]
struct Plan {
    name: String,
    #[serde(default)]
    image: Option<String>,
    image_id: String,
    snapshot: String,
    files: Vec<PlanFile>,
}

/// A file a plan names, by its name relative to its home.
#[derive(serde::Deserialize)]
struct PlanFile {
    rel: String,
}

/// What the other machine lacks (daemon/moves.rs `Needs`), and the packings of the move's
/// stream it unpacks: none, from a daemon of an earlier build.
#[derive(serde::Deserialize)]
struct Needs {
    image: bool,
    files: Vec<String>,
    #[serde(default)]
    unpacks: Vec<String>,
}

/// `shards move vm NAME CONTEXT`, its local steps asked through `ask`.
pub(crate) fn move_vm(parsed: &Parsed, home: &Path, ask: &Ask<'_>) -> ExitCode {
    let (Some(name), Some(to)) = (parsed.args.first(), parsed.args.get(1)) else {
        return failed("shards: move vm: a microVM and a context");
    };
    match moved(home, name, to, parsed.bool("live"), ask) {
        Ok(keys) => {
            let _ = writeln!(io::stdout(), "{name}");
            // Its run's own key there and the one it replaced (D176 part 6, D238): for its
            // controller to pin anew and its leases' broker to reissue them (step 33).
            if keys.key.is_some() || keys.replaces.is_some() {
                let answer = serde_json::json!({
                    "name": name,
                    "context": to,
                    "key": keys.key,
                    "replaces": keys.replaces,
                });
                let _ = writeln!(io::stdout(), "{answer}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => failed(&format!("shards: {e}")),
    }
}

/// What a home keeps the CPU baseline its microVMs are told in (D176 part 6): the ID
/// registers on its first line, as `cpu::encode` writes them, and the contexts it is every
/// machine's of on its second. Its daemon reads it at each run (daemon/moves.rs).
pub(crate) const BASELINE: &str = "cpu-baseline";

/// `shards configure cpu` and `shards inspect cpu`'s page (`-h` is help everywhere).
const CPU_USAGE: &str = "usage: shards configure cpu --baseline CONTEXT[,CONTEXT...] | --none
       shards inspect cpu
  configure: tell every microVM this home starts the CPU this machine and every machine
             the contexts reach share, so a microVM moves to any of them; --none: this
             machine's own CPU again.
  inspect: show the baseline in use, and the machines it is shared with.";

/// `shards configure cpu --baseline CONTEXT[,CONTEXT...]` (`cpu configure`): the CPU every
/// machine the contexts reach and this one share, as each one's hypervisor shows a guest
/// (D176 part 6), told every microVM this home starts from then on, so that a move to any
/// of them finds nothing its guest may use lacking; `--none` takes it away. `shards inspect
/// cpu` (`cpu`) says it.
pub(crate) fn cpu(args: &[std::ffi::OsString]) -> ExitCode {
    let words: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let said: Vec<&str> = words.iter().map(String::as_str).collect();
    if crate::context::asks_help(&said, &["--baseline"]) {
        return crate::context::help_page("cpu", CPU_USAGE);
    }
    let home = match shards_ipc::home() {
        Ok(home) => home,
        Err(e) => return failed(&format!("shards: {e}")),
    };
    let at = home.join(BASELINE);
    let done = match words.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => match std::fs::read_to_string(&at) {
            Ok(text) => {
                let mut lines = text.lines();
                let regs = lines.next().unwrap_or_default();
                let of = lines.next().unwrap_or_default();
                let _ = writeln!(io::stdout(), "baseline of this machine and {of}: {regs}");
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let _ = writeln!(io::stdout(), "none: microVMs are told this machine's own CPU");
                Ok(())
            }
            Err(e) => Err(format!("{}: {e}", at.display())),
        },
        ["configure", "--none"] => match std::fs::remove_file(&at) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(format!("{}: {e}", at.display())),
            _ => Ok(()),
        },
        ["configure", "--baseline", contexts] => baseline_of(&home, contexts).and_then(|regs| {
            let text = format!("{}\n{contexts}\n", shards_vmm::arch::cpu::encode(&regs));
            std::fs::write(&at, text).map_err(|e| format!("{}: {e}", at.display()))
        }),
        _ => Err("usage: shards configure cpu --baseline CONTEXT[,CONTEXT...] | --none".into()),
    };
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => failed(&format!("shards: {e}")),
    }
}

/// The CPU this machine and every one `contexts` (comma-separated) reach share: each
/// one's, as its daemon asks its hypervisor (`move step cpu`), met field by field.
fn baseline_of(home: &Path, contexts: &str) -> Result<shards_vmm::arch::cpu::Id, String> {
    let step = || vec![shards_ipc::MOVE_STEP.to_string(), "cpu".to_string()];
    let read = |what: &str, (status, out, err): There| -> Result<shards_vmm::arch::cpu::Id, String> {
        if status != 0 {
            return Err(format!("{what}: {}", String::from_utf8_lossy(&err).trim()));
        }
        shards_vmm::arch::cpu::decode(String::from_utf8_lossy(&out).trim())
    };
    let daemon = super::shardsd()?;
    let command = shards_ipc::Command {
        argv: step(),
        daemon: shards_ipc::Identity::of_build(&daemon).map_err(|e| format!("{}: {e}", daemon.display()))?,
        ..shards_ipc::Command::default()
    };
    let mut cpus = vec![read(
        "this machine's CPU",
        super::client::ask(home, &daemon, &command, &[])?,
    )?];
    let identity = crate::context::identity(home)?;
    for name in contexts.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        let context = crate::context::load(home, name)?.ok_or_else(|| format!("no context named {name}"))?;
        let (out_tx, out_rx) = std::sync::mpsc::channel();
        let (err_tx, err_rx) = std::sync::mpsc::channel();
        let request = Request {
            args: step().into_iter().map(String::into_bytes).collect(),
            env: Vec::new(),
            terminal: None,
        };
        let (ran, _) = crate::context::exec(
            home,
            &identity,
            &context,
            request,
            None,
            Taken(out_tx),
            Taken(err_tx),
            |_| {},
            |_| {},
        );
        let there = match ran {
            Ok(Ok(status)) => (u8::try_from(status).unwrap_or(1), taken(&out_rx), taken(&err_rx)),
            Ok(Err(refused)) => return Err(format!("{name}: {refused}")),
            Err(e) => return Err(format!("{name}: {e}")),
        };
        cpus.push(read(&format!("{name}'s CPU"), there)?);
    }
    if cpus.len() < 2 {
        return Err("a baseline names the contexts of the machines it is every one's of".into());
    }
    shards_vmm::arch::cpu::baseline(&cpus)
        .map_err(|lacks| format!("these machines' CPUs have no baseline: {lacks}"))
}

fn failed(said: &str) -> ExitCode {
    let _ = writeln!(io::stderr(), "{said}");
    ExitCode::FAILURE
}

/// A step's refusal, in its own words.
fn said(step: &str, (_, _, err): &There) -> String {
    let text = String::from_utf8_lossy(err);
    let text = text.trim_end();
    let text = text.strip_prefix("shards: ").unwrap_or(text);
    if text.is_empty() {
        format!("{step} failed")
    } else {
        text.to_string()
    }
}

/// Why the other machine's `ready` step does not take a move: its refusal, typed
/// (daemon/moves.rs `Refused`), or, from a daemon of an earlier build or a step that failed,
/// its words.
fn unready(step: &str, answer: &There) -> String {
    match serde_json::from_slice::<crate::daemon::Refused>(&answer.1) {
        Ok(refused) => refused.to_string(),
        Err(_) => said(step, answer),
    }
}

/// Times each step to stderr where `SHARDS_MOVE_TIMES` is set: what a measurement of moves
/// reads (docs/research/measurements/migrate).
struct Times {
    on: bool,
    began: Instant,
}

impl Times {
    fn step(&self, what: &str) {
        if self.on {
            let _ = writeln!(
                io::stderr(),
                "move: {what} at {} us",
                self.began.elapsed().as_micros()
            );
        }
    }
}

/// What a move has heard of the other machine over every connection it made there
/// (`shards_remote::net::Seen`), from nothing as it starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Heard {
    /// The longest that machine went unheard and was heard again: its slowest answer in this
    /// move.
    slowest: Duration,
    /// When it was last heard.
    last: Option<Instant>,
    /// The link's round trip, as the connection that heard it last estimated it (RFC 9002 §5).
    rtt: Duration,
}

impl Heard {
    /// What one connection there saw, taken in.
    fn saw(&mut self, seen: &Seen) {
        self.slowest = self.slowest.max(seen.longest_silence);
        if seen.heard > self.last {
            self.last = seen.heard;
            self.rtt = seen.rtt;
        }
    }

    /// Whether that machine has gone unheard longer than its slowest answer, or never was heard.
    fn silent(&self) -> bool {
        self.last.is_none_or(|at| at.elapsed() > self.slowest)
    }
}

/// Whether the microVM runs on the other machine, its restore there unanswered (D176 part 6),
/// asked through `ask` while that machine shows it is there, from what this move has `heard`
/// of it. An answer says; a refusal leaves it not known (none). An ask that heard that
/// machine is followed by another, as is one that heard nothing until that machine is
/// [`Heard::silent`]: none then. Asks begin a round trip apart at least, the link's.
fn running_there(ask: &mut dyn FnMut() -> (Asked, Seen), mut heard: Heard) -> Option<bool> {
    loop {
        let began = Instant::now();
        let (asked, seen) = ask();
        heard.saw(&seen);
        match asked {
            Ok(Ok((0, out, _))) => return Some(String::from_utf8_lossy(&out).trim() == "true"),
            Ok(Ok(_)) => return Some(false),
            Ok(Err(_)) => return None,
            Err(_) if seen.heard.is_none() && heard.silent() => return None,
            Err(_) => std::thread::sleep(heard.rtt.saturating_sub(began.elapsed())),
        }
    }
}

/// Whether microVM `name` is gone from the other machine, removed there through `there`:
/// what a restore there left of it, running or not, which a restore still going on there
/// cannot bring back once its record is gone.
fn removed_there(there: &AskThere<'_>, name: &str) -> bool {
    let argv = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<String>>();
    let _ = there(argv(&["rm", "-f", name]), None);
    matches!(there(argv(&["inspect", "-f", "{{.Id}}", name]), None), Ok((s, _, _)) if s != 0)
}

/// What a move's stream carried and what its connection saw of the link, as its log says it
/// (docs/research/measurements/migrate/two-hosts.sh reads it): the stream's bytes as the
/// export wrote them, and as they were sent where it was packed; the bytes of UDP the
/// other machine acknowledged, over the import's whole time, the rate the link was seen to
/// carry; its round trip, the packets it took for lost, and the longest its peer went
/// unheard.
fn sent_line(tally: Option<crate::packing::Tally>, seen: &Seen, took: std::time::Duration) -> String {
    let packed = match tally {
        Some(t) => format!(
            "stream {} bytes, sent {} bytes, {} of {} frames as zstd",
            t.raw, t.wire, t.packed, t.frames
        ),
        None => "stream sent as it is".to_string(),
    };
    let us = took.as_micros().max(1);
    let rate = u128::from(seen.acked_bytes).saturating_mul(1_000_000) / us;
    format!(
        "move: {packed}; link {} bytes acknowledged in {us} us, {rate} B/s, rtt {} us, {} packets lost, longest silence {} us",
        seen.acked_bytes,
        seen.rtt.as_micros(),
        seen.lost_packets,
        seen.longest_silence.as_micros()
    )
}

/// A moved run's own key on the machine it landed on, and the one it had here, each a key's
/// SHA-256 in lowercase hex (`move step key`).
#[derive(Debug, Default, PartialEq, Eq, serde::Deserialize)]
struct Keys {
    key: Option<String>,
    replaces: Option<String>,
}

fn moved(home: &Path, name: &str, to: &str, live: bool, ask: &Ask<'_>) -> Result<Keys, String> {
    let times = Times {
        on: std::env::var_os("SHARDS_MOVE_TIMES").is_some(),
        began: Instant::now(),
    };
    let context = crate::context::load(home, to)?.ok_or_else(|| format!("no context named {to}"))?;
    let identity = crate::context::identity(home)?;
    // A step on the other machine: `argv`, `stdin` sent as it comes, its handshake waiting on
    // while `waits` says (`context::exec_with`); what it came to, and what its connection saw
    // of the link.
    let exec = |argv: Vec<String>,
                stdin: Option<Box<dyn Read + Send>>,
                waits: &mut dyn FnMut(shards_remote::client::Unanswered) -> bool|
     -> (Asked, Seen) {
        let (out_tx, out_rx) = std::sync::mpsc::channel();
        let (err_tx, err_rx) = std::sync::mpsc::channel();
        let request = Request {
            args: argv.into_iter().map(String::into_bytes).collect(),
            env: Vec::new(),
            terminal: None,
        };
        let (ran, seen) = crate::context::exec_with(
            home,
            &identity,
            &context,
            request,
            stdin,
            Taken(out_tx),
            Taken(err_tx),
            |_| {},
            |_| {},
            waits,
        );
        let asked = match ran {
            Ok(Ok(status)) => Ok(Ok((
                u8::try_from(status).unwrap_or(1),
                taken(&out_rx),
                taken(&err_rx),
            ))),
            Ok(Err(refused)) => Ok(Err(format!("{to}: {refused}"))),
            Err(e) => Err(format!("{to}: {e}")),
        };
        (asked, seen)
    };
    // What this move has heard of the other machine, each step's connection taken in: locked,
    // as the image's load and the import are asked on threads of their own.
    let heard = Mutex::new(Heard::default());
    // A step there, a refusal and no answer alike its error.
    let there_seen =
        |argv: Vec<String>, stdin: Option<Box<dyn Read + Send>>| -> (Result<There, String>, Seen) {
            let (asked, seen) = exec(argv, stdin, &mut |_| true);
            heard.lock().unwrap_or_else(PoisonError::into_inner).saw(&seen);
            (asked.flatten(), seen)
        };
    let there = |argv: Vec<String>, stdin: Option<Box<dyn Read + Send>>| there_seen(argv, stdin).0;
    let step = |argv: &[&str]| argv.iter().map(|a| (*a).to_string()).collect::<Vec<String>>();
    let mv = shards_ipc::MOVE_STEP;

    // 1. Saved here; or, moved live, its plan alone, as it runs on (D176 part 7).
    let saved = ask(step(&[mv, if live { "plan" } else { "save" }, name]), &[])?;
    if saved.0 != 0 {
        return Err(unready(if live { "its plan" } else { "its save" }, &saved));
    }
    let plan_json = String::from_utf8(saved.1).map_err(|_| "its plan is not UTF-8".to_string())?;
    let plan_json = plan_json.trim().to_string();
    let plan: Plan = serde_json::from_str(&plan_json).map_err(|e| format!("its plan: {e}"))?;
    times.step(if live { "planned" } else { "saved" });
    // From here, what fails leaves it here as it was: running on, where it moves live.
    let resume_here = |why: String| -> String {
        if live {
            return format!("{why}; {} goes on here", plan.name);
        }
        let back = crate::cli::request::restore(
            "shards restore vm",
            &[plan.name.clone().into(), plan.snapshot.clone().into()],
        );
        if back == ExitCode::SUCCESS {
            format!("{why}; {} goes on here", plan.name)
        } else {
            format!(
                "{why}; and {} could not be resumed here: shards restore vm {} {}",
                plan.name, plan.name, plan.snapshot
            )
        }
    };

    // 2. Whether the other machine can take it, and what it lacks.
    let ready = |phase: &str| -> Result<Needs, String> {
        let answer = there(step(&[mv, "ready", &plan_json]), None)?;
        if answer.0 != 0 {
            return Err(format!("{to} cannot take it: {}", unready(phase, &answer)));
        }
        serde_json::from_slice(&answer.1).map_err(|e| format!("{to}'s answer: {e}"))
    };
    let mut needs = match ready("its check") {
        Ok(needs) => needs,
        Err(e) => return Err(resume_here(e)),
    };
    if needs.image {
        // Its image, saved here and loaded there: one no registry it can reach holds.
        let loaded = (|| -> Result<(), String> {
            let (reader, writer) = std::io::pipe().map_err(|e| e.to_string())?;
            std::thread::scope(|scope| {
                let loading = std::thread::Builder::new()
                    .name("move-image".into())
                    .spawn_scoped(scope, move || there(step(&["load"]), Some(Box::new(reader))))
                    .map_err(|e| e.to_string())?;
                let saved = ask(
                    step(&["save", &plan.image_id]),
                    &[crate::cli::Stream::To(shards_ipc::fd::AsFd::as_fd(&writer))],
                );
                drop(writer);
                let loaded = loading
                    .join()
                    .map_err(|_| "loading its image failed".to_string())?;
                match (saved, loaded) {
                    (Ok((0, _, _)), Ok((0, _, _))) => Ok(()),
                    (Ok(s), _) if s.0 != 0 => Err(said("its image's save", &s)),
                    // Each breaks the other's stream: both said.
                    (Err(e), Err(l)) => Err(format!("{e}; its load: {l}")),
                    (Err(e), _) | (_, Err(e)) => Err(e),
                    (_, Ok(l)) => Err(format!("{to}: {}", said("its image's load", &l))),
                }
            })
        })();
        if let Err(e) = loaded {
            return Err(resume_here(format!("its image: {e}")));
        }
        // Saved by its digest, it is loaded with no name: given the one it has here, where
        // nothing there has it.
        if let Some(named) = &plan.image
            && !named.is_empty()
            && there(step(&["image", "inspect", "-f", "{{.Id}}", named]), None).is_ok_and(|r| r.0 != 0)
        {
            let _ = there(step(&["tag", &plan.image_id, named]), None);
        }
        times.step("image loaded there");
        needs = match ready("its check") {
            Ok(needs) if !needs.image => needs,
            Ok(_) => {
                return Err(resume_here(format!(
                    "{to} has its image loaded and still finds none"
                )));
            }
            Err(e) => return Err(resume_here(e)),
        };
    }
    times.step("ready there");
    if times.on {
        let _ = writeln!(
            io::stderr(),
            "move: {} of its {} files sent, {to} lacking them",
            needs.files.len(),
            plan.files.len()
        );
    }

    // 3. Its files, from this daemon to that one: moved live, its memory round after round
    // first, then its last save, which holds it here until that one has it.
    let checked: Vec<String> = plan.files.iter().map(|f| f.rel.clone()).collect();
    let mut said_here = Vec::new();
    // Packed for the link where the other machine unpacks it (crate::packing).
    let packs = crate::packing::taken(&needs.unpacks);
    let mut tallies = None;
    let mut seen = Seen::default();
    let import_began = Instant::now();
    let imported = (|| -> Result<(), String> {
        let (reader, writer) = std::io::pipe().map_err(|e| e.to_string())?;
        let mut import = step(&[mv, "import"]);
        let stdin: Box<dyn Read + Send> = if packs {
            import.extend(["--packed".to_string(), crate::packing::CODEC.to_string()]);
            let (packing, told) =
                crate::packing::pack(reader).map_err(|e| format!("packing its stream: {e}"))?;
            tallies = Some(told);
            Box::new(packing)
        } else {
            Box::new(reader)
        };
        let mut export = step(&[mv, if live { "live" } else { "export" }, name, &plan.snapshot]);
        if live {
            for rel in &needs.files {
                export.extend(["--need".to_string(), rel.clone()]);
            }
            for rel in &checked {
                export.extend(["--checked".to_string(), rel.clone()]);
            }
        } else {
            export.extend(needs.files.iter().cloned());
        }
        std::thread::scope(|scope| {
            let importing = std::thread::Builder::new()
                .name("move-import".into())
                .spawn_scoped(scope, move || there_seen(import, Some(stdin)))
                .map_err(|e| e.to_string())?;
            let sent = ask(
                export,
                &[crate::cli::Stream::To(shards_ipc::fd::AsFd::as_fd(&writer))],
            );
            drop(writer);
            let (taken, link) = importing.join().map_err(|_| "its import failed".to_string())?;
            seen = link;
            if let Ok((_, _, err)) = &sent {
                said_here.clone_from(err);
            }
            match (sent, taken) {
                (Ok((0, _, _)), Ok((0, _, _))) => Ok(()),
                (Ok(s), _) if s.0 != 0 => Err(said("its export", &s)),
                // Each breaks the other's stream: both said.
                (Err(e), Err(t)) => Err(format!("{e}; its import: {t}")),
                (Err(e), _) | (_, Err(e)) => Err(e),
                (_, Ok(t)) => Err(format!("{to}: {}", said("its import", &t))),
            }
        })
    })();
    if times.on {
        let _ = writeln!(
            io::stderr(),
            "{}",
            sent_line(
                tallies.and_then(|t| t.try_iter().last()),
                &seen,
                import_began.elapsed()
            )
        );
    }
    // A live move's microVM is held here, saved, from its last save on, until it runs there.
    let held = live && String::from_utf8_lossy(&said_here).contains("move: saved at ");
    let back_here = |why: String| -> String {
        if held {
            let _ = ask(step(&[mv, "done", name, &plan.snapshot, "back"]), &[]);
        }
        resume_here(why)
    };
    if let Err(e) = imported {
        return Err(back_here(e));
    }
    times.step("sent");
    if times.on {
        for line in String::from_utf8_lossy(&said_here).lines() {
            let _ = writeln!(io::stderr(), "{line}");
        }
    }

    // 4. Resumed there, where it was saved.
    let resumed = there(step(&["restore", name, &plan.snapshot]), None);
    match resumed {
        Ok((0, _, _)) => {}
        // Refused there: what was made there of it goes, and it goes on here.
        Ok(r) => {
            let _ = there(step(&["rm", "-f", name]), None);
            return Err(back_here(format!("{to}: {}", said("its restore", &r))));
        }
        // No answer: it may run there. Asked again until the other machine says; never
        // run in both places, nor in neither, where it cannot.
        Err(e) => {
            let so_far = *heard.lock().unwrap_or_else(PoisonError::into_inner);
            // An ask that hears nothing gives up as that machine goes silent past this move's
            // slowest answer ([`Heard::silent`]), as a daemon whose last wait ran out has no
            // handshake deadline of its own (D180).
            let mut inspect = || {
                let argv = step(&["inspect", "-f", "{{.State.Running}}", name]);
                exec(argv, None, &mut |_| !so_far.silent())
            };
            match running_there(&mut inspect, so_far) {
                Some(true) => {}
                // Not running there, and seen gone: it goes on here alone.
                Some(false) if removed_there(&there, name) => return Err(back_here(e)),
                _ => {
                    return Err(format!(
                        "{e}; whether {name} runs on {to} is not known: {}, saved, is held here until it is (shards --context {to} ps -a; then shards rm -f {name}, here or there)",
                        if held { "it" } else { "its copy" }
                    ));
                }
            }
        }
    }
    times.step("resumed there");
    // Its run's key there, made after its restore, and the one it replaced: none from a
    // daemon of an earlier build, which has no such step.
    let keys = match there(step(&[mv, "key", name]), None) {
        Ok((0, out, _)) => serde_json::from_slice::<Keys>(&out).unwrap_or_default(),
        _ => Keys::default(),
    };
    // How long it ran nowhere, as this machine's clock says: from its last save asked,
    // which held it here, to its restore there said done.
    if times.on
        && let Some(asked) = String::from_utf8_lossy(&said_here)
            .lines()
            .find_map(|l| l.strip_prefix("move: saved at ")?.trim().parse::<u128>().ok())
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let _ = writeln!(
            io::stderr(),
            "move: downtime {} us",
            now.saturating_sub(asked) / 1000
        );
    }

    // 5. Removed here: it runs there now. One a live move held stops as moved first.
    if held {
        let _ = ask(step(&[mv, "done", name, &plan.snapshot, "moved"]), &[]);
    }
    let removed = ask(step(&["rm", "-f", name]), &[])?;
    if removed.0 != 0 {
        return Err(format!(
            "{name} runs on {to} now, and its stopped copy here was not removed: {}",
            said("its removal", &removed)
        ));
    }
    times.step("removed here");
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a connection to the other machine saw: that machine last heard at `heard`, the
    /// longest it went unheard `longest`, the link's round trip `rtt`.
    fn seen(heard: Option<Instant>, longest: Duration, rtt: Duration) -> Seen {
        Seen {
            longest_silence: longest,
            heard,
            rtt,
            ..Seen::default()
        }
    }

    /// The other machine's answer: the microVM runs there, or not.
    fn running(yes: bool) -> Asked {
        Ok(Ok((0, format!("{yes}\n").into_bytes(), Vec::new())))
    }

    fn unanswered() -> Asked {
        Err("there: the connection to 192.0.2.1:3000 ended: timed out".into())
    }

    /// What a move learns of the other machine: its slowest answer the longest silence any of
    /// its connections there lived through; when it was last heard, and the link's round
    /// trip, the latest connection's that heard it; a connection that heard nothing changes
    /// neither. A move that heard nothing takes it for silent.
    #[test]
    fn a_move_learns_the_other_machines_slowest_answer_from_every_connection() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut heard = Heard::default();
        assert!(heard.silent());
        // `ready`, kept alive while it fetched a kernel; the import, through a stall of 31 s;
        // the restore; an ask that heard nothing.
        heard.saw(&seen(Some(t0), ms(2_500), ms(4)));
        heard.saw(&seen(Some(t0 + ms(9_000)), ms(31_000), ms(9)));
        heard.saw(&seen(Some(t0 + ms(9_100)), ms(120), ms(5)));
        heard.saw(&Seen::default());
        assert_eq!(
            heard,
            Heard {
                slowest: ms(31_000),
                last: Some(t0 + ms(9_100)),
                rtt: ms(5),
            }
        );
        assert!(!heard.silent());
    }

    /// The other machine's first answer says: the microVM running there, not running, or not
    /// there (its `inspect` failing); a refusal leaves it not known, as asking again would
    /// only ask to be refused again. One ask each, however patient the move could be.
    #[test]
    fn the_other_machines_first_answer_says() {
        let now = Instant::now();
        let heard = Heard {
            slowest: Duration::from_secs(3_600),
            last: Some(now),
            rtt: Duration::ZERO,
        };
        let failed: Asked = Ok(Ok((1, Vec::new(), b"shards: no such microVM: web\n".to_vec())));
        let refused: Asked = Ok(Err("there: this machine has not enrolled this client".into()));
        for (asked, says) in [
            (running(true), Some(true)),
            (running(false), Some(false)),
            (failed, Some(false)),
            (refused, None),
        ] {
            let mut asks = 0;
            let mut ask = || {
                asks += 1;
                (
                    asked.clone(),
                    seen(Some(Instant::now()), Duration::ZERO, Duration::ZERO),
                )
            };
            assert_eq!(running_there(&mut ask, heard), says);
            assert_eq!(asks, 1);
        }
    }

    /// A machine that shows it is there though its asks go unanswered (a connection it closed,
    /// its daemon restarting) is asked again until it answers, however many times: forty here,
    /// where twelve were all it was asked before; from a move that heard nothing of it before
    /// too. One that hears nothing is asked again while its silence is within its slowest
    /// answer in the move.
    #[test]
    fn a_machine_unanswered_is_asked_again_until_it_answers() {
        let before = [
            Heard::default(),
            Heard {
                slowest: Duration::from_millis(1),
                last: Some(Instant::now()),
                rtt: Duration::ZERO,
            },
        ];
        for heard in before {
            let mut asks = 0;
            let mut ask = || {
                asks += 1;
                let asked = if asks <= 40 { unanswered() } else { running(true) };
                (asked, seen(Some(Instant::now()), Duration::ZERO, Duration::ZERO))
            };
            assert_eq!(running_there(&mut ask, heard), Some(true));
            assert_eq!(asks, 41);
        }
        let heard = Heard {
            slowest: Duration::from_secs(3_600),
            last: Some(Instant::now()),
            rtt: Duration::ZERO,
        };
        let mut asks = 0;
        let mut ask = || {
            asks += 1;
            let asked = if asks <= 3 { unanswered() } else { running(false) };
            (asked, Seen::default())
        };
        assert_eq!(running_there(&mut ask, heard), Some(false));
        assert_eq!(asks, 4);
    }

    /// A machine that hears nothing and has gone unheard longer than the longest it went
    /// unheard in the move and was heard again is not known: at once where its silence had
    /// passed that before the asking began, as a restore's connection that ended unheard
    /// leaves it (its first ask still goes out), else as soon as its silence passes it, and
    /// never sooner. A move that heard nothing of it ends at the first ask that hears nothing.
    #[test]
    fn a_machine_silent_past_its_slowest_answer_is_not_known() {
        let slowest = Duration::from_millis(40);
        let rtt = Duration::from_millis(1);
        let mut asks = 0;
        let mut ask = || {
            asks += 1;
            (unanswered(), Seen::default())
        };
        let long_ago = Instant::now().checked_sub(Duration::from_millis(100)).unwrap();
        let heard = Heard {
            slowest,
            last: Some(long_ago),
            rtt,
        };
        assert_eq!(running_there(&mut ask, heard), None);
        assert_eq!(asks, 1);

        // Heard at its first ask, then nothing.
        let mut heard_at = None;
        let mut asks = 0;
        let mut ask = || {
            asks += 1;
            let at = (asks == 1).then(Instant::now);
            heard_at = heard_at.or(at);
            (unanswered(), seen(at, Duration::ZERO, rtt))
        };
        let heard = Heard {
            slowest,
            last: Some(long_ago),
            rtt,
        };
        assert_eq!(running_there(&mut ask, heard), None);
        assert!(asks >= 2);
        assert!(
            heard_at.unwrap().elapsed() > slowest,
            "not known only past its slowest answer"
        );

        let mut asks = 0;
        let mut ask = || {
            asks += 1;
            (unanswered(), Seen::default())
        };
        assert_eq!(running_there(&mut ask, Heard::default()), None);
        assert_eq!(asks, 1);
    }

    /// The wait takes every figure from what the other machine showed the move: its asks begin
    /// the link's round trip apart at least, whatever that was, and its source holds no number
    /// of its own, the exit status that answers aside (R03).
    #[test]
    fn the_wait_takes_its_figures_from_the_link_alone() {
        for rtt in [Duration::ZERO, Duration::from_millis(15)] {
            let mut asks = 0;
            let mut ask = || {
                asks += 1;
                let asked = if asks < 4 { unanswered() } else { running(true) };
                (asked, seen(Some(Instant::now()), Duration::ZERO, rtt))
            };
            let began = Instant::now();
            assert_eq!(running_there(&mut ask, Heard::default()), Some(true));
            assert_eq!(asks, 4);
            assert!(began.elapsed() >= rtt * 3, "four asks, a round trip apart");
        }
        let source = include_str!("moves.rs");
        let body = |from: &str| {
            source
                .split_once(from)
                .and_then(|(_, rest)| rest.split_once("\n}\n"))
                .map(|(body, _)| body)
                .unwrap()
        };
        for (what, code) in [
            ("running_there", body("fn running_there(")),
            ("Heard", body("impl Heard {")),
        ] {
            let code = code.replace("Ok(Ok((0, out, _)))", "");
            // A number's first digit follows no letter, digit or underscore (`utf8` is a name).
            let mut before = ' ';
            let number = code.chars().any(|c| {
                let starts = c.is_ascii_digit() && !(before.is_alphanumeric() || before == '_');
                before = c;
                starts
            });
            assert!(!number, "{what} holds a figure of its own:\n{code}");
        }
    }

    /// A refusal the other machine answers typed is said from what it is; one of a daemon of
    /// an earlier build, or a step that failed, in the step's own words.
    #[test]
    fn a_refusal_is_said_from_its_type_and_else_in_the_steps_words() {
        let refused = crate::daemon::Refused::NameTaken { name: "web".into() };
        let typed = (
            1,
            serde_json::to_vec(&refused).unwrap(),
            b"shards: anything\n".to_vec(),
        );
        assert_eq!(
            unready("its check", &typed),
            "a microVM named web is here already"
        );
        let worded = (
            1,
            Vec::new(),
            b"shards: a microVM named web is here already\n".to_vec(),
        );
        assert_eq!(
            unready("its check", &worded),
            "a microVM named web is here already"
        );
        assert_eq!(
            unready("its check", &(1, Vec::new(), Vec::new())),
            "its check failed"
        );
    }
}
