# Frozen owned-API portability gate

This is a light follow-up to [the actual State/Tail binding](../BINDING.md),
[native mutation/restoration](../MUTATION.md) and [ordinary concurrent callers](../CONCURRENT.md).
It does not close MR-OOM-MAC-1's remaining native-error obligations or release
the sustained lane. ROOT reviews and merges; this branch does not merge to main.

## Policy, source and exact change

POLICY_READ: implementer with assigned bounded testing authority. After resume,
read current canonical `b57e2fcec7170cad4c2366f7462e3fa5ff3e3fbe` AGENTS.md,
FULL development-rules R01–R16, development-evidence, CLAUDE, managed-runtime-plan,
the required combined0ae record and `/Users/adalundhe/g121-handoff.md`.
Rules SHA256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`;
handoff SHA256 `d876ce27dfa008e5385d07e77f3a97876eee80ce9d731a04af5840f8e5072202`.

Frozen source is `0aad84439f2b6231a0028713f40bf74d4bf7646c`, whose production
binding bytes are `cd0649bfedf2537bb4f9878b6e82c2c6b6e5faf0`. Originals and
their existing proof refs remain unchanged. This checkpoint adds exactly:

- One `#[cfg(target_vendor = "apple")]` on `Failure`'s existing native-only
  constructor impl in `crates/api/src/sync.rs`. The actual name is `native`,
  not `from_pthread`. Non-Apple adapters do not call it. No lint is allowed or
  suppressed; the strict command still uses `-D warnings`.
- Three lines in the existing post-reap attach **test** in `client.rs`, accepting
  only `ProcessFailure::Os { operation: OpenStat, code: ENOENT }` alongside
  the original exact alternatives, with the original PID/start guards intact.
  Linux's held-process lookup opens the pidfd before reading `/proc/PID/stat`;
  the actual reaped path disappearance is a typed refusal, not a sync failure.
- A complete private source export, short newly created TMPDIR, independent
  lib/sim invocations, and durable evidence. No borrowed test, production
  process lookup, native sync algorithm, lifecycle state or waiter changed.

| File | Final SHA256 |
| --- | --- |
| `crates/api/src/sync.rs` | `303be4efd0ce83b54fbe7ae36cf7f7202d0f791bc7126e8aac533ff202026e4c` |
| `crates/api/src/client.rs` | `3941532a6ab8c450ba532001b19dca657a6327391c04c3666ee69b3635ad9a32` |
| `crates/api/src/lib.rs` (unchanged) | `b8d330ac62c6eda930bb581d2a925285d8d33ad3bdd60d83cfd0dfa3fb187e01` |

The client's complete 43,941-byte prefix before its test module is byte-identical
to frozen0aad, SHA256 `6bc35389260520ce3d28c4e3433beee705a4c8b43d2a6d3a123544525b82246e`.
The previously recorded complete `Attempt`/handshake/borrowed span remains
`20ccf7c14cfe86a82a5ac6f1e2dfcb29118055592c93b21e61ef5ed3d800f7e1`.
Owning zero/MAX and borrowed Ready/Until/Unbounded semantics are unchanged.

`prepare.py` asserts all four-crate sources, manifests and imported Rust leaves
against frozen Git bytes before permitting only the exact cfg/test deltas.
The private root workspace/lock is reduced to that same dependency closure;
no dependency version, checksum or cryptographic implementation changes.
The legacy export metadata field `private_test_edits` also labels the explicitly
recorded sync cfg overlay; it does not imply that overlay is test-only.
Darwin additionally registers the exact existing private concurrent fixture;
the cold allocation-refusal overlay is absent. Linux retains the existing
ordinary reader tests' local allocator checks; no new fault campaign or fault
environment was introduced, and the coordinator-only OOM case stays ignored.

## Actual before results, retained without relabelling

| Run | Actual result |
| --- | --- |
| r152, frozen export | GNU/musl lib-test compile101: omitted existing `managed-proc-main/repro.rs`, included by `taken.rs:96`. Strict GNU Clippy101: unused `Failure::native`, `sync.rs:35`. Tests never ran: no pass/ignore counts. |
| r158, export includes repro | Compile101: next omitted existing `reader-tests.rs`, included by `taken.rs:100`; same strict lint red. Tests never ran: no pass/ignore counts. |
| r202, complete imports + cfg | GNU compile/list0, strict API/afunix lint0, musl linkcompile0. GNU lib actually ran:43passed/9failed/6ignored. Cargo stopped at lib failure; sim runtime UNRUN. |
| r203, cfg on Darwin | Compile0; original5 ordinary +2 concurrent0; strictClippy0. This predates the final test-only assertion change. |

The export omissions were harness mistakes, not repository missing-file defects.
Raw JSON compiler diagnostics and separate stderr are retained. No frozen source
or existing error output was overwritten to correct the export.

