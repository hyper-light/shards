# Container engine internals: a lean, rootless, Docker/compose-compatible engine inside the microVM

*Research note, 2026-09-28. Evidence is limited to peer-reviewed papers, specs, official docs and source code. Preprints are labeled. Claims without a source are marked UNVERIFIED or deferred to §4.*

## 1. Scope

This note covers the in-guest engine and OCI runtime. The VMM appears only where it constrains the engine: the restore working set, identity after restore, and device exposure.

The six questions:

- Q1: where start time and memory go.
- Q2: the OCI runtime contract and rootless specifics.
- Q3: engine behaviors needed for Docker/compose parity.
- Q4: designs for millisecond-scale container start.
- Q5: memory under copy-on-write (CoW) snapshot cloning.
- Q6: rootless `docker build`.

**Citation tags** (format `[tag path:Lx-y]`; full commit hashes are in §5):

| Tag | Source |
|---|---|
| `k` | Linux v7.2-rc4, local tree |
| `rs` | runtime-spec v1.3.0 |
| `is` | image-spec v1.1.1 |
| `cs` | compose-spec @914ec15 |
| `cdi` | CDI v1.1.1 |
| `moby` | docker-v29.8.1 |
| `cli` | docker/cli v29.8.1 |
| `compose` | docker/compose v5.5.1 |
| `ctrd` | containerd v2.4.1 |
| `runc` | runc v1.5.2 |
| `crun` | crun 1.30.1 |
| `youki` | youki v0.7.0 |
| `bk` | buildkit v0.33.0 |
| `tini` | tini v0.19.0 |

**Caveat.** Every peer-reviewed measurement here was taken on Linux 4.4–4.19. I re-checked each cost driver against the 7.2 source, and several conclusions change. Those items are marked **7.2:**.

## 2. Findings

### 2.1 Q1: where start time and memory go

| System (kernel) | Start result | Source |
|---|---|---|
| Docker (4.8) | ≈200 ms. For comparison, fork/exec of a plain process: 3.5 ms mean, 9 ms p90. | [Manco17 §4.2, Fig. 4] |
| Docker 18.09 + overlay2 (4.15) | 541 ms on an idle host, 1.5 s with >1,000 containers present, 8.5 s mean with 16-way parallel creation. 5.3 creations/s versus 45/s for processes. | [Cadden20 §7, Table 3] |
| containerd/runc via CRI (4.19) | create 0.50 s, run 0.18 s, destroy 0.48 s | [Espe20 §5.3, Table 1] |
| SOCK lean containers vs Docker (4.13) | 18× the throughput; 130 ms mean latency (19× lower) | [Oakes18 §5.1, Fig. 14] |
| Catalyzer sfork | 0.97 ms; Zygote path 5–14 ms | [Du20 §6.2, Fig. 11] |

**Network namespaces are the largest single cost.**

- In Docker, "Network Create" plus "Network Connect" take 90% of startup [Mohan19 §4].
- Creating network namespaces took 0.28 s for one and 14.41 s for 100 concurrent; cleanup took 0.20 s and 7.77 s (kernel 4.4) [Mohan19 §4, Table 1].
- SOCK found a global lock held during creation, which walked every namespace. Cleanup waited on RCU while holding the lock [Oakes18 §2.2, Fig. 3]. Container churn [Oakes18 §2.2, Fig. 4]:
  - 200 containers/s with network namespaces;
  - >400/s with IPv6 disabled and the broadcast code removed;
  - 900/s with no network namespaces.
- A bridge processes each broadcast once per endpoint, and holds at most 1,024 endpoints [Cadden20 §7; k net/bridge/br_private.h:L28-29].
- **7.2:**
  - `copy_net_ns` takes `pernet_ops_rwsem` shared, so creations run concurrently [k net/core/net_namespace.c:L575-581].
  - Creation cost scales with the number of registered `pernet_operations` [L436-465].
  - Teardown is a single batching work item bounded by `synchronize_rcu`/`rcu_barrier` [L238-245, L658-726, L745-751].
- Steady state: Docker's NAT doubled round-trip latency, while CPU and memory overhead was about zero [Felter14-RC §III.F Fig. 3, §III.J].

**Mount namespaces and the rootfs.**

- The rate of cloning mount namespaces falls toward zero as host mounts grow [Oakes18 §2.1–2.2, Figs. 1–3].
- Mount and IPC namespace cleanup take tens of ms (RCU waits) [Oakes18 §2.1–2.2, Figs. 1–3].
- Bind mounts are about 2× faster than AUFS, and `chroot` takes <1 µs [Oakes18 §2.1–2.2, Figs. 1–3].
- A device-mapper rootfs took ≈10 s at 200 concurrent starts, versus ≈30 ms alone [Li22 §3.1].
- A volatile, reflinked writable layer cut rootfs preparation from 207 ms to 0.2 ms [Li22 §4.2].
- **7.2:**
  - `copy_mnt_ns` still runs `copy_tree` over the whole mount table [k fs/namespace.c:L4232-4300].
  - `open_tree(OPEN_TREE_NAMESPACE)` (v7.0) avoids that copy. The commit calls it a "combined unshare(CLONE_NEWNS) and pivot_root()".
  - `CLONE_EMPTY_MNTNS` (v7.1) also avoids it, cloning only the root mount.
  - Sources for both: [k commits 9b8a0ba68246, 9d4e752a24f7; include/uapi/linux/mount.h:L65; include/uapi/linux/sched.h:L42].

**cgroups.**

- Reusing a cgroup is at least 2× faster than creating one [Oakes18 §2.3, Fig. 5].
- `cgroup_mutex`, `css_set_lock` and `freezer_mutex` serialized 2,000 concurrent creations. A pool reached by renaming cut creation time by 94% [Li22 §3.3, §4.4, Fig. 6].
- **7.2:**
  - `cgroup_mkdir` takes `cgroup_lock()` [k kernel/cgroup/cgroup.c:L1695-1720, L6017-6026].
  - cgroup v2 has **no rename operation**, and v1's rename also takes `cgroup_lock()` [cgroup.c:L6315-6320; kernel/cgroup/cgroup-v1.c:L847-877].
  - So RunD's premise that rename is lock-free does not hold, and a v2 pool must not rely on renaming. The spec allows any cgroup naming [rs config-linux.md:L327-329].
  - `CLONE_INTO_CGROUP` takes `cgroup_mutex` and the threadgroup rwsem, and checks delegation the same way a `cgroup.procs` write does [cgroup.c:L6756-6860].
  - clone(2) calls spawning directly into the target cgroup "significantly cheaper than moving the child process into the target cgroup after it has been created" [man clone(2), man-pages 6.19].

