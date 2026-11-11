# Allocation-backed managed capture

Implementer `/root/managed_resource_enforcement`, coordinator `/root` (Kiln
`01a148884a4770fdaf0111f3f792a248`), base
`0342b85d9c808e3ced041a6c3ef0e9bf70438272`. Rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
The full policy, plan and handoff readback is retained in
[OWNER_INPUT.md](OWNER_INPUT.md). ROOT approved this scoped source extension:
existing capture-buffer ownership transitions in `crates/remote/src/managed.rs`
and resource/credit/bridge portions of `crates/shards/src/daemon/managed.rs`.
There are no net driver, vendor, init, ABI, metrics, external caller or main edits.

This is an implementation of owned capture backing storage. It preserves
existing managed launch behavior. It does not establish owner entitlements,
hard parent/kernel reservation, complete transport accounting or protected
control capacity. The [missing producer contract](OWNER_INPUT.md) remains
required work owned by ROOT, not an optional product limit.

## Ownership and transitions

| Existing owner/state | Backing lifetime and next transition |
| --- | --- |
| `Endpoint::bind` → `Slot::Waiting` | Fallibly allocate the actual record Vec before endpoint/actor publication; retain its actual allocator-reported capacity |
| `Waiting` → `Ready` | Checked subscriber/cursor makes the same allocation available |
| `Ready` → `Capturing` | `Producer::take_buffer` transfers that Vec to the existing credit owner; a second take cannot succeed |
| `CreditSlot::Granted` → `Receiving` → `Captured` | Credit owns the allocation before its wire write; bridge reads into it without allocating a replacement |
| `RevokedGranted` → `RevokedReceiving` | Preserve the already issued buffer to drain its finite input after revocation |
| `Captured` → `Record` → `AwaitingAck` | Submit moves the same allocation into the existing remote slot; transport progress does not return record credit |
| Application ACK → `Ready` | Clear and recycle that allocation after the contiguous application cursor check |
| Observer disconnect/reconnect | Preserve a borrowed or unacknowledged record and its original cursor; no second buffer or progress cache |
| Finish/abort/error/drop | Idle finish releases backing; captured finish waits for the record/ACK. Existing daemon worker error/drop boundaries abort the owner, terminalizing detached capture and retiring its storage |

The API returning a buffer documents its actual caller obligation: submit it or
abort its `Control` on cancellation/error/drop. It does not add a second manager,
standalone pool, periodic poll, connection cache or a raw backend handoff.

Initial accounting adds allocator-reported record-capacity excess before actor
release. Growth keeps the old daemon allocation charged while fallibly preparing
the replacement, includes the changed existing VM record allowance, accounts
actual replacement capacity, then retires the old backing charge before new
credit publication. A prepared growth charge rolls back on failed acquisition.
All arithmetic is checked; protocol bounds remain the existing frame/offset
limits. No runtime threshold, ratio or public default is introduced.

## Scope and evidence limits

This is a new preallocation/ownership requirement, not a source-suspected bug
fix; no live defect reproduction is claimed. Existing `submit(Vec<u8>)` callers
retain their owned-payload and pointer-preserving behavior. New light units
exercise prepared allocation identity, transfer/second-take refusal, ACK reuse,
reconnect/cursor preservation, growth and explicit terminal cleanup. Source/unit
behavior does not prove real transport, kernel charge, allocator-fault or whole
VM isolation results.

The service already owns inline `[u8; CONTROL_BYTES]` control backing. The net
driver's receive scratch Vec, receive BytesMut, datagram Vec/VecDeque entries,
command/connection state, QUIC/TLS/handshake internals, thread stacks, allocator
overhead and induced kernel/parent costs remain outside this slice. Allocating
unused shadow storage for their numeric allowances would not back those real
allocations and was deliberately not implemented. Root's broader complete
resource and protected-control obligations remain open.

## Decision sources and alternatives

