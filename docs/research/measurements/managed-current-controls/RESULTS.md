# Requested current-caller proof at2c57

Signed source `2c57ac6d7c410b90fc61a6f186a506eedde840ee`, base
`28e147c4dda846ca023020e03bc9b00eb2e58e94`, passed all three requested actual
cases on the moved shared Client/Observer. Fresh full normal carried-helper
caller and matching static-musl guest builds exited0. Each selected case actually
executed1 test,0 failed/ignored. Seven pure tests passed with three live tests
explicitly ignored in that separate invocation. Full scoped caller Clippy with
warnings denied exited0. No actual failure was retried or omitted.

| Actual case | Observed control and terminal witnesses | Cleanup |
| --- | --- | --- |
| KVM header-only FIN stdout |Real Open;5-byte write accepted/pending0; FIN success; typed Closed4; held accepted28/consumed0, final aborted/unknown;0 queued future target records. Exact new controlled neighbor fd1/fd2 and independent main stdout/stderr before/after release.|Owner stop, inspectfalse0, remove/metadata unlink, observer joins, FD7→7.|
| KVM header-only FIN stderr |Same exact witnesses on the separate stderr endpoint and a fresh run.|Same, FD7→7.|
| Native Subscribe after supplied canonical Aborted, stdout/errno5 |Received exact Aborted offset26/errno5, ACK and actual transport End; remote Finished90/90. Real58-byte Subscribe wrote55/pending3 then3/pending0; typed Protocol1; canonical cause/cursors unchanged. Postterminal owned42 bytes returned Closed. Observer stop/join leaves Finished90/90,0 queued records; owner abort preserves90/90 and finish returns Closed.|Endpoint/observer joins, FD4→4.|

Native cause is supplied, not a kernel-observed guest error. Remote Finished/
Aborted/cursors and canonical Aborted(errno) remain distinct. Its driver result
does not establish daemon metadata or a VM population outcome. Local receipt
acceptance is never treated as a backend application ACK. VM progress is actual
post-refusal controlled neighbor/main output, not a liveness inference from a
command enqueue. All raw canonical/control/main bytes and typed events remain.

## Exact source, environment and bounds

The current shared-client diff is **3939d78f2bf3e175a2c504e859a08a5d8cb2a1d10bf81c3ad868d5f121eee123**;
the **registration** diff is
**83132563b3eadea370e89912f641141638ff310af278e25a24d826d4d003f6bc**.
These are different patches. All four fixture children still match frozen247
byte for byte. Only leaf imports/include paths adapt; all original strict
assertions remain. Exact hashes and patch snapshots are in source-scope.json.
The cfg(test) seam uses the sole existing Client/pending/partial-write/finish
path, its sole constructor initializes None, and no second observer/parser,
control queue, cache, capacity or production protocol is introduced.

Root-reviewed28e includes the metrics-test layout correction only over3f7;
the unbuilt3f7 branch and all older campaigns remain preserved. ROOT's separate
native/legacy current gates do not stand in for this new source's cases. ROOT
still owns the final combined/matrix/other-consumer validation and main merge.

Runner Fedora8fadc1ec213332e16cb0d915a3fb913f, Linux6.19.10-300.fc44.x86_64,
glibc2.43, Rust/Cargo1.98.1/LLVM22.1.8, Zig0.16.0. Fresh private
`/home/adalundhe/managed-current-controls-28e-v1/` owns source, target, cargo-home,
Zig/runtime caches, artifacts and records. Actual headroom admission was
2CPU/4GiB/24GiB, j4; small transfer/collection used1CPU/1GiB/2GiB. No protected
ROOT clone/cache, old helper reuse, local build/target or paused WinARM activity.

Actual normal caller command:
`cargo +1.98.1 test --locked -p shards --test managed_output --profile test-release --no-run -j4 --message-format=json`.
Cargo reports `fresh:false`; normal helpers are carried, with no skip/prebuilt/
ABI override or extraction. Matching own guest used normal guest-profile musl
build with scripts/zig-cc and scripts/zig-ar. The build operation had its original
3600s bound; it did not reset that deadline for clone/toolchain/build phases.
The separate matching guest build also had a3600s bound.

Each actual caller supplied phase/work240000ms, cleanup15000ms, outer255000ms,
fresh records/source/scope and direct hard255s timeout. All Rust phase/cleanup
deadlines remain anchored to the first Instant. Two VM calls used explicit
guest gate32197; native used a fresh UUID/generation1/positive errno5. No polling,
retry, endpoint replacement, reset or removed assertion. Tool outer bounds were
600s for the two VM cases and300s for the one native case.

