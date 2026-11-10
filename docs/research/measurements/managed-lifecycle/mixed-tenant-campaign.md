# Mixed-tenant observation-fault campaign

Coordinator `/root`, R01–R16 policy SHA-256
`0d936117dbc6663eb02829c3e921879cecdad3a8f503047588baa21dc82da936`.
Source base:68391c398. This is a repeatable test, not a passing result.

Current recurring lane: `/root/multitenant_live_e2e`, role=implementer, owned host/
guest lifecycle fixtures and this measurement directory. POLICY_READ revision
`c921a0b12b06d495b354907162e991376edffd41`, rules SHA-256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`.
Ada explicitly authorized the recurring multi-tenant live E2E/adversarial lane;
the coordinator retains final combined gates, review and merges. This supersedes
historical root-only campaign assignments below, without expanding source ownership.

One VM contains an exiting AGENT and a live HARNESS, plus its main workload.
Each has distinct private volume grants. The retained source patch substitutes
only agent-faulty's cgroup.events handle with a duplicate of its held directory.
The real pread therefore returns EISDIR. It does not simulate a kernel cgroup
failure or install a product fault flag; cgroup.kill and the pidfd remain real.
Apply that patch only in an isolated validation checkout and restore it afterward.

```sh
git apply docs/research/measurements/managed-lifecycle/tenant-events-directory.patch
git apply docs/research/measurements/managed-lifecycle/tenant-console-observation.patch
cargo test -j 4 -p shards --profile test-release --test managed_lifecycle \
  a_tenant_observation_fault_leaves_a_mixed_sibling_and_workload_alive \
  -- --ignored --exact --nocapture
```

After the trusted quarantine diagnostic, the host executes a fresh command in the
main namespace, atomically publishes a new128-bit challenge to the HARNESS's
private volume, and verifies its exact synced response. An inotify watch installed
before checking the file avoids sleeps and lost publication wakes. The host also
reads the surviving main/HARNESS processes, then explicitly stops the VM. The
120-second guard is the existing supplied-startup test guard; no product timeout
or entitlement changes. Challenge bytes establish ordering, not authentication.

The independent tenant-observation-fatal-mutation.patch restores whole-boundary
failure for that same pread error. The same test must then observe actual refusal
of post-fault main execution, rather than merely miss an alert. This mutation
does not corrupt the control handles or manufacture a different I/O error.

For subsequent isolated Linux runs, use the retained script with a **new** absolute
evidence prefix and a private `DOCKER_CONFIG`, after kiln admits this lane's four
CPU/eight-GiB-memory/twenty-GiB-disk envelope:

```sh
bash docs/research/measurements/managed-lifecycle/run-tenant-observation.sh \
  green "$HOME/managed-tenant-fault-UNIQUE-green"
bash docs/research/measurements/managed-lifecycle/run-tenant-observation.sh \
  fatal "$HOME/managed-tenant-fault-UNIQUE-fatal"
```

The script requires a clean checkout, refuses existing output files, records the
exact source patch and environment, and restores only the three injected source
files. A fatal run should return the test's nonzero status; assess its actual main
execution refusal in the log rather than treating every failure as decisive.

First KVM attempt e6558222b/r68 failed0/1 after145.24s: the test waited for a
diagnostic which is written to init's console, while warm VMs deliberately discard
that console and keep main stdout/stderr private. This is an observation-seam
failure, not evidence that quarantine failed. The separate retained console patch
enables warm boot/restore console only in this isolated campaign. It is not a
production output-policy change and cannot be used as raw-output independence
evidence. Production typed control/metadata observation is separate capture work.

The second attempt55a70c84/r76 failed0/1 after120.90s, but its daemon.log
contains the exact trusted EISDIR quarantine diagnostic. The UART writer retains
the daemon's original output destination across client stdio handover. The fixture
therefore now observes that actual file with the existing cross-platform FileWatch
and an event wait, then performs the live sibling challenge. It does not add a
periodic log reread or change production routing to accommodate the test.

Recurring-lane KVM run r4 at `02d60fc0` failed 0/1 in 2.69s, exit101. The real
EISDIR quarantine diagnostic was present, but two fixture seams were wrong: the
watcher returned the unterminated log tail before `21` arrived, and post-fault exec
requested bare `report` instead of `/bin/testguest report`. Exec accepted the new
request and returned127 for its missing executable. This is a fixture failure;
it is not a reproduced whole-VM failure. The correction only accepts newline-
terminated diagnostic lines and names the actual executable for fresh exec.
Raw logs/exit/environment are retained on Fedora as
`~/managed-tenant-fault-watched-r4.{log,exit,identity}`. This first recurring run
reused the old watched-green filename before copying it to its unique prefix;
the prior r81 raw file was overwritten. Its earlier Kiln transcript remains,
and this evidence-loss error is explicitly reported to the coordinator. The new
no-clobber script prevents that reuse on subsequent attempts.

Native light checks, actual KVM/HVF/WHP runs, mutation outcomes and resource counts
must be appended by the coordinator after running. This campaign does not prove
full pressure fairness, checkpoint consistency, all syscall tamper routes,
leak freedom, comparative performance or the seven-target matrix. The production
quarantine retains unknown population, held handles and charges until reconciliation
or disposal. Guest/root resource and per-tenant channel guarantees remain separate
requirements in the management plan.
