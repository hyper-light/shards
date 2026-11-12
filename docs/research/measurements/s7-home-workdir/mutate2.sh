# Item 2's guard-removal runs: each variant mutants2.py wrote under mut2/<name>/ laid over the
# change (apply.sh's overlay at REV), then the test that must fail for it; then green.
set -u
rev=$1
here=$PWD
export CARGO_TARGET_DIR=$HOME/g121-tmp/s7p2/target SHARDS_HELPERS=skip SHARDS_INIT_BINARY=$HOME/g121-tmp/fake/init SHARDS_SERVER_BINARY=$HOME/g121-tmp/fake/server
echo "host $(hostname) $(uname -sr) $(uname -m), $(date -u +%Y-%m-%dT%H:%M:%SZ), base $rev"
run() {
  (cd ~/g121-tmp/s7p2/shards && nice -n 10 cargo test -j12 -q "$@" 2>&1) | grep -a -E "^test .*FAILED|panicked|test result|^error" | head -6
}
for m in relative-taken:abi:a_working_directory_is_absolute_plain_and_one twice-staged:shards:api_run::tests::staging_names_a_running_member no-client-check:api:what_the_guest_refuses_is_refused_first guest-ignores:init:staged::tests::a_working_directory_is_read_again image-workdir-escapes:init:domains::tests::a_members_own_working_directory wire-unread:api:messages_round_trip; do
  name=${m%%:*}; rest=${m#*:}; crate=${rest%%:*}; filter=${rest#*:}
  bash apply.sh "$rev" > /dev/null || { echo "apply failed"; exit 1; }
  (cd "$here/mut2/$name" && find . -type f | sed 's|^\./||') | while read -r p; do cp "$here/mut2/$name/$p" ~/g121-tmp/s7p2/shards/"$p"; done
  echo "== $name"
  case $crate in
    abi) run -p shards-abi "$filter" ;;
    api) run -p shards-api --lib "$filter" ;;
    init) run -p shards-init -- "$filter" ;;
    shards) run -p shards --bin shards -- "$filter" ;;
  esac
done
bash apply.sh "$rev" > /dev/null
echo "== green"
run -p shards-abi a_working_directory_is_absolute_plain_and_one
run -p shards-api --lib image::
run -p shards-init -- staged::tests::a_working_directory domains::tests::a_members_own_working_directory
run -p shards --bin shards -- api_run::tests::staging_names_a_running_member
