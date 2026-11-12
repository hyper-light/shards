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
