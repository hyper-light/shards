# Raw control input: ROOT approval required

Base cbd39284937d024640c56db6f8d8ef2bd9b0ee24; current R15 assigned adversarial role. The new adversarial leaf owns no
parent/production source. Item A uses existing Ask::Ack and needs only the
unapplied nested registration patch, reusing normal Inputs/Capture/gate/cleanup.
The c705/79f sources were inspected via git show; their initial-cursor/scope/
generation cases remain frozen and are not duplicated.

Item B needs a way to submit controlled malformed bytes after the real existing
owner has authenticated/subscribed and actually received a record. Proposed
ROOT-owned test-only command:

```rust
// In the existing Ask enum:
RawControl { bytes: Vec<u8>, finish: bool, receipts: mpsc::Sender<ControlReceipt> },

// In existing Client::command:
Ask::RawControl { bytes, finish, receipts } => {
    if bytes.len() > managed::CONTROL_BYTES
        || self.control_receipts.is_some()
        || !self.pending.is_empty()
    {
        self.fail("raw control requires one bounded command after flushed subscription".into());
        return;
    }
    self.control_receipts = Some(receipts);
    self.pending.extend_from_slice(&bytes);
    self.finish |= finish;
    self.flush(io);
}
```

The exact unapplied patch is `raw-control.patch`. The one optional test receipt
sink is armed once; it has no protocol authority. Existing flush reports each
positive accepted write and remaining pending bytes, readable reports actual
read counts/end, and FIN reports its actual result. No queue, window, worker,
transport or retry is added. The command
carries at most the existing CONTROL_BYTES. An overdeclared wire length is only
a small actual header; the fixture does not allocate its declared payload. The
existing pending/flush/writable callback retains partial writes; existing finish
requests FIN only after pending becomes empty. Enqueue means accepted command,
not processed bytes or acknowledged consumption. One command per selected case,
one fresh owned run/endpoint, no replacement connection.

Concrete callers are unknown/malformed/overdeclared control headers without FIN,
and a truncated ACK prefix followed by FIN. Current controls/readable predicts
Protocol for invalid frame/tag/length and Closed for an incomplete control at
client FIN; actual event/cause is retained even when it misses the expectation.
No source prediction is a claimed passing result or permission for a product fix.

Alternatives: existing typed Ack is sufficient for A but cannot serialize invalid
headers; copying Client/authentication/Driver into a separate handler duplicates
the existing implementation; raw UDP bypasses the authenticated QUIC stream;
production state injection bypasses the actual failure path. A larger handoff,
hidden retry, polling or sleeping would mask other behavior and is excluded.

Primary contracts: current remote net.rs Io::write returns partial/zero progress,
Io::finish ends only this send side; actual parent Client::flush handles those
through the existing pending buffer/callback. Current managed Service reads into
its fixed CONTROL_BYTES array and calls controls before the Read::End branch.
[RFC9000 sections2 and3.2–3.4](https://www.rfc-editor.org/rfc/rfc9000.html#section-2)
separate ordered stream delivery, its directions, application consumption and
connection lifetime. None supplies Shards' application ACK or metadata guarantee.

Proposal sent to ROOT before dependent code under stable key
`adversarial-controls-A-contract-B-raw-seam-proposal-v1`, then the exact receipt
patch under `adversarial-controls-raw-control-exact-patch-cbd392-v2`. No raw-control caller
will use this command before ROOT approves it; ROOT owns the parent edit.
