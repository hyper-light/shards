use std::collections::BTreeMap;

use super::*;

/// An image in memory: its written blocks by number.
#[derive(Default)]
struct Image(BTreeMap<u64, Vec<u8>>);

impl Sink for Image {
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        if !offset.is_multiple_of(BLOCK) {
            return Err(io::Error::other(format!("a write at {offset}, inside a block")));
        }
        for (i, chunk) in bytes.chunks(BLOCK as usize).enumerate() {
            let block = offset / BLOCK + i as u64;
            let mut whole = chunk.to_vec();
            whole.resize(BLOCK as usize, 0);
            if self.0.insert(block, whole).is_some() {
                return Err(io::Error::other(format!("block {block} written twice")));
            }
        }
        Ok(())
    }
}

impl Image {
    fn block(&self, n: u64) -> &[u8] {
        self.0.get(&n).map_or(&[0; BLOCK as usize][..], |b| &b[..])
    }

    fn u16(&self, block: u64, at: usize) -> u16 {
        u16::from_le_bytes(self.block(block)[at..at + 2].try_into().unwrap())
    }

    fn u32(&self, block: u64, at: usize) -> u32 {
        u32::from_le_bytes(self.block(block)[at..at + 4].try_into().unwrap())
    }

    /// SHA-256 over every block not all zeros, its number then its bytes, in order: what
    /// scripts/ext4/generate computes of the file it hands e2fsck.
    fn digest(&self) -> String {
        use sha2::Digest as _;
        let mut h = sha2::Sha256::new();
        for (n, b) in &self.0 {
            if b.iter().any(|&x| x != 0) {
                h.update(n.to_le_bytes());
                h.update(b);
            }
        }
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}

fn options(bytes: u64) -> Options {
    Options {
        bytes,
        uuid: *b"shards-ext4-test",
        hash_seed: *b"shards-hashseed!",
        time: 1_792_486_936,
    }
}

fn made(bytes: u64) -> (Made, Image) {
    let mut image = Image::default();
    let made = format(&mut image, &options(bytes)).unwrap();
    (made, image)
}

#[test]
fn crc32c_is_castagnolis() {
    // RFC 3720 B.4's check value, and its 32 bytes of zeros.
    assert_eq!(!crc32c(!0, b"123456789"), 0xE306_9283);
    assert_eq!(!crc32c(!0, &[0; 32]), 0x8A91_36AA);
}

#[test]
fn a_geometry_is_mke2fss_for_its_size() {
    // mke2fs 1.47.0's for 257536 blocks (testdata, the image scripts/ext4/generate made):
    // 8 groups, 8048 inodes each, a 4096-block journal.
    let g = Geometry::of(257_536 * BLOCK).unwrap();
    assert_eq!(
        (g.groups, g.ipg, g.itb, g.gdt, g.journal),
        (8, 8048, 503, 1, 4096)
    );
    // Its tables as mke2fs lays out a flex group, past the superblock and descriptors
    // (mke2fs's start after its 125 reserved descriptor blocks).
    assert_eq!(
        (g.block_bitmap(0), g.inode_bitmap(0), g.inode_table(0)),
        (2, 10, 18)
    );
    assert_eq!(
        (g.block_bitmap(1), g.inode_bitmap(1), g.inode_table(1)),
        (3, 11, 18 + 503)
    );
    assert_eq!(g.root(), 18 + 8 * 503);
    assert_eq!(g.journal_start().unwrap(), 2 * GROUP);
}

#[test]
fn a_last_group_too_small_for_its_tables_is_left_out() {
    // 2 GiB and 1 MiB: group 16 would lead a flex group of one, its tables more than its
    // 256 blocks.
    let g = Geometry::of((2 << 30) + (1 << 20)).unwrap();
    assert_eq!((g.blocks, g.groups), (16 * GROUP, 16));
    // A small last group without tables of its own stays.
    let g = Geometry::of((128 << 20) + (1 << 20)).unwrap();
    assert_eq!((g.blocks, g.groups), (GROUP + 256, 2));
}

#[test]
fn sizes_out_of_reach_are_refused() {
    assert!(Geometry::of(LEAST_BLOCKS * BLOCK - BLOCK).is_err());
    assert!(Geometry::of(LEAST_BLOCKS * BLOCK + 1).is_err());
    assert!(Geometry::of((MOST_BLOCKS + 1) * BLOCK).is_err());
    assert!(Geometry::of(LEAST_BLOCKS * BLOCK).is_ok());
}

#[test]
fn superblock_copies_are_where_sparse_super_puts_them() {
    let g = Geometry::of(1 << 40).unwrap();
    let copies: Vec<u64> = (0..g.groups).filter(|&n| g.has_super(n)).take(14).collect();
    assert_eq!(copies, [0, 1, 3, 5, 7, 9, 25, 27, 49, 81, 125, 243, 343, 625]);
}

#[test]
fn no_two_things_share_a_block_and_every_count_adds_up() {
    for bytes in [
        8 << 20,
        64 << 20,
        (128 << 20) + (1 << 20),
        1 << 30,
        10 << 30,
        17 << 30,
        64 << 30,
    ] {
        let g = Geometry::of(bytes).unwrap();
        let mut owner: BTreeMap<u64, String> = BTreeMap::new();
        let mut claim = |range: std::ops::Range<u64>, what: String| {
            assert!(
                range.end <= g.blocks,
                "{bytes}: {what} {range:?} past {}",
                g.blocks
            );
            for b in range {
                if let Some(other) = owner.insert(b, what.clone()) {
                    panic!("{bytes}: block {b} is {other}'s and {what}'s");
                }
            }
        };
        for n in 0..g.groups {
            if g.has_super(n) {
                claim(
                    g.first(n)..g.first(n) + 1 + g.gdt,
                    format!("group {n}'s superblock"),
                );
            }
            claim(
                g.block_bitmap(n)..g.block_bitmap(n) + 1,
                format!("group {n}'s block bitmap"),
            );
            claim(
                g.inode_bitmap(n)..g.inode_bitmap(n) + 1,
                format!("group {n}'s inode bitmap"),
            );
            claim(
                g.inode_table(n)..g.inode_table(n) + g.itb,
                format!("group {n}'s inode table"),
            );
        }
        claim(g.root()..g.root() + 1, "the root".into());
        claim(
            g.lost_found()..g.lost_found() + LOST_FOUND_BLOCKS,
            "lost+found".into(),
        );
        let j = g.journal_start().unwrap();
        claim(j..j + g.journal, "the journal".into());
        // The descriptors' free blocks are what nothing claimed, group by group.
        let descs = g.descriptors(0, 11).unwrap();
        for (n, d) in descs.iter().enumerate() {
            let first = g.first(n as u64);
            let taken = owner.range(first..first + g.size(n as u64)).count() as u64;
            assert_eq!(d.free_blocks, g.size(n as u64) - taken, "{bytes}: group {n}");
        }
    }
}

#[test]
fn every_checksum_is_the_kernels() {
    let (made, image) = made(1 << 30);
    assert_eq!(made.groups, 8);
    let seed = crc32c(!0, &options(0).uuid);
    // The superblock, at byte 1024.
    let sb = &image.block(0)[1024..2048];
    assert_eq!(
        u32::from_le_bytes(sb[0x3FC..].try_into().unwrap()),
        crc32c(!0, &sb[..0x3FC])
    );
    // Each descriptor, and the bitmaps they say are written.
    let g = Geometry::of(1 << 30).unwrap();
    for n in 0..g.groups {
        let d = &image.block(1)[(n * DESC_SIZE) as usize..((n + 1) * DESC_SIZE) as usize];
        assert_eq!(
            group_desc_csum(seed, n, d).unwrap(),
            u16::from_le_bytes([d[0x1E], d[0x1F]])
        );
        let flags = u16::from_le_bytes([d[0x12], d[0x13]]);
        if flags & bg::BLOCK_UNINIT == 0 {
            let bits = image.block(g.block_bitmap(n));
            let csum = crc32c(seed, bits);
            assert_eq!(u16::from_le_bytes([d[0x18], d[0x19]]), csum as u16, "group {n}");
            assert_eq!(
                u16::from_le_bytes([d[0x38], d[0x39]]),
                (csum >> 16) as u16,
                "group {n}"
            );
        }
        if flags & bg::INODE_UNINIT == 0 {
            let bits = &image.block(g.inode_bitmap(n))[..(g.ipg / 8) as usize];
            let csum = crc32c(seed, bits);
            assert_eq!(u16::from_le_bytes([d[0x1A], d[0x1B]]), csum as u16, "group {n}");
        }
    }
    // Each inode in use.
    let table = image.block(g.inode_table(0));
    for ino in 1..=11u32 {
        let raw = &table[(ino as usize - 1) * 256..ino as usize * 256];
        let csum = inode_csum(seed, ino, raw).unwrap();
        assert_eq!(
            u16::from_le_bytes([raw[0x7C], raw[0x7D]]),
            csum as u16,
            "inode {ino}"
        );
    }
    // Each directory block's tail.
    for (block, dir) in [(g.root(), 2u32), (g.lost_found(), 11), (g.lost_found() + 3, 11)] {
        let b = image.block(block);
        let tail = BLOCK as usize - 12;
        assert_eq!(&b[tail..tail + 8], &[0, 0, 0, 0, 12, 0, 0, 0xDE]);
        assert_eq!(
            image.u32(block, tail + 8),
            crc32c(inode_seed(seed, dir, 0), &b[..tail])
        );
    }
}

#[test]
fn the_root_holds_lost_found_and_the_journal_is_clean() {
    let (_, image) = made(10 << 30);
    let g = Geometry::of(10 << 30).unwrap();
    let root = image.block(g.root());
    let entries: Vec<(u32, &[u8])> = {
        let mut at = 0;
        let mut out = Vec::new();
        while at < BLOCK as usize - 12 {
            let ino = u32::from_le_bytes(root[at..at + 4].try_into().unwrap());
            let rec = u16::from_le_bytes([root[at + 4], root[at + 5]]) as usize;
            let len = root[at + 6] as usize;
            out.push((ino, &root[at + 8..at + 8 + len]));
            at += rec;
        }
        assert_eq!(at, BLOCK as usize - 12, "the entries reach the tail");
        out
    };
    assert_eq!(entries, [(2, &b"."[..]), (2, b".."), (11, b"lost+found")]);
    let j = image.block(g.journal_start().unwrap());
    assert_eq!(&j[..4], &JBD2_MAGIC.to_be_bytes());
    assert_eq!(
        u32::from_be_bytes(j[0x1C..0x20].try_into().unwrap()),
        0,
        "s_start: nothing to replay"
    );
    assert_eq!(
        u32::from_be_bytes(j[0x10..0x14].try_into().unwrap()) as u64,
        g.journal
    );
    // The superblock keeps the journal inode's extent.
    let sb = &image.block(0)[1024..];
    assert_eq!(&sb[0x10C..0x10C + 60], &g.journal_inode_blocks().unwrap());
    assert_eq!(image.u16(0, 1024 + 0x38), MAGIC);
}

#[test]
fn what_is_written_is_metadata_alone() {
    // 10 GiB: its superblock and descriptor copies (9 groups of 3 blocks), 7 block bitmaps,
    // an inode bitmap, an inode table block, 5 directory blocks and the journal's
    // superblock.
    let (made, image) = made(10 << 30);
    assert_eq!(made.groups, 80);
    assert_eq!(made.written, image.0.len() as u64);
    assert!(made.written < 64, "{} blocks written", made.written);
}

/// Each size's image, as scripts/ext4/generate handed it to e2fsck 1.47.0, which found
/// nothing to fix in any (`e2fsck -fn`): the images made now are those.
#[test]
fn e2fsck_finds_nothing_to_fix() {
    #[derive(serde::Deserialize)]
    struct Case {
        bytes: u64,
        digest: String,
        exit: i32,
        output: String,
    }
    let cases: Vec<Case> = serde_json::from_str(include_str!("../testdata/e2fsck.json")).unwrap();
    assert!(cases.len() >= 6);
    for c in cases {
        assert_eq!(c.exit, 0, "e2fsck of {} bytes:\n{}", c.bytes, c.output);
        assert!(!c.output.contains("Fix?"), "{}", c.output);
        let (_, image) = made(c.bytes);
        assert_eq!(
            image.digest(),
            c.digest,
            "{} bytes: not the image e2fsck checked",
            c.bytes
        );
    }
}

/// mke2fs 1.47.0's own structures (scripts/ext4/generate): each checksum, as computed
/// here, is the one mke2fs stored.
#[test]
fn checksums_agree_with_mke2fss() {
    #[derive(serde::Deserialize)]
    struct Mke2fs {
        superblock: String,
        descriptors: String,
        inodes: String,
        root: String,
        root_block: u64,
    }
    let m: Mke2fs = serde_json::from_str(include_str!("../testdata/mke2fs.json")).unwrap();
    let hex = |s: &str| {
        s.as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect::<Vec<u8>>()
    };
    let sb = hex(&m.superblock);
    assert_eq!(
        u32::from_le_bytes(sb[0x3FC..].try_into().unwrap()),
        crc32c(!0, &sb[..0x3FC])
    );
    let seed = crc32c(!0, &sb[0x68..0x78]);
    // With metadata_csum_seed, mke2fs stores the seed it computed from the UUID.
    assert_eq!(u32::from_le_bytes(sb[0x270..0x274].try_into().unwrap()), seed);
    let descs = hex(&m.descriptors);
    for (n, d) in descs.chunks(64).enumerate() {
        assert_eq!(
            group_desc_csum(seed, n as u64, d).unwrap(),
            u16::from_le_bytes([d[0x1E], d[0x1F]]),
            "group {n}"
        );
    }
    let inodes = hex(&m.inodes);
    for (i, raw) in inodes.chunks(256).enumerate() {
        let ino = i as u32 + 1;
        let csum = inode_csum(seed, ino, raw).unwrap();
        assert_eq!(
            u16::from_le_bytes([raw[0x7C], raw[0x7D]]),
            csum as u16,
            "inode {ino}"
        );
        if u16::from_le_bytes([raw[0x80], raw[0x81]]) >= 4 {
            assert_eq!(
                u16::from_le_bytes([raw[0x82], raw[0x83]]),
                (csum >> 16) as u16,
                "inode {ino}"
            );
        }
    }
    let root = hex(&m.root);
    assert!(m.root_block > 0);
    let tail = BLOCK as usize - 12;
    assert_eq!(
        u32::from_le_bytes(root[tail + 8..].try_into().unwrap()),
        crc32c(inode_seed(seed, 2, 0), &root[..tail])
    );
}

#[test]
fn a_file_system_is_sized_to_hold_what_is_asked() {
    let mib = 1u64 << 20;
    for free in [mib, 100 * mib, 1 << 30, 10 << 30, (10 << 30) + 12345] {
        let bytes = size_for(free, 2 * mib).unwrap();
        assert_eq!(bytes % (2 * mib), 0);
        let (made, _) = made(bytes);
        assert_eq!(made.blocks * BLOCK, bytes, "{free}: a group left out");
        // What its files may take: its free blocks, less those the kernel keeps back.
        let held = |g: &Geometry| {
            let free: u64 = g.descriptors(0, 11).unwrap().iter().map(|d| d.free_blocks).sum();
            (free - (g.blocks / 50).min(4096)) * BLOCK
        };
        let g = Geometry::of(bytes).unwrap();
        assert!(held(&g) >= free, "{free}: {}", held(&g));
        // And no more than a step past what the least would be.
        let less = Geometry::of(bytes - 2 * mib).map(|g| held(&g));
        assert!(
            less.is_err() || less.unwrap() < free,
            "{free}: {bytes} is more than it needs"
        );
    }
    assert!(size_for(1, 0).is_err() && size_for(1, 1000).is_err());
}

/// A file system of `bytes` as [`format`] made it, its blocks read as a file's.
struct Formatted(Image, u64);

impl read::ReadAt for Formatted {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        if offset + buf.len() as u64 > self.1 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        for (i, b) in buf.iter_mut().enumerate() {
            let at = offset + i as u64;
            *b = self.0.block(at / BLOCK)[(at % BLOCK) as usize];
        }
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.1)
    }
}

