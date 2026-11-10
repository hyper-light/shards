# Fixed ordinary frame and cleanup checklist — unexecuted

Continuation of the signed [proposal](PROPOSAL.md) at
`b59b180f04c55360e25b4acaef5171b552b60068`. ROOT assigned authoritative corpus
emission to the audit owner, from SAME actual Count/intent_record/
AuditReservation and terminal_shapes/terminal_record, with synthetic members1/3.
Actual emitted bytes/bounds have not arrived. Imax/Tmax/B remain symbolic; no
fixture leaf, source registration, build or actor is implemented/executed here.
The same ba78 constructor and8ac API contract remain authoritative.

POLICY_READ renewed: implementer / assigned adversarial; full current main
`ca3ab10c151aa5539e757f1cc76029746ccbb561` AGENTS/R01–R16/evidence/CLAUDE/plan
and current handoff read. Rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Only this measurement directory is owned; runner/target remain unallocated.
No children, emitter, production changes, profiles, old FIN retry or main merge.

## Ordinary wire shapes

Sources: frozen ba78 `fs::{write_frame,read_frame,answer}`, server's actual
dispatcher/Reply::entry, shared ABI Request/Reply and
[Linux v6.18 FUSE UAPI](https://raw.githubusercontent.com/torvalds/linux/v6.18/include/uapi/linux/fuse.h).
This is a literal-frame input checklist using the existing transport and codecs,
not a new parser or transport. There is one real stream pair and one server.

All FUSE fields below are little endian. Shared audit-control and retained file
fields use their actual ABI big-endian codec. The four-byte outer frame length
is separate from the FUSE header's length; both must agree with actual bytes.
No private high-bit SAVE/RESTORE/DESCRIPTOR frame is sent.

Common FUSE input header:40 bytes, with checked total length/opcode, unique ID,
actual node ID, and the native fixture process's actual uid/gid/pid. The extension
length and padding are zero. These uid/gid/pid fields are not audit scope grants;
the already-bound file handle selects the immutable Scope. Reply header:16 bytes,
checked total length, signed Linux errno and identical unique ID.

| Request | Body bytes and finite input | Exact success payload after16B reply header |
| --- | --- | --- |
| INIT |64: major7/minor45 (actual server contract), zero readahead/flags/flags2 and all reserved words |64B actual server INIT result; require major7/minor45, no DAX, matching declared max-write/pages; no negotiation retry |
| LOOKUP |9: actual `Scope::member_name()` including its NUL, at root node1 |128B actual entry:40B entry fields plus88B attributes; require nonzero returned node and regular file |
| OPEN |8: O_RDWR2, open_flags0 |16B: actual fh, FOPEN_DIRECT_IO1, zero backing field |
| IOCTL |32 plus `wire::CONTROL_BYTES`: actual fh, flags0, `wire::IOCTL`, arg0, both copy sizes176, `Request::encode()` |16B zero ioctl result header then exactly176B `Reply::decode()` |
| WRITE |`proto::WRITE_IN_BYTES`40 plus actual record bytes: fh, returned offset, exact payload size, all other input fields zero |8B: exact accepted length and zero padding |
| RELEASE |24: actual fh, O_RDWR2, release flags0, owner0 |Empty body; exact16B FUSE success header |

Every receive must supply an actual frame of the exact expected shape; early
None, short body, wrong unique, nonzero outer errno, unexpected typed cause or
native stream error is a retained failure. The intended quota refusal is outer
FUSE success with inner `Stage::Empty/Cause::Exhausted`, not a text match or an
assumed success. Require no lease fields on that refusal and remaining0.

The data frames use the actual corpus lengths Ilen/Tlen, not padding up to the
reserved maxima. Check each sum fits u32, `proto::MAX_WRITE` and `fs::MAX_FRAME`
before starting the case. No fragmentation, extra allocation allowance or
production cap change is selected if the corpus fails those bounds.

## Exactly24 requests, one connection

The unique IDs are the checked one-based positions in this fixed input list.
They are correlation values, not audit call IDs or authority. Call0 is the
existing private Journal's first-call convention. Stable member1/3 never become
indices0/1 after filtering the files-only member2.

1. INIT, exactly once.
2. LOOKUP member1's actual canonical name; retain node1-result separately from root1.
3. OPEN that returned node; retain its actual fh.
4. LOOKUP member3's canonical name; require a distinct returned node.
5. OPEN that returned node; retain its actual distinct fh.
6. Identify member1; expected Empty/None with remainingS.
7. Identify member3; same remainingS, its own bound Scope.
8. Reserve member1 call0, actual Imax/Tmax; expected Reserved/None/remaining0.
9. Duplicate that Reserve under a new FUSE unique ID; exact same typed Reply.
10. WRITE actual intent bytes once at returned intent_offset; exact acceptedIlen.
11. Inspect member1 call0; IntentWritten/None/acceptedIlen/remaining0.
12. CommitIntent call0; IntentSynced/None/accepted0/remaining0.
13. Duplicate CommitIntent; exact same typed Reply and retained bytes.
14. WRITE actual terminal bytes once at returned terminal_offset; acceptedTlen.
15. Inspect member1 call0; TerminalWritten/None/acceptedTlen/remaining0.
16. CommitTerminal call0; TerminalSynced/None/accepted0/remaining0.
17. Duplicate CommitTerminal; exact same typed Reply and retained bytes.
18. Close member1 call0; Closed/None/accepted0/remaining0.
19. Duplicate Close; exact same typed Reply and retained bytes.
20. Inspect member1 call0; same Closed result with lease offsets/maxima retained.
21. Reserve member3 call0 with same requested maxima; Empty/Exhausted/remaining0.
22. Identify member3; Empty/None/remaining0, no lease; same own Scope.
23. RELEASE member1's original fh; no extent refund or file change.
24. RELEASE member3's original fh; no extent refund or file change.

Then end/drop the client once and join `fs::answer`. There is no second INIT,
connection, OPEN, reserve attempt after refusal, duplicate WRITE, retry or
resubmission. A failed phase leaves later phases Unreached. In particular, no
field from a missing Reply is reconstructed from the expected offset formula.
The formula is checked against actual returned offsets only after receipt.

## Exact byte-region witnesses

Set H=`wire::JOURNAL_HEADER_BYTES`, O=`wire::SLOT_OVERHEAD`, S=O+Imax+Tmax and
ONE B=2H+S, all checked. File1 length is H before Reserve, H+S afterward. File3
length stays H. Both actual decoded headers contain the SAME B and retention.
Member2's files-only name is absent. No physical allocation or hard quota follows.

Open the two real files read-only once after construction and retain those same
descriptors. Record actual device/inode/nlink and compare path metadata to them
at the end. Every snapshot comes from those held objects, not reopened names.
Record directories/snapshots outside the disposable audit root.

After Reserve, freeze the entire raw file as the unmodified-slack reference.
File1's checked regions are:

```text
[0,H)                               exact JournalHeader::encode()
[H,H+24)                            BE64(call0), BE64(Imax), BE64(Tmax)
[intent_offset-8,intent_offset)      BE64 committed Ilen after CommitIntent
[intent_offset,intent_offset+Ilen)   exact actual intent codec bytes
[intent_offset+Ilen,terminal_offset-8) retained unused intent tail
[terminal_offset-8,terminal_offset)  BE64 committed Tlen after CommitTerminal
[terminal_offset,terminal_offset+Tlen) exact actual terminal codec bytes
[terminal_offset+Tlen,H+S)           retained unused terminal tail
```

Only the selected WRITE range changes during each append. Only its8-byte prefix
changes at its commit. All other regions equal the prior snapshot. Intentional
duplicate controls, Close/duplicate Close, refusal and RELEASE preserve whole
snapshots and exact lengths; every relevant typed Reply retains remaining0.
Keep the raw snapshot pair around each duplicate, rather than only a summary hash.

These observations reject double append, changed extents, changed bytes and
double debit. They cannot distinguish an identical overwrite at the same offset
from no write syscall. Source shows the immediate Ready duplicate path does not
create an I/O ticket; that is source evidence, not an executed syscall counter.
No new counter/shim/profile is authorized or added here, and no live no-syscall
claim is made. ROOT retains any stronger I/O-count obligation.

## Ownership and cleanup facts to record

Before later execution, create a new private writable temp parent within the
admitted scope and supply it as TMPDIR, separate from permanent records/artifacts.
Use the existing `shards_testdir::TempDir` under that parent. Source readback found
that first use of TempDir sweeps ended-process directories under its temp parent
and calls existing descriptor-limit preparation. A new initially empty private
parent confines that sweep to this scope. No old test parent, preserved FIN data,
root cache or unrelated file can be its cleanup input.

The common helper can raise the process soft FD limit up to its existing hard/
kernel maximum. Record inherited/resulting limits before accepting execution;
do not claim unchanged FD limits. No limit setter is added in this leaf, and no
change of limits may be used to bypass normal admission or a no-cap-change
requirement. This actual existing behavior is disclosed for ROOT review before
actors; no helper correction or override is proposed from this source finding.

Keep setup/request processing fallible until cleanup runs. On successful or
failed processing, end/drop the one client via existing `shards_ipc::end` before
joining the answer thread; its actual implementation shuts Write then Read.
Do not replace it with a single Shutdown::Both. No worker assertion/panic should
be the mechanism that triggers stream shutdown; store the first actual failure,
perform bounded cleanup, then assert with those retained receipts.

Record the answerer's actual Result and join, not merely that a join was called.
For exposed owned stream/witness descriptor numbers, EBADF checks occur directly
after their final drops before another open can reuse the number. Private writer
FD numbers are not exposed by the public API; Drop and file-byte observations do
not independently prove every kernel object has returned. Do not invent that fact.

Retain original requests/replies, typed outcomes, before/after snapshots and
cleanup facts outside TempDir before dropping it. Actual root absence after Drop
must be observed; a best-effort destructor is not a successful deletion receipt.
If a filesystem/stream call exceeds the one30s work deadline, parent45s includes
15s cleanup, then report Timeout with unreached/unknown facts. Do not assert join,
resource return or raw preservation that did not actually occur. No case retry
or expanded time window follows. Preserve the failed private scope as needed.

This is preparation only. No runtime cleanup, descriptor change, test-directory
sweep, native file reservation or syscall witness above has happened in this
lane. The later envelope remains2CPU/2GiB/4GiB/600s/j2/build180/case30+15/parent45,
ARM64 Mac only with normal admission, private target,30GiB floor and existing lock.
Build/actors remain held for exact corpus and ROOT review; all prior source/raw,
held campaigns and paused WinARM remain preserved.
