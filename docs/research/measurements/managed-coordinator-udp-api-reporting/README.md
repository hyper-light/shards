# Coordinator: UDP effects, public identity faults and reporting compatibility

POLICY_READ coordinator: origin/main
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`; rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
All actual guests, mutations and combined gates here are coordinator runs.
These results close no whole managed-runtime phase, release or platform-parity goal.

## Actual UDP guest effects

Fedora KVM r398, exact combined source
`7beb88056fe1ebd4c40179544452afbc172769f1`: all eight cases pass, IPv4/IPv6 ×
local-to-peer/forward × connected/unconnected. Two TCP local regressions pass too.
The experimental kernel/config identities, actual commands, inputs, guest stdout
and host assertions are retained under `linux/root-managed-udp-7beb-v1`.

Each UDP case actually exchanges a binary datagram and a zero-length datagram
before and after interception. Both workers join. The backend retains exactly
the baseline two arrivals/matches, one empty; the relay consumes two matching
arrivals, one empty, and completes its script. The client checks the reply's
original backend address **and port**; ancillary original-destination metadata
and actual source endpoints remain in the raw records. Empty datagrams are not
treated as stream EOF. Source and host exit codes alone are not the effect proof.

This is a transparent **responder**, not an owned forwarding relay. Same-namespace
UDP loopback/self, multi-destination UDP, fragments, PMTU, QUIC, absent-relay
semantics, full route coverage and production installation remain open. Elapsed
times include build/setup; they are not throughput or latency measurements.
The supplied effective config is build-association evidence, not an independent
inference of a kernel's features. No product kernel pin changes here.

Replay with the existing coordinator-only
`../managed-network-intercept/run.sh`, supplying all recorded explicit inputs
and a NEW record directory; the implementer's UDP.md describes its exact modes.
The route and socket mechanisms, source alternatives and limitations remain there.

## Public process-identity guards

Root r405, exact combined source
`46749a35d15c8dd331103a2e8971be936470ed9d`, runs the ordinary public
`Shards.attach` consumer; no private test module substitutes for its call chain.
The source is `../managed-proc-main/public-probe/root-mutations.py`.
Both modes use only their own TGID and a checked nonmatching expected start.
An unexpected guest is forgotten rather than dropped/stopped. The raw-name case
restores and byte-verifies its own name before final assertions or reporting.

| Independent mutation | Actual public-consumer effect |
| --- | --- |
| Replace typed process cause with the former String presentation bridge | Under Rust-global allocation refusal, aborts SIGABRT on a 33-byte allocation. |
| Validate the entire stat record as UTF-8 | A real legal non-UTF8 comm returns `InvalidStartRecord`, so the exact `DifferentStart` assertion fails after name restoration. |
| Replace fallible growth with `Vec::reserve` | Under the same refusal, aborts SIGABRT on an 8-byte allocation. |
| Restore exact original product bytes | Both modes pass; fixed/restored frozen executable SHA256 is identical, `d77bd9c9d656a35fabb759e604e4fe5ae6a52fed5d7cbaf302bf2554f3e07eb6`. |

The fixed OOM path returns the exact PID/start-scoped `CapacityUnavailable` and
the descriptor count returns. The normal raw-name path returns the original
observed start as `DifferentStart`. Every mutation has its own patch, rebuilt
`fresh: false` executable identity and raw output; immutable binaries stay remote.
`restored.diff` is empty and both original source hashes are retained.
Closing ordinary API library and simulator suites pass after restoration.
Raw: `mutations/root-proc-public-mutations-4674-v1`.

Coordinator replay, in an exclusively owned Linux checkout and private target:

```sh
ulimit -c 0
export CARGO_TARGET_DIR="$HOME/root-proc-public-replay-target"
export SHARDS_HELPERS=skip
python3 docs/research/measurements/managed-proc-main/public-probe/root-mutations.py \
  "$HOME/root-proc-public-replay-records"
