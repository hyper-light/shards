# What this FIN receipt can establish

Scope: current0342 / frozen d8a2 owned fixture only. This note was prepared while
native execution was pending headroom. The later actual result is in RESULTS.md;
the source/primary interpretations here are distinct from that actual evidence.
No code, actor or additional queue item is introduced by this note.

The new receipt samples the existing Client.pending length after its real
Io::finish call. Io::finish invokes the same QUIC send stream's finish method;
it does not call Io::write or receive an application ACK. A successful receipt
with pending0 establishes the local operation/buffer observation. The selected
peer's typed Closed event remains a separate required witness.

[RFC9000 section2.4](https://www.rfc-editor.org/rfc/rfc9000.html#section-2.4)
treats writing, ending and resetting as distinct stream operations.
[Section4.5](https://www.rfc-editor.org/rfc/rfc9000.html#section-4.5) relates final
size to the stream's byte offsets. Therefore pending0 at this operation does
not mean the entire established stream had zero prior bytes or final size0.
These protocol facts do not prove Shards' application refusal/lifetime behavior.

The selected command offers an actual empty Vec, but uses the already established
control stream. Source-derived prior inputs are one58B Subscribe and three13B
application consumed messages (Open/Data/Aborted),97B logical control input.
Derivation from current managed.rs:5B frame header +32B run +8B generation +4B
member +1B channel +8B cursor =58; consumed is5+8=13. The native record array has
three entries and sends one consumed message per observed record. This is source
accounting, not a measured packet, retransmission, TLS/QUIC overhead or final-size
readback. No runtime literal or entitlement is selected from it.

| Witness | Exact limited meaning |
| --- | --- |
| RawControl command accepted | Local command handoff; no assumed peer processing. |
| Actual successful Finish with pending0 | Same send-side operation accepted locally with an empty current pending buffer; not an Io::write return0 or whole-stream zero-byte claim. |
| Actual typed peer Closed | Required application refusal after transport end of control; not inferred from local finish success. |
| Canonical Aborted(offset26,errno5) | Actual received ABI cause, independent of remote Finished90/90 and the peer's connection reason. |
| Finished90/90 and capacity0 after control/join | Required authoritative credit/terminal observations; wake notification count is not credit authority. |
| Exact owned postterminal Closed return | Required same allocation/capacity/bytes; no successful new payload submission. |
| Observer/owned endpoint joins and FD4→4 | Required local teardown observations; not full population/kernel return or daemon metadata durability. |

The absence of a positive Write receipt alone establishes none of the first two
I/O facts. The actual successful Finish pending observation and typed peer event
must both be retained. Actual actor witnesses are reported only in RESULTS.md.
The original positive accountant and Writes::new(0) rejection stay unchanged;
empty-FIN assertions apply only to the explicit new selection. ROOT owns broader
combined/mutation/matrix/resource/durability validation.
