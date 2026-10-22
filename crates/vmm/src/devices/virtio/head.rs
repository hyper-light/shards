//! Memory a guest's kernel keeps a device's page structures in, the device's own rather
//! than the guest's RAM (a vmem_altmap): a writable layer's head (D190) and each shared
//! directory's DAX window's (D189). The guest's kernel writes it as it probes the device; a
//! snapshot keeps it beside the machine's memory, each of its 2 MiB blocks once among those
//! alike ([`Head::save`]), and a restore maps it back privately, as guest RAM is restored.

use std::fs::File;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use super::mem::Mapper;
use super::pmem::{ALIGN, SECTION};
use crate::platform;

/// The bytes of a DAX window's head (D189): a region of its device's own right before the
/// window, shared memory region 0xfe, which the guest's kernel keeps the window's page
/// structures in (kernel patch 0008) rather than its RAM, where they took a 64th of the
/// window and more (PM M269); for a window of `len` bytes, whole 2 MiB ranges, its head at
/// a section's start ([`SECTION`]). Each section of the device, head and window, takes a 2
/// MiB block of page structures (64 bytes for each 4 KiB page), placed at a 2 MiB boundary
/// of the head (mm/sparse-vmemmap.c, vmemmap_populate_hugepages and
/// altmap_alloc_block_buf). The head makes the device whole sections, the fewest whose
/// blocks it holds: a section the device ended inside would take its page structures from
/// the guest's RAM on x86_64, which fills a part section's from base pages of no device's
/// own (arch/x86/mm/init_64.c, vmemmap_populate, Linux 6.18).
pub fn window_head_len(len: u64) -> Result<u64, String> {
    if len == 0 || !len.is_multiple_of(ALIGN) {
        return Err(format!("a DAX window of {len} bytes: whole {} MiB", ALIGN >> 20));
    }
    // `s` sections hold their `s` blocks where s × (SECTION - ALIGN) is at least `len`.
    let sections = len.div_ceil(SECTION - ALIGN);
    sections
        .checked_mul(SECTION)
        .and_then(|device| device.checked_sub(len))
        .ok_or_else(|| format!("a DAX window of {len} bytes is past any guest's addresses"))
}

/// A head: memory of the VM's own, or a snapshot's copy of it, mapped privately, as guest
/// RAM is restored (its writes the restored VM's alone).
pub struct Head {
    host: NonNull<u8>,
    len: usize,
    /// The snapshot's copy, where the head was restored from one.
    _file: Option<File>,
}

// SAFETY: the head's memory is owned by this value; the guest writes it, through the
// hypervisor, and this process reads it only for a snapshot, of a paused machine.
unsafe impl Send for Head {}
// SAFETY: as above.
unsafe impl Sync for Head {}

impl Drop for Head {
    fn drop(&mut self) {
        // SAFETY: the reservation `new` or `restore` made (a snapshot's file mapped over
        // it); the VM that mapped it is gone (`Running` drops the VM before the devices).
        unsafe { platform::release(self.host, self.len) };
    }
}

impl std::fmt::Debug for Head {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Head")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl Head {
    /// `len` bytes of the VM's own memory, whole [`HEAD_BLOCK`]s, zeros until its guest
    /// writes them: a boot's.
    pub fn new(len: usize) -> Result<Head, String> {
        if len == 0 || !len.is_multiple_of(HEAD_BLOCK) {
            return Err(format!("a head of {len} bytes: whole {} MiB", HEAD_BLOCK >> 20));
        }
        let host = platform::reserve_ram(len).map_err(|e| e.to_string())?;
        Ok(Head {
            host,
            len,
            _file: None,
        })
    }

