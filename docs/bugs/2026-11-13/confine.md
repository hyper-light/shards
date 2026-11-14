> Archived on 2026-11-13 at the user's request. The report below records the completed review of revision `2bc5f78f9`; its read-only statements describe that review. This archival write is documentation only. The user will notify Shards.

# confine adversarial review

POLICY_READ: role `adversarial reviewer`, area `confine`, review coordinator for the scoped read-only lanes below; overall coordinator ROOT. HEAD and origin/main: `2bc5f78f9e48d09ddbf4e848f9960133f6d19148`. Base: `79caf2d74`.

Policy SHA-256 at HEAD:
- AGENTS.md: `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41`
- docs/development-rules.md: `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`
- docs/development-evidence.md: `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63`
- CLAUDE.md: `588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010`

Binding task handoff `/tmp/g121-shot/review/brief.md` read in full first. Repository policy and managed-runtime plan read. Owned scope: guest exec/attach/TTY confinement, exec concurrency, HOME/environment/cwd, host-path plugs/case folding/watch delivery and their daemon/workload composers. No tracked edits, builds, tests, VMs, daemons, vendor services or heavy commands authorized. Findings below are source-verified; live reproductions and tests are explicitly unrun and owed to ROOT.

Priority scale used here: P0 = critical isolation/security defect, P1 = high functional defect, P3 = medium, P5 = low (no P2/P4 assigned). These are **source-verified findings**, not live reproductions. The binding brief forbids executing their reproductions. Each proposed regression below is unrun.

Review lanes: member_exec, home_env and plug_watch each returned the full policy readback before inspection, with identical HEAD and hashes. Each was confined to reading this worktree and prohibited from further delegation or any build/test/runtime execution. Overall coordinator ROOT retains reproduction, audits, benchmarking and integration validation.

## Findings, ranked by severity

### confine-01 — P0 / critical — member exec can acquire a wider Landlock filesystem grant

**Location at HEAD:** `crates/init/src/domains.rs:3299` (path opening at 3280; rule construction 3299–3306); `crates/init/src/enter.rs:495`.

**Defect:** Exec reconstructs a member's writable volume rules by following mutable pathnames, so a running member can retarget a pathname and make its exec receive rights the member itself lacks.

**Concrete scenario:** Give a member an owned writable outer volume at `/data`, initially containing `parent/inner`, and a second writable volume mounted at `/data/parent/inner`. After startup, the member renames `/data/parent` to `/data/moved`, creates a new `/data/parent`, and makes `inner -> /proc` in it. Start an exec in this member. Its saved `Confinement.volumes` still contains `/data/parent/inner`; `landlocked` follows this symlink and adds `FS_ALL` for `/proc`. The original member's rules never granted write access to `/proc`, but the exec can now write its own `/proc/self/comm` (ordinary member DAC permits it). This proves a widening without requiring a host escape or capabilities. Another simple widening witness is a writable proc file such as the exec's own `oom_score_adj` toward a less protected value, subject to that file's own restrictions.

**Evidence and refutation:** `domains.rs:2167–2175` records volume paths and modes in `volume_rules`; `:2197` retains them for future exec. `landlocked` defines `open` as `open(path, O_PATH|O_CLOEXEC|flags)` at 3280–3281, then opens each volume with flags zero and grants `FS_ALL` for writable volumes at 3299–3306. Initial startup uses these paths before the member can mutate them; exec calls the same function much later. The outer writable volume defeats the otherwise sound read-only member root defense. Nested destinations are accepted: `once_each` at 200–207 rejects exact cleaned duplicates only; `domain_mount` at 238–259 does not reject nesting. Renaming a directory containing a submount does not rename the mountpoint itself: Linux's [v6.18 vfs_rename check](https://github.com/torvalds/linux/blob/v6.18/fs/namei.c#L4893-L4895) rejects source/target dentries that are themselves local mountpoints. The original member's `/data` Landlock rule does not follow a symlink into `/proc`; the new exec's ruleset creation does. The default seccomp profile permits ordinary open/write operations. D243 promises a command reaches nothing its member cannot, with the same rules; this violates that contract and R04's no-widening requirement.

**Confidence:** High from the complete source flow; actual mount rename and forbidden proc write remain unrun.

**Fix direction:** Preserve and validate volume object/mount identity rather than resolving saved paths afresh through mutable ancestors. Build exec grants from the original authorized objects, or explicitly refuse an exec when identity/topology no longer matches. Merely rejecting a final symlink leaves ancestor replacement races.

