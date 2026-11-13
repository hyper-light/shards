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

## macOS: the GPU probe's test, Pending (PM M534)

Host: adas-mac-studio.local-2 (M3 Ultra, 32 CPUs, macOS 26.4), kiln's shell (priority 20,
nice 10) and launchd jobs of the user's session (`macos-sched/launchd-run.sh`, priority 31),
under its co-tenants. `first-exec.py BASE ROUNDS` times a new script's first start against
its second, a new copy of /usr/bin/true, and /bin/sh reading a new script.

| build | binary sha256 | test | kiln's shell | launchd |
|---|---|---|---|---|
| main c2c4547c1 | 56a22432458a90ca… (lib) | alone | 9 of 100 | 89 of 100 |
| main c2c4547c1 | 56a22432458a90ca… (lib) | the whole library | 37 of 100 | 12 of 100 |
| main 99d88fccf | 062247244d680404… (lib) | alone | 14 of 100 | 9 of 100 |
| main 99d88fccf | 062247244d680404… (lib) | the whole library | 32 of 100 | |
| fix (patch 71b88f54…) | 2eea2873dbd03eb2… (sim) | alone | 0 of 200 | 0 of 200 |
| fix less `--warm` (patch 70959f71…) | 5639db51a90a5f21… (sim) | alone | | 0 of 200 |
| fix less `--warm`, at c2c4547c1 | f149a175382fae3b… (sim) | alone | 0 of 200 | 1 of 200, past 8.8 s |
| fix with a guest's bound (patch 32f2790e…) | 293307ee78953771… (sim) | alone | | 0 of 200 |

## macOS and Linux: the zero-patience test's socket, held open (PM M535)

`spawns.rs` (`client::spawns::a_refused_sockets_end_beside_spawns`, ignored) runs the test's
check `SHARDS_API_SPAWN_ROUNDS` times (5,000) beside processes `SHARDS_API_SPAWNS` names
(`none`, `sleep`, `fresh`). Builds at main 99d88fccf, macOS (adas-mac-studio.local-2):

| build | binary sha256 | the whole library, runs failed in this test |
|---|---|---|
| main (lib) | 062247244d680404… | 8 of 100 |
| fix (patch ed2c72a2…, with M534's) | d74424e668239cf2… | 0 of 200 (kiln's shell), 0 of 100 (launchd) |
| fix, its check in-process (patch 4c67b9e8…) | b6fe4d93540766d5… | 2 of 100 |

Linux (adacachy2), the whole library, 20 runs each: main db72dc4ccbf15e34… 1 failed;
the fix 03c5ca3492b4e15b… none.

## Windows: a blocking AF_UNIX read sleeping through its peer's end (PM M536)

`hang.sh LABEL N LIMIT ARGS...` runs a build's test binary N times and, at LIMIT seconds,
has cdb (WinDbg's console debugger, `winget install Microsoft.WinDbg`; `cdbX64.exe`)
attach noninvasively and print every thread's stack into `stacks-RUN.txt` before it ends
the run; symbolize them from the build's PDB with `cdbX64.exe -z api.exe -cf` a script of
`.sympath <deps dir>`, `.lines -e`, `.reload /f` and `ln`/`lsa` of each `api+0x…`.
`half-close.rs` (`half_close::a_waiting_reads_end_beside_its_peers_half_close`, in
shards-afunix's tests, ignored) is the read-beside-an-end measurement,
`SHARDS_AFUNIX_ROUNDS` rounds a way (2,000).

On jupiter, beside ROOT's WHP gate (CPU load 29% to 100%), at main 99d88fccf with
patches `hc-red` (the measurement), `hc-green` (the fix), `hc-mutant` (the fix less its
WSAPoll before a read with no timeout): shards-api's library c2d0c8c577d98bd8…,
c55ed033cd182014…, 4bda611df6bb80c0…; the flaky test alone with a 20 s limit, red 7 of
300 hung, green 0 of 1,000, mutant 26 of 300; the fix's whole library, 0 of 20.
