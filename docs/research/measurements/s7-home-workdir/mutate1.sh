# Item 1's guard-removal runs: each variant mutants1.py wrote under mut1/<name>/ laid over the
# change (apply.sh's overlay at REV), then the test that must fail for it; then green.
set -u
rev=$1
here=$PWD
export CARGO_TARGET_DIR=$HOME/g121-tmp/s7p2/target SHARDS_HELPERS=skip SHARDS_INIT_BINARY=$HOME/g121-tmp/fake/init SHARDS_SERVER_BINARY=$HOME/g121-tmp/fake/server
echo "host $(hostname) $(uname -sr) $(uname -m), $(date -u +%Y-%m-%dT%H:%M:%SZ), base $rev"
run() {
  (cd ~/g121-tmp/s7p2/shards && nice -n 10 cargo test -j12 -q "$@" 2>&1) | grep -a -E "^test .*FAILED|panicked|test result|^error" | head -6
}
for m in no-image-env:init:domains::tests::a_members_environment image-home:init:domains::tests::a_members_environment path-not-its-own:init:domains::tests::a_members_program no-build-check:agentfile:an_image_with_members_never no-daemon-entries:shards:agentfile::tests::the_images_environment; do
  name=${m%%:*}; rest=${m#*:}; crate=${rest%%:*}; filter=${rest#*:}
  bash apply.sh "$rev" > /dev/null || { echo "apply failed"; exit 1; }
  (cd "$here/mut1/$name" && find . -type f | sed 's|^\./||') | while read -r p; do cp "$here/mut1/$name/$p" ~/g121-tmp/s7p2/shards/"$p"; done
  echo "== $name"
  case $crate in
    init) run -p shards-init -- "$filter" ;;
    agentfile) run -p shards-agentfile --test agentfile "$filter" ;;
    shards) run -p shards --bin shards -- "$filter" ;;
  esac
done
bash apply.sh "$rev" > /dev/null
echo "== green"
run -p shards-init -- domains::tests::a_members
run -p shards-agentfile --test agentfile an_image_with_members_never
run -p shards --bin shards -- agentfile::tests::the_images_environment
