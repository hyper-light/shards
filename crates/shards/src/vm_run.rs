//! `shards vm run`: boot a kernel directly in a microVM.

use std::ffi::OsString;
use std::fmt::Display;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use shards_vmm::vm::{self, Config, Console, Disk, ExitReason};

const USAGE: &str = "usage: shards vm run --kernel PATH [--initrd PATH | --init PATH] [--cmdline STR] [--cpus N] [--memory MIB] [--disk PATH[:ro]]... [--no-console]
  Console escape: Ctrl-A x stops the VM.";

fn parse_args(args: impl Iterator<Item = OsString>) -> Result<Config, String> {
    let mut args = args.map(|a| {
        a.into_string()
            .map_err(|a| format!("argument {a:?} is not valid UTF-8"))
    });
    let mut cfg = Config {
        kernel: PathBuf::new(),
        initrd: None,
        init: None,
        cmdline: "console=ttyS0 earlycon panic=-1".into(),
        vcpus: 1,
        memory_mib: 256,
        console: Console::Stdout,
        disks: Vec::new(),
    };
    let mut kernel = None;
    while let Some(arg) = args.next() {
        let arg = arg?;
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"))?;
        match arg.as_str() {
            "--kernel" => kernel = Some(PathBuf::from(value("--kernel")?)),
            "--initrd" => cfg.initrd = Some(PathBuf::from(value("--initrd")?)),
            "--init" => cfg.init = Some(PathBuf::from(value("--init")?)),
            "--cmdline" => cfg.cmdline = value("--cmdline")?,
            "--cpus" => cfg.vcpus = value("--cpus")?.parse().map_err(|e| format!("--cpus: {e}"))?,
            "--memory" => {
                cfg.memory_mib = value("--memory")?.parse().map_err(|e| format!("--memory: {e}"))?
            }
            "--no-console" => cfg.console = Console::Discard,
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
    Ok(cfg)
}

/// Puts the controlling terminal in raw mode so keystrokes reach the guest unchanged;
/// restores it on drop.
struct RawTerminal(libc::termios);

impl RawTerminal {
    fn enable() -> Option<RawTerminal> {
        // SAFETY: termios calls on fd 0 with a zero-initialized struct they fill in.
        unsafe {
            if libc::isatty(0) != 1 {
                return None;
            }
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            (libc::tcsetattr(0, libc::TCSANOW, &raw) == 0).then_some(RawTerminal(saved))
        }
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        // SAFETY: restores the attributes saved by `enable`.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
    }
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

pub fn run(args: impl Iterator<Item = OsString>) -> ExitCode {
    let cfg = match parse_args(args) {
        Ok(cfg) => cfg,
        Err(e) if e.is_empty() => {
            let _ = writeln!(std::io::stdout(), "{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            report(format!("{e}\n{USAGE}"));
            return ExitCode::from(2);
        }
    };
    let (handle, running) = match vm::start(&cfg) {
        Ok(v) => v,
        Err(e) => {
            report(e);
            return ExitCode::FAILURE;
        }
    };
    let terminal = (cfg.console == Console::Stdout)
        .then(RawTerminal::enable)
        .flatten();
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
