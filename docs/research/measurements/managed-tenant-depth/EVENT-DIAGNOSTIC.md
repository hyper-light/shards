# Single-event fixture diagnostic preparation

POLICY_READ implementer/assigned-adversarial, receipt7371: canonical main
0342b85d9c808e3ced041a6c3ef0e9bf70438272, full AGENTS/rules/evidence/CLAUDE/plan/
handoff and required closing records read. Rules SHA256
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
ROOT explicitly approved this finite preparation in Capture::receive, not a new
VM attempt. Base29189b6a8 and all prior branches/artifacts remain preserved.

## Exact change and retained failure

Only crates/shards/tests/managed_output/tenant_exit.rs::Capture::receive changes.
It retains the Event returned by the SAME existing observer.next(until). Before
the unchanged strict Record failure, it writes to the existing optional private
trace directory:

- Lost with its actual optional numeric code (None stays null).
- Failed with its actual existing String cause, without classifying its text.
- End with its actual variant; no invented code/cause.
- The observer's member/channel, last accepted canonical StreamState and current
  wire_consumed cursor. State Debug contains only New/Open/Ended/Aborted and
  numeric offset/errno fields; no payload.

The new file is non-record-MEMBER-CHANNEL-CURSOR.json. File::create_new refuses
to overwrite an earlier trace. A sink error emits only io::ErrorKind to stderr,
then preserves the original constant panic, including when trace is absent.
Partial/failed disk writes and allocation aborts do not promise a retained receipt.
JSON escapes cause control characters in the private file. No failure text is
added to the ordinary panic/log. The Event is not cloned, replaced, or read again.
The canonical-End transport-EOF read that already existed remains unchanged.
All Record parsing, ACKs, byte/terminal/neighbor assertions and cleanup remain
byte-identical. No parser/Observer/API/helper/grant/deadline/native expiry changes.

Exact diff: event-diagnostic.diff. Source and unchanged dependency hashes:
event-diagnostic-source-freeze.json. Local structural comparison proves the diff
is within receive, the Record handling tail and both original next call sites
are unchanged, and current_tenant_stdio/workload/common observer/parent source
are byte-identical to29189. This is source comparison, not live diagnostic proof.

## Cause privacy and source provenance

Inspected primary source at29189 (same crate bytes as bbd/cba for these seams):
common/managed_capture.rs Event, Client::fail and every fail caller; remote
net.rs Io::{connect,write,finish,read_within}, remote/frame.rs::take/peek and
remote/lib.rs::Error; abi/run/managed.rs::StreamState. Failed currently receives
fixed fixture messages, frame errors containing numeric tag/length, transport
errors, or connect errors that can include the supplied endpoint. The readable
fallback covers Reset/errors; Bytes/End payloads take separate matching arms.
Lost discards the driver's text reason at its existing constructor, so this seam
cannot recover it. The diagnostic never fabricates that missing text.

No actor payload/chunk/expected bytes, identity secret or auth/token object is
serialized by this addition. The supplied fixture uses generated private owner
identity, without a real provider/auth cache. Existing String/library errors
are still treated as private content: exact cause goes only to the already
authorized trace under the campaign's umask077, never ordinary telemetry. This
review is not a promise about every future library error. No new debug rendering
of the Event into ordinary logs is used.

## Validation and operations

The first local locked rustfmt and scoped diff checks pass (tool305c22). A later
durable-check attempt refuses the already-held Mac lock before rustfmt executes
(tool543ed2); its original error log remains. No lock bypass or local build follows.
The remaining durable format/policy/diff checks run in the admitted private scope.
Structural source checks pass and the original expiry archive remains37,056B/SHA256
17a785e9b59502797cfe1f6371ab26c18caf51562961acccc1fb650e40c7c66d.
No original64 raw file, archive, failure, caption, frozen source or binary is
regenerated/relabelled. The source is based on the preserved bbd/cba fixture
campaign, not a claim of validation against current0342 production bytes.

The exact light-only command is event-diagnostic-lint.command. It admits the
existing private Fedora8fadc1ec213332e16cb0d915a3fb913f scope
~/managed-tenant-depth-a920-01a1c5d2-v1/{source,target,records,cache,frozen},
linux/amd64,2CPU/4GiB/24GiB,j4,600s lint+15s forced cleanup. No VM/test/helper is
executed. No local cargo/build/target/cache; local Mac ARM64 free space remains
above30GiB. ROOT managed-capture-check/target INUSE r921 is excluded.
Remote lint is pending at this source checkpoint; retain its exact receipt later.

## R01–R16 scope and remaining gates

| Rules | Evidence and remaining limit |
| --- | --- |
| R01 | Test-only diagnostic; strict panic retained. No production panic/error change. Sink errors do not replace that panic; allocation/disk success is not guaranteed. |
| R02 | Existing source constructors and actual r79 missing-event RED motivate only this diagnostic; no architecture/product decision or external contract change. |
| R03 | No runtime allowance/cap/default; names and fields describe existing typed values. All original caller inputs/deadlines remain. |
| R04 | Match actual Event variants; no text-based classification, guessed cause or success transition. Last canonical state and consumed wire cursor come from Capture. |
| R05 | Same observer/parser/trace/state owner; no wait, retry, reconnect, polling, cache or manager added. |
| R06/R07 | Local explicit match and one optional write; no new helper/interface or speculative comments. |
| R08/R14 | No audit/profile/benchmark/cost/allocation or resource-return advantage claimed; those are ROOT-owned and unrun here. |
| R09 | Original r79 strict RED retained; its actual Event/cause is still Unknown. This is diagnostic preparation, not a bug fix. |
| R10 | Exact source/diff/light command plus original EXPIRY-RESULT/raw. No new green/mutation/live receipt claimed. |
| R11 | Light GNUamd64 lint planned only; seven-target/other real VM hosts remain ROOT owed. Mac ARM64 only; paused WinARM untouched. |
| R12/R13 | Existing fixture seam only; no platform adapter, production API, wrapper, copied parser or new observer. |
| R15 | Explicitly bounded implementation/light lint only; no new VM under spent authorization, children, goals, main merge or held campaigns. ROOT review/new exact campaign assignment required. |
| R16 | Same private target/cache, campaign lock, admitted envelope; no active cache deletion or broad copies. Old raw/frozen artifacts preserved. |

Owed: ROOT review of exact source/diff/privacy/light receipt, then a NEW explicit
bounded VM assignment and fresh source/helper/input readback before an actor.
No r79 rerun, time increase or product fix is authorized by this preparation.
No inference from104.70s, memory logs or after-owner-stop aborted metadata.
Direct waits/full population/parent/kernel return and full management closure
remain Unknown/open as recorded in EXPIRY-RESULT.md.
