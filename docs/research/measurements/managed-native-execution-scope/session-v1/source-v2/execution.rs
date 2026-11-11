//! One actual job's cgroup lifetime object under its held member. This acquires
//! no process permission, controller entitlement, pidfd, audit or release ACK.

use std::fs::File;
use std::num::NonZeroU64;
use std::os::fd::AsRawFd as _;

use super::{Cause, Disposition, Group, Refusal, Removal, Rollback};

#[derive(Debug)]
pub(crate) enum Acquisition {
    Prepared(Prepared),
    Refused(Refusal),
    Retained(Retained),
}

#[derive(Debug)]
pub(crate) enum Cleanup {
    /// Only directory removal, never a claim of reaping or returned charges.
    Removed,
    Retained(Retained),
}

/// The original owner-issued Worker identity selects the component, not rights.
/// Both lifetime controls exist before this object can be passed to native birth.
#[derive(Debug)]
pub(crate) struct Prepared {
    job: NonZeroU64,
    group: Group,
    controls: crate::managed::Controls,
}

impl Prepared {
    /// The trusted current owner supplies its actual held member and checked
    /// Worker ID. A cgroup filesystem check is a native fact, not authorization.
    pub(crate) fn acquire(member: &File, job: NonZeroU64) -> Acquisition {
        if let Err(refusal) = member_filesystem(member) {
            return Acquisition::Refused(refusal);
        }
        let mut name = [0; super::NUMBER_BYTES];
        let name = match std::str::from_utf8(super::decimal(job.get(), &mut name)) {
            Ok(name) => name,
            Err(_) => return Acquisition::Refused(Refusal::new(c"execution name", Cause::InvalidNumber)),
        };
        match Group::acquire(member, name, false) {
            super::Acquisition::Created(group) => Self::finish(group, job),
            super::Acquisition::Refused(refusal) => Acquisition::Refused(refusal),
            super::Acquisition::Retained { refusal, removal } => Acquisition::Retained(Retained {
                job,
                holding: Holding::Unopened(removal),
                preparation: Some(refusal),
                removal: refusal.rollback,
            }),
        }
    }

    fn finish(group: Group, job: NonZeroU64) -> Acquisition {
        let controls: Result<crate::managed::Controls, Refusal> = (|| {
            let kill = super::open_at(&group.directory, c"cgroup.kill", libc::O_WRONLY)?;
            let events = super::open_at(&group.directory, c"cgroup.events", libc::O_RDONLY)?;
            Ok(crate::managed::Controls::held(kill, events))
        })();
        match controls {
            Ok(controls) => Acquisition::Prepared(Self { job, group, controls }),
            Err(mut refusal) => match remove(Holding::Opened(group), job, Some(refusal)) {
                Cleanup::Removed => {
                    refusal.rollback = Rollback::Removed;
                    Acquisition::Refused(refusal)
                }
                Cleanup::Retained(held) => Acquisition::Retained(held),
            },
        }
    }

    pub(crate) fn job(&self) -> NonZeroU64 {
        self.job
    }

    /// Borrow this SAME held directory for atomic CLONE_INTO_CGROUP. The caller
    /// retains all authority and charges; actors receive no migration/control FD.
    pub(crate) fn directory(&self) -> &File {
        &self.group.directory
    }

    /// Use after any possible birth, including a lost/uncertain birth reply.
    /// The real owner keeps these objects with its actual pidfd/ACK/current state.
    /// Drop after publication cannot remove the directory or imply a refund.
    pub(crate) fn publish(self) -> (Published, crate::managed::Controls) {
        let Self {
            job,
            mut group,
            controls,
        } = self;
        group.removal.disposition = Disposition::Retained;
        (Published { job, group }, controls)
    }

    /// Only before any possible birth. Unknown birth must publish/retain instead.
    /// Explicit cancellation reports failed removal with the SAME held objects;
    /// an unused Group's best-effort Drop is never a successful cleanup receipt.
    pub(crate) fn cancel_before_birth(self) -> Cleanup {
        let Self { job, group, controls } = self;
        drop(controls);
        remove(Holding::Opened(group), job, None)
    }
}

