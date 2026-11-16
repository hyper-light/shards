//! `shards api-check`: whether this host runs microVMs now, as shards-api asks it
//! (docs/design/architecture.md D262; `shards_api::host_check`): the VMM's own check, the one
//! each VM process makes before it boots, answered in one frame on stdout. Not for people to
//! run.

use std::io::Write as _;
use std::process::ExitCode;

use shards_api::host::{HostCheck, answer};

pub(crate) fn command() -> ExitCode {
    let check = HostCheck::of(shards_vmm::hv::backend(), shards_vmm::vm::check_host());
    let written = answer(&check).map_err(|e| e.to_string()).and_then(|frame| {
        let mut out = std::io::stdout().lock();
        out.write_all(&frame)
            .and_then(|()| out.flush())
            .map_err(|e| format!("its answer: {e}"))
    });
    match written {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "shards api-check: {e}");
            ExitCode::FAILURE
        }
    }
}
