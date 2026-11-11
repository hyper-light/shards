# Returned output (kiln run on 451397, cwd synced from this worktree)

## RED — `cargo test --locked -p shards-ipc --lib --target x86_64-unknown-linux-gnu -j2 locked_region -- --nocapture --test-threads=1 | tail -40`
```
HEAD f18fd14d75ac5988e5c31ad8df9c866a4bdb3ec7
 M crates/ipc/src/locked_region/tests.rs
02fe880924da4941251aea582136406529d027808cfec6a2211aafbc93fc8b1b  crates/ipc/src/locked_region.rs
3f05d75cb6a04fd0bb82cd67010c8b73838d9222031cd0502614df80f9305217  crates/ipc/src/locked_region/tests.rs
running 4 tests
test locked_region::tests::acquired_region_is_initialized_locked_writable_and_explicitly_released ... REGION usable=22 mapped=4096 page=4096 locked/resident observed; explicit unmap then mincore ENOMEM
ok
test locked_region::tests::region_fork_guard_prevents_mapping_inheritance_and_reaps_the_probe ... REGION_CHILD process_refused=1 mapping_absent=1 pidfd_ready=true waited=true ECHILD=true
ok
test locked_region::tests::region_guard_holds_for_a_pid_one_child_in_a_new_pid_namespace ... REGION_PIDNS report=[0, 1, 1, 0, 1, 0]
thread 'locked_region::tests::region_guard_holds_for_a_pid_one_child_in_a_new_pid_namespace' (1634005) panicked at crates/ipc/src/locked_region/tests.rs:313:5:
assertion `left == right` failed: a PID 1 child in a new namespace borrowed the excluded region: [0, 1, 1, 0, 1, 0]
  left: 0
 right: 1
FAILED
test locked_region::tests::unrepresentable_region_refuses_before_mapping_or_locking ... ok
test result: FAILED. 3 passed; 1 failed; 0 ignored; 0 measured; 59 filtered out; finished in 0.00s
RED-EXIT 101
```

## GREEN (pre-format tests b64ddfa0…, production ccc4ffd4…), filtered `^(test |REGION|thread|  left|assertion|test result)|SKIP`
```
test locked_region::tests::acquired_region_is_initialized_locked_writable_and_explicitly_released ... REGION usable=22 mapped=4096 page=4096 locked/resident observed; owner page wf; explicit unmap then mincore ENOMEM
test locked_region::tests::region_fork_guard_prevents_mapping_inheritance_and_reaps_the_probe ... REGION_CHILD process_refused=1 mapping_absent=1 pidfd_ready=true waited=true ECHILD=true
test locked_region::tests::region_guard_holds_for_a_pid_one_child_in_a_new_pid_namespace ... REGION_PIDNS report=[0, 1, 1, 1, 1, 1]
test locked_region::tests::unrepresentable_region_refuses_before_mapping_or_locking ... ok
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 59 filtered out; finished in 0.00s
GREEN-EXIT 0
CLIPPY-EXIT 0; CHECK x86_64-unknown-linux-musl 0; CHECK aarch64-unknown-linux-musl 0
```
(fmt then reflowed only the new test code: tests b76c36b2…; production unchanged.)

## MUTATION (address() guard replaced by `if true`, mutant hash 0431954a…) then RESTORE, final tree
```
ccc4ffd480ef39389d62855ab744861fe701d05006905ed1dbc0d66c7f4be942  crates/ipc/src/locked_region.rs
b76c36b2f811e89da492d4e723551d09af00186425d3e308010f10128f99906a  crates/ipc/src/locked_region/tests.rs
== MUTANT (guard removed) 0431954a114b2ba3
test ...region_fork_guard_prevents_mapping_inheritance_and_reaps_the_probe ... REGION_CHILD process_refused=0 mapping_absent=1 pidfd_ready=true waited=true ECHILD=true
assertion `left == right` failed: raw mincore observed a region inherited by the child
  left: [0, 1]
test ...region_guard_holds_for_a_pid_one_child_in_a_new_pid_namespace ... REGION_PIDNS report=[0, 1, 1, 0, 1, 0]
assertion `left == right` failed: a PID 1 child in a new namespace borrowed the excluded region: [0, 1, 1, 0, 1, 0]
  left: 0
test result: FAILED. 2 passed; 2 failed; 0 ignored; 0 measured; 59 filtered out; finished in 0.00s
MUTANT-EXIT 101
== restored ccc4ffd480ef39389d62855ab744861fe701d05006905ed1dbc0d66c7f4be942
test ...REGION_PIDNS report=[0, 1, 1, 1, 1, 1]
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 59 filtered out; finished in 0.00s
GREEN2-EXIT 0
FMT-EXIT 0
CLIPPY-EXIT 0
CLIPPY-x86_64-unknown-linux-musl-EXIT 0
CLIPPY-aarch64-unknown-linux-musl-EXIT 0
```
