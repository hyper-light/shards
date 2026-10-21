# Restore folios

Since Linux 6.18 a read fault on a file's page maps the page cache folio holding it whole,
where the folio lies within the mapping and one page table (mm/filemap.c,
`filemap_map_folio_range`: "Map the large folio fully where possible"), whatever the
mapping's madvise(2); before, fault-around mapped at most `fault_around_bytes` (64 KiB) of
pages the page cache held. A restored guest's memory is its snapshot's file mapped private
(D25), and on KVM the first warm restore records as its working set the pages its host
maps (`vm::x86_64::recorded`), which later restores map ahead. Where the file's folios
are large, as ext4 makes them for long writes since 6.16, a fault maps up to 2 MiB, the
recording takes in all of it, and every restore maps it ahead.

`.github/workflows/restore-folios.yml` applies `folios.patch`, whose switches choose how
the memory file is written and mapped, and runs, on a GitHub runner (Linux 7.0, ext4):

- the Firecracker comparison (`benches/firecracker.rs`, n = 20) in four arms:
  - `base`: as shards is;
  - `random`: restores map the file with MADV_RANDOM, so that a fault on a page the page
    cache lacks reads that page alone, into a folio of its own (no readahead);
  - `pagewise`: the save writes the file a page a write (`write_run`), so that the page
    cache holds it in folios of a page, and restores as `random`;
  - `drop`: the save writes the file back and drops it from the page cache
    (POSIX_FADV_DONTNEED), which restores read again, as `random`;
  and each arm's working sets, recorded and mapped ahead (`SHARDS_WS_LOG`);
- the restore bench (n = 30) and the image bench (n = 60, 3 templates) in `base`,
  `pagewise` and `drop`, alternating, twice. The daemon passes the switches to its VM
  processes (`netproc::child_env`, which passes only SHARDS_LOG and SHARDS_TIMING
  otherwise).
