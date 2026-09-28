# Millisecond microVM start via snapshot/restore/cloning, and minimal per-VM memory

Status: research notes, 2026-09-28.

**Local source versions**
- Linux 7.2-rc4 @ `1590cf03` (`/Users/adalundhe/Projects/linux`)
- Firecracker @ `edb60617`
- libkrun @ `1f5dd028`
- MacOSX26.4 SDK (Command Line Tools)
- Dev host: Apple M5 Max, macOS 26.4.1, `hw.pagesize` = 16384

**Citation tags**
- `[Paper §x, Fig./Tab. y]` for papers.
- `[linux: path:line]`, `[fc: path:line]` (Firecracker repo), `[libkrun: path:line]`.
- `[HVF: header:line]` for Hypervisor.framework headers.
- `[macOS: page(n)]` for man pages on the dev host; `[man7: page(n)]` for Linux man-pages 6.19.
- `[VIRTIO1.3 §x]` for the VIRTIO 1.3 spec.
- `[meas: Mx]` for **our own in-repo measurements** on this host, in `docs/research/platform-measurements.md`.
  - These come from the harness at `docs/research/measurements/hvf/hvfbench.c`.
  - They are not literature. They are single runs taken while the machine was loaded, as that file itself cautions.

**Markers used in the text**
- *Preprint*: an arXiv paper that has not been peer-reviewed.
- **UNVERIFIED**: no acceptable source could be found.
- *Inference*: our own reading of the cited source, not something the source states.

## 1. Scope

1. Which snapshot/restore/clone mechanisms exist? What start latency, memory sharing and limitations do they report?
2. Lazy vs eager memory restore: how much page faults cost, recording and prefetching the working set, userfaultfd, `MAP_PRIVATE` copy-on-write (CoW), huge pages. Which of these work on macOS/Hypervisor.framework (HVF) and which on Linux/KVM?
3. Sharing memory across VMs and reducing per-VM overhead: CoW snapshots, KSM, content-based page sharing, balloon / free-page reporting, DAX.
4. What breaks when a VM is cloned, and how to fix it: RNG, clocks, network identity, page cache, device state. Also covers what Firecracker documents and implements.
5. How to measure per-VM memory correctly on Linux and on macOS.
6. A ≤5 ms restore-path budget.

## 2. Findings

### 2.1 Q1: mechanism survey

| System | Unit | Mechanism | Reported start latency | Memory | Limits for shards |
|---|---|---|---|---|---|
| Potemkin, SOSP'05 | Xen VM | Clone from a reference image, CoW guest memory | 521.2 ms from ping to reply. Only 11.1 ms of it is memory preparation/copy; 149.2 ms goes to creating devices and 142.8 ms to configuring IP [Vrable05 §5.3.2, Tab. 1] | 116 clones of a 128 MB image used 98 MB in total. A clone dirtied 2.1–3.7 MB per service [§5.3.1, Fig. 6] | Clones on the same host only |
| SnowFlock, EuroSys'09 | Xen VM, across hosts | Ships a 1051±7 KB descriptor, then fetches memory lazily over multicast | 600–800 ms for a 1 GB VM, regardless of the number of clones [LagarCavilla09 §4.2, Fig. 3] | 40 MB transferred for a 1 GB footprint [§7] | Requires guest-kernel heuristics. Rewrites MAC addresses [§4.6.2] |
| SOCK, ATC'18 | Container | Zygote process tree, forked with CoW | Cold start 32 ms; unpausing takes 3 ms [Oakes18 §5.1, Fig. 16]. 20 ms with caching [§5.2, Fig. 17] | CoW sharing "obfuscates" how much memory each instance uses [§4.3] | Process-level isolation only |
| Firecracker, NSDI'20 (cold-boot baseline) | KVM microVM | Normal boot | Under 125 ms to init; p99 of 146 ms with 50 concurrent boots [Agache20 §5.1, Figs. 5–6]. VMM process start: 6–60 ms wall-clock, typically 12 ms [fc: SPECIFICATION.md:13-17] | VMM overhead about 3 MB (`pmap` non-shared memory minus guest size) [§5.2, Fig. 7] | The overhead figure leaves out the guest |
| Catalyzer, ASPLOS'20 | gVisor sandbox | Function image mapped on demand; `sfork` | `sfork` 0.97 ms; Zygote 5–14 ms; full restore about 30 ms more [Du20 §6.2, Fig. 11] | Lower RSS and PSS than gVisor [§6.5, Fig. 14] | All clones share one ASLR layout [§6.8] |
| SEUSS, EuroSys'20 | Unikernel | Stacked snapshots with page-level CoW | Cold 7.5 ms, warm 3.5 ms, hot 0.8 ms [Cadden20 §7, Tabs. 1–2] | 2 MB diff per function on a 114.5 MB base. 54,000 cached instances vs 450 for Firecracker-based Kata [§7, Tabs. 1, 3] | Not a Linux guest |
| REAP, ASPLOS'21 | Firecracker | Record the working set with userfaultfd, then prefetch it in one read | Baseline: VMM restore 50 ms, then 182 ms of processing dominated by page faults, for a function that takes 1 ms when warm [Ustiugov21 §6.2, Fig. 7]. REAP is 3.7× faster on average [§6.3, Fig. 8] | Working set 8–99 MB, i.e. 3–39% of the memory a booted function uses [§4.3, Fig. 4] | Working set changes with input [Ao22 §3.3]. userfaultfd pages are anonymous, so VMs cannot share them [Psomadakis25 §2.1] |
| FaaSnap, EuroSys'22 | Firecracker + guest patch | Load pages in the background while the guest runs; record the loaded set with `mincore`; map zero pages as anonymous memory | Up to 3.5× faster than prior work, and only 3.5% slower than a snapshot held in memory [Ao22 Abstract]. 136 ms vs 480 ms for REAP [§6.4, Tab. 3] | — | Guest kernel must zero pages it frees [§4.5] |
| Fireworks, EuroSys'22 | Firecracker | Snapshot taken after JIT compilation, shared across VMs | 20.6× faster [Shin22 Abstract] | 565 VMs before the host swaps, vs 337 for Firecracker (measured by PSS) [§5.4, Fig. 10] | Each clone needs its own network namespace plus NAT [§3.5] |
| Medes, EuroSys'22 | CRIU container | Deduplicate 64 B chunks against warm sandboxes | Restoring a deduplicated sandbox takes 378–554 ms [Saxena22 §7.8] | Up to 30% memory saved [§2.1, Fig. 2]. Deduplication itself takes 2–3.3 s [§7.7] | ASLR cuts savings from 28.8 to 12.1 MB [§7.2.1] |
| RunD, ATC'22 | Kata-like microVM | Pre-patched template VM; condensed guest kernel | Over 200 starts/s [Li22 Abstract] | Kata+Firecracker overhead is 94 MB per 128 MB container [§3.2, Fig. 5]. RunD cuts this 75.1% and fits over 2,500 sandboxes in 384 GB [§5.3, Fig. 12] | Of the 10,012 KB of kernel code and read-only data touched during boot, 7,928 KB is rewritten, which defeats sharing [§3.2] |
| Groundhog, EuroSys'23 | Process, reset in place | Soft-dirty bits track changes; only dirtied pages are restored | Median latency overhead 1.5% [Alzayat23 Abstract] | Cost grows with the number of dirtied pages plus a scan of the whole address space [§5.2, Fig. 3] | Userfaultfd write-protect mode was slower than soft-dirty bits [§4.3] |
| Nephele, EuroSys'23 | Xen unikernel | — | **UNVERIFIED**: full text could not be retrieved | — | — |
| Mitosis, OSDI'23 | Container | Remote fork over RDMA | Over 10,000 containers forked in about one second [Wei23 Abstract] | — | Needs RDMA and a kernel modification |
| Sabre, OSDI'24 | Firecracker | Compressed working-set prefetch, accelerated by Intel IAA | Memory restore 25–55% faster [Lazarev24 §4.3, Figs. 11–12] | Up to 4.5× compression [Abstract] | Intel IAA hardware only |
| PASS, ATC'24 | MicroVM | Zero-copy paging from persistent memory (PMEM) via DAX | Up to 72% lower execution time than Firecracker on a PMEM filesystem, and up to 47% lower than FaaSnap [Pang24 Abstract] | — | PMEM hardware only |
| TrEnv, SOSP'24 | Container (VM variant discussed) | Reused sandboxes plus "mm-templates" (<1 MB) holding pre-built page tables | Under 10 ms [Huang24 Abstract]. p99 13 ms vs 49 ms for FaaSnap+ and 90 ms for REAP+ [§6.2.2] | 48% less memory [Abstract]. Firecracker baselines used about 2× more memory [§6.2.2, Fig. 11b] | Kernel modification. Every clone gets the same memory layout [§5.1.2] |
| CXLfork, ASPLOS'25 | Process | Remote fork over CXL shared memory | 2.26× faster [Alverti25 Abstract] | 87% less local memory [Abstract] | CXL hardware only |
| SnapBPF, HotStorage'25 | Firecracker | Capture and prefetch the working set in the page cache with eBPF | Matches state of the art [Psomadakis25 Abstract] | Deduplicates working sets through the page cache [Abstract]; userfaultfd-based designs cannot [§2.1] | Needs a new kernel kfunc [§3] |
| Squeezy, EuroSys'26 | VM memory reclaim | Partitions in the guest kernel | Reclaims 2 GiB in 127 ms vs about 2.5 s for virtio-mem [LagkasNikolos26 §6.1.1, Fig. 5] | — | Guest kernel modification |
| Spice, OSDI'26 | Process snapshot | New snapshot format (SHELF) plus a `spliceVMA` primitive | Within 0.6–18 ms of warm latency, vs 3.6–1197 ms for existing systems. 9.5× faster than VM-based systems [Holmes26 Abstract] | — | Kernel modification |
| Aquifer, 2026 (preprint) | Firecracker | Hot pages installed before the VM resumes | — | 82.8% of snapshot pages are zero; 5.4% get dirtied [Hu26 §2.3.3, Fig. 3] | CXL/RDMA hardware |

