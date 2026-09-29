# Platform measurements: Hypervisor.framework on Apple M5 Max

Ground-truth numbers for the primitives the shards VMM is built on, measured on the
development host. Every design decision that depends on a macOS/HVF cost cites a row
here. Harness: [`measurements/hvf/hvfbench.c`](measurements/hvf/hvfbench.c).

## Environment

| | |
|---|---|
| Machine | Apple M5 Max, 18 cores, 128 GiB |
| OS | macOS 26.4.1 (25E253), Darwin 25.4.0 |
| SDK / compiler | CommandLineTools MacOSX.sdk, Apple clang 21.0.0, `-O2` |
| Date | 2026-09-28 |
| Counter | `mach_absolute_time` / `CNTVCT_EL0`, 24 MHz (41.67 ns resolution) |

Caveat: runs were taken while other workloads (background research agents) were
active, so tail percentiles carry system noise. Repeated runs of the same test moved
p50 interrupt latency between 8.9 and 19.5 µs. Treat single-run tails as indicative,
and re-run on a quiet machine before quoting final numbers.

## Method

Each test boots a bare-metal AArch64 guest whose code is assembled into the harness
binary and copied into guest RAM. The guest runs EL1 with MMU on: identity map,
4 KiB stage-1 granule, 1 GiB blocks, Normal WB for RAM and Device-nGnRE for GIC/MMIO.
Guest RAM layout:

- a 2 MiB system region at `0x8000_0000` (code, L1 table, vectors, stack)
- the region under test at `0x1_0000_0000`
- an unmapped MMIO page at `0x4000_0000`
- the in-kernel GIC distributor at `0x0800_0000`

Timings are host `mach_absolute_time` deltas. Aggregate rates divide a loop's total
time by its iteration count. Distributions report min/p50/p90/p99/max.

Thread policy is varied with flags that apply to every harness thread:
`--qos=ui|in|ut|bg` (QoS class), `--rt` (Mach `THREAD_TIME_CONSTRAINT_POLICY`,
computation 0.5 ms / constraint 1 ms) and `--lat0` (`THREAD_LATENCY_QOS_POLICY`
tier 0). Pass flags as separate argv words. An early run bundled them into one word
under zsh (which does not word-split unquoted variables) and silently ran at
background QoS. Those numbers were discarded, and the harness now rejects unknown
values.

Reproduce: `docs/research/measurements/hvf/build.sh && TMPDIR=<dir> ./hvfbench [flags] [tests]`.

## Results

### M1. Capabilities (`info`)
- max vCPUs 64.
- IPA size 36 bits by default, 40 bits max.
- **EL2 (nested virtualization) supported**.
- Default IPA granule is **16 KiB**; the host page is 16 KiB.
- `CNTFRQ` 24 MHz.

In-kernel GIC:

| Item | Value |
|---|---|
| Distributor | 64 KiB (64 KiB aligned) |
| Redistributor | 128 KiB per vCPU; 32 MiB region; 64 KiB aligned |
| MSI frame | 64 KiB |
| SPIs | INTID 32–1019 (988) |
| EL1 virtual timer | PPI INTID 27 |

### M2. Object lifecycle (`lifecycle`, n=200, µs)

| Operation | p50 | p99 |
|---|---|---|
| `hv_vm_create` (warm process) | 10.8 | 16.6 |
| `hv_vm_destroy` | 17.7 | 27.4 |
| `hv_gic_create` (incl. config) | 2.3 | 5.6 |
| `hv_vcpu_create` | 6.8 | 16.3 |
| `hv_vcpu_destroy` | 3.9 | 11.5 |

### M3. Stage-2 mapping (`map`, n=20, µs, p50)

| Size | `hv_vm_map` | `hv_vm_protect` | `hv_vm_unmap` |
|---|---|---|---|
| 16 MiB | 0.33 | 0.21 | 0.21 |
| 256 MiB | 0.46 | 0.21 | 0.29 |
| 1 GiB | 1.13 | 0.42 | 0.79 |
| 8 GiB | 20.0 | 2.4 | 6.8 |

Mapping is lazy: cost is near-constant, and memory is only faulted in on guest touch
(M5). With a 4 KiB IPA granule, `hv_vm_map`/`hv_vm_protect` accept 4 KiB-aligned
host offsets and IPAs even though host pages are 16 KiB.

