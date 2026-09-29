# HVF (arm64) and KVM (arm64) ground-truth reference for shards

Status: research reference, compiled 2026-09-28. Every row cites a primary source that was read during compilation. Anything not verifiable from a primary source is marked **UNVERIFIED** and collected in §6.

## Citation conventions

| Prefix / form | Meaning |
|---|---|
| `hv_*.h:N` | `/Library/Developer/CommandLineTools/SDKs/MacOSX26.4.sdk/System/Library/Frameworks/Hypervisor.framework/Headers/<file>:<line>` (the SDK symlinked as `MacOSX.sdk`; header files dated 2026-02-24) |
| `hv_kern_types.h:N` | `MacOSX26.4.sdk/usr/include/arm64/hv/hv_kern_types.h:<line>` (included by every arm64 Hypervisor header) |
| `AD:<page>` | Apple developer documentation `https://developer.apple.com/documentation/hypervisor/<page>`, read through its DocC JSON (`/tutorials/data/documentation/hypervisor/<page>.json`) on 2026-09-28. Where AD and the headers disagree, both are cited. |
| `path/in/tree:N` | Linux 7.2-rc4 at `/Users/adalundhe/Projects/linux` (Makefile VERSION=7, PATCHLEVEL=2, EXTRAVERSION=-rc4) |
| `<DocID> <issue> §x (p.N)` | Arm specification PDF, as retrieved (see §2 header) |
| `DTSpec v0.4 §x` | Devicetree Specification v0.4 PDF (devicetree.org release asset) |
| `HOST:` | Observation on the dev host (`sysctl`; macOS 26.4.1 build 25E253, Mac17,6, Apple M5 Max). A measurement, not a contract. |
| (derived) | Arithmetic or logic applied to a cited value, e.g. decoding an enum constant. Not a quoted fact. |

libkrun and Firecracker were used only to decide what to look up. They are never cited as sources.

## Contents

1. Hypervisor.framework arm64 API
2. Arm architecture ground truth (ESR, PSCI, SMCCC, GICv3, Generic Timer, PL011, PL031)
3. Linux arm64 boot contract, DT bindings, FDT format
4. KVM arm64 API (+ x86_64 headline)
5. HVF vs KVM: semantic differences a shared VMM core must abstract
6. Consolidated UNVERIFIED list

---

## 1. Hypervisor.framework (HVF) arm64 API

SDK: MacOSX26.4. On arm64 `Hypervisor.h` includes `hv_gic*.h`, `hv_sme_config.h`, `hv_vcpu*.h`, `hv_vm*.h`, and `hv_vm_allocate.h` (`Hypervisor.h:10-25`). `hv.h`, `hv_arch_vmx.h`, `hv_arch_x86.h`, `hv_error.h`, `hv_intr.h`, `hv_types.h`, and `hv_vmx.h` are x86_64-only (`Hypervisor.h:26-34`) and are ignored here. The arm64 `hv_return_t` and `hv_memory_flags_t` come from `hv_kern_types.h`, not from `hv_error.h` or `hv_types.h`.

### 1.1 Process and threading model, limits

| Rule | Detail | Source |
|---|---|---|
| One VM per process | "There can only be one virtual machine at a time per process." `hv_vm_create` "Creates a VM instance for the current process." | AD:(root) Overview › Virtual Resource Mapping; `hv_vm.h:27-33` |
| VM teardown precondition | `hv_vm_destroy` "Requires all vCPUs be destroyed." | `hv_vm.h:35-42` |
| vCPU is bound to its creating thread | `hv_vcpu_create` "Creates a vCPU instance for the current thread". "Each thread can only have one vCPU associated at a time." | `hv_vcpu.h:19-29` |
| Owning-thread rule | "Call functions that operate on the vCPU from the same thread with the exception of `hv_vcpus_exit`." Per-function "Must be called by the owning thread" appears at `hv_vcpu.h:36,48,60,72,87,102,118,146,162,178,194,209,226,242,257,273,285,296,307,319,329,339,351,366,389`. | AD:vcpu-management; `hv_vcpu.h` lines listed |
| No dispatch queues | "Don't use vCPUs on dispatch queues, because work from a single queue can run on different threads." | AD:vcpu-management (Warning) |
| Cross-thread kick | `hv_vcpus_exit(vcpus, count)` forces an immediate exit. If a target vCPU is not running, its next `hv_vcpu_run` returns immediately without entering the guest. The exit reason is `HV_EXIT_REASON_CANCELED`. | `hv_vcpu.h:371-381`; `hv_vcpu_types.h:39-40` |
| vtimer mask/offset calls | The header doesn't state a thread rule for `hv_vcpu_{get,set}_vtimer_{mask,offset}`. Treat them as owning-thread, per the AD rule above. | `hv_vcpu.h:394-449` (no thread note); AD:vcpu-management |
| VM-wide GIC calls | `hv_gic_set_spi`, `hv_gic_send_msi`, `hv_gic_{get,set}_distributor_reg`, `hv_gic_{get,set}_msi_reg`, `hv_gic_get_redistributor_base`, `hv_gic_set_state`, and `hv_gic_reset` carry no owning-thread note. Per-vCPU GIC register calls (redistributor, ICC, ICH, ICV) do carry the note. | `hv_gic.h:51-276` (thread notes at `123,139,155,167,182,197,212,227`) |
| Per-vCPU IRQ/FIQ line injection | `hv_vcpu_set_pending_interrupt` is owning-thread only. Pending state is "automatically cleared after hv_vcpu_run returns"; set it before every `hv_vcpu_run`. | `hv_vcpu.h:301-312` |
| Entitlement | "All process must have the `com.apple.security.hypervisor` entitlement to use Hypervisor API." | AD:(root) Requirements |
| Runtime availability | `sysctl kern.hv_support` | AD:(root) Requirements; HOST: `kern.hv_support: 1` |
| Max vCPUs | `hv_vm_get_max_vcpu_count(uint32_t *)` is a runtime query. No constant is published. | `hv_vm.h:19-25` |
| Exit record | `hv_vcpu_create` returns a pointer to this vCPU's `hv_vcpu_exit_t`. "The function `hv_vcpu_run` updates this structure on return." | `hv_vcpu.h:22,28`; AD:hv_vcpu_create(_:_:_:) |
| GIC ordering | `hv_gic_create` must run after `hv_vm_create` and before any `hv_vcpu_create`, or it errors. Topology is final once vCPUs run. "Destroy vcpus only when you are tearing down the virtual machine." | `hv_gic.h:35-43` |
| Host constants (observed) | `kern.hv.ipa_size_16k: 4398046511104` (2^42, a 42-bit IPA with 16 KiB granule). `kern.hv.ipa_size_4k: 1099511627776` (2^40, 40-bit with 4 KiB granule). `hw.pagesize: 16384`. `hw.tbfrequency: 24000000`. `kern.hv.max_address_spaces: 128` (meaning undocumented). | HOST |

### 1.2 Function inventory (all arm64 functions in SDK 26.4, plus macOS 27 additions from AD)

Availability comes from `API_AVAILABLE(macos(x))` on each declaration and matches AD `introducedAt` for every function checked.

| Function | Decl | macOS | Semantics / constraints |
|---|---|---|---|
| `hv_vm_get_max_vcpu_count(uint32_t*)` | `hv_vm.h:25` | 11.0 | Max vCPUs supported. |
| `hv_vm_create(hv_vm_config_t _Nullable)` | `hv_vm.h:33` | 11.0 | Creates the VM for the current process. NULL means the default config. AD's `config` text ("must be nil") predates `hv_vm_config_t` (13.0). |
| `hv_vm_destroy(void)` | `hv_vm.h:42` | 11.0 | Requires all vCPUs destroyed first. |
| `hv_vm_map(void *addr, hv_ipa_t ipa, size_t size, hv_memory_flags_t flags)` | `hv_vm.h:53` | 11.0 | `addr` and `ipa` must be page-aligned. `size` must be a multiple of the page size (`hv_vm.h:45-49`). "The host memory must encompass a single VM region, typically allocated with `mmap` or `mach_vm_allocate` instead of `malloc`." (AD:hv_vm_map(_:_:_:_:)). Guest access outside mapped ranges causes `hv_vcpu_run` to exit, and the VMM emulates it (AD:(root)). |
| `hv_vm_unmap(hv_ipa_t, size_t)` | `hv_vm.h:62` | 11.0 | Same alignment rules. |
| `hv_vm_protect(hv_ipa_t, size_t, hv_memory_flags_t)` | `hv_vm.h:72` | 11.0 | Changes permissions of a mapped region or subregion (AD:memory-management). This is the only primitive available for write-protect-based dirty tracking (§1.9). |
| `hv_vm_config_create(void)` | `hv_vm_config.h:25` | 13.0 | OS object. Release with `os_release`. |
| `hv_vm_config_get_max_ipa_size(uint32_t*)` | `hv_vm_config.h:36` | 13.0 | Max IPA bit length. "36 means only the least significant 36 bits of an IPA are valid, and covers a 64GB range." (`:31-33`) |
| `hv_vm_config_get_default_ipa_size(uint32_t*)` | `hv_vm_config.h:45` | 13.0 | Used when the IPA size isn't set. |
| `hv_vm_config_set_ipa_size(cfg, uint32_t bits)` | `hv_vm_config.h:56` | 13.0 | Must be ≤ max IPA size (`:52`). |
| `hv_vm_config_get_ipa_size(cfg, uint32_t*)` | `hv_vm_config.h:65` | 13.0 | |
| `hv_vm_config_get_el2_supported(bool*)` | `hv_vm_config.h:73` | 15.0 | Whether the platform supports guest EL2. |
| `hv_vm_config_get_el2_enabled(cfg, bool*)` | `hv_vm_config.h:82` | 15.0 | |
| `hv_vm_config_set_el2_enabled(cfg, bool)` | `hv_vm_config.h:108` | 15.0 | Also changes PMU handling. EL2 disabled: PMU accesses trap to the VMM as EC=0x18 exits. EL2 enabled: behavior depends on the guest-visible `ID_AA64DFR0_EL1.PMUVer`. 0 or invalid gives UNDEF to the guest. A valid value makes the framework emulate the PMU. `ID_AA64DFR0_EL1` is settable via `hv_vcpu_set_sys_reg` (`:90-105`). |
| `hv_vm_config_get_default_ipa_granule(hv_ipa_granule_t*)` | `hv_vm_config.h:125` | 26.0 | |
| `hv_vm_config_get_ipa_granule(cfg, hv_ipa_granule_t*)` | `hv_vm_config.h:134` | 26.0 | |
| `hv_vm_config_set_ipa_granule(cfg, hv_ipa_granule_t)` | `hv_vm_config.h:143` | 26.0 | `HV_IPA_GRANULE_4KB`=0, `HV_IPA_GRANULE_16KB`=1 (`:113-117`). The effect on `hv_vm_map` alignment is **UNVERIFIED** (§6). |
| `hv_vm_allocate(void **uvap, size_t, hv_allocate_flags_t)` | `hv_vm_allocate.h:51` | 12.1 (x86: 12.0) | Anonymous memory "suitable to be mapped as guest memory", `VM_PROT_DEFAULT`, "enables accurate memory accounting". Size must be a multiple of `PAGE_SIZE`. Free with `hv_vm_deallocate`. `HV_ALLOCATE_DEFAULT`=0 is the only flag (`:27-49`). |
| `hv_vm_deallocate(void*, size_t)` | `hv_vm_allocate.h:60` | 12.1 | |
| `hv_vcpu_create(hv_vcpu_t*, hv_vcpu_exit_t**, hv_vcpu_config_t _Nullable)` | `hv_vcpu.h:28` | 11.0 | Binds to the calling thread. One vCPU per thread. |
| `hv_vcpu_destroy(hv_vcpu_t)` | `hv_vcpu.h:39` | 11.0 | Owning thread. |
| `hv_vcpu_get_reg` / `hv_vcpu_set_reg(vcpu, hv_reg_t, u64)` | `hv_vcpu.h:51` / `:63` | 11.0 | Owning thread. See §1.5 for `hv_reg_t`. |
| `hv_vcpu_get_simd_fp_reg` / `hv_vcpu_set_simd_fp_reg(vcpu, hv_simd_fp_reg_t, hv_simd_fp_uchar16_t)` | `hv_vcpu.h:78` / `:94` | 11.0 | Owning thread. In streaming SVE mode, Qn aliases the low 128 bits of Zn (`:74-75`). |
| `hv_vcpu_get_sme_state` / `hv_vcpu_set_sme_state(vcpu, hv_vcpu_sme_state_t*)` | `hv_vcpu.h:110` / `:136` | 15.2 | `{streaming_sve_mode_enabled (PSTATE.SM), za_storage_enabled (PSTATE.ZA)}` (`hv_vcpu_types.h:177-186`). Entering or leaving streaming mode zeroes all Z/P and sets all FPSR flags (`:120-122`). Returns `HV_UNSUPPORTED` without SME. |
| `hv_vcpu_{get,set}_sme_z_reg(vcpu, hv_sme_z_reg_t, u8*, len)` | `hv_vcpu.h:152` / `:168` | 15.2 | Streaming mode only. `len` must equal max SVL bytes. |
| `hv_vcpu_{get,set}_sme_p_reg(vcpu, hv_sme_p_reg_t, u8*, len)` | `hv_vcpu.h:184` / `:200` | 15.2 | Streaming mode only. `len` = max SVL / 8. |
| `hv_vcpu_{get,set}_sme_za_reg(vcpu, u8*, len)` | `hv_vcpu.h:217` / `:234` | 15.2 | Requires PSTATE.ZA=1. `len` = SVL × SVL bytes. Streaming mode not required. |
| `hv_vcpu_{get,set}_sme_zt0_reg(vcpu, hv_sme_zt0_uchar64_t*)` | `hv_vcpu.h:249` / `:264` | 15.2 | Requires PSTATE.ZA=1. 64 bytes. |
| `hv_vcpu_get_sys_reg` / `hv_vcpu_set_sys_reg(vcpu, hv_sys_reg_t, u64)` | `hv_vcpu.h:276` / `:288` | 11.0 | Owning thread. See §1.6. |
| `hv_vcpu_get_pending_interrupt` / `hv_vcpu_set_pending_interrupt(vcpu, hv_interrupt_type_t, bool)` | `hv_vcpu.h:299` / `:312` | 11.0 | Owning thread. Sets the vCPU's virtual IRQ/FIQ line, which is the injection path when the VMM emulates the interrupt controller itself. The header doesn't say how it interacts with `hv_gic`. Auto-cleared after each run (`:307-309`). With EL2 enabled, the `hv_vcpu` interrupt functions "are unsupported for injecting interrupts to a nested guest" (`hv_gic.h:32-33`). |
| `hv_vcpu_{get,set}_trap_debug_exceptions(vcpu, bool)` | `hv_vcpu.h:322` / `:332` | 11.0 | "The equivalent system register is `MDCR_EL2.TDE`" (AD:hv_vcpu_set_trap_debug_exceptions(_:_:)). |
| `hv_vcpu_{get,set}_trap_debug_reg_accesses(vcpu, bool)` | `hv_vcpu.h:344` / `:356` | 11.0 | Covers `DBGBCR/BVR/WCR/WVR<n>_EL1` and `MDSCR_EL1` (`:340-341`). |
| `hv_vcpu_run(hv_vcpu_t)` | `hv_vcpu.h:369` | 11.0 | Blocks until the next exit or a cancel by `hv_vcpus_exit`. Owning thread. A `VTIMER_ACTIVATED` exit auto-masks the vtimer (AD:hv_vcpu_run(_:)). |
| `hv_vcpus_exit(hv_vcpu_t*, uint32_t count)` | `hv_vcpu.h:381` | 11.0 | Any thread. See §1.1. |
| `hv_vcpu_get_exec_time(vcpu, u64*)` | `hv_vcpu.h:392` | 11.0 | Header: cumulative time "in units of mach_absolute_time()" (`:384`). AD says "in nanoseconds". **Conflict**: trust the header and convert with the timebase. |
| `hv_vcpu_{get,set}_vtimer_mask(vcpu, bool)` | `hv_vcpu.h:401` / `:426` | 11.0 | See §1.7. |
| `hv_vcpu_{get,set}_vtimer_offset(vcpu, u64)` | `hv_vcpu.h:436` / `:449` | 11.0 | `CNTVCT_EL0 = mach_absolute_time() - vtimer_offset` (`:444-446`). AD: "corresponds to the value of the `CNTVOFF_EL2` register". |
| `hv_vcpu_config_create(void)` | `hv_vcpu_config.h:25` | 11.0 | |
| `hv_vcpu_config_get_feature_reg(cfg, hv_feature_reg_t, u64*)` | `hv_vcpu_config.h:55` | 11.0 | Returns the ID register value the vCPU will present. |
| `hv_vcpu_config_get_ccsidr_el1_sys_reg_values(cfg, hv_cache_type_t, u64 values[8])` | `hv_vcpu_config.h:65` | 11.0 | CCSIDR_EL1 per cache level. Needed because `CSSELR_EL1`/`CCSIDR_EL1` are virtualized. |
| `hv_sme_config_get_max_svl_bytes(size_t*)` | `hv_sme_config.h:30` | 15.2 | Max streaming vector length. `HV_UNSUPPORTED` without SME. |
| `hv_gic_create(hv_gic_config_t)` | `hv_gic.h:49` | 15.0 | In-framework GICv3: distributor, redistributors, MSI, and ICC system registers. Plus ICH when EL2 is on. One per VM. Ordering in §1.1. MSI works only if both the MSI base and the MSI INTID range are set (`hv_gic.h:22-47`). |
| `hv_gic_set_spi(uint32_t intid, bool level)` | `hv_gic.h:66` | 15.0 | Drives an SPI line. For edge-configured INTIDs, `true` makes an edge and `false` is ignored. INTIDs outside `hv_gic_get_spi_interrupt_range` or inside the MSI range return `HV_BAD_ARGUMENT` (`:51-64`). |
| `hv_gic_send_msi(hv_ipa_t address, uint32_t intid)` | `hv_gic.h:77` | 15.0 | `address` = GPA of `GICM_SET_SPI_NSR` in the MSI frame (`:73-74`). |
| `hv_gic_{get,set}_distributor_reg(hv_gic_distributor_reg_t, u64)` | `hv_gic.h:90` / `:103` | 15.0 | Enum values equal GICv3 register offsets. Raw offsets may be used when looping (`:84-87`). |
| `hv_gic_get_redistributor_base(vcpu, hv_ipa_t*)` | `hv_gic.h:114` | 15.0 | Call only after that vCPU's `MPIDR_EL1` affinity is set (`:110-111`). |
| `hv_gic_{get,set}_redistributor_reg(vcpu, hv_gic_redistributor_reg_t, u64)` | `hv_gic.h:130` / `:146` | 15.0 | Owning thread. Enum = offset from RD_base. The SGI_base frame registers are at `0x10000+` (§1.8). |
| `hv_gic_{get,set}_icc_reg(vcpu, hv_gic_icc_reg_t, u64)` | `hv_gic.h:158` / `:170` | 15.0 | Owning thread. The CPU-interface state that is **not** in the GIC state blob. |
| `hv_gic_{get,set}_ich_reg(vcpu, hv_gic_ich_reg_t, u64)` | `hv_gic.h:185` / `:200` | 15.0 | EL2-enabled VMs only. Owning thread. |
| `hv_gic_{get,set}_icv_reg(vcpu, hv_gic_icv_reg_t, u64)` | `hv_gic.h:215` / `:230` | 15.0 | EL2-enabled VMs only. Owning thread. |
| `hv_gic_{get,set}_msi_reg(hv_gic_msi_reg_t, u64)` | `hv_gic.h:239` / `:248` | 15.0 | `GICM_TYPER`, `GICM_SET_SPI_NSR`. |
| `hv_gic_set_state(const void*, size_t)` | `hv_gic.h:265` | 15.0 | Restore only after the GIC and vCPUs are created and before any vCPU runs. CPU registers must be restored compatibly. "can fail if a software update has changed the host" (`:250-263`). |
| `hv_gic_reset(void)` | `hv_gic.h:276` | 15.0 | Resets distributor, redistributor, and internal state on VM reset. |
| `hv_gic_config_create(void)` | `hv_gic_config.h:28` | 15.0 | Create after the VM exists (`:24`). |
| `hv_gic_config_set_distributor_base(cfg, hv_ipa_t)` | `hv_gic_config.h:40` | 15.0 | Aligned to `hv_gic_get_distributor_base_alignment`. |
| `hv_gic_config_set_redistributor_base(cfg, hv_ipa_t)` | `hv_gic_config.h:54` | 15.0 | Aligned to `hv_gic_get_redistributor_base_alignment`. The region holds redistributors "for all vCPUs supported by the virtual machine" (`:47-51`). |
| `hv_gic_config_set_msi_region_base(cfg, hv_ipa_t)` | `hv_gic_config.h:69` | 15.0 | Aligned to `hv_gic_get_msi_region_base_alignment`. |
| `hv_gic_config_set_msi_interrupt_range(cfg, base, count)` | `hv_gic_config.h:86` | 15.0 | Must lie inside the SPI range, or it errors (`:78-80`). |
| `hv_gic_get_distributor_size(size_t*)` | `hv_gic_parameters.h:26` | 15.0 | Runtime value. |
| `hv_gic_get_distributor_base_alignment(size_t*)` | `hv_gic_parameters.h:34` | 15.0 | Runtime value. |
| `hv_gic_get_redistributor_region_size(size_t*)` | `hv_gic_parameters.h:46` | 15.0 | Total size. "Each redistributor is two 64 kilobyte frames per vCPU and is contiguously placed." (`:40-43`) |
| `hv_gic_get_redistributor_size(size_t*)` | `hv_gic_parameters.h:54` | 15.0 | One redistributor. Expected 128 KiB per the text above; the actual value is a runtime query. |
| `hv_gic_get_redistributor_base_alignment(size_t*)` | `hv_gic_parameters.h:62` | 15.0 | |
| `hv_gic_get_msi_region_size(size_t*)` | `hv_gic_parameters.h:70` | 15.0 | |
| `hv_gic_get_msi_region_base_alignment(size_t*)` | `hv_gic_parameters.h:78` | 15.0 | |
| `hv_gic_get_spi_interrupt_range(u32 *base, u32 *count)` | `hv_gic_parameters.h:87` | 15.0 | |
| `hv_gic_get_intid(hv_gic_intid_t, u32*)` | `hv_gic_parameters.h:99` | 15.0 | INTIDs "reserved by the hypervisor framework" (`:95-96`). See §1.8. |
| `hv_gic_state_create(void)` | `hv_gic_state.h:30` | 15.0 | "The virtual machine must be in a stopped state." Returns no object if there is no GIC or the state isn't representable (`:22-26`). |
| `hv_gic_state_get_size(state, size_t*)` | `hv_gic_state.h:39` | 15.0 | |
| `hv_gic_state_get_data(state, void*)` | `hv_gic_state.h:57` | 15.0 | Opaque, versioned, "stable" blob of the whole device "except for the GIC cpu registers" (`:46-54`). |
| `hv_vcpu_get_serror` / `hv_vcpu_set_serror(vcpu, bool pending)` | not in SDK 26.4 | **27.0** | "Gets/Sets pending SError for a vcpu. Must be called by the owning thread." (AD:hv_vcpu_get_serror(_:_:), AD:hv_vcpu_set_serror(_:_:)) |
| `hv_vcpu_get_wait_for_interrupt_time(vcpu, u64*)` | not in SDK 26.4 | **27.0** | Cumulative time "spent at WFI instruction while waiting for interrupts in the units of mach_absolute_time()". Owning thread. "Returns HV_UNSUPPORTED if the VM was created without a GIC device" (AD:hv_vcpu_get_wait_for_interrupt_time(_:_:)). |
| `hv_vcpu_invalidate_tlb(vcpu, hv_tlbi_op_t, u64 param)` | not in SDK 26.4 | **27.0** | Invalidates TLB entries for the vCPU. With EL2 enabled, targets the guest hypervisor's entries, not nested guests'. Ops: `HV_TLBI_OP_{VMALLE1IS, VAE1IS, VALE1IS, VAAE1IS, VAALE1IS, ASIDE1IS, RVAE1IS, RVALE1IS, RVAAE1IS, RVAALE1IS}` (AD:hv_vcpu_invalidate_tlb(_:_:_:), AD:hv_tlbi_op_t, AD:(root) Variables). |
| `HV_FEATURE_REG_ID_AA64{ISAR2,MMFR3,MMFR4,PFR2}_EL1`, `HV_SYS_REG_ID_AA64{ISAR2,MMFR3,MMFR4,PFR2}_EL1` | not in SDK 26.4 | **27.0** | New ID registers (AD:hv_feature_reg_id_aa64isar2_el1 etc.). Numeric values are not visible in AD. **UNVERIFIED**. |

Declared but not a public API: `hv_kern_types.h:78-116` defines `hv_data_abort_notification_t` and `hv_vm_mem_access_msg_t` for "monitored memory regions" whose faults become Mach messages. They reference `hv_vm_monitor_data_abort` / `TRAP_HV_VM_MONITOR_DATA_ABORT`, and no public header declares either. The SDK therefore exposes no ioeventfd equivalent.

### 1.3 Return codes (arm64) — `hv_kern_types.h:50-63`

| Name | Value | AD meaning |
|---|---|---|
| `HV_SUCCESS` | 0 | success |
| `HV_ERROR` | 0xfae94001 | unsuccessful |
| `HV_BUSY` | 0xfae94002 | owning resource busy |
| `HV_BAD_ARGUMENT` | 0xfae94003 | invalid argument |
| `HV_ILLEGAL_GUEST_STATE` | 0xfae94004 | (arm64-only; no AD page) |
| `HV_NO_RESOURCES` | 0xfae94005 | host lacks resources |
| `HV_NO_DEVICE` | 0xfae94006 | no VM or vCPU available |
| `HV_DENIED` | 0xfae94007 | system didn't allow the operation (e.g. missing entitlement) |
| `HV_EXISTS` | 0xfae94008 | (arm64). The x86 `hv_error.h:35` uses 0x…08 for `HV_FAULT`. |
| `HV_UNSUPPORTED` | 0xfae9400f | not supported |

Encoding: `err_local | err_sub(0xba5) | code` (`hv_kern_types.h:40-41`). `hv_return_t` = `mach_error_t` (`:63`). AD meanings are from AD:hypervisor-errors pages.

### 1.4 Memory mapping

| Item | Value / rule | Source |
|---|---|---|
| `hv_memory_flags_t` | `HV_MEMORY_READ`=1<<0, `HV_MEMORY_WRITE`=1<<1, `HV_MEMORY_EXEC`=1<<2. The x86-only MAXPROT/UEXEC flags don't exist on arm64. | `hv_kern_types.h:65-76`; `hv_types.h:76-86` (x86) |
| Alignment | `addr`, `ipa` page-aligned. `size` a multiple of page size. | `hv_vm.h:44-72` |
| Page size | "Page" is the host page, 16 KiB on this host. `hv_vm_allocate` says "multiple of PAGE_SIZE". A 4 KiB IPA granule (26.0) *may* allow 4 KiB-aligned stage-2 mappings; **UNVERIFIED**. | HOST `hw.pagesize: 16384`; `hv_vm_allocate.h:46` |
| Host backing | Must be a single VM region (`mmap`/`mach_vm_allocate`), not `malloc`. Whether file-backed `MAP_PRIVATE` or `MAP_SHARED` mappings are accepted is not documented (**UNVERIFIED**). This matters for snapshot restore via mmap. | AD:hv_vm_map(_:_:_:_:) |
| Accounting | `hv_vm_allocate` "enables accurate memory accounting". | `hv_vm_allocate.h:42-48` |
| Unmapped IPA | Guest access "causes `hv_vcpu_run` to exit". MMIO is emulated on exit. | AD:(root) Virtual Resource Mapping |
| IPA size | Default and max are runtime queries. The config setter must not exceed the max. Host sysctls give 42 bits (16K granule) and 40 bits (4K granule) on this M5 Max. | `hv_vm_config.h:27-65`; HOST |
| IPA granule | `hv_ipa_granule_t` {`HV_IPA_GRANULE_4KB`=0, `HV_IPA_GRANULE_16KB`=1}, macOS 26.0. | `hv_vm_config.h:110-143`; AD:hv_ipa_granule_t |
| Dirty tracking | No dirty-log or dirty-ring API exists. The only primitive is `hv_vm_protect` (drop `HV_MEMORY_WRITE`) plus handling the resulting data-abort exits (derived from the inventory in §1.2). | `hv_vm.h:64-72` |

