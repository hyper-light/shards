# ring-pins-v1: Review (plan step 2)

The review ROOT's plan step 2 asked of this lane (goal 52, kiln 42b9409a4d468a0d93b040a6dc7b3320; its plan
01a1d2c5867975b7ba71815aa3a9cb9e, step 5; research findings f2, f3, f4, f11, f12): every commit of
ring-pins-v1, its fix3 and its E2E round read whole against R01-R16 and AGENTS.md's review rules, each
defect fixed on main with a red/green test and a guard-removal mutation or recorded owed with its evidence.
The lane's own record is [README.md](README.md); D230 and section 16 of
[engine-ring-channel-decision-v1](../../engine-ring-channel-decision-v1.md) hold the decision.

## Identity

- **Revision reviewed:** origin/main 672b6fb47b2194f3ac27a3a40a90896587f640d1. None of the ring's files
  (crates/{abi,init}/src/ring.rs, init's engine.rs, the vsock ring.rs and conn/ringed.rs, engine_ring.rs,
  engine_bulk.rs) changed on main after the lane's last commit (a7dbf1446): the code read is the lane's.
- **Policy (POLICY_READ, sent to the goal agent and ROOT before any edit):** AGENTS.md
  f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41, docs/development-rules.md
  df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3, docs/development-evidence.md
  61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63, CLAUDE.md
  588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010. Role: implementer (goal 52's child;
  goal agent a88ba9336b2a5c118, ROOT 01a148884a4770fdaf0111f3f792a248).
- **Owned paths:** crates/abi/src/ring.rs; crates/init/src/{ring,engine,plug,run,domains}.rs (D230's
  hunks); crates/vmm/src/devices/virtio/vsock/{ring.rs,conn/ringed.rs}; crates/testguest/src/{workload,
  engine_bulk}.rs (the ring's hunks); crates/shards/tests/engine_ring.rs and the ring's helpers in
  tests/common; this record; D230's pointer to it.
- **Host:** one Mac runner, adas-mac-studio-3.local (macOS 26.2 25C56, Apple M4 Max, 14 CPUs, 36 GiB),
  through a carrier of this step's (a blobless clone at the revision, a patch, a target directory of its
  own); rustc 1.98.1. No Linux or Windows host, no VM, no E2E, no CI, no scripts/lint (the person's rules of
  2026-11-13, the goal's addendum); nothing built on adas-macbook-pro-2.

## Commits reviewed

| Commit | What it does | Review |
|---|---|---|
| 946eaa687 | Version 2: a slot a stream, made within the run's room (D188), grown by `SWAP`, freed after the device's `DETACH`; the relay in `run/engine`; `ROOM`/`RECLAIM` through a shared page and an eventfd; the device's `Detached` state | F2 and F3 (the relay); O1, O2 |
| 02350b8d3 | The pin and unpin calls on a real kernel (`VmPin`); the stand-in answers a swap crossing its `DETACH`; the record | No defect |
| 18a1dc2b6 | The relay tests' writer leaves a refused write be; an unsafe block's comment | No defect |
| 43114652a | r4 and r5 gate records | Records only |
| 3480f9299 | fix3: the device says it waits for the relay's first bytes as it attaches (red r1, green and guard-removal r2) | Correct; its evidence complete |
| 026dbc846 | The E2E's first failures: the test guest's create and its 204, the held connection, the pins counted as FOLL_PIN pages | No product defect; F4 |
| 33cc69d1e | Unix-only helpers gated for Windows' lint | Superseded by 037c69cc8; no defect |
| ea3482851 | A Windows test's unsafe block documented (cli, not D230's) | No defect |
| a1fc36a84 | The vsock ring tests' socket pairs hold twice a ring (macOS's 8 KiB) | No defect |
| 0ba8fca9f | The E2E round's records (r13-r15) | Records only |
| 037c69cc8 | Give-back and shrinking-room on every host; `Told` moved to tests/common | No defect |
| f674182bb | The shrinking-room test's SKIP said by the test | No defect |
| bdd0e9583 | A root host's budget: a cgroup of root's own hierarchy (D183 slice 2) | O6 |
| a7dbf1446 | r16, D230's text | Records only |

Read with them, as each relies on it: `Ring::attach` (F1, its loop D230 step 2's, 51e6429a1, and fix3's
target), the host's hello reader (`workload.rs`, `warm.rs` `serve_engine`: both parse an offer before it
reaches the device), `plug::{cap, hold_within, working_room}`, the vsock connection's `next_rx` order.

## Findings

### F1. `Ring::attach` never ended on an offer longer than its slot (fixed, b81d707a9)

- **Where:** crates/vmm/src/devices/virtio/vsock/ring.rs:208-243 at 672b6fb47. Each extent was cut at the
  control page's and the rings' boundaries, and the total checked against the slot only after the loop. An
  extent running past `CONTROL + 2 * ring` met no region: `piece_end` equalled `piece`, and the loop pushed
  empty runs forever, the device's worker filling its memory.
- **Rules:** R01 (an implicit failure path behind a library precondition: allocation without bound), the
  device's own contract ("The guest is hostile": it checks what a guest named).
- **Reach:** none from a guest today: `Offer::parse` refuses such an offer, and both callers parse first (the
  engine port's owner, warm.rs `serve_engine` through workload.rs; a swap's offer, ringed.rs `offer_bytes`).
