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
| 1. D234, the record, this skeleton | (this commit) | scripts/check-development-policy on a Linux runner | recorded in the handoff |
