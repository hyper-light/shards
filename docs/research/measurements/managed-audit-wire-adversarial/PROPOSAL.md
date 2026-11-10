# One native framed private-audit extent fixture — preparation only

This is a new P7/P9 fixture proposal. No native build, fixture registration,
mount, VM, actor or storage-fault campaign has run. No old FIN result is inherited.
ROOT must review the exact fixture/input closure before execution. The current
assignment authorizes only this directory and unapplied source proposals.

POLICY_READ: implementer / assigned adversarial, origin/main
`ca3ab10c151aa5539e757f1cc76029746ccbb561`, rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Full AGENTS, R01–R16, evidence, CLAUDE, managed plan and updated handoff read,
including the handoff's combined0ae and engine-relay records. Coordinator is
Kiln `01a148884a4770fdaf0111f3f792a248`; this lane is
`01a1c5d2ee5871a2a9a9cdc9f56793f0`. No delegation, goal or child was started.

## Exact source, ownership and constructor readback

The new sparse worktree `managed-audit-wire-adversarial`, branch
`p/managed-audit-wire-adversarial-v1`, starts at signed candidate
`8c817dbaf3e1bac2f038c83175c4f690580785a0`. It preserves the separate FIN
worktree, `9764144d` source, `ab5d5820` binary and all old refs/records.
Its only edited paths are new files beneath this measurement directory.

ROOT supplied frozen constructor refinement
`ba78d4ba775e7d83b7fea9a5989e3ca6b9857a2a` and the full API document at
`8ac004cc1b91f5bfd0269260f5eacc9497de0ffc`:
`docs/research/measurements/managed-audit-producer/NATIVE-FUSE-FIXTURE.md`.
These immutable Git objects were read without touching the audit owner's active
worktree. See [source-scope.json](source-scope.json) for exact blob/SHA256 records.
The narrow, UNAPPLIED [constructor-refinement.patch](constructor-refinement.patch)
retains only ba78's three-file constructor/typed-status delta on8c. No other
startup, native Hand, broker dispatcher or mounter changes from that branch are
proposed for composition.

The actual API is:

```rust
use shards_abi::run::audit as wire;
use crate::devices::virtio::fs::{self, host, proto, server::Server};

// Existing concrete APIs, not proposed wrappers:
// StorageRequest::new(u64, Retention::UntilOwnerDeletion) -> Result<_, wire::Error>
// ExportConfig::new(StorageRequest, Vec<Scope>) -> Result<_, wire::Error>
// Server::audited(host::Owned, &ExportConfig, u64) -> Result<Server, audit::StartFailure>
// Server::audit_readiness() -> Result<wire::ExportStatus, wire::Error>
// fs::answer(&Server, fs::Stream) -> std::io::Result<()>
// fs::write_frame(&mut impl std::io::Write, &[u8]) -> std::io::Result<()>
// fs::read_frame(&mut fs::Stream) -> std::io::Result<Option<Vec<u8>>>
```

On the proposed ARM Mac host, `fs::Stream` is the existing `ShareStream` Unix
stream. Use one real pair and one owned thread running actual `fs::answer` with
the held `Server`. The fixture constructs literal ordinary FUSE frames and uses
the existing frame reader/writer and shared audit Request/Reply codecs. It does
not call `Server::handle` in place of the framed path, construct `audit::Run`
directly, reach private ledger state, fake an IO ticket or add a backend.

`Server::audited` owns the actual private directory descriptor. Its fallible
common constructor preserves `StartFailure::Directory(DirectoryNative(errno) |
NotDirectory | StateCapacity)` separately from `StartFailure::Writer(...)`.
Construction exclusively owns unpublished state; there is no runtime ledger lock
across its file/directory barriers. `audit_readiness()` must actually return
`Ready { remaining: B - 2*H, objects: 2 }`; file existence is insufficient.
The native fixture supplies a disposable owned directory, not an actor grant or
a production storage issuer. Durably naming a real production root remains a
parent obligation; this fixture does not establish it.

## Smallest registration — unapplied

[include.patch](include.patch) adds only these lines to `crates/vmm/src/lib.rs`:

