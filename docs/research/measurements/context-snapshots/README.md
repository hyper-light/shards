# Context snapshots under writers (PM M100)

`torn.rs` measures the raw phenomenon on a file system: a reader that takes a file as
fstat, read, fstat, while a writer rewrites it one write(2) per version, sees versions
mixed, and the comparison of the two fstats misses some; on macOS it also reports what
the file's write count (getattrlist ATTR_CMN_GEN_COUNT) misses.

    rustc -O --edition 2021 torn.rs
    ./torn 4194304 4000 /path/on/the/file/system [PAUSE_US]

What shards' `host::Stage` makes of the same writers is in `crates/build/tests/context.rs`:
`snapshots_at_length` (every snapshot taken is checked whole) and `a_take_costs` (a take
of a 1 KiB file no one writes), both ignored with the tests:

    SNAPSHOT_TAKES=20000 cargo test --release -p shards-build --test context -- --ignored --nocapture
