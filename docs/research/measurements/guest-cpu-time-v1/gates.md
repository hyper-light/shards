# Gates run by the implementer (light checks only)

Runner: adacachy (kiln 451397fcc585f7f7cb9aaf00e42064bb): Linux 7.2.9-2-cachyos x86_64, rustc 1.98.1
(48a229cea 2026-09-01), zig 0.16.0, `SHARDS_HELPERS=skip`, `-j4`, `systemd-run --user --scope -p
MemoryMax=16G`, amphora-bench not running. Its CLK_TCK is 100, with CONFIG_HZ=1000,
CONFIG_VIRT_CPU_ACCOUNTING_GEN=y and CONFIG_NO_HZ_FULL=y, and no nohz_full CPUs asked for. The tree is this
branch's working tree as synced by kiln, base 032b63b1c.

| Run | What | Result |
|---|---|---|
| r569 | `cargo check -p shards-vmm --all-targets`, x86_64-unknown-linux-gnu (KVM) | clean |
| r586 | `cargo fmt --all --check` | failed: rustfmt layout in guest_time.rs and runtime.rs. Fixed with `cargo fmt --all` |
| r586 | clippy `-D warnings`, x86_64-unknown-linux-gnu, shards-vmm, shards-ipc, shards, shards-vm-process, `--all-targets` | failed: 5 lints in tests/guest_time.rs's helpers (unwrap, eprintln, indexing), which only `#[test]` bodies are spared. Fixed with the file-level allow every E2E file has |
| r586 | clippy `-D warnings`, x86_64-unknown-linux-musl, shards-testguest | clean |
| r586 | `cargo test -p shards-vmm --lib -- guest_time platform::unix::usage_tests hv::tests` | 18 passed (before the KVM rework below) |
| r586 | `cargo test -p shards-ipc --lib` | 54 passed, 5 ignored (their own harness), `every_message_kind_has_its_own_number` among them |
| r586 | `cargo test -p shards --bin shards -- engine::watch` (dev profile) | did not link: zig-cc's dev-profile C calls UBSan's runtime (`__ubsan_handle_*`). The repository runs tests under `--profile test-release` (CLAUDE.md) |
| r631 | `cargo fmt --all --check` | clean |
| r631 | `cargo test --profile test-release -p shards --bin shards -- engine::watch` | 2 passed |

The KVM rework came after r586: the clock reads each time by the vCPU's TID instead of a stat file opened at
vCPU setup, which was on the start path. Its gates follow.

## The merged tree, 9c2a671cb (this branch on origin/main c4ae59ee0), 2026-11-12

kiln's server stopped answering through the session's restart, and its syncs of this worktree timed out. So the
tree went to adacachy as a git bundle (`git bundle create gct.bundle origin/main..HEAD`, 50,783 bytes). It was
checked out in a scratch clone there (`~/g121-tmp/gct-check`, sharing objects with kiln's earlier copy of this
worktree, whose target directory it built into) and run over ssh. The runner's guards held: amphora-bench idle,
`systemd-run --user --scope -p MemoryMax=16G`, `-j4`, `TMPDIR=~/g121-tmp`. Linux 7.2.9-2-cachyos x86_64,
rustc 1.98.1.

