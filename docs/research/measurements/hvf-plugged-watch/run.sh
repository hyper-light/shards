#!/bin/bash
# small-vm-fault lane (implementer, ROOT g121): red and green of HVF's vCPU giving back memory
# mapped after it was made (a plug's), which the working-set watch took away.
# $1 RED commit, $2 GREEN commit, $3 steps (comma list; default all).
set -uo pipefail
here=$(pwd)
RED=$1; GREEN=$2; STEPS=${3:-unit-red,unit-green,unit-mutant,e2e-red,e2e-green,clippy}
W=$HOME/g121-tmp/smallvm
mkdir -p "$W"
for i in 1 2 3 4 5 6; do
  [ -d "$W/shards/.git" ] && break
  rm -rf "$W/shards"
  git clone -q --filter=blob:none --no-checkout git@github.com:hyper-light/shards.git "$W/shards" \
    || git clone -q --filter=blob:none --no-checkout https://github.com/hyper-light/shards.git "$W/shards" || sleep 30
done
cd "$W/shards" || exit 2
for i in 1 2 3 4 5 6; do git cat-file -e 6bfdcb1a975bb98c821849a5353e2efea5a838f1 2>/dev/null && break; git fetch -q origin main; sleep 5; done
git fetch -q "$here/lane.bundle" "+refs/heads/*:refs/remotes/carrier/*" || exit 2
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
export CARGO_TARGET_DIR="$W/target" CARGO_INCREMENTAL=0
at() {
  git checkout -q --force "$1" || exit 2
  git clean -fdq crates
  [ "$(git rev-parse HEAD)" = "$(git rev-parse "$1^{commit}")" ] || { echo "not at $1"; exit 2; }
  echo "== at $(git rev-parse --short HEAD): $(git log -1 --format=%s | cut -c1-100)"
}
at "$GREEN"
zig=$(sh scripts/install-zig 2>/dev/null) && export PATH="$zig:$PATH"
echo "on $(hostname) $(uname -sm) macOS $(sw_vers -productVersion) $(sysctl -n machdep.cpu.brand_string); zig $(zig version 2>/dev/null); df $(df -h "$W" | tail -1 | awk '{print $4}') free"
J="-j 8"
UNIT=hv::hvf::tests::vcpu_and_gic_state_round_trip_through_a_fresh_vcpu
unit() {
  nice -n 5 cargo test --locked $J --profile test-release -p shards-vmm --lib -- "$UNIT" --exact --nocapture 2>&1 \
    | grep -E "^test |panicked|unhandled|left|right|assert|test result|^error" | head -20
  echo "UNIT-$1 exit ${PIPESTATUS[0]}"
}
e2e() {
  cat "$here/sweep.rs" >> crates/shards/tests/api_image.rs
  nice -n 5 cargo test --locked $J --profile test-release -p shards --test api_image --no-run 2>&1 | grep -E "^error|warning: unused|Finished" | head; echo "BUILD-$1 exit ${PIPESTATUS[0]}"
  SWEEP_MIB=${SWEEP_MIB:-64,72,80,96,112,128,160,192,256} nice -n 5 cargo test --locked $J --profile test-release -p shards --test api_image -- zz_sweep_memory --exact --nocapture --test-threads=1 2>&1 \
    | grep -E "SWEEP|^    |panicked|test result" | cut -c1-600 | head -150
  echo "SWEEP-$1 exit ${PIPESTATUS[0]}"
  nice -n 5 cargo test --locked $J --profile test-release -p shards --test api_image -- the_least_envelope_saves_and_restores_its_template_and_grows_past_its_boot_memory --exact --nocapture --test-threads=1 2>&1 \
    | grep -vE "^\s+(Compiling|Running|Finished)" | grep -E "unhandled|ended|panicked|^test |test result|left|right|Boot|SCRATCH" | cut -c1-900 | head -60
  echo "E2E-$1 exit ${PIPESTATUS[0]}"
  git checkout -q -- crates
}
IFS=, read -ra steps <<< "$STEPS"
for step in "${steps[@]}"; do
  case $step in
  unit-red) at "$RED"; unit RED ;;
  e2e-red) at "$RED"; python3 -I "$here/inst.py" || exit 3; e2e RED ;;
  unit-green) at "$GREEN"; unit GREEN ;;
  unit-mutant) at "$GREEN"; python3 -I "$here/mut.py" || exit 3; unit MUTANT; git checkout -q -- crates; touch crates/vmm/src/hv/hvf/mod.rs ;;
  e2e-green) at "$GREEN"; e2e GREEN ;;
  clippy) at "$GREEN"
    nice -n 5 cargo clippy --locked $J -p shards-vmm -p shards --all-targets -- -D warnings 2>&1 | grep -E "^(warning|error)|-->|Finished" | grep -v aws-lc | head -30
    echo "CLIPPY exit ${PIPESTATUS[0]}"
    cargo fmt --all --check >/dev/null 2>&1; echo "FMT exit $?" ;;
  clean) rm -rf "$W"; echo "removed $W" ;;
  esac
done
