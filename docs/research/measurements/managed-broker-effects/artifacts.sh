#!/usr/bin/env bash
# Run only after the recorded normal test build has actually completed successfully.
set -eu
scope="$HOME/native-managed-broker-effects-3b-v1"
cd "$scope/source"
test "$(cat "$scope/evidence/test-build.exit")" = 0
export CARGO_HOME="$scope/cargo-home"
export CARGO_TARGET_DIR="$scope/target"
export CARGO_BUILD_JOBS=2
export CARGO_INCREMENTAL=0
export ZIG_GLOBAL_CACHE_DIR="$scope/zig-global"
export ZIG_LOCAL_CACHE_DIR="$scope/zig-local"
export XDG_CACHE_HOME="$scope/cache"
export SHARDS_CACHE="$scope/cache/shards"
export CC_x86_64_unknown_linux_musl="$PWD/scripts/zig-cc"
export AR_x86_64_unknown_linux_musl="$PWD/scripts/zig-ar"
export ZIG_TARGET=x86_64-linux-musl
unset DYLD_LIBRARY_PATH DYLD_FALLBACK_LIBRARY_PATH SHARDS_HELPERS SHARDS_HELPERS_DIR SHARDS_INIT_BINARY SHARDS_SERVER_BINARY
mkdir -p "$ZIG_GLOBAL_CACHE_DIR" "$ZIG_LOCAL_CACHE_DIR" "$XDG_CACHE_HOME" "$scope/guests/guest" target/artifacts
remaining() {
    python3 - "$scope/evidence/toolchain.txt" <<'PY'
import pathlib,sys,time
left=3600-int(time.time()-pathlib.Path(sys.argv[1]).stat().st_mtime)
assert left>0, 'original 3600s compilation budget exhausted'
print(left)
PY
}
set +e
timeout --signal=TERM --kill-after=15s "$(remaining)s" cargo clippy --locked -p shards --profile test-release --test managed_output -j2 -- -D warnings > "$scope/evidence/clippy.log" 2>&1
rc=$?
printf '%s\n' "$rc" > "$scope/evidence/clippy.exit"
tail -40 "$scope/evidence/clippy.log"
test "$rc" = 0 || exit "$rc"
timeout --signal=TERM --kill-after=15s "$(remaining)s" cargo build --locked -p shards-testguest --profile guest-test --target x86_64-unknown-linux-musl -j2 > "$scope/evidence/guest-build.log" 2>&1
rc=$?
printf '%s\n' "$rc" > "$scope/evidence/guest-build.exit"
tail -40 "$scope/evidence/guest-build.log"
test "$rc" = 0 || exit "$rc"
set -e
cp "$scope/target/x86_64-unknown-linux-musl/guest-test/shards-testguest" "$scope/guests/guest/shards-testguest"
curl --fail --location --max-time 120 --output target/artifacts/vmlinux-6.18.48-x86_64-17c3ba205400 https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-17c3ba205400/vmlinux-6.18.48-x86_64 > "$scope/evidence/kernel-download.log" 2>&1
printf '48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7  target/artifacts/vmlinux-6.18.48-x86_64-17c3ba205400\n' | sha256sum -c - > "$scope/evidence/kernel-check.log"
python3 - "$scope" <<'PY'
import hashlib,json,pathlib,sys
root=pathlib.Path(sys.argv[1])
paths=[root/'target/test-release/shards',root/'guests/guest/shards-testguest',root/'source/target/artifacts/vmlinux-6.18.48-x86_64-17c3ba205400']
paths.extend(p for p in (root/'target/test-release/deps').glob('managed_output-*') if p.is_file() and p.suffix=='')
paths.extend(p for p in root.glob('target/test-release/build/shards-*/out/*') if p.is_file() and p.name in ('shards-init','shards-server'))
paths.extend(p for p in root.glob('target/test-release/build/shards-*/out/helpers/*') if p.is_file())
facts=[]
for p in paths:
 with p.open('rb') as f: digest=hashlib.file_digest(f,'sha256').hexdigest()
 facts.append({'path':str(p),'bytes':p.stat().st_size,'sha256':digest})
(root/'evidence/artifacts.json').write_text(json.dumps(facts,indent=2))
print(json.dumps(facts,indent=2))
PY
