//! shardsd: the daemon, pulls and the guest. The `shards` command runs it for every
//! command but `run` and the container commands, which it serves through the daemon, and
//! `vm`, which is shards-vm's (src/bin/shards-vm): the daemon starts shards-vm for each
//! microVM too.

use std::io::Write;
use std::process::ExitCode;

#[cfg(unix)]
mod containers;
#[cfg(unix)]
mod daemon;
mod guest;
mod kernel;
#[cfg(unix)]
mod names;
mod pull;
mod run;
mod spec;

const USAGE: &str = "usage: shards <command> [args...]

Commands:
  daemon      Serve `run` from warm microVMs (`run` starts it when needed)
  guest       Choose the kernel and shards-init that `run` boots
  pull        Pull an image from a registry
  run         Run a command in a new microVM booted into an image
  vm run      Boot a kernel directly in a microVM
  vm restore  Resume a microVM from a snapshot
  version     Print version information";

fn main() -> ExitCode {
    shards_vmm::log::init();
    let mut args = std::env::args_os().skip(1);
    let command = args.next();
    match command.as_ref().and_then(|c| c.to_str()) {
        #[cfg(unix)]
        Some("daemon") => daemon::daemon(args),
        #[cfg(not(unix))]
        Some("daemon") => {
            usage_error("the daemon needs Unix sockets, which shards does not support on this platform yet")
        }
        Some("guest") => guest::guest(args),
        Some("pull") => pull::pull(args),
        Some("run") => usage_error("run: the `shards` command runs commands, through the daemon"),
        Some("vm") => usage_error("vm: the `shards` command runs microVMs, through shards-vm"),
        Some("version" | "--version") => {
            let _ = writeln!(std::io::stdout(), "shards {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("help" | "-h" | "--help") | None => {
            let _ = writeln!(std::io::stdout(), "{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => usage_error(&format!("unknown command {other:?}")),
    }
}

fn usage_error(message: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "shards: {message}\n{USAGE}");
    ExitCode::from(2)
}
