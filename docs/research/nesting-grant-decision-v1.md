# The nesting grant: what a nested runtime under shards-init needs (decision record, v1)

The record behind D232 (docs/design/architecture.md). Step 38 of kiln plan
01a1cd8e862e7169a074db3d5fa33f3d; its planning research is kiln research
01a1cda5426f73cc86b87e7749163666, reproduced below as kiln graded it (each finding's
grade and score are kiln's, from its sources' kinds, how many agree and whether each quote
was found where cited; `f1` to `f38` are its findings in order).

- **Question.** What does Hex's node, now its own Shards guest's PID 1 (D215), need of
  shards-init to run as one designated workload's command instead, and how is each grant
  checked at build and at run?
- **Revisions read.** shards: origin/p/integrate-engine-20261111 032b63b1c (paths below are
  repo-relative there); D231: p/runtime-image-boot-v1 75c36864f; Hex: main 24c100f8 (its
  guest.rs, node.rs, bundle.rs and gpu.rs unchanged since cd78e56a); Linux: torvalds/linux
  79eb6679; man-pages: man7.org as fetched 2026-11-11; moby/profiles seccomp v0.2.3; moby
  1e9198df (its daemon/oci_linux.go and hack/dind); systemd b4259f37 (its
  constants.h, notify-recv.c and CGROUP_DELEGATION.md; systemd.service(5) and sd_notify(3)
  as fetched 2026-11-11); runc c0512b4f; opencontainers/cgroups b62147e9; BuildKit
  8cd4dbe1; Incus 3dcffe4b; Podman 32654652; CDI 9e21b8bb.
