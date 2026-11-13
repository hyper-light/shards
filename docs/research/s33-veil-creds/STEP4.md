# Step 33, step 4: values just in time, leased through the run host, released as the run ends

Kiln goal #63, plan step 33 of kiln plan 01a1cd8e862e7169a074db3d5fa33f3d; step 4 of the goal's plan
01a1d2b88be57304b972761e2a5c6781 (research 01a1d2b88be57304bf6b081fe46832e0: f7, the run host as the orchestrator's
only party, over its stdio; f35, Vault's leases tied to their holder's life; f72, kiln-seams' `Secrets` leases, no holder
identity in a `Lease`). Role: implementer. Policy read at origin/main 8479be870: AGENTS.md sha256 f66d304c…, docs/
development-rules.md df2acf47…, docs/development-evidence.md 61613a24…. Worktree `s33-lease`, branch `p/s33-lease-v1`,
from origin/main 8479be870, fast-forwarded to 72cdfbf06 before any edit. The design and its decisions are D242's
"Built: step 4"; this file is the light-test record.

## Change

- `crates/net/src/lease.rs` (new): the network process's lease client. `Leases` holds one lease of a credential at a time,
  asked (its waiting flows) or held (its value in `Zeroizing`, its end on this process's clock). `ask` returns the value,
  joins the flow to an ask, or refuses typed (`Ungranted`, `Ended`); `asks` hands the loop what to send; `answered` takes
  the run host's answer; `resumed` names the flows to resume; `lapse` and `next_end` release values at their ends;
  `end` releases everything. `encode_ask`/`decode_ask`, `encode_answer`/`decode_answer`: `NET_LEASE`'s payloads.
  `Refusal`: `Ungranted`, `Live`, `Denied`, `KeyReplaced { asked, current }`, `Unavailable`, `Ended`, `Lapsed`.
- `crates/net/src/lib.rs`: `Credentials.leases`, made as the grants come; `NET_LEASE` answers taken from the run host's
  control alone (`Control::Daemon`), the message's payload wiped once taken; asks sent each pass (`ask_run_host`);
  leases ended as the run host's control hangs up; values released on the loop's timers.
- `crates/ipc/src/lib.rs`: `kind::NET_LEASE = 100` (98 and 99 went first to D251's `LANE_OPEN` and `LANE_HELD`).
- `crates/api/src/image/wire.rs`: run channel version 16 (14 and 15 went first, to guest-cpu-time-v1 and D252);
  `to_client::LEASE` (30), `to_client::RELEASED` (31), `to_host::LEASED` (13); `LeaseAsk`, `Lease`, `LeaseRefusal`,
  `LeaseAnswer`, `released`; `Secret` wiped as it drops.
- `crates/api/src/image.rs`: `Lessor` (the orchestrator-side interface: `lease(reference)`, `release(lease)`),
  `Leased`, `RunLessor`, `ImageSpec::lessor`; a spec with grants and no lessor refused.
- `crates/api/src/image/session.rs`: each `LEASE` asked of the run's lessor on a thread of its own and answered
  `LEASED`; the session's ledger (asked, given); every lease given released once, at `RELEASED`, at the channel's end,
  or at once when given after the release or lapsed.
- `crates/shards/src/api_run.rs`: `Ledger` (asked, held; one lease per grant), `Leasing`, `relay` (the network process's
  asks to `LEASE`), `leased` (`LEASED` to `NET_LEASE`), `end_run` (every end: `RELEASED` then `ENDED`), `Audit`
  (diagnostics by reference and id), `net_gone` (the network process reaped after the run's end, so that the relay ends
  whatever that process does). `Answers` takes any writer, for tests.
- `zeroize` (already in the lockfile) for shards-net and shards-api; `lessor: None` in the three E2E specs.

## The identity decision (for ROOT to confirm)

A lease is asked under the run's own key where it has one, and by its channel where it has none; no key is made for
leases alone. Evidence: D238's key is made where an agent-control endpoint binds, by the daemon (`daemon/managed.rs`
`bind_agent`, `vm_key`) and, since D252 (merged here from main), by `shards api-run` where its orchestrator asks for an
endpoint (`ImageSpec::control`; the key's SHA-256 told in `Started::control`, before the relay starts). The session
takes it as it takes `STARTED` and gives it to the run's `Lessor` with each ask (`Lessor::lease(reference, key)`), so
the broker issues the leases to it and refuses a replaced one as `KeyReplaced { asked, current }`, carried end to end.
A run with no endpoint has no key; its host's stdio is its orchestrator's alone (D231, D248, f7), and its `Lessor` is
given for that run alone. Moves are the daemon's runs'; the daemon gives no network process grants yet. D242 records it.

## Tests (one host: adas-mac-studio.local, macOS arm64, Darwin; kiln runs through the carrier
`.claude/worktrees/s33-lease-carrier`, cold `CARGO_TARGET_DIR=~/g121-tmp/s33-lease/target`, `SHARDS_HELPERS=skip`)

- Run r244, head fa88b2328:
  - `cargo test -p shards-ipc --lib -- every_message_kind_has_its_own_number`: 1 passed.
  - `cargo test -p shards-net --lib -- lease each_control_socket credential`: 12 passed.
  - `cargo test -p shards-api --lib -- image::`: 37 passed, 1 failed:
    `a_lease_the_run_was_not_granted_breaks_the_channel`. A real defect: the session's `insert` of a second ask by an id it
    held a lease by put the ask in the given lease's place before refusing it, so the channel's end released nothing of
    that lease. Fixed in 5dd06d453 (the map's entry: refused before anything is put).
  - `cargo test -p shards --bin shards -- api_run::`: 21 passed.
  - clippy: `type_complexity` on `decode_answer`'s return type; named (`lease::Answer`) in 5dd06d453.
- Run r256, head 5dd06d453: shards-net 12 passed; shards-api `image::` 38 passed; `cargo clippy -p shards-ipc -p shards-net
  -p shards-api -p shards --all-targets -- -D warnings` clean.
- Run r301, head a9c0ae4e4: the reds below. Its greens' report was cut by the carrier's 80-line filter, so:
- Run r320, head a9c0ae4e4, the tree clean after the reds (0 files changed): shards-net 12 passed; shards-api `image::` 38
  passed; shards `api_run::` 21 passed; shards-ipc's `every_message_kind_has_its_own_number` 1 passed; the four crates'
  clippy, `--all-targets -- -D warnings`, each re-checked (`Checking shards v0.7.0`, sources touched), clean.
- Run r332, head 87862c2da (merged with main e1995061f, which brought step 2's second slice): shards-net 12, shards-api
  `image::` 38, shards `api_run::` 23 (main's two added), shards-ipc 1 passed; the four crates' clippy clean.
- Run r385, head 4985c80b2 (merged with main dcb403688, whose guest-cpu-time-v1 took the image channel's version 14, so
  the leases take 15; and main's one unformatted line of wire.rs formatted): shards-net 12, shards-api `image::` 38,
  shards `api_run::` 23, shards-ipc 1 passed; the four crates' clippy, `--all-targets -- -D warnings`, clean
  (`Compiling shards v0.7.0`, its build script rerun after main's build.rs change, then checked).
- Between r282 and r301 the runner's disk was full (116 MiB free; this lane's directory was 6.1 GB of it, other lanes'
  far more); another lane's cleanup freed 87 GiB before r301, and nothing of others' was touched here.

## R10: each guard's red, shown once (crates' tests, by hand; `scripts/step4-reds.py` holds each removal; it ran from the carrier, run from the repository root)

Run r301 on adas-mac-studio.local, head a9c0ae4e4: each guard below removed by its hand-written replacement, its one test
run with `--exact`, the file restored; 16 of 16 red, each for its guard's reason (the line it failed at). The two waits
of 120 s are the tests' bound on what should come at once (`BOUND`, as `crates/shards/tests/api_image.rs` bounds its).

| Guard removed | Test that fails | Failed at |
|---|---|---|
| `Leases::ask` refusing what no grant names | `lease::tests::a_run_with_no_grant_asks_nothing` | lease.rs:492, asked anyway |
| `Leases::ask` joining a flow to the lease asked | `lease::tests::a_value_is_asked_on_first_use_and_held_while_its_lease_lives` | lease.rs:445, two asks |
| `Leases::lapse` releasing a lease at its end | the same | lease.rs:482, none released |
| `Leases::end` releasing what is held | `lease::tests::the_runs_end_releases_every_lease` | lease.rs:560, held past the run |
| `Control::Daemon` saying `NET_LEASE` | `tests::each_control_socket_says_only_its_own` | lib.rs:3590 |
| `ask_run_host` ending the leases with no control | `tests::a_lease_is_asked_on_the_run_hosts_control_and_taken_as_its_answer` | lib.rs:3657, a flow never told |
| `LeaseAnswer::decode` refusing a life of none | `image::wire::tests::leases_round_trip_and_bad_ones_are_refused` | wire.rs:2406 |
| `request` refusing grants without a lessor | `image::tests::a_run_with_credential_grants_needs_a_lessor` | image.rs:684 |
| the session releasing at `RELEASED` | `image::session::tests::a_runs_leases_are_given_as_asked_and_released_at_each_end` | session.rs:1956, after 120 s |
| the session releasing as its channel ends | the same | session.rs:1991, a lease never released |
| `settle` releasing a lease given after the release | `image::session::tests::a_lease_given_after_the_runs_release_is_released_as_it_comes` | session.rs:2015, after 120 s |
| `lease_asked` refusing an ungranted reference | `image::session::tests::a_lease_the_run_was_not_granted_breaks_the_channel` | session.rs:2036 |
| `Ledger::ask` refusing what no grant names | `api_run::tests::a_runs_ledger_holds_one_lease_of_a_credential_and_releases_them_all` | api_run.rs:4125 |
| `Ledger::ask` holding one lease of a credential at a time | the same | api_run.rs:4127 |
| `Ledger::ask` refusing after the run's release | the same | api_run.rs:4151 |
| `end_run` releasing the leases before the run's end | `api_run::tests::a_granted_runs_lease_is_relayed_on_first_use_and_released_as_it_ends` | api_run.rs:4216, `ENDED` with no `RELEASED` |

And one red that was a defect, not a removal: r244's `a_lease_the_run_was_not_granted_breaks_the_channel` (above),
green since 5dd06d453.

## Not tested here

- That a dropped `Secret`, `Zeroizing` value or wiped message holds zeros: zeroize's own guarantee (zeroize 1.9.0,
  `Zeroize for Vec<Z>` zeroes the whole capacity, then clears), not re-proved by these tests.
- The relay's start and end inside a real run (`serve`, the link thread's `end_run` and `net_gone`, the asks loop's
  `LEASED`): each needs a VM. The pieces they call are tested above; their wiring is ROOT's real-VM run.
- Windows and Linux: CI on main runs the matrix (the person, 2026-11-13); ROOT watches it.

## Owed (ROOT)

- A real run with a credential grant through `shards api-run` (KVM x86_64, HVF, WHP): a lease asked on first use and
  released at the run's exit, at `STOP` and at its orchestrator's channel closing, its diagnostics saying each by
  reference and id, and no value in them. Nothing in a run asks until the injector (step 3) calls `Leases::ask`.
- Hex's vendored shards-api takes `ImageSpec::lessor` (step 6), and its `Lessor` is kiln's broker (step 5).
- The daemon's runs have no grants: their lease relay, and a move's release there, are owed with them.