### M4. Exit round trips (`exits`, 200k iterations)

| Exit | aggregate | p50 | p99 |
|---|---|---|---|
| `hvc #0` → userspace → re-enter | 700 ns | 708 ns | 833 ns |
| MMIO write (data abort + get/set PC) | 808 ns | 792 ns | 917 ns |
| MMIO read (data abort + set Rt + PC) | 799 ns | 792 ns | 917 ns |

- After an HVC exit, PC already points past the `hvc`. After a data abort, the VMM
  must advance PC.
- Data-abort syndromes arrive with ISV=1 and valid SAS/SRT/WnR. The faulting IPA is in
  `exit->exception.physical_address`.

### M5. Stage-2 first-touch cost (`faults`, 512 MiB region, ns per page)

16 KiB IPA granule, 16 KiB stride:

| Backing | read first | write first | resident |
|---|---|---|---|
| anon (untouched) | 1219 | 1357 | 7–10 |
| anon, host-prefaulted | 1251 | 1245 | 8–10 |
| file `MAP_PRIVATE`, page-cache hot | **1072** | 1900 (CoW) | 7–9 |
| file `MAP_PRIVATE`, host pre-read | 1372 | 2425 | 9–10 |
| file `MAP_SHARED`, page-cache hot | 1314 | — | 8 |

4 KiB IPA granule: 1560–2030 ns per **4 KiB** page at 4 KiB stride. That is about
4× the per-byte cost of the 16 KiB granule.

Write-protect dirty tracking (`hv_vm_protect` → guest write → permission-fault exit
→ unprotect page → resume):

| Granule | per dirtied page | protect pass (512 MiB) |
|---|---|---|
| 16 KiB | 3744 ns | 1.17 ms |
| 4 KiB | 4658 ns | 4.0 ms |

### M6. Parallel first-touch (`pfault`, 1 GiB cache-hot `MAP_PRIVATE` file, 16 KiB)

| vCPUs faulting disjoint slices | 1 | 2 | 4 | 8 | 12 |
|---|---|---|---|---|---|
| effective ns/page | 999 | 722 | 453 | 418 | 326 |
| throughput (GB/s) | 16.4 | 22.7 | 36.1 | 39.2 | 50.2 |

### M7. Idle (`wfi`)
- Without a GIC, guest `WFI` exits to userspace (EC 0x01).
- **With `hv_gic`, `WFI` blocks inside HVF.** The only way out is a pending interrupt
  or `hv_vcpus_exit`, and no userspace exits occur while the guest is idle.

### M8. Interrupt injection (`irq`: `hv_gic_set_spi` on a host thread → guest IRQ handler → `hvc` exit; µs)

| Thread policy | vCPU idle (WFI) p50 / p99 | vCPU busy p50 / p99 |
|---|---|---|
| default QoS (two runs) | 8.9 / 61.5 · 19.5 / 315 | 3.75 / 5.9 · 6.7 / 86 |
| `--qos=ui` | 14.8 / 96 | 6.3 / 66 |
| `--qos=ui --lat0` | 8.5 / 20.6 | 3.75 / 5.8 |
| **`--qos=ui --rt`** | **8.2 / 11.0** | **3.75 / 4.7** |

These figures include the guest handler's `hvc` exit (≈0.7 µs, M4).

### M9. vCPU kick (`kick`: `hv_vcpus_exit` → `hv_vcpu_run` returns CANCELED; µs)

| Thread policy | busy p50 / p99 | idle (WFI) p50 / p99 |
|---|---|---|
| default | 1.96 / 44 | 11.9 / 102 |
| `--qos=ui --rt` | **0.92 / 1.33** | **4.5 / 8.5** |

### M10. Guest timer precision (`vtimer`: guest arms `CNTV` for +period, then idles; lateness = handler entry − deadline; µs p50 (p99))

