# What a VM process's boot keeps of its own binary (D36, Envelope row 11)

**Question.** In CI's Firecracker comparison, shards-vm's file-backed resident memory at boot
(`rss_file`, `pss_file`) is above Firecracker's, which fails the Envelope. Which cut closes
it, and at what cost?

**What CI's runs say.** From the firecracker job's logs: each VMM's mappings of its own
binary at boot (`describe_mappings`, `maps.py`), in bytes resident/mapped, and the Envelope's
paired differences (exact samples since 0080a20e5). `ci.txt` keeps both for the three runs
(each log's second boot is the THP advice step's).

| Revision | Runner CPU | shards-vm text | its read-only segment | shards-vm file-backed | Firecracker file-backed | rss_file, pss_file: shards less Firecracker |
|---|---|---|---|---|---|---|
| 1793da7c2, before 13fc49770 | AMD EPYC 9V74 | 2,240,512 / 2,371,584 | 196,608 / 356,352 | 2,437,120 | 2,338,816 | +0.1, +0.1 MiB (samples then to 0.1 MiB) |
| b18952ae6, after | Intel Xeon 6975P-C | 2,240,512 / 2,371,584 | 196,608 / 319,488 | 2,437,120 | 2,338,816 | +0.0898438, +0.09375 MiB |
| 8d6029e4d, after | AMD EPYC 7763 | 2,240,512 / 2,371,584 | 196,608 / 319,488 | 2,437,120 | 2,338,816 | +0.0898438, +0.09375 MiB |

- 13fc49770's packed relocations took 36,864 bytes from the read-only segment, whose
  resident bytes stayed 196,608. Its text stayed 2,371,584 bytes to the byte: the GPU
  device's gate took out nothing fat LTO had kept.
- In 19 earlier runs' logs (lane release-rows) the read-only segment kept 196,608 bytes
  however it grew, and the text all but 128 KiB of itself in most.
- A boot keeps the same pages each time, so each pair's difference is one value and the
  bootstrap interval a point: any gap above zero is "worse" (scripts/bench-envelope).
  shards-vm must keep at most Firecracker's 2,338,816 bytes: 98,304 fewer, 24 pages.

**Why whole windows.** A read fault on a file mapping maps the cached, up-to-date pages of
the 64 KiB around it, aligned by address and clipped to the mapping (Linux 7.2-rc4
mm/memory.c `do_fault_around`; `fault_around_pages` 16; debugfs `fault_around_bytes`, at one
page no fault-around). The binary is in the page cache as the bench runs it, so each 64 KiB
window a boot touches is mapped whole. A cut takes whole windows: 2 (32 pages) take
shards-vm 8 pages under Firecracker. 1 (16 pages) leaves it 8 over.

**The measurement that decides the cut** (ROOT's, R14). `run.sh OUT windows`, on CI's runner
(`gh workflow run vm-text.yml --ref main -f run=windows`): the comparison with fault-around
at the host's size, then at one page, where each VMM's text was taken to keep only the pages
its boot touches; it keeps the page cache's folios its boot touches (The order's A/B, below).
Both VMMs, boot and restore, n = 30 a setting.
- The boot's touched pages, laid out together, would take
  ceil(touched bytes / 64 KiB) windows of text. If that is at least 2 fewer than now, the
  cut is the boot path ordered first (LLD's `--symbol-ordering-file`, the functions on the
  touched pages in their order), which costs no instruction.
- If it is not, ordering cannot take 2 windows, and the cut is text size:
  `-C opt-level=s` for the musl helpers, whose speed `to_ready`, `to_beat` and the restore
  bench weigh.

**What it decided** (run 44031672087 at 2893961893, AMD EPYC 9V74, 2026-11-17; its `maps.txt`
in `windows.txt`). With fault-around at one page shards-vm's boot kept 490 of its text's
579 pages and 48 of its read-only segment's; Firecracker's 196 of 617 and 33. Laid out
together the 490 take 31 windows, 32 as the text's start falls, where the boot keeps 2,240,512
bytes now: the cut is the boot path ordered first.

