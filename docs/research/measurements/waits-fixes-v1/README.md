# waits review fixes, 2026-11-13

Implementer for ROOT, branch `p/fix-waits-v1`, starting at
`9efde72524dd626f3ff84e6b9891852d7c810a9b`. Owned findings: waits-01, 04, 08,
11 and 12. The other judge and registry findings belong to Claude lanes.
The binding inputs are `/tmp/g121-shot/review/fix-brief.md` and
`/tmp/g121-shot/brief-common.md`; their current instruction authorizes focused
remote unit tests, clippy, signed commits and fast-forward landings.

POLICY_READ (implementer): AGENTS SHA-256
`f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41`;
development-rules `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`;
development-evidence `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63`;
CLAUDE `588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010`.
All four were read at the starting revision. Initial Kiln notification was
refused with backpressure (pending=3); this record retains the readback.

## waits-01: one authoritative word judge

Red source: `bb799b90da0d8f0c984f90a790d6e322a3c5098e`.
`cargo test -p shards --bin shards daemon::groups::tests::a_pending_member_word_is_not_a_failed_give_back -- --exact`
failed in Kiln r5 on `adas-mac-studio.local` (macOS arm64):
"a pending word was treated as a completed failed pass"; 0 passed, 1 failed.
This directly exercises the real groups verdict, with a pending word past W*.

Fix source: `8168e33f567bd9d6d9d37a3f56e290a36f6f8238`.
The word's existing `Wait::WhileRuns` judge is now used as `Wait::GroupWord`,
reporting its typed Cut when refused, before awaiting the guest's acknowledgement
of the exec kill. The groups thread consumes Waiting/Refused state; elapsed time
cannot invent a refusal. This preserves the ability to judge a genuinely stalled
member even when its kill is not acknowledged, while extending CPU progress,
host holds and the wait behind host-pressure reclaim. The wake is published under
the existing mutex/condvar protocol. No second progress reader, timer or cache.

Cold words retain the existing no-bound/liveness contract. The older parallel-group
fixture incorrectly required a cold word to be stopped after another word taught
the record; it now explicitly starts learned, so its own exec judge can refuse it.
No production constant or authority change. D183/D215 are the existing design;
the change removes the contradictory external elapsed-time verdict.

Kiln r6: groups module, parallel-member regression and shards all-target clippy
passed on the same host. Raw logs are retained beside this record when fetched.
Subsequent merged-tree validation is recorded below at landing.

## Rule accounting and limits

R01: fallible code remains under existing typed Cut/io paths; production adds no
unwrap/assert/indexing. R02/R05/R12: reuse the established exec judge and the
existing group notification seam; no new scheduler, process reader or cache.
R03: no new production figure; cold, learned and refusal semantics stay with that
judge. R04: explicit Waiting/Refused(Cut), terminal publication before kill-ack.
R06/R07/R13: remove the second timer and its redundant saying/since state.
R09/R10: actual red unit regression and subsequent green/module checks, with raw
logs; this is not a real-VM reproduction. R08/R14: no performance claims or local
profiling; comparative results and costs remain ROOT's. R11: macOS arm64 focused
checks only; Windows and Linux matrix remain unverified here. R15: scoped unit
checks only, no E2E/VM/CI campaign. R16: a private remote target, removed after the
last slice; no other actor's cache/target is changed.

ROOT owes real-VM `cargo test -p shards --profile test-release --test containers a_microvm_under_a_group_is_held_to_its_budget`
on macOS and the Windows group/Job scenarios; also a loaded/host-held member that
returns a positive give-back beyond its prior W*. These were not run by implementer.

### Merge validation, waits-01

Signed merge 63001feed (main 31d154797) passed all 22 focused groups,
parallel-member, health and reclaim tests in r14, macOS arm64. Merge 014fa8017
(main 5d805549d) was clean. Its repeated tests r16 passed groups 7 and
parallel-member 1; health passed 12 and failed its pre-existing cold-wait timing
assertion: the answer was correct, W* 798.948916 ms versus a sleep of 800 ms.
The answer thread starts before await_exit opens its measured interval, so that
comparison was invalid. The test still requires the successful answer without a
cut and a genuine nonzero learned interval. No production judge change.
Later clean merges use ROOT's amended check-only contention gate; conflicts
require focused reruns.

## Claude takeover, 2026-11-14 (waits-01 remainder, 04, 08, 11)

The four commits above were cherry-picked (signed) onto main `97f404be8`; PM M565 is
renumbered M575, as M565 and M574 were taken. Their SHAs on main: red `e4056b11f` (was
`bb799b90d`), fix `8d1356a32` (was `8168e33f5`), records `fd5889769`, test fix `aebd91dc4`. Reviewing them found a remainder of waits-01:
`WordState::Refused` stood from a room's refusal through the give-back after it and into the
next round. Carrier: a git bundle over main, cloned on adas-mac-studio-3.local (macOS arm64),
`CARGO_INCREMENTAL=0 SHARDS_HELPERS=skip`.

Red, at `21f8bee02` (every test added, no fix):
```
cargo test -p shards-ipc --lib slots::fifo::tests::a_readers_end_is_no_take      # [Began, Ended(57.125µs)]
cargo test -p shards-ipc --lib tests::an_unreaped_daemon_does_not_live           # "exited and unreaped, lives"
cargo test -p shards --bin shards daemon::tests::a_refused_room_is_not_the_next_rounds_verdict  # killed: kind 4 where 24
cargo test -p shards --test harness concurrent_keeps_never_shrink_a_kept_record  # Some(true) where Some(false)
```
Green at the fixed tip and its merge with main: those four, shards-ipc's lib (66 passed),
`daemon::groups` (7), `daemon::health` (13), `daemon::reclaim` (1), the daemon tests
`a_member_that_never_answers_holds_up_no_other_member_or_group`,
`a_members_room_holds_as_of_the_footprint_it_was_read_with` and
`a_microvm_that_does_not_give_back_keeps_no_other_waiting`, harness
`an_answer_already_there_teaches_nothing`, and clippy `-p shards-ipc -p shards --all-targets
-- -D warnings`. `harness.rs::a_wait_is_refused_though_its_own_polls_keep_its_daemon_busy`
fails at the base as well (its daemon cannot start with no helpers carried).

Owed to ROOT, unrun here (real VMs, other hosts):
- `cargo test -p shards --profile test-release --test containers a_microvm_under_a_group_is_held_to_its_budget`
  on macOS and Windows, a member under host hold returning a give-back past its prior W*.
- Linux: `cargo test -p shards-ipc --lib tests::an_unreaped_daemon_does_not_live` (the
  `/proc` state read; CI runs it).
- A Linux daemon started under a supervisor that defers reaping, exited before it served:
  `shards run` must end "the daemon ended before it served".
