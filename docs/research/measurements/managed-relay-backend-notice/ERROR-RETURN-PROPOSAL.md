# Proposed error-result witness across a faulty work-return notice

IMPLEMENTER relay `01a1c5d2ee0e7248982c95a59f1fd905`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Documentation-only continuation from signed
`40fd147b751c9ea9f6e4366c7e91f690fed852d1`. Full canonical policy/plan/handoff
loaded; canonical five files have no diff69dbc→main4bfaceb38, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
No new production defect, source remedy, native execution or goal expansion.
This additional variant is PROPOSED, not adopted; ROOT decides whether to queue
it after reviewing/running the already prepared fixtures. Existing active goal,
exclusive index and every signed branch/evidence checkpoint remain preserved.

## Question and finite candidate

40fd tests a completed-success BackendComplete result while its earlier notice
is duplicate or absent. The existing extra-datagram fixture independently tests
actual decoder UnexpectedDatagram after a consumed fourth datagram. A finite
combination could require both that original decoder error and a distinct faulty
notice observation to survive into the final evidence. It would be new fixture
coverage, not evidence of a reproduced product failure or a speculative fix.

Proposed explicit command, both modes, no defaults:

```
sock-addr-extra-notice4 MODE WORK_MS BUFFER_BYTES {duplicate-complete|missing-complete}
```

Minimal candidate changes at existing seams:

1. Work::ObserveExtra carries existing BoundaryNotice. Original extra case
   explicitly selects Once; only the new selected case chooses Duplicate/Missing.
   The same notify_work_return helper already accepts Notice at40fd.
2. Add one explicit controller kind for this selected phase. Existing
   Cancellation::ExtraDatagram can carry Notice instead of a parallel manager.
   The owned receipt/receive-entry/completion descriptors remain the same.
3. Wait for actual held third receipt and relay receive entry, consume the same
   one-use FixtureInjection sender, then observe completion. The existing decoder
   must actually consume/count/record fourth BackendUnexpected Binary before
   returning UnexpectedDatagram. No synthetic WouldBlock or second decoder.
4. Original Once follows its original request-cancel path. The selected negatives
   record observer Rejected(InvalidInput/TimedOut) without a successful
   CancelBoundary, then use the existing owned cleanup. No new grace or interval.
5. A new ignored host case requires original decoder Error/UnexpectedDatagram,
   the separate observer rejection, retained receipt/count facts, actual joins
   and owned cleanup. Every existing host case remains byte-identical.

| Fact | Required candidate witness |
| --- | --- |
| Actual effect | Exact Binary+Empty baseline, held third Backend Binary, and actual fourth BackendUnexpected Binary5245515545535400ff from the owned upstream, original backend tuple |
| Original decoder outcome | BackendWorkReturned Error/UnexpectedDatagram and actual joined UdpError UnexpectedDatagram at HoldReply; consumed4/validated3/empty1/replies2 |
| Duplicate notice observation | Aggregate word2, exact-one reader InvalidInput; relay genuinely Cancelled by owned teardown at UpstreamReceive/KernelAccepted with ingress1/sent1/no replies |
| Missing notice observation | Omitted write, observer original TimedOut; relay genuinely Io/TimedOut at the original deadline, same retained counts |
| Lifetime | Work return is not thread exit; owned child reap, actual backend/relay joins, every acquired link detach and cgroup removal remain mandatory |

Original UnexpectedDatagram cannot be relabelled as cancellation, successful
work completion or observer expiry. The observer's rejection likewise cannot
replace the actual decoder record. A notification/report/OS failure at
NotifyWorkReturn already becomes typed Io; that different joined terminal must
fail the proposed strict expected case. This proposal does not change that
behavior or claim it is a reproduced defect. Further remedies require ROOT logs.

No sender/receiver stdout lock order is assumed after an actual send/write; only
same-worker and controller causal order. Missing receipt/error is failure, never
proof of nondelivery. No rollback, UDP EOF, atomic cancellation/external commit,
full kernel return or hard-budget/cost guarantee.

## Basis and alternatives

[Linux v6.18 eventfd counter source](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/eventfd.c)
and [eventfd(2)](https://man7.org/linux/man-pages/man2/eventfd.2.html) support the
aggregate2/omitted-write choice; existing researchf7 is **B/76**, both quotes
found, inference rather than actual runtime result. [UDP's datagram contract](https://man7.org/linux/man-pages/man7/udp.7.html)
supports retaining actual per-datagram receipts rather than EOF or missing-reply
proof; prior researchf5 is **C/68**, one primary quote found and inferred limits.
Existing40fd source/invariants are the local implementation basis, not a live
reproduction. ROOT's actual new-case results and resource costs remain unrun.

A separate manager/queue/pipe/timer/worker/decoder would obscure this same existing
error-result seam. Two separately scheduled writes would weaken deterministic
duplicate input. Only observer logs would omit the required independent bytes
and original decoder cause. Refactoring all successful cases into a general
failure framework is outside this proposal; existing strict semantics stay.
Counts derive from the current script plus its one extra datagram and uint64
signal ABI. Original caller deadlines/envelopes and OS-assigned ports remain.
No new authority, syscall permission, security/trust/crypto or installed artifact.

## Role and evidence gates

Before source implementation ROOT reviews this additional four-case scope.
If accepted, implementer returns only targeted pure/light compilation under NEW
private remote1CPU/1GiB/2GiB admission; ROOT alone executes all actual cases,
mutations/restoration, failures, matrix/hardware and profiles. Paused Windows ARM,
ROOT clones/targets/caches, source/evidence/final binaries remain untouched.
Local free30818938880B is below30GiB; no local cargo/build/compile/largecopy or
agent target/cache deletion. No active LOCAL target/cache. This doc creates no
child session or parallel/independent goal step, and closes no lane goal.

R01/R04/R06/R07/R13: docs only; proposed typed causes/states/ownership remain
separate, no new panic/source refactor. R02/R03/R05/R12: cited current seams,
alternatives and derived finite bounds, no manager/cache/interval/adapter/grant.
R08/R14: no performance/profile/audit claim. R09/R10: no new reproduced defect
or remedy; exact future actual-effect and cleanup/mutation proof remains ROOT.
R11: no new target/hardware pass; macOS arm64 only, Windows ARM paused.
R15/R16: proposal-only signed checkpoint, ROOT owns review/actual campaigns/main
merge; same worktree, small docs, NEW admitted remote check, no local heavy work
or deletion. Prior source-ready and actual correction evidence do not establish
managed-runtime closure; explicit ROOT release remains required.
