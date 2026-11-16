# Step 33, step 9: each kiln harness's trust and token checks, and its credential mode (D242)

Kiln goal #70 (8f474b45c7cd86559e0566f22f9d0c33), plan step 33's step 9 (plan 01a1d2b88be57304b972761e2a5c6781; research
01a1d2b88be57304bf6b081fe46832e0: f36, f50, f52, f57, f61, f66). Each harness is the one kiln's Agentfile carries
(`.kiln/images/harness-<name>/harness.json` and `pin.json` at kiln main), read from its own build alone: its pinned
linux/amd64 files, fetched by their pins' URLs and held to their pins' sha256, then searched for the strings below. No
vendor CLI was run. The search script is the carrier's `survey/survey.py` (in `.claude/worktrees/s33-carrier`, run on
adacachy2, Linux x86_64, 2026-11-13). Each harness's counts and one line per string found are in
`evidence/survey/<harness>.txt`.

What the survey asks of each build:
- **Its CA:** whether it reads a file of roots it is told of (`NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`/`SSL_CERT_DIR`,
  `CODEX_CA_CERTIFICATE`), or the system bundle at its path (`/etc/ssl/certs/ca-certificates.crt`, Debian trixie's,
  the harness images' base). Step 2 lays the run's CA in both for a granted member, so either reaches the CA.
- **Its pins:** whether it pins a certificate or a key (the pinning APIs' names: `pinnedCertificates`,
  `publicKeyPins`, `pin-sha256`, `certificatePins`, `CertificatePinner`). None of them is in any build.
- **Its token checks:** whether it checks a token locally (`at_hash`, ID-token verification). A placeholder access token
  beside a real ID token fails an `at_hash` check (f50).

| Harness (version) | Runtime | Its CA, from its build | Pins | Local token checks | Mode |
|---|---|---|---|---|---|
| Claude Code (2.1.311) | Bun | `NODE_EXTRA_CA_CERTS` (89), `SSL_CERT_FILE` (33), the system bundle (9); docs: bundled Mozilla roots plus the OS store (f61) | none (f66) | an OIDC library's `at_hash` check is in its bundle ("missing required property at_hash", 7); its own tokens' signatures it does not verify (f52) | injected: its Anthropic sign-in runs no `at_hash` check (step 8, D242's question 5); tmpfs-fallback where it signs in through its LLM gateway's OIDC or XAA's IdP login (`at_hash`, f50) or takes Bedrock credentials through `aws login` (DPoP, f49) |
| Codex (0.167.2) | Rust | `CODEX_CA_CERTIFICATE` (17), `SSL_CERT_FILE` (23), the system bundle (15) (f57) | none | none of its own: its `at_hash` matches are gstreamer's voice library; it verifies no signature (f52) | injected |
| Gemini CLI (0.68.0) | Node v24.22.3 | `NODE_EXTRA_CA_CERTS` (22), `NODE_USE_SYSTEM_CA` (32) | none | ID-token verification strings (54), no `at_hash` | injected |
| opencode (2.2.7) | Bun | `NODE_EXTRA_CA_CERTS` (11); its docs: "the extra authorities are added to the runtime trust store when the process starts" (kiln s22 evidence) | none | ID-token strings (28), no `at_hash` | injected |
| goose (1.59.0) | Rust | rustls-native-certs (66), `SSL_CERT_FILE` (2); a run with `SSL_CERT_FILE` an empty file fails to build its client (kiln s23 `e_runtime_tls.txt`) | none | ID-token strings (6), no `at_hash` | injected |
| Amp (0.0.1794456033) | Bun | `NODE_EXTRA_CA_CERTS` (9), `NODE_USE_SYSTEM_CA` (19); its docs name `NODE_EXTRA_CA_CERTS` (kiln s24 `cli-settings.md`) | none | ID-token strings (4), no `at_hash` | injected |
| Devin (3000.12.43) | native | none of the variables; the system bundle's path (3) | none | ID-token strings (10), no `at_hash` | injected, by the system bundle alone |
| Cursor (2026.11.11) | Node | `NODE_EXTRA_CA_CERTS` (15), `SSL_CERT_FILE` (12), rustls-native-certs (28), the system bundle (17) | none | ID-token strings (12), no `at_hash` | injected |
| Antigravity (1.5.7) | Go | `SSL_CERT_FILE` and `SSL_CERT_DIR` (4 each, Go's crypto/x509), the system bundle (4) | none | none: its one `at_hash` match is absl's `log_backtrace_at_hash` | injected |
| pi (1.5.2) | Bun | `NODE_EXTRA_CA_CERTS` (3), `NODE_USE_SYSTEM_CA` (7) | none | ID-token strings (5), no `at_hash` | injected |

The numbers are each string's count in the harness's files. A string found says the build carries the code that reads
it. It doesn't say every path reads it, and a string not found doesn't prove its absence: pinning written without those
APIs' names would not show. ROOT's real runs (step 10) are what confirm a harness trusts the run's CA.

## What follows

- **No harness is tmpfs-fallback on its trust:** every build reaches the run's CA through a variable step 2 sets or
  through the system bundle step 2 lays it in, and none pins its provider.
- **Token checks went to step 8,** which captures a sign-in outside the microVM, and settled them from the builds
  (D242, step 8's questions 5 and 7; STEP8.md). Claude Code's `at_hash` check (f50's hazard) is panva's
  openid-client's, run by its LLM gateway's OIDC sign-in and XAA's IdP login for MCP servers, never by its Anthropic
  sign-in, which asks no `openid` scope and reads no ID token: those two paths are tmpfs-fallback. Neither provider's
  sign-in sends a sender-constrained token (DPoP or mTLS-bound, f49); Claude Code's Bedrock credentials through `aws
  login` refresh with a DPoP proof, so that path is fallback too. Codex checks no `at_hash`.
- **A tmpfs value lives in guest memory,** where the guest and every process it starts can read it (f36). That is
  why the fallback is the last resort, revoked at its run's end, and why no harness above is given one.