- **Fix:** attach refuses (`Refusal::Malformed`) an extent ending past the slot before cutting it.
- **Evidence ([logs/r17](logs/r17-review-f1-attach.txt)):** `an_offer_longer_than_its_slot_is_refused`
  (an extent a page longer than its slot; a slot's pages and one more) never ended on main, killed by
  [review-watchdog.sh](review-watchdog.sh) at 284,608 kB resident (red); never ended with the guard
  (`.filter(|&end| end <= slot)`) removed, 269,024 kB (mutation); refused at once with it (green).

### F2. A swap answered past belief frees the slot the device attached (owed: Linux)

- **Where:** crates/init/src/ring.rs:1475-1479 at 672b6fb47. In `swap`, the new slot attached
  (`Reply::Attached`), the device's last positions of the old slot past belief (`finals` none), the relay
  frees the old slot and returns `Swapped::Done`; `run` (:1268) answers `Done` with `Disposal::Release`, and
  `serve` (:1239-1245) frees `self.slot`, which is now the new slot: attached, its device's `DETACH` never
  asked or heard. Its pages go back to the guest's allocator while the device may still write them.
- **Rules:** D230's guard ("The relay frees a slot only after the device's `DETACH`"), the relay's own
  contract (ring.rs:33-35: "a host that writes nonsense spoils only the connection's own stream"), R04
  (`Swapped::Done` names two outcomes, the new slot lost and the new slot attached).
- **Reach:** a device that publishes impossible final positions; a correct one never does. So it is the
  guard's failure on an error path, not on the normal path.
- **Fix, owed its run:** [review-f2.patch](review-f2.patch): `Swapped::Broken` for that case, ended as
  `Turned::Ended` is, through one `end()`: the client shut, the device asked to let go, the new slot freed on
  its `DETACH` and kept pinned where none comes. Its test,
  `a_swap_past_belief_frees_its_new_slot_only_after_the_devices_detach`, has the stand-in put the old
  slot's tail past all the relay put, answer the swap, and look after 50 ms whether the pool still holds
  the new slot (as `the_relay_frees_a_slot_only_after_the_devices_detach` does), not touching it where it
  does not. [review-f2-red.patch](review-f2-red.patch) is the tests alone. All three (red, fix, mutation)
  apply to 672b6fb47 and pass fmt and clippy -D warnings for both musl guests
  ([logs/r18](logs/r18-review-f2-lint.txt)). Not run: the relay is the Linux guest's alone, and this step's
  one host is a Mac (R09: no fix without its red, so it is not on main).

### F3. A stream's room read and its reservation are two critical sections (owed: Linux)

- **Where:** crates/init/src/ring.rs:1035 (`hello_in` reads the supply under the pool's lock), 1452-1455
  (`swap`, the same), then 713-720 (`take` reserves under the lock again). Once the relay is in the run's
  cgroup (`Joined::In`), `outside()` is 0 (:427-433) and a slot made counts only through `memory.current`. A
  slot another stream made between the two (reserved, populated and counted `made`) is in neither the stale
  `held` nor `making`: the reservation admits a slot past the room, one for each slot made in between. With
  2V streams starting at once (the E2E's), each takes the lock twice, which widens the window.
- **Rules:** D188 and the step's own question ("pins come out of D188's room"), R03 (the room is "read as
  each slot is made", decision section 16), R05 (state read and acted on apart).
- **Evidence:** seen in source, not reproduced (R09): the code is the Linux guest's alone. The repro for
  ROOT: a unit test of a pool `Joined::In` whose test supply's `held` is `base + pinned` (as the run's
  `memory.current` counts its slots), room for one slot past its working room:
  `let stale = lock(pool).supply(); let a = take(pool, &lock(pool).supply(), None); let b = take(pool,
  &stale, None);` makes `b`, a slot past the room (red on main).
- **Fix proposed:** read the supply inside `take`'s critical section (and `swap`'s room generation before
  it, so that a refusal is retried once room may have come back): no slot can be made between a room's
  read and its reservation, and no stale room can be passed. Its green is the red sequence no longer
  expressible, with a stress test of N streams at once within a room of K slots never pinning past K.

### F4. `settled` waits on a raw deadline (owed: with the E2E)

- **Where:** crates/shards/tests/engine_ring.rs:292-305: the pinned-pages wait fails at `TIMEOUT` (600 s)
  whatever the microVM does, where every other wait of the file (`answering`, `await_rings`, `Budget::held`)
  is `common::Patience`'s: past its timeout while the daemon or a microVM progresses (a slow host, as WHP
  arm64 under emulation), a minute after the last progress where stuck (a slot never freed, the failure
  this wait exists to catch).
- **Rule:** R05 (waits), the tests' own convention.
- **Change owed:** `let mut patience = Patience::of(home, TIMEOUT);` and `assert!(patience.holds(), ...)` in
  its loop. Owed with the E2E it changes, which only a real VM shows (no fix blind).

### Observations, not changed

- **O1 (R04).** The device's `Ringed` keeps five booleans (`replied`, `doorbell`, `blocked`, `full`, `ended`)
  and `Detached` two (`reply`, `said`) whose (true, true) is never reached: a three-step sequence in two
  flags. Each is documented, and the lane recorded the `Ringed` gap; no wrong transition found.
- **O2 (D188 accounting).** A told room's debt (`owed`) is paid only by idle slots let go for it
  (ring.rs:611-628), not by slots freed as their streams end; the next `ROOM`, each pressure window,
  recomputes it from what the run then holds (:632-636). At most one window's idle slots go plain early;
  nothing is lost.
