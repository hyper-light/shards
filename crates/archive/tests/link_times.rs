//! A symlink's own time through a layer applied with `link_times`
//! ([`shards_archive::UnpackOptions`]): kept on every host, Windows too, where go-archive
//! keeps none, as a context's copy kept in step with its source needs (synced.rs).

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::Path;
use std::time::{Duration, SystemTime};

use shards_archive::{UnpackOptions, apply_layer, pack_changes};
use shards_testdir::TempDir;

/// `path`'s own modification time, a symlink's not followed.
fn own_mtime(path: &Path) -> SystemTime {
    std::fs::symlink_metadata(path).unwrap().modified().unwrap()
}

/// Sets the symlink `path`'s own modification time to `at`.
#[cfg(unix)]
fn set_own_mtime(path: &Path, at: SystemTime) {
    use std::os::unix::ffi::OsStrExt as _;
    let since = at.duration_since(SystemTime::UNIX_EPOCH).unwrap();
    let time = libc::timespec {
        tv_sec: since.as_secs().try_into().unwrap(),
        tv_nsec: since.subsec_nanos().into(),
    };
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: a NUL-terminated path, and two times for the call to read.
    let set = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            name.as_ptr(),
            [time, time].as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    assert_eq!(set, 0, "{}", std::io::Error::last_os_error());
}

/// Sets the symlink `path`'s own modification time to `at`: the link itself opened.
#[cfg(windows)]
fn set_own_mtime(path: &Path, at: SystemTime) {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_WRITE_ATTRIBUTES,
    };
    std::fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .unwrap()
        .set_modified(at)
        .unwrap();
}

/// Do: a symlink here, its own time set apart from its target's, sent in a layer of what
/// changed and applied there with `link_times`. Expect the link there, its target as
/// here, and its own time here's, to the 100 ns NTFS keeps. Skips where this user may make
/// no symlink (Windows without the privilege, or Developer Mode).
#[test]
fn a_symlinks_own_time_is_kept_with_link_times() {
    let (here_dir, there_dir) = (
        TempDir::new("links-here").unwrap(),
        TempDir::new("links-there").unwrap(),
    );
    let (here, there): (&Path, &Path) = (here_dir.as_ref(), there_dir.as_ref());
    std::fs::write(here.join("target"), "target").unwrap();
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink("target", here.join("pointer"));
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file("target", here.join("pointer"));
    if let Err(e) = made {
        eprintln!("SKIP: no symlink may be made here: {e}");
        return;
    }
    let at = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_700);
    set_own_mtime(&here.join("pointer"), at);
    assert_eq!(own_mtime(&here.join("pointer")), at, "the link's own time here");
    let changed: Vec<Vec<u8>> = ["pointer", "target"]
        .iter()
        .map(|p| p.as_bytes().to_vec())
        .collect();
    let layer = pack_changes(here, &changed, &[], &[], Vec::new()).unwrap();
    let opts = UnpackOptions {
        no_lchown: true,
        link_times: true,
        ..UnpackOptions::default()
    };
    apply_layer(&layer[..], there, &opts).unwrap();
    assert_eq!(
        std::fs::read_link(there.join("pointer")).unwrap(),
        Path::new("target")
    );
    assert_eq!(own_mtime(&there.join("pointer")), at, "the link's own time there");
}
