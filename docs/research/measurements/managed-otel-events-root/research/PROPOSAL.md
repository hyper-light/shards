# G25: configured cgroup memory/task event counters

Status: ROOT approved this exact selection/Snapshot/codec contract, received as
the user-delivered GO and acknowledged with
`managed-cgroup-events-root-go-received-v1`/seq7297. Implementation and pure light
validation are in progress. This is not completion of G25. ROOT retains host
caller fixtures and all actual consumer/kernel/VM/matrix/cost gates. No profile
executable is invoked by this lane.

POLICY_READ implementer01a1c5be192972d8b05dc7c207cc6fca, current main
0342b85d9c808e3ced041a6c3ef0e9bf70438272; complete AGENTS/R01–R16/evidence/
CLAUDE/managed plan/updated handoff and required0ae read. Rules SHA256
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Branch source parent81632cc92fcaa7b11d3844e0309dcc94fa397345, raw-only follow-up
ed9ac9e071fa31a9d052adc417b68ebb79160831; current ROOT4299285281a0b588ed04b854842c1c14b0950260
Reader/codec/tests are independently byte-identical to c298. Source hashes,
immutable upstream revisions and downloaded file identities are in
[primary-sources.json](records/primary-sources.json). The af7 owner-authority
research is already on main; parent issuer01a1c767840171d5872a1673e3e425f4 owns
that separate mechanism. This proposal adds no admission authority.

## Selected source contract

Read exactly `memory.events` and `pids.events` once each through the existing
held-directory fixed-name openat/tokenizer and one fallibly allocated reused
actual-page buffer. Select the complete six memory keys rendered by pinned
Linux6.18.48 plus the one task key. They are independent cumulative kernel
counter streams, not a combined event total, process inventory or causal actor
attribution. [Memory renderer][memory-source], [task renderer][pids-source].

Both base files are defined only for non-root cgroups (`CFTYPE_NOT_ON_ROOT`).
An actually attempted root/unavailable source can therefore remain Io(ENOENT),
with omitted counters and separate event diagnostics. This is source-level
availability evidence, not a diagnosis of the prior non-root81632 ENOENT or a
runtime probe of ROOT's current run group. No alternate group/fallback is selected.
[Kernel controller contract][kernel-doc], [memory cftype][memory-cftype],
[task cftype][pids-cftype].

| File | Fixed event values | Meaning of the selected key |
| --- | --- | --- |
| memory.events | low, high, max, oom, oom_kill, oom_group_kill | Respectively low-boundary reclaim, high-boundary throttle/reclaim, max-boundary encounters, OOM situations, killed-process occurrences and group-OOM occurrences. Different kinds must not be summed into one OOM/refusal total. |
| pids.events | max | The kernel-configured pids.max enforcement counter. It is not a count of all failed process operations, leaders or admitted task rights. |

Aggregation is **as configured by the kernel for these files**. Default base-file
behavior includes subtree events. `memory_localevents` changes memory.events to
local updates. For pids, `pids_localevents` selects the local FORKFAIL array in
BOTH base and .local files; without that option .local selects enforcement at
the local limiting boundary, while the base file reads the propagated MAX array.
Consequently, choosing `.local` alone does not preserve one universal definition
of fork-origin failures. [Controller/mount contract][kernel-doc],
[actual memory propagation][memory-header], [actual pids selection][pids-source].

The proposal exports the requested base-file observations and explicitly
documents configured aggregation. It adds no unconditional local/subtree label,
no mount-option probe, no `.local` fallback and no ancestor walk. ROOT's current
`run.rs::cgroups`1066–1088 mounts nsdelegate[,favordynmods], without either
localevents option. That is source configuration evidence, not a new runtime
validation of every native mount. ROOT can instead approve additional .local
streams; that alternative increases two reads/seven points to four/fourteen and
requires a separately reviewed source-mode contract.

## Proposed OTLP representation

| Instrument | Data | Unit | Fixed point attribute |
| --- | --- | --- | --- |
| shards.cgroup.memory.events | cumulative monotonic int Sum | {event} | shards.cgroup.event = one of the six selected memory keys |
| shards.cgroup.task.events | cumulative monotonic int Sum | {event} | shards.cgroup.event = max |

