# Pending cancellation before finite probe I/O

IMPLEMENTER `01a1c5d2ee0e7248982c95a59f1fd905`, coordinator
`01a148884a4770fdaf0111f3f792a248`. Separate branch `p/managed-relay-pending-io`
starts at strict pre-send source `200089e47bc125037a350e0a3954863a86bb6d47`.
Notification fixtures remain independently pushed as `f8d643c59` on
`p/managed-relay-boundaries`; receive-only `7d01dcaf` and strict200 branches stay
preserved. This batch changes only the existing guest probe Deadline seam,
focused pure tests, matching parser formatting and this evidence. It does not
select or complete a product relay implementation.

Full canonical AGENTS/R01–R16/evidence/CLAUDE/managed plan/handoff readback sent
before edits. Current main is coordinator-reported `096c9c7b`; policy files remain
`f5cec365`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
ROOT explicitly approved this narrow correction after its actual r597 red,
including expanding parent guest ownership beyond the prior parser-only scope.
ROOT owns every actual socket/eventfd/namespace/cgroup/BPF/VM/failure/mutation
campaign, audit, profile/benchmark, combined gate and main merge. No subagents.

## Actual baseline before correction

ROOT r597 ran exact probe `0456e99ef475e9e374af0685232c236a3d7b0a4c`, containing
strict200 and the unrelated tenant fixture. Normal source-matching host/helpers
compiled0. Both connected/unconnected pre-send tests were strict101. Each actual
record shows IngressValidated/NotAttempted, CancelBoundary Requested and
PreSendRelease ReleasedAfterCancelRequest, followed by an independent backend
ordinal3 Binary receipt `5245515545535400ff` from the owned upstream socket.
ForwardCompletion is Cancelled / UpstreamReceive / KernelAccepted with ingress1,
upstream_sent1 and no replies. Both child reaps, worker joins, selected link
detaches and cgroup removal are recorded; original time remaining is positive.
This is an actual sent-and-received request after the pre-send cancel request,
not a missing alert or a source-only finding.

ROOT retains originals under
`~/root-managed-boundary-0456-v1/precommit-{connected,unconnected}/` on Fedora.
The implementer read those observed.json/stdout files **read-only** after ROOT's
reproduction receipt. [root-r597-readback.txt](root-r597-readback.txt) preserves
the exact read output, paths, stdout/observation hashes and actual records. Source,
guest/kernel/helper/CLI identities, commands and full test logs remain ROOT's
central record; this read does not become a new runtime campaign or independent
artifact validation.

## Approved correction and precedence

Before correction, Deadline::io checked time, immediately attempted I/O and only
polled cancellation after an actual WouldBlock. ROOT's deterministic trusted
gate therefore exposed a pending cancellation losing to the immediately writable
upstream socket. The correction stays at that existing readiness seam:

1. Check the same original absolute deadline. An expired work deadline returns
   TimedOut before cancellation or I/O admission.
2. If this Deadline has a cancellation FD, reject a negative FD, then perform one
   native zero-time `poll(POLLIN)` readiness observation. It does not read the FD.
3. Recheck the same original deadline after the syscall, then classify readiness.
   No bits admits the operation; pure POLLIN returns ConnectionAborted. NVAL is
   InvalidInput, ERR is Other, HUP is BrokenPipe, and any unknown nonzero bits are
   Other. Failure bits take precedence over readable, never granting permission
   or falsely claiming Cancelled.
4. Only then invoke the actual operation closure. An Interrupted operation or
   readiness syscall retries the existing I/O loop, which rechecks time and
   cancellation before the next operation. No separate retry/poll scheduler,
   periodic interval, guessed sleep or cleanup allowance is added.
5. Actual WouldBlock retains the existing socket+cancel blocking wait and original
   timeout derivation. After that wait, original expiry wins before the same
   explicit cancellation-event classification. EINTR retries admission.

The existing worker remains the sole eventfd consumer and typed Cancelled
classifier. Failure/error/expiry do not become cancellation merely because a
cancel descriptor exists. With already-pending cancellation before the admission
observation, the strict pre-send path should finish at UpstreamSend/NotAttempted
with retained ingress1 and upstream_sent0. Actual ROOT green evidence is still
required; pure checks do not establish this effect.

