# Required producer seam (proposal for ROOT; not implemented)

Source base: `0342b85d9c808e3ced041a6c3ef0e9bf70438272`.

`init::domains::Server::start` (domains.rs:1964 onward) currently passes only
ready/filter/binary/private TLS directory, peer seqpacket endpoints and optional
actor-writable scratch. Its environment is empty. The TLS directory is visible
readably to the actor and has no persistence/storage lease. Neither scratch nor
that directory is an audit capability. `server::call` at server.rs:1409 explicitly
selects `MemoryOnly`; there is no admitted mandatory-audit policy to infer.

ROOT must establish an actual trusted daemon/init producer before creating a
production `broker::audit::Journal`. Proposed smallest cross-seam patch:

1. At existing checked launch/resource admission, select the owner's mandatory
   metadata audit policy and a real protected persistent storage allocation with
   exact byte authority and control survival. No defaultquota/environmentgrant.
   The producer owns extent/quota/inode/deleted-open accounting and persistence
   across VM/daemon/host failure; preallocation alone is not authority.
2. Create/open a regular journal object beneath a held owner-private root and
   establish name/directory durability. Hold exclusive writer/restart identity;
   reject aliases/shared writable handles and establish revocation/retention.
   Actor, siblings, supplied image and sandbox/helper never receive a writable
   path/descriptor, ancestor-control handle or broker credential. File flags/type
   checks are necessary but cannot themselves prove those properties.
3. Pass this held writer plus checked admitted extent, scope and policy through
   the existing trusted init start channel, not an actor-visible public environment
   string or unchecked fd number. Extend init descriptor placement/close_range
   accounting exactly once; do not duplicate it into actor/helper exec lists.
4. Add the narrow Journal production constructor only at that checked source.
   Constructor input must represent the proven producer above. Match actual
   scope/generation and one writer; check file kind/flags/offset/extent without
   inferring authority from those checks. No writable directory is needed in the
   server. Its `Prepared` owns the journal and its real dispatcher borrows it.
5. Add only required write/data-sync rights to the existing checked server filter,
   scoped to this descriptor. Preserve actor/helper syscall/object restrictions;
   unknown/missing grant refuses the required mode before actor release.
6. Decide disk-wait/control ownership from actual fault/cost evidence. The current
   broker is synchronous: it adds no lock, but a stalled fsync can still delay that
   caller. A cache/async queue cannot stand in for the required pre-effect barrier.
7. Before enabling, ROOT observes real peer effects and actor access denial under
   capacity exhaustion, partial/flush failures, death after every barrier, terminal
   failure after send, and restart. Incomplete/ambiguous records are never replayed.

This proposal deliberately changes no init/daemon/ABI/filter/network/metric/API
source. It is the explicit missing production integration, not a passing fixture.

## Exact type/caller shape for ROOT coordination

Do not add a `Journal::new(File, u64)` public constructor. The intended private
integration is `broker::audit::Journal::from_admitted(ProtectedAuditWriter)`, where
`ProtectedAuditWriter` is minted only by the trusted init/daemon launch-admission
path above. It owns the regular file, remaining admitted byte extent, and checked
owner/run/member/generation/policy identity. Fields and minting stay private to
that producer; receiving a file/byte count from actor JSON/environment is not a
minting operation. The concrete protection/admission evidence determines whether
this type can honestly be constructed; these names do not establish that evidence.

The existing actual caller delta is narrow: `Prepared` owns
`Option<broker::audit::Journal>` plus the checked mandatory/legacy policy selected
by trusted startup; `server::call` takes `&mut Prepared`, borrows that field once
for `Context.evidence`, and selects `Evidence::RequiredDurable` plus `Context.journal = journal.as_mut()` only when
that trusted policy requires it. `None` remains explicit unavailability. The
single existing HTTP and Program dispatchers propagate that mutable borrow; no
helper/sandbox journal field or tool-argument policy override is introduced.
A future exact signature must use the actual producer's established identity and
storage authority types rather than duplicate them here. This checkpoint does
not predeclare an unused generic trait or second resource ledger to guess those
types. ROOT owns the init/daemon/ABI/filter/mutable-caller patch and its live proof.
