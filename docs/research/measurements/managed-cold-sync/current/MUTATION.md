# Current owned-API adapter removal and exact restoration

Production source remains signed
`cd0649bfedf2537bb4f9878b6e82c2c6b6e5faf0`, including the SAFETY-only lint fix.
The API source files were not edited during ROOT review or this finite run.
[BINDING.md](BINDING.md) retains full policy/readback, architectural alternatives,
current-main red, final source/Clippy proof and R01–R16 account. This record closes
its specifically owed bound-API adapter-removal/restoration check; native error,
pending cancellation, matrix and cost obligations remain open.

POLICY_READ implementer / assigned-adversarial, current main
`33df95b08ad8f95bddeaa1c48acf0c64386e0eac`, rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Only owned `current/` fixture/evidence paths changed after cd0649. No borrowed
path, shared lifecycle ledger, root cache, security setting, paused VM, child
agent, goal/planning wrapper or main merge was involved.

## Exact source and method

`mutation_prepare.py` exports the exact cd0649 four-crate API closure. In the
private mutant it changes only two implementation cfg selections in sync.rs:
Darwin's native implementation is disabled and the existing std adapter is
selected. It checks unique anchors and exact byte reversibility. The restored
version is exported again from the original Git revision, not reconstructed
from an assumed patch reversal. No allocation-size whitelist is introduced.

| Identity | SHA-256 |
| --- | --- |
| Original/restored production sync.rs | `76a7dcda67e4e04693530c2a6d52c23af4249db5d2520bc3963960782ca18903` |
| Private mutant sync.rs | `dbc7ec090fd7b2ebb15179f0804014d7fc3131c9278d93a2abf790143c40b29d` |
| Production client.rs, unchanged | `9646a777cfc6602ef8ac34051cfb3b78e05bcd12e3878117b80b6397632f5d9f` |
| Client with private test module registration, both versions | `50e11cbf8172925edf35d227f61ea0103bdc2559f20f9ee8952fdb4527354b25` |
| Production lib.rs, unchanged | `b8d330ac62c6eda930bb581d2a925285d8d33ad3bdd60d83cfd0dfa3fb187e01` |
| Executed driver | `dfe5789756f9d6c2c4571be9147c42e1e11d614b473003866a504b1ba88a1a55` |

Kiln r97 used new private ARM64Mac d132 source/targets:
`/Users/adalundhe/managed-cold-sync-binding-mutation-v1-um1lt394`.
Admission1CPU/1GiB/2GiB; sync:false; compile180s/case15s/outer600s; -j1 and
codegen1/LLVMthreads1. Actual free disk exceeded32GiB before the run. This is
admission, not an enforced Darwin aggregate memory/disk quota. Commands and
environment are retained in the archive, including:

```text
python3 mutation_prepare.py --out NEW_SMALL_TRANSFER_DIRECTORY
python3 binding_run.py --bundle INPUT.json --root NEW_PRIVATE_SOURCE_ROOT --case-seconds 15 --compile-seconds 180
cargo +1.98.1 test --locked -j1 --profile test-release -p shards-api --lib --no-run --message-format=json
```

## Actual results

| Version | Compile/selection | Native result |
| --- | --- | --- |
| std adapter mutant | Compile0, exact selector1 | SIGABRT(-6), first refused64B and fatal64B allocation report, no timeout. This is the expected violation, not a native passing test. |
| Exact cd0649 restored | Compile0, each selector exactly1 | Two cold cases passed0 with typed Allocation, stop/wait Some(Stopped), direct parent waitpid=-1/ECHILD, outer stand-in/test exit observers and absent socket directories. Five ordinary stop/drop-reap/identity/owning0/MAX cases passed0. No timeout. |

The collector separately compares actual results with the declared expected
violation/correction and returns0 only after those checks. Original native exit
codes, stdout/stderr and observers remain retained. Independent post-run checks
rehashed actual source and both binaries and verified the two restored native
reap markers and each private socket directory's absence.