### 1.5 vCPU exit record and core registers

**`hv_vcpu_exit_t`** — `hv_vcpu_types.h:61-86`

| Field | Type | Meaning |
|---|---|---|
| `reason` | `hv_exit_reason_t` (uint32 enum) | See below. |
| `exception.syndrome` | `hv_exception_syndrome_t` (u64) | "Corresponds to ESR_ELx" (`:61-64`). Decode per §2.1. |
| `exception.virtual_address` | `hv_exception_address_t` (u64) | "Corresponds to FAR_ELx" (`:66-69`). |
| `exception.physical_address` | `hv_ipa_t` (u64) | Faulting IPA, for stage-2 aborts. |

C layout (derived): `reason` @0 (4 B + 4 B padding), `syndrome` @8, `virtual_address` @16, `physical_address` @24, `sizeof` = 32.

**`hv_exit_reason_t`** — `hv_vcpu_types.h:35-59`

| Value | Name | Meaning |
|---|---|---|
| 0 | `HV_EXIT_REASON_CANCELED` | Asynchronous exit requested by `hv_vcpus_exit()` |
| 1 | `HV_EXIT_REASON_EXCEPTION` | "synchronous exception to a higher EL triggered by the guest". Decode `exception.syndrome`. |
| 2 | `HV_EXIT_REASON_VTIMER_ACTIVATED` | vtimer became pending since the last run returned. Auto-sets the vtimer mask (§1.7). |
| 3 | `HV_EXIT_REASON_UNKNOWN` | "should not happen under normal operation" |

**`hv_reg_t`** — `hv_vcpu_types.h:93-137` (uint32 enum; values derived from C enum rules)

| Value | Names |
|---|---|
| 0..28 | `HV_REG_X0`..`HV_REG_X28` |
| 29 | `HV_REG_X29` = `HV_REG_FP` |
| 30 | `HV_REG_X30` = `HV_REG_LR` |
| **31** | **`HV_REG_PC`** |
| 32 | `HV_REG_FPCR` |
| 33 | `HV_REG_FPSR` |
| 34 | `HV_REG_CPSR` (PSTATE/SPSR format) |

**Bug magnet:** ESR `SRT`/`Rt` = 31 means XZR/WZR in MMIO and sysreg traps. Linux KVM treats 31 as "reads 0, writes discarded" (`arch/arm64/include/asm/kvm_emulate.h:163-176`). Computing `HV_REG_X0 + rt` with rt=31 addresses **PC**. SP is not an `hv_reg_t`. It lives in `HV_SYS_REG_SP_EL0` and `HV_SYS_REG_SP_EL1`.

Other enums:

| Enum | Values | Source |
|---|---|---|
| `hv_simd_fp_reg_t` | `HV_SIMD_FP_REG_Q0..Q31` = 0..31. Value type is `uint8_t` ext_vector(16). | `hv_vcpu_types.h:88-175` |
| `hv_sme_z_reg_t` / `hv_sme_p_reg_t` | Z0..Z31 = 0..31, P0..P15 = 0..15 (15.2) | `hv_vcpu_types.h:188-246` |
| `hv_sme_zt0_uchar64_t` | 64-byte vector (15.2) | `hv_vcpu_types.h:248-252` |
| `hv_interrupt_type_t` | `HV_INTERRUPT_TYPE_IRQ`=0, `HV_INTERRUPT_TYPE_FIQ`=1. The comment names `hv_vcpu_{get,set}_interrupt_level`, but the real functions are `hv_vcpu_{get,set}_pending_interrupt`. | `hv_vcpu_types.h:422-433` |
| `hv_cache_type_t` | `HV_CACHE_TYPE_DATA`=0, `HV_CACHE_TYPE_INSTRUCTION`=1 | `hv_vcpu_types.h:435-444` |
| `hv_feature_reg_t` | 0 `ID_AA64DFR0_EL1`, 1 `DFR1`, 2 `ISAR0`, 3 `ISAR1`, 4 `MMFR0`, 5 `MMFR1`, 6 `MMFR2`, 7 `PFR0`, 8 `PFR1`, 9 `CTR_EL0`, 10 `CLIDR_EL1`, 11 `DCZID_EL0`, 12 `ID_AA64SMFR0_EL1` (15.2), 13 `ID_AA64ZFR0_EL1` (15.2) | `hv_vcpu_config.h:27-45` |
| `hv_vcpu_t` | `uint64_t` | `hv_vcpu_types.h:30-33` |
| `hv_ipa_t` | `uint64_t` | `hv_vm_types.h:29-32` |

Initial vCPU register state after `hv_vcpu_create` is not documented (**UNVERIFIED**), so the VMM must set CPSR explicitly. Linux-compatible EL1 entry is `PSR_MODE_EL1h|PSR_A_BIT|PSR_I_BIT|PSR_F_BIT|PSR_D_BIT` = 0x5|0x100|0x80|0x40|0x200 = **0x3C5**. With EL2 enabled, use EL2h (0x9), giving **0x3C9**. Constants: `arch/arm64/include/uapi/asm/ptrace.h:33-48`. KVM uses the same reset values: `arch/arm64/kvm/reset.c:40-44`.

### 1.6 `hv_sys_reg_t` — every accessible system register (`hv_vcpu_types.h:254-420`)

Encoding (derived, and checked against every entry): `value = op0<<14 | op1<<11 | CRn<<7 | CRm<<3 | op2`. The tuples below match Linux, for example `SYS_CNTV_CTL_EL0 sys_reg(3,3,14,3,1)` (`arch/arm64/include/asm/sysreg.h:475`), `SYS_CNTVOFF_EL2 sys_reg(3,4,14,0,3)` (`:594`), `SYS_SP_EL2 sys_reg(3,6,4,1,0)` (`:626`), and `SCTLR_EL1 3 0 1 0 0` (`arch/arm64/tools/sysreg:2438`). This is **not** the ESR ISS layout (§2.1), so the VMM must convert (Op0 ISS[21:20], Op2 [19:17], Op1 [16:14], CRn [13:10], CRm [4:1]).

| Register(s) | Value(s) | (op0,op1,CRn,CRm,op2) | macOS | Condition / note |
|---|---|---|---|---|
| `DBGBVR<n>_EL1`, `DBGBCR<n>_EL1`, `DBGWVR<n>_EL1`, `DBGWCR<n>_EL1`, n=0..15 | 0x8004+8n, 0x8005+8n, 0x8006+8n, 0x8007+8n | (2,0,0,n,4..7) | 11.0 | lines 258-323 |
| `MDCCINT_EL1` | 0x8010 | (2,0,0,2,0) | 11.0 | |
| `MDSCR_EL1` | 0x8012 | (2,0,0,2,2) | 11.0 | |
| `MIDR_EL1` | 0xc000 | (3,0,0,0,0) | 11.0 | |
| `MPIDR_EL1` | 0xc005 | (3,0,0,0,5) | 11.0 | Must hold the affinity used by the GIC (`hv_gic.h:40-41`) |
| `ID_AA64PFR0_EL1` / `PFR1` | 0xc020 / 0xc021 | (3,0,0,4,0/1) | 11.0 | |
| `ID_AA64ZFR0_EL1` / `SMFR0` | 0xc024 / 0xc025 | (3,0,0,4,4/5) | 15.2 | |
| `ID_AA64DFR0_EL1` / `DFR1` | 0xc028 / 0xc029 | (3,0,0,5,0/1) | 11.0 | DFR0 writable (`hv_vm_config.h:104`) |
| `ID_AA64ISAR0_EL1` / `ISAR1` | 0xc030 / 0xc031 | (3,0,0,6,0/1) | 11.0 | |
| `ID_AA64MMFR0_EL1` / `MMFR1` / `MMFR2` | 0xc038 / 0xc039 / 0xc03a | (3,0,0,7,0/1/2) | 11.0 | |
| `SCTLR_EL1` | 0xc080 | (3,0,1,0,0) | 11.0 | |
| `ACTLR_EL1` | 0xc081 | (3,0,1,0,1) | 15.0 | Only bit 1 `EnTSO` is gettable/settable. 1 = TSO memory model (`:339-346`). |
| `CPACR_EL1` | 0xc082 | (3,0,1,0,2) | 11.0 | |
| `SMPRI_EL1` / `SMCR_EL1` | 0xc094 / 0xc096 | (3,0,1,2,4/6) | 15.2 | |
| `TTBR0_EL1` / `TTBR1_EL1` / `TCR_EL1` | 0xc100 / 0xc101 / 0xc102 | (3,0,2,0,0/1/2) | 11.0 | |
| `APIAKEY{LO,HI}_EL1`, `APIBKEY…`, `APDAKEY…`, `APDBKEY…`, `APGAKEY…` | 0xc108-0xc10b, 0xc110-0xc113, 0xc118-0xc119 | (3,0,2,1..3,x) | 11.0 | Pointer-auth keys |
| `SPSR_EL1` / `ELR_EL1` | 0xc200 / 0xc201 | (3,0,4,0,0/1) | 11.0 | |
| `SP_EL0` | 0xc208 | (3,0,4,1,0) | 11.0 | |
| `AFSR0_EL1` / `AFSR1_EL1` / `ESR_EL1` | 0xc288 / 0xc289 / 0xc290 | | 11.0 | |
| `FAR_EL1` / `PAR_EL1` | 0xc300 / 0xc3a0 | (3,0,6,0,0) / (3,0,7,4,0) | 11.0 | |
| `MAIR_EL1` / `AMAIR_EL1` / `VBAR_EL1` | 0xc510 / 0xc518 / 0xc600 | | 11.0 | |
| `CONTEXTIDR_EL1` / `TPIDR_EL1` | 0xc681 / 0xc684 | (3,0,13,0,1/4) | 11.0 | |
| `SCXTNUM_EL1` | 0xc687 | (3,0,13,0,7) | 15.2 | |
| `CNTKCTL_EL1` | 0xc708 | (3,0,14,1,0) | 11.0 | |
| `CSSELR_EL1` | 0xd000 | (3,2,0,0,0) | 11.0 | |
| `TPIDR_EL0` / `TPIDRRO_EL0` | 0xde82 / 0xde83 | (3,3,13,0,2/3) | 11.0 | |
| `TPIDR2_EL0` / `SCXTNUM_EL0` | 0xde85 / 0xde87 | (3,3,13,0,5/7) | 15.2 | |
| `CNTV_CTL_EL0` / `CNTV_CVAL_EL0` | 0xdf19 / 0xdf1a | (3,3,14,3,1/2) | 11.0 | Virtual timer |
| `SP_EL1` | 0xe208 | (3,4,4,1,0) | 11.0 | |
| `CNTP_CTL_EL0` / `CNTP_CVAL_EL0` / `CNTP_TVAL_EL0` | 0xdf11 / 0xdf12 / 0xdf10 | (3,3,14,2,1/2/0) | 15.0 | "only available if the VM was created with a GIC device" (`:387-391`) |
| `CNTHCTL_EL2`, `CNTHP_CTL_EL2`, `CNTHP_CVAL_EL2`, `CNTHP_TVAL_EL2`, `CNTVOFF_EL2` | 0xe708, 0xe711, 0xe712, 0xe710, 0xe703 | (3,4,14,…) | 15.0 | "only available if EL2 was enabled" (`:393-399`) |
| `CPTR_EL2`, `ELR_EL2`, `ESR_EL2`, `FAR_EL2`, `HCR_EL2`, `HPFAR_EL2`, `MAIR_EL2`, `MDCR_EL2`, `SCTLR_EL2`, `SPSR_EL2`, `SP_EL2`, `TCR_EL2`, `TPIDR_EL2`, `TTBR0_EL2`, `TTBR1_EL2`, `VBAR_EL2`, `VMPIDR_EL2`, `VPIDR_EL2`, `VTCR_EL2`, `VTTBR_EL2` | 0xe08a, 0xe201, 0xe290, 0xe300, 0xe088, 0xe304, 0xe510, 0xe089, 0xe080, 0xe200, 0xf208, 0xe102, 0xe682, 0xe100, 0xe101, 0xe600, 0xe005, 0xe000, 0xe10a, 0xe108 | | 15.0 | EL2-enabled VMs only (`:400-419`) |

Absent from `hv_sys_reg_t`: `CNTFRQ_EL0`, `CNTVCT_EL0` (use the offset), all PMU registers, `OSLAR/OSLSR/OSDLR_EL1`, `DISR_EL1`/`VSESR_EL2`, `ZCR_EL1` (no non-streaming SVE), MTE (`TFSR*`, `GCR_EL1`, `RGSR_EL1`), `CNTHV_*`, and AArch32 banked registers. Derived from the full enum at `hv_vcpu_types.h:257-420`.

### 1.7 Virtual timer (vtimer) protocol

| Item | Detail | Source |
|---|---|---|
| Exit | `HV_EXIT_REASON_VTIMER_ACTIVATED`: the vtimer became pending since the last run. The VMM must make the vtimer interrupt pending in the guest's interrupt controller. | `hv_vcpu_types.h:43-53` |
| Auto-mask | After this exit the timer is masked automatically. No further vtimer exits occur until the mask is cleared, "even when hv_vcpu_run() is called with the VTimer interrupt in a pending state". | `hv_vcpu.h:403-426`; AD:hv_vcpu_set_vtimer_mask(_:_:) |
| Unmask point | Call `hv_vcpu_set_vtimer_mask(false)` when the guest deactivates (EOIs) the INTID matching the vtimer. "should be called during a trap of the EOI for the guest's VTimer interrupt handler". | `hv_vcpu.h:417-423`; `hv_vcpu_types.h:48-51` |
| Mask semantics | "When the mask is set, the vCPU does not exit if the VTimer times out." | `hv_vcpu.h:408-409` |
| Offset | `CNTVCT_EL0 = mach_absolute_time() - vtimer_offset`. AD: "corresponds to … `CNTVOFF_EL2`". | `hv_vcpu.h:438-449`; AD:hv_vcpu_set_vtimer_offset(_:_:) |
| Counter rate | `mach_absolute_time` ticks at `hw.tbfrequency` = 24 MHz on this host. The guest-visible `CNTFRQ_EL0` value isn't settable or gettable through HVF (**UNVERIFIED** that it reads 24 MHz). | HOST; §1.6 |
| Snapshot rule (derived) | At save: `cntvct = mach_absolute_time() - offset`. Store `CNTV_CTL_EL0`, `CNTV_CVAL_EL0`, and the mask. At restore: set `offset' = mach_absolute_time() - cntvct` on **every** vCPU (per-vCPU API, keep them identical). | derived from `hv_vcpu.h:444-446` |
| With `hv_gic` | `hv_gic_get_intid(HV_GIC_INT_EL1_VIRTUAL_TIMER)` = 27 is "reserved by the hypervisor framework". The header doesn't say whether VTIMER_ACTIVATED exits and WFI exits still occur once an `hv_gic` exists (**UNVERIFIED**). macOS 27 docs define WFI wait-time accounting only for GIC VMs, which implies in-framework WFI handling with a GIC. | `hv_gic_types.h:37-50`; `hv_gic_parameters.h:89-99`; AD:hv_vcpu_get_wait_for_interrupt_time(_:_:) |

### 1.8 In-framework GICv3 (`hv_gic_*`, macOS 15.0+)

| Item | Detail | Source |
|---|---|---|
| Model | GICv3 with distributor, redistributors, MSI frame, and ICC system registers. ICH/ICV registers and nested injection are available when EL2 is enabled. One instance per VM. | `hv_gic.h:22-47` |
| Creation sequence | `hv_vm_create` → `hv_gic_config_create` → `set_distributor_base` / `set_redistributor_base` / (optional) `set_msi_region_base` + `set_msi_interrupt_range` → `hv_gic_create` → create vCPUs → set each vCPU's `MPIDR_EL1` → `hv_gic_get_redistributor_base(vcpu)` | `hv_gic_config.h:19-86`; `hv_gic.h:35-43,105-114` |
| Sizes and alignments | Runtime queries: `hv_gic_get_{distributor_size, distributor_base_alignment, redistributor_region_size, redistributor_size, redistributor_base_alignment, msi_region_size, msi_region_base_alignment}`. Redistributor = 2 × 64 KiB frames per vCPU (RD_base + SGI_base), contiguous, so the GICv4 VLPI frames are absent. | `hv_gic_parameters.h:20-78` |
| Redistributor region extent | Covers "all vCPUs supported by the virtual machine". Linux walks redistributors until `GICR_TYPER.Last` (bit 4) and ignores the DT region size (`drivers/irqchip/irq-gic-v3.c:984-1021`). The per-redistributor property callback returns 1, so Linux walks **all** of them (`drivers/irqchip/irq-gic-v3.c:1070-1119`). `GICR_TYPER_LAST` = 1<<4 (`include/linux/irqchip/arm-gic-v3.h:246`). The DT `reg` must therefore reach the redistributor that has Last=1. Which redistributor HVF marks Last is **UNVERIFIED**; read `GICR_TYPER` via `hv_gic_get_redistributor_reg` to check. | `hv_gic_config.h:47-51` |
| SPI range | `hv_gic_get_spi_interrupt_range(&base, &count)` | `hv_gic_parameters.h:80-87` |
| Reserved INTIDs (`hv_gic_intid_t`, u16) | `HV_GIC_INT_PERFORMANCE_MONITOR`=23 (PPI 7), `HV_GIC_INT_MAINTENANCE`=25 (PPI 9, EL2 only), `HV_GIC_INT_EL2_PHYSICAL_TIMER`=26 (PPI 10, EL2 only), `HV_GIC_INT_EL1_VIRTUAL_TIMER`=27 (PPI 11), `HV_GIC_INT_EL1_PHYSICAL_TIMER`=30 (PPI 14). PPI = INTID − 16 (derived). | `hv_gic_types.h:37-50` |
| SPI injection | `hv_gic_set_spi(intid, level)`: level semantics per ICFGR. Edge interrupts ignore `false`. No owning-thread rule, so device threads can call it (no thread note in the header). | `hv_gic.h:51-66` |
| MSI | Needs both MSI base and range, and the range must be inside the SPI range. `hv_gic_send_msi(GPA of GICM_SET_SPI_NSR, intid)`. MSI frame registers: `GICM_TYPER`=0x0008, `GICM_SET_SPI_NSR`=0x0040. | `hv_gic.h:45-46,68-77`; `hv_gic_config.h:56-86`; `hv_gic_types.h:1698-1704` |
| No ITS/LPI | The redistributor enum lacks `GICR_CTLR`, `GICR_WAKER`, `GICR_PROPBASER`, `GICR_PENDBASER`, and no GITS enum exists. Inference: no ITS/LPIs, so MSIs are SPI-based. | `hv_gic_types.h:1604-1630` |
| DT exposure of the MSI frame (inference) | The `GICM_*` offsets equal GICv2m's `V2M_MSI_TYPER` 0x008 and `V2M_MSI_SETSPI_NS` 0x040. TYPER base SPI = bits [25:16] and count = bits [9:0] (`drivers/irqchip/irq-gic-v2m.c:38-50`). When `GICD_TYPER.LPIS`=0, Linux's GICv3 driver calls `gicv2m_init()` (`drivers/irqchip/irq-gic-v3.c:2058-2064`). That scans for `compatible = "arm,gic-v2m-frame"` nodes with `msi-controller` (`drivers/irqchip/irq-gic-v2m.c:384-410`). The binding lives in `Documentation/devicetree/bindings/interrupt-controller/arm,gic.yaml:150-183`, a GICv2 schema, so a v2m child under an `arm,gic-v3` node is outside schema but accepted by the driver code. Whether HVF's frame fully behaves as v2m (e.g. `MSI_IIDR` 0xFCC) is **UNVERIFIED**. | cited inline |
| Distributor register enum | `GICD_CTLR` 0x0000, `GICD_TYPER` 0x0004, `IGROUPR0-31` 0x0080-0x00fc, `ISENABLER0-31` 0x0100-0x017c, `ICENABLER0-31` 0x0180-0x01fc, `ISPENDR0-31` 0x0200-0x027c, `ICPENDR0-31` 0x0280-0x02fc, `ISACTIVER0-31` 0x0300-0x037c, `ICACTIVER0-31` 0x0380-0x03fc, `IPRIORITYR0-254` 0x0400-0x07f8, `ICFGR0-63` 0x0c00-0x0cfc, `IROUTER32-1019` 0x6100-0x7fd8, `GICD_PIDR2` 0xffe8. No `IIDR`, `TYPER2`, `IGRPMODR`, `NSACR`, `STATUSR`, or `SETSPI_*`. | `hv_gic_types.h:52-1602` |
| Redistributor register enum (u32) | `GICR_TYPER` 0x0008, `GICR_PIDR2` 0xffe8. SGI_base frame: `IGROUPR0` 0x10080, `ISENABLER0` 0x10100, `ICENABLER0` 0x10180, `ISPENDR0` 0x10200, `ICPENDR0` 0x10280, `ISACTIVER0` 0x10300, `ICACTIVER0` 0x10380, `IPRIORITYR0-7` 0x10400-0x1041c, `ICFGR0-1` 0x10c00-0x10c04. | `hv_gic_types.h:1604-1630` |
| ICC enum (same encoding as §1.6) | `PMR_EL1` 0xc230, `BPR0_EL1` 0xc643, `AP0R0_EL1` 0xc644, `AP1R0_EL1` 0xc648, `RPR_EL1` 0xc65b, `BPR1_EL1` 0xc663, `CTLR_EL1` 0xc664, `SRE_EL1` 0xc665, `IGRPEN0_EL1` 0xc666, `IGRPEN1_EL1` 0xc667, `SRE_EL2` 0xe64d | `hv_gic_types.h:1632-1648` |
| ICH enum (EL2 only) | `AP0R0_EL2` 0xe640, `AP1R0_EL2` 0xe648, `HCR_EL2` 0xe658, `VTR_EL2` 0xe659, `MISR_EL2` 0xe65a, `EISR_EL2` 0xe65b, `ELRSR_EL2` 0xe65d, `VMCR_EL2` 0xe65f, `LR0..15_EL2` 0xe660-0xe66f | `hv_gic_types.h:1650-1679` |
| ICV enum (EL2 only) | Same encodings as ICC `PMR`..`IGRPEN1` (0xc230..0xc667) | `hv_gic_types.h:1681-1696` |
| Only AP0R0/AP1R0 exposed | Inference: guest-visible priority bits ≤ 5 (APR1..3 unused). **UNVERIFIED**. | `hv_gic_types.h:1639-1640` |
| Snapshot | Save: stop all vCPUs, then `hv_gic_state_create` → `get_size` → `get_data` (opaque, versioned, excludes CPU-interface registers). Plus per-vCPU `hv_gic_get_icc_reg` for all ICC registers (and ICH/ICV if EL2). Restore: create VM, GIC, and vCPUs, set MPIDRs, `hv_gic_set_state(blob)`, set ICC registers, all before the first run. The blob can be rejected after a macOS update, so keep a register-level fallback via the dist/redist register APIs. | `hv_gic_state.h:18-57`; `hv_gic.h:250-263` |

### 1.9 Snapshot / restore: what HVF lets you get and set

| State | Get | Set | Notes / source |
|---|---|---|---|
| X0-X30, PC, FPCR, FPSR, CPSR | yes | yes | `hv_vcpu_{get,set}_reg` (`hv_vcpu.h:41-63`) |
| SP_EL0, SP_EL1, ELR_EL1, SPSR_EL1 | yes | yes | sysregs (§1.6) |
| V0-V31 | yes | yes | `hv_vcpu.h:65-94` |
| EL1 MMU/exception context (SCTLR, TCR, TTBR0/1, MAIR, AMAIR, VBAR, CONTEXTIDR, TPIDR*, ESR, FAR, PAR, AFSR0/1, CPACR, CNTKCTL, CSSELR) | yes | yes | §1.6 |
| Pointer-auth keys | yes | yes | §1.6 |
| Debug registers + MDSCR, MDCCINT | yes | yes | §1.6 |
| ID registers (MIDR, MPIDR, ID_AA64*) | yes | set via `hv_vcpu_set_sys_reg`. Which writes are honored beyond `ID_AA64DFR0_EL1` is **UNVERIFIED**. | `hv_vm_config.h:104` |
| vtimer (CNTV_CTL/CVAL, offset, mask) | yes | yes | §1.7 |
| Physical timer CNTP_* | yes (GIC VMs) | yes (GIC VMs) | `hv_vcpu_types.h:387-391` |
| Pending IRQ/FIQ line | yes | yes, but auto-cleared on each run | `hv_vcpu.h:290-312` |
| Pending SError | macOS 27 only | macOS 27 only | AD:hv_vcpu_get_serror(_:_:) |
| SME (SM/ZA, Z, P, ZA, ZT0, SMCR, SMPRI, TPIDR2) | yes (15.2) | yes (15.2) | `hv_vcpu.h:96-264` |
| GIC distributor/redistributor | opaque blob and per-register | blob and per-register | §1.8 |
| GIC CPU interface (ICC) | per-register | per-register | §1.8 |
| EL2 context (nested) | EL2-enabled only | EL2-enabled only | `hv_vcpu_types.h:393-419` |
| PMU state | no API | no API | not in `hv_sys_reg_t` |
| Guest memory contents | VMM owns the host mapping | VMM owns the host mapping | `hv_vm_map` maps VMM memory (§1.4) |
| Dirty-page log | none | — | §1.4 |
| TLB maintenance on restore | macOS 27 `hv_vcpu_invalidate_tlb` | — | AD:hv_vcpu_invalidate_tlb(_:_:_:) |

### Section 1 bug-magnets

- **`HV_REG_X0 + rt` with rt = 31 writes PC.** Rt = 31 means XZR/WZR (discard or read as 0) (§1.5).
- **Thread affinity.** Every per-vCPU call must run on the vCPU's creating thread, except `hv_vcpus_exit`. `hv_vcpu_set_pending_interrupt` is cleared after every `hv_vcpu_run`, so it must be re-asserted before each run while the line is high (`hv_vcpu.h:301-312`).
- **GIC ordering.** `hv_gic_create` must come after `hv_vm_create` and **before** any `hv_vcpu_create`. `MPIDR_EL1` must be set before `hv_gic_get_redistributor_base`. The GIC state blob excludes ICC registers and can be rejected after a macOS update (§1.8).
- **Vtimer mask protocol.** `VTIMER_ACTIVATED` auto-masks the vtimer. Forget to unmask on guest EOI and the guest's timer interrupts stop. Unmask while the condition is still asserted and every run exits immediately again (inference from `hv_vcpu.h:403-426`).
- **Vtimer offset.** It is per vCPU and relative to `mach_absolute_time` (24 MHz on this host). booting.rst requires CNTVOFF to be identical on all CPUs (§3.2), so write the same offset on every vCPU.
- **Mapping granularity.** `hv_vm_map`/`unmap`/`protect` work in host pages (16 KiB here). A 4 KiB-aligned RAM or MMIO split that works on a 4K-page KVM host fails on HVF (§1.4).
- **API availability.** The macOS 27 APIs (SError, WFI time, TLBI, extra ID registers) are absent on the macOS 26.4.1 dev host and SDK. Gate them behind runtime symbol checks (derived).

## 2. Arm architecture ground truth

Arm documents retrieved and read for this section:

