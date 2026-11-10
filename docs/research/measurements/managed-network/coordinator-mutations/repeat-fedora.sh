set -eu
cd "$HOME/managed-network-native-check"
cp canonical.rs crates/testguest/src/managed_network.rs
trap 'cp canonical.rs crates/testguest/src/managed_network.rs' EXIT
cargo build --offline --release -j 4 > mutation-build.log 2>&1
binary=target/release/probe
python3 native-cases.py --binary "$binary" --out mutation-control-fixed --within-ms 120000 --revision 9d003774 --only v4-tcp-zero-deadline --only v4-udp-too-large > mutation-control-fixed.log
python3 - <<'PY'
from pathlib import Path
p=Path('crates/testguest/src/managed_network.rs');s=Path('canonical.rs').read_text()
a='        if within.is_zero() {\n            return Err(io::Error::new(io::ErrorKind::TimedOut, "probe deadline ended"));\n        }\n'
assert s.count(a)==1
p.write_text(s.replace(a,''))
PY
cargo build --offline --release -j 4 > mutation-zero-build.log 2>&1
set +e
python3 native-cases.py --binary "$binary" --out mutation-zero --within-ms 120000 --revision 9d003774-minus-zero-guard --only v4-tcp-zero-deadline > mutation-zero.log
rc=$?
set -e
printf '%s\n' "$rc" > mutation-zero.exit
python3 - <<'PY'
from pathlib import Path
import json
p=Path('mutation-zero/results.json');r=json.loads(p.read_text())['results'];assert len(r)==1 and not r[0]['passed'];assert b'Listening' in (p.parent/r[0]['stdout']).read_bytes()
p=Path('crates/testguest/src/managed_network.rs');s=Path('canonical.rs').read_text();a='    if result.is_err() {\n        let word = 1u64;';assert s.count(a)==1;p.write_text(s.replace(a,'    if false && result.is_err() {\n        let word = 1u64;'))
PY
cargo build --offline --release -j 4 > mutation-wake-build.log 2>&1
set +e
python3 native-cases.py --binary "$binary" --out mutation-wake --within-ms 120000 --revision 9d003774-minus-error-wake --only v4-udp-too-large > mutation-wake.log
rc=$?
set -e
printf '%s\n' "$rc" > mutation-wake.exit
python3 - <<'PY'
from pathlib import Path
import json
p=Path('mutation-wake/results.json');r=json.loads(p.read_text())['results'];assert len(r)==1 and not r[0]['passed'] and r[0]['status']=='parent-budget-expired',r
PY
cp canonical.rs crates/testguest/src/managed_network.rs
cargo build --offline --release -j 4 > mutation-restored-build.log 2>&1
python3 native-cases.py --binary "$binary" --out mutation-restored --within-ms 120000 --revision 9d003774 --only v4-tcp-zero-deadline --only v4-udp-too-large > mutation-restored.log
cat mutation-control-fixed.log mutation-zero.log mutation-wake.log mutation-restored.log
