# Finite writer mechanics checkpoint

The original seven writer mechanics cases each passed once on GNU amd64. The
full original-path server test graph compiled, test-only Clippy with `-D warnings`
passed and both owned files passed rustfmt. Production write.rs remains exactly
signed198bd; the only source change is the audited test-only correction14693.
This finishes the assigned finite source/light-test batch. The real CONTROL3
Write metadata/caller and production adoption remain unimplemented by this lane.

## Exact source and actual checks

Implementer `01a1c9f8742d77d89711aa024bb3978d`, ROOT coordinator
`01a148884a4770fdaf0111f3f792a248`; policy06ae067c0/df2 as recorded in
`../closure-v3/POLICY_READ.json` and [the R01–R16 account](POLICY-EVIDENCE.md).
Ownership stayed within the two writer source paths and writer measurements;
there were no children, foreign-worktree edits or local Rust commands.

The actual compiled graph uses immutable8b1abae44 server/ABI/IPC/testdir,
original complete workspace membership, Cargo.lock, dependency versions,
vendored sources, profiles and lints. All immutable test includes keep their
original paths. The only private module change is `pub(crate) mod write;`.
Owned production/test hashes are in [source-freeze.json](source-freeze.json).
All4357 input identities are in `v4/source.json` inside the retained raw archive.
Input source.tar.gz is30116189B SHA256
`017823571e5b9279679c00dd639f15625607df8048203324ddc5e712671b687c`.
It remains protected, untracked beside this README; it is not a claimed current
main or combined integration gate. `prepare-v4.py` recreates that export from
the exact Git source and owned files when its output files do not already exist.

All runs used normal Adacachy451397 admission:2CPU/2GiB/4GiB, Cargoj2,
GNUamd64, Rust1.98.1, one original600s limit, compile180s/case15s. Sources,
targets, cargo homes, temporary files and frozen executables were private.

| Actual operation | Result | Retained proof |
| --- | --- | --- |
| Original-path server test compilation with `--locked` | 0,90.101s | raw `v4/compile.*` |
| Exact binary list and source/binary/case receipt before first case | 0; seven selected,92 others unselected | raw `v4/binary.json`, `v4/list.*`, `v4/precase.json` |
| Original seven writer cases | Each1 passed/0failed/0ignored, once; noSKIP or timeout | raw `v4/case-0.*` through `case-6.*` |
| Combined `--bin shards-server --tests` Clippy | 101,6.545s;89 unused PRODUCTION APIs | raw `v4/strict-tests.*`; failure is retained, not passed |
| Intended `--tests`-only Clippy, unchanged source/fresh output target | 0,53.361s, `-D warnings` | raw-strict-only `strict-test-only.*` |
| Owned write.rs and write/tests.rs rustfmt check | 0,0.034s | raw-strict-only `format-owned.*` |
| Current policy script on exact06ae mechanical inputs via git show | 0 | `../closure-v3/policy-check.json` |

The actual sparse-worktree policy attempt failed because ci.yml is absent; that
failure is retained separately from the exact-current-input projection pass.
The original combined lint selected the production binary as well as its test
target. No unused production API was suppressed or given an invented caller.
Test-only lint omits that extra production selection and does not close the89
production errors. It executed no test, helper or actor. The seven cases were
never retried.

The compiler-only continuation used a new output target/cache while reading the
same frozen source and preserving the original binary and target. Its cutoff
was conservatively derived from the original root's initial mtime+570s, earlier
than the original driver's585s work cutoff/600s outer bound. It finished at
1794390596.0280762, before1794390892.6726599. Full180s lint/15s format/15s reap
budgets were required at entry. No deadline was reset or extended.

## Original seven and actual effect witnesses

Each command used the frozen exact binary, full case name, `--exact --nocapture
--test-threads=1` and the original15s case deadline:

1. `cancellation_before_commit_and_empty_write_preserve_actual_file`: actual
   target remains unchanged after cancellation and an empty write.
2. `checked_range_refuses_overflow_without_opening_or_mutating_an_object`:
   original representability/overflow predicates; Request has no object input.
