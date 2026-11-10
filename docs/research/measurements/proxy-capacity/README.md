# Proxy capacity notification experiment

Scope: the native host `Session::serve` accept loop, its admitted worker population,
capacity release and step-stop control. This is not a real-VM or network-interception
proof. Source starts at integration `adc019aa273599d9c320eb03dd736e2821e60d69`.
The coordinator owns combined gates and integration.

Policy read: `c921a0b12b06d495b354907162e991376edffd41`, rules SHA-256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`,
role implementer. Ada explicitly authorized recurring scoped implementation-quality,
live break/stress and performance work. This lane does not authorize global sweeps,
other agents' workspaces, host security changes or merges to main.

## Reproduction and method

The ignored `proxy_capacity_wait_costs` test runs the actual production accept loop
against a private native Unix listener. Its test-only connection allowance is one:
the minimum population exercising the same production admission branch. A real
worker sends its policy question and awaits its answer. A second real connection
then waits in the listener backlog. The test samples the accept thread's own
`/proc/self/task/TID/{status,stat,schedstat,wchan}` while the worker remains occupied.
It verifies that the second request has not been admitted, releases the first,
observes the second policy question, and closes stop with another connection queued.
It checks stop priority, worker-ledger emptiness and return of file descriptors.

The sampling interval and repetition count are experimental inputs, never production
timeouts or policy. Defaults are twenty 100 ms windows, with a 10 ms settling period;
`SHARDS_PROXY_SAMPLES` and `SHARDS_PROXY_SAMPLE_MS` override them. Five-second test
deadlines detect hung refill/stop and are not runtime behavior. `CLK_TCK` is recorded;
CPU ticks may be below its resolution. Missing scheduler nanoseconds remain null.

Run on an admitted Linux runner from the repository root:

```sh
CARGO_TARGET_DIR="$HOME/managed-proxy-events-target" \
PROXY_EVIDENCE_DIR="$HOME/managed-proxy-events-evidence" \
bash docs/research/measurements/proxy-capacity/run.sh baseline
```

`SHARDS_PROXY_REQUIRE_IDLE=1` additionally requires zero voluntary context switches
and zero CPU ticks during an occupied sample. This assertion is the baseline red
signal; functional refill/stop assertions precede it. Retain the environment, log,
exit status, source hashes and summary for each source. Comparative runs use release.

## Sources and alternatives

- [Go x/net v0.49.0 LimitListener](https://raw.githubusercontent.com/golang/net/v0.49.0/netutil/listen.go)
  blocks on permit acquisition or listener closure and releases permits on connection
  close. It establishes the completion/cancel contract, but importing an async runtime
  or duplicating its channel layer is unnecessary here.
- [Mio v1.1.0 pipe waker](https://raw.githubusercontent.com/tokio-rs/mio/v1.1.0/src/sys/unix/waker/pipe.rs)
  demonstrates persistent readiness notifications and draining. Mio's separate
  [kqueue implementation](https://raw.githubusercontent.com/tokio-rs/mio/v1.1.0/src/sys/unix/waker/kqueue.rs)
  uses EVFILT_USER. Shards already has a cross-platform UnixStream pair and
  `platform::wait_readable`; no new OS-specific backend is needed for three interests.
- [Rust UnixStream](https://doc.rust-lang.org/std/os/unix/net/struct.UnixStream.html)
  documents connected pairs and nonblocking I/O. Existing Shards `build::end` handles
  the macOS half-close behavior described in PM M236.
- [Linux proc documentation](https://docs.kernel.org/filesystems/proc.html) identifies
  voluntary context switches, thread CPU fields and wait channels. These are OS
  observations of the production thread, not implementation counters.

A Condvar alone cannot wait on the existing external stop socket; adding a stop
watcher thread would introduce another lifetime and cost. A separate completion
socket can share the existing readiness wait with stop and listener. Reusing the
policy-question socket would create competing readers and could steal policy wakes.
The worker map remains the capacity authority; notification bytes never confer a
permit. Do not add a cached count or parallel semaphore.

## Observed baseline and implementation

The first actual native run, kiln `r3`, used fixture commit
`23f2e86df57fe3b968da13689e0eae7863bb1c2c` over unchanged production at
`adc019aa`. Fedora x86_64 Linux `6.19.10-300.fc44`, Rust `1.98.1`, diagnostic
`test-release` profile: one test failed, zero passed, cargo exit 101. The acceptor
made **91 voluntary context switches in 100,181,689 ns** while the admitted worker
could not finish. Refill took 1,032,120 ns; stop took 1,026,903 ns. CPU ticks and
fault deltas were zero in that window; the tick resolution is not evidence of no
CPU cost. These diagnostic values are not the release-profile comparison.

The second controlled boundary, kiln `r6`, restored production and fixture from
`a27bdb581668a5099476592efd01269a66272ab1` and applied
`identity-boundary.patch`. It uses a real native connection with the test-only next
identity set to `u64::MAX`; it does not claim an ordinary external exploit. The
original optimized `hold` wrapped and retained one connection:
`refused=false panicked=false admitted=1`. One test failed, zero passed, cargo exit
101. The wrapper then stopped with exit 1 because Fedora has no `rg`; this wrapper
failure is separate from the observed boundary failure. The follow-on job uses
available tools. Release comparison had not started in `r6`.

Kiln `r7` subsequently completed the release baseline: one test passed, zero failed,
twenty samples, exit 0. Its source was the same old `a27bdb58` server/fixture plus
the retained boundary patch; it saved `baseline-native` before any source sync.
The occupied acceptor's wait channel was `hrtimer_nanosleep` in every sample.

| Release baseline, n=20 | p50 | p90 | p99 / max |
| --- | ---: | ---: | ---: |
| Voluntary context switches / approximately 100 ms | 91 | 91 | 91 |
| Scheduled acceptor CPU, ns | 480,221 | 559,931 | 565,334 |
| Worker release to next policy question, ns | 719,472 | 977,863 | 1,035,372 |
| Stop closure to acceptor return, ns | 1,067,397 | 1,121,410 | 1,124,086 |

CPU tick, minor-fault and major-fault deltas were zero in these windows. Every
session returned its descriptors after all workers and owned sockets dropped.
The scheduler nanoseconds are higher-resolution observations than the recorded
100 Hz CPU tick fields; zero ticks do not contradict the measured CPU cost.
This first sequential baseline is separate from the alternating-order comparison.

The implementation retains the live worker map as its authority, removes listener
interest while full, and publishes worker completion/session end through a private
nonblocking socketpair. Draining precedes the map check. A completion before the
check changes the predicate; one after the check remains readable through the wait.
Unread notifications coalesce when the OS queue is full. EOF or other notification
failure ends this session's admission and owned connections. Existing stop readiness
is checked first. Creation/nonblocking failures release the socketpair by ownership.

The new control resource is two descriptors and their OS socket queues per session,
held until its scoped workers finish and the session drops. There is no additional
thread, cached count, parallel permit owner or runtime dependency. The single byte
is the minimum nonempty stream write, independent of authority; the fixed wait
positions name stop, completion and listener. No production timeout was added.
On Windows the existing `crates/afunix/src/lib.rs::UnixStream::pair` creates a
temporary pathname listener, then connects and accepts. This additionally uses a
transient third socket and pathname at startup; its historical PM M199 time is
not a measurement of this change. Current Windows startup and memory costs remain
unmeasured.

Admission also checks identifier advancement and uses `HashMap::try_reserve` before
committing a cloned socket and next identity. Closed, exhausted identity, socket
and allocation errors retain distinct typed variants at the real caller. The
existing outer `Session::begin` String presentation boundary remains a narrow
compatibility bridge for startup I/O errors. The unchanged 512-connection bound
has an inherited derivation gap and is preserved by the coordinator's explicit
scope; this batch does not validate that constant.

Live logs remain under `~/managed-proxy-events-evidence` on Fedora runner
`8fadc1ec213332e16cb0d915a3fb913f`. Builds use the private
`~/managed-proxy-events-target`, initially reflinked read-only from the completed
endpoint target with coordinator authorization. The source target is preserved.
All campaign jobs request 4 CPUs, 8 GiB memory and 10 GiB disk. A short repeat
requested a mismatched smaller envelope, was rejected without execution, and was
not retried at that size. Subsequent jobs retain the assigned envelope.

Kiln `r8` tested production and fixtures from signed implementation commit
`1eff4a324cc9c8f6cf7fd51221079ef7b9fd8e75`: **21 passed, zero failed, two ignored**
native proxy tests in 5.23 s, followed by the explicitly selected strict-idle
measurement: **one passed, zero failed**, twenty sessions, exit 0. The boundary
reported `refused=true panicked=false admitted=0`. The portable cases observed
refill, stop with another socket queued, an early completion, notification-path
failure closure, HTTP/TLS compatibility and the existing connection limit.

Every changed occupied window recorded zero voluntary context switches, CPU ticks,
scheduler nanoseconds and page-fault deltas. Its wait channel was
`poll_schedule_timeout.constprop.0`, an indefinite socket-readiness wait: the symbol
name does not mean a periodic timeout was requested. Scheduler accounting recorded
positive CPU for the baseline; these zeroes still establish only the sampled idle
window, not zero total CPU or allocations. Each session returned its descriptors.
`fds_live` is observed late in teardown before all worker joins; it is not a peak
or a fixed-lifetime resource census.

| Sequential changed release, n=20 | p50 | p90 | p99 / max |
| --- | ---: | ---: | ---: |
| Worker release to next policy question, ns | 156,898 | 428,252 | 486,753 |
| Stop closure to acceptor return, ns | 11,894 | 51,676 | 90,070 |

The alternating-order comparison, stress, completed mutations and clippy remain
pending. Their unchanged 4 CPU / 8 GiB / 10 GiB follow-on request was rejected by
kiln for zero idle CPU headroom, without execution. No completion or comparative
target is claimed, and the envelope was not lowered.

The retained `paired.py` runs the saved release binaries in alternating old/new
order for twenty pairs, records binary hashes and host CPU pressure, then exercises
two hundred changed sessions. `mutations.py` removes completion publication,
listener disarming, checked identity advancement and notification-failure closure
independently, retains source patches/hashes/logs/exits and restores the source.
Mutation diagnostics use `test-release`; they make no comparative timing claim.
Run each script in this lane's assigned admitted envelope, never alongside another
job writing its private target:

```sh
PROXY_EVIDENCE_DIR="$HOME/managed-proxy-events-evidence" \
python3 docs/research/measurements/proxy-capacity/paired.py
CARGO_TARGET_DIR="$HOME/managed-proxy-events-target" \
PROXY_EVIDENCE_DIR="$HOME/managed-proxy-events-evidence" \
python3 docs/research/measurements/proxy-capacity/mutations.py
```

## R01–R16 review record

| Rule | Scoped evidence and remaining work |
| --- | --- |
| R01 | Admission advancement is checked and map growth is fallible before ownership commits; no production assert/unwrap/indexing was added. Counter boundary red and green, notification-failure closure and existing compatibility tests are retained. The coordinator separately composes the already implemented fallible `platform::wait_readable` adapter from capture `9151296a`; this branch does not duplicate it. Actual allocation-failure injection and strict clippy are not run. |
| R02 | Primary Go/Mio/Rust/kernel sources and alternatives above; actual native repeated-wake and identity-boundary observations precede their fixes. Sequential old/new release observations exist; interleaved comparison and coordinator review remain pending. |
| R03 | The 1 ms production probe is removed. Added literals represent one minimum stream byte and the three concrete readiness positions; no new numeric runtime policy exists. Existing connection-limit derivation remains explicitly owed. Test durations/counts are workload inputs only. |
| R04 | The worker map and existing ended flag remain authoritative; full/available interests derive from them. Typed admission variants preserve closed, identifier, socket and reservation causes. Persistent notifications have no authority. Stop, early completion, full-to-available and notification-failure closure passed their actual native fixtures. |
| R05 | Actual polling was reproduced, replaced and observed absent in twenty changed idle windows; no cache, cached count, permit ledger, trait or async manager was added. Independent mutation results remain owed; inventory alone is not claimed complete. |
| R06 | Focused notification write/drain helpers contain interruption, coalescing and closure behavior; admission retains one owner. Native compatibility tests passed; clippy remains pending. |
| R07 | Comments explain drain/check ordering, separate wake ownership and failure closure. D110 links this record. |
| R08 | The pinned existing implementation is the same-workload comparison baseline. Sequential release distributions are above; alternating-order and competing-system measurements are pending. No 4–5× claim is made. |
| R09 | Actual red `r3` and controlled boundary red `r6`, their source hashes and exact commands are retained. No production fault switch is used. The boundary preload is explicitly labelled rather than presented as an ordinary exploit. |
| R10 | Fixture commits, run/summary/interleaving/mutation scripts, boundary patch, D110 and this record retain the reproduction method. Raw red/green logs remain on the named Fedora runner; local artifact transfer and independent mutation results are pending. |
| R11 | Source uses the shared socket/readiness seam. Red/green observations are native Linux amd64 glibc only. Other six supported targets, actual VM paths and native macOS/Windows remain unverified. Intel Mac is unsupported; paused Windows ARM VM is untouched. |
| R12 | No OS branch, new dependency, helper deliverable, host-root action or cryptographic change. The existing shared platform adapters remain their owners' code. |
| R13 | Existing admission bound and worker ownership persist. The additional control resource is one socketpair per session, whose actual memory cost remains unmeasured. |
| R14 | The fixture observes actual per-thread switches/CPU/faults, refill/stop latency and descriptor cleanup; sequential release samples are retained. Allocator/copy/RSS/kernel-queue and neighbor-cost profiles remain unmeasured. `schedstat` zeroes can mean disabled accounting; positive old-source observations and the sysctl are recorded, but no zero-total-CPU claim follows. |
| R15 | Explicit scoped implementation/live/performance authorization, policy receipt and runner envelope are recorded above. The coordinator owns combined gates/merges. No child agents, other-agent interruption, global sweep or merge to main occurred. Signed implementation 1eff4a32 passed the native proxy group and strict-idle samples; follow-on proof is held by the runner guard. |
| R16 | This Mac had 51 GiB free before work and 47 GiB at the later check, above the 30 GiB floor. The private Fedora target was reflinked only from the coordinator-confirmed inactive source; no original cache, source, user data or evidence was removed. Kiln headroom rejections were respected. |
