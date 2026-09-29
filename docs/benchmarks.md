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

**2026-09-28** · e8ec2e6 · same host and OS · kernel vmlinux-6.18.48-aarch64 · n=100,
1 vCPU, 256 MiB · about 1.5 of 18 cores busy with other work (a Docker Desktop VM and a
file-sync agent)

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| vmm_setup | 2660 µs | 2833 µs | 3122 µs | 3317 µs |
| kernel | 18558 µs | 19117 µs | 19312 µs | 19457 µs |
| to_init | 21242 µs | 21870 µs | 22181 µs | 22324 µs |
| to_exit | 21396 µs | 22031 µs | 22401 µs | 22482 µs |
| spawn_to_exit | 25580 µs | 26345 µs | 26663 µs | 27466 µs |
| peak_rss | 59.3 MiB | 59.3 MiB | 59.3 MiB | 59.3 MiB |

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

**2026-09-28** · e8ec2e6 · same host, OS and kernel · n=100, 1 vCPU, 256 MiB · the same
background load as the boot run above

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| cold_restore | 794 µs | 996 µs | 1725 µs | 2134 µs |
| cold_spawn_exit | 4201 µs | 4765 µs | 6450 µs | 7920 µs |
| **warm_request** | **149 µs** | **169 µs** | **197 µs** | **201 µs** |
| warm_peak_rss | 12.5 MiB | 12.5 MiB | 12.5 MiB | 12.5 MiB |

**2026-09-29, x86_64 Linux on KVM** · d21a89f (branch kvm-snapshots) · GitHub's
ubuntu-24.04 runner: AMD EPYC 9V45, 4 vCPUs, itself a VM under Microsoft's hypervisor, so
KVM runs nested · Linux 6.17.0-1022-azure · kernel vmlinux-6.18.48-x86_64 · n=30, 1 vCPU,
256 MiB · load 1.53 0.84 0.33

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| cold_restore | 24438 µs | 24650 µs | 24950 µs | 24950 µs |
| cold_spawn_exit | 63914 µs | 65956 µs | 69934 µs | 69934 µs |
| **warm_request** | **20025 µs** | **23832 µs** | **24026 µs** | **24026 µs** |
| warm_peak_rss | 33.1 MiB | 33.1 MiB | 33.1 MiB | 33.1 MiB |

The first restores on KVM. From its release a restored guest takes 20 ms to run again,
where on the Mac it takes 17 µs: the VMM's own part of a cold restore is the other 4 ms.
Where the 20 ms goes is not measured yet; this host nests KVM, which makes each of the
guest's first touches of its memory, 4 KiB at a time from the snapshot's file, costly.

## Run (`crates/shards/benches/run.rs`)

`cargo bench -p shards --bench run [-- --runs N]`

Method:
- The command is `/bin/testguest exit 0`, in the minimal image the E2E tests use
  (`workload_image` in tests/common).
- **cold**: `shards vm run --rootfs IMAGE -- COMMAND` boots the kernel into the image for
  every command.
- **warm**: templates are saved first, each booted with its image mounted (`vm run
  --rootfs --snapshot-dir`, D16); `--templates T` of them, default 5. Guest state differs
  between templates, and so can a restore's cost. Each sample restores the next template
  with `--hold`. The copy resumes and connects at once, and the request (a line on stdin)
  sends it the command.
- Cold and warm samples alternate, after three warm-up pairs.

| Phase | Measured from | Measured to |
|---|---|---|
| `cold_spawn_exit`, `warm_spawn_exit` | spawn | reap (host wall clock around the whole process) |
| **`warm_request`** | the request | shards has read the command's exit status |
| `cold_boot` | first guest entry | shards-init running |
| `warm_resume` | the release, when the VM starts | the guest running again (before the request) |
| `*_connect` | init running | connected to the host (cold: the image mounted first) |
| `*_spawn` | connected (warm: the request) | the command executing: workload received, user resolved, fork, exec |
| `*_command` | the command executing | its exit |
| `*_report` | its exit | init powering off: output drained, status sent and read |
| `*_power_off` | init powering off | the VM stopped |

