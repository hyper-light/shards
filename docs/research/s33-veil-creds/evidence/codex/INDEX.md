# s33 Codex evidence index

POLICY_READ revision=99d88fccf6107d1929c1115eb220a732b182fd37 sha256=df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3 role=researcher(read-only) task=s33-codex owned=docs/research/s33-veil-creds/evidence/codex/

Collected 2026-11-12 (UTC) by a read-only research subagent. Nothing here was built or
executed; no credential (auth.json, keychain, environment token) was read.

## Source at the pinned tag

- Repository: https://github.com/openai/codex, tag `rust-v0.167.2`
- Tag object: `08a3b12b3298577534a28b62ed976acf8de66eda` (annotated); peeled commit:
  `ce21072c90d32c40af7a55b60b2fb35c84a43a6a` (commit date 2026-11-11 12:03:56 -0800)
- Fetch: `git -c core.hooksPath=/dev/null clone --depth 1 --branch rust-v0.167.2 https://github.com/openai/codex.git src`
  then `git -C src rev-parse HEAD` = `ce21072c90d32c40af7a55b60b2fb35c84a43a6a`.
- `src/` is a nested git clone (about 142 MB). Do not `git add` it wholesale; every cited file is
  listed below with its raw URL at the tag and its SHA-256, so it can be regenerated.
- Spot check that the clone equals raw.githubusercontent.com at the tag (SHA-256 identical):
  `codex-rs/login/src/auth/manager.rs`, `codex-rs/http-client/src/custom_ca.rs`,
  `codex-rs/app-server-protocol/src/protocol/v2/account.rs`, `docs/authentication.md`.
- `src/codex-rs/Cargo.lock` SHA-256 `9e71c74824a7ea114d0a3a4c94e93d81a1ddd13ca8fa6f698cbd1284c2e9ff2f`.

## Upstream crates (versions pinned by Codex's Cargo.lock)

Each `.crate` was downloaded from static.crates.io into `crates/<name>-<version>/dl/` and its
SHA-256 equals the `checksum` line in `src/codex-rs/Cargo.lock` (line given). Extracted
(untrusted, read only) under `crates/<name>-<version>/x/`; openssl-src: only `src/lib.rs` and
`Cargo.toml` extracted.

| crate | Cargo.lock checksum line | .crate SHA-256 |
|---|---|---|
| reqwest 0.12.28 | 13102 | `eddd3ca559203180a307f12d114c268abf583f59b03cb906fd0b3ff8646c1147` |
| native-tls 0.2.14 | 10549 | `87de3442987e9dbec73158d5c715e7ad9072fda936bb03d19d7fa10e00520f0e` |
| openssl-probe 0.1.6 (used by native-tls) | 11240 | `d05e27ee213611ffe7d6348b942e8f942b37114c00cc03cec254295a4a17852e` |
| openssl-probe 0.2.1 (used by rustls-native-certs) | 11246 | `7c87def4c32ab89d880effc9e097653c8da5d6ef28e6b539d313baaacfbafcbe` |
| rustls-native-certs 0.8.3 | 13716 | `612460d5f7bea540c490b2b6395d8e34a953e52b491accd6c86c8164c5932a63` |
| openssl-src 300.6.1+3.6.3 | 11252 | `46eb8fb9fb3b61ce1c0f8a026c4c1a0714d3a9e138e7fbde78753ce2babc3846` |
| openssl 0.10.75 | 11214 | `08838db121398ad17ab8531ce9de97b244589089e290a384c900cb9ff7434328` |
| openssl-sys 0.9.111 | 11261 | `82cab2d520aa75e3c58898289429321eb788c3106963d0dc886ec7a5f4adc321` |
| tempfile 3.27.0 | 15388 | `32497e9a4c7b38532efcdebeef879707aa9f794296a4f0244f6f69e9bc8574bd` |

## Documentation saved

- `docs/learn.chatgpt.com_docs_auth.md`: requested https://developers.openai.com/codex/auth.md;
  it redirected to https://learn.chatgpt.com/docs/auth.md, which answered HTTP 200 text/markdown.
