# Warm-pool daemon: pooled VMM processes, auto-started daemons, descriptor handoff

Research note, 2026-09-29. Evidence only; nothing here is a final design decision.

It informs D26 (docs/design/architecture.md), a warm VM process that serves one request, and D26's next steps:

- a per-user daemon with pools per template, refilled after each run;
- auto-start and idle exit;
- `shards run` as a thin client of the daemon.

Pinned sources (citation paths are relative to each repository; abbreviations as listed):

| Tag | Source |
|---|---|
| `kata` | kata-containers 4.2.0, `c7351e797eff`. `vc/` = `src/runtime/virtcontainers/` |
| `fc` | firecracker `edb60617c31e` (as in boot-latency.md) |
| `fcctr` | firecracker-containerd `be68640a5d22` (main, 2026-07-16; the repository has no release tags) |
| `bazel` | bazel 9.2.0, `8220c6198837`. `cpp/` = `src/main/cpp/`, `srv/` = `src/main/java/com/google/devtools/build/lib/server/`, `tools/` = `src/main/tools/`, `pb/` = `src/main/protobuf/` |
| `gradle` | gradle v9.8.0, `a927be5e08ef`. `CS/` = `platforms/core-runtime/client-services/src/main/java/org/gradle/launcher/daemon/`, `LA/` = `platforms/core-runtime/launcher/src/main/java/org/gradle/launcher/daemon/`, `DS/` = `platforms/core-runtime/daemon-server/src/main/java/org/gradle/launcher/daemon/`, `DP/` = `platforms/core-runtime/daemon-protocol/src/main/java/org/gradle/launcher/daemon/`, `PC/` = `platforms/core-execution/persistent-cache/src/main/java/org/gradle/cache/internal/`, `DOC` = `platforms/documentation/docs/src/docs/userguide/reference/runtime-configuration/gradle_daemon.adoc` |
| `sccache` | sccache v0.18.0, `01c35e69b97f` |
| `aosp` | Android `platform/frameworks/base` android-17.0.0_r1, `94b4c163b7df`. `os/` = `core/java/com/android/internal/os/` |
| `xnu` | xnu-12377.101.15, `5c306bec31e3`: the kernel of the macOS 26.4.1 host |
| `libc` | Apple Libc-1752.100.10, `71bbe350ab79`: macOS 26.4's, per Apple's `distribution-macOS` tag `macos-264` |
| `linux` | Linux v7.2-rc4, `1590cf032971` |
| `man-pages` | Linux man-pages 6.19, `adb436b2e447` |
| `systemd` | systemd v262, `8cc40e0c5e92` |
| `cli`, `moby` | docker/cli v29.8.1 `4a63305d7433`, moby docker-v29.8.1 `464cd50c3d9e` (as in container-engine-internals.md) |

macOS man pages are those installed on the 26.4.1 (25E253) host and in its MacOSX26.4 SDK:

- `launchd.plist(5)`, `launch_activate_socket(3)`, `launchctl(1)`, `launchd(8)`;
- `unix(4)`, `bind(2)`, `getpeereid(3)`, `confstr(3)`, `flock(2)`, `posix_spawnattr_setflags(3)`.

They are cited by page and section.

Markers:

- **(derived)**: our arithmetic on cited numbers.
- **(inference)**: reasoning from code or docs, not stated there.
- **(measured, this host)**: read on the M5 Max.
- **(probe)**: a behavior also checked once, pass/fail, by a C probe on this host and on Linux 6.12 aarch64 (glibc 2.41 and musl 1.2.5). The probe is not yet committed (E10).
- **UNVERIFIED**: no acceptable source found.

## 1. Scope

- **Q1.** Kata Containers' VMCache and VM templating: how VMs are pre-created, cached and handed to a client, what crosses the socket, and their lifecycle, sizing, failures, security and limitations.
- **Q2.** Firecracker and firecracker-containerd: official guidance or code for pre-warmed or pooled microVMs.
- **Q3.** Papers: LightVM's VM shells, Virtines' pool, Catalyzer's Zygotes and `sfork`. Also one production pool of pre-spawned processes, Android's USAP pool.
- **Q4.** Auto-started client/server tools (Bazel, Gradle, sccache): auto-start, races, handshake and versions, idle timeout, stale state.
- **Q5.** OS-native activation (launchd `Sockets`, systemd user socket units): what each asks of an installer, and whether a downloaded binary can use it.
- **Q6.** `SCM_RIGHTS` on XNU and Linux: limits, passable types, close-on-exec, peer credentials, `sun_path`, socket locations.
- **Q7.** `docker run` for a client whose workload runs elsewhere: attach, `--sig-proxy`, disconnect, `--rm`, TTY. This extends container-engine-internals.md §2.3.

## 2. Findings

### 2.1 Q1: Kata Containers VMCache and VM templating

Kata's Go runtime has two factories:

- **VMCache** keeps booted, paused VMs in a separate server process and hands one to each new sandbox.
- **Templating** clones each new VM from a saved template.

Both are QEMU-centric and Linux-only: "VM factory is unsupported on Darwin" [kata: vc/factory/factory_darwin.go:16].

Short names: "VMCache doc" is `docs/how-to/what-is-vm-cache-and-how-do-I-use-it.md`, and "templating doc" is `docs/how-to/what-is-vm-templating-and-how-do-I-use-it.md`.

**VMCache: how a VM is made and pooled**

- **The server.** `kata-runtime factory init` runs in the foreground when `vm_cache_number > 0`. It serves on `vm_cache_endpoint`, by default `/var/run/kata-containers/cache.sock` [kata: docs/how-to/what-is-vm-cache-and-how-do-I-use-it.md:7-15, 24-38; src/runtime/config/configuration-qemu.toml.in:514-536].
- **A pooled VM** is fully booted, its agent has passed a health check, and its vCPUs are paused with QMP `stop` [kata: vc/factory/direct/direct.go:32-45; vc/vm.go:86-167; vc/qemu.go:1990-2002].
- **The pool** is one unbuffered channel fed by N goroutines [kata: vc/factory/cache/cache.go:34-76].
  - Each goroutine builds a VM, blocks until a client takes it, then builds the next.
  - So an idle pool holds N paused VMs. Refill starts the moment one is taken, with no backoff. A request on an empty pool waits with no timeout (inference).
- **One failure closes the whole pool.** A single build error in any goroutine calls `CloseFactory`, which stops every pooled VM. Every later request fails with "cache factory is closed" until the server is restarted [cache.go:51-56, 112-130].

**VMCache: what crosses the socket**

- **The protocol.** gRPC over the Unix socket, with four RPCs (`Config`, `GetBaseVM`, `Status`, `Quit`), all taking `Empty`, so a request cannot say what it needs [kata: src/runtime/protocols/cache/cache.proto:15-52].
  - The client calls `Config` once to fetch the server's VM configuration for its compatibility check [kata: vc/factory/grpccache/grpccache.go:28-60].
- **A handout is a description, not a descriptor.** `GetBaseVM` returns `GrpcVM{id, hypervisor JSON, cpu, memory, cpuDelta}` [vc/vm.go:370-385].
  - For QEMU, the JSON holds the QMP socket path, hypervisor state, nvdimm count and SMP.
  - Before sending it, the server drops its own QMP connection and descriptors [vc/qemu.go:3677-3691].
  - No file descriptors cross the socket [cache.proto:22-52].
- **QEMU stays the server's child.** It runs without `-daemonize`, and a server goroutine waits on it (inference from [vc/qemu.go:1772-1780, 1680-1698]). On shutdown the server stops only the VMs still pooled [kata: src/runtime/cmd/kata-runtime/factory.go:176-180; cache.go:59-71].
- **Adoption.** The client rebuilds a VM object from the description [vc/vm.go:169-210] and checks compatibility. Then it [kata: vc/factory/factory_linux.go:110-185]:
  1. resumes the VM;
  2. reseeds its RNG "so that shared memory VMs do not generate same random numbers";
  3. syncs its clock;
  4. hot-adds vCPUs and memory if the pooled VM is smaller than requested.

  It then symlinks the sandbox's directories to the VM's [vc/vm.go:330-368].
- **The description loses state.** `fromGrpc` does not restore the QEMU pid-file path, so the adopting client's `GetPids` finds no PID [vc/qemu.go:3613-3618, 3651-3675]. No end-to-end test with a real hypervisor exists, so the effect is UNVERIFIED.
- **Deprecated.** "The **VMCache** feature is **deprecated** and is not implemented in `runtime-rs`" [kata: docs/migrating-config-go-runtime-to-runtime-rs.md:85-86].

**VMCache: fallback, failures, security, limitations**

- **Fallback to direct boot.**
  - If the factory cannot load, for example because the cache server is unreachable, the shim "will use direct boot" [kata: src/runtime/pkg/katautils/create.go:76-91].
  - If the configurations differ, the client logs "fallback to direct factory vm" and boots a VM itself [factory_linux.go:118-125]. The comparison covers the whole hypervisor and agent configuration, after CPU, memory and per-VM paths are zeroed [factory_linux.go:73-117].
