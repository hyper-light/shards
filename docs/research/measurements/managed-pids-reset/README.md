# Selected PID ceiling reset

Implementer `/root/managed_resource_enforcement`; coordinator `/root` (Kiln
`01a148884a4770fdaf0111f3f792a248`). Isolated branch `p/managed-pids-reset-v1`
starts at ROOT integration `4299285281a0b588ed04b854842c1c14b0950260`.
The completed capture branch/commits `1ae95bbda` and `0f10b76b0` remain untouched.
Owned source is only the shared `resources.rs` cgroup builder, its direct create,
visit and update caller spans, and relevant existing unit tests. This directory
owns the implementation/light evidence. No children or main merge.

POLICY_READ before edits: current origin/main
`0342b85d9c808e3ced041a6c3ef0e9bf70438272`, full AGENTS, R01–R16,
development-evidence, CLAUDE, managed-runtime-plan and current
`/Users/adalundhe/g121-handoff.md`. Rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
ROOT owns independent review, all actual VM/fault/mutation/platform integration,
audits, profiling, benchmarking and main merges.

## Actual prior failure

ROOT reproduced the symptom before this fix in its normal carried r916 at
integration 429: initial guest `pids.max=max`, owner update to 2 succeeds and
kernel/OTLP both report finite 2; update to -1 succeeds but the guest and OTLP
remain finite 2. The strict caller failed at `metrics.rs:275` expecting unbounded.
This is a real stale kernel ceiling, not an inferred telemetry encoding defect.

The ROOT-provided tool receipts/excerpts are copied byte-for-byte into
`records/root-429-gate-receipts.json` and
`records/pids-reset-red-direct-kernel.json`. The latter's actual kernel stdout
is 70 bytes, SHA-256
`06c859c7d2f2b6ab88c82f6925f16600d484e54b1c1ce56cfa2fb72f5fbb8012`,
reporting memory.max `max` and pids.max `2`. Update stdout is the 65-byte run ID,
with empty stderr. All four embedded payloads match their recorded lengths and
hashes (`records/root-receipt-verification.json`). ROOT's ordinary suite was
24 passed/1 failed/10 ignored, no timeout; controller FDs 9→9 and fixture FDs
7→7. Whole-population/kernel resource return remains Unknown.

These are verified tool receipts/excerpts, not the full original archive.
ROOT retains the frozen 429 binaries and original files under
`~/root-managed-limits-4299285281a0-v1` and owns their closing archival/source
association. This implementer read only the provided local receipts, never ROOT's
runner/target. Corrected real-VM/OTLP reset and mutation remain ROOT gates.

## Mapping and primary basis

