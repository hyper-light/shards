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

## Owed

- Slice 2 (Shards): the NET kinds, the image channel's next version, `Lessor` grown append-only, the injector wired, the
  light test with a simulated token endpoint.
- Slice 3 (kiln): `RunLessor` answering capture, refresh and revoke from the broker.
- ROOT: a real `claude auth login` and `codex login --device-auth` in a microVM through the injector, each surviving a
  refresh answered outside; a real request with the captured token filled in at egress; the `scope` a claude.ai
  sign-in's token response carries (whether `org:create_api_key` is granted).
