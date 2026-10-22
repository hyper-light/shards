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

## Third experiment: dropped from the page cache from outside the VM process

The second experiment's `evict` arms dropped the memory file from the page cache in the VM
process, before its restore mapped it (POSIX_FADV_DONTNEED). The VM process's seccomp filter
refuses fadvise(2) (`seccomp refused syscall 221 in thread shards-vm`, x86_64's fadvise64),
so each such restore was killed and its guest never beat; the first experiment's `drop` arm
died of the same in the save. Now an `evict-` arm drops the host's page cache once a second
(`drop_caches`, by root) for as long as its bench runs (`arms.sh`), Firecracker's VMs' with
shards': each restore's faults find the file as a busy host leaves it, read again with the
folios readahead makes, not those its save wrote. Arms `shipped`, `random` and `whole`, warm
and `evict-`; two rounds of the comparison; the restore bench, three rounds warm and two
evicted; the image bench warm only (a pool's runs under the drops would time the drops).

Runs 39693491072, 39693526565 and 39693536834 (6dd8e985) and 39694508087 (e2160f7f), all on
7.0.0-1017-azure; their tables by `summarize.py`, in PM M291. With the page cache dropped,
`evict-shipped` held a restore's file at 7.8 to 9.7 MiB (max 9.8) and a fleet at 1.2 to 1.3
MiB a VM, under Firecracker's (1.7 to 1.9): readahead, reading the file again, makes no
folio that maps 26 MiB, and `evict-whole` held as little (8.0 to 9.7): the 26 MiB are the
save's own long writes' folios, still in the page cache. `evict-random` held 2.6 to 2.9 MiB
in 7 rounds of 8 and 0.8 MiB a VM, each fault mapping its page alone; warm, `random` held
what `shipped` did, and restored 5 to 17 ms slower at p50 in the fourth run, mixed in
the other three.

## Second experiment (2026-10-21): the write path shards ships, warm

shards writes the memory file a page at a time on Linux (ffe3d1cb) and maps a restore's
memory MADV_NOHUGEPAGE alone. The first experiment's winning arm also mapped it
MADV_RANDOM, and every comparison run after the fix landed on a 6.17 runner, which maps no
folio whole, so the shipped path was not yet measured where the fault it answers happens.
Arms: `shipped` (as shards is), `random` (MADV_RANDOM too), `whole` (each run written whole,
as before the fix: the control); two rounds of the comparison, three of the benches,
alternating. Runs 39689552590 and 39689498647 on 7.0.0-1017-azure (AMD EPYC 9V45),
39689540483 on 6.17.0-1022-azure; tables by `summarize.py` from their logs.

A restored guest's memory file, resident MiB, p50 / max of each round; a 16-VM fleet's PSS
a VM (shards / Firecracker); the first beat, p50 ms (shards / Firecracker):

| run | arm | memory file | FC | fleet PSS a VM | first beat |
|---|---|---|---|---|---|
| 7.0 A | shipped | 9.2 / 9.3; 9.2 / 9.3 | 13.8, 13.9 | 1.2 / 1.9 | 10.90 / 10.48; 10.44 / 10.39 |
| | random | 9.1 / 9.1; 9.2 / 9.3 | 13.9, 13.8 | 1.2 / 1.9 | 11.60 / 10.46; 10.60 / 10.48 |
| | whole | 26.2 / 26.3; 26.1 / 26.2 | 13.6, 13.8 | 2.3 / 1.9 | 10.36 / 10.47; 11.71 / 10.72 |
| 7.0 B | shipped | 9.1 / 9.4; 9.2 / 9.3 | 13.6, 13.6 | 1.2 / 1.9 | 12.35 / 13.21; 12.66 / 13.51 |
| | random | 9.2 / 9.4; 9.2 / 9.4 | 14.0, 13.7 | 1.2 / 1.9 | 13.12 / 12.44; 13.92 / 12.41 |
| | whole | 26.0 / 26.1; 26.3 / 26.3 | 13.3, 14.0 | 2.3 / 1.9 | 12.05 / 15.10; 13.28 / 14.20 |
| 6.17 | shipped | 9.0 / 9.1; 8.9 / 9.0 | 11.7, 11.6 | 1.2 / 5.7 | 12.25 / 13.06; 13.04 / 14.23 |
| | random | 9.0 / 9.0; 8.9 / 9.0 | 11.8, 11.7 | 1.2 / 5.7 | 13.36 / 13.39; 13.30 / 13.72 |
| | whole | 9.0 / 9.1; 9.0 / 9.0 | 11.6, 11.6 | 1.2 / 5.7 | 11.84 / 13.25; 11.87 / 13.85 |

The benches, p50 of each round (ms; the restore bench's warm peak RSS, MiB):

| run | arm | cold restore | warm request | warm peak RSS | pooled run |
|---|---|---|---|---|---|
| 7.0 A | shipped | 22.88, 17.13, 23.34 | 16.65, 14.84, 19.62 | 16.3, 15.9, 15.9 | 29.94, 27.34, 33.93 |
| | random | 23.04, 14.25, 36.07 | 18.62, 11.03, 33.26 | 16.7, 16.2, 16.1 | 30.19, 34.50, 34.55 |
| | whole | 16.80, 10.08, 13.37 | 14.24, 6.79, 10.44 | 30.6, 31.4, 30.1 | 26.73, 36.31, 27.61 |
| 7.0 B | shipped | 19.16, 22.60, 16.15 | 16.27, 18.53, 12.17 | 15.3, 15.4, 15.1 | 34.71, 43.35, 44.27 |
| | random | 22.79, 18.74, 23.67 | 19.54, 14.86, 20.38 | 15.3, 15.8, 15.8 | 42.28, 37.01, 45.03 |
| | whole | 16.67, 15.89, 23.09 | 13.49, 12.63, 19.33 | 29.5, 31.5, 30.0 | 45.06, 42.15, 41.39 |
| 6.17 | shipped | 24.39, 17.38, 21.80 | 22.69, 15.74, 19.83 | 14.9, 15.0, 15.2 | 42.94, 36.11, 42.21 |
| | random | 36.42, 13.75, 23.14 | 34.26, 15.57, 24.09 | 14.9, 14.7, 15.4 | 45.77, 42.05, 36.04 |
| | whole | 24.71, 37.36, 29.71 | 23.31, 35.95, 27.62 | 15.0, 15.9, 15.4 | 39.92, 40.15, 41.55 |

- **The shipped path holds on 7.0** (its page cache warm from the save): 9.1–9.2 MiB, max
  9.4, against `whole`'s 26.0–26.3 and Firecracker's 13.3–14.0; a fleet at 1.2 MiB of PSS
  a VM against 2.3 and Firecracker's 1.9; the restore bench's peak RSS halved. MADV_RANDOM
  adds nothing here: with the file in the page cache there is nothing to read ahead.
- On 7.0 a restore's faults map `whole`'s 2 MiB folios at once, which made its cold
  restores and warm requests quicker than `shipped`'s in run A (cold restores by 6 to 10 ms
  at p50, warm requests by 2 to 9) and not in run B; pooled runs and first beats are the
  same within the rounds' spread. The
  shipped path restores as 6.17 does, whose faults map no folio whole.
- On 6.17 the arms do not differ: every arm holds 9 MiB.

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
