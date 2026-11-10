# Terminal publication ordering and metadata I/O fixture

This is fixture source for a coordinator-owned unverified obligation, **not a
new reproduced production defect or a production fix**. Logical `Life` becomes
terminal before fallible metadata publication. Successful/partial/failed append,
known logical outcome, transport stop request and actual endpoint retirement are
separate observations. The earlier actual normal/stdio defect and correction are
recorded in [managed-terminal-publication](../managed-terminal-publication/README.md)
and ROOT's source-specific native records. Their passing gates do not establish
slow/failed append or concurrent publication ordering.

## Identity and ownership

POLICY_READ implementer `/root/terminal_order_fixture`; coordinator `/root`
(Kiln `01a148884a4770fdaf0111f3f792a248`). Existing exclusive worktree
`/Users/adalundhe/Projects/shards/.claude/worktrees/managed-output-endpoint`,
branch `p/native-terminal-order-v1`, source
`b02e8592c547553c788702ac6c52ea6b633e875d`; current canonical policy read from
`origin/main` `eb5654d6f5356cacaca536ff51f14a632fa7e801`. Rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Read full `AGENTS.md`, `docs/development-rules.md` R01–R16,
`docs/development-evidence.md`, `CLAUDE.md`,
`docs/design/managed-runtime-plan.md`, `/Users/adalundhe/g121-handoff.md` and the
current-origin policy before edits. Read signed source
`d26084720cb86e530bf9f9b69af91984b6c85539`, its complete publication record,
`pure.py`, `output-calls.rs`, actual daemon Output/Life/credit construction and
actual remote Shared/Control/Service/Endpoint paths. Policy and concrete
fixture/caller readbacks preceded edits; ROOT accepted the scoped proposal.

Owned paths are this **new** directory and the **new**
[`terminal_order_tests.rs`](../../../../crates/shards/src/daemon/managed/terminal_order_tests.rs)
leaf. Parent `daemon/managed.rs` and `remote/managed.rs` remain byte-identical to
the prepared source. ROOT registers the leaf, compiles the complete actual
caller, executes all native/fault/order campaigns, reviews evidence and merges.
No child agents, production change, VM, new full local worktree, local cargo
target, host settings, privilege, trust or crypto changes. The paused Windows
ARM VM remains untouched.

## Actual seam and deliberately limited scaffold

Every native case constructs real private `Output`, `Life`, `Credits`, `Charge`
and `Learning` types, binds a real owner-authenticated loopback `Endpoint`,
retains its real `Producer`/`Control`, and runs `Endpoint::run` on an owned
thread. No alternative state machine or call-recording `Control` is supplied.
The metadata sink is an actual newly created private-directory `File`.

The stream is **empty and unsubscribed**. Actual `Output::finish` calls real
`Control::finish`, which makes the existing empty Waiting slot Finished(0).
There is no record submission, nonzero wire cursor, subscriber, authenticated
peer, application consumption ACK, FIN-ACK, actor or VM witness. Ended here is
the existing empty-stream state transition, not a nonzero output-delivery claim.
The zero-byte Charge and fixed-quantum/no-sample Learning fields are inert
constructor scaffold: admission, growth, throughput learning and accounting are
not exercised or measured. The one credit slot has no worker; real revocation is
checked, but no successful guest credit exchange is claimed.

| Exact native case / caller key | Controlled sequence and required witness |
| --- | --- |
| `native_owner_stop_after_finish_while_metadata_locked` / `owner-finished-lock` | Real finish establishes Finished(0). Caller owns metadata mutex; worker invokes unmodified `Output::abort`. Bounded real Shared callback receipt reports Aborted, sticky Life is Ended, actual endpoint returns and thread joins before mutex release, file remains empty. After release, exactly one complete row exists. |
| `native_owner_stop_before_finish_while_metadata_locked` / `owner-serving-lock` | Same actual abort/lock/retirement sequence from Serving. Life is Aborted(None), later finish is refused, and one aborted/unknown row appears only after unlock. |
| `native_completion_wins_then_owner_stops_while_metadata_locked` / `completion-lock` | Worker explicitly calls existing `resolve(Complete)`, sends its returned typed resolution, then calls existing `publish`. This is a **split method seam**, not an internal pause in the unmodified `complete` wrapper. Concurrent unmodified abort retires the actual driver while publication remains metadata-locked. Unlock permits the one ended/complete row; duplicate calls do not append another. |
| `native_readonly_file_error_keeps_logical_completion_and_retires_driver` / `readonly-file` | Unmodified `Output::complete` resolves empty End, attempts a real read-only File append and logs one error. Life remains Ended, bytes remain empty, and actual endpoint return/join must occur. Duplicate stop/completion does not fabricate a successful append. |
| Unix `native_file_limit_retains_partial_append_and_known_canonical_errno` / `file-limit` | Fresh child alone receives caller-selected RLIMIT_FSIZE and ignored SIGXFSZ. Unmodified complete attempts real File publication; exact strict prefix survives, one real publication error is logged, known supplied canonical EIO and unknown tail remain logical, and actual endpoint returns/joins. A separate postfailure File write records actual EFBIG; it is explicitly **not the swallowed original publish error's captured errno**. |