| Idle path, thread policy | period 100 µs | 1 ms | 10 ms |
|---|---|---|---|
| WFI in HVF, default | 30.8 (41) | 258 (276) | 2002 (2530) |
| WFI in HVF, `ui` | 31.3 (37) | 259 (287) | 1985 (2548) |
| WFI in HVF, `ui` + `lat0` | 18.3 (24) | 132 (152) | 1010 (1041) |
| **WFI in HVF, `ui` + `rt`** | **5.2 (10.4)** | **8.7 (23)** | **18.8 (33)** |
| `hvc` idle hint → VMM `kevent` `NOTE_CRITICAL`, leeway 0, default | 9.2 (23) | 23.4 (32) | 22.5 (35) |
| same, `ui` + `rt` | 7.5 (18) | 20.5 (46) | 34.3 (70) |

Host sleep precision, standalone (`sleep`), lateness p50 for a 1 ms deadline:
`mach_wait_until` 252 µs, `nanosleep` 253 µs, `kevent` `EVFILT_TIMER`
`NOTE_CRITICAL|NOTE_LEEWAY` (leeway 0) 16 µs. The first two scale with the interval,
with leeway of about 25% capped near 2.5 ms, which is macOS timer coalescing.

### M11. Process spawn → first guest instruction (`spawn`, n=50)

| | p50 | p99 |
|---|---|---|
| `posix_spawn` → child `main()` (entitled, ad-hoc signed binary) | **3.70 ms** | 4.77 ms |
| `posix_spawn` → VM + GIC + 128 MiB map → vCPU → first guest exit | 3.82 ms | 4.88 ms |
| in child: first `hv_vm_create` + GIC + maps | 554 µs | 790 µs |
| in child: `hv_vcpu_create` + sysreg setup | 51 µs | 97 µs |
| in child: first `hv_vcpu_run` → `hvc` | 9.7 µs | 24 µs |

### M11b. Where spawn time goes (100 spawns each; child writes one byte to fd 3 from `main`)

| Child binary | p50 | p10 | p90 |
|---|---|---|---|
| trivial C, ad-hoc signed | 0.81–0.87 ms | 0.77 | 0.88–1.13 |
| + `com.apple.security.hypervisor` entitlement | 0.93 ms | 0.85 | 1.07 |
| + linked against `Hypervisor.framework` | **2.38 ms** | 2.24 | 2.63 |

- The entitlement check adds ~0.06 ms. **Loading Hypervisor.framework at launch adds
  ~1.5 ms.**
- Freshly built binaries show rare first-exec outliers of ~100 ms (p99), consistent
  with one-time code-signature validation.
- Method: `posix_spawn` + pipe as in M11. This was a scratch program, not part of
  `hvfbench`: `child.c` = `write(3, "\1", 1)` in `main`.

### M12. Default guest-visible registers (`guestinfo`, EL1 guest, `hv_vcpu_config` = NULL)

