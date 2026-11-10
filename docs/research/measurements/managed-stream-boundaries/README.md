# Managed wire and stream-state boundary compositions

Four tests appended inside `crates/abi/src/run/managed.rs`'s existing cfg(test)
module exercise the actual `Record::encode` → `Record::decode` →
`StreamState::accept` seam. Production source and all existing tests remain
byte-identical; no contract or production fix accompanies the tests. There was
no unexpected typed result in the light run and no new bug/reproduction claim.

POLICY_READ implementer: full current canonical AGENTS.md, development-rules.md
R01–R16, development-evidence.md, CLAUDE.md, managed-runtime-plan.md and current
`/Users/adalundhe/g121-handoff.md` reread before work. Base/canonical revision:
`24f48ee5ebf198485117aa93bf47685ad7d7d53f`; policy file revision `f5cec365`,
rules SHA256 `d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Branch `p/managed-stream-boundaries` uses the prior clean worktree; the pushed
`p/managed-network-assertions` checkpoint e163 is preserved. Owned paths are
only this evidence directory and the existing managed.rs test module. Root
retains all audits, campaigns, profiling, benchmarking and main merges.

## Missing compositions added

Existing tests already exercised standalone truncated headers, malformed tags,
basic transitions and an encoded-length overflow. These cases add state
preservation and successful continuation through the real wire path:

| Test, source line | Exact condition and required result |
| --- | --- |
| `wire_offset_overflow_preserves_state_and_exact_boundary_can_end`, 552 | Two-byte stack payload starts at `u64::MAX - payload.len()`. Tampering only the encoded offset one byte higher makes decode→accept return Offset and leaves prior state intact; encoding that overflowing record also returns Offset. The last legal start then advances to exactly u64::MAX and an encoded/decoded End succeeds there. |
| `decoded_transition_rejections_allow_the_next_valid_record`, 595 | Encoded/decoded Data before Open returns State, then valid Open succeeds. Duplicate Open returns State; ahead-of-current-offset Data/End/Aborted return Offset. Each leaves an existing Open state unchanged and allows the same next valid Data to advance normally. |
| `decoded_terminals_reject_subsequent_well_formed_data`, 645 | Actual encoded/decoded Open→Data→End and Open→Data→Aborted reach their distinct exact terminal states. A later well-formed Data at the matching final offset returns State and leaves each terminal unchanged. |
| `malformed_wire_boundaries_leave_an_open_stream_usable`, 701 | Header minus one byte returns Header; Data header without payload returns Payload. Unsupported next version, nonzero reserved byte, invalid UTF-8 Open, End with an extra byte, Aborted lengths zero/one short/one long and errno zero/minimum signed value return their exact typed errors. Every rejection preserves an already-open stream and is followed by valid encoded/decoded Data. |

The private six-line `accept_encoded` test helper is used by all four concrete
compositions and performs the real encode/decode/state calls. It introduces no
production abstraction or stub. Its stack buffer is HEADER plus an i32 error
field, enough for these one-/two-byte payloads and Aborted. The malformed-wire
buffer adds one u8 to exercise exactly one byte beyond the abort field. Offset
byte positions derive from HEADER and u64 width; the reserved tag position
derives from HEADER minus the u32/u64 fields. All size boundaries come from ABI
field widths and adjacent wire lengths. u32::MAX, u64::MAX, i32::MAX/i32::MIN,
zero, and u8::MAX exercise their actual integer/protocol boundaries. The seeded
Open offset avoids allocating or transmitting a huge prefix. These are finite
fixture inputs, not new production defaults or exhaustive fuzz coverage.

## Actual light checks

Kiln r4: Fedora amd64 runner `8fadc1ec213332e16cb0d915a3fb913f`,
Linux 6.19.10-300.fc44/glibc 2.43, rustc 1.99.1 (58cf8f9c3),
cargo 1.99.1 (5f94df478). Headroom request: 1 CPU/1 GiB/2 GiB;
`CARGO_BUILD_JOBS=1`, one test thread, fresh private source/targets at
`~/managed-stream-boundaries-light-24f-v1`. No protected coordinator target writes.

[light-check.py](light-check.py) copies the complete actual ABI crate unchanged,
including its real build.rs, into a minimal workspace retaining the repository's
package metadata, Rust/Clippy lints, Clippy configuration and cargo target
configuration. There are no inert stubs. The integration allocation test is
compiled by scoped Clippy, but only `--lib` tests execute; no allocation profile
or integration campaign runs.

```text
python3 light-check.py REPO NEW_PRIVATE_DIRECTORY
cargo test --offline -p shards-abi --lib --target TARGET -- --test-threads=1
cargo clippy --offline -p shards-abi --lib --tests --target TARGET -- -D warnings
```

| Target | Actual library tests | Scoped Clippy |
| --- | --- | --- |
| x86_64-unknown-linux-gnu | 26 passed, 0 failed, 0 ignored; exit 0 | exit 0 |
| x86_64-unknown-linux-musl | 26 passed, 0 failed, 0 ignored; exit 0 | exit 0 |

Full actual commands, raw output and individual log hashes are in
[light-result.json](light-result.json). Source and preservation hashes are in
[source-identity.json](source-identity.json). Local focused rustfmt, policy and
diff checks pass; no local build ran. This Mac retained more than 30 GiB free.

Source SHA256 before:
`53dc711b4d434252b5d4cc6b136ee1244656d3201e90850e24b42c1c8c29cd01`;
after/actual tested input:
`12cb5220b60ddd80bccd197310a127070ba2ed92d09ebb78f41abd49b8ba0786`.
light-check.py SHA256:
`b21aee394ba3ee43fecd6ac50958d47e0d8b6781038131165a289c556d8b4042`.

The unchanged real ABI build.rs hashes all source text, including cfg(test),
into IDENTITY. Its source algorithm recomputes the base as
`0x99b4d607a9dc2b2f` and the new source as `0xeef37258a52f4b5e`; actual GNU and
musl generated identity.rs files report the latter. Host and guest artifacts
must therefore rebuild together. Reusing an old guest with a new host cannot
establish a combined passing gate. Hash behavior itself is unchanged.

## Owed coordinator work and policy limits

Root still owns review, exact combined-workspace checks, seven-target lint/build
and relevant host/guest rebuilding and identity validation. Independent guard
mutations remain unrun: wire payload/version/reserved/offset checks and stream
lifecycle/offset/terminal checks must each reject the corresponding forbidden
acceptance or state advance; restore source and require ordinary passes. No
mutation, real VM/socket/BPF/namespace, heavy E2E, chaos, stress, profile or
benchmark campaign was run here. Other platforms/hardware remain unverified by
these GNU/musl results; Windows ARM remains paused.

R01/R04/R06/R07/R13: assertions are test-only; explicit typed wire/state
transitions, exact errors, unchanged rejected state and valid continuation are
checked without changing production paths. R02/R03/R05/R12: this is a narrowly
authorized test composition at an existing concrete seam; no broader design,
production literal, platform adapter, wait or cache is introduced. R09/R10:
expected boundary outcomes all held, with exact raw logs and no speculative
fix; guard mutations and broader live validation remain root-owned. R08/R14:
no cost/performance/allocation/fault comparison was run or claimed. R11:
actual GNU/musl pure checks only; broader target/hardware coverage remains owed.
R15: scoped implementation and light tests only, no children/main merge, signed
named-path checkpoint for coordinator review. R16: prior checkpoint, source,
evidence, private targets and paused hardware preserved; no privilege, security,
trust or crypto change.
