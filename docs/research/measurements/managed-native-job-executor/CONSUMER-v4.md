# Native process mechanism handoff

The sole process writer is relay `01a1c5d2ee0e7248982c95a59f1fd905` in
`managed-native-job-executor-v1`. ROOT's subsequent ownership correction
supersedes the earlier transfer. The session status-placement fixture owns
disjoint documentation and made no process source edits.

The implemented leaf is frozen in `freeze-v4/source-manifest.json`. It consumes
the exact signed46ab `Executable`, `Workdir`, `Literals` and canonical
`child_fds::PreparedFdMap`, plus native's narrow `Literals::counts()` getter
whose postimage is `4ee5e5c298c345faf199e54acbcd33b4a2c9fbf3fb5289b6008f67482141e4ee`.
The old unbuilt drafts and freeze-v2/v3 remain immutable. Source-only freezes are
not a runtime pass or a registered consumer.

## Exact interface

```rust
struct Request {
    executable: Executable,
    workdir: Workdir,
    literals: Literals,
    lifetime: ExecutableLifetime,
}
enum ExecutableLifetime { ImageOnly, ReadOnlyScript }
struct Streams<'a> {
    input: BorrowedFd<'a>,
    output: BorrowedFd<'a>,
    error: BorrowedFd<'a>,
    launch_status: BorrowedFd<'a>,
}
enum Preparation { Prepared(Prepared), Rejected(Rejected) }
Prepared::prepare(Request, Streams<'_>) -> Preparation
Prepared::metadata(&self) -> Metadata
Prepared::retained(&self) -> Retained
Prepared::launch_status_before_install(&self) -> Option<BorrowedFd<'_>>
unsafe Prepared::exec_in_child(&self) -> Result<Infallible, Failure>
Failure::status_location(self) -> StatusLocation
```

`Rejected` returns the SAME owned Request and typed cause. Native object checks,
held device/inode observations, literal bytes and a prepared descriptor map are
mechanisms/data. None can issue a process permission, current-owner commitment,
resource lease or executable/view grant.

`Metadata` records executable/workdir `ObjectIdentity { device:u64, inode:u64 }`,
`arguments:usize`, `environment:usize`, and `Retained { literal_bytes:usize,
staged_descriptors:usize }`. Identities come from actual fstat during preparation;
counts come from the original literal vectors. It contains no argv/env values.
The snapshot does not freeze file content. Native/audit keep the existing checked
Programs identity; there is no execution counter here.

Literal retained bytes and staged FD count are observations, not total resource
accounting. Original held objects, map/vector and allocator/kernel overhead,
transient overlap, streams, audit and lifetime objects remain caller-owned costs.
No aggregate physical/kernel admission result follows from these fields.

## Existing birth and audit order

Native's frozen `NATIVE-OPERATION-WIRE-v1.md` identifies the actual consumer:
the existing gated Worker, SAME Programs ID and original captured Scope. The
actual owner prepares a held job Group, receives the birth pidfd and acknowledges
association. Canonical SERVER audit retains its Journal and commits the intent;
the same current owner then commits the operation. Only that existing consumer
may release the gated child to this raw exec leaf. Native owns module/consumer
registration; parent owns issued/current authority; resource owns Group lifetime.

This leaf has no clone, wait, reaper, cgroup, owner socket, audit FD, protocol
dispatcher, timer, cache or second lifecycle state. Outer
NotStarted/Started/Uncertain outcomes retain the actual Group, pidfd and launch
status. EOF alone does not certify exec, application success, reap, population
emptiness or a refund. Unknown report/population outcomes retain ownership.

## Descriptor and failure contract

| Slot | Source | After successful exec |
| --- | --- | --- |
| 0 | caller's input | retained |
| 1 | separate caller stdout | retained |
| 2 | separate caller stderr | retained |
| 3 | caller's launch status | closed by CLOEXEC |
| 4 | checked held executable | closed for ImageOnly; explicit read-only script retention otherwise |
| 5 | checked held workdir | closed by CLOEXEC |

The fixed slots come from native's existing canonical map contract, not a queue
size or resource allowance. Prepare duplicates only these objects above every
destination. Installation places every binding, then closes all other ranges.
Owner6, init/helper controls, roots, credentials and audit descriptors are absent.

Before birth/install the existing caller captures the raw descriptor from
`launch_status_before_install()`. Missing returns a typed preparation/reporting
failure; it cannot be assumed to be installed3. Never borrow staged owners again
after install/closure, and never run inherited Rust destructors in the child.

| Actual child failure | `status_location()` | Reporting descriptor |
| --- | --- | --- |
| map install at destination0..3 | Staged | raw staged status captured before install |
| map install at4/5 | Installed | status3, already installed |
| close-range operation | Installed | status3, already installed and excluded from closure |
| fchdir or execveat | Installed | status3 |
| preparation-only or unrecognized failure | Unknown | no fabricated delivery/success; retain uncertainty |

Map operation and saved errno remain typed in `Failure::Fds`. Working-directory
and exec failures save the immediate positive native errno in `NonZeroI32`;
unexpected nonpositive errno is `InvalidErrno`, not a fake success or retry code.
The caller reports that retained cause through the existing launch protocol and
raw `_exit`s on every returned error, without allocation, unwinding or destructor.

ReadOnlyScript requires the actual held descriptor to be O_RDONLY and not O_PATH.
The caller must separately authorize retained read access. ImageOnly uses
CLOEXEC; Linux shebang/interpreter ENOENT behavior is not hidden by a parser,
pathname reopen or shell fallback. The existing signed46ab script probes are
historical dependency evidence, not new process leaf passes.

## Ordinary light check and pending gates

The NEW private GNU projection includes exact leaf bytes and dependency pins;
it does not replace the real helper/owner graph. Its one pure denial case checks
literal rejection and returns the same O_PATH script object before birth. Its
one ordinary child uses the real prepared map and execveat, an explicit literal
argv/environment, renamed held executable/workdir, binary stdin and separate
stdout/stderr. The executed image verifies only stdio remains after exec.
The fixture holds a release gate until the direct child's pidfd is acquired,
then records actual output, launch EOF, readiness, direct reap, ECHILD and FD
return before strict terminal assertions. It claims mechanics only, no issued
owner grant or cgroup population proof.

All commands, raw results, source/binary hashes and exact list counts must be
retained. The first exact GNU pure and ordinary cases both passed; see RESULTS.md and the verified original raw. Musl, the full actual
helper/owner consumer, group/population/revocation, VM, seven targets, costs and
final integration are Native/ROOT gates. No old relay pass is inherited.

The source05ccf955 and testeb4c09e correct two actual first strict-Clippy failures: inline owned Preparation replaces a large Result error without boxing/suppression; the raw exec test matches its irrefutable Err directly. All prior source and raw remain frozen. Native reports a required inherited helper seccomp stage choice to ROOT; no new job may be released by assuming its current filter permits the exec/runtime. This is a source compatibility dependency, not a reproduced production fault or a reason to widen a filter here.
