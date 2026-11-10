# r598: production lint passed; test compile refused before native execution

**No native teardown case ran.** On frozen API/test source
`79276d1923811bde0b275dda7afa98f587737f3f`, production strict API Clippy returned0;
lib-test compilation returned101 with E0277. Shared's existing Debug derive
requires its new cfg(test) SharedWitness field to implement Debug. The actual
diagnostic names client.rs355/367 and observer.rs58. Test strict Clippy, selector,
test/stand-in processes, joins/reap/NOTE_EXIT and destroy events are **UNRUN**.
There is no libtest pass/fail/ignore count and no valid destroy error/leak result.

The exact correction prepared after this red is only:

```diff
-#[derive(Default)]
+#[derive(Default, Debug)]
 pub(crate) struct SharedWitness(OnceLock<Sender<Event>>);
```

Corrected observer SHA256
`69daba674bd8be3f32e37d131f1780791f11f61d4e5c234beb71997091f7f07b`.
Its parse/format check passed. **Corrected-source Rust compile/lint/native runs
remain unrun.** No cleanup branch, actual State/Tail owner, worker, borrowed
connector, lifecycle algorithm, native return or deadline changed. The original
failed source remains in Git792, the signed input and the private runner source.

## Exact source, runner and commands

POLICY_READ implementer / scoped ordinary native tester, current main
`0342b85d9c808e3ced041a6c3ef0e9bf70438272`; full AGENTS/R01–R16/evidence,
CLAUDE/managed plan/updated handoff reread before work. Rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
ROOT's explicit approval was ONE ordinary legal-final-owner case after source
review, preserving any failure and prohibiting retry/deadline increase.

Launcher/snapshot signed
`c08ac67227c56e868bb214b339a2a218204757ab` was sent to ROOT before execution.
Mac d132 `d1323898b255a3ed435eb83fa0f7b0cf`, ARM64 macOS26.4/25E246,
Darwin25.4.0; installed Rust1.98.1 commit
`48a229ceaefd4985c50990b14116b6d856af0985`, LLVM22.1.8. Kiln admitted
2CPU/2GiB/4GiB, j2; existing lockf, outer600s, one aggregate180s compile/lint
deadline and original15s case. No toolchain was installed.

Exclusive source/target/cargo-home/inputs/raw/frozen remain under:
`/Users/adalundhe/managed-cold-sync-destroy-792-v1-d025a65bc8ca441aab79b1f9815e3c24`.
No ROOT cache, old source/raw, paused VM or pre-existing path was altered.
The job ended1 after10.2373925s. The controller FD count returned4→4; this is
only that Python controller, not API worker/child or kernel cleanup evidence.
Free disk before/after build was2,401,024,466,944 /2,400,972,836,864 bytes,
above the30GiB floor. Admission is not an enforced Darwin RSS/disk quota.

Actual source snapshot was rehashed and written **before build**, and all46
file hashes also matched afterward. Driver SHA
`9087e24ff4a8af9664b57ee398cbfc2f1f95c2f11de687e3422717169ec73c3e`;
held helper SHA
`ef7fbf13f92bce72c7f39202556e3da7a946b08e3f857a4622dc70f4c471872e`.
No old cold allocator actor was registered or executed. Raw cargo commands were:

```text
cargo +1.98.1 clippy --locked -j2 --profile test-release -p shards-api --lib -- -D warnings
cargo +1.98.1 test --locked -j2 --profile test-release -p shards-api --lib --no-run --message-format=json
```

The first returned0, the second101. No later stage was dispatched. There was no
native retry or deadline extension. Further execution is held for ROOT review
of this actual compile red and minimal corrected test source. All original
private files/caches remain preserved, with no active own job after collection.

## Durable evidence and verification