**seccomp.**

- Each filter attach builds a new BPF program and emulates the filter for every syscall number to precompute an allow-bitmap [k kernel/seccomp.c:L669-705, L847-900].
- At syscall time, a bitmap hit skips BPF entirely [L156-176, L400-425].
- Filters are shared by refcount across fork [k kernel/fork.c:L1777-1790].
- Docker's profile: default action ERRNO, 33 rules, and a 361-syscall allow rule [moby vendor/github.com/moby/profiles/seccomp/default.json:L2-3, L62-427].
- Attach cost is unmeasured (M5).

**PID namespaces and PID 1.**

- A PID namespace is single-use: once its init exits, allocation fails with ENOMEM [k kernel/pid.c:L326-328; kernel/pid_namespace.c:L201].
- The namespace's init is marked `SIGNAL_UNKILLABLE` and ignores signals left at `SIG_DFL` [k kernel/fork.c:L2500-2506; kernel/signal.c:L94-96]. A PID-1 process with no handler therefore ignores `docker stop` until SIGKILL.

**Process creation.**

- fork takes >1 ms for a 176 MB parent and 6.5 ms for 1 GB, rising to 22.4 ms with 3 concurrent forks [Zhao21 §2.1, Fig. 2].
- `posix_spawn` takes ≈0.5 ms whatever the parent's size, and fork is not thread-safe [Baumann19 §4, Fig. 1].

**runc/crun/youki.** The only peer-reviewed runtime comparison found is [Espe20], which covers runc and gVisor. For crun and youki there are only vendor claims (§2.5).

### 2.2 Q2: the OCI runtime contract and rootless specifics

**Operations and lifecycle.**

- `state`, `create`, `start`, `kill` and `delete` are MUST, and they "are not specifying any command-line APIs" [rs runtime.md:L91-146]. An in-process library therefore conforms.
- Statuses are `creating`, `created`, `running` and `stopped` [L8-38].
- The lifecycle has 13 steps [L54-79; rs config.md:L548-614]. `create` applies all config except `process.args`. The hook order is:
  1. `prestart` (DEPRECATED)
  2. `createRuntime`
  3. `createContainer`
  4. `startContainer`
  5. the program runs
  6. `poststart`
- **There is no `exec` operation.** Docker's exec reuses the container's OCI `Process` and overrides only Args, Env, Cwd, Terminal and ConsoleSize [moby daemon/exec.go:L233-275].

**Linux config fields that matter for the design.**

- `namespaces[].path` joins an existing namespace. This is the hook for pools [rs config-linux.md:L20-46].
- `uid/gidMappings` "SHOULD NOT modify the ownership" of filesystems [L81-93]. `idmap`/`ridmap` mount options use `mount_setattr` [rs config.md:L154-155].
- A runtime MAY check whether the cgroup is fit for use, and MUST error if that check fails, e.g. a frozen cgroup, or a non-empty one at create [config-linux.md:L294-308].
- `netDevices` moves host interfaces into the container's netns [L192-262].
- Default devices and filesystems [L6-18, L178-190].
- Only stdio stays open, and the `/dev/fd` and `/dev/std*` symlinks MUST exist [rs runtime-linux.md:L3-18].

**The spec moby generates** (the parity target).

- Namespaces: mount, network, uts, pid, ipc, plus time. The cgroup namespace is private on v2 [moby daemon/pkg/oci/defaults.go:L117-124].
- 14 default capabilities [daemon/pkg/oci/caps/defaults.go:L4-21].
- Masked and read-only `/proc` and `/sys` paths [defaults.go:L110-116, L194-219].
- `/dev` is a tmpfs of `size=65536k`; `/dev/shm` is 64 MiB [defaults.go:L71-76; daemon/config/config.go:L41-42].
- Device rules: deny all, then allow c 1:3, 1:5, 1:8, 1:9, 5:0 and 5:1; deny 10:229 [defaults.go:L130-185].
- The network namespace is wired *after* create: `SetKey(/proc/<pid>/ns/net)`, then network allocation. This sits on the create→start critical path [moby daemon/start_linux.go:L17-44].
- Hooks are used only by the legacy NVIDIA `--gpus` path (a prestart `nvidia-container-runtime-hook`), and only after CDI fails [moby daemon/devices_nvidia_linux.go:L38-79, L138-157].

**Rootless kernel rules.**

- An unprivileged `uid_map` is a single line mapping the writer's own euid. `gid_map` also requires `setgroups=deny`. Arbitrary maps need CAP_SETUID in the *parent* user namespace [man user_namespaces(7)].
- With one ID ("Type III"), chown, setgroups and setresgid fail, which breaks package installs. "Type II" uses a privileged `newuidmap` [Priedhorsky21 §2.1–2.3].
- cgroup delegation means write access to the directory and to `cgroup.procs`, `cgroup.threads` and `cgroup.subtree_control`. Migration is contained to the common ancestor [k Documentation/admin-guide/cgroup-v2.rst:L537-600].
- The v2 device controller is a `BPF_PROG_TYPE_CGROUP_DEVICE` program [L2728-2750]. Loading one needs one of:
  - CAP_BPF and CAP_NET_ADMIN in the init user namespace [k kernel/bpf/syscall.c:L2840-2869, L3015-3044];
  - a BPF token from a bpffs whose `delegate_*` options were set by a CAP_SYS_ADMIN holder [k kernel/bpf/token.c:L110-150; kernel/bpf/inode.c:L1103-1104].
- overlayfs can be mounted inside a user namespace, with `userxattr` [k fs/overlayfs/super.c:L1576; Documentation/filesystems/overlayfs.rst:L867-869].
- Idmapped mounts need CAP_SYS_ADMIN over the superblock's user namespace [k fs/namespace.c:L4799-4838].

