# A microVM's stored request, whole or absent

Implementer for ROOT (coordinator). Base: main `bff3015182ad754274f1048ce393ac12cb6cd91f`.
Branch: `p/audit-fix-request-v1`. Rules SHA-256: `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`
(`POLICY_READ.json`). Owned paths: `crates/shards/src/daemon.rs` (the REQUEST persistence and its test)
and this directory.

Runner: `451397fcc585f7f7cb9aaf00e42064bb` (adacachy), Linux 7.2.9-2-cachyos x86_64, glibc 2.44.
Toolchain: rustc 1.98.1 (`48a229ce`), host `x86_64-unknown-linux-gnu`.
Kiln envelope: `cpus=2,mem=2g,disk=4g`, `-j2`, timeout 600 s or less.
Build environment: `SHARDS_HELPERS=skip`; `SHARDS_INIT_BINARY` and `SHARDS_SERVER_BINARY` name placeholder files.

The shards crate has no lib target (`autobins = false`, one `[[bin]]` at `src/main.rs`), so the test
and clippy ran on `--bin shards`. Exact commands are in `commands.sh`. Nothing was built on the Mac.

## The defect

At `bff3015`, the new-microVM create path (`again.is_none()`, daemon.rs:3537) kept its request (D37)
with `std::fs::write(dir.join(REQUEST), kept.encode())`. That call creates, truncates and writes
`request` in place, so a write that stops partway leaves a cut file named `request`.

Every reader reads the file and calls `Run::decode`:
- in daemon.rs: `again` (start), `joiners_pid`, `joiners_net`, `shared_ipc`, `microvm_of`,
  `shared_ipc_entry`, `names_of` and `parent_of`;
- under `daemon/`: commit, visit, moves, commands, update, networks, ship, restart, inspect and
  engine.

`Run::decode` (crates/ipc/src/lib.rs:1650–1827) reads the named options after the base fields only
`if !r.0.is_empty()`. A request cut exactly where those options begin therefore decodes as a whole
request without them.

## Live reproduction (R09)

Step 1 moved the write, unchanged, into `keep_request(path, request)` (`red-source.diff`).

Test `daemon::tests::a_request_written_partway_is_not_kept`: a request with `--cap-drop ALL` and
`--read-only` (163 bytes) is kept by `keep_request` in a forked child
(`kept_under_limit`). The child:
- ignores SIGXFSZ and sets RLIMIT_FSIZE to the cut, so the write stops there with EFBIG;
- takes no lock and allocates nothing after the fork. Its paths and bytes are made before the fork.
  std builds C strings for paths under 384 bytes on its stack (`sys/helpers/small_c_string.rs` in
  rust-src nightly-2026-09-05; 1.98.1 has no rust-src here). The child then calls `_exit`.

There are two cuts: half the request (81 bytes), and its longest prefix that decodes (115 bytes,
found by decoding every prefix). The test asserts:
- the child ended with EFBIG;
- `request` is absent or decodes to exactly the request;
- nothing else is left in the directory.

Red, against the unchanged write (`red.log`):

```
"cut at 81 of 163: `request` kept, 81 bytes, not a request",
"cut at 115 of 163: `request` kept, 115 bytes, read as another: cap_drop [], read_only false",
```

The first line is the symptom asked for: a `request` that exists and does not decode. The second is
worse: the cut file decodes, and `start` would run the microVM without `--cap-drop ALL` and
`--read-only`.

## The fix

`keep_request(path, new, request)` (daemon.rs:8795, `green-source.diff`):
1. writes `request.new` (`REQUEST_NEW`) in the microVM's directory;
2. renames it to `request`;
3. on any error, removes `request.new` and returns the error. The caller logs it with the existing
   message, unchanged.

The caller builds both paths so that the test's forked child can call the function without
allocating. std's rename replaces the target on every platform:
- Unix: rename(2);
- Windows: `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`, then `FileRenameInfoEx` with POSIX semantics on
  access denial (`sys/fs/windows.rs`).

On this path no `request` exists yet.

| Step | Source (daemon.rs SHA-256) | Result |
| --- | --- | --- |
| Red | bff3015 + red-source.diff (`437b0636…`) | FAILED, the two lines above (`red.log`) |
| Green | bff3015 + green-source.diff (`817cb76d…`) | ok, 1 passed (`green.log`) |
| Mutation: body back to `std::fs::write(path, request)` | green + mutation.diff (`12b293ef…`) | FAILED, the same two lines (`mutation.log`) |
| Restored, the committed source | `817cb76d…`, diff equal to green-source.diff | ok, 1 passed (`final-fmt-test.log`) |
| `cargo fmt --check`, `cargo fmt --all --check` | `817cb76d…` | exit 0, both (`final-fmt-test.log`) |
| `cargo clippy -p shards --bin shards -j2 -- -D warnings` | `817cb76d…` | exit 0, no diagnostics (`clippy.log`) |
| The same with `--profile test` (`profile.test: true`, cfg(test)) | `817cb76d…` | exit 0, no diagnostics (`clippy.log`) |
| `scripts/check-development-policy` (on the Mac) | worktree | passed |

## Durability: no fsync (decision)

### Alternatives
- moby/sys `atomicwriter/atomicwriter.go`, which Docker uses for its container records (main
  `a2d6b3d5`, read 2026-11-11). It writes a temp file in the target's directory, calls `Sync()`
  before `os.Rename`, removes the temp file on every error, and does not sync the directory.
- shards' `context.rs` `replace`: a temp file beside the target, `sync_all`, rename, removed on
  error.