```rust
#[cfg(all(test, target_os = "macos", target_arch = "aarch64"))]
#[path = "../../../docs/research/measurements/managed-audit-wire-adversarial/native.rs"]
mod managed_audit_wire_adversarial;
```

`native.rs` is intentionally not implemented or registered in this checkpoint.
The proposed one test is
`managed_audit_wire_adversarial::full_fuse_frames_share_one_retained_audit_budget`.
No Cargo/dependency/public-API token is needed by this portable VMM fixture.
The include is an exact proposal for ROOT review, not evidence of compilation.

## Codec inputs and aggregate B

ROOT's assigned codec corpus uses two runnable stable member IDs1 and3. ID2 is
the files-only declaration between them in the fixture's checked input table.
Selecting executable entries preserves these supplied stable IDs, producing
exactly the ordered scopes `[1, 3]`; it never renumbers the filtered table. Both
scopes share the same fixture owner/run/nonzero generation/static policy. They
are synthetic checked binding data, not minted production rights.
`Scope::member_name()` must yield `00000001\0` and `00000003\0`. The held export
directory contains exactly those two files; it has no `00000002` journal.
This native table does not prove normalized-image activation or guest launch.

Use actual existing broker record encoder outputs and bounds for one fixed,
nonsecret receipt input, rather than picked `abc`/`ok` write lengths:

- `Imax` is the real `AuditReservation::reserve` intent count using the same
  `intent_record`, `Count`, `usize::MAX` packet bound and
  `Some((u64::MAX, u64::MAX))` object fields as the frozen broker.
- `Tmax` is the maximum actual `terminal_record` count over the frozen
  `terminal_shapes()` with that same receipt and `audit::Status::IntentSynced`.
- The actual intent and terminal data are smaller, well-formed outputs of those
  exact encoders. Select a terminal `Effect::NotStarted` / failed outcome, so no
  serialized sample is presented as an observed peer/tool effect. Preserve the
  exact encoder input, wide-shape outputs, ordinary outputs and source hashes.

**Dependency before implementation/execution:** these semantic encoders are
private in Linux-only `crates/server/src/broker.rs`; the ARM Mac VMM library
does not expose them. ROOT has assigned the audit owner authoritative cfg(test)-only
emission from these SAME encoder/count/shape functions in a separate finite Linux scope.
Actual emitted bytes/bounds have not arrived; this proposal remains symbolic. The
preferred input is checked-in exact bytes emitted by the unchanged real codec,
plus a source/input/length/hash manifest. The portable case derives `Imax` from
the actual wide-intent byte length and `Tmax` from the maximum materialized shape
length, checking both against the original counted receipts. No guessed numeric
bound, copied JSON encoder, second parser or production extraction is proposed.
Corpus SHA256/manifest checks occur during source freeze outside the test binary;
the proposed leaf includes exact byte data and emitter-generated integer counts.
No corpus was generated in this preparation task, and no missing bound is passed.

For the eventual exact corpus, all arithmetic/conversions are checked:

```text
H = wire::JOURNAL_HEADER_BYTES
  = (1 version + 108 Scope) + (1 retention + 8 B) = 118
O = wire::SLOT_OVERHEAD = 3*8 immutable fields + 2*8 length prefixes = 40
S = O + Imax + Tmax
B = 2*H + S                    // ONE aggregate run storage request

initial file lengths: H, H
Ready.remaining: S
after member1 Reserve: remaining = 0
member1 file length: H + S; member3 length: H
intent_offset = H + 3*8 + 8
terminal_offset = intent_offset + Imax + 8
final retained file-length sum = 2*H + S = B
```

Both `JournalHeader::decode()` results must hold that exact SAME `StorageRequest`
including B and `UntilOwnerDeletion`. B is never copied into independent member
entitlements. Unused maxima/prefix extents remain charged; actual record lengths
do not refund them. Assert nonzero ordinary lengths, `Ilen <= Imax`, `Tlen <=
Tmax`, and positive retained slack from the real corpus before actor start.
All complete WRITE requests must fit `proto::MAX_WRITE` and `fs::MAX_FRAME`;
unsupported corpus sizes fail preparation instead of changing a production cap.

