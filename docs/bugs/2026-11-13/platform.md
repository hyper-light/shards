# Platform adversarial review

POLICY_READ — HEAD and origin/main: `2bc5f78f9e48d09ddbf4e848f9960133f6d19148`.
Role: assigned **adversarial reviewer**, area **platform**. Worktree: `/Users/adalundhe/Projects/shards/.claude/worktrees/rv-platform`; host: Darwin arm64.

All read in full before review/delegation:

| Binding file | SHA-256 |
|---|---|
| AGENTS.md | f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41 |
| docs/development-rules.md | df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3 |
| docs/development-evidence.md | 61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63 |
| CLAUDE.md | 588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010 |

`/tmp/g121-shot/review/brief.md` was read first and in full. Reviewed the scoped changes in `79caf2d74..2bc5f78f9` and current code. Three scoped read-only subreviewers returned matching policy receipts; no further delegation occurred. The user requires P0–P5 ranking and subsequently requested this report under `docs/bugs/2026-11-13/`; they will notify Shards themselves. Here P0 means critical, P1 high, P2 medium; no findings are assigned P3–P5.

**Result: 12 findings: 4 P0, 2 P1, 6 P2.** These are source-verified findings with concrete proposed reproductions. **No live reproduction, builds, tests, daemons, VMs, profiling, benchmarks, or policy-check script were run**, as the binding brief forbids them. No code was changed. This Markdown report is the only repository artifact added, as explicitly requested by the user after the read-only review. Runtime effects described below remain owed coordinator validation, not reported passing experiments. The exact Windows emulator instruction routes and timing impact especially need real-host confirmation. An attempted official documentation web read failed authentication; the report relies on source and repository research, with primary-source links supplied for the coordinator.

## P0 — critical

### platform-01 — Landlock leaves existing VM threads unrestricted and fails to revoke sibling snapshot access

**Location:** `crates/shards/src/confine.rs:726`; `crates/shards/src/vm_run.rs:1990`, `:2788`.

**Defect:** Landlock is applied to the calling thread only, after long-lived threads already exist, and snapshot sealing repeats that mistake.

**Scenario and proof:** Warm boot calls `serve_workload` (`vm_run.rs:1893`), which spawns `exec-hellos` at lines 1990–1992 before calling `start(vsock)` at 1996. That reaches `start_reported`, `confine` at 2534, and Landlock at 3318. The call is:

```rust
libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0u32)
```

Flags zero restrict only the caller and future descendants. The acceptor remains alive in `workload/hellos.rs:375–391`. Given a compromise of a confined VMM thread (the defense-in-depth threat D30 addresses), allowed `rt_sigaction` (`confine.rs:87`) can install a handler; allowed `/proc/self` access (`:702`) reveals the acceptor TID; allowed `tgkill(getpid(), tid, SIGUSR1)` (`:238`) runs that handler on the unrestricted sibling. Allowed `openat` there can read/write same-user files outside all grants. This does not require making a forbidden process or executing a new binary.

Separately, `seal()` applies another thread-only Landlock layer at `vm_run.rs:2788`. Its probe at 2789–2793 tests that caller only. Already-running vCPU/device threads retain their earlier template-directory write access after the advertised revocation, so compromised workers can alter later restores. Seccomp TSYNC does not synchronize Landlock.

