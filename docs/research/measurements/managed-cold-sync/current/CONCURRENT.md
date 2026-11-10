# Ordinary owned connect/wait/stop concurrency

Both assigned ordinary interleavings pass on frozen production source
`cd0649bfedf2537bb4f9878b6e82c2c6b6e5faf0`. Owning ZERO/MAX callers, two waiters,
the stopper and muxer all joined before the fixture's one caller deadline; the
actual parent independently observed waitpid=-1/ECHILD. No ordinary stale-State
or pendingCancel symptom was reproduced. No production correction follows from
these cases, and the native-error/retained-storage obligations remain OPEN.

## Identity and scope

POLICY_READ implementer / assigned-adversarial
`01a1c5bda0bf766683384c0705c0a9bc`, ROOT
`01a148884a4770fdaf0111f3f792a248`. Full canonical AGENTS, R01–R16, evidence,
CLAUDE, managed plan and `/Users/adalundhe/g121-handoff.md` were reread at current
main `a920c85b5c0c9d40aafc4f4cce32432c7c0aed79`; rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Handoff SHA-256 `02c673d70e112d7f5cbf5474230d883befcbd785469740c29ccc0990fa45ba6f`.
The user's new main receipt supersedes the older main value still in that handoff.

This continues the already assigned frozen cd0649/727471 campaign, without
rebasing or substituting a newer revision mid-campaign. Main33df -> a920 API
client/lib bytes are identical by source comparison; this is not a native pass
on the new whole-main revision. ROOT's OTel/KVM/Collector/profile results remain
separate. Full management P0–P13/G01–G25 remains OPEN.

Owned changes: `current/concurrent_case.rs`, preparation/runner scripts, the
existing held-exit helper's explicit selector argument, and this evidence.
Product sync/client/lib bytes remain exact cd0649. The exported client differs
only by private cfg(test) module registration. The ordinary closure explicitly
omits native_case.rs and its global allocation-refusal hook. No worker-result
fault, hostile actor, native-error injection, lifecycle ledger or borrowed-path
change is included.

## The two cases

Each case calls the real `client::start`, using the existing finite sleep stand-in
and a muxer at that guest's own private Unix socket. The fixture declares two
ports, one each for owning ZERO and MAX. The muxer reads both actual CONNECT
requests before any reply; both connect completion channels must still be empty.
Two other threads call actual Guest::wait_for. Their entry acknowledgments are
labelled announcements, not proof of a native CV park.

1. **AnswerThenStop:** answer both held requests. ZERO and MAX both return a
   stream; dropping those streams leaves lifecycle control with the same Guest.
   Stop runs on its own thread while the waiters remain pending. Both waiters
   return Some(Stopped).
2. **StopHeldHandshakes:** stop while both actual CONNECT requests are held.
   Both waiters must observe Some(Stopped), then the fixture closes the muxer
   peers. Both pending owning calls return Err(Ended(Stopped)). This checks the
   declared ordering; it does not prove stop independently interrupts a separate
   live muxer that never closes or replies.

One Instant deadline is created before setup and reused by fixture readiness,
channel waits and post-join checks. Owning ZERO/MAX arguments are passed unchanged;
they are not replaced with the fixture's finite patience. The outer held-process
supervisor also enforces its original15s case deadline. FD poll is finite socket
readiness, not a guest-state polling loop. All two connector threads, two waiter
threads, stopper and muxer are explicitly joined, with deadline checks. A final
nonblocking accept finds no extra queued dial after those callers have joined.
This does not join the API's detached internal drainer/reaper handles or establish
a whole-process thread/descendant census.

The real parent then calls waitpid(WNOHANG) and requires -1/ECHILD. Outer kqueue
observers were registered for test/stand-in exit before releasing the fixture.
Both exits were observed and private socket directories were absent afterward.
Completed-group kill attempts returned EPERM, preserved as OS errors; no security
setting or signal workaround was attempted. The controller explicitly waited its
held test process. No general leak-free or kernel-resource-return claim follows.

## Actual commands and results

Runner `d1323898b255a3ed435eb83fa0f7b0cf`, ARM64Mac/Darwin25.4.0,
Rust1.98.1 commit `48a229ceaefd4985c50990b14116b6d856af0985`, LLVM22.1.8.
sync:false, NEW private source/target, admission1CPU/1GiB/2GiB, -j1,
codegen1/LLVMthreads1, compile180s/case15s/outer600s. Actual free disk exceeded32GiB
before each build. Admission is not an enforced Darwin aggregate memory/disk quota.

```text
python3 concurrent_prepare.py --out NEW_SMALL_TRANSFER_DIRECTORY
python3 concurrent_run.py --bundle INPUT.json --helper binding_run.py --root NEW_PRIVATE_SOURCE_ROOT --case-seconds 15 --compile-seconds 180
cargo +1.98.1 test --locked -j1 --profile test-release -p shards-api --lib --no-run --message-format=json
cargo +1.98.1 clippy --locked -j1 --profile test-release -p shards-api --lib --tests -- -D warnings
```

