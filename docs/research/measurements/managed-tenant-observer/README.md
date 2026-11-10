# Reconnect fixture component checkpoint

This checkpoint is the pure retained-record component, not a completed live
reconnect fixture. No observer/transport/helper/socket/namespace/BPF/VM was executed.
ROOT owns all actual E2E/mutations, audits/profiling/benchmarks/chaos/stress,
seven-target combined gates and main merges. The lane goal remains open.

POLICY_READ IMPLEMENTER canonical main24f, then ROOT-reported096c9c7b with policy
files unchanged fromf5cec365; rules SHA256
d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
Full canonical policy/evidence/CLAUDE/managed-plan/handoff read before work;
role/constraints and updated ownership sent to coordinator01a148884a4770fdaf0111f3f792a248.
Implementer01a1c5d28cea74a5b0241200190d6c54; no children or self-main merge.

Reused the same clean worktree on p/managed-tenant-observer from signed crash
87503b7d14d7a34821ce3f7f5dc79bba5e2a33bf. Crash and normal checkpoints/branches
remain preserved. Owned changes are new tenant_reconnect.rs, its declaration in
the owned tenant_exit family, and this evidence/proposed patch. Host parent is
UNCHANGED at this checkpoint. No header/allocator/cost/production change.

RetainedRecord holds one actually observed canonical Data record, its nonzero
transport wire start and pre-record canonical payload state. hold rejects wrong
member/channel, non-Data, invalid canonical tail and overflowing wire end. replay
requires the exact retained start and exact raw canonical bytes, then derives
post-replay state from the saved prior state. Its one immutable witness is scoped
to a single outstanding record; this is not a product history/cache or a guarantee
of arbitrary replay. Full live body must supply actual observed offsets and must
not ACK this retained record until the reconnected observer replays it.

Three pure composition tests exercise actual ABI encoding/state with an Open,
prior prefix Data, a nonzero retained tail and subsequent new Data. They check
that wire and payload offsets advance separately, replay advances payload once,
shifted/zero/forward cursors and equal-length changed/truncated bytes fail, and
cross-member/channel, End-as-Data, payload-tail mismatch and u64 wire overflow fail.
These are targeted pure tests of fixture guards, not live mutation/effect proof.

The [minimal contract](PROPOSAL.md) and [concrete initial-cursor patch](initial-offset-seam.patch)
remain submitted to ROOT. That patch is NOT applied: the parent-host ownership
restriction permits only the earlier task parameter until ROOT expands it or
supplies the seam. No native helper mechanism is added. The real sequence also
depends on ROOT/refactor's actual-r606 finish-wake correction; dependency was
reported before edits, and fce End/tail assertions remain intact. No guessed
production fix or transport-race workaround is part of this component.

## Exact light evidence

Fedora8fadc1ec213332e16cb0d915a3fb913f, Kiln r12 admission
linux/amd64,cpus=1,mem=1g,disk=2g. NEW private
~/managed-tenant-observer-pure-01a1c5d2-v1/{source,target}; source192KiB and final
target66,216KiB. Headroom is not measured hard aggregate accounting. Only actual
ABI sources/build.rs and the exact pure component were copied. No live fixture,
guest helper or surrounding host stubs are present or executed in this crate.

GNU3/3 and musl3/3, no ignored cases, clippy0 on each:

```sh
CARGO_BUILD_JOBS=1 CARGO_TARGET_DIR=PRIVATE_TARGET \
  cargo test --lib --offline -j1 -- --nocapture --test-threads=1
CARGO_BUILD_JOBS=1 CARGO_TARGET_DIR=PRIVATE_TARGET \
  cargo clippy --tests --offline -j1 -- -D warnings
CARGO_BUILD_JOBS=1 CARGO_TARGET_DIR=PRIVATE_TARGET \
  cargo test --lib --offline -j1 --target x86_64-unknown-linux-musl \
  -- --nocapture --test-threads=1
CARGO_BUILD_JOBS=1 CARGO_TARGET_DIR=PRIVATE_TARGET \
  cargo clippy --tests --offline -j1 --target x86_64-unknown-linux-musl -- -D warnings
```

Repository-configured formatting succeeded. Component checked SHA256
b48e575bfcc0532a35472261c90ff3d6be2d1a8d68727c2d968715651e837e2c;
exact checked formatted bytes were SHA256-matched on adoption. Raw environment,
generated minimal Cargo.lock, test/clippy logs, source hashes and transfer manifest
are in raw/v1. prepare-light.py reproduces source; kiln-v1.command retains the
runner command with payload replaced by its hash. The required policy script
passed in a separate NEW private admitted observer-policy source/target using
unchanged policy inputs; raw/v1/policy.log and policy-rules.sha256 retain it.

No local build/compile/new full worktree or protected ROOT target/cache mutation.
Paused Windows ARM untouched; source, evidence, private targets and small transfer
snapshots preserved. No Rust/compiler test beyond the scoped pure crate is passed.

## R01–R16 and owed ROOT work

| Rule | Scoped accounting |
| --- | --- |
| R01 | Test-only component returns typed errors and checked wire arithmetic; test assertions allowed. No production edit or global panic/allocation proof. |
| R02 | Actual ABI and authenticated endpoint retain/cursor contract from pinned source; no architectural choice. Live body/effects still owed. |
| R03 | u64 limits, ABI encoded sizes, native Channel variants and controlled binary fixture data; no production default/number. |
| R04 | Prior/post canonical StreamState and typed replay failures; pure positive/negative transitions exercised. Actual endpoint/control transitions unrun. |
| R05 | Reuse actual ABI and one immutable outstanding-record witness, no new manager/cache/polling. Full transport/wait audit remains ROOT. |
| R06 | Narrow retained-record component plus three scoped tests/clippy0; combined review owed ROOT. |
| R07 | Comment documents provenance/lifetime; no safety/security workaround. |
| R08 | No performance/comparison/4–5× claim. |
| R09 | No production fix or claimed reproduction; ROOT owns actual r606 report/fix verification. |
| R10 | Reproducible pure corpus/raw logs/hashes retained; real red/green/mutation/effect evidence owed. |
| R11 | GNU/musl x86_64 pure execution only; full seven-target and KVM/HVF/WHP/distribution coverage unverified. macOS ARM64 only. |
| R12 | Shared actual ABI; no platform adapter/installed helper/privilege/trust/crypto change. |
| R13 | Small fixture component; full case awaits exact existing host-cursor seam, no parallel protocol/manager. |
| R14 | No profiling/benchmark/allocation/fault/kernel-accounting or leak claim. ROOT owns all. |
| R15 | Scoped pure/light only, no agents/main merge; signed own-branch checkpoint. Full reconnect implementation and all ROOT gates remain open. |
| R16 | New small private admitted sources/targets, no local compile/new full worktree/deletion; ROOT jobs and paused VM preserved. |

ROOT approval/provision of the exact initial-offset test seam permits live-body
composition in this lane. ROOT executes it only with matching source/artifacts and
its finish correction, then validates retained replay, new later sibling/main
production, refusal/cursor/identity negatives and mutations, raw metadata,
owned stop/remove/observer joins/FDs and independent full population/resource
cleanup. Local observer close is not a remote close-processing proof. Unsupported,
lost or incomplete history and resource observations remain Unknown. No original
normal-exit assertion is waived and no checkpoint releases this lane.
