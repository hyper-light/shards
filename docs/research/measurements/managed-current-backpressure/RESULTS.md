# Actual owned-output campaign at 82019

All three approved live cases ran exactly once and passed, with no ignored test,
retuning, traffic inflation, fake return or retry. The immutable live source is
82019ce362dea316bfe582db54f6f0591579bede, directly from main a920c85b5. The later
4ef5589087306f7cc9a10cec075a8433213dc55e changes only a test helper's assertion
error rendering for Clippy; no live result is attributed to that later revision.
No production source changed. Policy/role, primary alternatives and the full
R01–R16 accounting are in README.md and the two source-scope manifests.

## Actual results and forbidden effects

| Actual case | Observed result | Terminal/cleanup witnesses |
| --- | --- | --- |
| Native Full, stdout/errno5 | Observed Open; Pending28/0 and capacity0 before application ACK. One checked42B candidate returned typed Full with the same allocation/capacity/exact bytes. Pending28/0 and capacity0 unchanged; candidate not retried. | Original records then progressed after real ACK/credit notification. Canonical Aborted(offset26,errno5), transport End, separate Finished90/90; late duplicate ACK Protocol; exact postterminal Closed return. Observer join/no future records, owner abort/finish Closed, endpoint join, FD4→4. |
| Native post-Aborted Subscribe58 | Actual write counts55,0,3. At zero, pending was the identical nonempty3B suffix [0,0,90]. Actual writable callback matched the owned connection/stream with that same suffix; positive3 then pending0. Original positive accounting passed independently. | Typed Protocol; canonical errno5 and Finished90/90 unchanged. Exact postterminal Closed return, observer stop/join, then owner Aborted90/90/finish Closed and endpoint join, FD4→4. |
| Real KVM post-End Subscribe58, stdout | Actual write counts43,0,15. Zero retained the exact15B suffix. Actual owned writable callback preceded positive15/pending0; original positive assertions passed. | Canonical End/ACK/transport EOF, typed Protocol. Final member1 metadata ended/complete102/102 and99/99. Actual new neighbor fd1/fd2 plus independent main stdout/stderr; owner stop, inspectfalse0/remove/unlink, joins, FD7→7. Live member2 ended only at owner stop: aborted/unknown177/177 and171/171. |

Native cases each executed1test/pass0failure/0ignored. The VM case likewise
executed1/pass0/0ignored. Their respective raw test durations were0.05s/0.06s/1.74s;
these are finite-test receipts, not latency/performance measurements. Fresh scopes
are retained in validated-cases.json: native UUIDs b797e6a7-e72e-4440-9e49-84c10f1020ca
and14aaa653-eac9-4987-9c2d-00271bc711d4; VM run
7c30c735424c775d0b34bd5ed1b10b20e0e489386aeb0e5c8504e1d6d45ec0de.

The saved actual attempt/backlog/result/callback receipts establish zero and
resumption; their absence was never treated as evidence. Local write acceptance
is not peer ACK/backend success. Typed refusal and canonical/final state have
separate actual witnesses. Parent/kernel costs, full population return and remote
connection drain remain Unknown; stop/PID0/FD return do not establish them.

## Source, commands and build

Private Fedora8fadc, Linux6.19.10-300.fc44.x86_64/glibc2.43, Rust1.98.1
(48a229cea, LLVM22.1.8), Cargo1.98.1, Zig0.16.0. Each actual case admitted
2CPU/4GiB/24GiB with240s work+15s cleanup/direct outer255s. All source, target,
Cargo/Zig/runtime caches and artifacts are owned beneath
/home/adalundhe/managed-current-backpressure-a920-v1; no ROOT clone/cache used.

The normal full carried-helper caller build passed0 (fresh artifact; Cargo6m18s,
394.57s including setup). Command:
`cargo +1.98.1 test --locked -p shards --test managed_output --profile test-release --no-run -j4 --message-format=json`.
No skip/prebuilt helper/ABI override was used. Matching testguest static-musl
build passed0 (38.13s), with the existing scripts/zig-cc/zig-ar and musl target.
The own verified experimental kernel is retained; this is not a stock product-pin
gate or additional platform coverage.

| Frozen artifact | SHA256 |
| --- | --- |
| managed_output (62391992B) | 1193e4a97a651325bdfe1eb9c503c47e2b20cb586dd38ee67f276000ad46fe84 |
| shards (231527872B) | 157ccb4df72c7062a171698b7262edb4be6fc2a658989f3d06e3ce8d7f09fbe2 |
| matching guest (3301880B) | f91a6578f0fa75e9936ffdd55036449f0ee1557dea3090ca0b95e6bd1fc6b32e |
| experimental kernel (27760784B) | 97819d00fecb5de0cffc4bf292af9c78c5d662336e07226ae58731fd580537ae |

The exact submitted build/guest/Full/native-zero/VM/check/collector commands are
the named *.command.txt records. Their actual argument arrays, stdout, stderr,
exits, finite bounds, environment, artifact hashes and JSON/binary witnesses are
inside the archive. The frozen remote source HEAD and frozen/backpressure-82019
ref retain82019. For a new reproduction pin that commit explicitly (git fetch
origin82019ce362dea316bfe582db54f6f0591579bede); the own branch has since advanced.
Do not rebase the retained source or call a pure test an actual endpoint result.

## Focused checks and retained lint failure

At82019, focused actual-binary pure checks passed9/0failure, with5 explicitly
ignored actor entrypoints in that pure-only invocation. Three assigned actor
selections ran separately as the live cases above. The two new receipt tests
reject positive-only/missing-zero coverage, mutated zero backlog, missing
readiness, incomplete writes and invalid returned counts/order. They parse
receipts; they do not model or drive an endpoint.

Strict fullcaller Clippy at82019 returned101: native_terminal.rs314 used denied
Result.expect in a helper. Original stderr/exit are preserved. The subsequent
signed4ef5589 delta replaces only that error renderer with unwrap_or_else(panic),
using the same success/error condition. The shared-client and seven other scoped
source hashes remain unchanged. lint-rendering-delta.patch and lint-source-scope.json
record the exact distinction. The separate private source-lint/target-lint strict
Clippy job r151 is RUNNING at this evidence checkpoint; no pass is claimed yet.
No additional live case/retry was scheduled for this rendering-only delta.

Canonical named-source rustfmt--check0, source/prose diff-check0 and
scripts/check-development-policy0 pass locally. The remote full caller also ran
the policy checker0. Exact raw patches/context remain unchanged and separate
from prose whitespace checks. ROOT still owes actual combined latest-candidate
validation; a lint/type result does not replace it.

## Raw inventory and remaining work

evidence-backpressure-v1.tar.gz is51423B, SHA256
9e081b23a468adea82acef7c06eb997623049361e53202acdefa9d754b820318.
Exact inventory:152 original payloads + member-hashes.json =153 total members.
All153 paths are unique safe regular files; every152 original hash was verified
locally. The copied outside manifest and validated-cases.json are byte-identical
to their archive members. The remote originals/archive and all frozen2c57/
earlier checkpoints remain preserved. There were no case/collection validation
errors. Original Clippy101 is a real retained failure, not relabeled green.

Next in this approved queue is completion of the already running targeted lint
correction and its signed raw proof. The three finite live selections are done;
this does not release the lane or close P0–P13/G25. ROOT owes combined composition,
meaningful guard mutations/restoration, supported-target/real-hypervisor matrices,
broader disconnect/revoke/order/resource/race/durability gates and any audits/
profiling/benchmarks. No held helper-control/worker-result allocator/cotenant
theft, paused WinARM, children, security change or main merge was attempted.
