# Held-object native job execution

Implementation checkpoint at main `7d7e0855a5073ed5163b4fe7ed3355c9ed7c2506`;
rules SHA256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Relay is the assigned implementer and sole writer of the new `process.rs`, its
tests and this measurement directory. ROOT coordinates integration and main.
The completed relay batches, skipped response-limit mutation, all review holds
and paused Windows ARM VM remain unchanged.

`process.rs` prepares the actual checked executable/workdir, literal argv/env and
the single shared child FD map. Its child-only syscall path installs that map,
fchdirs the held directory and execveats the held image. It returns typed failures
with saved native cause and an explicit pre/post-placement reporting location.
There is no process creation, authorization or lifecycle manager in this leaf.

See [the exact consumer contract](CONSUMER-v4.md), [actual results](RESULTS.md) and
[frozen source identities](freeze-v4/source-manifest.json). Native's real helper
consumer/module composition remains a separate owned dependency until its exact
source is received. A source projection is not that consumer's validation.

## Actual checks and frozen inputs

The local development-policy command returned0; its exact output is retained in
`local-checks/`. No local Rust build or native actor was started. ROOT's local
floor hold was respected and later lifted; no local target was created.

The bounded GNU batch finished with one pure case and one ordinary native child
case passing on Adacachy `451397fcc585f7f7cb9aaf00e42064bb`. The final private root
is `/home/adalundhe/native-job-executor-7d7-v4`; original v1–v3 preparation failures
remain retained. Each stage used normal2CPU/2GiB/4GiB admission and Cargo j2, with
the SAME original600s absolute budget copied byte-exact. Native completion had
188.5s remaining; no runtime case was retried. Source/target/cargo-home/records/
artifacts are private; no ROOT or peer scope is reused.

`native-packet-v4.json` pins the complete final package, driver and source
manifest; the package includes each input's identity. The projection imports the
canonical source bytes from signed46ab, current7d7 testdir/tool/lint facts and the
one frozen native counts getter. It has no fabricated production implementation
or dead-code suppression. The original projection-v1 and source drafts remain
retained and unrun. `native_batch.py` uses one original600s absolute scope budget,
pidfd event waits and separately retained command stdout/stderr/receipt files.

The actual exact checks passed GNU `cargo check --tests`, strict Clippy,
test-build, both one-test listings, one pure denial and one ordinary native child
case. Each test reported1passed/0failed/0ignored/16filtered; filtered dependencies
are not counted as passes. The driver
freezes independent test and executed-child binaries before actors. The ordinary
case checks exact literal argv/env/input/output, held identities after pathname
replacement, the actual working directory, separate stderr, launch-status EOF,
held pidfd exit readiness, direct wait/ECHILD and FD return. It records actual
facts before final terminal assertions. All74 raw payload lengths/hashes were
verified against the recovered original manifest; see `recovery-verification-v1.json`.

## R01–R16 account and remaining limits

| Rule | Applicable implementation/evidence and remaining gate |
| --- | --- |
| R01 | No production assert/unwrap/indexing/allocation in the child path; fallible preparation delegates to the canonical mapper/literals. Scoped GNU strict check0; full consumer remains pending, no proof from lints alone. |
| R02 | ROOT's existing native-authority decision and native's frozen PROCESS-WRITE-OPERATIONS / OPERATION-WIRE contracts select the actual seams. This leaf does not choose a new architecture. Primary behavior and alternatives remain linked there; no new compatibility claim. |
| R03 | Slots0..5 are the existing native ABI map; CLOEXEC/open/access/AT_EMPTY_PATH values are OS protocol flags. Zero is the syscall success convention. No capacity, time or production queue number is selected. Fixture30s and scope600s are explicit test limits, not runtime policy. |
| R04 | Typed preparation rejection returns the SAME Request; child failure retains map/native cause and Staged/Installed/Unknown report location. Existing owner retains NotStarted/Started/Uncertain, Group, pidfd and launch status; no second lifecycle enum here. |
| R05 | Real caller is native's existing gated Worker after owner association, audit intent and current commit. One mapper, no manager/cache/poller/timer. Exact registered consumer and caller accounting remain native/parent dependencies. |
| R06 | Preparation has one explicit executable-lifetime match; child path is map→fchdir→exec; failures carry stage. Focused GNU checks and two exact cases pass. |
| R07 | Comments describe raw-child safety, staged descriptor lifetime, script CLOEXEC behavior and authority/resource limits. |
| R08 | No speed or4–5× result. Comparable performance remains ROOT-owned and unrun. |
| R09 | This is a newly assigned mechanism, not a guessed defect repair. Actual strict-Clippy failures informed the small preparation enum/test correction; first ordinary native proof passed. |
| R10 | Source/patch/manifest, actual commands and raw outcomes are retained. Old failures/drafts stay preserved. No mutation is authorized in this batch. |
| R11 | The implementation is inside the existing Linux guest server seam. GNU light checks and two exact cases pass; static musl, both guest architectures and seven host targets remain unrun ROOT gates. Mac support remains ARM64 only. |
| R12 | Existing held objects, literals and shared child_fds implementation are reused. No shell, interpreter parser, pathname reopen, installed product binary or OS shim. The tiny executed binary is an owned test fixture only. |
| R13 | Only process mechanics plus explicit metadata/reporting handoff; no authority issuer or duplicate pump/process manager. |
| R14 | Retained literal capacities and staged descriptor count are observations only. Total allocator/kernel/parent overlap and cost profiles remain ROOT work; no allocation-free/leak-free claim. |
| R15 | Assigned ordinary private light scope only, no child agents/VM/held campaigns/main merge. Actual native/helper/owner integration and review remain ROOT/native gates. Own genuine signing or frozen unsigned handoff is required at closeout. |
| R16 | No cache deletion or ROOT/peer target access. No local target exists; actual Mac floor hold is respected. Private remote resources require normal guard admission. Sources, raw and frozen artifacts remain preserved. |

Actual issuer rights, current-owner commit, audit durability, born-in-Group
association, descendant population, cancellation/revocation and external delivery
uncertainty are not established by a mechanics-only fork fixture. Neither pidfd
readability nor EOF can return resource charges. Real helper/owner consumption,
ordinary composed calls, relevant faults/mutations, musl/seven-target/VM coverage,
resource backing and comparative cost remain explicitly owed to their owners.
Full managed-runtime closure and lane release remain OPEN.
