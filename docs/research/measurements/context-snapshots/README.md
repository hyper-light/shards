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

## Taking them as one version on APFS (PM M242)

`barriers.py` finds what waits out a write under way on APFS, where a file's times are
stamped as a write ends: each candidate call made while a writer's pwrite(2) is under way,
and directory clones made then, each checked for a torn file. `costs.py` times each way of
taking a tree of small files as one version each, phase by phase. `ab.sh` runs two builds'
`a_context_load_costs` and `a_take_costs` interleaved.

    python3 -I barriers.py /path/on/apfs 268435456 20
    python3 -I costs.py /path/on/apfs 2000 10
    sh ab.sh BEFORE/context-… AFTER/context-…
