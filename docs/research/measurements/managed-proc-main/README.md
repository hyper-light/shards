# Linux process start reader, independently based on main

The exact-main reader rejected a legal non-UTF8 process name. The focused native
regression now passes, including initial and later returning allocation failures.
This is a source/light-test checkpoint for coordinator review, not adoption or a
whole-library panic-free, resource-return or performance claim.

## Identity and scope

POLICY_READ implementer: current main
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, full R01–R16 rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Canonical AGENTS, rules, evidence, CLAUDE, managed-runtime plan and current handoff
were read before work; policy and handoff were reread after compaction. Root owns
all audits, profiling, benchmarks, heavy E2E/VM/chaos/sweeps, mutations for this
batch, final review and main merge. No child agents or local Mac builds were used.

Branch `p/managed-proc-main` starts directly at that main revision. Its source
checkpoint is `fd2edd898df1d34bc736553db8e34c000ccce456`, pushed before this
separate evidence record. The three source commits are:

| Commit | Scope |
| --- | --- |
| `ed92fccafe261625fe50bb05f586c6a517669941` | Repro fixture/script and Linux cfg(test) include; production reader identical to main. |
| `35658b3ec42a6a83c0c32c3a6ffe85b0deb31d5d` | Private Linux stack path, byte reader/parser and focused unit/OOM coverage. |
| `fd2edd898df1d34bc736553db8e34c000ccce456` | Test-only redundant import correction and allocator forwarding SAFETY comments after actual clippy red. |

Only Linux private `taken.rs` implementation, Linux cfg(test) wiring in `lib.rs`
and this measurement directory are changed. There is no ancestry from the
unfinished borrowed connector or synchronization candidates: no public API,
Refusal, dependency, stream, dial, process lifetime, held-handle, Mutex or non-Linux
production change. `crate::channel` is not used. Existing `io::Result<u64>` remains.

Native environment: Fedora44, Linux `6.19.10-300.fc44.x86_64`, GNU amd64;
Rust1.98.1 commit `48a229ceaefd4985c50990b14116b6d856af0985`, LLVM22.1.8;
Cargo1.98.1 commit `797e8a9bca276c1c9f9f738d2a20f484fa4eea9d`.
Full identity and source hashes are retained per run. All three source-specific
admissions requested exactly 4CPU/1GiB/2GiB once and were accepted. The runner was
`8fadc1ec213332e16cb0d915a3fb913f`; kiln's free-GB field describes memory, not disk.

Private remote checkout:
`/home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-proc-main-c4ac3e415a`.
Private target: `~/managed-proc-main-target`. Evidence and immutable copied
executables: `~/managed-proc-main-evidence/{baseline,fixed,fixed-checks}/test.bin`.
Root-owned and earlier frozen clones/targets were untouched. No cleanup was needed.

## Live exact-main red and fixed result

Each invocation runs the committed script with a unique evidence phase:

```sh
bash docs/research/measurements/managed-proc-main/run-light.sh baseline
bash docs/research/measurements/managed-proc-main/run-light.sh fixed
bash docs/research/measurements/managed-proc-main/run-light.sh fixed-checks
```

The script explicitly touches owned selected source/harness inputs before the
build, retains Cargo JSON and requires the selected executable's `fresh:false`.
It copies and hashes that executable before testing. Commands, exit files,
toolchain, source status and hashes accompany each run. No timing result is used.

| Job/source | Build | Focused tests | Strict API all-target clippy | Wrapper |
| --- | --- | --- | --- | --- |
| r216 / `ed92fccafe261625fe50bb05f586c6a517669941` | 0 | 101: one live red, one private child role ignored | 0 | 1 |
| r218 / `35658b3ec42a6a83c0c32c3a6ffe85b0deb31d5d` | 0 | 0: five passed, one private child role ignored | 101: test import and four missing SAFETY comments | 1 |
| r220 / `fd2edd898df1d34bc736553db8e34c000ccce456` | 0 | 0: five passed, one private child role ignored | 0 | 0 |

