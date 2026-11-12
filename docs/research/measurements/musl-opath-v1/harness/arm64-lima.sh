#!/bin/bash
# arm64 Linux on an Apple silicon Mac (no arm64 Linux host here has its own kernel to run
# on): a Lima VM of Ubuntu with rootful Docker, made once and kept, the checkout copied
# onto its own disk, then checks.sh in CI's pinned musl image (container-musl.sh) and in
# Debian's rust image (container-gnu.sh). Logs go to /tmp/lima/musl-opath-REV, which the VM
# shares with the Mac.
#
#   docs/research/measurements/musl-opath-v1/harness/arm64-lima.sh [musl|gnu]...
set -u
vm=musl-opath
rev=$(git rev-parse --short HEAD)
src=$PWD
out=/tmp/lima/musl-opath-$rev
mkdir -p "$out"
musl=rust:1.98.1-alpine@sha256:7cc1c22d77d9432f7fe012a70e6d3e555af54c2a6832700ed7d553f1769ae89f
gnu=rust:1.98.1
if ! limactl list -q 2>/dev/null | grep -qx "$vm"; then
  limactl create --name="$vm" --tty=false --vm-type=vz --cpus=16 --memory=24 --disk=100 \
    template://docker-rootful || exit 1
fi
limactl start --tty=false "$vm" >/dev/null 2>&1 || limactl start --tty=false "$vm" || exit 1
in_vm() { limactl shell --workdir / "$vm" -- "$@"; }
in_vm uname -a
# The checkout on the VM's own disk: its home, the Mac's, is mounted read-only.
in_vm bash -c "rm -rf ~/work-$rev && mkdir -p ~/work-$rev && tar -C '$src' --exclude=./target -cf - . | tar -C ~/work-$rev -xf - && echo copied \$(du -sh ~/work-$rev | cut -f1)"
for kind in "${@:-musl gnu}"; do
  for k in $kind; do
    case $k in
    musl)
      in_vm bash -c "mkdir -p ~/cargo-musl && cd ~/work-$rev && sudo docker run --rm -v \$PWD:/work -w /work \
        -v \$HOME/cargo-musl:/home/shards/.cargo -e TARGET=aarch64-unknown-linux-musl -e REV=$rev \
        -e CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=cc $musl \
        sh docs/research/measurements/musl-opath-v1/harness/container-musl.sh" > "$out/aarch64-musl.log" 2>&1
      echo "aarch64 musl: exit $?"
      ;;
    gnu)
      in_vm bash -c "cd ~/work-$rev && sudo docker pull -q $gnu >/dev/null && sudo docker image inspect --format '{{index .RepoDigests 0}}' $gnu && \
        sudo docker run --rm --security-opt seccomp=unconfined -v \$PWD:/work -w /work -e TARGET=aarch64-unknown-linux-gnu \
        -e REV=$rev $gnu sh docs/research/measurements/musl-opath-v1/harness/container-gnu.sh" > "$out/aarch64-gnu.log" 2>&1
      echo "aarch64 gnu: exit $?"
      ;;
    esac
  done
done
for f in "$out"/*.log; do
  echo "=== $f"
  grep -aE "^(RESULT|PROBE|MUTANT)|^rust:|STOPPED_IN_WAIT|KEPT|REAPER|refused syscall|error(\[|:)|could not compile" "$f" | head -60
done
