# Rootless operation and security hardening (VMM + in-VM engine)

*shards research note, 2026-09-28. All sources below were retrieved on that date.*

**How sources were read**
- Every cited paper was downloaded as a PDF and read in full text. Section, figure and table numbers are as printed in the paper.
- Linux behaviour comes from the local 7.2-rc4 tree (`/Users/adalundhe/Projects/linux`) and man-pages 6.19 from man7.org.
- Every repository is pinned to a commit, listed in §5.

**Citation tags:** `[Key §x]` paper; `[man:]` man page; `[linux: path:line]` kernel tree; `[fc:]` Firecracker, `[krun:]` libkrun, `[gmv:]` go-microvm, `[moby:]` Docker engine source; `[oci:]` runtime-spec; `[docker: page › heading]` docs.docker.com; `[apple:]` developer docs, `[sdk:]` macOS 26.4 SDK headers; `[virtio §x]` OASIS VIRTIO 1.3; `[cve: ID]` MITRE record text.

Anything I could not confirm from a source is marked **UNVERIFIED**.

## 1. Scope

This note covers Q1–Q5 of the brief and the scope extension "user-configurable isolation controls" (§2.6 and R7).

**Threat model**
- Once a vCPU runs, guest code, including the guest kernel, is controlled by the attacker [fc: docs/design.md:88-95; Schumilo20 §III.A].
- All containers in one microVM belong to one tenant, but the agents in them do not trust each other.
- The host user running shards is the tenant, not an administrator.

**Out of scope:** microarchitectural side channels (beyond host-configuration pointers) and the performance of virtio data paths.

## 2. Findings

### 2.1 Q1: Linux rootless primitives (ground truth)

| Primitive | What an unprivileged process gets | Hard limits and caveats |
|---|---|---|
| User namespaces | Create a userns; other namespace types then need only CAP_SYS_ADMIN in it, and CLONE_NEWUSER is applied first when combined [man: user_namespaces(7) §"Interaction of user namespaces and other types of namespaces"]. | ≤32 nesting levels, ≤340 map lines [linux: kernel/user_namespace.c:92; include/linux/user_namespace.h:17]. Distros can veto creation via the `userns_create` LSM hook (AppArmor implements it) [linux: include/linux/lsm_hook_defs.h:267; security/apparmor/lsm.c:1044]; Ubuntu ≥24.04 does so by default [docker: rootless/troubleshoot › Distribution-specific hint]. |
| ID maps without a helper | Exactly one map line, mapping the writer's own euid. The same applies to egid, but only after writing `deny` to `/proc/pid/setgroups` [man: user_namespaces(7) §"Defining user and group ID mappings"; linux: kernel/user_namespace.c:1172-1212]. | Any other map needs CAP_SETUID/CAP_SETGID in the *parent* userns. `newuidmap(1)` writes maps after checking `/etc/subuid` [man: newuidmap(1)], so it must itself be privileged. SC'21 calls such a helper "a security boundary" [Priedhorsky21 §2.1.2]. Real failures: newgidmap re-enabled setgroups [cve: CVE-2018-7169], and the kernel mishandled maps with more than 5 extents [cve: CVE-2018-18955]. |
| Capabilities inside a userns | They act only on resources of namespaces that userns owns [man: user_namespaces(7) §"Effect of capabilities within a user namespace"]. | These still require the *initial* userns: CAP_SYS_TIME, CAP_SYS_MODULE, CAP_MKNOD, and mounting block-based filesystems [same]. |
| Mounts inside a userns | proc, sysfs, devpts, tmpfs, ramfs, mqueue, bpf, overlayfs (≥5.11), and cgroup2 with a cgroupns [man: user_namespaces(7)]. In 7.2-rc4 fuse, binderfs and binfmt_misc are also `FS_USERNS_MOUNT` [linux: fs/fuse/inode.c:2004; drivers/android/binderfs.c:754; fs/binfmt_misc.c:1025]. | virtiofs, erofs and ext4 cannot be mounted this way; virtiofs carries only `FS_ALLOW_IDMAP` [linux: fs/fuse/virtio_fs.c:1787; fs/super.c:741-755]. |
| Device nodes | None can be created. | `vfs_mknod` requires `capable(CAP_MKNOD)` in the init userns; the only exception is the 0/0 overlay whiteout [linux: fs/namei.c:5109-5117]. Every superblock created in a non-init userns is `SB_I_NODEV` [linux: fs/super.c:358-359]. So rootless engines bind-mount existing nodes, as Podman does [podman: options/device.md:26-27]. |
| Overlayfs | Unprivileged mounts work. `-o userxattr` switches to `user.overlay.*` and is described as "useful for unprivileged mounting" [linux: Documentation/filesystems/overlayfs.rst:867-869]. | The mounting task must not gain privilege, but other tasks MAY gain privilege through the overlay [overlayfs.rst:292-330]. `metacopy=on` is unsafe with untrusted layers [overlayfs.rst:399-404]. Overlay bugs reachable from a userns have occurred [cve: CVE-2021-3493; CVE-2023-0386]. |
| Idmapped mounts | Available only to whoever owns the superblock's userns. | Requires CAP_SYS_ADMIN in the userns that owns the superblock, `FS_ALLOW_IDMAP`, and a mount not yet attached [linux: fs/namespace.c:4799-4838; man: mount_setattr(2)]. Supported by ext4, xfs, btrfs, erofs, squashfs, tmpfs, virtiofs and fuse; overlay only through idmapped layers [man: mount_setattr(2); linux: fs/erofs/super.c:902; fs/fuse/virtio_fs.c:1787]. |
| cgroup v2 delegation | Write access to the directory plus `cgroup.procs`, `cgroup.threads`, `cgroup.subtree_control` (or a cgroupns with `nsdelegate`); limits stay hierarchical, "nothing can escape" [linux: Documentation/admin-guide/cgroup-v2.rst:537-570]. | Moves need write access to the common ancestor's `cgroup.procs` [572-611]; no-internal-process rule [507-535]; `cgroup.max.*` caps the subtree [967-979]; migration is expensive, "organize once" [614-632]. `CLONE_INTO_CGROUP` is "significantly cheaper" than moving [man: clone(2)]; `favordynmods` trades fork/exit cost for migration latency [cgroup-v2.rst:194-200]. |
| Device controller | — | It is BPF-only (`BPF_PROG_TYPE_CGROUP_DEVICE`) and has no interface files [cgroup-v2.rst:2728-2750]. It counts as a net-admin program type [linux: kernel/bpf/syscall.c:2841-2861]. A BPF token could delegate it: bpffs `delegate_*` options need `capable(CAP_SYS_ADMIN)`, and creating the token needs CAP_BPF in the non-init userns that owns that bpffs [linux: kernel/bpf/inode.c:1107-1109; kernel/bpf/token.c:138-150]. **UNVERIFIED** end to end; see §4 Q5. |
| seccomp | Any task with `no_new_privs` (or CAP_SYS_ADMIN in its userns) [linux: Documentation/userspace-api/seccomp_filter.rst:69-75; man: seccomp(2)]. | "isn't a sandbox" [seccomp_filter.rst:33]; must check arch incl. the x32 bit [174-182; man: seccomp(2)]; allow-lists recommended [man: seccomp(2)]; allowing ptrace lets code escape [146-152]. User notification "must not be used to make security policy decisions"; `…_FLAG_CONTINUE` is TOCTOU-prone [man: seccomp_unotify(2) §"Design goals"; Garfinkel03 §4.3]. |
| no_new_privs | Irreversible and inherited. After it is set, exec cannot add privileges through setuid bits or file capabilities [linux: Documentation/userspace-api/no_new_privs.rst:18-30]. | It does not stop privilege changes that don't involve exec [no_new_privs.rst:41-43]. |
| Landlock | Any process can sandbox itself, given NNP [linux: Documentation/userspace-api/landlock.rst:7-20; man: landlock_restrict_self(2)]. ABI versions add, in order: 1 filesystem, 2 REFER, 3 TRUNCATE, 4 TCP bind/connect, 5 IOCTL_DEV, 6 abstract-UNIX and signal scoping, 7 audit-log flags, 8 TSYNC, 9 pathname-UNIX sockets, 10 UDP [landlock.rst:678-790]. | Once applied it cannot be removed [landlock.rst:300]. A landlocked thread cannot call mount or pivot_root [615-622]. At most 16 rulesets can be stacked [639-648]. Overlay layers and the merged directory are separate hierarchies [344-368]. IOCTL_DEV applies only to newly opened files [656-676]. |
| Bounding set and securebits | Capabilities dropped from the bounding set cannot be restored; dropping them needs CAP_SETPCAP. `SECBIT_NOROOT` plus `_LOCKED` stops uid 0 from regaining capabilities on exec [man: capabilities(7) §"Capability bounding set", §"The securebits flags"]. | — |
| pidfd | Race-free process handles; a PID cannot be reused while a pidfd refers to it [man: pidfd_open(2)]. | `pidfd_getfd` needs PTRACE_MODE_ATTACH_REALCREDS [man: pidfd_getfd(2)]. |
| Networking | Changing links over rtnetlink or nfnetlink needs CAP_NET_ADMIN over the userns that owns the target netns [linux: net/core/rtnetlink.c:7003, 2667-2683; net/netfilter/nfnetlink.c:659]. Creating a TAP has the same requirement. Opening a persistent TAP owned by the caller needs no capability [linux: drivers/net/tun.c:515-523, 2827]. | Loopback and bridge devices are `netns_immutable`; virtio_net is not [linux: include/linux/netdevice.h:2089; drivers/net/loopback.c:176; net/bridge/br_device.c:493]. Physical devices move back to the init netns when their netns is destroyed [man: network_namespaces(7)]. |
| /dev/kvm | Controlled only by file permission [linux: Documentation/virt/kvm/api.rst:10-17]. | Only disabling NX huge pages needs CAP_SYS_BOOT in the init userns [linux: arch/x86/kvm/x86.c:6940-6951]. |
| userfaultfd | `UFFD_USER_MODE_ONLY` is always allowed [linux: Documentation/admin-guide/mm/userfaultfd.rst:55-81]. | Handling kernel-mode faults needs CAP_SYS_PTRACE, `vm.unprivileged_userfaultfd=1`, or access to `/dev/userfaultfd`. Faults without FAULT_FLAG_USER are skipped for user-mode-only contexts [linux: mm/userfaultfd.c:2722, 4481-4488]. |
| VFIO | Usable once an admin binds the device to vfio-pci and chowns `/dev/vfio/$GROUP` or `/dev/vfio/devices/vfioX` [linux: Documentation/driver-api/vfio.rst:159-164, 319-322]. | Pinned DMA pages count against RLIMIT_MEMLOCK unless the process has `capable(CAP_IPC_LOCK)` [linux: drivers/vfio/vfio_iommu_type1.c:1588, 1659]. iommufd accounts per user by default, and changing that mode needs privilege [linux: include/uapi/linux/iommufd.h:304-307]. |

