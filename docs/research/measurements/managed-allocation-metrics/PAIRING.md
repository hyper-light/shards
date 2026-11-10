# Serial paired cost runner — implementer has not executed it

`paired.py` launches only the existing Linux `managed_output` cost test. ROOT
alone executes it after source/provenance review, full normal-helper compilation
and actual VM/resource admission. This lane runs `test_paired.py` synthetic unit
tests only. No benchmark, profile, actual case or performance result is supplied.

The caller must supply positive **n pairs**, active **N members** (including0),
fixture phase deadline, external-process timeout, two exact manifests and a new
absolute record directory. No default n, warmup, automatic retry, count sweep,
overhead allowance or concurrent cases is selected. The order alternates AB/BA;
odd n has one extra AB pair, explicitly reported as requested order. Nothing is
inferred from a single pair or from different N values/readiness contracts.

## Manifest contract

Each JSON manifest has these mandatory fields; all placeholders must be replaced.

| Field | Required value |
| --- | --- |
| `schema` | `managed-output-cost-pair-manifest-v1` |
| `build_profile` | `release`, required by CLAUDE.md for measurements; root independently reviews the build receipt |
| `variant` | A distinct simple label for each variant |
| `source.directory` | Absolute frozen source directory on the execution host |
| `source.revision`, `source.tree` | Full lowercase40-hex Git commit and tree IDs |
| `artifacts.fixture`, `.cli`, `.testguest`, `.kernel`, `.init` | Each an absolute `path` and exact SHA-256 `sha256`; same five roles the actual fixture hashes |
| `artifacts.build_record` | Absolute `path` + SHA-256 of root's retained actual source-matching normal-helper build receipt/record |
| `artifacts.workload_record` | Absolute `path` + SHA-256 of root's reviewed workload/ABI-equivalence descriptor |
| `test_guests` | Absolute prebuilt root; testguest must be exactly `guest/shards-testguest` beneath it |
| `environment` | Explicit public string values only: PATH, HOME, optional TMPDIR/LANG/LC_ALL/TZ; PATH/HOME mandatory |

The build receipt must identify actual compiler/linker/formatter versions,
release command/flags, source and helper/ABI association. An MSRV field does not
select those tools. Root must rebuild host and guest together when ABI identity
changes. The runner verifies the receipt's bytes/hash but does not establish
build history or semantic equivalence by trusting a label.

Both variants require identical public environment, workload-record hash and
kernel hash. Different build-coupled testguest/init/CLI hashes remain explicit;
root's reviewed descriptor must establish equivalent actual workload and
isolation despite those changes. A manifest is evidence input, not semantic or
grant authority. The strict actual fixture retains its own byte/scope/control
checks; root still owes relevant mutations and real execution.

`SHARDS_TEST_GUESTS` selects the existing prebuilt guest path and avoids nested
guest builds in a timed case. The fixture's build-embedded init/CLI must match
the manifest at execution. Copies may have different absolute paths, but all
actual `inputs.json` hashes and compiled source-revision field must match.
Root compiles with its actual source identity supplied through the existing
`SHARDS_OUTPUT_COST_SOURCE_REVISION` build-time option. Null is a refusal.

## Root invocation and recorded facts

```sh
python3 docs/research/measurements/managed-allocation-metrics/paired.py \
  --a "$manifest_a" --b "$manifest_b" --n "$pairs" --members "$members" \
  --phase-deadline-ms "$phase_ms" --case-timeout-ms "$process_ms" \
  --records "$new_records"
```

Python3.11+ is needed for the standard streaming `hashlib.file_digest`. The
runner never builds, changes source, restores a process by numeric PID, signals
daemon/VM identities, resumes a VM or cleans an existing record directory.
Only root invokes it. Its child receives the exact supplied public environment
plus mandatory VM enforcement, the selected prebuilt kernel/testguest and
same-N cost inputs. Other inherited environment is not silently included.

Before and after each case, the runner reads the declared source HEAD/tree,
requires tracked-clean state and checks all manifest file digests. Untracked
build state, past compiler actions, atomic binary immutability and environmental
equivalence beyond recorded fields are not proven by these checks. Root owns
frozen artifacts and source/cache exclusion throughout the campaign.

A successful fixture exit is insufficient. Before the next case the runner
checks actual source/artifact/N/deadline inputs; distinct N0/N>0 readiness and
terminal contracts; observer, daemon and VM PID/start before/after identities;
held watch readiness/identities; both valid exit events; protocol/owned stop and
join; exact baseline + actual Linux exit objects = acquired, then released =
baseline; main progress, declaration mapping and terminal consumed cursors.
These are retained fixture witnesses, not independent kernel measurements or a
proof of full-population reaping, exit causality, kernel-charge return or leaks.

Each external Git command and fixture child uses the caller's timeout. Plain
file I/O remains synchronous and is not interruptibly deadline-bounded. An outer
fixture timeout/interrupt leaves daemon/VM cleanup **Unknown** and stops the
schedule; Python child kill/wait is not a daemon/VM cleanup witness. No next
sample, blind retry or process-discovery cleanup hides that uncertainty.
[Python subprocess contract](https://docs.python.org/3/library/subprocess.html).

Original manifests, selected commands/environment, stdout/stderr, all fixture
records and a per-case hash list are retained. Each JSONL outcome is flushed
before another sample, with an immutable progress record; failure preserves a
stopped record. No automatic resume/record overwrite is supported. Source/file
preflight failure, malformed records and uncertain identity/cleanup stop further
cases. Full quantiles/paired summaries are computed at stop/completion, retaining
raw values throughout instead of repeatedly summarizing the growing history.
Out-of-space/uninterruptible host I/O still requires root reconciliation;
this harness does not promise durable recording through every host failure.

## Metrics and uncertainty

Per variant and paired signed B−A differences retain n requested, known,
conditional, unknown and missing, with p50/p90/p99/max of the available values.
Unknown values/reasons are not converted to zero or removed from coverage.
Quantiles use type1/inverse empirical CDF: one-based rank `ceil(p*n)`, explicitly
named. These are descriptive sample quantiles, not confidence intervals or
guarantees about population tails. [R Core definition](https://stat.ethz.ch/R-manual/R-devel/library/stats/html/quantile.html),
[NIST alternatives](https://www.itl.nist.gov/div898/handbook/prc/section2/prc262.htm).

Child elapsed nanoseconds use `perf_counter_ns`; its clock metadata is retained.
The separate through-validation duration includes root-runner pre/post checks;
neither metric is VM boot latency alone. Provenance hashing, process launch,
fixture records and native probes have costs; nothing subtracts or claims them
free. [Python clock contract](https://docs.python.org/3/library/time.html#time.perf_counter_ns).

Observer allocation metrics are requested Rust wrapper bytes/events, with
non-atomic phase deltas for cumulative counters and absolute live/lifetime peak
gauges. Peaks are never subtracted as phase peaks. Final live values can include
other test/runtime/input/result owners and do not establish leak freedom.
Native fault deltas remain conditional, tied to the native Linux aggregate,
width and actual role identity; missing/invalid coverage is Unknown. No native
fault categories are equated across operating systems. This Linux cost caller
does not establish macOS/Windows host results.

Raw proc observations remain available for separate root analysis. CPU/copies,
foreign C/direct-System allocators, daemon/VM/guest allocations, kernel/parent
cost and full process-tree attribution are not completed by the observer Rust
counter or this runner. Root still owes distributions/noise/contention, actual
paired execution, instrumentation perturbation, source-matching helpers, native
platform and independent mutation/restoration/cleanup gates before any measured
optimization or performance claim. No benchmark number becomes runtime policy.