fn formatted(bytes: u64) -> Formatted {
    Formatted(made(bytes).1, bytes)
}

/// What `format` makes reads as an empty file system: a root holding lost+found, both as
/// mke2fs makes them, in every group's descriptor checked.
#[test]
fn a_volume_reads_the_file_system_format_made() {
    let v = read::Volume::open(formatted(1 << 30)).unwrap();
    let root = v.inode(read::ROOT).unwrap();
    assert_eq!((root.kind, root.mode, root.links), (read::Kind::Dir, 0o40755, 3));
    let names: Vec<Vec<u8>> = v.read_dir(&root).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, vec![b"lost+found".to_vec()]);
    let lost = v.lookup(b"/lost+found").unwrap().unwrap();
    assert_eq!(
        (lost.ino, lost.mode, lost.links, lost.size),
        (11, 0o40700, 2, 4 * BLOCK)
    );
    assert!(v.read_dir(&lost).unwrap().is_empty());
    assert!(v.lookup(b"/none").unwrap().is_none());
    assert!(v.lookup(b"/lost+found/none").unwrap().is_none());
    assert_eq!(root.mtime.sec, 1_792_486_936);
}

/// The tree scripts/ext4/generate made of every kind of file with mke2fs -d, its
/// directories indexed by e2fsck -D, reads as debugfs read it: each file's inode, type,
/// mode, owner, size, links and times to the nanosecond, a link's target, a device's
/// numbers, its attributes (an ACL as the kernel gives it), its bytes, and each
/// directory's names.
#[test]
fn a_volume_reads_as_debugfs_reads_it() {
    use sha2::Digest as _;
    use std::io::Read as _;
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let mut packed = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(data.join("tree.blocks.gz")).unwrap())
        .read_to_end(&mut packed)
        .unwrap();
    let size = u64::from_le_bytes(packed[..8].try_into().unwrap());
    let mut image = vec![0u8; size as usize];
    for chunk in packed[8..].chunks(4 + BLOCK as usize) {
        let n = u32::from_le_bytes(chunk[..4].try_into().unwrap()) as usize * BLOCK as usize;
        image[n..n + BLOCK as usize].copy_from_slice(&chunk[4..]);
    }
    let tree: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(data.join("tree.json")).unwrap()).unwrap();
    let v = read::Volume::open(image.clone()).unwrap();
    let mut walker = read::Walker::new(read::Volume::open(image).unwrap());
    let kinds = [
        ("file", read::Kind::File),
        ("dir", read::Kind::Dir),
        ("symlink", read::Kind::Symlink),
        ("char", read::Kind::Char),
        ("block", read::Kind::Block),
        ("fifo", read::Kind::Fifo),
    ];
    let mut indexed = 0;
    for e in &tree {
        let path = e["path"].as_str().unwrap();
        let inode = v
            .lookup(path.as_bytes())
            .unwrap()
            .unwrap_or_else(|| panic!("{path}"));
        let names = |v: &serde_json::Value| -> Vec<Vec<u8>> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|n| n.as_str().unwrap().as_bytes().to_vec())
                .collect()
        };
        if path == "/" {
            let mut got: Vec<Vec<u8>> = v.read_dir(&inode).unwrap().into_iter().map(|e| e.name).collect();
            got.sort();
            assert_eq!(got, names(&e["children"]), "/");
            continue;
        }
        let kind = kinds.iter().find(|(k, _)| *k == e["type"]).unwrap().1;
        let num = |k: &str| e[k].as_u64().unwrap();
        let time = |k: &str| (e[k][0].as_i64().unwrap(), e[k][1].as_u64().unwrap() as u32);
        assert_eq!(u64::from(inode.ino), num("ino"), "{path}");
        assert_eq!(inode.kind, kind, "{path}");
        assert_eq!(u64::from(inode.mode & 0o7777), num("mode"), "{path}");
        assert_eq!(
            (u64::from(inode.uid), u64::from(inode.gid)),
            (num("uid"), num("gid")),
            "{path}"
        );
        assert_eq!(
            (inode.size, u64::from(inode.links)),
            (num("size"), num("links")),
            "{path}"
        );
        assert_eq!(u64::from(inode.flags), num("flags"), "{path}");
        assert_eq!((inode.mtime.sec, inode.mtime.nsec), time("mtime"), "{path}");
        assert_eq!((inode.atime.sec, inode.atime.nsec), time("atime"), "{path}");
        assert_eq!((inode.ctime.sec, inode.ctime.nsec), time("ctime"), "{path}");
        let xattrs: Vec<(Vec<u8>, String)> = v
            .xattrs(&inode)
            .unwrap()
            .into_iter()
            .map(|(n, val)| (n, val.iter().map(|b| format!("{b:02x}")).collect()))
            .collect();
        let want: Vec<(Vec<u8>, String)> = e["xattrs"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(n, val)| (n.as_bytes().to_vec(), val.as_str().unwrap().to_string()))
            .collect();
        assert_eq!(xattrs, want, "{path}");
        match kind {
            read::Kind::File => {
                let mut bytes = Vec::new();
                v.reader(&inode).unwrap().read_to_end(&mut bytes).unwrap();
                let digest: String = sha2::Sha256::digest(&bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                assert_eq!(digest, e["sha256"].as_str().unwrap(), "{path}");
            }
            read::Kind::Symlink => {
                assert_eq!(
                    v.read_link(&inode).unwrap(),
                    e["target"].as_str().unwrap().as_bytes(),
                    "{path}"
                );
            }
            read::Kind::Char | read::Kind::Block => {
                let (major, minor) = inode.device();
                assert_eq!(
                    (u64::from(major), u64::from(minor)),
                    (e["device"][0].as_u64().unwrap(), e["device"][1].as_u64().unwrap()),
                    "{path}"
                );
            }
            read::Kind::Dir => {
                let mut got: Vec<Vec<u8>> = v.read_dir(&inode).unwrap().into_iter().map(|e| e.name).collect();
                got.sort();
                assert_eq!(got, names(&e["children"]), "{path}");
                let listed: Vec<Vec<u8>> = walker
                    .list(path.as_bytes())
                    .unwrap()
                    .into_iter()
                    .map(|(n, _)| n)
                    .collect();
                assert_eq!(listed, names(&e["children"]), "{path}");
                // EXT4_INDEX_FL: a hash tree e2fsck -D made.
                if inode.flags & 0x1000 != 0 {
                    indexed += 1;
                }
            }
            _ => {}
        }
    }
    assert!(indexed > 0, "no directory with a hash tree read");
    // Eight runs of data: more extents than an inode holds, read through a tree's leaf.
    assert!(v.lookup(b"/sparse").unwrap().unwrap().depth() > 0);
    // A walk's lookups agree with the volume's.
    assert_eq!(
        walker.lookup(b"/deep/er/still/file").unwrap().unwrap().ino,
        v.lookup(b"deep/er/still/file").unwrap().unwrap().ino
    );
}