**Lessons across these systems**
- **The memory mapping is cheap; the control plane is not.**
  - Potemkin spent 11.1 of 521.2 ms on memory [Vrable05 Tab. 1].
  - Network-namespace setup reaches 600 ms under load [Huang24 §6.1].
  - A network interface adds about 20 ms to boot [Agache20 §5.1].
  - Spawning a Firecracker process takes about 12 ms [fc: SPECIFICATION.md:13-17].
- **After restore, page faults dominate.** A 50 ms restore is followed by 182 ms of fault-bound processing, which uses under 5% of SSD bandwidth [Ustiugov21 §6.2].
- **Working sets are small.**
  - 3–39% of a function's booted footprint [Ustiugov21 §4.3].
  - 82.8% of snapshot pages are zero (preprint) [Hu26 §2.3.3].
  - Only 6.4% of container-image data is read at startup [Harter16 Abstract].
- **None of the surveyed papers shows a full Linux microVM ready in ≤5 ms.**
  - Sub-5 ms results come only from process or unikernel systems: `sfork` [Du20 §6.2], SEUSS warm start [Cadden20 §7], container unpause [Oakes18 §5.1].
  - For VMs, the best number is "as little as 4ms" for Firecracker's restore alone (preprint) [Brooker21 §1].
  - Measured VM-based p99 is 49–90 ms [Huang24 §6.2.2].

### 2.2 Q2: lazy vs eager restore, and what each platform offers

**Cost of a page fault.** Measured on KVM/x86 with 4 KiB pages: c5d.metal, NVMe, Linux 5.4, 2 GB guest [Ao22 §3.1]. Figures from [Ao22 §3.3, Fig. 2]:

| Fault type | Average cost |
|---|---|
| Anonymous memory | 2.5 µs |
| Minor fault from the page cache | 3.7 µs |
| Firecracker reading from disk | 13.3 µs (9% of faults take over 32 µs) |
| userfaultfd (REAP) | 6.7 µs; userfaultfd adds "several microseconds" to each fault outside the recorded working set |

- **Fault counts.** One invocation took about 9,000 faults, costing 35 ms even with every page already cached [Ao22 §3.3].
- **Writes are extra.** A copy-on-write write to file-backed memory costs a VM exit plus a page allocation [Holmes26 §2.2].
- **Mapping count matters.**
  - Each VMA costs about 300 B, and creating VMAs dominates restore CPU time [Holmes26 §2.2].
  - FaaSnap had to merge over 1,000 mapping regions [Ao22 §4.6].
- **Gap:** the literature has no numbers for HVF or for 16 KiB pages.
- **Our own measurements on this host (16 KiB IPA granule)** partly fill the gap:
  - First-touch read from a page-cache-hot `MAP_PRIVATE` file: 1.07 µs per 16 KiB page.
  - A CoW write costs 1.9 µs.
  - Pre-reading the file on the host does *not* help (1.37 µs), because the stage-2 fault itself dominates.
  - With a 4 KiB granule: 1.56–2.03 µs per 4 KiB page [meas: M5].
  - Guest-side parallel faulting reaches 453 ns/page with 4 vCPUs and 326 ns/page with 12 [meas: M6].
  - An exit round trip costs 0.7–0.8 µs [meas: M4].

