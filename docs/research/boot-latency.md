# Cold-boot latency and guest-kernel footprint of microVMs

Research note, 2026-09-28. Evidence only; nothing here is a final design decision.

Pinned local sources: firecracker `edb6061`, linux `v7.2-rc4` (`1590cf0`), libkrun `1f5dd02`, go-microvm `7e148d8`, Apple `MacOSX26.4.sdk` Hypervisor.framework headers.

Markers: **(fig.)** = value read off a published plot (±~5%). **(derived)** = our arithmetic on cited numbers. **(inference)** = reasoning from source code, not measured. **UNVERIFIED** = no acceptable source found.

## 1. Scope

- **Q1.** Where does microVM cold-boot time go?
- **Q2.** Which guest-kernel specialization techniques are proven, and by how much?
- **Q3.** How much memory do the guest kernel and a minimal userspace use, and how can that be reduced?
- **Q4.** What does VMM-side startup cost? How does Firecracker specify and measure boot? What does the literature say about toolstack overhead?
- **Q5.** What is the realistic floor for cold boot versus snapshot restore? Can a cold boot meet ≤5 ms? What cold-boot target should shards set for building snapshot templates?

## 2. Findings

### 2.1 Q1: Where cold-boot time goes

The papers measure different intervals, so only numbers from the same paper can be compared:

- **Firecracker paper:** VMM fork → guest forks init; the init writes an I/O port [Agache20 §5.1].
- **SEVeriFast:** VMM exec → init, split into VMM, verifier, decompressor and Linux phases via port-0x80 writes [Holmes24 §6.1].
- **Unikraft:** VMM time and guest time (first guest instruction → `main()`) reported separately [Kuenzer21 §5.1].
- **Firecracker spec:** InstanceStart → `/sbin/init` [fc: SPECIFICATION.md:37-42].
- **VEE'20 Firecracker/gVisor study:** no boot latency at all; host-kernel code coverage and I/O microbenchmarks only [Anjali20 §3–7].

**Table 1. Phase costs**

| Phase | Measured cost | Source |
|---|---|---|
| VMM process start → API socket | ≤8 CPU-ms; wall-clock 6–60 ms, typically 12 ms | [fc: SPECIFICATION.md:13-17] |
| Process spawn + REST configuration | Median ≈160 ms end-to-end vs ≈110 ms pre-configured, serial boots (fig.) | [Agache20 §5.1, Fig. 5] |
| Construct a KVM VM and execute `hlt` | ≈1.2×10⁵ cycles, ≈45 µs at 2.69 GHz (fig., derived) | [Wanninger22 §4.2, Fig. 2] |
| VMM work before the first guest instruction | ≈10 ms (Lupine and AWS kernels); ≈19 ms (Ubuntu, 61 MB vmlinux) (fig.) | [Holmes24 §6.1–6.2, Figs. 8, 11] |
| Whole-VMM share for a unikernel guest | Firecracker/Solo5 ≈3 ms; QEMU microvm ≈10 ms; QEMU ≈40 ms | [Kuenzer21 §5.1, Fig. 10] |
| Firmware | None with direct boot; qboot saves ≈20 ms vs QEMU's default BIOS; OVMF ≈3.2 s under SEV-SNP | [Holmes24 §1, §3.1, Fig. 10]; [Agache20 §5.1] |
| Kernel decompression | ≈40 ms | [Agache20 §5.1] |
| Linux kernel entry → init | ≈17 ms (Lupine config), ≈35 ms (Firecracker "AWS" config), ≈127 ms (Ubuntu config). Linux 6.4, 1 vCPU, 256 MB; SEV-enabled builds booted without SEV (fig.) | [Holmes24 §6.1–6.2, Fig. 11] |
| Distro kernel | +900 ms: probe timeouts for legacy devices that are not emulated, plus unneeded drivers | [Agache20 §5.1] |
| ACPI scan + PCI enumeration + rootfs population | ≈30% of a vanilla QEMU/KVM Linux boot | [Wanninger22 §4.2] |
| Serial console logging | Up to 70 ms | [Agache20 §5.1] |
| One static network interface | +≈20 ms (Firecracker, Cloud Hypervisor); +≈35 ms (QEMU) | [Agache20 §5.1] |
| Host rootfs preparation | device-mapper: ≈30 ms for one sandbox, ≈10 s at 200 concurrent. Reflink volatile block device: 0.2 ms | [Li22 §3.1, §4.2] |
| Host cgroup creation | Serialized by global locks; a pool of pre-created cgroups plus rename cuts creation time by 94% | [Li22 §3.3, §4.4] |
| Userspace | Tinyx: 180 ms boot on Xen. Alpine: ≈330 ms on Firecracker. Firecracker under containerd/OpenNebula: 700–1300 ms | [Manco17 §4.2]; [Kuenzer21 §5.1]; [Ustiugov21 §2] |

**End-to-end reference points:**

- Pre-configured Firecracker: p99 146 ms with 50 parallel boots, 153 ms with 100 concurrent (Linux 4.14, 1 vCPU, 256 MB) [Agache20 §5.1, Fig. 6].
- Firecracker with the AWS microVM kernel: "about 40ms" (Linux 6.4, AMD EPYC 7313P) [Holmes24 §3.1, §6.1].
- RunD: 88 ms per sandbox; 200 sandboxes within 1 s [Li22 §5.2].
- Boot time grows linearly with image size [Manco17 §2, Fig. 2], consistent with the larger VMM share for the 61 MB Ubuntu vmlinux in Table 1.

