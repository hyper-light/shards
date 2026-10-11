//! A directory a test has of its own, in the system's temporary directory, removed as the
//! test goes, a failing test's too: its value is dropped as the panic unwinds. Each test
//! process's are under `shards-tests/<pid>/`, so that a process killed before it removed
//! its own leaves them where the next one finds them: the first directory a process makes
//! removes those of every process that has ended. Only that directory is swept, and only
//! a process's that no longer runs.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, Once, PoisonError};

/// How many of this process's directories are live: its own directory goes with the last,
/// and is made, or found, under this lock alone, so that none is made in it as it goes.
static LIVE: Mutex<usize> = Mutex::new(0);

/// Every test process's directories' parent.
#[expect(
    clippy::disallowed_methods,
    reason = "the one place a test's directory is made in the system's temporary directory"
)]
fn parent() -> PathBuf {
    std::env::temp_dir().join("shards-tests")
}

/// A test's directory, made empty, removed as it is dropped.
#[derive(Debug)]
pub struct TempDir(PathBuf);

impl TempDir {
    /// A directory of the test's own, named for `name`: `shards-tests/<pid>/<name>-<n>`
    /// under the system's temporary directory.
    pub fn new(name: &str) -> io::Result<TempDir> {
        static SWEPT: Once = Once::new();
        static N: AtomicUsize = AtomicUsize::new(0);
        let parent = parent();
        SWEPT.call_once(|| sweep(&parent));
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = parent
            .join(std::process::id().to_string())
            .join(format!("{name}-{n}"));
        let mut live = LIVE.lock().unwrap_or_else(PoisonError::into_inner);
        std::fs::create_dir_all(&dir)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", dir.display())))?;
        *live += 1;
        Ok(TempDir(dir))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<std::ffi::OsStr> for TempDir {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.as_os_str()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        remove(&self.0);
        // The process's own directory, with its last.
        let mut live = LIVE.lock().unwrap_or_else(PoisonError::into_inner);
        *live = live.saturating_sub(1);
        if *live == 0
            && let Some(own) = self.0.parent()
        {
            let _ = std::fs::remove_dir(own);
        }
    }
}

/// Removes `dir` and all in it, a test's directories it made unreadable or unwritable
/// too: where a removal fails, each directory in it is made its owner's to read, write and
/// search (no link followed), and it is removed again.
fn remove(dir: &Path) {
    if std::fs::remove_dir_all(dir).is_ok() || !dir.exists() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fn open_up(dir: &Path) {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    open_up(&entry.path());
                }
            }
        }
        open_up(dir);
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Removes the directories of test processes that have ended, under `parent`: each named
/// by its process's ID, removed once no process has that ID (kill(2) with no signal:
/// ESRCH). One whose ID another process has taken since stays until that one ends too.
#[cfg(unix)]
fn sweep(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let me = std::process::id();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|&pid| pid != me)
            .and_then(|pid| libc::pid_t::try_from(pid).ok())
            .filter(|&pid| pid > 0)
        else {
            continue;
        };
        // SAFETY: kill(2) with no signal sends nothing; it asks whether the process is.
        let gone = unsafe { libc::kill(pid, 0) } != 0
            && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            remove(&entry.path());
        }
    }
}

/// Windows tests run on runners made for each run, where nothing is left from before.
#[cfg(not(unix))]
fn sweep(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory is made empty and removed as it goes, a panicking test's too; the
    /// process's own directory goes with its last; another process's that has ended is
    /// swept as the next is made, one that runs is not.
    #[test]
    fn a_tests_directory_goes_with_it() {
        let dir = TempDir::new("one").unwrap();
        let path = dir.to_path_buf();
        assert!(path.is_dir() && std::fs::read_dir(&path).unwrap().next().is_none());
        std::fs::write(path.join("f"), "f").unwrap();
        drop(dir);
        assert!(!path.exists());
        let held = std::thread::spawn(|| {
            let dir = TempDir::new("panicking").unwrap();
            let path = dir.to_path_buf();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _dir = dir;
                panic!("a failing test");
            }));
            (r.is_err(), path)
        })
        .join()
        .unwrap();
        assert!(held.0 && !held.1.exists());
        // One holding a directory none may read, write or search, which a guest may make.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = TempDir::new("closed").unwrap();
            let path = dir.to_path_buf();
            std::fs::create_dir_all(path.join("d/e")).unwrap();
            std::fs::write(path.join("d/e/f"), "f").unwrap();
            for d in ["d/e", "d"] {
                std::fs::set_permissions(path.join(d), std::fs::Permissions::from_mode(0o0)).unwrap();
            }
            drop(dir);
            assert!(!path.exists());
        }
    }

    /// Directories made and dropped on many threads at once are each made: the process's
    /// own goes only with its last, never from under one being made (before, mkdir there
    /// failed, EINVAL on macOS, as another's drop took it).
    #[test]
    fn many_at_once_are_each_made() {
        std::thread::scope(|s| {
            for t in 0..16 {
                s.spawn(move || {
                    for i in 0..200 {
                        let dir = TempDir::new(&format!("race-{t}")).unwrap();
                        std::fs::write(dir.join("f"), i.to_string()).unwrap();
                    }
                });
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn an_ended_processs_directories_are_swept() {
        let parent = TempDir::new("sweep").unwrap();
        // A process that ended, and one that runs: this one's parent, which never ends
        // before its child.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let ended = child.id();
        child.wait().unwrap();
        let running = std::os::unix::process::parent_id();
        for pid in [ended, running] {
            std::fs::create_dir_all(parent.join(pid.to_string()).join("left-0")).unwrap();
        }
        std::fs::create_dir_all(parent.join("not-a-pid")).unwrap();
        sweep(&parent);
        assert!(!parent.join(ended.to_string()).exists());
        assert!(parent.join(running.to_string()).exists());
        assert!(parent.join("not-a-pid").exists());
    }
}