| Document | Issue |
|---|---|
| Arm ARM DDI0487 | M.c. Read as per-topic HTML/JSON from documentation-service.arm.com, so citations are by section and rule ID with no PDF page. |
| PSCI DEN0022 | F.b (PSCI 1.3) |
| SMCCC DEN0028 | v1.6 G (EAC1) |
| GIC IHI0069 | H.b |
| PL011 TRM DDI0183 | G (r1p5) |
| PL031 TRM DDI0224 | C (r1p3) |

Linux paths are relative to the 7.2-rc4 tree.

### 2.1 ESR_ELx (as delivered to a VMM in `hv_vcpu_exit_t.exception.syndrome`, or `ESR_EL2` under KVM)

| Field | Bits | Meaning | Source |
|---|---|---|---|
| RES0 | [63:56] | reserved | DDI0487 M.c D24.2.45 |
| ISS2 | [55:32] | Extra syndrome. For data aborts: HDBSSF[11], TnD[10], TagAccess[9], GCS[8], AssuredOnly[7], Overlay[6], DirtyBit[5], Xs[4:0] (FEAT_LS64*, THE, S1POE/S2POE, S1PIE/S2PIE, GCS, MTE, HDBSS) | D24.2.45; arch/arm64/include/asm/esr.h:82-84,163-176 |
| EC | [31:26] | exception class | D24.2.45; esr.h:73-76 |
| IL | [25] | 1 = 32-bit instruction. **Also 1** for SError, instruction abort, PC/SP alignment, **data abort with ISV=0**, illegal state and EC=0x00 | D24.2.45; esr.h:78-79 |
| ISS | [24:0] | class-specific | D24.2.45; esr.h:80 |

| EC | Linux macro | Meaning (DDI0487 M.c D24.2.45) | Source |
|---|---|---|---|
| 0x00 | `ESR_ELx_EC_UNKNOWN` | Unknown reason | esr.h:13 |
| 0x01 | `ESR_ELx_EC_WFx` | Trapped WF* (a conditional WF* that fails its condition doesn't trap) | esr.h:14 |
| 0x07 | `ESR_ELx_EC_FP_ASIMD` | SME/SVE/AdvSIMD/FP access trapped by CPACR_EL1.FPEN, CPTR_EL2.FPEN/TFP, CPTR_EL3.TFP | esr.h:20 |
| 0x12 / 0x13 | `HVC32` / `SMC32` | AArch32 HVC; AArch32 SMC (to EL2 only if HCR_EL2.TSC=1) | esr.h:30-31 |
| 0x16 | `ESR_ELx_EC_HVC64` | HVC executed in AArch64 "when HVC is not disabled" | esr.h:34 |
| 0x17 | `ESR_ELx_EC_SMC64` | SMC in AArch64; reported in ESR_EL2 **only when HCR_EL2.TSC=1** | esr.h:35 |
| 0x18 | `ESR_ELx_EC_SYS64` | Trapped MSR/MRS/System instruction (AArch64) | esr.h:36 |
| 0x19 | `ESR_ELx_EC_SVE` | SVE access trapped (CPACR_EL1.ZEN, CPTR_EL2.ZEN/TZ, CPTR_EL3.EZ) | esr.h:37 |
| 0x1D | `ESR_ELx_EC_SME` | SME trap. ISS SMTC[2:0]: 0 SME disabled, 1 illegal, 2 SM disabled, 3 ZA disabled, 4 ZT disabled | esr.h:41,394-401 |
| 0x20 / 0x21 | `IABT_LOW` / `IABT_CUR` | Instruction abort, lower / same EL | esr.h:44-45 |
| 0x22 / 0x26 | `PC_ALIGN` / `SP_ALIGN` | PC / SP alignment fault | esr.h:46,50 |
| 0x24 / 0x25 | `DABT_LOW` / `DABT_CUR` | Data abort, lower / same EL (0x25 also covers NV2 VNCR aborts). **Guest MMIO exits are 0x24.** | esr.h:48-49 |
| 0x2F | `ESR_ELx_EC_SERROR` | SError | esr.h:57 |
| 0x3C | `ESR_ELx_EC_BRK64` | BRK (ISS[15:0] = comment) | esr.h:69,211 |

**Preferred return address and PC-advance rules** (DDI0487 M.c D1.4.1.5):

| Rule | Text (condensed) | VMM consequence |
|---|---|---|
| QYCWH | "For synchronous exceptions other than exception generating instructions, the preferred exception return address is the address of the instruction that generates the exception." | After emulating a data abort (MMIO), WFx, or MSR/MRS trap: **PC += 4**. KVM: arch/arm64/kvm/mmio.c:149; arch/arm64/kvm/handle_exit.c:175; arch/arm64/kvm/sys_regs.c:4754-4778. |
| DKWPP | "For an exception generating instruction that is executed, the preferred exception return address is the address of the instruction that follows…" | HVC: **do not** advance PC. |
| LBLBR | "For an exception generating instruction that is trapped, disabled, or is UNDEFINED …, the preferred exception return address is the address of the exception generating instruction." | SMC trapped by HCR_EL2.TSC: **PC += 4**. KVM: arch/arm64/kvm/handle_exit.c:57-76. |
| (D1.4.9) | SMC is UNDEFINED when EL3 is not implemented and HCR_EL2.TSC doesn't trap it. | Prefer `method = "hvc"`. KVM returns X0 = ~0 for an SMC with a nonzero immediate (handle_exit.c:82-85). |

**Data-abort ISS (EC 0x24/0x25)**

| Field | Bits | Semantics | Linux macro | Source |
|---|---|---|---|---|
| ISV | [24] | 1 = ISS[23:14] holds a valid syndrome {SAS, SSE, SRT, SF, AR}. Only for a single-register load/store **without writeback**, not exclusive, on a stage-2 fault not on a stage-1 walk (plus LD64B/ST64B*). ISV=0 for MOPS, MTE tags, GCS, NV2 and **LSE atomics**. **ISV=0: the VMM must decode the instruction or fail.** | `ESR_ELx_ISV` | D24.2.45; esr.h:147-148 |
| SAS | [23:22] | 00 byte, 01 halfword, 10 word, 11 doubleword (size = 1<<SAS) | `ESR_ELx_SAS` | D24.2.45; esr.h:149-150; arch/arm64/include/asm/kvm_emulate.h:409-412 |
| SSE | [21] | sign-extend the loaded byte/halfword/word (with ISV=0 and FEAT_THE this bit is TopLevel) | `ESR_ELx_SSE` | D24.2.45; esr.h:151-152 |
| SRT | [20:16] | Rt number. **31 = XZR/WZR, not SP** (with ISV=0 and FEAT_RASv2 this field is WU) | `ESR_ELx_SRT_MASK` | D24.2.45; esr.h:153-154 |
| SF | [15] | 1 = 64-bit register (the instruction's register width, not the execution state) | `ESR_ELx_SF` | D24.2.45; esr.h:155-156 |
| AR | [14] | acquire/release (with ISV=0 and FEAT_PFAR: PFV) | `ESR_ELx_AR` | D24.2.45; esr.h:157-158 |
| VNCR | [13] | fault came from VNCR_EL2 use (FEAT_NV2) | `ESR_ELx_VNCR` | D24.2.45; esr.h:103-104 |
| LST / SET | [12:11] | LST: LD64B/ST64B type for translation, access-flag and permission faults. SET: sync-error type (RAS, DFSC=0x10). | `ESR_ELx_SET_MASK` | D24.2.45; esr.h:105-106 |
| FnV | [10] | FAR not valid (only meaningful when DFSC=0x10) | `ESR_ELx_FnV` | D24.2.45; esr.h:107-108 |
| EA | [9] | IMPLEMENTATION DEFINED external-abort type | `ESR_ELx_EA` | D24.2.45; esr.h:109-110 |
| CM | [8] | Cache-maintenance or address-translation instruction. **WnR is always 1 when CM=1**, so never treat it as an MMIO write. | `ESR_ELx_CM` | D24.2.45; esr.h:159-160 |
| S1PTW | [7] | Stage-2 fault on the guest's stage-1 table walk (page tables in unbacked IPA). Not MMIO. | `ESR_ELx_S1PTW` | D24.2.45; esr.h:111-112 |
| WnR | [6] | 1 = write | `ESR_ELx_WNR` | D24.2.45; esr.h:87-88 |
| DFSC | [5:0] | fault status (below) | `ESR_ELx_FSC` | D24.2.45; esr.h:115 |

**DFSC codes** (DDI0487 M.c D24.2.45; names agree with arch/arm64/mm/fault.c:913-976, where table index = FSC)

| DFSC | Meaning |
|---|---|
| 0x00-0x03 | Address size fault, L0-L3 |
| 0x04-0x07 | **Translation fault, L0-L3** (the usual exit for MMIO or unbacked RAM) |
| 0x08-0x0B | Access flag fault, L0-L3 |
| 0x0C-0x0F | Permission fault, L0-L3 (L0 needs FEAT_LPA2) |
| 0x10 | Synchronous external abort, not on a table walk |
| 0x11 | Synchronous tag check fault |
| 0x12 / 0x13 / 0x14-0x17 | External abort on table walk, level -2 / -1 / 0-3 |
| 0x18 | Synchronous parity/ECC error (without FEAT_RAS) |
| 0x1B-0x1F | Parity/ECC error on table walk, level -1..3 |
| 0x21 | Alignment fault |
| 0x22-0x28 | Granule protection faults (RME) |
| 0x29 | Address size fault, level -1. **Conflict:** esr.h:134 `ESR_ELx_FSC_ADDRSZ_nL(-1)` yields 0x25, while DDI0487 M.c and fault.c:954 (index 41 = 0x29) say 0x29. Follow the spec. |
| 0x2A / 0x2B | Translation fault, level -2 / -1 |
| 0x2C | Address size fault, level -2 |
| 0x30 | TLB conflict abort |
| 0x31 | Unsupported atomic hardware update |
| 0x34 / 0x35 | IMPLEMENTATION DEFINED (lockdown / unsupported exclusive or atomic) |

**MMIO load completion** (KVM as the reference): sign-extend if SSE is set and size < 8; mask to 32 bits if SF=0; discard writes to Rt=31. See arch/arm64/kvm/mmio.c:133-142 and arch/arm64/include/asm/kvm_emulate.h:163-178.

Linux MMIO accessors are single LDR/STR/LDRH/STRB with no writeback, so they always produce ISV=1. They use the `"rZ"` constraint, so **`writel(0, …)` is a store from WZR (SRT=31)**. Reads may be LDAR* (AR=1). See arch/arm64/include/asm/io.h:26-51,65-95.

When ISV=0, KVM returns -ENOSYS, or `KVM_EXIT_ARM_NISV` if that exit is enabled (mmio.c:174-189).

**MSR/MRS/System-instruction trap ISS (EC 0x18)**

| Field | Bits | Linux macro | Source |
|---|---|---|---|
| RES0 | [24:22] | `ESR_ELx_SYS64_ISS_RES0_MASK` | D24.2.45; esr.h:214-215 |
| Op0 | [21:20] | `…_OP0_SHIFT` 20 | D24.2.45; esr.h:230-231 |
| Op2 | [19:17] | `…_OP2_SHIFT` 17 | esr.h:228-229 |
| Op1 | [16:14] | `…_OP1_SHIFT` 14 | esr.h:226-227 |
| CRn | [13:10] | `…_CRN_SHIFT` 10 | esr.h:224-225 |
| Rt | [9:5] | `…_RT_SHIFT` 5 (31 = XZR) | esr.h:220-221 |
| CRm | [4:1] | `…_CRM_SHIFT` 1 | esr.h:222-223 |
| Direction | [0] | 0 = write (MSR), 1 = read (MRS); `DIR_READ` = 0x1 | D24.2.45; esr.h:216-218 |

This ISS order (Op0, Op2, Op1, CRn, Rt, CRm, Dir) differs from the kernel-internal `sys_reg(op0,op1,crn,crm,op2)` packing (shifts 19/16/12/8/5: arch/arm64/include/asm/sysreg.h:29-43; convert with `esr_sys64_to_sysreg`, esr.h:294-304). It also differs from the HVF/KVM-uapi 16-bit packing (§1.6, §4.6).

**Other ISS encodings**

| Class | Fields | Source |
|---|---|---|
| WFx (EC 0x01) | CV[24] (1 for AArch64); COND[23:20] (0b1110 for AArch64); RN[9:5] and RV[2] (FEAT_WFxT); TI[1:0]: 00 WFI, 01 WFE, 10 WFIT, 11 WFET | D24.2.45; esr.h:179-187 |
| HVC64 (0x16) | [24:16] RES0; imm16[15:0] | D24.2.45; esr.h:188 (`ESR_ELx_xVC_IMM_MASK`) |
| SMC64 (0x17) | [24:16] RES0; imm16[15:0] | D24.2.45 |

### 2.2 PSCI (DEN0022F.b = PSCI 1.3)

Calling convention:
- Functions with only 32-bit parameters use W0-W3 and return in W0. SMC64 functions use X0-X3 and return in X0 (§5.2.1 p.48).
- The HVC conduit uses the same format as SMC (§5 p.29).
- For SMC32 IDs, KVM clears the upper 32 bits of X1-X3 (arch/arm64/kvm/psci.c:223-233).

| Function | SMC32 ID | SMC64 ID | Args (x1, x2, x3) | Returns | In 1.x (Table 17 p.95-96) | Source |
|---|---|---|---|---|---|---|
| PSCI_VERSION | 0x84000000 | – | – | [31:16] major, [15:0] minor | Mandatory | §5.1.1 p.29; include/uapi/linux/psci.h:33 |
| CPU_SUSPEND | 0x84000001 | 0xC4000001 | power_state, entry_point, context_id | SUCCESS, INVALID_PARAMETERS, INVALID_ADDRESS, DENIED (OSI) | Mandatory | §5.1.2 p.29-30; psci.h:34,44 |
| CPU_OFF | 0x84000002 | – | (Linux passes power_state) | no return on success, else DENIED | Mandatory | §5.1.3 p.31; psci.h:35 |
| CPU_ON | 0x84000003 | 0xC4000003 | target_cpu (Aff3[39:32] Aff2[23:16] Aff1[15:8] Aff0[7:0]; other bits 0), entry_point (PA/IPA), context_id | SUCCESS, INVALID_PARAMETERS, INVALID_ADDRESS, ALREADY_ON, ON_PENDING, INTERNAL_FAILURE, DENIED | Mandatory | §5.1.4 p.31-33, §5.6 p.56-57; psci.h:36,45 |
| AFFINITY_INFO | 0x84000004 | 0xC4000004 | target_affinity, lowest_affinity_level | 0 ON, 1 OFF, 2 ON_PENDING, INVALID_PARAMETERS, DISABLED. From 1.0, level > 0 may return INVALID_PARAMETERS. | Mandatory | §5.1.5 p.33-34, §5.7 p.57-58; psci.h:37,46,92-94 |
| MIGRATE | 0x84000005 | 0xC4000005 | target_cpu | SUCCESS, NOT_SUPPORTED, INVALID_PARAMETERS, DENIED, INTERNAL_FAILURE, NOT_PRESENT | Optional | §5.1.6 p.34-35 |
| MIGRATE_INFO_TYPE | 0x84000006 | – | – | 0 UP trusted OS, migratable. 1 UP, not migratable. **2 trusted OS not present / no migration needed.** NOT_SUPPORTED ≡ 2. | Optional | §5.1.7 p.35-36; psci.h:97-99 |
| MIGRATE_INFO_UP_CPU | 0x84000007 | 0xC4000007 | – | MPIDR of the resident trusted-OS CPU | Optional (mandatory if MIGRATE) | §5.1.8 p.36-37 |
| SYSTEM_OFF | 0x84000008 | – | – | doesn't return | Mandatory | §5.1.9 p.37 |
| SYSTEM_RESET | 0x84000009 | – | – | doesn't return | Mandatory | §5.1.11 p.39 |
| PSCI_FEATURES | 0x8400000A | – | psci_func_id (a PSCI ID or SMCCC_VERSION 0x80000000) | NOT_SUPPORTED, or feature flags with bit31 = 0 | Mandatory from 1.0 | §5.1.15 p.42, §5.16 p.66-68 |
| CPU_FREEZE | 0x8400000B | – | – | no return; NOT_SUPPORTED, DENIED | Optional | §5.1.16 p.43 |
| CPU_DEFAULT_SUSPEND | 0x8400000C | 0xC400000C | entry, context | SUCCESS, INVALID_ADDRESS | Optional | §5.1.17 p.43-44 |
| NODE_HW_STATE | 0x8400000D | 0xC400000D | target_cpu, power_level | 0 HW_ON, 1 HW_OFF, 2 HW_STANDBY, NOT_SUPPORTED, INVALID_PARAMETERS | Optional | §5.1.18 p.44-45 |
| SYSTEM_SUSPEND | 0x8400000E | 0xC400000E | entry, context | no return; NOT_SUPPORTED, INVALID_ADDRESS, ALREADY_ON | Optional | §5.1.19 p.45-46 |
| PSCI_SET_SUSPEND_MODE | 0x8400000F | – | mode (0 platform-coordinated, 1 OS-initiated) | SUCCESS, NOT_SUPPORTED, INVALID_PARAMETERS, DENIED | Optional | §5.1.20 p.46; psci.h:127-128 |
| PSCI_STAT_RESIDENCY | 0x84000010 | 0xC4000010 | target_cpu, power_state | microseconds | Optional (pairs with STAT_COUNT) | §5.1.21 p.47 |
| PSCI_STAT_COUNT | 0x84000011 | 0xC4000011 | target_cpu, power_state | count | Optional | §5.1.22 p.47-48 |
| SYSTEM_RESET2 | 0x84000012 | 0xC4000012 | reset_type (bit31: 1 vendor, 0 architectural; 0 = SYSTEM_WARM_RESET), cookie | no return; NOT_SUPPORTED, INVALID_PARAMETERS | Optional (1.1+) | §5.1.12 p.39-40; psci.h:59,70,102-103 |
| MEM_PROTECT | 0x84000013 | – | enable | previous state (1/0), NOT_SUPPORTED | Optional (1.1+) | §5.1.13 p.40-41 |
| MEM_PROTECT_CHECK_RANGE | 0x84000014 | 0xC4000014 | base, length | SUCCESS, DENIED, NOT_SUPPORTED | Optional | §5.1.14 p.41-42 |
| SYSTEM_OFF2 | 0x84000015 | 0xC4000015 | type (0, or 1 = HIBERNATE_OFF; from F.b, 0 also means HIBERNATE_OFF), cookie (must be 0) | no return; NOT_SUPPORTED, INVALID_PARAMETERS | Optional (1.3) | §5.1.10 p.37-38; psci.h:62,72,106 |

**Return codes** (§5.2.2 Table 5 p.49; include/uapi/linux/psci.h:131-140):

| Code | Value |
|---|---|
| SUCCESS | 0 |
| NOT_SUPPORTED | -1 |
| INVALID_PARAMETERS | -2 |
| DENIED | -3 |
| ALREADY_ON | -4 |
| ON_PENDING | -5 |
| INTERNAL_FAILURE | -6 |
| NOT_PRESENT | -7 |
| DISABLED | -8 |
| INVALID_ADDRESS | -9 |

Codes are int32 for SMC32 IDs and int64 for SMC64 IDs. An AArch32 caller of an SMC64 ID gets 0xFFFFFFFF.

**Versions** (Table 6 p.49-50): F.b = 1.3, E = 1.2, D = 1.1, C = 1.0, B.b = 0.2. Encoding is major<<16 | minor (psci.h:109-119).

**PSCI_FEATURES flags** (Table 11 p.67-68):
- CPU_SUSPEND: bit1 = extended StateID format, bit0 = OS-initiated mode supported.
- SYSTEM_OFF2: bits[30:0] = supported hibernate types (bit0, HIBERNATE_OFF, must be 1).
- All other functions: 0.

**power_state layout** (§5.4.2 p.50-52; psci.h:75-89):
- Original format: PowerLevel[25:24], StateType[16], StateID[15:0].
- Extended format: StateType[30], StateID[27:0].

**State of a CPU started by CPU_ON or resuming from suspend** (§6.4 p.83-86):

| Item | Spec | KVM reference |
|---|---|---|
| Entry EL | Highest non-secure EL (EL2 if a hypervisor is enabled, else EL1), SP_ELxh | PSTATE EL1h\|A\|I\|F\|D = 0x3C5 (EL2h with NV): arch/arm64/kvm/reset.c:40-44,213-218 |
| DAIF | SPSR.{D,A,I,F} = 1111 (§6.4.3.3) | same |
| MMU/caches | SCTLR_ELx.{I,C,M} = 0; caches clean and coherent from the caller's view (§6.4.3.4) | vCPU sysreg reset (SCTLR_EL1 = 0x00C50078: arch/arm64/kvm/sys_regs.c:3377) |
| Endianness | SCTLR_ELx.EE = caller's (§6.4.3.2) | `reset_state.be` (arch/arm64/kvm/psci.c:95-96; reset.c:245-247) |
| X0 | context_id (§6.4.3.7) | `reset_state.r0` → X0 (kvm/psci.c:98-102; reset.c:264) |
| PC | entry_point_address, physical from the caller's view (§5.6.2) | kvm/psci.c:93; reset.c:236-249 |
| CNTFRQ | must be initialised by firmware (§6.4.3.6) | (not trappable at EL1: see 2.5) |

**Linux boot-time PSCI probe order** (DT boot):
1. `setup_arch` calls `psci_dt_init` (arch/arm64/kernel/setup.c:352-353).
2. The compatible string selects the version: "arm,psci" → 0.1, "arm,psci-0.2" → 0.2, "arm,psci-1.0" → 1.0 (drivers/firmware/psci/psci.c:801-806).
3. `method` must be "hvc" or "smc" (psci.c:287-307).
4. `psci_probe` (psci.c:690-717) then does the following, in order:
   - a) **PSCI_VERSION.** If major = 0 and minor < 2: "Conflicting PSCI version" and **PSCI is abandoned**.
   - b) Installs the 0.2 ops, the restart handler and pm_power_off (:668-685).
   - c) **MIGRATE_INFO_TYPE** (:607-641). A result of 0 or 1 leads to MIGRATE_INFO_UP_CPU (0xC4000007), which pins `resident_cpu` and blocks hot-unplug of that CPU (arch/arm64/kernel/psci.c:50-66).
   - d) If major ≥ 1, in order:
     - `psci_init_smccc` calls PSCI_FEATURES(0x80000000). If that isn't -1, it calls SMCCC_VERSION, and if the result is ≥ 0x10001 it runs `arm_smccc_version_init` (:643-666).
     - PSCI_FEATURES(0xC4000001) (:595-601).
     - PSCI_FEATURES(0xC400000E): anything other than -1 installs suspend-to-RAM (:582-593).
     - PSCI_FEATURES(0xC4000012): **anything other than -1** makes warm/soft reboot use SYSTEM_RESET2(0,0) (:560-568,309-322).
     - PSCI_FEATURES(0xC4000015): bit0 enables hibernate-off (:570-580).
     - `kvm_init_hyp_services` (:713).
   - e) `psci_1_0_init`: if the CPU_SUSPEND flags have bit0 set, calls SET_SUSPEND_MODE(PC) (:783-799).
5. **SMP bring-up:** CPU_ON(0xC4000003, target = MPIDR, entry = `__pa_symbol(secondary_entry)`, context = 0) (arch/arm64/kernel/psci.c:39-47; psci.c:217-223; `PSCI_FN_NATIVE` at psci.c:38-42).
6. **Hot-unplug:** CPU_OFF (StateType = power-down) (arm64/kernel/psci.c:68-78), then AFFINITY_INFO(mpidr, 0) polled until OFF (1), up to 100 ms (:80-109).
7. **Power-off:** SYSTEM_OFF (0x84000008) (psci.c:332-335).

**KVM reference answers** (arch/arm64/kvm/psci.c):

| Call | KVM behaviour | Source |
|---|---|---|
| PSCI_VERSION | 1.minor; default `KVM_ARM_PSCI_LATEST` = 1.3 | include/kvm/arm_psci.h:13-20 |
| CPU_SUSPEND | Treated as WFI, returns SUCCESS | :34-52 |
| CPU_ON | INVALID_PARAMETERS for an unknown MPIDR (mask `MPIDR_HWID_BITMASK` 0xff00ffffff: arch/arm64/include/asm/cputype.h:12); ALREADY_ON if the target is running | :60-119 |
| AFFINITY_INFO | INVALID_PARAMETERS if no vCPU matches | :121-162 |
| MIGRATE_INFO_TYPE | 2 | :280-287 |
| SYSTEM_OFF / RESET / RESET2 / OFF2 | Exit with SYSTEM_EVENT; X0 preloaded with INTERNAL_FAILURE | :164-212,288-311,388-432 |
| PSCI_FEATURES | 0 for VERSION, CPU_SUSPEND(32/64), CPU_OFF, CPU_ON(32/64), AFFINITY_INFO(32/64), MIGRATE_INFO_TYPE, SYSTEM_OFF, SYSTEM_RESET, PSCI_FEATURES, SMCCC_VERSION. 0 for SYSTEM_RESET2 if minor ≥ 1. 1 for SYSTEM_OFF2 if minor ≥ 3. Otherwise -1. | :333-373 |
| SMC64 ID from AArch32 | NOT_SUPPORTED | :235-244 |

### 2.3 SMCCC (DEN0028 v1.6 G)

**Function-ID layout** (§2.5 Table 2-1 p.14; include/linux/arm-smccc.h:25-62):

| Bits | Meaning |
|---|---|
| [31] | 1 = fast call |
| [30] | 0 = SMC32/HVC32, 1 = SMC64/HVC64 |
| [29:24] | Owner: 0 Arm arch, 1 CPU, 2 SiP, 3 OEM, 4 standard secure (PSCI, SDEI, TRNG, FF-A…), 5 standard hypervisor, 6 vendor hypervisor, 7 vendor EL3 monitor, 48-49 trusted apps, 50-63 trusted OS |
| [23:17] | Must be zero |
| [16] | SVE live-state hint (≥ 1.3). **Must be ignored** when identifying the function. |
| [15:0] | Function number |

**Reserved ranges** (Tables 6-2..6-5 p.25-27):

| Service | SMC32 range | SMC64 range |
|---|---|---|
| PSCI | 0x8400_0000-001F | 0xC400_0000-001F |
| SDEI | 0x8400_0020-003F | 0xC400_0020-003F |
| TRNG | 0x8400_0050-005F | 0xC400_0050-005F |
| FF-A | 0x8400_0060-00EF | 0xC400_0060-00EF |
| PV Time | – | 0xC500_0020-003F |

**Conduit and registers:**
- EL2 without EL3 means HVC is the only conduit (Table 2-2 p.15).
- SMC32: arguments in W1-W7, results in W0-W7, X8-X30 preserved (§2.6 p.15).
- SMC64: arguments in X1-X17, results in X0-X17, X18-X30 preserved (§2.7 p.16).
- Compliant calls use immediate 0 (§2.10 p.17).
- **An unknown function ID returns sign-extended -1 in X0** (§5.2 p.22).

**Return codes** (§7.1 Table 7-1 p.28; arm-smccc.h:311-314): SUCCESS 0, NOT_SUPPORTED -1, NOT_REQUIRED -2, INVALID_PARAMETER -3. `SMCCC_ARCH_WORKAROUND_RET_UNAFFECTED` = 1 (arm-smccc.h:210).

