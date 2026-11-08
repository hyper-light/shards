#!/bin/sh
# Inside the shards-dind container (docker:29.3.1-dind): a local Sigstore as Sigstore's own
# CI runs one (sigstore/scaffolding's setup-sigstore-env, run-containers.sh): a fake OIDC
# issuer, Fulcio, Rekor v1, a timestamp authority and Rekor v2 (rekor-tiles), each from
# its pinned release's own docker compose; their trusted root and signing config; and a
# registry. Written for run.sh, which copies it in and runs it.
#
#   setup.sh WORK
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=$1
mkdir -p "$work"
cd "$work"
apk add --no-cache bash git curl coreutils findutils jq python3 >/dev/null

clone() { # REPO REF DIR
	if [ ! -d "$3" ]; then
		git -c advice.detachedHead=false clone -q --depth 1 --branch "$2" "https://github.com/$1.git" "$3"
	fi
}
# The scaffolding at a commit (it has no releases), each service at its release.
if [ ! -d scaffolding ]; then
	git init -q scaffolding
	git -C scaffolding fetch -q --depth 1 https://github.com/sigstore/scaffolding.git 86f48a0938b736821ade90fc3a5a3da086808807
	git -C scaffolding -c advice.detachedHead=false checkout -q FETCH_HEAD
fi
mkdir -p services
clone sigstore/fulcio v1.9.0 services/fulcio
clone sigstore/rekor v1.5.4 services/rekor
clone sigstore/timestamp-authority v2.1.4 services/timestamp-authority
clone sigstore/rekor-tiles v2.3.0 services/rekor-tiles

# rekor-tiles v2.3.0 builds its witness from transparency-dev/witness's main branch, which
# dropped a flag its compose passes (--poll_interval; e288f04d, "Remove feeders from
# omniwitness", hours before v2.3.0 was released): pinned to the commit before that.
sed -i 's|transparency-dev/witness.git#[0-9a-z]*|transparency-dev/witness.git#362a011c6368d2724824b0e8da647675ea4e2825|' \
	services/rekor-tiles/compose.yml

# run-containers.sh clones what is not there and pulls what is: on these detached
# checkouts the pull fails and leaves each at its release.
cd scaffolding/actions/setup-sigstore-env
# Sourced with no arguments: it reads its own (--no-fulcio and the like) from them.
CLONE_DIR="$work/services" WORK="$work" bash -c 'set --; source ./run-containers.sh >"$WORK/stack.log" 2>&1 &&
	env | grep -E "^(TRUSTED_ROOT|SIGNING_CONFIG|OIDC_TOKEN|OIDC_URL|ISSUER_URL|TSA_URL|CT_LOG_KEY)=" >"$WORK/stack.env"'
cd "$work"
test -s stack.env || { tail -50 stack.log; exit 1; }

# Dex, the OIDC provider Fulcio's own compose runs (the provider oauth2.sigstore.dev runs),
# for the sign-in flows: its name as its issuer URL has it, and Fulcio taking its tokens
# (its config file edited in place, as it is bind-mounted, then Fulcio restarted).
grep -q " dex-idp$" /etc/hosts || echo "127.0.0.1 dex-idp" >>/etc/hosts
python3 -I - "$work/services/fulcio-config.json" <<'PY'
import json, sys
path = sys.argv[1]
c = json.load(open(path))
c["OIDCIssuers"]["http://dex-idp:8888/auth"] = {"IssuerURL": "http://dex-idp:8888/auth", "ClientID": "fulcio",
                                                "Type": "email"}
with open(path, "r+") as f:
    f.seek(0)
    json.dump(c, f, indent=1)
    f.truncate()
PY
docker restart fulcio-fulcio-server-1 >/dev/null
for _ in $(seq 120); do curl -sf http://localhost:5555/healthz >/dev/null && break; sleep 1; done
mkdir -p browser
cp "$here/browser.sh" browser/xdg-open
cp "$here/device-login.sh" browser/device-login

if ! docker inspect shards-keyless-registry >/dev/null 2>&1; then
	docker run -d -q --name shards-keyless-registry -p 5000:5000 registry:2 >/dev/null
fi
echo "stack up: $(cut -d= -f1 stack.env | tr '\n' ' ')"
