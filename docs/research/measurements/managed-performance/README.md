# Managed-output native performance checkpoint

The later [private RPK policy comparison](RPK-POLICY.md) retains the upstream
certificate-type experiment, fresh paired native data and allocator calibration.
Its consumer pin and shared-listener prototype remain experimental.

This is a native loopback experiment, not real-VM output validation, a production
architecture decision, a leak-freedom proof, or a 4–5× achievement. See
[source/alternative/resource research](RESEARCH.md) and the exact
[raw source and binary hashes](results/baseline/batch-1/source.txt).

Policy: canonical `c921a0b12b06d495b354907162e991376edffd41`, SHA-256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`.
Coordinator `/root`; implementer `/root/resource_performance`. Owned paths are
this directory and the coordinated four-line `cfg(test, linux)` hook in
`crates/remote/src/managed.rs`. No production behavior or dependency pin changed.

## Completed runs and reproduction

Fedora amd64, kernel `6.19.10-300.fc44.x86_64`, Rust `1.98.1` / LLVM `22.1.8`.
The runner is shared and noisy; per-phase load/CPU pressure is retained. Kiln
admitted each command with `linux/amd64,cpus=4,mem=8g,disk=10g`. Insufficient
headroom refusals were honored without lowering the request. The private synced
clone is `~/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-performance-1496391d6e`;
the private target is `~/managed-performance-target`, reflinked from the approved
inactive endpoint cache. Original caches and other lanes were not modified.

- r2: standalone release, one tenant / two channels ×16 binary records ×1024
  bytes; real pinned-owner TLS, exact byte validation and application ACK credit:
  exit 0. The source was synced before the first docs commit; the later identical
  committed harness is `e2cf31ba`. This is an initial functional result only.
- r6, source `8feb09ab`: one process pair, endpoint and shared variants, both
  `1 passed; 0 failed`. [Endpoint](results/baseline/endpoint-libtest-first.tsv)
  and [shared](results/baseline/shared-libtest-first.tsv) raw logs.
- r9, source `d9d60bfa7640691748e1800ef3c48c22c44a37cf`: 24/24 passing release
  processes: three repetitions × one/two tenants × normal/stall × endpoint/shared.
  Each channel completes 128 records of 1024 bytes. In stall cases, the first
  stdout record remains unacknowledged; each neighbor first completes one record,
  then cancellation wakes the blocked producer, then every neighbor must finish
  another 127 records. Two-tenant cases therefore require three records before
  cancellation and 381 afterward. Every process returned to its initial nine FDs
  and two libtest tasks. [Summary](results/baseline/summary.json),
  [raw samples](results/baseline/batch-1/), and per-process `time -v` logs are retained.
- r11, source `5a8b227d`: separate diagnostic allocation-stack run passed. It is
  excluded from timing statistics. The [stack](results/baseline/allocation-trace/stacks.log)
  attributes a 33,554,432-byte zeroed allocation to Brotli H10 history during TLS
  certificate compression. The earlier handshake-ceiling hypothesis is discarded.
- r30, source `c679e30b570d5a0a3d43ec8c55efc406341b3d1e`: the direct
  certificate codec probe passed 1/0. The generated Ed25519 SPKI Certificate body
  is 53 bytes; Brotli payload is 57 bytes and CompressedCertificate body is
  65 bytes. Amortized compression requested 34,333,162 Rust bytes; Interactive
  requested 550,671. These are single diagnostic samples, not distributions.
  The separate warm reconnect fixture timed out at its external 120-second guard
  (exit124), after a cold TLS exchange; it is not a cache-hit performance result.
  [Raw codec and partial reconnect logs](results/certificate/certificate/).
- Strict Linux clippy passed at `a484f292`, after fixing two harness-only
  collapsible conditions: `cargo clippy --locked -j4 --profile test-release
  -p shards-remote --lib --tests -- -D warnings`.
  [Raw log](results/certificate/harness-clippy-fixed.log).

Exact r9 invocation, within an admitted runner command and the frozen source:

```sh
export CARGO_TARGET_DIR="$HOME/managed-performance-target"
export MANAGED_EVIDENCE="$HOME/managed-performance-evidence/batch-1"
export MANAGED_HANDSHAKE_BYTES=16384 MANAGED_DATAGRAM_BYTES=65527
export MANAGED_RECORDS=128 MANAGED_BYTES=1024
export MANAGED_PATIENCE_MS=10000 MANAGED_REPETITIONS=3
docs/research/measurements/managed-performance/run.sh
```

These numbers are explicit fixture inputs, not new production policy. Datagram
allowance equals the existing RFC 9000 maximum UDP payload bound; 16 KiB is an
operator-selected handshake fixture allowance, not justification for a runtime
default. The harness propagates setup errors and uses bounded channels/reservations.

## Measured common-path results

Normal mode, pooled record ACK latency; nearest-rank empirical percentiles.
Process and channel identity remain available in raw samples. Three process
repetitions do not establish stable tail estimates or controlled contention.

| Tenants | Variant | Records | ACK p50 / p90 / p99 / max (µs) | TLS Rust requested bytes, median | Subscribed PSS, median KiB | Tasks / FDs at subscription |
| --- | --- | ---: | --- | ---: | ---: | --- |
| 1 | Per-channel endpoint | 768 | 57.360 / 72.281 / 147.652 / 903.114 | 138,224,839 | 7,696 | 6 / 19 |
| 1 | Private shared prototype | 768 | 64.187 / 80.887 / 148.972 / 549.439 | 69,056,054 | 4,264 | 4 / 16 |
| 2 | Per-channel endpoint | 1,536 | 46.679 / 63.775 / 125.569 / 824.483 | 276,598,567 | 8,176 | 10 / 29 |
| 2 | Private shared prototype | 1,536 | 69.247 / 103.833 / 141.483 / 810.315 | 69,047,441 | 4,342 | 4 / 20 |

The shared prototype loses median ACK latency by about 1.12× (one tenant) and
1.48× (two tenants), while its requested TLS allocation volume is about 2×/4×
smaller. This is not an overall 4× win: socket/TLS counts differ by construction,
PSS varies substantially, and hostile-refusal isolation is not equivalent.

TLS setup Rust allocation/reallocation counts are 1,597/126 vs 659/60 at one
tenant and 3,245/261 vs 683/62 at two tenants (process medians). The median TLS
phase durations are 6.552/6.102 ms and 4.959/4.474 ms respectively. These are
instrumented setup times, with cold crypto/allocator/page state and noise visible
in the raw files; they are not a product startup claim. Cumulative process faults
at close are 1,188/720 and 2,132/734 minor faults respectively, with zero major
faults in these runs. CPU, free/retained requested bytes, receive latency, per-run
maxima and cancellation distributions are in the machine-readable summary.

At one/two tenants, cancel-waiter medians are 30.735/38.347 µs for endpoints and
42.196/26.497 µs for the shared prototype; each is only three samples. All record
counts after cancellation are verified by completed workers. The authoritative
unacknowledged record remains charged until ACK or owner cancellation.

Configured socket buffer limits are recorded separately. Actual unique-socket
SO_MEMINFO values often show zero queued receive/transmit bytes and 4096 bytes
of forward allocation on active sockets at the snapshot; full per-inode arrays
are retained. These snapshots miss transient peaks and do not include every
kernel socket/slab cost. The cgroup exposes CPU statistics but no readable memory
controller files here, so exclusive cgroup memory/kernel attribution is unverified.

## Current allocation lead and owed work

The trace is `alloc_zeroed(33554432)` → `alloc-stdlib::StandardAlloc::alloc_cell`
→ Brotli 8.0.4 `initialize_h10` → `BrotliMakeHasher` → hyper-tls
`compress.rs::BrotliCompressor::compress` → `CompressionCacheInner::compression_for`
→ server TLS 1.3 compressed Certificate emission. Source inspection finds H10
history nodes initialized as `1 << lgwin`; the existing fixed `lgwin=22` creates
`2^22 ×2 ×sizeof(u32) = 33,554,432` bytes. Further source attribution finds
`BrotliMakeHasher` initializes H10 with `one_shot=false` and size0, even through
the official one-shot/size-hint APIs. This identifies the underlying cost.

RFC 8879 §4 permits an ordinary uncompressed Certificate response and explicitly
covers raw-public-key Certificate encodings. Existing rustls compressors, Brotli
input size hints, one-shot APIs and input-derived history windows are under
comparison. An isolated upstream input-derived-window experiment is rejected:
it reduced the small RPK cost but increased peak memory and CPU above the old
window boundary through the uint24 maximum. Rejection and all four raw boundary
outcomes are retained on upstream `p/managed-certificate-window` at `bc2bc83`;
no Shards vendor pin changed. A separate clean pinned-baseline certificate-type
policy experiment is under review. The reconnect failure is independently owned
by the quality lane; no cap increase, retry polling or cache masks it here.
Native wire packet/flight deltas, independent mutation,
unmodified-allocator calibration, TLS C malloc/copy/slab accounting and stronger
host isolation remain owed. Any chosen hyper-tls/hyper-quic change must land in
`~/Projects/hyper-raft` first, then be re-pinned in Shards.

## R01–R16 account

| Rule | Evidence / remaining gap |
| --- | --- |
| R01 | Product change is a test-only module hook. Harness setup uses fallible buffer reservations; retained existing Service/Driver APIs are unchanged. Strict Linux clippy passed at a484f292; raw log linked above. Tests may assert; no new production panic is added. |
| R02 | RFC 9000/8879/7932 and pinned Quinn/quic-go/rustls/Brotli alternatives inspected; real TLS data retained. No production architecture selected; certificate wire/cold-warm comparison remains owed. |
| R03 | Fixture inputs explicit; bounds derive from channel allowances/protocol framing. No measured value becomes runtime policy. Existing compressor tuning remains under research. |
| R04 | Existing Subscription/Slot/Ending states and typed Reasons are reused. Shared adapter is private and its connection-wide refusal mismatch is explicit. |
| R05 | Producer waits use Condvar credit/abort notifications; driver wakes use existing datagram/stop readiness. No cache added. Existing TLS compression cache cost is being measured; its presence does not justify hiding the cold allocation. |
| R06 | New dispatch lives only in private measurement code. It routes bounded channels and delegates record/control behavior to Service; production complexity unchanged. |
| R07 | Comments explain measurement limits, allocation/safety contracts and protocol-derived framing. |
| R08 | Pinned product baseline and common workloads measured; median latency misses and non-equivalence reported. No overall 4–5× claim. Credible external implementation benchmark remains owed. |
| R09 | Actual32MiB allocation has live stack capture before any proposed fix. No Producer publication/wake-failure defect has been reproduced or fixed. |
| R10 | Exact commands, hashes, logs, samples, timings and summarizer retained. No changed production guard, so its mutation proof is not applicable yet; harness ACK/cancel mutation remains owed. |
| R11 | Native Linux amd64/glibc only. Windows amd64/arm64, Linux arm64 and musl, macOS arm64, and actual supported VM hosts remain unverified here; Intel Mac unsupported. |
| R12 | Existing Driver/TLS/identity/Service reused. No new crypto/framework, root privilege, installed binary or OS production adapter. |
| R13 | Test-only seam keeps experimental dispatch out of production behavior; unfinished refusal isolation is not advertised as complete. |
| R14 | Rust alloc/realloc/free/requested retention, process CPU/faults/RSS/PSS/private mappings, FD/task counts and unique kernel socket snapshots recorded. C allocations, copies, full slab/cgroup attribution, transient peaks and instrumentation-free comparisons remain gaps. |
| R15 | Scoped recurring performance lane authorized; no child agents or main merges. Signed checkpoints returned for root review; combined source6d7cc93c4 remains root's separate real-VM/final-gate responsibility. |
| R16 | Private cache/source only, approved inactive reflink source preserved. No cleanup of other lanes or user data; no runner guard bypass. This Mac remains above30GiB and no builds ran here. |