| Call | ID | Returns / semantics | Linux use | Source |
|---|---|---|---|---|
| SMCCC_VERSION | 0x80000000 | bit31 = 0, [30:16] major, [15:0] minor; -1 means v1.0. 1.1 = 0x10001 … 1.6 = 0x10006. | Called only after PSCI_FEATURES(0x80000000) ≠ -1. Needs ≥ 0x10001 to enable the conduit (psci.c:643-666; drivers/firmware/smccc/smccc.c:44-50). | §7.2 p.28-29, App. F Table F0-1 p.52; arm-smccc.h:69-72,78-81 |
| SMCCC_ARCH_FEATURES | 0x80000001 | Argument is an ID in the 0x8000/0xC000 or 0x8500/0xC500 ranges. <0 means not implemented, ≥0 implemented. Must return 0 for VERSION and FEATURES. | Many probes | §7.3 p.29-30; arm-smccc.h:83-86 |
| SMCCC_ARCH_SOC_ID | 0x80000002 / 0xC0000002 | type 0: JEP-106 SiP + SoC ID; 1: revision; 2: name (SMC64 only). Otherwise INVALID_PARAMETER. | Probed only if SMCCC ≥ 1.2 (smccc.c:31-41) | §7.4 p.30-32; arm-smccc.h:88-96 |
| SMCCC_ARCH_FEATURE_AVAILABILITY | 0xC0000003 | EL3 feature bitmask (1.5+) | not used by Linux 7.2 | §7.8 p.38-40 |
| SMCCC_ARCH_WORKAROUND_1 | 0x80008000 | No return value. Discovery: NOT_SUPPORTED / 0 / 1. | Spectre-v2 | §7.5 Table 7-3 p.32-33; arm-smccc.h:98-101 |
| SMCCC_ARCH_WORKAROUND_2 | 0x80007FFF | No return value; argument = enable. Discovery: NOT_SUPPORTED / NOT_REQUIRED / 0 / 1. | Spectre-v4 (SSBD) | §7.6 Table 7-4 p.34-35; arm-smccc.h:103-106 |
| SMCCC_ARCH_WORKAROUND_3 | 0x80003FFF | No return value. Discovery: NOT_SUPPORTED / 0 / 1. If implemented, WORKAROUND_1 must also be reported. | Spectre-BHB | §7.7 Table 7-5 p.36-37; arm-smccc.h:108-111 |
| SMCCC_ARCH_WORKAROUND_4 | 0x80000004 | Presence means a higher EL mitigates CVE-2024-7881; never actually called | not in Linux 7.2 | §7.9 p.41 |
| Vendor hyp call UID | 0x8600FF01 | UUID in W0-W3. KVM's is 28b46fb6-2ec5-11e9-a9ca-4b564d003a74. | `kvm_init_hyp_services` (drivers/firmware/smccc/kvm_guest.c:18-41; smccc.c:70-81) | Table 6-3 p.26; arm-smccc.h:119-129 |
| KVM vendor hyp calls | FEATURES 0x86000000, PTP 0x86000001, HYP_MEMINFO 0xC6000002, MEM_SHARE 0xC6000003, MEM_UNSHARE 0xC6000004, MMIO_GUARD 0xC6000007, DISCOVER_IMPL_VER 0xC6000040, DISCOVER_IMPL_CPUS 0xC6000041 | FEATURES returns a bitmap in a0-a3 | Used only if the UID matches KVM's | arm-smccc.h:132-257 |
| PV_TIME_FEATURES / PV_TIME_ST | 0xC5000020 / 0xC5000021 | SUCCESS / IPA of the stolen-time structure | arch/arm64/kernel/paravirt.c:135-144 | arm-smccc.h:264-274 |
| TRNG_VERSION / FEATURES / GET_UUID / RND32 / RND64 | 0x84000050 / 51 / 52 / 53 / 0xC4000053 | VERSION must be ≥ 0x10000 | Probed whenever SMCCC ≥ 1.1 (smccc.c:29; arch/arm64/include/asm/archrandom.h:11-24) | arm-smccc.h:277-305 |
| FFA_VERSION | 0x84000063 | -1 = not supported | `ffa_init` probes it only if SMCCC ≥ 1.2 (drivers/firmware/arm_ffa/smccc.c:20-39; driver.c:148-159) | include/linux/arm_ffa.h:26 |

**How Linux reacts to the Spectre workaround answers** (arch/arm64/kernel/proton-pack.c):
- Linux asks firmware only when the hardware doesn't already mitigate. CSV2 in ID_AA64PFR0_EL1 means v2 is unaffected (:165-168); SSBS means v4 is mitigated (:457-476).
- With no SMCCC ≥ 1.1 conduit, `arm_smccc_1_1_invoke` returns NOT_SUPPORTED **without trapping** (include/linux/arm-smccc.h:680-722).

| Answer to ARCH_FEATURES(x) | Linux behaviour | Source |
|---|---|---|
| WORKAROUND_1 = 0 | v2 mitigated. Installs an HVC callback that runs **on every address-space switch** and some EL0-entry paths. | :177-196,267-299; arch/arm64/mm/context.c:263; arch/arm64/kernel/entry-common.c:566ff |
| WORKAROUND_1 = 1 | v2 unaffected | :189-190 |
| WORKAROUND_1 = -1 or other | v2 vulnerable (no calls) | :191-194 |
| WORKAROUND_2 = 0 | v4 mitigated. Calls WORKAROUND_2(1) once; in dynamic mode, an HVC **on every kernel entry and exit from EL0**. | :479-500,613-630; arch/arm64/kernel/entry.S:114-128 |
| WORKAROUND_2 = 1 or -2 (NOT_REQUIRED) | v4 unaffected | :491-494 |
| WORKAROUND_2 = -1 | v4 vulnerable | :495-498 |
| WORKAROUND_3 = 0 / 1 / -1 | BHB mitigated / unaffected / vulnerable (firmware mitigation only if a conduit exists) | :929-957 |

KVM reports SMCCC_VERSION 1.1 (arch/arm64/kvm/hypercalls.c:290-291). It answers the workaround queries from the host's own state (:293-342) and falls through to PSCI for unknown IDs (:379-380).

### 2.4 GICv3/v4 (IHI0069H.b)

| Frame | Size / layout | Source |
|---|---|---|
| GICD | 64 KiB (register map to 0xFFFC) | §12.8 Table 12-25 p.12-533..535; include/linux/irqchip/arm-gic-v3.h:104 (`GIC_V3_DIST_SIZE` 0x10000) |
| GICR per PE (GICv3) | 2 contiguous 64 KiB frames: RD_base then SGI_base, 128 KiB total | §12.10 p.12-634; arm-gic-v3.h:258 (0x20000) |
| GICR per PE (GICv4) | RD_base, SGI_base, VLPI_base, reserved = 4 × 64 KiB | §12.10 p.12-634 |
| Linux redistributor walk | Stride 128 KiB (+128 KiB if GICR_TYPER.VLPIS). Stops at GICR_TYPER.**Last**. PIDR2.ArchRev must be 3 or 4. | drivers/irqchip/irq-gic-v3.c:984-1021 |
| Linux redistributor ↔ CPU match | GICR_TYPER[63:32] == the CPU's MPIDR Aff3.Aff2.Aff1.Aff0 | irq-gic-v3.c:1023-1056 |
| ITS | 64 KiB-aligned base. Control frame at +0x00000; translation frame at +0x10000 (**GITS_TRANSLATER = ITS base + 0x10040**, write-only); vSGI frame at +0x20000 (v4.1) | §12.18 Tables 12-33/12-34 p.12-823..824; arm-gic-v3.h:362-382 |

**INTID ranges** (§2.2 Table 2-1 p.2-35..36; irq-gic-v3.c:255-273):

| Range | Type |
|---|---|
| 0-15 | SGI |
| 16-31 | PPI |
| 32-1019 | SPI |
| 1020-1023 | Special. 1023 = spurious (`ICC_IAR1_EL1_SPURIOUS` 0x3ff, arm-gic-v3.h:586). |
| 1056-1119 | Extended PPI (GICv3.1; `EPPI_BASE_INTID` 1056, arm-gic-v3.h:136) |
| 4096-5119 | Extended SPI (`ESPI_BASE_INTID` 4096, arm-gic-v3.h:47) |
| 8192 and up | LPI (maximum IMPLEMENTATION DEFINED) |

Arm recommends SGIs 0-7 for non-secure use and 8-15 for secure use.

| Register | Offset | Fields that matter | Source |
|---|---|---|---|
| GICD_CTLR | 0x0000 | RWP[31], DS[6], ARE_NS[4], EnableGrp1A[1], EnableGrp1[0]. Layout depends on DS and the security view. | arm-gic-v3.h:13,58-63; §12.9.4 p.12-544 |
| GICD_TYPER | 0x0004 | ESPI_range[31:27], RSS[26], No1N[25], A3V[24], IDbits[23:19] (+1), DVIS[18], LPIS[17], MBIS[16], num_LPIs[15:11], SecurityExtn[10], NMI[9], ESPI[8], CPUNumber[7:5], ITLinesNumber[4:0] (max SPI INTID = 32(N+1)-1) | §12.9.38 p.12-618..621; arm-gic-v3.h:82-91 |
| GICD_IIDR / TYPER2 / STATUSR | 0x0008 / 0x000C / 0x0010 | | Table 12-25; arm-gic-v3.h:15-17 |
| GICD_SETSPI_NSR / CLRSPI_NSR | 0x0040 / 0x0048 | Message-based SPI doorbells | arm-gic-v3.h:18-19 |
| GICD_IGROUPR / ISENABLER / ICENABLER / ISPENDR / ICPENDR / ISACTIVER / ICACTIVER / IPRIORITYR / ICFGR / IGRPMODR | 0x080 / 0x100 / 0x180 / 0x200 / 0x280 / 0x300 / 0x380 / 0x400 / 0xC00 / 0xD00 | Per-INTID banks | arm-gic-v3.h:22-31 |
| `GICD_IROUTER<n>` | 0x6000 + 8n (n ≥ 32; spec 0x6100-0x7FD8) | bit31 = IRM (1 = any PE) | arm-gic-v3.h:42,97-98; Table 12-25 |
| GICD_PIDR2 | 0xFFE8 | ArchRev[7:4]: 0x3 GICv3, 0x4 GICv4. Linux refuses anything else. | Table 12-18 p.12-210; arm-gic-v3.h:45,100-102; irq-gic-v3.c:2075-2083 |
| GICR_CTLR | RD+0x0000 | EnableLPIs[0], RWP[3] | arm-gic-v3.h:114,129-132 |
| GICR_TYPER | RD+0x0008 (64-bit) | Affinity[63:32], PPInum[31:27], CommonLPIAff[25:24], Processor_Number[23:8], Last[4], DirectLPI[3], VLPIS[1], PLPIS[0] | §12.11.37 p.12-711; arm-gic-v3.h:116,242-249 |
| GICR_WAKER | RD+0x0014 | ProcessorSleep[1], ChildrenAsleep[2]. Linux clears ProcessorSleep and polls ChildrenAsleep until 0 (1 s timeout). | §12.11.42 p.12-728; irq-gic-v3.c:367-399 |
| GICR_PROPBASER / PENDBASER | RD+0x0070 / 0x0078 | LPI tables | arm-gic-v3.h:121-122 |
| SGI/PPI registers | SGI_base + GICD-like offsets (IGROUPR0 0x80, ISENABLER0 0x100, IPRIORITYR 0x400, ICFGR0/1 0xC00/0xC04) | | §12.10; arm-gic-v3.h:230-240 |

**ICC system registers** (`sys_reg(op0,op1,CRn,CRm,op2)`; arch/arm64/include/asm/sysreg.h):

| Register | Encoding | Line |
|---|---|---|
| ICC_PMR_EL1 | (3,0,4,6,0) | :301 |
| ICC_DIR_EL1 | (3,0,12,11,1) | :385 |
| ICC_RPR_EL1 | (3,0,12,11,3) | :386 |
| ICC_SGI1R_EL1 | (3,0,12,11,5) | :387 |
| ICC_ASGI1R_EL1 | (3,0,12,11,6) | :388 |
| ICC_SGI0R_EL1 | (3,0,12,11,7) | :389 |
| ICC_IAR1_EL1 | (3,0,12,12,0) | :390 |
| ICC_EOIR1_EL1 | (3,0,12,12,1) | :391 |
| ICC_HPPIR1_EL1 | (3,0,12,12,2) | :392 |
| ICC_BPR1_EL1 | (3,0,12,12,3) | :393 |
| ICC_CTLR_EL1 | (3,0,12,12,4) | :394 |
| ICC_SRE_EL1 | (3,0,12,12,5) | :395 |
| ICC_IGRPEN1_EL1 | (3,0,12,12,7) | :397 |

ICC_SRE_EL1 bits are SRE[0], DFB[1], DIB[2] (arm-gic-v3.h:577-579).

ICC_SGI1R_EL1 layout: TargetList[15:0], Aff1[23:16], INTID[27:24], Aff2[39:32], IRM[40], RS[47:44], Aff3[55:48] (arm-gic-v3.h:591-603).

**DT specifier for GIC interrupts**: `<type number flags>`.
- type: 0 SPI, 1 PPI, 2 ESPI, 3 EPPI.
- number: SPI = INTID-32, **PPI = INTID-16**.
- flags: 1 edge rising, 4 level high.
- **SPIs accept only 1 or 4** (irq-gic-v3.c:716-718). PPIs accept any level type, including 8 (level-low), which is treated as level (drivers/irqchip/irq-gic-common.c:63-66).
- Sources: arm,gic-v3.yaml:39-69; irq-gic-v3.c:1604-1632; include/dt-bindings/interrupt-controller/arm-gic.h:13-14; irq.h:14,17,18.

### 2.5 Generic Timer

| Register | (op0,op1,CRn,CRm,op2) | Semantics | Source |
|---|---|---|---|
| CNTFRQ_EL0 | 3,3,14,0,0 | ClockFreq[31:0] Hz. Writable only at the highest EL. **An EL1 MRS reads it directly, with no trap condition in the pseudocode**, so a hypervisor can't virtualize it. | DDI0487 M.c D24.10.1 (access pseudocode: EL1 → `X[t] = CNTFRQ_EL0()`); sysreg.h:463 |
| CNTPCT_EL0 | 3,3,14,0,1 | physical count | sysreg.h:465 |
| CNTVCT_EL0 | 3,3,14,0,2 | = physical count − CNTVOFF_EL2 (EL0/EL1 reads) | D24.10.26, D12.2.2; sysreg.h:466 |
| CNTV_CTL_EL0 | 3,3,14,3,1 | ENABLE[0], IMASK[1], ISTATUS[2] (RO). The IRQ asserts when ENABLE=1, ISTATUS=1 and IMASK=0. | D24.10.28; include/clocksource/arm_arch_timer.h:12-14; sysreg.h:475 |
| CNTV_CVAL_EL0 | 3,3,14,3,2 | 64-bit compare; fires when CNTVCT − CVAL ≥ 0 | D24.10.29/30; sysreg.h:476 |
| CNTV_TVAL_EL0 | 3,3,14,3,0 | Read: (CVAL − CNTVCT)[31:0]. Write: CVAL = SignExtend(TVAL[31:0]) + CNTVCT. | D24.10.30; sysreg.h:474 |
| CNTP_CTL/CVAL/TVAL_EL0 | 3,3,14,2,{1,2,0} | physical timer | sysreg.h:470-472 |
| CNTVOFF_EL2 | 3,4,14,0,3 | 64-bit virtual offset; UNKNOWN on warm reset | D24.10.27; sysreg.h:594 |
| CNTHCTL_EL2 / CNTKCTL_EL1 | 3,4,14,1,0 / 3,0,14,1,0 | trap and access controls | sysreg.h:401,595 |

| Timer | Linux index (= DT position) | interrupt-names | Usual DT cell (INTID) | KVM default INTID | Source |
|---|---|---|---|---|---|
| EL1 secure phys | 0 | sec-phys | `<1 13 f>` (29) | — | include/clocksource/arm_arch_timer.h:34-41; drivers/clocksource/arm_arch_timer.c:46-52; arm,arch_timer.yaml:33,116 |
| EL1 NS phys | 1 | phys | `<1 14 f>` (30) | 30 | yaml:34,117; arch/arm64/kvm/arch_timer.c:35-39 |
| EL1 virt | 2 | virt | `<1 11 f>` (27) | 27 | yaml:35,118 |
| EL2 phys | 3 | hyp-phys | `<1 10 f>` (26) | 26 | yaml:36,119 |
| EL2 virt (VHE) | 4 | hyp-virt | `<1 12 f>` (28, inferred from the KVM default) | 28 | yaml:37; KVM arch_timer.c:39 |

Timer notes:
- **Positional list.** Without `interrupt-names`, Linux reads timer interrupts purely by index (`of_irq_get(np, i)`, arm_arch_timer.c:1154-1161).
- **Which timer an EL1 guest uses.** With no EL2 it picks **virt (index 2)** and falls back to NS phys (arm_arch_timer.c:1115-1132).
- **clock-frequency** overrides CNTFRQ but is "strongly discouraged" (arm_arch_timer.c:864-876).
- **Snapshot (derived from D12.2.2 and D24.10.29/30).** Save the 64-bit CNTV_CVAL and CNTV_CTL, not TVAL. On restore, choose the offset so CNTVCT continues: CNTVOFF_new = CNTPCT_now − CNTVCT_saved. On HVF this is `vtimer_offset` (§1.7).

### 2.6 PL011 (DDI0183G r1p5; include/linux/amba/serial.h; drivers/tty/serial/amba-pl011.c)

The register map is TRM §3.2 Table 3-1 p.3-3..3-4. The FIFO is 32 deep when enabled, 1 when disabled (§1.1.2 p.1-3).

| Offset | Name | TRM reset | Linux macro / bits used | Minimum VMM behaviour | Source |
|---|---|---|---|---|---|
| 0x000 | UARTDR | – | DR; RX error flags OE[11] BE[10] PE[9] FE[8] | write: emit byte to host. read: pop byte, error flags 0 | serial.h:25,78-81 |
| 0x004 | UARTRSR/ECR | 0x0 | RSR[3:0] | RAZ/WI | serial.h:26-27,83-86 |
| 0x018 | UARTFR | 0b-10010--- | RI[8] TXFE[7] RXFF[6] **TXFF[5]** **RXFE[4]** **BUSY[3]** DCD[2] DSR[1] CTS[0] | TXFF=0, BUSY=0, TXFE=1; RXFE = input buffer empty; modem bits 0 | TRM Table 3-4 p.3-8; serial.h:34,88-96 |
| 0x020 | UARTILPR | 0x00 | unused | RAZ/WI | serial.h:38 |
| 0x024 / 0x028 | IBRD / FBRD | 0x0000 / 0x00 | written when setting line speed | store and read back | serial.h:39-40 |
| 0x02C | UARTLCR_H | 0x00 | WLEN[6:5], FEN[4], … | store and read back | serial.h:41,126-135 |
| 0x030 | UARTCR | 0x0300 | UARTEN[0], TXE[8], RXE[9], RTS[11], DTR[10] | store and read back | serial.h:43,108-124 |
| 0x034 | UARTIFLS | 0x12 | RX[5:3], TX[2:0] | store and read back | serial.h:44,159-170 |
| 0x038 | UARTIMSC | 0x000 | RXIM[4], TXIM[5], RTIM[6], … (1 = enabled) | store; IRQ line = (RIS & IMSC) ≠ 0 | serial.h:45,175-185 |
| 0x03C / 0x040 | RIS / MIS | 0x00- | same bit positions | RXIS/RTIS level-follow the RX buffer | serial.h:46-47,187-197 |
| 0x044 | UARTICR | WO | write-1-to-clear | clear latched bits; reads return 0 (the driver may read it: amba-pl011.c:1652) | serial.h:48,199-209 |
| 0x048 | UARTDMACR | 0x00 | written 0 | RAZ/WI | serial.h:49,211-213 |
| 0xFE0-0xFEC | PeriphID0-3 | 0x11, 0x10, 0x_4 (rev nibble; r1p5 → 0x3), 0x00 | — | return 0x11, 0x10, 0x34, 0x00 | TRM Table 3-1, Table 3-21 p.3-24 |
| 0xFF0-0xFFC | PCellID0-3 | 0x0D, 0xF0, 0x05, 0xB1 | `AMBA_CID` 0xb105f00d | return these | TRM Table 3-1; include/linux/amba/bus.h:22 |

**Driver match and AMBA bus behaviour:**
- Linux matches {0x00041011, mask 0x000fffff} for Arm, plus ST 0x00380802 and NVIDIA 0x0006b011 (amba-pl011.c:3199-3214). FIFO depth is 16 if rev < 3, else 32 (:123-126). The peripheral ID is config[31:24], revision[23:20], manufacturer[19:12], part[11:0] (include/linux/amba/bus.h:144-152).
- The AMBA bus reads IDs with 32-bit `readl` at **(DT reg size − 0x20)** and **(size − 0x10)**. DT reg size must therefore be 0x1000 to land on 0xFE0/0xFF0 (drivers/amba/bus.c:138-152).
- The read is skipped if DT has `arm,primecell-periphid` (drivers/of/platform.c:238; bus.c:186-188).
- The bus **requires a clock named "apb_pclk"** for the ID read and for probe (bus.c:63-76,119-123,273-275). Only nodes whose `compatible` includes "arm,primecell" become AMBA devices (platform.c:361).
- The PL011 driver also needs a first clock, whose rate becomes the UART clock (amba-pl011.c:3019-3021,1849).

**Access widths:**
- The main driver uses **16-bit** `readw`/`writew` unless `reg-io-width = <4>` (amba-pl011.c:351-369,3026-3046).
- Earlycon ("arm,pl011", amba-pl011.c:2823) uses **32-bit `readl` on FR and 8-bit `writeb` on DR** (:2736-2746), plus a 16-bit read-modify-write of CR (:2809-2818).
- The emulation must therefore accept 8-, 16- and 32-bit accesses.

**IRQ and startup sequence:**
- Registration writes IMSC = 0 and ICR = 0xffff (:2973-2975).
- Startup:
  1. Clears error, RX and RT bits via ICR.
  2. Sets IMSC = RTIM|RXIM.
  3. Writes IFLS.
  4. Sets CR = UARTEN|RXE|TXE (:1833-1871,1959-2004).
- The IRQ handler reads **RIS & its own IMSC copy**, not MIS. It never ICR-clears TXIS/RXIS/RTIS (:1663-1687), so the RX interrupt must drop when the buffer drains or the guest takes an IRQ storm.
- TX writes DR directly while FR.TXFF = 0 (:1555-1612) and enables TXIM only if the FIFO fills.
- TRM: the TX interrupt is not set on enable with an empty FIFO; it asserts on a level crossing (§2.8.3 p.2-23).

### 2.7 PL031 (DDI0224C r1p3; drivers/rtc/rtc-pl031.c)

| Offset | Name | Width / reset | Linux use | Minimum VMM behaviour | Source |
|---|---|---|---|---|---|
| 0x000 | RTCDR | RO 32, 0 | `read_time` (u32 seconds) | host time + offset | TRM Table 3-1 p.3-3; rtc-pl031.c:28,249 |
| 0x004 | RTCMR | RW 32 | alarm (u32 seconds) | store; set RIS=1 when the counter equals MR | :29,267,279 |
| 0x008 | RTCLR | RW 32 | `set_time` | offset = LR − now | :30,258 |
| 0x00C | RTCCR | RW 1 (bit0 start) | probe does `CR \|= EN` on every boot (:328-334) | keep running. The TRM says a write after enable resets to 0 (§3.3.4 p.3-5), so treat rewriting 1 as a no-op (recommendation, see §6). | :31,44 |
| 0x010 | RTCIMSC | RW 1 (1 = enabled) | `alarm_irq_enable` (:95-110) | store | §3.3.5 p.3-5; :32,50 |
| 0x014 / 0x018 | RTCRIS / RTCMIS | RO 1 | pending / IRQ handler (:233) | RIS; MIS = RIS & IMSC | §3.3.6-3.3.7 |
| 0x01C | RTCICR | WO 1 | W1C (:102,235) | clear RIS | §3.3.8 p.3-6 |
| 0xFE0-0xFFC | PeriphID 0x31, 0x10, 0x04, 0x00; PCellID 0x0D, 0xF0, 0x05, 0xB1 | RO 8 | AMBA bus (32-bit readl) | return these | TRM Table 3-1 |

- **Match:** {0x00041031, mask 0x000fffff} for Arm; ST variants 0x00180031 / 0x00280031 (rtc-pl031.c:435-453).
- **Accesses:** all 32-bit `readl`/`writel`.
- **IRQ:** optional. Without one the alarm feature is cleared (:360-361).
- **Bus rules:** same apb_pclk and 0x1000 reg-size rules as the PL011.

### Section 2 bug-magnets

- **PC advance.**
  - SMC trapped by TSC, MMIO data abort, WFx, MSR/MRS: PC += 4 (DDI0487 D1.4.1.5 rules LBLBR and QYCWH).
  - HVC: already past the instruction (DKWPP).
- **MMIO decode.**
  - SRT=31 is the zero register (Linux emits `str wzr` for `writel(0, …)`), not SP.
  - Apply SSE sign-extension and SF truncation.
  - Never treat CM=1 (WnR forced 1) or S1PTW=1 as device writes.
  - ISV=0 (atomics, pairs, writeback) needs decode or an abort.
- **Unknown SMCCC or PSCI IDs → sign-extended -1.**
  - Linux treats *any* non-(-1) PSCI_FEATURES answer as "supported" for SYSTEM_RESET2 and SYSTEM_SUSPEND.
  - It queries PSCI_FEATURES using **SMC64** IDs (0xC4000001, 0xC400000E, 0xC4000012, 0xC4000015).
- **Spectre answers cost exits.**
  - WORKAROUND_1 = 0 means an HVC on every address-space switch.
  - WORKAROUND_2 = 0 (dynamic) means an HVC on every EL0 entry and exit.
  - Answer 1 / NOT_REQUIRED only when true, else NOT_SUPPORTED.
  - Reporting SMCCC ≥ 1.2 adds SOC_ID and FF-A probes; 1.1 (as KVM reports) avoids them.
- **PSCI_VERSION must be ≥ 0.2**, or Linux abandons PSCI and cannot start secondaries. MIGRATE_INFO_TYPE should be 2 (or -1), because 0 or 1 pins a CPU.
- **GIC redistributors.**
  - Each GICR_TYPER affinity must equal its vCPU's MPIDR.
  - The final redistributor needs GICR_TYPER.Last.
  - PIDR2.ArchRev must be 3 or 4.
  - GICR_WAKER.ChildrenAsleep must follow ProcessorSleep, or Linux waits 1 s per CPU.
- **Timers.**
  - The DT list is positional [sec-phys, phys, virt, hyp-phys]. The guest uses **virt = INTID 27 = `<1 11 4>`**.
  - CNTFRQ_EL0 can't be trapped at EL1, so the guest always sees the host counter frequency.
  - Restore CNTVOFF from the saved CNTVCT, not TVAL.
- **PL011 / PL031.**
  - DT reg size 0x1000 (IDs are read at size−0x20 and size−0x10).
  - "apb_pclk" is mandatory.
  - PL011 must accept 8/16/32-bit accesses.
  - RX IRQ must drop when the buffer drains (Linux never ICR-clears RX/TX).

## 3. Linux arm64 boot contract, DT bindings, FDT format

Path aliases for this section (Linux tree): `booting.rst` = Documentation/arch/arm64/booting.rst; `bind/` = Documentation/devicetree/bindings/; `head.S` = arch/arm64/kernel/head.S; `asm/image.h` = arch/arm64/include/asm/image.h; `fdt.c` = drivers/of/fdt.c; `pl011.c` = drivers/tty/serial/amba-pl011.c; `libfdt/` = scripts/dtc/libfdt/. DTSpec page numbers are the printed page numbers (PDF page index = printed + 3).

### 3.1 Image header (64 bytes, all fields little-endian since v3.17: booting.rst:78-94; C struct asm/image.h:44-55)