The descriptor-cache budget is fixture input0, matching the existing actual
writer tests: transient node cache trim must not discard the pinned writer
objects. It is not a file entitlement or a measured resource optimum.

## One decisive request and observation sequence

Every ordinary frame carries the real40-byte FUSE input header and exact body;
`fs::write_frame` adds its existing4-byte LE length. Give each request a checked
monotone unique ID and validate the literal16-byte reply header's length, signed
Linux error and same unique ID before decoding its exact payload. A successful
transport write is not a successful audit operation.

INIT is sent once, version7/minor45, no optional flags/extensions. LOOKUP names
come from actual `Scope::member_name`; OPEN uses full `fuse_open_in` with O_RDWR
and zero open_flags. Its actual reply must grant a single fh and FOPEN_DIRECT_IO.
No second connection, reinitialization, reopening or resubmission on failure.

An audit IOCTL body has the actual fh, flags0, `wire::IOCTL`, ignored arg0,
both sizes `wire::CONTROL_BYTES`, then `Request::encode()` bytes. Require outer
success, the16-byte zero `fuse_ioctl_out`, exact176-byte payload, and
`Reply::decode()` typed result with the correct bound scope. Typed Exhausted is
not inferred from outer errno or text. WRITE has the complete40-byte arguments
and the actual codec bytes; require exact8-byte `fuse_write_out` with the full
accepted count and zero padding.

| Phase | Exact action | Required independent observation |
| --- | --- | --- |
| Construct | Open real private root; construct audited Server and obtain readiness | Exactly two118B files; actual Ready objects2/remainingS; decoded headers same aggregateB and expected distinct scopes |
| Hold witnesses | Open both actual files read-only once, retain their descriptors | Actual device/inode/nlink/length; initial exact raw bytes; these same descriptors read all later changes |
| Bind | One INIT, LOOKUP/OPEN for both members, then Identify each | Outer framing valid; typed Empty/None; no lease and S remaining; scope1 versus scope3; header bytes unchanged |
| Reserve | Member1 Reserve call0 with Imax/Tmax | Reserved/None; exact maxima/call/offsets; accepted0, remaining0; lengthH+S; second file byte-identicalH |
| Duplicate reserve | Same member/call/maxima, new FUSE unique ID | Exact same typed Reply, whole files byte-identical and same lengths; no second debit/slot |
| Intent | One contiguous WRITE at actual intent_offset, ordinary codec intent | Exact acceptedIlen; Inspect IntentWritten/None/acceptedIlen/remaining0; only intent range changes |
| Intent commit | CommitIntent call0, then its intentional duplicate | IntentSynced/None/accepted0; actual BE64 Ilen at intent_offset-8; exact intent bytes; duplicate Reply/files identical |
| Terminal | One contiguous WRITE at actual terminal_offset, ordinary codec terminal | Exact acceptedTlen; Inspect TerminalWritten/None/acceptedTlen/remaining0; only terminal range changes |
| Terminal commit | CommitTerminal call0, then its intentional duplicate | TerminalSynced/None/accepted0; BE64 Tlen prefix and exact terminal bytes; duplicate Reply/files identical |
| Close | Close call0, duplicate Close and Inspect | Closed/None; call/maxima/offsets retained, accepted0/remaining0; all file bytes/lengths identical across these controls |
| Shared refusal | Member3 Reserve call0 with same Imax/Tmax | Outer FUSE success but typed Stage::Empty/Cause::Exhausted, accepted0/remaining0, no lease; its header and member1's entire retained file unchanged |
| Neighbor integrity | Identify member3 after refusal | Empty/None/remaining0 with its own scope, no lease; both held descriptors still reference same objects; no names added or data removed |
| Release | Full FUSE RELEASE bodies for the two actual fhs | Actual success; exact retained bytes/lengths before owner deletion; no claim that RELEASE/Close refunded storage |
| End | End/drop client stream through existing IPC end handling and join answer thread | Actual server result and join; drop Server and read witnesses; retain copied evidence outside disposable root before its owner deletes only that root |

