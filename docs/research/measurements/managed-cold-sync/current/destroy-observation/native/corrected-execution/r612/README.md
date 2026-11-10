# r612: native14-event case passes; controller FD return fails

**The single native test passed; the overall gate remains FAILED.** Production
strict Clippy, lib-test compilation and test strict Clippy all returned0. Exact
selection returned0. Libtest recorded1passed/0failed/0ignored/46filtered. All14
derived events matched, including four actual raw native destroy returns0 and
four points after the original Box-drop path returned. All eight handles joined;
actual parent ECHILD, both outer NOTE_EXIT observations, parent wait exit0 and
owned socket-directory absence were recorded.

The Python controller's own FD-return assertion failed: **4→5**. It changed the
overall result to exit1, despite the native case exit0. No live FD identities were
recorded, so the extra descriptor's owner, kind and attribution are UNKNOWN. This
is not a reproduced native mutex leak. The final held-group signal also returned
EPERM1; it remains in the original receipt. Neither result is hidden by warming,
changing the baseline, relaxing an assertion or declaring the whole gate passed.
There was no native runtime repetition, deadline extension or source change.

## Policy, source and exact scope

POLICY_READ implementer / scoped ordinary native tester at current origin/main
`ca3ab10c151aa5539e757f1cc76029746ccbb561`, full AGENTS/R01–R16/evidence,
CLAUDE,606-line managed plan and updated g121 handoff reread before work. Rules
SHA256`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
The frozen packet's0342 policy texts are historical; current ca3/df2 and ROOT's
explicit latest GO govern. No old snapshot waives current instructions.

Executed source`10f282bc642f37be54c701bd4ce389574b641e1d`, packet
`23fc4a79d5b46c7baca8024703f60e1b00356691`. The ONLY Rust delta from failed792
is the cfg(test) SharedWitness Debug derive required by the actual
[r598 E0277](../../r598/README.md). The ONLY driver-code delta is its SOURCE
constant. All46 source files and imported Rust leaves were matched to the signed
packet before build and rehashed unchanged after execution. No borrowed connector,
native cleanup branch, wait/stop algorithm, authority or production limit changed.

Runner d132`d1323898b255a3ed435eb83fa0f7b0cf`, ARM64 Mac26.4/25E246,
Darwin25.4.0, preinstalled Rust1.98.1
`48a229ceaefd4985c50990b14116b6d856af0985`, LLVM22.1.8. NEW exclusive root:
`/Users/adalundhe/managed-cold-sync-destroy-10f-v1-79f3b5744f224feaa5d388e53fcd8fec`.
Source/target/cargo-home/inputs/raw/frozen remain preserved there. Kiln admitted
2CPU/2GiB/4GiB, j2, lockf; one aggregate compile/lint180s budget, original case15s,
outer600s. Actual outer elapsed117.925582833s; no timeout. Before/after free disk
2,398,079,418,368 /2,397,814,157,312 bytes, above30GiB. Admission is not an
enforced Darwin aggregate memory/disk quota.

Driver SHA`c7d1eb3ae93833b23a313c6bcb04652a1d87a0dce71e154ce4cce7c7826c0760`;
unchanged held supervisor SHA
`ef7fbf13f92bce72c7f39202556e3da7a946b08e3f857a4622dc70f4c471872e`.
Actual frozen executable is1,803,176 bytes, SHA256
`082048ce5d5dd45270fc16eaeb7ec825b8a5ed8da452a2438a18f303741a43df`,
retained at `frozen/api-final-owner` outside Cargo target. Its hash matched again
after the run. No final binary or old source/raw/cache was removed.

## Actual commands and observations

```text
cargo +1.98.1 clippy --locked -j2 --profile test-release -p shards-api --lib -- -D warnings
cargo +1.98.1 test --locked -j2 --profile test-release -p shards-api --lib --no-run --message-format=json
cargo +1.98.1 clippy --locked -j2 --profile test-release -p shards-api --lib --tests -- -D warnings
frozen/api-final-owner client::destroy_observation::ordinary_final_owner_releases_native_storage --exact --ignored --list
frozen/api-final-owner client::destroy_observation::ordinary_final_owner_releases_native_storage --exact --ignored --test-threads=1 --nocapture
```

All five commands returned0. The existing held-exit supervisor registered before
release, retained unreaped test PID19249 as its group anchor, observed stand-in
PID19250 exit and test exit, then explicitly waited the test. Both owning
ZERO/MAX connections succeeded; the two public waiters returned Some(Stopped).
Two connectors, two waiters, stopper and muxer joined, then the actual API drainer
and watcher joined. While Guest still owned Shared the event channel was empty;
the fixture recovered the unique public Guest Arc and dropped it before consuming
the final-owner observations. Its unchanged assertions passed.

| Actual point | What is established in this one case |
| --- | --- |
| SharedDropEntered | Actual final Shared drop entry after the fixture/API joins and public owner drop. |
| State, Changed, Tail, Drained DestroyEntered/DestroyReturned | Each original native destroy call ran once and returned actual code0. Zero is not used to establish quiescence. |
| Four PointeeDropReturned | The existing Box/ManuallyDrop drop path returned after each successful native call. No allocator callback or physical/kernel release was observed. |
| OriginalSharedFieldsDropped | Original Shared fields finished their Rust drop glue before the test-only last-field witness. The witness's own remaining drop and enclosing allocation are distinct. |
| Sender disconnection | Observation sender ownership ended; no observation retained Guest/Shared/guard/native storage. This is not an Arc deallocation callback. |
| Cleanup | All eight join calls, parent ECHILD, two NOTE_EXIT observations, test wait exit0 and private directory absence observed; controller FD return FAILED and final group signal EPERM1 retained. |

**Enclosing triomphe Arc deallocation remains UNKNOWN.** Native wait parking was
not observed, nor were exceptional native init/lock/wait/unlock/notify/destroy
errors, retention/recovery, foreign cancellation or forgotten guards. These facts
do not establish leak freedom, complete resource return, a legal destroy-error
producer, or a production stale-State/pendingCancel fix.

## Original evidence and transport failures

[raw.tar.gz.b64](raw.tar.gz.b64):10,456 compressed bytes, SHA256
`09321ea8150b6c8e9ff54063076e509c03202f1f9595a3a0185ff827b374559c`,
22 original payloads plus manifest. It retains every stdout/stderr, selection,
native result, before/after identity, source snapshot, current policy/admission,
outer result and FD failure. [verify.py](verify.py) checked all23 members as safe,
unique regular files with exact lengths/hashes, all46 source identities against
the complete signed [corrected packet](../../inputs-corrected-v1/input.json.gz.b64),
the actual14 events and native pass, and the **failed** outer FD assertion.
[verification.json](verification.json) retains those results without redefining
success.

The full source/raw archive is preserved on d132,616,484 bytes,75 payloads plus
manifest, remote SHA256
`ecf21e335648b94ed76859b4387a5a9ce5b8898be46a0f65e8834c6a5e963dad`.
Full compressed bytes were not locally retrieved/verified; the complete source
packet is already signed locally and every original raw stream was retrieved in
the smaller package. Frozen binaries and Cargo artifacts remain on the runner.

Initial part0 transfer did not answer in15s. Its setup was treated as UNKNOWN;
fresh normal-tool inventory and a read-only same-envelope check confirmed the
named directory absent before exclusive creation continued. No uncertain path
was overwritten and no native command had yet been submitted. The actual r612
case then ran once. The r613 read returned captured data/exit0 together with a
missing-receipt transport error; that error remains in
[tool-receipts.json](tool-receipts.json). Subsequent normal archive creation/read
completed and the complete package was independently verified. No SSH, alternate
approval channel, headroom bypass or runtime rerun was used. Initial transfer/
recovery receipts are [retained separately](../transport-and-launch.json).

## R01–R16 and remaining proof

| Rule | Actual evidence / remaining limit |
| --- | --- |
| R01 | Production/test strict lint0, compile0 and one ordinary native case0. Controller FD gate1 remains; no global panic/cleanup/leak-free claim. |
| R02 | Existing pinned Apple/POSIX/Rust/ownership research plus this exact live owner teardown. No alternative manager or cleanup policy selected. |
| R03 | Existing explicit bounds and14 events from four concrete objects/two Shared points. No production sizing/default introduced. |
| R04 | Same authoritative State/Tail/Shared, typed observation only; no mirrored lifecycle or error ledger. |
| R05 | Existing completion notifications, guards and actual joins; no warming/cache/poll/spin/refusal masking or retry manager. |
| R06 | One exact ordinary caller case and its evidence, no corpus/audit/sweep. |
| R07 | Raw return, Box-drop, original-field and enclosing-allocation observations remain distinct; no quiescence inference from0. |
| R08 | No comparison or4–5x claim; elapsed correctness deadline data is not a benchmark. |
| R09 | Actual r598 compile red fixed by one test-only Debug derive; actual r612 controller FD4→5 captured, attribution unknown. No speculative native fix. |
| R10 | Exact source/packet/binary/commands plus original streams and safe verified package; native pass and overall failure retained separately. |
| R11 | ARM64 Mac ordinary case only; no current-main integration, seven-target, VM or other-OS pass. Intel Mac unsupported, WinARM paused. |
| R12 | Same shared/native seam, no new OS mechanism/root/security/crypto/deliverable. |
| R13 | No further source or algorithm change after the approved Debug/SOURCE delta. |
| R14 | No allocation/native-error actor/profiling/benchmark or physical/kernel/Arc release/leak-free claim. |
| R15 | ROOT exact GO, one native attempt, no runtime repetition/deadline increase/children/goals/main merge. ROOT review/integration/full gates owed. |
| R16 | Exact NEW private source/target/cache, admitted envelope/floor, no cleanup/deletion/full worktree. All r598/r124/r125/holds/ROOT scopes/frozen artifacts preserved. |

The controller has exited, so its extra FD cannot be identified retrospectively
from counts. The smallest next proof needs live controller-owned descriptor
identities at the same baseline/after boundaries, with observer effects stated,
before any attribution or correction. That would require ROOT's next exact
scope; this record does not authorize repeating the native case, warming a
baseline, adding a logger/manager or changing a cleanup assertion. ROOT retains
all integration/seven-target/VM/fault/cost gates. The sustained lane remains assigned.
