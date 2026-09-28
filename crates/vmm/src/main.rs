use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use shards_vmm::vm::{self, Config, Console, ExitReason};

const USAGE: &str = "usage: shards-vmm --kernel PATH [--initrd PATH] [--cmdline STR] [--cpus N] [--memory MIB] [--no-console]
  Console escape: Ctrl-A x stops the VM.";

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Config, String> {
    let mut cfg = Config {
        kernel: PathBuf::new(),
        initrd: None,
        cmdline: "console=ttyS0 earlycon panic=-1".into(),
        vcpus: 1,
        memory_mib: 256,
        console: Console::Stdout,
    };
    let mut kernel = None;
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--kernel" => kernel = Some(PathBuf::from(value("--kernel")?)),
            "--initrd" => cfg.initrd = Some(PathBuf::from(value("--initrd")?)),
            "--cmdline" => cfg.cmdline = value("--cmdline")?,
            "--cpus" => cfg.vcpus = value("--cpus")?.parse().map_err(|e| format!("--cpus: {e}"))?,
            "--memory" => cfg.memory_mib = value("--memory")?.parse().map_err(|e| format!("--memory: {e}"))?,
            "--no-console" => cfg.console = Console::Discard,
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

fn forward_stdin(handle: vm::Handle) {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 256];
        let mut escape = false;
        loop {
            let n = match stdin.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            for &b in &buf[..n] {
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
}

fn main() -> ExitCode {
    shards_vmm::log::init();
    let cfg = match parse_args(std::env::args().skip(1)) {
        Ok(cfg) => cfg,
        Err(e) if e.is_empty() => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("shards-vmm: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let (handle, running) = match vm::start(&cfg) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("shards-vmm: {e}");
            return ExitCode::FAILURE;
        }
    };
    let terminal = (cfg.console == Console::Stdout).then(RawTerminal::enable).flatten();
    forward_stdin(handle.clone());
    let reason = running.wait(handle);
    drop(terminal);
    match reason {
        ExitReason::PowerOff | ExitReason::Stopped => ExitCode::SUCCESS,
        ExitReason::Reset => {
            eprintln!("shards-vmm: guest requested a reset");
            ExitCode::from(3)
        }
        ExitReason::Error(e) => {
            eprintln!("shards-vmm: {e}");
            ExitCode::FAILURE
        }
    }
}
