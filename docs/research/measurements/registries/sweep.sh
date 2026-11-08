#!/bin/sh
# sweep.sh SHARDS_BIN OUT REF...: pulls each reference cold with shards and with docker,
# for linux/arm64 (PLATFORM overrides), each after removing what the last pull left,
# and appends to OUT one line a pull: `CLIENT REF EXIT SECONDS DIGEST`, the digest as
# `images --digests` shows it. Then prints, per reference, whether both pulled and agree.
# SHARDS_HOME must name the shards home to use; LEDGER, when set, gets each reference
# docker pulled, for cleanup.
set -u
bin=$1 out=$2
shift 2
platform=${PLATFORM:-linux/arm64}
digest() { # CLIENT REF
    # Both CLIs filter Docker Hub images by their familiar names alone.
    ref=${2#docker.io/library/}
    ref=${ref#docker.io/}
    tag=${ref##*:}
    case $1 in
        shards) "$bin" images --digests "$ref" 2>/dev/null ;;
        docker) docker images --digests "$ref" 2>/dev/null ;;
    esac | awk -v t="$tag" 'NR > 1 && $2 == t { print $3; exit }'
}
for ref in "$@"; do
    for client in shards docker; do
        case $client in
            shards) "$bin" rmi -f "$ref" >/dev/null 2>&1 ;;
            docker) docker rmi -f "$ref" >/dev/null 2>&1; [ -n "${LEDGER:-}" ] && echo "docker-image $ref" >>"$LEDGER" ;;
        esac
        start=$(python3 -c 'import time; print(time.time())')
        case $client in
            shards) "$bin" pull --platform "$platform" "$ref" >"$out.last" 2>&1 ;;
            docker) docker pull --platform "$platform" "$ref" >"$out.last" 2>&1 ;;
        esac
        code=$?
        secs=$(python3 -c "import time; print(f'{time.time() - $start:.2f}')")
        [ "$code" -eq 0 ] || sed "s/^/  $client: /" "$out.last" | tail -3
        echo "$client $ref $code $secs $(digest "$client" "$ref")" >>"$out"
    done
done
python3 - "$out" "$@" <<'PY'
import sys
lines = [l.split() for l in open(sys.argv[1])]
print("| reference | shards s | docker s | shards | docker | digests agree |")
print("|---|---|---|---|---|---|")
for ref in sys.argv[2:]:
    got = {l[0]: l for l in lines if l[1] == ref}
    s, d = got.get("shards"), got.get("docker")
    if not s or not d: continue
    ok = lambda l: "ok" if l[2] == "0" else f"exit {l[2]}"
    agree = "yes" if len(s) > 4 and len(d) > 4 and s[4] == d[4] else "NO"
    print(f"| {ref} | {s[3]} | {d[3]} | {ok(s)} | {ok(d)} | {agree} |")
PY
