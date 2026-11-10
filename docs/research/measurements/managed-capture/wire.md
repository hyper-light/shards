# Reviewed capture wire and finite transport contract

Implementer `/root/managed_capture`, policy SHA-256
`0d936117dbc6663eb02829c3e921879cecdad3a8f503047588baa21dc82da936`.
Parent approved this scoped representation after its actual KVM red at fixture
`cdc463b4302bbe85ca2d6107a0b4c2cc6b8a594a`: Fedora runner `8fadc`, kiln r47,
42.08 s, exit 101, 0 passed/1 failed. The parent retains
`~/managed-capture-red.log` plus environment/policy logs on that runner. Main stdout
was the expected JSON-RPC. Main stderr contained prefixed, merged and rewritten
member stdout/stderr and invalid-UTF-8 replacement. This is a coordinator-supplied
live result, not an implementer's E2E run.

The same red exposed a fixture confound: `common::run_ending` always enables
`SHARDS_TIMING`. The corrected fixture accepts exactly its main stderr bytes and
one final timing diagnostic, strictly checking the existing numeric fields and
marker pairs. It does not discard arbitrary lines. Its new revision still needs
the coordinator's live run.

## Representation and limits

The coordinator's subsequent tenant clarification supersedes the initial aggregate
slot proposal below. Each declared AGENT/HARNESS is a tenant. Credit, subscriber
authority and offsets bind `(run, member, channel)`, with an ancestor run/tenant
lease preventing multiplied entitlement. `Credit` is exactly u32 member + u8
canonical channel + u64 admitted capacity (13 bytes); channel validity and finite
capacity are checked. `Abort` on the independent signal/control path is exactly
u32 member + positive i32 errno (8 bytes). At most one trusted tenant abort is
reserved/coalesced per declared member; observer loss never emits one. Owner-end
and tenant quarantine report unknown uncaptured tails, never zero loss.

The internal setup entries are unique `managed-capacity=` (finite protocol ceiling),
`managed-members=` (positive u32 count) and `managed-token=` (exactly the existing
16-byte HELLO token). These are checked launch metadata, not new manifest fields.
No tenant/channel receives credit merely because a sibling has an available slot.
Adaptive issued credit must fit the caller's actual admitted lease and protocol
ceiling; it cannot expand authority. Full tenant/channel slots must not block the
reader serving other admitted records or main/control progress.

Files-only declarations keep their declared identity without starting an output
service. `managed-active=` carries a nonempty sorted unique list of u32BE declared
IDs below `managed-members`; its checked parser borrows fixed-width entries.
The daemon derives the active plan from authoritative image files using the
existing EROFS/VFS resolution seam; trusted init verifies its actual runnable
ordinals match before release. IDs remain the existing `Domain.id - DOMAIN_FIRST_ID`
(agents then harnesses, including gaps); scheduling positions are separate.

Normal duplex management closure uses empty `MANAGED_CONTROL_END` (wire ordinal47,
46 reserved by lifecycle for quiesce refusal). It follows independent reserved
control records before Signal closure; an EOF without that terminal is not a
successful scope end. No subscriber acknowledgement is required to end the owner
run. This new-path contract still needs the coordinator's real layer-save/order
and control-death mutation checks.

`shards_abi::run::managed::Record` borrows its payload. It allocates nothing and
checks version, reserved/tag values, exact variant payload shape and arithmetic.
`StreamState::accept` enforces New → Open → Data* → End|Aborted with contiguous
per-channel offsets. It leaves state unchanged on rejection. The owning transport
maps the trusted member index and channel to that state and validates the Open
label against its checked launch plan.

| Field | Source/derivation |
| --- | --- |
| Header bytes | Four u8 tags + u32 member index + u64 byte offset = 16 bytes |
| Revision/event values | Version 1 and Open/Data/End/Aborted wire ordinals; not tuning values |
| Channel values | Existing stdcopy stdout/stderr ordinals 1/2 |
| Reserved value | Zero; unknown extensions are refused |
| Open | Nonempty UTF-8 trusted label, offset zero |
| Data | Nonempty opaque bytes; checked offset addition |
| End | Empty payload; only actual capture EOF |
| Aborted | Exact positive Linux read errno, signed i32 representation |
| Run scope | Authenticated dedicated connection; this slice makes no restore-generation or exact descendant-writer claim |
| Frame capacity | Caller-admitted finite record capacity, including the 16-byte envelope; no copied default/percentage |

Existing run-header kinds MANAGED and MANAGED_CREDIT extend the wire ordinals
after OVER. Managed records travel on a separate EXEC_PORT connection authenticated
with a single registered HELLO token. No new fixed VSOCK port is needed. Credit is
backed by a finite admitted producer slot and returned only after the authenticated
owner's consumption acknowledgement. Observer loss returns no credit and never
takes VM lifetime ownership. No subscriber means no canonical-data read credit.
There is no persistent spool or lossy Logger fallback.

## Research, alternatives and remaining proof

The existing [buffered-relay review](../../managed-runtime-buffered-relays.md)
compares raw capture and display, explicit credit, Envoy/HAProxy resource handling,
HTTP/2 connection interference and transport delivery limits. This slice applies
its reviewed dedicated-path proposal rather than another multiplexed callback on
the main connection.

- Linux [pipe semantics](https://man7.org/linux/man-pages/man7/pipe.7.html): pipes
  are byte streams; capacity is queryable, and EOF requires all writers to close.
  Separate fd1/fd2 pipes preserve channel identity, not exact descendant identity.
- [RFC 9113 §§5.2/6.9](https://www.rfc-editor.org/rfc/rfc9113.html): per-stream
  windows do not remove connection-wide interference. Distinct frame tags on the
  existing main connection therefore do not establish independent progress.
- [RFC 8949 §§3.1/10](https://www.rfc-editor.org/rfc/rfc8949.html) distinguishes
  bytes from text and requires resource-aware decoding. The existing small binary
  frame seam avoids an unnecessary generic CBOR dependency and base64 expansion.
- Linux [VSOCK contract](https://man7.org/linux/man-pages/man7/vsock.7.html) and
  existing Shards `accept_hellos`/exec consumer supply the owned transport seam.
  Restore can lose stream connections, so restore generations remain a separate
  explicitly owed requirement.
- The [Linux socket accounting contract](https://man7.org/linux/man-pages/man7/socket.7.html)
  includes overhead in socket buffers; OS queue sizes are accounting inputs,
  never authority to reserve extra host memory. The parent owns actual run/kernel
  admission and endpoint integration review.

ABI unit checks cover opaque binary bytes, both channels, End/Aborted, truncated
headers, invalid tags/shape, offset overflow and rejected transitions. Coordinator
still owes raw real-VM canonical/EOF checks, meaningful mutations, unsolicited or
stalled HELLOs, FD return, no-consumer/backpressure control progress, resume/move,
admitted kernel/host cost, all seven targets and the full adversarial/performance
matrix. The performance target and G01–G24 completion are not claimed.
