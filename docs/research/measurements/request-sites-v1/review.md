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
- **Item 3, the lost update:** in progress here. Its result will be recorded in this file when it lands.
- **Item 6, `<name>.new.<n>` left by a crash:** in progress with item 3.

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

Clippy for the landing is in the commit that lands it.

## Owed to ROOT

- This test on Linux and Windows (CI's unit tests run it there). Windows' hard link is CreateHardLinkW on NTFS.
- Windows' rename over an open record (the lane's item 2), on jupiter: `cargo test -p shards --bin shards
  a_request_kept_at_once_is_whole` is `cfg(unix)`, so a Windows twin is owed with this review's item 3.
