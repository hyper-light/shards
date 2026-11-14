# Guest-memory adversarial review

Saved to this repository on 2026-11-13 at the user's request. Findings describe revision `2bc5f78f9`; they have not been revalidated against a later revision. The user will notify Shards.

POLICY_READ — role: **adversarial reviewer**, area **memory**. Coordinator receiving this review: ROOT, Claude g121, Kiln agent `01a148884a4770fdaf0111f3f792a248`. Reviewer: Codex, Kiln agent `01a1d7f0024b71b6892c38bca96281ec`.

- HEAD and origin/main: `2bc5f78f9e48d09ddbf4e848f9960133f6d19148`.
- Review range: `79caf2d74..2bc5f78f9`, plus the paths at HEAD.
- Worktree: `/Users/adalundhe/Projects/shards/.claude/worktrees/rv-memory` (detached).
- `AGENTS.md` SHA-256: `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41`.
- `docs/development-rules.md` SHA-256: `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
- `docs/development-evidence.md` SHA-256: `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63`.
- `CLAUDE.md` SHA-256: `588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010`.

The binding brief was read first, in full; all four files above were then read in full. Three read-only review helpers (DAX, reclaim, pmem) acknowledged the same revision and all four hashes before inspecting their scopes. The main reviewer re-read the reported defect paths. No source edits, builds, tests, policy script, daemons, VMs, benchmarks, profiling, or live reproduction were performed. The initial review wrote only the temporary Markdown report; this repository copy is now saved at the user's explicit request. No production code was changed. Test sketches below are proposals for ROOT, not executed results.

Priority mapping: P0 critical; P1 high memory-safety defects; P2 high functional defects; P3 medium; P4/P5 low. Totals: **17 findings: 1 critical, 4 high, 10 medium, 2 low**. Findings 01–05 concern behavior and ownership; 06–17 are separately identified violations of the brief's explicit R03 rule, not claims of measured performance regressions. For those, the concrete wrong result is a fixed production policy where a protocol bound or learning on the running host was required. Existing measurements are acknowledged; they do not satisfy the brief's stronger requirement.

## Findings, ranked P0–P5

### memory-01 — P0 / critical — Windows DAX can silently replace in-bounds writable file data with anonymous zeros

**Location:** `crates/vmm/src/devices/virtio/fs/window_whp.rs:167`, with destructive effect at `:836–863`.

**Defect:** The window retains a node's first read-only file handle even after receiving a writable handle, then turns writable file mappings into anonymous memory when remapping through the retained handle fails.

**Scenario:** On Windows, create a writable 3 MiB file filled with `0x5a`. Keep a READ mapping of its first 2 MiB, opened through an O_RDONLY handle. Map its second 2 MiB window range WRITE through an O_RDWR handle; the first MiB of that range is existing file data. Grow the file to 4 MiB through a host handle and trigger reconciliation (for example, the guest's next size-bearing FUSE reply). The writable entry's file extent grows from 1 MiB to 2 MiB. Reconciliation frees its old mapping, tries to create a PAGE_READWRITE section from the first, read-only handle, fails, and places writable anonymous zeros over the whole range. Existing `0x5a` bytes now read zero, and subsequent guest stores no longer reach the file. This is silent loss of file-backed write semantics and persistence, not merely a refused operation.

**Evidence:** Table::insert does `if node.file.is_none() { node.file = file; }` (:167–169), with no access upgrade. Server::descriptor actually returns O_RDONLY for READ on Windows (`fs/server.rs:1291–1298`; SHARED_NEEDS_WRITE is false there). Reconcile uses the retained file and the entry's writable flag in `section_of(f, writable)` (:836–841), then `let to = if section.is_some() { to } else { 0 };` (:842) and `part_to(to, i)` to install zeros. PM M412 (`docs/research/platform-measurements.md:17092`) explicitly records that PAGE_READWRITE sections of read-only handles are refused, and that growing a file with mapped views succeeds. D189 promises shared file data and file-backed mappings after growth. The Unix twin's node tracks writable access and upgrades its retained descriptor; this is missing in WHP.

**Refutation checked:** The second mapping receives a genuinely writable handle, but insert drops it because the node already has a handle. Retaining the first entry keeps that node alive. Growth is allowed by NTFS; no truncation-with-open-view issue is required. This does not allege that read-only volumes are writable.

**Confidence:** High, static call-path proof plus existing platform evidence; not reproduced live.

**Fix direction:** Upgrade the retained handle when writable access arrives. Preserve explicit mapping failure state and return/propagate failure rather than representing failed in-bounds data as EOF zeros.

**Test sketch:** Extend Windows window tests with READ then WRITE ranges of the same node using distinct RO/RW handles. Grow the file, reconcile, assert the old data at window offset `2 MiB + 4096` remains `0x5a`, write `0xa5` there, flush, and assert the host file contains `0xa5`. The existing file-window test supplies RW handles for every map and misses this interleaving.

**Status (2026-11-14):** fixed in 7f0cb8ce9 (red `devices::virtio::fs::window::tests::a_range_mapped_to_be_written_after_one_to_be_read_stays_its_files` and `a_range_no_view_of_its_file_can_be_made_for_is_refused_never_zeros`, 4697f1e7b: zeros read where the file holds 0x5a on Windows 10.0.26200 x86_64; PM M573, `docs/research/measurements/windows-p0/`).

### memory-02 — P1 / high — Live copying races virtio-mem unplug and reads inaccessible memory

**Location:** `crates/vmm/src/memory.rs:739–741` and `:765–768`; caller `crates/vmm/src/vm/runtime.rs:549`.

**Defect:** Live-copy readers use plugged-memory pointers and borrowed slices without holding the Access guard that protects them against unplug.

**Scenario:** A VM has a plugged block B. Live precopy captures B in `h.runs(&h.plugged)` or obtains its pointer through page_at. Before is_zero or the stream writer consumes B, the guest legitimately frees/offlines B and virtio-mem processes its UNPLUG. Access::unplug withdraws the guest mapping and replaces the host mapping with PROT_NONE on Unix (decommits it on Windows). The live-copy thread then dereferences B and faults the VM process. No malicious out-of-range GPA is needed; host-pressure reclaim during a live move suffices.

**Evidence:** each_used calls `runs(h.host(gpa), len, gpa, true)` after a separate atomic-bitmap scan (:739–741). runs dereferences pages at :714 and constructs slices at :723. page_at calls host_ptr and returns a slice after the check (:765–768), with no lifetime protection. Runtime invokes each_used while the VM runs, before the pause at :559; subsequent send_pages serializes page_at's slice at :632. Access::unplug clears the bits at memory.rs:1137 and withholds pages at :1146; platform/unix.rs:255–273 maps PROT_NONE over them. Only Access owns the host-memory mutex; neither live reader takes it. D133's invariant says no host thread can be between finding a block plugged and touching it.

**Refutation checked:** A VM Arc retains the reservation object, not the protection or backing of each block. Dirty logging does not pin it. The first-round pause occurs after RAM copying. warm.rs:1020–1055 starts precopy on its own thread while guest/relay work continues; vm_run.rs:907–917 adds no guard.

**Confidence:** High static interleaving; not live reproduced.

**Fix direction:** Copy each bounded chunk under Access (or an equivalent mapping-lifetime guard) into owned memory, release the guard before potentially blocking stream writes, and include unplug/replug transitions in live migration state. Do not return borrowed slices whose mappings can change independently.

**Test sketch:** Gate the reader immediately after collecting a plugged range/pointer; complete UNPLUG from another thread; release the gate. Require a safely skipped/retried chunk or a consistent owned copy, with no invalid dereference. Exercise both each_used and page_at on each OS and verify final migration handles replugged/unplugged state.

**Status (2026-11-13):** fixed in 29196c6b3 (red `live_copy_keeps_bytes_after_their_block_is_unplugged` and `live_copy_page_keeps_bytes_after_their_block_is_unplugged`, SIGBUS at 04ae8a9cd; focused green r7). Evidence: `docs/research/measurements/memory-fixes-v1/README.md`.

### memory-03 — P1 / high — Stopping a VM during live precopy can unmap a layer underneath a read

**Location:** `crates/vmm/src/devices/virtio/pmem.rs:464–469`; teardown `crates/vmm/src/vm/runtime.rs:1182`.

**Defect:** Shared::read checks an atomic released flag and then copies without holding the mapping's release lock, while precopy is absent from the lifecycle guard awaited by teardown.

**Scenario:** Live precopy is reading the VM's writable layer. Shared::read observes released=false. A simultaneous normal guest exit or host stop runs teardown, whose await_save sees no ordinary save in progress. Teardown explicitly calls layer.release and unmaps the file. The read resumes at copy_in on the old address, faulting or reading memory newly placed there. This is an actual use-after-unmap opportunity, although no cross-VM disclosure is demonstrated here.

**Evidence:** Shared::read checks `self.released()` at :464 and performs raw copy_in at :469. Shared::release flips released, locks file, and unmaps (:584–589); the reader never takes that mutex. Handle::save marks `sh.saving`, but Handle::precopy (:498–531) does not. await_save (:1015–1020) waits only on that saving flag. precopy resumes before reading layer data (:566–586). Teardown drops its VM Arc and calls layer.release (:1177–1182) regardless of the Arc retained by precopy.

**Refutation checked:** Keeping an Arc<Layer> or Arc<hv::Vm> cannot prevent the explicit release method from unmapping. The ordinary save guard does not cover precopy. Atomic visibility does not protect the interval between the check and the copy.

**Confidence:** High static interleaving; not live reproduced. Severity is high rather than an asserted isolation escape.

**Fix direction:** Hold mapping ownership across read operations and make active precopy a typed lifecycle operation that teardown cancels/joins or waits for before releasing mapped files and the hypervisor.

**Test sketch:** Gate a read after its released check, stop the VM, then resume the read. Require teardown to wait or copying to return a typed stopped error before unmapping. Observe actual mapping release and VM destruction order; repeat for run swap as well as layer mappings.

**Status (2026-11-13):** fixed in e5e2ef793 (red `a_live_layer_read_waits_for_the_mapping_owner`, r13). Teardown also waits for typed save/precopy reader ownership. Actual stop-during-stream VM coverage is owed to ROOT; see `docs/research/measurements/memory-fixes-v1/README.md`.

### memory-04 — P2 / high — Removing a builder's budget can permanently leave its old guest cap installed

**Location:** `crates/shards/src/build/give.rs:334`; dropped send `crates/shards/src/build/builder.rs:199`.

**Defect:** The budget watcher records a ROOM change as told even when the builder is busy and receives no frame, and never retries a dropped removal.

**Scenario:** A builder has acknowledged ROOM(64 MiB). A GIVE_BACK reply is delayed past the pressure pass. The user removes memory.high/memory.max while this reply is outstanding. budget_written(room=None) calls tell, which records unsaid.told=None, but Giver::send returns Ok(false), transmitting nothing because out is still set. The late GAVE then arrives. late only tells a room when room() is Some; again immediately returns when unsaid.told is None. With no outstanding reply or recorded budget, watch_words blocks indefinitely. The guest's old CAP and memory.high remain even though the host budget was removed, throttling future build allocations.

**Evidence:** give.rs:230–234 sends the removal through tell; tell assigns unsaid.told before dispatch (:334–340). builder.rs:198–204 explicitly returns false when another ask is out. late only resends when `gave && let Some(r) = room()` (give.rs:294–295); again uses `let was = unsaid.told?` (:317); watcher chooses blocking recv at :183–193 once both conditions clear. Guest cap changes only when ROOM invokes hold_within (`init/plug.rs:633–646`). The supported late-reply case is documented by PM M350; give.rs:218–221 explicitly promises a removed budget is told as none.

**Refutation checked:** Later unbudgeted pressure passes also omit ROOM(None) (:255–256). A late HELD likewise has no retry. This does not depend on a failed socket write: busy is a successful false return intentionally ignored by the bookkeeping.

**Confidence:** High, deterministic state-machine proof; not executed.

**Fix direction:** Keep desired and acknowledged budget generations per Giver, including explicit unbudgeted state. Retry pending changes after any prior response and update applied state only after transmission/acknowledgment.

**Test sketch:** Using give.rs's builder()/gave() helpers, acknowledge an initial ROOM, start GIVE_BACK without answering, call budget_written with None, then supply GAVE and call late. Assert the next peer frame is ROOM with empty payload. It is currently absent. Repeat with a pending HELD and multiple builders, only one busy.

**Status (2026-11-13):** fixed in 27522e6ca with typed pending state aef83d2c4 and acknowledgment completion 8951ffa2d (red `a_budget_removed_during_an_ask_is_delivered_after_its_answer`, resumed r2). It retains and transmits removal after either pending reply; evidence is in `docs/research/measurements/memory-fixes-v1/README.md`.

### memory-05 — P2 / high — APFS live-copy failure resumes a VM whose layer mapping is lost

**Location:** `crates/vmm/src/vm/runtime.rs:566–569`.

**Defect:** The live-copy initial scan resumes the VM before checking whether obtaining layer extents irreversibly lost its mapping.

**Scenario:** On macOS/APFS, precopy pauses the VM to find writable-layer extents. The layer is unmapped from stage 2 and replaced in the host while checking holes. Mapping it back fails (host mmap or hypervisor mapping failure). Shared marks lost and returns Err. precopy unconditionally resumes the VM, then propagates the error to the caller. Its guest now runs with the layer's required mapping missing. The ordinary save path would stop the VM instead.

**Evidence:** Layer::data routes through Shared::data_ranges (`pmem.rs:959–979`). With MAPPED_HIDES_HOLES, it unmaps/withholds (:531–537), and remap failure calls lose (:543–546), which sets lost (:551–553). precopy_rounds calls self.resume at runtime.rs:567 and only then evaluates found.0? / found.1? (:568). No lost check occurs in this error path; precopy merely unwatches on error. In contrast, save_parked explicitly checks control.lost and stops (:749–756). D190/M285 treat losing a withdrawn mapping as terminal.

**Refutation checked:** Normal VM save/template paths contain the guard; they are different callers. Error reporting to the host does not restore the mapping. This requires injected/resource failure, not the normal successful APFS map-back path.

**Confidence:** High for error-path control flow; actual mmap/hypervisor fault injection unrun.

**Fix direction:** Inspect mapping-loss state while paused, stop on irreversible loss, and resume only a recoverable failure whose mappings are intact. Share the ordinary save's terminal-outcome handling.

**Test sketch:** Inject map_file_shared_at failure, then stage-2 mapper failure, during initial precopy extent discovery. Assert no resume occurs after lost=true and the VM reports a terminal error with cleanup, rather than continuing.

**Status (2026-11-13):** fixed in fb58160ac (red `a_live_copy_whose_mapping_was_lost_stops_instead_of_resuming`, r14). Unit continuation proof; actual APFS/map-back fault injection is owed to ROOT. Evidence: `docs/research/measurements/memory-fixes-v1/README.md`.

### memory-06 — P3 / medium — A fixed two-second policy still controls pressure, budget retries, and recent-growth protection

**Location:** `crates/abi/src/lib.rs:74`; `crates/vmm/src/platform/pressure.rs:37`.

**Defect:** A chosen two-second window is reused as production memory-control policy instead of being learned where the workload runs.

**Scenario/evidence:** PRESSURE_WINDOW_US is exactly 2,000,000; PRESSURE_WINDOW carries it into daemon budget retry waits, builder PATIENCE/rechecks, and init's recent-growth protection. macOS/Windows also set REPEAT=2 seconds. A slow or fast host with no learned history receives the same repeat interval and growth exclusion. The Linux PSI minimum supports that trigger's minimum legal window, but does not require two-second repeats on macOS/Windows or establish the workload's recent-growth time. D188 cites inherited lmkd timing; PM M466 motivates the protection but does not make it runtime learning. This violates the brief's R03 instruction covering every fixed timeout.

**Confidence:** High policy violation; no performance/2x-goal failure measured.

**Fix/test direction:** Keep only required OS trigger bounds in the adapter; derive caller pacing and growth protection from observed completion/progress, with explicit cold-start behavior. Test different completion scales and short pressure pulses, showing the non-Linux control policy changes with observations rather than this literal.

**Status (2026-11-13):** owed to ROOT: `cargo test -p shards moves_live --test moved` and the corresponding pressure/cold-start variants. Unchanged pending a locally derived design; see finding 06 in `docs/research/measurements/memory-fixes-v1/owed-designs.md`.

**Status (2026-11-14):** fixed in 2275e3829 (red, as each guard mutated back to its window: `pressure::saying::a_pressure_is_said_as_it_doubles`, `daemon::budget::tests::a_microvm_held_short_is_told_again_as_telling_it_weighs_against_how_long_it_was_short`, `daemon::groups::tests::the_groups_passes_follow_their_cost_against_how_long_groups_stayed_as_they_are`, `build::give::tests::a_cold_pass_judges_no_ask_and_its_first_answer_teaches_its_patience`, `platform::pressure::tests::a_lasting_pressure_is_said_again_as_its_act_weighs_against_how_long_it_lasted`, init `plug::tests::memory_that_came_within_a_peaks_period_is_kept`). The 2 s stays only as Linux's unprivileged trigger bound; D188, PM M581. Owed to ROOT: `cargo test -p shards moves_live --test moved`, `update_under_pressure_stress` (tests/containers.rs, ignored) for PM M466's rounds, macOS/Windows repeat counts under a raised system pressure.

### memory-07 — P3 / medium — PSI's stall threshold and fallback percentage are inherited constants

**Location:** `crates/vmm/src/platform/pressure.rs:22` and `:32`.

**Defect:** STALL_US=140,000 and AVERAGE_PERCENT=7.0 choose reclaim authority without learning from the running host.

**Scenario/evidence:** arm writes `some 140000 2000000`; fallback triggers only when avg10 reaches 7%. Below those chosen levels, a pressured workload receives no give-back request, independent of its observed memory/reclaim costs. Comments/D188 attribute them to Android lmkd's lowest default (70 ms per second), not a PSI protocol threshold. The brief explicitly says an upstream default is insufficient. PSI's supported minimum window does not prescribe a 7% threshold.

**Confidence:** High policy violation; no claim that every lower stall must cause reclaim.

**Fix/test direction:** Derive the trigger level using the host's observed pressure and reclamation outcomes within authority; retain typed unavailable-data/cold-start states. Feed multiple pressure/reclaim histories and assert the chosen trigger is derived, not always this pair of values.

**Status (2026-11-13):** owed to ROOT: `cargo test -p shards --test budget` and the corresponding pressure/cold-start variants. Unchanged pending a locally derived design; see finding 07 in `docs/research/measurements/memory-fixes-v1/owed-designs.md`.

**Status (2026-11-14):** fixed in 2275e3829 (red `platform::pressure::tests::a_windows_stall_is_pressure_once_it_costs_what_acting_does` with lmkd's 140 ms put back): the stall is the last give-back's cost (rent or buy), cold the kernel's least threshold; D188, PM M581. Owed to ROOT: `cargo test -p shards --test budget` on a loaded Linux host with cgroup v2 delegation, the trigger's word count and give-backs against lmkd's 140 ms.

### memory-08 — P3 / medium — PSI fallback polls at a chosen ten-second interval

**Location:** `crates/vmm/src/platform/pressure.rs:34`, used at `:147–148`.

**Defect:** A host that cannot arm any PSI trigger sleeps a fixed ten seconds between checks.

**Scenario/evidence:** For a host with no armed trigger, wait sleeps AVERAGE_EVERY before reading avg10. Pressure starts immediately after a check; the caller hears nothing until the fixed sleep ends. avg10 is a kernel statistic's averaging interval, not a requirement to sample only every ten seconds. The ten-second constant is neither learned nor a protocol deadline. Brief R03 explicitly rejects fixed timeouts; D188 documents the same behavior but supplies no runtime derivation.

**Confidence:** High policy violation and static notification delay; no live latency measurement.

**Fix/test direction:** Use available event notification or learned progress/observation pacing with an explicit degraded state. Simulate trigger unavailability and short pressure transitions, and observe notification latency under different learned schedules.

**Status (2026-11-13):** owed to ROOT: `cargo test -p shards --test budget` and the corresponding pressure/cold-start variants. Unchanged pending a locally derived design; see finding 08 in `docs/research/measurements/memory-fixes-v1/owed-designs.md`.

**Status (2026-11-14):** fixed in 2275e3829 (red `platform::pressure::tests::a_psi_files_stall_so_far_is_its_some_total` with avg10 put back): no trigger, the same trigger kept from `some total` read at once and each window; D188, PM M581. Owed to ROOT: the fallback on a kernel before 6.4 (or a refused trigger) with short pressure pulses.

### memory-09 — P3 / medium — DAX's minimum file size is a portable fixed benchmark result

**Location:** `crates/vmm/src/devices/virtio/fs/server.rs:105`, applied at `:1266`.

**Defect:** DAX_SMALLEST=1 MiB turns selected Mac/Fedora measurements into a fixed policy on every host.

**Scenario/evidence:** A 512 KiB file is unconditionally excluded even on the measured Mac/efficient cores where the documented crossover was 256 KiB. Windows and Linux arm64 inherit the same policy without crossover measurements. The literal's comment (:96–104), D189 and PM M338 explicitly derive the common cutoff from experiments on those particular hosts; it is not FUSE's required mapping alignment/size bound. Under the binding brief, a benchmark value is not a value learned where it runs. This is a policy finding about goal #13, not a claim that 1 MiB was unsupported by those historical experiments.

**Confidence:** High policy violation.

**Fix/test direction:** Learn or derive the crossover on the deployment host, accounting for cold read, warm reread, overwrite, memory and core scheduling. Verify the selection responds to measured cost curves and has a documented cold-start state, rather than asserting a global 1 MiB threshold.

**Status (2026-11-13):** owed to ROOT: `cargo test -p shards --test budget` and the corresponding pressure/cold-start variants. Unchanged pending a locally derived design; see finding 09 in `docs/research/measurements/memory-fixes-v1/owed-designs.md`.

**Status (2026-11-14):** owed to ROOT: design: the share learns the crossover where it runs. Its own READ handling time per byte and per request (served by the server) is set against SETUPMAPPING's handling time plus the guest's fault cost per mapped page, by file-size class, and the least size whose DAX cost falls under FUSE's becomes the cutoff. The uncertainty is held as the classes' spread. Cold start: FUSE, which works everywhere, with a typed `Calibrating` state that maps a share of eligible inodes to sample DAX. A calibration that fails or is unsupported stays FUSE with its reason. Existing inodes keep `fuse_dax_dontcache`'s lifecycle, and no measurement changes write permission. The guest fault cost cannot be seen by the VMM, so the measurement owed is D189's file-size/working-tree harness (PM M338's) on HVF, KVM x86_64 and WHP, plus KVM arm64 through the L1 path: per class, cold read, warm reread and overwrite, and resident memory, n ≥ 20 per class with p50/p90/p99/max, against the learner's chosen cutoff. Also `cargo test -p shards --test containers` (the DAX_SMALLEST tests) once the learner lands.

### memory-10 — P3 / medium — FUSE budget refresh uses an arbitrary 100x coefficient

**Location:** `crates/vmm/src/devices/virtio/fs/headroom.rs:21`, used at `:91`.

**Defect:** LOOKS_APART=100 turns measured observation cost into a refresh delay using an unexplained fixed policy coefficient.

**Scenario/evidence:** After one room observation taking T, fits continues to spend cached room until 100T passes, even if another process reduces the budget in that interval. Changing the running workload's cost or risk never changes the hundredfold coefficient. The comment supplies only the desired one-percent observation share, not a protocol bound or runtime-derived target. Measuring T does not derive 100. R03 and the brief specifically cover fixed multipliers.

**Confidence:** High policy violation; no quantified budget overrun reproduced.

**Fix/test direction:** Derive refresh policy from observed room-change and observation costs or use budget-change notifications. Test a budget reduction immediately after observation with variable read costs and demonstrate bounded staleness from the chosen method's evidence.

**Status (2026-11-13):** owed to ROOT: `cargo test -p shards --test budget` and the corresponding pressure/cold-start variants. Unchanged pending a locally derived design; see finding 10 in `docs/research/measurements/memory-fixes-v1/owed-designs.md`.

**Status (2026-11-14):** fixed in 2275e3829 (red `devices::virtio::fs::headroom::tests::a_look_is_due_as_its_cost_weighs_against_how_long_its_verdicts_held` with the hundredfold rule put back): D215's pace of the look's cost and how long its verdicts held; D188, PM M581. Owed to ROOT: `cargo test -p shards --test budget a_microvm_held_to_a_budget_reads_through_fuse_at_the_pace_of_reads_held_to_none` and the share's look count and CPU against the hundredfold rule's (R14).

### memory-11 — P3 / medium — Builder memory reserve still includes a fixed 64 MiB slack

**Location:** `crates/init/src/plug.rs:36`, consumed at `:1053`.

**Defect:** SLACK=64 MiB remains a host-independent reserve floor beside the build's cgroup.

**Scenario/evidence:** reserve(total,left) always adds 64 MiB regardless of observed kernel overhead, concurrency, allocator overshoot or cold-start uncertainty. A guest whose actual uncharged need differs gets the same floor. The comment cites PM M193's workload, while the independently derived page-metadata and per-CPU-list terms are computed separately. D133 repeats the chosen reserve; it does not turn it into a physical/protocol bound. The current brief rejects a benchmark number as a runtime policy constant.

**Confidence:** High policy violation; actual insufficient/excessive reserve magnitude unmeasured here.

**Fix/test direction:** Derive slack from observed uncharged kernel/overshoot cost with cold-start uncertainty, while preserving real pinned-kernel invariants. Exercise distinct CPU counts and cold growth workloads, observing reserve adequacy and cost rather than asserting 64 MiB.

**Status (2026-11-13):** owed to ROOT: `cargo test -p shards --test build a_builders_memory_is_plugged_as_fast_as_its_jobs_take_it` and the corresponding pressure/cold-start variants. Unchanged pending a locally derived design; see finding 11 in `docs/research/measurements/memory-fixes-v1/owed-designs.md`.

**Status (2026-11-14):** owed to ROOT: design (derived, not landed): SLACK becomes two terms. (a) The kernel's own bound on how far a cgroup charges past `memory.high` before its synchronous reclaim: per online CPU, two charges of max(MEMCG_CHARGE_BATCH = 64 pages, the largest folio a charge takes, `hpage_pmd_size` where THP is on). The source is mm/memcontrol.c `try_charge_memcg` (`memcg_nr_pages_over_high > MEMCG_CHARGE_BATCH`) and include/linux/pagemap.h `MAX_PAGECACHE_ORDER`, read in 7.2-rc4. (b) The kernel's uncharged growth beside the build, learned by the grower at each `memory.events` wake as the largest rise of `MemTotal − MemFree − memory.current` between wakes, less the memmap of blocks plugged between them. Cold start: (a) alone plus the kernel's uncharged use at the hold (its use so far standing for what it adds). Failure: unread files keep the last reserve, never zero. Measurement owed: `cargo test -p shards --test build a_builders_memory_is_plugged_as_fast_as_its_jobs_take_it` and PM M392's 8-compiler build on 32 vCPUs, with the guest's `memory.peak − memory.high` overshoot and the uncharged rise logged at each wake, on HVF and KVM at 4, 16 and 32 vCPUs. The reserve's terms must cover each observed maximum.

### memory-12 — P3 / medium — One-millisecond pacing and read-timeout floors remain chosen constants

**Location:** `crates/init/src/plug.rs:113` and `:1350`; `crates/shards/src/build/builder.rs:251`.

**Defect:** The grower polls/scores time in fixed 1 ms units, and a builder reply reader imposes the same fixed timeout floor even after its deadline.

**Scenario/evidence:** Grower pause sleeps PAUSE=1 ms (:1290–1291) and counts at most that much time per look (:1350), independent of measured observation time. Giver::answer sets `left.max(Duration::from_millis(1))`, so an already-expired read still waits a chosen millisecond. These are distinct retained uses of the same unlearned timing choice, not an OS protocol deadline. A slow shared host and a fast idle one receive the same floors; adding a learned slowest plug duration does not derive them.

**Confidence:** High policy violation; actual scheduling distributions unrun.

**Fix/test direction:** Use readiness/completion events and the existing learned waiting mechanism. At an expired deadline do a nonblocking availability check; test it does not install a fixed-duration blocking floor. Verify grower progress/stall judgment under varying scheduler holds without a fixed millisecond clock.

**Status (2026-11-13):** owed to ROOT: `cargo test -p shards-init plug::tests::` and the corresponding pressure/cold-start variants. Unchanged pending a locally derived design; see finding 12 in `docs/research/measurements/memory-fixes-v1/owed-designs.md`.

**Status (2026-11-14):** fixed in 2275e3829 (red `build::builder::tests::a_giver_past_its_deadline_looks_at_once` with the 1 ms floor put back, 200 looks over 100 ms; red `plug::tests::the_growers_looks_follow_their_cost_against_the_wait` with a 1 ms pace): grower and oom_said on D215's pace, the reply reader looks at once past its deadline; D137, PM M581. Owed to ROOT: `cargo test -p shards --test build a_builders_memory_is_plugged_as_fast_as_its_jobs_take_it` on HVF and KVM.

### memory-13 — P3 / medium — Hotplug still gives up after exactly one retry

**Location:** `crates/init/src/plug.rs:1327–1346`.

**Defect:** The learned block interval is paired with a fixed two-attempt retry policy.

**Scenario/evidence:** After a prior successful block establishes slowest=S, a later block is delayed across two S-sized active observation windows. come retries once, then `if again { break; }` returns Stalled even if the driver remains able to finish. The delay duration is learned but retry count is hard-coded in a boolean. The comment at :1303 explicitly says past it again, stops; neither virtio-mem nor D133 specifies that count. This is a direct violation of the brief's fixed-retry prohibition.

**Confidence:** High for transition and R03; no real delayed-driver reproduction.

**Fix/test direction:** Judge actual driver/VM progress and typed inability, with explicit cold-start/held states, rather than a fixed attempt budget. Extend the existing fake Grows cases with a third-window successful completion and a genuine terminal failure so the two are distinguished by evidence.

**Status (2026-11-13):** owed to ROOT: `cargo test -p shards-init plug::tests::` and the corresponding pressure/cold-start variants. Unchanged pending a locally derived design; see finding 13 in `docs/research/measurements/memory-fixes-v1/owed-designs.md`.

**Status (2026-11-14):** fixed in 2275e3829 (red `plug::tests::memory_that_comes_after_a_third_ask_is_waited_for` with a give-up at the second window put back): asked again each window past W*, no count; ended typed by the driver's own "marking device broken" (`a_driver_that_stopped_for_good_is_given_up`), a refused ask, a lowered request or MemTotal unread; D137, PM M581. Owed to ROOT: a delayed-driver fault live (the device answering BUSY three windows) on a real builder.

### memory-14 — P3 / medium — Growth and unplug retention use a fixed 2x working-memory multiplier

**Location:** `crates/init/src/plug.rs:798` and `:1562`.

**Defect:** The guest chooses both growth requests and retained plugged memory by multiplying current usage/size by two.

**Scenario/evidence:** to_keep retains twice used (bounded by peak/ceiling); grow asks for twice total. Thus workloads with different observed allocation bursts, plug cost and host pressure get the same expansion/retention ratio. D133 explains logarithmic requests and bounded speculative memory; that explains the family of geometric policies but does not derive factor 2 over alternatives or learn it on the host. It is a policy multiplier, not a memory block's physical bound. PM M275 justifies peak protection, which is a separate max operand.

**Confidence:** High policy violation; no claim that geometric growth itself is wrong.

**Fix/test direction:** Derive requested headroom and retained reserve from observed burst/plug/reclaim behavior and costs, with explicit cold-start treatment. Test bursty and steady traces and pressure transitions, preserving peak and alignment invariants while allowing the chosen headroom to differ.

**Status (2026-11-13):** owed to ROOT: `cargo test -p shards --test build a_build_held_short_of_memory_has_its_builder_give_back` and the corresponding pressure/cold-start variants. Unchanged pending a locally derived design; see finding 14 in `docs/research/measurements/memory-fixes-v1/owed-designs.md`.

**Status (2026-11-14):** owed to ROOT: design (derived, not landed): both the grower's ask and a give-back's keep rest on the run's own use (`memory.current`), not on MemTotal for one and use for the other. Each holds the least whole blocks whose hold (`needed_for`) keeps twice the use. The factor is the delta-t median estimator (Gott, Nature 363, 1993: what a run has grown so far stands for what it grows more), the estimator D215's `w` already uses. That makes it an estimator, not a tuning knob, and its cold start is the same. A factor learned from realized peaks is biased low, because the hold caps what a run can show (censored), so it is not learned naively. Peak protection and ALIGN rounding are kept. Measurement owed, to validate the median: realized episode ratios (final peak over use at the first ask of a growth episode) on real builds and runs, `cargo test -p shards --test build a_build_held_short_of_memory_has_its_builder_give_back` and `a_builders_memory_is_plugged_as_its_build_needs_it` on HVF and KVM. Ask counts and stall per build, before and after, n ≥ 20, with p50/p90/p99/max.

### memory-15 — P3 / medium — Builders always boot with a chosen 512 MiB floor

**Location:** `crates/shards/src/build/builder.rs:51`, used at `:727`; fallback/floor `crates/shards/src/build/mod.rs:819–826`.

**Defect:** BOOT_MEMORY_MIB=512 fixes the cold boot memory and minimum builder ceiling independently of the running environment.

**Scenario/evidence:** A home with no recorded build history still boots at min(ceiling,512 MiB), and builder_most uses 512 MiB as a fallback/minimum. D133/PM M82/M193 motivate avoiding the page-structure cost of booting all available RAM, but no protocol requires 512 MiB, and the value is not learned from this host/kernel's startup need. The current brief explicitly includes chosen sizes and floors.

**Confidence:** High policy violation; minimum safe boot memory not measured here.

**Fix/test direction:** Derive the boot requirement from the pinned guest's actual startup resource need and observed cold starts, account for uncertainty, and keep unavailable-data state explicit. Exercise startup on different supported architectures/CPU counts and verify the selected amount has evidence beyond the literal.

**Status (2026-11-13):** owed to ROOT: `cargo test -p shards --test build a_builders_memory_is_plugged_as_its_build_needs_it` and the corresponding pressure/cold-start variants. Unchanged pending a locally derived design; see finding 15 in `docs/research/measurements/memory-fixes-v1/owed-designs.md`.

**Status (2026-11-14):** owed to ROOT: design (not landed): the builder's boot memory is learned per home and per kernel/init identity. As its grower starts, init records the guest's startup need (MemTotal − MemFree at its first hold, less the memmap of its boot memory, a 64th) beside `builder-memory`'s NEEDED. The next boot takes the least whole blocks holding that need plus the reserve (memory-11) and the first growth's working room. Cold start, with no record for this identity: boot with the ceiling, all it may have, which always boots; it costs one cold boot's page structures (a 64th of the ceiling, PM M82/M193). Never a guessed floor. Host memory unreadable is a typed refusal unless a record exists. Measurement owed: cold-home and warm-home builder boot time and VM-process footprint on HVF, KVM x86_64, KVM arm64 (L1) and WHP, at 4 and 32 vCPUs, n ≥ 20 each with p50/p90/p99/max, for the ceiling cold start and the learned boot. Also `cargo test -p shards --test build a_builders_memory_is_plugged_as_its_build_needs_it` across cold homes.

### memory-16 — P4 / low — Head deduplication stops considering new kinds after 64

**Status (2026-11-13):** fixed in d794c1dd5 (red `a_heads_duplicate_after_the_sixty_fourth_kind_is_kept_once` at 5ede7aecb). Focused results and remaining performance validation: `docs/research/measurements/memory-fixes-v1/README.md`.

**Location:** `crates/vmm/src/devices/virtio/head.rs:696`, used at `:749`.

**Defect:** An unexplained comparison cap causes equal head blocks beyond the first 64 kinds to be stored separately.

**Scenario/evidence:** Provide 66 nonzero 2 MiB blocks: first 65 pairwise distinct, last equal to block 65. save_head compares only `kept.iter().take(HEAD_KINDS)`. It never compares the final block with kind 65, so both are retained under separate indices and file offsets rather than sharing one kept block. No data corruption follows, but disk/page-cache sharing is lost. The rationale is only “a head's alike blocks are a few kinds”; D190/PM entries supply no bound of 64 on possible patterns. R03 forbids this chosen resource/time tradeoff without derivation.

**Confidence:** High static result; workload incidence and cost unmeasured.

**Fix/test direction:** Deduplicate by content identity (with equality confirmation), or derive a bounded method from measured resource cost. Construct the 66-block input and assert the last two identical contents have the same kept index. Do not turn this sketch into an unconditional heavy test on the shared Mac.

### memory-17 — P5 / low — The builder give-back parser's 4096-byte frame cap is not derived from its protocol

**Status (2026-11-13):** fixed in 5826a9bcf (red `a_give_back_room_past_signed_decimal_ends_before_reading_payload` at c61cc6b1d). The bound is the longest canonical signed `i64` decimal request; signed extremes stay accepted. Evidence: `docs/research/measurements/memory-fixes-v1/README.md`.

**Location:** `crates/init/src/plug.rs:877`, enforced at `:900`.

**Defect:** GIVE_FRAME chooses a 4096-byte allocation/refusal boundary although this protocol carries an empty request or a signed integer.

**Scenario/evidence:** An otherwise framed request with payload length 4096 passes the allocation gate, while 4097 ends the connection; those values have no relation to an empty GIVE_BACK or ROOM's decimal i64 representation. The comment explains why a cap is needed, not why this cap is correct. Keeping a finite protocol bound is sound; choosing 4096 without deriving it violates the brief's R03. No security exploit is asserted for this bounded allocation.

**Confidence:** High policy violation; low operational impact.

**Fix/test direction:** Derive a per-message bound from the wire grammar (empty or the signed-integer textual maximum), and give malformed frames typed refusal semantics. Test exact valid maxima and one byte past each grammar's bound.

### memory-18 — P2 / high — Refused FUSE replies retain their nodes and open handles

**Found by:** the Windows P0 lane, recorded in D197 and assigned by ROOT after memory-01 landed at b2e3b069a (fix 7f0cb8ce9).

**Status (2026-11-14):** fixed in c37547114, eddbef457, fbabcfe6b and ee3cf6fe9 (red `refused_lookups_leave_the_held_count_unchanged` and `refused_creates_leave_the_held_count_unchanged`, abecce1a5: eight refused lookups left nine nodes; eight refused creates also left eight handles and descriptors). Green on ee3cf6fe9 (a merge of main 118d14ccf) on macOS arm64 and Linux x86_64, nine guard mutants killed; a RELEASE the server refuses keeps the share and still forgets its entry. Owed to ROOT: Windows `cargo test -p shards-vmm --lib devices::virtio::fs`, and the real-VM runs in `docs/research/measurements/memory-fixes-v1/memory-18.md`.

**Location:** `crates/vmm/src/devices/virtio/fs/mod.rs`, the window-reconciliation refusal after `Slot::ask_into`; `server.rs` lookup/handle ownership.

**Defect:** A successful LOOKUP or CREATE has already increased the server's lookup count and, for CREATE, taken a handle/descriptor. The device can replace its success with EIO when its window cannot follow the file. The guest never receives the new identity, so it cannot issue the matching FORGET or RELEASE. Repeating such refusals retains nodes, references and descriptors until unmount, worsening resource pressure.

**Red test:** `refused_lookups_leave_the_held_count_unchanged` and `refused_creates_leave_the_held_count_unchanged` at abecce1a5 run real framed requests through Slot and Server, then invoke the device's extracted EIO replacement. Compare held nodes, lookup references, handles, cached descriptors and pins before and after a loop. The replacement is the production path; the unit test does not cause an actual Windows/Unix mapping failure.

**Fix direction:** Retire exactly the acquisitions in an unpublished successful reply through the existing FUSE FORGET/RELEASE operations. Preserve references and handles from earlier accepted replies, and preserve the filesystem effects of completed mutations. Amend D197's until-unmount limitation.

## Checked and found sound

These are static checks, not passing runtime results.

- DAX window-offset/length calculations use checked additions/conversions and validate complete aligned ranges before MAP_FIXED. Windows also validates file-offset alignment and checked file-offset-plus-length. No verified host-window escape was found.
- Descriptor acquisition checks writable-share permission for writable mappings (and every HVF mapping where SHARED_NEEDS_WRITE is required), regular-file type, no-follow on the final component, and opened-file identity. No proved read-only-share write bypass or unrelated-VM writable mapping was found.
- Immutable pmem regions map read-only in host and guest; restored template heads use private/COW mappings. Snapshot creation stages separate files, and clone reuse checks its generation before choosing reuse versus copy. I found no template modified in place through these checked paths.
- Ordinary save/template failures stop a machine that has lost an APFS mapping; the missing equivalent in precopy is memory-05. Shared::flush holds the file mutex used by release; Shared::read lacks it (memory-03).
- virtio-mem request arithmetic uses checked multiplication/addition and rejects zero, unaligned, out-of-region, already-plugged/unplugged transitions before changing the mapping. Normal host-side plug/unplug processing runs through Access and one device worker. The live readers bypass that ownership (memory-02).
- Free-page reporting coalesces ranges before taking Access and delegates whole-host-page validation to discard. Pin overlap blocks memory give-back. Actual freed bytes and requested unplug bytes remain distinct in reclaim responses.
- The per-CPU reserve fraction 1/8 is explicitly derived from the pinned kernel's MIN_PERCPU_PAGELIST_HIGH_FRACTION; struct-page overhead 1/64 is derived from 64-byte metadata per 4 KiB guest page. Those differ from the separately chosen 64 MiB slack in memory-11. The 2 MiB FUSE range alignment has a cited guest-kernel requirement; it is not treated as an arbitrary policy knob here.
- FaultAroundOff retains its userfaultfd in the machine lifetime, rather than closing it immediately after registration. Linux/macos discard implementations are deliberately different and documented; Windows file-view give-back separately handles complete COW views and private gaps. No new unconditional equivalence claim is made for them.

## Not covered

- All live reproduction, tests, policy checker, fault injection, concurrency gates, real VM/daemon startup, profiling, benchmarking, heavy E2E, stress/chaos and mutation campaigns. The brief prohibits running these; ROOT must validate the proposed scenarios. No implementation/fix is supplied, so no before/after or green claim exists.
- D188 goals #6/#11's at-most-2x FUSE timing under sustained and short pressure. Historical PM evidence was read where it supports the inspected implementation; this review did not establish a HEAD performance result, 4–5x competitor result, latency distribution, or allocation/fault count.
- The full Linux glibc/musl amd64/arm64, Windows amd64/arm64 and macOS arm64 runtime matrix. In particular, WHP's read-only host views under writable stage-2 mappings were not exercised with guest stores; the inspected mapping test does not establish that enforcement live. No vulnerability is asserted without that proof.
- Adversarial APFS last-writer writeback, mapped-hole, vnode-recycling campaigns and persistence across actual crash/restart. Existing precautions were traced, but keeping/restarting a layer and return of freed disk blocks were not validated live.
- An exhaustive R03 inventory of every memory-related literal across the entire repository, all daemon group budgeting/move state machines, IOMMU implementations, or unrelated devices. Findings 06–17 cover the concrete policy choices inspected here; absence from that list is not an attestation that another literal complies.
- Potential budget watcher lifetime/cgroup-path reuse concerns and Windows failed-remap fallback-state concerns did not receive a sufficiently complete reachable-scenario check and are not promoted to findings.

R01/R04 ownership and error-path violations are recorded above; R03 violations are explicit. R02/R05/R07/R10 historical evidence and comments were inspected only to support/refute these findings. R06/R12/R13 were not used for style complaints. R08/R09/R11/R14/R15 live gates remain unrun under the binding brief. R16 had no cleanup/build/resource action. No repository readiness, merge approval, or passing validation claim is made.

Delivery status: direct Kiln agents.send notifications to ROOT (`01a148884a4770fdaf0111f3f792a248`) were rejected repeatedly with HTTP 429, typed `backpressure`, `pending: 3`, `authority: 2`; queued delivery was also rejected. These are **not accepted/delivered receipts**. The immutable original notification key is `g121-review-memory-2bc5f78f9-final-v1` (alternate queued attempt: `g121-review-memory-2bc5f78f9-queued-v1`). The complete ranked result remains in this report for ROOT; successful Kiln notification is outstanding. agents.list confirmed this is the working Shards coordinator. No attempt was made to interrupt it or bypass its inbox control.
