# Checked peer-effect journal barrier (partial implementation)

This is an implementer checkpoint for P7/P9 and G03/G13/G19. Production required
persistent audit remains unavailable: no trusted protected-storage producer exists
in current Shards. `RequiredDurable` with `journal: None` still refuses without opening a hand
file or sending to a peer. `server::call` continues its existing `MemoryOnly`
contract. This is not completion of managed effects, durable audit, or the plan.

POLICY_READ: implementer `/root/managed_effect_audit`, base/current policy
`0342b85d9c808e3ced041a6c3ef0e9bf70438272`, rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
AGENTS, development-rules/evidence, CLAUDE, full managed plan/current handoff,
broker research and current combined evidence were read before edits/builds.
No children. ROOT retains audits/profiles/benchmarks/real-VM/cross-lane gates and
main merges. Five proper adversarial lanes and all held campaigns remain separate.

Owned source: `crates/server/src/broker.rs`, its narrow new `broker/audit.rs`, and
this record directory. No init/daemon/ABI/metrics/API/network/shared-fixture edits.
The production-producer seam is proposed in [PRODUCER.md](PRODUCER.md), not enabled
through an environment variable, actor scratch, or an invented storage quota.

## Decision and sources

The existing server supplies the one real authenticated domain principal and
init-held peer descriptors/grants/request ownership. `send`, `answer` and `hand`
continue using that authority and a single nonblocking sequenced-packet send.
There is no new native-exec, writable workspace, provider, delegation or fallback
capability. Payload labels and tool/process names supply no authority.

The journal has one held file writer and mutable reservation ownership, without a
second manager, background queue, mutex, cache, timer or polling. Its fields are
private; only native unit fixtures can currently construct it. A future production
constructor requires the producer proof below. Merely opening a regular file,
preallocating it or passing `sync_data` cannot prove storage/protection authority.

Pinned alternatives inspected before implementation (full source identities and
SHA256 in [sources.json](sources.json)):

