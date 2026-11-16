//! `shards api-networks`: a home's networks as shards-api makes and removes them
//! (docs/design/architecture.md D285 slice 6; `shards_api::networks`), with no daemon: `create
//! NAME`, made as `network create NAME` makes one (`networks::made::create`), its subnet clear
//! of every network of the home's and of the default network's that this host elects for its
//! runs (D31, as `api-run` elects it); `remove NAME`, its leases then its record removed
//! (`networks::made::remove_named`), once no run of any process holds it. Each answered in one
//! frame on stdout. Not for people to run.

use std::ffi::OsString;
use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use shards_api::networks::{CreateCause, RemoveCause, created, removed};

use crate::networks::made::{self, Kept, Unmade};

pub(crate) fn command(args: impl Iterator<Item = OsString>) -> ExitCode {
    let args: Vec<OsString> = args.collect();
    let answered = shards_ipc::home().and_then(|home| {
        let encoded = match args.iter().map(|a| a.to_str()).collect::<Vec<_>>().as_slice() {
            [Some("create"), Some(name)] => created(&create(&home, name)?),
            [Some("remove"), Some(name)] => removed(&made::remove_named(&home, name).map_err(kept)),
            _ => return Err(format!("asked {args:?}: `create NAME` or `remove NAME`")),
        };
        let frame = encoded.map_err(|e| format!("its answer: {e}"))?;
        let mut out = std::io::stdout().lock();
        out.write_all(&frame)
            .and_then(|()| out.flush())
            .map_err(|e| format!("its answer: {e}"))
    });
    match answered {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "shards api-networks: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Network `name` made in `home` as `network create NAME` makes one: its ID, or why not by a
/// cause shards-api reads. A refusal of its flags or settings, which a name alone never meets,
/// is this command's failure, said on its stderr.
fn create(home: &Path, name: &str) -> Result<Result<String, (CreateCause, String)>, String> {
    let bridge = shards_net::bridge::elected_here(&mut |note| {
        let _ = writeln!(std::io::stderr(), "shards api-networks: {note}");
    });
    match made::create(home, bridge, &made::Create::named(name)) {
        Ok(n) => Ok(Ok(n.id)),
        Err((Unmade::Name, why)) => Ok(Err((CreateCause::Name, why))),
        Err((Unmade::Exists, why)) => Ok(Err((CreateCause::Exists, why))),
        Err((Unmade::Pool, why)) => Ok(Err((CreateCause::Pool, why))),
        Err((Unmade::Host, why)) => Ok(Err((CreateCause::Host, why))),
        Err((Unmade::Flags | Unmade::Settings, why)) => Err(format!("network {name}: {why}")),
    }
}

/// Why a network was kept, as shards-api reads it.
fn kept((kept, why): (Kept, String)) -> (RemoveCause, String) {
    let cause = match kept {
        Kept::Unknown => RemoveCause::Unknown,
        Kept::Shards => RemoveCause::Shards,
        Kept::InUse => RemoveCause::InUse,
        Kept::Host => RemoveCause::Host,
    };
    (cause, why)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D285 slice 6: a network made by its name is answered by its ID, the home's record of
    /// it; made again, `Exists`; removed by its name, gone; removed again, `Unknown`; one of
    /// shards' own, `Shards`, whatever the home holds.
    #[test]
    fn a_network_is_made_and_removed_by_its_name_alone() {
        let dir = shards_testdir::TempDir::new("api-nets").unwrap();
        let home = dir.path();
        let id = create(home, "team").unwrap().unwrap();
        let recorded: Vec<String> = crate::networks::Store::new(home)
            .list()
            .into_iter()
            .filter(|n| n.name == "team")
            .map(|n| n.id)
            .collect();
        assert_eq!(recorded, [id]);
        assert!(matches!(create(home, "team"), Ok(Err((CreateCause::Exists, _)))));
        assert!(matches!(create(home, "bridge"), Ok(Err((CreateCause::Name, _)))));
        assert_eq!(made::remove_named(home, "team").map_err(kept), Ok(()));
        assert!(matches!(
            made::remove_named(home, "team").map_err(kept),
            Err((RemoveCause::Unknown, _))
        ));
        assert!(matches!(
            made::remove_named(home, "none").map_err(kept),
            Err((RemoveCause::Shards, _))
        ));
    }
}
