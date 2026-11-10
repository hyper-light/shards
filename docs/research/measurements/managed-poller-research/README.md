# CONSTANTS RESEARCH lane: source identities and handoff

Coordinator: Kiln `01a148884a4770fdaf0111f3f792a248`. Implementer lane:
`p/managed-poller-research`, initial exact main
`24f48ee5ebf198485117aa93bf47685ad7d7d53f`.
The [research note](../../managed-poller-capacity.md) is the deliverable;
[source-identity.json](source-identity.json) pins fetched primary bytes and local
Git blobs. This directory contains textual research/evidence only, no executed
measurement harness or runtime result.

POLICY_READ implementer before research/edits: full canonical
`/Users/adalundhe/Projects/shards/.claude/worktrees/managed-main-landing/AGENTS.md`,
`docs/development-rules.md` R01–R16, `docs/development-evidence.md`, `CLAUDE.md`,
`docs/design/managed-runtime-plan.md`, checkout policies from HEAD, and full
`/Users/adalundhe/g121-handoff.md`. Checkout policy/plan bytes matched canonical
24f. Rules SHA-256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`;
AGENTS SHA-256
`5fd5c2bc14079994c7390926c8f3865efdfedf066faa9c33d7dc361697aeea28`;
evidence SHA-256
`a84c60f20e3a06a294bd5b091c1f43cfd5cc4b33ef1bd22a50cd94cea07bb97b`.
Read the policy and current handoff again after compaction or policy change.
The manifest records the handoff bytes actually read; later handoffs can change.

Ownership is only `docs/research/managed-poller-capacity.md` and textual records
under `docs/research/measurements/managed-poller-research/`. Production source is
read-only. The research skill's primary-source/Markdown workflow applies; Ada's
explicit no-child instruction overrides its background-agent suggestion.

## Scope and dispatch

Only two root-selected literals: unix `Poller::wait` batch 64/token collection,
then guest `Memory::relieve` inotify read buffer 4096. Read focused functions and
semantic callers only, sparse omissions through `git show` exact main. No broad
audit, speculative fix, architectural selection or new numerical recommendation.

Root owns all R15 audits, profiling, benchmarking, heavy real E2E, chaos, stress,
sweeps and main merges. This lane runs none of those and no actual VM/socket/
namespace/BPF campaign. No production changes without root-approved measured or
reproduced basis. No host-root/admin/security/crypto changes. macOS arm64 only;
Mac floor 30 GiB; no local builds or further full worktrees. Any later approved
light compile requires new private Kiln source/target and headroom at most
1 CPU / 1 GiB / 2 GiB. Protected root targets/caches and the paused Windows ARM VM
remain untouched. No cleanup, reset, stash, clean or signing waiver.

The initial root-created `--no-checkout` index had no entries. This lane wrote
zero files before/during the setup hold. Root corrected it with sparse-aware
initialization, reported exact 24f/clean, and explicitly resumed the assignment.
The first documentation writes happened only after that resume.

## Source identity and freshness

Pinned Linux v6.18 resolves to `7d0a66e4bb9081d75c82ec4957c50034cb0ea449`;
XNU xnu-11417.101.15 to `e3723e1f17661b24996789d8afc084c0c3303b26`;
libevent release-2.1.12-stable to `5df3037d10556bfcb675bc73e516978b75fc7bc7`;
mio v1.1.0 to `3a1b47180161fe191a8f8a8cc0e1cc572678aab2`;
Rust 1.91.0 to `f8297e351a40c1439a467bbbb6879088047f50b3`.
The libev 4.33 official archive SHA-256 is
`507eb7b8d1015fbec5b935f34ebed15bf346bed04a11ab82b8eee848c4205aea`;
individual relevant members also have hashes. Archive member bytes were read in
memory without extracting a source tree.

The manifest gives commit dates and retrieval UTC/HTTP identity per object. These
versions are deliberate research pins, not claims to be the latest releases or
to match a running host kernel. XNU matches the existing Shards source citation.
libevent/libev historical releases establish mechanisms; an actual competitive
baseline and current product compatibility still require coordinator selection.
Mutable man7/Apple archive pages are content-hashed snapshots without invented
Git revisions. No upstream source copies are retained in this repository.
Browser access to the libev archive/CVS failed; HTTPS retrieval from the official
maintainer archive succeeded and supplied the recorded archive/member hashes.

## Checkpoints and current handoff

Policy receipt was sent with stable key
`managed-poller-research-policy-read-24f-v1`; no-write hold status with
`managed-poller-research-setup-hold-no-writes-v1`; resumed receipt/finding with
`managed-poller-research-resume-policy-24f-v1`.

First batch documents Poller facts, syscall limits versus defaults, existing
callers/ownership, alternatives and an **unrun** coordinator reproduction/cost
plan. The next batch immediately researches Memory::relieve's 4096 buffer while
root reviews this piece. After both batches, request the next specific literal
from root and remain available without manufacturing an idle campaign or waiter.
Signed checkpoints use `/Users/adalundhe/.kiln-run/bin/gits`, named paths and the
required Claude Opus 5.5 trailer. Only this lane's branch is pushed; no self-main
merge. Commit IDs are sent in coordinator messages and visible in Git history.

## R01–R16 account for first research batch

| Rule | Actual work and remaining limit |
| --- | --- |
| R01 | Production unchanged. Distinguishes possible infallible Vec growth from a live OOM result; derived pre-dequeue obligation recorded. Reproduction/error-path proof unrun. |
| R02 | Pinned Linux/XNU/UAPI, Rust, libevent/libev/mio and official manuals read. Alternatives documented; no architectural choice without live data. |
| R03 | No literal introduced or replaced. Kernel ABI bounds separated from defaults, startup/overflow/refusal requirements listed. Method/number selection remains open. |
| R04 | Existing add/wait/remove/close/exit/interrupt/cancel transitions described; candidate preservation obligations listed. No production state machine changed. |
| R05 | Focused named wait and semantic callers only. Existing reuse versus fresh caller Vec distinguished; duplicate membership cache rejected absent consistency proof. No repository audit or cache benefit claim. |
| R06 | Documentation-only diff; no runtime branch/complexity change. Future candidate must be scoped and locally understandable. |
| R07 | Claims link to primary source and exact local blobs. No production comments changed. |
| R08 | No performance result or winning architecture. Pinned equivalent comparison and distributions are root-owed; 4–5× target remains unproven. |
| R09 | No reproduced runtime defect claimed or production fix made. Exact caller reproduction is the first proposed coordinator step. |
| R10 | Durable note, source-byte identity, existing root-evidence links and unrun method retained. Before/after/mutations explicitly unrun. |
| R11 | Linux GNU/musl amd64/arm64 and macOS arm64 ABI obligations recorded. Windows and all build/real-VM coverage unrun here. |
| R12 | Existing platform seam and caller ownership considered; no parallel Poller, manager, OS abstraction, dependency or binary added. |
| R13 | Only research note and textual identity/evidence, no runtime code/interface or speculative test framework. |
| R14 | No allocations, faults, CPU, latency, memory or kernel costs measured. All such work remains coordinator-owned. |
| R15 | No children, audit, profiling, campaign or main merge. Small signed research checkpoint returned; coordinator review/combined gates remain owed. |
| R16 | No cache/target/worktree cleanup or heavy build. Root setup preserved all data; protected paths and paused VM untouched. |

## Checks and owed runs

Source retrieval and local-blob hashing are identity checks, not runtime
measurements. `python3 scripts/check-development-policy` at exact base 24f
returned 0: "Development policy checks passed; evidence/coordinator review is
still required." Named-path staged whitespace and local-link/manifest checks are
recorded in [checks-poller.json](checks-poller.json) before the checkpoint commit.
No Rust compile,
unit/integration test, syscall fixture, fault injection, mutation, VM, lint matrix,
benchmark, profile or audit ran in this batch. All runtime and comparative plans
in the note are coordinator-owed and unrun; historic evidence is identified at
its actual historic revision and does not validate this documentation as code.