| Register | Value | Decoded |
|---|---|---|
| `CNTFRQ_EL0` | 0x016e3600 | 24 MHz; the guest `CNTVCT` equals host `mach_absolute_time` (offset 0) |
| `MIDR_EL1` | 0x610f0000 | implementer 0x61 (Apple), part 0 |
| `MPIDR_EL1` | 0x80000000 | bit 31 is forced on even though 0 was written |
| `ID_AA64MMFR0_EL1` | 0x000010000f100022 | **PARange = 40 bits** (while the VM's default IPA is 36 bits); 16-bit ASIDs; 4K and 16K granules supported, 64K not |
| `ID_AA64PFR0_EL1` | 0x1101000011110011 | EL0/EL1 AArch64 only, **no EL2 exposed**, FP/AdvSIMD, GIC sysregs, no SVE, CSV2/CSV3 = 1 |
| `ID_AA64PFR1_EL1` | 0x0000000202000001 | BTI, **SME = 2 (SME2) exposed**, no MTE |
| `ID_AA64DFR0_EL1` | 0x10305006 | no PMU (PMUVer = 0) |
| `CTR_EL0` | 0x99444c004 | 64-byte I/D minimum lines |

`hv_vcpu_get_exec_time` after a ~3 µs run returned 84, which is consistent with the
header's mach-tick units (84 × 41.67 ns), not nanoseconds.

### M13. Redistributor assignment (`guestinfo`, n vCPUs created concurrently)

Redistributor frames (128 KiB each, contiguous from the configured base) and
`GICR_TYPER.Processor_Number` follow **`hv_vcpu_create` call order**, not
`MPIDR_EL1`. `GICR_TYPER.Last` is set on the frame of the **last-created** vCPU.
Example with 4 vCPUs created concurrently: index 2 (Aff0 = 2) was created last, got
frame 3 (`base + 0x60000`) and processor number 3, and is marked Last.
`GICR_TYPER.Affinity` does track MPIDR.

### M14. Restore-path state transfer (`restore`, `gicregs`)

| Operation | p50 | p99 |
|---|---|---|
| vCPU state save (35 core + 32 SIMD + 37 sysreg + 9 ICC via HVF getters) | 0.67 µs | 0.96 µs |
| vCPU state load (same registers via setters) | 0.54 µs | 0.75 µs |
| `hv_gic_state_create` + `get_data`, 1/4/16 vCPUs (blob **126 314 B**, size independent of vCPU count) | 1.84 / 1.97 / 1.87 ms | 1.9 / 2.1 / 3.4 ms |
| `hv_gic_set_state` into a fresh VM, 1/4/16 vCPUs | **1.22 / 1.30 / 1.25 ms** | 1.26 / 1.36 / 1.62 ms |
| `hv_gic_set_distributor_reg`, one call | ~37 ns (mean) | — |
| full SPI distributor rewrite (IGROUPR, ICENABLER, ICPENDR, IPRIORITYR, ICFGR, IROUTER for INTID 32–1019; 1387 writes) | **19.6 µs** | 27.4 µs |
| `hv_gic_reset` | 10.5 µs | 11.6 µs |

### M15. First-touch latency per fault (`faulttail`)

- **Method.** The guest reads CNTVCT (24 MHz, 41.7 ns) around every first touch and stores
  each delta. The region is 256 MiB at 16 KiB stride, remapped fresh 16 times: 262 144 timed
  faults per row, 6.3 M in total. Backings:
  - anonymous
  - anonymous, host-prefaulted
  - file `MAP_PRIVATE`, cache-hot
  - file `MAP_FIXED` over an anonymous reservation (how a VMM places a file inside guest
    RAM it reserved)
- **Ops.** Load, store, and instruction fetch (a `blr` into a page that starts with `ret`).
  Each op runs with stage 1 on (Normal WB) and off (Device-nGnRnE, as in early kernel
  boot).

| Backing | load p50 / max | store p50 / max | exec p50 / max |
|---|---|---|---|
| anon | 1.08–1.12 / 46 µs | 1.08–1.12 / 44 µs | — |
| anon, host-prefaulted | 1.12 / 111 µs | 1.12 / 108 µs | 1.12 / 50 µs |
| file `MAP_PRIVATE`, cache-hot | 0.96–1.00 / 26 µs | **1.75–1.79** (CoW) / 80 µs | 0.92–0.96 / 45 µs |
| file `MAP_FIXED` over anon | 0.96 / 38 µs | 1.75–1.79 / 65 µs | 0.96 / 72 µs |

- **No fault took more than 111 µs** in 6.3 M, for any backing, op or MMU state.
- p99.99 is 7–26 µs.
- Cache-hot file pages are the cheapest to read or execute.
- A copy-on-write store costs ~0.65 µs more than an anonymous first write.

### M16. Booting with a file-mapped kernel image (E2E, `shards vm run`)

- **Method.** Interleaved boots, alternating every boot: 200 copy the kernel image into
  anonymous guest RAM, 200 map it `MAP_PRIVATE`/`MAP_FIXED` over guest RAM before
  `hv_vm_map` (an experiment build).
- **Results** (µs):

| Mode | to_init p50 | p99 | max | > 30 ms |
|---|---|---|---|---|
| copy | 20 009 | 22 677 | 23 585 | 0 |
| map | 20 595 | **1 028 028** | 1 060 093 | **5** |

- **Where the time goes.** In a further 600 mapped boots with console output, one boot
  stalled for 941 ms. The guest's own clocks did not see it: its last printk read 53.9 ms
  and init's CLOCK_BOOTTIME 49.6 ms. The stall therefore happens before the guest's
  timekeeping starts, early in boot.
- **M15 doesn't reproduce it** in a long-lived process, with MMU on or off, for loads,
  stores or instruction fetch. The stall must depend on the process lifecycle: a fresh
  process maps a file object that the process just before it mapped and tore down. It
  doesn't come from per-fault cost. **Cause UNVERIFIED.**
- **Consequences.**
  - Cold boot copies the kernel image (docs/benchmarks.md).
  - For snapshot restore (D7): map snapshot memory in the warm-pool process before the
    request arrives, never on the request path.
  - Restore benchmarks must include fresh-process restores, so they can catch this.

### M17. Which host mappings `hv_vm_map` accepts for read-only device memory

- **Method.** One VM. A 2 MiB file, opened read-only, mapped `PROT_READ` into this
  process, shared or private, then `hv_vm_map`ped with guest permissions R, RX and RWX.
  Controls: a shared mapping of the file opened read-write, and anonymous memory. Harness:
  `hvf_maps_private_but_not_shared_read_only_files` in crates/vmm/src/hv/hvf/mod.rs
  (`cargo test -p shards-vmm --lib -- --ignored --exact
  hv::hvf::tests::hvf_maps_private_but_not_shared_read_only_files`), 2026-09-28, macOS
  26.4.1 on the M5 Max.
- **Results.** `MAP_SHARED` of the read-only file fails with `HV_ERROR` (0xfae94001) for
  every guest permission, even R alone. `MAP_PRIVATE` of the same file succeeds for all
  three. So do the controls.
- **Consequence.** virtio-pmem maps image files `MAP_PRIVATE` and read-only
  (`platform::map_file_readonly`). Pages still come from, and stay shared with, the host
  page cache: nothing writes them, since the host mapping is read-only and so is the
  guest's stage 2. Image files are never opened for writing. Our reading, **UNVERIFIED**:
  HVF wants mappings whose maximum protection includes write, which a copy-on-write
  mapping has and a shared mapping of a read-only descriptor does not.

### M18. Linting every target with a C dependency (registry-pull M1)

- **Question.** aws-lc-sys (D19) compiles C for every target, and `cargo clippy` runs its
  build script. What does linting all 8 CI targets from this Mac cost?
- **Setup.** `brew install zig llvm nasm` and `cargo install cargo-xwin` (zig 0.16.0, LLVM
  23.1.2, NASM 3.02, cargo-xwin 0.23.1). `scripts/lint` compiles the C with zig for Linux
  (glibc and musl), with clang-cl through cargo-xwin for Windows, and with Apple clang for
  macOS. The first Windows lint downloads Microsoft's CRT and SDK: that run took 312 s
  (n = 1).
- **Disk.** zig 246 MB, LLVM 1.8 GB, NASM 2.9 MB, the cargo-xwin cache 1.1 GB.
- **Method.** Harness: `docs/research/measurements/cross-lint/run.sh 5`. For each target:
  - one untimed run;
  - 5 cold runs: the target's build directory is removed, so AWS-LC compiles again, but
    the host's build scripts stay built;
  - 5 warm runs: `crates/registry/src/lib.rs` touched.
  - Wall time of `scripts/lint <target>`. rustc 1.98.0; the workspace at 2e7a3db plus
    the registry crate; 2026-09-28, on this machine and nothing else running.
- **Results** (seconds, n = 5 each; at n = 5, p90 and p99 are the maximum):

| Target | cold p50 | cold p90 | cold p99 | cold max | warm p50 | warm max |
|---|---|---|---|---|---|---|
| aarch64-apple-darwin | 10.86 | 12.38 | 12.38 | 12.38 | 0.20 | 0.29 |
| x86_64-apple-darwin | 9.70 | 15.13 | 15.13 | 15.13 | 0.19 | 0.20 |
| x86_64-unknown-linux-gnu | 10.01 | 14.00 | 14.00 | 14.00 | 0.25 | 0.26 |
| aarch64-unknown-linux-gnu | 10.39 | 11.24 | 11.24 | 11.24 | 0.19 | 0.70 |
| x86_64-pc-windows-msvc | 9.02 | 13.58 | 13.58 | 13.58 | 0.23 | 0.31 |
| aarch64-pc-windows-msvc | 7.74 | 8.01 | 8.01 | 8.01 | 0.23 | 0.25 |
| x86_64-unknown-linux-musl | 12.19 | 12.48 | 12.48 | 12.48 | 0.22 | 0.25 |
| aarch64-unknown-linux-musl | 11.28 | 11.54 | 11.54 | 11.54 | 0.18 | 0.21 |

- **Consequences.**
  - Lint all 8 targets locally (registry-pull R3, option a). A cold pass costs about
    80 s in all, and an edit re-lints in about 2 s. That is cheaper than waiting on CI.
  - The compilers disagree on some of AWS-LC's feature probes: zig's clang fails
    `neon_sha3_check.c`, so aws-lc-sys leaves that code out of the lint build. Lints don't
    depend on it, and CI builds each target with its native compiler.

### M19. AWS-LC's first random bytes in a new process

- **Question.** AWS-LC seeds its DRBG on a process's first random draw. What does that
  cost with its default CPU jitter entropy source, and when seeded from the OS
  (`AWS_LC_SYS_NO_JITTER_ENTROPY=1`)?
- **Method.** Harness: `docs/research/measurements/aws-lc-entropy/run.sh 50`. It builds a
  tiny binary on our vendored aws-lc-rs 1.18.1 / aws-lc-sys 0.45.0 both ways (release),
  then runs each as 50 fresh processes, interleaved. Each process times its first and its
  second 32-byte `SystemRandom::fill`. 2026-09-28, this machine, revision 965c5ed.
- **Results** (µs, n = 50 each):

| Seed source | first p50 | first p90 | first p99 | first max | second p50 | second max |
|---|---|---|---|---|---|---|
| CPU jitter (default) | 17 372 | 18 885 | 19 720 | 19 720 | 2 | 3 |
| OS | 7 | 8 | 29 | 29 | 2 | 2 |

- **Consequence.** Every build seeds AWS-LC from the OS (D19, vendor/README.md). Jitter
  seeding costs 17 ms, once per process: over three times the whole start budget, for any
  process that makes a TLS connection.

### M20. A restored guest's first vsock connection, with every CPU busy

- **Question.** A restored guest dials the host at once: shards-init does, for its
  workload. Does that connection survive the reset that follows a restore?
- **Method.** Harness: `docs/research/measurements/vsock-restore-race/run.sh N COMMAND…`.
  It keeps every CPU busy with `yes`, runs COMMAND N times, and prints each failure.
  - COMMAND: `shards vm restore TEMPLATE -- /bin/testguest exit 0`, on a template of the
    E2E test image, and `shards run --pull never IMAGE exit 0`, which restores the
    image's template.
  - For the console of failed runs, a diagnostic build did not discard it.
  - 2026-09-28/29, this machine (18 CPUs), on a host running other VMs.
- **Results.**

| Device after a restore | Command | Failed |
|---|---|---|
| posts TRANSPORT_RESET (3e47544) | `vm restore` | 13 of 350 |
| resets the snapshot's streams with RSTs on RX | `vm restore` | 0 of 400 |
| same | `shards run`, templated | 0 of 400 |

  - The six failures with a console showed the same line: `shards-init: dialing the
    host: Connection reset by peer (os error 104)`. init then powered off, and shards
    reported that the guest stopped before the command ended.
  - The other seven showed the same markers: the guest resumed and never connected.
  - At the old failure rate, 400 clean runs would happen by chance with p ≈ 3 × 10⁻⁷.
- **Cause.** Busy CPUs delay the device's worker thread. If it first runs after the
  guest has sent its REQUEST, one interrupt carries both the RESPONSE (RX queue) and
  the owed TRANSPORT_RESET (event queue).
  - Linux's `vm_interrupt` visits queues in setup order: RX, TX, event (virtio_ring.c
    `list_add_tail`).
  - So `rx_work` establishes the socket, then `event_work` resets every established
    socket, the new one included (virtio_transport.c `virtio_vsock_reset_sock`).
  - `connect` then returns ECONNRESET.
