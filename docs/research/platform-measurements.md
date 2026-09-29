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

### M21. Kernel work a template hands to every copy

- **Question.** In most templates, a restored guest lost about one tick (10 ms at
  `CONFIG_HZ=100`) before its command ran. In the rest it did not, and each template
  always behaved the same way. Where did the time go?
- **Method.**
  - Markers in a diagnostic shards-init placed the stall in the first vsock connect,
    after the host accepted the connection: 9.1–9.2 ms in slow templates, 50–330 µs in
    the rest.
  - A diagnostic VMM kicked the vCPU during and after the stall and read its PC. Every
    sample was kernel code: `mpihelp_submul_1`, `mpihelp_addmul_1`, `mpihelp_divrem`,
    the multi-precision arithmetic under RSA. PCs were resolved against the guest's
    `/proc/kallsyms`; the kernel has no KASLR.
  - In a restored copy of alpine, `ps` showed `cryptomgr_test` running, and
    `/proc/crypto` listed an algorithm not yet tested.
  - Harness: `docs/research/measurements/template-quiescence/run.sh`. Templates saved by
    two shards-init builds alternate, and each is restored twice.
  - 2026-09-29, this machine, with other VMs running.
- **Cause.** After boot, the kernel runs its crypto self-tests in `cryptomgr_test`
  threads (crypto/algapi.c `crypto_start_tests`; `CONFIG_CRYPTO_SELFTESTS=y`). They were
  still running when init saved the template, so every copy replayed the rest. The
  kernel is `PREEMPT_NONE`, so a copy's one vCPU stayed with that thread until a tick.
- **Results** (12 rounds, 24 restores per build):

| Templates saved by shards-init that | release → VM stopped p50 | p90 | max | restores with a step > 5 ms |
|---|---|---|---|---|
| snapshots once the image is mounted | 10 636 µs | 12 170 µs | 12 522 µs | 14 of 24 |
| also waits for the self-tests | 2 981 µs | 4 233 µs | 4 563 µs | 0 of 24 |

  - Waiting costs about 20 ms, once per template: the tests finished 39–40 ms into boot
    in each of 8 saves, about 20 ms after init mounted the image.
  - Restored copies of templates that waited had no `cryptomgr_test` running and no
    untested algorithm.
  - The warm path (`vm restore --hold`) had the same stall: 9.3 ms after the release in
    most templates (benchmarks.md, "Run"). With the new init, `warm_resume` is 163 µs
    at p50 and 192 µs at most.
  - A template's first restore is slower than later ones: its spawn took 1.9–2.4 ms
    instead of 0.8–1.0 ms. It is the first process to map the just-written memory. The
    image benchmark leaves it out.
- **Consequence.** D14, D16 and D25. shards-init saves a template only once
  `/proc/crypto` lists no larval and no untested algorithm, waiting at most 2 s. The
  self-tests stay on: turning them off (`cryptomgr.notests`) would drop the kernel's check
  of its own crypto.

### M22. Thread QoS on a templated run's request path

- **Question.** On a busy host, a templated `shards run` has a long tail in three places:
  restoring, the command's round trip, and launching and tearing down the process. Only
  vCPU threads run at user-interactive QoS (M10). The main thread, the workload relay
  and the virtio-vsock worker run at the default. Does raising them shorten the tail?
- **Method.** Harness: `docs/research/measurements/service-qos/run.sh`.
  - `qos.patch` adds a switch that raises those three threads to user-interactive. The
    harness builds it in a temporary worktree.
  - One binary serves both modes, and runs alternate.
  - Templates are quiescent (M21). Each is saved and restored once before sampling.
  - `--load` kept all 18 CPUs busy with `yes`.
  - 2026-09-29, this machine, with other VMs running.
- **Results** (ms):

| Load | QoS | n | wall p50 | p90 | p99 | max | restore p99 | command p99 | process p99 |
|---|---|---|---|---|---|---|---|---|---|
| the host's own (about 4–5) | default | 300 | 6.55 | 7.31 | 14.26 | 20.09 | 6.21 | 4.44 | 7.13 |
| | user-interactive | 300 | 6.50 | 7.44 | 17.19 | 17.55 | 7.72 | 4.54 | 7.10 |
| every CPU busy | default | 160 | 9.07 | 18.04 | 38.61 | 48.47 | 16.67 | 10.97 | 15.09 |
| | user-interactive | 160 | 8.96 | 17.73 | 30.46 | 36.10 | 14.46 | 6.72 | 16.32 |

  - Neither the median nor p90 moved. The p99s moved both ways between repeats. A first
    run of the same comparison under load had user-interactive at 37.8 ms p99 against
    37.2, with a max of 86.9 ms.
  - In the slowest runs, restoring (5–14 ms), handing off the exit status (1.7–3.8 ms)
    and launching and tearing down the process (up to 12 ms) grew together. The guest's
    own steps stayed at their medians.
