# Native backend candidate: correctness and notification mutation

This is a tested **standalone backend candidate**, not an integrated Guest fix.
The [actual current owned-API red](../README.md) remains open. Nothing registers
the new [sync.rs](../../../../../../crates/api/src/sync.rs) in lib.rs or changes
client.rs, borrowed deadlines, afunix, server, init or net.

POLICY_READ implementer01a1c5bda0bf766683384c0705c0a9bc, coordinator
01a148884a4770fdaf0111f3f792a248, canonical main
`cbd39284937d024640c56db6f8d8ef2bd9b0ee24`; full AGENTS/R01–R16/evidence/CLAUDE/
handoff read, current managed plan previously read in full and hash checked.
Rules SHA256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`;
resumed full handoff SHA256 `b27c3cb42658e41c5d7538212ef0a7bd3ae581daa258957389d765d0202d9bb0`.
This continues the explicitly assigned native correctness/mutation scope.

## Source and interface boundary

Backend SHA256 `1685d88186cab60f197a81f26d8b2e82b75b1cb19817fdbfe72d9fa8bfda8ddd`.
The small existing caller slice requires State, Tail and Child exclusion plus two
condition variables; the private generic value parameter serves those real types,
not a public synchronization framework. This experimental file currently builds
only for Apple; a shared seven-target interface and platform necessity decision
have not been approved or validated by these tests.

The candidate allocates storage using the actual type Layout and a null check,
initializes native objects at their stable final addresses, and returns T on
construction refusal. Native init/lock/wait/notify errors stay typed. A failed
lock cannot create a guard. Guards cannot be sent to another thread. Wait pairing
is an explicit unsafe private-caller obligation, with no duplicate pairing cache.
The original predicate/deadline belongs to the caller; a notification alone is
not a state change. No warming, spin/poll fallback, new manager or phase ledger.

It is **not ready for production binding**: runtime native errors must preserve
the owning stop/reap and authority contract; existing infallible lock helpers
cannot simply acquire a new `?` path. Partial native cleanup failure currently
reports the cause but may retain native memory without returning its owner;
destructor unlock/destroy is best effort. Ownership/cleanup design and native
errno failure proof remain required before adoption. An attempted residual-owner
edit failed to apply and did not change this tested backend. These source limits
are not represented as reproduced native errno failures or passing cleanup gates.

## Actual native runs

Runner `d1323898b255a3ed435eb83fa0f7b0cf`, admitted `sync:false`, 1CPU/1GiB/2GiB,
NEW private scopes. Installed Rust1.98.1 commit48a229ceaefd4985c50990b14116b6d856af0985,
LLVM22.1.8. No enforced Darwin data/RSS quota or aggregate disk quota is claimed.
All programs and binaries are private; no root protected cache was used or edited.
The exact already-built libc artifact in our ended owned-API closure was read
only, SHA256 `85646a3e267421fc07d236d20e625beaef4018c60b9291405b82f35766985f3a`.
Full source/commands/toolchain/raw outputs are in [first record](first/record.json)
and [notification record](notify/record.json).

| Run | Actual outcome |
| --- | --- |
| First backend compile + two native cases | compile0, tests2pass0fail |
| First non-Send guard negative compile | expected1/E0277 |
| Fresh notification baseline | compile0, tests2pass0fail |
| Remove only pthread_cond_broadcast | compile0, selected1case fails101 at TimedOut |
| Exact original backend bytes restored, fresh build | compile0, tests2pass0fail |
| Restored guard-Send negative compile | expected1/E0277 |

Storage refusal checks retain the exact owned Box pointer and reject both mutex
and CV storage under Rust allocation refusal. The other native case exercises a
two-thread predicate exchange, actual CV wait/notify and zero-timeout handling
while each participating thread refuses Rust allocations. Preparation and helper
Arc/thread construction precede the guard; native mutex lock and first CV notify/
wait do not receive an artificial warmup. The observed refused storage72B is a
diagnostic result, not a proposed size/default. C/kernel allocations and strict
mutex-contention scheduling are not measured or proven by this test.

The [mutation](notify/mutation-sync.rs) replaces exactly the native broadcast
statement with a successful no-op. The actual selected test reached its original
15000ms deadline and failed on TimedOut. The test restores allocation before
rendering an unexpected timeout so an allocator-abort cannot disguise this
specific wake failure. Test assertions may panic; this is not a production panic.
The private source was restored in finally and its SHA verified before the fresh
passing build. Baseline, mutant and restored binaries remain at separate paths.

Notification run root:
`/Users/adalundhe/managed-cold-sync-notify-v1-q_39inap/execution-v2`.
The earlier `hoxizp0z` first-run scope remains preserved. Exact commands and
binary hashes are in their records; [primitive_run.py](../primitive_run.py)
replays with caller-required finite case/command bounds and explicit readonly
dependency path. The first notification launch failed before compile because a
resumed tool-only store had lost that path; [failure](notify/launch-failure.json)
and its corrected explicit-path command are retained. No native result is inferred
from that failed launch. The actual replay uses case15000ms, command60s, outer360s.

The named current source/format/Python/policy checks pass. Full staged whitespace
checking reports one extra blank EOF in byte-exact first/allocation.rs; its
executed-source hash is preserved. No whole-diff whitespace pass is claimed and
no whitespace configuration is changed.

## R01–R16 account and remaining work

| Rule | Evidence / remaining obligation |
| --- | --- |
| R01 | Typed storage/native errors and guarded access implemented; cold allocation-refusal cases pass. Partial cleanup, destructor/native-error semantics and actual owning API correction remain open. |
| R02 | Prior33 pinned alternatives, actual current red and this native backend experiment; no architecture/platform adoption from documents or this standalone run. |
| R03 | Layout/type/ABI-derived storage and timeout representation; no production replacement number. Test inputs remain fixture bounds. |
| R04 | Typed failure and wait outcomes, acquisition guard ownership; no parallel Guest phase state. Runtime native-error/stop ownership still requires review. |
| R05 | State/Tail/Child and two CV users justify the private interface; no cache/poll/spin/manager. No broader caller binding yet. |
| R06 | Focused leaf plus finite fixture/driver; constructor cleanup complexity remains a review item. |
| R07 | Unsafe address, initialization, guard thread/pairing and destruction obligations stated. No syscall/layout-as-performance claim. |
| R08 | No performance, comparative baseline, ratio or overhead allowance measured. |
| R09 | Actual current/candidate owned red retained at f39ce52fff82f8d273a791431cb2c4a62a425ddb; native backend cases do not close it. |
| R10 | Exact source/tool/dependency/binary/command identities; real notification guard removal fails for timeout and exact source restore passes. Owning API green/mutation/cleanup owed. |
| R11 | Native Mac arm64 only. Shared seven-target implementation/gates, native hosts and VM matrix remain unverified/ROOT-owned. Paused WinARM untouched. |
| R12 | No extra installed binary, privilege, trust or crypto change. Apple adapter is experimental; shared-platform necessity/binding review still required. |
| R13 | One private candidate at the existing API seam; no alternative runtime manager or speculative public trait framework. |
| R14 | Correctness allocation refusal only; C/kernel/parent cost, faults and distributions remain ROOT profiling work. |
| R15 | Explicit assigned bounded native tests/mutation, sole worktree writer; no children/goals/main merge or standalone-as-integration claim. |
| R16 | Source/formatter only locally, no local compile/target/cache. New admitted private Mac outputs and all artifacts preserved; no protected cache/paused VM writes. |

The sustained production-correction objective remains active. Next is the reviewed
shared interface and owning native-error/cleanup binding, followed by actual
current owned-API green and meaningful mutation; final combined gates are ROOT's.
