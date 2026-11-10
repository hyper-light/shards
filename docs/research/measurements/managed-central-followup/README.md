# Coordinator follow-up: native cleanup, local routes and reader cost

Policy receipt: coordinator, main `f5cec365d2e435afae5e6f613c92d9d3759ef60f`,
rules SHA256 `d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
These are root executions and raw observations, not completion of the managed
runtime plan. Source revisions differ below; a later tree does not inherit a pass.

## Actual gates

| Source / root run | Observed result | Raw directory |
| --- | --- | --- |
| c8d7ecfc / Linux r200 | Native cleanup5/0; actual KVM cleanup1/0,17.58s excluding build. Own8, init8, host-PID8 and host-PID72 descendants end/reap; independent neighbor answers. Deliberately held negative control times out as expected. | managed-cleanup-c8d7-v1 |
| c8d7ecfc / arm64 macOS r202 | Native cleanup5/0,1.30s excluding build. | managed-cleanup-macos-c8d7-v1 |
| c8d7ecfc / native amd64 Windows r209 | Native cleanup5/0,1.12s excluding build; shared Rust readiness artifact, no Python alias dependency. | managed-cleanup-windows-c8d7-tools-v2 |
| d5fb2b95e / Fedora KVM r221 | Four actual IPv4 TCP guests: local-loopback, local-self, local-to-peer and forwarded, each1/0. Actual backend1/relay1, distinct bytes, original tuples and joined workers. | managed-intercept-d5fb-targets-v1 |
| 3f7aeba45 / Fedora KVM r222 | Host units4/0; actual task-limit test fails **before Fill**, because its top assertion assumes an internal reaper row. No exhaustion/refusal/recovery result. | managed-limits-3f7a-v1 |
| 22ea786d8 / arm64 macOS r226 | Full supported seven-target scripts/lint exit0, including GPU feature checks. | managed-seven-target-22ea-v1 |

Native cleanup cases exercise owner drop, early exit, normal release, silent
readiness and partial readiness. Mac and Windows use SHARDS_HELPERS=skip: those
results are native client tests, not HVF/WHP VM validation. Each record names
compiler, source, child artifact hash, actual commands/output and exit status.
Windows r201 fails before tests because NASM is missing; its original log/101 is
retained separately. CI-pinned NASM3.02 and Zig0.16 were installed privately before
r209, without global PATH, security, host privilege or crypto changes. Tool hashes
and source association are retained under managed-native-tools. An intermediate
archive-extraction setup error is not evidence of a client defect.

The task fixture's actual Ready reports inner PID2/PPID1, actor UID200000,
reaper UID0, threads1 and five descriptors. Trusted top shows actor935/parent933,
server923/helper929 and the healthy tenant, but excludes the internal reapers.
Existing init/procs.rs133–137 deliberately removes same-executable internal
processes. Therefore a source-predicted reservation of4 is not an observed total.
The host assertion is being corrected without changing product visibility,
tenant budget or deadline. Preserve the pre-Fill failure as a fixture defect;
it demonstrates neither product resource escape nor successful resource control.

The local-route observations use new guest network namespaces and RFC5737/loopback
addresses, not host network administration. They are transparent responders, not
forwarding relays or a completed mandatory mediation mechanism. Their independent
removals and restored gates are a separate campaign; none is inferred here.

## Exact-source process reader comparison

Root r224 extracts read_stat byte-identically from API candidate
`05f02669048b1d1a9c9c1ca39e021a211d2ad103` (function introduced atf26bbd00),
using the probe committed at22ea786d8. Source/build hashes, copied test sources,
toolchain, raw samples and summary are in managed-proc-reader-cost-f26-v1.
The candidate remains unadopted. This comparison does not close the separate
owned-client MR-OOM-MAC-1 synchronization defect.

Both variants open, read and drop this process's ordinary ASCII /proc/self/stat.
The baseline is Rust1.98.1 std::fs::read, including its metadata/probe behavior;
the candidate calls its exact private reader with File::open. Four supplied ABBA
rounds,512 reads per process, yield4096 timing samples per variant. Timing runs
disable allocator accounting. Separate four-read runs count Rust global allocation
attempts and requested layout sizes. The counts exclude native libc allocations;
they are neither allocator-resident bytes nor kernel charges. No user content or
process cache is added. Inputs describe an experiment, not runtime constants.

| Variant | n | p50 µs | p90 µs | p99 µs | max µs |
| --- | ---: | ---: | ---: | ---: | ---: |
| std | 4096 | 6.459 | 6.650 | 9.189 | 155.351 |
| candidate | 4096 | 6.0105 | 6.589 | 8.457 | 41.757 |

For the observed324-byte records, both retain capacity512 until drop and perform
one allocation/one deallocation. Std performs4 reallocations, cumulative requested
992 bytes; candidate performs6, cumulative requested1016 bytes. Cumulative requested
bytes include replacement layouts, not simultaneous retention. Timing samples
sum to51/48 minor faults for std/candidate, zero major faults; raw process CPU
microseconds are also retained. Per-call rusage resolution and instrumentation
cost constrain those comparisons. Strace is **unavailable**, recorded explicitly;
no syscall counts were measured or inferred. The first four accounting samples
retain their cold-process effects; no filesystem/allocator cold-cache claim is made.

This is one host/campaign with an std comparison, not a controlled 4–5× competing
implementation result, confidence bound or optimization selection. The candidate
uses more reallocations. Independent repetitions/noise, syscall counts, native
allocator retention, complete API effects, parent/kernel costs, contention and
other platforms remain owed before adoption or performance claims.

## Evidence and policy limits

Three archive transfers were SHA256 verified:

- Linux raw: d66f53816446c2037d32a30139dc4b4561781873f86b54103fd595ef376a056b.
- Mac raw: 9bb9e898d3aeeeefad98555a2756a0faf006ae55864787e4577d9e813d2ae90c.
- Windows transfer: 99f2881457a1445b5eb88f80953ad2b25cde2fe632d88528f8138beaf05bc689.

SHA256SUMS covers committed regular files. Executables/objects and Windows debug
symbols remain outside this source evidence set, with original artifact hash
records retained. Originals, setup failures and archives remain on their owners;
no user data, source, credentials or experiment was removed.

R01/R04/R06/R07/R13: reviewed fixture states/typed results and cleanup at existing
seams, with the false observation assertion recorded rather than hidden. No
production panic-free proof. R02/R03/R05/R12: mechanism alternatives/primary-source
contracts remain in managed-network-intercept, managed-cleanup, managed-limits
and managed-borrowed records; no architecture selection, cache, timing allowance,
new installed binary, root or crypto change. R09/R10/R15: exact live outcomes and
raw identities above; coordinator alone runs these heavy campaigns. R11: seven
target source gate22ea and the named actual hosts only, not all VM combinations;
Windows ARM remains paused. R08/R14: narrow reader cost observation only, with
unmeasured induced costs and competitive target explicitly open. R16: privately
owned admitted runners/targets, preserved originals; no local heavy build while
this Mac approaches its30GiB floor.
