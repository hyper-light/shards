# shards-api's flaky library tests, reproduced and fixed

POLICY_READ: implementer, ROOT g121-46's lane (2026-11-12). origin/main ad600dd52's
docs/development-rules.md, sha256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.

Failures the step 9 agent met in shards-api's library tests at main a7dbf1446, each
reproduced here before its fix (R09) and measured again after (R10), each with its record
in `docs/research/platform-measurements.md`.

## The harness

- `runs.sh build SRC LABEL [TEST]` builds a checkout's library test binary, or its
  integration test TEST's, once (`cargo test -p shards-api --lib|--test TEST --profile
  test-release --no-run`), in a target directory of LABEL's own, and keeps a copy with its
  sha256. A target shared by two checkouts kept the first one's binary for both: cargo's
  fingerprints compare mtimes, and a patch applied before the first build is older than
  its output (round 1 on jupiter, discarded).
- `runs.sh loop LABEL N ARGS...` runs that copy N times with libtest's ARGS from the
  checkout's `crates/api`, as cargo does, keeping each failed run's log and a line a run
  (number, exit code, seconds, failed tests) in `summary.txt`. A run past 120 s is ended
  and its tests still running are named `HUNG:`.
- The runners' checkouts: `~/api-flakes-src`, a clone of origin; a git worktree per build
  under `~/api-flakes-wt/LABEL`, at the base revision with the build's patch applied
  (its `git diff`'s sha256 in `~/api-flakes/LABEL/PATCH.sha256`).

## Windows: a second GPU report of one scope refused ERROR_PIPE_BUSY (PM M533)

Host: jupiter (Windows x86_64, 32 CPUs, kiln runner, shared with CI jobs: 0 to 28 of 32
cores idle as these ran), rustc 1.98.1 (the toolchain pin).

| build | revision | binary sha256 | args | runs failed |
|---|---|---|---|---|
| red | ad600dd52 | 37403dd70d9146dc… | `physical::report` | 40 of 40 |
| red | ad600dd52 | 37403dd70d9146dc… | `physical::report --test-threads=1` | 0 of 40 |
| red | c2c4547c1 | 539508ec9cb32d72… | `physical::report` | 40 of 40 |
| red + the new test | c2c4547c1 + its hunk | 1884f49d04e9f507… | `--exact …reports_of_one_scope_at_once_each_transfer_their_own_frame` | 20 of 20 |
| green | c2c4547c1 + the fix (patch 3b29fa99…) | da57d4eebc627852… | `physical::report` | 0 of 200 |
| red | c2c4547c1 | 539508ec9cb32d72… | the whole library, runs interleaved with green's | 11 of 40: 8 these, 3 hung |
| green | c2c4547c1 + the fix | da57d4eebc627852… | the whole library, runs interleaved with red's | 4 of 40, all hung |

Every red failure, 130 at ad600dd52 and every one after, was
`ManagedIo { operation: "GPU report pipe", kind: Uncategorized, code: Some(231) }`:
`CreateNamedPipeW` refused, ERROR_PIPE_BUSY, not the ERROR_ACCESS_DENIED that
FILE_FLAG_FIRST_PIPE_INSTANCE's documentation names for a second instance. The hung runs
were all `client::borrowed_tests::exact_bytes_clone_and_half_close`, ended at
`runs.sh`'s 120 s, red's and green's alike: another fault, measured apart.