The ordinary unignored live fixture starts only its own child, opens that child's
existing pidfd-backed `Held`, samples starttime while alive, then sends a ready
command. The child writes `6f 77 6e 20 ff 29 20 29 20 fe` to its own
`/proc/self/comm`, targeting its TGID leader even though libtest invokes the role
from a worker thread. A worker-only PR_SET_NAME would miss this process stat.
The name fits TASK_COMM_LEN and includes non-UTF8, spaces and right parentheses.
The parent observes the actual raw bytes and invalid UTF8 in `/proc/PID/stat`
while that same held child is still alive, then calls the actual private reader.

Baseline [tests.log](records/baseline/tests.log) records
`held_alive=true observed_start=Err(InvalidData("stream did not contain valid UTF-8"))`.
Before the final assertion fails, it explicitly restores the original name,
observes the same starttime and held-alive status, releases the child, waits for
exit0 and joins its event reader. Those cleanup facts are printed before failure.
The symptom was reported to root before changing the production reader.

The final [tests.log](records/fixed-checks/tests.log) records `observed_start=Ok(10130423)`
and the same successful explicit restoration/start/exit/join sequence. The pidfd,
numeric identity checks and ordinary process authority are unchanged. Events use
the existing conformance PATIENCE budget with one absolute outer liveness guard;
there is no sleep, repeated readiness probe or new timeout. RAII kills/waits only
the owned Child and joins the reader on an error. The sole ignored test is the
private child role, which the ordinary regression actually invokes.

The other four ordinary tests exercise actual helpers:

- Stack path for PID0,1,9,10,99,100,u32::MAX under allocation refusal; actual
  started(0/MAX) preserve NotFound from procfs rather than wrapping/rejecting a
  different pathname.
- Last-right-parenthesis field selection with raw comm and numeric zero, ordinary
  value and u64::MAX; empty/missing fields, negative, invalid UTF8 decimal and
  overflowing u64 return InvalidData under allocation refusal.
- Exact bytes from a private file, empty EOF and actual directory-read error.
- Actual `read_stat(File)` and actual `started(current_pid)` return OutOfMemory
  with zero or one successful allocation before refusal. Assertions/logging run
  after the thread-local refusal guard resets. These are ordinary unignored tests.

The r218 strict-clippy red was confined to a redundant Read import inherited from
the changed parent module and missing SAFETY comments around test allocator
System forwarding. Both production files are byte-identical between r218/r220.
The red log is preserved; only the final source has the passing clippy result.

## Primary sources, alternatives and narrow implementation

