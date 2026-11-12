# The commands of this record, verbatim, in the order run. Not a script to run whole: the
# source differs between steps (each step's diff is beside this file). Each build and test ran
# through kiln's `run` tool on 8fadc1ec213332e16cb0d915a3fb913f (kiln-a094a12d.attlocal.net),
# needs cpus=4,mem=16g,disk=40g, cwd this worktree, synced to
# /home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/request-sites-v1-1548cdb1b8.
# Nothing was built on the Mac.
#
# Runner sessions before the first build (no build ran in any):
#  - a probe, sync false: amphora-bench none; Linux 6.19.10-300.fc44.x86_64; 32 CPUs; zig 0.16.0.
#  - r527, r532 and one sync-only probe on 8fadc1ec: no command ran ("interrupted: the sync's
#    link closed", "did not answer the sync in 60s"). Their cwd was not given, so kiln synced
#    the session's folder, the g121 worktree, not this one.
#  - one sync-only probe on 451397fcc585f7f7cb9aaf00e42064bb (adacachy), cwd not given: it
#    synced the g121 worktree (e5b787503, clean) to
#    /home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/g121-5dc8e2cd12 (files_sent
#    1294, removed 243, 991 MB) and ran `pwd; git log -1; git status; sha256sum; uptime`.
#    A mistake: that folder is the g121 session's copy. Reported to ROOT.
#  - one sync-only probe on 8fadc1ec with cwd this worktree: synced (16760 files, 1.27 GB,
#    139 s); HEAD 032b63b1c.
#
# Every build and test command ran with this preamble:
set -o pipefail
if pgrep -x amphora-bench >/dev/null; then echo "amphora-bench is running: held"; exit 3; fi
export PATH=$HOME/.local/share/shards-zig/zig-x86_64-linux-0.16.0:$PATH TMPDIR=$HOME/g121-tmp
mkdir -p "$TMPDIR" target/placeholders && printf 'placeholder\n' > target/placeholders/shards-init && printf 'placeholder\n' > target/placeholders/shards-server
export SHARDS_HELPERS=skip SHARDS_INIT_BINARY=$PWD/target/placeholders/shards-init SHARDS_SERVER_BINARY=$PWD/target/placeholders/shards-server CARGO_BUILD_JOBS=4 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0

# red-first.log (r557; source: 032b63b1c + red-first-source.diff)
echo "== probe"; uname -srm; nproc; df -h . | tail -1; pwd; ldd --version 2>&1 | head -1; zig version; rustc -vV | grep -E '^(host|release|commit-hash)'; git log -1 --format=%H; git status --short; sha256sum crates/shards/src/daemon.rs; date -u +%FT%TZ
echo "== red: cargo test -p shards --bin shards -j4 (the three tests)"
time systemd-run --user --scope -q -p MemoryMax=24G cargo test -p shards --bin shards -j4 -- --exact daemon::tests::a_record_written_partway_is_not_kept daemon::tests::an_update_written_partway_keeps_the_request_it_had daemon::tests::a_request_kept_at_once_is_whole --nocapture 2>&1 | grep -v '^warning: aws-lc-sys' | tail -150
echo "exit: ${PIPESTATUS[0]}"; date -u +%FT%TZ

# red.log (source: 032b63b1c + red-source.diff) and green.log (032b63b1c + the green source as
# tested, daemon.rs 84c850da; tested-to-committed.diff is what the committed source adds)
git log -1 --format=%H; git status --short; sha256sum crates/shards/src/daemon.rs; date -u +%FT%TZ    # green.log: the five sources' sha256
echo '$ cargo test -p shards --bin shards -j4 -- --exact (the three tests) --nocapture'
systemd-run --user --scope -q -p MemoryMax=24G cargo test -p shards --bin shards -j4 -- --exact daemon::tests::a_record_written_partway_is_not_kept daemon::tests::an_update_written_partway_keeps_the_request_it_had daemon::tests::a_request_kept_at_once_is_whole --nocapture 2>&1 | grep -v '^warning: aws-lc-sys' | tail -80    # green.log: tail -60
echo "exit: ${PIPESTATUS[0]}"; date -u +%FT%TZ    # green.log also: ls -A "$TMPDIR/shards-tests" | head

# one-name.log (green as tested + one-name.diff)
git log -1 --format=%H; sha256sum crates/shards/src/daemon.rs; date -u +%FT%TZ
for i in 1 2 3; do
echo "== one name, run $i: cargo test -p shards --bin shards -j4 -- --exact (the three tests) --nocapture"
systemd-run --user --scope -q -p MemoryMax=24G cargo test -p shards --bin shards -j4 -- --exact daemon::tests::a_record_written_partway_is_not_kept daemon::tests::an_update_written_partway_keeps_the_request_it_had daemon::tests::a_request_kept_at_once_is_whole --nocapture 2>&1 | grep -v '^warning: aws-lc-sys' | grep -v '^ *Compiling \|^ *Finished \|^ *Running \|^note: run with' | tail -30
echo "exit: ${PIPESTATUS[0]}"
done; date -u +%FT%TZ

