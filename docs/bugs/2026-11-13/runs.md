# Runs and pools adversarial review

POLICY_READ — role: **adversarial reviewer**, area **runs**. Review coordinator within this lane: Codex; main coordinator: ROOT, kiln agent `01a148884a4770fdaf0111f3f792a248`. Head and loaded `origin/main`: `2bc5f78f9e48d09ddbf4e848f9960133f6d19148`. Base: `79caf2d74`. Worktree: `/Users/adalundhe/Projects/shards/.claude/worktrees/rv-runs`.

Read `/tmp/g121-shot/review/brief.md` in full first, then the four required policy/mechanics files in full. SHA-256 receipts:

| File | SHA-256 |
| --- | --- |
| `AGENTS.md` | `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41` |
| `docs/development-rules.md` | `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3` |
| `docs/development-evidence.md` | `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63` |
| `CLAUDE.md` | `588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010` |

Three scoped read-only subreviewers acknowledged the same full policy, revision and hashes before reviewing platform/volumes, helpers/templates, and defaults/homes. None edited tracked files, built, tested, started actors or delegated further. The active review brief was the task handoff. Relevant portions of the D-entries and linked evidence were checked; no prior evidence is represented as a new head validation result.

**Verification boundary:** these are source-proven defects and explicit counterexample schedules, independently reread and checked for contrary guards. The brief forbids running their reproductions. **No live reproduction, test, build, daemon, VM, Docker, policy-check script, benchmark or mutation was run.** Proposed tests below are unrun coordinator work. The final worktree status was clean. Inherited defects are identified separately from changes introduced in this range.

Priorities follow the requested P0–P5 ordering: P0 critical, P1 high, P2 medium; no P3–P5 findings are assigned. Counts: **P0 0; P1 5; P2 6; P3 0; P4 0; P5 0**.

## Findings, highest severity first

### runs-01 — P1 / high — Failed owner metadata grants guest-created files to a later run's user

**At head:** `crates/vmm/src/devices/virtio/fs/server.rs:2071`, with `:2063–2075` and `:2354–2365`.

**Defect:** ordinary-file creation acknowledges owner-xattr write failures, while the new missing-owner fallback assigns the file to whichever run user mounts it next.

**Scenario:** a uid-0 run creates a mode-0600 file in a bind after init sets the share owner. `fset_xattr` fails (ENOSPC, EIO or unsupported xattrs). For an ordinary file, `kept.kind == None`, so this guard does not return the failure:

```rust
if let (Err(e), Some(_)) = (recorded, kept.kind) {
    let _ = unlink_at(dir, name, false);
    return Err(e);
}
Ok(())
```

A later uid-1000 run sees the same bind. `node_owner` uses `.ok().flatten() ... .unwrap_or(unrecorded)`, and `unrecorded_owner` supplies uid 1000. The kernel now sees that 0600 file as uid 1000's and permits access. Owner-xattr read errors also collapse into this permissive fallback, even when an owner was previously recorded.

**Evidence:** D38, `docs/design/architecture.md:3189`, explicitly requires every guest-created file, including root's, to retain its maker and refuse a later user. R04 prohibits swallowed errors. Ignoring ordinary-node record failures is inherited, but the range changes fallback from root to the current run's user and makes recording root-created files necessary. This is a new consequence of goal #18, not a claim that ordinary host files should stay root-owned.

**Confidence:** high in the source path and permissions consequence; live fault behavior unverified. **Fix direction:** propagate persistence errors and undo creation; distinguish absent metadata from failed reads and malformed records, allowing the fallback only for actual absence. **Test:** force owner-xattr persistence failure on an ordinary bind file and require explicit failure and no orphan; separately fault reads of a root-owned 0600 record and verify uid 1000 cannot read it. Observe actual guest permissions and cleanup.

**Status (2026-11-13):** fixed in 978d29f03d6084ec91d6d3010bd15b4e62c1302b (red `owner_metadata_failures_refuse_creation_and_lookup`, kiln r4); green regression +49 server tests and shards-vmm all-target clippy on macOS arm64 (r7). Evidence: `docs/research/measurements/runs-fixes/README.md`; real-VM fault/permissions validation owed to ROOT.