| Run | Actual outcome |
| --- | --- |
| r124, first fixture | Compile0; each selector exactly1; both native cases0/no timeout, six fixture joins/ECHILD each. Strict Clippy101 on eight println diagnostics only; raw source and failure retained. |
| r125, final fixture | Only fixture diagnostics changed to nonpanicking writeln. Fresh source/target compile0, both native cases0/exact1/no timeout, six fixture joins/ECHILD each, strict API lib/tests Clippy0. |

Final fixture SHA-256
`431e2ea29a2e6d257f0720063ba1a1c009ca9f6793b6453ee637462c0c620a80`.
Final native test binary SHA-256
`b7eb4e931421c35277c93021b996e7839f19937840dadcbcf22f5df55a639650`.
The exact binary was rehashed and copied outside Cargo target to
`/Users/adalundhe/managed-cold-sync-concurrent-v1-15_wfvut/frozen/concurrent-v2`.
Initial binary/source/raw are also preserved, not rebuilt or relabelled passing
strict Clippy. No production file changed to address a source-only theory.

[Archive index](binding-evidence/concurrent-archives.json),
[final input identities](binding-evidence/concurrent-input-v2-identities.json),
[scope receipt](binding-evidence/concurrent-policy-receipt.json) and two encoded
archives retain commands, environment, exact source, native stdout/stderr,
selections, native exits, cleanup, initial lint failure and final Clippy result.
Final archive SHA-256:
`28b85299a3284b857c97c8309872120b6bdb0d84503bc6a3126829c01874cbd7`.
Initial archive SHA-256:
`c57c9c2d3a679f7f7db061e0a375a84403f510bb2692ed0c4a8feb3c9e6c3794`.
Both archive hashes, safe unique regular paths and all50 original member size/hash
entries were verified locally. Raw whitespace/blank EOFs stay byte-exact inside
the text-encoded archives. Derived originals remain under
`/Users/adalundhe/managed-cold-sync-binding-raw-retained/concurrent-v{1,2}`.

## R01–R16 and remaining limits

| Rule | Evidence / remaining obligation |
| --- | --- |
| R01 | Actual ordinary calls/joins/reap pass. Only fixture diagnostics fixed after actual lint red. Native failure/destroy/prefix and global allocation/panic behavior remain unverified, not leak-free. |
| R02 | Existing pinned primitive/ownership research in BINDING; this tests the approved real caller seam without a new mechanism. |
| R03 | Two callers/ports distinguish ZERO/MAX and exercise multiple waiters; all numbers are fixture inputs/protocol/unit bounds. No production sizing/default changed. |
| R04 | Explicit fixture orderings; same authoritative production State/Tail and typed Ended(Stopped). Native error/in-flight stale-State obligations in MUTATION remain OPEN. |
| R05 | Channel/readiness notifications and original deadlines; no production polling/spin/cache, new phase ledger or manager. |
| R06 | Two bounded ordinary interleavings at existing API calls; no repository audit or broader rewrite. |
| R07 | Pairing/ownership/readiness/park limits documented; strict Clippy0 after nonpanicking diagnostics. |
| R08 | No cost ratios, comparable workload or4–5x result claimed; ROOT owns those gates. |
| R09 | No ordinary defect reproduced. Source-only stale-State/pendingCancel concern remains OPEN and is not permission to fix it. Initial Clippy red is a fixture lint failure. |
| R10 | Both original and final source/raw retained, actual native outcomes independently checked; prior cold red/guard-removal/restored proof stays linked in BINDING/MUTATION. |
| R11 | ARM64Mac focused checks only; no inherited a920 integration/7target/other host/VM proof. Intel Mac unsupported; paused WinARM untouched. |
| R12 | Actual shared API primitive unchanged; no OS-specific product copy, installed binary, root/admin or crypto/security change. |
| R13 | Existing stand-in/muxer/held-exit seam reused; no new public synchronization or lifecycle interface. |
| R14 | No profiling/benchmarks/allocation/kernel/parent cost measurements; ROOT owns full resource evidence. |
| R15 | Assigned finite ordinary scope, sole writer, no child agents/goals/autoplanning or main merge. Small signed checkpoint, not lane release. |
| R16 | NEW private small remote closures only; no local compile/target/cleanup/full worktree. Frozen sources/binaries/raw and protected ROOT caches preserved. |

Failed native destroy storage remains retained by the production implementation;
no destroy refusal or successful recovery was injected/observed here. Unpublished
cleanup retry ownership, already-blocked CV/native-error wakeups and exceptional
in-flight identity/reap races still require ROOT's scoped decision and real red
before another correction. These ordinary results do not close those obligations.
