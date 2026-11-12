# Item 3's guard-removal runs: each variant mutants3.py wrote under mut3/<name>/ laid over the
# change (apply.sh's overlay at REV), then the test that must fail for it; then green.
set -u
rev=$1
here=$PWD
export CARGO_TARGET_DIR=$HOME/g121-tmp/s7p2/target SHARDS_HELPERS=skip
echo "host $(hostname) $(uname -sr) $(uname -m), $(date -u +%Y-%m-%dT%H:%M:%SZ), base $rev"
run() {
  (cd ~/g121-tmp/s7p2/shards && nice -n 10 cargo test -j12 -q -p shards-init -- "$@" 2>&1) | grep -a -E "^test .*FAILED|panicked|test result|^error" | head -6
}
for m in shallowest-mount:domains::tests::a_members_home_lies dotdot-home:domains::tests::a_members_home_lies twice-written:staged::tests::a_file_from_before no-replace:staged::tests::a_file_from_before link-followed:staged::tests::links_in_the_way; do
  name=${m%%:*}; filter=${m#*:}
  bash apply.sh "$rev" > /dev/null || { echo "apply failed"; exit 1; }
  (cd "$here/mut3/$name" && find . -type f | sed 's|^\./||') | while read -r p; do cp "$here/mut3/$name/$p" ~/g121-tmp/s7p2/shards/"$p"; done
  echo "== $name"
  run "$filter"
done
bash apply.sh "$rev" > /dev/null
echo "== green"
run domains::tests::a_members_home staged::tests::