- **No fallback later.** A `GetBaseVM` error after that fails the sandbox, with no retry [grpccache.go:53-60].
- **Stale socket.** A leftover socket blocks restart: "already exist. Please stop running VMCache server and remove" [factory.go:104-114].
- **Access control is the socket's mode alone.** The socket is chmod 0600 after `Listen`, inside a 0755 directory. gRPC runs with insecure credentials, and the handlers check nothing [factory.go:104-124, 182-186; grpccache.go:29]. Anyone who can open the socket can drain the pool, list PIDs or `Quit` the server (inference).
- **Documented limitations:** "Cannot work with VM templating." and "Only supports the QEMU hypervisor." [VMCache doc:40-42]. Only the hypervisor rule is enforced in code [kata: src/runtime/pkg/katautils/config.go:2148-2157]. No VMCache measurements are published.

**Templating**

- **The template.** `factory init` mounts a tmpfs at `template_path` (default `/run/vc/vm/template`): mode 0700, `nosuid,nodev`. Its size is guest RAM plus a margin for device state, 8 MiB, or 300 MiB on arm64 [kata: vc/factory/template/template_linux.go:100-129; template_arm64.go:9-14].
  - It boots a VM whose RAM is a `memory-backend-file` with `share=on`.
  - It disconnects the agent and sleeps 2 s so the agent listens again. Then it pauses and saves device state by `x-ignore-shared` migration to a file.
  - Then it stops the VM, so the template is files only [template_linux.go:131-168; vc/qemu.go:1043-1060, 3027-3078].
- **A clone** starts QEMU with `-S -incoming defer` on the same memory file without `share`, a `MAP_PRIVATE` mapping. It loads the device state and waits, paused, for `GetVM` [vc/qemu.go:1787-1817; vc/vm.go:149-157]. Each clone gets its own UUID and vsock CID [vc/vm.go:102, 123-135].
- **Limits.**
  - arm64 does not use `x-ignore-shared` ("not support in arm64 for now"), so its whole RAM goes through the state stream [kata: vc/qemu_arm64.go:112-115] (inference).
  - Templating requires `initrd` and rejects virtio-fs [templating doc:47-54; vc/qemu.go:1195-1200].
- **A stale template goes undetected.** `Fetch` checks only that the files exist, then builds the factory from the caller's own configuration: "TODO: save template metadata and fetch from storage" [template_linux.go:30-41, 183-191].
- **Security.** Clones share memory: "If you care about such attack vector, do not use VM templating or KSM" (CVE-2015-2877) [kata: docs/how-to/what-is-vm-templating-and-how-do-I-use-it.md:36-43]. The RNG is reseeded after `cont`, so a clone briefly runs with the template's RNG state (inference, [factory_linux.go:142-157]).
- **Claimed gains** (from external links we did not check): 9 GB less memory for 100 containers ("about 72%" of their guest memory), and creation up to 38.68% faster [templating doc:24-32].
- **runtime-rs** implements templating only, with no pool [kata: src/libs/kata-types/src/config/hypervisor/mod.rs:1661-1674].
  - It reloads the default configuration for each clone, with no compatibility check [kata: src/runtime-rs/crates/runtimes/virt_container/src/lib.rs:146-156, 201-223].
  - Its QEMU path allows 280 ms for the incoming migration: "we need more empirical data" [kata: src/runtime-rs/crates/hypervisor/src/qemu/inner.rs:587-590].

### 2.2 Q2: Firecracker and firecracker-containerd

**Firecracker documents the parts of a pool, not a pool.**

- **Loading.** A snapshot is loaded "in a different Firecracker process" [fc: docs/snapshotting/snapshot-support.md:49-50], and only before the microVM is configured: only the logger and metrics may be set first [ibid.:180, 420-422].
- **Load now, run later.** The loaded microVM "is now in the `Paused` state", and `resume_vm` resumes it at once [ibid.:501-502, 512-513]. So a process can be spawned, loaded and paused before a request and resumed on it (inference). This is what shards' `--hold` does.
- **A failed load ends the process:** "the current Firecracker process is ended (as it might be in an invalid state)" [ibid.:514-515]. A pool must count a failed restore as a dead worker, not retry it.
- **Memory** is a `MAP_PRIVATE` mapping of the snapshot file, loaded on demand. The file must stay in place for the clone's lifetime [ibid.:78-87].
- **Host resources.** Every clone needs the original's host resources (disks, TAPs, the vsock socket) "at the same relative paths" [ibid.:487-492].
  - The network doc gives each clone its own network namespace [fc: docs/snapshotting/network-for-clones.md:17-19, 34-37].
  - It renames TAPs with `network_overrides` "when the tap device comes from a pool of such devices" [ibid.:148-155].
- **Measured restore.** Host console logging "degraded the snapshot restore time from 3ms to 8.5ms on `aarch64`" [fc: docs/prod-host-setup.md:63-67].
- **The restore benchmark** spawns a new process per restore and times only `load_snapshot`, so spawn cost is excluded [fc: tests/integration_tests/performance/test_snapshot.py:132-139; tests/framework/microvm.py:1342-1368].

**Lambda's pool.** This comes from the Firecracker paper, not from Firecracker's code.

- The MicroManager "keeps a small pool of pre-booted MicroVMs, ready to be used when Placement requests a new slot" [Agache20 §4.1.2].
  - "The required mean pool size can be calculated with Little's law": creation rate × creation latency. At 125 ms, that is one pooled VM per 8 creations/s.
  - boot-latency.md cites this passage as §4.1.1; it is in §4.1.2.
- Slots are "only ever used for a single function, and a single concurrent invocation of that function". They are reused for serial invocations: the MicroVM and the function's process both persist [Agache20 §4.1.1].
- Slots move Init → Idle ⇄ Busy → Dead. An idle slot still holds memory [Agache20 §4.2, Fig. 4]. There is one Firecracker process per MicroVM [Agache20 §4.1.2].

**firecracker-containerd has no VM pool either.**

- **VMs are pre-created explicitly.** The orchestrator calls `CreateVM` with a `vm_id`, then creates containers that name it [fcctr: docs/shim-design.md:244-258]. With no `vm_id`, the shim creates a default VM on the request path [ibid.:278-282].
- **Binding is the lock.** `CreateVM` decides whether a VM exists by listening on an abstract socket named after it: "If we get EADDRINUSE, then we assume there is already a shim" [fcctr: firecracker-control/local.go:113-125]. The design doc proposed `open(2)` with `O_EXCL` on `shim.pid` for the same race [fcctr: docs/shim-design.md:157].
- **The listener is handed to the child.** The parent binds the shim's sockets, then starts the shim with them as inherited descriptors (`cmd.ExtraFiles`) in its own process group. The socket therefore exists before the child runs [fcctr: firecracker-control/local.go:476-501].
- **Path length.** A comment warns that the shim's working directory sets the length of its relative socket paths, a "relatively low limit (usually 108 chars)" [fcctr: firecracker-control/local.go:468-473].

### 2.3 Q3: Papers, and one production pool

**LightVM's split toolstack** [Manco17 §5.2, Fig. 8]

- Much of VM creation "does not actually need to run at VM creation time": it is common to all VMs, or to VMs with similar configurations. There are few of those, "similar to OpenStack's flavors".
- **Prepare phase** (the `chaos` daemon, in the background): hypervisor reservation (domain ID and management information), compute allocation, memory reservation, memory preparation, device pre-creation. The daemon "generates a number of VM shells and places them in a pool" and keeps "a certain (configurable) number of shells available".
- **Execute phase** (per request): `chaos` parses the configuration and "asks for a shell fitting the VM requirements, which is then removed from the pool". It then initializes devices, builds the image (loads the kernel) and boots.
- **Gains** [Manco17 §6.1, Fig. 9; §7.4]:
  - chaos alone: 15–80 ms per VM.
  - With the split toolstack: at most ≈25 ms for the last of 1,000 VMs.
  - With every optimization: 4 ms, 4.1 ms at the 1,000th VM.
  - Compute service: noxs creation rose from ≈2.8 to ≈3.5 ms under load, while "the split toolstack and its pre-created domains takes a nearly constant 1.3 ms regardless of the number of already-created domains".
- Pre-creation "is independent of the underlying hypervisor technology" [Manco17 §9]. LightVM shares no pages between VMs [ibid.].

**Virtines' shell pool** [Wanninger22 §5.2, Figs. 6, 8]

- Wasp keeps "a pool of cached, uninitialized, virtines (shells)". A cold system pays `KVM_CREATE_VM` and the kernel's VMCS/VMCB allocation.
- **Cleaning.** A returned virtine's context is cleared, "preventing information leakage", and the shell goes back to a pool of "clean" virtines. "Wasp+CA" cleans asynchronously, from a background thread or when idle.
- **Gain.** Caching "brings the cost of provisioning a virtine shell to within 4% of a bare vmrun".
- **Snapshots.** Snapshots taken after initialization let later executions skip it. But the saved state "is exposed to all future virtines", so what it holds must be chosen with care.

**Catalyzer's Zygotes and `sfork`** [Du20]

