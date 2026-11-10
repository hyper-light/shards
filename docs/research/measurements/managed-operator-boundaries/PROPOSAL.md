# Operator control parser/state compositions and current EOF diagnostic seam

This is scoped test preparation plus a source-only diagnostic proposal. No
production file is changed. No operator endpoint, socket, VM, namespace, BPF,
stress/profile or mutation workload is run by this preparation batch. These pure cases do
not establish terminal causation. ROOT subsequently proved the finish-Full
cause in r606 and authorized the separate signed fix
`d6f2e4e63cc6e155a041da63a3e42cc61fdd8185`, native red/green evidence under
`docs/research/measurements/managed-finish-notify` on its own branch. The original
normal-guest, matrix and distinct e1cb metadata obligations remain ROOT-owned.

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
or production edit has happened here. The first checkpoint 4038 left state cases
uncompiled/unrun; the later private full-module r12 result below fills that pure
compile/run gap. ROOT still owns integration/combined gates. A malformed frame's actual protocol path
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

ROOT then logged exact Output.finish Full in r606, with pre-stop aborted102/86
metadata and other channels queued only Open. That later causal result authorized
the separate focused Control::finish fix; r604 and the metadata-only prediction
alone did not authorize it. The source-only instrumentation and alternatives
below record the proposal as it stood before that evidence, not a new fix here.

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

## Primary alternatives reviewed before ROOT's separate selection

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
ROOT later selected endpoint-private unit coalescing for the separately confirmed
finish-Full cause. The older e1cb proposal remains conditional and its distinct logical metadata
obligations must not be substituted for this observed connection loss.

## Actual parser-only light checkpoint

Kiln r10: Fedora amd64 `8fadc1ec213332e16cb0d915a3fb913f`, Linux
6.19.10-300.fc44/glibc2.43, rustc1.99.1
(58cf8f9c35b355199727627d07ae34a4e2846d67), LLVM23.1.1. NEW private source/target
`/home/adalundhe/managed-operator-boundaries-light-096c-v1`; explicit
1 CPU/1 GiB/2 GiB headroom admission, one Cargo build/test thread. Exact root
rustfmt.toml was copied and explicitly selected. Policy and formatting exited0.

```text
python3 light-check.py REPO NEW_PRIVATE --target x86_64-unknown-linux-gnu
cargo test --offline --manifest-path NEW_PRIVATE/Cargo.toml \
  --target-dir NEW_PRIVATE/target --target x86_64-unknown-linux-gnu \
  -p managed-operator-boundaries --lib -- --test-threads=1
exit0; 3 passed, 0 failed, 0 ignored
```

Only the three actual extracted parser tests ran in r10. The real ABI was compiled
as their dependency, with no ABI/source change. That run formatted the two state
cases without compiling/running them or the complete actual remote module;
the subsequent r12 result below extends the pure scope. Protocol dispatch, I/O
and transport for these prepared additions remain unrun.
The extraction generated SHA256
`53c9feb683586cbad9ec7ede594887641de5f3a893d124fdc6f22e25731d98e4`.
Parser-cases SHA256
`7b31d68a74c6eb9f1ccc3b0957133afe072f116f84c71fa63d329bb43f33adcf`;
formatted-only state-cases SHA256
`051286d38757ab862c71fc4d14634dccbeee9413448a4f9aec0b3467addffd1f`.
[light-result.json](light-result.json) retains exact commands, full original logs,
scope, source hashes and resolved lock. No local build/compile or protected ROOT
cache write occurred. Full-module/source ownership approval and ROOT campaigns
remain owed; no state or live-transport pass follows from r10's parser result alone.

## Actual full-module pure state checkpoint

Kiln r12 used NEW private
`/home/adalundhe/managed-operator-state-light-096c-v1/{source,target}`, Fedora
amd64, 1 CPU/1 GiB/2 GiB admission, build/test threads1 and debug/incremental0.
It copied the existing lane-owned minimal four-crate source snapshot, excluding
Git/build output, and restored the actual main096c managed.rs from its Git object.
It inserted the exact prepared cases in a private cfg(test) child module of the
existing tests, using that module's real service helper. There were **no** service,
parser, mutex, transport or state stubs and no production method substitutions.
The repository's managed.rs remained unchanged. Original production SHA256:
`eeb45ad4e8a7fd2fcaaff293bede87b2adc99e37cd5fa9f3d036da2d7f6d852f`.
Private source with test-only insertion:
`4777135e5b58403ccd1d91376b64494571cdabdcdbd2e76fa613f25067577fe5`.

```text
cargo test --offline -p shards-remote --lib managed::tests::operator_boundaries \
  --target x86_64-unknown-linux-gnu --target-dir PRIVATE/target --jobs 1 -- --test-threads=1
exit0; 5 passed, 0 failed, 0 ignored
cargo clippy --offline -p shards-remote --lib --tests \
  --target x86_64-unknown-linux-gnu --target-dir PRIVATE/target --jobs 1 -- -D warnings
exit0
```

Only the three parser and two direct Service state compositions executed.
No native endpoint/driver/socket, actual wire connection, protocol controls
dispatch, refusal I/O or VM ran. Full source+target/storage measured348,936KiB;
CPU/RSS were not profiled and headroom admission is not a hard quota. Ordinary
unchanged provider notices and unused-patch warning remain in raw logs.
[state-light-result.json](state-light-result.json) retains full original logs,
commands, case hashes and receipt. [state-light-check.py](state-light-check.py)
keeps the same private insertion/build recipe for replay; its Python AST passed,
while the actual first run used the recorded equivalent inline setup. No ABI
source changes or additional target/hardware results are implied.

This closes the local pure-case compile/run gap, not ROOT's module ownership,
integration, actual protocol, adversarial/mutation or seven-target gates. ROOT's
reported r632 now observes both member1 ACKs and transport EOF after the separate
wake fix, but the fixture's missing TCP gate entry prevents final metadata and
causally later sibling checks. Header-only intermediate metadata is not a closed
whole-case result or authority to implement e1cb.

[integration.patch](integration.patch) makes the exact five prepared bodies
directly reviewable against signed wake-fix d6f2. It inserts only a private child
module of the existing cfg(test) module and imports its real helper; it changes
no production line. [integration-identity.json](integration-identity.json) pins
the base/prepared source and patch hashes. This is a proposed patch, not an
applied repository source change. Its d6f2-combined compilation and ROOT adoption
remain unrun/pending; the actual previous private five-case result is on096c.
An ordinary `git apply --check` passed in a new admitted tiny private source
directory against the exact d6f2 file; no patch was applied or compiler invoked.
[integration-check.json](integration-check.json) retains the actual command and
the earlier mechanical check as well. This proves patch applicability only.

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
