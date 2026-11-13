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
3. **F3's red and fix** (Linux): the test and fix above, with a stress test of concurrent streams.
4. **F4's change** with the E2E of item 1.
5. **Light gates this step could not run:** `cargo test --locked -p shards-init ring::` and
   `cargo test --locked -p shards-vmm --lib -- devices::virtio::vsock` on Linux x86_64 and arm64 (musl or glibc);
   the vsock device's tests and clippy on Windows x86_64 (no Windows target or cargo-xwin on the Mac
   runner); `scripts/lint` of the seven targets for b81d707a9 (it compiles for each).
6. **The lane's own owed items** (README.md): the engine suite (D181), isolation's token test, the budget
   and reclaim suites on real VMs; the idle-pins red on 032b63b1c; the E2E mutations (`told` removed from
   `ROOM`, the leaf join removed); the measurements.
