# Next finite proposal: already-pending cancel before recovery

Prepared after tested signedce60195826e41d4f5075d66a68648e9bc6d6d632.
Current tested sourceb840, all existing proofs and original db4 pressure19/20
remain frozen. This is a proposal only: no new test code, actor, mutation or
production change is implemented or executed. ROOT reviews the interface and
assigns any subsequent campaign. Current approval covered only the completed
forwarding-pressure case, one invocation per libc.

The existing `exercise(original: &Deadline) -> Result<(), Failure>` already
holds a real cancel eventfd, records independent prefill WouldBlock, obtains
pending from a real source recv, and verifies one real blocked turn. The next
case can reuse that exact setup rather than add a second pump or notification.

Proposed private cfg(test) interface, not a production API:

```rust
enum Terminal {
    Forward,
    CancelBeforeRecovery,
}
fn exercise(original: &Deadline, terminal: Terminal) -> Result<(), Failure>;
```

The existing test selects Forward. A new ignored
`native_direction_turn_pending_cancel_retains_suffix` selects the second path.
The only branch is after the independently verified prefill drain and before
the first recovery turn. No production visibility/type/state/queue or helper
manager changes; caller byte/queue/time inputs remain the same.

Finite sequence and decisive witnesses:

1. Require the same actual prefill send-WouldBlock and real source-recv pending
   suffix, then one actual turn send-WouldBlock with exact retention. Missing
   events fail immediately; no tuning, sleep or retry of the missing event.
2. Independently drain and retain the exact prefill. The relay's payload written
   count remains0; source accepted13 versus actually consumed/pending3 remain
   distinct facts. The receiver has capacity after this real drain.
3. Write the eventfd ABI's uint64 value1 to the SAME owned cancel object before
   another turn. Record the actual8-byte write and positive ORIGINAL deadline
   remaining; do not consume the eventfd or construct another deadline.
4. The SAME Direction turn must return typed Fault::Cancelled and retain the
   suffix/read/written/pending/sent/half/extra and WouldBlock counters exactly.
   Expired, native fault or an actual successful write is failure. Record the
   actual typed turn result and snapshot before any strict assertion.
5. Close that owned output and independently perform a nonempty backend read:
   actual EOF0 and no relay payload, preserving the already accepted/drained
   prefill receipt. This is stronger than a missing-byte alert or timeout. All
   owned FDs drop, with child/parent pidfd/wait/reap/binary/descriptor witnesses
   and raw data before assertions. No rollback of prefill or consumed facts.

If ROOT separately assigns a private guard-removal mutation, removing only the
existing pending-cancel precheck would need an actual independent backend
payload prefix524551, not just a wrong error label. Exact hash restoration and
ordinary controls would be mandatory. Such a mutation is not authorized by this
proposal and has not run.

Suggested finite native queue after approval: GNU+musl one pending-cancel case
and one existing forwarding-pressure control each, private Linux2CPU/2GiB/4GiB,
<=600s/-j2, original case30s/parent45s and unchanged finite caller inputs.
No VM, full scheduler, performance, all-route or hard-resource guarantee.
The checked-ready/concurrent-cancel race after admission remains unresolved;
this proposal tests only cancellation that is already pending before the attempt.
ROOT's central VSOCK/matrix and management closure remain separate obligations.
