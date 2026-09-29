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
| D2 | Starts are served by **snapshot restore into a pre-spawned, pre-initialized VMM process**, never by spawning or cold-booting on the request path. | `posix_spawn` → `main` 3.7 ms p50; framework load +1.5 ms; first `hv_vm_create` 0.55 ms [PM M11, M11b]; one VM per process [GT §1.1] |
| D3 | Use the in-kernel GIC (`hv_gic`). Snapshot it as **individual registers**, never as the state blob. | WFI blocks in-kernel with zero userspace exits [PM M7]; blob restore 1.2 ms vs 20 µs register rewrite [PM M14] |
| D4 | vCPU threads run with Mach time-constraint policy plus QoS user-interactive. Pending: fail-safe behaviour under CPU-bound guests. | Timer lateness 258 µs → 8.7 µs at 1 ms; idle IRQ p99 315 → 11 µs [PM M8, M10] |
| D5 | Create vCPUs strictly sequentially in index order, both at boot and on restore. | Redistributor frames and processor numbers follow creation order [PM M13] |
| D6 | Guest RAM uses the 16 KiB IPA granule (default); the guest's own page size stays 4 KiB. | First-touch cost per byte is 4× lower than with a 4 KiB granule [PM M5] |
| D7 | Snapshot memory is file-backed `MAP_PRIVATE`, mapped lazily. The working set can be prefetched from helper vCPUs, not from host reads. Map it in the warm process before the request: in-process faults on file-backed memory have no tail beyond 111 µs in 6.3 M, but fresh processes mapping a just-unmapped file stalled ~1 s in ~1% of boots [PM M15, M16]. | Cheapest first-touch backing, 1.07 µs per 16 KiB page; host pre-read doesn't help; 4 vCPUs fault 2.2× faster [PM M5, M6] |
| D8 | Every HVF exit is a userspace exit: negotiate EVENT_IDX, batch, and keep notify handlers to a hand-off. Adaptive polling only above a rate threshold. | No ioeventfd on HVF [GT §1.2]; ELVIS [VIO §2.3] |
| D9 | Implement **both** virtio-mmio and virtio-pci (modern, per-queue MSI-X). Choose the default transport by measuring the restore path and runtime. | MMIO costs 2 exits per interrupt; PCI is needed for VFIO [VIO §2.4, R3]; GPU-free default VMs must stay pin-free [GPU R1] |
| D10 | Pin the guest's CPU view explicitly: MPIDR, PARange clamped to the IPA, SME exposure decided per image. Don't inherit defaults. | Defaults show PARange 40 on a 36-bit IPA and expose SME2 [PM M12] |
| D11 | GPUs are zero-cost when unused. GPU VMs are a separate class assigned from a warm pool (VFIO via iommufd on Linux; virtio-gpu/Venus plus a remoting broker on macOS). | Assigned devices pin all RAM and break CoW; FLR ≥ 100 ms; CUDA init takes seconds [GPU §2.3, R1–R6] |
| D12 | vsock is the host↔guest control plane (exec, stdio, lifecycle, engine API). Built (a7b32ab): guest ports map to host Unix sockets as in Firecracker (`CONNECT <port>`; the guest reaches `<path>_P`). Unlike Firecracker, host EOF is a half-close, so a guest can answer after stdin ends. After a restore the device posts TRANSPORT_RESET; each restored copy binds its own socket. | Rootless and portable; Firecracker's AF_UNIX mapping [VIO R7]; macOS poll reports POLLHUP on a half-close, so the device waits with kqueue there |

### Snapshots (D14)

A snapshot is taken at a point the guest chooses. The guest writes the control page's
`SNAPSHOT` register, and every clone resumes at the instruction after that store. A
template is thus a guest that has finished initializing and says so; restores never
re-run that work.

