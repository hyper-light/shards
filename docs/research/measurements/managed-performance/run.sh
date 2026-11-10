#!/usr/bin/env bash
# Run only after kiln admits the coordinator-assigned 4 CPU / 8 GiB / 10 GiB envelope.
set -euo pipefail
: "${CARGO_TARGET_DIR:?use the lane private target}"
: "${MANAGED_EVIDENCE:?use a new evidence directory}"
: "${MANAGED_HANDSHAKE_BYTES:?explicit fixture allowance}"
: "${MANAGED_DATAGRAM_BYTES:?explicit fixture allowance}"
: "${MANAGED_RECORDS:?explicit workload}"
: "${MANAGED_BYTES:?explicit workload}"
: "${MANAGED_PATIENCE_MS:?explicit measurement deadline}"
: "${MANAGED_REPETITIONS:?explicit sample count}"
mkdir -p "$MANAGED_EVIDENCE"
git rev-parse HEAD > "$MANAGED_EVIDENCE/source.txt"
git diff --binary > "$MANAGED_EVIDENCE/source.patch"
uname -a > "$MANAGED_EVIDENCE/environment.txt"
rustc -Vv >> "$MANAGED_EVIDENCE/environment.txt"
sha256sum crates/remote/src/managed.rs crates/remote/src/net.rs docs/research/measurements/managed-performance/src/*.rs >> "$MANAGED_EVIDENCE/source.txt"
cargo test --locked --release -j4 -p shards-remote --lib --no-run --message-format=json > "$MANAGED_EVIDENCE/build.jsonl" 2> "$MANAGED_EVIDENCE/build.stderr"
python3 - "$MANAGED_EVIDENCE/build.jsonl" "$MANAGED_EVIDENCE/binary.txt" <<'PY'
import json, pathlib, sys
artifacts = [json.loads(line) for line in pathlib.Path(sys.argv[1]).read_text().splitlines()]
binaries = [a['executable'] for a in artifacts if a.get('reason') == 'compiler-artifact' and a.get('target', {}).get('name') == 'shards_remote' and a.get('executable')]
if len(binaries) != 1:
    raise SystemExit(f'expected one source-matched binary, found {len(binaries)}')
pathlib.Path(sys.argv[2]).write_text(binaries[0])
PY
binary=$(cat "$MANAGED_EVIDENCE/binary.txt")
sha256sum "$binary" >> "$MANAGED_EVIDENCE/source.txt"
for ((repeat=0; repeat<MANAGED_REPETITIONS; repeat++)); do
    # Alternate which variant is first to retain scheduler/cache-order evidence.
    variants=(endpoint shared)
    if ((repeat % 2)); then variants=(shared endpoint); fi
    for tenants in 1 2; do
        for mode in normal stall; do
            for variant in "${variants[@]}"; do
                prefix="$MANAGED_EVIDENCE/r${repeat}-t${tenants}-${mode}-${variant}"
                MANAGED_ARGS="$tenants $MANAGED_RECORDS $MANAGED_BYTES $MANAGED_PATIENCE_MS $mode $variant" \
                    /usr/bin/time -v timeout 120 "$binary" managed::performance::native_authenticated_comparison --ignored --exact --test-threads=1 --nocapture > "$prefix.tsv" 2> "$prefix.time"
            done
        done
    done
done
