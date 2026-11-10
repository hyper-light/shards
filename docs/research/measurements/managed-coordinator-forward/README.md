# Actual finite buffered UDP4 comparison and admission ordering

Root coordinator executed r531/r556 at source
`7ee6c4bcf90796c6a1540ed748a5ef89940825e1` and r567 at
`c141f67d01777b93ab0b41f8628a68c9f911a370`, on Fedora amd64 KVM with normal
carried helpers. This is one known loopback flow, Binary plus Empty datagrams,
with two retained payload slots and an actual independent backend. It is not a
production relay, full route enforcement or native server-side transparency.

## Actual forwarding and guard effects

The r531 full host caller compiles0. Fixed-original-peer connected and
unconnected cases pass strict0: client original endpoint and actual bytes match;
ingress/request, upstream/reply and backend receipts establish both legs. The
backend sees the distinct held upstream socket, **not** the actor's original
source. Destination-only cases complete both legs but fail strict101 on actual
client peer presentation. All four report owned child reaps, worker joins,
selected-link detach and scoped cgroup removal.

Root r556 performs independent changes with exact source restoration:

| Changed behavior | Actual strict result and independent witness |
| --- | --- |
| Remove connected Connect4 | 101; correct client bytes still arrive, but backend peer is the actor, not the held upstream. |
| Remove unconnected Sendmsg4 | 101; the same actual direct-backend bypass despite an Exact client result. |
| Flip request-slot byte after validation | 101; backend records `5345515545535400ff`, differing from original `5245515545535400ff`. |
| Flip reply-slot byte after validation | 101; client records `4341434b454e4400ff`, differing from original `4241434b454e4400ff`. |
| Remove payload reservation comparisons, supply17 | 101; guest actually retains18 payload bytes and forwards both exchanges; the independent host capacity assertion rejects the excess. |

Each of the five restored runs passes strict0. All mutation/restoration cases
report both child reaps, joined workers, detached acquired links and cgroup
removal. The request corruption exercises the backend error and bounded receive
deadline path; it is not a throughput result. The buffer mutations establish
that actual slot contents reach the other leg rather than synthesized replies.
The hook cases establish a meaningful bypass witness rather than a missing log.

`raw-7ee6/root-managed-forward-guards-7ee6-v2/driver.py` is the exact executed
guard driver, SHA256
`bf44be181b162eba93239bb7363b4837b423dfa38f0e3d6ca68a190ef28b780c`.
Per-case source patches, actual guest hashes, commands, selected environment,
raw console and `results.json` remain retained. Restoration runs in `finally`,
checks exact original bytes and advances source mtime; `restored.diff` is empty.
Both changed source hashes are in `restored-source.json`.

## Reproduced ordering and the small fix

The original17-byte case returns typed Limit(provided17, required18), after
Target has already reported a namespace, cgroup inode and two bound sockets.
It has no aggregate Cleanup/Observed record. This establishes unnecessary setup
before refused admission, **not a demonstrated leak or product defect**.

Source c141 moves the existing reservation and its record before namespace,
cgroup and socket acquisition. A local Scripted/Buffered owner carries the
same reserved Slots once into existing Work; it creates no second manager,
decoder, cache, deadline or platform adapter. Slot sizing/refusal, native socket
operations, original endpoint predicates and the worker algorithm are unchanged.
Root reviewed the complete18-add/8-remove code delta and its
[implementation evidence](../managed-network-sock-addr/admission-order/README.md).

Root r567 full actual host compile passes0, then executes six real guests:

| Caller case | Connected / unconnected result |
| --- | --- |
|17-byte envelope | Both strict101 with exact typed Limit17/18; no Target, ForwardTarget, child, hook or admitted-slot record. The exact source returns before setup. |
|18-byte admitted forwarding | Both strict0; PayloadSlots precedes Target, all two-leg/backend-source checks and complete owned cleanup hold. |
| Existing scripted responder | Both strict0; no PayloadSlots/ForwardTarget record, and existing original-endpoint/cleanup checks hold. |

