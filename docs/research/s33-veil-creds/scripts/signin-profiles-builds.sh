#!/bin/bash
# The closed harness builds' sign-in strings (D242, the sign-in profiles lane): each pinned linux/amd64 archive (kiln's
# .kiln/images/harness-<name>/pin.json) streamed through tar and ripgrep by signin-profiles-scan.sh, nothing written
# but the matches; each output ends with the archive's sha256 held to its pin. Run from the repository root:
#   docs/research/s33-veil-creds/scripts/signin-profiles-builds.sh docs/research/s33-veil-creds/evidence/signin-profiles
set -euo pipefail
out=${1:?an output directory}
scan="$(dirname "$0")/signin-profiles-scan.sh"
redact() { sed -E 's/GOCSPX-[A-Za-z0-9_-]+/GOCSPX-<redacted>/g'; }
"$scan" https://storage.googleapis.com/antigravity-public/antigravity-cli/1.5.7-6190022064472064/linux-x64-musl/cli_linux_x64_musl.tar.gz \
  5000036342e1269ff91624a9eb49890613e8b88ab24de22081546f5ec625c654 antigravity \
  'oauth2/v3/tokeninfo|failed to call tokeninfo|oauth2/v2/userinfo|https://oauth2.googleapis.com/token|oauth2/google: no credentials|RefreshToken.json:"refresh_token"' 60 \
  | redact > "$out/antigravity-build.txt"
"$scan" https://static.devin.ai/cli/3000.12.43/devin-3000.12.43-x86_64-unknown-linux.tar.gz \
  6a19997b5fc2d6ba1a8c341d4e2b12f1b78cad5d415ee9c0ca413a8d52a99562 bin/devin \
  'ExchangePKCEAuthorizationCode|ExchangePkceAuthorizationCodeResponse|/auth/cli/token|/auth/cli/continue|WindsurfCredentials|credentials\.toml|meta\.api_key|Paste the code from the sign-in page' 70 \
  > "$out/devin-build.txt"
"$scan" https://downloads.cursor.com/lab/2026.11.11-b912975/linux/x64/agent-cli-package.tar.gz \
  be01f42ad334f81d05a9992fcf060a52cc746f52cc451e849eb652146a6e3c91 dist-package/index.js \
  'auth/poll`|exchange_user_api_key`|writeAuthData\(\{accessToken|"auth\.json"\)\}case"darwin"|JWT payload is null' 110 \
  > "$out/cursor-build.txt"
