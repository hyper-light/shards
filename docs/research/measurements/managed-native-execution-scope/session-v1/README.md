# Native job lifetime object: implementation checkpoint

The new leaf implements actual cgroup object acquisition at the existing native
admission seam. It is **unregistered/unapplied in the actual owner graph**. The
shared parent acquisition refactor is a separate unapplied patch. Seven pure and
regular-file source-projection cases passed; no host cgroup, actor population,
native-helper birth, current-owner or hard-admission result is claimed.

Coordinator `01a148884a4770fdaf0111f3f792a248`; implementer
`/root/native_program_executor`. Full current origin/main AGENTS, R01–R16,
development-evidence, current handoff and authority decision were read at
`7d7e0855a5073ed5163b4fe7ed3355c9ed7c2506`. Rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Only the new `crates/init/src/domains/admission/execution.rs` and this new
`session-v1/` directory belong to this implementer. Existing primary research,
policy receipts and parent source remain unchanged. No children were started.

## Implementation and exact integration hook

See `API.md` for the full callable interface and exact parent/current-owner hook.
The leaf creates one decimal component from the existing nonzero Worker ID under
the actual held member. It first checks native cgroup-v2 filesystem identity,
then uses the SAME Group/Removal acquisition and opens both existing kill/events
controls with the SAME NOFOLLOW/CLOEXEC opener before returning Prepared. An
existing component is refused; no adoption, controller enabling, limit write,
migration, clone, signal or cgroup event read is performed.

The outer owner MUST publish before sending the Group FD to the helper or
enabling any possible birth. Publication retains the same Group, held parent and
name and returns the existing `managed::Controls` to that same owner. Unknown
birth never uses pre-birth cancellation. Failed control acquisition/removal
returns the actual still-held object, original preparation cause and exact
cleanup outcome. Failed open plus failed rollback retains the SAME unopened
Removal object. An explicit retry makes one removal attempt, never a poll loop.

`remove_after_reap` only attempts rmdir. Actual matching reap plus kernel
`populated 0` remain prerequisites that the real owner must supply. Leader exit,
pipe EOF, a successful kill request, held directory or directory removal alone
cannot establish both. The owner retains actual pidfd, Controls and charges on
uncertainty. Drop is never a refund or proof that the job ended.

Parent refactor before SHA-256:
`a5188b304b7c35668c2c035329269ba1437f675dcaa956fb2a035b4b810151d6`.
Final `candidate-v2/admission.rs` SHA-256:
`de247243e4fae1ce2218b6263e23af8a1b412ad633e2ecfd2e5c2adff4a635ec`.
`PARENT-ACQUISITION-UNAPPLIED-v2.patch` SHA-256:
`9b668c542cb6c74d7d06a33af0f32aa6f0b5254d104aab2b71c107ed661edd28`.
This preserves the ordinary `Group::create` Refusal-only wrapper, introduces one
shared acquisition result retaining the same Removal, widens only the existing
error types for typed consumers, and adds the single execution module token.
The domain re-export and real owner call remain parent-owned and unapplied.
The first candidate/patch and first leaf freeze are retained, not overwritten.

## Functional source versus final compile source

| Exact source | Actual work and outcome |
| --- | --- |
| `cab19d8d8e2a8cb4a92feb67ba20f3f9b5e435909995559b7b19255ef5b53fc9` | Build/list 0; **7 passed, 0 failed, 0 ignored**. Subsequent Clippy failed 101 on test-only `Permissions::from_mode(0)` requiring octal spelling. First batch is not reported wholly passed. |
| `f1dc920bf54cc1d2bb86025f77f5658b9485f83bd26b87febf7a418df80405cb` | Fresh offline locked compile and Clippy 0; targeted final rustfmt 0. Runtime cases **0** in this batch, per coordinator instruction. |

`FINAL-ONLY-COMMENTS-AND-TEST-LITERAL.patch` retains the complete difference:
test `0 -> 0o0` has the same native mode bits, and publication comments clarify
the coordinator's before-FD-handoff ordering. Executable production code and the
functional assertion predicates are unchanged. The seven passes remain attached
to the first exact source; they are not claimed to be a re-execution at final
source. Intermediate `682630b2...` and its package are source-only/unrun.

Both actual batches used normal Kiln admission on
`451397fcc585f7f7cb9aaf00e42064bb`: two CPUs, requested 2 GiB memory/4 GiB disk,
cargo `-j2`, fresh private scope and 600-second absolute batch including cleanup.
They used `sync:false`; no ask reduction, headroom bypass, root install or local
build occurred. The inherited CPU set was restricted to its first two actual
available CPUs. These asks and observed files are not hard resource backing.

