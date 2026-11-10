# Typed local memory.max observation

This is a Reader primitive for ROOT's next real on-demand limit metric. It is
**not wired into Snapshot/export/callers and is not feature-ready**. ROOT owns
that wiring and its combined compilation/validation. Configured ceilings are
raw observations, not guarantees, reservations, effective ancestor limits or
available capacity.

Base/policy: main `a920c85b5c0c9d40aafc4f4cce32432c7c0aed79`, rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Implementer `01a1c5be192972d8b05dc7c207cc6fca`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Full AGENTS/R01–R16/evidence/CLAUDE/current
plan/global handoff read before code; POLICY_READ key
`managed-otel-memory-limit-policy-read-a920-v1`. OWN ONLY
`crates/init/src/metrics.rs`, its existing `metrics/tests.rs`, and this new
`docs/measurements/managed-otel-limits/**` path explicitly assigned by the user.
New sparse branch/worktree: `p/managed-otel-memory-limit-v1`,
`/Users/adalundhe/Projects/shards/.claude/worktrees/managed-otel-limits`.
Previous source/checkpoints are preserved; none was rebased or rebuilt.

## Interface and semantics

```rust
pub enum MemoryLimit {
    Finite(u64),
    Unbounded,
}

// Existing Reader; caller supplies the supervisor-held directory object.
pub fn memory_limit(&mut self, group: &File) -> Result<MemoryLimit, Failure>
```

The method uses the original `tokens`, fixed `memory.max` name, held-directory
openat/O_NOFOLLOW and checked `number` implementation. It reuses the Reader's
one fallibly reserved actual kernel-page buffer. The existing token grammar
bound already covers a u64 and `max`; no new production bound, parser/module,
cache, polling, dependency or platform adapter is added. Snapshot, codec,
run/daemon and production module declarations are unchanged.

| Actual input/result boundary | Result |
| --- | --- |
| A single unsigned decimal scalar | `Finite(value)`, including distinct finite zero and full-width u64. No signed narrowing or page renormalization of an observed value. |
| Exact lowercase kernel token `max` | `Unbounded`, not finite zero, MemTotal, a cached value or a resource grant. |
| Empty/whitespace-only contents | Existing `Failure::Missing`. |
| Malformed or additional scalar tokens | Existing `Failure::Invalid`. |
| Numeric/token overflow | Existing `Failure::Overflow`. |
| Missing file / symlink at fixed name | Existing `Failure::Io(ENOENT)` / `Failure::Io(ELOOP)` on the actual Linux regular-file cases. |

Whitespace follows the existing tokenizer. Reads of limit and usage remain
separate, non-atomic observations. This method reports this group's own file;
it neither walks ancestors nor computes effective/remaining entitlement.

