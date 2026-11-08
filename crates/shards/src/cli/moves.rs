//! `shards move vm NAME CONTEXT` (D176 part 6): a microVM moved to the machine a context
//! reaches (D180), driven here as `shards cp` is, each step asked of this machine's daemon
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
use std::time::Instant;

use shards_cmdline::flags::Parsed;
use shards_remote::frame::Request;
use shards_remote::net::Seen;

use crate::cli::cp::Ask;

/// What a step on the other machine said: its status, its stdout and its stderr.
type There = (u8, Vec<u8>, Vec<u8>);

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
        Ok(()) => {
            let _ = writeln!(io::stdout(), "{name}");
            ExitCode::SUCCESS
        }
        Err(e) => failed(&format!("shards: {e}")),
    }
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

/// Whether microVM `name` runs on the other machine, asked through `there` up to a minute
/// as its answers do not come: none where none came.
fn running_there(there: &AskThere<'_>, name: &str) -> Option<bool> {
    let ask = || {
        there(
            ["inspect", "-f", "{{.State.Running}}", name]
                .iter()
                .map(|a| (*a).to_string())
                .collect(),
            None,
        )
    };
    for _ in 0..12 {
        match ask() {
            Ok((0, out, _)) => return Some(String::from_utf8_lossy(&out).trim() == "true"),
            Ok(_) => return Some(false),
            Err(_) => std::thread::sleep(std::time::Duration::from_secs(5)),
        }
    }
    None
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
/// unheard, which `shards_remote::tls::QUIET` bounds.
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

fn moved(home: &Path, name: &str, to: &str, live: bool, ask: &Ask<'_>) -> Result<(), String> {
    let times = Times {
        on: std::env::var_os("SHARDS_MOVE_TIMES").is_some(),
        began: Instant::now(),
    };
    let context = crate::context::load(home, to)?.ok_or_else(|| format!("no context named {to}"))?;
    let identity = crate::context::identity(home)?;
    // A step on the other machine: `argv`, `stdin` sent as it comes; and what its
    // connection saw of the link.
    let there_seen =
        |argv: Vec<String>, stdin: Option<Box<dyn Read + Send>>| -> (Result<There, String>, Seen) {
            let (out_tx, out_rx) = std::sync::mpsc::channel();
            let (err_tx, err_rx) = std::sync::mpsc::channel();
            let request = Request {
                args: argv.into_iter().map(String::into_bytes).collect(),
                env: Vec::new(),
                terminal: None,
            };
            let (ran, seen) = shards_remote::client::exec_seen(
                &identity,
                context.remote(),
                request,
                stdin,
                Taken(out_tx),
                Taken(err_tx),
            );
            let ran = match ran {
                Ok(Ok(status)) => Ok((u8::try_from(status).unwrap_or(1), taken(&out_rx), taken(&err_rx))),
                Ok(Err(refused)) => Err(format!("{to}: {refused}")),
                Err(e) => Err(format!("{to}: {e}")),
            };
            (ran, seen)
        };
    let there = |argv: Vec<String>, stdin: Option<Box<dyn Read + Send>>| there_seen(argv, stdin).0;
    let step = |argv: &[&str]| argv.iter().map(|a| (*a).to_string()).collect::<Vec<String>>();
    let mv = shards_ipc::MOVE_STEP;

    // 1. Saved here; or, moved live, its plan alone, as it runs on (D176 part 7).
    let saved = ask(step(&[mv, if live { "plan" } else { "save" }, name]), &[])?;
    if saved.0 != 0 {
        return Err(said(if live { "its plan" } else { "its save" }, &saved));
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
            return Err(format!("{to} cannot take it: {}", said(phase, &answer)));
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
        Err(e) => match running_there(&there, name) {
            Some(true) => {}
            // Not running there, and seen gone: it goes on here alone.
            Some(false) if removed_there(&there, name) => return Err(back_here(e)),
            _ => {
                return Err(format!(
                    "{e}; whether {name} runs on {to} is not known: {}, saved, is held here until it is (shards --context {to} ps -a; then shards rm -f {name}, here or there)",
                    if held { "it" } else { "its copy" }
                ));
            }
        },
    }
    times.step("resumed there");
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
    Ok(())
}
