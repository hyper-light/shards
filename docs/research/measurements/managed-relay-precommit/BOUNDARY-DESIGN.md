# Conditional cancellation and finite boundary cases

Implementer continuation at signed `200089e47bc125037a350e0a3954863a86bb6d47`,
reusing the existing worktree on `p/managed-relay-boundaries`. Receive-only
`7d01dcaf1ed7b46fe12eea6fcd33b2e50658f520` and pre-send `200089e4` stay on their
original pushed branches. Full canonical policy/plan/handoff readback was sent to
coordinator `01a148884a4770fdaf0111f3f792a248` before edits. Policy main `24f48ee5`,
files `f5cec365`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Role: IMPLEMENTER. ROOT owns all audits, actual event/socket/namespace/cgroup/BPF/
VM/failure campaigns, profiles, benchmarks, combined gates and main merges. This
record selects no production remedy and reports no new runtime result.

The coordinator's current handoff reports normal host compilation and ten actual
`68362c3e` cases with expected outcomes, including both receive-cancel modes. The
`70178457` change addresses only parser formatting from the separate matrix red.
Neither result establishes the new pre-send expectation. Actual pre-send logs,
independent third backend receipt on a send, and exact artifact identities remain
the prerequisite for any behavior fix. See [the unchanged strict fixture](README.md).

## Existing seam and conditional alternatives

At `200089e4`, `Deadline::io` checks the original time before trying the operation;
it polls cancellation after a real WouldBlock. The trusted pre-send barrier uses
`cancel=None` and **the same original absolute `at`** so its next actual send owns
the pending cancellation observation. An immediately successful send could
therefore precede that observation. This is a source inference, not a reproduced
defect. No assertion is relaxed to accept it.

Only if ROOT retains an actual red at this boundary:

| Alternative | Contract and implementation seam | Cost and limitations |
| --- | --- | --- |
| Shared Deadline preflight | ROOT could check the existing cancellation descriptor for readiness before each real operation, without consuming it; existing worker terminal classification would remain its consumer. Interrupted retries must recheck the original deadline and cancellation. | Applies consistently to all users but changes shared semantics outside this lane's parser-only ownership. Adds a cancellation readiness syscall on applicable I/O attempts. Cancellation arriving after preflight can still race a send. ROOT must measure and validate all callers before selecting it. |
| Local forward-worker preflight | The existing worker could decide pending cancellation at `UpstreamSend` before entering the syscall closure, returning a typed terminal without losing report facts. It must either leave the token to the existing classifier or return Cancelled directly; consuming it and later requiring a second read is invalid. | Smallest affected caller, but duplicated policy would cover only this send seam. Same check/send race remains; it establishes pending-before-entry priority, not atomic rollback or universal cancellation linearization. Needs ROOT's exact contract choice and actual red first. |
| Serialized effect admission | The existing authoritative worker could own an explicit admission/commit transition if a stronger concurrent-cancel contract is actually required. | Broader design and measurement work; a new manager, cache or shared flag does not solve the syscall race. Not approved for this finite fixture batch and not a proposed immediate fix. |

Keeping the current behavior and relabelling a send as NotAttempted would erase
an actual effect, so it is not a corrective alternative. Preserve Attempted once
the actual send closure runs, KernelAccepted after a complete return, and the
independent backend bytes when delivered. No cancellation can roll back those
facts. The existing receive-cancel predicate must remain strict.

ROOT's decisive record must contain exact source/host/guest/helper/kernel/config
identities; ingress bytes and tuple; requested cancel before release; final
stage/progress/counts and typed error/terminal; actual third Backend bytes/tuple
if sent; both child reaps, both worker joins, every link detach and cgroup removal;
and original deadline remaining or actual expiry. No missing alert proves
nondelivery. After an approved remedy, ROOT owes the corresponding guard removal,
exact restoration and ordinary/receive-cancel regressions. No such runs occurred
in this implementer session.

## Minimal notification and early-completion proposal

Proposal sent with stable keys `relay-notification-design-200089e4-v1` and
`relay-notification-design-200089e4-v2` before source implementation. The latter
requires the error gate to remain unreleased. Proposed explicit command:

```
sock-addr-boundary4 MODE WORK_MS BUFFER_BYTES {duplicate-ready|missing-ready|backend-complete}
```

