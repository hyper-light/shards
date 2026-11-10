# Frozen corrected owned-API native Windows light gate

At exact `f08cc06b009158a5eafce00d3bc806b702a8aff7`, Jupiter's native Windows
ordinary API library and simulator pass, as does strict scoped API/afunix lint.
This checkpoint changes only this fixture/evidence directory. It does not change
production source or close the [binding's remaining native-error obligations](../BINDING.md).
The sustained lane remains active; ROOT owns review, integration and main merge.

## Policy and owned scope

POLICY_READ: implementer with assigned bounded testing authority. Before edits
or native checks, reread current main `b57e2fcec7170cad4c2366f7462e3fa5ff3e3fbe`
AGENTS, FULL R01–R16, evidence checklist, CLAUDE, managed-runtime-plan and handoff.
Rules SHA256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`;
handoff SHA256 `fa345a68c9de9e7b6fe4b75bfdabb6bc51e1291484854d957ca02698cfb75c09`.
Own only `docs/research/measurements/managed-cold-sync/current/windows/` and
new private paths below. Frozen API/source and previous reds remain read-only.

Runner `f766f09294f2e17e3b8a631ffd4e6a4b` (Jupiter), native AMD64 Windows
NT10.0.26200, PowerShell5.1.26100.9711. Rust1.98.1
`48a229ceaefd4985c50990b14116b6d856af0985`, LLVM22.1.8,
Cargo1.98.1 `797e8a9bc`; VS2022 BuildTools installation present. No exact SDK or
linker version is inferred from that installation path. Admission2CPU/4GiB/12GiB;
cargo-j2, codegen1, incremental disabled; admission is not an enforced RSS/disk quota.
One launcher Stopwatch deadline870s and Kiln outer900s; final gate44.175s wall
time is a correctness-run receipt, not a performance comparison.

Private root:
`C:\Users\corph\managed-cold-sync-windows-f08-v1-a59029f2d752470c98a96216f08223cf`.
Original failed scope is `execution`; final scope is **`execution-v2`**, each with
its own source, cargo-home, target, raw and frozen directories. No ROOT clone or
cache was used. Final new short TMP/TEMP is
`C:\Users\corph\csw-335cf5f2553849b396799a0d0cb23866`;
only its empty `shards-tests` parent remains, preserved. Original failed scope's
temp `C:\Users\corph\csw-775c408a655d44e4a0eb71f759c7f8ed` also remains intact.
No host job, administrator, execution-policy, Defender or firewall setting changed.
No VM, paused Windows ARM, child agent, goal/plan dispatch or rejected campaign ran.

## Exact source and retained setup failures

The [f08 Linux/Darwin record](../linux/README.md) preserves the four-line cfg/test
correction and earlier native/compile reds. Here `prepare.py` reuses that complete
four-crate exporter with **f08 as the frozen Git ref**, no candidate overlay and
no new allocation hook. All33 crate/manifests/imported Rust files are asserted
byte-identical to Git. Only the private root workspace/lock closure is reduced;
dependency versions/checksums are unchanged. Native reconstruction verifies every
exported body hash before writing it with CreateNew.

| Product file (unchanged) | SHA256 actually compiled and verified |
| --- | --- |
| `crates/api/src/client.rs` | `3941532a6ab8c450ba532001b19dca657a6327391c04c3666ee69b3635ad9a32` |
| `crates/api/src/sync.rs` | `303be4efd0ce83b54fbe7ae36cf7f7202d0f791bc7126e8aac533ff202026e4c` |
| `crates/api/src/lib.rs` | `b8d330ac62c6eda930bb581d2a925285d8d33ad3bdd60d83cfd0dfa3fb187e01` |

Source bundle gzip SHA256
`f8de0641f894063bb0b0138d651fc2aa89f93bb33ffc5133c97ea25775ff704d`.
Final launcher SHA256
`92be2d3fa6b35def1dd29e58951559ba34b3a8e8120d050d863aa677aa1ee43a`;
its native PowerShell parser reports zero errors. The original launcher
`fa0b7f40e52a2d6f06c2e7df0a0daa021199fb9ef5c07447dfc03a460be05c20`
is retained separately on the runner and in raw records.

Three failures precede the green native gate and remain actual failures:

- Python3 discovery exits49: the installed command is a Microsoft Store alias,
  not an available interpreter. [Original command/output](setup-python49.json).
  Existing PowerShell is used instead; no installation or alias change.
- First long source-transfer command exits2 in Bash's quote parser before
  PowerShell/build starts. [Diagnostic](setup-transfer2.json) and
  [exact command](setup-transfer2-command.txt) retained. Its precise transport
  cause is unknown. Short CreateNew chunks succeed and reconstruct the same
  checked bytes; no pre-existing path is overwritten.
- First native launcher: compile/list/lib/sim/Clippy **all101 before compilation**.
  Windows LLVM rejects the copied harness `-Cllvm-args=-threads=1` during Cargo's
  target query. No test pass/ignore count exists for that run. Original raw
  stderr, argv, source and summary remain under `execution`. Its final ordered-map
  Select-Object presentation printed null fields; the full summary/raw were
  inspected independently, and v2 fixes only that presentation and resource setup.

V2 removes the unsupported LLVM flag, retains j2/codegen1 and narrows only its own
PowerShell process to the first two already-allowed CPU bits. Native readback is
`4294967295 → 3`. This is process-scoped scheduling, without a job or host change.
Microsoft documents that process affinity is inherited by child processes;
the pinned [primary identity](primary-identities.json) records source commit
`08ed091870a25a4ddeffe0727789ca0164beadd2`, SHA256
`ec96f59ecef57850ef28f3f033bf7e43e64ba3f4c68f270adf4dc535df81e83c`.
This contract supports the harness choice, not a trace of every descendant or
resource-cost claim. [Official API contract](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-setprocessaffinitymask),
[.NET property](https://learn.microsoft.com/en-us/dotnet/api/system.diagnostics.process.processoraffinity?view=netframework-4.8.1).

## Actual final gate r505

| Command | Actual result |
| --- | --- |
| Native x86_64-msvc API lib+sim `test --no-run` | 0, linked executable artifacts |
| Native lib+sim `--list` | 0 |
| Full ordinary API library | **29passed,0failed,0ignored,0filtered**, noSKIP |
| Full ordinary simulator | **4passed,0failed,0ignored,0filtered**, noSKIP |
| Strict API+afunix `--all-targets` Clippy | 0, unchanged `-D warnings` |

All five held commands exit0 without timeout and close both raw capture streams.
The eight actual native borrowed cases include changed identity before dial,
overflow before process/dial I/O, dribbled/withheld reply deadlines, exact bytes/
clone/half-close, typed malformed/closed replies, bounded private refcounts and
raw Ready operations. No assertion, case filter, ignore or lint was relaxed.

**Cfg absence is not a passing ignore.** Owning zero/MAX shell stand-ins,
native owned-child stop/reap cases and the public zero-patience withheld-reply
fixture are Unix-gated and did not run on Windows. Linux reader tests and the
ignored coordinator allocation case also are not Windows cases. Windows simulator
`no_clock_waits_for_the_fact` exercises MAX with simulated guests, not native owned
Child cleanup. The conformance test exercises simulator stop/terminal behavior.
No separate native pending-connect cancellation campaign ran.

The existing simulator calls GetProcessHandleCount for admission; it supplies no
before/after return-count assertion here. Raw Ready readiness, timeout and
half-close tests pass, but there is no exact native handle-return, native owned
child reaping or whole-population proof. Controller command exits/closed raw
pipes and absent test subdirectories do not establish those stronger results.
Native Darwin errors/destroy-retention/pendingCancel/stale-State remain OPEN.

## Reproduction and durable bytes

```sh
python3 docs/research/measurements/managed-cold-sync/current/windows/prepare.py \
  --out NEW_LOCAL_EXPORT
```

Transfer the checked bundle and launcher into an explicitly new private Jupiter
path. Use normal `powershell.exe -NoProfile -NonInteractive -File .../run.ps1
-Bundle .../input.json -Root NEW_EXECUTION -Seconds 870`; outer900. Existing
execution policy is respected. [launch.json](launch.json) and
[launch-v2.json](launch-v2.json) preserve actual invocations, hashes and ownership.
Each cargo command uses `+1.98.1 --locked -j2 --profile test-release --target
x86_64-pc-windows-msvc`. Compile/list name `-p shards-api --lib --test sim`;
lib and sim run separately with `--test-threads=1 --nocapture`. Strict lint names
only API+afunix and `--all-targets -- -D warnings`. No real/VM test is executed.

Final frozen binaries remain outside target under `execution-v2/frozen`:
API `shards_api-26f65113450b8bf4.exe`, SHA256
`3c63932eadbdcd42e27843fd714f1408d3ed50ad967ab45a55945193fa535283`;
sim `sim-a4574a043c4fab25.exe`, SHA256
`96667c4bad280db216fd605905433aaf5728b98fb70e0a98ab94926c8a31dabb`.
Targets, PDBs, original source, both temp parents and all earlier artifacts remain.

[archive.json](archive.json) identifies55 original payloads in the checked gzip
container,736954bytes, SHA256
`5d4f122ea2c7547bcd76e0603b859da1ddc19b878de34cf240c0d99eacf7ae80`.
`collect.ps1` reads only explicit completed-scope root/raw receipts; it does not
walk target/cache or delete anything. The original input.json retains complete
source bytes. `verify.py` passed: all55 payload lengths/hashes and safe unique
relative names, all33 frozen Git source comparisons, final launcher hash and
all actual raw stream hashes. No newline normalization or extraction is needed.

## R01–R16 account and remaining gates

| Rule | Evidence and limit |
| --- | --- |
| R01 | No production edit. Strict native lint and ordinary cases pass. Original cold symptom/guard proof remains in BINDING/MUTATION; native error paths and retained allocations on destroy refusal are not resolved by this gate. |
| R02 | Existing researched shared synchronization seam unchanged. Exact target-query red justifies harness-only flag removal; pinned primary affinity contract and actual own-mask readback retained. No architecture selection. |
| R03 | No production constant. j2/870/900 and admission are explicit caller test bounds; selected mask is derived from two allowed CPUs. Printed socket buffers are observations, not copied runtime limits. |
| R04 | Typed refusal/identity/deadline assertions unchanged. Native borrowed and simulator transitions exercise their own scopes, not native Child terminal/error guarantees. |
| R05 | No new sync abstraction, phase manager, cache, warming, polling or spin. Existing APIs/callers only; launcher ownership stays separate from production state. |
| R06 | One finite five-command native harness, separate raw streams and one original deadline. No broader source rewrite. |
| R07 | Existing safety comments preserved. Harness comments identify ownership and raw-capture/timeout limits. |
| R08 | No benchmark or competitive claim. ROOT retains performance comparison obligations. |
| R09 | All setup/target-query reds retained before harness corrections. No speculative production correction based on Windows source suspicion. |
| R10 | Exact source, commands, failed/successful streams, native parser and binary identities retained and byte-verified. Existing cold mutation/restoration remains revision-specific prior evidence. |
| R11 | Native Windows amd64 lib29/sim4/strict lint0 at exactf08. Cfg exclusions explicit; WinARM, VMs and new seven-target integration unrun here. Previous Linux/Mac gates retain their own revision identities. |
| R12 | Exact shared product bytes, no OS-specific production duplication, extra deliverable or host/security change. |
| R13 | Harness-only portability corrections; no new production feature or diagnostic-only product workaround. |
| R14 | No profiling, allocation/leak-free or whole-operation kernel/handle-return claim. Admission is not a measured/enforced RSS/disk peak. |
| R15 | Scoped implementer/light work, signed named paths and required attribution, no children/goals/main merge/held campaigns. ROOT review and combined integration remain owed. |
| R16 | No cleanup/reset/stash/clean, target/cache deletion or source replacement. Existing proof refs, roots, failed runs, frozen executables and paused WinARM preserved. |

Local exporter/verifier AST, complete archive/source verification, development-policy
and whitespace checks are recorded in `local-checks.json`. No local Cargo build
ran. Native init/lock/wait/notify/unlock/destroy failures, retained native storage,
pendingCancel/stale-State and full P0–P13/G01–G25 remain open. This checkpoint
requests ROOT's next exact scoped queue, not lane completion.
