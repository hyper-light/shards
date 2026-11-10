# Owned member completion and subsequent neighbor progress

Implementer checkpoint only. No real guest, socket, namespace, BPF, mutation,
stress, profiling, benchmark or audit was executed by this lane. The coordinator
owns those runs under R15. This fixture may expose a normal-terminal metadata
failure; no product fix or passing real-VM result is claimed.

POLICY_READ: implementer, base main
`24f48ee5ebf198485117aa93bf47685ad7d7d53f`, policy files unchanged from
`f5cec365`, rules SHA-256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Read full canonical AGENTS.md, development-rules.md, development-evidence.md,
CLAUDE.md, managed-runtime-plan.md and `/Users/adalundhe/g121-handoff.md` before
edits. Owned paths: the normal fixture functions in
`crates/shards/tests/managed_output.rs`, new
`crates/shards/tests/managed_output/tenant_exit.rs`, this evidence directory, and
the coordinator-approved narrow `crates/testguest/src/workload.rs` helper branch.
No children or main merge. The cost fixture, allocation header/module registration,
network lanes, root runners/caches and paused Windows ARM were not modified.

## Behavior and decision

The multi-tenant contract in managed-runtime-plan.md requires stable declared
identity through inactive declarations and subsequent surviving-neighbor progress
after one tenant ends. Existing source at the base revision supplied the seam:

- `managed_output.rs`: `Fixture`, `Observer`, canonical binary payloads, private
  scoped owner subscriptions, exact identity assertions, main exec and shutdown.
- `workload.rs::managed_capture`: exact fd1/fd2 bytes, then native pause. Its
  original one-argument behavior is preserved.
- `managed_limits.rs::agentfile` and `connect`: existing declared ingress grammar
  and host port discovery. The new fixture uses the same NETWORK/EXPOSE/CONNECT
  shape, limited to the continuing HARNESS's explicit input port.
- `abi::run::managed::{Record, StreamState}`: actual canonical state/offset
  validation; these types are reused without another product decoder or manager.

Alternatives: delayed credit of the old helper's initial bytes would prove later
observation, not production after the member ended. Existing `streams` can exit
normally but has no binary pair plus later owner gate. Shell wrappers, another
process manager, polling files and extra product authority were excluded. The
coordinator approved the narrow native helper extension before its edit.

The fixture keeps files-only AGENT member0, normally completing AGENT member1 and
continuing HARNESS member2. Both member1 channels must reach canonical End at the
exact raw-byte offset and acknowledged transport EOF. Only then does the owner
send the single release byte to member2's explicitly declared fixture listener.
Member2 must produce the exact second binary stdout/stderr pair; main exec is
checked independently before and after this release. `Capture` retains the actual
ABI New/Open/Ended states and independent wire/payload offsets across both phases.
It checks member/channel identity on every record, not only the initial labels.

The helper's listener and release-byte read use native one-FD ppoll readiness
against one supplied absolute deadline. Timeout/EINTR re-enters that same deadline
wait; there is no periodic state probe, cache or new product control mechanism.
No syscall/grant/security changes were made. A refused helper remains a failed
fixture for the coordinator; the test cannot widen its own permissions.

Port and positive finite patience are explicit test inputs. Release byte1, the two
fd channels, member IDs0/1/2 and exact payload suffixes define this controlled
fixture protocol. No new production defaults/constants or entitlement are added.
The shared Fixture start uses the caller's absolute deadline for build and start.

## Terminal and cleanup limits

Canonical End plus transport EOF proves stream completion, not population/reaping
or kernel resource return. The trusted top snapshot's absence of the ended UID
is a visible-row assertion only. Stopped inspect `false 0` is daemon state, not
waitpid proof. Those limits are written into the runtime records.

The fixture independently retains `managed-output.ready`,
`managed-output.member-ended`, `managed-output.before-stop` and
`managed-output.stopped`. It stops the owned run, records stopped state, removes
the private run through the daemon, requires its metadata path absent, stops the
private daemon, joins observers and invokes the existing exact observer FD-return
guard. These records and teardown are written before strict terminal assertions.
The normally completed member must retain `ended` / `tail complete` with exact
accepted=consumed wire offsets. The still-running sibling's owner-stop outcome is
`aborted` / `tail unknown`. Observer cancellation after EOF cannot waive the normal
terminal requirement. A red keeps the earlier independent EOF/cleanup evidence.

Source-only concern, **not reproduced**: `Output::transport_ended` publishes normal
metadata when its endpoint worker returns, while the service's ordinary EOF path
appears to leave the endpoint available. Owner stop calls Output::abort. The
coordinator must run the actual fixture and inspect the retained states before
deciding whether there is a product bug. No metadata expectation was relaxed.

## Light validation

`source.sha256` identifies the exact source checked. The new host module SHA-256 is
`311cd9be519de728667c8f820fc9c811842a6068ce6415d040b507ba238175c1`;
the exact extracted helper-functions SHA-256 is
`f86373271aeed93504dbe5cc313afa102dd4885f462af4566242b670702e018c`.

Fedora runner `8fadc1ec213332e16cb0d915a3fb913f`, Kiln headroom request
`linux/amd64,cpus=1,mem=1g,disk=2g`, private
`~/managed-tenant-exit-light-24f-v4/{source,target}`. Source204KiB-scale; final target
81,196KiB. The headroom reservation is not hard child/kernel accounting. The Mac
had approximately33GiB free and ran no build. All other named root caches and
sources remained untouched. Raw environment/toolchain/logs/lock/source hashes are
under `raw/`; `TRANSFER.json` hashes the text-only transfer archive.