The [Rust Vec implementation at the exact observed compiler revision](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/alloc/src/vec/mod.rs#L1506-L1546)
specifies fallible reservation and permits allocator-reported capacity beyond
the request. The implementation therefore checks the existing protocol maximum
and accounts the reported capacity. Vector clearing preserves its allocation;
the pointer/ACK tests exercise that actual reuse rather than infer it from a
numeric limit. The [parent contract](OWNER_INPUT.md) separately retains pinned
Linux/cgroups sources; a successful Vec allocation is not exclusive physical
backing, a cgroup reservation or a future kernel-allocation promise.

The existing allocate-per-record path is the compatibility baseline. Reusing
its actual owned Vec through the existing slot was selected to meet the new
pre-release backing requirement. A second spare pool or unused buffers beside
QUIC were rejected because they would add retained memory without owning the
actual transport allocations. No comparative speed, CPU, RSS or allocation-count
win is claimed; ROOT owns those measurements and any broader topology decision.

## Checks

This checkpoint has scoped light results below. ROOT owns remaining faults,
whole-caller live before/after and meaningful mutation,
allocation/copy/fault profiles, heavy real E2E, seven-target/VM integration and
main merging. macOS support is arm64 only. The Windows ARM64 environment and
all held campaigns remain paused/untouched. No active source, cache, evidence
or frozen output was removed.

The first remote constructor/slot unit run is retained in `records/r1.json`
(18 passed, no failed/ignored); its earlier source hash is not a later source
gate. Initial adapter r2 was deliberately stopped before tests because it
omitted product dependency pins (`records/r2-stopped-unpinned.json`, exit 70).
Adapter r3 then failed compilation before tests on unrelated test fixtures left
after removing their caller (`records/r3-adapter-compile-failure.json`, exit 101).
Both original private trees/locks and target artifacts remain available.

The corrected adapter excludes only the outer startup/key lookup and unrelated
generation/key/ROOT terminal-order fixtures. The modified credit/accounting/
bridge core is otherwise actual source and uses real remote, registry, VMM,
ABI and IPC dependencies. Its `host_available=u64::MAX` and inert log sink are
explicit pure-test inputs, not a host-capacity or diagnostic-path result.
All non-adapter dependency tuples match the product lock
(`records/dependency-pins-v2.json`). r4 executed all 13 core terminal/credit units
with zero failures, ignored or filtered cases; see
`records/r4-daemon-core-units.json`. Full startup/caller compilation remains owed.

Final native light batch r5 returned 0 with unchanged product/lock hashes:
18 remote units passed, zero failed/ignored (nine unrelated units filtered),
strict remote library Clippy 0, strict adapted-core library Clippy 0. Exact
commands, output streams and source SHA-256 values are in
`records/final-v1/receipt.json` and `records/r5-final-tool.json`. The adapter
unit result and lint do not establish full Daemon startup integration.

### Exposed public submit/loan boundary: exact red then typed guard

At signed `1ae95bbda`, r8 used the real canonical Observer and public Producer
APIs: submit one owned byte, receive it over authenticated QUIC, send the actual
application ACK, then ask for the next advertised capacity and loan. The
producer advertised **58 bytes** but lent an allocation with **capacity 1**.
The test returned 101 for that exact assertion, after both actors joined.
Product source hashes stayed unchanged. This is an exposed API boundary, not
evidence that the current daemon mixes these public APIs.

The narrow guard returns typed `Admission::Capacity` before entering Capturing
when the actual Vec cannot back the selected window. It allocates no substitute
or shadow and preserves the direct owned-Vec submit path. The daemon propagates
the typed failure before writing new credit. With identical fixture/Observer
bytes, r9 returned 0: typed refusal at the advertised 58-byte window, then a
successful 58-byte owned submission on that same real QUIC connection; both
actors joined. The fixture adapts only the old/new Rust return signature,
without changing either observed result. Exact raw streams and hashes are in
`records/boundary-{red,green}-v1/` and their tool JSON receipts.

Current-source r10 returned 0: remote managed units **18 passed**, adapted
daemon core units **13 passed**, zero failed/ignored; strict remote/core library
Clippy both 0. See `records/final-v2/` and `records/r10-final-tool.json`.
Current source SHA-256: remote
`005bdb67ced70ecb26312c5738c9b53eb092c09f7ca84e8f8f75fc9dc536da95`;
daemon `2eb2ce4a683786ab46bc6604cf9e890ea47ed76f5e44c953642212040b84230a`.
Actual caller/drop/charge invariants and limits are in [OWNERSHIP.md](OWNERSHIP.md).

The first local boundary attempt had zero idle CPUs and was not admitted.
A separate synced Mac refused before preparation/build because only
4,608,966,656 bytes were free, below the 30 GiB floor. No test ran there and no
cache was reclaimed. After fresh local headroom returned (14 idle cores and
about 180 GiB disk), r8/r9 ran with the original 2 CPU/2 GiB/4 GiB, 600-second
batch envelope and explicit 10,000 ms case deadline. All holds and eligible
run receipts remain retained; no guard was bypassed.

Full whitespace check of checkpoint `1ae95bbda` returns 2 solely for the
authentic test stdout's extra blank line at EOF. The commit shell sequence
incorrectly continued; no full-check pass is claimed. Raw evidence remains
untouched and the narrower Rust/docs/scripts/JSON check passed. The mandated
gits wrapper created the checkpoint; local verify-commit reports Kiln-only
verification, so no independent local verification is claimed. See
`records/checkpoint-gates.json`.

## R01–R16 account and coordinator handoff

| Rule | This slice's evidence and remaining obligation |
| --- | --- |
| R01 | New record acquisition uses `try_reserve_exact`; actual capacity/protocol and arithmetic checks precede resizing/credit. Buffer reads stay within owned capacity. Poison handling follows existing locks. Original opaque/Arc/thread/kernel allocations and actual allocator-fault paths remain ROOT checks. |
| R02 | Exact compiler Vec source, pinned Linux/cgroups research and actual Slot/credit caller trace; alternatives above. Live mechanism/complete resource design and comparative cost remain ROOT work. |
| R03 | Existing protocol maximum/offset/control bounds only. Zero represents empty/no-excess state; no throughput threshold, amount, percentage or public default is introduced. Test capacities are fixture inputs. |
| R04 | Explicit Capturing and prepared/committed growth states; table above names transfer, ACK, reconnect, revoke/error/terminal ownership. Allocation refusal is typed; rejected submit still returns its payload. Full races/faults require ROOT live checks. |
| R05 | Reuse the single existing Slot and credit ownership, their notifications and lifetime. No cache/pool/poll/manager is added. Shared locks are not held over actor I/O or allocation growth. No measured cache or wait benefit is claimed. |
| R06 | Focused existing producer/credit transitions plus one scoped acquisition guard; no generic framework. ROOT owns independent complexity/path audit. |
| R07 | Comments document transfer/drop obligations, replacement overlap and storage-versus-entitlement limits. |
| R08 | No competing-baseline or speed/resource advantage claim. The 4–5× objective remains unmeasured, not passed. |
| R09 | Preallocation is a new requirement. The separate public submit/loan symptom was reproduced through real QUIC/ACK at unchanged signed bytes before its guard, then passed with the same fixture. This is not a daemon caller/VM defect claim. |
| R10 | Commands/source hashes, native API red/green, actual logs and failed/stopped/held attempts retained. Broader caller/VM before/after, independent guard removal/restoration and actual whole-population/kernel cleanup remain ROOT work. |
| R11 | Native macOS arm64 light results only. Seven targets, Linux libc/kernel coverage and actual KVM/HVF/WHP remain unverified here. Intel Mac unsupported; Windows ARM64 remains paused. |
| R12 | Shared standard Rust buffer ownership; no OS adapter, privilege, installed helper or weakened crypto. |
| R13 | Actual record storage ownership at the existing producer seam. Other required resource classes remain open; unused shadow backing is not offered as enforcement. |
| R14 | No profiling/benchmarking by this implementer. ROOT owes actual allocation/reallocation/copy/fault, retained/parent/kernel/neighbor costs and equivalent distributions. Pointer reuse tests do not prove allocation-free or leak-free operation. |
| R15 | Scoped implementer, no children/main merge; bounded private light jobs only. Signed initial checkpoint 1ae95bbda plus the typed-guard follow-up supplied in handoff. Exact caller/live/fault/mutation/profile/matrix gates and evidence review remain ROOT's. |
| R16 | Own source/target and finite jobs; Mac lock and 30 GiB floor respected. No cache, evidence, frozen result or other lane's source was removed. Failed generated sources/locks remain preserved. |
