# Current caller, detached buffers and charges

This is the implementer's source/ownership handoff, not ROOT's independent
audit or live fault/cleanup proof. The full startup caller, thread/death races,
allocator faults and actual parent/kernel attribution remain coordinator work.

## Actual buffer caller and terminal owner

`send_credits` is the only production caller of `Producer::take_buffer` at this
checkpoint. The reader `bridge` borrows the issued buffer from the credit slot;
it never calls `take_buffer` itself. Actual normal ownership is:

`Endpoint::bind → Waiting → Ready → Capturing + Granted → Receiving → Captured → Record → AwaitingAck → Ready`.

`Slot` and `Credits` remain the existing authoritative owners of progress and
issued credit. The Vec moves between them; no spare pool or second progress
ledger exists. The public loan returns a raw owned Vec, so dropping it alone
cannot notify the endpoint. Its documented trusted-caller obligation is to
submit it or abort `Control`. This is not a universal RAII guarantee for future
arbitrary Rust callers.

The actual daemon paths preserve that obligation as follows:

| Exit/transition | Existing terminal owner and storage consequence |
| --- | --- |
| Preparation fails before publication | Staged producers/controls/endpoints and the charge drop; `Control::drop` aborts. No actor has been released. |
| Startup fails after `Serving` insertion | `managed_capture` calls `Outputs::end`; the retained Owner aborts capture and controls. |
| Subscriber disconnects during a loan | Remote `Capturing` persists. The existing issued credit/reader retains its Vec and may submit it; reconnect must use that cursor. |
| A channel is revoked while `Granted` | `Credits::revoke` moves its buffer into `RevokedGranted`. An already issued finite input can still drain. |
| A channel is revoked while receiving | `RevokedReceiving` records that the bridge's stack owns the Vec; `captured` retires it after reading that finite record. |
| Revocation wins between take and credit publication | `send_credits` sees the slot is no longer `Sending` and drops its local buffer without writing credit. `Output::stop_transport` owns the concurrent stop. |
| Capture read/parse/accounting/credit-write failure | Existing reader/writer scoped closures call `Owner::abort` after the function returns Err. The owner closes credit admission and the stream and aborts each output. |
| Normal bridge return | The last buffer is already moved to `Captured`; the retained writer submits it and applies canonical finish/ACK handling. |
| Typed undersized-loan refusal | Remote stays `Ready` and returns no buffer. The daemon propagates a typed Endpoint failure before writing a new grant; its writer's Owner boundary aborts capture. Legacy owned-Vec submission remains valid for direct callers. |

Ordering is exact: `Output::stop_transport` revokes credit **before** calling
`Control::abort`. `Owner::abort` closes credit slots, ends the stream, then stops
outputs. Dropping a detached buffer may therefore precede completion of the
already owned stop operation. This source trace does not claim that the remote
slot is already terminal at the instant every Vec is dropped. It identifies the
retained stop owner and error boundaries; ROOT owes thread/death/fault witnesses.
No credit/metadata/Life lock is held across `Control::abort` or actor I/O.

## Charge arithmetic and limits

For a logical window `C` and selected daemon allocation capacity `A`, the
existing two-record calculation plus allocator excess charges `C + A`: one
existing VM-process record allowance and the actual daemon record Vec. This is
not complete guest/init/transport accounting or an owner entitlement.

When growing to logical window `D`, the prepared charge adds `(D - C) + D`
while the old allocation remains charged, then adds any reported replacement
excess `Anew - D`. Successful replacement releases `Aold`, leaving `D + Anew`.
Thus the known old/replacement daemon overlap stays represented until commit.
Acquisition refusal keeps the previous window and the scoped growth guard
returns its temporary charge. A release underflow quarantines the existing
global optional balance as unavailable; it never manufactures extra room.

The global sum/free-memory comparison remains the existing experimental logical
pressure check. It is explicitly not hard backing or a checked owner grant.
If actual-capacity accounting cannot be completed after preparation, no larger
credit is published and the actual caller aborts capture. This does not prove
hard admission for the transient preparation itself. ROOT still needs the
missing owner producer and complete induced-parent/kernel support.

`size_of::<CreditSlot>()` in the existing structural calculation automatically
includes the enlarged grant variant's Vec metadata. No claim is made that it
includes allocator headers, QUIC/TLS/driver/command structures, thread stacks,
guest init outgoing storage or all kernel objects. Those obligations remain
open in [OWNER_INPUT.md](OWNER_INPUT.md) and [README.md](README.md).