- `docs/learn.chatgpt.com_docs_auth.html`: the HTML page (https://developers.openai.com/codex/auth
  redirected to https://learn.chatgpt.com/docs/auth), SHA-256
  `c89e32cfd95a9dc4517327484f1c421b7386cf03ebcd45437518aa68afe2273f`; not quoted.
- `docs/learn.chatgpt.com_docs_auth_ci-cd-auth.md`: https://learn.chatgpt.com/docs/auth/ci-cd-auth.md.
- `docs/openssl-3.6.4_SSL_CTX_load_verify_locations.pod`: OpenSSL's own man page source at tag
  `openssl-3.6.4` (the OpenSSL version Codex's musl release build compiles).
- Codex's own docs at the tag (in the clone): `src/docs/authentication.md`, `src/docs/config.md`
  (both are pointers to developers.openai.com).

## Quote verification

- `tools/quotes.txt`: one record per quote, `ID@@@file@@@line@@@quote` (file relative to this
  directory). Each quote is an exact contiguous substring of the one cited line.
- `tools/verify-output.txt`: result per quote (`OK`), with all matching line numbers from
  `grep -F -n -- QUOTE FILE`. 266 quotes, 0 failures. Procedure for each record:
  `grep -F -n -- "$quote" "$file"` must match, and `sed -n "${line}p" "$file" | grep -F -q -- "$quote"`
  must succeed.
- `tools/absence-checks.txt`: commands and outputs for claims of absence (no certificate pinning
  or custom verifier in non-test code; no workspace manifest enables reqwest `native-tls-alpn` or
  `http3`; refresh parameters carry no `scope`).

## Cited files

| source type | file (relative) | URL at pin | SHA-256 |
|---|---|---|---|
| upstream-crate | `crates/native-tls-0.2.14/x/native-tls-0.2.14/src/imp/openssl.rs` | https://static.crates.io/crates/native-tls/native-tls-0.2.14.crate (member native-tls-0.2.14/src/imp/openssl.rs) | `2792ea2b87385c26f7386617499bd602943bf0fb94f5e28a457826e9808cb04b` |
| upstream-crate | `crates/openssl-0.10.75/x/openssl-0.10.75/src/ssl/connector.rs` | https://static.crates.io/crates/openssl/openssl-0.10.75.crate (member openssl-0.10.75/src/ssl/connector.rs) | `aff992017be573af56151ffc5b36fd0369435f5dd27067b800d488dfd631cb79` |
| upstream-crate | `crates/openssl-probe-0.1.6/x/openssl-probe-0.1.6/src/lib.rs` | https://static.crates.io/crates/openssl-probe/openssl-probe-0.1.6.crate (member openssl-probe-0.1.6/src/lib.rs) | `adc9b41353d31a78eb519bd8babefa2a4ed883e5aaf2e36d881ddb321aa82675` |
| upstream-crate | `crates/openssl-probe-0.2.1/x/openssl-probe-0.2.1/src/lib.rs` | https://static.crates.io/crates/openssl-probe/openssl-probe-0.2.1.crate (member openssl-probe-0.2.1/src/lib.rs) | `84ec9c4dfb24f51bb7e6bd17ac574dc4861034dadbfb3d298eac3a17a54ed396` |
| upstream-crate | `crates/openssl-src-300.6.1+3.6.3/x/openssl-src-300.6.1+3.6.3/src/lib.rs` | https://static.crates.io/crates/openssl-src/openssl-src-300.6.1%2B3.6.3.crate (member openssl-src-300.6.1+3.6.3/src/lib.rs) | `c0f7ee0384ee12835abc6197122dc60ac75e33a3906dfe90bc378e62df56839d` |
| upstream-crate | `crates/openssl-sys-0.9.111/x/openssl-sys-0.9.111/build/main.rs` | https://static.crates.io/crates/openssl-sys/openssl-sys-0.9.111.crate (member openssl-sys-0.9.111/build/main.rs) | `d17fa8715a43ff0da872ac504abb2cc0a395644e8f5c8c128812758c458d5963` |
| upstream-crate | `crates/reqwest-0.12.28/x/reqwest-0.12.28/Cargo.toml` | https://static.crates.io/crates/reqwest/reqwest-0.12.28.crate (member reqwest-0.12.28/Cargo.toml) | `23c8a0827aaac74059965a182c6873990e75aa550e8079d3b51bb95092871205` |
| upstream-crate | `crates/reqwest-0.12.28/x/reqwest-0.12.28/src/async_impl/client.rs` | https://static.crates.io/crates/reqwest/reqwest-0.12.28.crate (member reqwest-0.12.28/src/async_impl/client.rs) | `8c30b009838be2126b90344ef97fb6141ac8bdf695f4e7bb00a3052a707d9b5c` |
| upstream-crate | `crates/reqwest-0.12.28/x/reqwest-0.12.28/src/tls.rs` | https://static.crates.io/crates/reqwest/reqwest-0.12.28.crate (member reqwest-0.12.28/src/tls.rs) | `73afedbf9e144a8c66dad7706bc37c6a73ff185eff1ee8654d158e57a0a8cbd6` |
| upstream-crate | `crates/rustls-native-certs-0.8.3/x/rustls-native-certs-0.8.3/src/lib.rs` | https://static.crates.io/crates/rustls-native-certs/rustls-native-certs-0.8.3.crate (member rustls-native-certs-0.8.3/src/lib.rs) | `e96b60d226d77599e68fdbb180335866d1f2a99301cfdfb12ed3f6e207a46af0` |
| upstream-crate | `crates/rustls-native-certs-0.8.3/x/rustls-native-certs-0.8.3/src/unix.rs` | https://static.crates.io/crates/rustls-native-certs/rustls-native-certs-0.8.3.crate (member rustls-native-certs-0.8.3/src/unix.rs) | `81249d335b14e6fdff360146627a9c5d37635e738492e004a967df12df5d5163` |
| upstream-crate | `crates/tempfile-3.27.0/x/tempfile-3.27.0/src/file/imp/unix.rs` | https://static.crates.io/crates/tempfile/tempfile-3.27.0.crate (member tempfile-3.27.0/src/file/imp/unix.rs) | `cc39f94cb6cb87b36cf7603a75429d64ecc4ad1a2076a765e5aba2b11771d0f7` |
| docs | `docs/learn.chatgpt.com_docs_auth.md` | https://learn.chatgpt.com/docs/auth.md (requested https://developers.openai.com/codex/auth.md, HTTP redirect) | `67d3f0c64897ea26b48e62a634ced7dbd2763a47104e12a0923cc528986de588` |
| docs | `docs/learn.chatgpt.com_docs_auth_ci-cd-auth.md` | https://learn.chatgpt.com/docs/auth/ci-cd-auth.md | `16b17c03ed601be6567893c244debdc8eff09de3aa8838b3e307a6b73a65b796` |
| upstream-docs | `docs/openssl-3.6.4_SSL_CTX_load_verify_locations.pod` | https://raw.githubusercontent.com/openssl/openssl/openssl-3.6.4/doc/man3/SSL_CTX_load_verify_locations.pod | `e3357aa599e0e0656f8b63dfd9ae1344a9f00e0308c9bd722205a860028e8b48` |
| vendor | `src/.github/scripts/install-musl-build-tools.sh` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/.github/scripts/install-musl-build-tools.sh | `ba729818732a5dd8c1aefce157871afa084016ae37566034f3d09899dc7fa46f` |
| vendor | `src/.github/scripts/install-musl-openssl.sh` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/.github/scripts/install-musl-openssl.sh | `d55f9f295726ffd4408d194ea536a4e083ccf5e2a94f29d68281b163d9df9610` |
| vendor | `src/.github/workflows/rust-release.yml` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/.github/workflows/rust-release.yml | `f181ba37d0c7f6ebb8028ca050eca043d9b961fb93958da6430a6092826ffc4b` |
| vendor | `src/codex-rs/Cargo.toml` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/Cargo.toml | `db6893c3972b47ec35dd1944be2cd36008775d3f2b1eea533a4f71de30b5b173` |
| vendor | `src/codex-rs/app-server-protocol/src/protocol/common.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/app-server-protocol/src/protocol/common.rs | `4c449f1da399848c714c780fc1304ba566d0089aeaef8c33c3a9c7eb986e4272` |
| vendor | `src/codex-rs/app-server-protocol/src/protocol/v1.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/app-server-protocol/src/protocol/v1.rs | `6e767472256756ec8262097f32a2b88330a469de4a4879255835c9f99f178139` |
| vendor | `src/codex-rs/app-server-protocol/src/protocol/v2/account.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/app-server-protocol/src/protocol/v2/account.rs | `8768b6d379a1045522c72b678ac812e1e0aedfbf73f7948c31e479192ada76cc` |
| vendor | `src/codex-rs/app-server/src/external_auth.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/app-server/src/external_auth.rs | `06c20470609d680b0dcc17cc054bbff035b8db76f1afe32b6149492888db9227` |
| vendor | `src/codex-rs/app-server/src/lib.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/app-server/src/lib.rs | `a44b0266619d6adda0c1e7135d5be18e6581cff44307e78097efecc5195f8b6c` |
| vendor | `src/codex-rs/app-server/src/message_processor.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/app-server/src/message_processor.rs | `de79b843e440610da5b0b6abb31a97f9d6c6e30bc60e6d5f7a45d4f69cf6fedc` |
| vendor | `src/codex-rs/app-server/src/request_processors/account_processor.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/app-server/src/request_processors/account_processor.rs | `76d7f41221f74291f03bda83dba9cd0166c943245cdde66cd44b01f63c497915` |
| vendor | `src/codex-rs/cli/src/login.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/cli/src/login.rs | `7c887f1f55ea9e9514afd85ce19307c670b575a1edb323d7e805acd96cb524be` |
| vendor | `src/codex-rs/cli/src/main.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/cli/src/main.rs | `fdd2b41303dba1f0d022c1d40268ffc0c1cb086c852a87ef437937822bda5339` |
| vendor | `src/codex-rs/codex-api/src/endpoint/responses.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/codex-api/src/endpoint/responses.rs | `4f2fff3da4ca3ca9fcfa40ada8e98a0229af296094e03febb4d823f32ef932c8` |
| vendor | `src/codex-rs/codex-api/src/endpoint/responses_websocket.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/codex-api/src/endpoint/responses_websocket.rs | `454f4282f7b935e8eff1ad6a744c6b95d7c50a078db9747a157eb8b439e72393` |
| vendor | `src/codex-rs/codex-api/src/lib.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/codex-api/src/lib.rs | `5093646bef6b38ba47c5ea1d22492baf1f3196eab30e3e3332d12d8b69acaebc` |
| vendor | `src/codex-rs/codex-client/src/provider.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/codex-client/src/provider.rs | `5e85192349aa611f3f6a8b5ebac2e73d0841dda5caf8c875382da7d9f3a4079c` |
| vendor | `src/codex-rs/config/src/config_toml.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/config/src/config_toml.rs | `89e8c49221699a16b222f9d2f23574b1fa8d1179ddee81702de684c7fc21c2e8` |
| vendor | `src/codex-rs/config/src/types.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/config/src/types.rs | `99a4377be6d6f834f853acb530e3bd0c546f6096387afc708785b459233897b2` |
| vendor | `src/codex-rs/core/Cargo.toml` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/core/Cargo.toml | `48b3e7817b9b9391a4e05d08859f2ec460d4bcfa709885fa8577a6edd58bb393` |
| vendor | `src/codex-rs/core/src/client.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/core/src/client.rs | `662cf64d10f4b0d7eaf3ad046e53b1eb6dcf8dfb3cb5a425a3c8c222adc05da5` |
| vendor | `src/codex-rs/core/src/session/session.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/core/src/session/session.rs | `b6196ae8d707a3a3ac2bebc488cdb3aa88f0660e4a5ebc9a8ac3fa13f53cf680` |
| vendor | `src/codex-rs/features/src/lib.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/features/src/lib.rs | `2ba7074ce16eb6578f0662ab8dfde90a0fb89a16ca04c8b459ae67e398749742` |
| vendor | `src/codex-rs/http-client/Cargo.toml` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/http-client/Cargo.toml | `b522db0fba570b1667e21c4bbe1fae63f960f3821fa387b6c5bf9a24e3530f7c` |
| vendor | `src/codex-rs/http-client/src/client_builder.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/http-client/src/client_builder.rs | `6c6bc802474dbf44477afea07e3f5fc88f7279aa2e4e3bb2f817ee0e640f6a01` |
| vendor | `src/codex-rs/http-client/src/custom_ca.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/http-client/src/custom_ca.rs | `a45b0f608bda8454af32bc81951a82e4f5b68988e018fa87e2f9909b14a6e8b1` |
| vendor | `src/codex-rs/http-client/src/native_roots.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/http-client/src/native_roots.rs | `4b78d5b5f264273a3adc89cb4f8f1a797d27aa0a9d82cec6de66e717f62de7ff` |
| vendor | `src/codex-rs/http-client/src/openai_headers.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/http-client/src/openai_headers.rs | `ea7700cf5a048272f17489bf680e6f68f750b91d1c716b2df3d194e6151dc3c9` |
| vendor | `src/codex-rs/http-client/src/tls_backend_fallback.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/http-client/src/tls_backend_fallback.rs | `bfad1ec0d0dadce462a88e4d5a5d48dd1824312bea97acb013aa61a7bd005462` |
| vendor | `src/codex-rs/login/src/auth/agent_identity.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/login/src/auth/agent_identity.rs | `4f446f23ccb25f9c160398a51717b0e86bf8643daa09b4c59da6f266823bcc82` |
| vendor | `src/codex-rs/login/src/auth/default_client.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/login/src/auth/default_client.rs | `acfc0eba844d9ba02c9830c66a19ab920b51d8f3a516b95ccf5066f94ef53fcd` |
| vendor | `src/codex-rs/login/src/auth/manager.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/login/src/auth/manager.rs | `584348a470e6d7f147962bf5d642d23859f60a85772aac1e916032d414756a90` |
| vendor | `src/codex-rs/login/src/auth/revoke.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/login/src/auth/revoke.rs | `db4490daaeb89bf781d05ceeeb53f2291e4729caea2b62a635d61c98e305223e` |
| vendor | `src/codex-rs/login/src/auth/storage.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/login/src/auth/storage.rs | `ae70ac3edff091da6b0074d1eca0375c65657197d849eb46be8d16909377255d` |
| vendor | `src/codex-rs/login/src/auth/storage_commit.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/login/src/auth/storage_commit.rs | `6be37ce446b3ef21cbd25dc67e45b91ac6ea8a0d85447427b504d550f148de19` |
| vendor | `src/codex-rs/login/src/auth/storage_file.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/login/src/auth/storage_file.rs | `f8dac509e10f31a6ba105dfc2d85eca8afdf593ca08e8626a5e6c89d389dc67e` |
| vendor | `src/codex-rs/login/src/device_code_auth.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/login/src/device_code_auth.rs | `075c3f83cbc2612dde7b50f118b7a3c1ef3b7641ce6acfcca99aff5dd7454012` |
| vendor | `src/codex-rs/login/src/oauth/authorization.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/login/src/oauth/authorization.rs | `a64d8198e65b7555824f24cd641440e9769d1627052b2fbaa4a874a759139465` |
| vendor | `src/codex-rs/login/src/oauth/client.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/login/src/oauth/client.rs | `f79cdda556c137c9a244026030d604324c7c5df73b0961fc5b7a766f05f7a460` |
| vendor | `src/codex-rs/login/src/oauth/error.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/login/src/oauth/error.rs | `266de78d2063a654aa000416cb10e812816792a09bba518f53e897e409348282` |
| vendor | `src/codex-rs/login/src/oauth/pkce.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/login/src/oauth/pkce.rs | `0b11301bdf2a01b2b43d0047d187d70f01da2fec54d93e8175872fd26ed2f9a4` |
| vendor | `src/codex-rs/login/src/server.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/login/src/server.rs | `2f66d1c033e3918bc8bfda0cdad047c4553c10ec6a33ef8f80a7f9d6c2b47c10` |
| vendor | `src/codex-rs/login/src/success_page.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/login/src/success_page.rs | `bd377224b3c9c8ce773e1f961ccf56c6fffe3507d9c22e37efea4182bc4f912c` |
| vendor | `src/codex-rs/login/src/token_data.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/login/src/token_data.rs | `ca59ca56d607a602bf512603eeb1f60a8f9bbe6ef9f1bbc8555de499800b32ff` |
| vendor | `src/codex-rs/login/tests/suite/auth_refresh.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/login/tests/suite/auth_refresh.rs | `125d7300564d2dc7329056c09e6f614787a99533b764ef116de20afc0c70024d` |
| vendor | `src/codex-rs/login/tests/suite/device_code_login.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/login/tests/suite/device_code_login.rs | `1e8e811e6e51191e8fdf137a0c7f8b121d3357e0dd3a879f304fdc8aeb0971ca` |
| vendor | `src/codex-rs/model-provider-info/src/lib.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/model-provider-info/src/lib.rs | `7a9eb4b69e85fd79c590245fca24b013c24652db3a81c41a2266293495f5ff85` |
| vendor | `src/codex-rs/model-provider/src/auth.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/model-provider/src/auth.rs | `d1fd51191af4d89c07f2ca9e32f9d56b455a6f1ea24cbc43c43941f94b662456` |
| vendor | `src/codex-rs/model-provider/src/bearer_auth_provider.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/model-provider/src/bearer_auth_provider.rs | `5f97d4e4683f5eb11c0ba1765167e70a89093f7094c30ecad5055b2ce816679a` |
| vendor | `src/codex-rs/model-provider/src/chatgpt_base_url.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/model-provider/src/chatgpt_base_url.rs | `0768d523a8d71c99199644ea94c827ef4a312f325f573e0b197bbf37ca659f16` |
| vendor | `src/codex-rs/network-proxy/src/certs.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/network-proxy/src/certs.rs | `7104312a192a871014942bd9990fc0c8d09500cccd5112bd2be48e0b66081105` |
| vendor | `src/codex-rs/network-proxy/src/credential_broker/providers.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/network-proxy/src/credential_broker/providers.rs | `6221c57ac52c8b712690649c36c74ceea80f09a862dd5c62c3953378925ec791` |
| vendor | `src/codex-rs/network-proxy/src/credential_broker/providers/openai.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/network-proxy/src/credential_broker/providers/openai.rs | `a01530a958128beaee63627be9dc00e5faed6e0c76b0b378dba07965d515a322` |
| vendor | `src/codex-rs/protocol/src/auth.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/protocol/src/auth.rs | `dcae6a282263f505572575703849ce71ae8ab3faef4f909ac57d5aee113ca6ea` |
| vendor | `src/codex-rs/utils/home-dir/src/lib.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/utils/home-dir/src/lib.rs | `1338259489ade54b01caa1468b6d25fc5a183657fa94cca0add8b91865635ef5` |
| vendor | `src/codex-rs/websocket-client/src/lib.rs` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/codex-rs/websocket-client/src/lib.rs | `d27c993b77f4c795fa3cc414af57b1dbd9b2807865a7c8a3ec5751ecaf68670a` |
| vendor | `src/docs/authentication.md` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/docs/authentication.md | `3fe4a065894ac5c1c212432a2c35b9e16a39a79377ea4c9521122a28ae0f7aef` |
| vendor | `src/docs/config.md` | https://raw.githubusercontent.com/openai/codex/rust-v0.167.2/docs/config.md | `745768b4da6cd73e414fc49c5c6ef3d6bf8a0c08289fab23a7dd79776ab31d9d` |
