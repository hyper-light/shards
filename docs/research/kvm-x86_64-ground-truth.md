# KVM (x86_64) ground-truth reference for shards

Status: research reference, compiled 2026-09-28, for the x86_64 KVM backend (Linux hosts, tested on GitHub `ubuntu-24.04` runners). Every row cites a primary source that was read during compilation. Anything not verifiable from a primary source is marked **UNVERIFIED** and collected in §10. "Derived:" marks our own conclusions.

It complements [hvf-arm64-kvm-ground-truth.md](hvf-arm64-kvm-ground-truth.md). That document's §4 covers KVM arm64 (including the generic memslot, dirty-log and ioctl encoding rules) and its §5 covers the HVF-vs-KVM backend contract; neither is repeated here.

## Citation conventions

| Prefix / form | Meaning |
|---|---|
| `path/in/tree:N` | Linux 7.2-rc4 at `/Users/adalundhe/Projects/linux` (commit 1590cf032971, Makefile VERSION=7 PATCHLEVEL=2 EXTRAVERSION=-rc4) |
| `api` | `Documentation/virt/kvm/api.rst` |
| `ukvm` / `xkvm` | `include/uapi/linux/kvm.h` / `arch/x86/include/uapi/asm/kvm.h` |
| `boot` / `bp` | `Documentation/arch/x86/boot.rst` / `arch/x86/include/uapi/asm/bootparam.h` |
| `kmain`, `x86.c`, `cpuid.c`, `irq.c`, `ioapic.c`, `lapic.c`, `vmx.c`, `svm.c`, `eventfd.c`, `pvabi`, `kpara` | `virt/kvm/kvm_main.c`, `arch/x86/kvm/x86.c`, `arch/x86/kvm/cpuid.c`, `arch/x86/kvm/irq.c`, `arch/x86/kvm/ioapic.c`, `arch/x86/kvm/lapic.c`, `arch/x86/kvm/vmx/vmx.c`, `arch/x86/kvm/svm/svm.c`, `virt/kvm/eventfd.c`, `arch/x86/include/asm/pvclock-abi.h`, `arch/x86/include/uapi/asm/kvm_para.h` |
| `fc:path:N` | Firecracker at `/Users/adalundhe/Projects/firecracker`, commit edb60617c31e (`v1.16.0-dev-751-gedb60617c`, 2026-09-25) |
| `ll:` / `kb:` / `ki:` / `vs:` | rust-vmm crates Firecracker links: linux-loader 0.14.0, kvm-bindings 0.14.1, kvm-ioctls 0.25.0 and vm-superio 0.8.2. Downloaded from crates.io; each `.crate` sha256 equals the checksum in `fc:Cargo.lock`. Paths are relative to the crate root. |
| `ART:` | Measured on the Firecracker CI kernel artifacts downloaded on 2026-09-28 (§7). `ART:cfg:N` = line N of `x86_64/vmlinux-6.18.48.config`; `ART:vmlinux` = that kernel's ELF headers. |
| `ACPI6.5 §x` | ACPI Specification 6.5, HTML edition at uefi.org/specs/ACPI/6.5/, retrieved 2026-09-28 |
| `PROBE` | Computed by compiling the 7.2-rc4 uapi headers for `x86_64-linux-gnu` (§1 preamble) |
| `CI:` | Observed in this repo's GitHub Actions log (run 36459178590, job `x86_64-unknown-linux-gnu`, runner image `ubuntu-24.04` version 20260920.314.1) |

Firecracker and its crates are cited only for what Firecracker *does*: it is the benchmark shards has to beat. KVM and Linux semantics are always cited from the kernel tree.

## Contents

1. KVM x86 API: minimal VM bring-up
2. Linux x86_64 boot protocols
3. Firmware tables and device discovery
4. Power-off and reset paths
5. Timekeeping and legacy devices affecting boot time
6. Memory layout
7. The guest kernel artifact
8. Boot-time cost facts
9. Implementation checklist
10. UNVERIFIED

---

## 1. KVM x86 API: minimal VM bring-up

`PROBE` values (ioctl numbers, struct sizes and offsets) come from compiling the 7.2-rc4 uapi headers (`include/uapi/linux/kvm.h`, `arch/x86/include/uapi/asm/{kvm,bootparam}.h`, `include/xen/interface/hvm/start_info.h`) with `clang -target x86_64-linux-gnu` into an ELF object and reading the values back. Nothing was executed. Every size checked against kvm-bindings' bindgen layout tests matched (e.g. `kvm_run` 2352 B, `kb:src/x86_64/bindings.rs:4648`; `kvm_sregs` 312 B, `:1274`; `kvm_fpu` 416 B, `:1362`).

### 1.1 fd model and VM-level ioctls, in bring-up order

