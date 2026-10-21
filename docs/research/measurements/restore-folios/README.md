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

`.github/workflows/restore-folios.yml` applies `folios.patch`, whose switches change how the
memory file is written and mapped, and runs on a GitHub runner (ext4) the Firecracker
comparison (`benches/firecracker.rs`, n = 20 an arm a round) and the restore (n = 30) and
image (n = 60, 3 templates) benches, the arms alternating. Each arm's whole output is kept;
its rows are printed, or its last lines where it fails, and the other arms go on. The
daemon passes the switches to its VM processes (`netproc::child_env`, which passes only
SHARDS_LOG and SHARDS_TIMING otherwise).

## Second experiment: the write path shards ships

shards writes the memory file a page at a time on Linux (ffe3d1cb) and maps a restore's
memory MADV_NOHUGEPAGE alone. The first experiment's winning arm also mapped it
MADV_RANDOM, and every comparison run since the fix landed on a 6.17 runner, which maps no
folio whole, so the shipped path was not yet measured where the fault it answers happens.
The arms, two rounds of the comparison and three of the benches:

- `shipped`: as shards is;
- `random`: its restores mapped MADV_RANDOM too;
- `whole`: each run written whole, as before the fix: the control, which shows the
  fault on a runner that has it;
- `evict`: the file written back and dropped from the page cache (POSIX_FADV_DONTNEED,
  of the pages no other process maps) before each restore maps it, as a busy host drops
  it: the restore reads it again, readahead making its folios;
- `evict-random`: both.

Results: pending (runs dispatched 2026-10-21).

## First experiment (2026-10-21, runs 39614418049 and 39614443940 on 7.0.0-azure; 39614431058 on 6.17)

Arms: `base` (as shards was: runs written whole); `random` (restores mapped MADV_RANDOM);
`pagewise` (a page a write **and** restores mapped MADV_RANDOM: both switches); `drop` (the
file written back and dropped after the save, restores as `random`). The `drop` arm's
restores failed in every run, and the bench step stopped at its failure (its `grep` found
no row), so the benches ran one round, not the two intended, and `drop`'s error was not
kept.

A restored guest's memory file, resident MiB (p50 → max), shards against Firecracker:

| kernel | base | random | pagewise | FC |
|---|---|---|---|---|
| 7.0 (run 1) | 25.8 → 26.0 | 26.5 | **8.9** | 13.5 → 13.8 |
| 7.0 (run 2) | 26.1 → 26.2 | 26.0 → 26.3 | **9.1** | 13.3 → 14.0 |
| 6.17 | 9.0 | 9.0 | 8.9 | 11.6 |

- On 7.0 `base` balloons to 26 MiB, past Firecracker's 14: each read fault maps the whole
  page-cache folio, up to 2 MiB, as base PTEs (`pmd 0` in smaps). On 6.17 ext4 makes the
  same large folios (6.16+), but a fault maps at most fault-around's 64 KiB of them, and
  `base` holds 9 MiB, under Firecracker's.
- `random` does not help (26 MiB): it stops readahead, but the folios are large because
  the save wrote the file in long runs moments before, and the restore's faults find them
  in the page cache.
- `pagewise` holds 8.9–9.1 MiB on 7.0 and changes nothing on 6.17. Which of its two
  switches does it was not separated; the second experiment separates them.

A restore's first beat (p50, ms), shards / Firecracker: on 7.0 run 1 base 11.34 / 11.45,
pagewise 10.89 / 10.74; run 2 base 12.98 / 13.85, pagewise 12.45 / 13.59; on 6.17 base
10.13 / 10.49, pagewise 10.59 / 10.54: within 5% of `base`.

The benches, one round, `base` → `pagewise` (p50, ms; peak RSS MiB):

| kernel | cold restore | warm request | warm peak RSS | pooled run |
|---|---|---|---|---|
| 7.0 (run 1) | 22.56 → 16.14 | 20.19 → 13.28 | 33.0 → 15.3 | 34.26 → 29.42 |
| 7.0 (run 2) | 24.24 → 29.50 | 20.25 → 23.88 | 30.0 → 15.2 | 48.96 → 46.77 |
| 6.17 | 23.29 → 27.66 | 22.25 → 27.98 | 15.2 → 15.4 | 26.04 → 26.08 |

Mixed: on two runs of three `pagewise` restored 18–26% slower at p50, where its restores
read with no readahead (MADV_RANDOM); on the other, faster. One round each, on shared
runners: the second experiment repeats them.
