# Native turn: method, evidence scope and current limits

Tested source b84005a214b58e11a1ee80671ef2d4a7cec062bb; current policy
b57/df2acf as recorded in [README](README.md). Implementer plus assigned
adversarial relay tester. Exact pre-build closure is
[source-identities.json](source-identities.json). The first source-only8cd
checkpoint carried no compilation or native pass claim. The actual results below
belong to the stated final source; its two native invocations were each run once.

## Actual GNU and musl outcome

r164 COMPLETE0, private Fedora44/Linux6.19.10 runner
8fadc1ec213332e16cb0d915a3fb913f, `~/relay-turn-pressure-2300-v5`.
Policy/format, module check, strict Clippy with all targets/current lint flags,
native builds and one exact native case per GNU/musl all return0. This is the
managed-network source projection, not the complete normal init/testguest/host
or a real guest. [environment.json](environment.json) pins compiler/kernel/libc.

Both cases use the original supplied1MiB prefill envelope, seed00ff0d0a01020304,
request13, slots3/5 and queue inputs4096. Actual option readbacks are8192; actual
prefill accepted8064 then native WouldBlock/errno11. This is an observation, not
a queue capacity guarantee. No pressure-event attempt failed and was repeated.
Four earlier formatting/compilation/lint failures reached zero native cases.

| Actual stage, both libcs | Result |
| --- | --- |
| Independently accepted prefill |8064 exact seed bytes, then real send-WouldBlock |
| SAME Direction's initial real source recv |read3/pending3, suffix524551, written0/sent0, Half::Reading |
| One blocked turn |Ok(false), write-WouldBlock0→1; read3/written0/pending3/sent0/half/extra/suffix unchanged |
| Independent prefill drain |8064 exact bytes, separate from relay counters |
| Six positive recovery turns, no repeated WouldBlock |read/written13, pending0/extra0, WriteClosed(Eof) |
| Actual destination receipt and nonempty EOF probe |8077 bytes = prefill8064+request13 exactly; EOF read0 |

The prefill accepted/drained SHA256 is
`eeddbf048773b11549f9041a2ef7889e4628f9d0f7cf613a53ca2fd97824f27b`;
request13 is `60e5b59d5737f888a971ab693cfc73c0abe9dd689780807efcdd13790837cda8`;
actual destination8077 is
`2785c4c0668e9dbdfa8c9ac142beda0e9835d566e77ee115c5a7e83f7af558e5`.
Original remaining time is29987941656ns musl/29988151655ns GNU, with no renewed
deadline or extra allowance. Native childFD4→4 is recorded before assertions,
additional owned workers0; each parentFD4→4, held child pidfd exit readiness,
actual wait/reaping and exact before/after binary hashes. All11 final phase
children have exit/reap witnesses, no forced cleanup; outer callerFD9→9.
Whole-system/kernel resource return remains Unknown, hard caps Unverified.

Final source closure hashes match before/after; tracked git diff0, only the
declared untracked source projection root. Frozen binaries remain outside target:

| Target | SHA256 |
| --- | --- |
| GNU | `0f3634c2ab403d01bdb77c4258e809b7b9bb985c1a9233a99f12319d332b36fc` |
| static-musl | `d8a389e777daea7c1126c6837bb243e17983a72cdeddb973c67c3f9a8ff03fde` |

Final test file SHA256
`6e3bad23c1c3f22efd3be2a3070a41aae62c3b0bb08708477d9bdcb307cc92a2`.
Shared file with the cfg(test) declaration hashes94bf80f0…; stripping only that
declaration recovers the original716-line083bce5e… production bytes exactly.
No visibility, production state/queue/hook, pump or old fixture was changed.

## Method

The fixture calls the actual private Direction, not a model or replacement
pump. Its ordinary initialization matches the existing pump. The first recv
sets read/pending/suffix from native bytes. The next turn must return Ok(false)
with write-WouldBlock incremented once and all suffix/cursor/read/write/half/
extra fields unchanged. No field is assigned to synthesize that condition.

Prefill is accepted by an independent native caller before those turns; it is
never charged to the relay's read/written counters. The held destination reads
and records that prefix separately. Positive recovery turns reuse the SAME
Direction/storage/progress/control and are followed by actual destination reads
of each newly written range. Recovery stops at its first missing progress or
WouldBlock, rather than becoming another readiness loop. Actual source EOF and
native write shutdown lead to an independent nonempty destination EOF probe.
The final destination must equal prefill+payload exactly; relay counters describe
only payload. The case creates no additional worker threads. The native launcher's
held pidfd, actual wait/reap, descriptor and binary receipts remain required.

The shared file's only difference is a cfg(test) module declaration. All716
reviewed lines still hash to083bce5e… after removing it. The production engine,
current TCP/parser/Worker code and5a4/2300 fixture/evidence are untouched. The
earlier db4 pressure run remains19/20 plus one separate unchanged passing repeat.
Success here cannot reclassify that original scheduling-sensitive gate.

## R01–R16 scope account

