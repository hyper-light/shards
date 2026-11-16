# Remote review fixes, 2026-11-13

POLICY_READ: Shards base `9efde72524dd626f3ff84e6b9891852d7c810a9b`, role implementer, remote-04 through remote-08 only. ROOT owns P0s, real VM validation, audits and profiling. The fix brief and brief-common were read in full. Policy SHA256: AGENTS `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41`; rules `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`; evidence `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63`; CLAUDE `588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010`.

## remote-05: ordinary logs do not carry request values

Red source: `144818a4a88d144176552da831a0faebf56faed8`.
Fix source: `68b43042c111dbd7696e16fa5b8020993de9d274`.

Runner: `adas-mac-studio.local`, macOS arm64, Rust 1.98.1, CARGO_INCREMENTAL=0, CARGO_BUILD_JOBS=4, private target `~/g121-tmp/fix-remote-target`. A tiny git-bundle carrier checks out each source revision in `~/g121-tmp/fix-remote-source`. Guest payloads are prebuilt artifacts, unused by these host unit tests; SHARDS_HELPERS=skip. No real-VM behavior is validated by these tests.

Red command (Kiln r6):

```sh
cargo test -p shards --bin shards daemon::remote::tests::remote_requests_never_log_argument_values -- --exact --nocapture
```

Observed: 0 passed, 1 failed; `private request value entered the log` at daemon/remote.rs:1737. The test invokes actual Machine::start with an enrolled fixture identity, refuses a raw env-file argument before spawning any command, and captures the daemon diagnostic on stderr. A subprocess isolates log capture; the sentinel is synthetic. This reproduces logging itself, not a mocked formatter. The original client-expanded environment/guest scenario remains ROOT's live check.

Change: remove raw argv from ordinary command, copy, move, context, policy, signature and refusal diagnostics. Remove relay tokens from directory-serving diagnostics. Keep client identity, operation kind and safe refusal cause. A static operation description avoids a partial redaction policy that would need to identify every credential-bearing flag or guest argument. No timing, size or resource constants were added. D180 records this contract.

Green (Kiln r10, same runner and fix source):

```sh
cargo test -p shards --bin shards daemon::remote::tests -- --nocapture
cargo clippy -p shards --all-targets -- -D warnings
```

Observed: 5 passed, 0 failed (including the red regression); clippy exit 0. Runner Darwin 25.2.0 arm64. Logs retained on the runner under `/Users/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/fix-remote-carrier-a63d6bf735/{red,green}.log`. Merged tree `70161aaa5528162c687a0579923c1fad542ea268` (fresh main `53d5cea99`) passed the same 5 tests and clippy again in Kiln r14; log `merged05.log` beside the earlier logs. ROOT explicitly authorized the secret-exposure landing ahead of the P0 function-order rule through the correlated local peer reply. Landing follows this evidence update. No mutation campaign or live guest scenario was run.

## Rule/evidence accounting for remote-05

- R01/R04: production log changes add no indexing, unwraps, new refusal or lifecycle state. Existing refusal behavior is retained; private arguments no longer enter diagnostics.
- R02/R03/R05/R12/R13: focused source-proven information leak; retain existing logging seam, omit private data instead of inventing a parser/redaction abstraction. No new production literals selecting timeouts/sizes, caches, waits, platform adapters or dependencies.
- R06/R07: remove argument-rendering branches/temporary vectors; one comment states the non-obvious information boundary.
- R08/R14: no performance claim or benchmark; ROOT retains profiling/comparative validation.
- R09/R10: exact failing unit invocation and source above, green above; no separate mutation run (the failing test commit retains the old behavior). The defect is confirmed from the actual log, not inferred from a source assertion.
- R11: shared implementation, tested on one macOS arm64 host; Linux/Windows and real VM expansion-to-guest scenarios unrun, owed to ROOT/CI.
- R15/R16: signed scoped commits; no local builds, no delegation, no force push; test target cleanup due after landing. The original review's read-only restriction was superseded by the explicit fix task.

ROOT owed: an isolated real context run with synthetic inherited `-e NAME`, env-file, exec env and build-arg markers; prove guest receives each marker and daemon.log omits it on success/refusal. Retain actual command/revision/logs in the real-VM campaign. No such run was started here.


## remote-04: build-file timestamp validation

Red source `c3ad61ab109ae4d507874683d4b6da96f507bde3`, based on fetched main
`9068a72cd` containing remote-05's landing. Policy files unchanged from the receipt
above (checked by Git diff). Fix `063a5d339f3e6c6691621fb4bd437475957b4910`.
Same macOS arm64 runner/toolchain, carrier, target and unused guest artifacts as
remote-05; no local build.

```sh
# Red, Kiln r18
cargo test -p shards --bin shards synced::tests::malformed_build_file_time_is_refused_before_the_file_is_created -- --exact --nocapture
# Green, Kiln r19
cargo test -p shards --bin shards synced::tests -- --nocapture
cargo clippy -p shards --all-targets -- -D warnings
```

Red: 0 passed/1 failed, `overflow when adding duration to SystemTime` at
synced.rs:701, directly in the real receiver called with an i64::MAX/u32::MAX
network time, before any archive is needed. Green: all 9 synced tests passed;
clippy exit 0. Logs `red04.log`, `green04.log` beside the remote-05 logs on the
runner. The regression now requires the typed malformed-nanoseconds cause and
asserts that the named file was never created.

The receiver validates that the nanosecond fraction contains no whole second
(utimensat(2)'s tv_nsec/SI fractional-second definition) and uses the standard
library's checked SystemTime operation for the actual host range. No baked-in
platform maximum, floor, timeout or learning seed is introduced. The writer takes
a checked SystemTime, so it cannot recreate the unchecked addition. Existing
pre-epoch normalization remains. `ReceiveError::Timestamp` retains its
`FileTimeError` cause until the existing context-step stderr presentation; no
caller parses refusal text. Legacy context parse errors remain the existing
messages under the Context variant; this change does not claim a whole-wire error
migration.

R01/R04/R06: checked conversion and explicit typed failures before file effects;
R02/R03/R05/R12/R13: existing receiver/writer seam, protocol fraction and host
representation bound, no duplicate platform implementation/cache/wait. R07 cites
the non-obvious bound; R08/R14 no performance claim. R09/R10 actual receiver red
and green above; R11 shared code tested on macOS only, Windows-specific range
assertion is compiled only there and remains unrun here. R15/R16 scoped signed
commits and private target, no E2E/VM/profiling. D180 records the decision. Merge
validation/landing pending.

ROOT owed: `cargo test -p shards --bin shards synced::tests` on jupiter (Windows
host range), and the real-context suite `cargo test -p shards --profile
test-release --test context` on the release-capable context fixture. The existing
suite needs the malformed raw request case added to assert that an abort-profile
listener survives and serves another client; the unit's unwinding test process
does not establish that runtime cleanup. No such listener/VM run was started here.