| Off | Size | Field | Value / meaning | Source |
|---|---|---|---|---|
| 0x00 | 4 | code0 | Executable. With CONFIG_EFI it is `ccmp x18,#0,#0xd,pl`, whose encoding starts with "MZ" (PE/COFF). Otherwise a NOP. | asm/image.h:45; head.S:61; arch/arm64/kernel/efi-header.S:10-22 |
| 0x04 | 4 | code1 | `b primary_entry` | head.S:62 |
| 0x08 | 8 | text_offset | Offset from a 2 MiB-aligned base. This tree emits **0**. | booting.rst:82,138-141; head.S:63 |
| 0x10 | 8 | image_size | `_end - _text`. Covers the file **plus** BSS, `.pgtbl`, `init_pg_dir` and a 4 KiB early stack, so it is larger than the file. | booting.rst:83; arch/arm64/kernel/image.h:63-65; arch/arm64/kernel/vmlinux.lds.S:352-372 |
| 0x18 | 8 | flags | see the flags table | booting.rst:109-131 |
| 0x20/0x28/0x30 | 8 each | res2/3/4 | 0 | booting.rst:85-87; head.S:66-68 |
| 0x38 | 4 | magic | 0x644d5241 ("ARM\x64") | booting.rst:88; asm/image.h:6; head.S:69 |
| 0x3C | 4 | res5 | PE header offset (EFI stub). Ignored for direct boot. | booting.rst:89,98-101; head.S:70 |

**Flags field**

| Bits | Meaning | Encoding | Source |
|---|---|---|---|
| 0 | Kernel endianness | 0 LE, 1 BE | booting.rst:113; asm/image.h:8,12,16-17 |
| 2:1 | Kernel page size | 0 unspecified, 1 = 4K, 2 = 16K, 3 = 64K. Computed as `(PAGE_SHIFT-10)/2`. | booting.rst:114-119; asm/image.h:9,13,18-20; arch/arm64/kernel/image.h:50 |
| 3 | Physical placement | 0: the 2 MiB-aligned base should be as close as possible to the base of DRAM (memory below it is not linearly mapped). 1: any 2 MiB-aligned base with all image_size bytes inside the 48-bit PA range. This tree always sets 1. | booting.rst:120-129; asm/image.h:10-11,14,21; arch/arm64/kernel/image.h:52-56 |
| 63:4 | Reserved | — | booting.rst:130 |

**Placement rules**

| Rule | Detail | Source |
|---|---|---|
| Alignment | 2 MiB-aligned base + text_offset, anywhere in usable RAM. With text_offset = 0 that means a 2 MiB-aligned load address. | booting.rst:138-141; arch/arm64/include/asm/boot.h:15-18 (`MIN_KIMG_ALIGN` = SZ_2M) |
| Space | At least image_size bytes from the image start must be free. The kernel zeroes its own BSS and early page tables and memblock-reserves `_text`..`_end`. | booting.rst:142-143; arch/arm64/kernel/pi/map_kernel.c:251-252; arch/arm64/mm/init.c:290 |
| image_size == 0 | Pre-v3.17 kernel: text_offset is 0x80000. Leave as much space after the image as possible. | booting.rst:103-107,133-136 |
| Memory below the image | Usable since v4.6. Any described memory that isn't reserved is available. | booting.rst:144-146,152-155 |
| Compression | No decompressor on arm64. The VMM must gunzip Image.gz itself. | booting.rst:61-70 |
| Entry | The primary CPU jumps to the first instruction of the image (offset 0). | booting.rst:581 |
| initrd | Must lie within one 1 GiB-aligned window of at most 32 GiB that also covers the Image. Otherwise: WARN "initrd not fully accessible via the linear mapping" and the initrd is dropped. | booting.rst:148-150; arch/arm64/mm/init.c:257-283 |
| KASLR | Virtual offset mod 2 MiB = physical offset mod 2 MiB. Higher bits come from the seed. | arch/arm64/kernel/pi/map_kernel.c:245,268-281 |

### 3.2 CPU and register state at kernel entry

| Item | Requirement | Source |
|---|---|---|
| Primary GPRs | x0 = physical address of the DTB (in system RAM). **x1 = x2 = x3 = 0.** The kernel saves x0-x3 in `boot_args` and warns "x1-x3 nonzero in violation of boot protocol". | booting.rst:163-168; head.S:170-175; arch/arm64/kernel/setup.c:88,372-377 |
| PSTATE | D, A, I and F all masked. Non-secure EL2 (recommended) or EL1. VMM values: 0x3C5 (EL1h+DAIF) or 0x3C9 (EL2h+DAIF), built from EL1h 0x5, EL2h 0x9, F 0x40, I 0x80, A 0x100, D 0x200. | booting.rst:170-175; arch/arm64/include/uapi/asm/ptrace.h:32-48; arch/arm64/include/asm/ptrace.h:19-22 |
| MMU / caches | MMU off. The I-cache may be on or off but must hold no stale entries for the image. D-cache off (per the head.S comment). The image range must be cleaned to PoC by VA. head.S tolerates MMU-on entry (the EFI path), but the protocol requires MMU off. | booting.rst:177-191; head.S:49-51,133-165 |
| Architected timer | "CNTFRQ must be programmed with the timer frequency and CNTVOFF must be programmed with a consistent value on all CPUs." For EL1 entry, CNTHCTL_EL2.EL1PCTEN (bit 0) must be set where available. | booting.rst:193-198 |
| Coherency | All CPUs in the same coherency domain. | booting.rst:200-205 |
| System registers | All writable architected system registers at or below the entry EL must be initialised by a higher EL. Traps are allowed if handled transparently. | booting.rst:207-212,573-576 |
| Uniformity | All of the above applies to every CPU, and all CPUs enter at the same EL. | booting.rst:571-573 |
| enable-method | Required on each cpu node. Only used for secondaries: the boot CPU is matched by MPIDR and skipped. | booting.rst:581-587; arch/arm64/kernel/smp.c:702-718; arch/arm64/kernel/cpu_ops.c:68-76 |
| Boot-CPU identity | `MPIDR_EL1 & MPIDR_HWID_BITMASK` (0xff00ffffff) must equal some `/cpus/cpu` reg, or the kernel prints "missing boot CPU MPIDR, not enabling secondaries". The FDT header's `boot_cpuid_phys` is never read (grep finds no reference in drivers/of or arch/arm64). | arch/arm64/kernel/setup.c:90-93; arch/arm64/include/asm/cputype.h:12; arch/arm64/kernel/smp.c:680-731,747-749 |
| Secondaries (PSCI) | Held outside the kernel until Linux calls CPU_ON(target = DT reg (MPIDR), entry = PA of `secondary_entry`, context_id = 0). The target enters with x0-x3 = 0 and MMU off. `secondary_entry` zeroes x0 and runs `init_kernel_el`. | booting.rst:604-620; arch/arm64/kernel/psci.c:39-47; drivers/firmware/psci/psci.c:217-233; head.S:355-358 |
| Secondaries (spin-table) | `cpu-release-addr` points to a naturally aligned, zeroed 64-bit location in a /memreserve/ region. The CPU jumps when that location becomes non-zero (LE). | booting.rst:589-602 |

**Feature-specific requirements for a guest entered at EL1 with EL2 present.** There is no EL3 in a VM, so the "EL3 present" rows don't apply. The EL2 rows are what HVF or KVM (which own EL2) must present. The VMM influences them only through the features it exposes (§1, §4). booting.rst:573-576 allows traps that are handled transparently.

| Feature | Requirement (kernel entered at EL1, EL2 present) | EL3/EL2-entry-only rows (N/A in a VM) | Source |
|---|---|---|---|
| GICv3, v3 mode | ICC_SRE_EL2.Enable (bit 3) = 1 and SRE (bit 0) = 1. The DT describes a GICv3. If SRE can't be set: "GIC: unable to set SRE (disabled at EL2), panic ahead". | ICC_SRE_EL3.Enable/SRE = 1; ICC_CTLR_EL3.PMHE uniform | booting.rst:267-281; drivers/irqchip/irq-gic-v3.c:1148-1149; include/linux/irqchip/arm-gic-v3.h:645-658 |
| GICv3, v2 compat mode | ICC_SRE_EL2.SRE = 0; the DT describes a GICv2. | ICC_SRE_EL3.SRE = 0 | booting.rst:283-294 |
| GICv5 | The listed ICH_HFGRTR/HFGWTR/HFGITR_EL2 bits = 1. | — | booting.rst:226-265 |
| Pointer auth | HCR_EL2.APK (bit 40) = 1 and API (bit 41) = 1. | SCR_EL3.APK/API | booting.rst:296-306 |
| AMUv1 | AMCNTENSET0_EL0 = 0b1111; AMCNTENSET1_EL0 platform-specific. | CPTR_EL3/EL2.TAM = 0 | booting.rst:308-324 |
| FGT/FGT2/HCX | — | EL2 entry: SCR_EL3.FGTEn (bit 27), FGTEn2 (bit 59), HXEn (bit 38) = 1 | booting.rst:326-342 |
| FP/SIMD | CPTR_EL2.TFP (bit 10) = 0. | CPTR_EL3.TFP = 0 | booting.rst:344-352 |
| SVE | CPTR_EL2.TZ (bit 8) = 0; ZEN (bits 17:16) = 0b11; ZCR_EL2.LEN equal on all CPUs. | CPTR_EL3.EZ; ZCR_EL3.LEN | booting.rst:354-370 |
| SME / FA64 / SME2 | CPTR_EL2.TSM (bit 12) = 0; SMEN (bits 25:24) = 0b11; SCTLR_EL2.EnTP2 (bit 60) = 1; SMCR_EL2.LEN equal on all CPUs; HFG{R,W}TR_EL2.nTPIDR2_EL0 (bit 55) and nSMPRI_EL1 (bit 54) = 1; SMCR_EL2.FA64 (bit 31) = 1; EZT0 (bit 30) = 1. | CPTR_EL3.ESM; SCR_EL3.EnTP2; SMCR_EL3.* | booting.rst:372-430 |
| MTE2 | HCR_EL2.ATA (bit 56) = 1. | SCR_EL3.ATA | booting.rst:412-420 |
| BRBE / PMUv3p9 / SPE_FDS | BRBCR_EL2.CC (bit 3) and MPRED (bit 4) = 1; the listed HDFG{R,W}TR(2)_EL2 and HFGITR_EL2 bits = 1. | MDCR_EL3.SBRBE/EnPM2/EnPMS3 | booting.rst:432-478 |
| MOPS | HCRX_EL2.MSCEn (bit 11) = 1 and MCE2 (bit 10) = 1; the hypervisor must handle MOPS exceptions. | — | booting.rst:480-487 |
| TCR2 / S1PIE | HCRX_EL2.TCR2En (bit 14) = 1; HFGRTR/HFGWTR_EL2.nPIR_EL1 (bit 58) and nPIRE0_EL1 (bit 57) = 1. The doc misspells HFGWTR as "HFGRWR" at line 513. | SCR_EL3.TCR2En/PIEn | booting.rst:489-513 |
| GCS | GCSCR_EL1 = GCSCRE0_EL1 = GCSCR_EL2 = 0; HCRX_EL2.GCSEn = 1; HFGITR_EL2.nGCSEPP (bit 59), nGCSSTR_EL1 (bit 58), nGCSPUSHM_EL1 (bit 57) = 1; HFG{R,W}TR_EL2.nGCS_EL1 (bit 53), nGCS_EL0 (bit 52) = 1. | SCR_EL3.GCSEn | booting.rst:515-545 |
| LS64 / LS64_V | HCRX_EL2.EnALS (bit 1) = 1; EnASR (bit 2) = 1. | — | booting.rst:559-569 |
| Debug, PMU, FIQ, EL2 entry | — | MDCR_EL3.TDA = 0 and TPM = 0; SCR_EL3.FIQ uniform; SCR_EL3.HCE = 1 for EL2 entry | booting.rst:215-224,547-557 |

**Nested (guest entered at EL2): what the guest kernel does itself**

| Item | Behaviour | Source |
|---|---|---|
| EL2 init | `init_el2` sets HCR_EL2 = HCR_HOST_NVHE_FLAGS + HCR_ATA, runs `init_el2_state`, installs `__hyp_stub_vectors`, then ERETs to EL1h. `finalise_el2` later switches to VHE if possible. | head.S:244,286-330 |
| E2H | Chosen from ID_AA64MMFR4_EL1.E2H0. The code notes that Apple ("Fruity") CPUs implement HCR_EL2.E2H as RAO/WI and detects VHE via sysreg remapping. | arch/arm64/include/asm/el2_setup.h:19-49 |
| Timer PPI | VHE uses hyp-virt (DT index 4); if missing, logs FW_BUG and falls back to hyp-phys. EL2 available but no VHE: NS EL1 physical (index 1). | drivers/clocksource/arm_arch_timer.c:1115-1132 |
| GIC maintenance IRQ | The GIC node's `interrupts` property is only needed if the guest itself runs a hypervisor. | bind/interrupt-controller/arm,gic-v3.yaml:89-92 |

### 3.3 DTB placement and early FDT handling

| Rule | Detail | Source |
|---|---|---|
| Alignment and size | "placed on an 8-byte boundary and must not exceed 2 megabytes in size" (`MIN_FDT_ALIGN` 8, `MAX_FDT_SIZE` SZ_2M) | booting.rst:53-54; arch/arm64/include/asm/boot.h:8-13 |
| Attributes | "mapped cacheable using blocks of up to 2 megabytes", so it must not share a 2 MiB region that needs specific attributes | booting.rst:54-56 |
| Early idmap | `map_fdt()` maps [dtb, dtb+2 MiB) as PAGE_KERNEL (Normal, cacheable), cut at `_text` when the Image follows. That window must be RAM, never MMIO. | arch/arm64/kernel/pi/map_kernel.c:200-216 |
| Fixmap checks | Rejects dt_phys == 0, misalignment, bad magic, and totalsize > 2 MiB. The fixmap window is MAX_FDT_SIZE + one page. | arch/arm64/mm/fixmap.c:151-167; arch/arm64/include/asm/fixmap.h:39-46 |
| Failure mode | `pr_crit` "invalid device tree blob … must be 8-byte aligned and must not exceed 2 MB", then spins forever in `cpu_relax()`. No panic; only visible with earlycon. | arch/arm64/kernel/setup.c:183-196 |
| Header validation | `fdt_check_header`: 8-aligned pointer, magic, version ≥ 2, last_comp_version ≤ 17 and ≤ version, totalsize ≥ header, off_mem_rsvmap 8-aligned, off_dt_struct 4-aligned, all blocks within totalsize. | fdt.c:1205-1224; libfdt/fdt.c:89-146; libfdt/libfdt.h:16-18 |
| Self-reservation | The kernel memblock-reserves the DTB, the Image and the initrd; no /memreserve/ entries are needed for them. | arch/arm64/kernel/setup.c:176-177; arch/arm64/mm/init.c:257-290 |
| **Kernel writes the DTB** | Zeroes /chosen/kaslr-seed in place, turns /chosen/rng-seed into FDT_NOP, and deletes linux,dmcryptkeys. The DTB must be in writable guest RAM. | arch/arm64/kernel/pi/kaslr_early.c:31-36; fdt.c:1110-1120,873 |
| No overlap | The DTB must not overlap [Image, Image + image_size). | booting.rst:142-143 |
| Pre-v4.2 | The DTB also had to be within 512 MB starting text_offset below the Image. | booting.rst:58-59 |
| Memory nodes | Only direct children of `/` with device_type "memory" that are available. `linux,usable-memory` overrides reg. Size-0 entries are skipped. `hotpluggable` is honoured. | fdt.c:1036-1088 |
| Root cell counts | If root `#address-cells` or `#size-cells` is missing, Linux WARNs and uses its defaults (1/1 on arm64). DTSpec's default is 2/1. Always set both. | fdt.c:1009-1020; drivers/of/of_private.h:33-39; DTSpec §2.3.5 (p.14) |

### 3.4 DT bindings

**arm,gic-v3** (bind/interrupt-controller/arm,gic-v3.yaml)

| Property | Type | Semantics | Source |
|---|---|---|---|
| compatible | string | "arm,gic-v3" | :22-28 |
| interrupt-controller | empty | | :30 |
| #interrupt-cells | u32, 3 or 4 | Cell 1 is the type: 0 SPI, 1 PPI, 2 ESPI, 3 EPPI. Cell 2 is the number within the type: SPI 0-987 → INTID n+32, PPI 0-15 → INTID n+16, ESPI 0-1023, EPPI 0-63. Cell 3 is flags bits[3:0]: 1 = edge rising, 4 = level high. Cell 4 is a PPI-partition phandle, or 0. | :39-69; drivers/irqchip/irq-gic-v3.c:1604-1632; include/dt-bindings/interrupt-controller/arm-gic.h:13-16; include/dt-bindings/interrupt-controller/irq.h:13-18 |
| Flag constraints | — | SPI and ESPI accept only LEVEL_HIGH (4) or EDGE_RISING (1); anything else is -EINVAL. v3 masks flags with IRQ_TYPE_SENSE_MASK, so the GICv2 CPU-mask byte (x<<8) is ignored. | drivers/irqchip/irq-gic-v3.c:715-718,1632; arm-gic.h:22-23 |
| reg | addr/size pairs | Order: GICD, then one entry per GICR region, then optional GICC, GICH, GICV (only if the CPUs support them). At least 2 entries. | :71-87; drivers/irqchip/irq-gic-v3.c:2212,2235-2243 |
| #redistributor-regions | u32 | Required when there is more than one GICR region. Default 1. | :102-107; irq-gic-v3.c:2226-2227 |
| redistributor-stride | u64 | A multiple of 64 KiB, only with padding pages. Default 0. | :94-100; irq-gic-v3.c:2245-2246 |
| interrupts | 1 specifier | VGIC maintenance interrupt | :89-92 |
| #address-cells/#size-cells/ranges | u32 | 0/1/2 and 1/2. Needed when ITS children have reg. | :32-37 |
| msi-controller + mbi-ranges | empty + `<intid span>` pairs | MBI (MSI without ITS). Each property requires the other. `intid` is absolute. Doorbell = GICD base (or mbi-alias) + GICD_SETSPI_NSR (0x40); data = INTID; edge by default. | :115-128,180-182; drivers/irqchip/irq-gic-v3-mbi.c:58-67,108,148-153,226-273; include/linux/irqchip/arm-gic-v3.h:18-19 |
| mbi-alias | u32 or u64 address | Alias frame exposing only {SET,CLR}SPI | :130-137 |
| ITS child | node `msi-controller@…` | compatible "arm,gic-v3-its"; `msi-controller`; `#msi-cells = <1>` (the cell is the DeviceID); one reg. Without `msi-controller`: "ITS ignored". | :188-234; drivers/irqchip/irq-gic-v3-its.c:5575 |

**arm,armv8-timer** (bind/timer/arm,arch_timer.yaml)

| Property | Semantics | Source |
|---|---|---|
| compatible | "arm,armv8-timer" (optionally followed by "arm,armv7-timer") | :17-28; drivers/clocksource/arm_arch_timer.c:1198-1199 |
| interrupts | **Positional**: [0] secure phys (if EL3 exists), [1] NS EL1 phys, [2] EL1 virt, [3] EL2 phys, [4] EL2 virt (VHE). At least 2 entries. Without interrupt-names, Linux reads by index. | :30-37; arm_arch_timer.c:1152-1161 |
| interrupt-names | "phys", "virt", "hyp-phys", "hyp-virt", optionally preceded by "sec-phys" | :39-53; arm_arch_timer.c:46-52 |
| PPI Linux uses | Guest at EL1 with no EL2: **virt ([2])**. EL2 without VHE: phys ([1]). VHE: hyp-virt ([4]). Chosen entry missing: "No interrupt available, giving up". | arm_arch_timer.c:1115-1132,1183-1186 |
| INTIDs | KVM defaults: phys 30 (PPI 14), virt 27 (PPI 11), EL2 phys 26 (PPI 10), EL2 virt 28 (PPI 12). The binding's example lists PPIs 13, 14, 11, 10. HVF reserves the same 27/30/26 (`hv_gic_types.h:37-50`). | arch/arm64/kvm/arch_timer.c:35-39; :113-120 |
| clock-frequency | Discouraged (broken firmware only). Linux reads CNTFRQ. | :55-59; arm_arch_timer.c:1165-1166 |
| always-on | Absent means the timer is marked C3STOP. | :61-64; arm_arch_timer.c:1168 |
| arm,no-tick-in-suspend | The counter stops in system suspend. | :93-98; arm_arch_timer.c:1189-1190 |

**virtio,mmio** (bind/virtio/mmio.yaml)

| Property | Semantics | Source |
|---|---|---|
| compatible, reg, interrupts | "virtio,mmio", one reg entry, one interrupt; all required | :17-26,40-43 |
| dma-coherent | Allowed. arm64 is **non-coherent by default**: only mips, riscv and powerpc select ARCH_DMA_DEFAULT_COHERENT. | :23; kernel/dma/mapping.c:27; arch/mips/Kconfig:518; arch/riscv/Kconfig:21; arch/powerpc/Kconfig:127; drivers/of/address.c:1007-1021 |
| DMA API use | Virtio bypasses the DMA API unless VIRTIO_F_ACCESS_PLATFORM is negotiated. | drivers/virtio/virtio_ring.c:382-400; include/linux/virtio_config.h:283-290 |
| #iommu-cells / iommus | `#iommu-cells = <1>` on a virtio-iommu node; `iommus` references the endpoint | :28-34 |
| Register window | Magic 0x000, version 0x004, device config at 0x100. reg must cover 0x100 + config size. | include/uapi/linux/virtio_mmio.h:43,46,141 |
| Child node | Optional `compatible = "virtio,device<hex id>"` | bind/virtio/virtio-device.yaml:17-23 |

**arm,pl011 + arm,primecell** (bind/serial/pl011.yaml, bind/arm/primecell.yaml), fixed-clock, and the arm,sbsa-uart alternative

| Item | Semantics | Source |
|---|---|---|
| compatible | "arm,pl011", "arm,primecell" | pl011.yaml:26-30 |
| reg, interrupts | One each, required | pl011.yaml:32-36,107-110 |
| clocks / clock-names | clocks[0] = UARTCLK ("uartclk"), clocks[1] = PCLK ("apb_pclk"). A single clock is deprecated. | pl011.yaml:53-66; primecell.yaml:29-35 |
| **apb_pclk is effectively mandatory** | The AMBA bus does `clk_get(dev,"apb_pclk")`. If missing: -ENOENT, the peripheral-ID read fails, `amba_match` returns **-EPROBE_DEFER forever**. Probe needs the clock again anyway. | drivers/amba/bus.c:63-76,119-123,186-199,273-275; drivers/clk/clkdev.c:72-75,100-112 |
| uartclk | `devm_clk_get(dev, NULL)` takes the first clock; probe fails without it. Its rate becomes port.uartclk (max_baud = uartclk/16). | pl011.c:1849,2166-2171,3019-3021 |
| PrimeCell ID read | Low byte of 32-bit words: PID0-3 at size-0x20+4i, CID0-3 at size-0x10+4i. CID must be **0xB105F00D**. For a 0x1000 window the IDs are at 0xFE0-0xFFC. `arm,primecell-periphid` (u32) overrides the PID, but apb_pclk is still required at probe. | drivers/amba/bus.c:145-167; include/linux/amba/bus.h:22; primecell.yaml:26-28; drivers/of/platform.c:237-238 |
| Driver match / FIFO | ID 0x00041011 with mask 0x000fffff. FIFO depth 16 if revision (PID bits 23:20) < 3, otherwise 32. | pl011.c:123-126,3199-3204; include/linux/amba/bus.h:144-147 |
| Access widths | The main driver uses 16-bit readw/writew unless `reg-io-width = <4>`. Earlycon writes DR with 8-bit stores and reads FR with 32-bit loads. The ID probe uses 32-bit reads. | pl011.c:351-369,3026,3033-3046,2736-2746; pl011.yaml:101-105; drivers/amba/bus.c:149-152 |
| earlycon | `OF_EARLYCON_DECLARE(pl011,"arm,pl011")`. A bare `earlycon` uses /chosen/stdout-path. | pl011.c:2823; drivers/tty/serial/earlycon.c:239-240; Documentation/admin-guide/kernel-parameters.txt:1397-1402 |
| fixed-clock | compatible "fixed-clock", `#clock-cells = <0>`, clock-frequency (u32); all required | bind/clock/fixed-clock.yaml:23-29,38-41; drivers/clk/clk-fixed-rate.c:168 |
| arm,sbsa-uart | Plain platform device: no clocks, no PrimeCell ID. Requires compatible, reg, interrupts and `current-speed` (u32; probe fails without it). 32-bit accesses, fixed baud, 32-byte FIFO, own earlycon. | bind/serial/arm,sbsa-uart.yaml:11-36; pl011.c:143-155,2841,3109-3164 |

**arm,pl031** (bind/rtc/arm,pl031.yaml)

| Item | Semantics | Source |
|---|---|---|
| compatible | "arm,pl031", "arm,primecell" | :24-27 |
| required | compatible, reg, clocks, clock-names ("apb_pclk" in the example). `interrupts` is optional. | :29-47,56-57 |
| No IRQ | The alarm feature is cleared | drivers/rtc/rtc-pl031.c:360-361,371-376 |
| Match | ID 0x00041031 with mask 0x000fffff. Same CID and apb_pclk rules as PL011. | drivers/rtc/rtc-pl031.c:435-440 |

**arm,psci and cpus** (bind/arm/psci.yaml, bind/arm/cpus.yaml)

| Item | Semantics | Source |
|---|---|---|
| Node | Named `psci`; Linux finds it by compatible | psci.yaml:35-36; drivers/firmware/psci/psci.c:815 |
| compatible | Prefix-truncatable lists: "arm,psci-1.0", "arm,psci-0.2", "arm,psci"; or "arm,psci-0.2", "arm,psci"; or "arm,psci" (v0.1). "arm,psci" requires u32 `cpu_on` and `cpu_off` IDs, with optional `cpu_suspend` and `migrate`. | psci.yaml:38-63,73-87,130-139; psci.c:801-806 |
| method | "hvc" or "smc". Missing gives -ENXIO; any other value -EINVAL. Uses immediate #0, function ID in x0, parameters in x1-x3. | psci.yaml:21-29,65-71; psci.c:287-307 |
| /cpus | #address-cells 1 or 2; #size-cells 0 | DTSpec §3.7 Table 3.8 (p.34) |
| cpu@N reg | MPIDR_EL1 affinity. 1 cell: bits[23:0] = MPIDR[23:0]. 2 cells: cell0[7:0] = MPIDR[39:32], cell1[23:0] = MPIDR[23:0]. Other bits 0. | cpus.yaml:64-79 |
| cpu@N other | device_type "cpu", reg and compatible are required ("arm,armv8" is allowed "Only for s/w models"). enable-method "psci" or "spin-table" is required on arm64. status "fail" is skipped; "disabled" is still enumerated. | cpus.yaml:128,240-246,514-517; drivers/of/base.c:823-852 |

**memory and chosen**

| Node / property | Type | Semantics | Source |
|---|---|---|---|
| /memory@addr device_type | string | "memory" | DTSpec §3.4 Table 3.3 (p.29) |
| /memory reg | (addr,size) pairs | Sized by root cells. At least one memory node required. | DTSpec §3.1 (p.26), §3.4 (p.28-29); fdt.c:1036-1088 |
| /memory hotpluggable | empty | Hint | DTSpec Table 3.3 (p.29); fdt.c:1061,1079-1084 |
| UEFI boot | — | Memory nodes ignored; the EFI stub deletes them | DTSpec §3.4.1 (p.29); Documentation/arch/arm/uefi.rst:46-47 |
| bootargs | string | Copied up to COMMAND_LINE_SIZE = 2048 on arm64. CONFIG_CMDLINE extend/force rules apply. | DTSpec §3.6 (p.33); fdt.c:1122-1144; arch/arm64/include/uapi/asm/setup.h:25 |
| stdout-path | string "path[:opts]" (alias allowed) | Falls back to linux,stdout-path. Selects the preferred console, and the earlycon when `earlycon` is given without options. | DTSpec §3.6 (p.33-34); drivers/of/base.c:1936-1947; fdt.c:949-994 |
| linux,initrd-start / linux,initrd-end | u32 or u64 (cells = len/4) | End exclusive. Ignored if start > end. Needs CONFIG_BLK_DEV_INITRD. The example in usage-model.rst wrongly omits "linux,". | fdt.c:806-834; Documentation/devicetree/usage-model.rst:193-202 |
| rng-seed | bytes (len > 0) | Passed to `add_bootloader_randomness`, then overwritten with FDT_NOP in place | fdt.c:1110-1120 |
| kaslr-seed | exactly 8 bytes (u64) | Other lengths ignored. Zeroed in place. Used only with RANDOMIZE_BASE and without `nokaslr`; otherwise falls back to RNDR. | arch/arm64/kernel/pi/kaslr_early.c:21-52; arch/arm64/kernel/pi/map_kernel.c:275-282; Documentation/arch/arm/uefi.rst:68 |
| linux,elfcorehdr / linux,usable-memory-range / linux,dmcryptkeys / linux,kho-* | addr/size | Crash dump and kexec only. usable-memory-range takes at most 2 ranges. | fdt.c:841-945 |
| linux,uefi-system-table, linux,uefi-mmap-start (64-bit); linux,uefi-mmap-size/-desc-size/-desc-ver (32-bit) | u32/u64 | Written by the EFI stub. Don't emit for direct boot. | Documentation/arch/arm/uefi.rst:49-72; drivers/firmware/efi/fdtparams.c:49-56,71 |
| linux,pci-probe-only | u32 | Non-zero keeps firmware BARs. Looked up on the host-bridge node first, then /chosen. | drivers/pci/of.c:255-285 |
| root compatible | string | e.g. "linux,dummy-virt" (QEMU virt binding) | bind/arm/linux,dummy-virt.yaml:12-16; DTSpec §3.2 Table 3.1 (p.27) |

