# An orchestrator's run: its room told, its microVM's cost in its stats (D241)

Implementer for ROOT (coordinator, kiln agent 01a148884a4770fdaf0111f3f792a248), 2026-11-12, kiln plan
01a1cd8e862e7169a074db3d5fa33f3d step 14 and ROOT's two routed findings: shards-api had no call that tells an image
run (D231) its room, so Hex refused it typed (`Awaits(GuestRoom)`, `Awaits(RingPinsWithinRoom)`), and an image run's
stats carried no guest time per vCPU (`Awaits(GuestCpuTimePerVcpu)`). POLICY_READ: origin/main e82b9403b,
docs/development-rules.md sha256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3, AGENTS.md
f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41, docs/development-evidence.md
61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63; role implementer. Branch p/hex-s13-s14-v1 from
origin/main 2365d7844, origin/main e82b9403b merged in (30d6828dd).

## What was built

- `ImageGuest::room(room, patience)` (shards-api): the room the orchestrator's budget leaves the run, told its init's
  `builtin::ROOM` by `shards api-run` as the daemon tells its own runs (D188), answered `HELD` with what init holds the
  run within past its use, typed refusals (`ImageRefusal::Room`). init's ring pins (D230) come out of that room
  (crates/init/src/ring.rs: "pins come out of the room init is told").