    /// A head of `len` bytes as a snapshot kept it in `kept` ([`save`](Self::save)): each
    /// 2 MiB block mapped privately from the copy where the head had it, fresh memory where
    /// it had zeros.
    pub fn restore(kept: File, len: usize) -> Result<Head, String> {
        let at = |e: std::io::Error| e.to_string();
        if len == 0 || !len.is_multiple_of(HEAD_BLOCK) {
            return Err(format!("a head of {len} bytes: whole {} MiB", HEAD_BLOCK >> 20));
        }
        let blocks = len / HEAD_BLOCK;
        let index = read_head_index(&kept, blocks)?;
        let host = platform::reserve(len).map_err(at)?;
        // Held from here, so that a failure releases the reservation.
        let mut head = Head {
            host,
            len,
            _file: None,
        };
        for (i, kept_at) in index.iter().enumerate() {
            let Some(n) = kept_at else {
                continue;
            };
            let offset = kept_block_at(*n, blocks)?;
            // SAFETY: a whole block of the reservation, which nothing references yet.
            match unsafe { platform::map_file_private(&kept, offset, HEAD_BLOCK, host.add(i * HEAD_BLOCK)) } {
                Ok(()) => {}
                // Where no file maps over part of a reservation (Windows), read into it, as
                // guest RAM is restored there (`GuestMemory::from_file`).
                Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                    // SAFETY: a whole block of the reservation, committed memory that
                    // nothing references yet.
                    let block = unsafe {
                        std::slice::from_raw_parts_mut(host.as_ptr().add(i * HEAD_BLOCK), HEAD_BLOCK)
                    };
                    platform::read_exact_at(&kept, block, offset).map_err(at)?;
                }
                Err(e) => return Err(at(e)),
            }
        }
        // On the host's base pages, as guest RAM restored from a file is (`GuestMemory::
        // from_file`, PM M287): a block's page structures the guest touches are a few pages.
        platform::small_pages(host, len);
        head._file = Some(kept);
        Ok(head)
    }

    /// Where this process has it.
    pub fn host(&self) -> NonNull<u8> {
        self.host
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Writes it into `file`, as a paused machine's guest left it, its 2 MiB blocks each
    /// once: all but a few hold the page structures of a section none of whose pages is in
    /// use, which are alike, the 64 bytes of each page the same (mm_init.c,
    /// __init_zone_device_page), so that a 10 GiB layer's head of 85 blocks is kept in
    /// five (PM M277). An index first, block by block: which of those it is, or none for
    /// zeros.
    pub fn save(&self, file: &File) -> std::io::Result<()> {
        save_head(file, self.host, self.len)
    }

    /// Maps it back from `kept`, the copy a save has just written of it
    /// ([`save`](Self::save)), as a restore maps one ([`restore`](Self::restore)): each
    /// block privately from the copy where the head had it, fresh memory where it had
    /// zeros. Mapped back, it holds what its guest goes on to write alone, and its alike
    /// blocks are one copy in the host's page cache, its restores' too. One that fails
    /// partway leaves each block the copy's or the head's own memory, which hold the same.
    /// Where no file maps over part of a reservation (Windows), it stays its own memory.
    ///
    /// # Safety
    /// The machine is paused and the head saved into `kept` since: no vCPU runs and no host
    /// thread touches the head until this returns, and no hypervisor whose stage 2 holds the
    /// pages it maps maps the head meanwhile.
    pub unsafe fn map_back(&self, kept: &File) -> Result<(), String> {
        let blocks = self.len / HEAD_BLOCK;
        let index = read_head_index(kept, blocks)?;
        // On the host's base pages, as a restore's head is ([`restore`](Self::restore)).
        platform::small_pages(self.host, self.len);
        for (i, kept_at) in index.iter().enumerate() {
            // SAFETY: a whole block of the head, which nothing touches meanwhile (the
            // caller's contract), and which held what the copy keeps of it.
            let block = unsafe { self.host.add(i * HEAD_BLOCK) };
            let remapped = match kept_at {
                // SAFETY: as above.
                None => unsafe { platform::discard(block, HEAD_BLOCK) },
                Some(n) => {
                    let offset = kept_block_at(*n, blocks)?;
                    // SAFETY: as above.
                    unsafe { platform::map_file_private(kept, offset, HEAD_BLOCK, block) }
                }
            };
            match remapped {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Unsupported => return Ok(()),
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    }
}

/// A machine's DAX windows' heads (D189), one after another in one [`Head`], each mapped at
/// its window's head ([`window_head_len`]): what a snapshot keeps of them, and what a
/// template's save maps back, as a writable layer's head is (pmem `Layer::remap_head`).
pub struct WindowHeads {
    head: Head,
    /// Each window's head's guest address, in the head's order, each `each` bytes.
    at: Vec<u64>,
    each: usize,
    /// What maps them at stage 2, once its VM has mapped them ([`placed`](Self::placed)).
    mapper: OnceLock<Box<dyn Mapper>>,
    /// Whether its guest lost one: a head taken from it to be mapped back that could not be
    /// given back ([`remap`](Self::remap)).
    lost: AtomicBool,
}

impl std::fmt::Debug for WindowHeads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowHeads")
            .field("at", &self.at)
            .field("each", &self.each)
            .finish_non_exhaustive()
    }
}