```

No native allocator, every identity/error caller, process race, complete read
interruption campaign, whole-library panic freedom or leak freedom is established.
This does not resolve the owned-client cold macOS Mutex allocation failure.

## Ordinary API and seven-target combined gates

Current-main-only candidate `66dc1d2cae7d6cf0db234b31b20b36a79dd3b0d9`:

| Actual native host/run | Library | Simulator |
| --- | --- | --- |
| Fedora GNU amd64 r399 | 42 passed, 5 ignored | 15 passed, 2 ignored |
| macOS arm64 r400 | 33 passed, 3 ignored | 15 passed, 2 ignored |
| Windows amd64 r449 | 20 passed, none ignored | 4 passed, none ignored |

Raw includes commands, source/toolchain identity and output under `linux`/`macos`.
Ignored cases remain unverified. Root r404 runs `scripts/lint` on exact combined
`d804bd384516cb8183fea27bfc243825f15b28da`: format and all seven supported targets
pass, including the script's GPU-feature checks. `matrix` retains the actual stage
log and exit0. This is a compile/lint gate, not native Windows tests or real WHP/HVF
coverage. Later source changes do not inherit it. The mutation-driver-only next
commit changes no product or workspace build input; the identity comparison is
recorded separately. Main adoption and migration review remain pending.

## Cold reporting and the rejected serialization assumption

Root native static-report probes: Linux r396 and Windows r397 both print the
expected bytes and exit0 with ordinary and cold Rust-global refusal. Windows'
initial Python Store-alias setup failure is retained; that failed launcher ran
no probe. The corrected native shell launcher used no Python, security-setting
change or trust modification. These limited results do not establish every
diagnostic, native allocator or I/O failure path. The separate cold macOS standard
stderr 64-byte abort remains recorded in `../managed-coordinator-tcp6-startup`.

Mac arm64 r402, exact combined `99474d2e94ae8cdfa17cf84340339761031e9d3c`, tests the
unadopted native C-stream reporting prototype through `managed-reporting-coordinator/root-probe`:

* Unicode/u64 formatting under cold Rust-global refusal returns0 with exact bytes.
* Formatter refusal returns typed `Formatter`; closed integer fd2 returns typed
  I/O `EBADF`. A failure is not converted to success or an invented empty message.
* A rendezvous holds the real Rust stderr formatter while the C reporter runs.
  The native report completes **before** Rust is released; actual stderr is
  `RUST-BEGINshards: NATIVE\nRUST-END\n`. The two locks do not serialize one another.

Thus the candidate survives the tested allocation failure but cannot replace one
standard writer while claiming unchanged mixed-writer serialization. It is NOT
adopted. The prototype's FILE lifetime contract, unavailable native lock status,
native libc allocation, partial/interrupted I/O, blocking and resource costs
remain open. Warming is a diagnostic control, never a remedy. A failed Git SSH
setup before r402 is retained; no host-key or crypto policy was changed to proceed.

Pinned Rust1.98.1 and Apple Libc primary-source identities are retained in the
prototype record and `stdio-primary-source-identities.json`. Apple revision
`71bbe350ab79eef58113991d817ccc6165061a64`'s FILE lock is distinct from Rust's
private stderr lock; its public void lock wrappers discard native mutex status.
This is a source comparison, not a proven match to the running macOS Libc build.
No production architecture or allocation workaround is selected from this result.

## Integrity and policy accounting

Eight initial immutable archive transfers have exact byte/SHA identities in `transfer.json`;
later Windows-API and task-limit transfers have their own named transfer records.
Regular safe paths only were extracted; executable/object/PDB artifacts remain
with their owners and their identities are recorded. The first Linux archive
accidentally retained an ELF probe because its archive filter matched escaped
text; it was not transferred or committed. The preserved second archive and the
local extractor check executable magic. No original evidence was deleted.
The reporting probe's experimentally resolved lock is retained for locked replay.

R01/R04/R06/R07/R13: exact typed outcomes and independent real guard failures,
restoration and cleanup observations; no whole-path panic claim. R02/R03/R05/R12:
existing researched native seams, no new runtime constants/cache/polling/manager,
installed deliverable, host root or crypto change. R09/R10: actual datagrams,
public-caller failure effects and mixed-lock output, original failures preserved.
R08/R14: no cost, allocation-free, competitive or 4–5× claim; complete parent/kernel
accounting and paired distributions are owed. R11: explicit GNU KVM and native
macOS/Windows probe scope; real supported hypervisors and other architectures
remain owed, Windows ARM paused. R15: coordinator owns these campaigns and main
review; implementers supplied only scoped source/light checks. R16: private paths,
no data/credential/source/active-cache deletion, frozen artifacts preserved.

Remaining management work includes hard aggregate and parent/kernel charges,
mandatory full routes and actual forwarding relay, broker/adapters, credential
modes and generation-safe restore. The experimental output topology remains2N
listeners/threads; performance and final topology are unselected. Functional pass
codes do not remove these gaps or authorize a completeness claim.

## Closing combined real-VM regressions

Root r437 runs actual Fedora KVM suites on46749a35d: lifecycle3passed/2ignored,
managed output3passed/1ignored, and the selected ignored limits suite3passed.
No `SKIP:` line occurs. The limits count includes two actual VM cases and the
existing port-probe helper, not three resource campaigns. The output count
includes two real owner-stream cases and one pure metadata case.

The live reaping log records all20 declared orphan-crash rounds: each adopted
child is reaped and the actor's descriptor count stays5→5. A fresh mixed sibling
challenge and main workload continue after the rounds. The output campaign
retains binary channels, stalled stdout with independent stderr, observer loss,
neighbor progress and authentication/scope refusal. Its stalled channel ends
accepted86/consumed58, accurately retaining unacknowledged bytes; other consumed
channels complete. Unknown retained tails stay labelled unknown.

The limits cases observe the actual kernel fork refusal `EAGAIN` while two owned
children remain held; separate fresh neighbor and main challenges answer.
Recovery observes held0/reaped3/replacementPID6/FD6. Held cancellation never sends
Release and never manufactures a recovered publication. The last guest facts
remain `Refused/held`; host/kernel resource return is explicitly unobserved, not
deduced from whole-run stop. Raw commands, inputs, kernel identity and logs are
in `combined/root-managed-combined-real-4674-v1`.

Root r441 runs12 native socket pair cases on the same source: IPv4/IPv6 × TCP,
unconnected UDP and connected UDP × binary/empty payload. All12 actual guest
observations pass, including TCP half-close behavior, with no skips. Raw records
are in `native-sockets/root-managed-native-sockets-4674-v1`. These are native
compatibility regressions after the receive-wrapper change; their identity
records explicitly say interception is unverified. They do not add12 mediation
proofs or settle the unrun UDP loopback/self interception cases.

## Task-limit guard: actual excess admission and exact restoration

Root r448 at `250269aba0b9717dd9819a58b825018a6ad22aed` exercises one finite
recovery cycle with caller-supplied tasks=6/reserved=4/children=2. The guarded case
passes. The only enforcement mutation omits the private guest domain's
`pids.max` write. The actor's existing single excess attempt then actually creates
child PID 5. Test-only observation releases that child normally, observes its
pidfd exit, waits and checks `ECHILD` before publishing PID 5/reaped=1 and
`UnexpectedAdmission`. The real host test fails with exit101 for that actual effect.
There is no unbounded process creation or host cgroup modification.

The instrumentation changes only the unexpected-success witness; it does not
make an extra attempt or weaken admission. Both production manager and actor
source files are restored byte-identically, the final diff is empty, and actual
recovery and held-population cancellation pass again. Raw source patches,
commands, inputs, typed failure and restoration hashes are under
`task-limit/root-managed-task-limit-mutation-2502-v1`. Replay uses the adjacent
coordinator-only `task-limit-mutation.py` in an exclusively owned Linux checkout,
private target and the retained explicit environment. The recorded driver is
immutable; its nonidentical-patch assertion is simplified in the replay source.
These observations prove this guard catches bounded excess admission, not
complete task/memory/descriptor/storage accounting or every cleanup outcome.

The restored log also reported missing host memory-controller watch files.
Root independently read its own real cgroup/mount and ancestor file state:
the directories exist; the lower three levels have empty controller lists and
ENOENT memory interfaces; the service ancestor has memory available but empty
subtree control. This supports controller unavailability there, **not** the
initial pathname-resolution suspicion. The observation is retained under
`task-limit/root-managed-cgroup-observation-v1`. The
[kernel cgroup-v2 controller contract](https://docs.kernel.org/admin-guide/cgroup-v2.html#enabling-and-disabling)
explains that parent enablement creates child controller files. No controller,
host hierarchy or fallback was changed. Kiln headroom requests are not evidence
of a memory-controller hard limit; hard parent/kernel accounting stays open.

## Migration and strict documentation evidence

Reviewed comment-only source commits `2a45ce9db367b7ec6ac298c142f609feaa5cdc2f` and
`08316c12f7c980b580d0182431e31d445f9a86f6` are composed in `c7e87dc506abfe73a7839a32f8ecc514993ab63c`.
The [migration guide](../../../development/managed-safety-migration.md) names both
public startup return-type changes, the new refusal variant, retained traits,
identity/uncertainty and the separate unresolved macOS allocation failures.
The original strict API+VMM Rustdoc attempt actually fails with exit101 on preexisting
references. Narrow reference-format corrections make that exact broken-link
gate pass; 22 private-link warnings remain unsuppressed. The `StartError`
compile-only example passes (1 passed, 0 failed). Commands, original red, fixed logs, source hashes
and all R01–R16 accounting are retained in `migration`; no runtime logic or API
change is introduced by this documentation batch. Native Windows r449's 20/4
results and actual 66dc source/status/toolchain are under `windows-api`.

Closing root r455 runs format and all seven supported targets, including GPU checks,
on exact `c7e87dc506abfe73a7839a32f8ecc514993ab63c`: all pass. The actual stages,
source, log and exit0 are retained in `matrix-c7e8`. This includes the UDP-local
fixture and composed migration comments, not the still-unimplemented BPF
comparison or every native hypervisor. Later record-only edits do not add a
runtime result.

## Native Windows x64 WHP smoke gate

Root r462 ran exact main-only safety source
`08316c12f7c980b580d0182431e31d445f9a86f6` on Jupiter, with real carried
helpers, `SHARDS_REQUIRE_VMS=1` and a private empty Docker configuration.
All four selected tests pass: backend identity, actual guest PID1 boot/power-off,
invalid VM configuration and malformed VM arguments. The boot assertion requires
the guest PID1 marker and its init marker before VM exit; this is an actual WHP
guest result, not just capability detection or compilation.

The 22 raw records under `windows-whp` retain exact commands, logs, original
exits, source/status/toolchain and test/runtime artifact hashes. The pinned
Windows hypervisor kernel is SHA256
`43219b3751e5f60c29a12fb31518b7a11606d7a3796f68a152740c41c7bd81f6`.
AWS-LC's compiler-feature probe warning remains in the unmodified successful
build logs. No host admin, Defender, firewall, trust or global tool setting changed.
This gate covers neither the full suite nor Windows ARM, managed capture or the
experimental network mechanisms. The Windows ARM VM remains paused.
