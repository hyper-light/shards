//! EROFS images written by shards-image, mounted by a real guest kernel over virtio-pmem
//! with DAX, and checked from inside against the manifest each image carries.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{TempDir, cannot_run_vms, kernel, test_guest, vm_run};
use shards_image::erofs::{self, DataRef, Dir, Kind, Meta, Node, NodeId, Source, Tree};

const TIMEOUT: Duration = Duration::from_secs(60);
/// The source id that stands for the manifest; every other id is a pattern salt.
const MANIFEST: u32 = u32::MAX;

/// Pattern bytes computed on demand, and the manifest.
struct Contents {
    manifest: Vec<u8>,
}

impl Source for Contents {
    fn read_at(&mut self, data: DataRef, at: u64, buf: &mut [u8]) -> io::Result<()> {
        if data.source == MANIFEST {
            let start = (data.offset + at) as usize;
            buf.copy_from_slice(&self.manifest[start..start + buf.len()]);
        } else {
            shards_testguest::fill(u64::from(data.source), data.offset + at, buf);
        }
        Ok(())
    }
}

fn meta(mode: u16, uid: u32, gid: u32) -> Meta {
    Meta {
        mode,
        uid,
        gid,
        mtime: 1_700_000_000,
        mtime_nsec: 0,
        xattrs: BTreeMap::new(),
    }
}

/// A tree and the manifest lines that describe it.
struct Builder {
    tree: Tree,
    lines: String,
}

impl Builder {
    fn dir(&mut self, parent: NodeId, path: &str, mode: u16) -> NodeId {
        let name = path.rsplit('/').next().unwrap();
        let id = self
            .tree
            .insert(
                parent,
                name.as_bytes(),
                Node {
                    kind: Kind::Dir(Dir::default()),
                    meta: meta(mode, 0, 0),
                },
            )
            .unwrap();
        writeln!(self.lines, "d {path} {mode:o} 0 0").unwrap();
        id
    }

    fn file(
        &mut self,
        parent: NodeId,
        path: &str,
        mode: u16,
        owner: (u32, u32),
        size: u64,
        salt: u32,
    ) -> NodeId {
        let name = path.rsplit('/').next().unwrap();
        let data = DataRef {
            source: salt,
            offset: 0,
        };
        let id = self
            .tree
            .insert(
                parent,
                name.as_bytes(),
                Node {
                    kind: Kind::File { size, data },
                    meta: meta(mode, owner.0, owner.1),
                },
            )
            .unwrap();
        writeln!(
            self.lines,
            "f {path} {mode:o} {} {} {size} {salt}",
            owner.0, owner.1
        )
        .unwrap();
        id
    }

    fn special(&mut self, parent: NodeId, path: &str, kind: Kind, line: String) {
        let name = path.rsplit('/').next().unwrap();
        self.tree
            .insert(
                parent,
                name.as_bytes(),
                Node {
                    kind,
                    meta: meta(0o600, 0, 0),
                },
            )
            .unwrap();
        writeln!(self.lines, "{line}").unwrap();
    }

    fn xattr(&mut self, id: NodeId, path: &str, name: &str, value: &[u8]) {
        let node = self.tree.node_mut(id).unwrap();
        node.meta.xattrs.insert(name.as_bytes().to_vec(), value.to_vec());
        let hex: String = value.iter().map(|b| format!("{b:02x}")).collect();
        writeln!(self.lines, "x {path} {name} {hex}").unwrap();
    }
}

