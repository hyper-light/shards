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

Release comparison, completed mutations, green native tests and R01–R16 accounting
remain pending; no completion or performance target is claimed here.

## R01–R16 review record

| Rule | Scoped evidence and remaining work |
| --- | --- |
| R01 | Admission advancement is checked and map growth is fallible before ownership commits; no production assert/unwrap/indexing was added. Counter boundary red is retained. Green/fault tests are pending. The coordinator separately composes the already implemented fallible `platform::wait_readable` adapter from capture `9151296a`; this branch does not duplicate it. Actual allocation-failure injection is not run. |
| R02 | Primary Go/Mio/Rust/kernel sources and alternatives above; actual native repeated-wake and identity-boundary observations precede their fixes. Coordinator review and release comparison remain pending. |
| R03 | The 1 ms production probe is removed. Added literals represent one minimum stream byte and the three concrete readiness positions; no new numeric runtime policy exists. Existing connection-limit derivation remains explicitly owed. Test durations/counts are workload inputs only. |
| R04 | The worker map and existing ended flag remain authoritative; full/available interests derive from them. Typed admission variants preserve closed, identifier, socket and reservation causes. Persistent notifications have no authority. Stop and notification-failure paths are exercised by focused fixtures, with green execution pending. |
| R05 | Actual polling was reproduced and replaced in source; no cache, cached count, permit ledger, trait or async manager was added. Before/after and independent mutation results remain owed; inventory alone is not claimed complete. |
| R06 | Focused notification write/drain helpers contain interruption, coalescing and closure behavior; admission retains one owner. Native compatibility tests and clippy remain pending. |
| R07 | Comments explain drain/check ordering, separate wake ownership and failure closure. D110 links this record. |
| R08 | The pinned existing implementation is the same-workload comparison baseline. Release distributions and competing-system measurements are pending; no 4–5× claim is made. |
| R09 | Actual red `r3` and controlled boundary red `r6`, their source hashes and exact commands are retained. No production fault switch is used. The boundary preload is explicitly labelled rather than presented as an ordinary exploit. |
| R10 | Fixture commits, run/summary scripts, boundary patch, D110 and this record retain the reproduction method. Raw logs remain on the named Fedora runner; local artifact transfer and completed green/mutation records are pending. |
| R11 | Source uses the shared socket/readiness seam. Red observations are native Linux amd64 glibc only. Other six supported targets, actual VM paths and native macOS/Windows remain unverified. Intel Mac is unsupported; paused Windows ARM VM is untouched. |
| R12 | No OS branch, new dependency, helper deliverable, host-root action or cryptographic change. The existing shared platform adapters remain their owners' code. |
| R13 | Existing admission bound and worker ownership persist. The additional control resource is one socketpair per session, whose actual memory cost remains unmeasured. |
| R14 | The fixture observes actual per-thread switches/CPU/faults, refill/stop latency and descriptor cleanup. Release raw samples remain pending; allocator/copy/RSS/kernel-queue and neighbor-cost profiles remain unmeasured. `schedstat` zeroes can mean disabled accounting and are not proof of zero CPU; tick resolution is recorded. |
| R15 | Explicit scoped implementation/live/performance authorization, policy receipt and runner envelope are recorded above. The coordinator owns combined gates/merges. No child agents, other-agent interruption, global sweep or merge to main occurred. Only fixture commits are currently signed; implementation validation/commit is pending. |
| R16 | This Mac had 51 GiB free before work and 47 GiB at the later check, above the 30 GiB floor. The private Fedora target was reflinked only from the coordinator-confirmed inactive source; no original cache, source, user data or evidence was removed. Kiln headroom rejections were respected. |