impl WindowHeads {
    /// The heads of the windows whose heads are at `at`, each `each` bytes, one after
    /// another in `head`.
    pub fn new(head: Head, at: Vec<u64>, each: usize) -> Result<WindowHeads, String> {
        if at.is_empty() || each.checked_mul(at.len()) != Some(head.len()) {
            return Err(format!(
                "{} DAX windows' heads of {each} bytes in a head of {}",
                at.len(),
                head.len()
            ));
        }
        Ok(WindowHeads {
            head,
            at,
            each,
            mapper: OnceLock::new(),
            lost: AtomicBool::new(false),
        })
    }

    /// Each head's mapping for its guest, `(gpa, host, len)`, writable by the guest.
    pub fn mappings(&self) -> impl Iterator<Item = (u64, *mut u8, usize)> + '_ {
        let host = self.head.host().as_ptr();
        self.at
            .iter()
            .enumerate()
            .map(move |(i, &gpa)| (gpa, host.wrapping_add(i * self.each), self.each))
    }

    /// Says what maps them at stage 2, once its VM has mapped them all
    /// ([`mappings`](Self::mappings)): what takes them from the guest and gives them back as
    /// they are mapped back ([`remap`](Self::remap)). Once.
    pub fn placed(&self, mapper: Box<dyn Mapper>) {
        let _ = self.mapper.set(mapper);
    }

    /// Writes them into `file`, as a paused machine's guest left them ([`Head::save`]).
    pub fn save(&self, file: &File) -> std::io::Result<()> {
        self.head.save(file)
    }

    /// Whether its guest has lost one ([`remap`](Self::remap)), so that its machine cannot
    /// go on.
    pub fn lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    /// Maps them back from `kept`, the copy a save has just written of them
    /// ([`Head::map_back`]), each taken from its guest meanwhile where they are placed
    /// ([`placed`](Self::placed)): a booted machine's are its own memory, written as its
    /// guest's kernel probed each window's device, which a template's machine, its image's
    /// first run, would hold for its life. One its guest would not give up leaves those
    /// already taken given back, and all its own memory; one its guest could not be given
    /// back leaves them [`lost`](Self::lost).
    ///
    /// # Safety
    /// As [`Head::map_back`]'s: the machine is paused and the heads saved into `kept` since.
    pub unsafe fn remap(&self, kept: &File) -> Result<(), String> {
        let mapper = self.mapper.get();
        let give_back = |taken: &[(u64, *mut u8, usize)]| -> Result<(), String> {
            let Some(mapper) = mapper else {
                return Ok(());
            };
            for &(gpa, host, len) in taken {
                // SAFETY: the head stays mapped in this process for its life, which
                // outlives the VM (`Running` drops the VM before the devices).
                if let Err(e) = unsafe { mapper.map(host, gpa, len) } {
                    self.lost.store(true, Ordering::Release);
                    return Err(e);
                }
            }
            Ok(())
        };
        let all: Vec<(u64, *mut u8, usize)> = self.mappings().collect();
        if let Some(mapper) = mapper {
            for (n, &(gpa, _, len)) in all.iter().enumerate() {
                if let Err(e) = mapper.unmap(gpa, len) {
                    give_back(all.get(..n).unwrap_or_default())?;
                    return Err(e);
                }
            }
        }
        // SAFETY: the caller's contract; the guest has none of them meanwhile.
        let remapped = unsafe { self.head.map_back(kept) };
        give_back(&all)?;
        remapped
    }
}

