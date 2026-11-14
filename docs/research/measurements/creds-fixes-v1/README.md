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
