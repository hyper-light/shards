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

## Result (2026-10-21, runs 39614418049 and 39614443940 on 7.0.0-azure; 39614431058 on 6.17)

A restored guest's memory file, resident MiB (n 20, p50 → max), shards against Firecracker:

| kernel | base | random | pagewise | FC |
|---|---|---|---|---|
| 7.0 (run 1) | 25.8 → 26.0 | 26.5 | **8.9** | 13.5 → 13.8 |
| 7.0 (run 2) | 26.1 → 26.2 | 26.0 → 26.3 | **9.1** | 13.3 → 14.0 |
| 6.17 | 9.0 | 9.0 | 8.9 | 11.6 |

- On 6.18+ (7.0) `base` balloons to 26 MiB (far past Firecracker's 14) — each read fault
  maps the whole 2 MiB page-cache folio as 512 base PTEs (`pmd 0` in smaps, so not a huge
  page, just 512 pages faulted at once). On 6.17 there are no large folios and `base` is
  already 9 MiB, beating Firecracker.
- **`random` (MADV_RANDOM on the restore mapping) does not help** (26 MiB): it disables
  readahead but `filemap_map_folio_range` still maps an already-cached large folio whole,
  and the folio is large because the save wrote the file in large contiguous runs moments
  before, leaving large folios in the page cache that the restore's private COW map reuses.
- **`pagewise` (the save writes the memory file a page at a time) fixes it**: 8.9–9.1 MiB
  on 7.0, beating Firecracker, and unchanged on 6.17. The page cache then holds the file in
  single-page folios, so a whole-folio fault maps one page. This is the version-agnostic
  fix (it builds on write(2) granularity, nothing kernel-version-specific), applied to the
  save path on Linux; its added save latency is measured before it lands.
- `drop` (evict the file from the page cache after the save, re-read on restore) was also
  tried; the comparison did not emit its restore row, so it is not relied on.
