# Decoded terminal/replay control boundaries

Three additional pure tests exercise existing remote Service contracts without
changing production code: a Finished cursor may resubscribe at its exact tail
but cannot accept another ACK; Aborted cannot reopen even when accepted equals
consumed; disconnect of a partially written finishing record resets only its
write cursor and requires replay before ACK. These are source/state boundary
checks, not a new reproduced production bug or actual network replay result.

POLICY_READ IMPLEMENTER `01a1c5d2ee5871a2a9a9cdc9f56793f0`, ROOT coordinator
`01a148884a4770fdaf0111f3f792a248`. Full canonical AGENTS.md, development-rules
R01–R16, evidence checklist, CLAUDE.md, managed-runtime-plan.md and
`/Users/adalundhe/g121-handoff.md` read after compaction. Policyfiles f5cec365;
rules SHA256 `d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Base `1d0588b28f839fd0fec049e4bcb00e261e28c3aa`. Scope/next-case readback sent
before edits, key `refactor-next-terminal-control-fixture-1d05-v1`.

Owned paths: `crates/remote/src/managed.rs` cfg(test) child module
`terminal_controls` and this directory only. Existing clean worktree reused on
`p/managed-operator-terminal-boundaries`. The terminal implementation was already
signed/pushed separately as `d26084720cb86e530bf9f9b69af91984b6c85539` on
`p/managed-terminal-publication`; switching to this main-based test branch did
not lose/revert that checkpoint. Existing c2cedada's five cases, unmerged82425
ABI tests, e1cb/611777 proposals and d8aaa451 Poller harness remain preserved.
This branch does not silently compose those unmerged changes or probe logging.

## Concrete contract and cases

The existing methods are authoritative: input decodes fixed Subscribe/Consumed
frames; check_scope matches identity and generation; subscribe matches a retained
record/start or Finished tail; consumed only releases AwaitingAck at its exact
checked end; disconnect keeps Finished/Aborted and resets a Record's written
cursor without replacing its bytes/start. Current production byte prefix is
identical to the base; no new state rule is implemented by these tests.

1. Finished tails at zero and the maximum wire cursor survive disconnect. A
   decoded adjacent subscription is refused; the exact decoded tail subscribes.
   Decoded duplicate ACK is refused, repeated disconnect retains Finished and
   Finishing. This does not exercise transport FIN retransmission or new peer I/O.
2. Aborted with equal accepted/consumed at zero or the maximum cursor refuses
   decoded subscription and ACK before/after duplicate disconnect. State and
   Aborted ending remain unchanged; cursor equality cannot reopen it.
3. At the last legal two-byte record end, prepared written positions cover one
   header byte, the full data-frame header and one payload byte. Decoded ACK is
   refused before full send. Disconnect resets written to zero, preserves the
   actual Vec pointer/bytes/start and Finishing, accepts only the replay start,
   and still refuses ACK until replay has been sent. Prepared partial state is a
   unit boundary, not evidence of an actual partial native write.

The initially considered overflowing internal AwaitingAck injection was omitted:
Producer already rejects start+length overflow before admission, and no admitted
example was identified. No product fix or extra defensive framework follows from
that source observation. These three cases fill distinct current terminal/
partial-record branches rather than duplicate c2cedada's fragmentation, malformed
first-frame and AwaitingAck replay cases.

The semantics keep application consumption separate from QUIC stream FIN and
durability, as in [RFC9000 §§3.1–3.2/4.5](https://www.rfc-editor.org/rfc/rfc9000.html#section-3.1)
and pinned
[hyper-raft233b7fce](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/connection/streams/send.rs).
The alternative of only testing parser return values would miss retained-record
ownership and terminal-state interaction; these tests use actual Service methods
after actual wire decode. They deliberately do not mock Handler::controls/Io's
connection-close, authenticated peer or stream dispatch. Those real effects
remain ROOT checks.

## Actual light result

Kiln r26, NEW private source/target only:
`/home/adalundhe/managed-operator-terminal-light-1d05-v1/{source,target}`,
admitted1CPU/1GiB/2GiB; one Cargo build job/test thread, debug/incremental off.
Final directory366,080KiB. Headroom admission is not a hard quota or peak CPU/RSS
measurement. Fedora44 amd64 GNU, Linux6.19.10-300.fc44, rustc1.99.1
58cf8f9c3/LLVM23.1.1, cargo1.99.1. No local compilation or ROOT cache/target use.

```text
cargo test --offline -p shards-remote --lib managed::tests::terminal_controls:: \
  --target x86_64-unknown-linux-gnu --target-dir PRIVATE/target --jobs 1 -- --test-threads=1