- `Stats::microvm` on the run's stats (`StatsScope::Run`): each vCPU's guest time by index and what departed vCPUs ran
  (`GUEST_TIME`, asked of the VM on api-run's link as the daemon asks it), then the VM process's CPU time, resident
  and own memory (`platform::process_usage`), read after it so the process's CPU less the guest's is never negative
  past clock steps.
- The image wire at version 7: `ROOM` 10, `HELD` 27, the stats answer's microVM after its lines; vCPUs out of index
  order refused (`WireError::Order`); a vCPU count held to the bytes that follow it before anything is allocated.

## Light tests (R15: the implementer's scope)

Command, from the repository root, with `CARGO_TARGET_DIR` of the run's own and `SHARDS_HELPERS=skip`:

```
cargo test -p shards-api --lib image
cargo test -p shards-api --test allocations
cargo test -p shards --bin shards api_run
python3 -I docs/research/measurements/orchestrator-room-v1/mutants.py
python3 -I scripts/check-development-policy
```

| Host | Revision | shards-api image | allocations | api_run | mutants caught | policy | lint |
|---|---|---|---|---|---|---|---|
| adacachy, Linux x86_64 (glibc), rustc 1.98.1 | c96b136a6 | 22/22 | 1/1 | 11/11 | 6/6 | pass | not run |
| adacachy2, Linux x86_64 (glibc), rustc 1.98.1 | 388bfd4fc (origin/main 1145bd3f2 merged in) | 22/22 | 1/1 | 11/11 | 6/6 | pass | not run |

One host, as the person asked on 2026-11-13 ("run their focused unit and integration tests then MERGE TO MAIN"):
macOS, Windows and the 8-target lint are CI's on main, which ROOT watches. Runs begun on other hosts before that
were stopped there: adalundhe-macbook-pro (macOS arm64, M5 Max) at 388bfd4fc passed shards-api image 22/22 and
allocations 1/1, its api_run unbuilt (the in-VM server's C needs zig, which that host lacks); jupiter (Windows
x86_64) passed image 21/21 (one test is Unix's alone) and allocations 1/1 before it was stopped.

New tests: `image::wire`'s `rooms_and_their_answers_read_back_as_said` and
`a_runs_microvm_reads_back_as_its_host_said_it`, and its version 6 refused in `unknown_versions_and_tags_are_refused`;
`image::session`'s `a_rooms_answer_is_kept_for_its_awaited_ask_alone` and
`a_room_no_host_hears_is_refused_and_leaves_no_ask`; `tests/allocations.rs`'s
`a_hostile_vcpu_count_allocates_nothing_its_bytes_cannot_fill`; `api_run`'s `a_rooms_answer_reads_as_it_was_asked`.

### Guard-removal mutants (R10)

`mutants.py` takes each guard out, runs the test that holds it, and checks it fails for that guard's reason:

| Mutant | Guard taken out | Test that fails, and what it says |
|---|---|---|
| `vcpu-order` | vCPUs out of index order refused | `a_runs_microvm_reads_back_as_its_host_said_it`: `Order` expected, read as a cost |
| `hostile-count` | a vCPU count held to the bytes after it | `a_hostile_vcpu_count_allocates_nothing_its_bytes_cannot_fill`: asks for 160 GiB for an answer of 55 bytes |
| `version-7` | the channel's version 7 | `unknown_versions_and_tags_are_refused`: version 6 read |
| `answer-for-no-ask` | an answer kept only for an awaited ask | `a_rooms_answer_is_kept_for_its_awaited_ask_alone` |
| `ask-left-behind` | an ask removed when answered or given up | `a_room_no_host_hears_is_refused_and_leaves_no_ask` |
| `held-without-budget` | a hold said with no budget refused | `a_rooms_answer_reads_as_it_was_asked` |

`hostile-count` survived its first run (adacachy, 799470ce5): with the bound taken away, Linux granted
`Vec::with_capacity(u32::MAX)` of 40-byte vCPUs lazily, and the answer was still refused as short, so the refusal
alone showed nothing of the guard. `tests/allocations.rs` now counts the largest allocation the decode asks for, on
its own thread, as `crates/abi/tests/allocations.rs` counts a spec's, and the mutant fails it (c96b136a6). The
older `image::wire` test `a_hostile_count_is_short` (a request's members, a port's receivers) asserts only the
refusal, so it holds its guards the same weak way.

## Real VMs

`crates/shards/tests/api_image.rs`: `the_orchestrators_room_holds_its_run_within_it` (a room of 64 MiB held
exactly, none left held within the least room and short of it, no budget answered none) and
`stats_read_the_run_and_a_member` (the run's microVM: its VM process's use, its one vCPU's guest time; a member's,
none). VM_RESULTS

## R01 to R16

- R01: no unwrap, expect or indexing in production; the wire's reads are checked, a vCPU count is held to the bytes
  after it before its vector is allocated (pinned by `tests/allocations.rs`), `GuestTimes::total_ns` saturates (a
  total said, never wrapped); a room's and the stats' threads come from `thread::Builder`, and a failed spawn is
  answered.
- R02: no new mechanism. The room is D188's, through init's own `builtin::ROOM` as the daemon sends it; the stats'
  microVM is the daemon's (D43, D170's `microvm`, guest-cpu-time-v1). The alternative, a hold from outside alone
  (Hex's `memory.high` on the VMM), is what PM M309 measured: its guest read its pages again for minutes.
- R03: no new literal. The kinds are the next free numbers, the version the next, a vCPU's least bytes (13) its
  fields' widths.
- R04: typed refusals (`ImageRefusal::Room`, `WireError::Order`); an ask is awaited from before it is sent until it
  is answered or given up (`State::rooms`), and an answer for no awaited ask is dropped.
- R05: no new layer. One matcher answers the VM's device steps and guest times by number; a room waits on its answer,
  the run's end or its host's exit, never a poll; no cache.
- R06, R07, R13: one function per step; comments give derivations and the D-entries they follow.
- R08, R14: no performance claim; a room's and a stats read's cost are ROOT's to measure.
- R09: no defect fixed. The weak hostile-count test was shown live: the mutant survived on adacachy at 799470ce5.
- R10: red and green and the mutants above; this record.
- R11, R12: one implementation on every OS (`platform::process_usage`, init's `builtin::ROOM`, the VM's
  `GUEST_TIME`); every target linted; the light tests on Linux x86_64 and macOS arm64. Owed: Windows' light tests
  (no Windows host with room: jupiter's headroom guard holds kiln runs to thousandths of a core), and Linux arm64's
  (no native host; its lint is clean).
- R15: implementer; light tests, plus one run of each of the two real-VM tests above where noted.
- R16: each host's target under `~/g121-tmp/hex-s13-s14`, removed after.

## Owed (ROOT)

- The real-VM tests on KVM (x86_64, and arm64 through L1), HVF and WHP (VM_OWED).
- Hex's adoption: its re-vendor of Shards at this revision, its learner telling each Shards guest its budget's room
  each window, and its usage reading each vCPU's guest time from the run's stats (kiln plan step 14, Hex's half).
- D188's give-back at the host's pressure (`RECLAIM`) for an orchestrator's runs: no run host watches its host's
  pressure; an orchestrator gives one back by telling it a room short of what it holds.
