# Owned Full return and actual write backpressure

Implementation scope approved by ROOT from ffefcaa57's proposal, retained
byte-for-byte in APPROVED-PROPOSAL.md (SHA256
14f2d802713a22cb191623a5529837825907bf02db848559eac5c461ccb399a4).
Base/main a920c85b5c0c9d40aafc4f4cce32432c7c0aed79; implementer / assigned
adversarial agent 01a1c5d2ee5871a2a9a9cdc9f56793f0. Full policy/plan/handoff read;
rules SHA256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
ROOT 01a148884a4770fdaf0111f3f792a248 retains integration/review authority.

This new branch carries only the scoped 2c57 test caller/shared-client seam and
minimal registration onto a920, then adds passive actual I/O receipts and the
three approved selections. Frozen 2c57 sources/binaries/proof and prior refs are
unchanged. Production source is unchanged. source-scope.json names every source
hash and the exact shim/registration patches; zero-write-instrumentation.patch
isolates the shared-client additions relative to frozen 2c57.

## Contract and authorities

At a920, remote/src/net.rs Io::write maps real QUIC WriteError::Blocked to Ok(0).
Its existing Handler::writable callback supplies readiness. Producer::submit in
remote/src/managed.rs returns the owned Vec in Rejected::Full when its single
slot is occupied; record capacity is charged, and state/progress/capacity remain
authoritative. Local byte receipts are neither application ACKs nor backend
control success. RFC9000 sections4.1/19.8 separate stream bytes and final size;
FIN with empty pending cannot establish a write0. See the approved proposal for
primary links and rejected alternatives (positive-only inference, fake driver,
new pause/coordinator or tuned window).

Only an armed bounded RawControl sink records attempts, actual zero/positive
returns, exact pending suffixes and actual callback connection/stream match.
The same sole Client, pending Vec, flush, real Io and writable callback perform
all work. The zero branch still returns immediately. Existing positive-write
and FIN accounting assertions remain unchanged. Separate receipt validation has
explicit Ready/Attempt/Blocked/Notified states. It requires unchanged nonempty
pending on zero, owned readiness before resumption, contiguous accepted bytes
and final pending0. NoZero remains a coverage miss/failure, never a passing gate.

The Full selection submits one checked in-budget candidate before the actual
Open's ACK, requiring Pending/capacity0, unchanged state/cursors and the same
returned allocation/capacity/bytes. It does not retry that candidate. The original
records continue after actual ACK and the existing credit notification. Native
post-Aborted Subscribe retains supplied errno5 independently of remote Finished
cursors, typed Protocol and owned postterminal Closed return. Owner abort then
finish must remain Aborted/Closed. The VM mirror retains canonical End metadata,
exact fresh neighbor fd1/fd2 and independent main progress, stop/remove/joins and
FD return. Its zero-coverage assertion follows these ordinary witnesses/cleanup.

## Execution and storage

Private Fedora8fadc scope:
/home/adalundhe/managed-current-backpressure-a920-v1/{source,target,cargo-home,
zig-global-cache,zig-local-cache,runtime-cache,artifacts,records}.
Admission2CPU/4GiB/24GiB, cargo-j4; full normal carried-helper caller build<=3600s.
Each selected case has phase240s+cleanup15s and direct hard outer255s, with a new
owned endpoint/run/records. First native Full, then native actual zero/readiness/
positive Subscribe, then one VM mirror. No capacity/traffic retuning or retry if
zero is absent. Source+shim readback precedes builds/actors. No local target/cache.
No held helper-control/worker-result allocation/cotenant-theft cases, root cache,
children, goal dispatch, security changes, paused VM or main merge.

At this source checkpoint, format and scripts/check-development-policy pass.
Full caller compilation, pure receipt checks, strict Clippy and all three actual
cases are PENDING; no source-only pass is claimed. Exact commands, artifacts,
toolchain/platform and raw outcomes will be retained in a subsequent checkpoint.

## R01–R16 evidence and remaining obligations

| Rule | Scoped accounting |
| --- | --- |
| R01 | Test-only changes/assertions; no production panic/error path added. Full compile/lint pending. |
| R02 | Existing actual authority/source and approved primary-source alternatives above; no architecture change. |
| R03 | No production literal/window change. Existing CONTROL_BYTES and actual record capacity derive byte bounds; owner supplies finite deadlines/resources. |
| R04 | Explicit trace phases; typed Full/Protocol/Closed and canonical errno vs remote terminal cursors checked independently. Actual cases pending. |
| R05 | One existing Client/Observer/pending/flush/readiness path. Trace snapshots are bounded test evidence, not protocol state/cache. Existing authoritative credit predicate plus blocking notification retained; no polling. |
| R06 | Focused pure trace checker and selected native/VM callers; no new driver/parser/manager. |
| R07 | Comments identify provenance/invariants; raw original bytes retained. |
| R08 | No comparative performance claim. Equivalent baseline/4–5x objective remains ROOT-owned and unmet here. |
| R09 | No product bug/fix claimed. Actual zero coverage remains pending and may miss. |
| R10 | Signed source scope, original proposal/patches and subsequent exact raw command/result record; mutation/combined proof remains ROOT owed. |
| R11 | Assigned Linux amd64/glibc KVM/native only; static musl guest build pending. Other supported targets/hypervisors unverified here; macOS ARM64 only, paused WinARM untouched. |
| R12 | Reuses shared transport/ABI and canonical capture, no OS duplicate adapter/product deliverable. |
| R13 | Only approved receipt/fixture seam; no protocol/capacity/queue change. |
| R14 | No profiling/benchmarking; allocation/kernel/population costs remain unknown. Pointer equality checks owned refusal, not allocation cost/leak freedom. |
| R15 | Exact assigned finite disposable scopes; signed own branch, no children or main merge. ROOT reviews/combines/mutates/matrix-validates. |
| R16 | Mac floor>30GiB before edits, no local build/large copy; guarded private remote admission, no deletion or protected cache use. |

ROOT owed: combined caller/fixture composition (including moved shared seam),
meaningful guard mutations/restoration, full supported matrix and relevant real
hypervisors, disconnect/revoke/publication ordering/slow metadata and broader
resource/race/durability gates. No P0–P13/G25 or entire-lane completion is claimed.
