# Owner-output resource fixture — TEST ONLY, execution owed

Base combined source: `c7e87dc506abfe73a7839a32f8ecc514993ab63c`. This adds one
ignored real-VM fixture, `cost::owner_output_resources_at_explicit_member_count`,
to `crates/shards/tests/managed_output.rs` via private `fixture.rs`. Implementer
work includes source, pure parser tests and compilation where the light envelope permits. **No VM, resource
measurement, profile, comparison, mutation, audit or campaign is run by the agent.**
The current 2N owner endpoint/listener/thread topology remains experimental and
unadopted; this fixture supplies evidence for the coordinator's choice.

The later, unadopted owned-host exit-watch extension from combined95e3b55e is
documented in [HOST-EXIT.md](HOST-EXIT.md). Its changed caller and every live
handle/FD/deadline proof remain coordinator-owed; earlier results below do not
validate that extension.

## Explicit caller contract

All three environment inputs are mandatory; there is no count/default/max choice:

| Input | Meaning |
| --- | --- |
| `SHARDS_OUTPUT_COST_MEMBERS` | Decimal active member count N, including zero; checked declaration-ID and 2N representability. The coordinator must choose a finite count inside actual admission. |
| `SHARDS_OUTPUT_COST_RECORDS` | Absolute **new**, private record directory with an existing parent. Existing records are not overwritten. |
| `SHARDS_OUTPUT_COST_DEADLINE_MS` | Positive, representable protocol-phase deadline supplied by caller; cleanup is a separate phase using the same explicit duration. |

At build time the coordinator may set `SHARDS_OUTPUT_COST_SOURCE_REVISION` to the
actual frozen source revision. Absence remains null; it is never guessed. Actual
fixture/CLI/testguest/kernel/init binary hashes are retained before the ready
checkpoint. Use the normal real test build with real carried helpers/init/server;
the pure parser light command below does not build that caller artifact.
The existing common kernel override is `SHARDS_TEST_KERNEL`, not `SHARDS_KERNEL`.

Coordinator invocation after recording exact source/binary provenance and actual
VM admission (shell variables here are caller-selected inputs, not defaults):

```sh
SHARDS_REQUIRE_VMS=1 \
SHARDS_OUTPUT_COST_MEMBERS="$members" \
SHARDS_OUTPUT_COST_RECORDS="$records" \
SHARDS_OUTPUT_COST_DEADLINE_MS="$deadline_ms" \
"$managed_output_test_binary" --exact \
  cost::owner_output_resources_at_explicit_member_count --ignored --nocapture
```

## One actual workload, one readiness checkpoint

The fixture uses `common`'s real build/run path in a fresh private home/context.
Local verified AGENT/HARNESS directories split N between the two kinds (the odd
member is an AGENT); each contains the existing testguest and its native manifest.
IDs follow the existing `agentfile.rs::managed_plan` normalization: all AGENT
declarations precede HARNESS declarations. A files-only AGENT at
declaration0 leaves a real hole, then runnable IDs1..N. No RUN/VOLUME installation.
Per-member labels and payload tokens are passed directly to the existing
`Observer` APIs, including for N>2; the old `declared_label` remains unchanged.
The existing exact binary payload, scope, offset, decoder, TLS pinning, ACK and
control path are reused. All 2N observers are established before consuming data.

For N>0, every channel's exact payload is received; member1 stdout's final record
stays unacknowledged, while other channels' final ACKs are sent and their
connections stay open. Main exact stdout/stderr progress succeeds. `ready.json`
then publishes **OwnerChannelsReady** once; metadata retains actual scoped
endpoint addresses and their unique count. ACK-sent is not yet ACK-consumed
proof. After the snapshot, the held observer is canceled/joined, the other
channels' duplicate final ACK + FIN is confirmed through the existing typed
Closed result, and main progress succeeds again. Duplicate consumed offset is
already legal (`remote/src/managed.rs::consumed`, Ready branch and its test).

N0 is explicitly **NoManagedMembers**, not an ambiguous absent metadata value:
the files-only image must run, have no managed-output metadata, and demonstrate
main exact progress. It never uses metadata/member-required observer methods.
This readiness contract differs from N>0 and must be reported as such in baselines.
Source feasibility comes from `daemon.rs` filtering empty active-members before
managed capture; an actual zero-member VM result is still owed.

## Fixed one-shot observations, no collector loop

Daemon identity comes from the existing `shards_ipc::Named` daemon.pid record;
the actual VM **host** PID comes from inspect `.State.Pid` and existing
`shards_ipc::started`. The observer/test parent is included separately. Each
snapshot opens that task's proc directory, checks PID/start before retaining
resource fields, then checks again after. An unknown/changed/read-failed identity
is explicit. The pinned open directory prevents subsequent reads being redirected
to a newly reused PID; it does not make the samples atomic or preserve a live task.

Only status/stat/smaps_rollup/cgroup, FD numbers/types and owned socket inodes are
sampled, once at readiness. There is no environ/cmdline/key-file read. Ordinary
FD pathnames are omitted. Namespace-wide udp/udp6 tables are filtered **before
retention** to rows matching sampled owned FD socket inodes; unrelated host
connections are never logged. Read errors retain typed kind/raw OS code;
incomplete enumeration stays Partial/Unavailable, and known inode/row counts
are not advertised as complete concurrent socket counts. Public owner/server
fingerprints and published endpoint addresses are retained, never private keys.