- **Consequence.** D12: the restored device resets what its snapshot held with RSTs on
  RX. `rx_work` handles RX strictly in order, so the resets land before anything newer.
  The event queue is never used.

## Implications for shards (macOS/HVF backend)

1. **≤5 ms start cannot include a process spawn on macOS.**
   - Spawn alone is 3.7 ms p50 (M11), and HVF allows one VM per process.
   - The irreducible floor is ~0.8 ms spawn + ~1.5 ms Hypervisor.framework load +
     ~0.55 ms first `hv_vm_create` (M11b, M11).
   - Start must therefore be served by a **pool of pre-spawned VMM processes**. Each
     has already paid dyld, first-`hv_vm_create` (~0.55 ms, M11), GIC and vCPU-thread
     creation.
   - A start request then only maps snapshot memory and restores state.
2. **Use the in-kernel GIC (`hv_gic`), but never its state blob on the restore path.**
   - Idle vCPUs stay in the kernel with zero userspace exits (M7).
   - SPI injection reaches a running guest in ~3.8 µs (M8).
   - `hv_gic_set_state` costs ~1.2 ms, a quarter of the 5 ms budget. Rewriting the
     distributor at register level costs ~20 µs (M14).
   - Snapshots therefore record GIC distributor, redistributor and ICC registers
     individually, and restore them the same way.
   - The opaque blob remains useful only as a cross-check in tests.
