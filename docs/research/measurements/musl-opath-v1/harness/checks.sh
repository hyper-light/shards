#!/bin/sh
# The lane's checks for $TARGET, from the checkout's root, as its owner (never root):
#   1. shards-init's and shards-server's unit tests, whole.
#   2. Probes: test modules appended to a source for one run, then taken away (probes/):
#      keep.rs, what changes::keep keeps (F_GETFL) and a layer none may read; reaper.rs,
#      the managed reaper's own loop under its filter, stopped in its wait; net.rs (x86_64,
#      where its filter is installed), the network process's poller under its filter,
#      stopped in its wait; atomics.rs, what a program's Atomics.wait comes to in a worker's
#      runtime.
#   3. Mutants: each guard taken away, its test run, the file put back and touched so that
#      cargo builds it again. A mutant is KILLED where its test fails.
# Every result is a line that starts with RESULT, PROBE or MUTANT.
set -u
h=docs/research/measurements/musl-opath-v1/harness
t() { cargo test --locked --profile test-release --target "$TARGET" "$@" 2>&1; }
said() { grep -aE "^test |test result|panicked at|STOPPED_IN_WAIT|KEPT|REAPER|NETWORK|ATOMICS_WAIT|refused syscall|not O_PATH|O_PATH asked" | head -${1:-40}; }
uname -a
echo "TARGET=$TARGET REV=${REV:-unknown}"

echo "== 1. unit tests"
for p in shards-init shards-server; do
  out=$(t -p "$p" --bin "$p" -- --test-threads 8)
  echo "$out" | grep -aE "^test result|FAILED|panicked at" | head -20
  echo "$out" | grep -aE "STOPPED_IN_WAIT"
  if echo "$out" | grep -aq "^test result: ok"; then echo "RESULT $p unit: passed"; else echo "RESULT $p unit: FAILED"; fi
done

# probe NAME FILE "PACKAGE TARGET-KIND" TEST...: each TEST run alone, in a process of its own.
probe() {
  name=$1 file=$2 which=$3
  shift 3
  cp "$file" /tmp/probe-kept.rs
  cat "$h/probes/$name" >> "$file"
  for test in "$@"; do
    # shellcheck disable=SC2086
    out=$(t $which -- --exact "$test" --nocapture --test-threads 1)
    echo "$out" | said 12
    if echo "$out" | grep -aq "^test result: ok. 1 passed"; then r=passed; else r=FAILED; fi
    echo "PROBE $name $test: $r"
  done
  cp /tmp/probe-kept.rs "$file"
  touch "$file"
}

echo "== 2. probes"
init="-p shards-init --bin shards-init"
probe keep.rs crates/init/src/changes.rs "$init" \
  changes::probe::kept_layers_are_path_descriptors changes::probe::a_layer_none_may_read_is_kept
probe reaper.rs crates/init/src/managed.rs "$init" managed::probe::the_reaper_stopped_in_its_wait_lives_on
probe atomics.rs crates/server/src/sandbox.rs "-p shards-server --bin shards-server" \
  sandbox::probe::a_program_cannot_wait_on_atomics
case $TARGET in
x86_64-*)
  probe net.rs crates/net/src/confine.rs "-p shards-net --lib" \
    confine::probe::the_network_process_stopped_in_its_wait_lives_on
  ;;
esac

# mutant NAME FILE PACKAGE SED-SCRIPT TEST...: the guard taken away by SED-SCRIPT.
mutant() {
  name=$1 file=$2 pkg=$3 script=$4
  shift 4
  cp "$file" /tmp/mutant-kept.rs
  sed -i "$script" "$file"
  if cmp -s "$file" /tmp/mutant-kept.rs; then
    echo "MUTANT $name: NOT APPLIED"
    return
  fi
  for test in "$@"; do
    out=$(t -p "$pkg" --bin "$pkg" -- --exact "$test" --nocapture --test-threads 1)
    echo "$out" | said 12
    if echo "$out" | grep -aq "^test result: ok. 1 passed"; then r=SURVIVED; else r=KILLED; fi
    echo "MUTANT $name $test: $r"
  done
  cp /tmp/mutant-kept.rs "$file"
  touch "$file"
}

echo "== 3. mutants"
# A call site back on std's OpenOptions, as before this lane.
mutant keep-through-std crates/init/src/changes.rs shards-init \
  's#^    let open = |path: &str| crate::opath::open(path, libc::O_DIRECTORY);$#    let open = |path: \&str| -> io::Result<OwnedFd> { use std::os::unix::fs::OpenOptionsExt; Ok(std::fs::OpenOptions::new().read(true).custom_flags(libc::O_PATH | libc::O_DIRECTORY).open(path)?.into()) };#' \
  opath::tests::no_source_asks_std_for_a_path_descriptor
# The helper's flags as std's open_c makes them of read(true) and custom_flags(flags | O_PATH).
mutant helper-as-std crates/init/src/opath.rs shards-init \
  's#libc::open(path.as_ptr(), flags | libc::O_PATH | libc::O_CLOEXEC)#libc::open(path.as_ptr(), libc::O_CLOEXEC | libc::O_RDONLY | ((flags | libc::O_PATH) \& !libc::O_ACCMODE))#' \
  opath::tests::what_it_opens_is_a_path_descriptor
# The code-mode filters as main had them: restart_syscall the helper's alone.
mutant restart-helper-only crates/server/src/sandbox.rs shards-server \
  '/^        libc::SYS_restart_syscall,$/d; s#^        libc::SYS_ppoll,$#        libc::SYS_ppoll, libc::SYS_restart_syscall,#' \
  sandbox::tests::a_worker_stopped_in_a_timed_wait_lives_on sandbox::tests::the_helper_stopped_in_its_poll_lives_on
# Neither filter allows it.
mutant restart-none crates/server/src/sandbox.rs shards-server \
  '/^        libc::SYS_restart_syscall,$/d' \
  sandbox::tests::a_worker_stopped_in_a_timed_wait_lives_on sandbox::tests::the_helper_stopped_in_its_poll_lives_on
echo "== done"
