# Working-set decoder and initramfs allocation probe

The saved baseline uses revision **38457b709e67c4a43de87adfd7f2053531672dc2**.
If the current product APIs have changed, copy this harness directory into an
isolated checkout of that revision before reproducing or comparing the baseline.

Run from the repository with `python3 docs/research/measurements/audit-working-set/run.py`.
The probe calls the current public `snapshot::write_working_set` and
`snapshot::read_working_set` functions, using aligned, distinct 16 KiB Touch entries
and a byte/page bound equal to the set's page count. It needs neither a VM nor a
kernel artifact. It uses a unique temporary generation directory and removes it.

For each of 1, 718, 3,867 and 65,536 pages, it records 30 allocation samples and
30 timing samples after one preliminary cached read. Timings have counters disabled.
Allocation samples record successful Rust System allocator calls, reallocations,
total requested bytes, peak requested live bytes, and bytes retained by the returned
Touch vector. Native C allocations and mmap memory are excluded. Requested capacity
is not allocator fragmentation, resident memory, private physical memory, or PSS.
The allocation counter assumes this single-threaded successful decoder path does
not drop an allocation made before the measurement interval; input/test values stay
alive throughout the interval. It also measures actual Touch and arm64 VcpuState
inline layouts, excluding the latter's heap vectors.

The same executable then measures `initramfs::with_init` for 256-byte and 1 MiB
synthetic payloads, with 30 allocation samples and 30 separate timing samples each.
Each archive is compared byte for byte with an initial reference construction
outside the measured interval. Returned archive length and capacity are recorded;
payload and reference allocations are excluded. This isolates CPIO construction,
excluding host file reads and copying the resulting archive into guest memory.

`samples.json` stores raw samples and host, OS, revision and dirty-state metadata;
`summary.json` records nearest-rank n/p50/p90/p99/max timings. This measures decoder
cost on a warm local filesystem, not guest-page faults, VM restore latency, a
completed optimization, or a comparison with Firecracker. `assert_eq!` checks every
decoded set against its original before accepting the sample. Assertion code is
outside the timing and allocation intervals.

The runner also executes `src/bin/range-order.rs`, which gives the actual GuestMemory
API two accepted ranges in descending GPA order, writes distinct `L`/`H` tags,
saves and remaps the memory, and records the restored tags in `range-order.json`.
This is a correctness probe, not a timing sample. It asserts the saved file contains
the proper low/high tags and reports whether remapping preserves them. The current
VMM swaps them, so `correct` is false. Functional machine builders currently give
GuestMemory sorted ranges; this defect concerns the general library API.
The same probe seeds a target file with nonzero bytes, saves fresh memory whose
second page is entirely zero, and reports whether the restored second page is
zero. Currently its bytes remain 0xaa (`reused_file_correct` is false). The
generation publisher creates fresh target files, so that publishing path is
unaffected; the general save API does not enforce a fresh target.
