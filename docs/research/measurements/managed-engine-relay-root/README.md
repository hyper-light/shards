# Engine stream pump: actual VSOCK compatibility and combined gates

ROOT coordinator, policy main b57e2fcec, R01–R16/evidence/plan/handoff read;
rules SHA256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
This adopts a narrow shared guest stream implementation after real caller
comparison. It does not complete mandatory traffic mediation, parent/kernel
resource admission or management P0–P13/G01–G25.

## Source and behavior

Baseline `0150692542d7203a376a2fbcf4ad08b3ae9feb7b` carries the reviewed current
test closure onto b57. Its production engine is unchanged, SHA256
`e2e4e6e6ae014ddb82aba96362801bd14bf4a69a2955ac44941bd481f6563eea`.
The shared module is then used only by testguest. No old branch is merged.

The current a920-based selection1be and its evidence checkpointdb4 are reviewed
and merged at680090328. Only production `engine.rs` changes from015; all five
selected fixture/module files remain identical. Engine now uses the same native
Linux guest pump as the experimental TCP test caller. Its two directional
buffers reserve one actual OS page each before host dial/HELLO. Readiness waits,
retained tails and typed Reading/Draining/WriteClosed states replace the two
copy loops and their extra scoped upload thread. The existing accepted-connection
worker remains. Mappings are finite per flow, not admitted parent/kernel leases.

Engine peer EOF drains its tail and closes the client's two halves before
publishing EOF. Independent TCP half-close policy remains separate. Faults retain
leg, operation and native cause; error cleanup closes the client, and failed
reservation never dials a direct fallback. HELLO remains token-gated and uses
its exact protocol-sized array. No port, grant, network rule, public command,
installed deliverable, cache or periodic probe is added. Testguest forwarding
does not install a production interception route.

Final source `4fa823919a5afe0cab2c2a74bf5e57aeacf8c840` changes only the copied
test helper's validated string slice to checked access and removes an outdated
engine comment claiming the old1023 vsock bound/allocation-free acceptance.
Production operations are unchanged from680. Final engine SHA256
`050266736a4d86632a094e9f738ca92a4d2815b0d3b919de34146fdaedebcf8d`;
shared pump SHA256 `083bce5ec7f0a96c74d1e44c0aa3d882344a36fce1401b017f8e114a8841fe27`.

## Actual baseline, candidate and regressions

Normal carried CLI/init/server/VM/network builds and matching static musl
testguest pass for each015/680/4fa. No external helper override is used.
Fedora44 amd64 host, Linux6.19.10-300.fc44.x86_64;4CPU/8GiB/16GiB admission,
cargo-j4. These are admission settings, not measured hard physical/kernel peaks.
Each real guest has1CPU/256MiB, case30s/boot120s/outer240s; no guessed AS cap.

The ordinary `run --use-api-socket` caller sends the fixed56-byte GET/_ping and
independently receives the exact242-byte HTTP response ending in OK. Both close
orders are actually run,1passed/0ignored each, at ALL three sources:

| Mode | Old015 | Candidate680 | Final4fa |
| --- | --- | --- | --- |
| Client FIN first | Full response/EOF, local write closed, no late write attempted | Exact same fields | Exact same fields |
| Host FIN first | Full response/EOF; actual late X fails BrokenPipe/errno32 | Exact same fields | Exact same fields |

`comparison.json` compares the exact response hex, read/write counts, phase,
read/write ends, error and late-write outcomes plus worker join. Test0 alone is
not substituted for equality. Guest callerFD4→4, controllerFD9→9, held pidfd
readiness/direct-child reaping and owned worker joins return. Whole-population
and kernel return remain Unknown. Elapsed case times are not performance ratios.
Drivers: [old-015.py](old-015.py), [new-680.py](new-680.py), [new-4fa.py](new-4fa.py).

At final4fa, the entire ordinary managed_output binary passes25/0failed/10ignored
in44.46s; legacy code mode passes1/0ignored in4.15s with its original grants,
effect/error/cancel assertions. Other ignored cases are unrun here. Root Mac
ARM64 `scripts/lint` passes all7 supported targets, GPU and format; native remote
managed tests pass15/0ignored. [mac-4fa.py](mac-4fa.py) retains the exact gate.
No actual new HVF/WHP/ARM64 guest result is inferred; paused WinARM is untouched.

The first matrix attempt680 fails101 at its first Apple target, before later
targets: denied `clippy::string_slice` in the copied test helper. Its actual log
is preserved. Checked `get(..tail)` under existing hex validation fixes that
compiler finding without weakening a lint/assertion. The full new4fa matrix
then passes; the first attempt is not reported as a seven-target pass.

## Kernel and historical native evidence