The [Docker update reference](https://docs.docker.com/reference/cli/docker/container/update/)
documents -1 as unlimited. Pinned [Moby v28.5.1 getPidsLimit](https://github.com/moby/moby/blob/f8215cc266744ef195a50a70d427c345da2acdbb/daemon/daemon_unix.go#L115)
normalizes a selected nonpositive PID value to OCI -1 so an existing ceiling can
be reset. [cgroups v0.0.4 pids](https://github.com/opencontainers/cgroups/blob/a3e2ecd1f756a19cee15f85b96337a59c3b5337b/fs2/pids.go#L14)
writes a nonzero selected PID limit; its
[numToStr](https://github.com/opencontainers/cgroups/blob/a3e2ecd1f756a19cee15f85b96337a59c3b5337b/fs2/memory.go#L15)
renders -1 as `max`. Raw file lengths/hashes and resolved tag commits are retained
in `records/primary-sources.json`. Initial web-tool tag-API opens were unavailable;
standard-library HTTPS retrieval subsequently resolved both tags and hashed the
primary files. No retrieval failure is counted as a runtime result.

Shards' existing typed `Resources` uses zero for absent. Preserve that meaning:

| Selected value | Shared emitted setting |
| --- | --- |
| Negative | `pids.max=max` |
| Positive | Exact decimal `pids.max=N` |
| Zero/absent | No PID setting; update merge retains the previous selection |

Create, revisit and update use this same mapping. A separate update-only reset
encoder was rejected because it would leave the other real callers inconsistent.
Explicit API-zero presence loss is not fixed or claimed. No other unreproduced
resource reset, owner entitlement or broader policy change is included.

## Construction and failure ownership

`cgroup` now returns the complete byte vectors or typed `CgroupError::Capacity`.
The existing formatting/writer pattern counts UTF-8 bytes with checked addition,
fallibly reserves that exact requirement, and checks each fragment against actual
remaining capacity before `push_str`. Each output entry gets a fallible Vec slot
reservation; `String::into_bytes` transfers its existing storage. There is no
picked capacity, speculative trait/framework/dependency or fixed scratch size.

Only the PID mapping changes emitted values. Positive PID, memory, swap, IO,
CPU and cpuset bytes/order remain covered by old and new golden assertions.
Production zero/negative branches derive from the selected-resource contract;
`max` is the existing cgroup reset representation. Reserving one Vec slot derives
from appending one setting; subtracting one CPU derives from zero-based numbering.
No numeric runtime default or allowance is introduced.

New failure paths remain typed until the existing outer String boundary.
Diagnostic construction uses the same fallible writer, with empty emergency
text if allocation also fails; it does not call `.to_string()` on that failure.
Create unconditionally abandons/returns None, visit returns Err, and update
returns Err without printing success. Update prepares every setting before its
existing PLUG/CPUS/CGROUP sends. A partial settings Vec cannot escape construction.

This is a narrow builder migration. Existing surrounding resolve/merge/verify/
request encoding/dispatch allocations are unchanged and not certified OOM-safe.
The builder calls `run_cpus` only when cpuset text is empty; that helper's existing
empty `parse_uint_list` returns an empty Vec without population. No unrelated
allocation cleanup, polling/cache, manager, ABI, kernel, metrics or helper edit
is introduced.

## Scoped checks and remaining gates

Kiln r11 ran on native Mac arm64, admitted 2 CPU/2 GiB RAM/4 GiB disk, at most
600 seconds, cargo -j2, under `/tmp/g121-mac.lock` and a checked 30 GiB disk floor.
Private harness `/Users/adalundhe/managed-pids-reset-light-v1`, private target
`/Users/adalundhe/managed-pids-reset-target-v1`. It executed **12 tests passed,
zero failed/ignored/filtered**, then strict library Clippy 0. Product and lock
hashes stayed unchanged. Raw streams and commands are in `records/light-v1/`.

The harness includes the entire actual resources module by path. Its update
adapter excludes only the outer Daemon impl/imports; actual asked/merge/restart
policy/tests remain. No helper, VM, kernel controller write, allocator-fault
campaign, full matrix, profile or benchmark ran here. This is not a complete
three-caller compilation or proof the real guest reset now passes. ROOT owes
those actual caller/carried/VM/OTLP before/after and mutation gates, supported
seven targets and any performance/fault review. Earlier 429 passes do not pass
this source. No performance or allocation-free/leak-free claim is made.

Policy and owned-source rustfmt checks passed. The complete staged whitespace
check returned 2 solely for the authentic `records/light-v1/units.stdout` extra
blank line at EOF. It remains unchanged; no whole-check pass is claimed. The
named-path check excludes only that exact raw stream, with its result recorded
in `records/whitespace-check.json`. Exact product-lock package tuples and capture
branch preservation are in `records/dependency-pins.json`.

## R01–R16 account

| Rule | Evidence and remaining boundary |
| --- | --- |
| R01 | New builder/diagnostic allocations and arithmetic are fallible/checked, fragment capacity bounds prevent hidden writer growth, callers retain refusal on empty text. Ordinary writer/UTF-8/extreme units pass; actual allocation faults/full caller proof remain ROOT. |
| R02 | Exact ROOT live red plus independently checked pinned Docker/Moby/cgroups mapping and shared-versus-specialized alternative above. No broader policy selected. |
| R03 | Literal/representation derivations above; no cap/default/preallocation allowance added. Test inputs are fixtures. |
| R04 | Complete-or-typed-failure construction before dispatch; selected absent/finite/reset meanings explicit. No partial success or error-text control protocol. |
| R05 | One existing shared builder for three real callers. No wait/cache/manager added. The single update-local host CPU observation supplies complete construction; no polling or persistent cache. |
| R06 | Focused formatter/writer/error helper and existing builder branches; caller changes are explicit. Independent complexity/path audit remains ROOT. |
| R07 | Comments record the reset/absence distinction and non-growing writer obligation. |
| R08 | No comparative measurement or 4–5× success claimed; target/cost work stays with ROOT. |
| R09 | ROOT's exact real-kernel stale PID ceiling precedes implementation; tool receipt hashes and source revision retained. Corrected VM result still owed. |
| R10 | Source/command/raw receipt record and ROOT excerpt provenance retained. ROOT owns full original archive, after/mutation/cleanup proof. |
| R11 | Native macOS arm64 light only; Linux GNU/musl and Windows amd64/arm64/full VM combinations unverified here. Paused Windows ARM64 remains untouched; Intel Mac unsupported. |
| R12 | One standard Rust implementation, no OS adapter or installed binary, root privilege or crypto change. |
| R13 | Only the shared builder, three caller spans, direct units and evidence changed. No unrelated reset/zero-presence/policy claim. |
| R14 | No profiling/benchmarking by implementer; ROOT owns all allocation/copy/fault/parent/kernel/latency and comparable distributions. |
| R15 | Implementer, no children/main merge. Finite private light run only; signed checkpoint returned for ROOT source review and combined gates. |
| R16 | Own source/target, floor/lock respected, no cleanup or other lane/cache/source/evidence mutation. Capture branch and all frozen/held scopes preserved. |
