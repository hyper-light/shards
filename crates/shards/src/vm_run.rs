//! `shards vm run` boots a kernel directly in a microVM; `shards vm restore` resumes one
//! from a snapshot. Either can run a command in an image as `docker run` does
//! (workload.rs): `vm run --rootfs IMAGE -- COMMAND` boots into the image for it, and
//! `vm run --rootfs IMAGE --snapshot-dir DIR` saves a template, booted and mounted, that
//! `vm restore DIR -- COMMAND` resumes for each command.

use std::ffi::OsString;
use std::fmt::Display;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use shards_vmm::vm::{
    self, AfterSnapshot, Config, Console, Disk, ExitReason, Handle, RestoreConfig, Running, SnapshotPolicy,
};

use crate::terminal::RawTerminal;
use crate::workload::Options;

const RUN_USAGE: &str = "usage: shards vm run --kernel PATH [--initrd PATH | --init PATH] [--cmdline STR] [--cpus N] [--memory MIB] [--disk PATH[:ro]]... [--pmem PATH]... [--vsock PATH] [--no-console] [--snapshot-dir DIR [--snapshot-then stop|resume]]
       shards vm run --kernel PATH --init SHARDS-INIT --rootfs IMAGE [OPTIONS] [WORKLOAD OPTIONS] -- COMMAND [ARG...]
       shards vm run --kernel PATH --init SHARDS-INIT --rootfs IMAGE --snapshot-dir DIR [OPTIONS]
  --pmem: a read-only virtio-pmem device backed by PATH: /dev/pmem0, pmem1, ... in order.
  --rootfs: boot into the EROFS image IMAGE. With a COMMAND, run it there as `docker run`
            would: its output is shards' output, and its exit status shards' exit status.
            With --snapshot-dir instead, save the VM to DIR once the image is mounted: a
            template for `shards vm restore DIR -- COMMAND`.
  Workload options, as for `docker run`: -e NAME[=VALUE], -w DIR, -u USER[:GROUP],
  --hostname NAME, -i.
  --vsock: a vsock device. Host programs connect to the Unix socket PATH and send
           `CONNECT <port>`; the guest's connections to host port P reach PATH_P.
  --snapshot-dir: where to write a snapshot when the guest asks for one (then stop, by default)
  Console escape: Ctrl-A x stops the VM.";

const RESTORE_USAGE: &str = "usage: shards vm restore DIR [--hold] [--vsock PATH] [--no-console] [--snapshot-dir DIR [--snapshot-then stop|resume]]
       shards vm restore DIR [--hold] [WORKLOAD OPTIONS] -- COMMAND [ARG...]
       shards vm restore DIR --warm FD
  Resumes the VM in snapshot directory DIR. With a COMMAND, DIR is a template saved by
  `shards vm run --rootfs`, and the command runs there as `docker run` would.
  Workload options, as for `docker run`: -e NAME[=VALUE], -w DIR, -u USER[:GROUP],
  --hostname NAME, -i.
  --vsock: this VM's vsock socket; without a COMMAND, required when the snapshot has a
           vsock device.
  --hold: prepare the VM, print `shards-ready` on stderr, and start it when a line arrives
          on stdin: a warm VM whose start costs only the release. With a COMMAND, the VM
          resumes at once and connects, and the line runs the command: a warm VM whose
          request costs only the command.
  --warm: resume at once and connect, then take one command, with the stdio and connection
          of the client it is for, from the daemon on the Unix socket at descriptor FD.
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
    workload: Options,
    /// Whether a workload option was given.
    workload_options: bool,
}

impl Common {
    fn new() -> Common {
        Common {
            console: Console::Stdout,
            snapshot_dir: None,
            then: AfterSnapshot::Stop,
            vsock: None,
            workload: Options::default(),
            workload_options: false,
        }
    }