- **Zygote cache (§3.4).** Construction cannot simply be cached, because it depends on function details (e.g. the rootfs path) and on resources that are not reusable. Catalyzer therefore separates a base configuration and rootfs. It caches Zygotes by "parsing the base configuration file, allocating virtualization resources (e.g., VCPU) and mounting the base rootfs", then specializes one per invocation.
- **`sfork` (§4).** A per-function "template sandbox" holds "clean system state at the func-entry point" and "no information about user requests". Each request sforks it.
  - Fork boot is faster than warm boot but costs more memory; "thus, fork boot is more suitable for frequently invoked (hot) functions" [§2.2].
- **Numbers** [§6.2, Fig. 11; §6.5, Table 3]:
  - sfork: 0.97 ms (C-hello);
  - Zygote boots: 5–14 ms;
  - restore without a Zygote: about 30 ms more;
  - a SPECjbb template sandbox: more than 200 MB.
- **The tail.** "Caching can not help reduce tail latency, which is dominated by the 'cache miss boot'" [§6.9].
- **Security.** The shared base mapping holds only request-independent state. The loss of ASLR "can be mitigated by periodically updating func-images and template sandboxes" [§6.8].
- Catalyzer is gVisor-based, not a hardware VM (boot-latency §2.5).

**Android's USAP pool: a production pool of pre-spawned processes.** Its members are processes, not VMs, but the shape is shards': made before any request, each claimed by one request, then replaced.

- **Claiming.** Idle members wait in `accept()` on one shared pool socket, at raised priority. The first to accept a connection takes the request.
  - It reads the caller's peer credentials and applies the UID policy.
  - It blocks SIGTERM, so flushing the pool cannot kill it while it holds a claim [aosp: os/Zygote.java:776-797].
- **Refill.** A claimed member reports its PID to the zygote.
  - Below the minimum, the zygote refills to the minimum at once.
  - Otherwise, once the pool is `threshold` below its maximum, it tops up to the maximum only after the refill delay has passed since the last claim. Each claim restarts the delay [aosp: os/ZygoteServer.java:114-124, 312-335, 457-498, 624-655].
- **Defaults.** Min 1, max 3 (limit 100), threshold 1, delay 3,000 ms. The pool is off by default [aosp: os/ZygoteConfig.java:30-76].
- **Fallback.** The client tries the pool and falls back to an ordinary zygote fork on any I/O error [aosp: core/java/android/os/ZygoteProcess.java:416-427].

### 2.4 Q4: Auto-started daemons: Bazel, Gradle, sccache

All three start their server on demand with no installer. They talk over TCP loopback, and none passes descriptors.

**Bazel 9.2.0**

- **Start.** "If the client cannot find a running server instance, it starts a new one" [bazel: site/en/run/client-server.md:17-25].
  - The client `posix_spawn`s a `daemonize` helper and waits for it, which "guarantees that the pid file exists" [bazel: cpp/blaze_util_posix.cc:402-431, 462-483].
  - The helper forks. The child ignores SIGHUP, calls `setsid()`, sends stdio to `jvm.out` and execs the JVM [bazel: tools/daemonize.cc:241-305].
  - The server binds, then publishes its connection file by write-then-rename "so the user never sees incomplete contents" [bazel: srv/GrpcServerImpl.java:437-525].
- **Readiness is polled.** Every 100 ms the client reads the file, verifies the PID and sends a gRPC `Ping`, for up to `--local_startup_timeout_secs` (120 s) [bazel: cpp/blaze.cc:838-894, 1711-1786].
  - An inherited socketpair end tells the client at once if the server died while starting [cpp/blaze_util_posix.cc:357-390, 433-450].
- **Races: locks.** "First the client acquires filesystem locks on the install and output bases": shared on the install base, exclusive on the output base. It "will busy-wait until both locks are available" unless `--noblock_for_lock` is set [cpp/blaze.cc:96-102].
  - The lock is a one-byte fcntl lock, preferring `F_OFD_SETLK`, because "POSIX locks can be lost 'accidentally' due to any close() on the lock file, and are not reliably preserved across execve()" [cpp/blaze_util_posix.cc:615-648].
  - After locking, the client checks that the lock file was not unlinked meanwhile. It writes its PID into the file for the "Another command holds the … lock" message [cpp/blaze_util_posix.cc:650-657, 672-752].
  - The lock is taken on every invocation, warm or cold. It is held through the version check and any server start, until the Run RPC is issued [cpp/blaze.cc:1995-2006].
- **Authentication.**
  - The server directory is 0700: "The server dir has the connection info - don't allow access by other users" [cpp/blaze.cc:668-675].
  - Two random 16-byte cookies authenticate each direction, compared in constant time: "a rudimentary form of mutual authentication" [bazel: pb/command_server.proto:33-35; srv/GrpcServerImpl.java:724-733].
- **Version.** The version is the install base's md5, a content hash of the binary. On a mismatch the client kills the old server [cpp/blaze.cc:1125-1172]: "the server is stopped and a new one started" [client-server.md:56-58].
  - Differing startup options also restart it, except volatile ones such as `--max_idle_secs` [cpp/blaze.cc:978-1123].
- **Idle.** `--max_idle_secs` defaults to 10800 (3 hours), and "system sleep time … is counted as idle time" [bazel: site/en/docs/user-manual.md:2396-2406].
  - Idle means no command running, checked every 5 s [bazel: srv/ServerWatcherRunnable.java:167-204].
  - An opt-in low-memory exit applies after 5 minutes idle [ibid.:38-41, 95-133].
  - The server shuts itself down at once if its PID file changes: "Someone overwrote the PID file. Maybe it's another server" [bazel: srv/PidFileWatcher.java:105-137].
- **Stale state.** The PID file is written before the server listens. A client kills the process it names when the server does not answer [cpp/blaze.cc:128-172, 896-947].
  - PID reuse is guarded by the process start time on Linux and Windows.
  - On macOS only existence is checked, which "might accidentally kill an unrelated process if the server died and the PID got reused" [bazel: cpp/blaze_util_linux.cc:161-226; cpp/blaze_util_darwin.cc:247-252].
  - A stop asks the server to shut down and waits up to 60 s, then sends `killpg(SIGKILL)` [cpp/blaze.cc:1879-1949; cpp/blaze_util_posix.cc:758-773].
- **stdio, signals, exit.**
  - stdout and stderr stream as bytes in `RunResponse` chunks, with flow control. There is no stdin and no descriptor passing [pb/command_server.proto:143-147; srv/GrpcServerImpl.java:149-199].
  - Ctrl-C becomes a `Cancel` RPC. The signal handler only writes a byte to a pipe [cpp/blaze.cc:1788-1877]. The third SIGINT kills the server [cpp/blaze_util_posix.cc:143-225].
  - The exit code arrives in the last `RunResponse` [pb/command_server.proto:149-154].
  - For `bazel run`, the server "doesn't have a terminal", so it tells the client what to `exec()` [bazel: site/en/contribute/codebase.md:79-82].

**Gradle 9.8.0**

- **Start.** The daemon is a direct child of the client JVM. It gets its configuration over stdin, then calls `setsid()` and logs to an owner-only file [gradle: CS/client/DefaultDaemonStarter.java:128-231; DS/bootstrap/DaemonMain.java:73-183].
  - Readiness is a greeting line on the child's stdout ("Daemon started…"), then registry polling every 200 ms for up to 30 s [DS/bootstrap/DaemonMain.java:94-125; CS/client/DefaultDaemonConnector.java:216-264].
- **Races.** Startup is not serialized. A client that finds no idle compatible daemon starts its own, and "a daemon will start in busy state so that nobody else will grab it". Each daemon runs one build at a time [CS/client/DefaultDaemonConnector.java:121-180, 248].
  - The shared registry file takes a file lock per operation, with backoff up to 60 s [gradle: PC/DefaultFileLockManager.java:67, 372-395].
- **Version.** "A given Gradle version can only connect to Daemons of the same version" [gradle: DOC:94-95]. Each version has its own registry directory [gradle: DP/registry/DaemonDir.java:32-36].
  - Beyond the version, `DaemonCompatibilitySpec` requires the same Java home and daemon options. An incompatible daemon is skipped and left to expire, not killed [CS/client/DefaultDaemonConnector.java:186-198].
  - The registry directory is 0700 and the file 0600 [gradle: PC/FileBackedObjectHolder.java:114-115]. A 16-byte token authenticates clients [gradle: LA/server/Daemon.java:122-127].
- **Idle.** The default is 3 h (`DEFAULT_IDLE_TIMEOUT = 3 * 60 * 60 * 1000`), counted only while idle [gradle: CS/configuration/DaemonParameters.java:43; DOC:199-202].
  - Other expiries: low system memory (a threshold between 384 MiB and 1 GiB), being a duplicate idle daemon that was not used most recently, and losing its own registry entry [gradle: LA/server/MasterExpirationStrategy.java:35-76; LA/server/health/LowMemoryDaemonExpirationStrategy.java:45-47].
- **Failures.**
  - A client that cannot connect removes the daemon's registry entry [CS/client/DefaultDaemonConnector.java:266-303].
  - A daemon that dies mid-build yields "the daemon has disappeared" and the tail of its log [CS/client/DaemonClient.java:245-299].
  - A daemon whose client disconnects cancels the build [LA/server/exec/WatchForDisconnection.java:27-44].

**sccache 0.18.0**