**Status (2026-11-13), follow-up:** fixed in 8b2505f423ac560dc2e853e6f6b394bfbe356429 (red `unrecorded_nondevice_nodes_keep_the_runs_user`, r34; green same test +50 server tests and VMM clippy r35). This corrects a regression introduced by 978d29f03: unrecorded native nondevices use the run user, while devices retain root. Guest-created native kinds without owner-xattr support retain their existing persistence exception; it is not claimed fixed by the ordinary-file metadata work.

### runs-02 — P1 / high — A renamed daemon home can make its daemon mutate another home's state

**At head:** `crates/shards/src/daemon.rs:2477`; effects at `:2267`, `:2287–2292`; request path retention at `crates/ipc/src/slots/fifo.rs:275`.

**Defect:** the home-loss guard checks existence and lock link count rather than identity, while later operations use the original absolute pathname.

**Scenario (Unix):** rename a running daemon's home `A` to `B`, immediately recreate `A`, and start a second daemon in `A`. The old daemon's CWD remains the original directory at `B` (`daemon.rs:1831`), but its `self.home` and `Requests.dir` still name `A`. `home_going` is only:

```rust
std::fs::symlink_metadata(&self.home).is_err()
    || crate::os::unlinked(&self.home_lock)
```

The replacement `A` exists and the old lock still has a link in `B`. The old daemon continues. Its close unoffers `A/requests` and writes `A/daemon.stopping` and `A/daemon.stopping.lock`, affecting the new daemon without holding its home lock. Old operations can likewise resolve new absolute paths in `A`.

**Evidence:** CLAUDE.md's per-`SHARDS_HOME` daemon contract and D185's ownership of request slots. `home_id` is compared only in `removed_here` (`:2483–2486`), guarding final recursive removal, not serving or these writes; file-watch wakeups do not add the comparison. The underlying existence-only guard is inherited from base; the range preserves it and adds absolute-path lifecycle effects.

**Confidence:** high for the Unix counterexample; no Windows rename claim. **Fix direction:** retain directory identity/handles, operate relative to the owned directory, and detect replacement before absolute-path work; cleanup must target the old directory rather than its reused pathname. **Test:** rename `A→B`, recreate/start `A`, stop the old daemon through B's Engine socket, and verify A's PID, request lists, stopping files and continuing query responses remain owned by the new daemon.

**Status (2026-11-13):** fixed in d3b11258a (red `renamed_home_cleanup_keeps_replacement_files`, resumed kiln r5; green regression including old-home remainder removal + stopping-lock check, shards all-target clippy r12 on macOS arm64). Landed on main via 3956d259b. Real-daemon/VM validation remains owed to ROOT as `docs/research/measurements/runs-fixes/README.md` specifies.

### runs-03 — P1 / high — BootStop cannot cancel a boot blocked sending its initial request

**At head:** `crates/api/src/image/session.rs:934–937`; same warm path `:894–898`; blocking write `:233–238`; stop registration `:741`.

**Defect:** the host is registered with `BootStop` only after the synchronous CREATE write finishes, so a stalled request write cannot be interrupted by the stop.

**Scenario:** boot with a valid staged member file large enough to exceed the host's stdin pipe capacity (the protocol permits payloads up to 6 MiB). The spawned host is stopped or stalls before reading stdin. `Session::start` calls `begun.create(...)?` before `.started(..., stop)`. CREATE reaches `stdin.write_all(&frame)` with no timeout. Another thread calls `stop.stop()`: the state is still Armed, so it changes to Stopped without killing any host. The boot stays blocked, retaining its host and resources, beyond its supplied patience; `.started` and `stop.hold` have not been reached. A warm take has the same ordering. This is also possible during a scheduling hold; cancellation must not depend on the host resuming to read its request.

**Evidence:** D231 at `architecture.md:25076–25115` promises stop kills the host and its tree during a boot and ends the boot. `Shared::send` has an unbounded pipe write; `patience` bounds only the later condvar wait. Existing stop stand-ins send small requests that fit the pipe, so they do not exercise this boundary. This API and BootStop implementation are introduced in the review range.