#[derive(Debug)]
pub(crate) struct Published {
    job: NonZeroU64,
    group: Group,
}

impl Published {
    pub(crate) fn job(&self) -> NonZeroU64 {
        self.job
    }

    pub(crate) fn directory(&self) -> &File {
        &self.group.directory
    }

    /// The authoritative owner must first observe this scope empty AND verify
    /// matching actual reaping. rmdir enforces emptiness, not the reap condition.
    /// Keep pidfd, Controls and charges on failure; no replay or leader-only end.
    pub(crate) fn remove_after_reap(self) -> Cleanup {
        remove(Holding::Opened(self.group), self.job, None)
    }
}

#[derive(Debug)]
enum Holding {
    Opened(Group),
    Unopened(Removal),
}

impl Holding {
    fn removal(&mut self) -> &mut Removal {
        match self {
            Self::Opened(group) => &mut group.removal,
            Self::Unopened(removal) => removal,
        }
    }
}

/// A cleanup obligation, including the actual held parent/name even if opening
/// the new directory failed. Drop closes objects; it does not retry or claim end.
#[derive(Debug)]
pub(crate) struct Retained {
    job: NonZeroU64,
    holding: Holding,
    preparation: Option<Refusal>,
    removal: Rollback,
}

impl Retained {
    pub(crate) fn job(&self) -> NonZeroU64 {
        self.job
    }

    pub(crate) fn preparation_failure(&self) -> Option<Refusal> {
        self.preparation
    }

    pub(crate) fn removal_failure(&self) -> Rollback {
        self.removal
    }

    pub(crate) fn directory(&self) -> Option<&File> {
        match &self.holding {
            Holding::Opened(group) => Some(&group.directory),
            Holding::Unopened(_) => None,
        }
    }

    /// Retry only after proven no-birth, or actual empty plus matching reaping.
    /// This makes one native removal attempt; it adds no watcher, timer or poller.
    pub(crate) fn retry_remove(self) -> Cleanup {
        remove(self.holding, self.job, self.preparation)
    }
}

fn remove(mut holding: Holding, job: NonZeroU64, preparation: Option<Refusal>) -> Cleanup {
    let guard = holding.removal();
    // Only this owner re-arms the SAME one-component removal implementation for
    // an explicit attempt. On failure Removal marks itself Retained before return.
    guard.disposition = Disposition::Unused;
    let removal = guard.remove();
    match removal {
        Rollback::Removed => Cleanup::Removed,
        // NotCreated is retained uncertainty, never translated into success.
        Rollback::NotCreated | Rollback::Failed(_) => Cleanup::Retained(Retained {
            job,
            holding,
            preparation,
            removal,
        }),
    }
}