/// The oracle's image, as its file holds it.
fn tree_image() -> Vec<u8> {
    image_of("tree")
}

/// testdata's image `name`.blocks.gz, as its file holds it.
fn image_of(name: &str) -> Vec<u8> {
    use std::io::Read as _;
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let mut packed = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(data.join(format!("{name}.blocks.gz"))).unwrap())
        .read_to_end(&mut packed)
        .unwrap();
    let size = u64::from_le_bytes(packed[..8].try_into().unwrap());
    let mut image = vec![0u8; size as usize];
    for chunk in packed[8..].chunks(4 + BLOCK as usize) {
        let n = u32::from_le_bytes(chunk[..4].try_into().unwrap()) as usize * BLOCK as usize;
        image[n..n + BLOCK as usize].copy_from_slice(&chunk[4..]);
    }
    image
}

/// Rewrites the superblock's checksum after a change to it.
fn resum(image: &mut [u8]) {
    let sum = crc32c(!0, &image[1024..1024 + 0x3FC]);
    image[1024 + 0x3FC..1024 + 0x400].copy_from_slice(&sum.to_le_bytes());
}

/// A file system its kernel left with its journal to replay, damaged, or with a feature
/// this does not read is refused, said why: nothing of it is taken as it stands.
#[test]
fn a_volume_that_cannot_be_read_as_it_stands_is_refused() {
    let good = tree_image();
    let refused = |image: Vec<u8>| read::Volume::open(image).map(|_| ()).unwrap_err().0;
    // Marked as needing recovery, as its kernel leaves it mounted, its journal empty (as
    // EXT4_IOC_CHECKPOINT leaves it): nothing to replay, and read as it stands.
    let mut recover = good.clone();
    recover[1024 + 0x60] |= 0x4;
    resum(&mut recover);
    assert!(!journal::needs_replay(&recover).unwrap());
    assert!(!journal::needs_replay(&good).unwrap());
    read::Volume::open(recover).unwrap();
    // Its journal's log holding transactions, as a killed microVM's guest leaves it:
    // refused, to replay first.
    let logged = image_of("journal");
    assert!(journal::needs_replay(&logged).unwrap());
    assert!(refused(logged.clone()).contains("journal"));
    // Its journal's superblock damaged (`s_errno`): refused, its log's start not taken as
    // it says.
    let mut damaged = logged;
    let jsb = journal_block_at(&damaged, 0);
    damaged[jsb + 0x20] ^= 1;
    assert!(
        journal::needs_replay(&damaged)
            .unwrap_err()
            .0
            .contains("checksum")
    );
    assert!(refused(damaged).contains("checksum"));
    let mut errors = good.clone();
    errors[1024 + 0x3A] |= 0x2;
    resum(&mut errors);
    assert!(refused(errors).contains("errors"));
    let mut inline = good.clone();
    inline[1024 + 0x61] |= 0x80;
    resum(&mut inline);
    assert!(refused(inline).contains("features"));
    let mut unsummed = good.clone();
    unsummed[1024 + 0x30] ^= 1;
    assert!(refused(unsummed).contains("checksum"));
    // Past its file.
    let mut short = good.clone();
    short.truncate(good.len() / 2);
    assert!(refused(short).contains("blocks"));
    // An inode whose bytes changed under its checksum.
    let v = read::Volume::open(good.clone()).unwrap();
    let small = v.lookup(b"/small").unwrap().unwrap();
    assert_eq!(small.ino, 23);
    drop(v);
    let table = {
        let d = 1024 * 4 + 0x8;
        u32::from_le_bytes(good[d..d + 4].try_into().unwrap()) as usize * BLOCK as usize
    };
    let mut flipped = good.clone();
    flipped[table + 22 * 256 + 0x4] ^= 1;
    let v = read::Volume::open(flipped).unwrap();
    assert!(v.inode(23).unwrap_err().0.contains("checksum"));
}