/// Files around block boundaries, owners that need extended inodes, set-id bits, a
/// directory of several blocks with names that sort before `.`, links, devices and xattrs.
/// Returns the image and how many manifest lines describe it.
fn build(dir: &Path) -> (PathBuf, usize) {
    let mut b = Builder {
        tree: Tree::new(meta(0o755, 0, 0)),
        lines: String::new(),
    };
    let root = Tree::ROOT;
    let etc = b.dir(root, "/etc", 0o755);
    let hostname = b.file(etc, "/etc/hostname", 0o644, (0, 0), 12, 1);
    b.xattr(hostname, "/etc/hostname", "user.test", b"hello");
    let bin = b.dir(root, "/bin", 0o755);
    let big = b.file(bin, "/bin/big", 0o755, (0, 0), 3 * (1 << 20) + 7, 2);
    b.xattr(big, "/bin/big", "trusted.overlay.opaque", b"y");
    b.tree.link(bin, b"big2", big).unwrap();
    writeln!(b.lines, "h /bin/big2 /bin/big").unwrap();
    b.special(
        bin,
        "/bin/sh",
        Kind::Symlink(b"busybox".to_vec()),
        "l /bin/sh busybox".into(),
    );
    let long = "a".repeat(3000);
    b.special(
        bin,
        "/bin/long",
        Kind::Symlink(long.clone().into_bytes()),
        format!("l /bin/long {long}"),
    );
    let usr = b.dir(root, "/usr", 0o755);
    let lib = b.dir(usr, "/usr/lib", 0o755);
    b.file(lib, "/usr/lib/empty", 0o644, (0, 0), 0, 3);
    b.file(lib, "/usr/lib/exact", 0o644, (0, 0), 8192, 4);
    b.file(lib, "/usr/lib/owned", 0o640, (1000, 1000), 100, 5);
    b.file(lib, "/usr/lib/wide-owner", 0o600, (100_000, 100_001), 5000, 6);
    let sbin = b.dir(usr, "/usr/sbin", 0o755);
    let suid = b.file(sbin, "/usr/sbin/suid", 0o4755, (0, 0), 64, 7);
    b.xattr(
        suid,
        "/usr/sbin/suid",
        "security.capability",
        &[1, 0, 0, 2, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    );
    let data = b.dir(root, "/data", 0o755);
    let many = b.dir(data, "/data/many", 0o755);
    for i in 0..300u32 {
        b.file(
            many,
            &format!("/data/many/file-{i:03}"),
            0o644,
            (0, 0),
            u64::from(i % 50),
            100 + i,
        );
    }
    b.file(many, "/data/many/-first", 0o644, (0, 0), 1, 400);
    b.file(many, "/data/many/!bang", 0o644, (0, 0), 1, 401);
    let dev = b.dir(root, "/dev", 0o755);
    b.special(
        dev,
        "/dev/null",
        Kind::CharDevice { major: 1, minor: 3 },
        "c /dev/null 1 3".into(),
    );
    b.special(
        dev,
        "/dev/sda",
        Kind::BlockDevice { major: 8, minor: 0 },
        "b /dev/sda 8 0".into(),
    );
    let run = b.dir(root, "/run", 0o755);
    b.special(run, "/run/fifo", Kind::Fifo, "p /run/fifo".into());
    b.special(run, "/run/sock", Kind::Socket, "s /run/sock".into());

    let entries = b.lines.lines().count();
    let manifest = b.lines.into_bytes();
    let data = DataRef {
        source: MANIFEST,
        offset: 0,
    };
    b.tree
        .insert(
            root,
            b"MANIFEST",
            Node {
                kind: Kind::File {
                    size: manifest.len() as u64,
                    data,
                },
                meta: meta(0o444, 0, 0),
            },
        )
        .unwrap();
    let path = dir.join("root.erofs");
    let mut out = io::BufWriter::new(std::fs::File::create(&path).unwrap());
    erofs::write(&b.tree, &mut Contents { manifest }, &mut out).unwrap();
    io::Write::flush(&mut out).unwrap();
    (path, entries)
}

#[test]
fn a_guest_mounts_our_erofs_images_with_dax_and_finds_every_entry() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("erofs");
    let (image, entries) = build(&dir);
    let r = vm_run(
        &[
            "--kernel".as_ref(),
            kernel().as_os_str(),
            "--init".as_ref(),
            test_guest().as_os_str(),
            "--memory".as_ref(),
            "256".as_ref(),
            "--pmem".as_ref(),
            image.as_os_str(),
            "--cmdline".as_ref(),
            "console=ttyS0 quiet shards_test=erofs".as_ref(),
        ],
        TIMEOUT,
    );
    assert!(r.stdout.contains("SHARDS-TEST PASS"), "{r}");
    assert!(r.stdout.contains(&format!("checked {entries} entries")), "{r}");
}
