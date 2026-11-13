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

### Landed

Main 914a85a1b..fcc6d2147 after four merges of main, each rerun where main touched this slice's crates: r367
(da4da7c4f: dns 4, api `image::` 57, net 13, shards `api_run::` 37, clippy clean), then api `image::` 57 and shards-api's
clippy at 0f71af555, and shards `api_run::` 37/38 with the four crates' clippy at d5463d82f and fcc6d2147.

### Owed (slice 1)

- Hex: each grant's placeholder filled as it stages the credential into its member (plan step 6), its vendored
  shards-api at version 22.
- ROOT: CI's matrix on main.

## Slice 2: the injector

### Change

- `crates/net/src/inject.rs` (new): `Flow` (its states `Hello`, `Ended`, `Gone`; a request's `Head`, `Parked`, `Body`,
  `Sent`, `Tunnel`, `Done`; a response's `Head`, `Body`, `Tunnel`), `Upstream`, `Run`, `Audit`, `Queues`, `Said`, the
  typed `Refusal`, the HTTP/1.1 reading (heads, RFC 9112 §6.3's framing, chunks, hop-by-hop fields), and the mask.
- `crates/net/src/tcp.rs`: `Conn.inject`, the window less what it holds (`held`), the run's credentials on
  `on_segment` and `on_readable`, `resume`, the injector's `Close` (both sides end once their queues drain).
- `crates/net/src/lib.rs`: `Prepared`, `Credentials.upstream`, each flow of a run with credentials given a `Flow`
  (Windows' dialled ones too), `resume_flows`, the loop's end running it and `ask_run_host` until neither moves, the
  audit on stderr.
- `crates/net-process/src/main.rs`: `Prepared` made before the process confines itself.
- `crates/verify` (new, shards-verify): the registry's verifier moved (`apple.rs` by `git mv`, and the refusal of a
  host with no roots); `crates/registry` takes it from there; `crates/shards/build.rs` lists it among the helpers'
  inputs; `Cargo.lock` by `cargo metadata --offline` (the new crate, httparse and shards-verify for shards-net).

### Decisions (D242's "Built: step 3's second slice" records each with its alternatives and evidence)

- Each flow is read for its ClientHello; one whose server name no grant names passes as it came.
- TLS 1.3, TLS13_AES_256_GCM_SHA384 alone both ways, X25519MLKEM768 first, `http/1.1` alone, nothing resumed.
- The echo mask is a same-length mask: the value's bytes replaced by its placeholder's, cycled; no framing changes.
- The verifier lives in shards-verify, which the registry and the network process both use.
- Hop-by-hop fields: every connection option's field goes but `close`, `keep-alive` and `upgrade`, which the injector
  applies to both hops alike.

### Tests (adas-mac-studio.local, macOS arm64; the same carrier and environment)

- Run at 41d7b6fb4: shards-verify 3, shards-registry `tls::` 2; shards-net did not build (a test's helper named as
  its local, `mask`), and clippy asked for `?` and for the flow's `from_` steps of `self` to be named otherwise.
- Run at 4f7f8ca99: shards-net 43 passed, 1 failed: the handoff test's window held at an unscaled window's 65535;
  its connection scales its window since 7465e196e. Clippy of shards-verify, shards-registry, shards-net and
  shards-net-process clean.
- Run at 7465e196e: `cargo test -p shards-net --lib -- inject tcp:: credential lease each_control_socket ca::` 44
  passed (the injector's 11 among them); `cargo test -p shards-verify` 3; `cargo test -p shards-registry --lib --
  tls::` 2; `cargo test -p shards --bin shards -- api_run::` 38; `cargo clippy -p shards-dns -p shards-verify -p
  shards-registry -p shards-net -p shards-net-process -p shards --all-targets -- -D warnings` clean.

### R10: each guard's red (r466, head a91c1e92b, `scripts/step3-reds.py inject: tcp:`), 12 of 12

| Guard removed | Test that fails | Failed at |
|---|---|---|
| a Host that is not the SNI answered 421 | `inject::tests::a_request_for_a_host_that_is_not_the_sni_is_answered_421` | inject.rs:2202, no 421 |
| only a granted name's TLS ended | `inject::tests::a_placeholder_toward_an_ungranted_host_carries_no_credential` | inject.rs:1993, the guest's TLS refused the run's leaf |
| a value filled where `decide` says (a fill regardless of host) | the same | inject.rs:2009, after 30 s: the request parked on a lease of another host's credential |
| a request waiting for its lease | `inject::tests::a_granted_hosts_request_carries_the_value_and_its_echo_comes_back_masked` | inject.rs:2089, refused 503, nothing held |
| the echo of the value masked | the same | inject.rs:2110, the value reached the guest |
| a body streaming, held by nothing | `inject::tests::a_large_body_streams_through_unchanged_under_backpressure` | inject.rs:2013, after 30 s: a body held whole stalls within the window |
| hop-by-hop fields cut | `inject::tests::hop_by_hop_fields_go_and_the_rest_stays` | inject.rs:2413 |
| Transfer-Encoding beside Content-Length refused | `inject::tests::a_requests_framing_is_read_strictly` | inject.rs:2335 |
| a head's lines in CRLF alone | `inject::tests::a_heads_lines_end_in_crlf_alone` | inject.rs:2354 |
| no control in a chunk's extensions | `inject::tests::chunked_bodies_are_read_strictly` | inject.rs:2372 |
| AES-256-GCM alone on the guest's side | `inject::tests::a_guest_offering_no_aes_256_gcm_is_refused` | inject.rs:2298, after 10 s: served |
| what the injector holds out of the window | `tcp::tests::the_injector_passes_an_ungranted_flow_and_ends_a_granted_ones_tls` | tcp.rs:1892 |

### Not run here, and why

- The upstream verifier's own reds are the registry's, moved with it (shards-verify's tests, green).
- `resume_flows`, the loop's turn that resumes a parked flow on a lease's answer in the network process itself, has no
  unit test of its own: it is the integration tests' `Flow::resume`, driven by `Leases::resumed` as the loop drives it.

### Owed (ROOT)

- A real run with a credential grant through `shards api-run` on KVM x86_64 (the network process under its seccomp
  filter), HVF (App Sandbox: trustd) and WHP (AppContainer: CryptoAPI): a member's request with its placeholder to a
  granted host filled in, an ungranted host's passing, the audit lines in the run host's diagnostics.
- The injector's cost per connection (a leaf minted, two handshakes) and per request, and the CA's (step 10).
- CI's matrix on main (shards-verify is new; the network process and the registry now build it on every target).
