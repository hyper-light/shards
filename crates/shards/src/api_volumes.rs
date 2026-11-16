//! `shards api-volumes`: a home's volumes as shards-api reads and clears them
//! (docs/design/architecture.md D281; `shards_api::volumes`): `list`, each volume of the home's
//! store and the bytes its files take, as `system df` counts them, with its bound and what its
//! files take by its bound's measure; `clear NAME`, the volume
//! removed with its files, as `volume rm` removes one, once no run holds it. Each answered in
//! one frame on stdout. Not for people to run.

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
/// not all be read (`Store::size`'s -1); its bound; and what its files take by that bound's
/// measure (`Store::charge`, D281), none where they could not all be read.
fn every(store: &Store) -> Vec<HomeVolume> {
    store
        .list()
        .into_iter()
        .map(|v| HomeVolume {
            bytes: u64::try_from(store.size(&v.name)).ok(),
            bound: v.bound().ok().flatten(),
            charge: store.charge(&v).ok(),
            created_ns: u64::try_from(v.created).unwrap_or(u64::MAX),
            anonymous: v.is_anonymous(),
            name: v.name,
        })
        .collect()
}

/// Volume `name` removed from `store` with its files, as `volume rm` removes one, under the
/// store's lock: refused `InUse` at once where a run that mounts or plugged it holds it (D281),
/// a daemon's or an orchestrator's (`Store::remove_unheld`); refused by why.
fn clear(store: &Store, name: &str) -> Result<(), (ClearCause, String)> {
    Store::valid(name).map_err(|e| (ClearCause::Name, format!("{name}: {e}")))?;
    let _held = crate::volumes::lock();
    store.remove_unheld(name)
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

    /// D281: a volume is listed with its bound and what its files take by its measure: walked
    /// where no run holds it, a name a unit and a length its whole units; and while a run holds
    /// it bounded, the count its servers share as they left it, walked again by none.
    #[test]
    fn a_volume_is_listed_with_its_bound_and_its_count() {
        use std::sync::atomic::Ordering;
        let dir = shards_testdir::TempDir::new("api-volumes").unwrap();
        let store = Store::new(dir.path());
        let none = std::collections::BTreeMap::new();
        let sized = std::collections::BTreeMap::from([("size".to_string(), "1M".to_string())]);
        {
            let _held = crate::volumes::lock();
            store.create("plain", &none, &none, "").unwrap();
            store.create("sized", &none, &sized, "").unwrap();
        }
        std::fs::write(store.data("sized").join("f"), [7u8; 10]).unwrap();
        let listed = every(&store);
        let (plain, sized) = (&listed[0], &listed[1]);
        assert_eq!((plain.bound, plain.charge), (None, Some(0)));
        let walked = sized.charge.unwrap();
        assert_eq!(sized.bound, Some(1 << 20));
        assert!(walked > 10, "a unit and f's whole units: {walked}");
        let mut run = crate::volumes::Holds::default();
        run.take(&store, "sized").unwrap();
        let (_, count) = run
            .count(&store, "sized", &store.data("sized"), None)
            .unwrap()
            .unwrap();
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&count)
            .unwrap();
        let server = shards_vmm::devices::virtio::fs::bound::mapped(&file).unwrap();
        server.fetch_add(4096, Ordering::Relaxed);
        assert_eq!(every(&store)[1].charge, Some(walked + 4096), "its servers' count");
        drop(run);
        assert_eq!(
            every(&store)[1].charge,
            Some(walked),
            "walked again once none holds it"
        );
    }

    /// D281: a volume a run holds (its mount's or plug's hold, `Store::hold`), by one run or two,
    /// is refused its clearing `InUse` at once, its files kept; once every hold is let go of, it
    /// is cleared. A hold asked while a clearing holds it is refused at once.
    #[test]
    fn a_volume_a_run_holds_is_refused_its_clearing_until_it_lets_go() {
        let dir = shards_testdir::TempDir::new("api-volumes").unwrap();
        let store = Store::new(dir.path());
        let none = std::collections::BTreeMap::new();
        {
            let _held = crate::volumes::lock();
            store.create("agent-a", &none, &none, "").unwrap();
        }
        let run = store.hold("agent-a").unwrap().unwrap();
        let other = store.hold("agent-a").unwrap().unwrap();
        assert!(matches!(clear(&store, "agent-a"), Err((ClearCause::InUse, _))));
        assert!(store.data("agent-a").exists());
        drop(run);
        assert!(matches!(clear(&store, "agent-a"), Err((ClearCause::InUse, _))));
        drop(other);
        let clearing = store.alone("agent-a").unwrap().unwrap();
        assert!(store.hold("agent-a").unwrap().is_none());
        drop(clearing);
        assert_eq!(clear(&store, "agent-a"), Ok(()));
        assert!(!store.data("agent-a").exists());
        assert!(every(&store).is_empty(), "a hold's file is no volume");
    }

    /// A home with no volume yet lists none.
    #[test]
    fn a_home_without_volumes_lists_none() {
        let dir = shards_testdir::TempDir::new("api-volumes").unwrap();
        assert!(every(&Store::new(dir.path())).is_empty());
    }
}
