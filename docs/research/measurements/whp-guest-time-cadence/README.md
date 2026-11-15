# A managed guest's vCPU guest time, against its VM process's CPU time, and how often it moves (PM M601, D265)

Two probes, each a test of `crates/shards/tests` copied in for its run and removed after:

- `whp_probe.rs`: a `--host-managed` one-vCPU guest spins 5 s, then halts 6 s; `USE` every
  500 ms prints its guest time, its VM process's CPU time (as its `host` line says it) and its
  charge (`Charged`), each span's growth as a share of the wall.
- `whp_cadence.rs`: the same guest spinning 8 s; after 1.5 s, `USE` back to back for 2 s:
  each ask's latency, and the intervals between the asks that saw its guest time move, and
  how far each move went, n p50 p90 p99 max.

Run, from a checkout at the revision measured, with a pinned kernel fetched by the tests:

    cp docs/research/measurements/whp-guest-time-cadence/whp_cadence.rs crates/shards/tests/
    SHARDS_REQUIRE_VMS=1 cargo test --locked --profile test-release -p shards --test whp_cadence -- --nocapture --test-threads=1
    rm crates/shards/tests/whp_cadence.rs

Hosts: hex-winvm (Windows Server 2025 Standard Evaluation 24H2, build 26100.32230, nested
WHP on KVM, the classic scheduler, event 2 0x1, 12 vCPUs) and jupiter (Windows 11 Pro
10.0.26200, the root scheduler, event 2 0x4, AMD Ryzen AI Max+ 395). Results: PM M601.
