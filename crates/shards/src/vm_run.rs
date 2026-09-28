//! `shards vm run` boots a kernel directly in a microVM; `shards vm restore` resumes one
//! from a snapshot.

use std::ffi::OsString;
use std::fmt::Display;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use shards_vmm::vm::{
    self, AfterSnapshot, Config, Console, Disk, ExitReason, Handle, RestoreConfig, Running, SnapshotPolicy,
};

use crate::terminal::RawTerminal;

const RUN_USAGE: &str = "usage: shards vm run --kernel PATH [--initrd PATH | --init PATH] [--cmdline STR] [--cpus N] [--memory MIB] [--disk PATH[:ro]]... [--no-console] [--snapshot-dir DIR [--snapshot-then stop|resume]]
  --snapshot-dir: where to write a snapshot when the guest asks for one (then stop, by default)
  Console escape: Ctrl-A x stops the VM.";

const RESTORE_USAGE: &str =
    "usage: shards vm restore DIR [--no-console] [--snapshot-dir DIR [--snapshot-then stop|resume]]
  Resumes the VM in snapshot directory DIR.
  Console escape: Ctrl-A x stops the VM.";

/// Arguments as UTF-8 strings, with an error naming the first one that is not.
fn utf8(args: impl Iterator<Item = OsString>) -> impl Iterator<Item = Result<String, String>> {
    args.map(|a| {
        a.into_string()
            .map_err(|a| format!("argument {a:?} is not valid UTF-8"))
    })
}