3 passed; 0 failed; 0 ignored; exit0
cargo clippy --offline -p shards-remote --lib --tests \
  --target x86_64-unknown-linux-gnu --target-dir PRIVATE/target --jobs 1 -- -D warnings
exit0
rustfmt --edition 2024 --config-path SOURCE/rustfmt.toml \
  --config skip_children=true --check crates/remote/src/managed.rs
exit0
python3 scripts/check-development-policy
exit0; evidence/coordinator review still required
git diff --check
exit0
```

Only these three pure tests executed. Native Endpoint/driver/socket helpers were
compiled but not constructed or run. The private filtered sparse workspace uses
actual remote/ABI/IPC/testdir packages and pinned provider, narrowing only private
workspace membership. No transport model or dependency stub replaces their code.
Ordinary AWS-LC build notices and the unused-rquickjs patch warning remain in logs.

[raw/commands.json](raw/commands.json) retains exact commands/status/log hashes,
with full logs and toolchain under raw. All17 transferred raw members are verified
by [MANIFEST.json](MANIFEST.json). [driver-r26.py](driver-r26.py) is the exact
executed Python payload copied after execution; outer command was
`python3 - <<'PY' ... PY`. It retains the submitted source bundle. Source identity:

- full remote/managed.rs SHA256
  `27f55c48b035aeb8e91db009b7b15542113d0f110a972f37c4ddde21d4faa00d`;
- byte-identical production prefix SHA256
  `abf3c9e06cd65f701dc60c576dafac414920baedfd0192b45b537f41a346624f`.

## R01–R16 and owed ROOT gates

| Rule | Evidence and explicit limit |
| --- | --- |
| R01 | Production byte-identical; test assertions permitted. Actual package lib/tests Clippy0. No universal panic claim. |
| R02 | Existing concrete source contracts, wire decode and pinned primary stream semantics above. Test composition chosen over parser-only checks; no architectural decision. |
| R03 | No production literal/capacity change. Fixture offsets derive from wire maximum and real frame/payload sizes; duplicate count is a test event, not policy. |
| R04 | Actual typed parser/Service state checks preserve exact refusal, terminal, cursor and retained-byte ownership. No new production state or string control. |
| R05 | Existing waits/caches unchanged; pure tests call no waits, endpoint or new owner/manager. No polling/cache work proposed. |
| R06 | One focused child test module, three distinct branches and two small decode assertions; no broad framework. |
| R07 | Comments/docs describe prepared-state and unrun-I/O limits; no unsafe code. |
| R08 | No competitive baseline or4–5× claim; measurements ROOT-owned/unrun. |
| R09 | No production fix or new bug claim; these close specific unverified pure boundaries. Impossible overflow injection was not treated as a live defect. |
| R10 | Exact source/commands/environment and raw green logs retained; actual malformed peer effects/meaningful mutations unrun and ROOT-owned. |
| R11 | GNU amd64 light only; other targets/hosts/VMs unverified for this source. macOS arm64 only; Windows ARM stays paused. |
| R12 | Shared existing remote implementation; no OS duplication, extra installed binary, privilege or crypto change. |
| R13 | Production unchanged; finite focused cases and no speculative interface. |
| R14 | No profiling/latency/allocation/fault/CPU/resource-return inference; all such work ROOT-owned. |
| R15 | Scoped implementer source/light, no children/auto goals/audits/main merge. Separate signed named-path checkpoint; overall lane remains active. |
| R16 | No active local target/cache, builds or new full worktree, mirror or large archive. Private remote source/target/frozen evidence preserved; protected ROOT and paused VM paths untouched. |

ROOT owes combined adoption review with c2/d260 if desired, full managed suite,
real malformed/replayed/partial-control peer effects and cleanup, independent
guards/removals/restores, supported-target/native gates and actual disconnect/
reconnect/revoke behavior. This source-only test batch does not hold the already
returned terminal implementation or ROOT's strict normal/stdio VM reruns.
