# shards architecture

Living design document. Every decision cites its evidence:

- `[PM Mx]`: our measurements, [../research/platform-measurements.md](../research/platform-measurements.md)
- `[VIO]`: [virtio-io-exits.md](../research/virtio-io-exits.md)
- `[GT]`: [hvf-arm64-kvm-ground-truth.md](../research/hvf-arm64-kvm-ground-truth.md)
- `[GPU]`: [gpu-accelerators.md](../research/gpu-accelerators.md)

Sections marked **pending** wait on the research documents still being written:
boot-latency, snapshot-restore-memory, image-storage, networking,
docker-compose-compat, rootless-security, container-engine-internals.

## 1. What shards is

An all-in-one, rootless microVM platform for agents. There are two layers:

- **Host layer.** `shards` is a Docker-compatible CLI and daemon whose "containers"
  are microVMs. It ships its own VMM with native backends: Hypervisor.framework
  (macOS/arm64) and KVM (Linux). Microvm images are specified and built like Docker
  images.
- **Guest layer.** Every default microVM runs shards' own Docker- and
  Compose-compatible engine, rootless. Agents run in containers inside it.

### Targets

| Target | Budget |
|---|---|
| Start latency | ≤ 5 ms from request to environment ready |
| Per-VM memory | minimal; beat Firecracker's ≤ 5 MiB VMM overhead |

The whole stack (VM, in-VM engine and containers) must match or beat Firecracker's
performance and resource usage.

## 2. Decisions so far

| # | Decision | Evidence |
|---|---|---|
| D1 | On macOS, drive Hypervisor.framework natively. No nested KVM (EL2 stays off by default). | Nested exits cost 27–37× more even with NV2 [VIO §2.2]; native HVF exit ≈ 0.7–0.8 µs [PM M4] |
| D2 | Starts are served by **snapshot restore into a pre-spawned, pre-initialized VMM process**, never by spawning or cold-booting on the request path. | `posix_spawn` → `main` 3.7 ms p50; framework load +1.5 ms; first `hv_vm_create` 0.55 ms [PM M11, M11b]; one VM per process [GT §1.1] |
| D3 | Use the in-kernel GIC (`hv_gic`). Snapshot it as **individual registers**, never as the state blob. | WFI blocks in-kernel with zero userspace exits [PM M7]; blob restore 1.2 ms vs 20 µs register rewrite [PM M14] |
| D4 | vCPU threads run with Mach time-constraint policy plus QoS user-interactive. Pending: fail-safe behaviour under CPU-bound guests. | Timer lateness 258 µs → 8.7 µs at 1 ms; idle IRQ p99 315 → 11 µs [PM M8, M10] |
| D5 | Create vCPUs strictly sequentially in index order, both at boot and on restore. | Redistributor frames and processor numbers follow creation order [PM M13] |
| D6 | Guest RAM uses the 16 KiB IPA granule (default); the guest's own page size stays 4 KiB. | First-touch cost per byte is 4× lower than with a 4 KiB granule [PM M5] |
| D7 | Snapshot memory is file-backed `MAP_PRIVATE`, mapped lazily. The working set can be prefetched from helper vCPUs, not from host reads. | Cheapest first-touch backing, 1.07 µs per 16 KiB page; host pre-read doesn't help; 4 vCPUs fault 2.2× faster [PM M5, M6] |
| D8 | Every HVF exit is a userspace exit: negotiate EVENT_IDX, batch, and keep notify handlers to a hand-off. Adaptive polling only above a rate threshold. | No ioeventfd on HVF [GT §1.2]; ELVIS [VIO §2.3] |
| D9 | Implement **both** virtio-mmio and virtio-pci (modern, per-queue MSI-X). Choose the default transport by measuring the restore path and runtime. | MMIO costs 2 exits per interrupt; PCI is needed for VFIO [VIO §2.4, R3]; GPU-free default VMs must stay pin-free [GPU R1] |
| D10 | Pin the guest's CPU view explicitly: MPIDR, PARange clamped to the IPA, SME exposure decided per image. Don't inherit defaults. | Defaults show PARange 40 on a 36-bit IPA and expose SME2 [PM M12] |
| D11 | GPUs are zero-cost when unused. GPU VMs are a separate class assigned from a warm pool (VFIO via iommufd on Linux; virtio-gpu/Venus plus a remoting broker on macOS). | Assigned devices pin all RAM and break CoW; FLR ≥ 100 ms; CUDA init takes seconds [GPU §2.3, R1–R6] |
| D12 | vsock is the host↔guest control plane (exec, stdio, lifecycle, engine API). | Rootless and portable; Firecracker's AF_UNIX mapping [VIO R7] |

## 3. Components

