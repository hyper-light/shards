# migrate adversarial review

POLICY_READ — role: **adversarial reviewer, area migrate** (coordinator for three scoped read-only review lanes; ROOT owns validation and fixes).

- Head and origin/main: `2bc5f78f9e48d09ddbf4e848f9960133f6d19148`.
- Reviewed range: `79caf2d74..2bc5f78f9` and the code at head.
- Worktree: `/Users/adalundhe/Projects/shards/.claude/worktrees/rv-migrate`.
- Binding brief: `/tmp/g121-shot/review/brief.md`, read in full first.
- SHA-256 `AGENTS.md`: `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41`.
- SHA-256 `docs/development-rules.md`: `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
- SHA-256 `docs/development-evidence.md`: `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63`.
- SHA-256 `CLAUDE.md`: `588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010`.
- All three delegated lanes acknowledged the same policy before analysis; no further delegation.

Read-only source review on the supplied macOS arm64 checkout. No code changes, commits, builds, tests, daemons, VMs, benchmarks, policy-script runs or live mutations. The review report was first written to `/tmp/g121-shot/review/migrate.md`, then saved here on 2026-11-13 at the user's explicit request. This Markdown report is the only repository addition; no code was changed. The user will notify Shards. Findings below are verified **source-path proofs**, not live reproductions. R09/R10 live reproduction, exact hardware effects, fixed/green and mutation evidence remain ROOT's work; none is represented as passed.

Priorities: **P0 = critical**, **P1 = high**, **P3 = medium**, **P5 = low**; no P2/P4 findings assigned. Counts: **2 critical/P0, 6 high/P1, 2 medium/P3, 0 low/P5**.

## Findings, in severity order

### migrate-01 — P0 / critical — Archive-created symlinks escape migration staging

**Location:** `crates/shards/src/daemon/moves.rs:1174`, `:1188`, `:1195`, `:1226`.

**Defect:** The importer checks path components lexically but follows symlinks created by earlier archive entries when creating subsequent entries, allowing a malicious migration source to write outside its staging directory.

**Concrete scenario:** A plan names volume `data`. Its stream supplies `LINK volumes/data/_data/out -> /tmp/destination-owned-dir`, then `FILE volumes/data/_data/out/new-file` containing attacker bytes. Both entry names pass `allowed` and `relative`. The existing destination directory is followed, and `new-file` is created there as the daemon's user. A target such as an existing user startup/configuration directory allows creation of a new executable/configuration file outside the imported volume. Import may fail later for missing container metadata; that does not undo the external write.

**Evidence:** `path()` returns `self.root.join(relative(rel)?)` (:1158). The LINK branch calls `guest::make_link(... target.as_bytes())` (:1174), without limiting the target. The FILE branch invokes ordinary `create_dir_all` through `make_parent` (:1188, :1226) and `OpenOptions::create_new(true).open(&path)` (:1193-1195). `create_new` protects the final component, not its parent path. On Unix, `server_guest.rs:154-157` calls `symlink_at`, and `server_unix.rs:57-65` opens directories with `O_DIRECTORY`, without a root-beneath/no-symlink constraint. No race is necessary. Authentication of a peer does not make archive paths safe; this is an authenticated malicious-source/imported-data confinement defect, not an anonymous network-authentication bypass.

**Refutation checked:** `relative` rejects `..`, absolute names and backslashes, but the payload's names need none. Symlinks must remain representable as volume data; rejecting their traversal as parents is the missing property. `Staging::drop` only removes the staging tree.

**Confidence:** High, deterministic Unix source path; no exploit executed. Windows WSL-link behavior not verified.

**Fix direction:** Extract relative to held directory handles with beneath/no-follow semantics for every parent; permit symlink objects as data but never traverse them during extraction or metadata application.

**Regression sketch:** Feed the two entries above to `Placer` with a separate disposable outside directory. Require refusal and assert that the outside sentinel is absent, including after import cleanup. Also test relative symlink targets and directory/node/metadata operations below links. Run only in ROOT's disposable scope.

### migrate-02 — P0 / critical — Ending HVF working-set recording silently disables live dirty tracking

**Location:** `crates/vmm/src/hv/hvf/mod.rs:516-523`; `crates/vmm/src/vm/runtime.rs:498-535`.

**Defect:** `Watch::stop` restores full memory permissions even when a concurrent live migration still needs write protection, so guest writes can disappear from every later round and the final delta.

**Concrete scenario:** An HVF warm run is recording its startup working set. Start live pre-copy before recording ends and copy page P with its old bytes. The recording timer fires, granting P full write permissions. The guest then writes new bytes to P. That write no longer faults, so P is absent from the dirty set; the last save omits it and the destination resumes with old bytes.

**Evidence:** `watch_writes` sets `dirty = Some(...)` and protects writable regions read-only (`hvf/mod.rs:532-541`). `stop` takes `recording`, then calls `sys::protect(r.gpa, r.len, r.perms)` for all regions without consulting `dirty` (:516-523). `written` only re-protects pages already in the dirty set (:550-579), so an unrecorded write cannot be recovered. `Handle::precopy` checks startup/backend readiness but neither ends nor rejects recording (`runtime.rs:498-535`). The independent timer is reachable: warm `started` announces startup then calls `end_recording_in` (`vm_run.rs:2069-2071`); its thread later calls `handle.end_recording` (:2384-2396), which reaches the HVF recorder's `stop` (`runtime.rs:845-849`, `vm/aarch64.rs:987-989`). Timer scheduling can extend the overlap beyond its nominal 50 ms. The final live save uses only collected dirty pages (`runtime.rs:695-714`), with memory files intentionally left empty.

**Refutation checked:** `give` combines recording and dirty tracking, and `unwatch_writes` honors recording, but `stop` lacks the reciprocal check. No migration gate serializes the two lifetimes.

**Confidence:** High in the interleaving; not run on HVF.

**Fix direction:** End recording atomically before installing migration tracking, or preserve dirty-tracking permissions when recording stops, respecting the documented recording → dirty → regions lock order.

**Regression sketch:** On a real HVF VM, execute `watch.start(); watch.watch_writes(); watch.stop();`, guest-write P, and require `written()` to contain P. Then force recording stop during a blocked first migration round and assert the destination observes P's new contents.

### migrate-03 — P1 / high — Pre-copy dereferences hotplug RAM after it can be unplugged

**Location:** `crates/vmm/src/memory.rs:707-741`, `:751-768`; caller `crates/vmm/src/vm/runtime.rs:550-555`.

**Defect:** Live pre-copy reads raw hotplug-memory pointers without the access guard or a pin that prevents concurrent virtio-mem unplug, allowing the guest to crash its source VMM during migration.

**Concrete scenario:** A plugged block contains nonzero data. `each_used` captures that block in `h.runs(&h.plugged)`. Before its scan reaches the block, the running guest requests UNPLUG. The device unmaps the block and returns its host pages. The scan then loads from an inaccessible mapping, faulting the VMM. `page_at` has the same check-to-use lifetime gap between finding a plugged page and the stream reading its returned slice.

**Evidence:** `each_used` calls `runs(h.host(gpa), ...)` (:739-741); `runs` executes raw `is_zero` loads (:714) and exposes raw slices (:723-724), with no `Access`. `page_at` returns a slice after `host_ptr` succeeds (:763-768), also without an access guard. Pre-copy invokes these while vCPUs/devices run; its first pause happens later (`runtime.rs:560`). Virtio-mem UNPLUG reaches `Access::unplug` (`devices/virtio/mem.rs:245-260`), which clears plugged bits and calls `platform::withhold` (`memory.rs:1137-1146`). That replaces memory with `PROT_NONE` on Unix (`platform/unix.rs:255-263`) or decommits it on Windows (`platform/windows.rs:149-151`). This violates `GuestMemory`'s own host-access safety invariant (`memory.rs:341-344`).

**Refutation checked:** Atomic plugged bits do not retain an accessible mapping after enumeration/check. The scanner does not participate in `Access` serialization or DMA pinning. This affects both first and subsequent rounds.

**Confidence:** High, source-proven lifetime violation; fault not executed.

**Fix direction:** Copy bounded chunks into owned buffers under a short access guard, or pin ranges during reads; release the guard before network writes.

**Regression sketch:** In a disposable child, synchronize after plugged-run enumeration, unplug a later block, then resume scanning. Require source survival and either a consistent migration or typed refusal. Repeat against `page_at` and with a guest issuing real virtio-mem requests.

### migrate-04 — P1 / high — A forged snapshot header defeats the state allocation bound

**Location:** `crates/vmm/src/snapshot/mod.rs:652-655`; `crates/vmm/src/platform/mod.rs:242-255`.

**Defect:** The state-file read bound comes from the same unvalidated file's RAM claim, so a tiny sparse migration payload can force a multi-GiB committed allocation before state validation.

**Concrete scenario:** Supply a snapshot state with a valid format/arch/generation header, `memory_mib = u64::MAX`, and a sparse logical length of 8 GiB or more. Migration can create the sparse file from its declared length and a short header run. Reading/relocating it accepts the header-derived bound and zero-fills a Vec for the entire logical file, before rejecting the malformed machine state. On an overcommitting Linux host the reserve may succeed and the following zero-fill can OOM-kill the daemon/restorer; even without that outcome the attacker controls the committed allocation size.

**Evidence:** `state_most` maps the unchecked header RAM directly to `most_state` (:652-655), whose multiply saturates to `u64::MAX` (:144-145). `read_beneath_by` reads only the head, obtains that bound, then calls `read_whole` (`platform/mod.rs:224-228`). `read_whole` performs `try_reserve_exact(len)` and `bytes.resize(len, 0)` (:249-255) before the snapshot decoder/configuration checks. Both `backing_named` and `read` reach this reader (`snapshot/mod.rs:1441-1444`, :1451 onward). The importer's sparse FILE path directly uses wire `len` in `file.set_len(len)` (`daemon/moves.rs:1198`), so wire bytes need not approach the declared state length.

**Refutation checked:** `try_reserve_exact` handles allocator refusal but does not bound a successful reservation or prevent zero-fill from faulting in its pages. Architecture and name checks do not validate/admit the claimed RAM. Later decoder bounds are too late.

**Confidence:** High for unbounded allocation relative to admitted resources; actual OOM outcome OS/resource dependent and unrun.

**Fix direction:** Apply an independently admitted restore/resource allowance before whole-state allocation; validate the header against it and parse incrementally where possible. Do not replace this with another unexplained constant.

**Regression sketch:** ROOT runs a constrained disposable child against a large sparse malformed state. It must refuse after its header with bounded RSS, without allocating according to the forged RAM claim. Include the import/relocation entry path.

### migrate-05 — P1 / high — Named-volume hardlinks become independent files after a move

**Location:** `crates/shards/src/daemon/moves.rs:891`, `:904-912`, `:1183-1198`.

**Defect:** The migration stream serializes each regular filename independently and recreates each as a fresh file, losing hardlink identity and file consistency visible to the guest.

**Concrete scenario:** A guest creates volume file `a`, then hardlinks `b` to it. After a cold or live move, writing `a` no longer changes `b`; link count changes from two to one. Applications using hardlinks for shared updates or locking see different data/inodes after resume.

**Evidence:** `Writer::tree` sends every `S_IFREG` through `file` (:891), which emits independent FILE metadata/data (:904-912). `guest::Meta` contains mode/rdev/owner/xattrs only (`server_guest.rs:19-29`); no inode identity is carried. Placer creates a fresh file for each FILE (:1189-1198). Stream LINK means symlink exclusively (:861-868, :1171-1175). No hardlink frame or alias reconstruction exists. D176 part 6 claims volume files arrive as the guest saw them; the fs server itself models multiple names for one inode (`server.rs:130-131`, :2438 onward), so this is meaningful state.

**Refutation checked:** Sparse extents preserve contents of each initial copy, not aliasing. Fs-server node restoration cannot make distinct destination host inodes share later writes.

**Confidence:** High, deterministic format limitation; unrun.

**Fix direction:** Carry per-volume file identity and hardlink records; recreate aliases to an already imported inode using safely rooted operations.

**Regression sketch:** Extend `a_volumes_files_move_as_its_guest_saw_them` with `hard_link(a,b)`, check destination identity/link count, write one alias and read the other. Then exercise open guest handles across a real move.

### migrate-06 — P1 / high — vCPU shaping re-enables TSC-deadline outside the configured baseline

**Location:** `crates/vmm/src/arch/x86_64/cpuid.rs:147-150`; `crates/vmm/src/hv/kvm/x86_64.rs:164`, `:243`.

**Defect:** KVM applies the fleet baseline, then re-adds TSC_DEADLINE from the source host's capability, exposing a feature absent from the common CPU and causing destination restore to fail.

**Concrete scenario:** Same-vendor source supports `KVM_CAP_TSC_DEADLINE_TIMER`, destination does not, and their common baseline clears CPUID leaf 1 ECX24. Source guest nevertheless sees ECX24. Destination `ready` accepts the common baseline, but actual restore rejects the saved CPUID for missing ECX24.

**Evidence:** `Vm::new` calls `cpuid::within` (:164), separately discovers host deadline capability (:165-168), then `create_vcpu` passes it to `for_vcpu` (:243). `for_vcpu` unconditionally sets the bit when true (`cpuid.rs:147-148`). `FEATURES` checks leaf 1 ECX in full (:443); `covers` computes guest bits absent from host (:559-560), so destination `restore_state` refuses (`kvm/x86_64.rs:686-687`). With a nonempty baseline, the earlier x86 migration check deliberately drops named host-feature comparisons (`daemon/moves.rs:201-205`), so it does not catch this extra feature.

**Confidence:** High for the deterministic shaping/refusal path; hardware pair unverified.

**Fix direction:** Intersect deadline support with the configured guest baseline; keep topology/synthetic shaping separate from host-dependent feature grants.

**Regression sketch:** Clear ECX24 in baseline, apply `within`, then `for_vcpu(..., true)` and assert the bit stays clear. Follow with the two-host move and timer-progress test. D245's common-CPU contract is the expected behavior.

**Status (2026-11-13):** fixed in a09a09f0c5f96d5868c601f38202f71a25d8cbc8 (red `a_cpu_baseline_keeps_deadline_timer_shaping_within_its_features`, Linux x86_64, Kiln r2; green 10 CPUID tests and clippy, r3). Mixed-host VM validation remains ROOT's. [Fix evidence](../../research/measurements/migrate/fix-review-2026-11-13.md).

### migrate-07 — P1 / high — A stronger destination's XSAVE buffer size rejects a compatible guest

**Location:** `crates/vmm/src/hv/kvm/x86_64.rs:705-710`.

**Defect:** Restore requires the source's serialized XSAVE buffer to be at least the destination host's XSAVE allocation size, even when the configured guest baseline excludes the destination-only components.

**Concrete scenario:** A non-AMX source saves a 4096-byte XSAVE area. A same-vendor AMX destination has an 11008-byte host area. Both expose the common non-AMX baseline. Feature coverage succeeds, but restore refuses because `4096 < 11008`, after the migration stream was transferred.

**Evidence:** `xsave_size` derives from host `KVM_CAP_XSAVE2`, independently of the baseline (`x86_64.rs:170-189`). Save serializes that source host size (:568-571). Restore checks coverage (:686-687), then rejects a shorter source buffer (:705-710). The state decoder's fixture explicitly handles an 11008-byte AMX host area (`hv/kvm/state.rs:176-179`); this is an anticipated host variation. D176/D245 promise movement to a CPU covering the guest's exposed features. The safe refusal predates portions of the new migration stack, but prevents its new mixed-host contract.

**Confidence:** High for the guard and its incompatibility with host-size variation; exact hardware pair unrun.

**Fix direction:** Validate compatible guest component/layout state, construct a destination-sized zero-initialized ioctl buffer, and copy the admitted serialized components. Keep incompatible layouts a typed refusal.

**Regression sketch:** Save on a non-AMX host, restore on a same-vendor AMX host with their common baseline, and verify register state and guest progress. Also test incompatible component/layout rejection.

### migrate-08 — P1 / high — Linux arm64 readiness treats baseline-masked features as unmasked

**Location:** `crates/shards/src/daemon/moves.rs:185-191`, `:1819-1823`.

**Defect:** The arm64 feature-name mapping recognizes macOS names only, but is used with Linux `/proc/cpuinfo` names, so a valid Linux common baseline does not suppress source-only features at readiness.

**Concrete scenario:** Source Linux arm64 offers AES, destination does not, and their common ID_AA64ISAR0_EL1 baseline masks AES. Both hypervisor views cover that baseline and the source guest is masked correctly. `unmasked` nevertheless retains source host feature `aes`, and `ready` refuses `Refused::Features` before the move.

**Evidence:** `feature_register` matches names such as `FEAT_AES` (:164-178). Linux `host_cpu` reads `Features` spellings directly (:289-304). The shared arm64 `unmasked` retains a feature when `feature_register` returns None (:190). Readiness compares those names with the destination (:1819-1823). Existing tests use macOS spellings (:2665-2676), leaving this Linux path uncovered.

**Confidence:** High, deterministic false refusal; real Linux arm64 host pair unrun.

**Fix direction:** Map each OS's spellings correctly or derive residual compatibility directly from guest-visible register identities instead of names.

**Regression sketch:** On arm64, `assert!(unmasked(&["aes".into()], &[(0xc030, 0)]).is_empty())` fails currently. Add Linux spelling cases and a real masked-feature move between suitable hosts.

### migrate-09 — P3 / medium — D191's hold still uses an explicitly unjustified fixed period

**Location:** `crates/vmm/src/vm/mod.rs:205-209`; related `crates/vmm/src/platform/windows_thread.rs:66`.

**Defect:** The hold uses a fixed 100 ms upstream default despite the binding R03 requirement and its own design record's conclusion that the period must follow measured wake/clock resolution.

**Concrete scenario:** D191 documents that Windows GetThreadTimes may charge 15.625 ms ticks: with four vCPUs below 625 milli-CPU, a 100 ms period cannot resolve the share within that period, so enforcement is only averaged across periods. The runtime continues to report/use 100 ms regardless of local resolution or cold-start observations.

**Evidence:** `HOLD_PERIOD` is `Duration::from_millis(100)` (:209); its comment says the derivation is waiting on measurement (:208). `docs/design/architecture.md:4036` onward explicitly calls the upstream default insufficient under R03, decides `P = vcpus × g / share` with measured g, and records its implementation/measurements as owed. The later documentation's decision to retain 100 ms pending ROOT's work cannot waive the current brief's explicit R03 standard. Windows's priority governor also copies the fixed 100 ms (`windows_thread.rs:66`).

**Confidence:** Certain policy violation; the timing limitation is documented, not reproduced here.

**Fix direction:** ROOT performs the deciding backend measurements and implements the measured derivation, including cold-start behavior; report remaining performance misses honestly.

**Regression sketch:** Vary observed clock/wakeup granularity and requested rate, require the hold period/report to follow the admitted derivation, then measure real guest progress, overshoot and neighboring work. No benchmark was run here.

### migrate-10 — P3 / medium — Migration/restore retains chosen cutoffs and budgets forbidden by the brief

**Location:** `crates/shards/src/daemon/moves.rs:69-70`; `crates/vmm/src/vm/live.rs:76-78`; `crates/vmm/src/hv/mod.rs:74`; `crates/vmm/src/snapshot/mod.rs:178`.

**Defect:** Production migration/restore decisions still depend on fixed retry counts, cutoffs and budgets derived from upstream choices or isolated past measurements rather than the running environment, contrary to the binding R03 instruction.

**Concrete scenarios and evidence:**

- Live moves stop at 30 rounds, at 8 MiB remaining, or after a 3× first-round byte factor (`daemon/moves.rs:69-70`, `vm/live.rs:76-78`). D176 part 7 cites Xen's choices and explains 8 MiB using a chosen 1 Gb/s rate; neither establishes a protocol/physical bound or locally learned decision for a slow/cold link. Two different links with the same byte rounds take the same cutoff despite different residual downtime. The performance impact is unmeasured here.
- Written working-set pre-copy is capped at 64 MiB (`hv/mod.rs:66-74`), explicitly rounded above a single measured largest set of 3900 × 16 KiB pages. Beyond the cap, runtime clears written marks and performs reads instead (`vm/runtime.rs:1295-1302`): pages beyond it fault COW on the request. This directly limits goal #16's copying-ahead behavior. This inherited constant remains in the head path reviewed; it is not claimed as a newly added regression.
- Snapshot generation replacement retries are fixed at eight (`snapshot/mod.rs:178`, :1454-1465), with no protocol bound or learning. Eight consecutive generation replacements cause refusal even if a valid generation remains available immediately afterward. This is also inherited behavior, not a new regression.
- Further selected sizes in the same changed transport: 1 MiB copy chunks (`daemon/moves.rs:61`) and 1 MiB packing frames/two queued frames (`packing.rs:46-49`). Packing's comment refers to a past compression measurement; it provides no local learning for these selected sizes/queue depth. The fixed recording horizon is 50 ms HVF/1 s otherwise (`vm_run.rs:2377-2381`), justified using past hosts and a target start time rather than current cold-start completion.

**Refutation checked:** These are runtime policy/resource choices, unlike wire tag values, architecture page sizes or register widths. The brief expressly says an upstream default or one benchmark alone is insufficient. Presence of a citation does not make those choices protocol invariants.

**Confidence:** High for fixed decisions and R03 conflict; no claim that a measured latency/resource regression was established.

**Fix direction:** Replace selected policy/budget values with locally learned or independently justified bounds; retain format/spec constants. Record cold-start, uncertainty, overflow and adversarial behavior in the D/evidence record.

**Regression sketch:** Vary link progress, restore working-set sizes, resource allowance and concurrent snapshot publication. Verify decisions follow the learned/admitted inputs rather than these fixed thresholds; require live before/after and mutation evidence from ROOT.

## Checked and found sound

These are source-inspection conclusions only; none is a new passing runtime gate.

- The remote migration carrier is authenticated and integrity protected: client server-key pinning, mandatory Ed25519 client proof, TLS 1.3 AES-256-GCM and early-data disabled (`crates/remote/src/tls.rs`). Server Exec handling requires enrollment (`server.rs:393`, :434), and daemon command start rechecks enrollment (`daemon/remote.rs:96-103`). No anonymous migration-authentication gap established. This does not validate archive contents from an authenticated malicious source.
- Archive names reject absolute paths, `..`, empty components, NUL and backslashes; normal FILE extents use checked addition, enforce monotone non-overlapping runs and stay within declared file length (`daemon/moves.rs:772-788`, :1100-1117). The symlink-parent hole remains migrate-01.
- Live record bodies are capped by `MAX_RECORD` before the reader takes a slice (`vm/live.rs`); packed frames bound both encoded and declared decoded sizes, cap decompressed output, and reject size mismatches (`packing.rs:287-323`). These local bounds do not establish aggregate disk/memory limits.
- Snapshot Reader byte/sequence parsing checks remaining input, uses checked sequence minima and fallible reservation. Generation names and current pointers are restricted; architecture is explicitly compared (`snapshot/mod.rs:636-642`). Memory-file length is checked before mapping. RAM mappings validate alignment, checked ranges and overlap, and Unix/Windows file-view layouts validate ordered in-range runs.
- Working-set parsing checks page alignment, uniqueness/count and mapped-memory membership before prefetch. No Windows-specific out-of-range prefetch defect established.
- Lost-timer fix holds in the inspected order: KVM reads MSRs before LAPIC (`hv/kvm/x86_64.rs:581-582`), then events; restore installs LAPIC before the deadline MSR. This matches the lost-timer explanation in D176/PM M280. Timing/hardware reproduction was not rerun.
- The save barrier parks vCPUs and pauses devices before VM/device state capture. KVM dirty-ring-full exits drain rings; harvesting uses acquire/release flags and resets harvested entries before copying. No ring-overflow page-loss defect established in this pass.
- KVM arm64 uses bitmap tracking that includes ITS writes; GIC capture flushes pending/ITS tables before memory capture and final dirty collection. WHP harvests dirty ranges before replacing/unmapping mappings and conservatively treats bitmap-read failures as fully dirty.
- x86 CPU coverage rejects vendor mismatches and missing declared feature bits; missing leaves count as zero. Feature masks include XCR0 and IA32_XSS. Arm baseline application reads back ID registers and refuses failure to apply them. The three compatibility problems above remain.
- D191 held-vCPU waits use the stored period end and can finish without a delayed governor wakeup. No deadlock established there.

## Not covered

- No R09 live reproduction, green fix validation, guard-removal mutation, stress/chaos run or policy-script execution: expressly prohibited by the binding brief. Proposed tests are sketches, not executed checks. No fixes were made.
- No live two-machine movement, destination failure, client death, split-brain recovery, timer/GIC hardware campaign, filesystem corruption campaign, backend API race validation or Windows-arm64 VM run.
- No benchmarks/profiling, cold/contended latency distributions, downtime measurements, allocation/fault measurements, competitor comparison or seven-target build/lint matrix. Existing PM records belong to their cited revisions, not this review's new validation.
- The full Arm ID-field compatibility lattice, cross-backend direct snapshot decoding, every virtio queue/vsock state combination, simultaneous external writers to shared volumes/binds, backing-file digest-cache invalidation, aggregate archive quotas, hardlink/open-handle details beyond the proven identity loss, and durability under destination crashes were not exhaustively reviewed.
- Fixed-value inventory is focused on the assigned paths; it is not a repository-wide literal audit. No blanket R01–R16 compliance, production readiness, performance or merge approval is asserted.
