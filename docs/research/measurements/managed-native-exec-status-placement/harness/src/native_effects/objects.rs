use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd};

use super::file;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Kind,
    Stat(file::ReadFailure),
    ExecuteAccess(Option<i32>),
}

pub(crate) struct Rejected {
    pub object: OwnedFd,
    pub failure: Failure,
}

impl std::fmt::Debug for Rejected {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Rejected")
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

/// A native checked object, not an execution grant. The protected owner must
/// select this descriptor from its actual allowed view and authorize its use.
pub(crate) struct Executable(OwnedFd);

impl Executable {
    pub fn check(object: OwnedFd) -> Result<Self, Rejected> {
        let check = || {
            let metadata = file::stat(object.as_fd()).map_err(Failure::Stat)?;
            if metadata.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Err(Failure::Kind);
            }
            // SAFETY: faccessat2 checks the held object with this helper's actual
            // effective identity. It neither follows another pathname nor grants use.
            if unsafe {
                libc::syscall(
                    libc::SYS_faccessat2,
                    object.as_raw_fd(),
                    c"".as_ptr(),
                    libc::X_OK,
                    libc::AT_EMPTY_PATH | libc::AT_EACCESS,
                )
            } != 0
            {
                return Err(Failure::ExecuteAccess(
                    std::io::Error::last_os_error().raw_os_error(),
                ));
            }
            Ok(())
        };
        if let Err(failure) = check() {
            return Err(Rejected { object, failure });
        }
        Ok(Self(object))
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// A held working-directory object; renaming its path does not replace it.
pub(crate) struct Workdir(OwnedFd);

impl Workdir {
    pub fn check(object: OwnedFd) -> Result<Self, Rejected> {
        let failure = match file::stat(object.as_fd()) {
            Ok(metadata) if metadata.st_mode & libc::S_IFMT == libc::S_IFDIR => return Ok(Self(object)),
            Ok(_) => Failure::Kind,
            Err(error) => Failure::Stat(error),
        };
        Err(Rejected { object, failure })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn executable_checks_native_permission_and_returns_rejected_object() {
        let tmp = shards_testdir::TempDir::new("native-exec-object").unwrap();
        let path = tmp.join("program");
        std::fs::write(&path, b"owned fixture, not a valid executable image").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let object: OwnedFd = std::fs::File::open(&path).unwrap().into();
        let error = Executable::check(object).err().unwrap();
        assert!(matches!(
            error.failure,
            Failure::ExecuteAccess(Some(libc::EACCES))
        ));
        assert!(file::stat(error.object.as_fd()).is_ok());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let checked = Executable::check(error.object).unwrap();
        assert!(file::stat(checked.fd()).is_ok());
        // Executable permission is not a claim that the kernel accepted an exec.
        let dir: OwnedFd = std::fs::File::open(&tmp).unwrap().into();
        assert!(matches!(
            Executable::check(dir),
            Err(Rejected {
                failure: Failure::Kind,
                ..
            })
        ));
    }

    #[test]
    fn workdir_stays_bound_to_the_held_object_after_rename() {
        let tmp = shards_testdir::TempDir::new("native-workdir-object").unwrap();
        std::fs::create_dir(tmp.join("work")).unwrap();
        let object: OwnedFd = std::fs::File::open(tmp.join("work")).unwrap().into();
        let checked = Workdir::check(object).unwrap();
        let original = file::stat(checked.fd()).unwrap();
        std::fs::rename(tmp.join("work"), tmp.join("old")).unwrap();
        std::fs::create_dir(tmp.join("work")).unwrap();
        let replacement = std::fs::File::open(tmp.join("work")).unwrap();
        let old = file::stat(checked.fd()).unwrap();
        let other = file::stat(replacement.as_fd()).unwrap();
        assert_eq!((old.st_dev, old.st_ino), (original.st_dev, original.st_ino));
        assert_ne!((old.st_dev, old.st_ino), (other.st_dev, other.st_ino));
        std::fs::write(tmp.join("file"), b"x").unwrap();
        let regular: OwnedFd = std::fs::File::open(tmp.join("file")).unwrap().into();
        let error = Workdir::check(regular).err().unwrap();
        assert_eq!(error.failure, Failure::Kind);
        assert!(file::stat(error.object.as_fd()).is_ok());
    }
}
