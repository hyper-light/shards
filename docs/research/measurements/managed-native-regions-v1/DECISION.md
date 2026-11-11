# D-NR1: acquire initialized private storage before native publication

ROOT coordinator, policy main `0d261aa931ae2e33e1f4a528b33f7e26b5ee7c90`,
R01–R16 SHA `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
This is one acquisition mechanism in the incomplete native resource producer.
It does not enable native Session admission or claim hard parent/kernel backing.

## Source facts and selected representation

The current native Preparation owns a snapshot and policy but uses allocator
capacity to obtain their transient bytes. Owner later grows a Vec of whole Jobs.
Those are real allocations, but capacity is not proof of populated or resident
pages. The existing [backing review](../../managed-record-backing-research-v1.md)
separates actual storage, residency, authority, ancestor protection and return.
The new retirement review is also separate: no largest-arrived or retired ID is
a safe floor, and a stuck earlier operation must retain all unresolved facts.

This leaf acquires a dedicated private anonymous RW mapping, excludes it from
fork, performs ordinary mlock on the whole checked page-rounded extent, then
initializes all mapped bytes. Only the requested usable prefix is exposed.
Successful return owns those actual bytes and their lock. Lengths are supplied
by the concrete representation requirement after independent owner admission;
the API chooses no initial capacity, resource quota, expansion factor or timer.

Linux 6.18 [do_mlock](https://raw.githubusercontent.com/torvalds/linux/v6.18/mm/mlock.c)
applies VM_LOCKED, checks existing RLIMIT_MEMLOCK/CAP_IPC_LOCK and calls
__mm_populate without ignoring errors. Its
[population path](https://raw.githubusercontent.com/torvalds/linux/v6.18/mm/gup.c)
uses writable faults for private writable mappings to break COW. MAP_LOCKED and
MLOCK_ONFAULT have weaker population contracts and are not used here. No limit,
capability, overcommit setting or controller is changed.

Later fork would reintroduce COW obligations; inherited locks do not fund that
new allocation. Linux's
[MADV_DONTFORK implementation](https://raw.githubusercontent.com/torvalds/linux/v6.18/mm/madvise.c)
marks this region VM_DONTCOPY and documents its exclusion from the child. This
is applied before lock/acquisition completes. A copied Rust wrapper also checks
the original process before borrowing or unmapping; it cannot dereference an
excluded mapping or unmap an unrelated replacement in the child. This process
check protects memory safety; it is not an authorization/principal check.

The earlier ROOT ordinary Linux page observation
`/Users/adalundhe/g121-root-parallel-evidence-20261110/record-page-backing-probe-v1`
observed successful lock and lock-byte return for one actual OS page. That is
mechanism data only, not a passing result for this code, fork exclusion, pinned
guest, pressure, caller integration or aggregate backing. New code checks and
the relevant ownership/fork guard observation are recorded separately.

## Alternatives and boundaries

| Alternative | Assessment at this seam |
| --- | --- |
| Vec capacity or read-only prefault | Insufficient writable population/residency evidence. No capacity-as-grant constructor. |
| Lock an allocator subrange | Page rounding can overlap another allocation's lock lifetime. A separate mapping gives exact ownership and cleanup boundaries. |
| Private mapping without fork exclusion | Subsequent COW can require new pages after apparent acquisition. |
| Shared mapping inherited by the child | Adds a mutable cross-process object and reference/retirement obligations. The parent-private region does not need that sharing. |
| Sealed memfd | Retained for immutable transferred bodies by the existing ONE sealed_body implementation; immutable seals cannot provide reusable mutable records. No duplicate sealer is added. |
| HugeTLB/io_uring registered pool | Requires independently acquired pool/pin/metadata and different charging/lifetime machinery. Neither is selected to manufacture authority. |

The Linux-only adapter is shared by init/server through the existing IPC crate;
it does not link VMM/platform host machinery into the guest or add an installed
binary. Its consumers still need their real prepared objects, original Scope,
live owner and explicit acquired parser/reply/body/record/transport overlaps.

## Failures, ownership and accounting

An invalid/unrepresentable extent is refused before mmap. Mapping, DONTFORK and
lock failures preserve typed operation/errno. After partial acquisition, exact
unmap is attempted. Failed cleanup returns the actual mapping in Retained;
that type cannot lend unready bytes. Explicit consuming release similarly
returns its mapping on failure. Drop is only best effort and certifies no
resource return. No ledger may refund on Drop or a failure description alone.

The page size comes from the running OS. Rounding and the Rust slice isize bound
use checked arithmetic. The original requested extent and actual mapped extent
remain distinct. No bytes, address, owner token or credential enters Debug.

These pages are charged in the actual allocating process's current kernel
context. Labels do not move that charge. Residency does not establish effective
memory.min throughout ancestors, exclusive host physical backing of a VM,
preacquired descriptors/SCM metadata/cgroup objects or immunity to OOM. Those
are still ROOT implementation/proof obligations. The existing D59 victim policy
is unchanged; no entitlement or neighbor guarantee is silently strengthened.

Current status: source implementation in a new isolated worktree. Actual native
Preparation/Session integration, finite record acquisition/reuse/publication
fence, parser/decoder/reply/body overlaps, seven targets, pinned guest/LSM,
pressure/mutation/control progress and measured costs remain incomplete.

Audit's 4,819-file compiler graph and five portable tests remain held. This
independent Linux storage scope does not import, validate or release that graph.
The Windows ARM VM and all other named holds remain untouched.
