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
- **Guest layer.** Every default microVM runs shards' own runtime, rootless, in the place
  containerd has on a Docker host. One microVM holds many agents and their compose services.
  The runtime is Docker- and Compose-compatible at its interface, but it is not containerd,
  runc or any container runtime underneath: it is a different implementation, built so that
  a microVM full of running agents snapshots and restores at our start-time targets.
  Workloads inherit their microVM's OS: the microVM's image, built like a Docker image, is
  the only userland, and no workload brings its own. They are still isolated like
  containers. Each workload in a microVM gets its own boundaries, set per workload as Docker
  and Compose set them per container: which networks it joins (none, its own, or one shared with chosen
  workloads), its file view (root, volumes, read-only or writable), its devices and its
  permissions. The microVM boundary sits outside all of them.

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
| D2 | Starts are served by **snapshot restore into a pre-spawned, pre-initialized VMM process**, never by spawning or cold-booting on the request path. | `posix_spawn` → `main` 3.7 ms p50; framework load +1.5 ms; first `hv_vm_create` 0.55 ms [PM M11, M11b]; one VM per process [GT §1.1]; on a shared host, a per-request process's launch, restore and teardown carry the tail, and thread QoS does not shorten it [PM M22] |
| D3 | Use the in-kernel GIC (`hv_gic`). Snapshot it as HVF's own serialization of the device (`hv_gic_state`), not as its registers (revised 2026-09-29, D14). Open: its 1.2 ms restore, paid ahead of requests by warm VMs. | WFI blocks in-kernel with zero userspace exits [PM M7]; registers lose interrupts on their way to a vCPU, and every restore of a guest with a disk request in flight stalled [PM M45]; the blob restores in 1.2 ms against 20 µs for registers [PM M14] |
| D4 | vCPU threads run with Mach time-constraint policy plus QoS user-interactive. Pending: fail-safe behaviour under CPU-bound guests. | Timer lateness 258 µs → 8.7 µs at 1 ms; idle IRQ p99 315 → 11 µs [PM M8, M10] |
| D5 | Create vCPUs strictly sequentially in index order, both at boot and on restore. | Redistributor frames and processor numbers follow creation order [PM M13] |
| D6 | Guest RAM uses the 16 KiB IPA granule (default); the guest's own page size stays 4 KiB. | First-touch cost per byte is 4× lower than with a 4 KiB granule [PM M5] |
| D7 | Snapshot memory is file-backed `MAP_PRIVATE`, mapped lazily. On HVF the working set can be prefetched from helper vCPUs, not from host reads; on KVM the VMM maps it ahead (`KVM_PRE_FAULT_MEMORY`) [PM M33]. Map it in the warm process before the request: in-process faults on file-backed memory have no tail beyond 111 µs in 6.3 M, but fresh processes mapping a just-unmapped file stalled ~1 s in ~1% of boots [PM M15, M16]. | Cheapest first-touch backing, 1.07 µs per 16 KiB page; host pre-read doesn't help; 4 vCPUs fault 2.2× faster [PM M5, M6] |
| D8 | Every HVF exit is a userspace exit: negotiate EVENT_IDX, batch, and keep notify handlers to a hand-off, pmem's too. Adaptive polling only above a rate threshold. Devices serve their queues in rounds of at most a ring's worth, looking for a stop between requests; a round that leaves work comes back for it without waiting to be notified, and one that finds its queue empty re-arms notifications first (audit A09). | No ioeventfd on HVF [GT §1.2]; ELVIS [VIO §2.3]; a driver refilling a queue from another CPU held a device's drain forever, and with it a reset, a stop or vsock's RX (audit A09) |
| D9 | Implement **both** virtio-mmio and virtio-pci (modern, per-queue MSI-X). Choose the default transport by measuring the restore path and runtime. | MMIO costs 2 exits per interrupt; PCI is needed for VFIO [VIO §2.4, R3]; GPU-free default VMs must stay pin-free [GPU R1] |
| D10 | Pin the guest's CPU view explicitly: MPIDR, PARange clamped to the IPA, SME exposure decided per image. Don't inherit defaults. | Defaults show PARange 40 on a 36-bit IPA and expose SME2 [PM M12] |
| D11 | GPUs are zero-cost when unused. GPU VMs are a separate class assigned from a warm pool (VFIO via iommufd on Linux; virtio-gpu/Venus plus a remoting broker on macOS). | Assigned devices pin all RAM and break CoW; FLR ≥ 100 ms; CUDA init takes seconds [GPU §2.3, R1–R6] |
| D12 | vsock is the host↔guest control plane (exec, stdio, lifecycle, engine API). Built (a7b32ab): guest ports map to host Unix sockets as in Firecracker (`CONNECT <port>`; the guest reaches `<path>_P`). Unlike Firecracker, host EOF is a half-close, so a guest can answer after stdin ends. Each restored copy binds its own socket, where it is given a path; the ports its own process serves (the run's) take no socket file (D30). A snapshot keeps the streams the device held. The restored device resets each of them with an RST on its RX queue, ahead of every other packet, and continues host port allocation past the snapshot's, never reusing a held port. It posts no TRANSPORT_RESET: Linux handles that event in a work item apart from RX, and on one interrupt it visits RX first. So a connection made right after the restore could be established and then reset (13 of 350 restores under CPU load). | Rootless and portable; Firecracker's AF_UNIX mapping [VIO R7]; macOS poll reports POLLHUP on a half-close, so the device waits with kqueue there; restores [PM M20]: Linux 7.2 net/vmw_vsock/virtio_transport.c (`event_work`, `rx_work` handles RX in order), drivers/virtio/virtio_mmio.c `vm_interrupt` over queues in setup order (virtio_ring.c `list_add_tail`); a REQUEST matching a closing socket is dropped (virtio_transport_common.c `virtio_transport_recv_disconnecting`) |
| D26 | `shards run` is served by a per-user daemon that hands each request to a warm VM process of the image's template: resumed, connected, waiting for its command. The client's stdio and connection pass by `SCM_RIGHTS`, and the daemon keeps its copies until the warm VM has taken them. The CLI is a thin binary. | Handoff 31 µs p50; warm VM 12.3 MiB, no CPU; a thin client costs 1.4 ms against 3.5 ms for a binary linking the VMM's frameworks [PM M23]; XNU flushes a socket in flight that no process holds [PM M24]; a pooled run takes 3.4 ms at p50 and 3.9 ms at p99 with the thin client [PM M26]; pre-created VM shells [Manco17 §5.2; Wanninger22 §5.2] |
| D27 | Every `shards run` is a container, as `docker run`'s is: an ID and a name, running until its command ends, then exited until `shards rm` or `--rm` removes it. Command lines are read as the Docker CLI reads them, by one crate (`shards-cmdline`) in the client and the daemon: the client answers `--help` and usage mistakes itself, and the daemon keeps the records and answers `ps`, `wait`, `logs`, `stop`, `kill` and `rm`. | The Docker CLI's own answers: a differential test against docker/cli v29.8.1's command tree [scripts/docker-cli]; dockerd's names, IDs, start failures and stop semantics [moby daemon/names.go, daemon/errors.go, daemon/stop.go, daemon/kill.go @ docker-v29.8.1]; docs/research/container-lifecycle-cli.md; a record costs no run anything it waits for (a spare container made ahead) |

### Snapshots (D14)

A snapshot is taken at a point the guest chooses. The guest writes the control page's
`SNAPSHOT` register, and every clone resumes at the instruction after that store. A
template is thus a guest that has finished initializing and says so; restores never
re-run that work. That includes the kernel's own background work: shards-init waits for
the crypto self-tests first, since every clone would replay the rest (PM M21).

- **Pause, in phases** (audit A02; `vm/barrier.rs`). A snapshot is a cut of the whole
  machine:
  - The request kicks every vCPU, and each parks out of the guest, capturing nothing yet.
  - Once all have, the coordinator pauses the devices at a request boundary (each
    device's worker stops after the request it is answering and hands back its queue;
    what it did not answer stays in the ring, for the worker that resumes): no device
    completes a request or raises an interrupt after that.
  - Only then does each vCPU capture its own state, on its own thread (HVF's
    owning-thread rule): registers, the GIC CPU interface and PSCI power state.
  - The coordinator saves the GIC device, the devices and guest memory, then releases
    the vCPUs or stops. A stop or an error at any phase releases every thread.
- **Kicks survive exits.** A kick can arrive as a vCPU leaves the guest for another exit.
  HVF then reports that exit and drops the cancel, and the vCPU went back into the guest
  to idle there while the barrier waited for it forever: a hang in about one storm run in
  fifteen [PM M45]. Every entry checks for a pending kick first, as KVM checks a vCPU's
  requests before entering it.
- **Format.** System registers are keyed by op0..op2 encoding (ground-truth doc §5 row
  16). The GIC device is the backend's own serialization: HVF's `hv_gic_state`, "the
  complete serialized state of the device, except for the GIC cpu registers"
  (hv_gic_state.h). It holds interrupts HVF has passed on toward a vCPU that has yet to
  take them, which no distributor or redistributor register shows; saved as registers,
  they were lost, and every restore of a guest with a disk request in flight waited on it
  forever [PM M45]. The blob is versioned: a host update that changes it fails the
  restore, and the run boots and saves the template again (D25). The state records one
  guest counter for the whole VM and the CPU ID registers; a restore on another CPU is
  refused. Memory is sparse (zero pages are holes). Each run of pages the guest used is one
  write: on ext4 a write's length sets the order of the page-cache folios a restore maps,
  and restores of a snapshot written that way reached their first beat 17–25% sooner than
  of one written a page per write [PM M37]. Decoding treats the files as untrusted.
- **Generations.** A snapshot directory holds generations, each a directory of its own
  (`state`, `memory`, and later a `working-set`), and `current`, naming the one in use.
  - A write stages a generation under a name of its own, syncs its files and directory,
    renames it into place, and points `current` at it by renaming a synced file over it.
    Writers to one directory take turns under a lock, and each removes what `current`
    does not name.
  - A reader opens the generation `current` names, and both its files through that open
    directory, so no write can pair one generation's state with another's memory. The
    state names its generation; a state under another name is refused.
  - A failure after any step leaves the generation before, or, once `current` moved, the
    one after (audit A03).
- **Backing files.** A machine's disks and pmem files are resolved to absolute paths when
  it is built, and a snapshot records them in the OS's own bytes, with each file's
  device, inode, size and modification time. A restore refuses another file under the
  name, and a file the guest only reads that has changed; a writable disk may have been
  written since. Nothing reads whole files to check them (audit A18).
- **Working sets are bounded** by the guest's pages: RAM, and each pmem region at its
  2 MiB-aligned size. A longer file is not read, a count is checked against the bytes
  left before anything is allocated, and every page must be aligned, distinct and inside
  the guest; otherwise the restore goes without prefetching (audit A16).
- **Restore.** Memory is mapped copy-on-write from the snapshot file; clones share every
  page none of them writes. vCPUs are created in order and loaded with one counter offset,
  so CNTVCT continues and agrees across CPUs. **The GIC distributor is applied only after
  every vCPU exists:** HVF routes an SPI when IROUTER is written, and routing to a CPU
  that does not exist yet loses the SPI (found by the E2E test). Devices follow, and the
  vCPUs are released.
- **Identity.** The control page's `GENERATION` counts restores. Every restore also writes
  a new VMGenID and raises its interrupt (the guest kernel has the `microsoft,vmgenid`
  driver), so each clone reseeds its RNG before its first user instruction. The E2E test
  checks that clones of one snapshot draw different random bytes.
- **x86_64 on KVM.** What Firecracker saves, after KVM's api.rst (fc: arch/x86_64/vcpu.rs,
  save_state and restore_state):
  - per vCPU: MP state, registers, special registers, the XSAVE area (KVM_CAP_XSAVE2's
    size where larger), XCRs, debug registers, the LAPIC, events, the TSC's frequency,
    and every MSR in KVM's list that it reads (a PMU MSR of a guest without a PMU is
    left out); per VM: both PICs, the IOAPIC and kvmclock;
  - kept as KVM's own structures, since a template restores only where it was saved;
  - restored in the order KVM's dependencies need: MP state first, the registers before
    the events (SET_REGS drops a pending exception), the LAPIC after the special
    registers and before the MSRs, IA32_TSC_DEADLINE after IA32_TSC;
  - the interrupt controllers and kvmclock only once every vCPU exists, as the GIC on
    arm64; kvmclock goes on from its saved value;
  - **the TSCs by their offsets** (Linux 5.16+, `KVM_VCPU_TSC_OFFSET`), as KVM documents
    bringing a VM's TSCs back (Documentation/virt/kvm/devices/vcpu.rst §4): each vCPU's
    offset from the host's TSC is saved with its state; at release, vCPU 0's TSC starts
    at the value it saved, and every vCPU's offset becomes its saved one plus the same
    difference, so each TSC goes on from the snapshot as far from the others' as it was,
    and the deadlines are armed after. Writing each vCPU's IA32_TSC instead passes
    through KVM's legacy synchronization, which matches the writes to one another only
    on a host whose TSC it trusts; elsewhere each restored TSC keeps its own capture's
    instant, and the guest's clock went back by up to 166 µs from one CPU to another
    (arch/x86/kvm/x86.c, kvm_synchronize_tsc; CI's x86_64 runners). Where KVM lacks the
    attribute, the MSRs are written as before;
  - a vCPU whose CPUID differs from the snapshot's refuses, as arm64's CPU ID does;
  - the VMGenID reaches Linux's ACPI driver (`VMGENCTR`, with its `ADDR`), and its
    interrupt a Generic Event Device (`ACPI0013`) on GSI 23 whose `_EVT` notifies it, as
    Firecracker declares them; the 16 bytes are the first of the firmware area.
  - The E2E snapshot, template, pooled-run, vsock and pmem tests run on CI's x86_64
    runners (KVM) as on the Mac.

### Platforms (D13)

shards is platform- and architecture-agnostic. The matrix is the sibling projects' release
matrix: 8 targets, all 64-bit. Hardware virtualization runs guests of the host's own ISA, so
the guest arch is always the host arch.

| Host OS | Arch (Rust triple / OCI name) | Backend (`hv`) | Status |
|---|---|---|---|
| Linux (glibc, musl) | x86_64 / amd64 | KVM | booting Linux: SMP, ACPI, virtio-blk; snapshots with cold and warm restore; CI on both libcs |
| Linux (glibc, musl) | aarch64 / arm64 | KVM | planned |
| macOS | aarch64 / arm64 | Hypervisor.framework (arm64 API) | booting Linux; snapshots with cold and warm restore |
| macOS | x86_64 / amd64 | Hypervisor.framework (x86 VMX API) | planned |
| Windows | x86_64 / amd64 | Windows Hypervisor Platform | planned |
| Windows | aarch64 / arm64 | Windows Hypervisor Platform (arm64) | planned |

A Linux host runs VMs on Linux 6.10 or later with Landlock enabled (its `lsm=` list), whose
ABI v5 is the first to govern `/dev/kvm`'s ioctls: a VM process starts under no weaker
confinement (D30). macOS runs them under App Sandbox (D30).

Every target builds and passes the lints. Until a target's backend lands, `vm run` there
fails with an explanation (`vm::check_host`), as it does where the OS reports no hardware
virtualization, such as hosted CI runners without nested virtualization.

Code layers:

- **`hv`**: one backend per host platform. build.rs selects it at compile time as
  `cfg(hv = "...")`. Every backend presents KVM's semantics, so the VM runtime is written once
  (ground truth: [hvf-arm64-kvm-ground-truth.md](../research/hvf-arm64-kvm-ground-truth.md) §5):
  - Device accesses complete inside `Vcpu::run` through the `Io` bus callback. On HVF the
    backend writes Rt and advances PC; KVM completes on the next `KVM_RUN`; WHP's instruction
    emulator completes them.
  - Firmware power management stays in the backend. On HVF, PSCI (CPU_ON included) runs in
    the backend, and a powered-off vCPU parks inside `run`, like a KVM vCPU created with
    `KVM_ARM_VCPU_POWER_OFF`.
  - `run` returns only `Canceled` (a kick), `Shutdown` or `Reset`.
- **`arch`**: per guest architecture. Covers memory map, boot protocol (arm64 `Image` + FDT;
  x86_64 64-bit boot protocol/PVH), interrupt-controller description, firmware interface (PSCI
  on arm64) and vCPU reset state.
- **`platform`**: per host OS. Covers memory reservation (mmap / VirtualAlloc), positional I/O,
  durable flush, entropy, thread scheduling policy, and the console.
- **`vm`**: the runtime (vCPU threads, lifecycle, exits) written once. There is one machine
  per guest architecture (arm64: memory map, GICv3, devicetree, devices), which is also
  backend-neutral.
- **Written once**: everything else — devices, virtio, the runtime, and guest software (built
  for every arch's musl target).

### Images (D15)

The host flattens an image's layers into one EROFS image, which guests mount from
virtio-pmem with DAX ([image-storage](../research/image-storage.md) R1, R2). The code is
`crates/image`.

- **Regular files are plain**, whole blocks, so the guest serves them with DAX: what a VM
  reads stays in the host's page cache, shared by every VM of the image, where a file with
  an inline tail is copied into the VM's own memory. A python VM that imported 24 modules
  kept 42 MB against 63, for images 3 to 6% larger [PM M74]. Directories and symlinks keep
  their tails inline.

- **Reading layers.** Docker, containerd and BuildKit read and write layers with Go's
  `archive/tar`, so our reader accepts what it accepts. That covers V7, ustar, star, GNU
  and PAX headers and base-256 numbers. Global PAX headers are ignored, as containerd
  ignores them. Sparse files are refused, since layers SHOULD NOT use them (image-spec
  layer.md).
  - Evidence: Go 1.27.1's own test archives, and what Go reads from each, are checked into
    `crates/image/testdata/go-tar`. Our reader returns the same 50 entries and refuses the
    6 sparse and dumpdir ones.
- **Stacking layers.** Each layer applies at its own paths over the layers below. This is
  how containerd v2.4.1 applies a layer for its default overlayfs snapshotter: into the
  layer's own upper directory (`core/diff/apply/apply_linux.go`), with
  `pkg/archive/tar.go`'s rules.
  - Whiteouts apply first, and only to lower layers (layer.md). Directories merge.
  - A lower layer's symlinks are not followed; the layer's own are.
  - Missing parents are made 0755. Entries for the root are ignored.
  - Attributes follow containerd's `createTarFile`: `trusted.*` xattrs are dropped, and
    times outside Go's range become 0.
  - Where appliers disagree, rootful containerd with overlayfs decides. Its naive and
    rootless appliers follow lower symlinks instead.
- **Differs from R1:** xattrs come only from `SCHILY.xattr` records, the ones containerd
  applies. `LIBARCHIVE.xattr` records are ignored, as containerd ignores them.
- **Tests:** unit tests cover each rule. An E2E test stacks three layers (whiteouts, an
  opaque directory, hard links, a file capability). A guest mounts the image over pmem and
  must find exactly the expected tree.

### Running a workload (D16)

`shards vm run --rootfs IMAGE -- COMMAND` boots into an image and runs a command there as
`docker run` does. The guest side is `crates/init/src/run.rs`; the host side is
`crates/shards/src/workload.rs`.

- **Root filesystem.** shards-init mounts the image from `/dev/pmem0` (EROFS,
  `dax=always`) as the lower layer of an overlay with a tmpfs upper (image-storage R3, R4).
  - It moves the overlay over the initramfs, which cannot be unmounted
    (`Documentation/filesystems/ramfs-rootfs-initramfs.rst`).
  - It then makes Docker's mounts (moby `daemon/pkg/oci/defaults.go`): proc, read-only
    sysfs, devpts, a 64 MiB `/dev/shm`, and mqueue.
  - `/dev` is devtmpfs, the VM's own devices.
  - Unlike Docker, there is no cgroup mount, `/etc/hosts` or `resolv.conf` yet.
- **Protocol.** Once the image is mounted, the guest dials host port 1024 over vsock, so
  the host never polls. A restored guest can dial again (D14).
  - One connection carries everything, in the frames of Docker's attach streams
    (stdcopy). The host sends the workload and stdin; the guest sends stdout, stderr,
    errors and the exit status.
  - The guest waits for the host to close the connection before powering off, so the
    status is never lost in flight.
- **Semantics**, each from Docker's own sources:
  - `-u`: moby/sys/user v0.4.1 `GetExecUser`, held to its own test cases. The primary
    group comes first (moby `getUser`).
  - Environment: Docker's `PATH` and `HOSTNAME`, then `-e` (moby
    `CreateDaemonEnvironment` and `ReplaceOrAppendEnvValues`; docker/cli `ValidateEnv`).
    Then runc v1.5.2's `prepareEnv`: the last value wins, and `HOME` comes from
    `/etc/passwd`, else `/`.
  - The working directory is made 0755 if missing (moby `SetupWorkingDirectory`). The
    command is found on `PATH` as Go's `exec.LookPath` finds it, as the user.
  - The exit status is what `docker run` reports (docker/cli v29.8.1 `toStatusError`,
    cli/command/container/run.go):
    the command's own; 128 plus a fatal signal; 127 if the command is not found; 126 if
    it is not executable or is a directory; 125 otherwise.
  - The run ends with the main process. Everything left is killed, as when a container's
    PID namespace ends.
- **Warm runs.** `vm run --rootfs IMAGE --snapshot-dir DIR` saves a template: shards-init
  asks for the snapshot once the image is mounted and the kernel's crypto self-tests have
  finished (at most 2 s), before it dials the host. Each
  `vm restore DIR -- COMMAND` resumes a copy that dials in for its own command (D2, D14).
  Copies share the image and nothing they write.
  - With `--hold`, the copy resumes at once, reseeds and connects. Its request is then only
    the command. It is answered in 1.0 ms at p50 and 2.4 ms at p99, over six templates
    (benchmarks.md, "Run").
  - Before init waited for the self-tests, most templates stalled a restored guest for a
    tick after its release: it replayed their remaining RSA work, and this kernel is
    `PREEMPT_NONE` (PM M21). Now `warm_resume` is 163 µs at p50.
- **A standby.** Once the image is mounted, and before any snapshot, shards-init reads the
  image's user database and forks the workload's process. That process waits on a pipe
  for its orders: the command, environment, user and working directory init resolved
  from the request. So no run waits for a fork, and every copy of a template has its
  standby. A standby that has ended is replaced when the request comes. The run saves
  about 45 µs at the median and 90 µs at p99. The rest of the fork's cost reappears as
  the standby's first touches of memory after a restore [PM M27].
- **Kernel command line:** `noautogroup`, because otherwise most templates stalled a tick
  in the first `setsid(2)` after a restore (benchmarks.md, "Run").
- **Wall clock.** shards-init sets `CLOCK_REALTIME` to the host's time at boot and after
  each restore, before any workload runs, as a container shares its host's clock.
  - The kernel reads the RTC in whole seconds. A restored guest's clock is its
    snapshot's: one restored 4 min 41 s after its template was saved read the save time.
  - The host's time comes from the control page (`HOST_TIME`), read in one 64-bit access:
    the VMM answers with its clock at that moment. The monotonic clock stays continuous,
    and wall time is stepped once, as NTP steps it.
  - E2E: the guest's time lies between the host's readings before and after the run,
    booted or restored. Without the sync after a restore, the check fails, 106 ms behind.
- **Signals** reach the command as `docker run --sig-proxy` forwards them (docker/cli
  `signals.go`): by name, as Linux numbers them.
  - Docker forwards every catchable signal but CHLD, PIPE and URG. shards leaves out also
    those a fault raises (BUS, FPE, ILL, SEGV, SYS, TRAP), which a process cannot block
    safely, and those Linux has no number for.
  - They travel on a second vsock connection that the guest opens once the command runs,
    so unread stdin cannot hold them up.
  - Through the daemon, a signal that arrives once the container is made, before the
    command runs, waits for it (D26). `vm run`, whose process is the VM, ends as that
    signal would end it. dockerd instead drops a signal sent before its container starts
    (warm-pool-daemon.md §2.7).
  - Before the container is made, as the image is pulled, SIGINT and SIGTERM end the
    client with 128 and their number, saying nothing. The daemon sees its client's
    connection end, lets the pull go and makes no container. That is `docker run`
    interrupted as it pulls, measured on Docker 29.3.1 (its CLI cancels its request
    until `ContainerCreate` returns, docker/cli `notifyContext`). The daemon tells the
    client the container is made (`CREATED`), from which point it forwards signals. A
    client that went hears nothing more of its run: the daemon holds its stderr.
  - A signal shards was started ignoring is forwarded too, as the Docker CLI's Go
    runtime takes it once asked. A script's `shards run ... &` starts with SIGINT and
    SIGQUIT ignored, and macOS drops an ignored signal even for a thread in `sigwait`
    [PM M28].
- **Terminals** (`run -t`), as Docker gives a container one
  (docs/research/tty-and-interactive-runs.md):
  - shards-init opens a pty per request with runc's steps: a new master from
    `/dev/ptmx`, unlocked; the size the client's stdout has, if both dimensions are
    nonzero; the peer owned by the command's user, its group left to devpts. The
    standby opens the peer after `setsid` and makes it its controlling terminal
    (`TIOCSCTTY`), so the command leads a session in the terminal's foreground.
  - The pty keeps the kernel's settings, as nothing on Docker's path changes them: the
    command's output reaches the host as one stream, with `\r\n` line ends and its
    echoes, and `logs` keeps it so. `TERM=xterm` joins PATH and HOSTNAME, as dockerd
    sets it.
  - Its input never closes, as dockerd keeps a TTY container's stdin open: under `-t`
    alone nothing writes it, and under `-it` the client's end only stops writing.
  - init reads the master until EIO, which Linux returns once every holder of the peer
    has gone and nothing is left to read; ending at POLLHUP would lose output.
  - A resize travels beside the signals, as its own frame; the kernel then signals the
    terminal's foreground group, if the size changed.
- **Not yet:** a terminal for `vm run --rootfs`, whose commands are shards' tests and
  benchmarks. Detached runs came with containers (D27).
- **Tests:** E2E runs a minimal image (no `/proc`, `/sys` or `/dev`). It covers users,
  groups, the environment, working directories, mounts and every exit status. It also
  sends 8 MiB through stdin and back, and reads 32 MiB of output.

### Mounts (D17, design)

A mount applies to one workload, a subset of the workloads, or all of them. It is
attached in one step: the user names it once, with the workloads it is for. shards does
not mount it into the microVM and again into each workload, which would also show it to
workloads it is not for. Built with the in-VM runtime (phase 4).

- **One source, one instance.** Each source (a host directory, a volume, a device) gets
  one transport into the VM and one filesystem instance there, however many workloads
  use it.
- **Attached only where it applies.** Linux v6.18 (`fs/namespace.c`) supports this:
  - init creates the instance as a detached mount (`fsopen`/`fsmount`), which appears in
    no mount tree;
  - it clones the mount for each target workload (`open_tree` with `OPEN_TREE_CLONE`;
    `may_copy_tree` permits clones in the namespace the mount came from);
  - it attaches each clone inside the target workload's mount namespace (`move_mount`;
    `do_move_mount` attaches a detached mount wherever the caller is).
  - The clones share one superblock.
