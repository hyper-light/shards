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

Priorities: **P0 = critical**, **P1 = high**, **P3 = medium**, **P5 = low**; no P2/P4 findings assigned. Counts: **2 critical/P0, 6 high/P1, 2 medium/P3, 0 low/P5**; migrate-11 (P0) was added after the review by the fix lane that found it, making 3 P0.

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

**Status (2026-11-13):** fixed in b97a4336f (red `daemon::moves::tests::a_streams_symlinks_never_lead_out_of_its_directory` at 9efde7252: `new-file` made outside the staging directory through an absolute link; adas-mac-studio.local macOS arm64; green with `server::guest::tests::beneath_goes_through_no_symlink`; mutations red: openat without O_NOFOLLOW (both tests), links allowed outside a volume's files). Each entry is placed through `server::guest::Beneath` (component-wise openat O_DIRECTORY|O_NOFOLLOW from the held staging root, NT FILE_OPEN_REPARSE_POINT), and a stream's symlink is taken only in a volume's `_data`, since `adopt` writes the microVM's own directory by path. Real-VM moves (tests/moved.rs) owed to ROOT.

### migrate-02 — P0 / critical — Ending HVF working-set recording silently disables live dirty tracking

**Location:** `crates/vmm/src/hv/hvf/mod.rs:516-523`; `crates/vmm/src/vm/runtime.rs:498-535`.

**Defect:** `Watch::stop` restores full memory permissions even when a concurrent live migration still needs write protection, so guest writes can disappear from every later round and the final delta.

**Concrete scenario:** An HVF warm run is recording its startup working set. Start live pre-copy before recording ends and copy page P with its old bytes. The recording timer fires, granting P full write permissions. The guest then writes new bytes to P. That write no longer faults, so P is absent from the dirty set; the last save omits it and the destination resumes with old bytes.

**Evidence:** `watch_writes` sets `dirty = Some(...)` and protects writable regions read-only (`hvf/mod.rs:532-541`). `stop` takes `recording`, then calls `sys::protect(r.gpa, r.len, r.perms)` for all regions without consulting `dirty` (:516-523). `written` only re-protects pages already in the dirty set (:550-579), so an unrecorded write cannot be recovered. `Handle::precopy` checks startup/backend readiness but neither ends nor rejects recording (`runtime.rs:498-535`). The independent timer is reachable: warm `started` announces startup then calls `end_recording_in` (`vm_run.rs:2069-2071`); its thread later calls `handle.end_recording` (:2384-2396), which reaches the HVF recorder's `stop` (`runtime.rs:845-849`, `vm/aarch64.rs:987-989`). Timer scheduling can extend the overlap beyond its nominal 50 ms. The final live save uses only collected dirty pages (`runtime.rs:695-714`), with memory files intentionally left empty.

**Refutation checked:** `give` combines recording and dirty tracking, and `unwatch_writes` honors recording, but `stop` lacks the reciprocal check. No migration gate serializes the two lifetimes.

**Confidence:** High in the interleaving; not run on HVF.

**Fix direction:** End recording atomically before installing migration tracking, or preserve dirty-tracking permissions when recording stops, respecting the documented recording → dirty → regions lock order.

**Regression sketch:** On a real HVF VM, execute `watch.start(); watch.watch_writes(); watch.stop();`, guest-write P, and require `written()` to contain P. Then force recording stop during a blocked first migration round and assert the destination observes P's new contents.

**Status (2026-11-13):** fixed in db8604ed6 (red `hv::hvf::tests::vcpu_and_gic_state_round_trip_through_a_fresh_vcpu` at 9efde7252 on real HVF: a guest's read within a recording the watch began in went unrecorded, and its writes after a recording ended went unwritten; adas-mac-studio.local macOS arm64; mutations red: `stop` ignoring the watch, `watch_writes` ignoring the recording). `Watch::rest` puts each region to what both watchers leave it (none while recording, read-only while watched, else full) as either starts or ends. Real-VM live moves on HVF owed to ROOT.

### migrate-03 — P1 / high — Pre-copy dereferences hotplug RAM after it can be unplugged

**Location:** `crates/vmm/src/memory.rs:707-741`, `:751-768`; caller `crates/vmm/src/vm/runtime.rs:550-555`.

**Defect:** Live pre-copy reads raw hotplug-memory pointers without the access guard or a pin that prevents concurrent virtio-mem unplug, allowing the guest to crash its source VMM during migration.

**Concrete scenario:** A plugged block contains nonzero data. `each_used` captures that block in `h.runs(&h.plugged)`. Before its scan reaches the block, the running guest requests UNPLUG. The device unmaps the block and returns its host pages. The scan then loads from an inaccessible mapping, faulting the VMM. `page_at` has the same check-to-use lifetime gap between finding a plugged page and the stream reading its returned slice.

**Evidence:** `each_used` calls `runs(h.host(gpa), ...)` (:739-741); `runs` executes raw `is_zero` loads (:714) and exposes raw slices (:723-724), with no `Access`. `page_at` returns a slice after `host_ptr` succeeds (:763-768), also without an access guard. Pre-copy invokes these while vCPUs/devices run; its first pause happens later (`runtime.rs:560`). Virtio-mem UNPLUG reaches `Access::unplug` (`devices/virtio/mem.rs:245-260`), which clears plugged bits and calls `platform::withhold` (`memory.rs:1137-1146`). That replaces memory with `PROT_NONE` on Unix (`platform/unix.rs:255-263`) or decommits it on Windows (`platform/windows.rs:149-151`). This violates `GuestMemory`'s own host-access safety invariant (`memory.rs:341-344`).

**Refutation checked:** Atomic plugged bits do not retain an accessible mapping after enumeration/check. The scanner does not participate in `Access` serialization or DMA pinning. This affects both first and subsequent rounds.

**Confidence:** High, source-proven lifetime violation; fault not executed.

**Fix direction:** Copy bounded chunks into owned buffers under a short access guard, or pin ranges during reads; release the guard before network writes.

**Regression sketch:** In a disposable child, synchronize after plugged-run enumeration, unplug a later block, then resume scanning. Require source survival and either a consistent migration or typed refusal. Repeat against `page_at` and with a guest issuing real virtio-mem requests.

**Status (2026-11-13):** fixed by the memory reviewer as memory-02 in 29196c6b3, integrated here; red `live_copy_keeps_bytes_after_their_block_is_unplugged` / `live_copy_page_keeps_bytes_after_their_block_is_unplugged`, focused green in memory-fixes-v1. No duplicate implementation in this lane. [Fix evidence](../../research/measurements/migrate/fix-review-2026-11-13.md).

**Integration (2026-11-13):** all five `live_copy_` unit tests passed again on migration merge 7720df839, with shards-vmm/shards clippy clean. Real guest unplug during migration remains owed to ROOT; [retained log](../../research/measurements/migrate/livecopy-03-integrated.log).

### migrate-04 — P1 / high — A forged snapshot header defeats the state allocation bound

**Location:** `crates/vmm/src/snapshot/mod.rs:652-655`; `crates/vmm/src/platform/mod.rs:242-255`.

**Defect:** The state-file read bound comes from the same unvalidated file's RAM claim, so a tiny sparse migration payload can force a multi-GiB committed allocation before state validation.

**Concrete scenario:** Supply a snapshot state with a valid format/arch/generation header, `memory_mib = u64::MAX`, and a sparse logical length of 8 GiB or more. Migration can create the sparse file from its declared length and a short header run. Reading/relocating it accepts the header-derived bound and zero-fills a Vec for the entire logical file, before rejecting the malformed machine state. On an overcommitting Linux host the reserve may succeed and the following zero-fill can OOM-kill the daemon/restorer; even without that outcome the attacker controls the committed allocation size.

**Evidence:** `state_most` maps the unchecked header RAM directly to `most_state` (:652-655), whose multiply saturates to `u64::MAX` (:144-145). `read_beneath_by` reads only the head, obtains that bound, then calls `read_whole` (`platform/mod.rs:224-228`). `read_whole` performs `try_reserve_exact(len)` and `bytes.resize(len, 0)` (:249-255) before the snapshot decoder/configuration checks. Both `backing_named` and `read` reach this reader (`snapshot/mod.rs:1441-1444`, :1451 onward). The importer's sparse FILE path directly uses wire `len` in `file.set_len(len)` (`daemon/moves.rs:1198`), so wire bytes need not approach the declared state length.

**Refutation checked:** `try_reserve_exact` handles allocator refusal but does not bound a successful reservation or prevent zero-fill from faulting in its pages. Architecture and name checks do not validate/admit the claimed RAM. Later decoder bounds are too late.

**Confidence:** High for unbounded allocation relative to admitted resources; actual OOM outcome OS/resource dependent and unrun.

**Fix direction:** Apply an independently admitted restore/resource allowance before whole-state allocation; validate the header against it and parse incrementally where possible. Do not replace this with another unexplained constant.

**Regression sketch:** ROOT runs a constrained disposable child against a large sparse malformed state. It must refuse after its header with bounded RSS, without allocating according to the forged RAM claim. Include the import/relocation entry path.

**Status (2026-11-13):** owed to ROOT: independent restore-admission design and bounded sparse-state resource-fault reproduction; see the fix evidence (proposed test is not yet implemented). [Fix evidence](../../research/measurements/migrate/fix-review-2026-11-13.md).

**Status (2026-11-14):** fixed in a3889a6ee (red `snapshot::tests::on_disk::a_forged_states_head_has_no_more_allocated_than_the_host_holds` with the code before the fix, the head's claim saturated and no host bound: a head of u64::MAX MiB over a 1 MiB sparse state was read whole, `unwrap_err` on an `Ok`; adacachy2 Linux x86_64 on merge 9cbe68bfe of main be8c0854c, its fixture amended in bd70e9cf3 and rerun green there; green 39 `snapshot::` tests on that merge and 38 on adas-mac-studio.local macOS arm64 at a3889a6ee; clippy `-p shards-vmm -p shards --all-targets -D warnings` on Linux x86_64; mutations red, each alone: the host bound (`PastHost`), the checked RAM (`PastAnyMachine`)). A state is refused, typed (`snapshot::StateRefused`), from its head and length before its whole is allocated: past any machine's RAM, past its machine's, past what the reading host can hold now (`platform::holdable_known`: its available memory, else all of it), or where the host says neither. D176's decoders entry amended. Owed to ROOT: a reservation across concurrent reads; the probe's cost on the restore path (R14); a template restored on KVM (its VM process confined, now allowed sysinfo(2)), Hypervisor.framework (App Sandbox's sysctls) and WHP; and the bounded-RSS run of a forged sparse state through a move's import (`tests/moved.rs`).

### migrate-05 — P1 / high — Named-volume hardlinks become independent files after a move

**Location:** `crates/shards/src/daemon/moves.rs:891`, `:904-912`, `:1183-1198`.

**Defect:** The migration stream serializes each regular filename independently and recreates each as a fresh file, losing hardlink identity and file consistency visible to the guest.

**Concrete scenario:** A guest creates volume file `a`, then hardlinks `b` to it. After a cold or live move, writing `a` no longer changes `b`; link count changes from two to one. Applications using hardlinks for shared updates or locking see different data/inodes after resume.

**Evidence:** `Writer::tree` sends every `S_IFREG` through `file` (:891), which emits independent FILE metadata/data (:904-912). `guest::Meta` contains mode/rdev/owner/xattrs only (`server_guest.rs:19-29`); no inode identity is carried. Placer creates a fresh file for each FILE (:1189-1198). Stream LINK means symlink exclusively (:861-868, :1171-1175). No hardlink frame or alias reconstruction exists. D176 part 6 claims volume files arrive as the guest saw them; the fs server itself models multiple names for one inode (`server.rs:130-131`, :2438 onward), so this is meaningful state.

**Refutation checked:** Sparse extents preserve contents of each initial copy, not aliasing. Fs-server node restoration cannot make distinct destination host inodes share later writes.

**Confidence:** High, deterministic format limitation; unrun.

**Fix direction:** Carry per-volume file identity and hardlink records; recreate aliases to an already imported inode using safely rooted operations.

**Regression sketch:** Extend `a_volumes_files_move_as_its_guest_saw_them` with `hard_link(a,b)`, check destination identity/link count, write one alias and read the other. Then exercise open guest handles across a real move.

**Status (2026-11-13):** fixed in fd33ec11a (red `a_volumes_hardlinks_keep_shared_contents_after_move`, committed in 5bf16ecff and rerun against the landed migrate-01 Beneath extractor at 73af9fb62). Version 3 carries regular-file aliases; both names stay under plan-authorized volume data and Beneath traversal. [Red/green and remaining VM/platform checks](../../research/measurements/migrate/fix-review-2026-11-13.md).

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

**Status (2026-11-13):** owed to ROOT: `SHARDS_MOVE_INVITE=<destination-invitation-file> cargo test -p shards --test moved a_microvm_moved_to_another_host_keeps_its_cpu_image_files_and_connections` on a retained non-AMX/AMX host pair; validate CPUID0xD component layouts before implementing padding. [Fix evidence](../../research/measurements/migrate/fix-review-2026-11-13.md).

**Status (2026-11-14):** fixed in 943c653df (red `arch::x86_64::xsave::tests::a_smaller_hosts_xsave_area_is_laid_into_a_larger_ones` with the code before the fix, an area shorter than this host's refused: `Err(Short { len: 4096 })` for a non-AMX host's 4096 bytes into an AMX host's 11008; adacachy2 Linux x86_64 at 943c653df and again on merge 6bf9e1e86 of main be8c0854c; green both `xsave` tests and `hv::kvm::x86_64::tests::xcr0_is_read_from_a_vcpus_xcrs`, clippy `-p shards-vmm --all-targets -D warnings` (Linux x86_64, the KVM backend's code); mutations red, each alone: the code before the fix, the layout comparison, the past-area bound, the header bound). As the fix review's design asked: the saved area is laid into a zeroed area of this host's KVM_CAP_XSAVE2 size, its legacy region and header kept and each component the guest enabled (XCR0) or holds (XSTATE_BV) copied where both hosts' CPUID leaf 0xD lay it out alike; one laid out otherwise or nowhere, a compacted or short area, or one past either area refused typed (`XsaveRefused`, `hv::kvm::Error::Xsave`), after the CPU coverage check and before anything of the vCPU is set. D176's decoders entry amended. Owed to ROOT: the non-AMX/AMX host-pair move above, with the guest's vector registers checked after.

### migrate-08 — P1 / high — Linux arm64 readiness treats baseline-masked features as unmasked

**Location:** `crates/shards/src/daemon/moves.rs:185-191`, `:1819-1823`.

**Defect:** The arm64 feature-name mapping recognizes macOS names only, but is used with Linux `/proc/cpuinfo` names, so a valid Linux common baseline does not suppress source-only features at readiness.

**Concrete scenario:** Source Linux arm64 offers AES, destination does not, and their common ID_AA64ISAR0_EL1 baseline masks AES. Both hypervisor views cover that baseline and the source guest is masked correctly. `unmasked` nevertheless retains source host feature `aes`, and `ready` refuses `Refused::Features` before the move.

**Evidence:** `feature_register` matches names such as `FEAT_AES` (:164-178). Linux `host_cpu` reads `Features` spellings directly (:289-304). The shared arm64 `unmasked` retains a feature when `feature_register` returns None (:190). Readiness compares those names with the destination (:1819-1823). Existing tests use macOS spellings (:2665-2676), leaving this Linux path uncovered.

**Confidence:** High, deterministic false refusal; real Linux arm64 host pair unrun.

**Fix direction:** Map each OS's spellings correctly or derive residual compatibility directly from guest-visible register identities instead of names.

**Regression sketch:** On arm64, `assert!(unmasked(&["aes".into()], &[(0xc030, 0)]).is_empty())` fails currently. Add Linux spelling cases and a real masked-feature move between suitable hosts.

**Status (2026-11-13):** fixed for the mapped Linux single-register spellings in f004611b5 (red `linux_arm64_feature_names_follow_their_baseline_registers`, r7; green 3 Arm CPU tests, 10 x86 CPUID tests and shards/shards-vmm clippy, r8, Linux x86_64). Unknown and multi-register names remain conservatively checked; actual Arm VM moves are owed to ROOT. [Fix evidence](../../research/measurements/migrate/fix-review-2026-11-13.md).

### migrate-09 — P3 / medium — D191's hold still uses an explicitly unjustified fixed period

**Location:** `crates/vmm/src/vm/mod.rs:205-209`; related `crates/vmm/src/platform/windows_thread.rs:66`.

**Defect:** The hold uses a fixed 100 ms upstream default despite the binding R03 requirement and its own design record's conclusion that the period must follow measured wake/clock resolution.

**Concrete scenario:** D191 documents that Windows GetThreadTimes may charge 15.625 ms ticks: with four vCPUs below 625 milli-CPU, a 100 ms period cannot resolve the share within that period, so enforcement is only averaged across periods. The runtime continues to report/use 100 ms regardless of local resolution or cold-start observations.

**Evidence:** `HOLD_PERIOD` is `Duration::from_millis(100)` (:209); its comment says the derivation is waiting on measurement (:208). `docs/design/architecture.md:4036` onward explicitly calls the upstream default insufficient under R03, decides `P = vcpus × g / share` with measured g, and records its implementation/measurements as owed. The later documentation's decision to retain 100 ms pending ROOT's work cannot waive the current brief's explicit R03 standard. Windows's priority governor also copies the fixed 100 ms (`windows_thread.rs:66`).

**Confidence:** Certain policy violation; the timing limitation is documented, not reproduced here.

**Fix direction:** ROOT performs the deciding backend measurements and implements the measured derivation, including cold-start behavior; report remaining performance misses honestly.

**Regression sketch:** Vary observed clock/wakeup granularity and requested rate, require the hold period/report to follow the admitted derivation, then measure real guest progress, overshoot and neighboring work. No benchmark was run here.

**Status (2026-11-13):** owed to ROOT: D191 measured wake/CPU-clock/kick-cost campaign, then its existing derived-period design; no picked replacement period added. [Fix evidence](../../research/measurements/migrate/fix-review-2026-11-13.md).

**Status (2026-11-14):** owed to ROOT: the learner and its cold start are built (78c3b7012, 951010ac1): `platform::granule` learns g, the least lateness of the governors' waits and a thread clock's measured step (cold: `SOONEST`; unreadable clock typed `Unclocked`), and `hold::derived_period` is D191's vcpus × g / share, said in the debug log beside the 100 ms in force. Held by `platform::granule::tests` (3) and `vm::hold::tests::a_holds_derived_period_resolves_its_share`; mutations (an early end counted, the first move taken as the step, g its wake alone) each caught; `cargo clippy -p shards-vmm --all-targets -- -D warnings` clean (adas-mac-studio-3.local). The switch waits on ROOT's kick-cost runs on each backend, `SHARDS_LOG=debug cargo test -p shards --test boot a_managed_guest_is_held_to_the_rate_its_client_asks_on_its_socket` at D191's rates reading each "derived … from g" line, and on the image API's `RATED` carrying each rate's period (D191, "Its learner, built"). Windows' priority governor's copy of the period is platform-11's.

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

**Status (2026-11-13):** owed to ROOT: cold-link, contention and working-set campaigns plus stopping/budget/publication designs in the fix evidence; no replacement constants added. [Fix evidence](../../research/measurements/migrate/fix-review-2026-11-13.md).

**Status (2026-11-14):** partly fixed: the snapshot reader's eight looks went in b0072d884 (red 606f6bdea: ten replacements in a row refused, `snapshot::tests::on_disk::a_reader_follows_every_replacement_and_refuses_a_pointer_to_a_gone_generation`; the mutant looking again at a pointer that did not move hangs and fails it; 36 snapshot, granule and hold tests green, adas-mac-studio-3.local). It looks again as long as its generation was replaced, and refuses a pointer to a gone generation as that (D176's decoders, "A reader looks again as long as its generation was replaced"). Owed to ROOT, each with its design in the fix evidence above: the live move's 30 rounds, 8 MiB and 3x stopping rules (cold-link and slow-link moves, `cargo test -p shards --test moved` on a two-host pair with a throttled link), the 64 MiB pre-copy cap (a working set past it restored on each backend), the 1 MiB copy chunks and packing frames with two queued, and the 50 ms / 1 s recording horizon; none is derivable from what this lane can run, and no number replaced another.

