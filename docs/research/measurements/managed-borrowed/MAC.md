# Native macOS arm64 light checks, including full-lib RED

Source `c5c8e4c658e9b0d61fe4ce433a5e457979b899bb` (production53535ab6),
MacBook arm64/Darwin25.4.0/macOS26.4.1, pinned Rust1.98.1
`48a229ceaefd4985c50990b14116b6d856af0985`, LLVM22.1.8. All commands ran from
the managed-borrowed worktree under `/usr/bin/lockf -t 0 /tmp/g121-mac.lock`,
cargo-j4, private target `/Users/adalundhe/.cache/g121-managed-borrowed-mac-target`.
Root explicitly authorized these scoped local checks after the Linux4CPU request
was rejected. No Linux ask was reduced/repeated and no Mac result substitutes for
that exact Linux gate. No real VM, profile, audit or repeated native Windows run.

Actual free space before:41,040,998,400 bytes (~38.2GiB), final:40,893,206,528 bytes
(~38.1GiB), above the owner30GiB floor. Source/toolchain/floor/raw logs are in
[mac-c5c8](mac-c5c8/). Build used the normal `scripts/hvf-run` test runner.

| Command | Actual result |
| --- | --- |
| `cargo test --locked -j4 --profile test-release -p shards-api --lib -- --test-threads=1 --nocapture` | **RED, SIGABRT** at new owned_connection_oom test: allocation64B failed; full lib did not finish |
| Same lib command with `borrowed::tests` filter |12 pass,0fail,1 private child role ignored |
| Same lib command with `stream::tests` filter |11 pass,0fail, including ready queued byte at1ns timeout |
| `cargo test --locked -j4 --profile test-release -p shards-api --test sim -- --test-threads=1 --nocapture` |15 pass,0fail,2 existing measurement cases ignored |
| `cargo clippy --locked -j4 -p shards-api --lib --test sim -- -D warnings` |0 |

The full-lib failure is retained in [lib.log](mac-c5c8/lib.log). Independent
passes are [borrowed](mac-c5c8/borrowed-focused.log),
[stream](mac-c5c8/stream-focused.log), [sim](mac-c5c8/sim.log),
[clippy](mac-c5c8/clippy.log). They do not make the aborted full corpus green.
The full run passed owned zero/MAX and legacy refusal OOM before reaching the
owned test that aborted. The independent borrowed filter covers actual no-CONNECT
zero/MAX overflow, held identity, child death before OK, child response after
stream drop, exact bytes/clone/half-close, withheld/dribbled reply, malformed EOF,
and fallible shared-state/box/clone failure. No Mac handle/FD leak claim is inferred.

## Exact binary and owned-OOM replay

Binary:
`/Users/adalundhe/.cache/g121-managed-borrowed-mac-target/test-release/deps/shards_api-1004f0f8fcc190b7`
SHA256:
`46054f6fa67730987529b54eb73b9a6bb0ca8f119828c5c9b88fddb312dade9d`.
The hash was rechecked after the final stream test run. No source/test code changed
after compilation; later evidence-only commits do not change these input bytes.

Exact single-test invocation for the coordinator, under the same lock/floor:

```sh
scripts/hvf-run /Users/adalundhe/.cache/g121-managed-borrowed-mac-target/test-release/deps/shards_api-1004f0f8fcc190b7 \
  --exact client::tests::owned_connection_oom_is_typed_before_error_formatting \
  --test-threads=1 --nocapture
```

Or rebuild from this checkout using the same target and pinned compiler:

```sh
export CARGO_TARGET_DIR=/Users/adalundhe/.cache/g121-managed-borrowed-mac-target
cargo test --locked -j4 --profile test-release -p shards-api --lib \
  client::tests::owned_connection_oom_is_typed_before_error_formatting \
  -- --exact --test-threads=1 --nocapture
```

## Cold64B source chain and uncertainty

The all-after-zero test allocator returned null, and the process actually aborted
on64B instead of completing the intended typed Socket allocation failure. This does not establish
the original stack caller. Primary-source inference, explicitly still awaiting
root's independent fresh-primitive control:

- [Rust1.98.1 mutex selection](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/std/src/sys/sync/mutex/mod.rs)
  selects pthread for Darwin (Linux/Windows take futex).
- [pthread Mutex::get](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/std/src/sys/sync/mutex/pthread.rs)
  initializes its OnceBox with `Box::pin(pal::Mutex::new())` on first use.
- [pal Mutex](https://raw.githubusercontent.com/rust-lang/rust/1.98.1/library/std/src/sys/pal/unix/sync/mutex.rs)
  wraps UnsafeCell<pthread_mutex_t>. Local SDK
  `/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/include/sys/_pthread/_pthread_types.h`
  defines LP64 mutex storage as one8-byte long +56 opaque bytes =64B.
- Existing owned Shared constructs a cold std Mutex, and Guest::connect locks it
  before the new fallible Socket path. A watcher may race that first use. The
  observed64B matches this cold primitive's source-derived layout; actual stack
  attribution remains owed.

The source chain and observed size support that inference; neither is an actual
stack trace or frequency measurement. Production synchronization was not redesigned
and the fixture was not warmed/disabled to call the gate green. Root will run the
isolated cold std Mutex control and own any broader remedy/research. The borrowed
connector itself creates no std Mutex or owned Guest. No whole-library panic-free
or ready-main statement is made while the full-library RED remains.

Linux raw r176/r184/r185/r186 evidence remains preserved in
`~/managed-borrowed-evidence` on the assigned private runner. Fetch/commit the
remaining raw files centrally when admission returns; do not delete that evidence.
Exact535 Linux complete light run, seven-target/Windows and real-VM gates remain
owed as listed in [CANDIDATE](CANDIDATE.md).
