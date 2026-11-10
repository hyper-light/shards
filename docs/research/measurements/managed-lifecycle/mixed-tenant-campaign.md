# Mixed-tenant observation-fault campaign

Coordinator `/root`, R01–R16 policy SHA-256
`0d936117dbc6663eb02829c3e921879cecdad3a8f503047588baa21dc82da936`.
Source base:68391c398. This is a repeatable test, not a passing result.

One VM contains an exiting AGENT and a live HARNESS, plus its main workload.
Each has distinct private volume grants. The retained source patch substitutes
only agent-faulty's cgroup.events handle with a duplicate of its held directory.
The real pread therefore returns EISDIR. It does not simulate a kernel cgroup
failure or install a product fault flag; cgroup.kill and the pidfd remain real.
Apply that patch only in an isolated validation checkout and restore it afterward.

```sh
git apply docs/research/measurements/managed-lifecycle/tenant-events-directory.patch
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

Native light checks, actual KVM/HVF/WHP runs, mutation outcomes and resource counts
must be appended by the coordinator after running. This campaign does not prove
full pressure fairness, checkpoint consistency, all syscall tamper routes,
leak freedom, comparative performance or the seven-target matrix. The production
quarantine retains unknown population, held handles and charges until reconciliation
or disposal. Guest/root resource and per-tenant channel guarantees remain separate
requirements in the management plan.