**What existing runtimes do rootless.**

- runc:
  - writes `uid_map` directly and falls back to `newuidmap` on EPERM [runc libcontainer/nsenter/nsexec.c:L266-294];
  - `runc spec --rootless` drops the netns, maps a single ID, and removes `Resources` [runc libcontainer/specconv/example.go:L164-219];
  - ignores cgroup errors when rootless; v2 limits need systemd delegation, which covers memory and pids by default [runc libcontainer/configs/config.go:L227; docs/cgroup-v2.md:L43-67];
  - skips device eBPF inside a user namespace [runc vendor/github.com/opencontainers/cgroups/devices/v2.go:L31-37].
- youki needs `newuidmap` for more than one mapping, and treats cgroup EROFS/EACCES as non-fatal [youki crates/libcontainer/src/user_ns.rs:L379-438; crates/libcgroups/src/v2/manager.rs:L102-148].
- containerd's overlay snapshotter adds `userxattr` inside a user namespace. It prefers idmapped mounts (kernel ≥5.19) to a recursive chown, which "duplicated" the image size per pod [ctrd plugins/snapshots/overlay/overlay.go:L150-157; plugins/snapshots/overlay/plugin/plugin.go:L85-96; docs/user-namespaces/README.md:L90, L111-112].

**Docker's rootless mode.**

- Needs `newuidmap`/`newgidmap` and at least 65,536 subordinate IDs.
- Resource limits work only with cgroup v2 plus systemd `Delegate=yes`.
- overlay2 needs kernel ≥5.11.
- No AppArmor, checkpoint, overlay networks or SCTP.
- Networking goes through RootlessKit with slirp4netns, pasta, vpnkit, gvisor-tap-vsock or lxc-user-nic.
- Sources: [docs.docker.com/engine/security/rootless/ (+tips/, troubleshoot/); moby contrib/dockerd-rootless.sh:L13-27, L125-168].

### 2.3 Q3: engine behaviors required for parity (ground truth)

| Area | Behavior | Source |
|---|---|---|
| States | created, running, paused, restarting, removing, exited, dead. Derived from flags that are not mutually exclusive. | [moby api/types/container/state.go:L12-18; daemon/container/state.go:L121-147] |
| Restart | Policies: no, always, unless-stopped, on-failure[:N]. Backoff starts at 100 ms, doubles, and is capped at 1 min; it resets if the run lasted ≥10 s. A manual stop suppresses restarts. | [moby daemon/internal/restartmanager/restartmanager.go:L11-15, L65-119] |
| Health | Defaults: interval 30 s, timeout 30 s, retries 3, start-period 0, start-interval 5 s. The probe is an exec. Output capped at 4,096 B; last 5 results kept. Emits `health_status: <s>`. | [moby daemon/health.go:L19-44, L195-282, L338-353] |
| json-file (default log driver) | JSON lines with `log`, `stream`, `time` (RFC3339Nano) and optional `attrs`. No rotation unless max-size is set; max-file defaults to 1; compress needs max-file ≥2. | [moby daemon/logger/jsonfilelog/jsonfilelog.go:L39-72, L127-142; daemon/logger/loggerutils/logfile.go:L167-297] |
| local log driver | Protobuf `LogEntry` with a big-endian u32 length before and after each entry. Defaults: 20 MiB × 5 files, compressed. | [moby daemon/logger/local/local.go:L28-30; doc.go:L3-8] |
| Log copier | 16 KiB buffer, with partial-line handling. In non-blocking mode a 1e6 B ring drops messages when full. | [moby daemon/logger/copier.go:L15-23; ring.go:L10-12, L174-177] |
| Exec | Env is the container env plus the request env. User and workdir default to the container's. Privileged exec gets all caps. Failures map to 126/127/128. Emits exec_create, exec_start and exec_die. | [moby daemon/exec.go:L96-157, L233-308; exec_linux.go:L44-96; monitor.go:L206-259] |
| Attach | Detach keys: ctrl-p,ctrl-q. Non-TTY output uses stdcopy frames: stream byte, three zero bytes, then a big-endian u32 length. The HTTP connection is hijacked with `101 UPGRADED` / `Upgrade: tcp`. | [moby daemon/internal/stream/attach.go:L14; api/pkg/stdcopy/stdcopy.go:L14-27; daemon/server/router/container/container_routes.go:L1139-1159] |
| Stop/kill | Signal is SIGTERM, or STOPSIGNAL / `--stop-signal`. Timeout is 10 s, overridden by the container config, then by the API; negative means wait forever. Then SIGKILL. `docker kill` sends SIGKILL. | [moby daemon/container/container.go:L56-57; daemon/config/config_linux.go:L40; daemon/stop.go:L56-118; daemon/kill.go:L36-40] |
| `--init` | Static tini v0.19.0 installed as `docker-init`, bind-mounted read-only at `/sbin/docker-init`. Args become `[init, --, cmd…]`. Only with a private PID namespace. tini reaps zombies, restores default signal behavior, propagates the exit code, and has a `-s` subreaper mode. | [moby daemon/oci_linux.go:L35, L726-746; Dockerfile:L292-317; tini README.md:L18-33, L125-141] |
| OOM | The shim watches `memory.events` `oom_kill` via inotify and raises TaskOOM. The engine sets OOMKilled and emits `oom`. `--oom-kill-disable` is dropped on v2. | [ctrd internal/oom/watcher.go:L126-139; moby daemon/monitor.go:L190-204; daemon/daemon_unix.go:L447-454] |
| Stats | 1 s collector, only for subscribed containers. `cpu.stat` usec ×1000; `memory.current`; `memory.max` (capped at host RAM); `memory.stat` passed through verbatim; failcnt from `memory.events` oom; `pids.*`; `io.stat` bytes. The CLI subtracts `inactive_file` from usage. | [moby daemon/daemon.go:L1139; daemon/stats_unix.go:L160-259; cli cli/command/container/stats_helpers.go:L254-264] |
| Events | create, start, die, kill, stop, oom, health_status, exec_*, attach, destroy, and others. Ring of 256 events. | [moby api/types/events/events.go:L24-98; daemon/events/events.go:L13-16] |
| /etc files | hosts, resolv.conf and hostname are generated per container and rbind-mounted rprivate (read-only only with a read-only rootfs). On user-defined networks, resolv.conf points at 127.0.0.11 with `ndots:0`, and the real servers become ExtServers. The default bridge has no embedded DNS. | [moby daemon/container/container_unix.go:L47-119; daemon/libnetwork/sandbox_dns_unix.go:L27, L303-341; daemon/network.go:L986-988] |
| Rootfs | v29 defaults to the containerd snapshotter (overlayfs). An `-init` layer adds `/.dockerenv`, `/etc/{hosts,hostname,resolv.conf}`, `/dev/{pts,shm,console}` and `/etc/mtab`. | [moby daemon/image_store_choice.go:L105-150; daemon/initlayer/setup_unix.go:L23-34] |
| Volumes | Image content is copied up only at create, only for `type=volume` with CopyData (the default; `nocopy` turns it off), and only into an *empty* volume. An image VOLUME becomes an anonymous volume. Local driver options: type, o, device, size. | [moby daemon/create_unix.go:L47-118; daemon/container/container_unix.go:L386-399; daemon/volume/local/local_unix.go:L23-33] |
| tmpfs / shm | `noexec,nosuid,nodev,rprivate` plus user options, with no default size. `/dev/shm` is 64 MiB. | [moby daemon/oci_linux.go:L522-535; daemon/config/config.go:L41-42] |
| sysctls | The daemon copies them without validation. The CLI allows `kernel.{msg*,sem,shm*}`, `net.*` and `fs.mqueue.*`. runc enforces namespacing. A private netns also gets `ip_unprivileged_port_start=0`. | [moby daemon/oci_linux.go:L759-769, L975-992; cli opts/opts.go:L252-281; runc libcontainer/configs/validate/validator.go:L218-275] |
| Networking | On user-defined bridges, DNS resolves container name, aliases, short ID and hostname. Resolver TTL 600, at most 3 upstreams. Publishing is DNAT plus one `docker-proxy` process per port (on by default). | [moby daemon/container_operations.go:L661-693; daemon/libnetwork/resolver.go:L54-60; daemon/config/config_linux.go:L161] |
| GPUs | `--gpus` becomes a DeviceRequest (all = −1). The engine tries CDI `nvidia.com/gpu=<id>` first, else a prestart hook. CDI is on by default and reads `/etc/cdi` and `/var/run/cdi`. CDI edits cover env, deviceNodes, mounts and hooks. | [cli opts/gpus.go:L21-24; moby daemon/devices_nvidia_linux.go:L38-79; daemon/cdi.go:L25-37; cdi SPEC.md:L221-252] |
| Compose | Implicit `<project>_default` network. `depends_on` conditions: `service_started`, `service_healthy`, `service_completed_successfully`, plus `required` and `restart`. Compose polls every 500 ms. Containers are named `project-service-N`. | [cs 06-networks.md:L62-63; 05-services.md:L409-423; compose pkg/compose/service_containers.go:L127-137, L200-233; compose-go v2.15.0 loader/normalize.go:L258-278] |
| Plumbing | moby → containerd (namespace `moby`) → `io.containerd.runc.v2` shim → runc. The shim groups containers by annotation and moby sets none, so each container gets its own shim (inferred from code). Shims are subreapers and survive a containerd restart. | [moby daemon/runtime_unix.go:L29-66; ctrd cmd/containerd-shim-runc-v2/manager/manager_linux.go:L64-70, L200-231; pkg/shim/shim_linux.go:L29-31; core/runtime/v2/README.md:L270-271] |