3. **vCPU threads run under Mach time-constraint policy plus QoS user-interactive.**
   - Timer lateness drops from ~26% of the interval to 5–19 µs (M10).
   - Kick latency drops to 0.9 µs busy / 4.5 µs idle (M9).
   - Interrupt p99 drops to ≤11 µs (M8).
   - Open risk: XNU's real-time fail-safe demotion under CPU-bound guests, and
     fairness across many VMs. Must be measured before committing (see open
     questions).
4. **Keep the 16 KiB IPA granule for RAM.**
   - First-touch costs ~1.1–1.4 µs per 16 KiB page, about 4× cheaper per byte than
     4 KiB (M5).
   - A restored working set of W MiB costs ≈ 64·W µs of faults on one vCPU (e.g.
     16 MiB ≈ 1.0 ms), or ~2.2× less with 4 prefetch vCPUs (M6).
   - Guest stage-1 can remain 4 KiB; the stage-2 granule is independent.
5. **Snapshot memory: file `MAP_PRIVATE` is the cheapest first-touch backing** (1.07 µs
   per 16 KiB read, M5).
   - Host-side pre-reading does *not* help (1.37 µs), because the stage-2 fault
     itself dominates.
   - On macOS, working-set prefetch (cf. REAP) must therefore be done from guest
     context (helper vCPUs), not by host reads.
   - Every page a restored guest writes costs a ~1.9 µs CoW fault plus 16 KiB of
     private memory.
