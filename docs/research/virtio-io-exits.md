# VM exits, virtio I/O, interrupts, and native-vs-nested on macOS

Status: research note, 2026-09-28. The papers and specifications cited here were retrieved and read in full. Quoted numbers carry their section, table, or figure. Local sources were read at these versions: firecracker `edb60617c` (2026-09-25), libkrun `1f5dd028`, linux 7.2-rc4, and the macOS 26.4 SDK (`/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/.../Hypervisor.framework/Headers`, cited here as `hv_*.h`).

**Caveat.** Every peer-reviewed cycle count below comes from non-Apple hardware of 2012–2019. The only Apple-silicon numbers are our own preliminary measurements [PM]. §4 lists what we still need to measure.

## 1. Scope

- **Q1.** What do VM exits and traps cost on arm64 and x86, and what does nested virtualization cost? Should shards drive Hypervisor.framework (HVF) directly on macOS, or run KVM inside a Linux VM with EL2 enabled?
- **Q2.** What does the evidence say about virtio design, VIRTIO 1.3 ring features, and ways to avoid or reduce notifications and interrupts?
- **Q3.** How do virtio-mmio and virtio-pci compare? What does Firecracker do, and how does GPU passthrough affect the choice?
- **Q4.** How are interrupts delivered on arm64 (GICv3/v4, in-kernel vs userspace GIC, irqfd/ioeventfd)? What does HVF offer, and what does that mean for device threads?
- **Q5.** Which block backends should we use (io_uring, Firecracker's engines, macOS I/O semantics)?
- **Q6.** Which virtio-net features matter?
- **Q7.** What are virtio-vsock's semantics, and how does Firecracker implement it?

## 2. Findings

### 2.1 Exit costs (Q1)

| Operation (cycles) | ARM KVM, split-mode | ARM KVM, VHE-style redesign | x86 KVM | Source |
|---|---|---|---|---|
| Null hypercall | 6,500 / 6,413 | 752 | 1,300 / 1,437 | [Dall16 Tab. II], [Dall17 Tab. 2] |
| Exit handled in host kernel ("I/O Kernel") | 8,034 | 1,604 | 2,565 | [Dall17 Tab. 2] |
| Exit handled in host user space ("I/O User") | 10,012 | 7,630 | 6,732 | [Dall17 Tab. 2] |
| Virtual IPI | 13,121 | 2,526 | 3,102 | [Dall17 Tab. 2] |
| Guest notify to backend (ioeventfd path) | 6,024 | — | 560 | [Dall16 Tab. II "I/O Latency Out"] |
| Virtual interrupt completion | 71 | — | 1,556 | [Dall16 Tab. II] |

Test hardware: APM X-Gene and Xeon E5-2450 in [Dall16 §III]; AMD Seattle (Cortex-A57) and Xeon E5-2450 in [Dall17 §5]. The "VHE-style" column is el2Linux, which stands in for VHE [Dall17 §3.2, §5].

- **The trap itself is cheap on Arm; switching state is not.** A trap costs 27 cycles on Cortex-A15 [Dall14 Tab. 3]. Entering EL2 from EL1 costs 68–76 cycles and returning costs 65 on X-Gene [NEVE17 §5]. Saving the VGIC state alone takes 3,250 of the 6,500 cycles in a split-mode hypercall [Dall16 Tab. III].
- **Returning to the VMM's user space is the expensive class of exit.** Even after the VHE redesign, an exit handled in user space costs about 10× a null hypercall (7,630 vs 752) [Dall17 Tab. 2]. vhost exists to avoid that trip [Dall17 §5.1].
- **An in-kernel virtual GIC matters by an order of magnitude.** With an in-kernel VGIC, acknowledging and completing an interrupt costs 427 cycles. With the ACK/EOI emulated in QEMU it costs 13,726, and an IPI rises from 14,366 to 32,951 [Dall14 Tab. 3, §5.2].
- **Interrupt placement matters at application level.** Spreading virtual interrupts across vCPUs instead of one cut KVM's overhead from 35% to 14% on Apache and from 26% to 8% on Memcached [Dall16 §V].

**What the HVF (arm64) SDK shows**

- **Every unhandled exit returns to the VMM thread.** `hv_vcpu_run` blocks until the next exit, and only the thread that owns the vCPU may call it [hv_vcpu.h:19–29, 358–369]. Exits report CANCELED, EXCEPTION (with ESR syndrome, virtual address and IPA), or VTIMER_ACTIVATED [hv_vcpu_types.h:35–86].
- **arm64 has no public in-kernel notifier.** No public API registers an MMIO handler or an ioeventfd-like notifier in the kernel. Only the x86 API has `hv_vm_add_pio_notifier`, which turns port I/O into a Mach message [hv.h:157–185].
  - The arm64 kernel types do describe a Mach-message notification for "monitored memory regions" (`hv_data_abort_notification_t`). Its entry point, `hv_vm_monitor_data_abort`, is declared in no public header [`usr/include/arm64/hv/hv_kern_types.h:78–116`]. Treat it as unavailable (UNVERIFIED).
  - Inference: on arm64, every virtio notify and every device-register access is a user-space ("I/O User"-class) exit.
- **In-kernel GICv3 (macOS 15+).** `hv_gic_create` provides a distributor, redistributors, an MSI frame and the GIC CPU system registers [hv_gic.h:22–49].
  - `hv_gic_set_spi` and `hv_gic_send_msi` carry no owning-thread restriction; the redistributor and ICC accessors do [hv_gic.h:51–77 vs 116–230].
  - libkrun injects interrupts from device threads through `hv_gic_set_spi` and treats GIC MMIO as "managed in-kernel" [libkrun `src/devices/src/legacy/hvfgicv3.rs:130–161`].
  - MSI arrives through a GICv2m-style SPI frame (`GICM_TYPER`, `GICM_SET_SPI_NSR`). There is no ITS and no LPIs [hv_gic_types.h:1701–1703; hv_gic_config.h:56–86].
  - GIC state can be saved and restored [hv_gic.h:250–265; hv_gic_state.h:28–57].
- **Without the in-kernel GIC it gets worse.** Pending interrupts must be set before every `hv_vcpu_run` and are cleared afterwards [hv_vcpu.h:301–312]. libkrun's userspace GICv3 emulates the guest's `ICC_IAR1`/`ICC_SGI1R` accesses on trapped system-register exits [libkrun `src/devices/src/legacy/vcpu.rs:145–160`; `builder.rs:1176–1182`]. That is the "no VGIC" regime measured in [Dall14].
- **Timers.** When the vtimer expires, the vCPU exits and the timer stays masked until the VMM unmasks it at EOI [hv_vcpu.h:403–426].
- **Idle.** Without a GIC, guest WFI exits to user space; libkrun parks the vCPU thread in that case [libkrun `src/hvf/src/lib.rs:808–833`; `vstate.rs:478–484`]. With `hv_gic`, WFI blocks inside HVF [PM §M7].
- **One VM per process** [hv_vm.h:27–33]. The `com.apple.security.hypervisor` entitlement is required; root is not [Apple-ent].
- **Possible VM cap (unverified).** On the dev host, `sysctl kern.hv.max_address_spaces` reports 128. The sysctl has no description (M9).
- **No published HVF exit costs.** We found no peer-reviewed measurement.
- **Our own preliminary numbers (M5 Max, noisy host)** [PM]:

  | Operation | Result |
  |---|---|
  | HVC round trip | 0.70 µs p50 [§M4] |
  | MMIO read/write exit | 0.79 µs p50 [§M4] |
  | `hv_gic_set_spi` → guest handler, vCPU busy | 3.75 µs p50 [§M8] |
  | `hv_gic_set_spi` → guest handler, vCPU idle | 8.2–19.5 µs p50, depending on thread QoS [§M8] |
  | `posix_spawn` → first guest instruction | 3.82 ms p50; loading Hypervisor.framework alone adds ≈1.5 ms [§M11, §M11b] |

### 2.2 Nested virtualization (Q1)

- **Turtles (x86).** An L2 PIO exit costs L1 31 extra exits, and the handler takes ~183K cycles in L1 against ~12K in L0 [Turtles10 §4.1]. `cpuid` costs ~2,600 cycles in a VM and ~58,000 nested [§4.3, Fig. 10]. CPU-bound benchmarks hide this: kernbench overhead is 14.5% and SPECjbb 7.8% [Tab. 2].
- **ARMv8.3 trap-and-emulate.**
  - A nested hypercall costs 422,720 cycles (non-VHE guest hypervisor) or 307,363 (VHE), against 2,729 in a VM: 155× and 113× [NEVE17 §5, Tab. 1].
  - Each nested hypercall takes 126 or 82 traps [Tab. 7].
- **NEVE, adopted as Armv8.4 FEAT_NV2.** NEVE redirects EL2 register accesses to memory through `VNCR_EL2` [NEVE17 §6.1, abstract].
  - Traps per hypercall fall to 15 [Tab. 7].
  - A nested hypercall still costs 92,385–100,895 cycles (34–37×), and a nested device I/O 96,002–105,071 (27–30×). That is **27–37× a non-nested exit**. x86 nested is 16–31× [Tab. 6].
  - Memcached drops from a >40× slowdown to <3×, against 8× on x86 nested [§7.2].
- **Notification policy dominates when exits are expensive.** Faster backends re-arm notifications sooner and so cause more exits. Slowing the L1 backend on purpose cut x86 Memcached overhead [NEVE17 §7.2].
- **DVH (x86 Skylake).**
  - A nested virtio notify costs 48,390 cycles against 4,984 in a VM [DVH20 Tab. 3].
  - Nested paravirtual I/O is more than 3× worse than a VM for Apache, Memcached and Netperf [§4, Fig. 7].
  - DVH's fixes need the L0 hypervisor to provide virtual hardware [DVH20 abstract, §3].
- **Linux status.** arm64 nested KVM requires FEAT_NV2 and is marked "experimental" [kernel-parameters.txt:3211–3221; cpufeature.c:2607–2619; api.rst:3563–3572].
- **Apple status.**
  - Guest EL2 needs macOS 15+ and is detected with `hv_vm_config_get_el2_supported` [hv_vm_config.h:67–108].
  - Apple states "Nested virtualization is available for Mac with the M3 chip, and later" [Apple-VZ-nested].
  - With EL2 enabled, the guest hypervisor gets ICH/ICV registers, and the VMM cannot use `hv_vcpu_set_pending_interrupt` to inject into a nested guest [hv_gic.h:26–33, 172–230].
  - We found no peer-reviewed data on Apple's nested implementation.

**Conclusion for Q1: drive HVF natively.**

1. **Nested multiplies exits even with NV2.** Exits are 27–37× more expensive [NEVE17 Tab. 6], and nested paravirtual I/O is more than 3× worse [DVH20 §4]. The fix requires changing the L0 hypervisor [DVH20 §3], and Apple's L0 is closed.
2. **Rough magnitude favours native.** A user-space exit on VHE-style KVM costs 7,630 cycles; a nested device I/O on NEVE costs 96,002–105,071 [Dall17 Tab. 2; NEVE17 Tab. 6]. The hardware differs, so treat this as order of magnitude only. On our host a native HVF exit takes 0.7–0.8 µs [PM §M4]. If Apple's nesting multiplies exits the way NEVE-class hardware does, an L2 exit would take about 19–30 µs. That is a hypothesis for M2.
3. **Nested runs on fewer Macs.** It needs M3 or later and macOS 15; native HVF runs on all Apple silicon.
4. **Nested adds resident memory.** It needs an L1 kernel and VMM plus shadow stage-2 tables [NEVE17 §4], which works against the minimal-memory target.

Nested would give us the KVM feature set, and it might get around a 128-VM cap if that cap is real (M9). Keep it only as an experimental mode.

### 2.3 virtio mechanisms and exit mitigation (Q2)

**Russell's design rules.**
- Drivers batch kicks because "notification usually involves an expensive exit" [Russell08 §3.1].
- Ring flags suppress notifications [§4].
- Efficient I/O means few notifications and little cache-cold data [§4.1].
- The paper reports no performance data [§7].

**VIRTIO 1.3 ring features.**
- **Split ring.** Each of the three parts is written by only one side. Sizes are 16·Q + (6+2·Q) + (6+8·Q) bytes, with Q ≤ 32768 [virtio-1.3 §2.7]. At Q=256 that is 6,668 B per queue.
- **Packed ring (34).** One ring that both sides read and write, and each side polls a single memory location [§2.8, §2.8.2]. We found no peer-reviewed performance evidence for it.
- **EVENT_IDX (29).** `used_event`/`avail_event` set thresholds and replace the cruder flags. They are "not reliable" because they are unsynchronized [§2.7.7, §2.7.10]. The packed-ring equivalent is `RING_EVENT_FLAGS_DESC` [§2.8.10].
- **INDIRECT_DESC (28)** stores descriptor tables outside the ring [§2.7.5.3].
- **IN_ORDER (35)** lets the device write one used entry per batch [§2.7.9, §2.8.8].
- **NOTIFICATION_DATA (38).** The notify value carries `next_off`/`next_wrap`, so the device learns progress without reading ring memory [§2.9, §4.2.3.3].
- **RING_RESET (40)** resets a single queue [§2.6.1]. The Linux 7.2 guest implements it only over PCI [virtio_pci_modern.c:376–377]; the MMIO transport clears the bit [virtio_mmio.c:114]. Feature bit numbers are from [§6].

**Who supports what.**
- The Linux 7.2 guest accepts INDIRECT_DESC, EVENT_IDX, RING_PACKED, NOTIFICATION_DATA and IN_ORDER [virtio_ring.c:3548–3575].
- Firecracker negotiates EVENT_IDX on block and net and IN_ORDER on vsock. It uses no packed ring, no indirect descriptors and no multiqueue [FC `net/device.rs:295–305`; `vsock/device.rs:50–58`; `block/virtio/device.rs:446`].

**Evidence on exitless and mitigated notification.**
- **ELI** (exitless interrupts for assigned devices): throughput and latency improve 1.3–1.6×, reaching 97–100% of bare metal from a 60–65% baseline [ELI12 abstract, §5 Fig. 3]. The technique is x86-specific and does not apply to Arm [Dall16 §VII].
- **ELVIS** (a polling sidecore for requests plus exitless replies): 1.2–3× better [ELVIS13 abstract].
  - Exits fall from 142K, 109K and 146K per second to under 800 [§4.3].
  - Polling switches on only above a notification-rate threshold and switches off when idle [§3.4]. Replies are made exitless with ELI [§3.5].
- **vRIO.** A sidecore consumes 100% of its cycles even under light load [vRIO16 §1]. Consolidating sidecores on a remote server costs at most 1.18× network latency [§1].
- **Polling vs interrupts.**
  - Synchronous polling completes a 4 KiB read in 4.4 µs against 7.6 µs with interrupts [Yang12 §3.3, Fig. 1].
  - KVM halt-polling saves "a trip through the scheduler… a few micro-seconds", with an adaptively grown and shrunk window [halt-polling.rst:7–21].
- **Direct injection (GICv4).** GICv4 injects vLPIs without the hypervisor, but only for MSIs translated by an ITS. It uses doorbell LPIs when the vPE is not resident, and GICv4.1 adds vSGIs [IHI0069G §1 "Changes specific to GICv4", §7.2, §7.2.1, §8.8].
  - KVM forwards vLPIs only for VFIO endpoints [vgic-v4.c:16–80].
  - Inference: interrupts from software virtio devices do not benefit, and HVF has no ITS at all.

### 2.4 virtio-mmio vs virtio-pci (Q3)

| | virtio-mmio | virtio-pci (modern) |
|---|---|---|
| Discovery | None generic; needs DT or the command line [virtio-1.3 §4.2.1] | Config space and capabilities [§4.1.2–4.1.4] |
| Interrupts | One dedicated signal; the driver MUST read `InterruptStatus` and write `InterruptACK` [§4.2.3.4]. The DT binding allows one interrupt [mmio.yaml:25–26] | MSI-X, up to 0x800 vectors, mapped per queue; ISR unused [§4.1.5.1.2] |
| Guest work per interrupt (Linux) | Reads status, writes ack, scans every queue [virtio_mmio.c:285–307] | Per-queue `vring_interrupt`; no register access [virtio_pci_common.c:355–365] |
| Notify | One `QueueNotify` register [§4.2.2] | Per-queue addresses via `notify_off_multiplier` [§4.1.4.4] |

- **Cost model.** MMIO adds at least two trapped accesses to every interrupt. On HVF both are user-space exits.
  - With MSI-X, completion stays inside the vGIC, the 71-cycle class [Dall16 Tab. II].
  - Spreading interrupts pays off at application level [Dall16 §V].
- **Firecracker, MMIO transport.** One GSI per device, and one ioeventfd on the shared notify address with `datamatch` set to the queue index [FC `src/vmm/src/device_manager/mmio.rs:196–212`].
- **Firecracker, PCI transport** (added in v1.13, #5364 [CHANGELOG.md:463–467]).
  - Notify: one zero-length ioeventfd per queue at a multiplier of 4 [`transport/pci/device.rs:232, 681–705`]. `NoDatamatch` produces `len=0` [kvm-ioctls `vm.rs:52–53, 816–833`], and a zero-length ioeventfd "may get a faster vmexit" [api.rst:2134–2137].
  - Interrupts: MSI-X through GSI routing plus one irqfd per vector, which is re-created on snapshot restore [`pci_mngr.rs:135–142, 679–680`].
  - The docs recommend PCI as "typically… higher throughput and lower latency" but give no numbers [getting-started.md:213–217; kernel-policy.md:156–166].
  - The NSDI paper left PCI out as unnecessary for serverless [FC20 §1.1].
  - The boot-time tests are parametrized on `pci_enabled`, but no results are published [test_boottime.py:108–114] (M4).
- **Probe cost.** We found no peer-reviewed data. Linux can keep BARs assigned by the VMM through `linux,pci-probe-only` [drivers/pci/of.c:247–289]. On HVF, MSI-X maps onto `hv_gic_send_msi`, bounded by the SPI range [hv_gic_parameters.h:80–87].
- **GPU passthrough needs PCI.**
  - NVIDIA requires an IOMMU and binding the GPU to `vfio-pci` [NVIDIA-vGPU].
  - VFIO works unprivileged only after an administrator hands over group ownership [vfio.rst:5–12, 159–164].
  - VFIO pins DMA memory against RLIMIT_MEMLOCK unless the process has CAP_IPC_LOCK [vfio_iommu_type1.c:1588, 1659].
  - The arm64 HVF API has no device-assignment or IOMMU interface; it only maps process memory [hv_vm.h:44–72].

### 2.5 Interrupts, irqfd and ioeventfd (Q4)

- **KVM_IOEVENTFD.** A guest write to a registered address "will signal the provided event instead of triggering an exit". The write completes in the kernel without returning to the VMM. Datamatch and zero-length variants exist [api.rst:2093–2137].
- **KVM_IRQFD.** Writing to an eventfd injects a GSI. On arm64 that becomes SPI pin+32, or an MSI translated to an LPI through the in-kernel ITS. A resample mode handles level-triggered interrupts [api.rst:3178–3215].
- **Where the GIC lives matters.** Emulating the GIC in EL2 (Xen) costs 1,356 cycles against 7,370 in EL1 (KVM) [Dall16 Tab. II], and userspace emulation is the costliest of all [Dall14 Tab. 3].
- **HVF has the irqfd half but not the ioeventfd half.** Device threads can inject interrupts directly (§2.1), but every notify still passes through the vCPU thread.
  - libkrun dispatches MMIO on the vCPU thread [vstate.rs:389–401].
  - It wakes device threads through an eventfd built on a pipe [`src/utils/src/macos/eventfd.rs:7`].
  - macOS also provides `EVFILT_USER`/`NOTE_TRIGGER` [SDK `sys/event.h:77, 204`].

### 2.6 Block (Q5)

**Firecracker baseline.**
- In the NSDI'20 paper: about 13,000 IOPS (52 MB/s at 4 KiB) against more than 340K from the hardware, because I/O was serial and there was no flush. p99 at 4 KiB and QD1 was 49 µs above native [FC20 §5.3, Fig. 8–9].
- Current spec: 1 GiB/s using at most 70% of a core [SPECIFICATION.md:55].

**Firecracker's I/O engines.**
- `Sync` is the default. The io_uring-based `Async` engine is a developer preview.
  - It adds up to ~110 ms to device creation.
  - Reads get 1.5–3× IOPS per CPU and up to 30× IOPS.
  - Writes get 20–45% more IOPS but worse IOPS per CPU.
  - Risks: io-wq workers can exhaust PIDs, and on 5.10 they run in the root cgroup [block-io-engine.md:68–149].
- The default cache mode is `Unsafe`, which does not advertise FLUSH [block.md:74–80]. A guest may then assume a writethrough cache [virtio-1.3 §5.2.5.1].
- Methodology: fio at 4 KiB random read/write, QD 32, `direct=1` [test_block.py:120–124; utils_fio.py:79–82].

**io_uring.**
- Polled I/O (`IOPOLL`) requires `O_DIRECT`.
- SQPOLL removes submit syscalls and sleeps after `sq_thread_idle`, 1 s by default [Axboe19 §8.2–8.3].
- Throughput: 1.7M 4K IOPS polled, 1.2M unpolled, and 608K for aio [§9.1].
- SQPOLL needed CAP_SYS_NICE in 5.11 and needs no privilege from 5.13 [io_uring_setup(2)].
- `io_uring_disabled` set to 1 or 2 blocks ring creation [sysctl/kernel.rst:496–512].
- **SQPOLL sharing a core with the application collapses:** 13 KIOPS with an 8 ms median. With two cores it recovers to 18% below SPDK's peak [Didona22 §3.1]. Matching SPDK takes about twice the cores [§1].

**macOS.**
- **Durability.**
  - `fsync` does not flush the drive cache; `F_FULLFSYNC` does, and "may take quite a while" [fsync(2); fcntl(2)].
  - `F_BARRIERFSYNC` guarantees ordering but not durability.
- **Caching.** `F_NOCACHE` turns off data caching [fcntl(2)].
- **Vector I/O.** `preadv` exists (nonstandard) [pread(2)].
- **Event notification.** kqueue provides `EVFILT_AIO` and `EVFILT_USER` [sys/event.h:70, 77].
- **Clones.** `clonefile` creates copy-on-write clones that share data blocks [clonefile(2)].
- **No io_uring equivalent** and no study on macOS (M6).

### 2.7 virtio-net (Q6)

- **Features.** CSUM, GUEST_TSO4/6, HOST_TSO4/6, MRG_RXBUF (15), MQ (22), NOTF_COAL (53), USO and RSS [virtio-1.3 §5.1.3]. Notification coalescing is described in [§5.1.6.5.9].
- **Receive buffers cost guest memory.** Without MRG_RXBUF, a driver that negotiates guest TSO/UFO/USO SHOULD post receive buffers of at least 65,562 B. With MRG_RXBUF, a buffer only has to hold the header [§5.1.6.3.1]. For 256 descriptors that is about 16 MiB of guest RAM per receive queue.
- **Offloads matter.** Adding scatter/gather, TSO and checksum offload to Xen's virtual NIC raised guest transmit throughput by 272% (750 → 2,794 Mb/s) [Menon06 §6.3, Fig. 5]. With the other optimizations the total gain was 4.4× [§6.2, Fig. 3]. The GSO header reduces calls out of the VM [Russell08 §5.2].
- **Packed ring and multiqueue.** We found no peer-reviewed data for the packed ring. For multiqueue, the evidence is interrupt spreading [Dall16 §V].
- **Firecracker.** Offers CSUM, TSO, UFO, MRG_RXBUF and EVENT_IDX on one queue pair [net/device.rs:295–305].
  - NSDI'20: about 15 Gb/s, against 44 Gb/s host loopback [FC20 Tab. 1].
  - Current spec: 14.5 Gb/s using at most 80% of a core, and 0.06 ms added latency [SPECIFICATION.md:48–52].

### 2.8 virtio-vsock (Q7)

- **Spec.**
  - Three queues (rx, tx, event); stream and seqpacket types.
  - Credit-based flow control through `buf_alloc`/`fwd_cnt`, "so data is never dropped".
  - A 44-byte header [§5.10.6].
  - On TRANSPORT_RESET, established connections close and listeners survive [virtio-1.3 §5.10.2–5.10.3, §5.10.6.3, §5.10.6.7].
  - There is no multiqueue feature.
- **Firecracker.**
  - The device runs in user space, "bypassing vhost". AF_VSOCK ports map to AF_UNIX sockets.
    - Host-initiated: the host sends `CONNECT PORT\n` and gets back `OK PORT\n`.
    - Guest-initiated: the connection goes to `uds_path_PORT` [docs/vsock.md:39–99].
  - Each connection has a 64 KiB TX buffer, up to 1,023 connections [`csm/mod.rs:13`; `unix/mod.rs:20`]. Busy vsock connections can push VMM memory above 5 MiB [SPECIFICATION.md:28–31].
  - Snapshot restore resets vsock and closes connections [snapshot-support.md:674–685].
- **Performance.** We found no peer-reviewed vsock study (M8).

## 3. Implications for shards (ranked)

**R1. On macOS, use HVF directly with the in-kernel GIC (macOS 15+), and leave EL2 off.**
- *Benefit:* avoids exit multiplication of 27× or more [NEVE17 Tab. 6; DVH20 §4]. Works on M1 and later, and adds no resident L1 VM.
- *Cost/risk:* two backends (HVF and KVM), since HVF has no ioeventfd or vhost. One VM per process adds per-VM process overhead [hv_vm.h:27–33].
- *Constraints:* fits rootless (entitlement only) and minimal memory.
- **Conflict with ≤5 ms start:** spawning the process alone takes 3.8 ms p50 [PM §M11]. VMM processes must be pre-spawned or pooled, and pooled processes cost memory.
- *Gate:* M1 and M2 confirm the magnitudes; M9 checks the possible 128-VM cap.

**R2. Treat every HVF exit as a user-space exit.**
- Negotiate EVENT_IDX everywhere, batch kicks, and use NAPI.
- Poll adaptively on one shared I/O thread per host: enable above a notification-rate threshold and back off when idle [ELVIS13 §3.4]. Keep every notification path non-spinning at idle [vRIO16 §1].
- Deliberately delay re-arming notifications under load [NEVE17 §7.2].
- *Benefit:* the exit rate falls from over 100K/s to under 1K/s [ELVIS13 §4.3].
- *Risk:* CPU burn works against density. Tune with M5.

**R3. Default to virtio-pci (modern) with per-queue MSI-X on both KVM and HVF. Keep virtio-mmio only for single-vCPU "minimal" profiles if M4 shows PCI costs time on the critical path.**
- *Benefit:* no status/ack exits and per-queue interrupt affinity [virtio-1.3 §4.2.3.4, §4.1.5.1.2; Dall16 §V]. One topology also covers VFIO GPUs, and RING_RESET works in the Linux guest.
- *Cost:* the guest needs `CONFIG_PCI`, enumeration exits, and emulating the MSI-X table.
- *≤5 ms start:* if start means snapshot restore, enumeration is paid when the snapshot is built. Restore still needs O(vectors) irqfd setup on KVM [pci_mngr.rs:679–680] and `hv_gic_set_state` on HVF [hv_gic.h:250–265].

**R4. Interrupt paths.**
- **KVM:** irqfd plus one zero-length ioeventfd per queue, and the in-kernel vGICv3 with its ITS [api.rst:2093–2137, 3178–3215].
- **HVF:** call `hv_gic_send_msi` or `hv_gic_set_spi` from device threads. Replace pipe wakeups with `EVFILT_USER`, or process short requests inline on the vCPU thread (M3/M5). Never use the userspace GIC [Dall14 Tab. 3].
- **HVF thread policy decides wake latency.** With a time-constraint policy, idle-vCPU IRQ delivery p99 falls from 61–315 µs to 11 µs, and timer lateness at a 1 ms period falls from 258 µs to 8.7 µs [PM §M8, §M10]. The energy and density cost of that policy is unmeasured.
- **Halt polling.** With `hv_gic`, WFI already blocks inside HVF [PM §M7]. User-space halt polling is only relevant without a GIC, by analogy with KVM's [halt-polling.rst:7–21].

**R5. Block backends.**
- **Linux:** io_uring without SQPOLL by default [Didona22 §3.1]. Create rings off the critical path, or pool them, because Firecracker reports up to ~110 ms [block-io-engine.md:68–79] (M6). Fall back to a thread pool when `io_uring_disabled` is set.
- **macOS:** worker threads using `preadv`/`pwritev`.
  - Map FLUSH to `F_FULLFSYNC` on durable volumes and use `F_BARRIERFSYNC` or unsafe mode on ephemeral scratch disks [fcntl(2)].
  - Use `F_NOCACHE` on per-VM writable layers so host memory does not grow, and leave shared read-only layers cached (M6).
  - Use `clonefile` for O(1) per-VM copy-on-write disks.
- Advertise FLUSH honestly; Firecracker's unsafe default is a durability gap [FC20 §5.3].

**R6. virtio-net.**
- Always negotiate CSUM, GSO/TSO, MRG_RXBUF, EVENT_IDX and NOTF_COAL [Menon06; virtio-1.3 §5.1.6.3.1]. MRG_RXBUF alone saves about 16 MiB per queue.
- Enable MQ only when there is more than one vCPU and PCI/MSI-X is in use.
- The packed ring waits for M7.

**R7. Use vsock as the host↔engine control plane, following Firecracker's user-space AF_UNIX mapping.**
- It is rootless and portable to macOS.
- Cap per-connection buffering: 1,023 × 64 KiB is about 64 MiB in the worst case.
- Reconnect from the host after snapshot restore, because established connections close [virtio-1.3 §5.10.6.7].

**R8. GPU passthrough: Linux only, through VFIO-PCI; plan a paravirtual GPU for macOS separately.**
- VFIO needs an administrator to bind devices and grant group ownership, which conflicts with "users install nothing".
- It pins guest RAM, which conflicts with minimal memory and fast start.
- GICv4 vLPI forwarding does help GPU MSIs [vgic-v4.c:16–80].
- HVF has no way to assign devices.

## 4. Open questions needing our own measurement

- **M1 — HVF exit costs versus KVM.** HVC and MMIO are partly answered [PM §M4].
  - Still to do: trapped system register and VTIMER exits, re-measured on a quiet host.
  - Run the same bare-metal guest loop (10⁶ iterations, CNTVCT timestamps, median and p99) on KVM arm64 (VHE, Graviton/Ampere) and x86 KVM, so the backends are compared like for like.
- **M2 — Nested cost.** Run the M1 suite in L2 under EL2-enabled HVF with L1 KVM. Also measure L2 cold boot and restore time, L1 RSS, and shadow stage-2 fault counts during the first 100 ms.
- **M3 — Interrupt latency and idle behaviour.** SPI injection and WFI are partly answered [PM §M7–M8].
  - Still to do: latency of `hv_gic_send_msi` (MSI-X path) against SPI.
  - Count whether guest EOI/ACK exit at all with `hv_gic`.
  - Measure CPU and energy cost of time-constraint threads with 100 idle VMs.
- **M4 — Transport start cost.**
  - Exit counts and time from start to init for MMIO vs PCI, with and without `linux,pci-probe-only`, on KVM and HVF, for both cold boot and snapshot restore.
  - Reproduce Firecracker's `pci_enabled` boot tests.
- **M5 — Notify and interrupt rates.**
  - Exits per GB and CPU per VM for net and block with EVENT_IDX, NOTF_COAL and adaptive polling.
  - Idle CPU must be about 0 with 100 idle VMs.
  - Compare inline vCPU-thread processing, pipe, and `EVFILT_USER` wakeups.
- **M6 — Block.**
  - Methodology: fio 4 KiB randread/randwrite at QD1 and QD32, `direct=1`, as in Firecracker.
  - macOS: compare a `preadv` thread pool, POSIX AIO with `EVFILT_AIO`, and dispatch_io. Compare `fsync`, `F_FULLFSYNC` and `F_BARRIERFSYNC` latency, and host RSS with and without `F_NOCACHE`.
  - Linux 6.x: compare sync, io_uring (interrupt, IOPOLL, SQPOLL), and ring-creation latency.
- **M7 — Net.** iperf3 at MTU 1500 over one and ten streams, as in [FC20 §5.3]. Vary MRG_RXBUF (measure guest RSS), TSO on/off, split vs packed ring, and MQ scaling.
- **M8 — vsock.** Throughput, round-trip latency, connect time, and VMM RSS per connection. Compare against virtio-net loopback carrying Docker Engine API traffic.
- **M9 — HVF VM cap.** Start minimal VMs until one fails, with and without EL2.
- **M10 — VFIO pinning.** Time and RSS to pin 1–64 GiB through iommufd at VM start.

## 5. References

- [Dall14] C. Dall, J. Nieh. "KVM/ARM: The Design and Implementation of the Linux ARM Hypervisor." ASPLOS 2014. https://www.cs.columbia.edu/~nieh/pubs/asplos2014_kvmarm.pdf
- [Dall16] C. Dall, S.-W. Li, J. T. Lim, J. Nieh, G. Koloventzos. "ARM Virtualization: Performance and Architectural Implications." ISCA 2016. https://www.cs.columbia.edu/~cdall/pubs/isca2016-dall.pdf
- [Dall17] C. Dall, S.-W. Li, J. T. Lim, J. Nieh. "Optimizing the Design and Implementation of the Linux ARM Hypervisor." USENIX ATC 2017. https://www.usenix.org/system/files/conference/atc17/atc17-dall.pdf
- [NEVE17] J. T. Lim, C. Dall, S.-W. Li, J. Nieh, M. Zyngier. "NEVE: Nested Virtualization Extensions for ARM." SOSP 2017. doi:10.1145/3132747.3132754; https://www.cs.columbia.edu/~nieh/pubs/sosp2017_neve.pdf
- [Turtles10] M. Ben-Yehuda et al. "The Turtles Project: Design and Implementation of Nested Virtualization." OSDI 2010. https://www.usenix.org/legacy/event/osdi10/tech/full_papers/Ben-Yehuda.pdf
- [DVH20] J. T. Lim, J. Nieh. "Optimizing Nested Virtualization Performance Using Direct Virtual Hardware." ASPLOS 2020. https://www.cs.columbia.edu/~nieh/pubs/asplos2020_dvh.pdf
- [Russell08] R. Russell. "virtio: Towards a De-Facto Standard for Virtual I/O Devices." ACM SIGOPS OSR 42(5), 2008. doi:10.1145/1400097.1400108; author copy https://ozlabs.org/~rusty/virtio-spec/virtio-paper.pdf
- [virtio-1.3] OASIS. Virtual I/O Device (VIRTIO) Version 1.3, Committee Specification Draft 01, 6 Oct 2023. https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.pdf
- [ELI12] A. Gordon, N. Amit, N. Har'El, M. Ben-Yehuda, A. Landau, A. Schuster, D. Tsafrir. "ELI: Bare-Metal Performance for I/O Virtualization." ASPLOS 2012. https://www.mulix.org/pubs/eli/eli.pdf
- [ELVIS13] N. Har'El, A. Gordon, A. Landau, M. Ben-Yehuda, A. Traeger, R. Ladelsky. "Efficient and Scalable Paravirtual I/O System." USENIX ATC 2013. https://www.usenix.org/system/files/conference/atc13/atc13-harel.pdf
- [vRIO16] Y. Kuperman, E. Moscovici, J. Nider, R. Ladelsky, A. Gordon, D. Tsafrir. "Paravirtual Remote I/O." ASPLOS 2016. doi:10.1145/2872362.2872378
- [FC20] A. Agache et al. "Firecracker: Lightweight Virtualization for Serverless Applications." NSDI 2020. https://www.usenix.org/system/files/nsdi20-paper-agache.pdf
- [Menon06] A. Menon, A. L. Cox, W. Zwaenepoel. "Optimizing Network Virtualization in Xen." USENIX ATC 2006. https://www.usenix.org/legacy/event/usenix06/tech/menon/menon.pdf
- [Yang12] J. Yang, D. B. Minturn, F. Hady. "When Poll is Better than Interrupt." FAST 2012. https://www.usenix.org/legacy/event/fast12/tech/full_papers/Yang.pdf
- [Didona22] D. Didona, J. Pfefferle, N. Ioannou, B. Metzler, A. Trivedi. "Understanding Modern Storage APIs: A Systematic Study of libaio, SPDK, and io_uring." SYSTOR 2022. doi:10.1145/3534056.3534945
- [Axboe19] J. Axboe. "Efficient IO with io_uring," v0.4, 2019-10-15. Primary document by the author. The original URL https://kernel.dk/io_uring.pdf now returns 404, so it was read from the Internet Archive snapshot of 2024-12-28.
- [IHI0069G] Arm. GIC Architecture Specification, GICv3/v4, IHI 0069G (2021). Arm's portal blocks automated retrieval; it was read from a third-party mirror of the official PDF, https://www.scs.stanford.edu/~zyedidia/docs/arm/gic_v3.pdf
- [Apple-ent] Apple Developer Documentation, "com.apple.security.hypervisor" entitlement (retrieved 2026-09-28). https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.hypervisor
- [Apple-VZ-nested] Apple Developer Documentation, `VZGenericPlatformConfiguration.isNestedVirtualizationSupported`, macOS 15.0+ (retrieved 2026-09-28). https://developer.apple.com/documentation/virtualization/vzgenericplatformconfiguration/isnestedvirtualizationsupported
- [NVIDIA-vGPU] NVIDIA Virtual GPU Software User Guide, "Using GPU Pass-Through." https://docs.nvidia.com/vgpu/latest/grid-vgpu-user-guide/
- Linux man page: io_uring_setup(2), liburing project, https://man7.org/linux/man-pages/man2/io_uring_setup.2.html. macOS man pages (local, macOS 26.4): fcntl(2), fsync(2), pread(2), clonefile(2).
- [PM] shards internal measurements, `docs/research/platform-measurements.md` (harness `docs/research/measurements/hvf/hvfbench.c`), 2026-09-28. Preliminary numbers taken on a noisy host; not peer-reviewed.
- Source trees: macOS SDK 26.4 Hypervisor headers, `arm64/hv/hv_kern_types.h` and `sys/event.h`; linux 7.2-rc4 (`Documentation/`, `drivers/`, `arch/arm64/`); firecracker `edb60617c`; libkrun `1f5dd028`; rust-vmm/kvm `kvm-ioctls/src/ioctls/vm.rs` (main, retrieved 2026-09-28).
