#!/bin/bash
# The lane's arm64 checks in Docker Desktop's arm64 Linux on an Apple silicon Mac (no arm64
# Linux runner is here): the branch's head cloned from GitHub, copied into a volume of its
# own for each image, owned by uid 1000 (the checks run as /work's owner, never root; each
# image's probes and mutants change its own copy alone), each file newer than any build
# kept in the target volumes (a build of a mutant a stopped run left is never reused), then
# checks.sh in CI's pinned musl image and in Debian's rust image, side by side, each
# container detached so that it outlives this script. Logs in ~/musl-opath-v1-logs/REV/.
set -u
w=~/musl-opath-v1-work
if [ -d "$w/.git" ]; then
  git -C "$w" fetch -q --depth 1 origin p/musl-opath-v1 && git -C "$w" reset -q --hard FETCH_HEAD
else
  git clone -q --depth 1 --branch p/musl-opath-v1 https://github.com/hyper-light/shards "$w"
fi
rev=$(git -C "$w" rev-parse --short HEAD)
out=~/musl-opath-v1-logs/$rev
mkdir -p "$out"
musl=rust:1.98.1-alpine@sha256:7cc1c22d77d9432f7fe012a70e6d3e555af54c2a6832700ed7d553f1769ae89f
gnu=rust:1.98.1
for v in src-musl src-gnu target-musl target-gnu cargo-musl cargo-gnu; do docker volume create "musl-opath-$v" >/dev/null; done
docker run --rm -v "$w:/src:ro" -v musl-opath-src-musl:/w1 -v musl-opath-src-gnu:/w2 -v musl-opath-target-musl:/t1 \
  -v musl-opath-target-gnu:/t2 -v musl-opath-cargo-musl:/c1 -v musl-opath-cargo-gnu:/c2 alpine:3.22 \
  sh -c 'for d in /w1 /w2; do find $d -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} +; cp -R /src/. $d/; done && chown -R 1000:1000 /w1 /w2 /t1 /t2 /c1 /c2' || exit 1
docker pull -q "$gnu" >/dev/null && docker image inspect --format '{{index .RepoDigests 0}}' "$gnu" > "$out/gnu-image"
docker rm -f "musl-opath-musl-$rev" "musl-opath-gnu-$rev" >/dev/null 2>&1
docker run -d --name "musl-opath-musl-$rev" -v musl-opath-src-musl:/work -v musl-opath-target-musl:/work/target \
  -v musl-opath-cargo-musl:/home/shards/.cargo -w /work -e TARGET=aarch64-unknown-linux-musl -e REV="$rev" \
  -e CARGO_BUILD_JOBS=9 -e CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=cc "$musl" \
  sh docs/research/measurements/musl-opath-v1/harness/container-musl.sh >/dev/null
docker run -d --name "musl-opath-gnu-$rev" --security-opt seccomp=unconfined -v musl-opath-src-gnu:/work \
  -v musl-opath-target-gnu:/work/target -v musl-opath-cargo-gnu:/home/shards/.cargo -w /work \
  -e TARGET=aarch64-unknown-linux-gnu -e REV="$rev" -e CARGO_BUILD_JOBS=9 "$gnu" \
  sh docs/research/measurements/musl-opath-v1/harness/container-gnu.sh >/dev/null
for k in musl gnu; do
  code=$(docker wait "musl-opath-$k-$rev")
  docker logs "musl-opath-$k-$rev" > "$out/aarch64-$k.log" 2>&1
  echo "aarch64 $k: exit $code"
  docker rm "musl-opath-$k-$rev" >/dev/null
done
cat "$out/gnu-image"
for f in "$out"/*.log; do
  echo "=== $f"
  grep -aE "^(RESULT|PROBE|MUTANT)|STOPPED_IN_WAIT|KEPT|REAPER|ATOMICS_WAIT|refused syscall|^error|could not compile" "$f" | head -60
done