[raw.tar.gz.b64](raw.tar.gz.b64) preserves every original stdout/stderr and
before-build/summary/outer/source snapshot receipt, plus derived exact compiler
diagnostics. SHA256 of compressed bytes:
`d8070a3ca76835cbc82066bc1c98855ad85e954e72a26209823485e49563258d`,
11,113 bytes,14 payloads plus manifest. [verify.py](verify.py) checked safe unique
regular paths and every original size/hash, both actual command results and the
zero native-attempt state. It also matched all46 source-snapshot file hashes to
the complete signed local [input](../inputs-v1/input.json.gz.b64), whose compressed
SHA is`fa8e3dfa0f2a5463029f5ead19b79d3e8be96ddb0d039440d8c7e2732a8a6bd3`.
[verification.json](verification.json) retains that exact result.

The larger original `r598-source-and-raw.tar.gz` remains on d132:616,913 bytes,
67 payloads plus manifest, remote SHA256
`6b8d6edbf391e0a9573e1e409877f2208d968e0f138160d2f348e0b3ecb3d47e`.
Its full compressed bytes were **not** locally retrieved/verified. Only its
first24,000 base64 characters are preserved in
[full-archive-first-part.b64](full-archive-first-part.b64), explicitly partial.
The complete source already exists in the signed local input; all original raw
streams were retrieved/verified in the smaller package. No full-archive local
verification is claimed from a prefix or remote digest.

The initial archive-read wrapper treated async r602 as a failed bounded read.
The SAME read was resumed and ended0; its accepted partial bytes were retained.
This was transfer orchestration, not another Rust/native attempt. Tool receipts
are in [tool-receipts.json](tool-receipts.json). The earlier copied-supervisor EOF
whitespace check2 remains recorded in [launch checks](../launch-checks.json),
without a false full-whitespace pass. No lint/test was relaxed.

## R01–R16 and owed gates

| Rule | Actual evidence and remaining limit |
| --- | --- |
| R01 | Production strict lint0; actual test compile E0277 retained. One test-only Debug correction parse/format0; corrected Rust/native validation unrun. No production panic/cleanup claim. |
| R02 | Existing pinned destruction/ownership alternatives and actual compiler diagnostic justify only this test-interface correction; no new architecture. |
| R03 | Existing explicit resource/deadline envelope, no production number or copied default. Four tags/event bound remain derived, unobserved live. |
| R04 | Same authoritative Shared/State/Tail; no mirrored lifecycle/error ledger, native result substitution or transition change. |
| R05 | Existing waits/notifications, per-instance witness only; no warm/cache/poll/spin/retry manager. Native wait/notification activity unrun. |
| R06 | Focused test witness trait requirement and own closure/receipts; no audit or broader rewrite. |
| R07 | Code/Box-drop/original-field/enclosing-allocation distinctions preserved; no quiescence inference from0. |
| R08 | No comparative result or4–5x claim; correctness-run elapsed time is not a benchmark. |
| R09 | Exact E0277 reproduced before its one-line test correction; no valid destroy failure/leak or ordinary stale-State/pendingCancel symptom. |
| R10 | Immutable source/launcher snapshot, complete raw streams, actual red, exact correction and verified package; no false native pass/ignore count. |
| R11 | Native ARM64 Mac compilation/lint only. No new actual API case, seven-target, VM, Linux/Windows or deployed-libpthread proof. WinARM untouched. |
| R12 | Existing shared seam; cfg(test) witness only, no alternate OS manager, root/admin/security/crypto/deliverable change. |
| R13 | One necessary Debug derive, no cleanup algorithm or error policy expansion. |
| R14 | No profiles/benchmarks/allocator/native-error actor or allocation/leak-free claim. Native error/retention, enclosing Arc deallocation and parent/kernel costs remain open. |
| R15 | ROOT GO one finite ordinary scope, no native case actually started and no retry. No children/goals/main merge. ROOT source review and remaining gates owed. |
| R16 | NEW private small source/target/cache, floor checked, no deletion/cleanup/full worktree. All source/raw/caches/frozen proofs and ROOT scopes preserved. |

After ROOT reviews this correction, the original single ordinary native proof is
still owed on a newly pinned corrected-source snapshot with the same limits.
Native error/destroy retention, legal final-owner/Box/field observations, actual
eight joins/reap/NOTE_EXIT, enclosing Arc deallocation, integration/seven-target/
VM/fault/cost obligations remain open. This checkpoint does not release the lane.
