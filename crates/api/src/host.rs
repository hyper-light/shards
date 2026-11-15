//! Whether this host can run microVMs now, without booting one (docs/design/architecture.md
//! D262): [`host_check`] asks `shards api-check`, which runs the VMM's own check, the one
//! every VM process makes before it boots (`shards_vmm::vm::check_host`), and answers in one
//! frame ([`CHECKED`]): this build's backend and its verdict, typed.
//!
//! It runs in the `shards` process, not in the VM process the hypervisor entitlement is on:
//! no backend's check makes a hypervisor call (KVM opens `/dev/kvm` and asks its capabilities
//! as this user, as the VM process does; Hypervisor.framework's is `sysctl kern.hv_support`
//! and `kern.hv_vmm_present`, which any process reads; WHP's loads its library and asks
//! `WHvGetCapability`, making no partition), so it needs neither the entitlement nor App
//! Sandbox's grants, and costs one launch of `shards` (PM M113), where the VM process would
//! cost its writing out (D237) and a sandboxed start besides.

use std::path::Path;

pub use shards_abi::host::{Backend, Unusable};

use crate::command::{Command, NotStarted};
use crate::image::wire::{Reader, WireError, Writer, frame};

/// The frame kind of `shards api-check`'s answer: its one frame.
pub const CHECKED: u8 = 1;

/// The answer's version, its first byte: changed only by appending.
pub const VERSION: u8 = 1;

/// Whether this host runs microVMs now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCheck {
    /// It does, with this backend.
    Usable(Backend),
    /// It does not: this build's backend, where it has one for this host, and why.
    Unusable { backend: Option<Backend>, why: Unusable },
}

impl HostCheck {
    /// The check `backend` (a build's, `shards_vmm::hv::backend`) made: `checked`. A build with
    /// no backend runs none, whatever its check said.
    pub fn of(backend: Option<Backend>, checked: Result<(), Unusable>) -> HostCheck {
        match (backend, checked) {
            (Some(backend), Ok(())) => HostCheck::Usable(backend),
            (backend, Err(why)) => HostCheck::Unusable { backend, why },
            (None, Ok(())) => HostCheck::Unusable {
                backend: None,
                why: unsupported(std::env::consts::OS, std::env::consts::ARCH),
            },
        }
    }

    /// Whether a microVM runs here now.
    pub fn usable(&self) -> bool {
        matches!(self, HostCheck::Usable(_))
    }
}

/// Why the check could not be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckRefusal {
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

impl std::fmt::Display for CheckRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start(why) => write!(f, "starting shards api-check: {why}"),
            Self::Os { what, kind } => write!(f, "{what}: {kind}"),
            Self::Ended { status, said } => {
                f.write_str("shards api-check ended without its answer")?;
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
                "shards api-check answered version {v}; this side reads {VERSION}"
            ),
            Self::Wire(e) => write!(f, "shards api-check's answer: {e}"),
        }
    }
}

impl std::error::Error for CheckRefusal {}

impl From<NotStarted> for CheckRefusal {
    fn from(e: NotStarted) -> CheckRefusal {
        match e {
            NotStarted::Start(why) => CheckRefusal::Start(why),
            NotStarted::Os { what, kind } => CheckRefusal::Os { what, kind },
        }
    }
}

/// Whether this host can run microVMs now, without booting one, as `binary` (else
/// `SHARDS_BIN`, else `shards` on `PATH`) says: its backend and why it cannot, typed. A host
/// shards has no backend for (an Intel Mac) is answered here, as no `shards` runs there.
pub fn host_check(binary: Option<&Path>) -> Result<HostCheck, CheckRefusal> {
    if let Err(crate::Refusal::NoBackend { os, arch }) = crate::backend() {
        return Ok(HostCheck::Unusable {
            backend: None,
            why: unsupported(os, arch),
        });
    }
    let mut command = Command::start(binary, &["api-check".as_ref()])?;
    let answer = command.frame();
    let ended = command.end();
    match answer {
        Ok(Some((CHECKED, payload))) => read(&payload),
        Ok(Some((kind, _))) => Err(CheckRefusal::Wire(WireError::Tag {
            what: "a host check's frame",
            tag: kind,
        })),
        Ok(None) | Err(_) => Err(CheckRefusal::Ended {
            status: ended.status,
            said: ended.said,
        }),
    }
}

fn unsupported(os: &str, arch: &str) -> Unusable {
    Unusable::Unsupported {
        os: os.into(),
        arch: arch.into(),
    }
}