**Catch test (ROOT, real guest):** Seed the two volumes above with ownership assigned to the member. Have its original process show `open("/proc/self/comm", O_WRONLY)` fails under Landlock, perform the rename/symlink mutation, then run a member exec that tries the same operation. Assert it is refused or stays denied. Check the actual proc write effect, not only the reported ruleset. Remove the identity guard independently to restore the widened effect.

### confine-02 — P0 / critical — initial cwd entry bypasses member DAC permissions

**Location at HEAD:** `crates/init/src/domains.rs:3065`.

**Defect:** The initial member changes directory while still root, retaining a cwd reached through ancestors its eventual uid cannot traverse.

**Concrete scenario:** Image has `/private` owned by root mode `0700`, `/private/work` mode `0755`, and a harmless sentinel `/private/work/token` mode `0644`. The member artifact contains `work -> /private/work`; config uses `run.workdir="work"` and an absolute executable that reads relative `token`. Lexical validation passes. Root enters `/agents/a/work`, then drops to the member uid. A relative read of `token` starts from the already-held `/private/work` cwd and succeeds, bypassing the root-only ancestor. A `chdir` performed as the member would fail with EACCES.

**Evidence and refutation:** `domains.rs:752–759` validates workdir lexically and prefixes the artifact directory; it does not resolve symlinks. `exec_child` calls `chdir` at 3065 before `confine` at 3068, whose `take_ids` is at 3203. `image_root` at 1175–1184 retains the full image under the skeleton, so the generic `/private` subtree remains present. Landlock grants reads and execution under `/` at 3283–3287 and cannot repair the skipped ancestor DAC check. The relative file access still checks the leaf directory and file permissions; the scenario deliberately makes those accessible. In contrast, member exec correctly confines at `enter.rs:495` before `chdir` at 499. D231 (architecture.md:25039 onward) says configured `run.workdir` stays inside the artifact; D59 also relies on the member's uid and filesystem permissions. This is the cwd/symlink escape explicitly assigned in the brief.

**Confidence:** High from source; no live forbidden read claimed.

**Fix direction:** Enter cwd with member credentials and confinement already active, retaining typed launch failure. If resolution is prepared earlier, do not turn a root-opened directory into authority the member could not obtain.

**Catch test:** Boot the fixture image above and attempt the relative sentinel read. Require startup refusal/EACCES and no sentinel output. Independently move cwd entry ahead of credential drop to show that the test observes the actual forbidden read.

### confine-03 — P1 / high — staged HOME files are written behind nested mounts

**Location at HEAD:** `crates/init/src/domains.rs:2123`; `crates/init/src/staged.rs:107`.

**Defect:** Init chooses one backing mount for HOME and stages every relative file through it, ignoring nested mounts that cover the final file.

**Concrete scenario:** Writable volume A at `/home/h` contains `.config`; writable private volume B mounts at `/home/h/.config`; member `HOME=/home/h`. Stage `.config/auth.json`. Init writes A's hidden `.config/auth.json`; after B is mounted there, the member sees no staged authentication file. The sensitive file also persists in the unintended backing volume. Using a public sentinel is sufficient to prove the failure.

**Evidence and refutation:** `home_in` at `domains.rs:677–693` selects the deepest mount containing HOME itself, so it selects A. `:2113–2133` passes that one mount to `staged::files`. `staged.rs:107–115` opens it once and writes all staged paths relative to it; `:135–141` walks components without consulting the mount plan. Child later attaches the member's individual volumes at `domains.rs:2969–2980`, hiding the staged file. Nested distinct mountpoints are accepted by `domains.rs:200–207` and the build's duplicate-destination check (`crates/agentfile/src/agentfile.rs:1120`). A read-only nested cover is also omitted by the `!sh.ro` filter at 2115, causing writes behind it rather than a refusal. Existing nofollow protection does not address mount visibility. D231's Staging contract (architecture.md:25024–25037) and D240's home contract (26837 onward) require files to land in the home the member actually reads.

**Confidence:** High; runtime result unrun.

**Fix direction:** Resolve each final staged target through the complete visible member mount topology, including read-only covers; write into its actual backing mount or refuse before any write. Preserve descriptor-relative nofollow traversal.

**Catch test:** Stage a sentinel under `.config` with A/B as above; member must read it, B must contain it, and A's hidden directory must not. Repeat with a read-only nested cover and require a typed refusal with no hidden write.

### confine-04 — P1 / high — initial member PATH search uses init's directory and mount view

