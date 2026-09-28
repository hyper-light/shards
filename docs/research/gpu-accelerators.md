# GPU and training-accelerator access for shards microVMs

Research date: 2026-09-28. Status: findings plus recommendations. No code.

## 1. Scope

This document covers first-class GPU and accelerator access for agents running in containers inside shards microVMs. It covers two host types:

- **Linux/KVM** (x86_64 and arm64): NVIDIA, AMD and Intel GPUs, Gaudi, RDMA NICs, NVLink.
- **macOS/Hypervisor.framework** (Apple Silicon).

It answers Q1–Q7 and adds the requested device assignment and isolation controls (§2.6). Everything is judged against the hard targets: ≤5 ms start through lazily mapped copy-on-write (CoW) snapshot memory, minimal per-VM memory, a rootless stack, and nothing extra for users to install.

**Citation tags**

Local trees, cited as `path:Lstart-end`:
- `linux:` Linux 7.2-rc4 @1590cf032971
- `fc:` Firecracker @edb60617c (v1.16.0-dev-751)
- `krun:` libkrun @1f5dd028

Remote source, pinned to a commit and fetched 2026-09-28:
- `ch:` cloud-hypervisor @8e2e2c2
- `kata:` @6eea374
- `crosvm:` @f959ff9
- `rutabaga:` rutabaga_gfx @ec60ee1
- `qemu:` @81ce3a8
- `moby:` @c3c2a9e
- `runc:` @41b7477
- `ocg:` opencontainers/cgroups @30a2293
- `oci:` runtime-spec @6999a89
- `cspec:` compose-spec @914ec15
- `cdi:` container-device-interface @cb98bcf
- `lnc:` libnvidia-container @24a848d9
- `mvk:` MoltenVK @b0753eb
- `vrend:` virglrenderer @881294c
- `hbkrun:` libkrun/homebrew-krun @3088d4f

Papers are cited as `[AuthorYY §/Fig./Table]`. Vendor docs use short tags, defined in §5.

