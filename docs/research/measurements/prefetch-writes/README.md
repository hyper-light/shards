# What a warm VM's written-page prefetch costs (PM M62)

HVF's prefetch writes, in guest context, each page its working set says the guest wrote,
so the copy the guest's first write would make is made before the request (audit D02).
`writes.patch` (against 8e00eec) lets `SHARDS_PREFETCH_WRITES=0` prefetch every page as a
read instead. The fleet measurement (`docs/research/measurements/fleet/fleet.rs`) then
runs with each, alternating:

    git apply docs/research/measurements/prefetch-writes/writes.patch
    SHARDS_PREFETCH_WRITES=1 FLEET_SIZES=10 cargo test --release -p shards --test fleet -- --ignored --nocapture
    SHARDS_PREFETCH_WRITES=0 FLEET_SIZES=10 cargo test --release -p shards --test fleet -- --ignored --nocapture