- **O3 (f11, D181's).** On Windows the daemon holds a microVM's own engine to 13 connections at once
  (`share(27, 0)`, daemon/engine.rs) and its clients to 27 (`clients_for(POSIX_THREADS)`, daemon.rs), as
  `thread_limit()` says none there: the pins test past 13 streams is refused on WHP x86_64 by D181's bound,
  not D230's. Recorded, not taken on.
- **O4 (R03).** `CUE` (0x00, the owner's cue to its own device after an offer, host-side) was in no literal
  table: D230's protocol row now names it.
- **O5 (test parameters).** The E2E's sizes (64 MiB a bulk arm, 4 MiB a stream, 50 and 200 pings), its 50 ms
  looks and `TIMEOUT` (600 s, under `Patience` but for F4) are a test's; `vcpus()` falls back to 4 vCPUs
  where the host's count is unread.
- **O6 (bdd0e9583).** On a root host with no user's manager the shrinking-room test enables the memory
  controller in the root cgroup's `cgroup.subtree_control` and leaves it so (its `Drop` removes only its own
  cgroup); on the L1 guest, its own hierarchy.

## Every ring literal (R03)

The step's questions: every size, pin count and wait is a protocol constant, a cited bound, or learned per
stream; pins come out of D188's room (F3 aside); no CPU cap remains (no read of the CPUs in the ring's code
but `plug::working_room`'s, of those online as read); one, two and many vCPUs read their working room as
applied. No wait in the ring's production code has a timeout: each is a poll(2) on a socket or an eventfd.

| Literal | Where | Source or derivation |
|---|---|---|
| `VERSION` 2, `MAGIC` "RING", `SPEC` "engine-ring=2", `REPLY_MAGIC` "RNG", status bytes 0-4 | abi ring.rs:24-30, 275-306 | Protocol constants (D230) |
| `DOORBELL` 0x01, `END` 0x02, `DETACH` 0x03, `SWAP` 0x04 | abi ring.rs:86-106 | Protocol constants; any other byte a broken rule |
| `OFFER_HEADER` 16, `EXTENT` 16, `REPLY` 8 (`REPLY_LEN`) | abi ring.rs:110-114; ringed.rs:121 | Protocol framing |
| `CUE` 0x00 | vsock ring.rs:45 | Host-side protocol constant: the device takes the first byte after its owner's offer as the cue and checks it is `CUE` (O4) |
| `PAGE` 4096, `CONTROL` = `PAGE`, `MIN_RING` = `PAGE` | abi ring.rs:34-36, 69 | The smallest page of every supported guest (x86_64's; arm64's 4 KiB granule, the pinned kernel's) |
| `STRIDE` 128, the fields at its multiples | abi ring.rs:40-61 | Apple silicon's 128-byte line (`hw.cachelinesize`), x86's paired-line prefetch |
| `MOST_IOVECS` 1024 (`MOST_SPANS`) | abi ring.rs:66; vsock ring.rs:85 | Linux `UIO_MAXIOV`, macOS `IOV_MAX`; Windows' WSABUF arrays set no lower bound |
| `MAX_RING` 2 MiB | abi ring.rs:75-83 | Derived: the largest power of two each turn of which (S/4096 + 1 pages) one call reaches; tested |
| A stream's ring | init ring.rs:1081-1157 | Learned per stream (doubling, Bentley and Yao); cold start the last stream's need, a relay's first `MIN_RING` |
| Room for pins; a told room's debt | init ring.rs:251-299, 242-249 | Read as applied: `plug::cap`, `memory.current`, `MemAvailable`, `plug::working_room` (D188, PM M309/M321) |
| `MOST_BUFFERS` 16,384; `STREAM_FDS` 3, `STREAM_SLOTS` 2 | init ring.rs:54-59, 647-668 | `IORING_MAX_REG_BUFFERS` (v6.18 io_uring/rsrc.c); a stream's client, vsock and wake descriptors, its slot and its next while it swaps; the table of the relay's `RLIMIT_NOFILE` as read |
| `IORING_REGISTER_BUFFERS2` 15, `..._UPDATE` 16, `IORING_RSRC_REGISTER_SPARSE` 1; `io_uring_params` 120 bytes | init ring.rs:49-51, 87 | Linux uapi (include/uapi/linux/io_uring.h) |
| pagemap bits 0-54 and 63 | init ring.rs:989-990 | Documentation/admin-guide/mm/pagemap.rst |
| `MOST_SAID` 3 | init ring.rs:1596 | What the device says between two of a stream's reads; more is read on, so it bounds no behaviour |
| A turn's quantum, a ring's length | ringed.rs:319, 371 | Derived: one connection's turn moves at most its ring, as a packet's 64 KiB at most did |
| Names `shards-engine-ring`, `engine` | init ring.rs:769, 675 | Identifiers |

## R01-R16

| Rule | This review |
|---|---|
| R01 | F1 fixed (an unbounded loop on a malformed offer). The relay's and device's production paths read by hand for panics: none past the lints (positions checked before subtraction, `get`, saturating sums); clippy -D warnings clean (logs/r17). |
| R02 | The lane's alternatives (section 16) hold; no new decision made here. |
| R03 | Every literal above, each sourced; F3 is where the room's reading falls short of "read as each slot is made". |
| R04 | F2 (an outcome named for two states); O1 (documented booleans). Refusals stay typed (`Refusal`, `Malformed`, `Unmade`). |
| R05 | No polling in the ring's production code; F3 (state read and acted on apart); F4 (a test's wait). No cache. |
| R06 | The relay's `run` and `swap` gain a shared `end()` in F2's patch; the pump stays one loop. |
| R07 | F1's and F2's comments say why; nothing narrated. |
| R08 | No performance claim; the measurements stay owed (README.md). |
| R09 | F1 reproduced live before its fix (r17). F2 and F3 are the Linux guest's: their reds are ROOT's, written out above; neither is on main unrun. |
| R10 | F1: red, green and guard-removal mutation, the harness committed; F2: tests and patches committed for ROOT's red, green and mutation. |
| R11 | macOS arm64 unit tests and lints, both musl guests' lints (r17, r18). Unverified: every real VM; Linux and Windows unit tests; Windows lints; WHP arm64 (no host). Owed below. |
| R12 | F1 is the one device's, every backend's; no platform copy. |
| R13 | F1 one filter; F2's patch shares one `end()`. |
| R14 | None run (ROOT's). |
| R15 | Implementer's light tests on one Mac; the fix landed fast-forward on the person's rule of 2026-11-13; ROOT owns the rest. |
| R16 | The carrier's clone and target directory on adas-mac-studio-3 removed at the end; nothing else touched. |

## What ran where

| Log | Host | What |
|---|---|---|
| [r17](logs/r17-review-f1-attach.txt) | adas-mac-studio-3, macOS 26.2 arm64 | F1 red, mutation, green; fmt; shards-abi ring 9 of 9; the vsock device's 67 of 67; clippy -D warnings of abi and vmm (aarch64-apple-darwin) and of init and abi (aarch64 and x86_64 musl) |
| [r18](logs/r18-review-f2-lint.txt) | the same | F2's red, fix and mutation patches: fmt and clippy -D warnings, both musl guests |
| [r19](logs/r19-review-merged-gates.txt) | the same | r17's gates on the merged tree (origin/main 94be8818b and this step's commits): all pass; fmt's only diffs main's own (guest_time.rs, hv/kvm/mod.rs, from 2fb8e9ba3), none of this lane's |

## Owed to ROOT

At the landed revision, from a checkout of it:

1. **engine_ring.rs on real VMs** (`a_microvms_engine_carries_bulk_whole_both_ways`,
   `an_idle_microvm_pins_nothing_and_each_streams_pins_go_as_it_ends`, `a_give_back_lets_an_idle_slot_go`,
   `a_shrinking_room_lets_an_idle_slot_go_and_a_stream_past_it_goes_plain`):
   - HVF arm64 (an M-series Mac):
     `docs/research/measurements/macos-sched/launchd-run.sh /tmp/ring-hvf env SHARDS_REQUIRE_VMS=1 cargo test --locked --profile test-release -p shards --test engine_ring -- --test-threads 1 --nocapture`
   - KVM x86_64 (Linux with a user's systemd manager; TMPDIR on disk):
     `SHARDS_REQUIRE_VMS=1 cargo test --locked --profile test-release -p shards --test engine_ring -- --test-threads 1 --nocapture`
   - KVM arm64 through L1 (an M3 or later Mac):
     `docs/research/measurements/macos-sched/launchd-run.sh /tmp/ring-l1 env SHARDS_L1_TESTS=engine_ring:--nocapture SHARDS_L1_CONSOLE=/tmp/ring-l1-console.txt cargo test --locked --profile test-release -p shards --test boot -- snapshot_tests_pass_on_a_kvm_host_of_this_architecture --nocapture`
   - WHP x86_64 (jupiter, NASM and zig on PATH):
     `SHARDS_REQUIRE_VMS=1 cargo test --locked --profile test-release -p shards --test engine_ring -- --test-threads 1 --nocapture`;
     the pins test past 32 streams waits on D181's Windows bound (O3), and the bulk test's microVM stop
     (r16) on its root cause.
   - WHP arm64: unverified, no host.
2. **F2's red, green and mutation** (any Linux host):
   `git apply docs/research/measurements/ring-pins-v1/review-f2-red.patch && cargo test --locked -p shards-init a_swap_past_belief` (red:
   "the relay let go of the attached slot before the device's DETACH"); then `git checkout crates/init/src/ring.rs && git apply docs/research/measurements/ring-pins-v1/review-f2.patch && cargo test --locked -p shards-init ring::` (green, with the relay's other tests);
   then `sed -i 's/Swapped::Broken => return self.end(),/Swapped::Broken => return (Offered::Done, Disposal::Release { given_back: false }),/' crates/init/src/ring.rs && cargo test --locked -p shards-init a_swap_past_belief` (red); land the fix where green.
3. **F3's red and fix** (Linux): cured, red, green and mutation run (6557c8597, "Cures, F3 and F4"
   below).
4. **F4's change**: made (a2c887eeb, "Cures, F3 and F4" below); its run is item 1's pins test, and its
   red is owed with it (below).
5. **Light gates this step could not run:** `cargo test --locked -p shards-init ring::` and
   `cargo test --locked -p shards-vmm --lib -- devices::virtio::vsock` on Linux x86_64 and arm64 (musl or glibc);
   the vsock device's tests and clippy on Windows x86_64 (no Windows target or cargo-xwin on the Mac
   runner); `scripts/lint` of the seven targets for b81d707a9 (it compiles for each).
6. **The lane's own owed items** (README.md): the engine suite (D181), isolation's token test, the budget
   and reclaim suites on real VMs; the idle-pins red on 032b63b1c; the E2E mutations (`told` removed from
   `ROOM`, the leaf join removed); the measurements.

## Cures, F3 and F4

ROOT's word (2026-11-13): "one critical section for read-and-reserve, with a concurrent-starts test
red first, and the settle following the ring's progress, Part B's shape." F2 is ROOT's (a6edfb8e9)
and is untouched here.

- **Identity:** goal 52's child for F3 and F4, role implementer (goal agent a88ba9336b2a5c118, ROOT
  "main"). Base: origin/main 15215d729609fa1200f3028c3af476b71351f0c5. Policy as above (AGENTS.md
  f66d304c…, development-rules.md df2acf47…, development-evidence.md 61613a24…, CLAUDE.md 588b0020…),
  POLICY_READ sent to both before any edit. Owned: crates/init/src/ring.rs and the admission's callers
  (D230's hunks), crates/shards/tests/engine_ring.rs, this section, and D230's text.
- **Hosts:** adacachy2 (Linux 7.3.3-1-cachyos x86_64, 32 CPUs) for shards-init's tests and lints;
  adas-mac-studio-3 (macOS 26.2 arm64, M4 Max) for engine_ring's lint. Each ran through a carrier of this
  child's: a blobless clone at the revision, a patch, `CARGO_TARGET_DIR` under ~/g121-tmp/s2-ring-f3,
  `CARGO_INCREMENTAL=0`, `nice -n 10`, rustc 1.98.1. No E2E, VM, CI or scripts/lint ran, and nothing
  was built on adas-macbook-pro-2.

### F3, cured (6557c8597)

- **Reproduced first (R09; [r20](logs/r20-cure-f3-at-once.txt)).**
  `streams_started_at_once_pin_no_more_than_their_room`, with the test alone
  ([cure-f3-red.patch](cure-f3-red.patch) on 15215d729), failed: "round 1: 73728 bytes pinned at once,
  past the room's 61439". That is six slots of `MIN_RING` held at once against a room for four, with
  the relay in the run's cgroup. The relay's other 21 tests passed.
- **The cure.**
  - `Pool::reserve` reads the room itself (`self.supply()`), under the same hold of the pool's lock as
    the reservation it makes, and picks a new stream's start ring from that read.
  - `take` and its two callers read no room any more: `hello_in`'s `lock(pool).supply()` and `swap`'s
    `(p.supply(), p.room_gen)` are gone.
  - The pool is the room's one owner, and a reservation needs its lock (`&mut Pool`), so no caller can
    read a room and reserve against it apart.
  - `swap` reads the room's generation before the reservation's critical section. A refusal is tried
    again once the generation moves (a slot freed, a word of the run's room), so no move after the
    read is missed. A move between the two costs at most one try more.
- **The test.**
  - 64 streams are released at once into their hellos (`Hold`, a barrier with the stand-ins' 20 s
    deadline) against a room for four slots, and less than a fifth.
  - Each holds its slot, or goes plain, until every stream has started and the test has read the
    pool's pins. The pins are never past the room, every stream's bytes arrive whole, and every slot is
    freed.
  - It runs eight rounds with a budget and the relay in the run's cgroup, and eight with no budget.
  - The test pool's room is read as its kernel reads it (the cfg(test) arm of `Pool::supply`): the
    slots made count among what the run's cgroup holds once the relay is in it (`memory.current`), and
    out of what the guest has available (`MemAvailable`). Before, a test pool counted made slots only
    through `outside()`, which is 0 once the relay is in, so it could not show F3.
- **Its sizes (R03: a test's; r20, RATES).**
  - On main's two critical sections, 16 rounds of each of five sizes pinned past the room in 25 to 94%
    of them ([cure-f3-rates.patch](cure-f3-rates.patch), a diagnostic test, not committed as one).
  - 64 streams against four slots did so in 8 and in 6 rounds of 16 (with a budget, with none).
  - At that rate, about one round in two on that host, the test's 16 rounds miss a split section about
    once in 10^4.
  - The cure holds in every round on any host, because a reservation and the room it is judged by are
    one state under one lock.
- **Green and mutation (r20).**
  - With the cure, `cargo test -p shards-init ring::` passed 22 of 22. The new test's 16 rounds took
    7.4 s of that run.
  - With the section split back in two at its owner ([cure-f3-split.patch](cure-f3-split.patch):
    `reserve` handed a room read under a hold of the lock of its own), it failed again: "round 0: 98304
    bytes pinned at once, past the room's 61439" (eight slots).
- **Lints.** clippy -D warnings `--all-targets` of shards-init was clean for x86_64-unknown-linux-gnu
  and for x86_64- and aarch64-unknown-linux-musl (r20). `cargo fmt -p shards-init -p shards -- --check`
  was clean.
- **Its cost (R05, R14).**
  - The room is now read with the pool's lock held: `plug::cap` (an atomic load), `plug::working_room`
    (sysconf), and one kernel file (`memory.current` with a budget, `/proc/meminfo` with none).
  - None of these takes a lock, so no lock order is added. Each stream start or growth reads the room
    once, as before.
  - How much longer the lock is held is not measured (R14, ROOT's).

### F4, cured (a2c887eeb)

- **The cure.** `settled` follows the ring's progress, in D215's shape.
  - *Progress:* the guest's pinned pages fall to a new low as a slot's pins are released. The relay
    frees a slot once it has heard its device let go and has passed on what the slot held.
  - *Window:* as long as the ring took at the work whose slots the settle waits for. That runs from the
    work's start (the setup's emitter, or the streams) to the settle's start, which follows the
    device's word that it let go of every ring.
  - *Judged:* starting one window after its first look, and once each window after that. A window over
    which the pins fell no lower is refused, typed, as `Stalled`, which carries the pins now and
    wanted, how long it waited, the time looked over, and the window. A count that reaches what the
    microVM pinned idle settles the wait exactly, as before.
  - *Cold start:* none. Each settle follows work the ring did, timed as it ran, and its first look is
    its baseline.
  - *Its looks* are execs into the microVM, each paced by its answer. The 50 ms sleep is gone.
- **Not chosen.**
  - `Patience::of(home, TIMEOUT)` (this review's own proposal): its 600 s and `DAEMON_QUIET`'s minute
    are numbers of ours.
  - Part B's own W*, the slowest settle this process has seen, with a cold settle following liveness
    alone: the test's first settle would then wait without end on a slot never freed, the very
    failure this wait exists to catch.
- **Its limits.**
  - A window is the whole work's time, so a stall is refused no sooner than one window after its last
    fall: seconds on a fast host, longer on a slow one.
  - A relay that frees one slot each window but never all of them is never refused, as a guest that
    spins is never stalled in D215.
- **Evidence ([r21](logs/r21-cure-f4-settle-lint.txt)).** It compiled, and `cargo clippy -p shards
  --test engine_ring -- -D warnings` was clean on adas-mac-studio-3; fmt was clean. Its run is a real
  VM's, owed below. No red is claimed here.

### R01-R16 for the cures

| Rule | F3 and F4 |
|---|---|
| R01 | No new panic path in production: `reserve` returns `None` through `?`, as before. The tests may panic. |
| R02 | ROOT's word and this review's proposed fix; F4's alternatives above. |
| R03 | No production literal added or changed. The test's sizes are a test's, sized from measured leak rates. F4's window is the ring's own work, measured where it runs, with no cold start. |
| R04 | `Stalled` is typed. The slot's states (`Making`, made, released, lost) are unchanged. |
| R05 | The room is read and acted on in one critical section of its one owner. F4's wait follows progress, with no deadline of ours. No cache. |
| R06 | `reserve` gains the read and the start ring's choice; `take` loses a block; `hello_in` and `swap` lose their reads. |
| R07 | Comments say why: the invariant, the generation's order, the window's basis. |
| R08 | No performance claim. |
| R09 | F3 was reproduced live, red, before its cure (r20). F4 has no live reproduction here: it needs a real VM. |
| R10 | F3: red, green and guard-removal mutation, with their patches and the sizing harness committed (r20). F4: owed with its E2E. |
| R11 | shards-init's tests ran on Linux x86_64, and its lints for both musl guests; engine_ring's lint ran on macOS arm64. Unverified: shards-init's tests on Linux arm64, engine_ring's lint on Windows and Linux (CI on main), every real VM. |
| R12 | One owner, one path for every host. |
| R13 | The cure moves one read; the test adds one barrier and one field. |
| R14 | Not run (ROOT's): how long the lock is held with the read inside it. |
| R15 | Implementer's light tests, on one host each; landed fast-forward on the person's rule of 2026-11-13. |
| R16 | The carrier's clones and target directories on both runners are removed at the end. |

### What ran where (cures)

| Log | Host | What |
|---|---|---|
| [r20](logs/r20-cure-f3-at-once.txt) | adacachy2, Linux 7.3.3 x86_64 | F3: red, the sizes' leak rates, green (ring:: 22 of 22), clippy for x86_64-gnu and both musl guests, mutation |
| [r21](logs/r21-cure-f4-settle-lint.txt) | adas-mac-studio-3, macOS 26.2 arm64 | F4: `cargo clippy -p shards --test engine_ring -- -D warnings` |
| [r22](logs/r22-cure-merged-gates.txt) | both, on the merged tree (origin/main 5b91d43de and this lane, merge abdc66854) | r20's green and lints, and r21's lint, again: all pass; fmt clean |

### Owed to ROOT (cures)

1. **The pins test with its new settle, on real VMs**
   (`an_idle_microvm_pins_nothing_and_each_streams_pins_go_as_it_ends`), with item 1's commands above:
   HVF arm64, KVM x86_64, KVM arm64 through L1, and WHP x86_64 (past 13 streams it waits on D181's
   bound, O3). WHP arm64 is unverified: no host.
2. **F4's red**, on one of those hosts, with every slot kept so that the first settle stalls:
   `sed -i 's/Disposal::Release { given_back } => p.release(slot, given_back),/Disposal::Release { .. } => p.lose(slot),/' crates/init/src/ring.rs`,
   then that host's command for the pins test. Expected: refused "1 vCPUs: the setup's slots freed: N
   pages pinned, not M, after …: none released over the last …, its window …", one to two windows after
   the setup, where before it waited 600 s. Then `git checkout crates/init/src/ring.rs`.
3. **F3's test on Linux arm64** (CI on main runs it), and R14's measure of how long the lock is held.

## Cures, F5 (goal 52's agent, 2026-11-13)

F3's child saw F5 in the source. Before this cure, `Pool::made`'s failure arm (crates/init/src/ring.rs) gave a failed
slot's bytes back to the room (`making` lessened, its table entry freed) without moving `room_gen`. A stream refused
growth while that slot was being made (`swap`: `refused_at = room_gen`, read before the room) never asked again until
an unrelated free or a word of the run's room. So growth came late, though pins never passed the room.

- **Cure:** the failure arm moves `room_gen` as `release`, `lose` and `heard` do, where the bytes come back.
- **Test:** `ring::tests::a_slot_whose_making_fails_moves_the_room_a_refused_growth_waits_on` uses a pool with room
  for one `MIN_RING` slot. It reserves that slot, sees a second reserve refused, fails the making (`Unmade::Short`),
  then needs `room_gen` moved and the room back. It waits on facts and has no constant beyond the ABI's own
  `slot_len(MIN_RING)`.
- On adacachy2 (Linux 7.3.3 x86_64, rustc 1.98.1), at main 454aa9a70:

| Run | Code | Result |
|---|---|---|
| Red | the test alone | FAILED: "a growth refused at 0 would never ask again" (left 0, right 0) |
| Green | the test and the cure | 1 passed; `ring::` 23 passed |
| Guard removed | the cure's `room_gen` move taken out, which is the red run's code | FAILED, as red |
| Lint | `cargo clippy -p shards-init --all-targets -- -D warnings`, for x86_64-gnu, x86_64-musl and aarch64-musl | clean |

`cargo fmt -p shards-init -- --check` is clean. Owed to ROOT: the test on Linux arm64 (CI on main).

## Cures, goal 71 (CI cluster 9), 2026-11-13

Two failures of engine_ring.rs on main, one on HVF and one on CI's Linux, both in the test's
waits. Neither shows a defect of the ring.

- **Identity:** goal 71's child for CI cluster 9, role implementer (ROOT "main"). Base: origin/main
  d5afadbc8ed9f5c7ed5dcedee7a7a88da7300efe; the cure's commit 3c93f59e9, merged with main d9f759025 as
  b97d37664 before landing. Policy at the base:
  AGENTS.md f66d304c…, development-rules.md df2acf47…, development-evidence.md 61613a24…; POLICY_READ
  sent before any edit. Owned: crates/shards/tests/engine_ring.rs, `common::Told`'s Windows pipe name,
  `shards_api::pause` made public (not copied), D230's text and this section.
- **Host:** adalundhe-macbook-pro (macOS 26.5.2 arm64, 18 CPUs, rustc 1.98.1), through a carrier: a
  blobless clone of GitHub's repository, this lane's commits from a bundle, `CARGO_TARGET_DIR` under
  ~/g121-tmp/g71-ring, `CARGO_INCREMENTAL=0`, `nice -n 10`, `SHARDS_HELPERS=skip` with placeholder init
  and server binaries. No E2E, VM, CI or scripts/lint ran, and nothing was built on adas-macbook-pro-2.

### G1. The settle's window refused 18 vCPUs on HVF (dc7f45bc6)

- **Symptom** (ROOT's workspace run on macOS arm64, its log e2e-hvf9-workspace-dc7f45bc6.log):
  `an_idle_microvm_pins_nothing_and_each_streams_pins_go_as_it_ends` passed at 1 and 2 vCPUs and failed
  at 18, "every stream's slot freed: 21 pages pinned, not 0, after 206.90675ms: none released over the
  last 201.082458ms, its window 176.721958ms". HVF's batch at 9483b5a84 had passed it.
- **What the evidence shows.**
  - The device let go of every ring, bytes for bytes, before the settle began: the test's own checks
    passed, and the device's lines in the daemon's log end with the last streams' rings let go
    (`HostEnded`).
  - Nothing on the ring's path changed between 9483b5a84 and dc7f45bc6 (git log over init's ring.rs
    and engine.rs, the vsock device, the ABI's ring.rs, the test guest's engine_bulk.rs, engine_ring.rs
    and tests/common/mod.rs).
  - The relay frees a slot only once its stream's thread has heard the device let go and passed on what
    the slot held; its vsock connection closes after that (engine.rs `relay`, ring.rs
    `Stream::serve`). Its release drops the pins in the update itself, no request holding the buffer
    (ring.rs `unregister`, on the guest's Linux 6.18.48).
  - Whether the 21 pages were freed later is not in the log: the test stopped at 207 ms. So no defect
    of the ring is shown, and none is ruled out. A real VM answers that (owed below).
- **Cause, in the test.** The settle's window was the time its own work had taken (a2c887eeb), a seed
  of no answer of the settle's own. The person's ruling of 2026-11-13: "No record seeds. The cold start,
  and every wait after a run-out, follows the party's liveness and progress. After that the bound is the
  slowest answer seen. No ×2."
- **The cure (3c93f59e9).** The settle is D215's judge, shared, its progress a slot freed (the pins at a
  new low).
  - Its kind is a work (the setup's, or the streams') at the microVM's vCPU count, each kind's record
    its own, in the test's memory.
  - A kind not yet seen follows its microVM's liveness alone, each look an exec it answers. A kind seen
    is looked at once each W* and refused where no slot was freed since the look before (`Stalled`,
    carrying `shards_api::Stall`).
  - Its looks are paced by D215's `sqrt(2 c w)`, `shards_api::pause` made public rather than copied, `c`
    a look's own time. Its progress is told on stderr as it comes, its home named, and each settle's
    time is reported per size.
  - In one run each kind settles once, so every settle there is a cold start. Its gap: a slot never
    freed while its microVM lives is not refused on a kind's first settle. Its looks go on, ever further
    apart, until its caller ends it. A learned settle reads the guest's pins alone, not its host's hold
    on the VM process, which shards-api's followers read privately.
- **Not chosen.**
  - `common::Patience`: its 600 s and `DAEMON_QUIET` are numbers of ours.
  - D185's slowest gap between frees, learned within a settle: its first gaps are short, so a later,
    longer gap of the same settle is refused. Shared by the setup and the streams, the setup's few
    slots, freed a connection at a time, would bound the release of the streams' 3V, which is the
    failure's own shape.
  - One record for every size of a work: the 1 and 2 vCPUs' answers then judge the 18. Its red is below.

### G2. The held connection's wait looked for a state a give-back ends (f5c0c8b04, CI)

- **Symptom** (CI run 43435318858, job x86_64-unknown-linux-gnu, its log lines 3590 to 3880):
  `a_shrinking_room_lets_an_idle_slot_go_and_a_stream_past_it_goes_plain` (engine_ring.rs:355) failed,
  "shards exec prov /bin/testguest api POST /containers/idle/wait did not exit within 600s" and "the
  held connection's ring: Rings { attached: 4, pinned: 49152, let_go: 4, unpinned: 49152, asked: 1 }".
  The musl job of the same run passed it.
- **What the evidence shows.** In the daemon's log, the held connection's ring attached at 1272652 µs
  of VM 460995's clock. The daemon's give-back to that microVM answered next ("giving back what it holds
  cold: answered after 69.022659ms"), and the ring was let go `(Asked)` at 1303913 µs, 31 ms after it
  attached. The daemon said why: "the host is short of memory: its microVMs gave back 3 MiB ...". The
  test looked each 50 ms for the ring attached and not yet let go, a state that lasted 31 ms between two
  of its looks, and waited on. The held exec, a wait on a joiner the test removes only at its end, ran
  to its own 600 s.
- **The product did as D230 says:** a give-back lets every idle slot go, the connection goes on over
  plain vsock, and the device let go of it bytes for bytes. No defect of the let-go path is shown.
- **The cure (3c93f59e9).**
  - `held_open` waits for the device's word that the ring attached, which the log keeps
    (`held_attached`).
  - Each test checks that its ring was not let go before it asks for the slot (`held_still`). Once let
    go a ring stays let go, so a ring found attached then was attached all along.
  - The shrinking-room test hears its host's pressure only through a pipe of its own, never said, as
    the give-back test does. The daemon then takes its host's word from that pipe in place of its
    kernel's (reclaim.rs `TOLD`), and its budget's room is still told through its file watches
    (budget.rs `watch_budgets`, groups.rs `hold_groups`).
  - `common::Told` is named per instance on Windows, so that both tests' pipes live at once in one test
    process.

### Red and green (R09, R10)

Each red is a patch of the cure's own code restoring one old rule, the same unit tests run on it. The
unit tests take no VM: the settle is fed the looks of ROOT's HVF run, and the log's parser and the held
wait are fed the CI run's lines. At the merge b97d37664 ([r23](logs/r23-g71-settle-held-light.txt),
[cure-g71-light.sh](cure-g71-light.sh), kiln run r327), and the same again, red for the same reasons, at
the merge with main b87c0ce19, 4130e0725 (r341), and at the landed merge with main f6d6f7da2, e072a886f (r351:
main's window refused at 222.75 ms, the sizes shared at 9.22 ms, W* 8.59 ms):

| Run | Code | Result |
|---|---|---|
| Green | the cure | 4 passed: `a_settle_of_a_size_not_yet_seen_follows_its_liveness_whatever_other_kinds_answered`, `a_settle_of_a_kind_seen_is_refused_a_whole_w_star_after_its_last_freed_slot`, `a_settle_of_a_kind_seen_goes_on_past_w_star_while_its_slots_are_freed`, `the_held_connections_wait_reads_a_word_its_log_keeps` |
| Red: main's window | [cure-g71-red-main-window.patch](cure-g71-red-main-window.patch): a kind not yet seen seeded with its work's own time, a2c887eeb's window | the HVF test FAILED: `Stalled { now: 21, want: 0, waited 223.16 ms, over 223.16 ms, slowest 176.722 ms }`, as main refused HVF at 207 ms; the other three passed |
| Red: sizes shared | [cure-g71-red-sizes-shared.patch](cure-g71-red-sizes-shared.patch): one record for every size of a work | the HVF test FAILED: refused at 21.61 ms, W* 21.09 ms, the 1 and 2 vCPUs' answers. The two learned-kind tests failed too, at their own lookups of a record by kind, which the patch moves |
| Red: main's held wait | [cure-g71-red-held-attached-still.patch](cure-g71-red-held-attached-still.patch): the ring attached still, main's predicate | `the_held_connections_wait_reads_a_word_its_log_keeps` FAILED at its `held_attached(&r, 3)`; the other three passed |
| Lint | `cargo clippy -p shards-api -p shards --all-targets -- -D warnings` | clean |
| shards-api | `cargo test -p shards-api --profile test-release --lib a_pause_weighs` | 1 passed |

`rustfmt --check` is clean for every file touched.

- **What the reds found in the test itself.** At 5f129b93d (r291) all three reds failed as here. Merged
  with main as 5bd87fcae (r313), main's window passed: the HVF test looked only while its time was within
  206.907 ms, so a pause that ended past it, its judged look still to come, left the seeded rule
  unjudged, and the last look (0 pages) answered first. Its guard hung on a pause's lateness. Its last
  look now begins past 206.907 ms, however late a pause ends. In the same run the sizes-shared red failed
  at the test's own lookup of the other kinds' records, not at a refusal; that lookup now reads
  `records.get`.

### R01-R16 for this cure

| Rule | |
|---|---|
| R01 | No production code changed but `pause`'s visibility. The tests may panic. |
| R02 | The person's ruling, D215's judge and D185's and D180's records of it; alternatives above. |
| R03 | No literal added. W* is each kind's own, learned in the test's memory; the looks' pacing is D215's derivation; the HVF test's figures are that run's own. Cold start: liveness alone, its gap stated. |
| R04 | `Stalled` is typed and carries the judge's `Stall`. A settle's outcomes are settled, not yet, or refused. The held connection's state is read from the device's words, which stay said. |
| R05 | The settle's looks are paced by a derivation, not a sleep of ours. The held waits read words that stay said. `await_rings`' 50 ms looks and `Patience` are as they were. No cache. |
| R06 | `Settle::looked` is one judgment; `held_attached` and `held_still` are a line each. |
| R07 | Comments give each source and the evidence. |
| R08, R14 | No performance claim. |
| R09 | Both symptoms are reproduced from their own logs in unit tests, red on main's rules. Neither is reproduced on a real VM here (ROOT's). |
| R10 | Red, green, and each guard's removal as above, with their patches and log committed. |
| R11 | Ran on macOS arm64 alone. The Windows-only `Told` change and every other target's lint are CI's on main; every real VM is ROOT's. |
| R12 | D215's judge and `pause`, shared. |
| R13 | Two small types and a judgment; the old settle's loop gone. |
| R15 | Implementer's light tests on one host; landed fast-forward on the person's rule of 2026-11-13. |
| R16 | The carrier's clone and target directory on adalundhe-macbook-pro are removed after landing. |

### Owed to ROOT (real VMs)

1. engine_ring's four E2E tests and four unit tests, on HVF arm64 (the pins test at 1, 2 and the
   host's every CPU), KVM x86_64 (CI's glibc and musl jobs), and WHP x86_64:
   `cargo test -p shards --profile test-release --test engine_ring -- --nocapture`. To see the HVF
   failure's conditions, run it as ROOT's workspace run did: the binary's tests at once, under the
   session's scheduling. Each size's settle times are printed ("the setup's slots … into their settle
   (it took …), the streams' … into theirs (they took …)").
2. KVM arm64 through L1, on an M3 or later Mac: `SHARDS_L1_TESTS=engine_ring cargo test -p shards
   --profile test-release --test boot snapshot_tests_pass_on_a_kvm_host_of_this_architecture --
   --nocapture`, timed through docs/research/measurements/macos-sched/launchd-run.sh.
3. If a settle goes on without its pins falling, its told line names its home: look at it live with
   `SHARDS_HOME=<home> shards exec prov /bin/testguest vmstat nr_foll_pin_acquired
   nr_foll_pin_released`. A count that stays past every stream's end is a slot the relay did not free:
   the first live evidence of a defect of the ring, where these logs show none.