/// Options `run` and `restore` share. Returns whether `arg` was one of them.
fn common_option(
    arg: &str,
    value: &mut dyn FnMut(&str) -> Result<String, String>,
    console: &mut Console,
    snapshot_dir: &mut Option<PathBuf>,
    then: &mut AfterSnapshot,
) -> Result<bool, String> {
    match arg {
        "--no-console" => *console = Console::Discard,
        "--snapshot-dir" => *snapshot_dir = Some(PathBuf::from(value("--snapshot-dir")?)),
        "--snapshot-then" => {
            *then = match value("--snapshot-then")?.as_str() {
                "stop" => AfterSnapshot::Stop,
                "resume" => AfterSnapshot::Resume,
                other => return Err(format!("--snapshot-then: {other:?} is not stop or resume")),
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn policy(dir: Option<PathBuf>, then: AfterSnapshot) -> Option<SnapshotPolicy> {
    dir.map(|dir| SnapshotPolicy { dir, then })
}

fn parse_run(args: impl Iterator<Item = OsString>) -> Result<Config, String> {
    let mut args = utf8(args);
    let mut cfg = Config {
        kernel: PathBuf::new(),
        initrd: None,
        init: None,
        cmdline: "console=ttyS0 earlycon panic=-1".into(),
        vcpus: 1,
        memory_mib: 256,
        console: Console::Stdout,
        disks: Vec::new(),
        snapshot: None,
    };
    let (mut kernel, mut snapshot_dir, mut then) = (None, None, AfterSnapshot::Stop);
    while let Some(arg) = args.next() {
        let arg = arg?;
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"))?;
        if common_option(&arg, &mut value, &mut cfg.console, &mut snapshot_dir, &mut then)? {
            continue;
        }
        match arg.as_str() {
            "--kernel" => kernel = Some(PathBuf::from(value("--kernel")?)),
            "--initrd" => cfg.initrd = Some(PathBuf::from(value("--initrd")?)),
            "--init" => cfg.init = Some(PathBuf::from(value("--init")?)),
            "--cmdline" => cfg.cmdline = value("--cmdline")?,
            "--cpus" => cfg.vcpus = value("--cpus")?.parse().map_err(|e| format!("--cpus: {e}"))?,
            "--memory" => {
                cfg.memory_mib = value("--memory")?.parse().map_err(|e| format!("--memory: {e}"))?
            }
            "--disk" => {
                let spec = value("--disk")?;
                let (path, read_only) = match spec.strip_suffix(":ro") {
                    Some(path) => (path, true),
                    None => (spec.as_str(), false),
                };
                cfg.disks.push(Disk {
                    path: PathBuf::from(path),
                    read_only,
                });
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    cfg.kernel = kernel.ok_or("--kernel is required")?;
    cfg.snapshot = policy(snapshot_dir, then);
    Ok(cfg)
}

fn parse_restore(args: impl Iterator<Item = OsString>) -> Result<RestoreConfig, String> {
    let mut args = utf8(args);
    let (mut dir, mut console, mut snapshot_dir, mut then) =
        (None, Console::Stdout, None, AfterSnapshot::Stop);
    while let Some(arg) = args.next() {
        let arg = arg?;
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"))?;
        if common_option(&arg, &mut value, &mut console, &mut snapshot_dir, &mut then)? {
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => return Err(String::new()),
            flag if flag.starts_with('-') => return Err(format!("unknown argument {flag:?}")),
            _ if dir.is_some() => return Err(format!("unexpected argument {arg:?}")),
            _ => dir = Some(PathBuf::from(arg)),
        }
    }
    Ok(RestoreConfig {
        dir: dir.ok_or("the snapshot directory is required")?,
        console,
        snapshot: policy(snapshot_dir, then),
    })
}

/// Console output that never fails the caller (e.g. with stderr closed).
fn report(message: impl Display) {
    let _ = writeln!(std::io::stderr(), "shards: {message}");
}

fn forward_stdin(handle: vm::Handle) {
    let spawned = std::thread::Builder::new()
        .name("console-in".into())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 256];
            let mut escape = false;
            loop {
                let n = match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                for &b in buf.iter().take(n) {
                    match (escape, b) {
                        (true, b'x') => return handle.stop(),
                        (true, 0x01) => handle.console_input(&[0x01]), // Ctrl-A Ctrl-A sends one
                        (true, other) => handle.console_input(&[0x01, other]),
                        (false, 0x01) => {}
                        (false, other) => handle.console_input(&[other]),
                    }
                    escape = !escape && b == 0x01;
                }
            }
        });
    if let Err(e) = spawned {
        report(format!("console input unavailable: {e}"));
    }
}

/// The parsed configuration, or the exit code after printing help or a usage error.
fn parsed<T>(parse: Result<T, String>, usage: &str) -> Result<T, ExitCode> {
    match parse {
        Ok(cfg) => Ok(cfg),
        Err(e) if e.is_empty() => {
            let _ = writeln!(std::io::stdout(), "{usage}");
            Err(ExitCode::SUCCESS)
        }
        Err(e) => {
            report(format!("{e}\n{usage}"));
            Err(ExitCode::from(2))
        }
    }
}

pub fn run(args: impl Iterator<Item = OsString>) -> ExitCode {
    match parsed(parse_run(args), RUN_USAGE) {
        Ok(cfg) => supervise(vm::start(&cfg), cfg.console),
        Err(code) => code,
    }
}

pub fn restore(args: impl Iterator<Item = OsString>) -> ExitCode {
    match parsed(parse_restore(args), RESTORE_USAGE) {
        Ok(cfg) => supervise(vm::restore(&cfg), cfg.console),
        Err(code) => code,
    }
}

/// Runs a started VM to its end: console, timing report, exit code.
fn supervise(started: Result<(Handle, Running), String>, console: Console) -> ExitCode {
    let (handle, running) = match started {
        Ok(v) => v,
        Err(e) => {
            report(e);
            return ExitCode::FAILURE;
        }
    };
    let terminal = (console == Console::Stdout).then(RawTerminal::enable).flatten();
    forward_stdin(handle.clone());
    let reason = running.wait(handle.clone());
    drop(terminal);
    if std::env::var_os("SHARDS_TIMING").is_some() {
        // One machine-readable line for benchmark harnesses.
        let markers: Vec<String> = handle
            .markers()
            .iter()
            .map(|(m, t)| format!("[{m},{t}]"))
            .collect();
        let _ = writeln!(
            std::io::stderr(),
            "shards-timing {{\"entry_us\":{},\"exit_us\":{},\"markers\":[{}]}}",
            handle.entered_at_us().unwrap_or(0),
            handle.exited_at_us().unwrap_or(0),
            markers.join(",")
        );
    }
    match reason {
        ExitReason::PowerOff | ExitReason::Stopped | ExitReason::Snapshotted => ExitCode::SUCCESS,
        ExitReason::Reset => {
            report("guest requested a reset");
            ExitCode::from(3)
        }
        ExitReason::Error(e) => {
            report(e);
            ExitCode::FAILURE
        }
    }
}