**Confidence:** high in the blocking/control gap; live scheduling reproduction unrun. **Fix direction:** hold the spawned host in the stop state before any potentially blocking CREATE I/O, retaining ownership through all initialization/error exits. Ensure stop can kill the host without acquiring the writer lock. **Test:** a stand-in announces its PID but never reads stdin; send a staged payload exceeding pipe capacity, wait until the write blocks, call stop, and require `Ended(Stopped)`, host/tree termination and resource cleanup without resuming the reader. Exercise fresh and warm paths.

**Status (2026-11-13):** fixed in a7ad98b9bb873f750fe7eadf916343c176c4e3c4 (red `a_stop_cancels_a_create_whose_host_never_reads`, kiln r20; green regression +33 session tests and API all-target clippy r22). Exact logs retained in `docs/research/measurements/runs-fixes/runs03-{red,green}.log`; real-VM cancellation/resource checks remain owed to ROOT.

### runs-04 — P1 / high — An orchestrator pool hands out a dead warm host and fails its run

**At head:** `crates/api/src/image/pool.rs:393–397`; `crates/api/src/image/session.rs:894–901`.

**Defect:** ready hosts are not evicted on death, and a known pre-CREATE dead host's write failure becomes a run refusal instead of a safe fresh-host retry.

**Scenario:** an `ImagePools` host successfully reports WARMED and is placed in `pool.ready`. It then dies while idle (for example, external kill or host failure). Its session watcher marks shared state, but nothing sends a death/removal event to its pool. `Pools::take` pops the stale host. `warm.create` fails writing to its closed stdin, and `ShardsHosts::take` returns `Taken::Refused(e)`. `Pooling::boot` propagates that refusal; only `Taken::Unfit` or no warm host obtains a fresh one. A healthy image whose first run succeeded therefore has its next run fail solely because the pool retained a dead spare.

**Evidence:** goal #3's no-dead-warm-VM contract; D253 adopts the daemon's warm-pool behavior. The daemon separately removes dead waiting VMs (`daemon/follow.rs:452–463`) and distinguishes surely untaken from uncertain handoffs (`daemon.rs:4499–4514`). The new API pool has only Take/Warmed/Ended/Stop events and no ready-host death event. Its test named `a_warm_host_that_does_not_fit_or_start...` exercises Unfit and failure before readiness, not death after WARMED.

**Confidence:** high; no live host kill performed. **Fix direction:** evict ready hosts on terminal state and retain typed definitely-untaken versus possibly-started failure semantics across CREATE. Retry only the definitely-untaken case; do not retry an uncertain run and risk duplicate execution. **Test:** hold a warmed host in the pool, kill and reap it before the next boot, then require the next request to run exactly once on a healthy replacement. Also kill during handoff and assert typed uncertainty rather than an unsafe replay.

**Status (2026-11-13):** fixed in bc0f69b5c3e4d3471236b2d2b13cbd1bcdc4246d (red `a_dead_warm_host_is_definitely_untaken`, kiln r13; green r14/r18). Replacement is limited to definitely pre-CREATE dead spares; uncertain handoffs remain non-replayed failures. Landed on main via 3956d259b; the real-VM fault scenario remains pending.

### runs-05 — P1 / high — Long-lived API runs and warm hosts block all image-store collection

**At head:** `crates/shards/src/api_run.rs:3507`, `:3526`, `:3618`, `:3750`; lease creation `crates/shards/src/run.rs:940`; exclusive collection `crates/image/src/store.rs:2296–2297`.

**Defect:** the API path retains the preparation lease for its entire warm wait/workload lifetime, rather than releasing it once the VM has its root filesystem.

**Scenario:** retain a long-running API guest or idle warm host in home H, then remove/replace an unrelated image so collection is due. `shape()` calls `run::prepare`, storing `Prepared { lease: Some(lease) }` inside `Shaped`, then `Plan`. A warm host keeps it across `input.next()`, and a running host across `serve()` through workload completion. There is no lease take/drop in `api_run.rs`. `Store::collect()` waits on an exclusive lock of that same `.lease`, so every unrelated unreferenced blob, root filesystem and abandoned template waits for these independent hosts to end. Continuing image turnover can fill the filesystem despite requested collection.

