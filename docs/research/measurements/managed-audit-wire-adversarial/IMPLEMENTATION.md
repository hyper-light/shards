# Frozen native leaf for ROOT review — no build or actor

The explicit implementation GO follows ROOT's full read of ce992's177-line frame
and cleanup contract. The smallest cfg(test) include is now applied only in this
private candidate's `crates/vmm/src/lib.rs`; the one test lives in [native.rs](native.rs).
The active audit writer worktree is untouched. Only the test include, this owned
fixture and its evidence change. There is no new production API/backend/queue.

POLICY_READ renewed after the local Kiln restart: full main
`ca3ab10c151aa5539e757f1cc76029746ccbb561` AGENTS, rules R01–R16, evidence,
CLAUDE, managed plan and current handoff; rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Role is implementer / assigned adversarial. ROOT delivery receipt8295 records
exact scope and execution holds. Source-only work has no active target/cache.

## Constructor dependency and corpus provenance

This branch remains based on8c817dba plus the signed proposal checkpoints. The
frozen ba78/8ac constructor contract is the required dependency. The exact
three-file [constructor selection](constructor-refinement.patch) remains
UNAPPLIED: this lane does not edit audit-owner production files. ROOT must compose
that selection, or supply its exact composed source, before any build. A build
of the old common constructor is not a build of the specified candidate.

The source-only corpus emitter at `6b86176f7` was inspected through immutable Git
objects (`broker/corpus.rs` and the corpus README), never an active writer tree.
It emits both scopes1/3 and all four existing operations through the actual
encoders/counts/shapes/reservation. ROOT reports r197 emitted once successfully;
this lane has not received/verified its finalized archive/manifest or payloads.
No source-only count or message's numeric maximum is copied into this leaf.

The leaf selects the original `member-1-Send` and `member-3-Send` data, derives
Imax from the original materialized intent-max length and Tmax from every actual
materialized terminal shape, and checks a dense shape-file sequence. It decodes
the original Scope and ExportConfig with the shared ABI and requires the original
ordered scopes. The original all-eight-call allocation is retained as provenance;
the test constructs ONE selected aggregate B=2*H+S1, where S1=40+Imax1+Tmax1.
Member3 requests its own original Send maxima, with zero remaining after the
first member's slot. No assumption that the two selected maxima happen to equal
is needed and no independent B per member is created.

**Three separate IDs:** audit lease call0 is the actual private Journal's first
call; original broker receipt correlation1 is immutable codec data; FUSE unique
IDs1..24 correlate the24 actual frame exchanges. No encoded broker byte is edited
to make these values equal.

The emitter's ordinary terminal uses synthetic Completed/KernelAccepted inputs.
This supersedes the earlier illustrative NotStarted/failed-terminal suggestion in
the proposal/input-contract document. The leaf will consume the original actual
bytes unchanged; it never executes a peer/tool operation or labels those sample
fields as a live effect receipt. Exact manifest/source/scope/operation/request/
shape/count/hash verification remains required before build or actors.

## Implemented observations and cleanup

`sequence` spells out the exact24 ordinary requests using actual `fs::write_frame`,
`fs::read_frame` and the one `fs::answer` thread. It sends complete INIT, LOOKUP,
OPEN, IOCTL, WRITE and RELEASE bodies. It checks reply lengths, matching unique
IDs, outer Linux errno and the independently decoded typed audit result. There
is no raw host ioctl, FUSE mount, namespace, VM or new protocol implementation.

The two actual files are opened read-only once as witnesses. Initial headers
decode to their distinct scopes and the SAME selected B/retention. A whole-file
snapshot follows Reserve and every duplicate/commit/Close/refusal/release boundary.
Appends may change only exact record bytes; commits may change only their actual
BE64 length prefix. All other bytes, slack and the neighboring header must remain
equal. File lengths sum to exactly B. Close and RELEASE do not refund extents.
Member3 must receive typed Empty/Exhausted, then identify as its own Empty scope.
These checks do not count identical overwrite syscalls or establish durability,
physical quota, fairness or semantic effect execution.

