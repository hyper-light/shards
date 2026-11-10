# Current selection: light compile, lint and native endpoints

Source `1be384ecb3e3852a6b97daaf1ead61d441f21d20`, directly based on
main `a920c85b5c0c9d40aafc4f4cce32432c7c0aed79`. All seven approved source files
remain byte-identical to [source-identities.json](../source-identities.json)
and the reviewed original37ee. This checkpoint adds evidence only. The
[original red/correction/mutation records](../README.md#original-evidence-unchanged-provenance)
keep their original source and campaign context.

Policy revision a920; rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Role: implementer plus assigned adversarial relay tester. Full current AGENTS,
R01–R16, development-evidence, CLAUDE, plan and handoff were read before the
run and again after compaction. ROOT accepted the current-selection scope.

## Actual results

| Scope | Actual outcome |
| --- | --- |
| Mechanical policy, seven selected files' rustfmt, unapplied host registration `git apply --check` | Each0. Parent host registration remains unapplied. |
| Normal `shards-init` binary caller, GNU and static-musl targets | Both `cargo check --locked` and strict Clippy `-D warnings`0, each libc. |
| Exact `managed_network` source projection, GNU and musl | Check, strict Clippy including all test targets, and native test build0, each libc. This projection is not the complete normal testguest or host build. |
| Current-source native corpus r142 | Musl10/10 pass; GNU9/10 pass and one strict pressure-witness failure. Entire r142 remains exit1. |
| One separately retained, unchanged GNU pressure case repeat | Exit0; exact bytes and half-closes, actual100 short sends and100 send-WouldBlock events. Original failure is retained. |

Each libc's ten cases are Unix client-FIN-first and host-FIN-first comparisons
against the frozen original engine body; TCP server-first and backend-FIN-first;
extra client byte; held-backend-receipt cancellation; backend-origin reset;
client-origin reset; and both TCP orders under the existing finite pressure input.
There is no new VM, mutation, production change or assertion relaxation here.

Normal TCP request13/greeting8/tail6, caller slots3/5, case30s and parent45s are
explicit fixture inputs. Pressure uses request1048576, greeting8, tail131072 and
slots65536 each; caller queue inputs4096 read back8192. Both independent native
endpoints witness exact payload bytes and terminal events. Held-receipt cancel
retains original-deadline time and sent/consumed facts; direct and mediated reset
cases retain the independent peer's actual reset. Extra-client-byte cases retain
the backend prefix and actual EOF without the extra byte.

### Strict pressure failure and unchanged repeat

In r142's final GNU backend-FIN-first case, both actual endpoints completed with
backend1048576/client131080 exact bytes and EOF. The flow ended
`BothHalvesClosed`, pending0/extra-consumed0, but the relay observed
partial-writes0/send-WouldBlock0. The unchanged test failed at
`tcp_forward.rs:1624`, `actual short send required`, with native exit101 and
driver exit1. Read-WouldBlock observations are not send-WouldBlock evidence.

The single later invocation used the same frozen binary, driver and exact input
SHA256 `5ad7c44003a9d0649328bb727dfa3a0de765f9987240686cd1b1d99955c8bdf4`.
It passed with100 actual short sends and100 send-WouldBlock events, retaining
exact bytes/EOF and empty pending tails. No loop retried until success, and no
queue, input, threshold, source or assertion was changed. The other pressure
cases in r142 observed musl server-first1/2, musl backend-FIN-first80/80 and GNU
server-first39/39 short-send/send-WouldBlock counts.

This is evidence that the current pressure induction is scheduling-sensitive.
It establishes neither an I/O corruption defect nor a deterministic pressure
gate. The original batch is not reported as all green. A repeatable pressure
hold/release design remains a separate fixture question; no guessed production
fix follows from this assertion failure.

### Retained harness failure

The earlier private r141 scope passed policy/format/registration and normal musl
init check/strict Clippy, then failed its source projection with E0583. Locating
the projection root under `check/lib.rs` made Rust seek managed-network children
under the wrong directory. No native actor ran. r142 used a new private scope
and the prior working layout: one declared untracked
`crates/testguest/src/interface_root.rs` with `mod managed_network;` adjacent to
the real source. All selected files stayed unchanged. The failed source, target,
logs and original result remain preserved; this was a harness layout failure.

## Identity, invocation and cleanup

Runner `8fadc1ec213332e16cb0d915a3fb913f`: Fedora44, Linux
6.19.10-300.fc44.x86_64, Rust1.98.1/LLVM22.1.8, glibc2.43. Complete toolchain,
OS and compiler identities are in [environment.json](environment.json).
Each run requested private2CPU/2GiB/4GiB admission. Compilation used `-j2`,
two-CPU affinity, no incremental output, private Cargo home/target and compiler/
native-process RLIMIT_AS2GiB. Outer build/corpus bound600s; repeat90s.
These are admission and process settings, not verified physical/kernel caps.

Exact executed commands are [failed-harness.sh](failed-harness.sh),
[current-selection.sh](current-selection.sh),
[gnu-pressure-repeat.sh](gnu-pressure-repeat.sh) and
[package-raw.sh](package-raw.sh). The first two receive `RELAY_RETAINED_DRIVER`,
the gzip/base64 encoding of the original `native_cases.py` at signed37ee,
SHA256 `eef969298d83c23dc6247131c9ae14078971ab732b47f0c25864f55218838fd9`.
Those exact bytes are also in the archive. The projection retains the full
current workspace lint flags and actual `clippy.toml`; it omits the full
testguest dependency/build path. Scripts deliberately refuse existing record
roots and unexpected source identities. They record the historical branch tip;
after that branch advances, a future authorized rerun needs a new private scope
and an explicit checkout of immutable1be before its identity check.

Frozen native binaries, outside target:

| Target | Bytes | SHA256 |
| --- | ---: | --- |
| GNU | 12679912 | `0ba3e64e24bc900fc02a036814c0c8990df9cafe29b8e0ca86ceee760b14dc09` |
| musl | 12978560 | `d96924b518cce5f96936383cbc2458ae2dcbe70286f220b14eec21bea4bf34d6` |

All34 r142 phase children had held pidfds, actual exit readiness and reaping;
no phase required forced cleanup. r142 callerFD9→9. Every native parentFD4→4,
held pidfd exit/reaping and unchanged binary hash was observed. Successful tests
join their owned workers; Unix/extra/reset tests also retain their explicit own
descriptor snapshots. The failed pressure test recorded both worker joins, but
its assertion preceded the final child descriptor snapshot: that snapshot is
unverified. Parent descriptor return does not replace it. The repeat records
normal test success, worker joins, native parentFD4→4 and outer callerFD9→9.
Whole-system/kernel resource return remains unknown.

After all cases, the seven source hashes still matched, tracked `git diff` was
empty, and the only untracked source file was the declared projection root.
The after-case receipt is separate from r142's skipped final success-only phase.

All private remote scopes remain intact:

- `~/relay-current-1be-a920-light-v1/{source,target,check,record,cargo-home}`
- `~/relay-current-1be-a920-light-v2/{source,target,check,record,cargo-home,artifacts,cases}`
- `~/relay-current-1be-gnu-pressure-repeat-v1`
- `~/relay-current-1be-evidence-v1`

[results.json](results.json) retains compact endpoint hashes, exact phase
commands/results, the failed case and unchanged repeat. Original stdout/stderr,
observed bytes, inputs, failed/passing results, driver, projection/config/locks
and environment are in [raw-linux.tar.gz](raw-linux.tar.gz):131513 bytes,
SHA256 `304feaf9d716717c023a24718fcf335d1516486996a3b42e3719599046e584b3`.
All195 regular safe members were verified locally;194 original files match
[raw-manifest.json](raw-manifest.json) by exact length/SHA. No newline, encoding,
payload or original result was normalized.

## R01–R16 account and open work

| Rule | Scoped evidence and limit |
| --- | --- |
| R01 | Both normal init callers and projection/tests pass the current denied panic-related lints; no production edit. This does not prove every implicit allocation/error path safe. |
| R02 | Original pinned primary research and real caller alternatives remain linked in the selection README; exact carry adds no architectural decision. |
| R03 | No new runtime number. Buffer/byte/time/queue inputs above are caller fixture values; shared production slots use the documented caller/OS-page contracts. |
| R04 | Actual finite cases retain connect/directional/half-close/terminal variants, typed cancel/reset/extra-data and consumed/sent facts. Concurrent cancel after admission remains unresolved. |
| R05 | One shared readiness/Deadline pump; no new cache, scheduler or polling interval. Strict pressure induction is explicitly nondeterministic here. |
| R06 | Seven reviewed source files unchanged; focused current caller/projection checks, not a repository audit. |
| R07 | Original safety/invariant comments preserved; no new production comments or allowances. |
| R08 | No comparative performance claim or4–5× result. ROOT owns equivalent-baseline measurement. |
| R09 | Original actual EOF/reset/extra-byte reds and mutations remain immutable; this checkpoint retains the actual pressure-witness red and makes no speculative fix. |
| R10 | Exact commands, both failures, passing cases, binary/source identities and raw bytes are durable here; old evidence keeps its original provenance. |
| R11 | Actual native GNU/musl x86_64 only. Seven-target/full callers and actual carried VSOCK are owed ROOT gates. macOS ARM64 only; paused Windows ARM remains untouched. |
| R12 | Production engine and testguest import one shared Linux guest pump/Deadline. No second pump, changed grant, controller, helper or installed deliverable. |
| R13 | Exact narrow carry; host leaf registration remains a minimal unapplied patch. |
| R14 | No audit/profile/benchmark run by this lane. Parent/kernel charges, allocation/fault cost and leak-freedom remain unverified. |
| R15 | Approved existing finite native corpus only, private admitted bounds and owned-child witnesses; signed branch checkpoint, no main merge/children/goals. ROOT reviews and validates integration. |
| R16 | No caches/targets deleted or warmed; all frozen sources/artifacts preserved. No local build/target/cache; observed Mac free195499652KiB exceeded30GiB. ROOT and paused scopes untouched. |

ROOT still owes full normal testguest/host/helper context, the selected combined
seven-target matrix and actual normal carried KVM engine VSOCK before/after
cases. Unix sockets are not VSOCK proof. Mandatory internal/IPv6/UDP/FD route
coverage, hard producer/parent/kernel budgets and management P0–P13/G25 remain
open. This is a tested checkpoint, not lane or management-layer completion.