The source projection uses the actual candidate parent **production prefix**,
the exact actual `Controls` slice and the new leaf. Existing parent tests require
the actual Launch graph and are excluded from this component projection. No
fake Launch/owner/permission constructor or unused-call shim was introduced.
All 26 Rust unused-code warnings from that unregistered projection remain
visible. Final Clippy denies all Clippy lints, undocumented unsafe blocks and
the repository's panic-related lints; it does not claim `-D warnings` on the
complete production graph.

## Seven actual light cases

| Case | Observed result |
| --- | --- |
| Wrong filesystem | Public acquire refuses the private regular filesystem before creating the Worker component. |
| Missing controls | Private source projection creates a new ordinary directory; missing `cgroup.kill` gives ENOENT and actual removal succeeds. |
| Existing component | EEXIST; existing marker is unchanged and the component is neither adopted nor removed. |
| Failed rollback | A private nonempty directory makes rmdir fail ENOTEMPTY. The same Group/parent/name and ENOENT preparation error remain held. After parent directory rename/replacement, explicit retry removes only the original component; replacement marker remains unchanged. FD count returns `4 -> 4`. |
| Controls and publication | Regular-file control stand-ins are opened before Prepared; kill bytes remain unchanged. After publication and explicit fixture-file retirement, dropping Published leaves the empty directory present. |
| Failed open plus failed rollback | Actual non-root ordinary permission check yields EACCES when opening the newly created private directory, then ENOTEMPTY on removal. The actual unopened Removal is returned and later succeeds after private fixture permissions/content are restored. |
| Explicit cancellation/removal | Failed pre-birth cancellation retains the actual nonempty Group. Explicit retry and a separate no-process state projection exercise the cleanup outcomes. No wait receipt or kernel population proof is fabricated. |

The regular directory/file stand-ins are private test projections of the actual
shared methods; they are not fake cgroups or evidence of kernel control
semantics. The public filesystem gate refuses them. All paths live below the
driver's private TMPDIR through the actual `shards_testdir` implementation.
No host cgroup path is mutated or controller/limit changed. No native actor or
child fixture process is born by these cases.

## Raw records and reproduction

First actual root: `/home/adalundhe/native-execution-scope-cab1-v1`.
`PACKAGE-gnu-v1.tar.gz`: 33,740 bytes, SHA-256
`fdf3ec212493d8b955bff3b143519ff821df8b3dbb42aab5aa3bfa60da47a562`.
Its original source manifest SHA-256 is
`3572868d746682aa7dac1b0dcda68ee773549335eb863890662f4dcdb1b5dc66`.
The first frozen test binary is 8,324,384 bytes, SHA-256
`ba7973052733ae56be881292e47f747274612f5933a706cbd0f7ae7dba5284c7`.
`raw-gnu-v1.tar.gz`: 9,359 bytes, SHA-256
`29eecf699096398b61d690a350620d29d6e044cbdc4999306c23b264b4b0ad93`.
All **19 payloads plus manifest** were checked against the retained manifest;
exact readable copies include the first Clippy error and all unused warnings.

Final compile-only root:
`/home/adalundhe/native-execution-scope-f1dc-compile-v2`.
`PACKAGE-gnu-final-compile.tar.gz`: 30,042 bytes, SHA-256
`af21fb220b109baebd79e394c8b0b760b6e1980fe0023e0ed90dff20ab96124a`.
Final source manifest SHA-256:
`d06ff6e1d5bc6642552c10d1ba42655b7a49410d99087c328a0c249ad09db7c6`.
The compiled, **unexecuted** final test binary is 8,324,392 bytes, SHA-256
`fdb66515c73b45561edb839d5c3c76255ad7ac1f39ee0a77cbae35e5072965fe`.
`raw-gnu-final-compile.tar.gz`: 8,212 bytes, SHA-256
`e870dc197923fc8b032413aa46302f1509f0545dca52bfdd90e01df90527074b`.
All **14 payloads plus manifest** were checked; complete readable copies remain.

Original sources, targets, temporary roots and frozen binaries remain in both
remote scopes. No self-cleanup of source/evidence/cache/targets was performed.
The actual host was amd64 Linux `7.2.9-2-cachyos`, Rust `1.99.1`
(`58cf8f9c35b355199727627d07ae34a4e2846d67`), LLVM `23.1.1`. This is not a
product Linux 6.18 VM result, musl result or seven-target gate. Stage elapsed
times are deadline receipts, not latency/throughput measurements.

For the exact first functional projection, extract its immutable package in a
fresh normally admitted private Linux scope and run `python3 run.py`. For the
final compile-only projection, extract its final package and run
`python3 run-final-compile.py`. No runtime stage exists in the latter. The
original first package is authoritative for its old source manifest; the working
harness has the final source. Each driver checks its own full manifest before
and after work, retains logs/binary, and kills/reaps its own process group on
timeout with explicit uncertainty instead of a pass.

## Decision evidence and limitations

