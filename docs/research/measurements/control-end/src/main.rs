//! Where agent control terminates, measured (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step
//! 10; README.md): a microVM's control over D180's QUIC and TLS (AES-256-GCM alone), ended in
//! the guest and reached through shards-net (arm g), or ended at a host endpoint that carries
//! each stream on over vsock (arm h).

mod client;
mod endpoint;
mod guest;
mod image;
#[cfg(test)]
mod loopback;
mod procs;
mod proto;
mod record;
mod run;
mod serve;

use std::io::Write as _;
use std::process::ExitCode;

const USAGE: &str = "usage:
  control-end run --shards PATH --guest PATH --out DIR [OPTION VALUE]...   the measurement (README.md)
  control-end endpoint --controller HEX --sock-dir DIR --vms N [--listen ADDR]   arm h's endpoint
  control-end guest quic --controller HEX | guest idle | guest check   in a daemon run's microVM
  control-end spin N   N busy threads, a load
  (PID 1 of a directly booted microVM: control_end=vsock|idle on its kernel's command line)";

fn main() -> ExitCode {
    // PID 1 of a directly booted microVM, told its mode on its kernel's command line; not a
    // daemon run's command, PID 1 of its own PID namespace under shards-init.
    #[cfg(target_os = "linux")]
    if std::process::id() == 1 && std::env::var_os("control_end").is_some() {
        guest::pid1();
    }
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let rest = args.get(1..).unwrap_or_default();
    let result = match args.first().map(String::as_str) {
        Some("run") => run::main(rest),
        Some("endpoint") => endpoint::main(rest),
        Some("guest") => guest::main(rest),
        Some("spin") => run::spin_main(rest),
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "control-end: {e}");
            ExitCode::FAILURE
        }
    }
}
