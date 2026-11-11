# Reserved lease: ordinary client disconnect — SOURCE PROPOSAL ONLY

This is the newly assigned third case, not the earlier unapproved IntentWritten
WRITE proposal. No patch is applied to native.rs; no build, actor, target/cache,
source package or runtime deadline is allocated. ROOT reviews this exact delta
before authorizing either source composition or execution. The24/19-frame bodies,
assertions, helpers, source/binaries/raw and first preflight failure stay frozen.

POLICY_READ implementer / assigned adversarial: current main
`0a6d91b3de0f10a8397868aa3794adab1e09afdc`, rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Full AGENTS/R01–R16/evidence/CLAUDE/managed plan/current handoff and its required
closing records loaded; coordinator receipt9926 preceded source preparation.
Own only this evidence leaf and the proposed test-only native.rs delta/minimal
existing Case selection. No production/ABI/writer/constructor changes.

## Exact source and what disconnect actually means

The immutable starting point is `a7185733c4bfb1cc18d8018b96ac1790b29ecb24`;
executed native leaf remains SHA256
`228bb9e67e1e699d969a71de817c5a9c6e461ee18e07d98aa070029b07b59e4e`.
Reader/IPC/writer/ABI/platform files are byte-exactba78; hashes are in
[source-scope.unformatted.json](source-scope.unformatted.json). Read-only source
inspection establishes these boundaries, not a new live result:

| Existing pinned source | Concrete behavior |
| --- | --- |
| fs/mod.rs574 `read_any_into` | Header `UnexpectedEof` becomes None. A clean end at the next frame boundary ends the answer loop. A truncated body is an error; this case sends no partial header/body. |
| fs/mod.rs610 `answer` | Borrows `&Server`; reads until None, then returns Ok. EOF sends no DESTROY/Release/Close, calls no `close_all` and does not drop the owner. |
| ipc/unix.rs578 `end` | Existing Write shutdown then Read shutdown; errors are not returned by this API. The fixture records the actual subsequent answer/join/FD outcomes, not a synthetic successful shutdown receipt or peer-side read. |
| Server312/State201 | The still-owned Server holds Mutex<State>, whose audit Option owns Run. The answer thread's borrowed reference is not the lifetime owner. No custom Server/State/Run/Member Drop hook was found in this exact closure; ordinary field ownership releases held files when Server is dropped. |
| audit.rs434 / server.rs680 | Public `audit_readiness` returns actual typed Ready with remaining extent and object count. It exposes NO individual lease stage/live cursor. Its mutex guard is gone before subsequent trace/file I/O. Ready here is not new connection admission or a grant. |
| audit.rs570 / server.rs1538 | Explicit `close_all`/DESTROY closes members; the selected disconnect sends neither. Close does not refund extents. |

Expected source-derived answer is Ok(()) via frame-header EOF, after actual frame8
was completely answered/consumed. A future actual different I/O result/native
cause/panic/timeout must be retained and fail the case; no retry or inferred pass.
Do not claim an independently traced kernel EOF or terminal protocol message from
the local shutdown. Normal answer return under this pinned source is the read-end
observation; no second observer/connection/parser or wrapper is introduced.

The LAST actually observed typed lease reply is request8 Reserved/None with its
validated call/maxima/offsets/accepted0/remaining0. After disconnect, live individual
lease stage/cursors are **Unobserved** through the public interface. The source
contains no EOF-triggered close, but this fixture must not fabricate Closed or a
fresh typed cursor/refund from that observation. After Server drop, its logical
accounting is unobservable; file/root cleanup is witnessed separately.

## Exact8 requests, observations and owner cleanup

1. Same actual constructor/corpus/headers/readiness and read-only held-file
   witnesses as both accepted cases. Preserve stable members1/3 with absent2;
   one aggregate codec-derived B, unchanged original6b/5da bytes, no re-emission.
2. Execute the SAME first8 complete requests: INIT; LOOKUP/OPEN1; LOOKUP/OPEN3;
   Identify1; Identify3; Reserve1. Existing full typed/extent/header predicates
   and actual post-Reserve whole-file snapshots all run unchanged.
3. Third Case returns after verifying sent==8 and records the exact LAST typed
   Reserved reply. No ninth frame: no explicit Close/Release/commit/DESTROY/WRITE,
   replacement connection, new INIT, direct Server::handle call or resubmission.
4. Use existing common owned `shards_ipc::end`, drop client BEFORE joining the
   existing answer thread, and immediate client/server EBADF checks. Keep actual
   joined Result/native cause and all current cleanup predicates/order unchanged.
5. ONLY after successful sequence+answer/FD cleanup, call existing public
   `server.audit_readiness`: require actual Ready remaining0 and original scope
   count. Read the previously created own raw `reserved.member-index-*.bin`
   references using the EXISTING bounded regular-file helper; compare fresh
   reads through the SAME held object descriptors byte-for-byte. These references
   are raw before/after evidence, not a live state cache, new object or ledger.
   Reference read failure fails evidence; no weaker comparison is substituted.
