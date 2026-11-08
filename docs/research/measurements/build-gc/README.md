# What a build's collection of its cache costs (PM M426, D212)

The harness is a unit test of the collection itself, `build::gc::tests::a_collections_cost_by_records`
(crates/shards/src/build/gc.rs), ignored unless asked for: a store of its own with N build
cache records of 1 MiB each, collected by the default policy (the user's 10%, 80% and 20% of
the disk, D186) `SHARDS_GC_ROUNDS` times (101 by default) where nothing is past its bounds,
as after most builds, each collection timed whole: its rules' reads of every record, the
disk's size and free space.

    TMPDIR=<a directory on the disk measured> SHARDS_GC_RECORDS=100,1000,10000 \
      cargo test -p shards --bin shards --profile test-release build::gc::tests::a_collections_cost_by_records \
      -- --include-ignored --nocapture

`TMPDIR` on the disk to measure: the store is made under `$TMPDIR/shards-tests/`, and a
tmpfs `/tmp` would measure memory. `results.txt` is the run PM M426 reports.