These are custom cgroup instruments. Pinned semconv1.45 container metrics have
no matching event family; system/process observations and container identity
would misstate this source. Names/units follow its naming/count guidance.
[Container instruments][semconv-container], [metric conventions][semconv-metrics].

Use existing cumulative temporality2/isMonotonic=true and original Snapshot
timestamp. Retain raw observed nonnegative counts, including genuine zero.
Parse checked u64, then require exact nonnegative int64 for OTLP as_int.
Unrepresentable positive values are omitted with existing Failure::Overflow;
never cast to floating point, wrap, clamp or default them to zero. Pids uses a
signed64 renderer; a negative text token remains invalid input, not an assumed
wrapped value to reinterpret. Memory uses atomic_long/%lu; supported64-bit guest
architectures do not prove counters never wrap. [Kernel storage/format][memory-header],
[pids format][pids-source], [NumberDataPoint signed field][metrics-proto].

Start origin stays unknown: native omits startTimeUnixNano; generated reference
keeps its valid default0. No first/reset time, delta, rate or lifetime continuity
is invented. Cgroup recreation, restoration, counter wrap or mount-option changes
(especially pids' backing-array selection) may break continuity. Preserve the
existing unsigned VMM-lineage string and canonical run/provider/member identity;
no management epoch/cache is introduced. Exact reset/rate/restore proof remains
ROOT/P11 work. [OTLP timestamps][metrics-proto], [resets and gaps][otel-model].

Failure handling stays per selected observation:

- Missing selected key → Missing; malformed/negative/missing value or duplicate
  selected line → Invalid; numeric/token-width overflow → Overflow.
- Independently valid known fields survive another selected field's parse error.
  Unknown future keys/values, including tokens longer than a page, are skipped
  through their line and cannot consume/hide a following known key.
- A file open/read failure maps its actual typed Io(errno) to that file's
  selected fields. The resulting error count is failed observations, not a claim
  that six separate memory file reads failed.
- Omit failed numeric points and extend the existing diagnostics/count. Error
  detail points also carry the fixed event attribute so six same-cause failures
  cannot create identical metric/attribute/timestamp identities. No new stale
  marker, synthetic zero, unbounded branch or deletion guarantee is added.

The source-derived bound is seven new numeric points and zero through seven new
error-detail points per attempted scope; existing six limit-state points remain
unchanged. Two fixed numeric descriptors cover the families. Unknown kernel
strings never become labels. These are schema/cardinality bounds, not a measured
cost or an overhead allowance.

## Established implementation comparison

- cAdvisor0.60.6/5bf5d43ac6f60d7ee36a13b4d5b4a78ad2d0abc7 reads memory.events
  high/max separately, exports distinct Prometheus Counters and converts to
  float64. Its per-key read errors leave zero-initialized fields; that cannot
  preserve the required Shards unknown/error distinction. [Actual reader][cadvisor-reader],
  [actual exporter][cadvisor-exporter]. The earlier inspected0.56.2 lacked this
  family; it was not treated as evidence for current behavior.
- opencontainers/cgroups0.1.0/30a2293c0b0325280ea70a86fe991ec1e87b5d00 memory
  reader is byte-identical to the previously pinned0.0.6 file and supplies no
  event observations here. Its stats/meminfo fallback is not adopted.
  [Actual memory reader][oci-memory].
- Collector contrib0.165 Docker receiver metadata describes Docker API stats,
  with no selected raw memory/pids event family. It is not a matching process or
  cgroup-source convention to rename into this slice. [Actual metadata][docker-metadata].

The native streaming codec remains the only native encoder. The official Rust
0.33.1/372ebbc74d4dba71bb9820732f57f7316b4e7a1e generated-proto reference must
construct complete equivalent requests from the SAME concrete Snapshot inputs;
its older embedded schema already supports these existing Sum/point/attribute
fields. This is encoding equivalence, not an SDK pipeline or Collector proof.

## Exact proposed data/source spans and validation

Shared pure codec type: fixed `CgroupEvent` enum with six memory variants and
TaskMax, plus `ALL`. Add only `Snapshot.events: [Result<u64,Failure>;
CgroupEvent::ALL.len()]`; ROOT must coordinate this addition with its current
host fixtures/central driver before composition.

Approved owned source spans: metrics.rs event method/Snapshot population;
NEW metrics/events.rs consuming existing Token stream; otlp.rs shared enum,
two Sum families/event attributes and diagnostics; existing Reader/codec tests;
existing encoder/reference fixture/check/equivalence; profile src/main.rs INPUT
FIELDS ONLY. No run/daemon/CLI/host fixture/ABI/issuer or production dependency.
The parser is justified by these two real files and their independent field
errors, not a generic sensor or flat-key framework.

Pure/light cases: shuffled fields, real zero, missing selected field, duplicate,
malformed/negative/value max, full u64/signed bounds, actual-page splits,
very-long unknown fields between known fields, fixed-name missing/symlink I/O,
same buffer reuse and valid fields surviving independent errors. Full official
typed request equality must cover all enum values/causes/scopes and stable
canonical identity; source arrays and counts derive from enum definitions.
Private GNU/musl2CPU/2GiB/4GiB<=600s/j2 only, normal headroom; native reads only
in an owned own-cgroup fixture. No generated pressure/forks/faults, VM, Collector,
mutation or profile execution. ROOT retains real caller/controller/restore/
target/consumer/adversarial/cost and induced-parent/kernel gates.

81632 remains frozen: live finite/max cap reads UNVERIFIED. The SAME ordinary
1CPU/256MiB/1GiB read was subsequently admitted; original53payloads+manifest are
now verified, with its before-start refusal retained. This proposal neither
reruns the case nor uses a different cgroup to manufacture a positive result. All old failures,
sources, binaries, automatic-review holds and paused WinARM remain preserved.

[kernel-doc]: https://github.com/gregkh/linux/blob/5bbb9c9f8f808710e2123f2b30f0d61d7d698f52/Documentation/admin-guide/cgroup-v2.rst
[memory-source]: https://github.com/gregkh/linux/blob/5bbb9c9f8f808710e2123f2b30f0d61d7d698f52/mm/memcontrol.c#L4465
[memory-header]: https://github.com/gregkh/linux/blob/5bbb9c9f8f808710e2123f2b30f0d61d7d698f52/include/linux/memcontrol.h#L1004
[pids-source]: https://github.com/gregkh/linux/blob/5bbb9c9f8f808710e2123f2b30f0d61d7d698f52/kernel/cgroup/pids.c#L243
[memory-cftype]: https://github.com/gregkh/linux/blob/5bbb9c9f8f808710e2123f2b30f0d61d7d698f52/mm/memcontrol.c#L4629
[pids-cftype]: https://github.com/gregkh/linux/blob/5bbb9c9f8f808710e2123f2b30f0d61d7d698f52/kernel/cgroup/pids.c#L407
[semconv-container]: https://github.com/open-telemetry/semantic-conventions/blob/fb30ca17193b551473b1f43c315482918ab6987c/docs/system/container-metrics.md
[semconv-metrics]: https://github.com/open-telemetry/semantic-conventions/blob/fb30ca17193b551473b1f43c315482918ab6987c/docs/general/metrics.md
[metrics-proto]: https://github.com/open-telemetry/opentelemetry-proto/blob/b3f75588eb23c5fca62264edd05d382de49beb1a/opentelemetry/proto/metrics/v1/metrics.proto
[otel-model]: https://github.com/open-telemetry/opentelemetry-specification/blob/18c5c1706e346fec3bdb5e135a6e3a78f461e586/specification/metrics/data-model.md#resets-and-gaps
[cadvisor-reader]: https://github.com/google/cadvisor/blob/5bf5d43ac6f60d7ee36a13b4d5b4a78ad2d0abc7/lib/container/libcontainer/handler.go#L902
[cadvisor-exporter]: https://github.com/google/cadvisor/blob/5bf5d43ac6f60d7ee36a13b4d5b4a78ad2d0abc7/lib/metrics/prometheus.go#L596
[oci-memory]: https://github.com/opencontainers/cgroups/blob/30a2293c0b0325280ea70a86fe991ec1e87b5d00/fs2/memory.go
[docker-metadata]: https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/6083b2c53d78b21d0925b703194aaed920c01af5/receiver/dockerstatsreceiver/metadata.yaml