`prepare-light.py` copies the **exact new host fixture**, exact existing payload/
label functions, exact new native helper functions and actual ABI sources/build
script into a small compile-only crate. `light-stubs.rs` explicitly substitutes
only surrounding host glue (Fixture/Observer/common/sender scope). None of those
stubs, native helper functions, test bodies or sockets was executed. This checks
Rust types and lints of the actual fixture/helper bodies; it does **not** check
actual host integration, dispatcher, carried helpers, product grants or a real VM.
Direct libc/serde_json versions are pinned to the repository Cargo.lock; the small
generated dependency lock is retained and is not the complete workspace lock.

Exact successful invocations in that generated source, `CARGO_BUILD_JOBS=1` and
private `CARGO_TARGET_DIR`, were:

```sh
cargo check --tests --offline -j1
cargo clippy --tests --offline -j1 -- -D warnings
cargo check --tests --offline -j1 --target x86_64-unknown-linux-musl
cargo clippy --tests --offline -j1 --target x86_64-unknown-linux-musl -- -D warnings
```

All four exit0 (`raw/v4`). No test binary was run. `kiln-v4.command` retains the
runner command with the transfer payload replaced by its exact archive hash; the
reproducible source preparation is:

```sh
python3 docs/research/measurements/managed-tenant-exit/prepare-light.py "$PWD" NEW_PRIVATE_SOURCE
```

Initial harness failures are retained, not passed: v1 omitted the ABI build script
and failed missing OUT_DIR; v3 exposed unsupported Result serialization in the
minimal dependency set, corrected by explicitly recording completed/error fields.
Local Python lacked tomllib and tarfile's newer filter argument; preparation now
uses the lock's constrained name/version pattern and validated regular-file
transfer extraction. These are compile/evidence-harness corrections, not product
bug reproductions. `policy-check.log` passes; changed Rust files passed rustfmt
and `git diff --check`. No complete workspace/clippy or seven-target result exists
for this source.

## Owed coordinator gates

Root must wire the module declaration centrally, preserving the resource lane's
allocation registration ownership:

```rust
#[path = "managed_output/tenant_exit.rs"]
mod tenant_exit;
```

Then build the actual carried testguest/host callers and run on an authorized real
Linux VM host, with independently selected port and finite patience:

```sh
SHARDS_TENANT_EXIT_PORT=EXPLICIT_PORT \
SHARDS_TENANT_EXIT_DEADLINE_MS=EXPLICIT_POSITIVE_MS \
SHARDS_TENANT_EXIT_RECORDS=NEW_ABSOLUTE_DIRECTORY \
cargo test -p shards --test managed_output --profile test-release \
  tenant_exit::member_normal_exit_preserves_sibling_and_main_progress \
  -- --ignored --exact --nocapture --test-threads=1
```

Root supplies existing kernel/init/helper selection and source revision metadata.
Actual grants/syscall compatibility, normal terminal state, exact bytes/offsets,
independent neighbor/main progress, run removal, observer joins/FDs, held daemon/
VM exit watches and population/reaping/kernel return remain unrun. Root also owes
its review, relevant negative/mutation and failure-cleanup checks, seven-target/
supported real-host portability gates and any profiling. Do not call an unavailable
host, ignored test, earlier revision's gate or lightweight stubs a passing result.

## R01–R16 accounting

| Rule | Scoped proof / remaining gate |
| --- | --- |
| R01 | No product code changed. New native test helper returns IO failure; no added unwrap/assert/panic in it. Host assertions are tests. Implicit allocation/panic audit remains root-owned. |
| R02 | Narrow fixture decision uses pinned actual seams and alternatives above; no architectural choice or product fix. Actual VM compatibility/data still owed. |
| R03 | Test literals/protocol and explicit finite inputs explained above; no production policy number. |
| R04 | Actual ABI StreamState/Record enforce canonical transitions; per-channel wire/payload identity and terminal states checked. Gate is one finite release then hold; no new product state machine. |
| R05 | Existing Fixture/Observer reused; one new reader retains actual ABI state for two phases. ppoll/mpsc readiness waits; no cache or periodic probe. Runtime wait cost/audit owed root. |
| R06 | Focused test/helper functions and scoped compile/clippy0; root reviews combined source and complexity. |
| R07 | Comments explain unsafe syscall borrowing, EOF/ACK constraint and cleanup-before-contract ordering. |
| R08 | No speed/resource comparison or 4–5× claim. All benchmarking root-owned and unrun. |
| R09 | No product bug fixed. Terminal concern is explicitly source-only until root reproduces it live. |
| R10 | Repeatable fixture and raw compile/error records retained; live red/green/mutation/cleanup results owed. |
| R11 | GNU/musl scoped Linux x86_64 compile0 only. Seven-target actual integration and KVM/HVF/WHP coverage unverified. macOS ARM64 only; Windows ARM remains paused. |
| R12 | Existing native helper/capture paths and explicit normal grants reused. No privilege, trust/crypto, security or new installed binary. Actual portability owed. |
| R13 | One targeted fixture; no generic manager, cache, shell or product abstraction. |
| R14 | No profiles/allocations/faults/performance measurements or claims. Root owns them. |
| R15 | Implementer performed scoped source/compile only; no children/audits/heavy E2E/chaos/sweeps/main merge. Signed named-path commit/push checkpoint follows. Root owns review and all live campaigns. |
| R16 | Private source/targets only, headroom checked; no local build or cache deletion. Existing evidence/source preserved; root caches and paused VM untouched. |
