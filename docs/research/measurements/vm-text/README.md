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
at the host's size, then at one page, where each VMM's text keeps only the pages its boot
touches. Both VMMs, boot and restore, n = 30 a setting.
- The boot's touched pages, laid out together, would take
  ceil(touched bytes / 64 KiB) windows of text. If that is at least 2 fewer than now, the
  cut is the boot path ordered first (LLD's `--symbol-ordering-file`, the functions on the
  touched pages in their order), which costs no instruction.
- If it is not, ordering cannot take 2 windows, and the cut is text size:
  `-C opt-level=s` for the musl helpers, whose speed `to_ready`, `to_beat` and the restore
  bench weigh.

**What it decided** (run 44031672087 at 2893961893, AMD EPYC 9V74, 2026-11-17; its `maps.txt`
in `windows.txt`). With fault-around at one page shards-vm's boot touched 490 of its text's
579 pages and 48 of its read-only segment's; Firecracker's 196 of 617 and 33. Laid out
together the 490 take 31 windows, 32 as the text's start falls, where the boot keeps 2,240,512
bytes now: the cut is the boot path ordered first.

| Fault-around | shards-vm text | its read-only segment | Firecracker text | its read-only segment |
|---|---|---|---|---|
| host's, 64 KiB | 2,240,512 / 2,371,584 | 196,608 / 319,488 | 2,002,944 / 2,527,232 | 262,144 / 446,464 |
| one page | 2,007,040 / 2,371,584 | 196,608 / 319,488 | 802,816 / 2,527,232 | 135,168 / 446,464 |

**The A/B** (ROOT's). `run.sh OUT order ROUNDS` (`gh workflow run vm-text.yml --ref main -f
run='order 2'`): arm A as main builds it, its comparison with fault-around at one page, its
boot's pages of shards-vm (the bench's `pages.tsv`, `pages_to`) made an ordering file
(`order.py`), arm B linked by it, each in a target directory of its own, then the rounds of
both, their order alternating, n = 30 pairs a run. `run.sh OUT ab ROUNDS` does the same for
any `B_RUSTFLAGS` (`-f run='ab 2' -f b_rustflags='-C opt-level=s'`). Each keeps each arm's
sections (`readelf -lSdW`, `size -A`), each run's samples, stdout, stderr and envelope, and
`maps.txt`. B's flags reach the init's musl build too, whose memory the comparison does not
count.
