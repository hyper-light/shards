# Coordinator receive-cancellation and combined gate

POLICY_READ coordinator: main24f48ee5ebf198485117aa93bf47685ad7d7d53f;
rules SHA256 d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
Root reviewed the full shared assertion and receive-cancellation source, owned
the normal artifact build, ten actual KVM cases, seven-target gate and this record.
Implementers supplied scoped fixture code and pure/light checks only.

## Exact revisions and actual results

- Shared strict host checks: e1637959af1dbae631aa6f91c0d4d227f4c7dfac.
- Receive-cancel fixture: 7d01dcaf1ed7b46fe12eea6fcd33b2e50658f520.
- Composed host module wiring and real-VM source:
  68362c3eb236f2d06e9a78984521eeb67f71ede4.
- Closing format-only change: 7017845723af98fbce2da21e96989de5dee60da5.
  It only wraps the command parser match pattern; the KVM results below name6836,
  not a claimed execution of7017.

Root r582 on Fedora amd64 runner8fadc1ec213332e16cb0d915a3fb913f compiled the
ordinary managed_network test and its carried helpers (exit0), then ran ten real
guests. [driver.py](receive6836/driver.py) is the exact executed driver; each case
retains command, environment, CLI/guest/kernel/config identity, raw stdout/stderr,
actual status and decoded observations. [results.json](receive6836/results.json)
keeps the strict results:

| Cases, connected and unconnected | Actual test status |
| --- | --- |
| Scripted responder, destination-only native address presentation |101, expected exact peer mismatch |
| Scripted responder, fixed original peer presentation |0 in both modes |
| Two-leg buffered relay, destination-only |101, expected exact peer mismatch |
| Two-leg buffered relay, fixed original peer |0 in both modes |
| Cancel after independent backend receipt and upstream-receive entry |0 in both modes |

Thus six strict positive cases pass and four deliberate negative controls fail
for the required reason. The driver's0 does not turn101 into a passing test.

Both cancellation cases retain one validated ingress, one real upstream send and
a third independent backend Binary receipt; the backend holds that reply.
The relay records Cancelled at UpstreamReceive, KernelAccepted, one ingress/send
and zero upstream replies/client sends. The owned waiting client is actually
SIGKILLed and reaped, not reported as a successful child. Both workers join before
the original work deadline with positive remaining time; no new cancellation
grace is introduced. Stage entry is not an observation of kernel sleep.

All ten cases record joined backend/relay, detached acquired links and removed
owned cgroup. Those facts do not prove all host kernel charges returned or effects
after the observation window. Native source presentation remains a one-flow
experiment: the backend sees the separate relay upstream socket, not the actor
source. Mandatory general route coverage is not established.

Root r583 on macOS arm64 runnerb24c5c951260dcdfd0972f2aeaf3cf51 stopped at format
on6836 (exit1, no target compilation). Its original [log](matrix6836/lint.log)
is preserved. After the layout-only7017 change, root r585 ran full scripts/lint
under lockf /tmp/g121-mac.lock with CARGO_BUILD_JOBS=4 (exit0):
format, arm64 macOS, both Linux architectures with GNU and musl, both Windows
architectures, and the script's GPU checks. [Exact command](matrix7017/command.txt)
and [full log](matrix7017/lint.log) are retained. AWS-LC's optional arm SHA3 compiler
feature probe warnings/fallback remain visible; no crypto policy was weakened.
Compilation is not native hardware validation.

## Replay and provenance

Use the exact commands/environments in each case and the existing normal helper
build. The explicit fixture inputs are boot120000ms, work30000ms, memory256MiB,
one vCPU and18 payload bytes (two actual9-byte slots); they are caller-supplied
test settings, not production defaults. Ports are assigned by the OS. The supplied
experimental kernel/config hashes are recorded per case, not a new product pin.
Config association is a supplied build record, distinct from runtime observations.

Text-only archives are indexed in [ARCHIVES.json](ARCHIVES.json). Every regular
member and its hash/size was checked after private transfer. Original archives,
raw records and separately frozen actual guest binaries remain on the runners;
local archives are retained in /Users/adalundhe/g121-retained-archives.
No binary artifact or credential was added to this evidence tree. Raw blank lines
and patch-context whitespace are preserved byte-identically and classified below.

## Policy review and remaining gates

R01/R04/R06/R07/R13: no production behavior changed by the shared test refactor;
the cancellation fixture uses explicit typed stages/outcomes, fallible native
events/owned handles and checked counters. Existing production panic lints remain.
Strict effect, peer, payload, terminal, detach and ownership assertions remain.

R02/R03/R05/R09/R10/R12: reuse the existing guest Deadline, worker, socket and
owned child seams. Two one-shot notifications name actual receipt and receive
entry; cancellation uses its existing eventfd. No timing poll, cache, new manager,
picked production limit, grant widening, host privilege or duplicate OS adapter.
The source's research and implementation record is in
[managed-relay-cancel](../managed-relay-cancel/README.md).
Earlier forwarding mutations and restored controls remain in
[managed-coordinator-forward](../managed-coordinator-forward/README.md).

R08/R14: no resource, latency, throughput, allocation-free, leak-free or4–5× claim.
Payload slots exclude allocator, parent, kernel and console costs.
R11: actual KVM amd64 only here; seven-target compilation above. Other real
hypervisor/architecture behavior remains unverified; Windows ARM remains paused.
R15/R16: root owns these campaigns/review/main landing. Agent checkpoint commits
and exact root sources are identified above. Existing source, artifacts and evidence
were preserved; this Mac's30GiB floor was restored by removing only inactive old
Cargo intermediate objects with cache tag, age/type, lock and open-file checks.

Still owed: targeted cancellation guard removal/restoration, notification/worker/
backend failure and expiry cases, pre-send commitment boundary, normal member
terminal metadata, full hard aggregate accounting, route coverage, broker/native
adapters/credentials/generations and actual supported hardware matrix.
These are open work, not completion of the managed-runtime plan.
