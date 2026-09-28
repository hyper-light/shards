# Benchmarks

Every performance claim cites a run recorded here, from a committed harness (CLAUDE.md).

## Boot (`crates/shards/benches/boot.rs`)

`cargo bench -p shards --bench boot [-- --runs N --cpus N --memory MIB]`

Method:
- Each sample is a fresh `shards vm run` process that boots the pinned guest kernel with
  `shards-init` as PID 1, which powers off at once.
- The host page cache is warm: three warm-up boots are discarded first.
- Samples run sequentially, and percentiles are nearest-rank.

| Phase | Measured from | Measured to |
|---|---|---|
| `vmm_setup` | VMM `main` | boot vCPU enters the guest |
| `kernel` | guest entry | PID 1 writes its first marker |
| `to_init` | VMM `main` | PID 1 starts |
| `to_exit` | VMM `main` | the guest powers off |
| `spawn_to_exit` | host wall clock, process spawn | process reaped (includes exec, dyld, teardown) |
| `peak_rss` | — | `ru_maxrss` from wait4(2) |

`peak_rss` includes the guest memory the VMM process touched. It is not the VMM's own
overhead.

### Runs

**2026-09-28** · a8bd94a plus the uncommitted harness · Apple M5 Max (Mac17,6) · macOS 26.4.1
(25E253) · kernel vmlinux-6.18.48-aarch64 (Firecracker CI) · n=30, 1 vCPU, 256 MiB

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| vmm_setup | 2545 µs | 2756 µs | 3480 µs | 3480 µs |
| kernel | 18156 µs | 18613 µs | 18767 µs | 18767 µs |
| to_init | 20617 µs | 21348 µs | 22133 µs | 22133 µs |
| to_exit | 20799 µs | 21517 µs | 22302 µs | 22302 µs |
| spawn_to_exit | 25076 µs | 26327 µs | 27434 µs | 27434 µs |
| peak_rss | 59.2 MiB | 59.2 MiB | 59.2 MiB | 59.2 MiB |

**2026-09-28 A/B: copy the kernel image vs map it copy-on-write** · same host and OS · n=30
per round, 3 alternating rounds per mode (six benchmark runs)

"map" replaced guest RAM under the image with a `MAP_PRIVATE` mapping of the kernel
file, so the guest faulted pages in from the page cache. "copy" is the pread(2) into
anonymous guest RAM that shards uses.

| Mode | vmm_setup p50 | kernel p50 | to_init p50 | worst to_init | peak_rss |
|---|---|---|---|---|---|
| copy | 2508–2533 µs | 17641–18337 µs | 20213–20794 µs | 24257 µs | 59.2 MiB |
| map | 829–859 µs | 20065–20280 µs | 20934–21139 µs | **149306 µs** | 40.7 MiB |

Mapping moved more time into the guest's first touches than it saved on the host. Its
to_init p50 was 0.1–0.9 ms worse. Two of 90 mapped boots stalled for 62 ms and 149 ms in
the kernel phase; none of 90 copied boots did. It saved 18.5 MiB of RSS. **Decision:** copy.
An interleaved rerun (200 + 200 boots, platform-measurements.md M16) confirmed it. Mapped
boots stalled up to 1.06 s (5 of 200); copied boots never did (max 23.6 ms). M15 found no
such tail in 6.3 M in-process faults. The stall comes from fresh processes mapping the
file, so D7 maps snapshot memory before the request arrives.

## Restore (`crates/shards/benches/restore.rs`)

`cargo bench -p shards --bench restore [-- --runs N --cpus N --memory MIB]`

Method:
- One snapshot is taken of the `resume` test guest. It asks for the snapshot and, as its
  first act afterwards, writes the RESUMED marker, then powers off.
- The snapshot is restored many times, alternating cold and warm samples after three
  warm-up pairs:
  - **cold**: a fresh `shards vm restore` process per sample
  - **warm**: `shards vm restore --hold` prepares everything, then receives its start
    request (a line on stdin)

| Phase | Measured from | Measured to |
|---|---|---|
| `cold_restore` | VMM `main` | the RESUMED marker (guest running again) |
| `cold_spawn_exit` | spawn | reap (host wall clock around the whole process) |
| `warm_request` | the release (the start request) | the RESUMED marker |
| `warm_peak_rss` | — | `ru_maxrss`, including touched guest memory |

A warm VMM has already mapped snapshot memory, created the VM, GIC and vCPUs, loaded
vCPU state, and restored the distributor and devices (D2, D14). A start request only
releases the vCPUs; the counter offset is taken at release, so the guest sees no time
jump.

"Guest running again" is not yet "usable". The ≤5 ms target counts until the in-VM
engine acknowledges over vsock; that part is to be measured.

### Runs

**2026-09-28** · f190071 plus the uncommitted harness · Apple M5 Max (Mac17,6) · macOS
26.4.1 (25E253) · kernel vmlinux-6.18.48-aarch64 · n=50, 1 vCPU, 256 MiB

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| cold_restore | 744 µs | 937 µs | 1195 µs | 1195 µs |
| cold_spawn_exit | 5574 µs | 6083 µs | 7378 µs | 7378 µs |
| **warm_request** | **17 µs** | **20 µs** | **35 µs** | **35 µs** |
| warm_peak_rss | 13.4 MiB | 13.4 MiB | 13.4 MiB | 13.4 MiB |

**2026-09-28, with VMGenID** · 747b215 plus VMGenID (uncommitted) · same host, OS, kernel
and parameters

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| cold_restore | 939 µs | 1153 µs | 1378 µs | 1378 µs |
| cold_spawn_exit | 4632 µs | 5310 µs | 5889 µs | 5889 µs |
| **warm_request** | **157 µs** | **173 µs** | **209 µs** | **209 µs** |
| warm_peak_rss | 12.5 MiB | 12.5 MiB | 12.6 MiB | 12.6 MiB |

A restored clone now handles the VMGenID interrupt and reseeds its RNG before its first
user instruction: about 140 µs of guest work on the request path. Without the reseed,
clones of one snapshot share RNG state (the E2E test shows identical `getrandom` output),
so it stays. Next step for the warm pool: release warm VMs ahead of the request, so they
reseed and idle in-kernel (WFI costs no exits, PM M7), and make the request a wakeup.