**Takeaway.** Firmware, decompression, legacy probing and the console are avoidable costs of 20–900 ms each. Once they are removed, the guest kernel dominates at ≈17–35 ms and the VMM adds ≈3–10 ms.

### 2.2 Q2: Kernel specialization, proven effects

**Config tailoring**

- **Lupine** started from Firecracker's microVM config and removed ≈550 options (66%), leaving 283 ("lupine-base") [Kuo20 §3, Fig. 4]. Against that baseline on Linux 4.0 [Kuo20 §1, §4, §4.2–4.4]:

  | Metric | Lupine | Change vs baseline |
  |---|---|---|
  | Kernel image | 4 MB | 27% of the baseline image |
  | Boot time | 23 ms | 59% faster |
  | Memory footprint | 21 MB | 28% lower |

  - 19 extra options cover the 20 most-downloaded Docker Hub applications [Kuo20 §4.1].
  - A build 6% smaller ("-tiny") booted no faster: "boot time is more about reducing the complexity of the boot process than the image size" [Kuo20 §4.2–4.3].
- **RunD** condensed its CentOS 4.19 kernel for ≈16 MB less memory and ≈4 MB less image [Li22 §4.3.1]: no pre-created loop devices (2.2 MB), no ACPI (2 MB), no ftrace (6 MB), no graphics (2 MB), no i2c or ceph (3 MB).
- **Tinyx** starts from `tinyconfig`, disables modules and removes options iteratively. Its kernels are half the size of a typical Debian kernel [Manco17 §3.2].
- **Firecracker's CI guest configs are not minimal:** 1,332 (arm64) and 1,391 (x86) `=y` options, including ACPI, PCI, MODULES and NUMA [fc: resources/guest_configs/microvm-kernel-ci-{aarch64,x86_64}-6.18.config].
- **Kernel ground truth:** `make tinyconfig` applies `kernel/configs/tiny.config` (XZ, SLUB_TINY, dead-code elimination) [linux: scripts/kconfig/Makefile:115-116]. SLUB_TINY "is not recommended for systems with more than 16MB RAM" [linux: mm/Kconfig:177-186].
- **Conflict with shards.** Lupine removed ~20 cgroup/namespace options and 12 security-domain options [Kuo20 §3.1.2]. A rootless in-VM container engine needs exactly those.

**CONFIG_PARAVIRT (x86).** The Lupine authors call it "a primary enabler of fast boot time". Without it, Lupine boots in 71 ms instead of 23 ms [Kuo20 §4.3].

**ACPI, PCI and the virtio transport**

- Firecracker appends `pci=off` when PCI is disabled [fc: src/vmm/src/builder.rs:217-219]. `pci=off` is documented for x86 only [linux: Documentation/admin-guide/kernel-parameters.txt:5035].
- The PCI core scans all 32 device slots of every bus [linux: drivers/pci/probe.c:3093-3094]. Each config-space access must be trapped and emulated by the VMM (inference).
- virtio-mmio "provides no generic device discovery mechanism": the guest must be told each device's registers and interrupts [VIRTIO1.3 §4.2.1]. It has one interrupt per device, signalled through InterruptStatus/InterruptACK registers, with no MSI [VIRTIO1.3 §4.2.2].
- Firecracker has deprecated command-line virtio-mmio devices and MPTable [fc: docs/kernel-policy.md:141-146]. It recommends the PCI transport for "higher throughput and lower latency" [fc: docs/getting-started.md:213-217].
- We found no peer-reviewed measurement comparing virtio-mmio and virtio-pci probe cost (see E2).

**Compressed vs uncompressed kernel**

- Decompression adds ≈40 ms [Agache20 §5.1]. A bzImage "costs additional boot time and guest memory" [fc: docs/rootfs-and-kernel-setup.md:13-16].
- LZ4 decompresses faster than LZO; ZSTD is "around the same speed as LZO, but slower than LZ4" [linux: init/Kconfig:406-434].
- `KERNEL_UNCOMPRESSED` exists only for parisc, s390 and riscv [linux: init/Kconfig:436-445]. arm64 has no in-kernel decompressor: the bootloader decompresses, or the raw `Image` is used [linux: Documentation/arch/arm64/booting.rst:61-71].
- **Exception (SEV).** Under SEV measured boot the kernel must be hashed and copied, which can take twice as long for an uncompressed kernel. An LZ4 bzImage with an uncompressed initrd is fastest [Holmes24 §3.3, §6.2, Fig. 5]. Sizes, vmlinux / LZ4 bzImage [Holmes24 Fig. 8]: Lupine 23/3.3 MB; AWS 43/7.1 MB; Ubuntu 61/15 MB.
- **KASLR.** On x86, KASLR is chosen inside the decompressor [linux: arch/x86/boot/compressed/misc.c:490], so booting vmlinux directly gives it up (inference). arm64 takes its seed from `/chosen/kaslr-seed` [linux: arch/arm64/kernel/pi/kaslr_early.c:23].

**Preset `lpj`: little or no benefit on arm64 and x86**