| # | ioctl (fd) | Number | Argument | Rule | Source |
|---|---|---|---|---|---|
| 1 | `open("/dev/kvm", O_RDWR\|O_CLOEXEC)` | — | — | System fd. `KVM_CREATE_VM` on it gives the VM fd, `KVM_CREATE_VCPU` on that gives vCPU fds. VM ioctls must come from the creating process; vCPU ioctls "should" come from the creating thread (performance only). | api:10-47 |
| 2 | `KVM_GET_API_VERSION` (sys) | 0xAE00 | none | Must return 12; refuse to run otherwise. | api:132-146; ukvm:22, 715 |
| 3 | `KVM_CHECK_EXTENSION` (sys or VM) | 0xAE03 | cap number, **by value** | 0 = absent; >0 = present (some caps return a value). Prefer the VM fd. | api:262-279; ukvm:724 |
| 4 | `KVM_GET_VCPU_MMAP_SIZE` (sys) | 0xAE04 | none | x86 returns **3 pages**: `kvm_run` (page 0), PIO data page (page 1 = `KVM_PIO_PAGE_OFFSET`), coalesced-MMIO ring (page 2). 12288 on a 4 KiB host. Call once per process. | api:281-299; kmain:5540-5550, 4051-4060; xkvm:16-17 |
| 5 | `KVM_GET_SUPPORTED_CPUID` (sys) | 0xC008AE05 | `kvm_cpuid2` + `nent` × `kvm_cpuid_entry2` | `nent` < 1 gives E2BIG. `nent` > 256 is **clamped to 256 and succeeds** (the doc's "ENOMEM if too high" does not match the code). Too few entries for the host gives E2BIG. Enumerates ranges 0x0…, 0x8000_0000…, 0x4000_0000… (KVM leaves) and Centaur on Centaur/Zhaoxin hosts. Pass `nent = 256` once and cache. | api:1762-1817; cpuid.c:1977-2018, 1925-1948; arch/x86/include/asm/kvm_host.h:170 |
| 6 | `KVM_CREATE_VM` (sys) | 0xAE01 | type, by value: 0 = `KVM_X86_DEFAULT_VM` | New VM has no vCPUs and no memory. Supported types via `KVM_CAP_VM_TYPES` (235). | api:149-164; xkvm:970 |
| 7 | `KVM_SET_TSS_ADDR` (VM) | 0xAE47 | address **by value** | "three-page region … within the first 4GB … must not conflict with any memory slot or any mmio address", "required on Intel-based hosts". Code: EINVAL if addr > 0xFFFF_D000. It is a **no-op returning 0** when `kvm_intel.unrestricted_guest=1` (default when the CPU has it with EPT) and on AMD (optional op defaulting to "return 0"). | api:1438-1455; x86.c:6674-6682; vmx.c:5254-5271, 105-106, 8640-8641; arch/x86/include/asm/kvm-x86-ops.h:94 |
| 8 | `KVM_SET_IDENTITY_MAP_ADDR` (VM) | 0x4008AE48 | **pointer to a u64** (not by value) | One page below 4 GiB, default 0xFFFB_C000. EINVAL once any vCPU exists. Used only for EPT without unrestricted guest (`init_rmode_identity_map`). | api:1621-1643; x86.c:7281-7295; vmx.c:4010-4028, 7735-7739; arch/x86/include/asm/vmx.h:588 |
| 9 | `KVM_CREATE_IRQCHIP` (VM) | 0xAE60 | none | Creates PIC×2 + IOAPIC (base 0xFEC0_0000, 24 pins) and a LAPIC for every future vCPU (base 0xFEE0_0000). EINVAL if any vCPU exists; EEXIST if repeated. Installs the default GSI routing (§1.7). Compiled only with `CONFIG_KVM_IOAPIC` (default y); `KVM_CAP_IRQCHIP` still reports 1 without it. | api:847-859; x86.c:7299-7342, 4773; arch/x86/kvm/Kconfig:172-180; arch/x86/kvm/ioapic.h:19-20; arch/x86/include/asm/apicdef.h:14-15 |
| 10 | `KVM_CREATE_PIT2` (VM) | 0x4040AE77 | `kvm_pit_config {u32 flags; u32 pad[15]}` (64 B) | ENOENT unless the PIC is in the kernel. Claims ports 0x40-0x43; `KVM_PIT_SPEAKER_DUMMY` (1) also claims 0x61. **Spawns a kthread worker `kvm-pit/<pid>`** per VM. Whether a PIT is needed is in §5. | api:3021-3051; x86.c:7346-7364; arch/x86/kvm/i8254.c:753-785; arch/x86/kvm/i8254.h:55-57 |
| 11 | `KVM_SET_USER_MEMORY_REGION` (VM) | 0x4020AE46 | `kvm_userspace_memory_region` (32 B) | §1.3 | ukvm:1267-1268 |
| 12 | `KVM_SET_USER_MEMORY_REGION2` (VM) | 0x40A0AE49 | `…_region2` (160 B) | Only needed for guest_memfd. Cap 231. | ukvm:1271-1272, 981 |
| 13 | `KVM_IRQFD` / `KVM_IOEVENTFD` (VM) | 0x4020AE76 / 0x4040AE79 | §1.7 | | ukvm:1295, 1298 |
| 14 | `KVM_CREATE_VCPU` (VM) | 0xAE41 | vCPU id, by value: "(apic id on x86)" | Id < `KVM_CAP_MAX_VCPU_ID`. BSP is id 0 unless `KVM_SET_BOOT_CPU_ID` (0xAE78) was called before any vCPU. | api:307-333, 1645-1657; ukvm:1263 |

### 1.2 Capabilities

x86 answers from `kvm_vm_ioctl_check_extension` (x86.c:4768-4960) and the generic `kvm_vm_ioctl_check_extension_generic` (kmain:4864-4938). Every cap below except the host-dependent ones returns 1 on any 7.2 x86 host.

| Cap | # | x86 result | shards needs it for | Source |
|---|---|---|---|---|
| IRQCHIP | 0 | 1 | in-kernel PIC/IOAPIC/LAPIC | ukvm:736; x86.c:4773 |
| USER_MEMORY | 3 | 1 | memslots | ukvm:739; kmain:4868 |
| SET_TSS_ADDR | 4 | 1 | step 7 | ukvm:740; x86.c:4776 |
| EXT_CPUID | 7 | 1 | `KVM_GET_SUPPORTED_CPUID` | ukvm:742; x86.c:4777; api:1765 |
| MP_STATE | 14 | 1 | AP state, snapshots | ukvm:749; x86.c:4787 |
| IRQFD | 32 | 1 | virtio interrupts | ukvm:768; kmain:4877 |
| PIT2 | 33 | 1 (only with `CONFIG_KVM_IOAPIC`) | optional PIT | ukvm:770; x86.c:4780-4782 |
| IOEVENTFD | 36 | 1 | virtio notify | ukvm:776; x86.c:4790 |
| SET_IDENTITY_MAP_ADDR | 37 | 1 | step 8 | ukvm:777; x86.c:4793 |
| ADJUST_CLOCK | 39 | `KVM_CLOCK_VALID_FLAGS` | kvmclock snapshot | ukvm:781; x86.c:4881-4883 |
| VCPU_EVENTS | 41 | 1 | snapshots | ukvm:784; x86.c:4794 |
| XSAVE / XCRS | 55 / 56 | 1 / host has XSAVE | snapshots | ukvm:802, 805; x86.c:4813, 4918-4920 |
| TSC_CONTROL | 60 | host-dependent (`has_tsc_control`) | `KVM_SET_TSC_KHZ` on restore | ukvm:810; x86.c:4921-4923 |
| MAX_VCPUS | 66 | `KVM_MAX_VCPUS` (1024 unless `CONFIG_KVM_MAX_NR_VCPUS`) | sizing | ukvm:816; x86.c:4904-4908; arch/x86/include/asm/kvm_host.h:54-56 |
| TSC_DEADLINE_TIMER | 72 | 1 | may set CPUID.1:ECX[24] | ukvm:822; x86.c:4819; api:1841-1848 |
| KVMCLOCK_CTRL | 76 | 1 | pause notification | ukvm:826; x86.c:4817 |
| SPLIT_IRQCHIP | 121 | 1 | alternative to IRQCHIP | ukvm:871; x86.c:4822 |
| IOEVENTFD_ANY_LENGTH | 122 | 1 | len=0 fast MMIO notify | ukvm:872; kmain:4879 |
| X2APIC_API | 129 | flag mask | >255 vCPUs | ukvm:879; x86.c:4925-4928 |
| IMMEDIATE_EXIT | 136 | 1 | kick protocol (§1.6) | ukvm:886; x86.c:4823 |
| X86_DISABLE_EXITS | 143 | allowed mask | only with dedicated pCPUs | ukvm:893; x86.c:4884-4886; api:8060-8083 |
| NR_MEMSLOTS | 10 | `KVM_USER_MEM_SLOTS` = SHRT_MAX − 3 on x86 | sizing | kmain:4904; include/linux/kvm_host.h:710-711; arch/x86/include/asm/kvm_host.h:72 |

**Irqchip choice.** `KVM_CREATE_IRQCHIP` puts PIC, IOAPIC and LAPIC in the kernel. `KVM_CAP_SPLIT_IRQCHIP` (enable-cap on the VM, `args[0]` = routes reserved for a userspace IOAPIC; fails after any vCPU exists or after `KVM_CREATE_IRQCHIP`) keeps only the LAPIC in the kernel. Only MSI routes are then allowed, and every EOI on a reserved route exits with `KVM_EXIT_IOAPIC_EOI` (api:7918-7937). Derived: the full in-kernel irqchip is the right choice for boot. Split mode moves the IOAPIC, PIC and PIT into the VMM and adds an exit per level-triggered EOI.

### 1.3 Memory slots

| Item | Value | Source |
|---|---|---|
| `struct kvm_userspace_memory_region` | `{u32 slot; u32 flags; u64 guest_phys_addr; u64 memory_size; u64 userspace_addr}`, 32 B, offsets 0/4/8/16/24 (PROBE) | api:1369-1375; ukvm:30-36 |
| slot field | bits 0-15 slot id, bits 16-31 address-space id. x86 has 2 address spaces with `CONFIG_KVM_SMM` (`KVM_MAX_NR_ADDRESS_SPACES` 2; as_id 1 is SMM); normal memory is as_id 0. | api:1381-1392; arch/x86/include/asm/kvm_host.h:2446-2453 |
| flags | `LOG_DIRTY_PAGES` 1, `READONLY` 2 (writes become `KVM_EXIT_MMIO`), `GUEST_MEMFD` 4 | api:1413-1418; ukvm:56-58 |
| Alignment | size, GPA and HVA multiples of host `PAGE_SIZE` (4 KiB on x86); HVA untagged and `access_ok` | kmain:2011-2033 |
| Changes | size 0 deletes; move and flag changes allowed; no resize | api:1394-1396 |
| Huge pages | keep the low 21 bits of GPA and HVA equal | api:1409-1411 |
| Private slots | KVM places up to 3 internal slots in the same GPA space: the TSS (3 pages at the `SET_TSS_ADDR` address), the EPT identity map (1 page, default 0xFFFB_C000) and the APIC-access page (1 page at **0xFEE0_0000**, created when APICv/"virtualize APIC accesses" is on). User slots must not overlap them. An overlap fails with **EEXIST** on whichever comes second, because KVM checks every slot, private ones included, and creates private slots in every address space. Derived: keep 0xFEC0_0000 (IOAPIC) and 0xFEE0_0000 (LAPIC) out of RAM memslots too, so guest accesses reach the in-kernel devices. | arch/x86/include/asm/kvm_host.h:72; vmx.c:5262, 4026-4028; lapic.c:2911-2912; arch/x86/kvm/ioapic.h:19-20; kmain:1981-1992, 2092-2094; x86.c:13405-13413 |

### 1.4 vCPU setup ioctls

| ioctl | Number | Struct (PROBE size) | Semantics | Source |
|---|---|---|---|---|
| `KVM_SET_CPUID2` | 0x4008AE90 | `kvm_cpuid2 {u32 nent; u32 pad; entries[]}` (8 B) + `kvm_cpuid_entry2 {function, index, flags, eax, ebx, ecx, edx, pad[3]}` (40 B) | Set before the first `KVM_RUN`: changing CPUID after running "may cause guest instability". On failure the previous CPUID state is not guaranteed. Only APIC IDs and topology may differ between vCPUs. flags bit 0 `SIGNIFCANT_INDEX` (sic). | api:717-724, 1773-1792; xkvm:251-270 |
| — the template | | | `KVM_GET_SUPPORTED_CPUID` is a template, not a finished answer: leaf 1 EBX still carries the **host** CPU's APIC ID in bits 31:24, and leaves 0xB and 0x1F come back with EAX=EBX=ECX=0 (no topology). The VMM must fill both per vCPU. KVM adds `HYPERVISOR` (CPUID.1:ECX[31]), x2APIC and TSC-deadline as emulated bits, and the KVM leaves 0x4000_0000 ("KVMKVMKVM", max leaf 0x4000_0001) and 0x4000_0001 (feature bits). | cpuid.c:1436-1439, 1531-1538, 872-881, 1705-1734; Documentation/virt/kvm/x86/cpuid.rst:15-40 |
| — why the hypervisor bit matters | | | The guest looks for the KVM signature only if CPUID.1:ECX[31] is set. Without it there is no kvmclock and no PV features. | arch/x86/kernel/kvm.c:887-906 |
| `KVM_SET_MSRS` | 0x4008AE89 | `kvm_msrs {u32 nmsrs; u32 pad; entries[]}` (8 B) + `kvm_msr_entry {u32 index; u32 reserved; u64 data}` (16 B) | Returns the **number of MSRs set** and stops at the first failure. Check `ret == nmsrs`. | api:665-702; xkvm:189-200 |
| `KVM_SET_REGS` | 0x4090AE82 | `kvm_regs` (144 B): rax,rbx,rcx,rdx,rsi,rdi,rsp,rbp,r8-r15 at 0..120, rip @128, rflags @136 | KVM ORs in RFLAGS bit 1 (`X86_EFLAGS_FIXED`). | api:433-440; xkvm:117-125; x86.c:12149 |
| `KVM_SET_SREGS` | 0x4138AE84 | `kvm_sregs` (312 B): cs,ds,es,fs,gs,ss,tr,ldt (`kvm_segment`, 24 B each) @0..168; gdt @192, idt @208 (`kvm_dtable {u64 base; u16 limit; u16 pad[3]}`, 16 B); cr0 @224, cr2 @232, cr3 @240, cr4 @248, cr8 @256, efer @264, apic_base @272, interrupt_bitmap[4] @280 | Rejected unless consistent: EFER.LME && CR0.PG ⇒ CR4.PAE, **EFER.LMA set by the VMM**, legal CR3. Otherwise LMA=0 and CS.L=0. CR0 and CR4 are validated against the guest CPUID, so call `KVM_SET_CPUID2` first. `kvm_segment` = `{u64 base; u32 limit; u16 selector; u8 type, present, dpl, db, s, l, g, avl, unusable, padding}`. | api:487-495; xkvm:132-159; x86.c:12368-12391 |
| `KVM_SET_FPU` | 0x41A0AE8D | `kvm_fpu` (416 B): fpr[8][16] @0, fcw @128, fsw @130, ftwx @132, last_opcode @134, last_ip @136, last_dp @144, xmm[16][16] @152, mxcsr @408 | A new vCPU's FPU already has FCW=0x37F and MXCSR=0x1F80. Setting those values again is a no-op. | api:784-797; xkvm:175-187; arch/x86/kernel/fpu/core.c:244-258, 537-541, 559-567 |
| `KVM_GET/SET_LAPIC` | 0x8400AE8E / 0x4400AE8F | `kvm_lapic_state {char regs[0x400]}` (1024 B), xAPIC register page layout | LVT0 @0x350, LVT1 @0x360; delivery mode bits 10:8 (ExtINT 0x700, NMI 0x400); mask bit 16. KVM's LAPIC reset masks every LVT, then (quirk `KVM_X86_QUIRK_LINT0_REENABLED`, on by default) sets the **BSP's** LVT0 to ExtINT unmasked. LVT1 stays masked. Linux reprograms LVT0 from the masked bit it finds and always writes LVT1 = NMI on the BSP. | api:2038-2090; xkvm:127-130; arch/x86/include/asm/apicdef.h:95-121; lapic.c:2990-2997; arch/x86/kernel/apic/apic.c:1609-1641 |
| `KVM_GET/SET_MP_STATE` | 0x8004AE98 / 0x4004AE99 | `u32`: RUNNABLE 0, UNINITIALIZED 1, INIT_RECEIVED 2, HALTED 3, SIPI_RECEIVED 4 | "only useful after KVM_CREATE_IRQCHIP". At creation with an in-kernel irqchip the BSP is RUNNABLE and **every AP is UNINITIALIZED**. The AP's `KVM_RUN` blocks in the kernel until the guest sends INIT/SIPI, so the VMM never loads AP registers. | api:1526-1558, 1608-1610; x86.c:12784-12787, 11973-12002 |
| `KVM_SET_TSC_KHZ` | 0xAEA2 | value | vCPU ioctl (`TSC_CONTROL`) or, before vCPUs exist, VM ioctl (`VM_TSC_CONTROL`) | api:2001-2022 |

### 1.5 `KVM_RUN` (0xAE80) and `struct kvm_run`

Layout (PROBE; sizeof = 2352):

| Field | Offset | Type | Notes |
|---|---|---|---|
| `request_interrupt_window` | 0 | u8 | userspace-irqchip only |
| `immediate_exit` | 1 | u8 | read once at KVM_RUN entry (§1.6) |
| `exit_reason` | 8 | u32 | valid only when KVM_RUN returned 0 |
| `ready_for_interrupt_injection`, `if_flag` | 12, 13 | u8 | userspace-irqchip only |
| `flags` | 14 | u16 | `KVM_RUN_X86_SMM` 1, `BUS_LOCK` 2, `GUEST_MODE` 4 |
| `cr8`, `apic_base` | 16, 24 | u64 | userspace-LAPIC only |
| `io` | 32 | `{u8 direction (IN 0, OUT 1); u8 size; u16 port; u32 count; u64 data_offset}` | data at `kvm_run + data_offset` = 4096 on x86 (the PIO page) |
| `mmio` | 32 | `{u64 phys_addr; u8 data[8] @40; u32 len @48; u8 is_write @52}` | |
| `fail_entry` | 32 | `{u64 hardware_entry_failure_reason; u32 cpu @40}` | |
| `internal` | 32 | `{u32 suberror; u32 ndata @36; u64 data[16] @40}` | |
| `system_event` | 32 | `{u32 type; u32 ndata @36; u64 data[16] @40}` | |
| `kvm_valid_regs`, `kvm_dirty_regs`, `s` | 288, 296, 304 | u64, u64, 2048 B | `KVM_CAP_SYNC_REGS` |

Sources: api:6582-6755; ukvm:224-521 (io 260-269, mmio 274-280, internal 326-332, system_event 385-402, sync regs 515-520); x86.c:8424-8431.

**Return codes.** `0` means `exit_reason` is valid. `-1/EINTR` means a signal was pending (`exit_reason = KVM_EXIT_INTR`) **or** `immediate_exit` was set, in which case `exit_reason` is **not written** and holds a stale value. `-1/EAGAIN` comes from an AP that sat UNINITIALIZED and has just received INIT/SIPI; re-enter. Sources: api:401-410; x86.c:11973-12002, 12053-12056; include/linux/kvm_host.h:2471-2487.

| Exit | # | When it happens on x86 | VMM action | Source |
|---|---|---|---|---|
| `IO` | 2 | IN/OUT to a port that no in-kernel device claims. PIC, PIT and ioeventfd ports are claimed. | OUT: read `count × size` bytes at `data_offset`. IN: write them there. **KVM advances RIP on the next KVM_RUN for both directions** (except the `OUT 0x7E` quirk). KVM zero-fills the IN buffer before exiting, so an unhandled IN reads **0x00, not 0xFF**. String I/O (`rep ins/outs`) arrives with `count` > 1. | api:6695-6710; x86.c:8390-8434, 9654-9745 |
| `MMIO` | 6 | Access to a GPA with no memslot and no in-kernel device. The LAPIC and IOAPIC are in the kernel. Also writes to a `READONLY` slot. | Write: consume `data[0..len)`. Read: fill `data[0..len)`. KVM completes the instruction on the next KVM_RUN. Accesses wider than 8 bytes arrive as several exits (fragments). | api:6724-6755; x86.c:11843-11897 |
| `HLT` | 5 | Only **without** an in-kernel LAPIC. With `KVM_CREATE_IRQCHIP`, HLT blocks in the kernel until an interrupt, and a guest halting with IF=0 never returns to the VMM. | n/a with the in-kernel irqchip | x86.c:11741-11765 |
| `SHUTDOWN` | 8 | Triple fault: VMX `EXIT_REASON_TRIPLE_FAULT`, SVM shutdown intercept (SVM also re-INITs the vCPU before exiting). | Treat as guest reset/poweroff. See §4. | x86.c:11185-11194; vmx.c:5562-5567, 6320; svm.c:2160-2181 |
| `FAIL_ENTRY` | 9 | VM entry rejected the guest state. `hardware_entry_failure_reason` = VMX exit reason (failed-entry) or VM-instruction error; `cpu` = last pCPU. | Fatal. Dump REGS and SREGS. Usually wrong SREGS. | vmx.c:6771-6787; svm.c:3727, 4505 |
| `INTR` | 10 | A signal is pending (KVM_RUN returns EINTR) | kick handling (§1.6) | include/linux/kvm_host.h:2471-2487 |
| `INTERNAL_ERROR` | 17 | `suberror` EMULATION 1 (the emulator could not decode or emulate, e.g. an MMIO access by an unsupported instruction), SIMUL_EX 2, DELIVERY_EV 3, UNEXPECTED_EXIT_REASON 4 | Fatal. Log `suberror` and `data[0..ndata)`. | ukvm:197-205, 326-332; x86.c:9084-9171, 12352-12366 |
| `SYSTEM_EVENT` | 24 | Plain x86 guests produce this only through Hyper-V crash/reset MSRs (with `CONFIG_KVM_HYPERV` and Hyper-V CPUID exposed), SEV termination or TDX. A normal Linux shutdown never produces it. | Handle SHUTDOWN 1 / RESET 2 / CRASH 3 generically. | x86.c:11227-11244; arch/x86/kvm/svm/sev.c:4401, 4566; arch/x86/kvm/vmx/tdx.c:1301; api:6936-6999 |
| `IOAPIC_EOI` | 26 | Split irqchip only | n/a | x86.c:11214-11222; api:7930-7934 |

**Completing PIO/MMIO before snapshot or teardown.** KVM finishes a pending IN/MMIO read inside the *next* `KVM_RUN`, before it looks at `immediate_exit` (`complete_userspace_io` runs, then `wants_to_run` is tested). So "re-enter with `immediate_exit = 1`" completes the access without executing any further guest instruction (api:6743-6755; x86.c:12042-12056; kmain:4469).

**PIO vs MMIO cost.** "KVM_EXIT_IO is significantly faster than KVM_EXIT_MMIO" (api:6772). An MMIO exit makes KVM fetch and decode the instruction. An IN/OUT exit already carries port, size and direction in the VM-exit information. Derived: put any *high-frequency* VMM-only doorbell on a PIO port. The shards control page is written only a handful of times per boot, through `/dev/mem`, so it can stay MMIO at 0xC000_0000 (§6.4) for uniformity with arm64. A PIO marker would need `ioperm` in the guest; `CONFIG_X86_IOPL_IOPERM` is set in the CI kernel (ART:cfg:421).

### 1.6 Kicking a vCPU without lost wakeups

| Fact | Source |
|---|---|
| `immediate_exit` is "polled once when KVM_RUN starts; if non-zero, KVM_RUN exits immediately, returning -EINTR". The intended use: "set up a signal handler that sets run->immediate_exit to a non-zero value" instead of `KVM_SET_SIGNAL_MASK`, "which has worse scalability". | api:6591-6600 |
| KVM reads it once (`wants_to_run = !immediate_exit`) and then checks for pending signals before every guest entry (`__xfer_to_guest_mode_work_pending`), exiting with `-EINTR` / `KVM_EXIT_INTR`. | kmain:4469-4471; x86.c:11730-11735; include/linux/kvm_host.h:2471-2487 |
| KVM returns plain `-EINTR`, not `-ERESTARTSYS`, so `SA_RESTART` on the handler does not restart KVM_RUN. | include/linux/kvm_host.h:2479-2485 |
| `KVM_SET_SIGNAL_MASK` (0x4004AE8B) is the older alternative: the mask applies only while inside KVM_RUN. | api:745-768 |

Derived protocol (Firecracker's concrete version is in §1.8):
1. The kicker sets the vCPU's atomic request flag, then `pthread_kill(vcpu_thread, SIGRTMIN + k)`.
2. The handler (async-signal-safe) stores 1 to that thread's `kvm_run->immediate_exit`.
3. The vCPU loop, on `EINTR`, stores 0 to `immediate_exit` and services the request flag, then re-enters.

All three arrival windows are covered. If the signal arrives while the guest runs, the pending signal forces an exit. If it arrives inside KVM_RUN before entry, the per-entry signal check catches it. If it arrives in userspace just before the ioctl, the handler has already set `immediate_exit`, so the next KVM_RUN returns at once.

### 1.7 Interrupts: `KVM_IRQ_LINE`, `KVM_IRQFD`, `KVM_IOEVENTFD`

| Item | Detail | Source |
|---|---|---|
| Default GSI routing after `KVM_CREATE_IRQCHIP` | GSI *n* → IOAPIC pin *n* for 0 ≤ *n* < 24, **and** → PIC (master for 0-7, slave for 8-15, pin *n* mod 8) for *n* < 16. Identity mapping: GSI 0 is IOAPIC pin 0. No ISA "IRQ0 → pin 2" override is built in, so the MP table or MADT the guest reads must describe the same identity mapping. | api:857-859; irq.c:549-580 |
| `KVM_IRQ_LINE` (VM, 0x4008AE61) | `struct kvm_irq_level {u32 irq /* GSI */; u32 level}` (8 B). "edge-triggered interrupts require the level to be set to 1 and then back to 0". `level` means asserted, whatever the polarity (`KVM_CAP_IOAPIC_POLARITY_IGNORED`). VM ioctl: any thread may call it. | api:869-894; ukvm:1282 |
| IOAPIC edge pins | A 0→1 transition sets IRR and delivers. Asserting again while IRR is still set is **coalesced** (returns 0, nothing delivered). Lower the line after every pulse. | ioapic.c:187-243 |
| IOAPIC level pins | The line stays asserted until the VMM deasserts it. The pin re-delivers after the guest's EOI while it is still asserted (remote IRR). The VMM deasserts when the device's interrupt condition clears (e.g. guest ACKs virtio `InterruptStatus`). | ioapic.c:187-243 |
| Multiple sources | Levels from different source ids are ORed per pin. `KVM_IRQ_LINE` and irqfd share `KVM_USERSPACE_IRQ_SOURCE_ID`. | ioapic.c:500-518 |
| `KVM_IRQFD` (VM, 0x4020AE76) | `struct kvm_irqfd {u32 fd; u32 gsi; u32 flags; u32 resamplefd; u8 pad[16]}` (32 B). flags DEASSIGN 1, RESAMPLE 2. **Without RESAMPLE, every eventfd signal is a 1-then-0 pulse**, i.e. an edge. With RESAMPLE the GSI stays asserted until the guest EOIs, and then KVM signals `resamplefd`. | api:3178-3206; ukvm:1065-1081; eventfd.c:43-57 |
| irqfd latency on IOAPIC routes | `irqfd_wakeup` tries `kvm_arch_set_irq_inatomic`, which injects inline only for MSI (and Hyper-V/Xen) routes. IOAPIC/PIC routes return `-EWOULDBLOCK`, and injection is **deferred to `schedule_work`**, a kernel workqueue hop. | eventfd.c:225-240; irq.c:241-276 |
| `KVM_IOEVENTFD` (VM, 0x4040AE79) | `struct kvm_ioeventfd {u64 datamatch; u64 addr; u32 len; s32 fd; u32 flags; u8 pad[36]}` (64 B). flags DATAMATCH 1, PIO 2, DEASSIGN 4. `len` ∈ {0,1,2,4,8}. 0 requires `IOEVENTFD_ANY_LENGTH` and **cannot be combined with DATAMATCH**. | api:2093-2137; eventfd.c:1006-1008 |
| Fast MMIO doorbell | An MMIO ioeventfd with `len = 0` is also put on `KVM_FAST_MMIO_BUS`. VMX's EPT-misconfig handler signals it and skips the instruction **without decoding** it. SVM does the same when NRIPS gives `next_rip`. Any other MMIO ioeventfd needs the instruction emulated first. | eventfd.c:1014-1020; vmx.c:5983-6002; svm.c:1995-2010 |
| MSI | `KVM_SIGNAL_MSI` (0x4020AEA5) or MSI entries in `KVM_SET_GSI_ROUTING` (0x4008AE6A) inject inline through `kvm_irq_delivery_to_apic_fast`. This matters later for virtio-pci and MSI-X. | irq.c:255-263; api:2982-3013 |

### 1.8 Firecracker's x86_64 bring-up, as implemented

The order is `build_microvm_for_boot` (fc:src/vmm/src/builder.rs:143-364). The VMM thread sets all vCPU state before any vCPU thread exists.

| # | Step | Firecracker value / behaviour | Source |
|---|---|---|---|
| 1 | `/dev/kvm`, API version | `O_RDWR\|O_CLOEXEC`; refuses anything but 12 | ki:src/ioctls/system.rs:42-44, 124-136; fc:src/vmm/src/vstate/kvm.rs:30-37 |
| 2 | Required caps (system fd) | IRQCHIP 0, USER_MEMORY 3, SET_TSS_ADDR 4, EXT_CPUID 7, MP_STATE 14, IRQFD 32, PIT2 33, PIT_STATE2 35, IOEVENTFD 36, ADJUST_CLOCK 39, VCPU_EVENTS 41, DEBUGREGS 50, XSAVE 55, XCRS 56. **Never probed:** IMMEDIATE_EXIT (its kick depends on it), SPLIT_IRQCHIP, SET_IDENTITY_MAP_ADDR, TSC_DEADLINE_TIMER. | fc:src/vmm/src/arch/x86_64/kvm.rs:31-46; fc:src/vmm/src/vstate/kvm.rs:39-73 |
| 3 | AMX permission | `arch_prctl(ARCH_REQ_XCOMP_GUEST_PERM, XTILEDATA)` before `KVM_GET_SUPPORTED_CPUID` when AMX exists. Otherwise pre-6.4 hosts report TILECFG without TILEDATA, and the guest's XSETBV takes #GP. | fc:src/vmm/src/arch/x86_64/xstate.rs:32-84; api:1802-1804 |
| 4 | `KVM_GET_SUPPORTED_CPUID` | ≤ 256 entries, cached once | fc:src/vmm/src/arch/x86_64/kvm.rs:55-57 |
| 5 | `KVM_CREATE_VM(0)` | retried up to 5× on EINTR | fc:src/vmm/src/vstate/vm.rs:148-182 |
| 6 | `KVM_GET_MSR_INDEX_LIST`, `KVM_CAP_XSAVE2` | Snapshot MSR list and XSAVE buffer size | fc:src/vmm/src/arch/x86_64/vm.rs:76-99 |
| 7 | `KVM_SET_TSS_ADDR(0xFFFB_D000)` | 3 pages to 0xFFFC_0000. `SET_IDENTITY_MAP_ADDR` is never called, so the KVM default 0xFFFB_C000 applies (the page just below). | fc:src/vmm/src/arch/x86_64/vm.rs:101-104; fc:src/vmm/src/arch/x86_64/layout.rs:40-41; vmx.c:4023-4024 |
| 8 | `KVM_CREATE_IRQCHIP`, then `KVM_CREATE_PIT2 {flags: SPEAKER_DUMMY}` | Full in-kernel irqchip; split mode is never used | fc:src/vmm/src/arch/x86_64/vm.rs:116-120, 171-183 |
| 9 | `KVM_CREATE_VCPU(i)`, i = 0..n−1, then mmap `kvm_run` | vCPU id = index = APIC ID | fc:src/vmm/src/arch/x86_64/vcpu.rs:175-188; fc:src/vmm/src/vstate/vm.rs:209-217 |
| 10 | Memslots, **after** vCPU creation | Slots 0,1,2…; flags 0 (`LOG_DIRTY_PAGES` only when dirty tracking is on). Memory is `MAP_PRIVATE\|MAP_ANONYMOUS\|MAP_NORESERVE` (+ hugetlb). | fc:src/vmm/src/builder.rs:179-180; fc:src/vmm/src/vstate/vm.rs:413-424, 458-495; fc:src/vmm/src/vstate/memory.rs:1054-1066 |
| 11 | Legacy devices, kernel load, boot timer, virtio devices | §1.8 interrupts table, §2.4, §8 | fc:src/vmm/src/builder.rs:214-290 |
| 12 | Per vCPU: `SET_CPUID2` → `SET_MSRS` → `SET_REGS` → `SET_FPU` → `GET/SET_SREGS` → `GET/SET_LAPIC` | CPUID goes first because KVM gates CPUID-dependent MSRs on it. `SET_MSRS` must return the full count. | fc:src/vmm/src/arch/x86_64/mod.rs:181-230; fc:src/vmm/src/arch/x86_64/vcpu.rs:203-303; fc:src/vmm/src/arch/x86_64/msr.rs:441-452 |
| 13 | Guest memory writes | cmdline, mptable, zero page **or** `hvm_start_info`, ACPI tables | fc:src/vmm/src/arch/x86_64/mod.rs:257-301 |
| 14 | vCPU threads | One per vCPU, started Paused. The Resume message enters `KVM_RUN`. | fc:src/vmm/src/builder.rs:342-351; fc:src/vmm/src/vstate/vcpu.rs:182-237 |
| — | Not at boot | `SET_MP_STATE`, `SET_TSC_KHZ`, `SET_XSAVE`, `SET_XCRS`, `SET_VCPU_EVENTS`, `SET_DEBUGREGS` run on restore only; `KVMCLOCK_CTRL` on pause and restore | fc:src/vmm/src/arch/x86_64/vcpu.rs:684-750; fc:src/vmm/src/vstate/vcpu.rs:277-278 |
| — | Derived | Firecracker also writes the BSP's registers into every AP. That is harmless: with the in-kernel irqchip, APs are UNINITIALIZED until INIT/SIPI (§1.4). | x86.c:12784-12787 |

**CPUID normalization** (per vCPU, applied to the supported-CPUID template):

| Leaf | Change | Source |
|---|---|---|
| 0x1 EBX | [15:8] CLFLUSH line = 8 (64 B); [23:16] max logical IDs per package; **[31:24] initial APIC ID = vCPU index** | fc:src/vmm/src/cpu_config/x86_64/cpuid/normalize.rs:216-239 |
| 0x1 ECX/EDX | PDCM[15] = 0; **TSC-Deadline[24] = 1**; **Hypervisor[31] = 1**; HTT EDX[28] = (vCPUs > 1) | normalize.rs:241-260 |
| 0xB (and Intel 0x1F, copied from it) | EDX = x2APIC ID = index. Subleaf 0: SMT (threads per core). Subleaf 1: core level (EAX = 5, EBX = vCPU count). Subleaf 1 is inserted if missing. | normalize.rs:266-392; fc:src/vmm/src/cpu_config/x86_64/cpuid/intel/normalize.rs:164-277 |
| Intel 0x4 / 0x6 / 0x7.0 / 0xA | Cache sharing fields rewritten. Turbo and EPB cleared. EBX[6] and EBX[13] set. **WAITPKG (ECX[5]) cleared.** 0xA zeroed (no vPMU). | intel/normalize.rs:84-277 |
| AMD | 0x8000_001D/1E normalized; ARCH_CAPABILITIES CPUID bit cleared; TopoExt set | fc:src/vmm/src/cpu_config/x86_64/cpuid/amd/normalize.rs:88-116, 184-209 |
| 0x4000_0000+, 0x15, 0x4000_0010 | **untouched.** KVM's leaves pass through, and no TSC-frequency leaf is synthesized (§5). | Derived: no reference in fc:src/vmm/src/cpu_config/x86_64/cpuid/ (grep) |

**Boot MSRs** (every vCPU; they override template MSRs): SYSENTER_CS/ESP/EIP (0x174-0x176) = 0; STAR/LSTAR/CSTAR/SYSCALL_MASK (0xC000_0081-84) = 0; KERNEL_GS_BASE (0xC000_0102) = 0; IA32_TSC (0x10) = 0; IA32_MISC_ENABLE (0x1A0) = 1 (FAST_STRING); **MTRRdefType (0x2FF) = 0x806** (MTRRs enabled, default type WB). Firecracker's only justification is "required … for booting Linux", and its docs omit MTRRdefType (fc:src/vmm/src/arch/x86_64/msr.rs:392-425; fc:docs/cpu_templates/boot-protocol.md:11-25; index values arch/x86/include/asm/msr-index.h:11-17, 247-249, 422, 946, 1020). KVM does not use guest MTRRs for the EPT memory type (RAM is always WB, vmx.c:7819-7833), so MTRRdefType only affects the guest's own view. Derived: treat the whole list as optional and keep it only if a boot test fails without it (§10).

**Initial register state:**

| Register | 64-bit protocol | PVH | Source |
|---|---|---|---|
| RIP | `e_entry` (ELF) or load + 0x200 (bzImage) | PVH note entry | fc:src/vmm/src/arch/x86_64/regs.rs:86-113 |
| RSI / RBX | RSI = 0x7000 (zero page) | RBX = 0x6000 (`hvm_start_info`) | regs.rs:91, 107; layout.rs:43-55 |
| RSP = RBP | 0x8FF0 | 0 | regs.rs:103-105 |
| RFLAGS | 0x2 | 0x2 | regs.rs:90, 97 |
| GDT | At 0x500, limit 31: NULL; CS `0x00AF9B000000FFFF` (L=1); DS `0x00CF93000000FFFF`; TSS `0x008F8B000000FFFF` | NULL; CS `0x00CF9B000000FFFF` (D=1); DS as 64-bit; TSS `0x00008B0000000067` | regs.rs:163-166, 196-229; fc:src/vmm/src/arch/x86_64/gdt.rs:15-21 |
| Selectors | CS 0x08; DS/ES/FS/GS/SS 0x10; TR 0x18. **Not** boot.rst's 0x10/0x18. It still works because `startup_64` loads its own GDT at once. | same | fc:src/vmm/src/arch/x86_64/gdt.rs:101-120; head_64.S:74-80 |
| IDT | At 0x520, limit 7 (one null gate). Any exception before the kernel's IDT is loaded becomes a triple fault → `KVM_EXIT_SHUTDOWN`. | same | regs.rs:231-233 |
| CR0 / CR3 / CR4 / EFER | **0xE000_0011** (KVM reset value \| PE \| PG; CD and NW remain set) / 0x9000 / PAE / LME\|LMA | 0x11 / — / 0 / 0 | regs.rs:243-252, 278-280; x86.c:13055-13065 |
| Page tables | PML4 0x9000 → PDPT 0xA000 → PD 0xB000: 512 × 2 MiB entries `(i<<21)\|0x83`, identity [0, 1 GiB) | none | regs.rs:19-22, 154-157, 258-282 |
| FPU | FCW 0x37F, MXCSR 0x1F80 (same as the KVM default, §1.4) | same | regs.rs:61-69 |
| LAPIC | LVT0 delivery = ExtINT, LVT1 delivery = NMI, mask bits preserved. Derived effective values: BSP LVT0 0x700, LVT1 0x10400 (still masked); AP LVT0 0x10700. | same | fc:src/vmm/src/arch/x86_64/interrupts.rs:24-66; lapic.c:2990-2996 |

**Run loop and kick:**

| Item | Behaviour | Source |
|---|---|---|
| Pre-run | If `immediate_exit` is 1, clear it and treat as interrupted without calling `KVM_RUN` | fc:src/vmm/src/vstate/vcpu.rs:407-412 |
| Errors | EINTR: clear `immediate_exit`, process events. **EAGAIN: retry.** Anything else is fatal. | vcpu.rs:414-419, 513-528 |
| IO / MMIO | The data slice goes to the PIO or MMIO bus. Reads are zero-filled first, so **unregistered ports and addresses read 0** (logged). | ki:src/ioctls/vcpu.rs:1539-1591; fc:src/vmm/src/arch/x86_64/vcpu.rs:757-779; fc:src/vmm/src/vstate/bus.rs:187-216, 249-260 |
| SYSTEM_EVENT SHUTDOWN/RESET | clean exit | vcpu.rs:486-505 |
| HLT, SHUTDOWN (triple fault), FAIL_ENTRY, INTERNAL_ERROR, unknown | fatal (`UnhandledKvmExit` or GenericError) | fc:src/vmm/src/arch/x86_64/vcpu.rs:780-787; vcpu.rs:465-485 |
| i8042 reset | Guest `OUT 0x64, 0xFE` → the i8042 signals `reset_evt`, a clone of `vcpus_exit_evt` → the VMM stops cleanly (no vCPU exit involved) | fc:src/vmm/src/devices/legacy/i8042.rs:74, 96-97, 258-266; fc:src/vmm/src/device_manager/mod.rs:217-221 |
| Kick | Send the message on the channel; store `immediate_exit = 1` through the handle's own `MAP_SHARED` mapping of `kvm_run`; Release fence; `pthread_kill(SIGRTMIN+0)`. The handler only issues an Acquire fence. `KVM_SET_SIGNAL_MASK` is never used. On pause, `KVM_KVMCLOCK_CTRL`. | vcpu.rs:32-33, 122-132, 170-178, 270-282, 622-637; fc:src/vmm/src/utils/signal.rs:17-19 |

**Interrupts:**

| Item | Behaviour | Source |
|---|---|---|
| Routing | `KVM_SET_GSI_ROUTING` is never called without PCI, so the default identity routing (§1.7) applies. The mptable says "Per kvm_setup_default_irq_routing()". | fc:src/vmm/src/arch/x86_64/mptable.rs:228; fc:src/vmm/src/vstate/vm.rs:754-764 |
| GSI plan | 0-4 reserved; 5-23 legacy devices; 24-4095 MSI | fc:src/vmm/src/arch/x86_64/layout.rs:24-38; fc:src/vmm/src/vstate/resources.rs:66-91 |
| COM1 / i8042 | PIO 0x3F8 (8 ports) on GSI 4; PIO 0x60-0x64 on GSI 1. Both via **irqfd**. No other COM port. `KVM_IRQ_LINE` is never used. | fc:src/vmm/src/device_manager/legacy.rs:38-82 |
| virtio-mmio | 4 KiB per device from 0xC000_1000 (0xC000_0000 is the boot timer), one GSI each from 5. QueueNotify: one `KVM_IOEVENTFD {addr = base + 0x50, len 4, DATAMATCH = queue index}` per queue. Interrupt: `KVM_IRQFD`, no RESAMPLE. Setting `InterruptStatus` then writing the eventfd gives an edge. | fc:src/vmm/src/device_manager/mmio.rs:64, 164-213; fc:src/vmm/src/devices/virtio/transport/mmio.rs:454-467 |
| Declared trigger mode | The mptable intsrc uses "conforms to bus" (ISA → edge). The DSDT declares virtio, COM1 (PNP0501) and i8042 (PNP0303) interrupts **edge**, active-high. | mptable.rs:228-245; legacy.rs:87-142; fc:src/acpi-tables/src/aml.rs:562-600 |
| Derived | Every Firecracker line interrupt is an irqfd pulse on an edge pin. Nothing depends on level semantics or resamplefd. It uses len-4 DATAMATCH ioeventfds, so every QueueNotify is decoded by KVM's emulator. The len-0 fast path would drop the per-queue DATAMATCH. | rows above; §1.7 |

### 1.9 Other x86 ioctl numbers (snapshot, restore, debugging; PROBE)

| ioctl | Number | ioctl | Number |
|---|---|---|---|
| `KVM_GET_REGS` | 0x8090AE81 | `KVM_GET_SREGS` / `SET_SREGS2` / `GET_SREGS2` | 0x8138AE83 / 0x4140AECD / 0x8140AECC |
| `KVM_GET_MSRS` | 0xC008AE88 | `KVM_GET_MSR_INDEX_LIST` | 0xC004AE02 |
| `KVM_GET_FPU` | 0x81A0AE8C | `KVM_GET_CPUID2` | 0xC008AE91 |
| `KVM_GET/SET_VCPU_EVENTS` | 0x8040AE9F / 0x4040AEA0 | `KVM_GET/SET_DEBUGREGS` | 0x8080AEA1 / 0x4080AEA2 |
| `KVM_GET/SET_XSAVE`, `GET_XSAVE2` | 0x9000AEA4 / 0x5000AEA5, 0x9000AECF | `KVM_GET/SET_XCRS` | 0x8188AEA6 / 0x4188AEA7 |
| `KVM_GET_IRQCHIP` / **`KVM_SET_IRQCHIP` (encoded `_IOR`)** | 0xC208AE62 / 0x8208AE63 | `KVM_GET/SET_PIT2` | 0x8070AE9F / 0x4070AEA0 |
| `KVM_GET/SET_CLOCK` | 0x8030AE7C / 0x4030AE7B | `KVM_GET_TSC_KHZ` / `KVM_KVMCLOCK_CTRL` | 0xAEA3 / 0xAEAD |
| `KVM_ENABLE_CAP` | 0x4068AEA3 | `KVM_SET_GSI_ROUTING` / `KVM_SIGNAL_MSI` | 0x4008AE6A / 0x4020AEA5 |
| `KVM_SET_SIGNAL_MASK` | 0x4004AE8B | `KVM_NMI` / `KVM_SET_BOOT_CPU_ID` | 0xAE9A / 0xAE78 |

All values match the independent computation from kvm-ioctls' definitions and kvm-bindings' sizes (ki:src/kvm_ioctls.rs:15-237; kb:src/x86_64/bindings.rs). Numbers 0x9F, 0xA0, 0xA2, 0xA3 and 0xA5 are each shared by two ioctls. Only direction and size distinguish them, so always derive the number from `_IOC(dir, 0xAE, nr, sizeof)`.

### Section 1 bug-magnets

1. **`KVM_SET_TSS_ADDR` takes the address by value; `KVM_SET_IDENTITY_MAP_ADDR` takes a pointer to a u64.**
2. **Order matters.** `KVM_CREATE_IRQCHIP`, `SET_IDENTITY_MAP_ADDR`, `KVM_CAP_SPLIT_IRQCHIP`, `KVM_CAP_X86_DISABLE_EXITS` and `SET_BOOT_CPU_ID` must all come before the first `KVM_CREATE_VCPU`. `SET_CPUID2` must come before `SET_SREGS` and `SET_MSRS`, because CR4 and MSRs are validated against guest CPUID.
3. **`KVM_GET_SUPPORTED_CPUID` is a template.** Leaf 1 EBX[31:24] holds the *host's* APIC ID, and leaves 0xB/0x1F come back empty. Patch them per vCPU. Keep CPUID.1:ECX[31], or the guest never finds kvmclock.
4. **`KVM_SET_MSRS` returns a count**, and a short count is a failure.
5. **KVM's reset CR0 has CD|NW set.** ORing PE|PG into it (as Firecracker's 64-bit path does) boots with caches disabled until the kernel rewrites CR0. EFER.LMA must be supplied by the VMM.
6. **`EINTR` with `immediate_exit` leaves `exit_reason` stale; APs return `EAGAIN` after SIPI.** Branch on errno before reading `exit_reason`.
7. **An unhandled `IN` reads 0x00** (KVM zero-fills the buffer; Firecracker does too). Real hardware floats to 0xFF, so legacy probes may detect phantom devices (§5).
8. **Edge pins need 1-then-0.** An edge pin asserted again before being lowered is silently coalesced. Irqfd without RESAMPLE pulses by itself.
9. **Irqfd into IOAPIC pins is deferred to a workqueue**; MSI routes inject inline.
10. **Fast MMIO ioeventfd (`len` 0) cannot use DATAMATCH.** Firecracker's per-queue DATAMATCH doorbells take the decode path.
11. **With the in-kernel LAPIC, `HLT` never exits.** A guest that halts with IF=0 hangs silently (§4).
12. **`KVM_CAP_IRQCHIP` reports 1 even on hosts built without `CONFIG_KVM_IOAPIC`**, where `KVM_CREATE_IRQCHIP` is ENOTTY. Treat that ioctl's failure as "no x86 backend".
13. **The first `KVM_RUN` of a VM creates the `kvm-nx-lpage-recovery` task** (vhost task, `call_once`) unless the NX-hugepage mitigation is hard-disabled. It runs before the `immediate_exit` check (x86.c:11963; mmu/mmu.c:7967-7994). Derived: a warm VMM (D2) should issue one `KVM_RUN` with `immediate_exit = 1` during warm-up to pay this cost off the request path.

---

## 2. Linux x86_64 boot protocols

Linux offers three ways into an x86_64 kernel from a VMM. Only the first two avoid the decompressor.

| Entry | Image | CPU mode at entry | VMM builds | Source |
|---|---|---|---|---|
| `startup_64` via ELF `e_entry` (the 64-bit boot protocol applied to `vmlinux`) | uncompressed `vmlinux` ELF | 64-bit, paging on | identity page tables, GDT, `boot_params` (zero page) with e820 | boot:1362-1399; arch/x86/kernel/head_64.S:36-60 |
| PVH `pvh_start_xen` via `XEN_ELFNOTE_PHYS32_ENTRY` | uncompressed `vmlinux` ELF with `CONFIG_PVH` | 32-bit protected, paging off | `hvm_start_info` + memmap (+ modlist) | arch/x86/platform/pvh/head.S:29-48; include/xen/interface/elfnote.h:189-197 |
| bzImage 64-bit entry at load address + 0x200 | bzImage (compressed) | 64-bit, paging on | as row 1, plus the setup header copied from the file | boot:1362-1399, 704-706 |

### 2.1 The `vmlinux` ELF: what `e_entry` is and where PT_LOADs go

| Fact | Source |
|---|---|
| The linker script declares `ENTRY(phys_startup_64)` on x86_64, with `phys_startup_64 = ABSOLUTE(startup_64 - LOAD_OFFSET)`. **`e_entry` is the *physical* address of `startup_64`**, the kernel proper's 64-bit entry. It is not the decompressor's. | arch/x86/kernel/vmlinux.lds.S:39-43, 126-131 |
| `LOAD_OFFSET = __START_KERNEL_map = 0xffffffff80000000`. Every output section is `AT(ADDR(sec) - LOAD_OFFSET)`, so PT_LOAD `p_paddr = p_vaddr - 0xffffffff80000000`. | arch/x86/kernel/vmlinux.lds.S:18, 134, 175, 213; arch/x86/include/asm/page_64_types.h:46 |
| Link address: `__START_KERNEL = __START_KERNEL_map + LOAD_PHYSICAL_ADDR`, where `LOAD_PHYSICAL_ADDR` = `CONFIG_PHYSICAL_START` rounded up to `CONFIG_PHYSICAL_ALIGN` (0x100_0000 for the CI kernel, §7). | arch/x86/include/asm/page_types.h:32-34 |
| Loading rule: copy `p_filesz` bytes of each PT_LOAD to `p_paddr` and zero up to `p_memsz` (bss and brk live in the last PT_LOAD's memsz, §7). Loading at a different physical base works only with a **2 MiB-multiple** offset, and the entry moves with it. `__startup_64` spins in `for(;;)` on a non-2 MiB-aligned load delta and on an address beyond `MAX_PHYSMEM_BITS`, with no output. | arch/x86/boot/startup/map_kernel.c:102-116 |
| `startup_64` lives in `.init.text` (`__INIT`), not at the start of the image. Don't assume entry = first PT_LOAD. | arch/x86/kernel/head_64.S:36-38 |

### 2.2 64-bit boot protocol: CPU state and `boot_params`

**CPU state at entry** (boot:1387-1399, plus what `startup_64` and KVM enforce):

| Item | Required | Source |
|---|---|---|
| Mode | 64-bit mode, CS.L=1 CS.D=0, paging on | boot:1391; head_64.S:40-43 |
| Identity map | Covers the kernel image (from the load address for `init_size`), the zero page and the command line. `startup_64` reads `%rsi` and the kernel through it. With `CONFIG_AMD_MEM_ENCRYPT` it also parses the command line through it (`sme_enable(bp)`). | boot:1392-1393; head_64.S:84-95 |
| GDT | Descriptors for `__BOOT_CS` (0x10, execute/read) and `__BOOT_DS` (0x18, read/write), both 4 GiB flat. CS = 0x10; DS, ES, SS = 0x18. | boot:1394-1398 |
| Interrupts | disabled (RFLAGS.IF = 0) | boot:1398 |
| `%rsi` | physical address of `struct boot_params` | boot:1398-1399; head_64.S:46-59 |
| Paging depth | 4-level: CR4.LA57 = 0. `startup_64` takes 5-level only if CR4.LA57 is already set. | map_kernel.c:17-30, 102 |
| KVM validity | EFER.LME and EFER.LMA both set, CR0.PE\|PG, CR4.PAE, legal CR3 (the VMM sets LMA itself) | x86.c:12368-12391 |
| What the kernel replaces at once | Its own GDT and IDT (`startup_64_setup_gdt_idt`), then `__KERNEL_CS` via `lretq`, then its own page tables (`early_top_pgt`). The VMM's GDT and tables only have to survive until then. | head_64.S:74-80, 112-140 |

**`boot_params` (4096 B) for a raw `vmlinux`.** The file has no setup header, so the loader writes `boot_params` from scratch. Offsets are PROBE values, cross-checked against the comments in bootparam.h and against zero-page.rst.

| Field | Offset / size | Value to write | Why | Source |
|---|---|---|---|---|
| whole struct | 0x000 / 4096 | **zero it first** | If `sentinel` (0x1EF) is non-zero, `sanitize_boot_params` zeroes every field not on its preserve list, and `acpi_rsdp_addr` is **not** on that list. | bp:116-160; arch/x86/include/asm/bootparam_utils.h:37-83 |
| `acpi_rsdp_addr` | 0x070 / 8 | RSDP physical address, when ACPI tables are provided | first place Linux looks (§3) | zero-page.rst:22; bp:122 |
| `ext_ramdisk_image/size`, `ext_cmd_line_ptr` | 0x0C0, 0x0C4, 0x0C8 / 4 | high 32 bits | >4 GiB initrd or cmdline | zero-page.rst:28-30 |
| `e820_entries` | 0x1E8 / 1 | ≤ 128 | | bp:108, 137 |
| `e820_table[128]` | 0x2D0 / 20 each | `{u64 addr; u64 size; u32 type}`, packed (RAM 1, RESERVED 2, ACPI 3, NVS 4, UNUSABLE 5) | An empty table makes the kernel fake a 2-range map from `alt_mem_k`/`ext_mem_k`. More than 128 entries go in a `SETUP_E820_EXT` (1) `setup_data` node. | bp:159; arch/x86/include/uapi/asm/setup_data.h:7, 45-49; arch/x86/kernel/e820.c:449-470, 1234-1260 |
| `hdr.type_of_loader` | 0x210 / 1 | non-zero; 0xFF = "undefined" | **The initrd is ignored when this is 0** (`reserve_initrd`, `early_reserve_initrd`). "write (obligatory)". | boot:412-421; arch/x86/kernel/setup.c:355-357, 369-371 |
| `hdr.loadflags` | 0x211 / 1 | `LOADED_HIGH` (1). Never set `KASLR_FLAG` (1<<1, "kernel internal"): it turns on `kaslr_enabled()` memory-region randomization. | | boot:463-504; bp:13-17; arch/x86/include/asm/setup.h:85-89 |
| `hdr.ramdisk_image/size` | 0x218, 0x21C / 4 | initrd physical address and size, or 0 | | boot:544-562; setup.c:297-371 |
| `hdr.cmd_line_ptr` | 0x228 / 4 | command-line physical address | Copied only when non-zero. The kernel `memcpy`s **`COMMAND_LINE_SIZE` = 2048 bytes** from it whatever the string length, so the whole 2 KiB must be mapped. "If this field is left at zero, the kernel will assume that your boot loader does not support the 2.02+ protocol." | boot:616-632; arch/x86/kernel/head64.c:185-211; arch/x86/include/asm/setup.h:7 |
| `hdr.version` | 0x206 / 2 | e.g. 0x020C, as PVH writes | `x86_64_start_reservations` re-runs `copy_bootdata` when it is 0. | head64.c:295-299; arch/x86/platform/pvh/enlighten.c:90 |
| `hdr.hardware_subarch` | 0x23C / 4 | 0 (`X86_SUBARCH_PC`) | | head64.c:302-308; bp:191-205 |
| `hdr.setup_data` | 0x250 / 8 | 0, or a linked list (`SETUP_E820_EXT` 1, `SETUP_RNG_SEED` 9, …) | | bp; setup_data.h:7-15 |
| `hdr.boot_flag` 0xAA55, `hdr.header` "HdrS", `hdr.kernel_alignment` | 0x1FE, 0x202, 0x230 | optional | "read" fields in boot.rst. No read found in `arch/x86/kernel`, `arch/x86/mm` or `arch/x86/boot/startup` (grep), so they are cosmetic for a raw vmlinux. | boot:331-359, 648-663 |

### 2.3 PVH entry

| Item | Detail | Source |
|---|---|---|
| Discovery | ELF note name "Xen", type **18** `XEN_ELFNOTE_PHYS32_ENTRY`: the "32bit entry point … in 32bit protected mode with paging disabled". Linux emits it as `_ASM_PTR`, so **the desc is 8 bytes on x86_64** (4 on i386). Accept both. Its value is the physical address of `pvh_start_xen`. | include/xen/interface/elfnote.h:189-197; arch/x86/platform/pvh/head.S:312-313; arch/x86/kernel/vmlinux.lds.S:532-535 |
| Relocation note | type **19** `XEN_ELFNOTE_PHYS32_RELOC`: up to three u32 {alignment, min start, max end}. Linux writes {`CONFIG_PHYSICAL_ALIGN`, `LOAD_PHYSICAL_ADDR`, `KERNEL_IMAGE_SIZE − 1`}. When loaded at an offset, the entry code fixes its own page tables. | elfnote.h:199-215; head.S:112-160, 306-309 |
| CPU state at entry | `ebx` = physical address of `hvm_start_info`. CR0: PE=1, all other writable bits clear. CR4 = 0. CS: 32-bit execute/read, base 0, limit 0xFFFF_FFFF, selector unspecified. DS, ES: 32-bit read/write, base 0, limit 4 GiB. TR: 32-bit TSS (active), base 0, limit 0x67. EFLAGS: VM(17), IF(9), TF(8) clear. "All other processor registers and flag bits are unspecified. The OS is in charge of setting up its own stack, GDT and IDT." | head.S:29-48 |
| What the kernel does | Loads its own GDT (entry 1 = CS, entry 2 = DS). Copies `hvm_start_info` into `.init.data`. Sets CR4.PAE and EFER.LME. Enables paging on prebuilt tables `pvh_init_top_pgt`, which **identity-map and direct-map only the first 1 GiB** (512 × 2 MiB) plus the kernel high map. Far-returns to 64-bit, calls `xen_prepare_pvh()`, then jumps to `startup_64` with `%rsi = &pvh_bootparams`. | head.S:57-196, 258-304 |
| `xen_prepare_pvh()` checks | `magic` must be `XEN_HVM_START_MAGIC_VALUE` 0x336EC578, else `BUG()`. On a non-Xen host **`version` ≥ 1 and `memmap_entries` > 0 are mandatory**, else `BUG()`. | arch/x86/platform/pvh/enlighten.c:43-60, 116-126; include/xen/interface/hvm/start_info.h:86 |
| What it builds | memmap → `e820_table`, plus a RESERVED entry for 0xA0000-0x100000 (ISA hole) that it appends itself. `cmd_line_ptr = cmdline_paddr`. Module 0 = initrd (`ramdisk_image/size`). `hdr.version = 0x020C`. `type_of_loader = 0xB0` (0x90 on Xen). `acpi_rsdp_addr = rsdp_paddr`. | enlighten.c:41-94 |
| Hidden cost | `xen_prepare_pvh()` always calls `xen_cpuid_base()`, which scans CPUID 0x4000_0000…0x4000_FF00 in 0x100 steps (**256 CPUID executions**) for "XenVMMXenVMM". On KVM it never matches. KVM intercepts CPUID on both VMX and SVM, so each is a VM exit handled in the kernel. | enlighten.c:119-120; arch/x86/include/asm/xen/hypervisor.h:45-48; arch/x86/include/asm/cpuid/api.h:192-213; vmx.c:6325; svm.c:1165 |
| Placement constraints (Derived) | `hvm_start_info` is read in 32-bit non-paged mode, so it must be below 4 GiB. The memmap and modlist are read through `__va()` while only the first 1 GiB is mapped, so they must be **below 1 GiB**. The command line and initrd are read later, under the kernel's own page tables. | from head.S:258-281; enlighten.c:47, 78-79 |
| Kconfig | `CONFIG_PVH` ("Support for running PVH guests") is opt-in and absent from upstream `x86_64_defconfig`. | arch/x86/Kconfig:848-852; arch/x86/configs/x86_64_defconfig (no `PVH` line) |

`struct hvm_start_info` (56 B; PROBE, matches the byte diagram at start_info.h:15-42):

| Field | Offset | Type | Value for shards |
|---|---|---|---|
| `magic` | 0 | u32 | 0x336EC578 |
| `version` | 4 | u32 | 1 |
| `flags` | 8 | u32 | 0 |
| `nr_modules` | 12 | u32 | 1 with an initrd, else 0 |
| `modlist_paddr` | 16 | u64 | → `hvm_modlist_entry[]` (32 B: `paddr`, `size`, `cmdline_paddr`, `reserved`) |
| `cmdline_paddr` | 24 | u64 | NUL-terminated command line |
| `rsdp_paddr` | 32 | u64 | RSDP, or 0 without ACPI |
| `memmap_paddr` | 40 | u64 | → `hvm_memmap_table_entry[]` (24 B: `u64 addr; u64 size; u32 type; u32 reserved`) |
| `memmap_entries` | 48 | u32 | > 0 |
| `reserved` | 52 | u32 | 0 |

Memmap types: RAM 1, RESERVED 2, ACPI 3, NVS 4, UNUSABLE 5, DISABLED 6, PMEM 7 (start_info.h:94-100). Structs: start_info.h:108-140.

### 2.4 What Firecracker does (details in §1.8)

| Case | Protocol | Source |
|---|---|---|
| ELF `vmlinux` with a `PHYS32_ENTRY` note | **Always PVH. There is no switch.** | fc:src/vmm/src/arch/x86_64/mod.rs:514-562; fc:docs/pvh.md:3-11 |
| ELF without the note | 64-bit protocol at `e_entry` | same |
| bzImage | 64-bit protocol at load address + 0x200 (needs `version` ≥ 0x020C and `XLF_KERNEL_64`) | same; ll:src/loader/bzimage/mod.rs:105-176 |
| Firecracker's CI kernel configs | All of them set `CONFIG_PVH=y`, so every CI `vmlinux` boots via PVH. Only bzImages exercise the ELF 64-bit path. | fc:resources/guest_configs/microvm-kernel-ci-x86_64-6.18.config:344; …-6.1.config:335; …-5.10.config:325 |
| RSDP | ACPI tables are written for both protocols, with the RSDP at 0xE0000. The zero page gets `acpi_rsdp_addr = 0xE0000`, but `hvm_start_info.rsdp_paddr` is **left 0**, so a PVH guest finds the RSDP by the legacy BIOS-area scan (§3). | fc:src/vmm/src/arch/x86_64/mod.rs:305-395, 427-430; fc:src/vmm/src/arch/x86_64/layout.rs:63-64 |
| CR0 on the 64-bit path | Firecracker ORs PE and PG into KVM's reset CR0, which has CD\|NW\|ET set, so the guest enters with **CR0 = 0xE000_0011 (caches disabled)** until `common_startup_64` writes `CR0_STATE`. PVH assigns CR0 = PE\|ET (0x11). | fc:src/vmm/src/arch/x86_64/regs.rs:243-252, 278-280; x86.c:13055-13065; head_64.S:403-406 |

### 2.5 Derived: which protocol shards should use

| Criterion | 64-bit protocol at `e_entry` | PVH |
|---|---|---|
| Kernel-config independence | Any x86_64 `vmlinux` (§2.1) | Needs `CONFIG_PVH`, which upstream `x86_64_defconfig` lacks (§2.3) |
| Guest work before `startup_64` | none | Builds its own page tables and executes **256 CPUID exits** in `xen_cpuid_base()` (§2.3) |
| VMM work | 3 page-table pages (identity map of [0, 1 GiB) with 2 MiB pages), a 4-entry GDT and a 4 KiB zero page with e820. All VMM memory writes, microseconds. | 56 B start_info + memmap + modlist; 32-bit protected-mode SREGS |
| ACPI hand-off | `boot_params.acpi_rsdp_addr` | `rsdp_paddr` |
| Parity with Firecracker's boots of its own CI kernel | no (Firecracker uses PVH there) | yes |
| Request path (D2: snapshot restore) | not involved | not involved |

Derived decision: **boot the uncompressed `vmlinux` through the 64-bit boot protocol at `e_entry`.** It works on every x86_64 vmlinux, and it skips PVH's 256 CPUID exits and in-guest page-table setup. Only template builds and cold boots pay for either protocol, never a request. Set CR0 to PE|PG|ET (0x8000_0011) outright; do not OR into KVM's reset value. Use the boot.rst selectors, `__BOOT_CS` 0x10 and `__BOOT_DS` 0x18. Put the RSDP in `acpi_rsdp_addr` so the guest skips the BIOS-area scan. Keep PVH as the documented alternative. Add it only if a committed benchmark on the Firecracker CI kernel shows it faster, since Firecracker's own numbers for that kernel come from PVH.

### Section 2 bug-magnets

1. **`e_entry` is physical** and points into `.init.text`. The PT_LOADs must go at `p_paddr`, not `p_vaddr`, and not at an offset from the file start.
2. **Relocating the image by anything other than a 2 MiB multiple hangs silently** in `__startup_64` (`for(;;)`), with no console output.
3. **Zero the whole zero page.** A non-zero `sentinel` wipes `acpi_rsdp_addr`. Also set `type_of_loader` non-zero, or the initrd is silently ignored.
4. **The kernel copies 2048 bytes from `cmd_line_ptr`**, so the whole 2 KiB behind the string must be mapped RAM.
5. **PVH on KVM needs `version` = 1 and a non-empty memmap**, or the guest `BUG()`s. The entry note desc is 8 bytes on x86_64.
6. **PVH's early page tables map only 1 GiB.** `hvm_start_info`, the memmap and the modlist must sit below it.
7. **Never inherit KVM's reset CR0** (CD|NW set). Build every SREGS field explicitly.

---

## 3. Firmware tables and device discovery

### 3.1 How Linux x86_64 discovers CPUs, interrupt routing and ACPI

| Item | Behaviour | Source |
|---|---|---|
| MP floating-pointer scan | Scans in 16-byte steps over [0, 0x400), [0x9FC00, 0xA0000) ("top 1K of base RAM") and [0xF0000, 0x100000). Then it scans the first 1 KiB of the EBDA, whose real-mode segment is at 0x40E (0 means none). A pointer is accepted with `_MP_`, length 1, a 16-byte sum of 0 and spec 1 or 4. It is then memblock-reserved. | arch/x86/kernel/mpparse.c:557-636; arch/x86/include/asm/bios_ebda.h:10-19 |
| MP config table | Requires `PCMP`, a zero sum over `length`, spec 1 or 4, and a non-zero LAPIC address. `feature1` ≠ 0 selects a built-in default configuration instead of the table. | arch/x86/kernel/mpparse.c:140-170, 517-529 |
| Entries consumed | PROCESSOR (skipped if the MADT already listed LAPICs), BUS, IOAPIC (only with `MPC_APIC_USABLE`), INTSRC (saved as routing). LINTSRC is only printed. | arch/x86/kernel/mpparse.c:104-113, 129-135, 197-238 |
| Layouts | `mpf_intel` 16 B, `mpc_table` 44 B, `mpc_cpu` 20 B; `mpc_bus`, `mpc_ioapic`, `mpc_intsrc` and `mpc_lintsrc` 8 B each | arch/x86/include/asm/mpspec_def.h:15-153 (sizes derived) |
| MP-table IRQ semantics | `irqflag` 0 on an ISA bus means active-high, edge. An IOAPIC from the MP table gets the LEGACY IRQ domain, where Linux IRQ number = GSI. | arch/x86/kernel/apic/io_apic.c:673-686, 737-751, 882-898; arch/x86/kernel/mpparse.c:104-113 |
| ACPI precedence | The MP table is always searched, but it isn't parsed once the MADT has supplied both LAPICs and an IOAPIC. | arch/x86/kernel/setup.c:1064, 1187, 1230; arch/x86/kernel/mpparse.c:487-495 |
| `CONFIG_X86_MPPARSE` | The prompt exists only `if ACPI`, so without ACPI the option is forced on. When it is off, the find/parse hooks are no-ops. | arch/x86/Kconfig:507-513; arch/x86/include/asm/mpspec.h:50-62 |
| Neither MP table nor MADT | IOAPIC support is disabled and the APIC falls back to virtual wire ("APIC: ACPI MADT or MP tables are not detected"). The guest gets one CPU and no IOAPIC GSIs. | arch/x86/kernel/apic/apic.c:1287-1293 |
| RSDP lookup order | 1. `acpi_rsdp=` (kexec builds). 2. `x86_init.acpi.get_root_pointer`, i.e. `boot_params.acpi_rsdp_addr` (zero page +0x070). 3. EFI. 4. Legacy scan (x86 selects `ACPI_LEGACY_TABLES_LOOKUP`). | drivers/acpi/osl.c:192-226; arch/x86/kernel/acpi/boot.c:1831-1839; arch/x86/kernel/x86_init.c:127-128; bp:122; arch/x86/Kconfig:63 |
| Legacy RSDP scan | 16-byte steps. First the EBDA's first 1 KiB, used only if 0x400 < base < 0xA0000. Then 0xE0000-0xFFFFF. | drivers/acpi/acpica/tbxfroot.c:112-215; include/acpi/acconfig.h:150-155; ACPI6.5 §5.2.5.1 |
| PVH hand-off | `rsdp_paddr` becomes `boot_params.acpi_rsdp_addr`. The memmap is copied into e820, and Linux appends [0xA0000, 0x100000) as reserved. | arch/x86/platform/pvh/enlighten.c:41-93; arch/x86/include/asm/e820/types.h:93-97 |
| HW-reduced detection | FADT flags bit 20 sets `acpi_gbl_reduced_hardware` | drivers/acpi/acpica/tbfadt.c:377-379; include/acpi/actbl.h:296; ACPI6.5 Table 5.10 |
| HW-reduced effects (x86) | `early_acpi_boot_init` sets `timer_init` and `pre_vector_init` to no-ops and `legacy_pic` to null (so `nr_legacy_irqs()` = 0). This happens before IOAPICs are parsed. Consequences: no `check_timer()`, no SCI setup, no ISA IRQ 0-15 identity map, and the MADT IOAPIC gets the **DYNAMIC** domain. Linux IRQ numbers are then allocated upward from `gsi_top` and **do not equal GSIs**. | arch/x86/kernel/acpi/boot.c:1407-1422, 1608-1638, 1664, 469-486, 1131, 1231; arch/x86/kernel/apic/io_apic.c:2279-2292, 2253-2254, 2356-2373 |
| MADT | LAPIC entries set `acpi_lapic`. IOAPIC entries set `smp_found_config`. `PCAT_COMPAT` (flags bit 0) calls `legacy_pic_pcat_compat()`. | arch/x86/kernel/acpi/boot.c:149-150, 1256-1305; ACPI6.5 Table 5.20 |
| FADT IAPC_BOOT_ARCH | 8042 bit clear (FADT rev ≥ 2): i8042 becomes `FIRMWARE_ABSENT`, and the driver then returns -ENODEV unless PNP finds a PS/2 controller. `NO_CMOS_RTC`: no RTC platform device. `NO_VGA`: no VGA probe. | arch/x86/kernel/acpi/boot.c:974-996; drivers/input/serio/i8042-acpipnpio.h:1630-1645; ACPI6.5 Table 5.11 |
| HW-reduced FADT fields | OSPM ignores offsets 46-108 and 148-232 and flag bits 1-3, 7, 8, 13, 14, 16, 17 | ACPI6.5 §5.2.9 (Note) |

### 3.2 Firecracker's MP table (written on every x86_64 boot)

| Item | Value | Source |
|---|---|---|
| Placement | It is the first `system_memory` allocation (first fit, 1-byte aligned). The floating pointer lands at **0x9FC00**, inside Linux's top-1K window, and the config table follows at +16. | fc:src/vmm/src/arch/x86_64/mptable.rs:125-174; fc:src/vmm/src/vstate/resources.rs:88-89; fc:src/vmm/src/arch/x86_64/layout.rs:66-68 |
| Floating pointer | `_MP_`, physptr = table, length 1, spec 4, checksum; feature bytes 0 | fc:src/vmm/src/arch/x86_64/mptable.rs:155-169 |
| Header | `PCMP`, spec 4, OEM `FC`, LAPIC 0xFEE00000, checksum over header plus entries | fc:src/vmm/src/arch/x86_64/mptable.rs:79-86, 284-310 |
| CPUs | One entry per vCPU: apicid = index, apicver 0x14, ENABLED (plus BSP on vCPU 0), signature 0x600, features APIC\|FPU. Maximum 254 (the IOAPIC takes an APIC ID). Firecracker itself caps vCPUs at 32. | fc:src/vmm/src/arch/x86_64/mptable.rs:68-71, 87-90, 176-199; fc:src/vmm/src/vmm_config/machine_config.rs:13 |
| Bus, IOAPIC | ISA bus 0. IOAPIC id = vCPUs + 1, version 0x14, USABLE, at 0xFEC00000. | fc:src/vmm/src/arch/x86_64/mptable.rs:140, 200-227 |
| INTSRC | 24 entries: ISA IRQ i to IOAPIC pin i, `irqflag` 0 (edge, active-high). The code comment cites KVM's default routing (GSI 0-15 to PIC and IOAPIC, 16-23 to IOAPIC only). | fc:src/vmm/src/arch/x86_64/mptable.rs:228-245; arch/x86/kvm/irq.c:561-574 |
| LINTSRC | ExtINT to LINT0 of APIC 0; NMI to LINT1 of all APICs (0xFF) | fc:src/vmm/src/arch/x86_64/mptable.rs:246-279 |
| Size | 284 + 20·vCPUs bytes | fc:src/vmm/src/arch/x86_64/mptable.rs:105-113 |

### 3.3 Firecracker's ACPI tables (x86_64, written on every boot since v1.8.0)

| Item | Contents | Source |
|---|---|---|
| Both, always | `configure_system_for_boot` writes the MP table, then the zero page or PVH info, then the ACPI tables, with no conditions | fc:src/vmm/src/arch/x86_64/mod.rs:271-301 |
| RSDP | Fixed at **0xE0000**, the start of the legacy scan window. Revision 2; points to the XSDT only (RSDT = 0); both checksums set. | fc:src/vmm/src/acpi/mod.rs:164-180; fc:src/vmm/src/acpi/x86_64.rs:45-47; fc:src/acpi-tables/src/rsdp.rs:38-56 |
| RSDP hand-off | Linux boot: `boot_params.acpi_rsdp_addr` = 0xE0000. PVH: `rsdp_paddr` stays 0, so the guest falls back to the scan. | fc:src/vmm/src/arch/x86_64/mod.rs:427-432, 368-376 |
| XSDT | Points to FADT, MADT and MCFG | fc:src/vmm/src/acpi/mod.rs:135-152, 187-201 |
| FADT | Revision 6, minor 5. Flags: HW_REDUCED_ACPI (20), PWR_BUTTON (4), SLP_BUTTON (5), meaning control-method buttons, none present. IAPC_BOOT_ARCH has only VGA Not Present; 8042 and LEGACY_DEVICES are clear. X_DSDT set. Hypervisor vendor `FIRECKVM`. RESET_REG, SLEEP_CONTROL_REG, SLEEP_STATUS_REG and FACS are left zero. | fc:src/vmm/src/acpi/mod.rs:29-31, 99-115; fc:src/vmm/src/acpi/x86_64.rs:27-34; fc:src/acpi-tables/src/fadt.rs:12-32, 44-126 |
| MADT | LAPIC base 0xFEE00000; flags 0 (no PCAT_COMPAT). One Local APIC entry per vCPU (uid = APIC id = i, enabled) and one IOAPIC (id 0, 0xFEC00000, GSI base 0). No interrupt-source overrides. | fc:src/vmm/src/acpi/mod.rs:117-133; fc:src/vmm/src/acpi/x86_64.rs:15-25; fc:src/acpi-tables/src/madt.rs:14-125 |
| DSDT | In order: virtio devices; VMGenID (`VMGENCTR`); VMClock (`AMZNC10C`); GED (`ACPI0013`, whose `_EVT` notifies both); COM1 (`PNP0501`, 0x3F8/8, GSI 4); PS/2 (`PNP0303`, ports 0x60 and 0x64, GSI 1). A PCI root (`PNP0A08`) appears only with `--enable-pci`. **No `_S5`**: the only one is in a unit test. | fc:src/vmm/src/acpi/mod.rs:81-97; fc:src/vmm/src/device_manager/mod.rs:321-331; fc:src/vmm/src/device_manager/acpi.rs:101-152; fc:src/vmm/src/device_manager/legacy.rs:87-142; fc:src/vmm/src/devices/pci/pci_segment.rs:240; fc:src/acpi-tables/src/aml.rs:1188, 1510-1516 |
| `_CRS` IRQs | Every one is an Extended IRQ descriptor: consumer, edge, active-high, exclusive | fc:src/acpi-tables/src/aml.rs:570-600 |
| MCFG | Segment 0, buses 0-0, ECAM base 0xEEC00000. Written even when PCI is off. | fc:src/vmm/src/acpi/mod.rs:154-162, 198; fc:src/acpi-tables/src/mcfg.rs:17-23, 34-59 |
| Placement | Tables go first-fit, byte-aligned, after the MP table within [0x9FC00, 0xE0000). VMGenID (16 B) and VMClock (one 4 KiB page) are allocated from the top of the region. | fc:src/vmm/src/acpi/mod.rs:55-79; fc:src/vmm/src/devices/acpi/vmgenid.rs:78-90; fc:src/vmm/src/devices/acpi/vmclock.rs:77-90 |
| Policy | ACPI arrived in 1.8.0. MPTable and cmdline virtio devices are deprecated and will be removed. The docs say ACPI boot needs `CONFIG_ACPI=y` and `CONFIG_PCI=y` ("needed for ACPI initialization"). `pci=off` is appended when PCI is disabled. | fc:CHANGELOG.md:742-751, 795-799; fc:DEPRECATED.md:19-21; fc:docs/kernel-policy.md:124-154, 205-207; fc:src/vmm/src/builder.rs:217-219 |
| CI kernel (x86_64, 6.18.48) | `X86_MPPARSE` off. `VIRTIO_MMIO_CMDLINE_DEVICES` off. `ACPI`, `PCI`, `PNPACPI` and `SERIAL_8250_PNP` on. `ACPI_REDUCED_HARDWARE_ONLY` off. `OF` off. | ART:cfg:351, 550, 584, 1655, 1824, 1833, 2149, 2562-2563 |

### 3.4 virtio-mmio discovery

| Mechanism | Detail | Source |
|---|---|---|
| Kernel cmdline | `[virtio_mmio.]device=<size>@<baseaddr>:<irq>[:<id>]`. The size takes K/M/G suffixes; irq 0 is rejected. Each one creates platform device `virtio-mmio.N` and needs `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES`. It is a level-6 param, parsed just before the device initcalls. **`<irq>` is a Linux IRQ number, not a GSI.** | drivers/virtio/virtio_mmio.c:39-50, 663-756; include/linux/moduleparam.h:272-273; init/main.c:1397-1408 |
| ACPI | `_HID "LNRO0005"`. ACPI devices are created in `acpi_init` (subsys_initcall), before the driver registers (`module_init`). | drivers/virtio/virtio_mmio.c:793-806, 824; drivers/acpi/bus.c:1616 |
| Device tree | `"virtio,mmio"`. DT on x86 needs `CONFIG_OF`, which is off in the CI kernel. | drivers/virtio/virtio_mmio.c:787; arch/x86/kernel/devicetree.c:339-364; ART:cfg:1824 |
| Probe | Maps resource 0 and checks magic `virt` and version 1-2 | drivers/virtio/virtio_mmio.c:587-606 |
| Firecracker slots | 4 KiB per device (`MMIO_LEN`), allocated first-fit from 0xC0001000. GSIs come from the legacy pool 5-23; 0-4 are kept free. | fc:src/vmm/src/device_manager/mmio.rs:59-64, 164-186; fc:src/vmm/src/arch/x86_64/layout.rs:24-32, 114-120 |
| Firecracker describes each device twice | On x86 it appends `virtio_mmio.device=4K@0x…:<gsi>` **and** a DSDT `V###` node (`_UID` = gsi − 5, `_CCA` = 1, `Memory32Fixed` + IRQ). Its docs say the guest initializes the device twice, the second attempt fails with dmesg warnings, and the device still works. | fc:src/vmm/src/device_manager/mmio.rs:77-109, 231-282; ll:src/cmdline/mod.rs:431-452; fc:docs/kernel-policy.md:216-221 |
| Order | DSDT bytes are appended in attach order, so the root block device becomes `/dev/vda` | fc:src/vmm/src/device_manager/mmio.rs:127-135; fc:src/vmm/src/builder.rs:228-290 |
| Wiring | One irqfd on the device's GSI; an ioeventfd on QueueNotify | fc:src/vmm/src/device_manager/mmio.rs:204-213 |

### 3.5 Derived: what shards should emit

- **The bootstrap kernel requires ACPI.** Its config has `X86_MPPARSE` and `VIRTIO_MMIO_CMDLINE_DEVICES` off (ART:cfg:351, 2563). It sees CPUs and the IOAPIC only through the MADT and virtio devices only through the DSDT. With no tables it runs on one CPU with no IOAPIC (arch/x86/kernel/apic/apic.c:1287-1293).
- **Tables to emit:** RSDP at 0xE0000, XSDT, FADT (HW-reduced, rev 6.5), MADT and DSDT. Add MCFG plus an e820-reserved ECAM only with virtio-pci: Linux refuses an ECAM that isn't reserved (arch/x86/pci/mmconfig-shared.c:471-480, 543-550).
- **Hand over the RSDP explicitly** on both paths: `boot_params.acpi_rsdp_addr` and `hvm_start_info.rsdp_paddr`. This skips the 128 KiB scan that Firecracker's PVH path triggers.
- **Describe each virtio-mmio device once, in the DSDT.** Under HW-reduced ACPI a cmdline `<irq>` no longer names the GSI (DYNAMIC IOAPIC domain, §3.1).
- **Give every emulated legacy device a DSDT node:** `PNP0501` for COM1 (GSI 4), and `PNP0303` if an i8042 is emulated. Two reasons:
  - HW-reduced mode drops the ISA identity IRQs.
  - With the FADT 8042 bit clear, the i8042 driver depends on PNP.

  The CI kernel has `SERIAL_8250_PNP` and `PNPACPI` (ART:cfg:2149, 1833).
- **IOAPIC and vCPU IDs:**
  - Use IOAPIC id 0. That is KVM's reset value (arch/x86/kvm/ioapic.c:707), and the x86_64 `setup_ioapic_ids` hook is a no-op (arch/x86/kernel/x86_init.c:78).
  - Type-0 MADT LAPIC entries hold 8-bit IDs (fc:src/acpi-tables/src/madt.rs:22-28). Stay at ≤ 254 vCPUs or add x2APIC entries.
- **Keep an MP-table plus cmdline writer only for a future ACPI-less tuned kernel.**
  - `CONFIG_ACPI=n` forces `X86_MPPARSE` on (arch/x86/Kconfig:507-510), and on that path cmdline IRQ = GSI (LEGACY domain).
  - Decide between the two by measured boot time. The only evidence so far is the ≈30% "ACPI scan + PCI enumeration + rootfs population" share of a vanilla QEMU boot [boot-latency.md, Wanninger22 §4.2]. No microVM measurement exists.
- **Notes for §4 and §5:**
  - Firecracker's FADT has no sleep or reset register and its DSDT has no `_S5`, so it offers no ACPI power-off (fc:src/acpi-tables/src/fadt.rs:87, 103 are never set).
  - HW-reduced mode makes `timer_init` a no-op (arch/x86/kernel/acpi/boot.c:1413), which bears on whether a PIT is needed at all.

### Section 3 bug-magnets

1. The CI kernel ignores an MP table and `virtio_mmio.device=` without any warning. The symptoms are one CPU and no root disk.
2. HW-reduced ACPI changes IRQ numbering so that Linux IRQ ≠ GSI. Never mix cmdline IRQs with ACPI.
3. With the FADT 8042 bit clear, `i8042.nopnp` or a missing `PNP0303` disables the i8042 driver. Firecracker fixed Ctrl-Alt-Del by dropping `i8042.nopnp` (fc:CHANGELOG.md:563-564).
4. An MP floating pointer outside a scan window is invisible. The EBDA at 0x40E is 0 in a zeroed guest, so it is never scanned. Use 0x9FC00 or 0xF0000-0xFFFFF.
5. Every structure needs a zero byte-sum:
   - MP pointer: 16 B
   - MP table: `length` bytes
   - RSDP: the first 20 B, and all 36 B
   - every SDT: its whole `length`
6. DSDT device order fixes the `/dev/vdX` names.
7. The RSDP sits in [0xE0000, 0x100000). Firecracker lists that range in no e820 entry. Linux removes [0xA0000, 1 MiB) from RAM anyway (arch/x86/kernel/setup.c:770).

---

## 4. Power-off and reset paths

x86 has no PSCI. A guest can reach the VMM only through a device access (PIO/MMIO exit) or a CPU event that KVM reports (`KVM_EXIT_SHUTDOWN`). A plain Linux KVM guest never produces `KVM_EXIT_SYSTEM_EVENT` on x86 (§4.3).

### 4.1 Guest side: what `reboot(2)` does on x86 (Linux 7.2-rc4)

| Step | Behaviour | Source |
|---|---|---|
| Commands | `LINUX_REBOOT_CMD_RESTART` 0x01234567, `HALT` 0xCDEF0123, `POWER_OFF` 0x4321FEDC (libc `RB_AUTOBOOT`, `RB_HALT_SYSTEM`, `RB_POWER_OFF`) | include/uapi/linux/reboot.h:29-33 |
| Power-off with no handler | If `!kernel_can_power_off()`, `POWER_OFF` is rewritten to `HALT` (`poweroff_fallback_to_halt`). `kernel_can_power_off()` is true only if a sys-off handler is registered or legacy `pm_power_off` is set. | kernel/reboot.c:693-697, 759-761 |
| HALT | `kernel_halt()` prints "Power off not available: System halted instead", then `machine_halt()`; `sys_reboot` then calls `do_exit(0)` | kernel/reboot.c:315-326, 778-780 |
| POWER_OFF (handler present) | `kernel_power_off()` → `machine_power_off()` → `native_machine_power_off()`: `machine_shutdown()` unless `reboot=f`, then `do_kernel_power_off()` runs the sys-off chain. If that returns, `do_exit(0)`. | kernel/reboot.c:705-716, 782-784; arch/x86/kernel/reboot.c:749-758 |
| After `do_exit(0)` in PID 1 | `panic("Attempted to kill init!")` | kernel/exit.c:962-963 |
| `native_machine_halt` | `machine_shutdown()` (IO-APIC clear, `stop_other_cpus()`, LAPIC shutdown), then `stop_this_cpu()` | arch/x86/kernel/reboot.c:670-719, 739-747 |
| `stop_this_cpu` | `local_irq_disable()`, `disable_local_APIC()`, then `for (;;) native_halt();` with IF=0 | arch/x86/kernel/process.c:822-867 |
| Other vCPUs | `stop_other_cpus()` sends `REBOOT_VECTOR` and waits up to 1 s (`USEC_PER_SEC` × `udelay(1)`) for them to park in the same HLT loop | arch/x86/kernel/smp.c:195-205 |
| RESTART | `kernel_restart()` → `native_machine_restart()`: `machine_shutdown()` unless forced, `do_kernel_restart()`, then `__machine_emergency_restart(0)` | kernel/reboot.c:287-299, 766-767; arch/x86/kernel/reboot.c:727-737 |
| `reboot_type` default | `BOOT_ACPI` (`'a'`). `reboot_init` runs DMI quirks only when no `reboot=` was given; it forces `BOOT_EFI` only on HW-reduced ACPI with EFI runtime (the non-EFI stub returns false) | kernel/reboot.c:48-50; arch/x86/kernel/reboot.c:497-519; arch/x86/platform/efi/quirks.c:649-656; arch/x86/include/asm/efi.h:383-386 |
| `reboot=` letters | `b` BIOS, `a` ACPI, `k` KBD, `t` TRIPLE, `e` EFI, `p` CF9_FORCE set `reboot_type`; `f` sets `reboot_force` (skip `machine_shutdown`); any `reboot=` clears `reboot_default` (no DMI quirks) | kernel/reboot.c:1097-1176; include/linux/reboot.h:28-36 |
| Emergency-restart state machine | `BOOT_ACPI`: `acpi_reboot()` then KBD. `BOOT_KBD`: 10 × {`kb_wait()`; `outb(0xfe, 0x64)`}, then ACPI once more (if the original type was ACPI) else EFI. EFI → BIOS (`machine_real_restart`, writes CMOS 0x8F) → CF9 (port 0xCF9) → TRIPLE (`idt_invalidate()` + `int3`) → KBD, forever | arch/x86/kernel/reboot.c:580-668, 99-133 |
| `kb_wait()` | Polls `inb(0x64) & 0x02` (input-buffer full) up to 0x10000 × 2 µs | arch/x86/kernel/reboot.c:522-531 |
| `acpi_reboot()` | No-op unless ACPI is enabled, FADT revision ≥ 2, and FADT flag `RESET_REG_SUP` (bit 10) is set; then writes `RESET_VALUE` to `RESET_REG` | drivers/acpi/reboot.c:36-80; include/acpi/actbl.h:286 |
| `panic=N` | N > 0: busy `mdelay` loop for N s, then `emergency_restart()`. N < 0: restart immediately. N = 0: spin forever. The console is flushed first. | kernel/panic.c:709, 719-744, 771-778; Documentation/admin-guide/kernel-parameters.txt:4813-4817 |
| `CONFIG_PANIC_TIMEOUT` | 0 in the Firecracker CI kernel, so without `panic=` a guest panic spins forever | kernel/panic.c:74; ART:cfg:3645 |

Derived: with `reboot=k`, a guest restart (and a panic with `panic≠0`) always ends in a PIO write of 0xFE to port 0x64. Without `reboot=k` it still reaches KBD after one `acpi_reboot()` no-op, unless the FADT advertises a reset register. `kb_wait()` returns on its first read if the VMM returns bit 1 clear, so no i8042 driver is needed for the reset path.

### 4.2 KVM side: a halted guest produces no exit

| Item | Behaviour | Source |
|---|---|---|
| HLT exit handler | VMX `EXIT_REASON_HLT` and SVM `SVM_EXIT_HLT` → `kvm_emulate_halt` | arch/x86/kvm/vmx/vmx.c:6329; arch/x86/kvm/svm/svm.c:3379 |
| In-kernel LAPIC | `__kvm_emulate_halt`: with `lapic_in_kernel()` (always true after `KVM_CREATE_IRQCHIP`), set `mp_state = HALTED` and stay in the kernel. Only a userspace LAPIC gets `KVM_EXIT_HLT` (5). | arch/x86/kvm/x86.c:11742-11777; arch/x86/kvm/lapic.h:179-184; ukvm:157 |
| Blocking | `vcpu_run` → `vcpu_block` → `kvm_vcpu_halt` (halt-polling, then sleep) until an event makes the vCPU runnable | arch/x86/kvm/x86.c:11624-11690, 11692-11740; virt/kvm/kvm_main.c:78, 3643, 3721 |
| Leaving KVM_RUN | Only a signal: `-EINTR` with `exit_reason = KVM_EXIT_INTR` | kernel/entry/virt.c:5-11; include/linux/kvm_host.h:2472-2487 |

Derived: `stop_this_cpu` halts with IF=0 and the LAPIC disabled, so no interrupt can wake it. After a guest `poweroff` without a power-off handler, every vCPU thread sleeps inside `KVM_RUN` forever and the VMM sees nothing. The VMM must learn about shutdown from a device write, never from an exit.

### 4.3 Exits the VMM can receive

| Exit | Cause on x86 | Source |
|---|---|---|
| `KVM_EXIT_SHUTDOWN` (8) | Triple fault. VMX `handle_triple_fault`; `KVM_REQ_TRIPLE_FAULT`; uncorrected MCE while `MCG_STATUS` shows one in progress. SVM `shutdown_interception` first re-INITs the vCPU (clears the VMCB, `kvm_vcpu_reset`) unless SEV-ES. | ukvm:160; arch/x86/kvm/vmx/vmx.c:5562-5567, 6320; arch/x86/kvm/x86.c:11185-11194; arch/x86/kvm/svm/svm.c:2153-2180, 3385; api:4600-4610 |
| `KVM_EXIT_SYSTEM_EVENT` (24) | x86 raises it only for Hyper-V crash/reset MSRs (`KVM_SYSTEM_EVENT_CRASH` 3 / `RESET` 2), SEV-ES termination (6) and TDX fatal (7). The api doc names arm64 PSCI as the example. | ukvm:176, 387-389; arch/x86/kvm/x86.c:11232-11245; arch/x86/kvm/hyperv.c:1462-1468; arch/x86/kvm/svm/sev.c:4402, 4567; arch/x86/kvm/vmx/tdx.c:1302; api:6936-6975 |
| `KVM_EXIT_IO` write 0xFE to 0x64 | Guest i8042 reset (§4.1) | arch/x86/kernel/reboot.c:613-628 |

### 4.4 ACPI S5 (only if the VMM emits ACPI)

| Item | Rule | Source |
|---|---|---|
| Registration | `acpi_sleep_init` registers `acpi_power_off` as the `SYS_OFF_MODE_POWER_OFF` handler only if `acpi_sleep_state_supported(S5)`: `\_S5` evaluates, and on HW-reduced platforms `FADT.sleep_control.address` and `sleep_status.address` are both non-zero. Otherwise `acpi_no_s5 = true`. | drivers/acpi/sleep.c:87-95, 1103-1137 |
| Build | `sleep.o` needs `ACPI_SYSTEM_POWER_STATES_SUPPORT`, which x86 selects with ACPI | drivers/acpi/Makefile:34; arch/x86/Kconfig:64 |
| Entry | `acpi_power_off` → `acpi_enter_sleep_state(S5)`; with `acpi_gbl_reduced_hardware` → `acpi_hw_extended_sleep` | drivers/acpi/sleep.c:1094-1101; drivers/acpi/acpica/hwxfsleep.c:289-302 |
| HW-reduced sequence | Write `WAK_STS` (0x80) to SLEEP_STATUS to clear it. Write `((SLP_TYPa << 2) & 0x1C) \| 0x20` (SLP_EN) to SLEEP_CONTROL. Then poll SLEEP_STATUS forever for `WAK_STS`. | drivers/acpi/acpica/hwesleep.c:69-135; include/acpi/actbl.h:314-319 |
| `SLP_TYPa` | Element 0 of the `\_S5` package (a 1-element package gives type_a = low byte, type_b = next byte). Must be ≤ 7. | drivers/acpi/acpica/hwxface.c:335, 398-431; include/acpi/actypes.h:609; drivers/acpi/acpica/hwxfsleep.c:289-294 |
| Spec: registers | SLEEP_CONTROL_REG and SLEEP_STATUS_REG are 8-bit; SystemIO, SystemMemory or PCI config (bus 0); bit width 8, offset 0. Control: SLP_TYPx bits 4:2, SLP_EN bit 5 (write-only). Status: WAK_STS bit 7 (write 1 to clear). | ACPI6.5 §4.8.3.7 (Tables 4.19, 4.20) |
| Spec: FADT offsets | RESET_REG @116 (GAS, 12 B), RESET_VALUE @128, SLEEP_CONTROL_REG @244, SLEEP_STATUS_REG @256, Hypervisor Vendor Identity @268. Flags: RESET_REG_SUP bit 10, HW_REDUCED_ACPI bit 20. | ACPI6.5 §5.2.9 (Tables 5.9, 5.10); include/acpi/actbl.h:199-255, 286, 296 |
| Spec: `\_Sx` | Package byte 0 = value for `SLEEP_CONTROL_REG.SLP_TYP` on HW-reduced platforms; byte 1 ignored there. "If _S5 is not specified, alternative methods are used to turn-off the system." | ACPI6.5 §7.4.2 (Table 7.11), §7.1 (Table 7.1) |
| EFI fallback | `efi_poweroff_required()` is true on HW-reduced or `acpi_no_s5`, but registration also needs EFI runtime `ResetSystem`; the CI kernel has no EFI | arch/x86/platform/efi/quirks.c:658-661; drivers/firmware/efi/reboot.c:61-78; ART:cfg:456 |

### 4.5 What Firecracker does (x86_64)

| Path | Behaviour | Source |
|---|---|---|
| i8042 device | "emulates just enough to shutdown the machine". PIO 0x60-0x64 (base 0x60, size 5), GSI 1. Status starts at `SB_KBD_ENABLED` (0x10), so IBF (bit 1) is always clear. Commands: 0x20/0x60 CTR, 0xD0/0xD1 output port, **0xFE → `reset_evt.write(1)`**. Other data-port bytes are ACKed with 0xFA plus IRQ 1. Unknown commands and non-1-byte accesses are counted as missed. | fc:src/vmm/src/devices/legacy/i8042.rs:62-83, 93, 122-135, 214-245, 248-338; fc:src/vmm/src/device_manager/legacy.rs:42, 50-52, 62-66, 78-83 |
| Reset → VMM exit | `reset_evt` is a clone of `vcpus_exit_evt`. The VMM event loop drains vCPU responses and calls `stop(exit_code)`; with no vCPU error the code is `Ok`. | fc:src/vmm/src/device_manager/mod.rs:217-221; fc:src/vmm/src/lib.rs:801-829 |
| Ctrl-Alt-Del | `SendCtrlAltDel` API action queues scancodes (Ctrl 0x14, Alt 0x11, Del 0xE071) and raises IRQ 1. Needs `CONFIG_SERIO_I8042` and `CONFIG_KEYBOARD_ATKBD` in the guest; Intel/AMD only. | fc:src/vmm/src/devices/legacy/i8042.rs:86-88, 138-148; fc:docs/api_requests/actions.md:34-45 |
| `KVM_EXIT_SHUTDOWN`, `KVM_EXIT_HLT` | Not handled: they fall into `unexpected_exit` → `UnhandledKvmExit` → vCPU exits with `FcExitCode::GenericError` | fc:src/vmm/src/arch/x86_64/vcpu.rs:780-787; fc:src/vmm/src/vstate/vcpu.rs:261, 721-733 |
| `KVM_EXIT_SYSTEM_EVENT` | SHUTDOWN or RESET → `VcpuEmulation::Stopped` → exit `Ok`; others → error. The code comments this as ARM-specific; "On x86 the i8042 emulation signals the main thread directly". | fc:src/vmm/src/vstate/vcpu.rs:249-252, 486-503 |
| ACPI power-off | None. The FADT sets only HW_REDUCED_ACPI, PWR_BUTTON and SLP_BUTTON; no reset register and no sleep registers are populated, although the struct has the fields. `_S5_` appears only in an `acpi_tables` unit test. | fc:src/vmm/src/acpi/mod.rs:102-115; fc:src/acpi-tables/src/fadt.rs:87-88, 103-104; fc:src/acpi-tables/src/aml.rs:1188-1189, 1508-1519 |
| User guidance | `reboot` in the guest "will gracefully shutdown Firecracker... Firecracker doesn't implement guest power management". Default cmdline carries `reboot=k panic=1` ("shut down the guest on reboot, instead of rebooting"). | fc:docs/getting-started.md:333-338; fc:docs/kernel-policy.md:186-194; fc:src/vmm/src/vmm_config/boot_source.rs:10-20; fc:docs/design.md:112-116 |

Derived: under Firecracker a guest `poweroff` (`RB_POWER_OFF`) hangs the VM (§4.2), because no power-off handler exists. Only `reboot`, a panic with `panic=1`, or `SendCtrlAltDel` ends the VMM cleanly.

### 4.6 Derived: shards x86_64 power-off and reset

| Mechanism | Implement? | Why |
|---|---|---|
| i8042 command port: write 0xFE to 0x64 → "guest reset" → VMM stops | **Yes** | Terminal state of every restart and emergency restart (§4.1). One PIO exit. Needs only port 0x64 decoded, with reads returning bit 1 clear; no guest i8042 driver required (derived from arch/x86/kernel/reboot.c:522-531, 613-628). |
| `KVM_EXIT_SHUTDOWN` → treat as guest reset/crash, stop the VM | **Yes** (backstop) | Triple fault, `reboot=t`, or a broken guest (§4.3). Firecracker errors out instead (§4.5). |
| `KVM_EXIT_HLT` | Unreachable with the in-kernel irqchip; treat as a fatal backend bug | §4.2 |
| ACPI S5 (FADT SLEEP_CONTROL/STATUS + DSDT `\_S5`) | Only if §3 chooses ACPI | Makes `RB_POWER_OFF` work natively and lets the VMM tell power-off from reboot (§4.4). Costs FADT/DSDT generation plus guest ACPI init. |
| Guest cmdline | `reboot=k` plus `panic=-1` (not Firecracker's `panic=1`) | `reboot=k` skips the no-op ACPI attempt. `panic=-1` removes the 1 s busy wait before the reset write (kernel/panic.c:719-744). Some `panic=` is mandatory because `CONFIG_PANIC_TIMEOUT=0` spins forever. |
| shards-init | Today `power_off()` does `sync(); reboot(RB_POWER_OFF)` (the comment assumes PSCI SYSTEM_OFF), and testguest does the same. On x86 without ACPI S5 that becomes HALT → the VM hangs (§4.1-4.2). | crates/init/src/linux.rs:98-113; crates/testguest/src/linux.rs:26-30 |

Derived, for shards-init: pick one of the following.

- (a) Write an explicit "power off" marker to the shards control page (`crates/vmm/src/devices/control.rs`), then `reboot(RB_AUTOBOOT)`. The marker is arch-neutral, costs one MMIO exit, and tells power-off from reboot. The i8042 0xFE write is the fallback.
- (b) Emit ACPI S5 and keep `RB_POWER_OFF`.

Either way, restart and panic paths from other guest code still end at i8042 0xFE, so the device is required. Host-initiated graceful stop goes over vsock (D12), so no keyboard or Ctrl-Alt-Del emulation is needed.

### Section 4 bug-magnets

1. **`RB_POWER_OFF` silently becomes HALT** when no power-off handler is registered. With an in-kernel LAPIC the halted vCPUs never exit `KVM_RUN`, so the VM looks alive forever. Every x86 shutdown design must end in a device write.
2. **Reset and power-off both look like i8042 0xFE** unless ACPI S5 or a control-page marker distinguishes them.
3. **`CONFIG_PANIC_TIMEOUT=0` in the CI kernel.** Without `panic=` on the cmdline, a panic spins a vCPU at 100 % with no exit.
4. **The VMM must kick all vCPU threads** (signal + `immediate_exit`) after the reset write. The other vCPUs are already parked in-kernel by `stop_other_cpus()` and will not return on their own.
5. **SVM re-INITs the vCPU before reporting `KVM_EXIT_SHUTDOWN`**, so register state read after the exit is post-reset on AMD, unlike VMX (arch/x86/kvm/svm/svm.c:2159-2180).
6. **A FADT with `RESET_REG_SUP` changes the default reboot path** to the ACPI reset register (drivers/acpi/reboot.c:54-70). If ACPI is emitted, either implement that register or leave the flag clear and rely on `reboot=k`.

---

## 5. Timekeeping and legacy devices affecting boot time

### 5.1 kvmclock (pvclock)

| Item | Value / behaviour | Source |
|---|---|---|
| Discovery leaves | 0x40000000: EAX = max leaf (0x40000001), EBX:ECX:EDX = "KVMKVMKVM\0\0\0". 0x40000001 EAX: CLOCKSOURCE bit 0, NOP_IO_DELAY 1, CLOCKSOURCE2 3, CLOCKSOURCE_STABLE_BIT 24 (plus PV_EOI, PV_UNHALT, etc.). `KVM_GET_SUPPORTED_CPUID` fills both leaves. | kpara:10-25, 45; arch/x86/kvm/cpuid.c:1705-1733 |
| MSRs | `MSR_KVM_WALL_CLOCK_NEW` 0x4b564d00 and `MSR_KVM_SYSTEM_TIME_NEW` 0x4b564d01 when CLOCKSOURCE2; legacy 0x11 / 0x12 otherwise | kpara:47-53; arch/x86/kernel/kvmclock.c:318-326; Documentation/virt/kvm/x86/msr.rst:20-75 |
| `pvclock_vcpu_time_info` (32 B) | `u32 version, pad0; u64 tsc_timestamp, system_time; u32 tsc_to_system_mul; s8 tsc_shift; u8 flags; u8 pad[2]`. flags: TSC_STABLE 1<<0, GUEST_STOPPED 1<<1. SYSTEM_TIME MSR data = 4-byte-aligned GPA \| enable bit 0. | pvabi:26-44; msr.rst:59-75 |
| `pvclock_wall_clock` (12 B) | `u32 version, sec, nsec`. Written by KVM only at the MSR write: epoch = host realtime − kvmclock | pvabi:37-41; arch/x86/kvm/x86.c:2387-2425, 3435-3491, 4091-4115 |
| kvmclock origin | `kvmclock_offset = -get_kvmclock_base_ns()` at VM creation, so guest kvmclock starts near 0 | arch/x86/kvm/x86.c:13311 |
| PV feature gating | Host MSR writes check `guest_pv_has()`, which is enforced only with `KVM_CAP_ENFORCE_PV_FEATURE_CPUID` | arch/x86/kvm/cpuid.h:233-240 |
| `KVM_GET/SET_CLOCK` | VM ioctl, `struct kvm_clock_data {u64 clock; u32 flags, pad0; u64 realtime, host_tsc; u32 pad[4]}`; flags TSC_STABLE / REALTIME / HOST_TSC; cap `KVM_CAP_ADJUST_CLOCK` (39) | api:1040-1100; ukvm:781, 1090-1097, 1300 |
| `KVM_KVMCLOCK_CTRL` | vCPU ioctl 0xAEAD; sets GUEST_STOPPED on a paused vCPU so the guest soft-lockup watchdog stays quiet. Firecracker calls it after pause and restore, ignoring errors. | api:2958-2980; ukvm:1420; fc:src/vmm/src/arch/x86_64/vcpu.rs:312-321 |
| Guest `kvmclock_init` overrides | `calibrate_tsc = calibrate_cpu = kvm_get_tsc_khz` (sets `X86_FEATURE_TSC_KNOWN_FREQ`; kHz from mul/shift). `get_wallclock = kvm_get_wallclock` (rewrites the MSR on every read). `set_wallclock` returns -ENODEV. `preset_lpj` skips delay-loop calibration. Rating 400, or 299 when CONSTANT_TSC + NONSTOP_TSC and the TSC is stable. | arch/x86/kernel/kvmclock.c:62-73, 139-155, 311-371; arch/x86/kernel/pvclock.c:27-37; init/calibrate.c:289-293 |
| Guest `paravirt_ops_setup` | NOP_IO_DELAY → `pv_info.io_delay = false`, so `outb_p` drops the port-0x80 write. Always sets `no_timer_check = 1`. | arch/x86/kernel/kvm.c:320-329, 831; arch/x86/include/asm/io.h:250-256; arch/x86/kernel/io_delay.c:38-61 |
| Ordering | `init_hypervisor_platform()` (→ `kvm_init_platform` → `kvmclock_init`) runs before `tsc_early_init()` | arch/x86/kernel/setup.c:1007-1009; arch/x86/kernel/cpu/hypervisor.c:111; arch/x86/kernel/kvm.c:1010 |
| Kernel config | `CONFIG_KVM_GUEST` (Firecracker: "which enables CONFIG_KVM_CLOCK"); set in the CI kernel | fc:docs/kernel-policy.md:94; ART:cfg:365 |

### 5.2 TSC

| Item | Behaviour | Source |
|---|---|---|
| Early order | `cpu_khz = calibrate_cpu()`; `tsc_khz = tsc_early_khz ?: calibrate_tsc()` | arch/x86/kernel/tsc.c:1442-1482 |
| Native `calibrate_tsc` (no kvmclock) | Intel only. CPUID 0x15 ratio × crystal (KNOWN_FREQ if the crystal Hz is given), else base MHz from 0x16. Also presets `lapic_timer_period`. | arch/x86/kernel/tsc.c:652-722 |
| Native `calibrate_cpu` | CPUID 0x16 → MSR → `quick_pit_calibrate` (≤ 50 ms) → PIT/HPET/PM-timer | arch/x86/kernel/tsc.c:554-557, 726-741, 900-925 |
| KVM and 0x15 / 0x16 | No case in `__do_cpuid_func`, so `default:` zeroes them in `KVM_GET_SUPPORTED_CPUID` (basic leaves clamped to ≤ 0x24) | arch/x86/kvm/cpuid.c:1432-1435, 1896-1903 |
| CPUID 0x40000010 | Read by Linux only for VMware (feature leaf) and ACRN ("TSC frequency in kHz"). A KVM guest never reads it. Firecracker does not set it (no match in fc:src/vmm/src/cpu_config). | arch/x86/kernel/cpu/vmware.c:46, 446-453; arch/x86/include/asm/acrn.h:15-22, 35-38; arch/x86/kernel/cpu/acrn.c:32-33 |
| Invariant TSC | KVM passes 0x80000007 EDX masked by the host. The guest sets CONSTANT_TSC + NONSTOP_TSC from EDX[8], so kvmclock drops to 299 and the TSC (300) becomes the clocksource. | arch/x86/kvm/cpuid.c:1157-1159, 1767-1773; arch/x86/kernel/cpu/intel.c:261-263; arch/x86/kernel/cpu/amd.c:628-631; arch/x86/kernel/tsc.c:1155-1157, 1176-1178 |
| Known frequency | With TSC_KNOWN_FREQ the TSC clocksource registers directly; refined calibration is skipped | arch/x86/kernel/tsc.c:1421-1431 |
| Knobs | `tsc_early_khz=` (early only); `tsc=reliable` disables the watchdog and stability checks | arch/x86/kernel/tsc.c:50, 71-75; Documentation/admin-guide/kernel-parameters.txt:7919-7925, 7943-7947 |
| `KVM_SET_TSC_KHZ` / `KVM_GET_TSC_KHZ` | vCPU ioctl (or VM ioctl before vCPUs exist with `VM_TSC_CONTROL`); `KVM_CAP_TSC_CONTROL` 60. GET returns -EIO on an unstable host TSC. | api:2001-2034; ukvm:810, 1309-1310 |
| Firecracker | Snapshots `tsc_khz`; on restore sets every vCPU's TSC kHz when it differs by > 250 ppm. Restores `MSR_IA32_TSC_DEADLINE` after `MSR_IA32_TSC`. | fc:src/vmm/src/arch/x86_64/vcpu.rs:32-47, 666-681; fc:src/vmm/src/builder.rs:453-465 |

### 5.3 APIC timer and TSC-deadline

| Item | Behaviour | Source |
|---|---|---|
| Calibration skip | `calibrate_APIC_clock` returns at once with TSC_DEADLINE. It also skips when `lapic_timer_period` is preset (CPUID 0x15). Otherwise it runs a polled loop of `LAPIC_CAL_LOOPS` = HZ/10 ticks = **100 ms**. | arch/x86/kernel/apic/apic.c:652, 799-825, 860 |
| Errata check | `apic_validate_deadline_timer` trusts TSC_DEADLINE whenever the hypervisor bit is set | arch/x86/kernel/apic/apic.c:544-557 |
| KVM | TSC-deadline needs the in-kernel LAPIC; `KVM_CAP_TSC_DEADLINE_TIMER` (72). Newer kernels report CPUID.1:ECX[24] in `KVM_GET_SUPPORTED_CPUID`; older ones need the VMM to set it. | api:1841-1848, 9526-9529; ukvm:822 |
| ARAT | KVM reports CPUID 6 EAX = 0x4 (ARAT). Without ARAT, `apic_needs_pit()` is true. | arch/x86/kvm/cpuid.c:1471-1475; arch/x86/kernel/apic/apic.c:781-783 |
| Firecracker | Forces CPUID.1:ECX[24] (TSC-deadline) and ECX[31] (hypervisor). Intel leaf 6 normalization clears only EAX[1] and ECX[3], so ARAT passes through. | fc:src/vmm/src/cpu_config/x86_64/cpuid/normalize.rs:246-252; fc:src/vmm/src/cpu_config/x86_64/cpuid/intel/normalize.rs:164-181 |

### 5.4 Is a PIT needed?

| Item | Behaviour | Source |
|---|---|---|
| Timer init order | `intr_mode_select` → `timer_init` (`hpet_time_init`: `hpet_enable()` returns 0 with no HPET address, then `pit_timer_init()`) → `intr_mode_init` → `tsc_init` | arch/x86/kernel/time.c:57-87; arch/x86/kernel/hpet.c:129-132, 997-999 |
| `pit_timer_init` | If a TSC exists and `!apic_needs_pit()`: no PIT clockevent. The PIT is stopped with 4 PIO writes (0x30→0x43, 0→0x40 twice, 0x30→0x43). | arch/x86/kernel/i8253.c:32-56; drivers/clocksource/i8253.c:104-135 |
| `apic_needs_pit()` true when | `tsc_khz` or `cpu_khz` unknown; no or disabled APIC; interrupt mode PIC or `VIRTUAL_WIRE_NO_CONFIG`; no ARAT; then false if TSC_DEADLINE; else true if the APIC timer is disabled or `lapic_timer_period == 0` | arch/x86/kernel/apic/apic.c:759-797 |
| `VIRTUAL_WIRE_NO_CONFIG` | Selected when neither an MP table (`smp_found_config`) nor an ACPI MADT (`acpi_lapic`) exists | arch/x86/kernel/apic/apic.c:1252-1304 |
| `check_timer` | Runs only with legacy IRQs and a global clockevent (the PIT). `timer_irq_works()` returns 1 at once under `no_timer_check`, which KVM guests always set. | arch/x86/kernel/apic/io_apic.c:1466-1473, 1519-1524, 2049-2058, 2291-2292 |
| HW-reduced ACPI | `timer_init = x86_init_noop`, `legacy_pic = &null_legacy_pic`: no PIT or PIC code at all | arch/x86/kernel/acpi/boot.c:1407-1422 |
| `KVM_CREATE_PIT2` | Valid only after `KVM_CREATE_IRQCHIP`. Starts a `kvm-pit/<pid>` kthread. Handles PIO 0x40-0x43 in-kernel. `KVM_PIT_SPEAKER_DUMMY` (1) also handles port 0x61. | api:3021-3051; ukvm:89-96, 1296; arch/x86/kvm/i8254.c:753, 774-786; arch/x86/kvm/i8254.h:55-57 |
| Firecracker | `create_irq_chip()` then `create_pit2(KVM_PIT_SPEAKER_DUMMY)`, "so that writing to port 0x61 ... does not trigger an exit to user space" | fc:src/vmm/src/arch/x86_64/vm.rs:171-184 |

Derived: a guest with kvmclock, TSC-deadline, ARAT, and a MADT or MP table never uses a PIT. It only writes the 4 stop bytes. Without `KVM_CREATE_PIT2` those writes become 4 ignored userspace PIO exits, and the VM saves the `kvm-pit` kthread. The in-kernel PIC and IOAPIC (`KVM_CREATE_IRQCHIP`) remain required for GSIs (§1). The CI kernel has `CONFIG_X86_MPPARSE` off (ART:cfg:351; arch/x86/Kconfig:507-513), so for that kernel the MADT is the only source that avoids `VIRTUAL_WIRE_NO_CONFIG` (cross-ref §3).

### 5.5 CMOS RTC (ports 0x70/0x71)

| Item | Behaviour | Source |
|---|---|---|
| Wall clock | `read_persistent_clock64` → `x86_platform.get_wallclock`, which is `kvm_get_wallclock` under kvmclock (default `mach_get_cmos_time`) | arch/x86/kernel/rtc.c:108-111; arch/x86/kernel/x86_init.c:152; arch/x86/kernel/kvmclock.c:348 |
| `rtc_cmos` device | Registered when `legacy.rtc` (default 1; cleared by FADT "CMOS RTC Not Present" or the XEN/MID subarchs). The driver needs `RTC_CLASS`, which is off in the CI kernel. | arch/x86/kernel/rtc.c:134-147; arch/x86/kernel/platform-quirks.c:9-31; arch/x86/kernel/acpi/boot.c:988-991; ART:cfg:2496 |
| Remaining CMOS writes | AP bring-up writes CMOS 0xF = 0x0A once, and restores 0 afterwards, while `legacy.warm_reset` (default 1) is set. The BIOS reboot method writes 0x8F. | arch/x86/kernel/smpboot.c:139-165, 1048-1052, 1131-1132; arch/x86/kernel/platform-quirks.c:13; arch/x86/kernel/reboot.c:113-115 |
| Unhandled PIO | KVM zero-fills the PIO data for IN before `KVM_EXIT_IO`. Firecracker also `fill(0)`s, and registers only 0x3F8 (8 ports) and 0x60 (5 ports), so 0x70/0x71 reads return 0x00 and writes are dropped. | arch/x86/kvm/x86.c:8421-8436; fc:src/vmm/src/arch/x86_64/vcpu.rs:759-779; fc:src/vmm/src/device_manager/legacy.rs:45-66 |

### 5.6 i8042 probe

| Step / flag | Behaviour | Source |
|---|---|---|
| Platform init | PNP detection runs unless `i8042.nopnp`. If PNP finds no controller: with `legacy.i8042 != EXPECTED_PRESENT` (FADT rev ≥ 3 with the 8042 flag clear → FIRMWARE_ABSENT), return -ENODEV with **no port access**; else "Probing ports directly". `nopnp` bypasses that check. Then A20 command 0xD1 and null 0xFF. | drivers/input/serio/i8042-acpipnpio.h:1610-1646, 1779-1846; arch/x86/kernel/acpi/boot.c:981-986; include/acpi/actbl.h:261, 267 |
| Probe | Flush (`i8042_controller_check`), read CTR (0x20) until two reads agree, AUX setup and MUX check, KBD port; `atkbd_probe` sends commands through the port | drivers/input/serio/i8042.c:257-275, 931-939, 986-1011, 1445-1456, 1537-1566; drivers/input/keyboard/atkbd.c:1276-1305 |
| Timeouts | `i8042_wait_read`/`wait_write` poll up to `I8042_CTL_TIMEOUT` = 10000 × 50 µs = **0.5 s** per missing response | drivers/input/serio/i8042.c:34, 230-250 |
| `noaux` | Skip AUX (mouse) probe: `AUX_LOOP` 0x11D3, `AUX_TEST` 0x01A9, IRQ test | drivers/input/serio/i8042.c:800-870, 1556-1560; include/linux/i8042.h:24-26 |
| `nomux` | Skip the active-MUX check | drivers/input/serio/i8042.c:1454 |
| `dumbkbd` | KBD serio `write = NULL`, so `atkbd` never probes or sets LEDs | drivers/input/serio/i8042.c:1332; drivers/input/keyboard/atkbd.c:1281-1305 |
| `nopnp` / `nokbd` / `direct` | Skip PNP driver registration / skip the KBD port / untranslated mode | drivers/input/serio/i8042.c:34-35, 87-89, 115-116; Documentation/admin-guide/kernel-parameters.txt:2171-2181 |
| Firecracker device | Implements only 0x20, 0x60, 0xD0, 0xD1, 0xFE; ACKs data-port bytes with 0xFA. No AUX command answers, hence `noaux`. The docs say the driver "spends a few tens of milliseconds probing" without the flags. | fc:src/vmm/src/devices/legacy/i8042.rs:69-74, 248-338; fc:docs/api_requests/actions.md:47-55 |

Derived: shards needs no guest i8042 driver. Host stop goes over vsock, and the reset path uses raw `outb` (§4). Options, cheapest first:

- Build the guest kernel without `CONFIG_SERIO_I8042`: zero probe cost.
- With ACPI and a stock kernel: clear the FADT 8042 boot flag, omit PNP0303, and do not pass `nopnp`. Platform init then exits with no port access.
- Otherwise, use Firecracker's flags.

Any i8042 command the VMM leaves unanswered costs up to 0.5 s.

### 5.7 Serial 8250 (COM1 0x3F8, IRQ 4)

| Item | Behaviour | Source |
|---|---|---|
| Legacy port table | ttyS0 0x3F8/IRQ 4, ttyS1 0x2F8/3, ttyS2 0x3E8/4, ttyS3 0x2E8/3. COM1-3 carry `UPF_SKIP_TEST` (no loopback test). | arch/x86/include/asm/serial.h:14-28 |
| Port count | Only the first `nr_uarts` (= `CONFIG_SERIAL_8250_RUNTIME_UARTS`) legacy ports are set up. `8250.nr_uarts=0` makes `serial8250_init` return -ENODEV: no 8250 driver and no ttyS console. | drivers/tty/serial/8250/8250_platform.c:35, 59-98, 296-301, 383-384 |
| CI kernel | `SERIAL_8250=y`, `8250_PNP=y`, `8250_CONSOLE=y`, `NR_UARTS=1`, `RUNTIME_UARTS=1` | ART:cfg:2147-2158 |
| Probe | `autoconfig` reads/writes IER, LCR, scratch; the loopback test is skipped with `UPF_SKIP_TEST` | drivers/tty/serial/8250/8250_port.c:1070-1143 |
| Console write cost | Per char: LSR poll (`wait_for_xmitr`) + THR write, unless the FIFO path is active; then wait for BOTH_EMPTY. `earlycon=uart8250,io,0x3f8` polls LSR after every char. | drivers/tty/serial/8250/8250_port.c:1968-1994, 3243-3255, 3364-3392; drivers/tty/serial/8250/8250_early.c:85-96; Documentation/admin-guide/kernel-parameters.txt:1397-1415 |
| Firecracker UART | One device at 0x3F8 (8 ports), GSI 4 via irqfd, DSDT PNP0501. vm-superio reports 16550A (IIR FIFO bits); LSR defaults to THR-empty \| idle, so every poll exits on its first read. | fc:src/vmm/src/device_manager/legacy.rs:39-47, 57-61, 68-76, 87-108; vs:src/serial.rs:48, 97, 680-683 |

Derived: every console byte costs at least one PIO exit to the VMM. When no console is requested, use `8250.nr_uarts=0`, as Firecracker's default and its boot-time test do. When one is requested, `quiet` keeps kernel logs off the exit path.

### 5.8 Firecracker's guest command line

Default: `reboot=k panic=1 nomodule 8250.nr_uarts=0 i8042.noaux i8042.nomux i8042.dumbkbd swiotlb=noforce`, plus `pci=off` when PCI is disabled (fc:src/vmm/src/vmm_config/boot_source.rs:10-20; fc:docs/kernel-policy.md:186-207; fc:src/vmm/src/builder.rs:217-219). The boot-time test adds `i8042.nopnp cryptomgr.notests` (fc:tests/integration_tests/performance/test_boottime.py:16-19). Functional tests use `console=ttyS0` without `8250.nr_uarts=0` (fc:tests/framework/kvm.py:64-66, 89-95).

| Parameter | Linux effect | Source |
|---|---|---|
| `nomodule` | `modules_disabled = 1` | kernel/module/main.c:131 |
| `swiotlb=noforce` | `swiotlb_force_disable`: no 64 MiB bounce buffer allocated at boot | kernel/dma/swiotlb.c:190-201, 362-365; include/linux/swiotlb.h:36 |
| `pci=off` | `pci_probe = 0` | arch/x86/pci/common.c:516-520 |
| `cryptomgr.notests` | Skips crypto self-tests (the CI kernel has `CRYPTO_SELFTESTS=y`) | crypto/testmgr.c:43-45, 5624; ART:cfg:3212 |
| `quiet`, `loglevel=` | Not used for guests. Firecracker recommends `quiet loglevel=1` only on the **host** kernel. | fc:docs/prod-host-setup.md:58-67 |

### 5.9 Derived: shards x86_64 timekeeping and legacy-device plan

| Device / feature | Emulate? | cmdline | Boot-time effect |
|---|---|---|---|
| kvmclock (KVM leaves 0x40000000/1 passed through) | In-kernel | — | Skips TSC/CPU/delay-loop calibration and timer check; wall clock without an RTC |
| TSC-deadline + ARAT + hypervisor bit | Set CPUID.1:ECX[24], ECX[31]; keep 6:EAX[2] | — | Skips 100 ms APIC calibration; no PIT needed |
| Invariant TSC (0x80000007 EDX[8]) | Pass through | — | TSC clocksource (rating 300) |
| `KVM_CREATE_IRQCHIP` | Yes | — | LAPIC/IOAPIC/PIC in-kernel |
| MADT (or MP table with an `X86_MPPARSE` kernel) | Yes | — | Avoids `VIRTUAL_WIRE_NO_CONFIG` → no PIT |
| PIT (`KVM_CREATE_PIT2`) | No | — | With a HW-reduced FADT the guest never touches the PIT (`timer_init` no-op, §5.4). Otherwise: 4 ignored PIO exits vs. one ioctl + kthread (F5-2). |
| HPET, CMOS RTC, ACPI PM timer | No | — | 0x70/0x71 writes at AP bring-up become ignored exits |
| i8042 | Port 0x64 reset decode only | None. With ACPI: FADT 8042 flag clear, no `PNP0303`, never `i8042.nopnp` (§5.6). Firecracker's flags only for ACPI-less kernels that have the driver. | 0 probe cost |
| 8250 COM1 | Only when a console is attached | `8250.nr_uarts=0` otherwise; `quiet` | No per-byte exits |
| NOP_IO_DELAY | Pass through (KVM sets it) | — | No port-0x80 write per `outb_p` |
| Misc | — | `nomodule swiotlb=noforce pci=off` (when no PCI) | Skips 64 MiB swiotlb, PCI probe |

### Section 5 bug-magnets

1. **No MADT and no MP table** → `APIC_VIRTUAL_WIRE_NO_CONFIG` → PIT required and IOAPIC unused. The CI kernel ignores MP tables (`X86_MPPARSE` off).
2. **Clearing CPUID.1:ECX[24]** (or an old host that doesn't report it) silently adds 100 ms of APIC-timer calibration per boot.
3. **Unhandled PIO IN returns 0x00, not 0xFF** (KVM zero-fill, Firecracker `fill(0)`). Real hardware floats high, so "absent device" probes that expect 0xFF can misdetect.
4. **Every unanswered i8042 command costs up to 0.5 s.** Emulate either nothing (kernel without the driver) or everything the probe sends.
5. **`KVM_GET_SUPPORTED_CPUID` zeroes 0x15/0x16.** With `no-kvmclock`, calibration falls back to the PIT (≤ 50 ms quick path) and needs a PIT. Never disable kvmclock without filling 0x15.
6. **Snapshot restore:** set TSC kHz before vCPU state, and restore `IA32_TSC_DEADLINE` after `IA32_TSC`, or timer interrupts are lost (fc:src/vmm/src/arch/x86_64/vcpu.rs:39-47).
7. **Each console byte is ≥ 1 PIO exit.** A chatty `console=ttyS0` boot is dominated by UART exits.

---

## 6. Memory layout

### 6.1 Firecracker x86_64 guest-physical map

| Address / range | Use | Source |
|---|---|---|
| 0x500 / 0x520 | Boot GDT (4 entries, limit 31) / IDT (limit 7, one zero entry) | fc:src/vmm/src/arch/x86_64/regs.rs:163-166, 176-194, 229-233 |
| 0x6000 / 0x6040 | PVH `hvm_start_info` / module list (initrd) | fc:src/vmm/src/arch/x86_64/layout.rs:43-48 |
| 0x7000 | Zero page (Linux boot) or PVH memmap. The two protocols are exclusive, so they share the address. | fc:src/vmm/src/arch/x86_64/layout.rs:50-55 |
| 0x8FF0 | Initial RSP and RBP | fc:src/vmm/src/arch/x86_64/layout.rs:13-14 |
| 0x9000 / 0xA000 / 0xB000 | PML4 / PDPT / PD, identity-mapping [0, 1 GiB) with 512 × 2 MiB pages | fc:src/vmm/src/arch/x86_64/regs.rs:20-22, 258-282 |
| 0x20000 | Command line, ≤ 2048 B including the NUL | fc:src/vmm/src/arch/x86_64/layout.rs:16-19 |
| 0x9FC00-0xDFFFF | "System memory" (257 KiB): MP table, ACPI tables, VMGenID, VMClock | fc:src/vmm/src/arch/x86_64/layout.rs:66-92 |
| 0xE0000 | RSDP | fc:src/vmm/src/arch/x86_64/layout.rs:63-64 |
| 0x100000 | `HIMEM_START`: where e820 RAM resumes, the minimum ELF `e_entry`, and the bzImage `code32_start` default | fc:src/vmm/src/arch/x86_64/layout.rs:21-22; ll:src/loader/elf/mod.rs:221-225; ll:src/loader/bzimage/mod.rs:149-162; arch/x86/boot/header.S:284-286 |
| each PT_LOAD `p_paddr` | vmlinux segments. The CI kernel spans [0x1000000, 0x2830000), with `PHYSICAL_START` = 0x1000000. | ll:src/loader/elf/mod.rs:254-293; ART:vmlinux; ART:cfg:475 |
| Top of low RAM | initrd, aligned down to 4 KiB | fc:src/vmm/src/arch/x86_64/mod.rs:166-179 |
| 0xC0000000-0xFFFFFFFF | 1 GiB 32-bit MMIO gap. RAM beyond 3 GiB continues at 4 GiB. | fc:src/vmm/src/arch/x86_64/layout.rs:94-100; fc:src/vmm/src/arch/x86_64/mod.rs:112-159 |
| 0xC0000000 | Boot-timer page. The guest init writes 123 there through /dev/mem. It is attached first so the address stays stable. | fc:src/vmm/src/arch/x86_64/layout.rs:114-115; fc:src/vmm/src/device_manager/mmio.rs:367-394; fc:src/vmm/src/builder.rs:221-226; fc:resources/rootfs/overlay/usr/local/bin/init.c:15-22 |
| 0xC0001000-0xEEBFFFFF | 32-bit device windows (virtio-mmio slots, PCI BARs) | fc:src/vmm/src/arch/x86_64/layout.rs:117-120 |
| 0xEEC00000-0xFEBFFFFF | PCIe ECAM, 256 MiB | fc:src/vmm/src/arch/x86_64/layout.rs:102-109 |
| 0xFEC00000 / 0xFEE00000 | IOAPIC / LAPIC | fc:src/vmm/src/arch/x86_64/layout.rs:57-61 |
| 0xFFFBD000, 3 pages | `KVM_SET_TSS_ADDR` | fc:src/vmm/src/arch/x86_64/layout.rs:40-41; fc:src/vmm/src/arch/x86_64/vm.rs:101-104 |
| 0xFFFBC000 | EPT identity-map page. Firecracker never calls `KVM_SET_IDENTITY_MAP_ADDR`, so KVM's default applies. | arch/x86/include/asm/vmx.h:588; api:1636-1637 |
| 256-512 GiB / 512 GiB-1 TiB | 64-bit MMIO (PCI BARs) / virtio-mem hotplug | fc:src/vmm/src/arch/x86_64/layout.rs:122-136; fc:src/vmm/src/builder.rs:608-622 |
| GSIs | 0-4 kept free (COM1 = 4, i8042 = 1); 5-23 legacy pool; 24-4095 MSI (`KVM_MAX_IRQ_ROUTES` = 4096) | fc:src/vmm/src/arch/x86_64/layout.rs:24-38; fc:src/vmm/src/device_manager/legacy.rs:38-42; include/linux/kvm_host.h:2205 |

Derived: allocation order puts VMGenID at 0xDFFF0 and VMClock at 0xDE000. VMGenID (16 B, LastMatch) is allocated before VMClock (4 KiB, LastMatch) (fc:src/vmm/src/builder.rs:301-302), and both before the MP and ACPI tables. See F6-1.

### 6.2 The e820 map and PVH memmap Firecracker writes

| # | Range | Type | Why | Source |
|---|---|---|---|---|
| 1 | [0, 0x9FC00) | RAM (1) | Conventional memory. Linux needs free RAM below 1 MiB for the AP trampoline (§6.3). | fc:src/vmm/src/arch/x86_64/mod.rs:434-437 |
| 2 | [0x9FC00, 0xE0000) | Reserved (2) | EBDA-style area holding the MP and ACPI data | fc:src/vmm/src/arch/x86_64/mod.rs:438-443 |
| 3 | [0xEEC00000, 0xFEC00000) | Reserved | Linux accepts an MCFG ECAM only if it is reserved | fc:src/vmm/src/arch/x86_64/mod.rs:444-449; arch/x86/pci/mmconfig-shared.c:471-480 |
| 4… | [max(1 MiB, start), end] of each DRAM region | RAM | Kernel, initrd, free memory | fc:src/vmm/src/arch/x86_64/mod.rs:451-463 |
| — | [0xE0000, 0x100000) | Not listed | Holds the RSDP; Linux treats the hole as non-RAM | fc:src/vmm/src/arch/x86_64/mod.rs:434-463 |
| PVH | Entries 1-3 plus the DRAM entries, as `hvm_memmap_table_entry` at 0x7000 | same types | Linux appends [0xA0000, 1 MiB) as reserved | fc:src/vmm/src/arch/x86_64/mod.rs:329-361; arch/x86/platform/pvh/enlighten.c:62-71 |

The zero page holds at most 128 e820 entries (bp:108, 137, 159). Every DRAM region is its own KVM memslot (fc:src/vmm/src/vstate/vm.rs:484-489).

### 6.3 Linux and KVM constraints behind the layout

| Constraint | Detail | Source |
|---|---|---|
| Page 0 and the low 64 KiB | Page 0 is converted to e820-reserved. [0, 64 KiB) is memblock-reserved against BIOS corruption and L1TF. | arch/x86/kernel/setup.c:752-771, 796-818 |
| 640 KiB-1 MiB | Always removed from e820 RAM | arch/x86/kernel/setup.c:770 |
| BIOS/EBDA reservation (PC subarch) | Reads the BDA word at 0x413 (KiB). A value of 0 is treated as 636 KiB, so [0x9F000, 1 MiB) is reserved. The 0x40E EBDA pointer is used only if it is ≥ 128 KiB. | arch/x86/kernel/ebda.c:51-97; arch/x86/kernel/platform-quirks.c:9-20 |
| AP trampoline | Needs a page-aligned block below `realmode_limit` = 1 MiB, after which the whole first MiB is reserved. If none is free: panic "Real mode trampoline was not allocated". | arch/x86/realmode/init.c:47-70, 211; arch/x86/kernel/x86_init.c:74 |
| 64-bit entry | Paging on. The kernel (`init_size`), zero page and cmdline must be identity-mapped. | boot:1389-1398 |
| Command line | `COMMAND_LINE_SIZE` = 2048; the header advertises 2047 | arch/x86/include/asm/setup.h:7; arch/x86/boot/header.S:384 |
| initrd | Relocatable 64-bit kernels set `XLF_CAN_BE_LOADED_ABOVE_4G`. Other kernels need the initrd ≤ `initrd_addr_max` (0x7FFFFFFF). | arch/x86/boot/header.S:324, 349-353; boot:635-645, 708-710 |
| KVM private memslots | TSS: 3 pages, required on Intel, below 4 GiB, overlapping no memslot or MMIO; a no-op with unrestricted guest. Identity page: 1 page, default 0xFFFBC000, used only with EPT and no unrestricted guest; set it before any vCPU. APIC-access page: at 0xFEE00000. | api:1438-1455, 1621-1643; arch/x86/kvm/vmx/vmx.c:4010-4024, 5254-5272, 7735-7739; arch/x86/kvm/lapic.c:2901-2919 |
| In-kernel IOAPIC | MMIO at 0xFEC00000, length 0x100, 24 pins | arch/x86/kvm/ioapic.h:19-20; xkvm:82 |
| Address width | KVM reports the host MAXPHYADDR in CPUID 0x80000008 (with TDP). Addressable width is capped at 48 without 5-level TDP. | arch/x86/kvm/cpuid.c:1774-1818 |
| Huge pages | Keep the low 21 bits of GPA and HVA equal | api:1409-1411 |

### 6.4 Derived: proposed shards x86_64 map

Reuse Firecracker's low-memory layout, which the CI kernel is known to boot with. Validate every GPA against the MAXPHYADDR from `KVM_GET_SUPPORTED_CPUID` leaf 0x80000008 at VM creation.

| GPA | Use | e820 |
|---|---|---|
| 0x0-0xFFF | Zero-filled (IVT/BDA area; Linux reserves it) | RAM |
| 0x500 / 0x520 | Boot GDT / empty IDT (contents: §2) | RAM (entry 1) |
| 0x6000, 0x6040 | `hvm_start_info`, modlist (PVH) | RAM |
| 0x7000 | Zero page (64-bit) or PVH memmap | RAM |
| 0x8FF0 | Boot stack | RAM |
| 0x9000-0xBFFF | PML4 / PDPT / PD, identity 0-1 GiB, 2 MiB pages | RAM |
| 0x20000 | cmdline, ≤ 2047 chars + NUL | RAM |
| 0x9FC00-0xDFFFF | ACPI tables (XSDT, FADT, MADT, DSDT, +MCFG); MP table first only for ACPI-less kernels | Reserved |
| 0xE0000-0xFFFFF | RSDP at 0xE0000 | Reserved (listed, unlike Firecracker) |
| 1 MiB-3 GiB | RAM. vmlinux at `p_paddr`; initrd at the top, 4 KiB-aligned (2 MiB-aligned eases THP) | RAM |
| 0xC0000000 | Shards control page, the x86 twin of `CONTROL_PAGE_AARCH64`. Guest init already writes through /dev/mem (shards `crates/init/src/linux.rs:68-92`), as Firecracker's init does at this address. | None |
| 0xC0001000+ | virtio-mmio slots. Stride 0x200 as on arm64 (shards `crates/vmm/src/arch/aarch64/mod.rs:23-25`) or 0x1000 as Firecracker; any size ≥ 0x100 + config space works (fc:src/vmm/src/device_manager/mmio.rs:59-64). GSIs from 5. | None |
| 0xEEC00000-0xFEBFFFFF | ECAM, only with virtio-pci | Reserved when used |
| 0xFEC00000 / 0xFEE00000 | KVM IOAPIC / LAPIC | None |
| 0xFFFBC000 / 0xFFFBD000 | KVM identity page / TSS. Set both explicitly so the reservations are visible in code. | None |
| ≥ 4 GiB | RAM above 3 GiB | RAM |

### Section 6 bug-magnets

1. No free e820 RAM below 1 MiB gives a boot-time panic (the trampoline).
2. User memslots must avoid 0xFEE00000 (APIC-access page) and 0xFFFBC000-0xFFFBFFFF (identity page and TSS) (api:1447-1451, 1630-1634). An overlap fails with **EEXIST** from whichever call comes second: the memslot ioctl, `KVM_SET_TSS_ADDR`, or vCPU creation (APIC-access page). KVM checks overlaps against every slot, private ones included, and creates private slots in every address space (kmain:1981-1992, 2092-2094; x86.c:13405-13413).
3. The boot page tables map only 1 GiB. The kernel image (`init_size`), zero page and cmdline must all sit below 1 GiB. The initrd may sit higher.
4. `KVM_SET_IDENTITY_MAP_ADDR` fails after any vCPU exists (api:1643).
5. The e820 table is limited to 128 entries, and the cmdline to 2047 characters. Each `virtio_mmio.device=` string costs about 36 bytes (ll:src/cmdline/mod.rs:442-447).
6. Guest-physical addresses above the host MAXPHYADDR are unreachable. Firecracker's virtio-mem window at 512 GiB-1 TiB needs 40 bits.

---

## 7. The guest kernel artifact

Listing: `GET https://s3.amazonaws.com/spec.ccfc.min?list-type=2&prefix=firecracker-ci/20260923-6f82ac4cf331-0/x86_64/` on 2026-09-28 returned `KeyCount` 29 with `IsTruncated` false. **The x86_64 `vmlinux-6.18.48` exists.** Everything below is `ART:`, measured on the downloaded files with a Python ELF parser (no `readelf` on the dev host).

### 7.1 Objects under the x86_64 prefix

| Key (after `…/x86_64/`) | Size (B) | Note |
|---|---|---|
| `vmlinux-6.18.48` | 27,846,792 | **the one to pin** |
| `vmlinux-6.18.48.config` | 99,233 | its `.config` |
| `bzImage-6.18.48` | 9,171,968 | same kernel, gzip-compressed bzImage |
| `debug/vmlinux-6.18.48`, `debug/bzImage-6.18.48`, `debug/vmlinux-6.18.48.config`, `debug/vmlinux-6.18.48.debug.gz` | 39,987,928; 12,551,168; 101,891; 172,982,603 | debug build |
| `vmlinux-6.1.186`, `vmlinux-5.10.268`, `vmlinux-5.10.268-no-acpi` (+ `.config`, `bzImage-*`, `debug/*`) | — | older lines. "no-acpi" exists for 5.10 only. |
| `initramfs.cpio`; `ubuntu-24.04.squashfs` (+`.manifest`); `amazonlinux-2023.squashfs` (+`.manifest`) | 2,126,336; 108,204,032; 106,405,888 | test rootfs images |

### 7.2 Pins

| File | sha256 | Size |
|---|---|---|
| `x86_64/vmlinux-6.18.48` | `9204218e8bcca6ac23848d74f45df2eb19d7f31e8277840a7d145a0df8b078d2` | 27,846,792 |
| `x86_64/vmlinux-6.18.48.config` | `83fcd475947094f843ef5d96b5f2c45e0b80fd69a64265ef9f250bcea99fe34f` | 99,233 |
| `x86_64/bzImage-6.18.48` | `8930dbf6a70d226d2558cd5ecb43e3102282feac4d7d68859293cb888dfb03d0` | 9,171,968 |

(For comparison, the aarch64 pin in `crates/shards/tests/common/mod.rs:54-56` is `a80108af…3451e`.)

### 7.3 ELF header and program headers (`vmlinux-6.18.48`)

ELF64, little-endian, `ET_EXEC`, `e_machine` 62 (x86-64), **`e_entry = 0x2353eb0`**, 3 program headers, 37 sections, symbol table present.

| Type | Offset | VirtAddr | **PhysAddr** | FileSiz | MemSiz | Flg | Align |
|---|---|---|---|---|---|---|---|
| LOAD | 0x200000 | 0xffffffff81000000 | **0x1000000** | 0x11a6de8 | 0x11a6de8 | R-X | 0x200000 |
| LOAD | 0x1400000 | 0xffffffff82200000 | **0x2200000** | 0x35c000 | 0x630000 | RW- | 0x200000 |
| NOTE | 0x12a7874 | 0xffffffff820a7874 | 0x20a7874 | 0xac | 0xac | — | 0x4 |

| Symbol | Value | Meaning |
|---|---|---|
| `phys_startup_64` (SHN_ABS) | 0x2353eb0 | = `e_entry` (§2.1) |
| `startup_64` | 0xffffffff82353eb0 (`.init.text`) | 64-bit entry, physical 0x2353eb0 |
| `pvh_start_xen` | 0xffffffff82353a90 (`.init.text`) | PVH entry, physical 0x2353a90 |
| `_text` / `_end` | 0xffffffff81000000 / 0xffffffff82830000 | image spans physical **0x100_0000-0x283_0000** (24.19 MiB, ending at 40.19 MiB) |
| `__bss_start` / `__brk_base` | 0xffffffff8255c000 / 0xffffffff82800000 | bss and brk are the zero-fill tail of LOAD[1] (memsz − filesz = 0x2d4000) |

Derived: a loader that copies `filesz` and zeroes to `memsz` at `p_paddr` puts the kernel at 16 MiB-40.19 MiB. Guest RAM must extend past 0x283_0000 plus the kernel's early allocations. The 2 MiB-aligned `p_paddr` values satisfy the load-delta rule (§2.1) with a zero delta.

### 7.4 Notes

| Owner | Type | descsz | Value | Meaning |
|---|---|---|---|---|
| GNU | 3 | 20 | `2399a908…8740a` | build-id |
| Linux | 0x101 | 4 | 0 | `LINUX_ELFNOTE_LTO_INFO` (include/linux/elfnote-lto.h:6): no LTO |
| Linux | 0x100 | 39 | "6.18.39-79.141.amzn2023.x86_64.microvm" | `LINUX_ELFNOTE_BUILD_SALT` (include/linux/build-salt.h:6-16) = `CONFIG_BUILD_SALT` (the Amazon Linux microvm config lineage, not the kernel version) |
| Xen | 19 `PHYS32_RELOC` | 12 | align 0x100_0000, min 0x100_0000, max 0x3fff_ffff | §2.3 |
| Xen | **18 `PHYS32_ENTRY`** | **8** | **0x2353a90** = `pvh_start_xen` | **the PVH note is present** |

### 7.5 Kernel config (`vmlinux-6.18.48.config`, "Linux/x86 6.18.48", gcc 13.3.0 Ubuntu 24.04)

| Option | Value | Consequence for shards |
|---|---|---|
| `CONFIG_PVH` | **y** (line 367) | PVH entry available |
| `CONFIG_ACPI` | **y** (550); `ACPI_REDUCED_HARDWARE_ONLY` not set (584) | ACPI tables are parsed |
| `CONFIG_KVM_GUEST` | **y** (365); `PARAVIRT` y (360); `PARAVIRT_CLOCK` y (369); `HYPERVISOR_GUEST` y (359) | kvmclock and PV features |
| `CONFIG_VIRTIO_MMIO` | y (2562) | |
| `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES` | **not set** (2563) | **`virtio_mmio.device=` on the cmdline does nothing.** virtio-mmio devices can be found only via ACPI (or DT). |
| `CONFIG_X86_MPPARSE` | **not set** (351) | **An MP table is ignored.** SMP and IOAPIC discovery need an ACPI MADT. |
| `CONFIG_SERIAL_8250` / `_CONSOLE` / `_PNP` | y / y / y (2147, 2152, 2149) | ttyS0 console |
| `CONFIG_SERIAL_8250_NR_UARTS` / `RUNTIME_UARTS` | **1 / 1** (2157-2158) | only one legacy UART slot is registered |
| `CONFIG_SERIAL_EARLYCON` | y (2146) | `earlycon=uart8250,io,0x3f8` available |
| `CONFIG_SERIO_I8042`, `KEYBOARD_ATKBD` | y, y (2115, 2082) | i8042 is probed at boot (§5) |
| `CONFIG_RTC_CLASS` | **not set** (2496) | no rtc-cmos driver |
| `CONFIG_HPET_TIMER` / `CONFIG_HPET` | y / not set (392 / 2210) | HPET clocksource only if an ACPI HPET table exists |
| `CONFIG_X86_PM_TIMER` | y (598) | ACPI PM timer, if FADT describes one |
| `CONFIG_PCI`, `PCI_MSI`, `VIRTIO_PCI`, `PCI_DIRECT`, `PCI_MMCONFIG` | y (1655, 1667, 2555, 652, 653) | PCI is probed via port 0xCF8 unless `pci=off` |
| `CONFIG_EFI` | not set (456) | direct boot only |
| `CONFIG_RANDOMIZE_BASE` / `RANDOMIZE_MEMORY` | y / y (477, 480) | inert for direct vmlinux boot (§2.2 `KASLR_FLAG`) |
| `CONFIG_PHYSICAL_START` / `PHYSICAL_ALIGN` | 0x100_0000 / 0x100_0000 (475, 479) | matches `p_paddr` |
| `CONFIG_SMP` / `NR_CPUS` / `X86_X2APIC` | y / 64 / y (349, 399, 350) | |
| `CONFIG_HZ` / `NO_HZ_IDLE` / `PREEMPT_NONE` | 250 / y / y (461, 109, 133) | |
| `CONFIG_MODULES` | y (878) | |
| `CONFIG_BLK_DEV_INITRD`, `RD_GZIP`, `RD_ZSTD` | y (244, 246, 252) | |
| `CONFIG_VIRTIO_BLK`, `VIRTIO_NET`, `VIRTIO_VSOCKETS`, `VIRTIO_CONSOLE`, `HW_RANDOM_VIRTIO`, `VIRTIO_BALLOON`, `VIRTIO_MEM`, `VIRTIO_PMEM` | y | |
| `CONFIG_VMGENID` | y (2547) | ACPI VM generation ID (clone safety) |
| `CONFIG_PTP_1588_CLOCK_KVM` | y (2247) | |
| `CONFIG_KERNEL_GZIP` | y (51) | the bzImage payload is gzip |
| `CONFIG_SUSPEND` / `CONFIG_ACPI_SLEEP` / `CONFIG_ACPI_BUTTON` | not set / y / not set (532, 558, 563) | §4 |
| `CONFIG_DEVMEM` / `STRICT_DEVMEM` / `IO_STRICT_DEVMEM` | y / y / not set (2207, 3749, 3750) | Guest init can `mmap` `/dev/mem` at a non-RAM control page, as shards-init does on arm64 |

**Provenance.** The published config is Firecracker's in-repo `microvm-kernel-ci-x86_64-6.18.config` with `ci.config` and `nvme.config` concatenated after it, then `make olddefconfig` (fc:resources/rebuild.sh:165-178, 241-254). `ci.config` adds `IKCONFIG`, `MSDOS_PARTITION`, `DEVMEM`, `STRICT_DEVMEM`, and `SERIO`/`SERIO_I8042`/`KEYBOARD_ATKBD`/`INPUT_KEYBOARD` "for CTRL+ALT+DEL support". `nvme.config` adds `BLK_DEV_NVME` (fc:resources/guest_configs/ci.config:1-13; nvme.config:1-2). ART: a diff of the published config against the in-repo one shows 52 differing symbols. Every one comes from those two fragments or the toolchain (published: gcc 13.3.0 Ubuntu 24.04; in-repo: gcc 11.5.0 RHEL). All boot-relevant options above are identical. Derived: the in-repo 6.18 config alone has **no i8042 driver** (`SERIO` off), and only the published artifact has one.

Derived: with this kernel, **ACPI tables are mandatory** for anything beyond a uniprocessor serial console. Without a MADT, only the BSP comes up and the IOAPIC is not discovered. Without a DSDT, no virtio-mmio device is found, so there is no root disk. Neither the command line (`VIRTIO_MMIO_CMDLINE_DEVICES` off) nor an MP table (`X86_MPPARSE` off) can substitute. §3 lists the tables.

### 7.6 The bzImage, for comparison

setup header (offsets per §2.2): protocol **2.15** (`version` 0x020F), `setup_sects` 31, `loadflags` 0x01, `code32_start` 0x10_0000, `kernel_alignment` 0x100_0000, `relocatable_kernel` 1, `min_alignment` 21 (2 MiB), `xloadflags` 0x63 (`KERNEL_64 | CAN_BE_LOADED_ABOVE_4G | 5LEVEL | 5LEVEL_ENABLED`), `cmdline_size` 0x7FF, `pref_address` 0x100_0000, `init_size` 0x184_D000, `kernel_info_offset` 0x8B_A1C0. Payload: gzip, 0x8B_24BD bytes (`payload_length`). Derived: booting the bzImage adds gzip decompression of ~9 MB into ~28 MB inside the guest before `startup_64`. The vmlinux avoids that step entirely.

---

## 8. Boot-time cost facts

| Fact | Value | Source |
|---|---|---|
| Spec hosts | M5D.metal (x86_64, hyperthreading off) and M6G.metal (arm64) | fc:SPECIFICATION.md:8-11 |
| VMM start | ≤ 8 CPU ms to API-socket availability. Wall clock "spanning 6 ms to 60 ms, with typical durations around 12 ms". | fc:SPECIFICATION.md:13-17 |
| Boot | ≤ **125 ms** from the InstanceStart API call to guest `/sbin/init`, with 1 vCPU, 128 MiB, serial console disabled, and a minimal kernel and rootfs | fc:SPECIFICATION.md:24-26, 37-42; fc:FAQ.md:62-64 |
| VMM memory overhead | ≤ 5 MiB for the VMM threads (1 vCPU, 128 MiB guest) | fc:SPECIFICATION.md:27-35 |
| Boot-timer device | x86_64 MMIO pseudo-device at **0xC000_0000** (4 KiB), attached before every other device. A **byte write of 123 at offset 0** logs `Guest-boot-time = now − request_ts` in wall-clock µs and process CPU µs. `request_ts` is taken at entry to `build_microvm_for_boot`, so VM construction is included. | fc:src/vmm/src/devices/pseudo/boot_timer.rs:11-44; fc:src/vmm/src/builder.rs:150, 221-226; fc:src/vmm/src/arch/x86_64/layout.rs:114-115 |
| Guest side | `/usr/local/bin/init` mmaps `/dev/mem` at 0xC0000000 (0x40000000 on aarch64), writes 123, then execs `/sbin/init` | fc:resources/rootfs/overlay/usr/local/bin/init.c:15-43 |
| Boot-time test matrix | (1 vCPU, 128 MiB), (1, 1024), (2, 2048) and (4, 4096) × pmem or block root × THP off or on × PCI on or off, 10 boots each. ACPI kernels only. Net and entropy devices attached, threads pinned. Marked `nonci`. | fc:tests/integration_tests/performance/test_boottime.py:11-21, 102-143, 153-220; fc:tests/framework/artifacts.py:163 |
| Test cmdline | `reboot=k panic=1 nomodule 8250.nr_uarts=0 i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd swiotlb=noforce cryptomgr.notests`, plus `init=/usr/local/bin/init`. Firecracker appends `pci=off` when PCI is off. | test_boottime.py:16-19, 119-134; fc:src/vmm/src/builder.rs:217-219 |
| **Thresholds** | **None, on any architecture.** The test only checks that the boot-time log line appears within 50 × 0.1 s. Regressions are decided by an A/B permutation test (p < 0.01 and a mean change > 5%). | test_boottime.py:34-48; fc:tools/ab_test.py:121-126, 261-285, 627-629; fc:.buildkite/pipeline_perf.py:73-77 |
| Historical threshold | `MAX_BOOT_TIME_US = 170000` ("bigger than the default 150_000", to allow ftrace kernels): 1 vCPU, 128 MiB, kernel 4.14. Removed in commit 359d79d4c (2024-09). | fc@359d79d4c^:tests/integration_tests/performance/test_boottime.py:13-16, 132-151 |
| Also recorded | `build_time` ("build microvm for boot"), `resume_time` ("boot microvm"), and systemd-analyze kernel and userspace times | test_boottime.py:71-99, 209-218; fc:src/vmm/src/builder.rs:379-385 |
| i8042 probing | "a few tens of milliseconds". Avoided with `i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd`. | fc:docs/api_requests/actions.md:47-55 |
| Serial | Disabled by default "for boot time performance reasons" (`8250.nr_uarts=0`) | fc:FAQ.md:124-126; fc:docs/prod-host-setup.md:36-40 |
| bzImage vs vmlinux | Decompression costs boot time and guest memory; vmlinux is recommended | fc:docs/rootfs-and-kernel-setup.md:13-16 |
| Hugepages | 2 MiB hugetlbfs backing improves boot time "up to 50%" | fc:docs/hugepages.md:41-47 |
| Host-side regression | On 6.1 hosts `KVM_CREATE_VM` is slow: creating the `kvm-nx-lpage-recovery` thread attaches it to a cgroup. Mitigations: cgroup `favordynmods`, or `kvm.nx_huge_pages=never`. | fc:docs/prod-host-setup.md:389-458 |
| Same cost on Linux 7.2 | The thread is now a vhost task created by `call_once` on the VM's **first `KVM_RUN`**, before the `immediate_exit` check, and skipped only when the NX mitigation is hard-disabled | x86.c:11956-11965; arch/x86/kvm/mmu/mmu.c:7967-7994 |
| PVH guest cost | 256 CPUID exits in `xen_prepare_pvh()` (§2.3) | §2.3 |
| KVM PIT | `KVM_CREATE_PIT2` spawns a `kvm-pit/<pid>` kthread worker per VM | arch/x86/kvm/i8254.c:753-755; api:3043-3049 |

Derived:
- Firecracker commits **no x86_64 boot-time number**, only the 125 ms spec and A/B deltas. shards must measure its own baseline on the same kernel (§7) with the same method: a guest write to a boot-marker page, timed from the start of VM construction. Report n, p50, p90, p99 and max.
- Two host-side costs land on the boot path and belong in the benchmark: the first `KVM_RUN` (NX-recovery task) and the PIT kthread. Both can move into the warm VMM process (D2).

---

## 9. Implementation checklist

Every step here is **Derived**, each from the section it cites. The order is the order the code runs. The addresses are the §6.4 map.

1. **Probe the host** (§1.1, §1.2).
   - Open `/dev/kvm` with `O_RDWR|O_CLOEXEC`. On `EACCES`, fail with an actionable message: on the CI runner the node is `crw-rw---- root kvm` (CI:). The runner then needs to be in group `kvm` or relax the node mode.
   - Require `KVM_GET_API_VERSION` = 12.
   - Require these caps: IRQCHIP 0, USER_MEMORY 3, SET_TSS_ADDR 4, EXT_CPUID 7, MP_STATE 14, IRQFD 32, IOEVENTFD 36, SET_IDENTITY_MAP_ADDR 37, IMMEDIATE_EXIT 136. Also PIT2 33 if a PIT is created (§5).
   - Cache `KVM_GET_VCPU_MMAP_SIZE` and `KVM_GET_SUPPORTED_CPUID` (`nent` = 256) once per process. Unlike Firecracker, check IMMEDIATE_EXIT, because the kick depends on it.
2. **Create the VM**, before any vCPU (§1.1).
   - `KVM_CREATE_VM(0)`.
   - `KVM_SET_TSS_ADDR(0xFFFB_D000)`, passed **by value**.
   - `KVM_SET_IDENTITY_MAP_ADDR(&0xFFFB_C000)`, passed **by pointer**.
   - `KVM_CREATE_IRQCHIP`. Treat ENOTTY as "host lacks `CONFIG_KVM_IOAPIC`".
   - Then the PIT decision from §5.
3. **Memory** (§1.3, §6).
   - Back guest RAM with `mmap(MAP_PRIVATE|MAP_ANONYMOUS|MAP_NORESERVE)`, or with file-backed `MAP_PRIVATE` for snapshots (D7).
   - Slot 0 = [0, min(RAM, 3 GiB)). Slot 1 = [4 GiB, …) only when RAM > 3 GiB.
   - Leave [3 GiB, 4 GiB) for MMIO, IOAPIC, LAPIC, TSS and the identity page. A slot overlapping a KVM private slot fails with EEXIST (kmain:1981-1992, 2092-2094; x86.c:13405-13413).
   - Keep the low 21 bits of GPA and HVA equal (api:1409-1411).
4. **Load the kernel** (§2.1, §7).
   - Validate ELF64, LE, `EM_X86_64`, `ET_EXEC`.
   - Copy each PT_LOAD's `p_filesz` bytes to `p_paddr`. Fresh anonymous RAM is already zero up to `p_memsz`.
   - Reject segments that fall outside RAM or below 1 MiB.
   - The entry is `e_entry`. Record the PVH note for diagnostics only (§2.5).
5. **Boot structures** (§2.2, §6.4).
   - GDT at 0x500: `[0]` null, `[2]` = `__BOOT_CS` 0x10 (64-bit code, `0x00AF9B000000FFFF`), `[3]` = `__BOOT_DS` 0x18 (`0x00CF93000000FFFF`). The selector values are the kernel's own (arch/x86/include/asm/segment.h:22-27). No TSS slot is needed: only the real-mode setup and the decompressor ever load `__BOOT_TSS` (arch/x86/boot/pm.c:77, pmjump.S:35, compressed/head_64.S:246), never `startup_64`. VM entry reads TR from SREGS (step 7).
   - An empty IDT at 0x520.
   - PML4/PDPT/PD at 0x9000/0xA000/0xB000, identity-mapping [0, 1 GiB) with 2 MiB pages.
   - Command line at 0x20000.
   - Zero page at 0x7000: zero all 4 KiB first, then `type_of_loader` 0xFF, `loadflags` `LOADED_HIGH`, `version` 0x020C, `cmd_line_ptr`, `ramdisk_image/size`, `acpi_rsdp_addr`, and e820.
   - e820: RAM [0, 0x9FC00), reserved [0x9FC00, 0x100000), RAM [1 MiB, 3 GiB ∧ RAM), RAM above 4 GiB.
6. **ACPI** (§3.5).
   - RSDP rev 2 at 0xE0000 → XSDT → FADT (HW_REDUCED_ACPI, revision 6) + MADT (one LAPIC per vCPU, APIC id = index; IOAPIC id 0 at 0xFEC0_0000, GSI base 0; no overrides) + DSDT.
   - DSDT: one `LNRO0005` per virtio-mmio slot (edge, active-high GSI); `PNP0501` for COM1 when a console is configured; `PNP0303` only if an i8042 is advertised (§4, §5).
   - Checksum everything. Emit no MP table and no `virtio_mmio.device=` (§3.4, §7.5).
7. **vCPUs**, created strictly in index order (as D5 does for HVF).
   - For each vCPU: `KVM_CREATE_VCPU(i)`, then mmap `kvm_run`, then `KVM_SET_CPUID2`. The CPUID is the template with leaf 1 EBX[31:24] = i, leaves 0xB/0x1F topology with x2APIC ID = i, and leaf 1 ECX bits 31 (hypervisor) and 24 (TSC-deadline, if `KVM_CAP_TSC_DEADLINE_TIMER`) set (§1.4).
   - **BSP only:** `KVM_GET_SREGS` then overwrite every field explicitly, then `KVM_SET_SREGS`. Values:
     - CR0 = 0x8000_0011 (not ORed), CR3 = 0x9000, CR4 = 0x20, EFER = 0x500.
     - CS = 0x10 (L=1); DS/ES/SS = 0x18; TR = selector 0x20, base 0, limit 0x67, type 11 (busy TSS), present, TI = 0. TR exists only in SREGS; KVM's own validity model is vmx.c:3891-3907.
     - GDT base 0x500 limit 31; IDT base 0x520.
   - Then `KVM_SET_REGS`: RIP = `e_entry`, RSI = 0x7000, RFLAGS = 0x2.
   - **APs: nothing.** They sit UNINITIALIZED until the guest's INIT/SIPI (§1.4).
   - Skip `KVM_SET_FPU` (already the default), `KVM_SET_LAPIC` and the boot MSR list unless a boot test proves one is needed (§1.8).
8. **Devices** (§1.7, §3, §5).
   - PIO bus: COM1 at 0x3F8-0x3FF, GSI 4, reusing `crates/vmm/src/devices/serial.rs` (16550) with register offset = port − 0x3F8. The reset/power-off device comes from §4.
   - MMIO bus: control page at 0xC000_0000; virtio-mmio from 0xC000_1000, GSIs 5-23.
   - Interrupts: irqfd per GSI, with no RESAMPLE (edge).
   - Notify: `KVM_IOEVENTFD` per queue. Measure `len` 0 (fast path, no DATAMATCH) against Firecracker's `len` 4 + DATAMATCH.
9. **vCPU threads and run loop** (§1.5, §1.6).
   - Install a `SIGRTMIN + k` handler that stores `immediate_exit = 1`. Never block that signal.
   - Before each `KVM_RUN`, drain requests.
   - `EINTR`: clear `immediate_exit`. `EAGAIN`: re-enter.
   - `IO`: both directions. For IN, fill `count × size` bytes at `data_offset`.
   - `MMIO`: fill `data[..len]` on reads.
   - `SHUTDOWN` (triple fault): stop or reset per §4.
   - `FAIL_ENTRY` / `INTERNAL_ERROR`: fatal, with a REGS/SREGS dump. `SYSTEM_EVENT`: generic.
   - During warm-up, issue one `KVM_RUN` with `immediate_exit = 1` to create the NX-recovery task off the request path (§1 bug-magnet 13).
10. **Power-off and reset** (§4).
    - ACPI S5 is the primary power-off path, because the tables are mandatory anyway (§7.5):
      - FADT: `SLEEP_CONTROL_REG` and `SLEEP_STATUS_REG` as 8-bit SystemIO GAS on a VMM-chosen free port pair.
      - DSDT: `Name (_S5, Package () { 5, 0 })` (any SLP_TYP ≤ 7).
      - Result: the guest's `reboot(RB_POWER_OFF)` registers `acpi_power_off` and writes `(5 << 2) | 0x20` = 0x34 to the control port. The VMM stops the VM with "powered off".
      - shards-init keeps `RB_POWER_OFF` on both architectures (PSCI SYSTEM_OFF on arm64).
    - Decode port 0x64: reads return 0 (IBF clear), and a write of 0xFE means "guest reset". Every restart, emergency restart and `panic=` path ends there. Don't advertise `PNP0303`, and leave the FADT 8042 flag clear, so the guest never binds an i8042 driver (§3.1).
    - `KVM_EXIT_SHUTDOWN` (triple fault) means guest crash: stop the VM.
    - Fallback for a future ACPI-less kernel: shards-init writes a power-off marker to the control page before `reboot()` (§4.6 option a).
11. **Command line and legacy devices** (§5).
    - CPUID timing bits: keep leaf 1 ECX[24] (TSC-deadline) and ECX[31] (hypervisor), leaf 6 EAX[2] (ARAT; KVM reports 0x4), 0x8000_0007 EDX[8] (invariant TSC) and KVM's leaves 0x4000_0000/1 (kvmclock). The guest then skips TSC, delay-loop and APIC-timer calibration and never needs a PIT. Dropping ECX[24] costs 100 ms of APIC calibration (§5.1-5.4).
    - **No `KVM_CREATE_PIT2`** initially. With the HW-reduced FADT from step 6, `timer_init` is a no-op, so the guest never programs the PIT (arch/x86/kernel/acpi/boot.c:1407-1422; §5.4). This also saves the per-VM `kvm-pit` kthread. Keep a switch until F5-2 is measured.
    - Command line: `reboot=k panic=-1 nomodule swiotlb=noforce cryptomgr.notests pci=off` (drop `pci=off` once virtio-pci exists). Then either `console=ttyS0 quiet` (debug console, with `PNP0501` in the DSDT) or `8250.nr_uarts=0` (production).
    - No `i8042.*` flags are needed: with the FADT 8042 flag clear and no `PNP0303`, the driver exits with no port access. **Never pass `i8042.nopnp`**, which bypasses that exit (§5.6).
    - Unclaimed PIO (the CMOS 0x70/0x71 warm-reset writes at AP bring-up, anything else): IN returns 0, OUT is ignored, and both are counted so the boot benchmark can list them (F5-3).
12. **Tests and benchmarks** (CLAUDE.md rules).
    - Pin `x86_64/vmlinux-6.18.48` (sha256 in §7.2) next to the aarch64 pin in `crates/shards/tests/common/mod.rs`.
    - On `ubuntu-24.04`, make `/dev/kvm` accessible to the runner user in CI.
    - Boot benchmark: guest control-page marker timed from the start of VM construction, reporting n/p50/p90/p99/max. Include the first `KVM_RUN` separately (§8).

---

## 10. UNVERIFIED

Nothing below rests on a primary source that was read. Each item needs a runtime probe on a KVM host, a measurement, or a source not yet retrieved before code depends on it.

### KVM API and boot protocol (§1, §2)

| # | Item | How to settle |
|---|---|---|
| K1 | PROBE values come from compiling the 7.2-rc4 uapi headers with clang for `x86_64-linux-gnu` on macOS. They were cross-checked against kvm-bindings' layout tests and kvm-ioctls' definitions (§1.9), but never observed on a Linux target. | Assert them in a Linux-target unit test |
| K2 | **Nothing was executed on a KVM host.** All KVM, Linux and Firecracker behaviour comes from reading source at the cited revisions. | First E2E boot on the CI runner |
| K3 | The CI runner's host: CPU vendor, `kvm_intel.unrestricted_guest` (decides whether `KVM_SET_TSS_ADDR` does anything), host kernel version, and whether the `runner` user is in group `kvm`. CI: shows only `crw-rw---- root kvm` for `/dev/kvm`. | Print `/proc/cpuinfo` model, `/sys/module/kvm_*/parameters/*`, `uname -r` and `id` in the CI "Host virtualization" step |
| K4 | The host kernel on the runner is older than 7.2. Behaviour read in 7.2 that may differ there: where the NX-recovery task is created (first `KVM_RUN` vs `KVM_CREATE_VM`, see FC-2 below), the `CONFIG_KVM_IOAPIC` gate, and the `KVM_GET_SUPPORTED_CPUID` clamping. | Read the runner kernel's source, or probe |
| K5 | The hardware VM-entry checks on segment and TR state were not read: the Intel SDM Vol. 3 guest-state checks and the AMD APM Vol. 2 VMRUN consistency checks. KVM's own `tr_valid()` model (vmx.c:3891-3907) stands in for them. | Read the SDM checks, or rely on `KVM_EXIT_FAIL_ENTRY` in tests |
| K6 | Whether Linux under KVM needs any of Firecracker's boot MSRs (MTRRdefType, MISC_ENABLE and the rest) or its LAPIC LINT programming. No source says they are required. | Boot test without them (checklist step 7) |
| K7 | Cost of PVH's 256 CPUID exits (§2.3), and of entering with CR0.CD\|NW set as Firecracker's 64-bit path does (§2.4). | Committed boot benchmark, A/B |
| K8 | Latency of irqfd on IOAPIC pins (workqueue) against `KVM_IRQ_LINE`, and of `len`-0 fast-MMIO ioeventfd against `len`-4 DATAMATCH. | Committed microbenchmark |

### Firmware tables and memory layout (§3, §6)

| # | Item |
|---|---|
| F3-1 | The guest-side result of describing a device on both the cmdline and in the DSDT. Only Firecracker's docs describe it (fc:docs/kernel-policy.md:216-221). That the ACPI copy binds first is inferred from initcall order, not observed. |
| F3-2 | "Under HW-reduced ACPI a cmdline `<irq>` does not name the GSI" is inferred from the DYNAMIC-domain code path, not run. The same applies to a legacy 8250 at 0x3F8 with IRQ 4 and no `PNP0501` node. |
| F3-3 | Why Firecracker says `CONFIG_PCI` is required for ACPI on x86 (fc:docs/kernel-policy.md:132-135). `menuconfig ACPI` doesn't depend on PCI (drivers/acpi/Kconfig:9-17), and the cause was not found. |
| F3-4 | The boot-time cost of ACPI (table load, AML namespace, PNP) versus MP table plus cmdline in a microVM. Not measured, and needed before the tuned-kernel decision. |
| F3-5 | The Intel MP Spec 1.4 itself was not retrieved. All MP layouts and semantics come from Linux headers and parser code. |
| F6-1 | VMGenID at 0xDFFF0 and VMClock at 0xDE000 in real boots is arithmetic from allocation order and the LastMatch policy. The `vm-allocator` crate source was not read. |
| F6-3 | The MAXPHYADDR of the GitHub `ubuntu-24.04` runner CPUs. Read CPUID 0x80000008 at runtime rather than assume it. |
| F6-4 | The size of the real-mode trampoline blob was not computed. Firecracker's free window [64 KiB, 0x9F000) is assumed to be ample. |

### Power-off and reset (§4)

| # | Item |
|---|---|
| F4-1 | Wall-clock cost of the guest `reboot` path (`device_shutdown`, `stop_other_cpus`) up to the 0xFE write. Not measured. |
| F4-2 | Whether any shipped guest userspace (busybox, systemd) calls `RB_POWER_OFF` on x86 where shards expects `RB_AUTOBOOT`. Only shards-init and testguest were read. |
| F4-3 | ACPI 6.5 citations come from the HTML edition by section and table number; PDF page numbers were not retrieved. |

### Timekeeping and legacy devices (§5)

| # | Item |
|---|---|
| F5-1 | Real i8042 probe time against a Firecracker-like device without the flags. 0.5 s per timeout is derived; Firecracker claims "tens of ms". Measure. |
| F5-2 | Cost of `KVM_CREATE_PIT2` (ioctl + `kvm-pit` kthread) versus 4 extra PIO exits. Not measured. |
| F5-3 | Full list of legacy ports the CI kernel touches during boot (0x61, 0x80, 0x70/0x71, 0x2F8…). Needs an exit trace on a real boot. |
| F5-4 | The exact `CMOS_WRITE` port sequence (index 0x70, then data 0x71). `asm/mc146818rtc.h` was not read. |
| F5-5 | Whether GitHub ubuntu-24.04 runners' (nested) KVM reports invariant TSC, TSC-deadline and a stable kvmclock. Probe at runtime. |

### Firecracker side (§1.8, §2.4, §8)

| # | Item |
|---|---|
| FC-2 | Which host kernel the `ubuntu-24.04` runners use, and in which Linux release the NX-recovery thread creation moved from `KVM_CREATE_VM` to the first `KVM_RUN` (only 7.2 was read). See K3/K4. |
| FC-3 | The sigaction flags vmm-sys-util's `register_signal_handler` uses. That crate was not fetched. |
| FC-5 | External Firecracker x86_64 boot-time baselines (e.g. A/B dashboards). None are committed in the repo (grep `guest_boot_time`). |

FC-1 is settled in §7.5: the published kernel is the in-repo 6.18 config plus `ci.config` and `nvme.config`, and the boot-relevant options are identical. FC-4 is K7 and FC-6 is K2.
