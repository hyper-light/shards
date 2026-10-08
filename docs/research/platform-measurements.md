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
  `published_ports_reach_the_guest_as_dockerd_publishes_them`, four at once, three
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
  it until measured on x86_64, and `run_limits_resources_as_docker_run_does` holds, on
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

