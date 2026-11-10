set -eu
set -o pipefail
umask 077
campaign="$HOME/managed-tenant-live-01a1c5d2-v1"
mkdir "$campaign"
mkdir "$campaign/records" "$campaign/records/empty-docker"
exec 9>"$campaign/.campaign.lock"
flock -n 9
cd "$campaign"
python3 - <<'PY' > records/preflight.json
import os,platform,json,fcntl
s=os.statvfs('.')
free=s.f_bavail*s.f_frsize
assert free >= 40*1024**3,free
fd=os.open('/dev/kvm',os.O_RDWR|os.O_CLOEXEC)
try: version=fcntl.ioctl(fd,0xae00,0)
finally: os.close(fd)
assert version==12,version
print(json.dumps({'uname':platform.uname()._asdict(),'uid':os.getuid(),'free_bytes':free,'kvm_api':version,'ask':{'cpus':4,'memory_bytes':8*1024**3,'disk_bytes':40*1024**3},'local_build':False},indent=2))
PY
timeout 300 git clone --no-tags --depth 1 --single-branch --branch p/managed-tenant-live-cbd-v1 git@github.com:hyper-light/shards.git source >records/clone.log 2>&1
cd source
test "$(git rev-parse HEAD)" = "446211e57d302729832ece92e55cfa13713b7d7a"
test -z "$(git status --porcelain)"
git rev-parse HEAD > ../records/source.txt
git show --format=fuller --no-patch HEAD > ../records/commit.txt
python3 - <<'PY' > ../records/policy.json
import hashlib,json
from pathlib import Path
b=Path('docs/development-rules.md').read_bytes()
sha=hashlib.sha256(b).hexdigest()
assert sha=='df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3',sha
print(json.dumps({'main':'cbd39284937d024640c56db6f8d8ef2bd9b0ee24','rules_sha256':sha,'role':'assigned adversarial','owned':'inherited-writer Case A + required fixture task registration'},indent=2))
PY
kernel="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
test -f "$kernel"
printf '%s  %s\n' 97819d00fecb5de0cffc4bf292af9c78c5d662336e07226ae58731fd580537ae "$kernel" | sha256sum --check > ../records/kernel.sha256
rustc -Vv > ../records/rustc.txt
cargo -V > ../records/cargo.txt
rustup target list --installed > ../records/installed-targets.txt
grep -Fx x86_64-unknown-linux-musl ../records/installed-targets.txt
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT
export CARGO_BUILD_JOBS=4
export CARGO_TARGET_DIR="$campaign/target"
export DOCKER_CONFIG="$campaign/records/empty-docker"
export SHARDS_REQUIRE_VMS=1
export SHARDS_KERNEL="$kernel"
export SHARDS_TEST_KERNEL="$kernel"
export SHARDS_TENANT_EXIT_SOURCE_REVISION="446211e57d302729832ece92e55cfa13713b7d7a"
export SHARDS_TENANT_STDIO_SOURCE_REVISION="446211e57d302729832ece92e55cfa13713b7d7a"
python3 - <<'PY' > ../records/build-environment.json
import json,os
names=['CARGO_BUILD_JOBS','CARGO_TARGET_DIR','DOCKER_CONFIG','SHARDS_REQUIRE_VMS','SHARDS_KERNEL','SHARDS_TEST_KERNEL','SHARDS_TENANT_EXIT_SOURCE_REVISION','SHARDS_TENANT_STDIO_SOURCE_REVISION']
print(json.dumps({k:os.environ[k] for k in names},indent=2))
PY
scripts/check-development-policy > ../records/policy.log 2>&1
git diff --check > ../records/diff-check.log 2>&1
printf '%s\n' 'timeout --signal=TERM --kill-after=30s 3600s cargo test --locked -j4 --profile test-release -p shards --test managed_output --no-run --message-format=json' > ../records/compile.command
set +e
timeout --signal=TERM --kill-after=30s 3600s cargo test --locked -j4 --profile test-release -p shards --test managed_output --no-run --message-format=json > ../records/compile.jsonl 2> ../records/compile.log
compile_status="$?"
set -e
printf '%s\n' "$compile_status" > ../records/compile.exit
if [ "$compile_status" -ne 0 ]; then tail -80 ../records/compile.log; exit "$compile_status"; fi
printf '%s\n' 'timeout --signal=TERM --kill-after=30s 1800s cargo build --locked -j4 -p shards-testguest --profile guest --target x86_64-unknown-linux-musl --target-dir source/target/guest' > ../records/testguest.command
set +e
CC_x86_64_unknown_linux_musl="$campaign/source/scripts/zig-cc" AR_x86_64_unknown_linux_musl="$campaign/source/scripts/zig-ar" ZIG_TARGET=x86_64-linux-musl timeout --signal=TERM --kill-after=30s 1800s cargo build --locked -j4 -p shards-testguest --profile guest --target x86_64-unknown-linux-musl --target-dir "$campaign/source/target/guest" > ../records/testguest.log 2>&1
guest_status="$?"
set -e
printf '%s\n' "$guest_status" > ../records/testguest.exit
if [ "$guest_status" -ne 0 ]; then tail -80 ../records/testguest.log; exit "$guest_status"; fi
python3 - <<'PY' > ../records/artifacts.json
from pathlib import Path
import json,hashlib,subprocess,os
records=Path('../records')
packets=[json.loads(line) for line in (records/'compile.jsonl').read_text().splitlines()]
bins=[v['executable'] for v in packets if v.get('reason')=='compiler-artifact' and v.get('target',{}).get('name')=='managed_output' and v.get('executable')]
assert len(bins)==1,bins
(records/'test-binary.txt').write_text(bins[0]+'\n')
paths=[Path(bins[0]),Path(os.environ['CARGO_TARGET_DIR'])/'test-release/shards',Path('target/guest/x86_64-unknown-linux-musl/guest/shards-testguest')]
outdirs=[Path(v['out_dir']) for v in packets if v.get('reason')=='build-script-executed' and v.get('package_id','').split('#')[-1].startswith('shards@')]
for d in outdirs:
    paths.extend(d/n for n in ['shards-init','shards-server','carried/shards-vm','carried/shards-net'] if (d/n).exists())
entries=[{'path':str(p.resolve()),'sha256':hashlib.sha256(p.read_bytes()).hexdigest(),'bytes':p.stat().st_size} for p in paths]
assert len(entries)>=7,entries
print(json.dumps(entries,indent=2))
PY
du -sk "$campaign/source" "$campaign/target" "$campaign/records" > ../records/build-footprint-kib.txt
cat ../records/preflight.json ../records/build-footprint-kib.txt ../records/artifacts.json
printf '%s\n' 'matching source host+carried helpers+testguest build complete'
