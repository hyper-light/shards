# Operator control boundary tests on current main

This branch places the five previously prepared cases into a private child of
`remote::managed`'s existing cfg(test) module. **No production line changes.**
The authoritative current main is
`69dbc3180b8c8c55cb03f9f95f822d2752ba2887`, including the separately validated
d6f2 finish-wake and cd5 receive-cancel fixes. This is a test-only candidate for
coordinator review, not an implementation of e1cb terminal metadata publication.

POLICY_READ IMPLEMENTER: full canonical AGENTS.md, R01–R16,
development-evidence.md, CLAUDE.md, managed-runtime-plan.md and
`/Users/adalundhe/g121-handoff.md` read before work; canonical current rules
SHA256 rechecked unchanged:
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`,
policyfiles f5cec365. Coordinator `01a148884a4770fdaf0111f3f792a248`, implementer
`01a1c5d2ee5871a2a9a9cdc9f56793f0`. User's current steering continues the approved
pure boundary queue while ROOT's complete normal-member fixture is gated. Owned
paths: only `crates/remote/src/managed.rs`'s cfg(test) child and this new evidence
directory. Existing clean worktree reused on `p/managed-operator-tests`; no
children/local build/new full worktree/main merge or protected ROOT target write.

## Existing contract and added compositions

| Test | Required existing behavior |
| --- | --- |
| ACK prefixes and coalesced controls | An incomplete ACK returns None. The first of two complete encoded ACKs reports its own exact frame end; the remaining frame retains its own cursor. |
| Malformed ACK before a valid following control | Invalid ACK length, unknown tag or valid-but-inapplicable Stdout rejects the first frame with the exact typed error, without scanning forward to the valid tail. |
| Invalid subscription channel | The exact channel field rejects zero/u8::MAX; a later independent valid parse retains complete run/generation/member/channel/cursor values. |
| Decoded ACK at the last legal wire end | Zero/start/one-short ACKs retain AwaitingAck bytes, pointer and progress. The exact final u64::MAX cursor advances Open→Ready or Finishing→Finished. |
| Malformed control and explicit disconnect cleanup | The existing disconnect retains owned bytes, resets replay's written count, and permits only the original cursor; an ACK cannot release that unsent replay. |

The existing tests covered parser and direct-method pieces independently. These
are finite compositions at those actual methods, using the existing service
helper. The private child imports that helper, introduces no production API or
framework and confines indexing allowance to tests. Fixture bytes, integer
endpoints and adjacent invalid lengths/tags derive from the existing wire widths
and finite test payload. The seeded last legal cursor avoids constructing an
enormous prefix. Remote cursors count encoded bytes; canonical payload offsets
remain a different contract.

The last test deliberately calls the existing disconnect method after observing
the parser error; it does not claim to invoke Io::close or Service::controls.
A real malformed control refuses/disconnects rather than accepting the next
ACK on the same connection. The pure tests establish local guard/state/ownership
behavior only, not authentication, live transport, actual refusal I/O, resource
return, metadata durability or lifecycle completion.

Previous signed preparation: `4038ca73de68bef9b7382452a02fbddaafcfb8fb`,
`20c62371ee057bba8ef1493ba133ca2c41f47a97`,
`e3807b8e9013a6d67e7f928fc95db139e05f481f`. The latter's proposed patch was
mechanically applicable to d6f2. Actual first light results were parser3/3 and
all five pure cases in a private real-module copy on096c, with scoped Clippy.
Those are older revision results, not this current main candidate's passes.

## Exact current-revision light method

ROOT received path/readback before the actual test-only edit under stable key
`implementation-d8-summary-checkpoint-69db-pure-source-scope-v1`. The new private
Fedora source/target is
`/home/adalundhe/managed-operator-tests-light-69db-v1/{source,target}`.
Kiln request is 1 CPU/1 GiB/2 GiB, jobs1/test-thread1, debug/incremental0. No
memory/CPU peak or performance claim follows from admission. The filtered sparse
checkout is exact main69db; the small workspace retains actual remote/ABI/IPC/
testdir and unchanged pinned dependencies/provider, package metadata, lints and
configuration. There are no parser/state/transport stubs.

The checker compares every byte before cfg(test) to the original source both
before and after formatting. Formatting explicitly uses the checkout's actual
rustfmt.toml (max_width110). Only `managed::tests::operator_boundaries` executes;
it does not call a native endpoint, Driver::run, socket, namespace, BPF, VM or
profile. Scoped Clippy compiles but does not execute other tests.

Actual Kiln r18 completed on Fedora amd64
`8fadc1ec213332e16cb0d915a3fb913f`: Linux6.19.10-300.fc44/glibc2.43,
rustc1.99.1 (58cf8f9c35b355199727627d07ae34a4e2846d67), LLVM23.1.1.
Source+target/storage measured366,064KiB. CPU/RSS peaks were not profiled;
headroom is not a hard resource quota. Exact commands and complete original
logs are retained in [light-result.json](light-result.json).

```text
cargo test --offline -p shards-remote --lib managed::tests::operator_boundaries \
  --target x86_64-unknown-linux-gnu --target-dir PRIVATE/target --jobs 1 -- --test-threads=1
exit0; 5 passed, 0 failed, 0 ignored, 22 filtered out
cargo clippy --offline -p shards-remote --lib --tests \
  --target x86_64-unknown-linux-gnu --target-dir PRIVATE/target --jobs 1 -- -D warnings
exit0
python3 scripts/check-development-policy
exit0; evidence/coordinator review still required
git diff --check -- crates/remote/src/managed.rs
exit0
```

The five pure cases ran; filtered tests were not executed or passed. Ordinary
unchanged provider notices and unused-patch warning remain in raw logs. Full
tested source SHA256:
`821cbc12fa53778d9c4933bdbddecdb2dfe9801390847bdebc7a44ab3c8288fb`.
Original and final production prefix both hash to
`abf3c9e06cd65f701dc60c576dafac414920baedfd0192b45b537f41a346624f`.
Original full source hash was
`5782d2f9d9c83cf2db6d4d1baf3152cd1f3daf5aef9856ebce788bdcbf098bab`.
Exact formatted/tested bytes returned to the owned source path. Full combined
managed suite, other native targets, seven-target gate and guard mutations on
this candidate remain ROOT-owned and unrun by this lane.

## Policy and owed coordinator gates

R01/R04/R06/R07/R13: test-only assertions at existing checked parser/state paths,
explicit exact errors/transitions/retained bytes; no production fix or panic-free
claim for existing libraries. R02/R03/R05/R12: concrete missing compositions at
the actual seam, no architectural choice, production literal, wait/cache/thread/
manager/OS adapter. R09/R10: no production defect is claimed from these tests;
retain exact source/commands and typed outcomes, with guard mutations owed.
R08/R14: no benchmark/profile/allocation/fault/competitive claim. R11: only the
actually reported GNU light target is checked; combined seven-target/other native
hosts/hardware are not inherited, macOS arm64 only, Windows ARM paused. R15:
ROOT owns review, actual protocol/VM/chaos/mutation/combined gates and main merge.
R16: source/evidence/prior checkpoints preserved, no local target or ROOT cache
mutation. ABI source is unchanged; unmerged82425 still separately requires paired
host/guest rebuilds and is not silently included here.

ROOT's r632/r662 confirms both normal member1 final ACKs and actual transport
EOF after the separate wake fix, but subsequent fixture errors prevent the final
strict metadata result. The tenant lane owns its duplicate-ACK fixture correction;
no generic Sender/capacity/production change or e1cb fix is made here.