This establishes **pending-before-check priority only**. Cancellation after a
clear readiness observation can still race the syscall. It does not provide
atomic external commitment, universal revocation linearization or rollback of
any consumed/sent/delivered bytes. Stage/progress updates inside the real send
closure and independent receipt-on-red behavior remain unchanged.

Every real worker Deadline retains its existing Some(cancelFD). The trusted
fixture barrier alone keeps explicit `cancel=None`, test-only, with exactly the
same original absolute `at`. Full byte identities of sock_addr.rs, udp.rs,
udp/forward.rs and host cancel.rs match strict200 in
[source-identities.json](source-identities.json); no original strict assertion,
actual send, worker cancellation observation/finish or trusted gate changed.

## Sources, alternatives and cost

The inspected native seam is the existing Deadline::io libc::poll wait and the
same zero-time readiness pattern in RunningClient::finish. No new OS adapter or
library is needed. Primary references:

- [Linux v6.18 select.c](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/select.c):
  do_poll's no-wait path and existing poll error/readiness delivery. Zero is the
  API's immediate observation, not a picked scheduling interval.
- [Linux v6.18 eventfd.c](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/eventfd.c):
  readiness observes the counter; consuming read is separate. Preserve the one
  authoritative worker consumer, rather than consume in preflight then require
  a second read to classify Cancelled.