### migrate-11 — P0 / critical — A move's plan names reach home paths unchecked

Added 2026-11-13 by the migrate fix lane (not one of the review's three lanes): found while fixing migrate-01, in the same `adopt` path.

**Location (at 35b1e23d7):** `crates/shards/src/daemon/moves.rs`: `move_ready` `:1829`, `:1837`; `move_import` `:2019`; `adopt` `:2051`, `:2055`, `:2085`, `:2118`, `:2142`, `:2147`, `:2168`; `move_done` `:1561`.

**Defect:** The receiving daemon builds paths of its home from names the sending machine supplies, and checks none of them to be one path component: the plan's `id` (`containers/<id>`, `registry.dir(id)`), its `snapshot` (`containers/<id>/snapshots/<snapshot>`, handed to `lay_under` and `snapshot::relocate`), each of its `volumes` (`volumes/<v>` as the rename's source and target), and each volume name the moved record's `mounts` carry, which becomes `store.data(name)`: the host directory shared into the guest. `move_done`'s snapshot argument is joined the same way before `snapshot::remove`. The stream's entry names go through `relative` (migrate-01's checks); these names do not.

**Concrete scenario:** A malformed or hostile plan whose record mounts volume `../../../../etc` (or `/etc`, which `Path::join` takes as the whole path) is adopted with that mount's `Source` outside `<home>/volumes`, so the guest's share is a host directory of the sender's choosing once the microVM is restored. A snapshot named `/some/other/snapshot` makes `relocate` rewrite another snapshot's state. An `id` of `../x` makes `containers/../x` the microVM's directory. `ready` takes such a plan and answers its needs.

