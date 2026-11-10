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

Results, mutation evidence and R01–R16 accounting will be recorded after execution.
