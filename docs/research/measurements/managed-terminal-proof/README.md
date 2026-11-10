# Canonical managed-output completion: coordinator closing record

POLICY_READ coordinator; main base eb5654d6f5356cacaca536ff51f14a632fa7e801,
rules SHA256 d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
The selected implementation and fixtures are in [COMPOSITION.md](COMPOSITION.md).
This closes the reproduced false terminal classification in this batch. The
managed-runtime plan, concurrency/fault coverage and cost obligations remain open.

## Problem and change

After receiving and acknowledging canonical End, an owner stop could publish
`aborted/unknown` while accepted and consumed covered the actual End wire tail.
Pipe EOF also cannot establish actor exit: the controlled actor remains callable
after independently closing fd1/fd2.

The correction preserves the canonical End/Aborted(errno) type in Life, obtains
the wire/credit view from authoritative Shared, and lets the existing credit
worker complete a draining stream even with no further credit. A known normal
Finished outcome wins over a later stop, including before worker publication.
Equal offsets alone do not imply completion. Canonical errors remain aborted,
with their errno and unknown tail. Stop always retires its owned endpoint;
logical terminal state does not release transport ownership.

The callback only wakes the existing credit worker. Shared releases its lock
before invoking it. Reviewed held edges are credit→Shared and Life→Shared;
no held Shared→credit or credit→Life edge is introduced. Metadata I/O occurs
after lifecycle/credit guards are released. Logical completion and successful
metadata append remain different facts. The [implementer record](../managed-terminal-publication/README.md)
retains primary contracts, alternatives, transition tables, code/light checks
and candidate-edge red/correction. This is not a new manager, cache, polling
algorithm, queue capacity or authority grant.

## Actual root before, after and guard removal

Fedora runner 8fadc1ec213332e16cb0d915a3fb913f, native Linux amd64 KVM;
source-matching normal carried helpers, no helper-skip/substitute build.
The pinned experimental kernel is SHA256
97819d00fecb5de0cffc4bf292af9c78c5d662336e07226ae58731fd580537ae,
used as both SHARDS_KERNEL and SHARDS_TEST_KERNEL. This does not change the
product kernel pin. Exact compiler, commands, caller inputs and artifact hashes
are retained under each record.

| Gate | Exact source | Actual outcome |
| --- | --- | --- |
| Before r692 normal | 26bab0778ddb8f35b11e4b5644ee83b1b1d0df99 | One test failed101 at strict final `aborted/unknown` versus required `ended/complete`. |
| Before r692 stdio | Same26ba | One test failed101 at the same strict assertion after actual End/ACK/EOF and fresh post-EOF actor challenge. |
| After r696 normal and stdio | b02e8592c547553c788702ac6c52ea6b633e875d | Each one test passed0; strict assertions unchanged. |
| After ordinary managed_output | Sameb02e | 16 passed,0 failed,4 ignored. |
| Actual daemon managed units | Sameb02e | 15 passed,0 failed,0 ignored. Full production module caller. |
| Actual remote managed units | Sameb02e | 15 passed,0 failed,0 ignored. |
| Guard r702 | Sameb02e plus [one-arm mutation](native/guard/mutation.patch) | Stdio one test failed101; independently retained actual false-abort metadata despite canonical End wire tails. |
| Restored r702 normal and stdio | Exactb02e bytes restored | Each one test passed0; source hash restored and tracked diff empty. |

[Before results/logs](native/before/), [after results/logs](native/after/),
[mutation/restored records](native/guard/) retain actual effects.
The mutation disables only
`Draining(End) + Finished → Ended`, with an always-false guard. Its caller checks
the observed End bytes and both actual metadata channels, not merely a failed
assertion or lost alert. Both channels falsely reported aborted with
accepted==consumed==their canonical End wire tails102 and99, before cleanup and
after stop. The restored production daemon SHA256 is
2b3e4d2c80be615a58b062b6a077c11511e2679c738c798a27f0e014a880f655;
remote SHA256 is
79c949b781e71821079a0858c67270355ed24a231789aafad80c9695f6226fdd.

Normal and stdio fixtures observed owner stop/observer join, private run removal
and observer FD baseline7→7. In the stdio case, raw fd1/fd2 payloads and End
records arrive unchanged; after both EOFs a fresh UUID16-byte challenge receives
its exact21-byte framed response and peer EOF. The actor is still visible,
sibling emits new exact fd1/fd2 data, and main makes independent progress.
The final normal actor channels report ended/complete with wire offsets102/99;
the still-live sibling's owner-stopped channels report aborted/unknown177/171.
No arbitrary descendant or semantic-tool attribution follows from inherited
channel bytes or the visible actor row.

