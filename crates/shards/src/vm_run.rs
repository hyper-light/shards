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

const RUN_USAGE: &str = "usage: shards vm run --kernel PATH [--initrd PATH | --init PATH] [--cmdline STR] [--cpus N] [--memory MIB] [--disk PATH[:ro]]... [--pmem PATH]... [--vsock PATH] [--no-console] [--snapshot-dir DIR [--snapshot-then stop|resume]]
       shards vm run --kernel PATH --init SHARDS-INIT --rootfs IMAGE [-e NAME[=VALUE]]... [-w DIR] [-u USER[:GROUP]] [--hostname NAME] [-i] [OPTIONS] -- COMMAND [ARG...]
  --pmem: a read-only virtio-pmem device backed by PATH: /dev/pmem0, pmem1, ... in order.
  --rootfs: boot into the EROFS image IMAGE and run COMMAND in it as `docker run` would:
            its output is shards' output, and its exit status shards' exit status.
  -e, -w, -u, --hostname, -i: as for `docker run`.
  --vsock: a vsock device. Host programs connect to the Unix socket PATH and send
           `CONNECT <port>`; the guest's connections to host port P reach PATH_P.
  --snapshot-dir: where to write a snapshot when the guest asks for one (then stop, by default)
  Console escape: Ctrl-A x stops the VM.";

const RESTORE_USAGE: &str =
    "usage: shards vm restore DIR [--hold] [--vsock PATH] [--no-console] [--snapshot-dir DIR [--snapshot-then stop|resume]]
  Resumes the VM in snapshot directory DIR.
  --vsock: this VM's vsock socket, required when the snapshot has a vsock device.
  --hold: prepare the VM, print `shards-ready` on stderr, and start it when a line arrives
          on stdin: a warm VM whose start costs only the release.
  Console escape: Ctrl-A x stops the VM.";

/// Arguments as UTF-8 strings, with an error naming the first one that is not.
fn utf8(args: impl Iterator<Item = OsString>) -> impl Iterator<Item = Result<String, String>> {
    args.map(|a| {
        a.into_string()
            .map_err(|a| format!("argument {a:?} is not valid UTF-8"))
    })
}

/// Options `run` and `restore` share.
struct Common {
    console: Console,
    snapshot_dir: Option<PathBuf>,
    then: AfterSnapshot,
    vsock: Option<PathBuf>,
}

impl Common {
    fn new() -> Common {
        Common {
            console: Console::Stdout,
            snapshot_dir: None,
            then: AfterSnapshot::Stop,
            vsock: None,
        }
    }

