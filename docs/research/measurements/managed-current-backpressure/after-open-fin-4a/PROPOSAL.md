# After-Open empty FIN with an unacknowledged record

PREPARATION ONLY. ROOT review of this exact source/ordering is required before
building or executing it. No new native, pure, VM or mutation result is claimed.
The completed post-Aborted/ACK/transport-End case at source
`d8a2bf8aa4469c8962a825cb8e2154b7c175f091`, checkpoint
`4a6c4321686fdcdf1db86a9b18325ec1427436bd`, remains frozen and separate.

POLICY_READ implementer / assigned adversarial, main
`0342b85d9c808e3ced041a6c3ef0e9bf70438272`, rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Full AGENTS, R01–R16, development-evidence, CLAUDE, managed plan, updated
`/Users/adalundhe/g121-handoff.md` and its combined-0ae/engine-relay closing
records were read. Branch `p/managed-after-open-fin-proposal-v1` starts exactly
at4a in the reused managed-network-assertions worktree. No main/old2c rebase.

## Exact interface and source scope

Only `crates/shards/tests/managed_output/current_controls/native_terminal.rs`
and this new evidence directory change. The shared Client/Observer, registration,
RawControl, receipt enum/serialization, positive accountant, parser, remote/daemon
production and ABI remain byte-identical to4a. [source-scope.json](source-scope.json)
and [native-caller.patch](native-caller.patch) identify the exact prepared bytes.

The existing actual interface is used unchanged:

```rust
Ask::RawControl { bytes: Vec::new(), finish: true, receipts }
ControlReceipt::Finish { error, pending }
Ask::Ack(observed_wire_end)
```

`NativeCase::{Terminal(TerminalControl), AfterOpenEmptyFinish}` separates the
nonterminal experiment from the three existing terminal controls. The latter
keep their original test entrypoints, positive supplied-errno constraints and
assertions. The new case has `supplied_errno: null`; it never constructs Data,
canonical End or canonical Aborted. A variable-length test record list selects
one Open versus the original three records; the sole Endpoint/worker setup is
shared. There is no second native transport implementation or manager.

The new fixture retains the SAME RawControl receipt receiver in its existing
Native owner until cleanup, so an early panic does not discard unread receipts.
This is ownership of that existing channel, not an additional queue/cache. Its
first actual receipt is persisted and checked before the caller consumes the
peer event. Final cleanup records unread receipt/event tails. No synthetic
receipt or inference from absent writes is used.

## Proposed decisive order and evidence

One ignored entrypoint is prepared:
`tenant_exit::current_controls::native_terminal::canonical_open_empty_finish_retains_unacked_record_until_authorized_replay`.
It is uncompiled and unexecuted. One stdout case only, one native Endpoint,
one existing endpoint worker, and two sequential instances of the same existing
Observer constructor. No guest/daemon/actor exists in this scope.

1. Bind the existing authenticated Endpoint with one fresh owner/server identity,
   scope/run/generation and loopback address. Admit the actual encoded Open Vec's
   capacity; transport allowance is that capacity plus existing DATA_HEAD.
   Existing TLS/datagram bounds and finite patience are unchanged. The producer's
   wake-only callback and existing notification receiver remain the only credit
   wait path.
2. Wait for the original authoritative Ready predicate. Submit exactly ONE
   accepted checked Open. Independently consume `Observer::next` through Trace;
   do not use `Observer::exact`, which sends ACKs. Compare exact submitted/received
   bytes, member/channel/label and checked ABI decode. Derive held start/end from
   the actual Event start plus encoded byte length. No runtime28 is selected.
   Require canonical `Open { offset: 0 }`, remote Pending(end,start), capacity0.
   Save only-submitted-open.bin and held-before-fin.json, including actual
   candidate capacity and admitted record allowance.
3. On that SAME control stream send one actual empty Vec+finish. Persist the
   actual channel receipt as finish-before-peer-event.json and require exactly
   one successful Finish with pending0, without any Write/WriteError. Then require
   the actual peer Lost code to decode as typed Reason::Closed, before local
   cancellation. Require unchanged Pending(end,start), capacity0 and canonical
   Open. Persist actual peer event and held-after-peer-close.json.
4. Stop/join the first Observer under the ORIGINAL deadline; drain and retain
   its queued events and remaining receipts. Require no extra Record, still-held
   cursors/capacity0, and exactly one successful empty FIN across all receipts.
   A client join is not a server connection-removal receipt.