- The docs say `lpj=` avoids autodetection costing "up to 250 ms per CPU" [linux: kernel-parameters.txt:3692-3700].
- But the calibration loop only runs as a fallback:
  - arm64 sets `lpj_fine = arch_timer_rate / HZ` [linux: arch/arm64/kernel/time.c:69].
  - x86 derives `lpj_fine` from the TSC [linux: arch/x86/kernel/tsc.c:1547].
  - `calibrate_delay()` then skips the loop [linux: init/calibrate.c:294-297].
  - x86 secondary CPUs reuse CPU0's value when the TSC is constant [linux: arch/x86/kernel/tsc.c:1567-1577].

**Async probing**

- Controls: `driver_async_probe=` and `module.async_probe` [linux: kernel-parameters.txt:1351-1356, 4161-4165]. The kernel calls PROBE_PREFER_ASYNCHRONOUS "a temporary measure … to speed up boot" [linux: include/linux/device/driver.h:26-50].
- `initramfs_async=1` is the default: the initramfs is unpacked while devices are probed [linux: kernel-parameters.txt:2404-2414].
- There is no peer-reviewed number. With one vCPU, async probing can only overlap waits (inference).

**initramfs vs block/pmem rootfs**

- rootfs is always a ramfs/tmpfs instance [linux: Documentation/filesystems/ramfs-rootfs-initramfs.rst:78-88]. ramfs pages "can't be freed by the VM" [ibid.:27-30]. A ramdisk double-copies into the page cache [ibid.:40-51].
- An uncompressed initrd beats a compressed one [Holmes24 §3.3].
- Firecracker's boot benchmark compares pmem with `rootflags=dax` against a virtio-blk rootfs [fc: tests/integration_tests/performance/test_boottime.py:118-135]. DAX removes the page-cache copy [linux: Documentation/filesystems/dax.rst:11-14].

**SMP bring-up**

- **arm64 is serial.** For each CPU, `__cpu_up` issues PSCI CPU_ON (a trap to the VMM), then waits in `wait_for_completion_timeout` [linux: arch/arm64/kernel/smp.c:111-135; arch/arm64/kernel/psci.c:39-42; booting.rst:604-613].
- **x86_64 is parallel.** It selects HOTPLUG_PARALLEL [linux: arch/x86/Kconfig:307], controlled by `cpuhp.parallel=` [kernel-parameters.txt:1028-1031]. arm64 does not select it.
- `maxcpus=n` boots only n CPUs; the rest can be onlined later via sysfs [kernel-parameters.txt:3716-3723].
- On HVF, each vCPU is created and run on its own thread [hvf: hv_vcpu.h:20-28, 359-369].
- The literature gives no per-vCPU cost (see E4).

**Console**

- Turning off serial console logging saved up to 70 ms [Agache20 §5.1]. Firecracker's default command line has `8250.nr_uarts=0` [fc: src/vmm/src/vmm_config/boot_source.rs:10-20].
- Each character costs ≥2 trapped register accesses:
  - 8250: poll LSR, then write THR [linux: drivers/tty/serial/8250/8250_port.c:3243-3254].
  - PL011: read FR, then write DR [linux: drivers/tty/serial/amba-pl011.c:2453-2461].
- A round-trip trap to a user-space-emulated UART costs 6,732 cycles on x86 and 7,630–10,012 on arm64 [Dall17 §5.1, Table 2]. On HVF every exit returns to the VMM thread [hvf: hv_vcpu.h:359-369].
- Estimate: ≈4–8 ms of exits per KiB of console output, at 2 exits/char and 2–4 µs/exit (derived).
- virtio-console (via hvc) sends 16-byte chunks, one kick each [linux: drivers/tty/hvc/hvc_console.c:49,151-154; drivers/char/virtio_console.c:614-636]. That is ≈32× fewer exits (derived).

**Deferred initcalls.** There is no mainline mechanism: no `deferred_initcall` symbol exists in the tree (UNVERIFIED as a usable technique). Only `initcall_blacklist=` and `initcall_debug` exist [linux: kernel-parameters.txt:2396-2402]. DEFERRED_STRUCT_PAGE_INIT targets "very large machines" [linux: mm/Kconfig:1169-1183].

**Huge-page backing of guest memory.** 2 MiB hugetlbfs pages improve Firecracker boot "by up to 50%" [fc: docs/hugepages.md:41-47]. Under SEV, THP cut `pvalidate` time from over 60 ms to under 1 ms [Holmes24 §6.1].

### 2.3 Q3: Memory footprint

**Table 2. Measured footprints**

| Item | Footprint | Definition and source |
|---|---|---|
| Firecracker VMM | ≈3 MB | Non-shared pmap segments, binary excluded; constant across VM sizes [Agache20 §5.2, Fig. 7] |
| Firecracker spec / CI | ≤5 MiB at 1 vCPU and 128 MiB. CI thresholds: 5 MiB booted, 7 MiB snapshotting, 5 MiB restored | [fc: SPECIFICATION.md:24-35]; [fc: tests/host_tools/memory.py:49-51] |
| Cloud Hypervisor / QEMU VMM | ≈13 MB / ≈131 MB | [Agache20 §5.2, Fig. 7] |
| SEV add-on | ≈16 KB | [Holmes24 §6.3] |
| Kata-FC sandbox overhead (kernel, agent, per-page structures) | 94 MB at a 128 MB spec; 71 MB average at 1,000 VMs | [Li22 §3.2, Fig. 5] |
| RunD sandbox | <20 MB; 75.1% below Kata-FC at 1,000 sandboxes | [Li22 §5.3, Figs. 11–12] |
| Lupine: minimum memory to run the app | 21 MB | [Kuo20 §4.4, Fig. 8] |
| Tinyx | ≈27 GB per 1,000 guests, ≈27 MB each (derived); a TLS-proxy VM uses 40 MB | [Manco17 §6.3, Fig. 14; §7.3] |
| Debian VM | ≈111 MB each | [Manco17 §6.3] |
| Unikernels | 3.6 MB (Mini-OS "daytime" unikernel); 2–6 MB (Unikraft apps) | [Manco17 §3.1]; [Kuenzer21 §5.1, Fig. 11] |