- **Start.** The client connects first and starts a server only if the connection is refused, times out or finds no socket [sccache: src/commands.rs:314-352].
  - Readiness is pushed. The client binds a notify socket before spawning ("must be bound before spawning `_child` below to avoid a race"), and the server reports `Ok`, `Err{reason}` or `AddrInUse` [src/commands.rs:79-136; src/server.rs:85-133].
  - The server double-forks and closes every inherited descriptor [src/util.rs:900-960].
- **Races.** There is no lock; the bind decides. Over TCP the loser reports `AddrInUse` and the client retries 10 × 500 ms [src/server.rs:555-558; src/client.rs:72-92].
  - Over a Unix socket, a new server unlinks the path and then binds ("Unix socket will report addr in use on any unlink file") [src/server.rs:507-515]. Two servers started together can therefore leave the first unreachable (inference).
  - Its own TODO asks for a pipe "so it can notify us once it starts the server instead of us polling" [src/client.rs:77-81].
- **No authentication and no version check.** A mismatch shows only as a decode error: "Mismatch of client/server versions?" [src/commands.rs:367-378].
- **Idle.** 600 s by default, reset whenever a request arrives [src/server.rs:77-78, 834-860].
- **If the server dies** mid-compile, the client compiles locally instead [src/commands.rs:531-569].

**What the three share.**

- The warm path is a plain connect, and startup is paid only when the connection is refused or finds no socket.
- The client can check the server before trusting it: a 0700 directory plus cookies (Bazel), a per-version registry plus a token (Gradle), or nothing (sccache).
- None passes descriptors or forwards a terminal. Bazel hands terminal work back to the client as an exec request.
- None publishes its connect or handshake latency.

### 2.5 Q5: OS-native on-demand activation

**launchd** (macOS 26.4.1)

- **Where.** Per-user agents live in `~/Library/LaunchAgents` [launchd(8), FILES]. `launchctl bootstrap gui/<uid> <plist>` loads one into the user's GUI login domain [launchctl(1), domain targets; bootstrap].
  - A plist must be owned by the loading user and must not be group- or world-writable [launchctl(1), load].
- **Sockets.** A `Sockets` entry with `SockPathName` makes launchd bind a Unix socket. `SockPathMode` sets its mode, in decimal because plists lack octal [launchd.plist(5), Sockets].
  - The job calls `launch_activate_socket(name, &fds, &cnt)` to get the descriptors. The call fails with `ESRCH` if launchd did not start the process, and with `EALREADY` on a second call [launch_activate_socket(3)].
- **Program.** `Program` "must be an absolute path". The app-relative `BundleProgram` works only for plists installed through SMAppService, that is, from an app bundle [launchd.plist(5), Program, BundleProgram].
- **Rules for the job.**
  - It must not daemonize: no `daemon(3)`, no fork-and-exit [launchd.plist(5), EXPECTATIONS].
  - `KeepAlive` defaults to false, so only demand starts it [ibid., KeepAlive].
  - `TimeOut` "is no longer implemented", so exiting when idle is up to the job [ibid., TimeOut].
  - Jobs are spawned at most once every 10 s by default [ibid., ThrottleInterval].
  - When a job dies, launchd kills the rest of its process group unless `AbandonProcessGroup` is set [ibid., AbandonProcessGroup].
- **Throttled by default.** With no `ProcessType`, launchd applies "light resource limits to the job, throttling its CPU usage and I/O bandwidth". `Interactive` removes the limits but is meant for apps whose responsiveness depends on it [launchd.plist(5), ProcessType]. A pool daemon on a ≤5 ms path would need `Interactive` (inference).
- **Visible to the user.** Since macOS 13, login items, launch agents and daemons go through a framework "used to create transparency to the user". On macOS 26, background tasks an app leaves running prompt the user [Apple-BTM]. What a plist written by a command-line binary triggers is not documented (UNVERIFIED; E6).

**systemd user units** (systemd v262)

- **Where.** User units are read from `~/.config/systemd/user/`, among other places [systemd: man/systemd.unit.xml:63-80]. In a user unit, `%t` is `$XDG_RUNTIME_DIR` [ibid.:2598-2600].
- **Socket and service.** `foo.socket` needs a matching `foo.service`. With `Accept=no` (the default), one service receives the listening sockets [systemd: man/systemd.socket.xml:54-71, 408-413].
  - "For performance sensitive services, a choice of `Accept=no` is preferable, since that way only the first connection will have to pay the activation resource cost" [ibid.:415-418].
- **Permissions.** `SocketMode=` defaults to 0666 and `DirectoryMode=` to 0755 [ibid.:392-404]. Privacy rests on the 0700 `$XDG_RUNTIME_DIR` (§2.6).
- **Handover.** The service receives its sockets from fd 3 up, with `$LISTEN_FDS` and `$LISTEN_PID`. `sd_listen_fds()` sets `FD_CLOEXEC` on them [systemd: man/sd_listen_fds.xml:48-53, 82-83, 190-201].
  - The manager keeps its own copy, so a restarted daemon "will receive file descriptors to the very same sockets": there is no stale-socket state [ibid.:55-66].
  - The protocol is environment variables plus inherited descriptors, so accepting it needs no libsystemd (inference).
- **Lifetime.**
  - Linger keeps a user's manager running at boot and "kept around after logouts" [systemd: man/loginctl.xml:186-195]. Unprivileged users may enable it for themselves: `set-self-linger` allows `allow_any` [systemd: src/login/org.freedesktop.login1.policy:127-135].
  - A process started from a login session lives in the session's scope. `KillUserProcesses=` decides whether logout kills it, and its default is a build option [systemd: man/logind.conf.xml:104-118; meson_options.txt:358-359].

**Verdict for a downloaded binary.** The binary can install either mechanism itself, without root: write the plist or unit, then run `launchctl bootstrap` or `systemctl --user enable --now` (inference from the pages above). Neither covers the matrix:

- Windows has neither.
- Non-systemd Linux (e.g. Alpine, the usual home of the musl targets), most containers and CI, and sessions without `pam_systemd` have no user manager.
- On macOS the plist pins an absolute path, so moving or replacing the download breaks it.

Self-spawning must exist anyway, and OS activation can only be an optional layer that hands the daemon the same listening socket.

### 2.6 Q6: Unix-domain sockets on XNU and Linux

**Descriptors per message**

- **Linux: 253.** `#define SCM_MAX_FD 253`; more gives EINVAL [linux: include/net/scm.h:15-18; net/core/scm.c:76-82; man-pages: man/man7/unix.7:459-469].
  - Several SCM_RIGHTS headers in one message are summed against the limit [scm.c:103-104] **(probe)**.
  - musl refuses a control buffer over 1056 bytes with ENOMEM before the syscall, which is irrelevant at ≤253 fds [musl 1.2.5: src/network/sendmsg.c:9-23].
- **XNU: 254, in exactly one header.**
  - On LP64, the control buffer must fit an mbuf cluster once each fd has grown to a pointer: `(len − 12)·2 + 12 ≤ MCLBYTES` (2048), else EINVAL [xnu: bsd/kern/uipc_syscalls.c:3302-3317]. That gives 8n + 12 ≤ 2048, so n ≤ 254 (derived) **(probe)**.
  - The message must hold exactly one SCM_RIGHTS header whose length is the whole control buffer, else EINVAL [xnu: bsd/kern/uipc_usrreq.c:2505-2508].

**When the receiver's buffer is too small, or it is out of descriptors**

- **Linux** installs what fits, closes the rest and sets MSG_CTRUNC: "the excess file descriptors are automatically closed in the receiving process". The same holds when there is no control buffer at all [man-pages: man/man7/unix.7:445-449; linux: net/core/scm.c:355-402].
- **XNU installs every descriptor before it looks at the caller's buffer.**
  - `dom_externalize` runs for each SCM_RIGHTS record. With no control buffer (e.g. `read(2)`), the mbuf is freed and the new descriptors stay in the process's table [xnu: bsd/kern/uipc_socket.c:3069-3098].
  - A short buffer is truncated with MSG_CTRUNC, and the excess is never closed [xnu: bsd/kern/uipc_syscalls.c:2113-2122].
  - The truncated header still states the full length (52 for 10 fds, in a 20-byte buffer) **(probe)**. A receiver that trusts `cmsg_len` over `msg_controllen` reads past what the kernel wrote.
- **At the descriptor limit, XNU is all or nothing:** "Allocate all the fds, and if it doesn't fit, then fail and discard everything" [xnu: bsd/kern/uipc_usrreq.c:2401-2418].
  - `recvmsg` fails (EMFILE or EMSGSIZE), and the data stays queued, to arrive on the next call without its fds **(probe)**.
  - Linux instead installs part of the set, sets MSG_CTRUNC and consumes the data [scm.c:370-402] **(probe)**.

**What can be passed; shared state**

