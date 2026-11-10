# Actual canonical-Aborted and positive main-byte record at eed0

Frozen source `eed0dddf2abcf739e577b42a2f065d84512811de` passed **4 native
canonical-Aborted cases and24 actual KVM cases**, each one executed test with
zero failures/ignored tests. Full normal caller build and scoped Clippy with
warnings denied exited0. This is a tested checkpoint; the managed plan and this
continuous lane remain open.

Assigned adversarial implementer01a1c5d2ee5871a2a9a9cdc9f56793f0;
coordinator01a148884a4770fdaf0111f3f792a248. Policy base cbd392849, unchanged
at current main33df95b08; rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Only the owned new leaf/children and evidence changed. Production, ABI and
matching helper/testguest bytes remain unchanged from the separately frozen57a8
campaign. The unchanged-production record lists every intervening path; it does
not substitute for the new caller compilation.

## Actual native outcomes

The real public Endpoint/Producer/Control and existing authenticated Observer
handled stdout/stderr, each with owner-supplied positive errno5 and i32::MAX.
These are controlled canonical records, not kernel-observed guest read failures.

| Witness | stdout | stderr |
| --- | --- | --- |
| Canonical Aborted payload offset |26|23|
| Remote Finished accepted/consumed after terminal ACK and actual transport End |90/90|87/87|
| Duplicate ACK |typed Protocol, canonical errno/offset and remote cursors unchanged|same|
| New owned payload after terminal |Closed returns exact42 bytes|Closed returns exact39 bytes|
| Owner abort |remote Aborted preserves90/90; finish-after-abort Err(Closed)|remote Aborted preserves87/87; finish-after-abort Err(Closed)|
| Cleanup |endpoint/observer joins; FD4→4|endpoint/observer joins; FD4→4|

All four positive errno selections retained the exact supplied cause. The
wake-only callback uses a bounded nonblocking coalesced notification; the
existing Control predicate is authoritative after notification. No Life reentry,
metadata I/O, periodic polling, new transport or production fix was introduced.
Native observations establish wire/credit/owned-driver behavior only. They do
not establish daemon metadata, actual guest population or kernel cost/return.

Native test:
`tenant_exit::adversarial_controls::native_terminal::canonical_aborted_duplicate_ack_preserves_errno_and_terminal_credit`.
Each fresh process used explicit channel, positive errno, fresh run/generation,
new records, phase60000ms, cleanup30000ms and outer100000ms. The direct caller
enforced timeout100s; the four-case operation had a finite550s outer bound.
No case retry or alternate endpoint was used.

## Actual VM outcomes and retained main bytes

All A6, B12 and C6 selections described in [the57a8 record](LIVE-57A8.md) ran
again against this fresh eed0 caller:24 distinct actual run IDs,24 executed
cases, all0. Typed refusal, held credit, raw record/control bytes and final
metadata expectations remain unchanged. All cases retained controlled fresh
member2 fd1/fd2 after the existing TCP gate and independent main progress before
and after release. Successful main stdout and stderr now have original binary
records `main-after-refusal-raw.*` and `main-after-neighbor-raw.*`; each stdout is
exactly `main-progress\n`, and stderr retains the original asserted
`main-error\n` prefix and optional existing timing diagnostic.

Every VM case retained owner stop, inspect `false 0`, remove/metadata unlink,
observer joins and FD7→7. C6 member1 stays ended/complete102/102 stdout and99/99
stderr; stopped live member2 stays aborted/unknown177/177 and171/171. Invalid
early controls retain the exact held accepted/consumed cursors and unknown tail.
Actual B writes/FIN receipts are local acceptance facts, never backend ACKs.
No partial local write occurred, so that coverage remains unverified.

VM test:
`tenant_exit::adversarial_controls::invalid_application_ack_after_record_preserves_credit_neighbor_and_main`.
Each fresh direct process/run/endpoint used phase300000ms, guestwork240000ms,
cleanup120000ms, outer480000ms, guest fixture port32197 and new records. Its
direct timeout was480s. Finite waves selected6 cases each; no failed case was
retried, skipped, weakened or replaced by a pure pass.

## Source, commands and artifacts

Runner Fedora8fadc1ec213332e16cb0d915a3fb913f, Linux amd64 GNU, Rust/Cargo1.98.1,
Zig0.16.0, same experimental kernel and source-matching own static guest as57a8.
These are not stock kernel-pin, other-libc or other-host acceptance results.
Private `/home/adalundhe/managed-adversarial-controls-live-v1/` owns source,
target, cargo-home, runtime/Zig caches, artifacts and records. Admitted actual
build/campaign envelope2CPU/4GiB/16GiB; small evidence operations1CPU/1GiB/2GiB.
Headroom refusals start no work; resources were not lowered to bypass admission.
No protected ROOT cache/clone, paused WinARM, local target or heavy local build.

Actual full normal caller, including carried helpers:
`cargo +1.98.1 test --locked -p shards --test managed_output --profile test-release --no-run -j2 --message-format=json`.
Exit0; the actual fixture Cargo artifact has `fresh:false`. Scoped
`cargo +1.98.1 clippy --locked -p shards --test managed_output --profile test-release -j2 -- -D warnings`
also exited0. Exact commands/environments, source hashes, actual parent diff,
raw compiler output, artifact identities and every case invocation are retained.
The earlier7 pure checks belong to57a8 and are not relabelled as a new eed0 run.

