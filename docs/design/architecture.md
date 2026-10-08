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

Every target builds and passes the lints. Until a target's backend lands, `run --kernel` there
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

`shards run --rootfs IMAGE -- COMMAND` boots into an image and runs a command there as
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
- **Warm runs.** `run --kernel --rootfs IMAGE --snapshot-dir DIR` saves a template: shards-init
  asks for the snapshot once the image is mounted and the kernel's crypto self-tests have
  finished (at most 2 s), before it dials the host. Each
  `restore DIR -- COMMAND` resumes a copy that dials in for its own command (D2, D14).
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
    command runs, waits for it (D26). `run --kernel`, whose process is the VM, ends as that
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
- **Not yet:** a terminal for `run --kernel --rootfs`, whose commands are shards' tests and
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
  - **gzip** streams may have many members (flate2, inflating with zlib-rs). flate2
    refuses reserved header flags, as RFC 1952 §2.3.1.2 requires. Go's reader ignores
    them.
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
  - Its layers decompress and hash on the host's cores at once, and stack in order as
    each and those before it are done: a third of the time for golang:1.26 (PM M109).
    containerd and BuildKit apply them one after another. An error is the one a
    sequential unpack would meet first.
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

- **Hosts** are reached as containerd's defaults reach them
  (`core/remotes/docker/config/hosts.go`): Docker Hub at `registry-1.docker.io`, all
  others over https, but a loopback host on port 80 over plain HTTP, and on any port
  but 80 and 443 over https first, then plain HTTP from the first handshake answered
  in plain HTTP or timed out (`NewHTTPFallback`, `isTLSError`) [PM M112].
  - Unlike containerd, a loopback host's certificate is verified, against `certs.d` and
    the system's roots: containerd skips the check there.
- **Requests** retry as `doWithRetries` does, at most 5 times:
  - a timeout or cut connection is tried again after 50 ms;
  - a 401 is answered (D21), then the request is sent again;
  - a manifest HEAD refused with 405 becomes a GET;
  - 408 is tried again, and a 500, 503 or 504 once.
  - A 429 is tried again, as containerd tries it, but after a wait: `Retry-After`
    when it fits in the waits left, else up to 50, 100, 200, 400 and 800 ms, a random
    share of each. containerd asks again at once, and ECR Public, which throttles
    anonymous requests at random even at 1 a second, failed 12 of 20 pulls of shards
    that never retried and none that waits [PM M112]. A quota spent
    (`ratelimit-remaining` at 0: Docker Hub counts pulls over hours) is never retried,
    and the error reports its `ratelimit-*` fields and `Retry-After`.
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
  - `shards restore DIR --warm FD` is one, where FD is its socket to the daemon.
    `shards run … --rootfs R [--snapshot-dir D] --warm FD` boots one instead, saving a
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
    - It holds as many clients at once as the threads it may have hold (review 7.9),
      each a thread, the watcher of a VM started for its run, and up to six
      descriptors; past that, connections wait in the listener's backlog. The threads
      are the system's to say: `kern.num_taskthreads` on macOS (16,384 here, so 8,187
      clients past the daemon's own eight threads and the dispatch worker a framework
      it calls keeps [PM M98]), and on Linux the least of
      `RLIMIT_NPROC`, `threads-max` and the `pids.max` of its control groups, up to the
      root its namespace sees, where a container's limit is (Linux
      Documentation/admin-guide/cgroup-v2.rst). Where none is said, POSIX's
      `_POSIX_THREAD_THREADS_MAX` (64). `SHARDS_MAX_CLIENTS` sets fewer. The 256
      before was chosen, not derived.
    - A client that waits long, on a container's end (`wait`) or its output
      (`logs -f`), counts among those in hand no more, so that waiters shut out no
      client however many there are. A thread that cannot be spawned still drops its
      client.
    - Out of descriptors, it waits for room rather than spin on a listener that stays
      readable, and accepts again only once a descriptor is free: an accept that fails
      for want of one leaves the client queued on Linux (net/socket.c,
      `__sys_accept4_file`) but drops it on macOS ("Don't put this back on the socket
      like we used to, that just causes the client to spin. Drop the socket.",
      xnu-11417.101.15 bsd/kern/uipc_syscalls.c). On macOS it looks for a free
      descriptor before every accept, a `dup` and a `close`, 250 ns [PM M99], and drops
      no client; looking only once starved, it dropped two, the one whose accept found
      none and, once room came, the one after the first it took, which had taken the
      last.
    - It raises its soft limit on descriptors to its hard one, capped at
      `kern.maxfilesperproc` on macOS, as Go's runtime raises its own (go1.25.0
      src/syscall/rlimit.go, after go.dev/issue/46279): macOS starts a process with 256.
  - It keeps up to SHARDS_POOL warm VMs (default 2) of each template it has served, as
    many as its runs need (below). A pool refills once its VM has taken its run, since
    starting the next VM on the request's path cost 200–600 µs [PM M26]: on the refiller's
    thread, which takes every pool asked for meanwhile, each once. A run with no template
    boots a VM that saves one on the way, and a run with its own kernel and init boots
    every time. A template whose warm VMs fail three times in a row is removed and saved
    again.
  - **VMs start outside the pools' lock** (review 7.8). What a pool needs is planned under
    it and counted as starting at once, so that no other claim or refill starts it again,
    then started without it: a start spawns the VM and its network process. A claim that
    finds no VM ready starts its own on its own thread. Started under the lock, they held
    every other claim, refill and the listener's look at the pools for up to 1.3 ms (p99
    0.7 to 0.9 ms); now under 0.2 ms. A burst's runs took as long either way, its restores
    outweighing its spawns [PM M91].
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
      pull`, has moved a reference (it leaves `images/collect-due`, whose coming the
      daemon's watch of `images` sees).
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
      - It runs on a thread of its own, the collector's (review 7.14): on the thread that
        accepts clients, every client waited in the backlog for as long as it took, 4.5 s
        for 20,000 files left in `ingest/`, against 3.9 ms at most now [PM M94]. It waits
        for the store's lease, the kernel waking it once the last is let go (flock(2)).
        None starts while the listener is out of descriptors, which its files would take
        from clients macOS then drops, nor once the daemon is stopping.
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
    each run has sent (`settle`), under a lock per run that the followers' loop (below)
    reads under too, so it answers with all any client has seen: `ps` lists a container whose
    output has appeared, and not a `--rm` one whose `run` has returned, as dockerd
    records a container's state before `docker run` learns it (docker/cli run.go
    `waitExitOrRemoved`). Waiting for the daemon to acknowledge each instead cost a run
    241 µs at the median (95% [206, 293], `build-ab/ab.py`, n = 400); this costs none
    measurable (+16 µs, 95% [−30, +73]). The daemon can signal a command meanwhile, on
    the same socket.
  - **It waits on events, not a clock** (review 7.16, 7.22). Each run had a thread
    following it and another for its health checks, check or none, each waking every
    250 ms, as the listener and every `wait` and `logs -f` did: an idle running
    container cost the daemon two threads, 0.18 MiB and about 40 µs of CPU a second, and
    threads, 16,384 a process on macOS, would stop runs near 8,000, where their VMs take
    60 GB of a 128 GB host [PM M89].
    - One thread follows every run: a readiness poller over all their VMs' sockets
      (`platform::Poller`: kqueue(2) on macOS, epoll(7) on Linux, level-triggered) takes
      each run's messages as they come in whole. A socket the poller cannot take has its
      run followed on a thread of its own. It waits on no write: a run's end is kept in
      memory, in its reservation if its record is not yet written, its record written by
      the recorder and a `--rm` container's removal set aside by the completer. Waiting
      there for the record, or for the removal's rename, held up every other run's
      messages by 81 ms at a loaded host's p90 [PM M96]. Nor does it make the files a
      run's VM asks for: a working set is gathered there, in memory, within the bound
      read where the VM was started, and the set written, and each log segment made, on
      the files' thread (`daemon/files.rs`). On the loop, a segment held them 10.9 ms at
      a loaded host's p90 [PM M98].
    - The same thread follows every VM process to its end, and its network process to its
      own (`Poller::add_exit`: kqueue's `EVFILT_PROC` on macOS, a pidfd on Linux 5.3 and
      later), reaping each as it ends: a network process still running its grace after
      its VM, a second, is ended, and the VM's ports are freed once both have gone. A VM's
      watcher thread lives until its VM is ready, where it waited out the VM's life, the
      one thread an idle run still cost [PM M90]: now the daemon's threads stay 6 at 10
      running containers and at 100, and an idle run costs it about 10 KiB [PM M92];
      8 since the collector's and the files' threads [PM M98]. macOS watches only ends
      to come, and
      refuses a child that has ended already (`ESRCH`, measured), which is then reaped at
      once; a process whose end cannot be watched is waited for on a thread of its own.
    - One thread schedules the health checks of the containers that have one, from a
      heap of their due times, and runs each probe on a thread while it runs; dockerd
      keeps a goroutine per container and one per probe (moby 0fed273 daemon/health.go
      `monitor`).
    - One thread makes `--rm` containers' removals durable: one sync of their directory
      serves every removal pending, so runs that end together share it. A run named as
      one still being removed waits for the name.
    - One thread makes the files runs' VMs ask for, in the order asked: log segments, and
      working sets, each synced into its template.
    - The listener sleeps until a client arrives or leaves, a run ends, a warm VM comes
      ready, a name comes or goes in the home or in `images` (kqueue's `EVFILT_VNODE`,
      inotify(7)), or its next duty is due: the idle exit once nothing runs, or a pool's
      keep-alive. It looks every 250 ms only out of descriptors, or where a watch cannot
      be made.
    - A `wait` sleeps on a socket its run's end writes to and on its client's
      connection together, as a `logs -f` sleeps on that and on its log.
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
  - `stop`, `kill` and `rm -f` end every container they name at once, from one loop over
    the containers' end sockets, each escalating on its own clock (signal, SIGKILL at
    its grace, its VM 10 s on). docker/cli sends 50 requests at a time
    (cli/command/container/utils.go, parallelOperation), so stopping 60 containers that
    ignore SIGTERM takes two graces; here one, with no thread a container
    (`stop_ends_every_container_at_once`). Answers come as the CLI's do: in the order
    asked, each once it and those before it are done.
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
    - Reading on, it takes the index 8,192 entries at a time and the log 64 KiB at a
      time, each record whose head and output lie in what is read taken from it, and
      sends a stream's lines in messages of up to 64 KiB, sent as one fills, as the
      stream changes, and once what is there is read: each record read alone and each
      line sent alone cost a log of a million short lines 1.7 s of the daemon's CPU,
      against 0.2 s, and its `logs` 1.70 s, against 0.29 s (review 7.10, PM M95).
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
    after, by the recorder's thread, outside the registry's lock (review 7.7): written
    under it, a record held every other run's container and every command for as long
    as the filesystem took, up to 0.37 s on a busy host, against microseconds now
    [PM M93]. Records are written in the order their changes came, a container changed
    again before its record is written once, as it then stands. A command is answered
    once every record changed before it is written, a detached run's client hears of
    its start once it is recorded, and the daemon exits once its records are written; a
    record that cannot be written is behind, logged, told to a detached client as a
    warning, and written again before any command is answered.
  - **A removal takes the container out of sight at once, and sets its directory aside
    after** (`.ID.removing`), outside the registry's lock: set aside under it, a removal
    held up every run's end and every command for as long as the rename took, 34 ms at a
    loaded host's p99 [PM M96]. Until it is set aside, a crash would bring back a
    container `rm` removed, so commands wait for it, as `rm` does to answer; a `--rm`
    container's removal makes no one wait, since the next start removes every one that
    no longer runs. One that cannot be set aside is put back as it was, its record
    written again, and `rm` says why. It is then synced before its name is let go and
    `rm` answers: an answered `rm` never comes back, and no power loss brings back a
    container beside one that took its name. The sync, 4.3 ms at the median on macOS
    (PM M46), is out of the registry's lock.
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

- **Inspect** (`daemon/inspect_doc.rs`, `daemon/inspect.rs`). `inspect`, `container
  inspect` and `image inspect` show dockerd's InspectResponse as Go types
  (shards_template::Struct, with typed nils), so that `--format` reads them as
  docker/cli's inspector does (typed first, then the raw JSON with `missingkey=error`),
  and the JSON is dockerd's field for field: a microVM's Config is its request merged
  with its image's config as dockerd merges them, and its HostConfig dockerd's defaults
  for what was not asked. Held to Docker Engine 29.3.1 by testdata/inspect.json
  (`scripts/inspect/generate`, 18 command lines). What is shards' own, each true of a
  microVM where Docker's would not be: Runtime `shards`; no host paths for the guest's
  resolv.conf, hostname and hosts; no libnetwork sandbox or endpoint IDs; the guest's
  address on the bridge, the same in every guest; the VM process as State.Pid. Docker
  Desktop's engine sets Config.StopTimeout 1 where none was asked; dockerd and shards
  leave it unset. `--size`, networks' and volumes' documents are still to come.
- **Filters** (`daemon/ps.rs`, `daemon/images.rs`, `daemon/filters.rs`). `ps`, `images`
  and the prunes take `--filter` as dockerd does at docker-v29.8.1 (moby list.go,
  containerd image_list.go and image_prune.go, prune.go), names and statuses as regular
  expressions (RE2, as Go's), times by dockerd's own reading (`gotime::parse_timestamp`,
  held to moby's code by the docker-time oracle), and image names as 29.8.1 matches them:
  familiar or whole, with or without the tag (29.3.1 matched the familiar alone).
  `commit` records the image a commit was made from, as dockerd's
  `org.mobyproject.image.parent` label, for `ancestor` and for hiding an image's dangling
  parents. Two differences, each to keep what dockerd risks: each filter's expressions
  are compiled once for a list, where dockerd compiles them for each container; and
  `image prune` never deletes the last name of an image a microVM was made from, which
  dockerd does when that microVM named it by a name since given to another (a stopped
  microVM starts again from its image, D37). Filters that need what shards does not have
  yet (volumes, networks) match no microVM. Tests:
  `ps_filters_microvms_as_dockerd_filters_containers`,
  `images_and_prunes_filter_as_dockerd_does`.
- **Labels, names and resolution** (`cli/request.rs`, `daemon.rs` `name_guest`, init
  `set_hostname`). `run` and `create` take `--label`/`-l`, `--label-file`,
  `--env-file`, `--expose`, `--add-host`, `--dns`, `--dns-option` (and the hidden
  `--dns-opt`), `--dns-search` and `--domainname` as docker/cli v29.8.1 validates them
  (opts.ValidateLabel, ValidateIPAddress, ValidateDNSSearch, ValidateExtraHost, held by
  the docker-cli oracle) and reads their files (kvfile.Parse: a bare name in an env file
  takes the client's value or is dropped, and in a label file is dropped). The daemon
  writes them into the guest as dockerd's sandbox does: resolv.conf from `--dns*`
  (moby resolvconf, an override skipping the legacy rewrite), /etc/hosts with each
  `--add-host` and then the guest's own line `IP\tname.domain name` (makeHostsRecs), and
  the NIS domain name (setdomainname(2)). `host-gateway` is the bridge's gateway, the
  address at which the guest reaches its host. Labels are the image's with the run's
  over them (moby merge), and reach `ps --filter label=`, `{{.Label}}`, the prunes'
  `label`/`label!` and inspect. The request carries the new fields in an extension
  section after the daemon identity, so a request of an older client still decodes.
  Test: `run_labels_names_and_resolves_as_docker_run_does`.
- **CID files, quiet pulls, platforms and signals** (`cli/request.rs` `before_create`,
  `CidFile`; `run.rs` `prepare`). `run` and `create` take `--cidfile`, `-q`,
  `--platform` (its default DOCKER_DEFAULT_PLATFORM) and, `run` alone, `--sig-proxy`,
  as docker/cli v29.8.1 does (create.go createContainer, cidFile, run.go toStatusError):
  the CID file is refused if it exists, made before the request, written with the ID the
  daemon sends as the microVM is made (`CREATED`'s payload), and removed if none came;
  `run`'s errors exit 125, or 127 and 126 as their words say. `--platform` is read as
  containerd's `platforms.Parse` reads it and goes to the daemon, which looks for and
  pulls that platform's image, so `linux/386` on an amd64 host runs the 386 image. One
  difference: a platform this host's microVMs cannot run is refused before anything is
  pulled. Docker pulls it and then fails as it starts, or runs it under emulation, which
  a microVM, running its own kernel on the host's CPU, has none of. Test:
  `run_writes_cidfiles_pulls_quietly_and_keeps_signals_as_docker_run_does`.
- **Memory and CPU limits** (`resources.rs`, init `cgroups`, `isolate`, `limit`).
  `run` and `create` take `-m`, `--memory-reservation`, `--memory-swap`,
  `--memory-swappiness`, `--oom-kill-disable`, `--cpus`, `--cpu-period`, `--cpu-quota`,
  `-c`, `--cpuset-cpus`, `--cpuset-mems` and `--pids-limit` as docker/cli v29.8.1 reads
  them (MemBytes through go-units' RAMInBytes, NanoCPUs through math/big's Rat, each held
  to the real CLI by the docker-cli oracle), and dockerd checks them as on a cgroup v2
  host (verifyPlatformContainerResources: its errors, and its warnings for swappiness and
  OomKillDisable, which v2 has not). A container's limits are a cgroup's, inside the
  microVM as on a Linux host: the guest kernel has cgroup v2 with every controller runc
  uses, init mounts it with `nsdelegate`, and each workload process joins one cgroup, in
  a cgroup namespace rooted there and a mount namespace (a slave of init's) where
  `/sys/fs/cgroup` is that cgroup, read-only, as a Docker container sees its own; the
  standby the template keeps does so before the snapshot, so a run pays nothing for it.
  init writes the limits as runc v1.5.1 does (fs2 Manager.Set, after moby's and runc's
  translations: `--cpus` as `cpu.max` over 100 ms, shares to a weight, swap less memory).
  The microVM is sized to hold them: as many vCPUs as `--cpus`, the quota or the cpuset
  needs, and memory whose MemAvailable holds `-m`, from the guest kernel's own share
  measured by size (PM M117). Where Docker limits a container below the host, shards does
  both: the limit binds inside a VM no larger than it needs. A process the guest kernel
  kills for want of memory is reported as dockerd reports containerd's OOM:
  State.OOMKilled until the next start, and an `oom` event. Two differences:
  `--cpuset-mems` is checked against the guest's memory nodes, where dockerd checks it
  against the host's CPUs (its cgroup2 sysinfo parses `Cpus` into `MemSets`) and runc then
  fails at the start; and a limit above the host's memory gives a VM of the host's
  memory, past which it limits nothing. `shards create`'s refusals are dockerd's words
  alone, exit 1, as runCreate returns them. Test:
  `run_limits_resources_as_docker_run_does`.
- **Mounts, rlimits and sysctls** (`setup.rs`; init `setup.rs`, `isolate`). `run` and
  `create` take `--read-only`, `--tmpfs`, `--shm-size`, `--ulimit` and `--sysctl` as
  docker/cli v29.8.1 reads them (ValidateSysctl, go-units' ParseUlimit, MapOpts), and
  dockerd checks the tmpfs destinations as it makes the container and merges their
  options with its defaults (`noexec,nosuid,nodev,rprivate`, MergeTmpfsOptions) as it
  starts it. In the guest, the standby sets them up in the workload's mount namespace
  as runc does: each tmpfs, the shallowest first, its options read as runc's
  parseMountOptions reads them; `/dev/shm` resized; the rlimits; and the root remounted
  read-only last. init writes the sysctls through a `/proc/sys` it opened before
  `/proc/sys` was made read-only, as runc writes through an unmasked procfs. An exec's
  standby joins the workload's cgroup and mount namespaces, as runc's exec enters a
  container's, and takes its rlimits, so an exec sees the same root, mounts and limits.
  Failures are runc's inner words (its `OCI runtime create failed` chain left out, as for
  every start failure), with Docker's exit codes; `cp` onto a read-only root is refused
  as checkWritablePath refuses it. Test:
  `run_sets_up_mounts_limits_and_sysctls_as_docker_run_does`. Not yet Docker's: the
  default rlimits, which are the guest kernel's (`nproc` 994, `memlock` 8 MiB) where
  dockerd's come from its own and containerd's.
- **Capabilities, groups, OOM score, privilege** (`setup.rs` `capabilities`; init
  `Inherited`, `setup::privileged`; `shards_user::additional_groups`). `run` and `create`
  take `--cap-add`, `--cap-drop`, `--group-add`, `--oom-score-adj` and `--privileged`, and
  `exec` `--privileged` and `--env-file`, as docker/cli v29.8.1 reads them. dockerd's
  rules: capabilities kept as NormalizeLegacyCapabilities keeps them and checked as the
  container is made, then tweaked as TweakCapabilities tweaks the defaults; groups added
  as moby's getUser adds them (moby/sys/user GetAdditionalGroups), after the user's own;
  the OOM score set on the process before it execs; a privileged container's every
  capability, no masked or read-only paths, `/sys` and its cgroup writable, every device
  the VM has made in `/dev` from `/sys/dev`, and `SecurityOpt` `label=disable`. An exec
  takes the workload's process (moby daemon/exec.go): its capabilities (all, with
  `--privileged`), groups, rlimits and OOM score. Checked side by side with Docker 29.3.1:
  the same CapEff, CapBnd, groups and OOM score. Test:
  `run_sets_capabilities_groups_and_privileges_as_docker_run_does`. Not yet Docker's: its
  default seccomp profile, which shards' guest does not apply (`--security-opt`, with it).
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
  - Compiled to classic BPF: the architecture first, then a binary search over the
    syscalls' numbers, and over an argument's values, as libseccomp's binary tree does
    [man: seccomp_attr_set(3), `SCMP_FLTATR_CTL_OPTIMIZE`], each comparison jumping
    straight to its target in a pool after the comparisons. KVM_RUN's ioctl runs 16
    instructions where comparing each rule in turn ran 179 (x86_64), which halves what
    the filter costs a vCPU's run. The install, which converts and JITs the program at
    about a quarter of a microsecond an instruction and runs every number through it for
    the kernel's cache of syscalls allowed whatever their arguments [kernel/seccomp.c,
    `bpf_prepare_filter`, `seccomp_cache_prepare_bitmap`], takes 110 µs at p50 where it
    took 294 (EPYC 7763; the program 224 instructions, not 343) [PM M107]. Only the
    instructions that cache follows (`seccomp_is_const_allow`). Arguments are
    compared in their low 32 bits (the ones filtered are `int`s to the kernel, and musl
    passes ioctl's request sign-extended). A refused syscall traps; a SIGSYS handler
    names it and the thread, and the process exits 159.
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
  - The daemon answers its own VMs, on the thread that watches each. `shards run --kernel`, which
    becomes the VM by exec, starts a broker, `shardsd grants`, that answers and exits. A
    broker per VM would cost each warm VM's start 3.9 ms, 95% [3.8, 4.1], all of it a
    directory's bookmark made in a fresh process, where the daemon makes one in 0.4 ms;
    a broker is up before its VM asks [PM M71].
  - It costs about 3.1 ms at launch (M67), before a warm VM's request, as the profile's
    compiling cost 3.7 ms (M53).
- **A template's saver gives it up before its run** (review 8.2). A VM that saves a
  template goes on to serve the run it was booted for, and every later run of the image
  restores what it saved: a guest that took its process over could otherwise rewrite
  them all. Once the template is committed, and before any run's command reaches the
  guest, the template is out of the VM's reach, or the VM takes no run, and the daemon
  gives the run to another, restored from the template.
  - Linux: a second Landlock layer, with the first's rules but the template's directory
    (layers only restrict further [Documentation/userspace-api/landlock.rst]), applied
    before the VM says it is ready, and checked: a file made there must be refused.
    Nothing of the template is open by then, and Landlock takes back nothing open.
  - macOS: a grant cannot be given up, but names a path [PM M102]: the daemon moves the
    template into place (`settle`) before it hands over the run, and the VM checks that it
    is gone from the path it was granted before it takes one.
  - A restore given its template's files (`--backing`) is confined to them before it
    reads the template's state, which a VM process wrote: what that names past them is
    refused, by a process that can reach nothing else.
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
- **Built so far.** Builds' `RUN` steps and `shards run` are on Docker's default bridge,
  on the subnet dockerd would make it on this host (moby docker-v29.9.0
  daemon_unix.go initBridgeDriver): the first of dockerd's default pools (libnetwork
  ipamutils: 172.17.0.0/16, 172.18 and 172.19, the /16s of 172.20/14, 172.24/14 and
  172.28/14, then 192.168.0.0/16 in /20s) that overlaps nothing the host seems to use
  (netutils.InferReservedNetworks: its resolvers, and its on-link IPv4 routes, by
  rtnetlink on Linux as vishvananda/netlink lists them, and by XNU's route dump on
  macOS, its routes without a gateway that were not cloned). A host on 172.17.0.0/16, as
  a CI job in a container on Docker's own bridge is, gets 172.18.0.0/16, as dockerd in
  it does; with every pool in use, runs on the bridge are refused in dockerd's words,
  which would not start, and runs on `none` go on. The daemon elects it as it starts and
  keeps it for its life, as dockerd keeps its bridge; a build elects it as it starts its
  builder (PM M101 for its cost). The guest is the subnet's second address behind its
  first, with a random, locally administered MAC as Docker gives a container, which a
  template keeps and its restores reuse; the subnet is on the guest's command line, by
  which templates are named, so a template is restored only on the subnet it was saved
  on. The daemon starts each VM's network process beside it, on that bridge, and hands
  the VM its side of the ring.
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
  - *Frames, and what the guest loses of them (PM M104).* The VM's device returns a
    received frame's buffers to the guest together (virtio 1.2 §5.1.6.4.1), and a
    segment's bytes go from a connection's queue to the ring in one copy. A guest short of
    memory drops segments, and closes its window on what was in flight (Linux
    `ICSK_ACK_NOMEM`); since ring and guest keep the order segments are sent in, the
    network process repairs a loss at the first duplicate acknowledgement past RFC 6582's
    `recover`, and each hole a partial acknowledgement shows; sends again, as the window
    opens, what a window that shrank had the guest drop; sends a timeout's segment alone
    (RFC 6298 §5.4); and probes a window closed on bytes waiting (RFC 9293 §3.8.6.1).
    Its loop costs what is ready, not what it holds: each descriptor registered once with
    epoll or kqueue, timers in a heap, the connections a full ring held back in a queue
    of their own; a round trip beside 3,500 idle connections costs what it does beside
    none (PM M106).
- **Open, measured before it is built** (networking.md §4):
  - *The data path between the two processes* (E2), **decided (PM M83):** a ring of frame
    slots in memory the two processes share, not the datagram socket Apple's model uses
    [VZFileHandleNetworkDeviceAttachment.h:13-49], whose sends macOS refuses with ENOBUFS
    while poll calls it writable, nor a vhost-user backend, which would give the network
    process the guest's memory. The VM process copies between the virtqueues and the
    ring; the ring moved 131 to 370 Gbit/s and a round trip in 0.8 µs, where datagrams
    moved 11 to 75 Gbit/s in 22 µs, with a receiver that spins before it sleeps. Neither
    side spins: each sleeps on its doorbell as soon as its ring is empty, which costs an
    idle VM nothing, and a round trip 6.6 µs at p50, 12 at p99, with 124 to 250 Gbit/s
    one way (PM M105).
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
  - Where BuildKit's limits come from its machinery, not from Dockerfiles, shards takes
    more: lines of any length in a Dockerfile or a .dockerignore (BuildKit's
    `bufio.Scanner` refuses one past 64 KiB), and either file of any size (BuildKit's
    frontend refuses one past 16 MiB, the largest message its gRPC takes from the client;
    shards reads them where they are). Everything BuildKit takes reads alike.
- **Only RUN runs in a VM**, one per step, over its parent state read-only with a fresh
  upper (image-build §3.3). BuildKit runs COPY, ADD, WORKDIR's mkdir and the export in its
  own process (§2.1), and shards does them in its daemon. File operations apply to an
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
  s, 2026-10-02); a whole cold shards VM is 39 ms (`run --kernel` of alpine `true`, n = 22).
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
  host's: `--security=insecure` grants privilege inside the VM alone, so `--allow
  security.insecure` (and `network.host`) grants it on the client's word, where BuildKit's
  daemon must also be set up to (`entitlements.WhiteList`, "not allowed by build daemon
  configuration"): the daemon's gate guards a host the builder VM does not share. A
  guest's change stream is exact, so nothing diffs the lower tree.
- **The build's secrets and limits** (`--secret`, `--ulimit`; shards_cmdline::buildflags,
  held to buildx's answers by crates/cmdline/tests/buildx.rs, and to BuildKit's plans by
  the corpus). Secrets are read once, as the build starts, so every step sees one value,
  where BuildKit reads one each time a step asks; held in the client's memory alone and
  overwritten when dropped; as large as a step's frame carries (1 MiB,
  `shards_abi::run::MAX_PAYLOAD`), where BuildKit's gRPC session caps them at 500 KiB; and
  mounted on a tmpfs of their own, in no layer. `--ulimit` takes `as` too, which go-units
  refuses for the way Docker starts a container; every RUN takes the limits, as
  Dockerfile2LLB's AddUlimit gives them, the run alone.
- **Not yet as BuildKit:** a step's network is its own loopback until the guest has a
  network (D31); BuildKit's seccomp profile; caches kept past one build; ssh from the
  client; memory plugged as a build needs it (PM M82: a builder pays about 21
  MiB and 4 ms per GiB of guest memory, and takes half the host's, as Docker Desktop's VM
  has). Each is an item of AGENTFILE_ARCH.md §11.

### One binary (D36)

shards is one file to install and run, `shards`: every command, the daemon included,
one CLI.

- **The command and the daemon are one executable.** `src/main.rs` is its root: the
  command line (`src/cli/`) answers what it reads itself and asks the daemon for the
  rest, and every other command, the daemon among them, runs in its process. They had
  been two binaries so that the command started fast: the daemon's binary took 3.5 ms to
  launch where the command's took 2.3 [PM M113]. Size cost 0.18 ms of that; loading
  frameworks the rest. So the one binary links none that loads at launch: Security and
  CoreFoundation are bound when first called (`crates/apple`, a verifier that trusts and
  refuses as rustls-platform-verifier did, held to it case by case), and so is
  Hypervisor.framework (`vmm` `hv::hvf::ffi`), which neither the command nor the daemon
  calls. One binary launches in 2.50 ms where the command alone did in 2.32.
- **The VM process and the network process stay processes of their own, carried inside
  it.** Every VM maps the binary it runs: one running all of `shards` would hold 1.2 MiB
  more than one running its own [PM M34]. And on macOS the VM process runs in App
  Sandbox, granted by entitlements that are its binary's code signature, which the
  command and the daemon must not share (D30); `sandbox_init`, the one way to enter a
  sandbox without them, is deprecated. So `crates/shards/build.rs` builds both (crates
  `vm-process`, `net-process`) with a nested cargo, signs the VM process with its
  entitlements on macOS, and embeds them with their digests; `src/helpers.rs` writes
  them out the first time a build needs them, into this user's cache under a directory
  named by their digests, whole (each synced, the directory renamed into place), and
  every later process of the build finds them with a `stat` each. They are no
  deliverables: nothing but `shards` is installed.
- A daemon's build is its binary's identity, which includes what it carries; one given
  another VM binary (`SHARDS_VM_BINARY`, for VMM work and tests) is another build.
- **Tests:** `helpers::tests` (written whole once, found after, a cut copy replaced);
  E2E runs the one binary placed alone, its VMs from what it carries; `apple::tests`,
  the verifier against rustls-platform-verifier over eleven chains.

### A container's files outlive its microVM (D37)

A Docker container keeps its writable layer when it stops: `docker start` runs its
command again over the files it left, and `diff`, `export`, `cp` and `commit` read them.
A microVM's writable layer is the upper directory of its root's overlay, in guest memory,
gone with the VM. So a container that stays (not `--rm`) keeps it on the host:

- **Saved as it stops.** After the command's status, the host asks init for the layer
  (`kind::SAVE`); init packs the upper directory as go-archive packs one
  (`WhiteoutFormat::Overlay`: whiteouts become `.wh.` entries, opaque directories
  `.wh..wh..opq`), without what init makes of every container (its mounts, `/etc`'s files),
  and sends it as `kind::LAYER` frames. The VM process writes it to a file the daemon
  opened in the container's directory (`layer.new`), and says so (`LAYER_SAVED`); the
  daemon keeps it as `layer.tar` only whole. That is an OCI image layer of the container's
  changes, which `commit` takes as it is.
- **After the end is told.** Saving before the run's end cost an empty layer 29 ms at p50
  and a 4 MiB one 155 ms; told first, the empty layer costs nothing measurable and the
  4 MiB one 6.5 ms at p50, from the save running beside the next run [PM M115]. Only what
  reads the layer waits for it (`start`, until the daemon has it settled).
- **Put back as it starts.** `shards start` sends the request the container was made by
  (kept as `request` beside its log), and the daemon hands the VM its layer
  (`RUN_LAYER_IN`), which init applies over the image's root before the command, as
  go-archive's ApplyLayer does; its log goes on from its newest segment. `restart` stops
  it first; `create` makes it, and starts nothing.
- **Visits** (`daemon/visit.rs`). `cp`, `diff` and `export` of a stopped container read
  its files as dockerd reads a stopped container's: in a VM booted over them, its image
  and its layer put back, running nothing of the image's (`builtin::HOLD`: init's standby,
  never given its orders, is the workload). The same built-ins answer as for a running
  one, so both are read alike; the container stays stopped (no state, no event of the
  visit's), one visit at a time, and `start` waits for one under way; what `cp` copied in
  is saved as its layer as the visit ends. It costs 5.7 ms at p50 over a running one's
  [PM M116]. `top` of a stopped container is refused, as dockerd refuses it.
- Open, and owed before the Agentfile: keeping the layer on the host from the start (a
  disk under the overlay) would cost a stop nothing and let a stopped container's files
  be read without a VM; to be measured against M115 and M116. It is also what makes
  shards hold what Docker holds: in guest memory, the files a container writes are its
  memory, charged to its cgroup, so that `stats` shows a container that wrote a 20 MiB
  file as 20.38MiB where Docker shows 1.398MiB (the file its page cache, which the CLI
  leaves out), and a container without a limit can write what its VM's memory holds,
  where Docker's writes what its host's disk does. Under a limit both alike kill a writer
  past it (`dd` of 100 MiB under `--memory 64m`: killed at 64 MiB in both, Docker 29.3.1,
  2026-10-05).
- **`cp`.** docker/cli's `cp.go` on shards-archive's port of go-archive's copy rules;
  dockerd's half (stat, archive, extract into a directory, `-a`'s owner) runs as init
  built-ins in the microVM, and the archive passes between it and the client through a
  pipe the daemon hands on, never through the daemon. Two of the CLI's faults are not
  kept: its progress line adds the running total to itself (`last += n`) where shards
  keeps what it drew, and a copy from stdin reports `0B` where shards counts what came.
  A stopped microVM's files are copied as a running one's.
- **Tests:** `start_runs_a_stopped_microvm_again_over_its_own_files` (files and
  deletions survive, `create`, `restart`, errors);
  `cp_copies_files_into_and_out_of_a_microvm_as_docker_cp_does`,
  `a_stopped_microvms_files_are_read_and_written_as_dockerd_does`; shards-archive's oracle
  for the layer's form against go-archive on overlayfs.

### Agentfiles: a Dockerfile and shards' directives (D35)

An Agentfile is a Dockerfile with the directives docs/architecture/AGENTFILE_ARCH.md
specifies (§4, with the review's answers of §12): `AGENT`, `HARNESS`, `SKILL`, `MCP`,
`NETWORK`, `CONNECT`, `ATTACH`, and `EXPOSE … AS/FOR` and `VOLUME` with options, a name
and `FOR`.

- **Read as one or the other.** `shards build` reads a file as an Agentfile where it is
  named as one, as Docker names a Dockerfile (`Agentfile`, `*.Agentfile`, `Agentfile.*`),
  and without `-f` finds the context's `Agentfile` before its `Dockerfile`. A Dockerfile is
  read as BuildKit reads it, byte for byte (`tests/oracle.rs`): its `AGENT` is an unknown
  instruction, its `VOLUME --chown` an unknown flag. The parser knows the dialect
  (`parser::parse_as`), since BuildKit keeps no arguments of an instruction it does not
  know; `SKILL` takes heredocs as `ADD` does. The build names the file it reads in its
  progress, as BuildKit names `-f`'s (`load build definition from Agentfile`). A
  `# syntax=` line will name the frontend (§8 Q16) once it exists.