- **Consequence.** The service threads keep the default QoS. The tail is the host-side
  work of a per-request process on shared CPUs: launching it, restoring a VM into it, and
  tearing it down. D2's warm pool takes all three off the request path. Warm requests in
  the run benchmark take 0.97 ms at p50.

### M23. A warm pool's handoff, and the client that asks for it

- **Question.** A warm pool's daemon receives a client's request and hands it, with the
  client's stdio, to a warm VM process. What does that handoff cost? And what does the
  client process cost, since every `shards run` starts one?
- **Method.** Harness: `docs/research/measurements/daemon-ipc/run.sh 500 SHARDS version`.
  - Two daemons run side by side. One answers each client itself. The other passes the
    client's connection, and the three stdio descriptors it sent (`SCM_RIGHTS`), to a
    pre-spawned worker, which answers on the client's connection.
  - Requests are 256 bytes and answers 8 bytes.
  - Samples interleave: in-process requests, then whole client processes (spawn →
    exit), a no-op process, and the full `shards` binary's `version`.
  - 2026-09-29, this machine. The load was 6.2 (1 min); an earlier run at load 14 gave
    the same order and gaps.
- **Results** (µs, n = 500 each):

| | p50 | p90 | p99 | max |
|---|---|---|---|---|
| request answered by the daemon | 22.5 | 40.2 | 55.9 | 122.7 |
| request handed to a worker | 31.2 | 44.4 | 74.7 | 91.8 |
| client process, request handed to a worker | 1 425 | 1 654 | 1 975 | 2 272 |
| no-op process (the same thin Rust binary) | 1 368 | 1 609 | 1 860 | 2 001 |
| `shards version` | 3 481 | 3 920 | 4 409 | 5 503 |

  - A handed-off request costs about 9 µs more than one the daemon answers itself.
  - A thin client's request adds about 60 µs to its launch.
  - `shards` costs 2.1 ms more than a thin binary at p50 and 2.5 ms more at p99, before
    doing anything. Interleaved launches put the cause in its frameworks: Hypervisor,
    Security and CoreFoundation each add the same ~1.3 ms over a trivial binary, and
    together no more than one alone. It also runs AWS-LC's static constructor
    (`OPENSSL_cpuid_setup`) at every launch.
- **A warm VM's standing cost.** Resumed, reseeded, connected and waiting for its
  command, one takes 12.3 MiB of RSS (n = 12, VMM process and touched guest memory
  together). It uses no CPU: 0.000 s over 3 s. Its vCPU waits in WFI inside HVF (M7).
- **Consequence.** D26: the handoff fits the start path's budget (§4), with room to
  spare. A pooled VM costs 12 MiB and no CPU. The client must be a binary that links
  none of those frameworks.

### M24. A socket in flight whose sender has closed it

- **Question.** Between 1 in 13 and 1 in 53 runs through the daemon never ended after
  their client's SIGINT. The warm VM's thread that relays the client's signals had
  already exited: its first read of the client's connection returned end of stream,
  0.9 µs after it started, while the client was alive and connected. The client's
  SIGNAL bytes then vanished (receive queue 0, `netstat -f unix`). Why?
- **Mechanism** (xnu-12377.101.15, the kernel of this host).
  - Freeing any Unix socket schedules the collector of in-flight descriptors
    (`thread_call_enter(unp_gc_tcall)`, bsd/kern/uipc_usrreq.c:2912).
  - `unp_gc` walks only descriptors that are themselves in flight (`unp_msghead`,
    uipc_usrreq.c:2572-2576). It marks one reachable when a process holds it
    (`fg_count > fg_msgcount`, :2603-2612) or when it sits in the receive buffer of a
    reachable socket that is also in flight.
  - The daemon passes the client's connection to a warm VM, then closes its own copy.
    The connection then has no holder but the message, which sits in the buffer of the
    warm VM's socket. That socket was never in flight, so the walk never reaches it.
  - The collector takes the connection for garbage (:2724) and calls `sorflush` on it
    (:2746). That marks it unable to receive, so reads return end of stream
    (`socantrcvmore`), and sets `SB_DROP`, "a barrier to prevent further appends"
    (bsd/kern/uipc_socket.c:4452-4533). The warm VM then installs the flushed socket as
    usual (`unp_externalize`, uipc_usrreq.c:2385-2461).
  - The daemon freed its own end of the handoff socket right after the send, which
    started a collection just as the VM was about to receive: hence the hangs.
