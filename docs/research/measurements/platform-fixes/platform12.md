# platform-12: delegated cgroup cleanup notifications

POLICY_READ: shards base `9efde72524dd626f3ff84e6b9891852d7c810a9b`; role implementer,
area platform, finding 12. AGENTS SHA-256 `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41`;
rules `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`;
evidence `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63`;
CLAUDE `588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010`.
The common and fix briefs explicitly authorize focused remote unit tests and implementer
fast-forward landings; ROOT retains real cgroups, VMs, performance and matrix gates.

## Red

Source `8d013f126695c4a9d3470b240c0b8b5c51663f27`, test
`daemon::parents::tests::a_cold_leaf_waits_for_changes_without_periodic_reads`.
On adacachy2 (Linux x86_64), kiln r4 exited 101: one test failed because inotify
observed another IN_ACCESS on an unchanged populated fixture before any modification.
The test finished in 0.02 s. [Retained failure](platform12-red.log).

The witness observes filesystem reads independently of the implementation. Its
250 ms observation window is test-only; no production duration was added. The fixture
is emptied and its scoped waiter joined before the assertion, including the red path.

## Decision and state

Fix source `ab5399eda`: replace the fixed 10 ms sleep with the process's existing shared
FileWatch. Arm both directory and file before the first read; clear pending notification
before reading; then wait for a modification/removal or the judge's next learned deadline.
Cold start has no learned deadline and therefore waits for notification without probing.
A pending modification racing the read is either reflected in that read or remains ready
for the next wait. A notification alone never means the group is empty: the subsequent
state read remains authoritative. The record still learns only a successful emptying.

Primary source: Linux v6.18 `fs/kernfs/file.c`, `kernfs_notify_workfn` lines 912–975,
especially the FS_MODIFY emission at 957–967, and `kernfs_notify` lines 985–1013.
See [pinned kernel source](https://github.com/torvalds/linux/blob/v6.18/fs/kernfs/file.c#L912).
Direct cgroup POLLPRI is another supported choice; FileWatch reuses the existing adapter,
its shared inotify owner, removal handling and ordinary-file test fixtures. No new cache,
thread, periodic duration, or inotify-instance-per-leaf is introduced.

Errors from opening, arming or waiting propagate as typed `io::Error`; an events file
missing on entry or removed while waiting yields the existing absent-leaf outcome.
Unchanged process-watch/judge error handling is not claimed repaired by this finding.

## Green and coordinator work

On source `ab5399eda`, adacachy2 (Linux x86_64), kiln r5 exited 0: the exact regression
passed (one test, 0.25 s), the parents module passed (six tests, 1.70 s), and
`cargo clippy -p shards --all-targets -- -D warnings` passed (36.68 s).
[Retained green output](platform12-green.log). The merged-tree rerun is recorded at landing.
Only test fixtures run; no real delegated cgroup or VM is created by this lane.

ROOT owes the real delegated-cgroup integration command on Linux:
`cargo test -p shards --test containers a_microvm_is_made_in_the_delegated_cgroup_its_cgroup_parent_names -- --nocapture`.
Also observe actual leaf removal after its last process exits and under directory
removal/change races; a fixture notification test does not prove the full kernel lifecycle.

## R01–R16 accounting

- R01/R04/R06/R07/R13: existing empty/absent/error outcomes retained; fallible operations
  propagate io errors, no new production unwrap/index/unchecked sizing; one notification
  owner and local loop, with comments for the lost-wakeup/source obligation.
- R02/R03/R05/R12: pinned kernel source, direct POLLPRI alternative, existing shared
  FileWatch selected; no new production literals or cache, cold wait has no timer.
- R09/R10: focused remote red and retained log above; green and merged checks are recorded
  when complete. No VM or real-cgroup reproduction is claimed.
- R08/R14: no performance/4–5x claim. Comparative throughput, allocations, faults, CPU
  and cleanup cost remain ROOT's work; removing a timer is not measured cost evidence.
- R11: only Linux x86_64 targeted checks are assigned here. macOS/Windows builds, Linux
  arm64/glibc/musl matrix and actual VM hosts remain ROOT/CI's unverified gates.
- R15/R16: no children, no E2E/chaos/benchmark; one remote cargo at a time, nice 10,
  incremental off, cold task-specific target under `~/g121-tmp/platform-fix`, removed
  after landing. No local Mac build/test. Signed commit and merge receipts accompany landing.
