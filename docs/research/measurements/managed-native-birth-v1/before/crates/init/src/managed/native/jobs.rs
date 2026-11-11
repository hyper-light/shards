//! The existing Owner's actual issued jobs and retained terminal facts. Kernel
//! population and trusted native waits are independent retirement prerequisites.

use std::fs::File;
use std::num::{NonZeroI32, NonZeroU64};
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd};

use shards_abi::run::native_grants::{self as wire, Object, operation as protocol};

use super::{Close, ProjectionFailure, operations};
use crate::domains::admission::{self, execution};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Rejection {
    Closed(Close),
    Current(protocol::Refusal),
    Source(ProjectionFailure),
    Codec(wire::Error),
    Selection(operations::Failure),
    RootIndex(u32),
    Resources(admission::Refusal),
}

impl Rejection {
    pub(super) fn wire(self) -> protocol::Refusal {
        match self {
            Self::Closed(_) => protocol::Refusal::Closed,
            Self::Current(reason) => reason,
            Self::Selection(cause) => cause.refusal(),
            Self::RootIndex(_) => protocol::Refusal::RootDenied,
            Self::Source(_) | Self::Codec(_) => protocol::Refusal::Identity,
            Self::Resources(_) => protocol::Refusal::ResourcesUnavailable,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    CheckPidfd,
    Kill,
    Population,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    State,
    Identity,
    Bytes,
    Native {
        operation: Operation,
        errno: NonZeroI32,
    },
    InvalidNative {
        operation: Operation,
        actual: Option<i32>,
    },
}

fn native(operation: Operation, error: std::io::Error) -> Failure {
    let actual = error.raw_os_error();
    match actual.and_then(NonZeroI32::new).filter(|n| n.get() > 0) {
        Some(errno) => Failure::Native { operation, errno },
        None => Failure::InvalidNative { operation, actual },
    }
}

/// Exact borrowed selections copied as DATA into the SAME issued operation;
/// current scope/rights are still reselected from real retained roots at Commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Selected {
    effect: protocol::Effect,
    // Process names exactly executable/workdir; Read/Write name one root.
    objects: [Option<Object>; 2],
}

impl Selected {
    pub(super) fn from_selection(selection: operations::Selection<'_>) -> Self {
        let mut objects = [None; 2];
        for (slot, root) in objects.iter_mut().zip(selection.roots()) {
            *slot = Some(root.object);
        }
        Self {
            effect: selection.effect(),
            objects,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Acceptance {
    Prepared,
    Committed,
    CancelledBeforeCommit,
    CancelledAfterCommit,
    Refused(Rejection),
}

impl Acceptance {
    fn committed(self) -> bool {
        matches!(self, Self::Committed | Self::CancelledAfterCommit)
    }

    fn close(self) -> Self {
        match self {
            Self::Prepared => Self::CancelledBeforeCommit,
            Self::Committed => Self::CancelledAfterCommit,
            other => other,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Completion {
    outcome: protocol::Outcome,
    accepted_bytes: u64,
}

#[derive(Debug)]
enum Birth {
    // Published before Prepared is sent: a missing Associate is uncertain birth,
    // never permission to re-arm the preparation rollback or infer no child.
    Unassociated,
    NotBorn,
    Associated {
        namespace_pid: i32,
        pidfd: OwnedFd,
        wait: Wait,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wait {
    Pending,
    Reaped { code: i32, status: i32 },
}

#[derive(Debug)]
enum Scope {
    Published(execution::Published),
    Retained(execution::Retained),
}

#[derive(Debug)]
enum Lifetime {
    File,
    Preparing(execution::Prepared),
    Process {
        scope: Scope,
        controls: crate::managed::Controls,
        birth: Birth,
    },
    PreparationRetained(execution::Retained),
    RemovedBeforeBirth,
    Retired(RetiredBirth),
    // A local move slot only during one exclusive owner turn. No callback,
    // readiness wait, packet or returned state can expose this transition.
    Moving,
}

enum RetiredBirth {
    File,
    NotBorn,
    Reaped {
        namespace_pid: i32,
        code: i32,
        status: i32,
    },
}

impl std::fmt::Debug for RetiredBirth {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File => out.write_str("File"),
            Self::NotBorn => out.write_str("TrustedNotBorn"),
            Self::Reaped {
                namespace_pid,
                code,
                status,
            } => out
                .debug_struct("TrustedReaped")
                .field("namespace_pid", namespace_pid)
                .field("code", code)
                .field("status", status)
                .finish(),
        }
    }
}

#[derive(Debug)]
pub(super) struct Job {
    id: NonZeroU64,
    selected: Selected,
    // Trusted native's observed prepared regions, not charged entitlements.
    facts: protocol::PreparedFacts,
    acceptance: Acceptance,
    completion: Option<Completion>,
    retirement: Retirement,
    lifetime: Lifetime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Retirement {
    Waiting,
    Notify,
    Delivered,
}

#[derive(Debug)]
pub(super) enum Acquisition {
    Prepared(Job),
    Refused(Rejection),
    Retained { job: Job, cause: Rejection },
}

impl Job {
    pub(super) fn acquire(
        id: NonZeroU64,
        selected: Selected,
        facts: protocol::PreparedFacts,
        member: &File,
    ) -> Acquisition {
        let lifetime = if matches!(selected.effect, protocol::Effect::Process { .. }) {
            match execution::Prepared::acquire(member, id) {
                execution::Acquisition::Prepared(prepared) => Lifetime::Preparing(prepared),
                execution::Acquisition::Refused(cause) => {
                    return Acquisition::Refused(Rejection::Resources(cause));
                }
                execution::Acquisition::Retained(retained) => {
                    let Some(cause) = retained.preparation_failure() else {
                        // Even an unexpected missing failure cannot drop the
                        // SAME cleanup obligation or create a successful job.
                        let cause = Rejection::Current(protocol::Refusal::ResourcesUnavailable);
                        return Acquisition::Retained {
                            job: Self {
                                id,
                                selected,
                                facts,
                                acceptance: Acceptance::Refused(cause),
                                completion: None,
                                retirement: Retirement::Waiting,
                                lifetime: Lifetime::PreparationRetained(retained),
                            },
                            cause,
                        };
                    };
                    let cause = Rejection::Resources(cause);
                    return Acquisition::Retained {
                        job: Self {
                            id,
                            selected,
                            facts,
                            acceptance: Acceptance::Refused(cause),
                            completion: None,
                            retirement: Retirement::Waiting,
                            lifetime: Lifetime::PreparationRetained(retained),
                        },
                        cause,
                    };
                }
            }
        } else {
            Lifetime::File
        };
        Acquisition::Prepared(Self {
            id,
            selected,
            facts,
            acceptance: Acceptance::Prepared,
            completion: None,
            retirement: Retirement::Waiting,
            lifetime,
        })
    }

    pub(super) fn id(&self) -> NonZeroU64 {
        self.id
    }
    pub(super) fn effect(&self) -> protocol::Effect {
        self.selected.effect
    }

    pub(super) fn retained(&self) -> protocol::PreparedFacts {
        self.facts
    }

    pub(super) fn commitment(&self) -> bool {
        self.acceptance.committed()
    }

    pub(super) fn terminal_pending(&self) -> bool {
        self.retirement == Retirement::Notify
    }

    pub(super) fn terminal_delivered(&mut self) {
        self.retirement = Retirement::Delivered;
    }

    pub(super) fn cleanup_failure(&self) -> Option<admission::Rollback> {
        match &self.lifetime {
            Lifetime::PreparationRetained(held)
            | Lifetime::Process {
                scope: Scope::Retained(held),
                ..
            } => Some(held.removal_failure()),
            _ => None,
        }
    }

    pub(super) fn birth(&self) -> Option<(i32, BorrowedFd<'_>)> {
        match &self.lifetime {
            Lifetime::Process {
                birth: Birth::Associated {
                    namespace_pid, pidfd, ..
                },
                ..
            } => Some((*namespace_pid, pidfd.as_fd())),
            _ => None,
        }
    }

    pub(super) fn matches_selection(&self, selected: Selected) -> bool {
        self.selected == selected
    }

    pub(super) fn directory(&self) -> Result<BorrowedFd<'_>, Failure> {
        match &self.lifetime {
            Lifetime::Process {
                scope: Scope::Published(group),
                ..
            } => Ok(group.directory().as_fd()),
            _ => Err(Failure::State),
        }
    }

    pub(super) fn publish(&mut self) -> Result<Option<NonZeroU64>, Failure> {
        if matches!(self.lifetime, Lifetime::File) {
            return Ok(None);
        }
        let old = std::mem::replace(&mut self.lifetime, Lifetime::Moving);
        let Lifetime::Preparing(prepared) = old else {
            self.lifetime = old;
            return Err(Failure::State);
        };
        let (group, controls) = prepared.publish();
        self.lifetime = Lifetime::Process {
            scope: Scope::Published(group),
            controls,
            birth: Birth::Unassociated,
        };
        Ok(Some(self.id))
    }

    pub(super) fn associate(&mut self, namespace_pid: i32, pidfd: OwnedFd) -> Result<(), Failure> {
        if namespace_pid <= 0 {
            return Err(Failure::Identity);
        }
        let Lifetime::Process { birth, .. } = &mut self.lifetime else {
            return Err(Failure::State);
        };
        if !matches!(birth, Birth::Unassociated) {
            return Err(Failure::State);
        }
        // This does not wait on a nonchild or infer cgroup membership from a
        // pathname/PID. The private initialized helper is the trusted clone3
        // producer; the zero-signal native operation verifies a real pidfd.
        loop {
            // SAFETY: signal zero checks this transferred object without signaling.
            let checked = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    pidfd.as_raw_fd(),
                    0,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            if checked == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => continue,
                // A real exited task is still retained until its matching wait.
                Some(libc::ESRCH) => break,
                _ => return Err(native(Operation::CheckPidfd, error)),
            }
        }
        *birth = Birth::Associated {
            namespace_pid,
            pidfd,
            wait: Wait::Pending,
        };
        Ok(())
    }

    pub(super) fn can_commit(&self) -> Result<(), Failure> {
        if self.acceptance != Acceptance::Prepared || self.completion.is_some() {
            return Err(Failure::State);
        }
        match &self.lifetime {
            Lifetime::File
            | Lifetime::Process {
                birth: Birth::Associated {
                    wait: Wait::Pending, ..
                },
                ..
            } => Ok(()),
            _ => Err(Failure::State),
        }
    }

    pub(super) fn commit(&mut self) -> Result<(), Failure> {
        self.can_commit()?;
        self.acceptance = Acceptance::Committed;
        Ok(())
    }

    pub(super) fn close(&mut self) {
        self.acceptance = self.acceptance.close();
    }

    pub(super) fn refuse(&mut self, cause: Rejection) -> Result<(), Failure> {
        if self.acceptance.committed() {
            return Err(Failure::State);
        }
        self.acceptance = Acceptance::Refused(cause);
        match &self.lifetime {
            Lifetime::Preparing(_) => {
                let Lifetime::Preparing(prepared) = std::mem::replace(&mut self.lifetime, Lifetime::Moving)
                else {
                    return Err(Failure::State);
                };
                self.lifetime = match prepared.cancel_before_birth() {
                    execution::Cleanup::Removed => Lifetime::RemovedBeforeBirth,
                    execution::Cleanup::Retained(held) => Lifetime::PreparationRetained(held),
                };
                Ok(())
            }
            Lifetime::Process { controls, .. } => crate::managed::kill_population(&controls.kill)
                .map_err(|cause| native(Operation::Kill, cause)),
            Lifetime::File | Lifetime::PreparationRetained(_) | Lifetime::RemovedBeforeBirth => Ok(()),
            _ => Err(Failure::State),
        }
    }

    pub(super) fn cancel(&mut self) -> Result<bool, Failure> {
        let committed = self.acceptance.committed();
        self.close();
        if let Lifetime::Process { controls, .. } = &self.lifetime {
            crate::managed::kill_population(&controls.kill)
                .map_err(|cause| native(Operation::Kill, cause))?;
        }
        Ok(committed)
    }

    pub(super) fn reaped(&mut self, code: i32, status: i32) -> Result<(), Failure> {
        let Lifetime::Process {
            birth: Birth::Associated { wait, .. },
            ..
        } = &mut self.lifetime
        else {
            return Err(Failure::State);
        };
        if *wait != Wait::Pending {
            return Err(Failure::State);
        }
        // The canonical decoder validated CLD/status. This receipt is supplied
        // by the SAME trusted helper that retained and waited its actual child.
        *wait = Wait::Reaped { code, status };
        Ok(())
    }

    pub(super) fn not_born(&mut self) -> Result<(), Failure> {
        if self.acceptance.committed() {
            return Err(Failure::State);
        }
        let Lifetime::Process { birth, .. } = &mut self.lifetime else {
            return Err(Failure::State);
        };
        if !matches!(birth, Birth::Unassociated) {
            return Err(Failure::State);
        }
        // Only the private trusted helper emits this after discarding the SAME
        // Worker/prepared state and every birth capability. Cancel/EOF/absence
        // never constructs this fact; future Commit remains closed permanently.
        *birth = Birth::NotBorn;
        self.close();
        Ok(())
    }

    pub(super) fn complete(
        &mut self,
        outcome: protocol::Outcome,
        accepted_bytes: u64,
    ) -> Result<(), Failure> {
        if self.completion.is_some() {
            return Err(Failure::State);
        }
        let bound = match self.selected.effect {
            protocol::Effect::Process { .. } => 0,
            protocol::Effect::Read { bytes, .. } | protocol::Effect::Write { bytes, .. } => bytes,
        };
        if accepted_bytes > bound
            || (!self.acceptance.committed()
                && (accepted_bytes != 0 || outcome == protocol::Outcome::Success))
        {
            return Err(Failure::Bytes);
        }
        self.completion = Some(Completion {
            outcome,
            accepted_bytes,
        });
        Ok(())
    }

    pub(super) fn events(&self) -> Option<BorrowedFd<'_>> {
        match &self.lifetime {
            Lifetime::Process { controls, .. } => Some(controls.events.as_fd()),
            _ => None,
        }
    }

    pub(super) fn population_changed(&mut self) -> Result<(), Failure> {
        // Read even if the matching wait/terminal has not arrived, clearing the
        // actual kernfs notification without inventing readiness or a wait.
        if let Lifetime::Process { controls, .. } = &self.lifetime {
            let _ = population_empty(&controls.events)?;
        }
        self.retire().map(|_| ())
    }

    pub(super) fn retire(&mut self) -> Result<protocol::Reply, Failure> {
        let not_born = matches!(
            self.lifetime,
            Lifetime::Process {
                birth: Birth::NotBorn,
                ..
            } | Lifetime::Retired(RetiredBirth::NotBorn)
        );
        // NotBorn is itself the trusted negative lifetime terminal. It does not
        // fabricate the separate effect outcome (cancellation vs setup failure).
        if self.completion.is_none() && !not_born {
            return Ok(protocol::Reply::Retained);
        }
        match &self.lifetime {
            Lifetime::File => {
                self.lifetime = Lifetime::Retired(RetiredBirth::File);
                self.retirement = Retirement::Notify;
                return Ok(protocol::Reply::Retired);
            }
            Lifetime::Retired(_) => return Ok(protocol::Reply::Retired),
            Lifetime::Process {
                controls,
                birth:
                    Birth::NotBorn
                    | Birth::Associated {
                        wait: Wait::Reaped { .. },
                        ..
                    },
                ..
            } => {
                if !population_empty(&controls.events)? {
                    return Ok(protocol::Reply::Retained);
                }
            }
            _ => return Ok(protocol::Reply::Retained),
        }
        let old = std::mem::replace(&mut self.lifetime, Lifetime::Moving);
        let Lifetime::Process {
            scope,
            controls,
            birth,
        } = old
        else {
            self.lifetime = old;
            return Err(Failure::State);
        };
        let retired_birth = match &birth {
            Birth::NotBorn => RetiredBirth::NotBorn,
            Birth::Associated {
                namespace_pid,
                wait: Wait::Reaped { code, status },
                ..
            } => RetiredBirth::Reaped {
                namespace_pid: *namespace_pid,
                code: *code,
                status: *status,
            },
            _ => {
                self.lifetime = Lifetime::Process {
                    scope,
                    controls,
                    birth,
                };
                return Err(Failure::State);
            }
        };
        let cleanup = match scope {
            Scope::Published(group) => group.remove_after_reap(),
            Scope::Retained(held) => held.retry_remove(),
        };
        self.lifetime = match cleanup {
            execution::Cleanup::Removed => {
                self.retirement = Retirement::Notify;
                Lifetime::Retired(retired_birth)
            }
            execution::Cleanup::Retained(held) => Lifetime::Process {
                scope: Scope::Retained(held),
                controls,
                birth,
            },
        };
        Ok(if matches!(self.lifetime, Lifetime::Retired(_)) {
            protocol::Reply::Retired
        } else {
            protocol::Reply::Retained
        })
    }
}

fn population_empty(events: &File) -> Result<bool, Failure> {
    crate::managed::population_empty(events).map_err(|cause| native(Operation::Population, cause))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shards_abi::run::{
        managed::Epoch,
        native_grants::{Entry, Header, ObjectKind, Processes, Rights, Snapshot, Source},
    };
    use std::os::unix::fs::MetadataExt as _;

    fn selected(effect: protocol::Effect, root: &File) -> Selected {
        let metadata = root.metadata().unwrap();
        let entry = Entry {
            source: Source::Image,
            rights: Rights::READ,
            object: Object {
                device: metadata.dev(),
                inode: metadata.ino(),
                kind: ObjectKind::Directory,
            },
            path: b"/",
        };
        let header = Header {
            scope: wire::Scope {
                epoch: Epoch {
                    run: [0x5a; 32],
                    generation: NonZeroU64::MIN,
                },
                member: 0,
            },
            processes: Processes::Allowed,
            entries: 1,
            body_bytes: entry.encoded_len().unwrap() as u64,
        };
        let mut bytes = header.encode().unwrap().to_vec();
        let start = bytes.len();
        bytes.resize(start + entry.encoded_len().unwrap(), 0);
        entry.encode(bytes.get_mut(start..).unwrap()).unwrap();
        let snapshot = Snapshot::decode(&bytes).unwrap();
        let objects = if matches!(effect, protocol::Effect::Process { .. }) {
            vec![entry.object, entry.object]
        } else {
            vec![entry.object]
        };
        Selected::from_selection(operations::select(&snapshot, header.scope, effect, &objects).unwrap())
    }

    fn read_job(root: &File) -> Job {
        let selection = selected(
            protocol::Effect::Read {
                root: 0,
                offset: 0,
                bytes: 7,
            },
            root,
        );
        match Job::acquire(
            NonZeroU64::MIN,
            selection,
            protocol::PreparedFacts {
                retained_bytes: 0,
                descriptors: 0,
            },
            root,
        ) {
            Acquisition::Prepared(job) => job,
            _ => panic!("file state acquisition failed"),
        }
    }

    #[test]
    fn native_owner_file_commit_cancel_keeps_typed_accepted_prefix_and_terminal() {
        let root = File::open(".").unwrap();
        let mut job = read_job(&root);
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stdout(),
            "OWNER_RECORD_LAYOUT Job={} Selected={} Lifetime={} bytes; layout only, allocator/kernel/backing unverified",
            size_of::<Job>(),
            size_of::<Selected>(),
            size_of::<Lifetime>()
        );
        assert!(job.publish().unwrap().is_none());
        job.commit().unwrap();
        assert!(job.cancel().unwrap());
        job.complete(protocol::Outcome::Uncertain, 3).unwrap();
        assert_eq!(
            job.completion,
            Some(Completion {
                outcome: protocol::Outcome::Uncertain,
                accepted_bytes: 3
            })
        );
        assert_eq!(job.retire().unwrap(), protocol::Reply::Retired);
        assert!(job.terminal_pending());
        job.terminal_delivered();
        assert!(!job.terminal_pending());
        assert_eq!(job.commit(), Err(Failure::State));
        assert_eq!(job.complete(protocol::Outcome::Success, 3), Err(Failure::State));
        assert!(job.commitment());
    }

    #[test]
    fn native_owner_precommit_cancellation_never_claims_accepted_bytes_or_success() {
        let root = File::open(".").unwrap();
        let mut job = read_job(&root);
        assert!(!job.cancel().unwrap());
        assert_eq!(job.commit(), Err(Failure::State));
        assert_eq!(job.complete(protocol::Outcome::Success, 0), Err(Failure::Bytes));
        assert_eq!(job.complete(protocol::Outcome::Cancelled, 1), Err(Failure::Bytes));
        assert!(job.completion.is_none());
        job.complete(protocol::Outcome::Cancelled, 0).unwrap();
        assert_eq!(job.retire().unwrap(), protocol::Reply::Retired);
        assert!(!job.commitment());
    }

    #[test]
    fn native_owner_read_prefix_never_exceeds_the_exact_admitted_effect() {
        let root = File::open(".").unwrap();
        let mut job = read_job(&root);
        job.commit().unwrap();
        assert_eq!(job.complete(protocol::Outcome::Success, 8), Err(Failure::Bytes));
        assert!(job.completion.is_none());
        job.complete(protocol::Outcome::Success, 7).unwrap();
        assert_eq!(job.retire().unwrap(), protocol::Reply::Retired);
        assert!(job.completion.is_some());
    }

    #[test]
    fn native_owner_regular_filesystem_never_becomes_a_process_cgroup() {
        let root = File::open(".").unwrap();
        let selection = selected(
            protocol::Effect::Process {
                executable_root: 0,
                workdir_root: 0,
            },
            &root,
        );
        match Job::acquire(
            NonZeroU64::MIN,
            selection,
            protocol::PreparedFacts {
                retained_bytes: 0,
                descriptors: 0,
            },
            &root,
        ) {
            Acquisition::Refused(Rejection::Resources(cause)) => {
                assert_eq!(cause.cause, admission::Cause::WrongFilesystem)
            }
            _ => panic!("regular directory acquired as job cgroup"),
        }
    }

    #[test]
    fn native_owner_file_operation_cannot_forge_a_native_reap_or_notborn() {
        let root = File::open(".").unwrap();
        let mut job = read_job(&root);
        assert_eq!(job.reaped(libc::CLD_EXITED, 0), Err(Failure::State));
        assert_eq!(job.not_born(), Err(Failure::State));
        assert!(job.completion.is_none());
        assert_eq!(job.retire().unwrap(), protocol::Reply::Retained);
    }
}
