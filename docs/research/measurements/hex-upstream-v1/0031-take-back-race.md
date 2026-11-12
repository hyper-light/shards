# Hex patch 0031: the take-back tests accept the reap between the pidfd's open and the stat's

**Defect.** `attach_takes_back_a_guest_whose_handle_was_lost` and `…_whose_client_crashed`
(crates/api/src/client.rs) stop a taken-back guest, wait for its end, and attach again, which
must be refused. On Linux `Held::find` opens the process's pidfd, then `/proc/<pid>/stat`, then
reads it; the client's own watcher reaps the stopped process meanwhile. The tests accepted the
refusal of the reap landing before the pidfd's open, the exit's registration or the stat's read
(ESRCH), or after (`Exited`), not between the pidfd's open and the stat's, where procfs's lookup
finds no entry (ENOENT) or its permission check of an entry still cached finds no task (ESRCH,
fs/proc/base.c `proc_pid_permission`). Production's refusal, `Process { failure: Os { OpenStat,
… } }`, is the same typed fact as the others (the process is gone) and is unchanged.

**Fix (Hex's 0031, adapted).** Both tests also accept `Os { OpenStat, ENOENT | ESRCH }`. Hex
accepts ENOENT alone; ESRCH was seen too, below.

**Live runs (R09/R10).** Whole shards-api lib, libtest's default threads:

- kiln-a094a12d (8fadc1ec), 2026-11-11, during `reader-isolation.sh reader-0030 30`'s 90 runs
  (base 9f3c91da, the take-back tests as committed in all three builds): the handle-was-lost test
  failed 24 times, refused `Os { OpenStat, code: 2 }` (ENOENT) 21 times and
  `Os { OpenStat, code: 3 }` (ESRCH) 3 times (green-run10.log among them).
- adacachy (451397fc, kernel 7.2.9-2-cachyos), 2026-11-11, `take-back-race.sh take-back-0031 150`
  (base 03786682):

| Build | client.rs | Runs whose take-back test failed | Refusals seen |
|---|---|---|---|
| red: as committed | HEAD's | 13 of 150 | `Os { OpenStat, code: 2 }` 13 |
| green: OpenStat ENOENT or ESRCH accepted | working tree's | 0 of 150 | none |
| enoent-only: Hex's 0031 as written | green less ESRCH | 0 of 150 | none (ESRCH did not occur on this host in 150) |

The green build's 2 failed runs were both
`client::borrowed_tests::contract::zero_patience_attempts_one_request_and_does_not_wait_for_accept_or_reply`,
the separate flake 0030's record names. `client_crashed` did not fail in these 540 runs; it
accepts the same refusals, as Hex's 0031 has it.

**Raw.** kiln-a094a12d `~/g121-tmp/hex-upstream-v1/archive/reader-0030-full.tgz` (the 90 runs);
adacachy `~/g121-tmp/hex-upstream-v1/take-back-0031/` (identity, each build's failures and
refusals, the logs of the runs that failed).
