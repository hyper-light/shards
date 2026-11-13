# Hex's last vendored patch, upstreamed: the GPU's VM process for Windows on arm64

Goal 52's plan, step 3: Hex vendors Shards with one change of its own,
`vendor/patches/shards-gpu/0001-gpu-build-and-carry-the-gpu-vm-process-for-windows-on-arm64.patch` (Hex
97f9bbbb, re-vendored at Shards e82b9403 by Hex f0179240). This brings it into Shards, so Hex can build from a Shards
revision with no patch of its own. D232 records it in its paragraph "Windows on arm64", which replaces "Held".

## Identity

- Tested on Shards origin/main 72cdfbf06 with the change applied, 2026-11-13. Policy at that revision:
  - AGENTS.md sha256 f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41;
  - docs/development-rules.md df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3;
  - docs/development-evidence.md 61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63.
- Role: implementer (goal 52's agent).
- Paths: crates/shards/build.rs's `gpu_target` and its doc, scripts/gpu-libs, D232's paragraph, and this record.

## The change

- `gpu_target` takes `aarch64-pc-windows-msvc`. The rest of building the GPU's VM process doesn't depend on the target
  (`compile_gpu_helper` runs `cargo build -p shards-vm-process --features gpu --target <TARGET>`), and
  crates/vmm/build.rs already links the renderer on Windows for any arch (`link_gpu_windows`).
- scripts/gpu-libs:
  - Windows' shells (`MINGW*`, `MSYS`, `CYGWIN`) name the host `Windows-aarch64` or `Windows-x86_64`. The name comes
    from `SHARDS_GPU_TARGET`, else `VSCMD_ARG_TGT_ARCH` (the VS developer prompt's target), else
    `PROCESSOR_ARCHITEW6432`, else `PROCESSOR_ARCHITECTURE`. Any other name is refused (exit 2, with its words).
  - Every Windows build, x86_64's too, is a meson cross build. Its cross file sets `clang-cl --target=<triple>`,
    `llvm-lib`, `pkg-config` and a `host_machine` with the target's cpu. The renderer is built for the machine named,
    not for the one the tools default to. Hex's reason: a shell running x64 tools under emulation on an ARM host.
  - Every Windows-only branch matches `Windows-*`. That includes the identity bridge's `/MD` (`-Db_vscrt=md`,
    1f86d092a) and its copy as `shards_gpu_selection.lib` (meson names it `lib*.a` on Windows too, 1cd728678).
  - Two pins are added (below).

The patch doesn't apply to main as it stands, because main changed both bridge lines after the patch's base. A
three-way apply took everything else. The bridge's copy and its `/MD` lines were then widened to `Windows-*` by hand,
as Hex's current vendor has them.

## The same as Hex's

At Hex main 65ef204f (GitHub's, 2026-11-13), Hex's `vendor/shards-gpu` is Shards e82b9403 plus that patch.
- Its `scripts/gpu-libs` and `crates/shards/build.rs` are byte-identical to this change's (`cmp`).
- Shards main changed neither file between 831854804 and 72cdfbf06.
- So at the revision this lands at, Hex's vendoring yields these bytes with no patch.

Hex's to do, through ROOT (nothing here is pushed to Hex): re-vendor shards-gpu at the landed revision without
`vendor/patches/shards-gpu/0001-…`, and drop the words that name the patch:
- vendor/MANIFEST.toml's shards-gpu comment (line 123);
- scripts/shards-gpu/build.sh's header (line 8);
- CLAUDE.md:348-350's exception.

## Pins (R03)

Each pin is a publisher's digest, not a number of ours. Both were read from PyPI's JSON API on 2026-11-12. The wheels
downloaded from those URLs on 2026-11-13 match them (`shasum -a 256 -c`).

| Wheel | sha256 | Source |
|---|---|---|
| ninja-1.13.2-py3-none-win_arm64.whl | 1db9852e528efa7702f5123969f86678663e46d57ff28ab13f5fd84d64a85fb1 | https://pypi.org/pypi/ninja/1.13.2/json, uploaded 2026-08-30T15:49:50Z |
| pkgconf-3.0.7.post0-py3-none-win_arm64.whl | 3716a2a2b2dca3466589b371e5bfa88bb5a4e84e4e7a3771ce14b112a0965f05 | https://pypi.org/pypi/pkgconf/3.0.7.post0/json, uploaded 2026-09-22T02:51:32Z |

meson and colorama are pure-Python wheels, already pinned. PyYAML is pinned for Linux alone, as before.

## The x86_64 path, checked against source (not built)

x86_64 Windows' renderer, which CI and jupiter build today, is now a cross build. It was checked against meson
1.11.2's source (the pinned wheel) and against the pinned sources' meson files:

- Compilers: the cross file's entries win over `CC` and `AR` (environment.py:383, `setdefault`). They name the same
  programs as before: clang-cl, with the target an x64 clang-cl defaults to.
- pkg-config: a cross build finds the host's only in the cross file (dependencies/pkgconfig.py:237-238,
  `allow_default_for_cross=False`). The cross file names `pkg-config`, and both pkgconf wheels install that console
  script (their entry_points.txt).
- No exe wrapper: meson skips running the compiler's sanity program (compilers/compilers.py:1398-1400), and a
  `cc.run()` would fail (704-706).
  - libepoxy 1.5.10's and the krunkit fork's meson files have no `.run(`, `sizeof`, `alignment`, `compute_int`,
    `native: true` or cross checks.
  - Their custom targets run Python, a build-machine program.
  - Their tests, fuzzers and render server are off, and Windows builds `src/libvirglrenderer.a` alone.
- The build machine's compiler is optional (interpreter/interpreter.py:1311-1312, 1567-1571).
- `--vsenv` is unchanged (utils/vsenv.py): vcvars64 on an x64 machine, vcvarsarm64 on an arm64 one, and an active VS
  developer prompt's environment kept as it is.

This is a desk check, not a build. Its first compile is jupiter's (owed) or CI's. CI's "The GPU's renderer" step keys
its cache on scripts/gpu-libs, so main's next CI run rebuilds the renderer:
- on its x86_64 Windows jobs, through the cross file;
- on macOS and Linux, whose paths this change leaves as they were.

## What ran where

- macOS 26.2 arm64 (adas-mac-studio.local, kiln run r178, rustc and cargo 1.98.1), main 72cdfbf06 with the change
  applied: `SHARDS_HELPERS=skip cargo clippy -p shards --all-targets -- -D warnings`. Placeholder init and server
  binaries (`SHARDS_INIT_BINARY`, `SHARDS_SERVER_BINARY`) stood in, since a check build needs no guests.
  - Result: exit 0 in 32 s, with 39 packages checked, `shards` and every workspace crate it depends on among them.
  - The log's only `warning:` lines are aws-lc-sys's build-script notes; there are no lints. The log is
    ~/g121-tmp/s3-logs/clippy.log on that runner.
  - It compiles and lints build.rs for the host. `gpu_target` depends on the target's name alone, so this checks its
    code, but it doesn't run it for arm64 Windows.
- No `cargo test` covers the change. Cargo runs no build-script tests, and no test names the targets that carry the
  GPU's VM process.
- `bash -n scripts/gpu-libs` parsed these bytes (goal 52's agent, 2026-11-13).
- Not run, under the person's rules (2026-11-13): gpu-libs, scripts/lint, any Windows build, any CI job.
- No cross clippy ran: the runner has no `aarch64-pc-windows-msvc` std.

## R01-R16

- R01, R04, R06 and R07: no Rust beyond one match arm. The script refuses an unnamed Windows target with exit 2 and
  its words.
- R02: Hex's patch and records; meson 1.11.2's source; PyPI's digests and wheels.
- R03: two publisher digests (above). There's no tuned number.
- R05, R08 and R14: not applicable. There's no wait, cache or hot path.
- R09 and R10: no defect is claimed. The change adds a target, so there's no red to show. Removing it would only
  restore main's behavior, which no light check observes either. Its proof is the native build ROOT owes.
- R11: arm64 Windows is a supported target (resources/supported-targets.txt), and this is its GPU build.
  - The build is unverified until ROOT runs it.
  - A GPU VM on WHP arm64 stays unverified for want of a host.
- R12: one script and one cross file for both Windows arches. There's no fork.
- R13: the smallest change, and the one Hex's vendor already carries.
- R15: implementer scope (one clippy on one host). Every build and run below is ROOT's.
- R16: none.

## Owed to ROOT, at the landed revision

1. On a native arm64 Windows host (VS Build Tools with the ARM64 tools, LLVM's clang-cl and llvm-lib, Git Bash,
   Python 3.10 or later, and rustup's native toolchain, whose host is `aarch64-pc-windows-msvc`), from Git Bash:
   - `SHARDS_GPU_TARGET=aarch64-pc-windows-msvc scripts/gpu-libs "$PWD/target/gpu-libs"`
   - `SHARDS_GPU_LIBS="$PWD/target/gpu-libs" cargo clippy -p shards-vmm --features gpu --all-targets -- -D warnings`
   - `SHARDS_GPU_LIBS="$PWD/target/gpu-libs" cargo test -p shards-vmm --features gpu --profile test-release gpu::`
   - `SHARDS_GPU_LIBS="$PWD/target/gpu-libs" cargo build -p shards --profile test-release`, which builds the carried
     shards-vm-gpu. Expect no LNK4098.
2. On jupiter (x86_64 Windows), README.md's S1 to S4 again, since x86_64's build now goes through the cross file:
   - `scripts/gpu-libs`;
   - `cargo test -p shards-vmm --features gpu --profile test-release gpu::` (16 passed before), with no LNK4098.

   CI's "The GPU's renderer" also builds it, on main's x86_64 Windows jobs.
3. `scripts/lint aarch64-pc-windows-msvc` and `scripts/lint x86_64-pc-windows-msvc`.
4. The arm64 GPU VM process's Rust, which no lint or CI job compiles: scripts/lint and ci.yml's "Lints of the GPU's
   renderer" lint `shards-vmm --features gpu` for x86_64 Windows alone. On a Mac with scripts/lint's tools:
   `SHARDS_HELPERS=skip PATH="$(brew --prefix llvm)/bin:$PATH" cargo xwin clippy -p shards-vmm --features gpu --all-targets --target aarch64-pc-windows-msvc -- -D warnings`.
   If it passes, add `aarch64-pc-windows-msvc` to both GPU lints. Those files are outside this step's paths.
5. On jupiter, the arm64 path as far as an x64 host allows, without admin installs, from Git Bash:
   `SHARDS_GPU_TARGET=aarch64-pc-windows-msvc scripts/gpu-libs "$TEMP/gpu-libs-arm64"`.
   - Expected: the host is named `Windows-aarch64` and meson is given `--target=aarch64-pc-windows-msvc`.
   - It should then stop at meson's C compiler check, because `--vsenv` sets up x64's libraries on an x64 machine.
   - From a VS x64_arm64 developer prompt with VS's ARM64 build tools installed, it can cross-build instead.
6. Unverified, for want of a host: a GPU VM on WHP arm64 (R11).