/// Bytes changed anywhere in a file system's metadata never panic its reader, nor make it
/// read past the file or for ever: each read ends, with what it found or why it could not.
#[test]
fn a_damaged_volume_is_read_without_panic() {
    let good = tree_image();
    // The blocks that hold anything: its metadata, directories and attributes.
    let used: Vec<usize> = (0..good.len() / BLOCK as usize)
        .filter(|&b| {
            good[b * BLOCK as usize..(b + 1) * BLOCK as usize]
                .iter()
                .any(|&x| x != 0)
        })
        .collect();
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..400 {
        let mut image = good.clone();
        for _ in 0..1 + next() % 8 {
            let block = used[(next() % used.len() as u64) as usize];
            let at = block * BLOCK as usize + (next() % BLOCK) as usize;
            image[at] ^= 1 << (next() % 8);
        }
        // Checksums made good again where they cover what changed, so that what lies
        // behind them is read too.
        if next() % 2 == 0 {
            resum(&mut image);
        }
        let Ok(v) = read::Volume::open(image) else {
            continue;
        };
        let mut walker = read::Walker::new(v);
        let mut stack = vec![b"/".to_vec()];
        let mut seen = 0;
        while let Some(dir) = stack.pop() {
            seen += 1;
            assert!(seen < 10_000, "a walk without end");
            let Ok(names) = walker.list(&dir) else {
                continue;
            };
            for (name, is_dir) in names {
                let path = [
                    dir.as_slice(),
                    if dir.ends_with(b"/") { b"" } else { b"/" },
                    &name,
                ]
                .concat();
                if let Ok(Some(inode)) = walker.lookup(&path) {
                    let v = walker.volume();
                    let _ = v.xattrs(&inode);
                    let _ = v.read_link(&inode);
                    if let Ok(mut r) = v.reader(&inode) {
                        let _ = std::io::copy(&mut r, &mut std::io::sink());
                    }
                }
                if is_dir {
                    stack.push(path);
                }
            }
        }
    }
}