- **Names.** Agents, harnesses, MCP servers and networks are named as stages are
  (`^[a-z][a-z0-9-_.]*$`, read lowercase). Stages, agents and harnesses share one
  namespace (§7 Q19.2): a name declared twice is an error naming the first declaration;
  MCP servers and networks are each declared once. Every name a grant uses (`FOR`,
  `CONNECT`, `ATTACH`, `EXPOSE … FOR`) is checked against the declarations of its stage's
  lineage, a stage seeing those of the stages it is built `FROM` (§7 Q19.5), of the kind
  `--target-kind` says; a `CONNECT` joins one kind, on networks whose `FOR` allows each
  name (§12.10, §12.11). `ONBUILD` takes none of the directives: a trigger runs in another
  file's build, a Dockerfile's maybe, and what an Agentfile grants is its own.
- **What Docker sees.** `EXPOSE … AS ingress` and `AS` both ways expose their ports in the
  image config, as `EXPOSE` does; `AS egress` exposes none, the container listening on none
  of them (§12.6). `VOLUME` lists its mount points in the config, its name and grant aside
  (§12.8).
- **Where the directives travel** (§8, docs/research/oci-artifacts.md §4). The target
  stage's lineage's directives, in order, defaults resolved (`/agents/<name>`,
  `/harness/<name>`), are written as JSON to `/.agentfile.json`, mode 0444, in a layer of
  its own, the build's last: the normalized Agentfile, which shards' runtime reads, and
  which survives every store and copy. Its first field is its schema's version
  (`schemaVersion`: 1), which a reader refuses past what it knows. The config label
  `vnd.osi.agentfile.digest` carries its digest (`vnd.osi` as the OSI's media types,
  `application/vnd.osi.agent.v1`, which no IANA registration holds).
- **Built so far.** The directives that declare and grant (`NETWORK`, `CONNECT`, `ATTACH`,
  `EXPOSE`, `VOLUME`) build. `AGENT`, `HARNESS`, `SKILL` and `MCP`, whose content a build
  fetches, refuse to build until it does, by name.

### Volumes and bind mounts (D38)

`-v`, `--mount` and `--volumes-from`, and an image's `VOLUME`s, are read and kept as
dockerd reads and keeps them (moby docker-v29.8.1 daemon/volume/mounts linux_parser.go
and validate.go, daemon/volumes.go registerMountPoints; `volumes.rs`), and reach the
microVM as virtio-fs shares (virtio 1.3 §5.11; Linux fs/fuse/virtio_fs.c):

- **Kept as dockerd keeps them.** A container's mount points are registered as it is made,
  named volumes made as they are named, anonymous ones for the image's `VOLUME`s and
  `-v DEST`, binds checked; a later failure removes the anonymous volumes it made, as
  dockerd's cleanup does. Volumes live in the home (`volumes/NAME/_data`, `opts.json`)
  as the local driver keeps them under its root. `rm -v` and `--rm` remove a container's
  anonymous volumes that no other container mounts (removeMountPoints), `--rm`'s as its
  removal completes; a command that reads volumes waits for removals queued, so that,
  as with Docker, `run --rm` then `volume ls` lists none of its.
  `inspect` gives dockerd's `Mounts`, `HostConfig.Binds`, `Mounts`, `VolumeDriver`,
  `VolumesFrom` and `Config.Volumes`, held to Docker Engine 29.3.1's by
  testdata/inspect.json; its `Mounts` is sorted by destination, where dockerd lists a
  map's order.
- **Served by a process of the run's, not the VM's.** The VM process is confined before it
  is given a run (D30): Landlock (Linux) and App Sandbox (macOS) let it reach no directory
  given later, and its seccomp list holds no filesystem calls. So each run that shares
  anything gets a share process (`shards share`, `share.rs`; the one binary, D36),
  started by the daemon with the directories open, reaped on the followers' loop. The VM
  device forwards each FUSE request over a Unix socket, one connection a share, and the
  share process answers it (`fs/server.rs`): every operation relative to a descriptor of
  the shared tree, never following a final symlink (`O_NOFOLLOW`), names that are one
  component only. Its connections reach the VM after the run is taken: the run's message
  carries one socket, over which the share process sends a connection a share,
  `MAX_FDS` a message, keeping its copies until the VM says it has them (M24). A VM
  restored ahead of its run has its devices (`shards{i}`), empty slots its run fills:
  a template is made for each number of shares (`run::template`). A guest that mounts
  a share before its run has one is told ENODEV.