/// `check` as `shards api-check` answers it: one frame.
pub fn answer(check: &HostCheck) -> Result<Vec<u8>, WireError> {
    let mut w = Writer(Vec::new());
    w.u8(VERSION);
    let (backend, why) = match check {
        HostCheck::Usable(backend) => (Some(*backend), None),
        HostCheck::Unusable { backend, why } => (*backend, Some(why)),
    };
    w.u8(match backend {
        None => 0,
        Some(Backend::Kvm) => 1,
        Some(Backend::HypervisorFramework) => 2,
        Some(Backend::WindowsHypervisorPlatform) => 3,
    });
    match why {
        None => w.u8(0),
        Some(Unusable::Unsupported { os, arch }) => {
            w.u8(1);
            w.text(os)?;
            w.text(arch)?;
        }
        Some(Unusable::KvmMissing) => w.u8(2),
        Some(Unusable::KvmDenied) => w.u8(3),
        Some(Unusable::HvSupportOff) => w.u8(4),
        Some(Unusable::WhpOff) => w.u8(5),
        Some(Unusable::NoHypervisor) => w.u8(6),
        Some(Unusable::NoNested) => w.u8(7),
        Some(Unusable::Lacks(what)) => {
            w.u8(8);
            w.text(what)?;
        }
        Some(Unusable::Unasked(what)) => {
            w.u8(9);
            w.text(what)?;
        }
    }
    frame(CHECKED, &w.done()?)
}