### 2.2 Q2: Literature on unprivileged containers and the case for a VM boundary

**Unprivileged containers work, but have a ceiling.**
- Charliecloud runs Docker images with only user and mount namespaces, "no privileged operations or daemons" [Priedhorsky17 Abstract]; unprivileged processes may map only their EUID [§2.2]; its test suite (chroot escape, device-file creation, privileged ports, root remount, setgroups, seteuid, cross-user signals) found no privileged functionality [§3.2.1].
- SC'21 defines Type I (no userns), Type II (privileged helper, many IDs) and Type III (unprivileged, one ID) [Priedhorsky21 §2.2]. Type III builds fail on `chown(2)`/setgroups in package managers [§2.3, Figs. 2–3]; Type II needs privileged helpers and correct subuid files [§3]; rootless Podman without helpers still fails installs [§4.1.1].

**Isolation through a shared kernel is weak.**
- In default Docker, 50 of 88 representative exploits (56.82%) worked. Capabilities, seccomp and MAC blocked more attacks than namespaces or cgroups, and 11 exploits still escaped through kernel privilege escalation [Lin18 Abstract, §4.2].
- Kernel bugs reachable by unprivileged users are routine: fs_context overflow exploitable "in case of unprivileged user namespaces enabled" (CVE-2022-0185), x_tables "through user name space" (CVE-2021-22555), nf_tables UAF giving unprivileged local users root (CVE-2023-32233) [cve].
- cgroup accounting leaks. Work pushed outside a container's cgroup reached 200× its CPU limit and slowed neighbours by 95%; work done by the container engine alone was about 3× [Gao19 Abstract, §4 Table 2].
- eBPF tracing ignores container UID and PID namespaces, and capabilities cannot grant eBPF partially [He23 §1].

**Costs and benefits of the VM boundary.**
- Firecracker: for containers, seccomp-bpf is "the most important security isolation boundary", at a compatibility cost [Agache20 §2.1.1]; one VMM process per microVM [§3]; block devices chosen over filesystem passthrough for security [§3.1]; jailer = chroot, pid/net namespaces, privilege drop, seccomp (24 syscalls + 30 ioctls then) [§3.4.1]; ~3 MB VMM overhead [§5.2, Fig. 7].
- Firecracker and gVisor both run *more* host-kernel lines than native. Lines executed (Table 2): host 63,163; Firecracker 77,392; LXC 90,595; gVisor 91,161. MicroVMs reduce how often host code runs, not how much of it is reachable [Anjali20 §3.3, §8].
- gVisor's user-space kernel is at least 2.2× slower per simple syscall, and 216× slower for open/close on an external tmpfs [Young19 §1].

### 2.3 Q3: VMM attack surface and how it gets broken

**Attack surfaces:** port I/O, MMIO, PCI DMA, hypercalls and instruction emulation. The attacker owns the guest kernel, and DoS is in scope [Schumilo20 Table I, §III.A].

