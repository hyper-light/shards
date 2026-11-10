# Actual authenticated control campaign at57a8

Full caller/source revision `57a8cdb3edefe0a363534ad5e2c2676d9f3dc886` on policy
base cbd392; assigned adversarial implementer01a1c5d2ee5871a2a9a9cdc9f56793f0,
coordinator01a148884a4770fdaf0111f3f792a248. Rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.

**24 actual KVM cases passed**, one selected case per fresh process/run/endpoint.
Each actually executed one ignored test with zero ignored/failed results. This
closes this revision's listed cases, not the lane or managed-runtime plan.

| Group | Selections | Actual result |
| --- | --- | --- |
| A6 | partial/replayed/forward application ACK ×stdout/stderr | Protocol1; held target accepted/consumed28/0 or replay58/28, aborted/unknown after stop; no future queued target record. |
| B12 | unknown tag, known wrong tag, overdeclared length, wrong ACK length, truncated header/ACK ×stdout/stderr | First four Protocol1; truncated input+FIN Closed4; actual4/5/12-byte writes, zero pending, requested FIN success. Held credit28/0. |
| C6 | duplicate/forward/ACK+FIN after canonical End+ACK+actual transport End ×stdout/stderr | Protocol1; final member1 rows remain ended/complete102/102 and99/99 after invalid control and owner stop. |

All24 cases passed exact new controlled member2 fd1/fd2 after the owner TCP gate
and independent main execution checks. All24 retained successful owner stop,
observer joins, inspect `false 0`, remove/metadata unlink and observer FD7→7.
Member2's stopped rows remain aborted/unknown177/177 and171/171. There are24
distinct actual run IDs; no endpoint replacement/retry was used.

Application enqueue and local write acceptance were never treated as peer
consumption. Original canonical/control bytes and actual Lost codes are retained.
The tested B executions produced Write/Finish receipts; no Read receipt occurred
after the raw-command sink was armed. The already received canonical Open and
later typed close events are separate witnesses. No partial local write occurred
in these executions, so partial-write callback coverage remains a separate limit.

Main stdout/stderr were checked by the exact existing `Fixture::main_progress`
method; their successful raw bytes were not separately saved by that older helper.
The next owned source batch will retain them through the existing CLI seam.
This record does not invent missing bytes or retrospectively claim their retention.

## Exact environment and build

Fedora8fadc1ec213332e16cb0d915a3fb913f, Linux6.19.10-300.fc44.x86_64,
glibc2.43; Rust/Cargo1.98.1, Zig0.16.0. `/dev/kvm` was accessible. The actual
kernel was the handoff's **experimental** TPROXY kernel, copied read-only into
the owned artifact directory after verifying SHA-256
`97819d00fecb5de0cffc4bf292af9c78c5d662336e07226ae58731fd580537ae`.
These are not stock product-pin or other-host/libc results.

Private scope `/home/adalundhe/managed-adversarial-controls-live-v1/` owns source,
target, cargo-home, runtime-cache, later Zig caches, artifacts and records.
The source is a fresh remote clone of the signed own branch, not a ROOT clone or
target. Source/target state was exclusive during each build/campaign. No local
compile, protected ROOT helper/cache access or paused WinARM activity occurred.

Approved test-only patches were applied only in that private candidate:

- `raw-control.patch` SHA-256 `0178601648d07a27eff18f944eb03ac775c121190bc059cb88fb86bbda8923ee`.
- `register.patch` SHA-256 `41e04e2d0b1099aae02d698bb1f8cb7c635c99e90bda76549412014a4e090489`.

Canonical parent rustfmt then changed layout only. Exact actual source diff and
source hashes are in `live-57a8-v1/build-v2/`. Every original Client constructor
sets its optional receipt sink to None. The original patches remain unapplied
in this implementer's local worktree and associated with this frozen campaign.
ROOT's later move of the common client will require adaptation and new validation;
these results do not transfer automatically to that source.

Actual full normal `cargo test -p shards --test managed_output --profile
test-release --no-run -j2 --locked --message-format=json` exited0, including the
normal carried helpers. It produced a fresh actual full caller; no extraction,
stub or skipped helper stood in for it. Matching static-musl testguest build
exited0. Exact full caller's own pure checks were7 passed/0 failed/1 explicitly
ignored VM case; these are separate from the24 actual executed VM cases.
Scoped full caller Clippy with warnings denied exited0.

| Artifact | SHA-256 |
| --- | --- |
| Full fixture | `28f3f24a5b153607bb88e6151130f34390e05a87f2cd2ada2feb83e0709187b0` |
| CLI | `5cd44adca10f3964cfbe7bed6cbb717aadaf0fe3002a61821146061eed9e2b91` |
| Matching testguest | `0b7b3cca41317c9c67e9ff1c8a318772b786beafe2a9493ca02ec9fa268103ad` |

