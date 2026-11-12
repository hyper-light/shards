# Item 4's red, guard-removal and green runs of the new test, in the clone at REV with the
# carried overlay, each variant of domains.rs laid over it in turn; then the green test
# binary run as root of IDs past the test's own (`unshare --map-auto --map-root-user`), where
# owners are checked too.
set -u
rev=$1
here=$PWD
T=domains::tests::a_skeletons_directories_are_the_images_own
export CARGO_TARGET_DIR=$HOME/g121-tmp/s7p2/target SHARDS_HELPERS=skip
echo "host $(hostname) $(uname -sr) $(uname -m), $(date -u +%Y-%m-%dT%H:%M:%SZ), base $rev"
for v in red no-copy root-0755 green; do
  bash apply.sh "$rev" > /dev/null
  if [ "$v" != green ]; then cp "$here/mut4/$v.rs" ~/g121-tmp/s7p2/shards/crates/init/src/domains.rs; fi
  echo "== $v: domains.rs sha256 $(sha256sum ~/g121-tmp/s7p2/shards/crates/init/src/domains.rs | cut -c1-16)"
  (cd ~/g121-tmp/s7p2/shards && nice -n 10 cargo test -j8 -q -p shards-init -- "$T" --nocapture 2>&1) | grep -a -E "^test |SKIP|panicked|assertion|left|right|test result" | head -12
done
bin=$(ls -t $CARGO_TARGET_DIR/debug/deps/shards_init-* | grep -v '\.d$' | head -1)
echo "== green, as root of the subordinate IDs: $bin"
unshare --map-auto --map-root-user "$bin" "$T" --nocapture 2>&1 | grep -a -E "^test |SKIP|panicked|test result" | head -8
bash apply.sh "$rev" > /dev/null