Each metadata row is checked again independently by the Python caller, including
exact bytes, no duplicate row and the supplied partial prefix. `canonical EIO`
is a fixture-supplied cause, not an observed guest read failure. Native stderr
retains the original actual append error rendered by production `log`.

The held mutex and callback/resolution receipts establish cooperative ordering;
there are no sleeps, repeated state probes or fabricated timing windows. The
callback reads real Shared state after Shared unlock and uses bounded
`try_send`; it performs no Life transition or metadata I/O. The registration
receipt is drained before starting the stop worker. No new production callback,
waiter, manager, polling loop or cache exists.

This does **not** inject or prove a kernel-blocked filesystem write. Metadata
mutex delay, real read-only failure and isolated-child file-size failure are the
focused mechanisms. OS/filesystem write stall, durability, nonzero consumed
records, competing channel appends and actual guest completion/owner-stop races
remain ROOT-owned obligations.

## Primary contracts and alternatives

Reviewed 2026-11-10 against official Rust 1.99.1 docs (58cf8f9c3), RFC9000
(May 2021), Linux man-pages getrlimit and Python subprocess/resource docs:

- [Mutex](https://doc.rust-lang.org/std/sync/struct.Mutex.html): holding the real
  metadata guard excludes append entry. Life and Shared are inspected through
  their actual separate locks; no inference is drawn from scheduling delay.
- [Condvar](https://doc.rust-lang.org/std/sync/struct.Condvar.html#method.wait):
  predicate ownership and notifications must be distinguished. Existing credit
  Condvar remains production-owned; fixture receipts use bounded channels and
  caller deadlines instead of periodic observations.
- [Write](https://doc.rust-lang.org/std/io/trait.Write.html#method.write_fmt):
  formatting may call writes repeatedly. A failed formatted append can leave an
  earlier successful prefix; a write error does not undo preceding writes.
- [getrlimit / RLIMIT_FSIZE](https://man7.org/linux/man-pages/man2/getrlimit.2.html):
  the owned child can lower its own file-size limit. SIGXFSZ disposition permits
  the File error path to execute. No parent or host-wide limit changes.
- [Python resource](https://docs.python.org/3/library/resource.html) and
  [subprocess](https://docs.python.org/3/library/subprocess.html): fresh child
  sets its own limit then execs the exact native case. `subprocess.run` enforces
  the native outer timeout, kills/waits on expiration and retains partial logs.
  No unsafe multithreaded `preexec_fn` is used.
- [RFC9000 §3.1](https://www.rfc-editor.org/rfc/rfc9000.html#section-3.1) and
  [pinned hyper-quic send states](https://github.com/hyper-light/hyper-raft/blob/233b7fcef7a363327c5439b191724a38065062cd/crates/hyper-quic/src/connection/streams/send.rs):
  application terminal status and transport ownership are separate. This empty
  fixture claims only the real driver's return/join, not packet/FIN delivery.

The existing extracted `output-calls.rs` was considered but its Control call
recorder cannot prove actual Shared or retirement. A new production fault-sink
interface was rejected because private child access already reaches the actual
types. Sleeps/polling, `/dev/full`, global disk filling, host-wide limits and
large owned-pipe pressure were unnecessary for these focused cases. Child FSIZE
provides an exact finite regular-file prefix; Windows FSIZE is unsupported by
this fixture and remains unverified. No production state or I/O policy changes.

## Coordinator recipe and explicit bounds

ROOT adds only this registration to its parent source:

```rust
#[cfg(test)]
mod terminal_order_tests;
```

Rebuild the actual source-matching daemon test binary and retain full command,
source revision/leaf/production hashes, binary SHA256 and compiler/build logs.
The caller records source/binary bytes before and after; a matching pair of
hashes alone cannot establish that a binary was compiled from that source.
Do not substitute the compile-only fragment artifact for the complete caller.

Run **one case per fresh process/directory**, on the supplied binary:

```sh
python3 docs/research/measurements/managed-terminal-order/caller.py \
  --binary /absolute/source-matching-shards-test-binary \
  --binary-sha256 "$BINARY_SHA256" \
  --source-root /absolute/coordinator-checkout \
  --source-revision "$SOURCE_REVISION" \
  --case owner-finished-lock \
  --case-ms "$CASE_MS" --timeout-seconds "$NATIVE_OUTER_SECONDS" \
  --output /absolute/new-private-case-directory
```

Keys are in the table. Only `file-limit` additionally requires
`--prefix-bytes "$STRICT_NONEMPTY_PREFIX_BYTES"`; it must be less than the
complete canonical/error terminal serialization. The limit applies to only the
new child, whose code sets SIGXFSZ handling immediately before exec. A launcher
receipt is not proof the selected native test ran. Caller requires the actual
entry marker, matching case completion marker, actual endpoint return, exactly
one passing selected test, independent exact File bytes and unchanged identities.
Zero-test filtered success, loader failure, native panic, timeout and missing
metadata remain failed/unverified.

The supplied `CASE_MS` begins before file/identity/endpoint setup and is shared
by all readiness/completion/cleanup waits without renewal. `NATIVE_OUTER_SECONDS`
must exceed it. Each source-identity git command also uses that supplied timeout;
ROOT supplies a finite whole-driver tool deadline covering setup, native process
and before/after verification. Native receive deadlines cannot interrupt an
individual blocking OS syscall; the process outer deadline is the final bound.
Cleanup requests stop without metadata I/O, joins only after completion readiness,
and reports unknown joins on deadline. Files/raw receipts are retained on every
failure; no cleanup deletes user data or evidence.

The case has one native test process, its harness/main and one selected test
thread, one real endpoint thread, and at most one publication/stop worker. It
binds one owned loopback endpoint, sends no actor/peer payload and writes at most
the small declared terminal serialization. These are fixture structure bounds,
not measured FD/RSS/kernel-resource-return claims. ROOT owns actual before/after
FD/handle/kernel facts, native campaigns and resource measurements.

## Light validation and limits

`test_caller.py` is pure evidence rejection coverage; it never runs native test
code. Its internal-child CLI rejection uses invalid prefix zero and exits before
limits or exec. `light.py` creates a compile-only workspace from exact production
Charge/Output/Life/Credits/lock/log fragments plus actual remote/registry crates
and the unchanged new leaf. Its sole behavior scaffold is unused
`build::host_available() -> None`. This is neither a full daemon build nor native
execution. Fragment/full-source hashes and generated files are retained.

`remote-light.py` uses a NEW sparse source, Cargo home and target, job1 and no
debug/incremental output on GNU amd64. Admission is 1 CPU / 1 GiB memory / 2 GiB
disk, not a measured peak or hard quota. Root paths/caches are never reused.
Initial remote launch omitted driver CLI arguments and exited2 before creating
the private path; corrected compile-only run is recorded separately. Exact
commands/results/source identities are retained under `raw/` and `MANIFEST.json`.
No native case has been executed by this implementer.

| Actual check | Outcome and retained scope |
| --- | --- |
| Local pure caller rejection tests | 11 passed; includes zero-test/launcher-only/wrong-case/timeout/missing-retirement rejection and invalid child CLI before limits/exec. |
| r2 first compile-only generated scaffold | `cargo check --tests -j1` exited101: missing PoisonError import, plus unused `run` import. No native execution. Exact failed generated source/log retained. |
| r3 corrected compile-only scaffold | Exact same leaf, actual remote/registry dependencies, `cargo check --tests -j1` exited0; scoped Clippy `--tests --no-deps -j1 -- -D warnings` exited0. No native execution. |
| Local final policy/format/whitespace | Required policy script, explicit Rust2024/110-column rustfmt, Python AST and named staged diff checks recorded under `raw/local/`. |

Remote environment: Fedora44 GNU amd64, Linux6.19.10-300.fc44;
rustc1.99.1 `58cf8f9c35b355199727627d07ae34a4e2846d67`, Cargo1.99.1,
LLVM23.1.1. Exact private root
`/home/adalundhe/native-terminal-order-light-b02e-v1` retained361708KiB after
compile-only checks, before the small artifact archive. Cargo package identity
comparison adds only `terminal-order-light`; all real dependencies retain their
source Cargo.lock identities. No provider/build-security option was weakened.
Existing unused-rquickjs patch and AWS-LC configuration notices remain in logs.

Transferred original archive `raw/light-artifacts.tar.gz`:57918 bytes, SHA256
`48202afc8ad739a62ad4321b472bfa7c3b2fa18e9d0580dd8168ccc69f4cbabc`.
Verified44 unique safe regular members and every43 manifest-listed member hash;
extracted originals retained under `raw/remote/`. Failed and corrected generated
scaffolds are separate. Final source identities:

| Path | SHA256 |
| --- | --- |
| New `terminal_order_tests.rs` | `6379f2335f3220f9cb188c364d147871d9def135aa234f3ff844e45ee69f0f7f` |
| Frozen daemon `managed.rs` | `2b3e4d2c80be615a58b062b6a077c11511e2679c738c798a27f0e014a880f655` |
| Frozen remote `managed.rs` | `79c949b781e71821079a0858c67270355ed24a231789aafad80c9695f6226fdd` |

## R01–R16 and owed ROOT work

| Rule | Scoped evidence / remaining obligation |
| --- | --- |
| R01 | Production unchanged. Test-only assertions/unwraps are allowed; cleanup requests stop without metadata lock, waits are deadline bounded, failed joins remain unknown. No universal panic-free claim. |
| R02 | Actual source, earlier signed evidence, primary contracts and alternatives above. No new production architecture or inferred defect fix. ROOT reviews the fixture and live witnesses. |
| R03 | No production literal. Protocol capacities use actual constants; identity/cursors/one slot are structural empty fixture inputs. All wait/file-prefix bounds are explicitly caller supplied. |
| R04 | Actual typed Life/State/Terminal transitions; Ended and known canonical Aborted remain sticky. Successful, absent and partial File bytes are separately checked. |
| R05 | Existing Life/Shared/credit ownership only; bounded event receipts, no new poll/cache/manager. Metadata lock intentionally held only by fixture. Actual I/O stalls remain owed. |
| R06 | One fixture constructor/cleanup owner; focused cases reuse actual methods. No alternate state transition implementation. Full production review remains ROOT-owned. |
| R07 | Comments identify empty/unsubscribed semantics, inert scaffold, split seam and uncertainty. Only new unsafe call reads this child's own limit into valid storage. |
| R08 | No comparative or 4–5× result. Equivalent baseline/distribution measurement remains ROOT-owned. |
| R09 | New obligation explicitly unverified, no new production fix. Source ordering and compile/pure checks are not a live issue reproduction. |
| R10 | Executable leaf/caller/compile recipe, hashes, actual light records and launcher failure retained. All native red/green/mutation/restoration remain ROOT-owned. |
| R11 | GNU amd64 compile-only scope; native Linux, macOS arm64, Windows and seven-target integration remain owed. Windows FSIZE unavailable. Intel Mac unsupported; ARM Windows paused. |
| R12 | Real shared Rust types; no new production OS adapter or installed binary. Unix-only child resource fixture is isolated evidence tooling. No security/crypto weakening. |
| R13 | Minimal private child seam and real File/endpoint; no production testing interface or duplicate owner. |
| R14 | No profiling/benchmark/allocation/latency/leak claim. ROOT owns all such data. |
| R15 | Implementer source/pure/targeted compile only; ROOT registers parent, runs all native/fault campaigns, reviews and integrates signed named paths. |
| R16 | Existing local worktree only, no local cargo build, no cache deletion, NEW remote private paths, 30 GiB Mac floor preserved. Source/evidence/paused VM untouched. |

ROOT still owes complete actual daemon compilation with registration, all five
source-matching native cases and exact retained logs/File bytes/real joins,
meaningful unconditional-retirement and sticky-outcome guard removals plus exact
restoration, actual OS write-delay/nonzero canonical-data/ACK cases, concurrent
channel rows and metadata readers, full guest completion/owner-stop/failure
ordering, actual resource return, supported-target/native hardware gates and
all relevant performance/adversarial campaigns. No G01–G24/P0–P13 closure follows
from this fixture checkpoint.
