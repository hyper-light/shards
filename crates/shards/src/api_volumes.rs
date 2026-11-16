//! `shards api-volumes`: a home's volumes as shards-api reads and clears them
//! (docs/design/architecture.md D281; `shards_api::volumes`): `list`, each volume of the home's
//! store and the bytes its files take, as `system df` counts them; `clear NAME`, the volume
//! removed with its files, as `volume rm` removes one. Each answered in one frame on stdout.
//! Not for people to run.

use std::ffi::OsString;
use std::io::Write as _;
use std::process::ExitCode;

use shards_api::volumes::{ClearCause, HomeVolume, cleared, listed};

use crate::volumes::Store;

pub(crate) fn command(args: impl Iterator<Item = OsString>) -> ExitCode {
    let args: Vec<OsString> = args.collect();
    let answered = shards_ipc::home().and_then(|home| {
        let store = Store::new(&home);
        let encoded = match args.iter().map(|a| a.to_str()).collect::<Vec<_>>().as_slice() {
            [Some("list")] => listed(&every(&store)),
            [Some("clear"), Some(name)] => cleared(&clear(&store, name)),
            _ => return Err(format!("asked {args:?}: `list` or `clear NAME`")),
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
            let _ = writeln!(std::io::stderr(), "shards api-volumes: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Every volume of `store`, by name, each with the bytes its files take: none where they could
/// not all be read (`Store::size`'s -1).
fn every(store: &Store) -> Vec<HomeVolume> {
    store
        .list()
        .into_iter()
        .map(|v| HomeVolume {
            bytes: u64::try_from(store.size(&v.name)).ok(),
            created_ns: u64::try_from(v.created).unwrap_or(u64::MAX),
            anonymous: v.is_anonymous(),
            name: v.name,
        })
        .collect()
}

/// Volume `name` removed from `store` with its files, under the store's lock, as `volume rm`
/// removes one; refused by why.
fn clear(store: &Store, name: &str) -> Result<(), (ClearCause, String)> {
    Store::valid(name).map_err(|e| (ClearCause::Name, format!("{name}: {e}")))?;
    let _held = crate::volumes::lock();
    if store.get(name).is_none() {
        return Err((ClearCause::Unknown, format!("get {name}: no such volume")));
    }
    store.remove(name).map_err(|e| (ClearCause::Store, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A volume's bytes are its files' as `system df` counts them, each once; one cleared is
    /// gone with its files, and asked again, refused `Unknown`; a name the store takes none by,
    /// `Name`; what is left listed as it was.
    #[test]
    fn a_volume_is_listed_with_its_bytes_and_cleared_with_its_files() {
        let dir = shards_testdir::TempDir::new("api-volumes").unwrap();
        let store = Store::new(dir.path());
        let none = std::collections::BTreeMap::new();
        for name in ["agent-a", "agent-b"] {
            let _held = crate::volumes::lock();
            store.create(name, &none, &none, "").unwrap();
        }
        std::fs::write(store.data("agent-a").join("cache"), [7u8; 1000]).unwrap();
        std::fs::create_dir(store.data("agent-a").join("deeper")).unwrap();
        std::fs::write(store.data("agent-a").join("deeper/more"), [7u8; 24]).unwrap();
        let listed = every(&store);
        let seen: Vec<(&str, Option<u64>, bool)> = listed
            .iter()
            .map(|v| (v.name.as_str(), v.bytes, v.anonymous))
            .collect();
        assert_eq!(
            seen,
            [("agent-a", Some(1024), false), ("agent-b", Some(0), false)]
        );
        assert!(listed.iter().all(|v| v.created_ns > 0));
        assert_eq!(clear(&store, "agent-a"), Ok(()));
        assert!(!store.data("agent-a").exists());
        assert!(matches!(clear(&store, "agent-a"), Err((ClearCause::Unknown, _))));
        for bad in ["a", "../agent-b", "a/b", ""] {
            assert!(
                matches!(clear(&store, bad), Err((ClearCause::Name, _))),
                "{bad:?}"
            );
        }
        let left: Vec<String> = every(&store).into_iter().map(|v| v.name).collect();
        assert_eq!(left, ["agent-b"]);
    }

    /// A home with no volume yet lists none.
    #[test]
    fn a_home_without_volumes_lists_none() {
        let dir = shards_testdir::TempDir::new("api-volumes").unwrap();
        assert!(every(&Store::new(dir.path())).is_empty());
    }
}