/// An image in memory, read and written in place, and what was done to it in order: each
/// write's offset, and each sync (None).
struct Mem(std::cell::RefCell<Vec<u8>>, std::cell::RefCell<Vec<Option<u64>>>);

impl Mem {
    fn new(image: Vec<u8>) -> Mem {
        Mem(std::cell::RefCell::new(image), std::cell::RefCell::default())
    }
}

impl read::ReadAt for Mem {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.0.borrow().as_slice().read_exact_at(buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.0.borrow().len() as u64)
    }
}

impl journal::WriteAt for Mem {
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let mut image = self.0.borrow_mut();
        let at = offset as usize;
        let end = at + buf.len();
        if end > image.len() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        image[at..end].copy_from_slice(buf);
        self.1.borrow_mut().push(Some(offset));
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        self.1.borrow_mut().push(None);
        Ok(())
    }
}

/// The journal scripts/ext4/generate wrote with debugfs, as a guest's killed microVM leaves
/// its layer's, replays as debugfs replays it: every block of the file system as debugfs
/// left it, a revoked block as it was, a later transaction's over an earlier's, an escaped
/// block's magic given back; then it reads with nothing to replay.
#[test]
fn a_journal_replays_as_debugfs_replays_it() {
    use sha2::Digest as _;
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let oracle: serde_json::Value =
        serde_json::from_slice(&std::fs::read(data.join("replayed.json")).unwrap()).unwrap();
    let before = image_of("journal");
    let image = Mem::new(before.clone());
    assert!(journal::needs_replay(&image).unwrap());
    assert!(read::Volume::open(&image).unwrap_err().0.contains("journal"));
    let done = journal::replay(&image).unwrap().unwrap();
    assert_eq!(
        done,
        journal::Replayed {
            transactions: 3,
            blocks: 3,
            revoked: 1
        }
    );
    // What it replayed in its file before its journal is marked empty, then its superblock
    // too: a host stopped between them finds the journal to replay again, or replayed.
    let jsb = journal_block_at(&before, 0) as u64;
    let done_to = image.1.take();
    let synced = done_to.iter().position(Option::is_none).unwrap();
    assert!(synced > 0, "{done_to:?}");
    assert!(
        done_to[..synced]
            .iter()
            .all(|&w| w != Some(jsb) && w != Some(1024)),
        "{done_to:?}"
    );
    assert_eq!(&done_to[synced + 1..], &[Some(jsb), Some(1024), None]);
    let image = image.0.into_inner();
    // The superblock as it was, but for needs_recovery cleared and its checksum: debugfs
    // sets two fields of its own besides, s_lastcheck (to 0xFFFFFFFF) and s_kbytes_written
    // (by what it wrote), which neither the kernel's replay nor this one keeps.
    let mut sb = before[1024..2048].to_vec();
    sb[0x60] &= !0x4;
    let sum = crc32c(!0, &sb[..0x3FC]);
    sb[0x3FC..].copy_from_slice(&sum.to_le_bytes());
    assert_eq!(&image[1024..2048], &sb[..], "the superblock");
    assert_eq!(&image[..1024], &before[..1024]);
    let want = oracle["blocks"].as_object().unwrap();
    let mut differ = Vec::new();
    for (n, block) in image.chunks(BLOCK as usize).enumerate().skip(1) {
        let got = block.iter().any(|&b| b != 0).then(|| {
            sha2::Sha256::digest(block)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        });
        let expected = want
            .get(&n.to_string())
            .and_then(|h| h.as_str())
            .map(str::to_string);
        if got != expected {
            differ.push(n);
        }
    }
    assert_eq!(differ, Vec::<usize>::new(), "blocks unlike debugfs's replay");
    assert!(!journal::needs_replay(&image).unwrap());
    let v = read::Volume::open(image).unwrap();
    let multi = v.lookup(b"/multi").unwrap().unwrap();
    let mut bytes = vec![0u8; multi.size as usize];
    v.read_at(&multi, 0, &mut bytes).unwrap();
    // Its first block given back its magic, its second as it was before the journal,
    // its third transaction 3's.
    let was = read::Volume::open(tree_image()).unwrap();
    let mut before = vec![0u8; multi.size as usize];
    was.read_at(&was.lookup(b"/multi").unwrap().unwrap(), 0, &mut before)
        .unwrap();
    assert_eq!(&bytes[..4], &[0xC0, 0x3B, 0x39, 0x98]);
    assert!(bytes[4..4096].iter().all(|&b| b == b'e'));
    assert_eq!(&bytes[4096..8192], &before[4096..8192]);
    assert!(bytes[8192..12288].iter().all(|&b| b == b'h'));
}

