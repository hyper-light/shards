#!/bin/sh
set -eu

task_root=$(git rev-parse --show-toplevel)
cd "$task_root"
task_record=docs/research/measurements/managed-otel-encoder
task_output=$task_record/restore-label-v1/raw
task_target=${OTEL_ENCODER_TARGET:-$HOME/.cache/managed-otel-encoder-restore-label-11d1}
mkdir -p "$task_output"

rustfmt --edition 2024 --check crates/init/src/metrics/otlp.rs "$task_record/fixture.rs"
lockf /tmp/g121-mac.lock cargo test --manifest-path "$task_record/Cargo.toml" \
    --target-dir "$task_target" -j 4 > "$task_output/unit.log" 2>&1
lockf /tmp/g121-mac.lock cargo clippy --manifest-path "$task_record/Cargo.toml" \
    --target-dir "$task_target" --all-targets -j 4 -- -D warnings \
    > "$task_output/clippy.log" 2>&1
lockf /tmp/g121-mac.lock cargo run --manifest-path "$task_record/Cargo.toml" \
    --target-dir "$task_target" --bin fixture -j 4 \
    > "$task_output/fixture.json" 2> "$task_output/fixture-build.log"
python3 "$task_record/check.py" < "$task_output/fixture.json" > "$task_output/json-check.json"
python3 scripts/check-development-policy > "$task_output/policy.log"
python3 - <<'PY'
from pathlib import Path
import gzip
import hashlib
import json

raw = Path("docs/research/measurements/managed-otel-encoder/restore-label-v1/raw")
source = raw / "unit.log"
original = source.read_bytes()
compressed = gzip.compress(original, mtime=0)
stored = raw / "unit.log.gz"
stored.write_bytes(compressed)
record = {
    "original": source.name,
    "original_bytes": len(original),
    "original_sha256": hashlib.sha256(original).hexdigest(),
    "stored": stored.name,
    "stored_sha256": hashlib.sha256(compressed).hexdigest(),
    "reason": "Preserve original libtest final blank line without changing raw bytes.",
}
(raw / "raw-storage.json").write_text(json.dumps(record, indent=2) + "\n")
assert gzip.decompress(stored.read_bytes()) == original
source.unlink()
PY
git diff --check