### 2.4 Q4: designs for sub-ms to few-ms start inside a pre-warmed VM

**What the evidence supports.**

- **Pools.**
  - Cgroup pools [Oakes18 §4.1; Li22 §4.4].
  - Pre-created network namespaces ("pause containers") cut total execution time of cold-start invocations by 75–80% at 50–100 concurrent starts [Mohan19 §6.1, Fig. 6]. Memory cost: 2 MB for 50 of them, versus 1,500–1,600 MB for pre-warmed or warm containers [Mohan19 §6.2, Table 2].
- **Skipping namespaces.** SOCK uses `chroot` with no mount or network namespace [Oakes18 §4.1]. Charliecloud uses only user and mount namespaces [Priedhorsky17 §2.5].
- **Zygotes.**
  - SOCK Zygotes gave 3× throughput [Oakes18 §4.2, Fig. 11; §5.1, Fig. 15]. The protocol:
    1. pass namespace and root fds to the zygote;
    2. `setns`, `fchdir`, `chroot`;
    3. fork again, because `setns` only partly applies to the caller;
    4. move the child into its cgroup.
  - Catalyzer's sandbox Zygote pre-parses config, pre-allocates resources and pre-mounts a base rootfs [Du20 §3.4].
  - A plain fork loses threads and any state derived from the pid [Du20 §4].
- **Snapshots.**
  - SEUSS deploys from a snapshot in under a millisecond [Cadden20 §7].
  - After restore, page faults account for 95% of processing time, and the pages touched are stable across runs. Prefetching them gave 3.7× [Ustiugov21 §1].

**Linux 7.x primitives** (none measured yet):

- `clone3` flags [k include/uapi/linux/sched.h:L36-42; kernel/fork.c:L2700-2711, L2985-2994; commits 12ae2c81b21c, 24baca56fafc, c8134b5f13ae]:
  - `CLONE_INTO_CGROUP`
  - `CLONE_PIDFD`
  - `CLONE_AUTOREAP`: no zombie; exit status comes from `PIDFD_GET_INFO`
  - `CLONE_NNP`
  - `CLONE_PIDFD_AUTOKILL`: SIGKILL when the pidfd closes. Its commit says it is "useful for container runtimes". It needs NNP or CAP_SYS_ADMIN.
  - `CLONE_EMPTY_MNTNS`
- `PIDFD_INFO_EXIT` and `PIDFD_INFO_CGROUPID` [k include/uapi/linux/pidfd.h:L25-30].
- `OPEN_TREE_NAMESPACE` (§2.1).

