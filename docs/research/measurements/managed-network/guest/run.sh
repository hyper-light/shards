#!/usr/bin/env bash
# Coordinator only: this command boots real VMs; implementers do not execute it.
set -eu
: "${CARGO_TARGET_DIR:?coordinator private target}"
: "${SHARDS_KERNEL:?absolute experimental kernel; production pin is never selected here}"
: "${SHARDS_NETWORK_EFFECTIVE_CONFIG:?absolute uncompressed build-output config evidence}"
: "${SHARDS_NETWORK_CONFIG_PROVENANCE:?explicit source/build identity for supplied config}"
: "${SHARDS_NETWORK_RECORDS:?absolute retained evidence directory}"
: "${SHARDS_NETWORK_BOOT_MS:?coordinator liveness budget}"
: "${SHARDS_NETWORK_CASE_MS:?coordinator socket-operation budget}"
: "${SHARDS_NETWORK_MEMORY_MIB:?coordinator guest memory allocation}"
: "${SHARDS_NETWORK_CPUS:?coordinator guest CPU allocation}"
export CARGO_BUILD_JOBS=4
export SHARDS_NETWORK_SOURCE_REVISION=$(git rev-parse HEAD)
python3 -c 'import pathlib,sys; pathlib.Path(sys.argv[1]).mkdir(parents=True,exist_ok=False)' "$SHARDS_NETWORK_RECORDS"
git status --porcelain > "$SHARDS_NETWORK_RECORDS/source-status.txt"
git rev-parse HEAD > "$SHARDS_NETWORK_RECORDS/source.txt"
python3 - "$SHARDS_NETWORK_RECORDS" <<'PY'
from pathlib import Path
import hashlib,json,sys,os,shutil
files=['crates/testguest/src/main.rs','crates/testguest/src/linux.rs','crates/testguest/src/managed_network.rs','crates/shards/tests/managed_network.rs']
rows=[{'path':p,'sha256':hashlib.sha256(Path(p).read_bytes()).hexdigest()} for p in files]
records=Path(sys.argv[1])
(records/'source-sha256.json').write_text(json.dumps(rows,indent=2)+'\n')
config=Path(os.environ['SHARDS_NETWORK_EFFECTIVE_CONFIG'])
assert config.is_absolute() and config.is_file(), 'absolute supplied config required'
shutil.copyfile(config,records/'supplied-effective.config')
(records/'supplied-config.json').write_text(json.dumps({'path':str(config),'sha256':hashlib.sha256(config.read_bytes()).hexdigest(),'provenance':os.environ['SHARDS_NETWORK_CONFIG_PROVENANCE'],'association_with_booted_kernel':'coordinator-supplied, not independently verified'},indent=2)+'\n')
PY
touch crates/testguest/src/main.rs crates/testguest/src/linux.rs crates/testguest/src/managed_network.rs crates/shards/tests/managed_network.rs
set +e
cargo test --locked -j4 --profile test-release -p shards --test managed_network direct_guest_ -- --ignored --nocapture --test-threads=1 > "$SHARDS_NETWORK_RECORDS/campaign.log" 2>&1
code=$?
set -e
printf '%s\n' "$code" > "$SHARDS_NETWORK_RECORDS/campaign.exit"
cat "$SHARDS_NETWORK_RECORDS/campaign.log"
exit "$code"
