# Step 33, step 1: the credential contract (typed grants by reference) and D242

Kiln goal #63 (8f474b45c7cd86559e0566f22f9d0c33), plan step 33 of kiln plan 01a1cd8e862e7169a074db3d5fa33f3d; step 1 of
the goal's plan 01a1d2b88be57304b972761e2a5c6781 (research 01a1d2b88be57304bf6b081fe46832e0, findings f1-f76). Policy:
origin/main 1145bd3f2, development-rules.md sha256 df2acf47…; POLICY_READ.json beside this file. Built by the stopped
step-33 agent 01a1d1f9 (uncommitted), finished and landed by goal #63's agent.

## What this slice is (and is not)

The contract only: the typed credential grant, its wire form, the injector's typed decision, the spec's check, and
D242. Not the CA, the injector, the leases, the broker, sign-in or any real-VM run: those are the plan's steps 2-10.

## Change

- `crates/api/src/image/wire.rs`
  - `CredentialGrant { reference: String, toward: Vec<String> }`: a credential named by reference (a secret's path in
    the store, never a value) and the hosts it may be filled in toward. Carries no value, so it may print.
  - `Fill { With(String), Pass, Misdirected }` and `decide(grants, sni, host) -> Fill`: the injector is a gateway, not
    a proxy (RFC 9110 §3.7, §11.6.2), so its decision is typed (R04). `decide` fills only when the TLS SNI, the
    request's host and a grant's host agree, ASCII case-insensitively (RFC 9549 §2.3); a host that differs from the
    SNI is `Misdirected` (421, RFC 9110 §7.4); no grant is `Pass` (default deny).
  - `Request.credentials`, run channel `VERSION` 7 -> 8 (main took 7 for the budget room, D241), appended, so an older reader refuses a newer request by its
    version. Decoding bounds the grants' count by the bytes left (each grant at least 8 bytes, each host 4).
- `crates/api/src/image.rs`: `ImageSpec.credentials`; `request()` refuses an empty reference and any host that is not a
  DNS name (shards_dns' RFC 1035/1123 rules) or `host.docker.internal`: never a port, never `localhost`, and never an
  address, since TLS names a server by its DNS name alone (RFC 6066 §3) and a grant that could never be filled is
  refused, not kept silently. Hosts are stored lowered.
- `crates/api/Cargo.toml`: `shards-dns` (a leaf crate, no cycle).
- `docs/design/architecture.md`: D242, with the shape, gateway-not-proxy, the SNI=Host=grant match, default deny, the
  host vocabulary, who a grant is for (the run's; a placeholder is the capability), the leases' identity and moves
  (ROOT's decision, with D238), and the literals' sources.
- Test constructors given `credentials: Vec::new()`: `crates/shards/src/api_run.rs` (tests),
  `crates/shards/tests/api_image.rs`, `crates/shards/tests/nesting.rs`.

## Tests (one host: macOS arm64, adas-macbook-pro-2, Darwin 25.4.0)

- `cargo test -p shards-api --profile test-release`: 91 lib tests pass (5 ignored), 17 integration tests pass
  (2 ignored), before and after each mutation below. The contract's: `image::wire::tests::decide_fills_only_on_an_
  agreed_granted_host`, `a_value_never_prints`, `messages_round_trip`, `unknown_versions_and_tags_are_refused`;
  `image::tests::a_credential_grant_rides_by_reference`, `a_spec_is_the_request_the_host_reads`.
- R10 guard removals, each failing its test (run before the person's 2026-11-13 rule ended mutation runs):
  - (a) `decide` without the SNI != Host check: `decide_fills_only_on_an_agreed_granted_host` fails;
  - (b) `decide` filling whatever the grant's host: the same test fails;
  - (c) `request()` without the host-vocabulary refusal: `a_credential_grant_rides_by_reference` fails
    (`api.anthropic.com:443 was accepted`);
  - (d) `request()` without the empty-reference refusal: the same test fails;
  - (e) `request()` taking an IPv4 address again: the same test fails (`10.0.0.5 was accepted`).
  (a)-(d) ran first on adalundhe-macbook-pro (kiln run r1078) on the code before the address refusal; (c), (d), (e)
  ran on the final code on this Mac.
- `SHARDS_HELPERS=skip cargo clippy -p shards-api -p shards --all-targets --target aarch64-apple-darwin -- -D warnings`:
  clean. `cargo fmt --all --check`: clean. `scripts/check-development-policy`: passed.
- The flake STEP1's first author saw in `client::tests::gpu_probe_protocol_requires_exact_completed_receipt` did not
  show in these runs (main's 7cd1ac183, PM M534, changed that test).

## Owed

- The other targets' clippy and tests: CI on main runs the matrix (the person, 2026-11-13); ROOT watches it.
- No real-VM run belongs to this slice. Real-VM validation of the whole shape is the plan's step 10, ROOT's.