After any sequence result, the client is ended with existing IPC end handling
and dropped BEFORE joining the answer thread. Immediate exposed-descriptor EBADF
checks precede new opens; the actual join Result is retained. The actual Server
is dropped even if setup/sequence failed; owned root/file witness descriptors and
the final private-root absence are checked. Hidden writer/kernel populations are
explicitly not independently counted. Late phases are unreached on an earlier
failure, never synthesized from expected state.

Requests, replies, typed states, held-file snapshots and separately labelled path
salvage are create-exclusive records outside TempDir. First actual sequence errors
are retained while cleanup progresses. Trace write failures are also written to
stderr; unavailable raw data remains an evidence failure, never a passed receipt.
Filesystem/record-device failure and an externally killed process can still leave
cleanup or raw retention Unknown; ROOT's storage/stall/crash campaigns remain owed.

The shared TempDir's native soft-FD-limit preparation is disclosed and expressly
allowed only in this new scope. Actual before/after limits are recorded. TMPDIR
must be the new empty absolute private parent, so its automatic stale-directory
sweep has no old source/data input. No entitlement/headroom increase is inferred.

## Exact dispatch input proposal — NOT dispatched

The test is ignored in ordinary runs so an unreviewed matrix run cannot start it.
The eventual exact filter is:

```text
managed_audit_wire_adversarial::full_fuse_frames_share_one_retained_audit_budget
--exact --ignored --nocapture --test-threads=1
```

ROOT must review the matching actual Cargo executable/hash and one selected test
from `--list --exact --ignored` before the case. Environment consists of explicit
absolute `SHARDS_AUDIT_WIRE_CORPUS` (original verified corpus),
`SHARDS_AUDIT_WIRE_RECORDS` (new absent output directory) and TMPDIR (new empty
private directory). Their exact runner/path/archive/manifest/source hashes are
not yet available, so there is NO executable dispatch receipt or implied approval.
The fixed input work deadline is30s; the reviewed external parent must enforce
30+15 cleanup/hard45s with no case retry. Stream timeouts use remaining work time,
and a late response cannot pass; file/join stalls still need the external bound.

The eventual original scope stays ARM64 Mac2CPU/2GiB/4GiB/600s/j2, build180s,
work30s+cleanup15s/parent45s, normal admission, private target,30GiB floor and
existing lock. No runner/target was allocated, no local build, cargo, Clippy,
native case, VM, mutation, allocator hook, profile or held campaign was executed.

## Source checks and R01–R16

Canonical rustfmt with `skip_children=true` under `/tmp/g121-mac.lock` returned0;
this parses/formats source and is NOT compilation. Policy script returned0.
Actual full caller/native Clippy and the one-case runtime remain UNRUN/HELD.
The new source/diff/hash/command record accompanies this checkpoint.

The R01–R16 account in [PROPOSAL.md](PROPOSAL.md) remains applicable, with this
source delta: R01/R04/R06/R07/R13 use checked byte fields and explicit typed
assertions/errors/cleanup in a cfg(test)-only fixture; R02/R03/R05/R12 reuse the
reviewed actual constructor/frame/codecs and original data bounds; R09/R10 make
no production defect/fix/pass claim; R08/R14 remain unmeasured and ROOT-owned;
R11 native compilation/hosts/matrix are unrun; R15 preserves review/build/actor
holds and no children/main merge; R16 preserves old FIN976/ab5d and all old raw,
active/protected caches and paused WinARM. Overall managed-runtime closure is OPEN.

Mechanical source-preparation failures are not runtime reds: the first sparse-add
command used an unsupported `--no-cone` option and failed before edits; corrected
addition used the already configured non-cone mode. A patch context failed after
rustfmt and was reapplied against exact readback. No original source or campaign
record was restored/deleted. The interrupted pre-restart read/message cells no
longer existed after restart; initial clean status confirmed no leaf edits then.
