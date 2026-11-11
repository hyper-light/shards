//! One prepared held-object exec, consumed by the existing native job's child.
//! This module creates no job, permission, resource scope, child or lifetime owner.

use std::convert::Infallible;
use std::num::NonZeroI32;
use std::os::fd::{AsRawFd as _, BorrowedFd, RawFd};

use shards_ipc::child_fds::{Binding, Inheritance, PreparedFdMap};

use super::file;
use super::literals::Literals;
use super::objects::{Executable, Workdir};

// Existing native exec map (child_fds/exec_tests): stdio0..2, status3,
// executable4, working directory5. These are exact protocol slots, not quotas.
pub(super) const STATUS_FD: RawFd = 3;
const EXECUTABLE_FD: RawFd = 4;
const WORKDIR_FD: RawFd = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecutableLifetime {
    ImageOnly,
    /// Linux shebang exec needs the held script descriptor after exec. The
    /// caller must separately authorize this read-only object in the child.
    ReadOnlyScript,
}

pub(crate) struct Request {
    pub executable: Executable,
    pub workdir: Workdir,
    pub literals: Literals,
    pub lifetime: ExecutableLifetime,
}

/// Already-owned separate caller endpoints. Preparation duplicates only these
/// objects; it never inherits the helper's control/owner/audit descriptors.
pub(crate) struct Streams<'a> {
    pub input: BorrowedFd<'a>,
    pub output: BorrowedFd<'a>,
    pub error: BorrowedFd<'a>,
    pub launch_status: BorrowedFd<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    ExecutableAccess,
    WorkingDirectory,
    Exec,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Object {
    Executable,
    Workdir,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Fds(shards_ipc::child_fds::Failure),
    Metadata {
        object: Object,
        cause: file::ReadFailure,
    },
    ScriptRequiresReadOnly {
        actual_flags: i32,
    },
    Native {
        operation: Operation,
        errno: NonZeroI32,
    },
    InvalidErrno {
        operation: Operation,
        actual: i32,
    },
}

/// A location to report a child-only failure, never evidence of exec or reap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusLocation {
    Staged,
    Installed,
    Unknown,
}

impl Failure {
    pub fn status_location(self) -> StatusLocation {
        use shards_ipc::child_fds::{Failure as FdFailure, Operation as FdOperation};
        match self {
            Self::Fds(FdFailure::Native {
                operation:
                    FdOperation::Install {
                        destination: 0..=STATUS_FD,
                    },
                ..
            }) => StatusLocation::Staged,
            Self::Fds(FdFailure::Native {
                operation:
                    FdOperation::Install {
                        destination: EXECUTABLE_FD | WORKDIR_FD,
                    }
                    | FdOperation::Close { .. },
                ..
            })
            | Self::Native {
                operation: Operation::WorkingDirectory | Operation::Exec,
                ..
            }
            | Self::InvalidErrno {
                operation: Operation::WorkingDirectory | Operation::Exec,
                ..
            } => StatusLocation::Installed,
            _ => StatusLocation::Unknown,
        }
    }
}

pub(crate) struct Rejected {
    pub request: Request,
    pub failure: Failure,
}

