# Engine ring v1 (D230): light checks run, and what the coordinator runs

Implementer record for branch `p/engine-ring-v1` (from `origin/p/vsock-credit-v1`
1e448c7e). POLICY_READ: origin/main bff30151, rules df2acf47 (POLICY_READ.json). The
decision and its evidence: [engine-ring-channel-decision-v1](../../engine-ring-channel-decision-v1.md).
Nothing here ran a VM or a measurement.

## Light checks the implementer ran (runner adacachy, Linux 7.2.9-cachyos x86_64, -j4)

| step | command | result |
|---|---|---|
| 1 abi | `cargo test -p shards-abi --lib` | 28/28; mutations (slot-size check, page alignment) caught |
| 2 ring core | `cargo test -p shards-vmm --lib vsock::ring` | 9/9; mutations (head ahead, in use, tail behind, flag left set, pins kept) caught |
| 3, 3b device | `cargo test -p shards-vmm --lib vsock` | 48/48 (vmm lib 435/435 at step 3); mutations (kill keeps the ring, shutdown before drain, any byte a doorbell, host end not told, full ring never waits, owed doorbell never sent) caught |
| 5 init | `cargo test -p shards-init ring::` | 4/4; mutations (never ring for data, never ring for room, end never said, client ended before the ring drained) caught, each failing in about 5 s |
| 4 shards | `cargo test -p shards --bin shards engine_hellos` | see the final report |
| all | `cargo clippy -p shards-abi -p shards-vmm -p shards-init -p shards --all-targets` (`SHARDS_HELPERS=skip`, zig 0.16.0 on PATH) | no warnings from these crates |

Not run by the implementer: any other target (macOS arm64, Windows x86_64 and arm64,
Linux aarch64 gnu and musl, x86_64 musl), the workspace suite, VMs, measurements.

## What the coordinator runs

Correctness, on KVM x86_64, KVM arm64 (nested, `SHARDS_L1_TESTS`), HVF arm64, WHP x86_64
and WHP arm64, with `SHARDS_LOG=debug` in the daemon's environment so that its log shows
`an engine ring attached` for each ringed connection:

```
cargo test -p shards --profile test-release --test engine_ring -- --nocapture a_microvms_engine_carries_bulk_whole_both_ways
cargo test -p shards --profile test-release --test engine --no-fail-fast    # every D181 test now runs through rings
cargo test -p shards --profile test-release --test isolation a_microvms_engine_port_answers_its_inits_token_alone
```

`ENGINE_RING_BYTES` sets the bulk test's size (default 64 MiB). Hostile offers, positions
and bytes are covered at the device by unit tests (`vsock::ring`, `vsock::conn::ringed`);
a live hostile-offer run needs a guest that knows the run's token, which only init has
(D181): a measurement branch's test-guest init would.

Measurement, the same hosts, two builds interleaved
(docs/research/measurements/build-ab/ab.py), the build before the ring
(`origin/p/vsock-credit-v1` or `p/integrate-engine-20261111`) and this branch:

```
ENGINE_RING_ROUNDS=11 ENGINE_RING_BYTES=268435456 \
  cargo test -p shards --release --test engine_ring -- --ignored --nocapture engine_ring_measurement
```

It prints n, p50, p90, p99 and max for 256 MiB engine to command (a joiner's log, kept on
the host, read back through the engine alone), 256 MiB command to engine (a joiner's stdin
through an attach, which also crosses the joiner's own exec channel, so its rate is the
lower of the two), and pings on one kept connection back to back and paced 2 ms apart (each
side asleep between them, for the cold path).

The decision's transport-level arm (section 14: the relayed path itself, as
vsock-path-v1's harness measured it, without the daemon's engine) needs a port that takes
rings in a directly booted VM; that is a measurement branch's knob (as
`SHARDS_VSOCK_CREDIT` was vsock-path-v1's), not product code: register the bench port as
`ServedPort::Ringed` in `vm_run.rs` with an owner that answers the offer with no token and
splices the connection to `<vsock path>_<port>`, and give the test guest's `vsock_bench` a
ring arm built on `crates/init/src/ring.rs`'s pump (include it by path).