- **Method.** Harness: `docs/research/measurements/unp-gc-flush/run.sh 1000`.
  - Each trial passes one end of a socket pair over another pair, as the daemon does.
    The sender closes its copy before the receive ("closed") or after it ("held").
    Optionally ("trigger") it frees an unrelated Unix socket, then waits 0, 100 or
    1000 µs before the receive.
  - A trial counts as flushed when the received socket reads end of stream while its
    peer is open and has just written a byte.
  - 2026-09-29, this machine (macOS 26.4, xnu-12377.101.15). Linux: the same program,
    built with `zig cc -target aarch64-linux-musl`, in a Linux 6.12.76 VM.
- **Results** (trials flushed, of 1000):

| sender | trigger | delay | macOS | Linux 6.12 |
|---|---|---|---|---|
| closed | no | 0 | 0 | 0 |
| closed | no | 100 µs | 704–754 | 0 |
| closed | no | 1 ms | 362–937 | 0 |
| closed | yes | 0 | 0 | 0 |
| closed | yes | 100 µs | 997 | 0 |
| closed | yes | 1 ms | 1000 | 0 |
| held | any | any | 0 | 0 |

  - Without a trigger of its own, the probe's previous trial frees sockets, and the rest
    of the system frees others, so flushes still happen whenever the receive waits.
  - Linux never flushed: its collector counts references from sockets not in flight.
  - A sender that keeps its copy until the receiver has the socket never saw a flush
    in 6 000 trials.
- **Consequence.** D26: the daemon holds its copies of the client's connection and
  stdio until the warm VM answers `TAKEN`, and a VM that ends first is replaced. In
  shards: 0 hangs in 600 pooled runs and 200 cold ones, then 300 pooled runs with all
  18 CPUs busy. Before, the same loop hung at run 30 and at run 53.
  `daemon::tests::a_handed_over_connection_survives_until_taken` fails every time
  without the wait.

### M25. Descriptors over Unix sockets, macOS against Linux

- **Question.** What exactly does each kernel do when descriptors pass by `SCM_RIGHTS`:
  the limits, a receiver with too little room, close-on-exec, what may pass, `MSG_PEEK`,
  and peer credentials? shards' daemon hands every run's stdio across processes (D26).
- **Method.** Harness: `docs/research/measurements/fdpass/run.sh`, one C program of
  ten probes, run as an ordinary user.
  - 2026-09-29: this machine (macOS 26.4, xnu-12377.101.15); Linux 6.12.76 (aarch64,
    in a VM) built with musl and with glibc 2.41, which agree except where noted.
  - docs/research/warm-pool-daemon.md §2.6 cites each probe next to the kernel source
    that explains it.
- **Results.**

| Probe | macOS | Linux 6.12 |
|---|---|---|
| Most descriptors in one message | 254 (255: EINVAL) | 253 (254: EINVAL) |
| Two SCM_RIGHTS headers in one message | EINVAL | accepted, delivered as one |
| Room for 2 of 10 sent | MSG_CTRUNC; all 10 installed, 2 reported; the header claims 52 bytes in a 20-byte buffer | MSG_CTRUNC; 2 installed, the rest closed |
| No control buffer, or `read(2)` | all installed, none reported | none installed (MSG_CTRUNC) |
| Receiver with 2 free slots, 5 sent | EMFILE; the data arrives later without them | 2 installed, MSG_CTRUNC, data consumed |
| `MSG_CMSG_CLOEXEC` | absent: received descriptors are inheritable | honoured |
| `MSG_PEEK` | installs none | installs duplicates |
| Refused kinds | kqueue | io_uring (not creatable here) |
| One descriptor, zero data bytes, stream | delivered | dropped |
| Status flags (`O_NONBLOCK`), offsets | shared with the sender | shared with the sender |
| Peer credentials | `getpeereid`, `LOCAL_PEERCRED`, `LOCAL_PEERPID` | `SO_PEERCRED`, `SO_PEERPIDFD` |