- **Scopes.** One workload or a subset get clones at attach time. "All workloads" also
  records the mount, so every workload created later gets a clone when it starts.
- **Detail:** a thread that shares its filesystem context cannot join a mount namespace
  (`mntns_install` refuses it). So attaching runs in a dedicated thread that has
  unshared `CLONE_FS`.

### Image store (D18)

The store keeps what a pull fetches and what guests boot from. The code is
`crates/image/src/store.rs`.

- **Blobs as served.** Each blob is kept under its digest, as the registry served it,
  compressed layers included. A later push or save then reproduces the registry's digests,
  as with containerd's content store and the OCI image layout
  ([registry-pull](../research/registry-pull.md) Q1).
- **Nothing unverified.** A blob is hashed as it arrives under `ingest/`.
  - It is committed only when exactly its descriptor's size arrived and it hashes to its
    digest. containerd v2.4.1 checks in that order (`plugins/content/local/writer.go`;
    registry-pull §5, row 4).
  - Commit is fsync, then rename, so a crash never leaves a torn file under a verified
    name. A name that exists already holds the same bytes, so it is kept; it may be
    mapped by a running VM.
  - A small blob (a manifest, index or config) is checked again whenever it is read
    (`Store::content`; audit A11). It is read whole under its limit and hashed, and then
    its length is compared with its descriptor's: content of another length is not to
    be trusted (image-spec descriptor.md). A stored copy that has changed is refused
    from the store alone, by name; a pull fetches it again and renames it into its
    place, so readers find the old file or the new one, never none. `run --pull
    missing` pulls again for it, saying why; `--pull never` refuses.
- **References** record the descriptor their manifest was chosen by: its media type, size
  and any platform an index labelled it with. Finding the image again then checks what
  pulling it checked (audit A11). Their directory is versioned like the root
  filesystems': a record of an older shape is not read, and its image is pulled again.
  A record is written only once the directories of what it names (the blobs, the root
  filesystems) are synced, and its own directory is synced after: no power loss leaves
  a record naming what it lost (audit A15).
