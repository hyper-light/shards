# Combined coordinator gates at f06247968

Source `f0624796855b068b3f61c4a96dc11cfcf774de18`, Fedora amd64 KVM,
coordinator r497 and r500. This record closes the listed fixture gates only.
The managed-runtime plan, mandatory forwarding, hard aggregate accounting,
native endpoint compatibility, adapters and performance targets remain open.

## Actual results

| Gate | Observed result |
| --- | --- |
| Full normal-helper host callers | `managed_network` and `managed_output` compile successfully; actual executable hashes retained. |
| Output regressions | 6 passed, 0 failed, 2 ignored. Two actual VM output/authentication cases and four pure parser/metadata cases. Ignored tests are not counted as passing. |
| Explicit N0 | Actual files-only VM, no managed endpoint metadata, main exact progress, owned shutdown and observer FD return pass. |
| Explicit N1 | Actual two output channels, exact bytes, held stdout credit, main progress, cancellation/join, remaining FIN/consumption and terminal cursor checks pass. |
| Explicit N2 | Actual mixed AGENT/HARNESS and files-only declaration hole, four channels, the same checks and surviving main progress pass. |
| Destination-only connected UDP4 | Both Binary/Empty exchanges complete, children reap, workers join, link detaches and cgroup is removed. Strict original-endpoint compatibility remains Mismatch; test exit101. |
| Destination-only unconnected UDP4 | The same cleanup observations succeed; received peer port remains the responder's. Strict compatibility remains Mismatch; test exit101. |

The r497 diagnostic driver returns0 because it explicitly requires the two known
endpoint mismatches and complete cleanup, then requires the output regressions
and N0/N1/N2 runs to pass. It does not turn either strict network failure into
native compatibility. No presentation hook or forwarding implementation is
selected here.

All protocol/cancellation fixtures use the real CLI, carried helpers, actual
testguest and the retained experimental kernel. Kernel SHA256
`97819d00fecb5de0cffc4bf292af9c78c5d662336e07226ae58731fd580537ae` remains an
experimental artifact, not a product pin. Caller-selected cost deadlines are
300000ms, one immutable protocol deadline and a separately labelled cleanup
phase of the same duration. These test envelopes do not become runtime policy.
Each command, selected environment, source, artifact identity, exit and raw log
is retained. `driver.py` is the exact executed coordinator driver.

## One-shot resource pilots

There is **one observation per count**, with different N0/N>0 readiness contracts.
The samples are unpaired, non-atomic and include fixture/observer costs. They are
not latency distributions, a throughput result, an allocation count, a scaling
law or a comparison with another implementation.

| Active members | Published endpoints | Daemon threads / FD entries / RSS kB | VM threads / FD entries / RSS kB | Observer threads / FD entries / RSS kB |
| --- | --- | --- | --- | --- |
| 0 | 0 | 16 / 75 / 29852 | 48 / 68 / 267084 | 3 / 8 / 24780 |
| 1 | 2 | 20 / 85 / 33916 | 51 / 74 / 271252 | 5 / 12 / 27308 |
| 2 | 4 | 22 / 91 / 34388 | 51 / 74 / 273936 | 7 / 16 / 27688 |

All nine role identities matched before and after their snapshot. Raw status,
stat, smaps_rollup, cgroup, partial-coverage annotations and only owned-inode
UDP rows are retained. Published endpoint counts describe the experimental 2N
topology, not a final choice. The derived `pilot-status-summary.json` preserves
units and scope. No constant, quota or entitlement is inferred from these values.
Required interleaved comparisons, repeated distributions, CPU/allocations/fault
attribution and isolated parent/kernel accounting remain owed.

## Meaningful cleanup guard removal

Root r500 removes **only** the held-mount branch's `unshare(CLONE_FS)` operation
from the fixed fixture, keeping all strict native endpoint checks. The actual
guest then reports EINVAL at cleanup and `cgroup_removed=false`, while both
children still reap, both workers join and the link detaches. After exact-byte
restoration, a fresh actual guest reports `cgroup_removed=true`.

Both tests still exit101 for the independent endpoint mismatch. The mutation
driver checks actual cleanup fields and errno, not a changed exit code or a lost
alert. Fixed/restored source SHA256
`52b4c270623abac18894c98cfd1bca880fd091804dac56488706f12c9ec4a5f5`, mutant patch,
owned source, original commands, before/after logs and final empty diff are
retained. Restoration runs in `finally`, with source mtime advanced so the
instrumented/mutated guest artifact cannot be reused as the restored source.

## Remaining exit and accounting limits

The cost fixture proves owner-stop/observer-join success and observer FD return;
it does not hold daemon/VM exit handles through shutdown. A later independent
read of only the six own daemon/VM identities finds all six absent with ENOENT.
Those are later pathname observations, not bounded exit timing, complete
population reaping or kernel-resource return. No discovered PID was signalled,
adopted or killed. Stronger held-handle coverage remains separate work.

## Durable records and policy

Root r513 runs the full `scripts/lint` entrypoint on exact source
`338b6b71e51568f42f39b966e2c42bc0a663e00a`: format, all seven supported targets
and the script's GPU checks pass. This includes the original finite
destination-only comparison, cleanup fix, cost fixture and host hex-chunk lint
correction. It does **not** cover the later presentation-profile source or future
exit-handle work. Original r498's actual constant-chunk lint failure, r504's
metadata-only preflight failure and r513's complete passing log are retained
under `matrix-338b` with 27 verified raw files. Archive8880 bytes, SHA256
`e38cc545742f4bd6c57a1970d9403a862849ab856f05562d3e534f21a6d7e156`.

The private Mac clone repeatedly receives untracked `.DS_Store` metadata. The
passing invocation verifies tracked source is clean, hashes and classifies only
those exact untracked metadata files, and preserves them in place. No source
file is ignored and no global Git setting changes. The prior moved metadata is
also preserved. A separate remote hand-off integrity refusal (r501) is retained
with exact hashes and no command record; its cause is not established. Retrying
from an immutable detached source retains the same transport integrity checks.
The normal AWS-LC compiler-feature probe/fallback warnings remain unmodified in
the successful log; no crypto or compiler verification is disabled.

All 172 regular raw records are listed in `MANIFEST.json`. `TRANSFER.json`
records verified private archive69013 bytes, SHA256
`5c671839b3de7f797c0a4b15353bf29e1efb25596256854915c6673f07221c8f`.
Regular paths and absence of executable artifacts were verified before retention;
originals and the archive remain on the runner and local workspace. No private
credential/key material is supplied to these synthetic fixtures.

Policy coordinator receipt: origin/main f5cec365, R01–R16 SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Root owns the actual review/tests/mutation/samples; implementers provided scoped
source and light checks only. No production panic-free, leak-free, 4–5x,
all-platform or complete-layer claim follows. Windows ARM remains paused.