/// Where journal block `n` of `image`'s file system lies in it.
fn journal_block_at(image: &[u8], n: u64) -> usize {
    let v = read::Volume::open_any(image).unwrap();
    let runs = v.extent_runs(&v.inode(8).unwrap()).unwrap();
    let &(l, _, p, _) = runs
        .iter()
        .find(|&&(l, len, _, _)| (l..l + len).contains(&n))
        .unwrap();
    ((p + n - l) * BLOCK) as usize
}

/// A transaction whose commit block does not hold (written in part as its microVM died)
/// never was: the transactions before it replay, and its blocks stay as they were.
#[test]
fn a_transaction_never_committed_is_not_replayed() {
    let mut image = image_of("journal");
    // Transaction 3's commit, its log's tenth block (replayed.json's logdump).
    let at = journal_block_at(&image, 10);
    image[at + 16] ^= 1;
    let was = read::Volume::open(tree_image()).unwrap();
    let mut before = vec![0u8; 13_893];
    was.read_at(&was.lookup(b"/multi").unwrap().unwrap(), 0, &mut before)
        .unwrap();
    let mem = Mem::new(image);
    let done = journal::replay(&mem).unwrap().unwrap();
    assert_eq!(
        done,
        journal::Replayed {
            transactions: 2,
            blocks: 2,
            revoked: 1
        }
    );
    let v = read::Volume::open(mem.0.into_inner()).unwrap();
    let mut bytes = vec![0u8; 13_893];
    v.read_at(&v.lookup(b"/multi").unwrap().unwrap(), 0, &mut bytes)
        .unwrap();
    assert_eq!(&bytes[..4], &[0xC0, 0x3B, 0x39, 0x98]);
    assert_eq!(&bytes[8192..12288], &before[8192..12288], "transaction 3's block");
}

/// Bytes changed anywhere in a journal's log never panic its replay, nor make it write
/// past its file system or walk its log for ever: each replay ends, with what it did or
/// why it could not, and what it leaves reads without panic too.
#[test]
fn a_damaged_journal_is_replayed_without_panic() {
    let good = image_of("journal");
    let starts: Vec<usize> = (0..12).map(|n| journal_block_at(&good, n)).collect();
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut replayed = 0;
    for _ in 0..400 {
        let mut image = good.clone();
        for _ in 0..1 + next() % 4 {
            let at = starts[(next() % starts.len() as u64) as usize] + (next() % BLOCK) as usize;
            image[at] ^= 1 << (next() % 8);
        }
        let mem = Mem::new(image);
        if let Ok(Some(_)) = journal::replay(&mem) {
            replayed += 1;
            if let Ok(v) = read::Volume::open(mem.0.into_inner())
                && let Ok(Some(multi)) = v.lookup(b"/multi")
            {
                let _ = v.read_at(&multi, 0, &mut [0u8; 16384]);
            }
        }
    }
    assert!(replayed > 0, "no damaged journal replayed at all");
}
