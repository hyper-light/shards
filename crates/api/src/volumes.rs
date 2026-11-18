//! A home's volumes, as an orchestrator keeps its agents' across their sessions on one machine
//! (docs/design/architecture.md D281): [`volumes`] reads each volume of the home's store with
//! the bytes its files take, which its orchestrator admits to its node's room and holds to its
//! own bound, and [`clear_volume`] removes one with its files, so that the next plug of its name
//! ([`crate::image::PlugSource::Volume`]) makes it empty, once no run holds it. Each asks
//! `shards api-volumes`, which reads the store as the daemon's `volume ls`, `system df` and
//! `volume rm` do, and answers in one frame.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use crate::command::{Command, NotStarted};
use crate::image::wire::{Reader, WireError, Writer, frame};

/// The frame kind of `shards api-volumes list`'s answer.
pub const LISTED: u8 = 1;

/// The frame kind of `shards api-volumes clear NAME`'s answer.
pub const CLEARED: u8 = 2;

/// The answers' version, each one's first byte: changed only by appending. 2: a clearing
/// refused [`ClearCause::InUse`] (D281); 3: each volume's bound and what its files take by
/// its measure ([`HomeVolume::bound`], [`HomeVolume::charge`], D281).
pub const VERSION: u8 = 3;

/// A volume of a home's store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeVolume {
    pub name: String,
    /// The bytes its files take, as `system df` counts a volume's (moby's `directory.Size`: each
    /// file's length, a hard link's once); none where its files could not all be read.
    pub bytes: Option<u64>,
    /// What its files may take (D281): its bound, held as they are written, a write past it
    /// refused ENOSPC; none where it has none, its host's room alone bounding it.
    pub bound: Option<u64>,
    /// What its files take by its bound's measure (D281), what an admission holds to its
    /// bound: each name a unit of its host file system's, each file's length and its
    /// attributes in whole units. The count its servers share while a run holds it bounded,
    /// else walked as it is asked; none where its files could not all be read.
    pub charge: Option<u64>,
    /// When it was made, in nanoseconds since the Unix epoch.
    pub created_ns: u64,
    /// Made for one run by no one's name (an image's `VOLUME` no share stands in for), and gone
    /// with that run.
    pub anonymous: bool,
}

/// Why the store cleared no volume, as `shards api-volumes` answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearCause {
    /// A name the store takes no volume by.
    Name = 1,
    /// No volume of that name is there.
    Unknown = 2,
    /// Its files were not all removed.
    Store = 3,
    /// A run holds it (D281): each run, a daemon's or an orchestrator's, holds every named
    /// volume it mounts or plugs for its life. Its files are kept.
    InUse = 4,
}

/// Why a home's volumes were not read, or one not cleared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeRefusal {
    /// The store cleared none, by why, in its words.
    Clear(ClearCause, String),
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

impl std::fmt::Display for VolumeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clear(cause, why) => write!(f, "the volume's clearing: {cause:?}: {why}"),
            Self::Start(why) => write!(f, "starting shards api-volumes: {why}"),
            Self::Os { what, kind } => write!(f, "{what}: {kind}"),
            Self::Ended { status, said } => {
                f.write_str("shards api-volumes ended without its answer")?;
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
                "shards api-volumes answered version {v}; this side reads {VERSION}"
            ),
            Self::Wire(e) => write!(f, "shards api-volumes' answer: {e}"),
        }
    }
}

impl std::error::Error for VolumeRefusal {}

impl From<NotStarted> for VolumeRefusal {
    fn from(e: NotStarted) -> VolumeRefusal {
        match e {
            NotStarted::Start(why) => VolumeRefusal::Start(why),
            NotStarted::Os { what, kind } => VolumeRefusal::Os { what, kind },
        }
    }
}

