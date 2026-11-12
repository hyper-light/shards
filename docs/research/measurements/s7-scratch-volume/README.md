# A domain's `/tmp` and the volumes under it (D109): evidence record

This record follows [docs/development-evidence.md](../../../development-evidence.md) for step 7 of kiln plan 01a1cd8e862e7169a074db3d5fa33f3d. The decision is D109's paragraph "A domain's `/tmp` is its own" in [architecture.md](../../../design/architecture.md). **Owed** marks what is not done, and nothing owed counts as passed.

## Identity

- Task: plan step 7 (a member's HOME, env, image ENV and working directory). This record covers its open item: a domain's scratch against a `VOLUME` at or under `/tmp`.
- Branch: `p/s7-home-env-v1`, from origin/main 451672250.
- Policy: development-rules.md sha256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
- Role: implementer for Shards ROOT (01a148884a4770fdaf0111f3f792a248). Unit and light tests only.
- Owned paths: `crates/init/src/domains.rs` (mount order, HOME, env, workdir) and the build's and daemon's twin checks (`crates/agentfile/src/agentfile.rs`, `crates/shards/src/{agentfile,volumes}.rs`).
- Targets: the seven of resources/supported-targets.txt. Init's part is Linux guest code; the rest is host code for every target.

## Decision (R02, R03, R05, R12)

Recorded in D109 with its two alternatives.

The refusal reuses D109's existing three-end structure, so there is no new seam:
- the build, in `shards_agentfile::agentfile::domain_mount`;
- the daemon, through the same function;
- init, which has its own twin in `domains.rs`, since the guest links no host crate.

The refusal is typed, `DomainMount { System, Scratch }`, and its text is rendered by `Display` (R04). It adds no literal: `/tmp` is the path init moves the scratch onto (D139).

## Wait and cache review (R05)

No wait or cache is added or touched. The checks are pure functions of a path.

## Live example (R09, R10)

- `repro.py` replays init's mount steps in its order (domains.rs `skeleton`, `image_root`, `scratch`, staged.rs `files`, then `child`'s `move_mount`s). It uses the same system calls (fsopen, fsmount, open_tree, move_mount) in a user and mount namespace: `unshare -rm python3 -I repro.py DIR`.
- `repro-adacachy.log`: Linux 7.2.9-2-cachyos x86_64, 2026-11-12.
  - Control, a volume at `/data`: mounted. The staged file and handed file are in the scratch.
  - `/tmp/x`: the mount point is present before the scratch and gone after it. `move_mount` gets ENOENT, which is init's `mounting its volumes: errno 2`.
  - `/tmp`: mounted over the scratch. The member no longer sees its staged file, and its `/tmp/handme` lands in the volume, not the scratch its server reads.
- In the namespace only uid 0 is mapped, so the scratch is uid 0's there, where init gives it the member's uid. Ownership plays no part in either case.
- **Owed (ROOT):** the in-VM symptom, both cases on KVM, Hypervisor.framework and WHP.

## Red, green and mutation (R10)

`mutate.py`, with its log `mutate-adacachy.log` (Linux x86_64):
- Red: the code at 451672250 fails the new tests (the build's, the daemon's two).
- Each guard removed alone fails its test:
  - the build's `/tmp` arm;
  - the daemon's re-check;
  - the daemon's Docker `VOLUME` guard;
  - init's `/tmp` arm;
  - init's refusal of a declared volume an image's symlink leads into `/tmp`.
- Green: all pass.
- Not reached by a unit test: init's call to `given_at` inside `start`, which only a real VM runs. **Owed (ROOT)** to the real-VM run above.

## State and code (R01, R04, R06, R07, R13)

- No panic path is added. The checks are matches on path prefixes, with no indexing or arithmetic.
- `given_at` replaces an inline closure and nested `if` in `start` with one match over its four outcomes.

## Portability (R11, R12)

The host code is the same on every OS. Results at 03b32b299 (the change merged with main 3103d2eab):
- **Linux x86_64** (adacachy, glibc, cargo 1.99.1): every test of shards-agentfile (91) and of shards-init (198, 1 ignored as before); shards' `volumes::` and `agentfile::` tests (11). All pass.
- **macOS arm64** (Mac Studio, Darwin arm64): the same shards-agentfile and shards tests pass.
- **Windows x86_64** (the Windows 11 guest on adacachy, rustc 1.98.1): the build's test and the daemon's crafted-image test pass. `volumes.rs`'s test is `cfg(unix)`, so it has no Windows run.
- **Lint**: `scripts/lint` on the Mac Studio (zig 0.16.0 from scripts/install-zig) is clean: format, then all seven targets, plus the GPU renderer's four.
- **Owed**: unit runs on Linux arm64 and Windows arm64. This work had no host for them; CI's matrix runs them after the push.

## Operations (R15, R16)

The scratch work on adacachy is under `~/g121-tmp/s7-*`: the clone, the target directory and the fake helpers that light tests embed and never start. It is removed when the step ends.
