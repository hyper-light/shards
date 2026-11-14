# Credential review fixes: creds-05 through creds-08

POLICY_READ: implementer; base `9efde72524dd626f3ff84e6b9891852d7c810a9b`.
AGENTS `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41`;
development-rules `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`;
development-evidence `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63`;
CLAUDE `588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010`.
Fix brief: `/tmp/g121-shot/review/fix-brief.md`, common brief read in full.
P0 creds-01 through creds-04 and chunked token-path request handling belong to Claude lanes.

## Environment and scope

One runner: `adas-mac-studio.local`, Darwin arm64, through kiln with explicit `on`.
Own checkout/target: `~/g121-tmp/fix-creds-v1/{repo,target}`; one cargo at a time,
`CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=4` (build concurrency only, not a runtime policy).
A tiny carrier transmits a git bundle; source clones from GitHub. No local build/test,
VM, benchmark, E2E, fleet or mutation campaign. No real credentials used.
Other supported targets remain unverified in this lane; macOS Intel is unsupported.

## Red confirmation

Commit `6eb3ad718128d45f54008601b4ce121087055345`, kiln r9:
`cargo test -p shards-net creds_ -- --nocapture`: **0 passed, 3 failed** for the
reported symptoms, not compile failures. Retained runner log:
`~/g121-tmp/fix-creds-v1-red.log`.

- `creds_05_a_delayed_answer_cannot_restart_the_lessors_lease`: a delayed answer
  extended the lease; `held` returned a value after the lessor's end.
- `creds_06_a_pending_lease_resumes_each_flow_once`: actual waiters
  `[1, 1, 2, 1, 2, 2]`, expected `[1, 2]`.
- `creds_07_ungranted_descendants_and_addresses_are_not_signed`: the signer
  accepted `child.api.example.test` with only `api.example.test` granted.

Earlier carrier attempts failed before tests because the sparse checkout omitted
the injector A/B module and HPACK's included RFC fixture; these are setup failures,
not reds. Sparse checkout now includes both and the repository's clippy.toml.

## creds-05: the pending ask owns the time budget

Fix `a52f4518c`: the network process records the first ask's monotonic Instant and
anchors the returned remaining lifetime there. Retries keep that Instant. If the
budget is spent at delivery, or its end cannot be represented, the valid response
resumes its callers with typed `Lapsed`, retaining no copy of its value. Held leases
now always have a finite representable end; there is no overflow-to-run-lifetime
fallback. Two regressions cover wholly delayed answers and partial delay/retries.

Proof: let A be the network ask time, S the SDK's settle time and E the lessor's
expiry. The SDK returns B = E - S; causality gives A <= S. The network's cutoff
A + B <= E regardless of delivery delay. No wire timestamp, wall-clock alignment,
clock-origin conversion, fixed allowance or learned cold-start estimate is needed.
The first request measures its own full wait, including cold broker startup.

This deliberately spends broker work as well as transport time. A slow broker's
returned lease can therefore be refused early; this is conservative uncertainty,
not renewed authority. The run-host ledger can still answer `Live` until its later
bookkeeping expiry, which is an existing typed refusal and sends no provider request.
ROOT should exercise renewal at that boundary in its real relay campaign.

