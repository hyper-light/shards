# Same-owner operation admission: concrete caller obligations

Implementation task at main7d7e0855/df2, not a new authority design or completed
implementation. Parent owns native.rs/Owner/phase/reactor/registration; native
owns the one runtime wire/helper client; resource owns the held execution leaf;
epochs owns the original-current fence. This leaf will implement transitions as
methods on the SAME Owner. No second OperationOwner, grant registry or ledger.
The initial b797/382f parent freeze and3ce native ABI freeze are independently
verified/read; their constructors/Bootstrap are not runtime operation admission.

Before editing operations.rs the owners must freeze these real caller inputs:

1. Native's canonical RuntimeRequest/Reply/refusal and exact Operation selection
   fields, Scope44 and SAME Programs nonzero checked Worker identity. No copied
   encoder, actor/adaptor ID or new execution counter. Bootstrap Loaded does not
   stand in for runtime association/commit.
2. Parent's SAME Owner access to original protected scope/context, actual member
   lifecycle and issued operation rights, plus where the existing job's held
   pending/committed/retained state lives. The original freeze drops snapshot on
   Installed; it has no published operation-selection method. A table decoder or
   Context getter cannot fill this missing producer.
3. Resource's exact held member-parent→prepared job→born/associated/retained
   objects and fallible methods. Ordinary held objects, receipts and changes
   must pass through the actual parent loop; no test closure replacing them.
4. Epoch's captured-current check integrated at the same mutation/commit point.
   A copied current boolean or lookup of the latest run generation is excluded.

## Admission order to implement at that existing call

| Actual event | Required authority/ownership transition |
| --- | --- |
| Runtime request on installed private owner endpoint | Match the original captured Scope/Epoch/member AND actual owner lifecycle/association. A decoded table, Loaded or a matching label supplies no authority by itself. |
| Prepare operation | Check actual issued operation rights against the selected owner object and explicit process permission; acquire actual resource/lifetime objects before returning a prepared job. Preserve typed refusal and returned rejected objects. |
| Associate birth | Bind SAME job identity and actual held pidfd/ACK, using native's existing birth mechanism and resource's held job scope. PID/FD numbers alone cannot associate another child. |
| Commit after canonical audit intent | Recheck original-current/lifecycle and unchanged issued operation selection immediately before the existing effect release. A successful initial prepare/load cannot authorize a later stale commit. |
| Cancel before commit | Close admission at the existing authoritative state, return a typed cancellation and prevent effect release; do not invent execution/EOF/wait facts. |
| Cancel/revoke/loss after commit or uncertain effect | Preserve immutable scope, actual resource/kernel handles and charge ownership. Report uncertainty; never convert unknown completion into a fresh refusal/refund/replay permission. |
| Matching native terminal | Actual matching reap AND actual populated0 are required for the resource owner's retirement method. Leader death, socket EOF or caller drop alone cannot return charges. |

The parent loop must call these methods for actual received typed operations;
an isolated predicate or unused API is incomplete. Parent receives the smallest
module/call-token patch for its own source; no active whole-file edits/copies.
No lock or mutation borrow is held while performing actor I/O; owner reactor
readiness remains the existing implementation, not another waiter/manager.

## Targeted validation once the signatures are frozen

Use the actual parent Owner plus canonical native codec/resource objects in a
new normally admitted private2CPU/2GiB/4GiB600/j2 scope. Exact source/command and
result counts precede a passing claim. Pure transition/codec tests cover scope/
member/current/phase mismatch, rights refusal, duplicate/out-of-order operations,
pre-effect cancellation, committed uncertainty retention and release requiring
matching reap+empty. An ordinary owned caller demonstrates success/refusal/
cancel-before-effect with actual effect/held-object cleanup witnesses where its
frozen API permits. No counterfeit authority object/test-only manager is used to
turn unavailable kernel backing into a passing gate.

Linux guest authority/cgroup paths cannot be proved by a Mac crosscompile or an
ordinary file standing in for cgroupfs. Hardware/guard unavailable or missing
real APIs remain explicit gaps. No VM, hostile-helper/malformed flood, allocator,
ptrace, cotenant effect, race campaign, audit/profile/benchmark/sweep or main merge.
Current R15/R16 and the30GiB Mac floor/lock remain binding.

Parent/native/resource/epochs immutable source receipts were requested at
Kiln11478/11480/11481/11482 after initial disconnected deliveries. Until their
runtime signature freezes arrive this is caller preparation, not leaf source,
test evidence, a new grant, or lane completion. Independent source work continues
without mutating their in-flight graphs or the completed a718 native proof.
