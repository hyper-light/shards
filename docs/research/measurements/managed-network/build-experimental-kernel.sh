#!/usr/bin/env bash
# Coordinator experiment only; no production kernel pin is changed.
set -euo pipefail

checkout=${1:?source checkout}
revision=${2:?exact source revision}
proof=${3:?new absolute evidence directory}
jobs=${4:?assigned CPU count}
memory=${5:?assigned container memory, such as 16g}
case "$proof" in /*) ;; *) echo 'evidence directory must be absolute' >&2; exit 2 ;; esac
case "$jobs" in ''|*[!0-9]*|0) echo 'positive assigned CPU count required' >&2; exit 2 ;; esac
[ "$(id -u)" -ne 0 ]
[ "$(podman info --format '{{.Host.Security.Rootless}}')" = true ]
[ ! -e "$proof" ]
mkdir -m 700 "$proof"
mkdir "$proof/input" "$proof/out" "$proof/out/work" "$proof/docker"
printf '{"auths":{}}\n' > "$proof/auth.json"
export REGISTRY_AUTH_FILE="$proof/auth.json" DOCKER_CONFIG="$proof/docker"
git -C "$checkout" rev-parse "$revision^{commit}" > "$proof/source-revision"
git -C "$checkout" archive "$revision" scripts resources/kernel |
    tar -xf - -C "$proof/input"
cat > "$proof/input/resources/kernel/managed-tproxy.config" <<'CONFIG'
CONFIG_NFT_SOCKET=y
CONFIG_NFT_TPROXY=y
CONFIG
cp "$proof/input/scripts/build-kernel.sh" "$proof/build-kernel.original.sh"
python3 - "$proof/input/scripts/build-kernel.sh" <<'PY'
from pathlib import Path
import sys
path = Path(sys.argv[1])
source = path.read_text()
old_trap = "trap 'rm -rf \"$work\"' EXIT"
new_trap = "trap 'rc=$?; printf \"%s\\n\" \"$work\" > \"$out/source-directory\"; exit \"$rc\"' EXIT"
old_jobs = '-j"$(nproc)"'
new_jobs = '-j"${KERNEL_BUILD_JOBS:?assigned CPU count}"'
if source.count(old_trap) != 1 or source.count(old_jobs) != 1:
    raise SystemExit('build script changed: review experiment adapter')
path.write_text(source.replace(old_trap, new_trap).replace(old_jobs, new_jobs))
PY
diff -u "$proof/build-kernel.original.sh" "$proof/input/scripts/build-kernel.sh" > "$proof/build-adapter.patch" || [ "$?" -eq 1 ]
find "$proof/input" -type f -print0 | sort -z | xargs -0 sha256sum > "$proof/inputs.sha256"
podman version > "$proof/podman-version"
podman info --format '{{.Host.Security.Rootless}}' > "$proof/rootless"
uname -a > "$proof/host"
id >> "$proof/host"
. "$proof/input/resources/kernel/builder.env"
printf '%s\n' "$BUILDER" > "$proof/builder-image"
set +e
podman run --rm --authfile "$proof/auth.json" --cpus "$jobs" --memory "$memory" \
    -v "$proof/input:/src:ro,Z" -v "$proof/out:/out:Z" \
    -e SNAPSHOT="$SNAPSHOT" -e PACKAGES="$PACKAGES" \
    -e KERNEL_BUILD_JOBS="$jobs" -e TMPDIR=/out/work \
    "$BUILDER" bash -euo pipefail -c '
        rm -f /etc/apt/sources.list.d/*
        echo "deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/$SNAPSHOT bookworm main" > /etc/apt/sources.list
        apt-get -o Acquire::Check-Valid-Until=false -o Acquire::Retries=5 update -q
        apt-get -o Acquire::Retries=5 install -y -q --no-install-recommends $PACKAGES
        {
            gcc --version | head -1
            ld --version | head -1
            dpkg-query -W -f="\${Package} \${Version}\n" $PACKAGES gcc-12 binutils libc6-dev
        } > /out/toolchain.txt
        /src/scripts/build-kernel.sh "$(uname -m)" /out managed-tproxy
    ' > "$proof/build.log" 2>&1
code=$?
set -e
printf '%s\n' "$code" > "$proof/build.exit"
podman image inspect "$BUILDER" --format '{{.Id}} {{.Digest}}' > "$proof/pulled-image" 2>&1 || true
printf 'kernel experiment exit=%s; evidence=%s\n' "$code" "$proof"
exit "$code"
