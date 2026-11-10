# Controlled inherited-writer descendant depth

Actual first exact-source gates: [RESULTS.md](RESULTS.md), cba6d1c32. Matching
full build, host/native strict Clippy and real strict normal+D2 pass; independent
per-edge wait/population/kernel return remains Unknown. Initial source-only pending
statements below describe the preserved pre-build checkpoint.

POLICY_READ: assigned adversarial/implementer 01a1c5d28cea74a5b0241200190d6c54;
coordinator 01a148884a4770fdaf0111f3f792a248. Canonical base
b57e2fcec7170cad4c2366f7462e3fa5ff3e3fbe, rules SHA256
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Full AGENTS, R01–R16, evidence guide, CLAUDE, active plan and g121 handoff read
before edits. ROOT approved proposal tenant_descendant_depth_minimal_contract_v1
(receipt6779). Prior b1f/629/fabd source, campaigns and all raw/frozen inputs remain
unchanged. ROOT's combined 0ae results are prior evidence, not this revision's gate.

## Owned extension

Only existing inherited-fixture portions of testguest/workload.rs, the existing
current_tenant_stdio.rs leaf and this new evidence directory change. One existing
Fixture/start_with_options, task input, publication, Capture/ReadGoal parser and
common/managed_capture observer remain. No production, parent API, new observer,
wrapper manager, cgroup/namespace layer, runtime authority or installed binary.

Caller D counts physical descendant generations below the actor. D>=2 creates a
finite chain by fresh exec, at most one held unreaped Child per direct parent.
The existing one-child test supplies no depth and keeps its previous mode. The
new testcase requires an explicit depth input; the first assigned case is D2.

The original actor emits the existing prefix. Each direct parent owns its Child,
holds a CLOEXEC pidfd, closes and verifies its own fd1/fd2, releases its child with
the existing single private stdin byte, closes that writer and waits for held
pidfd readiness followed by actual Child.wait/status. Intermediate children close
their own read-only handoff before creating a child; they emit no extra capture.
The leaf alone uses the existing listener, challenge, ready and suffix protocol.
No parent's success precedes its direct child's successful wait, propagating
leaf-up. Stream End does not independently export these native statuses.

For the depth path the first actor reads guest CLOCK_MONOTONIC and creates ONE
absolute work expiry. Numeric argv carries the same expiry and original cleanup
duration across every exec. A local Instant sampled before the clock read bounds
each existing readiness wait conservatively; no host Instant is serialized and
no hop creates a new relative work allowance. Cleanup derives from that same
expiry plus the original duration. Clock errors, checked arithmetic/parse failure,
zero work/cleanup/challenge, insufficient report and invalid depth refuse before
spawn. Acquisition/release/EOF/wait failures use existing owned cleanup; inability
to reap retains the owned Child/handle and Unknown for outer owner stop. A forced
outer stop is not a claim that every direct parent completed its own wait.

## Inputs, effects and bounded runner

NEW private Fedora8fadc1ec213332e16cb0d915a3fb913f:
~/managed-tenant-depth-a920-01a1c5d2-v1/{source,target,records,cache,frozen}.
Kiln admission2CPU/4GiB/24GiB and cargo-j4 includes the source, CLI, all carried
init/server/VM/net helpers, static musl testguest and VM/evidence build footprint.
No local heavy build or Root cache. Production pinned kernel48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7
is fetched to this private cache; the old experimental kernel is not relabelled.

First finite campaign: sequential strict normal +D2; each work240000ms,
cleanup15000ms, hard outer255s. Two distinct supplied guest ports, existing checked
NETWORK/EXPOSE/CONNECT and actual OS-assigned loopback publications. UUID16
challenge, report269=227 existing capture+2*(1tag+4length+16payload). D private
release bytes are separately recorded, not included in that report definition.
TASKS is explicit positive u32 and at least checked D+4: existing trusted server,
sandbox, reaper and actor plus D descendants; initial D2/TASKS6. Prior actual
TASKS4 refused the one-child Spawn while5 passed; these inputs do not establish
hard aggregate/parent/kernel accounting or authorize automatic growth.