/// Every volume of the home `env` names (`SHARDS_HOME`, else its default), by name, as `binary`
/// (else `SHARDS_BIN`, else `shards` on `PATH`) reads its store: each walked for its bytes as
/// it is asked, and for its charge where no run holds it bounded, so a call costs two walks of
/// every volume's files.
pub fn volumes(
    binary: Option<&Path>,
    env: &[(OsString, OsString)],
) -> Result<Vec<HomeVolume>, VolumeRefusal> {
    let payload = ask(binary, env, &[OsStr::new("list")], LISTED)?;
    read_listed(&payload)
}

/// Removes volume `name` of the home `env` names, and its files, as `volume rm` does: gone at
/// once, deleted after. Refused [`ClearCause::InUse`] at once where a run that mounts or
/// plugged it holds it (D281), a daemon's or an orchestrator's, never waiting for that run to
/// end.
pub fn clear_volume(
    binary: Option<&Path>,
    env: &[(OsString, OsString)],
    name: &str,
) -> Result<(), VolumeRefusal> {
    let payload = ask(binary, env, &[OsStr::new("clear"), OsStr::new(name)], CLEARED)?;
    read_cleared(&payload)
}

/// `shards api-volumes ARGS…`'s one answer, of frame kind `kind`.
fn ask(
    binary: Option<&Path>,
    env: &[(OsString, OsString)],
    args: &[&OsStr],
    kind: u8,
) -> Result<Vec<u8>, VolumeRefusal> {
    let mut all = vec![OsStr::new("api-volumes")];
    all.extend_from_slice(args);
    let mut command = Command::start(binary, &all, env)?;
    let answer = command.frame();
    let ended = command.end();
    match answer {
        Ok(Some((k, payload))) if k == kind => Ok(payload),
        Ok(Some((k, _))) => Err(VolumeRefusal::Wire(WireError::Tag {
            what: "a volumes answer's frame",
            tag: k,
        })),
        Ok(None) | Err(_) => Err(VolumeRefusal::Ended {
            status: ended.status,
            said: ended.said,
        }),
    }
}

/// `volumes` as `shards api-volumes list` answers them: one frame.
pub fn listed(volumes: &[HomeVolume]) -> Result<Vec<u8>, WireError> {
    let mut w = Writer(Vec::new());
    w.u8(VERSION);
    w.count(volumes.len())?;
    for v in volumes {
        w.text(&v.name)?;
        match v.bytes {
            None => w.u8(0),
            Some(bytes) => {
                w.u8(1);
                w.u64(bytes);
            }
        }
        w.u64(v.created_ns);
        w.u8(u8::from(v.anonymous));
        // Appended in version 3 (D281): its bound, 0 for none, which none is.
        w.u64(v.bound.unwrap_or(0));
        match v.charge {
            None => w.u8(0),
            Some(charge) => {
                w.u8(1);
                w.u64(charge);
            }
        }
    }
    frame(LISTED, &w.done()?)
}

/// `result` as `shards api-volumes clear NAME` answers it: one frame.
pub fn cleared(result: &Result<(), (ClearCause, String)>) -> Result<Vec<u8>, WireError> {
    let mut w = Writer(Vec::new());
    w.u8(VERSION);
    match result {
        Ok(()) => w.u8(0),
        Err((cause, why)) => {
            w.u8(*cause as u8);
            w.text(why)?;
        }
    }
    frame(CLEARED, &w.done()?)
}

fn version(r: &mut Reader<'_>) -> Result<(), VolumeRefusal> {
    match r.u8().map_err(VolumeRefusal::Wire)? {
        VERSION => Ok(()),
        v => Err(VolumeRefusal::Version(v)),
    }
}