/// What [`answer`] wrote, its frame's payload.
fn read(payload: &[u8]) -> Result<HostCheck, CheckRefusal> {
    let mut r = Reader(payload);
    let wire = CheckRefusal::Wire;
    match r.u8().map_err(wire)? {
        VERSION => {}
        v => return Err(CheckRefusal::Version(v)),
    }
    let backend = match r.u8().map_err(wire)? {
        0 => None,
        1 => Some(Backend::Kvm),
        2 => Some(Backend::HypervisorFramework),
        3 => Some(Backend::WindowsHypervisorPlatform),
        tag => {
            return Err(wire(WireError::Tag {
                what: "a backend",
                tag,
            }));
        }
    };
    let why = match r.u8().map_err(wire)? {
        0 => None,
        1 => Some(Unusable::Unsupported {
            os: r.text().map_err(wire)?,
            arch: r.text().map_err(wire)?,
        }),
        2 => Some(Unusable::KvmMissing),
        3 => Some(Unusable::KvmDenied),
        4 => Some(Unusable::HvSupportOff),
        5 => Some(Unusable::WhpOff),
        6 => Some(Unusable::NoHypervisor),
        7 => Some(Unusable::NoNested),
        8 => Some(Unusable::Lacks(r.text().map_err(wire)?)),
        9 => Some(Unusable::Unasked(r.text().map_err(wire)?)),
        tag => {
            return Err(wire(WireError::Tag {
                what: "why a host runs no microVM",
                tag,
            }));
        }
    };
    r.end().map_err(wire)?;
    match (backend, why) {
        (Some(backend), None) => Ok(HostCheck::Usable(backend)),
        (backend, Some(why)) => Ok(HostCheck::Unusable { backend, why }),
        (None, None) => Err(wire(WireError::Tag {
            what: "a usable host's backend",
            tag: 0,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every() -> Vec<HostCheck> {
        let mut all: Vec<HostCheck> = [
            Backend::Kvm,
            Backend::HypervisorFramework,
            Backend::WindowsHypervisorPlatform,
        ]
        .into_iter()
        .map(HostCheck::Usable)
        .collect();
        for why in [
            Unusable::Unsupported {
                os: "macos".into(),
                arch: "x86_64".into(),
            },
            Unusable::KvmMissing,
            Unusable::KvmDenied,
            Unusable::HvSupportOff,
            Unusable::WhpOff,
            Unusable::NoHypervisor,
            Unusable::NoNested,
            Unusable::Lacks("this host's KVM lacks KVM_CAP_IRQFD".into()),
            Unusable::Unasked("KVM_GET_API_VERSION: EIO".into()),
        ] {
            for backend in [None, Some(Backend::Kvm)] {
                all.push(HostCheck::Unusable {
                    backend,
                    why: why.clone(),
                });
            }
        }
        all
    }

    fn payload(check: &HostCheck) -> Vec<u8> {
        let frame = answer(check).unwrap();
        let (kind, len) =
            shards_abi::run::parse_header(frame[..shards_abi::run::HEADER].try_into().unwrap()).unwrap();
        assert_eq!(kind, CHECKED);
        assert_eq!(len as usize, frame.len() - shards_abi::run::HEADER);
        frame[shards_abi::run::HEADER..].to_vec()
    }

    #[test]
    fn every_check_reads_back_as_it_was_answered() {
        for check in every() {
            assert_eq!(read(&payload(&check)).unwrap(), check);
        }
    }

    #[test]
    fn an_answer_this_side_does_not_read_is_refused_typed() {
        let usable = payload(&HostCheck::Usable(Backend::Kvm));
        let mut later = usable.clone();
        later[0] = VERSION + 1;
        assert_eq!(read(&later), Err(CheckRefusal::Version(VERSION + 1)));
        let mut trailing = usable.clone();
        trailing.push(0);
        assert_eq!(read(&trailing), Err(CheckRefusal::Wire(WireError::Trailing)));
        assert_eq!(read(&usable[..2]), Err(CheckRefusal::Wire(WireError::Short)));
        let mut backend = usable.clone();
        backend[1] = 4;
        assert!(matches!(
            read(&backend),
            Err(CheckRefusal::Wire(WireError::Tag { tag: 4, .. }))
        ));
        let mut why = usable.clone();
        why[2] = 10;
        assert!(matches!(
            read(&why),
            Err(CheckRefusal::Wire(WireError::Tag { tag: 10, .. }))
        ));
        // Usable with no backend is no answer.
        assert!(matches!(read(&[VERSION, 0, 0]), Err(CheckRefusal::Wire(_))));
    }

    /// `script` as an executable stand-in for `shards`, in `dir`.
    #[cfg(unix)]
    fn stand_in(dir: &Path, script: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("shards");
        std::fs::write(&path, format!("#!/bin/sh\n{script}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// The bytes of `frame` as printf(1) writes them back.
    #[cfg(unix)]
    fn printf(frame: &[u8]) -> String {
        let octal: String = frame.iter().map(|b| format!("\\{b:03o}")).collect();
        format!("printf '{octal}'")
    }

    #[cfg(unix)]
    #[test]
    fn a_hosts_answer_is_read_from_its_one_frame() {
        let dir = shards_testdir::TempDir::new("host-check").unwrap();
        let check = HostCheck::Unusable {
            backend: Some(Backend::Kvm),
            why: Unusable::KvmDenied,
        };
        // Asked by name: the command shards-api starts is `api-check`.
        let script = format!(
            "[ \"$1\" = api-check ] || exit 9\n{}\n",
            printf(&answer(&check).unwrap())
        );
        let binary = stand_in(dir.path(), &script);
        assert_eq!(host_check(Some(&binary)), Ok(check));
    }

    #[cfg(unix)]
    #[test]
    fn a_host_that_ends_without_its_answer_is_refused_with_its_words() {
        let dir = shards_testdir::TempDir::new("host-check").unwrap();
        let binary = stand_in(
            dir.path(),
            "echo 'shards: no such command: api-check' >&2\nexit 3\n",
        );
        assert_eq!(
            host_check(Some(&binary)),
            Err(CheckRefusal::Ended {
                status: Some(3),
                said: "shards: no such command: api-check\n".into()
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_host_that_answers_another_frame_is_refused() {
        let dir = shards_testdir::TempDir::new("host-check").unwrap();
        let other = frame(CHECKED + 1, &[VERSION]).unwrap();
        let binary = stand_in(dir.path(), &format!("{}\n", printf(&other)));
        assert!(matches!(
            host_check(Some(&binary)),
            Err(CheckRefusal::Wire(WireError::Tag { tag, .. })) if tag == CHECKED + 1
        ));
    }

    #[test]
    fn a_missing_shards_is_refused_as_not_started() {
        let dir = shards_testdir::TempDir::new("host-check").unwrap();
        let missing = dir.path().join("no-shards-here");
        assert!(matches!(host_check(Some(&missing)), Err(CheckRefusal::Start(_))));
    }

    #[test]
    fn a_build_without_a_backend_runs_nothing_whatever_its_check_said() {
        assert_eq!(
            HostCheck::of(Some(Backend::Kvm), Ok(())),
            HostCheck::Usable(Backend::Kvm)
        );
        assert!(matches!(
            HostCheck::of(None, Ok(())),
            HostCheck::Unusable {
                backend: None,
                why: Unusable::Unsupported { .. }
            }
        ));
        assert_eq!(
            HostCheck::of(Some(Backend::HypervisorFramework), Err(Unusable::NoNested)),
            HostCheck::Unusable {
                backend: Some(Backend::HypervisorFramework),
                why: Unusable::NoNested
            }
        );
    }
}