Primary format/uncertainty evidence:
[kernel proc documentation](https://docs.kernel.org/filesystems/proc.html),
[proc_pid_fd(5)](https://man7.org/linux/man-pages/man5/proc_pid_fd.5.html),
[proc_pid_stat(5)](https://man7.org/linux/man-pages/man5/proc_pid_stat.5.html),
[Linux v6.18 UDPv4 producer](https://github.com/torvalds/linux/blob/v6.18/net/ipv4/udp.c),
[UDPv6 producer](https://github.com/torvalds/linux/blob/v6.18/net/ipv6/udp.c).
The proc namespace tables are not process-exclusive; inode association is
required. smaps_rollup is mapping accounting, not proof of isolated kernel socket
charges. Proc membership alone leaves cgroup kernel attribution **Unverified**.
No host cgroup modification, sudo, quota invention or endpoint change is added.

Snapshot reads, serialization, file hashing and all observer drivers have parent
costs. Files/fields/processes are sampled non-atomically. These artifacts neither
subtract that overhead nor establish an allocation count, retained-byte budget,
performance ratio, throughput, distribution, leak freedom or hard charge proof.

## Shared helper change and teardown

Only the existing TEST helper deadline/command parameters are refactored:
`Observer::{start,exact,refused,ack_and_finish,join}` and the Fixture
command/observe/main-progress/shutdown callers receive explicit bounds. Existing
tests still pass WITHIN at their prior operations and preserve all exact byte,
scope, pin, refusal, cancellation, progress, terminal and FD assertions. There is
one Client/Driver/parser/validator path, no duplicate implementation or default
wrapper. The new protocol uses one immutable absolute deadline. Shutdown uses a
separately labeled finite cleanup phase and the existing owned stop/join path,
even after a protocol panic. No discovered-PID kill or periodic resource probe.

Teardown records private owner stop, observer joins/FD equality, and for N>0 exact
accepted/consumed terminal cursors with tail unknown. N0 has its own explicit
terminal state. Daemon/VM host exit readiness, complete population reaping and
kernel resource return remain **Unverified**; stop/FD success does not prove them.

## Light commands and owed gates

`bash docs/research/measurements/managed-output-cost/run-light.sh v1` is the agent
light gate: NEW private `~/managed-output-cost-target`, 4CPU/1GiB/2GiB admission,
synthetic rustc parser tests only. The actual-source host caller compile
`SHARDS_HELPERS=skip cargo check -p shards --test managed_output --offline --locked -j4`
is **owed to the coordinator**: `shards/build.rs` still requires actual init/server
artifacts or nested guest builds outside this scoped light step. No placeholder
init/server file is used and no placeholder compile began. This light run does
not build a runnable product or constitute a real-VM/matrix gate. Source,
toolchain, exact commands, logs and original exit codes are retained under
`~/managed-output-cost-light/<phase>`. The full caller compile is explicitly unrun.

### Actual light result

Exact SOURCE `0fb2f849bf07f1a9e74bfde5af4c434d71ad30a2`, Fedora GNU amd64 r238:
`rustc --test -D warnings` succeeded and all **3 synthetic parser tests passed**,
0 failed/ignored. One admitted 4CPU/1GiB/2GiB request; no retries, placeholders,
integration caller build, VM, proc/resource sampling or profiling execution.
Actual pinned compiler Rust1.98.1 (`48a229ceaefd4985c50990b14116b6d856af0985`),
host Linux6.19.10-300.fc44 amd64. Pure parser executable SHA256
`04e3723eeef9a09401951406e7451add67ed3994ac34de2ad873ed02b20d11f1`, retained
at `~/managed-output-cost-target/parser-tests-v1`. This is **not** the real-VM
integration executable.

[Raw commands/exits/logs](records/native-v1), [job](records/native-v1-job.json),
[transfer](records/native-v1-transfer.json), [verification](records/native-v1-verification.json)
and [manifest](records/native-v1-manifest.json) retain exact provenance. Original
1559-byte archive SHA256
`e6f56693c7246e71845ce321b0bf0ff1c813ae07ebab317813298afb5ceef961`;
14 raw files and 5 source hashes verified against that source's Git blobs. Native
checkout status was clean. Only the approved test file and this measurement
directory differ from c7e87dc; production source/dependencies are unchanged.
Local formatter, shell syntax and policy proofs are recorded separately in
[local-checks.json](records/local-checks.json).
Staged whitespace checking reports one new blank EOF line in the original
libtest `parser.log`; it is retained byte-for-byte, not rewritten to manufacture
a clean raw-log check. Rust source formatting, shell syntax and policy checks0;
no source changed after the passing parser run.

Coordinator owed: source review, real N0/N1/N2 and explicitly admitted finite
higher counts, deadline/error/cleanup paths, all prior tests after shared-helper
refactor, exact source/binary/kernel identities, repeated/interleaved equivalent
baselines, parent/kernel/cgroup costs, distributions/uncertainty, independent
guards/restores, full supported platform/VM matrix and final topology/adoption.
Cold startup and sustained steady-state effects need coordinator methodology;
this one-shot finite payload is not itself a complete performance campaign.

Policy receipt: full canonical main AGENTS/R01–R16/evidence/CLAUDE/managed plan/
refreshed handoff; implementer at f5cec365d2e435afae5e6f613c92d9d3759ef60f,
rules SHA256 d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
R01/R04/R06/R07/R13: test-only explicit states, typed observation failures,
small existing seams, no production panic claim. R02/R03/R05/R12: primary formats,
actual existing consumers/shared path, caller inputs, no production literal,
cache, polling, architecture choice or dependency. R08/R14: root owns every cost
execution/comparison. R09/R10: no new product fix or guessed result; light/raw and
root real-proof obligations retained. R11: Linux-specific proc fixture; seven
targets and actual VM support remain root gates. R15: signed named-path source/
evidence, no children/main merge. R16: private target, no root/frozen artifacts or
cleanup, Mac floor preserved with no local build. Historical API timing and
owned macOS Mutex OOM reds remain unchanged.
