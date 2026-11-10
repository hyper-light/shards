# Managed capacity constants: Poller, memory events and transport

This is primary-source research for coordinator-selected capacity literals, not a
repository audit, reproduced defect, implementation decision or performance
result. **No replacement number or architecture is selected.** Production source
is unchanged. Runtime reproduction, profiling and final selection belong to the
coordinator under R15.

Base: `24f48ee5ebf198485117aa93bf47685ad7d7d53f`. POLICY_READ implementer:
full AGENTS/R01–R16/evidence/CLAUDE/managed plan/current handoff; rules SHA-256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Owned paths: this file and textual records in
[`measurements/managed-poller-research/`](measurements/managed-poller-research/).
The [lane record](measurements/managed-poller-research/README.md) accounts for
R01–R16, checkpoints and unrun work. The
[source manifest](measurements/managed-poller-research/source-identity.json)
records exact URLs, resolved commits, retrieval times and raw-byte SHA-256 hashes.

## 1. Poller: what the current source actually does

The scoped source is
[`Poller` in unix.rs](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/platform/unix.rs#L1873), lines 1873–2106 at
the base. `wait(&self, ready: &mut Vec<u64>, timeout)` clears an existing
caller-owned token vector, makes a zeroed stack array of 64 native events,
requests at most that many kernel results and extends the token vector. Thus
there are two different storage questions:

* The native event array is fixed per invocation. Its capacity is not the number
  of registrations and is not a kernel watch limit.
* The token vector retains its capacity across `clear`. Growth can allocate when
  the returned count exceeds retained capacity; this does not establish an
  allocation on every wait. The current extension has no fallible reservation.
  Rust's pinned [`Vec` source](https://github.com/rust-lang/rust/blob/f8297e351a40c1439a467bbbb6879088047f50b3/library/alloc/src/vec/mod.rs)
  distinguishes clearing, infallible growth and `try_reserve_exact`; allocator
  layout and refusal are handled in
  [`RawVec`](https://github.com/rust-lang/rust/blob/f8297e351a40c1439a467bbbb6879088047f50b3/library/alloc/src/raw_vec/mod.rs).

This is a source observation, not an allocation/OOM reproduction. In particular,
the [host-exit source record](measurements/managed-output-cost/HOST-EXIT.md)
discloses the unchanged batch and Vec dependency; it does not measure its cost.
The [coordinator's r518/r523 record](measurements/managed-coordinator-output-exit/README.md)
establishes two held exit notifications and the stated fixture FD checkpoints at
`ebbda9b3`, not capacity scaling, fairness, allocation freedom or kernel charge.

### Callers needed to distinguish ownership and demand

These are focused reads of users of this exact Poller, not an inventory of other
similarly named pollers. Source hashes are in the manifest.

| Caller at base | Actual output storage and relevant demand |
| --- | --- |
| [`daemon/follow.rs`](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/shards/src/daemon/follow.rs#L222), `Followers::new`, `follow_all`, `follow_vm` | `ready` lives outside the wait loop. A wake socket, followed run sockets and process exits share the poller. Registration can happen outside the wait thread. The existing run/process maps have lifecycle purposes; their lengths are not automatically an exact kernel-membership count. |
| [`daemon/commands.rs`](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/shards/src/daemon/commands.rs#L1970), bulk ending loop near lines 1970–2110 | Registers the actual `going` collection; reuses one token Vec; removes completed watches. Work and deadline checks occur between waits. |
| [`netproc.rs`](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/shards/src/netproc.rs#L345), `reap` | Watches one process and reuses one token Vec until readiness/deadline. Its pre-existing fallback is outside this capacity investigation. |
| [`managed_cleanup.rs`](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/shards/tests/managed_cleanup.rs#L186), `Client::wait_change` | Makes a fresh token Vec per invocation; watches stdout and, while applicable, child exit. This test helper differs from the production followers loop. |
| [`managed-output-cost/fixture.rs`](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/docs/research/measurements/managed-output-cost/fixture.rs#L709), `ExitProof::finish` | Reuses one Vec while waiting for two owned host-role exits; consumes each observed watch. This finite two-role fixture cannot establish behavior above a batch. |
| [`platform/mod.rs`](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/vmm/src/platform/mod.rs#L471), existing Poller tests | Source includes 150 ready socket pairs drained across waits, level readiness, explicit removal and child-exit cases. They are **unrun here** and do not test persistent-hot-source fairness or allocator refusal. |

## 2. OS limits, event semantics and portability

### Linux epoll

The pinned [Linux v6.18 implementation](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/eventpoll.c)
defines `EP_MAX_EVENTS = INT_MAX / sizeof(struct epoll_event)` and rejects
`maxevents <= 0` or values above that bound (`ep_check_params`). `ep_send_events`
stops at the supplied length and requeues still-ready level-triggered entries.
This bounds a **single syscall's event count**, not registrations or a desired
service batch. Use checked conversions, allocation-layout arithmetic and the
target's native event size if designing dynamic storage; integer maximum alone
is insufficient.

The [epoll_wait manual](https://man7.org/linux/man-pages/man2/epoll_wait.2.html)
documents rotation through excess ready descriptors on successive waits. It also
permits waiting on an empty interest set with a positive output length: a later
ready registration can wake it. The [epoll manual](https://man7.org/linux/man-pages/man7/epoll.7.html)
describes coalescing, already-collected events, and the registration key of
descriptor plus open-file description. Closing a watched descriptor does not
necessarily remove its registration while duplicates of that open-file
description remain. `DEL` and closure are not interchangeable membership events.

Current Shards registrations request `EPOLLIN | EPOLLRDHUP`, without ET or
ONESHOT. A unique held pidfd remains a level-readiness source after exit until
retired; batching must preserve caller retirement. A kernel rotation guarantee
does not bound time spent in each application handler or latency to its wake,
stop or deadline work.

### macOS kqueue, arm64 only

Apple's [archived kqueue contract](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/kqueue.2.html)
defines an output array of `nevents` records, aggregation, `(ident, filter)`
registration identity, replacement on repeated registration, and removal of
descriptor filters on close. This is archived documentation, not a freshness or
fairness guarantee for every current macOS release.

In the [pinned XNU implementation](https://github.com/apple-oss-distributions/xnu/blob/e3723e1f17661b24996789d8afc084c0c3303b26/bsd/kern/kern_event.c),
`kevent_callback_inline` stops after the supplied output count; `kqueue_process`
leaves further processing for a later call. Its ordinary kqueue processing
suppresses delivered notes during the scan and restores them afterward. This is
evidence against interpreting a full output array as an overflow notification;
it is not a portable numeric starvation bound.

Shards adds a READ filter without `EV_CLEAR`, but uses PROC/NOTE_EXIT for process
watching. XNU's `filt_procattach` automatically marks PROC filters `EV_CLEAR` and
only captures subsequent edges; exit processing marks the note EOF/ONESHOT when
NOTE_REAP was not requested. `knote_process` drops an ordinary consumed oneshot.
Shards' empty macOS `ExitWatch` is therefore not a Linux-style registration
lease whose drop cancels the pending process filter. Preserve the caller's
register-then-check behavior and whole-poller lifetime; a capacity proposal must
not silently promise cross-platform watch cancellation.

**Failure ordering matters:** a fallible token-vector reservation after a
successful kevent wait can fail after the kernel has consumed a one-shot exit
event. A second wait is not a demonstrated recovery. A candidate must reserve
all needed output storage before dequeue, retain the raw returned batch until
delivery, or represent an explicit terminal uncertainty. This is a derived
design obligation to reproduce and test, not a claim that this lane observed a
lost event.

### ABI and cancellation constraints shared by the alternatives

The [Linux UAPI](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/include/uapi/linux/eventpoll.h)
uses architecture-dependent event packing; the
[XNU header](https://github.com/apple-oss-distributions/xnu/blob/e3723e1f17661b24996789d8afc084c0c3303b26/bsd/sys/event.h)
defines a different kevent layout with `udata`. A `Vec<u64>` cannot be handed to
either syscall as native event storage. Use actual `libc` types and their target
layout, valid writable space, checked counts and only the initialized returned
prefix. An internal native-event buffer can expose tokens without exposing a
generic framework or changing the token contract. Pointer-width token roundtrips
must remain valid on the supported 64-bit Unix targets.

Zero storage needs an explicit contract: epoll rejects zero `maxevents`, while
XNU's zero output count does not enter the event wait. Replacing an empty buffer
with a page, a copied default or a spin would not derive a useful capacity. A
minimum usable slot is a storage precondition, not permission for another watch.
Keep legitimate empty-interest waiting, the existing wake descriptor and deadline
semantics distinct.

The current API clears output and maps EINTR to an empty successful result. It
does not itself distinguish timeout from interruption. Existing absolute-deadline
callers recompute remaining time; a capacity change must not restart their full
timeout or introduce a loop that drains forever before checking cancellation.
No lock over the owning registration maps should span a blocking syscall.

## 3. What established implementations demonstrate

These are pinned mechanism comparisons, **not selected performance competitors
or numeric authorities**. Their guarantees and error policies differ from Shards.

| Source | Mechanism and relevant limitation |
| --- | --- |
| [libevent 2.1.12 epoll](https://github.com/libevent/libevent/blob/5df3037d10556bfcb675bc73e516978b75fc7bc7/epoll.c) | Retains native storage; starts at 32 and doubles a full batch up to its private 4096 cap. Failed optional growth retains old storage. Those are project defaults, not Linux limits. Dispatch activates callbacks separately. |
| [libevent 2.1.12 kqueue](https://github.com/libevent/libevent/blob/5df3037d10556bfcb675bc73e516978b75fc7bc7/kqueue.c) | Retains native event/change storage, initially 64; grows after a full result and for changelist requirements. Its READ/WRITE mapping and queued changes are broader than Shards' token-only wait. Its 64 supplies no derivation for Shards' 64. |
| [libev 4.33 official archive](https://dist.schmorp.de/libev/Attic/libev-4.33.tar.gz), `ev_epoll.c`, `ev_kqueue.c`, `ev.c` | Retains backend arrays, grows on full results, and reserves kqueue output room for changelist errors. Its allocator and default system-error path can abort; that policy cannot be copied into R01-compliant production. Archive/member hashes are in the manifest. |
| [mio 1.1.0 Events](https://github.com/tokio-rs/mio/blob/3a1b47180161fe191a8f8a8cc0e1cc572678aab2/src/event/events.rs), [epoll](https://github.com/tokio-rs/mio/blob/3a1b47180161fe191a8f8a8cc0e1cc572678aab2/src/sys/unix/selector/epoll.rs), [kqueue](https://github.com/tokio-rs/mio/blob/3a1b47180161fe191a8f8a8cc0e1cc572678aab2/src/sys/unix/selector/kqueue.rs) | Caller-owned reusable native-event storage; its capacity is the syscall output bound. Construction uses infallible Vec capacity. Mio requests ET/EV_CLEAR, so its drain rules must not replace Shards' existing level-readiness contract. Example capacities and unchecked casts are not a Shards derivation. |

## 4. Alternatives for coordinator evaluation

All choices must fit admitted memory and preserve cancellation. A smaller output
batch is not itself evidence of lost kernel events; a full batch is only a
censored demand observation, not an exact backlog count.

| Alternative | Cold start and authority | Overflow, failure and fairness obligations |
| --- | --- | --- |
| Keep the current code as the measured baseline | Records the existing unexplained 64 without endorsing it. No new allocation/cache/interface. | Measure native-array work separately from token growth and caller lifetime. An unchanged baseline cannot close R03 or establish OOM behavior. |
| Reusable native-event storage owned by the actual wait caller, with a narrow token view | Fallibly provision from that caller's admitted demand or explicit owned watch set before the first wait. The two-role fixture and single-process reap have concrete demand, unlike the concurrent follower. | Check native byte/count bounds and initialized length; keep reusable capacity charged through its lifetime. If output remains a separate Vec, provision both before dequeue. A full batch returns control for stop/deadline work before a further wait. No copied startup default. |
| Bound service storage from an existing authoritative registration owner | Derive usable capacity from actual admitted registrations and service resources, rather than adding a second watched-FD map. Existing owner collections may give only an upper bound, and a concurrently changing snapshot needs an explicit contract. | Prove add success/failure, repeated registration, explicit remove, close/dup, fd reuse, exit oneshot, partial setup and concurrent wait behavior. A snapshot or count must not claim exact membership without those proofs. Do not impose a new registration ceiling merely to fit storage. |
| Adapt retained storage to observed returned demand inside admitted bounds | Start from known admitted demand or a typed inability to provision; no benchmark-derived seed. Feedback may tune service allocation, never grants or entitlement. | Full batches only prove the output filled. Require a justified growth/shrink rule, checked arithmetic, retained high-water accounting, refusal fallback and neighbor/cancel evidence. Growth failure can keep an already valid smaller buffer only if its delivery/progress contract remains intact. |

Adding an atomic membership counter is not a consistency proof. Epoll duplicate
registration can fail while kqueue replacement succeeds; descriptor close can
remove without `Poller::remove`; Linux duplicated open-file descriptions can
outlive the watched fd; macOS exit delivery removes a filter without a caller
`remove`. Tokens need not be unique registrations. Existing `&self` registration
also allows concurrent changes. None licenses a duplicate watched-state cache.

## 5. Coordinator-owned reproduction and measurement plan — entirely unrun

First pin the tested source, compiler/libc/native ABI, OS/kernel and exact caller.
Use isolated coordinator-owned sources/targets and explicit resource admission;
no local heavy build or protected cache is authorized for this lane. A fixture
implementation can be scoped later after coordinator approval.

1. **Separate the hypotheses.** Instrument existing wait boundaries to record
   requested native slots, returned count, output capacity before/after, actual
   successful registrations/removals and caller lifetime. Reproduce any claimed
   allocation or latency symptom at its exact caller before changing production.
   Treat a full batch as censored demand. Do not infer allocation from `Vec::new`
   alone or substitute a microbenchmark for the real caller.
2. **Functional boundary cases.** Exercise empty interest versus empty output,
   one source, all ready, more ready than supplied storage and sources that remain
   hot across waits. Include read EOF/RDHUP, explicit remove, close with/without
   duplicates, same-fd re-add, fd/token reuse and registration failure. Prove each
   intended token eventually appears; independently observe bytes/EOF or child
   state. The existing 150-pair drain test is a starting fixture, not this proof.
3. **Control fairness.** Mix sustained readable sources, a quiet source becoming
   ready, a wake/stop request and an expiring absolute deadline. Record handler
   work and time to control progress, not just syscall rotation. Keep number of
   handlers per turn and repeated wait behavior visible; no arbitrary latency
   allowance or endless drain-until-empty rule is selected here.
4. **Refusal and cancellation.** Inject cold storage refusal and later attempted
   growth refusal, count overflow/layout refusal, interruption and registration
   races. On macOS explicitly test exit already finished versus exit after
   registration, oneshot consumption and drop-before-event behavior. A candidate
   that can fail after dequeue needs an independent witness for event retention
   or explicit uncertainty. Verify every owned descriptor and retained buffer
   through completion, error, cancellation and partial initialization.
5. **Meaningful counterfactuals.** After a candidate exists, independently remove
   its pre-dequeue reservation, native/count bounds, consumed-watch retirement
   and per-turn control check where applicable. Require the corresponding actual
   lost notification, invalid input/refusal, repeated readiness or delayed control
   witness; restore exact source and repeat. These are proposed guards, not run
   mutations or known production faults.
6. **Comparable cost.** Compare current and candidate same-workload callers,
   paired/interleaved, with cold/warm and changing demand. Coordinator records n,
   p50/p90/p99/max, CPU, allocations/reallocations/copies, stack and retained native
   and token storage, faults, parent/kernel charges, throughput and neighbor
   impact/noise. Pin equivalent libevent/libev/mio configurations and their
   readiness/dispatch guarantees before any competitive comparison. Report
   R08's target and misses; do not turn a winning observed batch into a constant.
7. **Portability.** Linux amd64/arm64 with GNU/musl and named actual kernels;
   macOS arm64 with named XNU/SDK/libc correspondence. Windows uses a separate
   existing Poller implementation and is not validated by these Unix documents.
   Any changed shared caller still owes the supported seven-target gate and
   relevant actual host/VM checks to the coordinator. Unavailable hardware stays
   unverified; the paused Windows ARM VM stays paused.

## 6. Memory::relieve inotify buffer

The second item is `Memory::{watch,relieve}` and `Ooms::of` in
[`domains.rs`](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/init/src/domains.rs#L886), lines 886–1043 at base, plus only
its setup and poll/dispatch callers in
[`run.rs`](https://github.com/hyper-light/shards/blob/24f48ee5ebf198485117aa93bf47685ad7d7d53f/crates/init/src/run.rs#L248), near lines 248, 4679 and 5124.
`watch` creates one nonblocking/CLOEXEC inotify instance and registers
`{CGROUPS}/memory.events` itself for `IN_MODIFY`. It does not register a directory,
one watch per domain, or ONESHOT. The successful watch descriptor is not retained
separately; the owned inotify fd controls this instance's lifetime.

`relieve` creates a zeroed 4096-byte stack array and reads until `read <= 0`,
discarding the returned bytes. It does not distinguish EAGAIN, EINTR, EINVAL or
another read failure, and does not inspect overflow/invalidation records. It then
rereads per-domain OOM counters/population and current aggregate memory/high,
and retains the existing largest-populated-domain victim policy. These are
specific source facts, not observed failed relief, lost notifications, excessive
stack cost or starvation. No fix to this code is approved by these observations.

The relay polls this fd for `POLLIN` and invokes relief inline with other owner
handlers. Therefore the amount of drain work before state evaluation and return
to control handling matters independently of byte-array size. A nonblocking
descriptor prevents sleeping for an unavailable record; it does not impose a
bound on a loop whose queue can keep receiving notifications.

### A record bound is not a service-batch choice

Let `H = size_of::<libc::inotify_event>()` on the guest target. The pinned
[Linux UAPI](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/include/uapi/linux/inotify.h)
has four 32-bit fields followed by an optional name. The
[inotify reader](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/notify/inotify/inotify_user.c)
uses `H + round_event_name_len(event)` for each record. An absent name adds zero;
a present name includes a terminating NUL and padding to a multiple of H. The
buffer receives whole records. If the next record does not fit, `get_one_event`
returns EINVAL **before removing it from the queue**; after a successful prefix,
`inotify_read` normally returns those bytes instead of that later size error.
A short read consequently does not prove that the queue is empty.
By contrast, the source removes a record before copying it to userspace; an
EFAULT copy failure is not evidence that retry preserves every notification.

This exact watch has a useful conditional framing invariant. The
[fsnotify dispatcher](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/notify/fsnotify.c)
strips the parent name when delivering a non-directory-entry event to the file's
inode watch (`fsnotify_handle_event`). Thus its file-modification record has no
name. IN_IGNORED/IN_Q_OVERFLOW also have no filename. H is a researched bound for
**one record under that unchanged file-watch contract**. It is not the chosen
read length, an upper bound on queued records or evidence that reading one at a
time is a suitable service policy. Adding directory/named-event watches would
invalidate the premise and require a checked variable-record bound.

Neither a host/guest page size nor 4096 is the inotify record protocol limit. A
larger buffer changes how many records one read can collect; it does not enlarge
the kernel queue or prove absence of overflow. The
[notification queue](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/notify/notification.c)
has its own event-count bound and separately queues an overflow marker. Queue
configuration is not an admitted userspace byte budget and must not be changed
on the host to make a candidate pass.

`FIONREAD` is an alternative demand input: the pinned inotify implementation
scans the queued records under its lock and returns the aggregate bytes through
an `int`. It is a snapshot, not a reservation or an upper bound on a later read's
queue. A caller must validate the signed result, byte/layout arithmetic, memory
admission and allocation failure, and still accept a complete prefix/retry.
An extra ioctl, locked scan and possible retained high-water allocation require
measurement; one observed queued length cannot become a new runtime constant.

### Notifications, authoritative state and terminal conditions

The [cgroup v2 contract](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/admin-guide/cgroup-v2.rst)
defines `memory.events` counters and modified-event notification, including
hierarchical behavior and the separate local-events interface. Actual mount and
controller configuration must be named in a reproduction. The current code uses
the notification as a wake to reread current state, not as a counted OOM history.
Keep that distinction: coalesced or lost wake records are not the source of the
counter values, and a snapshot of several files is not an atomic history.

The [inotify event backend](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/notify/inotify/inotify_fsnotify.c)
merges identical successive queued events. That can reduce demand for this
one-file watch, but proves neither a finite service latency nor impossible loss:
its kernel event-allocation failure explicitly queues overflow too. More storage
in this userspace array cannot repair such a lost notification.

The [inotify contract](https://man7.org/linux/man-pages/man7/inotify.7.html) and
[watch-removal contract](https://man7.org/linux/man-pages/man2/inotify_rm_watch.2.html)
also distinguish invalidation from ordinary modification. Removal produces
IN_IGNORED; pending records can outlive their watch. Closing the last descriptor
for the inotify instance destroys its watches. Closing an unrelated open of the
watched file does not cancel this instance's watch. ONESHOT, if ever introduced,
removes a watch after an event; current Memory does not use it. Re-registering
by pathname or recycled watch number must not silently adopt stale records or
an unrelated replacement object.

| Input/state to distinguish in an approved candidate | Required meaning; current behavior is not claimed verified |
| --- | --- |
| Cold setup succeeds or fails | Admission and object/watch registration precede active notification ownership. Read a coherent-enough initial state under the selected contract; do not assume registration synthesizes past pressure. Startup ordering needs actual before/after registration cases. |
| Positive read | Complete records were dequeued; notification counts are not counter increments. Validate a parsed prefix and retain any record needed for a later action before fallible work. |
| EAGAIN / EWOULDBLOCK | Queue had no available record at that check. State can change immediately afterward; preserve event-driven reentry. |
| EINTR | Interrupted operation, not proven drain completion. Follow the actual caller's control/deadline contract without an unbounded retry that postpones control. |
| EINVAL / zero / other failure | Not interchangeable with clean queue exhaustion. Represent a size/invariant failure, unavailable watch or read error explicitly as the selected boundary contract requires. No typed production change is made here. |
| IN_Q_OVERFLOW | Notification loss/uncertainty. Reconcile authoritative counters/current state if that suffices for the chosen policy; never reconstruct exact history from the number of queued events. |
| IN_IGNORED / IN_UNMOUNT | Future notifications from the old watch are unavailable. Rescanning current state alone does not restore a live watch. Any reopen/re-register or terminal failure needs owned identity, lifecycle and cleanup proof. |
| Cancel/owner teardown | Release this actual instance and its owned storage; allow relay control and neighbor work to progress. No new periodic state-probe fallback or duplicate counter/watch cache is justified. |

For byte-buffer parsing, do not cast an arbitrary `[u8]` address into an aligned
reference to `libc::inotify_event`. Validate the returned prefix and checked
`offset + H + len`; use an appropriate unaligned copy or field decoding, native
integer byte order, and the real UAPI layout. Overflow's negative watch number
is a protocol event, not an array index. If a candidate validates the one owned
watch ID, retain the successful registration receipt and its state; a parallel
general watched-object map is unnecessary. This is a constraint on alternatives,
not a speculative parser/interface implementation.

### Established comparison and alternatives

The same pinned [libev 4.33 archive](https://dist.schmorp.de/libev/Attic/libev-4.33.tar.gz)
contains `EV_INOTIFY_BUFSIZE` in `ev.c`: a header/name/padding-derived expression,
not a page size. `infy_cb` does one read and visits complete records; `infy_wd`
rechecks its watched state on overflow and re-adds invalidated stat watches.
Its generic stat watcher can also use timed stat fallback. That broader contract,
infallible allocation policy and formula are not an adoption decision for this
single cgroup file. The earlier libevent/mio comparisons concern **native
readiness records**, a different buffer from these inotify payload records.

| Alternative, all unselected | Cold start / failure / resource tradeoff |
| --- | --- |
| Current 4096-byte stack drain as baseline | No new allocation or interface. Retains the unexplained literal and existing errno/record treatment; measure exact symptoms before claiming a defect or cost. |
| File-watch framing derived from H, serviced in bounded turns | Fixed record storage can avoid a new heap allocation under the unchanged unnamed-record premise. Batch/number-of-reads policy still needs admitted service demand and control-progress evidence; H alone cannot select it. Unexpected named records/size errors must fail explicitly rather than silently stop. |
| Reusable, fallibly provisioned byte storage based on admitted/observed demand | Can collect several complete records without recreating heap storage. Provision before dequeue; keep retained bytes charged, check growth arithmetic and hold the prior valid buffer on optional-growth refusal only if progress remains valid. FIONREAD is optional feedback with its own syscall/lock cost, not automatic authority. |
| One bounded collection/rescan per relay turn instead of drain-until-empty | Distinguishes scheduling policy from record framing. Remaining records keep the inotify fd readable, but state/readiness races, redundant rescans and control fairness require proof. No arbitrary read-count or time quantum is selected. |
| Hold `memory.events` and wait for its change condition directly | Could remove the inotify payload-buffer question at a different existing OS seam. Requires a separate reproduced compatibility/cost decision and precise read-to-ack/object-removal contract; not an approved refactor. |

The last alternative has a specific primary-source basis. The
[memory controller file definition](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/mm/memcontrol.c)
associates `memory.events` with an events-file notification object;
[memory event accounting](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/include/linux/memcontrol.h)
updates counters and calls
[`cgroup_file_notify`](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/cgroup/cgroup.c).
[kernfs](https://github.com/torvalds/linux/blob/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/kernfs/file.c)
wakes its open-file poll state, marks changed state with POLLPRI/ERR, and samples
the notification generation in `kernfs_seq_show`. Ordinary POLLIN can already be
true without a change, so copying the current relay's POLLIN interest would be
incorrect for this alternative. Seeking/rereading through the **same retained
open file** and notification-before/during/after-ack races need actual evidence.
This is event-driven kernel notification, not periodic cgroup probing.

Changing buffer storage must not silently change the D59 victim/entitlement
policy, OOM reporting or management lifetime. No source observation here settles
the managed plan's unresolved owner-budget or fairness requirements.

## 7. Coordinator-owned inotify plan — entirely unrun

1. Pin exact source/kernel/config/libc/UAPI, cgroup mount options, actual watched
   object, allowed memory/current/high values and owned member identities. Begin
   with a bounded private fixture on the existing coordinator-approved guest
   setup. No host sysctl, mount/controller provisioning or privileged host change
   is proposed. This lane ran no fixture or cgroup operation.
2. Reproduce the specific claimed symptom before a production change. Trace the
   actual watch mask/ID, raw record lengths/masks, read returns/errno, unread
   readiness, authoritative state before/after and actual relief outcome. Show
   pre-watch existing pressure, update during registration, ordinary coalescing
   and a later independent update. A regular-file write fixture can prove framing
   but cannot stand in for actual cgroup notification/relief semantics.
3. Compare buffers relative to the **derived framing**, including below a next
   record, exactly fitting one and a complete-prefix read with more records
   pending. Add a separate test-owned directory/named-record case as an invalidated
   premise control, not as evidence that Memory already watches names. Independent
   queue/readiness observation must establish that a too-small failed read left
   its next record pending. Do not infer queue emptiness from a short positive read.
4. Exercise interruption, watch creation refusal, watched-object deletion or owned
   removal, pending records after removal, closure, partial setup and owner cancel.
   Only a separately approved failure fixture may inject allocator/read/overflow
   outcomes. Scripted overflow records validate consumer handling only; they do
   not prove actual kernel queue exhaustion or kernel-allocation failure. Do not
   change host queue limits or claim overflow impossible from coalescing.
5. Under controlled changing memory demand, independently observe main/sibling
   progress and stop processing while Memory is readable. Record drain work,
   state-read work, control/deadline latency and whether the actual population
   chosen by the unchanged policy ended. An event mask or log alone is not that
   effect. Establish watch/storage/FD and parent/kernel return on error/cancel.
6. For any candidate, remove only its relevant errno/invalidation/record-bound/
   per-turn guard and require the associated actual stale-watch, stuck record,
   wrong state/effect or delayed control witness; restore exact source and rerun.
   There is no implemented guard or executed mutation in this research batch.
7. Pair/interleave current and approved alternatives on the same workloads,
   including cold start, burst/coalesced demand and idle-to-active transitions.
   Root records n and p50/p90/p99/max, syscall/read counts, allocation/reallocation/
   copies, stack/retained storage, faults, CPU and parent/kernel/neighbor costs.
   A changed record batch is justified by its method/invariants and live evidence,
   never a copied library formula or fastest observed number. Direct kernfs poll
   additionally needs ack/removal/race compatibility proof on named actual kernels.
8. This guest code is Linux-specific on amd64/arm64 with GNU/musl build coverage;
   it also runs inside guests hosted by supported macOS arm64 and Windows hosts.
   Coordinator owns changed-source lint and real-guest gates. No host platform
   result follows from the Linux document, no unavailable/paused hardware is a
   passing skip, and all tests/costs above remain **unrun** here.

## 8. Concrete capacity contracts for coordinator comparison

This continuation makes the alternatives testable without selecting an API,
batch number, growth rule or production change. The original two literals remain
the entire source scope. These are necessary constraints inferred from the
primary sources above; satisfying them alone would not establish correctness,
fairness or hard resource accounting.

### Logical service length, allocated space and reservations

Let C be a proposed syscall's **logical service length**, E the target's native
event size, N the native buffer's allocated capacity, and T the separate token
buffer's allocated capacity if one is retained. C counts native records, not
distinct token values: this API does not require unique tokens.

For a positive event wait, a candidate with separate native/token storage needs
all of the following **before dequeuing**:

* C is positive and within the syscall count/byte bounds from section 2.
* C <= N and C <= T, with checked layouts and conversions.
* The actual syscall receives C, even when N is larger. Returning up to N while
  only T >= C was prepared can reintroduce allocation after dequeue.
* Retained native and token capacities, allocator overhead and any growth peak
  fit the actual admission contract. C * (E + size_of::<u64>()) is only requested
  payload arithmetic, not a proof of total allocator/parent/kernel charges.

The public [`Vec::try_reserve_exact` contract](https://github.com/rust-lang/rust/blob/f8297e351a40c1439a467bbbb6879088047f50b3/library/alloc/src/vec/mod.rs)
guarantees at least `len + additional`, permits allocator excess, and does not
promise exact capacity. It takes additional elements relative to **length**,
not additional space beyond capacity. After clearing, requesting `C - capacity`
can do nothing and fail to establish capacity C. A candidate must derive the
argument from current length and its target, with checked arithmetic, then
validate the resulting usable storage contract.

This is not an observed over-allocation result. The pinned
[`RawVec::grow_exact`](https://github.com/rust-lang/rust/blob/f8297e351a40c1439a467bbbb6879088047f50b3/library/alloc/src/raw_vec/mod.rs)
sets its requested capacity; its allocation path comments on the current
allocator behavior. Independently, a reused buffer can already retain a larger
capacity from an earlier turn. Logical C must therefore stay independent of N
even if a particular allocator happens to report exact requested capacities.
The pinned mio epoll implementation passes allocated capacity as its syscall
length; that describes mio's contract, not a proof that the same choice fits a
Shards candidate with separately admitted token/output storage.

The admission input itself is unresolved. The current Poller does not carry a
hard admitted storage budget, and existing collection lengths are not proven
exact kernel membership. Giving these quantities symbols does not create such a
budget or justify a new registration cap. Coordinator data and the surrounding
resource contract must supply the actual constraints before selection.

For inotify the corresponding distinction is between B, the supplied byte
length, and the allocated byte capacity. B must fit the next complete record
under the proved watch contract; the number of reads/records serviced before
returning to the relay remains a different choice. Queue bytes from FIONREAD,
the kernel event-count limit and total allocated bytes cannot substitute for
one another.

### Ownership and transitions without a second watch registry

| Candidate storage arrangement | Required order and lifetime |
| --- | --- |
| Retain the current token-Vec result API with reusable native storage | Provision native and token room for the **same logical C**, then enter the syscall. On success only the returned initialized prefix is converted, without allocating. A failed cold reservation occurs before kernel dequeue. Optional later growth refusal can retain the prior valid service buffer only under an explicit progress/error contract. |
| Caller-owned native storage with a borrowed token view | Fallibly provision storage, wait into it, then expose only its initialized prefix. The view remains valid until the caller finishes consuming it; reuse/growth must wait until that borrow ends. This avoids a separate token-copy buffer, but changes the caller contract and requires live caller evidence. It does not require adding a watched-FD map or changing registration ownership. |

The proposed sequence is `unprepared -> prepared(C) -> waiting -> filled(n) ->
consumed`, where 0 <= n <= C; reuse returns to prepared state. A syscall error or
interruption establishes no filled prefix under the current Poller contract.
Reservation failure is a preparation failure, not an empty successful wait.
If caller processing fails while records remain filled, the selected contract
must retain them, reconcile owned terminal state, or report explicit uncertainty;
clearing the storage is not automatically a retry of a consumed oneshot event.
These are contract states, not a new production enum or manager.

Current `wait` and registration take `&self`. Moving mutable event storage inside
Poller would need an explicit rule for simultaneous waits and registrations;
serializing the whole object with a mutex across a blocking wait could prevent
another caller from adding/waking it. Caller-owned storage avoids that particular
shared mutable buffer, but does not itself promise which of multiple simultaneous
waiters receives a one-shot event. The focused production followers caller has
one wait loop; do not invent a stronger multi-waiter contract or hold its
registration-owner locks across the wait.

Cancellation continues through actual existing wake/deadline/ownership paths.
No proposal here relies on closing a poller fd from another thread as a portable
wait-cancellation guarantee. A synchronous allocation attempt before waiting
also needs control-progress and admitted-cost evidence; merely moving a fallible
step earlier does not prove bounded cancellation latency.

### Additional decisive cases, all unrun and coordinator-owned

These complement sections 5 and 7. C and the other letters below are explicit
finite fixture inputs chosen inside the coordinator's admitted resources, never
new runtime policy values. A synthetic reserve/parser case verifies only its
local contract; it cannot replace the exact caller's live reproduction.

| Case | Setup and independent witness | Candidate requirement / useful counterfactual |
| --- | --- | --- |
| Capacity argument semantics | Clear a reusable Vec with old capacity G; request a target C where G < C and C - G <= G. Record requested additional, length and resulting capacity. | Correct preparation must establish capacity >= C. A mutant that treats `additional` as space beyond capacity exposes a no-op reservation. This pure case says nothing about syscall behavior or allocation cost. |
| Retained space exceeds service grant | Provision native capacity N, then use a lower logical C and token room T = C; make more than C sources actually ready within the fixture envelope. Retain the requested syscall count and raw return count. | The syscall must receive C and return n <= C. Removing only that logical bound can witness n > C; that alone is not yet proof of lost tokens/OOM. No special allocator behavior is needed to construct N > C. |
| Cold reservation refusal before oneshot | On macOS use one private owned child with validated registration/exit and a separately approved reservation-refusal injector. Preallocate the test's recording fields; restore the injector before reporting. | Refusal must precede dequeue, and a later legitimate wait must still observe the event or reconcile the owned terminal state. Record child status independently. An isolated child fixture proves this ordering, not all daemon caller recovery. |
| Refusal after consumed oneshot | Only after a candidate and reproduction basis are approved: retain the raw successful kevent count, then remove its pre-dequeue output provision and refuse the ensuing conversion growth. Finish cleanup before asserting. | Establish actual consumed notification plus failed delivery, not merely a timeout. A later empty wait alongside independently verified child exit is meaningful only with the recorded initial successful dequeue. Restore exact source and repeat. |
| inotify complete prefix | Use a private fixture with known complete record sizes, including one deliberately named-record case separate from Memory's file-watch premise. Independently retain raw bytes and whether another complete record remains readable. | A short successful read must not be interpreted as EAGAIN. EINVAL on the next too-small record must not be accepted as clean drain completion. Consumer-injected records establish parsing only; actual kernel framing needs the real reader. |

Before approving implementation, the coordinator still needs an exact reproduced
symptom/goal at a named caller, authoritative resource inputs, the selected
delivery/cancel/failure contract, meaningful counterfactuals and paired costs.
This refinement supplies concrete comparisons for that decision; it supplies no
performance claim, architectural selection, new cache or production literal.

## 9. Step 2 checkpoint return

The [current return record](measurements/managed-poller-research/STEP2.md) separates
source observations, derived obligations, historical coordinator results, fresh
documentation checks and entirely unrun runtime work. It returns signed Poller
`df37d254f366e8a7c9d75c876d816927860ee620`, inotify
`c0db0f6ccd6780d9c703e1ae936674f1795e288f` and contract continuation
`6eb84cf671278e140f8018768896f408324e0b28` for coordinator review. The pinned local
citations above are checked against the base blobs in the retained manifest;
the current branch has identical bytes for all those sources. This closes the
research return only. No candidate, replacement number, runtime fix, measured
improvement or managed-runtime completion follows from these document checks.

## 10. TLS handshake capacity: the next approved item

The [TLS return](measurements/managed-poller-research/tls-handshake.md) traces
daemon `HANDSHAKE_BYTES = (1 << 24) - 1 + 4` through the actual locked
hyper-raft `233b7fcef7a363327c5439b191724a38065062cd` transport. The named
Shards source/dependency blobs also match current main
`3ea2164dd98878426936c880f0e8d542ef67d98f`.

RFC 8446's envelope bounds one encoded Handshake message. It does not establish
whole-flight, connection or aggregate admission memory. The pinned QUIC check
bounds receive offset reach; packet backing/metadata, an undecryptable packet
queue, ordered TLS storage and outgoing/retransmission state remain separate.
The TLS parser's 65535-byte body cap and Quinn's historical default are different
mechanisms, not replacement recommendations. Exact primary links and the
reachable-state/accounting obligations accompany each claim in the return.

The return compares four conditional alternatives and gives root a bounded,
entirely **unrun** matrix for cold admission, ordered/reordered/partial messages,
boundary/refusal, simultaneous endpoints, cancellation, retention and decisive
guard/cost checks. It includes individual R01–R16 gaps and a
[separate source-identity record](measurements/managed-poller-research/tls-handshake-source-identity.json).
No size, architecture, crypto change, reproduced defect or production fix is
selected. Work continues with UDP datagram storage/QUIC credits, then the four
coordinator-named VMM ring decisions.

## 11. UDP datagram storage and QUIC credit

The [QUIC return](measurements/managed-poller-research/quic-datagram.md) resolves
managed DATAGRAM_BYTES to UDP scratch/output storage and a receive-turn
calculation. RFC 9221 application DATAGRAM frames are explicitly disabled in this
endpoint. A protocol payload envelope, configured receive advertisement,
peer/path send limit, retained queue capacity and application record are
different constraints. The pinned default receive advertisement of 1472 does
not justify replacing the driver's protocol-max scratch buffer; native
truncation/error behavior and ownership would need proof first.

The Driver already reuses a fallibly provisioned receive Vec. Its copied packet
backing and queued payload capacities have separate lifetimes and costs.
Transport peer flow credit, local stored send bytes, explicit application
Consumed acknowledgment and caller-held guest/daemon reservation are traced
separately. No transport ACK is treated as an application-consumption witness.

The return gives conditional alternatives, exact
[source identities](measurements/managed-poller-research/quic-datagram-source-identity.json)
and an entirely unrun root-only boundary/backpressure/cancellation/credit/cost
matrix. It selects no number, window method, receive API or architecture and
does not propose a speculative terminal fix. The next research item is the
four specific blk/fs/GPU-control/IOMMU-request maxima supplied by root.