**Mechanisms**
- **Lazy, the Firecracker default.** The snapshot's memory file is mapped `MAP_PRIVATE` and writes go to anonymous CoW pages [fc: docs/snapshotting/snapshot-support.md:78-86; fc: src/vmm/src/vstate/memory.rs:1070-1091]. The file must stay immutable [fc: snapshot-support.md:503-507]. Linux leaves it "unspecified" whether later changes to the file become visible [man7: mmap(2)].
- **Eager.**
  - REAP prefetches the working set in one read [Ustiugov21 §6.3].
  - FaaSnap loads pages in the background, records the loaded set with `mincore`, and maps zero pages as anonymous memory [Ao22 §4.2, §4.4, §4.5].
  - `MADV_POPULATE_READ`/`WRITE` and `MAP_POPULATE` prefault page tables [man7: madvise(2), mmap(2)].
  - `KVM_PRE_FAULT_MEMORY` fills the stage-2 (second-level) page tables but "doesn't break CoW" [linux: Documentation/virt/kvm/api.rst:6473-6531]. Only x86 and s390 select it; arm64 does not [linux: arch/x86/kvm/Kconfig:48; arch/s390/kvm/Kconfig:33]. TrEnv proposes pre-filling EPT entries for hot regions [Huang24 §5.1.3].
- **Huge pages.**
  - THP covers only anonymous and tmpfs/shmem memory [linux: Documentation/admin-guide/mm/transhuge.rst:15].
  - In Firecracker, 2 MiB hugetlbfs pages require the userfaultfd backend, and THP does not work with userfaultfd [fc: docs/hugepages.md:34, 65].
  - Huge pages mean fewer KVM exits to rebuild the EPT after restore [fc: hugepages.md:44]. Dirty-page tracking and the balloon's 4 KiB granularity cancel that benefit [hugepages.md:76-84].

**Availability matrix**

| Capability | Linux/KVM | macOS/HVF (arm64) |
|---|---|---|
| Lazy CoW guest RAM backed by a file | `MAP_PRIVATE` plus the page cache | `MAP_PRIVATE` is copy-on-write [macOS: mmap(2)]. `hv_vm_map` accepts any page-aligned range of "the current process" [HVF: hv_vm.h:45-53]. libkrun maps a `MAP_SHARED` file into the guest for virtio-fs DAX [libkrun: src/devices/src/virtio/fs/macos/passthrough.rs:2741-2770; src/libkrun/src/vmm/macos/vstate.rs:133-151]. Private-file CoW was measured working on this host, and the first write takes a CoW fault. `hv_vm_map` is lazy and costs 0.33–1.13 µs for up to 1 GiB [meas: M3, M5]. Sharing across processes and footprint accounting are still unmeasured (E2) |
| userfaultfd | Trapping *kernel* faults, which is what KVM guest accesses are, requires `CAP_SYS_PTRACE`, `vm.unprivileged_userfaultfd=1` (default 0), or permission on `/dev/userfaultfd` [linux: Documentation/admin-guide/mm/userfaultfd.rst:60-81]. *Inference:* `UFFD_USER_MODE_ONLY` would kill KVM faults with `SIGBUS`, because get_user_pages faults do not carry `FAULT_FLAG_USER` [linux: mm/userfaultfd.c:2696, 2722; mm/gup.c:1087-1121] | Not available: the SDK headers contain no userfaultfd. Substitute: leave guest-physical ranges unmapped and handle the `HV_EXIT_REASON_EXCEPTION` exit, which carries the syndrome, VA and IPA [HVF: hv_vcpu_types.h:42, 77], then call `hv_vm_map` on demand. Cost is unknown (E1) |
| Record the working set | userfaultfd, or `mincore` [Ao22 §4.4] | `mincore` with INCORE / REFERENCED / MODIFIED bits [macOS: mincore(2)] |
| Prefault | `MADV_WILLNEED`, `MADV_POPULATE_*`, `KVM_PRE_FAULT_MEMORY` (x86 only) | `MADV_WILLNEED` [macOS: madvise(2)]. No documented stage-2 prefault. Host-side pre-reading does not reduce fault cost, so prefault from guest context using helper vCPUs [meas: M5, M6] |
| Page granularity | 4 KiB base pages; THP for anonymous memory | 16 KiB host pages. Guest-physical (IPA) granule of 4 KiB or 16 KiB, selectable since macOS 26 [HVF: hv_vm_config.h:111-143]. With the 4 KiB granule, `hv_vm_map` accepts 4 KiB-aligned ranges [meas: M3] |
| Dirty tracking | KVM dirty log [fc: snapshot-support.md:326-336]; soft-dirty bits [Alzayat23 §4.3] | `hv_vm_protect` write-protect, then trap the fault [HVF: hv_vm.h:64-72]. Costs 3.7 µs per dirtied 16 KiB page [meas: M5] |
| Return memory to the host | `MADV_DONTNEED`. On a private file mapping this reverts pages to the file's contents, so Firecracker maps fresh anonymous memory over the range instead [fc: src/vmm/src/vstate/memory.rs:853-861] | `MADV_FREE`, `MADV_FREE_REUSABLE` [macOS SDK: sys/mman.h:215-218] |
| Restore timers | `KVM_ARM_SET_COUNTER_OFFSET` [linux: api.rst:6237-6275]; `KVM_SET_CLOCK` on x86 [api.rst:1090-1121] | `CNTVCT_EL0 = mach_absolute_time() - vtimer_offset` [HVF: hv_vcpu.h:446-449] |
| Interrupt-controller state | Firecracker restores the vGIC, but only onto the same GIC version [fc: snapshot-support.md:129-130] | `hv_gic_state_*` and `hv_gic_set_state` (macOS 15+). A saved blob "can fail" to load after an OS update [HVF: hv_gic_state.h:26-57; hv_gic.h:251-265]. `hv_gic_set_state` costs about 1.2 ms, while rewriting the registers individually takes 19.6 µs [meas: M14] |
| VMs per process | Several | One VM per process [HVF: hv_vm.h:28]; one vCPU per thread [HVF: hv_vcpu.h:20]. `posix_spawn` to the child's `main` takes 3.70 ms p50; loading Hypervisor.framework accounts for about 1.5 ms of that [meas: M11, M11b] |
| Copy-on-write clone of memory | `fork` | `fork`/`minherit` [macOS: minherit(2)], `mach_vm_remap(copy)` [SDK: mach/mach_vm.h:270], `MAP_MEM_VM_COPY` [SDK: mach/memory_object_types.h:291]. HVF VM state is **not** inherited by a child |

### 2.3 Q3: sharing memory across VMs and reducing overhead

**Snapshot CoW sharing**
- Firecracker's design "allows sharing of memory pages" across VMs restored from the same snapshot [fc: snapshot-support.md:78-86].
- Sharing works only through the page cache. userfaultfd fills pages as anonymous memory, which cannot be deduplicated [Psomadakis25 §2.1].
- Fireworks ran 565 vs 337 VMs [Shin22 §5.4].
- *Inference:* a snapshot taken after boot already holds the kernel text the guest patched at boot. RunD found that boot-time patching blocks sharing [Li22 §3.2]; restoring from such a snapshot sidesteps that.

