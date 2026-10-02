//! `shards vm run` boots a kernel directly in a microVM; `shards vm restore` resumes one
//! from a snapshot. Either can run a command in an image as `docker run` does
//! (workload.rs): `vm run --rootfs IMAGE -- COMMAND` boots into the image for it, and
//! `vm run --rootfs IMAGE --snapshot-dir DIR` saves a template, booted and mounted, that
//! `vm restore DIR -- COMMAND` resumes for each command.

use std::ffi::OsString;
use std::fmt::Display;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use shards_vmm::vm::{
    self, AfterSnapshot, Config, Console, Disk, ExitReason, Handle, RestoreConfig, Running, SnapshotPolicy,
    VsockHost,
};

use crate::spec::Options;
use crate::terminal::RawTerminal;

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

/// An argument that is text, with an error naming it if it is not UTF-8. Paths are taken
/// as the OS gives them: a home's may be any bytes on Linux.
fn text(arg: OsString) -> Result<String, String> {
    arg.into_string()
        .map_err(|a| format!("argument {a:?} is not valid UTF-8"))
}

/// A `--disk` value: its path, and whether it ends in `:ro`.
fn disk(spec: OsString) -> Disk {
    let bytes = spec.as_encoded_bytes();
    match bytes.strip_suffix(b":ro") {
        Some(path) => Disk {
            // SAFETY: split just before `:ro`, a non-empty UTF-8 substring, which
            // `OsStr::from_encoded_bytes_unchecked` documents as a valid boundary.
            path: PathBuf::from(unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(path) }),
            read_only: true,
        },
        None => Disk {
            path: PathBuf::from(spec),
            read_only: false,
        },
    }
}

