# Enumerated queue at the frozen current caller

Tested source remains `2c57ac6d7c410b90fc61a6f186a506eedde840ee`, base28e147c4,
fixture SHA `6217535e1bdfc54e81334318d68f4359c6d2648234f81c2e67a6156c4f614a69`.
No Rust source, helper, guest, binary, capacity or expected assertion changed
after the signed requested proof50c6223a. Main advanced to
`a920c85b5c0c9d40aafc4f4cce32432c7c0aed79` during this campaign. Per instruction
there was no fetch/rebase/rebuild; these results are not new-main integration gates.

**26 additional actual KVM cases and7 additional native cases passed**, each
exactly1 executed test with0 failures/ignored tests. No case was retried. Including
the requested proof, this exact source has28 distinct VM runs and8 distinct
native scopes. The previously passed7 pure checks/full scoped Clippy/full normal
caller and matching guest builds retain the same source/binary association;
they were not needlessly repeated for evidence-only changes.

| Continuation | Actual cases and witnesses |
| --- | --- |
| A6 |Partial/replayed/forward application ACK ×stdout/stderr after actual records. Replay ACKed Open and observed next Data before replay. Typed Protocol1, held accepted/consumed28/0 or58/28, aborted/unknown on stop; no future queued target record.|
| B12 |Unknown/wrong-known tag, overdeclared/wrong ACK length, truncated header/ACK ×both channels. First four kinds Protocol1; truncated input+actual FIN Closed4. Real4/5/12-byte writes, zero pending; requested FIN receipts success. No allocation of declared huge body by fixture.|
| C8 |Duplicate/forward/ACK+FIN/valid second Subscribe after actual canonical End, its ACK and transport End ×both channels. Protocol1; member1 remains ended/complete102/102 stdout and99/99 stderr after refusal and owner stop. Both Subscribe controls made actual43+15-byte writes.|
| Native7 |Duplicate ACK ×both channels/errno5 and i32::MAX, plus remaining Subscribe channel/cause combinations. Together with requested native case, both controls cover both channels/causes. Exact supplied canonical Aborted offset/errno, Finished90/90 stdout or87/87 stderr, Protocol1, returned owned42/39-byte payload with Closed. Observer join leaves Finished and0 queued records; owner abort preserves cursors and finish returns Closed. Subscribe writes55+3 stdout or52+6 stderr.|

Every VM case required exact new controlled neighbor fd1/fd2 after the existing
TCP gate and positive main stdout/stderr before/after release. All26 retained
owner stop, inspectfalse0, remove/metadata unlink, observer joins and FD7→7.
Every native case retained both joins and FD4→4. Canonical supplied errno is
not a kernel-observed failure or daemon metadata claim. Typed ACK commands do
not arm the raw receipt sink: C's ACK+FIN request is not presented as a separately
retained local FIN receipt. B's raw FIN and C's raw Subscribe have actual receipts.

The reused release helper's historical label mentions both member1 EOFs; A/B's
precise before-release record shows target refusal and the other member1 channel
End/ACK/EOF. That label is not evidence of normal EOF on the refused target.
Full population/wait status/parent/kernel return, durability and accounting remain
unknown despite successful owned stop and finite FD observations.

## Exact bounded callers and raw storage

Same private Fedora8fadc scope, Linux amd64 GNU/KVM and own experimental kernel,
matching guest301cbb/CLI643cbf from RESULTS.md. Each selected case had original
phase/work240000ms, cleanup15000ms, hard outer255s, new records/run/endpoint;
no polling/retry/deadline reset/replacement connection. Admitted2CPU/4GiB/24GiB,
runtime serial per job. Finite operation bounds: A1600s, B3400s, C2200s,
remaining native1900s. Small collection/transfer1CPU/1GiB/2GiB. Source, frozen
artifacts and caches stay private; no ROOT target/cache, local heavy build,
paused WinARM, protected artifact or held campaign was touched.

`drivers/queue-*.txt` and `collect-continuation.txt` preserve submitted command
text. The archive retains actual per-case command arrays/environments/stdout/
stderr/exits, canonical/control/main bytes, typed events, terminal metadata,
progress and cleanup. The unchanged source/full-build lineage is the earlier
requested archive, not an invented new build.

`continuation-v1.tar.gz`:190427 bytes, SHA-256
`e5e1f5bc3c3a0c60bf1f332f38f5f889608cca3e9bb60dee6d350f8731221aef`.
All1514 unique safe regular members and every individual hash were verified
locally. Original raw bytes stay compressed and unnormalized. Exact original
member-hash index and validated summary are beside the archive; all remote
originals and the earlier requested archive remain preserved.

Shared-client patch stays3939d78f2bf3e175a2c504e859a08a5d8cb2a1d10bf81c3ad868d5f121eee123;
registration stays83132563b3eadea370e89912f641141638ff310af278e25a24d826d4d003f6bc.
All four fixture children remain frozen247 bytes. Before-test source/hash/role
readbacks and ROOT review are retained. Loaded policy was main33df/full R01–R16,
evidence/CLAUDE/plan/handoff; current rule content is unchanged at a920, SHA
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.

| Rule | Proof / remaining limit |
| --- | --- |
| R01 | No code changes; exact previously built/linted source and actual33 continuation passes. Production panic behavior unchanged. |
| R02 | Same reviewed current shared path and primary/alternative record; actual bounded variants now retained. |
| R03 | Observed record cursors, real ABI/parser bounds, explicit finite caller inputs; no production literals or cap change. |
| R04 | Typed Protocol/Closed and canonical End/Aborted, remote Finished/Aborted/cursors and lifecycle remain separate with actual witnesses. |
| R05 | Existing pending/partial-write/finish/event/callback path; actual partial writes, no new polling/cache/manager. |
| R06 | Same focused caller/children and shared implementation; no framework or duplicate parser/observer. |
| R07 | Actual receipt limits, legacy release label and supplied-cause/cleanup uncertainty disclosed. |
| R08 | No performance/comparative/4–5× claim. |
| R09 | No product fix or new bug inference; all enumerated expected outcomes observed live. |
| R10 | Exact raw invocation/source/artifact/progress/cleanup and locally verified original archives. Product mutations/restoration ROOT-owed. |
| R11 | Linux amd64 GNU/native/KVM experimental-kernel scope; current final matrix/other host/libc/VM gates unverified here, macOS arm64 only/WinARM paused. |
| R12 | Same shared ABI/transport/platform/current Client; no OS adapter or installed binary. |
| R13 | One fresh scope/run/endpoint per selected finite process, cleanup invariants unchanged. |
| R14 | No profiles/benchmarks or allocation/fault/CPU/parent/kernel accounting, durability or leak-free claim. |
| R15 | Scoped assigned actual campaigns only, signed named paths, no children/goals/autoplan/main merge. ROOT owns audits/profiles/benchmarks/final integration. No stale objective releases lane. |
| R16 | All sources/evidence/frozen binaries preserved, private admitted scopes and Mac floor, no deletion/local target/build. |

ROOT still owns current-main combined/other-consumer/matrix review, meaningful
product guard mutations/restoration, metadata/fault order, population/kernel
return, performance and full P0–P13/G25 acceptance. The lane remains open. A
next bounded FIN-with-no-additional-control-data boundary was proposed to ROOT;
it is unimplemented and unrun. Any new source scope starts current main with the
required full reread/hash readback. Rejected helper-control, worker-result
allocator and cotenant-theft campaigns remain held.
