//! Working sets from what the host maps of a restored machine's memory (KVM's, on x86_64
//! and arm64 alike): the host maps a page of RAM, mapped copy-on-write from the snapshot's
//! file afresh, or of pmem, mapped from its image, only once the guest (or a device) touches
//! it, and a private copy of RAM is a page it wrote; and what a working set says the guest
//! wrote is copied ahead of it, as its writes would copy it.

use std::sync::Arc;

use crate::devices::virtio::pmem;
use crate::hv;
use crate::memory::GuestMemory;
use crate::{debug, info, platform};

/// The host's page, which a working set recorded from its mappings is made of: a host of
/// pages of another size records none.
const PAGE: u64 = 4 << 10;

/// Makes the private copies of RAM that the guest wrote while its working set was
/// recorded, as its writes would make them: KVM then maps these pages writable ahead,
/// where a write to a page mapped read-only would fault, copy, and fault again. Every run
/// it can ([`every_run`]).
pub fn copy_written(memory: &GuestMemory, working_set: &[hv::Touch]) -> Result<(), String> {
    let t0 = crate::log::uptime_us();
    let written = runs(working_set.iter().filter(|t| t.written).map(|t| t.gpa));
    let (pages, count) = (
        written.iter().map(|(_, len)| len / PAGE).sum::<u64>(),
        written.len(),
    );
    let copied = every_run(written, |gpa, len| {
        let len = usize::try_from(len).map_err(|e| e.to_string())?;
        let host = memory.host_ptr(gpa, len).map_err(|e| e.to_string())?;
        platform::populate_writable(host, len).map_err(|e| e.to_string())
    });
    if pages > 0 {
        info!(
            "copied {pages} written working-set pages ahead, in {count} runs, in {} us",
            crate::log::uptime_us().saturating_sub(t0)
        );
    }
    copied.map_err(|e| format!("copying written pages {e}"))
}

/// Does `f` to each run of pages of `runs`, past any it fails on: what it does only
/// readies pages ahead, and a page it does not is only faulted in later, so one run that
/// fails is no reason to leave the others (review 1.19). The first failure, and how many
/// runs failed, if any did.
pub fn every_run(
    runs: Vec<(u64, u64)>,
    mut f: impl FnMut(u64, u64) -> Result<(), String>,
) -> Result<(), String> {
    let mut failed: Option<(String, usize)> = None;
    for (gpa, len) in runs {
        if let Err(e) = f(gpa, len) {
            match &mut failed {
                Some((_, n)) => *n += 1,
                None => failed = Some((format!("at {gpa:#x}+{len:#x}: {e}"), 1)),
            }
        }
    }
    match failed {
        None => Ok(()),
        Some((first, 1)) => Err(first),
        Some((first, n)) => Err(format!("{first}, and {} more", n - 1)),
    }
}

/// The runs of consecutive pages among `pages`, as `(gpa, len)`, in order.
pub fn runs(pages: impl Iterator<Item = u64>) -> Vec<(u64, u64)> {
    let mut pages: Vec<u64> = pages.map(|gpa| gpa & !(PAGE - 1)).collect();
    pages.sort_unstable();
    pages.dedup();
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for gpa in pages {
        match runs.last_mut() {
            Some((start, len)) if start.checked_add(*len) == Some(gpa) => *len += PAGE,
            _ => runs.push((gpa, PAGE)),
        }
    }
    runs
}

/// What records a working set: a restored guest's memory, whose host mappings start
/// empty, kept mapped until the recording ends, even if the VM stops first; the host's
/// fault-around is held off over it for the machine's life (the restore's
/// `platform::FaultAroundOff`), so that what the host maps is what the guest touched, and
/// what it held of the RAM the guest gives back meanwhile is kept
/// ([`GuestMemory::keep_given_back`]).
#[derive(Debug)]
pub struct Recorder {
    memory: Arc<GuestMemory>,
    pmem: Vec<(u64, Arc<pmem::Region>)>,
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // A recording let go of keeps nothing more.
        let _ = self.memory.given_back();
    }
}

/// The host's ranges of a machine's guest memory and its pmem: what a working set is
/// recorded from and mapped ahead into.
pub fn host_ranges(memory: &GuestMemory, pmem: &[(u64, Arc<pmem::Region>)]) -> Vec<(*mut u8, usize)> {
    memory
        .regions()
        .map(|(_, host, len)| (host, len))
        .chain(pmem.iter().map(|(_, r)| (r.host(), r.len())))
        .collect()
}

