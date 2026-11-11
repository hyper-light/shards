//! Actual inode access needed by the unchanged capability-free helper's atomic
//! cgroup birth. This does not authorize an operation or establish its backing.

use std::fs::File;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::MetadataExt as _;

use shards_abi::run::native_grants::Scope;

use super::{Cause, Group, Refusal, Staged};

/// Produced only from the original normalized Domain and its unused Staged
/// member. Neither a wire UID nor a directory descriptor constructs this object.
pub(crate) struct Member {
    directory: File,
    procs: File,
    scope: Scope,
    principal: libc::uid_t,
}

impl std::fmt::Debug for Member {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("NativeBirthMember").finish_non_exhaustive()
    }
}

impl Member {
    pub(super) fn prepare(
        staged: &Staged,
        domain: &crate::domains::Domain,
        scope: Scope,
    ) -> Result<Self, Refusal> {
        if domain.id.checked_sub(crate::domains::FIRST_ID) != Some(scope.member)
            || staged.group.removal.name.strip_suffix(&[0]) != Some(domain.cgroup.as_bytes())
            || staged.group.removal.disposition != super::Disposition::Unused
        {
            return Err(Refusal::new(c"native member", Cause::BirthBinding));
        }
        super::execution::member_filesystem(staged.group())?;
        // Acquire every retained descriptor before changing access. Both stay
        // private to INIT; the helper receives only actual per-job directories.
        let directory = staged
            .group()
            .try_clone()
            .map_err(|error| Refusal::io(c"native member", error))?;
        let procs = super::open_at(&directory, c"cgroup.procs", libc::O_RDONLY)?;
        grant(&directory, &procs, domain.id)?;
        Ok(Self {
            directory,
            procs,
            scope,
            principal: domain.id,
        })
    }

    pub(crate) fn scope(&self) -> Scope {
        self.scope
    }

    /// Compare the actual current Owned group's handle with the originally
    /// prepared member, and observe that its narrow access is still installed.
    pub(crate) fn check_member(&self, actual: &File) -> Result<(), Refusal> {
        let original = self
            .directory
            .metadata()
            .map_err(|error| Refusal::io(c"native member", error))?;
        let current = actual
            .metadata()
            .map_err(|error| Refusal::io(c"native member", error))?;
        if original.dev() != current.dev() || original.ino() != current.ino() {
            return Err(Refusal::new(c"native member", Cause::BirthBinding));
        }
        let procs = self
            .procs
            .metadata()
            .map_err(|error| Refusal::io(c"cgroup.procs", error))?;
        if procs.uid() != self.principal || !narrow_writer(procs.mode()) {
            return Err(Refusal::new(c"cgroup.procs", Cause::BirthPermissions));
        }
        Ok(())
    }

    pub(super) fn directory(&self) -> &File {
        &self.directory
    }

    pub(super) fn prepare_job(&self, group: &Group) -> Result<(), Refusal> {
        self.check_member(&group.removal.parent)?;
        if group.removal.disposition != super::Disposition::Unused {
            return Err(Refusal::new(c"native job", Cause::BirthBinding));
        }
        let procs = super::open_at(&group.directory, c"cgroup.procs", libc::O_RDONLY)?;
        grant(&group.directory, &procs, self.principal)
    }
}

// Linux MAY_WRITE is satisfied by owner-write for the original helper UID.
// Group/other write would delegate beyond that principal. No mode is changed.
fn narrow_writer(mode: libc::mode_t) -> bool {
    mode & libc::S_IFMT == libc::S_IFREG
        && mode & libc::S_IWUSR != 0
        && mode & (libc::S_IWGRP | libc::S_IWOTH) == 0
}

