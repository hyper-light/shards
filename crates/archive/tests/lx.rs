//! A shards client's layer applied over a Linux tree kept on NTFS
//! ([`shards_archive::apply_layer_lx`], D180's copies of a context on a Windows machine):
//! what NTFS's own files hold none of kept as WSL keeps it, read back through shards-ntfs as
//! a walk of the tree reads it, and nothing reached past the tree.

#![cfg(windows)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{Read as _, Write as _};
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::{AsHandle as _, OwnedHandle};
use std::path::Path;

use shards_archive::apply_layer_lx;
use shards_archive::tar::{
    Format, Header, TYPE_CHAR, TYPE_DIR, TYPE_FIFO, TYPE_LINK, TYPE_REG, TYPE_SYMLINK, Time, Writer,
};
use shards_ntfs as ntfs;
use shards_testdir::TempDir;
use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_READ_EA,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
};

/// A time to the nanosecond, as a Linux client's are.
const AT: Time = Time {
    sec: 1_700_000_000,
    nsec: 123_456_789,
};

/// An entry's header, PAX so that its times keep their nanoseconds.
fn header(typeflag: u8, name: &str, mode: i64) -> Header {
    Header {
        typeflag,
        name: name.as_bytes().to_vec(),
        mode,
        mtime: AT,
        format: Format::PAX,
        ..Header::default()
    }
}

/// A layer of `entries`, each a header and its file's bytes.
fn layer(entries: Vec<(Header, &[u8])>) -> Vec<u8> {
    let mut w = Writer::new(Vec::new());
    for (mut h, data) in entries {
        h.size = i64::try_from(data.len()).unwrap();
        w.write_header(&h).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap()
}

fn file(name: &str, mode: i64, data: &'static [u8]) -> (Header, &'static [u8]) {
    (header(TYPE_REG, name, mode), data)
}

/// A link `name` to `to`, of `mode`: a symlink's 0777, a hard link's its file's, as a
/// packer writes it (go-archive's unpack sets a hard link's mode on the file it shares).
fn linked(typeflag: u8, name: &str, mode: i64, to: &str) -> (Header, &'static [u8]) {
    let mut h = header(typeflag, name, mode);
    h.linkname = to.as_bytes().to_vec();
    (h, b"")
}

/// The tree's root, opened as a walk opens it.
fn root(dir: &Path) -> OwnedHandle {
    std::fs::OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | FILE_READ_EA)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)
        .unwrap()
        .into()
}

/// `path` beneath the root, each of its Linux names opened as NTFS keeps it, following no
/// link; none where it is not there.
fn at(root: &OwnedHandle, path: &str) -> Option<OwnedHandle> {
    let mut held = root.try_clone().unwrap();
    for name in path.split('/') {
        let wide = ntfs::nt_name(name.as_bytes()).unwrap();
        match ntfs::open_at(held.as_handle(), &wide, FILE_GENERIC_READ, FILE_OPEN, 0) {
            Ok(h) => held = h,
            Err(ntfs::ENOENT) => return None,
            Err(e) => panic!("{path}: errno {e}"),
        }
    }
    Some(held)
}

fn stat(root: &OwnedHandle, path: &str) -> ntfs::Stat {
    ntfs::stat(at(root, path).unwrap_or_else(|| panic!("no {path}")).as_handle()).unwrap()
}

fn read(root: &OwnedHandle, path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    std::fs::File::from(at(root, path).unwrap())
        .read_to_end(&mut out)
        .unwrap();
    out
}