**Snapshotting with the engine resident.**

- Clones share RNG state, UUIDs and nonces [Brooker21-pre §1–2, preprint].
- **7.2:** vmgenid is exposed via ACPI `VMGENCTR` or device-tree `microsoft,vmgenid`. When it changes, the kernel calls `add_vmfork_randomness`, which reseeds the CRNG and bumps the generation counter; vDSO `getrandom` then re-keys [k drivers/virt/vmgenid.c:L28-35, L155-165; drivers/char/random.c:L264-283, L978-983; lib/vdso/getrandom.c:L137-157].
- No uevent is sent, so any RNG state cached in userspace is not refreshed.

### 2.5 Q5: memory

**The budget.**

- Firecracker's VMM costs ≈3 MB per VM [Agache20 §5.2, Fig. 7].
- Kata on Firecracker cost 94 MB per 128 MB container, falling to 71 MB at 1,000 VMs. The overhead is the guest OS, `struct page`, base OS, shimv2 and the agent [Li22 §3.2, Fig. 5].
- RunD got it under 20 MB [Li22 §5.3].
- The guest kernel's self-modifying code rewrote 7,928 KB of the 10,012 KB of text/rodata it touched, which breaks sharing; RunD fixes this with a pre-patched kernel image [Li22 §3.2, §4.3.2].

**Docker's process model.**

- dockerd plus containerd plus one shim per container, and each shim fork/execs runc [ctrd core/runtime/v2/README.md:L38-67].
- All of it is Go (moby builds with Go 1.26.8 [moby Dockerfile:L3]). The Go runtime forces a GC at least every 2 minutes, even when idle [golang/go go1.26.8 src/runtime/proc.go:L6476].
- No peer-reviewed figure for per-shim RSS was found (M6).

**Runtime internals.**

- runc:
  - re-execs itself from a sealed copy of `/proc/self/exe`. Sealing via memfd "adds around ~60% overhead during container startup"; the zero-copy overlayfs seal requires privilege [runc libcontainer/exeseal/cloned_binary_linux.go:L226-230; overlayfs_linux.go:L98];
  - splits create and start via `exec.fifo` [runc libcontainer/standard_init_linux.go:L260-269];
  - uses `CLONE_INTO_CGROUP` only for `runc exec` [runc libcontainer/process_linux.go:L369, L419-420, L824-827].
- crun calls the entrypoint in-process, with no re-exec, and uses `clone3` + `CLONE_INTO_CGROUP` for both run and exec [crun src/libcrun/linux.c:L6575-6596, L6703-6717, L7050-7078].
- youki goes main → intermediate → init, and calls `clone3` with no cgroup fd and no pidfd [youki docs/src/developer/youki.md:L11, L23; crates/libcontainer/src/process/fork.rs:L97-116].
- **Vendor claims, UNVERIFIED:**
  - crun ran 100 sequential `/bin/true` containers in 1.69 s versus 3.34 s for runc [crun README.md:L46-53].
  - The youki README (youki 0.3.3, runc 1.1.7, crun 1.15) reports create+start+delete at 111.5 ms (youki), 224.6 ms (runc) and 47.3 ms (crun) [youki README.md:L42-51].

**Kernel memory per container.** It is accounted in `memory.stat` under `kernel`, `percpu` and `sock` [k cgroup-v2.rst:L1550-1570]. No byte counts are published (M7).

**CoW sharing.**

- SOCK Zygotes keep "copy-on-write memory unbroken by any call to exec" [Oakes18 §4.2].
- Page sharing let SEUSS hold 54,000 instances, versus 3,000 Docker containers [Cadden20 §7, Table 3].
- Catalyzer's sfork lowered PSS [Du20 §6.5, Fig. 14].
- A restore touched 8–99 MB, versus 148–256 MB for a cold boot [Ustiugov21 §1].
- *Inference (not measured):* every write after restore makes a page private to that VM. So the periodic work Docker parity requires costs memory in every VM: the 1 s stats loop, exec-based health probes, log copying, and a GC if there is one.

### 2.6 Q6: rootless `docker build`

**Literature.**

- Charliecloud runs Docker images with only user and mount namespaces, no daemons, in about 800 lines of code. In 2017 its build step was still privileged and ran on user machines [Priedhorsky17 §2.5, §3.1, §3.3].
- SC'21 compares three options [Priedhorsky21 §2.3, §4.1, §5, §6.1–6.2]:
  - Fully unprivileged (Type III) builds fail on chown and setgroups.
  - Type II (rootless Podman via `newuidmap`) preserves ownership but trusts a privileged helper.
  - Type III with auto-injected `fakeroot(1)` works but flattens ownership.
- A preprint describes a seccomp filter that fakes success for privileged syscalls. It built "all Dockerfiles we examined" but gives zero consistency [Priedhorsky24-pre abstract, §5].

**BuildKit** (the architecture to reproduce).

- **LLB.**
  - A protobuf DAG; "LLB is to Dockerfile what LLVM IR is to C".
  - Op kinds: Exec, Source, File, Build, Merge, Diff, Passthrough.
  - ExecOp carries mounts, network mode (sandbox/host/none), security mode, and `cdiDevices`.
  - Per-step cgroup limits are excluded from cache keys.
  - Sources: [bk README.md:L181-185; solver/pb/ops.proto:L14-21, L44-91, L283-285].
- **Cache** [bk docs/dev/solver.md:L13-15, L138-153, L272-274]:
  - A key is `CacheMap.Digest` combined with the input keys. Root keys are a manifest digest or a git SHA.
  - Content-based keys use a per-input `Selector` plus a `ComputeDigestFunc` over the selected paths.
  - "Cache-fast" (definition) lookups run before "cache-slow" (content) lookups.
  - Hashing content after every op is called "too slow", so it is deferred.