fn grant(directory: &File, procs: &File, principal: libc::uid_t) -> Result<(), Refusal> {
    let parent = directory
        .metadata()
        .map_err(|error| Refusal::io(c"native group", error))?;
    let before = procs
        .metadata()
        .map_err(|error| Refusal::io(c"cgroup.procs", error))?;
    if !parent.is_dir()
        || before.dev() != parent.dev()
        || before.uid() != parent.uid()
        || !narrow_writer(before.mode())
    {
        return Err(Refusal::new(c"cgroup.procs", Cause::BirthPermissions));
    }
    // Linux v6.18 cgroup.c:5292-5357,6689-6763 checks CURRENT helper
    // credentials at destination and common ancestor. Change ONLY this held
    // procs inode's UID, retaining its actual group/mode and all directory and
    // controller ownership. Actor path/FD exclusion remains independently owed.
    // SAFETY: procs is the NOFOLLOW fixed child of the held cgroup-v2 directory;
    // INIT owns both descriptors and uses only the original normalized UID.
    if unsafe { libc::fchown(procs.as_raw_fd(), principal, before.gid()) } != 0 {
        return Err(Refusal::io(c"cgroup.procs", std::io::Error::last_os_error()));
    }
    let after = procs
        .metadata()
        .map_err(|error| Refusal::io(c"cgroup.procs", error))?;
    if after.dev() != before.dev()
        || after.ino() != before.ino()
        || after.uid() != principal
        || after.gid() != before.gid()
        || after.mode() != before.mode()
    {
        return Err(Refusal::new(c"cgroup.procs", Cause::BirthOwnership));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    #[test]
    fn only_regular_owner_write_without_other_writers_is_accepted() {
        assert!(narrow_writer(libc::S_IFREG | libc::S_IRUSR | libc::S_IWUSR));
        for mode in [
            libc::S_IFREG | libc::S_IRUSR,
            libc::S_IFDIR | libc::S_IWUSR,
            libc::S_IFLNK | libc::S_IWUSR,
            libc::S_IFREG | libc::S_IWUSR | libc::S_IWGRP,
            libc::S_IFREG | libc::S_IWUSR | libc::S_IWOTH,
        ] {
            assert!(!narrow_writer(mode));
        }
    }

    #[test]
    fn binding_and_filesystem_refusals_precede_any_inode_change() {
        let before = std::fs::read_dir("/proc/self/fd").unwrap().count();
        let root = shards_testdir::TempDir::new("birth-binding").unwrap();
        {
            let parent = File::open(&root).unwrap();
            let group = Group::create(&parent, "agent-birth", false).unwrap();
            let staged = Staged { group };
            let path = root.join("agent-birth/cgroup.procs");
            std::fs::write(&path, b"ordinary file, not a cgroup grant").unwrap();
            let original = path.metadata().unwrap();
            let mut domain = crate::domains::Domain {
                label: "agent birth".into(),
                cgroup: "agent-birth".into(),
                dir: Vec::new(),
                id: crate::domains::FIRST_ID,
                pids: None,
                no_processes: false,
                memory: None,
                argv: Vec::new(),
                env: Vec::new(),
                workdir: std::ffi::CString::default(),
                name: (false, "birth".into()),
                link: None,
                channels: Vec::new(),
                mcp: "[]".into(),
                resolv: None,
                volumes: Vec::new(),
            };
            let scope = Scope {
                epoch: shards_abi::run::managed::Epoch {
                    run: [0x51; 32],
                    generation: NonZeroU64::MIN,
                },
                member: 0,
            };
            assert_eq!(
                Member::prepare(&staged, &domain, Scope { member: 1, ..scope })
                    .unwrap_err()
                    .cause,
                Cause::BirthBinding
            );
            domain.cgroup = "other".into();
            assert_eq!(Member::prepare(&staged, &domain, scope).unwrap_err().cause, Cause::BirthBinding);
            domain.cgroup = "agent-birth".into();
            assert_eq!(Member::prepare(&staged, &domain, scope).unwrap_err().cause, Cause::WrongFilesystem);
            let after = path.metadata().unwrap();
            assert_eq!((original.uid(), original.gid(), original.mode(), original.ino()),
                (after.uid(), after.gid(), after.mode(), after.ino()));
            assert_eq!(std::fs::read(&path).unwrap(), b"ordinary file, not a cgroup grant");
            std::fs::remove_file(path).unwrap();
        }
        drop(root);
        let after = std::fs::read_dir("/proc/self/fd").unwrap().count();
        assert_eq!(before, after);
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout().lock(), "BIRTH_BINDING FD {before}->{after}; host_permissions_changed=0");
    }
}