| Rule | Applicable proof or limit |
| --- | --- |
| R01 | New assertions/allocation helpers are cfg(test) only. Strict GNU/musl lint0; no production panic-proof claim. |
| R02 | Implements the exact reviewed f6 second option, based on actual missing-pressure-witness evidence, at the existing native turn seam. No production architecture is selected. |
| R03 | Prior explicit caller1MiB prefill envelope/seed, request13, slots3/5, queues4096, case30s/parent45s. No queue-capacity guarantee or tuning based on observed counts. |
| R04 | Original Half/SourceEnd/Progress variants remain authoritative. Test failure distinguishes I/O, typed Turn fault and missing invariant/event. Original at is retained. |
| R05 | No production hook/notification, polling interval/cache/manager. Prefill continues only after actual positive finite writes; all demanded WouldBlock events stop that phase. |
| R06 | Separate private test file and focused decoder; existing production lines remain exact. This is scoped fixture work, not an audit. |
| R07 | Safety comments cover owned FDs/slices/native options; comments explain phase ownership and the source of pending. |
| R08 | Functional event/byte proof only, no performance result or4–5× claim. |
| R09 | No production fix. The previous actual missed pressure witness is retained, and current event absence must remain failure. |
| R10 | Exact commands/source closure, raw stdout/stderr, bytes, snapshots, preparation failures and cleanup are retained/verified below. No mutation is authorized in this slice. |
| R11 | GNU/musl x86_64 native checks only in this scope; complete normal callers/seven targets and guest hypervisors are ROOT gates. macOS ARM64 only. |
| R12 | Calls the same existing private Direction/Deadline, with no visibility/trait/queue/state/pump change or extra installed deliverable. |
| R13 | One finite socketpair pressure case; no generic readiness or forwarding framework. |
| R14 | No audit/profile/benchmark. Kernel/parent accounting, allocation/fault/copy costs, hard caps and leak-freedom remain unverified. |
| R15 | Private admitted Linux2CPU/2GiB/4GiB, <=600s/-j2, exact source closure before build, one native case per libc and owned-child witnesses. No VM/children/main merge/held campaign. |
| R16 | No protected path/cache writes/deletion/warming. Original sources/evidence/frozen outputs preserved; no local compilation/target/cache, Mac remains above30GiB. |

This test does not run the full scheduler, simultaneous directions, TCP, VSOCK,
interception, grants or a real-VM resource boundary. It cannot close mandatory
route coverage, producer/parent/kernel budgets or management P0–P13/G25. ROOT's
central680 candidate and original015 VSOCK results retain their separate source
and authority; no current candidate pass is inferred here.

## Retained preparation failures

| Run / immutable source | Actual outcome before native actors |
| --- | --- |
| r159 /8cd7a3a6c, privatev1 | Complete source closure and policy0; format1. Formatter-only packet used default width while actual repository rustfmt.toml requires110. Native cases0. |
| r161 /bb09b782f, privatev2 | Closure/policy/format/module-musl-check0; cfg(test) compilation E0277 at two WriteZero conversions. Native cases0. |
| r162 /d914908a9, privatev3 | Closure/policy/format/module-musl-check0; strict cfg(test) lint101: array chunk API, assertions in Result helpers and suspicious map/count. Native cases0. |
| r163 /ce05b83cb, privatev4 | Closure/policy0; one returned-error formatting hunk after lint fixes, format1. Native cases0. |

The formatting correction matches the actual workspace formatter's exact output
hash; its optional trailing-comma edits also made an earlier local whitespace-
stripping equality helper fail. That helper was not semantic proof and was
replaced by exact formatter-output/source hash equality. The local helper's
diagnostic remains in the session transcript; it is not claimed to be a retained
original Linux log file. Source/cargo/byte/queue/time inputs were not tuned.

Typed Failure::Io wrapping fixes only the actual compilation diagnostics.
Result helpers now return typed errors, hex decoding uses the requested current
array-chunk interface, and the own-descriptor count validates directory entries.
No lint allowance or production change was added. All failed private scopes and
their source/target/logs stay intact. Every one of these preparation phases had
actual pidfd exit readiness/reaping, no forced cleanup and callerFD9→9. None
retries a missing native event, since none reached a native case.

## Durable raw evidence

[CHECKS.json](CHECKS.json) retains every native row and preparation outcome;
the original byte rows are not replaced by their compact hashes.
[raw-linux.tar.gz](raw-linux.tar.gz),61283 bytes, SHA256
`2c31d80e184fdd1f19682450aa61606936391c06fec59f231b3d5a4f2ae5c3fe`.
All83 unique safe regular members were verified locally against the manifest;
82 payload files match [raw-manifest.json](raw-manifest.json) by exact size/SHA.
Raw includes all four failed Linux records, both formatter packets, final
stdout/stderr/observed bytes/results, before-build/after-source receipts,
workspace lint/config/locks, long-type diagnostics and final test/driver bytes.
No original Linux failure, encoding, newline or byte record was normalized.

Exact executed commands are in [commands](commands), including all five campaign
versions and [package-raw.sh](commands/package-raw.sh). Existing record roots
refuse overwrite; future authorized reproduction requires a new scope and an
explicit checkout of immutable tested source after the branch tip advances.
All private scopes v1–v5 and formatter-v1/v2, including their source/targets and
final frozen binaries, remain intact. No source/evidence/cache was deleted or
warmed. No local compilation/target/cache, ROOT path, paused VM, held campaign
or main branch was touched. This is a tested seam checkpoint, not lane or
management-layer completion. The previous db4 pressure19/20 gate is unchanged.