Mutant binary SHA-256:
`16e8a90f9996e6e98289269db6edeb2ed9fe4a6f1e0d05815dad7667276329b0`.
Fresh restored binary SHA-256:
`b8b54856e8211bfaf13347d6cb78382d51d9491fac9b7fe91b29ac52a729f303`.
Strict Clippy0 belongs to r54's final byte-identical production source; no new
Clippy execution or seven-target gate is claimed for r97.

The mutant's held test group was signalled and its marked private paths removed;
outer observers saw test and stand-in exit. The outer is not the aborted test's
child's parent, so that stand-in's final reaping after abort remains unknown.
Restored completed-group kill attempts returned EPERM, retained as observed OS
errors without a signal/security workaround. The collector waited/reaped its
held test processes. There was no independent census of ordinary-test descendants.

[Archive identity](binding-evidence/mutation-archive.json),
[exact inputs](binding-evidence/mutation-input-identities.json),
[independent final identities](binding-evidence/mutation-final-identities.json)
and [byte-preserving archive](binding-evidence/adapter-mutation.tgz.b64) retain
58 original members plus their manifest. Archive SHA-256:
`156c2ae0d7c09a101e86388a9a335f0fb70516b0d4e3a01923e2317e1ea677b7`.
Archive SHA, every safe regular member path and all58 original size/hash entries
were independently verified locally. Native binaries, source and raw originals
remain preserved remotely; extracted local copies remain under
`/Users/adalundhe/managed-cold-sync-binding-raw-retained/adapter-mutation`.

## Remaining pendingCancel / stale-State / native-error obligations

These are exact source obligations for ROOT decision and targeted reproduction,
not newly reproduced bugs or a basis for speculative production rewrites:

- `Exit::watch(self, shared: &Shared, writer: PipeWriter)` at cd client.rs609
  attempts Child.wait on State-lock refusal, then returns without publishing
  State.exited or notifying changed. `Mutex::checked()` at sync.rs203 refuses
  subsequent explicit acquisitions, but does not establish progress or latch
  observation for an already-held guard or an existing Condvar waiter.
- `Guest::wait_for(&self, Duration)` at client.rs1313 may already be inside its
  original unbounded CV wait. Native failure/reaper/notify interleavings need
  actual proof that this caller wakes or returns typed uncertainty. Ordinary
  terminal notifications and the cold allocation case do not prove that path.
- `Guest::connect` at client.rs1209 checks State, handshakes outside its guard,
  then records ready; `Guest::stop` at client.rs1267 records stopping and signals
  under State. The existing pending stop/cancel versus successful or failing
  handshake ordering was not exercised by these serial cases. No pendingCancel
  field, parallel phase ledger or changed borrowed semantics was introduced.
- In-flight native failure versus identity/signal/reap needs a real bounded
  interleaving before claiming stale-State/PID safety for exceptional paths.
  Subsequent-lock refusal alone is not proof for existing in-flight callers.
- Native attribute/init/lock/wait/notify/unlock/destroy refusal and constructor
  prefix cleanup were not injected here. Destroy refusal retains native storage;
  no such actual refusal or recovery was observed. Unpublished cleanup failure
  still lacks an exposed retry owner; the original constructor draft is preserved.
  Do not classify retained storage as reclaimed or call the implementation leak-free.

ROOT received these exact function/source limits before adoption, with a proposed
next named finite State/CV-native-error and pending-stop fixture scope. That new
campaign and any further product correction await ROOT assignment/reproduction.
Whole seven-target/current-host gates, contention/fairness, first-parking/C-runtime
and kernel allocations, parent costs, full ownership cleanup and profiles remain
ROOT-owned. The separate OTel/collector result supplies none of these proofs.

All R01–R16 rows in BINDING remain applicable: this adds actual R09/R10 removal
and restored evidence and R15 finite owned execution, without waiving any R01
native-error, R02/R05 ownership, R08/R14 cost or R11/R12 platform gap. Policy and
format checks pass; this evidence checkpoint does not release the sustained lane.
