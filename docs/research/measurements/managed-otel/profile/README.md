# Coordinator encoding comparison

This measures only the exact three-Snapshot edge corpus and its complete request
construction/encoding into a counted `Write`. It excludes cgroup reads, existing
builtin output buffering, syscalls/HTTP, SDK readers/export pipelines, VMs,
parent/kernel producer charges and admission. A full-management speed or resource
claim would exceed these measurements.

Native codec SHA-256 `0bb86b2f172e12b8053cf30333ccbabd1a6963a315836a639c79de5a4458f28c`;
official request builder `5e16b6cbf87b221b1e061690c23283197e11571b6aa0535b163bf046df07f4ca`.
The [pinned reference](../../managed-otel-rust-reference/README.md) includes its
own actual Strings, Vecs, CPU decimal conversion, generated defaults and serializer.
All three actual payloads were independently decoded with official Collector
pdata v1.71.0; the complete normalized receipts are equal. Native omission and
generated unknown start `"0"` remain untouched and semantically equivalent.

POLICY_READ: root coordinator, main33df, rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Only the coordinator performed these profiles. They do not set runtime constants.

## Method and exact builds

`run.py` profiles source `cf8f8d600414aced7b24ddfea10410f8ddafd103` on the
root-owned Fedora6.19.10 x86-64 runner. The raw identity records rustc, lock,
optimized binary/source hashes, environment and load. Admission2CPU/2GiB/4GiB
is a requested finite envelope, not a measured peak or hard cgroup assertion.
Builds use Cargo release and the default System allocator. Allocation counting
is compiled into a separate binary; its slower instrumented times are not used
for latency ratios.

Nine blocks rotate three modes through every execution position equally. Each
mode has 1000 samples per fresh process, 9000 pooled samples; no first-call or
outlier is discarded. Raw per-call monotonic nanoseconds are retained, including
the first call. Each process captures `getrusage` CPU/fault/context-switch deltas
around the loop. The sample array is allocated/initialized before measurement.
Modes construct and drop the full request/buffer on every call. A counting writer
and `black_box` retain formatting and writer-call work without a destination I/O
benchmark. This is not a language-level guarantee that allocations cannot be
optimized away. [Rust GlobalAlloc](https://doc.rust-lang.org/std/alloc/trait.GlobalAlloc.html),
[Instant](https://doc.rust-lang.org/std/time/struct.Instant.html),
[getrusage](https://man7.org/linux/man-pages/man2/getrusage.2.html).

Load averages were13.61/15.05/13.44 before and after the brief batch, on32 logical
CPUs; no isolated CPU/frequency claim. Mode-to-mode and block variance remains.
The extrema/ranges below describe these actual samples; they are not independent
population confidence intervals. More host/workload profiles remain required.

## Actual results

| Path | p50 µs | p90 µs | p99 µs | max µs | CPU over9000 calls | Minor / major faults |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Native stream | 3.775 | 4.164 | 4.410 | 55.303 | 31.213ms | 27 / 0 |
| Official compact request/buffer | 15.623 | 16.943 | 23.800 | 453.324 | 134.578ms | 122 / 0 |
| Official pretty request/buffer | 23.949 | 33.828 | 40.623 | 767.490 | 240.694ms | 140 / 0 |

Pooled p50 ratio to native: compact4.139×, pretty6.344×. Paired per-block median
ratios: compact median4.245×, range3.899–6.666×; pretty median6.200×,
range5.885–14.668×. The compact path therefore **misses4× in some blocks**.
The first-call native median is13.798µs versus49.377µs compact (3.579×, also
below4×) and73.150µs pretty. This does not establish4–5× over a full official SDK
or the whole management layer, nor a resource/throughput win under actual I/O.

Separate100-call System-allocation observations:

| Path | allocs / request | reallocs / request | initial alloc bytes / request | Peak live delta | Retained delta | Output bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Native stream | 0 | 0 | 0 | 0 | 0 | 4833 |
| Official compact | 175 | 15 | 9652 | 19127 | 0 | 6500 |
| Official pretty | 175 | 16 | 9652 | 27319 | 0 | 16108 |

Realloc requested-new-byte totals per request are19996 compact and36380 pretty;
these are not additional simultaneous live bytes or measured copy counts.
All175 allocations/request are deallocated in the observed region. Native has
535 writer calls/request versus1 buffered official call. Production already has
an output BufWriter; these counts are **not syscall counts**. This census excludes
native/C allocator internals and kernel allocations; zero observed calls is not
whole-operation allocation/leak freedom. Copy counts remain unmeasured.

All reported process `ru_maxrss` values are the same19048KiB. This is a process
high-water value across fork/exec, not an incremental encoder footprint witness;
there is no measured RSS/PSS advantage. Parent/kernel/shared-service charges
remain open. Neither the allocation table nor minor faults resolves those costs.

## Recorder correction, with the original failure retained

The first instrumented run's JSON claimed739 additional retained bytes for every
mode, even while native recorded no allocation/reallocation/deallocation at all.
Inspection confirmed that `end()` loaded LIVE after building part of its own
JSON report. This was a measurement-recorder bug, not a Shards encoder leak.

`acd0b8dafc6f91fe2ac1ad12dd94a77117399d52` captures every counter before report
construction. `run-corrected-counts.py` rebuilds the counter binary, reruns the
three100-call cases and asserts actual before/after equality. All pass; strict
all-feature Clippy passes. Timing source/binary/results remain the original
uninstrumented `cf8` run; only the retained-byte receipt changed. Both original
wrong receipts and corrected receipts are retained without rewriting. Corrected
binary SHA-256 `88205cd13af3778e180da5b0244933e90f93697fb486b89292d72dc080c9521e`.

## Storage and adoption boundary

[summary.json](summary.json) is the exact computed summary; raw per-call samples,
first/wrong and corrected counters, source/binary/lock identities, original
commands, build/Clippy logs and complete official-decoder receipts are in
`../raw-storage/profile-originals.tar.gz`:82499 bytes, SHA-256
`7d223974d328bddf15722560ba57633fbca8ddcc6bc18ea784346686a3486ccd`.
The coordinator verified118 originals and every safe unique regular member.
Frozen binaries/caches remain in the private runner directories.

This supports retaining the direct streaming codec for the initial on-demand
artifact: it is interoperable at the pinned decoder/Collector and avoids the
observed generated-object/buffer allocations for this corpus. It does not justify
new caches, periods, grants, source identities or authority. Full collection/
transport/parent/kernel costs, production delivery, larger cardinality and real
supported-host distributions remain required. See [R01–R16 review](../VALIDATION.md).