**Location at HEAD:** `crates/init/src/domains.rs:649`.

**Defect:** Relative and empty PATH components are resolved before the member's cwd, mounts and credentials are established, refusing valid commands or selecting a different candidate.

**Concrete scenario:** Member artifact has executable `/agents/a/bin/tool`, config `run.command=["tool"]`, `run.workdir="."`, and runtime `PATH=bin`. Init checks `bin/tool` relative to its own `/`, so a valid local program is refused if `/bin/tool` is absent. For `PATH=:/usr/bin`, the empty component becomes `/tool` rather than `./tool`.

**Evidence and refutation:** `executable` concatenates `[p, "/", first]` then calls `access(X_OK)` at 649–652. It runs at 2094, before runtime workdir selection at 2097 and staging at 2133. `exec_child` later enters the member cwd and execs only the selected path at 3065–3075, without retrying candidates. Ordinary exec's `run::candidates` already maps empty components to `.` (`run.rs:6624`); initial members use this separate implementation. The existing PATH unit case uses absolute paths. D59's environment contract (architecture.md:24040–24043) and the function's own execvp contract require lookup on the environment the member runs with.

**Confidence:** High; startup behavior unrun.

**Fix direction:** Prepare candidate strings, then resolve and attempt them in the actual member filesystem view, cwd and credentials, sharing ordinary exec semantics.

**Catch test:** Start `tool` from artifact `bin/tool` with `PATH=bin`; assert it runs. Repeat with a leading empty component and a cwd-local executable. Include a later PATH candidate to catch selection in the wrong view.

### confine-05 — P1 / high — concurrent plugs overwrite destination ownership

**Location at HEAD:** `crates/shards/src/api_run.rs:5098`, `:5174`; daemon twin `crates/shards/src/daemon/devices.rs:431`, `:511`.

**Defect:** Destination availability is checked without reserving it, allowing concurrent plugs to mount twice and overwrite one owned-device record.

**Concrete scenario:** Two API-run `Plug` asks target the same member and path. Both pass `contains_key` before either inserts. Each starts its own share/device and mounts it. Both return success, but the map retains only the last `Plugged`. Unplug knows only that device; a hidden mount/device remains untracked, or completion order leaves the retained record describing a different device from the top mount.

**Evidence and refutation:** `api_run.rs:4435–4440` explicitly spawns each plug independently. The temporary check lock at 5098 is released before share startup (5118), device admission and guest mount (5156); insertion at 5174 does not test replacement or clean up the overwritten value. `Shares` requires explicit `.end()` for reaping (2679); overwritten API-run ownership bypasses it. Daemon `plug_one` has the same check/setup/insert structure at the locations above, without a destination reservation. Init's member mount path does not add a destination-ownership state. D235 provides `Taken`, owned plug/unplug behavior and resource cleanup; R04 requires explicit transitions.

**Confidence:** High for the race; kernel mount schedule/resource effects unrun.

**Fix direction:** Reserve destination with explicit Pending/Mounted/Removing ownership before any side effect. Serialize plug/unplug for that destination and release reservations on every failure.

**Catch test:** Barrier two requests after admission; require exactly one success and one Taken, one live mount/device, then complete device/share/fd return after unplug. Also reverse completion order and overlap unplug with pending plug.

### confine-06 — P1 / high — overflow/coalescing loses wakes for descendant watches

**Location at HEAD:** `crates/vmm/src/devices/virtio/fs/notices.rs:208`, `:293`, `:59`; `crates/init/src/notice.rs:135`.

**Defect:** Lost or coalesced subtree changes become a single ancestor touch, which never wakes inotify watches below that ancestor.

**Concrete scenario:** Guest watches `/share/subdir/file` or `/share/subdir`; host changes the file in a burst whose individual events are replaced by a share-root event. No later edit occurs. Only the share root receives `IN_MODIFY`; the nested watcher remains asleep indefinitely and sees no loss indication.

**Evidence and refutation:** Mac notices dispatch reads `change.path` but discards `change.flags` at 208–212, including `MUST_SCAN_SUBDIRS` defined by `crates/apple/src/fsevents.rs:23`. Windows handles native overflow (`got == 0`) by `told(..., Vec::new())` at 293–295. Pending saturation truncates paths toward the root at 59–64. Guest `Mounts::touch` resolves and touches only the resulting path or `.` at `notice.rs:135–145`; it neither visits descendants nor produces `IN_Q_OVERFLOW`. Inotify watches are inode-specific, not recursive. Root watches that an application happens to install cannot substitute for its requested nested watch. D235's "Host edits wake the guest's inotify" and `docs/research/measurements/s8-notices/README.md` require a host edit to wake the guest watch; retained basic write/remove cases do not exercise loss/coalescing.

