# The nesting grant (D234): evidence record

This record follows [docs/development-evidence.md](../../../development-evidence.md) for step 38 of kiln plan 01a1cd8e862e7169a074db3d5fa33f3d.

The decision is [D234](../../../design/architecture.md). Its research, with every finding graded, is [nesting-grant-decision-v1](../../nesting-grant-decision-v1.md). The policy receipt is [POLICY_READ.json](POLICY_READ.json).

Each item names its command, revision, log and outcome as it is done. **Owed** marks what is not done yet, and nothing owed is passed.

## Identity

- Task: plan step 38. It is planned as kiln plan 01a1cda5426f73cc810ea93e3bd008cd and researched as kiln research 01a1cda5426f73cc86b87e7749163666.
- Branch: `p/s38-nested-grants-v1`, from origin/p/integrate-engine-20261111 032b63b1c, merged with origin/main c4ae59ee at c63b3e944.
- Policy: origin/main bff30151. The rules' sha256 is df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
- Role: implementer for Shards ROOT (01a148884a4770fdaf0111f3f792a248).
- Owned paths: as in POLICY_READ.json.
- Targets: the seven of resources/supported-targets.txt. Guest code is Linux only, on amd64 and arm64.

## Decision (R02, R03, R05, R12)

D234 and its record:
- the alternatives and why each was not chosen;
- every source at a pinned revision;
- every literal's derivation.

No measurement backs the decision itself. The two inferences that only a live run shows are owed to the real-VM test:
- the boxes' pivot_root under shards-init's root (f28);
- the workload's filter bounding nested device programs (f20).

## Wait and cache review (R05)

The grant adds one wait: the readiness state machine's wait on the caller's deadline. It waits on the deadline the caller set and on the workload's own READY=1 notification, and it adds no probe and no cache.

Owed: the code's review once written (plan steps 3 and 4).

## Live example (R09, R10)

Owed. The real-VM test `crates/shards/tests/nesting.rs` (plan step 5) runs runc as a stand-in nested runtime under the grant, and without it. ROOT runs it on:
- KVM x86_64;
- nested arm64 KVM;
- HVF with its GPU;
- WHP.

Each guard's mutation is listed with the test.

## State and code (R01, R04, R06, R07, R13)