- SQLite 3.51.0, `c2aa8e3875907d84540af7624534e326b26b82ee`,
  [`full_fsync`/directory handling](https://github.com/sqlite/sqlite/blob/c2aa8e3875907d84540af7624534e326b26b82ee/src/os_unix.c):
  separates data sync from directory/name durability and has platform adapters.
  Its transactional database/recovery/locking machinery is unnecessary for this
  single synchronous broker append owner. No SQLite transaction guarantee is copied.
- PostgreSQL 18.0, `3d6a828938a5fa0444275d3d2f67b64ec3199eb7`,
  [`issue_xlog_fsync`](https://github.com/postgres/postgres/blob/3d6a828938a5fa0444275d3d2f67b64ec3199eb7/src/backend/access/transam/xlog.c):
  distinguishes WAL write from sync and stops on sync failure. Its multi-process
  WAL manager and panic behavior are inappropriate here; Shards returns typed
  failure and closes this journal's admission.
- [Linux write](https://man7.org/linux/man-pages/man2/write.2.html) and
  [fsync/fdatasync](https://man7.org/linux/man-pages/man2/fsync.2.html):
  short writes and deferred failures require distinct states; file sync does not
  establish the directory entry. This server code runs in Linux guests on every
  supported host; GNU/musl and both guest architectures retain the shared seam.
  POSIX Issue8 fsync page fetch returned HTTP403; it was not used as inspected proof.

The rejected alternatives are best-effort telemetry/TCP acknowledgement (neither
is persistent pre-effect evidence), actor-writable scratch/TLS material directory
(no protected durable capacity), environment/path grants (actor-controlled input
must not mint authority), a global journal cache/worker (no real caller or
admitted capacity), and retrying an ambiguous effect (unsafe).

## State, bounds and metadata

`authorized -> reserved` acquires the complete response/packet/pending-ID space,
intent/terminal encoding buffers and their checked journal byte reservation. The
journal debit derives from the exact counting encoder for actual trusted labels,
maximum native integer representations and every terminal/error shape. There is
no default byte/storage allowance. A rejected reservation opens/sends nothing.
Dropping an unused reservation returns its bytes; a synced intent dropped without
a terminal closes subsequent admission as incomplete.

`execute` appends and `sync_data`s the intent before its file read or peer send.
The record contains only trusted principal/peer, checked operation/request ID,
channel byte ceiling and, for a hand, dev/inode of the held actual file. It omits
payload text, file paths/names/content, JSON extras, credentials and environment.
The hand uses the held `openat2` object even when the actor replaces the pathname;
its existing beneath/no-link/no-magiclink/no-cross-mount, regular-file and byte
checks remain. Writable file contents may still change; there is no snapshot or
content-integrity promise.

The writer performs one native append attempt. Any partial append/error or flush
failure closes this journal and preserves actual stage/accepted/expected bytes
and OS cause. It never repairs/replays an effect. Successful pre-effect sync
permits the existing peer send. The terminal captures completed/local-failed/
uncertain peer outcome, then appends and syncs that observed outcome. A terminal
record says `intent_synced`; it cannot claim its own later flush already happened.
Only the caller receipt after successful terminal sync says `terminal_synced`.
A failed terminal audit after local kernel acceptance preserves that accepted
byte effect and returns `uncertain`, never `no effect` or retry advice.

New production literals are protocol syntax/newline framing, the native signed
write-size representation bound and checked zero-based accounting. Existing
message kind/header/u64 request/u16 filename fields are unchanged. Native test
byte grants/payloads are explicit fixture inputs, not runtime defaults. No
performance/allocation/leak-free or hard storage-entitlement claim is made.

## Validation and owed work

`run-light.py` retains original baseline refusal, candidate broker/held-file/
caller tests, strict light clippy, source hashes and separate stdout/stderr. Its
native admission is Linux amd64, 2CPU/2GiB/4GiB, cargo-j2, at most600s per batch;
this describes scheduling admission, not measured physical/cgroup/kernel peaks.
Only targeted native checks are implementer scope. Results remain pending until
recorded below; a native sync return cannot prove survival of actual power loss.

ROOT still owes production trusted storage/admission/name/protection producer,
checked init transport and exact descriptor syscall grant, real guest actor
FD/path/ptrace-theft protection, real storage-full/partial/flush/power-loss and
owner/death/crash cuts, terminal-after-accepted failure, no-replay/recovery,
meaningful guard-removal forbidden peer effect, cleanup/control progress and the
seven-target/actual supported VM matrix. Generation/revoke/restore/move and
alternate-effect-route closure remain incomplete; no strict semantic coverage is
claimed for opaque/direct routes. Full costs and comparative measurements are
ROOT work, with no4–5x win asserted.

## R01–R16 record

| Rule | Scoped proof and remaining gap |
| --- | --- |
| R01 | Fallible buffers/checked byte reservation, owned file/socket, native error results, no new production panic. Final GNU/musl strict light lint passed; ROOT transitive/OOM audit owed. |
| R02 | Pinned SQLite/PostgreSQL alternatives and OS contracts above; live native data recorded separately. Production storage topology requires ROOT review/proof. |
| R03 | Exact counted metadata/error encoding, checked authority debit, actual admitted input; no invented quota/default/timer. |
| R04 | Explicit journal/reservation/status/stage/failure and existing effect terminal transitions. Durable restart classification/epoch remains open. |
| R05 | One owned synchronous writer at existing effect seam; no cache/queue/lock/poll/manager. Sync disk wait exists and control-progress architecture remains ROOT owed. |
| R06 | Narrow native journal module and one broker reservation/barrier/terminal path. ROOT complexity review owed. |
| R07 | Comments specify held-object/protection and write/sync/name limits, no payload logging. |
| R08 | No benchmark/4–5x claim; ROOT comparable wholepath costs owed. |
| R09 | New requirement, not a claimed production exploit. Original required-audit refusal retained. New response-bound defect reproduced at r11 before correction; exact unknown-versus-uncertain assertion and restored GNU/musl passes retained. |
| R10 | Repeatable finite driver, baseline/candidate source copies/hashes and raw streams retained;197 archive payloads plus manifest transfer-checked. ROOT real forbidden-effect/mutation/crash evidence owed. |
| R11 | Shared Linux guest code; native GNU+static-musl amd64 scoped checks passed at final product hashes. Other Linux libc/arch, seven hosts and KVM/HVF/WHP remain unverified here; paused WinARM untouched; Intel Mac unsupported. |
| R12 | Existing guest broker/held-root/openat2 seams, no host sensor/root/admin/extra installed binary/crypto change. |
| R13 | Only existing granted peer operations and actual required-audit state, no new executor authority; production missing producer remains explicit. |
| R14 | No implementer profiling or performance claim; ROOT allocations/copies/parent/kernel/fault distributions owed. |
| R15 | Implementer/no children/native-light scope only; signed checkpoint/evidence returned for ROOT review, no main merge. |
| R16 | Private source373MiB plus scope measured by driver, no shared target/cache deletion. Mac remained above30GiB; no local build. |

## Current native checkpoint (initial candidate; lint not ready)

Kiln r2 completed on Fedora44/kernel6.19.10 amd64 with2CPU/2GiB/4GiB admission,
cargo-j2 and600s batch bound. Driver reports original mandatory-audit refusal0,
candidate broker0, original held-file0, original Program caller0, strict clippy101,
storage0. Exact counts, diagnostics and source/log archive remain on the private
runner pending admitted retrieval; they are not guessed from exit codes. The
first diagnostic retrieval r4 was refused at0/32 idle CPU, and a later unchanged
read was also refused. No read shell ran on those refusals and no guard was
bypassed. No eligible Linux runner existed at the next inventory (all three0/32).
Local policy/format/diff checks passed; no local build was performed. Initial
native test passes do not close lint, later source, production wiring or ROOT gates.

After that frozen run, the source was narrowed to omit the new audit field from
`MemoryOnly` presentation, preserving its existing response encoding. The required
mode retains explicit audit state. This correction is independent of the unknown
lint diagnostic; the later source needs its own native checks.

Exact r2 diagnostics were retrieved after Fedora read admission returned: three
private-interface errors (Journal/Failure/Status), test-module item order, and
production dead-code for the payload policy variant/Ready state constructed only
by fixtures. The minimal correction uses the original Copy required/legacy policy
plus a separate optional held capability, crate-visible types, tests last, and an
actual fail-closed outstanding-reservation state restored to Ready only on unused
or terminal-complete release. No lint was suppressed and no fake capability was
constructed to satisfy the compiler. Checked arithmetic also now preserves closed
state instead of saturating internal reservation/refund invariants.

r7 at that source passed broker/held-file/Program again, but strict clippy failed
on the added short-write test's safety-comment placement. Only that comment moved
for the next source. The positive short write comes from a private nonblocking
pipe's queried actual capacity; both this and the pipe sync error are deliberately
invalid-producer native error fixtures, not a trusted regular-file storage result.

One local shell bundle was rejected before running because it included unnecessary
`rm -rf` cleanup of a py_compile-generated cache. No cache was created/deleted.
Python syntax was subsequently checked with in-memory compile, avoiding that
side effect. This did not change any held campaign or bypass an approval guard.

r8 final GNU and r9 same-source static-musl broker/held-file/Program tests and
strict all-targets clippy each completed0. These precede one additional targeted
assertion for the combination of terminal audit failure and an already uncertain
peer effect. That assertion was added without a production correction and its
native result must precede any bound change; neither earlier pass covers it.


## Final scoped result and retained raw records

Final product SHA256:

- broker.rs: `41ba3b420c2be8dff14ab627b6edd3d4cba0773fc21788c5b08b74265c3be5c3`.
- broker/audit.rs: `b817575c57916bd0d0bea1e827dd066eab329707c3025426bd921e853300249f`.
- server.rs: `5a46b1a5ce364dcd378334e36e19f2f59eda110129b9b31c29311f63e81df8b7`.

Final r13 uses these exact bytes in GNU and static-musl. Each mode actually runs
15 broker tests (0failed/0ignored), the original regular-file corpus1 test, and
the original Program caller1 test; strict server `--all-targets` clippy exits0.
Local policy/format/diff checks exit0. These are native unit/light caller checks,
not authenticated new-TLS/real-worker/VM/full-matrix/security-durability results.
Native source371MiB plus owned scope805MiB remains below the admitted4GiB; the
scheduler's2CPU/2GiB envelope is not a physical/kernel peak measurement.

r11 is the actual new-bound RED: the prebuilt Program response fell back to
`effect.kind=unknown` instead of preserving `uncertain`. It ran after adding only
the assertion and before correcting the bound. All typed audit failures crossed
with the four actual effect states now contribute to counted capacity; execution
uses that same audit-failure outcome classifier. The final GNU/musl assertion
passes for both Program and MCP. This is an encoding/state-retention result, not
a real partial SOCK_SEQPACKET acceptance or storage failure after a peer commit.

Earlier r10 never executed: Kiln rejected source transfer SHA256
`4c311a50548f5e88ce5b8131ada463a6e14811dc4ed62433d960ed1b06be061c`
versus handed-on
`d681aaa4b00ca1fc24404bb102d1510dc072d0af0aed9072f40a4ff7a76c1b68`.
The evidence README edit immediately after background submission may have raced
handoff; this is an inference, not a diagnosed Kiln defect. The whole tree was
frozen for r11 retry; no bound fix preceded its actual assertion failure.

[raw/linux-records.tar.gz](raw/linux-records.tar.gz):589721bytes, SHA256
`9565790e4b51660a1906c3ba8fcd7d8d3c74d749cdc4d214f2d14899f2b642ce`.
It contains197 source/log/command/result payloads plus one manifest, all unique
safe relative regular members. The implementer checked every original member's
length/SHA256 against [raw/manifest.json](raw/manifest.json), checked final source
against both native records, and retained [raw/transfer-check.json](raw/transfer-check.json).
ROOT independent evidence review remains owed. Initial lint reds, subsequent
safety-comment lint red, original refusal pass, new bound RED and green records
are all included. `driver-latest.py` is the exact final recipe; older per-command
files and frozen source/logs identify prior runs. Regenerable Cargo/target/Zig
caches and latest test binaries remain in the private runner scope, not this
archive; no old overwritten test executable is claimed frozen or retained.

Repeat with a fresh records name and private scope:

```sh
SHARDS_EFFECT_AUDIT_SCOPE="$HOME/managed-effect-audit-native-v1" \
SHARDS_EFFECT_AUDIT_RECORDS=records-new-gnu \
SHARDS_EFFECT_AUDIT_BASELINE=skip \
python3 docs/research/measurements/managed-effect-audit/run-light.py
```

Static-musl uses `SHARDS_EFFECT_AUDIT_TARGET=x86_64-unknown-linux-musl`,
`CC_x86_64_unknown_linux_musl=$PWD/scripts/zig-cc`, corresponding `AR` using
`zig-ar`, and `ZIG_TARGET=x86_64-linux-musl`. Dropping the baseline skip runs the
original baseline refusal with both original broker/server restored temporarily;
`finally` restores the candidate. Existing evidence directories are never reused.
The same <=600s/2CPU/2GiB/4GiB/-j2 admission and ROOT role limits apply.

No lane job remains active. ROOT owns the accepted producer cross-seam, actual
regular-file/storage partial/flush/power-loss, after-accepted terminal failure,
actor access/inheritance/revocation/crash/generation, mutation/forbidden peer
observations, resource/control costs, seven-target and real-VM closing gates.
This is an unwired checked component ready for that review, not production durable
coverage or completion of the management plan.
