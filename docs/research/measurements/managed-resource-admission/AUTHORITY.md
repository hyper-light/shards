# P0 authority trace: caps are not guaranteed allocations

Current landed cap source and later musl proof are in MUSL-0E.md; the earlier
source/check chronology below is historical. ROOT owns the new established-standard
OTel export contract/integration and live collectors/profiles. OTEL-SOURCE-FACTS.md
supplies existing stats/cap boundaries and seven pure parser cases, with no new
external API, exporter, sensor/cache or checked owner-claim producer. The missing
guaranteed-owner authority remains unchanged.

Source baseline `cbd39284937d024640c56db6f8d8ef2bd9b0ee24`; current resource
implementation `5f131376ec43681ff50d32b9c1ad0a9f3ec6adf7`. This is the concrete
parent-capability interface prototype and source-contract data returned to ROOT.
It adds no public flag, manifest field, production guarantee hook or unused ledger.
ROOT owns the checked owner-policy producer and P0 normalization decision.

## Existing public contract and actual producers

| Authority source | What it actually supplies | What it does not establish |
| --- | --- | --- |
| [AGENTFILE_ARCH.md:885–889](https://github.com/hyper-light/shards/blob/cbd39284937d024640c56db6f8d8ef2bd9b0ee24/docs/architecture/AGENTFILE_ARCH.md#L885-L889) | `--processes` cap/no-spawn semantics; otherwise the VM's limit until a measured default exists. | A reserved number of runnable tasks or an extra helper allowance. |
| [AGENTFILE_ARCH.md:921–923](https://github.com/hyper-light/shards/blob/cbd39284937d024640c56db6f8d8ef2bd9b0ee24/docs/architecture/AGENTFILE_ARCH.md#L921-L923) | `asks.memory` is “a limit it takes on itself, scratch included”, under D59's aggregate policy. | An artifact-created owner grant or reserved `memory.min`. |
| `init/src/domains.rs::domain`, lines468–489 at baseline | Existing declared/asked task cap and asked memory cap; absence remains distinct from zero. | A new guarantee mode, issuer or protected control share. |
| `shards/src/resources.rs::cgroup`, lines380–406 | Actual workload `pids.max`, `memory.max` (or `memory.high` with oom-kill-disable), and `memory_reservation` translated to `memory.low`. | Hard guaranteed reservation from the option's name. |
| `shards/src/daemon.rs`, lines7400–7424; `vm_run.rs::parse_run` | VM boot/hotplug maxima; checked user maximum versus boot memory. | Exclusive backing reserved against other host consumers. |
| `shards/src/daemon/budget.rs::{room,held_at,Holding::room_with}` | Current hierarchical high/max/stat/current/mapped/reclaim headroom observations. | An atomic, exclusive owner-allocation lease. |
| `abi/src/run.rs::builtin::ROOM`, lines663–669; `init/src/{run,plug}::hold_within` | Existing signed room/hold/reclaim control. | A typed guarantee grant or authority to promise those bytes to tenants. |
| D59 part nine; checked admission `workload_headroom` | Existing primary workload cap-minus-current subtraction for startup pressure policy; required reads now fail explicitly. | A physical reservation, effective min, or coherent simultaneous parent/child snapshot. |

ROOT explicitly confirmed there is currently **no established typed guaranteed
owner-claim producer**. This absence is a contract fact, not an inferred zero-byte
grant. Existing caps keep their current meaning and may overlap; their sum is not
rejected merely because it exceeds a parent cap. D59 victim selection is unchanged.

## Minimal parent capability prototype, tied to the real caller

The actual cap caller already has this shape, implemented without a new ledger:

```text
Parent::prepare(existing_D59_pressure)
    -> held run/domain objects, checked required controllers/readback
Parent::prepare_child(existing_member_name)
    -> Prepared (new unused group)
Prepared::commit(existing_pids_cap, existing_memory_cap)
    -> Committed | typed Refusal
Committed::group() -> borrowed held object for ROOT's Programs/RUN integration
Committed::into_file() -> existing lifecycle owner after successful clone
```

The next parent-capability input must distinguish these facts explicitly:

```text
held_run_object, held_primary_workload_object, held_member_object
observed_hard_caps = Known(Unbounded | Limit(value)) | Unavailable(cause)
existing_primary_pressure_remainder = Known(value) | Unavailable(cause)
owner_guarantee_authority = Unavailable(NoCheckedOwnerClaimProducer)
control_and_induced_parent_charge = OwnedActualResources | Unavailable(cause)
```

These are a data/interface prototype for ROOT's real allocation/worker callers,
not a production constructor that can mint authority from supplied numbers. Held
objects identify the controlled scope; they do not make observations immutable or
turn a resource cap into an owner allocation. Reported labels/tool IDs never select
the authority. Missing observations are not `max`, zero, or successful reservation.

For a future strict request, the missing producer must yield typed
`Unsupported(NoCheckedOwnerClaimProducer)` before release. A present producer
would still need authoritative parent bounds, support/epoch checks, actual control
requirements and typed `Unadmitted` on insufficient admitted capacity. It cannot
be built from the current cap request, ROOM estimate or a hardcoded percentage.
No strict-request producer or alternate string selector is introduced here.

The immediate concrete integration remains `domains::start` followed by ROOT's
actual server/Programs/RUN admission and existing `managed::Launch`. Control
objects already acquired before clone include the held population/kill objects
and launch's real protocol endpoints. That acquisition is fallible resource
preparation, not proof that all induced parent/kernel memory or future control
work has a protected budget. Server/runtime tasks consume the same task cap;
existing first-runs evidence disproves an actor-only task assumption. No extra
task or byte allowance is invented.

## Why min/low and observed free space do not fill the missing authority

The pinned Linux [cgroup v2 contract](https://raw.githubusercontent.com/torvalds/linux/refs/tags/v6.18/Documentation/admin-guide/cgroup-v2.rst)
distinguishes limits, protections and allocations. Min protection depends on
ancestors and overcommit; low is best effort. High is a pressure threshold.
ROOT must decide which checked owner guarantees exist before configuring new
protections, reclaim priorities or victim rules. A readable maximum/free/current
number alone cannot authorize one. The exact pinned implementation and systemd/
runc alternatives, source hashes and known readback-rounding caveat are in README
and research records. No new min/low writes or policy change is made.

## Actual claim data and remaining work

`records/authority-source-facts.json` records the existing fields and explicit
missing producer. Its runtime parent cap, min/low, reserved/available and induced
charge values are **unmeasured/null**, not zero. No VM/profile/actual guarantee
data is fabricated. Full P0/P1/P2/P12/P13 resource obligations remain open.

The new source's nine private-file/pure cases and isolated Clippy pass, and actual
init binary-target typecheck0, validate only their named scopes. The real init
check exposed one obsolete unowned `run::workload_headroom` helper after moving
the sole domain read to checked admission; ROOT was asked to remove it or assign
that exact site. Actual caller Clippy, linking/matching helper builds, kernel
cap/rounding/rollback, control-survival, tenant/execution/flow charges, actual
disposable-guest correctness/mutations, supported-target gates and all profiling/
benchmarking remain owed. None is replaced by the interface prototype.