Both cases use the product kernel, not the experimental TPROXY kernel:
vmlinux-6.18.48-x86_64-17c3ba205400,27758808bytes, SHA256
`48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7`.
The same official release's effective config97678bytes hashes
`434fdb527326d93bbbc13f65cb1bacefca0cb3df0a93f86f5ae7db0cce2b399c`.
Both match published SHA256SUMS, whose digest is
`5586fd117a6959d166186ec2b417a18e752fb646a3bb3bc740e3302250b0b3e6`.
The release target is d217bbbed24b2cd65a0d1e339510c0f50bd15266. The actual kernel
has no embedded IKCONFIG marker; no source fragment was substituted. This is
release/checksum association, not an independent guest runtime config readback.

The [current native record](../managed-tcp-forward/implementation/current-main/validation/README.md)
keeps GNU/musl checks/lints and actual19/20 corpus result. One pressure induction
fails its short-send witness although exact bytes/EOF pass; an unchanged single
repeat observes100 short sends/100 send-WouldBlock and passes. Neither erases the
other. A deterministic pressure fixture remains open; no speculative pump fix
or adjusted threshold follows from that observation.

The historical native correction is a **candidate compatibility regression**,
not a reproduced defect in the old production engine: first candidate3bbd
locally accepts a byte after EOF while the old body refuses BrokenPipe32.
Moving full client shutdown to EOF publication corrects it; changing only
SHUT_RDWR back to SHUT_WR reproduces Accepted1 versus old BrokenPipe, then exact
restoration passes. Actual native endpoint prefixes, joins, failures and
original harness argument mistakes remain in the immutable37ee source records.
ROOT verified those nested archives and retains all18 relevant original Git
blobs in [historical-native-37ee.tar.gz](raw/historical-native-37ee.tar.gz).
This is evidence selection, not an old-branch/product merge. The initial archive
verifier's object/list mistake is identified in its provenance; no raw changed.

## Review, literals and remaining work

| Rules | Coordinator evidence / limit |
| --- | --- |
| R01 | New maps/arrays/native results and progress arithmetic are checked/fallible; denied panic lints pass7targets. This does not certify every older caller allocation/error path or new fault-path coverage. |
| R02/R05/R12 | Existing two-copy-loop baseline, native independent/engine policies, exact old/new byte/end behavior and current shared guest seam; no second pump/OS implementation/cache/sensor. Original pinned Linux Unix shutdown research is retained. |
| R03 | Two buffers are the two directions; each engine buffer is one actual sysconf page/minimum private mapping unit. HELLO uses HEADER+TOKEN protocol fields; signed poll range/millisecond conversion follows native poll ABI, -1 its event-wait sentinel. Bounded test deadlines/byte ceilings/queue values are explicit caller inputs. No measured default becomes runtime policy. |
| R04/R06/R07/R13 | Explicit halves/end policy/typed faults and one retained-tail loop; no untrusted lock-held I/O or hidden new lifecycle owner. Old numeric accept-loop commentary removed. ProcessOwned is the existing engine process lifetime, not a fabricated infinite Instant. |
| R08/R14 | No benchmark/profile/resource-win claim. Extra thread removal and2maps are source facts; latency/throughput, allocations/faults, host-induced/kernel charges and fairness under sustained load remain ROOT work. |
| R09/R10 | Historical actual candidate RED/guard mutation/restoration preserved; current carried VSOCK baseline/candidate equality, original matrix failure and fixed pass retained with exact sources. |
| R11 | Full7/GPU/fmt and actual KVMamd64; native GNU/musl endpoints and Mac remote corpus. Other actual VM combinations remain unverified. |
| R15/R16 | Scoped signed current selection; ROOT source/archive review, combined KVM/matrix and main merge. Separate agent targets/frozen records preserved; local Mac remains above30GiB. |

All new current raw members are unique safe relative regular files and verified
against original length/SHA256 in [raw/verified.json](raw/verified.json):223 Linux
payloads+manifest,189566bytes/SHA1889a2c8e9993ae55f5968a996015228a3ff18f1456a144c439cc2f7433d1b58;
25 Mac payloads+manifest,7166bytes/SHAb027d53eeb0285f8e1c49a6523e31436eb525cd4618320fb4536453123d297b6.
Frozen ELF/guest artifacts, regenerable caches and tmp remain on their owned
runners; artifact hashes/compiler receipts identify them. Historical archive
verification is in [historical-verified.json](raw/historical-verified.json),
including151/119 original indexed payloads plus their respective manifests.

No full route ledger, hard aggregate/producer/kernel budget, native effect
broker/harness/credential contract, durable mandatory audit or management-epoch
restore/move guarantee is closed by these results. Those remain required work.