Keep mixed runnable IDs1/2 and files-only hole0. Require exact actor initial Open
bytes, new sibling fd1/fd2 and main progress before leaf release, fresh21B echo
with peer EOF, then exact inherited suffix/offsets, canonical End/ACK/transportEOF
and strict final metadata. Preserve the corrected GateEof caption and every
existing caller-specific phase/assertion. Retain one existing top receipt at
stable descendant-ready and after End plus existing asynchronous history. Physical
PID/PPID data and process labels are correlations, not semantic-agent identity,
authority, complete population observation or exact arbitrary writer attribution.

Forbidden effects: early/missing/wrong-channel End or suffix, cross-member bytes,
missing independent sibling/main progress, child release before held pidfd/parent
writer closure, dropped live ownership/false wait success, extra unbounded children
or widened deadline/grants. On failure retain original logs and cleanup/Unknown.
Existing owner stop, observer cancellation/join, stopped PID0 inspection/removal
and host FD baseline comparison remain mandatory. Hard timeout without teardown
is Unknown and prevents a subsequent case until owned cleanup is resolved.

## Primary basis and alternatives

Linux cgroup-v2 documentation specifies child inheritance from the parent's cgroup:
https://www.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html . No per-depth
group is needed. Rust Child documentation requires explicit waiting; Drop does
not reap: https://doc.rust-lang.org/std/process/struct.Child.html#method.wait .
Linux CLOCK_MONOTONIC is a nonsettable system-wide clock:
https://man7.org/linux/man-pages/man2/clock_gettime.2.html . Inspected2026-11-10. Rust std1.99.1 documents UNIX Instant::now using the same
CLOCK_MONOTONIC: https://doc.rust-lang.org/std/time/struct.Instant.html .
This supports the conservative guest-local conversion; other host clock behavior
is not inferred or used to serialize a deadline.
Passing a fresh relative work duration at each exec was rejected because depth
would renew the allowance. Serializing private Rust Instant layout or host time
was rejected because neither is the guest clock contract. A new manager/status
transport, duplicated parser/observer and per-depth namespaces were rejected as
unnecessary. This is fixture adaptation, not a production architectural decision
or defect correction.

## R01–R16 and checkpoint status

- R01/R04/R06/R07/R13: existing owned transition enum reused; explicit depth path,
  checked clocks/arithmetic/conversions, fallible bounded numeric formatting,
  status handling and retained unknown cleanup. Local rustfmt/diff/policy checks0;
  exact native/host compilation and Clippy pending at source checkpoint.
- R02/R03/R05/R12: primary basis/alternatives above; D+4, frame269 and SI time
  conversion/decimal-u64 digit bound are derived, not runtime policy constants.
  Existing ppoll readiness/one-byte handoff and owner reused; no periodic probe,
  cache, second lifecycle authority or new platform adapter.
- R08/R14: no audits/profiles/benchmarks, allocation/leak/resource-guarantee or
  4–5x claim. ROOT owns these and parent/kernel costs remain Unknown.
- R09/R10: no production fix. New exact-source live campaign is pending; preserve
  every original failure, before/after/restoration and uncertainty as encountered.
  No mutation or previous-revision gate is reported as passing here.
- R11: assigned GNU x86_64 KVM with matching static musl guest only. Other real
  hosts/architectures and combined seven-target gates remain ROOT-owned/unrun here.
- R15: exact approved assigned scope, no children/goals/autoplanning/main merge;
  signed named-path checkpoints, source/helper/input readback before actors, ROOT
  review/integration. Held campaigns remain held. Full management/lane OPEN.
- R16: existing clean local worktree reused, old branch and all raw/frozen source
  preserved; observed localfree192440792KiB>30GiB, no local target/cache. Private
  admitted Linux scope only; no cleanup/deletion of any existing target/cache.

Actual commands and subsequent results belong beside this record. The source
checkpoint is not an actual VM pass or lane release.
