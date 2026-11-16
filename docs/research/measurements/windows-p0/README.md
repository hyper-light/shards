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
- **Merged tree** 424a7180d (main 31d154797 merged in, whose commits touch crates/shards and
  crates/vmm): on Windows `grant_answer::windows_tests` 7 of 7 and the three crates' clippy exit
  0; on adas-mac-studio.local `grant_answer::` 5 of 5, macOS and aarch64-pc-windows-msvc clippy
  exit 0.
- **Owed (ROOT)**: the WHP real-VM tests that confine a VM with a snapshot directory and no vsock
  path, and one with a vsock path: `cargo test -p shards --profile test-release --test snapshot`,
  `--test restore`, `--test vsock` and `--test confined -- the_vm_process_is_confined` on jupiter.
  No unit test reaches `confine` itself, which refuses a process outside an AppContainer.

## memory-01: a WHP DAX window made zeros of a file's bytes it could not map again

- **Red** (`memory-01-red.log`, tree 287dc4700, whose `window_whp.rs` is 4697f1e7b's), each the
  finding's scenario on a real NTFS file and real placeholders, views and sections:
  - `a_range_mapped_to_be_written_after_one_to_be_read_stays_its_files`: a 3 MiB file of 0x5a,
    its first range mapped to be read through a handle opened to read alone, its second to be
    written through one opened to write; grown, and followed. The written range read 0 where
    the file holds 0x5a (assertion at window_whp.rs:1248, left 0, right 90): the node kept the
    first handle, no PAGE_READWRITE section could be made of it, and `reconcile` placed
    writable zeros over the range.
  - `a_range_no_view_of_its_file_can_be_made_for_is_refused_never_zeros`: a range to be written,
    past the file's end, through a handle to read alone; the file grown into it, let go of for
    a cut and followed. `reconcile` made the range zeros where the file has 0x77, and a vCPU
    that reached it was let run (window_whp.rs:1280) on memory that is not the file's.
  10 of the module's other tests passed.
- **Fix** (7f0cb8ce9): the node keeps the handle given for a mapping to be written over one
  given to read alone; `reconcile` makes the new section before it frees anything, leaves a
  mapping as it was where none can be made or the file's size is unread, lets its ranges go
  (`Held::LetGo`, vCPUs waiting) where a view cannot be placed once its old ones are freed, and
  refuses each, typed (`fs::Unfollowed`); the device refuses the reply that would tell the guest
  the file's size (EIO). `Held` replaces the entry's two booleans, `zeros` and `evicted`. The
  Unix window's `reconcile` refuses as its twin where it could not map a grown file's pages
  (`follow`'s MAP_FIXED), and follows the file again at its next request.
- **Green** (`memory-01-green.log`, tree 764f87712, the same files but warm.rs, a cfg(unix)
  `let _` added after it): `window::tests` 12 of 12 on Windows; the first test reads 0x5a and the
  grown 0x77 and its store reaches the file; the second is refused `Section { node: 9,
  writable: true }` twice, the range left as it was the first time (a fault there runs at once)
  and let go of still the second (a fault waits until it is unmapped).
- **Mutants**: `m2-no-upgrade` (the node keeps its first handle) fails the first test,
  `Err(Section)` where `Ok`; `m3-section-falls-through` (no section, and the mapping let go of
  anyway, not left as it was) fails the second, its first fault waiting.
- **Clippy** `-D warnings --all-targets`: on x86_64-pc-windows-msvc shards-vmm, shards-vm-process
  and shards exit 0 (764f87712). On adas-mac-studio.local, macOS arm64, 764f87712's macOS clippy
  found warm.rs's `window.cut` Result unused (the Unix daemon's CUT); the `let _` and its reason
  came with the fix, and the merged tree's runs are below.
- **Merged tree** e997f3df1 (main 5ded5b857, platform-03 on it): on Windows `window::tests` 12 of
  12, `grant_answer::windows_tests` 7 of 7, all four mutants failing their tests, and the three
  crates' clippy exit 0; on adas-mac-studio.local `devices::virtio::fs::window` 7 of 7 (1 ignored,
  a measurement), `devices::virtio::fs::tests` 11 of 11, `grant_answer::` 5 of 5, macOS and
  aarch64-pc-windows-msvc clippy of the three crates exit 0.
- **Unverified**: a view refused after its section was made (`Unfollowed::View`, `Held::LetGo`
  from `reconcile`), on Windows and in the Unix twin, is unreachable from a unit test (it needs
  MapViewOfFile3 or mmap(MAP_FIXED) to fail on a valid section), and the device's EIO in place
  of a reply is not exercised by a unit test.
- **Owed (ROOT)**: the WHP real-VM DAX tests: `cargo test -p shards --profile test-release
  --test containers -- dax a_microvm_held_to_a_budget_reads_a_large_file_through_its_window`
  and `--test saved -- a_saved_microvm_reads_on_through_its_dax_window` on jupiter; and the
  same on a KVM and an HVF host for the Unix twin's `reconcile`.
