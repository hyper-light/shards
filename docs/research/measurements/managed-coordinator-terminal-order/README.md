# Coordinator terminal metadata cases and reconnect failure

POLICY_READ coordinator main f9030566bc227d50770bba879bd1387ebffe410f;
rules SHA256 d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
[Composition](COMPOSITION.md) selects only signed6d0f699 plus root cfg(test)
registration at d2288c27ea29acbb9809fa2ff1f60176dac440b9. No production
algorithm, protocol, admission bound, authority or crypto changes are included.
The [implementer source/contracts/alternatives/light evidence](../managed-terminal-order/README.md)
and actual root records below have distinct roles.

## Actual complete-daemon native gate

ROOT r727 on Fedora44 amd64/Linux6.19.10 compiled the complete source-matching
daemon test binary with normal carried helpers, exit0. This is not the
implementer's extracted-fragment artifact. [Commands, environment, compiler,
artifact and results](native/native/) retain source d2288c27 and test binary
SHA256250a133cac8a7ebc34b16cbda7caced8c13776e33771b85ef72d019f64738b86.
It was independently frozen at
~/root-managed-terminal-order-d228-artifacts-v1/full-daemon-tests.

Each case ran in a fresh process/private directory with original caller
300000ms deadline,900s process bound; the selected test, real entry/completion,
actual endpoint return/join and independently read File bytes were required.
All five callers returned0, each actual selected test passed1/0/0ignored,
source/binary hashes remained unchanged, and Python observed/reaped its owned
native process.

| Case | Actual File and lifecycle/retirement observation |
| --- | --- |
| owner-finished-lock | Driver returned/joined while metadata mutex remained held; File empty until release; exactly one ended/complete row67B afterward. |
| owner-serving-lock | Same retirement-before-unlock; exactly one aborted/unknown row68B; subsequent finish refused. |
| completion-lock | Existing resolve→publish split yielded Ended before actual metadata-lock delay; concurrent real abort retired driver; one67B complete row after unlock. |
| readonly-file | Actual complete() append failed EBADF9, File stayed empty, Life remained Ended and owned driver returned/joined. |
| file-limit | Only the fresh child set its own FSIZE1 and ignored SIGXFSZ. Actual append retained exact byte0x6d and logged EFBIG27; a separate File probe observed EFBIG27. Supplied canonical EIO remained a distinct logical cause. |

These streams are empty/unsubscribed. They prove neither nonzero capture/ACK/FIN
delivery, authenticated peer behavior, VM/actor cleanup, kernel charges nor
durable metadata. Charge/Learning construction is inert and explicitly labelled.
The completion case splits existing methods; it is not a claimed pause inside
unmodified complete(). The locked metadata tests delay entry, not an actual
kernel-blocked filesystem write. Failed formatted writes can retain a prefix;
a logical winner does not imply successful publication.

Actual write stalls, competing channel rows/readers, nonzero completion/stop
races, publication durability, meaningful retirement-guard removals/restoration
and broader VM/resource/adversarial/cost campaigns remain ROOT work. No production
guard is changed by this test-only batch, and no new defect correction or
universal deadlock/livelock claim is made.

## Exact supported-target gate

Actual ROOT r730 on macOS arm64 runnerb24c5c951260dcdfd0972f2aeaf3cf51 ran full
scripts/lint at exactd2288c27: format, all seven supported targets and GPU checks0.
It separately ran the actual native remote managed corpus15/15,0failed/ignored.
[Source/tool/command/raw matrix records](matrix/matrix/) are authoritative.
Two terminal console labels still say b02e because the copied driver retained
those cosmetic labels; source.txt, checkout and actual command record d2288c27.
This is not a real HVF/WHP VM or native Windows terminal-case run. The paused
Windows ARM VM stays untouched and its full-suite criterion stays unverified.

The first matrix launcher passed Python text to bash and failed2 before Python,
fetch/build or any test. [Exact failure](matrix-launch-failure.json) is retained.
ROOT corrected only the quoted Python wrapper; r730 then passed. No source,
security option, trust setting, compiler flag or test assertion was changed to
obtain the pass. Existing AWS-LC feature-probe warnings remain verbatim.

## Separate actual reconnect red — not a fix in this batch

[First source54188](native/reconnect-first/) compiled normal helpers/caller0
then failed one real KVM reconnect test101. [Exact unchanged binary repeat](native/reconnect-repeat/)
also failed101. Both had actual owner stop/observer join/private run removal and
observer FD7→7, while full population/reaping/kernel return remain unverified.
The first replacement observer received Lost(None) before retained replay.

A [test-only diagnostic source1ff90](native/reconnect-diagnostic/) again compiled0
and failed101, retaining the previously discarded reason:
`aborted by peer: the server refused to accept a new connection`.
This establishes pre-application connection refusal, not the precise old
connection's closed/draining state. The current Driver admission counts all
connections against its single admitted slot and removes them when drained;
its involvement is a source-supported hypothesis, not a directly observed
causal state witness. TLS/identity, application subscription and transport
retirement remain distinct stages.

The diagnostic exists only on p/managed-reconnect-proof-v1 and must be removed
before its adoption; it is not selected here or on main. No admission increase,
retry, sleep, dropped drain state, weakened TLS or protocol shortcut is
implemented. ROOT is isolating the exact behavior with a smaller actual
Endpoint/Producer/Control/Observer fixture. The reconnect source/refusal
checkpoints and full G01–G24/P0–P13 obligations remain separate/open.

## Retention and R01–R16

[ARCHIVES.json](ARCHIVES.json) retains originals, exact transfer SHA256/size and
member manifests. ROOT verified safe unique regular names, every member size/
hash, archive identity and no ELF executables before extraction. Native+reconnect
archive77820B/225regular files SHA256
98394733e148310c886bac77ce8166e051b551a5ea262895f63ba0a4d8f784d3;
matrix5008B/14 SHA256
ea941154e83236379dfb559b77e27b5fb9965b40c8d90f4183886dca6611f3b4.
Runner originals/frozen executables and local archives under
/Users/adalundhe/g121-retained-archives are preserved. No executable or
credentials are committed. The exact Python payloads and shell wrappers are
separately labelled under [drivers](drivers/), copied after execution. Raw
whitespace remains original evidence, without a global source exception.

R01/R04/R06/R07/R13: production unchanged; test states, checked observations,
guard ownership, finite waits and unknown cleanup are explicit. Whole-product
panic freedom is not established by these tests. R02/R03/R05/R09/R10/R12:
linked primary contracts/alternatives and exact actual-type/File/endpoint
examples justify the private test seam; no production constant, manager, cache,
polling algorithm, platform duplicate, host privilege or installed binary.
R08/R14: timings are functional log facts, not distributions or performance;
no4–5×, allocation-free/leak-free or full-population/kernel-return result.
R11: actual GNU amd64 native cases, native macOS remote component and seven-target
compilation are separate; other native/hypervisor combinations unverified,
Intel Mac unsupported. R15/R16: agents supplied scoped implementation/light;
ROOT reviewed whole source/caller, ran heavy/native/matrix, retained failures/
artifacts and owns main landing. Protected caches and user data remain intact;
only the owned file-limit child changes its limit. All broader plan, hard
aggregate/parent/kernel budgets, effect-broker/adapters/credentials and
generation-safe restore/move requirements remain open.

