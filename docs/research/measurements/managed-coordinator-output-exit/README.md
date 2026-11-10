# Coordinator owner-output exit evidence

Root executed r518/r523 on Fedora Linux amd64 with real KVM guests and normal
carried helpers, from **`ebbda9b3b7b3de9a9143f4d383f60769ec7803b7`**.
The fixture is unchanged from agent source `8c8625b6`; its restored SHA256 is
`a0ed1cd19511ff27be24d8bff1acd70d0b5070c682a1fe462e09681439a655c3`.
This README author read immutable retained records only and ran no build or live
campaign. The experimental 2N topology remains unadopted; these results make no
architecture or product-completeness claim.

## r518: normal caller and positive coverage

[Driver](root-managed-output-exit-ebbd-v1/driver.py),
[driver log](root-managed-output-exit-ebbd-v1/driver.log),
[results](root-managed-output-exit-ebbd-v1/results.json) and
[compile command](root-managed-output-exit-ebbd-v1/compile.command.json)
retain the actual invocations. Full normal-helper `managed_output --no-run`
compile exited 0. The [default regression run](root-managed-output-exit-ebbd-v1/regression.log)
passed **6 tests, 0 failed, 2 ignored**; ignores are not passes.
The [normal test artifact](root-managed-output-exit-ebbd-v1/test-artifact.json)
SHA256 is `4aec800964a7058fca4cd14e223c4820e415aaed58ba679b15eed9206127e34e`.
Per-case `inputs.json` retains actual fixture/CLI/kernel/init/testguest hashes.

| Explicit members | Actual test exit | Valid daemon + VM exit events | FD baseline + held → with watches → released |
| --- | --- | --- | --- |
| [0](root-managed-output-exit-ebbd-v1/members-0/teardown.json) | 0 | Both | 7 + 5 → 12 → 7 |
| [1](root-managed-output-exit-ebbd-v1/members-1/teardown.json) | 0 | Both | 7 + 5 → 12 → 7 |
| [2](root-managed-output-exit-ebbd-v1/members-2/teardown.json) | 0 | Both | 7 + 5 → 12 → 7 |

All three cases recorded pinned/validated identities, protocol completion,
successful owner stop/observer join and exact FD delta/return. Both role states
were `ExitReadyObservedAfterOwnerStopWithinCleanupDeadline`, using the same
explicit cleanup deadline (caller input 300000 ms). N0 retains its distinct
NoManagedMembers readiness contract; N1/N2 use managed channel readiness. These
are finite cases, not latency distributions, a scaling law or kernel-charge proof.

## r523: negative witnesses and individual guard removals

[Driver](root-managed-exit-negative-guards-ebbd-v1/driver.py),
[driver log](root-managed-exit-negative-guards-ebbd-v1/driver.log) and
[results](root-managed-exit-negative-guards-ebbd-v1/results.json) retain the
campaign. Every case compiled 0, used explicit N0 and recorded protocol completion
plus successful unchanged owner stop/observer join. Each case directory retains
its `mutation.patch`, exact command/environment, test artifact hash, test log and
`observations/teardown.json`. Driver exit 0 means the expected witnesses were
observed; it does not turn guarded failures or false accepts into normal passes.

| Case | Actual test exit | Observed witness |
| --- | --- | --- |
| [wrong-identity-guarded](root-managed-exit-negative-guards-ebbd-v1/wrong-identity-guarded/test.log) | 101 | Private daemon expected start incremented by 1; typed DifferentIdentity at before_watch, no valid daemon watch; VM exit ready, FD7 + 3 → 10 → 7. |
| [wrong-identity-check-removed](root-managed-exit-negative-guards-ebbd-v1/wrong-identity-check-removed/test.log) | 0 | Same fault, removing only the PinnedProcess comparison: false accept with both exit events and FD7 + 5 → 12 → 7. |
| [extra-fd-guarded](root-managed-exit-negative-guards-ebbd-v1/extra-fd-guarded/test.log) | 101 | One owned `/dev/null` File opened after unchanged stop; final FD count 8 versus baseline 7, exact_delta_and_return=false. |
| [extra-fd-check-removed](root-managed-exit-negative-guards-ebbd-v1/extra-fd-check-removed/test.log) | 0 | Same extra File, removing only the total-FD assertion: false accept despite the retained 8-versus-7 witness. |
| [expired-exit-deadline](root-managed-exit-negative-guards-ebbd-v1/expired-exit-deadline/test.log) | 101 | Finish receives an expired deadline after real stop; both roles typed Deadline; FD7 + 5 → 12 → 7 and exact return true. |
| [restored](root-managed-exit-negative-guards-ebbd-v1/restored/test.log) | 0 | Fresh closing case: both valid exit events, FD7 + 5 → 12 → 7, exact return true. |

The identity fault targets only the fixture's own private daemon identity; it
does not adopt or signal a discovered process. The extra File is RAII-owned and
closes when its scope leaves: this witnesses a descriptor retained at the check,
not a permanent leak. The deadline case forces observation expiry after stop;
**deadline guard removal was not run** and remains owed.

Final [restored SHA](root-managed-exit-negative-guards-ebbd-v1/restored.sha256)
matches the original above; [restored.diff](root-managed-exit-negative-guards-ebbd-v1/restored.diff)
is empty. The closing artifact hash matches the normal r518 artifact. These are
restoration facts for this exact source, not validation of later changed code.

## Retention, meaning and remaining work

The raw links target the original root names in the coordinator's separately
integrated archive trees. The coordinator owns transfer/extraction/manifests:
supplied archive identity is
**370 regular files, 206618 bytes, SHA256
`406c0868f50e5ca5850e69e9b63885fbac818b7dd4bd292be610daf0d3e5a469`**.
This author did not independently transfer or verify that archive; the account
was checked against the retained remote results, patches and restoration files.

The [source contract and primary references](../managed-output-cost/HOST-EXIT.md)
explain the reused Poller/ExitWatch, pinned identity and FD phases. The guarded
positive and restored cases establish held process-exit readiness and the
reported fixture FD checkpoints.
Reaping, exit status, exact exit time, stop causality, full-population return,
isolated kernel charges and comparative costs remain **Unverified**. Partial
identity-rejection cleanup was observed; other acquisition/error/interruption
paths, deadline guard removal, broader adversarial/matrix coverage and resource
comparisons remain coordinator work. No allocation-free, leak-free, speedup,
hard-budget or final topology claim follows.

POLICY_READ: docs-only implementer, full current main
`42e3f7582d7bb9b5f0579aecb4085de5b1769b5f` AGENTS/R01–R16/evidence/CLAUDE/plan
and current handoff read; rules last changed at f5cec365, SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Ownership is this README only. R01–R07/R12/R13 production, constants, state,
interfaces, waits and caches are unchanged; no new design decision. R09/R10/R15
record root-owned revision-specific effects, counterfactuals and limits.
R08/R14 costs remain unclaimed/root-owned; R11 skips and unavailable coverage
remain unverified. R16 source, evidence, credentials and active caches are
preserved. No build, campaign, child agent or main merge was performed here.