fn member_filesystem(member: &File) -> Result<(), Refusal> {
    let mut metadata = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: fstatfs writes this native output for the actual held member.
    if unsafe { libc::fstatfs(member.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
        return Err(Refusal::io(c"execution member", std::io::Error::last_os_error()));
    }
    // SAFETY: successful fstatfs initialized the complete output above.
    if i128::from(unsafe { metadata.assume_init() }.f_type) != i128::from(libc::CGROUP2_SUPER_MAGIC) {
        return Err(Refusal::new(c"execution member", Cause::WrongFilesystem));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    fn job() -> NonZeroU64 {
        NonZeroU64::new(1).unwrap()
    }

    fn opened(member: &File, job: NonZeroU64) -> Group {
        match Group::acquire(member, &job.to_string(), false) {
            super::super::Acquisition::Created(group) => group,
            other => panic!("regular-directory source projection: {other:?}"),
        }
    }

    fn count() -> usize {
        std::fs::read_dir("/proc/self/fd").unwrap().count()
    }

    #[test]
    fn regular_filesystem_is_refused_before_any_component_is_created() {
        let root = shards_testdir::TempDir::new("execution-wrong-fs").unwrap();
        let member = File::open(&root).unwrap();
        let refusal = match Prepared::acquire(&member, job()) {
            Acquisition::Refused(refusal) => refusal,
            other => panic!("regular filesystem acquired as cgroup: {other:?}"),
        };
        assert_eq!(refusal.cause, Cause::WrongFilesystem);
        assert!(!root.join(job().to_string()).exists());
    }

    #[test]
    fn source_projection_missing_controls_removes_only_its_new_component() {
        let root = shards_testdir::TempDir::new("execution-missing").unwrap();
        let member = File::open(&root).unwrap();
        let group = opened(&member, job());
        let refusal = match Prepared::finish(group, job()) {
            Acquisition::Refused(refusal) => refusal,
            other => panic!("missing controls: {other:?}"),
        };
        assert_eq!(refusal.cause, Cause::Io { errno: libc::ENOENT });
        assert_eq!(refusal.rollback, Rollback::Removed);
        assert!(!root.join(job().to_string()).exists());
    }

    #[test]
    fn source_projection_existing_component_is_never_adopted_or_removed() {
        let root = shards_testdir::TempDir::new("execution-existing").unwrap();
        let member = File::open(&root).unwrap();
        let name = job().to_string();
        std::fs::create_dir(root.join(&name)).unwrap();
        std::fs::write(root.join(&name).join("marker"), b"preserved").unwrap();
        let refusal = match Group::acquire(&member, &name, false) {
            super::super::Acquisition::Refused(refusal) => refusal,
            other => panic!("existing component adopted: {other:?}"),
        };
        assert_eq!(refusal.cause, Cause::Io { errno: libc::EEXIST });
        assert_eq!(
            std::fs::read(root.join(name).join("marker")).unwrap(),
            b"preserved"
        );
    }

    #[test]
    fn source_projection_failed_rollback_retains_same_parent_name_and_errno() {
        let before = count();
        let root = shards_testdir::TempDir::new("execution-retained").unwrap();
        {
            let member_path = root.join("member");
            std::fs::create_dir(&member_path).unwrap();
            let member = File::open(&member_path).unwrap();
            let group = opened(&member, job());
            let name = job().to_string();
            std::fs::write(member_path.join(&name).join("marker"), b"old object").unwrap();
            let retained = match Prepared::finish(group, job()) {
                Acquisition::Retained(held) => held,
                other => panic!("failed unlink lost ownership: {other:?}"),
            };
            assert_eq!(retained.job(), job());
            assert!(retained.directory().is_some());
            assert_eq!(
                retained.preparation_failure().unwrap().cause,
                Cause::Io { errno: libc::ENOENT }
            );
            assert_eq!(retained.removal_failure(), Rollback::Failed(libc::ENOTEMPTY));
            let original = root.join("original");
            std::fs::rename(&member_path, &original).unwrap();
            std::fs::create_dir(&member_path).unwrap();
            std::fs::create_dir(member_path.join(&name)).unwrap();
            std::fs::write(member_path.join(&name).join("marker"), b"replacement untouched").unwrap();
            std::fs::remove_file(original.join(&name).join("marker")).unwrap();
            assert!(matches!(retained.retry_remove(), Cleanup::Removed));
            assert!(!original.join(&name).exists());
            assert_eq!(
                std::fs::read(member_path.join(name).join("marker")).unwrap(),
                b"replacement untouched"
            );
        }
        drop(root);
        let after = count();
        let _ = writeln!(
            std::io::stdout().lock(),
            "EXECUTION_SCOPE retained-parent fixture FD {before}->{after}; host_cgroup_mutations=0 kernel_population=Unverified"
        );
        assert_eq!(before, after);
    }

    #[test]
    fn source_projection_controls_precede_publication_and_drop_does_not_remove() {
        let root = shards_testdir::TempDir::new("execution-publish").unwrap();
        let member = File::open(&root).unwrap();
        let group = opened(&member, job());
        let path = root.join(job().to_string());
        std::fs::write(path.join("cgroup.kill"), b"untouched").unwrap();
        std::fs::write(
            path.join("cgroup.events"),
            b"regular fixture, not a kernel observation",
        )
        .unwrap();
        let prepared = match Prepared::finish(group, job()) {
            Acquisition::Prepared(prepared) => prepared,
            other => panic!("regular-file controls: {other:?}"),
        };
        assert_eq!(prepared.job(), job());
        assert!(prepared.directory().metadata().unwrap().is_dir());
        let (published, controls) = prepared.publish();
        assert_eq!(published.job(), job());
        assert!(published.directory().metadata().unwrap().is_dir());
        assert_eq!(std::fs::read(path.join("cgroup.kill")).unwrap(), b"untouched");
        drop(controls);
        std::fs::remove_file(path.join("cgroup.kill")).unwrap();
        std::fs::remove_file(path.join("cgroup.events")).unwrap();
        drop(published);
        assert!(path.is_dir(), "publication must disarm best-effort Drop removal");
    }

    #[test]
    fn source_projection_open_failure_retains_actual_unopened_removal() {
        // SAFETY: identity observation only; this ordinary permission projection
        // requires the normally admitted non-root runner, never host-root setup.
        assert_ne!(unsafe { libc::geteuid() }, 0);
        let root = shards_testdir::TempDir::new("execution-open").unwrap();
        let member = File::open(&root).unwrap();
        let name = job().to_string();
        let removal = Group::prepare_removal(&member, &name, false).unwrap();
        let path = root.join(&name);
        std::fs::write(path.join("marker"), b"private nonempty directory").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0)).unwrap();
        let (refusal, removal) = match Group::open(removal) {
            super::super::Acquisition::Retained { refusal, removal } => (refusal, removal),
            other => panic!("open/rollback source projection: {other:?}"),
        };
        assert_eq!(refusal.cause, Cause::Io { errno: libc::EACCES });
        assert_eq!(refusal.rollback, Rollback::Failed(libc::ENOTEMPTY));
        let retained = Retained {
            job: job(),
            holding: Holding::Unopened(removal),
            preparation: Some(refusal),
            removal: refusal.rollback,
        };
        assert!(retained.directory().is_none());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_file(path.join("marker")).unwrap();
        assert!(matches!(retained.retry_remove(), Cleanup::Removed));
        assert!(!path.exists());
    }

    #[test]
    fn source_projection_explicit_cancel_and_terminal_removal_keep_failed_objects() {
        let root = shards_testdir::TempDir::new("execution-explicit").unwrap();
        let member = File::open(&root).unwrap();
        let group = opened(&member, job());
        let path = root.join(job().to_string());
        std::fs::write(path.join("cgroup.kill"), b"untouched").unwrap();
        std::fs::write(path.join("cgroup.events"), b"source projection only").unwrap();
        let prepared = match Prepared::finish(group, job()) {
            Acquisition::Prepared(prepared) => prepared,
            other => panic!("regular-file preparation: {other:?}"),
        };
        let retained = match prepared.cancel_before_birth() {
            Cleanup::Retained(held) => held,
            Cleanup::Removed => panic!("nonempty regular directory unexpectedly removed"),
        };
        assert_eq!(retained.removal_failure(), Rollback::Failed(libc::ENOTEMPTY));
        assert!(retained.preparation_failure().is_none());
        std::fs::remove_file(path.join("cgroup.kill")).unwrap();
        std::fs::remove_file(path.join("cgroup.events")).unwrap();
        assert!(matches!(retained.retry_remove(), Cleanup::Removed));

        let group = opened(&member, job());
        std::fs::write(path.join("cgroup.kill"), b"untouched").unwrap();
        std::fs::write(path.join("cgroup.events"), b"source projection only").unwrap();
        let prepared = match Prepared::finish(group, job()) {
            Acquisition::Prepared(prepared) => prepared,
            other => panic!("regular-file preparation: {other:?}"),
        };
        let (published, controls) = prepared.publish();
        drop(controls);
        std::fs::remove_file(path.join("cgroup.kill")).unwrap();
        std::fs::remove_file(path.join("cgroup.events")).unwrap();
        // This is a state/source projection with no process ever born, not a
        // simulated wait receipt or a proof of actual kernel population/reaping.
        assert!(matches!(published.remove_after_reap(), Cleanup::Removed));
    }
}
