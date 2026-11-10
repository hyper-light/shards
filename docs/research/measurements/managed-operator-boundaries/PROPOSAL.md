# Operator control parser/state compositions and current EOF diagnostic seam

This is scoped test preparation plus a source-only diagnostic proposal. No
production file is changed. No operator endpoint, socket, VM, namespace, BPF,
stress/profile or mutation workload is run by this lane. The current terminal
production hypothesis is **not established by these pure cases**.

POLICY_READ IMPLEMENTER: full canonical AGENTS.md, development-rules.md R01–R16,
development-evidence.md, CLAUDE.md, managed-runtime-plan.md and
`/Users/adalundhe/g121-handoff.md` read before work. Original canonical read:
main24f48ee5ebf198485117aa93bf47685ad7d7d53f, policyfiles f5cec365, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
ROOT subsequently confirmed main/origin/main
`096c9c7be1752a5eaf99eda2c7eb4723c2a8f6db`, policy files/hash unchanged; this
clean reused branch `p/managed-operator-boundaries` starts at that exact revision.
Coordinator `01a148884a4770fdaf0111f3f792a248`; implementer
`01a1c5d2ee5871a2a9a9cdc9f56793f0`. Owned changes: only this new directory.
Remote production and its existing cfg(test) module remain read-only pending
ROOT ownership/implementation approval. No children/self-main merge. All prior
e163/82425/e1cb/f2a2 checkpoints and ROOT targets remain preserved.

## Concrete missing compositions

The existing nine `remote::managed::tests` exercise full subscription prefixes,
one complete ACK, malformed lengths/tags, direct ACK methods, direct disconnect/
replay and direct finishing. The following finite prepared cases target the
remaining compositions, without a new decoder, state framework or changed
contract. All constants derive from actual field widths, adjacent invalid values,
integer endpoints or these finite owned fixture bytes.

| Prepared case | Existing concrete contract and newly composed boundary |
| --- | --- |
| ACK prefixes and two coalesced controls | `input` returns None for an incomplete ACK and returns the first frame's exact end when another complete ACK follows; zero and u64::MAX retain their exact cursors. Existing prefix coverage was subscription-only. |
| Malformed ACK before a valid following frame | Wrong ACK payload lengths, unknown tag and known-but-inapplicable Stdout return the exact existing typed error at the first frame, rather than scanning forward to a later valid control. This is parser behavior, not a claim of connection recovery. |
| Invalid subscription channel | Zero/u8::MAX in the exact channel field returns ProtocolError::Channel; adjacent run/generation/member/cursor boundaries are still represented by the later valid subscription. |
| Decoded last legal ACK, prepared state case | Actual encoded/decoded ACKs at zero/start/one-short retain AwaitingAck bytes, pointer and progress; the exact u64::MAX wire end advances Open→Ready or Finishing→Finished as already specified. These count encoded remote bytes, not canonical payload offsets. |
| Malformed control followed by explicit disconnect, prepared state case | After the parser error, invoke the existing disconnect cleanup: AwaitingAck becomes Record(written=0), same owned bytes/start remain, only the original cursor resubscribes and a premature ACK cannot release an unsent replay. The case deliberately does not model `Io::close` or claim Service::controls was invoked. |

[parser-cases.rs](parser-cases.rs) contains three actual parser test bodies.
[state-cases.rs](state-cases.rs) contains two additional bodies prepared for the
**existing** inline `remote::managed::tests` module and its actual `service`
helper. ROOT can review and insert these bodies at that seam; no module wiring
or production edit has happened here. State cases are **uncompiled/unrun** until
ROOT compiles the complete real module. A malformed frame's actual protocol path
refuses and disconnects; these tests do not pretend it accepts the next ACK on
the same live connection.

[light-check.py](light-check.py) prepares a new tiny workspace containing the
unchanged actual ABI crate and **exact source slices** for remote Error,
frame::Tag/peek and managed typed errors/constants/scope/encode/input. It inserts
only parser-cases.rs. Extraction markers must be unique. No Service substitute,
mock transport or copied state implementation is generated. This can establish
only these parser compositions: it cannot establish full remote compilation,
controls dispatch, authentication, refusal I/O, transport progress, ACK-to-FIN,
metadata publication, endpoint cleanup or the current ROOT failure's cause.

ROOT owes full-module compilation/test and meaningful guard mutations. Remove
each relevant parser length/tag/channel/boundary check independently and require
the corresponding forbidden acceptance or wrong consumed frame end; restore
exact source. State guard/real protocol/cleanup campaigns also remain ROOT-owned.
Tests in the ABI checkpoint 82425 remain unmerged, require paired host/guest
rebuilds because existing build.rs hashes test source, and are not silently
incorporated into this baseline.

## Actual ROOT observations received, without cause attribution

ROOT reported r597 at source0456e99e: strict `transport EOF after consumed End`
failed at tenant_exit.rs101; that alone did not identify the returned Event or
establish metadata failure. Subsequent ROOT r601/r604 diagnostic at source
`84e63db080a11550bb5b2d18269b1c43a4dec9b0` reported controlled member1 stdout:
Open wire0, Data28, Data58, End86; canonical payload tail26 and wire102 decoded.
The final ACK enqueue failed with `the connection's driver has ended`, with
queued `Lost(Some(0))`; no transport EOF was witnessed. Owner stop/observer join
and observer FD7→7 were reported true, while causally later sibling progress was
not reached. These are ROOT-reported exact observations; original raw records
and independently checked artifact identities remain ROOT-owned, not a local run.

