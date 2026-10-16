//! `shards`: one binary for every command. The command line (cli/) reads `run`, the
//! container and image commands as the Docker CLI reads them and asks the daemon for
//! them; everything else is this file's [`shardsd`]: the daemon itself (which this binary
//! becomes when it starts one), builds, the guest. Each microVM runs in a VM process of
//! its own (src/bin/shards-vm), which the daemon starts.

use std::io::Write;
use std::process::ExitCode;

mod agent;
mod agentfile;
#[cfg(all(feature = "alloc-count", unix))]
mod alloc_count;
mod aws;
mod build;
mod cli;
#[cfg(unix)]
mod containers;
mod context;
#[cfg(unix)]
mod daemon;
// The daemon's (Unix's alone as yet) and a search's.
#[cfg_attr(not(unix), allow(dead_code))]
mod filters;
mod from_image;
mod frontend;
mod google;
#[cfg(any(target_os = "macos", windows))]
mod grant;
#[cfg(any(target_os = "macos", windows))]
mod grant_answer;
// The VM's half, which the daemon's tests answer.
#[cfg(all(test, any(target_os = "macos", windows)))]
mod grant_ask;
mod guest;
mod helpers;
mod kernel;
mod keyless;
#[cfg(unix)]
mod local_store;
#[cfg(unix)]
mod logging;
mod login;
#[cfg(unix)]
mod names;
mod netproc;
#[cfg(unix)]
mod networks;
mod oidc;
mod pull;
mod resources;
mod run;
mod search;
#[cfg(unix)]
mod segments;
mod setup;
#[cfg(unix)]
mod share;
mod spec;
mod spiffe;
#[cfg(unix)]
mod volumes;

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
        Some(kind @ ("agent" | "harness" | "mcp")) => agent::command(kind, args),
        // the VM's broker (`shards run --kernel`, `shards restore`): not for people to run.
        #[cfg(target_os = "macos")]
        Some("grants") => grant_answer::broker(),
        // a run's share process (D38), which the daemon starts: not for people to run.
        #[cfg(unix)]
        Some("share") => share::share(args),
        // the daemon's spawner (shards_ipc::start_spawner, PM M158): not for people to run.
        #[cfg(unix)]
        Some("spawner") => match shards_ipc::serve_spawner() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "shards spawner: {e}");
                ExitCode::FAILURE
            }
        },
        Some("build") => build::build(args),
        Some("login") => login::login(args),
        Some("logout") => login::logout(args),
        Some("search") => search::search(args),
        // BuildKit's frontend, which BuildKit runs from shards' image (D113): not for people
        // to run.
        Some("frontend") => frontend::frontend(args),
        Some("policy") => build::policy_command::command(args),
        Some("agentfile") => from_image::command(args),
        Some("run") => usage_error("run: the `shards` command runs commands, through the daemon"),
        Some("version") => cli::version::run(&args.collect::<Vec<_>>()),
        Some("--version") => cli::version::short(),
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

/// Says how much of phase `_phase`'s time went to `_part`, over `_bytes`, for the work
/// measured with the feature `alloc-count` (PM M194).
pub(crate) fn split(_phase: &str, _part: &str, _took: std::time::Duration, _bytes: usize) {
    #[cfg(all(feature = "alloc-count", unix))]
    {
        let _ = writeln!(
            std::io::stderr(),
            "alloc-split {_phase} {_part} ms={:.1} bytes={_bytes}",
            _took.as_secs_f64() * 1e3
        );
    }
}

/// This process's variables a log driver that ships to a cloud finds its credentials by,
/// `NAME=VALUE`: AWS's (D168) and Google's (D169), and the home, `PATH` and proxies. A
/// client gives them with a run, so that its credentials are found as its shell would.
pub(crate) fn cloud_environment() -> Vec<String> {
    std::env::vars_os()
        .filter_map(|(k, v)| {
            let (k, v) = (k.into_string().ok()?, v.into_string().ok()?);
            (aws::reads(&k) || google::reads(&k)).then(|| format!("{k}={v}"))
        })
        .collect()
}

/// A random UUID, version 4 (RFC 9562 §5.4): a splunk channel's (D167), a DPoP proof's
/// (D168).
pub(crate) fn random_uuid() -> std::io::Result<String> {
    let mut b = [0u8; 16];
    shards_vmm::platform::fill_random(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        hex.get(..8).unwrap_or_default(),
        hex.get(8..12).unwrap_or_default(),
        hex.get(12..16).unwrap_or_default(),
        hex.get(16..20).unwrap_or_default(),
        hex.get(20..).unwrap_or_default()
    ))
}

fn usage_error(message: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "shards: {message}\n{USAGE}");
    ExitCode::from(2)
}