| Frozen artifact | SHA-256 |
| --- | --- |
| Full current fixture |`6217535e1bdfc54e81334318d68f4359c6d2648234f81c2e67a6156c4f614a69`|
| Current CLI |`643cbf39cae4d3774a89a864103b633a84857283485a7b105edb423d3e2ab007`|
| Matching static guest |`301cbb100b9aab5ad4e58a51bc77d3f04aa5fefca0692309af5c27b944da723b`|
| Own verified experimental kernel |`97819d00fecb5de0cffc4bf292af9c78c5d662336e07226ae58731fd580537ae`|

The kernel is the existing owned experimental TPROXY kernel, copied read-only
with matching digest. This is not a stock product-pin or other-host/libc gate.
Frozen current binaries are outside target under artifacts; original source
and a named private frozen ref are preserved.

## Original raw record and R01–R16

`requested-v1.tar.gz`:50591 bytes, SHA-256
`c6397b7a3db9c1cdedfd733ea6de617b46ba41193c183d5f85c698ffda755c96`.
All173 unique safe regular members and every individual hash were verified
locally. Raw stdout/stderr, source/build commands/environment/hashes, wire/control/
main bytes, typed causes, metadata/progress and actual cleanup remain compressed
without normalization. Exact selected JSON copies, original member-hash index
and storage mapping are beside the archive; remote originals remain intact.
`drivers/` retains submitted operational text, not a new runtime wrapper.

Full current main33df policy/evidence/CLAUDE/plan/handoff reread; rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Assigned implementer/adversarial01a1c5d2ee5871a2a9a9cdc9f56793f0,
coordinator01a148884a4770fdaf0111f3f792a248. Source scope/hash readback preceded
tests; ROOT reviewed the current seam and corrected hash labels were reaffirmed.

| Rule | Proof / remaining limit |
| --- | --- |
| R01 | Test-only source; fresh normal caller, matching guest and scoped Clippy0. No production panic behavior changed. |
| R02 | Current shared Client/Observer/Service and frozen real callers inspected; primary contracts/established alternatives preserved, ROOT-reviewed seam and three current actual outcomes. |
| R03 | Existing encoder/parser CONTROL_BYTES and observed cursors, derived native allowance and explicit caller budgets. No production constants/capacity increase. |
| R04 | Typed Closed/Protocol, canonical cause, remote Finished/Aborted/cursors, transport End and owner stop have separate actual witnesses; no resurrection. |
| R05 | Existing pending/partial-write/finish/readiness and wake-only callback reused. Actual partial55+3 and FIN receipts; no new manager/cache/poll/transport. |
| R06 | Unchanged focused fixture children and one current shared implementation; no framework or duplicate client. |
| R07 | Record explains original-source association, hash mapping, local receipts and supplied-cause/resource uncertainty. |
| R08 | No comparative/4–5× or performance claim. |
| R09 | No product bug/fix; exact expected current failures observed through actual transport, no fake path. |
| R10 | Exact commands/source/build/artifact/raw/cleanup plus verified originals. Relevant product guard mutations/restoration remain ROOT-owed. |
| R11 | Linux amd64 GNU native/KVM with experimental kernel only. Current combined seven-target/other consumer/OS/libc/VM gates ROOT-owed; macOS arm64 only, WinARM paused. |
| R12 | Same shared ABI/platform/transport and moved common test fixture; no OS adapter or separately installed binary. |
| R13 | One selected fresh owned endpoint/run per finite case; original-start deadlines and strict unconditional cleanup preserved. |
| R14 | No profiling/benchmarking, allocations/faults/CPU/kernel/parent accounting or leak-free claim. FD equality and admitted envelope do not establish those costs. |
| R15 | Assigned bounded scope, signed named paths, no children/goals/autoplan/main merge. ROOT owns audits/profiles/benchmarks/heavy integration. Checkpoint is not lane release. |
| R16 | Private admitted remote source/target, Mac floor retained, no local build/cache or deletion. Earlier/current frozen sources/binaries/evidence remain preserved. |

Policy script, canonical format and named source/prose whitespace checks pass;
original raw patch context remains unchanged with its explicit separate record.
Owed ROOT: current final combined/matrix/other shared consumer validation,
meaningful product mutations/restoration, daemon Aborted cause/metadata fault
order, whole population/kernel return and final managed-plan acceptance. No
durability/full closure claim. Next permitted finite queue is the existing six
record-derived invalid/replayed/forward ACK cases on this same frozen source.
Rejected helper-control, worker-result allocator and cotenant-theft campaigns
remain held and are not retried/rerouted.