- **Linux passes any descriptor except io_uring** [linux: net/core/scm.c:117-121]. In-flight files count per user against the sender's `RLIMIT_NOFILE`, and the excess gets ETOOMANYREFS [linux: net/unix/af_unix.c:1926-1939].
- **XNU passes only** vnodes (files, directories, ttys, devices), sockets, pipes, POSIX shared memory and network-policy descriptors, none of them confined [xnu: bsd/kern/kern_descrip.c:389-402].
  - Kqueues are always confined, so they get EINVAL [xnu: bsd/kern/kern_event.c:3066-3070] **(probe)**.
  - Guarded descriptors raise a guard exception [uipc_usrreq.c:2515-2527].
  - unix(4)'s "Any valid descriptor may be sent" is therefore wrong [macOS unix(4), DESCRIPTION].
- **Both share the open file description.** unix(7): "what is being passed is a reference to an open file description … equivalent to duplicating (dup(2))" [man-pages: man/man7/unix.7:434-443]. unix(4): "a duplicate of the sender's descriptor, as if it were created with a call to dup(2)" [macOS unix(4), DESCRIPTION].
  - Status flags travel with it: `O_NONBLOCK` set by the receiver on a passed pipe or pty appeared on the sender's descriptor, on both systems **(probe)**.

**Close-on-exec on receipt**

- **Linux:** `MSG_CMSG_CLOEXEC` ("since Linux 2.6.23") sets `O_CLOEXEC` on each installed descriptor [man-pages: man/man2/recv.2:94-103; linux: net/core/scm.c:358, 373].
- **XNU:** there is no such flag. `unp_externalize` installs plain entries [xnu: bsd/kern/uipc_usrreq.c:2436-2448], and "Per-process descriptor flags, set with fcntl(2), are not passed to a receiver" [macOS unix(4)].
  - A thread that spawns between `recvmsg` and the `fcntl(FD_CLOEXEC)` can leak the descriptor to its child.
  - `POSIX_SPAWN_CLOEXEC_DEFAULT` closes that window: "only file descriptors explicitly created by the file_actions argument are available in the spawned process" [macOS posix_spawnattr_setflags(3)]. D26 spawns this way.

**Peer credentials**

- **Linux: `SO_PEERCRED`** gives pid, uid and gid "in effect at the time of the call to connect(2), listen(2), or socketpair(2)" [man-pages: man/man7/unix.7:322-347].
  - The pid is a bare number and can be reused. `SO_PEERPIDFD` (Linux 6.5+) gives a pidfd for the process captured at connect [linux: net/core/sock.c:1918-1956].
- **macOS: `getpeereid(3)` and `LOCAL_PEERCRED`** give the effective uid and groups captured at `connect` or `listen`. "This mechanism is reliable" [macOS getpeereid(3), DESCRIPTION; unix(4); xnu: bsd/kern/uipc_usrreq.c:1429-1436]. `struct xucred` has no pid.
- **macOS: `LOCAL_PEERPID`, `LOCAL_PEEREPID` and `LOCAL_PEERTOKEN` do not identify the connector.**
  - They read the peer socket's `last_pid` [uipc_usrreq.c:853-900], which every process that uses the socket overwrites: bind, connect, send, receive, poll [xnu: bsd/kern/uipc_socket.c:400-423].
  - The audit token is looked up when asked for, and fails once that process has gone [uipc_usrreq.c:902-935] **(probe)**.
  - macOS has no `SCM_CREDS` [uipc_usrreq.c:2505-2508].

**Socket paths**

- **Size.** Linux `sun_path` is 108 bytes [linux: include/uapi/linux/un.h:7-12]; macOS declares 104 [xnu: bsd/sys/un.h:76-80; macOS unix(4)].
  - XNU accepts an `address_len` up to 255 and refuses longer with ENAMETOOLONG [xnu: bsd/kern/uipc_syscalls.c:3366-3367; bsd/sys/socket.h:465; uipc_usrreq.c:1175-1187]. Neither kernel truncates **(probe)**.
  - Rust's std refuses a path of `sizeof(sun_path)` bytes or more, "path must be shorter than SUN_LEN", so through std the limit is 103 bytes on macOS and 107 on Linux [rust std 1.94.1: library/std/src/os/unix/net/addr.rs:40-45; the same in the local nightly].
- **Abstract names are Linux-only and unprotected:** "Socket permissions have no meaning for abstract sockets" [man-pages: man/man7/unix.7:246-260]. On macOS an abstract address gives ENOENT **(probe)**.
- **Stale paths.**
  - Binding over an existing path gives EADDRINUSE on both systems, stale socket or not [linux: net/unix/af_unix.c:1414; xnu: bsd/kern/uipc_usrreq.c:1202-1214]. macOS's bind(2) says EEXIST, which is wrong [macOS bind(2), ERRORS].
  - Connecting to a stale socket gives ECONNREFUSED on both [af_unix.c:1221-1229; uipc_usrreq.c:1355-1358].
- **Connecting needs write permission** on the socket file on both [af_unix.c:1213-1219; uipc_usrreq.c:1347]. unix(7) still warns: "Portable programs should not rely on this feature for security" [unix.7:224-232].
- **Bind applies the umask** on both [af_unix.c:1351-1352; uipc_usrreq.c:1216-1218].

**Where per-user sockets go**

- **Linux: `$XDG_RUNTIME_DIR`** [XDG basedir 0.81, §3].
  - It "MUST be owned by the user … Its Unix access mode MUST be 0700".
  - It lives from first login to full logout. It is local, and it must support AF_UNIX sockets and file locking.
  - Its files "MAY be subjected to periodic clean-up", unless their access time is refreshed every 6 hours or they have the sticky bit.
  - If it is unset, applications "should fall back to a replacement directory with similar capabilities and print a warning message".
- **systemd's** runtime directory is `/run/user/$UID`, a per-user tmpfs set up by `pam_systemd` and removed when the last session ends [systemd: man/pam_systemd.xml:42-45, 69-74]. It lives on while linger keeps the user manager up [man/loginctl.xml:186-195].
  - It is unset in the `alpine:3.22` and `debian:trixie-slim` containers **(probe)**, and wherever `pam_systemd` does not run [pam_systemd.xml:77-79].
- **macOS: `confstr(_CS_DARWIN_USER_TEMP_DIR)`** is created 0700, and "files in this location may be cleaned (removed) by the system if they are not accessed in 3 days". `_CS_DARWIN_USER_CACHE_DIR` is also 0700 but "will not be automatically cleaned", only removed during safe boot [libc: gen/confstr.3:100-118].
  - If the per-user lookup fails, `confstr` falls back to `$TMPDIR`, then to `/var/tmp/` [libc: gen/confstr.c:225-247]. The directory's owner and mode must therefore be checked, not assumed.
  - **This host:** both directories are 49 bytes (`/var/folders/…/T/` and `…/C/`), mode 0700 **(measured, this host)**. That leaves 54 bytes under 104 for everything below them, or 53 through Rust's std (derived).
  - On macOS, `connect` does not refresh the socket file's access time **(probe)**. A daemon kept busy for days with its socket and lock file in the temp directory could lose them to the cleaner (inference; dirhelper's exact rule is UNVERIFIED).

### 2.7 Q7: `docker run` for a workload that runs elsewhere

container-engine-internals.md §2.3 covers the attach transport (stdcopy frames, the `101 UPGRADED` hijack, detach keys) and stop/kill. This section adds the client side. The moby `client/` package and `stdcopy` are byte-identical to the CLI's vendored copies (checked with `diff`).

**Order: create, attach, wait, start** [cli: cli/command/container/run.go:128-264]

1. The signal proxy starts right after create.
2. Attach is synchronous: it returns once the client has read the 101 [moby: client/hijack.go:73-80].
3. The wait is registered next. `ContainerWait` "blocks until the request has been acknowledged by the server", so "next-exit" is in place before start [moby: client/container_wait.go:34-40; daemon/server/router/container/container_routes.go:437-450].
4. Start, then a resize with `-t`.
5. The CLI then waits on whichever ends first: the stream or the exit status.

- **Why the order matters.** The daemon's broadcaster writes only to the writers registered at that moment, with no backlog [moby: daemon/internal/stream/unbuffered.go:21-37]. Output written before the attach would never reach this client (inference), and a "next-exit" wait registered after the exit would never fire.
- **With `--rm`, the wait condition is "removed"** instead of "next-exit" [cli: cli/command/container/utils.go:18-21]. It fires only after the rootfs is deleted, the container deregistered and its anonymous volumes removed [moby: daemon/delete.go:162-182; daemon/container/state.go:395-409].

**Output drains before exit.**

- If the status arrives first, "we need to keep the streamer running until all output is read" [cli: run.go:249-259]. If stdin ends first, the streamer waits "for output to complete streaming" [cli: cli/command/container/hijack.go:72-92].
- At exit the daemon waits up to 2 s for the container's stdio copiers [moby: daemon/monitor.go:83-87].
- This has broken before: commit c27751fcfe5d (2025), "Fix stdout/err truncation after container exit", fixed a regression.

**Stdin.**

- Without `-i`, the CLI never reads its stdin [cli: run.go:279-281; hijack.go:171-172]. A `while read …; do docker run …; done` loop keeps its input (inference).
- `-i` in attached mode sets StdinOnce, "close stdin at client disconnect" [cli: cli/command/container/opts.go:729-732].
- When the client's stdin ends, the CLI half-closes (`CloseWrite`) [hijack.go:194-196]. The daemon then closes the container's stdin, but only if `CloseStdin && !TTY` [moby: daemon/internal/stream/attach.go:69-72].

