# Bounded native operation records: decision v1

ROOT coordinator 01a148884a4770fdaf0111f3f792a248, 2026-11-11. POLICY_READ main
bff3015182ad754274f1048ce393ac12cb6cd91f, rules
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3. This selects the
shape the two research notes left open and assigns each lane's piece. It is a
decision record, not an implementation or a passed gate.

Inputs: [retirement research](managed-native-retirement-frontier-research-v1.md)
(the conditions for forgetting an issued ID), [backing research](managed-record-backing-research-v1.md)
(what actually acquires storage), the ordering counterexample in
[managed-native-records-v1](measurements/managed-native-records-v1/README.md), and
ROOT's audit of 2026-11-11 (runtime finding F5: whole Job history is unbounded and
each Prepare copies it).

## Problem

The native Owner keeps every Job forever: it is the only duplicate guard, so
removing records today would let a stale or repeated Prepare re-execute. Its
storage is an ordinary growing `Vec`, not acquired backing, so the daemon cannot
admit the owner's `pending_operations` ceiling (it refuses every Envelope
dimension as `UnprovedCoverage`, `managed_admission.rs`). The research rules out a
maximum-arrival floor (independent bodies reorder first Prepare; burned IDs never
arrive) and a floor taken from a later `Retired`.

## Decision: fenced first publication, live exceptions in place

**Records.** One guest-scope pool of exactly `P` operation records, where `P` is
the guest Envelope's `pending_operations` (owner DATA; R03: no default, no derived
multiplier). Init acquires the pool's bytes at protected startup from a ROOT
locked region (`shards_ipc::locked_region`, branch `p/managed-native-regions-v1`):
`P` records of the consumer's actual record layout, computed with checked
arithmetic. Without a declared `P`, native operations are refused with the existing
typed `Refusal::ResourcesUnavailable`; nothing grows. The daemon may admit
`Dimension::PendingOperations` for the guest only once the guest reports that the
pool was acquired (coverage proof), never from the ceiling alone.

**Slot states.** `Free -> Live(id) -> Retired(id) -> Free`.
- A first Prepare takes a `Free` slot or is refused `ResourcesUnavailable` before
  any effect. The ID is then resolved (refused) for the issuer.
- A slot stays `Live` through every existing obligation: birth or NotBorn,
  reap, population and removal, Complete, audit. Unknown birth or failed cleanup
  stays `Live`; no fence frees it.
- `Retired` (the Owner has sent `Retired`) keeps the slot only for duplicate
  detection, until a fence covers its ID.

**Fence.** The issuer certifies that *first publication* is resolved for every
native ID up to and including `F`: each such ID's first Prepare was delivered and
answered by the Owner, or it was definitively never sent (code-mode IDs and burned
IDs resolve at issue or send failure). This is RAMCloud's acknowledgement of a
resolved prefix, applied to first publication rather than to results.
- Wire: one request `Fence { through: u64 }` from issuer to Owner, relayed by the
  helper, answered `Fenced { through: u64 }`.
  - The helper relays a fence only after it has accounted for every Stage it
    accepted with ID ≤ `F`: either its Prepare was sent and answered, or the Stage
    was cancelled before any Owner send.
  - Tags come from the native ABI owner, chosen from values unused in both
    directions of the shared runtime demux (1–8, 9–15 and 16 are taken).
- On `Fence`, the Owner frees every `Retired` slot with ID ≤ `F`. A later *first*
  Prepare with ID ≤ `F` is refused `Duplicate`, so nothing re-executes.
  Cleanup messages for `Live` slots ≤ `F` still route as today, because closed
  admission and age never make cleanup stale.
- A fence below the current one is refused `State`; an equal one is answered again.

**Bound.** Retained records = Live (at most `P`) + Retired-above-fence (at most
`P`, the same slots). Once the pool is full, new work is refused
`ResourcesUnavailable`. A stuck operation holds exactly one slot. Unlike a
contiguous all-terminal floor, it does not stop later operations from being
fenced and reused.

## Alternatives rejected

- **Contiguous all-terminal floor plus issuer window** (RAMCloud's `firstMissing`):
  simplest, but one Unknown-lifetime operation stops reuse for everything after it,
  and the window still needs an invented size.
- **Maximum arrival or latest `Retired` as the floor**: unsafe. Reordered bodies and
  burned IDs (retirement research traces 1 and 3).
- **NFSv4.1-style slot/sequence IDs** (RFC 8881 §2.10.6): would replace Programs'
  existing ID space. The research forbids a replacement allocator. We keep NFS's
  reply-before-reuse rule (a slot is reused only after its fence), not its IDs.
- **No ceiling, unbounded `Vec`**: today's state. It is what F5 reports, and the
  daemon cannot admit coverage for it.

## Pieces and owners

| Piece | Owner | Status |
| --- | --- | --- |
| Guest pool producer: acquire `P` records from a locked region at protected startup; typed refusal without `P`; coverage report | ROOT | next |
| Owner-side slot and fence bookkeeping: pure, allocation-free after acquisition, with unit tests | ROOT | next |
| `Fence`/`Fenced` codec and helper Stage accounting before relay | native lane | after its critical audit fixes |
| Owner consumes the pool and bookkeeping instead of `jobs: Vec` | runtime lane | after ROOT's module |
| Issuer computes `F` from first-publication resolution and sends `Fence` | audit lane (Programs) | its source graph is user-held; ROOT cannot validate it until cleared |
| Daemon admits `Dimension::PendingOperations` for the guest on the coverage report | ROOT | after the pool producer |

## Acceptance (R09/R10; red first where something exists to be red)

1. Red on current code: the Owner retains one Job per retired operation without
   bound (N Prepare/Retire cycles leave N records).
2. Green: after `Fence { through }`, retained records are exactly the Live count plus
   Retired above the fence, never more than `P`.
3. A first Prepare ≤ `F` is refused `Duplicate`. Late cleanup for a Live ID ≤ `F` is
   accepted. A non-monotone fence is refused `State`. With `P` Live operations, new
   work is refused `ResourcesUnavailable`.
4. Mutation: freeing Retired slots without a fence lets a duplicate Prepare execute
   again, and test 3 fails. Ignoring the fence lets test 2 fail.
5. musl: x86_64 and aarch64 checks for every touched crate (ROOT audit X2).

Not decided here: host-scope `pending_operations` (the daemon's own records),
sealed-body and reply storage (separate backing, same principle), and parent or
ancestor hard protection. All three remain open gaps.
