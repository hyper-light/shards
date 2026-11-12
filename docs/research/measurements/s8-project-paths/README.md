# Step 8: projects plugged into a running member at their host path (D235)

Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 8: a member sees a project plugged while it
runs, at the host's path; files it writes are the host's; Foo and foo are handled as the host
does; a host edit wakes an inotify watch in the guest. The decision and its alternatives are
architecture.md D235; this is the implementer's record. ROOT owns the real-VM runs on HVF,
WHP and arm64 KVM, the measurements and the merge gate's suites.

## Identity

| | |
|---|---|
| Policy | `origin/main` `451672250`; `docs/development-rules.md` sha256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`; role implementer ([POLICY_READ.json](POLICY_READ.json)) |
| Source | branch `p/s8-project-paths-v1`; landed alone as each part is ready: plug roots at `92409d138` (commit A), the daemon's plugs into members (commit B) |
| Hosts | adacachy (Linux 7.2.9 CachyOS, x86_64, 32 CPUs, KVM); adas-mac-studio.local-2 (macOS, arm64); Windows x86_64 |
| Toolchain | Rust 1.98.1 (`rust-toolchain.toml`), zig 0.16.0 (`scripts/install-zig`) |

## What changed

- **A, plug roots** (`92409d138`). Init provisions each domain as it starts with the run's
  plug roots (`plug-root=/X`): each made in its skeleton where the image lacks it, its root
  overlay given a writable layer only init writes, Landlock's every right beneath each root.
  A root grants nothing until a plug lands: the root mount stays read-only.
- **B, plugs into a member.** The daemon names the roots from data (setup.rs `plug_roots`:
  the host OS's own and each bind's top-level directory on Unix), filtered by the rule init
  applies (`shards_abi::run::plug::root_name`). `shards plug volume --for NAME` plugs a host
  directory into agent or harness NAME as it runs (init hotplug.rs `share … member N`): DST
  strictly beneath a root, the member's mount namespace entered by a pidfd re-checked once
  held, the mount points made through a writable copy of its root's mount, the share
  `nosuid`, `nodev`, its host's files the member's. `upper` takes the image root's owner and
  mode. `SYSTEM` pins the domain's read-only root.

## Live runs (R09, R10)

Real VMs where this lane could run them (KVM on adacachy), as light integration tests of the
change; ROOT's sweeps are owed on every other backend.

| Run | Revision | Result |
|---|---|---|
| `tests/plug.rs a_project_is_plugged_into_a_running_agent_at_its_host_path` (KVM x86_64) | `ad4c91cd1`, again at `774e9736` after main merged | pass, 32.5 s cold (first build's downloads), 0.73 to 0.85 s warm |
| the same, guard removed: no Landlock rule beneath the roots (`exec_child`'s plug-root loop skipped) | `ad4c91cd1` | fails as it should: the agent writes nothing beneath its root, "the agent wrote no …/proj/seen" (120.8 s, the test's patience) |

The test's agent records, before its plug, a directory's and a file's creation beneath each
root (EROFS, 30, each); after it, the host's file read and its own written, which the host
reads back.

## Light tests

`SHARDS_HELPERS=skip` for clippy and unit tests; on macOS and Windows empty placeholders
stand for the guest binaries `build.rs` embeds, which these tests never read.

| OS, arch | Revision | Result |
|---|---|---|
| Linux x86_64 (adacachy) | `8c3e591a7` | clippy `-D warnings` of shards-init, shards-testguest and shards-abi for x86_64 and aarch64 musl: clean. Tests: init 209, abi 50 and 2, cmdline 40 and 13, the shards binary's `setup::` and `daemon::devices` 11, and tests/plug.rs on KVM 3 of 3 (D214's own plug test among them, 25.9 s): pass |
| macOS arm64 (Mac Studio) | `fc1c507ef` | `scripts/lint`, all seven targets (format; clippy `-D warnings` for aarch64-apple-darwin, x86_64 and aarch64 Linux gnu and musl, x86_64 and aarch64 Windows): pass. Tests: abi 50 and 2, cmdline 40 and 16, api 84, 16, the shards binary's `setup::`, `daemon::devices` and `api_run::` 21: pass |
| Windows x86_64 (the Windows 11 guest on adacachy) | `8c3e591a7` | abi 50 and 2, cmdline 41 and 16, the shards binary's `setup::` and `daemon::devices` 11: pass |

`fc1c507ef` is `8c3e591a7` with one test's assertion formatted. Linux arm64 has no host here
for tests; its lint passes, and CI's arm64 runners run the suites on main.

## R01–R16

| Rule | This change |
|---|---|
| R01 | No unwrap, expect, indexing or unchecked arithmetic in production code; checked `FIRST_ID + n`; every syscall's failure returned. |
| R02 | D235's alternatives: a tmpfs at each root (hides images' homes), Landlock beneath `/` (widens socket directories), covering a root (Landlock skips hidden mount points, security/landlock/fs.c at v6.18), Docker Desktop's and Lima's shares up front (no default deny). |
| R03 | The roots are each OS's documented ones (Apple's standard directories; FHS 3.0 §3.8, §3.11, §3.12, §3.17; WSL's DrvFs `automount.root`) plus data (each bind's top). No new bound, wait or size. |
| R04 | Plugs keep D214's states (plugged, unplugged by path); the member is part of what a plug records. Refusals carry their reason in words on the daemon's path. |
| R05 | No new cache; the member's namespace is found at each plug from its cgroup, not kept. |
| R06, R07, R13 | Small functions (`plug_roots`, `beneath`, `member_of`, `member_process`, `mount_point`, `member_named`); one rule for roots, host and guest. |
| R08, R14 | No performance claim. |
| R09 | The gap, D214 reaching the command alone, is the plan's; the test above shows the member path working, its mutant failing. |
| R10 | Unit tests (abi `root_name`, `beneath`; init `plug_roots`, `beneath_root`, `domain_pid`, `SYSTEM`; shards `plug_roots`, `member_named`); the live run and its mutant above. |
| R11 | See *Light tests* below; unverified: HVF, WHP and arm64 KVM real-VM runs. |
| R12 | One provisioning in init for every host; the host's roots its OS's alone (setup.rs). |
| R15 | Implementer scope; merged to main fast-forward under the 2026-11-12 rule. |
| R16 | Work directories `~/g121-tmp/s8-*` on adacachy; nothing built on this Mac. |

## Owed to ROOT

- Real-VM runs of `tests/plug.rs a_project_is_plugged_into_a_running_agent_at_its_host_path`
  on HVF (macOS arm64), WHP (Windows x86_64) and arm64 KVM (L1).
