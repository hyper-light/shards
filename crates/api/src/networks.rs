//! A home's networks shared by runs (docs/design/architecture.md D285 slice 6), as an
//! orchestrator makes the named network kiln's `Change::Connect` names, and removes it:
//! [`Networks::create`] and [`Networks::remove`]. Each asks `shards api-networks`, which makes
//! or removes one by the daemon's own path (`network create NAME`'s; `network rm`'s removal,
//! the network's leases then its record, under the home's networks lock), with no daemon, and
//! answers in one frame.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use crate::command::{Command, NotStarted};
use crate::image::wire::{Reader, WireError, Writer, frame};

/// The frame kind of `shards api-networks create NAME`'s answer.
pub const CREATED: u8 = 1;

/// The frame kind of `shards api-networks remove NAME`'s answer.
pub const REMOVED: u8 = 2;

/// The answers' version, each one's first byte: changed only by appending.
pub const VERSION: u8 = 1;

/// Why the home made no network, as `shards api-networks` answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateCause {
    /// A name no user's network takes: none, or one of shards' own (`bridge`, `host`, `none`,
    /// `default`).
    Name = 1,
    /// A network is named so in the home already: made before, by this orchestrator or by
    /// another of its processes.
    Exists = 2,
    /// No subnet of dockerd's pools is free of the home's networks and the host's.
    Pool = 3,
    /// The home's files, its networks lock or the host's random source refused.
    Host = 4,
}

/// Why the home removed no network, as `shards api-networks` answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveCause {
    /// No network is named so.
    Unknown = 1,
    /// One of shards' own networks, which stay.
    Shards = 2,
    /// A run holds it (D285): a run on it of any process, the daemon's or an orchestrator's,
    /// a process linking its runs, or a network taking its settings from it. It is kept; its
    /// runs are never waited for.
    InUse = 3,
    /// The home's files or its networks lock refused.
    Host = 4,
}

/// Why a network was not made or removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkRefusal {
    /// The home made none, by why, in its words.
    Create(CreateCause, String),
    /// The home removed none, by why, in its words.
    Remove(RemoveCause, String),
    /// The `shards` binary did not start: its path and the OS's words.
    Start(String),
    /// What this process could not make for it.
    Os {
        what: &'static str,
        kind: std::io::ErrorKind,
    },
    /// It ended without its answer: its exit status where it was reaped, and its last
    /// diagnostics.
    Ended { status: Option<i32>, said: String },
    /// Its answer's version, which this side does not read: another shards'.
    Version(u8),
    /// Its answer is not one this side reads.
    Wire(WireError),
}

impl std::fmt::Display for NetworkRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Create(cause, why) => write!(f, "the network's making: {cause:?}: {why}"),
            Self::Remove(cause, why) => write!(f, "the network's removal: {cause:?}: {why}"),
            Self::Start(why) => write!(f, "starting shards api-networks: {why}"),
            Self::Os { what, kind } => write!(f, "{what}: {kind}"),
            Self::Ended { status, said } => {
                f.write_str("shards api-networks ended without its answer")?;
                if let Some(status) = status {
                    write!(f, " (status {status})")?;
                }
                if !said.is_empty() {
                    write!(f, ": {}", said.trim_end())?;
                }
                Ok(())
            }
            Self::Version(v) => write!(
                f,
                "shards api-networks answered version {v}; this side reads {VERSION}"
            ),
            Self::Wire(e) => write!(f, "shards api-networks' answer: {e}"),
        }
    }
}

impl std::error::Error for NetworkRefusal {}

impl From<NotStarted> for NetworkRefusal {
    fn from(e: NotStarted) -> NetworkRefusal {
        match e {
            NotStarted::Start(why) => NetworkRefusal::Start(why),
            NotStarted::Os { what, kind } => NetworkRefusal::Os { what, kind },
        }
    }
}

/// The networks of the home `env` names (`SHARDS_HOME`, else its default), as `binary` (else
/// `SHARDS_BIN`, else `shards` on `PATH`) keeps them.
#[derive(Debug, Clone, Copy)]
pub struct Networks<'a> {
    pub binary: Option<&'a Path>,
    pub env: &'a [(OsString, OsString)],
}

impl Networks<'_> {
    /// Network `name` made, as `network create NAME` makes one: a bridge network of IPv4
    /// alone, its subnet the lowest of dockerd's pools that meets no other network of the
    /// home's, the default network's subnet among them, nor the host's own. Its ID. Refused
    /// [`CreateCause::Exists`] where the home has a network so named already, which its caller
    /// may take as made.
    pub fn create(&self, name: &str) -> Result<String, NetworkRefusal> {
        let payload = self.ask(&[OsStr::new("create"), OsStr::new(name)], CREATED)?;
        read_created(&payload)
    }

    /// Network `name` removed, its leases then its record, as `network rm` removes one, found
    /// by its name alone, never by its ID's beginning. Refused [`RemoveCause::InUse`] at once
    /// while a run is on it, of any process, never waiting for that run to end.
    pub fn remove(&self, name: &str) -> Result<(), NetworkRefusal> {
        let payload = self.ask(&[OsStr::new("remove"), OsStr::new(name)], REMOVED)?;
        read_removed(&payload)
    }

    /// `shards api-networks ARGS…`'s one answer, of frame kind `kind`.
    fn ask(&self, args: &[&OsStr], kind: u8) -> Result<Vec<u8>, NetworkRefusal> {
        let mut all = vec![OsStr::new("api-networks")];
        all.extend_from_slice(args);
        let mut command = Command::start(self.binary, &all, self.env)?;
        let answer = command.frame();
        let ended = command.end();
        match answer {
            Ok(Some((k, payload))) if k == kind => Ok(payload),
            Ok(Some((k, _))) => Err(NetworkRefusal::Wire(WireError::Tag {
                what: "a networks answer's frame",
                tag: k,
            })),
            Ok(None) | Err(_) => Err(NetworkRefusal::Ended {
                status: ended.status,
                said: ended.said,
            }),
        }
    }
}