**Structural costs inside the guest**

- **struct page.** Each page frame costs a 64-byte `struct page` [linux: Documentation/mm/vmemmap_dedup.rst:73]. That is 1.56% of guest RAM with 4 KiB pages (2 MiB per 128 MiB) and 0.39% with 16 KiB pages (derived).
  - Firecracker's arm64 CI kernel uses 4 KiB pages [fc: microvm-kernel-ci-aarch64-6.18.config].
  - RunD attributes Kata-FC's overhead growth with memory size to management structures built for all configured memory [Li22 §5.3].
- **SWIOTLB.** The bounce buffer defaults to 64 MiB [linux: include/linux/swiotlb.h:36]. arm64 allocates it when RAM extends past the DMA limit [linux: arch/arm64/mm/init.c:339-356]. `swiotlb=noforce` skips it [linux: kernel/dma/swiotlb.c:199-200,364]; Firecracker passes it by default [fc: boot_source.rs:10-20].

**Techniques that reduce footprint**

- **Share kernel text across VMs.** Mapping the kernel file shares text and read-only data.
  - Self-modifying code breaks this: of 10,012 KB of accessed code/rodata, 7,928 KB was modified during boot. RunD therefore ships a pre-patched kernel [Li22 §3.2, §4.3.2].
  - libkrun maps the kernel straight out of libkrunfw on x86_64, but copies it on aarch64 [libkrun: src/libkrun/src/vmm/builder.rs:1576-1611, 1613-1712].
- **Load snapshots lazily and share them.** Firecracker maps snapshot memory `MAP_PRIVATE`: on-demand loading, copy-on-write, pages shareable between microVMs [fc: docs/snapshotting/snapshot-support.md:78-87]. RunD's template keeps overhead flat as the memory spec grows [Li22 §5.3].
- **Avoid double-caching the rootfs.**
  - A virtio-blk rootfs duplicates the page cache in host and guest.
  - virtio-fs with DAX avoids this, but has poor write performance and a daemon per sandbox.
  - RunD therefore uses virtio-fs for the read-only layer and a reflinked volatile block device for the writable one [Li22 §3.1, §4.2].
  - Prior art: go-microvm flattens OCI layers into a host directory and passes it to libkrun as the root (`krun_set_root`; a DAX window is configurable via `krun_add_virtiofs3`) [go-microvm: README.md:32-36; krun/context.go:103; krun/libkrun.h:113-126].
- **Return free memory to the host.** Balloon free-page reporting hands freed guest pages back [fc: docs/ballooning.md:46].
- **Allocate lazily.** Lupine's footprint is independent of the application, which its authors attribute to lazy allocation [Kuo20 §4.4].

### 2.4 Q4: VMM-side startup

**Firecracker's targets** on m5d.metal and m6g.metal [fc: SPECIFICATION.md:8-42]:

- Process start → API socket: ≤8 CPU-ms (wall-clock 6–60 ms, typically 12 ms).
- VMM memory overhead: ≤5 MiB.
- InstanceStart → `/sbin/init`: ≤125 ms, "with the serial console disabled and a minimal kernel and root file system".

The Firecracker paper claims <5 MB overhead, <125 ms to application code, and up to 150 microVMs/s per host [Agache20 §1].

**How Firecracker measures**

- **Boot timer.**
  - The start timestamp (wall-clock and CPU) is taken on entry to `build_microvm_for_boot` [fc: src/vmm/src/builder.rs:149-150]. The device is attached first so its MMIO address is fixed [builder.rs:221-226].
  - The guest's init wrapper maps `/dev/mem` at 0xc0000000 (x86) or 0x40000000 (arm64), writes 123, and execs `/sbin/init` [fc: resources/rootfs/overlay/usr/local/bin/init.c:15-43].
  - The device logs `Guest-boot-time` in µs and CPU-µs [fc: src/vmm/src/devices/pseudo/boot_timer.rs:11-41].
  - Process spawn and API configuration are excluded.
- **`test_boottime`** [fc: tests/integration_tests/performance/test_boottime.py:16-19,102-219]:
  - Matrix: 1 vCPU/128 MiB … 4 vCPU/4 GiB × pmem-DAX vs block rootfs × THP on/off; 10 boots per cell; pinned threads.
  - Also records build/resume event durations and the `systemd-analyze` split.
  - Boot args add `cryptomgr.notests`.
