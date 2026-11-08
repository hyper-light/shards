# Allocation, anonymous RAM, CoW, and snapshot-save audit probes

The saved baseline uses revision **38457b709e67c4a43de87adfd7f2053531672dc2**.
If the current product APIs have changed, copy this harness directory into an
isolated checkout of that revision before reproducing or comparing the baseline.

Run from the repository root:

```sh
python3 docs/research/measurements/audit-memory/run.py --n 30
```

This standalone Cargo workspace calls the actual public `GuestMemory`, `Tree`, and
EROFS writer APIs. It modifies no product source, starts no VM, and needs no guest
kernel. Each condition runs in a fresh process. `results.json` retains the source and
binary SHA-256 values, host, OS, compiler, revision, relevant dirty source state,
allocation census, raw samples, and nearest-rank n/p50/p90/p99/max distributions.
The recorded run is on 2026-09-29, Apple M5 Max, Mac17,6, 128 GiB RAM, macOS 26.4.1,
Darwin 25.4.0, 16 KiB host pages, Rust 1.98.0, revision 38457b7.

The RAM experiments reserve or map 64 MiB using production APIs. A nonzero backing
file is written and synced before samples; its page cache is therefore warm. Read
probes load one byte per host page. Write probes read that byte and store its original
value: they change no logical data, but can still cause CoW. The sparse case touches
64 pages, one per MiB. A read-then-write case measures the later CoW faults separately
from the first read faults. The runner checks that private stores did not modify the
backing file. No system cache flush, memory pressure, guest execution, HVF mapping,
stage-2 faults, or Firecracker comparison is involved.

Snapshot probes call actual `GuestMemory::save` with either completely untouched
anonymous RAM or one nonzero byte per MiB. RAM is isolated and single-threaded, so
the save's pause precondition holds. Samples record logical file length and allocated
filesystem blocks. A restore verifies one sampled byte per host page after each save,
outside the measurement. This is a RAM-scan/save measurement, excluding the complete
snapshot's CPU/device capture, generation publication and fsync. Sparse file size is
distinct from the anonymous memory materialized during the scan.

`getrusage` measures host minor/major-fault deltas. It does not count guest stage-2
faults. On macOS, `proc_pid_rusage(RUSAGE_INFO_V2)` measures the process's resident
size and physical-footprint deltas; neither is a fleet PSS/unique-memory measurement.
The Linux fallback reports RSS from `/proc/self/statm`, and reports footprint as zero
because this harness has no Linux equivalent for that field. Record host THP,
overcommit policy, filesystem and cgroup settings when repeating on Linux; the
recorded macOS numbers must not be transferred to KVM.

The image cases build 10,000 regular files in the root, then call the actual EROFS
writer with a reusable fill source and a byte-counting sink behind black-boxed trait
objects. The writer's byte count must equal its declared block count. Inputs include
0/512/4,095/4,096-byte files, 1,024-byte xattrs, and four replacements of the same
10,000 names. This isolates tree retention and writer allocation behavior; it omits
tar parsing, decompression, hashing, actual filesystem I/O and guest file access.
It does not measure a real OCI image's pull/build throughput. Replaced nodes retain
different source offsets but the same final names, sizes and xattrs; the writer still
emits only 10,001 reachable inodes. Repeated-layer retention is measured through the
same `Tree::insert` behavior used by layer application.

One untimed allocation census per case delegates to Rust's `System` allocator.
Timing samples disable the counters, leaving their flag check. `requested` sums
allocation sizes and realloc target sizes; `live` and `peak` are requested live heap
bytes within that phase. They exclude allocator metadata/size classes, native C
allocations, `mmap` RAM, page tables and hypervisor memory. Realloc counts do not prove
that the allocator moved the old bytes. The counters assume measured phases do not
free older allocations: names, borrowed trees, input files and accounting helpers
are prepared outside them. Warmups are discarded, and condition order is randomized
reproducibly between iterations. Other work on the host was not prohibited.

These probes establish costs and retention in current code. They do not implement an
optimization or establish an end-to-end speedup.