/// `result` as `shards api-networks create NAME` answers it: one frame, the network's ID or
/// why none was made.
pub fn created(result: &Result<String, (CreateCause, String)>) -> Result<Vec<u8>, WireError> {
    let mut w = Writer(Vec::new());
    w.u8(VERSION);
    match result {
        Ok(id) => {
            w.u8(0);
            w.text(id)?;
        }
        Err((cause, why)) => {
            w.u8(*cause as u8);
            w.text(why)?;
        }
    }
    frame(CREATED, &w.done()?)
}

/// `result` as `shards api-networks remove NAME` answers it: one frame.
pub fn removed(result: &Result<(), (RemoveCause, String)>) -> Result<Vec<u8>, WireError> {
    let mut w = Writer(Vec::new());
    w.u8(VERSION);
    match result {
        Ok(()) => w.u8(0),
        Err((cause, why)) => {
            w.u8(*cause as u8);
            w.text(why)?;
        }
    }
    frame(REMOVED, &w.done()?)
}

fn version(r: &mut Reader<'_>) -> Result<(), NetworkRefusal> {
    match r.u8().map_err(NetworkRefusal::Wire)? {
        VERSION => Ok(()),
        v => Err(NetworkRefusal::Version(v)),
    }
}

/// What [`created`] wrote, its frame's payload.
fn read_created(payload: &[u8]) -> Result<String, NetworkRefusal> {
    let wire = NetworkRefusal::Wire;
    let mut r = Reader(payload);
    version(&mut r)?;
    let cause = match r.u8().map_err(wire)? {
        0 => {
            let id = r.text().map_err(wire)?;
            r.end().map_err(wire)?;
            return Ok(id);
        }
        1 => CreateCause::Name,
        2 => CreateCause::Exists,
        3 => CreateCause::Pool,
        4 => CreateCause::Host,
        tag => {
            return Err(wire(WireError::Tag {
                what: "why a network was not made",
                tag,
            }));
        }
    };
    let why = r.text().map_err(wire)?;
    r.end().map_err(wire)?;
    Err(NetworkRefusal::Create(cause, why))
}

