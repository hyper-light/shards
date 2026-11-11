# Managed resource enforcement: checked producer boundary

Initial owner-input trace against `0342b85d9c808e3ced041a6c3ef0e9bf70438272`.
ROOT subsequently selected the allocation-backed capture slice in [README.md](README.md)
while retaining the missing hard-resource producer obligation. No blanket
managed-start refusal or authority/compatibility mode was implemented.
Implementer `/root/managed_resource_enforcement`; coordinator `/root` (Kiln
`01a148884a4770fdaf0111f3f792a248`). This lane runs scoped light checks only;
ROOT owns live reproduction, audits, profiling, benchmarks, kernel attribution,
combined validation and main merges. No child agents.

POLICY_READ: current origin/main revision above; `AGENTS.md`, full R01–R16,
`development-evidence.md`, `CLAUDE.md`, `managed-runtime-plan.md`, the current
`/Users/adalundhe/g121-handoff.md` and `managed-parent-resource-contract.md` read
before edits. Rules SHA-256:
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Owned source is only resource admission in `daemon/managed.rs`,
`init/domains/admission.rs` and any narrowly necessary private resource module.
This directory owns this lane's evidence. Other paths and active jobs stay with
their current owners.

## Current input trace and decision boundary

These are observations of the exact base source, not a reproduced exploit or a
claim that a complete runtime audit ran.

| Producer/caller | Actual supplied value | Admission boundary |
| --- | --- | --- |
| `ipc::Resources` (`crates/ipc/src/lib.rs:1130`) | Workload memory, reservation, task, CPU and IO settings | No checked management backing/entitlement issuer or protected control subset |
| `resources::cgroup` (`crates/shards/src/resources.rs:380`) | `memory.max`/`memory.high`, `memory.low`, `pids.max` settings | Preserves existing cap meanings; cannot manufacture rights in the daemon |
| `run::Prepared` / `sizes_for` (`run.rs:388`, `run.rs:982`) | Parent selector, VM boot/max addressability | Parent placement and VM capacity are not exclusive backing |
| `Daemon` managed caller (`daemon.rs:4016`) | Run ID, client, checked member labels/activation, setup vector | No resource authority argument reaches `managed_capture` |
| `Charge::reserve/grow` (`daemon/managed.rs:116`) | Logical aggregate plus fresh `build::host_available()` | Neither a parent reservation nor an owner entitlement |
| `remote::managed::Admitted` (`crates/remote/src/managed.rs:203`) | Retained/transport/handshake/datagram byte allowances | Its documented caller contract requires an actual host reservation, including endpoint/QUIC/kernel state |
| `admission::Prepared::commit` (`crates/init/src/domains/admission.rs:492`) | Existing declared task/memory caps and held kill/events controls | Cap verification and acquiring controls do not reserve all later parent/control work |

The owner-authority producer must originate at authenticated owner request/run
normalization before trusted transport or actor release. It must supply:

1. The affected run, owner, policy lifetime/generation, resource meaning and
   issuer. A cap, reclaim protection and exclusive backing remain distinct.
2. The finite admitted management amounts and protected stop/refusal/status/reap
   requirements, including known capture/retained/transport/structural costs,
   task stacks/threads, descriptors and actual induced parent/kernel objects.
3. A parent authorized and able to honor them, including competing obligations
   and the primary workload; authority must serialize reserve/commit/release,
   transfer, shrink/revocation and uncertainty cleanup.
4. Explicit refusal when the actual host/guest mechanism cannot establish the
   requested resource contract. Available-memory samples, caps and artifact
   asks cannot fill any missing authority field.

No current producer supplies these facts. This is a required input gap, not an
optional limit or a reason to invent an equal split, percentage, global default,
extra helper allowance or second resource manager.

## Existing authoritative progress and allocation boundaries

The existing `Credits` queue and `remote::managed::Shared` retain progress and
credit authority. A resource change must use their existing notifications and
ownership; it must not add polling or duplicate those states.

The current arithmetic includes initial two overlapping record allowances,
transport, handshake, control/datagrams, `Output`, declared-channel vectors and
copied labels, followed by observed endpoint/control and IPC socket queue sizes.
That list is not complete allocation accounting: the remote endpoint's private
`Shared`, driver/QUIC/TLS state, command queues, callback allocation, thread
stacks, `Arc` headers and allocator/kernel overhead are not established by those
`size_of` calculations. ROOT owes actual attribution before asserting a total.
Read-back socket buffer capacities after bind also do not constitute pre-effect
reservation. Learned record growth may use only established authority.

## Primary basis and established alternatives

The existing [parent contract](../../managed-parent-resource-contract.md) pins
the research and source hashes. This lane independently opened Linux commit
`7d0a66e4bb9081d75c82ec4957c50034cb0ea449`'s
[cgroup v2 contract](https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/Documentation/admin-guide/cgroup-v2.rst)
and opencontainers/cgroups commit
`a3e2ecd1f756a19cee15f85b96337a59c3b5337b`'s
[memory setter](https://raw.githubusercontent.com/opencontainers/cgroups/a3e2ecd1f756a19cee15f85b96337a59c3b5337b/fs2/memory.go)
on 2026-11-10. The kernel distinguishes overcommittable ceilings/protections
from allocations; the established setter maps reservation to `memory.low`.
Neither alternative provides an exclusive Shards management backing issuer.

Retaining observed experimental caps, adding explicit owner-authorized
protection, and implementing actual admitted backing have different contracts.
ROOT must choose the supported request behavior and produce its authority;
this lane cannot silently select a compatibility bypass or change D59's largest
populated member victim policy.

## Validation and remaining work

No build, test, VM or runtime reproduction has run for this new lane yet.
No performance, allocation-free, leak-free, entitlement or supported-matrix
result is claimed. macOS is arm64 only; the paused Windows ARM64 environment
and all held campaigns remain untouched. No cache/evidence was deleted.

Every R01–R16 result and owed check will be added with the actual implementation
and source revision. The full hard-resource requirement remains open until its
producer, bounded admission, protected control and induced-parent/kernel
evidence exist.
