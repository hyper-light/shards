# Actual bounded KVM broker case

One fresh-process actual case **passed0** in r78 on Fedora8fadc, using production
`3b1967838b140466ff3367637541492a77be0459`, unchanged leaf SHA256
`74d459d44f11f5bbe4dfe46611aa35c7e276c150001c7d6f198456b0adeabcaf`,
and only the approved three-line child registration in the disposable source.
Signed compiled checkpoint `3ead767611788ac086859c55db861fb9d54073d6`
was pushed before the native case. No production fix or output rewriting.

Actual native caller returned0, no outer timeout/signals; libtest reported exactly
one passed, zero failed and zero ignored. Its13.19s is an operational invocation
duration, not a latency distribution or performance comparison. The exact command
and final caller payload are in `native.submitted.sh`.

The real controlled testguest reported its own helper identity, actual program
success, TypeError,300ms timeout, a different session's ineffective cancellation,
the original session's cancellation, abandonment and successful later work.
“Stranger” is another MCP session of the same tenant, not a cross-principal attack.
The driver reported its own identity and reverse-direction send capability, and
its real inbox was empty after the owner released the private host gate following
helper results and main exec. Both main execs returned exact `main-progress\n`
and `main-error\n`, status0. The recipient also filters request kinds, so this is
not a sender-only guard-removal proof or a kernel packet-absence claim.

Authenticated generation1 captures had these actual receipt cuts:

| Member/channel | Payload bytes | Wire ACK enqueued | Actual ABI state |
| --- | ---: | ---: | --- |
| Helper1/fd1 | 948 | 1296 | Open offset948 |
| Helper1/fd2 | 0 | 28 | Open offset0 |
| Driver2/fd1 | 488 | 758 | Open offset488 |
| Driver2/fd2 | 0 | 30 | Open offset0 |

These are actual QUIC/Record/StreamState effects, including nonzero Open wire
records for empty stderr. No canonical End was observed. The actors remain live
after their programs finish; later owner stop recorded all four channels aborted,
tail unknown, with accepted=consumed equal to the listed wire offsets. Empty
stderr is only the observed cut; neither future-zero nor full EOF is inferred.
The envelope identifies tenant/channel. Controlled-client RPC output is not a
new per-execution supervisor receipt or durable pre-effect audit.

The actual guest process snapshot contained two `confined-ready`, two
`shards-server`, two `shards-sandbox`, and zero `shards-program` rows after both
markers and main progress. Parent owner stop, every observer join and FD comparison
returned success; host FDs were8→8 with the gate listener retained through that
comparison. A subsequent owned cwd/executable/argv `/proc` snapshot matched no
remaining host processes. These point observations do not prove all kernel
objects, population/thread histories, wait statuses, peaks or charges returned.
The gate's Drop close return remains unobserved.

## Build, interruption and retained artifacts

Host: Linux6.19.10-300.fc44.x86_64 GNU, Rust1.98.1
`48a229cea`, zig0.16.0. r70 normal carried VM/net/init/server and managed-output
test compilation passed0 in14m29s. r74 scoped Clippy passed0 in4m37s, then the
runner connection restarted during static guest compilation. Its final exit is
**unknown**, not passed. Retained original guest log SHA256:
`2c79aee6132fe56800205f241ba271f30c0954dc7f862a63393a02bb55047501`.
No guest exit receipt existed. A fresh kernel process snapshot found no matching
owned build cwd/executable/argv; r77 resumed only that compilation into new log
names and passed0 in42.75s. No passed lint was rerun. Original logs remain intact.

All jobs used fresh Kiln admission. r78 received2CPU/4GiB/24GiB with26 idle cores
and43GB free reported by the guard; this is admission, not a hard cgroup budget
or measured process peak. Case180000ms, hostcall120000ms, cleanup15000ms reserved
inside that same case, capture65536bytes/channel, program timeout300ms, cancellation
work1500ms, outer240000ms plus one15000ms reap bound, provider outer300s; case count1.
Static preparation used the remaining original3600s compilation budget.
Read-only monitoring requests refused at0 idle cores did not execute or bypass
the guard. No native repetition, chaos, benchmark, profile or VM sweep occurred.

