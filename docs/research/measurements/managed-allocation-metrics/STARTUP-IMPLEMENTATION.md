# Approved startup snapshot implementation

Implementer `01a1c5be192972d8b05dc7c207cc6fca`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Before edits, all required canonical policy,
plan and handoffs were reread at main
`eb5654d6f5356cacaca536ff51f14a632fa7e801`; rules SHA-256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Readback key: `managed-startup-implementation-policy-read-eb565-v1`.

Root authorized the EXACT [startup proposal](STARTUP-PROPOSAL.md) and historical
147-line patch after full review. It is now applied:115 additions/3 deletions,
fixture SHA-256 `874e8b38b46479d4199ac53940a84a8ac6c8f7199998a4edb591eacab81c515e`,
exactly the reviewed draft. All five original resolver expressions remain once,
in their original order; the artifact read/hash expression is unchanged. The
original `run` body and later cleanup/identity/FD/deadline/terminal tail are
byte-identical except the reviewed deferred startup-record write.

Fixed snapshot fields observe test entry, supplied inputs, each existing resolver
and artifact-value boundary, and completed input recording. They are aggregate
counter observations across observer threads; they do not attribute allocations
exclusively to those calls. `artifact_value_ready` precedes outer input-JSON
conversion/insertion. Earlier runtime work remains outside these intervals.
Requested/live/process-lifetime peak and sticky Unknown semantics remain; peaks
are not subtracted. An early panic outside the existing unwind boundary emits no
startup record, not zero, NotReached data or cleanup success. No Drop logger or
new catch changes that behavior.

## Private light result

The private light harness compiles byte-extracted actual startup structs and
the existing `allocation_snapshot_record` function, actual unmodified
`managed_alloc.rs`, real pinned `serde_json1.0.151`, and
[three shape tests](startup_shape_tests.rs). Its private `MANAGED_ALLOC` object
is unregistered. It tests types/record shapes, not process allocation counts.
No dummy caller, artifact resolver/hash, native API, VM or actual full fixture is
compiled/executed. Root owns the complete caller/borrow/integration gate.

[The narrow driver](run-startup-light.py) uses serial1CPU/1GiB/2GiB admission,
NEW remote source/target/log directories, private Cargo home/target/temp storage,
explicit installed Rust1.98.1 via the rustup proxy, one compile job and no
incremental build. It records exact extracted-source spans/hashes, Cargo.lock,
dependency checksum, tools, commands, exits and test-binary hash. The three unit
cases cover initial reached/not-reached states, reached boundary/semantics labels,
and Unknown values without counter mutation or invalid native operations.
The existing counter tests are compiled but filtered out of this shape run.

Source `4bc28256ee1285715e5573e9f4ba6b02caf436aa` passed build0, three shape
tests0 (five counter tests filtered out), and Clippy0 on Fedora GNU through
Kiln `8fadc1ec213332e16cb0d915a3fb913f`. [All20 original files](records/startup-4bc28256e)
were transferred and verified byte-for-byte, including the source spans,
commands, Cargo.lock and serde_json checksum. Actual rustc1.98.1 commit48a229ce,
LLVM22.1.8 and cargo1.98.1 commit797e8a9b were recorded; the MSRV is not used as
toolchain evidence. No actual artifact closure/full caller was typechecked.

The private test executable remains at
`/home/adalundhe/managed-allocation-metrics-target-startup-4bc28256ee1285715e5573e9f4ba6b02caf436aa/target/debug/deps/managed_startup_shape-287adfa93c4609bc`,
SHA-256 `7f51e090134b3e4251c0663ae3cd2c51cad83cf40246edd58b287f3731a93fa7`.
It was hashed before execution and rechecked after Clippy/at transfer. All own
jobs have completed; private source, Cargo cache, final binary and evidence are
preserved. No root cache/target/source was written and no local build ran.

## R01–R16 accounting and remaining root work

| Rule | Current evidence / limitation |
| --- | --- |
| R01 | Test-only straight-line snapshots; no production or hook change. Test assertions are outside allocator hooks. |
| R02 | Root-reviewed Rust operand/temporary/macro contracts and actual five callers; root sampled setup cause remains Unknown. |
| R03 | Fixed fields reflect existing callers/boundaries; no production literal, selected n, timing allowance or cache. |
| R04 | Explicit Observed/NotReached/Unknown record states; early panic still yields no record. No quiescence claim. |
| R05 | Private fixed structs at the existing caller, no manager/tracer/wait/cache abstraction. |
| R06 | Exact reviewed insertions; original protocol and cleanup source comparisons retained. Full-caller review/gate remains root-owned. |
| R07 | Boundary/scope metadata explains uncertainty; existing safety comments preserved. |
| R08 | No measured optimization or competitive result; comparative target remains open. |
| R09 | Measurement instrumentation, not a guessed defect fix or artifact-hashing diagnosis. |
| R10 | Exact proposal/application hashes and light records retained; no actual new-source integration result claimed. |
| R11 | Light type/shape target only; root owes full supported-target and native-host/caller gates. macOS ARM64 only, Windows ARM paused. |
| R12 | Existing System wrapper/platform seams unchanged; no new adapter or installed binary. |
| R13 | Same reviewed fixed fields; original getters/read/hash/order and later behavior intact. |
| R14 | Observer Rust requested-byte/event counters, independent loads/lifetime peak; physical/foreign/daemon/VM/kernel/copy/CPU/profile attribution remains root work. |
| R15 | Scoped implementer light only; root all actual caller/helper, N1, fault/profile/bench/paired/guard/cleanup/target gates and main merge. No children/goal dispatch/self-completion. |
| R16 | No local build/new worktree/large copy/cache deletion or root cache writes; private remote admission unchanged, source/evidence/final artifacts preserved. |

Root must rebuild matching host/guest helpers together, compile the real caller
and original five artifact closure/borrow expressions, run normal cases and real
startup N1/error/cleanup/guard cases, then all required target/profile/paired
campaigns. Source-specific f53f native gates precede this change and are not new
startup passes. Protected root f53f/all five eaf0 artifacts remain untouched.
The paired14 batch and22-line report proposal remain separate/unreviewed and
unmodified. This checkpoint does not complete the sustained lane or parent goal.

## Root runtime-record criteria — unrun here

* On strict success, `allocation-startup.json` should contain Observed test-entry,
  supplied-input and completed-input fields plus all three boundaries for each
  of the five existing artifact roles. Each snapshot retains ConcurrentNonAtomic
  and individual Known/Unknown counts; Observed is not exclusive attribution.
* A prelude input/host/artifact panic outside the original unwind boundary should
  leave this file absent. Neither NotReached nor a cleanup-success record is
  substituted for that absence. Only root executes these failure cases.
* Live/requested-lifetime peak remain absolute bookkeeping observations. Do not
  assert coherent live/peak relationships from independent loads, subtract peaks,
  equate native fault categories or infer setup cause from a large interval.
* Original protocol/main progress, held identity/exit readiness, exact FD return,
  owner/observer joins, deadlines and terminal checks remain required. This
  record supplements those witnesses and does not prove full population/kernel
  charge return or parent/foreign-C/daemon/VM allocation completeness.

Read-only lineage verification confirms canonical native base `eb5654d6f` has
the exact reviewed31df9e2b fixture, and source4bc adds exactly115/3 lines to it.
No root working tree, index, cache, target or frozen artifact was changed.
