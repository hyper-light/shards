#!/usr/bin/env bash
set -eu
scope="$HOME/native-code-mode-capture-0219-v1"
cd "$scope/source"
test "$(cat ../evidence/test-build.exit)" = 0
export CARGO_HOME="$scope/cargo-home" CARGO_TARGET_DIR="$scope/target" CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0
export ZIG_GLOBAL_CACHE_DIR="$scope/zig-global" ZIG_LOCAL_CACHE_DIR="$scope/zig-local"
export XDG_CACHE_HOME="$scope/cache" SHARDS_CACHE="$scope/cache/shards"
export CC_x86_64_unknown_linux_musl="$PWD/scripts/zig-cc" AR_x86_64_unknown_linux_musl="$PWD/scripts/zig-ar" ZIG_TARGET=x86_64-linux-musl
unset DYLD_LIBRARY_PATH DYLD_FALLBACK_LIBRARY_PATH SHARDS_HELPERS SHARDS_HELPERS_DIR SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_VM_BINARY
remaining() {
    python3 - <<'PY'
import pathlib,time
left=3600-int(time.time()-pathlib.Path('../evidence/toolchain.txt').stat().st_mtime)
assert left>0, 'original compilation budget exhausted'
print(left)
PY
}
set +e
timeout --signal=TERM --kill-after=15s "$(remaining)s" cargo clippy --locked -p shards --profile test-release --test build --test managed_output -j2 -- -D warnings > ../evidence/clippy.log 2>&1
rc=$?
printf '%s\n' "$rc" > ../evidence/clippy.exit
tail -45 ../evidence/clippy.log
test "$rc" = 0 || exit "$rc"
timeout --signal=TERM --kill-after=15s "$(remaining)s" cargo build --locked -p shards-testguest --profile guest-test --target x86_64-unknown-linux-musl -j2 > ../evidence/guest-build.log 2>&1
rc=$?
printf '%s\n' "$rc" > ../evidence/guest-build.exit
tail -40 ../evidence/guest-build.log
test "$rc" = 0 || exit "$rc"
set -e
mkdir -p "$scope/guests/guest" target/artifacts
cp "$scope/target/x86_64-unknown-linux-musl/guest-test/shards-testguest" "$scope/guests/guest/shards-testguest"
test "$(git rev-parse HEAD)" = 0219ab2eb2466ee3e9b08064434dc9efd70053d5
curl --fail --location --max-time 120 --output target/artifacts/vmlinux-6.18.48-x86_64-17c3ba205400 https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-17c3ba205400/vmlinux-6.18.48-x86_64 > ../evidence/kernel-download.log 2>&1
printf '48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7  target/artifacts/vmlinux-6.18.48-x86_64-17c3ba205400\n' | sha256sum -c - > ../evidence/kernel-check.log
python3 - "$scope" <<'PY'
import hashlib,json,pathlib,re,sys
root=pathlib.Path(sys.argv[1]); source=root/'source'
def sha(p):
 with p.open('rb') as stream: return hashlib.file_digest(stream,'sha256').hexdigest()
log=(root/'evidence/test-build.log').read_text()
targets={name:pathlib.Path(re.search(r'Executable tests/'+re.escape(name)+r'\.rs \(([^\n]+)\)',log).group(1)) for name in ('build','managed_output')}
source_hashes={line.split()[1]:line.split()[0] for line in (root/'evidence/source-hashes.txt').read_text().splitlines()}
assert all(sha(source/p)==digest for p,digest in source_hashes.items())
inputs={'revision':'0219ab2eb2466ee3e9b08064434dc9efd70053d5','source_hashes':source_hashes,'binary_sha256':sha(targets['build']),'guest_sha256':sha(root/'guests/guest/shards-testguest'),'kernel_sha256':sha(source/'target/artifacts/vmlinux-6.18.48-x86_64-17c3ba205400')}
(root/'evidence/inputs.json').write_text(json.dumps(inputs,indent=2))
(root/'evidence/test-paths.json').write_text(json.dumps({k:str(v) for k,v in targets.items()},indent=2))
paths=list(targets.values())+[root/'target/test-release/shards',root/'guests/guest/shards-testguest',source/'target/artifacts/vmlinux-6.18.48-x86_64-17c3ba205400']
paths.extend(p for p in root.glob('target/test-release/build/shards-*/out/*') if p.is_file() and p.name in ('shards-init','shards-server'))
paths.extend(p for p in root.glob('target/test-release/build/shards-*/out/helpers/*') if p.is_file())
facts=[{'path':str(p),'bytes':p.stat().st_size,'sha256':sha(p)} for p in paths]
(root/'evidence/artifacts.json').write_text(json.dumps(facts,indent=2))
print(json.dumps({'targets':{k:str(v) for k,v in targets.items()},'inputs':inputs,'artifacts':facts},indent=2))
PY
metadata_binary=$(python3 - <<'PY'
import json,pathlib
print(json.loads(pathlib.Path('../evidence/test-paths.json').read_text())['managed_output'])
PY
)
set +e
timeout --signal=TERM --kill-after=15s "$(remaining)s" "$metadata_binary" metadata_requires_unique_scoped_loopback_endpoints --exact --nocapture --test-threads=1 > ../evidence/metadata-pure.log 2>&1
rc=$?
printf '%s\n' "$rc" > ../evidence/metadata-pure.exit
cat ../evidence/metadata-pure.log
exit "$rc"