/// A head's blocks as a snapshot keeps them: what the guest's kernel fills each of a
/// whole 2 MiB, a section's page structures (vmemmap_populate_hugepages).
pub(crate) const HEAD_BLOCK: usize = ALIGN as usize;
/// A kept head's first bytes (D190).
const HEAD_MAGIC: [u8; 8] = *b"SHRDHEAD";
/// The most blocks a kept head's index compares each block with before it keeps it as one
/// more: a head's alike blocks are a few kinds.
const HEAD_KINDS: usize = 64;

/// The bytes of a kept head's index for `blocks` blocks: its magic, its count, and a 32-bit
/// number for each, whole blocks of it, so that what follows lies at block boundaries.
fn head_index_len(blocks: usize) -> u64 {
    let bytes = 12 + 4 * blocks as u64;
    bytes.next_multiple_of(HEAD_BLOCK as u64)
}

/// Where a kept head of `blocks` blocks keeps its block `n`: after its index, in the order
/// [`save_head`] kept them.
pub(crate) fn kept_block_at(n: u32, blocks: usize) -> Result<u64, String> {
    u64::from(n)
        .checked_mul(HEAD_BLOCK as u64)
        .and_then(|o| o.checked_add(head_index_len(blocks)))
        .ok_or_else(|| "a head's block past any file".to_string())
}

/// Writes the `len` bytes of a head at `host`, whole [`HEAD_BLOCK`]s, into `file`: an index
/// of its blocks, each a number among those kept after it, or `u32::MAX` for all zeros;
/// then each kept block once, in the order first met.
pub(crate) fn save_head(file: &File, host: NonNull<u8>, len: usize) -> std::io::Result<()> {
    let invalid = || std::io::Error::from(std::io::ErrorKind::InvalidInput);
    if !len.is_multiple_of(HEAD_BLOCK) {
        return Err(invalid());
    }
    let blocks = len / HEAD_BLOCK;
    let block = |i: usize| host.as_ptr().wrapping_add(i * HEAD_BLOCK).cast_const();
    // The head blocks kept, by their first place in the head.
    let mut kept: Vec<usize> = Vec::new();
    let mut index: Vec<u32> = Vec::with_capacity(blocks);
    for i in 0..blocks {
        // SAFETY: a whole block within the head, which the paused machine leaves alone.
        if unsafe { words_zero(block(i), HEAD_BLOCK) } {
            index.push(u32::MAX);
            continue;
        }
        // Compared with the first kinds kept alone: past them, each block is kept as it is.
        // SAFETY: as above, for both blocks.
        let alike = kept
            .iter()
            .take(HEAD_KINDS)
            .position(|&k| unsafe { words_equal(block(i), block(k), HEAD_BLOCK) });
        let n = match alike {
            Some(n) => n,
            None => {
                kept.push(i);
                kept.len() - 1
            }
        };
        index.push(u32::try_from(n).map_err(|_| invalid())?);
    }
    let first = head_index_len(blocks);
    let mut header = Vec::with_capacity(12 + 4 * blocks);
    header.extend_from_slice(&HEAD_MAGIC);
    header.extend_from_slice(&u32::try_from(blocks).map_err(|_| invalid())?.to_le_bytes());
    for n in &index {
        header.extend_from_slice(&n.to_le_bytes());
    }
    file.set_len(0)?;
    platform::write_all_at(file, &header, 0)?;
    for (n, &i) in kept.iter().enumerate() {
        let at = first + (n * HEAD_BLOCK) as u64;
        let mut done = 0;
        while done < HEAD_BLOCK {
            // SAFETY: the rest of block `i` of the head, which the kernel only reads.
            let wrote =
                unsafe { platform::write_at(file, block(i).add(done), HEAD_BLOCK - done, at + done as u64) }?;
            if wrote == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            done += wrote;
        }
    }
    file.set_len(first + (kept.len() * HEAD_BLOCK) as u64)
}