Intentional duplicate controls are selected finite protocol inputs, not recovery
retries. Never duplicate the WRITE bytes at an advancing cursor. The sequence is
serial; it cannot establish concurrent progress or stalled-I/O fairness.

Capture the complete file after Reserve as the slack reference. Compare the
immutable header/call/maxima and every unmodified interval after each operation.
Final member1 contents must contain the exact24B call/maxima tuple, both actual
BE64 length prefixes and the two exact codec payloads at checked reserved offsets.
Unused tails equal their post-Reserve bytes. Do not infer physical zeroing or
allocation from a logical extension. Both paths and retained descriptor metadata
must still identify the original files; no reopen replaces a witness.

## Cleanup, bounds and forbidden effects

The eventual case uses one private `shards_testdir::TempDir`, one existing stream
pair, one actual Server and one joined answer thread. No guest/helper/process
worker is started. No mount, namespace, privileged syscall, host-root operation,
security/trust change, ordinary volume edit, remote peer or device is requested.
The test's read-only witness descriptors observe only its two new files.

Normal and error paths end the one client stream before joining the answerer;
an assertion is delayed until cleanup receipts can be saved. Use the existing
`shards_ipc::end` and actual ownership drops. Do not add an event loop, retry,
timer thread, pool, audit state replica or manager. Retain exact error and native
cause; constructor or barrier failure is a failure, never a weaker sync fallback.

Before each request, use the remaining ONE absolute30s case deadline for stream
timeouts. No per-operation fresh window or sleep. The external parent enforces
30s work plus15s cleanup, hard45s. If a real file operation stalls past the bound,
retain Timeout and unknown join/cleanup; no second case, larger timeout or claim
of a successful join. The intended entire admitted native scope is600s,
2CPU/2GiB/4GiB, j2, private build maximum180s. This envelope is a caller input,
not a production resource constant or proven hard kernel charge.

No runner, target, build or case is allocated now. Later use one new writable,
normally admitted macOS ARM64 private source/target/records/artifacts scope,
never a protected ROOT cache or the old FIN target. Resolve and record the exact
runner/home/paths before dispatch, then recheck30GiB Mac floor and the existing
`/tmp/g121-mac.lock`. No local heavy build, cache deletion, read-only Fedora home
write, paused WinARM use or headroom bypass. Admission failure is Held, not Run.

Copy original frame requests/replies, file snapshots, metadata and typed stage
receipts into a separate new records directory before `TempDir` destruction.
Check actual owned stream/reader descriptors after their final drops, without
opening a replacement descriptor in between; record EBADF only when actually
observed. Record answerer join and private-root absence after explicit owner
deletion. A missing cleanup witness remains Unknown. No whole-process FD leak,
population/kernel-return, semantic writer/death or power-loss claim follows.
Preserve all evidence/source/binaries, even on failure. No cache cleanup is part
of this task.

## Provenance and later commands — unexecuted

Before actors: freeze source commit, exact selected constructor/include/leaf
diffs and hashes, codec corpus source/inputs/counts/materialized bytes, policy
hash, source status, Rust/Cargo/SDK/kernel/architecture, filesystem/device facts,
runner/admission and all private paths. Record every command/result and separate
stdout/stderr, build output and artifact hashes. Assert `--list --exact` selects
one test, then run that one native case with `--exact --nocapture --test-threads=1`.
Neither list/no-run nor a pure test is an executed endpoint/file proof.

Intended component build is `cargo test -p shards-vmm --lib --profile test-release
--target aarch64-apple-darwin -j2 --no-run` with only the private target and the
explicit180s outer build bound. Targeted strict Clippy must cover the actual lib
test caller, not a standalone reimplementation. Exact executable path/hash comes
from Cargo's real build messages. Those commands and the case remain UNRUN.
No blanket VMM corpus, matrix, benchmark, VM or mutation is authorized here.

## Established alternatives and limits

