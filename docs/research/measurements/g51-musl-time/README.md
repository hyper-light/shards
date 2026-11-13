# x86_64 musl's tests' step against its limit (PM M547)

What the x86_64 musl test job's "On the native architecture, in Alpine" step spent its 60
minutes on, from CI's own logs, beside x86_64 gnu's of the same run (main cc439a2a7).

- `account.py LOG`: the step's time, from its start to its limit, split into building the
  tests (to the first test binary) and each test binary, from its `Running` line to the next,
  with its result and failures. Output: `musl-131488530250-step.txt`.
- `phases.py LOG`: from cargo's `Finished test-release` line, each test binary's span, for
  comparing two jobs binary by binary. Outputs: `musl-131488530250.txt`,
  `gnu-131488530221.txt`.

Run with `python3 -I` over a job's log with its timestamps (GitHub's per-job log, as
`gh api repos/hyper-light/shards/actions/jobs/<id>/logs` gives it, ANSI stripped).