    /// Takes `arg` if it is a shared option; returns whether it was.
    fn option(
        &mut self,
        arg: &str,
        value: &mut dyn FnMut(&str) -> Result<String, String>,
    ) -> Result<bool, String> {
        match arg {
            "--no-console" => self.console = Console::Discard,
            "--snapshot-dir" => self.snapshot_dir = Some(PathBuf::from(value("--snapshot-dir")?)),
            "--snapshot-then" => {
                self.then = match value("--snapshot-then")?.as_str() {
                    "stop" => AfterSnapshot::Stop,
                    "resume" => AfterSnapshot::Resume,
                    other => return Err(format!("--snapshot-then: {other:?} is not stop or resume")),
                }
            }
            "--vsock" => self.vsock = Some(PathBuf::from(value("--vsock")?)),
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn policy(&mut self) -> Option<SnapshotPolicy> {
        let then = self.then;
        self.snapshot_dir.take().map(|dir| SnapshotPolicy { dir, then })
    }
}

/// `vm run`'s options: the VM, and a workload to run in an image (`--rootfs`, `--`).
struct Run {
    cfg: Config,
    workload: Option<(PathBuf, crate::workload::Options)>,
}

fn parse_run(args: impl Iterator<Item = OsString>) -> Result<Run, String> {
    let mut args = utf8(args);
    let (mut rootfs, mut options, mut workload_flags) = (None, crate::workload::Options::default(), false);
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
        pmem: Vec::new(),
        vsock: None,
    };
    let (mut kernel, mut common) = (None, Common::new());
    while let Some(arg) = args.next() {
        let arg = arg?;
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"))?;
        if common.option(&arg, &mut value)? {
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
            "--pmem" => cfg.pmem.push(PathBuf::from(value("--pmem")?)),
            "--rootfs" => rootfs = Some(PathBuf::from(value("--rootfs")?)),
            "-e" | "--env" => {
                options.env.push(value("--env")?);
                workload_flags = true;
            }
            "-w" | "--workdir" => {
                options.workdir = value("--workdir")?;
                workload_flags = true;
            }
            "-u" | "--user" => {
                options.user = value("--user")?;
                workload_flags = true;
            }
            "--hostname" => {
                options.hostname = Some(value("--hostname")?);
                workload_flags = true;
            }
            "-i" | "--interactive" => {
                options.interactive = true;
                workload_flags = true;
            }
            "--" => {
                options.argv = args.by_ref().collect::<Result<_, _>>()?;
                break;
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    cfg.kernel = kernel.ok_or("--kernel is required")?;
    cfg.snapshot = common.policy();
    cfg.console = common.console;
    cfg.vsock = common.vsock;
    let workload = match (rootfs, options.argv.is_empty()) {
        (Some(rootfs), false) => Some((rootfs, options)),
        (Some(_), true) => return Err("--rootfs needs a command after --".into()),
        (None, false) => return Err("a command needs --rootfs".into()),
        (None, true) if workload_flags => return Err("-e, -w, -u, --hostname and -i need a command".into()),
        (None, true) => None,
    };
    Ok(Run { cfg, workload })
}

fn parse_restore(args: impl Iterator<Item = OsString>) -> Result<RestoreConfig, String> {
    let mut args = utf8(args);
    let (mut dir, mut common, mut hold) = (None, Common::new(), false);
    while let Some(arg) = args.next() {
        let arg = arg?;
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"))?;
        if common.option(&arg, &mut value)? {
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => return Err(String::new()),
            "--hold" => hold = true,
            flag if flag.starts_with('-') => return Err(format!("unknown argument {flag:?}")),
            _ if dir.is_some() => return Err(format!("unexpected argument {arg:?}")),
            _ => dir = Some(PathBuf::from(arg)),
        }
    }
    Ok(RestoreConfig {
        dir: dir.ok_or("the snapshot directory is required")?,
        console: common.console,
        snapshot: common.policy(),
        hold,
        vsock: common.vsock,
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
        Ok(Run {
            cfg,
            workload: Some((rootfs, options)),
        }) => run_workload(cfg, rootfs, &options),
        Ok(Run { cfg, workload: None }) => supervise(vm::start(&cfg), cfg.console),
        Err(code) => code,
    }
}

/// Boots into `rootfs` and runs the workload there; exits as it does.
#[cfg(unix)]
fn run_workload(mut cfg: Config, rootfs: PathBuf, options: &crate::workload::Options) -> ExitCode {
    use crate::workload::{self, NOT_RUN};
    let failed = |e: String| {
        report(e);
        ExitCode::from(NOT_RUN)
    };
    let spec = match workload::spec(options) {
        Ok(spec) => spec,
        Err(e) => return failed(e),
    };
    // Without --vsock, the device's sockets go in a private directory.
    let mut sockets = None;
    let vsock = match &cfg.vsock {
        Some(path) => path.clone(),
        None => match workload::SocketDir::new() {
            Ok(dir) => sockets.insert(dir).path().join("vsock"),
            Err(e) => return failed(format!("socket directory: {e}")),
        },
    };
    cfg.vsock = Some(vsock.clone());
    let listener = match workload::listen(&vsock) {
        Ok(l) => l,
        Err(e) => return failed(format!("listening for the guest: {e}")),
    };
    cfg.pmem.insert(0, rootfs);
    // The workload's output is shards' output, so the console goes nowhere, and the
    // kernel need not print to it.
    cfg.cmdline.push_str(" quiet shards_root=/dev/pmem0");
    cfg.console = Console::Discard;
    let (handle, running) = match vm::start(&cfg) {
        Ok(started) => started,
        Err(e) => return failed(e),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let interactive = options.interactive;
    let served = std::thread::Builder::new()
        .name("workload".into())
        .spawn(move || {
            let _ = tx.send(workload::serve(&listener, &spec, interactive));
        });
    if let Err(e) = served {
        handle.stop();
        return failed(format!("workload thread: {e}"));
    }
    let reason = running.wait(handle);
    if let ExitReason::Error(e) = &reason {
        report(e);
    }
    // The guest waits for its status to be read before it powers off, so the relay has
    // finished unless the guest never ran the command.
    let status = match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(status)) => ExitCode::from(status),
        Ok(Err(e)) => failed(e),
        Err(_) => failed("the guest stopped without running the command".into()),
    };
    drop(sockets);
    status
}

#[cfg(not(unix))]
fn run_workload(_: Config, _: PathBuf, _: &crate::workload::Options) -> ExitCode {
    report("running a command in a microVM needs vsock, which shards does not support on this platform yet");
    ExitCode::from(125)
}

pub fn restore(args: impl Iterator<Item = OsString>) -> ExitCode {
    let cfg = match parsed(parse_restore(args), RESTORE_USAGE) {
        Ok(cfg) => cfg,
        Err(code) => return code,
    };
    let started = vm::restore(&cfg);
    if cfg.hold
        && let Ok((handle, _)) = &started
    {
        let _ = writeln!(std::io::stderr(), "shards-ready");
        // Any line (or end of input) is the start request.
        let _ = std::io::stdin().read_line(&mut String::new());
        handle.release();
    }
    supervise(started, cfg.console)
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
            "shards-timing {{\"released_us\":{},\"entry_us\":{},\"exit_us\":{},\"markers\":[{}]}}",
            handle.released_at_us().unwrap_or(0),
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