```
shards (host CLI, docker-compatible) ──unix socket──▶ shardsd (daemon)
                                                     │  image store · build · networks
                                                     │  volumes · templates/snapshots
                                                     │  warm VMM pool · policy
                                                     ▼
                                       shards-vmm process (one per microVM)
                                       hv backend (HVF | KVM) · memory · boot/FDT
                                       vCPU threads · GIC · virtio devices · snapshot
                                                     │ virtio (blk/net/vsock/console/rng/pmem/fs/gpu)
                                                     ▼
                                       guest: Linux (tuned) · shards-init (PID 1)
                                       shards-engine (Docker Engine API, rootless)
                                       containers (agents, compose services)
```

- **VMM** (`shards-vmm`): one process per microVM. That is forced on macOS
  [GT §1.1] and chosen on Linux for fault isolation, as Firecracker does. The hot
  path is kept free of allocation and locks; device threads communicate with vCPU
  threads through lock-free rings.
- **Daemon** (`shardsd`): serves a Docker-compatible API with extensions for VM
  specs and isolation policy. It owns the warm pool and the template snapshots.
- **Guest**:
  - a tuned Linux kernel built from source inside a shards builder VM
  - `shards-init`, a minimal static PID 1 that sets up and then drops privilege
  - `shards-engine`, our own Docker/Compose-compatible engine and OCI runtime,
    rootless. **pending**: engine-internals and rootless research.

## 4. Start path (≤ 5 ms budget)

| Step | Cost | Evidence |
|---|---|---|
| Daemon receives request; resolves template; claims warm VMM | ~10–50 µs of IPC (to be measured) | — |
| `hv_vm_create` + `hv_gic_create` + sequential vCPU creation | ~11 + 2 + 7·n µs; can be done ahead in the warm process | [PM M2] |
| `mmap` snapshot memory `MAP_PRIVATE` + `hv_vm_map` | ~1–10 µs | [PM M3] |
| GIC restore at register level | ~20–30 µs | [PM M14] |
| vCPU register restore | ~0.5 µs per vCPU | [PM M14] |
| Resume; guest faults in its working set | ~64 µs per MiB (1 vCPU), ~2.2× less with prefetch vCPUs | [PM M5, M6] |
| Engine starts the container, reports ready over vsock | to be measured | pending |

Clone correctness hazards (RNG reseed via vmgenid, clock via vtimer offset, network
identity) are **pending** the snapshot research.

## 5. Phased plan with exit criteria

Each phase ends with committed E2E tests and benchmarks that run real VMs.

1. **VMM core on HVF (arm64).**
   - Scope: memory, Image loader, FDT, in-kernel GIC, PSCI (incl. CPU_ON), vtimer,
     UART console, virtio-mmio plus blk/console/rng/vsock, boot timer.
   - Exit: the bootstrap kernel (Linux 6.18 from Firecracker CI) boots to a shell
     from a real rootfs, SMP.
   - Exit: boot-time and VMM-RSS benchmarks run in the suite.
2. **Snapshot/restore plus warm pool.**
   - Scope: register-level GIC/vCPU/device state, lazy file-backed memory, clone
     identity fixes.
   - Exit: measured request → guest-running ≤ 1 ms p50 on the M5 Max.
3. **Guest stack v0.**
   - Scope: `shards-init`; tuned kernel built in a shards builder VM; OCI pull →
     rootfs image; `shards run IMAGE CMD`.
   - Pending: image-storage, boot-latency research.
4. **In-VM engine.**
   - Scope: Docker Engine API subset → full; rootless OCI runtime; networks,
     volumes, build; compose.
   - Proven by running the official Docker CLI/Compose conformance suites against
     it. Pending: compat, engine, rootless research.
5. **Host CLI/daemon parity and networking.**
   - Scope: userspace network stack, port publishing, per-VM and per-container
     isolation policy. Pending: networking research.
6. **KVM backend.**
   - Scope: arm64 first, tested inside an EL2-enabled shards VM on this host; then
     x86_64. virtio-pci plus VFIO.
7. **GPU classes** [GPU R1–R6]: Linux VFIO warm pools; macOS virtio-gpu/Venus plus
   a remoting broker.
8. **Hardening.**
   - Scope: seccomp/sandbox profiles, virtio fuzzing, stress-to-failure.
   - Pending: rootless-security research.

## 6. Testing and benchmarking rules

- **E2E means real VMs.** Tests boot real guests on the host hypervisor. Mocks are
  allowed only for pure parsers and state machines.
- **Every performance claim is backed by a committed benchmark.**
  - Report n, p50, p90, p99 and max.
  - Record host, OS and git revision.
  - Compare against Firecracker's published targets and methodology
    (boot-timer MMIO write, RSS excluding guest memory) [VIO §2.6–2.7].
- **Stress to failure.** Also: VM density until OOM, concurrent starts, fuzzing of
  device register and descriptor inputs, and fault injection on host I/O.
