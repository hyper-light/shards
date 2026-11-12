# bash lint.sh REV PKG...: rustfmt's check of the overlay's Rust files and clippy of PKGs,
# all targets, warnings denied, on this host's target.
set -u
rev=$1; shift
bash apply.sh "$rev" > /dev/null || { echo "apply.sh failed"; exit 1; }
cd ~/g121-tmp/s7p2/shards
export CARGO_TARGET_DIR=$HOME/g121-tmp/s7p2/target SHARDS_HELPERS=skip SHARDS_INIT_BINARY=$HOME/g121-tmp/fake/init SHARDS_SERVER_BINARY=$HOME/g121-tmp/fake/server
files=$(cd ~/g121-tmp/s7p2/carrier/files && find . -name '*.rs' | sed 's|^\./||')
echo "== rustfmt --check $files"
rustfmt --edition 2024 --check $files 2>&1 | head -60; echo "rustfmt exit=${PIPESTATUS[0]}"
args=""; for p in "$@"; do args="$args -p $p"; done
echo "== cargo clippy$args --all-targets -- -D warnings"
nice -n 10 cargo clippy -j8 $args --all-targets -- -D warnings 2>&1 | grep -a -E "^(warning|error)|^ *--> |^ *= |^[0-9]+ \|" | head -80; echo "clippy exit=${PIPESTATUS[0]}"
