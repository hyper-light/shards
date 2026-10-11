//! The processes `shards` starts but is not: the VM process (`shards-vm`), one per
//! microVM, and the network process (`shards-net`), one per networked microVM (D36).
//!
//! `shards` is the one binary there is to install. These two are carried inside it
//! (build.rs) and written out the first time a build of it needs them, into a directory
//! of this user's cache named by their digests, from which every later process of that
//! build starts them. They stay processes of their own binaries for two measured reasons:
//! - every VM maps the binary it runs, and a VM running all of `shards` would hold 1.2
//!   MiB more than one running its own (PM M34);
//! - on macOS the VM process runs in App Sandbox, whose entitlements are its binary's
//!   signature, which the daemon and the command must not share (D30).

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

mod carried {
    include!(concat!(env!("OUT_DIR"), "/helpers.rs"));
}

/// The VM process's binary, written out if this build's is not yet; or the one
/// `SHARDS_VM_BINARY` names, an absolute path, for a VMM being worked on or a tool
/// wrapped around the VM process (tests gate VMs' starts with one).
pub fn vm() -> Result<PathBuf, String> {
    if let Some(given) = std::env::var_os("SHARDS_VM_BINARY").filter(|v| !v.is_empty()) {
        let given = PathBuf::from(given);
        if !given.is_absolute() {
            return Err(format!(
                "SHARDS_VM_BINARY: {} is not an absolute path",
                given.display()
            ));
        }
        return Ok(given);
    }
    Ok(dir()?.join(format!("shards-vm{}", std::env::consts::EXE_SUFFIX)))
}

/// The network process's binary, written out if this build's is not yet. Only Unix
/// hosts network their VMs yet (D31).
#[cfg(unix)]
pub fn net() -> Result<PathBuf, String> {
    Ok(dir()?.join(format!("shards-net{}", std::env::consts::EXE_SUFFIX)))
}

/// What this build carries: each helper's name, bytes and SHA-256.
fn carried() -> [(&'static str, &'static [u8], &'static str); 2] {
    [
        ("shards-vm", carried::SHARDS_VM, carried::SHARDS_VM_SHA256),
        ("shards-net", carried::SHARDS_NET, carried::SHARDS_NET_SHA256),
    ]
}

/// This build's helpers' directory: written out by the first process that needs it.
fn dir() -> Result<PathBuf, String> {
    static DIR: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    DIR.get_or_init(|| {
        if !carried::PRESENT {
            return Err(
                "this shards was built without its VM and network processes (SHARDS_HELPERS=skip)".into(),
            );
        }
        let root = cache()?.join("helpers");
        let id: String = carried()
            .iter()
            .flat_map(|(_, _, sha)| sha.get(..12).unwrap_or(sha).chars())
            .collect();
        let dir = root.join(&id);
        if complete(&dir) {
            return Ok(dir);
        }
        write_out(&root, &id)?;
        Ok(dir)
    })
    .clone()
}

/// Whether `dir` holds every helper at the size it was written: what the first process
/// of a build wrote, all of it, since a directory appears only once its files are synced.
fn complete(dir: &Path) -> bool {
    carried().iter().all(|(name, bytes, _)| {
        fs::metadata(dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
            .is_ok_and(|m| m.len() == bytes.len() as u64)
    })
}

/// Writes the helpers into `root/id`: into a directory of this process's first, each file
/// synced, then moved into place whole. Where another process placed them meanwhile, its
/// copy serves; a directory left incomplete (by a crash after a partial move, which a
/// rename cannot leave, or by hand) is replaced.
fn write_out(root: &Path, id: &str) -> Result<(), String> {
    let at = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    private_dir(root)?;
    let dir = root.join(id);
    let temp = root.join(format!(".{id}.{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp);
    private_dir(&temp)?;
    for (name, bytes, _) in carried() {
        let path = temp.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        let mut file = executable(&path).map_err(|e| at(&path, e))?;
        file.write_all(bytes).map_err(|e| at(&path, e))?;
        file.sync_all().map_err(|e| at(&path, e))?;
    }
    match fs::rename(&temp, &dir) {
        Ok(()) => Ok(()),
        Err(_) if complete(&dir) => {
            let _ = fs::remove_dir_all(&temp);
            Ok(())
        }
        Err(_) => {
            // There, but not whole: replaced.
            let _ = fs::remove_dir_all(&dir);
            let moved = fs::rename(&temp, &dir).map_err(|e| at(&dir, e));
            if moved.is_err() {
                let _ = fs::remove_dir_all(&temp);
            }
            moved
        }
    }
}

/// A new file at `path`, only this user's to read, write and run.
fn executable(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o700);
    }
    options.open(path)
}

/// `path`, made if it is not there, only this user's.
fn private_dir(path: &Path) -> Result<(), String> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// This user's cache: `~/Library/Caches/shards` on macOS, `$XDG_CACHE_HOME/shards` or
/// `~/.cache/shards` elsewhere on Unix, `%LOCALAPPDATA%\shards` on Windows.
fn cache() -> Result<PathBuf, String> {
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let base = if cfg!(target_os = "macos") {
        var("HOME").map(|h| h.join("Library").join("Caches"))
    } else if cfg!(windows) {
        var("LOCALAPPDATA")
    } else {
        var("XDG_CACHE_HOME")
            .filter(|p| p.is_absolute())
            .or_else(|| var("HOME").map(|h| h.join(".cache")))
    };
    base.map(|b| b.join("shards"))
        .ok_or_else(|| "no cache directory: HOME is not set".to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn helpers_are_written_whole_once_and_found_after() {
        if !carried::PRESENT {
            return;
        }
        let root_dir = shards_testdir::TempDir::new("helpers").unwrap();
        let root = root_dir.join("helpers");
        let _ = fs::remove_dir_all(&root);
        write_out(&root, "test").unwrap();
        let dir = root.join("test");
        assert!(complete(&dir));
        for (name, bytes, _) in carried() {
            assert_eq!(
                fs::read(dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))).unwrap(),
                bytes
            );
        }
        // Again: the copy there serves, and no temporary directory is left.
        write_out(&root, "test").unwrap();
        let left: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, vec![std::ffi::OsString::from("test")]);
        // A copy cut short is replaced.
        let vm = dir.join(format!("shards-vm{}", std::env::consts::EXE_SUFFIX));
        fs::write(&vm, b"cut").unwrap();
        assert!(!complete(&dir));
        write_out(&root, "test").unwrap();
        assert!(complete(&dir));
        let _ = fs::remove_dir_all(&root);
    }
}