- **Process startup:** measured to the API socket over 100 iterations [fc: tests/integration_tests/performance/test_process_startup_time.py:13-85].
- **Memory overhead:** psutil maps of a *restored* VM, excluding guest memory [fc: tests/integration_tests/performance/test_memory_overhead.py:49-97].
- **Single-shot configuration** via `--config-file`/`--no-api` [fc: src/firecracker/src/main.rs:201-213]. The paper proposed one combined API call, with memory allocation and kernel loading at configuration time [Agache20 §5.1].
- **Host pitfalls.**
  - `KVM_CREATE_VM` regressed on 6.1 x86 hosts; the mitigations (`favordynmods`, `kvm.nx_huge_pages=never`) need root [fc: docs/prod-host-setup.md:389-455].
  - The jailer's cgroup setup needs privileges [fc: docs/jailer.md:47-50].

**Toolstack literature**

- **Stock Xen:** `xl` creates the first VM in ≈100 ms and the 1,000th in ≈1 s. XenStore traffic (superlinear) and device creation dominate [Manco17 §4.2, Fig. 5; §6.1, Fig. 9].
- **LightVM,** cumulative optimizations on the daytime unikernel [Manco17 §5.1–5.2, §6.1, §7.4]:

  | Configuration | Creation time |
  |---|---|
  | chaos (replacement toolstack) | 15–80 ms |
  | + split toolstack | ≤≈25 ms |
  | chaos + noxs (no XenStore) | 8–15 ms |
  | all optimizations | 4 ms; 4.1 ms at the 1,000th VM |
  | noop unikernel, no devices | 2.3 ms |
  | from pre-created VM shells | constant 1.3 ms |

- **Virtines:** pooled VM shells bring provisioning to within 4% of a bare `vmrun` [Wanninger22 §5.2, Fig. 8].
- **Lambda:** keeps a small pool of pre-booted microVMs. By Little's law, at 125 ms per creation that is one pooled VM per 8 creations/s [Agache20 §4.1.2].
- **RunD:** pools pre-created cgroups and renames them on use [Li22 §4.4].

**HVF (no published timings)**

- Every exit returns to the calling thread [hvf: hv_vcpu.h:359-369]; one vCPU per thread [hvf: hv_vcpu.h:20-28].
- In-kernel GICv3 via `hv_gic_create` (macOS 15+) [hvf: hv_gic.h:23-49].
- Stage-2 IPA granule selectable, 4 or 16 KB (macOS 26+) [hvf: hv_vm_config.h:110-146].
- `hv_vm_map` requires page alignment [hvf: hv_vm.h:44-53].
- The headers contain no IOMMU or device-assignment API (grep).

### 2.5 Q5: The floor, cold boot vs restore

**Cold boot**

- Lowest Linux-to-init numbers found:
  - 23 ms: Lupine, Linux 4.0, Firecracker on x86 [Kuo20 §1].
  - ≈27 ms from VMM exec: Lupine config on Linux 6.4, stock Firecracker; ≈10 ms VMM + ≈17 ms kernel (fig.) [Holmes24 Fig. 11].
  - 18 ms: Lupine without KML, as reported by the Unikraft authors [Kuenzer21 §5.1].
- Every published boot under 5 ms is not Linux:
  - unikernels: 2.3–4 ms with LightVM [Manco17 §6.1]; ≈3 ms on Firecracker, with <1 ms in the guest [Kuenzer21 §5.1];
  - a sandbox fork: Catalyzer's `sfork` at 0.97 ms, which is gVisor-based, not a hardware VM [Du20 §6.2].
- No peer-reviewed Linux cold boot ≤5 ms was found. The best (18–27 ms) is ≈3.5–5.5× over budget (derived), before any in-VM engine starts.

**Restore**

- Restoring VM state takes "just milliseconds", but first-touch paging dominates [Ao22 §2.4]. Hello-world on a 2 GB guest [Ao22 §3.2, Fig. 1]:

  | Setting | Time |
  |---|---|
  | Warm VM | 4 ms |
  | Disk-backed Firecracker snapshot | 229 ms |
  | Snapshot memory file in host page cache | 65 ms |
  | REAP | 62 ms |

- Per-fault cost: under 4 µs warm; 3.7 µs mean for page-cache minor faults; 13.3 µs mean when faults hit disk [Ao22 §3.3, Fig. 2].
- Vanilla Firecracker under vHive: 50 ms to restore the VMM and devices, then 182 ms of fault-bound processing. REAP's working-set prefetch takes 15 ms [Ustiugov21 §6.2].
- Fault budget (derived): 5 ms ÷ 3.7 µs ≈ 1,350 minor faults ≈ 5.3 MiB of 4 KiB pages. Anything larger must be pre-mapped or huge-page backed.

**Verdict.** A ≤5 ms "environment ready" start is reachable only by restoring or cloning a snapshot taken after initialization, with all of the following already in place:

- the snapshot's memory resident on the host;
- a pre-created VMM process and VM shell;
- pooled host resources (cgroups, rootfs, network devices).

Each of these has a published precedent above. Cold boot remains the template-building path.

## 3. Implications for shards (ranked)