/// What [`removed`] wrote, its frame's payload.
fn read_removed(payload: &[u8]) -> Result<(), NetworkRefusal> {
    let wire = NetworkRefusal::Wire;
    let mut r = Reader(payload);
    version(&mut r)?;
    let cause = match r.u8().map_err(wire)? {
        0 => {
            r.end().map_err(wire)?;
            return Ok(());
        }
        1 => RemoveCause::Unknown,
        2 => RemoveCause::Shards,
        3 => RemoveCause::InUse,
        4 => RemoveCause::Host,
        tag => {
            return Err(wire(WireError::Tag {
                what: "why a network was not removed",
                tag,
            }));
        }
    };
    let why = r.text().map_err(wire)?;
    r.end().map_err(wire)?;
    Err(NetworkRefusal::Remove(cause, why))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(frame: &[u8], kind: u8) -> Vec<u8> {
        let (k, len) =
            shards_abi::run::parse_header(frame[..shards_abi::run::HEADER].try_into().unwrap()).unwrap();
        assert_eq!(k, kind);
        assert_eq!(len as usize, frame.len() - shards_abi::run::HEADER);
        frame[shards_abi::run::HEADER..].to_vec()
    }

    /// A making's answer reads back as made, its ID, or refused by each cause in the home's
    /// words; a broken one is refused typed: cut anywhere, a byte past its end, another
    /// version, a cause no version has.
    #[test]
    fn a_makings_answer_reads_back_typed() {
        let id = "0f".repeat(32);
        let p = payload(&created(&Ok(id.clone())).unwrap(), CREATED);
        assert_eq!(read_created(&p), Ok(id));
        for cause in [
            CreateCause::Name,
            CreateCause::Exists,
            CreateCause::Pool,
            CreateCause::Host,
        ] {
            let why = format!("{cause:?}'s words");
            let p = payload(&created(&Err((cause, why.clone()))).unwrap(), CREATED);
            assert_eq!(read_created(&p), Err(NetworkRefusal::Create(cause, why)));
        }
        for cut in 0..p.len() {
            assert!(read_created(&p[..cut]).is_err(), "cut at {cut}");
        }
        let mut trailing = p.clone();
        trailing.push(0);
        assert_eq!(
            read_created(&trailing),
            Err(NetworkRefusal::Wire(WireError::Trailing))
        );
        assert_eq!(
            read_created(&[VERSION + 1, 0]),
            Err(NetworkRefusal::Version(VERSION + 1))
        );
        assert!(matches!(
            read_created(&[VERSION, 5]),
            Err(NetworkRefusal::Wire(WireError::Tag { tag: 5, .. }))
        ));
    }

    /// A removal's answer reads back as removed, or refused by each cause in the home's words.
    #[test]
    fn a_removals_answer_reads_back_typed() {
        let p = payload(&removed(&Ok(())).unwrap(), REMOVED);
        assert_eq!(read_removed(&p), Ok(()));
        for cause in [
            RemoveCause::Unknown,
            RemoveCause::Shards,
            RemoveCause::InUse,
            RemoveCause::Host,
        ] {
            let why = format!("{cause:?}'s words");
            let p = payload(&removed(&Err((cause, why.clone()))).unwrap(), REMOVED);
            assert_eq!(read_removed(&p), Err(NetworkRefusal::Remove(cause, why)));
        }
        assert!(matches!(
            read_removed(&[VERSION, 5]),
            Err(NetworkRefusal::Wire(WireError::Tag { tag: 5, .. }))
        ));
        assert_eq!(
            read_removed(&[VERSION, 0, 0]),
            Err(NetworkRefusal::Wire(WireError::Trailing))
        );
        assert_eq!(
            read_removed(&[VERSION + 1, 0]),
            Err(NetworkRefusal::Version(VERSION + 1))
        );
    }

    /// `script` as an executable stand-in for `shards`, in `dir`, written by a process of its
    /// own (`shards_testdir::program`): no exec of it fails ETXTBSY.
    #[cfg(unix)]
    fn stand_in(dir: &Path, script: &str) -> std::path::PathBuf {
        let path = dir.join("shards");
        shards_testdir::program(&path, &format!("#!/bin/sh\n{script}")).unwrap();
        path
    }

    /// The bytes of `frame` as printf(1) writes them back.
    #[cfg(unix)]
    fn printf(frame: &[u8]) -> String {
        let octal: String = frame.iter().map(|b| format!("\\{b:03o}")).collect();
        format!("printf '{octal}'")
    }

    /// Each call asks `shards api-networks` by its words, in the home its environment names,
    /// and reads its one frame; one that ends without it is refused with its status, and one
    /// that answers another kind is refused.
    #[cfg(unix)]
    #[test]
    fn each_call_asks_shards_api_networks_in_its_home() {
        let dir = shards_testdir::TempDir::new("api-networks").unwrap();
        let home = [(OsString::from("SHARDS_HOME"), OsString::from("/h/ome"))];
        let id = "1a".repeat(32);
        let script = format!(
            "[ \"$1\" = api-networks ] && [ \"$SHARDS_HOME\" = /h/ome ] || exit 9\n\
             case \"$2 $3\" in\n\
             'create team') {} ;;\n\
             'remove team') {} ;;\n\
             'remove held') {} ;;\n\
             esac\n",
            printf(&created(&Ok(id.clone())).unwrap()),
            printf(&removed(&Ok(())).unwrap()),
            printf(&removed(&Err((RemoveCause::InUse, "runs are on it".into()))).unwrap()),
        );
        let binary = stand_in(dir.path(), &script);
        let networks = Networks {
            binary: Some(&binary),
            env: &home,
        };
        assert_eq!(networks.create("team"), Ok(id));
        assert_eq!(networks.remove("team"), Ok(()));
        assert_eq!(
            networks.remove("held"),
            Err(NetworkRefusal::Remove(
                RemoveCause::InUse,
                "runs are on it".into()
            ))
        );
        assert!(matches!(
            networks.create("other"),
            Err(NetworkRefusal::Ended { status: Some(0), .. })
        ));
        let elsewhere = [(OsString::from("SHARDS_HOME"), OsString::from("/other"))];
        let apart = Networks {
            binary: Some(&binary),
            env: &elsewhere,
        };
        assert!(matches!(
            apart.create("team"),
            Err(NetworkRefusal::Ended { status: Some(9), .. })
        ));
        let other_dir = shards_testdir::TempDir::new("api-networks").unwrap();
        let other = stand_in(
            other_dir.path(),
            &format!("{}\n", printf(&removed(&Ok(())).unwrap())),
        );
        let answering_another_kind = Networks {
            binary: Some(&other),
            env: &home,
        };
        assert!(matches!(
            answering_another_kind.create("team"),
            Err(NetworkRefusal::Wire(WireError::Tag { tag: REMOVED, .. }))
        ));
    }

    #[test]
    fn a_missing_shards_is_refused_as_not_started() {
        let dir = shards_testdir::TempDir::new("api-networks").unwrap();
        let missing = dir.path().join("no-shards-here");
        let networks = Networks {
            binary: Some(&missing),
            env: &[],
        };
        assert!(matches!(networks.create("team"), Err(NetworkRefusal::Start(_))));
    }
}
