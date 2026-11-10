# Actual control owner and remaining charge boundary

Source facts at landed0e and current33df; the three files below are byte-identical
between those revisions. This is a focused caller/authority trace for the resource
queue, not live cost data, a defect reproduction, an audit or an optimization.
No production source or new lease type is introduced.

`domains::start` obtains Committed at domains.rs:1616–1620, then passes its held
directory to `managed::Launch::new` at1779, before clone3 at1797. Launch::new
already opens and owns its own cgroup.events/cgroup.kill descriptors, executable
object and owner/gate/split/ready/setup pipes before clone. The source also makes
a temporary fallible clone of the owner write descriptor to send last_cap.
These acquisitions occur in trusted outer init before the child branch, rather
than becoming an owner budget by virtue of the domain label.

After successful clone, domains.rs:1815–1816 transfers `admitted.into_file()`
into Launch::parent. managed.rs:373–392 moves Launch's kill/events, directory,
pidfd and protocol endpoints into the existing Owned lifecycle. Thus the cap
module's additional held control pair is not the lifecycle's sole control pair;
Launch's pair was acquired before clone and moves into Owned. This trace does
not demonstrate a missing-control defect. It also supplies no measured benefit
for changing the pair acquisition; no deduplication is implemented.

The existing **Launch → Owned** transfer is the concrete real caller seam for
a future ROOT-approved parent/control charge: prepare the checked charge before
clone, carry it with that owned lifetime, and release only under the lifetime's
actual cleanup/uncertainty contract. No generic ledger, parallel manager or new
guarantee flag is needed merely to name that seam. ROOT owns any managed/server
integration change and the missing authoritative parent/owner charge producer.

Known authority remains the declared task/memory self-cap and D59 aggregate
policy documented in AUTHORITY.md. None of these File/OwnedFd objects creates an
owner guarantee. No checked guaranteed-claim producer or strict guaranteed-mode
caller is established by this trace. ROOM, reclaim headroom, memory.low, role
changes and successful readback still confer no reserved entitlement.

Actual per-domain charge data is **unknown**, including pipe/socket page
attribution, init/broker induced allocations and retained pages, physical control
reserve, ancestor available/reserved capacity and resource return under failures.
Source acquisition lists are not runtime FD high-water counts or kernel charges;
do not turn them into a fixed helper allowance, a source-picked threshold or a
parent reservation. The ordinary musl tests validate object/refusal behavior,
and ROOT's normal KVM proves its supplied successful caller case; neither closes
these charge/guarantee obligations.

The corresponding structured facts and exact file hashes are in
`records/control-caller-facts-0e.json`. ROOT's existing on-demand stats/cgroup
report seams can supply actual charge observations if selected by ROOT; this
lane adds no reporter/cache/poll, measurement campaign or production policy.