The existing writer tests in `fs/audit.rs` directly invoke `Server::handle` and
exercise real files. They supply useful state expectations but bypass the framed
`fs::answer`/ShareStream path; passing them is not this requested proof. The chosen
native fixture uses that public existing path. A host FUSE mount or real guest
would add kernel/device/startup obligations and is deliberately ROOT-owned.
A second in-memory journal, parser or fake filesystem would not observe the
requested held-file effects and is rejected.

For record payload reuse, prefer exact original codec output with provenance.
Copying the private encoder into this fixture would drift; moving it to a shared
production module is broader than the assigned fixture. Including the whole
Linux broker on Mac would require unrelated server/native-effect stubs. Neither
is proposed. An unverified corpus remains an explicit execution dependency.

Primary protocol source is Linux v6.18
[FUSE UAPI](https://raw.githubusercontent.com/torvalds/linux/v6.18/include/uapi/linux/fuse.h)
(`fuse_in_header`, `fuse_out_header`, `fuse_open_in`, `fuse_write_in`,
`fuse_ioctl_in/out`, `fuse_release_in`). Actual Shards codecs and server code,
with immutable hashes in the source manifest, determine this fixture's framing
and state contract. The native adapter calls real F_PREALLOCATE/F_FULLFSYNC;
[Apple's fcntl manual](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/fcntl.2.html)
also distinguishes preallocation and a device flush request. Successful calls
and matching bytes are not independent proof of power-loss durability or physical
quota; this is an inference boundary, not an additional claimed gate.

ROOT retains actual VM, stalled-I/O/control-progress, ENOSPC/crash/lost-reply,
mutations/restoration, physical/parent/kernel accounting, profiling/benchmarks,
supported seven-target integration and main merge. Fairness, mandatory default
audit activation and P0–P13/G01–G25 remain OPEN. All earlier automatic-review
holds remain held; this proposal never exercises helper-malformed control,
allocator refusal, observer reconnect, ptrace, FD theft or cotenant takeover.

## R01–R16 preparation account

| Rule | This checkpoint and remaining proof |
| --- | --- |
| R01 | Documentation/unapplied patches only; no production panic path changed. Later checked arithmetic/fallible owned setup and strict actual caller lint required. |
| R02 | Exact frozen owner API, real existing direct/framed paths and primary protocol/native docs inspected; alternatives above. No new transport architecture chosen. |
| R03 |118/40/176/40/16 derive from checked codec/UAPI fields; B derives from actual codec bounds; no bound guessed. Time/resource values are user-assigned finite fixture inputs. |
| R04 | Explicit typed stages, duplicate results, accepted cursors, aggregate remaining and retained Close transitions above; actual state sequence still UNRUN. |
| R05 | Reuses one Server/ledger, one ShareStream pair and existing end/join; no new cache/poll/manager. Existing serial transport's stalled fairness remains unproved. |
| R06 | One selected finite native sequence; no generalized campaign framework. Exact eventual leaf needs review. |
| R07 | Proposal comments explain authority/provenance and retained extents; no source comment changes. |
| R08 | No speed/resource ratio claim or benchmark. Whole-path comparative4–5x target remains ROOT work. |
| R09 | No production defect inferred or fixed. This is an unverified boundary fixture; failures must be preserved before any later corrective proposal. |
| R10 | Source hashes, exact unapplied patches and this contract are durable; live frames/files/cleanup and raw evidence are not yet available. |
| R11 | Intended native macOS ARM64 only; other hosts/libcs/real VMs/seven targets UNRUN here. Paused WinARM untouched. |
| R12 | Existing platform ShareStream, host::Owned, audit ABI and server; no OS duplicate/backend/helper. |
| R13 | Minimal cfg(test) include and one proposed test; semantic corpus dependency explicit rather than new serializer. |
| R14 | No profiling, allocation/fault or leak-free claim. Parent/kernel/physical costs and durability are unmeasured. |
| R15 | Scoped preparation only, ROOT API confirmation received; no actors/delegation/main merge. ROOT approval and later assigned actual native run still owed. |
| R16 | Separate sparse worktree; no full copy/build/cache deletion. Old FIN source/artifacts/raw and audit owner's worktree preserved. Local floor receipt retained. |