/// Do: a layer of a Linux client's tree applied over an empty directory: modes NTFS keeps
/// none of (0600, 0750, setuid), a symlink, one of a Windows client's mode (fsutil's 0755,
/// which Linux keeps of no symlink), a hard link, a FIFO and a device, names differing
/// in case alone and one NTFS holds no name with (`a:b`), an extended attribute, and a file
/// in directories the layer only implies. Expect each read back as Linux's: its mode, its
/// link's target, both names one file of two links, the device's numbers, the attribute,
/// the two names two files, `a:b` a name of its own (not a stream of `a`), its time to the
/// 100 ns NTFS keeps, a directory's its own though entries were made in it after; the tree
/// marked a Linux tree. Then a second layer: a whiteout takes one name of the hard link and
/// leaves the other, a directory replaced by a file takes what it held, a file replaced is
/// the new one. Expect a name beneath a symlink, `..` and a hard link out of the tree each
/// refused, and nothing made beside the tree.
#[test]
fn a_linux_tree_keeps_what_ntfs_holds_none_of() {
    let top = TempDir::new("lx").unwrap();
    let top: &Path = top.as_ref();
    let dir = top.join("tree");
    std::fs::create_dir(&dir).unwrap();
    let mut secret = file("secret", 0o600, b"s");
    secret
        .0
        .pax
        .insert(b"SCHILY.xattr.user.Mixed".to_vec(), b"value".to_vec());
    let mut null = header(TYPE_CHAR, "null", 0o666);
    (null.devmajor, null.devminor) = (1, 3);
    let first = layer(vec![
        (header(TYPE_DIR, "bin", 0o750), b""),
        file("bin/run.sh", 0o755, b"#!/bin/sh\n"),
        secret,
        file("a:b", 0o644, b"colon"),
        file("Make", 0o644, b"upper"),
        file("make", 0o644, b"lower"),
        linked(TYPE_SYMLINK, "link", 0o777, "bin/run.sh"),
        linked(TYPE_SYMLINK, "windows-link", 0o755, "secret"),
        linked(TYPE_LINK, "again", 0o600, "secret"),
        (header(TYPE_FIFO, "pipe", 0o644), b""),
        (null, b""),
        file("deep/er/set", 0o4755, b"x"),
    ]);
    apply_layer_lx(&first[..], &dir).unwrap();
    let r = root(&dir);
    assert!(ntfs::is_linux_tree(r.as_handle()).unwrap());
    for (path, mode) in [
        ("bin", 0o040_750),
        ("bin/run.sh", 0o100_755),
        ("secret", 0o100_600),
        ("again", 0o100_600),
        ("a:b", 0o100_644),
        ("Make", 0o100_644),
        ("make", 0o100_644),
        ("link", 0o120_777),
        ("windows-link", 0o120_755),
        ("pipe", 0o010_644),
        ("null", 0o020_666),
        ("deep", 0o040_755),
        ("deep/er", 0o040_755),
        ("deep/er/set", 0o104_755),
    ] {
        assert_eq!(stat(&r, path).mode, mode, "{path}: {:o}", stat(&r, path).mode);
    }
    assert_eq!(
        ntfs::readlink(at(&r, "link").unwrap().as_handle()).unwrap(),
        b"bin/run.sh"
    );
    let (one, other) = (stat(&r, "secret"), stat(&r, "again"));
    assert_eq!((one.ino, one.nlink), (other.ino, 2));
    // Linux's new_encode_dev of 1:3.
    assert_eq!(stat(&r, "null").rdev, (1 << 8) | 3);
    let eas = ntfs::eas(at(&r, "secret").unwrap().as_handle()).unwrap();
    assert!(
        eas.iter().any(|(name, value)| {
            ntfs::ea_xattr(name).as_deref() == Some(b"user.Mixed".as_slice())
                && value.strip_prefix(ntfs::XATTR_KEPT) == Some(b"value".as_slice())
        }),
        "{eas:?}"
    );
    assert_eq!(
        (read(&r, "Make"), read(&r, "make"), read(&r, "a:b")),
        (b"upper".to_vec(), b"lower".to_vec(), b"colon".to_vec())
    );
    assert!(at(&r, "a").is_none(), "a:b is a name of its own, no stream of a");
    for path in ["bin/run.sh", "bin", "deep/er/set", "link"] {
        assert_eq!(stat(&r, path).mtime, (1_700_000_000, 123_456_700), "{path}");
    }

    let second = layer(vec![
        file(".wh.secret", 0o644, b""),
        file("bin", 0o644, b"a file now"),
        file("make", 0o600, b"again"),
    ]);
    apply_layer_lx(&second[..], &dir).unwrap();
    assert!(at(&r, "secret").is_none());
    assert_eq!((stat(&r, "again").nlink, read(&r, "again")), (1, b"s".to_vec()));
    assert_eq!(
        (stat(&r, "bin").mode, read(&r, "bin")),
        (0o100_644, b"a file now".to_vec())
    );
    assert_eq!(
        (stat(&r, "make").mode, read(&r, "make")),
        (0o100_600, b"again".to_vec())
    );
    assert_eq!(read(&r, "Make"), b"upper");

    for (bad, says) in [
        (file("link/x", 0o644, b"x"), "beneath"),
        (file("../x", 0o644, b"x"), "invalid entry name"),
        (linked(TYPE_LINK, "h", 0o644, "../x"), "invalid hardlink target"),
    ] {
        let name = String::from_utf8_lossy(&bad.0.name).into_owned();
        let e = apply_layer_lx(&layer(vec![bad])[..], &dir)
            .unwrap_err()
            .to_string();
        assert!(e.contains(says), "{name}: {e}");
    }
    let beside: Vec<_> = std::fs::read_dir(top)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(beside, ["tree"]);
}
