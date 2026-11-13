# Was CI's `docker rm -f` timeout on Windows the signal link's lost SIGKILL? (PM M538)

ROOT's question (session g121-46, 2026-11-12): `engine.rs`'s
`the_docker_cli_checkpoints_and_restores_microvms` once timed out in its last `rm -f` on
windows-2025. Its restored microVM's command never ended after the SIGKILL. The signal link's
fix (3607aad11, on main since 1c349a632) made a frame that cannot reach the guest a typed
loss, and a SIGKILL refused for one stops the machine. Was the timeout that bug? If it no
longer fails and the log fits, it is closed as that bug, and the D217 change kept on
`p/win-ci-green-teardown-wip` (3f5698f5a) stays unmerged. If it still fails on main, it is
reproduced first. Implementer lane `p/win-ci-green-v1`, policy `docs/development-rules.md`
SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`. No code changes.

## The failure

CI job 131325553441 (c01aeb941, windows-2025-vs2026, 2026-11-12):
`"C:\Windows\system32\docker.exe" "rm" "-f" "cp" did not exit within 600s`.

- The test runs `counter 20` (a line every 20 ms), checkpoints it, starts it from the
  checkpoint, waits for 5 more counts, and removes it. It reached `rm -f` about 6 s after it
  began: its 60-second notice came at 17:41:46, and its failure, 600 s after `rm -f`, at
  17:50:52. The restored counter's output came through, so the restored guest had dialed its
  run connection again and was read.
- The test's home kept one log, `daemon.log` ([ci/job-131325553441-daemon.log](ci/job-131325553441-daemon.log)).
  The daemon's VM processes write their own lines into it, so there is no VM log beside it.
  After its 3 opening lines, every line is D217's, every 10 s from 10 s to 590 s after the
  SIGKILL: `its command not yet ended … after SIGKILL, its microVM going on with its end`.
  Each one's CPU time grew by 109 to 375 ms in its 10 s, against the 63 ms of an idle
  microVM (`IDLE_VM_CPU`, PM M357), so D217 waited another slice each time. Its page faults
  grew by 0 to 305.
- So the restored machine went on running for 600 s after the SIGKILL, much as a counter
  printing every 20 ms runs. The SIGKILL did not end the command.

## How a restored run's SIGKILL could be lost before the fix

At aeac12d67 (the last main before the fix), `crates/shards/src/workload.rs` dropped a
restored run's frames without a word, the SIGKILL included, in three ways:

1. **A dial it could not take.** The restored guest's dial of the signal port arrived as an
   error. The `signal-conn` thread (`again.recv().ok().and_then(Result::ok)`) returned with no
   connection. Every frame after that was queued for ever, and `to_guest` answered "sent".
2. **A queued frame's failed send** as the dial was taken. The connection was dropped, and
   later frames were queued for ever.
3. **A later failed send.** `to_guest` set the connection to none and answered "sent". Every
   frame after that was queued for ever.

Since 1c349a632, each of these is a typed loss (`Lost::Untaken`, or `Unsent`/`Lost`). A
daemon's SIGKILL refused for it goes through `warm.rs`'s `kill_command`. That logs
`SIGKILL for its command, which no signal reaches (…): its microVM stops` and stops the
machine (`vm_run::stop`).

One way stays as it was, by design (D16, D176). A restored guest's frames wait for its dial
again with no time bound. A guest that dialed its run connection again but never its signal
connection would show CI's symptom on main too. In the guest, `redial` (crates/init/src/run.rs)
dials the run connection and then the signal connection. If the signal dial fails, it gives
up the run connection as well, so the run's output would stop. CI's restored run went on
printing, which tells against that way, as far as the guest saw. Way 1 above is about the
host's take of that dial.

## What the failing run's log can show

None of the three. Before the fix, the VM process logged no loss, no failed send, no dial
and no redial. A loss or a redial on the restored VM can neither be found nor ruled out
in this log.

## How often: CI

[tally.sh](tally.sh) lists the test's result in every completed x86_64 Windows job of CI on
main's last 200 commits (2026-11-12 08:05Z to 2026-11-13 00:24Z). The table is
[ci/tally.tsv](ci/tally.tsv), with one GraphQL query per 100 commits and one REST call per
job log. Of 139 completed jobs:

| | windows-2025-vs2026 | windows-2022 |
|---|---|---|
| ran the test (all before the fix) | 28: 27 ok, 1 timed out | 30: 30 ok |
| never reached `engine.rs` (its tests failed before it) | 39 | 41 |

One log was not available. The failure rate is 1 in 28 on windows-2025, a 95% interval of
0.09% to 18% (Clopper-Pearson), and 0 in 30 on windows-2022.

After the fix there are no results yet. At 01:10Z on 2026-11-13, all 60 x86_64 Windows jobs
for main's commits with the fix were queued, behind 53 queued and 12 running for commits
without it, and the 4 that had completed were cancelled.
At the rate before the fix, about 83 windows-2025 runs without a failure would be needed to
say at 95% that the rate fell (27/28 to the 83rd power is 0.049). CI runs one windows-2025
job per main commit.

## On jupiter

jupiter: Windows 11 10.0.26200, AMD Ryzen AI Max+ 395, WHP. The runs were held to CPUs 0-3
by `start /affinity F`, with other agents' work beside them. The A/B alternated rounds, with
`engine.rs` run whole each time:

- aeac12d67 (before the fix) against 9e5e5f8ea (after), using [jupiter/keeplog.patch](jupiter/keeplog.patch).
  It times `rm -f` and keeps the home's `daemon.log`. There were 5 rounds
  ([jupiter/ab.sh](jupiter/ab.sh), logs `jupiter/ab-*.log`).
- 3 more rounds with [jupiter/keeplog-exec.patch](jupiter/keeplog-exec.patch). It also runs
  an exec into the restored microVM before `rm -f`. The third round's after-fix run was cut
  short as the lane changed hands ([jupiter/ab2.sh](jupiter/ab2.sh), logs `jupiter/ab2-*.log`).

[jupiter/build2.sh](jupiter/build2.sh) and [jupiter/run2.sh](jupiter/run2.sh) build and run
the tests.

Results:

- Before the fix, 8 of 8 passed, with `rm -f` taking 0.12 to 8.13 s. After the fix, 7 of 7
  passed, in 0.10 to 13.06 s. Neither side timed out. jupiter does not reproduce CI's
  failure, so it cannot compare the fix.
- After the fix, 4 of the 7 logs say, from a VM process,
  `the guest's signal connection is lost: the guest closed it before its end`.
  - That VM is not the restored one. Each of these runs would otherwise have logged
    `kill_command`'s "no signal reaches" at `rm -f`, and none of the 7 did.
  - It is not a pool VM either, since one with no workload has no link to lose.
  - So it is the checkpointed original, whose VM process ends with its save.
  - Before the fix, nothing read that connection, so nothing could say so. This looks like a
    loss reported as the saved machine ends, but it is inferred from the above, not observed.
