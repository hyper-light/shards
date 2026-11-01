//! A restore's working set, kept by its spawner (D30): `shards restore DIR`, whose restore
//! records the pages its guest touches where the snapshot's generation has none, and hands
//! them over (`--working-set`). On Linux the CLI starts this writer as a process of its own
//! before it becomes the VM; on Windows, where the CLI waits for its VM as a child, it is a
//! thread of the CLI's. Kept beside the generation as the daemon keeps a warm VM's, the next
//! restore of that generation maps them ahead, each a stage-2 fault its guest does not take
//! (PM M310).

use std::io::Read as _;
use std::path::Path;

/// `shards working-set --template DIR` (Linux): reads what the VM hands it on descriptor 3
/// until the VM closes it, and keeps it ([`keep`]). Not for people to run.
#[cfg(target_os = "linux")]
pub fn keeper() -> std::process::ExitCode {
    use std::io::Write as _;
    use std::os::fd::FromRawFd as _;
    let failed = |e: &str| {
        let _ = writeln!(std::io::stderr(), "shards: the working set: {e}");
        std::process::ExitCode::FAILURE
    };
    let mut args = std::env::args_os().skip_while(|a| a != "--template").skip(1);
    let Some(dir) = args.next().map(std::path::PathBuf::from) else {
        return failed("--template DIR is required");
    };
    // SAFETY: fcntl(2) asks whether descriptor 3 is open.
    if unsafe { libc::fcntl(3, libc::F_GETFD) } < 0 {
        return failed("descriptor 3 is not the VM's");
    }
    // SAFETY: the pipe's end the CLI left at descriptor 3 for this process alone.
    let from_vm = unsafe { std::fs::File::from_raw_fd(3) };
    match keep(from_vm, &dir) {
        Ok(_) => std::process::ExitCode::SUCCESS,
        Err(e) => failed(&e),
    }
}

/// Reads what the VM hands over until it closes its end, and keeps it beside the generation
/// of the snapshot in `dir` it names, as a restore would take it (`vm::keep_working_set`):
/// the pages kept, none where the VM recorded none.
pub fn keep(from_vm: impl std::io::Read, dir: &Path) -> Result<Option<usize>, String> {
    // A generation's name past it, and its newline.
    let limit = shards_vmm::vm::working_set_limit(dir)?.saturating_add(256);
    let mut handed = Vec::new();
    from_vm
        .take(limit.saturating_add(1))
        .read_to_end(&mut handed)
        .map_err(|e| format!("reading it from the VM: {e}"))?;
    if handed.is_empty() {
        return Ok(None);
    }
    if handed.len() as u64 > limit {
        return Err(format!("the VM handed more than {limit} bytes"));
    }
    let pages = shards_vmm::vm::keep_working_set(dir, &handed)?;
    shards_vmm::debug!("kept a working set of {pages} pages beside {}", dir.display());
    Ok(Some(pages))
}