r70 isolated source/Cargo home/target but left Zig's default cache location
unchanged: possible shared per-user Zig-cache writes remain unverified, as
reported and acknowledged by root. No data was deleted; root instructed preserving
the active build. Subsequent compilation and native runtime caches were explicitly
private. Root's protected named Cargo clone/target and paused WinARM were untouched.
Afterward the owned target was2.6GiB, source596MiB, Cargo home149MiB, local Zig
cache20MiB, evidence38MiB including retained runtime helper executables. Mac free
disk was59GiB at closing; no heavy local build or new local full copy.

Selected exact artifacts (all SHA256):

| Artifact | Bytes | SHA256 |
| --- | ---: | --- |
| Test binary | 60434760 | `e22757b0dde1b05a41bb6a2b66e516fbabd07b350b965412e9a1a250ce7f4d1e` |
| Main CLI | 270558360 | `d70b1c19ecc85b8255f81fae422a43eabbb33f1715d7e1d730eb76ac33a25823` |
| Actual static testguest | 3673888 | `eb4e1c16c05f1822df73d12fabbc923f76505bf352dc760d6983e9657767bf3e` |
| Pinned guest kernel | 27758808 | `48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7` |
| Actual runtime-cache VM helper | 30934384 | `6ff1caa4e6162f1cf63df1bf10058ea4ceda75ea38e207b41d982fd9988869e9` |
| Actual runtime-cache network helper | 7908816 | `b675cc382f55dd41df39b3e91310a0eb3108258470bc4018d8c6ca31de79779f` |
| Final strict caller | — | `254948b2a7bc64187df978edefeb49935b5e8fd9a4be70f4e719934d677777fb` |

`artifacts.json` retains both build-output contexts produced by compilation and
Clippy; not every listed output is claimed to be used. The actual private runtime
cache hashes match the normal carried helpers above. Source/test/parent/caller
association, raw bytes, canonical records, inputs, metadata, main results, process
rows, owner cleanup, all logs and interruption snapshot are in
[`raw-linux.tar.gz`](raw-linux.tar.gz),18388bytes, SHA256
`4ee4e1a549824a77ab733896ac7113bcb64becc2841189059ee7972b1a072f28`.
Its77 regular members passed safe-path/member checks and all76 manifest entry
sizes/hashes verified locally. `raw-manifest.json` and `decoded-selection.json`
provide readable indexes/selections; original bytes remain in the archive.
Original complete binaries/caches remain in the owned Fedora scope.

## Historical whitespace correction and owed work

Root's actual diff-check rejected the original `register.patch` raw blank-context
spaces. A local reproduction `git show --format= --check 3ead7676` returned2 at
lines7/13 (including blank EOF). My earlier unstaged diff-check0 before adding NEW
paths did not establish their cleanliness. This scope error is retained, not a
passing raw-patch check. Root preserved original bytes/digests in gzip plus its
storage record and removed the redundant plain copy on composed source
`88a4ef336` after a transparently recorded initial staged-removal refusal.
The private original3ead source/submitted scripts are historical and unchanged;
root's storage change does not alter the Rust leaf, registration semantics or
production bytes. Do not re-add its plain raw artifact to the composed source.

R01: assertions are test-only; no production edits; targeted Clippy0, no universal
panic-freedom proof. R02/R03: primary/alternative research and explicit test limits
in README; no production constants. R04/R05: actual typed ABI/driver, one owner,
event/canonical readiness and original deadlines; frozen guest blocking/delay
limits disclosed. R06/R07/R13: one focused leaf with local receipt functions;
no new production abstraction/manager. R08/R14: no profiling/comparisons; 4–5×
target remains root-owned and unproven. R09/R10: real case/raw witnesses retained,
no production fix inferred; legacy r751 failure remains separate/open. R11/R12:
Linux amd64 KVM plus musl guest only; other host/libc/arch/VM and exact combined
seven-target gates remain root-owed. R15: scoped signed implementation and actual
adversarial case; root audits, integrates, reviews evidence/mutations and merges.
R16: owned paths/headroom/interruption/caches handled as above, no destructive Git,
host privilege/security/crypto change or paused VM action.

Still owed: root combined revision gates, meaningful single-guard kernel-effect
mutations, concurrent/pressure/failure campaigns, full population/kernel/aggregate
resource accounting, broker-owned per-execution provenance and required durable
pre-effect audit, adapters/native executors, full managed-plan obligations and
performance/compatibility evidence. This one functional case closes none of those
by implication.
