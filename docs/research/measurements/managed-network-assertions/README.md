# Shared strict host hook and cleanup assertions

This test-only refactor extracts existing repeated behavior from two actual
host callers into the private `assert_sock_addr_hooks_and_cleanup` helper in
`crates/shards/tests/managed_network.rs`. The source delta is 46 additions and
86 removals. It changes no production code, socket operation, admission,
wait/cache, ownership or cleanup implementation.

POLICY_READ implementer: canonical main
`24f48ee5ebf198485117aa93bf47685ad7d7d53f`, policy file revision `f5cec365`,
rules SHA256 `d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Read full AGENTS.md, development-rules.md R01–R16, development-evidence.md,
CLAUDE.md, managed-runtime-plan.md and `/Users/adalundhe/g121-handoff.md` before
editing. Coordinator owns all audits, live/mutation campaigns, profiles,
benchmarks, combined gates, review and main merges. Owned source/evidence paths
are only this record's directory and the host fixture; branch
`p/managed-network-assertions` starts at that exact main revision. No children.

## Assertion correspondence

Before line numbers refer to main 24f; after numbers refer to the source hash
below. Both callers still pass `presentation.hooks(&mode)` without a default.

| Existing assertion | Before scripted / buffered caller | Shared after |
| --- | --- | --- |
| Exactly one Program per selected named hook | 790–794 / 1186–1197 | 774–778 |
| Forward Program backend→responder; reverse Recvmsg4/Getpeername4 responder→backend | 795–803 / 1208–1218 | 779–787 |
| Exactly one LinkAttached and LinkDetached per selected named hook | 804–814 / 1186–1197 | 788–798 |
| Each selected LinkDetached reports detached=true | 810–812 / 1205–1207 | 794–796 |
| Total Program/LinkAttached/LinkDetached count equals selected hooks, rejecting unnamed extras | 815–821 / 1198–1204 | 799–805 |
| Cleanup/Observed record exists; hook_setup=Ready | 872–876 / 1219–1223 | 806–810 |
| links_required and links_acquired each equal selected hook count | 877–878 / 1224–1225 | 811–812 |
| backend_joined, relay_joined, link_detached, cgroup_removed each true | 879–886 / 1226–1233 | 813–820 |

The scripted caller invokes the helper at 844; the buffered caller at 1193.
Cleanup validation now runs beside hook validation in the scripted caller;
all records are already captured after the guest returns. Both retain the
existing first Cleanup/Observed selection; this change adds no uniqueness claim
for that aggregate record. A missing selected hook/count/fact cannot turn into a
passing fallback. The helper uses the scripted caller's existing implementation;
its per-selected-hook detach check plus exact totals covers the buffered caller's
previous all-detached-record loop. Program endpoint checks are similarly bound
to the selected hook, with extra programs still rejected by the total count.

Both callers retain their operation-specific checks: actual namespace/cgroup
identity, exact original native IP and port, connected/unconnected negative
controls, distinct fresh clients, binary/empty bytes and metadata, worker/client
outcomes and error rejection. The buffered caller retains payload capacities,
all four custody/transfer observations, independent real backend receipts,
distinct held upstream socket and `backend_source` bypass rejection, explicit
source change, completions and both child reaps. No source-preservation,
total-budget or universal cleanup claim is added.

## Light checks and their limits

Local arm64 macOS, with 34 GiB free before work and no local build:

```text
scripts/check-development-policy
Development policy checks passed; evidence/coordinator review is still required.
rustfmt --check --edition 2024 --config skip_children=true crates/shards/tests/managed_network.rs
exit 0
```

[light-check.py](light-check.py) copies the complete host fixture into a new
standalone crate, replacing only `mod common;` with explicitly inert common
stubs. Every VM/common operation hard-panics if called. The five existing pure
tests execute; eight existing actual-guest tests retain their ignores. This
compiles both complete caller bodies and the helper against serde_json 1.0.151,
but **does not compile the normal common/helper graph or execute either real
caller**. No test, VM, socket, namespace or BPF campaign is hidden by the stubs.

Kiln run r2 on Fedora amd64 runner `8fadc1ec213332e16cb0d915a3fb913f`,
Linux 6.19.10-300.fc44/glibc 2.43, rustc 1.99.1 (58cf8f9c3), cargo 1.99.1 (5f94df478):
requested 1 CPU/1 GiB/2 GiB through headroom guard; `CARGO_BUILD_JOBS=1` and one test
thread. Private source/target: `~/managed-network-assertions-light-24f-v1`.

```text
python3 light-check.py managed_network.rs extracted-host
cargo test --offline --manifest-path extracted-host/Cargo.toml --target-dir extracted-host/target --target x86_64-unknown-linux-gnu -- --test-threads=1
exit 0; 5 passed, 0 failed, 8 ignored; 0 doctests
```

The raw [light-result.json](light-result.json) retains actual output, all 26
ordinary dead-code warnings, source hash, commands and the initial unstarted
invalid-cwd preflight (`/home/ada` absent). Omitting that guessed cwd uses the
runner's actual home `/home/adalundhe`; no build/test failure was suppressed.
Original private source, generated Cargo.lock, extracted source, log and target
remain on that runner. Protected coordinator clones/targets were not modified.

| Artifact | SHA256 |
| --- | --- |
| Before host fixture at main 24f | `1cc862553b4e51880db6ef2e7a1758c946de14d3c35fa76f68d3fb3b50720db3` |
| After host fixture / actual light input | `20f1ad7e0ce7f864a3f5b6326b0e5465dbd4baba2f7bd425c01586590777e22c` |
| light-check.py | `22fe404b6a77e36951b3ca71b7613e8d226c4db90c1c670fd65d3261d9f520e0` |
| Runner extracted-host/Cargo.lock | `22b884373db2e7577c7847f824cb5e232fbe7c27bd2cbf6ede4978ee62b7ea92` |
| Runner extracted-host/src/lib.rs | `93d04d05d5e8cc2cd5b11601a0b837d0b143f4903f9f0b296a984452948af66e` |
| Runner extracted-host/cargo-test.log | `dc780a828819d0a5cc5e731b5a836497cb246d86e0d5ec4ffd1fd498dd0075db` |

## Existing live evidence and owed coordinator gates

[The coordinator's retained baseline](../managed-coordinator-forward/README.md)
records r531/r556/r567. Original destination-only clients fail strict 101 on
actual peer mismatch; fixed-original-peer clients pass. Independent Connect4/
Sendmsg4 removal exposes actual direct backend peers despite correct client
bytes; request/reply corruption and over-budget admission produce the distinct
retained failures. Each restored case passes. The original17-byte case also
shows unnecessary setup before refusal, subsequently reordered on main; no leak
claim follows. Those are earlier source results, not this refactor's passes.

Owed to the coordinator on the combined exact source: normal complete host
compile; actual scripted and buffered callers in both modes/profiles; original
negative presentation controls; independent named-hook/endpoint/count/detach/
aggregate-cleanup assertion mutations with restoration, and existing backend
bypass/byte/budget regression controls. Actual forbidden-effect witnesses must
remain distinct from a missing log. Seven-target lint/build and relevant real
KVM/HVF/WHP validation remain root-owned. Windows ARM remains paused/unverified.

R01/R04/R06/R07/R13: only test assertions moved; the explicit lifecycle and all
production/error paths are untouched. R02/R03/R05/R12: one private helper at the
existing two-caller test seam replaces the same concrete behavior; retaining
duplication was the alternative. No broader architecture choice, new production
literal, platform adapter, wait or cache is introduced; the existing live record
justifies the assertions, without a new performance claim. R09/R10: no new bug
or reproduced defect claimed; before evidence is linked and new live/mutation
gates are explicitly owed. R08/R14: performance distributions, allocation/copy/
fault/kernel cost and 4–5× comparisons are unrun/root-owned, with no optimization
claim from deleting source lines. R11: light GNU extraction only, full platform
and hardware gates owed. R15: scoped implementer/light checks and signed
checkpoint only, review/merge and all campaigns reserved to root. R16: source,
evidence, paused Windows ARM and protected targets preserved; no privilege,
security, trust or crypto change.
