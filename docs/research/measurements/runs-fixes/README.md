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
reads pinned metadata through its fd. The native nondevice ownership mapping was
subsequently corrected by the follow-up below.
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

Runs-04 merged/resolved gate: 0e90f5866d010aac5c2e9d98e7c35f2c3b176a78 plus
its API patch passed the dead-spare regression (1), pool module (5), session
module (32, including main's new RunEnded/channel cases), and API all-target
clippy (2.01s), kiln r18. Thus both conflicting test-frame edits retain main's
new protocol and pass the focused tests. Fixing commit: bc0f69b5c.

## runs-03

Base 3956d259bd0d674b49b60708d30340fb767fd7d2 plus the session patch.
`cargo test -j 4 -p shards-api --lib a_stop_cancels_a_create_whose_host_never_reads`
red in kiln r20: `blocked CREATE did not end stopped: Err(Timeout)` (10.08s).
The stand-in never reads stdin, and the valid request stages half MAX_PAYLOAD
in a member file. The test observes the real writer mutex held before stopping;
its failure cleanup kills the owned process and unblocks the writer before the
assertion. Green r22: same regression 1 passed (0.05s), then
`cargo test -j 4 -p shards-api --lib image::session::tests` 33 passed (2.37s),
and `cargo clippy -j 4 -p shards-api --all-targets -- -D warnings` passed (1.22s).
Same macOS arm64 runner/owned target, stand-in processes and no VM.

CREATE moved into the already shared start operation, after BootStop::hold;
both fresh and warm callers use it. Registration spans encode/write/start wait,
and every success/error follows release. Cancellation kills the group without
acquiring the blocked sender's mutex. Its typed Stopped end overrides the write
error only after the existing watcher/diagnostic/group-end proof; ordinary send
failure remains refused. No new cache, manager, production constant or wait.
The test's resource bound/deadline is test-only. Existing R01-R16 dispositions
apply; no real guest cleanup or cross-platform pass is inferred.

Owed ROOT: `cargo test -p shards --test api_image
a_stop_mid_boot_ends_the_run_host_and_every_process_of_its_run` on supported
HVF/KVM/WHP hosts, extended with a valid large staged CREATE while its reader is
held; test fresh and warm paths, host/VM/network/share end, slots/layers/templates
and locks returned. Windows' job branch is compiled by CI, not tested here.

## Confirmed main landing

The ff-push `2c2dcf559 -> 3956d259b` succeeded: runs-01 (978d29f03),
runs-02 (d3b11258a) and runs-04 (bc0f69b5c) are on main. In addition to the
merged gates above, b6e302bf4 and 3956d259b passed
`cargo check -j 4 -p shards-vmm -p shards -p shards-api --tests` (r19/r21,
13.53s/16.38s). Each merge signature was made by gits; fixing/status signatures
were checked with one gpgsig. ROOT's kiln inbox rejected the distinct landing
receipt with 429 backpressure; no accepted model delivery is inferred.

## runs-05

Source c1b53393c935599a252d8cc070ed1c4a6f8a30a8 plus the api_run patch.
The path is unambiguous: Shaped/Plan own Prepared::lease, warm waits for CREATE
while owning it, and serve retains it through workload end; Store::collect
exclusively locks the same .lease. The daemon drops its preparation lease once
its VM has the rootfs. The API now drops it after launch/readiness/template
settlement, before WARMED/CREATE wait and at serve_launched entry.

No live red was run: the report's exact forbidden effect requires a ready real
VM and is owed to ROOT under the fix brief. This is a source-proven lifetime
correction, not a claimed GC/VM reproduction. `cargo test -j 4 -p shards --bin
shards api_run::tests` passes 43 (kiln r24, 0.04s); `cargo clippy -j 4 -p shards
--all-targets -- -D warnings` passes (16.99s), on the same Mac arm64 runner and
owned target, with unit-only /usr/bin/true guest fixtures. The existing template
settlement and shape tests pass; they do not observe the lease of a live VM.

No new cache, poll, state variant, constant, panic or refusal. Existing lease
Drop releases its file lock before close (M244). R01-R16 dispositions above
apply with R09's live GC counterexample and R11's guest ownership explicitly
unverified. ROOT: extend `cargo test -p shards --test api_image
an_images_next_run_restores_the_template_its_first_saved` and the warm-pool
case: retain a ready run and separately a WARMED spare, request collection of
unrelated garbage, require collection completes while both stay alive and
workload progress continues, and check template restore still succeeds. Exact
new regression filter to add/run: `a_live_api_run_and_a_warm_host_do_not_hold_up_collection`.

Runs-03 landed via c1b53393c. Its last merge added only crates/net and docs,
outside shards-api and its transitive inputs (no dependency on shards-net):
focused gate was 3956d259b plus its patch; the disjoint c1b53393c was pushed
without a repeated gate, per ROOT's amended rule.

## runs-06

Red source 46f2f014059b5d70d4b0e3ba1e4c719cc59c8ae2 plus the fact helper
refactor/test (the selected argument intentionally unused, original preference
behavior retained). `cargo test -j 4 -p shards --bin shards
container_facts_keep_the_runs_selected_manifest` fails in r27: User was "1100"
(arm64) instead of "2200" (selected amd64). The fixture ingests real digest-named
OCI config/manifest/index blobs into a store; there is no VM or synthetic selector.

Green source aff4005928bd724517734fcf7d438bb4c12a76cb plus the inspect/commands
patch. Same regression passes (1), `cargo test -j 4 -p shards --bin shards
daemon::inspect::tests` passes 2, `cargo test -j 4 -p shards --bin shards
daemon::commands::tests` passes 11; `cargo clippy -j 4 -p shards --all-targets
-- -D warnings` passes (17.96s), kiln r29 on the same macOS arm64 runner and
unit guest fixtures. It checks both selected variants after preference changes,
legacy preference with no selection, and missing selection with no fallback.

Inspect and ownership-mode copy share image_facts, taking the record's immutable
manifest; explicit run user still wins. Config and descriptor reads reuse the
store's bounded digest-verifying held(), and only a missing selection field uses
legacy preference. Copy has typed ContainerGone/ConfigMissing outcomes at its
existing CLI rendering boundary, and refuses a missing config instead of guessing
root. A present config without User keeps OCI's root semantics. No new runtime
number, cache, wait, adapter or platform emulator. R01-R16 dispositions above
apply; the unit store result is not proof of actual guest extraction ownership.

Owed ROOT: extend/run `cargo test -p shards --test platforms` with one index whose
arm64 and amd64 configs have distinct User/Env, start both variants, compare guest
uid/env to container Config/ImageManifestDescriptor, copy with ownership mode,
and check extracted uid for each after preference changes. Add/run filter
`a_containers_facts_and_copied_owner_keep_its_selected_platform`. No emulated VM
or copy execution was run here.

Runs-05 merged compile gates after its api_run unit/clippy gate: 160a2836 (r25,
14.88s), 46f2f014 (r26, 13.25s), aff40059 (r28, 13.66s), each
`cargo check -j 4 -p shards --tests`. Pushes rejected while main advanced were
not forced. Prepared runs-06 while the runs-05 landing retry remained pending.

## runs-01 follow-up: native nondevice owners

A source check found my ENODATA classification had also made native symlinks
(and Linux FIFOs/sockets) root-owned, though only device nodes have that rule.
The added `unrecorded_nondevice_nodes_keep_the_runs_user` test reproduces the
regression on af9477321467f1487cb9a15c8e17f99cc7d1960f plus its test: r34
returns uid 0 for the unrecorded symlink instead of the run's 1000. It is a
regression introduced by this lane, not an additional original finding.

The correction classifies only known native kinds as unable to carry owner
metadata. Devices stay root's; unrecorded nondevices keep init's owner mapping.
ENODATA from an ordinary file is still an error, not proof of absent metadata.
Neither lookup nor classification follows a symlink or opens a device driver.
Guest-created native kinds lacking owner-xattr support retain their existing
exception; cross-run owner persistence for them is not claimed by the ordinary
file/directory xattr fix. There is no new production constant, cache or wait.

`cargo test -j 4 -p shards-vmm --lib unrecorded_nondevice_nodes_keep_the_runs_user`
passes in r35; `cargo test -j 4 -p shards-vmm --lib
devices::virtio::fs::server::tests` passes 50 (including the EIO/malformed-owner
regression and root-owned /dev/null), and `cargo clippy -j 4 -p shards-vmm
--all-targets -- -D warnings` passes (3.04s). Same macOS arm64 runner/target;
no VM. Logs: runs01-followup-red.log and runs01-followup-green.log. R01-R16
and live guest/other-platform limits above remain explicit.

Further clean Shards merged compile gates before rejected pushes: 6f20a3f0
(r32, 10.64s) and af947732 (r33, 13.67s), `cargo check -j 4 -p shards --tests`.
The runner attempted its ff-push immediately after these gates, using its
existing GitHub SSH authorization; both were rejected because main had moved.
No force. These are compile successes, not main landings. The native-owner
follow-up's fixing commit is 8b2505f42; its source gate was af947732 plus patch.

## runs-07

Red on fc1bb4797fb686bc29529ec8a01beb8d62180c0e plus the regression:
`cargo test -j 4 -p shards --bin shards concurrent_end_writers_keep_each_others_maxima`
fails in r38, helper slice None rather than Some(8s). A stable-lock owner has
read the old record and is about to publish its own slower teardown; a second
actual Ends instance publishes a helper maximum without waiting, and loses that
maximum when the lock owner publishes. This exercises the real file operations,
with two opens of the OS file lock; it is a unit thread race, not a process/VM
campaign. Its waits and fixture durations are test-only.

Green on c6dd465259158831507ddc2ecac4eaa023236ab0 plus the Ends patch: same
regression passes (1), `cargo test -j 4 -p shards --bin shards daemon::ends::tests`
passes 5 (1.03s), and `cargo clippy -j 4 -p shards --all-targets -- -D warnings`
passes (19.61s), r39 on the same macOS arm64 runner and unit guest fixtures.
Logs runs07-red.log / runs07-green.log. No real VM or two-process campaign.

One stable file lock spans the reread/merge/write. Existing Locked unlocks
before close; existing platform write_in supplies unique create-new staging,
sync and whole rename. Each process keeps its learned maximum before a lock or
write error and can persist it later. The record's existing parser/cold fallback
and format are unchanged; this slice does not certify malformed/unreadable-record
recovery. No new selected constant, cache, poll or OS-specific implementation.
R01-R16 dispositions above apply. ROOT owes a two-process daemon/API update and
crash/publication campaign, with continuous complete-record readers and no lost
maxima: extend/run `cargo test -p shards --bin shards
daemon::ends::tests::concurrent_end_writers_keep_each_others_maxima` with
process actors and failpoints on the shared home; the unit race is not that proof.

ROOT landed the prepared runs-05/06 and native-owner follow-up via signed merge
c6dd46525 of fc1bb4797. ROOT states overlapping drift was documentation only,
with no repeated focused gate; CI owns that combination. Branch fast-forwarded
to c6dd46525 preserving the runs-07 regression. Policy hashes are unchanged.
From ROOT's new instruction, two consecutive lost ff-push races hand the tested
tip to ROOT, instead of further retry loops.