**pci-host-ecam-generic** (bind/pci/host-generic-pci.yaml)

| Property | Semantics | Source |
|---|---|---|
| compatible | "pci-host-ecam-generic" (or "pci-host-cam-generic") | :84-88; drivers/pci/controller/pci-host-generic.c:62-65 |
| device_type | "pci". Linux applies PCI address translation only for device_type pci/pciex/vci/ht, or a node named "pcie" (with a warning). | drivers/of/address.c:137-160 |
| #address-cells / #size-cells | 3 / 2 | :157-159 (example) |
| reg | ECAM window. The base corresponds to the first bus in bus-range. offset = (bus<<20) + (dev<<15) + (fn<<12) + reg, so **1 MiB per bus**. A window too small for bus-range reduces the usable bus range. | :31-34,90-98; include/linux/pci-ecam.h:23; drivers/pci/ecam.c:55-62 |
| bus-range | `<start end>`. Default 0-0xff; values above 0xff are capped. | drivers/pci/of.c:341-352 |
| ranges (required) | PCI address (3 cells), CPU address (parent #address-cells), size (2 cells). Must include non-prefetchable MEM. In phys.hi, bits 25:24 are 01 = I/O, 10 = MEM32, 11 = MEM64; bit 30 = prefetchable. | :100-104,117-120,162-167; drivers/of/address.c:108-131; drivers/pci/of.c:356-406 |
| Child unit address | phys.hi = (bus<<16) + (dev<<11) + (fn<<8) | DTSpec §2.4.4 (p.22) |
| #interrupt-cells, interrupt-map, interrupt-map-mask | #interrupt-cells = 1 (INTA=1..INTD=4). interrupt-map row = child unit address (3) + child pin (1) + parent phandle + **parent unit address (parent's #address-cells; 0 if the parent has none)** + parent specifier (parent's #interrupt-cells). Typical mask `<0xf800 0 0 7>`. | DTSpec §2.4.3 (p.20-21), §2.4.4 (p.21-23); drivers/of/irq.c:108-158 (134-136); :169-178 |
| msi-map / msi-map-mask / msi-parent | `msi-map = <rid-base &ctrl msi-base length>`. Output = rid − rid-base + msi-base (for an ITS, the DeviceID). Mask default 0xffffffff. No match: the ID passes through unchanged. RID = (bus<<8) \| devfn, after DMA aliasing. | drivers/of/base.c:2178-2319; drivers/of/irq.c:806-840; drivers/pci/msi/irqdomain.c:364-376; include/linux/pci.h:72 |
| iommu-map / iommu-map-mask | Same format as msi-map, with #iommu-cells | drivers/of/base.c:2323-2342; :107-108 |
| dma-coherent | Allowed. Needed for coherent passthrough and virtio DMA, since arm64 defaults to non-coherent. | :106 |
| linux,pci-domain | u32 | drivers/pci/of.c:218-234 |

### 3.5 FDT binary format (DTSpec v0.4 ch.5, cross-checked with libfdt)

Header fields are big-endian u32. The v17 header is 40 bytes (libfdt/fdt.h:12-29,60-64).

| Off | Field | Rule | Source |
|---|---|---|---|
| 0x00 | magic | 0xd00dfeed | DTSpec §5.2 (p.51); libfdt/fdt.h:50 |
| 0x04 | totalsize | Whole blob, including gaps and free space | DTSpec §5.2 (p.51) |
| 0x08 | off_dt_struct | Offset from the header start | DTSpec §5.2 (p.51) |
| 0x0C | off_dt_strings | Offset from the header start | DTSpec §5.2 (p.51) |
| 0x10 | off_mem_rsvmap | Offset from the header start | DTSpec §5.2 (p.51) |
| 0x14 | version | 17 | DTSpec §5.1-5.2 (p.51) |
| 0x18 | last_comp_version | 16 | DTSpec §5.2 (p.51-52) |
| 0x1C | boot_cpuid_phys | Should equal the boot CPU node's reg. arm64 Linux never reads it (3.2). | DTSpec §5.2 (p.52) |
| 0x20 | size_dt_strings | bytes | DTSpec §5.2 (p.52) |
| 0x24 | size_dt_struct | bytes | DTSpec §5.2 (p.52) |

| Block / token | Encoding and rules | Source |
|---|---|---|
| Order | Header, mem-rsvmap, struct, strings ("should"). Free space between blocks is optional. | DTSpec ch.5 intro (p.50) |
| Memory reservation block | Pairs of BE u64 {address, size}, non-overlapping, terminated by {0,0}. The block is 8-byte aligned. | DTSpec §5.3.2 (p.53); libfdt/fdt.h:31-34 |
| FDT_BEGIN_NODE | 0x00000001, then the NUL-terminated unit name (including @unit-address), zero-padded to 4 bytes. The root's name is "". | DTSpec §5.4.1 (p.53); libfdt/fdt.h:53; scripts/dtc/dtc-parser.y:160-164; scripts/dtc/flattree.c:247-254 |
| FDT_END_NODE | 0x00000002, no data. The next token is anything except FDT_PROP. | DTSpec §5.4.1 (p.53); libfdt/fdt.h:54 |
| FDT_PROP | 0x00000003, then u32 len, u32 nameoff (into the strings block), then len bytes of value, zero-padded to 4 | DTSpec §5.4.1 (p.53-54); libfdt/fdt.h:41-46,55 |
| FDT_NOP | 0x00000004; ignored (used to erase in place) | DTSpec §5.4.1 (p.54); libfdt/fdt.h:57 |
| FDT_END | 0x00000009. Exactly one, last. Its end offset equals size_dt_struct. | DTSpec §5.4.1 (p.54); libfdt/fdt.h:58 |
| Tree rule | Node = BEGIN_NODE, its properties, its child nodes, END_NODE. All properties precede subnodes. FDT_END follows the root. | DTSpec §5.4.2 (p.54) |
| Token alignment | Every token 4-byte aligned, padded with 0x00 | DTSpec §5.4, §5.4.1 (p.53); libfdt/libfdt_internal.h:10-11 |
| Strings block | Concatenated NUL-terminated names; no alignment requirement | DTSpec §5.5 (p.55) |
| Blob alignment | Load at an 8-byte-aligned address. Rsvmap 8-aligned, struct 4-aligned. | DTSpec §5.6 (p.55) |

| Encoding / naming rule | Detail | Source |
|---|---|---|
| Value types | `<empty>`; `<u32>` BE; `<u64>` as two cells, high first; `<string>` NUL-terminated; `<stringlist>` concatenated; `<phandle>` u32; `<prop-encoded-array>` per property | DTSpec §2.2.4 Table 2.3 (p.10-11) |
| Node names | `node-name@unit-address`. Name is 1-31 characters of [0-9a-zA-Z,._+-] and starts with a letter. The unit-address equals the first reg address; omit @ when there is no reg. | DTSpec §2.2.1 (p.7-8) |
| Property names | 1-31 characters of [0-9a-zA-Z,._+?#-] | DTSpec §2.2.4 Table 2.2 (p.9-10) |
| compatible | stringlist, most specific first | DTSpec §2.3.1 (p.12) |
| model | string | DTSpec §2.3.2 (p.12) |
| phandle | Unique u32 (`linux,phandle` is legacy). A hand-built FDT must emit phandles itself. | DTSpec §2.3.3 (p.13) |
| status | Absent = "okay". Also "disabled", "reserved", "fail", "fail-sss". | DTSpec §2.3.4 (p.13-14) |
| #address-cells / #size-cells | u32, **not inherited**, required on any node with children. Spec default 2/1; Linux root default 1/1 (3.3). | DTSpec §2.3.5 (p.14) |
| reg | (address, length) pairs sized by the parent's cells. Length is omitted when the parent's #size-cells = 0. | DTSpec §2.3.6 (p.15) |
| ranges | (child addr, parent addr, length). Empty = identity; absent = no translation. | DTSpec §2.3.8 (p.15-16) |
| dma-ranges | Same triplet format, for DMA | DTSpec §2.3.9 (p.16) |
| dma-coherent / dma-noncoherent | empty | DTSpec §2.3.10-2.3.11 (p.17) |
| name, device_type | Deprecated; device_type only on cpu and memory nodes. Linux still keys PCI translation on device_type "pci". | DTSpec §2.3.12-2.3.13 (p.17); drivers/of/address.c:157-159 |
| interrupts / interrupt-parent / interrupts-extended | Specifier size comes from the domain root's #interrupt-cells. The parent defaults to the DT parent, and Linux walks up to a node with #interrupt-cells, so `interrupt-parent` on `/` covers all devices. interrupts-extended = `<phandle spec>…` and takes precedence. | DTSpec §2.4.1 (p.19-20); drivers/of/irq.c:61-83 |
| #interrupt-cells / interrupt-controller | u32 / empty | DTSpec §2.4.2 (p.20) |
| interrupt-map / interrupt-map-mask | See the PCI table. If no unit address is needed, #address-cells must be explicitly 0. | DTSpec §2.4.3 (p.20-21) |

### Section 3 bug-magnets

- **PrimeCell clocks.** A PL011 or PL031 without a clock named "apb_pclk" never probes and logs nothing: `amba_match` returns -EPROBE_DEFER forever. PL011 also needs "uartclk" as clocks[0] with a non-zero rate (a fixed-clock). The alternative is "arm,sbsa-uart" with `current-speed`.
- **GIC interrupt cells.** An SPI cell holds INTID−32 and a PPI cell holds INTID−16. SPI flags must be 1 or 4. The timer `interrupts` list is positional: without interrupt-names, an EL1 guest needs ≥3 entries so that [2] is the virtual timer (PPI 11 = INTID 27).
- **PCI interrupt-map.** Rows need the parent's #address-cells worth of unit-address cells. A GIC with `#address-cells = <2>` (for ITS children) needs 2 extra zero cells per INTx row, or the map is silently misparsed.
- **DTB placement.** The DTB must be 8-byte aligned and ≤ 2 MiB, in **writable** RAM (the kernel rewrites the seeds), followed by 2 MiB that is entirely RAM (cacheable early idmap), and outside [Image, Image+image_size). Violations produce a silent infinite loop.
- **Kernel footprint.** Reserve image_size bytes, not the file size (BSS, page tables and early stack follow). Load address 2 MiB-aligned (text_offset = 0).
- **Boot CPU and secondaries.** The boot vCPU's MPIDR_EL1 & 0xff00ffffff must equal some cpu node's `reg`. Secondaries need `enable-method = "psci"`. PSCI CPU_ON arrives with target = that reg and context_id = 0.
- **Root cells and memory nodes.** Set root #address-cells and #size-cells explicitly (Linux assumes 1/1, DTSpec says 2/1). Memory nodes must be direct children of `/` with device_type "memory".
- **PL011 access widths.** Emulation must accept 8-bit (earlycon DR), 16-bit (main driver) and 32-bit (ID and FR) accesses. arm64 DMA is non-coherent unless `dma-coherent` is set, which matters for VFIO and for virtio with ACCESS_PLATFORM.

## 4. KVM arm64 API (+ x86_64 headline)

Path aliases for this section (all under the Linux tree): `api` = Documentation/virt/kvm/api.rst; `ukvm` = include/uapi/linux/kvm.h; `akvm` = arch/arm64/include/uapi/asm/kvm.h; `v3` = Documentation/virt/kvm/devices/arm-vgic-v3.rst; `its` = Documentation/virt/kvm/devices/arm-vgic-its.rst; `vcpu` = Documentation/virt/kvm/devices/vcpu.rst; `vm` = Documentation/virt/kvm/devices/vm.rst; `fw` = Documentation/virt/kvm/arm/fw-pseudo-registers.rst; `feat` = Documentation/virt/kvm/arm/vcpu-features.rst; `k/` = arch/arm64/kvm/; `kmain` = virt/kvm/kvm_main.c.

`PROBE` marks a value that was computed rather than quoted. It came from compiling the 7.2-rc4 uapi headers in a scratch C probe (clang, arm64 LP64; never run on a KVM host) and cross-checking against hand-derivation from `_IOC` (include/uapi/asm-generic/ioctl.h:23-66) and the documented example `PC = 0x6030000000100040` (api:2592).

### 4.1 VM creation and capability probing

| Item | Value / encoding | Semantics | Source |
|---|---|---|---|
| fd model | `open("/dev/kvm")` → `KVM_CREATE_VM` → VM fd → `KVM_CREATE_VCPU` / `KVM_CREATE_DEVICE` fds | Ioctl classes: system, VM, vCPU, device | api:10-30 |
| Thread / process rules | VM and device ioctls must come from the creating process. vCPU ioctls "should be issued from the same thread that was used to create the vcpu" | Unlike HVF, this is a performance hint, not a hard rule. Sharing fds via fork() or SCM_RIGHTS is unsupported. | api:31-47, 63-68 |
| `KVM_GET_API_VERSION` | `_IO(0xAE,0x00)`, returns 12 | Refuse to run on any other value | api:132-146; ukvm:22, 715 |
| `KVM_CREATE_VM` | 0xAE01; argument = machine type | The new VM has no vCPUs and no memory | api:149-159; ukvm:716 |
| arm64 type bits [7:0] | `KVM_VM_TYPE_ARM_IPA_SIZE_MASK` = 0xff; `KVM_VM_TYPE_ARM_IPA_SIZE(x)` | 0 means the default 40-bit IPA. N requires 32 ≤ N ≤ host IPA limit. | api:181-210; ukvm:698-706 |
| Type 0 on a small-IPA host | EINVAL ("using unsupported default IPA limit, upgrade your VMM") when the host limit is below 40 | Always pass an explicit IPA size | k/mmu.c:905-925; `KVM_PHYS_SHIFT` 40 at arch/arm64/include/asm/kvm_mmu.h:149; `ARM64_MIN_PARANGE_BITS` 32 at arch/arm64/include/asm/sysreg.h:893 |
| IPA size vs guest PARange | Setting the IPA size does **not** change the guest's `ID_AA64MMFR0_EL1.PARange`. It only affects stage 2. | | api:212-215 |
| `KVM_VM_TYPE_ARM_PROTECTED` | 1<<31 | EINVAL unless pKVM is enabled | ukvm:708-710; k/arm.c:242-253 |
| `KVM_CHECK_EXTENSION` | 0xAE03 | Some caps return values, not 0/1. Prefer the VM fd. | api:262-279; ukvm:724 |
| Host IPA limit | `KVM_CAP_ARM_VM_IPA_SIZE` returns `get_kvm_ipa_limit()` | | k/arm.c:460-461 |
| vCPU limits | `NR_VCPUS` = min(online CPUs, max). `MAX_VCPUS` = `MAX_VCPU_ID` = `kvm->max_vcpus`. `KVM_MAX_VCPUS` = `VGIC_V3_MAX_CPUS` = 512. Creating a vGICv3 sets max_vcpus to 512 (vGICv2: 8). | vcpu id range is [0, max_vcpu_id) | k/arm.c:403-419; arch/arm64/include/asm/kvm_host.h:40; include/kvm/arm_vgic.h:25-26; k/vgic/vgic-init.c:132-143; api:316-317 |
| Address spaces | `KVM_MAX_NR_ADDRESS_SPACES` = 1 (arm64 doesn't override) | Slot as_id must be 0 | include/linux/kvm_host.h:84-85; kmain:4898-4903 |

**Capabilities to probe.** Numbers are from ukvm:736-999. arm64 behaviour is from `kvm_vm_ioctl_check_extension` (k/arm.c:364-490) and the generic handler (kmain:4864-4938).

| Cap | # | arm64 behaviour |
|---|---|---|
| IRQCHIP | 0 | = vgic present (k/arm.c:372-373) |
| USER_MEMORY / USER_MEMORY2 | 3 / 231 | 1 (kmain:4868-4869) |
| NR_VCPUS / MAX_VCPUS / MAX_VCPU_ID | 9 / 66 / 128 | see 4.1 |
| NR_MEMSLOTS | 10 | `KVM_USER_MEM_SLOTS` (kmain:4904-4905) |
| MP_STATE | 14 | 1 |
| COALESCED_MMIO | 15 | returns page offset 1 (kmain:4885-4886) |
| IRQ_ROUTING | 25 | `KVM_MAX_IRQ_ROUTES` (kmain:4894-4896) |
| IRQFD / IRQFD_RESAMPLE | 32 / 82 | 1 |
| IOEVENTFD / IOEVENTFD_ANY_LENGTH | 36 / 122 | 1 |
| VCPU_EVENTS | 41 | 1 |
| ONE_REG | 70 | 1 |
| SIGNAL_MSI | 77 | 1 |
| READONLY_MEM | 81 | 1 |
| ARM_PSCI / ARM_PSCI_0_2 | 87 / 102 | 1 |
| DEVICE_CTRL | 89 | 1 |
| ARM_EL1_32BIT | 93 | host-dependent |
| CHECK_EXTENSION_VM | 105 | 1 |
| ARM_PMU_V3 | 126 | `kvm_supports_guest_pmuv3()` |
| VCPU_ATTRIBUTES | 127 | 1 |
| MSI_DEVID | 131 | VM fd only; returns `vgic.msis_require_devid` (k/arm.c:420-425) |
| IMMEDIATE_EXIT | 136 | 1 |
| ARM_USER_IRQ | 144 | 1 (userspace irqchip only) |
| ARM_INJECT_SERROR_ESR | 158 | requires RAS |
| ARM_VM_IPA_SIZE | 165 | host IPA limit |
| MANUAL_DIRTY_LOG_PROTECT2 | 168 | flag mask (kmain:4890-4893) |
| ARM_SVE | 170 | host-dependent |
| ARM_PTRAUTH_ADDRESS / _GENERIC | 171 / 172 | host-dependent |
| ARM_IRQ_LINE_LAYOUT_2 | 174 | 1 |
| ARM_NISV_TO_USER | 177 | 1; enable via `KVM_ENABLE_CAP` on the VM (k/arm.c:147-151) |
| ARM_INJECT_EXT_DABT | 178 | 1 |
| STEAL_TIME | 187 | pvtime |
| DIRTY_LOG_RING | 192 | **0 on arm64** (TSO-only; kmain:4906-4911) |
| PTP_KVM | 198 | 1 |
| ARM_MTE | 205 | enable before any vCPU exists (k/arm.c:152-159) |
| SYSTEM_EVENT_DATA | 215 | 1 |
| ARM_SYSTEM_SUSPEND | 216 | enable-cap (k/arm.c:160-163) |
| DIRTY_LOG_RING_ACQ_REL | 223 | max ring bytes (kmain:4912-4917) |
| DIRTY_LOG_RING_WITH_BITMAP | 225 | 1 (k/Kconfig:30) |
| COUNTER_OFFSET | 227 | 1 |
| ARM_EAGER_SPLIT_CHUNK_SIZE / ARM_SUPPORTED_BLOCK_SIZES | 228 / 229 | — |
| ARM_SUPPORTED_REG_MASK_RANGES | 230 | BIT(0) (k/arm.c:479-480) |
| MEMORY_FAULT_INFO | 232 | — |
| MEMORY_ATTRIBUTES | 233 | **not on arm64**. `KVM_GENERIC_MEMORY_ATTRIBUTES` is selected only in arch/x86/kvm/Kconfig:87,138,162. |
| GUEST_MEMFD / GUEST_MEMFD_FLAGS | 234 / 244 | 1 (k/Kconfig:39). MMAP is always available; INIT_SHARED depends on the arch (include/linux/kvm_host.h:735-743). |
| ARM_WRITABLE_IMP_ID_REGS | 239 | enable before any vCPU exists (k/arm.c:180-187) |
| ARM_EL2 / ARM_EL2_E2H0 | 240 / 241 | nested virtualization (k/arm.c:442-447) |
| ARM_CACHEABLE_PFNMAP_SUPPORTED | 243 | requires FWB |
| ARM_SEA_TO_USER | 245 | enable-cap |

### 4.2 vCPU creation and init

| Item | Value / encoding | Semantics | Source |
|---|---|---|---|
| `KVM_CREATE_VCPU` | 0xAE41; argument = vcpu id | Returns the vCPU fd | api:307-317; ukvm:1263 |
| `KVM_GET_VCPU_MMAP_SIZE` | 0xAE04 | arm64: 2×PAGE_SIZE (kvm_run plus the coalesced-MMIO page; no PIO page) | api:281-304; kmain:5540-5550 |
| `KVM_ARM_PREFERRED_TARGET` | VM ioctl 0x8020AEAF (PROBE); fills `struct kvm_vcpu_init` | target = `KVM_ARM_TARGET_GENERIC_V8` (5) | api:3574-3600; k/arm.c:2029-2032; akvm:62-70 |
| `KVM_ARM_VCPU_INIT` | 0x4020AEAE (PROBE); `struct kvm_vcpu_init { u32 target; u32 features[7]; }` | Other targets give EINVAL. Any bit set in features[1..6] gives ENOENT. KVM_RUN before INIT gives ENOEXEC. | akvm:110-113; k/arm.c:1583-1598, 1680-1701; api:3464-3483 |
| bit 0 `KVM_ARM_VCPU_POWER_OFF` | | Starts the vCPU stopped, for secondaries woken by PSCI CPU_ON. **Per-call only**: it is stripped before the VM-wide feature comparison. | akvm:100; api:3508-3510; k/arm.c:1709-1717, 1744-1751 |
| bit 1 `EL1_32BIT` | | AArch32 EL1. Incompatible with MTE and NV. | akvm:101; k/arm.c:1608-1617 |
| bit 2 `PSCI_0_2` | | In-kernel PSCI ≥0.2, reported as 1.3 (latest) unless overridden. Without the bit: PSCI 0.1 with KVM-specific IDs. | akvm:102; include/kvm/arm_psci.h:13-38 |
| bit 3 `PMU_V3` | | Emulated PMUv3 | akvm:103; api:3516-3517 |
| bit 4 `SVE` | | Requires `KVM_ARM_VCPU_FINALIZE(KVM_ARM_VCPU_SVE)` (0x4004AEC2) before KVM_RUN or GET_REG_LIST, else EPERM. SVE_VLS is writable only between INIT and FINALIZE. | akvm:104; api:3535-3561, 5142-5182 |
| bits 5/6 `PTRAUTH_ADDRESS` / `PTRAUTH_GENERIC` | | Set both or neither, else EINVAL | akvm:105-106; k/arm.c:1604-1606 |
| bits 7/8 `HAS_EL2` / `HAS_EL2_E2H0` | | NV; the vCPU boots at EL2. E2H0 requires HAS_EL2. | akvm:107-108; api:3563-3572 |
| Feature consistency | The first INIT fixes a **VM-wide** feature set. A later INIT with a different set gives EINVAL. | So PSCI_0_2 and every other feature is all-or-none | k/arm.c:1650-1678, 1693-1697; api:3501-3504 |
| Ordering | "all vcpus should be created before this ioctl is invoked" | | api:3498-3499 |
| Re-INIT = reset | Allowed after the vCPU has run. It unmaps stage-2 (without FWB) or invalidates the I-cache. | | api:3501-3503; k/arm.c:1723-1737 |
| Reset state | PSTATE = EL1h\|A\|I\|F\|D = **0x3C5** (0x3C9 EL2h with NV). GPRs, PC, SP, FP/SIMD and SVE are 0. Sysregs take warm-reset values. | The VMM then sets PC and x0 (FDT address) via SET_ONE_REG | api:3485-3496; k/reset.c:40-44, 213-230; arch/arm64/include/uapi/asm/ptrace.h:34-48 |
| Default MPIDR | Aff0 = id&0xF; Aff1 = (id>>4)&0xFF; Aff2 = (id>>12)&0xFF; bit31 = 1 | Writable via SET_ONE_REG | k/sys_regs.c:977-995, 3233, 5514-5518 |
| vCPU attr `KVM_ARM_VCPU_PMU_V3_CTRL` (group 0) | IRQ 0, INIT 1, FILTER 2, SET_PMU 3, SET_NR_COUNTERS 4 | The PMU IRQ is a PPI with the same number on all vCPUs, or distinct SPIs. INIT must follow vGIC init. | akvm:434-439; vcpu:13-166 |
| vCPU attr `KVM_ARM_VCPU_TIMER_CTRL` (group 1) | IRQ_VTIMER 0, IRQ_PTIMER 1, IRQ_HVTIMER 2, IRQ_HPTIMER 3 | Defaults vtimer **27**, ptimer **30**, hvtimer 28, hptimer 26. Must be PPIs (16-31). Setting on one vCPU applies to all; EBUSY after any vCPU has run. | akvm:440-444; vcpu:168-202; k/arch_timer.c:35-39 |
| vCPU attr `KVM_ARM_VCPU_PVTIME_CTRL` (group 2) | IPA 0 | Stolen-time structure base: 64-byte aligned, in guest RAM | akvm:445-446; vcpu:206-227 |

### 4.3 Memory

| Item | Layout / value | Semantics | Source |
|---|---|---|---|
| `struct kvm_userspace_memory_region` (32 B) | `u32 slot; u32 flags; u64 guest_phys_addr; u64 memory_size; u64 userspace_addr` | `KVM_SET_USER_MEMORY_REGION` = 0x4020AE46 (PROBE) | ukvm:30-36, 1267-1268 |
| `…_region2` (160 B) | above + `u64 guest_memfd_offset; u32 guest_memfd; u32 pad1; u64 pad2[14]` | `KVM_SET_USER_MEMORY_REGION2` = 0x40A0AE49 (PROBE) | ukvm:39-49, 1271-1272; api:6325-6368 |
| flags | LOG_DIRTY_PAGES 1<<0, READONLY 1<<1, GUEST_MEMFD 1<<2 | Only bits 0-15 are user-visible | ukvm:51-58 |
| slot field | bits 0-15 slot id; bits 16-31 as_id (0 on arm64) | | api:1381-1392; kmain:2011-2012 |
| Alignment | memory_size, guest_phys_addr and userspace_addr must be multiples of the **host kernel PAGE_SIZE** (4K, 16K or 64K). userspace_addr must be untagged. No overlap or wrap. | EINVAL otherwise | kmain:2014-2033 |
| Delete / modify | memory_size = 0 deletes. A slot can be moved or have its flags changed, but not resized. | | api:1394-1396 |
| Huge-page hint | Keep the low 21 bits of the GPA and the userspace address identical | | api:1409-1411 |
| READONLY slot | Guest writes become KVM_EXIT_MMIO. arm64 page-table-walker writes (A/D updates) inject an abort. | | api:1413-1430 |
| guest_memfd limits | A GUEST_MEMFD slot cannot also be LOG_DIRTY_PAGES or READONLY | So it can't be dirty-tracked for snapshots | kmain:1573-1598 |
| `KVM_CREATE_GUEST_MEMFD` | 0xC040AED4 (PROBE); `{u64 size; u64 flags; u64 reserved[6]}`; flags MMAP 1<<0, INIT_SHARED 1<<1 | size must be page-aligned. The doc header says "Architectures: none", but arm64 selects KVM_GUEST_MEMFD. | ukvm:1654-1662; api:6412-6471; virt/kvm/guest_memfd.c:634-640; k/Kconfig:39 |
| `KVM_SET_MEMORY_ATTRIBUTES` (PRIVATE 1<<3) | x86 only | not available on arm64 in 7.2-rc4 | api:6376-6410; ukvm:1643-1652 |
| `KVM_PRE_FAULT_MEMORY` | vCPU ioctl 0xC040AED5 (PROBE) | Populates stage 2 as if the guest read-faulted. Doc says "Architectures: none". | api:6473-6532 |
| MTE tags | `KVM_ARM_MTE_COPY_TAGS` (0x8030AEB4) | requires KVM_CAP_ARM_MTE (pointer only) | api:5831-5867 |

### 4.4 vGICv3 / ITS (`KVM_CREATE_DEVICE` 0xC00CAEE0; `KVM_SET/GET/HAS_DEVICE_ATTR` 0x4018AEE1/E2/E3)

| Item | Value / encoding | Rule | Source |
|---|---|---|---|
| Device type | `KVM_DEV_TYPE_ARM_VGIC_V3` = **7**; ITS = 8; VFIO = 4; PV_TIME = 10; VGIC_V5 = 16 | One vGIC per VM; v2 and v3 can't be mixed | ukvm:1206-1242; v3:8-14 |
| `KVM_CREATE_IRQCHIP` on arm64 | creates a **GICv2** | Use KVM_CREATE_DEVICE for v3 | api:860-862 |
| When creatable | Before any vCPU has run, and not during vCPU creation | Sets max_vcpus = 512 | k/vgic/vgic-init.c:118-143 |
| Attr groups | ADDR 0, DIST_REGS 1, CPU_REGS 2 (v2), NR_IRQS 3, CTRL 4, REDIST_REGS 5, CPU_SYSREGS 6, LEVEL_INFO 7, ITS_REGS 8, MAINT_IRQ 9 | | akvm:402-419 |
| ADDR types | V3_ADDR_TYPE_DIST 2, REDIST 3, ITS 4, REDIST_REGION 5 | DIST 64 KiB; REDIST 2×64 KiB per vCPU, contiguous; ITS 128 KiB. **All bases 64 KiB aligned.** | akvm:90-98; v3:24-34; its:25-28 |
| REDIST_REGION value | [63:52] count, [51:16] base[51:16], [15:12] flags (0), [11:0] index | Register in index order from 0. The sum of counts must be at least the number of vCPUs. Don't mix with REDIST. | v3:36-61; k/vgic/vgic.h:108-113 |
| vCPU ↔ redistributor | Assigned by vCPU creation order and region order | Must be reproduced exactly on restore | v3:63-68 |
| DIST_REGS / REDIST_REGS attr | mpidr (Aff3.Aff2.Aff1.Aff0) in [63:32] \| offset [31:0]; data is `__u32` | 64-bit registers are accessed as two 32-bit halves. Writes to RO registers are ignored. | v3:87-124 |
| Before `CTRL_INIT` | Only GICD_IIDR and GICD_TYPER2 are accessible; everything else gives EBUSY | Write back GICD_IIDR (read, then write) **before any other register** | v3:126-143; k/vgic/vgic-kvm-device.c:515-527, 578-581 |
| Pending semantics | ISPENDR/ISPENDR0 read and write the **latch**. ICPENDR is RAZ/WI. STATUSR writes set the value. | Full pending state = latch + line level (use LEVEL_INFO) | v3:146-183 |
| CPU_SYSREGS attr | mpidr [63:32]; [15:0] = Op0[15:14] Op1[13:11] CRn[10:7] CRm[6:3] Op2[2:0]; data is u64 | ICC_PMR, BPR0/1, AP0R0-3/AP1R0-3 (per priority bits), CTLR, SRE, IGRPEN0/1; plus ICH_* when EL2 is exposed | v3:193-274; akvm:412 |
| NR_IRQS | u32 total INTIDs (SGI+PPI+SPI), 64-1024 in steps of 32; set once | Default 256 total (224 SPIs) | v3:285-298; k/vgic/vgic-init.c:446-448 |
| CTRL attrs | VGIC_CTRL_INIT 0, ITS_SAVE_TABLES 1, ITS_RESTORE_TABLES 2, VGIC_SAVE_PENDING_TABLES 3, ITS_CTRL_RESET 4 | **CTRL_INIT is mandatory for v3** and must come after all vCPUs exist: `vgic_lazy_init` returns -EBUSY for non-v2 models. SAVE_PENDING_TABLES writes LPI pending bits into guest RAM. | akvm:426-431; v3:301-321; k/vgic/vgic-init.c:586-606 |
| LEVEL_INFO | mpidr [63:32] \| info [31:10] (LINE_LEVEL = 0) \| vINTID [9:0] (a multiple of 32); data is a u32 bitmap | PPIs per vCPU, SPIs global; SGIs and LPIs unsupported | v3:324-365; akvm:420-424 |
| MAINT_IRQ | attr [4:0] = PPI vINTID | NV | v3:367-376 |
| ITS restore order | a) memory and vCPUs; b) redistributors; c) ITS base; d) GITS_CBASER, then the other GITS_* except CTLR, then RESTORE_TABLES, then GITS_CTLR; e) MSI irqfds | GITS_CREADR after CBASER and before CTLR. GITS_IIDR before RESTORE_TABLES. Registers are u64. | its:89-145 |

