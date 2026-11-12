#!/bin/sh
# After the observer-socket attribution rewrite (common/managed_capture.rs only): the final round
# again, then clippy -D warnings for the shards test targets. The test guest is unchanged since the
# lint round (lint-round.log).
sh docs/research/measurements/audit-fix-tests-v1/logs/final-round.sh || exit 1
. docs/research/measurements/audit-fix-tests-v1/logs/runner-prelude.sh
echo "== clippy-shards-tests"
echo '$ cargo clippy -p shards -j2 --test managed_output --test managed_network --test managed_limits --test managed_cleanup --test build -- -D warnings'
cargo clippy -p shards -j2 --test managed_output --test managed_network --test managed_limits --test managed_cleanup --test build -- -D warnings 2>&1 | grep -v '^warning: aws-lc-sys\|^    Checking \|^   Compiling '
echo "[exit $(cargo clippy -q -p shards -j2 --test managed_output --test managed_network --test managed_limits --test managed_cleanup --test build -- -D warnings >/dev/null 2>&1; echo $?)]"