Eight r202 failures arose before the intended behavior at `UnixListener::bind`:
`InvalidInput: path must be shorter than SUN_LEN`. The private long TMPDIR
combined with the unchanged `TempDir` naming exceeded the socket path envelope.
No production socket limit or borrowed fixture changed. The ninth failed at
`client.rs:1927`, returning exactly:

```text
Some(Process { pid: 3021938, expected_started: 909006,
  failure: Os { operation: OpenStat, code: 2 } })
```

The final assertion adds only that actual operation/errno combination and keeps
the affected identity exact. No catch-all failure matching was added.

## Final affected gates

Linux r250: private
`/home/adalundhe/managed-cold-sync-linux-0aad-v1-iqt96o7k/candidate-v2` on
`8fadc1ec213332e16cb0d915a3fb913f`, Fedora44 GNU x86_64, kernel
`6.19.10-300.fc44.x86_64`, glibc2.43. Rust1.98.1
`48a229ceaefd4985c50990b14116b6d856af0985`, LLVM22.1.8.
Kiln admission2CPU/2GiB/4GiB, cargo-j2, LLVMthreads1/codegen1; actual driver
CPU affinity `[0,1]`. Admission is not an enforced RSS or aggregate disk quota.
One driver deadline570s, caller timeout580s, Kiln outer600s. All six commands
completed without timeout. GNU runtime is serial because existing reader tests
compare process-wide FD counts; build parallelism remains j2.

| Gate | Actual result |
| --- | --- |
| GNU lib+sim test artifact compile / list | both0 |
| Complete ordinary GNU library | **52passed,0failed,6ignored**,0filtered, noSKIP |
| Complete ordinary GNU sim | **15passed,0failed,2ignored**,0filtered, noSKIP |
| Strict scoped API+afunix `--all-targets` Clippy | 0, `-D warnings` unchanged |
| musl x86 lib+sim `test --no-run` | 0, actual linked artifacts; execution UNRUN |

Library ignored cases are the coordinator-owned allocation check, dial-cost
measurement, two attach child roles and two held-process reader child roles.
The existing parent tests actually invoke their helper roles, but the top-level
ignored cases are still counted ignored. Sim's two measurements remain ignored.
Passing this corpus does not pass those omitted measurement/fault invocations.

The harness's newly owned `/tmp/cs-aff9rcyf` was created0700. After the corpus,
only its empty `shards-tests` parent remains; it is preserved. Driver FDs were
4→4 for each of compile, list, lib, sim, lint and musl compile. Actual existing
reader checks record held-find refusal17→17, public attach refusal17→17,
wrong-start19→19 and exited18→18 with final17, actual child exit0. These are
the tested snapshots, not whole-host resource or allocation-leak proofs.

Darwin r251: private `/Users/adalundhe/managed-cold-sync-portability-darwin-v2-7nfs68wa`
on d132 (`d1323898b255a3ed435eb83fa0f7b0cf`), macOS26.4/25E246 arm64,
XNU25.4.0, same pinned Rust/LLVM. Admission1CPU/1GiB/2GiB, cargo-j1;
compile180s/case15s and whole outer600s. Actual compile0; affected exact attach
case +original5 ordinary +both concurrent cases0; strict API lib/tests Clippy0.
Concurrent records contain all-thread-joined and native `ECHILD` witnesses,
held test/stand-in exits, and absent owned vsock directories. The existing
driver's final group-signal attempt returns `EPERM(1)` in these ordinary runs;
this remains raw and is not proof of whole-population teardown. There is no
new cold-refusal/fault/VM/profile run in this portability batch.

## Repeatable commands and artifacts

Export to a caller-chosen **new** directory:

```sh
python3 docs/research/measurements/managed-cold-sync/current/linux/prepare.py \
  --candidate --test-portability --out NEW_LOCAL_EXPORT
```

For Darwin add `--darwin-ordinary`. The source identities are checked before
transfer and reconstruction; [launch-linux-v2.json](launch-linux-v2.json) and
[launch-darwin-v2.json](launch-darwin-v2.json) identify exact bundles/helpers,
runner, private paths and jobs. The complete actual command/environment records
are archived, including the initial failing exports and their source inputs.

Within Linux's explicit new private source/target/home, each cargo command uses
`cargo +1.98.1`, `--locked -j2 --profile test-release`, GNU target and
`-p shards-api`; the commands are compile `--lib --test sim --no-run`, list,
separate `--lib` and `--test sim` with `--test-threads=1 --nocapture`,
Clippy additionally `-p shards-afunix --all-targets -- -D warnings`, then musl
`--lib --test sim --no-run`. `run.py` retains every argv, cwd, result and raw
stream under one original deadline. `darwin_run.py` uses the frozen ordinary
and concurrent drivers with bounded exact single-case selection.