- One run after the fix, `ab2-post-2` with the exec, is unlike the rest.
  - The restored VM 91884's command had not ended 10 s after `rm -f`'s SIGKILL, and its VM
    ran less than an idle microVM through that slice. So D217 killed it: `its microVM gone on
    with nothing of its end for 10s: its microVM killed`, then `VM 91884 ended before its
    command did`. `rm -f` took 11.1 s and passed.
  - The exec just before had worked, so the restored signal link was up then. A counter
    printing every 20 ms runs more than an idle microVM does. So either the SIGKILL ended it
    and its end never reached the daemon, or the restored guest had stopped altogether.
  - The console and home went with the test. It was seen once in 7 runs after the fix, and
    in none of the 8 before.

## Verdict

**Consistent, not shown.**

- CI's symptom is a restored run's SIGKILL that never reached its command. Each of the
  three ways the fix closed produces exactly that.
- The failing run's log cannot say whether one of them was the cause.
- CI has run the test 0 times since the fix, and jupiter never fails it.

So it is not closed as the signal link's bug. By ROOT's rule, the D217 change stays
unmerged, since nothing fails on current main.

The way main leaves open, frames held for a dial that never comes, would end in D217's
CPU-following wait as CI's run did. 3f5698f5a's rule would end that wait at the first slice
in which the VM gives no memory back. That is an argument for reviewing it on its own
merits, not evidence that it is needed.

## Owed

1. **After-fix windows-2025 results.** Rerun `tally.sh` as CI's queue drains:
   `docs/research/measurements/restored-kill/tally.sh 3`, from a clone with origin/main
   fetched. About 83 windows-2025 runs without a failure are needed, or one failure, which
   reopens it under R09.
2. **A host where the failure is frequent.** None is known: jupiter failed 0 of 15 held to 4
   CPUs, 8 of them before the fix.
3. **`ab2-post-2`'s restored VM.** Whether its counter was killed and its end lost, or the
   guest stopped. That needs its console and `docker logs` kept past `rm -f`.
4. **The original VM's loss warning.** Shown above only by elimination. It belongs to the
   signal link's owner.
