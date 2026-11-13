# Step 33, step 8: sign-in and refresh through the injector (Claude Code and Codex profiles)

Kiln goal #70 (id 8f474b45c7cd86559e0566f22f9d0c33), plan step 33 of kiln plan 01a1cd8e862e7169a074db3d5fa33f3d; step 8 of
the goal's plan 01a1d2b88be57304b972761e2a5c6781 (research 01a1d2b88be57304bf6b081fe46832e0: f16, f43, f47, f49, f50,
f52-f55, f63, f64, f74). Role: implementer. Policy read at origin/main 8cc1f6673: AGENTS.md sha256 f66d304c…,
docs/development-rules.md df2acf47…, docs/development-evidence.md 61613a24…. Worktree `s33-signin`, branch
`p/s33-signin-v1`, from main 31a6c5b40, fast-forwarded to 8cc1f6673 before any edit. The design and its decisions are
D242's "Built: step 8"; this file is the light-test and evidence record.

## The pinned builds' own words (read on adacachy2, Linux x86_64, 2026-11-13; never run)

`scripts/step8-evidence.sh` and `scripts/step8-evidence2.sh` (run from a carrier, outputs under
`~/g121-tmp/s33-signin/evidence/out` on the runner) fetch each build by its kiln pin, hold it to the pin, and grep it;
rerun, they print every line quoted below.