- **Method.** Each internal claim read in the code at those revisions; each external one in
  its primary source, fetched and quoted; one measurement (s20): shards' vendored seccomp
  default compared byte for byte with moby/profiles seccomp v0.2.3 and its rules listed.
  Nothing was built or run in a microVM for this record: what only a live run shows (f28's
  pivot_root, f20's bound on nested device programs) is owed to the real-VM test of plan
  step 5, run by ROOT.
- **Decision.** D232.

**Research:** Plan step 38 [Shards]: shards-init grants one designated workload what Hex's node needs, each grant checked at build and at run: Hex ROOT's 8-item list in the step (mounts and /run, a writable delegated cgroup v2 subtree, descriptor limits, ready-by time, GPU words and DRM render nodes, subreaper, exit policy). Rules as plan step 5's goal. Full step: kiln plan 01a1cd8e862e7169a074db3d5fa33f3d.

38 findings (A 30, B 6, C 2) from 48 sources (32 internal, 16 external)

- **C · 65 (moderate)** Step 38 grants one designated workload, by name and checked at build and at run, exactly: a writable delegated cgroup v2 subtree for its boxes, CAP_SYS_ADMIN for its boxes' namespaces (never a blanket --privileged), DRM render nodes through D44's device rules, and Hex ROOT's eight items (mounts and /run, descriptors, ready-by time, GPU words, channel, readiness, reaping, end); default deny holds and nothing else in the microVM gains from it. [1]
  _1 independent source (kiln's records); 4 of 4 quotes found where cited_
  - [1] "a writable, delegated cgroup v2 subtree for the boxes it makes" (step 38, description) · quote found
  - [1] "CAP_SYS_ADMIN for its boxes' namespaces, as a named grant checked at build and at run, not a blanket --privileged;" (step 38, description) · quote found
  - [1] "Default deny holds: each grant is explicit, and nothing else in the VM gains from it." (step 38, description) · quote found
  - [1] "Nothing in it needs PID 1 as such, only these grants:" (step 38, description) · quote found
- **A · 96 (high)** This step's work follows plan step 5's rules and R15: an implementer's own worktree from origin/p/integrate-engine-20261111 (032b63b1c or later), builds and light tests only on Linux runners through kiln run, no push; real-VM suites and measurements are Shards ROOT's; every literal is derived (R03) and waits are on notifications (R05). [1][2][3]
  _3 independent sources (kiln's records, a document); 6 of 6 quotes found where cited_
  - [1] "Implementers do scoped implementation and light tests only. ROOT owns every real-VM E2E, measurement, audit, chaos run and sweep." (plan body, first paragraph) · quote found
  - [2] "Work in your own worktree from origin/p/integrate-engine-20261111 (032b63b1c or later) under ~/Projects/shards/.claude/worktrees/." (goal 908fa9306cf881078ffa22799868d4b8 (Plan step 5 [Shards]), printed from ROOT's goal stack) · quote found
  - [2] "Light tests only: real-VM suites and measurements are ROOT's." (goal 908fa9306cf881078ffa22799868d4b8, printed from ROOT's goal stack) · quote found
  - [3] "Use scoped subagents for parallel implementation and targeted unit/light" (line 147 (R15)) · quote found
  - [3] "Use online learned/data-derived values, or exceptionally well-researched invariants" (line 42 (R03)) · quote found
  - [3] "completion/readiness notifications and one authoritative state owner." (line 67 (R05)) · quote found
- **A · 97 (high)** Today Hex's guest runs hex itself as PID 1 under D215's direct boot, with no Agentfile: it mounts /proc, /sys, cgroup2, /dev, tmpfs /run and /tmp, starts its node and relay, reaps, and powers the guest off with reboot(2) when either ends. [15][14]
  _2 independent sources (code, a document); 4 of 4 quotes found where cited_
  - [15] "needs (`/proc`, `/sys`, cgroup2, `/dev`, a tmpfs `/run` and `/tmp`; the node's state is under `/run`," (line 11) · quote found
  - [15] "as PID 1 must. When the node or the relay ends, it says why and powers the guest off, so the host sees" (line 18) · quote found
  - [15] "rustix::system::reboot(rustix::system::RebootCommand::PowerOff)" (line 412) · quote found
  - [14] "orchestrator's own PID 1 and no Agentfile." (line 21461 (D231)) · quote found
- **A · 90 (high)** Hex's node puts its boxes' cgroups at /sys/fs/cgroup/hex and enables +cpu +io +memory +pids in the root cgroup.subtree_control of the hierarchy it sees: it expects to own that hierarchy's root. [15]
  _1 independent source (code); 2 of 2 quotes found where cited_
  - [15] "const BOX_ROOT: &str = "/sys/fs/cgroup/hex";" (line 304) · quote found
  - [15] "const CONTROLLERS: &str = "+cpu +io +memory +pids";" (line 307) · quote found
- **A · 90 (high)** shards-init mounts cgroup2 with nsdelegate, puts every workload process in /sys/fs/cgroup/run/workload, and gives the workload a cgroup namespace rooted there whose /sys/fs/cgroup is read-only. [4]
  _1 independent source (code); 3 of 3 quotes found where cited_
  - [4] "mounted with `nsdelegate`, so that a cgroup namespace bounds what" (line 1070) · quote found
  - [4] "const WORKLOAD_CGROUP: &str = "/sys/fs/cgroup/run/workload";" (line 1067) · quote found
  - [4] "is that cgroup, read-only." (line 1118) · quote found
- **A · 98 (high)** With nsdelegate, Linux treats a cgroup namespace as a delegation boundary: from inside, the namespace root's resource files refuse writes (only the delegate files such as cgroup.procs, cgroup.threads and cgroup.subtree_control are writable), nothing in the subtree escapes the parent's limits, and migrations to or from outside fail ENOENT. Delegation is how established runtimes nest (rootless runc asks systemd for it). [21][40]
  _2 independent sources (official docs); 5 of 5 quotes found where cited_
  - [21] "Consider cgroup namespaces as delegation boundaries." (line 188 (nsdelegate)) · quote found
  - [21] "the kernel rejects writes to all files on a namespace root from inside the cgroup namespace, except for those files listed in "/sys/kernel/cgroup/delegate"" (lines 554-556 (Model of Delegation)) · quote found
  - [21] "nothing can escape the resource restrictions imposed by the parent." (lines 564-565) · quote found
  - [21] "If either is not reachable, the migration is rejected with -ENOENT." (lines 610-611 (Delegation Containment)) · quote found
  - [40] "On cgroup v2 hosts, rootless runc can talk to systemd to get cgroup permissions to be delegated." (line 43) · quote found
- **A · 99 (high)** shards already has a writable cgroup subtree narrower than --privileged: `--security-opt writable-cgroups=true` (init's `cgroups-rw` entry) remounts the workload's own cgroup namespace writable, and D115 gives it even beside domains as touching only its own subtree; Docker likewise makes cgroups writable only for privileged containers unless WritableCgroups is set. [5][12][36]
  _2 independent sources (code, a document); 4 of 4 quotes found where cited_
  - [5] "// `--security-opt writable-cgroups=true`: the cgroup hierarchy writable." (line 593) · quote found
  - [12] "`writable-cgroups=true` (its cgroup namespace's subtree)" (line 17094 (D115 table)) · quote found
  - [36] "// if the user didn't specify otherwise, default to the value of privileged" (line 668) · quote found
  - [36] "writableCgroups := c.HostConfig.Privileged" (line 669) · quote found
- **A · 100 (high)** cgroup v2's no-internal-process rule: a non-root cgroup can enable domain controllers for its children only once it holds no process, so a nested runtime must first move its own processes into a leaf; shards-init already records nested runtimes (buildkitd, Docker in Docker) doing so, and the EBUSY they meet otherwise. [21][33][4]
  _3 independent sources (official docs, code); 5 of 5 quotes found where cited_
  - [21] "only domain cgroups which don't contain any processes can have domain controllers enabled in their "cgroup.subtree_control" files." (lines 511-513 (No Internal Process Constraint)) · quote found
  - [21] "the cgroup must create children and transfer all its processes to the children before enabling controllers in its "cgroup.subtree_control" file." (lines 531-534) · quote found
  - [33] "you have to move your main daemon process out of that cgroup (and into a sub-cgroup) before you can start further processes in any of your sub-cgroups." (lines 301-303) · quote found
  - [4] "runtime (BuildKit's buildkitd, Docker in Docker) moves its own processes into a cgroup" (line 1217) · quote found
  - [4] "keeps processes out of its own (EBUSY: no internal processes)." (line 1219) · quote found
- **B · 80 (good)** Run as shards' workload, Hex's node would write its controllers into its cgroup namespace's root, run/workload, which is not the kernel's exempt root and holds the node's own process, so the write fails EBUSY. Starting the designated workload in a leaf below its delegated subtree, with the namespace rooted at the subtree, gives it an empty root to enable controllers on and room to make box cgroups beside the leaf, as Hex expects. [15][21][4]
  _3 independent sources (code, official docs); 3 of 3 quotes found where cited; inferred from them_
  - [15] "const CONTROLLERS: &str = "+cpu +io +memory +pids";" (line 307) · quote found
  - [21] "The root cgroup is exempt from this restriction." (line 520) · quote found
  - [4] "const WORKLOAD_CGROUP: &str = "/sys/fs/cgroup/run/workload";" (line 1067) · quote found
- **A · 100 (high)** Hex's node as root needs CAP_NET_ADMIN as well as CAP_SYS_ADMIN: unshare and setns take CAP_SYS_ADMIN, bringing a box network namespace's loopback up takes CAP_NET_ADMIN, and the cgroup device programs it loads at its boxes' root are a program type the kernel loads only with CAP_NET_ADMIN. [16][24][26]
  _3 independent sources (code, official docs); 5 of 5 quotes found where cited_
  - [16] "(capget(2)): CAP_SYS_ADMIN for `unshare` and `setns`, CAP_NET_ADMIN for loopback's rtnetlink" (line 2520) · quote found
  - [16] "with the default policy's program on its oci boxes' root; a rootless node loads none, its containers" (line 2541) · quote found
  - [24] "case BPF_PROG_TYPE_CGROUP_DEVICE:" (line 2891, in is_net_admin_prog_type (lines 2876-2895)) · quote found
  - [24] "if (is_net_admin_prog_type(type) && !bpf_token_capable(token, CAP_NET_ADMIN))" (line 3023) · quote found
  - [26] "Perform various network-related operations:" (CAP_NET_ADMIN) · quote found
- **A · 90 (high)** Hex's node decides how it runs boxes from its euid: as root its boxes share its user namespace, as docker run's do; as any other user each box gets a user namespace mapping that uid alone to root, as runc's rootless containers do, and it loads no device programs. [17][16]
  _1 independent source (code); 3 of 3 quotes found where cited_
  - [17] "Root: the container shares the host's user namespace, as `docker run`'s does." (line 106) · quote found
  - [17] "alone, to root, as runc's rootless containers do (libcontainer/specconv, `ToRootless`)." (line 109) · quote found
  - [16] "(0, _) => Owner::Root," (line 313) · quote found
- **A · 100 (high)** Reaping needs no grant: Hex's node already makes itself a child subreaper, a subreaper takes init's role for its descendants, and a shards workload is already PID 1 of its own PID namespace, whose end the kernel follows by killing every process left in it. [16][29][12][30]
  _3 independent sources (code, official docs, a document); 4 of 4 quotes found where cited_
  - [16] "this process becomes a child subreaper, so each `oci` box's init is its child once the shim" (line 306) · quote found
  - [29] "A subreaper fulfills the role of init(1) for its descendant processes." (DESCRIPTION) · quote found
  - [12] "exactly itself, PID 1; under `--init`, the reaper (PID 1) and itself (PID 2), and of the" (line 17106 (D115, tested)) · quote found
  - [30] "If the "init" process of a PID namespace terminates, the kernel terminates all of the processes in the namespace via a SIGKILL signal." (The namespace init process) · quote found
- **A · 99 (high)** The end needs no new mechanism: a shards run ends with its main process (init marks WORKLOAD_EXITED and keeps its status), and a restart is a new microVM; Hex's PID 1 instead powers the guest off with reboot(2). [4][12][15]
  _2 independent sources (code, a document); 4 of 4 quotes found where cited_
  - [4] "let _ = crate::linux::control_write(control::MARKER, marker::WORKLOAD_EXITED);" (line 4824) · quote found
  - [4] "// A run ends with its main process; the runs joined" (line 4826) · quote found
  - [12] "agents with it; a restart is a new microVM |" (line 17101 (D115 table)) · quote found
  - [15] "rustix::system::reboot(rustix::system::RebootCommand::PowerOff)" (line 412) · quote found
- **A · 100 (high)** Descriptors: shards-init starts every workload at RLIMIT_NOFILE 1048576 soft and hard (the kernel's default nr_open, as Docker's were measured), above the 271062 Hex's node needed when its guest kernel gave 4096; since Hex 9c75ca76 the node fits its bounds to whatever hard limit it gets, refusing only below the least node. [6][15][19]
  _2 independent sources (code, a measurement); 4 of 4 quotes found where cited_
  - [6] "open files 1048576 (the kernel's nr_open)," (line 148) · quote found
  - [6] "set(libc::RLIMIT_NOFILE, 1_048_576, 1_048_576);" (line 163) · quote found
  - [15] "INR_OPEN_CUR and INR_OPEN_MAX), under which the node refused to start: "its own 271062 descriptors at its pools'" (line 1008) · quote found
  - [19] "a node's start fits every bound that holds descriptors to its hard limit and refuses only below the least node" (the command's output (the commit's subject)) · quote found
- **A · 85 (high)** No process can raise RLIMIT_NOFILE past fs.nr_open, privileged or not, and raising a hard limit takes CAP_SYS_RESOURCE in the initial user namespace: a workload asking for more than nr_open needs init to raise fs.nr_open first, as Hex's init does. [28][27]
  _1 independent source (official docs); 3 of 3 quotes found where cited_
  - [28] "This file imposes a ceiling on the value to which the RLIMIT_NOFILE resource limit can be raised" (/proc/sys/fs/nr_open) · quote found
  - [28] "This ceiling is enforced for both unprivileged and privileged process. The default value in this file is 1048576." (/proc/sys/fs/nr_open) · quote found
  - [27] "A privileged process (under Linux: one with the CAP_SYS_RESOURCE capability in the initial user namespace) may make arbitrary changes to either limit value." (DESCRIPTION) · quote found
- **A · 100 (high)** shards runs a workload under Docker's default seccomp profile, byte-identical to moby/profiles seccomp v0.2.3, which opens mount, umount2, unshare, setns, clone3, bpf, perf_event_open and the new mount API only with CAP_SYS_ADMIN and has no rule for pivot_root, so pivot_root fails EPERM even with the capability; Hex's boxes pivot_root into their roots. [20][35][12][42]
  _3 independent sources (a measurement, code, a document); 5 of 5 quotes found where cited_
  - [20] "SAME_AS_SHARDS" (cmp of the two files) · quote found
  - [20] "17 SCMP_ACT_ALLOW includes ['CAP_SYS_ADMIN'] ['bpf', 'clone', 'clone3', 'fanotify_init', 'fsconfig', 'fsmount', 'fsopen', 'fspick', 'lookup_dcookie', 'lsm_get_self_attr', 'lsm_list_modules', 'lsm_set_self_attr', 'mount', 'mount_setattr', 'm…" (rules naming CAP_SYS_ADMIN) · quote found
  - [35] ""defaultErrnoRet": 1," (line 3) · quote found
  - [12] "The workload runs under Docker's default seccomp profile (moby/profiles seccomp" (line 3291 (D42)) · quote found
  - [42] "rustix::process::pivot_root(c".", c".").map_err(|e| OciError::os("pivot_root", e))?;" (line 1013) · quote found
- **A · 90 (high)** A run given CAP_SYS_ADMIN by --cap-add also gets shards' stand-in for Docker's AppArmor `deny mount,`: a filter refusing mount(2), the new mount API and pivot_root, so it mounts nothing. `--cap-add SYS_ADMIN` alone gives a nested runtime no mounts. [8]
  _1 independent source (code); 2 of 2 quotes found where cited_
  - [8] "docker-default the new mount API and pivot_root (D121). So a run given SYS_ADMIN" (line 239) · quote found
  - [8] "mounts nothing of its microVM's, its shares among them." (line 240) · quote found
- **A · 85 (high)** CAP_SYS_ADMIN is the kernel's overloaded capability: besides mount, umount, pivot_root and namespaces through clone, unshare and setns, it carries CAP_BPF's and CAP_PERFMON's operations. Held in the microVM's initial user namespace, it reaches kernel-wide facilities every process of the microVM shares. [26]
  _1 independent source (official docs); 5 of 5 quotes found where cited_
  - [26] "this capability is overloaded" (CAP_SYS_ADMIN) · quote found
  - [26] "Perform a range of system administration operations" (CAP_SYS_ADMIN (mount, umount, pivot_root and others listed after it)) · quote found
  - [26] "employ CLONE_* flags that create new namespaces with clone(2) and unshare(2)" (CAP_SYS_ADMIN) · quote found
  - [26] "perform the same BPF operations as are governed by CAP_BPF" (CAP_SYS_ADMIN) · quote found
  - [26] "employ the same performance monitoring mechanisms as are governed by CAP_PERFMON" (CAP_SYS_ADMIN) · quote found
- **A · 90 (high)** D115 refuses, at the daemon and again in init, everything that would reach an image's agents and harnesses from its run's command (--privileged, CAP_SYS_ADMIN and the other crossing capabilities, NET_ADMIN where their flows cross, systempaths=unconfined), holds a joining run to the same rules, and refuses none of it where the image declares no domain. [12][9]
  _1 independent source (a document, code); 4 of 4 quotes found where cited_
  - [12] "### D115. A run's command reaches none of its image's agents and harnesses" (line 16970) · quote found
  - [12] "| A run joining the microVM (`--network container:`, D119) | held to every rule of this table, by the daemon and again by init | it runs beside them as the microVM's own run does |" (line 17089) · quote found
  - [9] "there. An image that declares no domain is refused none of it." (line 341) · quote found
  - [9] "if self.domains == 0 {" (line 64 (Agentfile::refuse)) · quote found
- **A · 100 (high)** shards-init attaches the workload's device filter with BPF_F_ALLOW_MULTI, under which a sub-cgroup's own program runs in addition to it (attaching with no flag would forbid any below), as runc's cgroups library attaches its own. So a nested runtime may put device programs on its boxes, and shards' filter, render node rules included, still bounds them. [7][23][39]
  _3 independent sources (code); 5 of 5 quotes found where cited_
  - [7] "const BPF_F_ALLOW_MULTI: u32 = 2;" (line 1273) · quote found
  - [7] "attach_flags: BPF_F_ALLOW_MULTI | replacing.map_or(0, |_| BPF_F_REPLACE)," (line 1320) · quote found
  - [23] "NONE(default): No further bpf programs allowed in the subtree." (line 1245) · quote found
  - [23] "that cgroup program gets run in addition to the program in this cgroup." (line 1251) · quote found
  - [39] "attachFlags := uint32(unix.BPF_F_ALLOW_MULTI)" (line 374) · quote found
- **A · 96 (high)** A run's GPU is its render node: `--gpus` gives the guest's virtio-gpu node /dev/dri/renderD128 at udev's 0666, in a GPU microVM restored from templates of its own. A render node serves rendering alone, with no modesetting or privileged ioctl, and file modes govern access to it. [12][22]
  _2 independent sources (a document, official docs); 4 of 4 quotes found where cited_
  - [12] "`/dev/dri/renderD128`, the node Linux's virtio-gpu driver makes for a guest's first" (line 14459 (D145)) · quote found
  - [12] "- **The render node's mode is a host's:** udev's 0666 (systemd v257" (line 14547) · quote found
  - [22] "Render nodes solely serve render clients, that is, no modesetting or privileged ioctls can be issued on render nodes." (lines 147-148) · quote found
  - [22] "With render nodes, user-space can now control access to the render node via basic file-system access-modes." (lines 167-168) · quote found
- **A · 99 (high)** Hex trusts GPU authority only from host-created kernel words, `hex.gpu.box=<owner> hex.gpu=<keys>` on /proc/cmdline, which a box's payload, environment or CDI label cannot assert, and writes its boxes' CDI specs from them. Under shards-init the command line is shards', so shards-init must hand the workload those words itself, somewhere its boxes cannot write. [18][34]
  _2 independent sources (code, a standard); 4 of 4 quotes found where cited_
  - [18] "The dedicated VM's boot authority and the guest GPU catalog it grants. Only host-created" (line 1) · quote found
  - [18] "kernel parameters grant this mapping; a box payload, environment or CDI label" (line 2) · quote found
  - [18] "write!(text, " hex.gpu.box={owner} hex.gpu=")" (line 88) · quote found
  - [34] "The device driver installation software writes a JSON file at a well known path (`/etc/cdi/vendor.json`)." (line 60) · quote found
- **B · 70 (good)** A restored microVM's monotonic clock goes on from its snapshot (kvmclock from its saved value); only the wall clock is set from the host's time after each restore. [12]
  _1 independent source (a document); 2 of 2 quotes found where cited_
  - [12] "kvmclock goes on from its saved value;" (line 169) · quote found
  - [12] "the VMM answers with its clock at that moment. The monotonic clock stays continuous," (line 385) · quote found
- **A · 90 (high)** Hex's boot deadline today is a CLOCK_BOOTTIME instant: the host's readiness bound counted from the kernel's start (hex.ready-ms), handed to the node and relay as HEX_BOOT_READY_BY. [15]
  _1 independent source (code); 2 of 2 quotes found where cited_
  - [15] "The variable that carries the boot's deadline from init to each child it starts before the guest is ready, the" (line 232) · quote found
  - [15] "host waits for it to be ready, its readiness bound ([`boot_budget`]) counted from the kernel's start, so the time" (line 234) · quote found
- **B · 78 (good)** A deadline counted from the guest kernel's start is wrong for a run whose microVM was restored from a template or waited in a pool. So shards-init must give the workload its ready-by on the guest's own clock, from when the run reached it (the caller's remaining budget, or its absolute host time through the control page's host clock), and none where the caller gives none. [12][15]
  _2 independent sources (a document, code); 2 of 2 quotes found where cited; inferred from them_
  - [12] "kvmclock goes on from its saved value;" (line 169) · quote found
  - [15] "host waits for it to be ready, its readiness bound ([`boot_budget`]) counted from the kernel's start, so the time" (line 234) · quote found
- **A · 100 (high)** shards' own readiness is its boot markers on the control page, none of which says a workload is ready to serve, while Hex's init reads its node's `ready` line from stdout. sd_notify's READY=1, which Podman proxies into containers, is the established readiness notification, and R05 prefers notifications to probes. [10][15][32][41][3]
  _4 independent sources (code, official docs, a document); 6 of 6 quotes found where cited_
  - [10] "/// Boot-phase markers written to the control page." (line 146) · quote found
  - [10] "pub const WORKLOAD_STARTED: u32 = 4;" (line 155) · quote found
  - [15] "waits for its first line, `ready`" (line 841) · quote found
  - [32] "Tells the service manager that service startup is finished" (READY=1) · quote found
  - [41] "Default is **container**, which means allow the OCI runtime to proxy the socket into the" (line 9) · quote found
  - [3] "completion/readiness notifications and one authoritative state owner." (line 67 (R05)) · quote found
- **A · 100 (high)** A workload under Docker's default filter cannot open an AF_VSOCK socket at all, while Hex's relay listens on vsock ports; the channel item is step 11's token-gated agent-control endpoint, not a grant of vsock. [8][15][1]
  _3 independent sources (code, kiln's records); 3 of 3 quotes found where cited_
  - [8] "nothing; and its `socket` and `socketpair` rules, which refuse AF_ALG and AF_VSOCK alone," (line 326) · quote found
  - [15] "With `--vsock` it listens on the guest's vsock ports" (line 21) · quote found
  - [1] "Channel: the relay's vsock ports become the token-gated agent-control endpoint (step 11)." (step 38, item 5) · quote found
- **B · 80 (good)** pivot_root(2) refuses the initramfs, which is why Hex's PID 1 first moves itself onto a tmpfs. shards-init moves its overlay root over the initramfs before any workload starts, so the designated workload needs no such step, and its boxes' pivot_root should not meet that refusal (to be shown live). [31][15][4]
  _3 independent sources (official docs, code); 4 of 4 quotes found where cited; inferred from them_
  - [31] "The rootfs (initial ramfs) cannot be" (NOTES (followed by pivot_root()ed)) · quote found
  - [31] "The current root directory is not a mount point (because of" (ERRORS, EINVAL) · quote found
  - [15] "since a container's start pivots its root and pivot_root(2) refuses to leave the" (line 9) · quote found
  - [4] "// The initramfs cannot be unmounted, so the new root moves over it" (line 510) · quote found
- **C · 56 (moderate)** Egress past a microVM is held by its network process, a host process separate from the VM's. So CAP_NET_ADMIN given inside the guest configures only the guest's own interfaces and namespaces, and default deny past the microVM holds whatever the workload does inside it. [12][43]
  _1 independent source (a document); 2 of 2 quotes found where cited; inferred from them_
  - [12] "- **Held by the member's network process**, as a remote MCP server's grant is (D59 part" (line 10139 (D192)) · quote found
  - [43] "The VM process and the network process are separate *processes*" (line 19) · quote found
- **A · 90 (high)** Every Agentfile grant is checked twice: the build refuses what it must; the daemon re-reads the normalized /.agentfile.json in the image's root against the image's digest and reruns the build's checks; init reads it in the image as built; and grants come from directives, never labels. [12][11]
  _1 independent source (a document, code); 3 of 3 quotes found where cited_
  - [12] "**Every check twice, at build and at run.** An image can be crafted past any build, and a" (line 17790 (D109)) · quote found
  - [12] "normalized Agentfile, `/.agentfile.json`, in the image's root as its microVM mounts it: the" (line 17798 (D109)) · quote found
  - [11] "What an Agentfile grants past its microVM (D59): the build's labels of it, and what the" (line 2236) · quote found
- **A · 90 (high)** No Agentfile directive today grants anything to the run's own command: they are AGENT, HARNESS, SKILL, MCP, NETWORK, CONNECT, ATTACH, EXPOSE and VOLUME. [11]
  _1 independent source (code); 2 of 2 quotes found where cited_
  - [11] "pub enum Directive {" (line 240) · quote found
  - [11] "Volume(Volume)," (line 249) · quote found
- **A · 99 (high)** Established nesting and privilege grants take both the workload's declaration and the operator's consent. BuildKit runs `RUN --security=insecure` only with the security.insecure entitlement allowed on the daemon and on the build request; shards' builds give it only under --allow; and Incus nests only where security.nesting is set on the instance and its project allows it (blocked by default). [37][13][38]
  _3 independent sources (official docs, a document); 5 of 5 quotes found where cited_
  - [37] "In order to access this feature, entitlement `security.insecure` should be" (line 1136) · quote found
  - [37] "and for a build request with [`--allow security.insecure` flag]" (line 1139) · quote found
  - [13] "`host` and `--security=insecure` only under `--allow`" (line 1002) · quote found
  - [38] "For containers, this controls whether Incus (nested) can be run inside of the instance." (security.nesting) · quote found
  - [38] "When set to `allow`, {config:option}`instance-security:security.nesting` can be set to `true` for an instance." (restricted.containers.nesting) · quote found
- **B · 80 (good)** A workload in a user namespace of its own (the rootless alternative) could not mount a fresh /proc for its boxes under shards' masked /proc: the kernel refuses a new proc mount from a non-initial user namespace unless its mount namespace holds a fully visible proc, and shards masks the workload's /proc as runc does. Docker likewise refuses writable cgroups together with user namespaces. [25][4][36]
  _3 independent sources (code); 4 of 4 quotes found where cited; inferred from them_
  - [25] "/* This mount is not fully visible if there are any" (line 6627 (mnt_already_visible)) · quote found
  - [25] "if (ns->user_ns == &init_user_ns)" (line 6655 (mount_too_revealing: the initial user namespace is never refused)) · quote found
  - [4] "masked and read-only where init's is (`masked`), as runc mounts a container's (D42)." (line 1257) · quote found
  - [36] "option WritableCgroups conflicts with user namespaces and rootless mode" (line 673) · quote found
- **B · 70 (good)** Hex will boot Agentfile images through D231's `shards api-run`, which prepares each run with the daemon's own functions and whose run's own command is the orchestrator's. D231 is decided and being built on p/runtime-image-boot-v1, not yet in the integration branch, so a run-side check placed in those shared functions reaches Hex's path once D231 merges (parent plan step 2). [14]
  _1 independent source (a document); 3 of 3 quotes found where cited_
  - [14] "### D231. An image booted for an orchestrator: a run of its own, hosted by one `shards` process (decided; building)" (line 21454) · quote found
  - [14] "The run host prepares the run with the daemon's own" (line 21467) · quote found
  - [14] "- *The run's own command* is the orchestrator's, or `builtin::ATTEND`: the standby holds," (line 21495) · quote found
- **A · 90 (high)** A nested runtime moves its own processes into a leaf named init before enabling controllers, as Docker in Docker's own entrypoint does, because writing subtree_control otherwise fails EBUSY; it loops because docker exec's new processes race the move. [44]
  _1 independent source (code); 3 of 3 quotes found where cited_
  - [44] "otherwise writing subtree_control fails with EBUSY." (line 64) · quote found
  - [44] "# move the processes from the root group to the /init group," (line 63) · quote found
  - [44] "# this happens in a loop because things like "docker exec" on our dind" (line 67) · quote found
- **A · 90 (high)** systemd reads sd_notify datagrams into a buffer of PIPE_BUF (NOTIFY_BUFFER_MAX), ignores a truncated or invalid one as if none came, accepts with NotifyAccess=all only members of the service's control group, and ignores assignments it does not know. [45][46][47][32]
  _1 independent source (code, official docs); 4 of 4 quotes found where cited_
  - [45] "#define NOTIFY_BUFFER_MAX PIPE_BUF" (line 35) · quote found
  - [46] "the logic that an invalid message shall be ignored, and treated like no message at all" (line 119) · quote found
  - [47] "all services updates from all members of the service's control group are accepted" (NotifyAccess=) · quote found
  - [32] "Unknown assignments are ignored." (Well-known assignments) · quote found
- **A · 90 (high)** The kernel's fs.nr_open defaults to 1024*1024 and is capped by its own ceiling, sysctl_nr_open_max: the least of INT_MAX and SIZE_MAX divided by a pointer's size, rounded down to a multiple of BITS_PER_LONG. [48]
  _1 independent source (code); 2 of 2 quotes found where cited_
  - [48] "unsigned int sysctl_nr_open __read_mostly = 1024*1024;" (line 96) · quote found
  - [48] "__const_min(INT_MAX, ~(size_t)0/sizeof(void *)) & -BITS_PER_LONG;" (line 101) · quote found
- **A · 90 (high)** The control page's HOST_TIME is the host's wall-clock time when read, in nanoseconds since the Unix epoch, in one 64-bit access; shards-init sets the guest's CLOCK_REALTIME from it, a restored guest's clock otherwise being its snapshot's. [10]
  _1 independent source (code); 2 of 2 quotes found where cited_
  - [10] "/// Read, 64 bits in one access: the host's wall-clock time when read, in nanoseconds" (line 101) · quote found
  - [10] "a restored guest's is its snapshot's; shards-init sets CLOCK_REALTIME from this." (line 103) · quote found

**Sources**

1. Shared plan, step 38: shards-init hosts a nested runtime (the privileges Hex's node needs) (kiln's records, internal) — plan:01a1cd8e862e7169a074db3d5fa33f3d · 2026-11-11
2. Shards ROOT's goal for plan step 5 (the rules each Shards step follows), as read from ROOT's goal stack in this agent's conversation (kiln's records, internal) — agent:01a1cda53b9474ab8a9721b7420f0054 · 2026-11-11
3. Binding development rules R01-R16 (sha256 df2acf47..., origin/main bff30151) (a document, internal) — docs/development-rules.md
4. shards-init run.rs at 032b63b1c: cgroup hierarchy, workload isolation, root switch, workload end (code, internal) — crates/init/src/run.rs
5. shards-init setup.rs at 032b63b1c: a run's setup entries (cgroups-rw, privileged, ulimit, tmpfs) (code, internal) — crates/init/src/setup.rs
6. shards-init defaults.rs at 032b63b1c: the resource limits a workload starts with (code, internal) — crates/init/src/defaults.rs
7. shards-init devices.rs at 032b63b1c: the device filter (D44) and its attach flags (code, internal) — crates/init/src/devices.rs
8. shards daemon setup.rs at 032b63b1c: seccomp profiles and the mount filter for SYS_ADMIN (D121) (code, internal) — crates/shards/src/setup.rs
9. shards daemon agentfile.rs at 032b63b1c: the run held to an image's Agentfile, D115's refusals (code, internal) — crates/shards/src/agentfile.rs
10. shards abi lib.rs at 032b63b1c: boot-phase markers on the control page (code, internal) — crates/abi/src/lib.rs
11. shards agentfile crate at 032b63b1c: directives and grants (code, internal) — crates/agentfile/src/agentfile.rs
12. shards architecture.md at 032b63b1c (D42, D109, D115, D144-D146, D192, D215, restore) (a document, internal) — docs/design/architecture.md
13. AGENTFILE_ARCH.md at 032b63b1c: build privileges only under --allow (a document, internal) — docs/architecture/AGENTFILE_ARCH.md
14. D231 (decided; building) on p/runtime-image-boot-v1 75c36864f: an image booted for an orchestrator (a document, internal) — p/runtime-image-boot-v1 (75c36864f): docs/design/architecture.md
15. Hex main 24c100f8: crates/hex-cli/src/guest.rs, what hex does as a Shards guest's PID 1 (code, internal) — Hex main (24c100f8): crates/hex-cli/src/guest.rs
16. Hex main 24c100f8: crates/hex-node/src/node.rs, the node's owner, capabilities, device programs, subreaper (code, internal) — Hex main (24c100f8): crates/hex-node/src/node.rs
17. Hex main 24c100f8: crates/hex-core/src/bundle.rs, root and rootless owners (code, internal) — Hex main (24c100f8): crates/hex-core/src/bundle.rs
18. Hex main 24c100f8: crates/hex-runtime/src/guest/gpu.rs, the guest GPU boot authority (code, internal) — Hex main (24c100f8): crates/hex-runtime/src/guest/gpu.rs
19. Hex commit 9c75ca76's subject: a node's start fits every descriptor bound to its hard limit (a measurement, internal) — git -C <Hex checkout> log -1 --format=%s 9c75ca7645a61703eb22b635276b31feb47736b3 · 2026-11-11
20. shards' vendored seccomp default against moby/profiles seccomp v0.2.3, and its CAP_SYS_ADMIN rules (a measurement, internal) — curl -o default.json https://raw.githubusercontent.com/moby/profiles/seccomp/v0.2.3/seccomp/default.json; cmp default.json <(git show 032b63b1c:crates/seccomp/data/default.json); python3 listing each rule whose includes.caps names CAP_SYS_ADMIN; grep -c '"pivot_root"' default.json · 2026-11-11
21. Linux kernel: Documentation/admin-guide/cgroup-v2.rst (official docs, external) — https://raw.githubusercontent.com/torvalds/linux/79eb6679ef6a0d5ea68da53a8321fb1dc72299b9/Documentation/admin-guide/cgroup-v2.rst
22. Linux kernel: Documentation/gpu/drm-uapi.rst (Render nodes) (official docs, external) — https://raw.githubusercontent.com/torvalds/linux/79eb6679ef6a0d5ea68da53a8321fb1dc72299b9/Documentation/gpu/drm-uapi.rst
23. Linux kernel: include/uapi/linux/bpf.h (cgroup-bpf attach flags) (code, internal) — https://raw.githubusercontent.com/torvalds/linux/79eb6679ef6a0d5ea68da53a8321fb1dc72299b9/include/uapi/linux/bpf.h
24. Linux kernel: kernel/bpf/syscall.c (program load capability checks) (code, internal) — https://raw.githubusercontent.com/torvalds/linux/79eb6679ef6a0d5ea68da53a8321fb1dc72299b9/kernel/bpf/syscall.c
25. Linux kernel: fs/namespace.c (mount_too_revealing, mnt_already_visible) (code, internal) — https://raw.githubusercontent.com/torvalds/linux/79eb6679ef6a0d5ea68da53a8321fb1dc72299b9/fs/namespace.c
26. capabilities(7), Linux man-pages (official docs, external) — https://man7.org/linux/man-pages/man7/capabilities.7.html
27. getrlimit(2), Linux man-pages (official docs, external) — https://man7.org/linux/man-pages/man2/getrlimit.2.html
28. proc_sys_fs(5), Linux man-pages (official docs, external) — https://man7.org/linux/man-pages/man5/proc_sys_fs.5.html
29. PR_SET_CHILD_SUBREAPER(2const), Linux man-pages (official docs, external) — https://man7.org/linux/man-pages/man2/PR_SET_CHILD_SUBREAPER.2const.html
30. pid_namespaces(7), Linux man-pages (official docs, external) — https://man7.org/linux/man-pages/man7/pid_namespaces.7.html
31. pivot_root(2), Linux man-pages (official docs, external) — https://man7.org/linux/man-pages/man2/pivot_root.2.html
32. sd_notify(3), systemd (official docs, external) — https://man7.org/linux/man-pages/man3/sd_notify.3.html
33. systemd: docs/CGROUP_DELEGATION.md (official docs, external) — https://raw.githubusercontent.com/systemd/systemd/b4259f378b0cf2f7836529e549efe7c165b28e96/docs/CGROUP_DELEGATION.md
34. Container Device Interface specification (SPEC.md) (a standard, external) — https://raw.githubusercontent.com/cncf-tags/container-device-interface/9e21b8bbcac8e46ec0d9500eafb23f633f71490f/SPEC.md
35. moby/profiles seccomp v0.2.3 default.json (Docker's default seccomp profile) (code, internal) — https://raw.githubusercontent.com/moby/profiles/seccomp/v0.2.3/seccomp/default.json
36. moby daemon/oci_linux.go (writable cgroups default to privileged) (code, internal) — https://raw.githubusercontent.com/moby/moby/1e9198df7debc4af8c1e7c1c2e668d33c6fc5170/daemon/oci_linux.go
37. BuildKit Dockerfile reference: RUN --security=insecure and the security.insecure entitlement (official docs, external) — https://raw.githubusercontent.com/moby/buildkit/8cd4dbe133ab23a3d9b9ea749012f38d5db4604d/frontend/dockerfile/docs/reference.md
38. Incus configuration options: security.nesting and restricted.containers.nesting (official docs, external) — https://raw.githubusercontent.com/lxc/incus/3dcffe4bcd0755b41b424434bdf95d684d0681a2/doc/config_options.txt
39. opencontainers/cgroups devices/ebpf_linux.go (runc's device filter attach flags) (code, internal) — https://raw.githubusercontent.com/opencontainers/cgroups/b62147e9bdc67f63540ce198bc3e071ab89de6f7/devices/ebpf_linux.go
40. runc docs/cgroup-v2.md (rootless delegation) (official docs, external) — https://raw.githubusercontent.com/opencontainers/runc/c0512b4ff3fddab104cad9fadf3fdcc8d8b9a193/docs/cgroup-v2.md
41. Podman --sdnotify option (NOTIFY_SOCKET proxied into a container) (official docs, external) — https://raw.githubusercontent.com/containers/podman/326546529da0f02947212532e21b2e0e64222d85/docs/source/markdown/options/sdnotify.md
42. Hex main 24c100f8: crates/hex-runtime/src/linux/oci/init.rs, a box's root switch (code, internal) — Hex main (24c100f8): crates/hex-runtime/src/linux/oci/init.rs
43. shards CLAUDE.md at 032b63b1c: one binary, separate VM and network processes (a document, internal) — CLAUDE.md
44. moby hack/dind: Docker in Docker's own entrypoint (cgroup v2 nesting) (code, internal) — https://raw.githubusercontent.com/moby/moby/1e9198df7debc4af8c1e7c1c2e668d33c6fc5170/hack/dind
45. systemd src/basic/constants.h (NOTIFY_BUFFER_MAX) (code, internal) — https://raw.githubusercontent.com/systemd/systemd/b4259f378b0cf2f7836529e549efe7c165b28e96/src/basic/constants.h
46. systemd src/shared/notify-recv.c (receiving sd_notify messages) (code, internal) — https://raw.githubusercontent.com/systemd/systemd/b4259f378b0cf2f7836529e549efe7c165b28e96/src/shared/notify-recv.c
47. systemd.service(5): NotifyAccess= (official docs, external) — https://man7.org/linux/man-pages/man5/systemd.service.5.html
48. Linux kernel: fs/file.c (nr_open's default and ceiling) (code, internal) — https://raw.githubusercontent.com/torvalds/linux/79eb6679ef6a0d5ea68da53a8321fb1dc72299b9/fs/file.c