- **Diffs.**
  - With the overlayfs snapshotter, the diff is the topmost upperdir, walked on its own [bk util/overlay/overlay_linux.go:L66-274; ctrd pkg/archive/tar.go:L561-567]:
    - a 0/0 char device becomes a delete;
    - opaque directories are re-walked;
    - `redirect_dir` is rejected;
    - deletes are written as explicit `.wh.<name>` entries.
  - OCI: generators SHOULD use explicit whiteouts; readers MUST also accept `.wh..wh..opq` [is layer.md:L248-315].
  - Kernel whiteout format [k overlayfs.rst:L140-165, L867-869]:
    - a whiteout is a 0/0 char device or a file with `trusted.overlay.whiteout`;
    - an opaque directory has `trusted.overlay.opaque=y`;
    - the prefix becomes `user.overlay.*` under `userxattr`.
  - Merge and Diff ops are lazy and hardlink rather than copy [bk docs/dev/merge-diff.md:L320-324, L541-546].
- **Rootless** [bk cmd/buildkitd/main_oci_worker.go:L210-211; docs/rootless.md:L6-13; util/rootless/specconv/specconv_linux.go:L10-44]:
  - needs RootlessKit and runs as mapped root;
  - uses overlayfs on kernel ≥5.11, else fuse-overlayfs;
  - network is always host;
  - `/sys` and all cgroup settings are stripped, so per-step limits don't work.
- **Reproducibility.** `SOURCE_DATE_EPOCH` sets the config and history `created` times. File times change only with `rewrite-timestamp=true`, which clamps them and never rewrites base layers [bk docs/build-repro.md:L43-78; util/converter/converter.go:L76-86; exporter/containerimage/writer.go:L465, L785-813].
- **`volatile` overlay mounts** skip all syncs, which suits build scratch space [k overlayfs.rst:L836-846].

## 3. Implications for shards (ranked)

**R1. Take the VM snapshot after the default compose project is up and healthy, and make the engine safe to snapshot.**
- *Why:* no runc-class start is anywhere near a few ms (§2.1 table). A template fork reaches 0.97 ms [Du20 §6.2]. After a restore, the cost is page faults on a stable set of pages [Ustiugov21 §1].
- *Benefit:* container creation is off the ≤5 ms path entirely.
- *Requirements and risks:*
  - Both VMM backends must expose vmgenid: device-tree `microsoft,vmgenid` on arm64/HVF, ACPI on x86.
  - The engine must draw container IDs, MACs and DNS IDs from `getrandom`/vDSO at the moment of use, never from a cached userspace CSPRNG [Brooker21-pre; k drivers/virt/vmgenid.c:L155-165; lib/vdso/getrandom.c:L137-157].
  - Timers will see a clock jump on restore (M10).
  - GPU state probably cannot be snapshotted (M11), so GPU containers need R3.

**R2. Run one resident Rust engine with the OCI runtime linked in. No containerd, no per-container shim, no runc re-exec.**
- *Why:*
  - OCI operations are not a CLI [rs runtime.md:L93-95].
  - Docker adds a shim process per container, which fork/execs runc [ctrd core/runtime/v2/README.md:L38-67].
  - runc's memfd seal costs "~60%" of startup [runc exeseal]. crun avoids re-exec entirely [crun linux.c:L6703-6717].
- *Design:*
  - Spawn from a small single-threaded zygote created at engine start, because fork is unsafe from a multithreaded parent [Baumann19 §4; Oakes18 §4.2].
  - Make the engine a subreaper, as the shims are [ctrd pkg/shim/shim_linux.go:L29-31].
  - Get exit status via `CLONE_PIDFD` + `CLONE_AUTOREAP` + `PIDFD_INFO_EXIT`.
  - Use `CLONE_PIDFD_AUTOKILL` only for execs and health probes.
- *Benefit:*
  - two fewer process creations per container;
  - no Go GC [go proc.go:L6476];
  - no per-container shim memory (M6).
- *Risk:* if the engine dies, supervision goes with it. Shims exist precisely so containers survive a daemon restart [ctrd core/runtime/v2/README.md:L270-271]. Mitigate by persisting state and re-attaching with `pidfd_open`.

**R3. For `docker run` after restore, combine pools that live in the snapshot with the 7.x `clone3` fast path.**
- *Pools:*
  - Network namespaces, pre-configured (veth, bridge port, IP) and joined through `namespaces[].path`. Network setup is 90% of Docker start [Mohan19 §4], and moby does it inside create→start [moby daemon/start_linux.go:L17-44].
  - Empty, pre-configured cgroups, entered with `CLONE_INTO_CGROUP`. Don't rely on rename, which v2 lacks. Keep pooled cgroups empty, because the spec lets a runtime reject a non-empty cgroup at create [rs config-linux.md:L294-308].
  - IPC and UTS namespaces.
- *Per container:*
  - A new PID namespace (they are single-use).
  - A mount namespace from `OPEN_TREE_NAMESPACE` (7.0) or `CLONE_EMPTY_MNTNS` (7.1), never a `copy_tree` of the engine's table [k fs/namespace.c:L4232-4300].
  - A seccomp program compiled before the snapshot.
- *Benefit:* the steps the literature found dominant disappear. Target ≤1 ms p50 (M1–M3).
- *Risk:*
  - `cgroup_mutex` is still taken [k kernel/cgroup/cgroup.c:L6756-6772].
  - Pools sit in every VM, though they stay CoW-shared until first touched (M7). Size them to the compose service count plus a few spares.

**R4. Rootfs: pre-mounted read-only lower layers, plus a per-container overlay upper on scratch with `volatile` (and `userxattr`).**
- *Why:* a volatile writable layer took rootfs preparation from 207 ms to 0.2 ms [Li22 §4.2]. `volatile` skips syncs [k overlayfs.rst:L836-846].
- *Design:*
  - Put the moby init-layer files in a shared lower [moby daemon/initlayer/setup_unix.go:L23-34].
  - Keep named volumes on persistent storage, with copy-up at create (§2.3).
- *Rootless:* store layers pre-shifted into the engine's ID range, or idmap them at template time. Idmapping needs CAP_SYS_ADMIN over the superblock [k fs/namespace.c:L4829-4831].

**R5. Memory discipline for CoW clones.**
- No GC.
- Periodic work must be event-driven:
  - compute stats only while someone subscribes, as moby does [moby daemon/stats/collector.go:L109-121];
  - accept that health probes are execs (Docker parity requires them);
  - keep log buffers bounded.
