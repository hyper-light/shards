# Every rewrite of a record a reader decodes, whole or absent

Implementer for ROOT (coordinator, agent `01a148884a4770fdaf0111f3f792a248`). Policy: origin/main
`bff3015182ad754274f1048ce393ac12cb6cd91f`, rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3` (`POLICY_READ.json`).
Base: `origin/p/integrate-engine-20261111` at `032b63b1c` (fast-forwarded from `82083f27` at
ROOT's word: the earlier base did not compile shards-vmm). Branch: `p/request-sites-v1`.

Runner: `8fadc1ec213332e16cb0d915a3fb913f` (kiln-a094a12d.attlocal.net), Linux
6.19.10-300.fc44.x86_64, glibc 2.43, 32 CPUs. Toolchain rustc 1.98.1 (`48a229ce`), host
`x86_64-unknown-linux-gnu`; zig 0.16.0 on `PATH`. Envelope: `systemd-run --user --scope -p
MemoryMax=24G`, `-j4`, `TMPDIR=$HOME/g121-tmp`, `amphora-bench` checked absent before each
command. Build environment: `SHARDS_HELPERS=skip`, placeholder `SHARDS_INIT_BINARY` and
`SHARDS_SERVER_BINARY`. Exact commands: `commands.sh`. Nothing was built on the Mac.

## The defect left after 2097de41

2097de41 kept a new microVM's `request` (D37) whole through `keep_request`. Every other rewrite
of a microVM's `request` still wrote it in place (`std::fs::write`: truncate, then write):

| Site (032b63b1c) | What rewrites it |
| --- | --- |
| `daemon/update.rs:215` | `shards update` |
| `daemon/networks.rs:531` (the method `keep_request`) | `network connect`, `network disconnect` |
| `daemon/engine/own_networks.rs:724` (`change_request`) | an engine's own network connect and disconnect (D181) |
| `daemon/moves.rs:1536` | a move's import, in its staging directory (not this defect, below) |

A write stopped partway leaves a cut `request`. A reader finds it truncated to nothing or cut.
`Run::decode` reads the named options only `if !r.0.is_empty()`, so a request cut where they
begin decodes as a whole request without them.

## Live reproduction (R09)

Red source: `032b63b1c` + `red-source.diff`. The tests below are as committed. `keep_whole` holds
the sites' own write, moved unchanged (`std::fs::write` in place). Every site is as at
`032b63b1c`, `update.rs:215` among them. Log: `red.log`.

### The `update` site, unchanged

Test `daemon::tests::an_update_written_partway_keeps_the_request_it_had`:
- A test daemon (`Test`) makes microVM `cut`. Its threads end as `t.run` returns.
- Its `request` is written: `--cap-drop ALL`, `--read-only`.
- `shards update --cpu-shares 512 cut` goes through the daemon's own command path
  (`Daemon::command`) in a forked child (`asked_under_limit`).
- The child ignores SIGXFSZ and has RLIMIT_FSIZE at the cut (`under_limit`).
- The cuts derive from the updated request's encoding (`cuts`): half of it (102 of 205 bytes),
  and its longest part that decodes as another request (122).

Red:

```
"request cut at 102 of 205: kept, 102 bytes, read as none",
"request cut at 122 of 205: kept, 122 bytes, read as another: ([], false, 0)",
```

Each time, the client was told `File too large (os error 27)`, and the microVM's request was left
cut. At 122 bytes it reads as a request without `--cap-drop ALL`, `--read-only` or the update
(`(cap_drop, read_only, cpu_shares)`), which `shards start` would run.

### An update after a cut one

The first red run (`red-first.log`, `red-first-source.diff`) asserted the un-cut update before
the cut findings, and failed there. After the cut cases, `request` was left cut. The next update,
not cut, read it as a request without the options and kept that whole.

Its written request names 1 option (`resources`); the request it should have kept names 3
(`resources`, `read-only`, `cap-drop`). So one failed `update` loses `--cap-drop ALL` and
`--read-only`, and the next successful one makes the loss permanent. The test now reports the cut
findings first.

### Each record a reader decodes

Test `daemon::tests::a_record_written_partway_is_not_kept`, the generalisation of
2097de41's `a_request_written_partway_is_not_kept`. For each record:
1. It is written by `keep_whole` over what it replaces, under the same fork and limit, at half
   and at the longest part its reader takes for another record.
2. Each reader is the one the daemon or its client uses:
   - `Run::decode`;
   - `cpu::decode` of the first line, as moves.rs `read_baseline`;
   - `shards_ipc::Named::parse`;
   - a `SocketAddr` parse of the trimmed text, as context.rs reads `listening`;
   - the trimmed text, as networks.rs `engine_id` reads it.

Red, in place:

```
"request cut at 81 of 163: kept, 81 bytes, read as none",
"request cut at 115 of 163: kept, 115 bytes, read as another: \"cap_drop [], read_only false, cpu_shares 0\"",
"request cut at 99 of 198: kept, 99 bytes, read as none",
"request cut at 115 of 198: kept, 115 bytes, read as another: \"cap_drop [], read_only false, cpu_shares 0\"",
"cpu-baseline cut at 32 of 65: kept, 32 bytes, read as another: \"[(c020, 11010011), (c030, 0)]\"",
"cpu-baseline cut at 64 of 65: kept, 64 bytes, read as another: \"[(c020, 11010011), (c030, 2211120), (c038, 10112)]\"",
"daemon.pid cut at 6 of 13: kept, 6 bytes, read as another: \"Named { pid: 4242, start: Some(1) }\"",
"daemon.pid cut at 11 of 13: kept, 11 bytes, read as another: \"Named { pid: 4242, start: Some(123456) }\"",
"daemon.stopping cut at 6 of 13: kept, 6 bytes, read as another: \"Named { pid: 4242, start: Some(1) }\"",
"daemon.stopping cut at 11 of 13: kept, 11 bytes, read as another: \"Named { pid: 4242, start: Some(123456) }\"",
"listening cut at 8 of 16: kept, 8 bytes, read as none",
"listening cut at 14 of 16: kept, 14 bytes, read as another: \"192.0.2.10:443\"",
"engine-id cut at 32 of 64: kept, 32 bytes, read as another: \"77777777777777777777777777777777\"",
"engine-id cut at 63 of 64: kept, 63 bytes, read as another: \"777777777777777777777777777777777777777777777777777777777777777\"",
```

What each cut means to its reader:
- **CPU baseline.** A cut one names fewer registers or a lower field. A move of the microVM is
  checked against it, so a machine with less than the guest was told could take it.
- **`daemon.pid` and `daemon.stopping`.** A cut line names the daemon with another start, so a
  client takes it for gone. Cut after the ID, it names a process by its ID alone, the reuse
  `Named` exists to avoid. Cut inside the ID, it names another process.
- **`listening`.** A cut address names another port, and an invitation made from it names that
  port.
- **`engine-id`.** A cut ID is another ID, and its hash is the home's IPv6 ULA pool.

### Two writers of one request at once

Test `daemon::tests::a_request_kept_at_once_is_whole`:
- Two threads each keep a request 1,000 times over one microVM's, as an `update` and a `network
  disconnect` do: two encodings of different lengths.
- The test's thread reads the request while they write.

Every client is served on a thread of its own (daemon.rs, `spawn_scoped` per connection), so two
clients may rewrite one microVM's request at once.

| Writes | Runs | Writes failed (of 2,000) | Reads with no whole request |
| --- | --- | --- | --- |
| In place (red, red-first, mutation 1) | 3 | 0, 0, 0 | 4,799 (reads not counted then); 5,029 of 5,906; 4,710 of 5,649 |
| One name beside it for both, `<record>.new` (`one-name.diff`) | 3 | 789, 840, 660 (`ENOENT` on rename) | 2,080 of 27,629; 4,175 of 31,846; 1,572 of 18,867 |
| A name of each write's own (green, mutation 2, gates; and both `daemon::` suites, 0 failed) | 3 logged | 0 (the test passed) | 0 (the test passed) |

With one name, writer B's open truncates the file A has just written, A's rename makes that
file the record, and B's rename finds no file. That is 2097de41's `request.new` applied to every
writer: whole-or-absent does not hold for two writers under one name.

## The fix

`keep_whole(path, bytes)` (daemon.rs:8799) takes the place of `keep_request`:
1. It writes `<name>.new.<n>` beside the record, where `n` is a count of this process's writes.
2. It renames that over the record.
3. On any error it removes its own `<name>.new.<n>` and returns the error to the site.

It is private to `crate::daemon`. Rust's privacy reaches every module below it (`update`,
`networks`, `engine::own_networks`, `remote`), as `lock` already does, so no visibility changed.
No helper was duplicated: every site calls this one function. Each site keeps its log line and
error handling:

| Site (committed) | Record | Error handling, unchanged |
| --- | --- | --- |
| daemon.rs:3537 | `request`, new microVM | the log line "keeping its request: …; it cannot be started again" |
| update.rs:215 | `request`, `update` | `cannot(e.to_string())` to the client |
| networks.rs:531 (method `keep_request`) | `request`, `network connect`/`disconnect` | `e.to_string()`, to the client by `daemon(&e)` |
| engine/own_networks.rs:724 (`change_request`) | `request`, an engine's own network | `(500, "container {id}: {e}")` |
| daemon.rs:4189 | `cpu-baseline` (D176 part 6) | the log line `"{path}: {e}"` |
| daemon.rs:1751 | `daemon.pid` | `"{path}: {e}"`, returned by `serve` |
| daemon.rs:6660 | `daemon.stopping` | the log line `"{path}: {e}"` |
| remote.rs:1664 | `listening` | `"{path}: {e}"`, returned by `listen` |
| networks.rs:447 | `networks/engine-id` | ignored (`let _`), as before |

### A name of each write's own (decision)

Alternatives, against the measurements above:
- **One name per record (2097de41).** Breaks under two writers: `one-name.log`.
- **moby/sys `atomicwriter`** (main `a2d6b3d5`, read for 2097de41). It uses `os.CreateTemp` in the
  target's directory, a random name for each write. A count does the same within the one daemon
  that holds a home, which `take_lock` enforces: no random source, no new failure path. The
  writers of a home's records are that daemon's threads alone.
- **One lock around every write** (a static `Mutex` in `keep_whole`). It would put every
  microVM's writes behind one lock, which waits on rename(2): PM M46 measured 4.4 ms at the median
  on a busy macOS host. A forked test child could also find it held.
- **A lock per microVM across read, change and write.** It would also close the lost update
  below. But `update` holds the request it read across an exec into the VM (`TAKE_TIMEOUT`), so
  it is a design question, not this fix's.
- **`O_EXCL` on one name.** A writer finding the name taken could only fail or wait, and a
  crash's leftover would refuse every later write.

What is left: a crash between write and rename leaves `<name>.new.<n>`.
- No reader opens it.
- The daemon's scans of a microVM's directory filter by the names they read (log segments).
- A move ships the directory whole, so the leftover would travel with it.
- `rm` removes it.
- 2097de41's fixed name was overwritten by the next write; a counted name is not reused.

### Durability: unchanged

Not synced, as 2097de41 decided for the request and D27 for every microVM's record. A synced
`request` beside an unsynced `config.json` makes nothing survive a power loss that the record
does not, and on macOS std's `sync_all` is `F_FULLFSYNC`. The reasoning, sources and power-loss
gap (ext4 `auto_da_alloc` covers only a rename over an existing file) are in
`../audit-fix-request-v1/README.md`, "Durability".

## Other in-place writes in the daemon (the search)

Searched: the production code of `crates/shards/src/daemon.rs` and `daemon/**`, test modules
left out. Patterns: `fs::write(`, `File::create(`, `OpenOptions` with `.write(true)`,
`.truncate(true)` or `.append(true)`, `create_private(` and `disk.write(`. Each hit was read with
its readers.

**The same defect, fixed here.** In-place rewrites of a record a reader decodes, each reproduced
above:
- daemon.rs:4189 `cpu-baseline`;
- daemon.rs:1751 `daemon.pid`;
- daemon.rs:6660 `daemon.stopping`;
- remote.rs:1664 `listening`;
- networks.rs:447 `engine-id`.

**Not this defect, left as they are:**
- **The import's staging, moves.rs:1536 (`request`) and :1549 (`config.json`).** Both are written
  in the import's private directory, `home/tmp/move-<hex>` (`private_staging`). That directory
  becomes the microVM's only by one rename of the whole, after every write succeeded. Any error
  drops `Staging`, whose `Drop` removes it. No reader opens a staged file.
- **Already written beside and renamed:**
  - ends.rs:120, under `slowest`'s lock;
  - restart.rs:105 (costs);
  - layers.rs:605 (restarts);
  - ship.rs:764 (cursor);
  - ship/learned.rs:118;
  - logs.rs:297 (index);
  - daemon.rs:524 (Windows' engine pipe name);
  - containers.rs records (`config.json.new`, renamed to `config.json`).
- **Appends whose readers take whole lines or records and tolerate a torn tail:**
  - events.rs:137 and :161;
  - processes.rs:89;
  - daemon.rs:8779 and :8943 (logs);
  - managed.rs's `Ledger`, which also cuts a failed append back.
- **managed.rs `generation`.** An 8-byte counter, overwritten in place under `flock`, durably
  synced, its length checked on read. RLIMIT_FSIZE cannot cut it, since the file does not grow.
  Seen in source, not reproduced as this defect.
- **Not records:**
  - swap and layer images (daemon.rs:198, :222, :5992; layers.rs:230, :714, :798, :950);
  - disks and VFIO groups (devices.rs);
  - a snapshot's swap copy (snapshots.rs:309);
  - a move's placed files (moves.rs:782, :1659, :1702, :1722, :1740);
  - kernel cgroup files (parents.rs:244; groups.rs:99, :257);
  - lock files (`daemon.lock`; remote.rs:729).
- **Status files no shards reader decodes: groups.rs:426 `memory.current` and :443
  `cgroup.procs`.**
  - The macOS and Windows emulation of a group (D183) rewrites them in place each hold pass, for
    users' tools.
  - budget.rs's readers read Linux's own cgroup files.
  - A user's tool reading one mid-rewrite can find it empty or cut. This is the same mechanism,
    but not a record shards reads back. Listed for ROOT; not changed.

## Runs

| Step | Source | Result | Log |
| --- | --- | --- | --- |
| Red (first) | 032b63b1c + `red-first-source.diff` (daemon.rs `c17b4f95`) | 3 FAILED. The update test failed at its un-cut check (above). | `red-first.log` |
| Red | 032b63b1c + `red-source.diff` (`eadcf0b1`) | 3 FAILED, as above | `red.log` |
| Green, as tested | 032b63b1c + the green source (`84c850da`) | 3 ok | `green.log` |
| One name | green + `one-name.diff` (`ac63a8de`) | the two-writer test FAILED, 3 of 3 runs; the others ok | `one-name.log` |
| Mutation 1, guard removed | green + `mutation-guard.diff` (`023b1d39`): `keep_whole` writes in place | 3 FAILED, the red findings | `mutation-guard.log` |
| Mutation 2, one site's guard removed | green + `mutation-site.diff` (update.rs `05990b65`): `update` writes in place | the update test FAILED with the red findings; the other two ok | `mutation-site.log` |
| Gates, committed | 032b63b1c + `green-source.diff` (`8b5b5c72`; `tested-to-committed.diff`: a doc comment and a type alias in the tests) | see below | `gates.log` |

Gates on the committed source (`gates.log`):

| Gate | Result |
| --- | --- |
| `cargo fmt --all --check` | exit 0 |
| The three tests | ok |
| `cargo test -p shards --bin shards -- daemon::` | 331 passed, 0 failed, 7 ignored (ROOT's native fixtures, a measurement, a child helper) |
| `cargo clippy -p shards --bin shards -- -D warnings` (x86_64-unknown-linux-gnu) | exit 0, profile_test false |
| The same with `--profile test` (cfg(test)) | exit 0, profile_test true |
| `scripts/check-development-policy` (on the Mac) | passed |

The first gate run (`gates-first.log`) failed clippy's `type_complexity` on the test's `cases`
type, which a local alias now names.

## The tests' forked children

`under_limit` forks the test process, which has other threads. The child allocates, and the
update test's child takes the test daemon's own locks, which no thread holds: `t.run` has
returned, so the daemon's threads have ended. The allocator's locks are released for the child:
- glibc `posix/fork.c` (master, 1991-2026): `__malloc_fork_lock_parent` before `_Fork`,
  `__malloc_fork_unlock_child` in the child.
- musl `src/process/fork.c`: `__malloc_atfork(-1)` before `_Fork`, `__malloc_atfork(!ret)` after.
- Apple Libsystem `init.c`: `libSystem_atfork_prepare` calls `_malloc_fork_prepare`, and
  `libSystem_atfork_child` calls `_malloc_fork_child`.

All three were read 2026-11-12. A child not ended within `PATIENCE` (20 s) is killed, so a
deadlock would fail its test rather than hang the suite.

## Owed or held by others

1. **Performance (R08/R14, ROOT).** One more rename(2) where the writes were in place:
   - each start, not resumed or joined, of a microVM whose home keeps a CPU baseline;
   - `update`, `network connect` and `disconnect`, and an engine's own network change;
   - the daemon's start and stop (`daemon.pid`, `daemon.stopping`, `listening`);
   - a home's first `engine-id`.

   None is measured. The new-microVM path's rename was already owed (2097de41).
2. **Unverified (R11):**
   - macOS arm64: not built, by instruction.
   - Windows: the tests are `cfg(unix)`. A rename over a record a reader holds open relies on
     std's `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`, then `FileRenameInfoEx` with POSIX semantics
     on access denial. Not run.
   - Linux musl and aarch64; any target but x86_64-unknown-linux-gnu for clippy and tests.
   - No E2E and no real VM: those are ROOT's.
3. **A lost update.** Two read-change-writes of one request at once (an `update` and a `network
   connect`) can lose one's change: each reads, the last rename wins. Seen in source, not
   reproduced (R09). It needs a per-microVM serialization (above).
4. **`engine-id`'s first making.** Callers that race to make it each make an ID. The file keeps
   one whole, but a caller in the race may have used another. Seen in source, not reproduced.
5. **The emulated group's status files** (above): not changed.
6. **`<name>.new.<n>` left by a crash:** not swept.
7. **A design record.** This decision is recorded here, and D37's entry in
   `docs/design/architecture.md` (not an owned path) does not mention it.

## R01–R16

| Rule | Account |
| --- | --- |
| R01 | `keep_whole` returns std's `io::Result`. It has no unwrap, indexing or unchecked arithmetic: `fetch_add` wraps. Its removal is best effort (`let _`), as 2097de41's was. Test-only code panics and indexes, as clippy.toml allows. |
| R02 | Alternatives, the sources read and the decision are above. |
| R03 | Production adds no number. `.new.` is a name, after `request.new` and `config.json.new`. The count is a counter. The tests' cuts derive from each encoding, and `WRITES`'s doc comment gives its basis, the runs above. |
| R04 | No new state. Errors stay `io::Error`, handled at each site as before. The temp file's life: written, then renamed, or removed on any error. |
| R05 | No wait or cache is touched. One function replaces one (`keep_request`) at the existing seam. The tests reuse the `Test` harness and `ask`'s client, whose `Asker` was moved to `asker()`. |
| R06/R13 | A nine-line function and one-line site changes. Tests share `under_limit`, `cuts` and `not_whole`. |
| R07 | The doc comment gives the invariant, why each write has its own name, and why there is no sync, with their evidence. |
| R08/R14 | Unmeasured; owed to ROOT (above). |
| R09/R10 | Red, green, the one-name counterexample, two mutations and the gates are live, with logs and diffs here. The update site's red is on its own unchanged code. The other records' red is each record's bytes and reader under the sites' former write. |
| R11/R12 | std only, one implementation for every platform. Verified on Linux x86_64 glibc only; the rest is listed above. |
| R15 | Implementer: light checks on the assigned runner, no E2E, benchmark or campaign. One mistaken sync (below). |
| R16 | On 8fadc1ec: this worktree's synced copy and its `target` (3.4 GiB, regenerable) at `/home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/request-sites-v1-1548cdb1b8`; `/home` 1.6 TB free. Nothing was built on the Mac. |

## Operations

**A sync to the wrong folder.** One sync-only probe on adacachy (`451397fcc585f7f7cb9aaf00e42064bb`),
tried while 8fadc1ec's syncs failed, was run without kiln's `cwd`. So kiln synced the session's
folder, the g121 worktree (`e5b787503`, clean), to adacachy's
`/home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/g121-5dc8e2cd12`:
- files_sent 1294, removed 243, 991 MB;
- the probe then ran only `pwd`, `git log -1`, `git status`, `sha256sum` and `uptime` there.

That folder is the g121 session's own copy: any file there that its local tree lacks, outside
what kiln leaves home, was removed. Every later run named this worktree in `cwd`. The three
failed attempts on 8fadc1ec before it also lacked `cwd`, but synced nothing.

## Files

| File | Contents |
| --- | --- |
| `POLICY_READ.json` | The policy receipt. |
| `commands.sh` | Every command, verbatim. |
| `red-first-source.diff`, `red-source.diff`, `green-source.diff`, `one-name.diff`, `mutation-guard.diff`, `mutation-site.diff`, `tested-to-committed.diff` | The sources tested, against 032b63b1c or the green source as tested. |
| `red-first.log`, `red.log`, `green.log`, `one-name.log`, `mutation-guard.log`, `mutation-site.log`, `gates-first.log`, `gates.log` | Raw outputs as kiln returned them. Each header says what its command dropped. |