**Fuzzers and their harness shapes**

| Fuzzer | Harness shape | Result |
|---|---|---|
| Hyper-Cube | Custom guest OS running a bytecode interpreter; blind, high throughput [Schumilo20 Fig. 2] | 54 bugs, 43 CVEs [Abstract] |
| Nyx | Coverage-guided; each snapshot reset restores only dirty pages; affine-typed input specs [Schumilo21 §3, Fig. 6] | 44 bugs, including bhyve virtqueue crashes (`vq_has_descs`, `vq_endchains`), six virtio-blk assertions and a QEMU infinite loop [Abstract; Table 3] |
| V-Shuttle | "DMA redirection": every guest-memory read by the device returns fuzz input; seed pools [Pan21 §3.3–3.4] | 35 bugs, 17 CVEs [Abstract] |
| Morphuzz | Generic PIO/MMIO/DMA, run with and without ASan. Names reentrancy and double fetch as bug classes specific to virtual devices [Bulekov22 §2.2, §5.2] | 110 bugs across 33 devices. Includes a virtio-gpu UAF where DMA targeted the VIRTIO MMIO reset register, and a PCNET double fetch [Abstract, §5.2, §5.2.3.1–2] |
| ViDeZZo | Grammar of dependencies within and between messages | 28 new bugs [Liu23 Abstract] |
| Truman | Device models derived from the guest drivers | 34% more virtio coverage than Morphuzz; 54 bugs. Example: a virtio-snd overflow via an unvalidated config write [Ma25 Abstract, §II-B] |
| HyperPill | Snapshot fuzzing through the VT-x interface | 26 bugs, including a virtio-net OOB in macOS Virtualization.framework. VT-x only [Bulekov24 Abstract, §5.3, §6] |

**Oracles**
- ASan detects heap, stack and global out-of-bounds accesses and use-after-free, at a 73% average slowdown [Serebryany12 Abstract].
- A double fetch is a value read twice that an attacker can change between the two reads. Fixes: copy once, use only one of the values, compare them, or overwrite [Wang17 §1, §5.3].

**Bug classes from CVE records** [cve: each ID]

| Class | Records |
|---|---|
| Chain loops | Zero-length descriptor causes an infinite loop (CVE-2016-6490). A huge length causes a NULL dereference (CVE-2016-7422). |
| Length and offset arithmetic | Invalid vhost lengths during migration lead to host privilege escalation (CVE-2019-14835). Firecracker vsock overflow (CVE-2019-18960). virtio-crypto `src_len≠dst_len` (CVE-2023-3180). vhost-user-gpu OOB write (CVE-2021-3546). Firecracker virtio-PCI OOB write when queue registers are modified after activation (CVE-2026-5747). |
| Unbounded resources | `virtqueue_pop` memory exhaustion (CVE-2016-5403). Firecracker serial buffer (CVE-2020-27174) and network-stack freeze (CVE-2020-16843). |
| Lifetime and UAF | virtio-net write after unmap (CVE-2021-3748). Cloud Hypervisor virtio-blk UAF when two chains reuse one `head_index` under async I/O (CVE-2026-45782). DMA reentrancy triggers a reset, then a UAF (CVE-2021-3750, CVE-2021-3929). Leak on an error path (CVE-2022-26354). |
| Double fetch on shared memory | Xen PV backends (CVE-2015-8550). |
| Rust guest-memory primitives | vm-memory OOB (CVE-2023-41051) and improper `read_obj`/`write_obj` access (CVE-2020-13759). vmm-sys-util unchecked length (CVE-2023-50711). |
| File sharing | virtiofsd lets the guest create device nodes (CVE-2020-35517) and setgid files (CVE-2022-0358). Kata ran virtiofsd as root with `--sandbox none --seccomp none`, and guest root drove it through a virtqueue it built itself (CVE-2026-47243). Annotations injected virtiofsd arguments (CVE-2026-44210). Guest writes persisted into a shared base image (CVE-2020-2025). |
| GPU paravirtualization | virglrenderer OOB gives arbitrary access inside crosvm's sandboxed process (CVE-2025-2509). |
| User-mode networking | libslirp `tcp_emu` overflows (CVE-2019-6778, CVE-2020-7039). |
| Privileged helpers | Firecracker jailer symlink attack when run as root (CVE-2026-1386). crun with libkrun, run rootful with passt, gives host root (CVE-2026-84042). |

**Existing practice and the spec.** Firecracker bounds chains with a TTL equal to the queue size, rejects head/next ≥ queue size, requires a power-of-two size ≤ max, and rejects avail-index jumps > queue size [fc: src/vmm/src/devices/virtio/queue.rs:92-167, 321, 445-460]; its Kani harnesses verify guest-facing parsers only under stated assumptions [fc: docs/formal-verification.md]. The spec's driver-side "MUST NOT"s (chains ≤ queue size, no INDIRECT+NEXT, no queue-register access once ready) are exactly what a hostile guest breaks; the device MUST NOT write device-readable buffers or the descriptor table, MUST NOT touch a non-ready queue, and must stop queue interaction after reset [virtio §2.7.5.3.1, §4.1.4.3.2, §4.2.2.2, §2.7.5.1, §4.2.2.1, §2.4.1].

### 2.4 Q4: Host-side hardening for an unprivileged VMM

**Linux: how Firecracker confines itself.** Per-thread allow-list filters load before guest code runs [fc: docs/seccomp.md:7-12]; x86_64, default action trap: VMM 51 syscalls (14 argument-filtered), API 32 (9), vCPU 27 (7) [fc: resources/seccomp/x86_64-unknown-linux-musl.json]. Filters are compiled to BPF at build time and embedded [docs/seccomp.md:22-30]; installation sets NNP then `SECCOMP_SET_MODE_FILTER` [fc: src/vmm/src/seccomp.rs:94-130].

**Linux: the Firecracker jailer** runs as root [fc: docs/jailer.md:291-292]. It closes fds, clears env, writes cgroups, unshares a mountns + pivot_root + chroot, `mknod`s/chowns /dev/kvm and /dev/net/tun, joins a netns (optional pid ns), sets rlimits (`fsize`, `no-file`), drops uid/gid and execs [jailer.md:114-160, 221-236; docs/prod-host-setup.md:125-140]. Jail creation slows 2× (10 parallel, no mounts) to 10× (500 host mounts) [jailer.md:299-306]; one uid/gid per VM is advised [prod-host-setup.md:107-113].