The new finite scenario enum belongs at existing ControllerKind/Control and
Work/ReplyPlan seams; old commands retain their exact strict behavior. No new
manager, decoder, generic fault framework, implicit flags, synthetic WouldBlock
loop, sleep or repeated state probe is needed.

| Case | Exact transition/signal | Required distinct outcome and retained facts |
| --- | --- | --- |
| duplicate-ready | After one actual ingress has been recorded/validated, write one uint64 word of value two to the existing ready eventfd. One aggregated write avoids a scheduler race between two single writes. Leave the trusted release gate unreleased until the original deadline. | The existing exact-one reader must reject InvalidInput; the retained worker later reports its own TimedOut. A reporting failure stays an error. The controller records the boundary error, then ends/reaps its owned child and joins workers before detaching links/removing cgroup. Retained ingress1 and NotAttempted are required; this case cannot claim a successful cancellation or nonreceipt from missing records. |
| missing-ready | At the same retained-ingress boundary, explicitly omit the ready write. Parent awaits the original absolute deadline, never a shorter invented timer or replacement deadline. | Boundary TimedOut is required and must not become Cancelled. Already consumed ingress and unsent progress survive worker expiry. Cleanup still runs; positive work time is not required after deliberate expiry. Child status, reap and worker error may differ from the successful cancellation case and must be recorded as actually observed. |
| backend-complete | After the real forwarded Binary receipt, record/validate it, then complete the existing backend worker without issuing a reply. Parent observes and joins that actual completion before cancelling/joining the relay. | An explicit controlled-complete terminal distinguishes this from ScriptComplete and Cancelled. Backend3/matched3/one Empty/two baseline replies and forward ingress1/sent1 persist. Relay cancellation, native I/O error and original expiry remain distinct; no claimed UDP EOF or rollback. |

For both notification errors, the selected gate remains unreleased. Releasing
after a cancel request could itself allow a send under the unverified existing
ordering. Both negative cases therefore consume the original work deadline and
cannot require positive remaining work time. A failed notification cannot be
treated as successful cancellation. Retain an independent Backend receipt whenever a completed send is
reported, including error cleanup. The existing reader's InvalidInput outcome is
already typed through io::ErrorKind; new scenario/controlled terminal choices
remain enums, not string-selected internal behavior.

No cleanup grace enlarges `deadline.at`. The current OwnedClient kills/reaps even
after its timed wait fails, then returns the wait error; no successful work-time
claim follows from that cleanup. The outer coordinator VM remains the bound for
the existing unknown-reap pending state. Hard process/kernel accounting remains
unproved.

## Finite peer-end and unexpected-extra proposal

UDP supplies no reliable stream-close indication. Use an explicit owned backend
completion, its actual joined terminal and the relay's actual error/cancel/expiry;
do not reinterpret an empty datagram as EOF or a missing reply as peer death.

For an independent extra-datagram case, retain a clone of the **existing owned
upstream socket only in this selected fixture**. After the baseline Binary+Empty
and one held actual forwarded Binary receipt, the trusted parent sends exactly
one additional declared Binary request through that same socket. Record the
actual send return and actual backend `BackendUnexpected` bytes/peer/original
tuple. A completion notification written after the backend's typed failure lets
the parent join it before cancellation can hide the readable datagram. The
existing decoder must consume/count the datagram and return UnexpectedDatagram;
do not fake WouldBlock while it remains readable.

Expected backend observations are Binary, Empty, held Binary, unexpected Binary:
four consumed, three validated, one Empty, two baseline replies. The unexpected
datagram is deliberately not validated as a normal script step. These counts
derive from the finite script, not a runtime capacity. Existing forward counts
remain ingress1/sent1/no reply; the parent's injected send is separately recorded
and cannot be attributed to the relay's upstream_sent count. Same native socket
preserves its actual source identity without reuseport, NAT, raw packets, new
actor access, privilege or grant changes.

Signal ownership and one completion word must be explicit; no shared completion
cache or second worker manager. ROOT must approve the exact additional state/
signal and execute all real effects, failure/mutation/restoration and cleanup.
Only pure report/argument/state checks are implementer work. This section is a
proposal, not implemented behavior or an actual socket result.

## Research and trust limits

Research `01a1c5d332c07528b80098796389b351`, plan
`01a1c5d332bf71459911474ba53ea603`; all cited quotes located by Kiln. Its grades
describe source support, not runtime validation. Each conclusion below is an
inference from the named sources. Full findings/quotes remain in the Kiln record.

