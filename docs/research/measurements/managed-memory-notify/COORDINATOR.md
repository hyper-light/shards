# Coordinator native regular-file notification gate

POLICY_READ coordinator main4bfaceb38, R01–R16 SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
ROOT read the full fixture, pure driver, decision/evidence records before these
native runs. Implementer checkpointedb25dd4f8ff5994a05f4da2015d9021f2747e26;
root-selected fixture bytes SHA256
`bafc3ff31ae9f5a4c484124911c890494860bc88c50fd2d751aed57c2f44aab7`.
Only the scoped fixture commit is adopted; unrelated research ancestors are not
merged by this gate. Production source and capacities remain unchanged.

The [exact executed driver](coordinator/driver.executed.py), native source/lockfile,
compiler/binary/host identities, exact arguments, raw output and results are
retained in [native-bafc](coordinator/native-bafc/). The driver was retained locally
before execution, submitted as that tool payload, and copied here afterward.
Its GNU amd64 native binary compiled0 with libc0.2.189 and the expected lockfile
checksum. This builds the non-test native module; the agent's earlier pure tests
explicitly excluded it. No production full-target or VM pass is inferred.

ROOT ran four finite cases in one new private Fedora directory, one CPU affinity,
1GiB address-space and2GiB per-file limits within Kiln's admitted1CPU/2GiB/2GiB.
The per-file bound is not a hard aggregate disk quota. The original30s monotonic
case budget and45s outer child timeout are explicit fixture inputs, not runtime
defaults or performance allowances. All created artifacts remain private and
retained; no file or watch belonging to another actor was changed.

| Case | Actual result and independent witness |
| --- | --- |
| Header16B, one write | exit0; read15→EINVAL, retained modify read16→16, pending ignored read16→16, final EAGAIN; independent exact one-byte file readback. |
| Header+1=17B, three writes | exit0; all three actual writes and exact readback, one coalesced modify frame; read17→16 was short while POLLIN still held, next exact16B read returned IN_IGNORED, final17B read→EAGAIN. |
| Existing directory | expected exit1/EEXIST, no owned paths acquired; pre-existing sentinel hash unchanged. |
| Expired absolute deadline | expected exit1/Deadline before object creation; no new path. |

Both positive cases record actual inotify_rm_watch0, raw complete native frames,
explicit notify/writer close0, and retained created paths. Each child was held
unreaped while the parent opened its pidfd, then exit readiness was observed and
wait returned. Parent FD9→9 after each case. This is not a claim of complete child
kernel charge return, native OOM resistance, pressure notification behavior,
process-tree isolation or a production memory-policy fix. Snapshot host statvfs
is retained; mount-type identity and a pre-run immutable compiler pin were not
established. No throughput/latency/cost distribution or comparative ratio was
measured.

The regular-file result concretely separates successful writes, coalesced events,
positive short reads, readiness and an instantaneous EAGAIN drain. It does not
justify replacing Memory::relieve4096 with16,17, a page or any measured sample.
The source's caller-owned framing envelope and current platform seams remain the
experiment; no production polling/cache/manager/permission/crypto change occurs.

Remaining root gates include GNU/musl and arm64 native compilation/execution,
actual FD/watch/allocation/readiness/report failures, interruption/deadline and
process-loss cases, meaningful native guard removals/restored passes, genuine
cgroup pressure/OOM semantics, production-equivalent cost/fairness and the full
managed plan. The README's original implementer UNRUN labels describe its own
checkpoint; this record adds only these coordinator results. macOS arm64-only
support and the paused Windows ARM VM remain unchanged.

R01/R04/R06/R07/R13: no production delta, typed fixture failures and explicit
watch/path ownership reviewed; no universal standard-library OOM/panic claim.
R02/R03/R05/R09/R10/R12: pinned primary-source alternatives, source-specific live
framing facts, retained raw records and remaining mutations above; no selected
production number or architecture. R08/R14: all profiles/baselines/distributions
remain unrun. R11: actual GNU amd64 only, other native/hardware unverified.
R15/R16: agent only pure/light; ROOT native execution/review/main landing;
protected caches, source, evidence, data and paused VM preserved.