| Fault-around | shards-vm text | its read-only segment | Firecracker text | its read-only segment |
|---|---|---|---|---|
| host's, 64 KiB | 2,240,512 / 2,371,584 | 196,608 / 319,488 | 2,002,944 / 2,527,232 | 262,144 / 446,464 |
| one page | 2,007,040 / 2,371,584 | 196,608 / 319,488 | 802,816 / 2,527,232 | 135,168 / 446,464 |

**The A/B** (ROOT's). `run.sh OUT ab ROUNDS REF` (`gh workflow run vm-text.yml --ref main -f
run='ab 2 REF'`): arm A as the dispatched revision builds it, arm B as git revision REF does (a
git worktree), each in a target directory of its own, then the rounds of both alternate,
n = 30 pairs a run. Each keeps each arm's sections (`readelf -lSdW`, `size -A`), each run's
samples, stdout, stderr and envelope, and `maps.txt`.

**The first A/B measured nothing** (run 44076339609 at fc9f30ccc, AMD EPYC 9V74,
2026-11-17; `ab-44076339609.txt`). Its arm B was to be linked by its order through
`CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS`, which the nested build of the VM process
never read: Cargo sets `CARGO_ENCODED_RUSTFLAGS` for a build script, which crates/shards/
build.rs's nested cargo inherits, and the first of "four mutually exclusive sources of extra
flags", "checked in order, with the first one being used", is `CARGO_ENCODED_RUSTFLAGS`
(Cargo's configuration reference, `build.rustflags`; its environment variables reference).
So B built as A: every section of its VM process the same size, and every boot of both
arms 2,240,512 bytes of text resident. Its order, 2,923 symbols from A's 490 touched pages,
was made; no arm linked by it. Arms are trees since.

**The order's A/B** (run 44117236848 at 9581374, Intel Xeon 6975P-C, Linux 7.0.0-1014-aws,
2026-11-17, 2 rounds an arm; `ab-44117236848.txt`). B, linked by an order of 2,963 symbols,
read better than A and still worse than Firecracker: `rss_file` +0.03125 MiB (A +0.09375),
`pss_file` +0.0351562 (+0.0976562), `overhead` -0.046875 (+0.0117188), each one value a pair,
`to_ready` and `to_beat` alike. Its boot kept 532 of its text's 580 pages, A's 548,
Firecracker 489 of 617.
- *Why so little.* A's boot with fault-around at one page (`touched`) kept 507 of its text's
  580 pages, in 8 runs, and 4 of its 7 gaps were 16 pages long, each from a 64 KiB boundary
  of the file: page 77 (the text's first, file offset 0x4d000) plus 115, 227, 259 or 387, a
  multiple of 16. Fault-around's windows lie on the mapping's addresses, a page off the
  file's here (the text's p_vaddr 0x4e4e0 to its p_offset 0x4d4e0). These lie on the file's:
  they are the page cache's large folios, each mapped whole by the fault that touches it
  (Linux 7.2-rc4 mm/memory.c `finish_fault`: "Now we can set mappings for the whole large
  folio"), whatever fault-around is set to. So `touched` counted folios, not pages; order.py,
  taking every function on a touched folio, ordered nearly all of them; and B kept the 64 KiB
  blocks its boot touched as A had.
- *What it leaves unread.* Which 4 KiB pages a boot runs: every reading so far was of the
  binary's page cache in 64 KiB folios, which fault-around and `finish_fault` each map whole.
- *The cure* (D36). shards-vm holds its own program's never-written segments under a
  userfaultfd from its start (`FaultAroundOff::over_this_program`), so that a fault there maps
  one page: `finish_fault` maps one page of a range a userfaultfd holds, and
  `should_fault_around` none around it (`uffd_disable_fault_around`). What its start mapped
  before that is let go of (MADV_DONTNEED). No list: what it keeps is what it runs, on every
  Linux host from 6.7, x86_64 and arm64 alike.
- *Its A/B* (owed, ROOT's): `gh workflow run vm-text.yml --ref main -f run='ab 2 REV'`, REV the
  main before the cure, whose arm B is A without it.

