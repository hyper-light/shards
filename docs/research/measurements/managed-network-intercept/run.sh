#!/usr/bin/env bash
# COORDINATOR ONLY: boots one real guest for the supplied finite IPv4 TCP origin case.
set -euo pipefail
: "${CARGO_TARGET_DIR:?coordinator private target}"
: "${SHARDS_KERNEL:?absolute experimental kernel, never production-pin fallback}"
: "${SHARDS_NETWORK_EFFECTIVE_CONFIG:?absolute effective build-output config}"
: "${SHARDS_NETWORK_CONFIG_PROVENANCE:?explicit source/build association}"
: "${SHARDS_NETWORK_RECORDS:?absolute new retained record directory}"
: "${SHARDS_NETWORK_BOOT_MS:?coordinator guest/report budget}"
: "${SHARDS_NETWORK_CASE_MS:?single original socket/setup deadline}"
: "${SHARDS_NETWORK_MEMORY_MIB:?coordinator VM memory allocation}"
: "${SHARDS_NETWORK_CPUS:?coordinator VM CPU allocation}"
: "${SHARDS_INTERCEPT_ORIGIN:?local, forward, local-loopback or local-self}"
: "${SHARDS_INTERCEPT_MARK:?decimal nonzero32-bit fixture mark}"
: "${SHARDS_INTERCEPT_TABLE:?decimal32-bit fixture table, excluding reserved IDs}"
: "${SHARDS_INTERCEPT_RULE_PRIORITY:?decimal priority before main and after local}"
export CARGO_BUILD_JOBS=4
export SHARDS_NETWORK_SOURCE_REVISION=$(git rev-parse HEAD)
python3 - <<'PY'
import hashlib,json,os,pathlib,shutil
r=pathlib.Path(os.environ['SHARDS_NETWORK_RECORDS'])
assert r.is_absolute()
r.mkdir(parents=True,exist_ok=False)
paths=['crates/testguest/src/main.rs','crates/testguest/src/linux.rs','crates/testguest/src/managed_network.rs','crates/testguest/src/managed_network/intercept.rs','crates/testguest/src/managed_network/intercept/wire.rs','crates/shards/tests/managed_network.rs']
(r/'source-sha256.json').write_text(json.dumps([dict(path=p,sha256=hashlib.sha256(pathlib.Path(p).read_bytes()).hexdigest()) for p in paths],indent=2)+'\n')
config=pathlib.Path(os.environ['SHARDS_NETWORK_EFFECTIVE_CONFIG'])
kernel=pathlib.Path(os.environ['SHARDS_KERNEL'])
assert config.is_absolute() and config.is_file() and kernel.is_absolute() and kernel.is_file()
shutil.copyfile(config,r/'supplied-effective.config')
(r/'supplied-artifacts.json').write_text(json.dumps(dict(kernel=dict(path=str(kernel),sha256=hashlib.sha256(kernel.read_bytes()).hexdigest()),config=dict(path=str(config),sha256=hashlib.sha256(config.read_bytes()).hexdigest(),provenance=os.environ['SHARDS_NETWORK_CONFIG_PROVENANCE'],association='supplied evidence; not independently inferred from inputs')),indent=2)+'\n')
(r/'inputs.json').write_text(json.dumps({key:os.environ[key] for key in os.environ if key.startswith(('SHARDS_INTERCEPT_','SHARDS_NETWORK_'))},indent=2)+'\n')
PY
git status --porcelain > "$SHARDS_NETWORK_RECORDS/source-status.txt"
git rev-parse HEAD > "$SHARDS_NETWORK_RECORDS/source.txt"
# Force actual guest rebuild after kiln synchronization even if source mtimes are old.
touch crates/testguest/src/{main.rs,linux.rs,managed_network.rs} crates/testguest/src/managed_network/{intercept.rs,intercept/wire.rs} crates/shards/tests/managed_network.rs
set +e
cargo test --locked -j4 --profile test-release -p shards --test managed_network experimental_tcp4_interception_has_distinct_backend_and_relay_observations -- --ignored --exact --nocapture --test-threads=1 > "$SHARDS_NETWORK_RECORDS/campaign.log" 2>&1
status=$?
if [ "$status" -eq 0 ]; then
    # A zero-test cargo success creates no actual guest observation file.
    python3 - <<'PY'
import json,os,pathlib
p=pathlib.Path(os.environ['SHARDS_NETWORK_RECORDS'])/('intercept-tcp4-'+os.environ['SHARDS_INTERCEPT_ORIGIN']+'.stdout')
records=[json.loads(line.removeprefix('managed-intercept ')) for line in p.read_text().splitlines() if line.startswith('managed-intercept ')]
assert sum(r.get('type')=='Intercepted' for r in records)==1, 'actual guest interception observation required'
PY
    status=$?
fi
set -e
printf '%s\n' "$status" > "$SHARDS_NETWORK_RECORDS/campaign.exit"
cat "$SHARDS_NETWORK_RECORDS/campaign.log"
exit "$status"