- **Consequence.** `shards_ipc` receives only with `recvmsg`, with room for 254
  descriptors, reads only within `msg_controllen`, and takes descriptors from every part
  of a message. Its messages always carry data, and it never peeks. On macOS it marks
  received descriptors close-on-exec and spawns children with
  `POSIX_SPAWN_CLOEXEC_DEFAULT`. It never changes a received descriptor's status flags.
  The daemon admits only clients of its own user, by `getpeereid` or `SO_PEERCRED`.

### M26. A pooled run's request path, and a client that links nothing

- **Question.** With restores off the request path (D26), what remains of a pooled
  `shards run`, and what makes its tail? And what does a client that links only the
  standard library save (M23)?
- **Method.**
  - *Phases.* Temporary timestamps in the client, the daemon and the warm VM: wall
    clock across processes, the workload thread's CPU time, and the process's page
    faults and context switches. 400 runs of `shards run IMAGE exit 0` with 30 ms between
    them, correlated by time. The home's path was too long for a socket address.
  - *`confstr`.* A C probe timed the first and the second
    `confstr(_CS_DARWIN_USER_CACHE_DIR)` in each of 5 new processes.
  - *QoS.* `docs/research/measurements/pool-qos/ab.py`: two homes with a daemon each.
    In arm B the client, the daemon's threads, the warm VM's workload thread and its
    virtio-vsock worker run at user-interactive QoS (`qos.patch`). The arms alternate,
    n = 300 each, first under the host's own load, then with all 18 CPUs busy (`yes`).
  - *End to end.* The image benchmark (benchmarks.md), n = 300 over 10 templates.
  - 2026-09-29, this machine, with other VMs and builds running.
- **Results.**
  - A pooled run at the median (µs): the client's launch (M23); its connection reaching
    the daemon's handler, 1 200 when the socket's path needed `confstr`; the daemon's
    receive 7, preparation 170, claim 200–240 and handover 40; in the warm VM, 70 from
    `TAKEN` to the command sent, 1 100 in the guest, and 80 from its status to `EXIT`.
  - The claim included starting the pool's next VM (`posix_spawn`), 200–600 µs of it.
  - In the slowest runs, one of the warm VM's two host-side stretches took 3–12 ms for
    work that takes 70 µs. A repeat at a lower host load had no such stall: its slowest
    run took 1.7 ms, all of it on CPU.
  - `confstr(_CS_DARWIN_USER_CACHE_DIR)`: 395–1 264 µs on a process's first call, then
    2–5 µs. macOS answers the first call through another process (inference).
  - QoS (ms):

| Load | QoS | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|---|
| the host's own (5.3) | default | 300 | 3.56 | 5.30 | 6.19 | 6.52 |
| | user-interactive | 300 | 3.56 | 5.25 | 6.12 | 7.37 |
| every CPU busy | default | 300 | 7.89 | 19.51 | 87.23 | 131.43 |
| | user-interactive | 300 | 7.86 | 20.08 | 108.71 | 114.81 |

  - End to end (`run_template`, ms; loads are the 1-minute average):

| Client | Load | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| `shards`, VMM frameworks linked | 6.0 | 5.15 | 5.42 | 5.63 | 5.77 |
| thin | 4.6–6.0 (3 runs) | 3.85–3.90 | 4.79–5.24 | 7.82–12.46 | 8.88–13.58 |
| thin, socket in the home, refill after handover | 2.5–3.2 (2 runs) | 3.37–3.38 | 3.56–3.68 | 3.75–3.88 | 3.98–4.22 |
| the same, every CPU busy | 17.2 | 4.84 | 16.57 | 31.50 | 46.62 |

  - The thin client's peak RSS is 1.6 MiB, against 6.3 MiB.
- **Consequence.** D26:
  - `shards` is a thin binary that runs `shardsd` for everything but `run` and
    `daemon stop`.
  - The daemon's socket is `daemon.sock`, named relative to the home, which each process
    that uses it makes its working directory.
  - A pool refills after its VM has taken its run.
  - The service threads keep the default QoS, as in M22. With every CPU busy, the tail
    is the CPU queue itself, which QoS does not shorten.
  - A run's remaining median is the client's launch and the guest's command, about
    1.1 ms each. The guest's command is the next target: a warm VM could fault in its
    working set while it waits.

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