1. **Two paths: restore for the ≤5 ms start, cold boot only for building templates.**
   - *What:* snapshot after the in-VM engine and compose services are up (init-less start) [Du20 §2.2, Fig. 5; Ustiugov21 §1, §2.2]. Restore into pre-spawned VMM processes and shells [Manco17 §5.2; Wanninger22 §5.2]. Pool per-VM host resources [Li22 §4.2, §4.4].
   - *Benefit:* the only evidence-backed route to ≤5 ms (§2.5).
   - *Cost/risk:* pools hold memory. Clones share RNG state and kernel layout, so they need a VMGenID reseed [fc: docs/snapshotting/snapshot-support.md:613-618].
   - *GPU:* not applicable (see 9).

2. **Keep snapshot memory resident and shared; map it eagerly.**
   - *What:* `MAP_PRIVATE` template files so clean pages are shared [fc: snapshot-support.md:78-87]. Keep templates in host RAM, never on disk [Ao22 §3.2]. Pre-map the recorded working set [Ustiugov21 §6.2]. Use huge pages where available [fc: docs/hugepages.md:41-47].
   - *Benefit:* targets the 60–230 ms first-invocation penalty of lazily restored snapshots (§2.5).
   - *Rootless:*
     - Unprivileged userfaultfd defaults to user-mode faults only [linux: Documentation/admin-guide/sysctl/vm.rst:1010-1023]. Such a userfaultfd ignores faults without `FAULT_FLAG_USER` [linux: mm/userfaultfd.c:2722], so faults KVM resolves via get-user-pages should not reach it (inference; E9).
     - `/dev/userfaultfd` access depends only on file permissions [linux: Documentation/admin-guide/mm/userfaultfd.rst:73-80].
     - Default to a file-backed mapping plus prefaulting.
   - *Memory:* eager mapping trades RSS for speed; sharing one template pays the residency once.

3. **Tailor the guest kernel like Lupine, but keep container features.**
   - *What:* built-in drivers, no modules; PARAVIRT on x86; device tree, not ACPI, on arm64; no legacy devices. Then add only what the engine needs (cgroup v2, namespaces, overlayfs, veth/bridge, seccomp).
   - *Benefit:* a kernel phase of ≈17–35 ms instead of ≥127 ms, and ≈16 MB less memory [Kuo20 §4.3; Holmes24 Fig. 11; Li22 §4.3.1].
   - *Risk:* container features were Lupine's first cuts [Kuo20 §3.1.2]; their cost is unmeasured (E5).

4. **No UART console on the boot path.**
   - *What:* `quiet`; virtio-console or a shared-memory log ring; `earlycon` off.
   - *Benefit:* up to 70 ms [Agache20 §5.1]. It matters more on HVF, where every exit goes to user space [hvf: hv_vcpu.h:359-369].

5. **Uncompressed kernel with shared text.**
   - *What:* arm64 `Image`, or vmlinux/PVH on x86 [fc: docs/pvh.md]. Use LZ4 only for measured/confidential boot [Holmes24 §3.3]. A pre-patched image for text sharing [Li22 §4.3.2]; libkrun's x86 kernel mapping is prior art [libkrun: builder.rs:1613-1712].
   - *Benefit:* ≈40 ms, plus guest memory [Agache20 §5.1; fc: rootfs-and-kernel-setup.md:13-16].
   - *Risk:* no KASLR on x86 direct boot (inference).

6. **Minimal boot devices; hybrid transport.**
   - *What:* virtio-mmio via device tree for boot-critical devices. PCI only for GPUs and high-throughput NICs, on a flat bus 0 to bound the 32-slot scan.
   - *Benefit:* one static network interface cost ≈20 ms in Firecracker [Agache20 §5.1]. PCI costs ≥32 trapped config reads per bus (inference) but provides MSI [VIRTIO1.3 §4.2.2; fc: getting-started.md:213-217].
   - *Measure:* E2.

7. **Lean VMM startup.**
   - *What:* one configuration message, no REST round-trips [Agache20 §5.1]. Pre-created VM shells [Manco17 §5.2] with memory allocated and the kernel loaded at configuration time [Agache20 §5.1]. Pooled KVM VM fds, since rootless hosts cannot fix `KVM_CREATE_VM` regressions [fc: prod-host-setup.md:389-455; Wanninger22 §5.2].
   - *Benefit:* closes the ≈50 ms end-to-end vs pre-configured gap [Agache20 Fig. 5] (fig.).

8. **Memory-shrinking defaults.**
   - *What:*
     - `swiotlb=noforce`, with guest RAM placed below the DMA limit [linux: arch/arm64/mm/init.c:339-356];
     - DAX rootfs for shared read-only layers [Li22 §3.1; linux: dax.rst:11-14];
     - only a tiny initramfs, since ramfs pages cannot be reclaimed [linux: ramfs-rootfs-initramfs.rst:27-30];
     - balloon free-page reporting [fc: docs/ballooning.md:46];
     - evaluate 16 KiB arm64 guest pages (4× smaller memmap, derived; E6);
     - not SLUB_TINY [linux: mm/Kconfig:177-186].
   - *Benefit:* up to 64 MiB/VM from SWIOTLB; ≈1.5 MiB per 128 MiB from 16 KiB pages (derived).

