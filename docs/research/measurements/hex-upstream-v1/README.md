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

## Owed to ROOT

Real-GPU VMs on each host class (D232); a native launcher's real guests (D233); the 19th
Vulkan context in an NVIDIA GPU VM before and after GPU 0008; Windows (the report pipe,
0019/0026) and macOS checks; the seven-target lint; shards' full suite in test-release; the
zero-patience contract flake (managed-borrowed-nowait/contract.rs:40, 4 times in about 550
whole-lib runs here),
no Hex patch's.
