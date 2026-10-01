//! shards-vm: one microVM per process, booted (`run`) or resumed from a snapshot
//! (`restore`), for `shards vm` and for the daemon's templates and warm VMs. It links the
//! VMM and what a VM process runs, its workload's relay and its link to the daemon, and
//! nothing else of shards: a process maps and relocates its whole binary as it starts, and
//! shardsd's registry, TLS and image code cost each VM about 1 MiB of memory
//! (docs/research/platform-measurements.md M34).

use std::io::Write;
use std::process::ExitCode;

#[path = "../../confine.rs"]
mod confine;
#[cfg(target_os = "macos")]
#[path = "../../grant.rs"]
mod grant;
#[cfg(target_os = "macos")]
#[path = "../../grant_ask.rs"]
mod grant_ask;
// Made by the daemon alone; the logger's tests make them as it does.
#[cfg(all(test, unix))]
#[path = "../../segments.rs"]
mod segments;
#[path = "../../spec.rs"]
mod spec;
#[path = "../../terminal.rs"]
mod terminal;
#[path = "../../vm_run.rs"]
mod vm_run;
#[cfg(unix)]
#[path = "../../warm.rs"]
mod warm;
#[path = "../../workload.rs"]
mod workload;

fn main() -> ExitCode {
    shards_vmm::log::init();
    // Before it reads anything: its arguments name files it has yet to open.
    if let Err(e) = confine::confine() {
        let _ = writeln!(std::io::stderr(), "shards-vm: {e}");
        return ExitCode::from(125);
    }
    let mut args = std::env::args_os().skip(1);
    match args.next().as_ref().and_then(|c| c.to_str()) {
        Some("run") => vm_run::run(args),
        Some("restore") => vm_run::restore(args),
        other => {
            let _ = writeln!(
                std::io::stderr(),
                "shards-vm: unknown command {other:?}; `shards vm run` and `shards vm restore` run it"
            );
            ExitCode::from(2)
        }
    }
}
