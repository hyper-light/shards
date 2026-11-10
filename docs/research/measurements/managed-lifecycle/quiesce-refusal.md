# Typed refusal of a known unready tenant at the run cut

POLICY_READ role=implementer revision=68391c398ffe7d552dff1ec4062cec80362197cc
policy_sha256=0d936117dbc6663eb02829c3e921879cecdad3a8f503047588baa21dc82da936.
Scope: P2/G08/G21's lifecycle eligibility and a narrow P11 refusal at the existing
D176 cut. This is not completion of P11/G20. The coordinator owns all real-VM,
mutation, fault, audit and comparative performance work.

## Decision, sources and alternatives

[Firecracker v1.17.0's RPC](https://raw.githubusercontent.com/firecracker-microvm/firecracker/v1.17.0/src/vmm/src/rpc_interface.rs)
represents snapshot creation as a typed VmmAction and returns a distinct
VmmActionError; success follows successful snapshot creation, with a paused-VM
precondition. [CRIU RPC revision5048](https://criu.org/index.php?title=RPC&oldid=5048)
separates request/response type, success and errno. These are established examples
of refusing a checkpoint through its control result, not ordinary program output.
Their checkpoint mechanisms are not claimed to validate Shards' own cut.

The existing run seam already requests QUIESCE with two private restore tokens
and waits for QUIESCED before warm::save calls the VMM. A dedicated fixed refusal
fits that seam; a second RPC/manager is unnecessary. A SYSTEM_ERR/output message
would not give the save waiter a typed refusal. Killing every tenant on a member
read error contradicts the shared-VM contract. Reporting QUIESCED while a known
tenant remains quarantined would misrepresent missing population proof.

The user requires errors, refusals and responses to be typed. No caller parses a
formatted string. Refused contains the declared member ordinal, checked reason
and errno, plus private request correlation. QuiesceError distinguishes invalid
request, Busy, Ended, NotStarted, transport and Refused, with a typed Protocol error for invalid length, unknown reason or
invalid errno. Its Display formats into
the caller's formatter; the legacy warm::save String-returning boundary still
allocates a presentation string. No claim that the entire error path is allocation
free is made. Tokens are omitted from Debug/Display and ordinary diagnostics.

## State, ordering and ownership

Guest state is Open → Draining → Committing → Quiet. Draining owns the bytes queued
at the request; later output remains in the normal Outbox. After that drain, and
before every attempt that has not written a success byte, all retained tenant
lifecycle states are rechecked. Running and proven Ended are eligible. Starting,
Stopping, Draining, Quarantined and Faulted produce a correlated refusal. The
member comes from Domain.id minus DOMAIN_FIRST_ID, checked before actor birth;
files-only declaration holes do not renumber runnable tenants.

A refusal is a separate fixed control frame and returns to Open. It neither
updates restore tokens nor stops siblings/main nor turns missing population proof
into Ended. The first successfully written QUIESCED byte is this slice's narrow
commit point: restore tokens are committed there. A partial successful header
cannot be replaced by refusal bytes. A zero/WouldBlock write is not commitment
and readiness is checked again before its next success attempt. The existing
empty successful QUIESCED frame bytes are unchanged.

The host state is Idle → Waiting(request) → Quiet or Refused → Idle, with Ended
sticky. A second save is Busy; send failure cancels only its matching waiting
request. Only the primary run relay resolves this waiter, never a joined-exec
connection. A stale refusal cannot complete a new request. Malformed refusal fails
the existing relay, whose Ended state retains the typed decode cause for its waiter; no malformed success is
invented. A complete known refusal reaches warm::save before the VMM snapshot call.

Stop, reap, memory control, host RESUME and output/control polling continue through
Committing and Quiet. **Observation deferral is not freezing; this slice does not
freeze actors or defer control.** A quarantine first observed after commitment
remains Quarantined and known in the live state. It is not reclassified ready.
The cut does not yet revalidate that post-commit state against host snapshot
publication. Full freeze/revalidation, capture and host-lease ownership transfer,
generation handling and external-effect uncertainty remain P11 work. Existing
success replies have no correlation payload for compatibility; only the new
refusal is correlated. This is not a universal stale-success defense.

## Literal derivations (D176)

| Value | Derivation and failure behavior |
| --- | --- |
| Run kind46 | Next unused discriminant after capture's reserved43–45; protocol identity, not a timeout/size tuning value. ABI identity includes the new source. |
| Reasons1–5 | Distinct serialized variants Starting, Stopping, Draining, Quarantined, Faulted; unknown values refuse decoding. |
| Request length | Two existing TOKEN fields, no new secret or token length. |
| Refusal length | Request bytes + u32 declared member + u8 reason + i32 errno, big-endian numeric fields; exact length required. |
| Fixed frame storage | Existing HEADER + exact Refused::LEN, reserved in the enum's fixed stack/state fields; no new queue capacity or heap allocation. |
| Zero/nonzero errno | Lifecycle-only reasons require zero; quarantine/fault require a positive errno. Invalid local combinations fail control handling instead of becoming success. |
| EPROTO | Linux protocol error for conflicting/inconsistent control state; EISDIR in tests comes from an actual failing directory pread in the prior native fixture. |

## Repeatable light checks and limits

```sh
cargo test -j4 -p shards-abi quiesce::
cargo test -j4 -p shards-init cut_tests::
cargo test -j4 -p shards-init managed::tests::
SHARDS_HELPERS=skip cargo test -j4 -p shards-vm-process quiesce_
cargo clippy -j4 -p shards-init -p shards-abi --all-targets -- -D warnings
SHARDS_HELPERS=skip cargo clippy -j4 -p shards-abi -p shards-vm-process --all-targets -- -D warnings
scripts/check-development-policy
```

Linux native unit logs record ABI2/2, cut4/4 and lifecycle10/10, including the eligibility
map. The cut cases use real private Unix sockets for earlier-byte drain, refusal,
continued output and token commit. A constructed partial-header state tests reply
immutability; it is not a real-VM partial-write campaign. The additional actual
full-socket case verifies WouldBlock retains a pre-commit state and a later
refusal replaces a still-unwritten success. Mac arm64 host units3/3 and warnings-as-errors lint cover Busy,
stale refusal, cancellation ownership and sticky Ended.

The first extra socket test did not compile because Read and Write both define
by_ref; it was qualified as Read::by_ref before the retained final check. Clippy then
required the new test module to follow production items; it was moved to the end.
The typed decode change caught an accidentally duplicated refusal arm in the
joined-exec reader; that unrelated arm was removed before the final host checks. No live
production defect is alleged from that test-code compile error. One Linux check
was refused by the headroom guard; it was not bypassed. Local Mac arm64 checks
use lockf/-j4 above the owner's30GiB floor. Exact hashes, environment, outcomes
and raw light logs accompany this record.

Coordinator acceptance still owed: real mixed-tenant quarantine → save/move typed
refusal, healthy sibling/main challenge afterwards, removal of the readiness guard
as a meaningful mutation, quarantine while draining/blocked/before first reply
byte/after commitment, owner/relay/host loss, malformed/stale frames and cross-target
plus supported KVM/HVF/WHP runs. Full P11 consistency and performance remain open.

## R01–R16 delta account

| Rule | Evidence / remaining proof |
| --- | --- |
| R01 | Fixed record/frame arrays, checked lengths/reasons/errno/member subtraction, typed errors, poison-tolerant existing lock. Legacy String presentation remains disclosed; root implicit-path review owed. |
| R02 | Pinned Firecracker and CRIU RPC alternatives plus existing D176 consumers; root-approved transition sketch. Real-VM acceptance remains root-owned. |
| R03 | Literal table above; no timeout, retry count, admission entitlement or learned authority change. |
| R04 | Guest Open/Draining/Committing/Quiet and host Idle/Waiting/Quiet/Refused/Ended; no partial-success/refusal transition. |
| R05 | Deepen one existing save/control seam; no new manager or generic framework. |
| R06 | Focused frame, eligibility, reply-write and host outcome functions; root reviews composed complexity. |
| R07 | Comments cover commit/FD/protocol obligations, not narration of every state. |
| R08 | No achieved ratio or low-cost claim; equivalent4–5x baseline comparisons owed. |
| R09 | New-path control feature; real socket cases prove only specified light behavior, not an inferred old exploit. Root owns actual-VM ordering/fault reproduction. |
| R10 | Commands, raw logs, hashes and explicit unverified fullP11/live/mutation gates retained. |
| R11 | Native Linux guest and Mac arm64 host light checks; remaining seven-target/actual hypervisor coverage unverified. Intel Mac unsupported. |
| R12 | Same run ABI and guest code on all hypervisors, shared host workload seam, no binary/shell/host-root/crypto changes. |
| R13 | Fixed control outcome at real existing consumers; no parallel checkpoint authority. |
| R14 | No measured allocation/latency claim. Fixed fields avoid introducing a new heap queue; whole-path profiling including legacy presentation remains root-owned. |
| R15 | Scoped implementation/light tests only, no child agents/self-merges/heavy runs. Root audits, profiles and batches reviewed commits. |
| R16 | Guard refusal honored; no compilation under30GiB. Source/evidence/user data preserved; owned caches and jobs reported. |