**Source limits**
- The PCIe Base Specification is PCI-SIG members-only and could not be retrieved. PCIe semantics are cited through Linux docs and source that reference it.
- vCUDA, GViM and rCUDA (HPCS'10) could not be retrieved, so they are dropped. rCUDA is represented by the CCPE'15 journal paper.
- For Medusa and TunneLs, only the abstracts are readable (ACM DL returned 403). They are marked *abstract only*.
- Preprints are labeled.

## 2. Findings

### 2.1 GPU virtualization approaches (Q1)

| Approach | Isolation and sharing | Performance evidence | Memory and start consequence |
|---|---|---|---|
| **VFIO passthrough** (whole GPU) | IOMMU-enforced DMA isolation. The unit of ownership is the IOMMU *group*, which ACS gaps or bridges can enlarge [linux: Documentation/driver-api/vfio.rst:L52-71]. No sharing. | With plain assignment, I/O-intensive guests reach only 60–65% of bare-metal throughput; ELI's exit-less interrupts reach 97–100% [Gordon12 §1]. Firecracker's authors: virtio "will not yield the near-bare-metal performance offered by PCI pass-through" [Agache20 §5.3]. | Pins all guest RAM (§2.3) |
| **SR-IOV VF** | Each VF has its own routing ID and BARs [linux: Documentation/PCI/pci-iov-howto.rst:L18-29]. VFs are the "best indicator of 'well behaved'" devices [vfio.rst:L684-686]. NVIDIA: "SR-IOV virtual functions enable full IOMMU protection" [nv-vgpu §1.3.1]. AMD MxGPU: "independent copies of memory space, interrupts, and DMA streams" [amd-mxgpu]. | Time-sliced vGPU processes "are scheduled to run in series" [nv-vgpu §2.1.1.1] | Pins all guest RAM |
| **MIG** (Ampere+) | Up to 7 GPU instances with "separate and isolated paths through the entire memory system". Offers QoS "with fault isolation for … VMs, containers or processes" [nv-mig: introduction]. | Workloads run "with predictable throughput and latency, with the same L2 cache allocation and DRAM bandwidth" [nv-mig: introduction]. Reconfiguration takes "about 4 seconds" and "requires stopping all applications" [Li22 §3, §2.1]. | Splitting one GPU's MIG slices across VMs requires MIG-backed vGPU, which is licensed [nv-mig: virtualization; nv-vgpu §2.1.1.2, §2.1]. |
| **Mediated pass-through** (mdev) | Privileged operations are trap-and-emulated, so a VM cannot "map unauthorized graphics memory" [Tian14 §3.5]. | gVirt: "up to 95% native … scale well up to 7 VMs" [Tian14 Abstract]. gScale: 15 vGPUs at up to 81% of gVirt [Xue16 Abstract, §5]. GPUvm para-virtualization is 2–3× slower than pass-through [Suzuki14 Abstract]. | **Pins on demand**: mdev drivers pin per page (`vfio_pin_pages`) [linux: Documentation/driver-api/vfio-mediated-device.rst:L252-265; linux: drivers/gpu/drm/i915/gvt/kvmgt.c:L153]. type1 skips pinning at MAP_DMA when no IOMMU-backed domain exists [linux: drivers/vfio/vfio_iommu_type1.c:L1786-1790]. |
| **API remoting** | "can have poor isolation … because the hypervisor is bypassed" [Yu20 §2.1]. Isolation reuses GPU process isolation [Yu20 §3.4]. | AvA: 2.4% slowdown for TensorFlow and 5.6% for CUDA, but a geomean of 79.6% on call-intensive CUDA [Yu20 §1, §6.2]. 28% overhead on 1 ms calls [Yu20 §6.3]. rCUDA: 97.7% of IB bandwidth, ~1% on compute-bound work [Reano15 §5]. DGSF remoting costs +28% on one workload, yet still wins end to end [Fingler22 §VIII-B]. | No device in the VM, no pinning |
| **Paravirtual virtio-gpu 3D** | The host driver executes guest command streams. Paradice, which forwards at the device-file boundary (conceptually the boundary DRM native context uses), fault-isolates the driver in a driver VM [AmiriSani14 §4.1]. | Paradice: 35 µs added per call, 2 µs with polling [AmiriSani14 §6.1.1]. **No peer-reviewed evaluation of virgl, Venus, gfxstream or native context was found** (searched 2026-09-28). | Blob memory lives on the host and is mapped on demand into a shm window. Guest RAM is not pinned. |

**virtio-gpu ground truth.** "3D mode will offload rendering ops to the host gpu" [virtio1.3 §5.7]. Features: VIRGL, EDID, RESOURCE_UUID, RESOURCE_BLOB, CONTEXT_INIT [§5.7.3]. RESOURCE_MAP_BLOB maps a host blob into the host-visible shm region (`SHM_ID_HOST_VISIBLE=1`) [§5.7.5, §5.7.6.9]. Capsets: VIRGL, VIRGL2, GFXSTREAM, VENUS, CROSS_DOMAIN [§5.7.6.8]. The DRM native-context capset (6) is not in v1.3 CSD01; it is defined in Linux uapi [linux: include/uapi/linux/virtio_gpu.h:L318-323] and rutabaga [rutabaga: src/rutabaga_utils.rs:L213-221].

Venus is "a Virtio-GPU protocol for Vulkan command serialization". A Linux host needs Vulkan 1.1 plus `VK_KHR_external_memory_fd`, and Venus "violates the spec … to support vkMapMemory" [mesa-venus]. Native-context renderers exist for amdgpu, panfrost and i915 (all experimental), asahi and msm; there is **no NVIDIA renderer** [vrend: meson_options.txt:L82-92]. Intel native context landed in Mesa 26.1 [mesa-26.1-rn].

### 2.2 Firecracker and other VMMs (Q2)

**Firecracker**
- The NSDI paper: it "does not emulate legacy devices nor PCI" [Agache20 §1.1], and "We removed device drivers including USB and GPU" when forking crosvm [Agache20 §3].
- The current tree adds an optional virtio-PCI transport through `--enable-pci` (v1.13.0, #5364) [fc: CHANGELOG.md:L463-467; fc: src/firecracker/src/main.rs:L278-280].
- It adds developer-preview hotplug of virtio-block, pmem and net only, with "No automatic guest notification" (v1.16.0) [fc: docs/device-hotplug.md:L9-29].
- `src/` contains no VFIO code. The only GPU token is the generated constant `VIRTIO_ID_GPU` [fc: src/vmm/src/devices/virtio/generated/virtio_ids.rs:L32].
- Stated targets are ≤5 MiB VMM overhead and ≤125 ms to guest init [fc: SPECIFICATION.md:L24-41].

**cloud-hypervisor**
- VFIO hotplug via `ch-remote add-device`; ACPI-only on AArch64 [ch: docs/hotplug.md:L158-186].
- GPUDirect P2P is opt-in (`x_nv_gpudirect_clique`) because root-port P2P routing "is optional" [ch: docs/vfio.md:L129-144].
- "VFIO pins the entire guest memory"; 2 MiB pages "speed up VM boot time" [ch: docs/iommu.md:L151-167].
- VFIO snapshots only via migration-v2 variant drivers (e.g., mlx5); generic `vfio_pci` is "non migratable" [ch: docs/snapshot_restore.md:L202-206; ch: docs/vfio.md:L205-212].

**Kata**
- NVIDIA: "Cold-plug is by design the only supported mode" [kata: docs/use-cases/NVIDIA-GPU-passthrough-and-Kata-QEMU.md:L98-101].
- **Two-level CDI**: host CDI resolves `/dev/vfio/devices/vfio0`; guest init loads NVIDIA modules and runs `nvidia-ctk cdi generate`; the agent applies `containerEdits` [same file L103-130, L178-206].
- Recommended `create_container_timeout`: 1200 s [L300-306].

**crosvm**
- `--vfio …,iommu=viommu|coiommu|pkvm-iommu|off`, `--coiommu unpin_policy=lru,…` [crosvm: src/crosvm/cmdline.rs:L2133-2146, L936-951]; `crosvm vfio add/remove` hotplug [L485-518].
- Snapshotting is "highly experimental … 100% not supported" [crosvm: docs/book/src/architecture/snapshotting.md:L3-5].

**QEMU**: VFIO migration is pre-copy plus stop-copy; with IOMMU dirty tracking "all pages are perpetually marked dirty" [qemu: docs/devel/migration/vfio.rst:L10-54, L127-145].

**libkrun** has virtio-gpu (Venus and native context) [krun: README.md:L59]. It has no VFIO and no VM snapshot (grep).

### 2.3 The latency and memory conflict (Q3)

**1. Pinning is total and long-term**
- type1 pins every page of each DMA map with `FOLL_LONGTERM` [linux: drivers/vfio/vfio_iommu_type1.c:L597]. This is charged to `RLIMIT_MEMLOCK` unless the process holds CAP_IPC_LOCK [same file L1588, L1659].
- iommufd pins in the same way [linux: drivers/iommu/iommufd/pages.c:L962, L1388-1391].
- The literature agrees:
  - Device assignment "requires pinning all of the guest's pages, thereby disallowing memory overcommitment" [Amit11 Abstract].
  - It forces the hypervisor to "statically pin the entire guest memory" [Tian20 Abstract].
  - Static pinning is standard practice for SR-IOV [Lesokhin17 §2.2].

**2. Pinning destroys CoW snapshot memory**
- type1 pins writable maps with `FOLL_WRITE` [linux: drivers/vfio/vfio_iommu_type1.c:L593-594], which faults pages in with `FAULT_FLAG_WRITE` [linux: mm/gup.c:L1096-1097]. On a private file mapping, that write fault becomes `do_cow_fault` [linux: mm/memory.c:L5995-5996].
- cloud-hypervisor documents the exact effect on restored VMs: a VFIO device hot-plugged after restore "write-faults every guest page … page-cache sharing is lost … host memory use grows to the eager-copy level. The same applies to `ondemand` restores" [ch: docs/snapshot_restore.md:L181-185]. CoW restore falls back to an eager copy whenever passthrough is configured [L146-161].

**3. Reset and driver initialization cost 20× to 1000× the budget**
- vfio-pci resets the function when the device is enabled [linux: drivers/vfio/pci/vfio_pci_core.c:L607-608]; the FLR path sleeps 100 ms, citing PCIe r4.0 §6.6.2 [linux: drivers/pci/pci.c:L4382-4386].
- Without persistence, CUDA jobs see "long load times … on the order of seconds" [nv-pers: Background].
- Measured: CUDA runtime init 3.2 s average (2.8–3.6 s), ~303 MB per context [Fingler22 §V-C]; task init 5,530 of 5,787 ms total (T4) [Bai20 §4.1, Table 1]; context creation 3.1 s vs 1.7 s data copy for Llama2-13B [Wei25 §2.3]; ≈500 ms per CUDA context [Zhang25 App. A.1]; V100 cold starts of 8 s (ResNet-152), 25 s (Stable Diffusion), 61 s (Llama2-13B) [Yu25 §2.2, Table 1].

**4. Techniques that address it**

Pre-warmed pools:
- PipeSwitch: 3.6–6.6 ms task startup with standby workers, each holding "a few hundred MB GPU memory" [Bai20 Abstract, §4.4].
- PhoenixOS pre-creates CUDA/cuBLAS/NCCL contexts and launches Llama2-13B in 622 ms; serverless startup is 24× and 16× better than cuda-checkpoint and Singularity [Wei25 §1, §6, §8.1].
- DGSF pools pre-initialized API servers [Fingler22 §V-C]; BlitzScale pools contexts and NCCL [Zhang25 App. A.1].

Loading and materialization: mmap page-fault loading is slow ("112K" faults for LLaMA-2-7B), while chunked direct I/O is 6–8.2× faster than PyTorch [Fu24 §7.2]. Medusa materializes CUDA graphs and KV-cache init offline: −42.5% model-loading latency, −53% tail TTFT [Zeng25, abstract only].

GPU checkpoint/restore:
- **cuda-checkpoint**: driver ≥550; copies device memory into driver-managed host allocations; no UVM or IPC memory; restore onto "the same chip type"; Linux only [nv-ckpt]. It "cannot fully utilize the PCIe bandwidth" [Wei25 §8].
- CRIUgpu on H100 (preprint, not peer-reviewed): GPT-2 Small checkpoint 4.9 s / restore 2.5 s; GPT-2 XL 28 s / 11 s; no NCCL [Stoyanov25 §5.2, §4].
- CRAC: C/R under 1 s, ~1% overhead [Jain20 Abstract, §IV]. Singularity (preprint) restores by replaying state-changing calls into a fresh device proxy, with <3% steady-state overhead for most models [Shukla22 §4.5, §7.1].
- AMD: the CRIU amdgpu plugin saves VRAM/GTT buffer contents; restore needs the same number and type of GPUs [criu-amdgpu].

Device-state save for VM snapshot:
- The VFIO migration v2 FSM defines STOP_COPY, RESUMING and optional PRE_COPY/P2P states [linux: include/uapi/linux/vfio.h:L1014-1040, L1078-1100].
- In 7.2-rc4 it is implemented by hisilicon, mlx5, pds, qat, virtio and **xe** (Intel SR-IOV VFs) [linux: drivers/vfio/pci/*; drivers/vfio/pci/xe/Kconfig]. Not by `nvgrace-gpu`, and no NVIDIA discrete-GPU variant exists. **A passthrough NVIDIA GPU VM cannot be snapshotted upstream.**
- vGPU migration requires the same GPU type and NVLink topology, and is disabled with UVM, debuggers or profilers [nv-vgpu §5.3]. mlx5 VF migration needs Linux ≥6.7, Ethernet mode, ≤4 VFs in parallel [doca-lm].

On-demand pinning:
- vIOMMU lets the host pin "only these pages" the guest maps. As % of 10 GbE line rate (same-core / side-core / bare metal): strict protection 10 / 30 / 43; optimistic teardown 82 / 100 / 100 [Amit11 §1, Table 1].
- coIOMMU (cooperative DMA tracking table): <3% throughput loss; peak pinned 174 MB (0.4% of a 32 GB guest); 49 GB free vs 8.8 GB under static pinning; GPU case −4.5% FPS because the app maps ~240 MB at launch [Tian20 §5.1-5.3]. crosvm ships it (§2.2).
- Caveats: guests default to IOMMU passthrough mode, so they expose no DMA information [Tian20 §2.2]; virtio-iommu v1.3 reports faults but has no page requests [virtio1.3 §5.13.6.9].

PRI/ATS:
- SVA devices using ATS+PRI need no pinning [linux: Documentation/arch/x86/sva.rst:L15-29, L251-266].
- DMA page faults cost 3–80× a CPU fault ("up to hundreds of microseconds"), and "most commodity devices do not support" them [Tian20 §2.2]; a NIC fault costs 220 µs [Lesokhin17 §4, Fig. 3].
- iommufd's PRI-backed FAULT object [linux: Documentation/userspace-api/iommufd.rst:L66-71] is rejected on nesting-parent (stage-2) HWPTs [linux: drivers/iommu/iommufd/hw_pagetable.c:L131-133]. No surveyed VMM demand-pins guest RAM via PRI; feasibility is UNVERIFIED.

Live update: `VFIO_DMA_MAP_FLAG_VADDR` re-points existing DMA maps to a new process address without unmapping them [linux: include/uapi/linux/vfio.h:L1625-1631].

### 2.4 macOS on Apple Silicon (Q4)

**Platform**
- Hypervisor.framework maps memory with `hv_vm_map`. Its only device entitlement is USB capture. There is no PCI or GPU passthrough API [apple-hv].
- Virtualization.framework's `VZVirtioGraphicsDeviceConfiguration` configures a virtio graphics device for a Linux VM. The page documents scanout configuration and says nothing about 3D acceleration [apple-vz].
- ParavirtualizedGraphics gives Metal-accelerated graphics to **macOS guests** only [apple-pvg].

**The libkrun path**
- Architecture: virtio-gpu → rutabaga_gfx → virglrenderer [krun: AGENTS.md:L81; krun: src/devices/Cargo.toml:L45].
- Capsets: cross-domain is always on; virgl is on unless `NO_VIRGL`; Venus and DRM are opt-in [krun: src/devices/src/virtio/gpu/virtio_gpu.rs:L1139-1160].
- On macOS it accepts only DMABUF-type exported blobs. These are mapped into the guest shm window through a worker that calls `hv_vm_map` with RWX permissions [virtio_gpu.rs:L935-984; krun: src/libkrun/src/vmm/macos/vstate.rs:L133-151; krun: src/hvf/src/lib.rs:L312-330].
- krunkit's renderer is a **fork**, slp/virglrenderer `0.10.4e-krunkit`. It has `depends_on "molten-vk"` and is built with `-Dvenus=true -Drender-server=false` [hbkrun: Formula/virglrenderer-krun.rb:L4, L18, L24-25].
- MoltenVK is "an almost-complete subset of the Vulkan 1.4" and is "not fully compliant" [mvk: README.md:L63-64, L397].
- Its extension list lacks `VK_KHR_external_memory_fd` [mvk: Docs/MoltenVK_Runtime_UserGuide.md, 0 matches], which Mesa lists as the Linux-host Venus requirement. **Inference:** the macOS Venus path depends on out-of-upstream virglrenderer changes.

**What ML training can use from a Linux guest**
- CUDA on macOS ended with 10.2 [cuda-rn10.2 §2.1].
- MLX "is only available on devices running macOS >= 14.0". Its Linux options are CUDA or CPU [mlx-install].
- PyTorch MPS is a macOS backend [torch-mps].
- The PyTorch Vulkan backend "is no longer maintained" and targeted mobile inference [torch-vk].
- **Net: the guest gets Vulkan compute only. No CUDA, no Metal, no MLX-GPU.**
- We found no peer-reviewed evaluation of this path (§2.1).

### 2.5 Container-level GPU exposure with Docker parity (Q5)

**Docker `--gpus`**
- Requires the NVIDIA runtime; forms `all`, `device=GPU-<uuid>`, `'"device=0,2"'` [docker-run: L1130-1157].
- moby maps a DeviceRequest to CDI kind `nvidia.com/gpu` when `nvidia-cdi-hook` exists; otherwise it sets `NVIDIA_VISIBLE_DEVICES`/`NVIDIA_DRIVER_CAPABILITIES` and injects a prestart hook [moby: daemon/devices_nvidia_linux.go:L42-61, L108-157]. Capabilities: compute, compat32, graphics, utility, video, display [L29-36]. Count and DeviceIDs are exclusive [L161-178]. AMD: `amd.com/gpu` CDI, then `AMD_VISIBLE_DEVICES` [moby: daemon/devices_amd_linux.go:L18-74].
- compute is "required for CUDA and OpenCL", utility "for using nvidia-smi and NVML"; default `utility,compute` [nv-ctk: docker-specialized].

**CDI**
- The spec is v1.1.0, with fully qualified names `vendor.com/class=name` [cdi: SPEC.md:L11, L197-204].
- `containerEdits` covers env, deviceNodes (permissions and uid/gid), mounts, hooks, additionalGids and netDevices. Device-level edits apply "only if the matching device is requested" [SPEC.md:L225-263].
- Docker enables CDI by default from Engine 28.3.0 [docker-run: L950-951]. The CDI README says 28.2.0 [cdi: README.md:L177]. The two sources conflict.
- NVIDIA names: `nvidia.com/gpu=0`, `=1:0` (a MIG device), `=all`. MIG reconfiguration requires regenerating the spec [nv-ctk: cdi-support].

**Compose**
- `gpus` (v2.30.0) is "a device request with an implicit `gpu` capability" [cspec: 05-services.md:L973-994].
- `devices` accepts CDI names [L482-498], and `device_cgroup_rules` is supported [L470-480].
- In `deploy.resources.reservations.devices`, `capabilities` is required, and `count` and `device_ids` "are exclusive" [cspec: deploy.md:L139-205].

**OCI and runc**
- Containers "MAY NOT access any device node that is not … explicitly referenced". The device allow-list is applied in order, and the spec's example starts with deny-all [oci: config-linux.md:L145-149, L402-415].
- runc bind-mounts device nodes when it runs in a user namespace [runc: libcontainer/rootfs_linux.go:L958-973].
- runc silently skips eBPF device rules in a user namespace ("ideally we would be blocking device access for rootless containers anyway") [ocg: devices/v2.go:L30-38].

**Kernel constraints that shape a rootless engine**
- The cgroup v2 device controller is **BPF-only** [linux: Documentation/admin-guide/cgroup-v2.rst:L2728-2749].
- Loading `BPF_PROG_TYPE_CGROUP_DEVICE` requires bpf_capable plus CAP_NET_ADMIN in the init namespace, or a **BPF token** [linux: kernel/bpf/syscall.c:L2840-2862, L3038-3044; linux: kernel/bpf/token.c:L17-28].
- A token requires a bpffs mounted by a privileged process with `delegate_cmds/maps/progs/attachs` options, and is created inside a non-init user namespace [linux: kernel/bpf/inode.c:L986-989; token.c:L138-151].
- `mknod` needs CAP_MKNOD in the init user namespace [linux: fs/namei.c:L5115-5117].
- Superblocks owned by a user namespace are `SB_I_NODEV` [linux: fs/super.c:L358-359; linux: fs/namei.c:L4230-4233].
- Docker rootless supports cgroup limits only with cgroup v2 plus systemd ("typically, only `memory` and `pids`" are delegated) and otherwise ignores cgroup-related flags [docker-rootless].
- NVIDIA rootless mode requires `no-cgroups` [nv-ctk: install-guide]. libnvidia-container's cgroup v2 path rewrites BPF_CGROUP_DEVICE programs, which needs privilege [lnc: src/nvcgo/internal/cgroup/v2.go:L110-183].

**Delegable limits**
- `dmem.max/min/low` limit per-device memory. In 7.2-rc4, only amdgpu and xe register regions [cgroup-v2.rst:L2844-2874; linux: drivers/gpu/drm/amd/amdgpu/amdgpu_vram_mgr.c; linux: drivers/gpu/drm/xe/xe_ttm_vram_mgr.c].
- NVIDIA MPS v3 maps its hard and soft memory limits onto `dmem.max` and `dmem.min`. It needs kernel ≥6.14 and CUDA 13.4+, and "MIG is explicitly unsupported" [nv-mps §1.1.10].
- `rdma.max` limits HCA handles and objects [cgroup-v2.rst:L2752-2764].
- Delegated subtrees cannot escape parent limits [cgroup-v2.rst:L537-565].

### 2.6 Device assignment and isolation controls

| Granularity | Assign to VM | Assign to container (in VM) | Memory / fault isolation | Performance isolation |
|---|---|---|---|---|
| Whole GPU | VFIO; the whole IOMMU group; NVLink peers go to the same VM [nv-vgpu Ch.3] | CDI `nvidia.com/gpu=<idx\|UUID>`; `--gpus device=` | IOMMU; per-VM. Reset between tenants (FLR ≥100 ms, §2.3) | Exclusive |
| SR-IOV VF (vGPU, MxGPU, xe, ConnectX) | VFIO VF. A GPU hosts vGPUs or is passed through, but "cannot do both at the same time" [nv-vgpu Ch.3]. MxGPU VFs: SPX 1, DPX 2, CPX 8 [amd-part]. ConnectX: ≤127 VFs/port [doca-sriov]. | CDI | "full IOMMU protection" [nv-vgpu §1.3.1] | Time-sliced vGPUs run "in series" [nv-vgpu §2.1.1.1]. MIG-backed vGPUs run in parallel [§2.1.1.2]. |
| MIG GI/CI | Passthrough GPU with MIG inside, or MIG-backed vGPU (licensed) [nv-mig: virtualization] | `nvidia.com/gpu=1:0` or `MIG-<UUID>`; `/dev/nvidia-caps` capability nodes [nv-mig: device-nodes; nv-ctk] | Per GI: memory QoS and error isolation. CIs "share memory and engines" [nv-mig: concepts, Table 3] | SMs, L2 and DRAM bandwidth partitioned; ≤7 [nv-mig]. No NCCL; no P2P across MIG [nv-mig: deployment-considerations]. |
| MPS | n/a (in-VM) | Env vars plus a shared MPS server | Isolated address spaces. A fatal fault is "reported to all the clients" on the affected GPUs [nv-mps §1.1.2.2.3]. Static SM partitions add "partial" error isolation (r610+) [§1.1.4.4.1]. | SM share "by percentage, not partitioning" [nv-mig Table 3]. Cache and memory are shared [Li22 §2.1]. |

**Interference and security**
- MPS co-location achieved up to 80% and 40% higher throughput than naive and MIG collocation, but MPS requires a single user [Robroek24 §1, §2.1.2 (EuroMLSys'24; arXiv text)].
- MIG's last-level TLB is shared, which gives a 31 kbps covert channel [Zhang23 abstract only].
- NVLink peer mapping enables a 3.95 MBps covert channel between GPUs [Dutta23 §I, §III (ISCA'23; arXiv text)].
- MPS co-location leaks model parameters through performance counters [Naghibijouybari18 Abstract, §5].
- **So MIG and MPS are resource and fault isolation, not side-channel isolation. Mutually distrusting agents need separate GPUs, or separate VMs with no NVLink peer mapping.**

**Selection**
- Prefer UUIDs to indices. Gaudi indices "may change after a system reboot", whereas module IDs are stable [gaudi-mt].
- AMD warns that environment variables "shouldn't be used for isolating untrusted applications" [rocm-iso]. Device-node exposure is the mechanism.
- `/dev/kfd` is "shared by all GPUs", so restrict per-GPU access through individual `renderD` nodes [rocm-docker].
- NVIDIA device files default to mode 0666 [nv-drv].

### 2.7 Other training hardware (Q6)

**RDMA**
- ConnectX supports ≤127 VFs per port, RoCE on VFs, and VM assignment of VFs as PCI devices [doca-sriov].
- `ib_uverbs` pins memory and enforces `RLIMIT_MEMLOCK`. Its device nodes are "safe for use by non-privileged processes" [linux: Documentation/infiniband/user_verbs.rst:L47-55, L68-75].
- On-demand paging (ODP/NPF, shipping in Mellanox InfiniBand NICs) removes pinning, at 215 µs (p50) to 464 µs (max) per 4 KB fault [Lesokhin17 §7, Table 4].
- CDI v1.1.0 `netDevices` covers RDMA netdevs [cdi: SPEC.md].

**GPUDirect / P2P**
- The IOMMU must be off or in 1:1 passthrough, and devices must "share the same upstream PCI Express root complex" [nv-gdr].
- Linux permits P2P below switches. Host-bridge P2P needs an allow-list [linux: Documentation/driver-api/pci/p2pdma.rst:L12-25].
- 7.2-rc4 iommufd can map VFIO PCI dma-bufs (device BARs) into an IOAS through `IOMMU_IOAS_MAP_FILE`, which enables guest-physical P2P [linux: include/uapi/linux/iommufd.h:L222-247; linux: drivers/vfio/pci/vfio_pci_dmabuf.c].
- GPUDirect RDMA through VFIO is not documented by NVIDIA. It needs measurement.

**NVLink / NVSwitch**
- Full passthrough gives the guest both GPUs and NVSwitches. The guest runs the driver and Fabric Manager. The hypervisor maintains 16/8/4/2/1-GPU partitions and resets them [nv-fm].

**Other vendors**
- **AMD**: passthrough gives "the highest level of isolation" [rocm-iso]. MxGPU runs on KVM/QEMU for MI210X–MI355X and V710 [amd-mxgpu].
- **Gaudi**: "PCI passthrough is the only virtualization mechanism … no support for SR-IOV or MIG", at single-HPU granularity [gaudi-virt].
- **TPU**: accessible through Compute Engine, GKE and Vertex AI TPU VMs [gcp-tpu]. There is no evidence of a host-attachable training TPU (UNVERIFIED), so TPUs are out of scope.
- **arm64 Grace Hopper**: the `nvgrace-gpu` VFIO variant exposes cacheable device memory as a BAR, plus a non-cacheable region for MIG [linux: drivers/vfio/pci/nvgrace-gpu/main.c:L17-23].

## 3. Implications for shards (ranked)

**R1. Make GPUs zero-cost when unused. (Must.)**
- The default VM class has no PCI root complex, no VFIO, no GPU drivers in the guest kernel, and no pinned memory. ≤5 ms CoW restore is then untouched.
- Why: any assigned device pins all RAM and breaks CoW (§2.3.1–2). An FLR alone is 100 ms. Driver and CUDA initialization take seconds (§2.3.3).
- Cost: a second guest-image flavor ("gpu"). NVIDIA modules "must be used with … user-space … from a corresponding" release [nv-okm], so the gpu image pins one driver version, and containers inherit it through CDI.

**R2. Linux: build VFIO on iommufd plus device cdev, not the legacy type1 container ("intended to be deprecated") [linux: Documentation/driver-api/vfio.rst:L245-267]. (Must.)**
- Build: PCIe root ports with hotplug; 64-bit MMIO windows for large BARs (ch's example GPU has a 64 GiB BAR) [ch: docs/vfio.md:L173-195]; MSI-X over irqfd; hugepage-backed guest RAM [ch: docs/iommu.md:L162-167]; dma-buf P2P maps; migration v2 for mlx5/xe VFs.
- Rootless conflict: one-time root host prep is needed. Bind vfio-pci [vfio.rst:L130-138]; chown `/dev/vfio/devices/vfioX` [vfio.rst:L319-322]; grant `/dev/iommu` (mode 0660) [linux: drivers/iommu/iommufd/main.c:L743]; raise `RLIMIT_MEMLOCK` to cover guest RAM [linux: drivers/vfio/vfio_iommu_type1.c:L1588; iommufd/pages.c:L962]; create VFs via `sriov_numvfs` [linux: Documentation/PCI/pci-iov-howto.rst:L43-77]; set MIG mode ("super-user privileges" on A100/A30) [nv-mig: deployment-considerations]. Ship this as `shards host-prep`; the VMM then runs unprivileged.

**R3. GPU VM "start" means assignment from a warm pool, not snapshot restore. (Must for GPU.)**
- A host broker keeps VFIO fds open (reset already done) and GPU VMs pre-booted: driver loaded, persistence on, CUDA/cuBLAS/NCCL contexts pre-created (PhoenixOS, PipeSwitch, BlitzScale, DGSF; §2.3.4). An agent gets a container in such a VM. Target: agent-to-first-kernel ≈ container start (unmeasured, E3).
- Cost: pooled VMs hold pinned RAM plus ~300–755 MB of device memory per pre-created runtime [Fingler22 §V-C]. Returning a GPU to the pool needs a reset and scrub, off the critical path.
- A passthrough NVIDIA VM cannot be snapshotted (§2.3.4). Use process-level C/R *inside* warm GPU VMs (cuda-checkpoint/CRIU, CRIU-amdgpu) for preemption and migration, and accept restores in seconds [Stoyanov25; Wei25].

**R4. Default-deny isolation with a CDI pipeline, enforced rootlessly. (Must.)**
- VMs get only explicitly assigned VFIO devices, as whole IOMMU groups. Guest init generates CDI at boot, as Kata's NVRC does, and regenerates it on MIG reconfiguration. The engine implements `--gpus`, `--device <cdi>` and Compose `gpus`/`devices`/`deploy…devices` through CDI (§2.5).
- Enforcement: (a) bind-mount only assigned nodes, including `/dev/nvidia-caps` entries only for the assigned GI/CI; no-mknod and nodev are kernel-guaranteed in the user namespace (§2.5). (b) Defense in depth: guest init, the only privileged step, mounts a delegating bpffs so the rootless engine can load device filters through a BPF token. (c) Apply `dmem.max` (amdgpu, xe, MPS v3) and `rdma.max` in delegated cgroups.
- Tell users that MIG and MPS are not side-channel boundaries (§2.6).

**R5. Fast-start remoting tier: vsock API remoting to a host GPU daemon (AvA/DGSF style) for inference and bursty agents. (Should.)**
- Benefit: no device, no pinning, ≤5 ms VM start kept; the daemon's pre-initialized contexts remove the 3.2 s CUDA init [Fingler22 §V-C].
- Costs: call-heavy code loses up to 79.6% geomean [Yu20 §6.2]; the isolation boundary becomes the host daemon executing guest kernels [Yu20 §2.1]; closed-CUDA API coverage. Not the training tier.

**R6. macOS: build virtio-gpu with Venus and cross-domain, modeled on libkrun. (Should.)**
- rutabaga/virglrenderer, blob maps through `hv_vm_map`, and virglrenderer (the krun fork) plus MoltenVK shipped in-repo. It exposes Vulkan compute only (§2.4).
- Blob memory is host-side and mapped on demand, so guest RAM stays unpinned and lazy. Whether renderer contexts survive a VM snapshot is unknown (E6).
- For Apple-GPU training the only fast APIs are host-side (MLX, MPS). Offer an R5-style host broker on macOS and state the isolation trade-off explicitly.

**R7. Linux paravirtual option: native context (AMD, Intel, Qualcomm) and Venus. (Could.)** No pinning, suits graphics and Vulkan agents. No NVIDIA renderer exists and no peer-reviewed performance data.

**R8. Multi-GPU, RDMA and memory research. (Could.)** Assign NVLink islands (GPUs plus NVSwitches) to one VM with Fabric Manager in the guest, and ConnectX VFs per VM. GPUDirect in a VM depends on topology and dma-buf P2P (E9). On-demand pinning (coIOMMU as in crosvm, or PRI) is the only path to lower GPU-VM memory without giving up passthrough (E5).

**Constraint conflicts, stated plainly**
- GPU VMs cannot meet "≤5 ms from snapshot" or "minimal memory"; only warm-pool assignment can approach the latency target.
- "Rootless and install nothing" conflicts with host device prep and with vendor host software: vGPU needs the NVIDIA vGPU manager plus a license [nv-vgpu §2.1]; MxGPU needs AMD's host driver. Redistribution terms for vendor guest drivers are UNVERIFIED.

## 4. Open questions needing our own measurement

- **E1. Pin cost.** Time `IOMMU_IOAS_MAP` for 1–64 GiB guest RAM (4K/2M/1G backing), from fresh anonymous memory vs a MAP_PRIVATE snapshot file. Record latency, RSS growth and lost sharing (`smaps` Private_Dirty), on x86_64 and arm64.
- **E2. Reset-to-ready.** vfio cdev open (reset) → guest `nvidia`/`amdgpu`/`xe` probe → first CUDA/HIP kernel. Vary persistence on/off and cold-plug vs hotplug; p50/p99 over 100 runs per GPU model.
- **E3. Warm-pool assignment.** `docker run --gpus device=<uuid>` in a warm GPU VM → first kernel, with no pre-created context, a context pool, and context plus cuBLAS/NCCL pools. Record device memory held per pooled VM.
- **E4. In-VM C/R.** cuda-checkpoint and CRIU throughput for 1–80 GB GPU state vs PCIe link rate, same GPU vs another of the same chip type. Checks the PhoenixOS and CRIUgpu figures on our hardware.
- **E5. On-demand pinning with GPUs.** crosvm `iommu=coiommu` and virtio-iommu with NVIDIA, amdgpu and xe guests: pinned-footprint time series and training-step time vs static pinning. Probe PRI (`lspci -vvv`, iommufd FAULT).
- **E6. macOS Venus.** M5 Max: Vulkan GEMM TFLOPS and llama.cpp-Vulkan tokens/s in the guest vs host MoltenVK; `hv_vm_map` blob map/unmap latency; renderer RSS per VM; snapshot/restore with live Venus contexts.
- **E7. Remoting overhead.** vsock CUDA remoting prototype (Linux) and MLX broker (macOS): per-call latency histogram and training-step overhead for 3 model sizes vs passthrough and native.
- **E8. Isolation tests.** From a rootless container in a GPU VM, try reaching an unassigned GPU or MIG instance via `/dev/nvidiactl` alone, mknod, and `/proc/driver/nvidia/capabilities`. Repeat with and without the BPF-token filter, and after MIG reconfiguration without CDI regeneration.
- **E9. GPUDirect RDMA in a VM.** GPU↔ConnectX-VF bandwidth and latency with dma-buf P2P maps across ACS settings and topologies (same switch vs across root ports), vs a host-bounce baseline.
- **E10. GPU-VM memory floor.** VMM RSS, pinned RAM, guest driver RSS and `nvidia-persistenced` footprint for the smallest workable GPU VM (e.g., 2 GiB), vs the default VM class.

## 5. References

**Peer-reviewed papers** (full text retrieved via WebFetch unless marked)
- [Agache20] A. Agache et al. "Firecracker: Lightweight Virtualization for Serverless Applications." NSDI 2020. usenix.org/system/files/nsdi20-paper-agache.pdf
- [AmiriSani14] A. Amiri Sani, K. Boos, S. Qin, L. Zhong. "I/O Paravirtualization at the Device File Boundary." ASPLOS 2014. yecl.org/publications/amirisani2014asplos.pdf
- [Amit11] N. Amit, M. Ben-Yehuda, D. Tsafrir, A. Schuster. "vIOMMU: Efficient IOMMU Emulation." USENIX ATC 2011. usenix.org/legacy/event/atc11/tech/final_files/Amit.pdf
- [Bai20] Z. Bai, Z. Zhang, Y. Zhu, X. Jin. "PipeSwitch: Fast Pipelined Context Switching for Deep Learning Applications." OSDI 2020. usenix.org/system/files/osdi20-bai.pdf
- [Dutta23] S. B. Dutta et al. "Spy in the GPU-box: Covert and Side Channel Attacks on Multi-GPU Systems." ISCA 2023. Text read from arXiv 2203.15981v1.
- [Fingler22] H. Fingler et al. "DGSF: Disaggregated GPUs for Serverless Functions." IPDPS 2022. Accepted manuscript: par.nsf.gov/servlets/purl/10340800
- [Fu24] Y. Fu et al. "ServerlessLLM: Low-Latency Serverless Inference for Large Language Models." OSDI 2024. usenix.org/system/files/osdi24-fu.pdf
- [Gordon12] A. Gordon et al. "ELI: Bare-Metal Performance for I/O Virtualization." ASPLOS 2012. mulix.org/pubs/eli/eli.pdf
- [Jain20] T. Jain, G. Cooperman. "CRAC: Checkpoint-Restart Architecture for CUDA with Streams and UVM." SC 2020. ccs.neu.edu/home/gene/papers/sc20.pdf
- [Lesokhin17] I. Lesokhin et al. "Page Fault Support for Network Controllers." ASPLOS 2017. dants.github.io/papers/npf-asplos-2017.pdf
- [Li22] B. Li et al. "MISO: Exploiting Multi-Instance GPU Capability on Multi-Tenant Systems for Machine Learning." SoCC 2022. Text read from arXiv 2207.11428v3.
- [Naghibijouybari18] H. Naghibijouybari, A. Neupane, Z. Qian, N. Abu-Ghazaleh. "Rendered Insecure: GPU Side Channel Attacks are Practical." CCS 2018. cs.ucr.edu/~nael/pubs/ccs18.pdf
- [Reano15] C. Reaño et al. "Improving the User Experience of the rCUDA Remote GPU Virtualization Framework." CCPE 27(14), 2015. riunet.upv.es
- [Robroek24] T. Robroek, E. Yousefzadeh-Asl-Miandoab, P. Tözün. "An Analysis of Collocation on GPUs for Deep Learning Training." EuroMLSys 2024. Text read from arXiv v3.
- [Suzuki14] Y. Suzuki, S. Kato, H. Yamada, K. Kono. "GPUvm: Why Not Virtualizing GPUs at the Hypervisor?" USENIX ATC 2014. usenix.org/system/files/conference/atc14/atc14-paper-suzuki.pdf
- [Tian14] K. Tian, Y. Dong, D. Cowperthwaite. "A Full GPU Virtualization Solution with Mediated Pass-Through." USENIX ATC 2014. usenix.org/system/files/conference/atc14/atc14-paper-tian.pdf
- [Tian20] K. Tian, Y. Zhang, L. Kang, Y. Zhao, Y. Dong. "coIOMMU: A Virtual IOMMU with Cooperative DMA Buffer Tracking for Efficient Memory Management in Direct I/O." USENIX ATC 2020. usenix.org/system/files/atc20-tian.pdf
- [Wei25] X. Wei et al. "PhoenixOS: Concurrent OS-level GPU Checkpoint and Restore with Validated Speculation." SOSP 2025. ipads.sjtu.edu.cn/_media/publications/phos-sosp25.pdf
- [Xue16] M. Xue et al. "gScale: Scaling up GPU Virtualization with Dynamic Sharing of Graphics Memory Space." USENIX ATC 2016. usenix.org/system/files/conference/atc16/atc16_paper-xue.pdf
- [Yu20] H. Yu, A. M. Peters, A. Akshintala, C. J. Rossbach. "AvA: Accelerated Virtualization of Accelerators." ASPLOS 2020. oscarlab.github.io/papers/ava-asplos20.pdf
- [Yu25] M. Yu et al. "Torpor: GPU-Enabled Serverless Computing for Low-Latency, Resource-Efficient Inference." USENIX ATC 2025. usenix.org/system/files/atc25-yu.pdf
- [Zeng25] S. Zeng, M. Xie, S. Gao, Y. Chen, Y. Lu. "Medusa: Accelerating Serverless LLM Inference with Materialization." ASPLOS 2025. *Abstract only.*
- [Zhang23] Z. Zhang, T. N. Allen, F. Yao, X. Gao, R. Ge. "TunneLs for Bootlegging: Fully Reverse-Engineering GPU TLBs for Challenging Isolation Guarantees of NVIDIA MIG." CCS 2023. *Abstract only.*
- [Zhang25] D. Zhang et al. "BlitzScale: Fast and Live Large Model Autoscaling with O(1) Host Caching." OSDI 2025. usenix.org/system/files/osdi25-zhang-dingyan.pdf

**Preprints (not peer-reviewed)**
- [Shukla22] D. Shukla et al. "Singularity: Planet-Scale, Preemptive and Elastic Scheduling of AI Workloads." arXiv 2202.07848v2.
- [Stoyanov25] R. Stoyanov et al. "CRIUgpu: Transparent Checkpointing of GPU-Accelerated Workloads." arXiv 2502.16631v1.

**Specifications**
- [virtio1.3] OASIS VIRTIO v1.3, CSD01, 2023-10-06: §5.7 GPU, §5.13 IOMMU. docs.oasis-open.org/virtio/virtio/v1.3/virtio-v1.3.html
- CDI SPEC.md v1.1.0 (`cdi:`)
- Compose Specification (`cspec:`)
- OCI runtime-spec config-linux.md (`oci:`)

**Official documentation** (retrieved 2026-09-28)

NVIDIA:
- [nv-mig] MIG User Guide (updated 2026-09-11): pages introduction, concepts, supported-gpus, mig-device-names, device-nodes-and-capabilities, virtualization, deployment-considerations. docs.nvidia.com/datacenter/tesla/mig-user-guide/
- [nv-vgpu] vGPU Software User Guide r20.0–20.2 (2026-09-22). docs.nvidia.com/vgpu/latest/pdf/grid-vgpu-user-guide.pdf
- [nv-fm] Fabric Manager User Guide v2.3. docs.nvidia.com/datacenter/tesla/fabric-manager-user-guide/
- [nv-gdr] GPUDirect RDMA v13.4. docs.nvidia.com/cuda/gpudirect-rdma/
- [nv-ckpt] github.com/NVIDIA/cuda-checkpoint README, and CUDA Driver API §6.1 "CUDA Checkpointing" v13.4
- [nv-ctk] NVIDIA Container Toolkit docs: cdi-support, docker-specialized, install-guide. docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/
- [nv-drv] NVIDIA Linux driver README 595.99.02, FAQ "device files"
- [nv-okm] open-gpu-kernel-modules README 615.71.09
- [nv-mps] Multi-Process Service, Release 615 (2026-09-09): §1.1.2.2.3, §1.1.4.4.1, §1.1.10. docs.nvidia.com/deploy/pdf/CUDA_Multi_Process_Service_Overview.pdf
- [nv-pers] Driver Persistence, "Background". docs.nvidia.com/deploy/driver-persistence/background.html
- [cuda-rn10.2] CUDA 10.2 Release Notes §2.1

AMD:
- [amd-mxgpu] Getting started with MxGPU. instinct.docs.amd.com/projects/virt-drv/
- [amd-part] GPU partitioning. instinct.docs.amd.com/projects/virt-drv/
- [rocm-docker] ROCm Docker how-to
- [rocm-iso] ROCm 7.1.0 GPU isolation
- [criu-amdgpu] criu plugins/amdgpu/README.md

Intel, Google, NVIDIA networking:
- [gaudi-virt] Intel Gaudi 1.24.0 docs, Configuring_VMs_on_Gaudi
- [gaudi-mt] Intel Gaudi 1.24.0 docs, Multiple_Dockers_each_with_Single_Workload
- [gcp-tpu] Cloud TPU system architecture (updated 2026-09-24)
- [doca-sriov] NVIDIA DOCA 3.1.0 SR-IOV
- [doca-lm] NVIDIA DOCA 3.1.0 SR-IOV Live Migration

Apple:
- [apple-hv] developer.apple.com/documentation/hypervisor
- [apple-vz] VZVirtioGraphicsDeviceConfiguration
- [apple-pvg] ParavirtualizedGraphics

ML frameworks and Mesa:
- [mlx-install] ml-explore.github.io/mlx/build/html/install.html
- [torch-mps] docs.pytorch.org/docs/2.14/notes/mps.html
- [torch-vk] docs.pytorch.org/tutorials/unstable/vulkan_workflow.html
- [mesa-venus] docs.mesa3d.org/drivers/venus.html
- [mesa-26.1-rn] Mesa 26.1.0 release notes

Docker:
- [docker-run] docs.docker.com/reference/cli/docker/container/run.md
- [docker-rootless] docs.docker.com/engine/security/rootless/ (tips, troubleshoot)

**Source code**, at the commits listed in §1. Local trees: `/Users/adalundhe/Projects/{linux,firecracker,libkrun}`.
