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

use crate::cli::cp::Ask;

/// What a step on the other machine said: its status, its stdout and its stderr.
type There = (u8, Vec<u8>, Vec<u8>);

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
    image_id: String,
    snapshot: String,
}

/// What the other machine lacks (daemon/moves.rs `Needs`).
#[derive(serde::Deserialize)]
struct Needs {
    image: bool,
    files: Vec<String>,
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

fn moved(home: &Path, name: &str, to: &str, live: bool, ask: &Ask<'_>) -> Result<(), String> {
    let times = Times {
        on: std::env::var_os("SHARDS_MOVE_TIMES").is_some(),
        began: Instant::now(),
    };
    let context = crate::context::load(home, to)?.ok_or_else(|| format!("no context named {to}"))?;
    let identity = crate::context::identity(home)?;
    // A step on the other machine: `argv`, `stdin` sent as it comes.
    let there = |argv: Vec<String>, stdin: Option<Box<dyn Read + Send>>| -> Result<There, String> {
        let (out_tx, out_rx) = std::sync::mpsc::channel();
        let (err_tx, err_rx) = std::sync::mpsc::channel();
        let request = Request {
            args: argv.into_iter().map(String::into_bytes).collect(),
            env: Vec::new(),
            terminal: None,
        };
        let ran = shards_remote::client::exec(
            &identity,
            context.remote(),
            request,
            stdin,
            Taken(out_tx),
            Taken(err_tx),
            |_| {},
        );
        match ran {
            Ok(Ok(status)) => Ok((u8::try_from(status).unwrap_or(1), taken(&out_rx), taken(&err_rx))),
            Ok(Err(refused)) => Err(format!("{to}: {refused}")),
            Err(e) => Err(format!("{to}: {e}")),
        }
    };
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
                    (Err(e), _) | (_, Err(e)) => Err(e),
                    (_, Ok(l)) => Err(format!("{to}: {}", said("its image's load", &l))),
                }
            })
        })();
        if let Err(e) = loaded {
            return Err(resume_here(format!("its image: {e}")));
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

    // 3. Its files, from this daemon to that one: moved live, its memory round after round
    // first, then its last save, which holds it here until that one has it.
    let checked: Vec<String> = plan.files.iter().map(|f| f.rel.clone()).collect();
    let mut said_here = Vec::new();
    let imported = (|| -> Result<(), String> {
        let (reader, writer) = std::io::pipe().map_err(|e| e.to_string())?;
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
                .spawn_scoped(scope, move || {
                    there(step(&[mv, "import"]), Some(Box::new(reader)))
                })
                .map_err(|e| e.to_string())?;
            let sent = ask(
                export,
                &[crate::cli::Stream::To(shards_ipc::fd::AsFd::as_fd(&writer))],
            );
            drop(writer);
            let taken = importing.join().map_err(|_| "its import failed".to_string())?;
            if let Ok((_, _, err)) = &sent {
                said_here.clone_from(err);
            }
            match (sent, taken) {
                (Ok((0, _, _)), Ok((0, _, _))) => Ok(()),
                (Ok(s), _) if s.0 != 0 => Err(said("its export", &s)),
                (Err(e), _) | (_, Err(e)) => Err(e),
                (_, Ok(t)) => Err(format!("{to}: {}", said("its import", &t))),
            }
        })
    })();
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
        failed => {
            let why = match failed {
                Ok(r) => format!("{to}: {}", said("its restore", &r)),
                Err(e) => e,
            };
            // What was made there of it goes, and it goes on here.
            let _ = there(step(&["rm", "-f", name]), None);
            return Err(back_here(why));
        }
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
