# request-sites-v1: review (plan step 2, goal 52's step 7)

Reviewer: goal 52's agent (role implementer), 2026-11-13, at origin/main 94be8818b. Policy at that revision:
AGENTS.md f66d304c…6266db41, docs/development-rules.md df2acf47…38368fb63, docs/development-evidence.md
61613a24…68fb63. It reviews 873ee6661 ("Every rewrite of a record a reader decodes, whole or absent (keep_whole)") and
the lane's record (README.md), with the record's open items 3, 4, 6 and 7 first.

## Findings

- **Item 4, engine-id's first making: reproduced, then cured.**
  - networks.rs `engine_id` read `networks/engine-id`. Where there was none, it made an ID and kept it with
    `keep_whole`, a rename over the record. Callers that asked at once each made their own, each kept by rename (the
    last one wins), and each returned its own, so their IPv6 ULA pools (`ula_base`) differed from the one the home
    keeps.
  - Clients are served on threads of their own (daemon.rs `spawn_scoped` per connection), so it happens whenever
    clients first touch a fresh home's networks at once.
  - The fix, `keep_first` (daemon.rs, beside `keep_whole`): write `<name>.new.<n>` whole, as `keep_whole` does, then
    `std::fs::hard_link` it to the record and remove the temp. The link fails AlreadyExists where another kept one
    first: link(2)'s EEXIST, CreateHardLinkW's ERROR_ALREADY_EXISTS. Each caller then takes the record, never its
    own.
  - `keep_whole` and `keep_first` share one counted name (`beside`, R12). No lock, no random source, no constant.
- **Item 7, D37:** the decision is now in D37 ("Every record a reader decodes is written whole or not at all", added
  by 89f411fc4). This review adds `keep_first` to it.
- **Item 3, the lost update: reproduced, then cured.**
  - `update` (update.rs `update_one`) reads a microVM's request, may exec into its VM (bounded by `TAKE_TIMEOUT`),
    and keeps what it read. `network connect`/`disconnect` (networks.rs `request_of` then `keep_request`) and an
    engine's own network (own_networks.rs `change_request`) each read, change and keep it too. Any two at once lost
    one change: the last keep wins over the other's.
  - The cure follows moby, the reference: `daemon/update.go` holds the container's lock across the runtime's
    update, and ConnectToNetwork takes it too. `Daemon::request_turn` gives each microVM one client at a time, held
    from the request's read to the keep of what changed (update's exec included).
    - It is a set of the microVMs being changed and one condvar, told as each turn goes: no Arc, a completion wait,
      not a probe (R05). It follows the daemon's `settling`/`settled` pattern.
    - All three writers take it: `update_one`; `request_of`, which hands its turn back with the request, held until
      the keep; and `change_request`.
- **Item 6, `<name>.new.<n>` left by a crash: not swept, decided (R13).** No reader opens one, a counted name is never
  used again, and `rm` removes it with its microVM. A crash between a write and its rename is the only way to make
  one. A sweep would add a writer to every record's directory, and nothing it removes was ever read.
- **Item 2, Windows' rename over an open record:** owed. `a_request_kept_at_once_is_whole` is `cfg(unix)`. On
  Windows, std's rename is MoveFileExW(MOVEFILE_REPLACE_EXISTING), with FileRenameInfoEx's POSIX semantics on access
  denied, and a reader's open handle may refuse it. Whether a keep fails while a client reads the record is to be run
  on jupiter (below).

## What ran where

On adas-mac-studio.local (macOS 26.2 arm64, 14 CPUs, rustc 1.98.1), at main 94be8818b, with `SHARDS_HELPERS=skip`
and placeholder init and server binaries. The test is
`daemon::tests::a_homes_first_engine_id_is_one_however_many_ask_at_once`: as many callers as the host runs at once
(two at the least), released together by a barrier, 200 rounds, each on a home with no engine-id.

| Run | Code | Result |
|---|---|---|
| Red | main as it is (rename) | FAILED: 200 of 200 rounds of 14 callers took an ID the home did not keep |
| Green | `keep_first` | 1 passed (1.08 s) |
| Guard removed | engine_id keeping by `keep_whole` again | FAILED: 200 of 200 rounds |
| Neighbour | `a_request_kept_at_once_is_whole` | 1 passed |

The lost update's test is `daemon::tests::an_update_and_a_connect_at_once_each_keep_their_change`. Two clients, on
threads of their own through the daemon's own command path (`ask`), run 200 rounds each against one stopped microVM:
`update --cpu-shares N` and `network connect nR`. At main ecd19d38c:

| Run | Code | Result (networks kept, last cpu-shares; want (200, 201)) |
|---|---|---|
| Red, 3 runs | main as it is | FAILED: (186, 185), (182, 174), (154, 186) |
| Green | `request_turn` at the three writers | 1 passed (0.20 s) |
| Guard removed | the turn taken without waiting | FAILED: (155, 186) |
| Neighbours | engine-id, request-kept, `update::`, `networks::`, `own_networks` | 8 passed |

The landing ran `cargo clippy -p shards --all-targets -- -D warnings`, natively and with cargo xwin for
x86_64-pc-windows-msvc: clean. `cargo fmt -p shards -- --check`: clean.

## Owed to ROOT

- This test on Linux and Windows (CI's unit tests run it there). Windows' hard link is CreateHardLinkW on NTFS.
- Windows' rename over an open record (the lane's item 2), on jupiter: the test with its `cfg(unix)` lifted,
  `cargo test -p shards --bin shards a_request_kept_at_once_is_whole`, to see whether a keep fails while a reader holds
  the record open (std opens without FILE_SHARE_DELETE).
- Both race tests (`a_homes_first_engine_id_is_one_however_many_ask_at_once`,
  `an_update_and_a_connect_at_once_each_keep_their_change`) on Linux and Windows; CI's unit tests run them.