- Pre-size allocator arenas before the snapshot, and disable background purge/decay.
- Pre-patch the guest kernel image [Li22 §4.3.2].
- Verify all of this with `Private_Dirty` measurements (M4).

**R6. Rootless model: a Type II user namespace set up before the snapshot, a delegated cgroup subtree, and optionally a delegated BPF token.**
- *Design:*
  - The guest init writes a full `uid_map`/`gid_map` (≥65,536 IDs, as Docker expects) while it still holds CAP_SETUID in the parent namespace [man user_namespaces(7)]. runc takes the same direct-write path first [runc nsexec.c:L266-294].
  - The init then drops privilege. The engine runs as mapped root, as BuildKit does [bk main_oci_worker.go:L210-211].
  - Delegate every controller, not only memory and pids [k cgroup-v2.rst:L537-600].
- *Devices:* without a BPF token, device policy cannot be enforced; runc and youki silently skip it [runc devices/v2.go:L31-37]. The VM then becomes the only device boundary.
- **Conflict:** if "no root" also rules out any privileged step when the VM template is built, only Type III is possible. That means:
  - a single ID per container;
  - chown and setgroups fail during builds [Priedhorsky21 §2.3];
  - no device policy;
  - broken compatibility with many Docker images.

  This needs an explicit product decision.

**R7. Networking: reproduce user-defined-bridge semantics inside the VM, without `docker-proxy`.**
- Reproduce the 127.0.0.11 resolver, `ndots:0` and aliases (§2.3).
- Publish ports through the VMM instead of one proxy process per port [moby daemon/config/config_linux.go:L161].
- Keep traffic inside the VM un-NATed; Docker's NAT doubled round-trip latency [Felter14-RC §III.F].

**R8. GPUs: CDI first, with no hook binaries on the start path.**
- Resolve CDI specs (deviceNodes, mounts, env [cdi SPEC.md:L221-252]) at engine start or before the snapshot.
- moby's legacy GPU path runs a hook binary on every start [moby daemon/devices_nvidia_linux.go:L138-157].
- BuildKit ExecOp already carries `cdiDevices` [bk ops.proto:L44-51].
- Bind-mount the user-space driver libraries, as Charliecloud does [Priedhorsky17 §3.2.3].

**R9. Build: reimplement BuildKit's model on top of the same runtime.**
- Content-addressed LLB with lazy merge/diff.
- Diffs taken from the upperdir only, emitting explicit whiteouts.
- `SOURCE_DATE_EPOCH` clamping.
- With the delegated cgroup subtree from R6, per-step limits keep working, which rootless BuildKit loses [bk util/rootless/specconv/specconv_linux.go:L43-44].
- The Type III fallback (fakeroot or seccomp emulation) loses ownership [Priedhorsky21 §5; Priedhorsky24-pre].

**R10. Implement the §2.3 semantics exactly**, including every default, compose's 500 ms dependency polling, and `project-service-N` naming.

## 4. Open questions needing our own measurement

All measurements run on the 7.2 guest on HVF arm64 and on KVM x86. Report p50, p99 and CPU time over at least 1,000 iterations.

| ID | Question | Method | Decides |
|---|---|---|---|
| M1 | Namespace create/destroy cost | Time `clone3`/`unshare`/`setns` per namespace type at concurrency 1–32. For mount namespaces, compare `CLONE_NEWNS`, `OPEN_TREE_NAMESPACE` and `CLONE_EMPTY_MNTNS`. Trace with ftrace function_graph on `copy_net_ns`, `setup_net`, `cleanup_net`, `copy_mnt_ns`. Compare a minimal-pernet kernel config against a distro config. | Which namespaces to pool (R3); kernel config |
| M2 | cgroup costs | Compare mkdir, pool reuse, `CLONE_INTO_CGROUP` and a `cgroup.procs` write with memory, pids, cpu and io controllers on. Record the `memory.stat` `percpu`/`kernel` delta [k cgroup-v2.rst:L1550-1568]. | Pool size; controller set |
| M3 | Critical path of `docker run` after restore | eBPF timestamps from API accept to the entrypoint's `sched_process_exec`, with and without pools | R3 target: ≤1 ms p50, ≤2 ms p99 |
| M4 | CoW pages dirtied | After restore: idle for 60 s, then per `run`, `exec`, health probe and stats-second. Measure `smaps_rollup` `Private_Dirty` in the guest and PSS on the host. Compare system, jemalloc and mimalloc allocators. | Allocator and polling design (R5) |
| M5 | seccomp attach cost | Docker's default profile with JIT on and off, versus inheriting the filter from a pre-filtered spawner [k kernel/fork.c:L1777-1790] | Spawner design |
| M6 | Docker's userspace cost per container | PSS of each `containerd-shim-runc-v2`, plus idle dockerd and containerd, on the same guest | Size of R2's win |
| M7 | Kernel memory per pooled object | `memory.stat` and `slabinfo` deltas per netns, cgroup, pty and overlay mount, before and after first use in a clone | Pool sizing |
| M8 | Rootfs backends | Overlay upper on tmpfs with `volatile`, over EROFS vs virtio-fs (DAX) vs virtio-blk lowers. Measure mount latency, first-exec faults and host page-cache sharing. | R4 transport |
| M9 | Uniqueness after restore | Restore 1,000 clones and diff IDs, MACs, DNS IDs and `getrandom` output. Verify vmgenid is delivered via device tree on HVF and via ACPI on KVM. | R1 gate |
| M10 | Clock jump after restore | Snapshot mid health-interval, restore hours later, and count probe bursts and restart-backoff anomalies | Timer semantics |
| M11 | GPU with snapshots | CDI injection latency; whether any driver state can be snapshotted; cost of attaching after restore | Whether GPU services go in the snapshot |
| M12 | Rootless build fidelity | Build the top-N Dockerfiles as Type II vs Type III with seccomp emulation. Compare digests, ownership and failure rate. | The R6 conflict |

## 5. References

All papers were retrieved on 2026-09-28 (curl/WebFetch) and read in full via pdftotext. Section, figure and table numbers refer to those texts.

