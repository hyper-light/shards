# Confine P1-and-below fixes

POLICY_READ: role implementer, base `9efde72524dd626f3ff84e6b9891852d7c810a9b`.
Owned confine-03..09, with confine-10 subsequently assigned by ROOT. All four policy
hashes are recorded in `docs/bugs/2026-11-13/confine.md`; development-rules hash
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Binding briefs: `/tmp/g121-shot/review/fix-brief.md`, `/tmp/g121-shot/brief-common.md`.
No subagents. No builds/tests on the shared Mac. ROOT owns real VMs, campaigns,
benchmarks, CI and integration audits.

## Scoped evidence

All tests below use adacachy2, Linux x86_64, nice 10, CARGO_INCREMENTAL=0,
CARGO_TARGET_DIR `~/g121-tmp/fix-confine-v1/target`, one cargo at a time.
Init uses `--target x86_64-unknown-linux-musl` and rust-lld.
The tiny carrier is `/tmp/g121-shot/carriers/confine`; its source bundle contains
signed branch commits. Runner source is `~/g121-tmp/fix-confine-v1/source`.

- confine-03 red: `dcbec0e97`, kiln r3,
  `cargo test -p shards-init --target x86_64-unknown-linux-musl staged::tests -- --nocapture`:
  11 passed, 2 failed for the claimed hidden nested file / accepted read-only cover.
  [Raw log](staged-red.log). r1 was a carrier ref-export error before compilation;
  r3 corrected the bundle's advertised ref. Green at `5bcf45657`, kiln r4:
  staging 13/13 and static-musl init clippy clean. Canonical alias follow-up and
  merged-source checks are recorded at landing.
- confine-04 red: `265894e98`, kiln r5,
  `cargo test -p shards-init --target x86_64-unknown-linux-musl domains::tests::a_members_relative_path_is_resolved_in_its_working_directory -- --nocapture`:
  failed because valid PATH=bin was checked in init's cwd. [Raw log](path-red.log).
  Green uses real fork/chdir/exec of fixture scripts for relative, empty and missing
  leading PATH components. At `22ebba75a`, kiln r6, domains 13/13 passed; clippy found
  one needless borrow. At `d0fea6fb3`, kiln r7, domains 13/13 and init clippy passed.
- confine-05: the check/setup/insert race is unambiguous; the fix brief permits its
  correction while real-VM reproduction stays ROOT's. Daemon reuses request_turn;
  API-run serializes plug/unplug ownership separately from its readable mounted map.
  At `d0fea6fb3`, kiln r7: devices::tests 4/4 and api_run::hosted::tests 6/6 passed.
  Host clippy caught a new VM test diagnostic requiring Debug on common::Run;
  `0d53a7fd7` corrects that diagnostic without changing common::Run.
- confine-07 red: `0d53a7fd7`, kiln r8,
  `cargo test -p shards-init --target x86_64-unknown-linux-musl enter::tests::rejected_environment_values_never_enter_diagnostics -- --nocapture`:
  failed because Display contained the synthetic public credential sentinel.
  [Raw log](env-red.log). Fix `e272925b4` uses value-free typed variants.
- P0 ordering: merged ROOT's `60245cefb` in `f270eb8bf` after confine-01/02 landed.
  The one exec_child conflict preserves `rules -> confine -> chdir -> PATH search`,
  RULESET_FD=4 and all of ROOT's fd/Confinement changes.
- confine-08 native admission red/green and final merged checks: pending.

## confine-06 and confine-09: owed with design

Neither is called fixed. Root-only touches do not notify descendant inode watches,
and MOST=16384 is an upstream default, not executing-instance admission.
A replacement constant, simply growing the queue, or a root-only ordinary event
would preserve the defect.

Proposed design: retain a typed Exact(path) versus Subtree/Lost(scope) event through
host intake and delivery. Mac MUST_SCAN_SUBDIRS and Windows overflow produce Lost;
pending storage exhaustion does too. A loss is a per-share durable dirty state that
cannot itself be dropped. Delivery snapshots the server's actually held directory
and file identities, enumerates the affected subtree, and wakes every retained inode
(or delivers a loss signal to each affected watcher), including file-only watchers.
Bound retained pending work by the server's live retained objects/admitted byte
budget; forget must retire pending identities, and batches must acknowledge delivery
before releasing their charge. Start cold with no guessed allowance: derive from
actual held objects and resource admission, refusing by type if it cannot be backed.
Do not infer guest inotify capacity from the host's default. This needs a protocol and
kernel-effect design and real-guest validation; source-only tests of an ancestor path
would not prove the required wake. D235 currently documents the incomplete mechanism.

ROOT must extend/run `cargo test -p shards --profile test-release --test plug a_host_edit_wakes_a_watch_in_the_guest -- --nocapture`
with a nested file-only watch and independent Mac MUST_SCAN_SUBDIRS, Windows zero-byte
overflow, and pending-capacity loss injections. Preserve the final host bytes and
observe an actual guest wake/loss, not merely a queued root record. The existing
basic case alone does not close confine-06. For confine-09, establish admission and
cold-start behavior from actual retained-object/resource inputs, then vary them and
show the capacity follows them. No production queue change is made in this slice.

## ROOT's real-VM queue

- confine-03/04: `cargo test -p shards --profile test-release --test api_image staged_credentials_reach_the_harness_and_its_output_and_end_come_back -- --nocapture`,
  extended with nested HOME mounts/read-only covers and relative/empty PATH members.
- confine-05 authored checks:
  `cargo test -p shards --profile test-release --test plug devices_are_plugged_into_a_run_and_its_disk_grows -- --nocapture`;
  `cargo test -p shards --profile test-release --test api_image a_project_is_plugged_into_the_running_harness -- --nocapture`.
  Both now ask twice concurrently for one destination; require one owner, one refusal,
  and daemon's single unplug must leave no hidden mount. Also observe device/share/fd
  return and run a deterministic barrier schedule in the real device path.
- confine-06/09: the loss/admission design and extended real-guest command above.
- confine-07: malformed public-sentinel env via a managed exec lane; verify the complete
  refusal wire/stderr/telemetry stays value-free. `cargo test -p shards --profile test-release --test member_exec -- --nocapture`.
- confine-08: native watch quota exhaustion/recovery with actual guest lookups/watches;
  extend the same plug host-edit test. The unit test exercises a real native EACCES
  admission error without modifying global kernel quotas.

## R01–R16 account

R01: no unwrap/index/panic in added production paths; async child search uses only
prepared buffers and kernel calls. Scoped clippy and failure checks are recorded above.
Allocation/resource profiling remains ROOT's. R02/R05/R12/R13: existing member mount
plan and backing-directory walker, ordinary exec candidate composer, daemon request
turn and API-run ownership seam are reused. No new helper binary/framework/cache or
polling loop. R03: no replacement timeout/count chosen; confine-09 remains owed with
its derived-admission design. R04: destination and environment refusals are typed;
legacy setup strings render them at the existing boundary. R06/R07: focused routing,
search and ownership changes; comments document alias/mount/fd and child obligations.
R08/R14: no performance claim or benchmark; all comparative/profiling gates ROOT's.
R09/R10: actual targeted red/green commands and logs above; VM-only effects explicitly
owed. R11: one Linux x86_64 host, init static musl and host glibc; remaining matrix and
real hosts are CI/ROOT's, not claimed passed. R15: implementer only, signed commits,
P0-first merge order; focused remote checks only, no campaigns or main force-push.
R16: only this lane's target directory is removed after landing; retain evidence and
source until the run completes. No user data or other lane's builds are touched.
