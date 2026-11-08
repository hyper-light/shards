# Device allocation and poll audit probes

The saved baseline uses revision **38457b709e67c4a43de87adfd7f2053531672dc2**.
If the current product APIs have changed, copy this harness directory into an
isolated checkout of that revision before reproducing or comparing the baseline.

This harness measures the unchanged production `Queue` through its public API and includes production `vsock/poll.rs` by path. It starts no VM and modifies no product source. `run.py` builds offline, captures the revision, host, OS, compiler and SHA-256 of the relevant source files, and writes allocation counts plus n/p50/p90/p99/max to `results.json`. `results-samples.jsonl` retains every raw latency sample in chronological order for each case; its SHA-256 is recorded in the metadata. Samples are written only after a case's timing phase.

Run from the repository root:

```sh
python3 docs/research/measurements/audit-device-allocations/run.py
python3 docs/research/measurements/audit-device-allocations/run.py --fragmentation
```

The optional second command binds temporary Unix sockets, drives the real public Vsock device and its worker with an isolated synthetic guest driver, and cleans up the sockets. It does not boot a guest CPU. A restricted sandbox may require permission to bind the sockets; the allocation/poll measurements need only socket pairs.

`queue_*_pop_and_complete` reserves 4 MiB of isolated RAM, constructs the descriptor table outside the measured operation, publishes one request, then repeatedly resets queue progress, pops the chain and returns it to the used ring. Direct requests use the smallest power-of-two queue that holds their descriptors; indirect requests use a 256-entry queue. The 4,096-descriptor indirect case is **nonconforming hostile input accepted by the current implementation**, rather than a normal driver workload. The queue operation includes the chain's allocation/free and used-ring stores. There are 500 warmup and 20,000 individually timed iterations.

`poll_zero_timeout_*` retains the interest and readiness vectors across calls, as production does, and polls writable Unix socket pairs with a zero timeout. The read-and-write case registers both filters but receives only write readiness. Socket setup and destruction are outside measurement. There are 100 warmup and 2,000 timed iterations. This isolates wait-call bookkeeping with all writers ready; it does not model idle waits, payload throughput or command latency.

One separate untimed iteration uses a global allocator delegating to `System` to count alloc/alloc_zeroed, realloc and dealloc calls and requested layout bytes. Timing leaves counting disabled, retaining one AtomicBool test at the allocator boundary. These are **Rust allocation calls and requested sizes**, not native kernel/C allocations, resident memory, allocator size-class consumption, bytes actually copied by realloc or process-wide RSS. `old_reallocation_bytes` records original layout sizes; it is a bound on potential data movement, since the allocator may grow in place. Instant timer overhead, assertions and harness bookkeeping remain included. Concurrent work on the host was not prohibited, and max samples can contain scheduling delays.

`credit_bounded_vec_growth_shape` reproduces only the Vec growth shape of `TxBuf::append`: resize a fresh byte vector to 65,535 and then to 65,536 bytes. It records actual capacity and allocation requests, without timings. This model does not drive the private production TxBuf or establish how often guest traffic creates that shape. It demonstrates why the device's 64 KiB live-byte credit bound is not an allocated-memory bound when append uses ordinary geometric Vec growth.

The optional fragmentation probe tests 1,024 and 1,025 one-byte payload spans, with one additional 44-byte header descriptor, at this host's IOV_MAX boundary. Both chains exceed the device's 256-entry queue size. The VIRTIO 1.3 specification says indirect chains must not exceed Queue Size ([§2.7.5.3.1](https://docs.oasis-open.org/virtio/virtio/v1.3/virtio-v1.3.html)); the implementation's global MAX_CHAIN of 4,096 currently admits them. The probe therefore concerns unnecessarily accepted work and delayed rejection of nonconforming input. It does **not** demonstrate failure of a conforming driver packet. The appropriate current-device bound is Queue Size; if future devices support more than the host IOV_MAX, their scatter/gather syscall construction must also respect the host limit.

No optimization or Firecracker comparison is implemented by this harness. Preserve descriptor-local copies, memory bounds checks, ring ordering, notification rechecks, stream credit and half-close behavior when acting on its results.