**Linux: other host facts**
- Linux 6.1 slowed `KVM_CREATE_VM` via a cgroup rwsem; the fixes (`favordynmods`, `kvm.nx_huge_pages=never`) need root [prod-host-setup.md:389-466]. Disable SMT and KSM [327-341]. Firecracker does not filter egress [fc: docs/design.md:100-102]; libkrun treats VMM and guest as one security context to confine with namespaces [krun: README.md:93-97].
- seccomp cost: up to 18% throughput with linear un-JITed filters vs 7% skip-list, ≤6% with JIT; SSB mitigation on seccomp'd threads added ≈10% [DeMarinis20 §4]; x86 now defaults to `prctl`, so seccomp no longer implies SSBD [linux: Documentation/admin-guide/kernel-parameters.txt:7152-7187].
- Rootless networking at MTU 1500 (project CI benchmark, not peer-reviewed): slirp4netns 1.69, pasta 0.24, gvisor-tap-vsock 2.46 Gbps vs 49.1 Gbps for SUID `lxc-user-nic` [rlk: docs/network.md:11-22]; host loopback leaks unless disabled [network.md:86-88]; passt needs no capabilities [passt(1) §DESCRIPTION].

**macOS: Hypervisor.framework**
- Every process that uses Hypervisor.framework needs `com.apple.security.hypervisor` [apple: Hypervisor §Entitlements; com.apple.security.hypervisor]. Apple describes the caller as an "entitled, sandboxed, user-space process" [apple: Hypervisor overview].
- One VM per process [apple: Hypervisor §Virtual Resource Mapping; sdk: Hypervisor.framework/Headers/hv_vm.h:28-33].
- The 26.4 headers have no device-assignment, IOMMU or DMA API; the only match was the `INVPCID` instruction [sdk: Hypervisor.framework/Headers/*.h]. GPUs can therefore only be paravirtualized.
- libkrun provides virtio-gpu through rutabaga and virglrenderer, using venus or native context [krun: src/devices/Cargo.toml:17,45; README.md:24,59].

**macOS: networking**
- vmnet needs `com.apple.vm.networking`. Apple describes it as managing interfaces "without escalating privileges to the root user", and says it is "restricted … contact your Apple representative" [apple: com.apple.vm.networking; vmnet §Entitlements].
- The macOS 26 `vmnet_network_*` API states no requirement [sdk: vmnet.h:1009-1171]. VZ says vmnet "requires an entitlement to create or configure a network" [sdk: VZVmnetNetworkDeviceAttachment.h:31-35].

**macOS: code signing and sandboxing.** Hardened Runtime is required for notarization and has six runtime exceptions (JIT, unsigned executable memory, DYLD env, library validation, executable page protection, debugger) [apple: Hardened Runtime]. App Sandbox grants files only via open/save panels and security-scoped bookmarks [apple: Accessing files from the macOS App Sandbox]; `sandbox-exec(1)` and `sandbox_init(3)` are DEPRECATED [man-macos]. libkrun and go-microvm ad-hoc sign with the entitlement [krun: examples/Makefile:29; gmv: docs/MACOS.md:43-59]; Apple doesn't document that this suffices (**UNVERIFIED**).

### 2.5 Q5: Operations that inherently need privilege

| Operation | What it needs | Rootless route |
|---|---|---|
| Host: open /dev/kvm | File permission | Group or ACL, set by an admin once [linux: api.rst:10-17] |
| Host: jail the VMM | A userns, which an LSM may veto | A userns mapping only the caller's uid, plus mountns, pivot_root and netns. Landlock and seccomp need no userns [§2.1] |
| Host: CPU and memory limits | A delegated cgroup | systemd `Delegate=` [systemd: docs/CGROUP_DELEGATION.md:182-200] |
| Host: TAP device | CAP_NET_ADMIN over the netns owner | A user-mode stack, or an admin-created TAP owned by the user [linux: drivers/net/tun.c:515-523] |
| Host: ports below 1024 | CAP_NET_BIND_SERVICE or `ip_unprivileged_port_start` | Admin sysctl [docker: rootless/tips › Exposing privileged ports] |
| Host: GPU passthrough | Bind to vfio-pci, chown the node, RLIMIT_MEMLOCK ≥ pinned RAM | Admin once [linux: vfio.rst:159-164; vfio_iommu_type1.c:1588] |
| Host: lazy snapshot restore through UFFD | `/dev/userfaultfd` access or a sysctl | Admin once [linux: userfaultfd.rst:55-81] |
| macOS: HVF and vmnet | An entitlement for HVF; for vmnet, a restricted entitlement or root | Sign the binary; use user-mode networking [apple] |
| Guest: mount block or virtiofs roots, devtmpfs, cgroup2 | CAP_SYS_ADMIN in the init userns | PID 1 at boot [man: user_namespaces(7)] |
| Guest: device nodes | CAP_MKNOD in the init userns | Kernel devtmpfs; PID 1 chowns; bind-mount into containers [linux: fs/namei.c:5109-5117] |
| Guest: many-ID map for the engine | CAP_SETUID/SETGID in the parent userns | PID 1 writes the map; no subuid or newuidmap needed in the VM [linux: kernel/user_namespace.c:1172-1212] |
| Guest: hand the NIC to the engine | CAP_NET_ADMIN over both netns | PID 1 moves virtio_net into the engine's netns [linux: rtnetlink.c:2667-2683] |
| Guest: idmaps for shared volumes | CAP_SYS_ADMIN over the superblock's owner | PID 1 at boot, or FUSE/tmpfs mounted inside the engine's userns [linux: fs/namespace.c:4799-4838] |
| Guest: device-cgroup BPF | CAP_SYS_ADMIN in the init userns, or a BPF token | PID 1 delegates a bpffs (**UNVERIFIED**) [linux: kernel/bpf/inode.c:1107-1109] |
| Guest: global sysctls, modules, clock | Init userns | Kernel command line or built-in modules; PID 1 at boot [man: user_namespaces(7)] |

### 2.6 User-configurable isolation controls: the Docker and OCI contract

**`--privileged`** "gives all capabilities", all host devices, and relaxes AppArmor/SELinux [docker: engine/containers/run › Runtime privilege and Linux capabilities]. In moby: all caps [moby: daemon/pkg/oci/caps/utils.go:81-85], all host devices with allow-all `rwm` [daemon/oci_linux.go:853-879], masked/read-only paths cleared and `/sys` writable [oci_linux.go:655-665], seccomp off unless a profile is named [daemon/seccomp_linux.go:21-30]. Rootless: `--cap-add` covers only userns-governed resources [docker: rootless/troubleshoot › Known limitations]; userns containers "cannot have more privileges than the user that launched them" [podman: options/privileged.md:22-23].

**Docker defaults**
- 14 capabilities [moby: daemon/pkg/oci/caps/defaults.go:4-21].
- Masked paths include `/proc/kcore`, `/proc/keys` and `/sys/firmware`. Read-only: `/proc/{bus,fs,irq,sys,sysrq-trigger}`. The default namespaces include time [moby: daemon/pkg/oci/defaults.go:109-124, 191-216].
- Device allow-list: deny everything, allow `c 1:3,1:5,1:8,1:9,5:0,5:1 rwm`, and deny `c 10:229` (fuse) [defaults.go:130-188].

**Docker's default seccomp profile** (profiles v0.2.3): default `SCMP_ACT_ERRNO` (EPERM); 361 names allowed unconditionally by my count (docs: "around 44" disabled); CAP_SYS_ADMIN unlocks mount/unshare/setns/fsopen/…; `clone3` → ENOSYS without it; `socket` allowed except AF_ALG (38) and AF_VSOCK (40) [moby: vendor/github.com/moby/profiles/seccomp/default.json:2-3, 436-476, 634-667, 718-724; docker: engine/security/seccomp].

**Other flags**
- `--device` grants `rwm`. `--device-cgroup-rule` allows dynamic device majors [docker: reference/cli/docker/container/run › --device-cgroup-rule; moby: oci_linux.go:881-896].
- `--gpus` uses CDI when `nvidia-cdi-hook` exists, and otherwise an OCI prestart hook [moby: daemon/devices_nvidia_linux.go:38-60, 108-150].
- `--security-opt` covers seccomp, apparmor, label, no-new-privileges and `systempaths=unconfined`. `--sysctl` accepts only namespaced keys [docker: reference › --security-opt, --sysctl].
- Rootless Docker ignores resource flags unless cgroup v2 and systemd are present. It lacks AppArmor, checkpoint, overlay networks and SCTP [docker: rootless/tips › Limiting resources; rootless/troubleshoot › Known limitations].

**OCI runtime-spec** (main, after v1.3.0): devices via mknod *or* bind mount, default devices mandatory [oci: config-linux.md:125-190]; device allow-list [402-440]; seccomp incl. `listenerPath` for `SCMP_ACT_NOTIFY` [874-960]; masked/read-only paths [1065-1090]; cgroup ownership changes only with a new cgroupns, never on v1 [335-350]; `netDevices` [192-240]; per-mount `idmap`/`ridmap` [oci: config.md:154-155]; five capability sets and `noNewPrivileges` [config.md:286-300]; no Landlock field.

**Research on generating policies**
- Confine removed at least 145 of 326 syscalls for more than half of 150 images. That neutralized 51 CVEs beyond the 25 Docker's default already covers [Ghavamnia20a Abstract, §7.3.2]. It can miss programs launched through library calls [§8].
- Temporal specialization installs a stricter filter at the switch from initialization to serving, then blocks `prctl` and `seccomp`. It removed 51% more security-critical syscalls than library specialization and neutralized 13 more privilege-escalation kernel vulnerabilities [Ghavamnia20b Abstract, §5.3].

**What a VM can reach on the host**
- A guest vsock connection to host port N succeeds only if a UDS listens at `<uds_path>_N`. The allow-list is made of filesystem objects [fc: docs/vsock.md:80-95].
- libkrun's TSI proxies guest sockets through the VMM [krun: README.md:66-80]. So whatever the VMM can reach, the guest can reach too [README.md:93-97].
- Apple's containerization runs one VM per container. A tiny init, `vminitd`, serves gRPC over vsock [apple-cz: README.md:27-32].

## 3. Implications for shards (ranked)

**R1 (P0): Define "no root" as invariants that CI can check.**
- **Host:** no euid 0, no file capabilities, no setuid helper; only the one-time admin grants of §2.5 (`/dev/kvm` group; optionally TAP, VFIO + memlock, `/dev/userfaultfd`). Root-run jailers and integrations keep producing CVEs [cve: CVE-2026-1386, CVE-2026-84042].
- **Guest:** once PID 1 finishes provisioning (before any image or agent input), no process but kernel threads holds a capability in the init userns; PID 1 ends with an empty bounding set, `SECBIT_NOROOT` + `_LOCKED`, and NNP [man: capabilities(7)]; engine and containers run with NNP, seccomp and Landlock. CI reads `/proc/*/status` (CapEff/CapBnd/NoNewPrivs/Seccomp) and `/proc/*/uid_map` in the guest.

**R2 (P0): Guest boot follows a provision-then-drop PID 1.** This is the vminitd pattern [apple-cz: README.md:27-32]. Every step needs init-userns privilege, per §2.5. In order:
1. Mount the rootfs (erofs), virtiofs volumes, devtmpfs, proc, sys, and cgroup2 with `nsdelegate`.
2. Create and chown the engine's cgroup, plus its `cgroup.procs`, `cgroup.threads` and `cgroup.subtree_control`. Set `cgroup.max.*` [linux: cgroup-v2.rst:537-611, 967-979].
3. Start the engine with `clone3(CLONE_NEWUSER|NEWNS|NEWNET|NEWCGROUP|NEWPID|INTO_CGROUP|PIDFD)` [man: clone(2)].
4. Write a large ID map directly, e.g. 0→100000 with 2^24 IDs. No subuid or newuidmap is needed in the VM. Use at most 5 extents, because CVE-2018-18955 was in the path for more than 5 [linux: kernel/user_namespace.c:1172-1212; cve].
5. Move virtio_net into the engine's netns. The engine then owns addresses, bridges, veths and nftables at kernel speed, with no slirp inside the VM [linux: rtnetlink.c:2667-2683; nfnetlink.c:659; oci: config-linux.md:192-240].
6. Chown GPU and other device nodes into the engine's ID range. Create every idmapped volume mount the compose file declares, now [linux: fs/namespace.c:4799-4838].
7. Optionally, delegate a bpffs for device-cgroup BPF (§4 Q5).
8. Drop everything: `capset(∅)`, `PR_CAPBSET_DROP` for all capabilities, lock the securebits, set NNP. From then on PID 1 only reaps and forwards.

*Consequence:* changing volumes or devices after boot needs a VM respawn, which is cheap by design, rather than a privileged broker, which would break R1.

**R3 (P0): Confine the host VMM without a root jailer.**
- **Linux:** one process per VM [Agache20 §3]; a userns mapping only the caller's uid (no helper) plus a mountns; pivot_root into an empty tmpfs with `/dev/kvm` (and a TAP fd or `/dev/userfaultfd` if granted) bind-mounted, never mknod'ed (§2.1); start via `CLONE_INTO_CGROUP` in the delegated cgroup; rlimits NOFILE/FSIZE/NPROC/MEMLOCK [man: getrlimit(2)]; per-thread allow-list seccomp compiled at build time with arch checks (§2.4); Landlock for fs, TCP/UDP ports and signal/abstract-UNIX scope — the one layer that survives a distro veto of userns (§2.1). Keep few host mounts visible; jail creation cost grows with mount count [fc: docs/jailer.md:299-306].
- **macOS:** HVF forces one process per VM; sign with `com.apple.security.hypervisor` only and link statically to avoid the library-validation/DYLD exceptions (§2.4). A CLI has no supported programmatic sandbox, so isolate risky parsers (network stack, GPU renderer, file server) in separate low-privilege processes.

**R4 (P0): Virtio hardening checklist.** Each item maps to a spec rule or a CVE in §2.3.
1. Read each descriptor or ring field once into host memory, validate it, and use only that copy [Wang17 §5.3; Bulekov22 §5.2.3.1; cve: CVE-2015-8550].
2. Bound every chain: head and next indices below the queue size; total length, including indirect descriptors, no more than the queue size. Reject INDIRECT together with NEXT, and nested indirect tables [virtio §2.7.5.3.1–2; fc: queue.rs:92-167].
3. Don't loop on len = 0. Use checked arithmetic for `addr+len` and for sums. Cap bytes per device. Check cross-field lengths such as `src_len` against `dst_len` [cve: CVE-2016-6490, CVE-2016-7422, CVE-2023-3180, CVE-2019-14835].
4. Use a single guest-memory accessor. It checks the whole range against RAM regions, does volatile accesses of the correct width, and hands out no long-lived references [cve: CVE-2023-41051, CVE-2020-13759, CVE-2023-50711].
5. Reject an avail-index delta larger than the queue size [fc: queue.rs:445-460].
6. Allow each head in flight only once. Keep buffers, including bounce buffers, alive until completion, even across a reset [cve: CVE-2026-45782, CVE-2021-3748].
7. Enforce the transport state machine. After enable or DRIVER_OK, ignore queue-register writes or set DEVICE_NEEDS_RESET. Require a non-zero power-of-two queue size no larger than the maximum. Never touch a queue that is not ready [virtio §4.1.4.3.2, §4.2.2.1–2; cve: CVE-2026-5747].
8. Treat reset as a barrier: quiesce the I/O threads, finish or cancel in-flight work, and drop guest-memory references before reporting status 0 [virtio §2.4.1; Bulekov22 §5.2.3.2; cve: CVE-2021-3929].
9. Allow DMA to guest RAM only, never to an MMIO or PIO range. This prevents re-entrancy [cve: CVE-2021-3750; Bulekov22 §2.2].
10. Bound host resources per queue and per device, and add rate limiters [cve: CVE-2016-5403, CVE-2020-27174, CVE-2020-16843; Agache20 §3.3].
11. On every error path, detach or return the element and release its mappings [cve: CVE-2022-26354].
12. Never write device-readable buffers or the descriptor table [virtio §2.7.5.1].
13. Validate config-space writes. Size internal state only from values the device has validated [Ma25 §II-B].
14. Treat snapshot files as untrusted guest input [fc: queue.rs:314-330].
15. File sharing:
    - Assume guest root speaks raw FUSE.
    - Resolve paths with `openat2(RESOLVE_BENEATH|RESOLVE_NO_MAGICLINKS)` [man: openat2(2)].
    - Refuse to create device, setuid or setgid files.
    - Run the file server unprivileged, under Landlock and seccomp.
    - For images, prefer read-only erofs or block devices with a per-VM copy-on-write layer.

    [cve: CVE-2020-35517, CVE-2022-0358, CVE-2026-47243, CVE-2020-2025; Agache20 §3.1]
16. Run the GPU renderer and the user-mode network stack in separate sandboxed processes. No host loopback by default [cve: CVE-2025-2509, CVE-2021-3546, CVE-2019-6778; rlk: docs/network.md:86-88].
17. Never let metadata the workload controls become arguments to a host daemon [cve: CVE-2026-44210].

**R5 (P0): Fuzzing and stress plan.**
- **H1, per device, in-process (cargo-fuzz).** Input is an op sequence: config r/w, status, queue setup, notify, guest-memory patches, async-completion order, reset, snapshot/restore. Guest memory is fuzz-backed as in V-Shuttle [Pan21 §3.3], in a *consistent* mode and an *adversarial re-read* mode (every read returns fresh bytes); behaviour that differs between them flags a double fetch. Generators: a spec/driver-derived state machine that reaches post-DRIVER_OK states [Liu23; Ma25], plus a raw mode that breaks every "driver MUST NOT".
- **H2, whole VMM without a hypervisor.** Real device threads and event loop, driven by a synthetic vCPU injecting MMIO exits; deterministic, runs on the macOS/arm64 dev host; dirty-page reset per input as in Nyx [Schumilo21 §3].
- **H3, full system on KVM and HVF.** A Hyper-Cube-style in-guest agent drives MMIO/virtqueues from several vCPUs to hit vCPU-vs-I/O-thread races (CVE-2026-5747 class) [Schumilo20 Fig. 2]; HyperPill is VT-x only [Bulekov24 §6].
- **Oracles:** panic/abort; ASan build for `unsafe`/FFI [Serebryany12]; per-input step/time budget; per-input host-allocation ceiling; R4 invariants 2, 6, 7, 8, 12.
- **Also:** Kani proofs for queue parsing, as Firecracker does [fc: docs/formal-verification.md]; nightly soak (reset storms, max-length chains, reordered completions, tight rlimits); fuzz the host-side vsock control-channel parser and the in-VM Engine API/Compose parsers.

**R6 (P1): Networking.** Host: default to a user-mode stack beside the VMM (no root, no vmnet entitlement), block host loopback and enforce egress allow-lists there — Firecracker filters nothing [fc: docs/design.md:100-102]; offer an admin-provisioned TAP as the fast tier (§2.4). Guest: the engine owns the NIC (R2 step 5).

**R7 (P1): One layered policy: host ⊇ VM ⊇ container.** Reject any request that exceeds its parent at load time. Never drop it silently, as rootless Docker does [docker: rootless/tips › Limiting resources].
- **VM policy (host VMM + jail):** vCPU/memory/balloon; device set (absent device = no attack surface [Agache20 §3.1]); GPU mode {none, paravirtual, VFIO}; file shares {path, ro/rw, idmap} backed by Landlock rules; read-only images + per-VM CoW; network {none, user-mode + egress allow-list, TAP}; vsock host-port allow-list, one UDS per port [fc: docs/vsock.md:80-95]; rate limits; the VMM's own seccomp/Landlock/rlimits.
- **Container policy (in-VM engine → kernel), Docker-compatible with rootless meanings:** `cap-add` acts only inside the container userns; `privileged` = all caps in that userns + no seccomp/masks + every device the VM exposes, never init-userns power (§2.6); moby default seccomp as the floor (incl. AF_VSOCK/AF_ALG blocks); NNP on by default; devices only by bind mount; GPUs via static CDI-like specs with no image-supplied hooks [cve: CVE-2024-0132, CVE-2025-23266]; inode-verified path masking [cve: CVE-2025-31133, CVE-2025-52565, CVE-2025-52881]; per-container sub-range userns; cgroup v2 limits via `CLONE_INTO_CGROUP`.
- **Beyond Docker:** per-container Landlock (paths, ports, signal/abstract-UNIX scope), applied by container init after pivot_root since landlocked threads can't mount [linux: landlock.rst:615-622]; per-network egress rules; per-image generated seccomp (Confine) plus an opt-in serving-phase filter (Temporal) [Ghavamnia20a; Ghavamnia20b §5.3]; seccomp user-notify only for syscall emulation, never allow/deny [man: seccomp_unotify(2)].

**R8 (P2): Keep the ≤5 ms start path fast.**
- Precompiled, tree-shaped seccomp filters with JIT enabled [DeMarinis20 §4].
- Spawn with `CLONE_INTO_CGROUP` instead of migrating afterwards [man: clone(2)].
- A pre-built minimal mount-namespace template (see Q1).
- `favordynmods` as optional admin tuning [fc: prod-host-setup.md:389-466].

## 4. Open questions that need our own measurement

1. **Cost of jailing on the start path.** Micro-benchmark `clone3` with/without NEWUSER/NEWNS/NEWNET/INTO_CGROUP, `uid_map` writes, `pivot_root` with 10 vs 500 host mounts, Landlock create+restrict, seccomp load (JIT on/off), and `KVM_CREATE_VM` with/without `favordynmods`. Linux 6.x/7.x, arm64 and x86, 1 and 50 concurrent, 1,000 runs; p50/p99 via `CLOCK_MONOTONIC` + `perf trace`.
2. **Guest provisioning budget.** Timestamp every PID-1 step in R2 with a boot-time tracer. Target: 1 ms or less in total.
3. **HVF signing.** On macOS 26.4, ad-hoc sign (`codesign -s - --entitlements`) a binary that calls `hv_vm_create`. Run it as a normal user with and without hardened runtime, and once without the entitlement.
4. **vmnet on macOS 26.** Call `vmnet_network_create` and `vmnet_interface_start_with_network` from an unsigned and an ad-hoc-signed binary, as non-root. Record the status codes.
5. **Device policy through a BPF token.** PID 1 sets `delegate_cmds`, `delegate_progs` and `delegate_attachs` on a bpffs owned by the engine's userns, using `fsconfig`. The engine creates a token, then loads and attaches a `CGROUP_DEVICE` program to its delegated cgroup. Confirm that opening a blocked device is denied.
6. **Networking tiers.** Run iperf3 and netperf (TCP_STREAM, TCP_RR) against our user-mode stack and against a TAP, at MTU 1500 and 65520. Record CPU per Gbps on Linux and macOS.
7. **seccomp on VMM hot paths.** Measure guest-visible I/O latency with and without per-thread filters, tree versus linear filters, JIT on and off, on arm64 and x86.
8. **VFIO pinning.** Measure the time and RSS to DMA-map N GB with iommufd, using 4K and 2M pages. Decide whether GPU VMs need pre-warmed pools to meet 5 ms.
9. **UFFD without privilege.** Test whether KVM delivers guest-memory faults to a `UFFD_USER_MODE_ONLY` context. The expected answer is no, because faults without FAULT_FLAG_USER are skipped [linux: mm/userfaultfd.c:2722]. Benchmark alternative lazy-restore paths.
10. **NIC handoff.** Check that virtio_net moves into a netns the engine owns, and that the engine can create a bridge, veths and nftables rules there. Destroy the netns and confirm the device returns to the init netns.
11. **Hosts that veto user namespaces.** On Ubuntu 24.04+ with the AppArmor restriction on, run the Landlock-plus-seccomp jail and list what protection is lost.
12. **Fuzzer effectiveness.** Re-inject the CVE patterns from §2.3 as mutants. Measure time-to-detect for H1–H3 and for the adversarial re-read mode.
13. **"No root" audit.** In CI, have a guest agent dump `/proc/*/status` and `uid_map` after boot. Fail the run if any process holds a capability in the init userns.

## 5. References

**Peer-reviewed** (PDFs retrieved 2026-09-28)
- [Agache20] A. Agache, M. Brooker, A. Florescu, A. Iordache, A. Liguori, R. Neugebauer, P. Piwonka, D.-M. Popa, "Firecracker: Lightweight Virtualization for Serverless Applications," NSDI '20. https://www.usenix.org/system/files/nsdi20-paper-agache.pdf
- [Anjali20] Anjali, T. Caraza-Harter, M. M. Swift, "Blending Containers and Virtual Machines: A Study of Firecracker and gVisor," VEE '20, doi:10.1145/3381052.3381315. https://pages.cs.wisc.edu/~swift/papers/vee20-isolation.pdf
- [Bulekov22] A. Bulekov, B. Das, S. Hajnoczi, M. Egele, "Morphuzz: Bending (Input) Space to Fuzz Virtual Devices," USENIX Security '22. https://www.usenix.org/system/files/sec22-bulekov.pdf
- [Bulekov24] A. Bulekov, Q. Liu, M. Egele, M. Payer, "HyperPill: Fuzzing for Hypervisor-bugs by Leveraging the Hardware Virtualization Interface," USENIX Security '24. https://www.usenix.org/system/files/usenixsecurity24-bulekov.pdf
- [DeMarinis20] N. DeMarinis, K. Williams-King, D. Jin, R. Fonseca, V. P. Kemerlis, "sysfilter: Automated System Call Filtering for Commodity Software," RAID '20. https://www.usenix.org/system/files/raid20-demarinis.pdf
- [Gao19] X. Gao, Z. Gu, Z. Li, H. Jamjoom, C. Wang, "Houdini's Escape: Breaking the Resource Rein of Linux Control Groups," CCS '19, doi:10.1145/3319535.3354227. Author copy: https://www.cs.memphis.edu/~xgao1/paper/ccs19.pdf
- [Garfinkel03] T. Garfinkel, "Traps and Pitfalls: Practical Problems in System Call Interposition Based Security Tools," NDSS '03. https://www.ndss-symposium.org/wp-content/uploads/2017/09/Traps-and-Pitfalls-Practical-Problems-in-System-Call-Interposition-Based-Security-Tools-Tal-Garfinkel.pdf
- [Ghavamnia20a] S. Ghavamnia, T. Palit, A. Benameur, M. Polychronakis, "Confine: Automated System Call Policy Generation for Container Attack Surface Reduction," RAID '20. https://www.usenix.org/system/files/raid20-ghavamnia.pdf
- [Ghavamnia20b] S. Ghavamnia, T. Palit, S. Mishra, M. Polychronakis, "Temporal System Call Specialization for Attack Surface Reduction," USENIX Security '20. https://www.usenix.org/system/files/sec20-ghavamnia.pdf
- [He23] Y. He, R. Guo, Y. Xing, X. Che, K. Sun, Z. Liu, K. Xu, Q. Li, "Cross Container Attacks: The Bewildered eBPF on Clouds," USENIX Security '23. https://www.usenix.org/system/files/usenixsecurity23-he.pdf
- [Lin18] X. Lin, L. Lei, Y. Wang, J. Jing, K. Sun, Q. Zhou, "A Measurement Study on Linux Container Security: Attacks and Countermeasures," ACSAC '18, doi:10.1145/3274694.3274720. Author copy: https://csis.gmu.edu/ksun/publications/container-acsac18.pdf
- [Liu23] Q. Liu, F. Toffalini, Y. Zhou, M. Payer, "ViDeZZo: Dependency-aware Virtual Device Fuzzing," IEEE S&P '23, doi:10.1109/SP46215.2023.10179354. Author copy: https://hexhive.epfl.ch/publications/files/23Oakland4.pdf
- [Ma25] Z. Ma, Q. Liu, Z. Li, T. Yin, W. Tan, C. Zhang, M. Payer, "Truman: Constructing Device Behavior Models from OS Drivers to Fuzz Virtual Devices," NDSS '25. https://www.ndss-symposium.org/wp-content/uploads/2025-301-paper.pdf
- [Pan21] G. Pan, X. Lin, X. Zhang, Y. Jia, S. Ji, C. Wu, X. Ying, J. Wang, Y. Wu, "V-Shuttle: Scalable and Semantics-Aware Hypervisor Virtual Device Fuzzing," CCS '21, doi:10.1145/3460120.3484811. Author copy: https://nesa.zju.edu.cn/download/pgn_pdf_V-SHUTTLE.pdf
- [Priedhorsky17] R. Priedhorsky, T. Randles, "Charliecloud: Unprivileged Containers for User-Defined Software Stacks in HPC," SC17, doi:10.1145/3126908.3126925. The ACM PDF returned 403, so I read the author copy hosted at archive.fosdem.org (2018 containers_scientific attachment).
- [Priedhorsky21] R. Priedhorsky, R. S. Canon, T. Randles, A. J. Younge, "Minimizing Privilege for Building HPC Containers," SC '21, doi:10.1145/3458817.3476187. The ACM PDF returned 403. I read the authors' pre-print v3 (arXiv:2104.07508, 18 Aug 2021), which points to this version of record. Section numbers refer to that pre-print.
- [Schumilo20] S. Schumilo, C. Aschermann, A. Abbasi, S. Wörner, T. Holz, "HYPER-CUBE: High-Dimensional Hypervisor Fuzzing," NDSS '20. https://www.ndss-symposium.org/wp-content/uploads/2020/02/23096.pdf
- [Schumilo21] S. Schumilo, C. Aschermann, A. Abbasi, S. Wörner, T. Holz, "Nyx: Greybox Hypervisor Fuzzing using Fast Snapshots and Affine Types," USENIX Security '21. https://www.usenix.org/system/files/sec21-schumilo.pdf
- [Serebryany12] K. Serebryany, D. Bruening, A. Potapenko, D. Vyukov, "AddressSanitizer: A Fast Address Sanity Checker," USENIX ATC '12. https://www.usenix.org/system/files/conference/atc12/atc12-final39.pdf
- [Wang17] P. Wang, J. Krinke, K. Lu, G. Li, S. Dodier-Lazaro, "How Double-Fetch Situations turn into Double-Fetch Vulnerabilities: A Study of Double Fetches in the Linux Kernel," USENIX Security '17. https://www.usenix.org/system/files/conference/usenixsecurity17/sec17-wang.pdf
- [Young19] E. G. Young, P. Zhu, T. Caraza-Harter, A. C. Arpaci-Dusseau, R. H. Arpaci-Dusseau, "The True Cost of Containing: A gVisor Case Study," HotCloud '19. https://www.usenix.org/system/files/hotcloud19-paper-young.pdf

**Specifications and official documentation**
- [virtio] OASIS, *Virtual I/O Device (VIRTIO) Version 1.3*, Committee Specification Draft 01. https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html
- [oci] opencontainers/runtime-spec @6999a89 (main; latest tag v1.3.0), `config.md` and `config-linux.md`.
- [man] Linux man-pages 6.19 (man7.org):
  - user_namespaces(7), capabilities(7), namespaces(7), network_namespaces(7), mount_namespaces(7);
  - clone(2), unshare(2), pidfd_open(2), pidfd_getfd(2);
  - seccomp(2), seccomp_unotify(2), landlock(7), landlock_restrict_self(2);
  - mount_setattr(2), mknod(2), openat2(2), getrlimit(2), userfaultfd(2).
- [man] shadow-utils 4.19.0: newuidmap(1), newgidmap(1), subuid(5).
- [man-macos] sandbox-exec(1) and sandbox_init(3), from the local macOS 26.4 install.
- [linux] Linux 7.2-rc4 source tree (Makefile: VERSION 7, PATCHLEVEL 2, EXTRAVERSION -rc4).
- [docker] docs.docker.com: `/engine/security/rootless/`, `/engine/security/rootless/tips/`, `/engine/security/rootless/troubleshoot/`, `/engine/security/seccomp/`, `/engine/containers/run/`, `/reference/cli/docker/container/run/`.
- [apple] developer.apple.com/documentation, read through its JSON form:
  - Hypervisor;
  - BundleResources › Entitlements › com.apple.security.hypervisor, com.apple.vm.hypervisor, com.apple.vm.networking, com.apple.security.virtualization;
  - vmnet;
  - Security › Hardened Runtime; App Sandbox; Accessing files from the macOS App Sandbox.
- [sdk] `/Library/Developer/CommandLineTools/SDKs/MacOSX26.4.sdk`: Hypervisor, vmnet and Virtualization framework headers.
- [systemd] systemd @be1e78e, `docs/CGROUP_DELEGATION.md`.
- [podman] containers/podman @6f2c81b, `docs/source/markdown/options/{privileged,device}.md`.
- [rlk] rootless-containers/rootlesskit @e31dab4, `docs/network.md`. Its benchmark comes from the project's CI (Jul 26, 2026) and is not peer-reviewed.
- [passt(1)] https://passt.top/builds/latest/web/passt.1.html

**Source code**
- [fc] firecracker-microvm/firecracker @edb60617 (local, 2026-09-25).
- [krun] containers/libkrun @1f5dd028 (local).
- [gmv] go-microvm @7e148d85 (local).
- [moby] moby/moby @c3c2a9eb, with vendored `github.com/moby/profiles/seccomp` v0.2.3.
- [apple-cz] apple/containerization @bc994b88, `README.md`.

**CVE records** (https://cveawg.mitre.org/api/cve/<ID>)
- 2015–2019: CVE-2015-8550, CVE-2016-5403, CVE-2016-6490, CVE-2016-7422, CVE-2018-7169, CVE-2018-18955, CVE-2019-6778, CVE-2019-14835, CVE-2019-18960.
- 2020: CVE-2020-2025, CVE-2020-7039, CVE-2020-13759, CVE-2020-16843, CVE-2020-27174, CVE-2020-35517.
- 2021–2022: CVE-2021-3493, CVE-2021-3546, CVE-2021-3748, CVE-2021-3750, CVE-2021-3929, CVE-2021-22555, CVE-2022-0185, CVE-2022-0358, CVE-2022-26354.
- 2023–2024: CVE-2023-0386, CVE-2023-3180, CVE-2023-32233, CVE-2023-41051, CVE-2023-50711, CVE-2024-0132.
- 2025: CVE-2025-2509, CVE-2025-23266, CVE-2025-31133, CVE-2025-52565, CVE-2025-52881.
- 2026: CVE-2026-1386, CVE-2026-5747, CVE-2026-44210, CVE-2026-45782, CVE-2026-47243, CVE-2026-84042.