Alternatives considered: retaining receipt-time starts extends authority by an
unbounded scheduling delay (the red); serializing opaque std::time::Instant is
not a portable wire contract; absolute wall time permits clock adjustments to
extend an outstanding budget; a new shared monotonic-epoch/versioned wire would
expand SDK/Hex compatibility scope. The final consumer can instead reduce its
authorization conservatively without changing that wire. The measured acquisition
time debit is also the lease-validity technique described in the Redis primary
[distributed-lock documentation](https://redis.io/docs/latest/develop/clients/patterns/distributed-locks/);
this uses only that timing principle, not its quorum algorithm or drift constants.
Primary page retrieved 2026-11-13; runtime lifetime remains the broker's input.

Green merged source `950c1a0fe146f1e2eba0f1a7f1b489c1e87ab8b0`, kiln r21:
`cargo test -p shards-net lease::`: **14 passed, 0 failed, 0 ignored**;
`cargo clippy -p shards-net --all-targets -- -D warnings`: passed.
Runner log: `~/g121-tmp/fix-creds-v1-05-06-green.log`.

```text
test lease::tests::creds_05_a_delayed_answer_cannot_restart_the_lessors_lease ... ok
test lease::tests::creds_05_retries_and_delivery_spend_the_original_ask_budget ... ok
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 150 filtered out
Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.27s
```

The optional protocol-versioning question became unnecessary when this narrower
final-consumer design was found; no external wire change is made.

## creds-06: unique live flow waiters

Fix `1b21eb21a403024baae2a7b815b8b1f5ac987255`. An existing refusal-routing test
deliberately retries one flow; `e68c58eac` updates its old duplicate-count assertion
to require exactly the distinct flows `[7, 8, 9]`, preserving refusal assertions.

Each pending lease holds at most one waiter for each live flow. A repeated H2
stream/turn joins that waiter. `Stack::release` removes the flow from pending and
resumed records before releasing its TCP slot; an outstanding broker ask remains
identifiable so a late answer still belongs to the run. Grant/reference authority,
refusals and value lifetimes are unchanged by this fix.

The existing vector is sufficient: no parallel queue, cache, timeout, retry count,
chosen bound or learning cold start. Its cardinality derives from live callers.
An additional regression exercises teardown both before and after the answer.
Successful and refused answers exercise the uniqueness regression.

Merged source `16ebc78cf4dfbab8e46a6fa99121ee07bb3bf046`, kiln r13/r16:
`cargo test -p shards-net lease::`: **12 passed, 0 failed, 0 ignored**.
`cargo clippy -p shards-net --all-targets -- -D warnings`: passed in r16;
`python3 scripts/check-development-policy`: passed in r16. Logs on the runner:
`~/g121-tmp/fix-creds-v1-06-merged.log` (tests and initial clippy failure),
`~/g121-tmp/fix-creds-v1-06-clippy.log` (passing clippy). The initial clippy
failure was an omitted clippy.toml in the sparse checkout, causing an unfulfilled
`disallowed_methods` expectation in shards-testdir; restoring the actual config
resolved it without source or lint suppression changes.

Retained exact relevant result excerpts from the kiln replies:

```text
red 6eb3ad718, r9:
creds_06_a_pending_lease_resumes_each_flow_once
assertion `left == right` failed
  left: [1, 1, 2, 1, 2, 2]
 right: [1, 2]

merged 16ebc78cf, r13/r16:
test lease::tests::creds_06_an_ended_flow_keeps_no_waiter_or_answer ... ok
test lease::tests::a_resume_carries_each_flows_refusals_to_it ... ok
test lease::tests::creds_06_a_pending_lease_resumes_each_flow_once ... ok
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 141 filtered out
Development policy checks passed; evidence/coordinator review is still required.
```

## creds-07: the CA signs exact grants

Fix `09ffbfd32`: `Ca::leaf` checks its immutable exact-host authorization list,
ASCII case-insensitively, before generating a key or signing. `LeafError::Ungranted`
is distinct from a certificate-operation failure; the injector only renders it.
The list is authorization state, not a performance cache; it lives with this run's
CA and changes only by creating a new CA. No new wait, cache, runtime number or
platform implementation is introduced.

The red at `6eb3ad718` accepted `child.api.example.test` under a grant to
`api.example.test`. The retained regression also checks unrelated DNS names, IPv4,
IPv6 and an allowed host in uppercase. Existing negative verifier cases separately
issue certificates in test code, bypassing the production signer guard, to retain
coverage that DNS constraints reject unrelated names. Exact-host authorization is
the signer's responsibility: RFC 5280 §4.2.1.10 defines DNS constraints as subtrees.
No unsupported exact-host encoding is added to the certificate and no assumption
that subtree constraints reject descendants is retained.

Green source `09ffbfd32278364250b30368256684855a843e34`, on merged main `67a344fde`:
`cargo test -p shards-net ca::`: **4 passed, 0 failed, 0 ignored**;
`cargo clippy -p shards-net --all-targets -- -D warnings`: passed (resumed-run r5,
runner log `~/g121-tmp/fix-creds-v1-07-green.log`).

```text
test ca::tests::creds_07_ungranted_descendants_and_addresses_are_not_signed ... ok
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 169 filtered out
Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.28s
```

## Lease landing

creds-05/06 landed at `67a344fde0bb7e150a19174097b8b18a93a7c9e6`.
Last focused checks on merge `74426d1ad`: 14 lease tests and shards-net all-target
clippy passed, runner `~/g121-tmp/fix-creds-v1-lease-final.log` (resumed-run r4).
The final disjoint merge `67a344fde` changed API/shards/VMM/docs only; its diff
against `74426d1ad` had no shards-net/dependency/Cargo inputs. It was pushed at once
under ROOT's amended landing-under-contention rule. No focused results are claimed
for those unrelated crates.

## Policy accounting and coordinator work

R01/R04/R06/R07/R13: checked ranges, no new production panic, explicit Asked/Held
states and typed refusals retained, ownership cleanup is in the existing release path.
R02/R03/R05/R12: retain one authoritative pending waiter set; no new cache or runtime
literal; shared network implementation applies on every supported host.
R09/R10: remote unit red/green evidence, with setup failures distinguished above.
R08/R14: no performance, allocation, fault or comparative claim; ROOT owns profiling.
R11: this lane tests one macOS arm64 runner only; CI/ROOT owe the other platforms.
R15/R16: scoped implementer, no subdelegation, signed commits, own target removed at
task end; policy script is only a mechanical check, not coordinator evidence review.

ROOT owes real-VM credential/HTTP2 lifecycle and broker-stall coverage. Existing
HTTP2 fixture: `cargo test -p shards --profile test-release --test api_image an_h2_request_to_a_granted_host_is_filled_never_indexed_and_its_echo_comes_back_masked`.
For waiter-resource coverage, withhold the broker's lease answer while a guest
sends PINGs on a connection with one parked request, then end that guest flow;
pending/resumed waiter state must stay bounded by live flows and contain none
of the ended flow. No such live VM/resource campaign ran here.