5. Make ONE new authorized subscription using the SAME owner, scope, metadata,
   generation and Endpoint address. The existing Client subscribes at wire
   origin0. The held first Event must actually have start0; assert that identity
   rather than add a cursor API or choose an encoded length. Save the attempt
   before starting it. Require the first actual replay Event to have the exact
   held start and bytes. Decode independently to the same canonical Open and
   require Pending(end,start)/capacity0 before ACK. No producer resubmission or
   fresh Endpoint is used to manufacture replay.
6. Send ONE actual application Ack(end). Reuse Worker::ready's authoritative
   state predicate plus notification wait; require Ready(end,end), accepted
   unchanged and returned allowance exactly the admitted record capacity. Save
   consumed interval end-start and actual-ack-credit.json. This establishes the
   application interval release, not allocator/kernel reclamation or transport
   durability.
7. Stop/join the replay Observer; retain its queued events and require no new
   Record. Require equal end/end cursors with Ready or Pending: normal observer
   disconnect may turn Ready into Waiting/Pending, without canonical completion.
   Explicit owner abort must then yield typed remote Aborted(end,end), capacity0,
   and finish Err(Closed). Submit a valid owned copy of the SAME Open encoding
   and require Rejected::Closed returning identical allocation, capacity and
   bytes; keep both the original and returned bytes. This uses the in-budget
   Open, rather than increasing admission for a new Data record. Canonical state
   remains the actually decoded Open; no canonical abort/death record is invented.
8. Existing unconditional cleanup requests observer stop, owner abort, typed
   finish-after-abort rejection and both joins even on failure. It records new
   case receipt/event tails before dropping resources. Preserve endpoint result,
   teardown failure and actual FD before/after counts; require equality (4→4 is
   the prior runner observation, not an assumed result). Cleanup has the existing
   absolute cleanup deadline, not a restarted budget. Whole population/kernel
   return and writer/death attribution remain Unknown.

## Source obligations and known admission dependency

At4a, `remote/src/managed.rs` Service::readable processes controls before End
refuses Reason::Closed. `disconnect` converts AwaitingAck into Record with the
same start/bytes and resets written0. `subscribe` accepts that retained start;
`consumed` advances only a complete exact interval into Ready when Ending is Open.
Control::abort independently revokes and requests owned driver stop. Canonical
StreamState::Open and remote Pending/Ready/Aborted are distinct checked types.