Final GNU lib binary SHA256
`d9daa9c4f5b56ccd89aae6e532ce154a796b611e21a1b901f15155e85336e435`;
GNU sim `653f3713441615eb237a8e1d26b23deff96fd92832f78c3426bea22033f1ffbd`.
Musl lib `e9898663d0e1f537a81cf80c2ce30814f07925a39c8af2cb84c498e06c7cf244`;
sim `766043d7ca1d78bd34b17c8a468f659e1fb4d1017c92a59272967c421200ed2e`.
Copies remain outside target in `candidate-v2/execution/frozen`.
Darwin binary `79f8bbac14a13892854d88aa4498286546577964708fec021c4cdb10ba5bb7aa`
remains at `checks/frozen-api-test`. Earlier binaries/source/targets remain intact.

[archives.json](archives.json) identifies the byte-preserving archives:
Linux113 payloads+manifest,668172bytes, SHA256
`1cb22624dc1afd0df1c96464f575534dfa165c4bc5ed88896dd8532f301c65e0`;
Darwin108 payloads+manifest,351919bytes, SHA256
`3b3eb843935af4213ec13a9535430cfe1ed012f3c84c2954e41c44e74e36b180`.
`collect.py` reads only the named finished private scopes. It leaves source,
targets/registries, binaries and runtime directories intact on the runners;
complete exported source bytes are in each original archived input.json.
`python3 .../current/linux/verify.py` passed: every member safe, unique, regular,
exact original length/hash; both manifest hashes and all exported source hashes
verified. No archive extraction or byte normalization is required.

Local `rustfmt --edition 2024 --check` on changed Rust files, Python AST checks,
`git diff --check` and `python3 scripts/check-development-policy` pass. No local
Cargo build ran. Local free196875902976bytes at final readback, above30GiB.
Three unrelated held `managed-poller-research/step9-*` files remain unstaged.

## R01–R16 and owed work

| Rule | This checkpoint and remaining limit |
| --- | --- |
| R01 | Exact native-only cfg, no production algorithm change; strict lint and affected native ordinary checks pass. Existing cold red/green is in BINDING, not rerun or relabelled here. Native init/lock/wait/notify/unlock/destroy error paths remain unverified. |
| R02 | Existing pinned alternatives and native necessity are in PROPOSAL/BINDING. This cfg follows actual target consumers and reproduced strict diagnostic; the test change follows exact actual typed error. No architectural selection. |
| R03 | No production numeric constant added. Resource/time bounds are explicit caller test inputs, not runtime sizing recommendations. |
| R04 | Exact refusal cause and PID/start remain required. State/Tail authority, terminal/stop and borrowed contract unchanged. Exceptional stale-State/pendingCancel obligations remain OPEN. |
| R05 | No new mutex framework, manager, state ledger, cache, warming, polling or spin fallback. Existing synchronization seam and wait owners retained. |
| R06 | Product/test diff is one cfg plus three exact test lines; no wider rewrite. Export/import fixes stay in the harness. |
| R07 | Source safety comments preserved; driver comments explain short socket path and serial FD tests. |
| R08 | No benchmark or competitive performance claim; comparison remains ROOT-owned and owed. |
| R09 | Actual frozen strict/compile failures and r202 runtime failures retained before minimal changes. Packaging failures distinguished from production behavior. |
| R10 | Exact inputs, raw streams, commands, source/artifact hashes and before/after outcomes archived and verified. Frozen guard/cfg removal is the retained failing source; no new campaign. |
| R11 | GNU x86 ordinary67passes, musl x86 linkcompile, native Mac arm64 affected8cases. Eight GNU top-level ignores explicit; musl execution, Windows, Linux ARM, seven-target integration and all VM coverage remain unrun here. |
| R12 | One existing shared synchronization interface; smallest Apple-only consumer cfg. No new platform duplication or installed binary. |
| R13 | Minimal target/test/harness corrections; no source change justified only by an unobserved lifecycle concern. |
| R14 | No profiling/benchmarking, allocation-free/leak-free claim or enforced RSS claim. Native destroy refusal can retain allocations; no repair or retry owner is proven. |
| R15 | Scoped implementer/light queue only, separate owned runners/targets, no children/goals/held campaigns/main merge. ROOT owes combined review/matrix and following task assignment. |
| R16 | No cache/target/source deletion, reset, stash or clean. Original proof refs, source, binaries, raw errors, paused WinARM and protected ROOT caches preserved. |

The native-error publication/wakeup/identity races, native destroy-retention and
pendingCancel/stale-State theories remain exactly as recorded in BINDING/MUTATION.
Ordinary success does not close them or authorize a speculative lifecycle ledger.
ROOT's final combined source review, seven-target/native gates and actual
error-path evidence remain owed. Full management P0–P13/G01–G25 stays OPEN.