| Finding | Kiln grade/score | Sources and resulting scope |
| --- | --- | --- |
| Pending cancellation may lose to initial immediate I/O; live reproduction precedes any fix. | B / 72 | `200089e4` Deadline::io and forward trusted barrier; canonical R09. Source ordering only. |
| Aggregated duplicate and omitted notifications fit the existing eventfd/read seam. | B / 80 | [Linux v6.18 eventfd.c](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/eventfd.c), [eventfd(2)](https://man7.org/linux/man-pages/man2/eventfd.2.html), `200089e4` udp::{notify,notified}. No new queue implied. |
| Existing worker owns cancellation/join; controlled completion belongs in its explicit variants. | B / 72 | `200089e4` udp::{Work,ReplyPlan,WorkerState,scripted} and forward progress/report. Actual new transitions unverified. |
| Readiness and real I/O result are separate observations. | B / 79 | [Tokio1.48.0 registration::try_io/poll_io](https://raw.githubusercontent.com/tokio-rs/tokio/tokio-1.48.0/tokio/src/runtime/io/registration.rs), [Envoy1.36.0 UDP onReadReady](https://raw.githubusercontent.com/envoyproxy/envoy/v1.36.0/source/extensions/filters/udp/udp_proxy/udp_proxy_filter.cc). Neither supplies a Shards cancellation guarantee. |
| UDP peer-end requires actual terminal/error/expiry witnesses without a fabricated stream EOF. | C / 68 | [udp(7)](https://man7.org/linux/man-pages/man7/udp.7.html). Protocol semantics only; native peer-close outcomes remain ROOT observations. |

## Evidence and R01–R16 accounting

This checkpoint changes documentation only; runtime/predicate sources remain
byte-identical to `200089e4`. The mechanical policy check passed in NEW remote
`~/managed-relay-boundary-docs-v1/source` with private target and admitted
1CPU/1GiB/2GiB. [Exact result, command and policy-input hashes](boundary-docs-light.json)
are retained; the remote command/log/source snapshot remain preserved. An initial
uncompressed command exceeded the OS argument bound before spawning; the retry
used a compressed in-memory payload, with no local archive. No compiler, unit
test, eventfd, socket, VM or profile executed. Policy input bytes are unchanged
after this evidence-only paragraph update.
ROOT has not selected a corrective design or supplied this session an actual
pre-send red. Proposal review, fixture implementation and all live gates remain
open. A signed checkpoint does not release the lane.

| Rule | Applicable result / remaining work |
| --- | --- |
| R01 | No runtime edit or new panic path. Existing error/panic behavior is not certified by a document. |
| R02 | Pinned primary source alternatives above; actual selection/cost data remains ROOT work. |
| R03 | Only proposed finite script/ABI-derived counts; no production number or deadline default. |
| R04 | Explicit scenario/controlled terminal proposal; implementation and transition evidence pending. |
| R05 | Existing authoritative worker/decoder/eventfd/deadline ownership; no new cache/manager/poll loop proposed. ROOT owns audits and actual wait proof. |
| R06 | Finite cases at existing seams; code complexity review follows implementation. |
| R07 | Text distinguishes inference, syscall acceptance and independent receipt. No rollback claim. |
| R08 | No comparative/performance result; ROOT equivalent-baseline measurements owed. |
| R09 | No fix before exact ROOT live red; source inference labelled. |
| R10 | Exact source/policy/Kiln identities and root evidence checklist; real mutation/restoration unrun. |
| R11 | No new platform result. Seven targets and real supported hosts remain coordinator gates; macOS ARM64 only, Windows ARM paused. |
| R12 | Existing seams only, no new platform adapter/installed artifact/host root/trust/crypto/grant change. |
| R13 | One finite case per scenario, original strict fixtures preserved; implementation not yet claimed. |
| R14 | No implementer profile/benchmark; allocator/copy/fault/parent/kernel costs unknown. |
| R15 | IMPLEMENTER scoped docs/proposal only, no delegation/live campaign/main merge; root review and runs owed. |
| R16 | Existing worktree, no local build/full copy/archive; local floor breach reported, no deletion. Any later light check requires NEW private remote source/target and Kiln1CPU/1GiB/2GiB admission. |