Primary basis: the existing pinned
[Linux cgroup contract](https://github.com/gregkh/linux/blob/5bbb9c9f8f808710e2123f2b30f0d61d7d698f52/Documentation/admin-guide/cgroup-v2.rst#memory-interface-files)
defines memory.max and symbolic max. ROOT's
[OTel report](../../research/managed-runtime-otel-metrics.md) already maps a finite
`memory.max` to `shards.cgroup.memory.limit`, Gauge, `By`, with explicit unbounded
semantics. This batch implements only reading. Point omission/diagnostics,
timestamps, signed OTLP value bounds and caller generation/identity are ROOT's
later wiring obligations; none is silently defaulted here.

## Actual light validation and its limits

Private runner `8fadc1ec213332e16cb0d915a3fb913f`, Fedora kernel
`6.19.10-300.fc44.x86_64`, glibc2.43. Compile/test admission stayed
2 CPU / 2 GiB memory / 4 GiB disk / j2 / at most600 seconds / sync:false, in NEW
`/home/adalundhe/managed-otel-limits-a920-light-v{1,2}` roots, each with separate
`source`, `target`, `cargo-home`, `tmp`, `out`. These are admitted/requested bounds,
not measured resource peaks or independently proved kernel enforcement. No local
build, target or cache; no Root clone/target/cache, VM or controller access.

V2 actual results: format0, format-check0, native light compilation0,
**14 Reader tests passed / 0 failed / 0 ignored**, strict all-target light Clippy0.
Ten unrelated codec tests in the same binary were explicitly filtered, not passed.
Four added Reader cases exercise finite/zero/full-u64/max, missing/malformed/
duplicates/overflow, fixed-name missing/symlink behavior, actual-page crossings
and reuse of the same buffer while ordinary snapshot reads still return their
independent values. Buffer pointer/length/capacity assertions establish that
reuse in these cases; they are not allocator profiles or allocation-count claims.
The private TMPDIR had no remaining files/symlinks after tests. No FD-count,
kernel-cgroup cleanup, actual controller/population or whole-process leak claim
follows from that file witness.

Actual tools: rustc1.98.1 `48a229ceaefd4985c50990b14116b6d856af0985`,
Cargo1.98.1 `797e8a9bc`, rustfmt1.9.0-stable and Clippy0.1.98. This is the existing
light compiler used, not an enforced repository toolchain pin; rust-version is
an MSRV. The actual root `rustfmt.toml` max_width110 was included. No new runtime
numeric policy derives from a tool/default/benchmark.

### Preserved first failure and guard holds

V1 stopped at `cargo fmt` with exit1 **before Rust compilation**:

```text
failed to resolve mod `otlp`: .../source/docs/measurements/managed-otel-limits/../../../crates/init/src/otlp.rs does not exist
```

The full original diagnostic is [format.stderr](records/light-v1/format.stderr).
The cross-directory `#[path]` research wrapper did not preserve rustfmt's normal
submodule lookup. No source file was changed by that failed formatter. Two
subsequent readback requests were refused before start because the runner had
zero idle CPUs; originals were preserved. A later inventory showed three idle
CPUs and the original archive was retrieved with the same earlier envelope.
ROOT then explicitly assigned a distinct read-only envelope of
1 CPU / 256 MiB / 1 GiB, used for V2's small archive retrieval. No compilation or
test ran in that read step; no guard bypass or larger fallback occurred.

V2 changes only the **private harness layout**: run-light.py generates the exact
101-byte `lib.rs` wrapper beside the private copy's existing `metrics.rs` and
declares `pub mod metrics;` normally. Cargo's test-only lib target points there;
the original Reader/codec/test modules are not rewritten or duplicated. The
testdir manifest likewise imports the actual unchanged testdir library source,
without running its unrelated own test suite or helper builder. No generated
wrapper is added under the repository's production source directory. Reader/test
bytes were unchanged between V1 and V2; their changed behavior was not guessed
from the harness error.

## Exact sources, commands and artifacts

| Identity | Bytes | SHA-256 |
| --- | ---: | --- |
| Compiled `crates/init/src/metrics.rs` | 9907 | `0b00dbbe509a768c60a5c24d2a7892d11a37dab5e9e32fef50ca6a5346cb4515` |
| Compiled `crates/init/src/metrics/tests.rs` | 11819 | `d77238f28651ca84f9f7c059920e3c21649c462b206712cef19a9d3f98b733cb` |
| Unchanged codec | 16634 | `0bb86b2f172e12b8053cf30333ccbabd1a6963a315836a639c79de5a4458f28c` |
| Test-only Cargo.lock | 496 | `3c76154b7e5d87f0518803c46299b0187006bc9e9e020f7f6e9facefaa5dab73` |
| Actual V2 native test binary | 7579032 | `e3084f49f5627651915e52c5ec5af2fdecab588c3dc95edbfb18136276665c36` |
| V1 raw archive | 3506 | `c1f05d8a6b6837eca452ed8f95cf681f3929fdfb81c02241c8a150252d90a7e6` |
| V2 raw archive | 11971 | `91b536596d9f24322e4c90f8f1ac84d9552f7141133cdfa0c825ec7ebbf0cc23` |

The native binary is V2's
`target/debug/deps/shards_managed_otel_limits_light-3c25f2e21fff10a4` under the
private root above. Its hash was recorded before execution and rechecked after
Clippy. It is a light test artifact, not an installed binary or a production
caller result. Actual commands, private environment, generated source identity,
tools and outcomes are in [V2 result](records/light-v2/result.json); literal
bootstrap/source closures and all holds/failures are under `records/light-v{1,2}`.

Selected test invocation was the exact binary with
`metrics::tests:: --test-threads=1`; build was
`rustup run 1.98.1 cargo test --manifest-path docs/measurements/managed-otel-limits/Cargo.toml --lib --no-run --message-format=json -j2`.
Clippy used the same manifest, `--locked --all-targets -j2 -- -D warnings`.

To reproduce, prepare a **new private root** with the retained source-package
archive extracted into its `source/` (safe regular member hashes are recorded),
then from that private root run
`python3 source/docs/measurements/managed-otel-limits/run-light.py` under the same
admission. The script generates its crate root only inside that private copy.
Do not run this staging driver against a live production worktree or reuse ROOT
targets. The wrapper/manifests are proof-only; actual init wiring remains separate.

Both raw archives were transferred intact and verified: V1 has20 safe unique
regular members/19 manifest-covered originals; V2 has36/35. Each manifest excludes
itself. Raw compiler/libtest whitespace was not normalized. No measurements were
executed; automatic Cargo/libtest diagnostic durations remain raw log text, not
performance samples. Owned sources/raw/binaries remain preserved; no cleanup of
caches/targets by this lane occurred.

## R01–R16 and owed ROOT gates

| Rule | Scoped evidence and boundary |
| --- | --- |
| R01 | Existing fallible page allocation, checked numeric parser, bounds-checked token handling and RAII file ownership reused. New method adds no assertion/unchecked indexing/unwrap/panic or new allocation API. Strict light Clippy0 is not an OOM proof for the whole caller. |
| R02 | Pinned kernel interface and existing OTel research/caller requirement; actual native regular-file corpus. No architecture, collector or entitlement policy was selected here. |
| R03 | Only the researched fixed control name and exact symbolic max are new protocol literals. Existing u64/token/page bounds reused; no new budget, timeout or production threshold. |
| R04 | Explicit Finite/Unbounded/existing Failure; missing and malformed are not success. No sentinel zero grant. |
| R05 | One method on the existing Reader for ROOT's specified on-demand caller; no alternate parser/state owner/cache/poll. Actual wiring still owed. |
| R06 | One bounded scalar token fold; no framework or OS-specific duplicate. |
| R07 | Type comment records the non-entitlement meaning; evidence records local/hierarchical and proof boundaries. |
| R08 | No performance/superiority claim. |
| R09 | New observation requirement, not an alleged existing production defect. Exact private fmt red retained before its harness-only correction. |
| R10 | Source/package/command/binary/raw identities, exact failed and passing results, typed boundary cases and private file cleanup retained. No actual-kernel mutation/cleanup proof claimed. |
| R11 | Actual x86 GNU light check only; shared Linux guest code. Full init GNU/musl/architectures and host/backend gates are ROOT's, unrun here. macOS arm64 only; paused WinARM untouched. |
| R12 | Existing Reader/openat/tokenizer/shared guest implementation; no installed binary, production dependency or platform API change. |
| R13 | Reader type/method plus its actual requested tests only. Proof manifests/wrapper do not change production registration or schema. |
| R14 | No allocation/fault/CPU/latency profiles or benchmarks; buffer identity is not such a profile. ROOT owns all measurements/attribution. |
| R15 | Scoped implementer, owned private light and distinct read envelopes, no children/goals/audit/campaign/main merge. Signed named-path checkpoint with required attribution. |
| R16 | Local floor was214634512KiB free before setup, above30GiB. No local build/target/cache or protected-cache write/deletion. First failures/own sources/records/binary preserved. |

ROOT must wire the real on-demand observation and later Gauge/omission/diagnostic
contract, resolve the temporarily unused interface in that combined source, and
run actual full-caller, controller/identity/generation/cleanup, supported-target,
collector and applicable guard/restoration gates. This light harness publicly
imports the module; its clean Clippy does **not** assert that unwired init has no
unused-interface warning. No broad/repeated test or production optimization is
substituted for those owed gates. Full G25/P0–P13 and parent entitlements stay open.