- Claude Code 2.1.311, linux-x64 (gnu): `downloads.claude.ai/claude-code-releases/2.1.311/linux-x64/claude`, sha256
  86dc1fcc69498159eaf15f446613e847010f9036bfbbdffd4c49350be8489ada (kiln's `harness-claude/pin.json`), 293776752
  bytes, held. Offsets are into that file (its app payload begins at 115363840).
- Codex rust-v0.167.2: `git clone --depth 1 --branch rust-v0.167.2 https://github.com/openai/codex.git`, HEAD
  ce21072c90d32c40af7a55b60b2fb35c84a43a6a (the tag's commit); ten cited files held to the sha256s in
  `evidence/codex/INDEX.md` (server.rs, auth/manager.rs, auth/revoke.rs, oauth/client.rs, device_code_auth.rs,
  token_data.rs, auth/storage.rs, cli/src/login.rs, model-provider/src/bearer_auth_provider.rs,
  auth/agent_identity.rs): all OK.

### Claude Code's sign-in, refresh and revocation (f63, f64)

- 238938203: `TOKEN_URL:"https://platform.claude.com/v1/oauth/token"`,
  `API_KEY_URL:"https://api.anthropic.com/api/oauth/claude_cli/create_api_key"`.
- 242102662 (`c9o`, the code exchange): `w={grant_type:"authorization_code",code:e,redirect_uri:…,client_id:…,
  code_verifier:r,state:n}` … `It.post(on().TOKEN_URL,w,{headers:{"Content-Type":"application/json"}…`, returning
  `C.data`; 243341777 (`formatTokens`) reads `access_token`, `refresh_token`, `expires_in`, `refresh_token_expires_in`,
  `scope`, `account`, `organization`, `workspace`: no `id_token`.
- 242103577 (the refresh): `P={grant_type:"refresh_token",refresh_token:e,client_id:…,scope:x.join(" ")}` …
  `{access_token:G,refresh_token:j=e,expires_in:Q}=H` (a refresh token kept where none comes).
- 242106079 (`i3r`, the revocation): ``It.post(`${on().TOKEN_URL}/revoke`,{token:e,token_type_hint:"refresh_token",
  client_id:…}`` … `continuing with local logout.` Callers: logout (`Ib`), and a refused sign-in's token
  (266762765: "Revoking a refused sign-in's token threw").
- 238937512 and 238937878: the scopes asked, `deo()` = `["org:create_api_key","user:profile", …Sxt()]`, `Sxt()` =
  `user:profile`, `user:inference`, `user:sessions:claude_code`, `user:mcp_servers`, `user:file_upload` (and plugins):
  no `openid`.
- 242107148 (`u9o`): `It.post(on().API_KEY_URL,null,{headers:{Authorization:`Bearer ${e}`…}}),s=r.data?.raw_key`;
  266757798: `if(CB(e.scopes))await y(r,n)…;else if(!await u9o(e.accessToken,r))throw Error("Unable to create API
  key…")`: the CLI mints a key only where its token lacks `user:inference` (the console's sign-in).
- Its token's format is checked nowhere on the sign-in path: `sk-ant-oat` appears 3 times, for
  `CLAUDE_CODE_OAUTH_TOKEN` (242681357), a WIF string (123130373) and the LLM gateway (268320008).

### Question 5: at_hash (f50)

- The 7 `at_hash` strings are panva's openid-client (its own package.json at 267668073:
  `homepage:"https://github.com/panva/openid-client"`): 267701855, `validateIdToken`:
  `if(!f.at_hash&&e.access_token)throw new We({message:"missing required property at_hash"…` and
  `Ol.validate({claim:"at_hash",source:"access_token"},f.at_hash,e.access_token,…)`.
- Its callers are the LLM gateway's OIDC sign-in (269041797: `oidc.scopes must include 'openid'`, 269168935:
  `oidcConfigured:n.oidc!==void 0`) and XAA's IdP login for MCP servers (129450115: `XAA IdP: token response missing
  id_token (check scope=openid)`); MSAL (261720944) is Azure's.
- Anthropic's sign-in asks no `openid` scope and reads no `id_token` (above), so no ID token is checked there. Codex
  0.167.2 has no `at_hash` (`git grep -n at_hash`: abseil's `flat_hash_map` alone).

### Question 4: Codex's ID token is a credential

- server.rs:446-450: `// Obtain API key via token-exchange and persist` / `obtain_api_key(&client, &opts.issuer,
  &opts.client_id, &tokens.id_token)` … `.ok()`; server.rs:1036-1069: `grant_type=urn:ietf:params:oauth:grant-type:
  token-exchange`, `requested_token=openai-api-key`, `subject_token=<id_token>`,
  `subject_token_type=urn:ietf:params:oauth:token-type:id_token`, answered `access_token` (an API key);
  login_server_e2e.rs:227-229: "the old oauth flow that exchanges tokens for an API key".
- The device flow (kiln's, `codex login --device-auth`) does not ask it: device_code_auth.rs:226
  `persist_tokens_async(&opts, /*api_key*/ None, &tokens)`. The exchange is the issuer's, open to whoever holds an
  unexpired ID token.
- Codex reads an ID token's payload alone: token_data.rs:144-155 (`decode_jwt_payload`: three parts, none empty, the
  payload base64url-decoded, the signature never read); server.rs:859 and manager.rs:1758-1759 parse it so.
- Its access token: `parse_jwt_expiration(&tokens.access_token)` (manager.rs:3536) for a proactive refresh, which
  falls back to `last_refresh` (8 days) and the 401 path where it is no JWT (3543-3545).

### Codex's refresh and revocation (f53, f55)

- oauth/client.rs:15 "ChatGPT refresh uses JSON; authorization-code and gateway grants use form encoding."; 97-113
  (`grant_type`, `client_id`, `refresh_token`, `resource`, `refresh_reason`), 124-135 (JSON as a sorted map).
- manager.rs:1855-1860 `RefreshResponse { id_token, access_token, refresh_token }`, each optional; 1758-1766 each kept
  where it comes.
- auth/revoke.rs:47-53, 68-77, 104-117: JSON `{token, token_type_hint, client_id?}` to
  `https://auth.openai.com/oauth/revoke` (manager.rs:231), the refresh token else the access token, success any 2xx.

### Question 7: sender-constrained tokens (f49)

- Neither provider's sign-in: Claude Code sends `Authorization: Bearer ${r.accessToken}` (e_q5_wire.txt, 242399004)
  and no DPoP header on its exchange or refresh; Codex has no `DPoP` word (`git grep -n -w -i dpop -- codex-rs`: none).
- Claude Code's Bedrock credentials through `aws login`: the AWS SDK's `fromLoginCredentials` refreshes with a DPoP proof
  (261406663: `this.createDPoPInterceptor(g.middlewareStack);let l={tokenInput:{clientId:…,refreshToken:…,
  grantType:"refresh_token"}}`; 261409863: `headers:{…,DPoP:n}`). Fallback.

## Slice 1: the profiles (`crates/net/src/signin.rs`)

### Change

- `crates/net/src/signin.rs` (new): `Cli`, `Profile`, `Endpoint`, `CLAUDE_CODE`, `CODEX`; `ask` (a refresh's or a
  revocation's placeholder found in its request: `Ask::Refresh`, `Ask::Revoke`, `Ask::Pass`); `read` and `write` (a
  token response's tokens read where they lie and given placeholders, the ID token made inert); `readable`, `frame`
  (the framing recomputed); `refreshed` and `REVOKED` (the answers given outside); `Profile::mints`.
- `crates/net/src/inject.rs`: `Field`, `fields`, `values`, `list`, `failure`, `step` crate-visible (shared, unchanged).
- `crates/net/src/lib.rs`: `pub mod signin`.

### Tests (one host: adas-mac-studio.local, macOS arm64; kiln runs through the carrier
`.claude/worktrees/s33-signin-carrier` (`run.sh`: a blobless clone, this branch's bundle over origin/main),
`CARGO_TARGET_DIR=~/g121-tmp/s33-signin/target`, `SHARDS_HELPERS=skip`, `CARGO_INCREMENTAL=0`; each run's whole output
in `~/g121-tmp/s33-signin/last.log`, read with `grep -a`)

- Run r611, head 8d4b53874 (cargo 1.98.1):
  - `cargo test -p shards-net --lib signin`: 17 passed, 88 filtered out.
  - `cargo clippy -p shards-net --all-targets -- -D warnings`: clean.

### R10: each guard's red, shown once (by hand: `scripts/step8-reds.py`, from the repository root)

Run r617 (adas-mac-studio.local, head 8d4b53874, `run.sh p/s33-signin-v1 python3
docs/research/s33-veil-creds/scripts/step8-reds.py`): 14 of 14 red, each its own test failing at its assertion, the
file restored after each (exit 101 each; 39 s).

| Guard removed | Its test, failing |
|---|---|
| a token's name nested or twice refused | `a_token_anywhere_but_its_place_is_refused` (signin.rs:1070) |
| a token's value echoed elsewhere refused | `a_tokens_value_echoed_elsewhere_is_refused_escaped_or_not` (1084) |
| an echo sought with its escapes decoded | the same (1084: the `\u0073` echo) |
| the access token given its placeholder (the real token through) | `a_claude_code_token_response_is_given_placeholders_and_nothing_else_changes` (998) |
| the ID token made inert (the real ID token through) | `a_codex_token_response_keeps_its_id_tokens_claims_and_loses_its_signature` (1008) |
| an ID token that is no JWT refused | `an_id_token_that_is_no_jwt_is_refused` (1032) |
| a placeholder a JSON string holds as it is | `a_placeholder_a_json_string_cannot_hold_as_it_is_is_refused` (1112) |
| a rewritten response's framing recomputed | `a_rewritten_responses_framing_is_recomputed` (1123) |
| a coded body is not read | `a_coded_body_is_not_read` (1152) |
| a body that is no JSON text refused | `a_response_without_tokens_passes_and_one_that_cannot_be_read_is_refused` (1061) |
| a refresh answered outside for its own placeholder alone | `a_request_that_is_no_refresh_of_a_placeholder_passes` (1242) |
| a request naming a field twice passes | the same (1239) |
| a minting path read widely | `a_sign_ins_token_is_never_filled_toward_an_endpoint_that_mints_a_key` (1280) |
| a placeholder never prints | `no_token_or_placeholder_prints` (1322) |

## R01-R16 (slice 1)

- R01: no unwrap, expect, panic, indexing or unchecked arithmetic on input sizes in signin.rs (clippy's workspace
  denials clean); JSON nesting followed by a stack of its own, never recursion (a 100 000-deep array read).
- R02: the shapes are the pinned builds' (above); the JSON reader is the network process's own because its tree has no
  JSON parser (`cargo tree -p shards-net -e normal --target all`: none) and serde_json's `Value` would drop a token's
  copies unwiped; considered and refused in D242.
- R03: no numeric literal of its own: RFC 8259's grammar, RFC 6749 §5.1's fields, RFC 7009 §2.2's 200, and the
  connection's window (the caller's) bound what is read.
- R04: `Ask`, `Refusal` typed; the injector renders them (slice 2).
- R05: no cache; it holds a response only while its caller does.
- R07: comments cite the builds and RFCs.
- R09: the CLIs' shapes are their pinned builds' own (no CLI run, no real sign-in); a real sign-in surviving a refresh
  is ROOT's (owed).
- R10: the reds above; evidence scripts committed.
- R11/R12: platform-independent code; the head and KMP helpers shared with inject.rs.
- R13: one module, pure functions.
- R14, R15, R16: no profiling, campaigns or runner cleanup owed by this slice beyond deleting the target dir.
- R06/R08: no complexity hot spot; no performance claim.

## Slice 2a: the injector takes a sign-in's tokens outside and answers its refresh and revocation there

### Change

- `crates/dns/src/credential.rs`: `Cli`, `SignInGrant`, `Captured` (their `Debug` shows no placeholder), and
  `check_sign_ins` (a run's grants and its sign-ins' placeholders checked as one).
- `crates/ipc/src/lib.rs`: `NET_SIGN_INS` (104) and `NET_SIGN_IN` (105), after main's highest (103).
- `crates/net/src/signin.rs`: `NET_SIGN_INS`' codec (`encode_grants`, `decode_grants`: one sign-in of a CLI a run);
  `NET_SIGN_IN`'s (`Told`: a capture's tokens, a refresh, a revocation; `Answered`, `Declined`); `SignIns`, the run's
  sign-ins and their asks (one refresh ask for every flow of a sign-in); `append` (a growing buffer whose old copy is
  wiped).