**Evidence:** Unit red (R09 from a reproducing unit test, brief-common): `daemon::tests::a_moves_names_that_are_no_one_name_are_refused_typed_and_reach_nothing` with every check turned off (the `off` mutation, which is the code before the fix): `ready` answers status 0 to a plan whose id is `../x`.

**Fix:** Each such name is checked to be one name of its kind (`Named::takes`): an id is `2 × MICROVM_ID_BYTES` lowercase hex digits, as `containers::new_id` makes one; a snapshot's passes `synced::name_alone(_, true)` (Windows' rules, the stricter, on every host, so a name one host takes every host takes); a volume's passes the local driver's `Store::valid` and `name_alone`. Each rule refuses separators, `.`, `..`, roots and drive colons, so no name can be more than one component. A refusal is typed: `Refused::NotAName { name, field: id | snapshot | volume, value }`. `ready` checks the plan first; `import` checks it right after `read_plan`, before the Placer places any entry; `adopt` takes every name it joins (`staged/containers/<id>`, the snapshot directory for `lay_under` and `relocate`, `registry.dir(id)`, both volume paths, each `mounts` volume before `store.data`) through the same `one_name` check; `done` checks its snapshot before anything else. `import` and `done` now answer through `move_step`'s `typed`, and the mover reads `import`'s refusal typed (`cli/moves.rs` `unready`). `server::guest::Beneath` (migrate-01) is not used for these joins: it places entries under a held root but has no rename, and `adopt`'s steps are renames and reads by path; with each name one checked component, every join stays in its directory.

**Status (2026-11-13):** fixed in e7024593b and 8802770fd (red `daemon::tests::a_moves_names_that_are_no_one_name_are_refused_typed_and_reach_nothing` and `daemon::moves::tests::a_moves_names_are_each_one_name_of_their_kind` with the checks off, which is the code before the fix: `ready` answered status 0 with its needs to a plan whose id is `../x`; adas-mac-studio-3.local macOS 26.2 arm64; green 23 of 23 focused tests (`daemon::moves::`, `daemon::tests::a_move`, `cli::moves::`) and clippy `-p shards --all-targets -D warnings`; mutations red, each alone: `import`'s check before the Placer (an untyped "failed to fill whole buffer"), `ready`'s, `adopt`'s mounts check (an untyped "no snapshot here" after the record was renamed in), `done`'s, the plan's volumes left out of `names_refused`, and each rule: the ID's length, its lowercase hex, a volume's driver rule, a volume's `name_alone`, a snapshot's Windows rules). `adopt`'s joins of the plan's id, snapshot and volumes repeat the up-front check as a second guard, and no test tells one from the other. Real-VM moves (tests/moved.rs) owed to ROOT.

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