- **Layers unpack as containerd unpacks them** (registry-pull §5, rows 6 and "Layer media
  types"):
  - **Media type.** It decides whether compression is sniffed (`DiffCompression`,
    `core/images/mediatypes.go`).
    - Docker's layer types, and OCI's with a last suffix of `gzip` or `zstd`, are
      sniffed. OCI's plain tar is read as it is.
    - Anything else is refused, and so are encrypted layers.
  - **Sniffing** reads the first 8 bytes, as `DetectCompression` does
    (`pkg/archive/compression/compression.go`): gzip, a zstd frame, a zstd skippable
    frame with its whole header, or none.
  - **gzip** streams may have many members (flate2). flate2 refuses reserved header
    flags, as RFC 1952 §2.3.1.2 requires. Go's reader ignores them.
  - **zstd** streams may have many frames, and skippable ones (ruzstd). Checksums are
    verified, and windows are capped at 512 MiB, as klauspost/compress v1.20.0 decodes
    for containerd.
    - klauspost lets single-segment frames reach 64 GiB and buffers them whole. We cap
      those at 512 MiB too.
  - The whole decompressed stream must hash to the layer's DiffID, including bytes after
    the tar's end (`core/diff/apply/apply.go` reads those too). It must also stay under a
    size cap.
- **Root filesystems by ChainID.** An image's EROFS (D15) is built once and kept by
  ChainID, so images with the same layer stack share one. One build at a time goes on in
  a store, under its lock, whichever process asks, and a second build of an image finds
  the first's (audit A10).
- **Limits** (audit A10). By default nothing but the machine bounds a build, as nothing
  bounds containerd's unpacking or a BuildKit build: containerd v2.3.6 applies a layer
  with no cap on its decompressed bytes or entries (`core/diff/apply`, `pkg/archive`), and
  BuildKit v0.33.1 only collects its cache after builds, by a policy sized from the disk
  (`cmd/buildkitd/config/gcpolicy.go`, `control/control.go`). A full disk fails the
  write, and the build leaves nothing. The defaults had been multiples of three images'
  sizes (PM M47) and a per-entry memory cost since cut by four (PM M78): a guess, now
  gone. Each setting of the daemon sets a limit where an operator wants one; a build
  then refuses, and leaves nothing, once it would pass it:
  - `SHARDS_MAX_IMAGE_BYTES`: what its layers decompress to, together, which `ingest/`
    holds while it builds. An image whose layers, as its manifest declares them, are
    larger is refused before any is downloaded.
  - `SHARDS_MAX_IMAGE_ENTRIES` and `SHARDS_MAX_IMAGE_METADATA`: its entries, held in
    memory as it builds, and the bytes of their names, links and xattrs.
  - `SHARDS_KEEP_FREE`: what it leaves free on the store's filesystem, looked at as it
    starts and every 64 MiB it writes. Downloads keep it too: one whose declared size
    would not leave it is refused before it starts, and one stops as it goes once it
    would. Unset, nothing is looked at.
  - The unpacked tars exist only while it is built.
  - Its directory is versioned, and the version is bumped whenever the EROFS writer's
    output changes.
- **Private.** Blobs can come from private registries, so the store's root is private to
  its user, as containerd makes its root 0700 (`cmd/containerd/server/server.go`). The
  store opens a root its caller has made.
- **Tests** cover:
  - blobs committed only on a match;
  - two-member gzip, and plain tar labelled gzip;
  - two zstd frames around a skippable one, and a bad zstd checksum;
  - the sniffing edges and every media-type rule;
  - one rootfs build per chain.

### Registry TLS (D19)

Pulls use rustls 0.23.45 with the AWS-LC provider (aws-lc-rs 1.18.1), verified by
rustls-platform-verifier 0.7.1 ([registry-pull](../research/registry-pull.md) R2, R3).
The code is `crates/registry/src/tls.rs`.

- **AWS-LC, not ring**, the siblings' provider (registry-pull §6.2):
  - It offers X25519MLKEM768. Docker Hub's token host and CDN, GCP, ECR and GHCR's blob
    host negotiate it, so registry passwords and refresh tokens resist
    harvest-now-decrypt-later.
  - Parts of it are formally verified; ring has no such proofs.
  - Both need a C compiler for every target, so the siblings' reason for ring ("builds
    without cmake") no longer separates them.
- **Vendored.** aws-lc-rs and aws-lc-sys build from in-repo copies, verified file for
  file against their published crates, in CI too (`vendor/README.md`).
- **Assembled from source.** aws-lc-sys's prebuilt Windows x64 objects are disabled, so
  NASM assembles that code. With NASM hidden, the Windows x64 build fails.
- **Seeded by the OS.** AWS-LC's DRBG seeds from the OS CSPRNG, as BoringSSL and ring
  seed, not from its default CPU jitter source (`AWS_LC_SYS_NO_JITTER_ENTROPY=1`).
  - Jitter entropy serves FIPS's two-source rule; ours is the non-FIPS build.
  - It cost every new process 17 ms before its first random bytes, against 7 µs from
    the OS (platform-measurements.md M19): more than three times our start budget.
- **Verified as Docker verifies.** On macOS and Windows the OS verifies, as Go delegates
  to it there; Linux uses webpki with the system bundle. Per-registry CAs are extra roots.
- **TLS 1.2 stays on:** a Docker-operated CDN host refuses TLS 1.3.
- **One C crate.** `crates/image` stays pure Rust. Every target lints from one host
  (`scripts/lint`): zig compiles the C for Linux, and cargo-xwin with clang-cl for
  Windows. The cost is measured (platform-measurements.md, M1).
- **Tests:** a loopback registry certified by a test CA.
  - It is trusted only through that CA, over TLS 1.3 with X25519MLKEM768.
  - A TLS 1.2-only host is still reached.

### Registry HTTP (D20)

Registries are reached with a small blocking HTTP/1.1 client on rustls and httparse
(registry-pull R4). The code is `crates/registry/src/http.rs` and `url.rs`.

- **Why our own.** Every probed registry, token host and CDN speaks HTTP/1.1 (§6.4).
  Pulls need exact control of redirects, credentials and streaming. ureq reaches
  providers other than ring only through an API it marks unstable, and hyper is async.
- **Responses are read as Go 1.27.1's net/http reads them** for Docker and containerd:
  - 1xx responses are skipped, and heads may total 10 MiB.
  - Framing: no body after HEAD, 1xx, 204 or 304. Chunked only when
    `Transfer-Encoding` is exactly `chunked`. Else a `Content-Length` whose copies
    agree. Else until the connection closes.
  - Chunked bodies as `internal/chunked.go` reads them:
    - lines end in CRLF only (RFC 9112 erratum 7633) and hold at most 4096 bytes;
    - sizes have at most 16 hex digits;
    - overhead is bounded, and trailers take at most 4 KiB.
  - A connection is not reused after a response with both framings, with bytes past its
    end, or after a 408, whose server stopped waiting there and may hold part of a
    request (RFC 9110 §15.5.9). The second rule is Go's too, and it keeps responses from
    desynchronizing.
- **Connections as containerd's transport keeps them** (`core/remotes/docker/registry.go`):
  - 30 s to connect, racing addresses 300 ms apart (RFC 8305);
  - 10 s for the TLS handshake, 30 s for the response head;
  - at most 10 idle connections, each kept for 30 s.
  - A GET or HEAD that fails on a reused connection before its response is resent on a
    new one, as Go resends replayable requests.
  - A request a reused connection answers with 408 is resent on a new one, whatever its
    method: the server's idle timeout crossed it, and it never had the request whole
    (RFC 9110 §15.5.9: one in transit "MAY" be repeated). Chromium resends it so
    (net/http/http_network_transaction.cc, 687b43f); Go drops an idle connection on its
    408 only once it has come, and returns one that crosses a request to the caller
    (go1.26.1 net/http/transport.go `readLoopPeekFailLocked`, `shouldRetryRequest`).
    Even a local server's 408 written before the client takes the connection can come
    after: macOS gives loopback no input thread of its own, and queues its packets for
    the main input thread rather than taking them in the sender's write (xnu-11417.101.15
    bsd/net/dlil.c `ifnet_attach`; dlil_input.c `dlil_input_handler`,
    `dlil_input_async`). Found as a test failing under load.
  - A body that makes no progress for 30 s fails, where Go would wait on its context.
- **URLs** follow RFC 3986 (iri-string): references resolve as §5.2 says, and nothing is
  normalized, so a presigned URL keeps the exact bytes its signature covers. Messages
  never show a query.
- **Fields** holding CR, LF or NUL are refused (RFC 9110 §5.5), so a token from a server
  cannot inject fields.
- **Proxies** as Go's net/http takes them from the environment (x/net httpproxy, as
  go1.26 vendors it; `crates/registry/src/proxy.rs`): `HTTPS_PROXY` for https and
  `HTTP_PROXY` for http, uppercase first, `HTTP_PROXY` refused under CGI; loopback and
  what `NO_PROXY` names (`*`, CIDRs, addresses with or without ports, domains and their
  subdomains) go direct. HTTPS goes through a CONNECT tunnel, over TLS to an https proxy,
  with the proxy's credentials from its URL; plain HTTP goes to the proxy whole. The
  daemon reaches registries through the proxies of the client that asks.
  - Unlike Go: a proxy value that is no URL, or names SOCKS, fails the requests it would
    carry, where Go goes direct; `NO_PROXY` names are compared as written, not punycode.
- **Content-Encoding**, as containerd v2.4.1's fetcher asks for and decodes it: zstd,
  gzip and deflate accepted, each coding undone, last first; a resumed download asks for
  the blob as it is, so its range counts the bytes already stored.
- **Refusals** in Docker's words: measured on Docker 29.3.1 against registry:2 with
  basic authentication, and otherwise taken from containerd v2.4.1 and moby.
  - Each request's: `unexpected status from <METHOD> request to <URL>: <status>`. A
    fetch's 404 is `content at <URL> not found`, a resolve's `<reference>: not found`.
    A HEAD's 403 takes its reason from a GET answered 403 too (`withGETErrorBody`).
  - A pull's or push's, as dockerd translates it (`translateRegistryError`): the
    registry's own errors (`error from registry: …`), or a token server's `details`;
    else `unknown:` before the error. A refused token, or no credentials for basic
    authentication, is `pull access denied …` or `push access denied …`.
  - Unlike Docker:
    - a URL's query is hidden, where containerd prints a CDN's signature or an upload's
      state;
    - control characters in what a registry says are escaped, where Docker prints them
      to the terminal;
    - an error that is only its code is said once, where dockerd says it twice;
    - a 429 is followed by its rate limits (§3.2).
- **Tests:** a scripted loopback server covers:
  - every framing and 1xx skipping;
  - 16 malformed responses, all refused;
  - reuse of plain and TLS connections, a stale pooled connection replaced, and a 408 on
    a reused connection resent, on a new one not;
  - RFC 3986's own resolution examples.

### Registry auth (D21)

Pulls authorize as containerd v2.4.1 authorizes ([registry-pull](../research/registry-pull.md)
§3.1, R5). The code is `crates/registry/src/auth.rs`.

- **Challenges** are parsed as `auth/parse.go` parses `WWW-Authenticate`:
  - one per line, with bearer preferred to digest, and digest to basic;
  - parameters are RFC 2616 tokens or quoted strings;
  - parsing stops at the first byte that doesn't fit.
  - containerd's own test cases pass.
- **Tokens** are fetched as `auth/fetch.go` and `authorizer.go` fetch them:
  - Anonymous: a GET that adds `service` and each scope to the realm's query, encoded
    in Go's sorted order.
  - With a password or an identity token: an OAuth2 POST (`client_id=shards`). Where
    the server has no OAuth2 endpoint (404, 401, 400, or 405 with a username), a GET
    with Basic authentication.
  - Tokens are cached per host and sorted set of scopes, with one fetch per set at a
    time.
  - A registry token goes to its registry as it is and is never retried, as dockerd
    sends it.
- **Stricter than containerd:**
  - A realm must be https, unless it and the registry are both plain HTTP on loopback:
    credentials never cross a network unencrypted. containerd sends them to any realm
    a registry names.
  - A token lives from `issued_at`, or its receipt, for `expires_in` and at least 60 s,
    as distribution's client counts. containerd keeps a token without `expires_in`
    forever.
  - Credentials go to their registry's host alone, and redirects carry no
    `Authorization` to another host (D20).
  - A request refused twice in a row is not retried. An `error=` challenge starts the
    host's handler over once.
- **Tests:** a scripted token server checks each flow's exact requests:
  - the query, the form, Basic authentication, and the OAuth2 fallback;
  - caching, and expiry from `issued_at`;
  - malformed answers, the realm rules, registry tokens and Basic challenges.

### Pulls (D22)

A pull resolves, fetches and checks as containerd v2.4.1 does
([registry-pull](../research/registry-pull.md) R1, R6, R8). The code is
`crates/registry/src/registry.rs` and `pull.rs`.

- **Hosts** are reached as containerd's defaults reach them: Docker Hub at
  `registry-1.docker.io`, loopback hosts over plain HTTP, all others over https.
- **Requests** retry as `doWithRetries` does, at most 5 times:
  - a timeout or cut connection is tried again after 50 ms;
  - a 401 is answered (D21), then the request is sent again;
  - a manifest HEAD refused with 405 becomes a GET;
  - 408 is tried again, and a 500, 503 or 504 once.
  - Unlike containerd, a 429 is never retried. Docker Hub counts pulls over hours, so
    the error reports its `ratelimit-*` fields and `Retry-After`.
- **Resolve** follows containerd's `Resolve`:
  - a HEAD of the tag or digest with its `Accept` list;
  - the digest comes from the reference, else from `Docker-Content-Digest` with a
    `Content-Length`;
  - a digest reference falls back to `blobs/` only after a 404.
  - When a GET was needed, the manifest is verified and kept, so it is never fetched
    twice.
