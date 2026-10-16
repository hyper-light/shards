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

### M27. The guest's spawn path in a restored VM

- **Question.** In a pooled run, the guest takes about 1.06 ms from receiving its command
  to answering with its status (M26), and `warm_spawn` is 0.8 ms of it (benchmarks.md,
  Run). Where does that go, and what shortens it?
- **Method.**
  - Temporary markers in shards-init around each step of starting the workload, in a
    template of the E2E test image. `vm restore TEMPLATE --hold -- /bin/testguest exit
    0`, released on stdin, n = 100 each, with the VMM's clock (`SHARDS_TIMING`).
  - A variant init read `/etc/passwd`, `/etc/group` and `/bin/testguest` before the
    template's snapshot.
  - The standby init (below) against the old one, from two templates restored in
    alternation, n = 150 each.
  - 2026-09-29, this machine.
- **Results** (µs, p50):

| Step | Init as it was | Files read before the snapshot |
|---|---|---|
| request → command received | 56–64 | 65 |
| read `/etc/passwd` and `/etc/group` | 200–212 | 109 |
| pipes, signalfd | 60–67 | 57 |
| fork | 143–161 | 186 |
| exec, until it succeeded | 303–337 | 255 |
| **total** | **766–846** | **675** |

  - Every step touches memory that a restored VM has not mapped yet, and pays a stage-2
    fault per 16 KiB page (M5), kernel memory included. Reading two small files still
    took 109 µs from the page cache.
  - Standby against old (µs):

| Init | request → exec p50 | p90 | p99 | request → status p50 | p90 | p99 |
|---|---|---|---|---|---|---|
| old | 813 | 945 | 1225 | 917 | 1212 | 1370 |
| standby | 747 | 860 | 1129 | 875 | 1008 | 1281 |

- **Consequence.**
  - shards-init forks the workload's process, a standby, before any snapshot. It also
    reads the image's user database then. A run only sends the standby its orders
    (D16).
  - The fork's own cost mostly reappears as the standby's first touches after a restore:
    the run saves about 45 µs at the median and 90 µs at p99.
  - What remains is first-touch faults across the whole path. A warm VM waits idle
    before its request, so it could fault in the path's working set then, from inside
    the guest (M6). That is the next measurement.

### M28. A signal a process was started ignoring, and `sigwait`

- **Question.** Two E2E tests that send SIGINT to `shards run` and `shards vm run` hung
  in one full run and passed in the next. Where did the signal go?
- **Method.**
  - The hung client, found by `ps`, with the byte counters of its connection to the warm
    VM (`netstat -f unix -anv`): it had sent 119 bytes, exactly its `START` request for
    that command line, and no `SIGNAL` frame, before or after a second SIGINT sent by
    hand. The warm VM's end had received those bytes and the control data of their three
    descriptors (143), and held nothing.
  - SIGINT's disposition in the hung client, read with `lldb -p PID` calling
    `sigaction(2, NULL, &old)`: the handler was 1, `SIG_IGN`. SIGTERM's was `SIG_DFL`.
  - The hung runs had been started by a non-interactive shell as `cmd &`, which starts an
    asynchronous list with SIGINT and SIGQUIT ignored [POSIX.1-2024, XCU 2.9.3.1]. The
    ignore is inherited through fork and exec, down to the test's `shards` process.
  - XNU drops a signal whose disposition is `SIG_IGN` when it is sent, before it looks
    for a thread in `sigwait` (bsd/kern/kern_sig.c, psignal_internal). Linux keeps a
    blocked signal pending whatever its disposition (kernel/signal.c, sig_ignored).
  - Go's runtime, and so the Docker CLI, leaves SIGINT and SIGHUP ignored at start but
    takes them once `signal.Notify` asks for them [go: src/os/signal/doc.go], and
    `docker run`'s signal proxy asks for every signal (docker/cli run.go,
    notifyAllSignals).
  - The daemon test, run 10 times as `cmd &` with the fix below and 2 times without it,
    2026-09-29, this machine.
- **Results.** Without the fix, both runs as `cmd &` hung at their first SIGINT. With it,
  10 of 10 passed. SIGINT sent through the daemon (`shards kill -s INT`) had reached
  the same command, whose guest-side path was never at fault.
- **Consequence.** shards takes the signals it forwards as the Docker CLI does: having
  blocked them, it makes any it was started ignoring default again, and forwards them
  (`shards_ipc::take_forwarded`). One that would end it with nowhere to send it still
  leaves it alone if it was ignored. E2E tests start `shards run` and `shards vm run`
  with SIGINT and SIGQUIT ignored.
  - The same run showed a race in the test guest: `trap` checked for its signal and then
    called `pause()`, so a signal between the two left it waiting for ever. It now waits
    in `sigsuspend(2)`, which unblocks and sleeps at once.

### M29. What reading command lines as the Docker CLI does costs a run

- **Question.** D27 has the thin client read `run`'s command line as docker/cli does
  (shards_cmdline): every flag Docker has, pflag's rules, cobra's checks. What does that
  cost a pooled run?
- **Method.**
  - An interleaved A/B (docs/research/measurements/build-ab/ab.py): f779d1b against this
    change, each with its own daemon and home, `shards run --pull never alpine true`
    alternating between them, n = 3000 per arm. Each run is split by its timing line into
    the command's time in the guest and the rest; the median of the paired differences
    has a bootstrap 95% interval.
  - The client alone: its launch, with `shards daemon stop` of a home that does not
    exist, and its launch and parse, with `run --pull sometimes`, which the client
    refuses; n = 2000 each, paired the same way. Page faults and instructions from
    `/usr/bin/time -l`.
  - `sigaction` and `sysctl` costs from a C loop (20000 iterations).
  - 2026-09-29, this machine, load 4.3–7.9.
- **Results.**
  - With each arm's own template, the new build was slower by 71 µs (95% [54, 86]), 48
    of them in the guest. The guest runs the same shards-init in both arms. Restored from
    one template copied into both homes, the guest's time differed by 1 µs (95% [−5, 6]):
    the 48 µs was the two templates' different restore costs, the variance the image
    benchmark spreads over 10 templates.
  - Same template, final build (µs):

| Part | Arm | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|---|
| wall | f779d1b | 3000 | 5080 | 5649 | 7732 | 19730 |
| wall | D27 | 3000 | 5105 | 5669 | 7415 | 21754 |
| command | f779d1b | 3000 | 1471 | 1650 | 1932 | 7271 |
| command | D27 | 3000 | 1475 | 1636 | 1950 | 2655 |
| outside | f779d1b | 3000 | 3588 | 4092 | 5897 | 18185 |
| outside | D27 | 3000 | 3599 | 4100 | 5841 | 20336 |

  - Paired, D27 − f779d1b: wall +17 µs (95% [1, 37]), command +1 (95% [−5, 6]),
    outside +23 (95% [8, 34]).
  - At first the client's launch and parse cost 12 µs more (95% [7, 16]), its launch
    alone 8 (95% [4, 13]). `run` knows Docker's 115 flags, and the 95 it does not serve
    were a table of `&str`s, whose pointers filled a second 16 KiB page of
    `__DATA_CONST` that every parse faulted in. Listed instead as one string, read only
    when a command line names one of them, they took the page and 175 of the 328 new
    fixups with them: launch and parse then cost 5 µs more (95% [−1, 9]). Launch alone
    still cost 9 µs more (95% [5, 13]), from 6 more page reclaims (288 against 282) and
    60 thousand more of 16.8 million instructions, nearly all dyld's.
  - Taking the forwarded signals (M28) is 18 `sigaction` reads: 1.9 µs. One
    `KERN_PROC_PID` sysctl, which returns every ignored signal at once, costs 8.5 µs.
- **Consequence.** Reading command lines as the Docker CLI does costs a pooled run about
  20 µs, all of it outside the guest and most of it the larger client's launch. A table
  of rarely used `&str`s costs every parse that touches its page; such data stays out of
  the thin client's hot path. Two builds' runs compare only from one template.

### M30. A pooled run's working set, recorded and prefetched

- **Question.** A pooled run spends about 1.4 ms in the guest (M29), where a booted guest
  takes the same path in a fraction of it: 145 µs from connection to exec against 795
  restored (benchmarks.md, Run). How much of it is first touches of guest memory (M5,
  M27)? Is what a run touches the same from one restore of a template to the next, as
  REAP found for serverless functions (Ustiugov et al., ASPLOS 2021)? And does touching
  it before the request pay?
- **Method.**
  - *Recording* (`docs/research/measurements/working-set/experiment.patch`, the tracker):
    guest RAM and the image's pmem are mapped with no access (`hv_vm_protect`), so each
    16 KiB page's first touch exits to the VMM. The VMM notes the page, the time, and
    the kind of touch (fetch, read, write, table walk), then gives it back: read-execute
    for a read, so that a later write exits too.
    - Warm VMs of one template: n = 7 runs of `shards run --rm alpine true` and n = 4 of
      `sh -c 'ls / >/dev/null; cat /etc/os-release >/dev/null'`. `pages.py` splits each
      VM's pages into before its request, while serving it (request to answer), and
      after.
    - Then a template saved afresh, recorded from its snapshot on, against 5 warm VMs of
      it.
  - *Prefetch* (the same patch). A warm VM does one of four things before its vCPU
    state is restored, chosen by process ID within one daemon and template, so the arms
    interleave (`ab.py`, n ≈ 100 each):
    - 0: nothing;
    - 1: vCPU 0 reads each recorded page, with its MMU off;
    - 2: as 1, after the host has written a byte of each page the guest wrote, copying it;
    - 3: vCPU 0 reads each page and adds zero atomically to each page the guest wrote,
      with its MMU on over an identity map of write-back memory.

    The list is one warm VM's pages up to its answer (`list.py`): 731, 308 of them
    written. Memory is the process's `phys_footprint` after the prefetch, when the
    request came and at the answer, with `footprint` and `vmmap` for its regions.
  - *The change*, as M29 measures one (`build-ab/ab.py`): a0f7926 against this change,
    both restoring one template (this change's, working set included),
    `shards run --pull never alpine true`, n = 1000 each.
  - 2026-09-29, this machine, load 4–12.
- **Results.**
  - A warm VM running `alpine true` first touches 350 pages between its restore and the
    request, 381 while serving it, and 69 after. Of the 381:
    - 97 fetched (kernel text), 110 read and 85 written first, all in RAM;
    - 86 read in the image;
    - 4 table walks;
    - 165 written by the answer.
  - The set is stable. 379 of the 381 pages were the same in all 7 VMs, and one VM's set
    covered another's by 98.4–100%. For `sh -c 'ls; cat'`, 467 pages were touched
    while serving it, 81% of them `true`'s: the kernel's path, init's and the loader's
    are most of it.
  - The boot that saves the template, recorded from its snapshot to its first answer,
    touched 718 pages, 304 of them written. They covered 99.7–100% of each warm VM's
    pages while serving (99.4–100% of those written) and 95–96% of those before the
    request; 3–4 went unused.
  - Recording slows the guest: `true` took 9.9 ms from request to answer instead of
    1.5 ms.
  - Prefetch arms, load 5.2 (µs; memory in MiB of `phys_footprint`, at the request and
    at the answer):

| Arm | n | guest p50 | p90 | p99 | wall p50 | p90 | p99 | prefetch p50 | memory waiting | at answer |
|---|---|---|---|---|---|---|---|---|---|---|
| 0 none | 95 | 1975 | 2454 | 2679 | 7481 | 8657 | 9423 | 0 | 4.4 | 7.1 |
| 1 reads, MMU off | 106 | 1420 | 1952 | 2304 | 6347 | 7970 | 8549 | 911 | 4.4 | 7.1 |
| 2 reads, host copies | 97 | 804 | 935 | 1075 | 6352 | 7510 | 8183 | 1732 | 12.0 | 12.0 |
| 3 reads and writes, MMU on | 102 | 768 | 938 | 1044 | 6396 | 7215 | 7711 | 1964 | 7.2 | 7.3 |

  - Arm 3 against arm 0: guest −1207 µs (95% [−1516, −961]), wall −1085 µs (95%
    [−1583, −614]). Arm 2: guest −1171, wall −1128. Arm 1: guest −555.
  - The guest's own copy-on-write pages count in `phys_footprint`, but in no region that
    `footprint` or `vmmap` shows: they are mapped at stage 2 alone. Arm 2's host copies
    show in both. The 308 pages (4.8 MiB) the host copied added 9.7 MiB of footprint
    (11.8 MiB after the prefetch, against arm 0's 2.1). Whether the host allocated them
    twice or counts them twice, free memory with 24 VMs per arm was too noisy to tell.
    Arm 3 copies at stage 2 only: 6.9 MiB after the prefetch, and at the answer about
    the same as arm 0.
  - The change (µs):

| Part | Arm | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|---|
| wall | a0f7926 | 1000 | 5155 | 5968 | 7081 | 10604 |
| wall | working sets | 1000 | 4252 | 5236 | 6957 | 14022 |
| guest | a0f7926 | 1000 | 1422 | 1535 | 1765 | 1955 |
| guest | working sets | 1000 | 517 | 586 | 867 | 1078 |
| outside | a0f7926 | 1000 | 3703 | 4571 | 5726 | 9086 |
| outside | working sets | 1000 | 3703 | 4738 | 6488 | 13542 |

  - Paired, working sets − a0f7926: wall −857 µs (95% [−891, −834]), guest −905
    (95% [−912, −898]), outside +45 (95% [16, 64]).
  - Most of the outside's 45 µs is the pool's refill. It starts at the handover, and its
    restore and prefetch now overlap the run's tail rather than its longer guest part. The same
    build refilling at the run's end instead (`build-ab/ab.py` with `ENV_NEW`, n = 1000,
    load 6.7–7.8) saved 49 µs of wall time at the median (95% [23, 75]): outside 30,
    guest 15. Its p99 was 6336 µs against 5794, as a refill at the end lands on the start
    of a run that follows at once; this n does not settle that difference.
- **Consequence.** D25, D26:
  - The boot that saves a template records the working set from its snapshot to its
    first answer, 50 ms after the request at most, and saves it with the template.
  - Warm and held restores prefetch it as arm 3 does, before the guest runs.
  - A waiting warm VM holds the pages its run will write, about 2.9 MiB for `true`.
    Arm 2's host copies count twice their size, and arm 1 leaves the writes' faults on
    the request's path.
  - The pool keeps refilling at the handover: refilling at the run's end is worth 49 µs
    at the median, at a risk to the tail of back-to-back runs.

### M31. A blocked signal whose default is to ignore it, and `sigwait`

- **Question.** `run -t` resizes the command's terminal when the client gets SIGWINCH,
  which the client forwards with every other signal from a `sigwait` thread (M28). On the
  Mac it never came. Does XNU deliver a blocked signal whose default action is to ignore
  it?
- **Method.**
  - `docs/research/measurements/signal-default-ignore/probe.c`: for SIGWINCH, SIGIO,
    SIGCONT and SIGINT, a new process blocks the signal, sets its disposition (SIG_DFL,
    or a handler), sends it to itself, and checks `sigpending`.
  - The same probe, built for Linux (`zig cc -target aarch64-linux-musl`), in Docker
    Desktop's VM.
  - XNU's source at the version this macOS runs (apple-oss-distributions/xnu
    xnu-12377.121.6).
  - 2026-09-29, this machine: macOS 26.4.1 (Darwin 25.4.0); Linux 6.12.76-linuxkit.
- **Results.**

| Signal | XNU, SIG_DFL | XNU, handler | Linux, SIG_DFL | Linux, handler |
|---|---|---|---|---|
| SIGWINCH | discarded | pending | pending | pending |
| SIGIO | discarded | pending | pending | pending |
| SIGCONT | pending | pending | pending | pending |
| SIGINT | pending | pending | pending | pending |

  - XNU's `setsigvec` puts a signal in `p_sigignore` when it is SIG_IGN, or SIG_DFL with
    a default of ignoring it, but for SIGCONT (bsd/kern/kern_sig.c:689-703), and
    `psignal_internal` discards any signal in `p_sigignore` when it is sent, blocked or
    not (:2133-2136). Linux's `sig_ignored` never ignores a blocked signal, "since the
    signal handler may change by the time it is unblocked" (6.18.48 kernel/signal.c:
    106-114).
- **Consequence.** D26: `shards_ipc::take_forwarded` gives SIGWINCH and SIGIO a handler,
  which never runs while they stay blocked for `sigwait`, as Go's `os/signal` gives every
  signal it is notified of. Before, neither the client nor `vm run` forwarded them on the
  Mac.

### M32. A keystroke's echo through `run -it`

- **Question.** Under `run -it` a key's echo crosses the VM twice: the client reads it,
  the warm VM sends it into the guest over vsock, the guest's pty echoes it, and init
  sends the echo back out (tty-and-interactive-runs.md E1). How long does a user wait
  for it, against Docker's own path on this Mac?
- **Method.** `docs/research/measurements/tty-echo/echo.py`: three sessions, each on a
  pty led by a shell, as a terminal window runs one, stay open together and take turns,
  one keystroke per round in rotating order, 5 ms apart:
  - `shards run --rm --pull never -it alpine sh -c 'echo ready; sleep 3600'`, a pooled
    run of 1b429f3;
  - `docker run --rm -it --init alpine sh -c '…'`, Docker Desktop's engine 29.3.1, its
    Linux VM on this Mac; `--init`, so ^C ends it as it ends shards' command;
  - `sh -c '…'` on the pty itself: the host kernel echoes, the harness's floor.

  A sample is the time from writing `a` to the master to reading its echo back, n = 1000
  per arm after 20 unrecorded. 2026-09-29, this machine, load 7.8–14.2.
- **Results** (µs):

| Session | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| `shards run -it` | 1000 | 265 | 339 | 387 | 530 |
| `docker run -it` (Docker Desktop) | 1000 | 447 | 627 | 1182 | 1949 |
| local pty | 1000 | 22 | 33 | 47 | 59 |

- **Consequence.** A key's echo takes a quarter of a millisecond, about 240 µs past the
  host's own; Docker Desktop's takes 1.7 times as long at the median and 3 times at
  p99. The path is two vsock crossings, two guest wakeups (M8) and the guest's pty; what
  of it is worth shortening is not measured.

### M33. Working sets on nested KVM, and where a restored guest's time goes

- **Question.** On GitHub's x86_64 runners KVM runs nested, under Microsoft's
  hypervisor. There a restored guest took 20 ms from its release to run again, against
  17 µs on the Mac, and a pooled `true` 81 ms (benchmarks.md, Restore and Image). What
  takes the time? Do working sets (M30) help KVM as they help HVF, and which run should
  record them?
- **Method.**
  - *Counters.* KVM's binary statistics (api.rst 4.133, `KVM_GET_STATS_FD`) of each vCPU
    and VM, read at teardown and at a pooled run's request, its command's start and its
    answer. With them, the guest RAM pages each VM's host had mapped at the request and at
    the answer (`/proc/self/pagemap`: present and file-page bits). These were temporary
    diagnostics on branches `kvm-restore-perf` and `kvm-ws-diag`, not merged.
  - *Pooled runs* of `shards run --pull never alpine true` from one template, 40 per arm,
    alternating on one runner, with the working set and without it; 3 runners per
    experiment.
  - *Paired A/B* (`build-ab/ab.py`, n = 100–200 pairs, 3–5 runners per experiment).
    - The template's working set against none, its file removed from one arm's copy.
    - Later, two homes with the same template and working set, on one build, whose arms
      differ only in what a restore does with the set: nothing, copying the pages written
      (`MADV_POPULATE_WRITE`), or that and mapping every page ahead
      (`KVM_PRE_FAULT_MEMORY`). This was a temporary switch, on branch `kvm-ws-ab`.
    - `AB_PAUSE` 10 ms or 500 ms between runs.
  - GitHub `ubuntu-24.04` runners on 2026-09-29: AMD EPYC 7763, 9V74 and 9V45, and Intel
    Xeon 8370C, 8573C and 6973P-C, all on Linux 6.17.0-1022-azure. The guest was
    vmlinux-6.18.48, 1 vCPU, 256 MiB.
- **Results.**
  - A restored guest's memory is file-backed, so KVM maps it 4 KiB at a time. Up to the
    guest's first act after its release it took 1,488 stage-2 faults and 986 4 KiB pages.
    A boot's anonymous, THP-backed memory took 35 faults and 31 2 MiB pages. 227 of the
    restore's pages were first mapped read-only from the file, then written. Each of
    those took a fault, a copy, a remote TLB flush and a second fault.
  - *A working set from the run that saves the template.* At the snapshot, that run's RAM
    was mapped afresh from the snapshot's file and its pmem from the image. It cut a pooled
    VM's faults from 2,480 to 410, but in the resume, before the request.
    - While serving the request, a restored VM first mapped 295–381 pages. Only 18–63 of
      them (5–16%) were in that working set, and 1–2 of the 121–168 it wrote were
      recorded as written.
    - Paired, it changed the command's time by +92 µs (95% [−109, +245]) of 4.5 ms (Intel
      6973P-C), +598 µs ([+345, +848]) of 6.0 ms (8573C), and −930 µs ([−1462, −173]) of
      194 ms (AMD 9V74).
  - *Restores touch the same pages as each other.* A median of 99.7–100% of one VM's
    request pages were in the next VM's. A restored guest's own resume (a new generation
    ID, vsock's reset and reconnection) moves its allocations away from the saving run's
    before the request.
  - *A working set from the first warm restore.* It was recorded to the answer, up to 1 s
    after the command's start. Paired, on 5 AMD runners (µs, command p50 without the
    working set in brackets):

| Arms | 10 ms between runs | 500 ms between runs |
|---|---|---|
| none → copy and map ahead | −26,440 to −39,043 (190,769–225,668) | −26,579 to −38,926 |
| copy only → copy and map ahead | −9,828 to −13,592 | −9,232 to −12,867 |

  - Every 95% interval there excluded zero, by at least 7.9 ms. An earlier run recorded
    only 50 ms past the command's start: none against both was −25.4 ms ([−25.8, −25.0])
    of 39.5 ms on the Intel 8573C, and −23.4 to −27.9 ms on 4 AMD runners. Mapping ahead
    beyond the copies gained 6.2–7.3 ms on AMD, but cost 1.4 ms ([+1.1, +1.9]) on that
    Intel host.
  - *The prefetch's own time, before the release.* Copying 341–476 written pages took
    0.7–1.4 ms. Mapping 3,472–3,867 pages ahead took 2.5–4.4 ms (Intel 8370C) and
    3.8–8.9 ms (AMD).
  - *The rest of a run's path is mostly waiting.* From the request to the command's start
    took 5 ms on the Intel 8370C, with 6 faults and 21 exits (medians). On the AMD 7763s
    it took 28 ms, with 12 faults and 62 exits. `true` then took 125 ms to answer there,
    with 375 faults and 580 exits.
  - The same guest's `true` took 4.5–40 ms on the Intel runners, 150–225 ms on the AMD
    ones, and 0.5 ms on the Mac (M30). The host's hypervisor, not the guest, sets the
    scale.
- **Consequence.**
  - On KVM the first warm restore without a working set records one (`vm::RESTORES_RECORD`),
    up to 1 s past its command's start. Recording costs it nothing: its mappings start
    empty, and `pagemap` is read once, after the answer.
  - A restore copies the pages written, then maps every page ahead before the guest
    runs.
  - HVF keeps recording in the run that saves the template, where recording slows the
    guest sixfold and the set covers 99.7–100% (M30).
  - Nested KVM's runs are bound by waiting for vCPUs and threads to wake, through the
    outer hypervisor. Working sets cannot remove that, and it is not measured here.
  - These are nested hosts only. On bare metal a fault costs less, and so does waking a
    vCPU; neither is measured.

### M34. What a VM process's binary costs it

- **Question.** The Firecracker comparison (benchmarks.md) measures each VMM's resident
  memory outside guest memory, by Firecracker's rule, while its guest idles. shards'
  was 2.4 MiB when last recorded (9ef57c2), against Firecracker's 4.5. On 2026-09-29 CI
  measured 3.8 MiB. What grew, and when?
- **Method.**
  - *History.* The `shards_overhead` and `fc_overhead` medians of every CI run's
    Firecracker comparison (n = 30 each), read from 49 runs' logs, 2026-09-29, AMD EPYC
    7763, 9V74 and 9V45 and Intel Xeon 8370C and 8573C runners, Linux 6.17.0-1022-azure.
  - *The binary.* The VM process's binary built for x86_64-unknown-linux-musl, static
    and position-independent, before and after the growth: `llvm-readelf -S -r`, the
    sections a process touches as it starts, and its relative relocations, each of
    which writes a pointer into its data as the process starts.
- **Results.**
  - Through df8f91c the overhead was 2.6–2.7 MiB. At 93cc1b7 it was 3.7, and from there
    3.6–4.0. Firecracker's was 4.5 in every run. The step came with 93cc1b7: the binary
    that runs each VM, `shardsd`, began to link the registry client (rustls, AWS-LC), to
    serve `shards pull`, and later the daemon and containers.

| x86_64 musl | VM binary at df8f91c | `shardsd` at 593fdee | `shards-vm` |
|---|---|---|---|
| `.text` | 614 KB | 5,264 KB | 756 KB |
| `.rodata` | 55 KB | 811 KB | 63 KB |
| `.data.rel.ro` | 10.7 KB | 231.8 KB | 12.0 KB |
| `.rela.dyn` | 17.6 KB | 316.2 KB | 19.7 KB |
| relative relocations | 735 | 13,177 | 821 |

  - A VM process runs one of `shardsd`'s commands, but maps and relocates all of it: every
    page of `.data.rel.ro` that holds a pointer becomes a private copy as the relocations
    are applied, all of `.rela.dyn` is read, and the code it runs is spread across a text
    8.6 times the size.
  - On the Mac, `shardsd` also loads Security and CoreFoundation for TLS, which a VM
    process never uses.
- **Consequence.** VM processes run a binary of their own, `shards-vm`, which links the
  VMM and what a VM process runs, and nothing of `shardsd`'s: on the Mac it loads
  Hypervisor.framework alone.
- **After** (da483ac, AMD EPYC 9V74; fd7628d, 7763; n = 30 each): the overhead was
  2.7 MiB at p50 and at max in both runs, against Firecracker's 4.5.

### M35. How many bytes one read or write may ask for

- **Question.** `GuestMemory::save` writes each run of pages the guest used with one call,
  and a run can be as long as guest memory. Does every host take a call that long?
- **Method.**
  - `docs/research/measurements/rw-limit/probe.c`: for INT_MAX and INT_MAX + 1 bytes of
    memory mapped and never touched, `pwrite` to /dev/null, which takes any count without
    reading it, and `pread` from an empty file, which has nothing to give.
  - 2026-09-29, this machine: macOS 26.4.1 (Darwin 25.4.0, xnu-12377.101.15); the same
    probe built with `zig cc -target aarch64-linux-musl`, in Docker Desktop's VM, Linux
    6.12.76-linuxkit.
  - XNU's source (apple-oss-distributions/xnu xnu-12377.121.6) and Linux's (v6.17).
- **Results.**

| Call | XNU, INT_MAX | XNU, INT_MAX + 1 | Linux, INT_MAX | Linux, INT_MAX + 1 |
|---|---|---|---|---|
| `pwrite` to /dev/null | 2,147,483,647 | EINVAL | 2,147,479,552 | 2,147,479,552 |
| `pread` from an empty file | 0 | EINVAL | 0 | 0 |

  - XNU refuses a count over INT_MAX before it looks at the file: `dofileread`,
    `read_internal`, `dofilewrite` and `write_internal` return EINVAL
    (bsd/kern/sys_generic.c:309, 356, 637, 684).
  - Linux cuts a count over `MAX_RW_COUNT`, INT_MAX rounded down to a page
    (include/linux/fs.h:2829), to it and returns what it moved (fs/read_write.c:566-567
    in `vfs_read`, 680-681 in `vfs_write`).
- **Consequence.** `platform::read_at` and `write_at` ask for at most INT_MAX bytes on
  Apple hosts, as Rust's std caps its own reads and writes (1.100.0-nightly 2026-09-04,
  library/std/src/sys/fd/unix.rs:69-81), and their callers loop until done. Without the
  cap, a snapshot whose guest used more than 2 GiB in one run would fail on the Mac, as
  would a guest's block request over 2 GiB (a descriptor's length has 32 bits).

### M36. What shipping the guest costs, and what `cargo install` honors

- **Question.** For `shards run IMAGE` to work on first use, shards ships its kernel and
  shards-init (docs/research/shipping-the-guest.md). How large is each? And can a build
  script embed shards-init when shards is installed with `cargo install`?
- **Method.** 2026-09-29, this machine: Apple M5 Max, macOS 26.4.1 (25E253), Rust
  1.98.0; the default toolchain, `stable`, is 1.94.1, without musl targets. Each build
  ran once, so sizes are exact and times indicative.
  - *S1, the kernel.* The assets of release `kernel-6.18.48-1bff175d35cb` by
    `Content-Length`, and the aarch64 kernel compressed with gzip -9 (Apple gzip 479),
    zstd -19 (1.5.7) and xz -9e (5.8.4).
  - *S2, shards-init* at 09a7226: `cargo build -p shards-init --profile guest --target
    <arch>-unknown-linux-musl` into a fresh target directory.
  - *S3, the host binaries* at 5926dc1: `cargo build --release -p shards`.
  - *S4, `cargo install`.* `docs/research/measurements/cargo-install-guest/run.sh`
    builds a workspace laid out like shards': `rust-toolchain.toml` pins 1.98.0 with both
    musl targets, and `.cargo/config.toml` links musl with `rust-lld`. Its host package's
    build script runs `cargo build -p <guest> --profile guest --target
    <arch>-unknown-linux-musl --target-dir $OUT_DIR/…`, embeds the result with
    `include_bytes!`, and records what it saw. The script builds and installs it six
    ways. Three runs gave the same results.
- **Results.**
  - S1: `Image-6.18.48-aarch64` is 18,883,072 bytes and `vmlinux-6.18.48-x86_64`
    27,708,976. The aarch64 kernel compresses to 8,350,871 bytes (gzip), 7,022,164 (zstd)
    and 6,104,196 (xz).
  - S2: shards-init is 428,912 bytes for aarch64, static and stripped (228,401 gzipped),
    built in 3.23 s; and 481,792 for x86_64, static-pie, in 3.73 s. The aarch64 build had
    the same SHA-256 as an earlier build in another target directory.
  - S3: `shardsd` is 5,547,896 bytes, `shards-vm` 957,032 and `shards` 598,904.
  - S4:

| Case | Command | Outer toolchain | Repo config | Nested musl build |
|---|---|---|---|---|
| A | `cargo build`, inside the repo | 1.98.0 | applied | ok, 402,832 bytes embedded |
| B | `cargo install --path host`, inside the repo | 1.98.0 | applied | ok |
| C | `cargo install --path <repo>/host`, elsewhere | 1.94.1 | applied | `error[E0463]: can't find crate for std` |
| D | `cargo install --git file://<repo>`, elsewhere | 1.94.1 | not applied | the same |
| E | D with `RUSTUP_TOOLCHAIN=1.98.0` | 1.98.0 | not applied | ok |
| F | E with the musl linker set to `cc` | 1.98.0 | not applied | ``linking with `cc` failed`` |

  - In C and D the build script saw `RUSTUP_TOOLCHAIN=stable-aarch64-apple-darwin`. The
    nested cargo ran in the checkout, beside its `rust-toolchain.toml`, and still used
    the outer toolchain. rustup sets `RUSTUP_TOOLCHAIN` for the tool it runs, and the
    variable outranks the file (rustup 1.29.1 src/toolchain.rs:167-183,
    doc/user-guide/src/overrides.md:3-20).
  - In E the nested build ran in the checkout and read its `.cargo/config.toml`, although
    the outer build did not. F shows what it needed from that file: without `rust-lld`,
    rustc links with `cc`, and Apple's clang cannot link a Linux ELF.
- **Consequence.** See shipping-the-guest.md §3. A build script that embeds shards-init
  must name the musl linker itself, and fail with a clear error when the musl standard
  library is missing. An install from source runs inside the checkout.

### M37. How a snapshot's memory is written, and how fast it restores

- **Question.** shards wrote a snapshot's memory one 4 KiB page per `pwrite`, where
  Firecracker writes its memory file in large writes. On Linux 6.17's ext4, a write's
  length sets the order of the page-cache folios it fills (fs/ext4/inode.c:1318,
  `write_begin_get_folio`; include/linux/pagemap.h:766-778), and a restore maps the file
  those folios hold. Does the write pattern change how fast a restored guest runs?
- **Method.**
  - `docs/research/measurements/snapshot-write-ab/per-page.patch` adds a third variant to
    the Firecracker comparison's restores (crates/shards/benches/firecracker.rs): shards'
    snapshot, copied with its memory rewritten one page per `pwrite` of each page that is
    not all zero, as `GuestMemory::save` wrote it before e9ed44d.
  - Each iteration restores the batched snapshot, the copy and Firecracker's, in a fresh
    process each, with the copy first or last in turn. Each sample is the time from
    spawn until the guest's first beat after the restore reaches the VMM's stdout. There
    were 3 warm-ups, then n = 20 per variant.
  - The hosts were GitHub's ubuntu-24.04 runners on 2026-09-29, with Linux
    6.17.0-1022-azure nested under Hyper-V: AMD EPYC 7763, AMD EPYC 9V45 and Intel Xeon
    Platinum 8370C.
  - Revision cd99f57 is 21772e3 plus diagnostics that read KVM's tracepoints and debugfs
    through sudo around each sample. The guest had 1 vCPU and 128 MiB, and the kernel
    was vmlinux-6.18.48-x86_64-1bff175d35cb.
- **Results** (ms, p50 / p90 / p99, where p99 is the maximum at n = 20).

| Host | Batched | Per page | Firecracker |
|---|---|---|---|
| AMD EPYC 7763 | 17.4 / 18.2 / 20.6 | 22.4 / 22.6 / 22.8 | 12.0 / 12.7 / 15.9 |
| AMD EPYC 9V45 | 8.5 / 9.2 / 10.2 | 11.3 / 12.0 / 12.1 | 9.3 / 9.8 / 10.0 |
| Intel Xeon 8370C | 7.6 / 8.2 / 17.7 | 9.2 / 18.1 / 22.0 | 8.6 / 9.6 / 10.3 |

  - The batched snapshot restored 17–25% sooner at p50 on every host.
  - Before its first beat it took fewer nested page faults, by KVM's `kvm_exit`
    tracepoint (medians): 472 NPT faults against 756 on the 7763, 440 against 689 on the
    9V45, and 361 EPT violations against 570 on the Xeon.
- **Consequence.** `GuestMemory::save` writes each run of used pages with one call
  (e9ed44d).
- **Open.**
  - Where in the kernel's fault path the fewer faults come from.
  - The Xeon's outliers, which took more EPT violations and more time in the guest than
    that host's other samples.
  - The 7763's gap to Firecracker. In the untraced comparison of 21772e3, on another
    7763 runner, the medians were 17.2 ms for shards and 19.0 for Firecracker.

### M38. The comparison's guests replayed their crypto self-tests

- **Question.** In the restore comparison (crates/shards/benches/firecracker.rs), how
  soon each VMM's guest beat again varied by host and by VMM more than any counter
  explained. shards led on some runners and trailed by 5 ms on others [M37]. What did
  the guests do between their restore and their first beat?
- **Method.**
  - `docs/research/measurements/restore-profile/diagnostics.patch` traces KVM's
    `kvm_exit`, `kvm_entry` and `kvm_userspace_exit` around each restore, on the trace
    clock CLOCK_MONOTONIC, and keeps only the events before the first beat. It counts
    each exit's guest RIP by reason and reports each restore's 80 most frequent RIPs.
  - `profile.py` resolves them against the kernel's symbols (`llvm-nm` of
    vmlinux-6.18.48-x86_64-1bff175d35cb, which runs at its link addresses [M21]).
  - GitHub runners, 2026-09-29, Linux 6.17.0-1022-azure: AMD EPYC 9V74 and 7763.
    Revision 1610645 (aaef171 plus diagnostics). n = 20 per variant.
- **Results.**
  - Before its first beat, every restore under both VMMs ran the kernel's crypto
    self-tests: exits whose RIPs were in the multi-precision arithmetic under RSA
    (`mpihelp_mul_1`, `mpihelp_addmul_1`, `mpih_sqr_n_basecase`) and in
    `crypto_alg_tested`. This held in 20 of 20 restores of each variant.
  - How much each did depended on its snapshot, and the VMM whose guest had more left
    beat later:

| Host | shards: crypto exits, first beat p50 | Firecracker: crypto exits, first beat p50 |
|---|---|---|
| AMD EPYC 9V74 | 26.0 per restore, 14.8 ms | 16.0 per restore, 9.9 ms |
| AMD EPYC 7763 | 11.8 per restore, 14.1 ms | 24.6 per restore, 16.8 ms |

  - The test guest's `beat` mode printed READY as soon as it started. shards'
    snapshot came 20 beats later; Firecracker's came after the first beat and two API
    calls. Both fell inside the ~40 ms the self-tests take after boot [M21], each at a
    different point, and every restore replayed the rest.
- **Consequence.**
  - The `beat` guest now waits for the self-tests before READY, as shards-init does
    before it saves a template [M21], so both VMMs snapshot a quiet guest.
  - Restore comparisons made before then measured the self-tests' tails as much as the
    VMMs.
- **After** (6e2a83e: 9fb77f5 plus diagnostics; n = 20 per variant). No restore under
  either VMM ran self-test code, and the medians agreed across runners:

| Host | shards | Per page [M37] | Firecracker |
|---|---|---|---|
| AMD EPYC 9V74 | 10.2 ms | 13.1 ms | 12.9 ms |
| AMD EPYC 7763 | 9.4 ms | 12.1 ms | 12.0 ms |
| AMD EPYC 7763 | 9.8 ms | 12.5 ms | 12.5 ms |

  - One shards restore on the first 7763 took 17.6 ms. Everything in it was about twice
    as slow as in the other samples on that runner: shards-vm's own setup before the
    guest ran (3.4 ms against 1.8), the time handling the same 347 nested page faults
    (3.4 ms against 1.75), and the guest's time. That points to the host, but it is not
    proven.

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
     context (helper vCPUs), not by host reads. It is, for pooled runs (M30).
   - Every page a restored guest writes costs a ~1.9 µs CoW fault plus 16 KiB of
     private memory. A page the host copies counts twice in the process's footprint
     once the guest maps it (M30).
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

### M39. The pinned builder's kernel, and a tick lost to the crypto self-tests at boot

- **Question.** Release kernel-6.18.48-296d2de54137 is the first built in the pinned
  builder (resources/kernel/builder.env: Debian's gcc 12.2.0 and ld 2.40, where the
  runners' own toolchains built the releases before), and it carries patch 0001
  (fs/dax). Does it boot and restore as fast as kernel-6.18.48-1bff175d35cb?
- **Method.**
  - `docs/research/measurements/kernel-ab/ab.py` boots shards-init, and restores a
    snapshot of the `resume` test guest taken with the same kernel, each in a fresh
    `shards vm` process. The kernels alternate, A then B, then B then A. After 3
    warm-ups, n = 200 each, timed by the VMM's clock.
  - Apple M5 Max (Mac17,6), macOS 26.4.1, revision 2ec158b, 1 vCPU and 256 MiB.
- **Results** (µs, p50 / p90 / p99 / max).

| Phase | 1bff175d35cb | 296d2de54137 |
|---|---|---|
| kernel: guest entry → PID 1 | 15903 / 25985 / 26267 / 26313 | 15884 / 26048 / 26203 / 26232 |
| to_init: VMM `main` → PID 1 | 18771 / 28858 / 29343 / 30083 | 18686 / 28913 / 29296 / 29531 |
| restore: VMM `main` → RESUMED | 1149 / 1314 / 1486 / 1586 | 1150 / 1313 / 1521 / 1938 |

  - The two kernels are indistinguishable.
  - Under both, the kernel phase has two modes, 10 ms apart: 15.5–16.3 ms, and
    25.5–26.3 ms in 57 and 49 of 200 boots. 10 ms is one tick at the configuration's
    `CONFIG_HZ=100`.
- **Where the tick goes.**
  - `boot-log.py` boots the test guest in its `kmsg` mode, which prints the kernel's log
    once the boot is over. Under `quiet`, the log costs the boot no console output. By
    the kernel's own clock, 31 of 100 boots ran init at about 22.4 ms and the rest at
    about 12.3 ms.
  - The slow boots fell 10.05 ms behind at a single step: between the late initcall
    that registers encrypted keys and "clk: Disabling unused clocks". With
    `initcall_debug`, which made every boot slow, `deferred_probe_initcall` took 9.7 ms
    there and probed nothing.
  - Just before it, `crypto_algapi_init` starts the crypto self-tests the boot deferred,
    one `cryptomgr_test` kthread per algorithm, at normal priority (crypto/algapi.c
    `crypto_start_tests`, crypto/algboss.c `cryptomgr_schedule_test`; unchanged in
    v7.2-rc4). The kernel is `PREEMPT_NONE` and the guest has one vCPU. When PID 1 sleeps
    in `deferred_probe_initcall`'s `flush_work`, the tests can take the CPU, and PID 1
    gets it back at the next tick.
  - With `cryptomgr.notests=1`, none of 100 boots was slow: init ran at 10.6–11.3 ms
    (p50 11.0 against 12.4). `fw_devlink=off` and `=permissive` left the slow share
    where it was, at 35–38% of 60 boots each.
- **Consequence.**
  - shards pins kernel-6.18.48-296d2de54137.
  - The self-tests stay on (resources/kernel/README.md). The fix lies in how they share
    the CPU with the end of boot: their priority, the preemption model or `HZ`. Each
    changes more than the boot, so each is to be measured
    (docs/audit/2026-09-29_response.md).
  - Templates never see the self-tests: shards-init waits for them before saving one
    [M21], so restores skip them [M38]. Only boots pay.
- **Open.** Why the boots of Firecracker's CI kernel in docs/benchmarks.md, on
  2026-09-28, had one mode, around 18.5 ms.

### M40. Sparse snapshot files, materialized RAM, and CoW stores

- **Question.** What do production RAM mapping/saving APIs allocate and fault, and
  does sparse output preserve sparse anonymous input? What does an identical store cost?
- **Method.** [Harness](measurements/audit-memory/README.md): actual GuestMemory APIs,
  64 MiB RAM, one byte touched per 16 KiB host page, fresh process per condition,
  discarded warmup, randomized condition order, n=30. Nonzero file backing is cache-hot
  from preparation. Count samples are separate from timings. Host getrusage fault
  deltas and proc_pid_rusage RSS/physical-footprint deltas are captured, excluding
  hypervisor/stage-2 mapping. Snapshot cases use untouched zero RAM or one nonzero byte
  per MiB. Post-save sampled restores pass; private stores leave the backing unchanged.
- **Host.** 2026-09-29, Apple M5 Max, Mac17,6, 128 GiB, macOS 26.4.1/Darwin 25.4.0,
  Rust 1.98.0, revision 38457b709e67c4a43de87adfd7f2053531672dc2. Source/binary hashes,
  method, all raw samples and every case's quantiles are in
  [results.json](measurements/audit-memory/results.json).
- **Results** (ms, p50 / p90 / p99 / max; medians for faults/resource deltas).

| Actual operation | Time | Host minor faults | RSS delta | Physical-footprint delta |
|---|---|---:|---:|---:|
| File read, every page | 2.201 / 2.316 / 2.526 / 2.526 | 4096 | 64 MiB | about 48 KiB |
| File read+identical write, every page | 10.627 / 11.217 / 11.259 / 11.259 | 8192 | 64 MiB | about 64 MiB |
| Identical write after full file read | 8.586 / 9.306 / 9.749 / 9.749 | another 4096 | 0 | another 64 MiB |
| File read/write, 64 pages one per MiB | 0.183 / 0.223 / 0.410 / 0.410 | 128 | 1 MiB | about 1.047 MiB |
| Save untouched zero anonymous RAM | 19.778 / 20.012 / 22.366 / 22.366 | 4096 | 64 MiB | about 64 MiB |
| Save anonymous RAM with 64 nonzero pages | 19.719 / 20.513 / 48.799 / 48.799 | 4032 | 63 MiB | 63 MiB |

  - Both saves make zero Rust heap allocations. The zero file is logically 64 MiB but
    has zero allocated data blocks; the sparse-used file allocates 1 MiB.
  - 64 identical one-byte stores still create 1 MiB of private copies; pre-reading
    does not eliminate the later CoW faults. Physical footprint is process accounting,
    not PSS or proof of fleet unique physical bytes. All median major-fault deltas are 0.
- **Consequence.** Audit D01/D02: count page materialization alongside allocations;
  bound speculative private prefetch and optimize full-RAM zero scans. Skipping a
  nonresident page is unsafe without zero/dirty provenance. This does not measure a
  completed optimization, full snapshot transaction, real VM or Firecracker comparison.
- **Image allocation companion.** The same harness calls actual Tree/EROFS APIs on
  10,000 files with a black-boxed fill source/byte-counting sink, excluding tar and I/O.
  Empty-file writing makes 20,128 allocations/341 reallocations; 512-byte inline files
  make 30,128/10,341. Their writer peak requested live heap is 4,263,451/9,797,147 bytes.
  With 1,024-byte xattrs, writing makes 40,128/40,341 and peaks at 38,495,931 bytes.
  One tree generation retains 17,816,656 bytes; four replacements of the same names
  retain 69,088,816, while output still has 10,001 reachable inodes. All n=30 timing
  distributions and separate counts are recorded. Heap capacity is not RSS.

### M41. Virtqueue allocation, readiness rebuilding, and bounded live buffers

- **Question.** What heap/syscall work recurs per device operation?
- **Method.** [Harness](measurements/audit-device-allocations/README.md): actual public
  Queue, production poll.rs included unchanged, one untimed allocation census;
  counters disabled for n=20,000 queue timings after 500 warmups, n=2,000 zero-timeout
  ready-socket timings after 100 warmups. Host/revision/compiler are M40's.
  No VM starts. [Results](measurements/audit-device-allocations/results.json) and
  [176,000 chronological timing samples](measurements/audit-device-allocations/results-samples.jsonl)
  include source hashes and all quantiles. Small timings approach clock resolution.
- **Results** (ns, p50 / p90 / p99 / max).

| Operation | Rust alloc / realloc | Time |
|---|---|---|
| Queue direct, 3 descriptors | 1 / 0 | 41 / 42 / 42 / 167 |
| Queue direct, 256 descriptors | 1 / 6 | 583 / 875 / 959 / 39792 |
| Queue indirect, 256 descriptors | 1 / 6 | 791 / 834 / 958 / 16375 |
| Queue indirect, 4096 descriptors, nonconforming | 1 / 10 | 10584 / 11042 / 11958 / 222458 |
| macOS wait, 2 read/write interests | 2 / 0 | 1084 / 1125 / 1375 / 1833 |
| macOS wait, 64 read/write interests | 2 / 0 | 18375 / 19041 / 34250 / 93292 |

  - A small chain requests 64 bytes. 256 descriptors cumulatively request 8128 bytes
    and reach 4096 bytes of capacity; 4096 descriptors reach 65536, before each chain
    is freed. Each macOS wait creates/closes a
    kqueue; 64 read/write interests request 8192 heap bytes per call.
  - Source-derived TxBuf growth 65,535→65,536 live bytes reallocates capacity from
    65,535 to 131,070. This verifies permitted capacity, not its normal traffic rate.
  - A synthetic driver of the actual vsock worker delivered 1024 one-byte spans;
    1025 delivered 0 and emitted RST at host IOV_MAX 1024. Both chains exceed this
    device's 256-entry Queue Size and are nonconforming: not a valid-packet failure.
- **Consequence.** Audit D05/D06/D09: bounded reusable metadata, persistent readiness
  with FD-generation correctness, capacity budgets, and early Queue Size bounds.
  Preserve validation, ordering, credits and half-close. No speedup is implemented;
  requested bytes exclude kernel/native allocations, socket buffers and physical RAM.

### M42. Stream compaction, launch allocations, and retained log history

- **Method.** [Harness](measurements/audit-stream-allocations/README.md) extracts current
  private frame/log routines by build script and calls actual ABI/IPC APIs. Construction
  cases reproduce identified operations. n=500 with separate allocation census;
  host/revision/compiler are M40's. Native System allocator, not guest musl.
  [Raw results](measurements/audit-stream-allocations/2026-09-29-macos-arm64.json)
  capture input hashes, all samples, n/p50/p90/p99/max and limitations.
- **Parser results.** 65,535 bytes contain 5461 valid 12-byte SIGNAL frames and a 3-byte
  partial header. Both parsers deliver every frame and preserve that tail. Actual
  each_frame requires 178,918,743 suffix bytes of compaction; a local cursor prototype
  shifts 3. These byte totals are calculated from drain lengths, not hardware counters;
  parser latency is measured.

| Parser | p50 / p90 / p99 / max, µs |
|---|---|
| Actual prefix-draining parser | 1670.042 / 1708.000 / 1799.875 / 1903.292 |
| Local cursor prototype | 3.417 / 3.500 / 3.709 / 6.167 |

- **Allocation results.** The six-entry poll construction makes 1 allocation + 1 realloc,
  requesting 40 then 80 bytes. A 64 KiB log record makes 1 allocation of 65,549 bytes;
  plain stdin makes 1 of 65,536. A 4166-byte Spec with 3 argv/64 env makes 1 allocation
  plus 10 reallocations when encoding, and 72 plus 4 when decoding. Exec pointer lists
  make 2+2; reproduced CString
  construction makes 69+71. Sizes/counts are per case, not an end-to-end census.
- **Retention.** Reading 1 MiB of log output in 1024 lines makes 1027 allocations + 8
  reallocations, requesting 3,227,904 bytes; retained Vec capacities total 2,138,320
  (1,048,784 pending + 1,048,576 payload + 40,960 line metadata). Actual logs-f ownership
  holds initial lines and pending capacity across follow. Read timing is
  356.375 / 374.291 / 400.750 / 410.250 µs.
- **Consequence.** Audit D07/D08/D10 and A12: one compaction per batch, bounded reused
  storage, release historical ownership, avoid duplicate serialization and terminator
  growth. The cursor is an algorithm experiment, not a completed guest/end-to-end
  optimization. Capacity retention is not RSS or immediate allocator reclamation.

### M43. Working-set expansion, CPIO capacity, and memory round-trip invariants

- **Method.** [Harness](measurements/audit-working-set/README.md): public working-set
  reader/writer and actual initramfs::with_init; n=30 allocation samples, all identical,
  and 30 separate timing samples per case. Decoder files were cache-warm; CPIO
  construction was memory-only. Host/revision/compiler are M40's.
  [Summary](measurements/audit-working-set/summary.json) and
  [raw samples](measurements/audit-working-set/samples.json) record source hashes and
  nearest-rank distributions. No VM is required. Returned contents/layout are checked
  outside measured intervals.
- **Results** (timing µs, p50 / p90 / p99 / max; heap is requested capacity).

| Operation | Alloc / realloc | Peak heap | Retained capacity | Time |
|---|---|---:|---:|---|
| Decode 718 touches | 4 / 0 | 23011 | 11488 | 10.584 / 10.875 / 14.875 / 14.875 |
| Decode 3867 touches | 4 / 0 | 123779 | 61872 | 17.459 / 17.708 / 17.792 / 17.792 |
| Decode 65536 touches | 4 / 0 | 2097187 | 1048576 | 157.959 / 203.333 / 397.833 / 397.833 |
| CPIO with 256-byte init | 79 / 7 | 1032 | 1024 | 3.334 / 3.417 / 3.500 / 3.500 |
| CPIO with 1 MiB init | 79 / 9 | 2098336 | 2098328 | 35.125 / 35.917 / 68.417 / 68.417 |

  - Touch is 16 bytes. Decoder peak is 32N+35, returned capacity 16N. ARM VcpuState is
    896 inline bytes, excluding owned heap vectors. One-shot restored state remains
    owned for the VM lifetime by Shared.start and vCPU closures at this checkpoint.
  - CPIO's 1 MiB archive is 1,049,288 bytes; its final trailer triggers capacity doubling.
    There are 78 field-formatting string allocations plus one output vector allocation.
- **Correctness probes.** [Source](measurements/audit-working-set/src/bin/range-order.rs)
  and [observations](measurements/audit-working-set/range-order.json) demonstrate:
  unsorted high/low ranges save low=L/high=H but restore low=H/high=L; a zero guest
  page saved into a preseeded 0xaa target restores 0xaa. Current concrete machine
  ranges are sorted and snapshot generations create fresh files, avoiding these
  triggers. They remain public-API defects A21/A22, not measured normal-CLI corruption.
- **Consequence.** Audit D03/D13: bounded/fallible decode planning, release one-shot
  metadata after all startup users, reserve complete CPIO capacity, and enforce memory
  serialization invariants before optimizing. These results exclude native/mmap memory,
  guest faults, complete restore latency and Firecracker comparisons.

**Audit measurement checkpoint:** M40–M43 describe 38457b7 and their recorded source
hashes. Subsequent in-progress changes to guest-memory access and device integration
are preserved and are not certified by these samples. Reproduce against the recorded
revision before comparing a changed API/implementation.

### M44. The guest-memory access guard: what checks it, and what it costs

- **Question.** D29 has the VMM's threads reach guest memory through one `Access` at a
  time, with volatile and atomic accesses within it (audit A01). Do the tools that know
  Rust's rules for concurrent accesses find a race? What does the guard cost runs, and
  what does its word-at-a-time zero check do to saves?
- **Method.**
  - `docs/research/measurements/access-guard/check.sh` runs ThreadSanitizer
    (nightly-2026-09-05, `-Zsanitizer=thread`, std rebuilt with it) over the memory and
    virtqueue unit tests, and Miri over those that map no file. In them, eight threads
    write and read back the same unaligned bytes, and two threads work two queues whose
    rings lie on each other's descriptors and rings. `negative-control.patch` gives every
    access a lock of its own, which excludes nothing.
  - Runs: `docs/research/measurements/build-ab/ab.py`, 38457b7 against this change, both
    restoring one template: `shards run --pull never alpine true`, n = 1000 twice, and
    `head -c 1048576 /dev/zero`, n = 300 then 1000.
  - Saves: `access-guard/save-ab/run.py 38457b7` times `GuestMemory::save` of 256 MiB
    with a nonzero byte in every 16th 16 KiB page, the others untouched or touched and
    zero, in a fresh process per sample, alternating the builds, n = 30.
  - Apple M5 Max (Mac17,6), macOS 26.4.1. Other work kept load averages at 7–15.
- **Results.**
  - With the guard, ThreadSanitizer reported nothing over 16 tests, and Miri found no
    undefined behavior in 9. With the negative control, ThreadSanitizer reported 5 data
    races in the two-queue test, among them a 2-byte atomic store against the other
    thread's accesses of the same bytes, and Miri stopped at "Data race detected between
    (1) non-atomic read ... and (2) atomic store": a descriptor read against an index
    store.
  - Runs, the paired median of new − old with its 95% interval:

| Command | n | Wall | In the guest |
|---|---|---|---|
| `true` | 1000 | −5 µs [−39, +38] | +3 µs [−6, +15] |
| `true` | 1000 | −54 µs [−132, +25] | −1 µs [−7, +6] |
| 1 MiB of output | 300 | +15 µs [−70, +115] | +10 µs [−63, +78] |
| 1 MiB of output | 1000 | +0 µs [−94, +50] | +10 µs [−20, +56] |

  - The tails followed the host's load, in both arms alike: wall-time p99 for `true` was
    15.1 ms and 11.4 ms (38457b7, this change) in the first run, and 50.3 ms and 37.3 ms
    in the second, at higher load.
  - Saves (ms, p50 / p90 / p99 / max):

| RAM besides every 16th page | 38457b7 | This change | Paired new − old |
|---|---|---|---|
| untouched | 79.5 / 80.4 / 82.4 / 82.4 | 26.1 / 28.5 / 38.1 / 38.1 | −53.4 [−54.0, −52.7] |
| touched, zero | 69.3 / 100.4 / 119.9 / 119.9 | 14.5 / 15.9 / 30.5 / 30.5 | −54.7 [−57.8, −52.8] |

  - The old check read each page a byte at a time until it found a nonzero one
    (`iter().any`); the new one reads words. Both still read, and so materialize, every
    untouched page (audit D01).
- **Consequence.** D29 stands: no host thread races another, at no measured cost to
  runs, and saves take a third to a fifth of the time. CI runs `check.sh`.

### M45. A snapshot of a machine busy in every way at once

- **Question.** Does a snapshot taken while every vCPU, timer, disk and vsock stream is
  busy come back whole (audit A02)?
- **Method.** The test guest's `storm` mode, driven by `crates/shards/tests/snapshot.rs`
  (`storm`):
  - Pairs of threads pinned to neighbouring CPUs wake each other in turn through
    condition variables, a thread sleeps in 200 µs ticks, one reads the read-only disk
    (`O_DIRECT`, 64 KiB, checked), and one writes 4 KiB records to the writable disk,
    reading each slot first to find it empty. The host streams 4 MiB rounds, checked,
    through a vsock echo.
  - The guest asks for the snapshot once the storm is up and the echo has carried a MiB.
    Every restore (the writable disk put back as the snapshot left it) and the resumed
    original must see every worker progress within 1 s. A restore must also lose no
    wakeup: no wait may time out to find its turn had come.
  - Each worker's longest gap between rounds and the lost wakeups are reported.
    `SHARDS_STORM_MS` runs the storm that long before the snapshot, reporting that window
    on its own.
  - 2, 8 and 64 vCPUs on an Apple M5 Max (Mac17,6, 18 cores), macOS 26.4.1.
- **Results.**
  - **Interrupts on their way.** With HVF's GIC saved and restored as distributor and
    redistributor registers, every restore of a snapshot taken with a disk request in
    flight stalled. The request's completion never reached the guest: its `used_event`
    still waited on an entry the device had added and signalled. An SPI edge raised in
    isolation, with no vCPU running, did survive the registers (`GICD_ISPENDR1` read back
    `0x2`), which is why the register tests had passed. With `hv_gic_state`, no restore
    stalled in 180 (20 runs of both tests).
  - **A lost kick.** In about one run in fifteen, the barrier waited forever. `sample`
    showed the coordinator waiting, one vCPU parked, and the other inside `hv_vcpu_run` in
    HVF's `VcpuStateManager::wait_for_interrupt`, its kick lost; the guest reported RCU
    stalls on the parked CPUs. Once every entry checked for a pending kick, no run hung,
    in the 20 runs of the final tests and some 70 of earlier versions.
  - **Steady state** (11 windows of 1–3 s per vCPU count). The median of the workers'
    longest gaps was 0.59–0.82 ms at 2 vCPUs, 0.23–0.37 ms at 8 and 3.7–4.6 ms at 64
    (64 vCPUs sharing 18 cores). The longest of all, 69 ms, was the disk reader's. No
    wakeup was lost in the 12 windows that counted them.
  - **Restores:** no lost wakeup in 45 at 8 vCPUs.
  - **Resumed originals** counted lost wakeups in 10 of 30 runs at 8 vCPUs (11 in all),
    and in 5 of 30 (6) with no GIC state taken, the same within noise (Fisher's exact
    p ≈ 0.23). An original's clock runs on through its pause, so a wait that times out
    during the pause counts. The pause was 60–75 ms at 2 and 8 vCPUs and 81–131 ms at 64
    when the snapshot came at once. After a 1 s storm it was 0.24–0.44 s at 2 vCPUs,
    0.35–0.61 s at 8 and 0.59–1.49 s at 64.
- **Consequence.** D14's phased barrier, `hv_gic_state`, and the kick check before every
  entry. The tests require restores to lose no wakeup, and let the original only report
  them.
- **Correction (2026-09-30).** The lost-wakeup counts above measured something else.
  - A wait counted when it timed out and then found its turn had come. But a futex wake
    takes its waiter off the futex in guest memory whatever becomes of the interrupt that
    should run it (Linux kernel/futex/waitwake.c, futex_wake), so a waiter whose wakeup
    interrupt is lost returns *woken*, late, when its own timer runs its CPU: the count
    could not see a lost interrupt, and the 100 ms timeout hid one.
  - What it counted was a timeout that fired as the other thread, late, was taking the
    turn. CI's x86_64 runners, 64 vCPUs on 4 cores, counted one after a restore, whose
    vCPUs took up to 0.48 s to run again.
  - The pairs now wait with no timeout, so a lost wakeup leaves its waiter asleep and the
    1 s progress check fails; each worker reports how long it took to take its turn.
    Turns after a restore on the M5 Max took at most 3.1 ms at 8 vCPUs and 165 ms at 64,
    their threads starting again on 18 cores; in steady state at most 2.7 ms at 2 vCPUs
    and 15 ms at 64.
- **Open.**
  - A busy guest's 256 MiB took up to 1.5 s to save at 64 vCPUs: the scan of every page
    (audit D01) and the durable flush.
  - A resumed original sees its snapshot's pause as time gone by, where a restore's clock
    continues. Whether to hide it, and how the guest's wall clock would then be kept
    right, is open.

### M46. What publishing a container's record costs, by how durably

- **Question.** Audit A15: what does each level of durability cost a container's record
  (`containers/ID/config.json`), and which can the run's start path afford?
- **Method.** `docs/research/measurements/record-sync/run.sh`: a 420-byte record written
  to a temporary sibling and renamed over the last, n = 1000 per level. Each level runs
  alone, in blocks of 500, in two rounds of opposite order: levels that took turns publish
  by publish measured each other, since a drive flush delays the next writes (plain renames
  then had a p90 of 8.8 ms). 2026-09-29, this machine (Darwin 25.4.0, arm64, APFS),
  revision b271e75, with another VM busy on 2 CPUs; two runs, in the home and in `$TMPDIR`.
  An earlier run, with two orphaned test processes spinning, had tails 10–50× longer.
- **Results** (µs; the two runs):

| Level | p50 | p90 | p99 | max |
|---|---|---|---|---|
| write, rename | 90–97 | 129–169 | 161–3 972 | 308–5 817 |
| `fsync`, rename | 104–122 | 149–150 | 191–776 | 4 026–6 365 |
| `F_BARRIERFSYNC`, rename | 376–382 | 598–671 | 2 179–12 108 | 10 125–214 562 |
| `F_FULLFSYNC`, rename | 4 267–4 424 | 7 550–12 860 | 16 974–26 458 | 170 126–282 421 |
| `F_FULLFSYNC`, rename, directory synced | 8 546–8 555 | 15 916–16 008 | 25 407–31 064 | 29 824–266 921 |
| a new directory and record, all synced | 8 554–8 682 | 15 976–20 317 | 25 487–37 210 | 36 152–226 825 |
| a directory renamed aside, parent synced, removed | 4 266–4 284 | 7 303–8 199 | 8 768–14 954 | 15 804–26 912 |

- **A record on the start path** (audit A15). `docs/research/measurements/build-ab/ab.py`,
  `shards run --pull never alpine true`, each arm its own template, 2026-09-29 after a
  reboot, load 7–14 (another VM on 3 CPUs, Spotlight reindexing):
  - Written and renamed before the container was seen, on the run's path: +2.6 ms at the
    median (n = 500, 95% [+2.1, +2.9]). Timed inside the daemon, the write took 0.18 ms
    alone but 7.5 ms at the median with the other arm's runs going: `open` and `rename`
    each took 4.4 ms at the median (n = 612). With shards idle, the same create and
    rename took 0.16 ms at the median and 4.4 ms at p90: the host's other processes'
    flushes, in which any metadata write waits.
  - Written beside the run, which waited for it only before its VM was committed: +0.33
    ms at the median (n = 1000, 95% [+0.29, +0.39]), p90 6.5 → 11.9 ms, p99 9.9 → 24.1 ms.
  - Written beside the run, which never waits for it: −21 µs (n = 1000, 95% [−44, +9]);
    p90 8.8 → 8.0 ms, p99 32.2 → 29.8 ms.
- **Consequence.** A record durable at once costs 8.5 ms at the median, 2.5 times a whole
  pooled run (M26, 3.4 ms); a barrier alone costs 0.3 ms and a tail of milliseconds. So
  container records are written and renamed, not synced, and never on a run's path: a
  metadata write there takes the filesystem's tail, not its median. A removal, off the
  path, is synced. Linux is not measured yet. D27, "Durability".

### M47. What an image's build holds, and what real images hold

- **Question.** Audit A10: what does building a root filesystem cost per entry, and how
  large are real images, to set its default limits against?
- **Method.**
  - `docs/research/measurements/image-budgets/run.sh`: the peak RSS of applying one
    layer of N empty files, 1000 to a directory, and writing its EROFS image.
  - `count.py` over a store holding `node:22`, `python:3.12` and `rust:1` (linux/arm64),
    pulled on 2026-09-30: entries, uncompressed bytes, and bytes of names, links and
    xattrs. `tensorflow/tensorflow:latest` has no arm64 image.
  - This machine, revision dc28003.
- **Results.**

| Entries | Applied | Written | Per entry |
|---|---|---|---|
| 100 000 | +40 MiB | +56 MiB | 589 B |
| 400 000 | +158 MiB | +252 MiB | 660 B |
| 1 000 000 | +443 MiB | +582 MiB | 610 B |

| Image | Layers | Entries | Uncompressed | Names, links, xattrs |
|---|---|---|---|---|
| node:22 | 8 | 34 129 | 1.07 GiB | 1.5 MiB |
| python:3.12 | 7 | 33 774 | 1.06 GiB | 1.4 MiB |
| rust:1 | 5 | 31 460 | 1.47 GiB | 1.3 MiB |

- **Consequence.** D18's defaults: 4 Mi entries (120 times these images, and 2.5 GiB of
  memory at 610 B each), 64 GiB decompressed (40 times; AWS Lambda takes container
  images up to 10 GB), and 1 GiB of names, links and xattrs.

### M48. `logs --tail 1` of a large log

- **Question.** Audit A12: what does `logs --tail 1` of a large log cost the daemon,
  before and after the log is read through its index?
- **Method.** `docs/research/measurements/log-tail/tail.py`: in each build's home a
  container's log is replaced by 2 GiB of 80-byte lines in 64 KiB records, as an earlier
  shards wrote it (no index), and `logs --tail 1` is asked for three times, the
  daemon's RSS sampled every 5 ms. The old build is 4a5ec9d; the new one this change.
  2026-09-30, this machine.
- **Results.**

| Build | Attempt | Time | Daemon RSS before | At peak |
|---|---|---|---|---|
| 4a5ec9d | 1 | 2 025 ms | 13 MiB | 7 176 MiB |
| | 2 | 1 767 ms | 5 151 MiB | 8 224 MiB |
| | 3 | 1 551 ms | 5 152 MiB | 8 225 MiB |
| this change | 1 (indexes the log) | 38 ms | 8 MiB | 8 MiB |
| | 2 | 3 ms | 8 MiB | 8 MiB |
| | 3 | 3 ms | 8 MiB | 8 MiB |

- **Consequence.** D27: logs are read through their index, in bounded buffers, and
  `--tail` back from the end. The old daemon held 3.5–4 times the log at its peak, and
  kept 5 GiB after.


### M49. A daemon's fleet: 1 to 100 templates, bursts, and stop

- **Question.** Audit A13: what do warm VMs cost a daemon serving many templates, with
  pools that keep what their runs need (D26) and at most `SHARDS_WARM_MAX` (16) warm VMs
  all together; what does a run wait when its template has none; how do bursts past a
  pool fare; and what is left after `daemon stop`?
- **Method.** `docs/research/measurements/fleet/fleet.rs`, an ignored test
  (`FLEET_SIZES=1,10,100 cargo test --release -p shards --test fleet -- --ignored
  --nocapture`). For each size N, a home of its own with default settings and N
  distinct test images, each from a loopback registry: every image run once (pulled,
  booted, its template saved), then again (restored); with the fleet settled for 2 s,
  the warm VMs' and the daemon's own memory (`footprint`'s physical footprint: private
  dirty and compressed pages, page tables included) and each warm VM's CPU time
  (`proc_pid_rusage`, which is its restore, since it has done nothing else); one
  image's runs one at a time, 200 ms apart, then 20 bursts of 8 at once; `daemon
  stop`'s wall clock. Client wall clock per run, spawn to reap. ceb1f0d, 2026-09-30,
  this machine (Darwin 25.4.0 arm64), load average 6–9.
- **Results.** All 1 × 2 + 10 × 2 + 100 × 2 image runs and 3 × 180 later runs succeeded.

| N | Pass | n | p50 | p90 | p99 | max (ms) |
|---|---|---|---|---|---|---|
| 1 | first (pull, boot, save) | 1 | 181.9 | 181.9 | 181.9 | 181.9 |
| 1 | second (restore) | 1 | 10.3 | 10.3 | 10.3 | 10.3 |
| 10 | first | 10 | 176.6 | 178.9 | 185.2 | 185.2 |
| 10 | second | 10 | 5.2 | 5.8 | 8.7 | 8.7 |
| 100 | first | 100 | 221.7 | 238.2 | 307.3 | 336.3 |
| 100 | second | 100 | 16.2 | 23.2 | 32.0 | 32.1 |

| N | Warm VMs | Their memory | Daemon | Warm VM restore CPU p50 / max | Templates on disk |
|---|---|---|---|---|---|
| 1 | 1 | 3.4 MiB | 4.7 MiB | 7.7 / 7.7 ms | 35.0 MiB |
| 10 | 10 | 33.3 MiB | 5.6 MiB | 7.3 / 8.1 ms | 350.4 MiB |
| 100 | 16 | 54.5 MiB | 6.2 MiB | 9.2 / 9.8 ms | 3 500.6 MiB |

| N | One image's runs | n | p50 | p90 | p99 | max (ms) |
|---|---|---|---|---|---|---|
| 1 | one at a time | 20 | 6.7 | 8.4 | 9.8 | 9.8 |
| 1 | bursts of 8 | 160 | 17.0 | 20.8 | 32.2 | 36.3 |
| 10 | one at a time | 20 | 5.7 | 6.2 | 6.4 | 6.4 |
| 10 | bursts of 8 | 160 | 17.1 | 23.5 | 33.1 | 33.6 |
| 100 | one at a time | 20 | 5.8 | 6.2 | 13.6 | 13.6 |
| 100 | bursts of 8 | 160 | 15.1 | 17.1 | 18.0 | 18.1 |

  `daemon stop` took 253–259 ms and left no VM each time.
- **Findings.**
  - Warm VMs stop at the bound: 100 templates keep 16, whose own memory is 54.5 MiB,
    3.4 MiB each. A warm VM's RSS, about 20 MB, counts in full the snapshot pages it
    shares with its template's file; its own cost is a sixth of that.
  - A run whose template keeps no warm VM restores one on demand: at 100 templates the
    second pass's p50 is 16.2 ms against 5.2 ms at 10, where every template kept one.
  - A burst of 8 finds at most `SHARDS_POOL` (2) ready and restores the rest on demand:
    p50 about 16 ms against 6 ms for runs one at a time.
  - A restore costs its VM 7–10 ms of CPU.
  - Each template takes 35 MiB of disk.
- **Not measured.** 1,000 templates: they would take 35 GiB, more than this machine's
  68 GiB free can spare; the warm VMs stay at 16 whatever N, and the daemon's own memory
  grew 1.5 MiB from 1 to 100. Low host memory: speculation is bounded at 16 VMs' 55 MiB
  of their own, and the kernel reclaims their pages as any process's.
- **Consequence.** D26's bound holds; the default's cost, which D26 put at 320 MB from
  RSS, is 55 MiB.

### M50. The vsock device's fresh kqueue per wait, in a run

- **Question.** Audit D06: on macOS each wait of the vsock device makes a kqueue,
  registers every interest, and closes it; 64 interests cost 18 µs at the median in a
  microbenchmark. What does it cost a run?
- **Method.** `docs/research/measurements/vsock-poll/count.patch` prints, for each wait,
  its interests and the time from its entry to the blocking `kevent`. 30 warm runs of
  `shards run --pull never --rm alpine true`, after three that saved and warmed the
  template. 8c6750e, 2026-09-30, this machine, load average about 7.
- **Results.** About 2 to 3 waits a run, with 3 to 4 interests each (the waker, the
  listener, the run's connections); the setup took 0.5 to 3.5 µs a wait, about 1 µs
  typically: some 3.6 µs a run, where a run takes about 5 ms (M29).
- **Consequence.** D06's persistent kqueue is not taken: it would save a few
  microseconds a run, 0.07%, against the stale registrations and descriptor reuse a
  fresh kqueue rules out by construction. M57 found that a stream, which waits
  constantly, pays 16.5% of its device thread for it; D06 was then taken.

### M51. Workloads, their output checked, and what streams cost on this host

- **Question.** The audit's matrix asks for workloads whose status and output are
  checked, not only timed. What do `true`, a line to stderr, 16 MiB out, 16 MiB in and
  back, and 64 MiB of hashing in the guest cost a run served from its template?
- **Method.** `cargo bench -p shards --bench workloads -- --runs 5`
  (crates/shards/benches/workloads.rs): each run's stdout, stderr and status checked
  byte for byte against the test guest's pattern (shards_testguest), the workloads in
  turn after two runs each. 46c855a, 2026-09-30, this machine, load average about 6,
  its data volume 99% full (42–68 GiB free of 7.3 TiB).
- **Results (µs, p50 / p90 / max).** `true` 4 781 / 21 589 / 21 589; stderr 5 663 /
  11 384 / 11 384; 16 MiB out 364 752 / 497 803 / 497 803 (44 MiB/s); 16 MiB in and back
  321 920 / 369 728 / 369 728; hashing 64 MiB 69 662 / 72 223 / 72 223. Every output was
  exact.
- **Where the streams' time goes.** A sample of the serving VM while it relayed
  `head -c 256M /dev/zero` (53 s, 4.8 MiB/s): its vCPU waited for interrupts 98% of the
  time, and its relay thread spent 97% in the two appends that keep each frame in the
  container's log (workload.rs, `Logger::keep`). Appends on this volume cost that much
  from any program: 4 KiB appends took 165–185 µs each from Python, 66 µs into a
  preallocated file, where a solid-state disk with room takes a few.
- **Consequence.** The log is written as Docker's default `blocking` log mode writes
  its: output waits for it. What streams cost therefore follows the disk the home is on;
  these stream figures are this host's full volume, not shards'. The benchmark keeps
  checking every byte.

### M52. Confining shards-vm: what Linux's filter and macOS's Seatbelt allow a VM

- **Question.** Audit, "Security and test coverage": shards-vm runs guest-facing device
  code with the user's permissions. On Linux, which syscalls must a seccomp filter
  allow it; on macOS, can Hypervisor.framework run under a sandbox at all, and which?
- **Linux, method.** `docs/research/measurements/vmm-syscalls/collect.sh` on CI's KVM
  runner: every VM test binary under `strace -f`, each shards-vm thread's syscalls,
  ioctl requests, socket domains, fcntl commands and prctl options (parse.py), each
  run's trace read with state of its own. b6ef38a onward.
- **Linux, results.** 15 thread classes; the union is 70 syscalls, 43 KVM ioctl
  requests (all the backend defines, `hv::IOCTLS`), `FIONBIO` and the terminal's, only
  `AF_UNIX` sockets, `PR_SET_NAME`, and threads made through `clone3`. The filter built
  from them (confine.rs), `clone3` failed with ENOSYS so that threads come from `clone`
  with a thread's flags, ran every VM test of the glibc build on the KVM runner with VMs
  required (b6ef38a). musl's `open()` makes open(2), not openat(2), and its `isatty()`
  asks `TIOCGWINSZ` (df23802).
- **macOS, method.** An ad-hoc-signed shards-vm booting the pinned kernel and
  shards-init: with App Sandbox's entitlement; under `sandbox-exec` profiles; and with
  a profile it applies to itself at the start of `main` (`sandbox_init`). 2026-09-30,
  this machine.
- **macOS, results.**
  - With `com.apple.security.app-sandbox`, the process ended by SIGTRAP at launch, before
    `main`: a command-line tool is not sandboxed that way on its own.
  - Under `sandbox-exec`, `(allow default)` with network and file writes denied booted
    the guest; `(deny default)` with reads of the kernel, init and the system's
    libraries allowed ended in SIGABRT before `main`, nothing logged.
  - Applied by the process itself after it has loaded, `(deny default)` with only
    `sysctl-read` and reads of the kernel and init booted the guest: Hypervisor.framework
    needs no Mach service, I/O Kit connection or file beyond the entitlement.
- **Consequence.** Linux: shards-vm installs the filter over its whole process first
  thing (D30). macOS: a deny-by-default profile the process applies to itself can confine
  it; which paths each of its modes reads and writes (disks, snapshot directories, vsock
  sockets, a warm VM's log) is its design, next.

### M53. What confining a VM process with Seatbelt costs

- **Question.** D30 has each VM process on macOS apply a Seatbelt profile to itself
  before its VM starts. What does it cost, and where?
- **Method.** A C program timing libsandbox's `sandbox_compile_string` and
  `sandbox_apply` apart, and `sandbox_init`, each in a fresh process, for the profile
  shards builds, a minimal `(deny default)` and `(allow default)`, and five compiles in
  one process; `confine::tests::a_profile_confines_the_process_to_its_paths` timing
  `sandbox_init` in 8 processes; `cargo bench --bench restore -- --runs 20` and `--bench
  image -- --runs 20 --templates 2`, with shards' profile. 2026-09-30, this machine, load
  about 6.
- **Results.**
  - Compiling took 3.5–4.1 ms whatever the profile held, even `(allow default)`, and
    2.9 ms again in the same process: the cost is the interpreter's. Applying the
    compiled profile took 22–69 µs. `sandbox_init`, both: 3 984–4 300 µs over 8
    processes.
  - With the profile, and the logging daemon's lookup denied: a restore in a new
    process 5 967 µs at p50 (2 464 µs without, earlier the same day), spawn to exit 19 648
    µs (6 286 µs). Allowing that one lookup: spawn to exit 9 935 µs, restore 5 941 µs.
  - Unchanged: a warm VM's request, 164 µs at p50 (160 µs), and a run served from a
    template, 3 282 µs.
- **Consequence.** D30 allows the logging daemon's lookup, and applies the profile
  before a VM starts, which a VM made ready ahead does before its run. A restore in a new
  process and a cold boot pay the compile, about 3.7 ms. Compiling once in the daemon
  and applying in the VM would cost the VM tens of microseconds; each VM's profile names
  its own socket directory, so each still needs a compile of its own.

### M54. Saving RAM the guest never touched, without touching it

- **Question.** Audit D01: a save reads every page to find the zero ones, and reading an
  untouched anonymous page makes the host give it one. Can a save skip what the guest
  never touched, and what does asking cost?
- **Method.** `GuestMemory::save` asks the OS which pages of anonymous RAM were never
  touched: neither resident nor paged out (macOS `mach_vm_page_range_query`; Linux
  /proc/self/pagemap's present and swapped bits); those read as zeros, and are skipped
  unread. On macOS it first asks each map entry's counts (`mach_vm_region`), and asks page
  by page only where some page is neither. `docs/research/measurements/access-guard/
  save-ab/run.py HEAD`, 256 MiB with a nonzero byte in every 16th 16 KiB page, the rest
  untouched or (`--touch-all`) touched and zero; n = 30 fresh processes per arm,
  alternating; the harness now reports each process's peak RSS. 2026-09-30, this machine.
- **Results (µs, p50 / p90 / p99; peak RSS p50).**

| RAM besides every 16th page | Before | After | Paired after − before |
|---|---|---|---|
| untouched | 25 904 / 26 744 / 29 321; 268 032 KiB | 9 801 / 10 169 / 10 792; 22 352 KiB | −16 066 [−16 277, −15 923] |
| touched, zero | 14 083 / 14 283 / 15 228; 268 016 KiB | 15 148 / 15 632 / 16 544; 268 000 KiB | +1 032 [+834, +1 175] |

  - Asking page by page for all 16 384 pages cost 4.7 ms where every page was touched;
    XNU split the 256 MiB mapping into two entries of 128 MiB, whose counts settle it in
    1.0 ms.
  - Linux's path is checked by the memory tests on CI's runners; its time is not
    measured here.
- **Consequence.** D01's untouched pages are skipped: a template saved from a guest that
  touched little of its RAM saves in a third of the time, and costs the host its RAM's
  used pages only, 22 MB where it was 268 MB. RAM the guest used throughout saves 7%
  slower.

### M55. The advice a restored guest's memory file gets, on Linux

- **Question.** Audit D04: a restore maps the snapshot's memory file privately over the
  reserved RAM, which loses the reservation's advice. Beside Firecracker, restoring the
  same guest, shards has twice the file pages resident (18.6 against 11.6 MiB of
  `Pss_File`, M53's run). Which madvise(2) should the mapping get?
- **Method.** `docs/research/measurements/restore-advice/`: `advice.patch` lets
  `SHARDS_FILE_ADVICE` choose the mapping's advice (none, `random`, `sequential`,
  `nohuge`, `huge`), and `.github/workflows/restore-advice.yml` runs, on CI's KVM runner
  (ubuntu-24.04 x86_64, THP `always`):
  - the Firecracker comparison for each (n = 20 each; 7efba41, run 36697958785);
  - the restore bench (n = 30) and the image bench (n = 60, 3 templates), for none and
    `nohuge`, two rounds alternating (3f547b2, run 36699671580).
- **Results (p50 / p90 / p99).**

| Advice | Comparison `to_beat` | `Pss_File` | cold_restore | warm_request | run_template | its RSS |
|---|---|---|---|---|---|---|
| none | 8.9 / 9.1 / 9.3 ms | 18.6 MiB | 9.5 / 10.0 / 12.5 ms; 12.6 / 17.4 / 39.9 ms | 8.4 / 9.3 / 9.5 ms; 12.2 / 16.1 / 31.1 ms | 56.1 / 60.2 ms; 57.3 / 59.5 ms | 27.6; 27.4 MiB |
| random | 9.7 / 10.0 / 10.4 ms | 18.5 MiB | | | | |
| sequential | 9.1 / 10.1 / 13.7 ms | 18.6 MiB | | | | |
| huge | 9.1 / 9.4 / 9.9 ms | 18.5 MiB | | | | |
| nohuge | 12.5 / 13.2 / 13.5 ms | 8.4 MiB | 20.1 / 20.8 / 36.0 ms; 16.4 / 40.0 / 44.0 ms | 19.1 / 19.8 / 35.0 ms; 15.3 / 34.9 / 42.9 ms | 56.4 / 60.1 ms; 57.7 / 61.1 ms | 19.2; 19.3 MiB |

  (Two rounds' figures are separated by a semicolon.)
  - Readahead advice changes nothing: the file pages come in as the page cache's large
    folios, whole, whichever is given.
  - `nohuge` maps them page by page. It halves the resident file pages, but faults each
    page separately, and restores and warm requests take 1.3 to 2.3 times as long.
  - A pooled run, whose VM restored before the request, takes as long either way.
- **Consequence.** The mapping keeps no advice. The pages `nohuge` saves are page cache,
  shared with every VM that restores the same template; `Pss_File` divides them among
  those VMs, and a warm request's latency is what shards is for. The Firecracker
  envelope's `rss_file` allowance stands for this reason. Commit accounting, D04's other
  half, is measured apart.

### M57. A sustained vsock stream: where the device thread's time goes

- **Question.** Audits D05 and D06. Do the vsock device's per-packet allocations
  (chains, spans, iovecs) or its fresh kqueue per wait cost a stream anything? M50
  measured the kqueue in a run, where it waits 2 or 3 times; a stream waits constantly.
- **Method.** `docs/research/measurements/vsock-stream/stream.py`: the test guest's
  `vsock` echo, a host writer sending 256 KiB buffers and a reader taking the echo back
  for 10 s. Builds alternate for 4 rounds, and sample(1) profiles one 12 s run of each.
  2026-09-30, this machine; the builds are 137afef, and the same with the vsock worker's
  kqueue kept (below).
- **Results.**
  - Before, the device thread had 4,720 of 7,847 samples busy, the rest waiting in
    `kevent`:
    - making and closing each wait's kqueue took 779 (`kqueue` 298, `close` 481), 16.5%
      of its busy time;
    - malloc, free and vector growth took about 84, 1.8%;
    - readv, writev, send and the rest took the remainder.
  - Echoed MiB/s per round, before and after keeping the kqueue: 297 / 313, 296 / 314,
    296 / 313, 297 / 308, with VM CPU time 13.6–13.7 s in every run. After, `kqueue` and
    `close` do not appear, and the thread waits more.
- **Consequence.**
  - D06 is taken. Each vsock worker keeps one kqueue (`poll::Poller`). Every wait adds
    each interest again, which updates a registration or makes a missing one, and deletes
    what is no longer wanted. Closing a descriptor removes its registrations (kqueue(2)),
    so a number reused since the last wait is registered anew. A stream moves 5% more for
    the same CPU. Linux's poll(2) has no object to keep; its `pollfd` array is now kept.
  - D05 is not taken: the allocations cost 1.8% of the busy thread, below what reworking
    every device's queue loop would buy.

### M56. What restored VMs are charged against Linux's commit limit

- **Question.** Audit D04: a restore maps its template's memory file private and
  writable, and Linux accounts such a mapping at its whole size (overcommit-accounting.rst),
  while a boot's anonymous RAM is mapped MAP_NORESERVE. What does a host of restored VMs
  get charged, under each overcommit mode, and what would MAP_NORESERVE on the file
  mapping change?
- **Method.** `docs/research/measurements/commit/` (`commit.rs`, run as the ignored test
  `crates/shards/tests/commit.rs`): one template of the `resume` test guest with 256 MiB
  of RAM, then up to 64 restores held before their start requests. After each,
  `Committed_AS`; of each, its template mappings' size, resident and private pages, and
  `VmFlags`. `.github/workflows/commit-accounting.yml` runs it under
  `vm.overcommit_memory` 0, 1 and 2 (ratio 50), as mapped at 137afef and with
  `noreserve.patch`. CI's ubuntu-24.04 x86_64 runner: 15,988 MiB of RAM, 3,071 MiB of swap,
  CommitLimit 11,066 MiB. Runs 36702507669 and its rerun, 2026-09-30.
- **Results.**

| Mapping | Mode | Held | Commit per VM (MiB, p50 / p99 / max, n) | `VmFlags` | The refusal |
|---|---|---|---|---|---|
| as mapped | 0 | 64 of 64 | 258.6 / 258.7 / 258.7, 64 | `ac` | none; Committed_AS 18,523 MiB, over the limit |
| as mapped | 1 | 64 of 64 | 258.6 / 258.8 / 258.8, 64 | `ac` | none |
| as mapped | 2 | 34 | 258.7 / 258.8 / 258.8, 34 | `ac` | the 35th: `snapshot memory: ENOMEM` |
| MAP_NORESERVE | 0 | 64 of 64 | 2.6 / 2.7 / 2.7, 64 | `nr` | none; Committed_AS +171 MiB |
| MAP_NORESERVE | 1 | 64 of 64 | 2.6 / 2.8 / 2.8, 64 | `nr` | none |
| MAP_NORESERVE | 2 | 35 | 258.7 / 258.8 / 258.8, 35 | `ac` | the 36th: its fork, ENOMEM |

  Every held VM had 20 KiB of its template's memory resident.
- **Consequence.** A restore's file mapping is MAP_NORESERVE, as a boot's RAM is:
  - Under modes 0 and 1, a restored VM is charged its process's own 2.6 MiB where it was
    charged its whole RAM. Committed_AS no longer runs past the limit on a host that has
    nearly all of its memory free.
  - Mode 2 ignores the flag, so a host that asked for strict accounting still has each
    VM's RAM charged, and refused at the limit.
  - As for booted VMs, a guest that writes more RAM than the host has meets the OOM
    killer instead of a refused start: admission by memory is D14's budgets.

### M58. What a restore holds only to set its vCPUs up

- **Question.** Audit D03: a restored machine kept its whole start, vCPU states,
  interrupt-controller and device state, and working set, for as long as it ran; every
  warm VM in a pool held it. How much is it, and what does dropping it give back?
- **Method.** The start is dropped once every vCPU is set up and the machine finished,
  and a restore at `info` reports its parts' capacities, checking that nothing holds it
  still. A template of the `resume` test guest (256 MiB, 1 vCPU, no working set) was
  restored 3 times. Then 5 held restores from each of the builds before and after this
  change, alternating: their `footprint -p` phys_footprint. 2026-09-30, this machine.
- **Results.**
  - Each restore dropped 864 bytes of vCPU state (its inline size) and 126,453 bytes of
    interrupt-controller and device state. A working set adds 16 bytes an entry: about
    62 KB for M33's sets of 3,500 to 3,900.
  - Held footprint (KB): before 5,232 / 5,281 / 5,569 / 5,841 / 5,969; after 5,296 /
    5,328 / 5,520 / 5,616 / 5,937. The medians differ by 49 KB, within the spread.
- **Consequence.** The start's state is freed before the guest runs, so the VM's own
  later allocations reuse it; malloc keeps its pages rather than returning them, so a
  held VM's footprint does not visibly shrink. The working-set decoder's second vector
  is reserved fallibly, as its first was. The transient buffers the prefetch builds
  (HVF's scratch code and tables, KVM's page runs) are freed before the guest runs and
  are bounded by the working set, so they are left as they are.

### M59. The guest relay's frames, parsed from a cursor

- **Question.** Audit D07: the relay removed each frame's bytes from the front of its
  buffer as it parsed them, moving the rest every time. What does a batch of control
  frames cost it, and a cursor that removes a batch's frames at once?
- **Method.** `frames::tests::frames_cost` in crates/init (`cargo test --release -p
  shards-init frames_cost -- --ignored --nocapture`): the audit's 65,535-byte batch,
  5,461 signal frames and a partial header, parsed by the old parser and by
  `each_frame`, alternating, n = 500 each. On this machine, the host's CPU, 2026-09-30.
- **Results (µs).** Prefix removed per frame: 1,667.3 / 1,739.6 / 1,941.6 / 2,025.6
  (p50 / p90 / p99 / max). Cursor: 3.8 / 4.0 / 8.1 / 13.6.
- **Consequence.** The relay parses from a cursor, and the bytes it writes to the host
  and to stdin are written from an offset. They move forward only once what is written
  is half of what is held, so a 256 KiB backlog written in 4 KiB pieces is moved at most
  once over, not 7.9 MiB. Its poll set is a six-slot array, and each output frame's
  header and payload are reserved together.

### M60. What writing an EROFS image allocates

- **Question.** Audit D12: the writer built each inode record in a vector that grew for
  its xattrs and tail, cloned directory names, split directories into a vector a block,
  allocated each tail and pad, and held every xattr body and the whole metadata area at
  once. What does writing 10,000 files cost, and what after?
- **Method.** `crates/image/tests/allocations.rs`: an allocator that counts this thread's
  allocations, reallocations and peak requested live bytes, over `erofs::write` alone,
  into a sink that keeps nothing but hashes it. The audit's five trees of 10,000 files.
  Before is 5d1811f's writer, after this change's; same host, 2026-09-30.
- **Results.**

| 10,000 files | Before: allocations / reallocations / peak bytes | After |
|---|---|---|
| empty | 20,141 / 357 / 4,300,383 | 23 / 29 / 3,800,172 |
| 512-byte, inline | 30,141 / 10,357 / 9,834,079 | 23 / 29 / 3,800,172 |
| 4,095-byte, plain | 30,141 / 357 / 4,300,383 | 23 / 29 / 3,800,172 |
| 4,096-byte, plain | 20,141 / 357 / 4,300,383 | 23 / 29 / 3,800,172 |
| empty, a 1,024-byte xattr each | 40,141 / 40,357 / 38,496,959 | 10,024 / 32 / 3,802,290 |

  Every image hashed the same before and after, as did the unit tests' sample tree of
  every kind of node.
- **Consequence.** The metadata area is written as it is laid out, its records at rising
  offsets: each record is built on the stack, its xattr body in one reused buffer, and
  its inline tail read straight out. Directories borrow their names from the tree and
  keep their blocks as ends among their sorted entries; pads come from one static
  block of zeros. What remains is one vector per inode with xattrs, which sorts its
  names into EROFS's order, and the layout of the inodes themselves.

### M61. What an image's tree holds of the layers that replaced it

- **Question.** Audit D11: the tree's arena only grew. Nodes that a later layer replaced
  or whited out stayed allocated, with their names, xattrs and link targets, while the
  writer wrote only what the root reaches. How much does that hold, and what does
  compacting between layers give back?
- **Method.** `crates/image/tests/allocations.rs`,
  `a_compacted_tree_holds_the_image_not_its_history`: requested live bytes, counted on
  this thread, of a tree of 10,000 files with a 1,024-byte xattr each. It is built once,
  and then inserted over itself four more times, as layers replacing the files would;
  `Tree::compact` follows. 2026-09-30, this machine.
- **Results.** One generation: 17,807,029 bytes. Five: 84,779,189 before compacting,
  17,296,469 after.
- **Consequence.** The store compacts the tree after each layer. Compaction keeps what
  the root reaches, a hard-linked node once, renumbered from the root. It costs a walk of
  the tree, and nothing when the layer replaced or removed nothing.

### M62. What a warm VM spends copying the pages its working set wrote

- **Question.** Audit D02: HVF's prefetch writes each page the guest wrote, in guest
  context, so every warm VM holds those pages' private copies before any request, and
  whether or not one comes. What do they cost it, and what do they buy?
- **Method.** `docs/research/measurements/prefetch-writes/`: `SHARDS_PREFETCH_WRITES=0`
  prefetches every page as a read. The fleet measurement with 10 templates of the test
  image (M49) ran with writes and without, alternating, three rounds (the third in the
  other order). It reports the warm VMs' physical footprint, and runs one at a time and
  in bursts of 8. 8e00eec, 2026-09-30, this machine, load average about 16 (another VM and
  builds running): the arms are paired, not absolute.
- **Results.**

| Round, prefetch | 10 warm VMs' footprint | One at a time, p50 / p99 | Bursts of 8, p50 / p99 |
|---|---|---|---|
| 1, writes | 58.5 MiB | 6.0 / 6.2 ms | 52.1 / 94.7 ms |
| 1, reads | 57.3 MiB | 6.0 / 6.9 ms | 20.3 / 25.8 ms |
| 2, writes | 57.6 MiB | 5.8 / 6.4 ms | 20.1 / 23.7 ms |
| 2, reads | 57.6 MiB | 6.0 / 6.4 ms | 20.0 / 23.0 ms |
| 3, reads | 57.3 MiB | 5.6 / 6.1 ms | 21.0 / 26.1 ms |
| 3, writes | 56.7 MiB | 6.1 / 6.6 ms | 22.5 / 25.9 ms |

  The first round's bursts with writes were the first runs after a build: that round's
  template saves took 302 ms at the median, where every later round's took 157 to 162.
  Rounds 2 and 3, in either order, do not repeat it.
- **Consequence.** For this image the written pages cost a warm VM no measurable memory
  and buy no measurable time. Nothing bounded them, though: a working set recorded from
  a workload that writes much of its RAM would have every warm VM copy all of it before
  any request. A restore now keeps the written marks, in first-touch order, only up to
  `hv::PREFETCH_PRIVATE`, 64 MiB, and prefetches the pages past it as reads. That is
  above the largest working set measured (M33's 3,900 pages of 16 KiB, were each
  written), so no measured workload changes. The pool's size times the budget bounds a
  fleet.

### M63. A busy guest's snapshot pause, its durable flush moved past it

- **Question.** M45's busy guest paused 0.35 to 0.61 s for its snapshot, at 8 vCPUs after
  a 1 s storm. After D01, which skips pages the guest never touched, where does the pause
  go, and must the guest wait for all of it?
- **Method.** The storm test, 8 vCPUs, `SHARDS_STORM_MS=1000`, `SHARDS_LOG=info`, the
  VM's log lines printed by a temporary change to the test. First with the memory file's
  save and durable flush timed apart, n = 5. Then with the write split in two, n = 20:
  - `snapshot::stage` writes the memory and state files while the guest is paused.
  - `Staged::commit` makes them durable, renames the generation into place and points
    `current` at it, with the guest running.
  2026-09-30, this machine, load average 16 to 23 (another VM running).
- **Results.**
  - Before the split, the whole write took 44.1 to 48.9 ms: the memory's save 9.0 to
    11.1 ms, its durable flush 12.6 to 16.5 ms, and the state file, the directory syncs,
    the rename and the pointer the rest.
  - After it, the guest paused 11.7 to 13.8 ms in 19 of 20 runs (p50 12.4 ms), and
    54.9 ms in one. The commit took 35 to 47 ms, with 71 ms in one run and 106 ms in
    the run whose pause was 54.9 ms. Both halves slowed together in that run, and 15
    further runs did not repeat it: the host's disk and CPU, shared with another VM
    running at 900% CPU.
- **Consequence.** The guest runs again once its state is in the staged files. The
  snapshot becomes durable and in use while it runs, before the VM process can end, since
  the coordinator's thread is joined. It is visible only once `current` points at it, as
  before. `Handle::wait_for_snapshot` waits for the commit: a warm VM that saved a
  template tells the daemon it is ready, and the daemon settles the template, only after
  it.

### M64. Fleets of restored VMs beside Firecracker's: what each VM costs the host

- **Question.** Audit D14 asks density to be measured as a fleet pays for it, the kernel
  included, where a lone VM's accounting charges it all the page cache its template
  shares. What does each of 16 restored VMs held at once cost, shards' against
  Firecracker's?
- **Method.** The Firecracker comparison's density rounds (`benches/firecracker.rs`,
  `density`): 16 restores of each VMM's snapshot of the beating test guest (128 MiB, 1
  vCPU) held at once, in rounds S F F S S F F S. Each VM gives its PSS, private pages and
  page tables with its fleet running. Each round gives the host's MemAvailable given up
  per VM, which counts the kernel's and KVM's memory and leaves out the reclaimable page
  cache. The envelope pairs each shards VM with the Firecracker VM of the same index in
  the matching round. CI's ubuntu-24.04 x86_64 runner (AMD EPYC 7763), 3d222b4, run
  36708222649, 2026-09-30.
- **Results (per VM; p50, and the envelope's paired median difference with its 95%
  interval).**

| | shards | Firecracker | shards − Firecracker |
|---|---|---|---|
| PSS | 1.9 MiB | 5.7 MiB | −3.8 [−3.8, −3.8] |
| private pages | 0.7 MiB | 5.0 MiB | −4.3 [−4.3, −4.3] |
| page tables | 0.1 MiB | 0.2 MiB | −0.1 [−0.1, −0.1] |
| host MemAvailable given up | 0.2 MiB | 2.5 MiB (3.1 paired) | −2.8 [−3.6, −1.9] |

- **Consequence.** A fleet of shards VMs costs its host less per VM than Firecracker's,
  on every axis measured. A lone restore's PSS is 2.6 MiB above Firecracker's (M53):
  its snapshot file's pages, which the page cache holds once for every VM of the
  template. The envelope's single-restore allowances now say so. The benchmark keeps
  measuring density, so a regression fails CI as the other rows do.

### M65. Which preemption model gives PID 1 its CPU back from the crypto self-tests

- **Question.** M39: in about 30% of cold arm64 boots PID 1 waits a tick, 10 ms, for
  the crypto self-tests' kthreads to give up the one vCPU; the kernel is `PREEMPT_NONE`.
  Does voluntary or full preemption give it back sooner?
- **Method.** `docs/research/measurements/boot-preempt/`: the pinned kernel built in the
  pinned builder with `CONFIG_PREEMPT_DYNAMIC` (run 36711540724), booted by
  `kernel-ab/ab.py` against itself, `preempt=none` against `preempt=voluntary`, then
  against `preempt=full`, alternating, n = 100 each. ecdaa64, 2026-09-30, this machine.
- **Results (µs, p50 / p90 / p99 / max).**

| Arm | boot_kernel | restore |
|---|---|---|
| none | 16237 / 26492 / 27325 / 27449 | 6654 / 7092 / 7622 / 7720 |
| voluntary | 26126 / 26401 / 27007 / 27775 | 6536 / 7067 / 7622 / 7746 |
| none | 16181 / 26285 / 26422 / 26461 | 6580 / 7308 / 8128 / 8378 |
| full | 26048 / 26248 / 26460 / 27045 | 6501 / 7185 / 7807 / 8358 |

- **Consequence.** Preemption makes it worse: the self-tests' kthreads, able to take the
  CPU from PID 1, take it in nearly every boot, and PID 1 waits the tick almost always,
  where without preemption it waits in about a third. Restores, which never run the
  self-tests (M38), are unchanged. The kernel stays `PREEMPT_NONE`; the tick's length,
  `HZ`, is measured next (hz.config).

### M67. What a command-line tool may do in App Sandbox, and what it costs

- **Question.** D30's macOS confinement used `sandbox_init`, which the SDK marks deprecated
  and "No longer supported" [sandbox.h:7,45]. Can App Sandbox, the supported sandbox, confine
  a VM process started from the command line, and give it the files and sockets a VM needs?
- **Method.** `docs/research/measurements/app-sandbox/`: a C probe signed ad hoc with App
  Sandbox, the hypervisor entitlement and Hardened Runtime, its Info.plist linked into
  `__TEXT,__info_plist`, run from an unsandboxed parent that passes descriptors and bookmarks
  (`bookmark.c`); `cost.py` times its launch against the same binary without App Sandbox,
  alternating, n = 200 each. macOS 26.4.1, Apple M5 Max, 2026-09-30, load average about 16.
- **Results.** docs/research/macos-confinement.md §2 lists every trial. In short: without an
  embedded Info.plist the tool is killed at launch; with one it runs in its own container and
  creates VMs. It cannot open, create or dial anything it was not given, nor bind TCP. It uses
  descriptors it inherits and files and directories granted by bookmark, but not `openat`
  under a passed directory descriptor, and cannot bind or dial Unix sockets outside its
  container even in a granted directory; it can accept on a listener its parent bound. A
  rebuild with another CDHash used the same container without a prompt. Launch to exit:
  sandboxed p50 7,200 / p90 8,106 / p99 8,594 / max 8,867 µs; unsandboxed 4,115 / 4,793 /
  5,075 / 5,501 µs.
- **Consequence.** App Sandbox replaces the Seatbelt profile (macos-confinement.md §3): files
  by bookmark, sockets by descriptor, about 3.1 ms at launch.

### M66. What ticking at 1,000 Hz costs a VM's host, and what it buys a cold boot

- **Question.** M65 left the tick's length: at `CONFIG_HZ=100` a cold arm64 boot that loses
  the CPU to the crypto self-tests waits 10 ms for it. At 1,000 Hz the wait is 1 ms, but a
  busy vCPU takes ten times the timer interrupts. What does each cost, measured?
- **Method.** `docs/research/measurements/boot-preempt/`: the pinned kernel, and the same
  built with `hz.config` (`CONFIG_HZ=1000`, run 36712388813), both `PREEMPT_NONE` and
  tickless when idle (`NO_HZ_IDLE`). `tick-cost.py` runs the test guest's `work` mode
  (hashing 1 GiB) and a boot with no work, alternating kernels, n = 100 pairs, and reports
  the VM process's host CPU time from wait4(2) and the median of the paired differences with
  a bootstrap 95% interval; an earlier run of n = 50 gave idle guests held 10 s. Boots:
  `kernel-ab/ab.py`, n = 200 each. de378dd and cb60bec, 2026-09-30, this machine, load
  average 13 to 20 from another VM running beside.
- **Results.**
  - Idle, 10 s (host CPU, p50 / p90 / p99 / max): 100 Hz 50.4 / 53.2 / 56.6 / 56.6 ms;
    1,000 Hz 50.3 / 53.0 / 55.9 / 55.9 ms. No cost: an idle vCPU does not tick.
  - Busy, 1 GiB hashed, host CPU less a boot's: 100 Hz 1,308.6 / 1,467.5 / 1,489.3 /
    1,512.2 ms; 1,000 Hz 1,316.6 / 1,487.9 / 1,509.7 / 1,532.8 ms. Paired, 1,000 Hz costs
    +9.3 ms [+4.8, +17.4], +0.71% [+0.37%, +1.33%] of the busy CPU. Wall time: +3.9 ms
    [−0.8, +19.3], not distinguishable.
  - A boot's host CPU: −7.5 ms [−8.3, −0.2] at 1,000 Hz.
  - Cold boot, guest entry to PID 1 (µs, p50 / p90 / p99 / max): 100 Hz 26,152 / 26,833 /
    27,418 / 28,937; 1,000 Hz 17,964 / 19,463 / 21,275 / 23,391. Under this load the 100 Hz
    kernel lost the tick in nearly every boot (M39 saw about a third, unloaded).
  - Restores (µs, p50): 7,718 and 7,784, the same: they never run the self-tests (M38).
- **Consequence.** Measured, not decided: 1,000 Hz makes a cold boot about 8 ms faster and
  costs a busy vCPU 0.4% to 1.3% more host CPU; idle VMs and restores are unaffected. The
  kernel is not changed without the user's decision, since the change also publishes a
  kernel release.

### M68. What serving the run's vsock ports in the VM process costs a pooled run

- **Question.** f434f9f serves the run and signal ports by socket pair in the VM process
  (D30), where each warm VM bound `<path>_<port>` in a private directory and its device
  dialled it. What does a pooled run gain or pay?
- **Method.** `build-ab/ab.py`, a487fea against f434f9f, each with its own daemon and home,
  `shards run --pull never alpine true` alternating, n = 3000 per arm. Both restore one
  template: the guest side (shards-init, the kernel) is the same in both (same init and
  kernel digests), so the old arm's generation was copied into the new arm's template.
  The comparison is of the two commits, so it includes f434f9f's other changes, none on
  a run's path but `settle`'s scan, which only commands take. 2026-09-30, this machine,
  load average 9.5 to 9.9 from another project's build.
- **Results** (µs; the paired difference is the median with a bootstrap 95% interval):

| Part | Arm | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|---|
| wall | a487fea | 3000 | 5250 | 7028 | 18731 | 67921 |
| wall | f434f9f | 3000 | 5197 | 6894 | 17026 | 103432 |
| command | a487fea | 3000 | 685 | 868 | 1878 | 16889 |
| command | f434f9f | 3000 | 684 | 871 | 2149 | 31290 |
| outside the guest | a487fea | 3000 | 4571 | 6171 | 15267 | 53579 |
| outside the guest | f434f9f | 3000 | 4523 | 6020 | 14835 | 80898 |

  - Paired, new − old: wall −66 [−81, −53]; command +1 [−3, +4]; outside −72 [−82, −60].
- **Consequence.** The change costs a run nothing and saves it about 66 µs, all outside
  the guest. The tails, at this load, are not conclusive either way: a quiet-host run is
  owed before any claim about them.

### M69. Whether moving each vCPU its part of the start costs a pooled run

- **Question.** The restore's start (vCPU states, working set, device state) was one
  `Arc<Start>` every vCPU thread held through its setup, and the machine checked with a
  `Weak` that it had gone. Now `split` moves each vCPU its own part and the machine keeps
  the rest. Does a pooled run pay for it?
- **Method.** `build-ab/ab.py`, dedc442 against the change, one template restored by
  both (the guest side is the same), `shards run --pull never alpine true`, n = 3000 per
  arm, interleaved. 2026-09-30, this machine, load average 3.5 to 8.6. A pooled VM
  restores before its request, so this measures the request's path; the restore itself
  was not timed apart.
- **Results** (µs; paired differences are medians with bootstrap 95% intervals):

| Part | Arm | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|---|
| wall | dedc442 | 3000 | 4779 | 5141 | 5465 | 16184 |
| wall | split | 3000 | 4764 | 5124 | 5479 | 27941 |
| command | dedc442 | 3000 | 715 | 1057 | 1133 | 1302 |
| command | split | 3000 | 709 | 1060 | 1135 | 1215 |
| outside the guest | dedc442 | 3000 | 3949 | 4322 | 4653 | 15514 |
| outside the guest | split | 3000 | 3952 | 4299 | 4662 | 27255 |

  - Paired, new − old: wall −15 [−26, −3]; command +0 [−5, +4]; outside −9 [−19, +3].
- **Consequence.** No cost on a run's path. The max differs by single runs under load, as
  in M68: no claim on the tails.

### M70. What a VM process in App Sandbox can be granted read-only

- **Question.** App Sandbox's grants (D30, macOS) come from the VM's spawner as bookmarks.
  A VM only reads most of what it is given (kernel, init, its image, a template), and a
  template is shared: a VM that could write one could change every run restored from it.
  Can a grant be read-only?
- **Method.** `docs/research/measurements/app-sandbox/run.sh`: the sandboxed probe
  (`probe.c`, signed with `entitlements.plist`, Info.plist embedded) is handed, in one
  process per trial since a sandbox extension lasts the process's life, a bookmark made by
  an unsandboxed parent with options 0 (read-write) or
  `kCFURLBookmarkCreationSecurityScopeAllowOnlyReadAccess`, for a file and for a
  directory, or a descriptor the parent opened read-only. Its files live under `$HOME`:
  App Sandbox lets a tool read the directory its own executable is in, so a file beside
  the probe reads with no grant (the control trial shows it), as a first run of this
  probe, with its files there, wrongly found read-only bookmarks working. macOS 26.4.1,
  Apple M5 Max, 2026-09-30.
- **Results.**

| Grant | Read | Write |
|---|---|---|
| Read-write bookmark, file | ok | ok |
| Read-only bookmark, file | refused | refused |
| Read-write bookmark, directory: a file in it read, one made in it | ok | ok |
| Read-only bookmark, directory | refused | refused |
| Descriptor opened read-only, by its number | ok | — |
| The same, opened again as `/dev/fd/N` | ok | refused (EACCES) |
| No grant: a file under `$HOME` | refused | — |
| No grant: a file beside the probe's executable | ok | — |

- **Consequence.** A bookmark passed between processes grants read and write or nothing:
  `AllowOnlyReadAccess` is for a process's own security-scoped bookmarks, and its
  resolver here gets nothing. What a VM only reads it is given as descriptors opened
  read-only, which it reaches as `/dev/fd/N`: it can read them, and neither write them nor
  reach their paths. Bookmarks, read-write, are for the directories a VM writes in, which
  are its own (a template it saves, its container's logs).

### M71. Who answers a VM's grants: the daemon, or a broker process

- **Question.** A VM in App Sandbox asks its spawner for every file it opens (D30, M70).
  `shards vm` must leave that to another process: it becomes the VM by exec, and it links
  nothing of the broker's. The daemon can answer on the thread that watches the VM, or
  spawn the same broker, `shardsd grants`, for each VM. What does each cost a warm VM's
  start, and a run?
- **Method.** `docs/research/measurements/grant-broker/`: `broker.patch` (against the commit
  that adds it) makes `SHARDS_GRANT_BROKER=spawn` have the daemon spawn the
  broker, logs each warm VM's start (spawn to READY, every grant answered), and has the VM
  log how long its first ask waits and when each answer arrives. `ab.py` runs `shards run
  --pull never alpine true` through two daemons of that build, alternating; each run's
  warm VM is replaced, and those starts are the samples. Process starts were timed alone,
  to exit, with output to `/dev/null`. macOS 26.4.1, Apple M5 Max, 2026-10-01; the host
  was shared with other work (load average 4 to 12), which the paired differences cancel
  and the tails do not.
- **Results.**

| | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| Warm VM start, the daemon answers | 300 | 18.2 ms | 38.7 ms | 100.8 ms | 112.8 ms |
| Warm VM start, a spawned broker answers | 300 | 22.2 ms | 42.8 ms | 137.8 ms | 156.8 ms |
| Paired, spawned − daemon | 300 | +3.93 ms, 95% [+3.76, +4.14] | | | |
| Run wall clock, paired, spawned − daemon | 300 | −0.13 ms, 95% [−0.24, +0.00] | | | |
| First ask's wait, daemon / spawned broker (warm VMs) | 109 / 109 | 43 / 49 µs | 58 / 57 µs | | 145 / 80 µs |
| First ask's wait, `shards vm restore` (spawned broker) | 200 | 56 µs | 71 µs | 111 µs | 520 µs |
| A directory's grant (bookmark), daemon / fresh broker | 20 / 19 | 0.43 / 5.1 ms | | | |
| A file's grant (descriptor), either | | 35 to 160 µs | | | |
| Process start to exit: `/usr/bin/true` / `shards` / `shardsd grants` | 200 each | 1.3 / 2.2 / 3.8 ms | | | |
| The `posix_spawn` call itself, `shardsd` | 200 | 0.31 ms | | 0.61 ms | |

- **Consequence.** A broker is up before the VM it serves asks (its first answer waits
  no longer than the daemon's), and spawning it blocks its spawner 0.3 ms. The 3.9 ms is
  the directory's grant: a bookmark made in a fresh process pays for CoreFoundation's
  start, 5 ms, where the daemon, warm, makes one in 0.4 ms. So the daemon answers its own
  VMs, and a broker serves only `shards vm`, which pays the fresh bookmark only when it
  writes a directory (`--snapshot-dir`). Neither reaches a pooled run (the wall clock is
  unchanged), but a warm VM's start bounds how fast a pool refills.

### M72. A sandboxed launch after another build's

- **Question.** Comparing two builds' daemons run by run (`build-ab/ab.py`), warm VM starts
  alternated between about 20 ms and 120 ms, in both builds, where either build alone
  started them in 18 ms (M71's harness, one build). Every App Sandbox process signed with
  one identifier (`dev.shards.vm`) shares one container. What does a launch cost when the
  previous launch for that container was another build's?
- **Method.** `docs/research/measurements/app-sandbox/identity-switch.py`: two builds'
  shards-vm, each ad-hoc signed with `resources/vm.entitlements` and `-o runtime` (one
  identifier, different code), launched with `--version`, spawn to reap, one build
  repeatedly, then the two alternately. For scale, one of them signed with
  `resources/hvf.entitlements` alone, outside App Sandbox. Inside the warm VMs, a probe
  timed process start (`proc_pidinfo`) to `main`, to READY. macOS 26.4.1, Apple M5 Max,
  2026-10-01, load average 8.
- **Results.**

| Launches | n | p50 | p90 | max |
|---|---|---|---|---|
| One build, repeatedly | 58 | 9.3 ms | 10.4 ms | 11.6 ms |
| The other build, repeatedly | 58 | 9.3 ms | 9.8 ms | 10.5 ms |
| The two alternately | 58 | 109.3 ms | 117.9 ms | 135.8 ms |
| The first build again | 58 | 9.6 ms | 10.3 ms | 10.9 ms |
| Outside App Sandbox / in it, one build (100 each, twice) | 100 | 4.9–5.1 / 9.4–9.6 ms | 5.3–5.5 / 10.0–10.1 ms | 6.4 / 10.8 ms |

  In the daemons' A/B, the slow warm VMs lost the time before `main` (95 to 136 ms against
  9 to 15 ms); from `main` to READY every one took 11 to 13 ms. The first launch of a
  newly signed binary took 436 to 456 ms.
- **Consequence.** A launch whose container was last used by another build's code pays
  about 100 ms; one build's launches do not, and the cost goes with the switch. So:
  - Builds compared by alternating them on one host must not share a sandbox identity
    for their VM processes, or every launch measured pays the switch. A pooled run's wall
    clock, which no launch reaches, is unaffected (−10 µs, 95% [−80, +46], between two
    builds of one source).
  - Two ad-hoc signed builds of shards in use at once (two homes on two versions) pay it
    on every VM start that follows the other's. Ad-hoc code's designated requirement is
    its own hash, so every local build is another identity; a release signed by one
    Developer ID keeps one requirement across versions. Whether such builds pay the switch
    is unmeasured: it needs a Developer ID.
  - App Sandbox costs a launch 4.5 ms here, at load 8, against M67's 3.1 ms.

### M73. What granting a warm VM its containers' directory cost

- **Question.** A warm VM spent about 6 ms from `main` to holding its grants, where its
  grants were answered in under 2 ms (M71). Where did the rest go?
- **Method.** Timestamps in the VM process (a probe, not kept) from `main` through each
  step of a warm restore's setup, 25 warm VMs; then around the one bookmark it resolved,
  12 VMs. Then the change: the VM is granted no directory for logs, its request brings the
  log's first segment open and the daemon makes later segments
  (`kind::LOG_SEGMENT`); measured with `docs/research/measurements/grant-broker/broker.patch`
  (`ready-us`, spawn to READY) on builds before and after it, in alternating blocks of 60
  pooled `shards run --rm alpine true` with 0.15 s between runs, one build at a time (a
  launch after the other build's pays M72's 100 ms; the first five starts of each block are
  dropped), six blocks each. macOS 26.4.1, Apple M5 Max, 2026-10-01; other work on the
  host (load average 5.6 to 10).
- **Results.** Before: `main` to its first grants 0.5 ms, and the last ask, the containers
  directory's bookmark, 5.7 to 6.0 ms, of which 0.4 ms is waiting for the answer and 3.9 to
  4.1 ms resolving it (`CFURLCreateByResolvingBookmarkData`, the process's first
  CoreFoundation call). Warm VM starts, spawn to READY:

| | n | p50 | p90 | p99 | max | block medians |
|---|---|---|---|---|---|---|
| Before | 371 | 21.0 ms | 45.2 ms | 271.3 ms | 531.1 ms | 27.0 26.9 38.9 16.6 18.9 17.0 |
| After | 371 | 14.8 ms | 47.4 ms | 170.6 ms | 418.4 ms | 16.0 19.6 16.0 16.1 13.2 13.3 |

  After is faster in every block, by 0.5 to 22.9 ms; in the two quietest, by 5.7 and
  3.7 ms. The tails are the host's other work: both builds share them.
- **Consequence.** A VM is given no directory for its log. Besides its start, this ends a
  wider grant: a pooled VM, which cannot know its container ahead, could write every
  container's log and record (on Linux too, by its Landlock rule). Bookmarks remain only
  for a template's directory, on a cold path. The tails need a quiet host to say more.

### M74. Plain files for DAX, against inline tails

- **Question.** A guest serves an EROFS file with DAX, straight from the pmem the host
  maps, only if its data is whole blocks (`FLAT_PLAIN`; fs/erofs/inode.c at v6.18.48); a
  file with an inline tail goes through the guest's page cache, copied into the VM's own
  memory. Writing every regular file plain costs image size. What does it save, and what
  does it cost?
- **Method.** Two builds of one revision but for the EROFS writer (inline tails, and
  regular files plain: `ROOTFS_VERSION` 1 and 2), each with a home holding alpine and
  python:3.13-slim pulled and their templates saved. Sizes from the image store. Then
  `docs/research/measurements/dax/dax.py`: a restore of the python template imports 24
  standard modules and waits while `footprint` reads its process; builds alternate in
  blocks of 8 (M72), 4 blocks each. The image writer's allocation test gives a synthetic
  extreme, 10,000 files of 512 bytes. macOS 26.4.1, Apple M5 Max, 2026-10-01; load average
  4 to 12.8 from other work.
- **Results.**

| | inline tails | plain |
|---|---|---|
| alpine's EROFS | 8,736,768 B | 8,962,048 B (+2.6%) |
| python:3.13-slim's EROFS | 143,024,128 B | 151,506,944 B (+5.9%) |
| 10,000 files of 512 B | 6,078,464 B | 41,504,768 B (6.8×) |
| VM footprint after the imports, p50 / p90 / max (n 32 each) | 63.0 / 63.0 / 63.0 MB | 42.0 / 43.0 / 43.0 MB |
| The imports, p50 / p90 / max | 483.5 / 582.6 / 590.2 ms | 419.5 / 574.8 / 605.2 ms |

- **Consequence.** Regular files are written plain (D15): each VM that reads an image's
  files keeps 21 MB less of its own for this workload, what it read staying in the host's
  page cache, which every VM of the image shares, for images 3 to 6% larger. The imports
  were no slower; at this load the samples say no more. A tree of many small files grows
  most, toward a block a file.
- **Found validating it.** The VM that saves an image's template serves the first run while
  it records the working set, all guest memory taken away at stage 2. A guest kernel cleans
  the data cache over a page of the image before it executes it (`DC CVAU`), and the abort
  reports WnR set, as every cache maintenance abort does (ESR 0x92000147: CM and WnR, no
  instruction syndrome). HVF's fault handler took it for a write to read-only pmem, could
  not emulate it, and the VM ended: alpine's and python's first runs failed. A cache
  maintenance abort now reads as a read (`esr::cache_maintenance`): the page goes back
  read-only and the instruction runs again; on no guest memory it is skipped, as KVM skips
  it. `daemon.rs`'s `runs_may_clean_the_cache_over_their_image` runs `DC CVAU` over an
  image file in a first run and a restored one; without the change the first fails.

### M75. Starting sandboxed VM processes at once: launches, and forks

- **Question.** In an open-loop run of the arrival benchmark (`benches/arrival.rs`, rates 2
  to 160 per second for 10 s each, revision 8929f92), no request failed and the daemon kept
  up to 160 per second, but p99 rose early: 118 ms at 10 per second, where p90 was 14 ms.
  Replayed at 10 per second for 30 s with each run's timing line, the slowest were in the
  first 1.2 s, as the pool grew to the demand, and the slowest of all (309 ms) went to a
  VM whose own clock had it ready 5 ms after `main`: the time went before `main`. How do
  sandboxed launches scale when several start at once? And would forking VM processes
  from one sandboxed process, which a child inherits without a launch, avoid it?
- **Method.** `docs/research/measurements/app-sandbox/concurrent.py`: K launches of
  shards-vm `--version` at once, signed into App Sandbox and not, 64 each. `zygote.c`: a
  process in App Sandbox (with the hypervisor entitlement) starts K children at once,
  by `posix_spawn` of itself or by `fork`, 64 each; each child creates and destroys a
  Hypervisor.framework VM and must be refused a file under `$HOME` outside its container,
  which an unsandboxed process reads. macOS 26.4.1, Apple M5 Max, 2026-10-01, load
  average 7.
- **Results** (p50 / p90 / max, ms).

| At once | shards-vm, App Sandbox | shards-vm, no sandbox | probe launched | probe forked |
|---|---|---|---|---|
| 1 | 6.9 / 7.4 / 8.2 | 3.6 / 3.8 / 501.5 (first launch) | 7.5 / 13.8 / 32.3 | 0.80 / 0.92 / 1.04 |
| 4 | 9.5 / 11.5 / 11.7 | 3.4 / 3.7 / 3.9 | 14.5 / 20.5 / 25.6 | 0.89 / 1.70 / 1.97 |
| 16 | 18.6 / 27.2 / 28.9 | 3.7 / 4.4 / 4.8 | 34.5 / 50.3 / 54.3 | 3.49 / 5.77 / 6.60 |
| 32 | 30.6 / 47.3 / 51.1 | 3.9 / 4.2 / 4.6 | 46.0 / 73.1 / 78.5 | 6.61 / 11.94 / 13.65 |

  Every child, launched or forked, made its VM and was refused the outside file: a forked
  child keeps the sandbox.
- **Consequence.**
  - Sandboxed launches queue on something the system serializes; unsandboxed ones do not.
    A burst of refills pays it, off a run's path while the pool holds VMs, and on it when
    the pool is short, as in the replay's first second.
  - Forking VM processes from one sandboxed process would start them 7 ms sooner and keep
    their sandbox, but every VM would share that process's address-space layout: one
    leaked address would serve against every VM. Android's zygote showed that sharing
    exploitable, and Morula answered it with a fresh process prepared ahead for each use
    (Lee et al., "From Zygote to Morula: Fortifying Weakened ASLR on Android", IEEE S&P
    2014). shards' warm pool already is that: each VM process launched fresh, ahead of its
    request. VM processes are not forked; what to make better is how far ahead the pool
    launches when demand rises (D26), measured from this benchmark.

### M76. Taking a context file into a build's stage: a clone, or the pack

- **Question.** `shards build` takes one version of every context file into a stage
  private to the build (D33). An APFS clone copies no data, but makes a file the build
  later opens and removes; appending the bytes to one pack file copies them, but makes no
  file. Which costs less, and from what size?
- **Method.** `docs/research/measurements/build-context/clone.py DIR 2000 10`: each sample
  takes 2,000 files of one size into a fresh directory, by `fclonefileat` and then the
  unlinks the stage's removal does, or by reading each into one pack file and removing that;
  the two alternate which goes first. Microseconds per file, n = 10 samples per size and
  method. 2026-10-01, Apple M5 Max, macOS 26.4.1, APFS; load average 37 from other work.

| Size | Clone p50 | p90 | p99 | max | Pack p50 | p90 | p99 | max |
|---|---|---|---|---|---|---|---|---|
| 1 KiB | 193.7 | 220.2 | 239.1 | 239.1 | 23.6 | 41.5 | 59.4 | 59.4 |
| 16 KiB | 220.9 | 519.8 | 686.2 | 686.2 | 31.2 | 45.0 | 45.0 | 45.0 |
| 64 KiB | 376.8 | 486.8 | 640.3 | 640.3 | 82.1 | 144.0 | 171.6 | 171.6 |
| 256 KiB | 333.1 | 482.9 | 609.7 | 609.7 | 97.9 | 246.4 | 255.0 | 255.0 |
| 1 MiB | 215.5 | 332.0 | 632.8 | 632.8 | 274.6 | 400.4 | 537.8 | 537.8 |
| 4 MiB | 216.3 | 235.8 | 375.9 | 375.9 | 2329.2 | 2968.9 | 7238.6 | 7238.6 |

- **Result.** A clone costs about 200 µs whatever the size; packing grows with it. Below
  1 MiB packing is cheaper at every percentile, at 1 MiB they meet, and at 4 MiB the clone
  is ten times faster.
- **Consequence.** The stage packs files under 1 MiB and clones the rest
  (`crates/build/src/host.rs` `CLONE_MIN`). With a clone per file, a context of 50,000
  files of 1 KiB took 91 s to build, against Docker's 6.3 s; packed, 2.3 s against 4.9
  (benchmarks.md, Build).

### M77. Pure-Rust xz and bzip2 decoders, against the tools that write them

- **Question.** ADD unpacks a local archive compressed with gzip, bzip2, xz or zstd
  (moby/go-archive's DecompressStream). moby runs the `xz` program for xz, so BuildKit
  fails to ADD a `.tar.xz` where `xz` is not installed (`exec: "xz": executable file not
  found in $PATH`, seen in scripts/build/generate's container before it installed xz).
  shards decodes in-process, in pure Rust. Which decoders decode everything the tools
  write?
- **Method.** `docs/research/measurements/decompress/cases.sh DIR` writes, with XZ Utils
  5.8.4 and bzip2 1.0.8: xz with each check (none, CRC32, CRC64, SHA-256), the x86 and
  ARM64 BCJ filters, delta, several blocks, an empty input, two streams concatenated, and
  two with stream padding between them; bzip2 one stream, two concatenated and empty; and
  one corrupt and one truncated file of each. `cargo run --release --manifest-path
  docs/research/measurements/decompress/Cargo.toml -- DIR` decodes each. 2026-10-01, Apple
  M5 Max, macOS 26.4.1.
- **Result.** lzma-rust2 0.21.0 (features `std`, `xz`) decodes every xz case byte for byte
  and refuses the corrupt and truncated ones. xz4rust 0.2.3 decodes only the first of
  concatenated or padded streams, which `xz -d` decodes whole. bzip2 0.6.1 on its default
  libbz2-rs-sys backend decodes every bzip2 case and refuses the corrupt and truncated.
- **Consequence.** `crates/build` decodes xz with lzma-rust2 and bzip2 with bzip2, as
  well as gzip with flate2 and zstd with the store's ruzstd decoder. Built without its
  `optimization` feature, lzma-rust2 forbids unsafe code. With `xz` installed for BuildKit,
  the ADD cases of `crates/build/testdata/ops.json`, one archive of each compression among
  them, unpack byte for byte as BuildKit unpacks them.

### M78. Memory of a build that ADDs a million entries

- **Question.** A build that ADDs an archive of a million empty files peaked at 915 MB of
  memory (D33's stress test), about 900 bytes an entry. Where does it go, and how much of
  it does the build need?
- **Method.** Temporary `getrusage` probes after each step put the peak at the export:
  the ADD step itself peaked at 311 MB (238 MB after the copy, 311 MB after its layer was
  written), and the export rose from there building the root filesystem, while the
  build's snapshots, sources and stages were still held. Two changes followed:
  `shards build` drops its executor and results once every layer is in the store, before
  the export (`crates/shards/src/build/mod.rs`); and `layer::apply` reads a layer twice,
  whiteouts in the first pass and everything else in the second, instead of holding a
  list of its entries (`crates/image/src/layer.rs`). Measured with
  `docs/research/measurements/build-memory/run.sh SHARDS DIR 1000000 N`, which writes the
  archive (`d{n/100}/f{n}`, USTAR, gzip) and runs the build N times with `/usr/bin/time
  -l`. 2026-10-01, Apple M5 Max, 128 GB, macOS 26.4.1; revision 2cb2db1 against it with
  both changes.
- **Result.** Maximum resident set size:

  | Build | n | p50 | max |
  |---|---|---|---|
  | 2cb2db1 | 4 | 935 MB | 937 MB |
  | both changes | 5 | 616 MB | 617 MB |

  With the probes, dropping the build's state took the peak from 861 MB to 726 MB, and
  the two-pass apply took it to 588 MB. What remains is the export's: 440 MB once the
  layers are stacked into the tree, and 572 MB once the EROFS image is written. User and
  system time are unchanged (6.2 to 6.8 s and 0.7 to 0.9 s a run). Wall time varied from
  8 to 19 s in both builds, on a volume 99% full; what the off-CPU time waits on is not
  yet measured.
- **Consequence.** Both changes are kept. The tree the export builds, about 440 bytes an
  entry, and the EROFS writer's 130 MB are what a further cut would go after.
- **Second round.** `docs/research/measurements/build-memory` (a crate: `cargo run
  --release --manifest-path .../Cargo.toml -- LAYER.tar [IMAGE]`) applies one layer as
  `Store::rootfs` does and writes its image, counting the heap with its global allocator:
  the bytes requested and live, and their peak, at each step. For the million entries
  (USTAR, uncompressed), at the revision above:

  | Step | live | peak |
  |---|---|---|
  | `layer::apply` | 157 MB | 261 MB |
  | `erofs::write` | 157 MB | 364 MB |

  The writer held an `Inode` of two vectors for every file, in a vector grown by
  doubling, a `HashMap` from node to inode, and every directory's sorted entries at once:
  207 bytes an entry. `apply` held a set of every entry the layer made (a node id and its
  name), and the tree's node vector doubled, holding both copies as it moved. Now an
  inode is 48 bytes, in a vector sized once; nodes map to inodes through a `u32` vector;
  a directory's entries are made again from the tree when its blocks are written; an
  entry this layer made is known by its node being newer than the layer's first, or by a
  set of the hard links it made to older nodes; and the tree keeps its nodes in chunks
  of 65,536 that grow as vectors do, so growing moves one chunk at most:

  | Step | live | peak |
  |---|---|---|
  | `layer::apply` | 154 MB | 156 MB |
  | `erofs::write` | 154 MB | 209 MB |

  The images are byte for byte those of the revision above, for this layer, Alpine
  3.22's, and one of hard links, devices, xattrs, large IDs, a 3 kB symlink, names that
  sort before `.`, directories of several blocks and inline tails. With `run.sh`, n 5:
  maximum resident set size p50 465 MB, max 470 MB, from 616 MB. Probes put the ADD
  step's peak at 338 MB and the export's at 463 MB.
- **Third round.** A directory's entries had been a `BTreeMap<Vec<u8>, NodeId>` in its
  node: a name allocated for each, and slots of 32 bytes in B-tree nodes partly full. Now
  the tree keeps every entry as one 16-byte record (its directory, what it names, where
  its name is, the next entry of its directory) in a chunked arena, every name in one
  byte arena, and one index from directory and name to entry: open addressing over
  `u32` entry ids, linear probing, at most three quarters full, hashed with SipHash keyed
  for each process, since names come from archives. A directory's node holds the head of
  its list, sorted only when listed. With the harness above:

  | Step | live | peak |
  |---|---|---|
  | `layer::apply` | 114 MB | 116 MB |
  | `erofs::write` | 114 MB | 169 MB |

  The three images are byte for byte the same again, the BuildKit oracle
  (`crates/build/tests/oracle.rs`) passes unchanged, and `run.sh`, n 3: 348, 350 and
  353 MB, from 465 MB. CPU time is unchanged, 5.8 to 5.9 s of user time: the ADD step's
  path walks, path-keyed change sets and staged archive are next.
- **Fourth round.** What a step changed had been kept as a set of every path it touched,
  each a heap string, and every operation resolved its path again: about six walks for
  each entry ADD unpacks. Now an entry carries the number of the last step that changed
  it, and a directory the entry that names it, so recording a change stamps the entry
  and climbs to the first ancestor already stamped; a removal leaves its entry in the
  directory's list, stamped, for its whiteout; the differ lists each directory's
  stamped entries. Path walks borrow their names instead of copying each. ADD resolves
  an entry's path once and makes it, owns it, sets its xattrs, mode and times on what it
  found, falling back to the path calls wherever something is in the way. The index
  keeps half of each entry's hash beside its id, so a probe reads an entry only when the
  hashes agree. An uncompressed layer is checked against its DiffID where it is stored,
  not copied first. The BuildKit oracle passes, with a case added whose files carry
  `security.capability`, the one extended attribute a layer keeps, which no case had
  covered. `run.sh`, n 3: 215, 217 and 225 MB; user time 3.5 to 3.7 s, from 5.9 s; the ADD
  step 3.2 s and the export 1.4 s of wall time.
- **Fifth round.** ADD had decompressed an archive whole into the stage (512 MB written
  for a million empty files) before unpacking it. Now a thread decompresses into a
  bounded channel of 256 KiB pieces, counting the bytes against the build's limit, while
  the unpacker reads the stream, and only regular files' contents go to the stage; the
  decompressor's error comes first, as it did when it ran first. A blob is hashed and
  written by a thread of its own; the tar writer puts octal fields' digits straight into
  the header, where it had formatted two strings for each; and the differ checks a
  parent against those already written only when it changes. `run.sh`, n 3: 219, 221 and
  222 MB; user time 2.82 to 2.92 s; the ADD step 1.4 s and the export 1.4 s. One run took
  5.6 s of wall time to the others' 3.2 and 3.6 with the same CPU time, on a host whose
  load average was 54 from other work; what it waited on is not yet measured. The export
  is now most of what is left: `layer::apply` reads back, twice, the layer the build
  wrote moments before.
- **Sixth round** (M80) writes the image from the build's last snapshot instead.

### M79. The signal port dialled before the workload starts

- **Question.** shards-init dialled the host's signal port once the workload had started,
  so a workload that dialled it first took it, and the host's signals went to the
  workload instead of to init; and the run port took further connections, which the host
  held unread (`crates/shards/tests/isolation.rs`, run before the change). Now each
  host port takes one connection, and init dials the signal port as soon as it holds the
  run port, before it receives the command. Does the earlier dial cost a run anything?
- **Method.** `docs/research/measurements/build-ab/ab.py OLD NEW alpine:3.22 300 true`,
  OLD at 4d59258 and NEW with the change, each restoring its own template: a template
  holds its own build's init, so one arm's cannot stand in for the other's. 2026-10-01,
  Apple M5 Max, macOS 26.4.1, load average 21 to 31 from other work on the host.
- **Result.** Paired differences, NEW minus OLD, median with bootstrap 95% interval: the
  command's time in the guest +4 µs [−4, +12]; the client's wall clock −23 µs [−98, +34].

  | Arm | n | p50 | p90 | p99 | max |
  |---|---|---|---|---|---|
  | command, OLD | 300 | 582 µs | 839 µs | 2812 µs | 12445 µs |
  | command, NEW | 300 | 591 µs | 843 µs | 2105 µs | 3489 µs |
  | wall, OLD | 300 | 5241 µs | 6150 µs | 15026 µs | 31720 µs |
  | wall, NEW | 300 | 5217 µs | 6069 µs | 10553 µs | 29089 µs |

- **Consequence.** No cost is measurable, and the dial is kept where no workload can take
  the port first. The p99 and max of both arms, up to 25 times their medians, are not
  explained by this comparison and are not yet root-caused.

### M80. A build's root filesystem from its last snapshot, and smaller nodes

- **Question.** The export of M78's build stacked the layers again (`Store::rootfs`):
  1.4 s, most of it reading back and applying the layer just written, and a second tree
  beside the writer. Writing the image from the build's last snapshot, put in the form
  its layers give it (`crates/build/src/stack.rs`), skips both; what does it save, and
  where does the memory go then?
- **Method.** `docs/research/measurements/build-memory/ab.sh BEFORE AFTER DIR 7`:
  interleaved builds of M78's context, `FROM alpine:3.22` and `ADD many.tar.gz /x/`, a
  fresh home each, with `/usr/bin/time -l` and `--progress=plain`, then the same with
  the feature `alloc-count`, whose global allocator and getrusage report each phase
  (`crates/shards/src/alloc_count.rs`); `phases.py DIR` summarizes. BEFORE is 8a26a7b
  with the phase marks alone; AFTER is d0835eb with the export's phase marks. vmmap
  `--summary` as the phases end (SHARDS_VMMAP). 2026-10-01, Apple M5 Max, 128 GB, 18
  cores, macOS 26.4.1, load average 43 to 51 from other work.
- **Result.** Whole build, n 7, p50 [p90, max]:

  | Build | wall | user + sys | max RSS | footprint | page reclaims |
  |---|---|---|---|---|---|
  | 8a26a7b | 3.52 s [8.28, 9.79] | 3.21 s [3.21, 3.22] | 219.7 MB [222.0, 222.0] | 212.9 MB | 21,123 |
  | d0835eb | 1.61 s [3.80, 3.99] | 1.92 s [1.95, 1.96] | 164.2 MB [166.9, 167.8] | 157.3 MB | 10,652 |

  BuildKit's step times, p50: the ADD step 1.50 s and 1.40 s; the export 1.60 s and
  0.10 s. Page faults (major) 7 in every run. Per phase, p50 of the counting builds:

  | Phase | Build | user ms | allocations | reallocations | requested | peak heap |
  |---|---|---|---|---|---|---|
  | ADD's file operations | 8a26a7b | 965 | 8,362,489 | 1,050,501 | 1,021 MB | 134.6 MB |
  | | d0835eb | 1,024 | 8,362,489 | 1,050,501 | 955 MB | 101.4 MB |
  | its layer written | 8a26a7b | 646 | 8,101,708 | 60,027 | 299 MB | 142.8 MB |
  | | d0835eb | 643 | 8,101,710 | 60,036 | 300 MB | 110.2 MB |
  | waiting on the blob | both | 0.5 | 36 | 5 | 0 | |
  | export | 8a26a7b | 1,261 | 10,116,689 | 820 | 644 MB | 183.5 MB |
  | | d0835eb | 74 | 20,471 | 168 | 44.8 MB | 129.7 MB |

  The blob writer kept up: the commit waited 9 ms p50 (42, 85 ms p90, max) for it. At
  the export's end vmmap shows the allocator keeping what was freed: 108 MB of empty
  large regions and 31 MB of empty small ones (163 and 33 MB before), against a peak
  heap of 130 MB; the 34 MB between the peak heap and the maximum resident set is the
  binary's own resident pages, about 14 MB before the build begins, and the allocator's
  regions. Nodes went from 80 to 56 bytes (xattrs boxed when there are any, a 12-byte
  packed `DataRef`, symlink targets boxed), index slots from 8 to 4 bytes (entry ids
  alone; applying the million entries took 0.96 s of user time to 0.94 s with the
  tagged slots, n 7 interleaved), and the writer's inodes from 48 to 32 bytes: the tree
  of a million entries 126 to 94 bytes an entry, the writer's 55 to 36. The images of
  M78's three layers are byte for byte those of 8a26a7b's writer.
- **Consequence.** The export writes from the snapshot wherever `stack.rs` can follow
  it, and stacks the layers as before where it cannot. What is left is the build step's:
  16.5 million allocations for a million entries, 8 for each in the file operations and
  8 in the differ, and the snapshot itself, 94 bytes an entry, which the export now
  holds while the writer adds its 36.

### M81. The files Docker gives a container, and loopback, in every run

- **Question.** A shards VM had loopback down (127.0.0.1 unreachable) and the image's own
  `/etc/hostname`, `/etc/hosts` and `/etc/mtab`, where Docker gives every container its
  own (moby daemon/initlayer/setup_unix.go). Of the 50 most pulled official images (both
  architectures, 99 images), 89 ship an `/etc/hostname` left from their build, 49 an
  `/etc/hosts`, 23 an `/etc/mtab` (amazonlinux's an empty file); none has an `/etc` that
  is not a directory. shards-init now brings loopback up and writes `/etc/mtab` and
  Docker's `/etc/hosts` lines before the template's snapshot, and at run start
  `/etc/hostname` and `/etc/hosts` with the run's own name on 127.0.1.1 (D16). Does the
  run-start part cost a run anything?
- **Method.** `docs/research/measurements/build-ab/ab.py OLD NEW alpine:3.22 300 true`,
  OLD at ab0457c and NEW with the change, each restoring its own template (M79).
  2026-10-02, Apple M5 Max, macOS 26.4.1, load average 30 to 32 from other work.
- **Result.** Paired differences, NEW minus OLD, median with bootstrap 95% interval: the
  command's time in the guest +16 µs [+4, +41]; the client's wall clock −16 µs [−79, +88].

  | Arm | n | p50 | p90 | p99 | max |
  |---|---|---|---|---|---|
  | command, OLD | 300 | 820 µs | 1083 µs | 1266 µs | 1270 µs |
  | command, NEW | 300 | 831 µs | 1142 µs | 1293 µs | 1423 µs |
  | wall, OLD | 300 | 5763 µs | 6494 µs | 7410 µs | 8232 µs |
  | wall, NEW | 300 | 5771 µs | 6595 µs | 7408 µs | 7549 µs |

- **Consequence.** The two files at run start cost the guest about 16 µs, within the tens
  of µs that restores differ by from template to template (M29), which this comparison of
  two templates cannot separate; the wall clock shows no cost. Both arms' medians, 5.8 ms
  at this load, are above the 5 ms target, as were M79's 5.2 ms at load 21 to 31.

### M82. What a builder VM's memory costs it

- **Question.** `RUN` steps run in one builder microVM per build (D34). Docker Desktop's
  VM, which runs Docker's builds on macOS, has half the host's memory (64 GiB of this
  host's 128, `docker info`, 2026-10-02), all 18 CPUs, 1 GiB of swap. What does a builder
  pay for its memory size?
- **Method.** The same three-`RUN` Dockerfile (alpine:3.22, files, `adduser`, a non-root
  step, `WORKDIR`), built warm through the signed binaries, n = 5 per size, sizes
  interleaved; the VM process's peak RSS from its timing line. 2026-10-02, Apple M5 Max,
  macOS 26.4.1, load average 20.
- **Result.**

  | Guest memory | Build wall p50 | max | VM RSS p50 |
  |---|---|---|---|
  | 512 MiB | 169 ms | 933 ms | 94 MiB |
  | 1 GiB | 195 ms | 244 ms | 104 MiB |
  | 2 GiB | 174 ms | 214 ms | 127 MiB |
  | 4 GiB | 192 ms | 199 ms | 223 MiB |
  | 8 GiB | 209 ms | 212 ms | 308 MiB |
  | 16 GiB | 246 ms | 270 ms | 470 MiB |
  | 32 GiB | 329 ms | 341 ms | 792 MiB |
  | 64 GiB | 434 ms | 445 ms | 1423 MiB |

- **Consequence.** About 21 MiB of RSS and 4 ms of boot per GiB of guest memory: the
  kernel's page structures, which it writes as it boots. A builder takes half the host's
  memory, Docker's capacity, and pays this; memory plugged as a build needs it would pay
  only for what the build uses. For scale, BuildKit in Docker Desktop's VM took 0.83 s
  uncached for the same build, against shards' 0.42 to 0.45 s warm.

### M83. How a VM process and its network process move frames

- **Question.** A VM with a network gets it from a network process of its own (D31). Its
  frames cross between the two processes; networking.md E2 left how open. Apple's model
  for a third-party stack is a connected datagram socket [VZFileHandleNetworkDeviceAttachment.h],
  which carries frames up to 65,561 bytes once SO_SNDBUF and SO_RCVBUF are raised
  (verified). But macOS fails a full Unix datagram socket's send with ENOBUFS rather than
  blocking, and poll(2) reports it writable all the while (verified: POLLOUT with three
  64 KiB frames queued and the fourth refused), so a sender has no way to wait but to
  retry.
- **Method.** `docs/research/measurements/net-transport`: two processes, as the VM process
  and its network process; one way, frames of 1514, 9014 and 65561 bytes for 2 s each;
  then 20,000 round trips of a 64-byte frame. Datagrams (send retried on ENOBUFS), a
  stream socket with each frame's length before it, and a ring of 64 slots in shared
  memory, the receiver spinning 2,000 times before it sleeps on a pipe the sender writes
  only then. 2026-10-02, Apple M5 Max, macOS 26.4.1, load average 22.
- **Result.**

  | Transport | 1514 B | 9014 B | 65561 B | 64 B round trip p50 / p99 / max |
  |---|---|---|---|---|
  | Unix datagrams | 11.5 Gbit/s | 55.0 Gbit/s | 75.0 Gbit/s | 22.1 / 118.2 / 348.5 µs |
  | Unix stream | 5.9 Gbit/s | 23.3 Gbit/s | 11.8 Gbit/s | 22.2 / 111.5 / 475.4 µs |
  | Shared ring | 131.5 Gbit/s | 158.0 Gbit/s | 369.6 Gbit/s | 0.8 / 1.0 / 83.1 µs |

- **Consequence.** Frames cross in a shared ring: 5 to 30 times the datagrams' throughput,
  a 25th of their latency, and backpressure that is the ring's own. The network process
  maps the ring alone, never guest memory, so D31's isolation holds: the VM process copies
  between the virtqueues and the ring. The ring's latency comes of a receiver that spins
  before it sleeps; what that spin costs an idle VM is measured with the device.

### M84. Where `load` decodes a compressed archive

- **Question.** `docker load` reads an archive of any compression go-archive's
  DecompressStream detects: plain, bzip2, gzip, xz and zstd, skippable frames before a
  zstd one included (measured: Docker 29.3.1 loads each). shards read plain, gzip and zstd,
  and zstd on a thread of its own, through a pipe, while the import read the pipe. Reading
  every compression through one decoder in the import's own thread is simpler; what does
  giving up the zstd thread cost?
- **Method.** `docs/research/measurements/load-decode/ab.py`: `shards load -i` of an
  archive of golang:1.26.8 whose layers are not compressed (915,750,400 bytes; BuildKit's
  docker exporter with `compression=uncompressed`), zstd -3 (274,664,165 bytes) and gzip
  -6 (299,263,848 bytes), a fresh home each run, the two builds interleaved. A is fe0e7de
  (the zstd thread); B decodes in the import's thread. 2026-10-03, Apple M5 Max, 128 GB,
  18 cores, macOS 26.4.1, load average 21 to 22 from other work.
- **Result.** Wall time:

  | Archive | Build | n | p50 | p90 | max |
  |---|---|---|---|---|---|
  | zstd | A | 15 | 3.541 s | 3.655 s | 3.793 s |
  | zstd | B | 15 | 3.642 s | 3.671 s | 3.699 s |
  | gzip | A | 5 | 3.521 s | 6.382 s | 8.261 s |
  | gzip | B | 5 | 3.531 s | 3.718 s | 3.829 s |

  Gzip was decoded in the import's thread by both builds. A load is mostly writing: the
  archive's blobs, then the root filesystem; the zstd thread overlapped about 0.1 s of a
  3.6 s load.
- **Also found.** lzma-rust2 0.21.0's XzReader reads a block's padding with one `read` and
  refuses fewer bytes (`src/xz/reader.rs`, consume_padding). A buffered stream returns
  fewer wherever the padding straddles the end of its buffer, so a sound `.tar.xz` can
  fail to decode. Fed a byte a read, XZ Utils 5.8.4's 96-byte stream of 29 bytes fails
  with "incomplete XZ block padding".
- **Consequence.** Every compression is decoded in the reading thread, through one
  decoder (`crates/build/src/archive.rs`, `decompressed`), so a decoder's error reaches the
  tar reader as itself, as Go's tar reader returns the decompressor's error. Keeping the
  0.1 s would need a channel that carries errors, for every compression. The xz decoder
  reads through `Full`, whose reads fill all they are asked for.

### M85. The allocations of applying a layer

- **Question.** `layer::apply` walked each entry's parent directory by copying every path
  component into a queue, kept a stack of the directories above for `..`, and checked
  whether each lower directory on the way was the layer's own by copying its name into a
  set's key, though only a symlink or a non-directory needs the answer. The tar reader
  cleaned each name into components and joined them again. What did it cost?
- **Method.** `docs/research/measurements/build-memory` counts allocations per step;
  `--below` applies lower layers first. `apply-ab.py` interleaves two builds of it, before
  (fe0e7de) and after: walks over borrowed names, `..` resolved through the tree (a
  directory has one name), freshness asked only where it decides, hard links to lower
  nodes kept by directory, and names cleaned in place. Workloads: M78's million entries
  (uncompressed), the same over itself, and golang:1.26.8's seven uncompressed layers.
  2026-10-03, the host of M84, load average 21.
- **Result.** The applies' time and allocations, summed, n 7 each:

  | Workload | Build | p50 | p90 | allocations |
  |---|---|---|---|---|
  | a million entries | before | 957.6 ms | 1,009.2 ms | 9,000,052 |
  | | after | 825.0 ms | 830.4 ms | 2,000,052 |
  | the million over themselves | before | 2,000.9 ms | 2,175.8 ms | 19,000,085 |
  | | after | 1,724.0 ms | 1,789.6 ms | 4,000,085 |
  | golang:1.26.8 | before | 193.1 ms | 197.4 ms | 572,298 |
  | | after | 173.7 ms | 176.1 ms | 80,075 |

  The EROFS images of all three are byte for byte those of the build before. Two
  allocations an entry remain: the name the tar reader reads in each of apply's passes.
- **Consequence.** Kept. An entry that borrowed its name from the reader would take the
  last two.

### M86. Reading layers back by position

- **Question.** Writing an image reads each file's data back from its layer's archive
  (`layer::Archives`): a seek, then a read. One positional read does both; what does that
  save?
- **Method.** `apply-ab.py` (M85), its `write` step, which now reads through
  `layer::Archives` as `Store::rootfs` does: fe0e7de against positional reads (pread(2);
  `seek_read` on Windows), the image of golang:1.26.8's seven layers (967,258,112 bytes,
  byte for byte the same both ways). 2026-10-03, the host of M84, load average 34 to 40
  from other work.
- **Result.** The write, n 7 each: 98.0 ms p50 (127.6 p90, 135.7 max) before, 79.3 ms
  (97.6, 104.0) after. M78's million empty files, which read no data: 74.5 and 74.6 ms.
- **Consequence.** Archives are read by position (`layer::ReadAt`). The build reads its
  own sources through the same `read_exact_at`.

### M87. Finding one image among many

- **Question.** `image inspect`, `tag`, `save` and `push` each read every image in the
  store (its index, manifests, configs and what of it is here) to find the one they were
  given. Reading the records alone, then only the image found: what does that save?
- **Method.** `docs/research/measurements/image-lookup/run.py A B 300 30 DIR`: a store of
  300 images (an OCI archive of one shared layer and a config each, loaded by each build
  into a home of its own), then `shards image inspect img-150`, the two builds
  interleaved, the client's wall time. A is fe0e7de; B reads records, then the one image.
  2026-10-03, the host of M84, load average 50 to 60 from other work.
- **Result.** n 30 each: A 29.80 ms p50 (32.55 p90, 41.66 p99, 41.85 max); B 13.12 ms
  (19.16, 20.96, 21.36).
- **Consequence.** Commands that name images read the records (`Store::named`) and then
  only what they name (`Store::image`); `images` reads them all, as it lists them all.
  What is left reads every record: an image's names are all its records', which only an
  index from image to names, kept as names come and go, would spare.

### M88. Finding one container among many

- **Question.** A container command finds its container by name, or by the start of its
  ID, by looking at every container in sight under the records' lock. How much does that
  cost as containers grow, and what does an index save?
- **Method.** `docs/research/measurements/container-lookup/run.py A B N 300 DIR`: a home
  of N exited containers' records for each build, then `shards port c-I` (by name) and
  `shards port PREFIX` (12 hex digits of its ID) of a random container I with no ports,
  the builds interleaved, the client's wall time. A is ace4820, which scans; B indexes
  names (a map kept as containers come and go) and finds a prefix in the ordered IDs.
  2026-10-03, the host of M84 (Apple M-series, macOS 26.4.1), load average 6 to 11 from
  other work. A calibration of A against itself gave 2.45 ms p50 at 10 containers, 2.65
  at 10,000, and 4.25 (name) and 6.53 (prefix) at 100,000.
- **Result.** n 300 each, p50 (p90, p99, max) in ms:
  - 10,000: by name A 3.49 (4.23, 4.83, 4.91), B 3.39 (4.13, 5.18, 6.53); by prefix
    A 3.67 (4.45, 5.27, 6.28), B 3.34 (4.09, 4.83, 5.86).
  - 100,000: by name A 5.57 (6.91, 8.21, 13.91), B 4.22 (4.95, 5.72, 6.07); by prefix
    A 8.25 (9.70, 11.11, 13.84), B 4.05 (4.68, 5.52, 11.21).
- **Consequence.** The records keep names in a map and IDs in order (`Registry::named`,
  `Registry::id_prefixed`), as dockerd's name registrar and prefix index do: a lookup no
  longer grows with the containers; a prefix, which scanned twice, gains most.

### M89. What a daemon and its runs cost while they idle

- **Question.** Each running container holds two daemon threads (its follower, polling
  its VM's socket with a 250 ms timeout, and its health monitor, even without a check),
  and every waiting `wait` or `logs -f`, and the listener, look at their clocks every
  250 ms (review findings 7.9, 7.16, 7.22). What do they cost, and which resource binds
  first: threads, the daemon's memory or CPU, or the VMs?
- **Method.** `docs/research/measurements/daemon-idle/run.py BIN alpine:3.22 0,10,100 30
  DIR`: running containers whose command is `sleep`, then over 30 s the daemon's CPU time
  (`ps` cputime), threads (`ps -M`) and RSS, and the VM processes' together; idle
  wakeups from `top`'s IDLEW. d84b1d3's signed binaries, 2026-10-03, the host of M84
  (128 GiB), load average 5 to 9 from other work.
- **Result.** 0 containers: daemon 0.000 s of CPU, 1 thread, 7.8 MiB. 10: 0.020 s, 25
  threads, 20.2 MiB; VMs (12 processes, 2 of them warm) 262 MiB. 100: 0.120 s, 205
  threads, 25.6 MiB; VMs (102) 1,975 MiB. IDLEW counts only wakeups out of an idle CPU,
  which a loaded host rarely is: 59 per 30 s at both 10 and 100, against the 12,000 the
  ticks alone make at 100. Not measured: 1,000 containers, about 19 GiB of VM RSS on a
  shared host short of free memory.
- **What a VM holds of its own.** RSS counts each VM's share of its template's pages
  again in every process. `footprint -p` of 6 of 10 running alpine VMs, the same day:
  7.5 to 7.8 MB of physical footprint each against 19 MiB of RSS (the first, which
  saved the template, 50 MB).
- **Consequence.** An idle running container costs the daemon about 40 us of CPU a second
  (0.4 % of a core per 100), growing with the ticks, two threads and 0.18 MiB; its VM
  about 7.5 MB. So the daemon's threads bind first: at two a run, macOS's 16,384 a
  process (`kern.num_taskthreads`) stop runs near 8,000, about 60 GB of VMs, on a host of
  128 GB. Following runs needs no thread of its own per run.

### M90. What a daemon and its runs cost while they idle, once it waits on events

- **Question.** With the daemon waiting on events (4a1bbce: one loop follows every run,
  one scheduler checks health, the listener sleeps until something happens), what does an
  idle running container still cost it, and what binds first now?
- **Method.** M89's harness and arguments, `daemon-idle/run.py BIN alpine:3.22 0,10,100
  30 DIR`, with 4a1bbce's signed binaries, 2026-10-03, the host of M84 (128 GiB), load
  average 6 to 7 from other work.
- **Result.** 0 containers: daemon 0.000 s of CPU, 2 threads (the listener and the
  completer of `--rm` removals), 8.0 MiB. 10: 0.000 s, 17 threads, 19.9 MiB; VMs (12
  processes, 2 of them warm) 262 MiB. 100: 0.000 s, 107 threads, 22.2 MiB; VMs (102)
  1,983 MiB. `ps` counts CPU in hundredths of a second: under 10 ms in 30 s at 100, where
  M89 saw 120 ms. No idle wakeups of the daemon in either window, where M89 saw 59.
- **Consequence.** An idle running container costs the daemon no CPU `ps` can see, 0.14
  MiB, and one thread: its VM's watcher, which waits for the VM process to end
  (`waitpid`). At one a VM, macOS's 16,384 threads a process stop VMs near 16,000, about
  120 GB of them at 7.5 MB each (M89): threads and a 128 GB host's memory now bind at
  about the same number, and a larger host's memory later. Waiting for every VM's end at
  once (kqueue `EVFILT_PROC`, Linux's pidfd) needs no thread per VM.

### M91. Warm VMs started outside the pools' lock

- **Question.** A claim that found no VM ready started its own under the pools' lock, and
  so did every refill (review 7.8): a VM's start spawns its process and its network
  process. How long did that hold the lock, which every other claim, refill and the
  listener's look at the pools waits for, and does starting VMs outside it change a
  burst's runs?
- **Method.** Two builds: 166ef50, and the change (`daemon/refill.rs`: planned and counted
  as starting under the lock, started outside it, refills on one thread). Each arm a home
  with `alpine:3.22` pulled and the same guest recorded (`shards guest use`, one kernel
  and one shards-init for both), each restoring its own template: a template names its
  home's root filesystem, and a VM is granted only its own home's. `build-ab/ab.py` with
  `AB_BURST=8`: 8 runs of `true` at once per turn, the arms' turns alternating, the old
  arm's `shards-vm` signed with an identifier of its own (`dev.shards.vm.ab-old`), since
  launches on the runs' path otherwise pay M72's switch. First with no warm pool
  (`SHARDS_POOL=0`, every claim starts its VM), 80 turns per arm; then with the default
  pool (2), 40 turns, each build probed (not kept) to log how long each claim's and
  refill's planning or starting held the lock. 2026-10-03, the host of M84, load average
  3 to 10 from other work.
- **Results.** The lock held, per claim or refill, with the default pool:

| Held by | n | p50 | p90 | p99 | max | total |
|---|---|---|---|---|---|---|
| 166ef50, claims (VMs started under it) | 843 | 39 µs | 484 µs | 695 µs | 853 µs | 123.8 ms |
| 166ef50, refills | 352 | 44 µs | 579 µs | 871 µs | 1,277 µs | 76.4 ms |
| The change, claims (planning only) | 1,004 | 28 µs | 54 µs | 111 µs | 163 µs | 33.4 ms |
| The change, refills | 338 | 34 µs | 87 µs | 130 µs | 169 µs | 14.3 ms |

  A run's wall clock did not move. No pool, 640 runs per arm: p50 26.3 ms against 26.7,
  each burst's median +0.8 ms (95% [−0.3, +3.3]), its slowest +0.2 ms ([−2.0, +2.2]).
  Default pool, 320 runs per arm: the burst's median −0.2 ms ([−0.5, +0.9]), its slowest
  +0.2 ms ([−1.8, +2.7]). The slow runs are a burst's last, its restores at once on a
  loaded host, and their tails changed sides from one comparison to the next with the
  host's load: a burst's 8 restores outweigh its spawns.
- **Consequence.** Starting VMs outside the lock leaves a burst's runs as they were, and
  takes what else waits on the pools from up to 1.3 ms to under 0.2 ms: another pool's
  claim, a refill, the listener's next duty. What remains under the lock is mostly the
  template's look-up (`snapshot::exists`, a file read).

### M92. What a run costs the daemon once VM processes are followed by its loop

- **Question.** With every VM process's end, and its network process's, followed by the
  followers' loop (5bbb111) rather than a thread each, what does an idle running
  container still cost the daemon?
- **Method.** M89's harness and arguments, `daemon-idle/run.py BIN alpine:3.22 0,10,100
  30 DIR`, with 5bbb111's signed binaries, 2026-10-03, the host of M84.
- **Result.** 0 containers: daemon 0.000 s of CPU, 2 threads, 7.9 MiB. 10: 0.000 s, 6
  threads, 19.6 MiB; VMs (12 processes) 262 MiB. 100: 0.000 s, 6 threads, 20.5 MiB; VMs
  (101) 1,973 MiB. No idle wakeups of the daemon in either window.
- **Consequence.** The daemon's threads no longer grow with its runs: 6 at 10 and at 100,
  against 107 at 100 in M90 and 205 in M89. With 3 running, `sample` names them: the main
  thread (the listener), the completer, the recorder, the followers, the refiller, and a
  parked libdispatch worker (`start_wqthread`, `__workq_kernreturn`), the system's, kept
  once a framework the daemon calls has used a dispatch queue. An idle run costs it
  about 10 KiB and no CPU `ps` can see. VMs are bound by the host's memory and the
  daemon's descriptors, no longer by its threads.

### M93. Containers' records written outside the registry's lock

- **Question.** A run's start and end wrote its container's record (a file written and
  renamed into place) under the lock the registry is kept under, on the followers' loop
  (review 7.7). How long did that hold the lock, which every other run's container
  waits for, and every command?
- **Method.** 1c40148 against the change (`daemon/record.rs`: the change kept at once, the
  record written by the recorder thread outside the lock, a command answered once every
  record changed before it is written, audit A15), each built with a probe (not kept)
  logging how long a run's start and its end held the lock. M91's arms and harness:
  `build-ab/ab.py`, `AB_BURST=8`, the default pool, 40 turns per arm, the old arm's
  `shards-vm` with a sandbox identity of its own. 2026-10-03, the host of M84, load
  average 7 to 8 from other work.
- **Results.**

| Held by | n | p50 | p90 | p99 | max | total |
|---|---|---|---|---|---|---|
| 1c40148, a run's start (its record written) | 352 | 457 µs | 108.3 ms | 192.2 ms | 215.9 ms | 13.6 s |
| 1c40148, a run's end | 352 | 31.7 ms | 113.8 ms | 201.8 ms | 371.3 ms | 16.3 s |
| The change, a run's start | 352 | 0 µs | 1 µs | 5 µs | 25 µs | 0.3 ms |
| The change, a run's end | 352 | 3 µs | 13 µs | 46 µs | 92 µs | 2.1 ms |

  At this load a record's write and rename waited tens to hundreds of milliseconds behind
  other processes' I/O (M46: milliseconds at a busy host's p90), and every other
  run's making of its container, every arrival and every command waited for it under the
  lock. The runs' wall clock, 320 per arm: p50 45.0 ms against 44.1, p90 147.5 against
  110.1, p99 299.5 against 137.3, max 315.9 against 156.4; paired, each burst's median
  −0.3 ms (95% [−3.1, +1.3]), its slowest −3.6 ms ([−8.3, +0.6]).
- **Consequence.** The lock is held microseconds for a run's start and end, where a
  record's write held it up to 0.37 s; a burst's slowest runs, which made their
  containers behind the others' writes, did not get slower, and their tail was lower
  in this comparison, which a load this high moves (M91).

### M94. Collections off the thread that accepts clients

- **Question.** A collection ran on the daemon's listener (review 7.14): how long did its
  clients wait while one ran?
- **Method.** `docs/research/measurements/collect-listener/run.py OLD NEW 20000 4 DIR`:
  a828707 against the change (`collect_all`: the collector's thread, waiting for the
  store's lease), alternating, each round 20,000 files left in the store's `ingest/` and a
  collection marked due as a pull marks it, then `shards ps` one after another until the
  files are gone. 2026-10-03, the host of M84, load average 2.6 to 3.3.
- **Result.** a828707: one `ps` a round, each waiting out the collection: 4 answers, p50
  2.02 s, max 4.46 s; its collections took 1.24 to 4.46 s. The change: 1,956 answers
  while its collections ran, p50 2.44 ms, p99 3.11 ms, max 3.85 ms; its collections took
  1.19 to 2.08 s.
- **Consequence.** Every client of a daemon waited for as long as a collection took,
  seconds for a store with that much to remove; on a thread of its own, a collection
  holds up no client.

### M95. A log of short lines, read on

- **Question.** `shards logs` read each record alone, its index entry, its head, a `stat`
  of its log, then its output, and sent the client each line in a message of its own
  (review 7.10). What did that cost a log of short lines, as a command writing a line at
  a time leaves it?
- **Method.** `docs/research/measurements/log-read/read.py alpine:3.22 1000000 6 OLD NEW`:
  a container's log and index made of 1,000,000 records of one 15-byte line each, one in
  ten on stderr, then `shards logs` six times in each arm, alternating, its output
  drained and counted, and the daemon's CPU time around each. 6834269 against the change
  (`Reader::read_segment` batched, `Show` in `commands.rs`), 2026-10-03, the host of M84,
  load average 2.9 to 3.5.
- **Result.** 6834269: p50 1,695.7 ms, max 1,705.1 ms; the daemon's CPU 1.68 s a read.
  The change: p50 292.5 ms, max 317.4 ms; the daemon's CPU 0.21 s. A variant sending a
  line whole in what is read from there, without holding it first, measured no
  different in a second comparison (p50 316.1 ms, the daemon's CPU 0.21 s, against
  1,710.9 ms and 1.70 s), and is not kept.
- **Consequence.** A log of short lines is read 5.8 times faster, the daemon spending an
  eighth of the CPU it did.

### M96. A run's end on the followers' loop, waiting on no write

- **Question.** The followers' loop takes every run's messages (M90). At a run's end it
  waited for the container's first record to be written, when the run ended first, and
  renamed a `--rm` container's directory aside under the registry's lock. How long did a
  run's end hold the loop, behind which every other run's messages wait, and what does
  keeping the end in memory change, the removal set aside by the completer?
- **Method.** First a probe (not kept) of the loop under
  `published_ports_reach_the_guest`, four at once, three
  rounds, at 65e940a's code: each run's end, its wait for the record, and the rename,
  2026-10-03, load average 6 to 7 from other work. Then 65e940a against the change
  (c2f9b6a, at c104b6f), each built with a probe (not kept) timing each batch of a run's
  messages the loop takes, `build-ab/ab.py` with `AB_FLAGS=--rm` and `AB_BURST=8`, 40
  turns per arm, three passes, the old arm's `shards-vm` with a sandbox identity of its
  own; the same day, the host of M84, load average 9 to 23.
- **Results.** Under the E2E test, 65e940a's loop:

| What | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| A run's end | 176 | 270 µs | 80.9 ms | 121.3 ms | 126.4 ms |
| Its wait for the record, where it waited | 48 | 59.3 ms | 83.0 ms | 112.5 ms | 112.5 ms |
| The rename, under the registry's lock | 179 | 389 µs | 14.0 ms | 33.9 ms | 52.9 ms |

  The A/B, a run's end on the loop, 352 a pass:

| Pass | Build | p50 | p90 | p99 | max | total |
|---|---|---|---|---|---|---|
| 1 | 65e940a | 252 µs | 4.4 ms | 12.7 ms | 15.4 ms | 490 ms |
| 1 | The change | 13 µs | 32 µs | 115 µs | 148 µs | 6.4 ms |
| 2 | 65e940a | 255 µs | 2.6 ms | 5.6 ms | 9.0 ms | 237 ms |
| 2 | The change | 12 µs | 22 µs | 55 µs | 175 µs | 5.1 ms |
| 3 | 65e940a | 219 µs | 2.3 ms | 5.0 ms | 8.1 ms | 230 ms |
| 3 | The change | 12 µs | 28 µs | 71 µs | 94 µs | 5.4 ms |

  Other batches took the same in both (p50 5 to 6 µs). The runs' wall clock, 320 per arm
  a pass, paired per burst: pass 1, each burst's median −0.1 ms (95% [−1.2, +0.5]), its
  slowest −0.6 ms ([−3.9, +1.2]); pass 2, −1.1 ms ([−1.6, +0.2]) and −1.3 ms ([−3.1,
  −0.5]); pass 3, −1.2 ms ([−2.0, −0.2]) and −1.1 ms ([−2.8, +0.4]). The change's p99 and
  max in pass 1, 78.6 and 89.5 ms against 37.9 and 42.2, are one burst whose runs took 77
  to 90 ms together, turn 27 of 40; no run of either arm took over 45 ms in the 80 turns
  of passes 2 and 3, the change's p99 30.6 and 28.1 ms against 30.8 and 29.7.
- **Consequence.** A run's end holds the loop tens of microseconds at p90, where it held
  it milliseconds (2.3 to 4.4 ms in the A/B, 81 ms under the E2E test), and every other
  run's messages with it. A burst's median run finished 1.1 to 1.2 ms sooner in two
  passes of three, and no later in the third.

### M97. A run's resolvers, read as it starts

- **Question.** Each run reads the host's `/etc/resolv.conf` and makes it over for the
  container (`build::step::resolv`), as dockerd does at every start (review 7.22). Would
  keeping what was made, and only looking at whether the file changed, be worth it?
- **Method.** `build/step.rs` `resolvers_cost` (ignored with the tests): 10,000 reads and
  makings over, against 10,000 `stat`s of the file, three times. 6834269 plus the review
  fixes in progress, 2026-10-03, the host of M84, load average 20 to 25 from other work.
- **Results.**

| Each | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| Read and made over | 3 × 10,000 | 11.4 to 17.0 µs | 14.6 to 22.8 µs | 31.4 to 52.8 µs | 0.17 to 1.23 ms |
| `stat` | 3 × 10,000 | 1.5 to 1.9 µs | 1.8 to 2.3 µs | 3.0 to 3.5 µs | 20 to 109 µs |

- **Consequence.** Kept as it is. A cache would save about 11 µs at the median, 0.04% of
  a pooled run's 26.3 ms (M91), and looking at the file's times could miss a change: Linux stamps a ctime
  from its coarse clock, a tick of 1 to 10 ms by `CONFIG_HZ` (fs/inode.c,
  `inode_set_ctime_current`), so two writes of the same size within one tick look like
  none. Since 6.13 (4e40eff0b573, multigrain timestamps) ext4, btrfs and xfs stamp a
  finer one once the ctime has been looked at, but other filesystems and older kernels
  do not. Missing none would take watching the file and those it names,
  systemd-resolved's included.

### M98. A run's files, made off the followers' loop

- **Question.** The followers' loop made each log segment a run's VM asked for (two files
  made, the oldest two removed) and wrote a template's working set (the template read for
  the set's bound at its first part, the set written and synced at its last). How long
  did each hold the loop, behind which every other run's messages wait, and what does
  making them on a thread of their own change (7460e93, ca4caa0)?
- **Method.** A probe (not kept) timing each `LOG_SEGMENT` and `WORKING_SET` part the loop
  took, and each batch of a run's messages. First 48d6358 alone: 10 rounds of four
  `shards run --rm alpine:3.22 sh -c 'head -c 104857600 /dev/zero'` at once, 100 MiB of
  output each, five 20 MiB segments; load average about 25 from other work. Then 48d6358
  against the change, both probed: `build-ab/ab.py` with `AB_FLAGS=--rm` and
  `AB_BURST=4`, 20 turns per arm of that command, load average 17 to 31; and
  `run-files/working_sets.py alpine:3.22 10`, a template saved anew each turn, its
  daemon stopped before the next, four passes, the first at load average 80, the
  others 20 to 24. Last, the change's daemon under M89's `daemon-idle/run.py` (0, 10
  and 100 containers, 30 s), its threads named by `sample` with 3 running.
  2026-10-03, the host of M84.
- **Results.** 48d6358 alone, a segment on the loop: n = 200, p50 238 µs, p90 10.9 ms, p99
  24.3 ms, max 25.0 ms; 520 ms of the loop in 7 s of runs. The A/B:

| What, on the loop | Build | n | p50 | p90 | p99 | max | total |
|---|---|---|---|---|---|---|---|
| A segment | 48d6358 | 480 | 214 µs | 843 µs | 6.2 ms | 13.8 ms | 281 ms |
| A segment | The change | 480 | 2 µs | 4 µs | 15 µs | 39 µs | 1.4 ms |
| A working set, pass 2 | 48d6358 | 10 | 9.4 ms | 25.0 ms | | 25.0 ms | 111 ms |
| A working set, pass 2 | The change | 10 | 9 µs | 22 µs | | 22 µs | 0.1 ms |

  The change's batches holding a segment took p50 16 µs and p90 45 µs, but 24 of 480
  took 2.9 to 30 ms: the probe's own write of the segment's time, inside the batch, to a
  file on a disk taking 400 MB of logs a turn; batches without one took p99 18 µs
  (48d6358) and 69 µs (the change). The chatty runs' wall clock: p50 398.6 ms against
  398.0, p90 1,559.9 ms against 579.1, max 3,009.7 against 913.0, from three slow turns
  of 48d6358's and one of the change's, the last turn slow in both; paired, each burst's
  median −4.5 ms (95% [−10.1, +11.5]), its slowest −6.0 ms ([−21.0, +10.4]).
  Working sets, pass 1, at load 80: 9.6 ms at the median, 32.9 ms the most, on 48d6358's
  loop, against 10 and 23 µs; but the change, before ca4caa0, wrote 7 sets of 10: three
  were queued on the files' thread as `daemon stop` came, and lost. With ca4caa0, 10 of
  10 in each pass. The runs that saved them: pass 2, p50 225 ms against 195 ms, max 508
  against 1,120 ms, one run; passes 3 and 4, p50 173 and 173 ms against 174 and 170, max
  230 and 192 against 184 and 185.
  The change's daemon idle: no containers, 4 threads, 8.1 MiB; 10, 8 threads, 19.7 MiB,
  VMs (12 processes) 263 MiB; 100, 8 threads, 20.6 MiB, VMs (102) 1,992 MiB; no CPU and
  no idle wakeups in either window. `sample` names them: the main thread (the
  listener), the completer, the recorder, the collector, the followers, the refiller,
  the files' thread, and the system's dispatch worker (M92).
- **Consequence.** A run's files hold the loop microseconds, where a segment held it 10.9
  ms at a loaded host's p90 and a working set 25 to 33 ms, and every other run's
  messages with it; the runs themselves are no slower. The daemon waits for its files as
  it exits, so a template's working set is not lost to a stop; and it keeps 8 threads
  with runs, M92's 6 and the collector's (7.14) and the files'. The client cap counts
  all but the clients', the dispatch worker among them: 8,187 of 16,384.

### M99. Looking for a free descriptor before an accept

- **Question.** On macOS an accept that finds no descriptor drops its client (XNU
  `accept_nocancel`). The daemon looked for a free one (`fcntl(F_DUPFD_CLOEXEC)`, then
  `close` of the copy) only once starved, and its first accept after room came took the
  last descriptor, so the next dropped a second client: `a_daemon_out_of_descriptors_
  waits_for_room` failed 1 run in 5 and once in a full suite. What does looking before
  every accept cost?
- **Method.** `descriptor-check/dupcost.c`: the look, on a Unix socket, 100,000 times,
  each timed with `clock_gettime_nsec_np(CLOCK_UPTIME_RAW)`, three runs. 2026-10-03, the
  host of M84, load average 5 to 9.
- **Result.** p50 250 ns, p90 250 ns, p99 250 to 292 ns, max 2.6 to 15.5 µs (the thread
  preempted).
- **Consequence.** Looked for before every accept on macOS: 250 ns a client, 0.005% of a
  5 ms run, and no client is dropped (the test, which allowed one, now allows none:
  15 runs of 15). Linux, which leaves the client queued, looks only once starved.

### M100. A context file taken whole while it is written

- **Question.** `shards build` takes each context file as one version (`host::Stage`): it
  read the file's metadata, took its bytes, and refused the file if the metadata had
  changed, as GNU tar's "file changed as we read it". CI's aarch64 Linux runner accepted a
  snapshot that mixed two versions (`a_file_written_while_it_is_taken_is_one_version_or_
  refused`). How often do reads and writes interleave, how often does that comparison see
  it, and what makes a take one version on Linux and macOS?
- **Method.** `context-snapshots/torn.rs`: a writer rewrites a file, each version one
  write(2) of one byte value; a reader takes it as the stage did (fstat, read or clone,
  fstat) and counts the snapshots that mix versions, those the comparison misses, and on
  macOS those the file's write count (getattrlist `ATTR_CMN_GEN_COUNT`) misses. Then the
  stage itself, `crates/build/tests/context.rs` `snapshots_at_length`: 64 KiB packed, 4 and
  64 MiB cloned, a writer holding the file open or opening it for each version, every
  snapshot taken checked whole; and `a_take_costs`, a take of a 1 KiB file no one writes,
  f2d85b4 against the change, interleaved. macOS on the host of M84 (APFS), 2026-10-04;
  Linux in Docker Desktop's VM on it (kernel 6.12.76, before multigrain timestamps), on
  overlayfs, ext4 and tmpfs.
- **Results.** The comparison alone, Linux:

  | File system | Size | Takes | Torn | Missed |
  |---|---|---|---|---|
  | overlayfs | 64 KiB | 20,000 | 376 | 365 |
  | overlayfs | 4 MiB | 3,000 | 38 | 33 |
  | overlayfs | 64 MiB | 300 | 50 | 43 |
  | ext4 | 64 KiB | 20,000 | 744 | 735 |
  | ext4 | 4 MiB | 3,000 | 45 | 40 |
  | ext4 | 64 MiB | 300 | 41 | 26 |
  | tmpfs | 64 KiB | 20,000 | 95 | 86 |
  | tmpfs | 4 MiB | 3,000 | 2,230 | 2,230 |
  | tmpfs | 64 MiB | 300 | 222 | 222 |

  Linux stamps a write's times as it begins (fs/ext4/file.c, `file_modified` in
  `ext4_write_checks`) from its coarse clock (fs/inode.c, `current_time`), and holds the
  inode's lock until the write's last byte is in place: a write in the clock's tick of the
  last, or under way when the file is looked at, leaves its times as they were.

  APFS, read: 64 KiB 20,000 takes, 16,301 torn, 0 missed; 256 KiB, 512 KiB and 1 MiB,
  20,000 takes each and again with the writer pausing, 46,783 torn, 0 missed; 4 MiB,
  4,000 takes, 397 torn, 0 missed. 64 MiB, 6,000 takes and 4,000 with pauses: 24 and 27
  torn, the times missing 18 and 27, the write count 3 and 3. APFS stamps a write's times
  as it ends, which misses a write that spans the whole read; its write count, though it
  moves as a write goes on, misses some too. Cloned (fclonefileat): 64 MiB 6,000 takes
  and 4 MiB 10,000, under the same writers, 0 torn.

  The stage, changed as below: Linux, 5,000 takes a size (500 at 64 MiB) on each file
  system, both writers, none torn; macOS, 20,000 (2,000), none torn. Under a writer that
  never pauses most takes are refused, the file having no version to take: on Linux
  4,971 to 4,999 of 5,000, on macOS 17 to 18,685 of 20,000, cloned files taken most.

  A take of 1 KiB, n = 20,000, three runs each: macOS p50 8.2 to 8.4 µs before and 9.0 to
  9.1 µs with the write count, the change keeping the times alone; Linux ext4 p50 1.17 µs
  and p99 1.75 to 1.92 µs before, 1.54 µs and 2.79 to 2.96 µs after.
- **Consequence.** Linux takes a read lease where it is granted (fs/locks.c: while no one
  has the file open for writing), and one who opens the file for writing or truncates it
  waits until the take is done (fs/open.c, `break_lease`). Without one, the stage waits
  until the file's ctime is behind the coarse clock's tick, so a write begun since moves
  it, and before taking the bytes waits out a write under way with lseek(2) `SEEK_DATA`,
  which takes the inode's lock shared on ext4, Btrfs and tmpfs (XFS's reads take its own).
  macOS lets no process but Apple's take a lease (bsd/vfs/vfs_subr.c, `vnode_setlease`):
  files under 1 MiB are read, where the times saw every torn read, and larger ones cloned,
  which takes one version; the write count is not kept, as it saw nothing the times did
  not on the stage's paths. Writes through a shared mapping are not seen where no lease is
  granted.

### M101. Electing the default bridge's subnet

- **Question.** A guest on Docker's default bridge had 172.17.0.2/16 behind 172.17.0.1.
  A host on that network, as a CI job in a container on Docker's own bridge is (its address
  172.17.0.2), could not be reached from its guests: the guest dialled itself
  (`run_steps_reach_the_network_unless_it_is_none` failed on the x86_64 musl runner).
  dockerd elects its bridge's subnet as it starts; what does that cost here?
- **Method.** `crates/net/src/bridge.rs`, `an_election_costs` (n = 1000): the host's
  resolvers read as dockerd reads them, its on-link IPv4 routes dumped (rtnetlink on
  Linux, XNU's `NET_RT_DUMP` sysctl on macOS), and a subnet elected; and
  `this_hosts_on_link_routes_are_read`, which prints what it read and elected. macOS on
  the host of M100; Linux in Docker Desktop's VM on it (kernel 6.12.76), in a container on
  the default bridge. Revision 05efda6 with the change, 2026-10-04.
- **Results.** macOS: p50 17.5, p90 18.8, p99 23.1, max 67.8 µs; this host's on-link
  routes left 172.17.0.0/16 free, and it was elected. Linux: p50 6.4, p90 7.5, p99 12.2,
  max 39.5 µs; the container read one on-link route, 172.17.0.0/16 (its eth0's; the
  default, via a gateway, is not one), and 172.18.0.0/16 was elected, as dockerd in such a
  container elects it.
- **Consequence.** The daemon elects once as it starts, and a build as it starts its
  builder: tens of microseconds, once (architecture.md D31).

### M102. What a VM in App Sandbox can give up of what it was granted

- **Question.** A VM that saves a template goes on to serve a run (review 8.2), and every
  later run of the image restores what it saved: before the run's command reaches its
  guest, the VM must lose its way to the template. On Linux a second Landlock layer takes
  the directory away (its rules hold inodes, so a rename alone does not). Can a process in
  App Sandbox give up a directory granted by bookmark, and does the grant follow the
  directory when its parent renames it?
- **Method.** `docs/research/measurements/app-sandbox/run.sh`, which builds and signs the
  probe as M70 did: `relinquish` resolves a directory bookmark, starts its scope, makes a
  file, stops it (`CFURLStopAccessingSecurityScopedResource`), and makes and reads a file
  again, under `$HOME` and under the per-user temporary directory, where a test's
  `SHARDS_HOME` lives; `renamed.py` has the probe make a file in a granted directory, then
  renames the directory from the unsandboxed parent and has the probe make and rewrite
  files at the new path and remake the old. macOS 26.4.1, Apple M5 Max, 2026-10-04.
- **Results.** After the stop, in both places: make ok, read ok. Nothing is given up. With
  no grant, the temporary directory refuses a file (EPERM), as `$HOME` does. Renamed: a
  file at the new path refused, a file there rewritten refused (EPERM); the old path made
  again, ok. The grant names the path, not the directory.
- **Consequence.** On macOS a VM's grants last its life, so the template must leave the
  path it was granted before a run's command reaches the guest: the daemon moves each
  template into place (run.rs, `settle`) before it hands over the run, and the VM checks
  that it is gone from that path before it takes a run, or takes none and the daemon gives
  the run to another VM (vm_run.rs, `moved`; tests/warm.rs,
  `a_template_saver_takes_a_run_only_out_of_the_templates_reach`, mutation-checked). On
  Linux the VM applies the layer and checks that a file made there is refused (vm_run.rs,
  `seal`; confine.rs, `a_second_layer_seals_a_directory_and_keeps_the_rest`).

### M103. A run's output, one read and one write a frame

- **Question.** The host read each frame of a run's output with two reads, its header
  and its payload, and wrote each to stdout through std's line buffering, which made one
  write of a frame's lines and another of the rest (review 8.20). What do one buffered
  read of all the guest sends at once, and one write a frame, buy?
- **Method.** `docs/research/measurements/build-ab/ab.py`, 0ada525 against the change,
  each arm on a template of its own: `shards run --pull never alpine head -c 1048576
  /dev/zero`, n = 300 pairs. Apple M5 Max, macOS 26.4.1, load average 2.2–2.5,
  2026-10-04.
- **Results.** The paired median of new − old with its 95% interval: wall −309 µs [−414,
  −220]; in the guest −322 µs [−411, −219]; the rest −23 µs [−87, +40]. Both arms'
  1 MiB runs had a long tail: in the guest p50 5.9–6.4 ms, p90 27–35 ms, p99 455–462 ms,
  max 664–696 ms.
- **Consequence.** The host reads up to what the guest sends at once (`run::BUFFERED`,
  shared with the guest) and writes each frame whole. The tail is the same in both arms,
  and is the next question.

### M104. TCP through a published port, and what the guest made of the frames

- **Question.** Each segment for the guest was copied three times on its way, and a
  timeout sent the guest everything unacknowledged again (review 2.15, 2.21). What does
  sending each once, from where it lies, and repairing losses as they are seen, buy? And
  why did one connection in seven take 200 ms or more?
- **Method.** `docs/research/measurements/net-throughput`: `ab.py` runs a container per
  build whose port 7000 is published, its workload echoing each connection, and times
  connections that send 32 MiB while reading them back, both builds in turn, n = 300
  each, every byte checked; `counters.py` runs 1,000 such connections against one build
  and reads the guest kernel's TCP and interface counters before and after. e19fa61, the
  change, and e19fa61 with only the device's half of it (below). Apple M5 Max, macOS
  26.4.1, load average 2.0–2.3, 2026-10-04.
- **What the guest saw.** On e19fa61, 600 connections left the guest's `eth0` with 226
  `rx_length_errors`, all its `rx_dropped`: its virtio-net driver had found frames whose
  header names more buffers than the device had returned (`receive_mergeable`, "buffers
  out of N missing"). The device returned each of a frame's buffers to the used ring,
  and moved the ring's index, one at a time; virtio 1.2 §5.1.6.4.1 has it "use all
  buffers used by a single receive packet together, such that at least num_buffers are
  observed by driver as used". A guest polling the ring meanwhile drops the frame, and
  takes the rest of its buffers for frames of their own. Of 1,000 connections, 4 came
  back cut short (a prefix, then the end) and 184 took over 100 ms. With the device
  returning a frame's buffers together and nothing else changed: 0 length errors, 0 cut
  short, 21 over 100 ms.
- **What was left.** The guest also drops what its memory cannot take: `TCPRcvQDrop`
  (21 in 1,000 connections), after which Linux closes its window at once
  (`tcp_select_window`, `ICSK_ACK_NOMEM`) and drops what was in flight past its edge
  (`BeyondWindow`, 40), and `TCPOFODrop` (7), out-of-order segments it drops without an
  acknowledgement. e19fa61 repaired none of these until a timeout, 200 ms, then sent the
  whole window again. The change sends the oldest segment alone at a timeout (RFC 6298
  §5.4); repairs a segment at the first duplicate acknowledgement past `recover` (ring
  and guest keep the order segments were sent in, so a duplicate means a loss; RFC 6582
  §3.2's `recover` keeps a duplicate of a timeout's or a shrunk window's resend from
  counting) and each hole a partial acknowledgement shows; sends again what a window that
  shrank had the guest drop; and probes a window closed on bytes waiting (RFC 9293
  §3.8.6.1, RFC 1122 §4.2.2.17). 1,000 connections: 0 over 100 ms, 0 cut short, 0 length
  errors; the guest dropped more (`TCPRcvQDrop` 88, `BeyondWindow` 143, `TCPOFODrop` 31),
  every one repaired without a timeout.
- **Results.** Connection times, ms, n = 300 per arm:

  | Arms | p50 | p90 | p99 | max | new − old, paired median [95%] |
  |---|---|---|---|---|---|
  | e19fa61 | 12.0 | 217.3 | 420.8 | 434.6 | |
  | the change | 11.3 | 16.1 | 19.9 | 21.6 | −1.2 [−1.5, −1.0] |
  | e19fa61 with the device's half | 11.1 | 12.6 | 215.6 | 216.9 | |
  | the change | 10.4 | 12.1 | 15.2 | 17.5 | −0.8 [−1.0, −0.6] |

- **Consequence.** The device returns a frame's buffers together and allocates nothing a
  frame (review 2.22); a segment's bytes go from the connection's queue to the ring in one
  copy, and the guest's straight to the host's socket while nothing waits before them.
  Two repairs still wait for a timeout: a segment sent again and lost again, and the
  holes after a timeout's resend. The guest's duplicate acknowledgements cannot say which
  segment they are for; SACK (RFC 2018) can, and with each segment's place in the order
  sent, would show both at once. The guest drops more for want of memory the faster it is
  sent to, which is the guest's to prune and collapse: what that costs it is the next
  question.

### M105. The frame ring as production waits on it

- **Question.** M83's ring made a round trip in 0.8 µs with a receiver that spins 2,000
  times before it sleeps; the device and the network process never spin, but ask to be
  rung and sleep on their doorbell as soon as their ring is empty (review 2.17). What do
  they get?
- **Method.** `shards_netring`'s own `frames_cost` (an ignored test): two threads for the
  two processes, each with the producer and consumer the device or the network process
  makes, sleeping on its own doorbell as they do (`arm`, then poll(2) on the pipe).
  20,000 round trips of a 64-byte frame, then 1 GiB one way in frames of 1514, 9014 and
  65,561 bytes. Three runs. Apple M5 Max, macOS 26.4.1, load average 2.0–2.4,
  2026-10-04.
- **Results.** Round trip p50 6.6 µs, p90 8.1–8.8, p99 11.2–12.5, max 52.5–61.0;
  one way 123.5–124.2, 194.9–223.2 and 246.3–250.5 Gbit/s.
- **Consequence.** Without the spin a round trip costs a doorbell's wake each way: 6.6
  µs, not M83's 0.8, still a third of the datagram socket's 22 µs, and large frames move
  at two thirds of M83's rate. The spin would buy 6 µs a round trip with a core's time
  each time a ring empties, in every VM, idle or not; production keeps none. D31 says
  so.

### M106. A round trip beside the connections a network process holds

- **Question.** The network process rebuilt its whole set of descriptors to poll, and
  went through every connection for its timer and to find those a full ring held back,
  on every pass of its loop (review 2.14). What does a connection's round trip cost as
  the connections beside it grow, and what is left of it when only what is ready costs?
- **Method.** `docs/research/measurements/net-throughput/idle.py`: a container whose
  `hold` workload (crates/testguest) holds every connection it is given, on epoll, so
  that those it holds cost the guest nothing an event; N connections through its
  published port, each through to the guest and then idle, beside one that sends 64
  bytes and waits for them back, 2,000 times. 1adde7d against the change; then
  `ab.py`, 32 MiB each way, n = 200 each, for what bulk transfer costs. Apple M5 Max,
  macOS 26.4.1, load average 1.9–2.4, 2026-10-04.
- **Results.** Round trip, µs:

  | N idle | 1adde7d p50 / p90 / p99 / max | the change p50 / p90 / p99 / max |
  |---|---|---|
  | 0 | 57 / 87 / 113 / 969 | 54 / 86 / 99 / 633 |
  | 250 | 98 / 118 / 206 / 567 | 53 / 61 / 71 / 176 |
  | 1,000 | 181 / 197 / 380 / 540 | 53 / 60 / 70 / 317 |
  | 3,500 | 802 / 916 / 1,418 / 2,396 | 54 / 62 / 70 / 877 |

  32 MiB each way: p50 9.9 against 10.0 ms, p99 14.5 against 14.1; paired median +0.1
  ms [−0.2, +0.2].
- **Consequence.** Each descriptor is registered once with the kernel's own event queue
  (epoll on Linux, kqueue on macOS), its interest changed only when what it waits for
  does; timers wait in a heap, a connection's armed once for its deadline; the
  connections a full ring held back wait in a queue of their own. A round trip costs
  the same beside 3,500 idle connections as beside none, and bulk transfer what it did.

### M107. What a seccomp filter's layout costs the syscalls it checks

- **Question.** shards-vm's filter compared a syscall's number with each rule in turn,
  and an argument with each of its values (review 1.7): ioctl, the call of every vCPU
  run, came after 84 rules, and KVM_RUN after 11 requests. Since Linux 5.11 the kernel
  skips a filter for a syscall it allows whatever the arguments (kernel/seccomp.c,
  `seccomp_cache_check_allow`), so what is left to cost is the syscalls it checks the
  arguments of, ioctl first, and the install, which runs every syscall's number through
  the filter for that cache (`seccomp_cache_prepare_bitmap`). What does each layout cost
  them, against a binary search?
- **Method.** `docs/research/measurements/seccomp-search` (`run.sh 200 7a0d8a0`):
  shards-vm's rules as confine.rs has them, less the 11 syscalls aarch64 lacks, compiled
  by the pre-7a0d8a0 layout (frozen in the harness) and by shards' `compile` at 7a0d8a0.
  Each variant in a process of its own, installed as shards-vm installs it, the
  variants in turns, round by round, in alternating order, 200 rounds: no filter;
  `allow`, a filter of one instruction that allows everything; `linear`; `search`. In
  each process, 500 samples of 64 `ioctl(/dev/null, KVM_RUN)` calls (ENOTTY, after the
  filter allows it) and of 64 `getpid` calls, as ns a call; the install timed once a
  process; each compile 2,000 times in turns. Instructions run counted by running both
  programs. Linux 6.12.76-linuxkit, aarch64, 18 CPUs, `bpf_jit_enable` 1: Docker
  Desktop's VM on an Apple M5 Max, macOS 26.4.1, load average 2.6–4.4, 2026-10-04.
- **Results.**
  - Instructions: KVM_RUN's ioctl ran 157 under `linear` and 16 under `search`; the
    most any allowed syscall ran, 192 and 18. The programs: 310 and 376 instructions.
  - A call, ns, n = 100,000 each:

    | | none | allow | linear | search |
    |---|---|---|---|---|
    | ioctl(KVM_RUN), p50 / p90 / p99 / max | 123.7 / 132.8 / 179.0 / 1,090 | 123.7 / 133.5 / 179.0 / 2,747 | 140.0 / 150.4 / 196.6 / 1,272 | 132.2 / 141.9 / 188.1 / 1,665 |
    | getpid, p50 / p90 / p99 / max | 113.3 / 121.1 / 165.4 / 1,839 | 113.3 / 121.8 / 165.4 / 2,349 | 113.3 / 121.7 / 164.7 / 786 | 113.3 / 121.7 / 164.7 / 760 |

    The filter's cost to KVM_RUN, over none: 16.3 ns at p50 and 17.6 at p99 under
    `linear`, 8.5 and 9.1 under `search`. `getpid`, which the cache lets skip each
    filter, cost the same under all four; the maxima of 1–3 µs came under no filter as
    under each.
  - The install, µs, n = 200 each: `allow` 138.5 / 170.8 / 400.0 / 499.4 (p50 / p90 /
    p99 / max), `linear` 208.7 / 241.8 / 455.9 / 495.9, `search` 156.0 / 184.8 / 421.1 /
    454.1. Compile: p50 3.2 µs `linear`, 1.7 `search`.
- **Consequence.** Filters find a syscall's rule, and an argument's value, by a binary
  search (7a0d8a0). Under shards-vm's rules a vCPU's run pays the filter about half what
  it did, and a VM process's install 53 µs less at p50, its layout's share 17.5 µs where
  it was 70. Most of the install is what any filter's costs on this host, 138.5 µs at
  p50 for one instruction; x86_64, below, is another matter.
- **x86_64, what an install is made of (review 1.22).** The harness grew variants
  (cbfef16): `allow-bare`, one instruction without the LOG and TSYNC flags;
  `allow-again`, one installed a second time in one process; no_new_privs timed apart
  from seccomp(2); and `trace.sh`, ftrace's function_graph over seccomp(2), as root. A
  CI workflow runs it (`.github/workflows/seccomp-search.yml`), 200 rounds, on GitHub's
  ubuntu-24.04 runners: Linux 6.17.0-1022-azure, 4 CPUs, `bpf_jit_enable` 1, all 88 of
  shards-vm's rules. At cbfef16, an Intel Xeon Platinum 8370C, load 0.2:
  - KVM_RUN's ioctl ran 179 instructions under `linear` (343 in all), 16 under
    `search` (405); a call, ns at p50, none 145.0, `allow` 163.4, `linear` 228.4,
    `search` 192.2. Any filter costs a syscall about 18–23 ns, `getpid` too (115.0
    against 137.5–137.7).
  - The install at p50: `allow` 21.3 µs, `allow-bare` 18.0, `allow-again` 9.4,
    no_new_privs 0.9; `linear` 274.1, `search` 107.3. What any install costs here is
    small, and what a program costs is its own.
  - By function (ftrace, a run of each): `bpf_prepare_filter`, the classic program's
    check, conversion to eBPF and JIT, 13.7–17.2 µs for one instruction, 99 for
    `linear`'s 343, 127–129 for `search`'s 405: about 14 µs and a quarter of a
    microsecond an instruction. `seccomp_cache_prepare_bitmap`, which runs every
    syscall's number through the filter, 199 µs under `linear`, 16.3–16.5 under
    `search`. `bpf_prog_alloc` 10–14 µs under each.
- **x86_64, comparisons pooled (f0a6419).** Each comparison now jumps straight to its
  target in a pool after the comparisons, one instruction a syscall where it took two
  (seccomp.rs). The harness keeps 7a0d8a0's layout frozen as `searched`, and times each
  compile as its process's first, as a VM process's own is. An AMD EPYC 7763, load 1.3,
  200 rounds:

  | | `linear` | `searched` (7a0d8a0) | `search` (pooled) |
  |---|---|---|---|
  | instructions; KVM_RUN's path; the longest | 343; 179; 214 | 405; 16; 18 | 224; 16; 18 |
  | ioctl(KVM_RUN), ns, p50 / p99 | 390.4 / 576.4 | 350.0 / 521.8 | 349.6 / 520.5 |
  | compile, a process's first, µs, p50 / p99 | 16.1 / 24.5 | 17.4 / 50.4 | 25.9 / 60.3 |
  | install, µs, p50 / p90 / p99 / max | 293.5 / 318.8 / 352.9 / 357.6 | 154.1 / 177.3 / 214.5 / 221.6 | 110.3 / 130.9 / 165.7 / 186.2 |

  No filter: ioctl 262.7 ns, `allow` 312.8; `getpid` 255.5–255.6 under each filter,
  210.4 under none. A compile and its install, the start's part, 171.5 µs at p50 under
  `searched` and 136.2 under `search`. One install of `allow` took 140.6 ms, its
  seccomp(2) all of it, among 1,400 installs in a run on a runner up under a minute:
  not yet explained.
- **Consequence (x86_64).** Comparisons jump to a pool of their targets (f0a6419): a VM
  process confines itself 35 µs sooner than with 7a0d8a0's search, and 173 µs sooner
  than with the linear layout, its filter as cheap to its syscalls.

### M108. What a build's snapshots hold, step by step

- **Question.** A build keeps every step's snapshot until its export, and each step
  began with a whole copy of the tree before it (review 4.2): every node, entry, name
  and index slot. What does that cost a step, on images as they are?
- **Method.** `docs/research/measurements/build-steps` (`cargo run --release -- STEPS
  LAYER.tar...`): an image's layers applied as a base image's are, then STEPS steps as
  the build executor takes them (crates/shards/src/build/exec.rs): the input snapshot
  cloned, a file put in the clone, a step begun, and the links counted (`Fs::links`, as
  `write_layer` and COPY count them), every snapshot kept; each clone's and count's µs,
  and the heap the steps' snapshots hold, by a counting allocator. golang:1.26 (36,576
  entries) and python:3.13 (33,833), their layers as `docker save` gives them,
  decompressed. And `build-memory`'s apply and write of golang:1.26, the tree before
  and after, interleaved, 9 runs each. Apple M5 Max, macOS 26.4.1, 2026-10-04.
- **Results.**

  | | the base snapshot | a step's clone | a step's snapshot holds | 50 steps hold |
  |---|---|---|---|---|
  | golang:1.26, the tree copied | 5.8 MB | 190–223 µs at p50 | 6.84 MB | 341.9 MB |
  | golang:1.26, chunks shared | 3.7 MB | 0–2 µs | 0.22 MB | 10.9 MB |
  | python:3.13, the tree copied | 5.9 MB | 188–223 µs | 6.34 MB | 317.2 MB |
  | python:3.13, chunks shared | 3.4 MB | 0–6 µs | 0.17 MB | 8.5 MB |

  Counting links, a pass over the whole tree, 228–316 µs a step at p50, either way.
  Applying golang:1.26's layers took 128.4 ms at p50 before and 129.7 after; writing
  its image 61.2 and 61.1 ms.
- **Consequence.** A tree's nodes, entries and index slots are kept in chunks of 1,024,
  and its names in chunks of 64 KiB a name never straddles, each chunk shared by the
  trees cloned from one another until one changes it, which copies that chunk alone
  (erofs.rs, `Arena`, `Names`). A clone copies the chunks' handles, a step's snapshot
  holds what the step changed, and a build of 50 steps over golang holds 11 MB of
  snapshots where it held 342. Two snapshots compare only the chunks they do not share
  (`Tree::unshared_nodes`), as a step's layer is checked against the nodes it changed in
  place (stack.rs). Counting links stays a pass over the tree.

### M109. Building a root filesystem: layers decompressed together

- **Question.** Building an image's root filesystem (`Store::rootfs`) decompressed
  and hashed its layers one at a time with flate2's default backend (miniz_oxide), and
  a probe of golang:1.26's layers put decompressing and SHA-256 at 1.86 s of the work
  (review 9.1). A layer's decompression and hash do not depend on another's; only
  stacking them does, in order. What does unpacking them on the host's cores, and
  zlib-rs's inflate, save end to end?
- **Method.** `docs/research/measurements/rootfs-build` (`ab.sh OLD_REV ROUNDS
  LAYOUT...`): golang:1.26 and python:3.13 for linux/arm64 as `docker save` gives them,
  their layers the registry's gzip blobs, put in a store; then each image's root
  filesystem built by `Store::rootfs` (decompressed, checked against its DiffIDs,
  stacked, and the EROFS image written), the built one removed between rounds. 655212b
  (before) against the change (after), in turn, one build of each image a round, after
  a warm-up round each, 15 rounds. golang's layers: 146, 60, 196, 274 and 240 MB
  decompressed (two empty); python's: 146, 60, 196, 653, 19 and 74 MB. Apple M5 Max (6
  performance and 12 efficiency cores), macOS 26.4.1, 2026-10-04.
- **Results.** Milliseconds a build:

  | image | build | n | p50 | p90 | p99 | max |
  |---|---|---|---|---|---|---|
  | golang:1.26 | before | 15 | 2263 | 2671 | 2990 | 2990 |
  | golang:1.26 | after | 15 | 771 | 861 | 917 | 917 |
  | python:3.13 | before | 15 | 2797 | 3075 | 3118 | 3118 |
  | python:3.13 | after | 15 | 1480 | 1672 | 1680 | 1680 |

  The probe, decompressing and hashing golang's layers alone: 1.86 s one at a time with
  miniz_oxide, 1.39 s with zlib-rs, 0.58 s and 0.45 s at once.
- **Consequence.** Layers are unpacked on as many threads as the host has cores, up to
  one a layer, and stacked in order as each and those before it are done
  (`Store::unpack_in_order`); flate2 inflates with zlib-rs. An error is the one a
  sequential unpack meets first. python's build is now bounded by its 653 MB layer, one
  gzip stream decompressed on one core; decompressing one stream in parallel
  (rapidgzip, Knespel and Brunst, HPDC 2023) is the next step for such layers.

### M110. Applying a layer: data passed over by seeking

- **Question.** `layer::apply` reads a layer's archive twice, whiteouts first, and each
  pass skipped entries' data by reading it into `io::sink`: every byte of a layer went
  through memory twice more after it was unpacked, though the archive is a file it can
  seek in (review 9.1). What does seeking past the data instead save, idle and busy?
- **Method.** `build-memory`'s `apply-ab.py` (interleaved, 9 runs each): golang:1.26's
  seven layers decompressed and applied in order, and M78's archive of a million empty
  files, alone and over itself; 08c4078 (before) against the change (after). And
  `rootfs-build`'s `ab.sh` (M109) of the same pair with `BUSY=18`: 18 `yes` processes
  spinning on the 18 cores throughout, 11 rounds. Apple M5 Max, macOS 26.4.1,
  2026-10-04; another session's process held one core at 100% throughout.
- **Results.** Apply, ms, p50 / p90 / max:

  | workload | before | after |
  |---|---|---|
  | golang:1.26's layers | 129.7 / 132.4 / 134.5 | 72.0 / 74.3 / 77.5 |
  | a million empty files | 618.0 / 626.3 / 628.2 | 617.2 / 625.4 / 628.0 |
  | a million over a million | 1317.1 / 1372.3 / 1375.9 | 1332.7 / 1374.2 / 1390.1 |

  Allocations were the same either way (80,498 for golang's). A whole root filesystem
  built with every core busy, ms:

  | image | build | n | p50 | p90 | max |
  |---|---|---|---|---|---|
  | golang:1.26 | before | 11 | 1120 | 1252 | 1354 |
  | golang:1.26 | after | 11 | 998 | 1128 | 1668 |
  | python:3.13 | before | 11 | 2170 | 2742 | 2912 |
  | python:3.13 | after | 11 | 1965 | 2230 | 2386 |

  Busy, the parallel unpack of M109 against 655212b's sequential one, in a separate
  run of 11 rounds: golang 5301 ms at p50 (p90 7791) before and 2656 (4580) after;
  python 6709 (9062) and 4356 (7945). The two runs' absolute numbers differ by more
  than 2x for the same code, as the host's other load changed between them; only each
  run's interleaved pairs compare.
- **Consequence.** A reader made over a seekable archive (`tar::Reader::seekable`)
  takes its length once and passes over data and padding with `seek_relative`, within
  its buffer where it can; `layer::apply` reads its archives so. An archive cut short is
  found as reading finds it, and the tar tests read every case both ways. On a busy
  host a build's time follows the CPU work it does, so work cut counts there most.

### M111. Building a root filesystem: CRC-32 in hardware, and room without a lock

- **Question.** A profile of golang:1.26's root filesystem built (macOS `sample`, 1 ms)
  showed gzip's CRC-32 in zlib-rs's portable `crc32_braid`, and unpack threads blocked
  in `__psynch_mutexwait` under `Checked::write`: 106 of one thread's 570 samples. flate2
  with `default-features = false` leaves out `runtime_detection`, which alone turns on
  zlib-rs's `std`, and zlib-rs looks for the CPU's CRC-32 (arm64), PCLMULQDQ and AVX2
  (x86_64) only with it (zlib-rs 0.6.8 cpu_features.rs). And every 8 KiB the unpack
  wrote locked the build's one `Room`. What does each fix save, idle and busy?
- **Method.** `rootfs-build`'s `ab.sh` (M109), which now reports each build's CPU
  time (getrusage, user and system, all threads), 2c05270 against the change, 9 rounds
  idle and 9 with `BUSY=18`. Apple M5 Max, macOS 26.4.1, 2026-10-04, other sessions
  running throughout.
- **Results.** Milliseconds a build, wall p50 / p90, then CPU p50:

  | image | host | before | after | CPU before | CPU after |
  |---|---|---|---|---|---|
  | golang:1.26 | idle | 950 / 3656 | 884 / 1411 | 2271 | 2079 |
  | golang:1.26 | busy | 1028 / 1239 | 925 / 1111 | 2152 | 1902 |
  | python:3.13 | idle | 1848 / 2808 | 1746 / 6385 | 2810 | 2598 |
  | python:3.13 | busy | 2246 / 5083 | 1881 / 1977 | 2629 | 2352 |

  CPU time falls 8–12% either way. Wall-time tails of 3–6 s fall on either build, idle
  or busy; the profile puts them in `write`, threads blocked as the build writes about
  1.8 GB (its unpacked layers, then its image) to a disk shared and 99% full.
- **Consequence.** flate2 is built with `runtime_detection` in every crate. `Room`
  keeps one atomic count of what its writers wrote together, and the writer whose bytes
  reach the next 64 MiB looks at the free space, so writers never wait on one another;
  with nothing to keep free it counts nothing. The write tails are the unpacked layers'
  temporary archives, which the next step takes away.

### M112. Pulling from real registries

- **Question.** shards' pulls had been checked against the E2E tests' loopback
  registry and Docker Hub. What do real registries make of them, next to Docker's?
- **Method.** `docs/research/measurements/registries/`:
  - `sweep.sh`: one cold pull with shards and one with Docker Desktop 29.3.1 (its
    containerd image store) of each of 13 images from 9 registries, for linux/arm64,
    and whether `images --digests` agrees.
  - `throttle.py` and `rates.py`: ECR Public's answers to anonymous manifest HEADs over
    one kept-alive connection: at 1 to 16 a second, 8 s each; and asked again 0 to
    250 ms after a 429.
  - `ab-pull.sh`: 20 rounds of cold pulls from ECR Public by 9d9d1de and by the change,
    in turn.
  - The CNCF distribution registry v3.0.0 on this host, over TLS (its CA in
    `~/.docker/certs.d`) and over plain HTTP, serving one image.
  Apple M5 Max, macOS 26.4.1, home broadband, 2026-10-04.
- **Results.**
  - Every image pulled by both resolved to the same digest. shards' cold pulls took
    2.1 to 24.0 s where Docker Desktop's took 3.4 to 53.8 s; gcr.io was as slow for
    either (shards 4.3 to 8.2 s over 5 pulls of distroless/static, Docker 3.7 to 9.7 s
    over 3), at about 0.45 s a request to it.
  - `shards pull --platform` was refused as an unknown flag: `docker pull`'s flags were
    not read.
  - ECR Public refused one of shards' pulls with `TOOMANYREQUESTS: Rate exceeded`,
    which Docker pulled. It throttles anonymous requests at random, more the faster
    they come, and never sends `Retry-After`:

    | asked a second | sent a second | throttled |
    |---|---|---|
    | 1 | 1.1 | 2 of 9 |
    | 2 | 2.1 | 2 of 17 |
    | 4 | 4.1 | 12 of 33 |
    | 8 | 8.1 | 34 of 65 |
    | 16 | 16.0 | 87 of 129 |

    Asked again after a 429, it answered 200 1 time in 6 at once, and 2 to 4 times in 6
    after 10 to 250 ms.
  - redis:7.4 from ECR Public, 20 pulls each: 12 failed with 9d9d1de, which never
    retried a 429; none with the change, at 2.43 s p50, 6.52 s p90 and 6.67 s max.
  - shards could not pull from the distribution registry on `localhost:5055` over TLS:
    it asked loopback hosts in plain HTTP only. With the change it pulls over TLS there,
    and over plain HTTP on `localhost:5056` and `127.0.0.1:5056`, after its HTTPS
    hello is answered in plain HTTP.
  - A loopback server that read the TLS hello and never answered cost 5 attempts of
    10 s each, reported as `Resource temporarily unavailable`: the handshake's socket
    timeout was not taken for its deadline.
- **Consequence.** A loopback registry on a port but 80 and 443 is asked in HTTPS first,
  and in plain HTTP from the first handshake answered in plain HTTP or timed out, as
  containerd v2.4.1 does; its certificate is verified, where containerd skips it. A
  handshake whose socket times out reports its deadline. A 429 is waited out, up to
  five times, by a random share of 50 to 800 ms or a `Retry-After` that fits, unless
  a quota is spent. `shards pull` is to read `docker pull`'s flags.

### M113. What a launch costs, and what loads at it

- **Question.** The command had been a binary apart from the daemon so that it started
  fast (M23). One binary for every command is to start as fast. What does a launch of
  each cost, and why?
- **Method.** Each binary run 300–400 times, in turn, through Python's `subprocess.run`
  (whose own cost is in every number alike), on a command each answers by itself:
  `shards push --help` (the command, 0.8 MB), `shardsd help` (the daemon, 8.7 MB),
  `shards-vm --help` (1.2 MB), `shards-net` (0.5 MB); and an empty C program built
  three ways, linking nothing, Security and CoreFoundation, and those and
  Hypervisor.framework. Load commands by `otool -L`, fixups by `dyld_info -fixups`.
  Apple M5 Max, macOS 26.4.1, 2026-10-04.
- **Results.** Milliseconds, p50 / p99:

  | launch | links at launch | p50 | p99 |
  |---|---|---|---|
  | empty C | libSystem | 2.10 | 2.68 |
  | empty C | + Security, CoreFoundation | 3.20 | 3.89 |
  | empty C | + those and Hypervisor | 3.21 | 4.05 |
  | `shards` command | libSystem | 2.31 | 3.08 |
  | `shards-net` | libSystem | 2.19 | 3.18 |
  | `shards-vm` | Hypervisor | 3.39 | 4.72 |
  | `shardsd` | Hypervisor, Security, CoreFoundation | 3.52–3.80 | 4.59–4.94 |
  | `shardsd`, Security and CoreFoundation bound when used | Hypervisor | 3.68 | 6.53 |
  | `shardsd`, every framework bound when used | libSystem | **2.50** | **2.94** |

  beside the command's 2.32 / 2.73 in the same run. `shardsd` has 15,119 fixups, the
  command 1,218. Hypervisor.framework alone costs as much as Security and CoreFoundation
  together: it loads what they would.
- **Consequence.** A framework linked is loaded by dyld at every launch, used or not:
  1.1 ms of one. The binary's size costs 0.18 ms. `shards` links none: Apple's
  frameworks are looked up with `dlsym` when first called (D36), and the command and the
  daemon are one binary, 0.18 ms slower to launch than the command alone was.

### M114. Pulling a 6 GB image: one connection, many, and what bounds it after

- **Question.** A pull of nvidia/cuda:12.9.1-cudnn-devel-ubuntu24.04 (12 layers, the
  largest 2.8 GB) took 118 to 163 s at about 46 MB/s, where the line carries more. What
  holds it: one connection's rate, the redirect each request takes, or the build after?
- **Method.** `docs/research/measurements/registries/`:
  - `ranges.py`: the first 537 MB of that 2.8 GB layer from Docker Hub's CDN over 1, 2,
    4, 8 and 16 ranged connections at once, 3 rounds in alternating order.
  - `hops.py`: 30 ranged requests, kept alive, asked of Docker Hub (which redirects to
    its CDN) and of the CDN's URL directly, in turn; time to the answer.
  - `ab-pull.sh`: cold pulls of the image by b8fbb05 and by the change, 3 each, in turn,
    fresh homes; and one traced pull of the change alone (its connections, rate each
    second, each layer's end).
  Apple M5 Max, macOS 26.4.1, home broadband, 2026-10-04. Every run here fell between
  17:55 and 19:37, while another session's 24 CPU-bound processes ran unintended on the
  same machine: the CPU-bound parts (unpacking, the build after) were measured under that
  load too, and are to be measured again.
- **Results.**
  - MB/s by connections (median of 3; each round): 1: 73 (49, 74, 73); 2: 80 (92, 80,
    78); 4: 98 (94, 102, 98); 8: 90 (98, 90, 85); 16: 91 (95, 91, 91).
  - Answer times, n=30: via the registry p50 151 ms, p90 170, max 303; the CDN's URL
    directly p50 52, p90 70, max 314 (measured while another pull ran).
  - Layers unpacked while the rest downloaded (pipelined, as the store's `ready` gate
    allows): b8fbb05 124.0 s p50 and 163.1 max, the change 135.6 and 142.0: the same
    within 3 runs, both at one connection's rate on the largest layer.
  - With ranged fetching, the traced pull had every layer by 57.7 s, at 97 to 129 MB/s a
    second once its connections had climbed to 16 (4, then 8 at +14%, then 16 at +49%,
    each doubling kept for 10% or more). Its whole took 140.1 s: the rest was building the
    root filesystem from the 2.8 GB layer, which starts only once that layer is whole.
  - An A/B of 3 pulls each, run while the full test suite ran on the same machine:
    b8fbb05 144.9 s p50, 165.0 max; the change 185.9 and 301.9. The suite's CPU load fell
    on the change's unpacking; the download had halved. Not a fair comparison; recorded as
    it ran.
- **Consequence.** Layers are fetched over connections that grow while they pay (4,
  doubling while a doubling brings 10% more, to 16), a large layer split between them as
  they free up, each split only where its half outlasts a request, and ranges after a
  blob's first asked of the URL the registry redirected it to (fetch.rs). The download no
  longer bounds a large pull: unpacking its largest layer does, after the layer has
  arrived. Next: unpack a layer as its bytes arrive in order, not after.

### M115. What keeping a container's writable layer costs a run

- **Question.** A container that stays keeps its writable layer once its microVM stops
  (D37): init packs its overlay upper directory as an OCI layer, sent to the host on the
  run's connection. What does that add to `shards run`?
- **Method.** `docs/research/measurements/layer-save/ab.py`: `shards run alpine` of a
  command that writes `size` bytes into the root (0, and 4 MiB), with `--rm` (no layer
  saved) and without (layer saved), interleaved, 50 each after one warm-up each. Apple M5
  Max, macOS 26.4.1, 2026-10-05, at load averages of 3 to 7 from other work on the host.
- **Results** (ms, n 50 each).
  - Saved before the run's end was told (the first build): 0 bytes, `--rm` p50 9.45, p90
    14.13, p99 18.89; kept p50 38.93, p90 52.64, p99 63.86. 4 MiB: `--rm` p50 17.97, p99
    78.95; kept p50 173.44, p90 207.96, p99 314.68.
  - Saved after it (the end told to the daemon and the client first, the layer after;
    `shards start` waits for it): 0 bytes, `--rm` p50 12.31, p90 22.28, p99 25.31; kept
    p50 12.78, p90 21.29, p99 30.99. 4 MiB: `--rm` p50 17.19, p90 26.48, p99 31.07; kept
    p50 23.69, p90 35.01, p99 65.98.
- **Consequence.** The layer is saved after the run's end is told. What it still costs is
  the save's work beside the next run: 4 MiB of changes moved over the run's connection
  at 64 KiB a frame. A writable layer kept on the host from the start (a disk of its own
  under the overlay) would cost a stop nothing; to be measured against this.


### M116. What a visit to a stopped microVM's files costs

- **Question.** `cp`, `diff` and `export` of a stopped microVM read its files in a VM
  booted over them (D37, `daemon/visit.rs`): its image, its writable layer put back,
  holding (`builtin::HOLD`), then ended with its changes saved. What does that add?
- **Method.** `docs/research/measurements/visit/visit.py`: `shards diff` of a running
  alpine microVM and of a stopped one, interleaved, 100 each after 5 warm-ups, each timed
  from the client's start to its exit. Apple M5 Max, macOS 26.4.1, revision 7d1065c plus
  the visit, 2026-10-05, at load averages of 5 to 9 from other work on the host.
- **Results** (ms, n 100 each). Running: p50 4.07, p90 4.86, p99 5.73, max 5.73.
  Stopped: p50 9.76, p90 10.60, p99 11.79, max 11.79.
- **Consequence.** A visit costs about 5.7 ms at p50 over reading a running microVM: a warm
  VM restored, the layer applied, the layer saved again. dockerd reads a stopped
  container's layer where it lies; a layer kept on the host from the start would let
  shards do the same, to be measured against this.

### M117. What the guest kernel keeps of a microVM's memory

- **Question.** `-m L` limits a workload's cgroup to L bytes (`memory.max`), as runc does;
  the microVM around it must hold L for the workload besides what its kernel keeps for
  itself, or the guest runs out before the limit binds. How much does the kernel keep, by
  the VM's size?
- **Method.** `docs/research/measurements/guest-memory/measure.sh`, with resources.rs's
  table emptied so that `-m SIZE` boots a VM of exactly SIZE: an alpine workload reads
  `/proc/meminfo`; the overhead is the VM's KiB less MemAvailable, which the kernel
  computes as what may be allocated without reclaim or swap (Documentation/filesystems/
  proc.rst). Three runs a size. Apple M5 Max (128 GiB), macOS 26.4.1, guest kernel
  6.18.48 aarch64 (Image-6.18.48-aarch64-8363f9e3806f), revision 35a5e99 plus the resource
  flags, 2026-10-05.
- **Results** (KiB; the three runs of a size agreed but at 256 MiB, where MemAvailable
  read 218,928, 218,988 and 218,988, and the largest overhead is given).

  | VM (MiB) | MemTotal | MemAvailable | overhead |
  |---:|---:|---:|---:|
  | 256 | 237,164 | 218,928 | 43,216 |
  | 384 | 365,288 | 346,212 | 47,004 |
  | 512 | 494,056 | 474,288 | 50,000 |
  | 768 | 750,308 | 700,260 | 86,172 |
  | 1,024 | 1,007,844 | 957,792 | 90,784 |
  | 1,536 | 1,520,340 | 1,468,424 | 104,440 |
  | 2,048 | 2,035,412 | 1,983,488 | 113,664 |
  | 3,072 | 2,998,248 | 2,906,440 | 239,288 |
  | 3,584 | 3,513,316 | 3,419,528 | 250,488 |
  | 4,096 | 4,028,388 | 3,932,580 | 261,724 |
  | 4,608 | 4,537,312 | 4,430,524 | 288,068 |
  | 6,144 | 6,082,524 | 5,969,704 | 321,752 |
  | 8,192 | 8,142,800 | 8,021,408 | 367,200 |
  | 12,288 | 12,251,068 | 12,106,300 | 476,612 |
  | 16,384 | 16,371,628 | 16,202,024 | 575,192 |

- **Consequence.** The overhead grows with the VM's size, and in steps: between 512 and
  768 MiB and between 2 and 3 GiB the kernel keeps 36 and 125 MiB more. No line through
  the points bounds them without giving small VMs over 100 MiB they do not need, so
  `resources::memory_mib` takes the overhead of the next measured size up, which bounds
  it where it does not decrease (it never did here), and past 16 GiB adds the 8 to 16 GiB
  slope, 25.4 KiB a MiB, rounded up to 26. The table is this kernel's on arm64; x86_64 uses
  it until measured on x86_64, and `run_limits_a_microvms_resources` holds, on
  each CI architecture, that a workload limited to L has L available.

### M118. What a local Docker engine does with an upload cut off part way

- **Question.** Can publishing a microVM to the local engine (`POST /images/load`) give
  up on an engine that stops answering without harm, and what does skipping an upload
  the engine already has save?
- **Method.** `docs/research/measurements/docker-load-lock/probe.py` sends an OCI layout
  of one fresh random blob to Docker Desktop's socket up to half the blob and closes the
  connection; then loads the same layout whole, a layout of another fresh blob, and the
  first again, each with a deadline. The presence check is 50 `GET
  /images/shards.local%2Falpine%3Alatest/json` in a row. Docker Engine 29.3.1 (Docker
  Desktop, containerd image store, 468 images), macOS 26.4, Apple M5 Max, revision
  0404821 plus the D41 work, load average 9 to 14 from other projects' tests, 2026-10-05.
- **Results.**
  - 4 MiB blob, 120 s deadlines: the same layout whole got `502 Bad Gateway` after 60.3 s
    (Docker Desktop's proxy); another blob had no answer in 120 s; the first again, 502
    after 60.4 s.
  - 1 MiB blob, 300 s deadlines: the same layout whole, 502 after 59.6 s; another blob,
    `200 OK` in 49.2 s; the first again, no answer in 300 s.
  - Earlier the same day a `docker load` killed at 60 s left its disk's digest failing
    the same way 10 minutes later, while a copy of the disk with one byte added loaded.
  - Loads of fresh 8 and 32 MiB artifacts each waited 13 and 15 s on a single write and
    11 and 7 s from their last byte to the answer; one of 64 MiB did not end in 400 s.
  - Presence check: n=50, p50 5.4 ms, p90 6.6 ms, p99 11.4 ms, max 11.4 ms.
- **Consequence.** An upload cut off leaves its content locked in the engine for at
  least minutes, and the engine's silences on a busy host run to tens of seconds, so a
  deadline would both cut off legitimate loads and lock what it cut off. shards cuts no
  upload off: each name's requests go on a thread of their own, so a stuck one holds up
  that name alone, and a microVM the engine holds already is not sent again (D41).

### M119. Which syscalls Docker's default seccomp profile allows that Docker's runc refuses

- **Question.** Docker's default profile (moby/profiles seccomp v0.2.3) names syscalls
  newer than the syscall tables of the libseccomp Docker's runc links. Does a container
  get them?
- **Method.** On Docker Desktop 29.3.1 (runc v1.3.4, which reports libseccomp 2.5.4;
  its VM's kernel 6.12.76-linuxkit, arm64), `python:3.12-alpine` called each of the 16
  syscalls the profile names and libseccomp 2.5.4's release tables (Linux 5.17's) lack,
  by its Linux 6.18 number, with invalid arguments (`syscall(nr, -1, 0, …)`), under the
  default profile and under `--security-opt seccomp=unconfined`
  (`scripts/seccomp/generate`'s oracle predicts the same profile's program). 2026-10-05.
- **Results.** Under the default profile `listmount`, `statmount`, `mseal`,
  `lsm_get_self_attr`, `lsm_list_modules` and `lsm_set_self_attr` returned ENOSYS, where
  unconfined the kernel answered (EFAULT or EINVAL): the kernel has them, Docker's filter
  refuses them. `cachestat`, `fchmodat2` and the futex2 calls answered alike either way,
  so Docker Desktop's libseccomp knows more than the 2.5.4 release (whose program, from the
  oracle, refuses from 450 up). The rest returned ENOSYS either way: 6.12 has none of them.
  The lsm calls are allowed only with CAP_SYS_ADMIN, so a container without it is refused
  them by the profile itself; the other three are allowed outright.
- **Consequence.** Docker's filter means what its runtime's libseccomp knows, which differs
  from build to build. shards reads a profile's names by the guest kernel's own tables
  (crates/seccomp, data/linux.csv of Linux 6.18.48), so that a profile means what it
  says on the kernel it runs (D42).

### M120. What shards' seccomp program costs, against runc's

- **Method.** `cargo test -p shards-seccomp --release -- --ignored --nocapture
  measure_programs` (crates/seccomp/src/oracle.rs): Docker's default profile with
  Docker's default capabilities, compiled by shards and as runc v1.3.4 with libseccomp
  2.5.4 loads it (testdata/oracle-*.json); each program's length, and the instructions
  each runs for every syscall of the guest kernel's tables with zero arguments, by the
  crate's BPF interpreter. Revision e1f416d plus the D42 work, 2026-10-05.
- **Results.**

  | ABI | n | shards len | runc len | shards mean / p50 / p90 / p99 / max | runc mean / p50 / p90 / p99 / max |
  |---|---:|---:|---:|---|---|
  | x86_64 | 383 | 397 | 1238 | 11.7 / 12 / 12 / 12 / 20 | 20.5 / 21 / 23 / 23 / 31 |
  | x86 | 459 | 397 | 1238 | 12.5 / 12 / 13 / 13 / 18 | 21.1 / 22 / 23 / 23 / 29 |
  | aarch64 | 325 | 252 | 805 | 10.2 / 10 / 11 / 11 / 19 | 18.0 / 19 / 20 / 20 / 28 |
  | arm | 422 | 252 | 805 | 12.3 / 12 / 13 / 13 / 18 | 20.8 / 21 / 22 / 22 / 27 |

- **Consequence.** A third of the program, and 43% fewer instructions per syscall at the
  median: each ABI's numbers as ranges of one decision found by a binary search, and
  identical decisions shared. Counts, not time: the guest's time per syscall under each
  is to be measured in a VM.

### M121. What the device filter costs a run

- **Method.** `docs/research/measurements/build-ab/ab.py OLD NEW alpine:3.22 200 true`,
  `AB_FLAGS=--rm`: 200 pooled runs through each of two builds' daemons, alternating, each
  arm's own template (the change is in shards-init, which the template holds, so the two
  cannot share one: templates alone differ by tens of µs, M29). OLD is 946ce6f; NEW is
  946ce6f with D44's device filter, first loaded and attached by shards-init at each
  start, then attached at boot, before the snapshot. Apple M5 Max (Mac17,6), macOS
  26.4.1, load average 80 to 86 throughout (another project's tests), 2026-10-05.
- **Results** (µs; "command" is the run's time in the guest, request to status, by the
  VM's clock; "outside" the rest of the client's wall clock).

  | Build | Part | n | p50 | p90 | p99 | max | NEW − OLD, paired median [95%] |
  |---|---|---:|---:|---:|---:|---:|---|
  | OLD | command | 200 | 842 | 1009 | 1641 | 2105 | |
  | NEW, at each start | command | 200 | 1862 | 2117 | 2739 | 2825 | +1026 [+1006, +1044] |
  | OLD | wall | 200 | 7900 | 9400 | 13854 | 14636 | |
  | NEW, at each start | wall | 200 | 8971 | 10069 | 13656 | 16273 | +1021 [+891, +1180] |
  | OLD | command | 200 | 892 | 1024 | 1518 | 3504 | |
  | NEW, at boot | command | 200 | 872 | 995 | 1355 | 1544 | −15 [−24, +0] |
  | OLD | wall | 200 | 7908 | 8934 | 14701 | 17655 | |
  | NEW, at boot | wall | 200 | 7853 | 9051 | 12460 | 13991 | −80 [−196, +28] |

- **Consequence.** Loading and attaching the eBPF filter (and reading `/sys/dev`) at each
  start cost a run a millisecond of guest time, a fifth of the start budget. Attached at
  boot, before the template's snapshot, it costs a run with Docker's rules nothing that
  200 paired runs can find; only runs with other rules load their own.

### M122. What the in-VM server's TLS costs a microVM, in init and apart

- **Question.** The in-VM server (D60) needs TLS 1.3 (rustls on AWS-LC). Compiled into
  shards-init, what does it cost every microVM's memory, and what does an instance of its
  own cost?
- **Method.** `docs/research/measurements/init-tls/measure.sh`: the same image, booted with
  each init in turn (`SHARDS_INIT`), interleaved, ten runs each after each init's template
  was made, reading `MemAvailable` in the guest as the run starts. Host: MacBook arm64,
  macOS 26.4, pinned kernel 6.18.48, 237,036 KiB guest, revision 6ac7c81 plus the change.
  Then, with the server apart, each instance's `RssAnon` and `RssFile`
  (`/proc/PID/status`), read by the run's command in
  `the_in_vm_server_knows_each_agent`.
- **Numbers.** init grew from 1,055,672 to 2,273,744 bytes. `MemAvailable`: old init
  218,604 KiB (9 runs) and 218,564 (1); new init 215,496 KiB (10 runs): 3,108 KiB less
  in every microVM, more than the binary grew, init being in the initramfs, which is guest
  memory. Apart, from its own read-only device with DAX: init back to 1,055,672 bytes;
  each instance 132–136 KiB anonymous, 848–856 KiB of file pages, its code, which DAX
  maps from the device rather than copying into the page cache (M74).
- **Consequence.** The server is a binary of its own, on a device of its own attached only
  to microVMs whose image has agents; a microVM without agents pays nothing for it.

### M123. What the guest kernel keeps of an x86_64 microVM's memory

- **Question.** M117's, for x86_64: shards sizes a microVM so that `-m` fits beside what
  its kernel keeps, and with arm64's table `-m 700m` left an x86_64 guest 679,348 KiB
  (CI, 2026-10-07), less than the limit.
- **Method.** `docs/research/measurements/guest-memory/measure-direct.sh`, which boots each
  size exactly from an unchanged shards (`run --kernel … --rootfs … --memory`), three runs
  a size, in CI's `firecracker comparison (x86_64)` job (GitHub's ubuntu-24.04 runner with
  KVM), guest kernel 6.18.48 x86_64 (sha256 f54407e5…), run 37696057174, revision 2e7b0ef,
  2026-10-07. The largest overhead of a size's three is taken, as M117 took it.
- **Results** (KiB: the VM's size less MemAvailable).

  | VM (MiB) | runs | largest |
  |---:|---|---:|
  | 256 | 50,176; 48,564; 48,916 | 50,176 |
  | 384 | 53,096; 53,488; 53,996 | 53,996 |
  | 512 | 57,504; 58,780; 67,588 | 67,588 |
  | 768 | 124,044; 124,696; 125,172 | 125,172 |
  | 1,024 | 141,456; 141,892; 141,416 | 141,892 |
  | 1,536 | 155,724; 155,932; 155,288 | 155,932 |
  | 2,048 | 166,364; 165,400; 165,708 | 166,364 |
  | 3,072 | 190,308; 190,728; 190,532 | 190,728 |
  | 3,584 | 308,956; 306,736; 304,880 | 308,956 |
  | 4,096 | 312,308; 313,512; 313,972 | 313,972 |
  | 4,608 | 337,788; 337,324; 337,324 | 337,788 |
  | 6,144 | 371,536; 368,020; 367,816 | 371,536 |
  | 8,192 | 417,128; 409,532; 417,136 | 417,136 |
  | 12,288 | 503,656; 511,692; 512,184 | 512,184 |
  | 16,384 | 586,356; 585,116; 592,948 | 592,948 |

- **Consequence.** x86_64's kernel keeps more than arm64's at every size (768 MiB:
  125,172 against 86,172), so each architecture sizes by its own table
  (`resources::overhead_kib`), and past 16 GiB by its own slope, 22 KiB a MiB.

### M124. The S3 cache backend's requests against a real S3

- **Question.** Does a real S3 that checks signatures accept every request the S3
  cache backend (D88) makes? That means bodies signed by their hash, `UNSIGNED-PAYLOAD`
  parts, query-signed multipart requests, copy headers and keys escaped under a prefix
  with a space. A fake S3 only shows that the requests are made.
- **Method.** `docs/research/measurements/s3/run.sh`:
  - starts versitygw (`versity/versitygw@sha256:30292fc2…`, posix backend, which checks
    SigV4) in the `shards-dind` container;
  - reaches it on loopback through `tunnel.py`;
  - runs `s3_requests_are_a_real_s3s` with 5 MiB parts.
  
  macOS 26.4 (Darwin 25.4.0), arm64, 2026-10-08.
- **Results.**
  - PutObject, GetObject and HeadObject pass, along with a missing key's 404 and
    CopyObject with `REPLACE`.
  - A 12 MiB object goes up in three parts from a file offset and is copied onto itself
    in three `UploadPartCopy` parts; it reads back byte for byte both times.
  - A body whose signed hash is not its own is refused.
  - The first run found a real fault. `CompleteMultipartUpload` without S3's XML
    namespace (`xmlns="http://s3.amazonaws.com/doc/2006-03-01/"`, which the SDK writes)
    was refused with `InvalidPart`. The backend now writes it.
- **Consequence.** The backend's requests are signed and shaped as S3 takes them,
  multipart included, which the 5 GiB-part path for large layers relies on and no fake
  covers.

### M125. The azblob cache backend's requests against Azurite

- **Question.** Does Blob Storage accept every request the azblob cache backend (D89)
  makes? That means shared-key signatures over escaped names, a container's making,
  uploads with `If-None-Match: *` in one request and in blocks, and downloads. The
  recorded SDK requests pin the signer, not the server.
- **Method.** `docs/research/measurements/azblob/run.sh`:
  - starts Azurite (`mcr.microsoft.com/azure-storage/azurite@sha256:830430c1…`, blob
    service) in the `shards-dind` container;
  - reaches it on loopback through `../s3/tunnel.py`;
  - runs `azblob_requests_are_blob_storages` with 1 MiB blocks, under a prefix with a
    space.

  macOS 26.4 (Darwin 25.4.0), arm64, 2026-10-08.
- **Results.**
  - Every request passes on the first run.
  - The container is made, then found.
  - A missing blob is a 404.
  - A manifest is written and overwritten.
  - A 1,000-byte blob in one request and a 3 MiB blob in three blocks read back byte for
    byte, and a second upload of either is refused as `BlobAlreadyExists` and taken as
    done.
  - A request signed with a wrong key is refused.
- **Consequence.** The backend's requests are signed and shaped as Blob Storage takes
  them, blocks included, which layers past 32 MiB rely on and no fake covers.

### M126. How deep a build policy may nest, and what it costs

- **Question.** How long do OPA and shards' Rego take on the deepest policy OPA parses?
  How much stack does shards' policy thread need for it, and for the deepest JSON
  `load_json` reads? Go grows its stacks to 1 GB; a Rust thread's stack is fixed when the
  thread starts (D101).
- **Method.** `docs/research/measurements/rego-depth/run.sh` times OPA v1.14.1, as buildx
  v0.37.1 vendors it, on a policy whose one rule nests an array N deep (`opa_depth.go`).
  It then bisects the smallest stack on which
  `the_deepest_policy_runs_on_the_policy_thread` passes, each size in its own process
  (`SHARDS_STACK_PROBE`). That test runs, on the policy thread:
  - a 10000-deep JSON document read by `load_json`, compared and dropped;
  - a policy nested as deep as OPA's parser takes (its `DefaultMaxParsingRecursionDepth`,
    100000), parsed, compiled and evaluated.

  macOS 26.4 (Darwin 25.4.0), arm64, release builds, 2026-10-09; x86_64 under Rosetta 2.
- **Results.**

  | depth | OPA parse | OPA compile | shards parse | shards compile |
  |---|---|---|---|---|
  | 1000 | 1.0 ms | 1.9 ms | 1.3 ms | 2.2 ms |
  | 4000 | 7.2 ms | 4.6 ms | 2.8 ms | 4.0 ms |
  | 33333 | 79 ms | 50 ms | 29 ms | 33 ms |

  - Before terms were shared, shards took 40 ms and 26 ms at depth 1000, and four times
    as long at each doubling: quadratic. The parser's cache of parsed terms and the
    compiler's copies cloned whole subtrees.
  - The stack the deepest cases need: 123 MiB on aarch64-apple-darwin, 120 MiB on
    x86_64-apple-darwin.
- **Consequence.** The policy thread's stack is 123 MiB (`policy::STACK`). Rust's std
  reserves it as address space: an `mmap`ed stack on Unix, and on Windows
  `CreateThread` with `STACK_SIZE_PARAM_IS_A_RESERVATION`. Memory is committed only as
  deep as a policy goes; this is unmeasured. The test holds every CI target to that stack.

### M127. What a build's policy check costs, and what an attestation chain can make it hold

- **Question.** What does each part of checking a source against a build policy cost:
  verifying an image's signatures, reading its provenance, building the input, compiling
  the policy, the evaluation's thread, a whole check? How much memory does a hostile
  provenance take to read, and how much stack the deepest chain documents?
- **Method.** `docs/research/measurements/policy-path/run.sh` runs `policy_path_costs`
  (ignored; release) on moby/buildkit v0.28.1's real linux/arm64 chain as Docker Hub
  served it (index, attestation manifest, signature manifest, Sigstore bundle, its
  80 KiB SLSA v1 provenance), offline, with the trusted root shards carries; a whole
  check is `Policies::evaluate` with a policy requiring Docker's GitHub builder's
  signature. Then valid provenances whose bulk is a field Go skips (1 to 64 MiB), each
  timed with the process's peak RSS (getrusage). Then it bisects the smallest stack on
  which the deepest chain is read and verified where a build does both
  (`the_deepest_chain_is_*`, `SHARDS_JSON_STACK_PROBE`). shards_sigstore's and
  shards_tuf's JSON parsers alone were bisected the same way, on aarch64 and on x86_64
  under Rosetta 2. Apple M5 Max, macOS 26.4 (Darwin 25.4.0), release builds, 2026-10-09,
  other builds running on the host.
- **Results.** Before is 502adc0; after is ac85ad7, where a build keeps each chain's
  readings (`signatures::Trust`), run on its code just before it was committed (the
  harness printed 502adc0). Microseconds; the quieter of two runs each.

  | step | n | before p50 / p90 / p99 / max | after p50 / p90 / p99 / max |
  |---|---|---|---|
  | verify image (`parse_signatures`) | 200 | 1337 / 1447 / 1614 / 2748 | 1162 / 1377 / 1502 / 2641 |
  | read provenance (80 KiB) | 200 | 402 / 458 / 536 / 1581 | 366 / 434 / 528 / 1507 |
  | input of an image with its chain | 200 | 1784 / 1915 / 1983 / 2058 | 38 / 66 / 92 / 2243 |
  | compile the policy | 200 | 535 / 641 / 746 / 1601 | 469 / 615 / 756 / 1879 |
  | an evaluation's thread (123 MiB) | 500 | 40 / 63 / 87 / 211 | 31 / 53 / 84 / 140 |
  | run, partial (compile and eval) | 200 | 631 / 736 / 823 / 1011 | 553 / 681 / 844 / 979 |
  | run, whole (compile and eval) | 200 | 482 / 605 / 672 / 717 | 514 / 624 / 764 / 817 |
  | a source checked (signed image) | 100 | 4566 / 4881 / 5439 / 5668 | 2455 / 2837 / 3143 / 3392 |

  - Run again at ac85ad7 with other builds running on the host: an input with its
    chain 46 / 65 / 272 / 3710, a source checked 3716 / 4091 / 4735 / 4891.
  - A hostile provenance: 1 MiB read in 4.0 ms, +49 MiB peak RSS; 4 MiB, 19.6 ms,
    +144 MiB; 16 MiB, 70.6 ms, +374 MiB; 64 MiB, 332 ms, +2209 MiB (peak 2777 MiB).
  - The deepest chain (an index and a signature manifest 10000 levels deep) is read and
    verified in 3616 KiB on aarch64-apple-darwin. Parsing and dropping such a document
    alone takes 3616 KiB on aarch64 and 3136 KiB on x86_64-apple-darwin; shards_tuf's
    parser, 2976 KiB and 2528 KiB.
  - The first input of a chain costs what it did (its max, 2243); each after it, the
    chain's SHA-256 and a copy. Compiling (469) is most of each run (553 partial, 514
    whole), and a check runs twice, partial then whole.
- **Consequence.** A chain's blobs come to at most 4 MiB, what buildx's policy session
  receives (`attest::CHAIN_MAX`), checked before each blob is fetched: the largest
  provenance a chain may hold took +144 MiB to read (the 4 MiB row). Chains are read
  and verified on a 4 MiB thread (`attest::JSON_STACK`), the most measured in whole
  MiB, held to every target by those two tests. A build reads each chain once.
  Compiling a policy once a build is the largest cost left; a compiled program is not
  `Send`, so that needs a long-lived evaluation thread (unmeasured).

### M128. What reading an image's config as Docker reads it costs

- **Question.** `parse_config` now reads a config as moby's encoding/json does
  (`shards_image::config`: Go's scanner builds the whole document, then every field is
  read, every history entry's time parsed, so each Go type's answer comes from one
  reading); before, serde read it by exact key and skipped what it did not model, the
  history among it. What does that cost, on a config of a build's size and on large ones?
- **Method.** `docs/research/measurements/image-config/run.sh` runs `parse_config_costs`
  (ignored; release): Go's reader and a copy of the serde reader it replaced, n readings
  each, interleaved and each first in turn, so a busy host weighs on both alike. Three
  configs: a BuildKit build's (12 history steps, 8 variables and labels, 2.6 KB); a long
  history (2000 steps, 500 variables and labels, 358 KB); one near `MAX_CONFIG` (18000
  steps, 4000 variables and labels, 3.2 MB). Apple M5 Max, macOS 26.4.1, revision
  258bea2, 2026-10-09, load average 20 (other builds running).
- **Results.** Microseconds, the first of two runs (the second's p50s within 3%).

  | config | n | Go p50 / p90 / p99 / max | serde p50 / p90 / p99 / max |
  |---|---|---|---|
  | build-sized, 2.6 KB | 2000 | 14 / 15 / 19 / 52 | 2 / 3 / 3 / 12 |
  | long history, 358 KB | 200 | 1634 / 1730 / 1844 / 1852 | 274 / 295 / 338 / 349 |
  | near MAX_CONFIG, 3.2 MB | 40 | 14415 / 15655 / 16617 / 16617 | 2408 / 2567 / 2882 / 2882 |

  - The second run: build-sized 14 / 15 / 19 / 53 against 2 / 3 / 4 / 9; long history
    1598 / 1807 / 2456 / 2737 against 254 / 297 / 334 / 397; near MAX_CONFIG
    14862 / 15669 / 20293 / 20293 against 2573 / 2644 / 4126 / 4126.
- **Consequence.** Reading as Go reads costs 5 to 7 times what serde's reading did,
  12 µs more on a build's config, where a run's budget is 5 ms; it reads what serde
  skipped (each history entry and its time), which Docker's own answers depend on.
  Where the time goes within the reading (the scanner's tree, the fields, the times) is
  unmeasured, and so is a reader that decodes as it scans, as encoding/json does.
- **Where the time goes, and a string's run read whole (2026-10-10).** `run.sh` now also
  runs `parse_config_phases` (ignored; release): a build-sized config read n=20000 times
  after 1000 warm-ups, the mean in nanoseconds of Go's scanner building the document
  (`scan`), that and its fields read (`scan+fields`), the whole `parse_config`, and serde's.
  The scanner took two thirds of a reading, pushing a string's bytes into it one at a
  time; it now copies each run of bytes that stand for themselves (no quote, backslash,
  control or non-ASCII byte) at once, every answer unchanged (the oracles of
  `shards-image` and `shards-agentfile`; each byte of the run's end mutation-checked,
  one of them by a new case of invalid UTF-8). Old and new alternated three times, back
  to back, on one host (Apple M5 Max, macOS 26.4.1, revision 1265bd5 and its string fast
  path, load average 13 to 26 with other builds running):

  | arm | scan | scan+fields | whole | serde |
  |---|---|---|---|---|
  | old, three runs | 9525 / 9926 / 9410 | 13847 / 14333 / 13789 | 14219 / 17968 / 14334 | 2779 / 2703 / 2662 |
  | new, three runs | 5945 / 5948 / 5470 | 9586 / 10233 / 9738 | 10381 / 10696 / 10095 | 2733 / 2632 / 2702 |

  The scan is 40% shorter and a whole reading 28% (14.3 µs to 10.4 µs at the median of
  three). The fields' part (about 4 µs) copies each string again into a `String`, as the
  reading walks the document by reference; one that took the document apart would not.

### M129. What virtio-fs's hostile-request fixes cost (audit V01-V04)

- **Question.** The share process now looks at a file before and after opening it, and
  restores the guest's status flags (V02); the VM's device bounds a request before
  allocating it (V03) and lets go of guest memory while the share answers (V04). What do
  an OPEN and a request's round trip cost now, and how long does another thread wait for
  guest memory while the share is slow? And do the primitives V01 rests on hold on Linux?
- **Method.** `docs/research/measurements/virtio-fs-audit/run.py`, which builds its
  harness against two revisions' `crates/vmm` (here: revision 8c71704's virtio-fs
  sources, and the fixed ones) and alternates fresh processes, OLD NEW then NEW OLD,
  20 per arm per case, after a warm-up process each:
  `open`, a share's OPEN then RELEASE of one file (`Server::handle`, 2000 samples a
  process after 200); `serve`, a GETATTR through the device from the driver's notify to
  its used entry, the share answering at once on a thread (2000 after 200); `held`, the
  share answering after 5 ms, the time another thread waits for `GuestMemory::access`
  once the share has the request (20 a process). Apple M5 Max, macOS 26.4.1, revision
  8c71704 and the fixes, 2026-10-09, load average 53 and 76 (other builds running).
- **Results.** Microseconds, pooled samples; the paired difference is of the processes'
  medians, with a bootstrap 95% interval. The second run, after one `fcntl` replaced two
  in restoring the guest's flags:

  | case | old n / p50 / p90 / p99 / max | new n / p50 / p90 / p99 / max | new − old |
  |---|---|---|---|
  | open | 40000 / 8.83 / 9.08 / 11.04 / 12229 | 40000 / 10.29 / 10.62 / 16.29 / 4772 | +1.54 [1.33, 3.12] |
  | serve | 40000 / 8.71 / 891 / 14917 / 72798 | 40000 / 8.54 / 733 / 14262 / 105084 | +0.04 [−0.96, 1.46] |
  | held | 400 / 8878 / 18654 / 27814 / 36159 | 400 / 0.25 / 0.50 / 0.83 / 2.38 | −8575 [−9263, −8084] |

  - The first run, with two `fcntl`s: open +1.58 [1.29, 1.83] (p50 8.79 against 10.29);
    serve +0.33 [−0.29, 1.75]; held 9506 against 0.21 at p50.
  - `serve`'s tails are the busy host's: the driver spins while the worker and the share's
    thread wait to be scheduled, in both arms alike.
  - Linux (`probes/chmodat.c`, uid 1000 in a `rust:1.98.0` container in shards-dind):
    V01's steps refuse a symlink to a file outside the share (EOPNOTSUPP, the file's
    0600 kept), change a regular file, and change one of mode 000, which its owner cannot
    open, through its `O_PATH` descriptor.
- **Consequence.** A guest's open of a shared file costs 1.5 µs more in the share process,
  the price of never opening a FIFO or device; the device's round trip is unchanged; and
  a slow share no longer holds guest memory from the VM's other devices and snapshots,
  which waited on it for as long as it took (8.9 ms of a 5 ms answer at the median
  here), or for good. Unmeasured: the open's whole round trip from the guest.

### M130. What a directory a guest opens costs the share (audit V05)

- **Question.** OPENDIR copied a directory's listing into its handle. What did a handle
  hold, what does one hold that reads from a descriptor of its own instead, and what do
  the C libraries' directory streams cost, which a handle might keep? Does a listing get
  slower?
- **Method.** `crates/vmm/tests/fs_dir_handles.rs`, a test binary of its own whose global
  allocator counts the bytes allocated and not freed: 100 OPENDIRs of a directory of 2000
  entries, after one that grows the server's tables. `probes/dirbuf.c`: malloc's bytes in
  use before and after 200 `fdopendir`s each read once (macOS: `malloc_zone_statistics`;
  glibc: `mallinfo2`, in a `rust:1.98.0` container in shards-dind). `run.py --cases
  list,listplus --runs 10`: whole listings of 10,000 entries (OPENDIR, pages of 4096
  bytes, RELEASEDIR), READDIR 30 a process and READDIRPLUS 10, the old arm at 6ad9857.
  Apple M5 Max, macOS 26.4.1, 2026-10-09, load average 51.
- **Results.**
  - Held for 100 handles: 10,759,216 bytes before (108 KB a handle, the listing's
    names and their table), 8,092 after (81 bytes a handle, the handle table's).
  - A stream: 2,208 bytes on macOS, 32,835 on glibc (its 32 KiB buffer; up to 1 MiB where
    a directory's `st_blksize` says so). So the Linux handle keeps no stream, only its
    descriptor, and reads by getdents64 into a buffer for the request alone.
  - Listings, milliseconds, n / p50 / p90 / p99 / max:

    | case | old | new | new − old, paired |
    |---|---|---|---|
    | READDIR | 300 / 17.7 / 42.6 / 96.7 / 167.1 | 300 / 4.4 / 10.7 / 36.4 / 49.1 | −12.3 [−14.7, −10.6] |
    | READDIRPLUS | 100 / 453.7 / 593.2 / 708.2 / 1560.7 | 100 / 458.5 / 605.3 / 850.8 / 888.2 | +15.4 [−56.7, +51.7] |

- **Consequence.** A guest can hold the share process to a descriptor and 81 bytes a
  directory it opens (with 2.2 KiB of stream on macOS), as many as the process may have
  descriptors, where it held the directory's size each, without bound. READDIR is four
  times faster on a large directory: each page copied the rest of the listing before; it
  now reads only the page. READDIRPLUS is its lookups', unchanged. The Linux path runs
  in CI's Linux jobs; `probes/getdents.rs` checks the paging by `d_off` on its own: on
  Linux 6.12.76 (linuxkit, aarch64), as uid 1000, 1,500 files paged by getdents64 at
  `d_off` cookies, pages of 3, 17 and 200 entries (buffers of 1,024, 4,096 and 32,768
  bytes), each came out once (2026-10-10).

### M131. How fast a guest can fail a device again and again (audit V07)

- **Question.** A device that meets a malformed ring says so in the host's log and needs
  reset; a guest can reset it, break it again, and so on. How many failures, and how
  many bytes of log, can one guest make?
- **Method.** `docs/research/measurements/device-reset-log/`: a virtio-blk device behind
  its MMIO transport, driven through the registers as a driver drives them, its queue's
  available index 100 ahead of a ring of 8. Each cycle writes STATUS 0, sets the device
  up, writes DRIVER_OK and waits for DEVICE_NEEDS_RESET; 5 s a run, the process's log
  sent to a file (`log::to`). Apple M5 Max, macOS 26.4.1, revision 4027d72 (old) and its
  fix (new), 2026-10-09, load average 51 to 89. No vCPU: a guest's register writes each
  add an exit's 0.8 µs (PM M4).
- **Results.** Runs in the order made:

  | arm | cycles in 5 s | log lines | log bytes |
  |---|---|---|---|
  | old | 2184 | 2184 | 220,584 |
  | new | 7267 | 1 | 101 |
  | old | 545 | 545 | 55,045 |
  | new | 6444 | 1 | 101 |

- **Consequence.** Each failure was a 101-byte line of the daemon's log, which nothing
  rotates: at the 1,450 cycles a second the fixed device managed, 147 KB a second, 13 GB
  a day, from one guest. A device's failure is now said once, across resets; the rest go
  to debug. The cycle rates themselves vary fourfold with the host's load, which was
  high and changing throughout.

### M132. How many directories a share can have looked up (audit V, open)

- **Question.** The share's server keeps a descriptor for every directory the guest has
  looked up and not forgotten, as it does every operation relative to one; a guest
  kernel forgets only under memory pressure. How many directories can a guest walk, at a
  given limit on descriptors?
- **Corrected (2026-10-09).** This entry first said the share process has macOS's 256:
  it inherits the daemon's limit, which the daemon raises at its start to min(hard limit,
  `kern.maxfilesperproc`), 245,760 here. 256 is a share process's only where the hard
  limit is that low or the daemon did not start it. At 245,760 the walk passes, and a
  guest pins as many of the host's table instead; see M136, which acts on both.
- **Method.** `virtio-fs-audit --case fds --dirs D --limit L`: the process's soft
  RLIMIT_NOFILE set to L, then a LOOKUP of each of D directories through
  `Server::handle`, none forgotten. Apple M5 Max, macOS 26.4.1, revision ce225fa,
  2026-10-09.
- **Results.** Limit 256: 252 of 600 found, then EMFILE (24). Limit 1024: 1,020 of 3,000,
  then EMFILE.
- **Consequence.** A guest that walks more directories than its share process may hold
  descriptors is told "Too many open files", and a hostile one can put its own shares
  there at will; at the limit the daemon gives it, one guest pins half of this host's
  table and two all of it. Done in M136.

### M133. What looking before opening a node's attributes costs (audit V08)

- **Question.** The share opens a node to read its attributes (the owner the guest sees)
  on every LOOKUP and GETATTR, and now looks at it first, so as not to open a device.
  What does that add?
- **Method.** `virtio-fs-audit/run.py --cases getattr`: a LOOKUP then a GETATTR of one
  regular file through `Server::handle`, timed as a pair, 2000 a process after 200, 20
  processes an arm alternating, the old arm at 996de13. Apple M5 Max, macOS 26.4.1,
  2026-10-09, load average 70 and 80.
- **Results.** Microseconds, n / p50 / p90 / p99 / max, and the paired difference of the
  processes' medians with a bootstrap 95% interval:

  | version | old | new | new − old |
  |---|---|---|---|
  | a look of its own in each metadata open | 40000 / 32.42 / 38.04 / 72.46 / 110456 | 40000 / 34.62 / 41.50 / 78.29 / 70812 | +2.21 [1.08, 2.88] |
  | LOOKUP's and GETATTR's own stat passed on | 40000 / 33.29 / 40.00 / 77.58 / 27689 | 40000 / 32.96 / 37.25 / 78.62 / 48552 | 0.00 [−4.33, 0.75] |

- **Consequence.** A second look at each node cost 2.2 µs, 7%, on the pair a guest's
  `stat` makes; with the stat each request has just taken passed on (the node's own,
  never a guest-named open file's), the device check costs nothing measurable. The
  extended attribute requests, rarer, still look for themselves.

### M134. How long a socket a process lets go stays bound while it spawns children (CI: the published-port bind race)

- **Question.** The published-port tests found a port refused (EADDRINUSE) right after
  the test had bound and let go of it, held for 10 to 106 ms by nothing `ss`, `netstat`
  or `lsof` could see (project-port-free-flake, 2026-10-06 to 2026-10-10, on this host
  and a GitHub Linux runner). Does a child another thread spawns hold such a socket?
- **Method.** `docs/research/measurements/bind-race`: one thread binds a port (std,
  `SO_REUSEADDR`), lets it go and binds it again, ten seconds; 0, 4 or 16 threads spawn
  `/usr/bin/true` meanwhile, by std's `Command` or by `posix_spawn` with
  `POSIX_SPAWN_CLOEXEC_DEFAULT`, as `shards_ipc::spawn` does on macOS. Each second bind
  refused is retried until it succeeds, and that wait recorded. Apple M5 Max, macOS
  26.4.1, 2026-10-10, load average 19 to 33.
- **Results.** Holds of a let-go listener, n / p50 / p90 / p99 / max:

  | spawns | spawning threads | drops | second binds refused | hold |
  |---|---:|---:|---:|---|
  | none | 0 | 21,310 | 0 | — |
  | std | 4 | 17,262 | 2,919 | 2,919 / 0.42 / 1.42 / 3.68 / 7.30 ms |
  | std | 16 | 12,279 | 2,459 | 2,459 / 0.42 / 1.96 / 18.6 / 238.9 ms |
  | CLOEXEC_DEFAULT | 4 | 18,830 | 2,906 | 2,906 / 0.36 / 1.20 / 3.28 / 18.2 ms |
  | CLOEXEC_DEFAULT | 16 | 12,098 | 3,048 | 3,048 / 0.37 / 1.37 / 16.4 / 153.8 ms |

- **Consequence.** A child holds every descriptor its parent had at the spawn until it
  execs, close-on-exec ones included, `POSIX_SPAWN_CLOEXEC_DEFAULT` too (XNU copies the
  table before exec closes them); glibc's `posix_spawn` (sysdeps/unix/sysv/linux/spawni.c:
  `clone(CLONE_VM | CLONE_VFORK)`, the descriptor table copied) does the same until
  `execve`, by its source, not measured here. So a listener a multithreaded process drops while another of its
  threads spawns stays bound in the child, up to a quarter of a second under load. The
  test process, its tests spawning in parallel, held its own probes of the ports it then
  had a run publish: those probes now run in a process of their own that spawns nothing
  (tests/common `port_free`, `hold_port`). The daemon holds a run's listeners from
  binding them until the run is taken, while its other threads spawn VMs: a spawn then
  holds them until it execs, so a run shorter than that spawn may end before its ports
  are free. Not seen in a failure; fixed since: every child of the daemon is made by its
  spawner, which holds none of them (M158, architecture.md D31).

### M135. Signatures as OCI 1.1 referrers: shards against cosign, on registries with and without the API

- **Question.** What does cosign v3.1.3 write when it signs an OCI artifact with a key, and
  how does it list the signature on a registry with the referrers API and on one without?
  Does cosign accept shards' signatures, and shards cosign's, on both? What does signing
  cost? (D116)
- **Method.** Registries as host processes: distribution v3.1.2 (built from its v3.1.2
  module, Go 1.26.1; no darwin release exists) and zot v2.1.22 (its release, checksum
  checked); cosign v3.1.3's release (checksum checked), signing with a key it made and a
  signing config naming no service (no Rekor, Fulcio or TSA: nothing left the host).
  First by hand: an OSI agent artifact pushed, cosign's signature read back from each
  registry (its referrer manifest, bundle and list kept as
  `crates/sigstore/testdata/cosign/measured.json`). Then
  `docs/research/measurements/referrers/run.py` with shards' release build (revision
  0279c04): for each registry an agent made and pushed by shards, signed by shards and
  pushed, verified by `cosign verify --key --insecure-ignore-tlog`; signed by cosign too;
  both pulled by shards; cosign's deleted by shards, then shards'; then `shards sign` and
  `cosign sign` of one artifact on zot, alternated, 20 each. Apple M5 Max (18 cores),
  macOS 26.4, 2026-10-09, load average 93 (other forks' builds running). Raw results:
  `results-2026-10-09.json` beside the harness.
- **Results.**
  - cosign's signature: a manifest of `artifactType`
    `application/vnd.dev.sigstore.bundle.v0.3+json`, the empty config (carrying the same
    artifact type), one layer, the bundle, annotations `dev.sigstore.bundle.content:
    dsse-envelope`, `dev.sigstore.bundle.predicateType:
    https://sigstore.dev/cosign/sign/v1` and `org.opencontainers.image.created` (RFC 3339,
    UTC, to the second), `subject` the artifact's manifest. The bundle names the key by its
    hint alone (the SHA-256 of its PKIX encoding, base64) and holds a DSSE envelope of an
    in-toto v1 statement, the artifact's digest its one subject, an empty predicate; no
    log entry, no timestamp. shards' statement and manifest are these bytes (given the
    bundle and the time); the bundle verifies in shards.
  - Without the API (distribution: `GET referrers/<digest>` is 404 even with none): cosign
    put the manifest by digest, asked the API (404), read the tag schema's list (404) and
    put a new one at `sha256-<hex>`, its entry without the manifest's annotations, which
    the spec says to copy (spec.md:508). shards' push makes the same requests (blobs
    HEAD, POST and PUT; the manifest HEAD and PUT; `GET referrers` 404; the list's GET
    and PUT), its entry with the annotations. With the API (zot: 200, an empty index
    where there are none), each listed it itself.
  - `cosign verify --key` passed shards' signature on both (exit 0); shards pulled both
    signatures on both; with cosign's deleted by shards, cosign still verified (shards'
    remained); with both deleted, cosign found none (exit 10, `no signatures found`).
  - `shards sign` (local, scrypt N=2^16 included): n 20, p50 195 ms, p90 236, p99 264,
    max 264. `cosign sign` (scrypt, signing and pushing): n 20, p50 1198 ms, p90 1328,
    p99 1466, max 1466. Not the same work: shards signs into its store and pushes with
    `shards push`; most of either is the key's scrypt, which go-securesystemslib sets at
    about 100 ms of computation by design (encrypted.go:33).
- **Consequence.** shards signs with the keys cosign makes, in cosign's format, which
  cosign verifies, and reads cosign's; both registries' kinds of listing work both ways.
  Unmeasured: registries with the API that answer no `OCI-Subject` (shards then asks the
  API, as cosign does), and keyless signing, which needs Fulcio and Rekor.

### M138. What shards' BuildKit frontend costs against docker/dockerfile:1 (D113)

- **Question.** BuildKit runs a gateway frontend for every build a `# syntax=` line names.
  What does shards' (`shards frontend`, the one binary) cost against
  docker/dockerfile:1.27.1: from its start to its first byte on the gateway, to its end,
  and on each gateway call?
- **Method.** `docs/research/measurements/frontend/run BINARY SPY DECODE 100` in
  `shards-dind` (Docker 29.3.1, BuildKit v0.28.1, buildx v0.37.1):
  - `scripts/frontend/cases/basic` built 100 times by each frontend, interleaved, each
    first in turn;
  - both run through D113's spy (`scripts/gateway/spy`), which relays the frontend's
    stdio and stamps each chunk;
  - `--output type=cacheonly`, with BuildKit's cache warm, so the frontends' own work and
    their calls stand out;
  - a call timed from the chunk that ends its request to the one that ends its answer:
    BuildKit's time to answer it. What a run's calls leave of it is the frontend's own:
    its start, its work, and its exit;
  - shards at revision ec49548 (static musl, aarch64, with line tables);
    docker/dockerfile:1.27.1 pinned by digest;
  - Apple M5 Max, macOS 26.4.1, 2026-10-10, load average 25–36 (other builds and tests
    running).
- **Results.** Milliseconds, n / p50 / p90 / p99 / max:

  | | docker/dockerfile:1.27.1 | shards |
  |---|---|---|
  | start to first byte | 100 / 3.828 / 4.844 / 5.382 / 5.584 | 100 / 0.749 / 1.260 / 1.581 / 2.252 |
  | start to end | 100 / 192.052 / 260.038 / 381.732 / 393.213 | 100 / 178.381 / 245.738 / 379.215 / 456.209 |
  | Ping | 100 / 0.448 / 0.679 / 1.247 / 7.114 | 100 / 0.550 / 0.792 / 1.431 / 1.701 |
  | Inputs | 100 / 0.306 / 0.473 / 0.966 / 1.097 | 100 / 0.320 / 0.503 / 1.013 / 6.192 |
  | Solve | 300 / 0.435 / 0.694 / 1.127 / 2.006 | 300 / 0.372 / 0.531 / 0.762 / 1.317 |
  | StatFile | 300 / 5.825 / 22.221 / 42.082 / 66.125 | 300 / 5.646 / 22.914 / 75.838 / 91.556 |
  | ReadFile | 300 / 0.432 / 0.666 / 1.218 / 1.949 | 300 / 0.382 / 0.588 / 0.759 / 1.439 |
  | ResolveSourceMeta | 100 / 153.535 / 200.742 / 353.455 / 361.405 | 100 / 151.871 / 190.664 / 246.920 / 306.700 |
  | Return | 100 / 0.391 / 0.713 / 1.479 / 1.750 | 100 / 0.336 / 0.534 / 0.749 / 0.849 |

  The three slowest runs of each, and what their calls leave of them:

  | | run | in BuildKit's answers | the frontend's own |
  |---|---|---|---|
  | docker/dockerfile | 393.2, 381.7, 351.7 | 379.4, 368.6, 340.3 | 13.8, 13.1, 11.4 |
  | shards | 456.2, 379.2, 378.4 | 451.8, 373.0, 370.6 | 4.4, 6.2, 7.8 |

  Their largest answers were ResolveSourceMeta (216–361 ms) and StatFile (up to 86 ms).
  The images, as `docker image inspect` gives their size there: 13,145,255 bytes for
  docker/dockerfile:1.27.1, and 33,169,104 for shards' (the one binary, with its line
  tables). An earlier run of 100 each, at a load average of 34–38, gave start to first
  byte at p50 / p99 of 0.820 / 3.086 ms for shards and 3.611 / 11.558 ms for
  docker/dockerfile.
- **Consequence.**
  - shards' frontend reaches the gateway sooner: 0.75 ms against 3.83 at p50, and 1.58
    against 5.38 at p99.
  - A build's frontend time is otherwise BuildKit's: its answers held 96–99% of every
    slow run of either frontend. Resolving the base image's metadata from its registry
    takes the most: 152 and 154 ms at p50.
  - In those runs the frontend's own time was 4.4–7.8 ms for shards and 11.4–13.8 ms for
    docker/dockerfile.

### M140. Where a cache mount keeps its files, and what keeping them costs a build

- **Question.** `RUN --mount=type=cache` kept across builds (D114) needs a home outside
  the builder microVM, which goes with its build. The candidates were the virtio-fs share
  (`shards run -v`'s device, D38) and the guest's own filesystems over something the host
  keeps (a block device, or the layers a build keeps). How fast is each for what package
  managers do with a cache, and what does moving a cache in and out cost a build?
- **Method.** `docs/research/measurements/cachefs/cachefs.py` with `fsbench.rs` (built
  static for the guest): in one builder step, each phase in turn on a cache mount (the
  record's layers under the builder's memory, D114) and on a tmpfs mount (the builder's
  memory, where caches lived before D114), interleaved and each first in turn; the same
  phases on a virtio-fs share and the run's tmpfs (`shards run -v`, `--tmpfs`); a cache an
  earlier build filled (10,000 files of 4 KiB in 100 directories, 2 of 128 MiB) read back;
  and the wall time of builds that mount that cache and do nothing, or fill a cache, each
  against its empty or tmpfs twin. Phases: create, stat, read, rename and delete of the
  small files, then the large ones written and read, a MiB at a time. Apple M5 Max, macOS
  26.4.1, revision f6d11c2 (D114), 2026-10-09, load averages 68.7, 72.3 and 74.9 at its end
  (other builds and Docker Desktop's VM). That run passed `--no-cache` to every build,
  which at f6d11c2 kept a step's caches; since D114 lets them go, the harness gives each
  build's step an `ENV` of its own instead, and refuses a warm build whose cache it had to
  fill (fsbench says so). Milliseconds:

  | phase | cache mount, n=20, p50 / p90 / p99 / max | builder tmpfs, n=20 | virtio-fs share, n=3 |
  |---|---|---|---|
  | create | 28.3 / 32.3 / 38.3 / 38.3 | 15.8 / 16.8 / 23.4 / 23.4 | 107889 / 114197 / 114197 / 114197 |
  | stat | 7.4 / 8.4 / 10.8 / 10.8 | 5.1 / 5.9 / 7.2 / 7.2 | 6380 / 15723 / 15723 / 15723 |
  | read | 16.6 / 17.2 / 23.1 / 23.1 | 13.2 / 15.3 / 16.2 / 16.2 | 22242 / 49611 / 49611 / 49611 |
  | rename | 18.4 / 19.6 / 23.3 / 23.3 | 9.9 / 12.4 / 12.8 / 12.8 | 50484 / 126640 / 126640 / 126640 |
  | delete | 16.9 / 18.3 / 25.0 / 25.0 | 9.3 / 11.6 / 13.7 / 13.7 | 31659 / 90593 / 90593 / 90593 |
  | 256 MiB written | 14.7 / 33.4 / 34.3 / 34.3 | 14.3 / 18.3 / 20.6 / 20.6 | 4490 / 6548 / 6548 / 6548 |
  | 256 MiB read | 9.9 / 10.4 / 12.3 / 12.3 | 10.0 / 12.5 / 13.3 / 13.3 | 5002 / 6000 / 6000 / 6000 |

  All p50 / p90 / p99 / max; with n=3 or n=5 the last three are the slowest. The run's own
  tmpfs beside the share (n=3): create 15.8 / 17.0 / 17.0 / 17.0, stat 4.7 / 4.7 / 4.7 /
  4.7, read 12.6 / 12.8 / 12.8 / 12.8, rename 10.0 / 10.2 / 10.2 / 10.2, delete 8.7 / 8.9 /
  8.9 / 8.9, large write 16.0 / 18.8 / 18.8 / 18.8, large read 11.5 / 11.6 / 11.6 / 11.6. A
  cache an earlier build filled, read back (n=20): stat 9.3 / 14.8 / 15.2 / 15.2, read 20.3
  / 22.6 / 23.0 / 23.0, large read 9.0 / 12.2 / 15.8 / 15.8, against the step's tmpfs filled
  in place: stat 6.4 / 6.7 / 7.3 / 7.3, read 13.3 / 14.0 / 14.5 / 14.5, large read 8.8 /
  13.0 / 13.5 / 13.5. Build wall times (n=5): a cache of those files mounted and left alone
  11177 / 14745 / 14745 / 14745 against an empty one's 3915 / 7217 / 7217 / 7217; filling a
  cache 16095 / 18723 / 18723 / 18723 against filling a tmpfs 3657 / 5797 / 5797 / 5797.
- **Consequence.** At the median virtio-fs took 0.64 to 10.8 ms a small file (stat to
  create), 1,251 to 6,828 times the builder's tmpfs, and 314 and 500 times for the large
  files written and read. Caches live in the guest's filesystems instead (D114): a record's
  layers under the builder's memory took 1.26 to 1.86 times tmpfs for a small file and 0.99
  to 1.03 for the large ones. What stays costly is moving a large cache in and out of the
  builder: mounting the filled cache made a build's median 11.2 s against 3.9 s, filling one
  16.1 s against 3.7 s.
- **Not measured here.** Where that time goes within a build (moving layers over vsock,
  applying them, making a layer of the changes), and what a block device per cache would
  cost instead: the builds that looked were instrumented by hand and are not in the tree,
  so their numbers are not recorded. Both are D114's open item.

### M141. What Docker 29.3.1's BuildKit does with cache mounts

- **Question.** What D114 must hold to: BuildKit v0.28.1 (Docker 29.3.1's) with cache
  mounts across builds, their records as `buildx du`, `system df` and `buildx prune` show
  them, and the GC policy dockerd gives its builder.
- **Method.** `docs/research/measurements/cachefs/buildkit.sh` against `shards-dind`
  (docker:29.3.1-dind, BuildKit v0.28.1), every name holding the run's token, `--output
  type=cacheonly`, only the run's own records pruned; 2026-10-10, the results below from
  one run of the committed script (token `forkt1791629692`). In P7 two builds of one cache
  start 3 s apart: A writes `a` and sleeps 8 s, B writes `b` and sleeps 1 s, each then
  listing the cache and printing when it started and ended; a third build lists it after.
- **Results.**
  - What a step writes in a cache is there for the next build's step, a failing step's
    writes too.
  - `uid=1000,gid=1001,mode=0750` makes the directory `750 1000:1001`; `mode=0700` alone
    `700 0:0`; none `755 0:0`. A root a step gives another owner and mode keeps them
    (`700 7:8` the next build). Another `BUILDKIT_CACHE_MOUNT_NS` finds the cache empty.
  - One id at two targets of one step is one directory (written at one, read at the other).
  - `from=` a stage and `source=/seed` shows that directory with its owner (`755 5:6`);
    `source=/seed/sub` the subdirectory, from the same record.
  - A record: `Mutable: true`, `Type: exec.cachemount`, described as `cached mount /m from
    exec /bin/sh -c stat -c "%a %u:%g" /m with id "/<id>"` (the id after the Dockerfile's
    namespace and its `/`). A cache from something has its source's record as `Parents`:
    `from=`, and `uid`, `gid` or `mode`, which the frontend mounts from a directory of its
    own (`setCacheUIDGID`: `Mkdir("/cache", mode, uid:gid)`, its selector `/cache`). `system
    df -v` lists each with its type, size, shared and in-use.
  - `buildx prune --filter type=exec.cachemount` lists each removed with its ID marked `*`;
    `--verbose` prints no `Type:` line for any record (BuildKit's prune results carry no
    type).
  - A copy of `a.txt` stays `CACHED`, and the step after it, when only `c.txt` changes.
  - `docker buildx inspect`: rule 0 types `source.local`, `exec.cachemount`,
    `source.git.checkout`, kept 48h, max used 25.99GiB; rule 1 kept 1440h, reserved
    188.1GiB, max used 1.466TiB, min free 375.3GiB; rules 2 and 3 the same space without
    the duration, rule 3 for all records: dockerd's `DefaultGCPolicy` for a disk of 2,014
    GiB, as `build/gc.rs` derives it (its unit test reproduces these figures).
  - A step not answered from the cache (`--no-cache`) finds its cache empty, and so does
    the next build's: BuildKit lets go of such a step's cache mounts as it loads the
    definition (llbsolver `detectPrunedCacheID`, the worker's `PruneCacheMounts`). The
    script's first version passed `--no-cache` to every build and found every cache empty.
  - Two builds at once (P7, seconds from the epoch as each step printed them):

    | sharing | A's step | B's step | B listed | A listed | then listed |
    |---|---|---|---|---|---|
    | `shared` | 707 to 715 | 710 to 711 | `a b` | `a b` | `a b` |
    | `private` | 717 to 725 | 720 to 721 | `b` | `a` | `a` |
    | `locked` | 727 to 735 | 735 to 736 | `a b` | `a` | `a b` |

    `shared` is one directory both steps write at once; `private` a record of B's own
    while A holds the first, a later build taking the record whose ID sorts first (A's
    in both runs: BuildKit looks a key's records up in its metadata index's byte order,
    `SearchCacheDir` and cache/metadata `Search`); under `locked` B's step began as A's
    ended (5.4 s into its vertex). A second run of P7 alone (`private` and `locked`) gave
    the same lists. During an earlier run, while two builds of one cache ran, `shards-dind`
    exited (status 2, 09:09:39 UTC); it was started again at 09:10:51, not by this probe.

### M155. What a run's guest keeps of a microVM's memory, on the path a run takes

- **Question.** `shards run -m LIMIT` sizes its microVM so that the guest's MemAvailable
  holds the limit, by a table of what the guest kernel keeps at each size (M117 for arm64,
  M123 for x86_64). Those measured cold boots (`run --kernel … --memory`). A run's VM is a
  template of its size, restored, its guest the run's: does it keep the same? CI found
  `-m 2500m` 184 KiB short on x86_64 (5eb6322).
- **Method.** `docs/research/measurements/guest-memory/measure-run.py`: for each table
  size and each interval's midpoint, as far as half the host's memory, the limit the
  daemon sizes to exactly that VM (resources::memory_mib over the table, read from the
  source), `shards run --rm -m LIMITk alpine cat /proc/meminfo` 30 times (the first makes
  the template, the rest restore it). The overhead is the VM's KiB less MemAvailable; the
  margin, MemAvailable less the limit. arm64: Apple M5 Max, macOS 26.4.1, guest kernel
  6.18.48, 2026-10-10, load average 20 to 57. x86_64: 10 runs a size, on a GitHub
  ubuntu-24.04 runner (AMD EPYC 7763, 4 vCPUs, 15,989 MiB, Linux 6.17.0-1022-azure, KVM),
  guest kernel 6.18.48, revision 41839c7, 2026-10-10 (CI run 38044351202, `diagnostic`).
- **Results.** arm64, KiB, at the table's sizes; each interval's midpoint kept less than
  its end (320 MiB: 47,136 at most; 896: 90,980; 14,336: 527,860):

  | VM (MiB) | cold boot's (M117) | run's n / p50 / p90 / p99 / max | least margin |
  |---:|---:|---|---:|
  | 256 | 43,216 | 30 / 43,632 / 43,752 / 43,784 / 43,784 | −568 |
  | 384 | 47,004 | 30 / 47,700 / 47,700 / 47,700 / 47,700 | −696 |
  | 512 | 50,000 | 30 / 50,488 / 50,488 / 50,488 / 50,488 | −488 |
  | 768 | 86,172 | 30 / 88,668 / 88,668 / 88,668 / 88,668 | −2,496 |
  | 1,024 | 90,784 | 30 / 93,524 / 93,524 / 93,524 / 93,524 | −2,740 |
  | 1,536 | 104,440 | 30 / 106,936 / 106,936 / 106,936 / 106,936 | −2,496 |
  | 2,048 | 113,664 | 30 / 116,160 / 116,160 / 116,160 / 116,160 | −2,496 |
  | 3,072 | 239,288 | 30 / 241,520 / 241,520 / 241,524 / 241,524 | −2,236 |
  | 3,584 | 250,488 | 30 / 252,728 / 252,728 / 252,728 / 252,728 | −2,240 |
  | 4,096 | 261,724 | 30 / 263,956 / 263,956 / 263,956 / 263,956 | −2,232 |
  | 4,608 | 288,068 | 30 / 290,332 / 290,332 / 290,332 / 290,332 | −2,264 |
  | 6,144 | 321,752 | 30 / 324,016 / 324,016 / 324,016 / 324,016 | −2,264 |
  | 8,192 | 367,200 | 30 / 368,980 / 368,980 / 368,980 / 368,980 | −1,780 |
  | 12,288 | 476,612 | 30 / 478,820 / 478,820 / 478,824 / 478,824 | −2,212 |
  | 16,384 | 575,192 | 30 / 577,416 / 577,416 / 577,468 / 577,468 | −2,276 |

  x86_64, KiB, every run of a size alike (n 10, p50 = max), as far as 7,168 MiB, half the
  runner's memory; the least margin is against the table then (M123's), which each size
  past a table size takes from the next one up:

  | VM (MiB) | cold boot's (M123) | run's | least margin |
  |---:|---:|---:|---:|
  | 256 | 50,176 | 48,820 | 1,356 |
  | 320 | | 52,276 | 1,720 |
  | 384 | 53,996 | 54,512 | −516 |
  | 448 | | 57,772 | 9,816 |
  | 512 | 67,588 | 58,348 | 9,240 |
  | 640 | | 112,492 | 12,680 |
  | 768 | 125,172 | 124,564 | 608 |
  | 896 | | 137,452 | 4,440 |
  | 1,024 | 141,892 | 141,724 | 168 |
  | 1,280 | | 151,132 | 4,800 |
  | 1,536 | 155,932 | 156,212 | −280 |
  | 1,792 | | 161,552 | 4,812 |
  | 2,048 | 166,364 | 166,684 | −320 |
  | 2,560 | | 179,648 | 11,080 |
  | 3,072 | 190,728 | 191,496 | −768 |
  | 3,328 | | 296,664 | 12,292 |
  | 3,584 | 308,956 | 305,576 | 3,380 |
  | 3,840 | | 308,896 | 5,076 |
  | 4,096 | 313,972 | 314,056 | −84 |
  | 4,352 | | 336,048 | 1,740 |
  | 4,608 | 337,788 | 337,908 | −120 |
  | 5,376 | | 353,248 | 18,288 |
  | 6,144 | 371,536 | 369,132 | 2,404 |
  | 7,168 | | 389,792 | 27,344 |

- **Consequence.** A run's guest keeps 0.5 to 2.7 MiB more than a cold boot's at every
  size on arm64: `-m` held less than its limit wherever a limit sized its VM at a table
  size, and at some midpoints. Each interval's overhead rises to its end, so the table,
  which takes the overhead of the next size up, is now each size's most on the run's path
  (resources::overhead_kib). Past 16 GiB the slope stays 26 KiB a MiB. On x86_64 a run's
  guest keeps 84 to 768 KiB more than M123's cold boots at six sizes, and up to 9,240
  less at the others: its table is now the run's at each size measured, which also gives
  back what a VM of 256, 512, 768, 1,024, 3,584 or 6,144 MiB took past its need; every
  midpoint keeps less than the next size's. From 8,192 MiB it keeps M123's, not measured
  on a run's path here (a runner has 16 GiB). Past 16 GiB the slope stays 22 KiB a MiB.

### M156. Where a release test build's time goes (CI: the build-time timeouts)

- **Question.** CI's x86_64 musl job timed out at 45 minutes in three runs and aarch64
  macOS's Tests step at 30 in two: the whole musl job had taken 31 minutes on d5726fe
  (2026-10-08) and 43 on feb42d6 (2026-10-09), and its release test build alone 32m25s
  on 2710849. Where does a test build's time go, and what would a test profile save that
  keeps release's optimizations?
- **Method.** `cargo test --workspace --release --no-run --timings` from an empty target
  directory, then the same with `CARGO_PROFILE_RELEASE_LTO=false
  CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16` (what `test-release` sets), Apple M5 Max,
  macOS 26.4.1, 2026-10-10 at 09:51 and 10:01 UTC; each unit's time summed from cargo's
  report. Then CI's test steps on release (dev d258710, run 38044119195) and on
  test-release (41839c7, run 38044351202), started 4 minutes apart.
- **Results.** The two builds, seconds:

  | | release | without fat LTO, 16 units |
  |---|---:|---:|
  | units' time, all 381 | 4,466 | 1,414 |
  | the 129 test binaries' | 3,147 | 456 |
  | shards' own test binary | 284 | 53 |
  | shards' build script (its helpers and guests) | 159 | 83 |
  | wall time | 543 | 203 |

  CI's test steps (the release run's failed, a test or the step's limit):

  | job | release | test-release |
  |---|---:|---:|
  | x86_64-unknown-linux-gnu, Tests | 24m19s, failed | 17m42s |
  | aarch64-apple-darwin, Tests | 30m13s, timed out | 14m03s |
  | aarch64-unknown-linux-gnu, Tests | 14m32s, failed | 9m38s |
  | x86_64-pc-windows-msvc, Tests | 28m26s, failed | 11m31s |
  | x86_64-unknown-linux-musl, lint, test and build | 45m13s, timed out | 43m22s |

  The musl step's 43m22s on test-release: clippy 4m16s, the stack bisection of M159
  2m44s, the test build 15m48s, the tests 9m34s, and the release build of `shards` its
  static check reads 10m28s.
- **Consequence.** Fat LTO with one codegen unit re-optimizes every dependency in each
  of 129 test binaries' links: 70 % of a release test build's time, and shards' own test
  binary alone 284 s on one LLVM thread. Tests build with `test-release` (release's
  optimizations, without fat LTO, 16 units; its guests' `guest-test`); releases and
  every measurement keep `release`. Release's stack guards, which test-release's
  smaller frames no longer hold it to, are tested on release on their own (M159), and
  the musl job's release build runs in a job of its own.

### M157. What huge pages under guest memory buy a cold boot, and what they cost against Firecracker (CI: the envelope's rss_anon)

- **Question.** shards advises guest memory MADV_HUGEPAGE (platform `reserve_ram`), so a
  boot's first touch of each 2 MiB makes all of it resident. Firecracker advises nothing,
  and under THP `always` gets huge pages too; against it, a boot's anonymous memory came
  out 2 MiB more in some runs (the envelope's `firecracker/rss_anon`). Do huge pages buy
  the start path enough to keep, and where does the 2 MiB come from?
- **Method.** GitHub ubuntu-24.04 runners, KVM, THP `always`, defrag `madvise`; the pinned
  guest kernel (vmlinux, loaded at 16 to 40.2 MiB) and the test guest's 3 MiB initrd; a
  128 MiB guest booted to its ready line, its VMM's memory read as the Firecracker
  comparison reads it; arms interleaved, each order turned, n 30 an arm.
  1. Huge pages or none: `cargo bench -p shards --bench firecracker -- --thp-ab all,never`,
     the VM process's THP disabled for `never` (PR_SET_THP_DISABLE, held across exec);
     and a 1 GiB guest writing a byte to each page of 512 MiB, then reading a byte at
     20,000,000 places in it spread by an LCG (testguest `walk`). AMD EPYC 7763,
     revision 0b602c7. An earlier run (Intel Xeon 6973P-C, 41839c7) set the advice by a
     knob instead: MADV_NOHUGEPAGE on all of guest memory, or below 64 MiB alone.
  2. Where: which 2 MiB stretches of guest memory each VMM held, and the 4 KiB pages in
     each, from /proc/PID/pagemap, with THP and with it off for both VMMs; then shards'
     layout changed, against Firecracker: `measurements/guest-layout` (its README).
- **Results.** Huge pages or none, AMD; to ready in ms, memory in MiB, p50 / p90 / p99 /
  max:

  | | all | never |
  |---|---|---|
  | boot to ready | 125.0 / 196.3 / 199.2 / 199.2 | 259.1 / 342.1 / 345.7 / 345.7 |
  | boot rss_anon | 60.2 / 60.2 / 62.2 / 62.2 | 49.5 / 49.9 / 50.0 / 50.0 |
  | 512 MiB written | 335.0 / 336.5 / 337.6 / 337.6 | 1,884.4 / 1,900.3 / 1,914.6 / 1,914.6 |
  | 20,000,000 reads | 208.3 / 210.4 / 218.2 / 218.2 | 230.8 / 234.6 / 241.9 / 241.9 |

  Intel, at p50: boot 71.8 ms with huge pages, 169.6 without, 154.1 without below 64 MiB
  (the boot's touches are low); 512 MiB written in 255.0 ms against 1,197.7.

  Where: with THP off, both guests touched the same pages, 12,675 for Firecracker and
  12,613 for shards at the median (13 boots each), but placed apart: shards put its
  initrd at the first 2 MiB after the kernel (42 to 45 MiB), Firecracker at the top of
  RAM, beside what the guest kernel allocates there from the top down early in boot. Its
  initrd moved to the top, shards' guest touched Firecracker's pages stretch for
  stretch. Under huge pages, every stretch touched is resident whole: in 33 boots each,
  Firecracker held 29 (in 30 boots), shards 30 (in 31); the first stretch holds the boot
  structures, 77 and 78 touched pages.

  Shards' layout against Firecracker v1.17.0, AMD, 6c9db11 (757c980 with the patch), n 30
  an arm; to ready in ms at p50, memory in MiB at p50 and the most:

  | arm | to ready | rss_anon | its most | peak RSS |
  |---|---:|---:|---:|---:|
  | Firecracker | 125.5 | 60.2 | 60.2 | 62.5 |
  | shards as it was | 122.1 | 60.2 | 62.2 | 63.0 |
  | initrd at the top of RAM | 121.7 | 58.2 | 60.2 | 61.0 |
  | first 2 MiB on small pages | 122.1 | 58.3 | 60.3 | 61.1 |
  | both | 119.9 | 56.3 | 58.3 | 59.1 |

- **Consequence.** Huge pages halve a cold boot (2.1 to 2.4 times) and make its first
  touch of memory 4.7 to 5.6 times faster: they stay. The 2 MiB was the initrd's place,
  and the first 2 MiB's few pages held resident whole. The x86_64 VMM now puts the
  initrd at the top of low RAM (boot::load_initrd), as Firecracker and QEMU do, and
  keeps the first 2 MiB on small pages (layout::SPARSE): a boot's anonymous memory is
  56.3 MiB at p50 and 58.3 at most, below Firecracker's least, its peak RSS 3.4 MiB
  below Firecracker's, and its boot time the same. A run's path, a template restored
  from its memory file, maps no huge pages and is not changed. arm64's layout is not
  measured (GitHub's arm64 runners have no /dev/kvm) and stays.

### M158. What a child holds of what its parent lets go of, made there or by a spawner

- **Question.** A child holds every descriptor its parent had at the spawn until it
  execs (M134). The daemon holds a run's published listeners from binding them until its
  network process has them, and its clients' stdio until a VM has the run, while its
  other threads make VMs: a run shorter than such a spawn ends with its port bound, and
  `run --rm -p N` again fails where Docker's succeeds. Either the listeners are bound
  where no child is made (the network process, M134's note), or every child is made by a
  spawner, a process made before any of them existed. Which holds nothing, and at what
  cost?
- **Method.** `docs/research/measurements/spawner`: the main thread lets go of a
  listener, then binds its port again, and of a pipe's write end, then reads for its end,
  again and again, and times each that something still holds, while 4 threads have
  /usr/bin/true made as `shards_ipc::spawn` makes children (posix_spawn, three
  descriptors given, `POSIX_SPAWN_CLOEXEC_DEFAULT`): here, with 0, 100, 300 or 1,000 more
  descriptors open (`direct`), or by a spawner forked before anything else was open,
  sent the three descriptors with each request (`spawner`); and posix_spawn's own time,
  here or there, and the round trip to the spawner alone. 10 s a run, direct and spawner
  in turn, 3 rounds at each count. Apple M5 Max, macOS 26.4.1, 2026-10-10, load average
  17 to 33 for 1,000 more and 40 to 56 for the rest. And what a daemon holds: `lsof` of
  one (a debug build of 5994c0c) with 0, 5, 10 and 20 detached `alpine sleep 600` runs
  going. Then a spawn as its requester sees it, from the request to the child's pid
  (`PACE_US` 5000): 1 or 4 threads, each asking for a spawn every 5 ms, nothing let go of;
  the spawn made here, or by a spawner made from the program anew (`exec-spawner`, as
  the daemon makes `shards spawner`); 10 s a run, in turn, 3 rounds. GitHub runners
  (ubuntu-24.04: x86_64 AMD EPYC 9V74, 4 vCPUs; arm64 Neoverse-N2, 4 vCPUs; load under
  1.1; revision b781615) and the M5 Max (load 50 to 56).
- **Results.** Holds, n held of those let go and the longest, and posix_spawn's p50, by
  round:

  | more | made | listeners held | longest | pipes held | longest | posix_spawn p50 (µs) | round trip p50 (µs) |
  |---:|---|---|---:|---|---:|---|---|
  | 0 | here | 1,703/13,556, 2,293/18,141, 2,714/15,971 | 242 ms | 110/68,872, 111/84,128, 122/81,944 | 49 ms | 277, 231, 213 | |
  | 0 | spawner | 0/15,583, 0/17,491, 0/17,038 | | 0 of as many | | 245, 190, 191 | 7.7, 8.8, 7.4 |
  | 100 | here | 1,728/12,191, 2,894/16,719, 2,590/16,107 | 154 ms | 88/67,463, 107/71,450, 95/60,444 | 127 ms | 293, 219, 235 | |
  | 100 | spawner | 0/15,156, 0/18,560, 0/17,155 | | 0 of as many | | 236, 188, 196 | 7.6, 11.0, 8.0 |
  | 300 | here | 1,760/13,199, 2,773/18,277, 2,256/15,174 | 299 ms | 97/64,589, 136/88,579, 96/62,817 | 125 ms | 325, 298, 288 | |
  | 300 | spawner | 0/17,135, 0/16,594, 0/18,196 | | 0 of as many | | 220, 201, 187 | 7.8, 12.4, 8.3 |
  | 1,000 | here | 3,308/16,592, 2,455/15,951, 2,089/16,263 | 42 ms | 145/63,993, 72/43,629, 65/74,519 | 20 ms | 376, 384, 415 | |
  | 1,000 | spawner | 0/19,430, 0/17,411, 0/18,087 | | 0 of as many | | 187, 165, 167 | 8.7, 38.8, 12.0 |

  posix_spawn's p90/p99/max, and the round trip's, are in the runs' output (n 15,126 to
  24,662 spawns a run; round trips 8,000 a run, p99 30 to 645 µs). The daemon held 29
  descriptors with no run going, and 41, 51 and 71 with 5, 10 and 20: 2 more a run, so
  a thousand more is a daemon of some 485 runs.

  A spawn as its requester saw it, µs, p50 by round, and its p99 at most:

  | host | threads | here | by the spawner | p99 here | p99 by the spawner |
  |---|---:|---|---|---:|---:|
  | x86_64 Linux | 1 | 131, 136, 142 | 166, 168, 172 | 222 | 283 |
  | x86_64 Linux | 4 | 140, 144, 158 | 166, 162, 167 | 606 | 505 |
  | arm64 Linux | 1 | 194, 191, 189 | 222, 218, 214 | 252 | 294 |
  | arm64 Linux | 4 | 223, 220, 222 | 245, 241, 249 | 655 | 626 |
  | M5 Max, load 50 | 1 | 630, 599, 965 | 2,231, 1,584, 1,470 | 10,029 | 17,382 |
  | M5 Max, load 50 | 4 | 1,145, 696, 576 | 2,803, 1,956, 1,968 | 8,615 | 21,138 |

  posix_spawn itself took as long in the spawner as here, paced (x86_64: 119 to 133 µs
  there, 127 to 154 here). With four threads spawning as fast as they could on the
  runners (6c9db11), it took 2 to 2.5 times as long in a spawner, forked or made anew,
  as here (x86_64 256 to 337 µs at p50 against 142 to 147; arm64 432 to 445 against 162
  to 197): contention, which spacing the spawns took away.
- **Consequence.** A child made here held 13 to 20 in 100 listeners let go of while it
  was made, up to 299 ms, and some pipes up to 127 ms; one made by the spawner held none
  of 207,836 listeners and as many pipes. Binding the listeners where no child is made
  would have freed ports alone, not clients' pipes. A spawn through the spawner costs
  two wakeups across processes: on a quiet Linux host 9 to 36 µs more at p50 than one
  made here, and no more at p99 with four at once; on a Mac at load 50, 0.5 to 1.7 ms
  more at p50, and 1.7 to 2.5 times the p99. Few spawns are on a run's path: a `-v`
  run's share process, a cold start's VM and network process; warm runs spawn their
  successors after they start. Every child of the daemon is now its spawner's
  (shards_ipc::start_spawner, architecture.md D31), and
  `published_ports_are_free_at_once_while_the_daemon_makes_vms` holds a run's port free
  once its end is told while other runs make VMs.

### M159. The stack a Rego builtin call takes, on each target CI tests (CI: aarch64-linux-gnu's overflow)

- **Question.** `stack::LEAF`, the stack the policy evaluation keeps for a builtin call,
  was 181 KiB: 177 measured on aarch64-apple-darwin and 181 on x86_64-apple-darwin.
  aarch64-unknown-linux-gnu's test of the builtin corpus, on a thread of that much,
  overflowed (CI, 2026-10-10). What does each target take, on release, which ships, and
  on test-release, which CI's tests now build (M156)?
- **Method.** On each CI target that runs tests, `tests/builtins.rs` built on each
  profile and run with `SHARDS_REGO_LEAF` bisected between 64 and 16,384 KiB, each probe
  a process of its own: the least that passed, the most that failed, and the least run
  three times more (the native jobs). CI run 38044351202, revision 41839c7, 2026-10-10.
- **Results.** KiB a builtin call took at most, the least that passed:

  | target | release | test-release |
  |---|---:|---:|
  | aarch64-apple-darwin | 177 | 81 |
  | x86_64-unknown-linux-gnu | 194 | 103 |
  | aarch64-unknown-linux-gnu | 205 | 65 or less |
  | x86_64-pc-windows-msvc | 193 | 129 |
  | x86_64-unknown-linux-musl | 187 | 95 |
  | aarch64-unknown-linux-musl | 195 | 95 |

  In the native jobs each least passed three runs more. aarch64-linux-gnu's test-release
  passed at the bisection's floor. x86_64-apple-darwin and aarch64-pc-windows-msvc are
  built in CI, not tested; x86_64-apple-darwin's 181 is a Mac's release, as before.
- **Consequence.** LEAF is now 205 KiB, the most a release build takes: at 181,
  aarch64-linux-gnu's evaluation let builtin calls start that its stack could not hold.
  A test-release build takes 46 to 67 % of release's (aarch64-linux-gnu 32 % or less),
  so its tests no longer hold release to the guards measured on it (LEAF here;
  RULE_STEP and the policy thread's STACK likewise): CI runs `tests/builtins.rs`,
  `deep.rs` and `depth.rs` on release too, on every target that runs tests. The larger
  LEAF leaves the deepest policies `depth.rs` holds (OPA's parser's deepest) running on
  the policy thread's 123 MiB on aarch64-apple-darwin, release.

### M160. Deeply nested policies: what OPA and shards' Rego take

- **Question.** How long, and how much memory, do OPA and shards' Rego take on policies
  nested as deep as OPA's parser takes them (closures, else chains, rule chains)? A
  remote build context's policy (D111) may be anyone's: what does a small file cost?
- **Method.** `docs/research/measurements/rego-nesting/run.sh` runs each shape of
  `crates/rego/benches/nesting.json` at its depths through shards' bench
  (`crates/rego/benches/nesting.rs`: parse, compile with buildx's functions, evaluate
  `data.docker.decision` on the policy thread's stack, a warm-up then N = 5 runs, one
  case a process) and through OPA v1.14.1 set up as scripts/rego/bench-opa sets it up,
  run as buildx runs a policy check (`nesting_test.go`), once a case: its deep cases
  take it minutes to hours a run. Apple M5 Max (Mac17,6), macOS 26.4.1 (25E253),
  revision 6e51402, 2026-10-10, on a host shared with other builds (load average 50 to
  112 over the run): the times are high, and OPA's are single runs.
- **Results.** shards: whole check p50 / p90 / p99 / max (n = 5), peak resident; OPA:
  one run, peak resident.

  | shape, depth | shards | shards peak | OPA | OPA peak |
  |---|---|---|---|---|
  | every, x unused, 1000 | 2.1 / 2.3 / 2.3 / 2.3 ms | 10 MiB | 4.07 s | 327 MiB |
  | every, x unused, 5000 | 11 / 20 / 20 / 20 ms | 37 MiB | 662 s | 7790 MiB |
  | every, x unused, 20000 | 146 / 276 / 276 / 276 ms | 122 MiB | | |
  | every, x unused, 99990 | 1.23 / 1.36 / 1.36 / 1.36 s | 521 MiB | | |
  | every, x used, 16 | | | 0.74 s | 38 MiB |
  | every, x used, 20 | | | 13.4 s | 54 MiB |
  | every, x used, 24 | 0.8 / 0.9 / 0.9 / 0.9 ms | 6 MiB | 426 s | 227 MiB |
  | every, x used, 1000 | 119 / 236 / 236 / 236 ms | 35 MiB | | |
  | every, x used, 4000 | 1.64 / 1.96 / 1.96 / 1.96 s | 125 MiB | | |
  | every, x used, 99990 | 4.5 / 9.1 / 9.1 / 9.1 s (1) | 1383 MiB | | |
  | every, printing, 16 | 1.0 / 2.2 / 2.2 / 2.2 ms | 6 MiB | 0.54 s | 29 MiB |
  | every, printing, 4000 | 1.52 / 1.65 / 1.65 / 1.65 s | 208 MiB | | |
  | template failing, 5 | 0.2 / 0.3 / 0.3 / 0.3 ms | 4 MiB | < 0.01 s | 18 MiB |
  | template failing, 16 | | | 0.64 s | 37 MiB |
  | template failing, 20 | 0.4 / 0.5 / 0.5 / 0.5 ms | 5 MiB | 6.92 s | 239 MiB |
  | template failing, 30 | 0.9 / 0.9 / 0.9 / 0.9 ms | 5 MiB | | |
  | else chain, 10000 | 135 / 201 / 201 / 201 ms | 90 MiB | 2.26 s | 75 MiB |
  | else chain, 100000 | 1.84 / 2.15 / 2.15 / 2.15 s | 803 MiB | 371 s | 629 MiB |
  | rule chain, 2000 | 105 / 144 / 144 / 144 ms | 43 MiB | 0.46 s | 50 MiB |
  | rule chain, 10000 | 2.10 / 2.41 / 2.41 / 2.41 s | 195 MiB | 8.80 s | 136 MiB |
  | rule wrap, 2000 | 1.39 / 1.56 / 1.56 / 1.56 s | 402 MiB | 0.80 s | 258 MiB |
  | rule wrap, 10000 | 24.4 / 24.5 / 24.5 / 24.5 s (1) | 3820 MiB | 15.8 s | 5428 MiB |

  (1) The evaluation ends with its stack error ("policy evaluation nests deeper than
  its thread's stack"); OPA answers rule wrap 10000 [20001], and every, x used, true
  where it finished (16, 20 and 24 deep), each level taking it twice as long or more.

  Every case answers as OPA answers (results, or the first error of the same list) but
  for the two marked. 99990 deep with x unused, OPA's memory passed 48 GiB in 64 s,
  when it was stopped (one run outside the harness, watched for its memory).
- **Open.** Shapes both still take time growing with the square or faster, from a
  policy of a few kilobytes; shards' whole check, one run each (the same harness), and
  OPA's (`run`, one run each):

  | shape, size | shards | shards peak | OPA | where |
  |---|---|---|---|---|
  | comprehensions nested, 10 | 0.51 s | 5 MiB | 0.75 s | type check, x3.7 a level |
  | comprehensions nested, 12 | 7.6 s | 5 MiB | 6.8 s | |
  | every and comprehension alternating, 12 | | | 7 min 4 s | local variables: each every statement's walk again rewrites the comprehensions within it, renumbering their every statements |
  | every and comprehension alternating, 16 | 1.1 s | 39 MiB | | |
  | every and comprehension alternating, 20 | 18.0 s | 598 MiB | | |
  | every declaring after the one within, 1000 | 2.5 s | 43 MiB | | local variables: the walk again of each every statement, a later declaration in its scope keeping it from being skipped (quadratic) |
  | every declaring after the one within, 2000 | 7.2 s | 84 MiB | | |
  | print calls, 500 in a body | 9.1 s | 19 MiB | | print rewrite: the outputs of all the expressions before each call, each equality copying the safe set (cubic) |
  | print calls, 1000 in a body | 69 s | 32 MiB | 1 min 53 s | |
  | closures, 500 in a body | 28.6 s | 19 MiB | | safety check: the outputs of the expressions placed, again for each candidate (cubic) |
  | closures, 1000 in a body | 101 s | 33 MiB | 4 min 42 s | |

  OPA's time on the shapes left blank was not measured. These are OPA's algorithms;
  what shards does better on the shapes above it can do on these.
- **Consequence.** shards answers the shapes of the results in time and memory linear
  or near it in the depth, but for rule wrap (its compile quadratic, open) and the
  stack its evaluation has; OPA's times and memory grow with the square of the depth or,
  for nested closures, double with each level. The open shapes stay in `nesting.json`
  for the harness (`--shape NAME --depth N`).

### M150. What a build's proxy costs (D110): its certificates, and a request through it

- **Question.** Under buildx's `exec.proxy` cap every request a step makes goes through
  the build's proxy, each asked of the build's policies, HTTPS through a tunnel the proxy
  serves with a certificate of its own CA. What does that add to a request, plain and
  HTTPS, in-process and from a microVM, and what do the certificates cost: P-256, which
  shards makes, against RSA-2048, which BuildKit v0.33.0 makes (`newCA`, `certForHost`)?
- **Method.** `docs/research/measurements/build-proxy/run.sh` runs three ignored tests,
  release builds:
  - `certificate_costs`: a CA, a host's certificate with its TLS config, and one kept,
    P-256 against RSA-2048 made the same way (rcgen and AWS-LC both), n = 200 each,
    interleaved, wall and the thread's CPU time (`CLOCK_THREAD_CPUTIME_ID`);
  - `request_costs`: a request through the proxy against the same request straight to its
    server, a connection each, n = 1000 each, interleaved: plain HTTP (its question
    answered at once, and by a real policy, `allow if input.http`, evaluated as a build
    evaluates one, on a thread woken as the builder's is), and HTTPS (TLS straight against
    a CONNECT tunnel, the proxy's TLS to the client and its own to the server);
  - `proxy_request_costs_in_microvms`: a `RUN` making 200 requests (testguest's `fetch
    --repeat`, a connection each) to a server on this host, straight through the builder's
    network (no cap) against through the proxy with each request checked by the build's
    policy, twice in turn, timed by the step's client.

  `docs/research/measurements/build-proxy/buildkit.sh` times BuildKit v0.33.0's own
  `newCA` and `certForHost` (Go 1.26.3, RSA-2048) as `certificate_costs` times shards',
  in `shards-dind` (Linux 6.12.76-linuxkit on the same host). Apple M5 Max, macOS 26.4.1,
  revision 0f4df5e, 2026-10-10, load average 26 to 36 (other builds running); earlier runs
  at 90ece32 and 0ab09ef, load 53 to 94, below.
- **Results.** Microseconds, p50 / p90 / p99 / max.

  | certificates (n = 200) | wall | thread CPU |
  |---|---|---|
  | CA, P-256 (shards) | 61 / 73 / 282 / 775 | 62 / 72 / 113 / 118 |
  | CA, RSA-2048 (AWS-LC) | 47372 / 101877 / 250083 / 314010 | 44606 / 85337 / 175681 / 270935 |
  | CA, RSA-2048 (BuildKit's Go) | 50947 / 115109 / 184034 / 226113 | 49879 / 107601 / 173302 / 217373 |
  | host certificate and TLS, P-256 (shards) | 82 / 102 / 237 / 601 | 83 / 99 / 129 / 163 |
  | host certificate and TLS, RSA-2048 (AWS-LC) | 48137 / 95607 / 173877 / 196114 | 44519 / 85993 / 121174 / 121544 |
  | host certificate and TLS, RSA-2048 (BuildKit's Go) | 49226 / 96872 / 221056 / 250485 | 49279 / 96441 / 206518 / 243280 |
  | host certificate kept (shards) | 1 / 1 / 2 / 5 | 1 / 2 / 2 / 5 |

  | a request, in-process (n = 1000) | p50 / p90 / p99 / max |
  |---|---|
  | plain, straight | 137 / 269 / 3121 / 12623 |
  | plain, through the proxy | 177 / 344 / 2638 / 25769 |
  | plain, through the proxy, a policy checking it | 1887 / 2396 / 4851 / 13075 |
  | HTTPS, straight | 408 / 710 / 2946 / 34662 |
  | HTTPS, through the proxy's tunnel | 583 / 963 / 4950 / 28651 |

  | a step's request from a microVM (n = 200) | round 0 | round 1 |
  |---|---|---|
  | straight through the builder's network | 304 / 413 / 570 / 1195 | 353 / 486 / 2649 / 11953 |
  | through the proxy, each checked by the policy | 2610 / 3119 / 6760 / 7012 | 2470 / 3054 / 6151 / 7187 |

  - At load 53 to 94 (three runs), p50s: P-256 CA 52 to 58, RSA-2048 CA 48000 to 104280;
    a request in-process straight 160 to 241, through the proxy 195 to 538, checked 1852
    to 4138; from a microVM straight 580 to 1629, through the proxy and checked 4406 to
    12024. Those runs' P-256 tails reached 44 ms of wall clock where the thread's own CPU
    time never passed 141 µs: the tail is the busy host's scheduling, not the work.
- **Consequence.** P-256 makes a CA in 61 µs and a host's certificate in 82 µs where
  RSA-2048 takes 47 to 51 ms for either, BuildKit's own Go code included: 600 to 800
  times less, and P-256 is the stronger key (128-bit security against RSA-2048's 112, NIST
  SP 800-57 Part 1 Rev. 5, Table 2). A build pays its CA once, as its first step under the
  proxy starts, and each host a step tunnels to its certificate once (again after 23
  hours). The proxy itself adds 40 µs to a plain request and 175 µs to an HTTPS one (the
  tunnel's TLS); checking each request against the build's policies adds 1.7 ms, most of
  what a request through the proxy costs from a microVM (2.1 to 2.3 ms over straight).
  That check compiles the policy afresh, twice (partial, then whole), each on a thread of
  its own, as M127 found of every check; compiling once a build on a long-lived
  evaluation thread is that measurement's open question, now paid per request.

### M165. What a VM in App Sandbox reaches of a template once it is renamed out of its grant

- **Question.** A VM saves a template into a directory it is granted, then serves a run;
  the daemon renames the template into place before the run's command reaches the guest
  (run.rs `settle`, M102). M102 found a file made at the new path refused. What else might
  a VM taken over then try: a descriptor of the directory opened before the rename, the
  same bookmark resolved again (bookmarks track a file, not its path), or a link planted
  before the rename? (D117, audit Q.)
- **Method.** `docs/research/measurements/app-sandbox/reresolve.py`, the probe (`probe.c`,
  `reresolve`) built and signed as `run.sh` signs it: the probe resolves its directory
  bookmark, makes a file, opens a descriptor of the directory and plants a link to
  `/etc/hosts`; the unsandboxed parent renames the directory; the probe makes, rewrites
  and links through the descriptor, then resolves the bookmark again, starts its scope,
  and makes and rewrites at the new path. Under `$HOME` and the per-user temporary
  directory. macOS 26.4.1, Apple M5 Max, 2026-10-10.
- **Results.** In both places: through the descriptor after the rename, every make,
  rewrite and link refused (EPERM); the bookmark resolved again names the new path
  (stale 1) and its scope starts (1), yet every make and rewrite there is refused (EPERM).
  The link planted before the rename is in the renamed directory.
- **Consequence.** The seal holds: a VM process loses its template at the rename, by any
  means it holds. What it planted before stays for every later reader, and on Linux what
  it made before its second Landlock layer stays alike. So nothing that reads a template
  may follow a link or wait on a FIFO in it, and the daemon's own record of a template
  lives outside it (D117).
- **Cost of reading a template so.** vmm `what_opening_beneath_costs`: a snapshot's four
  files (pointer, state, memory, working set) opened by path as before, and beneath its
  directory (each directory below it and each file checked by `lstat`, the file opened
  `O_NOFOLLOW` and checked again), alternately, n = 20,000 each, load average 45–56.
  Microseconds a restore's four files, p50 / p90 / p99: by path 42.6 / 77.1 / 149.8;
  beneath 55.1 / 97.6 / 201.1 (a second run: 41.1 / 71.2 / 206.0 against 53.4 / 90.6 /
  284.6). About 12 µs more at the median. A first form, which opened the directory again
  for each file and walked it with `openat`, cost 107.5 µs at the median and 1.0 ms at p99,
  and was not kept.
- **End to end.** `build-ab/ab.py` over c1c2525 and a5a378c's release builds, each its own
  home with busybox:1.36 and its template saved, `run --pull never busybox:1.36 true`,
  n = 200 a side, interleaved; microseconds, wall p50 / p90 / p99 / max:
  - pooled (the restore off the run's path), load 68–76: old 18,923 / 41,750 / 73,092 /
    85,574, new 20,658 / 42,896 / 69,357 / 105,235; paired median new − old +1,681, 95%
    [−2,686, +4,007];
  - `SHARDS_POOL=0` (each run restores its own VM, its launch and grants included), load
    59–70: old 93,214 / 153,079 / 203,267 / 220,727, new 90,680 / 144,029 / 196,573 /
    203,325; paired median +6,741, 95% [−13,588, +22,085].

  Neither distinguishable from nothing at this load, whose noise is a thousand times
  the 12 µs measured above.

### M166. How long a VM process takes to start, and what it waits for

- **Question.** VM processes sometimes asked nothing for tens of seconds: the daemon's
  "VM grant stall" (60 s, READY_TIMEOUT), and a builder that "did not start" in 30 s
  (BOOT_PATIENCE), both under the load of parallel builds. Where does a VM process's
  start go, and what does it wait for? (D117, audit Q.)
- **Method.** `docs/research/measurements/vm-launch/launch.py`: `shards-vm run --kernel K
  --grants 3`, spawned with a socket at descriptor 3, timed from its spawn to the first
  byte of its first request for access, which is the first thing it does once it runs
  (vm_run.rs `start` → `confine`); then ended. The signed VM process of this build
  (App Sandbox, the hypervisor entitlement), as launched before, or a new copy of it
  for each launch (the same bytes, a path and file of its own, as each new build's
  helper is); W at once. `stuck.py` reads the task info (proc_pidinfo PROC_PIDTASKINFO)
  of a launch while it waits. The security daemons' log (`log stream`, syspolicyd,
  amfid, secinitd) during three new copies' launches. Load average 50–65 throughout from
  other work on the host, the norm here. macOS 26.4.1, Apple M5 Max, 2026-10-10,
  revision a17bc1e.
- **Results** (ms: p50 / p90 / p99 / max).

| Launches | n | W | p50 | p90 | p99 | max |
|---|---|---|---|---|---|---|
| launched before | 400 | 4 | 17.1 | 32.7 | 82.0 | 87.4 |
| launched before | 10,000 | 16 | 21.4 | 35.6 | 59.5 | 134.7 |
| a new copy each, first launch | 3 | 1 | 202.6 | | | 205.6 |
| the same copies, second launch | 3 | 1 | 10.6 | | | 11.7 |
| a new copy each, first launch | 100 | 4 | 485.7 | 911.6 | 1,127.7 | 1,127.7 |
| a new copy each, first launch | 128 | 64 | 8,610.6 | 10,455.8 | 10,878.6 | 10,894.2 |
| the same copies, second launch | 128 | 64 | 1,805.3 | | 2,687.8 | 2,710.6 |
| one new copy, launched by all at once (10 rounds) | 160 | 16 | 203.0 | 234.0 | 241.0 | 246.8 |
| launched before, while 192 new copies (64 at once) were assessed | 100 | 1 | 14.5 | 1,613.1 | 2,918.5 | 2,918.5 |

  - A new executable's first launch: amfid finds it ad-hoc signed; syspolicyd scans it
    (Gatekeeper: a network lookup of its notarization, about 30 ms, then XProtect's
    analysis), about 120 ms in all; secinitd sets up its App Sandbox, 62–72 ms, against
    3 ms on a second launch. The process then runs.
  - While it waits it has run nothing: through 5.1 s behind 48 new copies, 0 user time,
    0 Unix and 0 Mach system calls, one thread; 211 system calls by its first request
    once let run. sample(1) of it shows no stack.
  - New executables are assessed about one at a time (about 130 ms each at 64 at once),
    and launches of one executable at once share one assessment.
- **Consequence.** A VM process's start is the host's to admit, and waits behind every
  new executable the host is assessing, the established VM process's own launches
  included: a backlog of a few hundred, as parallel builds make, reaches the stalls seen.
  A VM the host has not let run is told apart (`platform::launched`) and counts against
  no template; a builder not started says so (D117). The incidents themselves were not
  captured; the daemon's log now attributes the next.

### M145. A container's command as PID 1, and its signals, under Docker and under shards

- **Question.** D115 gives a run's command a PID namespace of its own. What does Docker do
  with a container's command there (its PID, PID 1's signals, `--init`, `--pid`, `stop`,
  exec, restarts, inspect), so that shards does the same, and what did shards' tests of
  its old layout, in which the command was never PID 1, assert that Docker does not?
- **Method.** `docs/research/measurements/pid1/`: `docker.py` runs each case in shards-dind
  (Docker 29.3.1, client and engine, arm64) on alpine:3.22 (`sha256:5291449c…`) and
  linuxserver.io's baseimage-alpine 3.22, an s6-overlay image (`sha256:ab81abc9…`, 13 MB);
  `testguest.py` the containers tests' own scenarios, with their test image built in
  shards-dind from the same files (testguest, its user `app`, its passwd and group);
  `order.py` 20 stops each way, reading the order of dockerd's `stop` and `die` events;
  `ctrlc.py` ^C typed at `docker run -it`; `shards.py` the same cases through a shards
  build. Apple M5 Max, macOS 26.4.1, D115's
  branch, 2026-10-10. Times are wall times of the CLI, Docker's through `docker exec`
  (37 ms alone).
- **Results.**

  | Case | Docker 29.3.1 | shards |
  |---|---|---|
  | `sh -c 'echo $$'` | 1 | 1 |
  | `--init`, `ps -o pid,ppid,comm` | 1 0 docker-init; 7 1 ps | 1 0 init; 2 0 ps |
  | from inside, `kill -TERM 1; kill -KILL 1` | unheard | unheard |
  | `--init`, from inside, `kill -TERM 1`, as root and as uid 1000 | 143 | 143 |
  | `stop -t 2`, PID 1 handling no signal | 2.16 s, 137 | 2.01 s, 137 |
  | `--init`, `stop -t 2` | 0.15 s, 143 | 0.01 s, 143 |
  | `stop -t 5`, PID 1 trapping SIGTERM (`exit 7`) | 0.22 s, 7 | 0.07 s, 7 |
  | `kill -s TERM`, PID 1; then `kill` | running; 137 | running; 137 |
  | exec: `$$`; `/proc/1/cmdline` | 7; the command | 2; the command |
  | `--restart on-failure:2`, `echo $$; exit 3` | logs 1, 1, 1; RestartCount 2 | the same |
  | `--pid host`: `$$`; `/proc/1/root` | 1474; denied | 100; denied (`/proc/2` is kthreadd) |
  | `--pid foo`; `create --pid container:nope` | `--pid: invalid PID mode`, 125; `No such container: nope`, 1 | the same |
  | inspect `Init`, `PidMode`, `{{if .HostConfig.Init}}`: none, `--init`, `--init=false`, `--pid host` | `null ""` unset; `true ""` set; `false ""` set; `null "host"` unset | the same |
  | `sysctl net.ipv4.ping_group_range ip_unprivileged_port_start` | `0 2147483647`; `0` | the same (none before D115) |
  | `ping` with `--cap-drop NET_RAW`, as root and as uid 1000 | answered | answered |
  | default capabilities (`CapEff`) | `00000000a80425fb` | the same |
  | s6-overlay: run; `--init`; `--pid host` | 0 in 3.5 s; 100, `s6-overlay-suexec: fatal: can only run as pid 1`; the same | 0 in 3.4 s; the same; the same |
  | `run -it … sleep 6`, ^C after 3 s (`ctrlc.py`, a host pseudo-terminal) | unheard, 0 after 6.6 s; `--init`: 130 after 3.2 s | the same (`tty.rs`) |
  | `top` under `--init`, CMD | `/sbin/docker-init -- sleep 1000`, `sleep 1000` | `/init`, `sleep 1000` |
  | `--privileged`, `--cap-add SYS_ADMIN` or `NET_ADMIN`, `systempaths=unconfined`, `--sysctl net.ipv4.ip_forward=1`, `-u 200000`, `--pid host` | each accepted, exit 0 | each refused beside an Agentfile's domains (D115) |

  - The containers tests' scenarios, their own testguest and image, under Docker:

    | Scenario | PID 1 | `--init` |
    |---|---|---|
    | `sleep`, `stop -t 1` | 1.24 s, 137 | 0.25 s, 143 |
    | `sleep` with `--stop-signal SIGUSR1`, `stop` | 10.28 s, 137 | 0.23 s, 138 |
    | `-i cat`, `stop` | 10.33 s, 137 | 0.26 s, 143 |
    | `kill`: SIGKILL to itself, then exit 1 | 1 | 137 |
    | events of pause, unpause, rename, `stop -t 1` | rename, kill (15), kill (9), stop, die (137), destroy | |

  - dockerd's event order, 20 stops each: where SIGTERM ends the command (`--init`), `kill
    stop die` 20/20; where the timeout's SIGKILL does (PID 1), `kill kill stop die` 20/20.
    moby's `handleContainerExit` marks the container stopped, which wakes `containerStop`
    to log `stop`, before it writes the container's checkpoint and logs `die`.
- **Consequence.**
  - The command is PID 1 of a namespace of its own; `--init` puts a reaper there, which
    forwards what PID 1 hears, as the command's user; `--pid host` is the microVM's own
    (D115).
  - Thirteen tests asserted shards' old layout: a handler-less command ending of stop's
    signal (143, 138), of a forwarded SIGTERM, of the terminal's ^C, of its own SIGKILL,
    with one `kill` event, within a daemon's shutdown before its stop timeout. Docker, with
    the same program, answers so only under `--init`; those tests now run under `--init`
    where the path has it, and say Docker's PID-1 answer where it has not (`run
    --kernel`), and `a_command_is_pid_1_as_a_containers_is` asserts the PID-1 answers.
  - shards logs `die` before `stop` (since before D115), Docker `stop` before `die`:
    recorded, not D115's to change.
  - Recorded differences, of numbers and names Docker's runtime makes: under `--init` the
    command is PID 2 (Docker's 7, which runc's threads take first) with PPID 0 (Docker's
    1, docker-init being its parent; init, its parent here, is outside the namespace), and
    the reaper is `init` (`docker-init`, shown with its arguments).

### M137. virtio-net's frames through guest memory's guard (audit V10)

- **Question.** virtio-net copied each frame between guest memory and the frame ring by
  memcpy, with no `Access` held: a guest that laid a TX buffer over memory another
  device's worker writes had two host threads race (D29's "not yet"). What does copying
  through the guard cost a frame, and what layout of the copy, and of the takes, costs
  nothing?
- **Method.**
  - `docs/research/measurements/net-copies` (`cargo run --release -- 1000`): one
    process, the two arms alternating a sample at a time, a sample a batch of about
    20 µs. `copy`: memcpy against an `Access`'s copies (held across the batch), 64 to
    65,536 bytes each way, at one place (`hot`) or walking 256 MiB (`spread`). `push`
    and `deliver`: a frame at a time through a real frame ring, each with the one take of
    guest memory its queue work makes, the device's copy before (memcpy, outside the
    take) against the change's (inside it). `take`: a take alone. Three fresh processes.
  - `docs/research/measurements/net-throughput`: `ab.py`, 32 MiB each way through a
    published port, every byte checked, n = 300 an arm a run; `rtt.py`, 200 turns of 50
    round trips of 64 or 1,400 bytes on one connection, every byte checked, n = 10,000
    an arm a run. 13ea35d against the change, each one shards binary carrying its VM and
    network processes, turn by turn interleaved.
  - Apple M5 Max, macOS 26.4.1, 2026-10-10, load average 11 to 15 (other forks' builds
    and tests), the host's usual.
- **What it took.** Each step measured by `net-copies`, the copy or the ring changed as
  said:
  - A take of its own for each copy costs 5.1 to 5.6 ns (`take`). The copy shares the
    take its queue work makes: a TX frame's with the pop of its chain, an RX frame's with
    the return of its chains, so a frame takes guest memory twice each way, as before.
  - Copies of 64 bytes or more go as macOS's memcpy goes (`_platform_memmove`,
    disassembled: pairs of 16-byte registers; loads non-temporal; stores non-temporal
    from 16 KiB; the first bytes and the last 32 unaligned, some bytes written twice), but
    with stores aligned to the cache line, not to 32 bytes: aligned to 32, a 9,000-byte
    delivery from memory the caches did not hold cost +15.9 to +18.5 ns against memcpy
    (four processes); aligned to the line, −25.9 to −0.6 (three). Elsewhere on aarch64 the registers
    are glibc's (sysdeps/aarch64/memcpy.S: LDP and STP of Q registers, nothing
    non-temporal).
  - What a frame of 64 to 1,514 bytes still pays is the take's release after the
    copy's stores: with memcpy also copying inside the take, the arms differ by −0.6 to
    +0.4 ns at 1,514 bytes and −0.2 to +1.6 at 64 (four processes). Holding the take
    until the ring had published the frame, its release after the ring's fence, cost more:
    a delivery +3.0 to +4.2 ns at 64 and 1,514 bytes, +6.4 to +7.0 at 9,000 and +7.8 to
    +16.7 at 64 KiB (three processes); not kept.
- **Results.** The change's copies, frames and takes, ns, one process of three; n / p50
  / p90 / p99 / max, and the paired median [95%], change less before:

  | case | before | the change | difference |
  |---|---|---|---|
  | take | | 1000 / 5.5 / 5.7 / 5.8 / 6.5 | +5.5 [+5.5, +5.5] |
  | copy 1,514 B in, hot | 1000 / 16.9 / 17.7 / 30.2 / 49.8 | 1000 / 14.1 / 15.2 / 27.2 / 46.3 | −2.8 [−2.8, −2.7] |
  | copy 64 KiB in, spread | 1000 / 1000.0 / 1063.6 / 1219.3 / 1875.0 | 1000 / 997.8 / 1054.8 / 1315.8 / 2350.9 | −2.2 [−4.4, +0.0] |
  | copy 64 KiB out, spread | 1000 / 991.7 / 1075.0 / 1400.0 / 2300.0 | 1000 / 989.6 / 1077.1 / 1550.0 / 2433.3 | −2.1 [−4.2, −0.1] |
  | push 64 B, hot | 1000 / 14.9 / 15.7 / 21.8 / 47.5 | 1000 / 17.3 / 18.4 / 29.0 / 54.3 | +2.5 [+2.5, +2.6] |
  | deliver 64 B, hot | 1000 / 13.6 / 17.1 / 23.8 / 29.9 | 1000 / 15.0 / 19.3 / 28.0 / 44.6 | +1.4 [+1.4, +1.4] |
  | push 1,514 B, hot | 1000 / 34.5 / 36.9 / 73.2 / 153.4 | 1000 / 35.5 / 38.4 / 66.3 / 149.5 | +1.0 [+1.0, +1.1] |
  | deliver 1,514 B, hot | 1000 / 33.0 / 35.1 / 40.4 / 94.8 | 1000 / 34.5 / 36.6 / 48.8 / 97.8 | +1.4 [+1.3, +1.4] |
  | push 9,000 B, spread | 1000 / 252.8 / 265.2 / 309.1 / 415.0 | 1000 / 247.8 / 259.0 / 290.0 / 420.0 | −3.9 [−3.9, −3.4] |
  | deliver 9,000 B, spread | 1000 / 311.9 / 334.0 / 449.5 / 685.6 | 1000 / 288.5 / 313.8 / 428.7 / 582.1 | −23.4 [−24.0, −22.7] |
  | push 64 KiB, hot | 1000 / 662.0 / 800.9 / 1013.9 / 1831.8 | 1000 / 600.3 / 788.6 / 1092.6 / 1390.4 | −41.7 [−46.3, −41.6] |
  | deliver 64 KiB, spread | 1000 / 1105.4 / 1144.6 / 1303.9 / 2607.8 | 1000 / 1098.0 / 1132.4 / 1269.6 / 1931.4 | −7.4 [−9.8, −7.4] |

  - Over the three processes, paired medians: copies −19.7 to +2.9 ns (64 bytes −0.1 to
    +0.7); pushes +2.4 to +3.0 at 64 bytes, +1.0 to +1.3 at 1,514, −41.7 to +2.2 from
    9,000 up but for one process's −125.0 at 64 KiB; deliveries +1.1 to +1.5 at 64 bytes, +0.6 to
    +1.5 at 1,514, −25.9 to −0.6 from 9,000 up.
  - End to end, 32 MiB each way, ms:

    | run | 13ea35d | the change | change less 13ea35d, paired median [95%] |
    |---|---|---|---|
    | 1 | 300 / 13.3 / 15.5 / 23.5 / 28.7 | 300 / 13.3 / 15.4 / 26.9 / 29.5 | −0.0 [−0.2, +0.1] |
    | 2 | 300 / 17.9 / 21.2 / 27.0 / 51.1 | 300 / 18.0 / 21.2 / 34.4 / 149.4 | −0.1 [−0.4, +0.3] |

    Run 2's slowest connection of each arm fell in the same turn (289): the host's.
  - Round trips, µs:

    | size, run | 13ea35d | the change | change less 13ea35d, paired median [95%] |
    |---|---|---|---|
    | 64 B, 1 | 10000 / 90.8 / 120.6 / 165.3 / 969.9 | 10000 / 91.0 / 121.7 / 165.5 / 1051.9 | +0.3 [−1.1, +1.1] |
    | 64 B, 2 | 10000 / 90.3 / 115.5 / 155.1 / 416.0 | 10000 / 90.4 / 114.1 / 154.3 / 373.9 | −0.0 [−0.8, +0.7] |
    | 1,400 B, 1 | 10000 / 92.2 / 121.9 / 172.7 / 660.6 | 10000 / 92.4 / 122.9 / 175.2 / 714.2 | −0.2 [−0.9, +0.9] |
    | 1,400 B, 2 | 10000 / 102.7 / 137.0 / 216.0 / 1549.4 | 10000 / 102.6 / 139.6 / 250.4 / 13272.4 | +0.4 [−0.6, +1.3] |
    | 1,400 B, 3 | 10000 / 88.4 / 114.5 / 158.0 / 1266.3 | 10000 / 88.3 / 115.4 / 165.0 / 834.5 | −0.1 [−1.4, +0.7] |

    Run 3's p99, change less 13ea35d, +7.0 µs [−1.6, +15.9], turns resampled; each
    arm's slowest 1% fell in 55 to 58 turns, 22 the same.
- **Consequence.** A frame crosses through guest memory's guard (D29), in the takes its
  queue work made before, by copies within 3 ns of memcpy's time and faster from 16 KiB.
  A frame of 64 to 1,514 bytes costs the device 0.6 to 3.0 ns more, the take's release
  after its stores, which a memcpy inside the take pays alike; from 9,000 bytes on it
  costs less. Neither throughput nor a round trip moved measurably at the median; tails
  went both ways from run to run, and the one tested (run 3's p99) was not resolved from
  zero. Open: a take whose release costs a frame nothing; x86_64's copies (SSE2 moves) on
  x86_64 hardware.

### M136. A share's descriptors, bounded: what it holds, and what a walk costs (audit V09)

- **Question.** A share's server now keeps a directory by its path and identity, its
  descriptor in an LRU of a derived size, and opens it again along its path when it is let
  go. What size does the derivation give, does the server keep to it, and what do a walk
  and a reopen cost?
- **The derivation** (`fs::server::Limits::budget`): the process's limit, raised as Go's
  runtime raises its own (min(hard limit, `kern.maxfilesperproc`) on macOS, the hard limit
  on Linux); less what it holds besides (its standard streams, connections and shared
  directories, counted from /dev/fd once they are open); less `REQUEST_FDS` a server, the
  most a request holds past its room; and no more than a share process's part of the
  system's table (`kern.maxfiles`, `fs.file-max`, less what is open) with as many share
  processes as runs the user may start, two processes a run (`kern.maxprocperuid`,
  Linux's thread limits). This host: 245,760, 491,520 with 10,754 open, 10,666 processes:
  90 a share process, the table binding (`a_budget_is_reckoned_from_the_hosts_limits`). A
  Linux host whose table systemd leaves unbounded gets its process's limit less what it
  holds.
- **Its basis, pinned.** `tests/fs_dir_budget.rs` sets its process's limit to what it holds
  and 24 more, then has a guest walk 584 directories, work in ones let go long since, and
  fill its handles: every request is answered (it failed EMFILE at directory 25 before),
  and the worst
  request, a RENAME between two directories let go with the server at its room, needs all
  of `REQUEST_FDS` = 3: at 2 the kernel refuses it (mutation-checked). What a request holds
  at most: the first directory, while the second's path is walked a component at a time
  (two at once; one on Linux, where openat2 walks it).
- **Method.** `virtio-fs-audit`: `--case mktree` made a tree of 100,000 directories (ten
  under each, five levels); `run.py --cases walk --runs 6` walked it as `find` does, each
  directory looked up, opened, listed in pages of 4096 bytes and released, depth first,
  each visit timed, old (ebc464a's server, a descriptor a directory until forgotten,
  245,760 of room) and new (da05f52, its budget of 90) alternating in fresh processes;
  `reopen.py --runs 10` timed a GETATTR of a directory held, and of one let go between
  requests (room for one, so no ancestor held either) and opened again from the root, at
  depths 1, 4 and 16. Apple M5 Max, macOS 26.4.1, 2026-10-09, load average 58.
- **Results.** Microseconds; n / p50 / p90 / p99 / max, pooled, and the paired difference
  of the processes' medians with a bootstrap 95% interval:

  | case | old or held | new or let go | difference |
  |---|---|---|---|
  | walk, a directory's visit | 600000 / 34.83 / 64.62 / 433.46 / 314972 | 600000 / 37.25 / 67.96 / 346.46 / 247113 | +2.12 [0.92, 8.38] |
  | reopen, depth 1 | 20000 / 7.62 / 11.04 / 36.62 / 5575 | 20000 / 16.88 / 21.79 / 110.50 / 11505 | +8.83 [8.50, 10.21] |
  | reopen, depth 4 | 20000 / 8.08 / 10.71 / 40.46 / 6199 | 20000 / 42.75 / 63.00 / 1059.42 / 17718 | +34.42 [29.42, 37.71] |
  | reopen, depth 16 | 20000 / 10.38 / 14.83 / 46.54 / 14067 | 20000 / 176.29 / 324.04 / 7362.08 / 81752 | +165.50 [164.00, 170.54] |

  - A walk took 6.5 s new; the old server held 100,000 descriptors at its end, the new 90.
- **Consequence.** A guest walks any tree within 90 descriptors, at 2.1 µs a directory
  more (6%): its directory is opened when first used, not at its lookup, checked by an
  fstat, and one let go closed. A reopen walks its path from the nearest directory held,
  8 to 10 µs a component on macOS when none above it is held, which the LRU makes rare.
  The guest's open files and directories are no longer bounded by it (2026-10-10, D38):
  their descriptors are the cache's too, opened again by path when let go, and only pins,
  descriptors no path could open again, count against the budget, so a guest holds as many
  as its own kernel lets it. What a workload pays past the cache, a reopen for each file
  read again, is unmeasured end to end; Linux's openat2, one call for a whole path, is
  unmeasured here.

### M167. What compiling a seccomp profile costs, by guest architecture (D42)

- **Question.** Seven Agentfile E2Es failed a gate together, each a `shards top` past its
  30 s while its run started; rerun, alone and together, they passed. Where does a run's
  start go before its VM, and what does compiling the filters it starts under cost?
- **Method.**
  - The start traced: one run of `agents_reach_only_the_unix_sockets_granted` (0b98e01)
    with marks in the daemon's log from its request to its VM's start, and the tests'
    `top` looks timed (both temporary, not kept); the seven tests three times each, n =
    30 runs, at load average 64 to 75 before the change and 78 to 90 after.
  - `docs/research/measurements/seccomp-compile` (`cargo run --release -- N`):
    `shards_seccomp::compile` of Docker's default profile for Docker's default
    capabilities and Linux 6.18, a sample one whole compile, the architectures
    alternating a sample at a time; built against 0b98e01's crate (n = 20) and the
    change's (n = 200), back to back.
  - `first-run.py`: a daemon's first run, `shards run --rm alpine:3.20 true` through a
    daemon just started, which compiles its filter as each daemon's first run does; 0b98e01
    and the change, each one release `shards` and a home whose template was saved, turns
    interleaved, n = 40 an arm a round, two rounds.
  - Apple M5 Max, macOS 26.4.1, 2026-10-10, load average 58 to 94 (other sessions' builds
    and tests, and a VM at 15 cores), the host's usual.
- **Results.**
  - The traced start: `run::prepare` took 1 ms; compiling the run's own filter
    (`setup::security_setup`) 2.36 s and the domains' two (`setup::domain_seccomp`)
    4.42 s; the VM was ready 59 ms after its spawn. Every command naming the container
    waited the while, its record written once the setup is done (`resolve` waits for it);
    the tests' first `top` that answered ended 7.45 s after the run was asked for at the
    median (n 30, p90 8.76, p99 and max 9.01 s), 0.37 s after the change (p90 0.61, p99
    and max 0.73 s), the two compiles then 12 ms and 11 ms.
  - The compile, sampled (macOS `sample`, every millisecond): its time in `section`,
    testing each number up to the stub's line, which on arm64 includes 32-bit Arm's
    private syscalls at 0x0f0001 to 0x0f0005 (`breakpoint`, `cacheflush`, `set_tls`);
    each test scanned the tables, 1,967,136 lookups for Docker's default profile. Read
    off the tables, 1,607.
  - Compiles, n / p50 / p90 / p99 / max:

    | arch | 0b98e01 | the change |
    |---|---|---|
    | amd64 | 20 / 2.56 / 4.32 / 7.39 / 7.39 ms | 200 / 1.27 / 3.26 / 20.74 / 24.49 ms |
    | arm64 | 20 / 1,722 / 1,870 / 1,872 / 1,872 ms | 200 / 1.35 / 2.46 / 14.61 / 23.75 ms |

    The same 660 programs, byte for byte, before and after: the 33 cases on both
    architectures, under Linux 4.14, 5.10, 6.1, 6.18 and 7.0, in both modes.
  - A daemon's first run, ms, n / p50 / p90 / p99 / max, and the change less 0b98e01,
    paired median [95%]:

    | round | 0b98e01 | the change | difference |
    |---|---|---|---|
    | 1 (load 69 to 74) | 40 / 2,582 / 3,559 / 5,210 / 5,210 | 40 / 136 / 199 / 248 / 248 | −2,345 [−2,723, −2,190] |
    | 2 (load 73 to 76) | 40 / 1,983 / 4,045 / 4,123 / 4,123 | 40 / 125 / 194 / 269 / 269 | −1,791 [−2,513, −1,428] |

    The command's own time in the guest moved +0.04 ms [−0.29, +0.40] and −0.05 ms
    [−0.45, +0.15].
- **Consequence.** A compile takes 1.3 ms at the median on either architecture: where the
  kernel's syscalls begin and end is read off its tables, which `kernel_has` now
  bisects. On arm64 each daemon's first run had paid 1.7 s or more for its filter, an
  Agentfile's three times, and each build its builder's, before its first step. The seven
  tests' timeouts fit this: a `top` waits for its run's setup, by design, and the
  compiles were nearly all of it (traced above); the gate's load was not recorded. The
  setup now takes milliseconds. What else a first run's 125 to 136 ms holds is unmeasured
  here.

### M168. What an empty disk costs a microVM's restore (D119)

- **Question.** `--network container:NAME` puts a joiner's image on a join disk that every
  microVM carries, empty until a container joins (D119). What does a restore pay for a
  disk it carries?
- **Method.** `docs/research/measurements/join-disk/restore.py`: a template saved from
  alpine:3.20's EROFS image for each count of empty (0-byte) read-only virtio-blk disks,
  then `shards restore TEMPLATE -- /bin/true` 200 times each, the counts interleaved, their
  order rotating each round; wall clock spawn to reap, and the timing line's
  `released_us` (the VM process's start to its vCPUs' release). Release build of da38223.
  Apple M5 Max, macOS 26.4.1, 2026-10-10, load average 73 to 80.
- **Results.** µs, n / p50 / p90 / p99 / max, and the change against no disk, paired by
  round, median [95%]:

  | round | disks | released | less no disk |
  |---|---|---|---|
  | 1 | 0 | 200 / 4,925 / 22,033 / 41,737 / 42,573 | |
  | 1 | 1 | 200 / 5,058 / 23,241 / 41,798 / 42,625 | +95 [−304, +383] |
  | 1 | 4 | 200 / 4,881 / 28,657 / 56,198 / 71,374 | +117 [−320, +369] |
  | 1 | 16 | 200 / 5,444 / 22,538 / 52,826 / 59,929 | +642 [+226, +994] |
  | 2 | 0 | 200 / 5,051 / 19,659 / 50,869 / 108,105 | |
  | 2 | 1 | 200 / 5,241 / 24,865 / 191,472 / 249,620 | +15 [−160, +299] |
  | 2 | 16 | 200 / 6,539 / 29,605 / 102,775 / 259,328 | +1,122 [+790, +1,440] |

  Wall clock, p50 37 to 39 ms throughout, no difference resolved but 16 disks' in round
  2 (+2,507 µs [+210, +4,445]).
- **The join disk as built**, against none, the same harness (`join` against `0`), n =
  300 each a round, load 63 to 74, µs:

  | round | variant | released | difference |
  |---|---|---|---|
  | 1 | none | 300 / 8,863 / 24,061 / 43,186 / 59,260 | |
  | 1 | join disk | 300 / 7,762 / 21,906 / 42,614 / 64,222 | −513 [−1,682, +120] |
  | 2 | join disk | 300 / 8,157 / 23,586 / 45,141 / 49,051 | |
  | 2 | none | 300 / 8,327 / 20,779 / 42,636 / 59,214 | +330 [−370, +883] (none less join) |

  Wall clock: −365 µs [−3,886, +2,639] and −312 [−1,932, +2,321] (none less join).
- **Consequence.** A file-backed disk costs a restore about 40 to 70 µs, resolved only
  where there are 16; one is within the noise at this load. Each is a file the restore
  reopens through the grants broker on macOS. The join disk, with no file until a
  container joins, cost no restore anything resolved in either round, its medians the
  lower: every microVM carries one (D119).

### M169. What dockerd does with another container's PID and network namespaces (D119)

- **Question.** `--pid container:NAME` beside `--network container:NAME`: what Docker keeps,
  shows and does, for a joiner in its provider's microVM to do the same.
- **Method.** `docs/research/measurements/docker-shared-namespaces/probe.sh`, run in a
  Docker-in-Docker container (docker:29.3.1-dind, dockerd 29.3.1, its own containerd),
  alpine:3.20, its containers `sh-p119-*`, removed at the end. Apple M5 Max, macOS 26.4.1,
  Docker Desktop's VM, 2026-10-10.
- **Results.**
  - Both modes are kept by the other container's ID, as it was made:
    `HostConfig.NetworkMode` and `HostConfig.PidMode` are `container:` and its 64-digit ID
    (moby daemon/daemon_unix.go, adaptSharedNamespaceContainer). Renamed, the provider
    holds: `restart` of the joiner after `rename` of the provider runs it.
  - Sharing its PID namespace, the joiner sees the provider's processes, its PID 1 the
    provider's command (`ps`: 1 `sleep 300`, 7 `sleep 301`).
  - `top` lists each container's own processes alone: the joiner's, its `sleep 301`; the
    provider's, its `sleep 300`, not the joiner's, which are in its PID namespace.
  - `--init` with another's PID namespace is kept (`HostConfig.Init` true) and not given:
    no docker-init in the namespace, the command `sleep 302` as it was (moby
    daemon/oci_linux.go gives it only to a PID namespace of the container's own).
  - What a joiner sharing it started is killed as its command ends:
    `sh -c 'sleep 1000 & sleep 2'` exited 0 and left `[sleep]`, a zombie of the provider's
    PID 1, which reaps nothing.
  - `--pid container:X` alone runs in X's PID namespace with a network of its own.
  - Refused, all with status 125: `--pid container:sh-p119-nope`, "docker: Error response
    from daemon: No such container: sh-p119-nope" as the container is made; one made and
    never started, "docker: Error response from daemon: failed to join PID namespace:
    container 4acb55f1…(its ID) is not running" as it starts; `--pid container:` and
    `--pid bogus`, the CLI's "docker: --pid: invalid PID mode".
- **Consequence.** shards keeps both modes by ID as dockerd does, lets a joiner share its
  provider's PID namespace (or the microVM's own, `--pid host`), lists `top` by cgroup,
  takes `--init` there without giving it, and kills what the joiner leaves (D119).
  `--pid container:X` alone is refused: X's PID namespace is in X's microVM, and a
  network of its own there is not built (D115).

### M170. What an empty device costs a microVM's memory (D119)

- **Question.** M168 measured what an empty disk costs a restore's time. What do the join
  disk and a join share (virtio-fs, served by none) cost a microVM's memory, counted as
  memory per VM is counted ([M30]: `phys_footprint`, not RSS)?
- **Method.** `docs/research/measurements/join-disk/restore.py`, as M168's, and two
  templates of each machine (`#a`, `#b`), for what templates of one machine differ by:
  none (`0`), a join disk (`join`), a join disk and a join share (`joinshare`), a join
  disk and a run's shared directory (`share`); 150 restores each of
  `shards restore TEMPLATE -- /bin/true`, interleaved. The VM process's
  `phys_footprint` at the timing line (`footprint_kib`, added to it), its peak RSS there
  (`rss_kib`) and over its life (wait4's `ru_maxrss`). Release builds of bede221 with the
  device, Apple M5 Max, macOS 26.4.1, 2026-10-10, load 21 to 65.
- **Results.** KiB, the median difference against the first template of a machine
  without the device [95%]:
  - Memory per VM: none resolved. The join disk +0 [−32, +16] and +0 [−16, +16]; the join
    share with it +0 [−16, +32] and +0 [−16, +32]; two templates of one machine differ by up
    to 32. With the share's queue workers started as it is activated, or at its first
    notification: +0 to +32 against templates 16 to 32 apart, alike.
  - RSS: each device +64 to +208, a run's shared directory alike (+176 [+176, +208]),
    where two templates of one machine differ by up to 96. These are the pages the guest's
    drivers allocated as it booted, kept in the template and mapped from it, shared by every
    microVM restored from it; `phys_footprint` leaves them out. With 16-entry queues, a
    share's RSS was +64 [+48, +80] where with 256 it was +144 to +176: the guest's queue
    allocations.
  - Restore time: none resolved but one round of the join share, +94 µs [+36, +142]; the
    rest −59 to +42.
- **Consequence.** Every microVM carries a join share as it carries a join disk (D119), at
  no resolved memory per VM. A share's workers start as it is activated: the lazy start
  tried saved nothing resolved, and was left out. RSS, which counts a template's shared
  pages in each process, is not what a microVM costs.

### M171. dockerd's endpoint sysctls, as a container sees them (D46, for its open item)

- **Method.** `docs/research/measurements/endpoint-sysctls/probe.sh`, Docker 29.3.1 in
  `shards-dind`, `alpine:3.22`, each container `--rm` and unnamed; 2026-10-10.
- **Found.**
  - `--network name=bridge,driver-opt=com.docker.network.endpoint.sysctls=net.ipv4.conf.IFNAME.log_martians=1`:
    eth0's file reads 1; `ifname` in lower case and two in one option alike; an IPv6 one
    (`disable_ipv6`) alike.
  - The endpoint's comes after the container's own: `--sysctl net.ipv4.conf.all.forwarding=1`
    with the endpoint's eth0 `forwarding=0` leaves all 1, eth0 0; the other way round, 0
    and 1.
  - A key with no file fails the start (125): "failed to set up container networking:
    failed to add interface vethNNNNNNN to sandbox: /proc/sys/net/ipv4/conf/eth0/nonexistent
    is not a sysctl file"; a value the kernel refuses: "… unable to write to
    '/proc/sys/net/ipv4/conf/eth0/log_martians': write /proc/sys/net/ipv4/conf/eth0/log_martians:
    invalid argument" (libnetwork osl/interface_linux.go, setSysctls).
  - On `none`, which has no interface, an endpoint sysctl is ignored: the run succeeds.
  - An interface's key given as `--sysctl` (`net.X.Y.eth*.Z`) is refused as the container
    is made, `create` (1) and `run` (125), on `bridge` and on `none`, for eth0 and eth1
    alike: "interface specific sysctl setting \"KEY\" must be supplied using driver option
    'com.docker.network.endpoint.sysctls'" (moby daemon/server/router/container,
    handleSysctlBC, API ≥ 1.48), before any of the daemon's own checks.
- **Consequence.** Built (D46): init writes each on eth0 after the run's own sysctls, in
  libnetwork's words, the daemon passing them on and refusing an interface's key given as
  `--sysctl` first (`an_endpoints_sysctls_are_its_interfaces`).

### M172. Whether GitHub's Windows runners can run a WHP partition

- **Method.** `docs/research/measurements/whp-probe/probe.ps1` by its workflow
  (`whp-probe.yml`), run 38097837105 at dev 76ef3913, 2026-10-11: the processor as
  Windows reports it, the optional features' states, WHvGetCapability's
  WHvCapabilityCodeHypervisorPresent and WHvCreatePartition, before and after enabling
  HypervisorPlatform without a restart.
- **Found.**
  - windows-2022 (10.0.20348, AMD EPYC 7763) and windows-2025 (10.0.26100, AMD EPYC
    9V74): HypervisorPlatform, Microsoft-Hyper-V and VirtualMachinePlatform enabled as
    found; HypervisorPresent 1; WHvCreatePartition S_OK. Nested virtualization is there.
  - windows-11-arm (10.0.26200, Cobalt 100): HypervisorPlatform disabled, enabling it
    needs a restart (RestartNeeded True), which a hosted runner cannot take;
    HypervisorPresent 0 and WHvCreatePartition 0xC0351000 before and after.
- **Consequence.** A WHP backend can be tested on real VMs in CI on x86_64 Windows; on
  arm64 Windows it can be built and linted, and run on a host of its own.

### M173. dockerd's MAC addresses, as a container sees them and inspect shows them (D46)

- **Method.** `docs/research/measurements/mac-address/probe.sh`, Docker 29.3.1 in
  `shards-dind`, `alpine:3.22`, each container removed after; 2026-10-10.
- **Found.**
  - Given, by `--mac-address` or the endpoint's `mac-address`, it is eth0's
    (`/sys/class/net/eth0/address`), in lower case whatever the case given; the dash
    (`02-42-ac-11-00-91`) and dot (`0242.ac11.0090`) forms, and spaces around it, alike.
    Of eight octets (`02:42:ac:11:00:99:00:01`), eth0 has the first six.
  - Malformed, the CLI refuses it (125): "docker: 02:42:zz:11:00:99 is not a valid mac
    address", and an endpoint's alike ("bad is not a valid mac address").
  - Multicast (`01:00:5e:00:00:01`) or all zeros, the start fails (125): "failed to set up
    container networking: failed to add interface vethNNNNNNN to sandbox: error setting
    interface \"vethNNNNNNN\" MAC to \"01:00:5e:00:00:01\": cannot assign requested
    address".
  - On `none`, `host` and another's network (`container:`), it is taken and the run
    succeeds; on `none` the container has no interface but lo and the kernel's tunnels.
  - None given, each container has a random, locally administered unicast one
    (`02:f9:a0:2a:00:52`, `56:11:20:27:a1:3e`, `da:47:b2:26:d7:52`), a new one each start.
  - inspect's `NetworkSettings.Networks.*.MacAddress`: a given one as Go's
    `HardwareAddr.String()` writes it (lower case, colons, every octet, an EUI-64's
    eight) while the container is created, running and restarted, empty once it has
    exited; a made one empty while created and exited. On `none` and `host`, the given
    one, created and running. `Config` has no MacAddress. `network inspect` shows a
    member's given one, an EUI-64's eight octets.
  - Two containers given one MAC on one user network both run, and neither reaches the
    other (`nc` to the other's listener: 0 of 3); two of different MACs do (3 of 3).
- **Consequence.** Built (D46): a run's own MAC, given or made, eth0's and its network
  process's, shown by inspect as dockerd shows it but where dockerd shows one no interface
  has; two given one MAC reach each other, as on dockerd's bridge they do not.

### M174. What Docker's AppArmor profile leaves a container of mounts (D121)

- **Method.** `docs/research/measurements/docker-apparmor-mount/probe.sh` by its workflow
  (`docker-apparmor-probe.yml`), run 38101649596 at dev 329c5000, 2026-10-11, on GitHub's
  ubuntu-24.04 (Linux 6.17.0-1022-azure x86_64) and ubuntu-24.04-arm (aarch64): Docker
  28.0.4, AppArmor enabled, docker-default enforced; each step's return and errno from
  python's ctypes in `python:3.13-alpine`.
- **Found**, alike on both:
  - Default capabilities: mount, umount2, fsopen and open_tree's clone EPERM (Docker's
    seccomp profile).
  - `--cap-add SYS_ADMIN`: mount EACCES (docker-default's `deny mount,`); fsopen and
    open_tree's clone succeed; umount2 of a mount (MNT_DETACH of /etc/hostname) succeeds.
    BusyBox's mount says "mount: mounting none on /mnt/t failed: Permission denied", 255.
  - With `seccomp=unconfined` too: the same. Without the capability, `seccomp=unconfined`:
    mount EACCES, since the kernel asks the LSM before it checks the capability (Linux
    fs/namespace.c path_mount: security_sb_mount, then may_mount); the rest EPERM.
  - `apparmor=unconfined` with the capability, and `--privileged`: every step succeeds.
  - An unknown profile (`apparmor=no-such-profile`): the start fails, 127, runc unable to
    apply it.
  - Detached mounts (run 38105405797 at b7d7af90, both runners alike): with
    `--cap-add SYS_ADMIN`, fsopen and fsmount make a detached tmpfs, and a file made in it
    through its descriptor (openat) is made; move_mount of it, and of open_tree's clone,
    EACCES. With `apparmor=unconfined` too, both move_mounts succeed. With the default
    capabilities, fsopen and open_tree's clone EPERM.
- **Consequence.** docker-default refuses mount(2) of these, and lets the new mount API
  through where the capability is given: a detached mount is made and used, and only
  attaching it to the tree is refused. shards' filter refuses both (D121): a detached
  mount (fsopen, fsmount, then files through its descriptor) needs no mount point, and
  would reach a microVM's own virtio-fs shares past the rule. Without the capability under
  `seccomp=unconfined`, shards' answer to mount(2) is the kernel's EPERM, not AppArmor's
  EACCES: a filter for that errno alone would be a seccomp filter on a container asked to
  have none.

### M175. dockerd's link-local addresses, as a container sees them and inspect shows them (D46)

- **Method.** `docs/research/measurements/link-local-ip/probe.sh`, Docker 29.3.1 in
  `shards-dind`, `alpine:3.22`, each container and network removed after; 2026-10-11.
- **Found.**
  - Given, by `--link-local-ip` or the endpoint's `link-local-ip`, each is on eth0 after its
    address: IPv4's as /16, `scope global`, broadcast 169.254.255.255, with the kernel's
    route 169.254.0.0/16 on eth0 from the first; IPv6's as /64, beside the address the
    kernel makes of the MAC, on a network with IPv6.
  - One not link-local (10.9.8.7, fd00::7, the network's own 172.17.0.200) fails the
    start, 125: "failed to set up container networking: invalid link local IP address:
    10.9.8.7": after "network … not found", the first bad one of several, on `none` too,
    never on another's network. An unspecified one (0.0.0.0) is refused as the container is
    made: "invalid config for network bridge: invalid endpoint settings:\ninvalid
    link-local IP address: 0.0.0.0".
  - IPv6 on a network without it fails the start with 126, docker/cli's status for an error
    that says "permission denied": "… error setting interface \"vethNNNNNNN\" link local
    IPs to [fe80::1234/64]: permission denied". The same address twice: "… link local IPs
    to [169.254.8.1/16 169.254.8.1/16]: file exists", 125.
  - Both ways at once, the CLI's "conflicting options: cannot specify both --link-local-ip
    and per-network link-local IP addresses"; a malformed one is dropped by the CLI, the
    run going on.
  - An IPv4-mapped one (::ffff:169.254.10.1) is on eth0 as 169.254.10.1/16, and inspect
    shows it so; a zone (fe80::1%eth0) inspect keeps.
  - On `none`, taken, and no interface has it; on another's network, taken and nothing of
    the joiner's own has it.
  - inspect's `IPAMConfig.LinkLocalIPs`: as given, created, running, exited and restarted
    alike; `null` where none is given. `network inspect` shows none; /etc/hosts names none.
  - On a user network: from a container with a link-local address of its own to another's,
    reached (`nc`, 1 of 1); from one without, to an IPv4 one, not (1 of 1); to an IPv6 one
    (`fe80::2:10%eth0`), with or without, reached; from off the network, not.
- **Consequence.** Built (D46): each as dockerd gives it, init adding it to eth0, the daemon
  checking it in dockerd's words; members reach each other's as on dockerd's bridge, each
  network process knowing the link-local addresses of its guest and its peers.

### M176. What a run's record of processes holds of short-lived processes, and what an exec's command line costs (D123)

- **Question.** The record listens to the kernel's process events (cn_proc), which name a
  process by its PIDs alone, and read the rest from /proc. How much of a shell's loop of
  processes that end at once does it hold, and what does the guest kernel's patch, which
  sends an exec's command line with its event (resources/kernel/patches/0002), cost an
  exec?
- **Method.** `docs/research/measurements/process-record/measure.sh`: in `alpine:3.22`,
  `sh -c 'for i in $(seq 2000); do /bin/true; done'`, timed in the guest by
  /proc/uptime (10 ms in 2000 execs, 5 µs an exec), then the run's record read
  (`shards read processes`); 20 rounds a kernel, the pinned kernel-6.18.48-9199fc6fad00
  and the patched kernel-6.18.48-4b8900bdffdd (the same bytes, built locally in the
  pinned builder first), interleaved, the order turned each round; one shards-init, this
  decision's. Apple M5 Max, macOS 26.4.1, load average 22 to 28, 2026-10-11. Before,
  with the record as first built (each process's member read from /proc at each event),
  the same loop of 200, three runs.
- **Results.**
  - As first built: of the 201 processes the shell started, 148 to 159 recorded at all;
    of those, 33 to 38 execs without a command line. A process gone by the time its
    event was read had no /proc to name its member, nor a command line.
  - Each process its parent's member, as the fork's event says: every process recorded,
    on both kernels (2003 forks, 2004 exits, every round; none lost).
  - The pinned kernel: 1,881 to 1,992 of the 2,000 execs with their command line; 8 to
    121 (p50 54) without, the process ended before /proc was read, its name alone said
    where it was not yet reaped.
  - The patched kernel: 2,000 of 2,000, every round.
  - An exec, µs, n / p50 / p90 / p99 / max: pinned 20 / 135 / 145 / 150 / 150; patched
    20 / 130 / 135 / 145 / 145. No cost resolved at this resolution.
- **Consequence.** D123: members by inheritance, and each exec's command line from the
  kernel, sent with its event by shards' guest kernel. A kernel without the patch leaves
  /proc, read as the event is, each exec it misses said as such.

### M177. dockerd's `--network host`, as a container sees it and the CLI and daemon take it (D124)

- **Question.** What `docker run --network host` gives a container of its host's network,
  and what of `run`'s options it takes, warns of or refuses, in whose words.
- **Method.** `docs/research/measurements/network-host/probe.sh`, against Docker 29.3.1's
  own dockerd in `shards-dind`, `alpine:3.22`, each container removed after; 2026-10-11.
- **Results.**
  - The container is the host's: its hostname, its `/etc/hosts` (the host's file, line for
    line), its interfaces (`lo`, `eth0`, the bridges, the tunnels); `resolv.conf` the
    host's resolvers, loopback ones kept (`127.0.0.53` stays), as Docker writes one
    ("Generated by Docker Engine", "Based on host file: '/etc/resolv.conf'").
  - A port it listens on is the host's (`nc` from the host, 1 of 1); it reaches the host's
    loopback (1 of 1); a port the host holds fails its bind, "nc: bind: Address in use".
  - `-p 8080:80`: started, 0, and on stderr "WARNING: Published ports are discarded when
    using host network mode"; `-P --expose 80`: started, 0, nothing said.
  - `--hostname nh`: its hostname nh, `/etc/hosts` still the host's (7 lines);
    `--add-host x:10.0.0.1`: `/etc/hosts` still the host's; `--domainname`, alike.
    `--dns 9.9.9.9`: `nameserver 9.9.9.9`; `--dns-search example.org`: `search
    example.org`.
  - Taken and left alone: `--mac-address`, `--link-local-ip`, `--cap-add NET_ADMIN`,
    `--userns host`, `--privileged`.
  - Refused, 125: `--ip 172.17.0.9`, "docker: Error response from daemon: invalid config
    for network host: invalid endpoint settings:\n* user-specified IP address is supported
    on user-defined networks only\n* no configured subnet contains IP address
    172.17.0.9"; `--network-alias a`, the CLI's "docker: network-scoped aliases are only
    supported for user-defined networks"; `--sysctl net.ipv4.ip_forward=1` and
    `net.core.somaxconn=1024`, "docker: Error response from daemon: failed to create task
    for container: failed to create shim task: OCI runtime create failed: runc create
    failed: sysctl \"net.ipv4.ip_forward\" not allowed in host network namespace" (runc's
    validator: every `net.*` sysctl outside a network namespace of the container's own);
    then `none` as a second network, libnetwork's "failed to set up container networking:
    container sharing network namespace with another container or host cannot be
    connected to any other network".
  - inspect: `NetworkSettings.Networks` `{"host":{...}}`, every field empty or null;
    `HostConfig.NetworkMode` `host`; `Config.Hostname` the host's.
- **Docker Desktop**, whose containers run in a Linux VM as a microVM's processes do,
  gives host networking at layer 4 from 4.34 (docs.docker.com/engine/network/drivers/host,
  2026-10-11): both ways, a container's servers reached from the host and the host's
  servers from the container, TCP and UDP; "Processes inside the container cannot bind to
  the IP addresses of the host"; nothing below TCP and UDP.
- **Consequence.** D124: shards gives the same at the same layers, each bind made the
  host's first so that what the host holds refuses it as above; Docker's warnings and
  refusals in its words, runc's without containerd's and runc's prefixes, as every start's.

### M178. What a bind costs on the host's network (D124)

- **Question.** On the host's network each bind(2) of the run's processes waits for the
  host to bind the same port, and each TCP listen(2) for the host's listen. What does
  that cost a server's listener and a client's bind of a port of the kernel's choosing
  (musl's resolver binds one for each query), against the default bridge?
- **Method.** `docs/research/measurements/network-host/binds.sh` with the test guest's
  `binds`: in `alpine:3.22`, 200 TCP listeners at every IPv4 address (std's
  `TcpListener::bind`: bind(2), then listen(2)), then 200 UDP sockets bound there
  (bind(2) alone), each at a port of the kernel's choosing and closed before the next,
  timed in the guest; 10 runs on each network, interleaved, the order turned each round;
  revision a65112b8 with the harness. Apple M5 Max (18 cores), macOS 26.4.1, load
  average 9.0 to 9.4 (other projects' builds running), 2026-10-11.
- **Results.** µs, one run's p50 / p90 / p99 / max, the range over the 10 runs:
  - TCP listener, host's network: p50 240–258, p90 294–336, p99 456–688, max 481–1,237.
  - TCP listener, bridge: p50 1, p90 2, p99 4–15, max 22–36.
  - UDP bind, host's network: p50 150–172, p90 184–235, p99 236–510, max 247–1,257.
  - UDP bind, bridge: p50 0–1, p90 1, p99 2–10, max 4–15.
- **Consequence.** D124: a bind on the host's network is a round trip to the host (init's
  process, the VM process, the daemon and back): a listener two, a UDP bind one. A
  server binds once; a resolver's query on the host's network costs one more round trip
  than the bridge's.

### M179. What a run's guest keeps of its microVM, boot by boot: free pages its CPUs hold (`-m`)

- **Question.** `-m`'s table (`resources::overhead_kib`) says what the guest keeps at each
  VM size, so that the VM holds the limit. CI's x86_64 jobs failed
  `run_limits_a_microvms_resources` at 700m (220 KiB short) and 2500m (1,856 KiB
  short): the table, M155's, came from one runner's boot of each size.
- **Method.** `docs/research/measurements/guest-memory/measure-run.py` by its workflow
  (`guest-memory.yml`): each size's template booted B times (the daemon stopped and the
  home's templates removed before each), N runs a boot, each reading /proc/meminfo and
  /proc/zoneinfo; sizes as far as half the host's memory. x86_64: three GitHub
  ubuntu-24.04 runners a run.
- **Found.**
  - A template's runs keep alike, to the KiB (10 runs a template, three runners, run
    38127711738): its boot is the sample.
  - x86_64, 8 boots a size on each of three runners (run 38131603252, Intel Xeon
    Platinum 8573C twice and AMD EPYC 9V74): a size's boots differed in MemAvailable by
    up to 13.8 MiB (320 MiB: 52,760 to 66,528 KiB kept), all of it MemFree, none in
    meminfo's other fields (Cached, Shmem, slab, KernelStack, PageTables, Percpu,
    AnonPages, AnonHugePages, Mapped).
  - The rest is free pages on the CPUs' lists (run 38133982946: AMD EPYC 9V74 and 9V45,
    Intel Xeon 8370C, one CPU in each guest): 4 to 22 MiB, which MemFree and
    MemAvailable leave out. Linux counts a page free once it is back with the buddy
    allocator (mm/page_alloc.c: `account_freepages` in `__free_one_page`; the per-CPU
    commit adds to `pcp->count` alone); allocations take from the lists first, and one
    that reclaim leaves unmet drains every CPU's and tries again (`__alloc_pages_direct_reclaim`,
    `drain_all_pages`). Counted with MemAvailable, a size's 48 boots differ by 0.3 to
    0.7 MiB.
  - arm64 (HVF; Apple M5 Max, macOS 26.4.1, 4 boots a size, 2 runs a boot, sizes to
    16 GiB, 2026-10-11): a size's boots differed by at most 156 KiB in what the guest
    keeps (292 KiB in MemAvailable); its CPU's list held 4.2 to 9.1 MiB, which M155's
    table, MemAvailable's, counted as kept.
- **Consequence.** What the guest keeps is the VM's KiB less MemAvailable and the listed
  pages; the table is each size's most over every boot measured, the measured midpoints
  among its sizes; `run_limits_a_microvms_resources` counts the listed pages as
  the kernel's allocator does. From 8 GiB, x86_64 keeps M123's cold boots' values, which
  leave those pages out and so keep more.

### M180. A run's first datagram to its gateway waited on ARP, and was lost with its port (D124, D31)

- **Question.** `a_run_on_the_hosts_network_is_on_the_hosts_network` failed once in 10
  runs: a UDP server's answer, relayed from the guest's loopback as the server ended,
  never reached this host (iteration 8 of 12). The VM puts what the guest gave its device
  into the network process's ring before a port is let go (D124) or a run's ports close
  (2026-10-10), so where was the answer?
- **Seen.** The test guest's `udp-serve` read `/proc/net/arp` before and after its
  answer, 80 runs: before, eth0 had no neighbor in each; right after a server at every
  address sent its answer, the gateway's entry was there with flags 0x0 (incomplete,
  `00:00:00:00:00:00`). Linux holds a packet whose next hop's MAC it is asking for in the
  neighbor's queue (net/core/neighbour.c, `__neigh_event_send`), out of the device's TX
  queue, until ARP's answer comes back through the VM and the network process, which
  answer every ARP request with the gateway's one MAC. A port let go first lost it.
- **Method.** `docs/research/measurements/gateway-neighbor/run.sh`, the ignored test
  `answers_lost_without_the_gateways_mac`: a UDP server on the host's network answers one
  datagram from this host and exits, at 127.0.0.1 (its answer relayed by init's
  supervisor) and at every address (its own); each run interleaved with one whose guest
  first deleted eth0's neighbors (CAP_NET_ADMIN), as a guest's were before init gave the
  gateway's MAC; an answer lost if none came in 5 s; 500 runs of each, the host as it was
  (other projects' builds running), then with 16 busy loops. Apple M5 Max (18 cores),
  macOS 26.4.1, 2026-10-11, the release build of the tree merged with this record (the
  harness printed its base, 6722ce77).
- **Results.** Answers lost, of 500 each:

  | load average | relay, MAC known | relay, MAC forgotten | own, MAC known | own, MAC forgotten |
  |---|---|---|---|---|
  | 7.0 to 5.6 | 0 | 2 | 0 | 0 |
  | 5.6 to 48.0, 16 busy loops | 0 | 3 | 0 | 0 |

  Before the harness, an A/B of this tree's init against one built without the entry,
  interleaved, under 16 busy loops and others' builds (load average to 77): the old init
  lost 21 of 500 relayed answers and 1 of 500 of its own; the new, none of either.
- **Why the relay's.** The supervisor sends the relayed answer and lets the port go in
  one turn of its loop, so the letting go races ARP's round trip from the start; a server
  that answers itself exits before its socket's end is told, which ARP's answer mostly
  beats.
- **Consequence.** init gives eth0 its gateway's MAC as a permanent neighbor, IPv4's and
  IPv6's, wherever it sets the gateway (`net::configure`, `readdress`, `address6`, and
  again after `set_mac`, since a new MAC flushes every neighbor, permanent ones too:
  net/core/neighbour.c, `neigh_changeaddr`); the MAC is `shards_abi::gateway_mac`, which
  the network process answers with. `a_runs_gateway_is_its_neighbor_from_its_start` pins
  it. The published UDP flake of 2026-10-06 and 2026-10-10 had this cause too, for a
  run's first answer to a client. What remains of the same kind: a datagram to a member
  of the run's network (on its link, so asked for by ARP), and frames in eth0's queueing
  discipline (pfifo_fast) while the device's ring is full, as a command ends.

### M181. A container joining a host-network container's network, and ports past a container's end (D124, D119)

- **Question.** What dockerd gives a container joining the network of one on the host's
  network (`--network container:NAME`), and whether a port a container on the host's
  network served is the host's to bind again once its end is told.
- **Docker** (`docs/research/measurements/network-host/joiner.sh`, Docker 29.3.1 in
  `shards-dind`, `alpine:3.22`, 2026-10-11):
  - The joiner's names and files are the host's (its hostname, the 7 lines of its
    hosts file, its resolvers, its 16 interfaces); nothing is said on its stderr.
  - A port its listener takes is the host's (`hi` from the host); one the host holds
    fails its bind, "nc: bind: Address in use".
  - inspect: `NetworkSettings.Networks` `{}`, `HostConfig.NetworkMode`
    `container:ID`, `Config.Hostname` the host's.
  - Its provider stopped, it runs on: its first listener still the host's, and one it
    starts then is the host's too.
  - Joining a stopped one: "docker: Error response from daemon: cannot join network
    namespace of a non running container: container nhj-probe-p is exited", 125.
- **Ports past their end, method.** `docs/research/measurements/network-host/release.sh`,
  the ignored test `ports_held_past_their_containers_end`: a server on the host's network
  serves one connection from this host and is killed with its container (`shards kill`);
  its client waited for at once, as a shell waits, then the server's port bound on the
  host; 100 runs alone, then 100 containers joining one such run, each killed while its
  provider runs; quiet (load average 26 to 21, others' builds) and with 16 busy loops.
  Apple M5 Max (18 cores), macOS 26.4.1, the release build of 04e5bc1a.
- **Results.** Ports still held as the port was bound: runs 0 of 100 and joiners 0 of
  100, quiet and loaded. Before the change, the same loop in a test-release build of
  that tree with the VM's letting go removed, under 16 busy loops (load average to 116):
  runs 96 of 100, each free 3 to 10 ms later (the network process's copies, closed only
  as that process ended); joiners 0 of 100 (the daemon's own letting go at a joiner's end
  came first each time).
- **Consequence.** D124: a container's ports of the host's go before its end is told
  (`HOST_RELEASE`), as dockerd's do; a joiner of a container on the host's network is on
  it too, told so, its ports its own (`HostBind::owner`).

### M182. What a command's datagrams to its network's members come to as it ends, and in bursts (D31, D46)

- **Question.** Do a command's last datagrams to a member of its network (on its link,
  so asked for by ARP, which M180's permanent gateway entry does not cover) reach it when
  the command ends at once? And bursts: does what the sender's guest sends reach the
  member's?
- **Method.** `docs/research/measurements/exit-drain/run.sh`, the ignored test
  `datagrams_a_command_sends_as_it_ends`: on a user network, a member counts the
  datagrams that come (the test guest's `udp-count`); another, whose guest never sent to
  it, sends one 64-byte datagram, or a burst of 1,400-byte datagrams as fast as it can
  (`udp-send`), and ends at once, or after 500 ms or a second; N of each, interleaved,
  quiet and with 16 busy loops. With `M182_COUNTERS`, each side's UDP counters and
  eth0's packets. Docker: `docker-burst.sh` (python3 on both sides) and
  `docker-fast-burst.sh` (the test guest's own `udp-send` and `udp-count`), Docker 29.3.1
  in `shards-dind`, network and containers its own; `shards-burst.sh`, the python3
  probe through shards. Apple M5 Max, macOS 26.4.1, 2026-10-11.
- **Results.**
  - One datagram to a member never sent to, its command ending at once: none lost of 50
    quiet (load average 4.8) and 50 under 16 busy loops (to 21), the release build of
    6bfe22b6; before the harness, 260 more across five runs of the loop, 100 of them under
    16 busy loops (load average to 60): none lost.
  - Bursts, before: of the fast sender's 200, its eth0 sent 112 to 200 and the member's
    eth0 had 51 to 130 (the link between the two network processes lost 35 to 54%):
    the link is a socket pair, to which macOS gives 8 KiB each way
    (net.local.stream.sendspace, recvspace), five full-size frames, and a frame a full
    link cannot take is dropped (D46). Each end now holds a ring's worth: every frame
    reached the member's eth0 (8 of 8 bursts of 200; 1,000 of 1,000 in bursts of 1,000).
  - Bursts, as they are (the harness, bursts of 200): short in 29 and 23 of 50 quiet
    (ending at once, after a second; least 121 and 143, medians 197 and 200), 26 and 21
    of 50 loaded (least 3 and 92, medians 199 and 200). The member's guest drops what
    its socket's buffer cannot hold (RcvbufErrors): 43 to 65 of 200 in 3 of 8 bursts,
    517 to 662 of 1,000 in each; past a ring's worth of waiting frames the member's
    network process drops too (of 5,000, 1,835 to 2,661 reached its eth0).
  - Docker delivers each: 200, 1,000 and 5,000, 5 of 5 each, from either sender. Python
    bursts through shards: 200, 1,000 and 5,000, 5 of 5 each, with or without the
    link's change (python3 sends slowly enough).
- **Consequence.** A command's last datagram to a member is not lost as it ends: no drain
  of the guest's neighbor queue is needed. The peer link holds a ring each way (D46). An
  address a run asked for (`--ip`) is free once its end is told, as found here: a run
  asking for it at once found it held, once in about 200 under load. Open: bursts from a
  fast sender to a one-vCPU member lose at the member's socket and, past a ring, in its
  network process, where Docker's container, reading on a core of its own as the bridge
  delivers, loses none.

### M183. What each vCPU costs a run, and what one vCPU costs its programs (`--cpus`, -m)

- **Question.** A run's microVM has one vCPU unless `--cpus`, the quota or the cpuset
  asks for more; Docker's container has every CPU of its host unless limited. What does
  each vCPU cost a run, in its start and its memory, and what does one cost its programs?
- **Method.** `docs/research/measurements/vcpus/run.py`: `shards run --rm --cpus C
  alpine true`, timed by its client, 50 a count at 1, 2, 4, 8 and 18 vCPUs, the counts in
  turn once each one's template is made, quiet (load average 3) and with 16 busy loops;
  then each count's guest memory and its VM process's resident memory. measure-run.py at
  each size and count (`CPUS`): arm64 here (2 boots of each of 29 sizes at 1, 2, 4, 8 and
  18), x86_64 on three GitHub runners (AMD EPYC 7763, 4 boots each of 24 sizes at 1, 2
  and 4, run 38140736249). Apple M5 Max (18 cores), macOS 26.4.1, the release build of
  the vcpus branch, 2026-10-11.
- **Start.** Not one: p50 5.46 to 5.99 ms at every count quiet, 6.28 to 6.48 ms loaded;
  p99 9.3 to 13.9 ms quiet, 14.5 to 19.9 ms loaded, at no count more than another.
- **Memory.** A 256 MiB VM's memory that may be allocated (MemAvailable and the CPUs'
  listed pages) was 1.8, 4.1, 7.5 and 21.0 MiB less at 2, 4, 8 and 18 vCPUs than at one;
  its VM process held 22.9, 23.8, 25.3, 27.6 and 30.6 MiB. Over every size, the most the
  vCPUs past the first kept: arm64 1,900 KiB at 2, 4,040 at 4, 7,720 at 8, 20,024 at 18;
  x86_64 120 at 2, 1,576 at 4. `-m` took none of it: 700m with 8 vCPUs left the
  workload 5,100 KiB short, with 18, 17,428 KiB short; 2500m with 18, 3,552 KiB.
- **Found: a microVM of many vCPUs ended as its template was saved.** The VM process
  said "vCPU 6: guest memory at 0x82f62e40 faults with its access given back" and was
  ended, the run saying "VM … ended before it was ready": while its working set is
  recorded (HVF), a page read goes back read-only and a page written goes back writable,
  and a read's page given back after another vCPU's write had it writable took the write
  away; the writer faulted again on a page it had just had back whole, which the VMM took
  for an access that can never succeed. 4 of 60 templates of 18 vCPUs failed so. With a
  page any vCPU wrote kept writable, and each page given back under the recording's lock,
  none of 180 did (60 quiet at 18 vCPUs; 60 at 18 and 60 at 8 under 16 busy loops).
- **What one vCPU costs its programs.** `nproc` is 1, where Docker's is the host's; a
  member receiving a fast burst of 1,000 UDP datagrams read 269 to 515 of them with one
  vCPU and 996 to 1,000 with four (`exit-drain/shards-fast-burst.sh`), as Docker's
  container read all (M182): one vCPU both takes the frames and runs the reader.
- **Consequence.** `-m` takes what each vCPU keeps (`resources::VCPUS`: the second's,
  and each past it, the most measured), and the test of limits runs with every CPU of
  the host's too. HVF's working-set recording gives pages back as above. How many vCPUs a
  run has when nothing asks, asked of the user with these costs: every CPU of the host's
  (D125).

### M184. Keyless signatures of shards and cosign, each verified by the other, against a local Sigstore (D126)

- **Question.** Does `shards sign` without a key make what cosign v3.1.3 makes, so that
  cosign verifies shards' signatures and shards cosign's, against real Fulcio, Rekor v1,
  Rekor v2 and a timestamp authority? Do shards' sign-in flows hold against the OIDC
  provider Sigstore runs (Dex)? What does a keyless signature take?
- **Method.** `docs/research/measurements/keyless/run.sh` in the shards-dind container
  (docker 29.3.1-dind, arm64): setup.sh starts Sigstore's local stack as Sigstore's own CI
  does (scaffolding 86f48a09's setup-sigstore-env) from each service's release, Fulcio
  v1.9.0, Rekor v1.5.4, rekor-tiles v2.3.0 and timestamp-authority v2.1.4, with a fake
  OIDC issuer, Dex (Fulcio's compose's) added as an issuer Fulcio takes, and a registry
  (distribution, `registry:2`). cases.py, with shards' build of revision f940a357
  (aarch64 musl, test-release) and cosign v3.1.3's release (checksum checked): for each
  signing config (Rekor v2 with the timestamp authority, Rekor v1 with it, Rekor v1
  alone) an agent made and pushed by shards, signed keylessly by shards with a token of
  the fake issuer, pushed, verified by `cosign verify` with that identity and with
  another; signed by cosign; each bundle read back from the registry. Then shards signing
  in at Dex in a browser (the harness's `xdg-open`, browser.sh, following Dex's mock
  connector by curl) and by a device code (device-login.sh), each verified by cosign. Then
  20 signatures each, alternated, one artifact. The bundles and the trusted root are kept
  in crates/sigstore/testdata/keyless for crates/sigstore/tests/keyless.rs. Apple M5 Max,
  macOS 26.4.1, Docker Desktop's VM, 2026-10-11.
- **Results.**
  - `cosign verify` passed shards' signature in each signing config (exit 0: "The cosign
    claims were validated", "Existence of the claims in the transparency log was verified
    offline", "The code-signing certificate was verified using trusted certificate
    authority certificates"), and refused it for another identity (exit 1: `expected SAN
    value "someone@else.example", got "foo@bar.com"`). shards verified cosign's (and its
    own) offline as `cosign verify` verifies: a log entry, the certificate's SCT, an
    observer timestamp, the identity, the artifact.
  - The bundles hold the same fields, signer by signer and config by config: the
    certificate, one entry (`dsse` v0.0.1 in Rekor v1, `hashedrekord` v0.0.2 in Rekor v2,
    the latter with no promise or integrated time), the timestamp where the config names
    an authority, the envelope. The one difference seen, `logIndex` absent, was shards'
    entry at index 0 of a fresh log, which protojson leaves out as Go's leaves it.
  - Signed in at Dex: in a browser (PKCE, the code brought back to the listener, the token
    checked: signature, issuer, audience, expiry, nonce, access token hash) and by a device
    code, each signature of `kilgore@kilgore.trout` verified by cosign.
  - A keyless signature, n 20 each: shards (signing and keeping) p50 1,300 ms, p90 1,527,
    p99 and max 1,652; cosign (signing and pushing) p50 1,636, p90 1,950, p99 and max
    3,422. Both are the services' time: Fulcio's certificate, the timestamp, Rekor's entry.
  - Found on the way: rekor-tiles v2.3.0's compose builds its witness from
    transparency-dev/witness's main, which dropped a flag the compose passes
    (`--poll_interval`, e288f04d, the morning of v2.3.0's release): pinned to the commit
    before. Dex's login page escapes a connector ID that holds slashes twice (the mock
    connector's `https://any.valid.url/`), so its link finds no connector ("failed to get
    connector object from storage: not found"); followed escaped once, it signs in.
- **Consequence.** Keyless signing as cosign's, held to it both ways on real services
  (D126). The oracle against cosign's own libraries (`scripts/keyless/generate`) holds
  what is sent byte for byte; Sigstore's public instance is the CI job
  `.github/workflows/keyless.yml`.

### M185. SOURCE_DATE_EPOCH taken from a build's sources, by docker/dockerfile:1.27.1 and by shards (D127)

- **Question.** What does docker/dockerfile:1.27.1 take as a build's time when
  SOURCE_DATE_EPOCH names a source, on Docker's BuildKit and on a newer one? What steps
  does buildx show for it? Does shards' frontend do the same?
- **Method.**
  - `scripts/frontend/compare` in the shards-dind container (Docker 29.3.1, its BuildKit
    v0.28.1, buildx v0.37.1), with its 11 `epoch-*` cases. Each case runs on a buildx
    docker-container builder of `moby/buildkit:v0.33.0`, except `epoch-daemon-refuses`,
    which runs on Docker's own.
  - Each case is built twice in turn by docker/dockerfile:1.27.1 (pinned by digest) and by
    shards' frontend (D127's tree, aarch64 musl, test-release). buildx's outputs are
    compared step by step, and, where an epoch resolves, the OCI images byte for byte.
  - The sources: moby/go-archive v0.3.3 (by Git, and its codeload tarball), cosign
    v3.1.3's `cosign_checksums.txt` (its Last-Modified), `alpine:3.20`, and a two-image
    OCI layout in the case.
  - Also buildx by hand, for the named image case's raw output on one platform and on
    two.
  - Apple M5 Max, macOS 26.4.1, Docker Desktop's VM, 2026-10-11.
- **Results.**
  - **v0.28.1 refuses** a SOURCE_DATE_EPOCH of `src` in its daemon, before the frontend
    runs, with either frontend: `ERROR: failed to build: failed to solve: invalid
    source-date-epoch: strconv.ParseInt: parsing "src": invalid syntax: strconv.ParseInt:
    parsing "src": invalid syntax`.
  - **v0.33.0 leaves it to the frontend.** The times taken:
    - go-archive v0.3.3 by Git (as the context, a named context, and a stage's ADD): its
      commit's committer time, `2026-08-05T17:35:52+02:00`, in the image's `created` and
      history, the committer's zone kept;
    - its codeload tarball, which has no Last-Modified: the newest file,
      `2026-08-05T15:35:52Z`;
    - cosign's checksums file: its Last-Modified, `2026-08-06T01:06:09Z`;
    - a local context, `alpine:3.20` as a named context, and the OCI layout: no time, so
      each image carries the time it was built;
    - `SRC`: refused by both frontends, "invalid context name SRC: invalid reference
      format: repository name (library/SRC) must be lowercase".
  - **A named image context** shows one step for its load and the time's resolution:
    `#4 [context src] load metadata for alpine:3.20`, `#4 DONE 0.3s`, then `#4 [internal]
    resolve SOURCE_DATE_EPOCH named context src`, `#4 DONE 0.5s`. With `--platform
    linux/amd64,linux/arm64`, the same lines appear once.
  - **shards' frontend equals docker/dockerfile:1.27.1** in all 11 cases, outputs and
    images (compare exit 0).
  - **Found on the way:** shards' frontend first failed `epoch-context-git` ("open
    Dockerfile: no such file or directory"). It read the remote context's Dockerfile
    where `-f` named a local one.
- **Consequence.** D127: both `shards build` and shards' frontend take the time as
  v0.33.0 with dockerfile 1.27.1 does, and the frontend reads `-f`'s Dockerfile.

### M186. How buildx ends a build a policy's own error stops, and an OCI layout under a policy (D128)

- **Question.** When a build policy itself fails (its input not built, its Rego in
  error), what do buildx v0.37.1 and BuildKit v0.28.1 print, and how does the policies'
  step end? What does a policy see of an OCI layout given as a named context?
- **Method.** buildx v0.37.1 in the shards-dind container (Docker 29.3.1, its BuildKit
  v0.28.1, its built-in Dockerfile frontend), each build run from inside its context as
  `.`:
  - `FROM alpine:3.20`, with a `Dockerfile.rego` whose two complete rules give different
    decisions for an image;
  - the same, the two rules for the Dockerfile's own (`local`) source;
  - for contrast, one rule denying images;
  - `FROM src`, with `--build-context src=oci-layout://../layout:v1` (a two-image layout)
    and a policy allowing everything.
  - Apple M5 Max, macOS 26.4.1, Docker Desktop's VM, 2026-10-11.
- **Results.**
  - **Rego's conflict over an image.** The error is the policy's alone: `ERROR: failed
    to build: failed to solve: Dockerfile.rego:7: eval_conflict_error: complete rules
    must not produce multiple outputs`, after the FROM line's excerpt. The policies'
    step ends `DONE`; no recap of it is printed.
  - **The same over the Dockerfile's source.** The same error and the step done, with no
    excerpt: the Dockerfile was not read.
  - **A denial.** The step fails and is recapped (` > loading policies
    Dockerfile.rego:` and its lines). The error is wrapped: `failed to solve:
    alpine:3.20: failed to resolve source metadata for docker.io/library/alpine:3.20:
    could not resolve image due to policy: source
    "docker-image://docker.io/library/alpine:3.20" not allowed by policy: action DENY`.
  - **The OCI layout.** The build fails at its FROM line, before any of the layout is
    resolved, with no step for it: `failed to solve: failed to build policy input:
    unsupported source scheme: oci-layout`. The step ends `DONE`.
  - **Why the texts differ.** A denial is BuildKit's error. A policy's own error is
    buildx's callback's, a gRPC status, which BuildKit's grpcerrors keeps over its
    wrappings. buildx fails the step only where its error names a denial
    (isPolicyEvaluationError).
- **Consequence.** D128.

### M187. CI's musl jobs: how long each part takes, runner to runner

- **Question.** Why did CI's x86_64 musl test job run out of its step's 45 minutes on
  3c77279f's push, and how should its work be shared between the musl jobs?
- **Method.** GitHub's records of the musl jobs of five pushes to dev (2026-10-11:
  4862d7d7, 45902169, 004cffbd, c3188dee, 3c77279f), and the phases of two of their job
  logs (c3188dee's, 3c77279f's) by their timestamps.
- **Results.**
  - The x86_64 test job (clippy, the test-release build and tests, the release build of
    the Rego stack guards and their run) took 42.1, 42.7, 45.1, 29.7 and 45.3 minutes.
    The last ran out of time in its last test binary.
  - The x86_64 release job took 10.5 to 15.1 minutes. The aarch64 test job took 25.9 to
    31.9; its release job 12.2 to 13.3.
  - **Runner to runner.** Every phase was about 1.7 times as long on the slower runner:
    - clippy 3 m 17 s and 5 m 50 s;
    - the test-release build 13 m 32 s and 22 m 41 s;
    - the unit tests 46 s and 75 s;
    - `containers.rs` 125 s and 190 s.
- **Consequence.** The lints and the Rego stack guards (built on release) move to the
  release job. That job builds release anyway, and has some 30 minutes to spare.

### M188. A daemon told to stop listened again, and its stop's next run failed

- **Question.** Why did CI's guest-memory measurement (`measure-run.py`, which stops the
  daemon before each run) fail twice in a row, each time on one run of hundreds: "the
  command's microVM stopped before the command ended"?
- **Method.**
  - The jobs' logs. In 3c77279f's, measure (b) failed at `-m 4773044k --cpus 1`, its
    third boot.
  - The harness's pattern alone on an M5 Max (macOS 26.4.1), 2026-10-11:
    `shards stop daemon`, the home's templates removed, `shards run --rm -m 1048576k
    --cpus 1 alpine cat /proc/meminfo`, again and again. Each run's daemon.log lines were
    read, and test-release builds of dev (19fca87d) and of the fix compared.
- **Results.**
  - **CI.** The run's daemon (38842) logged that its socket "was removed; listening there
    again", then that it was exiting. No daemon started for the failing run. Its client
    connected to that daemon's re-bound socket, and the daemon exited under it, 4 ms
    after the previous run's output.
  - **Here, unfixed: 2 of 200 stops** left a daemon listening again. Each time the next
    client found the socket dead, started a new daemon, and its run passed.
  - **The cause.** The listener reads `closed` as its pass begins, then asks whether the
    socket is there. A stop, handled on another thread, closes the daemon and removes
    the socket between the two. The listener took that removal for someone else's and
    bound the socket again, so a stopping daemon listened for clients until it exited.
  - **With a log line in `close()` before its removal**, as a probe: 0 of 1,300 stops.
    The line delayed the removal past the listener's look.
  - **Fixed.** `close()` and the listener's re-bind take one lock, and the re-bind asks
    `closed` again under it. In 600 stops (601 daemons), none listened again and no run
    failed, where the unfixed build had listened again after 2 of 200.
- **Consequence.** The fix, with `a_daemon_told_to_stop_never_listens_again`, which
  plays the listener's late look and fails without the re-check. Also: a closed daemon
  starts no warm VM. A waiting one that ended as the daemon exited had been replaced by
  one that ended with it (`a_closed_daemon_refills_nothing`).

### M189. SBOM attestations by docker/dockerfile:1.27.1 and by shards' frontend (D129)

- **Question.** Does shards' frontend, given `--sbom=true`, make what docker/dockerfile:1.27.1
  makes: the same steps, the same images, the same SBOM statements?
- **Method.**
  - `scripts/frontend/compare` in shards-dind (Docker 29.3.1, its BuildKit v0.28.1,
    buildx v0.37.1), each case built twice in turn by both frontends, `--sbom=true` with
    `--provenance=false`:
    - `sbom-basic`: `FROM alpine:3.20`, one COPY;
    - `sbom-extras`: a stage asking `BUILDKIT_SBOM_SCAN_STAGE` and a target asking
      `BUILDKIT_SBOM_SCAN_CONTEXT`;
    - `sbom-platforms`: `linux/amd64,linux/arm64`;
    - `attest-unknown`: `--attest type=foo`.
  - The scanner was BuildKit's default, `docker.io/docker/buildkit-syft-scanner:stable-1`
    ("starting syft scanner for buildkit v1.12.0").
  - Apple M5 Max, macOS 26.4.1, Docker Desktop's VM, 2026-10-11.
- **Results.**
  - **The first run differed in noise alone.** Two things differed between the two
    frontends, and between two runs of one:
    - the scanner's log lines carry its clock;
    - the digests of the attestation manifests and of the index naming them follow from
      the SPDX documents, whose `creationInfo.created` and `documentNamespace` the
      scanner makes anew each run.
  - **With those set aside, all four cases were equal**:
    - each step, the scans named `[linux/arm64] generating sbom using
      docker.io/docker/buildkit-syft-scanner:stable-1` (and `[linux/amd64] …`);
    - the image manifests and configs byte for byte;
    - the attestation manifests' layers, their media types and annotations;
    - the statements, subjects included;
    - for `attest-unknown`, the refusal.
- **Consequence.** D129. The harness's `sbom` cases compare that way.

### M190. Frontend inputs, by docker/dockerfile:1.27.1 and by shards' frontend (D130)

- **Question.** Given frontend inputs as buildx v0.37.1 sends them, does shards' frontend
  make what docker/dockerfile:1.27.1 makes: the same steps, the same images, the same
  refusals?
- **Method.**
  - `scripts/frontend/compare` in shards-dind (Docker 29.3.1, its BuildKit v0.28.1,
    buildx v0.37.1), each case built twice in turn by both frontends. Bake cases run
    `buildx bake` in the case's directory. The cases' repositories are served over
    git:// from dind by `scripts/frontend/git-serve`, whose `git upload-pack` is dind's
    own git (dind's git has no daemon).
    - `bake-target`: `contexts = { base = "target:base" }`, both targets one Dockerfile;
    - `bake-target-other`: the same, a Dockerfile each;
    - `bake-target-platforms`: the same for `linux/amd64` and `linux/arm64`;
    - `bake-remote`: a remote bake definition's target in a directory of the repository;
    - `git-query-input`: a Git context with a query, `BUILDX_SEND_GIT_QUERY_AS_INPUT=1`;
    - `input-lowercase`: the same, the repository's Dockerfile named `dockerfile`;
    - `git-lowercase`: a Git context whose Dockerfile is named `dockerfile`, on BuildKit
      v0.33.0;
    - `git-mtime-refused`: a Git context on Docker's own BuildKit.
  - Then every case of the harness.
  - Apple M5 Max, macOS 26.4.1, Docker Desktop's VM, 2026-10-11.
- **Results.**
  - **The first bake runs failed on both sides alike**, from the harness: bake asks that
    an output outside its directory be allowed (`--allow=fs.write`), now passed.
  - **A step's names in a bake depend on timing, with either frontend.** A step two
    targets share (the Dockerfile's load, `.dockerignore`'s, the base's COPY run in the
    app's build) is said under whichever target's build reported it first:
    `[base internal] load .dockerignore` beside `[app internal] load .dockerignore` in one
    run, not in the next. The harness sets the targets' names aside.
  - **Docker's BuildKit lacks `source.git.mtime`**, which a Git context needs under
    SOURCE_DATE_EPOCH (dockerui's `GitMTimeCommit`). Both frontends refuse such a build,
    and docker/dockerfile says it after `failed to resolve dockerfile:`. shards' frontend
    said it bare, as it did any capability refused before a solve, and dockerui wraps
    those where it solves the Dockerfile and an HTTP context. Fixed (`Gateway::solve_as`).
  - **docker/dockerfile reads a lowercase `dockerfile` from any Dockerfile source.**
    shards' frontend tried it only in the client's directory; it now tries it in a Git or
    HTTP context and in an input too.
  - **With those, all eight cases were equal**: each step, the images byte for byte,
    including the config the base target's `input-metadata` carries (`ENV`, `WORKDIR`),
    and the refusal word for word.
  - **Every case of the harness, 35, was equal** at the final revision: the eight above,
    and the 27 before them, unchanged by what D130 changed in the LLB writer (metadata,
    capabilities, source maps).
- **Consequence.** D130. The harness gains `bake`, `repo` and `env` case files.

### M191. Two CI failures of dev b753f6e4: a dropped go-ahead and a throttle outlasting the waits

- **Question.** CI run 38168340510 (dev b753f6e4, 2026-10-11) failed two tests, each of
  which had passed before. What failed, at the root?
- **Method.** The jobs' logs (`gh api …/jobs/ID/logs`, colours stripped). Then the
  failure was made to happen at will, and the fixes were mutation-checked against it.
- **Results.**
  - **`agents_out_of_memory_end_whole_and_spare_the_run` (x86_64 glibc): the host's
    SIGUSR1 was dropped.** The run's command (`outlive 64`) waited out its minute
    (`await timeout filling`), although agent y filled 60 MiB and was ended, and the host
    signalled it (`run_watching` printed no "never ready").
    - Without `--init` the command is the first process of its PID namespace. The kernel
      drops a signal sent to such a process that neither handles nor blocks it
      (kernel/signal.c, `sig_task_ignored`: SIGNAL_UNKILLABLE).
    - `outlive` blocked SIGUSR1 only after taking its 64 MiB. In a microVM of 128 MiB,
      with y filling, that took longer than y's filling and ending. The go-ahead arrived
      first and was dropped.
    - Made to happen at will: `await-late 3000`, a command that waits 3 s before it
      awaits, watched with a condition true from the first look. With the old handshake
      it timed out every time.
    - `top`'s `blocked` cannot say when a command waits. The kernel takes the signals
      waited for out of the blocked set while `sigtimedwait` waits (into `real_blocked`),
      so SigBlk read 0000000000000000 as the command waited.
    - **Fixed:** the test guest catches SIGUSR1 first (a handler that sets a flag),
      then blocks it, checks the flag and waits. The host signals only once `top`'s
      `caught` shows SIGUSR1 for a `testguest` process, and asks its condition no more
      once it has held. `an_awaiting_command_is_signalled_once_it_waits` passes, and
      fails with the old handshake (mutation-checked).
  - **`an_s6_overlay_image_runs_as_pid_1` (x86_64 musl): ghcr.io throttled past the
    waits.** The pull of `ghcr.io/linuxserver/baseimage-alpine` failed after 4.4 s:
    "error from registry: retry-after: 301.718µs, allowed: 44000/minute". The waits
    allowed at most 1.55 s in all (PM M112's schedule). Docker would have failed sooner:
    dockerd gives up on TOOMANYREQUESTS at once, and containerd asks again at once.
    **Fixed** by D131: such a throttle is waited out within the minute it names.
- **Consequence.** D131, and the handshake of `common::run_watching` and the test guest's
  `await`.

### M192. KVM in the arm64 guest kernel: what it costs a boot and a restore (D132)

- **Question.** Should the kernel every arm64 guest boots carry KVM, so that any guest can
  be a hypervisor, or should a variant carry it?
- **Method.** `docs/research/measurements/kernel-ab/ab.py`, brought up to today's CLI
  (`run --kernel`, `restore`). A: the pinned kernel (Image-6.18.48-aarch64, sha256
  684c7f5d…). B: its `kvm` variant as released (kernel-6.18.48-13af2abd3f30,
  Image-6.18.48-aarch64-kvm, sha256 5bf30280…), the same inputs and
  `resources/kernel/kvm.config`; the release also rebuilt A byte for byte. Both booted at
  EL1, with 1 vCPU and 256 MiB, each sample a fresh process, pairs alternating A B, B A,
  3 warm-ups, n = 100. Apple M5 Max, macOS 26.4.1, revision 0957cedd, 2026-10-11.
- **Results** (µs):

  | metric | kernel | p50 | p90 | p99 | max |
  |---|---|---|---|---|---|
  | boot_kernel | A | 18296 | 19533 | 20628 | 21494 |
  | boot_kernel | B | 18318 | 19814 | 21350 | 22237 |
  | boot_to_init | A | 20827 | 22095 | 24143 | 24147 |
  | boot_to_init | B | 20954 | 22670 | 24243 | 24824 |
  | restore | A | 2545 | 2793 | 2971 | 3048 |
  | restore | B | 2514 | 2815 | 3087 | 3091 |

  - The boots differ by 22 to 127 µs at p50, and B's restores are 31 µs faster: within
    what one template's restores differ from another's, tens of µs (PM M29).
  - An earlier run, against a `kvm` build of 2026-10-08 that lacked patch 0002, said the
    same: 17813 against 17812 µs to boot, 2384 against 2406 to restore, at p50.
  - The image grows by 800 KiB, 19,081,728 to 19,900,928 bytes (4.3%).
- **Consequence.** D132: KVM goes in a `kvm` variant of the kernel, not in the one every
  guest boots. It saves nothing measurable at boot either way, and its text would be in
  every guest.

### M193. A builder's memory, plugged as its build needs it (D133)

- **Questions.** How does a VMM on macOS give a guest's memory back to the host once the
  guest unplugs it? What does the guest kernel size by the memory it boots with, which a
  builder booted small would lack? What does a builder that boots small and grows cost a
  build, and save the host?
- **Giving memory back** (`docs/research/measurements/hvf/hvfbench.c`, `discard`): 256 MiB
  of anonymous memory mapped into a VM, the guest writing a word in each 16 KiB page, then
  given back four ways, three rounds each; the process's footprint (TASK_VM_INFO
  `phys_footprint`) after each step, and what the guest and the host read back.
  2026-10-11, Apple M5 Max, macOS 26.4.1, load average 7.

  | given back by | time | footprint change | guest reads | host reads |
  |---|---|---|---|---|
  | `MADV_FREE_REUSABLE` | 0.86-1.07 ms | +0.0 MiB | its old data | its old data |
  | `MADV_FREE` | 1.07-1.25 ms | +0.0 MiB | its old data | its old data |
  | `hv_vm_unmap`, `MAP_FIXED` anonymous memory over it, `hv_vm_map` | 2.4-3.4 ms | -256.1 MiB | zeros | zeros |
  | `MAP_FIXED` anonymous memory over it alone, still mapped in the VM | 3.7-11.4 µs | +0.0 MiB | its old data | zeros, and none of the guest's writes after |

  - Unmapping 2 MiB of a 256 MiB mapping and mapping fresh memory over it takes 15-18 µs
    and gives 2.0 MiB back; the guest's access there exits at its address (a data abort
    to the VMM), and mapped again it reads zeros, the rest of the region still the guest's.
  - Hypervisor.framework's stage 2 holds the pages it maps: replaced in the host alone,
    they stayed the guest's, and the two no longer shared memory.
  - Reading memory the guest never touched costs the host a page per page read: after the
    third way, the guest's read of the fresh 256 MiB took 256.1 MiB.
  - A page the hypervisor has unmapped is out of the footprint and the resident set
    whether the process still holds it or not: with its pages never given back, a VM
    process's footprint fell from 974 MiB to 78 MiB as the guest unplugged 1 GiB, and it
    held 896 MiB of it as it ended (`a_guest_grows_and_gives_back_its_memory`, mutation).
    What the process holds of the memory to plug is asked of the OS page by page
    (`mach_vm_page_range_query`, Linux's pagemap): 2.5 ms for 63.5 GiB.
- **What the kernel sizes by its memory at boot** (the test guest's `limits`, the pinned
  kernel 6.18.48, booted with each size): linear in the memory but where the kernel caps
  it.

  | limit | 512 MiB | 1 GiB | 4 GiB | 16 GiB | 64 GiB |
  |---|---|---|---|---|---|
  | kernel.threads-max | 3995 | 8009 | 31607 | 128038 | 513765 |
  | user.max_*_namespaces, RLIMIT_NPROC, RLIMIT_SIGPENDING | 1997 | 4004 | 15803 | 64019 | 256882 |
  | fs.file-max | 48903 | 100244 | 402298 | 1636622 | 6573917 |
  | fs.inotify.max_user_watches | 8192 | 8192 | 31794 | 129321 | 519427 |
  | fs.epoll.max_user_watches | 108989 | 223365 | 895777 | 3643481 | 14634317 |
  | net.ipv4.tcp_mem (pages) | 5688 7585 11376 | 11361 15151 22722 | 46362 61819 92724 | 190890 254522 381780 | 768324 1024432 1536648 |
  | net.ipv4.udp_mem (pages) | 11376 15171 22752 | 22725 30302 45450 | 92727 123638 185454 | 381783 509045 763566 | 1536648 2048864 3073296 |
  | net.ipv4.tcp_rmem, largest | 3883872 | 7757344 | 31651456 | 33554432 | 33554432 |
  | net.ipv4.tcp_wmem, largest | 3883872 | 4194304 | 4194304 | 4194304 | 4194304 |
  | net.ipv4.tcp_max_orphans, tcp_max_tw_buckets | 2048 | 4096 | 16384 | 65536 | 262144 |
  | MemTotal | 505786368 | 1031905280 | 4124942336 | 16764416000 | 67322318848 |

  - inotify's watches are 1:28.17 of epoll's items at 4, 16 and 64 GiB, where the kernel
    clamps neither: `4 × INOTIFY_WATCH_COST / EP_ITEM_COST`, 1296 and 184 bytes in this
    kernel. Below about 1 GiB inotify's are clamped at 8192.
  - TCP's receive buffers stop at 32 MiB, its send buffers at 4 MiB; its orphans and
    TIME-WAIT buckets are half its established table, a power of two.
  - Scaled from a boot of 512 MiB by the ratio of the memory, the limits in proportion to
    it come out up to 4% above a boot with all of it: the kernel's estimate of its free
    memory at boot, which `threads-max` is of, is MemTotal and about 17 MiB at every size.
  - `fs.file-max` is a file for each 10 KiB of the memory the kernel has as it starts its
    caches, less half again what it has used by then (`files_maxfiles_init`), so more
    memory adds a file for each 10 KiB: 48903 and that from 512 MiB comes within 38 files
    of 4 GiB's and 64 GiB's. Scaled in proportion instead, a builder booted with 512 MiB
    that may have 2560 had 254199, 145 short of a guest booted with 2560 on x86_64 (CI run
    38185002440); the rest of its limits were within bounds there.
  - A builder booted with 512 MiB that may have 2560 MiB gives its steps each limit of a
    guest booted with 2560 MiB, or up to a tenth more
    (`a_builders_memory_is_plugged_as_its_build_needs_it`).
- **The device on a real VM** (`a_guest_grows_and_gives_back_its_memory`, three runs): a
  guest booted with 256 MiB asked for 1 GiB and had it online in 5-7 ms, and gave it back
  in 10-21 ms; the VM process's footprint 974 MiB with 896 MiB of it written, 78 MiB after.
- **What a builder costs a build** (`docs/research/measurements/builder-memory/ab.py`):
  `shards build --no-cache` through two release builds, A the builder booted with all its
  memory (revision d1f88191), B one booted with 512 MiB that may have the same, half the
  host's (64 GiB); their VM processes signed with identifiers of their own; both arms each
  turn, in random order; two turns of warm-up, then n = 20. Wall clock, spawn to exit, and
  the builder's peak resident set. `steps`: M82's three RUNs over alpine:3.22; `grow`: one
  RUN that writes 4 GiB to a file and removes it. 2026-10-11, Apple M5 Max, 128 GiB, macOS
  26.4.1, load average 7.4-7.5.

  | build | metric | arm | p50 | p90 | p99 | max |
  |---|---|---|---|---|---|---|
  | steps | wall (ms) | A | 598.5 | 971.6 | 1229.5 | 1245.9 |
  | steps | wall (ms) | B | 443.0 | 715.5 | 899.5 | 933.6 |
  | steps | peak RSS (MiB) | A | 1424.2 | 1424.8 | 1425.0 | 1425.0 |
  | steps | peak RSS (MiB) | B | 95.9 | 97.0 | 97.5 | 97.6 |
  | grow | wall (ms) | A | 1441.5 | 1563.2 | 1793.1 | 1846.2 |
  | grow | wall (ms) | B | 1393.0 | 1538.8 | 1632.4 | 1641.6 |
  | grow | peak RSS (MiB) | A | 5527.5 | 5527.8 | 5528.0 | 5528.0 |
  | grow | peak RSS (MiB) | B | 4335.0 | 4335.5 | 4335.8 | 4335.8 |

  - Paired, B less A: `steps` -199.3 ms [-223.4, -158.4] and -1327.9 MiB; `grow` -67.2 ms
    [-87.6, -40.0] and -1192.6 MiB (median, bootstrap 95% interval).
  - B's timing line, which asks the OS what it holds of its 63.5 GiB to plug as it ends,
    costs its wall 2.5 ms, counted.
  - A build that needs 4 GiB, its builder growing to hold them, still ends sooner than one
    whose builder booted with 64 GiB; and its builder holds 1.2 GiB less: the page
    structures of the 56 GiB never plugged.
- **Consequence.** D133: a builder boots with 512 MiB and is given the rest as its build
  needs it; unplugged memory is given back by mapping fresh inaccessible memory over it,
  out of the VM first where its hypervisor holds the pages; the builder's limits are raised
  to all of its memory; and what a VM process holds of its memory to plug is asked of the
  OS, not read from its footprint.

### M194. Moving a cache mount in and out of the builder (D114, D134)

- **Question.** M140 left open where a build spends what a large cache mount costs it,
  and what a block device would cost instead: a cache of 10,000 files of 4 KiB in 100
  directories and two of 128 MiB made a build that mounted it and did nothing take 11.2 s
  at the median against an empty one's 3.9 s, under load. Where does that time go, and
  what is it with the cache's layers EROFS images on the builder's join disk (D134)?
- **Method.** `docs/research/measurements/cache-move/cachemove.py`, with M140's
  `fsbench.rs` built for the guest: two release builds of `shards` with `--features
  alloc-count`, whose builds mark each part of a step's cache mounts, A at 22cf16b7
  (D114's transport, the tree sent over the build's connection and the changes sent back
  and applied, marked) and B at 8f59734a (D134); each with a home of its own, taking
  turns, each turn starting with the other, n = 20. Each turn builds, with each in turn:
  `empty`, a step that mounts a new empty cache and does nothing; `filled`, one that mounts
  the cache a first build filled (M140's) and does nothing; `fill`, one that fills a new
  cache. Each build's step is new to the build cache by an `ENV` of its own. 2026-10-11,
  Apple M5 Max, 128 GiB, macOS 26.4.1; load averages 17.6, 19.1 and 39.2 at its end, the
  host also building and testing this change's mutants and running other projects' work.
  Wall clock in milliseconds, spawn to exit, p50 / p90 / p99 / max, and the paired
  difference (B less A, turn by turn: median, bootstrap 95% interval):

  | build | A (D114) | B (D134) | B less A |
  |---|---|---|---|
  | `empty` | 287.7 / 395.1 / 521.1 / 521.1 | 297.5 / 438.7 / 500.6 / 500.6 | 0.6 [-63.9, 90.7] |
  | `filled` | 1122.5 / 1892.2 / 1992.4 / 1992.4 | 266.4 / 407.4 / 541.9 / 541.9 | -859.2 [-1035.1, -789.7] |
  | `fill` | 1497.9 / 9392.9 / 12361.7 / 12361.7 | 644.6 / 894.4 / 1196.8 / 1196.8 | -782.0 [-1004.5, -715.1] |

  Where each build's time went (p50 / p90; each part from the step's previous mark):

  - A, `filled`: `cache-take` (the builder's start, the record taken, its tree made on
    the host) 285.3 / 377.7; `cache-send` 707.1 / 1116.5, of which 679.5 / 1094.6 waiting
    on the builder to take the tree's frames and write them; the step 18.5 / 28.7.
  - A, `fill`: the step 102.4 / 157.4; `cache-changes` 961.6 / 8568.9, of which applying
    them 108.3 / 8094.4 and the rest waiting on the builder; the layer written 169.7 /
    502.8, stored 14.9 / 41.6.
  - B, `filled`: `cache-take` 142.0 / 193.3; `cache-attach` (its layer, and a file for
    its changes, of which it made none) 0.3 / 0.5; the step 31.3 / 39.5, the layer mounted
    from the join disk as it starts; `cache-keep` 10.7 / 44.0.
  - B, `fill`: `cache-take` 165.8 / 235.8; `cache-attach` 0.3 / 2.3; the step 102.5 /
    173.7; `cache-keep` 288.9 / 413.8: the builder writing the image of its changes (295
    MiB of files, 10,000 of 4 KiB and two of 128 MiB) to the join disk and syncing it
    246.8 / 376.2, 1.25 GB/s of files at the median, and its file trimmed, synced
    (F_FULLFSYNC), named and recorded 26.2 / 126.1.
  - `empty` makes a new record each build, written and synced as it is taken: its
    `cache-take` was 211.1 / 279.0 in B against `filled`'s 142.0 / 193.3.
- **Consequence.** D134: a cache's layers are EROFS images on the builder's join disk,
  which the builder mounts and writes, so nothing of a cache crosses the build's
  connection. A cache mounted and left alone costs a build what an empty one does (B's
  `filled` and `empty` within their spread); filling one costs its image's writing, and no
  longer the host's applying of its changes, which under this load took 8.1 s at p90 and
  10.6 s at worst. Not split further here: B's `filled` step ran 12.8 ms longer than
  A's at the median, mounting its layer from the join disk where A's had the tree in
  memory.

### M195. What a builder's git says of a Git source, with `BUILDKIT_GIT_ADVICE` on and off (D135)

- **Question.** `BUILDKIT_GIT_ADVICE` asks BuildKit's git for its advice (D92). What do
  the builders print for a Git source with it on and off, and what of it is about the
  repository a build keeps?
- **Method.** In shards-dind (Docker 29.3.1): BuildKit v0.34.0 (`moby/buildkit:v0.34.0`,
  `buildctl build` with its dockerfile frontend, its cache pruned before each build) and
  Docker 29.3.1's own builder (BuildKit v0.28.1, `# syntax=docker/dockerfile:1.27.1`,
  `--no-cache`), git 2.52.0 in both. `ADD https://github.com/octocat/Hello-World.git`
  by its commit (`#7fd1a60b01f91b314f59955a4e4d4e80d8edf11d`), by its branch (`#master`),
  and by its branch with `--keep-git-dir=true`, each with `BUILDKIT_GIT_ADVICE` 0 and 1.
  2026-10-11.
- **Results.**
  - Both builders print git's lines as their step's log, each stamped (`#3 0.473 ...`),
    whatever the setting: `Initialized empty Git repository in` a path of their snapshot
    store (`/var/lib/buildkit/runc-overlayfs/snapshots/snapshots/9/fs/`,
    `/var/lib/docker/containerd/daemon/io.containerd.snapshotter.v1.overlayfs/snapshots/
    8415/fs/.git/`), by a commit's name a probe's `fatal: Not a valid object name
    <commit>^{commit}`, `From https://github.com/octocat/Hello-World`, the refs fetched,
    the commit; with the `.git`
    kept, another repository of theirs initialized and fetched into from the first
    (`From file:///var/lib/...`), and `HEAD is now at 7fd1a60 Merge pull request #6 from
    Spaceghost/patch-1`.
  - By commit and by branch, BuildKit v0.34.0 prints the same lines with advice on and
    off: no advice is printed.
  - With the `.git` kept, BuildKit v0.34.0 with advice on prints, between the fetch and
    `HEAD is now at ...`, `Note: switching to 'FETCH_HEAD'.`, and git's advice on the
    detached HEAD: sixteen lines, from an empty one to `Turn off this advice by setting
    config variable advice.detachedHead to false` and an empty one. With it off, none.
  - Docker 29.3.1 prints those lines with advice off: its BuildKit reads no `git.advice`,
    and git advises unless told not to.
- **Consequence.** D135: shards says what git says of the repository a build keeps (its
  checkout's `HEAD is now at ...`, the advice when asked) and none of what it says of a
  builder's own, each line stamped as BuildKit stamps it.

### M196. What code mode costs: its engine, and a program against MCP calls (D136)

- **Questions.** Which JavaScript engine should run code mode's programs: what does each
  add to the in-VM server's binary, and cost to start, to call into the host, to run a
  program, and in memory? And what does a program cost its caller, against the calls it
  makes made one by one over MCP?
- **Method.** `docs/research/measurements/code-mode/measure.sh`. Three static binaries
  built as shards' guest binaries are (aarch64 musl, `opt-level = "s"`, fat LTO, zig 0.16.0
  for QuickJS-NG's C): `base` (none), `boa` (Boa 0.22.0) and `qjs` (QuickJS-NG 0.16.2,
  rquickjs 0.14.0), each measure run in a microVM (`shards run`, the pinned kernel
  6.18.48), five rounds, the engines in turn within each: `create`, a runtime and context
  made with the host's functions and dropped, n = 1000 (µs); `calls`, a call into the host
  (ns per call, 50 batches of 10,000); `program`, a runtime made and the reference program
  run to its value (an async body awaiting three host calls, JSON, destructuring, a loop, a
  closure), n = 200 (µs); and each process's peak resident set (VmHWM). Then, in place
  (`code_mode_costs_in_microvms`): an agent in a microVM, in one MCP session to its
  instance, times in each of 200 rounds CALLS MCP calls of `whoami` one after another, one
  `run` of a program making those CALLS, and one `run` of a program making none, for
  CALLS of 1 and 10 (µs). 2026-10-11, Apple M5 Max, macOS 26.4.1, load average 7.7-12.3,
  the tree D136 was committed from (on dev fd9da553).
- **The engines** (each percentile's range over the five rounds):

  | measure | engine | p50 | p90 | p99 | max | peak RSS (KiB) |
  |---|---|---|---|---|---|---|
  | create (µs) | Boa | 107.5-116.6 | 195.2-286.1 | 1290.8-1421.2 | 1397.4-1593.6 | 3492-3532 |
  | create (µs) | QuickJS-NG | 123.0-132.5 | 140.8-153.8 | 185.2-227.8 | 294.3-362.9 | 432 |
  | call (ns) | Boa | 95.4-121.0 | 103.9-126.9 | 125.9-157.4 | 125.9-157.4 | 2532-2536 |
  | call (ns) | QuickJS-NG | 43.9-47.3 | 47.4-50.9 | 50.8-55.2 | 50.8-55.2 | 576 |
  | program (µs) | Boa | 200.0-230.4 | 1029.7-1293.2 | 1312.0-1656.0 | 1352.2-1812.8 | 4944-4948 |
  | program (µs) | QuickJS-NG | 94.8-110.4 | 101.3-161.4 | 139.2-302.3 | 359.2-413.8 | 636-640 |

  - Size: `base` 343,632 bytes; Boa 6,037,672, 5.69 MB more; QuickJS-NG 1,143,144, 0.80 MB
    more. `base`'s peak resident set: 88-92 KiB.
  - Boa's tail is its collector's: a tenth of its programs took over a millisecond, none of
    QuickJS-NG's did; its peak is 7.8 times QuickJS-NG's.
- **In place** (n = 200 each, µs):

  | CALLS | MCP calls, one by one | `run`, a program making them | `run`, a program making none |
  |---|---|---|---|
  | 1 | 32 / 45 / 94 / 147 | 447 / 514 / 643 / 700 | 408 / 473 / 558 / 633 |
  | 10 | 243 / 294 / 338 / 367 | 647 / 728 / 886 / 921 | 406 / 457 / 526 / 590 |

  (p50 / p90 / p99 / max.)
  - A program costs its caller about 0.4 ms of its own, what a process made for it costs:
    the helper's fork, the worker's filter, its runtime, the prelude and the program, and
    its frames. A call within it costs what one over MCP does, about 23 µs (from p50: 22
    within a program, 23 one by one): the instance answers both alike.
  - So code mode saves no time between an agent and its instance: what it saves is the
    model's turns, one for each call made one by one, against one for the program.
- **Consequence.** D136: QuickJS-NG runs code mode's programs, a process each; its cost is
  stated as measured, a program's own 0.4 ms, and its saving as the model's turns.

### M197. A WHP partition on GitHub's Windows runners: what its guest sees, and what an exit costs

- **Questions.** Before shards' WHP backend (AGENTFILE_ARCH §11's Windows builder): what
  does a partition's guest see of its processor and its hypervisor, with what it would
  keep time by in kvm-clock's place; and what does an exit cost?
- **Method.** `docs/research/measurements/whp-run`, by its workflow (`whp-run.yml`), run
  38209188341 on branch `whp`, 2026-10-12: a partition of one vCPU, its local APIC
  emulated (XApic), 2 MiB of memory, entered in long mode; its guest asks CPUID for the
  hypervisor's leaves (0x40000000-6), leaf 1 and 0x80000007, reads
  HV_X64_MSR_TSC_FREQUENCY and HV_X64_MSR_APIC_FREQUENCY where 0x40000003 offers them, then
  writes MMIO 1001 times and a port 1001 times, each exit emulated by WinHvEmulation as a
  VMM would, and timed from the host (n = 1000, the first of each left out). Twice: as
  created, and given every synthetic processor feature the host offers. Functions bound at
  run time (LoadLibraryExW, GetProcAddress).
- **Found** (windows-2022: Windows Server 2022 10.0.20348, AMD EPYC 7763; windows-2025:
  Windows Server 2025 10.0.26100, AMD EPYC 9V74):
  - Both: HypervisorPresent 1; capability features 0x2ff (local APIC emulation among
    them); extended exits 0x7fff; processor clock 2,445 and 2,596 MHz; interrupt clock
    200 MHz; synthetic features 0x7f448fff and 0x6fff448fff.
  - **As created, the guest sees no hypervisor**: leaf 1's hypervisor bit clear, the
    0x40000000 leaves zero, no frequency MSRs. A Linux guest would have nothing to
    calibrate its TSC by, with no PIT, HPET or PM timer in shards' hardware-reduced ACPI.
  - **Given the host's synthetic features, it sees Hyper-V**: leaf 1's hypervisor bit set,
    0x40000000 "Microsoft Hv" to 0x40000006, 0x40000001 "Hv#1", 0x40000003 EAX 0x00000e7f
    (the hypercall MSR, the partition reference counter and TSC, the frequency MSRs among
    them) and EDX bit 8 (the frequency MSRs available); the TSC's frequency
    2,445,432,807 and 2,596,141,229 Hz and the APIC timer's 200,000,000 Hz, read from the
    MSRs. Linux's `mshyperv.c` (CONFIG_HYPERVISOR_GUEST, in the pinned kernel) calibrates
    both by them.
  - Invariant TSC (0x80000007 EDX bit 8): set on windows-2022, clear on windows-2025, as
    created or not; 0x40000003 offers no TSC-invariance bit (15) on either.
  - **What an exit costs** (µs, p50 / p90 / p99 / max), the vCPU's run to its exit, and
    WinHvEmulation's decode and callbacks after it:

    | runner | exit | run | emulate |
    |---|---|---|---|
    | windows-2022 | MMIO | 12.1-13.4 / 12.2-13.6 / 12.9-16.3 / 34.6-79.1 | 8.2-9.5 / 8.3-9.6 / 10.6-11.8 / 24.3-54.4 |
    | windows-2022 | port | 12.6 / 12.8 / 13.6-18.9 / 48.4-57.1 | 0.2 / 0.3 / 0.5-0.6 / 7.3-7.8 |
    | windows-2025 | MMIO | 15.6 / 15.9-16.0 / 33.2-34.3 / 54.3-75.6 | 10.5-10.6 / 10.8 / 22.5-25.2 / 34.9-113.1 |
    | windows-2025 | port | 14.5-16.2 / 14.9-16.5 / 28.8-37.2 / 47.9-48.8 | 0.3 / 0.4 / 1.0-1.1 / 10.0-11.3 |

    (Ranges over the two partitions.) A partition was set up in 0.4-31.7 ms, the first of
    a process the slowest; 2 MiB mapped in 0.04-3.6 ms; a vCPU made in 0.09-0.25 ms.
  - Register values handed to WinHvPlatform 8-aligned (windows-sys's WHV_REGISTER_VALUE,
    whose C declaration is `DECLSPEC_ALIGN(16)`) made the process fault
    (STATUS_ACCESS_VIOLATION); 16-aligned, it ran. With the local APIC emulated, a HLT with
    interrupts off never returned to the VMM.
- **Consequence.** shards' WHP backend gives its partitions the host's synthetic features,
  and its guest kernel the Hyper-V reference TSC (CONFIG_HYPERV) where invariant TSC is not
  offered; hands WinHvPlatform 16-aligned register values; and takes virtio's
  notifications by doorbell (a notification port) rather than by an exit and WinHvEmulation
  each, a port exit costing 12.6-16.2 µs here and an MMIO exit 8-11 µs more.

### M198. What shards' WHP backend leans on: an AP's start, interrupts, register access, doorbells, cancels

- **Questions.** Before the backend is written, on the hosts its CI runs: which synthetic
  features a partition sets up with; whether an application processor waits for SIPI as
  created and starts on the emulated APIC's INIT and SIPI; what an interrupt the VMM asks
  for costs and which EOIs come back to it; what a register read and write cost between
  exits, as a decoder of its own would make them; whether a doorbell (a notification port)
  beats an MMIO exit for a virtio notification; and whether a cancel made before a run, or
  while the vCPU is halted in the hypervisor, returns the run.
- **Method.** `docs/research/measurements/whp-run/src/bin/m198.rs`, by `whp-run.yml`, runs
  38211215004, 38211379138 and 38211631065 on branch `whp`, 2026-10-12, each on a
  windows-2022 and a windows-2025 runner (AMD EPYC, 2,445 or 2,596 MHz, as each run was
  given): partitions of two vCPUs, the local APIC emulated (XApic), 2 MiB of RAM; a boot
  processor in long mode with a GDT and an IDT, its handler counting and writing the APIC's
  EOI; the host asking interrupts of it (WHvRequestInterrupt) one at a time, 200
  edge-triggered and 200 level-triggered, each timed until the handler had counted it; 1,000
  doorbell writes; 1,000 round trips each through a doorbell and through an MMIO exit whose
  VMM thread signals the same kind of event, a host thread answering in guest memory, timed
  by the guest's TSC; 1,000 register reads and writes on the stopped vCPU; partitions set up
  with the host's synthetic features less each one, and with shards' set plus each other.
- **Found:**
  - **Features.** WHvSetupPartition refuses (HV_STATUS_INVALID_PARAMETER, 0xC0350005)
    synthetic timers without the SynIC, and any bank without HypervisorPresent; it takes
    shards' set, 0x7000bcb (HypervisorPresent, Hv1, the reference counter, the APIC's MSRs
    and VP assist page, hypercalls, the VP index, the reference TSC, the frequency MSRs,
    extended processor masks, TLB-flush and cluster-IPI hypercalls). Its guest sees
    0x40000003 EAX 0x00000a72, EDX 0x00040130 (the frequency MSRs available, no synthetic
    timers), and 0x40000004 EAX 0x00000e2c: remote TLB flushes, APIC access and cluster IPIs
    by hypercall recommended. Leaf 1 offers neither x2APIC nor TSC-deadline, and leaf 0xB is
    zero.
  - **vCPUs.** Partitions set up with up to 1,024 (2,048 refused on windows-2022, taken on
    windows-2025).
  - **An AP as created** is held: its activity register's StartupSuspend set (vCPU 0's
    clear), its APIC base 0xfee00800 (vCPU 0's 0xfee00900, the BSP's), at the reset vector;
    run alone, it stays in the hypervisor until canceled (300 ms). INIT then SIPI written to
    the boot processor's emulated APIC start it in real mode at the vector's page
    (CS 0x0800), every run.
  - **Interrupts.** A call to WHvRequestInterrupt costs 3.8-6.3 µs at p50 (p99 4.6-20.8); an
    edge-triggered one is handled 6.5-8.0 µs after its call began (p99 7.7-34.2), a
    level-triggered one 13.5-19.7 µs (p99 16.9-32.3), its predecessor's EOI having
    exited to the VMM first. Every level-triggered EOI exits (WHvRunVpExitReasonX64ApicEoi,
    with its vector), 200 of 200; no edge-triggered one does.
  - **Registers.** A read of one register and a write of two, between exits: 0.1 µs each at
    p50, 0.1-0.2 at p99 (max 2.8). An I/O exit carries its instruction's length (1 for OUT); an MMIO
    exit carries its instruction's bytes (16) but no length (0), and its GPA, unmapped.
  - **Doorbells against exits.** A doorbell write cost the guest 5.9-22.1 µs, by run and
    runner; a round trip through one 17.4-29.0 µs; through an MMIO exit 17.1-24.4 µs. Of the
    four pairs (the first run set no partition up), the doorbell won one, by 0.1 µs, and
    lost three, by 0.5-4.6 µs.
  - **Cancels.** One made before a run returns that run at once (0-1 µs), canceled; the
    next runs on. One made while the vCPU is halted in the hypervisor returns its run 42-127
    µs later.
- **Consequence (D140).** The backend offers 0x7000bcb of what the host offers; leaves APs
  held as created; emulates the IOAPIC itself, asking WHP for each interrupt by its
  redirection entry and taking each level-triggered EOI back from its exit; decodes MMIO
  instructions itself, at two register calls' cost, not WinHvEmulation's 8-11 µs (M197);
  takes virtio notifications as MMIO exits, without doorbells; and kicks a vCPU with a
  cancel, which a run about to start takes too.

### M199. AF_UNIX stream sockets on Windows, before shards' Windows transport is built on them

- **Questions.** Can AF_UNIX sockets carry shards' vsock streams and builder sockets on
  Windows, as Unix sockets do elsewhere: does WSAPoll report them, does half-close work,
  does scatter-gather, does an inherited one work in a child process; what does a connected
  pair cost, made as Windows must make one (no socketpair()); and what are their
  throughput and latency beside loopback TCP's?
- **Method.** `docs/research/measurements/windows-afunix`, by `windows-afunix.yml`, run
  38213172739 on branch `whp`, 2026-10-12, on a windows-2022 and a windows-2025 runner:
  100 pairs made through a listener at a fresh path (bind, listen, connect, accept, the path
  removed), each timed; a half-close and the other direction after it; WSAPoll on a
  non-blocking end, quiet, with data, drained, after the peer's shutdown(SD_SEND) and
  after its close, and as its send buffer fills and drains; WSASend and WSARecv of three
  buffers; WSAEventSelect (FD_READ, FD_WRITE, FD_CLOSE) on one end, the event waited
  for after each step: associated, data, a full send buffer and its draining, the peer's
  half-close, and a full send buffer after end-of-stream and its draining (run
  38213370757); a socket inherited by a child process (std's CreateProcessW inherits
  inheritable handles), which writes through it; 1 GiB in 64 KiB writes between two
  threads, and 10,000 one-byte round trips, each over AF_UNIX and over loopback TCP with
  TCP_NODELAY.
- **Found** (windows-2022 / windows-2025):
  - A pair costs 609.5 / 699.8 µs at p50, 917.1 / 967.5 at p99, max 2,039.0 / 1,569.7.
  - Half-close: after shutdown(SD_SEND) the peer reads the data, then end-of-stream (0);
    data still goes the other way. Scatter-gather: three buffers sent and received whole.
  - WSAPoll reports writability (WRNORM) as a socket is quiet; RDNORM and WRNORM with
    data; WRNORM again once drained. A peer's half-close and its close both read
    **WRNORM|HUP, without RDNORM**: end-of-stream is reported as a hangup, and a hangup
    is reported whatever was asked. A full send buffer (131,072 bytes; WSAEWOULDBLOCK)
    reports nothing, and WRNORM again once drained.
  - WSAEventSelect records, on both runners: FD_WRITE as associated; FD_READ with data;
    nothing while the send buffer is full (the wait times out, 100 ms), FD_WRITE once
    drained; FD_CLOSE on the peer's half-close; and after end-of-stream has been read,
    again nothing while full and FD_WRITE once drained.
  - Inherited, a socket works in the child: 14 bytes written there, read here.
  - What a writer wrote before it exited is read whole once it has gone, whether it closed
    nothing, closed its socket, shut its writing down, or both, while the parent still holds
    a copy; a parent's copy closed first, by closesocket or by CloseHandle, leaves the
    child's working (runs 38218844747, 38219105752). But **where the writer's exit closes
    the socket's last reference, the peer's read fails, WSAECONNRESET (10054), and what was
    written is lost**; shut down and closed before the exit, it is read whole (run
    38219105752).
  - Throughput: AF_UNIX 11,954 / 9,020 MB/s, loopback TCP 3,277 / 2,694. A one-byte round
    trip: AF_UNIX p50 29.1 / 31.3 µs, p99 44.0 / 63.1; TCP 37.0 / 39.1, p99 69.3 / 116.8.
- **Consequence.** shards' Windows streams are AF_UNIX sockets, as its Unix hosts' are:
  3.4-3.7 times loopback TCP's throughput, and a round trip 20 percent shorter. A pair
  costs some 0.6 ms more than socketpair(2), and is made once a stream. WSAPoll cannot
  wait to write on a socket whose peer has half-closed: it reports the hangup at once,
  whatever is asked, so such a wait would spin. WSAEventSelect's events have no such fault:
  the vsock device's Windows poller sleeps on them, and reads each socket's state with a
  WSAPoll that does not wait, end-of-stream (HUP) a read's and never a write's. And a
  Windows process of shards' shuts down and closes its sockets before it exits: an exit
  that leaves the last reference to the system loses what the peer has not yet read.

### M200. A WHP partition from a confined process, and from a standard user

- **Questions.** shards confines its VM process (D30: Landlock and seccomp on Linux, App
  Sandbox on macOS); on Windows the like is an AppContainer, which reaches no file but by a
  handle given it. Can a process in one run a WHP partition? Can a standard user, not an
  administrator as GitHub's runner account is? And who may open what WHP opens?
- **Method.** `docs/research/measurements/whp-run/src/bin/m200.rs`, by `whp-run.yml`, on
  a windows-2022 and a windows-2025 runner, 2026-10-12 (runs 38214206152, 38214574386,
  38214718457, 38215445267): the probe launches itself as a child with exactly the handles named
  for it (PROC_THREAD_ATTRIBUTE_HANDLE_LIST: a file of its own, opened, and its stdout
  and stderr); the child reads the file through the handle, opens it by its path, and
  runs a partition of one vCPU to a port write. Three ways: as the runner's user; in an
  AppContainer of no capabilities (CreateAppContainerProfile,
  PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES); in that AppContainer holding one
  capability SID, the one the device's DACL names (below). Then as a standard user the
  workflow makes (New-LocalUser), through CreateProcessWithLogonW. The device: found
  among vid.dll's own strings, opened for READ_CONTROL alone, its descriptor read
  (GetSecurityInfo) as SDDL; the capability's name looked for by deriving each name the
  system knows (DeriveCapabilitySidsFromName: the registry's AllCachedCapabilities, and
  every Capability Name of the app manifests under SystemApps and WindowsApps).
- **Found** (both runners alike):
  - As the runner's user, the child reads the file both ways and runs the partition.
  - In an AppContainer of no capabilities: the handle reads, the path is refused
    (ERROR_ACCESS_DENIED), and **WHvCreatePartition is refused (E_ACCESSDENIED)**.
  - WinHvPlatform's functions reach the hypervisor through vid.dll, which opens
    `\\?\VidExo`, whose descriptor is
    `O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1201bf;;;AU)(A;;0x1201bf;;;S-1-15-3-1024-1350733694-1533467442-3402045280-1710179238-1008639688-2282869650-3611451526-669908847)S:AI(ML;;NW;;;LW)`:
    full access for SYSTEM and Administrators; read, write and execute (0x1201bf) for
    Authenticated Users and for one capability SID; a low integrity label.
  - **In the AppContainer holding that capability SID, the partition runs**, and the path is
    still refused, the handle still read.
  - As a standard user, the partition runs.
  - The capability's name is none of the 413 / 446 names the system knows (339 / 357
    cached in the registry, the rest from manifests).
- **Consequence (D140).** shards runs on Windows as a standard user, as it runs rootless
  elsewhere. Its Windows VM process runs in an AppContainer of its own, which reaches no
  file but the handles its spawner gives it, holding the capability VidExo's DACL grants,
  and that alone: read from the device's own descriptor as the VM process is spawned, not
  written into shards, so a Windows that grants another reaches no stale SID.

### M201. The Windows VM process confined as shards would confine it

- **Questions.** Each part of the design D141 builds on M200's AppContainer: does one need
  a profile, or does a SID derived from a name suffice (nothing per VM left in the
  registry or on disk)? Do handles given by DuplicateHandle into a suspended child work
  there, so that none is ever inheritable but a pipe that tells the child their values?
  Does a Job that kills the child as its last handle closes, and lets it run no process,
  hold; and do Windows' mitigations leave WHP working? Can the child reach an AF_UNIX socket
  in a directory whose DACL grants its SID, and not one in a directory whose DACL does
  not? What does a profile per VM cost?
- **Method.** `docs/research/measurements/whp-run/src/bin/m201.rs`, by `whp-run.yml`, runs
  38218366702, 38218592189 and 38218844759 on branch `whp`, 2026-10-12, each on a
  windows-2022 and a windows-2025 runner: the capability SIDs read from `\\?\VidExo`'s DACL
  (M200); a child made suspended in an AppContainer holding them (a profile made by
  CreateAppContainerProfile, or a SID from DeriveAppContainerSidFromAppContainerName alone),
  in a Job (PROC_THREAD_ATTRIBUTE_JOB_LIST: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE and an active
  process limit of 1), with the child process policy (PROCESS_CREATION_CHILD_PROCESS_RESTRICTED)
  and the mitigations, each alone and together (strict handle checks, no Win32k, no
  extension points, no dynamic code, no images from remote devices or of a low label; and
  the second word's no-FSCTL); given a file and a socket by DuplicateHandle, their values
  through a pipe, its one inheritable handle; then resumed. The child reads the file,
  writes the socket, dials a socket in a directory granted its SID (SetEntriesInAclW,
  GENERIC_ALL, inherited) and one in a directory not, runs a partition, and starts a
  process. A child left running is ended by closing its Job's handle. Profiles made and
  deleted, 20 of each, timed.
- **Found** (both runners alike but where said):
  - With no profile, CreateProcessW refuses the AppContainer (ERROR_FILE_NOT_FOUND). With
    one, the child runs: it reads the file and writes the socket given it, dials the socket
    in the directory granted its SID and is refused the other (WSAEACCES, 10013), and runs
    the partition (3.3-3.9 ms, partition made to port write).
  - The Job's process limit refuses the child a process (ERROR_NOT_ENOUGH_QUOTA, 1816), and
    the child process policy refuses it apart from the Job (ERROR_CHILD_PROCESS_BLOCKED,
    367). Closing the Job's last handle ends the child 0.7-1.1 ms
    later.
  - Every mitigation leaves WHP working on windows-2025, both words together. On
    windows-2022, **no dynamic code** (ACG) makes WHvSetupPartition fail
    (HRESULT_FROM_WIN32(ERROR_DYNAMIC_CODE_BLOCKED), 0x80070677), and CreateProcessW refuses
    the second word (ERROR_INVALID_PARAMETER); every other mitigation leaves it working.
  - A profile costs 4.4 / 5.7 ms at p50 to make (p99 553 / 6.5 ms), 1.8 / 2.1 ms to delete.
  - A socket given by DuplicateHandle and written by the child was read by no one once the
    child had exited: the child held its last reference (M199: an exit that closes it
    loses what was unread).
- **Consequence (D141).** The VM process runs in an AppContainer of its own, a profile made
  for it and deleted after it (made ahead of a request where a start's time counts),
  holding the capabilities VidExo's DACL grants; in a Job that ends it with its spawner and
  lets it start nothing; with the mitigations its Windows takes, which dynamic code is not
  where WHP needs it (windows-2022). Files and other handles reach it by DuplicateHandle
  alone; sockets it makes itself, in a directory of its own whose DACL grants its SID.


### M202. Dynamic code forbidden once a partition is set up

- **Question.** On windows-2022 a process spawned with ACG (no dynamic code) cannot set a
  partition up (M201). A process may forbid itself dynamic code later, for good
  (SetProcessMitigationPolicy, ProcessDynamicCodePolicy). Does every WHP call the VMM makes
  after a partition's setup still work then, so that the VM process can turn ACG on before
  it reads anything a guest wrote?
- **Method.** `docs/research/measurements/whp-run/src/bin/m202.rs`, by `whp-run.yml`, run
  38220452267 on branch `whp`, 2026-10-12, on a windows-2022 (AMD EPYC 7763) and a
  windows-2025 (Windows NT 10.0.26100) runner: a partition of two vCPUs set up, then ACG
  turned on, then each call that follows setup in a VM's life: guest memory mapped, a vCPU
  created on the main thread and one on another thread, registers set, a run to its exit
  (a port write), an interrupt asked, a run canceled, part of guest memory unmapped and
  mapped again, a vCPU's CPUID read, a vCPU deleted; last, a second partition's setup.
- **Found:**
  - ACG turns on (SetProcessMitigationPolicy returns 1) and every call after the setup
    works on both runners. The run exits at the guest's port write (0x501), as it does
    without ACG.
  - A second partition's setup works on windows-2025 and fails on windows-2022 with
    HRESULT_FROM_WIN32(ERROR_DYNAMIC_CODE_BLOCKED) (0x80070677), as a process spawned with
    ACG fails its first (M201).
  - The vCPU's CPUID leaf 0x40000005 reads 0 here as in M197's partitions: this partition
    was not offered the synthetic features, which a VM's is (D140).
- **Consequence (D141).** The VM process sets its one partition up, then forbids itself
  dynamic code before it loads the kernel or runs a vCPU, on every Windows the runners
  have. Nothing it does after that point needs dynamic code. The library never does this
  itself: tests set up many partitions in one process, which windows-2022 refuses after ACG.

### M203. Sockets and files a confined VM process is handed

- **Questions.** On Windows the VM process runs in an AppContainer that reaches no socket and
  no file outside what its DACLs grant it (M201), so its spawner must bind its vsock
  listener for it and dial the host ports its guest connects to, as on macOS (M67). Does a
  listener so bound, in a directory that does not grant the container, work in the child:
  does WSAEventSelect see a connection, WSAPoll report it, accept take it? Do connected
  sockets so handed work? Each by DuplicateHandle and by WSADuplicateSocketW, which
  Winsock's documentation names for sharing a socket; and by WSADuplicateSocketW with the
  spawner's copy closed before the child makes its own. Does GetFinalPathNameByHandleW
  name a file handed in? What does a dial through the spawner cost? Does a VM binary in a
  directory only its user, SYSTEM and Administrators may read start in an AppContainer?
- **Method.** `docs/research/measurements/whp-run/src/bin/m203.rs`, by `whp-run.yml`, run
  38223223296 on branch `whp`, 2026-10-12, on a windows-2022 and a windows-2025 runner. A
  child made as D141 makes the VM process (M201: its own AppContainer profile, VidExo's
  capabilities, a Job of one process killed with its last handle, no child processes, the
  mitigations) is handed two listeners, three connected sockets and a file, all in a
  directory that does not grant its SID; the spawner dials each listener and answers each
  connection's far end. Then 200 dials through the spawner each way, a byte asking over a
  socket and the answer a socket's protocol info or a handle's value, timed in the child
  from the ask to a socket it can use, each dial's socket written a byte and closed by the
  child; and 200 dials the spawner makes for itself. Last, the probe's binary is copied to
  a directory given a protected DACL of the user, SYSTEM and Administrators alone, and run
  from there in an AppContainer.
- **Found** (both runners alike):
  - Both listeners work in the child, by either way: its event is set for the
    connection, WSAPoll reports it (POLLRDNORM), accept takes it, and the line each way
    arrives.
  - The three connected sockets work in the child, each way: a line written there is read
    by the far end, and its answer raises the child's event and is read. One made from
    WSADuplicateSocketW's info works whose spawner closed its own copy before the child
    ran.
  - GetFinalPathNameByHandleW refuses the child a handed file's path, normalized and as
    opened alike: access denied (5).
  - A dial through the spawner takes the child 111.0 / 110.7 µs at p50, 197.4 / 196.6 at
    p99 (windows-2022 / windows-2025) by WSADuplicateSocketW; 99.9 / 105.1 at p50, 150.9 /
    154.6 at p99, by DuplicateHandle. The spawner's own dial: 41.0 / 38.3 µs at p50, 58.9 /
    78.2 at p99. Every byte written through the 400 dialed connections arrived.
  - The binary in the private directory starts in its AppContainer and exits 0, as it
    does from the probe's own directory.
- **Consequence (D141).** A VM process on Windows is handed its files by DuplicateHandle,
  each with the path the spawner resolved for it, since the VM cannot ask; and its sockets
  by WSADuplicateSocketW, the way Winsock documents, 6-10 µs more a dial at p50 than
  DuplicateHandle. Its spawner binds its vsock listener and dials its host ports for it, as
  on macOS. The carried VM binary needs no grant to its directory.

### M204. What a confined network process may reach, and doorbells over a section

- **Questions.** A VM's network process parses what a guest and the Internet send, so on
  Windows it is to be confined as the VM process is (D141), in an AppContainer. Which of
  its flows does an AppContainer allow: TCP and UDP to the Internet, with which
  capabilities? A connection to a host service on the loopback, and one from the host to
  a listener of its own, as a published port's? A listener its spawner bound and shared
  with it? And what does a doorbell cost between two processes, an event each way over
  shared memory, beside AF_UNIX's round trip (M199)?
- **Method.** `docs/research/measurements/whp-run/src/bin/m204.rs`, by `whp-run.yml`, run
  38228876254 on branch `whp`, 2026-10-12, on a windows-2025 (Intel Xeon 6973P-C) and a
  windows-2022 (AMD EPYC 7763) runner. A child made as D141 makes the VM process (an
  AppContainer of its own, a Job, the mitigations; M201), twice: holding no capability,
  then the network capabilities internetClient, internetClientServer and
  privateNetworkClientServer (S-1-15-3-1, -2, -3). Each time it connects by TCP to
  github.com's address (resolved by its spawner) and asks 1.1.1.1:53 by UDP for github.com's
  address; connects to a listener of its spawner's at 127.0.0.1; listens at 127.0.0.1 and at
  the host's own non-loopback address, which its spawner dials; and accepts on a listener at
  127.0.0.1 its spawner bound and shared (WSADuplicateSocketW). Each step is given up after
  5 s. Then 10,000 doorbell round trips, timed at the spawner: an auto-reset event set, the
  child woken, a count in a pagefile-backed section (CreateFileMappingW) incremented, and
  the other event set; both events and the section handed by DuplicateHandle.
- **Found** (both runners alike):
  - Without a capability, its TCP to the Internet is refused at once (WSAEACCES, 10013) and
    its DNS query goes unanswered. With the network capabilities both work: connected in
    45.2 / 51.5 ms (the spawner's own: 38.3 / 56.1), answered alike.
  - Loopback is closed both ways whatever it holds: its connection to its spawner's
    listener, and its spawner's to its own listeners, at 127.0.0.1 and at the host's own
    address alike, are answered by nothing in 5 s.
  - A listener its spawner bound at 127.0.0.1 and shared with it takes its spawner's
    connection, with or without capabilities.
  - A doorbell round trip: p50 28.8 / 22.8 µs, p90 45.0 / 31.9, p99 57.3 / 49.9, max
    152.4 / 213.5 holding no capability; 35.5 / 22.8, 48.8 / 29.6, 64.5 / 43.3, 103.3 /
    112.4 holding the network capabilities (windows-2025 / windows-2022; n = 10,000).
    Every round's count arrived (10,001 rounds answered, the first untimed).
- **Consequence (D142).** A network process on Windows runs in an AppContainer of its
  own holding the network capabilities: the Internet it reaches itself. The host's
  loopback it reaches through its spawner, which binds a published port's listeners and
  shares them, as it binds a VM's vsock socket (M203), and dials for it what is on the
  host's loopback. A doorbell costs what AF_UNIX's round trip does (29.1 / 31.3 µs at
  p50, M199).

### M205. AF_UNIX connects that do not wait, on Windows

- **Question.** A nonblocking connect to a Unix socket on Linux and macOS connects at once, or
  is refused (ECONNREFUSED where nothing listens, EAGAIN where the backlog is full): what
  shards' vsock device and its network stack's proxy dial take. On Windows both refused
  every connection (CI run 38235548677: a guest's connection to a host port reset, the
  proxy's never made). What does afunix.sys answer?
- **Method.** `docs/research/measurements/windows-afunix` (`m205`), by `windows-afunix.yml`,
  run 38239766177 on branch `whp`, 2026-10-12, on a windows-2025 and a windows-2022 runner:
  1,000 nonblocking connects to a listener with room, each's return and the time until
  WSAPoll says its socket may be written; one to a path whose listener has closed; up to 64
  to a listener of backlog 1 that accepts none, then one accept; and a blocking connect to
  that full backlog, an accept coming 500 ms later.
- **Found** (windows-2025 / windows-2022 alike):
  - Every nonblocking connect to a listener with room returned WSAEWOULDBLOCK (10035) and
    was writable (POLLWRNORM) 11.7 / 10.4 µs later at p50, 19.8 / 15.6 at p99, 62.0 / 39.9 at
    most (n = 1,000).
  - Where nothing listens: WSAECONNREFUSED (10061) at once.
  - A backlog of 1 took five connects, each under way then writable in microseconds; every
    later one was refused at once with WSAENOBUFS (10055), and stayed refused after an
    accept made room.
  - A blocking connect to the full backlog failed with WSAENOBUFS in 140-215 µs, not
    waiting for the accept.
- **Consequence (D141).** shards' AF_UNIX connect that does not wait waits out Windows'
  "under way", which ends in microseconds where the listener has room: so it connects at
  once or is refused, as Unix's does; WSAENOBUFS is Unix's EAGAIN, a full backlog.

### M206. The CPUID a WHP partition's vCPUs see, and what a VMM may change of it

- **Question.** A Linux guest's AP skips calibrating its delay loop where the TSC is
  constant and a CPU of its package is up already (`calibrate_delay_is_known`,
  arch/x86/kernel/tsc.c; a Hyper-V guest without the TSC-invariance privilege marks its TSC
  unstable, so only the second path is open). shards' WHP guests calibrated every AP: 63
  of them took a 240-vCPU boot past a minute on a 4-CPU runner, starving every VM beside
  it (CI run 38240024887). What topology and invariant TSC does a partition set up as
  shards' are give its vCPUs, and what may the VMM change, how, and at what cost? Does the
  guest's TSC keep one rate?
- **Method.** `docs/research/measurements/whp-run/src/bin/m206.rs`, by `whp-run.yml`, runs
  38246266456 and 38247442481 on branch `whp`, 2026-10-12, on a windows-2022 (Windows
  Server 2022 10.0.20348) and a windows-2025 (Windows Server 2025 10.0.26100) runner, both
  AMD EPYC 7763: the host's processor features (WHvGetCapability) and its own CPUID; the
  CPUID of each of four vCPUs (WHvGetVirtualProcessorCpuidOutput) of a partition set up as
  shards' (XApic, shards' synthetic features), checked against a guest's own CPUID on
  vCPU 0; the same given the host's processor features, with TscInvariantSupport asked of
  them, and with WHvPartitionPropertyCodeCpuidResultList2 results (per vCPU and for all,
  masked) set before setup and after the vCPUs exist; a second list after a first, and an
  empty one; a guest's TSC read against the partition's reference counter
  (HV_X64_MSR_TIME_REF_COUNT) every 10 ms for 3 s, a pair re-read where the counter moved
  more than 5 µs across it, the host idle and then every host CPU busy; and setting a
  list for one package once a partition's vCPUs exist, for 64 vCPUs and for the 240 a
  guest may have (n = 20 each).
- **Found:**
  - **Topology.** Each vCPU is a package of one: leaf 1's processor count 0 and HTT clear,
    0x80000008 ECX 0 (one thread, no APIC id bits), 0x8000001E's core id 0 on every vCPU;
    the hypervisor gives each its own APIC id in leaf 1 (EBX 31:24) and 0x8000001E EAX.
    Leaf 0 says 0xD, so no 0xB or 0x1F. The guest's own CPUID agreed with the API's on
    every leaf, in every partition.
  - **Invariant TSC.** The host's own CPUID has it (0x80000007 EDX bit 8) on both. Windows
    Server 2022 offers it (TscInvariantSupport in its processor features, bank 1 0x1cf) and
    its partitions' guests see it; Server 2025 does not (bank 1 0x200001ed), and its
    guests do not, as created or given the host's features; asked of the processor
    features there, it was taken without an error and the guest still did not see it.
  - **Results.** A list's masks say which bits are its own: a result of mask 0 changed
    nothing; one of the whole leaf for every vCPU gave all four leaf 1's EAX 0 and APIC id
    0. Masked, the rest stays the hypervisor's: leaf 1's count and HTT set with each vCPU's
    APIC id kept. Per-vCPU results set before setup fail the setup,
    HV_STATUS_INVALID_VP_INDEX (0xc035000e); set once the vCPUs exist, they are taken,
    and so is invariant TSC set there (on Server 2025 too). A second list replaces the
    first, and an empty one clears it.
  - **The TSC's rate.** Over 3 s: 2,445,426,012 Hz idle and 2,445,426,003 loaded on Server
    2022, 2,445,425,778 and 2,445,425,783 on Server 2025, each within 0.02 ppm of the
    partition's clock (WHvCapabilityCodeProcessorClockFrequency); each 10 ms's rate within
    3.3-4.1 ppm of the whole run's at p50, 43-101 at p99, 109 at most (n = 299 each), where
    the pairs' 5 µs windows allow 1,000.
  - **Cost** (µs, p50 / p90 / p99 / max, Server 2022 then 2025): setting 131 results for 64
    vCPUs 726 / 734 / 846 / 846 and 907 / 974 / 1,179 / 1,179; 483 for 240, 2,928 / 2,961 /
    3,347 / 3,347 and 3,709 / 3,783 / 4,035 / 4,035; against 7.6 / 9.9 ms and 29.1 / 37.2
    ms at p50 to make the partition and its vCPUs.
- **Consequence (D140).** shards' WHP VMs give their guests one package of single-thread
  cores, as KVM's do (`cpuid::for_vcpu`), and invariant TSC where the host's processor has
  it, which Server 2025 leaves out though its partitions' TSC keeps the host's one rate and
  a partition never moves to another host; each result masked to the bits it changes, so
  that the hypervisor keeps the bits it keeps per vCPU and as the guest runs; one list,
  set once the last vCPU exists and before any runs. A guest's APs then take the boot
  CPU's calibration.

### M207. A send while another thread waits to read the same connection, on Windows

- **Question.** On Windows CI the vsock tests' streams stalled for a minute with nothing
  read by the VM, then went on as each client gave up (CI run 38243944747): the test's
  client waits for the echo in a read of 60 s timeout, a WSAPoll then a recv on its
  socket, while another thread sends through a copy of it. Does a send wait for another
  thread's wait to read the same connection?
- **Method.** `docs/research/measurements/windows-afunix` (`m207`), by `windows-afunix.yml`,
  run 38251215070 on branch `whp`, 2026-10-12, on a windows-2022 and a windows-2025 runner,
  and run 38248859064 before it, which hung at its second case: for AF_UNIX and loopback
  TCP pairs, their sockets made without WSA_FLAG_OVERLAPPED and with it, one thread waits
  to read a connection, by a WSAPoll of 3 s, a blocking recv, or a recv of SO_RCVTIMEO 3 s,
  on the socket or on a copy WSADuplicateSocketW made of it; 100 ms on, another sends a
  byte through the socket or the copy, and the peer waits for it; at 4 s the peer sends
  the byte that ends a wait still waiting.
- **Found** (both runners alike, AF_UNIX and TCP alike):
  - **Made without WSA_FLAG_OVERLAPPED**, every send waited for the other thread's wait to
    end, through the socket or the copy, on either: 2,895-2,913 ms for a WSAPoll's 3 s,
    3,899-3,901 ms for a recv the peer's byte ended at 4 s; and SO_RCVTIMEO never ended a
    recv, which the peer's byte did at 4 s. Run 38248859064's send behind a blocking recv
    that only its own peer would end hung until the job's limit.
  - **Made with it**, every send went at once (0.0-0.1 ms), the peer had the byte as soon,
    and SO_RCVTIMEO ended each recv after 3,012-3,032 ms (WSAETIMEDOUT), on the socket and
    on the copy alike.
- **Consequence (D141).** Windows serializes the I/O of a handle opened for synchronous
  I/O, and a socket made without WSA_FLAG_OVERLAPPED is one; std makes its sockets
  overlapped. shards' AF_UNIX sockets (`shards-afunix`) are made overlapped too, so that
  a read never holds a send; their read timeout stays a WSAPoll before the read, as
  Winsock leaves a connection whose receive timed out in an indeterminate state. The
  vsock tests' stall was their own client's.

### M208. The environment a child in an AppContainer must be given

- **Question.** shards gives its Unix children `SHARDS_LOG` and `SHARDS_TIMING` alone (the
  processes a guest could take over carry none of their spawner's secrets). Given those and
  `SystemRoot` on Windows, every confined child was refused (CreateProcessW:
  ERROR_ENVVAR_NOT_FOUND, 203; CI run 38256632140). What must a child made in an
  AppContainer be given, and what does it have then?
- **Method.** `docs/research/measurements/whp-run/src/bin/m208.rs`, by `whp-run.yml`, runs
  38260099347 and 38260506180 on branch `whp`, 2026-10-12, on a windows-2022 and a
  windows-2025 runner: a child made as D141 makes the VM process (an AppContainer of its
  own, a Job, the mitigations), given each of: this process's environment; none; and
  `SystemRoot`, `LOCALAPPDATA`, `USERPROFILE`, `TEMP` and `TMP` in several combinations.
  The child prints every variable it has, its temporary directory and whether it may write
  a file there, WSAStartup's result, and whether it makes an AF_UNIX and an AF_INET socket.
- **Found** (both runners alike):
  - **Without `LOCALAPPDATA` no child is made** (203), whatever else it is given.
  - **Windows rewrites what it is given**: the child's `LOCALAPPDATA` is the container's
    own folder (`...\Packages\<profile>\AC`) and its `TEMP` and `TMP` the `Temp` in it,
    whatever they were or were not; it may write a file there.
  - **Without `SystemRoot`, Winsock starts and makes no socket**: WSAStartup returns 0, and
    every socket, AF_UNIX and AF_INET alike, fails with WSAEPROVIDERFAILEDINIT (10106).
  - `LOCALAPPDATA` and `SystemRoot` alone: the child is made, its temporary directory is
    its container's and writable, and it makes both sockets.
- **Consequence (D141, D142, D143).** A Windows child of shards' is given what it is told
  (`SHARDS_LOG`, `SHARDS_TIMING`) and this process's `LOCALAPPDATA` and `SystemRoot` alone;
  its temporary directory is its container's own.

### M209. What a network process asks of its kernel, once confined (D31)

- **Question.** A network process parses what its guest and the Internet send. Landlock
  keeps it from files and from the TCP ports it is not granted, but every syscall stays
  open to it. Which does it make, so that a filter allows those alone
  (rootless-security.md R3), as the VM process's allows its own (M52)?
- **Method.** `docs/research/measurements/vmm-syscalls/collect.sh`: every VM E2E test
  binary under `strace -f` on a KVM host, `build`'s steps that reach the network or a
  proxy among them; `parse.py --program shards-net` counts what each network process
  makes over its life, and with `--after landlock_restrict_self` what it makes once
  Landlock's first layer holds it, where its filter goes. glibc on CI's ubuntu-24.04 and
  musl in rust:1.98.0-alpine, x86_64, b09a55ae, run 38269376753, 2026-10-12: about 640
  network processes each, every one single-threaded.
- **Results.** Over its life, 47 syscalls (glibc), most of them its start's: the dynamic
  loader's (openat, pread64, mprotect, arch_prctl, access) and the runtime's (poll,
  rt_sigaction, prlimit64, sched_getaffinity, rseq, set_robust_list, set_tid_address).
  Once confined, 33 with glibc and 32 with musl:

  | | syscalls |
  |---|---|
  | its flows | socket, connect, bind, accept4, shutdown, getpeername, getsockopt, setsockopt, recvfrom, sendto, recvmsg, sendmsg, read, write, writev, close |
  | its loop | epoll_create1, epoll_ctl, epoll_wait (glibc) or epoll_pwait (musl) |
  | memory | mmap, munmap, mremap, brk (glibc alone) |
  | the rest | fcntl, ioctl, fstat (the ring's size), getrandom, sigaltstack (at its exit), exit_group |
  | its policy's Landlock layer | landlock_create_ruleset, landlock_add_rule, landlock_restrict_self, prctl |

  Their arguments: sockets AF_INET, AF_INET6 and AF_UNIX; ioctl FIONBIO; fcntl
  F_DUPFD_CLOEXEC and F_SETFD; prctl PR_SET_NO_NEW_PRIVS; setsockopt SO_RCVBUF and
  SO_SNDBUF of SOL_SOCKET and TCP_NODELAY of SOL_TCP; getsockopt SO_ERROR of SOL_SOCKET.
  None execs, makes a process or a thread, or opens a file.
- **Consequence (D31).** The network process installs a filter of these, glibc's and
  musl's together, after Landlock's first layer (`confine::filter`): each argument above
  held to its values, setsockopt to the socket's and TCP's levels, prctl's to
  no_new_privs and the thread's name its refusals' report reads, and clock_gettime too,
  which the vDSO answers but where it cannot read the host's clocksource. A syscall
  outside it ends the process with SIGSYS, the syscall named. Run again with the filter
  installed (8b16ffc0, run 38276224826, its count from the filter's install), glibc:
  every test passed under strace and none was refused, and what the filtered processes
  made is all in it; CI's every networked E2E passed through it (run 38276224802). musl:
  CI's network processes made rt_sigprocmask (syscall 14), which none had made under
  strace, and were refused, 482 times over its networked tests: the collection now
  prints each refusal's process's calls before it, to find where. Every one of the 486
  filtered network processes (320530ec, run 38282809609) was refused as its loop first
  ran, its frame buffer allocated: `rt_sigprocmask(SIG_BLOCK, every signal but musl's
  own three)`, which musl's `__block_app_sigs` makes. The loop's first timer reaches a
  thread-local with a destructor, the memory a connection let go of (M210, efc9c889,
  after b09a55ae); std then makes its key for thread-local destructors (`LazyKey`), and
  where the C library gives it key 0, its own mark of none, makes another and deletes 0.
  musl gives 0 first, and its `pthread_key_delete` blocks application signals around its
  work and then sets the mask back; glibc's deletes without a syscall. The filter allows
  rt_sigprocmask's SIG_BLOCK and SIG_SETMASK: a thread's own mask, which grants it
  nothing. Run again with it (922fcbda, run 38291399493), musl: every test passed under
  strace and none was refused; each of the 652 filtered network processes made
  rt_sigprocmask twice, its mask blocked and set back.

### M210. What a network process costs its host as its connections grow (D31, E2)

- **Question.** A VM's network process holds a host socket and a record for each of its
  guest's connections, and queues for the bytes between them. What does it cost in memory
  idle and holding 1,000 and 10,000 connections, beside the VM's own (M64), fresh and after
  they have carried bytes? Does a round trip beside them cost what it does beside none
  (M106)?
- **Method.** `crates/shards/benches/net_memory.rs`: a builder's `RUN` step
  (`testguest conns`) opens N connections to a server of the bench's at this host's address
  on its default route, each fresh or after B bytes each way, then makes 2,000 round trips
  of 64 bytes on one more, and holds them all; 1 s later, past the network process's idle
  time (one RTO), the bench reads the network process's and the VM process's memory
  (macOS: `proc_pid_rusage`'s physical footprint and resident size; Linux: `smaps_rollup`;
  Windows: `GetProcessMemoryInfo`). N = 0, 1,000, 10,000; B = 0, 256 KiB, 1 MiB; 3 rounds
  each. Each change's cost to bulk transfer: `net-throughput/ab.py`, 32 MiB each way through
  a published port, n = 300 per arm, the turns' paired median difference with its
  bootstrap 95% interval. Apple M5 Max, macOS 26.4.1, load average 5.8 to 12, b09a55ae and
  the changes below, 2026-10-12.
- **Results.**
  - Each connection's queues (`VecDeque`) kept their largest fill for the connection's
    life: 1,000 connections idle after 256 KiB each way held 131 KiB each, 132.6 MiB of
    footprint against 1.9 idle.
  - Letting a queue's memory go each time it drained cost bulk transfer +0.2 ms at p50
    [+0.1, +0.4] (2.5%): a streaming connection's queue empties between its host's reads.
    Letting both go once they have stayed empty for an RTO, TCP's own measure of a
    connection gone idle (RFC 7661 §4.4), cost none measurable (+0.1 ms [-0.1, +0.3]); but
    the footprint stayed (173 MiB for 1,000 connections after 1 MiB each way, 375 MiB for
    10,000): the C library's allocator keeps what is freed, dirty. A probe: 1,000 queues
    of 1 MiB, filled and freed, left 1,006 MiB of footprint, which
    `malloc_zone_pressure_relief` freed none of.
  - Queues over memory of their own from the OS (mmap(2); VirtualAlloc on Windows),
    unmapped as they are let go: the footprint follows what is held (below), but bulk
    transfer cost +0.3 ms at p50 [+0.0, +0.5] (2.9%), each new connection's queue zeroing
    fresh pages. Memory let go of kept an RTO for the next queue that grows to its size:
    -0.0 ms [-0.2, +0.1].
  - The network process, as built (footprint and resident size, MiB, p50 of 3):

    | N | fresh | after 256 KiB each way | after 1 MiB each way |
    |---|---|---|---|
    | 0 | 2.6 / 9.8 | 2.6 / 9.8 | 2.6 / 9.8 |
    | 1,000 | 3.8 / 11.0 | 4.6 / 11.8 | 4.6 / 11.8 |
    | 10,000 | 9.7 / 16.9 | 9.5 / 16.7 | 9.5 / 16.7 |

    0.7 KiB a connection at 10,000; the resident size's other 7.2 MiB are clean pages,
    its binary's and the system's libraries', which the footprint leaves out.
  - The VM process beside it, 89.5 MiB of footprint idle, +5.0 MiB for 1,000 fresh
    connections and +44.4 MiB for 10,000: the guest kernel's own sockets, in the guest's
    memory, as any VM's.
  - A round trip beside them (µs, p50 / p99 / max of 6,000): 91 / 196 / 1,052 beside
    none; 81 / 161 / 2,731 beside 10,000; after 1 MiB each way, 57 / 110 / 36,132.
  - On Linux (CI's ubuntu-24.04, AMD EPYC 7763, Linux 6.17, 75d51558, `net costs` run
    38283168175; private / proportional / resident, MiB, p50 of 3):

    | N | fresh | after 256 KiB each way | after 1 MiB each way |
    |---|---|---|---|
    | 0 | 0.7 / 1.1 / 3.0 | 0.7 / 1.1 / 3.0 | 0.7 / 1.1 / 2.9 |
    | 1,000 | 1.2 / 1.9 / 4.0 | 1.2 / 2.2 / 4.7 | 1.1 / 2.2 / 4.7 |
    | 10,000 | 4.7 / 5.8 / 8.3 | 4.7 / 5.7 / 8.3 | 4.7 / 5.8 / 8.3 |

    0.4 KiB a connection at 10,000, whatever it carried. The host's slab, which holds both
    ends of each connection (the bench's server is on the host): +9.6 MiB for 1,000 and
    +91.3 MiB for 10,000, 9.3 KiB a connection. The VM process: 71.7 MiB resident idle,
    +2.6 MiB for 1,000 fresh connections, +41.3 MiB for 10,000. A round trip beside them
    (µs, p50 / p99 / max of 6,000): 213 / 262 / 1,637 beside none, 215 / 267 / 620 beside
    10,000.
    The workloads bench's `network` there (n = 100): +2.5 ms at p50 of runs of 19 to 22 ms,
    p90 +16.2 ms, where it was −0.6 ms at cc147e6: within the run-to-run spread of a loaded
    CI host, which a paired build A/B there is to settle.
  - The start (E1): `build-ab/ab.py`, one build compared with itself, its new arm's runs
    on `--network none` (`FLAGS_NEW`), `exit 0` in the echo image, n = 200 per arm, load
    5.8. A run on the default bridge costs, paired: +418 µs at p50 [+286, +536] of the
    client's wall time (6,126 against 5,666 µs), +216 µs [+200, +227] of it in the guest
    from the request to the command's status (1,142 against 925), and +202 µs [+87, +283]
    outside it. The network process is started with the warm VM, before the request; what
    costs is the request's network: the daemon asks the network process twice before it
    hands the run over (the run's MAC, `NET_MAC`; then its address, peers and names), and
    init sets eth0's MAC and address as the request comes. The workloads bench's
    `network` (`true` less `none`, run by run, n = 50): +130 µs at p50. A run whose MAC
    is made rather than given then took its VM's own, made with the VM, which the VM's
    guest takes as it resumes (`control::MAC`), so neither the daemon nor init does
    anything of it as the request comes: in the guest, from the request to the command's
    status, −12 µs [−72, +30] (1,119 against 1,106 µs; n = 200 per arm, load 4.5 to 6.9).
    Outside the guest the host's load drowned the difference ([−1,058, +320]); the daemon
    asks the network process nothing for such a run on the default bridge. On CI's
    ubuntu-24.04 (AMD EPYC 7763, cc147e6, before that change), the workloads bench's
    `network`, n = 100: −645 µs at p50 of runs of 20 ms, as wide as its spread.
  - macOS gives a host address 16,384 ports for its connections, which TIME_WAIT holds
    30 s whatever their peer (`net.inet.ip.portrange`, `net.inet.tcp.msl` 15000), where
    Linux reuses one for another peer: the bench's cases one after another, whose
    connections the network process closed first, exhausted them (connect: EADDRNOTAVAIL,
    which the network process now says). The bench closes its side first.
  - What a run's network process is told before its VM has the run, asked one ask at a
    time, then all at once with their answers read after the last (`networks::Asks`):
    `build-ab/ab.py`, 922fcbda against it, one template, `--rm --network n` on a user
    network, `exit 0`, n = 200 pairs, load 7.5 to 11 (other work on the host). Without
    peers on it: −282 µs at p50 of wall time [−464, +18], −231 µs outside the guest
    [−389, −11]. With two peers running on it: +105 µs [−474, +635], no difference that
    resolves: each peer's own network process is still asked one after another, its
    socket being others' to use too.
  - What peers cost a run on their network: one home and its daemon, runs alternating
    between a network with two peers running on it (`sleep` containers) and one with none
    (`FLAGS_NEW="--network n"`, `FLAGS_OLD="--network m"`), n = 200 pairs: +6 µs in the
    guest from the request to the command's status [−22, +24], +161 µs of wall time
    [−222, +439]: none that resolves. A first comparison put the peers in a second home,
    copied from the first: +1,118 µs in the guest [+1,089, +1,148]; but with no peers
    running at all the two homes still differed by +302 µs there [+260, +345], so it
    measured the homes, not the peers.
- **Consequence (D31).** A network process's connections cost its host what they hold:
  their queues over memory of their own, let go once idle for an RTO, kept that long for
  the next connection's bytes, then handed back to the OS. Its memory beside a VM's own
  is a few MiB, whatever its connections carried before. E1: a run whose MAC is made, on
  the default bridge, costs its guest nothing of its network as the request comes, and
  what a run's network process takes before the run goes in one round trip; peers on its
  network cost it nothing measurable. Open: the daemon's share on a quiet host; the peers'
  processes asked at once.

### M211. The limits a build's `RUN` step starts with

- **Question.** A container's process starts with Docker's limits (`defaults::limits`,
  measured 2026-10-05). Does a `docker build` step start with the same, and does shards'?
- **Method.** `RUN cat /proc/self/limits` over `alpine:3.20`, `--no-cache`, built by Docker
  29.3.1's `docker build` (its BuildKit, in shards-dind) and by shards' (`shards build`,
  2a3941ea), on an Apple M5 Max, 2026-10-12.
- **Results.**

  | limit | Docker | shards, before |
  |---|---|---|
  | open files | 1048576:1048576 | 1024:4096 |
  | processes | unlimited | 271397 (the kernel's, by the builder's memory) |
  | locked memory | unlimited | 8 MiB |
  | core file | 0:unlimited | 0:unlimited |
  | stack | 8 MiB:unlimited | 8 MiB:unlimited |
  | pending signals | 256734 (its host's kernel's) | 271397 (the builder's) |

  The rest alike. A step kept the builder's kernel's own, where Docker's inherit its
  daemon's, which are its containers'.
- **Consequence.** The builder's init takes Docker's limits before its first step, as a
  run's init does (`defaults::limits`), and every step inherits them; `--ulimit` sets its
  own over them. D133 no longer sizes processes by the builder's memory: they are Docker's,
  unlimited. Pending signals stay the kernel's, by the builder's memory, as Docker's are by
  its host's. `run_steps_start_with_high_file_and_process_limits` holds a step to them.

### M212. A network process's connections to its host, on Windows (D142)

- **Question.** A network process in an AppContainer reaches its host's own addresses and
  loopback only through its spawner's dialer (D142, M204). Does that hold under many
  connections, and what does a connection to the host cost to open, beside one elsewhere?
- **Method.** `crates/shards/benches/net_memory.rs` on CI's windows-2025 (`net costs`): a
  builder's step opens N connections, one after another, to the bench's server at the
  host's own address on its default route; the guest times each connect, its SYN to its
  SYN-ACK (`testguest conns`, `conns connect-us`).
- **Results.** At 6f57f760 (run 38283168175), N = 1,000: the 178th connection's SYN went
  unanswered, its connect timed out (ETIMEDOUT) 136.9 s into the step; the guest's kernel
  gives a SYN up after 127 s (`tcp_syn_retries` 6), so the 177 before it took about 10 s,
  some 55 ms each. That dialer shared each socket while its
  connect was still under way and closed its own copy once told the network process had
  one (`kind::TAKEN`), where Windows documents a shared socket's use as one process making
  the connection and another exchanging data on it (WSADuplicateSocket); and the network
  process waited for each answer in its event loop, every flow of its VM's held up the
  while, each answer listing the host's adapters (GetAdaptersAddresses) to check the
  address.
- **Consequence (D142).** Each ask is numbered and answered once its connect has ended;
  the dialer waits on all its connects and on the network process at once (WSAPoll), and
  the network process asks without waiting, its guest's SYN waiting for the answer as for
  a far end's SYN-ACK (`dials_are_answered_as_they_end_and_hold_up_nothing`). The dialer
  checks an address by binding to it, then connects from it: one call, exact as it dials,
  where the host's addresses change. Its connects' times on Windows, and 1,000 and 10,000
  connections held, are M215's (net costs run 38314187451): a connect's p50 1.0 to
  15.3 ms on windows-2025, the network process's working set 7.5 MiB beside 1,000 idle
  connections and 16.2 to 16.5 MiB beside 10,000.

### M213. What waking a network process's loop costs on Windows, by how it waits (D142)

- **Question.** A network process's loop waits on its sockets, its VM's doorbell and its
  VM's process. On Windows shards' poller (crates/net/src/poll.rs) sleeps on one event
  every socket's network events set (WSAEventSelect) and then looks at every socket
  (WSAPoll). The memory bench's cases of 10,000 connections ran past 15 minutes there
  (M210, M212). What does a wake cost as connections grow, and what would another way of
  waiting cost?
- **Method.** `docs/research/measurements/whp-run/src/bin/m213.rs` on CI's windows-2022
  and windows-2025 (`whp run`): between two threads, 10,000 round trips each, timed by
  the side that starts them. A byte's round trip on one loopback TCP connection beside
  0, 100, 1,000 and 10,000 idle ones, the loop waiting as shards' poller does; on an I/O
  completion port its sockets are registered with, level-triggered
  (ProcessSocketNotifications, Windows 11 and Server 2022 on), which names the ready
  ones; and on the one event, the port asked without waiting which sockets are ready
  (both registrations on each socket), so that the ring's events could stay waited on
  beside it. A doorbell's round trip: two auto-reset events (shards' ring, M204);
  a byte on an AF_UNIX socket, waited on with recv and on a completion port; and an event
  a thread-pool wait posts to a port (RegisterWaitForSingleObject), as a loop on a port
  would need for the ring's events. 0ddd895f (run 38297680093) and ba8b226e (run
  38299243996, which added the last way), 2026-10-12.
- **Results** (µs, p50 / p99, windows-2022 then windows-2025):

  | beside N idle | event + WSAPoll | completion port |
  |---|---|---|
  | 0 | 38.6 / 53.7; 43.3 / 68.9 | 37.7 / 51.0; 41.0 / 69.1 |
  | 100 | 40.1 / 73.8; 41.2 / 80.6 | 36.9 / 56.4; 39.7 / 64.1 |
  | 1,000 | 678.8 / 843.7; 562.0 / 1,177.4 | 37.2 / 47.0; 40.2 / 67.4 |
  | 10,000 | 54,413.8 / 62,052.8; 38,904.2 / 63,701.6 | 37.4 / 50.9; 40.9 / 72.9 |

  The event with the port asked: 42.2 to 43.9 µs at p50 at every count on windows-2025,
  but its slowest round trip 105 to 115 ms at every count, each the 100 ms the probe
  bounds a wait by: the event did not come, where the port had the socket ready. On
  windows-2022 it stalled so often that the job's 20 minutes ran out on its first count.
  The completion port alone was slowest at 8.1 ms (10,000 idle, windows-2025).

  | doorbell | windows-2022 | windows-2025 |
  |---|---|---|
  | two events | 22.6 / 43.1 | 26.7 / 49.0 |
  | AF_UNIX byte, recv | 29.3 / 40.1 | 33.1 / 58.1 |
  | AF_UNIX byte, port | 29.0 / 38.4 | 33.6 / 59.1 |
  | event, thread-pool wait, port | 37.6 / 51.9 | 42.6 / 65.3 |

- **Consequence (D142).** The network process waits on a completion port, its sockets
  registered with it as their interest changes (ProcessSocketNotifications,
  level-triggered), which names those ready: a wake costs what it does beside none, at
  every count. The doorbell its VM rings it by is an AF_UNIX socket, registered with the
  port as its sockets are, a byte a ring: 6.5 to 7.4 µs a round trip more than an event,
  where an event a thread-pool wait posts would cost 15 µs more; its hang-up says its VM
  has gone, as a pipe's does on Unix. The doorbell it rings its VM by stays an event,
  which the VM process's worker waits on with its own. One event and the port asked
  together is not taken: its wakes were lost.

### M214. What Metal costs a process that links it, on macOS (D144)

- **Question.** A Venus renderer on macOS runs Vulkan over MoltenVK, which links Metal,
  QuartzCore, IOSurface and Foundation (gpu-accelerators.md R6). dyld loads a linked
  framework as the process launches. What would linking them cost every VM process, a
  GPU VM's or not, and what does a GPU VM's own Metal device cost it?
- **Method.** `docs/research/measurements/metal-launch` (two binaries, release): one
  linking nothing past libSystem, one linking those four frameworks and referencing
  `MTLCreateSystemDefaultDevice`, run with no argument (dyld's loading alone) and with one
  that makes the system's default Metal device. 300 launches each, alternating, spawn to
  reap; then each held 1 s for its resident size and its physical footprint (`ps`,
  `footprint`). Apple M5 Max, macOS 26.4.1, load 11 to 12.5 (other work on the host),
  2026-10-12.
- **Results.**

  | | launch, p50 / p90 / p99 | resident | footprint |
  |---|---|---|---|
  | plain | 3.18 / 4.21 / 6.38 ms | 1,616 KiB | 1,072 KB |
  | Metal linked | 4.50 / 5.84 / 9.27 ms | 5,888 KiB | 1,552 KB |
  | Metal device made | 22.55 / 28.42 / 44.13 ms | 11,328 KiB | 3,568 KB |

  MoltenVK's static library also needs IOKit, CoreGraphics and AppKit (NSApplication,
  NSScreen and NSView, for surfaces and displays a renderer never makes): its link fails
  without each. A third binary links all seven, as `shards-vm-gpu` must; run again with
  the others, load 10 to 12, at db0c67a0:

  | | launch, p50 / p90 / p99 | resident | footprint |
  |---|---|---|---|
  | plain | 3.05 / 3.50 / 4.12 ms | 1,600 KiB | 1,056 KB |
  | Metal linked | 4.12 / 4.78 / 5.57 ms | 5,840 KiB | 1,520 KB |
  | Metal device made | 21.21 / 24.34 / 29.13 ms | 11,296 KiB | 3,520 KB |
  | MoltenVK's seven linked | 4.97 / 5.78 / 6.31 ms | 8,432 KiB | 1,728 KB |
  | MoltenVK's seven, device made | 21.81 / 25.27 / 29.36 ms | 13,936 KiB | 3,808 KB |

- **Consequence (D144).** Linked into shards' VM process, Metal would cost every VM 1.3 ms
  of launch at p50 and 480 KB of footprint, GPU or none (R1: a GPU costs nothing where it
  is not used); what MoltenVK links, 1.9 ms and 672 KB. A GPU VM runs a VM process of its
  own build, `shards-vm-gpu`, which links the renderer and MoltenVK and is carried and
  written out as the VM process is (D36); every other VM's links neither. A GPU VM pays
  its device's 17 to 19 ms as it boots, in its pool's refill.

### M215. What changing a socket's registration with a completion port does (D142)

- **Question.** The network process's poller registers each socket with its completion
  port (ProcessSocketNotifications), level-triggered, and changes the registration as the
  socket's interest changes. A connection waits for nothing for a while when its guest's
  window is full and it has nothing for its host, then for its input again. Windows'
  documentation says a re-registration overwrites a registration, a filter of
  SOCK_NOTIFY_REGISTER_EVENT_NONE issues nothing, a disabled registration must not be
  enabled until its DISABLE notice comes (no such notice is defined), and a removed one
  posts a REMOVE notice. What does each way of waiting for nothing, then for input, do?
  And one-shot registrations, which a notice disarms?
- **Method.** `docs/research/measurements/whp-run/src/bin/m215.rs` on CI's windows-2022
  and windows-2025 (`whp run`), 200 rounds of each way on a fresh loopback connection: a
  persistent level-triggered registration for input; then OP_REMOVE, or OP_REMOVE and its
  notice, or OP_DISABLE, or OP_ENABLE filtering nothing; a byte sent, any notice within
  20 ms counted; OP_ENABLE for input again, its notice within a second counted. One-shot:
  armed for input; a byte; its notice; 20 ms more; armed again; armed for output; armed
  for input with nothing to read, then overwritten for output. 87058f85, run
  38314187392, 2026-10-12.
- **Results** (both runners alike; each change's p50 / p99 on windows-2022):

  | way | refused | told while waiting for nothing | told when waiting again |
  |---|---|---|---|
  | remove, enable at once | 0 of 200 | 0 | 200 |
  | remove, its notice, enable | 0 of 200 | 0 | 200 |
  | disable, enable at once | 0 of 200 | 0 | 200 |
  | enable filtering nothing | 200 of 200 (WSAEINVAL, 10022) | 200 | 200 |

  - Enabling costs 0.8 to 1.5 µs, enabling a registration removed or disabled 8.1 to
    12.2 µs at p50 (to 52.2 µs at p99), removing or disabling 0.5 to 0.6 µs.
  - One-shot, 200 of 200 each: told of its input, then nothing while disarmed though the
    byte is unread; told again once armed again; told of room to write once armed for
    it; and an armed registration overwritten by OP_ENABLE of another filter is told of
    what that filter asks.
- **The network process on the one-shot poller** (87058f85, net costs run 38314187451,
  windows-2025, 3 rounds each): every case passes, which none had past 1,000 connections
  of 256 KiB each way before.

  | connections held, bytes each way | case | round trip p50 | network process, working set / private |
  |---|---|---|---|
  | 0 | 4.0-4.1 s | 188-338 µs | 5.9-6.4 / 1.1-1.2 MiB |
  | 1,000, none | 16.8-17.6 s | 292-307 µs | 7.5 / 2.1-2.2 MiB |
  | 10,000, none | 109.6-120.3 s | 301-346 µs | 16.2-16.5 / 12.5-12.7 MiB |
  | 10,000, 256 KiB | 120.5-136.7 s | 309-357 µs | 16.2 / 12.5 MiB |
  | 10,000, 1 MiB | 134.6-138.2 s | 192-305 µs | 15.4-15.5 / 12.5-12.7 MiB |

  Event + WSAPoll took 795-845 s a case of 10,000 idle connections, its round trip p50
  65-71 ms, and timed out at 10,000 of 256 KiB (run 38297680089).
- **Consequence (D142).** A filter of nothing is refused, which 527b4519 used for a
  socket waiting for nothing: every connection that did was reset (net costs run
  38308200096). Removing and enabling again at once was not refused in this probe, one
  socket on an idle port, yet f5ac0f01, which paused sockets so, reset the sixth
  connection of 256 KiB each way; one-shot registrations neither remove nor filter
  nothing, and every case passes. The poller keeps each socket's registration one-shot: a
  notice disarms it, and the next wait arms it again for what the socket then waits for,
  in the call that waits; a socket that waits for nothing is left disarmed.

### M216. MoltenVK's host-visible memory, as a GPU's blobs map it (D144)

- **Question.** A blob of host-visible memory is the guest's to map: the device maps its
  host pages, in host pages of 16 KiB (hv_vm_map), where the guest chose in its window.
  Is a mapping 16 KiB aligned, and are its pages past its size another allocation's? When
  the memory is freed while the window still maps it, can its pages be given to another?
- **Method.** `docs/research/measurements/moltenvk-memory/run.sh` (C over MoltenVK
  1.4.2's static library). `layout.c` allocates 64 host-visible VkDeviceMemory objects of
  each size from 1 B to 1 MiB, maps each whole as virglrenderer's Venus maps a blob's
  (vkMapMemory), asks the kernel each mapping's region (mach_vm_region), frees them all
  and allocates again. `reuse.c` frees one while a second mapping of its pages lives
  (mach_vm_remap, shared, as stage 2 holds a mapped blob's, M193), allocates another,
  writes it, and reads the held mapping: 32 times each of 16 KiB to 64 MiB. Apple M5 Max,
  macOS 26.4.1, 2026-10-12.
- **Results.**
  - Vulkan's device has one heap, 137,438,953,472 bytes (128 GiB, the Mac's memory), and
    three memory types on it: device-local (0); device-local, host-visible, coherent and
    cached (1); lazily allocated (2).
  - Every host-visible mapping is 16 KiB aligned, at the start of a VM region of its own
    whose length is its size rounded up to 16 KiB: no other allocation is in its pages.
  - Freed, the next allocations take the freed addresses: 64 of 64 at every size.
  - Pages of a freed allocation that are still mapped elsewhere stay its own: the new
    allocation at its address has new pages, whose writes the held mapping never sees; it
    keeps the old bytes, 32 of 32 at every size.
- **Consequence (D144).** A blob maps into the window whole, in host pages, its pages past
  its size its own region's. The renderer frees a blob's memory when the guest frees it
  and keeps only its address, which the next allocation takes: the device holds each
  blob's pages from its making, remapped, and maps those. A blob said to be larger than its
  memory is refused, as its region ends first. The window is as long as Vulkan's
  host-visible heaps.

### M217. libnvdimm's pmem nodes as init mounts its root (D147)

- **Question.** Runs of a 2.1 GB image (E6's) failed every time: "mounting erofs on
  /lower: No such file or directory". A 108 MB image of the same build ran, and the 2.1 GB
  image's root mounted where the guest booted with its console on, whose printing slows
  the boot. Does init mount the root before libnvdimm has made its device?
- **Method.** Linux's source: drivers/nvdimm/bus.c registers a pmem device's region and
  namespace in `nd_async_domain`, an `ASYNC_DOMAIN_EXCLUSIVE`, and the wait before init
  (init/main.c, `async_synchronize_full`) waits for registered domains alone
  (include/linux/async.h); a bus's `wait_probe` returns once its probing is done
  (drivers/nvdimm/core.c). `docs/research/measurements/pmem-probe/run.sh`: cold boots of
  `run --kernel --rootfs`, on the pinned kernel (6.18.48, `4b8900bdffdd`), interleaved
  between the 108 MB root and the 2,109,083,648-byte one, each boot's time from PID 1's
  start to its connecting to the host (markers 1 to 3). The regression test
  `an_image_of_gigabytes_boots` (containers.rs): 2 GiB of xorshift64's bytes copied into
  the test image. Apple M5 Max, macOS 26.4.1, 2026-10-12.
- **Results.**
  - Before: the 2.1 GB root failed 8 of 8 boots, with either VM process and kernel; the
    108 MB root booted every time. A 2 GiB file of repeated 1 MiB patterns did not fail:
    its root's region is not of that size. Random bytes, without the wait, fail 4 of 4.
  - A first fix waited only where the node was missing: 1 of 200 boots then failed with
    "No such device or address" (ENXIO), its node made but not yet its disk.
  - Mounting, and on ENOENT or ENXIO waiting for each nd bus's probing and mounting again:
    200 of 200 boots. PID 1 to connected, n=100 each:

    | Root | p50 | p90 | p99 | max |
    |---|---|---|---|---|
    | 108 MB | 4.79 ms | 4.93 ms | 5.31 ms | 5.31 ms |
    | 2.1 GB | 8.93 ms | 10.91 ms | 13.19 ms | 13.19 ms |

- **Consequence (D147).** init mounts a pmem EROFS once, and only after ENOENT or ENXIO
  waits for libnvdimm and mounts again: no deadline, and nothing more where the first
  mount succeeds. A large root reaches the host later by its region's probing (4.1 ms at
  p50 for 2.1 GB here); what makes that probing cheaper is unmeasured.

### M218. E6: Venus in a guest against MoltenVK on the host (D144)

- **Question.** docs/research/gpu-accelerators.md's E6: in a guest given this Mac's GPU,
  how do Vulkan matrix multiplication and llama.cpp's tokens/s compare with the same code
  on the host through MoltenVK; what do host-visible memory's steps cost; and what does
  the renderer cost a VM in memory?
- **Method.** `docs/research/measurements/venus/`: `run.sh` builds the guest's image
  (Dockerfile: llama.cpp v0.6.0's Vulkan backend, TinyLlama 1.1B Q4_0, `blobs.c`) and the
  same on the host against MoltenVK 1.4.2, then, in 3 rounds, runs each benchmark on each
  side in turn: `blobs` (200 allocations of each size), test-backend-ops `perf -o
  MUL_MAT` (218 cases), llama-bench (5 repetitions of pp512 and tg128 a round); the
  guest's runs `--gpus all -m 8g` with release `shards` and the pinned gpu kernel.
  `compare.py` pools them. `cold-memory.sh`: 10 cold boots each of one kernel, init and
  root with `--gpu` and without, interleaved, the VM process's memory as each ends;
  `run-memory.sh`: the same of 10 `shards run`s each, as a user's runs have it. Apple
  M5 Max, macOS 26.4.1, 87a17403 with the gpu kernel pinned, 2026-10-12; the host busy
  with other projects' work throughout, its load average 7.9 to 38.9 (`load.txt`).
- **Results.**
  - Matrix multiplication: guest time over host time, per case, geometric mean 1.003;
    min 0.85, p10 0.95, p50 1.00, p90 1.07, max 1.24. What runs on the GPU runs at its
    speed through Venus.
  - llama-bench, tokens/s, n=15 each:

    | Test | Side | p50 | p90 | min | max |
    |---|---|---|---|---|---|
    | pp512 | guest | 5,567.1 | 5,636.0 | 4,092.1 | 5,670.4 |
    | pp512 | host | 4,177.4 | 5,142.6 | 2,873.6 | 5,305.3 |
    | tg128 | guest | 344.4 | 354.9 | 321.9 | 359.6 |
    | tg128 | host | 289.4 | 307.1 | 126.5 | 315.3 |

    The guest is ahead on a busy host, for two reasons found, not Venus's speed: its
    vCPU threads run at the Mach time-constraint policy (`ps -M`: priority 97R, M8, M10),
    which the host's other work does not preempt, where the host's llama-bench runs at
    31T, time-sharing, as any process does; and Venus submits asynchronously, its
    renderer's thread calling MoltenVK, where MoltenVK's own submits are synchronous by
    default: the host with `MVK_CONFIG_SYNCHRONOUS_QUEUE_SUBMITS=0` made tg128 p50 309.5
    from 280.6 (n=15 each, interleaved). A spawn cannot raise a process's QoS class
    (pthread/spawn.h), and a library raising it from within (`qos.c`, loaded by
    DYLD_INSERT_LIBRARIES) left llama-bench's threads at 31T. A quiet host's comparison
    is unmeasured.
  - Host-visible memory, microseconds, the median of the rounds' p50 (p99):

    | Step | 4 KiB guest | host | 64 MiB guest | host |
    |---|---|---|---|---|
    | allocate | 0.3 (0.9) | 4.5 (33.2) | 0.3 (0.9) | 8.0 (51.2) |
    | first map | 126.8 (680.3) | 0.0 (0.1) | 160.5 (1,331.2) | 0.0 (0.1) |
    | first writes | 5.6 (346.0) | 1.1 (5.5) | 13.1 (862.2) | 5.7 (43.8) |
    | unmap | 0.1 (0.4) | 0.0 (0.0) | 0.2 (0.5) | 0.0 (0.2) |
    | free | 6.1 (43.3) | 3.5 (116.9) | 96.2 (121.3) | 26.0 (124.5) |

    A guest's allocation is a command on Venus's ring, which it does not wait for; its
    first map waits for the blob's making and its mapping into the window
    (hv_vm_map); its first writes fault through stage 2. A sequential run of the same
    harness, the host's load lower, had the first map at 68 to 96 µs p50.
  - The renderer's memory, cold boots, n=10 each, MiB p50: `true` 26.3 footprint, 109.9
    RSS with the GPU, 24.0 and 100.9 without; `vulkaninfo --summary` 27.8 and 188.5
    with, 24.2 and 175.1 without (its device the guest's lavapipe). The renderer costs a
    VM 2.3 to 3.6 MiB of footprint. As a user's runs have it (`run-memory.sh`, n=10
    each, p50): a GPU run, booted cold where a run without one restores its template,
    holds 124.1 MiB of footprint (135.4 RSS), the run without 14.8 (29.5), both with 18
    vCPUs; with `vulkaninfo`, 149.4 (219.2) and 45.5 (117.3).
- **Consequence (D144).** Venus gives a guest the GPU's speed for its GPU's work, and
  costs a VM some 3 MiB. A blob's first map is the cost to keep down: about 0.1 ms each,
  past 1 ms at p99 on a busy host. A GPU run's memory is its cold boot's (D150's
  templates). Guest vCPUs at the time-constraint policy outrun the host's other work.

### M219. What plugging a run's memory costs a run (D148)

- **Question.** D148 gives every run's microVM a region to plug of the host's memory (here
  128 GiB), one more device, init's grower and a template whose kernel limits are sized
  for all of it. What does that cost a run's start and end, and an idle run's memory?
- **Method.** `docs/research/measurements/run-plug/region.py`: cold boots of one root,
  `--memory 256` and a region of 0 to 128 GiB, the sizes in turn, n=12 each.
  `build-ab/ab.py`: 300 runs of `run --rm alpine:3.22 true` in each of two builds, in
  turn (93ce8368, and it with D148), each restoring its own template.
  `run-plug/memory.py`: 20 such runs each, their VM process's memory as they end. Apple
  M5 Max, macOS 26.4.1, 2026-10-12, the host's load average 7.8 to 11.7.
- **Results.**
  - A region costs a cold boot and its end next to nothing, by its size, µs p50 (p90):
    none 38,576 (39,053); 2 GiB 38,516 (40,012); 16 GiB 39,027 (40,482); 64 GiB 39,279
    (40,087); 128 GiB 39,374 (40,728).
  - The first A/B found +7.75 ms a run, paired median, 95% [+7.63, +7.87], outside the
    command: `SHARDS_TIMING`'s count of the pages held of memory to plug asked the OS of
    the whole region, page by page (16 KiB pages, 8 Mi of them). It now asks of blocks
    ever plugged alone (`Hotplug::ever`), still those given back, whose pages a failed
    give-back would leave held, which `a_guest_grows_and_gives_back_its_memory` catches.
  - With that, a run's wall clock, new less old, paired: median +36 µs, 95% [-14, +100];
    the command's time in the guest -26 µs [-38, -9]; the rest +70 µs [+23, +121]. Wall,
    µs: old p50 6,131, p90 7,012, p99 16,420, max 25,772; new 6,142, 6,938, 10,104,
    10,675.
  - An idle run's memory, MiB, p50 (p90): footprint 15.06 (15.20) old, 15.27 (15.38)
    new; RSS 29.30 (29.34) old, 29.44 (29.53) new.
- **Consequence (D148).** Every run may plug the host's memory for what its start costs
  already: within 0.1 ms and 0.2 MiB of a microVM fixed at boot.

### M220. What a guest reports of its memory sizes a runtime's heap (D148)

- **Question.** Runtimes size themselves by the memory they see: what does Node.js see,
  and size its heap by, in a shards run and in a Docker container, with `-m` and without?
- **Method.** `node:22-alpine`, `node -e` printing V8's `heap_size_limit`,
  `os.totalmem()` and `process.constrainedMemory()`: in Docker 29.3.1 (shards-dind, its
  VM's 62.7 GiB), and in shards at 7a132d35 (a run booted small, plugging up to its limit)
  and with `-m` booting its limit again, on an Apple M5 Max of 128 GiB, macOS 26.4.1,
  2026-10-12. Each value is the runtime's own computation from what it read, the same
  every run.
- **Results.** MiB:

  | Run | heap_size_limit | totalmem | constrained |
  |---|---|---|---|
  | Docker, no `-m` | 4,144 | 64,186 | none |
  | Docker, `-m 8g` | 2,096 | 64,186 | 8,192 |
  | shards, no `-m` | 259 | 243 | none |
  | shards, `-m 8g`, booted small | 259 | 243 | 8,192 |
  | shards, `-m 8g`, booted with its limit | 2,096 | 8,362 | 8,192 |

- **Consequence (D148).** Node sizes its heap by the smaller of the memory the kernel
  reports and its cgroup's limit. A microVM booted small reports only what it has, so a
  run with `-m` boots with what its limit needs, and sizes as Docker's does. A run without
  one grows as it needs, but Node sizes its heap by the 243 MiB it boots with, where
  Docker's sees its host's: what a guest reports must be what it may plug.

### M221. A guest that reports what it may plug sizes a runtime's heap as Docker's (D149)

- **Question.** With D149's report, what does Node.js see of a run's memory, and size its
  heap by, against Docker's?
- **Method.** `docs/research/measurements/run-plug/heap.sh "" "-m 8g" "-m 1g"`, as M220:
  `node:22-alpine` (Node v22.23.3, libuv 1.51.0) printing V8's `heap_size_limit`,
  `os.totalmem()` and `process.constrainedMemory()`, in shards at D149 (on dd843b2c) with
  the 6.18.48 kernel patch 0004 makes (`SHARDS_KERNEL`), and in Docker 29.3.1 (shards-dind,
  its VM's 62.7 GiB), on an Apple M5 Max of 128 GiB, macOS 26.4.1, 2026-10-12. Each value
  is the runtime's own computation from what it read, the same every run. Inside the run,
  without `-m`: `vm.pluggable_kbytes` 133,931,008, `MemTotal` 134,179,892 kB (the host's
  134,217,728 less the 37 MiB the guest kernel keeps of what it boots with), `free -m`'s
  total 131,035.
- **Results.** MiB; "none" where libuv reads cgroup2's `max` (UINT64_MAX, in both):

  | Run | heap_size_limit | totalmem | constrained |
  |---|---|---|---|
  | shards, no `-m` | 4,144 | 131,035 | none |
  | Docker, no `-m` | 4,144 | 64,186 | none |
  | shards, `-m 8g` | 2,096 | 8,362 | 8,192 |
  | Docker, `-m 8g` | 2,096 | 64,186 | 8,192 |
  | shards, `-m 1g` | 524 | 1,085 | 1,024 |
  | Docker, `-m 1g` | 524 | 64,186 | 1,024 |

- **Builds and one large allocation.** A `RUN` step printing Node's `heap_size_limit` and
  `os.totalmem()`, then `dd bs=1G count=1` (scratchpad Dockerfile, `FROM node:22-alpine`):
  shards' builder (its most 64 GiB, half the host's) before D149, 259 MiB and 472 MiB, and
  dd "out of memory"; after, 4,144 MiB and 65,496 MiB, and dd's gigabyte copied; `docker
  build` (shards-dind), 4,144 MiB and 64,186 MiB, and copied. In a run without `-m`,
  `dd bs=1G count=2` (debian:bookworm-slim): before, "memory exhausted by input buffer of
  size 1073741824 bytes"; after, copied, `CommitLimit` 67,089,944 kB (half the memory the
  run may reach, as a host of it would have), as Docker's copies it (its VM's
  `overcommit_memory` is 1).
- **Consequence (D149).** Node sizes its heap as Docker's in each case: without `-m` by
  the memory the run may have (its host's), where M220's sized it by the 243 MiB it booted
  with (259); in a build step, by its builder's most. A run with `-m` reports its own
  memory, where Docker's reports its host's: the smaller of the two that Node takes is the
  limit either way. One allocation larger than what is plugged so far is made, as on a
  host of the memory the guest may reach.

### M222. What giving a run's freed memory back costs its runs (D151)

- **Question.** A virtio-balloon device that gives back the memory a guest reports free:
  what does it cost a run, and where does a cost come from?
- **Method.** `docs/research/measurements/build-ab/ab.py OLD NEW alpine:3.20 N true`,
  `AB_FLAGS=--rm`: each arm's `shards` and a home with alpine pulled, the guest recorded,
  and its template saved; N runs of each arm in turn, paired. OLD: ea38f209, no device.
  The other arms are builds of D151's work in turn, and `run-plug/age.py` reads each run's
  VM age as its request came. Apple M5 Max, 128 GiB, macOS 26.4.1, 2026-10-13, load 4 to
  30 (each A/B's own in the scratch records); the pinned kernel 4b8900bdffdd, and for the
  last, both arms on 6.18.48 with patches 0004 and 0005, built here.
- **Results.** Paired median differences, NEW less OLD, with 95% intervals, µs:

  | NEW | n | wall | command |
  |---|---|---|---|
  | the device, every reported range given back | 300 | +306 [+49, +568] | +21 [+12, +41] |
  | the device, no reporting offered | 300 | −61 [−205, +98] | +12 [−13, +43] |
  | reporting offered, against the device without it | 300 | −15 [−94, +108] | +32 [+4, +57] |
  | every range given back, again | 300 | +154 [−389, +1011] | +60 [+33, +91] |
  | ranges the host holds none of left alone | 400 | +83 [−98, +337] | +79 [+70, +92] |
  | nothing given back in a VM's first 5 s | 400 | +209 [−112, +397] | +50 [+43, +60] |
  | ten execs a run, the same | 200 | +186 [−17, +349] | +81 [+46, +117] |
  | templates saved 2.5 s into their boot | 400 | +24 [−65, +80] | +29 [+18, +45] |
  | templates saved after `page_reporting.flush` (D151) | 400 | −5 [−94, +98] | −11 [−22, +0] |

  Every run of `age.py` took a VM under 0.5 s old (the pool refills as a run ends), so no
  report came during any of them: each restore's reports came two seconds into its life
  (208 MiB, in two reports of 2 to 4 ms). The cost followed the template's state, not
  the reports: a template saved some 150 ms into its boot holds the first pass that
  registering the device schedules two seconds on; saved after one pass, it held the
  next, as a pass reports some 64 blocks of each free list; saved after the flush, it
  holds none. A restore of a flushed template reports nothing in its first 3 s, one of a
  template saved without it 212,992 KiB (`a_runs_memory_freed_goes_back_to_the_host`).
- **Giving back.** A run that touched 512 MiB and freed it, idle 10 s: footprint at its
  end 57 and 132 MiB, an idle run's 94, the peak 578 and 645; without the device, 562
  against 90.
- **Consequence (D151).** Templates are saved after a flush; a run's memory freed goes
  back to the host at no cost measured to its runs.


### M223. A GPU's runs from templates of their own, and a run's devices found by name (D150, D152)

- **Question.** What does restoring a template of its own (D150) save a run given the GPU,
  what does the GPU still cost a restored run, and where does that cost come from?
- **Method.** `docs/research/measurements/build-ab/ab.py`, `AB_FLAGS=--rm`, `true` in
  `gpu-venus:1` (Debian trixie-slim with Mesa's Venus and vulkan-tools, tests/gpu.rs's
  image), each arm a home of its own with a copy of one build of D150's work, the image
  loaded (`shards load`), its guest recorded (`shards guest use`), pairs n=200; the
  daemon of a home that boots every run was started with SHARDS_KERNEL, the pinned gpu
  kernel, and SHARDS_INIT. Inside restored microVMs, `device-setup` (its directory): the
  steps of finding and confining a device, as root of a privileged run, each round timed
  (n=300, one VM) and the first round of fresh VMs (n=30); and shards-init with
  `timed.patch` (over `preload.patch`), whose runs read what their devices cost it
  (n=30 each). Memory: `venus/run-memory.sh 10 gpu-venus:1`, in a home that restores
  templates and in one that boots every run. Apple M5 Max, 128 GiB, macOS 26.4.1,
  2026-10-13; kernel 6.18.48, b3f3b948641f; each A/B's 1-minute load below.
- **A GPU's runs.** Paired median differences, NEW less OLD, with 95% intervals, µs:

  | OLD → NEW (`--gpus all` unless said) | load | wall p50, OLD → NEW | wall | command |
  |---|---|---|---|---|
  | booted every run → restored | 7.0 | 98,803 → 7,591 | −90,577 [−91,588, −89,534] | +301 [+256, +337] |
  | restored: no GPU → the GPU | 7.6 | 6,445 → 7,410 | +920 [+792, +994] | +889 [+829, +930] |
  | restored: no device → `--device /dev/fuse` | 32.7 | 8,741 → 10,368 | +1,614 [+1,272, +2,017] | +1,234 [+1,167, +1,311] |

  Booted, p90 112,030, p99 129,751, max 136,082; restored, 13,004, 31,714 and 35,150.
  The GPU's cost in a restored run is in its command, the restore itself the same
  (outside the command, +8 [−97, +106]); and a device costs as much without the GPU.
- **Where a device's cost was.** `device-setup`, µs, p50 (p90, max):

  | step | each round, one VM | first round, fresh VMs |
  |---|---|---|
  | walk of /sys/dev's 104 uevents (init's lookup) | 229.3 (251.5, 652.3) | 581.8 (633.2, 657.3) |
  | fstatat(2) of the node in a devtmpfs held open | 0.3 (0.5, 11.2) | 1.5 (1.9, 3.3) |
  | the filter compiled | 3.2 (4.2, 10.6) | 6.0 (6.8, 7.5) |
  | loaded (BPF_PROG_LOAD) | 84.2 (112.0, 508.5) | 69.2 (89.4, 122.9) |
  | attached in the default's place | 1.2 (1.6, 8.1) | 2.4 (3.6, 3.7) |

- **Found by name (D152).** D150's init against one that finds a run's devices in its
  devtmpfs, `--device /dev/fuse` in both, load 9.5: wall −954 [−1,124, −780], command
  −774 [−796, −761] (p50 1,913 → 1,137). With it, against a run with no device, load
  6.8: wall +94 [+10, +160], command +106 [+90, +115]; the GPU, load 5.2: wall +235
  [+107, +359], command +194 [+180, +209]. With the gpu kernel recorded, so that both
  arms boot it, the GPU against `--device /dev/fuse`, load 12.2: command +52 [+21, +83].
- **A GPU's filter loaded before its template is saved** (`preload.patch`) against
  none, load 12.4: wall −2 [−105, +90], command −1 [−19, +15]. In place (`timed.patch`,
  restored runs, µs, p50): the lookup and the nodes made 17.0, the GPU's filter compiled
  7.4, a load of it 100.3, the loaded one attached 9.2; a fuse device's load 113.5. The
  load is off a GPU run's path: its command waits on something else as long.
- **Memory**, MiB p50 (n=10 each), footprint and RSS as each run ends: restored, the
  GPU's `true` 18.4 and 39.4, a run without the GPU 16.5 and 31.5; `vulkaninfo
  --summary` 51.4 and 132.3, and 46.5 and 118.7 (the guest's lavapipe). Booted, the gpu
  kernel in both: 94.8 and 106.3, 92.2 and 97.5; `vulkaninfo` 120.6 and 190.5, 115.7 and
  177.1.
- **Consequence.** A run given the GPU restores a template of its own (D150): 91 ms
  sooner at the median, and 76 MiB less footprint. A run's devices are found by name in
  the VM's devtmpfs (D152): a device costs a run 0.1 ms where it cost 1.2. A GPU's filter
  is loaded as its run comes, which costs the run nothing measured.

### M224. What a pass of free page reporting holds, and whom it starved (D151)

- **Question.** Two gates' runs ended where they should not have, each as its guest
  reported free memory: a restored run of `alloc 1024`, growing (D148), was ended (137)
  with 106 MiB reported; a run's agents (`agents_out_of_memory_end_whole_and_spare_the_run`,
  `-m 128m`) were given a share of none, and their run was ended. What does a pass of
  reporting do to a guest's memory, and what keeps it from starving anyone?
- **Method.** Apple M5 Max, macOS 26.4.1, 2026-10-13, load 12 to 25 (other projects'
  builds). Guest kernels: the pinned one, b3f3b948641f (patches 0001-0004), and two built
  here from 6.18.48 by scripts/build-kernel.sh in a builder microVM: A, with 0005 and with
  0006 counting what a pass holds as free; B, 0006 whole (resources/kernel/patches), its
  allocations also waiting for what a pass holds.
  - Growth: `run-plug/report-race.py`, restored runs of `alloc 1024` whose request came as
    the pinned kernel's template ran the pass its boot scheduled (about 1.85 s into a
    restore's life), pauses seeded from 1.0 to 2.6 s (n=60), then aimed at the pass (1.6
    to 1.9 s, n=60, two blocks of 30 per arm in turn).
  - Readings: `reporting-dip`, in a guest: 512 MiB touched and freed, then `MemAvailable`,
    `MemFree` and `page_reporting.held` read every millisecond for 5 s, 3 runs per kernel
    in turn; its pass comes two seconds after the free.
  - Allocations: the agents test itself, its run of fixed size booted cold (its first pass
    two seconds into its life), kernels A and B in turn, and the pinned kernel.
- **Results.**
  - Growth, the pinned kernel: D150's init, 2 of 60 ended, both asked at 2,050 ms; aimed,
    4 of 60, each asked at 2,000 to 2,049 ms, where 4 of 8 were. With room for a whole pass
    beside `memory.high` (128 MiB), none of 60, and none of 60 aimed.
  - Readings: the pinned kernel, reporting, `MemAvailable` ranged 132,044 KiB (129 MiB) as
    the workload idled, its passes reporting 686 to 836 MiB; kernel A, a pass held 131,072
    KiB at once, 32 blocks of 4 MiB, and `MemAvailable` ranged 7,684 to 12,704 KiB, as it
    did without reporting, the pinned kernel with the device let go: 7,624 to 10,232 KiB.
  - Allocations: kernel A, 6 of 8 failed, the run's command ended (137) as the first pass
    held the run's free memory; kernel B, none of 24 so, and once otherwise: the run spared
    and y ended, z was ended after it, its memory read as 65,679,360 bytes where it holds
    16 MiB, not yet explained; the pinned kernel, the device let go, 0 of 8.
- **Why.** A pass takes up to 32 free blocks of order 9 or more off the free lists, 4 MiB
  at most each, and holds them until the device has reported them (mm/page_reporting.c
  `page_reporting_cycle`); it starts only where the zone has its low watermark and 64 MiB
  free (`page_reporting_process_zone`), and then takes what it finds. For that time the
  zones count the pages gone: sysinfo(2) and /proc/meminfo showed that much less, and an
  allocation needing them found nothing to reclaim, and the OOM killer ended a task in a
  guest whose memory was free.
- **Consequence (D151, patch 0006).** What a pass holds is free memory: counted so in
  sysinfo(2) and /proc/meminfo, and waited for by an allocation that would otherwise fail.
  A guest kernel without it reports nothing: shards-init lets its device go as it boots.
  Room for a pass beside growth, which ended the kills in growing runs on the pinned
  kernel, is not kept: with 0006 no allocation fails for a pass, and without it no pass
  comes.

### M225. Huge pages advised or not, where the host's THP is always on (M157)

- **Question.** shards advises huge pages for guest RAM (MADV_HUGEPAGE,
  platform/unix.rs). Under `defrag=madvise` an advised region's fault may stall in direct
  compaction, which an unadvised one never does. Is that the tail `to_ready` showed beside
  Firecracker?
- **Method.** CI's x86_64 KVM runner, AMD EPYC 7763, Linux 6.17.0-1022-azure, revision
  07385b7, 2026-10-13, load 1.1 to 2.7; THP `enabled=[always]`, `defrag=[madvise]`, 2 MiB
  pages. `cargo bench -p shards --bench firecracker -- --thp-ab all,unadvised --runs 30`:
  the comparison's idle guest (1 vCPU, 128 MiB) booted to ready, each boot followed by a
  1 GiB guest writing 512 MiB and reading it at random (`walk`), the arms interleaved and
  their order alternating; `unadvised` is the VM process with SHARDS_THP_ADVICE=never. The
  same job's comparison with Firecracker v1.17.0, n=30, beside it.
- **Results** (p50 / p90 / p99 / max).
  - Boot `to_ready`: advised 129.4 / 195.9 / 202.2 / 202.2 ms, unadvised 128.6 / 199.0 /
    202.4 / 202.4 ms; huge anonymous memory 56.0 MiB at the median in both, 58.0 at most.
  - Work: writing 327.7 ms against 328.2 ms at the median, reading 202.7 against 202.6 ms.
  - The comparison: shards 123.0 / 129.7 / 130.2 / 130.2 ms, Firecracker 124.0 / 128.9 /
    206.0 / 206.0 ms, tied at the median (−483 µs, interval −1,948 to +1,516); in the
    guest 77.9 against 80.3 ms at the median, outside it 44.6 against 44.4 ms, Firecracker's
    outside 117.7 ms at p99.
- **Why.** With THP always on, every anonymous region is given huge pages, advised or not,
  and both arms were: the advice changed nothing here, its tail included. The tail both
  arms of the A/B show, 67 to 70 ms past their median at p90, the comparison's boots do
  not, which follow no 1 GiB guest: it is not the advice's, and its cause is not yet
  known.
- **Consequence.** The advice stays: on a host whose THP is `madvise`, it is what gives
  guest RAM huge pages at all (M157). SHARDS_THP_ADVICE=never stays the A/B's knob.

### M226. Where a cold boot's time goes beside Firecracker's, and the kernel's load overlapped

- **Question.** The comparison's `to_ready` ties with Firecracker's (M225) and fails the
  envelope where a run's interval lands above zero (2 of 4 runs, 2026-10-13). Where does
  each VMM's boot spend its time, and what of it is shards' own to cut?
- **Method.** `docs/research/measurements/boot-exits/measure.py` on CI's x86_64 KVM
  runners (AMD EPYC 9V74, Linux 6.17.0-1022-azure, revision 3df7f5c, two runners): the
  comparison's idle guest, 10 boots each VMM interleaved under `perf kvm stat record`,
  VM exits by reason; each VMM's guest booted once with `initcall_debug`, `quiet`, its
  kernel log read from /dev/kmsg once booted; shards once at `SHARDS_LOG=debug`.
  `docs/research/measurements/kernel-load/ab.py`: two builds of the VM process
  interleaved, n=30, time to the vCPU's first entry, on an M5 Max, macOS 26.4.1, load 10
  to 13.
- **Results.**
  - VM exits per boot, p50 (shards / Firecracker): nested page faults 341 / 269 (3.6 /
    2.9 ms handled), MSR 143 / 200 (4.8 / 4.8 ms), CPUID 221 / 477 (0.45 / 0.96 ms),
    port I/O 245 / 244, interrupts 152 / 151; 13.0 against 13.9 ms of exits handled in
    all, of a boot of about 120 ms.
  - shards' VM process, at debug: confined at 0.2 ms, the kernel loaded at 3.4 ms, the
    VM made and RAM mapped at 3.7 ms, its vCPU ready at 4.9 ms: the kernel's copy, 3.1 ms,
    is most of its setup, and runs before the VM's assembly, which needs none of it.
  - Initcalls, quiet: 67.8 ms of shards' guest's against 64.6 of Firecracker's, the
    difference within initcalls timed before a fine clocksource (4 ms steps); the crypto
    self-tests 12.4 against 25.2 ms, deferred probing 7.9 against 0.01 ms (shards' guest's
    worker, on its one vCPU, behind the tests' threads).
  - The kernel's load on a thread of its own while the VM is made, macOS: time to entry
    2,614 / 3,089 / 3,446 / 3,446 µs before, 2,586 / 3,021 / 3,114 / 3,114 µs after (p50 /
    p90 / p99 / max), −32 µs at the median paired: Hypervisor.framework's assembly is
    short beside the copy.
- **Why.** shards' guest takes 72 more nested page faults at the median: its first 2 MiB
  is on small pages (M157), where the kernel's scans of low memory (the MP floating
  pointer's areas, 0xF0000 to 0xFFFFF among them, as shards gives no MP table; the
  option ROMs from 0xC0000) each fault a page that Firecracker's huge page holds whole.
- **Consequence.** The kernel's load overlaps the VM's assembly on every backend; what it
  buys on x86_64 KVM is read from the comparison's next runs. The boot pages are next:
  mapped ahead of the guest (KVM_PRE_FAULT_MEMORY, a read fault: a page the guest only
  scans maps to the zero page, no memory resident).

### M227. A cold boot's copy, shared by two threads and read straight into guest memory

- **Question.** From the VM process's exec to its vCPU's first entry, shards took 1.0 and
  1.8 ms longer than Firecracker on CI's x86_64 KVM runners (boot-exits pass 4, revision
  ce8f85d, run 38483758187, 10 boots each, interleaved: p50 6,423 against 5,425 µs on one
  runner, 7,711 against 5,982 on the other). Where does that time go, and what of it can
  go?
- **Method.** `perf trace` of one boot each (the same run): every system call in order,
  its thread and duration. Then `docs/research/measurements/kernel-load/ab.py` on an M5
  Max, macOS 26.4.1, load 6.5 to 8.4: the VM process of revision ce8f85d (A) against one
  whose kernel and initrd are planned first and copied by two threads (B), both `release`
  and signed as `crates/shards/build.rs` signs them, n=30 each, interleaved, twice.
- **Results.**
  - shards, traced: its main thread made the machine (the VM, its devices, their tables)
    in 0.83 ms, then waited 4.74 ms for the loader thread, which read the kernel's two
    segments into guest memory (17.7 MB in 2.73 ms, 3.5 MB in 0.53 ms, 6.5 GB/s), then
    the initrd into a buffer of its own (2.59 MB in 1.14 ms, 2.3 GB/s), copied it into
    guest memory (0.25 ms from the read's end to the buffer's unmapping) and unmapped the
    buffer (0.15 ms).
  - Firecracker, traced: all on one thread; the kernel's segments read at 6.6 to 6.9
    GB/s, the initrd read straight into guest memory in 0.29 ms.
  - The A/B, entry µs p50 / p90 / p99 / max: A 2,405 / 2,540 / 2,572 / 2,572, B 1,740 /
    1,901 / 2,021 / 2,021, B less A −652 µs at the median paired; again, A 2,583 / 2,968
    / 3,191 / 3,191, B 2,080 / 2,497 / 2,650 / 2,650, −626 µs.
  - x86_64 KVM, the Firecracker comparison on CI (revision f9da33c, AMD EPYC 9V74, Linux
    6.17.0-1022-azure, n=30, interleaved): `to_ready` 98,394 µs against Firecracker's
    100,435 µs at the median, −1,627 µs paired, interval [−2,644, −1,075]: better, where
    it had tied on this runner type (M226's runs: −564 µs [−1,100, +243] and +622 µs
    [−62, +1,897]); outside the guest's clock −2,682 µs at the median paired.
- **Why.** The copy is the boot's setup's longest part, and the machine's making, which
  needs none of it, is the shorter: one thread copying while the other waits leaves the
  copy whole on the path to the first entry. An initrd read through a buffer pays its
  pages' faults and a second copy.
- **Consequence.** `vm::beside_copy`: the kernel's and the initrd's places are planned
  from the kernel's headers and the initrd's length, the initrd's file is read straight
  into guest memory (an init's into its place in the archive there), and the copy is cut
  at each 2 MiB of guest memory, a huge page each, which the loader thread and then the
  main thread, its own work done, take in turn.

### M228. What a run's own IPC namespace and /dev/shm cost a microVM (D156)

- **Question.** D156 gives each run's standby an IPC namespace and a 64 MiB /dev/shm of
  its own, made before the template's snapshot. What do they cost a restore's time, and a
  microVM's memory, counted as memory per VM is counted ([M30]: `phys_footprint`)?
- **Method.** `docs/research/measurements/ipc-namespace/restore.py`, as M170's: two
  templates (`#a`, `#b`) of each of two inits from one tree, D156's (`d156`) and the same
  init without the standby's `CLONE_NEWIPC` and its /dev/shm mount (`without`), so that
  they differ in those alone; `alpine:3.22`'s root and the pinned kernel
  (Image-6.18.48-aarch64-6f2b217a4feb); 150 restores of each,
  `shards restore TEMPLATE -- /bin/true`, interleaved, their order rotating each round. A
  release build of the tree on 1e072493 with D156; Apple M5 Max, macOS 26.4.1,
  2026-10-14, load 6.1 to 9.4.
- **Results.** The median difference against `without#a` [95%]:
  - Memory per VM (`footprint_kib`, KiB): `d156#a` +32 [+16, +32], `d156#b` +16 [+16, +16],
    where the other template without them is +16 [+0, +32]. p50 3,600 and 3,584 with,
    3,568 and 3,584 without.
  - RSS at the timing line (KiB): +256 [+240, +256] and +128 [+128, +144], the other
    template without −16 [−32, +0]: pages of the template, shared by every microVM restored
    from it, which `phys_footprint` leaves out (M170).
  - Restore (`released_us`): +3 [−43, +39] and +15 [−10, +47], the other template without
    −27 [−39, +6]; p50 2,561 and 2,589 µs with, 2,553 and 2,547 without; p99 2,917 and
    3,033 with, 3,028 and 2,998 without. From spawn to reap: +45 [−67, +213] and −10
    [−147, +77]. None resolved.
- **Consequence.** Every run has an IPC namespace and a /dev/shm of its own (D156): no
  restore time resolved, and memory per VM within one 16 KiB step of what two templates
  of one machine differ by.

### M229. What a run's swap costs its guest's kernel, and why a device sized to its allowance alone fails (D159)

- **Question.** D159 gives a run with a memory limit a swap area on a sparse file of its
  own. How large must the device be for the allowance its cgroup holds it to
  (`memory.swap.max`) to be usable, and what does its size cost the guest?
- **Method.** Real microVMs on an M5 Max (macOS 26.4.1, 18 CPUs, 128 GiB), 2026-10-14, the
  test image's workload: `/proc/meminfo` (`SwapTotal`, `VmallocUsed`), `/proc/vmstat`
  (`pswpout`), the workload cgroup's `memory.swap.events` and `memory.events`, and
  `alloc 96` (96 MiB touched a page at a time) under `-m 64m`, its swap allowance 64 MiB.
  Three runs of each size.
- **Results.**
  - A device exactly the allowance (64 MiB): the workload was killed with 30 MiB in swap,
    `memory.swap.events` fail 3,677, its whole device written once (pswpout 16,389). Each
    CPU keeps a cluster partly used for each order of folio it swaps out
    (mm/swapfile.c, percpu_swap_cluster; 10 orders with THP swap, 2 MiB clusters), so 18
    vCPUs may hold up to 360 MiB of clusters no other can take.
  - The allowance and that room (64 MiB + 18 × 10 × 2 MiB = 424 MiB, sparse): `alloc 96`
    done, fail 0, no OOM kill.
  - The guest's bookkeeping, `VmallocUsed`: 3,348 to 3,396 kB without swap; 3,748 to
    3,764 kB with 424 MiB; 4,436 to 4,468 kB with 1,384 MiB; 108,388 kB with 128 GiB (an
    unlimited allowance sized as the host's memory), in a guest of 226 MiB whose workload
    was then killed (`memory.events` oom 72, swapcached 58 MiB). About 0.8 KiB a MiB of
    device: swap_map's byte and swap_cgroup's two a page.
- **Consequence.** The device is the allowance and 20 MiB a vCPU, the cgroup holding the
  workload to the allowance; unlimited swap is as much again as the memory. The kernel's
  bookkeeping is counted into the microVM's memory, beside its limit: 128 KiB and 13 bytes
  a 16 KiB of device, above every point measured here (`resources::swap_kernel_bytes`).

### M230. A run's swap on a virtio-pmem window, against a block device's; what it costs (D159)

- **Question.** D159's first swap was a writable range of the join disk (virtio-blk,
  through a loop device): did it hold a workload within its swap? And the swap window that
  replaced it: what does it cost a run's start, a workload that swaps, a template's boot,
  and the guest's memory accesses, whose address space it widens?
- **Method.** Real microVMs on the M5 Max (macOS 26.4.1, 18 CPUs, 128 GiB), 2026-10-14, a
  release build of the tree on 036989e0 with D159.
  - The block device: `-m 64m`, `alloc 96` (96 MiB touched a page at a time), four
    detached runs, each run's cgroup's `memory.events` and `memory.stat` read after.
  - The window: `docs/research/measurements/swap/swap.py`, an image of the test guest's
    binary loaded into a fresh home, each pair of arms interleaved, their order alternating,
    100 turns, the host's load 9 to 11. `start`: `run --rm -m 64m IMAGE exit 0`, with swap
    and without (`--memory-swap 64m`), both restoring one template. `swapping`: `alloc 96`
    under `-m 64m`, against `-m 128m --memory-swap 128m`, which swaps none. `boot`: a
    microVM booted with a 64 GiB window, which init shows and probes raw, against one
    without. `walk`: a page-strided pointer chase over 512 MiB (`walk 512 4000000`), its
    guest's address space 40 bits with the window and 36 without.
- **Results.**
  - The block device: one run in four killed (137) within its swap, its cgroup's
    `memory.events` oom 45, 44 MiB in the swap cache when it was: pages written, or being
    written, that reclaim could not yet free. The others swapped 33 MiB out, 14,600 pages.
  - The window: none killed, in five runs of the E2E test in a row and three passes of the
    harness, 100 swapping runs each. `swapping`: +12.6 ms [+12.1, +13.1] from request to
    answer, p50 26.7 ms without, 39.6 ms with: 32 MiB past the limit swapped out, and the
    workload's end; +11.7 and +13.7 ms in two earlier passes.
  - `start`, from request to answer: +170 µs [+148, +182], p50 731 µs without and 889 with,
    p99 889 and 1,038. Where it goes (`markers.py`, n=100, the tree instrumented by
    `markers.apply.py`): the guest's header write 23 µs at p50, 31 at p99, 113 at most;
    swapon 25 µs, 44 at p99. Before the host wrote the file's first page, that write, the
    guest's first to the sparse file through the mapping, was 72 µs at p50, 3.4 ms at p90,
    35 ms at p99 and 80 ms at most (n=60), and the start +287 µs [+234, +321] in a pass of
    `swap.py`; with init swapping on from a child of its own, in a mount namespace and a
    `/dev` of its own, +471 µs [+455, +489]. The VM's mapping alone, its guest doing
    nothing with it: −2.5 µs [−15.0, +14.5].
  - Reserving the file's disk instead (`prealloc.c`, n=50): `F_PREALLOCATE` and
    `ftruncate` of 424 MiB 5,332 µs at p50 and 17.6 ms at most, of 4,456 MiB 29 ms at p50
    and 523 ms at most; `ftruncate` alone 8 µs at p50, 4.1 ms at most.
  - `boot`: +0.6 ms [+0.4, +1.0] from spawn to reap, p50 38.5 ms without (+0.85 ms [+0.37,
    +1.19] in a pass before): the window shown and probed, once a template.
  - `walk`: 98.2 ns a step without, 98.4 with, the paired difference +2.3 ns [+0.1, +4.2];
    in a pass before, 123.0 and 124.1, −0.1 ns [−1.3, +2.2].
- **Consequence.** Swap is the window (D159). The host writes a run's swap file's first
  64 KiB before it maps it, and the file stays sparse past that. The address space the
  window adds costs a load whose every step misses translation at most about 2%, on hosts
  whose microVMs would otherwise fit 36 bits; the window is as long as the host's memory,
  Docker's default swap of the largest run.

### M231. A run's rotated log segments compressed: which codec, at what level, in what frames (D162)

- **Question.** Which of the codecs shards already carries keeps a run's rotated log
  segments in the least disk, at a cost a thread of the daemon can bear and with reads as
  fast as `logs` needs: gzip (flate2 on zlib-rs), or zstd (shards-zstd, klauspost's
  encoder ported; ruzstd reads it). Which level? And in frames of what size, each
  compressed alone, so that a reader decompresses one frame to read a record, not the
  whole segment? What does that cost in size?
- **Method.** On the M5 Max (macOS 26.4.1, 18 CPUs, 128 GiB), 2026-10-14, the tree at
  8d7f4795.
  - The logs are real. From Loghub (He et al., "Loghub: A Large Collection of System Log
    Datasets for AI-driven Log Analytics", ISSRE 2023): the 2,000-line sample of each of
    its 16 systems, and the full logs of eight: Apache, HealthApp, HPC, Mac,
    OpenStack, OpenSSH, Zookeeper and a Hadoop container's (Zenodo record 8196385). Also
    three of shards' own CI job logs: cargo's output, escape sequences kept.
  - Each was written through shards' store as a run writes it.
    `docs/research/measurements/log-compress/segments.sh` prints each file a line at a
    time, one write a line (busybox awk with `fflush`), in a microVM (alpine:3.22). Its
    log has `--log-opt max-size=20m`, and the script keeps the first segment, its records
    as the guest's output came: Apache's 56,481 lines in 10,999 records. That makes 27
    segments, 128.9 MB in all, six of them 17 to 20 MB.
  - `log-compress` (same directory) compresses each segment whole with each codec and
    level, the codecs interleaved in each of 25 turns. It records the size, the time to
    compress, and the time to read the result back as a stream in 64 KiB reads. Then it
    cuts each segment into frames of 64 KiB, 256 KiB, 1 MiB and 4 MiB, each compressed
    alone, and records their size, with 8 bytes a frame for a seek table, and the time to
    read one frame back (25 turns, a frame in turn).
- **Results.** Size, all 27 segments, as a share of 128.9 MB:

  | | whole | 64 KiB frames | 256 KiB | 1 MiB | 4 MiB |
  |---|---|---|---|---|---|
  | gzip -1 | 14.20% | 15.12% | 14.43% | 14.26% | 14.21% |
  | gzip -6, Docker's | 9.20% | 10.31% | 9.47% | 9.27% | 9.22% |
  | gzip -9 | 8.91% | 10.06% | 9.20% | 8.98% | 8.93% |
  | zstd -1 | 9.17% | 10.59% | 9.53% | 9.24% | 9.18% |
  | zstd -3 | 9.38% | 10.60% | 9.60% | 9.35% | 9.35% |
  | zstd -7 | 8.38% | 9.91% | 8.80% | 8.46% | 8.38% |
  | zstd -11 | 7.53% | 9.19% | 8.00% | 7.63% | 7.54% |

  Time, over the eight segments of 5 MB and more, n=25 each:
  - **Compress.**
    - gzip -6: 11 to 103 ms at p50 (195 to 778 MB/s), 108 ms at most.
    - gzip -9: 18 to 215 ms (93 to 485 MB/s), 220 ms at most.
    - zstd -7: 8 to 57 ms (350 to 942 MB/s), 63 ms at most.
    - zstd -11: 24 to 175 ms at p50 (114 to 368 MB/s), 184 ms at p99 and at most.
  - **Read back whole.** gzip at 1.8 to 6.6 GB/s. ruzstd's zstd at 0.65 to 3.7 GB/s;
    zstd -11's at 0.88 to 3.7 GB/s, 23 ms at most for a segment of 20 MB.
  - **One frame of 1 MiB read back.** gzip -6 in 0.17 to 0.55 ms at p50, 0.75 ms at most.
    zstd -11 in 0.33 to 1.21 ms at p50, 1.71 ms at p99 and at most.
- **Consequence.** D162 compresses with zstd at klauspost's best level
  (`EncoderLevelFromZstd(11)`), in frames of 1 MiB with zstd's seekable format's seek
  table. That keeps 7.63% of the logs, where Docker's gzip of each whole file keeps 9.20%:
  17% less disk. It costs at most 184 ms of the packer's thread for a 20 MB segment. Any
  record is read by decompressing its 1 MiB frame, 1.7 ms at most, where Docker
  decompresses a whole rotated file to read any of it. Frames of 4 MiB would save 0.09
  points for four times the read; frames of 256 KiB cost 0.37 points. Only the ratios hold
  on every host. The times are arm64 macOS's; x86_64 and Linux are unmeasured.

### M232. How long a datagram a socket sends and takes on macOS, by its buffers (D163)

- **Question.** A syslog driver sends a line's 16 KiB parts as datagrams, over UDP or a
  unix socket, as moby's does. What does macOS take, and what lets it take more?
- **Method.** `docs/research/measurements/dgram-limits/probe.py`, on the M5 Max (macOS
  26.4.1), 2026-10-14:
  - the host's limits (sysctl);
  - a unix datagram of 3,000, 16,684 and 40,000 bytes, sent with a send buffer
    (`SO_SNDBUF`) one byte short of it, as long as it, and 16 bytes longer, to a receiver
    with a buffer of 1 MiB;
  - one of 16,684 bytes to receivers whose buffers (`SO_RCVBUF`) are as long as it plus 0,
    16, 64 and 512 bytes;
  - a UDP datagram of 16,684 bytes with the default send buffer, and with one as long as
    it.
- **Results.**
  - Limits: `net.local.dgram.maxdgram` 2,048, `net.local.dgram.recvspace` 4,096,
    `net.inet.udp.maxdgram` 9,216.
  - Sending, at all three sizes: a send buffer one byte short of the datagram is refused
    with EMSGSIZE, and one as long as it sends.
  - Receiving: a receive buffer exactly as long as the datagram is refused with ENOBUFS;
    16 bytes longer takes it.
  - UDP: 16,684 bytes are refused (EMSGSIZE) with the default send buffer, and sent with
    one as long as them.
- **Consequence.** D163 raises a datagram socket's send buffer to each message's length
  before sending it, and never lowers it. Linux's buffers are already larger than any
  message. A receiver's buffer is not the sender's to raise. So a line whose datagram is
  refused for its size (EMSGSIZE or ENOBUFS) goes in halves, each its own message, down
  to the 480 octets every receiver takes (RFC 5426 §3.2): nothing is lost, and nothing
  waits for ever.

### M233. What a TCP socket closed with bytes unread loses, and what its kernel says of what its receiver has (D163)

- **Question.** D163's first CI lost a run's last syslog line over TLS on every x86_64
  Linux run: its server got the line before it and then a reset. Why, and what can a
  sender learn of what its receiver has?
- **Method.** `docs/research/measurements/close-unread/`, 2026-10-14, at 29fc5846. Each
  probe ran on the M5 Max (macOS 26.4.1, Python 3.14.7, OpenSSL 3.6.4) and in a shards
  microVM on it (python:3-alpine: Linux 6.18.48 aarch64, Python 3.15.0, OpenSSL 3.5.9).
  - `probe.py`: a TLS 1.3 server on loopback (two session tickets, OpenSSL's default)
    reads until its connection ends. The client writes `out\n` and `err\n` as two
    records and ends its connection, 200 rounds each way:
    - `late-close`: 50 ms after the handshake, so the tickets have come, unread, it
      writes and closes at once, as the shipper did;
    - `late-nodelay`: the same, with TCP_NODELAY;
    - `late-drain`: the same, but it shuts its write side, reads until the server's
      EOF, then closes.

    Also 300 rounds each of `close`, `nodelay` and `drain`, which write as soon as the
    handshake is done.
  - `acked.py`: what the sender's kernel counts as unacknowledged, its RTO and its TCP
    state (Linux: SIOCOUTQ and TCP_INFO; macOS: TCP_CONNECTION_INFO's tcpi_snd_sbbytes,
    tcpi_rto and tcpi_state).
    Measured four times:
    1. 1,000 bytes written to a receiver that reads none;
    2. written on until the sender's buffer takes no more, the receiver's buffer set to
       4,096 bytes;
    3. all read, the receiver's buffer grown again;
    4. after shutdown(SHUT_WR), at once and 200 ms on.

    Two runs on each OS.
  - `unixq.py`: the same counts on a unix stream socket, 1,000 bytes unread, then 400
    read.
  - `held.py` (Linux): a listener's SO_RCVBUF of 1, three 1 KiB writes, then the
    receiver's SO_LINGER-0 close.
- **Results.**
  - `late-close` loses `err\n` in 200 of 200 rounds on Linux and 90 of 200 on macOS. The
    server sees a reset in all 400.
  - `late-nodelay` delivers both lines in all 400 rounds; the server still sees a reset
    in every one.
  - `late-drain` delivers both in all 400, each ending in a clean EOF.
  - `close`, `nodelay` and `drain` deliver both in all 900 rounds on Linux, since the
    tickets come after the client has closed. Their server sees a broken pipe or a reset
    as it writes them, except under `drain`.
  - `acked.py` on Linux, both runs:
    1. 0 unacknowledged;
    2. 2,568,704 of 2,604,032 bytes unacknowledged;
    3. 0;
    4. 1 at once (its FIN), in FIN_WAIT1; 0 200 ms on, in FIN_WAIT2.

    RTO 201 to 206 ms throughout.
  - `acked.py` on macOS:
    1. 0;
    2. the receiver's buffer is 526,720 bytes whatever it was set to. In one run the
       sender's buffer took no more after 327,212 bytes, none unacknowledged; in the
       other, 450,092 of 974,172 were unacknowledged;
    3. 0;
    4. 0 at once, in FIN_WAIT_1, and 0 200 ms on, in FIN_WAIT_2: macOS leaves the FIN
       out of the count. Its RTO reads 230 ms while the FIN is out and 0 otherwise.
  - `unixq.py`: macOS's SO_NWRITE reads 0. Linux's SIOCOUTQ reads 2,304 both before and
    after the 400-byte read: buffer memory, not bytes.
  - `held.py`:
    - the receiver's buffer is 2,304 bytes;
    - 2,048 bytes stay unacknowledged;
    - after the reset, SO_ERROR is 104 (ECONNRESET) and SIOCOUTQ still 2,048;
    - the next write fails with EPIPE.
- **Consequence.** A socket closed with bytes unread resets its connection, and what is
  not yet sent goes with it. The shipper lost the line Nagle's algorithm held back behind
  the one before it. TCP_NODELAY narrows that window but keeps the reset. Shutting the
  write side and reading the receiver's bytes until its end delivers everything.

  D163's streams (`daemon/ship/stream.rs`) therefore:
  - count a message delivered once its last byte is acknowledged. Linux's SIOCOUTQ counts
    the FIN; on macOS the FIN is added while the state is FIN_WAIT_1, CLOSING or LAST_ACK;
  - hold what is not yet delivered, and write it again on the next connection;
  - end a connection only once all is acknowledged, between checks waiting for the
    receiver's bytes or the kernel's RTO (RFC 6298's 1 s where it gives none);
  - keep the shipper's place only as far as what is delivered.

  Over a unix stream socket nothing can be counted. Its bytes are in the receiver's
  buffer once written, and no close resets them, so they are delivered as written.

### M234. What a search's "runs here" lookups cost against Docker Hub (D172)

- **Question.** A search's results are names. Whether each has an image for this
  machine's platform, and what it pulls, is a lookup of its own in the Hub API
  (`/v2/namespaces/NS/repositories/REPO/tags/latest`, documented in Docker Hub's API
  description). How long do a search's 25 lookups take, at how many at once, before Hub
  throttles them? Docker documents an "abuse rate limit" (a 429) without a number. And
  how many results have a `latest` to look up?
- **Method.** `docs/research/measurements/hub-lookups/measure.py`, 2026-10-14, at
  ceb0f651, on the M5 Max (macOS 26.4.1, Python 3.9.6), the host busy with other
  projects' builds. One index search for `python` (25 results), then each result's
  `tags/latest` fetched in 3 rounds at each of 1, 4 and 25 at once, 5 s between rounds:
  75 requests a level.
- **Results.**
  - A request took p50 196, p90 223, p99 266, max 266 ms one at a time; 200, 223, 370
    and 370 ms at 4; 212, 252, 404 and 404 ms at 25.
  - The 25 lookups took p50 5,029 ms (max 5,059) one at a time, 1,409 ms (max 1,424) at
    4, and 506 ms (max 525) at 25.
  - At 25 at once, one request of 75 was throttled (429); none at 1 or 4.
  - 11 of the 25 repositories have no `latest`: 404 in every round.
- **Conclusion.** Lookups go four at once, the most measured with nothing throttled: a
  quarter of the wall time of one at a time. 25 at once was faster still, and was
  throttled. A throttled lookup is its own result's alone. A repository without
  `latest` is said to have none: `shards run REPO` asks for `latest`, so that is the
  answer to whether it runs here.

### M235. CI's x86_64 musl test job, two weeks on: its slow runners at the step's edge

- **Question.** Gpu 9c7f6dca's x86_64 musl test job ran out of its step's 45 minutes
  (M187 set the split it runs under). How long does the job take now, runner to runner,
  and what limit does its step need?
- **Method.** GitHub's records of the `x86_64-unknown-linux-musl` test job in the last 14
  CI runs (2026-10-14 and 15, runs 38658570865 to 38688863631): the start and end of the
  job and of its test step; and the phases of 9c7f6dca's job log, by its timestamps.
- **Results.**
  - Two clusters. Three jobs took 29.2, 30.2 and 31.6 minutes; eleven took 37.4, 42.0,
    42.6, 43.3, 43.5, 43.6, 43.8, 43.9, 44.0, 44.9 and 45.3. Their medians, 43.6 and
    30.2 minutes, differ 1.44 times; M187's runners differed 1.7 times.
  - Each step took 0.1 to 0.2 minutes less than its job. Nine of the eleven slow steps
    reached 42 minutes, within three of the limit, and 9c7f6dca's was stopped by it.
  - 9c7f6dca's build took 28 min 10 s (M187: 13.5 to 22.7). Its tests then ran for 16.6
    minutes, and were stopped in `shards-seccomp`'s, among the workspace's last.
- **Consequence.** The step's limit becomes 60 minutes and the job's 70: a third above
  the slowest job measured, for the suite's growth until the job is split again. A limit
  this far out ends a hang, and paces nothing.

### M236. A read waiting on a socket as its peer half-closes, and SHUT_RDWR after one (D175)

- **Question.** Gpu's gate for D174 hung 9 hours in `a_compose_project_runs_as_microvms`,
  at `docker compose exec -T app testguest ask db:7000`.
  - The command had ended: db served it, and app's `top` showed its workload alone.
  - Compose waited for its stream's end. Its goroutine dump: `stdcopy.StdCopy` under
    `beginOutputStream`, in IO wait for 564 minutes.
  - The daemon's thread copying the client's stdin waited in `recvfrom` on the client's
    connection (`containers.rs:104`; lldb gave its descriptor), and the call's thread in
    its scope's join.
  - `lsof -T f` gave the client's socket SS_CANTSENDMORE: Compose's `CloseWrite` once
    its stdin, /dev/null, ended (docker/cli v29.2.1 hijack.go). The daemon's socket had
    SS_CANTRCVMORE but not SS_CANTSENDMORE: the end had come and its reader slept on,
    and the shutdown(SHUT_RDWR) that was to end the client's stream had shut nothing.

  How can a read sleep on a socket that has ended, how often does it, and what does
  SHUT_RDWR do there?
- **Source.** XNU (apple-oss-distributions/xnu, main).
  - `soreceive` (bsd/kern/uipc_socket.c), with nothing to read, looks for SS_CANTRCVMORE,
    lets go of its receive buffer and calls `pru_rcvd`. It then sleeps (`sbwait`) where
    `so_should_wait` still holds, which looks only at data and the low-water mark.
  - For AF_UNIX streams `pru_rcvd` is `uipc_rcvd` (bsd/kern/uipc_usrreq.c). Its
    `unp_get_locks_in_order` lets go of the reader's socket to take the peer's lock first,
    where the reader's socket has the higher address. A peer's shutdown(SHUT_WR) landing
    then (`unp_shutdown`, `socantrcvmore`) finds no sleeper to wake. A disconnect waits
    for it (UNP_DONTDISCONNECT); a half-close does not.
  - `soshutdownlock_final` refuses SHUT_RDWR (ENOTCONN) once SS_CANTRCVMORE is set,
    before it shuts the write side.
  - Linux (net/unix/af_unix.c): `unix_stream_data_wait` puts the reader on its wait
    queue and then looks for RCV_SHUTDOWN under `unix_state_lock`, which `unix_shutdown`
    takes to set it before it wakes the queue.
- **Method.** `docs/research/measurements/half-close/`, 2026-10-15, at 9c7f6dca.
  - `race`: each round a new socket pair. A reader thread enters recv(2) as the peer's
    thread calls shutdown(SHUT_WR), the two released together, each after a random spin
    of 0 to 399 iterations. A reader still waiting 1 s after the peer's shutdown
    returned counts as asleep, and a poll(2) of its socket then says whether its end was
    there. 50,000 rounds of each case.
  - `rdwr`: shutdown(SHUT_RDWR) of a socket whose peer half-closed 100 ms before, and of
    one whose peer is open, on AF_UNIX and on TCP; and whether the peer reads an end.
  - `roundtrip`: what waiting costs. Two processes pass a byte back and forth over an
    AF_UNIX socketpair, 100 batches of 1,000 round trips for each way of waiting, the
    ways in turn and rotated each batch, timed on the requester's side. Each side spins
    PAUSE µs before it sends: 0, and 50, so that each read waits on the other's work.
    The ways: `recv`, a recv(2) that may block; `poll`, a recv(2) with MSG_DONTWAIT,
    then poll(2) where nothing came, then the recv(2) again; `checked`, `poll` with the
    fcntl(2) and getsockopt(2) a read that keeps O_NONBLOCK and SO_RCVTIMEO makes;
    `kevent`, `poll` waiting in a kqueue that keeps the socket added; `oneshot`, the
    socket added to a kqueue with EV_ONESHOT in the kevent(2) that waits. Run three
    times, the host busy with other projects' builds (load average 12, 9 and 9).
  - On the M5 Max (macOS 26.4.1), and for `race` and `rdwr` in a shards microVM on it
    (alpine:3.22, Linux 6.18.48 aarch64, 18 vCPUs; built by zig 0.16.0 for
    aarch64-linux-musl).
- **Results.**
  - macOS, AF_UNIX, a recv(2) that may sleep: 360 of 50,000 readers asleep a second
    after the half-close, every one on a socket poll(2) found ended.
  - macOS, AF_UNIX, poll(2) for POLLIN then recv(2) with MSG_DONTWAIT: 0 of 50,000.
  - macOS, TCP on loopback: 0 of 50,000; and 0 of 50,000 where the reader had read 64
    KiB just before, so that its read updates the window.
  - Linux: 0 of 50,000 in each of the four cases.
  - macOS, SHUT_RDWR after the peer's half-close: ENOTCONN on AF_UNIX and on TCP, and
    the peer never reads an end. With the peer open it succeeds and the peer reads one.
    On Linux it succeeds in all four, and the peer reads its end.
  - `roundtrip` on macOS, the first run, n 99,900 each, µs:

    | PAUSE | way | p50 | p90 | p99 | max |
    |---|---|---|---|---|---|
    | 0 | recv | 6.08 | 11.54 | 38.71 | 6,317.79 |
    | 0 | poll | 8.00 | 16.62 | 47.62 | 1,683.38 |
    | 0 | checked | 8.08 | 16.46 | 52.21 | 2,574.12 |
    | 0 | kevent | 6.58 | 13.46 | 46.67 | 6,760.54 |
    | 0 | oneshot | 7.25 | 14.46 | 37.38 | 3,094.25 |
    | 50 | recv | 8.83 | 23.29 | 60.25 | 4,652.17 |
    | 50 | poll | 11.00 | 26.96 | 56.17 | 6,144.50 |
    | 50 | checked | 12.62 | 28.79 | 66.17 | 3,901.00 |
    | 50 | kevent | 10.21 | 26.04 | 60.21 | 3,748.54 |
    | 50 | oneshot | 10.58 | 26.00 | 57.58 | 3,905.50 |

    The second and third runs, `recv` and `checked`, p50 and p99 in µs:
    - PAUSE 0: `recv` 5.92 and 40.29, then 8.96 and 428.67; `checked` 8.17 and 51.38,
      then 14.79 and 435.92.
    - PAUSE 50: `recv` 14.71 and 478.79, then 14.33 and 536.38; `checked` 18.00 and
      457.88, then 18.25 and 473.50.
- **Consequence (D175).** On macOS no read of a Unix stream socket waits in the read: it
  waits in poll(2), which sees the end, the `checked` way, and then reads what is there
  without blocking. Over the three runs a round trip whose two ends both waited took
  2.0 to 5.8 µs more than with `recv` at p50 and 7.3 to 13.5 µs more at p99 at PAUSE 0;
  3.3 to 3.9 µs more at p50 at PAUSE 50, where p99 moved either way (−62.9 to +5.9).
  Linux's reads, which missed no end, and TCP's, stay as they are, and so cost nothing
  more. A stream, Unix or TCP, is ended one way at a time, SHUT_WR then SHUT_RD, so that
  its peer reads the end whichever side half-closed first.

### M237. What an agent holds as its peer is ended: its server spun, in the kernel (D60)

- **Question.** In gpu's gate for D174 and D175, `agents_out_of_memory_end_whole_and_spare_the_run`
  failed once in some thirty gates that had passed. After ending y, the filler, init also
  ended z, the agent the test must spare: "the agents' memory, 66117632 bytes, was past
  their share, 65912832, and its 64397312 the most". z holds 16 MiB and had held about
  18 MB as y was ended. What did z come to hold?
- **Method.** At c7330e7b, on the M5 Max (macOS 26.4.1), the guest Linux 6.18.48 aarch64.
  - The test run in a loop until it failed, init's message saying what the domain it ends
    held (`memory.stat`'s anon, file, shmem and kernel), and the test printing what init
    said in every run: 24 runs, then 80 more.
  - `docs/research/measurements/agents-oom/`: `sampler.patch` gives init a 250 ms timer
    that says each domain's `memory.stat` and each of its processes: state, CPU ticks from
    `/proc/PID/stat`, and `/proc/PID/syscall`. The test watches 8 s past y's end. Two runs.
- **Results.**
  - The loops: 1 failure in 104 runs. The share ranged from 62,554,112 to 67,600,384
    bytes, median 65,925,120. In the failure, z held anon 17,104,896 and **kernel
    47,230,976** bytes, with file and shmem 16,384 each. Each y ended held about 47 MB:
    anon 5.6 MB, shmem 38 to 42 MB (its filled scratch), kernel 0.57 MB.
  - The sampler, run 1: z's kernel memory went 14.2, 13.0, 10.7, 14.6, 22.3 and 42.3 MB
    in the 1.5 s after y's end, and init then ended z at 48.2 MB.
  - Run 2: z's kernel memory rose, unevenly, from 10.2 to 42.2 MB over 6.75 s, and init
    ended z. `slab_reclaimable` cycled between 1.6 and 13.3 MB within it, and the rest
    of `kernel` was outside every kind `memory.stat` names (slab, stacks, page tables,
    percpu, vmalloc, sock). z's `shards-server` was running (R) in every sample, 100 ticks
    a second (one CPU whole), 79% of them in the kernel. Its sandbox and the innocent
    process slept in ppoll.
- **Source.** The server's loop (crates/server/src/server.rs, `serve`) kept a peer's
  channel in its poll set once the peer was gone, with no events asked. poll(2) reports
  POLLHUP whatever is asked (fs/select.c, `do_pollfd`), and a SOCK_SEQPACKET socket
  whose peer has closed says it (net/unix/af_unix.c, `unix_dgram_poll`), so each poll
  returned at once. Each turn then called accept(2) on the listener, which makes a socket,
  a file and its dentry before it finds no connection waiting, and frees them after an RCU
  grace period (net/socket.c, `do_accept`). With one CPU turning that loop, what waited to
  be freed was charged to z's domain, and grew past the agents' share.
- **Consequence.** The server polls a peer only while it wants what the peer sends, and
  accepts and reads only where poll said so. The test watches 3 s past y's end and fails
  where z's server spent a second of CPU in them. Over three runs it spent 0 s; with the
  peer put back in the poll set it spent 3 s of 3, and the test failed. Init's word as it
  ends a domain for the agents' memory says what the domain held.

### M238. A log stream's close read as cut short on Linux: its send queue before its state

- **Question.** dev's CI at db1562db failed one test on x86_64 musl:
  `a_stream_closed_delivers_all_it_wrote` (D163, PM M233), "the connection ended before its
  receiver had all written on it", from a close of a TLS stream to a local server. It had
  passed in every gate and CI run since D175 (c7330e7b) changed the stream last. Was
  anything lost, and why did the close say so?
- **Method.** At 78811e3b, on the M5 Max (macOS 26.4.1), the guest Linux 6.18.48 aarch64,
  `docs/research/measurements/stream-close/`: `run.sh` builds shards' unit tests for the
  host's Linux and runs the test in a microVM of 2 vCPUs, eight loops at once of 25 runs
  each (`load.sh`); each run closes 20 streams, 4,000 a round. Then the guest kernel's
  TCP resets and aborts (`/proc/net/netstat`, `/proc/net/snmp`). `probe.patch` has a close
  that fails say its socket's TCP_INFO and what the server got.
  - Alone on a microVM of every host CPU, one loop of 50 runs: 1,000 closes.
  - Under load: rounds of 4,000 closes, before the change below, and after.
- **Results.**
  - Alone: 1,000 closes, no failure.
  - Under load, before: 4 failures in 6 rounds, 24,000 closes. In each, the server had
    got every byte, `first\nout\nerr\n`, and TLS's close_notify after them. The
    client's socket was closed (TCP_CLOSE) with no error pending, SIOCOUTQ 77 (the last
    two lines' TLS records, the close_notify's and the FIN), and TCP_INFO's unacknowledged
    packets 0. The guest sent and took no reset: TcpExt's aborts were all 0, and Tcp's
    EstabResets 0. (Its OutRsts and AttemptFails, one per stream, are `localhost`'s ::1,
    tried first and refused by the IPv4 listener.)
  - Under load, after: 0 failures in 6 rounds, 24,000 closes.
- **Source.** The close read SIOCOUTQ, `write_seq - snd_una` (net/ipv4/tcp.c, `tcp_ioctl`),
  then TCP_INFO's state, in two calls. The server's acknowledgement of the last records,
  and its FIN, came between them: the socket went to TIME_WAIT, its own state TCP_CLOSE
  (net/ipv4/tcp_minisocks.c, `tcp_time_wait`), and the close read the count from before
  and the state from after. A connection read as closed takes no more acknowledgements,
  so a count read after the state is its last.
- **Consequence.** `ship::sockets::unacknowledged` reads TCP_INFO first, then SIOCOUTQ.
  macOS reads both in TCP_CONNECTION_INFO, one call, and never had it.

### M239. A running microVM's save and restore, from the caller's view (D176)

- **Question.** How long do `shards save vm` and `shards restore vm` take for their caller,
  how long is the guest paused by a save it is left running after, and where does the
  time go, on a quiet host and on a loaded one?
- **Method.** At 9c7100f1, with the save's phases logged (`warm.rs`, at info), on the M5
  Max (18 CPUs, 128 GiB, macOS 26.4.1), release build, the guest Linux 6.18.48 aarch64 of
  `alpine:3.22` (256 MiB, 18 vCPUs) counting each millisecond.
  `docs/research/measurements/save-restore/`: `bench.py` runs ROUNDS microVMs, and of each
  times `save vm --leave-running`, the longest gap in its log's timestamps around it (what
  a reader of its output waits), `save vm`, and `restore vm` until it runs again; `run.sh`
  runs it in a home of its own and reads each save's phases from the daemon's log: its
  guest going quiet (QUIESCE to QUIESCED), then the microVM saved (vCPUs parked, memory
  and state written, vCPUs released). With `load`, 12 busy loops run beside it. Four runs
  of 100 rounds: two quiet without the phases, one quiet and one loaded with them.
- **Results.** In ms, n = 100 each (200 for the phases, two saves a round).

  | | quiet p50 | p90 | p99 | max | loaded p50 | p90 | p99 | max |
  |---|---|---|---|---|---|---|---|---|
  | `save --leave-running`, client | 91.8 | 101.1 | 120.2 | 120.2 | 141.4 | 272.8 | 327.7 | 327.7 |
  | its log's stall | 19.1 | 21.7 | 23.8 | 23.8 | 24.6 | 93.2 | 152.5 | 152.5 |
  | `save`, stopped, client | 79.4 | 92.0 | 145.7 | 145.7 | 106.1 | 155.3 | 217.0 | 217.0 |
  | `restore`, client | 18.4 | 22.1 | 67.2 | 67.2 | 24.8 | 29.6 | 45.2 | 45.2 |
  | guest quiet | 0.084 | 0.126 | 5.58 | 7.90 | 0.096 | 0.160 | 2.77 | 4.76 |
  | microVM saved | 17.2 | 20.1 | 33.9 | 41.5 | 20.4 | 25.5 | 38.5 | 46.2 |

  The two quiet runs without the phases, in ms: one's saves left running p50 92.9, p99
  244.3, its log's stall p50 21.0, p99 148.2, saves stopping p50 86.8, p99 169.6, and
  restores p50 20.0, p99 36.2; the other's saves left running p50 167.0, p99 241.6, its
  stall p50 80.4, p99 131.5, saves stopping p50 75.7, p99 130.0, restores p50 19.1, p99
  34.0, where the VMM's own save (its "saved for the host" lines) stayed p50 16.6, max
  22.6. Each snapshot took 74 to 82 MB of disk, of the guest's 256 MiB and the memory it
  plugged.
- **Consequence.** The guest is paused for the microVM's save, its vCPUs parked while
  memory and state are written: 17 ms at p50 quiet, 46 ms at most loaded. Its going quiet
  costs under 0.2 ms at p90. What a log reader waits beyond that is the held output's
  delivery once the guest runs again, which a loaded host's scheduler stretches, as it
  varied between quiet runs (p50 19 to 80 ms) while the save itself did not. A caller's
  save is that and the commit, the generation's files made durable (F_FULLFSYNC); a
  restore, the VM process's start, the snapshot restored, and the guest dialing back in.

### M240. A microVM's I/O limits, from its guest's writes (D178)

- **Question.** Does a microVM whose VMM paces its volume's writes at `--io-maxbandwidth`
  write at that rate, never above it, on a quiet host and on a loaded one; and what does
  the limiter cost a write it does not hold back?
- **Method.** On the tree of D178's commit (on 4614b300), on the M5 Max (18 CPUs, 128 GiB,
  macOS 26.4.1), release build, the guest Linux 6.18.48 aarch64 of `alpine:3.22`.
  `docs/research/measurements/io-limit/`: `bench.py` runs ROUNDS rounds of three microVMs,
  each writing to a volume with busybox `dd` (64 KiB writes, synced at the end) between two
  lines its log times (`logs -t`): 16 MiB under `--io-maxbandwidth 4m`, its achieved rate
  over the limit; and, interleaved, 64 MiB with no limit and under 1 GiB a second, which no
  write comes near. `run.sh` runs it in a home of its own; with `load`, 12 busy loops run
  beside it. The host's own load from other work ran 20 to 50 (load averages) throughout.
- **What it found first.** Three things cost a paced writer its rate, each found by
  counting the device's requests and the limiter's waits, and each fixed (limit.rs):
  - Waits overran their deadlines on the loaded host, a wait asked for 9.0 ms taking 13.9,
    3.7 taking 5.1, 11.3 taking 17.1, and each overrun was lost to the rate: 16 MiB at 4
    MiB a second took 7.0 to 7.5 s, half the rate. Giving the overrun back by moving the
    schedule earlier overshot instead: 1.19 of the limit at p50, 1.27 at most. A late
    request now keeps its place in the schedule, never going before it.
  - Each 64 KiB WRITE came with a GETXATTR of no bytes (the kernel's capability check on
    each write), which waited out the WRITE's time, so that the next WRITE came late by the
    GETXATTR's answer and the guest's turning round: 5.5 to 6.0 s. A request that costs a
    measure nothing now neither waits on it nor moves it: 3.913 to 4.456 s.
  - A request of no bytes took the late request's place in the schedule for the bytes it
    did not charge: 0.84 of the limit at p50. Each measure now keeps its own.
- **Results.** Achieved over the limit, n = 10 each: quiet, p50 0.844, p90 0.894, max
  0.894; loaded, p50 0.841, p90 0.931, max 0.931. Never above it. Seconds for 64 MiB,
  quiet: no limit p50 4.618, p90 7.617, max 7.617; under 1 GiB a second p50 4.565, p90
  11.448, max 11.448; loaded: no limit p50 1.510, p90 2.105, max 2.105; under 1 GiB a
  second p50 1.464, p90 2.223, max 2.223. Probes of three rounds each, 16 MiB at 4 MiB a
  second interleaved with 16 MiB unlimited, at a moment the host's storage was fast (its
  unlimited 16 MiB in 0.07 s), of the test-release build (release's optimizations without
  its fat LTO): 4.001, 4.001, 3.997 s, where the limit's time is 3.98 s (its first write
  going at once); and of the release build at one it was slow (its unlimited 16 MiB in 1.4
  to 3.5 s): 5.69 to 6.22 s.
- **Consequence.** The limit is a ceiling, kept: no run went over it. Where the host's
  storage answers within a write's place in the schedule (64 KiB in 15.6 ms at 4 MiB a
  second), the rate is the limit's, to 0.4%; where it stalls past that, as it did beside
  the bench's 64 MiB writes and the host's own load, the writer gets the less it can take,
  and catches up nothing, which would take a burst past the limit. A limit that does not
  hold a write back costs it nothing measurable: the 64 MiB writes with none and with one of
  1 GiB a second agree at p50 within 4%, either faster, where the runs vary by more than
  half.
