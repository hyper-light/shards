# Native domain lifecycle evidence

Policy: `docs/development-rules.md` R01–R16, SHA-256
`0d936117dbc6663eb02829c3e921879cecdad3a8f503047588baa21dc82da936`.
Implementer: `/root/managed_lifecycle`; baseline source: main `6bf5b15d`.
Root owns live VM runs, audits, mutations, fault campaigns and all measurements.

The fixture installs a native testguest through `AGENT`, with its root `agent.json`.
Its explicitly granted `/probe` volume carries only the test's process facts.
No harness is installed through RUN/VOLUME, and no shell supervises it.
`lifecycle-ready` is published only after the report is synced; the existing
host/main-workload handshake releases the workload after observing that fact.

Run on a supported VM host, from the fixture revision:

```sh
cargo test -p shards --profile test-release --test managed_lifecycle \
  a_supplied_agent_runs_below_a_protected_native_reaper -- --exact --nocapture
```

The expected baseline failure is the recorded actor being namespace PID1.
That is a live fixture hypothesis until the coordinator records its actual run;
source inspection alone is not recorded as a reproduced defect. The fixture also
records proc1 owner and actual signal-zero/proc memory/proc FD-directory access.
It does not claim an exploit from those facts or prove all tamper routes.

The current launch path was compared with Shards' `run.rs` native `--init` reaper,
[runc v1.3.3's fresh init/control handshake](https://raw.githubusercontent.com/opencontainers/runc/v1.3.3/libcontainer/process_linux.go),
[tini v0.19.0's spawn/reap/signal loop](https://raw.githubusercontent.com/krallin/tini/v0.19.0/src/tini.c),
[clone3 CLONE_PIDFD/CLONE_INTO_CGROUP](https://man7.org/linux/man-pages/man2/clone.2.html),
[PID-namespace init semantics](https://man7.org/linux/man-pages/man7/pid_namespaces.7.html),
[wait/reaping](https://man7.org/linux/man-pages/man2/waitid.2.html) and
[cgroup v2 subtree kill/population](https://docs.kernel.org/admin-guide/cgroup-v2.html).
The implementation decision and full every-rule account follow with its code.

This fixture commit changes no launcher behavior. VM red/green, descendant reaping,
helper/actor/owner death, malicious signals/ptrace/proc-FD access, native Windows
and macOS host runs, mutation checks, allocations/faults/latency and seven-target
validation remain owed. Its guard matches the existing supplied-startup fixture;
it is a test-run failure guard, not a production lifecycle timeout.
