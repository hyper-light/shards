# Stream allocation and buffer-copy audit probes

The saved baseline uses revision **38457b709e67c4a43de87adfd7f2053531672dc2**.
If the current product APIs have changed, copy this harness directory into an
isolated checkout of that revision before reproducing or comparing the baseline.

Run from the repository:

```sh
python3 docs/research/measurements/audit-stream-allocations/run.py 500 > /tmp/shards-stream-allocations.json
```

The standalone Cargo workspace uses the actual ABI and IPC crates. `build.rs` extracts
`each_frame`, `log_record`, `now`, and the log reader's structs and `read`/`split`/`take_lines`
methods directly from current product source. The first-output, poll-vector, plain stdin,
and exec pointer-list cases reproduce the identified source operations. The runner
records the source SHA-256 values and refuses results if those inputs change during
compilation. Its output includes host, OS, Rust version, revision, working-tree status,
all raw samples, n, p50, p90, p99 and max.

Allocation counts cover one event with a counting Rust global allocator. Each timing
distribution is measured separately with the counters disabled; the allocator wrapper's
flag check remains. `requested_bytes` sums initial allocation sizes and realloc target
sizes, not retained memory, bytes physically moved, or native/library allocations. The
results are native macOS/System measurements, not the guest's Linux/musl allocator.
The log-history result reports live vector capacities after reading 1 MiB of output
split into 1,024 lines, including the retained pending buffer and line-vector metadata.
It does not measure daemon RSS or PSS.

Parser timing excludes allocating/refilling its input. The input contains 5,461 valid
12-byte SIGNAL frames and a 3-byte partial header. Both parsers are required to deliver
all complete frames and preserve that tail. `suffix_bytes_moved` follows exactly from
the number of prefix drains and their remaining suffix lengths. The cursor variant is
a local prototype used to isolate the repeated compaction cost. It is not a product
implementation, and this one case is not a substitute for malformed-frame, split-read,
EOF, ordering, backpressure, signal and terminal regression tests.

This is an allocation/algorithm probe. It does not boot a VM, measure the actual guest
relay or end-to-end command latency, count hypervisor/page faults, establish a production
speedup, or compare Firecracker. Small nanosecond timing differences are especially
sensitive to instrumentation and host scheduling; the allocation counts and identified
copies are the stronger evidence.
