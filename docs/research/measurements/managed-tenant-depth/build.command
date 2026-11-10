set -eu
set -o pipefail
umask 077
candidate="${SHARDS_TENANT_DEPTH_REVISION:?exact signed depth candidate required}"
export SHARDS_TENANT_DEPTH_REVISION="$candidate"
campaign="$HOME/managed-tenant-depth-a920-01a1c5d2-v1"
mkdir "$campaign"
mkdir "$campaign/records" "$campaign/records/empty-docker" "$campaign/cache" "$campaign/frozen"
exec 9>"$campaign/.campaign.lock"
flock -n 9
cd "$campaign"
python3 - <<'PYCHECK' > records/preflight.json
import os,platform,json,fcntl
s=os.statvfs('.');free=s.f_bavail*s.f_frsize
assert free>=24*1024**3
fd=os.open('/dev/kvm',os.O_RDWR|os.O_CLOEXEC)
try: version=fcntl.ioctl(fd,0xae00,0)
finally: os.close(fd)
assert version==12
print(json.dumps({'uname':platform.uname()._asdict(),'uid':os.getuid(),'free_bytes':free,'kvm_api':version,'ask':{'cpu':2,'memory_gib':4,'disk_gib':24},'cargo_jobs':4,'purpose':'approved depth source build only; actors pending artifact/input readback'},indent=2))
PYCHECK
git init --quiet source
cd source
git remote add origin git@github.com:hyper-light/shards.git
timeout 300 git fetch --depth 1 --no-tags origin "$candidate" > ../records/fetch-base.log 2>&1 9>&-
git switch --detach "$candidate" > ../records/checkout-base.log 2>&1
test "$(git rev-parse HEAD)" = "$candidate"
test -z "$(git status --porcelain)"
git rev-parse HEAD > ../records/source.txt
git show --format=fuller --no-patch HEAD > ../records/commit.txt
python3 - <<'PYPOLICY' > ../records/policy.json
from pathlib import Path
import hashlib,json
sha=hashlib.sha256(Path('docs/development-rules.md').read_bytes()).hexdigest()
assert sha=='df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3'
print(json.dumps({'canonical_main':'b57e2fcec7170cad4c2366f7462e3fa5ff3e3fbe','candidate_base':'b57e2fcec7170cad4c2366f7462e3fa5ff3e3fbe','rules_sha256':sha,'role':'implementer/assigned adversarial','new_fixture_compile_or_VM_pass':False},indent=2))
PYPOLICY
kernel="$campaign/cache/vmlinux-6.18.48-x86_64-17c3ba205400"
curl --fail --location --max-time 240 https://github.com/hyper-light/shards/releases/download/kernel-6.18.48-17c3ba205400/vmlinux-6.18.48-x86_64 --output "$kernel" > ../records/kernel-fetch.log 2>&1
printf '%s  %s\n' 48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7 "$kernel" | sha256sum --check > ../records/kernel.sha256
rustc -Vv > ../records/rustc.txt
cargo -V > ../records/cargo.txt
rustup target list --installed > ../records/installed-targets.txt
python3 -c "from pathlib import Path; assert 'x86_64-unknown-linux-musl' in Path('../records/installed-targets.txt').read_text().splitlines()"
unset SHARDS_HELPERS SHARDS_INIT_BINARY SHARDS_SERVER_BINARY SHARDS_HELPERS_DIR SHARDS_VM_BINARY SHARDS_TEST_GUESTS SHARDS_ABI_IDENTITY SHARDS_INIT
export CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR="$campaign/target"
export DOCKER_CONFIG="$campaign/records/empty-docker" XDG_CACHE_HOME="$campaign/cache"
export SHARDS_REQUIRE_VMS=1 SHARDS_KERNEL="$kernel" SHARDS_TEST_KERNEL="$kernel"
export SHARDS_TENANT_EXIT_SOURCE_REVISION="$candidate" SHARDS_TENANT_STDIO_SOURCE_REVISION="$candidate"
python3 - <<'PY' > ../records/build-environment.json
import json,os
names=['CARGO_BUILD_JOBS','CARGO_TARGET_DIR','DOCKER_CONFIG','XDG_CACHE_HOME','SHARDS_REQUIRE_VMS','SHARDS_KERNEL','SHARDS_TEST_KERNEL','SHARDS_TENANT_EXIT_SOURCE_REVISION','SHARDS_TENANT_STDIO_SOURCE_REVISION']
print(json.dumps({k:os.environ[k] for k in names},indent=2))
PY
scripts/check-development-policy > ../records/policy.log 2>&1
git diff --check > ../records/diff-check.log 2>&1
printf '%s\n' 'timeout --signal=TERM --kill-after=30s 3600s cargo test --locked -j4 --profile test-release -p shards --test managed_output --no-run --message-format=json' > ../records/compile.command
set +e
timeout --signal=TERM --kill-after=30s 3600s cargo test --locked -j4 --profile test-release -p shards --test managed_output --no-run --message-format=json > ../records/compile.jsonl 2> ../records/compile.log 9>&-
compile_status="$?"
set -e
printf '%s\n' "$compile_status" > ../records/compile.exit
if [ "$compile_status" -ne 0 ]; then tail -80 ../records/compile.log; exit "$compile_status"; fi
printf '%s\n' 'timeout --signal=TERM --kill-after=30s 1800s cargo build --locked -j4 -p shards-testguest --profile guest --target x86_64-unknown-linux-musl --target-dir source/target/guest' > ../records/testguest.command
set +e
CC_x86_64_unknown_linux_musl="$campaign/source/scripts/zig-cc" AR_x86_64_unknown_linux_musl="$campaign/source/scripts/zig-ar" ZIG_TARGET=x86_64-linux-musl timeout --signal=TERM --kill-after=30s 1800s cargo build --locked -j4 -p shards-testguest --profile guest --target x86_64-unknown-linux-musl --target-dir "$campaign/source/target/guest" > ../records/testguest.log 2>&1 9>&-
guest_status="$?"
set -e
printf '%s\n' "$guest_status" > ../records/testguest.exit
if [ "$guest_status" -ne 0 ]; then tail -80 ../records/testguest.log; exit "$guest_status"; fi
python3 - <<'PY' > ../records/artifacts.json
from pathlib import Path
import json,hashlib,os,shutil
record=Path('../records')
packets=[json.loads(line) for line in (record/'compile.jsonl').read_text().splitlines()]
bins=[v['executable'] for v in packets if v.get('reason')=='compiler-artifact' and v.get('target',{}).get('name')=='managed_output' and v.get('executable')]
assert len(bins)==1,bins
(record/'test-binary.txt').write_text(bins[0]+'\n')
dirs=[Path(v['out_dir']) for v in packets if v.get('reason')=='build-script-executed' and any(k=='SHARDS_HELPERS_CARRIED' for k,_ in v.get('env',[]))]
assert len(dirs)==1,dirs
paths=[Path(bins[0]),Path(os.environ['CARGO_TARGET_DIR'])/'test-release/shards',Path('target/guest/x86_64-unknown-linux-musl/guest/shards-testguest')]
paths.extend(dirs[0]/n for n in ['shards-init','shards-server','helpers/shards-vm','helpers/shards-net'])
entries=[{'path':str(p.resolve()),'bytes':p.stat().st_size,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()} for p in paths]
assert len(entries)==7 and all(v['bytes']>0 for v in entries),entries
frozen=Path('../frozen')/os.environ['SHARDS_TENANT_DEPTH_REVISION'][:9]
frozen.mkdir()
for p,e in zip(paths,entries):
    dest=frozen/p.name
    shutil.copy2(p,dest)
    assert hashlib.sha256(dest.read_bytes()).hexdigest()==e['sha256']
(frozen/'manifest.json').write_text(json.dumps(entries,indent=2)+'\n')
print(json.dumps(entries,indent=2))
PY
du -sk "$campaign/source" "$campaign/target" "$campaign/records" "$campaign/cache" "$campaign/frozen" > ../records/build-footprint-kib.txt
cat ../records/preflight.json ../records/build-footprint-kib.txt ../records/artifacts.json
printf '%s\n' 'matching source host+carried helpers+testguest build complete'
