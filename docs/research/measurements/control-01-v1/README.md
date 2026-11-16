# control-01: a member's output cut at the run's end, 2026-11-13

POLICY_READ: implementer, base 9efde72524dd626f3ff84e6b9891852d7c810a9b; `AGENTS.md`
f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41,
`docs/development-rules.md` df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3,
`docs/development-evidence.md` 61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63.
Owned: `crates/api/src/image/session.rs`'s `pump` and its tests, D240's line on it, this record,
control-01's status ([finding](../../../bugs/2026-11-13/control.md)).

## Confirmed

Both of the finding's schedules, driven through the session's own reader (`read_answers`) by a
simulated run host over pipes (the tests' `hosted`), before the member's pump starts:

- `output_taken_before_the_runs_end_reaches_its_owner_whole`: `OUTPUT(A)`, `OUTPUT(B)`,
  `OUTPUT_END`, `ENDED`. Red at d7b62bd7d: the owner read A alone (`left: "first record\n"`),
  as the pump's credit after A found the host's channel closed by `ENDED` and ended the pipe.
- `output_said_after_the_runs_end_reaches_its_owner_before_its_end`: `ENDED`, then `OUTPUT(C)`
  and the host's answers closed with no `OUTPUT_END`, as the run host's capture reader may send
  past the end its link thread says (`api_run.rs` `serve`: `following` beside `records`). Red at
  d7b62bd7d: the owner read nothing (`left: []`), the pump having taken the run's end for its
  channel's.

## Fix (cf2764c6e)

`pump` ends its pipe at the channel's end or the end of the host's answers (`State::answered`,
set once `read_answers` has taken its last frame), never at the run's end alone; a refused
credit ends its credits (`granting`), never its writes. Both ends are protocol events: no
literal, timer or learned value is added, and a cold run behaves the same. An owner's EOF of a
channel with no `OUTPUT_END` now waits for its host's answers to end (its process's exit, after
its cleanup) rather than the run's end: the only point at which no more of it can come.

## Runs

adas-mac-studio-3.local, macOS 26.2 arm64, rustc/cargo 1.98.1; carrier (bundle over
b8da5eca3, blobless clone under `~/g121-tmp/control01`), `CARGO_INCREMENTAL=0`, one cargo; kiln
run r1481, [`run.sh`](run.sh):

| Step | Source | Result |
|---|---|---|
| red | d7b62bd7d (tests, old pump) | 0 passed, 2 failed, as above |
| green | 17ef27f54 (fix merged with main b8da5eca3) | the 2 passed; `image::session` 29 passed |
| mutant 1: `s.end.is_some()` back in the pump's end | 17ef27f54 + mutation | `output_said_after…` failed (`left: []`) |
| mutant 2: `return` on a refused credit | 17ef27f54 + mutation | `output_taken_before…` failed (A alone) |
| clippy | `cargo clippy -p shards-api --all-targets -- -D warnings` | clean |

## Follow-up: the run host's held bytes at its capture's end (ROOT's slice, 2026-11-13)

At its VM's capture's end with no record's end, the run host (`crates/shards/src/api_run.rs`
`read_records`) dropped what it held of a channel past its orchestrator's room (`Flows::waiting`)
and said no end: the owner's pipe waited for the host's exit and ended short. Now `records` ends
cut every channel its orchestrator gave room to or was told of that has no end (`Live::cut`),
its held bytes first, as a record's own end sends them (`Live::end_channel`, shared with `End`
and `Aborted` records), then `OUTPUT_END Aborted(104)`, ECONNRESET's Linux number. Room is not
waited for: the session closes its host's stdin at the run's end, so no credit could come once
the VM is gone, and what is held is bounded by the guest's credit (at most one record's; from a
guest that keeps its credit, the 3 bytes by which `grant`'s least, an errno's record, passes a
room of less). The typed end reaches the session (`Flow::ended`); `ImageGuest` gives its caller
the pipe's EOF alone, so the owner sees the cut as the end of the bytes, not as its type.

kiln run r1511, adas-mac-studio-3.local, macOS arm64, [`run-host.sh`](run-host.sh)
(`SHARDS_HELPERS=skip`, empty `SHARDS_INIT_BINARY`/`SHARDS_SERVER_BINARY`: no guest is built
or run by these unit tests):

| Step | Source | Result |
|---|---|---|
| red | 53faf0804 (test, old host) | `bytes_held_past_the_room_reach_the_orchestrator_as_the_capture_is_cut` failed: the owner had `01234` alone |
| green | 347d16312 | it passed; `api_run::hosted::tests` 7 passed |
| mutant 1: `cut` returns at once | 347d16312 + mutation | failed (`01234` alone) |
| mutant 2: `cut` drops what is held before the end | 347d16312 + mutation | failed (`01234` alone) |
| clippy | `cargo clippy -p shards --all-targets -- -D warnings` | clean |

## Owed (ROOT)

Real-VM: `cargo test -p shards --profile test-release --test managed_output` and the api_image
lanes (`cargo test -p shards --profile test-release --test api_image`), on HVF, KVM and WHP.
The run host's own hold of bytes past its owner's room when its VM's capture ends without a
record's end (`read_records` returns at EOF with `waiting` unsent) is D240's guest-abort
allowance and is unchanged here.

| Rules | Account |
|---|---|
| R01 | No new fallible step; a refused credit is handled, no panic path added; clippy clean. |
| R02/R05/R12 | One existing seam (`pump`), no new interface, cache or wait: the condvar wait it had, its end condition corrected to the authoritative events. |
| R03 | Session: no literal, both ends are the wire's own events. Host: `CUT` = 104, ECONNRESET's number in Linux's include/uapi/asm-generic/errno.h, the guest's errno space init's aborts use; no timer or size. |
| R04 | The pump's two latches (owner's pipe open, credits granting) are documented, set once; the end is the channel's typed `OUTPUT_END` or the answers' end. |
| R06/R07/R13 | Two lines of behaviour changed; the doc comment says why the run's end is no channel end. |
| R08/R14 | No performance claim; EOF of an unended channel now waits for the host's exit, unmeasured, ROOT's. |
| R09/R10 | Red on the old code for the finding's reason, green after, each guard's mutation red. |
| R11 | Shared code on every OS (std pipes); run on macOS arm64 only; other targets are CI's. |
| R15/R16 | Implementer; light tests on one runner; its target removed after the runs. |
