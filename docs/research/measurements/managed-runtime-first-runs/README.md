# First coordinator live managed-runtime results

Coordinator `/root`; integration policy base b4918cd8; policy SHA-256
`0d936117dbc6663eb02829c3e921879cecdad3a8f503047588baa21dc82da936`.
These are focused results at the named revisions, not a combined-tree gate or
completion of any management-plan phase. The VM intentionally supports multiple
AGENT/HARNESS tenants; tenant identity and failure isolation are required.

Runner8fadc1ec213332e16cb0d915a3fb913f is Fedora amd64, uid1000, with real KVM.
Environment logs retain toolchain/kernel facts. Root owns the campaigns, evidence
review, mutations, profiling and merges. Implementers ran scoped light checks.
Logs here retain the final90 lines; full logs remain in the runner's home under
the same names. Synthetic probe output contains no provider credentials.

## Real VM output baseline

At cdc463b4302bbe85ca2d6107a0b4c2cc6b8a594a, kiln r47:

```sh
cargo test -j 4 -p shards --profile test-release --test build \
  supplied_member_output_does_not_rewrite_the_main_protocol -- --exact --nocapture
```

Exit101;0passed/1failed,42.08seconds. Main JSON-RPC stdout was exact. AGENT and
HARNESS fd1/fd2 were combined and prefixed on main stderr, with line rewriting.
The Run test helper's displayed invalid-UTF8 replacement is diagnostic decoding,
not evidence that production capture itself rewrote invalid UTF8. Raw-byte green
round-trip and independent channel/tenant backpressure remain owed. The helper
also forces SHARDS_TIMING; the revised fixture checks the known final timing
diagnostic exactly instead of treating arbitrary stderr as acceptable.

## Real VM lifecycle identity and budget boundary

```sh
cargo test -j 4 -p shards --profile test-release --test managed_lifecycle \
  a_supplied_agent_runs_below_a_protected_native_reaper -- --exact --nocapture
cargo test -j 4 -p shards --profile test-release --test managed_lifecycle -- --nocapture
```

| Source/run | Observed outcome |
| --- | --- |
| c9d0a46f/r56, old launch with fixture | Exit101,0passed/1failed,29.50s; actorPID1/PPID0, proc1 owner200000, signal/memory/fd errno0. These are self-access facts, not a parent-tampering exploit. |
| f756d09d/r57, native protected reaper | Exit0,1passed/0failed,4.26s. Actor is separate from protected namespace init. |
| eb470a119/r59, initially actor-only one-task assumption | Exit101,0passed/1failed,3.30s. Startup refused EAGAIN/125. |
| c9d0a46f plus retained test-only diff/r60 | Exit101,0passed/1failed,2.13s; same EAGAIN/125. Existing MCP tasks already share the domain budget; this did not establish a reaper regression. |
| 5382f061a/r64, corrected identity and refusal tests | Exit0,2passed/0failed/1ignored helper,2.23s. Small-budget refusal writes no actor report and never widens the grant. |

Latest raw identity facts:

```text
actor_pid=2
actor_ppid=1
namespace_init_owner=0
namespace_init_signal_errno=1
namespace_init_memory_errno=13
namespace_init_descriptors_errno=13
```

The retained control patch changes only the fixture on the old launch source.
Neither this identity test nor errno denial proves every tamper/descendant/cleanup
case. CLOEXEC launch-status EOF is still not proof that exec actually happened if
the actor died before exec. Helper/parent/kernel cost remains to be measured.

## Native network probe correctness, outside a VM

The exact guest probe module ran as a native uid1000 Fedora process. The committed
native-cases.py reconstructs the commands and payloads. Summaries here retain case
outcomes, elapsed time, output hashes and full remote manifest hashes/paths; full
commands and stdout/stderr remain under ~/managed-network-native-check/native-red
and native-green. There is no VM, interception, fairness or performance claim.

At88a20dc4,27cases yielded19passed/8failed: six zero-deadline cases still opened
listeners, and both oversized UDP sends left the paired receiver waiting until the
120000ms parent guard. At48e16c053/r58,27passed/0failed;5unit tests and strict
clippy passed. The fix rejects zero before resource setup and wakes the owned
receiver's poll with an eventfd when its sender fails. Primary-source rationale,
protocol-derived datagram bounds and repeat commands are in the sibling
managed-network measurement record. Independent guard-removal mutations are still
owed; the old-to-new comparison does not replace them.

## R01–R16 evidence account and limits

| Rules | Current proof and remaining obligation |
| --- | --- |
| R01 | Focused fixture/probe source uses checked/fallible new control paths; scoped clippy results linked above. Full combined implicit panic/allocation review remains root-owned and open. |
| R02/R03 | Existing lifecycle/network primary-source comparisons and protocol-bound derivations are in their measurement/D59 records. These observations validate only the tested mechanism; they do not become picked product constants. |
| R04/R05 | Owned lifecycle states, explicit quarantine/control faults and channel codecs are implementation checkpoints. Their full composed state/cancel/resource review remains open. |
| R06/R07/R13 | Root reviewed the isolated lifecycle and native probe source; no code-size threshold substitutes for control-flow/ownership review. Further endpoint/capture composition review remains required. |
| R08/R14 | No4–5x, low-overhead, allocation-free or leak-free claim. Paired pinned competitor comparisons, distributions, allocations/copies/faults and parent/kernel accounting are unrun. |
| R09/R10 | Named real baseline/fixed runs and retained logs establish the exact listed symptoms. The negative task-budget control prevented an incorrect production change. Independent mutations and broader forbidden-effect proofs remain open. |
| R11/R12 | Linux amd64 KVM and native observations only; shared guest implementation uses existing seams. Seven-target and other KVM/HVF/WHP combinations remain unverified. No Intel Mac support or paused WindowsARM VM use. |
| R15 | Coordinator performed all live VM/campaign work; three acknowledged implementer lanes have light-only scope. No feature completeness or exact-tip combined pass inferred from earlier results. |
| R16 | Reclaimed only confirmed inactive regenerable compiler cache, preserving source, credentials, attachments, experiments and evidence. Mac floor30GiB remains binding. |

Current next gates include actual mixed-tenant quarantine/whole-boundary mutation,
real authenticated TLS observer/cancel/identity races, independent stdout/stderr
credit, stable declared IDs with files-only/symlinked configs, and full aggregate
resource authority. Existing aggregate memory victim policy/G12 and the full
checkpoint/restore cut/P11 remain unresolved. No phase or Step10 is marked met.