**Confidence:** High for the lost-wake transition; real overflow unrun.

**Fix direction:** Preserve subtree/loss semantics and invalidate/wake all affected held directories/files, or expose loss to every affected watcher so it can rescan. A root-only ordinary change is insufficient.

**Catch test:** Hold only a nested watch, inject/force a root-coalesced event (Mac MUST_SCAN_SUBDIRS, Windows overflow, and pending saturation independently), and assert that watch receives an observable change/loss and reads the final bytes.

### confine-07 — P3 / medium — rejected exec environment values are copied into errors

**Location at HEAD:** `crates/init/src/enter.rs:94`.

**Defect:** A malformed environment entry is included in full in the member-exec refusal, exposing values through the error/stdio/control path.

**Concrete scenario:** Managed exec request supplies an environment byte string such as `API_KEY=<public-test-sentinel>\0suffix`, e.g. a malformed credential from an integration. `over` rejects it, but the returned refusal contains the entire value. The controller receives it as an error instead of the value staying solely in the environment input channel. No real credential was used in this review.

**Evidence and refutation:** `enter.rs:290–292` returns `e.clone()` for a NUL, empty name or reserved key; line 94 renders `String::from_utf8_lossy(&e)` with Debug formatting, escaping the NUL while retaining the value. `daemon/managed/execs.rs:103–137` forwards `Spec.env` unchanged; `init/run.rs:56–59` decodes and checks setup keys, without environment NUL validation. `Exec::start` encodes the refusal into SYSTEM_ERR (`run.rs:4653–4666`), and `workload.rs:1792` onward renders the member refusal. This is a value-in-errors violation of R04 and the brief, even though the request itself is correctly refused and the error returns to its owner. It is not a demonstrated cross-owner disclosure.

**Confidence:** High for disclosure into the refusal; no live propagation run.

**Fix direction:** Return a typed reason and, if useful, a validated variable name; never include the input value in Display/Debug or wire error text.

**Catch test:** Pass public sentinel values in each rejected-entry shape; assert no error, encoded refusal, stderr or telemetry contains the value, while reason/member identity remain available.

### confine-08 — P3 / medium — Linux host watch admission failures disappear

**Location at HEAD:** `crates/vmm/src/devices/virtio/fs/server.rs:2580`.

**Defect:** Failed host directory-watch admission is swallowed, so a guest can install a watch that silently misses subsequent host edits.

**Concrete scenario:** Host user's `max_user_watches` is exhausted before a fresh share subdirectory is looked up. `watch_named` fails (e.g. ENOSPC); guest lookup and its inotify watch still succeed. Host edits produce no notices, and no failure/degradation is reported.

**Evidence and refutation:** `watch_dir` only handles `if let Ok(watched) = ...` at 2580–2583 and returns nothing. Its comment at 2564–2566 explicitly says a failed watch is omitted and the guest told nothing. D235 recognizes finite host watch resources; that does not waive R04's requirement to propagate failures or the advertised watch behavior. This is distinct from losing an event after a watch was successfully admitted (confine-06).

**Confidence:** High; quota exhaustion unrun.

**Fix direction:** Retain and propagate typed watch-admission failure and refuse the promised behavior or provide explicit degraded/recovery semantics.

**Catch test:** Exhaust the host watch quota, then look up/watch a fresh nested directory. Require an observable admission/degradation result rather than silent success followed by no events. Release quota and verify any promised recovery.

### confine-09 — P3 / medium — pending notice capacity is an upstream default

**Location at HEAD:** `crates/vmm/src/devices/virtio/fs/notices.rs:27`.

**Defect:** Production fixes pending notice capacity at `MOST = 16384`, justified only by Linux's default inotify queue size rather than learned/current capacity or a protocol invariant.

**Concrete scenario:** A guest configured with a larger inotify capacity still loses precise changes at this baked-in value; macOS and Windows use the same value despite having different native watcher capacities. The value controls the lossy root-coalescing transition in confine-06.

**Evidence:** Lines 23–27 explicitly cite the Linux default; D235 (architecture.md:25555–25559) and the R03 row in `s8-notices/README.md` repeat it. Neither makes this a spec bound or measures the executing instance. The brief's current R03 instruction expressly says an upstream default is insufficient and requires learned/data-derived production values, including cold-start behavior.