**KSM**
- Merges only anonymous pages [linux: Documentation/admin-guide/mm/ksm.rst:25].
- The application must opt in with `MADV_MERGEABLE` [ksm.rst:31-37].
- The sysfs switches are root-only, and it is off by default (`run`=0) [ksm.rst:76-77, 111-118]. It is therefore not usable rootless.
- Default scan rate is 100 pages every 20 ms [ksm.rst:79-93].

**Content-based page sharing**
- VMware ESX: approached 67% sharing across identical VMs [Waldspurger02 §4, Fig. 4]. Production deployments reclaimed 33% (673 MB) and 18.7% (345 MB) of memory at negligible CPU cost [§4, Fig. 5].
- Difference Engine: up to 90% saved for similar VMs and 65% for disparate ones, with under 7% overhead [Gupta08 Abstract].
- Satori: a memory scanner has to be rate-capped. ESX's default rate means a duplicate is found after about 40 minutes on average, so short-lived sharing is missed. Satori instead shares pages at block-device reads [Milos09 §4.1], reaching up to 94% of the achievable sharing [Abstract].
- Short-lived agent VMs therefore need sharing by construction, not by scanning.

**Medes.** Saves up to 30% [Saxena22 §2.1], but restores take 378–554 ms [§7.8]. It works as a memory tier, not as a fast-start path.

**Balloon, free-page reporting and hinting**
- VIRTIO spec: free-page reporting has no deflate queue. Reported pages are reusable once the device acknowledges them, and are "uninitialized" unless `PAGE_POISON` is negotiated [VIRTIO1.3 §5.5.6.7–5.5.6.7.1]. Free-page hinting targets migration [§5.5.6.5].
- Linux starts reporting 2 s after registration [linux: Documentation/mm/free_page_reporting.rst:19-21].
- Firecracker:
  - Frees reported ranges with `MADV_DONTNEED` [fc: docs/ballooning.md:307-314].
  - Recommends matching `page_reporting_order` to the host backing page size [ballooning.md:338-347].
  - A hinting run takes about 200 ms per GB, and has a race condition [ballooning.md:380-390, 448-466].
- Squeezy:
  - The classic balloon spends 81% of its time on VM exits.
  - virtio-mem needs 617 ms to reclaim 512 MiB, and about 2.5 s for 2 GiB.
  - Squeezy reclaims 2 GiB in 127 ms [LagkasNikolos26 §6.1.1].

**DAX**
- virtio-pmem keeps the page cache "only in the host" [VIRTIO1.3 §5.19].
- The virtio-fs DAX window maps file ranges directly into the guest [VIRTIO1.3 §5.11.6.4].
- In Firecracker, a 128 MB VM booted from pmem has an RSS of about 120 MB without DAX and about 96 MB with it. A backing file shared by several VMs is counted once [fc: docs/pmem.md:294-307].

**Side channels.** Sharing physical pages across VMs "could be exploited as a side channel" [fc: pmem.md:152-158; VIRTIO1.3 §5.19.8]. TrEnv limits sharing to functions of the same user [Huang24 §5.1.2].

**Where per-VM overhead actually sits**
- Firecracker's "5MB" covers only the VMM process. The guest adds much more: 94 MB for a 128 MB Kata+Firecracker container [Li22 §3.2].
- Condensing the kernel config saved about 16 MB [Li22 §4.3.1].
- VMs used about 2× the memory of containers, because each guest keeps its own page cache [Huang24 §6.2.2].

### 2.4 Q4: clone correctness hazards and fixes

**Firecracker's position.** Resuming the same snapshot more than once is "insecure" unless the VMM restores the uniqueness of identifiers, seeds, the entropy pool and tokens [fc: snapshot-support.md:548-556].

**RNG**
- *Firecracker:* it always attaches a VMGenID device. On resume it writes a new ID and injects an interrupt before the vCPUs run [fc: snapshot-support.md:604-628].
- *Linux guest:* the driver (ACPI `VMGENCTR` or devicetree `microsoft,vmgenid` [linux: drivers/virt/vmgenid.c:156, 162]) calls `add_vmfork_randomness`, which forces a CRNG reseed [vmgenid.c:26-36; linux: drivers/char/random.c:974-985].
- *Race:* there is a window between vCPU resume and the reseed [fc: docs/snapshotting/random-for-clones.md:127-130]. Snapshots taken during early boot can crash when the notification arrives [fc: snapshot-support.md:688-696].
- *Userspace PRNGs:* there is "no generic solution" [random-for-clones.md:7-12]. The proposed `MADV_WIPEONSUSPEND` and SysGenId interfaces [Brooker21 §3.2–3.4] are **not in Linux 7.2**; only `MADV_WIPEONFORK` exists [linux: include/uapi/asm-generic/mman-common.h:69].
- *VMClock:* it exposes a `vm_generation_counter` [linux: include/uapi/linux/vmclock-abi.h:118-123, 192-199] that userspace can mmap or poll [linux: drivers/ptp/ptp_vmclock.c:367, 432-449]. Per Firecracker, the poll support merged in Linux 7.0 [fc: snapshot-support.md:659-666].
- *In-kernel consumers* can register a vmfork notifier, as WireGuard does [linux: drivers/net/wireguard/device.c:445].

**Kernel secrets derived once (inference)**
- The TCP ISN and port-selection secret `net_secret` is generated once, via `net_get_random_once` [linux: net/core/secure_seq.c:22-28].
- No code under `net/` registers a vmfork notifier, so every clone shares that secret.
- KASLR and userspace ASLR layouts are likewise shared across clones [Ustiugov21 §7.1; Du20 §6.8; Huang24 §5.1.2]. Mitigation: re-snapshot periodically [Ustiugov21 §7.1].

**Identifiers.** Clones share `boot_id` and the systemd random seed [random-for-clones.md:146-155]. Duplicate UUIDs and nonces break correctness, and reused IVs break crypto [Brooker21 §2].

**Clocks**
- After restore, the guest wall clock resumes from the time the snapshot was taken [fc: snapshot-support.md:522-526].
- x86 can use `KVM_CLOCK_REALTIME` [fc: snapshot-support.md:527-531; linux: api.rst:1105-1121]. Firecracker rejects that option on aarch64 [fc: src/vmm/src/builder.rs:477-479].
- arm64 KVM offers the ptp_kvm hypercall `0x86000001` to sync wall time [linux: Documentation/virt/kvm/arm/ptp_kvm.rst].
- On HVF, guest `HVC` calls trap to userspace (libkrun handles PSCI this way [libkrun: src/hvf/src/lib.rs:100, 629]). *Inference:* our VMM could emulate the ptp_kvm call there.
- VMGenID's stated purpose is "time shift events" [MS-VMGenID].

**Network**
- Clones reuse the same TAP name and IP address. Firecracker's fix (a network namespace plus NAT per clone) needs root [fc: docs/snapshotting/network-for-clones.md:13-20, 41-60].
- The ARP cache should be flushed [network-for-clones.md:134-144].
- TCP connections are not preserved, and vsock is reset [fc: snapshot-support.md:56-61, 674-686].
- Other approaches: rewriting MACs [LagarCavilla09 §4.6.2]; netns plus NAT [Shin22 §3.5].
- Cloning breaks TCP/TLS session identity [Brooker21 §1.1].

