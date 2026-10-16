//! A layer of what changed ([`shards_archive::pack_changes`]) brings a copy of a tree in
//! step when applied over it ([`shards_archive::apply_layer`]): what changed, as it is now,
//! times to the nanosecond; what went, gone; what did not change, untouched.

#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::time::{Duration, SystemTime};

use shards_archive::{UnpackOptions, apply_layer, pack_changes};
use shards_testdir::TempDir;

fn mtime(path: &Path) -> (i64, i64) {
    let m = std::fs::symlink_metadata(path).unwrap();
    (m.mtime(), m.mtime_nsec())
}

fn set_mtime(path: &Path, at: SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(at)
        .unwrap();
}

#[test]
fn a_layer_of_what_changed_brings_a_copy_in_step() {
    let (here_dir, there_dir) = (
        TempDir::new("changes-here").unwrap(),
        TempDir::new("changes-there").unwrap(),
    );
    let (here, there): (&Path, &Path) = (here_dir.as_ref(), there_dir.as_ref());
    // The copy as it was: a file kept, one to change, one to go, a directory to go whole.
    for root in [here, there] {
        std::fs::create_dir_all(root.join("dir/gone")).unwrap();
        std::fs::write(root.join("kept"), "kept").unwrap();
        std::fs::write(root.join("dir/changed"), "old").unwrap();
        std::fs::write(root.join("dir/removed"), "removed").unwrap();
        std::fs::write(root.join("dir/gone/inner"), "inner").unwrap();
    }
    let kept_before = mtime(&there.join("kept"));
    // What changed here: a file's bytes and time, to the nanosecond; a new file, a link of
    // it, and a symlink; two removals.
    let at = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
    std::fs::write(here.join("dir/changed"), "new").unwrap();
    set_mtime(&here.join("dir/changed"), at);
    std::fs::write(here.join("dir/added"), "added").unwrap();
    std::fs::hard_link(here.join("dir/added"), here.join("dir/linked")).unwrap();
    std::os::unix::fs::symlink("added", here.join("dir/pointer")).unwrap();
    std::fs::remove_file(here.join("dir/removed")).unwrap();
    std::fs::remove_dir_all(here.join("dir/gone")).unwrap();
    let changed: Vec<Vec<u8>> = ["dir", "dir/added", "dir/changed", "dir/linked", "dir/pointer"]
        .iter()
        .map(|p| p.as_bytes().to_vec())
        .collect();
    let removed = vec![b"dir/gone".to_vec(), b"dir/removed".to_vec()];
    let layer = pack_changes(here, &changed, &[], &removed, Vec::new()).unwrap();
    let opts = UnpackOptions {
        no_lchown: true,
        ..UnpackOptions::default()
    };
    apply_layer(&layer[..], there, &opts).unwrap();
    assert_eq!(std::fs::read_to_string(there.join("dir/changed")).unwrap(), "new");
    assert_eq!(mtime(&there.join("dir/changed")), (1_700_000_000, 123_456_789));
    assert_eq!(std::fs::read_to_string(there.join("dir/added")).unwrap(), "added");
    let (added, linked) = (
        std::fs::metadata(there.join("dir/added")).unwrap(),
        std::fs::metadata(there.join("dir/linked")).unwrap(),
    );
    assert_eq!(
        (added.dev(), added.ino()),
        (linked.dev(), linked.ino()),
        "linked again"
    );
    assert_eq!(
        std::fs::read_link(there.join("dir/pointer")).unwrap(),
        Path::new("added")
    );
    assert!(!there.join("dir/removed").exists());
    assert!(!there.join("dir/gone").exists());
    // The directory's time is its own here, set once what it holds is in place.
    assert_eq!(mtime(&there.join("dir")), mtime(&here.join("dir")));
    // What did not change is as it was.
    assert_eq!(std::fs::read_to_string(there.join("kept")).unwrap(), "kept");
    assert_eq!(mtime(&there.join("kept")), kept_before);
}

/// A new name of a file the copy holds already, unchanged, goes as a link to it there: the
/// layer carries none of its bytes, and the copy's two names are one file, as here.
#[test]
fn a_new_name_of_a_file_the_copy_holds_is_linked_to_it_there() {
    let (here_dir, there_dir) = (
        TempDir::new("changes-link-here").unwrap(),
        TempDir::new("changes-link-there").unwrap(),
    );
    let (here, there): (&Path, &Path) = (here_dir.as_ref(), there_dir.as_ref());
    let held = vec![7u8; 64 << 10];
    for root in [here, there] {
        std::fs::write(root.join("held"), &held).unwrap();
    }
    std::fs::hard_link(here.join("held"), here.join("named")).unwrap();
    let layer = pack_changes(here, &[b"named".to_vec()], &[b"held".to_vec()], &[], Vec::new()).unwrap();
    assert!(
        layer.len() < 16 << 10,
        "{} bytes: its bytes went again",
        layer.len()
    );
    let opts = UnpackOptions {
        no_lchown: true,
        ..UnpackOptions::default()
    };
    apply_layer(&layer[..], there, &opts).unwrap();
    let (a, b) = (
        std::fs::metadata(there.join("held")).unwrap(),
        std::fs::metadata(there.join("named")).unwrap(),
    );
    assert_eq!((a.dev(), a.ino()), (b.dev(), b.ino()), "not linked there");
    assert_eq!(std::fs::read(there.join("named")).unwrap(), held);
    // Alone, without the copy's file named, it goes whole, as before.
    let whole = pack_changes(here, &[b"named".to_vec()], &[], &[], Vec::new()).unwrap();
    assert!(whole.len() > 64 << 10, "{} bytes", whole.len());
}
