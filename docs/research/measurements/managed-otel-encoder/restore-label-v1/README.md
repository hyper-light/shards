# Name the VMM restore count precisely

This is a scoped correction to the in-progress, unadopted codec contract at
`11d1c04d27a52133f3447321fd4b7d1057ce0212`. It is not a claimed live reproduced
export bug or a lifecycle implementation. Root's source review established that
[control::GENERATION](../../../../../crates/abi/src/lib.rs) is the guest-lineage
restore count: zero at initial boot, then one more for each snapshot restore.

The external resource attribute is now `shards.microvm.restore.generation`, always
an exact decimal stringValue. The former `shards.microvm.generation` is not emitted.
It does not identify an owner-output management generation, promise uniqueness
across restarts or establish a universal management epoch. The internal `write`
argument and its u64 type remain unchanged. No additional principal, identity
store, control state, lifecycle mechanism, cache or policy is introduced.

POLICY_READ: implementer `/root/otel_cgroup_encoder`, current origin/main
`33df95b08ad8f95bddeaa1c48acf0c64386e0eac`, full AGENTS.md/R01–R16/evidence/
CLAUDE/current handoff reread before edits, rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Owned paths remain the codec and managed-otel-encoder evidence only. The new
branch/worktree starts at 11d1; the previous branch/worktree and raw fixture remain
unchanged.

Actual light checks on this arm64 Mac passed: 10 unit tests (zero failures,
ignored or filtered), strict standalone all-target Clippy, independent Python
JSON/selected contract check, rustfmt, policy check and staged whitespace.
`../check.sh` writes this revision's results only into this directory's `raw/`,
using `~/.cache/managed-otel-encoder-restore-label-11d1` under lockf/-j4. Original
11d1 raw files were compared byte-for-byte with `git show 11d1:<path>`; their hashes
are retained in `raw/environment.json`. New unit output is preserved compressed
without trimming libtest's final blank line. No binary is committed.

The [original R01–R16 accounting](../README.md#r01r16-accounting-and-remaining-work)
and its explicit remaining gates continue to apply. This follow-up changes one
resource key, its API comment, the matching assertions/checker and their evidence
paths. It introduces no production literal, algorithm or platform-specific path.
Root owns actual caller/Collector/oracle validation, unknown cumulative-start and
restart/restore semantics, mutations, profiles, seven-target/real-VM gates and
main merge. None is claimed passed here.
