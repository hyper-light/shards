# Private native RPK policy comparison

**Experimental consumer pin, no adoption or endpoint architecture decision.** Root
reviewed the four upstream certificate-type conditions, then authorized this isolated
consumer experiment. It changes remote/Cargo.toml and its two corresponding lock
entries from f116ac276d2d53296de24c2a43837baf66ed1cbd to the signed, tested
233b7fcef7a363327c5439b191724a38065062cd child of that exact baseline. No unrelated
upstream687244f diff, Brotli-window change or other Cargo package update is included.
The rejected window experiment2504bac remains separate and unadopted.

Policy: canonical c921a0b12b06d495b354907162e991376edffd41, rules SHA256
1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d, all R01–R16.
Root coordinator owns review/integration/combined gates; resource/performance is
the scoped implementer. Native Linux amd64/glibc Fedora6.19.10, rustc1.98.1,
4CPU/8GiB/10GiB through kiln. Other targets/real VM hosts are unverified here.

## Decision and source evidence

Actual Shards TLS stack attribution identified Brotli's33,554,432B H10 allocation;
the handshake-ceiling hypothesis was discarded. Actual Ed25519 RPK Certificate
body53B becomes65B as a Brotli CompressedCertificate body. Upstream experiment
uses the existing verified certificate TYPE, with no size cutoff, new cache,
crypto/trust/key/signature/quality change or enlarged budget. RPK senders choose
ordinary Certificate even when old peers offer compression. RPK receivers omit
compression capability; this also prevents an old sender's optional encoder work.
Both are necessary against old peers in opposite authentication directions.
X509 compression/verification/size limits remain on the existing path.