Per-case `artifacts.json` also retains the actual init and compiled leaf/child
hashes. Own frozen executable copies remain remote under `artifacts/`; binaries
are not committed. Initial normal compilation used Zig's default shared toolchain
cache `/home/adalundhe/.cache/zig`; ROOT was notified to protect it while active.
Subsequent guest/runtime operations used explicit owned caches. No cache was
deleted. The admitted envelope is not a measurement of aggregate parent/kernel
cost or a hard per-object accounting proof.

Build envelope2CPU/4GiB/16GiB, build1800s/outer2100s; case phase300000ms,
guestwork240000ms, cleanup reserve120000ms anchored to original start,
outer480000ms. Every direct case invocation enforced that outer timeout.
Wave counts were finite (5or6 cases), each with its own process/run/records.
Temporary headroom refusals started no job; resources were not reduced to bypass
the guard. Local Mac floor remained >=30GiB, with no local target/cache.

## Durable raw record and failures

Archive `live-57a8-v1.tar.gz`:175237B,
SHA-256 `b9faca18a671f24c63370138144bb64dd7a5620702f3db8d6dddf902577da5b5`.
All1166 regular members, safe paths, uniqueness and individual hashes were
verified locally before extraction. Originals remain remote. The extracted
`live-57a8-v1/` contains exact build/caller commands, source diff/hashes, toolchain,
artifact hashes, raw stdout/stderr/exits, input/metadata/control/record bytes,
typed events, progress and cleanup for every selected case. The collector also
required exact positive execution counts and distinct run IDs. Exact submitted
operational text is retained in `drivers/`; these are records, not new runtime
wrappers or installed deliverables.

Two launcher failures are preserved: sh was incorrectly used for the Python
policy script (exit2 before Cargo); the first B launcher tried to print an A-only
missing receipt file (exit1 before any B case). Only the launchers were corrected;
no fixture expectation or production semantics were changed. No failed actual
case was retried or omitted. `launcher-failures.json` preserves tool outcomes.

## R01–R16 and remaining gates

| Rule | Proof / remaining limit |
| --- | --- |
| R01 | Test-only source; full caller build and scoped Clippy0. No production panic path changed. |
| R02 | Existing authenticated Client/Service/Io and normal fixture inspected; RFC9000, actual source and alternatives recorded in proposal/terminal contract. Actual24 outcomes now retained. |
| R03 | No production literals/capacity increase. Cursors derive from observed records; input lengths use the real encoder/checked parser. Explicit caller budgets/port. |
| R04 | ABI StreamState and typed Protocol/Closed matched; application cursor/canonical offset/logical terminal/transport/ownership remain distinct. |
| R05 | Existing pending/partial-write/readiness path reused. Single raw command after flushed subscription; no new wait/socket/protocol implementation/cache/polling/manager. Partial writes not encountered live. |
| R06 | Focused test selection, trace, receipt accounting, run and cleanup. No generic adversarial framework. |
| R07 | Lock/wake and witness limits documented; original raw bytes preserved. Historical release-helper label limitation remains disclosed. |
| R08 | No benchmark, speed/latency/resource comparison or4–5× claim. |
| R09 | No production correction or new confirmed production bug claimed. All listed real guards returned expected outcomes. Launcher errors are separately identified. |
| R10 | Exact full build,24 actual cases, raw bytes/hashes/callers/cleanup and failures retained. Relevant production guard mutations/restoration remain ROOT-owned. |
| R11 | Actual Linux amd64 GNU/KVM with experimental kernel only. Other libc/host/VM combinations and final seven-target source gate remain ROOT-owed; macOS arm64 only, WinARM paused. |
| R12 | Shared production ABI/transport/platform seams; no OS adapter or extra installed binary. |
| R13 | One selected finite case/process/new run/endpoint; cleanup precedes final strict metadata assertion. |
| R14 | No allocation/fault/CPU/kernel-cost profiling or leak-free claim. Runtime headroom and FD observations do not establish aggregate accounting. |
| R15 | Assigned bounded adversarial scope only, signed source, no children/goals/autodispatch/main merge. ROOT audits/profiles/benchmarks/integrates. Queue continues after checkpoint. |
| R16 | Private admitted remote source/target, preserved originals/artifacts, active cache notice. No deletion or heavy local build. |

Still owed: actual canonical-Aborted native wire/credit case (local owned source
is a separate uncompiled batch), daemon metadata with a controlled canonical
Aborted(errno) cause, publication/fault ordering, full population/reaping/kernel
return, partial-write forcing, relevant guard mutations/restored passes and
combined supported-target/integration gates. No durability, full managed-plan or
overall lane completion is claimed.