**Exit codes missing from D16.** D16 cites `runStartContainerErr`; in v29.8.1 create and start errors go through `toStatusError` [cli: run.go:151-154, 322-356].

| Case | Exit | Source |
|---|---|---|
| Container exits with n | n, nothing printed | [cli: run.go:246-249, 261-263] |
| The wait API fails, or `--rm` removal fails | 125, with an error line on stderr | [cli: utils.go:33-46] |
| The attach stream fails (reset, daemon error frame, stdout write error) | 1, error printed | [cli: run.go:236-245] |
| Detach (ctrl-p ctrl-q; needs `-t` and stdin) | 0; the container keeps running | [cli: run.go:236-241; hijack.go:95-129, 177-184] |
| `-d` | 0 after start; the ID is printed | [cli: run.go:173-181, 222-227] |
| The daemon does not answer the first ping | 1 | [cli: run.go:104-107] |
| Third SIGINT/SIGTERM to the CLI | 1: "got 3 SIGTERM/SIGINTs, forcefully exiting", after restoring the terminal | [cli: cmd/docker/docker.go:443-462] |

**`--sig-proxy`** defaults to true [cli: run.go:61].

- **TTY or not.** In `docker run` only `--sig-proxy=false` turns it off [run.go:155]. Commit ee295049231c (2019), "Do not disable sig-proxy when using a TTY", made this so. `docker attach` and `start -a` still skip it for TTY containers [cli: cli/command/container/attach.go:109; start.go:100-105].
- **What is forwarded.** Every catchable signal except SIGCHLD, SIGPIPE, the Go runtime's SIGURG and nameless signals. Each goes by name through `ContainerKill` [cli: cli/command/container/signals.go:31-56].
- **When.** The proxy runs from create until `run` returns [run.go:155-164]. The daemon refuses signals before the task runs ("container %s is not running") [moby: daemon/kill.go:66-78; daemon/container/container.go:865-868], so a signal between create and start is dropped and the run continues (inference).
- **The CLI's own signals.** Its first SIGINT or SIGTERM does not end the run: attach, wait and start use contexts that cannot be cancelled [run.go:166, 199-201].
- **SIGPIPE.** The CLI catches it, so a closed stdout gives it EPIPE: it prints the error and exits 1, while the container runs on (inference; run.go:236-245).

**TTY.**

- **Raw mode** is set only with `-t` and an attached stdin. It is restored once, at output EOF, stdin end, stream exit or forced exit, whichever comes first [cli: hijack.go:95-112, 145-150, 177; docker.go:460-461].
- Raw mode clears ISIG [cli: vendor/github.com/moby/term/termios_unix.go:15-35], so `^C` and `^Z` travel as bytes to the container's pty (inference).
- With a TTY, stderr goes to the CLI's stdout as one stream [cli: run.go:285-290].
- `-it` with a stdin that is not a terminal fails before create: "cannot attach stdin to a TTY-enabled container because stdin is not a terminal" (exit 1) [cli: cli/streams/in.go:66-75; run.go:128-131]. `-t` without `-i` is allowed.
- **Size.** The initial size is sent at create. One resize is sent after start, retried up to 10 times. Then SIGWINCH triggers a resize on Unix; Windows polls every 250 ms [cli: cli/command/container/tty.go:57-106; run.go:229-233].

**When the client goes away.**

- Only on Linux does the daemon watch the hijacked socket for `EPOLLHUP` [moby: daemon/server/router/container/notify_linux.go:33-48].
- On hangup it closes that attach's output streams [moby: daemon/attach.go:54-67]. It closes the container's stdin only for StdinOnce without a TTY [moby: daemon/internal/stream/attach.go:69-72, 100-109].
- Nothing on this path stops the container (inference). `--rm` is applied when the container exits [moby: daemon/monitor.go:134-141, 340-356]: "removed when it exits or when the daemon exits, whichever happens first" [cli: docs/reference/commandline/container_run.md:822-823].

**Backpressure.** Each attach has a pipe that blocks writers at 1e6 bytes [moby: daemon/internal/stream/bytespipe/bytespipe.go:15-17]. A slow client slows the container's output rather than losing it.

**What handing over the client's descriptors changes.** Docker's CLI keeps the terminal and pipes and relays them to the daemon. A warm VM that receives the client's descriptors writes to them directly. That is cheaper, but four kernel rules then apply to a process the shell is not waiting on:

- **Job control stops only the terminal's own session.**
  - POSIX sends SIGTTIN or SIGTTOU when a background process group reads from, or (with TOSTOP) writes to, "its controlling terminal" [POSIX XBD §11.1.4].
  - Linux returns early when the terminal is not the caller's controlling terminal: `if (current->signal->tty != tty) return 0;` [linux: drivers/tty/tty_jobctrl.c:39-40]. `n_tty` reads go through that check [linux: drivers/tty/n_tty.c:2090-2100].
  - XNU counts a process as background only if it is in the terminal's session and has a controlling terminal [xnu: bsd/kern/tty.c:3347-3366]. `ttread` and `ttwrite` signal only such processes [ibid.:2125-2140, 2498-2510].
  - A warm VM spawned by the daemon is in another session, so it is never stopped. A backgrounded `shards run -i` with the terminal as stdin would read keystrokes meant for the shell, where `docker run -i` in the background stops on SIGTTIN (inference).
- **Keyboard signals and SIGWINCH go to the foreground process group:** the client's, never the warm VM's. `tcsetwinsize()` delivers SIGWINCH "to the foreground process group associated with the terminal" [POSIX tcsetwinsize()].
- **A pipe ends only when every writer closes.** A reader sees end-of-file only "if all file descriptors referring to the write end of a pipe have been closed" [man-pages: man/man7/pipe.7:80-85]. A warm VM that keeps the client's stdout open while it tears down its VM holds up `shards run … | consumer` (inference).
- **Flags are shared** through the open file description (§2.6).

## 3. Implications for shards (ranked)

Ranked by how much of every run's ≤5 ms path and `docker run` parity each one decides. D26 already builds the warm VM; these rank the parts still to build around it.

1. **Keep D26's handoff: pass the request and the client's descriptors to the warm VM, and keep the daemon out of the data path.**
   - *What:* the daemon accepts the client and reads its request. It passes the connection and the stdio descriptors with `SCM_RIGHTS` to a warm VM of the template, which answers the client directly.
   - *Evidence:*
     - The handoff costs 31 µs at p50 and 75 µs at p99, about 9 µs more than answering in the daemon [PM M23].
     - Kata's alternative, serializing a VM description for the client to re-adopt, loses state (the pid file) and is deprecated (§2.1). Kata itself uses descriptor passing (QMP `getfd`) for live objects [kata: vc/qemu.go:2574-2595].
     - Bazel instead relays every output byte through its server [bazel: pb/command_server.proto:143-147].
     - A daemon out of the data path can crash without ending the runs in flight. Docker needs live-restore for that (§2.7).
   - *Alternative, not recommended:* Android-style members that `accept()` on a per-template socket, with no daemon on the path [aosp: os/Zygote.java:776-797]. It saves ≈31 µs, well under 1% of 5 ms (derived). But the client would have to resolve templates, pool accounting would move to a side channel, and there would be no single place for the peer check, the version check and misses.
   - *Risks:* the kernel rules below (items 2 and 3).

2. **Descriptor-passing rules, the same on both kernels.**
   - *Message shape.*
     - Exactly one SCM_RIGHTS header per message: XNU rejects anything else with EINVAL [xnu: bsd/kern/uipc_usrreq.c:2505-2508].
     - At least one data byte: "you must send or receive at least one byte of nonancillary data" [man-pages: man/man7/unix.7:774-782]. Linux's stream send loop only runs `while (sent < len)`, so with no data the descriptors are simply dropped [linux: net/unix/af_unix.c:2417, 2504-2506] (inference).
     - Never `MSG_PEEK` on sockets that carry descriptors: Linux installs duplicates on every peek [af_unix.c:1957-1962].
   - *Receive buffer.* Size it for the kernel's maximum: 254 descriptors on macOS, `CMSG_SPACE(1016)` = 1028 bytes. Parse only within `msg_controllen`. Receive descriptor-carrying messages only with `recvmsg`.
     - On macOS a shorter buffer, or a plain `read`, installs the excess descriptors and never closes them [xnu: bsd/kern/uipc_socket.c:3069-3098; uipc_syscalls.c:2113-2122].
   - *Count check.* Carry the count in the payload and check it. At the descriptor limit Linux delivers a partial set, while macOS fails and redelivers the data without its descriptors (§2.6).
   - *Close-on-exec.* `MSG_CMSG_CLOEXEC` on Linux. On macOS, `fcntl(FD_CLOEXEC)` right after receipt, plus `POSIX_SPAWN_CLOEXEC_DEFAULT` for every spawn (D26 does this).
   - *Flags.* Never change status flags (`O_NONBLOCK`) on a received descriptor: the client's shell shares them. Use blocking I/O on a thread per stream [man-pages: man/man7/unix.7:434-443] **(probe)**.
   - *Ownership.* The daemon keeps its copies until the warm VM says it has them, then closes them. On Linux, unreceived in-flight descriptors count against the user's `RLIMIT_NOFILE` [linux: net/unix/af_unix.c:1926-1939].
     - **Correction (2026-09-29).** This note first said to close them as soon as they were forwarded. On XNU that loses sockets: the collector of in-flight descriptors walks only descriptors in flight, so a socket that no process holds any more, sitting in the buffer of a socket not in flight, is taken for garbage and flushed if a collection runs before the receive. Every freed Unix socket starts one [xnu: bsd/kern/uipc_usrreq.c:2556-2750, 2912; PM M24].
   - *Risks:* none beyond a larger receive buffer. Windows needs another mechanism (E8).