3. `directory_object_is_returned_and_never_used_for_write`: the returned actual
   descriptor still stats as the same directory kind.
4. `literal_positional_write_preserves_prefix_suffix_and_shared_offset`: exact
   binary payload/prefix/suffix, actual dev/inode, prepared offset4 and unchanged
   shared open-file position.
5. `prepared_object_survives_rename_and_does_not_write_replacement`: actual held
   old object receives bytes and replacement destination remains byte-identical.
6. `readonly_and_append_modes_refuse_before_any_bytes_change`: typed mode
   refusals retain live objects/request bytes and the real file remains unchanged.
7. `same_canonical_audit_reserves_intent_and_terminal_before_ordinary_effect`:
   existing canonical cfg(test) Journal fixture records actualI/T bytes around
   the ordinary real file write; it does not mint production rights or backing.

The full raw case stdout establishes each single pass. The original exact source
establishes its assertions; no new output or weaker predicate substitutes for
the effect witness. No positive partial write/fault was injected. Partial/error
paths, guard-removal sensitivity, hard backing, parent/kernel costs and actual
owner/admission/transport remain unverified here.

The14 v4 runtime/compiler/list/formatter direct children all reported pidfd exit
readiness, wait/ECHILD and parent FD9→9; these are direct-child receipts only.
The final original TMP contains only the empty `shards-tests` parent directory.
No whole-population/kernel return or leak-free claim follows. The frozen
38204488B executable remains on451397 at
`/home/adalundhe/Projects/managed-native-file-writer-198bd-v4-01a1c9f8-1794390298/frozen/writer-tests`,
SHA256 `b1a1baefefc35a0cb58f22f8fbe8f35f497c8a45c0805611b48acc80a7859015`.

## Retained failures, evidence and ownership handoff

Prior v1 failed before Rust on a copied Cargo patch path; v2 failed before writer
cases on its missing immutable audit test include. Their original source/raw
archive/targets remain untouched. New v3's membership-only projection was also
refused by `--locked` before compilation; all2506 inputs remained unchanged.
V4 preserves the original full workspace and lock, with no dependency update or
flag/lint/crypto relaxation. Its source and every runtime result remain frozen.

Recovered [raw-v3-v4.tar.gz](raw-v3-v4.tar.gz) is397735B, SHA256
`dafd70838021eae64aa92da0f3d3ecd90b3e5e03716dbd65fb17aedfea587d0d`:
all72 payloads+recovery manifest73 members and both original manifests verified.
[raw-strict-only.tar.gz](raw-strict-only.tar.gz) is3556B, SHA256
`374a1694c766a930eb75c6c9d95da84b724c3a96424ace3559e77ba7dff9bd17`:
all11 payloads+recovery manifest12 members and original manifest verified.
Readable recovered copies remain protected/untracked in `raw/` and
`raw-strict-only/`; the commit carries the exact archives and verification JSONs.

Audit explicitly confirms the actual CONTROL3 client Stage is Process-only and
no production Write metadata/caller exists yet. Its Facts/Records source is
DATA; it does not supply a grant, actual held root/path selection, current owner,
payload/backing acquisition or initialized Session. The production offset getter
remains uncomposed. Native/audit retain that real consumer work. This lane has
only confirmed its existing effect fixture observes the matching actual offset.

Audit's five portable record/File tests remain auto-review HELD. Their newer
native_write module is absent from this older exact graph; no such case is linked,
executed, renamed or rerouted. Only the distinct original seven above ran.

ROOT retains audits, profiling, benchmarks, heavy E2E/VM/chaos/integration,
all-seven-target/real-host gates and main merges. No Mac, cgroup, VM, helper,
security/root, cleanup, benchmark, fault or held campaign ran. Linuxmusl/arm64,
Windowsamd64/arm64 and macOSarm64 are unrun here; Intel Mac is unsupported and
the WindowsARM VM remains paused. Active owned jobs0; no cases remain queued in
this finite batch. Broader P0–P13/G01–G25 remain incomplete.
