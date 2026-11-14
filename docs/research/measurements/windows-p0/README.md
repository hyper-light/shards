# Windows P0 findings platform-03 and memory-01 (2026-11-14)

Implementer lane of ROOT's (g121), policy at 9efde7252: AGENTS.md
f66d304c…6db41, docs/development-rules.md df2acf47…d13d3, docs/development-evidence.md
61613a24…b68fb63. Findings: `docs/bugs/2026-11-13/platform.md` platform-03,
`docs/bugs/2026-11-13/memory.md` memory-01.

Host: the Windows 11 guest on adacachy (`shards@127.0.0.1:2222`, ED25519
SHA256:Zy41Whwy7VAoSyleXvGlHx8kZUutMMXqCuCRP/vJfCI), Windows 10.0.26200, x86_64-pc-windows-msvc,
stable Rust, 8 vCPUs (jupiter was offline when the runs began). Each run is `run.ps1` here, run as
`powershell -ExecutionPolicy Bypass -File run.ps1 -sha <sha> -what test|mutate|clippy|all`
from a blobless clone of github.com/hyper-light/shards, `SHARDS_HELPERS=skip` and empty
stand-ins for the guest binaries build.rs embeds (no test here reads them). Mutants are the
`m*.patch` files here, applied with `git apply` to the fixed tree, each test rerun, the tree
restored.

## platform-03: a staging VM's grants left open after its guest starts

- **Red** (`platform-03-red.log`, tree 287dc4700, whose `grant_answer.rs` is e889915c4's):
  `a_vm_that_stages_its_snapshots_is_granted_nothing_more_once_it_starts` asks, as `confine`
  asked before, for a kernel to read and a snapshot directory to stage in, and no `SEALED`;
  stages and commits a generation; then asks for a sentinel file outside its grants, to write.
  The spawner granted it, and the sentinel read `overwritten` where it was `untouched`
  (assertion at grant_answer.rs:1053): the forbidden effect, a host file of the user's written
  by a VM after its guest started. 6 of the module's other tests passed.
- **Fix** (1e8c83104): `granted()` asks its last grants through `grant_ask::obtain_sealed`,
  which writes `SEALED` after them whatever was granted; `confine`'s branch that skipped it is
  gone. The broker handles DIAL, RANGES, STAGE and COMMIT before it looks at `started`.
- **Green** (`platform-03-green.log`, tree 764f87712, the same files): `grant_answer::windows_tests`
  7 of 7; the test's stage and commit after the seal succeed, and the late GRANT is refused
  `a VM asked for access after it started`, the sentinel untouched.
- **Mutants**: `m1-no-seal` (obtain_sealed writes no `SEALED`) and `m4-broker-no-refusal` (the
  broker's `if started` refusal removed) each fail the test with the sentinel overwritten.
- **Clippy** `-D warnings --all-targets` on x86_64-pc-windows-msvc: shards-vmm, shards-vm-process,
  shards exit 0. On adas-mac-studio.local (macOS 26.2 arm64), at 764f87712: `cargo test -p shards
  --bin shards -- grant_answer::` 5 of 5, and `cargo xwin clippy --target aarch64-pc-windows-msvc`
  of the three crates exit 0.
- **Owed (ROOT)**: the WHP real-VM tests that confine a VM with a snapshot directory and no vsock
  path, and one with a vsock path: `cargo test -p shards --profile test-release --test snapshot`,
  `--test restore`, `--test vsock` and `--test confined -- the_vm_process_is_confined` on jupiter.
  No unit test reaches `confine` itself, which refuses a process outside an AppContainer.