- **A pull:**
  - chooses the platform manifest (D15's platform rules) and checks an unlabelled
    one by its config;
  - refuses anything but an image config, and any layer type it cannot read, before
    downloading;
  - reads the config whole, and at most 4 MiB of it, as containers/image v5.36.2 reads
    configs for Podman, CRI-O and skopeo (`MaxConfigBodySize` in
    `internal/iolimits/iolimits.go`, read in `ConfigBlob`, `internal/image/oci.go`);
  - requires one DiffID per layer.
  - Layers download 3 at a time, dockerd's default. Every size, digest and DiffID is
    checked before the EROFS image is built and the reference recorded.
- **Found again** (`local`), an image is checked as its pull checked it (audit A11). Its
  recorded manifest and its config come through the store's checks (D18). One function
  checks both paths:
  - the config's type and size, and the layers' types;
  - the platform, by the index's label, else by the config's own;
  - the rootfs type;
  - one DiffID per layer.

  A changed spec is refused rather than run, and no layer stack is built short.
- **Resuming.** A blob downloads into `ingest/`, one file per digest.
  - The file is locked (`File::lock`) while in use, so one process at a time downloads
    a blob. Its commit renames it before the lock is released.
  - A cut download resumes with `Range: bytes=<offset>-`, after the bytes already there
    are hashed again.
  - A server that ignores the range sends the whole blob, and the download starts
    over. Three stops without progress fail it, as containerd's `httpReadSeeker` gives
    up.
- **Tests:** a fake registry answers behind a bearer token, and redirects blobs to a
  CDN on another origin that must never see the token. One layer's first download is
  cut in half. They check:
  - the whole pull, and that a second pull costs one HEAD;
  - platforms our guests can't run, DiffID mismatches and tampered bytes, all refused;
  - rate limits reported, not retried;
  - stored documents changed under their digests, and every check a pull makes,
    refused when the image is found again too.

### Credentials, certificates and `shards pull` (D23)

`docker login` state and Docker's certificates work unchanged
([registry-pull](../research/registry-pull.md) §3.3, §6.3, R5). The code is
`crates/registry/src/credentials.rs`, `certs.rs`, and `crates/shards/src/pull.rs`.

- **Credentials** are found as the Docker CLI v29.8.1 finds them:
  - `config.json` from `$DOCKER_CONFIG` or `~/.docker`;
  - Docker Hub's key `https://index.docker.io/v1/`, or else the host;
  - `DOCKER_AUTH_CONFIG` first; then the host's `credHelpers` entry, `credsStore`, or
    `auths`;
  - the platform's default helper when the config holds no credentials at all;
  - helpers spoken to over their protocol, with `<token>` marking an identity token.
  - dockerd's precedence: a registry token, then an identity token, then a password.
  - Nothing is written: `shards login` comes later.
- **`certs.d`** is read as dockerd reads it:
  - `*.crt` files hold CAs, beside the platform's roots;
  - a `*.cert` with its `*.key` is a client certificate. dockerd offers all of them;
    rustls takes one, so we use the first by name.
  - The directories are Docker Desktop's `~/.docker/certs.d`, the rootless engine's
    `$XDG_CONFIG_HOME/docker/certs.d`, and the native engine's `/etc/docker/certs.d` or
    `%PROGRAMDATA%\docker\certs.d` (docker/docs `engine/security/certificates.md`).
  - One TLS configuration serves every host of a pull (registry, token realm, CDN),
    as dockerd's per-registry client does.
- **The store** lives in `$SHARDS_HOME/images`, else in the platform's data directory:
  - `~/Library/Application Support` on macOS, per Apple's guidance;
  - `$XDG_DATA_HOME` or `~/.local/share` elsewhere on Unix;
  - `%LOCALAPPDATA%` on Windows.
  - It is created 0700, as containerd creates its root.
- **`shards pull`** prints `docker pull`'s lines: the default tag, `Already exists` or
  `Download complete` per layer, `Digest:` and `Status:`.
- **Checked against real registries** (2026-09-28, from the M5 Max):
  - Docker Hub (`alpine`, and by digest), GHCR (`ghcr.io/containerd/busybox:1.36`) and
    Quay (`quay.io/prometheus/busybox`).
  - `alpine` took 1.5 s the first time and 0.33 s once stored.
  - Its EROFS image booted in a shards microVM and ran `/bin/sh` in Alpine 3.24.2.
- **Tests:**
  - fake credential helpers that answer, mark an identity token, have nothing, or fail;
  - `DOCKER_AUTH_CONFIG` and its fallback;
  - `certs.d` CAs, and a mutual-TLS handshake with and without its client certificate.

### `shards run IMAGE` (D24)

`shards run [OPTIONS] IMAGE [COMMAND] [ARG...]` runs a command in a new microVM booted
into an image, as `docker run` runs one in a new container. The code is
`crates/shards/src/run.rs`. The VM and the workload are D16's.

- **The image** comes from the store, or is pulled first (D22, D23), as `docker run`
  pulls it: "Unable to find image … locally", then `docker pull`'s lines, on stderr.
  `--pull missing|always|never` as for `docker run`.
- **The workload** merges the command line over the image's config, as dockerd merges
  them (moby docker-v29.8.1 `daemon/commit.go`, `merge`):
  - the user and working directory are the image's unless given;
  - the environment is the given variables, then each of the image's whose name was
    not given, and D16 lays that over Docker's `PATH` and `HOSTNAME`;
  - the image's command applies only when neither an entrypoint nor a command is given;
  - its entrypoint applies unless one is given, and `--entrypoint ""` clears it.
- **Not yet:** ports and volumes. A kernel and shards-init that ship with shards came
  later (D28), as did terminals and detached runs (D16, D27).
- **Checked against Docker Hub** (2026-09-28): `alpine`, `busybox:1.36` pulled on
  demand then run (1.0 s in all), and `hello-world` from its own `Cmd`.
- **Tests:**
  - unit tests for each rule of the merge;
  - E2E, a loopback registry and a real VM: the image's user, directory and
    environment apply. A second run uses the stored image, fetches nothing, and the
    command line wins.

### Templates for `shards run` (D25)

Repeated runs of an image restore a template of it instead of booting (D2, D14). The code
is `crates/shards/src/run.rs` and `crates/shards/src/guest.rs`.

- **The guest, by content.** `shards guest use --kernel FILE --init FILE` copies both
  files into `$SHARDS_HOME/guest`, named by their SHA-256, and records them as the guest.
  A run then knows what it boots by digest, without reading either file.
- **Templates, by content.** A template's name is the SHA-256 of what goes into it:
  - the snapshot format;
  - the kernel's and init's digests;
  - the image's root filesystem, whose EROFS file is named by ChainID (D18);
  - the CPU count, memory and kernel command line.

  A change to any of these names another template. Nothing is compared by time.
- **The first run saves it.** The first run of an image on the guest in use boots. Once
  the image is mounted it saves the template, then resumes and runs the command
  (`AfterSnapshot::Resume`, D16).
  - It saves into a directory of its own, renamed into place only when complete.
  - If another run's template got there first, the other copy is removed.
- **Later runs restore it.** A template that does not restore is removed; the run boots
  instead and says so on stderr, and the next run saves the template again.
- **A run records the working set**: each guest page it touches until its command
  answers, or a while after the command starts, saved with the template as
  `working-set`. REAP found that restored copies of a serverless function's snapshot
  touch nearly the same pages (Ustiugov et al., ASPLOS 2021).
  - **HVF: the first run**, from the snapshot on, for up to 50 ms past its command's
    start [PM M30]. Its guest memory is taken away at stage 2 (`hv_vm_protect`), and
    each page goes back on its first fault, read-only if read, so a later write is seen
    too. Recording slows that run's command about sixfold, on a run that is a boot
    anyway. The boot's set covered 99.7–100% of what warm VMs touched while serving a
    run.
  - **KVM: the first warm restore without one** (`vm::RESTORES_RECORD`), from its restore
    on, for up to 1 s past its command's start [PM M33]. A restore's memory is mapped
    from files, copy-on-write, afresh, so the pages its host maps when the recording ends
    are the ones touched, and a private copy is one written (`/proc/self/pagemap`: the
    present and file-page bits, which need no privilege). Recording costs it nothing.
    The saving run's pages are not a restore's: only 5–16% of a restore's request pages
    were among them, while one restore's were 99.7–100% of the next's [PM M33].
  - The whole way from the request to the command's start is recorded, however long a
    host takes over it; the cap only ends a long command's recording.
  - It is written through the directory held open since the snapshot (or the restore),
    since the daemon renames a new template into place meanwhile. A damaged one is
    ignored, and on KVM recorded again.
- **Where it applies.** Builds that can snapshot (HVF on arm64, KVM on x86_64,
  `vm::SNAPSHOTS`, and so `vm::WORKING_SETS`); elsewhere every run boots. `--kernel`
  and `--init`, or `SHARDS_KERNEL` and `SHARDS_INIT`, name files by path, so those runs
  always boot.
- **Saved quiescent.** The kernel is still running its crypto self-tests after init
  mounts the image, for about 20 ms. The template waits for them (D16), or every restored
  run would replay the rest: most stalled for a tick (PM M21).
- **Measured** (docs/benchmarks.md, "Image"), over 10 templates on a busy host: 7.5 ms at
  p50 against 34.2 ms for a boot. It was 16.5 ms before templates waited for the
  self-tests.
- **Not yet:** removing templates and guests nothing uses. A working set is recorded
  once, from its template's first command; later commands' own pages still fault.
- **Tests** (E2E, a real VM):
  - the first run boots and saves one template, and then (HVF) its working set, or (KVM)
    the second run, the first restore, records it;
  - a later restore prefetches it, and a damaged one only costs the prefetch;
  - the second restores it, with no `INIT_STARTED` marker, the image's settings and the
    host's clock;
  - a corrupted template is removed and that run boots;
  - the next run saves the template again, under the same name.

### Warm pool (D26, in progress)

A per-user daemon hands each `shards run` to a **warm VM**: a VMM process that has already
restored the image's template, resumed the guest and let it connect, and now waits only
for a command (D2). The client asks the daemon for a run, passes it its stdio, and waits
for the exit status.

- **Built: warm VMs** (`crates/shards/src/warm.rs`; messages in `crates/ipc`).
  - `shards vm restore DIR --warm FD` is one, where FD is its socket to the daemon.
    `shards vm run … --rootfs R [--snapshot-dir D] --warm FD` boots one instead, saving a
    template on the way if asked.
  - It says `READY` once the guest waits for its command.
  - The daemon answers with `RUN`: the command, plus four descriptors passed by
    `SCM_RIGHTS`: the client's connection, then its stdin, stdout and stderr. They become
    the warm VM's own stdio, so the workload writes straight to the client's. The VM
    answers `TAKEN`.
  - Signals come from the client on its connection, as Linux numbers. Once the VM has
    taken a run, they queue until the command runs.
  - The exit status goes back the moment the command ends, before the VM is torn down.
    Any error reaches the client's stderr first. The VM lets go of the client's stdio
    before the status, so a pipeline reading it ends with the client, and again when the
    client hangs up, so a command outliving its client writes nowhere. A container's
    output likewise stops reaching a `docker run` that has gone.
  - The warm VM serves one request, then exits.
- **Built: the daemon** (`crates/shards/src/daemon.rs`), with `shards run` as its client
  (`client.rs`).
  - One serves each SHARDS_HOME, holding its `daemon.lock`. `shards run` starts it when
    nothing listens on its socket, `daemon.sock` in the home, through `shards daemon
    --detached`, which starts it in a session of its own and exits: init adopts the
    daemon and reaps it when it exits (APUE §13.3). Left the child of the client that
    started it, it stayed a zombie for as long as that client lived.
  - Every process that uses the socket first makes the home its working directory, and
    names the socket relative to it. That name fits a socket address whatever the
    home's path. The per-user directories macOS offers for sockets cost each new process
    0.4–1.3 ms to look up [PM M26].
  - It admits only clients of its own user (`getpeereid`, `SO_PEERCRED`): a home in a
    shared directory must not let another user run commands as this one [PM M25].
  - **Its clients are bounded (audit A07).**
    - A client has 10 s to send its whole request. The deadline bounds the message, not
      each read, so a client trickling it out a byte at a time gains nothing
      (`shards_ipc::recv_by`).
    - It holds at most 256 clients at once, each a thread and up to six descriptors;
      past that, connections wait in the listener's backlog.
    - Out of descriptors, it waits for room rather than spin on a listener that stays
      readable, and accepts again only once a descriptor is free: an accept that fails
      for want of one leaves the client queued on Linux (net/socket.c,
      `__sys_accept4_file`) but drops it on macOS ("Don't put this back on the socket
      like we used to, that just causes the client to spin. Drop the socket.",
      xnu-11417.101.15 bsd/kern/uipc_syscalls.c).
    - It raises its soft limit on descriptors to its hard one, capped at
      `kern.maxfilesperproc` on macOS, as Go's runtime raises its own (go1.25.0
      src/syscall/rlimit.go, after go.dev/issue/46279): macOS starts a process with 256.
  - It keeps up to SHARDS_POOL warm VMs (default 2) of each template it has served, as
    many as its runs need (below). A pool refills once its VM has taken its run, since starting the next VM on the request's
    path cost 200–600 µs [PM M26], and on a thread of its own, so that the run's own
    messages are read as they come. A run with no template boots a VM that saves one on the way, and a
    run with its own kernel and init boots every time. A template whose warm VMs fail
    three times in a row is removed and saved again.
  - **Warm VMs are speculation, and bounded; runs are served regardless** (audit A13,
    A14).
    - A run that finds no VM ready has one started for it, unless one is already
      starting for a run before it: a pool of 0 (`SHARDS_POOL=0`) keeps nothing warm and
      restores each run's VM on demand, and a burst larger than the pool waits for no
      refill. Before, a pool of 0 never filled, and every repeat run waited 60 s and
      failed.
    - All pools together keep at most `SHARDS_WARM_MAX` (16) VMs ahead of runs: a warm
      VM's own memory is 3.4 MiB, so about 55 MiB; RSS, about 20 MB a VM, counts the
      template's pages it shares in full (PM M49).
      A pool that would pass it first ends the ready VMs of the pools least recently
      claimed from. Only a ready VM can be ended, so once a pool's VM becomes ready, the
      pools claimed from since that are short refill again, and it gives way to them.
    - Settings are counts, checked before the daemon serves: a malformed one, a pool
      larger than all pools may keep, or more than 256 warm VMs (the clients' bound, for
      the same threads and descriptors) stop it, and `shards daemon --detached` says
      why to the client that started it, at once.
    - **A pool keeps what its runs need while they come** (`daemon/demand.rs`).
      - Its size is the largest burst of runs seen within its keep-alive, at least one
        and at most SHARDS_POOL. A burst is the runs that arrive within one refill of
        each other, the refill's time, from a warm VM's start to its `READY`, estimated
        as TCP estimates a round trip's, and its window that estimate's retransmission
        timeout, SRTT + 4·RTTVAR (RFC 6298 §2; its 1 s initial RTO until the first
        refill is timed): runs closer together than that may wait for a refill.
      - A pool unclaimed for SHARDS_POOL_KEEP seconds (600) ends its ready VMs and keeps
        none until its next run, which is served all the same: a fixed keep-alive, as
        AWS keeps an idle function 10 minutes and Azure 20 (Shahrad et al., "Serverless
        in the Wild", USENIX ATC 2020, §1). Their hybrid histogram policy, which sets
        each application's keep-alive and pre-warming from its inter-arrival times, is
        not taken: the thresholds that say when its histogram is representative are not
        published, and no traces of agents' runs exist here to set them.
    - **What nothing needs is collected** (`Store::collect`, daemon.rs
      `collect_garbage`), at the daemon's start and once a pull, by a run or by `shards
      pull`, has moved a reference (it leaves `images/collect-due`, which the daemon's
      tick takes).
      - The roots are the references: each one's manifest, config and layers, and the
        root filesystem of its layers' ChainID. Every other blob and root filesystem
        goes, with older versions' records and root filesystems and what `ingest/`
        holds.
      - Content in flight is under the store's lease, a shared `flock` of
        `images/.lease` (flock(2)): a pull holds it until it has recorded its
        reference, `local` while it may build a root filesystem, and a run's
        preparation until its VM has its root filesystem. A collection takes it
        exclusively or not at all, and stays due; it holds the store until it has
        collected the templates too, so no run begins meanwhile.
      - A template records its origin, its root filesystem and guest (`origin.json`).
        One whose root filesystem has gone, that another guest saved, that records no
        origin, or that a daemon before this one left half saved is removed, and its
        pool's VMs ended; one this daemon is saving stays.
      - A running VM keeps what it has open or mapped: removing a file removes its
        name, not the file (unlink(2)).
      - It runs on the thread that accepts clients, which wait in the backlog meanwhile,
        and not while clients wait for descriptors: a file it opened as an accept found
        none would drop that client on macOS. Beside the accepts it did, 1 time in 20.
    - Measured with 1, 10 and 100 templates [PM M49]: 100 keep 16 warm VMs, 54.5 MiB of
      their own; a template without one restores on demand, p50 16 ms against 5 ms
      warm; bursts of 8 find two ready and restore the rest, p50 about 16 ms; every
      run succeeded, and `daemon stop` left no VM. Not measured: 1,000 templates, whose
      35 GiB of templates this machine cannot spare, and low host memory.
  - **It keeps its copies of the client's descriptors until the VM says `TAKEN`.**
    XNU's collector of in-flight descriptors flushes a socket in flight that no process
    holds: the client's connection then read end of stream, and 1 run in 13 to 53 never
    got its signals [PM M24]. A VM that ends before `TAKEN` is replaced.
  - A client of another build, told apart by its binary's file identity, gets
    `RESTART`. The daemon removes its socket, ends its runs as `shards daemon stop`
    does, and exits; the client starts its own, which takes the home once the old one
    has gone. A daemon ending its runs names itself in `daemon.stopping` first, and the
    new daemon and the client wait while that process lives, however long its runs'
    stop timeouts make it; otherwise a daemon waits 20 s for another's lock, and a
    client 30 s for its daemon to listen.
  - It follows each run to its end. The warm VM tells the daemon of its command's start
    (`STARTED`) before its client has any of the command's output, and of its end
    (`DONE`) before its client has the status; a Unix socket's send puts a message in
    the daemon's queue before it returns. Every container command first takes what
    each run has sent (`settle`), under a lock per run that the run's own thread reads
    under too, so it answers with all any client has seen: `ps` lists a container whose
    output has appeared, and not a `--rm` one whose `run` has returned, as dockerd
    records a container's state before `docker run` learns it (docker/cli run.go
    `waitExitOrRemoved`). Waiting for the daemon to acknowledge each instead cost a run
    241 µs at the median (95% [206, 293], `build-ab/ab.py`, n = 400); this costs none
    measurable (+16 µs, 95% [−30, +73]). The daemon can signal a command meanwhile, on
    the same socket.
  - It exits after SHARDS_DAEMON_IDLE seconds (900) with no run in progress and none
    asked for. `shards daemon stop` first ends the runs in progress as dockerd ends its
    containers when it shuts down (moby daemon/daemon.go `shutdownContainer`,
    daemon/stop.go): each command gets its container's stop signal (SIGTERM if it has
    none), then SIGKILL once its stop timeout is up (10 s if it has none, never if it
    is negative), and its VM ends if the command outlives that by 5 s, as dockerd gives
    up 5 s after the longest stop timeout (`ShutdownTimeout`). The settings are those
    the container was made with, read before its record is written if need be. A
    daemon that another build replaces does the same, as dockerd without live-restore
    stops its containers when it restarts: two daemons never keep one home's records.
    Keeping runs through a restart (live-restore) is a later milestone. `stop` returns
    once the daemon has let go of its home: the daemon unlocks it and closes the stop
    connections itself, last, since the kernel closes an exiting process's descriptors
    in no order to rely on (XNU `fdt_invalidate` closes the highest first).
  - **A stop waits for nothing it can end (audit A07).** It shuts down the connections of
    clients still sending their requests and of container commands, so their reads and
    writes fail and their threads return, and cancels what runs being prepared are
    downloading, images or the guest kernel (`shards_registry::http::Cancel`, which shuts
    down the connections in use, as cancelling Go's request context ends them). A run
    waiting for a warm VM gives up; the run is refused, "the daemon is shutting down".
    Runs already handed over end as above.
  - A `wait` ends when its client hangs up, as moby's `ContainerWait` ends with its
    request's context, and forgets its registration; so does a `logs -f`, output or
    none. `stop`, `kill` and `rm -f` go on regardless: "Cancelling the request should
    not cancel the stop" (moby docker-v29.8.1 daemon/stop.go).
- **The command's stdin** is /dev/null, or with `-i` a pipe the client fills from its
  own. Under `-t` it feeds the guest's pty, which never closes (D16).
  - It ends when the client does, as `docker run -i`'s does when its client goes
    (StdinOnce).
  - Only the client reads its terminal. The client then leaves SIGTTIN unforwarded, so a
    background run stops for terminal input as any reader does; a blocked SIGTTIN would
    make the read fail with EIO instead [POSIX XBD §11.1.4].
- **Children get only what they are given.** A descriptor received on macOS is not
  close-on-exec until the `fcntl` that follows (there is no `MSG_CMSG_CLOEXEC`), so a
  child spawned by another thread in between could inherit another client's stdout.
  shards starts children with `posix_spawn` and, on macOS, `POSIX_SPAWN_CLOEXEC_DEFAULT`:
  a child gets only the descriptors named for it (`shards_ipc::spawn`). On Linux every
  descriptor is close-on-exec from the start.
- **Receives take every descriptor.** Each read is a `recvmsg` with room for the most a
  kernel passes at once (254 on macOS), parsed within `msg_controllen`: macOS installs
  descriptors that do not fit and never closes them.
- **Signals reach only children.** The daemon's kills go through the handle its waiter
  holds, and a pid stays the child's until the waiter marks it reaped (`waitid` with
  `WNOWAIT` first), so a kill never reaches a process that reused the pid.
- **Measured** (PM M23):
  - Handing a request and its stdio to a warm process costs 31 µs at p50 and 75 µs at
    p99; §4 budgeted 10–50 µs.
  - A waiting warm VM costs 12.3 MiB of RSS and no CPU.
  - A thin client's process costs 1.4 ms at p50. `shards` costs 3.5 ms before doing
    anything, because its frameworks load at every launch. Only a thin client leaves room
    for the 5 ms target at p99.
- **Built: the thin client.** `shards` links the standard library and `shards_ipc`
  alone. It serves `run` and `daemon stop` itself, and runs `shards-vm` for `vm` and
  `shardsd` for every other command, found beside it, in its own place.
  - Its launch no longer loads Hypervisor, Security and CoreFoundation, nor runs
    AWS-LC's constructor [PM M23].
  - A pooled run fell from 5.15 to 3.4 ms at p50 and from 5.6 to 3.9 ms at p99. The
    client's peak RSS fell from 6.3 to 1.6 MiB [PM M26].
  - Raising the request path's service threads to user-interactive QoS changed nothing,
    on a busy host or a saturated one [PM M22, M26].
- **Built: working-set prefetch.** A warm VM, and a held restore, maps its template's
  working set (D25) before the guest runs, so stage 2 is filled before the request
  instead of on it.
  - **HVF** maps guest memory only as the guest touches it, and a touch from the host
    does not count [PM M5]. vCPU 0 touches it, before its state is restored: a loop the
    VMM maps below RAM for the purpose (`layout::PREFETCH`), with its MMU on over an
    identity map of write-back 1 GiB blocks. It reads each page and adds zero atomically
    to each page the guest wrote, a write that copies the page now and changes no byte.
    It ends with `tlbi vmalle1is`, and the restore then sets every register it used.
  - The alternatives cost more. Reads alone left the writes' faults on the path. Copies
    by the host counted twice their size in the VM's footprint, once the guest mapped
    them [PM M30]. With the MMU off, the loop's accesses would be uncached, and whether
    HVF keeps those coherent with the host's cached copy is undocumented (ground-truth
    doc §5 row 25): a write-back could store a stale byte.
  - Measured: the guest's part of a pooled `alpine true` fell from 1422 to 517 µs at
    p50, and the whole run by 857 µs (95% [834, 891]) [PM M30]. A waiting warm VM holds
    the pages its run will write, 2.9 MiB for that run, at no cost to the run's peak.
  - A template's first two pooled VMs are restored before its working set exists, and
    do not prefetch; on KVM, neither does the third, restored as the recording run took
    its VM.
  - **KVM**: the restore copies the pages the guest wrote while recorded, as its writes
    would (`MADV_POPULATE_WRITE`, Linux 5.14), then vCPU 0, its state restored, maps
    every page into the stage-2 tables (`KVM_PRE_FAULT_MEMORY`, Linux 6.10, with
    two-dimensional paging): writable where copied, since KVM maps a read fault
    writable when the host page is (kvm_main.c `hva_to_pfn_fast`). A write to a page
    mapped read-only would have faulted, copied, flushed and faulted again. Where KVM
    cannot map ahead, the copies alone are made.
  - Measured on nested KVM (GitHub's AMD runners, paired, n = 100 each on 5 runners):
    a pooled `alpine true` took 26–39 ms less than without the working set, of
    191–226 ms, and 9–14 ms less than with the copies alone [PM M33]. On one Intel
    runner mapping ahead cost 1.4 ms against the copies alone, of 13 ms.
  - The pool still refills at the handover. Its restore and prefetch now overlap the
    run's tail, which costs the run about 45 µs. Refilling at the run's end instead
    saved 49 µs at the median, but lands on the start of a run that follows at once
    [PM M30].
- **Tests:**
  - the IPC crate: descriptors that work on arrival, are close-on-exec, and respect the
    limits; descriptors past the limit, or on any part of a message, are closed; children
    that inherit nothing else (a mutation removing the flag fails it), and a reaped child
    is never signalled;
  - the daemon's handoff: a connection handed over while collections run still carries
    the client's bytes (without the wait for `TAKEN` it fails every time), and a VM that
    ends first fails the handoff;
  - E2E, a real warm VM driven by the test as daemon and client:
    - the client's stdio carries the command's;
    - exit statuses are `docker run`'s: an exit code, a signal, 127 with a reason;
    - interactive stdin works, and signals arrive as the command's;
    - the VM lets go of the client's stdio before the status and on hang-up (a VM
      frozen at the status still holds none);
    - the warm VM tells the daemon the command's status, then exits; `--warm` refuses
      stdio and non-sockets;
  - E2E through the daemon: parallel runs keep their own stdio; signals, errors and
    timing reach the client; `-i` stdin ends with the client; a background run on a
    pseudo-terminal stops by SIGTTIN; `stop` ends a run by SIGTERM; a killed daemon, a
    rebuilt binary and idleness each end what they should.

### Containers (D27)

A run is a container, as in Docker. The daemon names it, follows it to its end, and keeps
its record after it, until `shards rm` removes it; `--rm` removes it once it ends.

- **Command lines** (`crates/cmdline`). `run` and the container commands (`ps`, `wait`,
  `logs`, `rm`, `stop`, `kill`, each also under `container`) are read as docker/cli
  v29.8.1 reads them. The thin client answers `--help` and usage mistakes itself, with
  no daemon, as `docker` answers them without dockerd; the daemon reads the same words
  by the same code.
  - pflag's parsing: shorthands run together, `=` values, booleans given values, Go's
    number syntax (`0x10`, `1_000`), `--`, and flags after arguments except in `run`.
  - cobra's order and the CLI's words: a flag mistake exits 125 with the usage; then
    `--help`; then a wrong count of arguments, exit 1; then flags shards does not serve,
    exit 1. Deprecated flags print pflag's notice on stdout, where cobra prints it.
  - `--help` is the CLI's template, its options wrapped to the width of the terminal on
    stdin, or 80, as pflag wraps them.
  - Every flag `docker` takes parses. Those shards does not serve yet are left out of
    `--help`, as the CLI leaves out what its daemon cannot do, and refuse any value but
    their default: `"--publish" is not supported by shards yet`.
  - `-e NAME` takes the client's value, as the CLI's `ValidateEnv` does. `--init` is
    served as given: shards-init is every command's PID 1, forwarding signals and
    reaping as docker-init does.
  - **Evidence:** a differential test. The real docker/cli command tree answers 110
    command lines (scripts/docker-cli/oracle_test.go), and shards gives the same stdout,
    stderr and status byte for byte (crates/cmdline/tests/docker_cli.rs), except where it
    refuses a flag it does not serve, which the test checks the CLI ran.
  - Escapes and widths come from the CLI's own code, dumped by scripts/docker-cli/generate:
    Go 1.26.1's `strconv.IsPrint`, go-runewidth v0.0.29 for the tables it aligns, and
    golang.org/x/text/width (Unicode 15.0.0) for the commands it cuts short. The locale's
    East Asian widths travel from client to daemon.
  - **Cost:** about 20 µs of a pooled run, outside the guest, most of it the larger
    client's launch [PM M29]. `docker` flags shards does not serve are one string rather
    than a table, whose page of pointers every parse would fault in.
  - **Known gap:** go-runewidth counts a grapheme cluster as at most two columns; shards
    counts its runes, which differs only for clusters of several visible runes (emoji
    sequences, flags, Hangul jamo) in `ps`'s COMMAND column.
- **Records** (`crates/shards/src/containers.rs`). Each has a 64-hex-digit ID from 32
  random bytes, a name, the image and command, and its life: created, running once the
  guest reports its command started, exited with its status once it ends.
  - An ID is drawn again while its first 12 digits are all decimal: they name the
    command's host (`hostname`, and `HOSTNAME` in its environment) unless `--hostname`
    does (moby daemon/internal/stringid, daemon/container.go).
  - The daemon writes each to `containers/ID/config.json` in the home when it starts and
    when it ends, so exited containers outlive the daemon. A daemon starting finds them
    as dockerd restores its own (moby daemon/daemon.go): one left running is exited
    with 255, the status nobody saw, and a `--rm` one goes.
  - Names are unique. A name must match `[a-zA-Z0-9][a-zA-Z0-9_.-]+`, and a taken one
    is refused with dockerd's message (moby daemon/names.go, daemon/errors.go).
  - A run without `--name` gets one as dockerd makes them by default: an adjective and a
    surname; from the second try a digit follows; after six collisions, the short ID
    (moby internal/namesgenerator/legacy, daemon/names.go). The word lists are shards'
    own.
  - A run's request path writes nothing: it takes a spare container, an ID with its
    directory and open log, made after the last handover.
- **Output.** Every run writes its output to the container's log, as records of stream,
  time and bytes, in the order they arrived (`workload.rs`); `--rm` takes the log with
  the container. The VM reaches no container's directory: the request brings its log's
  first segment open, and the daemon makes each next one as the VM asks
  (`kind::LOG_SEGMENT`, `segments.rs`). Before, every warm VM could write the whole
  `containers` directory, every container's log and record, since a pooled VM cannot know
  its container ahead; and on macOS its grant cost each warm VM's start about 4.5 ms
  [PM M73].
- **The guest says when the command started** (`STARTED`), before any output, so a
  container is running only once its command is.
- **Detached runs** (`-d`). The ID goes to stdout once the container exists, before its
  command starts, as `docker run -d` prints it; the client exits 0 once the command has
  started. The run's output goes only to its log, and it reads nothing.
- **Commands that cannot start.** shards-init says why in the words of runc's Go
  (`exec.LookPath`, `os.PathError`, Go's errno texts): `exec: "/x": stat /x: no such
  file or directory`. The client prints `shards: Error response from daemon: …` and the
  hint, exiting 125, 126 or 127 by the CLI's `toStatusError`; the container stays created
  with the exit code dockerd gives it, 126, 127 or 128, from the message dockerd amends
  (moby daemon/errors.go, setExitCodeFromError). Nothing goes to its log, as nothing
  reaches `docker logs`. A detached client gets the same, after the ID.
- **A run's start has one owner** (`daemon.rs`, `RunState`; audit A06). The container
  and its pending run appear together, under the records' lock, and the run then moves
  through three states the commands and the daemon's shutdown consult:
  - *Pending*: no warm VM committed to yet. `rm` removes the container and cancels the
    run, as dockerd removes a created container; the run's warm VM goes back to its pool
    unused, and its client hears `No such container: ID`, as `docker run` hears it of a
    container removed before its start (exit 125).
  - *Handing*: committed to a warm VM, which has the request or is getting it. `rm`,
    `stop`, `kill` and `wait` wait to learn whether it started, then act on what
    happened, as dockerd's removal waits for a start under way (moby daemon/start.go
    holds the container's lock throughout).
  - *Tracked*: taken, and followed until it ends, registered before the client stops
    counting as busy, so shutdown always sees one or the other.
  - `stop` and `kill` of a container still starting act once it runs: `shards run -d`
    prints the ID before the start, where `docker run -d` prints it after, so a script
    that stops what `run -d` printed stops a running command in both.
  - A daemon told to stop commits no pending run: `ending` is set under the runs' lock,
    so a run either committed before and is signalled once it runs, or sees it and is
    refused.
  - A warm VM says TAKEN before it touches the client's stdio or starts anything, and if
    the daemon cannot hear it, it runs nothing. So one that ends without a word surely
    never started the run, which goes to another VM; one that answers otherwise, or not
    in time, may have, and is ended and followed like any run, never retried: no run
    starts twice.
  - A record changed after it went is an error, not nothing to do.
  - **Evidence:** deterministic unit tests that play the warm VMs over socket pairs and
    hold each step (acquisition, TAKEN, STARTED, DONE) while `rm`, `rm -f`, `wait`,
    `stop`, `kill` and daemon stop act, fourteen mutations of the fix each caught; and
    the audit's real-VM reproduction, a `shards-vm` held at a gate, which the old
    daemon fails (the command ran, exit 7, after its container was removed or the
    daemon stopped) and this one passes (`containers::rm_cancels_a_run_whose_vm_is_not_ready`,
    `a_stopping_daemon_starts_no_pending_run`).
- **Commands** (`crates/shards/src/daemon/commands.rs`), answering with dockerd's and the
  CLI's words:
  - A container is named by its ID, its name, or the start of its ID and of no other's
    (moby daemon/container.go).
  - `ps`: the CLI's table, byte for byte: tabwriter widths, go-units durations,
    commands cut to 20 columns and quoted as Go quotes them, images by their familiar
    name without digest; `-a`, `-q`, `--no-trunc`, `-n`, `-l` as the CLI and dockerd
    combine them.
  - `wait`: one container at a time, its exit code once it stops: 0 for one that never
    started, dockerd's code for one that could not.
  - `logs`: stdout to stdout and stderr to stderr, in the order they arrived; a line
    of any length arrives whole, in messages of at most 1 MiB, and output that does not
    all reach the client fails the command (audit A08); `-f`
    while the container runs; `-t` with RFC 3339 times; `--tail` (not a number: all);
    `--details` adds the space before the attributes shards' lines do not have.
    `--since` and `--until` are read as the Docker client reads them, on the client's
    clock and in its zone: Go durations, Go's time layouts with their error texts, Unix
    timestamps (`shards_cmdline::gotime`, matched against the client's own code); the
    daemon then reads the timestamps as dockerd does, and filters the tail as dockerd's
    log forwarder does.
  - **Logs are read in bounded memory, and followed without a timer** (audit A12;
    `daemon/logs.rs`, spec.rs `LOG_STDOUT`, workload.rs `Logger`).
    - Each record's start goes into an index beside the log once the record is whole,
      with its stream and whether it ends a line. Readers find records by the index, so
      a record cut short, or bytes a guest wrote to look like one, are never taken for
      records. A log from an earlier shards is indexed as it is first read.
    - A record is read in 64 KiB pieces, and a line held until it ends or reaches 16
      KiB, moby's copier's buffer; then it goes out in pieces, its prefix before the
      first, so the client's bytes are the whole line's.
    - `--tail` reads the index back from the end, and only the records of the lines it
      shows: on a 2 GiB log, 3 ms and 8 MiB of daemon, against 1.6 s and 8.2 GiB (PM
      M48).
    - `-f` waits on the log (kqueue on macOS, inotify on Linux), the run's end and
      the client, not a timer: a hundred idle followers cost the daemon nothing, where
      each looked 50 times a second.
    - **A log keeps only its newest output**, as Docker's `local` driver keeps it:
      segments of `SHARDS_LOG_MAX_SIZE` bytes (20 MiB), at most `SHARDS_LOG_MAX_FILE`
      of them (5), the oldest removed as the next begins (docs.docker.com
      engine/logging/drivers/local: `max-size` 20m, `max-file` 5). The daemon checks
      them before it serves and sends them with each run; the run's VM writes the
      segments in the container's directory, which it is given open.
      - Segments are numbered, `log`, `log.1`, …, and never renamed, so a reader never
        pairs one segment's log with another's index. A segment is there once its index
        is: made after its log, removed before it. A later segment there says one is
        whole, and a reader asks that before it counts the records.
      - A reader reads on in a segment removed under it, and past those removed before
        it reached them; lines in segments gone are gone, as rotated lines are from
        Docker's. `--tail` reads back through only the segments its lines are in. `-f`
        watches the directory for segments coming, and the segment it reads for
        appends.
    - A record the log cannot keep, on a full disk or a log removed, costs the log, not
      the run: it is taken back from both files and counted; the run's VM tells the
      daemon, the container keeps the count, and `logs` says so and exits 1 rather than
      show the log as all the output. A log that is gone is said, not shown as nothing.
  - `stop`, `kill` and `rm` act on up to 50 containers at once. Each success prints its
    argument once it and those before it are done, and the errors follow (docker/cli
    parallelOperation).
  - `stop`: the signal (SIGTERM), then after `-t` seconds (10; negative: never)
    SIGKILL, 10 s more, the VM itself, and 2 s more (moby daemon/stop.go, kill.go). A
    stopped container stops again without complaint, and a signal is checked only for a
    running one.
  - `kill`: SIGKILL, waiting for the end as `stop` does after its signal; any other
    signal is only sent. Signals are named as moby/sys/signal names them, real-time ones
    included, and every error names its container.
  - `rm` trims `/` from its arguments, refuses a running container unless `-f`, which
    kills it, says nothing of a missing one with `-f`, and removes one container at a
    time; one still starting as above.
  - Whoever waits for a container is told its exit code as its record changes, under
    the records' lock, as moby's `State.Wait` is, so `wait` never reads a record before
    its end is written.
- **`run -t` and `-it`** as the Docker CLI runs them (docs/research/
  tty-and-interactive-runs.md §2.1):
  - `-it` with a stdin that is not a terminal is refused before anything is created:
    `cannot attach stdin to a TTY-enabled container because stdin is not a terminal`,
    exit 1. `--detach-keys` is checked next, in moby/term's syntax, with its words.
  - Under `-it` the client's terminal goes raw with moby/term's flags, which Apple's
    `cfmakeraw` does not set, unless `NORAW` is set. It goes back as the client ends
    with the command's status, a start that failed, a detach, or a signal it forwards
    and would end by; as with Docker, not after SIGKILL.
  - The detach keys (ctrl-p ctrl-q, or `--detach-keys`) end the client with status 0
    and nothing said; the container runs on. As moby/term's proxy does, a byte that may
    start them waits for the next.
  - The pty is as big as the client's stdout, 0×0 if that is no terminal. Each SIGWINCH
    resizes it, then goes to the command too, as both reach a Docker container. XNU
    drops a SIGWINCH at its default even for `sigwait`, so the client gives it a
    handler that never runs [PM M31].
  - ^C under `-it` is a byte the guest's pty turns into SIGINT for its foreground group.
    shards' command is never PID 1, so it ends, as under `docker run --init -it`.
  - **Not yet:** `docker attach`, and Docker's `detachKeys` in its config file.
- **Known gaps** (flags parsed and refused): `ps --format`, `--filter` and `--size`, and
  `run`'s ports, volumes, networks, limits, restart policies and `--sig-proxy=false`.
  Like Docker's, every run has loopback up and its own `/etc/hostname`, `/etc/hosts`
  (Docker's lines, and the run's name on 127.0.1.1 until the VM has an address of its
  own) and `/etc/mtab` (a link to `/proc/mounts`), in place of what the image has there,
  as ordinary files a workload may change (PM M81). `/etc/resolv.conf` comes with
  networking.
- **Tests:** E2E with booted VMs, so they need no snapshots: a container outlives its
  run until `rm`, and `wait` reports its status; names are unique, and `--rm` leaves
  nothing; `stop` ends a command by SIGTERM (143) and `kill -s USR1` reaches it; `rm`
  refuses a running container and `rm -f` kills it (137); `ps` and `logs`; `-d` prints
  the ID and runs on, named for its ID; a command that cannot start is reported as
  `docker run` reports it, attached and detached; usage mistakes start no daemon.
- **Durability** (`containers.rs`, `Registry`; audit A15). What a daemon crash may cut
  short, the next start reconciles exactly; what a power loss may lose is bounded:
  - **A container is seen once its record is written.** It is reserved first, its name
    held, and its record is written on a recorder thread beside the run's start, which
    never waits for it. Writing it on the start path cost nothing at the median but put
    the filesystem's tail there: on a busy host a file's create or rename waits
    milliseconds at p90 behind other processes' flushes (PM M46), and a run's p90 and
    p99 doubled. What happens to a reserved container waits in the reservation, and the
    recorder writes again until what it wrote is current. `run -d` prints the ID once
    the container is seen.
  - **What happened to a run stands.** Its start and end are kept at once, and written
    after; a record that cannot be written is behind, logged, told to a detached client
    as a warning, and written again before any command is answered.
  - **A removal changes nothing until the container's directory is set aside**
    (`.ID.removing`), so one that fails leaves the container seen, and removable again.
    It is then synced before its name is let go and `rm` answers: an answered `rm` never
    comes back, and no power loss brings back a container beside one that took its
    name. The sync, 4.3 ms at the median on macOS (PM M46), is out of the registry's
    lock.
  - **Records are written and renamed, not synced.** Syncing one costs 8.5 ms at the
    median on macOS (PM M46), 2.5 times a pooled run. A power loss ends every run
    anyway; what it leaves of their records, the next start reconciles: a record missing
    (a spare, or a container never seen: its directory goes), older (a running one exited
    with 255, a pending one keeps 255 as a run that did not start), torn (its next
    version is taken if a power loss kept that whole, else it is left and logged, as
    dockerd leaves a container it cannot load), a removal cut short (finished), or a
    write cut short (removed).
  - **Tests:** a fault-injecting disk fails each write, rename and sync, and cuts a
    container's life short at every step, and the next start must reconcile each; a
    model of a filesystem losing power, after ALICE's abstract persistence model (Pillai
    et al., OSDI 2014) at its weakest, enumerates every state a power loss may leave at
    every step of a life and of the next container to take its name; the daemon holds a
    removal's name until its sync returns; an E2E kills the daemon while a container is
    pending and finds it after. Eighteen mutations of the fix, each caught.
- **Tests of terminals** (E2E, crates/shards/tests/tty.rs): under `-t` the command's
  stdio is a terminal that leads its session, with `\r\n` in its output and its log;
  the refusal and bad detach keys create nothing. The client runs on a pty the test
  plays: the size arrives, the terminal is raw while the command runs and as it was
  after, typed input is echoed and read, a resize reaches the command, ^C interrupts
  it (130), and both detach keys leave it running.

### Shipping the guest (D28)

`shards run IMAGE` works on first use: with no guest recorded and none named, a run boots
shards' pinned kernel and the shards-init that `shardsd` carries. The evidence is in
docs/research/shipping-the-guest.md; the code is `crates/shards/build.rs`,
`src/kernel.rs` and `src/guest.rs`.

- **shards-init is inside `shardsd`.** `shards-abi` has no version field, so a host and
  an init from different builds could misread each other's frames. Every runtime surveyed
  keeps its host and guest halves together by shipping them in one release, and none
  negotiates versions [shipping-the-guest.md §3.1]. libkrun embeds its init the same way,
  with `include_bytes!` [libkrun v1.19.6 `src/init_blob`].
  - The package's build script builds shards-init for `<arch>-unknown-linux-musl` with
    the `guest` profile, through a nested cargo with a target directory of its own. It
    names `rust-lld` as the linker itself: `cargo install` from outside the checkout
    reads neither the toolchain file nor `.cargo/config.toml` [PM M36].
  - A missing musl standard library fails the build with the `rustup target add` that
    fixes it. `SHARDS_INIT_BINARY` names a prebuilt init instead.
  - The binary is copied into `OUT_DIR` for `include_bytes!`. No environment variable
    names it: cargo sets a build script's `rustc-env` for the package's tests and
    `cargo run` too, where `SHARDS_INIT` means the user's init.
  - Only `shardsd` holds it, 428,912 bytes on aarch64 (7.7% of it) [PM M36]. The client
    stays thin [PM M23], and VM processes lean [PM M34].
  - The daemon writes it into the store under its SHA-256 the first time a run needs it.
    Boots take it from there, as they take a recorded init.
- **The kernel is pinned, and fetched once.** `src/kernel.rs` pins the release asset for
  the host's architecture by URL, size and SHA-256. The tests pin theirs there too.
  - Embedding it would make `shardsd` 3.4 to 5 times larger: 18.9–27.7 MB against
    5.5 MB [PM M36]. Apple's `container`, Lima and Colima also compile a download's
    digest into their binaries and fetch on first need [shipping-the-guest.md §2.2, §2.4].
  - The first run that needs it downloads it through the registry client's HTTP and TLS
    (D19, D20), hashing as it arrives. Only a file of the pinned size and SHA-256 is
    renamed into the store. Anything else is refused before anything boots, and nothing
    of it is kept. Lima and Apple's `container` also verify before the rename
    [shipping-the-guest.md §3.2].
  - `SHARDS_KERNEL_URL` fetches it from elsewhere, a mirror or a machine off the
    Internet. The digest is still the pin.
  - Once stored it is trusted by its name, as Lima, Colima and Podman trust theirs
    [shipping-the-guest.md §3.2]. Restores read no kernel at all (D25).
- **The store comes before the network.** A run on the default guest checks the store
  first and fetches only what is missing, so runs after the first need no network. A
  daemon stores one file at a time, and removes temporary files that a daemon which
  ended left.
- **`shards guest use` overrides it.** A recorded guest wins over the default, and
  `shards guest` says which is in use. Its record is written once the files it names are
  durable, and is durable itself once `guest use` answers (audit A15); the default guest
  names nothing, and a file of it a power loss took is fetched again.
- **An init says which contract it speaks.** `shards-abi`'s build script hashes the
  crate's sources into `IDENTITY`. shards-init writes it to the control page as it starts
  (`control::ABI`), and a snapshot keeps it. The host hands a workload only to a guest
  whose init wrote the host's own. Otherwise the run fails with 125 before the command is
  sent, as `docker run` fails a command that never ran.
  - Any change to what the two share gives another identity, so an init from another
    build is refused rather than misread. gVisor checks its helpers' release label the
    same way [shipping-the-guest.md §2.9].
  - The check costs a run nothing: init writes the identity once, before a template is
    saved, and the host reads it from the VMM's own state.
- **Upgrades.** An upgrade that changes the init's bytes changes its digest, and so the
  templates it names (D25). A template never restores against an init it was not made
  with.
- **Not yet:**
  - a benchmark of the first run, which is mostly the kernel's download
    [shipping-the-guest.md E1];
  - a compressed kernel asset, which would move about a third of the bytes (E7);
  - fetching the kernel while the image is pulled;
  - signed release binaries (§3.8).
- **Tests** (E2E, crates/shards/tests/guest.rs, a real VM):
  - A first run fetches the kernel from a loopback server behind a redirect, stores it
    and the embedded init, and boots. The next run fetches nothing. Every E2E test boots
    the init `shardsd` carries, from build.rs's output.
  - A kernel with one byte changed, one byte more or less, or a 404 is refused before
    the image is pulled, and nothing is kept.
  - A shards-init built with another identity boots, but gets no workload: the run
    exits 125, saying why.

### Guest memory (D29)

Guest RAM is shared: the guest's vCPUs write it as they run, the host kernel reads and
writes it in system calls given guest addresses, and the VMM's threads (vCPU threads
handling exits, device workers, the snapshot coordinator) read and write it for the
devices. The code is `crates/vmm/src/memory.rs`.

- **One host thread at a time.** Rust makes a race between two of its threads undefined
  unless both accesses are atomic, and racing atomic accesses must not partially overlap;
  a volatile access counts as non-atomic [std::sync::atomic, "Memory model for atomic
  accesses"; std::ptr::read_volatile]. The guest decides where its devices' rings and
  buffers lie, and can lay one device's on another's. So the VMM's threads reach guest
  memory only through an `Access`, which one of them holds at a time: their accesses are
  ordered, whatever the guest overlaps (audit A01).
  - A thread that asks for one while it holds one gets an error, not a deadlock.
  - Devices hold one for a step of queue work (a pop, a request's header, a completion)
    and never across a system call, so no device waits on another's I/O.
- **Within one, volatile and atomic.** The guest and the kernel change guest memory under
  the host's reads, as I/O memory changes, so reads and writes are volatile, a word at a
  time where aligned. The virtqueue indices that order the host against the guest are
  atomic: acquire loads of `avail.idx`, release stores of `used.idx`.
- **No references into guest memory.** Nothing forms a Rust reference or slice into it.
  Bulk data moves by system calls given guest addresses: block I/O, vsock payloads,
  kernel loading and snapshot writes. The kernel, like the guest, is outside Rust's
  abstract machine.
- **Saves and restores agree.** Regions are saved and mapped in guest-address order,
  whatever order the caller lists them in (audit A21). A save empties its file first, so
  a zero page keeps nothing the file held (A22). A memory file too short for the guest is
  refused, not mapped past its end, where the guest's first access would raise SIGBUS.
- **Checked by the tools that know the rules.** ThreadSanitizer and Miri run the tests
  where host threads share guest memory, among them two queues whose rings lie on each
  other's; with a guard that excludes nothing, both report the data races. CI runs them
  [PM M44].
- **What it costs.** Nothing measurable in runs: the paired medians of interleaved
  pooled runs moved by 10 µs or less, within their intervals. The word-at-a-time zero
  check made a 256 MiB save three to five times faster [PM M44].
- **Not yet:** a save still reads every untouched page, and so makes it resident (audit
  D01); snapshots of a machine whose every CPU and device has stopped (A02).
- **Tests:** `memory::tests` (copies at every alignment, threads taking turns, a nested
  access refused, ranges in any order, a reused file) and
  `queue::tests::queues_laid_over_each_other_work_on_two_threads`, under
  `access-guard/check.sh` too; every E2E test runs its devices through the guard.

### Confining the VM process (D30)

shards-vm runs the device code a guest drives, parses snapshots it may have been given,
and holds the user's permissions. A flaw in it would reach what the user can, so it
narrows itself to what a VM process does (docs/research/rootless-security.md R3; the
audit's "Security and test coverage").

- **Linux: a seccomp filter over the whole process, first thing in `main`**
  (`confine.rs`, `platform::seccomp`), before it reads its arguments' files.
  - Its lists are what a VM process was seen to make over every VM test on a KVM host
    [PM M52]: 70 syscalls; ioctl requests only the KVM backend's own (`hv::IOCTLS`),
    `FIONBIO`, and the terminal's `TCGETS`, `TCSETS` and `TIOCGWINSZ`; only Unix
    sockets; `prctl` only to name threads.
  - It makes threads and nothing else: `clone3`, whose flags a filter cannot read, fails
    with ENOSYS, as Firecracker's filters have it, so the C library falls back to
    `clone`, which must carry a thread's flags (glibc's or musl's). No exec, no fork,
    no network.
  - Compiled to classic BPF: the architecture first, then each syscall's block behind
    a jump, arguments compared in their low 32 bits (the ones filtered are `int`s to
    the kernel, and musl passes ioctl's request sign-extended). A refused syscall
    traps; a SIGSYS handler names it and the thread, and the process exits 159.
  - Every VM test runs under it on CI's KVM runners, glibc and musl, with VMs required.
  - One filter for every thread: the lists are the union. Per-thread filters, as
    Firecracker keeps, would narrow a vCPU thread to its KVM ioctls: the next step
    (rootless-security.md R3).
  - `tkill(2)`, obsolete beside `tgkill(2)` [man: tkill(2)] and musl's `pthread_kill`, is
    refused: a vCPU's kick is `tgkill` to its own thread, and `tgkill` is allowed only
    within the process. `prctl` names threads and sets no_new_privs, nothing else.
- **Linux: Landlock, failing closed** (`confine.rs`, `landlock`), applied just before the
  VM starts (rootless-security.md R3; [Documentation/userspace-api/landlock.rst]).
  - Every filesystem right Landlock's ABI v5 knows is handled, TCP bind and connect are
    refused, and on ABI v6 signals and abstract Unix sockets outside the process are too.
    Allowed: its kernel, initrd, init, pmem and disks, a restore's snapshot and the files it
    records, the snapshot directory it saves, sockets made
    only beside a vsock path it was given (`--vsock`), `/dev/null`, its own `/proc` entry, and `/dev/kvm` and the
    huge page settings where the host has them. No directory may take a file from another
    (`REFER`). Rules hold inodes: a template keeps its rule when the daemon renames it.
  - It fails closed: a kernel without Landlock, or whose ABI is older than v5 (Linux 6.10,
    the first to govern device ioctls), starts no VM, as does a path it must allow that is
    not there. rootless-security.md R7 forbids weakening a policy silently; Landlock's
    maintainer asks that the version be read from the kernel, never from its release
    (firecracker-microvm/firecracker#5771).
  - Still to do, per R3: a user and mount namespace with `pivot_root` into an empty
    tmpfs, `CLONE_INTO_CGROUP`, rlimits; UDP (ABI v10) and pathname Unix socket (ABI v9)
    rules where the kernel has them; and the jail's cost on the start path (R4 Q1).
- **macOS: App Sandbox, replacing the Seatbelt profile** (docs/research/
  macos-confinement.md, [PM M67]). The profile was applied with `sandbox_init`, which the
  SDK marks deprecated and no longer supported [sandbox.h:7,45]; shards uses no deprecated
  or unsupported interface, so it goes, in the change that brings App Sandbox.
  - `shards-vm` is signed with App Sandbox, the hypervisor entitlement and Hardened Runtime,
    with its identity in an Info.plist linked into it. Before it opens anything it asks its
    spawner, on a socket of its own (`--grants`), for what its arguments name, and reaches
    nothing else: a file it reads as a descriptor opened read-only, since a bookmark passed
    between processes grants read and write or nothing [PM M70]; a file it writes as a
    descriptor opened read-write; the directory a template it saves goes in by a
    read-write bookmark, made first; a restore's template file
    by file, never its directory. Only for a `--vsock PATH` of the user's, the spawner
    binds the device's socket for it to listen on, and dials each host port its guest
    connects to, handing the connection over.
  - It fails closed: a VM process not in App Sandbox, or given nothing to ask, starts no
    VM. Its spawner keeps each descriptor it sends until the VM's next message, as XNU
    flushes a socket in flight that no process holds [PM M24].
  - The daemon answers its own VMs, on the thread that watches each. `shards vm`, which
    becomes the VM by exec, starts a broker, `shardsd grants`, that answers and exits. A
    broker per VM would cost each warm VM's start 3.9 ms, 95% [3.8, 4.1], all of it a
    directory's bookmark made in a fresh process, where the daemon makes one in 0.4 ms;
    a broker is up before its VM asks [PM M71].
  - It costs about 3.1 ms at launch (M67), before a warm VM's request, as the profile's
    compiling cost 3.7 ms (M53).
- **The run's own vsock ports are no socket files.** The run and signal ports are served
  by the VM process itself, so its device hands a guest's connection to one straight to
  the serving thread as one end of a `socketpair(2)` (`VsockHost::ports`). Before, the
  process bound `<path>_<port>` in a private directory and its own device dialled it:
  - any process that could reach a `--vsock` directory could dial the run's port first
    and be sent the command, its environment and stdin; a pair has no name to dial;
  - neither Landlock nor App Sandbox has anything to grant for a warm VM or `shards run`,
    whose VMs now make no socket file at all; App Sandbox refuses Unix sockets outside
    its container even in a granted directory (macos-confinement.md §2);
  - a `socketpair` is allowed by the seccomp filter already (Unix sockets only);
  - a pooled run is 66 µs faster at the median, 95% [53, 81], none of it in the guest
    [PM M68].
  A path is only for the user's `--vsock PATH`, Firecracker's interface, which host
  clients dial and whose `<path>_<port>` sockets other ports reach.
- **Windows:** nothing yet.

### Networking: a network process per VM (D31, design)

A VM process stays airgapped: its seccomp filter allows Unix sockets alone, its Landlock
rules refuse TCP, and on macOS its sandbox reaches no socket outside what it is handed
(D30). A VM with a network gets it from a second process of its own, the network process,
which holds the VM's only way out. rootless-security.md R4.16 ("run … the user-mode network
stack in separate sandboxed processes") and R6 decide this; networking.md R1, which put the
stack inside the VMM, is superseded by it.

- **Why a process, not a thread of the VMM.**
  - A flaw in the network stack, which parses whatever the guest and the Internet send,
    reaches only that process: not the guest's memory, not the VM's disks, not /dev/kvm.
    User-mode network stacks have a CVE history of their own [rootless-security.md R4.16:
    CVE-2019-6778, CVE-2021-3546, CVE-2025-2509].
  - The VM process keeps confinement that TCP would break, and the network process keeps
    confinement that a VM would break: no filesystem at all, sockets only where the policy
    says.
- **One network process per VM, rootless, on both OSes.** Per VM, so that one VM's stack,
  compromised or overloaded, reaches no other VM, and a clone gets its own NAT identity
  (networking.md R3, [firecracker network-for-clones.md]). No TAP, no host namespace, no
  vmnet: the only design with no root and no restricted entitlement on macOS [Apple: vmnet,
  com.apple.vm.networking] that creates no host-namespace churn [Oakes18 §2.2; Thomas20
  §2.3].
- **What it does.** It terminates the guest's L2 and speaks L4 to the host, as passt does
  [man: passt(1)], written in-repo because passt does not run on Darwin [passt: about]:
  - Egress: each guest flow becomes a host socket, opened only after the policy allows its
    5-tuple, after reassembly (RFC 1858, RFC 3128) — the one enforcement point a compromised
    guest kernel cannot pass (networking.md R7).
  - DNS: a resolver answering the guest, which forwards upstream and enforces name rules with
    the protections networking.md §2.6 lists (TTL capped, CNAME chains, TCP, rebinding to RFC
    6890 ranges refused, DoT/DoQ refused, SVCB hints stripped).
  - Published ports (`EXPOSE … ingress`, `-p`): host listeners in the network process,
    routed to the guest (networking.md R5); privileged ports as the host OS allows them.
  - No host loopback by default (rootless-security.md R4.16; passt's default mapping is the
    anti-pattern [man: passt(1)]).
- **Its confinement (D30's rules, its own lists).** Linux: seccomp allowing its sockets and
  no filesystem calls it does not need; Landlock handling every filesystem right and
  allowing none, and TCP bind and connect only on the ports the policy names (Landlock ABI
  v4 network rules) — the IP part of a policy is its own code's. macOS: App Sandbox with the
  network client and server entitlements and no file access (docs/research/
  macos-confinement.md).
- **The guest's side.** A virtio-net device in the VM process, configured before the
  template is saved, with a static address and no DHCP or duplicate-address detection, so
  restores do no network work (networking.md R3; [RFC 2131 §4.4.1; RFC 4862 §5.4]). A VM
  with no network gets no device (networking.md R7: absent device, no attack surface).
- **Built so far.** Builds' `RUN` steps and `shards run` are on Docker's default bridge:
  the guest is 172.17.0.2/16 behind 172.17.0.1, with a random, locally administered MAC
  as Docker gives a container, which a template keeps and its restores reuse; the daemon
  starts each VM's network process beside it and hands the VM its side of the ring.
  - *Default deny (AGENTFILE_ARCH.md §3).* A run's network process denies every flow the
    guest opens: the guest is on the bridge, and reaches nothing through it until a
    grant opens it. Builds' `RUN` steps keep BuildKit's access, as their parity needs.
  - *Published ports (`-p`, `-P`).* The daemon binds each host port as the run starts, as
    dockerd's port allocator does (moby docker-v29.3.1 portallocator/osallocator_linux.go:
    `SO_REUSEADDR`, `IPV6_V6ONLY`, one port at every address, 10 tries for a picked one),
    and hands the listening sockets to the VM's network process over a control socket,
    before the VM has the run. That process accepts each connection and opens it to the
    guest from the gateway, as dockerd's userland proxy's comes, apart from the policy,
    which governs only what the guest opens. A UDP port gives each host peer a flow of
    its own, from a gateway port of its own, and answers it from the host address it
    asked (`IP_PKTINFO`, as dockerd's proxy keeps it; ipi_spec_dst on send, Linux
    ip_cmsg_send and XNU udp_check_pktinfo); its flows end after the stack's UDP idle
    time. SCTP is refused: the stack does not carry it. A taken port fails the start in
    dockerd's words, its allocator's for a container's port, bindTCPOrUDP's for another
    program's.
  - *A run's ports are free when its end is told.* The network process says it has the
    sockets, and the daemon's copies close (M24 holds them until then); as the run ends,
    the VM has the network process close them, and waits for it to say so, before it tells
    the daemon and the client (`kind::UNPUBLISH`), so that `run --rm -p N …` followed by
    any bind of N succeeds, as Docker's does. Only runs that publish pay that round trip.
- **Open, measured before it is built** (networking.md §4):
  - *The data path between the two processes* (E2), **decided (PM M83):** a ring of frame
    slots in memory the two processes share, not the datagram socket Apple's model uses
    [VZFileHandleNetworkDeviceAttachment.h:13-49], whose sends macOS refuses with ENOBUFS
    while poll calls it writable, nor a vhost-user backend, which would give the network
    process the guest's memory. The VM process copies between the virtqueues and the
    ring; the ring moved 131 to 370 Gbit/s and a round trip in 0.8 µs, where datagrams
    moved 11 to 75 Gbit/s in 22 µs.
  - *Its cost per VM* (E2, D14): RSS idle and with 1k and 10k connections, beside a VM's
    own (M64), and the start: the network process is spawned and paired with a warm VM
    before its request, and must cost a restore nothing (E1).
  - *Enforcement overhead* (E5) at 0, 100 and 10k rules, and the DNS conformance suite (E6).
- **Not now:** a VM-to-VM switch (networking.md R8; VMs reach each other through published
  ports first), an administrator-provisioned TAP tier (R2), GPU networking (R9).

### Other engines: shards' runtime alone runs its microVMs (D32)

Docker, Compose and Kubernetes do not run shards microVMs, as a containerd shim, Docker
runtime or RuntimeClass. The user asked for that only if every guarantee held fully
(AGENTFILE_ARCH.md Q15), and three fail by the engines' design:

- **Networking.** The engine makes the VM's network namespace and veth after the runtime
  has started it, and `-p` is DNAT in the host's namespace that never reaches the runtime
  [moby 0fed273: daemon/start_linux.go:17-41; daemon/libnetwork/drivers/bridge/
  port_mapping_linux.go:26]. `EXPOSE ... FOR`, `NETWORK` and `CONNECT` cannot hold there,
  nor D31's network process.
- **Confinement on macOS.** Docker Desktop's engine runs in its own Linux VM, where App
  Sandbox (D30) does not exist.
- **Rootless.** dockerd and containerd run as root; Kata ships rootless only for QEMU, off
  by default.

Docker's own microVMs, Docker Sandboxes, are not a Docker runtime either. shards stays an
OCI citizen where that costs nothing: registries, the Agentfile's BuildKit frontend, and
Compose files that shards reads. Evidence: docs/research/oci-engines.md.

### Building images: `shards build` (D33)

`shards build` is a drop-in for `docker build`: the same Dockerfiles accepted, the same
image config and history, the same steps cached. RUN steps run in microVMs, not
containers. Evidence: docs/research/image-build.md (§3 ranks the choices below).

- **The frontend is BuildKit's, reimplemented and held to its code** (`crates/dockerfile`).
  It parses, lexes, types the instructions and plans the build as dockerfile/1.27.1's
  `Dockerfile2LLB` does. That covers stages, ONBUILD, ARG and ENV scoping, every step's
  command, environment, directory, user and mounts, file operations, the image config's
  bytes and history, progress names, and every build check.
  - Evidence: `scripts/dockerfile/generate` runs BuildKit's own packages over a corpus.
    `tests/oracle.rs` expects the same bytes: 75 plans op by op, 146 image configs, 63
    URLs, the parser's and the lexer's test tables. Deliberate differences are listed in
    `testdata/deviations.json` and the modules' documentation. Examples: no Go map order,
    Linux targets only, and no network I/O while planning.
- **Only RUN runs in a VM**, one per step, over its parent state read-only with a fresh
  upper (image-build §3.3). BuildKit runs COPY, ADD, WORKDIR's mkdir and the export in its
  own process (§2.1), and shards does them in `shardsd`. File operations apply to an
  in-memory tree, never the host's filesystem: case-insensitive APFS, owners, devices and
  xattrs rule that out (§3.6).
- **Layers are written as BuildKit writes them**, containerd's `ChangeWriter`: explicit
  whiteouts, ancestors with their merged metadata, `security.capability` alone among
  xattrs, PAX only when needed (§2.9). The exporter's config patching and history
  normalization follow `exporter/containerimage/writer.go`.
- **Order of work** (§3.13):
  1. The frontend and its oracle. Done.
  2. Metadata-only builds and host file operations, written to the store so `shards run`
     boots them. buildx's command line, held to buildx's own command tree as D27's
     commands are held to docker/cli's.
     - Done: buildx's `build` command line (36 answers of buildx v0.37.1's own tree), the
       exporter (512 configs and manifests of BuildKit's own functions), and builds of
       base images and settings, end to end. An image `shards build` wrote has the config
       Docker Desktop's BuildKit wrote for the same Dockerfile, byte for byte (the same
       digest, checked 2026-10-01), and `shards run` boots it (`tests/build.rs`).
     - Done: file operations and their layers (`crates/build`). BuildKit's mkdir, mkfile
       and copy actions (fsutil's copy, its chmod strings, `--chown` names from the
       image's /etc/passwd and /etc/group) run on an in-memory tree that changes as Linux
       changes a file system, and record what overlayfs's upper directory would hold;
       the layer is BuildKit's overlay differ over it, through containerd's
       ChangeWriter and a port of Go's tar writer. `scripts/build/generate` runs
       BuildKit's own backend on a real overlayfs mount as root and its differ over the
       result: 91 cases, 79 layers byte for byte and 12 errors word for word
       (`tests/oracle.rs`; each of 16 mutations of these semantics fails it).
     - Done: the build context. `.dockerignore` is read as ignorefile.ReadAll reads it
       (18 answers) and planned as `local.excludepatterns`; the directory is walked as
       buildx's client walks it, include, exclude and follow paths applied (36 cases of
       fsutil's own walk on this host), and written as BuildKit's receiver writes it
       (five COPYs from contexts sent and received by fsutil itself, in the overlayfs
       oracle).
     - Done: the executor. `shards build` runs WORKDIR, COPY (with `--chown`, `--chmod`,
       `--link`, heredocs, `.dockerignore`) and merges as FileOpSolver runs them: a mount
       per action chain, committed to a layer when it is an output or read twice; a
       merge stacks its inputs' layers. `tests/build.rs` boots such an image in a VM and
       finds every file, mode and owner as built. Built against Docker Desktop's
       BuildKit (v0.28.1, 2026-10-01), the same Dockerfile and context gave the same
       config and the same layers, byte for byte, but for the wall-clock mtimes
       BuildKit's own layers carry (WORKDIR's directory, a parent a COPY touched).
     - Where shards does better than BuildKit, on purpose:
       - The context is never sent: shardsd runs as the user beside the files, so
         BuildKit's session transfer has nothing to do, and progress says what was read
         ("read 5 files, 166B"), not a transfer's byte count. What the transfer gives
         BuildKit, one version of each file that no edit during the build can change,
         shards keeps another way: each file is taken into a stage private to the build,
         by a copy-on-write clone where the file system makes one (APFS, Btrfs, XFS) and
         by a copy elsewhere, from a descriptor opened without following symlinks. A file
         whose identity, size, mtime or ctime moved while it was taken is refused, as GNU
         tar reports "file changed as we read it". `tests/context.rs` holds this under a
         writer that never pauses: 1,000 snapshots on APFS, none torn; copies taken
         without the check tear on the first. A stage a crashed build leaves is
         collected with the store's other leftovers.
       - Layers are stored uncompressed (`application/vnd.oci.image.layer.v1.tar`), so
         no build or run spends time compressing or decompressing them; the diff ID is the
         digest. A push compresses.
       - Refs record what a reference resolved to, so a stored image reports the index
         digest Docker reports, as a fresh pull does.
       - The root filesystem is written from the target's last snapshot, not stacked
         again from its layers (`crates/build/src/stack.rs`, PM M80). The layers lose
         what the snapshot holds: an entry the differ writes keeps whole seconds of its
         mtime (containerd's ChangeWriter truncates; layer::meta zeroes times before
         1970 or past Go's range) and `security.capability` alone of its xattrs; what no
         layer writes keeps its last entry's attributes, or the base image's, such as a
         directory whose mtime a step changes but the differ leaves (continuity's
         sameDirent ignores it), a file judged the same by size and mtime; the root is
         layer::root()'s; sockets are left out. Each commit records which nodes its
         layer wrote and what the layers still hold of the nodes it left; the export
         puts the snapshot in that form in place, and writes it reading files where the
         build has them, under `Store::rootfs`'s lock, limits and path. Where it cannot
         follow the layers exactly (a hard link only some of whose names a layer
         writes, content a layer leaves as the lower's, a name layer::apply takes for a
         whiteout, a merge onto a snapshot not in its layers' form) it stacks the layers
         as before. `tests/stack.rs` holds both to `Store::rootfs` byte for byte, over
         every oracle case and multi-step cases, and the E2E builds compare each image
         with the store's own stacking.
     - Done: ADD's local archives, as BuildKit unpacks them (moby/go-archive's
       DecompressStream and chrootarchive.Untar): every entry resolved inside the
       destination as a chroot resolves it, so `../` names and absolute symlinks stay in
       it. 17 more oracle cases, among them each compression, an escape attempt, implied
       parents, replaced paths and out-of-range times, unpack byte for byte as BuildKit
       does; `tests/build.rs` boots one image of every compression. Where moby runs `xz`
       and `unpigz` and fails without them, shards decodes gzip, bzip2, xz and zstd
       in-process, in pure Rust (PM M77).
     - Stress-tested 2026-10-01: an archive 2,000 directories deep unpacks (1.5 s,
       46 MB), one of a million entries at 915 MB of memory, since brought to 164 MB
       (PM M78, M80). A 20 GB gzip bomb exposed two faults, now fixed: it was
       not unpacked at all, as an archive whose first file is past the bytes read to
       detect it was taken for no archive (the tar reader read on into that file's data;
       it now skips data when the next entry is asked for, as Go's does); and nothing
       bounded what ADD may unpack. A build's ADDs now hold to the limits a pull holds to
       (`SHARDS_MAX_IMAGE_BYTES`, `_ENTRIES`, `_METADATA`, `SHARDS_KEEP_FREE`), when set,
       stopping at the step that passes them, with nothing left in the store.
     - Copy-up as overlayfs makes it: a step's change to a file of the snapshot below
       (its mode, owner, times, xattrs, content, a rename, a new link to it) is made to
       a copy, so its other names keep the file as it was. overlayfs without its index
       breaks a hard link so, and containerd mounts with `index=off`; Docker Desktop's
       BuildKit gave `/a` mode 600 with one link and `/b` mode 644 after `RUN echo shared
       > /a && ln /a /b` then `RUN chmod 600 /a`, and a layer of `a` alone (2026-10-01).
       No file operation reaches this today (COPY and ADD replace their targets), but RUN
       will, and its guest mounts its overlay with `index=off` too.
     - Real Dockerfiles (2026-10-02, arm64 macOS): every official image's Dockerfile
       that needs no RUN (`scripts/build/realworld/corpus.txt`: five alpine releases,
       hello-world, nats on scratch) builds with layers, config and history equal to
       Docker Desktop's BuildKit entry by entry (path, type, mode, owner, size, link,
       xattrs, SHA-256, mtime), exports from the snapshot, and runs alike
       (`scripts/build/realworld/compare.py`). In each alpine VM every file of the 512 to
       518 has the hash, mode, owner and mtime it has in Docker's container; what differs
       is Moby's init layer (`/.dockerenv`, `/etc/{hosts,hostname,resolv.conf}`,
       `/etc/mtab` to `/proc/mounts`, daemon/initlayer/setup_unix.go) and directories'
       sizes, which are their filesystem's (ext4's 4096, EROFS's own). nats-server serves
       as under Docker with `--network none`, its log line for line: guest networking
       (D31) is not built yet. The first of these builds found every step from scratch
       given up to the layers, now fixed and held by the staged-build test.
     - Open: untagged images. A build without `-t`, and the image a moved tag named,
       stay in the store, as Docker keeps dangling images until they are pruned; shards
       has no `images`, `rmi` or `image prune` yet to show and remove them.
     - Next: ADD's URLs and git sources, then RUN (step 3).
  3. RUN in a booted VM without a network, and its layer.
  4. Cache keys, then RUN's network, mounts and builder templates.

## 3. Components

```
shards (host CLI, docker-compatible) ──unix socket──▶ shardsd (daemon)
                                                     │  image store · build · networks
                                                     │  volumes · templates/snapshots
                                                     │  warm VMM pool · policy
                                                     ▼
                                       shards-vm (one process per microVM)
                                       hv backend (HVF | KVM) · memory · boot/FDT
                                       vCPU threads · GIC · virtio devices · snapshot
                                                     │ virtio (blk/net/vsock/console/rng/pmem/fs/gpu)
                                                     ▼
                                       guest: Linux (tuned) · shards-init (PID 1)
                                       shards-engine (containerd's place; Docker Engine API)
                                       agents and compose services, many per microVM
```

- **VMM** (the `shards-vmm` library, run by the `shards-vm` binary): one process per
  microVM. That is forced on macOS [GT §1.1] and chosen on Linux for fault isolation, as
  Firecracker does. The binary links the VMM and what a VM process runs, and nothing of
  the daemon's: every process relocates its whole binary as it starts, and the daemon's
  registry, TLS and image code cost each VM about 1 MiB [PM M34]. The hot
  path is kept free of allocation and locks; device threads communicate with vCPU
  threads through lock-free rings.
  - A running machine is owned: waiting for it or dropping it stops it, joins its vCPU
    and snapshot threads (each vCPU is destroyed on its own thread), stops its device
    workers, destroys the VM, and only then lets go of the devices and guest memory the
    VM mapped (audit A04; `tests/lifecycle.rs`).
- **Daemon** (`shardsd`): serves a Docker-compatible API with extensions for VM
  specs and isolation policy. It owns the warm pool and the template snapshots, and
  carries the shards-init its guests run (D28).
- **Guest**:
  - a tuned Linux kernel built from source inside a shards builder VM
  - `shards-init`, a minimal static PID 1 that sets up and then drops privilege
  - `shards-engine`, our own runtime, rootless. It walks and talks like containerd and
    Docker (Engine API, Compose) but runs no containers underneath. **pending**: its
    design, informed by the engine-internals and rootless research.

### RUN steps: one builder microVM per build (D34)

A Dockerfile's `RUN` runs as BuildKit runs it (docs/research/buildkit-run.md), in a
microVM rather than a container on the build host's kernel.

- **One builder per build, not a container per step.** The first `RUN` boots a builder:
  shards-init with `shards_build=1`, every base image a `RUN` stands on as a virtio-pmem
  device (the store's own EROFS root filesystem, nothing copied), and the build port
  reaching the build process through the vsock muxer. Its steps then cost no boot and no
  root filesystem written: BuildKit prepares a snapshot and starts a runc container for
  every step, about 176 ms each on this host (Docker Desktop, 20 `RUN`s 3.85 s, one 0.48
  s, 2026-10-02); a whole cold shards VM is 39 ms (`vm run` of alpine `true`, n = 22).
- **Layers by id, never twice.** The guest holds layers, directories overlayfs stacks,
  written once from the host's change streams (`shards_abi::changes`): a host step's own
  changes when a `RUN` first stands on them (`shards_build::sync`), and each `RUN`'s upper
  layer, which stays where the step left it. Each step names its trees as layers over a
  base, so the guest keeps no tree of its own and stages share their layers. The stream
  is not tar: it never leaves shards, and keeps nanoseconds, every xattr and overlayfs's
  markers, so the guest's trees are the host's snapshots exactly; the published layer is
  written apart, as BuildKit writes it (`crates/build` diff).
- **A step is BuildKit's step.** Mount, PID, UTS, IPC, network and cgroup namespaces of
  its own (its command PID 1), runc's `/dev`, proc, read-only sysfs and cgroups, masked
  paths, `/etc/hosts` and `resolv.conf` bound read-only and in no layer, its mounts,
  `pivot_root` (the builder first moves off its initramfs so that a step with
  `CAP_SYS_CHROOT` cannot reach the builder's files), hostname `buildkitsandbox`,
  loopback up, BuildKit's user resolution from the snapshot's own files (`shards-user`),
  runc's environment and umask, BuildKit's capabilities, stubs removed. Its upper
  directory comes back and is put into the snapshot (`shards_build::upper`), whose layer
  the differ writes as BuildKit's (the `RUN` E2E test, and every `RUN` of a real
  Dockerfile compared entry by entry, `scripts/build/realworld`).
- **Better than the reference where it can be.** Every step is a VM's, not the build
  host's: `--security=insecure` grants privilege inside the VM alone. A guest's change
  stream is exact, so nothing diffs the lower tree.
- **Not yet as BuildKit:** a step's network is its own loopback until the guest has a
  network (D31); BuildKit's seccomp profile; caches kept past one build; secrets and ssh
  from the client; memory plugged as a build needs it (PM M82: a builder pays about 21
  MiB and 4 ms per GiB of guest memory, and takes half the host's, as Docker Desktop's VM
  has). Each is an item of AGENTFILE_ARCH.md §11.

## 4. Start path (≤ 5 ms budget)

| Step | Cost | Evidence |
|---|---|---|
| Daemon receives request; resolves template; claims warm VMM | ~10–50 µs of IPC (to be measured) | — |
| `hv_vm_create` + `hv_gic_create` + sequential vCPU creation | ~11 + 2 + 7·n µs; can be done ahead in the warm process | [PM M2] |
| `mmap` snapshot memory `MAP_PRIVATE` + `hv_vm_map` | ~1–10 µs | [PM M3] |
| GIC restore (`hv_gic_set_state`; its registers alone, 20–30 µs, lose interrupts in flight) | ~1.2–1.3 ms; paid by a warm VM before the request | [PM M14, M45] |
| vCPU register restore | ~0.5 µs per vCPU | [PM M14] |
| Resume; guest faults in its working set | ~64 µs per MiB (1 vCPU), ~2.2× less with prefetch vCPUs; a warm VM prefetches it before the request | [PM M5, M6, M30] |
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
   - Built: our kernel (CI releases); virtio-pmem; the EROFS writer; layers → one EROFS
     image (D15); booting into an image to run a command (D16); the image store (D18);
     registry TLS, HTTP and auth (D19–D21); pulls (D22); `shards pull` (D23); `shards run
     IMAGE` (D24); terminals (D16, D27); the kernel and shards-init shipped with shards
     (D28).
4. **In-VM engine.**
   - Scope: Docker Engine API subset → full; the rootless runtime (compatible, not
     containers underneath); networks, volumes, build; compose.
   - Proven by running the official Docker CLI/Compose conformance suites against
     it. Pending: compat, engine, rootless research.
5. **Host CLI/daemon parity and networking.**
   - Scope: userspace network stack, port publishing, per-VM and per-container
     isolation policy. Pending: networking research.
6. **KVM backend.**
   - Scope: x86_64 first, since hosted CI runners expose `/dev/kvm` (booting since
     4dd2799). Then arm64, tested inside an EL2-enabled shards VM on this host.
     virtio-pci plus VFIO.
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
