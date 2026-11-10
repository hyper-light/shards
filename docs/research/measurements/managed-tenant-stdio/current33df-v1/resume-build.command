set -eu
set -o pipefail
umask 077
candidate="${SHARDS_TENANT_CAMPAIGN_REVISION:?exact signed candidate required}"
campaign="$HOME/managed-tenant-current-33df-01a1c5d2-v1"
cd "$campaign/source"
exec 9>"$campaign/.campaign.lock"
flock -n 9
test "$(git rev-parse HEAD)" = "$candidate"
test -z "$(git status --porcelain)"
test ! -e ../records/compile.exit
test ! -e ../records/compile.log
kernel="$HOME/managed-kernel-tproxy-5d23-v1/out/vmlinux-6.18.48-x86_64-managed-tproxy"
python3 - <<'PYCHECK' > ../records/resume-check.json
from pathlib import Path
import os,json
r=Path('../records')
assert 'x86_64-unknown-linux-musl' in (r/'installed-targets.txt').read_text().splitlines()
s=os.statvfs('.')
assert s.f_bavail*s.f_frsize>=40*1024**3
print(json.dumps({'prior_run':'r5','prior_exit':127,'prior_failure':'rg unavailable on runner before compile','source_unchanged':True,'compile_previously_started':False,'installed_musl_target':True,'free_bytes':s.f_bavail*s.f_frsize},indent=2))
PYCHECK
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
frozen=Path('../frozen')/os.environ['SHARDS_TENANT_CAMPAIGN_REVISION'][:9]
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