**Confidence:** High; source-policy violation, no runtime reproduction needed to establish the literal's derivation.

**Fix direction:** Derive admitted capacity from the actual applicable resource/admission constraints and retain explicit loss semantics independently of the number. Do not merely substitute another fixed count.

**Catch test:** Change the applicable runtime capacity/resource envelope and show admission follows its authoritative input rather than 16384; exercise cold start and overflow/loss outcomes separately.

## Checked and found sound

- The exec admission fix separates `DOOR` from `WAITERS`, admits while registration holds the waiter lock, matches a complete HELLO by token, and uses nonblocking candidate sockets (`vm/mod.rs:2461`). Token comparison is whole and constant-time. No source-proven recurrence of the ordinary 64-exec busy-lock hang was found. `EXEC_FAILED`, signal-link failure and listener closure resolve their pending waiters. This is source inspection, not a 64-exec pass.
- Member exec joins a pidfd-selected member process's namespaces, is born into its PID namespace/cgroup with clone3, drops bounding capabilities before IDs, clears capabilities/ambient set, sets no_new_privs, applies Landlock/seccomp, and closes inherited descriptors above its CLOEXEC error channel. The mutable rule identity defect is reported separately.
- TTY creation uses a fresh PTY, TIOCGPTPEER and member ownership. Its master remains with init. Carried kernel configurations disable legacy TIOCSTI; TIOCLINUX has no console descriptor here, and controlling-terminal theft requires capabilities these members lose. No source-proven ioctl escape was found; custom-kernel behavior was not certified.
- Environment precedence follows image → member run.env → runtime staged env, with reserved SHARDS_DOMAIN refused. Image HOME is excluded intentionally. Permitting LD_PRELOAD/PATH overrides after confinement was not by itself a privilege defect.
- Staged paths reject lexical traversal; directory traversal uses held directory descriptors and O_NOFOLLOW; final staged symlinks are replaced with O_EXCL|O_NOFOLLOW. These guards do not solve the nested-mount visibility defect.
- Member plug paths require strict descent under authorized roots; component creation uses descriptor-relative openat2 no-symlink/no-cross-mount constraints; member shares get nosuid/nodev. Notice traversal follows no symlink and its special identity does not modify host files.

## Not covered

- Every proposed live reproduction, test, mutation, heavy E2E, chaos run, race schedule, overflow/quota experiment and seven-target/platform gate is **unrun** as required by the binding brief. No test/build/daemon/VM was started. `scripts/check-development-policy` was not run: this is a read-only review, not a readiness claim for code changes.
- Linux glibc/musl, HVF, WHP and nested arm64 KVM behavior at HEAD are not newly validated by older retained measurements. Full host/share filesystem security, network policy and managed capture outside the named seams are outside this area.
- A case-insensitive alias-cache candidate remains uncounted: cache canonical `Foo`, rename guest alias `foo` to `Bar`, then look up cached `Foo` while its canonical positive TTL remains. `server.rs:2622` disables TTL for alias spellings only; Mac IgnoreSelf suppresses the share's own rename notice. The kernel's exact alias/dentry handling needs ROOT's live APFS run before this becomes a finding. Existing canonical-source rename cases do not settle the inverse ordering. No case-fold collision exploit was proved here.
- Synchronous pre-exec error-pipe waiting and ignored read errors in `enter::start` were inspected, but no concrete reachable indefinite pre-exec stall or pipe-error symptom was established; not counted.
- D243 explicitly discloses sibling Landlock scopes (exec cannot signal the member's original processes/use their abstract sockets) and omission of the contested-port chooser filter. These are retained design limits, not newly claimed escapes.
- This is a bounded source review, not an exhaustive proof of panic freedom, every literal, allocation safety, absence of fd/thread leaks or race freedom.

## Result

9 findings: **P0 critical 2; P1 high 4; P3 medium 3; P2/P4/P5 0.** All source-verified; runtime confirmation remains ROOT's. No tracked files changed, no commits/pushes/PRs.

## Kiln notification

Explicit user authorization to notify ROOT was received during review. `agents_send` to Shards/ROOT `01a148884a4770fdaf0111f3f792a248` was attempted in steer mode, then queue mode with an immutable retry key. Kiln returned typed HTTP 429 backpressure (`pending=3`, `authority=2`), so those attempts were not accepted. ROOT's agent is running; the report and ranked message are retained for retry. No delivery is claimed without an acceptance receipt.