Phases come from markers shards-init writes to the control page (the VMM's clock).

### Runs

**2026-09-28** · 97d8b7e plus templates and this harness (uncommitted) · Apple M5 Max
(Mac17,6) · macOS 26.4.1 (25E253) · kernel Image-6.18.48-aarch64-1bff175d35cb · n=30,
1 vCPU, 256 MiB

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| **warm_request** | **1988 µs** | **2162 µs** | **2233 µs** | **2233 µs** |
| warm_resume | 271 µs | 290 µs | 325 µs | 325 µs |
| warm_connect | 513 µs | 565 µs | 579 µs | 579 µs |
| warm_spawn | 803 µs | 878 µs | 893 µs | 893 µs |
| warm_command | 11 µs | 12 µs | 14 µs | 14 µs |
| warm_report | 103 µs | 116 µs | 139 µs | 139 µs |
| warm_power_off | 290 µs | 319 µs | 341 µs | 341 µs |
| warm_spawn_exit | 7076 µs | 7799 µs | 13497 µs | 13497 µs |
| warm_peak_rss | 17.2 MiB | 17.3 MiB | 17.3 MiB | 17.3 MiB |
| cold_spawn_exit | 34825 µs | 43064 µs | 46106 µs | 46106 µs |
| cold_boot | 15712 µs | 25487 µs | 26328 µs | 26328 µs |
| cold_connect | 9780 µs | 9822 µs | 9830 µs | 9830 µs |
| cold_spawn | 187 µs | 9900 µs | 9998 µs | 9998 µs |
| cold_command | 137 µs | 172 µs | 201 µs | 201 µs |
| cold_report | 71 µs | 101 µs | 163 µs | 163 µs |
| cold_power_off | 137 µs | 152 µs | 173 µs | 173 µs |

That run restored one template, which turned out to be a fast one, and it counted from
the release to the VM's stop. The run below restores six templates, as the harness now
does.

**2026-09-28** · 229007d plus signals, `noautogroup` and warm pre-release (uncommitted) ·
same host, OS and kernel · n=30 over 6 templates, 1 vCPU, 256 MiB

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| **warm_request** | **1040 µs** | **1422 µs** | **2363 µs** | **2363 µs** |
| warm_spawn | 795 µs | 1057 µs | 2170 µs | 2170 µs |
| warm_command | 172 µs | 193 µs | 1162 µs | 1162 µs |
| warm_report | 115 µs | 148 µs | 196 µs | 196 µs |
| warm_power_off | 281 µs | 309 µs | 336 µs | 336 µs |
| warm_resume (before the request) | 9343 µs | 9424 µs | 9534 µs | 9534 µs |
| warm_connect (before the request) | 472 µs | 527 µs | 551 µs | 551 µs |
| warm_spawn_exit | 15614 µs | 16596 µs | 18435 µs | 18435 µs |
| warm_peak_rss | 17.3 MiB | 17.8 MiB | 17.8 MiB | 17.8 MiB |
| cold_spawn_exit | 32984 µs | 33632 µs | 34080 µs | 34080 µs |
| cold_boot | 15823 µs | 25450 µs | 25614 µs | 25614 µs |
| cold_connect | 9768 µs | 9829 µs | 9874 µs | 9874 µs |
| cold_spawn | 145 µs | 253 µs | 272 µs | 272 µs |

A request to a warm VM is answered in 1.0 ms at the median and 2.4 ms at p99: the
command's delivery, a fork and exec, its run, and its exit status back. The VM resumed,
reseeded and connected before the request, in the warm pool's time.

**Stalls of one guest tick.** The guest kernel runs at `CONFIG_HZ=100`. Several phases
stall for up to one tick, and whether they do depends on the template, since each
restore of a template repeats its guest's state:
- **setsid, fixed.** Markers inside the forked child placed a spawn stall of 7.6 to
  7.9 ms in `setsid(2)`, in 8 of 11 templates. With `CONFIG_SCHED_AUTOGROUP`, setsid
  creates a scheduler group and moves the caller into it. Booting with `noautogroup`, no
  template stalled there (0 of 6). shards now boots images that way; Docker's
  containers never get automatic groups either, as they live in cgroups
  (`task_wants_autogroup`, kernel/sched/autogroup.c at v6.18).
- **Resume, fixed.** In most templates, the release was followed by about 9.3 ms before
  init's next instruction. Warm VMs resumed before their request, so the stall cost the
  warm pool, not the request.
  - An early bisect blamed the signal forwarder, but over 8 templates the stall came and
    went without it.
  - Letting the template idle for 50 ms before its snapshot removed the stall (0 of 8).
  - The cause: the kernel's crypto self-tests were still running when the template was
    saved. Every copy replayed the rest, and this `PREEMPT_NONE` kernel gave them the CPU
    until a tick (platform-measurements.md M21).
  - shards-init now waits for the self-tests before the snapshot. The run after this list
    has no resume stall.
- **Marker writes, fixed.** In the first version, shards-init mapped and unmapped
  `/dev/mem` for every marker, and restored guests then stalled twice per run. Mapping
  the control page once removed those stalls.
- Cold, open. `cold_connect` (mounting the image) waits one tick at the median, and boot
  and spawn do at p90. With `rcupdate.rcu_expedited=1` on the kernel command line,
  `cold_connect` fell from 9758 to 651 µs (p50, n=10), so the mounts wait for an RCU
  grace period, which a 100 Hz kernel completes on its tick. The p90 stalls remained.
  Tuning the tick rate and RCU for our kernel is still to be measured.

**2026-09-29** · 7966873 plus shards-init waiting for the crypto self-tests (uncommitted) ·
same host, OS and kernel · n=50 over 5 templates, 1 vCPU, 256 MiB · load 4.65 5.30 7.04

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| **warm_request** | **974 µs** | **1058 µs** | **8001 µs** | **8001 µs** |
| warm_spawn | 865 µs | 927 µs | 7871 µs | 7871 µs |
| warm_command | 19 µs | 21 µs | 25 µs | 25 µs |
| warm_report | 127 µs | 155 µs | 4142 µs | 4142 µs |
| warm_power_off | 297 µs | 323 µs | 338 µs | 338 µs |
| warm_resume (before the request) | 163 µs | 179 µs | 192 µs | 192 µs |
| warm_connect (before the request) | 342 µs | 375 µs | 633 µs | 633 µs |
| warm_spawn_exit | 6854 µs | 10401 µs | 13918 µs | 13918 µs |
| warm_peak_rss | 17.1 MiB | 17.1 MiB | 17.2 MiB | 17.2 MiB |
| cold_spawn_exit | 33159 µs | 35709 µs | 42331 µs | 42331 µs |
| cold_boot | 15732 µs | 25413 µs | 25976 µs | 25976 µs |
| cold_connect | 9780 µs | 9824 µs | 9842 µs | 9842 µs |
| cold_spawn | 138 µs | 274 µs | 419 µs | 419 µs |
| cold_command | 126 µs | 152 µs | 245 µs | 245 µs |
| cold_report | 69 µs | 107 µs | 6320 µs | 6320 µs |
| cold_power_off | 114 µs | 125 µs | 149 µs | 149 µs |

The resume stall is gone: `warm_resume` is 163 µs at p50, where it was 9343 µs. A warm
VM's whole process takes 6.9 ms, where it took 15.6 ms. One request of 50 still spent
7.0 ms in spawn and 4.0 ms in report, which is the p99 row.

## Image (`crates/shards/benches/image.rs`)

`cargo bench -p shards --bench image [-- --runs N --templates T]`

Method:
- The command is `exit 0`, in the image the E2E tests pull (`test_image` in tests/common),
  pulled once from a loopback registry into a fresh `SHARDS_HOME`.
- Every run goes through the daemon (architecture.md D26), which the first run starts.
- **cold**: `shards run --pull never IMAGE exit 0` with `SHARDS_KERNEL` and `SHARDS_INIT`
  set (before 2026-09-29's D27 commit, `--kernel K --init I`): the daemon boots a VM for
  every run.
- **template**: with the guest recorded (`shards guest use`), `shards run --pull never
  IMAGE exit 0` is served from the daemon's pool of warm VMs of the image's template
  (D25, D26).
  - Restores cost more for some templates than others, so samples come from
    `--templates T` templates (default 5), each saved afresh under a new daemon.
  - A template's save and its first two runs are not samples: since 2026-09-29's working
    sets, the pool restores those VMs before the save's run has recorded the working
    set the others prefetch (platform-measurements.md M30). Before, only the first run
    was left out.
- Cold and templated samples alternate, after three cold warm-up runs. Between runs the
  daemon refills its pool.
- Each run records the host's load averages as it ends.

| Phase | Measured from | Measured to |
|---|---|---|
| `run_cold`, `run_template` | the client's spawn | its reap (host wall clock around the client process) |
| `template_command` | the command sent to the guest | its exit status read (the VM's clock) |
| `template_outside` | — | the rest of the wall clock: the client launched, the request handed to a warm VM, the status back, the client gone |
| `*_rss` | — | the VM process's peak RSS when it answered, guest memory included |
| `client_rss` | — | the client process's `ru_maxrss`, from wait4(2) |

Runs before the daemon (up to 2026-09-29, 05f92d6) timed `shards run` as the VMM
process itself: `template_restore` ran from its `main` to the restored vCPUs,
`template_command` from there to the VM stopped, `template_process` was the rest of the
wall clock, and `*_rss` came from wait4(2).

### Runs

**2026-09-29** · 10d4b29 plus `shards run` templates (uncommitted) · Apple M5 Max
(Mac17,6) · macOS 26.4.1 (25E253) · kernel Image-6.18.48-aarch64-1bff175d35cb · n=300
over 10 templates, 1 vCPU, 256 MiB · load 3.45 5.66 7.77 (other VMs and builds on the
host)

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| run_cold | 34208 µs | 42328 µs | 53740 µs | 104750 µs |
| **run_template** | **16511 µs** | **24646 µs** | **38477 µs** | **203897 µs** |
| template_restore | 1710 µs | 5344 µs | 17202 µs | 174800 µs |
| template_command | 10840 µs | 12828 µs | 13967 µs | 15280 µs |
| template_process | 3951 µs | 7686 µs | 11997 µs | 16535 µs |
| run_cold_rss | 60.9 MiB | 61.0 MiB | 61.1 MiB | 61.2 MiB |
| run_template_rss | 19.5 MiB | 19.6 MiB | 19.7 MiB | 19.7 MiB |

A templated run takes half as long as a boot at the median. Most of what remains is in
the guest: in most templates, the command waits about one guest tick (10 ms at
`CONFIG_HZ=100`) after the restore. Outside the guest, launching and tearing down the
process takes about 4 ms at the median and restoring 1.7 ms, both with long tails on this
busy host.

**2026-09-29, templates saved after the kernel's crypto self-tests** · 7966873 plus
shards-init waiting for them (uncommitted) · same host, OS and kernel · n=300 over 10
templates, 1 vCPU, 256 MiB · load 3.79 5.50 7.63

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| run_cold | 34178 µs | 42790 µs | 52938 µs | 59774 µs |
| **run_template** | **7517 µs** | **19187 µs** | **29838 µs** | **34695 µs** |
| template_restore | 1723 µs | 7833 µs | 14967 µs | 19452 µs |
| template_command | 1904 µs | 4544 µs | 6564 µs | 7516 µs |
| template_process | 3886 µs | 7646 µs | 12122 µs | 15762 µs |
| run_cold_rss | 61.0 MiB | 61.0 MiB | 61.1 MiB | 61.2 MiB |
| run_template_rss | 18.5 MiB | 18.5 MiB | 18.5 MiB | 18.6 MiB |

The guest's part fell from 10.8 to 1.9 ms at the median (platform-measurements.md M21),
and the median templated run from 16.5 to 7.5 ms: 4.5 times faster than a boot. The tail
is now outside the guest. On this busy host, restoring and launching the process each
reach about 7.7 ms at p90.

**2026-09-29, through the daemon** · 6c84ec7 plus the daemon (uncommitted) · same host,
OS and kernel · n=300 over 10 templates, 1 vCPU, 256 MiB · load 6.04 6.43 5.88 (other
VMs and builds on the host)

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| run_cold | 34824 µs | 35633 µs | 36229 µs | 36668 µs |
| **run_template** | **5149 µs** | **5416 µs** | **5630 µs** | **5766 µs** |
| template_command | 1063 µs | 1159 µs | 1282 µs | 1336 µs |
| template_outside | 4073 µs | 4278 µs | 4550 µs | 4606 µs |
| run_cold_rss | 59.8 MiB | 59.8 MiB | 59.9 MiB | 60.0 MiB |
| run_template_rss | 16.4 MiB | 16.7 MiB | 16.8 MiB | 16.8 MiB |
| client_rss | 6.3 MiB | 6.3 MiB | 6.3 MiB | 6.3 MiB |

A pooled run's restore happens before its request, so its variance left the tail: p99
fell from 29.8 to 5.6 ms, and the median from 7.5 to 5.1 ms. The rest is mostly outside
the guest. `template_outside` is 4.1 ms at the median, and launching the `shards`
binary alone costs 3.5 ms (platform-measurements.md M23). A thin client, at 1.4 ms, is
the next step toward 5 ms at p99.

**2026-09-29, the thin client** · 27e60c0 plus the thin `shards` (uncommitted) · same
host, OS and kernel · n=300 over 10 templates, 1 vCPU, 256 MiB · load 3.15 3.67 4.64

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| run_cold | 33529 µs | 34196 µs | 35193 µs | 37982 µs |
| **run_template** | **3375 µs** | **3682 µs** | **3880 µs** | **3981 µs** |
| template_command | 1011 µs | 1162 µs | 1396 µs | 1433 µs |
| template_outside | 2352 µs | 2632 µs | 2762 µs | 2799 µs |
| run_cold_rss | 59.8 MiB | 59.8 MiB | 59.9 MiB | 59.9 MiB |
| run_template_rss | 16.5 MiB | 16.7 MiB | 16.8 MiB | 16.8 MiB |
| client_rss | 1.6 MiB | 1.6 MiB | 1.6 MiB | 1.6 MiB |

`shards` now links only the standard library and `shards_ipc`, and runs `shardsd` for
everything but `run`. The daemon's socket is named relative to its home, and a pool
refills after its VM takes a run (platform-measurements.md M26). A repeat at load 2.53
gave 3374, 3560, 3752 and 4218 µs.

With every CPU busy (`yes` on all 18, load 17.23), run_template took 4844, 16570, 31501
and 46617 µs, and run_cold 36663, 58335, 80146 and 89872 µs. User-interactive QoS on
the request path's service threads made no difference (M26).

**2026-09-29, containers read as the Docker CLI reads them** · f779d1b plus D27's
command lines, `run -d` and dockerd's words (uncommitted) · same host, OS and kernel ·
n=300 over 10 templates, 1 vCPU, 256 MiB · load 8.30 6.02 5.31

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| run_cold | 33971 µs | 34590 µs | 35231 µs | 36686 µs |
| **run_template** | **3518 µs** | **3727 µs** | **4121 µs** | **4814 µs** |
| template_command | 1003 µs | 1110 µs | 1299 µs | 1402 µs |
| template_outside | 2499 µs | 2684 µs | 2963 µs | 3694 µs |
| run_cold_rss | 60.0 MiB | 60.0 MiB | 60.0 MiB | 60.1 MiB |
| run_template_rss | 16.6 MiB | 16.6 MiB | 16.7 MiB | 16.7 MiB |
| client_rss | 1.7 MiB | 1.7 MiB | 1.7 MiB | 1.7 MiB |

The host's load was more than twice the last run's. An interleaved A/B against f779d1b
from one template (platform-measurements.md M29) puts this change's cost at 17 µs of
wall time at the median (95% [1, 37]), all of it outside the guest: most of the
difference from the last run is the host.

**2026-09-29, working sets** · a0f7926 plus working-set recording and prefetch
(uncommitted) · same host, OS and kernel · n=300 over 10 templates, 1 vCPU, 256 MiB ·
load 8.53 8.84 7.03

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| run_cold | 34802 µs | 35875 µs | 36910 µs | 37458 µs |
| **run_template** | **2973 µs** | **3218 µs** | **3447 µs** | **6703 µs** |
| template_command | 366 µs | 425 µs | 487 µs | 1721 µs |
| template_outside | 2598 µs | 2831 µs | 3080 µs | 4982 µs |
| run_cold_rss | 60.0 MiB | 60.0 MiB | 60.1 MiB | 60.1 MiB |
| run_template_rss | 16.8 MiB | 16.8 MiB | 16.9 MiB | 16.9 MiB |
| client_rss | 1.7 MiB | 1.7 MiB | 1.7 MiB | 1.7 MiB |

The boot that saves a template records the pages its first run touches, and every warm
VM restored after that touches them before its request (platform-measurements.md M30).
The guest's part fell from 1.0 ms to 366 µs at the median. The first two runs of each
template, whose VMs the pool restored before the working set existed, are no longer
samples. An interleaved A/B against a0f7926 from one template (M30) puts the change at
857 µs of wall time at the median (95% [834, 891]).

**2026-09-29, x86_64 Linux on KVM** · d21a89f (branch kvm-snapshots) · GitHub's
ubuntu-24.04 runner as in Restore (AMD EPYC 9V45, KVM nested) · n=100 over 5 templates,
1 vCPU, 256 MiB · load 1.38 0.90 0.38

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| run_cold | 249985 µs | 255058 µs | 260014 µs | 272166 µs |
| **run_template** | **82921 µs** | **86346 µs** | **95990 µs** | **100363 µs** |
| template_command | 81230 µs | 84399 µs | 94095 µs | 97960 µs |
| template_outside | 1781 µs | 2207 µs | 2876 µs | 3376 µs |
| run_cold_rss | 66.5 MiB | 68.6 MiB | 68.6 MiB | 70.6 MiB |
| run_template_rss | 20.2 MiB | 20.4 MiB | 20.5 MiB | 20.6 MiB |
| client_rss | 33.6 MiB | 33.6 MiB | 33.6 MiB | 33.6 MiB |

Pooled runs from templates, on Linux for the first time: three times faster than a boot,
but the command's 81 ms in a restored guest is the whole of it, as in Restore. Outside
the guest a run costs 1.8 ms. The client's peak RSS, 33.6 MiB against 1.7 on the Mac, is
not explained yet.

## Firecracker (`crates/shards/benches/firecracker.rs`)

`cargo bench -p shards --bench firecracker [-- --runs N --cpus N --memory MIB]` (Linux, KVM)

Method:
- shards and Firecracker boot the pinned kernel with one initrd (the test guest in `idle`
  mode as `/init`) and one kernel command line, interleaved, alternating which goes first.
  Three warm-up pairs are discarded.
- Firecracker is the pinned release binary, run with `--no-api --config-file` and its
  default seccomp filters.

| Phase | Measured |
|---|---|
| `to_ready` | host clock, spawn → the guest's ready line on the VMM's stdout |
| `overhead` | RSS outside guest memory by Firecracker's rule (tests/host_tools/memory.py); max of 20 readings 10 ms apart while the guest idles |
| `peak_rss` | `ru_maxrss` from wait4(2), guest memory included |

### Runs

**2026-09-28** · 9ef57c2 · GitHub `ubuntu-24.04` runner: AMD EPYC 9V74, 4 vCPUs, 16 GB,
Linux 6.17.0-1022-azure, nested under Hyper-V · Firecracker v1.17.0 · kernel
vmlinux-6.18.48-x86_64 · n=30, 1 vCPU, 128 MiB

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| shards to_ready | 146131 µs | 149549 µs | 223079 µs | 223079 µs |
| Firecracker to_ready | 145085 µs | 151869 µs | 221177 µs | 221177 µs |
| **shards overhead** | **2.4 MiB** | **2.4 MiB** | **2.5 MiB** | **2.5 MiB** |
| Firecracker overhead | 4.5 MiB | 4.5 MiB | 4.5 MiB | 4.5 MiB |
| shards peak_rss | 64.4 MiB | 66.3 MiB | 66.4 MiB | 66.4 MiB |
| Firecracker peak_rss | 64.4 MiB | 66.4 MiB | 66.5 MiB | 66.5 MiB |

Boot latency is a tie: nearly all of it is the guest kernel, the same for both. The VMM
overhead is 47% lower.

**2026-09-28, guest RAM on transparent huge pages** · 173c5b2 (a69e070's MADV_HUGEPAGE) ·
GitHub `ubuntu-24.04` runner: AMD EPYC 7763, Linux 6.17.0-1022-azure, THP `enabled=always`,
`defrag=madvise` · otherwise as above

| Phase | p50 | p90 | p99 | max |
|---|---|---|---|---|
| **shards to_ready** | **143168 µs** | **146830 µs** | **162390 µs** | **162390 µs** |
| Firecracker to_ready | 147311 µs | 159104 µs | 228157 µs | 228157 µs |
| shards overhead | 2.4 MiB | 2.4 MiB | 2.5 MiB | 2.5 MiB |
| Firecracker overhead | 4.5 MiB | 4.5 MiB | 4.5 MiB | 4.5 MiB |
| shards peak_rss | 64.4 MiB | 66.4 MiB | 66.4 MiB | 66.4 MiB |
| Firecracker peak_rss | 64.4 MiB | 66.3 MiB | 66.5 MiB | 66.5 MiB |

The runner's THP mode is `always`, so both VMMs' 2 MiB-aligned guest RAM could already get
huge pages. `MADV_HUGEPAGE` adds alignment by construction and, under `defrag=madvise`,
direct compaction on fault. shards is 2.8% faster at p50 and 29% faster at p99. Peak RSS is
unchanged. A same-runner A/B without the advice is still needed to attribute the gain.
