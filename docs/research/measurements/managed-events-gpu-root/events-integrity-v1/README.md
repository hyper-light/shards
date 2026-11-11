# Live event-data corruption and restoration

ROOT r61 on Adacachy451397 completed0 at exact a8ad1ce8718bb08d8acaffce7c85e25ce49bce49.
Three independent fresh normal carried builds and the same exact real-KVM case
produced the following actual values:

| Build | Kernel task.max before/after | Export before/after | Exact case |
| --- | --- | --- | --- |
| Baseline |0 /1 |0 /1 |1pass, exit0 |
| Counter-corruption mutant |0 /1 |0 /0 |expected assertion failure, exit101 |
| Byte-restored source |0 /1 |0 /1 |1pass, exit0 |

Only the mutant parser's selected TaskMax value was replaced with0. Kernel
controls remained intact: all three attempts recorded child-creation EAGAIN11,
then actual stdout/stderr child markers after restoring the local task ceiling.
The mutant produced actual false telemetry rather than merely losing an alert.
Its failure is the exact comparison of after_events task.max against before_events.
The ordinary baseline and restored fixture also retained its original neighbor
stdout/stderr and main-progress assertions. No failure assertion was weakened.

Every direct build/list/test child was pidfd-ready and reaped; controller FD
sets returned. Whole descendant population and kernel-resource return remain
Unknown. Build/test durations are orchestration records, not performance data.
The actual before/mutant/restored source, six frozen test/CLI binaries and their
identities remain preserved on the runner. Source restoration was byte-exact
with empty tracked diff. Both guest/kernel identities were fixed before actors.

Original scope root: /home/adalundhe/root-managed-events-integrity-a8ad-v1.
Same normal4CPU/16GiB/64GiB/j4/7200s absolute budget; existing build2400,
guest900, list60, case1200 and reap15 guards. Private Cargo home, empty Docker
config, separate target per stage and short private TMP were used. The original
r40 source/target/evidence stayed untouched; its exact public pinned kernel was
read-only input. No other held campaign, guest mechanism or helper source changed.

The79,763-byte raw archive SHA256 is
e4f2776826edf44b8b9bc1f244c682e487af9ffce2e4c3929cbeeb283d69bb71.
ROOT recovered and independently checked all111 original payloads plus their
manifest112, exact kernel/export bytes, actual panic reason, restored source
and frozen-artifact associations. Run verify.py for the offline raw check;
it starts no process or VM. run.py is the exact executed driver, retained as a
single fresh-scope reproduction record; never replay it into retained paths.

R01/R04/R05/R06/R07/R12/R13: no new production change or alternate encoder;
mutation is isolated, explicit and restored; existing event-driven process waits.
R02/R03/R09/R10: actual same-kernel before/corrupt/restore evidence and input-bound
fixture method, with original failures retained. R08/R14: no benchmark or cost
claim. R11: Linuxamd64 KVM at a8ad only; candidate event source is byte-identical,
but its combined GPU/target matrix remains pending. R15/R16: ROOT campaign,
normal guarded fresh scopes, no children/held effects/data deletion. Fullrules
SHA256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Production delivery, continuity/restore, hard resources, actual costs and the
management plan remain open. This does not establish native tool authority.