**Evidence:** `run.rs:376–378` defines the lease as lasting until the VM has its rootfs; D26 at `architecture.md:1293–1316` relies on open/mapped files thereafter. The daemon drops its corresponding lease at `daemon.rs:4397`. D253 introduces the API composition but omits that release. The held lease refutes the suspected normal-live API `.new-<pid>` deletion race: collection cannot reach it while this lease is held.

**Confidence:** high in ownership and lock conflict; disk exhaustion not exercised. **Fix direction:** release preparation lease after launch/readiness and template settlement, before WARMED or serving the workload. **Test:** after STARTED and separately WARMED, collect unrelated garbage and require completion while the hosts remain alive and workload progress continues. Do not claim a daemon shutdown join deadlock: `daemon.rs:1911` explicitly exits internally without waiting for these scoped threads.

**Status (2026-11-13):** fixed in 4f2384399f497103b054b8362e6d134f2f59618f by releasing the preparation lease after readiness, before warm wait and run serving. The live red/green is owed to ROOT: add/run `cargo test -p shards --test api_image a_live_api_run_and_a_warm_host_do_not_hold_up_collection` with unrelated garbage and live progress witnesses. The source path is unambiguous; api_run's 43 unit tests and shards all-target clippy pass (kiln r24), and are not represented as a live GC reproduction.

### runs-06 — P2 / medium — Container facts and copy ownership silently use another platform's config

**At head:** `crates/shards/src/daemon/inspect.rs:676`, `:888–909`; `crates/shards/src/daemon/commands.rs:910–919`.

**Defect:** container inspection and ownership-mode copy resolve only the image index ID, ignoring the immutable manifest recorded for the platform the run actually uses.

**Scenario:** on arm64, retain one index with arm64 `USER 1100`/`PLATFORM=arm64` and amd64 `USER 2200`/`PLATFORM=amd64`. Start `--platform linux/amd64` with no user/env overrides. The daemon records the selected manifest at `daemon.rs:5199`, but inspection calls:

```rust
let (config, manifest) = self.image_facts(c.image_id.as_deref());
```

`image_facts` finds `store.images()` by index ID and takes its preferred manifest/config. `inspect_doc.rs:576–695` uses these to fill User, Env, Cmd, WorkingDir, entrypoint and manifest, reporting arm64 facts for the amd64 run. `container_user` independently repeats the ID-only lookup; `copy_step` sends that wrong user for extraction ownership (`commands.rs:826–831`), which guest `copy.rs:110–125` applies as `UnpackOptions.chown`.

**Evidence:** D179 at `architecture.md:13251–13266` requires the chosen platform's own config and disk; `containers.rs:72–76` documents why `image_manifest` is retained. `Store::images`/`image` uses the named preferred manifest (`store.rs:1868–1874`), not the container's selected one. These older helpers were not updated for the expanded multi-platform state.

**Confidence:** high, source-proven; runtime emulation/copy unrun. **Fix direction:** use the container's recorded manifest, with an explicit old-record fallback; preserve effective creation config if needed after image removal/retagging. **Test:** start both variants, compare guest uid/env with container Config and ImageManifestDescriptor, and check extracted file ownership for each; repeat after changing the preferred platform.

**Status (2026-11-13):** fixed in 4ec005478df8a5cd957de25d3791cec46f264906 (red `container_facts_keep_the_runs_selected_manifest`, kiln r27; green regression, 2 inspect tests, 11 command tests and shards all-target clippy r29). Exact logs: `docs/research/measurements/runs-fixes/runs06-{red,green}.log`. Both inspect and ownership-mode copy consume the recorded manifest; missing selected config is refused for copy. Guest/copy ownership proof remains owed to ROOT as the fix record specifies.

### runs-07 — P2 / medium — Concurrent processes lose learned end maxima and can corrupt the record

**At head:** `crates/shards/src/daemon/ends.rs:133–149`.

**Defect:** `Ends::heard` rereads/merges shared maxima and writes shared `ends.new` under only an instance-local mutex.