6. **Exits are expensive (~0.7–0.8 µs, M4) and HVF has no ioeventfd.**
   - Each virtio queue notify is a vCPU exit handled on the vCPU thread.
   - The design must suppress notifications (EVENT_IDX), batch, and make the notify
     handler do nothing but hand off to the device.
7. **Dirty tracking is feasible but not free** (3.7 µs per dirtied 16 KiB page, M5).
   It's fine for diff snapshots taken from a quiesced template, not for continuous
   tracking.
8. **Create vCPUs strictly sequentially in index order**, at boot and on restore (M13).
   - This makes redistributor frames, processor numbers and MPIDRs coincide.
   - The DT redistributor region is exactly `n × 128 KiB`, because Linux walks frames
     until `Last`.
9. **Pin the guest's view of the CPU explicitly** rather than inheriting defaults (M12):
   - Clamp `ID_AA64MMFR0_EL1.PARange` to the configured IPA size, or configure the IPA
     to match the advertised 40 bits.
   - Decide SME exposure deliberately. SME2 is visible by default, and exposing it
     means SME/ZA state must be part of every snapshot.

## Open questions (to measure)

- Behaviour of real-time vCPU threads under sustained CPU-bound guests:
  - demotion, throughput, and host responsiveness with N ≫ cores VMs.
- ~~Whether GIC plus vCPU state restore fits in <100 µs~~.
  - Answered by M14: the blob path does not fit (1.2 ms).
  - The register-level path does: ~20 µs for the distributor plus ~0.5 µs per vCPU.
  - Still to measure: per-vCPU redistributor and ICC restore, and the exact register
    set a Linux guest dirties.
- End-to-end restore of a real Linux guest snapshot: working-set size after resume,
  and time to first userspace instruction.
- The cause of M16's first-touch stalls (~1 s, ~1% of fresh processes mapping a file
  that a just-exited process mapped). Reproduce it in hvfbench with sequential child
  processes.
- Linux/KVM counterparts of M4–M11 (to be measured on a KVM host and in an EL2 guest).
