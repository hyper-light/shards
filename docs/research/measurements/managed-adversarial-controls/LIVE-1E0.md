# Actual native terminal controls at1e0

Signed source `1e0bd4785e9ba458cef121abcdb2728ae1e5cb2a` passed8 actual native
Endpoint cases: DuplicateAck and Resubscribe, each stdout/stderr with supplied
errno5/i32::MAX. Every direct process executed exactly1 selected ignored test,
0 failed/ignored, with a fresh run/scope/endpoint. Full normal caller compiled0,
`fresh:false`, fixture SHA-256
`7810e1aa8964841f1fc5c01ff5f8a468347519577c0a0ae2c32264d34135d123`.
The full caller's7 pure tests passed/0 failed/3 explicitly ignored; scoped full
caller Clippy-Dwarnings0. Earlier VM results retain their9e7/eed0 source identity.

| Actual witness, both supplied errnos | stdout | stderr |
| --- | --- | --- |
| Received canonical Aborted offset |26|23|
| Finished credit after canonical ACK and actual transport End |90/90|87/87|
| Duplicate ACK or valid second Subscribe |typed Protocol; supplied errno/offset and exact Finished cursor preserved|same|
| Subscribe raw writes |55 accepted/3 pending, then3/0|52 accepted/6 pending, then6/0|
| Postterminal owned payload |Closed returns exact42 bytes|Closed returns exact39 bytes|
| Observer retirement before owner abort |join succeeds;0 future queued records; owner still Finished90/90|same, Finished87/87|
| Owner abort |Aborted90/90; finish-after-abort Err(Closed)|Aborted87/87; finish-after-abort Err(Closed)|
| Final cleanup |endpoint/observer joins; FD4→4|same|

Actual canonical/control bytes, typed close and exact returned-payload hashes
are retained. The cause is owner-supplied, never presented as a kernel-observed
guest error. Canonical StreamState::Aborted is separate from remote Finished
and remote owner-aborted credit. No daemon/VM metadata or lifecycle result is
inferred from these native records. Observer stop/join does not erase the known
remote Finished state; owner abort still retires the actual endpoint driver.

Subscribe uses the approved original RawControl seam and bounded real encoder;
receipts are drained after local observer join and checked by the existing shared
accountant. Actual partial writes occurred in all4 Subscribe cases. Local writes
are not backend application ACKs. No FIN/reconnect/retry/alternate endpoint,
capacity increase or new transport was introduced. The original duplicate test
and the new test share one focused native execution/cleanup method.

Full test names are under
`tenant_exit::adversarial_controls::native_terminal::`:
`canonical_aborted_duplicate_ack_preserves_errno_and_terminal_credit` and
`canonical_aborted_resubscribe_preserves_errno_and_terminal_credit`.
Direct callers select exactly one with `--ignored --exact --test-threads=1`;
explicit channel/errno/fresh UUID/generation1/source/new records, phase60000ms,
cleanup30000ms, outer100000ms and direct100s timeout apply. Eight-case operation
outer1100s. No case failed or was retried.

Fedora8fadc1ec213332e16cb0d915a3fb913f, Linux amd64 GNU, Rust/Cargo1.98.1 and
same owned normal helper/toolchain bytes as earlier records. This batch starts
native authenticated loopback endpoints only, no VM/guest/daemon. Private
`/home/adalundhe/managed-adversarial-controls-live-v1/` source/target/cache/
artifacts/records, admitted2CPU/4GiB/16GiB build/check/case and1CPU/1GiB/2GiB
collection. No protected ROOT clone/cache, local build or paused WinARM use.

Exact full build, pure and Clippy command arrays/environments, source hashes,
actual unchanged parent diff, raw stdout/stderr/exits, per-case wire/control
bytes, cursors, typed causes and cleanup are in `live-native-subscribe-1e0-v1/`.
The source base remains frozen cbd392 plus owned changes/original approved
parent patches17.../41.... ROOT's later main/common move is not rebased into this
source or granted an inherited pass. Frozen57a8/eed0/9e7/1e0 binaries/refs remain
preserved. Submitted operational text is in `drivers/native-subscribe-*.txt`.

Archive `live-native-subscribe-1e0-v1.tar.gz`:49870 bytes, SHA-256
`4c208ec6a55e8d941af1386e99818dfabc65dc0b16516c105877c12ded728565`.
All206 unique safe regular members and individual hashes were verified locally
before extraction. Remote originals remain. Collection exited0; no native
test expectation, source or record was changed during collection.

Assigned adversarial implementer01a1c5d2ee5871a2a9a9cdc9f56793f0;
coordinator01a148884a4770fdaf0111f3f792a248. Full current main33df95b08 policy/
plan/handoff read; rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.

| Rule | Proof / remaining limit |
| --- | --- |
| R01 | Test-only source, actual full caller and Clippy0; production panic behavior unchanged. |
| R02 | Existing Endpoint/Observer/Service contracts, primary references and alternatives retained in NATIVE-SUBSCRIBE/TERMINAL-CONTRACT/POST-END-SUBSCRIBE;8 actual outcomes. |
| R03 | Actual encoded record Vec capacities derive native allowance; real CONTROL_BYTES and observed cursor, explicit positive caller bounds. No production constants. |
| R04 | Explicit control selection; canonical errno/offset, remote Finished/Aborted and cursors, transport End and owner stop have separate actual witnesses. No resurrection. |
| R05 | Same authenticated driver/pending/partial-write handling and wake-only coalesced callback; Control predicate authoritative. No new wait/cache/poll/manager/thread. |
| R06 | Two real native selections share the existing owner/cleanup path; no generic framework. |
| R07 | Comments/evidence preserve supplied-cause, local-receipt and lifecycle uncertainty distinctions. |
| R08 | No speed/comparative or4–5× claim. |
| R09 | No product bug/fix; exact native guard outcomes observed, no fake transport. |
| R10 | Exact source/build/artifact/raw/case/cleanup records and verified archive. Meaningful product guard mutations/restoration remain ROOT-owed. |
| R11 | Actual Linux amd64 GNU native only; other OS/libc/VM/matrix unverified here. macOS arm64 only; WinARM untouched. |
| R12 | Existing shared ABI/transport/platform; no OS adapter or installed binary. |
| R13 | Finite one-case/process/owned endpoint; cleanup remains unconditional and strict. |
| R14 | No profiling, allocation/fault/CPU/parent/kernel cost or leak-free claim. FD equality is a finite observation. |
| R15 | Assigned private native campaign; signed named paths, no children/goals/profiles/main merge. ROOT owns audit/integration/benchmarks. |
| R16 | Private admitted remote scopes, Mac floor retained, no local target/build or deletion; frozen sources/binaries/raw originals preserved. |

Policy script, canonical rustfmt, unchanged-source and named prose/whitespace
checks pass. Original raw stdout/patch whitespace is retained with a separate
diagnostic; no policy waiver. Owed ROOT: current common-file integration/matrix,
guard mutations/restoration, actual daemon canonical-Aborted cause/metadata,
publication/fault order, whole population/kernel return and plan acceptance.
No durability/performance/full closure claim. Next selected B boundary is a
complete consumed header with zero cursor-body bytes followed by actual FIN,
after an observed Open; the existing4-byte/12-byte truncated cases did not
execute that5-byte header/body cut. The same bounded raw seam is sufficient.
Rejected helper-control/worker-result allocation campaigns remain held.
