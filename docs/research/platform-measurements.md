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