- [poll(2)](https://man7.org/linux/man-pages/man2/poll.2.html): negative FDs are
  ignored by poll, zero timeout is immediate, NVAL/HUP/ERR are returned conditions,
  EINTR is an error and scheduling can overrun a blocking timeout. Hence explicit
  negative-FD refusal and repeated checks of the original absolute deadline.
- [Tokio1.48.0 Registration](https://raw.githubusercontent.com/tokio-rs/tokio/tokio-1.48.0/tokio/src/runtime/io/registration.rs)
  and [Envoy1.36.0 UDP proxy](https://raw.githubusercontent.com/envoyproxy/envoy/v1.36.0/source/extensions/filters/udp/udp_proxy/udp_proxy_filter.cc):
  readiness and actual I/O result remain separate. Their engines/defaults do not
  establish this probe's cancellation guarantee or justify importing a manager.

Alternative local-forward-only preflight would affect one caller but duplicate
cancellation policy while other Deadline users retained the same immediate-I/O
ordering. A serialized admission/effect protocol would be broader and necessary
only for a stronger contract; a cache/shared flag/new manager would not make the
check/syscall race atomic. ROOT chose the existing shared guest seam for its live
boundary defect. See the prior signed conditional alternatives on
`p/managed-relay-boundaries` at `756352582`.

The cost is an additional zero-time cancellation poll for each cancelable
admission reaching that observation, plus original-deadline reads after the
observation and blocking wait. EINTR can add failed admissions without an actual
I/O attempt. No syscall is added for a None preflight. ROOT must measure CPU,
latency, allocation/fault and parent/kernel cost; no negligible-cost or 4–5× claim.
The new literals are one held cancellation descriptor, timeout0's documented ABI
semantics and libc event bits; no learned/default capacity or wait number.

## Actual light validation

Own r8, Fedora amd64 `8fadc1ec213332e16cb0d915a3fb913f`, rustc1.98.1; NEW
`~/managed-relay-pending-io-light-v1/source` and private `guest/target`, admitted
1CPU/1GiB/2GiB by Kiln. Admission is not a hard cgroup budget. Policy/owned rustfmt0;
GNU37/musl37 source-linked pure tests0; both scoped production panic/unsafe
clippy subsets0. No actual eventfd, socket, namespace, cgroup, BPF, VM, fault
campaign or profile ran. No host source changed and no host composition was run
for this isolated checkpoint; normal source-matching host/guest/helper and full
matrix/regression builds remain ROOT work.

Pure additions check failure/readable bit precedence and unknown bits, original
expiry before cancel validation, invalid cancel rejection before a closure can
run, and an interrupted closure retry under unchanged `deadline.at`. They create
no descriptor and perform no readiness syscall: expired or invalid guards return
before poll; the no-cancel Interrupted case never returns WouldBlock. The actual
pending event, interrupted waits, concurrent races and native error conditions
are deliberately ROOT validation, not falsely counted as these unit results.

The source-linked guest wrapper remains the existing
`../managed-relay-precommit/light.py guest NEW_PRIVATE_GUEST`; root copies actual
sources to new directories and uses -j1. [light/](light/) retains exact commands,
logs, source manifests, helper/Cargo locks and two built test-binary identities.
All24 text members/six final source paths and four unchanged strict worker files
were locally hash-verified. In-memory text transfer9017B SHA256
`6a9eb1c8ca313943959aa05d1a8ae317b5aa16801a284c6922158d5132e53b42`;
no executable/local archive transfer. Remote source/targets/binaries remain intact.

## Owed ROOT gates and continuation

ROOT must build exact corrected normal artifacts and rerun both strict200
pre-send modes with the same explicit environment. Require actual baseline-only
backend bytes, Cancelled at UpstreamSend/NotAttempted, ingress1/sent0, positive
original deadline remaining and complete owned cleanup. Remove only the preflight
guard to restore the actual third Backend receipt/KernelAccepted red, then restore
the exact source and require green. Preserve raw independent effects and artifact
identities before assertions. The original strict pre-send assertion is unchanged.

Also owe existing receive-cancel and ordinary relay/responder/profile modes,
original-expiry versus simultaneous cancel, EINTR, native readiness/record/OS
failures, seven-target and actual supported hardware gates, and added-cost
measurements. These are unrun here; no final runtime fix/closure or performance
claim. After this signed checkpoint the implementer continues approved finite
early backend completion and peer-end/extra-datagram fixture work. Only ROOT can
confirm managed-runtime closure and release this lane.

## R01–R16 record

| Rule | Evidence / remaining gate |
| --- | --- |
| R01 | No panic/assert/unwrap/indexing/unchecked arithmetic in the correction. ErrorKind conversion and raw readiness use initialized stack data; denied scoped GNU/musl lints0. Actual OS/OOM faults remain ROOT owed. |
| R02 | ROOT exact live effect and explicit design approval; pinned native sources and alternatives above. Broader contract/cost choices remain data-gated. |
| R03 | Only poll ABI zero/single-FD/event-bit facts; original caller work deadline unchanged. No picked threshold/grace/capacity. |
| R04 | Typed io::ErrorKind for clear/readable/failure/expiry; worker terminal owner unchanged. Pure precedence guards pass; actual effect transitions owed. |
| R05 | Existing native readiness and sole eventfd consumer, existing I/O retry/wait loop, no cache/probe scheduler/new manager. ROOT owns audits and actual waits. |
| R06 | One pre-admission at an existing seam and private event classification shared by two real call sites. No generic framework. |
| R07 | Comments state nonconsumption/race limit, native safety and original deadline precedence. |
| R08 | No comparative/performance result; ROOT distributions and misses remain owed. |
| R09 | Exact ROOT r597 baseline precedes correction authorization; independent actual backend effect retained and read back. No other inferred issue is fixed. |
| R10 | Original readback/hashes, final source/commands/logs/binaries and owed mutation/restoration documented. Actual corrected green unrun. |
| R11 | amd64 GNU/musl pure/light only. Full seven-target and real supported VM coverage owed; macOS ARM64 only, Windows ARM pause respected. |
| R12 | Existing guest native seam only; no installed binary/platform adapter/host root/policy/grant/kernel/trust/crypto change. |
| R13 | Strict200 case and send/cancel/worker files byte-identical; selected shared seam correction only, no assertion relaxation or false rollback. |
| R14 | Added syscall/deadline-read cost stated, not measured. No implementer audit/profile/benchmark. |
| R15 | Scoped source/pure checks and signed named-path push; all actual/mutation/combined gates and main merges ROOT-owned. Checkpoint is progress, not lane closure. |
| R16 | Existing worktree/new private remote source/target under1CPU/1GiB/2GiB admission. No local build/cache/full copy/deletion; ROOT targets and paused VM untouched. |
