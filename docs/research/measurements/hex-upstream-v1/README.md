# Hex's vendored Shards patches, upstreamed (hex-upstream-v1)

Implementer's record (POLICY_READ.json: rules df2acf47…, origin/main bff30151; coordinator ROOT).
Hex (github.com/hyper-light/hex) vendored Shards at 33df95b0 with patches 0001–0031
(`vendor/patches/shards`) and Shards at 0342b85d with patches 0001–0008
(`vendor/patches/shards-gpu`), read from Hex main 2dc8295f (its vendor/ unchanged since
aaa07f4b). 0342b85d was already in Shards; the hex/physical-gpu-0342 chain on it was not (its
28 commits sat unpushed in Hex's shards-source.git). Each port applies Hex's patch three-way
onto this branch (a replay of Hex's series on the integration's files is the base and theirs).

## Every patch

| Hex patch | What | Classification | Shards commit |
|---|---|---|---|
| 0001, 0003, 0006 | backend_of names WHP for Windows arm64; conformance's excluded host; its loop | applies (one commit: 0003 and 0006 repair 0001's test and clippy); D215 updated | 41f4e3bf |
| 0002 | GuestSpec.gpu → `--gpu` | applies; comments in shards' voice | 9f3c91da |
| 0004, 0005, 0007, 0029 | crates/api/tests/hex_borrowed_contract.rs: Hex's ROOT-run ignored campaign (HEX_* variables, Windows handle counts, cold-handle stages) | Hex-only: Hex's coordinator's measurement harness, opt-in by Hex's variables; it uses shards-api's public surface alone, so Hex can keep it as a test of its own crate on its dependency, no patch | — |
| 0008 | GuestSpec.gpu_devices, `--gpu-device` arguments | applies | 1fd078bf |
| 0009 = GPU 0001 | the identity type in shards-abi | needs adaptation (abi lib.rs's module list) | 08bb8fda |
| 0010 + 0014 | managed launch receipts, GPU probe; 0014 its ABI clippy repair | applies | 84274050 |
| 0011 | Refusal::Scope | adapted: Scope's source() forwards its failure's (no repeated cause) | e185bc99 |
| 0012 | stop intent before an inline native stop | applies | ca94ecf8 |
| 0013 | ManagedReceipt through triomphe's unsize | applies; new dependency unsize =1.1.0 (no RustSec advisory, advisory-db fd6f3981) | 63ccdbdb |
| 0015 | a client's runtime directories, scope observations | adapted: "a node's" said as a client's; refusal text "runtime directories" | 301e5bdd |
| 0016 | probe: names, device-local memory | applies | c80d1440 |
| 0017 + 0018 | the private report's frames; 0018 repairs 0017's tests | applies | ca81077f |
| 0019 + 0020 + 0026 | the report read through the native owner; 0020, 0026 its test order and clippy repairs | applies (awaiting ROOT's review in Hex) | ef3aa256 |
| 0021 | the native test model refuses typed, never panics in a Result | applies; ported right after 0012, whose clippy it repairs | 1b904c2f |
| 0022 | finite owned-retry deadline regression test | applies | 19538579 |
| 0023 | absolute owning connections | adapted: Connect's source() forwards its failure's | 63ec3449 |
| 0024 | native startup images verified (API half of the chain's f7f8b03b) | adapted wording | 851452ce |
| 0025 | the reader fixture's gpu and gpu_devices | adapted: folded into 0002 and 0008 (the fixture stays at its evidence path) | 9f3c91da, 1fd078bf |
| 0027 | each report deadline recheck tested alone | applies (awaiting ROOT's review in Hex) | 5b4b2212 |
| 0028 | connect_once_before | applies | 173fc9b4 |
| 0030 | descriptor-counting reader tests each run alone | adapted: fixture kept in place (run-outer-oom.sh hashes it); its Process source() half not needed | 03786682 |
| 0031 | take-back tests accept the reap between pidfd and stat | adapted: OpenStat ESRCH accepted too (seen live) | 1227113e |
| — | Refusal's chain says each cause once (Borrowed, BorrowFailure::Channel; ROOT's note on Scope and Connect) | defect in shards (red/green/mutations) | 3b90dc25 |
| GPU 0002 + 0007 | each Venus context bound to the selected GPUs; 0007 the bridge's whole archive | adapted: no Windows arm64 GPU packaging (ROOT holds it), no gpu-libs scratch move (no reason recorded), usage text placed | 83b09843 |
| GPU 0003 | `--gpu-probe`, `--host-managed`, context whitelist | adapted usage | f57f1358 |
| GPU 0004 | names and heap sizes | applies | 002ade01 |
| GPU 0005 | the private report before the guest runs | adapted: MachineGpu (1ab4a163's too_many_arguments), StartError's source(), test SAFETY comments | b20e593c |
| GPU 0006 | a reused context id's test | applies (green + guard removal) | f2fe241a |
| GPU 0008 | the renderer holds the host's Vulkan driver | adapted to shards' host_visible (red/green on NVIDIA); comment on glibc corrected | 6accac1e, bbd22169 |
| chain f7f8b03b (not a Hex patch) | verify_managed_vm in crates/shards/src/helpers.rs | wrong as is, not ported: calls shards-api, only shards' dev-dependency, so a release build fails; nothing calls it. Its API half is Hex's 0024 | — |

D-numbers: runtime-image-boot took D231; these ports cite D232 (GPU selection) and D233
(launch through a client's native owner), renumbered in a68a3df7.

## Records

0030-reader-isolation.md, 0031-take-back-race.md, source-chain.md, G0002-selection-binding.md,
G0006-reused-context.md, G0008-held-driver.md; harnesses gate.sh, reader-isolation.sh,
take-back-race.sh, held-driver.sh, gpu-libs-archives.sh, static-tls-loop.c, mutation.sh with
mutations/. Runners: kiln-a094a12d (8fadc1ec) and adacachy (451397fc, RTX 5060 Ti); kiln's sync
of the full worktree timed out from 19:20, so later runs overlay the changed paths on the
runner's copy (in-copy.sh, outside the repository).

## Windows x86_64, after the merge to main

From 482c5d78, main's two x86_64 Windows CI jobs failed at "The GPU's renderer" (run 43230466376,
jobs 131224103597 and 131224104258): `cp: cannot stat '…/selection/shards_gpu_selection.lib'`.
These ports had not been built, linted or tested on Windows before. Four defects, each seen failing,
then passing:

| Defect | Seen failing | Fix | Seen passing |
|---|---|---|---|
| gpu-libs packaged a name no build makes: meson names every static library lib*.a, on Windows too (meson's FAQ, "Why does building my project with MSVC output static libraries called libfoo.a?"). From Hex's GPU 0002 | CI 43230466376, both x86_64 Windows jobs | 1cd72867: libshards_gpu_selection.a copied as shards_gpu_selection.lib, as the renderer's and libepoxy's are | CI 43251385669, "The GPU's renderer" on windows-2025-vs2026 and windows-2022; jupiter S1 |
| A Windows test's SAFETY comment parted from its unsafe block by rustfmt (cli/mod.rs, from GPU 0005, b20e593c) | CI job 130975788127 (aarch64-pc-windows-msvc, c4ae59ee0) | 8b5fb57c; ea348285 (ring-pins) made the same change on main, and the merge keeps it once | jupiter S5 and S6; CI 43251385669 Lints, aarch64 and x86_64 Windows |
| shards-api's managed test model gave prepare_boot `/model/…`, which is not absolute on Windows (from Hex 0012) | jupiter S5 at 2556e892: 2 of 66 lib tests panicked at client.rs:2347:89 | 7c4e3c4e: its paths sit under the root of the test's directory | jupiter S5 at c01aeb94 |
| The identity bridge was compiled for the debug C runtime (/MDd: its object asked for msvcrtd.lib, the renderer's 62 and libepoxy's 4 for msvcrt.lib), so every link warned LNK4098; MSVC requires one runtime across a link's modules (its /MD reference). The binaries imported the release runtime alone. From Hex's GPU 0002 | jupiter at c01aeb94: `llvm-readobj --coff-directives`, LNK4098 in each GPU test binary's link | 1f86d092: on Windows the bridge's setup names `-Db_vscrt=md` (its project names no buildtype; meson's default, debug, picks /MDd) | jupiter at 8320b782: all three libraries ask for msvcrt.lib, no linker warning |

jupiter (Windows 11 Pro 10.0.26200, Ryzen AI Max+ 395, Radeon 8060S, Visual Studio 2022 Build Tools,
MSVC 14.44.35207, SDK 10.0.26100.0) had no LLVM or Python of its own: LLVM 20.1.8 and Python 3.12.10
(python-build-standalone 20250517), the versions CI's windows-2025-vs2026 image has, were unpacked
user-local, each checked against its published sha256; zig 0.16.0, NASM 3.02, rustc 1.98.1, meson 1.11.2
and ninja 1.13.2 as pinned. Kiln agent 01a1d06b ran S1-S6 at c01aeb94 (main 87bf86b3 plus the
packaging and test-model fixes) and S1-S4 again at 8320b782 (main fe44bc8c plus all three), for
x86_64-pc-windows-msvc:

| Step | Command | c01aeb94 | 8320b782 |
|---|---|---|---|
| S1 | `scripts/gpu-libs` | exit 0, 106 s; shards_gpu_selection.lib 80,662 B, a whole archive (native-identity.c.obj) | exit 0, 65 s; 80,650 B; /DEFAULTLIB:msvcrt.lib in each library's every object |
| S2 | `cargo clippy -p shards-vmm --features gpu --all-targets -- -D warnings` | clean | clean |
| S3 | `cargo test -p shards-vmm --features gpu --profile test-release gpu::` | 16 passed (the_renderer_offers_venus_and_holds_its_blobs through vulkan-1.dll on the Radeon 8060S among them); LNK4098 | 16 passed; no linker warning |
| S4 | `cargo build -p shards --profile test-release`, SHARDS_GPU_LIBS set | exit 0; carried shards-vm-gpu 4,318,208 B (linked after gpu-libs removed its scratch) | exit 0; 4,318,208 B, sha256 d3c7709a…; imports VCRUNTIME140.dll and the release UCRT, no debug DLL |
| S5 | shards-api `--lib --test sim`; shards-abi; the shards binary's `cli:: spec:: vm_run::` | 66 + 5; 49 + 2; 33 passed | (unchanged by 1f86d092) |
| S6 | `cargo clippy --workspace --all-targets --keep-going -- -D warnings` | clean | (unchanged by 1f86d092) |

gpu-libs' Windows libraries are not byte-reproducible: each embeds its scratch directory's random
name (two builds of one source differed by 90 bytes, 80 of them that name).

## Owed to ROOT

Real-GPU VMs on each host class (D232); a native launcher's real guests (D233); the 19th
Vulkan context in an NVIDIA GPU VM before and after GPU 0008; the report pipe in a real managed
launch on Windows (D233); macOS checks; arm64 Windows' tests; the seven-target lint; shards' full suite in test-release; the
zero-patience contract flake (managed-borrowed-nowait/contract.rs:40, 4 times in about 550
whole-lib runs here),
no Hex patch's.

## R01–R16

| Rule | This port's proof, and its gap |
|---|---|
| R01 | Ported production code allocates fallibly (try_reserve, triomphe try_new) and checks its arithmetic; the workspace's panic lints pass with clippy -D warnings on the touched crates (x86_64 glibc; shards-abi and shards-init on musl). Tests assert. Implicit panic paths beyond the lints were not audited line by line. |
| R02 | D232 and D233 cite Hex's research and the managed-virtio-gpu lane's check of the renderer patches; unsize's alternatives are Hex's managed-native-receipts.md. Unresearched spans were left out (Windows arm64 GPU packaging, gpu-libs' scratch move). |
| R03 | D232 lists each new literal's source; MAX_PHYSICAL_GPUS's derivation is Hex's scheduler's, owed shards' own. |
| R04 | Every new refusal is typed (Gpu, GpuProbe, Launch, ManagedIo, Scope, Connect; StartError::GpuProbe); a receipt's end is TerminalScope's; a printed chain says each cause once (source-chain.md). |
| R05 | No cache added; every new wait is the caller's deadline or an event; ManagedLauncher's real caller is Hex. |
| R06 | client.rs grows by about 2,000 lines of Hex's design, reviewed for this port's adaptations, not re-designed. |
| R07 | Hex's acceptance IDs, ROOT and "node" wording replaced by shards' records; Hex's claim that glibc never gives static TLS back corrected. |
| R08 | No performance claim. |
| R09 | Reproduced live before fixing: 0030 (30/30 red), 0031 (OpenStat ENOENT and ESRCH), the refusal chain, GPU 0007 (thin archive, link failure), GPU 0008 (driver reloads on NVIDIA). The other patches port features whose defects Hex reproduced (cited in their commits); not re-reproduced here. |
| R10 | Records and harnesses here; guard removals for 0030, the chain, GPU 0006 and 0028; GPU 0008's and 0007's reds are their guards removed. |
| R11 | Verified on x86_64 Linux (glibc; musl for the guest crates) and natively on x86_64 Windows (jupiter: gpu-libs, the GPU VM process's lint and link, the renderer's unit tests on a Radeon 8060S, the touched crates' unit tests including 0019's and 0026's pipe code, the workspace lint); aarch64-pc-windows-msvc's workspace lint passes on CI (43251385669). macOS and arm64 tests are CI's (ROOT's). |
| R12 | One identity type in shards-abi for the API and the VM process; no OS-specific duplicate added. |
| R13 | f7f8b03b's unused helpers.rs half and Hex's own test campaign not ported. |
| R14 | No profiling or benchmark (ROOT's). |
| R15 | Implementer's light tests only; real-VM and GPU-VM validation listed as owed. |
| R16 | Nothing deleted. The Mac's disk filled during the work (not from this port; ROOT reclaimed); runner scratch is ~/g121-tmp/hex-upstream-v1 on each runner. |