The one new subscription is NOT guaranteed by those source observations.
`remote/src/net.rs` Io::close only calls QUIC close; bounded receive rejects a new
connection while the driver map is at its existing limit1, and pump removes the
old entry only on `is_drained()`. Peer Closed/local join do not establish that
removal. [RFC9000 §10.2](https://www.rfc-editor.org/rfc/rfc9000.html#section-10.2)
likewise distinguishes application close from retained closing/draining state.
This is an explicit source-level dependency, not a newly reproduced product bug.

If the single authorized attempt fails at admission, preserve its actual Event,
first-phase receipts/held state and unconditional cleanup; report replay/ACK
credit coverage UNREACHED. Do not retry, sleep, poll, reset a deadline, raise a
capacity, replace the Endpoint or weaken the exact replay assertion. A new
authoritative drain seam would require a separate concrete ROOT review; none is
implemented here. The separately assigned kernel PID reset defect is unrelated.

Alternatives: ACK+FIN consumes the held interval and exercises a different edge;
Cancel bypasses empty-pending finish; an independently reimplemented client or a
new producer/Endpoint cannot prove replay of this retained object. A timed delay
does not establish connection retirement. The chosen preparation keeps the
existing checked path and exposes an actual failure rather than repairing it
from source suspicion.

## Bounds, preservation and review gate

Proposed private Fedora8fadc scope
`/home/adalundhe/managed-after-open-fin-4a-v1/{source,target,cargo-home,zig-global-cache,zig-local-cache,runtime-cache,artifacts,records}`.
NOT created or admitted. Linux2CPU/4GiB/24GiB, cargo-j4, normal fullcaller build
≤3600s, exactly one native case with phase240000ms/cleanup15000ms/direct255s.
All waits share the original case start. Future exact argv/environment are in
[future-case.json](future-case.json), an unexecuted descriptor, not a wrapper.
Headroom remains mandatory; no active resources may be dropped to fit admission.

No VM/mutation/allocator/held-helper/cotenant effects, hostroot/security change,
children/goals/wrappers/main merge, profiling/benchmarking/audit or combined sweep.
No local build or target/cache is active. Mac floor30GiB and
`/tmp/g121-mac.lock` apply to local tooling. Frozen4a/d8,82019/4ef and2c source,
binaries, failures and raw evidence remain unchanged. Historical continuation
locators are [continuation-locators.json](continuation-locators.json); ROOT has
not reviewed those1513 payloads+manifest1514 total members. No archive was
regenerated, extracted or relabelled for this preparation.

Before build/actor: ROOT must review this exact source diff, unchanged shared
interface, one-attempt observation order and cleanup contract. After approval,
freeze exact signed source/leaf/shared hashes, use fresh admitted private paths,
compile the full caller and retain actual raw outputs. No previous binary is
evidence for this new source. Fullcaller Clippy/pure/native results, ROOT evidence
review/integration, supported targets and any later VM/mutation/fault gates are
unrun/owed; this preparation does not release the lane or close management gaps.

## Actual preparation checks

[preparation-checks.json](preparation-checks.json) retains commands/results:
development policy0, locked canonical rustfmt check0, and source whitespace0.
Host is Darwin arm64, free179999281152B before those checks. Static source
readback found16 original terminal assertion sequences unchanged after only
the explicit input control/errno binding normalization; original three test
bodies, empty receipt checker and ready wait are byte-identical.

The full staged whitespace check exits2 on eight context-only blank lines in
the exact generated native-caller.patch. It is retained unchanged; the check
excluding ONLY that raw patch exits0. [staged-checks.json](staged-checks.json)
retains the complete warning output. There is no whole-diff-clean claim or
general whitespace exemption. No compiler, pure test or actor ran.

## R01–R16 preparation record

| Rule | Applicable evidence and current limit |
| --- | --- |
| R01 | Tests may assert; only the native integration leaf changes. Production/ABI untouched. Format parsing is not compilation or denied-lint proof; new checks unrun. |
| R02 | Exact existing Client/Service/Shared/Driver/ABI source at4a, RFC9000 close states and alternatives above. No broader architecture choice or live defect is asserted. |
| R03 | Held cursor/allowance from actual encoded record; native origin0 and pending0 are checked protocol/test invariants. Existing TLS/datagram bounds and explicit finite caller inputs; no production literals. |
| R04 | Explicit AfterOpen versus original Terminal selections; canonical Open, typed peer Closed, Pending/Ready interval and owner Aborted/Closed remain separate. Live transitions unverified. |
| R05 | Same one Endpoint/worker, Client/Observer and pending/flush/writable path; notification plus authoritative Ready check. Same receipt receiver retained solely for unwind cleanup; no extra queue/cache/poll or lock changes. |
| R06 | One focused native method and held-state assertion; original terminal entrypoints/assertions retained. No generic transport/test framework. |
| R07 | Comments explain automatic-ACK avoidance and origin matching; source/replay/drain uncertainty explicit. No fabricated errno/death/metadata facts. |
| R08 | No resource/speed/comparative claim; old4a success is not this source's gate. |
| R09 | Coverage edge only, no product fix from source suspicion. Known replacement admission issue is not newly reproduced by this unrun draft. |
| R10 | Exact source patch/hashes, immutable old archive locators, proposed raw records and failure cleanup. New actual before/after/mutation results absent. |
| R11 | Future native Linux amd64 only; no new host/libc/VM/seven-target pass. Mac ARM64 only and paused WinARM untouched. |
| R12 | Reuses existing shared parser, transport and identity; sequential same Observer constructor, no duplicate OS adapter/installed binary. |
| R13 | One accepted Open, one empty FIN, one authorized replay, one real ACK and one rejected owned Open. No retries/traffic inflation or capacity change. |
| R14 | No audit/profile/benchmark; allocation/fault/parent/kernel/population/durability costs unknown. Pointer equality only checks owned return. |
| R15 | Preparation only until exact ROOT review; no actor/build/test now, children/goals/main merge or stale objective closure. Signed named paths with required attribution. |
| R16 | All prior source/raw/frozen artifacts/refs preserved, no archive or cache deletion. Mac floor observed before locked formatting; future runner admission still required. |