    /// Takes `arg` if it is a shared option; returns whether it was.
    fn option(
        &mut self,
        arg: &str,
        value: &mut dyn FnMut(&str) -> Result<String, String>,
    ) -> Result<bool, String> {
        let w = &mut self.workload;
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
            "-e" | "--env" => w.env.push(value("--env")?),
            "-w" | "--workdir" => w.workdir = value("--workdir")?,
            "-u" | "--user" => w.user = value("--user")?,
            "--hostname" => w.hostname = Some(value("--hostname")?),
            "-i" | "--interactive" => w.interactive = true,
            _ => return Ok(false),
        }
        self.workload_options |= matches!(
            arg,
            "-e" | "--env" | "-w" | "--workdir" | "-u" | "--user" | "--hostname" | "-i" | "--interactive"
        );
        Ok(true)
    }

    fn policy(&mut self) -> Option<SnapshotPolicy> {
        let then = self.then;
        self.snapshot_dir.take().map(|dir| SnapshotPolicy { dir, then })
    }

    /// Checks that workload options come with a command.
    fn check_workload(&self) -> Result<(), String> {
        if self.workload_options && self.workload.argv.is_empty() {
            return Err("-e, -w, -u, --hostname and -i need a command after --".into());
        }
        Ok(())
    }
}

/// What `vm run` boots.
enum Mode {
    /// The kernel and init as given.
    Plain,
    /// Into an image, for a command.
    Workload(PathBuf),
    /// Into an image, to save it as a template.
    Template(PathBuf),
    /// Into an image, as a warm VM that takes one command from the daemon on `fd`;
    /// with a snapshot policy, it saves a template on the way.
    Warm { rootfs: PathBuf, fd: i32 },
}

struct Run {
    cfg: Config,
    mode: Mode,
    workload: Options,
}

