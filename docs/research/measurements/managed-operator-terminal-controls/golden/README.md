# Independent version1 control bytes and decoded refusal

Two additional pure cases complement61a55b60's three terminal/replay tests. The
existing repeated/MAX-value round trips cannot independently expose a shared
encoder/decoder byte-order or layout change. Fixed byte vectors now check each
direction separately, with nonuniform run, generation, member and cursor bytes.
No production parser, state, wire or authority rule changes.

POLICY_READ IMPLEMENTER `01a1c5d2ee5871a2a9a9cdc9f56793f0`, ROOT
`01a148884a4770fdaf0111f3f792a248`. Full canonical policy R01–R16/evidence,
AGENTS, CLAUDE, runtime plan and `/Users/adalundhe/g121-handoff.md` reread after
compaction before this sequence; rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`, policyfiles
f5cec365 unchanged through canonical eb5654d6. Proposed before edits under
`refactor-next-golden-wire-proposal-v1`. Own only existing remote cfg(test)
`terminal_controls` additions and this evidence directory on the same own
`p/managed-operator-terminal-boundaries` branch, base
`61a55b60ec5e578d668583717092e25c97b296b1`. d26084720 terminal implementation is
frozen/pushed separately and remains ROOT's full-caller/VM/matrix candidate.

The primary wire declarations are pinned
[frame.rs](https://github.com/hyper-light/shards/blob/61a55b60ec5e578d668583717092e25c97b296b1/crates/remote/src/frame.rs)
and [managed.rs](https://github.com/hyper-light/shards/blob/61a55b60ec5e578d668583717092e25c97b296b1/crates/remote/src/managed.rs).
They define version1 ALPN, Exec=1, Stdin=16, u32 big-endian payload length,
run32/generation8/member4/channel1, and cursor8. The fixed subscription payload
length is53 and ACK payload length8 by that existing grammar. Golden literals
are test protocol bytes, not new production sizing constants. An intentional
protocol version change requires separate compatibility work rather than simply
calling a new matching round trip sufficient.

The first case checks exact Subscribe/Consumed encoding against independently
written bytes and decodes those same literal bytes to independently stated typed
values. High bits and nonuniform bytes expose unsigned/endianness/field-order
changes that all-zero/all-one values obscure. The second case decodes controlled
foreign run/member/channel combinations, including concurrent generation
mismatch, and requires Scope refusal; only generation mismatch requires Stale.
Adjacent malformed subscription lengths require typed Length refusal. This is
the existing method precedence, not a new security claim or a real unauthorized
peer campaign. No Handler/Io, TLS handshake, subscription delivery or backend
effect is modeled by these pure cases.

Actual Kiln r30: NEW private Fedora
`/home/adalundhe/managed-operator-golden-light-61a5-v1/{source,target}`,
1CPU/1GiB/2GiB admission, one Cargo job/test thread, incremental/debug off.
Directory366,116KiB. Same Fedora44 GNU amd64, rustc1.99.1/LLVM23.1.1/cargo1.99.1
as the [prior record](../README.md); exact environment remains in raw logs.
No local build, new full worktree, child, ROOT cache write or native/VM execution.
Admission is not a measured peak or hard quota.

```text
cargo test --offline -p shards-remote --lib managed::tests::terminal_controls:: \
  --target x86_64-unknown-linux-gnu --target-dir PRIVATE/target --jobs 1 -- --test-threads=1
5 passed; 0 failed; 0 ignored; exit0 (prior3 plus new2)
cargo clippy --offline -p shards-remote --lib --tests \
  --target x86_64-unknown-linux-gnu --target-dir PRIVATE/target --jobs 1 -- -D warnings
exit0
rustfmt --edition 2024 --config-path SOURCE/rustfmt.toml \
  --config skip_children=true --check crates/remote/src/managed.rs
exit0
python3 scripts/check-development-policy
exit0
git diff --check
exit0
```

Only the five pure cases executed; native helper code was compiled, not run.
The actual four-crate graph/provider is retained with only private workspace
membership narrowed, no stubs or provider changes. Normal build notices remain
in raw. [MANIFEST.json](MANIFEST.json) verifies all17 transferred raw members;
[commands](raw/commands.json), full logs, resolved lock and
[source identity](raw/source.json) are retained. The exact executed
[driver-r30.py](driver-r30.py) was copied after execution; outer command was
`python3 - <<'PY' ... PY`.

Final remote source SHA256
`8851bd31350edbae3d1ba022f3850105d540030a46cd8f761a32da6d40ff51b4`;
production prefix remains byte-identical SHA256
`abf3c9e06cd65f701dc60c576dafac414920baedfd0192b45b537f41a346624f`.
No ABI source or separately unmerged82425 identity was adopted.

R01: production unchanged, actual package Clippy0; test assertions allowed.
R02: pinned existing primary wire source and the independent-byte alternative
above, no architectural decision. R03: only test grammar bytes/derived offsets,
no production constant. R04: exact typed layout/refusal checked, no string
protocol. R05: no wait/cache/owner change or polling. R06: two focused cases,
one fixture scope, no framework. R07: comment explains independent fixed bytes;
no unsafe code. R08: no comparative/performance claim. R09: no production defect
claimed or fixed. R10: exact raw commands/source/greens retained, Root effect/
guard-removal/restoration checks unrun. R11: GNU amd64 light only; other targets,
native/VM combinations unverified, macOS arm64 only and Windows ARM paused.
R12: shared remote seam only, no OS duplication/binary/privilege/crypto change.
R13: no new production interface. R14: no profiles/CPU/fault/latency/allocation
claim. R15: scoped implementation/light, no children/goals expansion/main merge;
ROOT retains all campaigns/gates. R16: private remote source/target preserved,
no local target/cache/build/cleanup, all prior branches/artifacts retained.

ROOT still owes adoption/combined review with other optional wire fixtures,
actual peer/stream/identity/refusal effects and cleanup, meaningful guard
removals/restores, supported/native checks. This independent test checkpoint
does not hold d260's strict normal/stdio validation or close the overall lane.