9. **GPU VMs are a separate, pre-warmed class.**
   - *Why:*
     - VFIO pins DMA-mapped guest memory under RLIMIT_MEMLOCK [linux: drivers/vfio/vfio_iommu_type1.c:1580-1588], which defeats lazy restore and page sharing.
     - VFIO saves device state only through vendor migration drivers [linux: include/uapi/linux/vfio.h:1015-1042]. v7.2-rc4 ships them for hisi_acc, mlx5, pds, qat, virtio and xe; `nvgrace-gpu` has no migration ops (grep of drivers/vfio/pci/).
     - NVIDIA GPU initialization costs "order of 1-3 second" per GPU [NV-Persist, Overview].
     - HVF has no device-assignment API (§2.4).
   - *Consequence:* only warm pools meet ≤5 ms for GPU VMs.

10. **Cold-boot targets for template builds** (proposals anchored on Lupine 17–23 ms, VMM share ≈3–10 ms, Firecracker spec 125 ms, RunD 88 ms):
    - VMM exec → first guest instruction: ≤5 ms p50.
    - Kernel entry → init: ≤20 ms p50.
    - Exec → init: ≤25 ms p50 and ≤50 ms p99 (1 vCPU, 128 MiB). Hard ceiling: 125 ms p99, measured Firecracker-style.
    - Exec → engine ready: ≤88 ms p50, provisional until E5/E7.
    - HVF targets: set after E1.

11. **Measurement harness.** Reproduce Firecracker's boot-timer semantics. Add phase markers (MMIO writes à la port 0x80 [Holmes24 §6.1], `initcall_debug`, `printk.time`). Report InstanceStart→init, exec→init and exec→engine-ready as p50/p99, wall-clock and CPU.

**Constraint conflicts found**

| Constraints in tension | Evidence |
|---|---|
| ≤5 ms vs minimal memory | Resident, pre-mapped snapshots and pools cost RSS; lazy loading costs 3.7–13 µs per fault [Ao22 §3.3] |
| Rootless vs fast host paths | userfaultfd for kernel-mode faults, hugetlbfs pools, KVM/cgroup tuning and the jailer all need privileges (items 2 and 7) |
| Minimal kernel vs a Docker-compatible in-VM engine | Container features were Lupine's first cuts (item 3) |
| First-class GPUs vs ≤5 ms and minimal memory | Pinned memory, no NVIDIA VFIO migration, 1–3 s GPU init (item 9) |
| PCI's runtime I/O advantage vs its boot-time enumeration cost | Unmeasured (E2) |

## 4. Open questions needing our own measurement

- **E1. HVF baseline (M5 Max, macOS 26.4).** Timestamp `hv_vm_create`, `hv_gic_create`, each `hv_vcpu_create`, `hv_vm_map`, the first `hv_vcpu_run`, a kernel-entry MMIO marker and the init boot-timer write. Sweep RAM 64–1024 MiB × IPA granule 4K/16K × 1–8 vCPUs, 1,000 runs per cell. Report p50/p99 wall-clock and CPU time, and repeat on KVM arm64 and x86 with the same kernel.
- **E2. virtio-mmio vs virtio-pci.** Same kernel with 1–8 devices. Measure `initcall_debug` probe times and VMM exit counts by type, flat bus 0 vs bridges, and runtime IOPS/throughput to price the trade-off.
- **E3. Console cost.** Compare no console, 8250/PL011 at `loglevel=7`, and virtio-console, counting exits. Add a tight MMIO-write microbenchmark after [Dall17 Table 2] on HVF vs KVM.
- **E4. SMP bring-up.** At 1–16 vCPUs, time from boot-CPU start to the end of `smp_init`, comparing arm64 serial PSCI with x86 parallel bring-up. Also time `maxcpus=1` followed by onlining CPUs after restore.
- **E5. Container-capable minimal kernel.** Start from a Lupine-like base and add the engine's option groups one at a time. Per group, measure the boot delta, image size and idle MemTotal−MemAvailable.
- **E6. 4K vs 16K arm64 guest pages.** Measure host RSS after boot and after engine start, memmap size and page-cache fragmentation, crossed with HVF IPA granule 4K/16K under a sparse-touch test.
- **E7. Restore floor.** Use a template with the engine running. Vary memory backing (page cache, shared memory, huge pages) and loading strategy (lazy, `MAP_POPULATE`, recorded working set). Measure fault counts and request → first container exec. On HVF, first establish whether `hv_vm_map` of a file-backed `MAP_PRIVATE` region works, and how it faults.
- **E8. Kernel-text sharing.** Boot 100 VMs from a pre-patched, file-backed kernel and compare shared vs private pages (smaps PSS on Linux; `footprint`/`vmmap` on macOS).
- **E9. Rootless feasibility on stock distros.** Test `/dev/kvm` access, THP madvise, `UFFD_USER_MODE_ONLY` userfaultfd against KVM faults, and VFIO under the default RLIMIT_MEMLOCK. Record each failure mode.
- **E10. Rootfs.** Compare initramfs, pmem-DAX, virtio-fs-DAX and virtio-blk: time to engine ready, and guest plus host memory at 1 and 100 VMs.
- **E11. Delay calibration.** Confirm dmesg reports "Calibrating delay loop (skipped)" on HVF and KVM arm64 guests; if not, measure the cost.

## 5. References

**Peer-reviewed papers.** All were retrieved, and every number above was checked against the retrieved text.

