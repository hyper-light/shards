# Finite queue after the current-source checkpoint

The tested source stays at1be; signed evidence checkpoint
db4cfc08e7632dbeb3d1a6c45de82e68befeb257 retains both actual pressure outcomes.
These notes continue source/fixture preparation. They do not authorize another
actor, production change, new native case, mutation or VM run. ROOT owns the
combined carried VSOCK and seven-target gates.

## Existing pressure case: source diagnosis

Actual r142 GNU backend-FIN-first completed exact bytes and EOF but failed its
strict short-send assertion; the one unchanged repeat observed100 short sends
and100 send-WouldBlock events. Neither result replaces the other.

At frozen1be `native_tcp_pump_both_half_closes`, the backend Worker calls
`server_script` immediately after accept. `server_script` sends the greeting,
and in backend-FIN-first sends its tail/FIN, then immediately receives the
request. There is no held-read stage. The caller simultaneously starts its
request after receiving that response/EOF. In the shared `Direction::turn`,
the actual `recv` result becomes the pending send length. The caller slot
capacity is an upper bound; the code does not force each read to fill the slot.
The `partial_writes` counter increments only when a native send returns fewer
bytes than that pending slice, and `write_would_block` only on actual send
WouldBlock. The strict assertions correctly distinguish those events from
read-WouldBlock.

Thus the fixture has no synchronization step establishing the event it demands.
Concurrent backend draining is a source-based explanation consistent with the
observations, not a captured causal syscall trace. No I/O corruption or pump
deadlock was reproduced. Queue readbacks8192 and the1MiB request alone did not
make this a deterministic pressure test in the actual run.

Keep the failure and original assertions. Do not pick a larger request, smaller
queue, sleep interval, repeat count or pass allowance to conceal the missing
witness. No production state, syscall permission or Deadline change is proposed.

Two concrete fixture options for ROOT's separate review:

- A full-pump held-reader case needs a flow-owned notification from an **actual**
  native send-WouldBlock before releasing the backend reader. The existing
  Worker/pipe barrier can own the release; there is currently no mid-pump
  notification at that event. Adding a global counter, cache, timer, synthetic
  WouldBlock or second scheduler is outside this design. Any test-only signal
  would need an exact interface review first; these notes do not choose one.
- A focused cfg(test) case beside the existing private `Direction::turn` could
  stage finite real socket bytes, hold the backend read, invoke that same turn
  seam and stop on the first actual send-WouldBlock. It could then drain and
  exercise the retained tail, with no new production hook or second pump.
  It must fail explicitly if the supplied finite bytes do not induce the event;
  it must not spin after WouldBlock. Such a seam case supplements the existing
  whole-pump endpoint cases and cannot be presented as full scheduler/VSOCK proof.

Neither option has been implemented or executed. Preserve current source/binary
identities and original red evidence before reviewing a new fixture slice.

## Next prepared backend-byte boundary case

The already supplied proposal is finite and uses the current decoder, Worker,
original absolute Deadline and single pump. Proposed new cfg(test) function:
`native_tcp_extra_backend_byte_is_not_forwarded`, plus one focused native driver
case. The latest authorization covers the existing corpus only, so this new
case and its meaningful mutation remain unrun.

1. The backend Worker sends the greeting, receives the independent exact request
   and real request EOF, sends the tail, then returns its accepted socket still
   held with its write half open. Completion is not substituted for EOF.
2. The main native client independently receives the complete greeting+tail.
   After joining that receipt, the owner sends the caller's extra58 from the
   held backend socket and performs its real write half-close.
3. The client's nonempty EOF probe runs before relay join, retaining an actual
   unexpected client byte if present. This prevents a removed response ceiling
   from being reported merely as a missing alert or timeout.
4. Strict expected terminal is `ExtraData{leg:Backend,offset:response_len}`:
   to-client read/written=response_len, pending0, extra-consumed1. Backend local
   accepted writes=response_len+1; independent client read=response_len plus
   actual EOF, no extra byte. Preserve already sent request/response facts,
   original time remaining, both joins, explicit child descriptor snapshots,
   held parent pidfd exit/reaping and unchanged binary hashes before assertions.
5. Only after separate authorization, remove solely the response `ByteLimit`
   guard in a private copy. Require actual client.extra_hex58, then exact source
   hash/empty named-file diff restoration and ordinary controls. This is not a
   rollback claim.

No new manager, queue, production signal, cache, provider, grant, helper-control,
allocation-fault, namespace, BPF or VM action is needed for that proposal. New
execution would use another private admitted Linux scope with explicit caller
byte/time limits and the current2CPU/2GiB/4GiB,600s/-j2 ceiling. Neither runner
admission nor native endpoint proof establishes hard parent/kernel resource
guarantees or mandatory route coverage. Paused and ROOT scopes stay untouched.
