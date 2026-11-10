# Real Held::find allocation-refusal reproduction

**RED: the real outer caller aborts rather than returning Err.** No production
propagation design or formatting fallback is implemented in this checkpoint.

## Source, scope and method

POLICY_READ implementer: full canonical g121-main AGENTS, R01–R16, evidence,
CLAUDE, managed plan and current handoff, main
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Root assigned this one targeted native unit reproduction; root owns all audits,
profiling, benchmarking, heavy campaigns, mutations, matrix and main merge.
No child agents or production changes are made.

New branch `p/managed-proc-main-outer-oom` starts at
`1cbd4c4d853957276f702a80e71b9730bb350e94`. Exact tested signed source is
`6dbb908028e744691c4dadbca38a138de9143ac7`, pushed promptly after the actual result.
Only the existing [reader test helper](../../reader-tests.rs) and dedicated
[launcher](../../run-outer-oom.sh) change; existing includes suffice. Production
`taken.rs` and `lib.rs` are byte-identical to the starting checkpoint. The earlier
source, binaries, target and evidence remain untouched.

The one unignored test is
`taken::linux::tests::held_find_returns_error_when_start_allocation_is_refused`.
It gets its own executable's TGID and successfully samples the real started(pid)
before injection. The existing allocator helper is named `proc_allocation::after`
(the task's fail_after mechanism); after(0) refuses the next allocation on that
thread. Its closure contains only `crate::taken::Held::find(pid, start)`.
No test formatting, logging or assertion executes inside the refusal interval.
The expected assertion, reached only after the guard resets, requires a returned
Err. There is no external process, process naming, kill, child role, VM, lifecycle,
Mutex, stream, G22 or synchronization change.

Repeat invocation in a fresh private checkout/evidence path:

```sh
bash docs/research/measurements/managed-proc-main/run-outer-oom.sh
```

The launcher uses private target `~/managed-proc-main-outer-oom-target` and refuses
an existing `~/managed-proc-main-outer-oom-evidence/v1`. It retains exact commands,
source status/hashes and pinned toolchain, forces selected owned input recompilation,
requires the actual executable's Cargo `fresh:false`, hashes/copies that executable,
then runs only the exact unit with one test thread. It sets shell core size to0;
the shell's abort message does not establish that a core file exists. The selected
test is not hidden/ignored after its failure, and the full corpus was not run.

## Actual native result

Fedora GNU amd64 runner `8fadc1ec213332e16cb0d915a3fb913f`, Linux
`6.19.10-300.fc44.x86_64`, Rust1.98.1
`48a229ceaefd4985c50990b14116b6d856af0985`, LLVM22.1.8. Job r222 requested exactly
4CPU/1GiB/2GiB once and was accepted with23/32 idle; no lowered or repeated
admission. Build0, exact unit134/SIGABRT, strict API all-target clippy0; wrapper1.
The completed command fits the requested envelope; no measured peak claim is made.

[test.log](raw/test.log) prints before injection:

```text
own_tgid=50490 precomputed_start=10215685 successful_allocations_before_refusal=0
memory allocation of 28 bytes failed
```

There is no `held_find_returned_error` marker: the real call does not return to the
guard reset or assertion. This establishes a reachable abort in that outer call,
not a successfully reported resource failure. Source still maps started() errors
through format! into Result<Held,String>. No caller stack was collected here;
exact allocation-site stack attribution remains unrun rather than inferred from
the28B request alone. Previous inner-reader returning-OOM tests do not establish
safe propagation through this outer String interface.

Selected executable:
`~/managed-proc-main-outer-oom-target/test-release/deps/shards_api-41b64a187874479e`,
SHA256 `d9b7bc6f7c7c72f04d099268a9699a48d2908d0611f1310fad4c8855f585b4fe`,
`fresh:false`. Immutable copy:
`~/managed-proc-main-outer-oom-evidence/v1/test.bin`.

Private checkout:
`/home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-proc-main-outer-oom-48930bd65d`.
Original [archive](v1-raw.tgz) SHA256
`66de0a19aa6b4640a96b5ebb059d592f3af8b877a4027de365a16e1e18c56c26`,3321B;
the checked [raw extraction](raw/) includes source/binary identities, Cargo JSON,
commands and actual exit files. Executable is not committed. The native source
hashes were checked against exact6db Git blobs. Formatter, shell syntax, policy
and this source delta's diff check passed; [local checks](local-checks.json).
The earlier parent SHA256SUMS remains a historical1cbd manifest; this record's
separate manifest covers the new evidence, not a rewritten old archive.

The unit process aborts, so its normal return and destructor/FD-return observations
are not reached. No cleanup, unwind, leak-free or parent/kernel-accounting claim
is inferred from process termination.

## R01–R16 delta and owed work

This supplements the [main-reader account](../../README.md), whose unrun gates
remain unrun; no earlier green is inherited for this new test/source.

| Rule | Result/limit for this reproduction |
| --- | --- |
| R01 | Actual real caller abort134 under first allocation refusal; production remains RED. No remedy, fallback or whole-library panic-free claim. |
| R02 | No broader implementation/architecture choice; actual reproduction precedes any propagation design. Root reviews alternatives/design. |
| R03 | No production constants added. Injection0 means refuse the first allocation; the request28B is an observation, not a chosen capacity. |
| R04 | Production state and Result<Held,String> unchanged. Intended Err was not returned; no new error/control type introduced. |
| R05 | Reuses actual caller and existing allocator helper; no cache, poll, wait, fixture framework or manager added. |
| R06 | One direct focused test/closure, actual strict clippy0. No broad audit performed. |
| R07 | No new unsafe code; inherited allocator forwarding safety contract unchanged. |
| R08 | No performance,4–5×, allocation-free or leak-free claim. |
| R09 | Exact real own-TGID caller live abort captured before any fix. |
| R10 | Raw failure, commands, source/toolchain/binary identities retained. No fixed/green or mutation result exists for this seam. |
| R11 | Native GNU amd64 only. Other targets/libcs/VMs unrun; paused Windows ARM untouched. |
| R12 | Existing Linux private seam and helper reused; no dependency, installed binary, hostroot, crypto or non-Linux change. |
| R13 | Test-only direct call, no production abstraction or fallback. |
| R14 | No implementer profiling/benchmarking; root owns costs and attribution. |
| R15 | One assigned targeted unit, no child agents/heavy/mutations/main merge. Signed named-path checkpoint uses required trailer. |
| R16 | New private checkout/target/evidence; no old/root cache mutation or deletion; one unchanged envelope request. No local Mac build. |

Owed to root: review the actual abort and approve a narrow propagation design
before production edits; any further allocation-site stack/control evidence;
fixed before/after, cleanup/error-kind/compatibility controls and combined/platform
gates appropriate to that reviewed change. No repeat/sweep, formatting fallback
or guessed error framework was attempted. The independent historical owned-Mac
Mutex allocation abort also remains open.
