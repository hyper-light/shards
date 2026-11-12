#!/bin/sh
# Each guard removed in turn, the test that holds it run, and the file put back (touched,
# so that cargo builds it again): every mutant must fail its test, for the reason named.
set -u
export HOME=/home/shards CARGO_HOME=/cargo
t() { cargo test --locked --profile test-release --target "$TARGET" "$@" 2>&1; }
mutant() {
  name=$1 file=$2 from=$3 to=$4 pkg=$5 test=$6
  cp "$file" /tmp/kept
  sed -i "s#$from#$to#" "$file"
  if cmp -s "$file" /tmp/kept; then echo "MUTANT $name: NOT APPLIED"; return; fi
  out=$(t -p "$pkg" --bin "$pkg" -- --exact "$test")
  cp /tmp/kept "$file"
  touch "$file"
  if echo "$out" | grep -q "test result: FAILED"; then
    echo "MUTANT $name: KILLED"
  else
    echo "MUTANT $name: SURVIVED"
  fi
  echo "$out" | grep -E "panicked at|helper_signal|unusable_native|the fixture|Option::unwrap|REAP_UNKNOWN|os error" | head -4
}
mutant helper-allows-no-clone crates/server/src/sandbox.rs \
  '^        libc::SYS_clone,$' '        // mutant: clone not allowed' \
  shards-server sandbox::tests::a_worker_not_reaped_ends_no_other
mutant setfl-error-ignored crates/server/src/server.rs \
  'libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {' 'libc::F_SETFL, flags | libc::O_NONBLOCK) } < -1 {' \
  shards-server server::tests::an_unusable_helper_control_is_not_admitted
mutant fixture-not-o-path crates/server/src/server.rs \
  'libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC)' 'libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC)' \
  shards-server server::tests::an_unusable_helper_control_is_not_admitted
mutant udp-same-namespace-accepted crates/testguest/src/managed_network/intercept/udp.rs \
  '"local-loopback" | "local-self" => return Err(invalid(SAME_NAMESPACE_REPLY)),' '"local-loopback" | "local-self" => {}' \
  shards-testguest managed_network::tests::udp_interception_parser_has_explicit_origins
