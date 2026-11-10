set -eu
scope="$HOME/native-code-mode-capture-0219-v1"
cd "$scope"
test "$(cat evidence/clippy.exit)" = 0
test "$(cat evidence/metadata-pure.exit)" = 0
test "$(cat evidence/guest-build.exit)" = 0
export CARGO_HOME="$scope/cargo-home" CARGO_TARGET_DIR="$scope/target"
export ZIG_GLOBAL_CACHE_DIR="$scope/zig-global" ZIG_LOCAL_CACHE_DIR="$scope/zig-local"
export XDG_CACHE_HOME="$scope/cache" SHARDS_CACHE="$scope/cache/shards"
set +e
python3 "$scope/source/docs/research/measurements/managed-code-mode-capture/caller.py" --source "$scope/source" --binary "$scope/target/test-release/deps/build-80db23d53dc40e41" --records "$scope/evidence/run-v1" --guests "$scope/guests" --kernel "$scope/source/target/artifacts/vmlinux-6.18.48-x86_64-17c3ba205400" --inputs "$scope/evidence/inputs.json" --outer-ms 240000 --reap-ms 15000
rc=$?
printf '%s\n' "$rc" > evidence/native-caller.exit
if test -f evidence/run-v1/native.log; then tail -100 evidence/run-v1/native.log; fi
exit "$rc"