- [Agache20] A. Agache, M. Brooker, A. Florescu, A. Iordache, A. Liguori, R. Neugebauer, P. Piwonka, D.-M. Popa. "Firecracker: Lightweight Virtualization for Serverless Applications." USENIX NSDI 2020. https://www.usenix.org/system/files/nsdi20-paper-agache.pdf
- [Manco17] F. Manco, C. Lupu, F. Schmidt, J. Mendes, S. Kuenzer, S. Sati, K. Yasukata, C. Raiciu, F. Huici. "My VM is Lighter (and Safer) than your Container." ACM SOSP 2017. doi:10.1145/3132747.3132763. Retrieved from https://www.cs.utexas.edu/~witchel/380L/papers/manco17sosp-lightvm.pdf
- [Kuo20] H.-C. Kuo, D. Williams, R. Koller, S. Mohan. "A Linux in Unikernel Clothing." ACM EuroSys 2020. doi:10.1145/3342195.3387526. Retrieved from https://people.cs.vt.edu/djwillia/papers/eurosys20-lupine.pdf
- [Anjali20] Anjali, T. Caraza-Harter, M. M. Swift. "Blending Containers and Virtual Machines: A Study of Firecracker and gVisor." ACM VEE 2020. doi:10.1145/3381052.3381315. Retrieved from https://pages.cs.wisc.edu/~swift/papers/vee20-isolation.pdf
- [Li22] Z. Li, J. Cheng, Q. Chen, E. Guan, Z. Bian, Y. Tao, B. Zha, Q. Wang, W. Han, M. Guo. "RunD: A Lightweight Secure Container Runtime for High-density Deployment and High-concurrency Startup in Serverless Computing." USENIX ATC 2022. https://www.usenix.org/system/files/atc22-li-zijun-rund.pdf
- [Holmes24] B. Holmes, J. Waterman, D. Williams. "SEVeriFast: Minimizing the root of trust for fast startup of SEV microVMs." ACM ASPLOS 2024. doi:10.1145/3620665.3640424. Retrieved from https://people.cs.vt.edu/djwillia/papers/asplos24-severifast.pdf
- [Kuenzer21] S. Kuenzer, V.-A. Bădoiu, H. Lefeuvre, S. Santhanam, A. Jung, G. Gain, C. Soldani, C. Lupu, Ş. Teodorescu, C. Răducanu, C. Banu, L. Mathy, R. Deaconescu, C. Raiciu, F. Huici. "Unikraft: Fast, Specialized Unikernels the Easy Way." ACM EuroSys 2021. doi:10.1145/3447786.3456248. Text read from the arXiv:2104.12721 copy, which carries the EuroSys'21 ACM reference block.
- [Wanninger22] N. C. Wanninger, J. J. Bowden, K. Shetty, A. Garg, K. C. Hale. "Isolating Functions at the Hardware Limit with Virtines." ACM EuroSys 2022. doi:10.1145/3492321.3519553. Retrieved from https://nickw.io/papers/eurosys22.pdf
- [Ustiugov21] D. Ustiugov, P. Petrov, M. Kogias, E. Bugnion, B. Grot. "Benchmarking, Analysis, and Optimization of Serverless Function Snapshots." ACM ASPLOS 2021. doi:10.1145/3445814.3446714. Text read from arXiv:2101.09355v3, which carries the ASPLOS'21 ACM reference block.
- [Ao22] L. Ao, G. Porter, G. M. Voelker. "FaaSnap: FaaS Made Fast Using Snapshot-based VMs." ACM EuroSys 2022. doi:10.1145/3492321.3524270. Retrieved from https://www.sysnet.ucsd.edu/~voelker/pubs/faasnap-eurosys22.pdf
- [Du20] D. Du, T. Yu, Y. Xia, B. Zang, G. Yan, C. Qin, Q. Wu, H. Chen. "Catalyzer: Sub-millisecond Startup for Serverless Computing with Initialization-less Booting." ACM ASPLOS 2020. doi:10.1145/3373376.3378512. Retrieved from https://ipads.se.sjtu.edu.cn/_media/publications/catalyzer-asplos20.pdf
- [Dall17] C. Dall, S.-W. Li, J. Nieh. "Optimizing the Design and Implementation of the Linux ARM Hypervisor." USENIX ATC 2017. https://www.usenix.org/system/files/conference/atc17/atc17-dall.pdf

**Specifications and vendor documentation**

- [VIRTIO1.3] OASIS. *Virtual I/O Device (VIRTIO) Version 1.3*, Committee Specification Draft 01, §4.2. https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html
- [NV-Persist] NVIDIA. *Driver Persistence*, "Overview". https://docs.nvidia.com/deploy/driver-persistence/overview.html (last updated 2026-09-09)
- [hvf: …] Apple. Hypervisor.framework SDK headers, `/Library/Developer/CommandLineTools/SDKs/MacOSX26.4.sdk/System/Library/Frameworks/Hypervisor.framework/Headers/` (`hv_vm.h`, `hv_vcpu.h`, `hv_gic.h`, `hv_vm_config.h`).

**Source code** (paths are relative to each repository)

- [fc: …] Firecracker, `/Users/adalundhe/Projects/firecracker` at commit `edb60617c31e`.
- [linux: …] Linux 7.2-rc4, `/Users/adalundhe/Projects/linux` at commit `1590cf032971`.
- [libkrun: …] libkrun, `/Users/adalundhe/Projects/libkrun` at commit `1f5dd0288fdd`.
- [go-microvm: …] go-microvm, `/Users/adalundhe/Projects/go-microvm` at commit `7e148d855378`.