These caller-selected17/18 values are the boundary of the concrete9-byte request
and9-byte reply slots. They are not production defaults. The new early-refusal
cases do not infer a general kernel-resource return proof from missing records.
The strict positive-only host test intentionally fails on refusal; the retained
coordinator driver separately requires that exact typed negative outcome.

There are21 bounded real-guest cases across these campaigns:11 strict positives
and10 expected strict negatives, not21 normal passes. Each diagnostic driver
returns0 only after checking its declared outcomes. Frozen actually executed
guest binaries remain in separate runner-owned artifact directories outside the
text record archives. An initial freeze preflight compared common's formatted
`sha256:` value to bare hex and refused before any guard run. The original
driver/preflight diagnosis remains retained; parsing that documented prefix
then requiring exact64-hex/hash equality corrected the recorder. No integrity
comparison was disabled. Earlier per-process copies auto-retire through the
existing common helper; retained identity records and frozen copies distinguish
that behavior from permanent artifact availability.

## Closing target checks and retained data

Root r552 runs complete `scripts/lint` at combined source
`c2eb4760e8363af0c36adeb01dd08c3d2b9c3b08`, including the first relay source.
Root r564 repeats it at c141, including the ordering change. Both pass0:
format, all seven supported targets and the script's GPU checks. Compilation
does not establish KVM/HVF/WHP hardware coverage. Unmodified AWS-LC/Zig optional
SHA3 compiler-probe fallback warnings remain in both logs; crypto/trust and
compiler verification were not weakened. Only identified untracked `.DS_Store`
metadata is preserved in place in the Mac clone; tracked source remains clean.

| Retained subtree | Raw regular files | Verified private archive bytes / SHA256 |
| --- | --- | --- |
| `raw-7ee6` |182|98825 / `4175217b63b3049002ab3642be9c550a3507709adda79e79558510b1713bd758` |
| `admission-c141` |79|37959 / `4028354e2bcf08b398e5e139882c75db62547a7a6ac22e02f593bf75de8bacb6` |
| `matrix-c2eb` |12|4032 / `8c77039f8f8d691081c04e6ef8eca834a10612dcfb6250e3afe4eed0418fcc41` |
| `matrix-c141` |11|3860 / `9ece2ada23f2fa44dea6287c8b60cc2934f4b8927c09c72f7c2272c3ccc61905` |

Every subtree has its own MANIFEST/TRANSFER record. All284 raw hashes, safe
regular paths and absence of executable bytes were checked. Original archives,
source and frozen artifacts remain preserved. `raw-7ee6` retains r531's exact
per-case commands/environment and build/artifact records; its orchestration was
an inline coordinator command. The committed host fixture is its repeatable
harness. r556/r567 additionally retain their exact executed driver source.

The experimental kernel/config association, explicit120000ms boot/30000ms work
deadline,256MiB VM and one vCPU remain labelled test inputs. The kernel remains
the prior experimental artifact, not a new product pin. PayloadCapacityOnly
excludes allocator, stack, metadata, console, socket queues, BPF, parent and
kernel charges. No total hard budget, all-route mediation, generalized reverse
map, source preservation, multi-tenant fairness, leak freedom, allocation count
or4–5× performance claim is established. IPv6/multiple destinations, full failure
and revocation coverage, independent population/kernel return, broker/adapters,
credentials and restore generations remain open. Windows ARM stays paused.

Policy coordinator receipt: main42e3f758, file revision f5cec365, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
R01/R04/R06/R07/R13 source/error/ownership review and R02/R03/R05/R12 alternatives,
literal derivations and shared readiness/decoder choice are in the linked
[buffered source record](../managed-network-sock-addr/buffered-relay/README.md).
R09/R10 actual effects and restoration are recorded above; R11 target checks and
unrun hardware remain explicit. R08/R14 distributions, costs and comparisons
remain coordinator work. R15 scoped implementers supplied source/light checks;
root performed all live campaigns/review/integration. R16 preserves data,
evidence, active caches and the paused VM; no host privilege change occurred.