| Frozen artifact | SHA-256 |
| --- | --- |
| eed0 full fixture |`ddd45ceabb393b11e3853938a2900d8a2b447af83ee38037c961febac5bfee26`|
| Own unchanged CLI |`5cd44adca10f3964cfbe7bed6cbb717aadaf0fe3002a61821146061eed9e2b91`|
| Own matching musl testguest |`0b7b3cca41317c9c67e9ff1c8a318772b786beafe2a9493ca02ec9fa268103ad`|
| Experimental guest kernel |`97819d00fecb5de0cffc4bf292af9c78c5d662336e07226ae58731fd580537ae`|

The approved original raw patch remains
`0178601648d07a27eff18f944eb03ac775c121190bc059cb88fb86bbda8923ee`;
registration remains
`41e04e2d0b1099aae02d698bb1f8cb7c635c99e90bda76549412014a4e090489`.
Applied only in the private candidate, followed by canonical parent formatting.
The local parents are untouched. ROOT's common-module migration is not rebased
into these frozen results; ROOT adapts the same seam and validates integration.

## Verified raw record and collector corrections

`live-eed0-v2.tar.gz`:198384 bytes, SHA-256
`a0353887cdfa47cef00bacfda942d7737f540b5556d8b1a884d653340e7da37e`.
All1387 regular members have unique safe paths; every recorded member hash was
verified locally before extraction into `live-eed0-v2/`. That record includes
the current full build, four native cases,24 VM cases and current Clippy. Older
preparation records explicitly identify the unchanged own guest/helpers.
Remote originals and all frozen binaries remain preserved.
`scripts/check-development-policy`, unchanged-source verification and the named
source/prose whitespace check passed. The unrestricted whitespace diagnostic
reports original raw stdout/patch formatting; its exact output is retained in
`raw-whitespace-eed0-check.txt`. No raw bytes or whitespace policy were changed.

Two collector mistakes are retained separately. First, a broad refusal filename
glob also selected the new main-after-refusal record; validation failed before
archive creation. It was corrected to the numeric trace prefix. Second, the
first successful collector retained an old directory list and omitted the new
build/native directories. Local archive verification caught that omission.
The incomplete archive is preserved as `collector-incomplete-eed0-v1.tar.gz`,
not accepted as this source's record. The corrected collector validates and
includes the exact new directories. `collector-errors-eed0.json` and submitted
collector command text preserve both failures/corrections. No actual fixture,
assertion, source or case outcome changed during collection.

## R01–R16 and remaining work

| Rule | Proof / limit |
| --- | --- |
| R01 | Test-only assertions; fresh actual full caller and scoped Clippy0. Production panic behavior unchanged. |
| R02 | Existing Endpoint/Producer/Control/Observer and canonical fixture inspected; primary sources/alternatives in TERMINAL-CONTRACT and raw-control proposal. Actual28 cases retained. |
| R03 | No production constants/capacity changes. Wire sizes/allowance derive from actual checked ABI records; caller supplies positive bounded work. |
| R04 | Canonical End/Aborted(errno), remote Finished/Aborted, application cursors and transport/owner state have distinct actual witnesses. No finish-after-abort resurrection. |
| R05 | Existing authenticated driver, pending/partial writes and authoritative Control predicate reused; wake-only callback. No new manager/cache/polling/wait implementation. Partial writes not observed. |
| R06 | One focused native child and existing VM caller; no generic framework. Full caller build/Clippy exercised actual source. |
| R07 | Comments/evidence identify lock/wake obligations, supplied cause, local receipt limits and unknown population/kernel return. |
| R08 | No benchmark/comparative speed or4–5× claim. |
| R09 | No production bug/fix claim; actual guard outcomes and collector failures are separately retained. |
| R10 | Exact commands, source/binary hashes, raw bytes/results/progress/cleanup and verified archive retained. Relevant production guard mutations/restoration remain ROOT-owed. |
| R11 | Linux amd64 GNU native/KVM experimental-kernel scope only. Other libc/OS/VM/seven-target combined gates unverified here; macOS arm64 only, WinARM paused. |
| R12 | Shared production ABI/platform/transport, no duplicate OS adapter or installed binary. |
| R13 | Finite one-case/process/run/endpoint, cleanup before strict final assertions. Original smallest parent patches preserved. |
| R14 | No profiles, allocation/fault/kernel/CPU accounting or leak-free claim. FD equality is a bounded observation. |
| R15 | Assigned private bounded campaigns, signed source, no children/goals/main merge. ROOT owns audit/profiling/benchmark/integration; next scoped C case continues. |
| R16 | Private admitted remote scopes; local floor above30GiB, no local build/cache or deletions. All sources/evidence/frozen artifacts preserved. |

Owed ROOT: common-module integration and supported-target gates, relevant guard
mutations/restoration, daemon canonical-Aborted metadata with an actual controlled
cause, publication/fault ordering, full population/reaping/kernel-return evidence,
partial-write forcing and final plan acceptance. No durability/performance/full
closure claim. Next owned C boundary is one valid maximum-size Subscribe after
actual canonical End/ACK/transport End on the existing subscribed endpoint;
expected Protocol with sticky ended/complete and actual neighbor/main progress.
The rejected helper-control/worker-result allocation campaigns remain held.