6. Require retained lengths still sum to the same B; the neighbor header, call/
   maxima, committed-prefix bytes and all unused tails equal the actual Reserved
   snapshot. Preserve last wire cursor fields with that source; label individual
   post-disconnect lease/cursors Unobserved. No quota/fairness/durability inference.
7. Keep existing held dev/inode/nlink checks, witness EBADF, Server drop/root-FD
   EBADF, separately labelled path salvage and TempDir owner deletion/root absence.
   These observations precede the final assert. Private writer/kernel population
   remains uncounted, not a universal FD/leak/population-return proof.

If sequence, answer or FD cleanup fails, the added post-join observations are
UnreachedOrUnknown. A failed added reference/readiness/snapshot observation still
returns through the existing cleanup; it cannot block client end/join first.
External deadline kill leaves unavailable cleanup Unknown. Preserve the first
error and every actual record; no fake Closed, refund, record or successful stop.

## Exact unapplied delta and static checks

[native-caller.patch](native-caller.patch) SHA256
`77687ca0019061d41556c0dfd65319e7f7a30e41101dc144dfe83f63a5c560bb`;
formatted proposed leaf SHA256
`65c73f963edef4ffb81754cf2eea2bd4ecd6a44d7f14178994725b211d0a4c65`.
It adds Case::ReservedDisconnect, one return at the shared8-request boundary,
case-gated accounting/held-byte observations AFTER existing join/FD checks, and
one ignored test using the SAME `run`. No extra thread/transport/parser/wrapper/
queue/cache/manager/producer or registration/API seam. The actual leaf stays228b.

Removing ONLY the inserted spans reconstructs the ENTIRE predecessor native.rs
byte-for-byte, including both existing24/19 bodies/assertions/helper/cleanup lines.
This was checked before and after canonical rustfmt. Static format0 under the
existing lock/floor and unapplied `git apply --check`0; these do not compile or
execute the proposed caller. Unformatted proposal/patch and all read/check outputs
are preserved separately. Initial local reads hit sparse-absent/wrong fs paths;
immutable Git paths then supplied the actual source, without altering sparse
checkout or reading/copying an active audit writer. Those diagnostics are retained.

Full staged whitespace check returns2 on the single-space context line9 in EACH
exact formatted/unformatted patch artifact. Both original patch byte streams
are preserved. Excluding ONLY those two named artifacts returns0; canonical
source formatting0 and unapplied patch applicability0 are separate checks.

Proposed eventual exact filter:
`managed_audit_wire_adversarial::reserved_client_disconnect_retains_owned_extent`.
Only after ROOT GO: future NEW admitted Mac ARM64 source/target/cargo-home/TMPDIR/
records,2CPU/2GiB/4GiB600/j2/build180/work30+cleanup15/hard45,30GiB floor+lock,
frozen package/binary/exact-list1/dispatch BEFORE ONE first case. Those paths and
budget are deliberately unallocated now. No retry/time extension/reuse of either
completed target or any ROOT cache, no local heavy build/new full worktree.

## Alternatives and R01–R16 limits

The real existing frame/owner path meets this finite observation without a new
interface. A replacement stream/INIT risks a separate admission effect; direct
internal Handle/Run calls bypass the required wire/ownership boundary; a new
private state getter/manager would expose unnecessary production machinery. None
is proposed. The chosen public accounting view honestly leaves member terminal/
live cursor data unobserved. No broader transport architecture or fix is chosen.

| Rules | Concrete evidence / gap |
| --- | --- |
| R01/R04 | Test-only checked predicates and actual typed I/O/readiness; last observed versus Unobserved data explicit, no fabricated terminal. Compilation/new runtime unrun. |
| R02/R05/R12 | Actual pinned existing reader/owner/accounting source and alternatives above; shared helpers, no new interface/cache/poll/OS copy or lock held across trace I/O. |
| R03 |8 requests derive from shared prelude; B/offsets/maxima use actual codecs/ABI. Future resource/time asks are explicit inputs, never production defaults/rights. |
| R06/R07/R13 | Insertion-only third-case delta, explanatory ownership/uncertainty comments, complete old source recovered exactly. No production changes. |
| R08/R14 | No profile/benchmark/resource/performance claim; ROOT cost/parent/kernel evidence owed. |
| R09/R10 | Source-derived proposal, no live defect/fix/new pass claimed. Exact source/pins/patch, static checks, earlier runtime failures/raw/binaries all retained. |
| R11 | Proposed native Mac ARM64 only; all new compiler/host/VM/matrix gates unrun, paused WinARM untouched. |
| R15/R16 | Policy receipt before edits, no children/goals/main merge/runner/target/cache/actors, lock/floor for source formatting; ownership and all originals/protected scopes preserved. |

ROOT owns actual source/campaign authorization, integration/audits/profiles/heavy
VM/seven-target/main decisions. No in-flight/stall/ENOSPC/crash/fault/mutation,
malformed/helper/allocator/ptrace/other held campaign, WRITE/tool effect or new
authority/physical quota/durability/fairness/admission/cost closure is claimed.