**Page cache and disks.** Disk backing files must be present at the same paths. Firecracker drains block I/O and `fsync`s at snapshot time [fc: snapshot-support.md:286-291, 487-492]. The guest page cache inside the snapshot must match those files exactly.

**Device and host state**
- A snapshot restores only onto identical hardware and software [fc: snapshot-support.md:698-714].
- The TSX MSR is lost without a CPU template [snapshot-support.md:131-133].
- HVF GIC blobs are OS-update sensitive (see §2.2).
- Firecracker restores devices last, so the VMGenID interrupt is not overwritten [fc: src/vmm/src/builder.rs:427-520].

**GPU**
- VFIO type1 assumes guest pages are "pinned into memory" [linux: drivers/vfio/vfio_iommu_type1.c:18-19]. *Inference:* this rules out lazy restore and sharing for GPU VMs.
- VFIO device migration is an optional feature (`STOP_COPY`) [linux: include/uapi/linux/vfio.h:1014-1042].
- CUDA checkpoint copies GPU memory to the host, and restore needs a GPU "of the same chip type" [NVIDIA-CUDA13.4 §6.1].
- The HVF headers expose no IOMMU or device-assignment API (checked with grep).

### 2.5 Q5: measuring per-VM memory

**Linux**
- **RSS vs PSS.** RSS counts shared pages in full. PSS divides each shared page by the number of sharers [linux: Documentation/filesystems/proc.rst:500-505].
- **"Private".** A page counts as private when it is mapped exactly once [proc.rst:507-510]. USS is `Private_Clean + Private_Dirty`.
- **`smaps_rollup`.** Cheaper to read, and adds `Pss_Anon`, `Pss_File` and `Pss_Shmem` [proc.rst:153, 639-650].
- **Kernel-side memory.** cgroup v2 `memory.current` and `memory.stat`; `sec_pagetables` includes KVM's MMU pages [linux: Documentation/admin-guide/cgroup-v2.rst:1297-1301, 1558-1563]. `SecPageTables` in `/proc/meminfo` gives the system-wide figure [proc.rst:1208-1210].

**How prior work measured**
- Firecracker's CI takes RSS minus the guest region, with thresholds of 5 MiB (booted), 7 MiB (while snapshotting) and 5 MiB (after restore) [fc: tests/host_tools/memory.py:26-61].
- Firecracker's NSDI paper used `pmap` non-shared memory [Agache20 §5.2].
- Fireworks and Catalyzer used PSS [Shin22 §5.4; Du20 §6.5].

**macOS**
- `task_vm_info.phys_footprint` [macOS SDK: mach/task_info.h:346-376], or `ri_phys_footprint` via `proc_pid_rusage` [sys/resource.h:211; libproc.h:111].
- `footprint(1)` de-duplicates memory shared between processes [macOS: footprint(1)].
- `hv_vm_allocate` "enables accurate memory accounting" [HVF: hv_vm_allocate.h:37-51].
- HVF's stage-2 page tables and other kernel memory are not exposed. They can only be seen as host-level deltas (E7).

**Report four numbers:** marginal PSS or footprint, private-dirty memory, secondary page tables, and shared memory divided by the number of sharers.

### 2.6 Q6: ≤5 ms restore-path budget

"Ready" means the in-VM engine has acknowledged over vsock, after its identity and clock were refreshed. Budgets below are hypotheses to test.

| # | Step | Critical path? | Anchor | Budget |
|---|---|---|---|---|
| 0 | Spawn the VMM process, create the VM and vCPUs | No: keep a pre-spawned pool per VM shape. HVF needs one process per VM | Firecracker spawn is typically 12 ms [fc: SPECIFICATION.md:13-17]. On HVF, `posix_spawn` takes 3.70 ms p50 [meas: M11] | 0 |
| 1 | Dispatch the request to a pooled VMM | Yes | — | ≤0.2 ms (E6) |
| 2 | Map the snapshot file `MAP_PRIVATE` and register it with KVM or `hv_vm_map`, as a few large regions | Yes | Cost depends on the number of regions [Ao22 §4.6; Holmes26 §2.2]. On HVF, `hv_vm_map` takes 0.33–1.13 µs for up to 1 GiB, and `hv_vm_create` 10.8 µs in a warm process [meas: M2, M3] | ≤0.3 ms (KVM: E4) |
| 3 | Restore vCPU, GIC and timer state | Yes | Firecracker's ordering [fc: builder.rs:427-520]; whole restore "as little as 4ms" (preprint) [Brooker21 §1]. On HVF, vCPU state load is 0.54 µs; GIC restore is 19.6 µs register by register vs about 1.2 ms via `hv_gic_set_state` [meas: M14] | ≤0.1 ms on HVF if the GIC blob is avoided |
| 4 | Restore devices; publish new VMGenID and VMClock values; attach the user-mode network stack | Yes. No host kernel network objects are created here | [Vrable05; Huang24 §6.1; Agache20 §5.1] | ≤0.5 ms |
| 5 | Install the recorded "ready-path" pages | Yes | x86: 3.7 µs per 4 KiB minor fault [Ao22 §3.3]. HVF: 1.07 µs per 16 KiB page on one vCPU, 0.45 µs with 4 helper vCPUs [meas: M5, M6]. So 1.5 ms covers about 1,400 16 KiB pages (22 MiB) single-threaded, or about 3,300 (52 MiB) with 4 vCPUs | ≤1.5 ms (E4, E5) |
| 6 | Resume; guest reseeds, sets the clock, flushes ARP, acknowledges | Yes | REAP's connection restore still took 4–7 ms [Ustiugov21 §6.3] | ≤1.5 ms (E5, E8) |
| 7 | Everything else: background paging, lazy faults, zero pages as anonymous memory | No | [Ao22 §4.2, §4.5] | — |

**Feasibility**
- On HVF, our own measurements put steps 2–3 at microseconds, provided the GIC blob is not used [meas: M3, M14].
- The deciding factors are therefore the ready-path working set (E5), the guest's identity refresh (E8), and a pool that removes the 3.7 ms spawn [meas: M11].
- For scale: REAP's *whole-invocation* working sets averaged 24 MB [Ustiugov21 §4.3]. That is close to what 1.5 ms of parallel faulting can install on HVF, so the ready-path subset has to be smaller than that.
- KVM needs the same measurements (E4).
- Fallback that fits the evidence: keep a pool of VMs that are already restored and paused, and start one by resuming it plus refreshing identity and clock.
  - Unpausing a container takes 3 ms [Oakes18 §5.1].
  - Each pooled VM costs its VMM process (≤5 MiB for Firecracker [fc: SPECIFICATION.md:24-35]) plus its page tables; guest memory is shared.