/// Records what a restored machine of `memory` and `pmem` touches: its RAM is mapped
/// copy-on-write from the snapshot's file and its pmem from its images, afresh, so the host
/// maps a page only once the guest (or a device) touches it; what the host maps when the
/// recording ends is what the guest touched, and a private copy of RAM is a page it wrote.
/// None where the host's pages are not the working set's.
pub fn recorder(memory: &Arc<GuestMemory>, pmem: &[(u64, Arc<pmem::Region>)]) -> Option<Recorder> {
    let page = platform::page_size().ok()? as u64;
    if page != PAGE {
        return None;
    }
    if let Err(e) = memory.keep_given_back() {
        debug!("not recording a working set: {e}");
        return None;
    }
    Some(Recorder {
        memory: memory.clone(),
        pmem: pmem.to_vec(),
    })
}

/// The pages touched since the restore: those the host maps, and those of RAM it held as
/// the guest gave them back, written where it held a private copy. pmem is read-only, so a
/// page there that is not the file's is the shared zero page, past the end of the file.
pub fn recorded(recorder: &Recorder) -> Result<Vec<hv::Touch>, String> {
    let given = recorder.memory.given_back();
    if let Some(e) = given.as_ref().and_then(|g| g.unseen.as_ref()) {
        return Err(format!("the RAM its guest gave back as it was recorded: {e}"));
    }
    let mapped = |gpa: u64, host: *mut u8, len: usize| {
        platform::mapped_pages(host, len).map_err(|e| format!("the pages mapped at {gpa:#x}: {e}"))
    };
    let mut touched = Vec::new();
    for (region, (gpa, host, len)) in recorder.memory.regions().enumerate() {
        let mut pages: std::collections::BTreeMap<usize, bool> =
            mapped(gpa, host, len)?.into_iter().collect();
        for (i, copied) in given.as_ref().map(|g| g.pages(region)).unwrap_or_default() {
            *pages.entry(i).or_default() |= copied;
        }
        touched.extend(pages.into_iter().map(|(i, copied)| hv::Touch {
            gpa: gpa + i as u64 * PAGE,
            written: copied,
        }));
    }
    for (gpa, r) in &recorder.pmem {
        touched.extend(
            mapped(*gpa, r.host(), r.len())?
                .into_iter()
                .map(|(i, _)| hv::Touch {
                    gpa: gpa + i as u64 * PAGE,
                    written: false,
                }),
        );
    }
    Ok(touched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_join_consecutive_pages_once_each() {
        let last = !(PAGE - 1);
        let pages = [0x3000, 0x1000, 0x2000, 0x2000, 0x2fff, 0x9000, 0x8000, last];
        assert_eq!(
            runs(pages.into_iter()),
            [(0x1000, 0x3000), (0x8000, 0x2000), (last, PAGE)]
        );
        assert_eq!(runs(std::iter::empty()), []);
    }

    /// Every run is tried, past those that fail; the first failure is said, with how many
    /// more there were.
    #[test]
    fn every_run_is_tried_past_failures() {
        let all = vec![(0x1000, PAGE), (0x5000, PAGE), (0x9000, PAGE), (0xd000, PAGE)];
        let mut tried = Vec::new();
        let failing = |gpa: u64| gpa == 0x5000 || gpa == 0xd000;
        let done = every_run(all.clone(), |gpa, _| {
            tried.push(gpa);
            if failing(gpa) { Err("no".into()) } else { Ok(()) }
        });
        assert_eq!(tried, [0x1000, 0x5000, 0x9000, 0xd000]);
        assert_eq!(done.unwrap_err(), "at 0x5000+0x1000: no, and 1 more");
        assert_eq!(
            every_run(all.clone(), |gpa, _| if gpa == 0x9000 {
                Err("no".into())
            } else {
                Ok(())
            }),
            Err("at 0x9000+0x1000: no".into())
        );
        assert_eq!(every_run(all, |_, _| Ok(())), Ok(()));
    }

    /// The written pages of a run after one that cannot be copied are copied still: a
    /// damaged working set's write to what is not RAM (read-only pmem, say) leaves the
    /// rest of the copies ahead (review 1.19). Where restores take working sets
    /// (`WORKING_SETS`).
    #[cfg(any(hv = "hvf", hv = "kvm", all(hv = "whp", target_arch = "x86_64")))]
    #[test]
    fn written_pages_past_a_run_that_fails_are_copied() {
        let path_dir = shards_testdir::TempDir::new("copy-written").unwrap();
        let path = path_dir.join("copy-written");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.set_len(32 * PAGE).unwrap();
        let _ = std::fs::remove_file(&path);
        // Two regions with a gap between them, which holds no RAM.
        let ranges = [(0, 16 * PAGE as usize), (0x10_0000, 16 * PAGE as usize)];
        let memory = GuestMemory::from_file(&ranges, &file, None).unwrap();
        let touch = |gpa: u64| hv::Touch { gpa, written: true };
        let ws = [
            touch(0),
            touch(PAGE),
            touch(0x8_0000),
            touch(0x10_0000),
            touch(0x10_0000 + PAGE),
        ];
        let e = copy_written(&memory, &ws).unwrap_err();
        assert!(e.starts_with("copying written pages at 0x80000+0x1000: "), "{e}");
        for gpa in [0, 0x10_0000] {
            let host = memory.host_ptr(gpa, 16 * PAGE as usize).unwrap();
            let copies = platform::mapped_pages(host, 16 * PAGE as usize).unwrap();
            assert_eq!(copies, [(0, true), (1, true)], "at {gpa:#x}");
        }
    }

    /// A working set keeps the pages its guest touched and then gave back as it was
    /// recorded, which its host no longer maps when the recording ends: a set recorded as
    /// a guest gave its reads' page cache back held 9,529 pages, its pool's first 41,834
    /// (PM M320).
    #[cfg(hv = "kvm")]
    #[test]
    fn a_working_set_keeps_what_its_guest_gave_back_as_it_was_recorded() {
        let dir = shards_testdir::TempDir::new("given-back").unwrap();
        let path = dir.join("memory");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.set_len(32 * PAGE).unwrap();
        let ranges = [(0, 16 * PAGE as usize), (0x10_0000, 16 * PAGE as usize)];
        let memory = Arc::new(GuestMemory::from_file(&ranges, &file, None).unwrap());
        // Each read fault maps the page it faulted on alone, as a recording restore's
        // memory, whose host holds fault-around off (PM M312): no read ahead of the file's
        // holes for fault-around to map beside it.
        for (gpa, len) in ranges {
            let host = std::ptr::NonNull::new(memory.host_ptr(gpa, len).unwrap()).unwrap();
            platform::reads_at_random(host, len);
        }
        memory.keep_given_back().unwrap();
        let recorder = Recorder {
            memory: memory.clone(),
            pmem: Vec::new(),
        };
        let host = |gpa: u64| memory.host_ptr(gpa, PAGE as usize).unwrap();
        // SAFETY: pages of guest RAM no guest or device touches here.
        unsafe {
            host(0).write_volatile(1);
            let _ = host(PAGE).read_volatile();
            host(0x10_0000 + 2 * PAGE).write_volatile(1);
            let _ = host(0x10_0000 + 3 * PAGE).read_volatile();
        }
        // The guest gives back the first region's first four pages, and the second's
        // fourth; it takes its first page back after.
        let access = memory.access().unwrap();
        for (gpa, len) in [(0, 4 * PAGE), (0x10_0000 + 3 * PAGE, PAGE)] {
            access.discard(gpa, len, |_, _| Ok(()), |_, _, _| Ok(())).unwrap();
        }
        drop(access);
        assert!(platform::mapped_pages(host(0), PAGE as usize).unwrap().is_empty());
        // SAFETY: as above.
        unsafe { host(0).write_volatile(2) };
        let touch = |gpa, written| hv::Touch { gpa, written };
        assert_eq!(
            recorded(&recorder).unwrap(),
            [
                touch(0, true),
                touch(PAGE, false),
                touch(0x10_0000 + 2 * PAGE, true),
                touch(0x10_0000 + 3 * PAGE, false),
            ]
        );
        // Ended, the recording keeps nothing more.
        assert!(memory.given_back().is_none());
    }
}
