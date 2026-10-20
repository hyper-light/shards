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
