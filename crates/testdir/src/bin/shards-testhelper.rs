//! The tests' helper program, the same on every host, Windows' included, which has no
//! shell and none of a shell's tools: the commands a test runs where it would run `true`,
//! `echo`, `cat` or `sleep`, and a credential helper that never answers. Built and given to
//! tests by `shards_testdir::helper`.
//!
//! - `true`; `echo WORDS…`; `cat`; `sleep SECONDS`: as the tools are.
//! - `zeros N`: N zero bytes on stdout, as `head -c N /dev/zero` writes them.
//! - `say OUT ERR STATUS`: OUT on stdout and ERR on stderr, a line each, then exits STATUS.
//! - `drip MS`: reads its stdin 64 KiB at a time, MS milliseconds apart, as a slow consumer
//!   of a stream does (a moved microVM's import writing its files), then says how many bytes
//!   it read, a line.
//! - `await GO [ready=FILE] [say=TEXT] [exit=STATUS]`: makes FILE, then waits until GO is
//!   there, then says TEXT, a line, and exits STATUS (0 if none). It waits while GO's
//!   directory is, a test's own (`TempDir`), which goes as its test ends, failed or killed
//!   too: one whose test failed before it made GO is no process left behind.
//! - Run as `docker-credential-NAME` (a copy named so), whatever its arguments: a
//!   credential helper that never answers, its stdin never read. It adds its process ID to
//!   `docker-credential-NAME.pids` beside itself, a line, and waits for ever.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if let Ok(me) = std::env::current_exe()
        && me
            .file_stem()
            .is_some_and(|s| s.to_string_lossy().starts_with("docker-credential-"))
    {
        return never_answer(&me);
    }
    match run(args.get(1..).unwrap_or_default()) {
        Ok(status) => ExitCode::from(status),
        Err(e) => {
            let _ = writeln!(io::stderr(), "shards-testhelper: {e}");
            ExitCode::from(127)
        }
    }
}

/// The command `args` names, run: its exit status, or why it could not run.
fn run(args: &[String]) -> Result<u8, String> {
    let (command, rest) = args.split_first().ok_or("no command")?;
    let mut out = io::stdout().lock();
    match (command.as_str(), rest) {
        ("true", []) => Ok(0),
        ("echo", words) => {
            writeln!(out, "{}", words.join(" ")).map_err(|e| format!("echo: {e}"))?;
            Ok(0)
        }
        ("cat", []) => {
            io::copy(&mut io::stdin().lock(), &mut out).map_err(|e| format!("cat: {e}"))?;
            Ok(0)
        }
        ("sleep", [seconds]) => {
            let seconds: u64 = seconds.parse().map_err(|e| format!("sleep {seconds}: {e}"))?;
            std::thread::sleep(Duration::from_secs(seconds));
            Ok(0)
        }
        ("zeros", [n]) => {
            let mut left: u64 = n.parse().map_err(|e| format!("zeros {n}: {e}"))?;
            let block = [0u8; 64 << 10];
            while left > 0 {
                let now = usize::try_from(left.min(block.len() as u64)).unwrap_or(block.len());
                out.write_all(block.get(..now).unwrap_or(&block))
                    .map_err(|e| format!("zeros: {e}"))?;
                left = left.saturating_sub(now as u64);
            }
            Ok(0)
        }
        ("drip", [ms]) => {
            let pause = Duration::from_millis(ms.parse().map_err(|e| format!("drip {ms}: {e}"))?);
            let mut input = io::stdin().lock();
            let mut buffer = vec![0u8; 64 << 10];
            let mut read = 0u64;
            loop {
                match io::Read::read(&mut input, &mut buffer) {
                    Ok(0) => break,
                    Ok(n) => read = read.saturating_add(n as u64),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(format!("drip: {e}")),
                }
                std::thread::sleep(pause);
            }
            writeln!(out, "{read}").map_err(|e| format!("drip: {e}"))?;
            Ok(0)
        }
        ("say", [said, err, status]) => {
            let status = status.parse().map_err(|e| format!("say: status {status}: {e}"))?;
            writeln!(out, "{said}").map_err(|e| format!("say: {e}"))?;
            out.flush().map_err(|e| format!("say: {e}"))?;
            writeln!(io::stderr(), "{err}").map_err(|e| format!("say: {e}"))?;
            Ok(status)
        }
        ("await", [go, options @ ..]) => {
            let (mut said, mut status) = (None, 0u8);
            for option in options {
                match option.split_once('=') {
                    Some(("ready", file)) => {
                        std::fs::write(file, b"").map_err(|e| format!("await: {file}: {e}"))?;
                    }
                    Some(("say", text)) => said = Some(text),
                    Some(("exit", n)) => status = n.parse().map_err(|e| format!("await: exit {n}: {e}"))?,
                    _ => return Err(format!("await: {option}?")),
                }
            }
            let go = Path::new(go);
            // Its test's directory, which outlives the wait only while its test does.
            let dir = go
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .ok_or_else(|| format!("await: {}: no test's directory to wait in", go.display()))?;
            while !go.exists() {
                if !dir.is_dir() {
                    return Err(format!(
                        "await: {} went before {} came: its test ended",
                        dir.display(),
                        go.display()
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if let Some(said) = said {
                writeln!(out, "{said}").map_err(|e| format!("await: {e}"))?;
            }
            Ok(status)
        }
        _ => Err(format!("{}: no such command", args.join(" "))),
    }
}

/// A credential helper that never answers: its process ID added to the file beside `me`
/// named for it, then a wait for ever.
fn never_answer(me: &Path) -> ExitCode {
    let pids: PathBuf = me.with_extension("pids");
    let recorded = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&pids)
        .and_then(|mut f| writeln!(f, "{}", std::process::id()));
    if let Err(e) = recorded {
        let _ = writeln!(io::stderr(), "{}: {e}", pids.display());
        return ExitCode::FAILURE;
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