## 3. Implications for shards (ranked)

1. **Default to file-backed CoW snapshots served from the page cache, restored into pre-spawned VMMs. Add a pool of restored, paused VMs if p99 exceeds 5 ms.**
   - Evidence: [fc: snapshot-support.md:78-86; Psomadakis25 §2.1; fc: SPECIFICATION.md:13-17; Oakes18 §5.1; meas: M3, M5, M11].
   - Rootless: yes.
   - Benefit: clean guest pages are shared per image, and process spawn (3.7 ms on HVF) moves off the critical path.
   - Risk: sharing across processes on HVF is unmeasured (E2); pool memory is unmeasured (E7); HVF needs one process per VM [HVF: hv_vm.h:28].
   - Restore the GIC register by register, not through `hv_gic_set_state` (19.6 µs vs about 1.2 ms), which also avoids blobs that break across OS updates [meas: M14; HVF: hv_gic.h:251-265].
2. **Install the ready-path pages eagerly and stream the rest in the background.**
   - Record the ready path with `mincore`.
   - Prefault with `MADV_POPULATE_READ` on Linux and `KVM_PRE_FAULT_MEMORY` on x86. On HVF, have helper vCPUs touch the pages inside the guest, because host reads do not help there [meas: M5, M6].
   - Keep the 16 KiB IPA granule [meas: M5]. Map zero regions as anonymous memory. Keep the number of mapping regions small.
   - Evidence: [Ao22 §3.3, §4.2–4.6; man7: madvise(2); linux: api.rst:6473-6531; Holmes26 §2.2].
   - Risk: the working set drifts with input [Ao22 §3.3].
3. **Build the uniqueness protocol into the VMM, the guest kernel and the in-VM engine.**
   - VMGenID (devicetree on arm64) plus the VMClock generation counter. The engine re-keys its own IDs and RNGs before it reports ready.
   - Snapshot only after boot has finished. Scrub `boot_id` and the systemd seed. Re-key `net_secret` in a vmfork notifier (guest patch), or take the snapshot before `net_secret` is first used.
   - Snapshots should be per tenant and rotated periodically, because KASLR stays shared.
   - Evidence: [fc: snapshot-support.md:604-666, 688-696; random-for-clones.md:127-155; linux: vmgenid.c:26-36; secure_seq.c:22-28; Ustiugov21 §7.1].
4. **Keep guest counters continuous, and push wall-clock time right after resume.**
   - Use the counter or vtimer offset, plus VMClock or an emulated `ptp_kvm` call.
   - Evidence: [HVF: hv_vcpu.h:446-449; linux: api.rst:6237-6275; ptp_kvm.rst; libkrun: src/hvf/src/lib.rs:629].
   - Firecracker offers no realtime clock adjustment on arm64 [fc: builder.rs:477-479].
5. **Run networking in user mode inside the VMM, with no TAP devices or network namespaces.**
   - Identical guest IP and MAC addresses then become harmless.
   - It also avoids root-only namespace setup, which reached 600 ms under load [Huang24 §6.1], and the roughly 20 ms that a NIC adds at boot [Agache20 §5.1].
   - Flush ARP and reset vsock on restore.
   - Evidence: [fc: network-for-clones.md:13-60, 134-144; Huang24 §6.1; Agache20 §5.1].
6. **Share by construction, never by scanning.**
   - Serve OCI layers via virtio-pmem or virtio-fs DAX, and snapshot pages via the page cache, only within one trust domain. Do not use KSM.
   - Evidence: [fc: pmem.md:152-158, 294-307; VIRTIO1.3 §5.11.6.4, §5.19.8; ksm.rst:25, 76-77; Milos09 §4.1; Huang24 §5.1.2].
7. **Reclaim memory with free-page reporting, with `page_reporting_order` matched to the 16 KiB host page.**
   - Release private-file pages by mapping anonymous memory over them.
   - The traditional balloon serves only as a cap. Use virtio-mem-style partitioning to grow density.
   - Evidence: [VIRTIO1.3 §5.5.6.7; fc: ballooning.md:338-347; fc: memory.rs:853-861; LagkasNikolos26 §6.1.1].
8. **Keep the guest small.**
   - Condense the kernel config and avoid loadable modules. Snapshot after boot-time patching, so the patched text is shared.
   - Evidence: [Li22 §3.2, §4.3.1; Agache20 §5.1].
9. **Treat GPU VMs as a separate class.**
   - Guest memory is pinned; there is no lazy restore and no sharing; keep a warm pool. This is inferred from VFIO pinning (E11).
   - Checkpoint CUDA state to the host only for restore onto identical GPUs. There is no GPU path on HVF.
   - Evidence: [linux: vfio_iommu_type1.c:18-19; vfio.h:1014-1042; NVIDIA-CUDA13.4 §6.1].
10. **Use privileged accelerators only where the host already allows them.**
    - userfaultfd needs the sysctl or `/dev/userfaultfd` permission; hugetlbfs needs a reserved pool; shmem THP can be requested via `MADV_COLLAPSE`.
    - Evidence: [linux: userfaultfd.rst:60-81; fc: hugepages.md:34-84; transhuge.rst:421-460].

## 4. Open questions needing our own measurement

- **E1 – HVF primitive costs.**
  - Mostly answered on this host [meas: M2–M4, M14]. Re-run on a quiet machine before quoting final numbers.
  - Still open: the cost of lazy mapping driven by exits (an unmapped-IPA exit followed by `hv_vm_map` of one 16 KiB page), and how it scales with the number of mapped regions (10–10⁴).
- **E2 – HVF with a private file mapping.**
  - Fault cost is measured [meas: M5]. Still assert in CI that guest writes never reach the snapshot file.
  - Still open: memory sharing. For 2–64 clones mapping the same file, compare the sum of `phys_footprint` against `footprint -a` against host `vm_stat` deltas, both idle and after a write-heavy task.
- **E3 – Granule.**
  - IPA granule cost is measured [meas: M3, M5].
  - Still open: how guest page size (4 KiB, 16 KiB, 64 KiB kernels) changes fault count and ready-path time on the 16 KiB IPA granule.
- **E4 – KVM on arm64 and x86.** Reproduce the fault classes from [Ao22 §3.3]. Compare `MADV_POPULATE_READ`, `MAP_POPULATE` and `KVM_PRE_FAULT_MEMORY` against lazy faulting, tracing `kvm_mmu_page_fault` with bpftrace.
- **E5 – Ready-path working set.** Diff `mincore` output from restore until the engine acknowledges and the compose project answers. Repeat with varied inputs to measure drift.
- **E6 – Pooling.**
  - Spawn cost is measured [meas: M11].
  - Still open: a fork-based zygote VMM (CoW RAM plus a new `hv_vm_create`/`KVM_CREATE_VM`) vs a pool of restored, paused VMs. Report end-to-end p50/p99 at 1–256 concurrent starts, and the idle memory of each pooled VM.