#[derive(Debug)]
pub(crate) enum Preparation {
    Prepared(Prepared),
    Rejected(Rejected),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Retained {
    pub literal_bytes: usize,
    pub staged_descriptors: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ObjectIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Metadata {
    pub executable: ObjectIdentity,
    pub workdir: ObjectIdentity,
    pub arguments: usize,
    pub environment: usize,
    pub retained: Retained,
}

/// Inert syscall plan, not an authorization token. The caller reserves this
/// preparation's overlap and obtains actual owner/audit/current/job admission
/// before calling it in the SAME existing Worker birth/ACK path.
pub(crate) struct Prepared {
    request: Request,
    map: PreparedFdMap,
    executable: ObjectIdentity,
    workdir: ObjectIdentity,
}

impl Prepared {
    pub fn prepare(request: Request, streams: Streams<'_>) -> Preparation {
        let result = (|| {
            let executable = identity(request.executable.fd(), Object::Executable)?;
            let workdir = identity(request.workdir.fd(), Object::Workdir)?;
            let inheritance = match request.lifetime {
                ExecutableLifetime::ImageOnly => Inheritance::SetupOnly,
                ExecutableLifetime::ReadOnlyScript => {
                    // SAFETY: inspect this held executable without changing it.
                    let flags = unsafe { libc::fcntl(request.executable.fd().as_raw_fd(), libc::F_GETFL) };
                    if flags < 0 {
                        return Err(native(Operation::ExecutableAccess));
                    }
                    if flags & libc::O_ACCMODE != libc::O_RDONLY || flags & libc::O_PATH != 0 {
                        return Err(Failure::ScriptRequiresReadOnly { actual_flags: flags });
                    }
                    Inheritance::RetainAcrossExec
                }
            };
            let map = PreparedFdMap::prepare(&[
                Binding {
                    source: streams.input,
                    destination: 0,
                    inheritance: Inheritance::RetainAcrossExec,
                },
                Binding {
                    source: streams.output,
                    destination: 1,
                    inheritance: Inheritance::RetainAcrossExec,
                },
                Binding {
                    source: streams.error,
                    destination: 2,
                    inheritance: Inheritance::RetainAcrossExec,
                },
                Binding {
                    source: streams.launch_status,
                    destination: STATUS_FD,
                    inheritance: Inheritance::SetupOnly,
                },
                Binding {
                    source: request.executable.fd(),
                    destination: EXECUTABLE_FD,
                    inheritance,
                },
                Binding {
                    source: request.workdir.fd(),
                    destination: WORKDIR_FD,
                    inheritance: Inheritance::SetupOnly,
                },
            ])
            .map_err(Failure::Fds)?;
            Ok((map, executable, workdir))
        })();
        match result {
            Ok((map, executable, workdir)) => Preparation::Prepared(Self {
                request,
                map,
                executable,
                workdir,
            }),
            Err(failure) => Preparation::Rejected(Rejected { request, failure }),
        }
    }

    /// Facts sampled from the actual held objects during this preparation.
    /// They neither describe mutable file content nor authorize an effect.
    pub fn metadata(&self) -> Metadata {
        let (arguments, environment) = self.request.literals.counts();
        Metadata {
            executable: self.executable,
            workdir: self.workdir,
            arguments,
            environment,
            retained: self.retained(),
        }
    }

    /// Capture the raw value before birth/install. On StatusLocation::Staged
    /// only, that child copy remains open because the install loop has not begun
    /// closing staging. Never borrow it after installation/closure has begun.
    pub fn launch_status_before_install(&self) -> Option<BorrowedFd<'_>> {
        self.map.staged_source(STATUS_FD)
    }

    /// Actual retained literal capacities and staged FDs only. Original objects,
    /// preparation transients, allocator/kernel overhead remain separately owned
    /// and charged by the caller; this observation cannot mint entitlement.
    pub fn retained(&self) -> Retained {
        Retained {
            literal_bytes: self.request.literals.bytes(),
            staged_descriptors: self.map.descriptors(),
        }
    }

    /// # Safety
    /// Only the existing trusted single-threaded post-clone job child, AFTER its
    /// real owner-issued scope/admission/association/commit, may call this. The
    /// job must already have inherited its actual sandbox/resource restrictions.
    /// This closes unlisted FDs. On Err the caller MUST report its typed launch
    /// failure through the existing channel and raw _exit, without allocation,
    /// unwinding, Rust destructors or a return into the parent's runtime. Status
    /// EOF alone is not exec success, reaping, population emptiness or a refund.
    pub unsafe fn exec_in_child(&self) -> Result<Infallible, Failure> {
        // SAFETY: caller supplies the exclusive child FD table and never drops
        // inherited owners afterward. The single canonical map installs all slots.
        unsafe { self.map.install_in_child() }.map_err(Failure::Fds)?;
        // SAFETY: exact mapped setup-only descriptor is the checked held directory.
        if unsafe { libc::fchdir(WORKDIR_FD) } != 0 {
            return Err(native(Operation::WorkingDirectory));
        }
        // SAFETY: held executable at4, empty pathname and complete NUL-terminated
        // argv/env pointer regions were prepared before birth and remain live.
        unsafe {
            libc::syscall(
                libc::SYS_execveat,
                EXECUTABLE_FD,
                c"".as_ptr(),
                self.request.literals.argv(),
                self.request.literals.env(),
                libc::AT_EMPTY_PATH,
            )
        };
        Err(native(Operation::Exec))
    }
}

fn identity(fd: BorrowedFd<'_>, object: Object) -> Result<ObjectIdentity, Failure> {
    let metadata = file::stat(fd).map_err(|cause| Failure::Metadata { object, cause })?;
    Ok(ObjectIdentity {
        device: metadata.st_dev,
        inode: metadata.st_ino,
    })
}

fn native(operation: Operation) -> Failure {
    // SAFETY: copy this Linux thread's errno immediately after the failed syscall.
    let actual = unsafe { *libc::__errno_location() };
    match NonZeroI32::new(actual).filter(|value| value.get() > 0) {
        Some(errno) => Failure::Native { operation, errno },
        None => Failure::InvalidErrno { operation, actual },
    }
}

impl std::fmt::Debug for Rejected {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Rejected")
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Prepared {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Prepared")
            .field("retained", &self.retained())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
