//! What a guest keeps of its console to quote as it fails, read from the records shards keeps
//! for its kernel (D12): `<home>/said/<pin>`, each line a report of a guest of that kernel's,
//! whole or run out (`shards_abi::bounds::ConsoleRecords`). The client passes it as
//! `--console-said N` and keeps the tail its report fits in; the `shards` it starts records how
//! the report came out (shards' said.rs). Reading them fails no boot: where they cannot be
//! read, a guest keeps what a kernel with none keeps.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use sha2::{Digest as _, Sha256};
use shards_abi::bounds::{ConsoleRecords, console_said};

/// How many of its console's last bytes a guest of `kernel` keeps: what its records in the
/// home of the `shards` this process starts say ([`console_said`]), else every kernel's.
pub(crate) fn kept(kernel: &Path) -> usize {
    let Some(home) = home() else {
        return console_said(ConsoleRecords::default(), ConsoleRecords::default());
    };
    kept_at(&home, kernel)
}

pub(crate) fn kept_at(home: &Path, kernel: &Path) -> usize {
    let dir = home.join("said");
    let own = pin(home, kernel).map_or_else(ConsoleRecords::default, |pin| read(&dir.join(pin)));
    console_said(own, every(&dir))
}

/// shards' home, as the `shards` this process starts finds it in the environment it is given,
/// this process's (shards-ipc `home`): `SHARDS_HOME`; else the platform's data directory's
/// `shards`.
fn home() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    if let Some(home) = var("SHARDS_HOME") {
        return std::path::absolute(PathBuf::from(home)).ok();
    }
    let data = if cfg!(windows) {
        var("LOCALAPPDATA").map(PathBuf::from)
    } else {
        let home = var("HOME").map(PathBuf::from);
        if cfg!(target_os = "macos") {
            home.map(|h| h.join("Library").join("Application Support"))
        } else {
            var("XDG_DATA_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .or_else(|| home.map(|h| h.join(".local").join("share")))
        }
    };
    std::path::absolute(data?.join("shards")).ok()
}

/// The records in `file`, none where it cannot be read.
fn read(file: &Path) -> ConsoleRecords {
    std::fs::read(file).map_or_else(|_| ConsoleRecords::default(), |b| ConsoleRecords::read(&b))
}

/// Every kernel's records in `dir`: each file named by a pin, and restores' (`unpinned`).
fn every(dir: &Path) -> ConsoleRecords {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return ConsoleRecords::default();
    };
    entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n == "unpinned" || is_digest(n))
        })
        .fold(ConsoleRecords::default(), |all, e| all.and(read(&e.path())))
}

fn is_digest(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `kernel`'s pin, the SHA-256 of its bytes: named by it in the home's guest store
/// (`guest/sha256-<hex>`); any other hashed, once in this process for each file as it is.
fn pin(home: &Path, kernel: &Path) -> Option<String> {
    /// Each file hashed: its path, length and time of change, and its digest.
    type Hashed = (PathBuf, u64, Option<std::time::SystemTime>, String);
    static HASHED: Mutex<Vec<Hashed>> = Mutex::new(Vec::new());
    let kernel = std::path::absolute(kernel).ok()?;
    if kernel.parent() == Some(home.join("guest").as_path())
        && let Some(hex) = kernel
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("sha256-"))
            .filter(|h| is_digest(h))
    {
        return Some(hex.to_string());
    }
    let meta = std::fs::metadata(&kernel).ok()?;
    let (len, modified) = (meta.len(), meta.modified().ok());
    let mut hashed = HASHED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((.., digest)) = hashed
        .iter()
        .find(|(p, l, m, _)| *p == kernel && *l == len && *m == modified)
    {
        return Some(digest.clone());
    }
    let digest = digest(&kernel)?;
    hashed.retain(|(p, ..)| *p != kernel);
    hashed.push((kernel, len, modified, digest.clone()));
    Some(digest)
}

/// The SHA-256 of `path`'s bytes, in hex, read through std's own buffer.
fn digest(path: &Path) -> Option<String> {
    use std::io::BufRead as _;
    let mut file = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let mut h = Sha256::new();
    loop {
        let n = match file.fill_buf() {
            Ok([]) => break,
            Ok(buf) => {
                h.update(buf);
                buf.len()
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        file.consume(n);
    }
    Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A kernel's pin is its bytes' SHA-256, named by the store or hashed, and hashed again
    /// once its file changes.
    #[test]
    fn a_kernels_pin_is_its_bytes_digest() {
        let dir = shards_testdir::TempDir::new("api-pin").unwrap();
        let home = dir.path();
        let stored = home.join("guest").join(format!("sha256-{}", "a".repeat(64)));
        std::fs::create_dir_all(stored.parent().unwrap()).unwrap();
        std::fs::write(&stored, b"not hashed").unwrap();
        assert_eq!(pin(home, &stored).unwrap(), "a".repeat(64));
        let kernel = home.join("Image");
        std::fs::write(&kernel, b"one").unwrap();
        let one: String = Sha256::digest(b"one")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(pin(home, &kernel).unwrap(), one);
        std::fs::write(&kernel, b"three").unwrap();
        let three: String = Sha256::digest(b"three")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(pin(home, &kernel).unwrap(), three);
        assert_eq!(pin(home, &home.join("absent")), None);
    }
}