- **E7 – Marginal memory at N = 1…1000.**
  - Linux: PSS, `memory.current` and `sec_pagetables` per cgroup.
  - macOS: `vm_stat` deltas against summed `phys_footprint`.
  - Measure both idle and after one agent task.
- **E8 – Uniqueness.** Restore 1,000 clones. `getrandom` output, TCP ISNs to a fixed peer, `boot_id`, engine-generated IDs and TLS client randoms must all differ. Also measure the delay from notification to reseed.
- **E9 – Clocks.** Restore a snapshot taken 1 h earlier. Check `CLOCK_MONOTONIC` continuity, timer storms, wall-clock error at "ready", and watchdog warnings.
- **E10 – userfaultfd without root.** Confirm the inferred `SIGBUS` for `UFFD_USER_MODE_ONLY` on KVM memory. Survey the defaults for `vm.unprivileged_userfaultfd` and `/dev/userfaultfd` on target distros.
- **E11 – GPU.** Time CUDA checkpoint/restore for a VM with a passed-through GPU, and measure how much resident memory pinning adds.

## 5. References

**Papers.** All of these were retrieved in full and read, except Lupu23.
- [Agache20] A. Agache, M. Brooker, A. Florescu, A. Iordache, A. Liguori, R. Neugebauer, P. Piwonka, D.-M. Popa. Firecracker: Lightweight Virtualization for Serverless Applications. NSDI 2020. https://www.usenix.org/system/files/nsdi20-paper-agache.pdf
- [Alverti25] C. Alverti, S. Psomadakis, B. Ocalan, S. Jaiswal, T. Xu, J. Torrellas. CXLfork: Fast Remote Fork over CXL Fabrics. ASPLOS 2025. doi:10.1145/3676641.3715988 (https://iacoma.cs.uiuc.edu/iacoma-papers/asplos25_1.pdf)
- [Alzayat23] M. Alzayat, J. Mace, P. Druschel, D. Garg. Groundhog: Efficient Request Isolation in FaaS. EuroSys 2023. doi:10.1145/3552326.3567503 (https://people.mpi-sws.org/~dg/papers/eurosys2023-groundhog.pdf)
- [Ao22] L. Ao, G. Porter, G. M. Voelker. FaaSnap: FaaS Made Fast Using Snapshot-based VMs. EuroSys 2022. doi:10.1145/3492321.3524270 (https://www.sysnet.ucsd.edu/~voelker/pubs/faasnap-eurosys22.pdf)
- [Brooker21] M. Brooker, A. C. Catangiu, M. Danilov, A. Graf, C. MacCarthaigh, A. Sandu. Restoring Uniqueness in MicroVM Snapshots. arXiv:2102.12892, 2021. **Preprint (not peer-reviewed).**
- [Brooker23] M. Brooker, M. Danilov, C. Greenwood, P. Piwonka. On-demand Container Loading in AWS Lambda. USENIX ATC 2023. https://www.usenix.org/system/files/atc23-brooker.pdf. Retrieved, but not cited above: it is superseded by Harter16, the primary source for the 6.4% figure that Brooker23 §1 quotes.
- [Cadden20] J. Cadden, T. Unger, Y. Awad, H. Dong, O. Krieger, J. Appavoo. SEUSS: Skip Redundant Paths to Make Serverless Fast. EuroSys 2020. doi:10.1145/3342195.3392698 (https://www.cs.bu.edu/~jappavoo/Resources/Papers/seuss.pdf)
- [Du20] D. Du, T. Yu, Y. Xia, B. Zang, G. Yan, C. Qin, Q. Wu, H. Chen. Catalyzer: Sub-millisecond Startup for Serverless Computing with Initialization-less Booting. ASPLOS 2020. doi:10.1145/3373376.3378512
- [Gupta08] D. Gupta, S. Lee, M. Vrable, S. Savage, A. C. Snoeren, G. Varghese, G. M. Voelker, A. Vahdat. Difference Engine: Harnessing Memory Redundancy in Virtual Machines. OSDI 2008. https://www.usenix.org/legacy/event/osdi08/tech/full_papers/gupta/gupta.pdf
- [Harter16] T. Harter, B. Salmon, R. Liu, A. C. Arpaci-Dusseau, R. H. Arpaci-Dusseau. Slacker: Fast Distribution with Lazy Docker Containers. FAST 2016. https://www.usenix.org/system/files/conference/fast16/fast16-papers-harter.pdf
- [Holmes26] B. Holmes, B. Dinis, L. Honcharuk, A. Belay, J. Fried. Rethinking Process Snapshots for Near-Warm Serverless Cold Starts. OSDI 2026. https://www.usenix.org/system/files/osdi26-holmes.pdf
- [Hu26] J. Hu, H. Li, M.-C. Yang. Aquifer: Hierarchical Memory Pooling with CXL and RDMA for MicroVM Snapshots. arXiv:2606.24079, 2026. **Preprint (not peer-reviewed).**
- [Huang24] J. Huang, M. Zhang, T. Ma, Z. Liu, S. Lin, K. Chen, J. Jiang, X. Liao, Y. Shan, N. Zhang, M. Lu, T. Ma, H. Gong, Y. Wu. TrEnv: Transparently Share Serverless Execution Environments Across Different Functions and Nodes. SOSP 2024. doi:10.1145/3694715.3695967
- [LagarCavilla09] H. A. Lagar-Cavilla, J. A. Whitney, A. Scannell, P. Patchin, S. M. Rumble, E. de Lara, M. Brudno, M. Satyanarayanan. SnowFlock: Rapid Virtual Machine Cloning for Cloud Computing. EuroSys 2009. doi:10.1145/1519065.1519067 (http://www.cs.toronto.edu/~brudno/public/pdf/lagar2009snowflock.pdf)
- [LagkasNikolos26] O. Lagkas Nikolos, C. Alverti, S. Psomadakis, G. Goumas, N. Koziris. Squeezy: Rapid VM Memory Reclamation for Serverless Functions. EuroSys 2026. doi:10.1145/3767295.3769357. Read as arXiv:2411.12893v2 (2025-12-06), which carries the EuroSys'26 reference.
- [Lazarev24] N. Lazarev, V. Gohil, J. Tsai, A. Anderson, B. Chitlur, Z. Zhang, C. Delimitrou. Sabre: Hardware-Accelerated Snapshot Compression for Serverless MicroVMs. OSDI 2024. https://www.usenix.org/system/files/osdi24-lazarev_1.pdf
- [Li22] Z. Li, J. Cheng, Q. Chen, E. Guan, Z. Bian, Y. Tao, B. Zha, Q. Wang, W. Han, M. Guo. RunD: A Lightweight Secure Container Runtime for High-density Deployment and High-concurrency Startup in Serverless Computing. USENIX ATC 2022. https://www.usenix.org/system/files/atc22-li-zijun-rund.pdf
- [Lupu23] C. Lupu et al. Nephele: Extending Virtualization Environments for Cloning Unikernel-based VMs. EuroSys 2023. doi:10.1145/3552326.3587454. **Not retrieved; nothing in this note depends on it.**
- [Milos09] G. Miłoś, D. G. Murray, S. Hand, M. A. Fetterman. Satori: Enlightened Page Sharing. USENIX ATC 2009. https://www.usenix.org/legacy/event/usenix09/tech/full_papers/milos/milos.pdf
- [Oakes18] E. Oakes, L. Yang, D. Zhou, K. Houck, T. Harter, A. C. Arpaci-Dusseau, R. H. Arpaci-Dusseau. SOCK: Rapid Task Provisioning with Serverless-Optimized Containers. USENIX ATC 2018. https://www.usenix.org/system/files/conference/atc18/atc18-oakes.pdf
- [Pang24] X. Pang, Y. Zhang, L. Liu, D. Cheng, C. Xu, X. Zhou. Expeditious High-Concurrency MicroVM SnapStart in Persistent Memory with an Augmented Hypervisor. USENIX ATC 2024. https://www.usenix.org/system/files/atc24-pang.pdf
- [Psomadakis25] S. Psomadakis, D. Siakavaras, C. Alverti, S. Porgiotis, O. Lagkas Nikolos, C. Katsakioris, K. Nikas, G. I. Goumas, N. Koziris. SnapBPF: Exploiting eBPF for Serverless Snapshot Prefetching. HotStorage 2025 (peer-reviewed workshop; read as the author preprint). doi:10.1145/3736548.3737823
- [Saxena22] D. Saxena, T. Ji, A. Singhvi, J. Khalid, A. Akella. Memory Deduplication for Serverless Computing with Medes. EuroSys 2022. doi:10.1145/3492321.3524272 (https://divyanshusaxena.github.io/assets/pdf/medes.pdf)
- [Shin22] W. Shin, W.-H. Kim, C. Min. Fireworks: A Fast, Efficient, and Safe Serverless Framework using VM-level post-JIT Snapshot. EuroSys 2022. doi:10.1145/3492321.3519581 (https://multics69.github.io/pages/pubs/fireworks-shin-eurosys22.pdf)
- [Ustiugov21] D. Ustiugov, P. Petrov, M. Kogias, E. Bugnion, B. Grot. Benchmarking, Analysis, and Optimization of Serverless Function Snapshots. ASPLOS 2021. doi:10.1145/3445814.3446714 (https://marioskogias.github.io/docs/reap.pdf)
- [Vrable05] M. Vrable, J. Ma, J. Chen, D. Moore, E. Vandekieft, A. C. Snoeren, G. M. Voelker, S. Savage. Scalability, Fidelity, and Containment in the Potemkin Virtual Honeyfarm. SOSP 2005. doi:10.1145/1095810.1095825 (https://cseweb.ucsd.edu/~savage/papers/Sosp05.pdf)
- [Waldspurger02] C. A. Waldspurger. Memory Resource Management in VMware ESX Server. OSDI 2002. https://www.usenix.org/legacy/event/osdi02/tech/waldspurger/waldspurger.pdf
- [Wei23] X. Wei, F. Lu, T. Wang, J. Gu, Y. Yang, R. Chen, H. Chen. No Provisioned Concurrency: Fast RDMA-codesigned Remote Fork for Serverless Computing. OSDI 2023. https://www.usenix.org/system/files/osdi23-wei-rdma.pdf

**Specifications and vendor documentation**
- [VIRTIO1.3] OASIS. Virtual I/O Device (VIRTIO) Version 1.3, CSD01. Sections used: §5.5 balloon, §5.11.6.4 virtio-fs DAX window, §5.19 pmem. https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html
- [MS-VMGenID] Microsoft. Virtual machine generation identifier (updated 2025-03-12). https://learn.microsoft.com/en-us/windows/win32/hyperv_v2/virtual-machine-generation-identifier
- [NVIDIA-CUDA13.4] NVIDIA. CUDA Driver API Reference Manual 13.4, §6.1 CUDA Checkpointing. https://docs.nvidia.com/cuda/cuda-driver-api/cuda_driver_api/group__CUDA__CHECKPOINT.html
- [man7] Linux man-pages 6.19: mmap(2), madvise(2). https://man7.org/linux/man-pages/man2/
- **Apple SDK headers**, under `/Library/Developer/CommandLineTools/SDKs/MacOSX26.4.sdk`:
  - `System/Library/Frameworks/Hypervisor.framework/Headers/{hv_vm.h, hv_vm_allocate.h, hv_vm_config.h, hv_vcpu.h, hv_vcpu_types.h, hv_gic.h, hv_gic_state.h}`
  - `usr/include/{sys/mman.h, sys/resource.h, libproc.h, mach/task_info.h, mach/mach_vm.h, mach/memory_object_types.h}`
- **macOS man pages** (macOS 26.4.1): mmap(2), madvise(2), mincore(2), minherit(2), footprint(1).

**Own measurements (not literature)**
- [meas] `docs/research/platform-measurements.md`, sections M2–M6, M11/M11b and M14.
- Harness: `docs/research/measurements/hvf/hvfbench.c`.
- Measured on an Apple M5 Max, macOS 26.4.1, 2026-09-28. Single runs on a loaded machine, per that file's own caveat.

**Source trees.** Line numbers are given inline.
- Linux 7.2-rc4 @ `1590cf0329716306e948a8fc29f1d3ee87d3989f`
  - Documentation: `Documentation/{admin-guide/mm/{userfaultfd,ksm,transhuge}.rst, virt/kvm/api.rst, virt/kvm/arm/ptp_kvm.rst, filesystems/proc.rst, admin-guide/cgroup-v2.rst, mm/free_page_reporting.rst}`
  - Source: `mm/{userfaultfd,gup}.c`, `drivers/{virt/vmgenid.c, char/random.c, ptp/ptp_vmclock.c, net/wireguard/device.c, vfio/vfio_iommu_type1.c}`, `include/uapi/linux/{vmclock-abi.h, vfio.h}`, `include/uapi/asm-generic/mman-common.h`, `net/core/secure_seq.c`, `arch/{x86,s390}/kvm/Kconfig`
- Firecracker @ `edb60617c31ebd610c530f67706ec5c79d4c2725`
  - Docs: `SPECIFICATION.md`, `docs/snapshotting/{snapshot-support,random-for-clones,network-for-clones}.md`, `docs/{hugepages,ballooning,pmem}.md`
  - Source and tests: `src/vmm/src/{builder.rs, vstate/memory.rs}`, `tests/host_tools/memory.py`
- libkrun @ `1f5dd0288fdd3c2da5eb5b555126f82f2efc53d7`: `src/hvf/src/lib.rs`, `src/libkrun/src/vmm/macos/vstate.rs`, `src/devices/src/virtio/fs/macos/passthrough.rs`