fn parse_run(args: impl Iterator<Item = OsString>) -> Result<Run, String> {
    let mut args = utf8(args);
    let mut rootfs = None;
    let mut cfg = config(PathBuf::new(), None);
    let (mut kernel, mut common, mut warm) = (None, Common::new(), None);
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
            "--warm" => {
                let fd = value("--warm")?;
                warm = Some(
                    fd.parse::<i32>()
                        .map_err(|_| format!("--warm: {fd:?} is not a descriptor"))?,
                );
            }
            "--" => {
                common.workload.argv = args.by_ref().collect::<Result<_, _>>()?;
                break;
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    common.check_workload()?;
    cfg.kernel = kernel.ok_or("--kernel is required")?;
    cfg.snapshot = common.policy();
    cfg.console = common.console;
    cfg.vsock = common.vsock;
    let command = !common.workload.argv.is_empty();
    if let Some(fd) = warm {
        let rootfs = rootfs.ok_or("--warm needs --rootfs")?;
        if command || common.workload_options || cfg.vsock.is_some() {
            return Err(
                "--warm takes its command from the daemon: no --vsock, workload options or command".into(),
            );
        }
        // A template saved on the way resumes to serve its request.
        if let Some(policy) = cfg.snapshot.as_mut() {
            policy.then = AfterSnapshot::Resume;
        }
        return Ok(Run {
            cfg,
            mode: Mode::Warm { rootfs, fd },
            workload: common.workload,
        });
    }
    let mode = match rootfs {
        Some(rootfs) if command => Mode::Workload(rootfs),
        Some(rootfs) if cfg.snapshot.is_some() => Mode::Template(rootfs),
        Some(_) => return Err("--rootfs needs a command after --, or --snapshot-dir for a template".into()),
        None if command => return Err("a command needs --rootfs".into()),
        None => Mode::Plain,
    };
    Ok(Run {
        cfg,
        mode,
        workload: common.workload,
    })
}

struct Restore {
    cfg: RestoreConfig,
    workload: Options,
    /// `--warm FD`: the daemon's socket.
    warm: Option<i32>,
}

fn parse_restore(args: impl Iterator<Item = OsString>) -> Result<Restore, String> {
    let mut args = utf8(args);
    let (mut dir, mut common, mut hold, mut warm) = (None, Common::new(), false, None);
    while let Some(arg) = args.next() {
        let arg = arg?;
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"))?;
        if common.option(&arg, &mut value)? {
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => return Err(String::new()),
            "--hold" => hold = true,
            "--warm" => {
                let fd = value("--warm")?;
                warm = Some(
                    fd.parse::<i32>()
                        .map_err(|_| format!("--warm: {fd:?} is not a descriptor"))?,
                );
            }
            "--" => {
                common.workload.argv = args.by_ref().collect::<Result<_, _>>()?;
                break;
            }
            flag if flag.starts_with('-') => return Err(format!("unknown argument {flag:?}")),
            _ if dir.is_some() => return Err(format!("unexpected argument {arg:?}")),
            _ => dir = Some(PathBuf::from(arg)),
        }
    }
    common.check_workload()?;
    if warm.is_some()
        && (hold
            || common.workload_options
            || !common.workload.argv.is_empty()
            || common.snapshot_dir.is_some()
            || common.vsock.is_some())
    {
        return Err("--warm takes its command from the daemon: no --hold, --vsock, --snapshot-dir, workload options or command".into());
    }
    let cfg = RestoreConfig {
        dir: dir.ok_or("the snapshot directory is required")?,
        console: common.console,
        snapshot: common.policy(),
        hold,
        vsock: common.vsock,
    };
    Ok(Restore {
        cfg,
        workload: common.workload,
        warm,
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

/// A VM with shards' defaults: 1 CPU, 256 MiB, and a console on stdout.
pub fn config(kernel: PathBuf, init: Option<PathBuf>) -> Config {
    Config {
        kernel,
        initrd: None,
        init,
        cmdline: "console=ttyS0 earlycon panic=-1".into(),
        vcpus: 1,
        memory_mib: 256,
        console: Console::Stdout,
        disks: Vec::new(),
        snapshot: None,
        pmem: Vec::new(),
        vsock: None,
    }
}

/// Boots `cfg` into the image `rootfs` and runs `workload` there, as `vm run --rootfs`
/// does. Exits as the workload does.
pub fn run_in(mut cfg: Config, rootfs: PathBuf, workload: &Options) -> ExitCode {
    boot_into(&mut cfg, rootfs, false);
    cfg.console = Console::Discard;
    let source = Source::Given {
        options: workload,
        hold: false,
    };
    serve_workload(cfg.vsock.clone(), source, move |vsock| {
        cfg.vsock = Some(vsock);
        vm::start(&cfg)
    })
}

/// Boots into the image `rootfs`: /dev/pmem0, which shards-init mounts as the root.
fn boot_into(cfg: &mut Config, rootfs: PathBuf, template: bool) {
    cfg.pmem.insert(0, rootfs);
    // A workload's output is shards' output, so the kernel need not print to the console.
    // Without automatic task groups: in most templates, a restored guest's first setsid(2)
    // stalled until the next tick (docs/benchmarks.md, "Run"). Docker's containers never
    // get them anyway, since they live in cgroups (kernel/sched/autogroup.c).
    cfg.cmdline.push_str(" quiet noautogroup shards_root=/dev/pmem0");
    if template {
        cfg.cmdline.push_str(" shards_template=1");
    }
}

pub fn run(args: impl Iterator<Item = OsString>) -> ExitCode {
    let Run {
        mut cfg,
        mode,
        workload,
    } = match parsed(parse_run(args), RUN_USAGE) {
        Ok(run) => run,
        Err(code) => return code,
    };
    match mode {
        Mode::Plain => supervise(vm::start(&cfg), cfg.console, false),
        Mode::Template(rootfs) => {
            boot_into(&mut cfg, rootfs, true);
            with_vsock(cfg.vsock.clone(), |vsock| {
                // Restored copies dial the host through this device.
                cfg.vsock = Some(vsock);
                supervise(vm::start(&cfg), cfg.console, true)
            })
        }
        Mode::Workload(rootfs) => run_in(cfg, rootfs, &workload),
        Mode::Warm { rootfs, fd } => warm_boot(cfg, rootfs, fd),
    }
}

pub fn restore(args: impl Iterator<Item = OsString>) -> ExitCode {
    let Restore {
        mut cfg,
        workload,
        warm,
    } = match parsed(parse_restore(args), RESTORE_USAGE) {
        Ok(restore) => restore,
        Err(code) => return code,
    };
    if let Some(fd) = warm {
        return warm_restore(cfg, fd);
    }
    if !workload.argv.is_empty() {
        cfg.console = Console::Discard;
        // A held run resumes now, and holds the command instead: whatever a restored guest
        // does first (its reseed, its connection) is done before the request.
        let hold = std::mem::take(&mut cfg.hold);
        let source = Source::Given {
            options: &workload,
            hold,
        };
        return serve_workload(cfg.vsock.clone(), source, move |vsock| {
            cfg.vsock = Some(vsock);
            vm::restore(&cfg)
        });
    }
    let started = vm::restore(&cfg);
    if cfg.hold
        && let Ok((handle, _)) = &started
    {
        wait_for_release(handle);
    }
    supervise(started, cfg.console, false)
}

/// `--hold`: announces the prepared VM and releases it when a line (or end of input)
/// arrives on stdin.
fn wait_for_release(handle: &Handle) {
    let _ = writeln!(std::io::stderr(), "shards-ready");
    let _ = std::io::stdin().read_line(&mut String::new());
    handle.release();
}

/// Calls `f` with a vsock socket path: `given`, or one in a private directory that lives
/// as long as the call.
#[cfg(unix)]
fn with_vsock(given: Option<PathBuf>, f: impl FnOnce(PathBuf) -> ExitCode) -> ExitCode {
    if let Some(path) = given {
        return f(path);
    }
    match crate::workload::SocketDir::new() {
        Ok(dir) => f(dir.path().join("vsock")),
        Err(e) => {
            report(format!("socket directory: {e}"));
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(unix))]
fn with_vsock(_: Option<PathBuf>, _: impl FnOnce(PathBuf) -> ExitCode) -> ExitCode {
    report("images need vsock, which shards does not support on this platform yet");
    ExitCode::from(125)
}

/// Where a served VM's command comes from.
// Where shards has no vsock yet (Windows), nothing serves a command.
#[cfg_attr(not(unix), allow(dead_code))]
enum Source<'a> {
    /// The command line's: sent once the guest connects or, with `hold`, once a line then
    /// arrives on stdin.
    Given { options: &'a Options, hold: bool },
    /// A warm VM's: one request from the daemon (warm.rs).
    #[cfg(unix)]
    Warm(crate::warm::Link),
}

/// A warm VM's restore: `vm restore DIR --warm FD`.
#[cfg(unix)]
fn warm_restore(mut cfg: RestoreConfig, fd: i32) -> ExitCode {
    let daemon = match crate::warm::Link::new(fd) {
        Ok(link) => link,
        Err(e) => {
            report(e);
            return ExitCode::FAILURE;
        }
    };
    cfg.console = Console::Discard;
    serve_workload(None, Source::Warm(daemon), move |vsock| {
        cfg.vsock = Some(vsock);
        vm::restore(&cfg)
    })
}

#[cfg(not(unix))]
fn warm_restore(_: RestoreConfig, _: i32) -> ExitCode {
    report("warm VMs need Unix sockets, which shards does not support on this platform yet");
    ExitCode::from(125)
}

/// A warm VM's boot: `vm run --rootfs IMAGE [--snapshot-dir DIR] --warm FD`.
#[cfg(unix)]
fn warm_boot(mut cfg: Config, rootfs: PathBuf, fd: i32) -> ExitCode {
    let daemon = match crate::warm::Link::new(fd) {
        Ok(link) => link,
        Err(e) => {
            report(e);
            return ExitCode::FAILURE;
        }
    };
    let template = cfg.snapshot.is_some();
    boot_into(&mut cfg, rootfs, template);
    cfg.console = Console::Discard;
    serve_workload(None, Source::Warm(daemon), move |vsock| {
        cfg.vsock = Some(vsock);
        vm::start(&cfg)
    })
}

#[cfg(not(unix))]
fn warm_boot(_: Config, _: PathBuf, _: i32) -> ExitCode {
    report("warm VMs need Unix sockets, which shards does not support on this platform yet");
    ExitCode::from(125)
}

/// Runs a workload in the VM `start` starts, whose vsock device it gives the socket path
/// for. Exits as the workload does.
#[cfg(unix)]
fn serve_workload(
    vsock: Option<PathBuf>,
    source: Source<'_>,
    start: impl FnOnce(PathBuf) -> Result<(Handle, Running), String>,
) -> ExitCode {
    use crate::workload::{self, NOT_RUN, Request};
    let failed = |e: String| {
        report(e);
        ExitCode::from(NOT_RUN)
    };
    /// The command, resolved before anything starts.
    enum Command {
        Given {
            spec: shards_abi::run::Spec,
            interactive: bool,
            hold: bool,
        },
        Warm(crate::warm::Link),
    }
    let command = match source {
        Source::Given { options, hold } => match workload::spec(options) {
            Ok(spec) => Command::Given {
                spec,
                interactive: options.interactive,
                hold,
            },
            Err(e) => return failed(e),
        },
        Source::Warm(daemon) => Command::Warm(daemon),
    };
    with_vsock(vsock, |vsock| {
        let listeners = workload::listen(&vsock, shards_abi::run::PORT)
            .and_then(|run| Ok((run, workload::listen(&vsock, shards_abi::run::SIGNAL_PORT)?)));
        let (listener, signals) = match listeners {
            Ok(l) => l,
            Err(e) => return failed(format!("listening for the guest: {e}")),
        };
        // A warm VM's signals come from its client. The command line's are this process's:
        // blocked before the VM's threads start, so that they inherit the mask.
        let to_guest = workload::ToGuest::default();
        let warm = matches!(command, Command::Warm(_));
        if let Command::Given { interactive, .. } = &command {
            // SAFETY: isatty(3) on this process's stdin.
            let reads_terminal = *interactive && unsafe { libc::isatty(0) } == 1;
            if let Err(e) = workload::forward_signals(to_guest.clone(), reads_terminal) {
                return failed(e);
            }
        }
        let (handle, running) = match start(vsock) {
            Ok(started) => started,
            Err(e) => return failed(e),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let timing = std::sync::Arc::new(workload::Timing::default());
        let served_timing = timing.clone();
        let stopper = handle.clone();
        let served = std::thread::Builder::new()
            .name("workload".into())
            .spawn(move || {
                // How the workload ended, and whether its client has been told.
                let outcome = match command {
                    Command::Given {
                        spec,
                        interactive,
                        hold,
                    } => {
                        // Held, the guest is connected and waiting: the request is a line
                        // on stdin.
                        let gate = || {
                            let _ = writeln!(std::io::stderr(), "shards-ready");
                            let _ = std::io::stdin().read_line(&mut String::new());
                            Ok((spec.clone(), interactive))
                        };
                        let request = if hold {
                            Request::Later(&gate)
                        } else {
                            Request::Now {
                                spec: spec.clone(),
                                interactive,
                            }
                        };
                        (
                            workload::serve(&listener, signals, request, &to_guest, &served_timing),
                            false,
                        )
                    }
                    Command::Warm(link) => {
                        let client = std::sync::OnceLock::new();
                        let ask = || {
                            let request = crate::warm::receive(&link, &to_guest)?;
                            served_timing
                                .asked
                                .store(request.timing, std::sync::atomic::Ordering::Relaxed);
                            let _ = client.set(request.client);
                            Ok((request.spec, request.interactive))
                        };
                        let served = workload::serve(
                            &listener,
                            signals,
                            Request::Later(&ask),
                            &to_guest,
                            &served_timing,
                        );
                        match client.get() {
                            Some(connection) => {
                                let timing = served_timing
                                    .asked
                                    .load(std::sync::atomic::Ordering::Relaxed)
                                    .then(|| timing_json(&stopper, Some(&served_timing)));
                                crate::warm::finish(&link, connection, &served, timing.as_deref());
                                (served, true)
                            }
                            // The daemon went without a request: nobody will ever send one.
                            None => {
                                stopper.stop();
                                (served, false)
                            }
                        }
                    }
                };
                let _ = tx.send(outcome);
            });
        if let Err(e) = served {
            handle.stop();
            return failed(format!("workload thread: {e}"));
        }
        let reason = running.wait(handle.clone());
        // A warm VM's client printed its timing line from the exit status.
        if !warm {
            report_timing(&handle, Some(&timing));
        }
        if let ExitReason::Error(e) = &reason {
            report(e);
        }
        // The guest waits for its status to be read before it powers off, so the relay
        // has finished unless the guest never ran the command.
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            // A warm VM's client has its status, and any error on its stderr: the warm
            // VM's own status says that it served.
            Ok((_, true)) => ExitCode::SUCCESS,
            Ok((Ok(status), false)) => ExitCode::from(status),
            Ok((Err(e), false)) => failed(e),
            Err(_) => failed("the guest stopped without running the command".into()),
        }
    })
}

#[cfg(not(unix))]
fn serve_workload(
    _: Option<PathBuf>,
    _: Source<'_>,
    _: impl FnOnce(PathBuf) -> Result<(Handle, Running), String>,
) -> ExitCode {
    report("running a command in a microVM needs vsock, which shards does not support on this platform yet");
    ExitCode::from(125)
}

/// The workload's request and answer times, where there is a workload.
#[cfg(unix)]
type WorkloadTiming = crate::workload::Timing;
#[cfg(not(unix))]
type WorkloadTiming = ();

/// With `SHARDS_TIMING` set: one machine-readable line on stderr, for benchmark harnesses.
fn report_timing(handle: &Handle, workload: Option<&WorkloadTiming>) {
    if std::env::var_os("SHARDS_TIMING").is_none() {
        return;
    }
    let _ = writeln!(
        std::io::stderr(),
        "shards-timing {}",
        timing_json(handle, workload)
    );
}

/// The timing line's fields, as JSON: the VM's clock readings in microseconds since the
/// process started, 0 for any not yet taken, and the process's peak RSS so far in KiB.
fn timing_json(handle: &Handle, workload: Option<&WorkloadTiming>) -> String {
    let markers: Vec<String> = handle
        .markers()
        .iter()
        .map(|(m, t)| format!("[{m},{t}]"))
        .collect();
    #[cfg(unix)]
    let (request, answered) = workload.map_or((0, 0), |t| {
        (
            t.request_us.get().copied().unwrap_or(0),
            t.answered_us.get().copied().unwrap_or(0),
        )
    });
    #[cfg(not(unix))]
    let (request, answered) = {
        let _ = workload;
        (0, 0)
    };
    format!(
        "{{\"released_us\":{},\"entry_us\":{},\"exit_us\":{},\"request_us\":{request},\"answered_us\":{answered},\"rss_kib\":{},\"markers\":[{}]}}",
        handle.released_at_us().unwrap_or(0),
        handle.entered_at_us().unwrap_or(0),
        handle.exited_at_us().unwrap_or(0),
        max_rss_kib(),
        markers.join(",")
    )
}

/// This process's peak resident set so far, in KiB; 0 where unknown.
fn max_rss_kib() -> u64 {
    #[cfg(unix)]
    {
        // SAFETY: getrusage(2) into a zeroed rusage, a valid out-parameter.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
            return 0;
        }
        // ru_maxrss is bytes on macOS and KiB on Linux (getrusage(2) on each).
        let rss = u64::try_from(usage.ru_maxrss).unwrap_or(0);
        if cfg!(target_vendor = "apple") { rss / 1024 } else { rss }
    }
    #[cfg(not(unix))]
    0
}

/// Runs a started VM to its end: console, timing report, exit code. A template must end
/// in its snapshot.
fn supervise(started: Result<(Handle, Running), String>, console: Console, template: bool) -> ExitCode {
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
    report_timing(&handle, None);
    match reason {
        ExitReason::Snapshotted => ExitCode::SUCCESS,
        ExitReason::PowerOff | ExitReason::Stopped if template => {
            report("the guest stopped before its template was saved");
            ExitCode::FAILURE
        }
        ExitReason::PowerOff | ExitReason::Stopped => ExitCode::SUCCESS,
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
