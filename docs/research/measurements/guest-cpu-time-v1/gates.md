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
