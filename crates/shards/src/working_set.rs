//! A restore's working set, kept by its spawner (D30): `shards restore DIR` on Linux starts
//! this writer before it becomes the VM, whose restore records the pages its guest touches
//! where the snapshot's generation has none, and hands them over (`--working-set FD`).
//! Kept beside the generation as the daemon keeps a warm VM's, the next restore of that
//! generation maps them ahead, each a stage-2 fault its guest does not take (PM M310).

use std::io::{Read as _, Write as _};
use std::os::fd::FromRawFd as _;
use std::path::PathBuf;
use std::process::ExitCode;

/// `shards working-set --template DIR`: reads what the VM hands it on descriptor 3 until
/// the VM closes it, and keeps it beside the generation of the snapshot in DIR it names,
/// as a restore would take it (`vm::keep_working_set`); nothing where the VM recorded
/// none. Not for people to run.
pub fn keeper() -> ExitCode {
    let failed = |e: &str| {
        let _ = writeln!(std::io::stderr(), "shards: the working set: {e}");
        ExitCode::FAILURE
    };
    let mut args = std::env::args_os().skip_while(|a| a != "--template").skip(1);
    let Some(dir) = args.next().map(PathBuf::from) else {
        return failed("--template DIR is required");
    };
    // SAFETY: fcntl(2) asks whether descriptor 3 is open.
    if unsafe { libc::fcntl(3, libc::F_GETFD) } < 0 {
        return failed("descriptor 3 is not the VM's");
    }
    // SAFETY: the pipe's end the CLI left at descriptor 3 for this process alone.
    let from_vm = unsafe { std::fs::File::from_raw_fd(3) };
    let limit = match shards_vmm::vm::working_set_limit(&dir) {
        // A generation's name past it, and its newline.
        Ok(limit) => limit.saturating_add(256),
        Err(e) => return failed(&e),
    };
    let mut handed = Vec::new();
    if let Err(e) = from_vm.take(limit.saturating_add(1)).read_to_end(&mut handed) {
        return failed(&format!("reading it from the VM: {e}"));
    }
    if handed.is_empty() {
        return ExitCode::SUCCESS;
    }
    if handed.len() as u64 > limit {
        return failed(&format!("the VM handed more than {limit} bytes"));
    }
    match shards_vmm::vm::keep_working_set(&dir, &handed) {
        Ok(pages) => {
            shards_vmm::debug!("kept a working set of {pages} pages beside {}", dir.display());
            ExitCode::SUCCESS
        }
        Err(e) => failed(&e),
    }
}