3. **The client owns the terminal. The warm VM gets only descriptors that are not terminals, and sends the exit status last.**
   - *Stdio.* Pass stdin, stdout and stderr when they are pipes, files or sockets. When stdin is a terminal and `-i` is set, the client reads it and forwards the bytes, so a backgrounded run still stops on SIGTTIN (§2.7).
   - *Terminal.* With `-t`, the client sets raw mode and restores it on every exit path. It sends the initial size and a resize on each SIGWINCH [cli: tty.go:57-106].
   - *Signals.* The client forwards every catchable signal except CHLD, PIPE and URG, by name, from create until exit, with or without a TTY: `docker run --sig-proxy` in v29.8.1 [cli: run.go:155; signals.go:31-56]. That includes keyboard signals when the terminal is not raw, which is wider than D16's "every one another process sends". The third SIGINT or SIGTERM exits 1 [cli: cmd/docker/docker.go:443-462].
   - *Order at the end.* The warm VM:
     1. writes all output;
     2. closes its copies of the client's descriptors;
     3. sends EXIT;
     4. tears down.
     - Then no prompt interleaves with output, and pipes see EOF [man-pages: man/man7/pipe.7:80-85; cli: run.go:249-259]. D26 already sends EXIT before teardown.
   - *Client hang-up.* The warm VM stops writing to the client's descriptors and, for `-i` without `-t`, closes guest stdin. It lets the command finish, then exits, as dockerd does [moby: daemon/attach.go:54-67; daemon/internal/stream/attach.go:69-72].
   - *EPIPE.* On EPIPE, the warm VM tells the client, which reports it and exits 1 while the command runs on (§2.7).
   - *Warm-VM death.* If the warm VM dies before EXIT, the client prints why and exits 125, Docker's code for a failed wait [cli: utils.go:33-46].
   - *Risks:* relaying terminal input costs a copy per keystroke, which is negligible. D16 says a terminating signal before the command runs "ends shards", where Docker drops it and runs on (§2.7).

4. **Pools per template: refill to one at once, top up after a lull, size by Little's law, cap by memory, and never make a miss wait for a refill.**
   - *Key.* D25's template name (format, kernel, init, image, CPUs, memory, command line). This is LightVM's shell "fitting the VM requirements" [Manco17 §5.2] and Kata's configuration check (§2.1).
   - *Size.*
     - Each template used within the idle window keeps one warm VM, replaced as soon as it is taken.
     - Grow past one only when requests outpace refills. Top up after a quiet interval, as USAP does: immediately to the minimum, delayed to the maximum [aosp: os/ZygoteServer.java:624-655].
     - Pool ≈ arrival rate × refill latency [Agache20 §4.1.2]. M22's 6.5 ms p50 per-request run bounds a refill (launch plus restore) [PM M22], so one warm VM serves roughly 150 serial runs/s (derived).
   - *Cap.* A waiting VM costs 12.3 MiB and no CPU [PM M23]. Cap the total, and evict the least recently used template's VMs first.
   - *Miss.* On an empty pool, start a VMM for that request at once: the D25 path, 7.5 ms at p50. Count the miss.
   - *Evidence:*
     - Catalyzer: caching "can not help reduce tail latency, which is dominated by the 'cache miss boot'" [Du20 §6.9].
     - Kata's empty pool blocks with no timeout, and one build error kills the pool (§2.1).
     - LightVM and Virtines keep a configurable number of shells [Manco17 §5.2; Wanninger22 §5.2].
   - *Risks:* refills compete with running workloads for CPU. M22 put the per-request tail in exactly that host-side work, so refill scheduling must be measured (E1).

5. **Auto-start with no installer: lock, bind, and hand the daemon its listener and its lock.**
   - *What:* the client connects. If nothing is listening (`ECONNREFUSED` or `ENOENT`):
     1. It opens `lock` in the socket directory and takes `flock(LOCK_EX)`. It then checks that the path still names the file it locked, as Bazel does [bazel: cpp/blaze_util_posix.cc:650-657], and connects once more.
     2. If that also fails, it unlinks any stale socket, binds and listens.
     3. It spawns `shards daemon` with the listener and the lock descriptor inherited: `setsid`, stdio to a log file, `POSIX_SPAWN_CLOEXEC_DEFAULT` on macOS.
     4. It closes its own copies and connects. Its request waits in the listen backlog until the daemon accepts.
     - The daemon keeps the lock descriptor for its whole life, close-on-exec, so no warm VM inherits it. It answers a request it cannot serve with an error message, as sccache reports `Err{reason}` [sccache: src/server.rs:85-133].
   - *Evidence:*
     - A `flock` lock belongs to the open file description, survives `execve`, and is released when the last descriptor closes [man-pages: man/man2/flock.2:54-67, 85-88; macOS flock(2), NOTES; xnu: bsd/kern/kern_descrip.c:5993-6036]. "Lock free" therefore means exactly "no daemon", with no pid file and no PID-reuse check, the part Bazel gets wrong on macOS [bazel: cpp/blaze_util_darwin.cc:247-252].
     - Binding in the parent and passing the listener is how firecracker-containerd starts shims [fcctr: firecracker-control/local.go:476-501] and how launchd and systemd activate services (§2.5). With systemd, "only the first connection will have to pay the activation resource cost" [systemd: man/systemd.socket.xml:415-418].
     - Bazel takes an exclusive lock on every invocation [bazel: cpp/blaze.cc:1995-2006]. sccache races binds and can orphan a server [sccache: src/server.rs:507-515] (inference). Gradle starts duplicates and expires them later [gradle: CS/client/DefaultDaemonConnector.java:121-180].
   - *Risks:*
     - A daemon that hangs keeps the lock. Clients need connect and reply timeouts, and `shards daemon stop` must be able to kill the lock holder. Bazel escalates to `killpg(SIGKILL)` after 60 s [bazel: cpp/blaze.cc:1879-1949].
     - The first run after an idle exit pays the daemon's launch (≈3.5 ms [PM M23]) plus a miss (item 4).
     - The auto-start code must stay in the thin client without pulling in the VMM's frameworks: 3.5 ms against 1.4 ms per launch [PM M23].

6. **One socket directory per user and per build, verified private, checked by peer credentials, and self-fencing.**
   - *Where.*
     - Linux: `$XDG_RUNTIME_DIR/shards/<build>/`.
     - Linux without `$XDG_RUNTIME_DIR`: a directory the daemon creates 0700, then verifies (owner, mode, not a symlink), with a warning, as the spec asks [XDG basedir 0.81, §3].
     - macOS: `confstr(_CS_DARWIN_USER_CACHE_DIR)` + `shards/<build>/`, because the temp directory is cleaned after 3 days without access and `connect` does not refresh the access time (§2.6). Verify its owner and mode too, because `confstr` can fall back to a shared directory [libc: gen/confstr.c:225-247].
     - Keep the whole socket path within 103 bytes; this host has 54 bytes after the directory (§2.6).
   - *Who.* Accept only peers whose effective UID is the daemon's: `SO_PEERCRED` on Linux; `getpeereid`/`LOCAL_PEERCRED` on macOS, never `LOCAL_PEERPID` or `LOCAL_PEERTOKEN` (§2.6). Never use Linux's abstract namespace [man-pages: man/man7/unix.7:246-260].
   - *Why one directory per build:* a client must never hand a request to warm VMs of another snapshot format or VMM build.
     - Gradle keeps a directory per version and lets old daemons expire [gradle: DP/registry/DaemonDir.java:32-36; DOC:94-95]. Bazel instead restarts the server on a content-hash mismatch [bazel: cpp/blaze.cc:1125-1172].
     - A directory per build gives Gradle's behavior with no handshake, and old daemons idle out (item 8).
   - *Self-fencing.* The daemon exits when its socket or lock path no longer names its own inode, as Bazel's PID-file watcher and Gradle's lost-registry expiry do [bazel: srv/PidFileWatcher.java:105-137; gradle: LA/server/MasterExpirationStrategy.java:35-76].
   - *Evidence against weaker schemes:* Kata relies on the socket file's mode inside a 0755 directory, and its RPCs trust any peer [kata: src/runtime/cmd/kata-runtime/factory.go:104-124].
   - *Risks:* during an upgrade, two builds' daemons and pools coexist.

