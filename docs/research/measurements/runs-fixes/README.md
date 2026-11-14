# Runs review fixes, 2026-11-13

Implementer: Codex, p/fix-runs-v1. Main coordinator: ROOT. Policy at base
9efde72524dd626f3ff84e6b9891852d7c810a9b: AGENTS SHA256
f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41;
rules df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3;
evidence 61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63;
CLAUDE 588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010.
Read both /tmp/g121-shot/review/fix-brief.md and /tmp/g121-shot/brief-common.md.
No children. All cargo commands run through kiln on adas-mac-studio.local,
macOS arm64, in ~/g121-tmp/fix-runs-v1/source, with CARGO_INCREMENTAL=0,
CARGO_TARGET_DIR=../target (absolute), SHARDS_HELPERS=skip, one cargo at a time.
Local work is edits/format/git only. The tiny carrier lives at
/tmp/g121-shot/fix-runs-carrier; its patch is applied to the recorded revision.

## runs-01

Source: base above plus the server owner-metadata patch in the fixing commit.
`cargo test -j 4 -p shards-vmm --lib owner_metadata_failures_refuse_creation_and_lookup`
went red in kiln r4: injected owner-write EIO was acknowledged 0, expected -5.
[Red log](runs-01-red.log). The same test passed in r7 after correction;
`cargo test -j 4 -p shards-vmm --lib devices::virtio::fs::server::tests` passed
49 tests and `cargo clippy -j 4 -p shards-vmm --all-targets -- -D warnings`
passed. [Green log](runs-01-green.log). An intermediate module check (r6)
exposed symlink ELOOP and pinned-unlinked ENOENT regressions; the correction
keeps native special nodes root-owned and reads pinned metadata through its fd.
The final module gate includes these preexisting regressions and passes them.
No guard-removal mutation or live VM was run.

Owner syscall fault injection is test-only, thread-local and restored by a guard.
Production uses existing typed FUSE errno outcomes; absent metadata alone permits
run-owner fallback. Missing/failed opens, corrupt metadata and failed owner writes
do not silently grant ownership. Creation failure removes the new file/directory;
cleanup failure propagates its errno. No new runtime numbers, cache, wait or
platform adapter. Existing platform xattr calls carry the same test hooks.

R01/R04/R06/R07/R13: scoped source review and focused tests/clippy above; no new
production panics or string parsing. R02/R03/R05/R12: existing FUSE/xattr boundary,
no broader architecture, learned values or speculative manager introduced. R09/R10:
actual unit red/green retained. R08/R14: no performance claim or local profiling;
ROOT owns it. R11: one macOS arm64 unit host; Linux/Windows CI and guest effects
unverified. R15: signed ff land after main merge; no main checkout or force push.
R16: own target directory removed after the lane's final check, no shared cache
or user data removed. The narrow brief prohibits local tests and broad gates.

Owed ROOT: `cargo test -p shards --test volumes a_binds_host_files_are_its_runs_users`
on supported real-VM hosts, plus a real bind xattr-failure permission/cleanup
campaign; seven-target CI and `scripts/check-development-policy`. Existing live
case does not replace the fault campaign. No performance/portability pass inferred.

## runs-02

Base aaeee3b1935cde71f93a1de90a6bd86ab80c121e plus the daemon patch.
`cargo test -j 4 -p shards --bin shards renamed_home_cleanup_keeps_replacement_files`
red in resumed kiln r5: the old daemon overwrote the replacement home's stopping
marker, returning its own Named process bytes instead of `replacement`. This is a
unit subprocess (no daemon loop and no VM). The same test is green in r7 and r12;
r12 additionally observes removal of the old home's remainder after unlinking its
lock. `cargo test -j 4 -p shards --bin shards
daemon::tests::a_closed_daemon_holds_its_stopping_lock_until_its_home_is_let_go_of`
also passes (one test); `cargo clippy -j 4 -p shards --all-targets -- -D warnings`
passes (r12, 22.85s). Same macOS arm64 runner/target as runs-01. Unit builds name
/usr/bin/true as SHARDS_INIT_BINARY and SHARDS_SERVER_BINARY; these tests use no
guest binary, and no runtime/VM claim follows from the clippy gate.

Existing CWD ownership is authoritative. Unlike a check against the original
pathname before each operation, relative paths preserve identity across a rename
between a check and a write. Windows' CWD prevents renames; its path is unchanged.
The helper has existing String presentation errors; no new refusal protocol,
number, cache, resource limit or wait. The parent test never changes its CWD; the
child owns its CWD, paths and cleanup. R01-R16 dispositions are the scoped ones
above; no wider live correctness or platform claim.

Owed ROOT: extend/run the rename-and-recreate scenario with two actual daemons
(and live VM traffic), checking both homes' offers/PID/stopping files and workload
progress. Existing removal case: `cargo test -p shards --test daemon
a_daemon_whose_home_is_removed_exits_without_making_it_again`; relative-home case:
`cargo test -p shards --test daemon a_home_named_relatively_is_one_home`.

## Landing coordination

Main has advanced during repeated runs-01 merge/test/push attempts; every rejected
non-fast-forward push was left rejected. No force push. Main was merged through
5d805549d, with the selected regression rerun on aaeee3b1. The pending landing
window was requested from ROOT through kiln (429 backpressure) and the authorized
local Claude peer socket (transport sent, read acknowledgement not observed).
Independent runs-02 confirmation/preparation continued while that window was
pending. An unlanded fixing SHA is not described here as a main result.

## runs-04

Base aaeee3b1935cde71f93a1de90a6bd86ab80c121e plus the API patch.
`cargo test -j 4 -p shards-api --lib a_dead_warm_host_is_definitely_untaken`
red in kiln r13: a real stand-in process wrote WARMED and exited, was observed
gone, and its take was refused instead of definitely untaken (1 failed). In r14
the same test passes (1); `cargo test -j 4 -p shards-api --lib image::pool::tests`
passes 5, and `cargo test -j 4 -p shards-api --lib image::session::tests` passes
30. `cargo clippy -j 4 -p shards-api --all-targets -- -D warnings` passes (19.68s).
Same macOS arm64 runner, owned target and environment as above; no VM was started.

`Taken::Untaken` is an internal typed outcome, proven only before CREATE is
attempted. Both the watcher-reaped state and the child process's terminal state
are checked because either can win the race. The pool replaces that spare with a
fresh host. A death after this check keeps the existing typed failure; a CREATE
write may have reached the host, so the run is not replayed. There is no new
wire variant, chosen number, cache, periodic probe, or process-lifetime owner.
The pool's existing keep-alive bounds unused spares; dead spares are discarded at
take, rather than adding a second death-notification path to the pool. Existing
R01-R16 dispositions above apply; no performance or live-VM claim.

Owed ROOT: `cargo test -p shards --test api_image` on supported real-VM hosts,
with a warmed host killed/reaped before take and a second kill during CREATE;
require one replacement run in the former case and a typed non-replayed failure
in the latter. The existing suite needs this precise fault scenario added.

Landing gate updates: f8884c10853b8483c51a6de604d16404dca8b5a6 passed
runs-02's rename/cleanup and stopping-lock tests (one each) and
`cargo check -j 4 -p shards-vmm -p shards --tests` (kiln r15). After another
non-fast-forward rejection, 0e90f5866d010aac5c2e9d98e7c35f2c3b176a78 merged main
18a17cb96; the same cargo check passed (r16, 16.27s), per ROOT's amended
contention rule. Neither rejected push changed main. Main's RunEnded protocol
change was retained while resolving the runs-04 stash's two test-frame conflicts;
the focused API gate is rerun after that resolution.