/// The index of a head [`save_head`] kept in `file`, of `blocks` blocks: each block's
/// number among those kept, or none for zeros; checked against the file's length.
pub(crate) fn read_head_index(file: &File, blocks: usize) -> Result<Vec<Option<u32>>, String> {
    let mut magic = [0u8; 12];
    platform::read_exact_at(file, &mut magic, 0).map_err(|e| e.to_string())?;
    if magic.get(..8) != Some(&HEAD_MAGIC[..]) {
        return Err("not a head shards kept".into());
    }
    let count = magic
        .get(8..12)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_le_bytes);
    if count != u32::try_from(blocks).ok() {
        return Err(format!("a head of {count:?} blocks kept for one of {blocks}"));
    }
    let mut raw = vec![0u8; 4 * blocks];
    platform::read_exact_at(file, &mut raw, 12).map_err(|e| e.to_string())?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    let kept = len.saturating_sub(head_index_len(blocks)) / HEAD_BLOCK as u64;
    raw.as_chunks::<4>()
        .0
        .iter()
        .map(|b| {
            let n = u32::from_le_bytes(*b);
            match n {
                u32::MAX => Ok(None),
                n if u64::from(n) < kept => Ok(Some(n)),
                n => Err(format!("a head's block {n} past the {kept} it keeps")),
            }
        })
        .collect()
}

/// Whether the `len` bytes at `p`, word-aligned, are all zeros, read word by word as the
/// guest's memory they are.
///
/// # Safety
/// `p` must be valid for reads of `len` bytes, word-aligned, a whole number of words.
unsafe fn words_zero(p: *const u8, len: usize) -> bool {
    let words = p.cast::<u64>();
    // SAFETY: each word lies within the caller's range.
    (0..len / 8).all(|k| unsafe { words.add(k).read_volatile() } == 0)
}