- `crates/net/src/inject.rs`: TLS ended at a sign-in's token host and the hosts its access token goes to; at its token
  host no value filled, `Accept-Encoding: identity` asked, a POST to its token or revocation path read whole and
  answered outside where it carries the sign-in's placeholder (`Request::Refreshing`), every final response with a body
  read whole (`Response::Whole`) and, where it holds tokens, held until the run host answers their capture
  (`Response::Capturing`), then written with the placeholders and framed anew; `Refusal::Mints` (403),
  `Refusal::SignIn` (502), `Refusal::Declined` (`invalid_grant` 400 for a refresh its broker refused, else 502 or
  503); `Audit` says what it did (`Did`).
- `crates/net/src/lib.rs`: `Credentials::sign_ins`; `NET_SIGN_INS` taken before `NET_CREDENTIALS`, whose CA names the
  sign-ins' hosts; `NET_SIGN_IN`'s answers followed (`follow`: a capture's access grant in place of the one it
  replaced, a rotated token's held lease let go); its asks sent as leases' are, as they lie (`tell`).
- `crates/net/src/lease.rs`: `Leases::grant`, `Leases::forget`.

### Tests (adas-mac-studio.local, macOS arm64, cargo 1.98.1; the carrier as above)

- Run (head ceeab486b, after r676's compile errors, `REFRESH` named twice, were fixed):
  - `cargo test -p shards-dns --lib credential`: 5 passed (r676).
  - `cargo test -p shards-ipc --lib every_message_kind_has_its_own_number`: 1 passed (r676).
  - `cargo test -p shards-net --lib -- signin inject:: lease:: credential`: 46 passed, among them the plan's light test
    (`inject::tests::a_sign_in_is_taken_outside_and_its_refresh_and_revocation_answered_there`,
    `refreshes_of_one_sign_in_at_once_ask_once`, `a_sign_ins_token_never_goes_to_the_endpoint_that_mints_a_key`): a
    simulated token endpoint behind the injector and a test lessor standing for the run host and its broker.
  - `cargo clippy -p shards-dns -p shards-ipc -p shards-net --all-targets -- -D warnings`: clean.

### R10: each guard's red (`scripts/step8-reds.py 2a`)

Run r686 (head 56204ded6): 13 of 14 red, each its own test failing at its assertion; the fourteenth (single flight)
did not compile as written (its `None` arms left the type open, so it was no red), was rewritten to
`Asking::Refresh { waiting, .. } if false => Some(waiting)` and run alone (head 5124309b4): red. 14 of 14.

| Guard removed | Its test, failing |
|---|---|
| a sign-in's response read whole before any of it goes (the real tokens through) | `inject::tests::a_sign_in_is_taken_outside_and_its_refresh_and_revocation_answered_there` (inject.rs:3190) |
| a refresh answered outside, never sent on | the same (3242) |
| a revocation answered here, never sent on | the same (3272) |
| a refresh's answer given placeholders (the rotated tokens through) | the same (3245) |
| a sign-in's request asks identity coding | the same (3189) |
| no value toward an endpoint that mints a key | `a_sign_ins_token_never_goes_to_the_endpoint_that_mints_a_key` (3131: no 403, at its deadline) |
| a capture grants its access token | `a_sign_in_is_taken_outside_and_its_refresh_and_revocation_answered_there` (3217) |
| a refreshed token's held lease let go | the same (3255) |
| a rotated value's ask under way kept | `lease::tests::a_captured_credential_is_granted_and_a_rotated_ones_held_lease_let_go` (lease.rs:721) |
| refreshes of one sign-in ask once | `inject::tests::refreshes_of_one_sign_in_at_once_ask_once` (inject.rs:3335) |
| one sign-in of a CLI a run | `signin::tests::sign_in_grants_ride_their_message_and_a_cli_has_one` (signin.rs:2127) |
| a capture's placeholders checked | `signin::tests::a_captures_placeholders_that_are_no_tokens_or_anothers_are_declined` (2304) |
| a refresh's answer never prints its tokens | `signin::tests::a_sign_ins_words_round_trip_and_print_no_token` (2189) |
| a sign-in's refresh placeholder one credential's alone | `credential::tests::a_sign_ins_placeholders_are_tokens_and_one_credentials_alone` (credential.rs:442) |

## Slice 2b: the run host's relay (#74 step 8 slice 2b)

Role: implementer, for ROOT (g121). Policy read at origin/main ff08eca25: AGENTS.md sha256 f66d304c…,
docs/development-rules.md df2acf47…, docs/development-evidence.md 61613a24…. Worktree `s33-signin2b`, branch
`p/s33-signin2b-v1` from main ff08eca25; it builds on the previous implementer's partial start (the wire messages, the
`Lessor` defaults and the session's handlers, untested), taken and finished. The design is D242's "Built: step 8's
second slice, its second part".

### Change

- `crates/api/src/image/wire.rs`: version 24, `Request::sign_ins` appended; `SignInTokens` (with the inert ID token and
  `scope`), `CaptureAsk`, `RefreshAsk`, `Revoked`, `CaptureAnswer`, `RefreshAnswer`, `SignInRefusal`; kinds
  `CAPTURED` 34, `REFRESHED` 35 (to the host), `CAPTURE` 36, `REFRESH` 37, `REVOKE` 38 (to the client). The end-relative
  offsets of older tests moved past the new count, and a version 23 request is refused.
- `crates/api/src/image.rs`: `ImageSpec::sign_ins`, checked as the request is built (`sign_ins`); `Lessor` grown with
  `capture`, `refresh` and `revoked`, each defaulted (`Unanswered`).
- `crates/api/src/image/session.rs`: the three words put to the lessor on threads of their own; the captured access
  token leasable (`granted`); a refresh releasing the leases of it given before it (`given_of`, `rotated`); a
  `CAPTURE` frame wiped once taken.
- `crates/shards/src/api_run.rs`: `sign_ins` (checked against the egress, encoded for `NET_SIGN_INS`); `tell_net`
  sending it before `NET_CREDENTIALS`, which goes for a run with sign-ins and no grant too; `trust` naming the sign-ins'
  hosts; the ledger's sign-ins (`Signing`, `Ledger::sign_in`, `signed_in`, `captured`, `refreshed`); the relay of
  `NET_SIGN_IN` (`sign_in_told`) and of the answers (`captured`, `refreshed`); answers written through the process's own
  stdout descriptor in one vectored write (`stdout`, `Answers::send`).
- `crates/net/src/signin.rs`, `inject.rs`, `crates/dns/src/credential.rs` (ROOT's asks): `Captured::refresh_placeholder`
  and `Placeholders::refresh` optional, a refresh token with no placeholder refused in `write`; `Tokens::scope`;
  `Taken::id_token` (inert) and `Taken::scope`, carried by `Told::Capture`; `Taken::of` typed (`NoToken`, `NotAJwt`).

### Tests (adas-mac-studio.local, macOS arm64, cargo 1.98.1; carrier `.claude/worktrees/s33-signin2b-carrier`,
`CARGO_TARGET_DIR=~/g121-tmp/s33-signin2b/target`, `SHARDS_HELPERS=skip`, `CARGO_INCREMENTAL=0`)

- Run r825 (head e33c6aea, the tree landed but for formatting and three test-only lint fixes):
  - `cargo test -p shards-dns -p shards-net -p shards-api --lib -- credential signin inject:: lease:: image::`: 5, 47
    and 62 passed.
  - `cargo test -p shards --bin shards -- api_run::`: 45 passed, among them
    `a_run_whose_request_grants_a_sign_in_tells_its_network_process_and_one_that_grants_none_tells_none`,
    `a_sign_in_is_relayed_its_placeholders_kept_through_a_refresh_and_its_lease_released_at_the_runs_end` and
    `a_runs_ledger_relays_its_own_sign_ins_and_lets_a_refreshed_lease_go`.
- Run r840 (head a6d11fbb5, the three test-only lint findings of r825 fixed): `cargo clippy -p shards-dns -p shards-net
  -p shards-api -p shards --all-targets -- -D warnings` clean on aarch64-apple-darwin, on x86_64-unknown-linux-gnu (zig
  CC, as `scripts/lint` sets it) and on x86_64-pc-windows-msvc (`cargo xwin`); `image::session` 24 passed.
- Run r858 (head 9539320cc, the relay test given a direct assertion of its lease's drop): `api_run::` 45 passed.
- Run r874 (head 2fb3a0c23, the slice merged with main ed1136305, whose commits touched crates/net and D242): the same
  tests, shards-api 64, shards-dns 5, shards-net 47 (2 ignored: step 10's A/B harness) and `api_run::` 45 passed; the
  four crates' clippy clean on all three targets.

### R10: each guard's red (`scripts/step8-reds.py 2b`)

Run r847 (head ca5e5cf17, `run.sh p/s33-signin2b-v1 python3 docs/research/s33-veil-creds/scripts/step8-reds.py 2b`):
9 of 10 red, each its own test failing at its assertion, the file restored after each. The tenth (the relay letting a
refreshed token's held lease go) was held only by the relay test, which with the guard gone would wait on a lease ask
that never comes rather than fail: the test now asserts the ledger's drop directly, and run r858 (head 9539320cc) shows
it red, with the 2a guard of the same name still red. 10 of 10. Slice 1's and 2a's guard texts that this slice moved
(the optional refresh placeholder) are updated in the script.

| Guard removed | Its test, failing |
|---|---|
| a run's sign-ins told its network process | `api_run::tests::a_run_whose_request_grants_a_sign_in_tells_its_network_process_and_one_that_grants_none_tells_none` |
| a sign-in its egress cannot reach refused | the same |
| a refreshed token's held lease let go (the run host) | `api_run::tests::a_sign_in_is_relayed_its_placeholders_kept_through_a_refresh_and_its_lease_released_at_the_runs_end` |
| a capture's access token granted (the run host) | `api_run::tests::a_runs_ledger_relays_its_own_sign_ins_and_lets_a_refreshed_lease_go` |
| an ask after the run's release declined | the same |
| a captured access token leasable (the session) | `image::session::tests::a_sign_ins_capture_and_refresh_are_kept_by_the_broker_and_its_lease_released_at_the_runs_end` |
| a lease given before a refresh released (Q2) | the same |
| a sign-in's CLI no version has refused | `image::wire::tests::a_runs_sign_ins_and_their_words_cross_the_channel` |
| a refresh token with no placeholder never reaches the guest | `signin::tests::a_capture_carries_its_inert_id_token_and_scope_and_a_sign_in_may_hold_no_refresh_placeholder` |
| the capture's ID token inert | the same |

## R01-R16 (slice 2b)

- R01: no unwrap, expect, indexing or unchecked arithmetic in the new production code; `IoSlice::advance_slices` is
  given at most what `write_vectored` wrote (its precondition). Tests alone assert.
- R02: the relay rides step 4's path (D242), the words its channel's existing conventions; no new mechanism.
- R03: no numeric literal of its own: sequence numbers and wire tags; the channel's and `shards_ipc`'s bounds.
- R04: `Signing`, `SignInRefusal` (to `signin::Declined`, each cause kept), the ledger's states; an answer to no ask, of
  another kind, or a reused id breaks the contract, typed.
- R05: no cache; the ledger is bounded by the run's sign-ins, one ask of each kind under way.
- R06/R07/R13: focused functions; comments say why (Q1, Q2, wiping).
- R08/R14: no performance claim; one vectored write per frame replaces std's buffered writes (unmeasured: ROOT's).
- R09: no defect claimed; the gotcha (a refresh's lease refused `Live`) is held by a unit red.
- R10: the reds above.
- R11/R12: platform-independent code; the raw stdout is the twin of `stdin()` on Unix and Windows; clippy on three
  targets.
- R15: implementer scope only; R16: the target dir on the runner removed after landing.

## Owed

- Slice 3 (kiln): `RunLessor` answering capture, refresh and revoke from the broker.
- ROOT: a real `claude auth login` and `codex login --device-auth` in a microVM through the injector, each surviving a
  refresh answered outside; a real request with the captured token filled in at egress; the `scope` a claude.ai
  sign-in's token response carries (whether `org:create_api_key` is granted).