[Linuxv6.18 array.c](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/proc/array.c)
emits comm with escaping disabled inside parentheses, then decimal numeric fields.
[proc_pid_stat(5)](https://man7.org/linux/man-pages/man5/proc_pid_stat.5.html)
identifies starttime as field22. The kernel emits raw comm bytes; whole-record
UTF8 is not its contract.
[proc_pid_comm(5)](https://man7.org/linux/man-pages/man5/proc_pid_comm.5.html)
documents per-thread names and changes within one's own thread group. The live
example corroborates the precise TGID/nonUTF8 behavior rather than inferring it
from these sources alone.

The parser retains main's last-')' and twentieth following field selection.
It splits ASCII whitespace as bytes and validates UTF8 only for the decimal
start field, then uses the existing u64 parser. File/open errors propagate;
missing/malformed numeric data now returns nonallocating InvalidData instead of
constructing main's formatted Other error. There is no lossy conversion or new
parser framework. Other malformed fields are not newly validated.

Alternatives were reviewed against the concrete constraints:

| Alternative | Evidence and disposition |
| --- | --- |
| Keep whole-record `read_to_string` | Exact-main live rejection above. Legal comm need not be UTF8. |
| `std::fs::read` / `read_to_end` | Pinned [fs.rs](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/fs.rs) delegates to [default_read_to_end](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/io/mod.rs). A zero-size hint reaches a small probe with infallible Vec extension. Prior exact direct-std control actually aborted on32B refusal; see the immutable historical record below. This was not rerun as a campaign here. |
| Preallocate std's32B probe / fixed cap / page guess | Would couple correctness to an implementation detail or impose an unjustified input bound. No such value is adopted. |
| Opened File metadata preferred-I/O hint | Separate experimental prototype and root r282 comparison did not establish benefit; not adopted here. No mandatory metadata, fallback or new supported-input restriction is introduced. |
| New dependency/general reader or upstream toolchain redesign | Broader than this single private process-reader seam; not needed for the demonstrated input/error contract. |
| Existing f26 fallible loop at this seam | Root approved the narrow backport after the exact-main reproduction. Its byte growth, EOF, EINTR and returning errors are retained, independently of its unadopted API ancestors. Exact-main source costs and full matrix remain owed. |

Historical direct-std evidence, including the actual32B SIGABRT and later
fallible-growth guard-removal result, is retained at
[05f PROC-COMM.md](https://github.com/hyper-light/shards/blob/05f02669048b1d1a9c9c1ca39e021a211d2ad103/docs/research/measurements/managed-borrowed/PROC-COMM.md).
It is provenance for a reused method, not a passing test or mutation result for
this new main-based source. None of that branch's production/public API is imported.

The loop owns one Vec. `try_reserve(1)` requests the minimum additional byte
needed to observe further data or EOF, not a chosen chunk size or record cap.
It initializes only the Vec's already-owned capacity and reads into a checked
slice beyond the used prefix. Pinned
[Vec resize/extend_with](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/alloc/src/vec/mod.rs)
and [RawVec reserve](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/alloc/src/raw_vec/mod.rs)
show that initialization within capacity does not request growth; u8 cloning
does not execute user code. Read count addition is checked and constrained to
initialized length. Interrupted retries the same owned buffer; positive partial
reads retain their prefix; zero truncates to actual bytes and returns. File and
Vec drop on every returned error. No native allocation-failure claim is made.

The stack pathname capacity derives from `/proc/` bytes + decimal u32::MAX width
+ `/stat` bytes. `ilog10` is evaluated on the known nonzero u32 maximum; its small
result and literal lengths fit every supported64-bit usize. Even PID0 requires
one decimal digit. Radix10 and ASCII '0' derive from procfs decimal paths, field19
is field22 minus the first three positions, and all new dynamic offsets use
checked arithmetic/get access. The formatter has no generic abstraction or heap
allocation. The test verifies both ends of decimal-width boundaries under OOM.

There is no production cache/state owner change. The read loop consumes data
from one opened File, not repeated readiness polling. Existing pidfd ownership
remains the authority around PID/start sampling; the helper alone does not
eliminate caller lifecycle races. No grant, crypto, installed binary or root
privilege changes are involved.

## Artifacts

| Phase | Selected executable SHA256 | Raw archive SHA256 |
| --- | --- | --- |
| baseline | `380d499394e475781050ff66021f565e3e9bed8044da52306d13219bb86c3655` | `fb6218dcd92b7d9f123efa8e124c8a774cf072ff27da81e600692de6a4f7225c` |
| fixed-first | `e6958abb3bfbdc7e6b11d3af88608e7aa4d2ac23763a66ae026fdf37971ac1cd` | `366bc6af73c649f0f4a2bd3c08999784e84e72c53f3598a30bb21c2446e2b654` |
| fixed-checks | `b8878a76c38054556ce337ac2935b409a8a1045f50e452b8efac1f5841a29d97` | `7c082a72f5028c8a5cf91387049e10ae63ba1e29d682d88a7797cc8c945578d0` |

The [records](records/) directory includes original compressed raw archives,
checked extractions, admissions, commands, source/toolchain identities, Cargo
compiler JSON, test/clippy logs and exit files. Executables are not committed.
All hashes refer to actual selected artifacts, not a path assumed fresh from a
previous build. The local manifest covers these retained source/evidence files.

Formatter, shell syntax, policy and source-only diff checks passed. The whole
evidence commit's `git show --check` returns2 for the original command logger's
trailing spaces and libtest's extra final blank lines in raw records only; the
[exact check output](records/raw-whitespace-check.json) is retained. Those raw
bytes match the original archives and were preserved. The earlier local diff0
record predates staging the raw extraction; it is not a whole-evidence diff0 claim.

## R01–R16 account and coordinator work owed

| Rule | Proof/result and remaining gap |
| --- | --- |
| R01 | Checked Linux pathname/length handling, fallible Vec growth and nonallocating InvalidData/OutOfMemory; ordinary actual-reader OOM tests pass. Existing outer `Held::find` String formatting and unrelated API allocations remain. Full Mac owned std Mutex/OnceBox OOM RED is untouched. No whole-path/library panic-free claim. |
| R02 | Pinned kernel/Rust and primary man-page semantics, exact live red, alternatives above; coordinator reviews reuse and main-specific costs before adoption. |
| R03 | Path capacity/literals derive from fixed paths, decimal u32 width, field22 and minimum one-byte read capacity. No metadata/default/page/measurement constant adopted. |
| R04 | Existing held process/lifecycle state unchanged. Private result remains io::Result<u64>; malformed/OOM kinds explicit. Read transitions are reserve→initialized owned space→read/Interrupted retry→checked retain or EOF/error. Outer String presentation remains a known limit. |
| R05 | One existing private process-reader seam, no new generic manager/framework/cache. One File per call; fixture events and one existing outer guard, no poll/sleep. Caller-wide wait/cache audit remains root-owned. |
| R06 | Focused path/read/field functions with bounded arithmetic and local ownership; strict clippy0. Broader complexity audit belongs to root. |
| R07 | Comments cover TGID trap, capacity derivation, raw comm, fallible growth and test allocator unsafe forwarding. No production unsafe introduced. |
| R08 | No speed, allocation-free, leak-free or4–5× claim. Root comparison on this exact independent source is owed. Historical f26/metadata data do not validate this whole main source. |
| R09 | Exact-main live legal-name failure captured at ed92 before production edits; error/held identity/cleanup explicit. |
| R10 | Baseline red, first fixed clippy red, final green, raw commands/source/binary hashes retained. Exact-source guard-removal/restoration mutations are owed to root, not claimed from earlier branch evidence. |
| R11 | GNU amd64 native focused result only. GNU/musl arm64/amd64, seven-target matrix and actual supported VM hosts remain owed; Intel Mac unsupported; paused Windows ARM VM untouched. |
| R12 | Existing Linux-only procfs adapter and shared Held seam retained, no duplicated API/crypto, extra installed binary, dependency or root requirement. No non-Linux production change. |
| R13 | Byte parser/one Vec at the existing seam; fixed stack formatter replaces allocating main path. No reader architecture/cache added. |
| R14 | No profiling/benchmarks by implementer. Root owns exact-source CPU/syscall/alloc/realloc/copy/retention/fault/parent-kernel cost comparison. |
| R15 | Scoped implementation/light checks only, no children/mutations/campaigns/self-main-merge. Signed named-path source commits use required trailer; root owns review, combined gates and merge. |
| R16 | Private source/target/evidence only; no cache/source deletion or headroom bypass. One4CPU/1GiB/2GiB request per concrete source, all accepted. No local Mac build near floor. |

Owed root runs: full ordinary API unit/integration corpus on reviewed source;
actual old-whole-UTF8-read and fallible-growth removal/restoration controls;
EINTR/partial-read error-path evidence where an actual File seam can exercise it
without a new production framework; exact-source cost and parent/kernel/FD return;
Linux libc/architecture variants, full supported-target lint/build and relevant
held-process/VM compatibility. Root composes any architecture/decision entry and
final combined gates. Historical owned-Mac synchronization RED remains open.

For a new private admitted Linux checkout/target, the focused root command is the
script above with a fresh phase name; it uses cargo `--profile test-release -j4`
and strict clippy. The existing normal tests can also be selected from the exact
frozen `test.bin` with `taken::linux::tests:: --test-threads=1 --nocapture`.
Those are runnable instructions, not results on untested targets.
