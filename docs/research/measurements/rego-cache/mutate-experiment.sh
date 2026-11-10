#!/usr/bin/env bash
set -eu
record=${1:?evidence directory}
mkdir -p "$record"
record=$(cd "$record" && pwd)
source=crates/rego/src/funcs/regex.rs
harness=docs/research/measurements/rego-cache/experiment.rs
cp "$source" "$record/regex.rs.before"
cp "$harness" "$record/experiment.rs.before"
trap 'cp "$record/regex.rs.before" "$source"; cp "$record/experiment.rs.before" "$harness"' EXIT
for mutation in kind delimiter history owner; do
    cp "$record/regex.rs.before" "$source"
    cp "$record/experiment.rs.before" "$harness"
    python3 docs/research/measurements/rego-cache/mutate-experiment.py "$mutation"
    python3 - "$record" "$mutation" "$source" "$harness" <<'PYDIFF'
import difflib,pathlib,sys
root=pathlib.Path(sys.argv[1]);mode=sys.argv[2]
with (root/(mode+'.patch')).open('w') as out:
 for before,after in [('regex.rs.before',sys.argv[3]),('experiment.rs.before',sys.argv[4])]:
  out.writelines(difflib.unified_diff((root/before).read_text().splitlines(True),pathlib.Path(after).read_text().splitlines(True),fromfile=after+'.baseline',tofile=after+'.mutated'))
PYDIFF
    sha256sum "$source" "$harness" > "$record/$mutation.source.sha256"
    touch crates/rego/src/eval.rs crates/rego/src/funcs/mod.rs "$source" "$harness"
    cargo clean -p shards-rego --profile test-release > "$record/$mutation.clean.log" 2>&1
    workload=constant_hot
    if [ "$mutation" = history ]; then workload=dynamic_template; fi
    set +e
    SHARDS_REGO_EXPERIMENT_MODE=program SHARDS_REGO_EXPERIMENT_WORKLOAD="$workload" SHARDS_REGO_EXPERIMENT_CHECKS=1 cargo test -j 4 --profile test-release -p shards-rego --lib rego_cache_experiment -- --ignored --nocapture --test-threads=1 > "$record/$mutation.log" 2>&1
    code=$?
    set -e
    printf '%s\n' "$code" > "$record/$mutation.exit"
    [ "$code" -eq 101 ]
    reason='constant compilation identity changed policy result'
    if [ "$mutation" = history ]; then reason='dynamic expression retained compilation history'; fi
    if [ "$mutation" = owner ]; then reason='program compilation owner outlived Program drop'; fi
    grep -F "$reason" "$record/$mutation.log" > "$record/$mutation.guard.log"
    grep -F 'Compiling shards-rego' "$record/$mutation.log" > "$record/$mutation.rebuild.log"
    python3 - "$record" "$mutation" <<'PYBINARY'
import hashlib,pathlib,re,shutil,sys
root=pathlib.Path(sys.argv[1]);mode=sys.argv[2]
text=(root/(mode+'.log')).read_text()
match=re.search(r'Running unittests .*?\(([^)]+)\)',text)
assert match, 'executed binary identity missing'
original=pathlib.Path(match.group(1));frozen=root/(mode+'.test')
shutil.copyfile(original,frozen)
(root/(mode+'.binary.sha256')).write_text(hashlib.sha256(frozen.read_bytes()).hexdigest()+'  '+str(frozen)+'\nexecuted='+str(original)+'\n')
PYBINARY
done
cp "$record/regex.rs.before" "$source"
cp "$record/experiment.rs.before" "$harness"
touch crates/rego/src/eval.rs crates/rego/src/funcs/mod.rs "$source" "$harness"
cargo clean -p shards-rego --profile test-release > "$record/restored.clean.log" 2>&1
SHARDS_REGO_EXPERIMENT_MODE=program SHARDS_REGO_EXPERIMENT_WORKLOAD=dynamic_template SHARDS_REGO_EXPERIMENT_CHECKS=1 cargo test -j 4 --profile test-release -p shards-rego --lib rego_cache_experiment -- --ignored --nocapture --test-threads=1 > "$record/restored.log" 2>&1
printf '0\n' > "$record/restored.exit"