**Scenario:** the stored teardown maximum is 1 s. Daemon A and API host B independently read it. A learns/publishes 9 s teardown. B then publishes its helper observation with stale 1 s teardown, overwriting the 9 s observation. The next daemon learns a smaller slice. A second schedule lets B truncate `ends.new` after A wrote it but before A renames; A publishes B's empty/incomplete inode and B writes through its handle into the published file.

**Evidence:** D217 at `architecture.md:8557` promises one shared record retaining each process's slower observations. The file's write/rename is at `ends.rs:147–149`; reread/merge is at `:135`, without an interprocess lock. Slices influence VM escalation at `daemon.rs:7312–7326`; premature-kill effects remain unverified. The two-instance test calls `heard` sequentially and cannot catch the race. Introduced in range.

**Confidence:** high in lost/non-atomic record; resulting premature escalation unrun. **Fix direction:** stable interprocess lock covering reread/merge/publication, plus uniquely owned staging files; explicitly unlock before closing (M244). **Test:** barrier two real writers after stale reads, order their publications, and require every maximum to survive; force the truncation/rename interleaving while readers verify complete records.

**Status (2026-11-13):** fixed in 45ae96862a934b8dabee70b5405cd8e0444f1f13 (red `concurrent_end_writers_keep_each_others_maxima`, r38; green regression +5 end tests and shards all-target clippy r39). Logs `docs/research/measurements/runs-fixes/runs07-{red,green}.log`. Stable-lock reread/merge and unique atomic staging preserve both writers' maxima. Two-process/VM crash publication remains owed to ROOT as the fix record specifies.

### runs-08 — P2 / medium — Concurrent first GPU launches can publish an incomplete helper

**At head:** `crates/shards/src/helpers.rs:102–107`. **Inherited at base; still present.**

**Defect:** GPU extraction shares one temporary pathname among every thread of a process and unlinks it before writing.

**Scenario (Unix):** A creates and opens temp inode A at `.<name>.<pid>`. B removes that pathname, creates inode B at it and pauses before finishing. A completes/syncs its now-unlinked inode A, then renames the pathname, which now names incomplete inode B, to the final helper path. A returns a path visible before B completes. B's own rename later fails ENOENT. A launch may see an incomplete executable or a file still open for writing.

**Evidence:** `vm_gpu` is called by launch (`launch.rs:199`); extraction is outside `dir()`'s OnceLock. Its comment at `helpers.rs:101` promises synced whole publication, as D36 requires. The PID-only temporary name provides no thread exclusion.

**Confidence:** high in the interleaving; loader outcome unrun. **Fix direction:** serialize GPU extraction or use unique writer staging and verified atomic publication. **Test:** barrier two callers around create/write/rename to force this schedule; require every returned helper to be complete and both callers to avoid extraction-induced failure.

**Status (2026-11-13):** fixed in d720debff92c7a3eda0022799bf1085a641b9cfa (red `concurrent_gpu_writers_publish_their_own_complete_files`, r42; green regression +4 helper cases reported and shards all-target clippy r44). Two carried-helper cases return early under skip. Writer-owned staging and close-before-publish prevent the forced empty-file race; logs in `docs/research/measurements/runs-fixes/runs08-{red,green}.log`. Real carried/GPU/crash checks remain owed to ROOT.

### runs-09 — P2 / medium — Concurrent disk-setting commands can publish partial settings

**At head:** `crates/shards/src/disk.rs:139–145`, read/modify/write at `:258–266`. **Inherited at base; still present.**

**Defect:** independent configure commands share `.disk.new` and perform unlocked read/modify/write transactions.

**Scenario:** A completes its temporary write. B opens `.disk.new` with truncation and pauses. A renames the now-empty inode into `disk`; a run reads default settings from that empty file. B writes through its still-open descriptor into published `disk`, then fails its rename because the temporary name has gone. Thus a failed command changes live settings, and readers see neither whole intended version. Concurrent disjoint updates can also overwrite each other from stale snapshots.