[RFC8879](https://www.rfc-editor.org/rfc/rfc8879#section-4) permits ordinary
Certificate responses and requires unchanged verification/limits. That permission
alone does not derive this policy. [OpenSSL3.6.0 client](https://github.com/openssl/openssl/blob/7b371d80d959ec9ab4139d09d78e83c090de9779/ssl/statem/statem_clnt.c#L402)
and [server](https://github.com/openssl/openssl/blob/7b371d80d959ec9ab4139d09d78e83c090de9779/ssl/statem/statem_srvr.c#L430)
explicitly restrict compressed sending to X509. Upstream's evidence checkpoint
30545d2 on p/managed-rpk-negotiation records rustls0.23.45/GnuTLS alternatives,
official one-shot/size-hint results,32-case baseline/sender/receiver/both matrices,
all-X509 exact wire equivalence and8 wrong-pin negatives. Four independent guard
removals each restored one unwanted RPK encoder invocation and failed101; restored
source passed3/0 and strict clippy. Resolver predicates are exactly those already
used by type validators; dynamically mixed resolvers are unsupported by that API.

This avoids optional work for the actual RPK path; it does not close existing
X509 allocation failure/admission gaps or prove every future large RPK has fewer
wire bytes. Existing host/run reservations remain authoritative, never expanded
by measurement or learning. No production cache, buffer, task or wait is added.

## Native method, provenance and calibration

r65 at5f3f537e passed24 first-connection smoke processes. The actual paired run
r67 at58112217761880ba9e8cd0988c72f2ce29b229f0 passed128/128 serial processes:
80 counted (five repetitions) and48 System-allocator calibration (three).
Each repetition covers one/two tenants × normal/stalled stdout ×2N endpoint/
private shared prototype × old/candidate TLS policy. Per channel128 binary1024B
records, actual owner-pinned TLS/QUIC, independent application-consumed ACK,
one authoritative unacknowledged record, owner abort and surviving-neighbor progress.
Both variants use **the same candidate policy** when compared to each other.
The shared prototype still has the previously documented connection-wide malformed/
stale refusal mismatch and is not an equivalent hostile-failure architecture.

The runner uses our private managed-performance-rpk checkout and
~/managed-rpk-native-target reflink of our inactive owned cache. All four builds
force owned source mtimes current after sync, require Cargo's measured-crate
artifact fresh=false, retain build JSON/compile logs and copy the exact artifact
before another source variant overwrites its target path. Every pin/calibration
variant's complete source diff/hash and binary SHA256 is retained. The callback
snapshots use a cfg(test,Linux) net.rs hook into existing quic.stats; production
API/state/fast path is unchanged. No diagnostic allocation backtrace env is set.
Both counted policies have identical harness/hook hashes. The System calibration
changes only the harness global allocator/label in a captured source variant;
its zero allocation counters mean **unmeasured**, not zero allocations. The
summarizer marks them unmeasured. Restored source is byte-identical, git diff clean,
and strict Linux clippy passes before and after the campaign.

| Frozen binary | SHA256 |
| --- | --- |
| old counted |2e861620f624d5654918d73c93449a81b786e56ae0ef564530899cfcf721cb0c|
| candidate counted |5d75e78019fc6232342c5aed33098eb58414229aedac82f75e9b4e03d8749c4f|
| old System |a1ef82d8ccef5162cce01edd70c962d4d1455feb316516163235fe5f3c17f594|
| candidate System |bfc8ace41ed3744e9c53f5cd7b0f101aa863c11703440e6ba4a3cc0bb25083a4|

Cold setup here is the first live TLS subscription phase after endpoint bind,
not a cold filesystem/VM/program launch. Warm record distributions mean later
records on the established connection, **not TLS resumption or warm reconnect**.
The independent quality lane reproduced an immediate reconnect refusal while a
closed connection remained not drained; no reviewed production fix is imported.
Its retirement-event investigation remains separate. No polling/retry/cap increase
or cache masks that result.

## Measured outcomes and misses

Normal mode; medians over five processes, pooled record ACK percentiles retain
per-process identity. These are small process samples with correlated records;
noise/PSI/load and all maxima remain in raw data.

| Tenants / variant | TLS setup old→candidate ms | Requested Rust bytes old→candidate | Subscribed PSS old→candidate KiB | Closed cumulative minor faults old→candidate |
| --- | --- | --- | --- | --- |
|1 endpoint|5.198→2.921|138,243,198→721,794|8,159→5,750|1,189→472|
|1 shared|4.844→2.682|69,066,829→321,057|6,835→5,398|721→360|
|2 endpoint|5.226→3.039|276,388,308→1,451,453|10,757→6,669|2,129→718|
|2 shared|4.604→2.534|69,074,469→327,096|6,831→5,386|734→379|

Cold TLS setup medians improve about1.7–1.8×, **missing4–5×**. Requested-byte
reductions are much larger than resident savings: a large zeroed allocation need
not touch every physical page. This is not a32MiB RSS saving per certificate.

| Tenants / policy / variant | ACK n | p50 / p90 / p99 / max µs |
| --- | ---: | --- |
|1 old endpoint|1280|40.225 /46.096 /226.335 /502.516|
|1 candidate endpoint|1280|40.810 /51.074 /265.770 /562.352|
|1 old shared|1280|47.954 /55.522 /207.372 /292.047|
|1 candidate shared|1280|46.775 /52.793 /146.488 /329.307|
|2 old endpoint|2560|41.350 /46.426 /219.328 /544.033|
|2 candidate endpoint|2560|41.877 /48.605 /257.419 /1328.117|
|2 old shared|2560|55.553 /74.951 /193.569 /390.483|
|2 candidate shared|2560|51.870 /79.740 /229.595 /424.127|

Steady data ACK latency does not materially improve here; some candidate tails
are worse. The2tenant endpoint maximum rises544µs→1328µs. No causal attribution
or stable tail guarantee follows from five clustered processes. Candidate shared
median remains slower than candidate endpoints by about1.15×/1.24×. No overall
performance or shared-architecture win is claimed. System-allocator cold TLS
medians (old→candidate) are5.389→2.734ms/4.996→1.710ms at1tenant and
5.623→3.017ms/4.985→2.509ms at2tenants, endpoint/shared respectively.
Calibration/order noise is significant; allocation instrumentation is not assumed
free, and its own per-record/tail distributions remain in the raw/summary files.

Stalled stdout retains consumed=0 and no new credit. All neighbors prove one ACK
before cancellation and127 afterward. Candidate abort-wait medians/maxima µs:
1tenant endpoint39.616/121.064, shared29.258/93.794;2tenant endpoint32.866/469.057,
shared26.570/31.567 (n5 each). Every native process returns to its initial9FDs and
2libtest tasks. This checks descriptor/thread return for these bounded fixtures,
not universal leak freedom, population cleanup or an admitted host resource proof.

## Packet, parent and kernel accounting limits

Snapshots occur outside timed phases, once per actual unique client connection:
2N for endpoints and one for shared. RX observes peer UDP payload totals. Closed/
retired/cancelled missing states remain explicitly unknown; no zero substitution.
At TLS subscription, counted median TX/RX datagrams old→candidate:
1tenant endpoints37/37→38/34, shared13/13→12/11;
2tenant endpoints73/73→76/68, shared12/11→12/10.
TX count can increase even when Certificate bytes shrink. These are QUIC-reported
prepared/sent datagrams at a protocol event, not packet captures or proof all queued
I/O has drained. They exclude Driver wake datagrams, UDP/IP headers and some
kernel/control traffic. Actual TLS encrypted-flight byte comparisons remain the
upstream matrix; native QUIC padding/ACK/coalescing/scheduling varies.

A candidate counted1tenant endpoint workload has a187,535B TX outlier on one
connection versus the matching old20,653B; other connections/runs differ. It is
retained without inventing a cause or selecting a new transport tuning policy.
No universal wire-packet/byte improvement is claimed.

The same process owns all servers/clients and counted Rust requests, live/retained
bytes, RSS/PSS/private maps, CPU/faults, FDs/tasks and unique-socket SO_MEMINFO
snapshots. Socket configured limits are distinct from allocated/queued charges.
Parent Python RSS/PSS/FD/task/CPU/fault snapshots are retained before/after each
child, with time-v logs for each native process. C crypto malloc, copy volume,
transient kernel/slab peaks and exclusive cgroup memory attribution remain unmeasured;
this runner exposes CPU cgroup stats but no readable memory controller values.
No hard per-tenant kernel memory-budget proof or leak-free claim is made.

## Reproduction and retained data

[Raw logs, source/binary provenance and per-case summaries](results/rpk-policy/),
[paired summary](results/rpk-policy/paired-summary.json),
[paired.py](paired.py) and [paired_summary.py](paired_summary.py) are retained.
The verified raw archive SHA256 is
5ad455980fda169bae8363a71b0a6006a5bb355397849c7812cfb047af33949e.
Binaries remain in the private runner evidence directory, not Git. Run only after
kiln admits4CPU/8GiB/10GiB, within this owned private source/cache:

```sh
export CARGO_TARGET_DIR="$HOME/managed-rpk-native-target"
export MANAGED_PAIRED_EVIDENCE="$HOME/managed-rpk-native-evidence/NEW-UNUSED-PREFIX"
unset MANAGED_TRACE_ALLOC_MIN
python3 docs/research/measurements/managed-performance/paired.py
python3 docs/research/measurements/managed-performance/paired_summary.py "$MANAGED_PAIRED_EVIDENCE"
```

Owed: warm reconnect/resumption after independent reliability work, old/new Shards
QUIC cross-process interop (upstream old-peer TLS record checks are separate), full
upstream/consumer gates, supported seven targets and real supported host/VM matrix,
stronger noise/tail evidence, C/copy/full kernel charge accounting and failure-scope
equivalence for a shared-listener design. No production adoption follows this file.

## R01–R16 account

| Rule | Current proof / gap |
| --- | --- |
|R01|No new production behavior outside approved upstream pin. Four conditions add no allocation/arithmetic/precondition; test hooks only. Strict clippy passed. Broader existing allocation failure safety remains owed.|
|R02|Primary sources, alternatives, attributed live cost and both frozen-policy data above; no architecture adoption.|
|R03|No runtime literal/default/cutoff added. Explicit fixture inputs do not become authority.|
|R04|Existing type validation, pin/signature checks, subscription/credit/abort states retained. Reconnect refusal kept explicit.|
|R05|No new cache/wait/poll. Cold encoder work addressed at negotiated type seam; existing warm cache and reconnect limits documented.|
|R06|Minimal production pin; stats implementation private test-only and snapshots outside timing.|
|R07|Comments explain provenance, freshness/calibration and missing snapshots rather than narrating logic.|
|R08|Pinned equivalent policy comparisons with distributions, misses and adverse tails retained; no overall4–5×/shared win.|
|R09|Actual32MiB stack/red encoder reproduction precedes four guards; no unproven transport/reconnect fix.|
|R10|All128 native outcomes/raw data/hash/source patches retained; four upstream mutations/restored gates recorded separately.|
|R11|Linux amd64/glibc native only. All other supported targets and actual VM host combinations unverified; Windows ARM VM paused untouched.|
|R12|Existing Driver/TLS/identity/Service/ConnectionStats reused; no new framework/crypto/root privilege/installed binary.|
|R13|Private experimental pin touches only remote manifest/two lock entries; measurement hooks compiled out of production.|
|R14|Counted and System calibration, process/parent/kernel socket snapshots included; C/copies/full cgroup/transient charges and repeated stable tails remain gaps.|
|R15|Assigned implementer, no children/main merge. Root reviews signed checkpoint, owns final combined gates and adoption.|
|R16|Owned private source/cache; no guard bypass, other-lane cleanup or local builds; Mac30GiB floor preserved.|