**Evidence/contract:** D30; R04 explicit authority/revocation; [Landlock thread synchronization documentation](https://docs.kernel.org/userspace-api/landlock.html), which distinguishes calling-thread enforcement from ABI8 TSYNC. Support starts at ABI5, so simply requesting TSYNC everywhere is insufficient.

**Confidence:** very high for the authority mismatch; signal-handler forbidden effect and sibling template write are unrun.

**Fix direction:** establish initial restrictions before any worker starts; make snapshot revocation genuinely cover all retained threads, failing closed on older ABIs or redesigning ownership so revocation does not depend on unsupported synchronization.

**Catching test:** spawn a held sibling before initial confinement; install a handler and signal that sibling to `openat` a disposable forbidden sentinel. Require refusal. After snapshot sealing, repeat a template-write attempt from each existing worker, not just the sealing thread.

### platform-02 — Linux pathname Unix sockets reach unscoped host authority

**Location:** `crates/shards/src/confine.rs:606`, supporting `:107`, `:255`; `crates/shards/src/daemon.rs:2393`; `crates/shards/src/daemon/engine.rs:502`.

**Defect:** The confined VM may connect to arbitrary pathname Unix sockets, including its user's unscoped host Engine API.

**Scenario and proof:** A compromised VM creates `socket(AF_UNIX, SOCK_STREAM, 0)` and connects to a known disposable daemon's `SHARDS_HOME/engine.sock`. Seccomp permits AF_UNIX and unrestricted `connect`/I/O. Landlock's handled FS mask ends at `IOCTL_DEV`; it never enables pathname Unix resolution restrictions. Its optional v6 scope covers abstract sockets only. Opening the socket file is unnecessary. The daemon accepts the connection with:

```rust
Ok(conn) => self.take_engine(threads, conn, None, None)
```

Its peer check allows `Ok(uid) if uid == ours` (`daemon/engine.rs:504`); the VM retains that UID. Requests therefore receive host Engine authority instead of the deliberately scoped in-guest channel. Other same-user host sockets are also exposed. A Linux distro's extra MAC policy may restrict some hosts, but Shards itself does not.

**Evidence/contract:** default deny and D30. `docs/research/rootless-security.md:42` identifies pathname Unix controls at ABI9; `docs/design/architecture.md:2223–2225` already leaves them unfinished. [Landlock documentation](https://docs.kernel.org/userspace-api/landlock.html).

**Provenance:** underlying socket gap predates base; survives at head and interacts with the current daemon authority.

**Confidence:** very high from policy/acceptance paths; live connection and Engine action unrun.

**Fix direction:** mediate Linux host dialing as on macOS and deny arbitrary VM connects; use pathname restrictions where supported with an equally closed older-kernel path.

**Catching test:** a same-UID Unix listener outside every grant must reject a confined connection. Repeat against a disposable daemon and require an unscoped Engine request to fail.

### platform-03 — Windows snapshot staging leaves the grants broker open after guest start

**Location:** `crates/shards/src/vm_run.rs:3336`; `crates/shards/src/grant_answer.rs:542`, `:610`, `:434`.

**Defect:** A VM started with `--snapshot-dir` and no `--vsock` never seals startup grants, so a compromised running VM can obtain arbitrary host file handles.

**Scenario and proof:** `start_reported` includes the snapshot directory in `paths.write_under`, calls `confine` at 2534, then starts the guest at 2536. Windows confinement runs:

```rust
let stages = !paths.write_under.is_empty();
// ...
match &paths.vsock {
    Some(vsock) => dial_through_spawner(vsock),
    None if stages => Ok(()),
    None => /* send SEALED */
}
```

The staging branch keeps the link open without sending `SEALED`. The broker's `started` starts false and changes only on SEALED or a Listen grant; STAGE and COMMIT do not change it. After guest start, malicious VM code can send a valid GRANT request for `Access::Write` to an existing same-user sentinel outside its grants. `handed()` opens the supplied absolute path read/write (`grant_answer.rs:434–443`) and the broker duplicates it into the VM at 616–623. There is no parent-owned allowlist. Read access similarly exposes host secrets. Snapshot COMMIT revokes a staging directory but still does not seal arbitrary grants.

**Evidence/contract:** D30/D141 AppContainer confinement; D196 snapshot staging. Independent second source review confirmed the whole path.

**Confidence:** very high; Windows live transfer unrun.

**Fix direction:** seal initial grants regardless of whether the retained link serves snapshots. STAGE/RANGES/COMMIT are already handled before the broker's started check, so sealing need not remove staging.

**Catching test:** start a disposable snapshot-capable, no-vsock confined actor; after initial setup send GRANT Write for an ungranted sentinel. Require refusal and no change, while a valid STAGE/COMMIT still succeeds.

### platform-04 — Unfiltered prlimit64 changes other host processes' hard limits

**Location:** `crates/shards/src/confine.rs:86`.

**Defect:** The VM's unrestricted `prlimit64` rule permits irreversible resource-limit changes to matching-credential host processes outside the VM.

**Scenario and proof:** The syscall is in the `any` list, unlike PID-filtered `tgkill` and `pidfd_open`. A compromised VM can perform:

```c
struct rlimit64 zero = {0, 0};
syscall(SYS_prlimit64, getppid(), RLIMIT_NOFILE, &zero, NULL);
```

The same-user spawner meets Linux's UID/GID permission check; its hard descriptor limit becomes zero and later opens/pipes/dups fail with EMFILE. Known same-credential host processes are also targets. Cgroups, no_new_privs and Landlock do not prevent this operation; Landlock has no task_prlimit hook.

**Evidence:** [prlimit permissions and hard limits](https://man7.org/linux/man-pages/man2/getrlimit.2.html); [kernel permission checks](https://github.com/torvalds/linux/blob/master/kernel/sys.c); [Landlock task hooks](https://github.com/torvalds/linux/blob/master/security/landlock/task.c).

**Provenance:** predates base; remains at head. **Confidence:** very high; no sacrificial target was run.

**Fix direction:** remove the unconditional rule; permit PID zero/self only, with only required operations.

**Catching test:** use a disposable same-UID target with communication FDs already open. The confined actor attempts the cross-process reduction; target must retain its limits and successfully open a new file. Never target a real daemon during validation.

## P1 — high

### platform-05 — WHP's emulator retains pointers into partially unmapped memory

**Location:** `crates/vmm/src/hv/whp/mod.rs:643`, `:605`, `:755`, `:1385`.

**Defect:** The x64 WHP emulator's mapping registry neither cuts partial unmaps nor replaces overlapping maps, leaving invalid host pointers reachable by guest emulation.

**Scenario and proof:** Plug multiple virtio-mem blocks together, then unplug one subrange. `memory.rs:1124–1146` accepts a whole-block subrange and decommits its host memory after stage2 unmap. WHP removes a registry entry only on exact address AND length match:

```rust
lock(&self.1).retain(|m| m.gpa != gpa || m.len != len);
```

The original larger mapping survives. `host()` still returns that address; emulator `memory()` directly read/write_volatile dereferences it. A guest string instruction touching the unplugged GPA can enter Windows' fallback emulator (`whp/mod.rs:1714–1720`) and fault the VM process on decommitted memory. Map also appends records without cutting overlap, so replaced ranges can resolve to older backing. The separate dirty-tracking registry correctly calls `cut`, highlighting the missing emulator update.

**Provenance:** predates base; survives at head. **Confidence:** very high for stale registry pointers; actual guest instruction/AV unrun.

**Fix direction:** maintain disjoint mapping intervals on map/unmap and protect host pointer use against concurrent revocation/decommit.

**Catching test:** map 64 pages; unmap pages 16–31; require `host(16*PAGE, 1).is_none()` while neighbors remain valid. Replace an overlap with different host backing and require the new address. Then validate a guest REP MOVSB across unplugged memory on WHP.

### platform-06 — WHP's emulator ignores guest read-only memory permissions

**Location:** `crates/vmm/src/hv/whp/mod.rs:1385`, `:1391`, `:1190`.

**Defect:** The fallback emulator writes registered memory without retaining or checking the guest mapping's write permission.

**Scenario and proof:** `Mapped` stores only gpa/host/len (`:470`). `map_device_memory(..., false)` records read-only pmem through `Partition::map` (`:1203`); `vm/x86_64.rs:484` uses this for readonly regions. On a write callback, `memory()` looks up that host pointer then executes `at.add(i).write_volatile(*b)` with no permission check. Readonly pmem's host section is PAGE_READONLY (`platform/windows.rs:350–353`), so this callback attempts an invalid host write. Readonly zero-backed regions with writable host storage instead permit mutation of bytes denied to ordinary guest writes. A guest REP STOSB into readonly pmem is the proposed end-to-end trigger; Windows-emulator translation/callback behavior must be checked live, not assumed tested.

**Evidence/contract:** `map_device_memory` explicitly promises writable only when requested. Head's `map_unrecorded` excludes DAX for this very host-pointer hazard but leaves readonly pmem recorded.

**Provenance:** predates base. **Confidence:** very high for missing permission enforcement in callback; high for proposed end-to-end trigger, unrun.

**Fix direction:** retain and enforce mapping permissions before host accesses, or exclude readonly device regions from the emulator's RAM registry; preserve typed guest refusal instead of a host protection fault.

**Catching test:** invoke the callback with a readonly registered mapping and a write access; require no host write. Boot a WHP guest executing REP STOSB against readonly pmem and require unchanged bytes and a controlled guest outcome.

## P2 — medium

### platform-07 — Windows spawns leave unrelated handles inheritable

**Location:** `crates/ipc/src/windows.rs:2636`, `:2611`, `:2123`; `crates/registry/src/credentials.rs:624`.

**Defect:** Confined spawning permanently makes borrowed stdio handles inheritable, and temporary inheritable handles can leak into concurrent ordinary Command children.

**Scenario and proof:** `create()` calls `SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT)` on the caller's original stdio and never clears/restores the flag. `pipe()` similarly makes the bootstrap grants reader inheritable; `spawn_env` creates inheritable stdio copies. A handle list constrains only that CreateProcess call. A concurrent credential helper launched with ordinary `std::process::Command` can inherit those handles; original borrowed stdio can remain exposed after the custom spawn returns. An inherited reader can also keep a channel alive past its intended owner.

Local Rust 1.94.1 source `std/src/sys/process/windows.rs:330–342` explicitly describes all inheritable handles being inherited and its private CREATE_PROCESS_LOCK. Raw custom spawns do not share that lock. **The repository's exact pinned Rust 1.98.1 implementation was not available and this cross-launch consequence needs validation there.** The permanent mutation of supplied handle flags is directly established independently of that version.

**Confidence:** very high for flag mutation; high for cross-spawn leak on inspected Rust, exact pinned toolchain unverified.

**Fix direction:** never mutate caller-owned handle flags; use scoped copies and enforce handle-list inheritance for every production spawn that can overlap, including credential helpers.

**Catching test:** pass a noninheritable sentinel handle as confined stdio, then assert its flag is unchanged. Pause custom spawn after creating bootstrap handles, launch a disposable helper, and require those sentinel handles to be invalid in that helper.

### platform-08 — A valid Windows home path can trigger unbounded slot-wait threads

**Location:** `crates/ipc/src/slots/named.rs:724–737`, supporting `:714`, `:660`.

**Defect:** The busy-slot wait converts a Windows directory through lossy Unicode and treats every change-notification error as a changed list, repeatedly spawning uncancellable wait threads.

**Scenario and proof:** Use a valid Windows home path containing an unpaired UTF-16 surrogate, with every named-pipe slot held. `wait_free` first spawns a thread in `WaitNamedPipeW(..., NMPWAIT_WAIT_FOREVER)`. It then constructs the directory as `pipe::wide(&dir.to_string_lossy())`, changing the path to U+FFFD. Notification creation fails on the wrong path, and `change.is_none()` returns `Freed::Listed`. `take_listed` rereads the unchanged list and repeats, spawning another blocked thread each time until exhaustion. Actual watcher access/resource failures take the same swallowed-error loop even with ordinary paths.

**Confidence:** very high for retry/leak control flow; live Windows failure unrun.

**Fix direction:** encode the OsStr directly as UTF-16; create the watcher before the helper thread; propagate actual errors and give waits bounded ownership/cancellation.

**Catching test:** a home made with `OsString::from_wide` including D800, all slots held: acquisition should wait with bounded threads and finish when a slot frees. Inject notification allocation failure and require an error without additional waiter threads.

### platform-09 — Case-insensitive Windows filenames can make versioned saves loop forever

**Location:** `crates/vmm/src/platform/windows_files.rs:402`, `:381–395`.

**Defect:** Version enumeration is case-sensitive while filesystem collision handling is case-insensitive, so collision retries can choose the same occupied name forever.

**Scenario and proof:** On ordinary case-insensitive NTFS, `write_in(dir, "state", b"a")` creates `state.~1`. A subsequent `write_in(dir, "STATE", b"b")` uses `strip_prefix("STATE")`, misses that existing version, chooses `STATE.~1`, receives STATUS_OBJECT_NAME_COLLISION, and repeats unchanged. `nt_open` explicitly uses OBJ_CASE_INSENSITIVE (`:226`); rename replaces nothing (`:596`). A case-renamed/imported `CURRENT.~1` similarly hangs a snapshot save using the internal lowercase `current` pointer. Reads through alternate case also miss the latest version.

**Provenance:** introduced in-range by `76d80231b`. **Confidence:** high; real NTFS run unverified.

**Fix direction:** use the directory's filename-equivalence rules consistently and refuse an unchanged collision instead of retrying forever.

**Catching test:** write/read through mixed-case names; precreate case-renamed snapshot pointer versions; require operations to terminate and select the latest data.

### platform-10 — Windows priority credits do not protect the host against multiple raised threads

**Location:** `crates/vmm/src/platform/windows_thread.rs:226–231`, `:252–258`.

**Defect:** Independent half-CPU credits never demote a set of raised threads that collectively saturates the available CPUs while each stays below half a CPU.

**Scenario and proof:** Place three unrestricted one-vCPU VMs on the same single affinity CPU, each running continuously, beside a normal-priority host CPU worker. Each watched thread gets about one third of wall time. `credited` adds half of wall time and subtracts its own use, so every credit stays positive and every vCPU stays ABOVE_NORMAL. There is no host-wide aggregate credit owner. Under Windows' strict priority rule cited in this module, normal host work runs only when OS starvation-prevention/dynamic boosts rescue it, rather than sharing through the governor. The single-spinner test does not cover this state. **This is not a claim that Windows will literally never schedule the host thread; rescue boosts and actual delay are unmeasured.**

**Evidence:** the module's Scheduling Priorities discussion at lines 6–9 and 26–37, plus the arithmetic above; no VM CPU hold refutes unrestricted runs.

**Confidence:** high for the algorithmic fairness hole; host effect/timing requires coordinator measurement.

**Fix direction:** account for aggregate boosted occupancy against the CPUs actually available, including multiple VM processes, or use a host scheduler mechanism that preserves normal-thread fairness.

**Catching test:** compare a normal-priority host spinner beside one versus at least three continuously runnable prioritized actors sharing one CPU; assert host progress and priority demotion of sustained aggregate load. Observe CPU time, not only base priorities.

### platform-11 — Scheduling periods and pipe quotas remain chosen production constants

**Location:** `crates/vmm/src/platform/macos_thread.rs:34–35`, `:46`; `crates/vmm/src/platform/windows_thread.rs:66`; `crates/ipc/src/pipe.rs:68`, `:73`.

**Defect:** Production scheduling and IPC sizing use fixed benchmark/default-derived choices rather than host-learned values or specification bounds required by the binding R03 clarification.

**Concrete evidence:** macOS uses 500,000 ns computation and 1,000,000 ns constraint, explicitly attributed to M10; the latter also fixes suspect sampling. M8/M10 compare policies using those choices; they do not derive invariant required values across supported Macs. Windows sets PERIOD to 100 ms from PM M294's clock experiment. Pipe quotas are fixed at 1 MiB and 64 KiB: comments explicitly cite M340 throughput and Linux's default FIFO capacity. None is a protocol maximum. The windows_thread implementation does not learn the current clock/wake resolution; no pipe path adapts these quotas to measured capacity or cold-start uncertainty.

**Scenario:** a first launch on a supported host immediately adopts those periods/quotas before observing that host. Under load, sampling and backpressure retain those same choices. The concrete failure here is the R03 policy contract; no invented latency or throughput regression is claimed.

**Confidence:** very high; constants and stated derivations are explicit. XNU scheduler tick, fail-safe quanta, ABI constants, and Win32 priority enumerants are separate cited invariants and are not included in this finding.

**Fix direction:** derive choices from online host/workload observations with documented conservative cold-start behavior and authority ceilings, or establish a genuine specification bound for each retained constant.

**Catching test:** inject different clock/wake/capacity observations and cold-start states; require derived choices to follow those observations and preserve safety/refusal bounds. Coordinator owns real host contention validation.

### platform-12 — Delegated-cgroup cleanup polls at an arbitrary 10 ms interval

**Location:** `crates/shards/src/daemon/parents.rs:339`; `crates/shards/src/daemon.rs:411–412`.

**Defect:** Cleanup repeatedly probes cgroup files using a fixed production sleep despite cgroup readiness notifications and a surrounding learned deadline judge.

**Scenario and proof:** `emptied()` rereads cgroup.events/cgroup.procs, then unconditionally calls `slept(Duration::from_millis(10))`; the production implementation actually sleeps that duration. An empty transition just after the read cannot be observed until the next probe; a populated leaf causes approximately 100 probes/second, independent of the judge's due time. D183's learned cleanup decision does not derive this chosen interval. This violates both R03 and R05's notification-first requirement; no measured cost claim is made.

**Confidence:** high from the loop; actual latency/cost unmeasured.

**Fix direction:** wait on cgroup.events populated readiness with the learned judge's due time as needed, rather than periodic state reads.

**Catching test:** a disposable cgroup becomes unpopulated just after the first read; require readiness-driven completion and no periodic probe loop. Retain cleanup/resource witnesses in coordinator validation.

## Checked and found sound

- Seccomp compilation checks the native audit architecture before syscall dispatch (`crates/syscall-filter/src/lib.rs:135–140`); exact syscall matching rejects x32-numbered calls. ptrace, process_vm_writev, open_by_handle_at, exec and fork are absent. Clone is restricted to thread flags; clone3 returns ENOSYS. These checks do not repair platforms 01–04.
- Ordinary KVM ioctls examined are request-number filtered through architecture-specific IOCTLS tables. GPU builds deliberately add documented ioctl-type-wide exceptions; no generic all-ioctl allowance was found.
- Linux SCM_RIGHTS receive uses MSG_CMSG_CLOEXEC and takes ownership before error/truncation handling (`ipc/src/unix.rs:1090`). Unix spawn remapping handles source/target collisions and closes temporary copies.
- D183 places the spawner into its cgroup leaf and returns the group only after successful placement (`daemon/parents.rs:229–252`); placement failure ends the spawner. Parent paths reject dot components and enforce delegation ancestry. A correctly Landlocked VM has no cgroup-write grant; platform-01 undermines that premise.
- Windows named pipes examined use protected user-only DACLs, PIPE_REJECT_REMOTE_CLIENTS and identification-only client SQOS. Confined jobs enforce one process and kill-on-close at creation without breakaway. AppContainer handles are explicitly passed rather than arbitrary guest-selected parent handle numbers.
- Windows beneath-root walks use relative directory handles, reject reparse points by handle and validate plain components. The version-name flaw is separate from that confinement walk.
- WHP arm64's vcpus_created no-op corresponds to earlier GIC setup; hotplug returns a mapper used later. EL2/ITS limitations examined have explicit refusals.
- macOS carried VM entitlements request App Sandbox and hypervisor only. Read-only input files are passed as descriptors; write-directory bookmarks are limited to requested snapshot directories. Unsupported device grant paths explicitly refuse. These observations do not establish live sandbox behavior.

## Not covered

- Delivery: direct kiln sends were rejected with typed queue backpressure; no message acceptance is claimed. Further delivery is left to the user under their final instruction.

- No forbidden-effect experiment, build, test, policy script, KVM/HVF/WHP guest, cgroup exercise, race stress, or seven-target qualification was executed. R09/R10 live reproduction and mutation evidence remain owed to ROOT; the brief explicitly prohibited reviewer execution. R08/R14 performance gates were likewise not run or claimed.
- No exhaustive certification of WHP state restore, WHP arm64, hardware assignment, Windows daemon commands, all HVF/KVM paths, or every production literal. Platform parity closure in the audit is not treated as proof these defects cannot occur.
- No verified macOS host-starvation exploit; governor sampling and constants were reviewed, but adversarial busy-host fairness requires a live campaign. No broader-than-requested macOS bookmark grant was established.
- Exact Rust 1.98.1 handle inheritance, Windows scheduler rescue behavior, NTFS collision execution, and WHP emulator instruction/callback routing need real Windows validation. Proposed end-to-end failures were not mislabeled reproduced.
- No claim of an independent kernel/hypervisor exploit. P0 confinement findings assume arbitrary code in the VM process, precisely the scenario the outer process sandbox is meant to contain.
- Documentation sources reviewed include D30/macOS confinement, D141, D183, D191, D195/D196, PM M8/M10/M294/M317/M340 and the platform-parity audit. This report is source evidence at the pinned revision, not an R01–R16 implementation readiness or merge approval.