**Evidence:** D186 at `architecture.md:8352–8365` promises whole old/new files and that a failing command changes nothing. `main.rs:91` dispatches this command directly; no daemon transaction lock serializes it.

**Confidence:** high in the filesystem schedule; unrun. **Fix direction:** serialize the full read/modify/write and use uniquely owned staging files. **Test:** two actual CLI processes with barriers at truncation/publication, continuous readers, and assertions for whole versions, independent updates and failed-writer noninterference.

**Status (2026-11-14):** fixed in 2c148377c (red `concurrent_settings_updates_keep_each_others_fields` and `settings_publication_leaves_another_writers_staging_file_alone`, by mutation restoring the base's unlocked transaction and shared `.disk.new`, kiln r1785; green r1765 with shards all-target clippy, on merge 22d03c1a, macOS arm64). Salvaged from the Codex lane's uncommitted edit; reads take the newest version for Windows' write_in. Logs `docs/research/measurements/runs-fixes/runs09-{red,green}.log`; D186 amended. Two real CLI processes racing with a reader, per OS, owed to ROOT.

### runs-10 — P2 / medium — Reused helper bytes are accepted by length without checking their digest

**At head:** `crates/shards/src/helpers.rs:186–190`; GPU `:97–99`. **Inherited at base; still present.**

**Defect:** cached helper reuse discards the embedded SHA-256 and accepts any file of the expected length.

**Scenario:** one helper's content is corrupted without changing its length between invocations. `complete` destructures `(name, bytes, _)` and compares only metadata length; the GPU path does the same. New processes accept and execute the damaged helper rather than repairing/refusing it. Damaged executable-header bytes can prevent every subsequent launch until cache removal. OnceLock also returns the cached path on later calls without revalidation.

**Evidence:** D36 records embedded digests and whole-helper reuse; the user specifically requested digest verification before exec. The digests name the cache directory (`helpers.rs:158–163`) but do not validate its current content. Directory naming and atomic initial publication do not prove later bytes.

**Confidence:** high in incorrect acceptance; actual loader behavior unrun. This is integrity/recovery, with no unsupported cross-user escalation claim. **Fix direction:** validate reusable bytes against their expected digests, repair/refuse mismatches, and define integrity through execution. **Test:** extract, flip one byte preserving length, request the helper in another process and require exact repair or typed integrity refusal; include GPU.

**Status (2026-11-14):** fixed in 0122d69cc (red `a_helper_whose_bytes_changed_is_written_again_before_it_starts` and `a_changed_helper_that_cannot_be_written_again_is_refused`, by mutation restoring the length check, kiln r1785; green r1765 with launch tests and shards all-target clippy, on merge 22d03c1a, macOS arm64). Every helper start (VM, network, GPU; the daemon's kept path through `helpers::checked`) checks SHA-256 against build.rs's digest and rewrites or refuses. Logs `docs/research/measurements/runs-fixes/runs10-{red,green}.log`; D36 amended. Owed to ROOT: `env -u SHARDS_HELPERS -u SHARDS_HELPERS_DIR cargo test -p shards --bin shards helpers::tests`, a real run after flipping a cached helper's byte, Windows refusal with a running VM, and the per-start hashing cost (`docs/research/measurements/build-ab/ab.py`).

**Status (2026-11-14, ROOT's follow-up):** fixed in 8cdbf4a05 and e81b6da82 (red `a_helper_is_read_again_only_once_its_file_changed`, by mutation keying the version cache on the path alone, kiln r1904; green on macOS arm64 r1863, Linux x86_64 r1865 and Windows 11 x86_64, each with shards + shards-vmm all-target clippy). The bytes are read once per file version (`platform::file_identity`), the checked file is held through its start (Linux through `/proc/PID/fd/N`; Windows sharing only reads; macOS by name, no fexecve). Cost PM M580: 27 µs p50 a start with its version known, against 1.26 ms (shards-vm) and 1.70 ms (shards-net) p50 to read the bytes. Logs `docs/research/measurements/runs-fixes/runs10b-{red,green}.log`; D36 amended. Owed to ROOT: carried-helper unit tests (not skip), real VM starts through the held file on HVF/KVM/WHP, and `build-ab/ab.py` against 2e00a1043.

### runs-11 — P2 / medium — Runs still use explicitly arbitrary deadlines, retries and sizes

**At head:** `crates/shards/src/daemon.rs:146` (3 handoff tries), `:225` (10 s take timeout), `:236` (20 s takeover), `:252` (250 ms fallback probes); `daemon/slots.rs:29` (60 s sweep); `disk.rs:64` (20 MiB log segment).

**Defect:** operational retry/deadline/poll/chunk choices remain fixed without runtime learning or protocol/invariant derivation, contrary to the binding R03 instruction.

**Scenario/effect:** handoff reads at `daemon.rs:8966–8986` cut off a healthy VM delayed beyond the fixed take timeout, and `start_run` stops after three surely untaken VMs even if a healthy successor is available. Slot reclamation is delayed by the fixed sweep interval. The log segment's size remains Docker's copied local-driver unit. These are active choices, not unused constants.

**Evidence:** the brief quotes the person's 2026-11-13 R03 instruction. D185 at `architecture.md:8319` explicitly admits the fallback remains picked and owed; the production UNWATCHED comment says the same. D186 retains 20 MiB segments without deriving that unit. The handoff/takeover choices are inherited; fallback polling renames earlier RETRY behavior. This is a confirmed policy finding, not a live latency result. The list is a checked subset, not an exhaustive literal audit.

**Confidence:** high for this R03 violation; no performance claim. **Fix direction:** authoritative notifications and typed liveness/progress-based waits, with learned bounds where appropriate and explicit cold behavior; derive the segmentation choice from retained policy/data. **Test:** hold healthy actors past former constants and verify continued authorized progress; test cold/learned refusal and reclaimed-slot behavior. Do not merely replace these with different literals.

**Status (2026-11-14):** partly fixed. `HANDOFF_TRIES` is gone in 903fba0b5 and 66012636f (red `untaken_pooled_vms_give_way_to_each_other_and_a_fresh_one_ends_the_start`, by mutation restoring the count of three, kiln r1904; green `daemon::tests` 137/137 with shards + shards-vmm clippy, macOS arm64): a run tries each VM that waited in its pool before it asked, and stops after one started for it fails. Logs `docs/research/measurements/runs-fixes/runs11-{red,green}.log`; D-entry "Which VMs a run tries". Owed to ROOT, with designs in the runs-fixes record (no other number substituted): `TAKE_TIMEOUT` (delivery states plus D215 progress, liveness cold), `TAKEOVER` (the holder's identity in the lock file, followed to its end), `UNWATCHED` (typed refusal where no watch can be made; needs ROOT's interface ruling), `SWEEP` (the taker's process named in its slot, reclaimed on its end; a slots protocol change), `LOG_SEGMENT` (objective and measurement ROOT's to define first).

### runs-12 — P2 / medium — Windows readers lose the home's learned end record after its first write

**Found by** the runs takeover lane (Claude), 2026-11-14, while fixing runs-09. **At** `crates/shards/src/daemon/ends.rs` `Slowest::read`, introduced by runs-07's fix (45ae96862).

**Defect:** `heard` publishes through `platform::write_in`, which on Windows replaces no file (PM M345): it lands `ends.~N` and removes the plain name. `Slowest::read` read the plain name with `fs::read_to_string`, so every reader after the first write found no record: each Windows host's learned teardown, exit and helper maxima were lost to the next process, which fell back to liveness alone, and the next write lost earlier maxima the reader never saw. Every other `write_in` reader in the tree already opened the newest version; disk.rs's was fixed with runs-09.

**Status (2026-11-14):** fixed in aca3953c5 (`ends::newest`, open_newest_in; its test readers in netproc.rs, daemon.rs and containers.rs's E2E too). Red `each_published_record_is_read_back_by_the_next_process` by mutation restoring the plain-name read, on Windows 11 x86_64 (the container on adacachy); green there and on macOS arm64 and Linux x86_64, where it cannot be red (Unix has no versions). Logs `docs/research/measurements/runs-fixes/runs12-{red,green}.log`; D217 amended. Owed to ROOT: `cargo test -p shards --test containers a_kills_end_is_kept_as_this_hosts_in_its_home` on WHP.

## Checked and found sound

- Daemon warm VMs are removed from waiting pools by process-end handling. The RUN/TAKEN boundary retains client descriptors through acknowledgement (SCM_RIGHTS/M24), distinguishes definitely untaken from uncertain handoff, retries the former and avoids replaying the latter. Dead-ready selection races are accounted for in this daemon path; the new API pool finding is separate.
- Image-level requested-platform selection checks normalized matching, excludes attestations, and requires the requested manifest/config/layers rather than silently choosing a different architecture. The proven miss is container metadata/copy consumption of that choice.
- Bind/share access uses no-follow/beneath opens and directory-identity checks. Single-file bind canonicalization is intentional. No symlink escape was established in the examined paths.
- Ordinary VM/network helper initial publication stages privately, syncs files and renames the directory; the GPU path is the concurrency exception above. Helper locking explicitly unlocks before closing, addressing flock open-file inheritance (M244).
- Template settlement accepts only ready boots and removes failed saves on ordinary error returns. Origin records use a host-owned adjacent record and beneath/no-follow reads. The normal live API temporary-template GC deletion lead is refuted by its held lease; this is not a claim that cancellation cleanup is complete.
- Configurable GC 10%/80%/20% and 1 GiB log history are explicit user decisions in D186, so they are not flagged as arbitrary. Default stop grace is documented input semantics; binary restart backoff and cited smoothing have algorithmic justification. The learned end-record mechanism's concurrent persistence is flagged separately.
- The daemon's lock and stop-wait locks explicitly unlock before file close. Examined slot descriptors survive through the receiver's first acknowledgement.

## Not covered

- No runtime validation on macOS arm64, Linux glibc/musl amd64/arm64, or Windows amd64/arm64. No results from previous revisions are promoted to current passing gates. No comparative profiling, allocation/fault accounting, 4–5× claim, chaos, power-loss durability or full R01 panic/allocation audit was performed.
- The six branches integrated in plan steps 1–2 (engine-path, hex-upstream, runtime-image and goals #64/#65) are not certified as complete by this scoped read-only review. Shared launch/template/API ownership seams were examined; full external Hex integration and its cancellation caller behavior remain ROOT's work.
- Carrier-side D237 extraction/signing outside this checkout, exhaustive Engine/API authentication, every descriptor-bearing operation and every image command/platform combination are unverified.
- BootStop's watcher clears only host-created layer files (`image/session.rs:1659–1712`). It does not sweep unfinished `.new-<hostpid>-*` template directories after SIGKILL. The source suggests retained temporary snapshots, but retained size/lifetime and a full crash cleanup reproduction were not established; this is an unverified follow-up, not an additional counted finding.
- A possible concurrent `rmi --platform` versus new-container reservation race was not fully refuted and is not counted. Archive reuse of preexisting content-addressed blobs during load was also excluded as insufficient proof of a defect.

R01–R16 evidence disposition: R01/R04/R05/R06/R07/R13 were inspected only in the cited scopes; R03 breaches are reported; R02/R08/R09/R10/R11/R12/R14 broader research, live proof, platform/performance gates remain unrun. R15 roles/readbacks and read-only restrictions were followed; R16 required no cleanup or allocation of build storage. This report is a review handoff, not readiness, validation completion or merge approval.

## Delivery status

Attempted explicit delivery to ROOT / main Shards kiln agent `01a148884a4770fdaf0111f3f792a248`, verified listed as Shards at the g121 worktree. Full-report steer and shorter queued-summary delivery, each retried with its original immutable key/content, were rejected with HTTP 429: `{"code":"backpressure","pending":3,"authority":2}`. No acceptance receipt was returned. The report is retained here; do not infer model receipt from these attempts.

Main landing update (2026-11-13): ROOT confirms runs-05, runs-06 and the runs-01
native-owner follow-up landed through signed c6dd46525 (merge of fc1bb4797).

Main landing update (2026-11-13): ROOT confirms runs-07 landed through signed
02d3fe33b (merge of a68fc0ce4), own-file drift documentation only.