**Peer-reviewed**
- [Agache20] A. Agache, M. Brooker, A. Florescu, A. Iordache, A. Liguori, R. Neugebauer, P. Piwonka, D.-M. Popa. *Firecracker: Lightweight Virtualization for Serverless Applications.* NSDI '20. https://www.usenix.org/system/files/nsdi20-paper-agache.pdf
- [Baumann19] A. Baumann, J. Appavoo, O. Krieger, T. Roscoe. *A fork() in the road.* HotOS '19.
- [Cadden20] J. Cadden, T. Unger, Y. Awad, H. Dong, O. Krieger, J. Appavoo. *SEUSS: Skip Redundant Paths to Make Serverless Fast.* EuroSys '20, pp. 1–15. doi:10.1145/3342195.3392698
- [Du20] D. Du, T. Yu, Y. Xia, B. Zang, G. Yan, C. Qin, Q. Wu, H. Chen. *Catalyzer: Sub-millisecond Startup for Serverless Computing with Initialization-less Booting.* ASPLOS '20, pp. 467–481. doi:10.1145/3373376.3378512
- [Espe20] L. Espe, A. Jindal, V. Podolskiy, M. Gerndt. *Performance Evaluation of Container Runtimes.* CLOSER 2020, pp. 273–281. doi:10.5220/0009340402730281
- [Felter15] W. Felter, A. Ferreira, R. Rajamony, J. Rubio. *An updated performance comparison of virtual machines and Linux containers.* ISPASS 2015, pp. 171–172. doi:10.1109/ISPASS.2015.7095802
  - The ISPASS paper is only 2 pages. The numbers come from the full version, [Felter14-RC]: IBM Research Report RC25482, 21 Jul 2014. That report is *not peer-reviewed*; it was retrieved via web.archive.org.
- [Li22] Z. Li, J. Cheng, Q. Chen, E. Guan, Z. Bian, Y. Tao, B. Zha, Q. Wang, W. Han, M. Guo. *RunD: A Lightweight Secure Container Runtime for High-density Deployment and High-concurrency Startup in Serverless Computing.* USENIX ATC '22.
- [Manco17] F. Manco, C. Lupu, F. Schmidt, J. Mendes, S. Kuenzer, S. Sati, K. Yasukata, C. Raiciu, F. Huici. *My VM is Lighter (and Safer) than your Container.* SOSP '17, pp. 218–233. doi:10.1145/3132747.3132763
- [Mohan19] A. Mohan, H. Sane, K. Doshi, S. Edupuganti, N. Nayak, V. Sukhomlinov. *Agile Cold Starts for Scalable Serverless.* HotCloud '19.
- [Oakes18] E. Oakes, L. Yang, D. Zhou, K. Houck, T. Harter, A. C. Arpaci-Dusseau, R. H. Arpaci-Dusseau. *SOCK: Rapid Task Provisioning with Serverless-Optimized Containers.* USENIX ATC '18.
- [Priedhorsky17] R. Priedhorsky, T. Randles. *Charliecloud: Unprivileged Containers for User-Defined Software Stacks in HPC.* SC17. doi:10.1145/3126908.3126925
- [Priedhorsky21] R. Priedhorsky, R. S. Canon, T. Randles, A. J. Younge. *Minimizing Privilege for Building HPC Containers.* SC '21, pp. 1–14. doi:10.1145/3458817.3476187
  - Read from the authors' pre-print, arXiv:2104.07508v3; section numbers are the pre-print's.
- [Ustiugov21] D. Ustiugov, P. Petrov, M. Kogias, E. Bugnion, B. Grot. *Benchmarking, Analysis, and Optimization of Serverless Function Snapshots.* ASPLOS '21, pp. 559–572. doi:10.1145/3445814.3446714
  - Read from arXiv:2101.09355v3.
- [Zhao21] K. Zhao, S. Gong, P. Fonseca. *On-demand-fork: A Microsecond Fork for Memory-Intensive and Latency-Sensitive Applications.* EuroSys '21, pp. 540–555. doi:10.1145/3447786.3456258

**Preprints (not peer-reviewed)**
- [Brooker21-pre] M. Brooker, A. C. Catangiu, M. Danilov, A. Graf, C. MacCarthaigh, A. Sandu. *Restoring Uniqueness in MicroVM Snapshots.* arXiv:2102.12892v1.
- [Priedhorsky24-pre] R. Priedhorsky, M. Jennings, M. Phinney. *Zero-consistency root emulation for unprivileged container image build.* arXiv:2405.06085v1.

**Specifications and official documentation**
- OCI Runtime Spec v1.3.0 (92249139ee): runtime.md, runtime-linux.md, config.md, config-linux.md.
- OCI Image Spec v1.1.1 (147f9c13ce): layer.md.
- Compose Spec, compose-spec/compose-spec@914ec15d1f: 05-services.md, 06-networks.md.
- Container Device Interface v1.1.1 (cb98bcf66d): SPEC.md.
- Linux v7.2-rc4 (1590cf0329):
  - Documentation/admin-guide/cgroup-v2.rst
  - Documentation/filesystems/overlayfs.rst
  - commits 9b8a0ba68246, 9d4e752a24f7, 12ae2c81b21c, 24baca56fafc, c8134b5f13ae
- man-pages 6.19: clone(2), user_namespaces(7) (man7.org).
- docs.docker.com/engine/security/rootless/ (+ /tips/, /troubleshoot/), retrieved 2026-09-28. These were fetched through WebFetch, whose summarizing model makes the quotes near-verbatim rather than exact.

**Source code** (paths and lines are cited inline)
- moby/moby docker-v29.8.1 (464cd50c3d9e)
- docker/cli v29.8.1 (4a63305d7433)
- docker/compose v5.5.1 (5f94fb0aa42a)
- compose-spec/compose-go v2.15.0 (4ddbf11f8bd3)
- containerd v2.4.1 (f2551031d727)
- runc v1.5.2 (29dd3dc2b13b)
- crun 1.30.1 (079ff6a7a16d)
- youki v0.7.0 (94ba653efbb1)
- buildkit v0.33.0 (dddd5621af04)
- tini v0.19.0 (de40ad007797)
- golang/go go1.26.8 (src/runtime/proc.go)