Owed, for plan steps 2 to 4:
- the readiness transitions (D234's table), each typed and tested;
- the refusals, typed through to their words;
- the panic review;
- clippy for the touched crates on a Linux runner.

## Adversarial results (R15)

Owed to ROOT's campaign:
- a crafted image declaring NESTING beside an agent;
- a joiner into a granted microVM;
- a box writing outside the subtree;
- datagrams to the notify socket from outside the workload, and oversize ones;
- a missed deadline;
- the workload's death taking its boxes.

## Performance (R08, R14)

Owed to ROOT: a granted run's start against an ungranted one (docs/research/measurements/build-ab), with n, p50, p90, p99 and max, host, OS and revision.

## Portability (R11, R12)

- The guest side (crates/init) is Linux, on amd64 and arm64, and runs on KVM, HVF and WHP.
- The daemon's checks are shared code for every host.
- Owed to ROOT: `scripts/lint` on the seven targets, and the real-VM runs listed above.

## Operations (R15, R16)

- Commits are signed with gits, of named paths. Each step lands on main by fast-forward push once its tests pass, under the person's rule of 2026-11-12 (subagents merge their own work to main), which supersedes AGENTS.md's no-self-merge for this work.
- No build or test ran on this Mac, which is below its 30 GiB floor.
- Light tests run on kiln Linux runners within POLICY_READ.json's envelope.

| Step | Commit | Checks | Outcome |
|---|---|---|---|
| 1. D234, the record, this skeleton, the literal audit | 534a1401a to c026df6a2 (origin/main merged at c63b3e944, d59e0705b and fdfd28a87; D234 landed on main at d59e0705b) | **R03, measured:** `python3 docs/research/measurements/s38-nested-grants/literal-audit.py` at c026df6a2. It covers the 22 literals D234's grant adds, every backticked token of its Decided and Wire text, and the identifiers marker 7, `run::kind` 49 and ipc kind 90 against their tables. Its own mutation check, on copies of the files: an unmutated control passes; deleting PIPE_BUF's bullet fails (UNDERIVED PIPE_BUF); planting `quota=<n>` in Wire fails (UNPLANNED); taking marker 7 fails (TAKEN, next free 8). **Policy:** `python3 scripts/check-development-policy`, with the audit, at c026df6a2: (a) through kiln run on runner cec37f60 (macOS arm64), fetched from origin; (b) on adacachy (Linux x86_64, Python 3.14.8) over ssh from `git archive c026df6a2`. kiln's Linux runners refused runs from this machine after kiln's restart: 451397fc and 00aad8da "did not answer in 15s", 8fadc1ec "didn't answer hand … in 8914 ms", and "has no room here now: it isn't started". Earlier, at e57f32a4e, the policy check alone passed on adacachy over ssh | audit: planned 22, underived or unplanned 0; markers 7, 49 and 90 free; exit 0, on both. Policy: "Development policy checks passed; evidence/coordinator review is still required.", exit 0, on both. **Through kiln run on Linux** (the step's own test): `kiln run --on 451397fc… --no-sync --stdin` (the files by `git archive c026df6a2` on stdin; harness [kiln-linux-check.sh](kiln-linux-check.sh), raw [kiln-linux-check.log](kiln-linux-check.log)) ran on adacachy (Linux x86_64) at 2026-11-12T07:59Z, after adacachy2's runner restarted mid-run: policy_exit=0; audit planned 22, underived or unplanned 0, audit_exit=0. kiln's CLI then exited 1 with "refused: 451397fc… sent no receipt of it", a receipt kiln wants after the output, not the command's status. **Again, with a clean exit:** the harness, now requiring kiln's own exit status 0, ran on kiln-a094a12d (8fadc1ec…, Linux x86_64) at c026df6a2 ([kiln-linux-check-clean.log](kiln-linux-check-clean.log)): rc 0, policy_exit=0, audit planned 22, underived or unplanned 0, audit_exit=0 |
| 4 (coordinator's follow-up). init refuses the grant with `--pid host`, as D234 says; D234's literals gain step 4's (the socket's mode 0666, READY's 9-byte payload, the `notify` entry and tag `N`, NR_OPEN_MAX 2147483584) and the on-time boundary | 6f752173d | on adacachy (Linux 7.2.9-2-cachyos, rustc 1.98.1) under systemd-run MemoryMax=16G CPUQuota=400%, -j4, from `git archive 6f752173d`: `cargo test -p shards-init -p shards-abi --target x86_64-unknown-linux-musl`; clippy for that target with `-D warnings`; `cargo fmt --check`. Then the mutant that drops the `pid=host` arm of `undoes_nesting`, against `run::nesting_tests::the_grant_is_refused_with_the_microvms_cgroup_or_pid_namespace`. The literal audit: 26 planned | abi 49 passed and 2 passed; init 196 passed, 0 failed, 1 ignored (frames_cost, a measurement); clippy exit 0; fmt exit 0; the mutant fails its test (exit 101); the copy removed. Audit: 26 planned, 0 underived or unplanned; marker 7 and run kind 49 held by the grant's own WORKLOAD_READY and READY, ipc kind 90 free |
| 4 (step 4's lane). init applies D234 (b3001d90a to 16c72cc32), then a real-kernel light test of D234's cgroup claims (6b2c72a3a, main b51253c18) | 16c72cc32, b51253c18 | on adacachy (Linux 7.2.9-2-cachyos, cgroup2 nsdelegate): `cargo test -p shards-init -p shards-abi --target x86_64-unknown-linux-musl -j4`; clippy `-D warnings`, all targets, x86_64 and aarch64 musl; fmt `--check`; the mutants M1 to M6, each against its named test ([step4/mutants-586c018fb](step4/mutants-586c018fb)); and `run::nesting_tests::on_a_real_kernel_the_granted_command_sits_in_its_leaf_and_its_root_is_inits` through init's own isolate and setup code in a delegated systemd scope ([step4/real-kernel.sh](step4/real-kernel.sh); no VM, no daemon). Logs: [step4/](step4/) | at b51253c18: abi 49 and 2 passed; init 197 passed, 1 ignored, 0 failed; clippy exit 0 on both targets; fmt exit 0. Every mutant fails its named test. On the real kernel: the hierarchy is writable; the standby sits in the leaf with the namespace root empty; `+memory +pids` enabled beside the leaf; a box's limits set; the root's own memory.max and pids.max refused EPERM (nsdelegate) and unchanged (268435456, 64); an exec refused the root (EBUSY) and put in the leaf. Its two mutants (standby left in the root; exec sent to the root) fail, and restored, it passes |
| 1 (R03, independent review). A read-only reviewer was given D234, the policy and the sources, not the author's audit. It built its own literal list and checked each derivation ([r03-review.md](r03-review.md)) | 20f37abee reviewed; fixes in this commit | The reviewer's verdicts, against its own list and the sources: 25 hold, 7 weak, 2 missing; 7 literals omitted from the list; 5 derivations wrong or unsupported. Every point is answered in D234 (r03-review.md's Response). literal-audit.py after: 31 planned, 0 underived or unplanned, exit 0 |