- **Owned as Docker Desktop owns them.** A host file's owner is the user's; the guest sees
  the owner, group and mode recorded in the xattr `com.docker.grpcfuse.ownership` that
  Docker Desktop's file sharing writes (`{"UID":…,"GID":…,"mode":…}`), root's where there
  is none, and a file the guest makes records its maker (a setgid directory's group).
  Unlike Docker Desktop (fakeowner), the guest kernel checks permissions against those
  owners (`default_permissions`): user 1000 cannot write a directory root owns, as on
  Linux. The owner xattr is hidden from the guest's `listxattr`.
- **Mounted by init** (`init/src/setup.rs`, `volume`): each share at its destination,
  read-only where asked; with the mounts of `--tmpfs` and `--mount type=tmpfs`, sorted by
  depth (sortMounts). A file bound alone is shared through its directory, the server
  limited to its one name (the rest of the directory neither listed nor reachable, no
  name moved in or out), and bound at its destination from a staging mount. A volume
  whose destination holds files in the image, first mounted empty, gets them copied in
  as continuity's CopyDir copies them (owners, modes, times, xattrs, hard links), then
  its root's owner and mode (copyExistingContents); dockerd copies at create, shards at
  the first start, the only moment the image's files are in reach.
- **`shards volume`** (`daemon/volume.rs`): `create`, `ls`, `inspect`, `rm` and `prune`
  as `docker volume` (docker/cli v29.8.1 cli/command/volume; the help and parse held by
  the docker-cli oracle), dockerd's volume service behind them (filters `dangling`,
  `name`, `driver` and `label`; prune's `label`, `label!` and `all`, anonymous volumes
  alone without `--all`), its words held to Docker Engine 29.3.1's (`create NAME:
  invalid option`, `get NAME: no such volume`, `remove NAME: volume is in use - [ID]`),
  `volume ls`'s table the CLI's formatter's (formatter/volume.go, held to its output).
  Anonymous volumes carry dockerd's `com.docker.volume.anonymous` label. One lock orders
  volume changes and a container's registration, so that no volume a container is being
  made with is removed: the reference counts' guarantee. `system df` counts volumes
  (LocalVolumesSize: size and references), `system prune --volumes` prunes the anonymous
  ones after the containers, as pruner.pruneOrder has it. A colour terminal gets shards'
  pages: each volume, what it holds, and how many microVMs mount it. Shards' grammar says
  them as `create volume`, `list volumes`, `inspect volume`, `remove volume`, `prune
  volumes`.
- **The local driver's options** are dockerd's (`type`, `o`, `device`, `size`, and which
  require which). A volume of a host directory (`o=bind`, its `device`) is shared as a
  bind is. A size wants quotas, refused as dockerd refuses it on a filesystem without
  them. Other types are made, as dockerd makes them, and refused at start: the guest
  kernel has no NFS or CIFS client, and a tmpfs volume mounted in each microVM would not
  be the one tmpfs that Docker's containers share.
- **The kernel** has `CONFIG_VIRTIO_FS` and `CONFIG_FUSE_DAX` (kernel-6.18.48-98788948976a).
- Open, unmeasured: the forwarding hop's cost per request against an in-process server,
  and throughput against Docker Desktop's virtiofs; DAX windows, which would let file
  data skip the hop; the local driver's other types (NFS and CIFS clients in the guest
  kernel; a tmpfs volume held where every microVM that mounts it shares it).

### Restart policies (D39)

`--restart` is read as docker/cli reads it (opts.ParseRestartPolicy; `--rm` refused with
it), checked as dockerd checks it (ValidateRestartPolicy), and kept with the container.
Its restart manager is dockerd's (moby docker-v29.8.1 daemon/internal/restartmanager,
monitor.go handleContainerExit; `daemon/restart.rs`):

- **Whether.** `always`; `unless-stopped` unless stopped by hand (any stop or kill, but
  the daemon's own); `on-failure` on a non-zero status, under its count where it has one.
  A command that never started is not restarted. A stop or kill by the container's stop
  signal or SIGKILL cancels the next restart (ExitOnNext); a SIGHUP does not.
- **When.** After a wait that starts at 100 ms, doubles each time to at most a minute,
  and starts over once a run lasted 10 s. The wait is a deadline on the followers' loop
  (no thread waits it out); the start then goes through the daemon's own door, a detached
  `start` from a client of its own making, so a restart is handled, followed and recorded
  as any start is.
- **Meanwhile.** The container is restarting: running to `ps` (listed without `-a`,
  `Restarting (CODE) X ago`), to inspect (`Status` restarting, `Running` and `Restarting`
  true) and to `wait`, which waits on; `RestartCount` counts, and `shards start` starts it
  over. `stop` and `kill` stop it there; `rm` refuses it without `-f`, as dockerd does.
- **As the daemon starts**, the containers its stop ended start again where their policy
  says, as dockerd's restore does: shards' daemon starts with the first command, so they
  start with it.

### Updates (D40)

`shards update` is `docker update` (docker/cli container/update.go; moby docker-v29.8.1
daemon/update.go and container UpdateContainer, `daemon/update.rs`): what is asked
checked as at create (verifyContainerSettings with its `update` flag: swap alone
passes), merged into what the container has with dockerd's refusals (Nano CPUs against a
set CFS period or quota and the reverse; a memory limit over the swap kept, which for a
container made without one is none, as Docker refuses it; a restart policy beside
`--rm`), kept with its request, and written to a running workload's cgroup at once by an
init built-in (`builtin::CGROUP`, the writes runc's fs2 makes as it starts). A limit the
guest refuses changes nothing. Its words and its order of output are dockerd's and the
CLI's, held to Docker Engine 29.3.1's.

- `--blkio-weight` (run, create, update) is BFQ's weight where the guest kernel has BFQ,
  else `io.weight` on io.cost's scale (ConvertBlkIOToIOWeightValue), as runc writes it.
  The guest kernel has both; Docker Desktop's VM has neither and refuses the update.
- A microVM's memory is sized as it starts (PM M117): a limit raised past it binds at the
  VM's size until its next start. Open: resizing the guest (balloon or memory hotplug).
- `--cpu-rt-period` and `--cpu-rt-runtime` parse, and are not served: cgroup v2 has no
  real-time CPU controller to write them to.
- inspect's raw-JSON fallback (a template Docker's typed struct cannot answer) decodes
  numbers as encoding/json's Number, which prints its digits and encodes as a number.
- **Sizes** (`ps --size`, `inspect --size`; list.go asks for them too where a format
  shows `.Size`): SizeRw is the disk the writable layer uses, counted as containerd's
  snapshots count it for Docker 29 (continuity DiskUsage: each inode's blocks once,
  directories too): by an init built-in while the microVM runs (`builtin::SIZE`), and as
  its last run left it after, which init counts as it saves the layer (`kind::USAGE`).
  SizeRootFs adds the image's root filesystem as it is on the host (its EROFS disk). The
  numbers are the guest filesystem's blocks, not Docker Desktop's ext4's: the same writes
  measured 16 kB in a microVM and 8 kB under Docker 29.3.1.

### Import (D41)

`shards import` is `docker import` (docker/cli image/import.go; moby docker-v29.8.1
image_routes.go postImagesCreate and daemon/containerd/image_import.go, ImportImage;
`daemon/import.rs`): a tarball from a file or stdin, which the client sends, or a URL the
daemon fetches (BuildKit's HTTP source, `build/http.rs`), made an image of one layer, its
config BuildFromConfig of `--change` over an empty one, its platform `--platform`
normalized or the host's default spec (`v8` on arm64, as containerd's DefaultSpec), its
history one step with the comment (`-m`, else `Imported from -` or the URL), its ID the
manifest's digest. Its output, refusals and ordering are the CLI's and dockerd's, held to
Docker Engine 29.3.1's.

- The layer is kept as it came where it is a layer already: gzip and zstd as they are,
  bzip2 and xz decompressed (neither is a layer's media type), and a plain tar as it is,
  where dockerd spends a gzip on it: no compression on the import's path, the same diff ID.
- What an import or a commit writes is under the store's lease until it is named: a
  collection, which a daemon starting runs, took an import's blob from `ingest/` before.
- What it makes is a microVM, as a pull makes one (D25): an import of a container's files
  builds the image's EROFS disk at once and publishes it to the local engine as
  `shards.local/NAME`, so that its first run builds nothing. The
  tarball is kept once as it came and then read by what it is: an OCI image layout or a
  `docker save` archive is taken as `load` takes one (each image made a microVM, the one
  image it holds also named by the reference given), where dockerd would make a
  meaningless one-layer image of the archive's files; and a shards microVM (below) is one
  already, imported whole, its disk its root filesystem with nothing to unpack.
- A shards microVM is an OCI artifact (`application/vnd.shards.microvm.v1`): its own
  config the empty one, and two layers, its EROFS disk (`application/vnd.shards.erofs.v1`)
  and the image config it was made from (`application/vnd.shards.microvm.config.v1+json`),
  which `inspect` shows and a run runs by (`Manifest::image_config`). Docker 29.3.1 loads,
  lists and saves it and will not run it; `docker save shards.local/NAME | shards import -`
  brings one back.
- Publishing to the local engine never holds up what asked for it, and is robust to the
  engine (`local_store.rs`, PM M118). A pull, import or `rmi` only queues its request;
  each name's requests go in order on a thread of the name's, the ones queued behind a
  request in flight collapsed to the last, and names do not wait on each other. No upload
  is cut off part way, by a deadline or otherwise: Docker keeps the content of an upload
  cut off locked, and later loads of it fail with 502 after 60 s or never end, while other
  content still loads; an engine stuck on one image's content so holds up that image
  alone. A microVM the engine holds already, by its manifest's digest (the ID it gives
  it), is not sent again: one GET of 5.4 ms p50 in place of an upload of the whole disk,
  which took 10 to 50 s on the same loaded host.
- The engine is the client's: `DOCKER_HOST`, `DOCKER_CONTEXT` and `SHARDS_LOCAL_STORE`
  are sent with each request, as the Docker CLI uses its own, the daemon's taken where the
  client sets none. The E2E tests set `SHARDS_LOCAL_STORE=none` (`common::command`) and
  publish only to an engine of their own; before, every test daemon published to the
  host's Docker, and any upload a test's end cut off would lock its content there.

### Security options and seccomp (D42)

`--security-opt` on `run` and `create` is Docker's (docker/cli opts.go parseSecurityOpts
and parseSystemPaths; moby docker-v29.8.1 daemon/daemon_unix.go parseSecurityOpt,
daemon/seccomp_linux.go WithSeccomp, oci_linux.go; runc v1.3.4's init), held to Docker
Engine 29.3.1's words and to what its containers see (`security_opt_confines_as_docker_does`):

- The CLI reads a profile's file and sends its JSON compacted (Go's json.Compact, its
  words on what is no JSON), takes `systempaths=unconfined` out for no masked or
  read-only paths, and refuses an option without a value but `no-new-privileges`.
  dockerd refuses the rest as the container is made ("invalid --security-opt 1/2"), keeps
  them in HostConfig.SecurityOpt (`label=disable` after for a privileged container), and
  takes labels and AppArmor profiles as a host without SELinux or AppArmor does, which
  the guest kernel has neither of; `writable-cgroups=true` leaves the cgroup hierarchy
  writable.
- The workload runs under Docker's default seccomp profile (moby/profiles seccomp
  v0.2.3), the one a container names, or none (`unconfined`, or privileged and naming
  none), resolved for its capabilities, the guest's architecture and the guest kernel's
  version (`minKernel`), compiled at start, cached per profile, capabilities and kernel,
  and loaded by shards-init where runc loads it: before the capabilities go, or with
  `no-new-privileges` just before the command; an exec takes the container's. A profile
  that cannot be loaded fails the start, not the making, the container kept created with
  exit code 128 and the reason in State.Error, which shards now keeps for every start that
  fails (it was always empty); `docker run` exits 125.
- The filter is shards' own (crates/seccomp). Its rules are kept as libseccomp 2.5.4
  keeps them (db.c ported), so that a profile's overlapping rules mean what they mean to
  Docker, and Docker mode in its tests decides as runc's program does on every input of a
  grid and refuses in its words, for 33 cases on both guest architectures recorded on
  native Linux (scripts/seccomp/generate). Where Docker's toolchain gets a profile wrong,
  shards does better, each difference pinned by its tests:
  - Names are read by the guest kernel's own tables (Linux 6.18.48,
    scripts/seccomp/kernel-tables). Docker's runc reads them by its libseccomp's: Docker
    Desktop 29.3.1's refuses listmount, statmount and mseal, which its own default
    profile allows and its kernel has, with ENOSYS (PM M119).
  - runc's -ENOSYS stub for syscalls past the profile's last is kept; a number below it
    that is no syscall is ENOSYS too, as the kernel's own answer.
  - On x86, libseccomp writes socketcall(2)'s call number over a socket rule's first
  comparison, so that `socket` allowed but for some families is socketcall allowed for
  every family; here the multiplexer takes such a rule's action only where it is the
  more restrictive. ipc(2)'s call is compared in its low 16 bits, which are all the
  kernel reads of it.
  - libseccomp's slip merging 64-bit LT/LE rules (it sets the true action) is fixed.
  - `--privileged --security-opt seccomp=builtin` is the default profile; dockerd reads
  `builtin` as JSON and fails.
  - The program: ranges of one decision by binary search, identical decisions shared, a
  third of runc's length and 43% fewer instructions per syscall at the median (PM M120).
- Not served: SCMP_ACT_NOTIFY, which needs a seccomp agent (listenerPath) shards does not
  run; it is refused as the container starts.

### stats (D43)

`shards stats` reads each running container's guest as dockerd's collector reads a
container's cgroup (moby daemon/stats/collector_unix.go): shards-init's STATS built-in
says the workload cgroup's CPU time, memory in use and its inactive file pages, its
limit (memory.max, else the VM's memory), its processes and its block devices' bytes,
the VM's CPU time from /proc/stat and its online CPUs, and the bytes its interfaces but
loopback's moved. docker/cli's helpers make the figures of two samples a second apart
(calculateCPUPercentUnix; memory less inactive_file on cgroup v2), and its formatter
(container/formatter_stats.go, `format::stats`) lays them out, `--format` with it; a
stopped container under `-a` shows zeros, as Docker's does. Every container is sampled at
once, each on its own thread. The CPU's denominator is the host's clock between the
samples, where docker/cli's is the system's CPU time from /proc/stat: inside a VM on a
loaded host that does not keep time (one spinning vCPU read 329% and 2503%). A paused
container's VM cannot answer; it shows the sample taken as it was paused, its CPU 0%,
where dockerd reads a frozen cgroup's files from outside it. Before, `stats` showed the VM process's own CPU and
resident memory and `--` for the rest. Memory is the guest's truth: the files a
container writes are memory while its writable layer is in guest memory (D37's open
item).

### Devices and block I/O (D44)

A microVM's workload reaches the devices Docker gives a container and those it is given, and no other of
its VM's: before, it could make a node of the VM's image disk (259:0) and read it. Its
cgroup has runc's eBPF device filter (opencontainers/cgroups v0.0.4, which runc v1.3.4
pins: the rules emulator of devices_emulator.go, the program of devicefilter.go), over
moby's default rules (daemon/pkg/oci/defaults.go) and runc's own (specconv
AllowedDevices, less those whose path a device given takes), in `crates/devcgroup`, a
crate of no dependencies that shards-init builds. Its program is runc's, byte for byte:
`tests` hold it to what `scripts/devcgroup/generate` records of runc's own deviceFilter,
run in a pinned Go on Linux, for 27 rule sets (Docker's own, `--device`s, every kind of
`--device-cgroup-rule`, privileged, the emulator's refusals). Execs are in its cgroup and
under it, as a container's execs are.

`--device` takes the VM's devices as Docker's takes its host's (WithDevices,
DevicesFromPath): one by its path, a directory's each, at the path in the microVM
asked, with its permissions, refused as dockerd refuses one it cannot find; the VM's
devices are those `/sys/dev` lists, which shards-init reads, with the modes systemd-udev's
default rules give a host's (fuse, net/tun, vsock and vfio 0666, rfkill 0664; v257
50-udev-default.rules.in) where devtmpfs's are 0600, so that the image's user opens
`/dev/fuse` as on a Docker host; `--privileged` makes its nodes from the same list. A
CDI device's name is a device request, as the CLI makes it, which no VM has a spec for:
refused in dockerd's words. `--device-cgroup-rule` is checked by the CLI's pattern and
read as dockerd reads it; `--device-read-bps`, `--device-write-bps`, `--device-read-iops`,
`--device-write-iops` and `--blkio-weight-device` as docker/cli's validators take them and
dockerd stats their paths (getBlkioThrottleDevices), written as runc's setIo writes them:
io.max, and BFQ's per-device weights where its weight file takes them, which the kernel
refuses for the image's disk, bio-based, as it refuses runc on a host whose disk BFQ does
not schedule. Init does it all, outside the workload's cgroup, in Docker's order:
dockerd's lookups (the I/O limits' paths, the devices, the rules, the CDI devices), the
nodes, then runc's cgroup Set (pids, memory, the weight, the devices' I/O, CPU, the
filter, cpusets), each failure in its words with runc's procHooks prefix; dockerd's own
fail the start with 128, untranslated. `inspect` says each as Docker's does.

Improvements over Docker, each measured or tested:

- **No cost to a run with Docker's rules.** Init attaches the default filter as the VM
  boots, before the template's snapshot: every VM restored from it has it. A run with
  other rules swaps its own in (BPF_F_REPLACE, as runc replaces its one old program), a
  privileged one takes it away. Loading the filter at each start cost a run 1026 µs of
  its time in the guest; at boot, nothing (PM M121).
- **An `a` rule allows what it says.** runc's emulator reads any `a` rule as every
  device, every access: `--device-cgroup-rule 'a 1:2 m'` or `'a *:* r'` lets a Docker
  container read, write and make every device. shards reads one that is not `a *:* rwm`
  as a block and a char rule of its numbers and access (tested: `a *:* r` reads a loop
  device and cannot write it).
- `shards start` of a microVM that cannot start says why as `docker start` does, without
  run's help (it said run's before, for every start that failed).

Open: BuildKit's RUN steps run under containerd's default device rules; `shards build`'s
steps are to be held to them.

### attach (D45)

`shards attach` joins a running microVM's command as `docker attach` joins a container's (docker/cli
container/attach.go; moby daemon/attach.go, daemon/internal/stream CopyStreams): the CLI
inspects the microVM first, for its terminal and its stdin, and refuses a stopped,
paused or restarting one, a missing one, and a terminal's stdin that is not one, in
docker/cli's words; the client goes to the microVM's VM process as an exec's does (`ATTACH_RUN`,
held until the VM has it, M24), which writes it the command's output from then on,
passes its stdin on where the microVM's command reads one (`-i`), its signals without a terminal
(`--sig-proxy`), and its terminal's size, one row and column larger first so that the
command redraws (resizeTTY), and tells it the command's status, its own. The detach keys
leave with "read escape sequence" and 1, the microVM running. A client's stdin ending
closes the command's where moby's does (StdinOnce, no terminal), so a detach never closes
a shell's: before, `run -it`'s detach did.

Beyond Docker: where its stdin's end does not close the command's, an attached client's
output goes on to the command's end, where dockerd ends the attach with its stdin and
what the command says after is lost (measured: `echo hello | docker attach` of a
`run -di` container that answers printed nothing; shards' of a `run -di` microVM prints the answer; tested).
A slow client holds the command's output back, as Docker's and shards' own `run` client
do.

### Networks of microVMs (D46)

`shards network` makes networks of microVMs as `docker network` makes them of
containers, with Docker's inputs and words: create, ls, inspect, rm, prune, connect and
disconnect, as docker/cli parses them (held byte for byte to docker/cli by
`scripts/docker-cli/generate`) and as dockerd 29.3.1 (f78c987a) answers them, measured
on an isolated Docker Engine 29.3.1 (a docker-in-docker container, so that no probe
touches the user's Docker). Networks are kept in the home, one file each; dockerd's
predefined `bridge`, `host` and `none` are listed beside them, the bridge on the subnet
the daemon elected (D31).

- **Addresses, as libnetwork's IPAM gives them.** A network's IPv4 pool is the subnet
  asked for, or the lowest of dockerd's default pools that overlaps no other network's
  nor the host's (InferReservedNetworks); its gateway is the first free address of its
  range, or of its subnet; each microVM's is the lowest free one (not the next: a freed
  address is the next given), or its `--ip`. Status' counts are dockerd's (IPsInUse
  marks the network and broadcast addresses, the gateway, auxiliary addresses and
  members). IPv6 pools are kept and shown as dockerd's, the ULA one derived from the
  home's engine ID by dockerd's formula; a microVM's guest has no IPv6 (D31), so takes no
  address of one.
- **One template for every network.** A microVM on a network boots on the default
  bridge's template, as every run does, and shards-init moves eth0 to its own address
  and gateway as the run starts (one netlink address swap and route replace, only where
  the address differs), before it writes /etc/hosts, which names it. Its network process
  takes the address over its control socket (`NET_ADDRESS`).
- **MicroVM to microVM.** The daemon pairs the network processes of every two members
  with a stream socket (`NET_PEER`); a process sends a frame whose destination is a
  peer's address to that peer whole, its virtio header's segmentation cleared (a
  65520-byte MTU's TCP sends no aggregate) and its partial checksum kept, which the
  peer's device takes (GUEST_CSUM). A peer's frame is taken only from its own address to
  this guest's, and given the gateway's MAC as a routed frame; every peer is reached
  through the gateway's MAC, which the process answers ARP with. A frame a full socket
  cannot take whole waits only for the frame before it; others are dropped, as a full
  switch port drops them, and TCP sends them again. Members reach each other past the
  default deny; nothing else is opened (D31): a network is what Docker's `--internal`
  one is, and a name off it is SERVFAIL, as Docker's internal networks answer.
- **Names, at Docker's address.** A microVM on a network has Docker's resolver address,
  127.0.0.11 (resolv.conf `nameserver 127.0.0.11`, `options ndots:0` after its own, no
  comments naming an engine); a process of shards-init, outside the workload's cgroup,
  relays each query there, by UDP and TCP, to the network process at the gateway, which
  answers what Docker's embedded DNS answers (measured): each member's name, aliases,
  short ID and host name as A records with a TTL of 600, whatever the case; AAAA with
  none; PTR as `name.network.`. The daemon sends each member the table as members come
  and go (`NET_NAMES`). Programs written for Docker that ask 127.0.0.11 by address
  (nginx's `resolver`) work unchanged.
- **Members.** A microVM is a member while its run lasts: from its start, under the
  network's lock (no two take one address), to its end; one whose start fails is no
  member, however it failed. `network rm` refuses a network with members in dockerd's
  words; inspect lists them, and a microVM's NetworkSettings carry its endpoint as
  dockerd's, its network's ID and DNS names kept while it is stopped.
- **Measured and tested** (containers.rs `microvms_on_a_network_reach_one_another_by_name`):
  reach by every name and by address, the server seeing each client's own address, no
  reach from a microVM off the network, Docker's resolv.conf, inspect's endpoint, and
  dockerd's refusals; the network process's resolver (net/src/dns.rs), the IPAM
  (networks.rs) and the CLI's consolidation (daemon/networks.rs) held to Docker's answers
  in unit tests.

Unlike Docker, by the microVM: a microVM has one network device, so it is on one network.
One left on the default bridge and connected to one user network is on that network
alone, which under default deny gives it all the bridge would. A microVM on two user
networks is refused ("a microVM on more than one network is not supported by shards
yet"), and so is connecting or disconnecting a running one.

Open:
- A microVM on several networks, and connecting a running one: a network device per
  network, added to a running VM.
- A MAC per microVM: every microVM restored from one template shares the template's, which
  inspect shows.
- The TTY pages of `network ls` and `inspect`, in shards' own design (they print Docker's
  table on a terminal too).
- Egress grants for a network (AGENTFILE_ARCH §4.6 `NETWORK --egress`), with the
  Agentfile.

### Every action makes an image a microVM first (D47)

`shards run vm IMAGE` has made a microVM of a container image as it pulls it since D25:
any OCI image a registry or Docker holds is one shards runs. Shards' grammar does the same
for every other action on an image or a microVM that names an image: `inspect vm IMAGE`,
`inspect image`, `history image`, `tag image`, `push image` and `save image` pull an image
that is not here yet and convert it, as `run` does (one path, `run::find_image`, saying
the pull as `docker run` says it), and then act on the microVM it makes. `inspect vm IMAGE`
of a name that is no microVM describes the image's microVM. Removal converts nothing: it
fetches nothing to delete it. Docker's own order (`image inspect`, `history`, `tag`) answers
as Docker's does, "No such image" included, so that scripts written for Docker see
Docker's answers; the conversion is shards' grammar's (`--convert`, shown under shards'
options in each command's help). Tested in `images.rs`
(`actions_on_an_image_not_here_convert_it_first`, on real microVMs, and that neither
Docker's order nor removal fetches anything) and in the grammar's unit tests.

### ADD of Git repositories: shards' own client (D48)

`ADD <git URL> DEST` builds as BuildKit's git source builds it (moby/buildkit v0.28.1
source/git/source.go, the backend of Docker Engine 29.3.1, measured there against a real
smart HTTP server: every form, file header and error), but without a `git` beside `shards`
(D36): `crates/git` is a client of its own, written from git's documents
(gitprotocol-common, gitprotocol-v2, gitprotocol-http, gitprotocol-pack, gitformat-pack,
gitformat-index) and its code where they leave a format to it (tree-walk.c, commit.c,
tag.c, submodule-config.c).

- **Fetching.** Protocol v2 over smart HTTP(S) (the registry's client, its proxies and
  TLS) and over `git://` (git's daemon); a server that speaks only v0 is refused, as is an
  SSH remote until `--ssh` is served. One commit is fetched one deep (`deepen 1`), a full
  commit name too, where BuildKit fetches all of its history (`--unshallow`). Packs are
  read with both delta kinds; every object is named from its data with collision-detecting
  SHA-1, as git's sha1dc names them, so a pack holds only what it claims; the pack and each
  object are bounded by what a build may unpack.