### 4.5 Interrupt injection

| Mechanism | Encoding | Semantics | Source |
|---|---|---|---|
| `KVM_IRQ_LINE` (VM ioctl 0x4008AE61) | `struct kvm_irq_level {u32 irq; u32 level;}`; irq = vcpu2_index[31:28] \| irq_type[27:24] \| vcpu_index[23:16] \| irq_id[15:0] | irq_type CPU 0 (userspace irqchip only; irq_id 0 = IRQ, 1 = FIQ). SPI 1 with irq_id = **INTID 32-1019** (vCPU fields ignored). PPI 2 with INTID 16-31 on the target vCPU. level 1 asserts, 0 deasserts; an edge needs 1 then 0. | api:869-937; akvm:448-465; k/arm.c:1490-1556 |
| Example | SPI INTID 33 asserted: irq = 0x01000021, level = 1 | | derived from akvm:448-465 |
| `KVM_IRQFD` (0x4020AE76) | `{u32 fd; u32 gsi; u32 flags; u32 resamplefd; u8 pad[16]}`; flags DEASSIGN 1<<0, RESAMPLE 1<<1 | **SPI INTID = GSI pin + 32** (`spi_id = e->irqchip.pin + VGIC_NR_PRIVATE_IRQS`). Default routing after vGIC init maps GSI i to pin i for i < nr_spis. RESAMPLE gives level-triggered behaviour with an EOI notification on resamplefd. | ukvm:1065-1081; api:3178-3215; k/vgic/vgic-irqfd.c:18-33, 142-162; k/vgic/vgic-init.c:472 |
| `KVM_SET_GSI_ROUTING` (0x4008AE6A) | entry type IRQCHIP 1, MSI 2. MSI = {addr_lo, addr_hi, data, devid} with `KVM_MSI_VALID_DEVID` 1<<0 | On arm64, routing is irqfd-only. MSI with devid is translated through the ITS to an LPI. For PCI, devid is usually the BDF. | api:1884-1964; ukvm:1006-1062 |
| `KVM_SIGNAL_MSI` (0x4020AEA5) | `struct kvm_msi {addr_lo, addr_hi, data, flags, devid, pad[12]}` | >0 delivered; 0 blocked by the guest. Needs the in-kernel ITS. | api:2982-3013; ukvm:1161-1169 |
| `KVM_IOEVENTFD` (0x4040AE79) | `{u64 datamatch; u64 addr; u32 len; s32 fd; u32 flags; u8 pad[36]}`; flags DATAMATCH 1<<0, PIO 1<<1, DEASSIGN 1<<2 | len 1/2/4/8 (0 with ANY_LENGTH). A guest write signals the eventfd without a userspace exit (e.g. virtio-mmio QueueNotify). | ukvm:641-665; api:2093-2137 |
| ioeventfd needs ISV | The MMIO bus is consulted only for faults with a valid syndrome (inferred from control flow; see §6) | | k/mmio.c:174-190, 219-231 |
| Threading | IRQ_LINE, IRQFD and SIGNAL_MSI are VM ioctls, so any thread in the process may call them | | api:28-33 |

### 4.6 Registers (`KVM_GET_ONE_REG` 0x4010AEAB, `KVM_SET_ONE_REG` 0x4010AEAC, `KVM_GET_REG_LIST` 0xC008AEB0)

| Item | Encoding | Notes | Source |
|---|---|---|---|
| `struct kvm_one_reg` | `{u64 id; u64 addr;}` | ENOENT: no such register. EPERM: not finalized (SVE). | ukvm:1156-1159; api:2303-2324 |
| id arch | `KVM_REG_ARM64` = 0x6000000000000000 | | ukvm:1130 |
| id size [55:52] | U32 0x0020…, U64 0x0030…, U128 0x0040…, U256 0x0050…, U512 0x0060…, U2048 0x0080… | | ukvm:1135-1149 |
| coproc [27:16] | CORE 0x0010, DEMUX 0x0011, SYSREG 0x0013, FW 0x0014, SVE 0x0015, FW_FEAT_BMAP 0x0016 | | akvm:215-231, 276, 306, 352 |
| Core id | `KVM_REG_ARM64 \| size \| KVM_REG_ARM_CORE \| (offsetof(struct kvm_regs, f) / 4)` | `struct kvm_regs` = user_pt_regs {x0..x30, sp, pc, pstate}, sp_el1, elr_el1, spsr[5], then fp_regs 16-byte aligned; 864 B (PROBE) | akvm:46-55, 219-220 |
| Core ids | x0 0x6030000000100000 (xN adds 2N); sp (SP_EL0) …003e; **pc 0x6030000000100040**; pstate …0042; sp_el1 …0044; elr_el1 …0046; spsr_el1 …0048; v0 (U128) 0x6040000000100054; fpsr (U32) 0x60200000001000d4; fpcr …00d5 | V registers are rejected on SVE vCPUs | api:2575-2614; PROBE |
| SYSREG id | `ARM64_SYS_REG(op0,op1,crn,crm,op2)`: op0 [15:14], op1 [13:11], crn [10:7], crm [6:3], op2 [2:0], ORed with U64 | e.g. MPIDR_EL1 = 0x603000000013C005 (PROBE). The low 16 bits use the same packing as HVF `hv_sys_reg_t` (§1.6, derived). | akvm:231-255; api:2620-2622 |
| Timer registers | TIMER_CTL (3,3,14,3,1) = 0x603000000013DF19. **TIMER_CVAL = (3,3,14,0,2) = 0x603000000013DF02 and TIMER_CNT = (3,3,14,3,2) = 0x603000000013DF1A are "accidentally swapped" and must be used as defined.** PTIMER_* at akvm:258-260. | | akvm:257-273; api:2624-2632; k/sys_regs.c:5446-5456 |
| Timer write side effects | A TIMER_CNT write sets the **VM-wide** vtimer offset to (physical counter − value). It is silently ignored after `KVM_ARM_SET_COUNTER_OFFSET`. CTL writes mask ISTATUS. | | k/sys_regs.c:1698-1717; k/arch_timer.c:1078-1080; api:6270-6273 |
| DEMUX (CCSIDR) | 0x6020000000110000 \| csselr[7:0] | | akvm:223-228; api:2616-2618 |
| FW pseudo-registers | `KVM_REG_ARM_FW_REG(r)` = 0x60300000001400rr. r=0 PSCI_VERSION (VM-wide). r=1 SMCCC_ARCH_WORKAROUND_1 (0 NOT_AVAIL, 1 AVAIL, 2 NOT_REQUIRED). r=2 WORKAROUND_2 (0/1/2/3 plus ENABLED bit 4). r=3 WORKAROUND_3. | Save and restore these so the guest-visible "firmware" is stable | akvm:275-303; fw:22-75 |
| Hypercall bitmaps | 0x60300000001600rr: r=0 STD_BMAP (bit 0 TRNG); r=1 STD_HYP_BMAP (bit 0 PV_TIME); r=2 VENDOR_HYP_BMAP (bit 0 FUNC_FEAT, bit 1 PTP); r=3 VENDOR_HYP_BMAP_2 | EBUSY once any vCPU has run | akvm:351-395; fw:78-149; api:2697-2711 |
| SVE ids | Zn 0x6080…0015 \| n<<5 \| slice; Pn 0x6050…0015 \| 0x400 \| n<<5 \| slice; FFR 0x6050…00150600 \| slice; VLS 0x606000000015FFFF | | akvm:305-349; api:2638-2695 |
| `KVM_GET/SET_MP_STATE` (0x8004AE98 / 0x4004AE99) | RUNNABLE 0, STOPPED 5, SUSPENDED 10 | SUSPENDED emulates WFI and exits with SYSTEM_EVENT_WAKEUP (doc/code mismatch in §6) | ukvm:611-628; api:1515-1619; k/arm.c:794-818 |
| `KVM_GET/SET_VCPU_EVENTS` (0x8040AE9F / 0x4040AEA0) | `{u8 serror_pending, serror_has_esr, ext_dabt_pending, pad[5]; u64 serror_esr; u32 reserved[12]}` (64 B) | A pending SError can be saved and restored. An injected ext_dabt can't be read back and may only be set after an MMIO, NISV or LDST64B exit. | akvm:181-192; api:1193-1249, 1295-1319 |
| `KVM_GET_REG_LIST` | `{u64 n; u64 reg[]}`; E2BIG sets n | Lists core, SVE, sysregs (incl. timers) and FW registers | api:3603-3627; k/guest.c:661-668 |

### 4.7 `KVM_RUN` (0xAE80) and `struct kvm_run`

| Item | Detail | Source |
|---|---|---|
| mmap | vCPU fd at offset 0, GET_VCPU_MMAP_SIZE bytes. sizeof(kvm_run) = 2352; exit_reason @8; exit union @32 (256 B); kvm_valid_regs @288; `s` @304 (PROBE). | api:412-416; ukvm:223-238, 500-521 |
| Header | in: request_interrupt_window (u8), immediate_exit (u8), 6 B padding. out: exit_reason (u32), ready_for_interrupt_injection, if_flag, flags (u16; arm64 uses HSR_HIGH_VALID for DEBUG). cr8 and apic_base are x86. | ukvm:224-238; api:6582-6657 |
| KVM_RUN errors | EINTR: signal pending, exit_reason = KVM_EXIT_INTR (10). ENOEXEC: vCPU not initialized, or instruction fetch from device memory. ENOSYS: abort outside memslots with no syndrome and NISV_TO_USER off. EPERM: SVE not finalized. | api:392-410; include/linux/kvm_host.h:2471-2487 |
| `immediate_exit` | Checked once at entry; non-zero makes KVM_RUN return EINTR. **A pending MMIO completion is processed first**, so re-entering with immediate_exit=1 completes the MMIO without running the guest. | api:6591-6600, 6750-6755; k/arm.c:1256-1267; kmain:4469 |
| Exit numbers (arm64-relevant) | UNKNOWN 0, HYPERCALL 3, DEBUG 4, MMIO 6, FAIL_ENTRY 9, INTR 10, INTERNAL_ERROR 17, SYSTEM_EVENT 24, ARM_NISV 28, DIRTY_RING_FULL 31, MEMORY_FAULT 39, ARM_SEA 41, ARM_LDST64B 42 | ukvm:152-195 |
| MMIO | `mmio {u64 phys_addr; u8 data[8]; u32 len; u8 is_write;}`. For reads the VMM fills data[0..len) in byte-array order. On the next KVM_RUN, **KVM** sign-extends (SSE), truncates to 32 bits when SF=0, writes Rt, and advances PC. | ukvm:274-280; api:6724-6755; k/mmio.c:108-152, 219-259 |
| HYPERCALL (arm64) | `hypercall {nr = SMCCC function ID; args[6] and ret unused; flags}`; flags SMC 1<<0 (else HVC), 16BIT 1<<1. **PC already points past the HVC/SMC.** The VMM reads x1.. with GET_ONE_REG and writes x0-x3 with SET_ONE_REG. | ukvm:288-300; akvm:526-528; api:6774-6796; k/hypercalls.c:246-263 |
| SYSTEM_EVENT | `{u32 type; u32 ndata; u64 data[16]}`. Types SHUTDOWN 1, RESET 2, CRASH 3, WAKEUP 4, SUSPEND 5. arm64 data[0] = RESET2 flag 1<<0 (RESET) or OFF2 flag 1<<0 (SHUTDOWN). | ukvm:385-402; akvm:493-504; api:6936-7031 |
| ARM_NISV / LDST64B | `arm_nisv {u64 esr_iss; u64 fault_ipa;}`. NISV needs the VM cap. PC is **not** advanced; the VMM decodes and emulates, or injects an abort. | ukvm:418-422; api:7101-7166; k/mmio.c:174-212 |
| ARM_SEA | `{flags (GPA_VALID 1<<0), esr, gva, gpa}`; KVM_CAP_ARM_SEA_TO_USER | ukvm:490-497; api:7368-7398 |
| MEMORY_FAULT | `{flags (PRIVATE 1<<3), gpa, size}`. **Comes with ioctl = -1 and errno EFAULT or EHWPOISON.** | ukvm:458-464; api:7260-7280 |
| FAIL_ENTRY | hardware_entry_failure_reason CPU_UNSUPPORTED (1<<0) when running on a CPU outside the selected PMU's CPUs | akvm:506-507; k/arm.c:1211-1215; vcpu:136-142 |
| Sync regs | KVM_CAP_SYNC_REGS is s390/x86 only. arm64 `s.regs.device_irq_level` (VTIMER 1<<0, PTIMER 1<<1, PMU 1<<2) matters only with a userspace irqchip. | api:7684-7687, 9047-9088; akvm:156-164 |

### 4.8 SMCCC filter and in-kernel PSCI

| Item | Value | Semantics | Source |
|---|---|---|---|
| Group / attr | VM fd `KVM_SET_DEVICE_ATTR`, group `KVM_ARM_VM_SMCCC_CTRL` = 0, attr `KVM_ARM_VM_SMCCC_FILTER` = 0 | | akvm:397-399 |
| `struct kvm_smccc_filter` (24 B) | `{u32 base; u32 nr_functions; u8 action; u8 pad[15];}` covering [base, base+nr) | pad must be 0 | akvm:509-524; vm:346-392 |
| Actions | HANDLE 0 (default), DENY 1 (guest gets NOT_SUPPORTED), FWD_TO_USER 2 (KVM_EXIT_HYPERCALL) | | vm:379-389; k/hypercalls.c:265-287 |
| Errors / timing | EEXIST: overlaps an existing or reserved range. **EBUSY: any vCPU has run.** EINVAL: bad range or action. | | vm:335-344; k/hypercalls.c:172-209 |
| Reserved ranges | 0x8000_0000-0x8000_FFFF and 0xC000_0000-0xC000_FFFF (Arm Architecture Calls: SMCCC_VERSION, ARCH_FEATURES, WORKAROUND_*) can't be filtered | **PSCI (0x84xx_xxxx / 0xC4xx_xxxx) can be forwarded to userspace** | vm:394-402; k/hypercalls.c:138-165 |
| Precedence / defaults | The filter overrides the FW bitmaps. Allowed by default: SMCCC_VERSION, ARCH_FEATURES, standard-service function numbers ≤ 0x1F (PSCI), and the KVM PSCI 0.1 IDs. | | vm:374-377; k/hypercalls.c:70-96, 229-244 |
| Conduits | HVC and SMC imm16 = 0 take the same path. SMC with nonzero imm returns x0 = ~0. SMC traps pre-increment PC. With NV, calls forward to virtual EL2. | | k/handle_exit.c:38-95 |
| SMCCC version reported | `ARM_SMCCC_VERSION_1_1` | | k/hypercalls.c:290-291 |
| PSCI version | 1.3 (`KVM_ARM_PSCI_LATEST`) with PSCI_0_2, unless the FW register is written. Otherwise PSCI 0.1 at base 0x95c1ba5e: CPU_SUSPEND +0, CPU_OFF +1, CPU_ON +2, MIGRATE +3. | | include/kvm/arm_psci.h:13-38; akvm:479-491 |
| CPU_ON (in kernel) | Finds the target by MPIDR. The target must be STOPPED, else ALREADY_ON (≥0.2). Sets PC = arg2 (entry), x0 = arg3 (context_id), caller's endianness, then RUNNABLE and kick. | A stopped vCPU's KVM_RUN sleeps until then | k/psci.c:60-119; k/reset.c:237-264; k/arm.c:1011-1030 |
| SYSTEM_OFF / RESET / RESET2 / OFF2 | Exit with KVM_EXIT_SYSTEM_EVENT (ndata = 1). **All vCPUs set STOPPED.** x0 is preloaded with INTERNAL_FAILURE (-6) in case the VMM resumes. | A reset requires KVM_ARM_VCPU_INIT on every vCPU | k/psci.c:164-212, 288-311; include/uapi/linux/psci.h:137 |
| SYSTEM_SUSPEND | Exits to userspace only with KVM_CAP_ARM_SYSTEM_SUSPEND. The VMM must set MP_STATE = SUSPENDED or deny. | | api:7001-7025, 8861-8868; k/psci.c:214-221 |
| MIGRATE_INFO_TYPE | returns 2 (TOS_MP: no trusted OS / no migration needed) | | k/psci.c:280-287; include/uapi/linux/psci.h:99 |

### 4.9 Timers and counter

| Item | Detail | Source |
|---|---|---|
| `KVM_ARM_SET_COUNTER_OFFSET` | VM ioctl 0x4010AEB5 (PROBE); `struct kvm_arm_counter_offset {u64 counter_offset; u64 reserved;}` (reserved = 0). The offset is subtracted from **both** virtual and physical counters of all current and future vCPUs. EBUSY if a vCPU ioctl runs concurrently. | ukvm:1341-1342; akvm:202-209; api:6237-6273; k/arch_timer.c:1724-1757 |
| Doc naming | api.rst text says "KVM_ARM_SET_CNT_OFFSET"; the header name is `KVM_ARM_SET_COUNTER_OFFSET` | api:6248 vs ukvm:1342 |
| Default offset | Without a VM offset, vCPU init sets the vtimer offset to the current physical counter, so guest CNTVCT starts near 0. The ptimer offset is 0. | k/arch_timer.c:1106-1111 |
| Emulation | With the in-kernel vGIC, KVM injects the timer PPIs (vtimer 27, ptimer 30 by default). With a userspace irqchip, levels come via device_irq_level. | vcpu:186-202; api:9047-9088 |

### 4.10 Dirty tracking for snapshots

| Item | Detail | Source |
|---|---|---|
| `KVM_GET_DIRTY_LOG` (0x4010AE42) | `{u32 slot; u32 pad; u64 dirty_bitmap_ptr}`. One bit per **host** page, bit 0 = the slot's first page. Clear-on-read unless MANUAL_PROTECT2. The slot needs `KVM_MEM_LOG_DIRTY_PAGES`. | ukvm:572-580; api:354-389 |
| `KVM_CLEAR_DIRTY_LOG` (0xC018AEC0) | `{u32 slot; u32 num_pages; u64 first_page; u64 bitmap_ptr}`. first_page must be a multiple of 64, and so must num_pages unless the range reaches the slot end. | ukvm:582-591; api:5029-5069 |
| MANUAL_PROTECT2 enable-cap | flags MANUAL_PROTECT_ENABLE 1<<0, INITIALLY_SET 1<<1 (requires the first) | ukvm:1493-1494; api:8176-8216 |
| Dirty ring on arm64 | Use **KVM_CAP_DIRTY_LOG_RING_ACQ_REL (223)**; DIRTY_LOG_RING (192) returns 0. Enable once, **before any vCPU is created**. Size: a power of two, ≥ PAGE_SIZE, ≥ reserved entries. | k/Kconfig:29-30; kmain:4906-4917, 4941-4973; api:8652-8684 |
| Ring layout | `struct kvm_dirty_gfn {u32 flags; u32 slot (as_id\|id); u64 offset;}`; DIRTY 1<<0, RESET 1<<1. mmap the vCPU fd at `KVM_DIRTY_LOG_PAGE_OFFSET` (64) × PAGE_SIZE. Harvest with acquire/release, then `KVM_RESET_DIRTY_RINGS` (0xAEC7). A full ring gives KVM_EXIT_DIRTY_RING_FULL; flushing needs a vCPU kick. | ukvm:1505-1542; akvm:44; api:8686-8744 |
| WITH_BITMAP (225) | Needed on arm64 because vGIC/ITS table saves dirty memory without a vCPU. Collect the bitmap with GET_DIRTY_LOG as the last step. Enable after ACQ_REL, before any memslot exists. | api:8746-8775; its:53-58 |
| Eager split | KVM_CAP_ARM_EAGER_SPLIT_CHUNK_SIZE, set before any memslot | api:8814-8838 |

### 4.11 Other snapshot/restore constraints (arm64)

| Constraint | Source |
|---|---|
| Recreate vCPUs and redistributor regions in the **same order**, or the vCPU↔GICR association changes | v3:63-68 |
| Use identical KVM_ARM_VCPU_INIT features on all vCPUs (VM-wide). POWER_OFF doesn't matter on restore; restore MP_STATE instead. | k/arm.c:1650-1701 |
| Write **ID registers before any other vCPU register**; they're writable only until the first KVM_RUN. Query writable fields with `KVM_ARM_GET_REG_WRITABLE_MASKS` (0x8040AEB6; range 0 returns 3×8×8 u64 masks, indexed by `KVM_ARM_FEATURE_ID_RANGE_IDX`). | feat:21-48; api:6277-6323; akvm:530-560 |
| MIDR, REVIDR and AIDR are writable only with KVM_CAP_ARM_WRITABLE_IMP_ID_REGS, enabled before any vCPU exists (per-VM values) | api:8870-8886 |
| FW bitmap registers and the SMCCC filter must be set before the first KVM_RUN (EBUSY afterwards). PSCI_VERSION is VM-wide. | api:2709-2711; vm:340; fw:30-36 |
| Complete any pending MMIO before saving by re-entering with immediate_exit=1; that state isn't visible to userspace | api:6750-6755 |
| vGIC restore order: GICD_IIDR, then CTRL_INIT (after all vCPUs), then DIST, REDIST, CPU_SYSREGS, LEVEL_INFO, then the ITS sequence (4.4), then irqfds | v3:126-143, 301-306; its:126-145 |
| Counter: use `KVM_ARM_SET_COUNTER_OFFSET` (VM-wide; CNTVCT writes become no-ops) **or** write TIMER_CNT (the swapped constant) | api:6237-6273; k/sys_regs.c:1705-1712 |
| guest_memfd slots can't be dirty-logged; use anonymous or file-backed memslots when tracking dirty pages | kmain:1581-1583 |
| Re-apply per-vCPU PVTIME_IPA and PMU attrs on restore (PMU INIT after vGIC init) | vcpu:61-63, 206-227 |

### 4.12 x86_64 headline essentials