/// What [`listed`] wrote, its frame's payload.
fn read_listed(payload: &[u8]) -> Result<Vec<HomeVolume>, VolumeRefusal> {
    let wire = VolumeRefusal::Wire;
    let mut r = Reader(payload);
    version(&mut r)?;
    // Each at least its name's length, its bytes' flag, its time, its flag, its bound and its
    // charge's flag.
    let n = r.count(4 + 1 + 8 + 1 + 8 + 1).map_err(wire)?;
    let mut volumes = Vec::new();
    volumes
        .try_reserve_exact(n)
        .map_err(|_| wire(WireError::TooLong))?;
    for _ in 0..n {
        let name = r.text().map_err(wire)?;
        let bytes = match r.u8().map_err(wire)? {
            0 => None,
            1 => Some(r.u64().map_err(wire)?),
            tag => {
                return Err(wire(WireError::Tag {
                    what: "a volume's bytes",
                    tag,
                }));
            }
        };
        let created_ns = r.u64().map_err(wire)?;
        let anonymous = r.flag().map_err(wire)?;
        let bound = Some(r.u64().map_err(wire)?).filter(|b| *b > 0);
        let charge = match r.u8().map_err(wire)? {
            0 => None,
            1 => Some(r.u64().map_err(wire)?),
            tag => {
                return Err(wire(WireError::Tag {
                    what: "what a volume's files take",
                    tag,
                }));
            }
        };
        volumes.push(HomeVolume {
            name,
            bytes,
            bound,
            charge,
            created_ns,
            anonymous,
        });
    }
    r.end().map_err(wire)?;
    Ok(volumes)
}