These are bounded functional examples, not exhaustive races, full population
reaping, kernel-charge return, distributions or performance comparisons. Caller
300000ms work/cleanup limits and900s outer watchdogs are explicit experiment
inputs, never production defaults, learned values or overhead allowances.

## Matrix and provenance

Actual r697 on macOS arm64 runner b24c5c951260dcdfd0972f2aeaf3cf51 ran full
`scripts/lint` at exactb02e: format, all seven supported targets and GPU checks0.
It separately ran actual native remote managed15/15,0 failed/ignored. [Raw records](matrix/matrix/)
pin Rust/Cargo1.99.1 and commands. AWS-LC's existing compiler-feature probe
warnings remain unedited in the logs; a successful matrix is not a claim that
every optional assembly probe succeeded or that verification was weakened.
No native Windows b02e run or real HVF/WHP VM campaign was performed here.
The Windows ARM VM remains paused and its full-suite criterion unverified.

The first after-v1 attempt stopped before build/test when checkout128 followed
a fetch started before the candidate push finished. Its [remaining source/driver](native/after-v1-failed/)
are preserved, not counted as a test. The transient tool-output traceback is not
in this archive. The push was awaited and v2 ran successfully. Initial after
artifact freeze retained three copies but found the original per-process
testguest path already removed by the later ordinary test's cleanup.
[Freeze provenance](native/after/freeze-attempt-1.json) and
[completed manifest](native/after/frozen-artifacts.json) are explicit: testguest's
exact expected bytes were recovered from the frozen before artifact; this proves
content equality, not original inode identity.

Before/after frozen fixture, CLI, init and testguest copies remain on Fedora.
After fixture SHA256 is
a29eabc04334e8bed28a1c3bb29ed690b241fab469208449acb50d8abed708d9.
The restored guard artifact has that identical hash and was separately frozen
at ~/root-managed-terminal-restored-b02e-artifacts-v1/fixture.
The mutant executable's identity is recorded but no separate frozen mutant
executable is claimed. No executable or credentials are committed here.

[ARCHIVES.json](ARCHIVES.json) records private originals, transfer hashes and
all-member manifests. Root verified archive size/hash, safe regular member names,
every member size/hash and absence of ELF executables before extraction.
Linux archive116166B/294 files; matrix5800B/14 files. Source-side originals and
local originals under /Users/adalundhe/g121-retained-archives remain preserved.
The exact submitted [drivers/wrappers](coordinator-drivers/) were copied from
retained command text after execution; wrappers are not falsely claimed run as
remote files. Read per-case command/environment files for actual invocations.
Raw logs remain byte-identical; any trailing whitespace is original evidence,
not a source-wide whitespace exception.

## R01–R16 and remaining work

- R01/R04/R06/R07/R13: reviewed explicit canonical/lifecycle/credit states and
  checked transitions, guard release, error/stop ownership and focused code.
  Full matrix/actual caller tests pass; this does not certify all historical
  runtime allocations or cold synchronization as panic-free.
- R02/R03/R05/R09/R10/R12: linked primary-source alternatives and live
  before/after/guard/restoration establish this correction at existing Shared,
  worker, Condvar and transport seams. No new production literal/cache/poll,
  privilege/crypto choice, OS-duplicate implementation or installed binary.
- R08/R14: learning samples now include worker dispatch delay. Its scheduling,
  allocation/page-fault and comparative costs are unmeasured. No4–5×,
  allocation-free or leak-free claim.
- R11: exact seven-target compile/lint, actual Linux KVM cases and native
  macOS component results are recorded. Other real-VM combinations and native
  Windowsb02e remain unverified; Intel Mac is unsupported.
- R15/R16: scoped agents provided code/light checks; root reviewed/ran all
  combined heavy cases/mutation/gates and owns main landing. Protected caches,
  unmerged sources, raw/frozen evidence and paused VM remain preserved.
  Local free space was34617217024B after extraction, above30GiB.

Metadata lock/write delay, partial/failed publication, completion/stop ordering,
endpoint retirement under those faults, withheld ACK/revocation races and wider
owner/worker-loss cases remain separate root gates. A native scoped fixture lane
is implementing these tests; no speculative production ordering fix is included.
Reconnecting observers, inherited-writer lifetimes, all-route forwarding,
hard aggregate/parent/kernel budgets and G12 entitlements, effect brokerage,
real harness adapters/credentials and generation-safe restore/move remain open.

