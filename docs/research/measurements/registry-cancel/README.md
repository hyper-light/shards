# Registry DNS/dial cancellation events

Status: scoped implementation/evidence batch; combined matrix, real-VM and competing
implementation performance gates remain with the coordinator. This is a host-native
blocking-worker experiment, not registry-provider or hostile-tenant VM evidence.

## Identity and invocation

`POLICY_READ`: revision `c921a0b12b06d495b354907162e991376edffd41`, policy SHA-256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`, role
`implementer`, coordinator `/root`, lane `/root/constants_research`. Owned paths are
`crates/registry/src/http.rs` cancellation/DNS/dial and focused tests, this directory,
and the D222 addition in `docs/design/architecture.md`. All R01–R16 read and acknowledged.

Runner `8fadc1ec213332e16cb0d915a3fb913f`, Fedora x86_64 GNU/Linux
`6.19.10-300.fc44.x86_64`; repository Rust `1.98.1`. The initial runner shell's
unqualified rustc reported `1.99.1`; repository cargo uses its pinned toolchain.
Private synced checkout `~/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-cancel-events-62f4448a70`,
private target `~/managed-cancel-events-target`, evidence `~/managed-cancel-evidence`.
The target was reflink-copied from the approved inactive read-only endpoint target;
the source target was not modified. Requested/admitted envelope stays 4 CPUs,
8 GiB RAM, 10 GiB disk and `-j 4`. A later 2-idle-core rejection was respected until
5 idle cores became available. No other implementer's source/target was touched.
No local compiler/test ran; local rustfmt only. Mac free space was 47 GiB before work.

From this checkout on the admitted Linux runner:

```sh
export CARGO_TARGET_DIR="$HOME/managed-cancel-events-target"
export SHARDS_HELPERS=skip
bash docs/research/measurements/registry-cancel/run.sh "$HOME/managed-cancel-evidence/fixed"
cargo test -j 4 --profile test-release -p shards-registry --lib
bash docs/research/measurements/registry-cancel/mutations.sh "$HOME/managed-cancel-evidence/mutations"
cargo clippy -j 4 -p shards-registry --all-targets -- -D warnings
```

`block.c` interposes only `getaddrinfo("shards-cancel.invalid",...)` and
`connect(192.0.2.1:9,...)`. These are the reserved `.invalid` name and TEST-NET-1
address; the calls never reach DNS or that address. The actual production worker
enters its libc call, connects to the test's private local Unix socket, and remains
blocked on a controller release byte. Cancellation is issued only after that worker
handshake. Returning the request does not release the worker. The fixture subsequently
releases it, observes its control socket EOF, and checks process FD/thread counts
return. No resolver settings, firewall, public endpoint or credentials are used.
The interposer is a test artifact, not a shipped/installed product binary.

The first sample holds the worker 750 ms to expose idle timer wakeups; remaining
samples cancel as soon as the handshake arrives. Twenty observations per case are
an experiment sample count, not runtime policy. The median guard is half the old
100 ms polling period, with a separate 1 s watchdog below the actual 3 s operation
deadline. These are test observation windows; no measurement becomes a product
literal. `run.sh` now returns an actual failing case's exit status. Its older baseline
version retained each actual case's `.exit` but returned zero after collecting both.

## Primary sources and alternatives

Exact fetched bytes/SHA-256 and pinned URLs are in [sources.json](sources.json).

- [Rust 1.98.1 Condvar](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/std/src/sync/poison/condvar.rs),
  `wait`/`wait_timeout`: the predicate is checked with one mutex; waiting releases
  that mutex atomically; notifications can be lost without this coordination;
  spurious returns require rechecking. Reusing a Condvar with a different mutex can
  panic, so every existing/new wait on `Cancelling::rung` uses `Cancelling::live`.
- [Rust 1.98.1 socket resolver](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/std/src/sys/net/connection/socket/mod.rs),
  `LookupHost::try_from`: synchronous `getaddrinfo`, without a caller cancellation
  token or deadline. Moving the wait does not create an OS cancellation primitive.
- [Go 1.27.1 dial](https://raw.githubusercontent.com/golang/go/go1.27.1/src/net/dial.go),
  `dialParallel`/`dialSerial`: distinct result, context and fallback-timer events;
  an early failure starts the fallback immediately; unused successful connections
  are closed. [Go's resolver](https://raw.githubusercontent.com/golang/go/go1.27.1/src/net/lookup.go)
  separately handles caller cancellation and a lookup still needed by another caller.
  Adopting Go's resolver/singleflight manager here would enlarge the architecture
  and duplicate ownership; this patch instead deepens the existing blocking seam.
- [curl 8.17.0 threaded resolver](https://raw.githubusercontent.com/curl/curl/curl-8_17_0/lib/asyn-thrdd.c),
  `getaddrinfo_thread`, `Curl_async_pollset`, `async_thrdd_shutdown`: completion can
  signal a socketpair/eventfd. Its no-socketpair polling fallback remains a fallback,
  not justification for Shards' timer. Worker shutdown explicitly avoids unsafe
  thread cancellation after hangs/leaks, and retains the context until workers end.
  A new socket/eventfd per Shards wait would add descriptors and OS adapters where
  the existing cancellation Condvar already supplies the required portable event.
- [RFC 8305 §5](https://www.rfc-editor.org/rfc/rfc8305#section-5) distinguishes
  paced address attempts from ordinary cancellation probes. Shards' existing 300 ms
  `RACE` remains a genuine address schedule, not repeated cancellation polling.
  **Its value is still an upstream default, not a proven R03-derived value**: the RFC
  recommends 250 ms and describes deriving from SYN/RTT history. Learning that
  schedule is a separate assignment; deleting it would change congestion behavior.

Chosen interface: a private worker `Reply<T>` and `wait_reply` used by both actual
DNS and dial paths. There is one existing mpsc channel per operation. No new cache,
async framework, resolver manager, listener registry, Arc allocation or extra channel
is added. Each worker produces at most one answer; queue cardinality is bounded by
that operation's existing worker count (one lookup or attempted resolved addresses).
This is not a byte budget or an aggregate abandoned-worker admission bound.

## State, ownership and waits

`Replied::{Answer,Cancelled,Deadline,Disconnected}` makes terminal wait states
explicit. Cancellation is checked before the deadline and answer under the existing
live mutex, so a cancellation observed before consumption wins a racing answer.
Without a Cancel, mpsc sleeps directly to completion/disconnection/the real deadline.
With one, only completion/cancellation/spurious notification or the actual deadline
returns the Condvar wait. There is no periodic state probe. The next address timer
is still `RACE`, clamped to the complete dial deadline.

`Reply` drops its sender before notification, under the same live mutex, including
an unanswered worker drop. This prevents a missed completion/disconnection wake.
The worker holds only `Weak<Cancelling>`, so a discarded request cannot retain the
client's sockets/helper handles. A late successful unused TcpStream is dropped when
its existing channel has no receiver. The original channels/thread creation and
name/address allocations remain; allocation failure behavior there is a pre-existing
R01 audit gap, not silently certified by this patch.

Cancellation cannot terminate libc `getaddrinfo`. A blocked worker retains its own
thread, stack, channel sender and lookup input until libc returns. A pending dial
likewise retains its socket until its connect completes/deadlines. There is no new
listener registration to leak. The fixture records these resources while held,
not as released. **Aggregate abandoned lookup admission remains unresolved**: pull
workers have an existing OS-core-derived active-flow ceiling, but arbitrary concurrent
callers and a libc call outliving its operation are not globally bounded by it.
No invented worker cap or learned expansion of authority is introduced.

The shared Condvar wakes all operations of that Cancel on a completion, including
retry pauses; each rechecks its own predicate. Simultaneous-wait tests cover answer
routing and cancellation, but large fanout/lock-contention costs remain unmeasured.
Replacing it with per-wait registrations would add allocation, unregister and owner-drop
obligations; this patch does not speculate that manager into existence.

## Results

Baseline source `f69ec070815fd18aece912ecaac468a13c6cfb82`, kiln r8:
20 samples each. DNS median about 100.053 ms, max 100.849 ms; dial median about
100.039 ms, max 100.461 ms. Both guards failed with `0 passed; 1 failed`; raw
`baseline-final/{resolve,dial}.log` and `.exit` retain the expected red. The first
750 ms idle sample had eight voluntary context switches for both waiters.
Earlier fixture-only build errors (missing TempDir unwrap, moved mode String) are
retained in `baseline*/build.log`; they are not claimed as reproductions.

Fixed source `21788a8ab2c51f99838f8ea3894283df3e4fb19c`, kiln r12:
DNS/dial cancellation each `1 passed; 0 failed`; registry library
`164 passed; 0 failed; 1 ignored` (the ignored fixture is run explicitly).
Strict host registry clippy passed (`fixed/clippy.exit=0`). Removing only completion
notification independently made DNS and dial fail the 1 s watchdog (both 0 passed /
1 failed, 3.00 s total after operation deadline/cleanup). Removing only cancellation
notification made both fail that watchdog (0 passed / 1 failed, 1.75 s after release
and cleanup). State publication stayed intact in each mutation. Restored completion
and cancellation, 20 samples per DNS/dial case, all passed. The completion mutation's
common failure text says “cancel did not wake”; its action/patch and actual request
were completion. No mutation was accepted as production source.

A final matched-harness before/after run (kiln r13) uses original f69ec070 production
plus the current exact libc/timing/resource fixture (`paired/legacy.patch`), then
restores the event source byte-for-byte. Both old guards fail with exit 101, both
event guards pass with exit 0, and source restoration checks clean. This is
sequential A/B; host load is a confound. Nearest-rank statistics (20 observations
each; with this n, p99 is the maximum sample) are computed by summarize.py:

| Path / source | cancel µs p50 | p90 | p99 / max | waiter CPU µs p50 | idle first-sample voluntary switches |
| --- | ---: | ---: | ---: | ---: | ---: |
| DNS old | 100056.525 | 100102.108 | 100154.286 | 86 | 8 |
| DNS event | 10.715 | 13.854 | 34.198 | 36 | 1 |
| Dial old | 100050.738 | 100065.828 | 100140.705 | 64 | 8 |
| Dial event | 11.453 | 16.646 | 62.708 | 40 | 2 |

The observed median ratios are about 9338× DNS and 8736× dial for this deliberately
blocked-worker cancellation symptom. They do not establish ratios against Go/curl,
throughput, overall request latency, resource usage, or the 4–5× product target. The
second dial idle switch is its retained real address schedule. Whole-process maximum
RSS (KiB) was 6688 → 6088 (DNS), 6632 → 6332 (dial); minor faults 402 → 389 and
400 → 389; major faults 0 in all four. Process CPU resolution is too coarse
(0.00–0.01 s) for a
precise ratio. These single-process maxima are descriptive, not a memory-saving
claim. Full raw observations, timing-resource output and failing/green statuses are
preserved in [results.tar.gz](results.tar.gz), checked by [results-manifest.json](results-manifest.json);
all per-sample distributions are in [summary.json](summary.json). To expand and
recompute without overwriting the evidence archive:

```sh
mkdir -p docs/research/measurements/registry-cancel/results
tar -xzf docs/research/measurements/registry-cancel/results.tar.gz -C docs/research/measurements/registry-cancel/results
python3 docs/research/measurements/registry-cancel/summarize.py docs/research/measurements/registry-cancel/results
```

The fixed cancellation fixture observed baseline `(10 FDs, 2 threads)` return after
every sample. While the real worker remained held after cancellation it observed
`12 FDs` for lookup, `13 FDs` for dial, and usually 3 threads (occasionally the
just-completing caller was still visible as a fourth). Control-stream EOF plus
counts establish this fixture's cleanup, not global leak freedom or kernel-byte
accounting. `/usr/bin/time -v` retains whole-test CPU/RSS/fault/context-switch data;
`RUSAGE_THREAD` retains waiter CPU and voluntary switches per sample. Allocations,
reallocations/copies, kernel charge, PSS, large fanout and neighbor impact are unmeasured.
The comparison is old Shards versus new Shards; Go/curl are researched alternatives,
not measured competing baselines. No achieved 4–5× overall target is claimed.

## R01–R16 review and owed gates

| Rule | Applicable evidence / remaining gap |
| --- | --- |
| R01 | No new production unwrap/assert/indexing/unchecked size or allocation; Condvar always uses live mutex, poison recovered; existing channel/name/address allocation failure gap retained explicitly above. |
| R02 | Pinned Rust, Go, curl, RFC primary sources, source hashes, real libc-call fixture, alternatives/tradeoffs above. |
| R03 | Removes 100 ms probe with no replacement interval. Existing protocol RACE and its unjustified default are separate/open; test constants are experimental observation windows only. |
| R04 | Explicit Answer/Cancelled/Deadline/Disconnected states; actual ErrorKind::Cancelled preserved; failure/drop/racing completion tests. |
| R05 | Actual DNS/dial polling replaced after live red; no cache added; independent completion/cancel mutations fail and restored checks pass above. Large shared fanout remains unmeasured. |
| R06 | One short shared wait state loop, one RAII reply guard; dial retains paced address loop. Review public API unchanged; no numeric complexity threshold. |
| R07 | Comments explain mutex pairing, late worker lifetime and Weak ownership; no debug instrumentation in product. |
| R08 | Old/new distributions retained; 4–5× leading comparable implementation target not established; pinned competing runtime comparison unrun. |
| R09 | Actual held getaddrinfo/connect workers reproduced timer-bound cancellation before production edit, baseline r8. |
| R10 | Durable C fixture, scripts, raw logs/status/source hashes, mutation patches, D222 entry; errors and missing results explicit. |
| R11 | Production change uses shared std APIs. Native Linux amd64 GNU result only; other six target lint/runtime, libc/distros and KVM/HVF/WHP gates unrun here. macOS arm64 only; paused Windows ARM VM untouched. |
| R12 | Existing registry seam; no OS-specific product code, extra installed binary, host root, authority or crypto change. |
| R13 | Minimal existing-owner event path without manager/cache/framework; original connect/lookup algorithms retained. |
| R14 | Thread CPU/switch samples and process time/RSS/faults, live FD/task return; allocation/copy/PSS/kernel charge/fanout/neighbor profiling unmeasured. |
| R15 | Scoped recurring lane authorized; no subagents or main merges; signed named-path commits with required attribution; root reviews/integrates and runs combined gates. |
| R16 | Private cache/checkout only; read-only reflink source; failed headroom admission respected; source/evidence/user data preserved; no local builds below 30 GiB floor. |

The read-only next-batch [constant inventory](constant-inventory.md) classifies
RECORD_FOR, crypto wait/probe, proxy connection admission and regex/template caches.
Their source findings are not live reproductions and do not authorize touching
another lane's files. The coordinator received the specific upstream template-cache
compatibility hazard and proxy-cap finding before any proposed implementation.
