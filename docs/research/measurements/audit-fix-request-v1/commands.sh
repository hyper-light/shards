# The commands of this record, verbatim, in the order run. Not a script to run whole: the
# source differs between steps (see each step's diff). Each ran through kiln's `run` tool:
#   on: 451397fcc585f7f7cb9aaf00e42064bb (adacachy), needs: cpus=2,mem=2g,disk=4g,
#   cwd: this worktree, synced to
#   /home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/audit-fix-request-v1-b6a45b8bfb
#   timeout_s: 600 (120 for the probe, 300 for the mode check), cargo -j2.
# Every cargo command ran with:
export SHARDS_HELPERS=skip SHARDS_INIT_BINARY=$PWD/target/placeholders/shards-init SHARDS_SERVER_BINARY=$PWD/target/placeholders/shards-server CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0

# probe.log
uname -srm; nproc; df -h . | tail -1; pwd; command -v cargo rustup; rustup show active-toolchain 2>&1 | head -3; ls -d target 2>/dev/null && du -sh target 2>/dev/null; echo "CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-unset} RUSTC_WRAPPER=${RUSTC_WRAPPER:-unset}"; head -2 /etc/os-release; ldd --version 2>&1 | head -1; command -v cc gcc clang cmake; git log -1 --format=%H; git status --short | head

# build-red.log (source: bff3015 + red-source.diff); background run r343
set -o pipefail; mkdir -p target/placeholders && printf 'placeholder\n' > target/placeholders/shards-init && printf 'placeholder\n' > target/placeholders/shards-server; export SHARDS_HELPERS=skip SHARDS_INIT_BINARY=$PWD/target/placeholders/shards-init SHARDS_SERVER_BINARY=$PWD/target/placeholders/shards-server CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0; rustc -vV | grep -E '^(host|release|commit-hash)'; date -u +%FT%TZ; time cargo test -p shards --bin shards -j2 --no-run 2>&1 | tail -25

# red.log (source: bff3015 + red-source.diff)
ls -ld target; stat -c '%h %i %y' target/debug/deps/shards-18bc4ad81ca6886a 2>&1; git log -1 --format=%H; git diff --stat; sha256sum crates/shards/src/daemon.rs; date -u +%FT%TZ; cargo test -p shards --bin shards -j2 -- daemon::tests::a_request_written_partway_is_not_kept --exact --nocapture 2>&1 | tail -40; echo "exit: ${PIPESTATUS[0]}"

# green.log (bff3015 + green-source.diff), mutation.log (that + mutation.diff)
git log -1 --format=%H; sha256sum crates/shards/src/daemon.rs; date -u +%FT%TZ; cargo test -p shards --bin shards -j2 -- daemon::tests::a_request_written_partway_is_not_kept --exact --nocapture 2>&1 | grep -v '^warning: aws-lc-sys' | tail -30; echo "exit: ${PIPESTATUS[0]}"

# final-fmt-test.log (bff3015 + green-source.diff, restored after the mutation)
git log -1 --format=%H; sha256sum crates/shards/src/daemon.rs; date -u +%FT%TZ; echo '$ cargo fmt --check'; cargo fmt --check; echo "fmt exit: $?"; echo '$ cargo fmt --all --check'; cargo fmt --all --check; echo "fmt --all exit: $?"; echo '$ cargo test -p shards --bin shards -j2 -- daemon::tests::a_request_written_partway_is_not_kept --exact --nocapture'; cargo test -p shards --bin shards -j2 -- daemon::tests::a_request_written_partway_is_not_kept --exact --nocapture 2>&1 | grep -v '^warning: aws-lc-sys' | tail -20; echo "test exit: ${PIPESTATUS[0]}"

# clippy.log, first part; background run r361
git log -1 --format=%H; sha256sum crates/shards/src/daemon.rs; date -u +%FT%TZ; echo '$ cargo clippy -p shards --bin shards -j2 -- -D warnings'; cargo clippy -p shards --bin shards -j2 -- -D warnings 2>&1 | grep -v '^warning: aws-lc-sys' | grep -v '^ *Checking \|^ *Compiling ' | tail -40; echo "clippy exit: ${PIPESTATUS[0]}"; date -u +%FT%TZ

# clippy.log, second part
git log -1 --format=%H; sha256sum crates/shards/src/daemon.rs; date -u +%FT%TZ; echo '$ cargo clippy -p shards --bin shards --profile test -j2 -- -D warnings'; cargo clippy -p shards --bin shards --profile test -j2 -- -D warnings 2>&1 | grep -v '^warning: aws-lc-sys' | grep -v '^ *Checking \|^ *Compiling ' | tail -40; echo "clippy exit: ${PIPESTATUS[0]}"; date -u +%FT%TZ; du -sh target 2>/dev/null; df -h . | tail -1

# clippy.log, third part: which mode each check was, from cargo's compiler-artifact records
for p in dev test; do echo "== clippy --profile $p"; [ $p = dev ] && a="" || a="--profile test"; cargo clippy -p shards --bin shards $a -j2 --message-format=json -- -D warnings 2>/dev/null | python3 -I -c 'import json,sys
for l in sys.stdin:
    try: m=json.loads(l)
    except Exception: continue
    if m.get("reason")=="compiler-artifact" and m["target"]["name"]=="shards" and "bin" in m["target"]["kind"]:
        print({"profile_test": m["profile"]["test"], "fresh": m["fresh"], "kind": m["target"]["kind"]})
    if m.get("reason")=="compiler-message" and m["target"]["name"]=="shards":
        print("diagnostic:", m["message"]["rendered"][:300])
    if m.get("reason")=="build-finished": print("build-finished success:", m["success"])'; done

# On this Mac, no build: the formatting of daemon.rs while editing, and the policy check.
rustfmt +1.98.1 --edition 2024 --check --config-path rustfmt.toml crates/shards/src/daemon.rs
python3 -I scripts/check-development-policy
# -> Development policy checks passed; evidence/coordinator review is still required. (exit 0)
