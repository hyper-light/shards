# bash green.sh REV FILTER: the overlay's tests matching FILTER in shards-init, then the same
# run as root of subordinate IDs, each printing what its tests print.
set -u
rev=$1 filter=$2
bash apply.sh "$rev" > /dev/null || { echo "apply.sh failed"; exit 1; }
cd ~/g121-tmp/s7p2/shards
export CARGO_TARGET_DIR=$HOME/g121-tmp/s7p2/target SHARDS_HELPERS=skip
echo "host $(hostname) $(uname -sr) $(uname -m), $(date -u +%Y-%m-%dT%H:%M:%SZ), base $rev, domains.rs $(sha256sum crates/init/src/domains.rs | cut -c1-16)"
nice -n 10 cargo test -j8 -q -p shards-init -- "$filter" --nocapture 2>&1 | grep -a -v "^$" | tail -30
bin=$(ls -t $CARGO_TARGET_DIR/debug/deps/shards_init-* | grep -v '\.d$' | head -1)
echo "== as root of the subordinate IDs"
unshare --map-auto --map-root-user "$bin" "$filter" --nocapture 2>&1 | grep -a -v "^$" | tail -12