- shards' container records (`containers.rs`, D27 "Durability"): written and renamed, not synced.

### Chosen: not synced
This follows D27, which already decides this for the microVM's own record (`config.json`), written
at the same moment: "Records are written and renamed, not synced. Syncing one costs 8.5 ms at the
median on macOS (PM M46)."

The cost of syncing here:
- On Apple targets, std's `sync_all` and `sync_data` are both `F_FULLFSYNC` (`sys/fs/unix.rs`).
- PM M46 measured F_FULLFSYNC plus rename at p50 4.3–4.4 ms, p90 7.6–12.9 ms and p99 17.0–26.5 ms.
- This path is synchronous: every new microVM takes it before `CREATED`. That cost is more than a
  pooled run (3.4 ms, as M46 cites M26) and most of the 5 ms target.

What a sync would buy: a synced `request` beside an unsynced record does not make the microVM
survive a power loss that its record does not.

### What holds
Against every failure of the write and of the daemon, `request` is absent or whole. EFBIG is
reproduced here. ENOSPC, EDQUOT, EIO and a kill or crash mid-write take the same path but were not
run. A reader never sees a partly written `request`: the name appears only by rename(2), after the
whole file is written and closed.

### What does not hold
After a power loss, the rename may reach the disk before the bytes, so `request` may be found empty
or cut, as `config.json` may. ext4's `auto_da_alloc` forces the data out first only for a rename
that replaces an existing file:
- Documentation/admin-guide/ext4.rst, "replace-via-rename";
- `ext4_rename`: `if (new.inode && !test_opt(new.dir->i_sb, NO_AUTO_DA_ALLOC))
  ext4_alloc_da_blocks(old.inode);` (torvalds/linux master `79eb6679`, read 2026-11-11).

This `request` is a new name.

No directory fsync was added, so a power loss may also lose the rename itself: `request` is then
absent and the start is refused, as for any absent request. Changing this decision is one line, a
`sync_all` of `request.new` before the rename; the cost above is why it is not there.

## Owed or held by others

1. **Performance (R08/R14, coordinator).** The fix adds one rename(2) to the synchronous create path.
   PM M46 found metadata operations there waiting behind other processes' flushes on a busy macOS
   host: open and rename each took 4.4 ms at the median. This is unmeasured here. A/B `shards run`
   with `build-ab/ab.py` on macOS and Linux, quiet and loaded. Moving the write off the path, as
   D27 did for the record, is a design question.
2. **Other writers of `request`, under `daemon/` (another implementer's files).** These rewrite a
   whole request in place with `std::fs::write`, so a write that stops partway turns a whole request
   into a cut one:
   - update.rs:215
   - networks.rs:531
   - engine/own_networks.rs:724
   - moves.rs:1536

   `keep_request` made `pub(super)` would serve them. Two writers of one microVM's `request.new` at
   once would need serializing or separate names.
3. **Unverified (R11).**
   - Windows: the test is `cfg(unix)` because it uses fork and setrlimit.
   - macOS arm64: not built, by instruction.
   - Linux musl and arm64.
   - Any target but x86_64-unknown-linux-gnu, for fmt and clippy.
   - A daemon's real create path with a VM: no E2E was run.
   - Power-loss outcomes, and ENOSPC, EIO or a kill mid-write.

## R01–R16

| Rule | Account |
| --- | --- |
| R01 | `keep_request` returns std's `io::Result`. It has no unwrap, indexing or arithmetic. The removal is best effort (`let _`), as in daemon.rs's `listening_at`. The test's child cannot panic before `_exit`. |
| R02 | The alternatives and the decision are above. |
| R03 | Production has no numeric literal. `REQUEST_NEW = "request.new"` is a name, after `layer.new` and `config.json.new`. The test's cuts derive from the encoding. |
| R04 | No new state. Errors stay `io::Error` and go to the existing log line. The temp file's life: created, written, renamed, or removed on any error. |
| R05 | No wait or cache is touched. No new layer: one function at the existing call. |
| R06/R13 | One seven-line function. |
| R07 | The comment gives the invariant, the reason for no sync with its evidence, and the power-loss gap. |
| R08/R14 | Unmeasured; owed to the coordinator (above). |
| R09/R10 | Red, green, mutation and restore are live, with logs and diffs here. |
| R11/R12 | std only, one implementation for every platform. Verified on Linux x86_64 glibc only; the rest is listed above. |
| R15 | Implementer: light checks on the assigned runner only. No E2E, benchmark or campaign. |
| R16 | The runner's target directory (3.3 GiB, regenerable) is left at `/home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/audit-fix-request-v1-b6a45b8bfb/target`; /home had 190 GB free. Nothing was built on the Mac. |

## Files

| File | Contents |
| --- | --- |
| `POLICY_READ.json` | The policy receipt. |
| `commands.sh` | Every command, verbatim. |
| `probe.log` | The runner's environment. |
| `build-red.log` | The red source's build. Only its last 25 lines were kept (`tail -25`). It took 1m55s. The probe found no `target` directory before it; what the build reused, and how, was not investigated. |
| `red.log`, `green.log`, `mutation.log`, `final-fmt-test.log`, `clippy.log` | Raw outputs, with kiln's run facts after `---`. `red.log` keeps cargo's last 40 lines unfiltered. The others drop aws-lc-sys build-script notes, and clippy's also drop cargo's progress lines, as each file says. |
| `red-source.diff`, `green-source.diff`, `mutation.diff` | The sources tested. |