/// Options `run` and `restore` share.
struct Common {
    console: Console,
    snapshot_dir: Option<PathBuf>,
    then: AfterSnapshot,
    vsock: Option<PathBuf>,
    /// A network device, and its network process's side (D31).
    #[cfg(unix)]
    net: Option<shards_vmm::devices::virtio::net::NetHost>,
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
            #[cfg(unix)]
            net: None,
            workload: Options::default(),
            workload_options: false,
        }
    }

    /// Takes `arg` if it is a shared option; returns whether it was.
    fn option(
        &mut self,
        arg: &str,
        value: &mut dyn FnMut(&str) -> Result<OsString, String>,
    ) -> Result<bool, String> {
        let w = &mut self.workload;
        match arg {
            "--no-console" => self.console = Console::Discard,
            "--snapshot-dir" => self.snapshot_dir = Some(PathBuf::from(value("--snapshot-dir")?)),
            "--snapshot-then" => {
                self.then = match text(value("--snapshot-then")?)?.as_str() {
                    "stop" => AfterSnapshot::Stop,
                    "resume" => AfterSnapshot::Resume,
                    other => return Err(format!("--snapshot-then: {other:?} is not stop or resume")),
                }
            }
            "--vsock" => self.vsock = Some(PathBuf::from(value("--vsock")?)),
            #[cfg(unix)]
            "--net" => {
                if self.net.is_some() {
                    return Err("--net given twice".into());
                }
                self.net = Some(net_host(&text(value("--net")?)?)?);
            }
            "--cwd" => {
                let dir = PathBuf::from(value("--cwd")?);
                if !dir.is_absolute() {
                    return Err(format!("--cwd: {} is not an absolute path", dir.display()));
                }
                CWD.set(dir).map_err(|_| "--cwd given twice".to_string())?;
            }
            "--grants" => {
                let fd = text(value("--grants")?)?;
                let fd = fd
                    .parse::<i32>()
                    .map_err(|_| format!("--grants: {fd:?} is not a descriptor"))?;
                grants(fd)?;
            }
            "-e" | "--env" => w.env.push(text(value("--env")?)?),
            "-w" | "--workdir" => w.workdir = text(value("--workdir")?)?,
            "-u" | "--user" => w.user = text(value("--user")?)?,
            "--hostname" => w.hostname = Some(text(value("--hostname")?)?),
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
        self.snapshot_dir.take().map(|dir| SnapshotPolicy {
            dir,
            then,
            working_set: false,
        })
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
    let mut args = args;
    let mut rootfs = None;
    let mut cfg = Config::new(PathBuf::new(), None);
    let (mut kernel, mut common, mut warm) = (None, Common::new(), None);
    while let Some(arg) = args.next() {
        let arg = text(arg)?;
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        if common.option(&arg, &mut value)? {
            continue;
        }
        match arg.as_str() {
            "--kernel" => kernel = Some(PathBuf::from(value("--kernel")?)),
            "--initrd" => cfg.initrd = Some(PathBuf::from(value("--initrd")?)),
            "--init" => cfg.init = Some(PathBuf::from(value("--init")?)),
            "--cmdline" => cfg.cmdline = text(value("--cmdline")?)?,
            "--cpus" => {
                cfg.vcpus = text(value("--cpus")?)?
                    .parse()
                    .map_err(|e| format!("--cpus: {e}"))?
            }
            "--memory" => {
                cfg.memory_mib = text(value("--memory")?)?
                    .parse()
                    .map_err(|e| format!("--memory: {e}"))?
            }
            "--disk" => cfg.disks.push(disk(value("--disk")?)),
            "--pmem" => cfg.pmem.push(PathBuf::from(value("--pmem")?)),
            "--rootfs" => rootfs = Some(PathBuf::from(value("--rootfs")?)),
            "--warm" => {
                let fd = text(value("--warm")?)?;
                warm = Some(
                    fd.parse::<i32>()
                        .map_err(|_| format!("--warm: {fd:?} is not a descriptor"))?,
                );
            }
            "--" => {
                common.workload.argv = args.by_ref().map(text).collect::<Result<_, _>>()?;
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
    cfg.vsock = common.vsock.map(VsockHost::at);
    #[cfg(unix)]
    {
        cfg.net = common.net.take();
    }
    let command = !common.workload.argv.is_empty();
    if let Some(fd) = warm {
        let rootfs = rootfs.ok_or("--warm needs --rootfs")?;
        if command || common.workload_options || cfg.vsock.is_some() {
            return Err(
                "--warm takes its command from the daemon: no --vsock, workload options or command".into(),
            );
        }
        // A template saved on the way resumes to serve its request, and records what that
        // touches: the working set its restores prefetch (PM M30).
        if let Some(policy) = cfg.snapshot.as_mut() {
            policy.then = AfterSnapshot::Resume;
            policy.working_set = true;
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
    let mut args = args;
    let (mut dir, mut common, mut hold, mut warm) = (None, Common::new(), false, None);
    while let Some(arg) = args.next() {
        // The snapshot directory is a path, in whatever bytes; every option is text.
        let Some(flag) = arg.to_str().map(str::to_owned) else {
            if dir.is_some() {
                return Err(format!("unexpected argument {arg:?}"));
            }
            dir = Some(PathBuf::from(arg));
            continue;
        };
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        if common.option(&flag, &mut value)? {
            continue;
        }
        match flag.as_str() {
            "-h" | "--help" => return Err(String::new()),
            "--hold" => hold = true,
            "--warm" => {
                let fd = text(value("--warm")?)?;
                warm = Some(
                    fd.parse::<i32>()
                        .map_err(|_| format!("--warm: {fd:?} is not a descriptor"))?,
                );
            }
            "--" => {
                common.workload.argv = args.by_ref().map(text).collect::<Result<_, _>>()?;
                break;
            }
            flag if flag.starts_with('-') => return Err(format!("unknown argument {flag:?}")),
            _ if dir.is_some() => return Err(format!("unexpected argument {flag:?}")),
            _ => dir = Some(PathBuf::from(flag)),
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
        vsock: common.vsock.map(VsockHost::at),
        #[cfg(unix)]
        net: common.net.take(),
        // Restored ahead of its request: the prefetch costs the request nothing.
        prefetch: hold || warm.is_some(),
        // A warm VM's request ends the recording as it ends a template's (RECORD_FOR).
        record: warm.is_some(),
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

/// Boots `cfg` into the image `rootfs` and runs `workload` there, as `vm run --rootfs`
/// does. Exits as the workload does.
pub fn run_in(mut cfg: Config, rootfs: PathBuf, workload: &Options) -> ExitCode {
    boot_into(&mut cfg, rootfs, false);
    cfg.console = Console::Discard;
    let source = Source::Given {
        options: workload,
        hold: false,
    };
    serve_workload(cfg.vsock.take(), source, move |vsock| {
        cfg.vsock = Some(vsock);
        start(&cfg)
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
        Mode::Plain => supervise(start(&cfg), cfg.console, false),
        Mode::Template(rootfs) => {
            boot_into(&mut cfg, rootfs, true);
            // Restored copies dial the host through this device, served by whatever
            // restores them.
            cfg.vsock.get_or_insert_with(VsockHost::default);
            supervise(start(&cfg), cfg.console, true)
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
        return serve_workload(cfg.vsock.take(), source, move |vsock| {
            cfg.vsock = Some(vsock);
            restore_vm(&cfg)
        });
    }
    let started = restore_vm(&cfg);
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
        restore_vm(&cfg)
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
        start(&cfg)
    })
}

#[cfg(not(unix))]
fn warm_boot(_: Config, _: PathBuf, _: i32) -> ExitCode {
    report("warm VMs need Unix sockets, which shards does not support on this platform yet");
    ExitCode::from(125)
}

/// Runs a workload in the VM `start` starts, whose vsock device's host side it gives:
/// `given`, if the command line gave one, and the run's ports, which this process serves.
/// Exits as the workload does.
#[cfg(unix)]
fn serve_workload(
    given: Option<VsockHost>,
    source: Source<'_>,
    start: impl FnOnce(VsockHost) -> Result<(Handle, Running), String>,
) -> ExitCode {
    use crate::spec::NOT_RUN;
    use crate::workload::{self, Request};
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
        Source::Given { options, hold } => match crate::spec::spec(options, |name| std::env::var_os(name)) {
            Ok(spec) => Command::Given {
                spec,
                interactive: options.interactive,
                hold,
            },
            Err(e) => return failed(e),
        },
        Source::Warm(daemon) => Command::Warm(daemon),
    };
    {
        let mut vsock = given.unwrap_or_default();
        let (run_port, listener) = std::sync::mpsc::channel();
        let (signal_port, signals) = std::sync::mpsc::channel();
        vsock.ports.push((shards_abi::run::PORT, run_port));
        vsock.ports.push((shards_abi::run::SIGNAL_PORT, signal_port));
        // A warm VM's signals come from its client. The command line's are this process's:
        // blocked before the VM's threads start, so that they inherit the mask.
        // One workload a process (M34): what its threads share lives as long as they do.
        static TO_GUEST: workload::ToGuest = workload::ToGuest::new(workload::Signals::new());
        static TIMING: workload::Timing = workload::Timing::new();
        let to_guest = &TO_GUEST;
        let warm = matches!(command, Command::Warm(_));
        if let Command::Given { interactive, .. } = &command {
            // SAFETY: isatty(3) on this process's stdin.
            let reads_terminal = *interactive && unsafe { libc::isatty(0) } == 1;
            if let Err(e) = workload::forward_signals(to_guest, reads_terminal) {
                return failed(e);
            }
        }
        let (handle, running) = match start(vsock) {
            Ok(started) => started,
            Err(e) => return failed(e),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let timing = &TIMING;
        let stopper = handle.clone();
        let served = std::thread::Builder::new()
            .name("workload".into())
            .spawn(move || {
                let guest_abi = || stopper.guest_abi();
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
                            Ok(workload::Asked {
                                spec: spec.clone(),
                                interactive,
                                log: None,
                                started: None,
                            })
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
                            workload::serve(&listener, signals, request, to_guest, timing, &guest_abi),
                            false,
                        )
                    }
                    Command::Warm(link) => {
                        let client = std::sync::OnceLock::new();
                        let started = || {
                            crate::warm::started(&link);
                            end_recording_in(&stopper, RECORD_FOR);
                        };
                        let ask = || {
                            // A template this VM saved is in place before the daemon hears the
                            // VM is ready, and settles it: its commit runs as the guest does.
                            stopper.wait_for_snapshot();
                            let request = crate::warm::receive(&link, to_guest)?;
                            timing
                                .asked
                                .store(request.timing, std::sync::atomic::Ordering::Relaxed);
                            let _ = client.set(request.client);
                            Ok(workload::Asked {
                                spec: request.spec,
                                interactive: request.interactive,
                                log: request.log,
                                started: Some(&started),
                            })
                        };
                        let served = workload::serve(
                            &listener,
                            signals,
                            Request::Later(&ask),
                            to_guest,
                            timing,
                            &guest_abi,
                        );
                        // Some once the request came: its client, if it has one.
                        match client.get() {
                            Some(connection) => {
                                let timing = timing
                                    .asked
                                    .load(std::sync::atomic::Ordering::Relaxed)
                                    .then(|| timing_json(&stopper, Some(timing)));
                                // What the run touched goes to the daemon with its end, for the
                                // template it was restored from, which no VM writes (D30). A
                                // failure costs later runs their prefetch, and nothing else.
                                let working_set = stopper.take_working_set().unwrap_or_else(|e| {
                                    shards_vmm::debug!("{e}");
                                    None
                                });
                                crate::warm::finish(
                                    &link,
                                    connection.as_ref(),
                                    &served,
                                    timing.as_deref(),
                                    working_set.as_ref(),
                                );
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
        let reason = running.wait();
        // A warm VM's client printed its timing line from the exit status.
        if !warm {
            report_timing(&handle, Some(timing));
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
            Ok((Ok(ended), false)) => match &ended.not_run {
                // As `docker run` would say it, and exit.
                Some(why) => {
                    let (said, _) = shards_cmdline::commands::start_failed(why);
                    let (text, status) = crate::spec::not_run(&said);
                    let _ = writeln!(std::io::stderr(), "{text}");
                    ExitCode::from(status)
                }
                None => ExitCode::from(ended.status),
            },
            Ok((Err(e), false)) => failed(e),
            Err(_) => failed("the guest stopped without running the command".into()),
        }
    }
}

#[cfg(not(unix))]
fn serve_workload(
    _: Option<VsockHost>,
    _: Source<'_>,
    _: impl FnOnce(VsockHost) -> Result<(Handle, Running), String>,
) -> ExitCode {
    report("running a command in a microVM needs vsock, which shards does not support on this platform yet");
    ExitCode::from(125)
}

/// How long a VM records its working set after its command starts, at most, unless the
/// command answers first: a command still running then has long passed its start. The
/// whole way from the request to the start is recorded, however long a host takes over
/// it (a nested KVM host took 28 ms, PM M33).
/// - HVF: 50 ms. Recording slows the guest about sixfold (a pooled `true` took 9.9 ms
///   against 1.5, PM M30), so this is some 8 ms of the command's own time, past the 5 ms
///   a run should take.
/// - KVM: 1 s. Recording costs nothing, and a nested host took 125 ms to run `true` (PM
///   M33).
#[cfg(unix)]
const RECORD_FOR: std::time::Duration = if shards_vmm::vm::RESTORES_RECORD {
    std::time::Duration::from_secs(1)
} else {
    std::time::Duration::from_millis(50)
};

/// Saves the working set `after` from now, if the VM still records it then.
#[cfg(unix)]
fn end_recording_in(handle: &Handle, after: std::time::Duration) {
    if !handle.recording() {
        return;
    }
    let handle = handle.clone();
    let spawned = std::thread::Builder::new()
        .name("working-set".into())
        .spawn(move || {
            std::thread::sleep(after);
            if let Err(e) = handle.end_recording() {
                shards_vmm::debug!("{e}");
            }
        });
    if let Err(e) = spawned {
        shards_vmm::debug!("the working set's timer: {e}");
    }
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
        "{{\"released_us\":{},\"entry_us\":{},\"exit_us\":{},\"request_us\":{request},\"answered_us\":{answered},\"prefetched\":{},\"rss_kib\":{},\"markers\":[{}]}}",
        handle.released_at_us().unwrap_or(0),
        handle.entered_at_us().unwrap_or(0),
        handle.exited_at_us().unwrap_or(0),
        handle.prefetched(),
        max_rss_kib(),
        markers.join(",")
    )
}

/// This process's peak resident set so far, in KiB; 0 where unknown.
fn max_rss_kib() -> u64 {
    #[cfg(unix)]
    {
        shards_ipc::peak_rss_kib()
    }
    #[cfg(not(unix))]
    0
}

/// Starts the VM `cfg` describes, once this process is confined to the files it names, the
/// snapshot directory it saves to and its vsock socket: on Linux by Landlock, on macOS by
/// what its spawner grants it (D30). A warm VM's container log comes open from its daemon.
fn start(cfg: &Config) -> Result<(Handle, Running), String> {
    // Whole paths: grants and Landlock's rules name each file whole, a VM on macOS runs in
    // its sandbox's container rather than the caller's directory, and a snapshot records
    // its files by absolute path anyway.
    let mut cfg = cfg.clone();
    for path in [Some(&mut cfg.kernel), cfg.initrd.as_mut(), cfg.init.as_mut()]
        .into_iter()
        .flatten()
        .chain(cfg.pmem.iter_mut())
        .chain(cfg.disks.iter_mut().map(|d| &mut d.path))
        .chain(cfg.snapshot.as_mut().map(|p| &mut p.dir))
        .chain(cfg.vsock.as_mut().and_then(|v| v.path.as_mut()))
    {
        *path = absolute(path)?;
    }
    let cfg = &cfg;
    let mut paths = crate::confine::Paths::default();
    paths.read.push(cfg.kernel.clone());
    paths.read.extend(cfg.initrd.iter().cloned());
    paths.read.extend(cfg.init.iter().cloned());
    paths.read.extend(cfg.pmem.iter().cloned());
    for disk in &cfg.disks {
        let list = if disk.read_only {
            &mut paths.read
        } else {
            &mut paths.write
        };
        list.push(disk.path.clone());
    }
    written(
        &mut paths,
        cfg.snapshot.as_ref().map(|p| p.dir.as_path()),
        cfg.vsock.as_ref().and_then(|v| v.path.as_deref()),
    );
    // A host that runs no VM says so, before its files are confined: the check reads no
    // input of the VM's.
    vm::check_host()?;
    confine(&paths)?;
    vm::start(cfg)
}

/// [`start`] for a restore: the snapshot's directory, and the files it restores against.
fn restore_vm(cfg: &RestoreConfig) -> Result<(Handle, Running), String> {
    let mut cfg = cfg.clone();
    for path in std::iter::once(&mut cfg.dir)
        .chain(cfg.snapshot.as_mut().map(|p| &mut p.dir))
        .chain(cfg.vsock.as_mut().and_then(|v| v.path.as_mut()))
    {
        *path = absolute(path)?;
    }
    let cfg = &cfg;
    // A host that runs no VM says so, before its files are confined: the check reads no
    // input of the VM's.
    vm::check_host()?;
    let mut paths = crate::confine::Paths::default();
    // The snapshot says which files it restores against: on macOS it is granted first, to
    // be read for them.
    #[cfg(target_os = "macos")]
    granted_template(&cfg.dir)?;
    #[cfg(not(target_os = "macos"))]
    paths.read_under.push(cfg.dir.clone());
    for (path, read_only) in shards_vmm::snapshot::backing_files(&cfg.dir)? {
        let list = if read_only {
            &mut paths.read
        } else {
            &mut paths.write
        };
        list.push(path);
    }
    written(
        &mut paths,
        cfg.snapshot.as_ref().map(|p| p.dir.as_path()),
        cfg.vsock.as_ref().and_then(|v| v.path.as_deref()),
    );
    confine(&paths)?;
    vm::restore(cfg)
}

/// `path` made whole against the working directory.
fn absolute(path: &Path) -> Result<PathBuf, String> {
    match CWD.get() {
        Some(cwd) if path.is_relative() => Ok(cwd.join(path)),
        _ => std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display())),
    }
}

/// The directory relative paths are of (`--cwd DIR`): `shards vm`'s own on macOS, where App
/// Sandbox starts a VM process in its container instead.
static CWD: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The directory a VM writes in, the snapshot it saves, and the one holding the vsock
/// socket path it was given, if any.
fn written(paths: &mut crate::confine::Paths, snapshot: Option<&Path>, vsock: Option<&Path>) {
    paths.write_under.extend(snapshot.map(Path::to_path_buf));
    // Landlock holds a directory by its inode, so the one a snapshot goes to is made now,
    // as its snapshot would make it (confine.rs, `landlock`). On macOS the spawner makes it
    // as it grants it: a sandboxed process makes nothing outside its grants.
    #[cfg(target_os = "linux")]
    if let Some(dir) = snapshot {
        let _ = std::fs::create_dir_all(dir);
    }
    #[cfg(target_os = "macos")]
    {
        paths.vsock = vsock.map(Path::to_path_buf);
    }
    #[cfg(not(target_os = "macos"))]
    paths
        .sockets_under
        .extend(vsock.and_then(Path::parent).map(Path::to_path_buf));
}

/// The socket this VM asks its spawner for access on (`--grants FD`): one a process, as
/// its VM is.
#[cfg(target_os = "macos")]
static GRANTS: std::sync::OnceLock<std::os::unix::net::UnixStream> = std::sync::OnceLock::new();

/// `--net REGION,WAKE_ME,WAKE_PEER,MAC`: the frame ring and doorbells its spawner left this
/// process at those descriptors, which it owns from here on, and the guest's MAC.
#[cfg(unix)]
fn net_host(spec: &str) -> Result<shards_vmm::devices::virtio::net::NetHost, String> {
    use std::os::fd::{FromRawFd, OwnedFd};
    let parts: Vec<&str> = spec.split(',').collect();
    let [region, me, peer, mac] = parts.as_slice() else {
        return Err(format!("--net {spec}: not REGION,WAKE_ME,WAKE_PEER,MAC"));
    };
    let mut seen = Vec::new();
    let mut adopt = |what: &str, v: &str| -> Result<std::sync::Arc<OwnedFd>, String> {
        let fd: i32 = v
            .parse()
            .map_err(|_| format!("--net: {what} {v:?} is not a descriptor"))?;
        // SAFETY: fcntl(2) asks whether the descriptor is open.
        if fd < 3 || seen.contains(&fd) || unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(format!("--net: {what} {fd} is not a descriptor of its own"));
        }
        seen.push(fd);
        // SAFETY: an open descriptor the spawner left for this process alone, owned from
        // here on, closed on exec.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        // SAFETY: as above.
        Ok(std::sync::Arc::new(unsafe { OwnedFd::from_raw_fd(fd) }))
    };
    let region = adopt("REGION", region)?;
    let wake_me = adopt("WAKE_ME", me)?;
    let wake_peer = adopt("WAKE_PEER", peer)?;
    let octets: Vec<u8> = mac
        .split(':')
        .map(|h| u8::from_str_radix(h, 16))
        .collect::<Result<_, _>>()
        .map_err(|_| format!("--net: {mac:?} is not a MAC"))?;
    let mac: [u8; 6] = octets
        .try_into()
        .map_err(|_| format!("--net: {mac:?} is not a MAC"))?;
    Ok(shards_vmm::devices::virtio::net::NetHost {
        region,
        wake_me,
        wake_peer,
        mac,
    })
}

/// Takes the socket at `fd` to ask the spawner for access on (macOS).
#[cfg(target_os = "macos")]
fn grants(fd: i32) -> Result<(), String> {
    let link = crate::warm::inherited_socket("--grants", fd)?;
    GRANTS.set(link).map_err(|_| "--grants given twice".to_string())
}

#[cfg(not(target_os = "macos"))]
fn grants(_: i32) -> Result<(), String> {
    Err("--grants: macOS alone grants a VM its files".into())
}

/// On macOS, where the VM process is in App Sandbox from its launch, asks its spawner for
/// `wanted` (grant). It fails closed, as Landlock does: a VM process not signed into App
/// Sandbox starts no VM.
#[cfg(target_os = "macos")]
fn ask(wanted: &[crate::grant::Wanted]) -> Result<(), String> {
    if !crate::grant_ask::sandboxed() {
        return Err(
            "shards-vm is not signed into App Sandbox, which confines every VM on macOS: sign it \
             with resources/vm.entitlements"
                .into(),
        );
    }
    let link = GRANTS
        .get()
        .ok_or("nothing was granted to this VM: start it with `shards vm`, which grants it its files")?;
    crate::grant_ask::obtain(link, wanted)
}

/// `paths` as the VM asks for them: files it reads read-only, files it writes read-write,
/// and the directories it writes in made first. A directory cannot be granted read-only
/// (PM M70): a restore asks for its template's files instead.
#[cfg(target_os = "macos")]
fn granted(paths: &crate::confine::Paths) -> Result<(), String> {
    use crate::grant::Access;
    if !paths.read_under.is_empty() {
        return Err("a directory cannot be granted read-only under App Sandbox".into());
    }
    if !paths.sockets_under.is_empty() {
        return Err("a directory of sockets cannot be granted under App Sandbox".into());
    }
    let wanted: Vec<crate::grant::Wanted> = paths
        .read
        .iter()
        .map(|p| (Access::Read, p.clone()))
        .chain(paths.write.iter().map(|p| (Access::Write, p.clone())))
        .chain(paths.write_under.iter().map(|p| (Access::MakeDir, p.clone())))
        .chain(paths.vsock.iter().map(|p| (Access::Listen, p.clone())))
        .collect();
    ask(&wanted)
}

/// Has the spawner dial, beside `vsock`, every host port the guest connects to: App
/// Sandbox lets this process dial nothing outside its container (PM M67).
#[cfg(target_os = "macos")]
fn dial_through_spawner(vsock: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt as _;
    static DIALING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let mut prefix = vsock.as_os_str().as_bytes().to_vec();
    prefix.push(b'_');
    shards_vmm::platform::set_dialer(Box::new(move |target: &Path| {
        let port = target
            .as_os_str()
            .as_bytes()
            .strip_prefix(prefix.as_slice())
            .and_then(|p| std::str::from_utf8(p).ok())
            .and_then(|p| p.parse::<u32>().ok())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a vsock host port"))?;
        let link = GRANTS.get().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotConnected, "no spawner to dial through")
        })?;
        // A request and its answer at a time.
        let _one = DIALING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::grant_ask::dial(link, port)
    }))
}

/// A restore's template, as a VM in App Sandbox is granted it: its pointer, then the
/// files of the generation it names, never its directory.
#[cfg(target_os = "macos")]
fn granted_template(dir: &Path) -> Result<(), String> {
    use crate::grant::Access;
    ask(&[(Access::ReadIfThere, shards_vmm::snapshot::pointer(dir))])?;
    let ([state, memory], working_set) = shards_vmm::snapshot::generation_files(dir)?;
    ask(&[
        (Access::Read, state),
        (Access::Read, memory),
        (Access::ReadIfThere, working_set),
    ])
}

/// Applies `paths` where the OS confines by path: App Sandbox's grants on macOS, Landlock
/// on Linux (D30), beside Linux's seccomp filter, which is on the whole process already.
fn confine(paths: &crate::confine::Paths) -> Result<(), String> {
    // The last it asks for: the link closes, and the spawner grants nothing more, unless
    // the guest's connections to host ports go through it.
    #[cfg(target_os = "macos")]
    return granted(paths).and_then(|()| match &paths.vsock {
        Some(vsock) => dial_through_spawner(vsock),
        None => {
            if let Some(link) = GRANTS.get() {
                let _ = link.shutdown(std::net::Shutdown::Both);
            }
            Ok(())
        }
    });
    #[cfg(target_os = "linux")]
    return crate::confine::landlock(paths);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = paths;
        Ok(())
    }
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
    let reason = running.wait();
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

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt as _;

    fn args(list: &[&[u8]]) -> impl Iterator<Item = OsString> {
        list.iter()
            .map(|a| std::ffi::OsStr::from_bytes(a).to_os_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    /// Paths reach a VM in whatever bytes the OS gave them, as a home on Linux may be
    /// named: the daemon passes its templates' and images' paths under it. Options that
    /// are text must be UTF-8.
    #[test]
    fn paths_are_taken_in_any_bytes() {
        let run = parse_run(args(&[
            b"--kernel",
            b"/h\xff/k",
            b"--init",
            b"/h\xff/i",
            b"--disk",
            b"/h\xff/d:ro",
            b"--disk",
            b"/h\xff/e",
            b"--rootfs",
            b"/h\xff/r",
            b"--snapshot-dir",
            b"/h\xff/t",
            b"--warm",
            b"3",
        ]))
        .unwrap();
        let bytes = |p: &std::path::Path| p.as_os_str().as_bytes().to_vec();
        assert_eq!(bytes(&run.cfg.kernel), b"/h\xff/k");
        assert_eq!(bytes(run.cfg.init.as_deref().unwrap()), b"/h\xff/i");
        let disks: Vec<(Vec<u8>, bool)> = run
            .cfg
            .disks
            .iter()
            .map(|d| (bytes(&d.path), d.read_only))
            .collect();
        assert_eq!(
            disks,
            [(b"/h\xff/d".to_vec(), true), (b"/h\xff/e".to_vec(), false)]
        );
        assert_eq!(bytes(&run.cfg.snapshot.unwrap().dir), b"/h\xff/t");
        let Mode::Warm { rootfs, fd: 3 } = run.mode else {
            panic!("not a warm VM");
        };
        assert_eq!(bytes(&rootfs), b"/h\xff/r");

        let restore = parse_restore(args(&[b"/h\xff/t", b"--vsock", b"/h\xff/v"])).unwrap();
        assert_eq!(bytes(&restore.cfg.dir), b"/h\xff/t");
        assert_eq!(
            bytes(
                restore
                    .cfg
                    .vsock
                    .as_ref()
                    .and_then(|v| v.path.as_deref())
                    .unwrap()
            ),
            b"/h\xff/v"
        );

        let e = parse_run(args(&[b"--kernel", b"/k", b"--cpus", b"\xff"]))
            .err()
            .unwrap();
        assert!(e.contains("not valid UTF-8"), "{e}");
        let e = parse_restore(args(&[b"/t", b"/\xff"])).err().unwrap();
        assert!(e.contains("unexpected argument"), "{e}");
    }
}