Canonical End, application ACK enqueue, accepted application ACK and transport
EOF are different facts. `Ask::Ack` does not send client control FIN; that is
`Ask::AckAndFinish`. Fixture Event::End is emitted only after Read::End, after
dispatching the frames carried by that final read. Other returned events are
Record, Lost(Some raw application code), Lost(None) and Failed(text). Reset in
Io::read_within combines missing connection, recv read error and peer reset, so
Failed(`managed read Ok(Reset)`) would not identify its underlying cause.
Reason::Closed is code4; code0 is not that typed reason. Driver request_stop and
Command::Stop can send code0, but observing code0 alone does not select a caller.

ROOT's next diagnostic logs exact Output.finish Err and snapshots metadata and
queued controlled sibling events **before cleanup**. No terminal fix is
authorized from r604 or the older metadata-only prediction.

## Exact handoff hypothesis and smallest diagnostic seam

At main096c, Producer::submit publishes Slot::Record, releases Shared and enqueues
Handler(()). Control::finish separately publishes Ending::Finishing/possibly
Finished, releases Shared, invokes Shared.notify inline and enqueues a second
Handler(()) into the same bounded command channel. Endpoint::bind's existing
command_slots is one. Sender::send maps typed try_send Full and Disconnected to
different errors. Output.finish propagates that error; send_credits currently
discards its identity via is_err and calls Output.abort, which requests driver
stop through Control.abort. Driver::commands_now can then send close code0.
This is a **source hypothesis**, not a demonstrated full-queue origin.

The smallest first instrumentation is at the existing terminal finish call in
send_credits: retain the exact result/error, immutable member/channel/index and
Control progress immediately before the unchanged abort on Err. Credits is
already unlocked; Output.finish's Life guard has been released on return. Log
outside those locks and outside Shared.notify's inline callback. Existing
credit/bridge worker error logs already precede their Owner.abort; retain the
private daemon.log along with metadata before owner stop. If the first log misses,
the next discriminators are named existing end/close/abort_member/Owner-drop
call sites, rather than a new trace manager or broad instrumentation.

Retain a finite caller-budgeted `try_recv` snapshot of each owned observer before
cleanup, including budget exhaustion/disconnection/no-item distinctions. Metadata
and event differences between one member/channel and all active channels help
separate local Output abort from owner-wide failure. They do not prove main/
sibling causal progress, process reap or a complete atomic history. Preserve
the original strict EOF and metadata expectations and all cleanup attempts.

## Primary alternatives, not a selected production change

Rust's [SyncSender::try_send contract](https://doc.rust-lang.org/std/sync/mpsc/struct.SyncSender.html#method.try_send)
distinguishes a full live handoff from disconnection without blocking. The
[Waker contract](https://doc.rust-lang.org/std/task/struct.Waker.html#method.wake)
permits coalescing multiple wakeups while requiring later task polling under its
stated executor lifetime. Pinned
[mio 1.1.0 Waker](https://github.com/tokio-rs/mio/blob/3a1b47180161fe191a8f8a8cc0e1cc572678aab2/src/waker.rs)
is an established readiness notification with an explicit lifetime contract.
Neither is Shards' current UDP/command implementation, and neither grants it
those guarantees automatically. Alternatives if ROOT proves the exact Full cause:

- Coalesce **endpoint-private unit notifications** using typed try_send, proving
  a queued handler observes the already-published authoritative Shared state.
  Preserve Disconnected and owner-stop behavior; generic commands/owned payloads
  cannot be dropped under the same rule. Queue contents/caller exclusivity,
  ordering, reentrancy and final FIN progress still need evidence.
- Publish a terminal Record and Finishing in one existing owned transition and
  issue one notification. This avoids the second handoff but is a larger API/state
  change; canonical End versus Aborted(errno), logical Life/error ordering and
  existing nonterminal callers must remain explicit.

A bare UDP wake is insufficient in current source: the driver ignores its wake
datagram and dispatches Handler::command only from queued commands. Enlarging the
queue needs a derived admission bound; blocking retry can obstruct cancellation.
No option, new capacity, retry loop, cache, thread or manager is adopted here.
The older e1cb proposal remains conditional and its distinct logical metadata
obligations must not be substituted for this observed connection loss.

## Policy and owed evidence

R01/R04/R06/R07/R13: prepared test assertions only, typed parser/state outcomes;
production unchanged and no panic-free claim for the existing transport/library.
R02/R03/R05/R12: exact actual seam, existing test helper, primary contracts and
explicit alternatives; no architecture/runtime constant/wait/cache/OS adapter
selected. R09/R10: retain ROOT observations separately from inferred cause; no
speculative fix, actual causal red/green/mutations still owed. R08/R14: profiles,
resource/CPU/fault/competitive costs entirely unrun/ROOT-owned. R11: any pure
GNU result proves only that target's stated extraction, never hardware, macOS
(arm64 only), musl/Windows or paused Windows ARM coverage. R15: scoped preparation
and new-private-source pure checks only; ROOT owns audits, full real protocol/
VM/chaos/mutation validation and main merge. R16: no local build/full worktree,
protected cache or paused VM mutation; prior signed checkpoints preserved.