/// Whether the `len` bytes at `a` and at `b` are alike, read word by word.
///
/// # Safety
/// As [`words_zero`], for both.
pub(crate) unsafe fn words_equal(a: *const u8, b: *const u8, len: usize) -> bool {
    let (a, b) = (a.cast::<u64>(), b.cast::<u64>());
    // SAFETY: each word lies within the caller's ranges.
    (0..len / 8).all(|k| unsafe { a.add(k).read_volatile() == b.add(k).read_volatile() })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    fn file(dir: &std::path::Path, name: &str) -> File {
        File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.join(name))
            .unwrap()
    }

    /// A window's head makes its device whole sections, the fewest whose page structures,
    /// a 2 MiB block each, it holds; a window not whole 2 MiB ranges, or past any address,
    /// has none (D189).
    #[test]
    fn a_windows_head_makes_its_device_whole_sections_it_holds_the_page_structures_of() {
        for len in [
            ALIGN,
            126 * MIB,
            128 * MIB,
            896 * MIB,
            1 << 30,
            1536 * MIB,
            4 << 30,
            64 << 30,
        ] {
            let head = window_head_len(len).unwrap();
            let device = head + len;
            assert_eq!(device % SECTION, 0, "{len}");
            let sections = device / SECTION;
            assert!(head >= sections * ALIGN, "{len}: each section's block");
            assert!(
                (sections - 1) * (SECTION - ALIGN) < len,
                "{len}: one section fewer holds them"
            );
        }
        // 1 GiB: nine sections, 18 MiB of page structures in a head of one section; 4 GiB,
        // 33 and 66 in one; one section's own window, a block.
        assert_eq!(window_head_len(1 << 30).unwrap(), SECTION);
        assert_eq!(window_head_len(4 << 30).unwrap(), SECTION);
        assert_eq!(window_head_len(SECTION - ALIGN).unwrap(), ALIGN);
        assert!(window_head_len(0).is_err());
        assert!(window_head_len(ALIGN + 4096).is_err());
        assert!(window_head_len(u64::MAX - ALIGN + 1).is_err());
    }

    /// Windows' heads are each its own part of one head, saved and restored whole, each
    /// block where it was; heads of other lengths than their windows' are refused (D189).
    #[test]
    fn windows_heads_are_parts_of_one_head_kept_whole() {
        let dir = shards_testdir::TempDir::new("window-heads").unwrap();
        let each = 2 * HEAD_BLOCK;
        let heads = WindowHeads::new(
            Head::new(3 * each).unwrap(),
            vec![1 << 32, 3 << 31, 1 << 33],
            each,
        )
        .unwrap();
        let mappings: Vec<_> = heads.mappings().collect();
        let host = mappings[0].1;
        assert_eq!(
            mappings,
            [
                (1 << 32, host, each),
                (3 << 31, host.wrapping_add(each), each),
                (1 << 33, host.wrapping_add(2 * each), each)
            ]
        );
        // As a guest's kernel writes them: alike blocks, and a block of its own.
        // SAFETY: within the head, which `heads` holds.
        unsafe {
            for i in 0..3 {
                host.add(i * each).write_bytes(0x40, HEAD_BLOCK);
            }
            host.add(2 * each + HEAD_BLOCK).write_bytes(0x41, 64);
        }
        let kept = file(&dir, "heads");
        heads.save(&kept).unwrap();
        let restored = Head::restore(kept, 3 * each).unwrap();
        // SAFETY: within both heads, which their values hold.
        unsafe {
            for i in 0..6 {
                let (a, b) = (
                    host.add(i * HEAD_BLOCK),
                    restored.host().as_ptr().add(i * HEAD_BLOCK),
                );
                assert!(words_equal(a, b, HEAD_BLOCK), "block {i}");
            }
        }
        assert!(WindowHeads::new(Head::new(3 * each).unwrap(), vec![1 << 32, 1 << 33], each).is_err());
        assert!(WindowHeads::new(Head::new(each).unwrap(), Vec::new(), each).is_err());
    }

    /// Placed heads are each taken from their guest while they are mapped back, and given
    /// back after; one its guest would not give up leaves those taken given back, and one
    /// its guest could not be given back is lost to it (D189).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn placed_window_heads_are_taken_from_their_guest_while_they_are_mapped_back() {
        use std::sync::{Arc, Mutex};

        use crate::sync::lock;

        type Calls = Arc<Mutex<Vec<(&'static str, u64)>>>;
        struct Guest {
            calls: Calls,
            /// The guest address whose unmap fails, if one does; whether maps do.
            refuses: Option<u64>,
            maps: bool,
        }
        impl Mapper for Guest {
            unsafe fn map(&self, _: *mut u8, gpa: u64, _: usize) -> Result<(), String> {
                lock(&self.calls).push(("map", gpa));
                if self.maps { Ok(()) } else { Err("no map".into()) }
            }
            fn unmap(&self, gpa: u64, _: usize) -> Result<(), String> {
                lock(&self.calls).push(("unmap", gpa));
                if self.refuses == Some(gpa) {
                    Err("no unmap".into())
                } else {
                    Ok(())
                }
            }
        }
        let dir = shards_testdir::TempDir::new("window-remap").unwrap();
        let (a, b) = (1u64 << 32, 1u64 << 33);
        for (n, (refuses, maps)) in [(None, true), (Some(b), true), (None, false)]
            .into_iter()
            .enumerate()
        {
            let each = HEAD_BLOCK;
            let heads = WindowHeads::new(Head::new(2 * each).unwrap(), vec![a, b], each).unwrap();
            let host = heads.mappings().next().unwrap().1;
            // SAFETY: within the head, which `heads` holds.
            unsafe { host.add(each).write_bytes(0x40, each) };
            let kept = file(&dir, &format!("heads-{n}"));
            heads.save(&kept).unwrap();
            let calls = Calls::default();
            heads.placed(Box::new(Guest {
                calls: calls.clone(),
                refuses,
                maps,
            }));
            // SAFETY: nothing else touches the heads, which no hypervisor maps.
            let remapped = unsafe { heads.remap(&kept) };
            match (refuses, maps) {
                (None, true) => {
                    assert_eq!(remapped, Ok(()));
                    assert_eq!(
                        *lock(&calls),
                        [("unmap", a), ("unmap", b), ("map", a), ("map", b)]
                    );
                }
                (Some(_), _) => {
                    assert_eq!(remapped, Err("no unmap".into()));
                    assert_eq!(*lock(&calls), [("unmap", a), ("unmap", b), ("map", a)]);
                }
                (None, false) => {
                    assert_eq!(remapped, Err("no map".into()));
                    assert_eq!(*lock(&calls), [("unmap", a), ("unmap", b), ("map", a)]);
                }
            }
            assert_eq!(heads.lost(), !maps);
            // SAFETY: as above.
            assert_eq!(unsafe { *host.add(2 * each - 1) }, 0x40);
        }
    }
}
