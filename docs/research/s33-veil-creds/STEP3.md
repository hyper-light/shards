# Step 33, step 3: the injector, TLS ended for credential-granted hosts and HTTP/1.1 heads filled in

Kiln goal #70 (id 8f474b45c7cd86559e0566f22f9d0c33), plan step 33 of kiln plan 01a1cd8e862e7169a074db3d5fa33f3d; step 3 of
the goal's plan 01a1d2b88be57304b972761e2a5c6781 (research 01a1d2b88be57304bf6b081fe46832e0: f16, f20, f21, f26, f27,
f32-f34, f42-f44, f56, f65). Role: implementer. Policy read at origin/main 82659e407: AGENTS.md sha256 f66d304c…,
docs/development-rules.md df2acf47…, docs/development-evidence.md 61613a24…. Worktree `s33-inject`, branch
`p/s33-inject-v1`, from main ba0e32613, fast-forwarded to 82659e407 before any edit. The design and its decisions are
D242's "Built: step 3"; this file is the light-test record.

## Slice 1: each grant's placeholder, and the decision's shared home

### Change

- `crates/dns/src/credential.rs` (new): `CredentialGrant { reference, toward, placeholder }` (its `Debug` shows the
  placeholder's length alone), `Fill`, `decide(grants, sni, host, token)` (moved from shards-api's `wire.rs`, the
  placeholder compared in constant time), `placeholder` (RFC 6750 §2.1's `b64token`), `check` and its typed `Refused`.
- `crates/api/src/image/wire.rs`: the three re-exported; version 22 appends each grant's placeholder; the decision's
  test moved to shards-dns.
- `crates/api/src/image.rs`: each grant's placeholder carried and checked (`credential::check`).
- `crates/net/src/lib.rs`: `Grant` is the shared grant; `NET_CREDENTIALS` appends each placeholder (two-byte length);
  the grants checked again as they come.
- `crates/shards/src/api_run.rs`: the grants checked again (`Cause::Request`) and passed whole.

### Decisions (D242 records each with its alternatives)

- The placeholder rides its grant, minted by the orchestrator (kiln's broker: `kiln-placeholder-` and 256 random bits in
  hex, 81 bytes), as D242's Literals foresaw ("carried to the injector with the run's grants").
- `decide` and the grant live in shards-dns: both sides depend on it already, and Hex vendors it already (vendor/shards
  of Hex origin/main 025aae6d: abi, afunix, api, dns, ipc, testdir).

### Tests (one host: adas-mac-studio.local, macOS arm64, Darwin; kiln runs through the carrier
`.claude/worktrees/s33-inject-carrier`, `CARGO_TARGET_DIR=~/g121-tmp/s33-inject/target`, `SHARDS_HELPERS=skip`,
`CARGO_INCREMENTAL=0`; each command's whole output in `~/g121-tmp/s33-inject/logs/<name>.log`, read with `grep -a`)

- Run r342, head 51e674604:
  - `cargo test -p shards-dns --lib -- credential`: 4 passed.
  - `cargo test -p shards-api --lib -- image::`: 56 passed.
  - `cargo test -p shards-net --lib -- credential lease each_control_socket`: 12 passed.
  - `cargo test -p shards --bin shards -- api_run::`: 35 passed.
  - `cargo clippy -p shards-dns -p shards-api -p shards-net -p shards --all-targets -- -D warnings`: clean.
- Run r356, head dc2a12d61 (the network process's own check tested): shards-net's set 13 passed; the reds below.

### R10: each guard's red, shown once (by hand: `scripts/step3-reds.py`, from the repository root)

Run r356 on adas-mac-studio.local, head dc2a12d61: each guard removed by its hand-written replacement, its one test run
with `--exact`, the file restored; 9 of 9 red, each failing at its guard's assertion.

| Guard removed | Test that fails | Failed at |
|---|---|---|
| `decide` filling a grant's own placeholder alone | `credential::tests::decide_fills_only_a_grants_placeholder_on_an_agreed_granted_host` | credential.rs:199, another grant's filled |
| `decide` filling toward the grant's hosts alone (a fill regardless of host) | the same | credential.rs:200, filled toward another host |
| `check` refusing a placeholder that is no token | `credential::tests::each_placeholder_is_a_token_and_one_credentials_alone` | credential.rs:276 |
| `check` refusing one placeholder for two credentials | the same | credential.rs:270 |
| a grant's `Debug` hiding its placeholder | `credential::tests::a_grants_placeholder_never_prints` | credential.rs:291 |
| shards-api's request checking the placeholders | `image::tests::a_credential_grant_rides_by_reference` | image.rs:836 |
| each placeholder written to the image channel | `image::wire::tests::messages_round_trip` | wire.rs:2699, the request unread |
| the run host checking them again | `api_run::tests::credential_grants_reach_the_network_process_by_reference` | api_run.rs:4665 |
| the network process checking them again | `tests::credential_grants_are_taken_with_their_placeholders_checked` | lib.rs:3737 |

### Owed (slice 1)

- Hex: each grant's placeholder filled as it stages the credential into its member (plan step 6), its vendored
  shards-api at version 22.
- ROOT: CI's matrix on main.