# mutation-guard.log (green as tested + mutation-guard.diff), mutation-site.log (green as
# tested + mutation-site.diff)
git log -1 --format=%H; sha256sum crates/shards/src/daemon.rs crates/shards/src/daemon/update.rs; date -u +%FT%TZ
echo '$ cargo test -p shards --bin shards -j4 -- --exact (the three tests) --nocapture'
systemd-run --user --scope -q -p MemoryMax=24G cargo test -p shards --bin shards -j4 -- --exact daemon::tests::a_record_written_partway_is_not_kept daemon::tests::an_update_written_partway_keeps_the_request_it_had daemon::tests::a_request_kept_at_once_is_whole --nocapture 2>&1 | grep -v '^warning: aws-lc-sys' | grep -v '^ *Compiling \|^note: run with' | tail -60
echo "exit: ${PIPESTATUS[0]}"; date -u +%FT%TZ

# gates-first.log (r610; daemon.rs 6012c32b) and gates.log (r630; the committed source,
# daemon.rs 8b5b5c72). r610 ran the same less the three tests, the daemon:: filter
# (`| grep -v ' \.\.\. ok$' | tail -80`) and the profile check.
git log -1 --format=%H; git status --short; sha256sum crates/shards/src/daemon.rs crates/shards/src/daemon/update.rs crates/shards/src/daemon/networks.rs crates/shards/src/daemon/engine/own_networks.rs crates/shards/src/daemon/remote.rs; date -u +%FT%TZ
echo '$ cargo fmt --all --check'; cargo fmt --all --check; echo "fmt exit: $?"
echo '$ cargo test -p shards --bin shards -j4 -- --exact (the three tests) --nocapture'
systemd-run --user --scope -q -p MemoryMax=24G cargo test -p shards --bin shards -j4 -- --exact daemon::tests::a_record_written_partway_is_not_kept daemon::tests::an_update_written_partway_keeps_the_request_it_had daemon::tests::a_request_kept_at_once_is_whole --nocapture 2>&1 | grep -v '^warning: aws-lc-sys' | grep -v '^ *Compiling ' | tail -20
echo "three exit: ${PIPESTATUS[0]}"; date -u +%FT%TZ
echo '$ cargo test -p shards --bin shards -j4 -- daemon::'
time systemd-run --user --scope -q -p MemoryMax=24G cargo test -p shards --bin shards -j4 -- daemon:: 2>&1 | grep -v '^warning: aws-lc-sys' | grep -v '^ *Compiling ' | grep -E '^test |^test result|^failures|^---- |panicked|FAILED|running [0-9]+ tests|Running ' | grep -v ' \.\.\. ok$' | tail -60
echo "daemon:: exit: ${PIPESTATUS[0]}"; date -u +%FT%TZ
echo '$ cargo clippy -p shards --bin shards -j4 -- -D warnings'
systemd-run --user --scope -q -p MemoryMax=24G cargo clippy -p shards --bin shards -j4 -- -D warnings 2>&1 | grep -v '^warning: aws-lc-sys' | grep -v '^ *Checking \|^ *Compiling ' | tail -40
echo "clippy exit: ${PIPESTATUS[0]}"; date -u +%FT%TZ
echo '$ cargo clippy -p shards --bin shards --profile test -j4 -- -D warnings'
systemd-run --user --scope -q -p MemoryMax=24G cargo clippy -p shards --bin shards --profile test -j4 -- -D warnings 2>&1 | grep -v '^warning: aws-lc-sys' | grep -v '^ *Checking \|^ *Compiling ' | tail -40
echo "clippy (test profile) exit: ${PIPESTATUS[0]}"; date -u +%FT%TZ
echo '== which mode each clippy check was, from cargo'"'"'s compiler-artifact records'
for p in dev test; do echo "== clippy --profile $p"; [ $p = dev ] && a="" || a="--profile test"; systemd-run --user --scope -q -p MemoryMax=24G cargo clippy -p shards --bin shards $a -j4 --message-format=json -- -D warnings 2>/dev/null | python3 -I -c 'import json,sys
for l in sys.stdin:
    try: m=json.loads(l)
    except Exception: continue
    if m.get("reason")=="compiler-artifact" and m["target"]["name"]=="shards" and "bin" in m["target"]["kind"]:
        print({"profile_test": m["profile"]["test"], "fresh": m["fresh"], "kind": m["target"]["kind"]})
    if m.get("reason")=="compiler-message" and m["target"]["name"]=="shards":
        print("diagnostic:", m["message"]["rendered"][:300])
    if m.get("reason")=="build-finished": print("build-finished success:", m["success"])'; done
du -sh target 2>/dev/null; df -h . | tail -1; ls -A "$TMPDIR/shards-tests" 2>/dev/null | head

# On this Mac, no build: formatting while editing, and the policy check.
rustup run 1.98.1 rustfmt --edition 2024 --check --config-path rustfmt.toml crates/shards/src/daemon.rs
python3 -I scripts/check-development-policy
# -> Development policy checks passed; evidence/coordinator review is still required. (exit 0)