- **Pause.** The request kicks every vCPU. Each captures its own state on its own thread
  (HVF's owning-thread rule), including redistributor, ICC and PSCI power state, and parks.
  A coordinator then pauses devices at a request boundary (the virtio-blk worker stops and
  hands back its queue), and saves the GIC distributor, the devices and guest memory.
- **Format.** The state is backend-neutral: system registers are keyed by op0..op2
  encoding and GIC registers by GICv3 offset (ground-truth doc §5 row 16). It records one
  guest counter for the whole VM and the CPU ID registers; a restore on another CPU is
  refused. Memory is sparse (zero pages are holes). Files are written, synced and renamed.
  Decoding treats the files as untrusted.
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

### Platforms (D13)

shards is platform- and architecture-agnostic. The matrix is the sibling projects' release
matrix: 8 targets, all 64-bit. Hardware virtualization runs guests of the host's own ISA, so
the guest arch is always the host arch.

| Host OS | Arch (Rust triple / OCI name) | Backend (`hv`) | Status |
|---|---|---|---|
| Linux (glibc, musl) | x86_64 / amd64 | KVM | booting Linux: SMP, ACPI, virtio-blk; CI on both libcs. Snapshots next |
| Linux (glibc, musl) | aarch64 / arm64 | KVM | planned |
| macOS | aarch64 / arm64 | Hypervisor.framework (arm64 API) | booting Linux; snapshots with cold and warm restore |
| macOS | x86_64 / amd64 | Hypervisor.framework (x86 VMX API) | planned |
| Windows | x86_64 / amd64 | Windows Hypervisor Platform | planned |
| Windows | aarch64 / arm64 | Windows Hypervisor Platform (arm64) | planned |

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
  - The exit status is what `docker run` reports (docker/cli `runStartContainerErr`):
    the command's own; 128 plus a fatal signal; 127 if the command is not found; 126 if
    it is not executable or is a directory; 125 otherwise.
  - The run ends with the main process. Everything left is killed, as when a container's
    PID namespace ends.
- **Warm runs.** `vm run --rootfs IMAGE --snapshot-dir DIR` saves a template: shards-init
  asks for the snapshot once the image is mounted, before it dials the host. Each
  `vm restore DIR -- COMMAND` resumes a copy that dials in for its own command (D2, D14).
  Copies share the image and nothing they write.
  - With `--hold`, the copy resumes at once, reseeds and connects. Its request is then only
    the command. It is answered in 1.0 ms at p50 and 2.4 ms at p99, over six templates
    (benchmarks.md, "Run").
  - Guest state differs between templates, and a restored guest can stall for a tick
    after its release. That now happens before the request.
- **Kernel command line:** `noautogroup`, because otherwise most templates stalled a tick
  in the first `setsid(2)` after a restore (benchmarks.md, "Run").
- **Signals** reach the command as `docker run --sig-proxy` forwards them (docker/cli
  `signals.go`): every one another process sends, by name, as Linux numbers them.
  - They travel on a second vsock connection that the guest opens once the command runs,
    so unread stdin cannot hold them up.
  - A terminating signal that arrives before then ends shards.
- **Not yet:** TTYs (`-t`), detached runs.
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
  ChainID, so images with the same layer stack share one.
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
  - A connection is not reused after a response with both framings, or with bytes past
    its end. The second rule is Go's too, and it keeps responses from desynchronizing.
- **Connections as containerd's transport keeps them** (`core/remotes/docker/registry.go`):
  - 30 s to connect, racing addresses 300 ms apart (RFC 8305);
  - 10 s for the TLS handshake, 30 s for the response head;
  - at most 10 idle connections, each kept for 30 s.
  - A GET or HEAD that fails on a reused connection before its response is resent on a
    new one, as Go resends replayable requests.
  - A body that makes no progress for 30 s fails, where Go would wait on its context.
- **URLs** follow RFC 3986 (iri-string): references resolve as §5.2 says, and nothing is
  normalized, so a presigned URL keeps the exact bytes its signature covers. Messages
  never show a query.
- **Fields** holding CR, LF or NUL are refused (RFC 9110 §5.5), so a token from a server
  cannot inject fields.
- **Not yet:** proxies (`HTTPS_PROXY`, `NO_PROXY`, CONNECT), and decoding a
  `Content-Encoding`.
- **Tests:** a scripted loopback server covers:
  - every framing and 1xx skipping;
  - 16 malformed responses, all refused;
  - reuse of plain and TLS connections, and a stale pooled connection replaced;
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
  - requires one DiffID per layer.
  - Layers download 3 at a time, dockerd's default. Every size, digest and DiffID is
    checked before the EROFS image is built and the reference recorded.
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
  - rate limits reported, not retried.

## 3. Components

```
shards (host CLI, docker-compatible) ──unix socket──▶ shardsd (daemon)
                                                     │  image store · build · networks
                                                     │  volumes · templates/snapshots
                                                     │  warm VMM pool · policy
                                                     ▼
                                       VMM process (one per microVM)
                                       hv backend (HVF | KVM) · memory · boot/FDT
                                       vCPU threads · GIC · virtio devices · snapshot
                                                     │ virtio (blk/net/vsock/console/rng/pmem/fs/gpu)
                                                     ▼
                                       guest: Linux (tuned) · shards-init (PID 1)
                                       shards-engine (containerd's place; Docker Engine API)
                                       agents and compose services, many per microVM
```

- **VMM** (the `shards-vmm` library, run by the `shards` binary): one process per microVM. That is forced on macOS
  [GT §1.1] and chosen on Linux for fault isolation, as Firecracker does. The hot
  path is kept free of allocation and locks; device threads communicate with vCPU
  threads through lock-free rings.
- **Daemon** (`shardsd`): serves a Docker-compatible API with extensions for VM
  specs and isolation policy. It owns the warm pool and the template snapshots.
- **Guest**:
  - a tuned Linux kernel built from source inside a shards builder VM
  - `shards-init`, a minimal static PID 1 that sets up and then drops privilege
  - `shards-engine`, our own runtime, rootless. It walks and talks like containerd and
    Docker (Engine API, Compose) but runs no containers underneath. **pending**: its
    design, informed by the engine-internals and rootless research.

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
   - Built: our kernel (CI releases); virtio-pmem; the EROFS writer; layers → one EROFS
     image (D15); booting into an image to run a command (D16); the image store (D18);
     registry TLS, HTTP and auth (D19–D21); pulls (D22). Next: `shards pull`, and
     `shards run IMAGE`.
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