This implements a new explicit ownership requirement, not a claimed reproduced
legacy leak or production bug fix. The new caller needs acquisition to return
the original Removal after failed rollback, so the narrow shared acquisition
result is necessary. The old Refusal-only compatibility caller remains intact.
Alternative independent create/remove/manager code was rejected because it
would duplicate actual Group/Removal ownership and cleanup. No poller, cache,
timer, worker registry or controller topology is introduced.

Read/retained primary sources are pinned in `DEPENDENCIES.json` and the original
`../primary/SOURCES.json`: Linux v6.18 cgroup-v2 contract and cgroup implementation,
systemd v259 delegation and runc v1.2.6 fs2. The kernel's lifetime notifications,
controller-free child topology and kill behavior are distinct from admissions:
https://raw.githubusercontent.com/torvalds/linux/v6.18/Documentation/admin-guide/cgroup-v2.rst
and https://raw.githubusercontent.com/torvalds/linux/v6.18/kernel/cgroup/cgroup.c .
The established single-owner/hierarchy and manager alternatives are at
https://raw.githubusercontent.com/systemd/systemd/v259/docs/CGROUP_DELEGATION.md
and https://raw.githubusercontent.com/opencontainers/runc/v1.2.6/libcontainer/cgroups/fs2/fs2.go .
Their code/configuration is not imported or installed. Every retained original
primary file is rechecked byte-equal at closeout.

Filesystem type and a held directory do not authenticate an owner or grant
clone/migration permission. The actual birth participant's credentials,+cgroup permission checks, no-internal-process constraints, protected naming,+original current Scope/Epoch, lifecycle, audit and hard resources must be proved
by the actual combined owner/helper. This leaf changes no ownership modes,
delegation rights, controls or limits to make a fixture pass. The owner must
remain the sole writer and preserve the Worker/component association through
terminal removal, revocation, restore and move.

## R01–R16 account

| Rule | Evidence and remaining gap |
| --- | --- |
| R01 | New production uses checked existing fallible helpers, explicit matches and native-error return; no assert/unwrap/expect/indexing/arithmetic allocation was added. Final Clippy panic/unsafe checks pass in projection. Existing helper/library allocation, panic and real full-graph paths remain ROOT review. |
| R02 | Actual parent Group/Removal/Controls and pinned kernel/established sources above informed the existing-seam choice. Pure/live regular-file facts retained. No kernel compatibility/cost promise derives from source alone. |
| R03 | Decimal buffer length derives from existing u64 grammar bound; nonzero ID is caller-issued. File names/flags/filesystem magic are native ABI; exclusive create is the contract. Two CPUs/j2/600 seconds are assigned test bounds, not runtime policy constants. |
| R04 | Acquisition has Prepared/Refused/Retained, Holding has Opened/Unopened, explicit publish/cancel/remove outcomes and exact captured error/rollback. None or EOF never grants/reaps/refunds. |
| R05 | Same acquisition/removal implementation and actual Controls; no cache/probe/new event loop. Explicit retry makes one unlink attempt. Parent owns current/notifications/reaping. |
| R06 | Focused staged acquisition, control preparation, publication and cleanup; no generic manager or speculative framework. |
| R07 | Comments identify raw syscall safety and before-FD-handoff/empty-plus-reap obligations. Original source comments/data remain retained. |
| R08 | No 4–5x or comparative claim. ROOT equivalent baselines, distributions and misses remain unrun here. |
| R09 | New ownership behavior, not an asserted legacy leak. Actual regular-file failures/results and first Clippy failure are retained before correction. |
| R10 | Exact source/package/patch/binary identities, full raw manifests, first failure and final compile-only proof retained. No guard-removal or hostile campaign authorized/run here. |
| R11 | GNU amd64 source projection only. Actual pinned guest6.18, musl/arm64, seven targets and real KVM/HVF/WHP combinations are unverified here. Mac support arm64 only; WinARM remains paused. |
| R12 | Existing Linux guest Group/Removal/Controls reused; no new platform manager, helper binary, controller policy, installation or root effect. |
| R13 | One product leaf plus narrow unapplied shared dependency/current caller hook. No fake issuer, rights token or hard quota. |
| R14 | No profiles/benchmarks. Only selected parent FD return observed; whole kernel/parent charges, allocations/faults and comparative costs remain ROOT-owned. |
| R15 | Implementer-only light scope; exact owner handoff sent before final checks. No children/merge/push. Parent registration/current producer/VM validation and ROOT review/adoption remain owed. |
| R16 | Normal fresh remote headroom asks, no local build/actor/cache deletion. All original research, failed source/raw, targets and frozen binaries retained. ROOT Mac lock is untouched. |

Actual owner integration, kernel population/descendant cleanup, clone/pidfd/ACK,
current/revoke/restore/move/audit ordering, hard backing/control survival,
meaningful guard removals, seven targets/VM coverage and comparative costs remain
required coordinator gates.