7. **Failures: every warm VM is disposable, a template that keeps failing is retired, and nothing blocks.**
   - *A warm VM dies while idle* (EOF on its daemon socket, or child exit): drop it and replace it, with exponential backoff. launchd's default is one spawn per 10 s [launchd.plist(5), ThrottleInterval].
   - *A restore fails:* that process ends, as a Firecracker process ends on a failed load [fc: docs/snapshotting/snapshot-support.md:514-515]. After K consecutive failures of one template, remove it, as D25 does on the single-process path, and serve misses by booting.
   - *The guest never reports ready:* a VM that restores but whose guest sends no `READY` (D26) within a deadline counts as a failure too.
   - *The daemon dies:* idle warm VMs exit on EOF from it (D26), runs in flight finish, and the next client restarts it (item 5).
   - *Evidence:*
     - Kata detects neither a dead pooled VM nor a stale template, and one error closes its pool (§2.1).
     - USAP falls back to an ordinary fork on any pool error [aosp: core/java/android/os/ZygoteProcess.java:416-427].
     - sccache compiles locally when its server vanishes [sccache: src/commands.rs:531-569].
   - *Staleness:* a template is judged by its content name, never by time (D25). Kata's templates are not checked at all [kata: vc/factory/template/template_linux.go:30-41].

8. **Idle: shrink pools first and exit the daemon last. The daemon decides; nothing outside will.**
   - *What:*
     - A template's pool drops to zero after T₁ without a request for it.
     - The daemon exits after T₂ with no pools, no clients and no running commands. "Idle" means no work in flight, as in Bazel and Gradle, not merely no new request, as in sccache [bazel: srv/ServerWatcherRunnable.java:167-204; gradle: LA/server/DaemonStateCoordinator.java:455-460; sccache: src/server.rs:834-860].
     - Memory pressure shrinks every pool, as Gradle's low-memory expiry does [gradle: LA/server/health/LowMemoryDaemonExpirationStrategy.java:45-47].
   - *Evidence:*
     - Defaults elsewhere: Bazel 3 h, Gradle 3 h, sccache 10 min [bazel: site/en/docs/user-manual.md:2396-2406; gradle: CS/configuration/DaemonParameters.java:43; sccache: src/server.rs:77-78].
     - launchd's `TimeOut` "is no longer implemented" [launchd.plist(5), TimeOut].
     - An idle Lambda slot still costs its memory [Agache20 §4.2].
   - *Numbers:* T₁ and T₂ are policy. One warm VM for each of 10 templates holds 123 MiB (derived from [PM M23]).
     - Bazel's and Gradle's hours keep caches that are slow to rebuild; a shards pool refills in milliseconds. That argues for minutes, not hours (inference).

9. **OS activation is an option, not the default.**
   - *What:* an optional `shards daemon install` writes a launchd agent or a systemd user socket unit. The daemon takes an inherited listener (`launch_activate_socket`, `$LISTEN_FDS`) through the same path as item 5.
   - *Why not the default (§2.5):*
     - neither mechanism covers Windows, non-systemd Linux, containers or CI;
     - launchd pins an absolute path to the binary;
     - a launchd job without `ProcessType` is CPU- and I/O-throttled;
     - macOS shows background items to the user.

10. **Later, and only if process launch and teardown show in the tail: recycle warm processes.**
    - *What:* after a run, the VMM process restores the template again instead of exiting.
    - *Evidence:*
      - Virtines clear used contexts and re-pool them, within 4% of `vmrun` [Wanninger22 §5.2].
      - Lambda reuses a slot's VM for serial calls of one function [Agache20 §4.1.1].
      - M22 put the per-request tail in launch, restore and teardown [PM M22].
    - *Risks:*
      - Host-side state could leak between runs; Virtines clears contexts to prevent exactly that.
      - Whether M16's stall (a fresh process mapping a just-unmapped file) also happens within one process is unknown.
      - Measure first (E4).

**Constraint conflicts found**

| Constraints in tension | Evidence |
|---|---|
| ≤5 ms for every run vs minimal memory | A warm VM per template costs 12.3 MiB [PM M23]; a miss falls back to a 7.5 ms restore (D25) |
| Docker parity vs zero-copy stdio | Job control and SIGWINCH reach only the client's process group, so terminal input must go through the client (§2.7) |
| No installer vs first-run latency | Self-spawning adds the daemon's ≈3.5 ms launch to the first run after an idle exit [PM M23] |
| Platform-agnostic vs Unix mechanisms | `SCM_RIGHTS`, `flock`, launchd and systemd are Unix-only. Windows passes handles with `DuplicateHandle`, which needs `PROCESS_DUP_HANDLE` access to both processes [MS-DuplicateHandle] (E8) |
| Fast starts vs launchd defaults | An agent without `ProcessType` is CPU- and I/O-throttled, and `Interactive` is meant for apps (§2.5) |

## 4. Open questions needing our own measurement

- **E1. Refill versus the tail.**
  - Bursts of N parallel `shards run` against pools of 1…k.
  - Compare immediate and delayed top-up, and refills at default and background QoS.
  - Report the runs' p50/p99 and the miss rate (M22's method).
- **E2. First run after auto-start.** Measure client → lock → bind → daemon spawn → miss, then the second run. Report n and p50 through max.
- **E3. Auto-start races.** Start 1–64 clients together with no daemon, and `kill -9` the daemon at random points: while it holds the lock, mid-spawn, and with stale socket files. Pass means exactly one daemon, no request lost, and no client hung past its timeout.
- **E4. Recycling versus respawning.** Restore the template again inside a used VMM process. Compare time, RSS and PSS with a fresh process, and look for M16's stall.
- **E5. A waiting VM over time.** RSS and PSS of 1–100 idle warm VMs over an hour, and whether any of them grows.
- **E6. launchd on this host.** What a plist written by the CLI triggers (notification, Login Items), and the handoff latency with and without `ProcessType=Interactive`.
- **E7. Terminal and pipe semantics, end to end:**
  - a backgrounded `shards run -i` reading the terminal, which must stop as `docker run -i` does;
  - SIGWINCH round trips;
  - `shards run … yes | head -1`: EPIPE, exit 1, the workload continues;
  - `… | cat` reaching EOF;
  - a client killed mid-run.
- **E8. Windows.** Whether AF_UNIX on Windows carries descriptors (UNVERIFIED; no primary source found), or whether handles must go by `DuplicateHandle` over a named pipe. Also the socket location and access control there.
- **E9. macOS privacy attribution.** Which app's TCC permissions apply to a daemon self-spawned from a terminal, and so to files its VMs open for mounts (D17). No primary documentation was found.
- **E10. The descriptor probe.** Committed as `docs/research/measurements/fdpass/` and recorded as platform-measurements.md M25. Still to add: x86_64 runs on both OSes and a Linux 7.x kernel.

## 5. References

**Peer-reviewed papers** (retrieved 2026-09-29 and read via pdftotext)

- [Agache20] A. Agache et al. "Firecracker: Lightweight Virtualization for Serverless Applications." USENIX NSDI 2020. https://www.usenix.org/system/files/nsdi20-paper-agache.pdf
- [Manco17] F. Manco et al. "My VM is Lighter (and Safer) than your Container." ACM SOSP 2017. doi:10.1145/3132747.3132763. https://www.cs.utexas.edu/~witchel/380L/papers/manco17sosp-lightvm.pdf
- [Wanninger22] N. C. Wanninger, J. J. Bowden, K. Shetty, A. Garg, K. C. Hale. "Isolating Functions at the Hardware Limit with Virtines." ACM EuroSys 2022. doi:10.1145/3492321.3519553. https://nickw.io/papers/eurosys22.pdf
- [Du20] D. Du et al. "Catalyzer: Sub-millisecond Startup for Serverless Computing with Initialization-less Booting." ACM ASPLOS 2020. doi:10.1145/3373376.3378512. https://ipads.se.sjtu.edu.cn/_media/publications/catalyzer-asplos20.pdf

**Specifications and vendor documentation**

- [POSIX XBD §11.1.4] The Open Group. *POSIX.1-2024*, Base Definitions, ch. 11, "Terminal Access Control". https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/V1_chap11.html
- [POSIX tcsetwinsize()] https://pubs.opengroup.org/onlinepubs/9799919799/functions/tcsetwinsize.html
- [XDG basedir 0.81] freedesktop.org. *XDG Base Directory Specification* 0.81, §3 "Environment variables". https://specifications.freedesktop.org/basedir-spec/latest/
- [Apple-BTM] Apple. *Apple Platform Deployment*, "Manage login items and background tasks on Mac". https://support.apple.com/guide/deployment/manage-login-items-background-tasks-mac-depdca572563/web (retrieved 2026-09-29)
- [MS-DuplicateHandle] Microsoft. `DuplicateHandle` function (handleapi.h). https://learn.microsoft.com/en-us/windows/win32/api/handleapi/nf-handleapi-duplicatehandle
- macOS 26.4.1 man pages, and systemd v262 and Linux man-pages 6.19 sources, as listed at the top.

**Other source code**

- musl 1.2.5, `src/network/sendmsg.c`, as bundled with zig 0.16.0.
- Rust std 1.94.1, `library/std/src/os/unix/net/addr.rs`.
- docker/cli commits c27751fcfe5d and ee295049231c (read through the GitHub API).

**Our measurements:** PM M16, M22, M23 (platform-measurements.md); D16, D25, D26 (architecture.md).