| What | Result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo test -p shards-vmm --lib -- guest_time platform::unix::usage_tests hv::tests` (x86_64-unknown-linux-gnu) | 18 passed |
| `cargo test -p shards-ipc --lib -- every_message_kind` | passed, with main's MEMBER_ENDED at 87 and GUEST_TIME, GUEST_TIMED at 88, 89 |
| `cargo test --profile test-release -p shards --bin shards -- engine::watch` | 2 passed |
| clippy `-D warnings --all-targets`, shards-vmm, shards-ipc, shards: x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu | clean, both. The E2E file tests/guest_time.rs is among the targets |
| clippy `-D warnings --all-targets`, shards-testguest: x86_64-unknown-linux-musl, aarch64-unknown-linux-musl | clean, both |
| clippy `-D warnings --all-targets`, shards-vmm: x86_64-pc-windows-msvc, aarch64-pc-windows-msvc (WHP) | clean, both. The Windows target std was added to adacachy's 1.98.1 toolchain for this. vmm builds no C for Windows |
| clippy `-D warnings --all-targets`, shards-vmm: aarch64-apple-darwin (HVF), from Linux | clean. Its target std was added too. The build script's C shim (`simd.c`) needs the macOS SDK's Hypervisor.h, so `CC` was a stand-in writing an empty object. A check links nothing: every Rust line of the HVF backend, its tests among them, was type-checked and linted |
| clippy `-D warnings`, shards-vm-process (x86_64 Linux) | fails, on main's own code: `spec::has_vm_flag` (from the Hex GPU patches) is used only by `cli/`, which the VM process's binary does not include, so it is dead there. Not this branch's (its diff touches neither spec.rs nor crates/vm-process). Reported to ROOT. warm.rs and vm_run.rs, which this branch changes, are linted inside the `shards` crate above |

The Mac Studio runner (adas-mac-studio.local-2) was reachable only through kiln, whose server kept timing out. So
macOS's unit tests (the `mach_ns` test; `guest_time` and the stat parser run there too) and a native macOS
clippy are owed, beside the real-VM runs.

### Guard mutations (R10), on the merged tree

`docs/research/measurements/guest-cpu-time-v1/mutants.sh`, run in the scratch clone on adacachy under the same
guards ([mutants.log](mutants.log)). Each mutation removes one guard, runs the unit test that holds it, and is
restored from git and touched. All 10 were killed by the test meant for them, and none survived:

- a vCPU's time may fall;
- a leaving vCPU's time is dropped;
- a stale guard removes a newer vCPU of its index;
- a replaced vCPU's time is dropped;
- an unread vCPU says nothing of why;
- decoding takes vCPUs out of order;
- encoding keeps a reason's line breaks;
- the guest time is field 42;
- the name ends at its first `)`;
- WHP's guest time counts the hypervisor's.

The HVF reader's wait and the vCPU's publication have no unit-level mutant: they need a vCPU. The HVF ignored
test and the real-VM test hold them (owed).

## CI's arm64 failure of the sampling test, and its fix (2026-11-12)

CI's run 43230466376 on main failed
`vm::guest_time::tests::samples_never_see_the_total_fall_as_vcpus_come_and_go` on both arm64 Linux jobs
(aarch64-unknown-linux-gnu 131224103882, aarch64-unknown-linux-musl 131224103787). It panicked at
guest_time.rs:406 with `GuestTimes { vcpus: [], departed_ns: 0 }`. Both jobs ran
`cargo test --locked --workspace --profile test-release --no-fail-fast`, and this test was one of the crate's 483,
run beside each other. (The run's aarch64-pc-windows-msvc job failed elsewhere: tests/engine_ring.rs did not
compile on Windows, with an unused import and a dead `held_open`, D230's.)

**The cause.**
- Every round a vCPU thread finishes adds its last stored time, at least 50, to `departed`: its leave reads its own
  clock, on its own thread, after its last store. So a `departed_ns` of 0 means no thread had finished a round,
  nor started one, since a round has no early exit.
- The test sampled a fixed 2,000 times and then set `done`. Optimized, 2,000 samples of an empty registry take
  microseconds, and the four threads were spawned onto idle CPUs that had not yet run them. Each found `done` set
  at its first look, and came and went never.
- The registry holds all its state under one `Mutex`, so no memory ordering can lose a finished round: this is the
  test's race, not the product's. The other atomics this work added are HVF's `Ran`, whose stores go before its
  SeqCst `stints` bump and whose loads come after the reader's SeqCst load. A unit test can't reach them, and they
  are not in this failure.

**Red, live, on arm64 Linux.**
- Host: the Lima VM g121-winarm on this Mac: Ubuntu 26.04.1, Linux 7.0.0-38-generic aarch64, 6 vCPUs, its load
  1.5 to 3.6 from the Windows container it hosts.
- Binary: main's test, built on adacachy for aarch64-unknown-linux-musl (static, `rust-lld`) from origin/main
  7ad28fab1, and run in the VM at `nice 19`, `--exact`, 1,000 times (`target/gct-bins/loop.sh`, not kept).

| main's test (7ad28fab1) | conditions | runs | failed |
|---|---|---|---|
| dev profile | the VM's 6 vCPUs | 1,000 | 0 |
| dev profile | pinned to one CPU (`taskset -c 0`) | 1,000 | 0 |
| dev profile | pinned to one CPU, beside a niced busy loop on it | 1,000 | 0 |
| test-release, as CI | the VM's 6 vCPUs | 1,000 | **4**: the first, run 513, `panicked at crates/vmm/src/vm/guest_time.rs:406:9: GuestTimes { vcpus: [], departed_ns: 0 }`, CI's own symptom |
| test-release | pinned to one CPU | 1,000 | 0 |

**The fix** (e4b43764a, test only).
- Sample until every vCPU has come and gone 100 times (`ROUNDS`), yielding between samples, however late their
  threads first run. Its failure message says each one's rounds.
- A guard sets `done` however the sampling ends. Before, a failed check's panic left `thread::scope` waiting for
  ever on threads that never stopped.

**The cause, shown.**
- The test as on main, with per-thread round counters added to its failure message only (built test-release,
  same VM, VM load 5 to 7): 3 × 1,000 runs, with 97, 122 and 120 failures.
- Every failure said `GuestTimes { vcpus: [], departed_ns: 0 } rounds [0, 0, 0, 0]`: no thread had run a
  single round when the sampling ended.

**Red and green, the same VM, test-release** (the green binary's source is e4b43764a's, byte for byte):

| binary | conditions (VM load) | runs | failed |
|---|---|---|---|
| main's test | 6 vCPUs (3.6) | 1,000 | 4 |
| main's test | 6 vCPUs (7.9 to 8.3) | 1,000 | 107 |
| the fix | 6 vCPUs (7.1), three runs | 3 × 1,000 | 0, 0, 0 |
| the fix | one CPU, beside a niced busy loop | 1,000 | 0 |
| main's test, then the fix, interleaved | 6 vCPUs (9.1) | 1,000 + 1,000 | 39, then 0 |
| main's test, then the fix, interleaved | 6 vCPUs (8.1) | 1,000 + 1,000 | 0, then 0 |

In all, main's test failed 150 of 4,000 runs and the fix 0 of 6,000.

**The fix on every OS and architecture.**

| OS / arch | where | build | what ran | result |
|---|---|---|---|---|
| Linux arm64 (Ubuntu 26.04.1, 7.0.0-38) | Lima VM g121-winarm | test-release, aarch64-unknown-linux-musl | the sampling test ×6,000 above; `vm::guest_time` (9), `hv::tests` (3), `platform::unix::usage_tests` (6) | all passed |
| macOS arm64 (26.4) | Mac Studio, adas-mac-studio.local-2 | dev, native, rustc 1.98.1 | the relevant unit tests (17: `vm::guest_time`, `platform::unix::usage_tests` with `mach_ns`'s test, `hv::tests`, HVF's `a_page_written`); the sampling test ×1,000 | 17 passed; 1,000 of 1,000 |
| Windows x86_64 (11 Pro, 10.0.26200) | jupiter, a kiln agent | dev, native, rustc 1.98.1 | `vm::guest_time` (9), `hv::tests` (3); the sampling test ×1,000 | all passed; 1,000 of 1,000 in 9.9 s |
| Linux x86_64 (7.2.9-cachyos) | adacachy, over ssh | dev, rustc 1.98.1 | `vm::guest_time` (9); the sampling test ×1,000 | 9 passed; 1,000 of 1,000 |

Not run: Windows arm64 (no host here, and CI's Windows arm64 job stops before tests at engine_ring.rs's compile
error, which is not this branch's). The fix changes only this test.
