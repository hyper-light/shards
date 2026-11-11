# Locked region owner guard across PID namespaces

ROOT coordinator 01a148884a4770fdaf0111f3f792a248; POLICY_READ main bff3015182ad754274f1048ce393ac12cb6cd91f,
rules df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3. Source: ROOT audit
2026-11-11 (unit 6 finding 1, unit 5 F11), parent f18fd14d75ac5988e5c31ad8df9c866a4bdb3ec7.

## Defect

`Mapping::address()` admitted borrows and the unmap only when `getpid()` equalled the PID
recorded at acquisition. PIDs are namespace-relative. Guest init, the only user, is PID 1,
and so is every child it clones with `CLONE_NEWPID` (actors, reapers, `fork_in(None)`
children). In those children `MADV_DONTFORK` has removed the region, yet the guard passed:
`bytes()` returned a slice over unmapped memory, and dropping the copied owner unmapped
whatever the child had since mapped at that address — the case the guard claimed to prevent.

## Live reproduction (R09), runner 451397, Linux 7.2.9-2-cachyos x86_64, rustc 1.98.1

`region_guard_holds_for_a_pid_one_child_in_a_new_pid_namespace`: a single-threaded child
unshares user+PID namespaces (unprivileged; `kernel.unprivileged_userns_clone=1`); its child
is PID 1 there and acquires a region; that owner `clone(CLONE_NEWPID)`s a grandchild, also
PID 1, which maps its own page where the excluded region was (`MAP_FIXED_NOREPLACE`), tries
`bytes()`, drops the copied owner, and checks its page with `mincore`. Report bytes:
[stage, owner_pid1, child_pid1, refused, own_page_mapped, own_page_survived].

- RED on unmodified production (locked_region.rs 02fe8809…, tests 3f05d75c…):
  `REGION_PIDNS report=[0, 1, 1, 0, 1, 0]` — borrowed, and the child's own page was unmapped.
- GREEN (locked_region.rs ccc4ffd4…, tests b76c36b2…): `[0, 1, 1, 1, 1, 1]`; all 4 module tests pass.
- MUTATION (guard replaced by `if true`): pidns test `[0, 1, 1, 0, 1, 0]` fails and the existing
  fork test fails `process_refused=0`; restored hash ccc4ffd4… passes again.
- `cargo fmt --check` 0; `cargo clippy --all-targets -D warnings` 0 for x86_64-unknown-linux-gnu,
  x86_64-unknown-linux-musl and aarch64-unknown-linux-musl. RAW.md keeps the returned output.

## Change

One mapping now holds the region's page-rounded extent and one following owner page. The owner
page is written `OWNED` immediately after `mmap` (so every cleanup can still release), then
`MADV_DONTFORK` covers the region and `MADV_WIPEONFORK` (Linux >= 4.14; madvise(2)) the owner
page. Any process created without `CLONE_VM` inherits the owner page zero-filled, so
`address()` refuses it whatever its PID or namespace; threads and the acquiring process read
`OWNED`. One `munmap` releases both. The owner page is not locked. `Extent` is unchanged; a new
typed `Operation::MarkOwner` reports a failed `MADV_WIPEONFORK`. The per-borrow `getpid`
syscall is replaced by one memory read (not measured; no performance claim).

Limits: a `CLONE_VM` non-thread child (vfork) shares the address space and is admitted — the
mapping exists there; such children run no Rust destructors. A process that deliberately
remaps the owner page's address is not defended against. No VM/guest-init run yet.