- **Refs** resolve as BuildKit resolves them: the default branch from HEAD's target;
  `refs/NAME`, then `refs/heads/NAME` before `refs/tags/NAME`; an annotated tag peeled; a
  40- or 64-digit lowercase name as a commit, a shorter one as a ref. `--checksum` and
  `?checksum=` take a hex prefix of the commit's or the tag's name. Errors are BuildKit's
  (`repository does not contain ref NAME, output: ""`, `invalid subdir ...`, `expected
  checksum to match ...`, under `failed to load cache key:` where BuildKit says it).
- **The checkout** is git's under BuildKit's umask: files 0644 or 0755 by the owner's
  execute bit (canon_mode), symlinks, directories 0755, all root's; `#REF:SUBDIR` takes a
  directory every part of which is one (validateDirsOnly); submodules, unless
  `?submodules=false`, fetched one deep from `.gitmodules`' URLs, relative ones resolved
  against the superproject's, recursively.

Unlike BuildKit, by design:
- **Every entry's time is the commit's** (its committer's), where BuildKit's is the clock's
  at checkout: the same commit makes the same layer on every builder and every build
  (tested: built again with `--no-cache`, the layers are the same bytes), as frontend
  1.27.1's `mtime=commit` asks of a build context. Measured: BuildKit's two fresh
  checkouts of one tree made two layers differing only by mtimes.
- **No submodule `.git` file naming the builder** (`gitdir: ../../../../52/fs/modules/...`,
  BuildKit's, a path into its snapshot store that dangles in every image).
- **A ref's checkout is that ref's alone.** BuildKit checks out through a repository shared
  by every build of a URL, whose index and config outlive them: measured, a commit with no
  submodule checked out after one with a submodule got that submodule's files.
- **A checksum that matches neither an annotated tag nor its commit names both**
  (BuildKit's message loses the tag's: `got  or <commit>`).
- **`--keep-git-dir` keeps a `.git` that is the commit's alone:** HEAD detached at it,
  `shallow`, `origin` the URL without credentials, the ref asked for (a branch under
  `refs/heads`, a tag under `refs/tags`, none for a commit's name), the fetched pack with
  its index, and an index of the work tree at the commit's time; each submodule's
  repository under `.git/modules`, its work tree's `.git` naming it. BuildKit's holds the
  builder's inode numbers and times, its hostname in a reflog, and refs that depend on
  what earlier builds fetched. git reads shards' as its own (`verify-pack`, `fsck`, a
  clean `status`; crates/git/tests/real_git.rs). With a subdir no `.git` is kept, as
  BuildKit keeps none.

Tested: crates/git against git itself (its packs object for object, a shallow fetch from
its upload-pack checked out as `ls-tree` and `show` list it, refs as BuildKit resolves
them, `git daemon`, a kept `.git`); `build.rs` `add_fetches_git_repositories` on real
microVMs, from a git daemon and from smart HTTP served by git's own upload-pack.

- **Credentials from the build's secrets**, as BuildKit's git source takes them (v0.28.1
  source.go authSecretNames, getAuthToken, tokenScope): the first of
  `GIT_AUTH_HEADER.<host>`, `GIT_AUTH_TOKEN.<host>`, `GIT_AUTH_HEADER`, `GIT_AUTH_TOKEN`
  (`--secret id=...`); a token sent as `basic` credentials of `x-access-token`, a header as
  it is; to the remote, or to all of github.com for a github.com remote, scoped as git
  scopes `http.<url>.extraheader` (urlmatch.c), so a submodule elsewhere is not sent them
  (tested: `add_fetches_git_with_the_builds_secrets`, refused without them).

- **SOURCE_DATE_EPOCH from a Git stage**, as dockerfile/1.27.1 takes it (epoch.go,
  `sourceDateEpochFromMetadata`): the stage's ADD made the source `llb.Git` makes
  (`plan::git_identifier`, which planning shares), its ref resolved and checked as a fetch
  resolves it, the commit fetched with the build's secrets and SSH agent, and its
  committer's time, not its author's (tested: `source_date_epoch_is_taken_from_a_source_stage`,
  mutation-checked; a ref the repository lacks fails the build).

- **A commit a server will not send alone** ("not our ref": a commit asked for by its
  name, or a submodule's pinned commit, that is no ref's tip, from a server that keeps
  `uploadpack.allowReachableSHA1InWant` off): every ref's history fetched whole, as BuildKit
  then fetches it (`git fetch --tags origin`), and a kept repository not shallow, its
  history whole; a commit in no ref's history refused in the server's words (tested:
  `add_fetches_a_commit_a_server_will_not_send_alone`, against a server that refuses as
  such hosts do; mutation-checked). git's own upload-pack, speaking protocol v2, sends
  such a commit (measured, git 2.50.1), so the fallback is for hosts that do not.

SSH remotes: D69.

### RUN steps under Docker's default seccomp profile (D49)

BuildKit runs every step under moby's default seccomp profile (its executor's
oci/spec.go WithDefaultSeccomp; `Seccomp: 2` in a step's `/proc/self/status`, measured), an
insecure step unconfined. shards' builder does the same: the profile `docker run` applies
(D42, `crates/seccomp`, compiled as libseccomp and runc compile it) is compiled once per
build for a step's capabilities (the defaults a container has, `shards_abi::run::CAPS`), the
builder's architecture and its kernel's version, and carried in each step
(`shards_abi::build::Step::seccomp`); the builder guest loads it while the step is still
root with every capability, as runc loads a container's before it changes user where
no_new_privs is unset (BuildKit sets none), so that the change of user, the capabilities
and the exec that follow run under it. Tested in
`run_steps_take_the_builds_secrets_ulimits_and_entitlements` (mutation-checked: a step's
`Seccomp:` is 2, an insecure step's 0).

### The build cache (D50)

A step `shards build` has run before, from the same definition and inputs, is not run
again: its result is the layers it made then, and its progress says so as BuildKit's does
(`#N CACHED`), as BuildKit's solver takes a vertex its cache key finds.

- **Keys.** A step's key is the SHA-256 of shards' version, its definition, and each input's
  key in its inputs' order, not where they sit in the plan. An input a chain of layers
  makes, a base image or an earlier step, is keyed by what made it; one none makes, the
  build context, a download or a Git checkout, by its content: each path's kind, mode,
  owner, extended attributes, link target and bytes, without times, as BuildKit's content
  checksums take them (a cached step's layers keep the times they were made with, as
  BuildKit's do). A source is keyed by what it is and what it holds, not its session
  attributes (`local.unique` differs on every build).
- **Records** are the store's (`buildcache/v1`), one per key: each of the step's outputs
  as its layers, with their history, the size of the step's own layer, when it was made and
  last used, and how often it has been. Their blobs are roots of the store's collector
  while a record is there, as a reference's are.
- **What uses them.** `--no-cache` takes none, and still records what it makes, as
  BuildKit's does. `system df` counts them on its Build Cache row and lists them under
  `-v` (each step's own layer, shared where an image holds it, reclaimable where not);
  `system prune` removes them all, as `docker system prune` removes all build cache no
  build is using, its blobs collected where no image holds them.

Tested on real microVMs (`builds_reuse_the_steps_they_have_run`): built again, every step
is cached and the image's layers are the same; a changed context file runs again what
reads it and what follows, not what came before; `--no-cache` runs all; `system df` and
`system prune` count and remove the records.

Open: a `COPY`'s key from the paths it copies alone, where it is from the whole context,
so that a change elsewhere in it runs again what BuildKit's checksums would keep;
`--no-cache-filter`; `--cache-from` and `--cache-to`; `docker builder prune` and its
filters; a bound on what the cache keeps (BuildKit's default GC policy); `RUN
--mount=type=cache` kept across builds.

### RUN steps reach the client's SSH agent (D51)

`RUN --mount=type=ssh` reaches the agent `--ssh` names, as BuildKit's steps reach it:
`--ssh` read as buildx reads it (each `ID[=PATH,...]` kept, not normalized,
commands/build.go), its agents as BuildKit's sshprovider takes them (an id `default` where
none is given, `SSH_AUTH_SOCK`'s socket where no path is, one socket an agent, BuildKit's
errors word for word: held to buildx v0.37.1 and BuildKit by `scripts/buildx/generate`);
the mount at `/run/buildkit/ssh_agent.N` with `SSH_AUTH_SOCK` set, an empty id read as
`default`, a mount not `required` left out without an agent, a required one refused in
BuildKit's words.

- **Through the microVM.** The builder guest listens on a socket of the step's own, of the
  mount's mode and owner, bound at its target, and relays each connection to the host
  over vsock (`shards_abi::build::SSH_PORT`), opened with a token the host made for that
  mount of that step (16 bytes of getrandom) and the agent's id; the host takes relayed
  connections only while a step runs, and only of a grant of that step's, closing any
  other at once, so that a step given no agent, or another step's, reaches none by
  dialling the port itself (tested: an insecure step, which can make vsock sockets, is
  closed on unanswered). Both ends' relays stop as the step ends.
- **Read-only, as BuildKit's agent is.** BuildKit serves a step a read-only wrapper of the
  agent (readOnlyAgent): keys and signatures, no adding, removing, locking or extensions.
  The host relays an agent request only if it is one of those (identities, sign, unlock)
  and answers any other SSH_AGENT_FAILURE, each message bounded by OpenSSH's 256 KiB.
- **Tested** on real microVMs against a real ssh-agent holding a key ssh-keygen made
  (`run_steps_reach_the_clients_ssh_agent`): the step lists the key and has the agent sign,
  its request to forget the keys is refused and the agent keeps them, a rogue dial is
  refused; both the filter and the token check are mutation-checked.

Open: key files given to `--ssh` (BuildKit loads them into an agent of its own, which signs
with them: an in-process agent with AWS-LC's signatures); Git remotes over SSH (`ADD
git@...`), which need an SSH client; a `default` agent added where the build context is an
SSH Git URL, as buildx adds one.

### Builds write their outputs as BuildKit's exporters do (D52)

`--output` and `--push` are read and checked as buildx v0.37.1 reads them (ParseExports,
CreateExports, the `--push`/`--load` folding and `mode=delete`'s check; held to buildx by
`scripts/buildx/generate`), and each output is written as BuildKit v0.28.1's exporter
writes it through buildx, measured inside dockerd 29.3.1 (Docker's containerd store) and
read in its source (fsutil a2aa163d723f, containerd v2.2.1, buildkit client/ociindex):

- **`local`** (`-o DIR`): the whole root filesystem, received as fsutil's DiskWriter
  receives it (`shards_archive::receive`): the directory made 0700 if new and merged into
  if not (emptied of the rest with `mode=delete`), a directory over a directory taking
  only its attributes, anything else replaced; modes with set-id and sticky bits, hard
  links, symlinks, empty directories, nanosecond times (directories' set last), every
  entry owned by the one who builds, as buildx's receive filter sets it.
  - **Better than fsutil:** every name is walked from the destination as Go's os.Root
    walks it, so neither the tree's symlinks nor ones the destination already held take a
    write outside it; fsutil joins paths and follows what the destination held.
- **`tar`** (`-o -`, `type=tar`): fsutil.WriteTar through Go's archive/tar
  (`shards_archive::tar`, held to Go): walk order, no root entry, owners kept with no
  names, times rounded to the second, `PaxHeaders.0` records only where USTAR cannot hold
  a field, two zero blocks; stdout's and a file's are one archive (tested).
- **`oci`/`docker`**: the image as an OCI layout of the store's own blobs, in a tar as
  containerd's exporter writes one (names in order, times 0, blobs 0444, documents
  0644), with `org.opencontainers.image.created` (export time, or SOURCE_DATE_EPOCH) and an
  index entry per name (`io.containerd.image.name`, `org.opencontainers.image.ref.name`);
  `docker` adds `manifest.json` and names its manifest and layers in Docker's media types
  (`toDockerLayerType`). With `tar=false`, `oci` writes a content store as buildkit's
  client does: blobs it lacks, `ingest/`, `oci-layout` 0644, `index.json` read and
  merged under a lock (`latest` its name without one).
- **Images and pushes**: an image is kept and named only where an output loads it (none
  given, `--load`, `image`, `docker` without a file); one to files alone leaves none, as
  BuildKit's leave none. `--push` and `type=registry` push each name.
- **Refused before the build** where BuildKit refuses when its solve begins, in its words
  (an exporter it has none of, `tar` no bool, `source-date-epoch` no number, `docker`
  into a directory, two OCI directory layouts): BuildKit says so before any step runs
  too, so nothing is built in vain.
- **Times.** The filesystem outputs are written from the build's last snapshot, whose
  times are the kernel's to the nanosecond, before it is put in its layers' form.

Deviation, recorded: a layer shards made is uncompressed (D15's layers), where BuildKit's
are Go's gzip, which no other compressor reproduces byte for byte; an `oci`/`docker`
archive holds the same documents over different layer blobs. A non-directory reached by
two names is a hard link in every kind (fsutil writes a hard-linked symlink as a symlink
to its first name).

Tested on real microVMs (`builds_write_each_output_as_buildkit_exports_it`: six outputs
of one build, the refusals, a merged layout, SOURCE_DATE_EPOCH; the hard-link rule
mutation-checked) and in `shards_archive::receive`'s unit test (merge, mirror, a symlink
refused its escape).

Open: the attestations `image` outputs carry (provenance), `--platform` lists.

### Builds take named contexts (D53)

`--build-context NAME=VALUE` is read as buildx v0.37.1 reads it (ParseContextNames: each
name a reference's familiar form, `:latest` dropped; held to buildx by
`scripts/buildx/generate`) and handed to the planner as buildx's loadInputs hands it to
the frontend: a remote URL or `docker-image://` as it is; a directory as a local of the
context's name (`_context`, `_dockerfile` for those two), keyed by its base name, its own
`.dockerignore` read. The planner applies them as Dockerfile2LLB does (dockerui
NamedContext, dockerfile/1.27.1):

- a stage whose name is a context's is that context, its own steps never run; a base or
  `COPY --from` name a context names (looked up as `NAME::os/arch`, then `NAME`) is that
  context: an image (`[context NAME] REF`, its config's environment, working directory and
  platform, its ONBUILD triggers run), scratch, a Git repository, an HTTP file
  (`context`), or a directory read with only the paths the stage copies from it;
- BuildKit's own quirks kept: `--from=context` reaches a context named `context`, its
  name being normalized before the check that spares `context`; BuildKit's errors word
  for word (`invalid context specifier`, `unsupported context source`).

Held to BuildKit by 14 plan cases its own converter planned through a gateway client that
`scripts/dockerfile/oracle` fakes (images.json's images, contexts with no
`.dockerignore`); one recorded deviation: an SSH Git context carries no host keys scanned
over the network while planning (BuildKit's sshutil.SSHKeyScan), the fetch verifies them.
Also held to buildx: a `local` or `tar` output refused beside `--iidfile`. Tested on a
real microVM (`named_contexts_stand_in_for_what_they_name`); the platform-keyed lookup
mutation-checked.

`oci-layout://PATH[:TAG][@DIGEST]` contexts are read as buildx's ocilayout.Parse reads
them, the digest found as its resolveDigest finds it (the index entry named the tag, by
image name then reference name, else the only entry), and planned as BuildKit plans them
(`oci-layout://` over a stand-in of the context's name and that digest, `oci.store` the
layout's store; 4 more plan cases). Where buildx serves the layout to BuildKit as a session
content store, shards takes the image's blobs into its own store, each checked against its
digest as it is stored, the platform's manifest chosen from an index, so a layout's image
is built on as any base is (`oci_layout_contexts_are_the_images_they_hold`). buildx locks
the index to read it, leaving a lock file in the layout; shards only reads.

### Skills checked as the Agent Skills reference checks them (D54)

`SKILL` takes skills in the Agent Skills format (AGENTFILE_ARCH.md §8 Q12): a directory
whose `SKILL.md` opens with YAML frontmatter, `name` and `description` required. Each is
checked as the format's reference validator checks it (agentskills/skills-ref
`validate`, 69ef37e; `shards_build::skill`), held to it by `crates/build/tests/skills.rs`
over 131 skills, every error and every parsed frontmatter alike, as
`scripts/skills/generate` records skills-ref (on Python 3.14.3, from its own lockfile)
making of them:

- the 14 Apache-2.0 skills of anthropics/skills (683bc88), committed with their licence;
  skills-ref refuses one of them (`claude-api`, a 1068-character description), and so
  does shards;
- 117 crafted ones, for each rule: names (NFKC-normalized and stripped, Python's
  `str.isalnum` taken from the same Python, lowercase, hyphens, length in code points,
  the directory's name), descriptions, compatibility, unknown fields, and the YAML.

The frontmatter is read as strictyaml reads it, by a parser of its subset written for
this: block style only, every scalar a string, a document that is no collection its raw
text; flow style, anchors, aliases, tags, merge keys, repeated keys, tabs outside quoted
and block values, and `-`, `?` or `:` before a space where a value starts all refused.
Two of its found quirks kept: a value of the key's indentation on the next line is an
error, and NEL, LINE and PARAGRAPH SEPARATOR fold within a value without ending its line.
Deviation, recorded in the test: where the YAML is refused, shards says what is wrong and
on which frontmatter line after skills-ref's own prefix; strictyaml's texts (ruamel's,
naming `<unicode string>`) are not reproduced. Mutation-checked: a description limit
moved, and a value's `: ` let through, each fail the corpus.

**What the directives lay out.** `AGENT`, `HARNESS`, `MCP` and `SKILL` build (no longer
refused by name), each a layer of its own (`--link`, §7 Q19), so a domain's files are its
directive's alone:

- **Sources** (§12.2) are told apart as written: a Git URL (as BuildKit's git contexts
  read one) is cloned; an http(s) URL is downloaded and unpacked (an agent served over
  HTTP is an archive); a path (`.`, `./`, `../`, `/`) is taken from the build context as
  `ADD` takes one, a local archive unpacked; anything else is an OCI reference, an OSI
  artifact, which shards does not fetch yet (refused with what to name instead). A path
  ending in a `:tag` is refused: a version is an OCI reference's.
- **Where each goes** (§12.1): an agent at `/agents/<name>`, a harness at
  `/harness/<name>` (or `TO`'s path); an MCP server over stdio at `/mcp/<name>`, or with
  `FOR` at each grantee's `/agents/<name>.d/mcp/<server>` (`/harness/<name>.d/…` for a
  harness, by `--target-kind` or what declares the name); a remote one (an http(s) URL)
  fetched not at all, the normalized Agentfile carrying it.
- **Skills**: the source taken as `ADD` takes one (`--from`, `--checksum`,
  `--keep-git-dir`, `--exclude`), then a step of shards' own: each skill it holds checked
  as the reference validator checks it (above), and laid out in a directory of its name.
  A tree is one skill when its root holds `SKILL.md`, its name to match the directory it
  came as (a path's, a Git repository's or subdirectory's; none for a URL or heredoc);
  one Markdown file is a skill's `SKILL.md`; otherwise each entry at its root must be a
  skill. Anything else fails the build, each skill and what is wrong with it named. The
  skills then go to `/skills/` for every agent (shards' choice: one copy beside `/mcp/`,
  which §4.3 leaves open), to each grantee's `.d/skills/` with `FOR`, or to a destination
  given without `FOR`; a destination with `FOR` is refused, for a grant writes into its
  grantee's domain alone.
- `SKILL --from=<agent>` (§12.12) is refused until OSI agent configs, which list an
  agent's skills, are fetched.

Tested on a real microVM (`agentfile_directives_lay_out_what_they_bring`): an agent's
directory, a harness's archive unpacked, an MCP server granted to one agent and a remote
one not fetched, a skill granted to one agent and a directory of shared skills; and a
skill the reference refuses failing the build in its words.

**OSI artifacts.** Agents, harnesses and MCP servers are OCI 1.1 artifacts of their own
types (§8 Q1), and their content and config are as §12.17 proposes
(`shards_image::osi`): `artifactType` `application/vnd.osi.{agent,harness,mcp}.v1`, a
config `application/vnd.osi.*.config.v1+json` (schema version 1; a reader refuses another
version, unknown fields, a name no Agentfile could give, and a path that is absolute or
climbs out), and content layers `application/vnd.osi.*.content.v1.tar` (`+gzip` and
`+zstd` read).

- **Made** by `shards build agent DIR -t NAME` (and `harness`, `mcp`, in shards' own
  grammar, `shards agent build …` the group it is said as) of a directory and its
  `agent.json` (`harness.json`, `mcp.json`), which is the config and not content: one
  uncompressed tar, names in order, owned by root, times kept, refused if it holds what
  §9.2 keeps out of any domain (device nodes, FIFOs, sockets, set-ID bits, symlinks whose
  targets leave the directory).
- **Kept, pushed, pulled, listed, inspected, removed** as images are, in the same store
  (`shards push|pull|ls|inspect|rm agent`), pushed by the registry client every image push
  uses.
- **Taken** by `AGENT`, `HARNESS` and `MCP` `FROM` an OCI reference: resolved from the
  store, or pulled (with `--pull`, pulled again), its `artifactType`, config and layer types
  checked against what the directive takes, and its content read through and refused for
  whiteouts, devices, FIFOs, set-ID bits, absolute or climbing names, and links leaving
  the directory, each time a build takes it, however it came to be stored. Its content is
  laid as a layer of its own at the domain's directory and its config at
  `<directory>.d/osi.json`, where the runtime reads how it runs; the source names the
  artifact by digest.

Tested on a real microVM (`agents_are_osi_artifacts_made_pushed_and_taken`): an agent made
and pushed to a registry (its manifest, config and layer types as specified), listed,
removed here, then pulled by an Agentfile's `AGENT` and laid out with its config; a
harness `FROM` an agent refused; a symlink out of the directory refused when made.

### Domains checked at build time (D55)

An image with agents or harnesses is checked before anything of it leaves the build
(AGENTFILE_ARCH.md §9.2), every path placed in its domain: an agent's or harness's
directory and the `.d` grants beside it, or the system. The planner records each domain
where its directive lays it (`TO`'s path, which must now be absolute, as grants and checks
find a domain by it whatever order the file declares things in; grants follow it to
`<dir>.d`), and the build fails, naming each path and why, where:

- a symlink in a domain resolves, inside the image and through any symlinks on the way
  (40 at most, as Linux follows), outside it: absolute or relative, into the system or
  another domain;
- a hard link's names lie in two domains;
- a domain holds a device node, FIFO or socket, or a file with set-ID bits or file
  capabilities (`security.capability`);
- **a step other than a domain's own directives writes in it** (§7 Q19.3): every file
  operation and command of the target's lineage is marked, and each domain's own steps
  with the destination they write; each marked step's new layers are read entry by entry,
  a removal (a whiteout) counted as a write, and a path in a domain not the step's own
  fails it, naming the file. A step the cache answers is checked as one that ran.

Found while testing: a directive's path source (`AGENT … FROM ./agent`, `SKILL ./x`) did
not count among the context's paths, so a build that also `COPY`ed from the context sent
the context without them; it counts now.

Tested: the rules over trees made in memory (`build::domains` tests: symlinks out by
absolute and relative targets and through another symlink, in by both, a hard link across,
devices, FIFOs, set-ID bits, a grant's symlink), and on real microVMs
(`domains_are_checked_before_an_image_leaves_the_build`,
`only_a_domains_own_directives_write_in_it`: `RUN` writing in an agent's directory after
its `AGENT` and before it, `COPY` into its grants, all refused; `RUN` elsewhere built);
the write rule mutation-checked.

Owners, the last of §9.2's rules, came once the runtime decided its uids (D59): the n-th
domain, agents in the order declared and then harnesses, runs as uid and gid 200000 + n
(`shards_abi::DOMAIN_FIRST_ID`, init and the build reading one constant). A path whose
uid or gid is a domain's user, outside that domain or in another's, fails the build,
naming it (`--chown=200000` on /etc). Tested in memory (system and other domain, by uid
and by gid, an ID past the domains' none's), mutation-checked, and on a real build
(`a_domains_user_owns_nothing_past_its_domain`). Open: §9.8's triggers.

### The extensions expand their words; `COPY --from=<agent>` (D56)

The Agentfile's directives expand `ARG` and `ENV` where `ADD` does (BuildKit's
`dispatch` expands a command's words with the stage's environment before it runs it):
`AGENT`'s, `HARNESS`'s and `MCP`'s sources and `TO`, `SKILL`'s source, destination,
`--chown`, `--chmod` and `--checksum`, and `VOLUME`'s paths and options. Names stay as
written, as a stage's name does (`FROM … AS $x` is not expanded either), so a grant's names
are checked before any build argument is known.

`COPY --from=<agent or harness>` copies the domain's content, its files at the root, into
any stage, without the domain (§7 Q19.1). A domain's name is no stage (one namespace,
Q19.2), so the copy's source is the content state the directive itself copies from: the
context's, git's, http's, or the OSI artifact's. The stage that declares it is made a
dependency of the stage that copies, as `COPY --from=<stage>` makes it, so it is
dispatched first and its source expanded in its own scope (found by the test: expanding
with the global `ARG`s alone gave a declaring stage's `ARG SRC` the empty string). A domain
a later stage declares is refused, as a stage named before it is defined is.

Tested: `agents_expand_their_words_and_are_copied_from` (an `ARG`-named source,
`COPY --from=main` into a stage without the agent, the image holding the file and no
`/agents`), the dependency mutation-checked; BuildKit's oracle unchanged.

### An Agentfile's manifest says what it holds (D57)

The manifest of an Agentfile's image carries annotations (OCI image-spec `manifest.md`,
"annotations": arbitrary metadata, `vnd.`-prefixed keys for vendors) so that a registry's
listing, or the referrers API, finds it without fetching its config:
`vnd.osi.agentfile.digest` (the normalized Agentfile's digest, as the label), and
`vnd.osi.agentfile.agents` and `vnd.osi.agentfile.harnesses`, each the names
comma-separated in name order, left out when empty. `json.MarshalIndent` order is kept
(ocispec.Manifest writes `annotations` after `layers`), and a Dockerfile's manifest is byte
for byte what it was.

Tested: `an_agentfiles_manifest_says_what_it_holds` (the stored manifest's annotations for
an Agentfile with two agents and a harness; none on a Dockerfile's).

### Reach is transitive (D58)

The build computes, over the target's directives, what each agent and harness reaches
(AGENTFILE_ARCH.md §9.5). Edges: a network both join (`CONNECT … ON`), a named volume
granted to both (`VOLUME name … FOR`), `ATTACH`. A domain reaches the world when it joins a
network that is not internal and lets some port cross both its own boundary
(`NETWORK --expose/--ingress/--egress`) and the microVM's (`EXPOSE … FOR` it), either way
(an ingress-only port answers what reaches it, so replies carry data out; §12 answer 6:
a flow crossing both needs both, corrected with D59 part four, which first read either
alone as enough), or an external network (the host's, whose reach the build cannot see), or when a
remote MCP server is granted to it or to every agent. A local MCP server is no edge: each
caller has an instance of its own (§9.6). An internal-only domain (§9.5: one that joins
an internal network, and does not reach the world itself) with a path to one that does
fails the build with the shortest path, read outward:
`agent b -> network back -> agent a -> network world`. A domain on no internal network
holds nothing declared internal, so a harness driving an agent that reaches the world is
no error; one attached to an internal-only agent and to a world-reaching one is (§9.8).
No exception allows such a path: relays and declassifiers were dropped (§12 answer 14,
2026-10-07), the grants being what decides reach. Found by the suite: a first rule that took every domain not reaching the world
as internal-only refused the plain harness-drives-agent Agentfile of D54's test.

Tested: `reach_through_any_declared_edge_is_reach` (paths through an internal network, a
volume, `ATTACH` and a remote MCP server; networks with no port, or internal, reaching
nothing; a local MCP server joining no one; every agent reaching the world refusing none;
a harness driving a world-reaching agent allowed), mutation-checked three ways: a search
that follows no edge, every domain taken as internal-only, and none.

### D60. The in-VM server: an instance for each agent, apart from init

AGENTFILE_ARCH.md §5 and §12 answer 18 (Q17), decided by the user on 2026-10-07 in three
steps: kernel isolation and mutual TLS 1.3 (Noise not added inside it: two sessions
between the same ends, keyed by the same party); code mode as a typed API run in a
sandbox; and, once TLS compiled into init was measured costing every microVM 3.0 MiB of
memory (PM M122), one least-privileged instance for each agent, from a device of its own.

- **Where it lies.** `shards-server` (crates/server), a static musl binary `shards`
  carries as it carries init (build.rs). The daemon writes it, once, into an EROFS image
  of it alone (`guest::server_device`, `$SHARDS_HOME/guest/server-<sha256>.erofs`) and
  attaches that read-only after the root filesystem (`/dev/pmem1`) to microVMs whose
  image is an Agentfile's (`vnd.osi.agentfile.digest`); a template's key names it. init
  makes its own node for the device (the run's `/dev` holds the run's devices alone) and
  mounts it with DAX beside the run's writable layer, where no path from the run's root
  reaches.
- **An instance for each domain**, started before it: init forks, gives the child
  network, IPC and UTS namespaces of its own, the domain's group as its only supplementary
  group (to give its files to), its own uid and gid (`shards_abi::SERVER_FIRST_ID` plus
  n: past every domain's, there being no more domains than a kernel's PIDs), no
  capability, bounding set included, and `no_new_privs`, and execs the binary from the
  device (`execveat`). The instance makes its own CA, its certificate, and its domain's
  certificate and key in a directory of its own, listens there, says so to init, and then
  loads the seccomp filter the domains with no processes run under, before it reads
  anything an agent sends. init starts the domain only then, with that directory mounted
  read-only at `/run/shards`.
- **Who calls.** The socket a connection is accepted on, which only its domain sees, and
  over it mutual TLS 1.3 with the certificate issued for that domain alone: no key is
  shared between agents, and init holds none. No agent shares an instance, a queue or a
  process with another's, so none can time or stall another through it.
- **Its C.** AWS-LC is compiled for the guest by zig 0.16.0 on every host
  (`scripts/zig-cc`, `scripts/install-zig` in CI), its paths remapped
  (`-ffile-prefix-map`) as the Rust's are, so that what `shards` carries is the same
  wherever it is built.

Tested: `an_instance_answers_its_own_caller_alone` (in a microVM: its own caller's
certificate answered, another instance's and none refused; requests past their bounds
refused), and on real microVMs `the_in_vm_server_knows_each_agent`: x and y each ask
their instance, over MCP, who they are, and are told; each instance runs as its own uid
and gid with its agent's group alone beside them, no capability, `no_new_privs`, seccomp
mode 2, and a network namespace of `lo` alone. Mutation-checked: without its uid it ran
as root; without the groups, the bounding set (`000001ffffffffff`), `no_new_privs`, its
network namespace (`lo,eth0`) or its own seccomp filter (mode 0), the test fails.

**Messages, as granted.** init derives who may message whom from the Agentfile
(`netplan::channels`): each `CONNECT`'s agents send requests to those after `TO`, both
ways with `WITH`, and each `ATTACH`'s harnesses to its agents; the receiver answers. For
each two domains so joined it makes one `SOCK_SEQPACKET` socket pair, and gives each
instance its end, and nothing else joins two instances. An instance offers `peers` (who
its caller may message, and how), `send` (a request, to a peer it may send to),
`receive` (what peers sent and it has not taken) and `answer` (to a request taken, by
its ID). Each instance holds the grant too, as defence against another instance made to
misbehave: a request from a peer its caller does not answer, and an answer to no request
it sent, are dropped. It holds one unread message a peer; what a peer sends past that
waits in the kernel's socket buffer, whose bound is the kernel's, and a full buffer is
said to the sender (`has not taken what was sent it`). An instance joins its domain's
cgroup before its exec, so that what an agent makes its instance hold counts against the
agent's own memory and ends with it (part nine).

Tested: `a_peer_is_heard_only_as_granted` (in a microVM, forged frames dropped, granted
ones held) and, on real microVMs, `agents_message_one_another_as_granted`: with `CONNECT
a TO b`, a's peers are b (send), b's are a (answer), c's none; a's request reaches b,
b's answer reaches a, b may not send a a request of its own, and c may send no one.
Mutation-checked: requests from a peer not answered, answers to no request, sending to a
peer not sent to, and `TO` taken both ways each fail a test. `ATTACH`'s channels are the
same edges, harness to agent, and are not yet tested on a microVM.

**MCP servers, offered** (§4.4, §12 answer 5: offered, not imposed). init gives each
instance the servers its caller is in scope for: those with no `FOR` to every agent, and
those whose `FOR` names it, harnesses only so. A remote one is given by its URL; one
spoken to over stdio by where it lies in the caller's view (`/mcp/<name>`, or `<its
dir>.d/mcp/<name>` where `FOR` names it) and the command its OSI config gives, which the
caller runs itself, in its own confinement, as §9.6 asks: a server is never a deputy
holding more than its caller. The instance's `mcp` tool answers with them. Tested on real
microVMs: `mcp_servers_are_offered_to_those_in_scope` (a remote server `FOR a` offered
to a alone, a local one to a and b); mutation-checked: with every server in every
scope, b is offered a's.

No run-time labels, relays or declassifiers (§12 answer 14, decided by the user
2026-10-07): every path between domains is a grant, and the build refuses any that joins
an internal-only domain to the world (D58), so there is no data to label. Code mode
waits on the user.

### D72. `--attest`, `--provenance` and `--sbom`

The flags as buildx v0.37.1 reads them (util/buildflags/attests.go: the shorthands
canonicalized, `ParseAttests`, `ToMap`, each type's first; held to buildx by the buildx
oracle, `ATTEST` lines, with an empty value none at all as buildx's flag leaves it), read
before the named contexts as toBuildOptions reads them, and the provenance's attributes
as BuildKit takes them (NewProvenanceCreator): `--provenance=false` (or
`disabled=true`) turns it off, even where Docker would attest by default; an explicit
one attests a stored or pushed image with `builder-id` as the builder's ID and
`reproducible` as `buildkit_reproducible`, `mode=min` and `version=v1` as given; a bad
`mode`, `version` or `reproducible` refused in BuildKit's words.

Refused, named, until each is made and held to BuildKit's (no build is given less than it
asked for in silence): `mode=max` (its build config is LLB's protobuf definition and
digests), `version=v0.2`, SBOM attestations (a scanner image run over the image), other
types, and an explicit provenance in a docker or tar output (not recorded yet).

An explicit provenance (not `inline-only`) goes in every output, as BuildKit puts it
(measured: `oci-named`, `local-output`): an OCI layout, tar or directory, holds the
attestation and names the index (no platform of its own, the names' annotations on it),
its statement's subjects the image's names, and the metadata file names that index with
its provenance; a local output gets `provenance.json`, the statement indented, naming each
regular file it holds by its path and SHA-256.

Tested: the buildx oracle (shorthands, booleans, duplicates, quoting, errors);
`builds_attest_their_provenance_as_docker_does` (builder ID, reproducible, off, the
refusals).

### D71. Provenance, as Docker attests a build by default

A build whose image is stored or pushed carries its SLSA provenance, as `docker build`
does with Docker 29.3.1 (its BuildKit v0.28.1; buildx asks for
`attest:provenance=mode=min,inline-only=true` unless BUILDX_NO_DEFAULT_ATTESTATIONS is
true, so an image in an OCI or docker archive carries none, as there):

- **The statement** (in-toto Statement v0.1, SLSA provenance v1, `mode=min`): the
  request as buildx sends it (`buildx_attrs`: the frontend's options for every flag, its
  host's filtered out as FilterArgs does, build arguments and labels then dropped and the
  request said incomplete, as min does; the Dockerfile's name as the config source's path;
  the locals, named contexts' too); the materials, sorted as Capture.Sort sorts them: base
  images as package URLs (purl.RefToPURL: the familiar name's path, its tag, or its digest
  as a qualifier where it has no tag, its platform, a registry's port escaped) with the
  digest each resolved to, Git sources (the remote, `#` and the ref, its commit, `sha1`
  or `sha256`), HTTP sources (their SHA-256), each URL's credentials masked as
  RedactCredentials masks them; the builder's platform; the run's invocation (the build's
  reference), its start and end; completeness; and, for an image with names, a subject
  for each (its purl and the manifest's digest).
- **The documents**: the statement as an `application/vnd.in-toto+json` layer of an
  attestation manifest whose config is an image config of no platform with the
  statement's digest as its one diff ID, and the index of the image's manifest (its
  platform) and the attestation's (`unknown/unknown`, the reference annotations), each
  written as BuildKit's exporter writes it.
- **What names it**: the index is what the image's names resolve to and its ID
  (`image inspect`, `-q`, `--iidfile`, a dangling build's record), what `--push` pushes
  after its manifests, and what the metadata file's `containerimage.descriptor` and
  `.digest` name, beside `buildx.build.provenance`, the v0.2 form buildx writes there
  (every argument, the secrets and SSH agents mounted, no run metadata).

Each build's index is its own (its invocation and times are), as Docker's is: two
identical builds have one manifest and two IDs, the manifest's being the content's.

The flags are D72's. A local named context's shared key carries no node identifier
(buildx appends its own; shards has one builder).

Tested: `provenance_is_buildkits`, against what `scripts/provenance/generate` records
Docker's own builds making in `shards-dind` (18 builds: no flag to an OCI layout, min,
build arguments, labels and every frontend option, secret and SSH mounts, base images of
Docker Hub, another registry, a registry with a port, by digest and untagged, HTTP and Git
sources, a named context, `--provenance=false`, `--attest`, and a build stored with no
flag): each statement, attestation config and manifest, and index byte for byte given
what the builder alone decides (its time, its IDs, what it resolved), the metadata file's
provenance, and a stored build's whole metadata file. `images_are_package_urls_as_buildkit_writes_them`,
`credentials_are_redacted_as_buildkit_redacts_them`. On a real build
(`builds_attest_their_provenance_as_docker_does`, mutation-checked: no default, no
materials): the ID, `image inspect` and the metadata file name the index; the statement
names the image and its base; mode=min's request; the metadata file's provenance; no
attestation asked for, the manifest is the ID. `builds_push_what_they_build` follows the
pushed index to the image and the statement.

### D70. `--call`'s subrequests: outline, targets, describe

`build --call=outline|targets|subrequests.describe[,format=json]`, answered as BuildKit's
Dockerfile frontend answers each (dockerfile/1.27.1: dockerui's HandleSubrequest,
Dockerfile2Outline, ListTargets, describe) and printed as buildx v0.37.1 prints the
answer (commands/build.go printValue): `result.json`, as Go's `json.MarshalIndent` writes
it, and a newline, for `format=json`; otherwise the frontend's printer of it
(`PrintOutline`, `PrintTargets`, `PrintDescribe`, through Go's text/tabwriter, padding and
all). Nothing is built.

- **Outline**: the plan, made as a build's is up to its steps (bases resolved, the
  file's checks applied: a violation under `error=true` fails it, as it fails BuildKit's),
  then the target's arguments, secrets and SSH agents and those of the stages it stands
  on and reads: an argument its FROM or an `ARG` uses, and those its default names
  (markAllUsed), each with its doc comment, value and place; a secret's ID as
  dispatchSecret makes it (`target`'s base name where none is given); `default` for an
  SSH mount without an ID.
- **Targets**: every stage as written, its doc comment, its base and platform
  unexpanded, the last the default.

Better than BuildKit's:

- **The order within a line.** BuildKit sorts by line alone (`sort.Slice`, not stable)
  over a map's order, so two arguments of one `ARG`, or two mounts of one `RUN`, come in
  any order from one call to the next; shards gives them in the order they are written.
- **ListTargets with a `check=` comment.** BuildKit's ListTargets parses with no linter,
  which a stage's `# check=` comment dereferences: its frontend panics (measured, recorded
  in the oracle). shards lists the targets.
- A misspelt official image's suggestion names `docker.io/library/<name>` (as the plan's,
  `testdata/deviations.json`).

Tested: `subrequests_are_buildkits`, every plan of the corpus (114, with outline-specific
cases: doc comments, arguments naming others, secrets and SSH mounts across dependent
stages, a named target, platforms) held byte for byte to what `scripts/dockerfile/generate`
records BuildKit answering, JSON and text, and the subrequests described;
mutation-checked (dependencies' arguments, arguments named by defaults, doc comments). On
a real build (`call_answers_the_frontends_subrequests`): each subrequest, text and JSON,
nothing built.

### D69. Git over SSH

`ADD ssh://…` and `ADD git@host:path` (any URL git takes for SSH: `ssh://`,
`git+ssh://`, scp's form; connect.c), fetched as git fetches over `ssh`: `git-upload-pack
'PATH'` run on the host (the path quoted as git's `sq_quote_buf` quotes it),
`GIT_PROTOCOL=version=2` sent as git sends it (`SendEnv`), protocol v2 over the session,
submodules over SSH too. The agent is the one BuildKit mounts for the source
(`git.mountsshsock`, `default`): a socket forwarded or key files served (D51, D68), "no SSH
key "default" forwarded from the client" where the build has none. No `ssh` binary is run
or needed: shards is its own client (`build/ssh.rs`), on AWS-LC:

- **Transport** (RFC 4253): key exchange by `mlkem768x25519-sha256` (ML-KEM-768 with
  X25519, draft-ietf-sshm-mlkem-hybrid-kex, OpenSSH 10.0's default), then
  `curve25519-sha256` (RFC 8731); the host's signature verified (Ed25519, ECDSA P-256,
  P-384, P-521, RSA 2048–8192 with SHA-2 alone); AEAD ciphers alone,
  `chacha20-poly1305@openssh.com`, `aes256-gcm@openssh.com`, `aes128-gcm@openssh.com`
  (RFC 5647); strict key exchange (OpenSSH PROTOCOL §1.10), which closes Terrapin
  (CVE-2023-48795): a strict server's packets before its KEXINIT are refused, and sequence
  numbers start again at each NEWKEYS; rekeying when the server asks, its host key the
  same.
- **Authentication** (RFC 4252 §7): `publickey`, each of the agent's keys in turn,
  signed by the agent; RSA keys with `rsa-sha2-512` or `-256` (RFC 8332), as the
  server's `server-sig-algs` (RFC 8308) allows.
- **Channels** (RFC 4254): one connection for all of a fetch's requests, a session for
  each, as git's own transport takes a connection for each: the advertisement, then each
  command with its end (EOF) sent, so that upload-pack answers and exits. A command that
  fails gives the server's words (`does not appear to be a git repository`).

Better than BuildKit's:

- **Host keys.** BuildKit scans the host's keys while planning (llb.Git,
  `sshutil.SSHKeyScan`) and trusts whatever it is shown: whoever answers for the host's
  name is trusted. shards trusts only the keys the user's known_hosts files hold, as
  OpenSSH reads them (`~/.ssh/known_hosts`, `known_hosts2`, `/etc/ssh/ssh_known_hosts`,
  `ssh_known_hosts2`; patterns, negation, hashed names, `[host]:port`, `@revoked`), and
  offers the host key algorithms of keys it knows first, as OpenSSH orders them. A host
  not known is refused with its key's fingerprint and how to add it after checking it; a
  key changed, or revoked, is refused, named.
- **Crypto.** Post-quantum key exchange first; no SHA-1, CBC, or non-AEAD cipher is
  offered.

Not taken: host certificates (`@cert-authority`), ssh_config (BuildKit's `ssh` reads none
either), and a server that does not speak protocol v2 over SSH (`AcceptEnv
GIT_PROTOCOL`; GitHub, GitLab and Bitbucket do).

Tested on real builds against OpenSSH's sshd (10.2 here; the CI hosts' own), run
unprivileged by the test (`add_fetches_git_over_ssh`): three servers, each holding one
path (the hybrid exchange, where the host's OpenSSH has it (9.9 and later: here and on
the macOS runners, not Ubuntu 24.04's 9.6, where curve25519 stands in), with
ChaCha20-Poly1305 and an Ed25519 host key; curve25519 with AES-256-GCM, an ECDSA host
key, and rekeying every 256 KiB across a 3 MiB file; curve25519@libssh with AES-128-GCM
and an RSA host key), each repository's submodule over SSH, a key file and an agent's
socket; refused, each in its words: a host not known, a key not taken, a repository
missing, no agent. Mutation-checked: rekeying refused, sequence numbers not restarted,
the host key not checked, a failed command's end or words lost. Units:
`host_signatures_are_verified` (each kind, tampered, mutation-checked),
`strict_key_exchange_refuses_what_comes_before_kexinit` (a scripted server injecting a
packet, mutation-checked), `known_hosts_are_matched_as_openssh_matches_them`,
`urls_name_their_targets_as_git_reads_them`.

### D68. `--ssh` key files

`--ssh ID=FILE[,FILE…]`, as buildx v0.37.1 takes it (vendored BuildKit v0.33.0
session/sshforward/sshprovider/agentprovider.go `toDialer`): each file's first 100 KiB
read and parsed as golang.org/x/crypto v0.55.0's `ssh.ParseRawPrivateKey` parses it (the
first PEM block as Go's `pem.Decode` finds it; OpenSSH's format unencrypted, PKCS#1,
PKCS#8, SEC 1; Ed25519, ECDSA on P-256, P-384 and P-521, RSA), the keys put in a keyring
as x/crypto's `agent.NewKeyring` puts them (in order, a key given twice kept where it was
first), and a step's requests answered as `agent.ServeAgent` answers them: its keys
(comments dropped, as x/crypto drops them), and signatures. The client answers them, over
the relay a forwarded agent's requests take (D51, token-gated): the keys never enter the
microVM. Each refusal in buildx's words: a socket beside keys ("invalid combination of
keys and sockets"), a passphrase ("ssh: this private key is passphrase protected"), a
file that is no key, and x/crypto's own refusals of malformed keys.

Signed by AWS-LC (`aws-lc-rs`, which shards already links for TLS): Ed25519, ECDSA with
the hash RFC 5656 §6.2.1 gives each curve, RSA PKCS#1 v1.5 with SHA-256 or SHA-512 as
RFC 8332 §3.2's flags ask. The OpenSSH format carries no RSA CRT exponents, which AWS-LC
takes: shards works them out (`d mod (p-1)`, bit by bit, the same work whatever the bits),
and AWS-LC checks every part against the others (`RSA_check_key`). Key material lives in
buffers overwritten when dropped.

- **Better than buildx's:** the agent is read-only, as BuildKit's forwarded agent is:
  where buildx serves a key file's keyring as it is, so that a step can add keys to it,
  remove them, or lock it with a passphrase of its own for the steps after it, shards
  refuses each.
- **Refused, where x/crypto takes them:** DSA keys (FIPS 186-5 withdrew DSA; OpenSSH 10.0
  removed it); RSA keys outside 2048–8192 bits (NIST SP 800-131A Rev. 2 allows none
  shorter for signatures; AWS-LC signs none longer); and RSA's SHA-1 signatures
  (`ssh-rsa`, asked for with no flags), which RFC 8332 replaces (§1) and OpenSSH 8.8
  stopped accepting by default; a client that asks for one is refused it.
- Malformed DER inside a PEM block is refused in shards' words, not x509's.

Tested: `key_files_are_served_as_buildx_serves_them`, against what
`scripts/sshkey/generate` records x/crypto making of 35 cases (keys of every kind and
form, Go's PEM skips, malformed, encrypted, a keyring of four with one again): each
refusal word for word, each answer byte for byte (Ed25519's and RSA's signatures are
deterministic), each ECDSA signature verified; mutation-checked (the keyring's order,
mpints, the RSA hash, two PEM skips). `the_key_agent_is_read_only`,
`crt_exponents_are_the_remainders`. On a real build (`run_steps_reach_the_clients_ssh_agent`,
mutation-checked): a step lists the key file's key and has it sign, and is refused the
agent's forgetting; a passphrase and a socket beside keys refused in buildx's words.

### D67. `--check`, `--call` and `--debug`'s warnings

`--check` (`--call=check`, and `check,ignorestatus=true`), as buildx v0.37.1 reads and
answers it (util/buildflags/callfunc.go, commands/build.go `printResult`), over the
checks the plan already makes as BuildKit's frontend does (held by the Dockerfile
oracle): nothing built; "Check complete, N warnings have been found!" and each warning as
BuildKit's lint subrequest prints it (frontend/subrequests/lint `PrintTo`: by line, its
rule and URL, its message, its lines as `errdefs.Source` shows them), or "Check complete,
no warnings found."; exit 1 for warnings, unless `ignorestatus`. With `--debug`, a build's
warnings come in full, as buildx's `printWarnings` gives them at debug level: each
rule's description, "More info:" and its lines, and no "use --debug to expand".
`--call=outline`, `targets` and `subrequests.describe` are D70's; `check` with
`format=json` is refused, named, until its LintResults (which carry the Dockerfile's
source map and definition) are held to BuildKit's. The hidden `--print` is taken as buildx
takes it.

Tested on a real build (`check_says_the_builds_warnings_as_buildx_does`); the flags by
the buildx oracle (a flag whose value prints as nothing, as buildx's `callAlias` does,
now said so: `Flag::unshown`).

### D66. `--annotation`

As buildx v0.37.1 reads it (util/buildflags/export.go `ParseAnnotations`, held to buildx
by the buildx oracle) and BuildKit's image exporter applies it to an image of one platform
(exporter/containerimage/writer.go, annotations.go): `key=value` and `manifest:` into the
manifest, `manifest-descriptor:` onto the descriptor that names it (an OCI layout's
`index.json`, the metadata file's `containerimage.descriptor`), before the exporter's own
(when it was made, its names); an output's `annotation…` attributes (`-o
type=oci,annotation.KEY=…`) as well; `index:` and `index-descriptor:` refused, "index
annotations not supported for single platform export". An Agentfile's own annotations
(D57) follow, so that none asked for replaces its digest.

- **Better than BuildKit's:** an annotation for a platform (`manifest[linux/arm64]:`)
  applies where it is the image's platform, which BuildKit's single-platform export drops
  without a word (`Platform(nil)`), and one for another is refused in the words BuildKit
  refuses a platform the build lacks ("invalid annotation: no platform … found in source").

Tested on a real build (`annotations_land_where_buildkit_puts_them`, mutation-checked:
without the manifest's annotations, it fails).

### D65. `builder prune`

`shards builder prune` and `shards buildx prune`, as buildx v0.37.1's prune
(commands/prune.go), which `docker builder` runs where buildx is installed: its flags,
help and errors held to buildx's own by the buildx oracle (now running `prune` as well as
`build`, each case with `DEBUG` cleared, which an earlier `--debug` had left set for the
rest); its warning and `[y/N]` before it removes anything, unless `-f`; and what it
removes as BuildKit's cache manager chooses (cache/manager.go): the records (D50) no
filter keeps, `until` (or `unused-for`) an age a record's last use must reach, `id`
matched as a pattern, the rest by field (`type`, `shared`, `private`, ...); a record whose
layer an image holds is shared, and stays unless `--all`, as BuildKit keeps one an image
shares; with `--reserved-space` (`--keep-storage`), `--max-used-space` or
`--min-free-space`, the least recently and least often used first, one at a time, until
what stays is what `calculateKeepBytes` allows. Said as buildx says it: a table of what
went (or each in full with `--verbose`), padded with tabs as Go's text/tabwriter pads it
with buildx's settings (checked against Go's own), then the total. A record's `Created
at` is written in UTC: shards keeps its second.

Tested: `builder_prune_removes_the_build_cache_as_buildx_does` on real builds (mutation-
checked: with shared records not kept, an image's record went), the oracle's cases, and
the filters, keep bytes, order and table as unit tests.

### D64. What the build's flags give each RUN

`--add-host`, `--shm-size`, `--cgroup-parent`, `--network`, `--resource` and the legacy
`--memory`, `--memory-swap`, `--cpu-shares`, `--cpu-period`, `--cpu-quota`,
`--cpuset-cpus` and `--cpuset-mems`, as buildx v0.37.1 sends them (build/opt.go,
build/utils.go `toBuildkitExtraHosts`, `ParseResourceLimits`), dockerui reads them
(frontend/dockerui/attr.go, ported in `shards_dockerfile::dockerui` with its errors) and
Dockerfile2LLB applies them (convert.go `dispatchRun`): each RUN's hosts, its `/dev/shm`
tmpfs, its cgroup's parent and its op's `linux_resources`, the stage's next state keeping
none of them; `--network` the network each stage's steps have unless one says otherwise,
`host` granting itself `network.host` as buildx's does. `host-gateway` is the builder's
gateway on the bridge it is elected on; reaching the host stays refused (D31).

- **Limits, as Docker's runc applies them.** BuildKit puts them in the OCI spec, and runc
  converts them for cgroup v2. Docker 29.3.1 ships runc v1.3.4 (moby `Dockerfile`
  `RUNC_VERSION`), whose opencontainers/cgroups v0.0.4 writes `memory.swap.max` (the swap
  limit less the memory's, `max` for -1, a missing file ignored for `max` or `0`),
  `memory.max`, `cpu.weight` (`ConvertCPUSharesToCgroupV2Value`, a quadratic of the shares'
  logarithm), `cpu.max` (the quota, or `max`, and the period, 100000 by default) and the
  cpusets (`build::exec::cgroup_files`). The builder's init starts such a step with
  `clone3` into a cgroup of its own under `steps/`, its namespace rooted there, so that the
  step sees its own limits as a container does, and removes it once the step is reaped.
  `cpu.weight` is held to Go's own answers for every share value (`cpu_weights_are_runcs`,
  `testdata/cpu-weight.txt`, made by runc's function run in Go 1.26.1): Go's `math.Pow` is
  its own, and the platform's libm may not agree, so each CI target checks its own.
- **The classic builder's flags**, which buildx takes and BuildKit ignores: `--rm`,
  `--force-rm` and `--compress` silently, `--isolation`, `--security-opt` and `--squash`
  with buildx's warnings, as its logrus formatter writes them.
- **`--cgroup-parent`** is carried in the plan as BuildKit's; the builder microVM has no
  host's cgroups to place steps under, so each limited step's cgroup is the builder's own.

Tested on real builds (`run_steps_take_the_builds_hosts_shm_and_limits`): a step sees
`--add-host`'s name, a 32 MiB `/dev/shm`, `memory.max` and `cpu.max` as runc writes them;
past `--memory` it is ended (137) and the build fails in BuildKit's words; without, it
runs. Mutation-checked: without the step's cgroup files, and without the hosts in the
plan, it fails. The flags are held to buildx's command line by the buildx oracle;
dockerui's parsing by `options_read_as_dockerui_reads_them`. The plan of these options
is held to BuildKit's own by the Dockerfile oracle (`corpus/plan/frontend-run.Dockerfile`:
hosts, `/dev/shm`, the cgroup parent, the network mode and the limits on every step).

### D63. `--metadata-file`

What buildx v0.37.1 writes (commands/build.go `decodeExporterResponse`,
`writeMetadataFile`): the exporter's response, decoded, with the build's reference, as
Go's `MarshalIndent` writes the map (keys sorted, two spaces), whole or not at all. For
an image, what Docker's exporter answers with its containerd store, which names an image
by its manifest's (or index's) digest, as shards' store does (measured below):
`containerimage.descriptor`, `containerimage.digest` and `image.name`, each tag in full,
and no `containerimage.config.digest`; for files (`local`, `tar`), none.

Found with it, and fixed: `build -q`, `--iidfile` and the `writing image` line gave the
config's digest, which moby's classic store names an image by (BuildKit's image exporter
answers it, and buildx's `getImageID` prefers it), while shards' store names it by its
manifest's digest: `shards run $(shards build -q .)` found no such image. Now each gives
the manifest's digest, as Docker with its containerd store gives its image's
(`a_build_given_no_name_is_kept_dangling` runs the image by the ID `-q` printed). `buildx.build.ref` is
`shards/shards/` and an ID made as BuildKit's `identity.NewID` makes one.

Measured beside it (Docker 29.3.1, containerd store, `docker:29.3.1-dind`, 2026-10-07):
Docker's adds `buildx.build.provenance`, the provenance attestation it records by
default, and names an index of the image and that attestation. So does shards now
(D71); with BUILDX_NO_DEFAULT_ATTESTATIONS it names the image's manifest.

Tested: `metadata_is_written_as_buildx_writes_it`, `build_refs_are_buildkits_ids`, and
on a real build (`builds_take_steps_from_caches_written_elsewhere`): the digest is the
image's ID, the name the tag in full.

### D62. Build caches kept elsewhere, and stages built without the cache

`--cache-to`, `--cache-from` and `--no-cache-filter`, as buildx v0.37.1 takes them
(util/buildflags/cache.go `ParseCacheEntry`, build/opt.go `CreateCaches`), and as
BuildKit dockerfile/1.27.1 serves them (client/solve.go `parseCacheOptions`,
control/control.go, cache/remotecache, solver/llbsolver/bridge.go).

- **No driver to switch.** buildx refuses every cache export but `inline` on a `docker`
  driver whose engine keeps images in its classic store ("Cache export is not supported
  for the docker driver", build/opt.go `notSupported`); with the containerd image store
  (Docker 29's default for a new engine) it exports them (measured: Docker 29.3.1 in
  `docker:29.3.1-dind`, `--cache-to type=local` written, 2026-10-07). shards writes
  `type=local` and `type=registry` with any store.
- **What is written.** The build cache's records (D50) that the build used or made, by
  their keys: `mode=max` every one, `min` (the default, and what an unknown mode is, as
  `parseCacheExportMode` has it) those whose layers the image holds, as BuildKit's `min`
  keeps to the image's chain. A cache is an OCI image manifest whose config, of type
  `application/vnd.shards.buildcache.config.v1+json`, holds each record's body by key, and
  whose layers are every layer a record names (`build/remote.rs`). A directory holds it as
  an OCI layout found by its `index.json`'s `org.opencontainers.image.ref.name` (`tag`,
  `latest` by default; `reset=true` empties it first), as BuildKit's client finds its
  own; a registry by its reference. `type=inline`, or the build argument
  `BUILDKIT_INLINE_CACHE` as buildx reads it, puts the image's own records in the image's
  config, field `vnd.shards.buildcache.v1`, the config's other bytes as BuildKit writes
  them. The progress is BuildKit's: `exporting cache to client directory` or `to
  registry`, `preparing build cache for export`, `writing layer`, `writing config`,
  `writing cache image manifest`.
- **What is read.** Every `--cache-from`: a directory's, a registry's, or an image's
  inline records. A step that misses in the store takes an imported record, its layers
  fetched only then, and is `CACHED`. A cache that cannot be read is skipped, as BuildKit
  skips one (bridge.go: its `importing cache manifest from` vertex fails, the build goes
  on); a layer that cannot be had leaves its step to run.
- **Not BuildKit's records.** shards' keys are not BuildKit's (D50 keys definitions and
  inputs as shards plans them), so neither can use the other's caches. None is passed off
  as BuildKit's (`application/vnd.buildkit.cacheconfig.v0`): a BuildKit cache given to
  shards, or shards' to BuildKit, is read as no cache, which costs a rebuild and is never
  wrong.
- **Refused as BuildKit refuses** before it builds: a directory without `dest`, a registry
  without `ref`, an unknown backend (`unknown cache exporter: "x"`); and, for now, the
  `gha`, `s3` and `azblob` backends, which shards does not write yet. A failed export
  fails the build unless `ignore-error=true`. A `gha` entry without its token and URL is
  dropped, as buildx's `isActive` drops it.
- **`--no-cache-filter`** is the frontend's `no-cache` option, as buildx sends it
  (`--no-cache` sends it empty, every stage): dockerui's `IsNoCache` by stage name, any
  case, marks each `RUN` and `COPY`/`ADD` of those stages, and `--link`'s merge, with
  `IgnoreCache` (convert.go, convert_copy.go). The plan is held to BuildKit's own by
  the Dockerfile oracle (`corpus/plan/no-cache-*.Dockerfile`). The solver runs such a step
  without asking the cache, and keeps its result, as BuildKit's does; `IgnoreCache` is
  metadata, no part of a key.

Tested on real builds (`builds_take_steps_from_caches_written_elsewhere`), each guard
mutation-checked: a two-stage build writes its cache to a directory (`max`) and a
registry (`min`); fresh homes take every step from the directory, and from the registry
the final stage's steps, the build stage's running again; an image pushed with
`type=inline` gives its own steps to a third home; every image's layers the same.
`--no-cache-filter build` runs that stage's step alone. Without taking imported records,
nothing was cached; with `min` taking every record, the build stage's step was; without
inline records, the image gave none; with the solver asking the cache for `IgnoreCache`
steps, the filtered stage's step was cached.

### D61. A table for each agent: its gate

Decided by the user on 2026-10-07 ("A table per agent"), after D59 part nine's record of
flows through the switch proved wrong for flows conntrack may not evict (below). One
switch tracked every agent's flows in one table, of `nf_conntrack_max` entries (2048 in a
237 MB microVM), counted per network namespace (net/netfilter/nf_conntrack_core.c,
`__nf_conntrack_alloc`). A full table evicts only entries not yet assured (`early_drop`).
So one agent holding connections fills it, and every other agent's new flows fail.

- **A gate for each linked domain.** A network namespace of its own between the domain
  and the switch: `in0` to the domain (its gateways' addresses), `out0` to the switch
  (an address of its own on the transit link, 169.254.128.0/17, `transit_gate`). It has
  forwarding on, strict reverse-path filtering (RFC 3704 §2.2), and its own conntrack
  table. The switch, holding `169.254.78.1` on every `d<n>`, routes each domain's
  addresses through that domain's gate. Each gate needs a transit address of its own: the
  switch's reverse-path filter drops ARP from one address seen on two links.
- **What the domain opens is tracked in its gate alone.** Its `forward` chain accepts the
  domain's new flows as its grants allow, and their answers by conntrack. Its `raw` chain
  (priority −300, before tracking) gives what others open to the domain, and the domain's
  answers to them, `notrack`. A peer opening many flows takes none of the receiver's
  table. Those flows are accepted by their ports, from the peers granted them.
- **The switch keeps no state for agents.** Its rules are stateless, both ways of each
  grant; it has no conntrack expression, so the kernel tracks nothing it forwards.
- **The stateful point for what leaves the microVM is init.** The gates and the switch let
  a domain's answers out by their ports alone. A domain sending *from* a port it is
  reached on, to anything, would pass them. Before D61 the switch's conntrack refused
  that; now init does. Its `forward` from `agents0` to eth0 accepts answers, each
  domain's new flows from its own addresses on its egress grants, and the agents'
  resolver's questions; its `in` drops all but answers from `agents0`. Between peers,
  the opener's gate holds the state, so to it a forged answer is a new flow, and it is
  refused.
- **A domain's own connections take no port it accepts** (`local_ports`, its
  `ip_local_port_range`): their answers would otherwise arrive on a port its gate leaves
  untracked, and be dropped.
- **A gate serves nothing.** Its `input` and `output` drop: a probe of a domain's
  gateway is dropped, not refused.

Measured on this host (MacBook arm64, macOS 26.4.1, from 915f67b), n = 1 each, on real
microVMs (`an_agents_assured_flows_take_no_others`): a, granted TCP 7002 to x, opens
connections for 10 s and holds them; c, after 5 s, makes 50 connections to b and 50 to x,
each given 3 s, past a SYN's first retransmission.

| build | a's flows (table) | c to b | c to x |
| --- | --- | --- | --- |
| 915f67b (one switch, one table) | 2108 (2048) | 46 of 50, 16118 ms | 50 of 50 |
| D61 | 2177 (2048) | 50 of 50, 0 ms | 50 of 50, 0 ms |
| D61 without the gates' `notrack` | 2075 (2048) | 50 of 50 | 45 of 50, 16054 ms |

Tested, each guard mutation-checked:

- `an_agents_assured_flows_take_no_others`, the table above.
- `an_agent_reaches_nothing_past_its_grants`, now with d given egress (7400) and ingress
  (7300/udp) on a network of its own, asking every address from 7300. Without init's
  `in` drop, the run's own command answered it. Without the gate's `input` drop, its
  gateway refused d's connection (`ECONNREFUSED`), an answer.
- `agents_reach_past_the_microvm_what_their_networks_grant`, now with a, given ingress
  7300/udp, asking from 7300 a host port only e's network grants: e is answered, a is
  not. With init's egress rules matching no source, a was answered.
- `an_agents_own_flows_take_no_port_it_accepts`: c, which b may open 32768 to 60999
  to, reaches x. Without `local_ports`, c's connection timed out.

Open: packet-rate fairness. A flood's packets themselves can delay another agent's:
`an_agents_flood_of_flows_takes_no_others` saw, on x86_64 CI, c make 199 of 200
connections with 1 s each while a sent 1,068,575 datagrams, the one SYN lost recovered by
TCP's retransmission; the test now gives each connection 3 s, past it, so that it holds
the table to account, which would refuse them all. Per-agent packet rates are not
bounded yet.

Found on the way: a domain's first process took its IDs through musl's `setgroups`,
`setresgid` and `setresuid`. Each changes every thread of a process (`__synccall`) and
finds the others in its thread list unless `gettid()` differs from the caller's recorded
tid (src/thread/synccall.c). A child of init's in a PID namespace of its own is tid 1, as
init is. So with any thread of init's alive at `clone3`, the child looked for threads it
does not have, and failed ("taking its own IDs", `ENOENT`). Seen with a diagnostic thread
in init; none of init's own threads outlives the call that spawns it. The children of
`clone` and `clone3` (domains and build steps) now make the system calls themselves
(`defaults::take_ids`, `defaults::rlimit`).

Next: egress without init's NAT (the network process taking the agents' addresses), and
ingress straight to an agent's address, so that init tracks no agent's flow either.

### Agents run in their domains (D59, part one)

When a microVM runs an image whose normalized Agentfile (`/.agentfile.json`) declares
agents or harnesses, shards-init, as the run's command starts (part three says why not before),
starts each domain whose OSI config (`<dir>.d/osi.json`) has `run` (AGENTFILE_ARCH.md §9.3,
§9.9), with the guest kernel's own primitives and no container runtime. A domain with no
`run` is files alone. Init reads both files with a JSON reader of its own (RFC 8259, 1 MiB
and 32 levels at most, duplicate keys and leading zeros refused) rather than linking a
JSON library into PID 1. Each domain gets:

- **Its own IDs.** uid and gid `200000 + n` for the n-th domain in the Agentfile's order,
  no supplementary groups. Every capability is dropped: the bounding set first, which
  needs `CAP_SETPCAP`; then the permitted and effective sets, which go when no uid stays 0
  (capabilities(7), "Effect of user ID changes on capabilities"); then the inheritable and
  ambient sets. `no_new_privs` is set.
- **Its own cgroup**, `/sys/fs/cgroup/domains/<kind>-<name>`, entered at birth by
  `clone3`'s `CLONE_INTO_CGROUP` (Linux 5.7), so no process of it runs outside it even
  briefly. Its `pids.max` is `--processes`, or else the config's `asks.processes`; `none`
  is 1 until part two's seccomp refuses every way to start a process.
- **Namespaces of its own:** mount, PID (its first process is PID 1 there), IPC, network,
  UTS (its host name `<kind>-<name>`) and cgroup.
- **Its filesystem:**
  - The microVM's system is read-only, `nosuid` and `nodev`, made so by one recursive
    `mount_setattr` (Linux 5.12) after the namespace's mounts are made private. Workloads
    inherit the microVM's OS; that is the runtime model's rule.
  - Its own directory and grants are read-only with the system.
  - Every other domain's directory and `.d` grants are hidden under an empty read-only
    tmpfs of mode 000. `/sys` is hidden the same way.
  - A `/proc` of its PID namespace.
  - A `/dev` holding Docker's six nodes (moby `DefaultLinuxDevices` less the console),
    runc's `fd`/`stdin`/`stdout`/`stderr` links and a `shm` of its own, remounted
    read-only.
  - A scratch `/tmp` of its own, mode 700 and its uid's, lost when it ends.
- **Only a loopback**, brought up; network grants are part three.
- **Its stdio:** stdin is `/dev/null`; stdout and stderr are one pipe that init relays a
  line at a time on the run's **stderr**, each line prefixed `[agent main] `. It goes to
  stderr so that the workload's stdout stays its own, for pipes. Every descriptor past 2
  is closed before exec. Every descriptor init holds is close-on-exec today, which the
  mutation shows: without the close, nothing of init's reaches the agent. The close is
  there so that one descriptor not marked close-on-exec, such as the O_PATH descriptors
  of the image's layers that `diff` keeps, cannot become a way into another domain.
- **Its command:** as its config says. A relative program is its directory's, and a bare
  name is found on Docker's default `PATH` before the clone. After the clone the child
  only calls the kernel: no allocation, as it is the fork of a process that may run
  threads. A failed step writes `shards-init: <step>: errno N` on its output.

The workload's end ends the domains, as it ends every process of the microVM.

Tested: `agents_run_in_their_domains` boots a real microVM. Its agent comes from an OSI
artifact pushed to a registry; beside it is an agent of files alone. The agent reports:

- uid and gid 200000, no groups;
- PID 1, alone in its `/proc`;
- its host name;
- all five capability sets empty, and `no_new_privs`;
- the other agent's directory and `/sys` unreadable (EACCES);
- its `/dev`, and its descriptors (stdio alone);
- its own directory and `/etc` read-only (EROFS), `/tmp` writable;
- only `lo`.

The run's stdout stays empty, and the agent of files alone runs nothing. Each of these
mutations fails the test: hiding no other domain, keeping the bounding set, leaving the
system writable.

Part two, after `no_new_privs`, before its command:

- **Landlock** (Documentation/userspace-api/landlock.rst; ABI 6 at least, which Linux 6.12
  brought and the pinned 6.18 has). A domain refuses to start on a kernel without it,
  rather than run unconfined. The ruleset handles every filesystem right and gives back
  reads and execution everywhere, writes and device ioctls in `/dev`, and everything in
  its `/tmp` and `/dev/shm`. It handles TCP bind and connect and gives neither, as no
  network grant exists yet. Signals and abstract Unix sockets are scoped to the domain.
  It is a second layer under the mounts: it refuses what the mounts allow, such as
  writing its own `/proc/self/comm`, and TCP on its own loopback.
- **seccomp**, compiled by the host with the run's own compiler (D41, held to libseccomp)
  and sent to init in a setup entry (`domains-seccomp=`). The daemon sends it for an image
  carrying the Agentfile label, and init starts no domain without it. Its profile is
  Docker's default compiled for no capability, which leaves bpf and perf_event_open, there
  only on capabilities' lists, refused. Docker's own allow lists already lack io_uring,
  keyctl, add_key, request_key and userfaultfd (moby profiles v0.2.3 `default.json`). Its
  `socket` rules, which refuse only AF_ALG and AF_VSOCK, are replaced by `socket` for
  AF_UNIX, AF_INET and AF_INET6 and `socketpair` for AF_UNIX alone, so netlink, packet
  and every other family is refused (EPERM). Measured by the test: without the filter, a
  domain opens AF_VSOCK, the host's channel (§9.7), and netlink.
- **`kernel.io_uring_disabled`** is not set. The domain filter refuses io_uring already.
  The sysctl is the whole guest's, so all it would add is refusing io_uring to the run's
  own workload where its owner asked for `seccomp=unconfined`, against what they asked.

Tested: the same E2E test, the agent also reporting:

- writing `/proc/self/comm` refused (EACCES);
- TCP bind and connect on its loopback refused (EACCES);
- AF_VSOCK and AF_NETLINK sockets, `io_uring_setup`, `keyctl` and `userfaultfd` refused
  (EPERM);
- Unix and IPv6 sockets made.

Mutation-checked:

- no Landlock: the write and the bind go through, and the connect is refused only for
  want of a listener;
- no filter loaded: vsock and netlink sockets are made;
- the daemon sending none: the run fails, naming the missing filter;
- Docker's unmodified default sent: netlink is allowed.

**`--processes=none`** (§9.9) is a filter of its own, `domains-seccomp-none=`. Its
`pids.max` is left as the microVM's, because pids.max counts threads and such a domain may
start them. The filter refuses `fork` and `vfork`. It allows `clone` only with
`CLONE_THREAD` set and no namespace flag: Docker's own mask 0x7E020000 plus CLONE_THREAD's
bit, `SCMP_CMP_MASKED_EQ`. `clone3`, whose flags a filter cannot read, already fails with
ENOSYS for no capability under Docker's profile, and musl and glibc then fall back to
`clone`. `execve` stays: it starts no process. Tested by a third agent declared
`--processes=none` from the same artifact: its thread starts and its `fork` is refused
(EPERM), while the first agent forks. Mutation-checked: compiling the ordinary domain
filter for it lets the fork through.

Part three, network grants between domains (AGENTFILE_ARCH.md §4.6, §4.7, §9.7):

- **The switch.** Init makes a network namespace of no process's, held by a descriptor
  for as long as the microVM runs. Its forwarding is on. Each domain with a grant gets
  one veth link: `eth0` in its namespace, `d<n>` in the switch's. Each link is made in
  init's namespace, with both ends placed by `IFLA_NET_NS_FD` and their interface indexes
  chosen in advance, so the rules can name links before the links exist. Neither the
  switch nor any link touches the microVM's own namespace, its `eth0` or its workload.
- **Policy by link, never by address** (§9.7), in nf_tables, which init programs over
  nfnetlink in one transaction:
  - an `input` chain that drops everything, so no domain reaches the switch itself;
  - a `forward` chain that drops by default, accepts conntrack's established and related
    traffic, and accepts new traffic from `iif d<x>` to `oif d<y>` for each allowed pair.

  Whatever the protocol, UDP included, nothing else crosses.
- **Who may reach whom** (`netplan`). A network's members are the domains its `CONNECT …
  ON` names. Members reach each other (§4.6), except that `CONNECT x TO y` lets `y` only
  answer `x` (§4.7), unless another directive grants `y` to `x` outright. A domain has one
  link, so a pair any shared network allows is allowed. D58's reach graph reads a shared
  network the same way.
- **Addresses.** A network's subnet is its first IPv4 `--subnet`, or else the first /24
  of 10.244.0.0/16 that overlaps neither eth0's subnet nor a declared one. Its gateway is
  its first `--gateway`, or else `.1`. Members take addresses from `.2` in domain order.
  A domain holds each address as a /32 and routes the subnet through the gateway on its
  link (`RTNH_F_ONLINK`). The switch holds each gateway on each link that uses it, and a
  /32 route to each domain's address.
- **Names.** Each domain's `/etc/hosts` is Docker's lines followed by each member of each
  network it joins. It is written in the domain's scratch and bind-mounted read-only over
  the system's.
- **Landlock follows the grants.** TCP connect is refused unless the domain opens
  connections to someone, and TCP bind unless someone may open connections to it.
- **Ordering.** Domains now start once the run's own files are written, not before: the
  run replaces `/etc/hosts`, and replacing a file that another mount namespace mounts
  over detaches that mount (fs/namespace.c, `__detach_mounts`). The test found this: a
  domain's names resolved, then stopped resolving once the run's file was written.

Tested: `agents_reach_only_what_connect_grants`, four agents on a real microVM:

- `CONNECT a TO b ON back`, `CONNECT d WITH d ON back`, `CONNECT c WITH c ON side`;
- `a` reaches `b` by name;
- `b` finds `a` listening, and its connection times out: the switch drops it, and Landlock
  allows it, since `b` may connect to `d`;
- `d` reaches both;
- `c`, alone on `side`, may neither bind nor connect;
- each agent's `/etc/hosts` holds its network's members alone, and `localhost` resolves.

Mutation-checked:

- a forward chain that accepts by default, and `TO` that restricts nothing: `b` reaches
  `a`;
- Landlock handling no TCP right: `c` binds;
- no `/etc/hosts` of its own: names fail.

The planner has unit tests for pairs, pool allocation clear of eth0, declared subnets and
gateways, and capacity.

Part four, egress past the microVM, by port (AGENTFILE_ARCH.md §4.1, §4.6):

- **What is granted.** Two boundaries, each its own grant, and a flow crossing both needs
  both (AGENTFILE_ARCH.md §12 answer 6). A domain may open flows past the microVM to a
  port where, of a network it joins that is not internal, both open it outward: the
  network's own grant (`--egress`, `--expose`) and the microVM's for that network
  (`EXPOSE … FOR` it, both ways or `AS egress`). Ranges meet where both cover them
  (`agentfile::boundary`, and the guest's planner alike). The first version of this part
  (368663d) took either grant alone as enough, which opened what the decided grammar keeps
  shut; the test now holds a port that only the network grants to the switch's drop. A
  domain's grants let it connect, as far as Landlock is concerned. The build records the union of all domains' grants in the
  image's label `vnd.osi.agentfile.egress` (`443,53/udp,8000-8010`), as it records the
  Agentfile's digest.
- **The host holds the microVM to the union.** A run of a labelled image hands its VM's
  network process `NET_POLICY` before the VM has the run (warm VMs start before their run
  is known). That makes the policy `Ports`: what `AllowAll` reaches, on those ports and
  protocols alone, and never the host itself, its loopback, link-local addresses,
  multicast or broadcast. Other ports are refused at once, a TCP reset or ICMP
  administratively prohibited, as the default deny refuses them. A VM whose network
  process does not take the policy goes, as one that does not take its ports.
- **The switch holds each domain to its own grants, by link.** A veth links the switch
  (`up0`) to init's namespace (`agents0`), on a link-local /30 of its own, and the
  switch's default route goes up it. Its forward chain admits `iif d<x> oif up0` only
  for x's protocol and a destination port within one of x's ranges (`meta l4proto`; the
  transport header's port, compared as network-order bytes). Every linked domain's
  default route goes through its first network's gateway, so a domain without grants is
  dropped by the switch, not left without a route.
- **Init's namespace forwards it out** (`ip_forward` on). Its forward chain drops all but
  answers and `agents0 → eth0`, which it marks; its postrouting chain gives what is
  marked eth0's address. That is the address the network process takes frames from, and
  it maps the flows to host sockets.
- **The run's own command reaches no more than it did.** The host now opens the agents'
  ports to the whole microVM, so init's `out` chain drops every new flow leaving eth0
  except to eth0's own subnet (the network's members, D46, and its resolver relay).

Tested: `agents_reach_past_the_microvm_what_their_networks_grant`. A real microVM and two
servers run on the host's address, one port granted to network `out`, one not:

- `a`, on `out`, reaches the granted port, and is dropped on the other;
- `b`, on an internal network that names the granted port too, is dropped, though
  Landlock lets it connect to its peer;
- the run's own command is dropped.

Mutation-checked, each failing the test:

- no port comparison in the switch: `a` reaches the other port, and the host refuses it
  (ECONNREFUSED), which shows its own policy holds;
- no `NET_POLICY` sent: the host's default deny refuses `a`;
- `out`'s drop made an accept: the run's command reaches the host;
- the planner ignoring `internal`: `b` reaches the host.

The host's `Ports` policy and its encoding, the build's union, and the planner's
per-domain grants have unit tests.

Part five, names past the microVM:

- **The host's resolvers answer them.** A VM's network process knows the host's resolvers:
  the daemon passes each `--resolver`, from `SHARDS_DNS` (`ADDR[:PORT]`, comma-separated,
  as `dockerd --dns` names them) or else the host's own IPv4 nameservers in
  `/etc/resolv.conf`. Under `Ports`, a query to the gateway's port 53 that the network's
  members do not answer goes to the first of them, as Docker's embedded DNS asks the
  host's for a name its network does not hold. It goes as a UDP flow keyed to the
  gateway, so the answer comes back from the gateway. The host asks, so a loopback
  resolver (a local cache, systemd-resolved's 127.0.0.53) serves as it is, where Docker
  must replace it for a container.
- **Only agents with egress ask.** Each such domain's `/etc/resolv.conf` names the
  microVM's gateway (`options ndots:0`), mounted read-only over the system's as its
  `/etc/hosts` is. The switch admits its UDP to the gateway's address, port 53, and
  nothing else of DNS; a domain without egress keeps the system's file, and its queries
  are dropped at the switch.

Tested: the egress test runs a resolver on the host (`SHARDS_DNS`). `a` reaches the
granted port by name; `b`, without egress, resolves nothing (EAI_AGAIN).
Mutation-checked: a network process that never forwards, and a switch with no rule for
DNS, each leave `a` unable to resolve.

Part six, ingress:

- **What is let in.** A port is let in past the microVM where both boundaries open it
  inward: the network's own grant (`--ingress`, `--expose`) and the microVM's for it
  (`EXPOSE … FOR` it, both ways or `AS ingress`). `shards run -p` publishes it as any
  port; `EXPOSE … AS egress` stays out of the image's exposed ports, so `-P` does not.
- **To whom.** To the network's member. Which of several members a connection is for, no
  directive says yet, so the build refuses a network with such ports and more than one
  member, naming them (`agentfile::ingress`), rather than guess, as D58 refuses a path
  that only an undesigned relay could allow.
- **How.** A published connection arrives at eth0 as any does. Init's namespace's `pre`
  chain (nat, prerouting, NF_IP_PRI_NAT_DST) gives it the agent's address, its `forward`
  chain accepts `eth0 → agents0` on those ports, and the switch accepts `up0 → d<x>` on
  them; answers return as established. The agent's Landlock lets it bind. A port the
  agent listens on that no grant lets in stays the run's own: published, it reaches the
  workload's namespace, not the agent.

Tested: `agents_answer_what_their_networks_let_in`, a detached run publishing 7100 and
7200. The agent listens on both; 7100, which `front` lets in, answers with its host name,
and 7200, which nothing lets in, does not reach it. The build's refusal of a network of
two members has a unit test. Mutation-checked: no DNAT, and no switch rule, each leave
7100 unanswered.

Open: DNS over TCP, for answers too long for UDP; ingress to a network of several members
(which one a connection is for).

A port the Agentfile declares `EXPOSE … AS egress`, and nowhere both ways or for ingress,
is named in the image's label `vnd.osi.agentfile.egress-declared` (ranges as Docker writes
them). A run that publishes one, by `-p` or `-P`, is refused before its microVM is made:
an egress port is where agents reach out to, not where they listen (answer 6). Tested by
the ingress test (`-p` of an egress-declared port refused, its words), mutation-checked;
the label's ports by a unit test. (`--ingress`, `EXPOSE … AS ingress`: a published port to a
domain); remote MCP servers, whose grant names one destination and not a port; IPv6
subnets; process events; §9.10's escape tests beyond these.

### Default deny, made exact (D59, part seven)

Raised 2026-10-06 by the user, and now a rule of the project (CLAUDE.md): networks in a
shards microVM are default deny, and every agent is airgapped unless configured
otherwise. Parts three to six had let implicit grants through, each now removed:

- **Membership granted flows.** Members of a network reached one another (my reading of
  §4.6's "may communicate with one another"), with `TO` only restricting a direction.
  Now membership grants nothing; a flow exists only where a `CONNECT` names it.
- **Pairs reached every port.** A `CONNECT` pair passed any port and protocol. Now
  `CONNECT --port=<port>[/tcp|/udp]` (repeatable, ranges, TCP by default) names what the
  receiver accepts, and the switch passes those alone (`iif d<x> oif d<y>`, `meta
  l4proto`, the destination port's range). A `CONNECT` between agents with no `--port`
  is a build error; one naming one agent alone attaches it and grants nothing.
- **The network was no boundary inside the microVM.** A `CONNECT` port must lie within
  what each of its networks lets in (`NETWORK --ingress` or `--expose`), checked at build
  (`agentfile::connections`), as answer 6's two boundaries apply inside as at the edge.
- **Names of peers out of reach.** Each agent's `/etc/hosts` listed every member. It now
  lists the agent and the peers it is granted a flow to.
- **DNS came with egress.** It is now its own grant: `NETWORK --dns` (any name, on a
  network that is not internal), or a remote `MCP` server's host name alone.

**The agents' resolver** (`agentdns`), a process of init's in the switch's namespace,
answers on every switch address, so each agent asks its own gateway. It knows who asks
by the link the question arrives on (`IP_PKTINFO`'s interface index), not by any address
an agent could claim, and answers from the address asked. It forwards a granted question
to the network process under an ID of its own, never holding one query behind another,
and answers the rest REFUSED at once. The network process forwards what its policy
allows: any name if the image's label says `--dns`, else the remote MCP servers' host
names, REFUSED otherwise. That is the union of the agents' grants, held per agent by the
relay; the test's case shows why both are needed: an agent with an MCP grant alone beside
one whose network grants `--dns` (so the host allows any name) still has its other
names refused.

**Remote MCP servers are grants of that server alone** (§4.4, §9.6). The build labels
each server's `host:port` (`vnd.osi.agentfile.mcp`). An agent its `FOR` names (every
agent, with no `FOR`; no harness) gets a link, a network of its own if it has none, the
server's port up the switch, and its host's name at the relay. The network process lets a
TCP flow to that port reach only the addresses its resolver answered for the name through
this process (it reads each forwarded answer's A records, RFC 1035 §4.1), or the address
the URL names. Before the name is resolved, no address of it is reachable.

**What an agent cannot tamper with.** Nothing that enforces this runs where an agent can
reach it: the switch's rules, the agents' resolver and the network process are outside its
namespaces; it holds no capability (no `CAP_NET_ADMIN` to change its address or routes,
no `CAP_NET_RAW` for a raw socket), seccomp refuses netlink and packet sockets, and its
`/etc/hosts` and `/etc/resolv.conf` are read-only mounts of its own namespace. Identity is
by link, never by address: the switch's rules match the arriving and leaving link, the
relay the arriving one, and the switch's namespace runs strict reverse-path filtering
(RFC 3704 §2.2, `rp_filter` 1) so that a packet on an agent's link from any address not
its own is dropped by the kernel, as a second layer under the capabilities that keep it
from being sent at all.

Found while testing: the switch's `input` chain, dropping by default, dropped the
answers to the relay's own questions upstream until it took conntrack's established
traffic; and an edit that set `IP_PKTINFO` had not landed, so the relay received
questions with no arrival and dropped them, which a probe of the control data showed.

Tested on real microVMs:

- `agents_reach_only_what_connect_grants`: `CONNECT --port=7000 a TO b` reaches b on
  7000 and not on 7001, where b listens too; b's way back to a is dropped; d, paired with
  b alone, reaches b and not a, a member of the same network; each agent's names are its
  own and its granted peers';
- `agents_reach_past_the_microvm_what_their_networks_grant`: names past the microVM with
  `--dns`, and none without;
- `a_remote_mcp_server_is_a_grant_of_that_server_alone`: the server's address is refused
  before its name is resolved and reached after; another port is dropped; an ungranted
  name is REFUSED by the relay though the host would allow it; the agent cannot rewrite
  its resolver file (EROFS), open a raw socket or leave its network namespace (EPERM); an
  agent not named reaches nothing.

The build's rules (`CONNECT --port` required and within its networks' ingress, `--dns`
not internal, reach edges only from granted pairs) and the planner's (pairs, names,
grants, MCP scope) have unit tests. Mutation-checked: the relay granting any name, the
network process passing a named port without a learned address, and the switch ignoring
a pair's ports, each fail their test.

Open: DNS over TCP; IPv6; process events; ingress to a network of several members.

### A domain sees the image as built (D59, part eight)

Found by probe, 2026-10-07: an agent with no grant connected to a pathname Unix socket the
run's own command made at `/work/escape.sock` with mode 0777. A domain saw the microVM's
live system, read-only; connecting to a socket needs write permission on its file
(unix(7), "Pathname socket ownership and permissions") and no write access to its
filesystem, so the read-only mount and Landlock's write rights did not stop it. Landlock
closes this only from ABI 9 (`LANDLOCK_ACCESS_FS_RESOLVE_UNIX`, Linux 7.1, ae97330d1bd6),
and the pinned kernel, 6.18.48, has ABI 7. The live system also let an agent read every
file the run wrote and every volume it mounted, grants no directive made.

Now a domain's root is the image as built, an overlay with no writable layer of the
image's EROFS layers (the descriptor `changes::keep` holds for `diff`) under a tmpfs that
adds only what a domain mounts over and the image may lack (`/proc`, `/dev`, `/sys`,
`/tmp`, `/etc/hosts`, `/etc/resolv.conf`). Its first process attaches it over `/proc`
(`move_mount`), makes it its root (`pivot_root(".", ".")`) and detaches the system under
it, before any other mount. Nothing the run makes reaches it: not its files, mounts or
sockets, pathname or abstract. Only long-standing primitives hold this (the new mount
API, Linux 5.2; overlayfs), so it holds on any guest kernel a domain runs on, and for any
image; a newer kernel's Landlock rights would be a layer under it, never the only one.
The rule is the user's: fixes are version-agnostic where they can be.

An agent's own sockets, in its own `/tmp`, still work among its own processes; one
another domain made is hidden with its directory, and one the run makes does not exist
for it. No directive yet grants a domain a socket of the run's.

Tested on real microVMs: `an_agent_reaches_no_socket_of_the_runs_own`, where the run binds
`/work/escape.sock` (0777) and an abstract `shards-escape`, and an agent with no grants
tries both: `ENOENT` and `ECONNREFUSED`, its last try after the run bound both (one
`CLOCK_MONOTONIC`). Mutation-checked: without the new root the agent connects.

### What a failing agent may take (D59, part nine)

Raised 2026-10-07 by the user: how a malfunctioning or compromised agent might abuse the
microVM, mitigated without burdening the Agentfile. Two ways it could take down everything
in it, init (PID 1, whose end is the microVM's) and the run's own command included:

- **Its output.** init relays an agent's output a line at a time and held an unfinished
  line whole, so an agent writing no newline grew init's memory without bound. Now a line
  is written unfinished at 16 KiB, Docker's bound for a container's log lines (moby
  daemon/logger/copier.go, `defaultBufSize`), each piece a line of its own
  (`frames::LINE_MAX`; test `a_line_without_end_is_written_in_pieces`).
- **Its memory.** Domains had no memory cgroup, and each one's scratch tmpfs could grow to
  half the microVM's memory (Documentation/filesystems/tmpfs.rst, `size`). The OOM
  killer chooses by resident memory, page tables and swap (mm/oom_kill.c,
  `oom_badness`), not a tmpfs's pages, so an agent filling `/tmp` would have the kernel
  kill the run's command, again and again. Now:
  - the domains together are a cgroup whose `memory.high` is what the microVM has as they
    start (`MemAvailable`, Documentation/filesystems/proc.rst) less what the workload's
    `-m` still allows it (`memory.max` less `memory.current` of its cgroup); the root
    cgroup has no `memory.current` (Documentation/admin-guide/cgroup-v2.rst), so the
    kernel's own estimate is the measure. Their tmpfs pages are charged to them;
  - each domain has `memory.oom.group`, so that one out of memory ends whole: a filler
    killed alone would leave its scratch held by the rest of the domain;
  - an agent's OSI config may ask a lower limit of its own, `asks.memory` in bytes, beside
    `asks.processes`. The user chose this over a per-agent share or no default.

  - **init chooses, not the kernel.** Charged past a `memory.max`, a write invokes that
    cgroup's OOM killer (mm/memcontrol.c, `try_charge_memcg`, `mem_cgroup_oom`), whose
    victim is again chosen by resident memory: with the domains' cap first a `memory.max`,
    an agent filling its scratch had others ended before it (in the first version of the
    test, y and z both), and a killed domain's tmpfs is freed only as its mount namespace
    goes. So the cap is `memory.high`: past it the kernel throttles the domains, in the
    charge itself every 64 pages as well as on the way back to user space, and kills none
    (Documentation/admin-guide/cgroup-v2.rst: "Going over the high limit never invokes the
    OOM killer"; the file names the case, an external process that monitors the cgroup).
    init watches the domains' `memory.events`, which the kernel marks modified as it
    changes, and past the cap ends the domain whose `memory.current`, scratch included,
    is the most, with `cgroup.kill` (Linux 5.14), saying so on the run's stderr
    (`[agent y] shards-init: ended: the agents' memory, …`). systemd-oomd and oomd choose
    by cgroup usage in the same way. A domain ended still holds the most until it has left
    its cgroup, which a process does after its namespaces, and with them its scratch, are
    gone (kernel/exit.c, `do_exit`: `exit_nsproxy_namespaces`, `exit_task_work`, then
    `cgroup_task_exit`), so no other is ended for memory it still holds; a rule waiting for
    it, written first, changed no outcome under mutation, and is not kept.

- **The agents' resolver.** It holds each question in flight under an ID of its own, 65,535
  at most, and refused any beyond: one agent asking without end could take them all, and
  every other agent's names with them, for as long as its questions wait (5 s, `WAIT`).
  Now each asker, known by its link, may hold an equal share of the IDs, `u16::MAX` over
  the askers: no asker can take another's, and the share is the ID space's, not a chosen
  number (`agentdns::Flight`; test `no_asker_takes_anothers_share_of_the_ids`, run in a
  microVM as init's Linux tests are on this host, mutation-checked: with no share, one
  asker took every ID).

- **Flows through the switch, measured: no change needed. Wrong, corrected by D61:** the
  flood measured was of flows conntrack evicts; flows it may not, held, fill one table
  for every agent. As first recorded: Conntrack's table holds 2048
  entries in a 237 MB microVM (`nf_conntrack_max`, sized by the kernel from memory), a
  count kept per network namespace against that one limit (net/netfilter/
  nf_conntrack_core.c, `__nf_conntrack_alloc`), and an unanswered UDP flow stays 30 s: one
  agent with a UDP grant fills the switch's table at once. But a full table evicts an
  entry not yet assured (`early_drop`), which a flood's are. Measured
  (`an_agents_flood_of_flows_takes_no_others`, 5 runs, each 200 of 200, MacBook arm64, macOS 26.4; in the one printed, a sent
  860,478 datagrams in 10 s, each from a new socket, to b's granted UDP port, while c made
  200 of 200 TCP connections to b, in 16 ms. The test stays, so that a rule assuring such
  flows would be seen. The kernel lacks `nft_connlimit` (`CONFIG_NFT_CONNLIMIT`), which
  a per-agent bound would need; none is needed.

`/tmp` stays where an agent's scratch is: the run's own writable layer is a tmpfs as well
(`run.rs`, `mount_root`), so every writable directory in a microVM is its memory, and a
programs' default temporary directory (POSIX `TMPDIR`) is where they look.

Tested on real microVMs: `agents_out_of_memory_end_whole_and_spare_the_run`, the run's
command holding 64 MiB under `-m 128m`: x, asking 64 MiB, ends with 58 MiB written (its
resident memory the rest); y, asking nothing, is ended by init at the domains' cap, and z,
holding 16 MiB resident and no scratch, more than y's resident memory, lives; the run's
command outlives them, and each domain ended lost its PID 1 with its filler. Each guard
mutation-checked: without x's limit it wrote 82 MiB; without the workload's share taken
from the cap, the kernel killed the run's command; with the cap a `memory.max`, the
kernel ended z; with init choosing the least, z was ended; without a cap, init ended
none; without `memory.oom.group`, a domain's PID 1 outlived its filler. Found while testing: an agent
can fill and end between two looks of the run's command at `/proc`, and nothing else of
it reaches the run, so the test's agents are seen filling before they fill.

### Protocols a network carries, and Unix sockets granted (D59, part ten)

Raised 2026-10-07 by the user: a network should say which protocols it carries, TCP, UDP,
Unix or any of them, with TCP and UDP by default; and agents, harnesses, the skills they
call and anything they run must use no Unix socket not granted. The interface is the
user's choice of three put to them:

- `NETWORK --protocol=<tcp|udp|unix>` is a boundary like `--ingress`: every port on the
  network, and every `EXPOSE … FOR` it, must be of a protocol it carries, checked at build
  (`agentfile::connections`). The runtime grants only ports already, so TCP and UDP need
  nothing more there.
- A Unix socket is a port, `unix:<name>`: let in by `NETWORK --ingress` (or `--expose`;
  `--egress` refused, since one never leaves the microVM) and granted by `CONNECT
  --port=unix:<name>`, one way or both as the `CONNECT` says.

At run, each name granted on a network is a directory of the tmpfs the run's writable
layer lies in (`/rw`), which no path from the run's root reaches, and which is no shared
mount, so nothing made there after the run starts reaches the run's mount namespace. init
copies the directory's mount for each agent granted it (`open_tree`, `OPEN_TREE_CLONE`,
Linux 5.2: a copy of a mount of init's own; copying one already detached needs 6.15,
c5c12f871a30, so none is), read-only where it only connects (`mount_setattr`, 5.12), and
the agent attaches it at `/run/networks/<network>/<name>/` (`move_mount`), whose mount
point its own skeleton holds: no agent sees the name of a socket it is not granted. The
preview put the socket at `<name>.sock`; a socket cannot be a mount point before it
exists, and a directory shared by every name would grant each agent every name, so a
grant is a directory, the socket inside.

Two more things a socket needs, found in the kernel's source:

- Connecting needs write permission on the socket (unix(7)), and `bind` applies the
  binder's umask itself (net/unix/af_unix.c, `unix_bind_bsd`), so a default ACL on the
  directory would not help: an agent receiving a socket starts with umask 0, its sockets
  then let in exactly who can see the directory, those granted. The directory is sticky,
  so none removes another's.
- Landlock refuses making a socket outside an agent's scratch: an agent receiving one is
  given `MAKE_SOCK` and `REMOVE_FILE` beneath its directory alone.

Tested: `networks_carry_only_their_protocols` (the build's rules: TCP and UDP by default,
Unix where named, a protocol not carried refused wherever named, a name not let in, a Unix
socket as egress, what is no protocol and what is no port) and, on real microVMs,
`agents_reach_only_the_unix_sockets_granted`: b receives tools and secret and makes both;
a, granted tools, connects to it, cannot make one there (`EROFS`), and secret does not
exist for it; c, granted secret, likewise; d, a member granted none, sees neither.
Mutation-checked: a sender's directory writable, every grant mounted in every agent, the
umask kept, the Landlock rule taken away, and the default carrying Unix each fail a test.

### Nothing past an agent's grants (D59, part eleven)

§9.10's sweep, on a real microVM (`an_agent_reaches_nothing_past_its_grants`): the run's
own command answers TCP and UDP on 7100 at every address it has; d, granted TCP 7000 to b
alone, and e, granted nothing, try it at the microVM's address, both ends of the uplink
and the microVM's gateway, and d its own network's gateway (the switch's end of its link,
DNS among it). What came of each, measured: d has no default route, an agent without
egress having only its network's route (via the switch), so every address past its
network is `ENETUNREACH`, and its gateway drops all (timeouts); e has its loopback alone,
so UDP has no route and TCP is refused by Landlock before it leaves (`EACCES`). d still
reaches b on 7000. Mutation-checked: with the switch's `input` chain accepting, d's
connection to its gateway is refused by it (`ECONNREFUSED`), an answer. Since D61, d has
egress and ingress on a network of its own, so it has a route past its network, and
every probe times out; its gateway is its gate's.

§9.9's double fork, on a real microVM (`an_agents_daemon_ends_with_it`): an agent lets a
grandchild go as daemons are (fork, `setsid`, fork), waits until the run's command has
seen it, and its first process ends; the grandchild ends with it, the kernel ending a PID
namespace's every process with its first. Mutation-checked: without `CLONE_NEWPID` the
grandchild outlives the agent.

§9.8's kernel channels, on a real microVM (`no_kernel_channel_joins_two_agents`): a makes
a System V shared memory segment and message queue of one key, a POSIX message queue and
a file in `/dev/shm`, and opens each itself; b, given none, opens none (`ENOENT` each), and
its signal to every process it may signal (`kill(-1)`) reaches none of a's. Mutation-
checked: without `CLONE_NEWIPC`, b opens a's segment and both queues.

Found by it: an agent could make no POSIX message queue at all, even its own. `mq_open`
makes one in its IPC namespace's mqueue filesystem, which no path of the agent's reached,
so Landlock refused it (`EACCES`). A Docker container has `/dev/mqueue` (moby
daemon/pkg/oci/defaults.go), as the run's command here does; now each domain mounts its
own there, which is the filesystem `mq_open` uses (ipc/mqueue.c, `mqueue_get_tree` keys it
by the IPC namespace), with every Landlock right beneath it.

Found while writing the sweep: `getifaddrs` fails in an agent, musl's asking over netlink, which
an agent's seccomp filter refuses; the test reads `/proc/net/route` instead.

### `SKILL --from=<agent>` (D54, continued)

`SKILL --from=<agent> <skill> FOR <other>` copies, at build time, a skill the agent's
OSI config lists (`skills`) into the other's grants (AGENTFILE_ARCH.md §12 answer 12): a
declaration in the Agentfile of what the agent brings, not one agent reading another.
The skill comes from the agent's content, the state `COPY --from=<agent>` copies from
(D56), and is checked and laid out as any skill. A path its config does not list is
refused, naming what it lists; so is an agent with no config, one from a path, Git or
an http(s) URL, as only an OSI artifact carries one. As `COPY --from=<agent>` does, it
makes the agent's declaring stage a dependency, except where that stage is its own: a
directive of the same stage comes first in it, and the first version, which made it a
dependency of itself, failed as a circular one (found by the test, and the same held for
`COPY --from=<agent>` in its declaring stage).

Tested: `a_skill_an_agent_lists_is_taken_from_it` (an OSI agent's listed skill laid in
another's grants, nothing else of its content; an unlisted path and an agent with no
config refused in their words), the listing check mutation-checked.

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