/// What [`cleared`] wrote, its frame's payload.
fn read_cleared(payload: &[u8]) -> Result<(), VolumeRefusal> {
    let wire = VolumeRefusal::Wire;
    let mut r = Reader(payload);
    version(&mut r)?;
    let cause = match r.u8().map_err(wire)? {
        0 => {
            r.end().map_err(wire)?;
            return Ok(());
        }
        1 => ClearCause::Name,
        2 => ClearCause::Unknown,
        3 => ClearCause::Store,
        4 => ClearCause::InUse,
        tag => {
            return Err(wire(WireError::Tag {
                what: "why a volume was not cleared",
                tag,
            }));
        }
    };
    let why = r.text().map_err(wire)?;
    r.end().map_err(wire)?;
    Err(VolumeRefusal::Clear(cause, why))
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

    fn some() -> Vec<HomeVolume> {
        vec![
            HomeVolume {
                name: "agent-7.cache".into(),
                bytes: Some(3 << 30),
                bound: Some(4 << 30),
                charge: Some((3 << 30) + (1 << 20)),
                created_ns: 1_794_712_180_000_000_000,
                anonymous: false,
            },
            HomeVolume {
                name: "0f".repeat(32),
                bytes: None,
                bound: None,
                charge: None,
                created_ns: 0,
                anonymous: true,
            },
        ]
    }

    /// A list reads back as it was answered, none among them, and a broken one is refused typed:
    /// cut anywhere, a byte past its end, another version, a bytes flag or anonymity flag that is
    /// neither.
    #[test]
    fn a_homes_volumes_read_back_as_they_were_answered() {
        for volumes in [Vec::new(), some()] {
            let p = payload(&listed(&volumes).unwrap(), LISTED);
            assert_eq!(read_listed(&p), Ok(volumes));
        }
        let p = payload(&listed(&some()).unwrap(), LISTED);
        for cut in 0..p.len() {
            assert!(read_listed(&p[..cut]).is_err(), "cut at {cut}");
        }
        let mut trailing = p.clone();
        trailing.push(0);
        assert_eq!(
            read_listed(&trailing),
            Err(VolumeRefusal::Wire(WireError::Trailing))
        );
        let mut later = p.clone();
        later[0] = VERSION + 1;
        assert_eq!(read_listed(&later), Err(VolumeRefusal::Version(VERSION + 1)));
        // The first volume's bytes flag follows its version, its count and its name.
        let flag = 1 + 4 + 4 + "agent-7.cache".len();
        let mut bytes = p.clone();
        bytes[flag] = 2;
        assert!(matches!(
            read_listed(&bytes),
            Err(VolumeRefusal::Wire(WireError::Tag { tag: 2, .. }))
        ));
        let mut anonymous = p.clone();
        anonymous[flag + 1 + 8 + 8] = 2;
        assert!(matches!(
            read_listed(&anonymous),
            Err(VolumeRefusal::Wire(WireError::Tag { tag: 2, .. }))
        ));
        // Its charge's flag follows its anonymity's and its bound (version 3, D281).
        let mut charge = p.clone();
        charge[flag + 1 + 8 + 8 + 1 + 8] = 2;
        assert!(matches!(
            read_listed(&charge),
            Err(VolumeRefusal::Wire(WireError::Tag { tag: 2, .. }))
        ));
        // A count past what its bytes could hold allocates nothing.
        assert_eq!(
            read_listed(&[VERSION, 0xff, 0xff, 0xff, 0xff]),
            Err(VolumeRefusal::Wire(WireError::Short))
        );
    }

    /// A clearing's answer reads back as done, or refused by its cause in the store's words.
    #[test]
    fn a_clearings_answer_reads_back_typed() {
        let p = payload(&cleared(&Ok(())).unwrap(), CLEARED);
        assert_eq!(read_cleared(&p), Ok(()));
        for cause in [
            ClearCause::Name,
            ClearCause::Unknown,
            ClearCause::Store,
            ClearCause::InUse,
        ] {
            let why = format!("{cause:?}'s words");
            let p = payload(&cleared(&Err((cause, why.clone()))).unwrap(), CLEARED);
            assert_eq!(read_cleared(&p), Err(VolumeRefusal::Clear(cause, why)));
        }
        assert!(matches!(
            read_cleared(&[VERSION, 5]),
            Err(VolumeRefusal::Wire(WireError::Tag { tag: 5, .. }))
        ));
        assert_eq!(
            read_cleared(&[VERSION, 0, 0]),
            Err(VolumeRefusal::Wire(WireError::Trailing))
        );
        assert_eq!(
            read_cleared(&[VERSION + 1, 0]),
            Err(VolumeRefusal::Version(VERSION + 1))
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

    /// Each call asks `shards api-volumes` by its words, in the home its environment names, and
    /// reads its one frame; one that ends without it is refused with its words, and one that
    /// answers another kind is refused.
    #[cfg(unix)]
    #[test]
    fn each_call_asks_shards_api_volumes_in_its_home() {
        let dir = shards_testdir::TempDir::new("api-volumes").unwrap();
        let home = [(OsString::from("SHARDS_HOME"), OsString::from("/h/ome"))];
        let script = format!(
            "[ \"$1\" = api-volumes ] && [ \"$SHARDS_HOME\" = /h/ome ] || exit 9\n\
             case \"$2\" in\n\
             list) {} ;;\n\
             clear) [ \"$3\" = agent-7.cache ] && {} || {} ;;\n\
             esac\n",
            printf(&listed(&some()).unwrap()),
            printf(&cleared(&Ok(())).unwrap()),
            printf(&cleared(&Err((ClearCause::Unknown, "get x: no such volume".into()))).unwrap()),
        );
        let binary = stand_in(dir.path(), &script);
        assert_eq!(volumes(Some(&binary), &home), Ok(some()));
        assert_eq!(clear_volume(Some(&binary), &home, "agent-7.cache"), Ok(()));
        assert_eq!(
            clear_volume(Some(&binary), &home, "x"),
            Err(VolumeRefusal::Clear(
                ClearCause::Unknown,
                "get x: no such volume".into()
            ))
        );
        let elsewhere = [(OsString::from("SHARDS_HOME"), OsString::from("/other"))];
        assert!(matches!(
            volumes(Some(&binary), &elsewhere),
            Err(VolumeRefusal::Ended { status: Some(9), .. })
        ));
        let apart = shards_testdir::TempDir::new("api-volumes").unwrap();
        let other = stand_in(apart.path(), &format!("{}\n", printf(&cleared(&Ok(())).unwrap())));
        assert!(matches!(
            volumes(Some(&other), &home),
            Err(VolumeRefusal::Wire(WireError::Tag { tag: CLEARED, .. }))
        ));
    }

    #[test]
    fn a_missing_shards_is_refused_as_not_started() {
        let dir = shards_testdir::TempDir::new("api-volumes").unwrap();
        let missing = dir.path().join("no-shards-here");
        assert!(matches!(
            volumes(Some(&missing), &[]),
            Err(VolumeRefusal::Start(_))
        ));
    }
}
