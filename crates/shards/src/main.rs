//! `shards`: one binary for every command. The command line (cli/) reads `run`, the
//! container and image commands as the Docker CLI reads them and asks the daemon for
//! them; everything else is this file's [`shardsd`]: the daemon itself (which this binary
//! becomes when it starts one), builds, the guest. Each microVM runs in a VM process of
//! its own (src/bin/shards-vm), which the daemon starts.

use std::io::Write;
use std::process::ExitCode;

#[cfg(all(feature = "alloc-count", unix))]
mod alloc_count;
mod build;
mod cli;
#[cfg(unix)]
mod containers;
#[cfg(unix)]
mod daemon;
#[cfg(target_os = "macos")]
mod grant;
#[cfg(target_os = "macos")]
mod grant_answer;
// The VM's half, which the daemon's tests answer.
#[cfg(all(test, target_os = "macos"))]
mod grant_ask;
mod guest;
mod helpers;
mod kernel;
#[cfg(unix)]
mod names;
#[cfg(unix)]
mod netproc;
mod pull;
mod run;
#[cfg(unix)]
mod segments;
mod spec;

const USAGE: &str = "usage: shards <command> [args...]

Commands:
  daemon      Serve `run` from warm microVMs (`run` starts it when needed)
  guest       Choose the kernel and shards-init that `run` boots
  build       Build an image from a Dockerfile
  run         Run a command in a new microVM booted into an image
  vm run      Boot a kernel directly in a microVM
  vm restore  Resume a microVM from a snapshot
  version     Print version information";

fn main() -> ExitCode {
    cli::main()
}

/// The daemon side's commands, `args` after `shards`: the daemon, builds, the guest.
pub(crate) fn shardsd(args: Vec<std::ffi::OsString>) -> ExitCode {
    shards_vmm::log::init();
    let mut args = args.into_iter();
    let command = args.next();
    match command.as_ref().and_then(|c| c.to_str()) {
        #[cfg(unix)]
        Some("daemon") => daemon::daemon(args),
        #[cfg(not(unix))]
        Some("daemon") => {
            usage_error("the daemon needs Unix sockets, which shards does not support on this platform yet")
        }
        Some("guest") => guest::guest(args),
        // `shards vm`'s broker: not for people to run.
        #[cfg(target_os = "macos")]
        Some("grants") => grant_answer::broker(),
        Some("build") => build::build(args),
        Some("run") => usage_error("run: the `shards` command runs commands, through the daemon"),
        Some("vm") => usage_error("vm: the `shards` command runs microVMs, through shards-vm"),
        Some("version" | "--version") => {
            #[cfg(unix)]
            if let Some(p) = cli::look::styled() {
                cli::look::version(&p, &mut std::io::stdout().lock());
                return ExitCode::SUCCESS;
            }
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

/// Ends a phase of the work measured with the feature `alloc-count`, which prints what
/// the phase allocated; without it, nothing.
#[inline]
pub(crate) fn phase(_name: &str) {
    #[cfg(all(feature = "alloc-count", unix))]
    alloc_count::phase(_name);
}

fn usage_error(message: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "shards: {message}\n{USAGE}");
    ExitCode::from(2)
}