| Item | Essential | Source |
|---|---|---|
| CPUID | `KVM_GET_SUPPORTED_CPUID` (system ioctl; `struct kvm_cpuid2` of `kvm_cpuid_entry2`), then `KVM_SET_CPUID2` per vCPU. If SET fails, the CPUID state isn't guaranteed. | api:705-720, 1762-1800; ukvm:729, 1371 |
| MSRs | `KVM_GET_MSR_INDEX_LIST` (E2BIG size handshake). **`KVM_SET_MSRS` returns the number of MSRs set** and stops at the first failure. | api:218-257, 683-702 |
| irqchip | `KVM_CREATE_IRQCHIP` = PIC + IOAPIC + per-vCPU LAPIC (GSI 0-15 to PIC and IOAPIC, 16-23 to IOAPIC only). Or `KVM_CAP_SPLIT_IRQCHIP`: only the LAPIC in the kernel, MSI routes, enabled before any vCPU. | api:847-859, 7918-7937 |
| PIT | `KVM_CREATE_PIT2`, only after KVM_CREATE_IRQCHIP | api:3021-3051 |
| Intel quirks | `KVM_SET_TSS_ADDR` (3 pages below 4 GiB); `KVM_SET_IDENTITY_MAP_ADDR` (1 page, before any vCPU) | api:1438-1455, 1621-1643 |
| Snapshot state | `KVM_GET/SET_REGS`, `SREGS`/`SREGS2`, `FPU`, `XSAVE`/`XSAVE2`, `XCRS`, `LAPIC`, `MSRS`, `VCPU_EVENTS`, `MP_STATE`, `TSC_KHZ` | ukvm:1356-1413, 1490-1491, 1630; api:2001-2022 |
| PIO exit | KVM_EXIT_IO (2): `io {direction (IN 0 / OUT 1), size, port, count, data_offset}`; data at kvm_run + data_offset | ukvm:260-269; api:6695-6710 |
| Boot protocol | 64-bit entry = load address + 0x200. Long mode with paging on; identity map covering the kernel, zero page and cmdline. GDT with `__BOOT_CS`(0x10) and `__BOOT_DS`(0x18), 4G flat. Interrupts off. **%rsi = base of struct boot_params.** | Documentation/arch/x86/boot.rst:1362-1399 |

### Section 4 bug-magnets

1. **SPI numbering differs by API.** An irqfd GSI N becomes INTID N+32. `KVM_IRQ_LINE` type SPI takes the raw INTID (≥32). Mixing them gives off-by-32 interrupts.
2. **`KVM_REG_ARM_TIMER_CVAL` and `KVM_REG_ARM_TIMER_CNT` are swapped** relative to the architectural encodings. Never derive them. Writing CNT rewrites the VM-wide offset. After `KVM_ARM_SET_COUNTER_OFFSET`, CNT writes are silently ignored.
3. **`KVM_CREATE_VM(0)` means a 40-bit IPA** and fails on hosts with a smaller limit. Always pass `KVM_VM_TYPE_ARM_IPA_SIZE(n)`.
4. **vGICv3 is never lazily initialized.** `CTRL_INIT` must follow creation of all vCPUs. Before init, only GICD_IIDR and TYPER2 are accessible. Bases need 64 KiB alignment. GICR↔vCPU mapping follows creation order.
5. **KVM completes MMIO on the next KVM_RUN.** It writes Rt, sign-extends and advances PC then. For hypercalls, PC is already advanced at exit and results go to x0-x3 via SET_ONE_REG. Before a snapshot, re-enter with immediate_exit=1.
6. **Dirty tracking on arm64** requires ACQ_REL enabled before any vCPU exists, plus WITH_BITMAP for vGIC/ITS saves. Bitmaps and memslot alignment use the host PAGE_SIZE (16K or 64K hosts exist). guest_memfd slots can't be logged.
7. **INIT features are VM-wide; only POWER_OFF is per-call.** Secondaries without POWER_OFF start running at PC=0. PSCI SYSTEM_OFF and RESET stop *all* vCPUs.
8. **Default MPIDR packs 16 vCPUs per Aff0 cluster.** DT `cpu@` reg values and PSCI CPU_ON targets are MPIDR values, not vCPU indices.

## 5. HVF vs KVM: semantic differences a shared VMM core must abstract

"Derived" cells are conclusions drawn from the cited facts, not quotes. Section cross-references (§) point to the tables above, which carry their own citations.

| # | Concern | HVF (macOS arm64) | KVM (Linux arm64) | Implication for the shards core |
|---|---|---|---|---|
| 1 | VM ↔ process | One VM per process (AD:(root)); `hv_vm_create` acts on the current process (`hv_vm.h:27-33`) | Many VMs per process possible; VM ioctls must come from the creating process (api:31-33) | Use one VM per VMM process on both backends. |
| 2 | vCPU ↔ thread | **Hard binding.** vCPU created "for the current thread"; one vCPU per thread (`hv_vcpu.h:19-29`). Every per-vCPU call (registers, run, pending IRQ, per-vCPU GIC registers) must come from the owning thread, except `hv_vcpus_exit` (AD:vcpu-management; §1.1). No dispatch queues. | vCPU ioctls "should" come from the creating thread (a performance hint: api:38-41) | Derived: give each vCPU a dedicated OS thread for its whole life (create, init, restore, run, save, destroy). Marshal cross-vCPU work (PSCI CPU_ON target setup, snapshot register capture, reset) to the owning thread by message. Never touch vCPU state from a device, API or timer thread. |
| 3 | Kick / pause | `hv_vcpus_exit(list)` from any thread gives `HV_EXIT_REASON_CANCELED`. A vCPU that isn't running returns immediately from its *next* `hv_vcpu_run` (`hv_vcpu.h:371-381`; `hv_vcpu_types.h:39-40`). | Signal the thread (KVM_RUN returns EINTR / `KVM_EXIT_INTR`), or set `immediate_exit`, checked once at entry (api:392-410, 6591-6600) | Use one "kick" primitive. HVF's cancel is sticky for a non-running vCPU; KVM needs signal + `immediate_exit` to avoid the lost-wakeup race (derived). |
| 4 | PSCI / SMCCC ownership | No PSCI or SMCCC facility appears in the SDK headers or AD (derived from the §1.2 inventory). A guest HVC is a synchronous exception to EL2, which HVF reports as `HV_EXIT_REASON_EXCEPTION` ("synchronous exception to a higher EL triggered by the guest", `hv_vcpu_types.h:41-42`) with EC 0x16. No in-framework handling is documented (§6 H16). SMC arrives as EC 0x17 only if HVF sets TSC (§6 H8). The VMM implements PSCI: VERSION, FEATURES, CPU_ON/OFF, AFFINITY_INFO, SYSTEM_OFF/RESET, MIGRATE_INFO_TYPE, and the SMCCC_VERSION / ARCH_FEATURES / WORKAROUND probes Linux makes (§2). | In-kernel PSCI 1.3 with `KVM_ARM_VCPU_PSCI_0_2`. CPU_ON is in-kernel. SYSTEM_OFF/RESET become `KVM_EXIT_SYSTEM_EVENT` and stop all vCPUs. PSCI can optionally be forwarded to userspace with the SMCCC filter (`FWD_TO_USER`); the 0x8000_xxxx/0xC000_xxxx arch-call ranges can't be filtered (§4.8). | Derived: implement PSCI/SMCCC once in the core, for HVF. On KVM use in-kernel PSCI (faster CPU_ON), but pin `KVM_REG_ARM_PSCI_VERSION` and the WORKAROUND FW registers (§4.6) to what the core exposes on HVF, so the guest sees identical firmware on both backends. |
| 5 | HVC vs SMC return address | Raw exception. For an SMC trapped by `HCR_EL2.TSC` the return address is the SMC itself, so the VMM must `PC += 4`. For HVC it must not (DDI0487 M.c D1.4.1.5 rules LBLBR vs DKWPP, §2.1). Linux's KVM pre-increments only for SMC (`arch/arm64/kvm/handle_exit.c:57-76` vs `:38-54`). Whether HVF sets `HCR_EL2.TSC` (so SMC traps rather than UNDEFs) is **UNVERIFIED**. | Done in kernel. On `KVM_EXIT_HYPERCALL`, PC is already past the instruction (api:6774-6796). | Use DT `psci { method = "hvc"; }` on both backends to avoid the SMC path entirely (derived). |
| 6 | MMIO decode and completion | The exit carries only ESR (`syndrome`), FAR and IPA (`hv_vcpu_types.h:61-86`). The VMM must check ISV, decode SAS/SSE/SRT/SF/WnR (§2.1), perform the access, then for reads write Rt (**Rt=31 is XZR, discard it; `HV_REG` 31 is PC**, §1.5), sign-extend when SSE and size < 8, zero bits 63:32 when SF=0, and advance PC by 4 (D1.4.1.5 rule QYCWH). KVM's reference algorithm is `arch/arm64/kvm/mmio.c:108-149`. Also reject CM=1 and S1PTW=1 aborts as device accesses (§2.1). | The kernel decodes into `kvm_run.mmio {phys_addr, data[8], len, is_write}` and completes (Rt write, extension, PC advance) on the next KVM_RUN (§4.7) | Derived: the core device bus takes `(ipa, len, is_write, data)`. The HVF backend re-implements `kvm_handle_mmio_return` exactly, including Rt=31. |
| 7 | ISV=0 aborts (no syndrome: LDP/STP, writeback, SIMD, etc.) | No assistance. Emulate by decoding the guest instruction, or inject an abort (derived) | Default `-ENOSYS` from KVM_RUN, or `KVM_EXIT_ARM_NISV` with `KVM_CAP_ARM_NISV_TO_USER`; PC not advanced (§4.7; `arch/arm64/kvm/mmio.c:174-189`) | Derived: treat ISV=0 on device MMIO as a fatal guest/driver bug with a diagnostic, on both backends. Linux's PL011, PL031, virtio-mmio and GIC accessors are plain LDR/STR (see §2 for ISV rules). |
| 8 | WFI / WFE | Without `hv_gic`: an EC 0x01 exit. The VMM must sleep until an interrupt is pending or the vtimer deadline passes (`CNTV_CVAL` vs `CNTVCT = mach_absolute_time() - offset`), then `PC += 4` (KVM does the same increment: `arch/arm64/kvm/handle_exit.c:130-175`). With `hv_gic`: the macOS 27 WFI-wait-time API exists only for GIC VMs (AD:hv_vcpu_get_wait_for_interrupt_time(_:_:)), which suggests in-framework WFI handling. **UNVERIFIED.** | In-kernel halt; userspace never sees WFx (`arch/arm64/kvm/handle_exit.c:130-175`) | Derived: the HVF backend needs a per-vCPU timed park/unpark (condvar + deadline) woken by interrupt injection and `hv_vcpus_exit`. The KVM backend needs none. |
| 9 | Timers and counter | Per-vCPU offset relative to `mach_absolute_time` (`hv_vcpu.h:438-449`). Emulated-GIC path: `VTIMER_ACTIVATED` exit with auto-mask, unmask on guest EOI (§1.7). `hv_gic` path: vtimer INTID 27 and EL1 phys timer INTID 30 reserved by the framework (`hv_gic_types.h:37-50`). Host counter 24 MHz (HOST). | In-kernel timers; default PPIs vtimer 27, ptimer 30, hvtimer 28, hptimer 26 (`arch/arm64/kvm/arch_timer.c:35-39`). VM-wide `KVM_ARM_SET_COUNTER_OFFSET`, or swapped-constant `TIMER_CNT` writes (§4.6, §4.9). | Derived: use the same DT timer PPIs on both backends (virt = PPI 11 / INTID 27, non-secure phys = PPI 14 / INTID 30). Snapshot guest CNTVCT; on restore set per-vCPU `vtimer_offset` (HVF) or the VM-wide counter offset (KVM). Record `CNTFRQ` in snapshot metadata and refuse restore on mismatch. Neither backend can change what the guest reads: an EL1 read of `CNTFRQ_EL0` has no trap condition (DDI0487 M.c D24.10.1, §2.5), and HVF has no CNTFRQ register (§1.6). |
| 10 | Interrupt controller | macOS ≥15: `hv_gic` GICv3, configured before any vCPU exists. No ITS/LPI registers. MSIs are SPIs via a GICv2m-offset frame. Redistributor = 2×64 KiB per vCPU (§1.8). macOS <15: no in-framework GIC, so the VMM emulates the GIC and drives IRQ/FIQ with `hv_vcpu_set_pending_interrupt` (`hv_vcpu.h:301-312`). | vGICv3 via `KVM_CREATE_DEVICE`. Mandatory `CTRL_INIT` after all vCPUs exist. 64 KiB-aligned bases. Optional ITS (MSI → LPI with DeviceID) (§4.4). | Derived: generate DT per backend. KVM: `arm,gic-v3` plus an `arm,gic-v3-its` child (`msi-controller`, used by PCI `msi-map`). HVF: `arm,gic-v3` plus an `arm,gic-v2m-frame` node (inference, §1.8). HVF MSI capacity is bounded by the SPI range (`hv_gic_get_spi_interrupt_range`), which limits MSI-X vectors for virtio-pci and VFIO. |
| 11 | Injection from non-vCPU threads | `hv_gic_set_spi` / `hv_gic_send_msi` carry no owning-thread rule, so device threads may call them (`hv_gic.h:51-77`). Without `hv_gic`, only the owning thread can set IRQ/FIQ, so device threads must message it and `hv_vcpus_exit` it (`hv_vcpu.h:301-312`, §1.1). No eventfd-style path. | `KVM_IRQ_LINE` (VM ioctl, any thread) or `KVM_IRQFD` (eventfd; no VMM thread hop). MSI via routing or `KVM_SIGNAL_MSI` (§4.5). | Derived: core interface `set_spi(intid, level)` and `send_msi(addr, data, devid)`. **Number SPIs by INTID in the core.** Convert only at the KVM irqfd edge (GSI = INTID − 32, `arch/arm64/kvm/vgic/vgic-irqfd.c:18-33`). |
| 12 | Doorbells (ioeventfd) | No public equivalent. The `hv_vm_monitor_data_abort` Mach-message mechanism is referenced but undeclared (`hv_kern_types.h:78-116`). Every virtio notify is a full vCPU exit to the VMM. | `KVM_IOEVENTFD` (needs ISV=1 writes) (§4.5) | Derived: keep HVF notify handlers O(1) (hand off to an I/O thread). The notify path is a latency hot spot for startup I/O. |
| 13 | Mapping granularity / host pages | `hv_vm_map` addr/ipa/size are host-page aligned: **16 KiB** here (HOST `hw.pagesize`; `hv_vm.h:44-72`). IPA granule 4K/16K selectable since 26.0; effect on alignment **UNVERIFIED** (§1.4). | Host `PAGE_SIZE` alignment (4K/16K/64K) (`virt/kvm/kvm_main.c:2014-2033`) | Derived: lay out guest RAM, holes and snapshot page maps on ≥16 KiB boundaries (64 KiB for full portability). Never put RAM and an MMIO window in the same 16 KiB page. The guest's 4 KiB page size is a stage-1 choice independent of the host page, but the guest must see `ID_AA64MMFR0_EL1.TGran4` as supported or Linux parks in `__no_granule_support` (`arch/arm64/kernel/head.S:461-465, 498-506`). |
| 14 | Memory regions API | Unnamed IPA ranges: map/unmap/protect. Host backing must be one VM region from `mmap` or `mach_vm_allocate` (AD:hv_vm_map(_:_:_:_:)). | Numbered memslots with flags. Delete plus re-add; no resize (§4.3) | Derived: the core keeps a region table keyed by IPA; the KVM backend assigns slot ids. |
| 15 | Dirty tracking | None; only write-protect via `hv_vm_protect` plus fault exits (§1.4) | Dirty bitmap or dirty ring (ACQ_REL + WITH_BITMAP on arm64) (§4.10) | Derived: HVF snapshots are full-memory (or VMM-level WP tracking). Fast restore should mmap the snapshot file; whether `hv_vm_map` accepts file-backed mappings is **UNVERIFIED**. |
| 16 | Register naming | `hv_reg_t` (X0..X30 = 0..30, **PC = 31**, FPCR 32, FPSR 33, CPSR 34). SP_EL0/SP_EL1 are sysregs. `hv_sys_reg_t` = `op0<<14 \| op1<<11 \| CRn<<7 \| CRm<<3 \| op2` (§1.5-1.6). | Core regs by `offsetof(kvm_regs)/4` (PC = 0x6030000000100040). Sysreg id = 0x6030000000130000 \| the same 16-bit packing (§4.6). The vGIC `CPU_SYSREGS` attr also uses it (`v3`:193-274). | Derived: the 16-bit `op0..op2` packing is identical in `hv_sys_reg_t`, KVM `ARM64_SYS_REG` and KVM vGIC CPU_SYSREGS. Use it as the canonical sysreg key in the snapshot format. Map core registers explicitly. |
| 17 | Snapshottable state | Registers per §1.9. The GIC is an opaque versioned blob that "can fail" after a macOS update, plus per-vCPU ICC registers. No PMU. No pending-SError before macOS 27. No vCPU-events. | `KVM_GET_REG_LIST` enumerates everything (core, sysregs, timers, FW pseudo-registers). vGIC via attrs. `VCPU_EVENTS`, `MP_STATE` (§4.6, §4.11). | Derived: the snapshot format is backend-specific for the interrupt controller and pending-event state. Record backend, OS build and GIC-blob version in metadata. Keep a register-level GIC fallback for HVF (dist/redist/ICC register APIs). |
| 18 | vCPU reset | No reset API; the VMM rewrites all registers, plus `hv_gic_reset()` for the GIC (`hv_gic.h:267-276`) | `KVM_ARM_VCPU_INIT` re-init resets (§4.2) | Derived: the core owns a canonical reset register set (CPSR 0x3C5, SCTLR_EL1 reset, etc.). KVM's warm-reset SCTLR_EL1 value is 0x00C50078 (`arch/arm64/kvm/sys_regs.c:3377`). |
| 19 | Topology / MPIDR | The VMM must set `MPIDR_EL1` before `hv_gic_get_redistributor_base` (`hv_gic.h:105-114`) and GIC routing (`hv_gic.h:40-41`) | Default MPIDR derived from vcpu_id: Aff0 = id&0xF, Aff1 = (id>>4)&0xFF, Aff2 = (id>>12)&0xFF, bit 31 set (`arch/arm64/kvm/sys_regs.c:977-995`) | Derived: compute MPIDR with KVM's scheme and write it explicitly on HVF, so DT `cpu@` reg values and PSCI CPU_ON targets match across backends. |
| 20 | IPA size | `hv_vm_config_set_ipa_size` ≤ max. Host limit 40 bits (4K granule) or 42 bits (16K) (HOST; §1.4). | VM-type bits. 0 means 40, which fails if the host limit is below 40. The guest PARange is unchanged (§4.1). | Derived: keep the whole guest physical map below 2^40 and request the IPA size explicitly on both backends. |
| 21 | Guest CPU features | Read with `hv_vcpu_config_get_feature_reg`. ID registers written with `hv_vcpu_set_sys_reg` (DFR0 documented, `hv_vm_config.h:104`; the rest **UNVERIFIED**). | Writable ID registers before the first KVM_RUN, discovered via `KVM_ARM_GET_REG_WRITABLE_MASKS` (§4.11) | Derived: pin an explicit feature profile per snapshot so restore on a different CPU generation fails fast rather than silently. |
| 22 | Injecting aborts or SError | `hv_vcpu_set_serror` exists only on macOS 27 (AD). No external-abort injection API; the VMM would have to synthesize exception entry through `ESR_EL1`/`FAR_EL1`/`ELR_EL1`/`SPSR_EL1`/`VBAR_EL1`/PC/CPSR (derived). | `KVM_SET_VCPU_EVENTS` (serror, ext_dabt) (§4.6) | Derived: keep "inject abort" a backend capability flag. |
| 23 | Exit model | 4 reasons; everything else is an ESR EC (`hv_vcpu_types.h:35-86`) | Many pre-decoded exit reasons (§4.7) | Derived: the core defines its own exit enum (Mmio, Hypercall(fn, x1..x3), Wfx(deadline), SysReg, SystemEvent, Canceled, Fatal). Each backend adapts to it. |
| 24 | Privilege / availability | Requires the `com.apple.security.hypervisor` entitlement. Check `kern.hv_support` (AD:(root)). | Requires /dev/kvm access. `KVM_GET_API_VERSION` = 12 (§4.1). | Probe both at startup and fail with actionable errors. |
| 25 | Cache maintenance of VMM-written guest memory (kernel, DTB, initrd, restored snapshot pages) for an MMU-off guest (booting.rst:177-191 requires the image cleaned to PoC) | Undocumented whether `hv_vm_map` or stage-2 faults perform clean/invalidate (**UNVERIFIED**). The SDK provides `sys_dcache_flush` / `sys_icache_invalidate` / `sys_cache_control` (`MacOSX26.4.sdk/usr/include/libkern/OSCacheControl.h:55-61`). | KVM cleans and invalidates the D-cache to PoC (and invalidates the I-cache as needed) when installing a cacheable stage-2 PTE (`arch/arm64/kvm/hyp/pgtable.c:1000-1004`) | Derived: on HVF, after writing guest code or data through the host mapping and before first vCPU entry, call `sys_dcache_flush` + `sys_icache_invalidate` on the written ranges until HVF behavior is verified. Cost scales with bytes flushed, so limit it to the ranges actually written. |

## 6. Consolidated UNVERIFIED list

Nothing below is backed by a primary source that was read. Each item needs a runtime probe on the target host, or a source not yet retrieved, before code depends on it.

### HVF (Section 1 and HVF rows of Section 5)

| # | Item | Why unverified / how to settle |
|---|---|---|
| H1 | Does `HV_IPA_GRANULE_4KB` (macOS 26) make `hv_vm_map`/`unmap`/`protect` accept 4 KiB-aligned `addr`/`ipa`/`size` on a 16 KiB-page host? | Headers and AD are silent (`hv_vm.h:44-72`, `hv_vm_config.h:110-143`). Probe with a 4 KiB-aligned map. |
| H2 | Does `hv_vm_map` accept file-backed `mmap` regions (MAP_PRIVATE and MAP_SHARED)? This gates mmap-based snapshot restore. | AD only says "typically allocated with `mmap` or `mach_vm_allocate` instead of `malloc`". |
| H3 | Initial register state after `hv_vcpu_create` (CPSR, SCTLR_EL1, etc.). | Undocumented. Always write a full canonical state. |
| H4 | Do `HV_EXIT_REASON_VTIMER_ACTIVATED` exits and EC=0x01 WFx exits still occur once an `hv_gic` exists? | Inferred only from the macOS 27 WFI-wait-time API being GIC-only. |
| H5 | Which redistributor HVF marks `GICR_TYPER.Last`: the last *created* vCPU, or the last *supported* one? This determines how large the DT GICR `reg` must be (Linux walks until Last: `drivers/irqchip/irq-gic-v3.c:984-1021,1070-1119`). | Undocumented. Read `GICR_TYPER` via `hv_gic_get_redistributor_reg`. |
| H6 | Does HVF's MSI frame (`GICM_TYPER` 0x8, `GICM_SET_SPI_NSR` 0x40) fully behave as a GICv2m frame for Linux's `arm,gic-v2m-frame` driver (TYPER base/count encoding, `MSI_IIDR` 0xFCC)? | Only the register offsets match. |
| H7 | Guest priority bits under `hv_gic` (only AP0R0/AP1R0 exposed). | Inference. |
| H8 | Does HVF set `HCR_EL2.TSC`, so that guest SMC arrives as EC 0x17 rather than UNDEF? | Undocumented. Using `method = "hvc"` avoids the question. |
| H9 | Guest-visible `CNTFRQ_EL0` value under HVF. Expected to be the hardware value (24 MHz host timebase); it is untrappable at EL1 (DDI0487 D24.10.1) and absent from `hv_sys_reg_t`. | Not observed from a guest. |
| H10 | Which ID-register writes through `hv_vcpu_set_sys_reg` are honoured, beyond `ID_AA64DFR0_EL1` (`hv_vm_config.h:104`)? | Undocumented. |
| H11 | Numeric values of the macOS 27 additions: `hv_tlbi_op_t` members, `HV_SYS_REG_ID_AA64{ISAR2,MMFR3,MMFR4,PFR2}_EL1`, `HV_FEATURE_REG_*`. | Not in SDK 26.4. AD shows names only. Not callable on the macOS 26.4.1 dev host. |
| H12 | Units of `hv_vcpu_get_exec_time`: the header says mach_absolute_time units (`hv_vcpu.h:384`); AD says nanoseconds. | Conflicting sources. |
| H13 | Meaning of `kern.hv.max_address_spaces: 128` on arm64. | Sysctl observed only. |
| H14 | Runtime values on this host: `hv_vm_get_max_vcpu_count`, default and max IPA size per granule, the default granule, all `hv_gic_get_*` sizes, alignments and SPI range, `hv_sme_config_get_max_svl_bytes`. | Not queried: no code was written for this research. The sysctl IPA values (2^40 at 4K, 2^42 at 16K) are observations only. |
| H15 | Does HVF perform D-cache clean/invalidate to PoC when mapping, or on the first stage-2 fault, for VMM-written guest memory? booting.rst requires the image cleaned to PoC for MMU-off entry. | Undocumented. KVM does it (`arch/arm64/kvm/hyp/pgtable.c:1000-1004`). |
| H16 | Is there no in-framework PSCI/SMCCC handling? | Inferred from absence in headers and AD. |
| H17 | Is `hv_gic_set_spi` / `hv_gic_send_msi` safe to call from non-vCPU threads? | Inferred from the *absence* of an owning-thread note (`hv_gic.h:51-77`), versus explicit notes on the per-vCPU GIC calls. |

### Arm (Section 2)

| # | Item |
|---|---|
| A1 | DDI0487 M.c citations are by section and rule ID from the HTML edition. PDF page numbers were not retrieved. |
| A2 | The conventional timer PPI numbers (sec-phys 13, phys 14, virt 11, hyp-phys 10, hyp-virt 12) rest on the binding example, KVM defaults and HVF's reserved INTIDs. The Arm SBSA recommended-PPI table was not retrieved. |
| A3 | DFSC "address size fault, level -1": Linux `esr.h:134` gives 0x25, but DDI0487 M.c D24.2.45 and `fault.c:954` give 0x29. Not re-checked against a second Arm ARM issue. |
| A4 | PL031: treating a rewrite of RTCCR=1 as a no-op is a recommendation. It reconciles TRM §3.3.4 (a write after enable resets the counter) with Linux writing EN=1 at every probe. |

### Linux boot, DT and FDT (Section 3)

| # | Item |
|---|---|
| B1 | The full IEEE 1275 PCI `phys.hi` layout (`npt000ss bbbbbbbb dddddfff rrrrrrrr`, n/p/t bits) comes from a source outside the allowed set. Linux decodes only ss (bits 25:24) and p (bit 30); bus/dev/fn positions come from the DTSpec §2.4.4 example. |
| B2 | Some binding schemas live in the out-of-tree dtschema project, so their semantics were taken from Linux code: /chosen (`rng-seed`, `kaslr-seed`, `linux,initrd-*`), pci-bus-common / pci-host-bridge, and `msi-map`/`iommu-map`. |
| B3 | The kernel release in which the Image `text_offset` became 0 (this tree emits 0: head.S:63). |

### KVM (Section 4)

| # | Item |
|---|---|
| K1 | PROBE values (ioctl numbers, struct sizes and offsets, register IDs) came from compiling the 7.2-rc4 uapi headers with clang for arm64 LP64 on macOS. They were cross-checked against `_IOC` hand-derivation and the documented PC register ID. They were not verified on a Linux target. |
| K2 | Do guest_memfd and `KVM_PRE_FAULT_MEMORY` work for non-confidential arm64 VMs? api.rst says "Architectures: none" (api:6417, 6477), but arm64 selects `KVM_GUEST_MEMFD` (arch/arm64/kvm/Kconfig:39). |
| K3 | "ioeventfd only fires for ISV=1 writes" is inferred from control flow in arch/arm64/kvm/mmio.c:174-231. It is not documented. |
| K4 | `KVM_SET_MP_STATE` with SUSPENDED: api.rst:1612-1616 lists only STOPPED and RUNNABLE for arm64, while arch/arm64/kvm/arm.c:808-810 and api.rst:7017-7020 accept or require SUSPENDED. |
| K5 | Nothing was executed on a Linux KVM host. All KVM behaviour comes from reading documentation and source. |
