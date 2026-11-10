# Managed finish notification: confirmed full-handoff abort

ROOT authorized this focused fix after its actual r606 diagnostic at source
`fc7b` reported `managed output diagnostic finish member=1 channel=1: the driver's admitted command slots are full`.
ROOT retained pre-owner-stop metadata `aborted accepted102 consumed86 tailunknown`;
the controlled canonical End at wire86/payload tail26 had arrived, but the final
application ACK enqueue failed after `Lost(Some(0))`. ROOT reported other three
channels queued only Open; causally later sibling progress was not established.
This exact finish-Full→abort cause is distinct from e1cb's still-unimplemented
logical terminal-publication proposal. Original guest commands, raw logs and
artifact identities remain ROOT-owned; this lane performs only the authorized
native endpoint/unit validation and supplies source/evidence.

POLICY_READ IMPLEMENTER: full canonical AGENTS.md, R01–R16,
development-evidence.md, CLAUDE.md, managed-runtime-plan.md and
`/Users/adalundhe/g121-handoff.md` read before work. Base main
`096c9c7be1752a5eaf99eda2c7eb4723c2a8f6db`; policyfiles f5cec365, unchanged
rules SHA256 `d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Coordinator `01a148884a4770fdaf0111f3f792a248`, implementer
`01a1c5d2ee5871a2a9a9cdc9f56793f0`. Owned source:
`crates/remote/src/managed.rs::Control::finish` and targeted existing cfg(test)
additions; evidence: this new directory. Existing worktree reused on
`p/managed-finish-notify`. No subagents/main merge/local build/full worktree,
ROOT clone/target write, paused VM, privilege, trust or crypto change.

## Actual cause and chosen seam

Producer::submit stores the owned record in authoritative Shared state and sends
one Handler(()). Control::finish publishes Finishing, releases the Shared lock,
notifies existing waiters, then sends a second unit notification. The real endpoint
has an existing one-slot bounded handoff. A full handoff was mapped to a transport
error; daemon Output.finish propagated it and its caller converted it to abort.
This is ROOT's reproduced cause, rather than a queue-size inference alone.

The focused candidate uses the existing typed Sender::try_send only in
Control::finish. A Full result coalesces the unit wake; Disconnected remains the
same Transport error/message as the old Sender::send path. The captured bytes
stay in Shared; the queued command holds no record or unique request payload.
Producer::submit, generic command handling, stop, queue size and logical terminal
metadata behavior remain unchanged.

Rust's [SyncSender::try_send contract](https://doc.rust-lang.org/std/sync/mpsc/struct.SyncSender.html#method.try_send)
distinguishes full from disconnected without blocking. Its buffered send result
is not evidence that the receiver processed the message. The
[Waker contract](https://doc.rust-lang.org/std/task/struct.Waker.html#method.wake)
permits multiple notifications to coalesce under its task-poll ordering/lifetime
guarantee; pinned
[mio Waker](https://github.com/tokio-rs/mio/blob/3a1b47180161fe191a8f8a8cc0e1cc572678aab2/src/waker.rs)
provides another established notification/lifetime contract. These are mechanism
alternatives, not permission to infer their guarantees for Shards' UDP waker.
Shards already keeps state under its own Shared mutex and dispatches the pending
unit callback through the existing command channel.

Alternatives considered: combining record+finish publication into one submission
is broader API/state work; a larger queue needs a derived resource bound; a blocking
retry can obstruct cancellation; a bare UDP datagram does not dispatch the current
handler without a queued command. ROOT selected the smallest private unit-wake
coalescing change. No generic command/payload coalescing rule is introduced.

## Ordering and race obligations

1. Finishing/Finished publication occurs under the same existing Shared mutex,
   **before** the typed send attempt. No new copied terminal state is maintained.
2. If a previous unit was consumed before that publication, the new attempt can
   queue a unit. If the attempt observes Full, a unit still queued at that check
   will observe the published state through the Shared lock when dispatched.
   Dequeue concurrent with the check still processes state published beforehand.
3. Only managed Producer/Control own these private unit senders. Their notifications
   carry no unique bytes or command data. Revocation already uses request_stop's
   separate flag. The change does not weaken generic Sender semantics.
4. Disconnection remains an error even though the preexisting code publishes its
   state before notification. Existing caller abort behavior remains in that case.
   Stop or driver loss after a successful/coalesced notification can still win;
   success is accepted finishing intent, not application ACK, FIN or durability.
5. Shared.notify still runs inline after releasing Shared. Its callback and daemon
   credit/Life lock ordering are untouched; no new I/O or Life acquisition is added
   to that callback, and no wait/retry/cache/manager/thread is introduced.
6. The similar Producer::submit Full branch remains a source-only question with
   no separate reproduced example. It is not changed by this fix. Neither are
   other preexisting abort/finish races or e1cb's metadata-publication obligations.

## Targeted native regression and remaining gates

Three tests added before the production change construct the actual native
Endpoint/Producer/Control and real bounded Sender. They do not run Driver::run,
establish a QUIC peer, execute a VM or claim transport EOF. Their finite one-second
patience is setup admission only, not a measured latency tolerance. The handshake
allowance uses RFC 8446's uint24 length plus header; other allowances use existing
wire/datagram sizes. No runtime budget is enlarged.

- The actual Producer::submit fills the handoff while its driver is deliberately
  not running. An independent typed Full check establishes the queue condition;
  finish must succeed and retain the exact pending record pointer, bytes and
  accepted/consumed progress while publishing Finishing.
- A queued unit before an empty stream's finish must coalesce, including a
  repeated finish, and retain Finished(0).
- Dropping the actual native driver establishes typed Disconnected; finish must
  retain its Transport error rather than report success.

Actual Kiln r5/r6, Fedora amd64 runner `8fadc1ec213332e16cb0d915a3fb913f`:
Linux 6.19.10-300.fc44, glibc2.43, rustc1.99.1
(58cf8f9c35b355199727627d07ae34a4e2846d67), LLVM23.1.1. NEW private base
`/home/adalundhe/managed-finish-notify-light-096c-v1`, source `source/`, target
`target/`; 1 CPU/1 GiB/2 GiB Kiln headroom requests, one build/test thread,
debug information/incremental disabled for the small target. Source+target+
Git/evidence storage measured after r6 was 365,916 KiB. CPU/RSS peaks were not profiled; admission
is not a hard memory quota. No ROOT cache or clone was touched.

A filtered sparse checkout of exact main096c retained the actual remote, ABI,
IPC and testdir crates and unchanged pinned vendor/provider. Only workspace
membership was narrowed; actual package metadata, lints/dependencies/configuration
were retained. There were no stubs or copied state methods. The unused rquickjs
patch and ordinary inherited AWS-LC build notices remain in raw logs; provider/
crypto settings were not changed. [light-result.json](light-result.json) retains
the original full commands/logs, private manifest/lock, toolchain and receipt.

```text
cargo test --offline -p shards-remote --lib managed::tests::finish_ \
  --target x86_64-unknown-linux-gnu --target-dir PRIVATE/target --jobs 1 -- --test-threads=1
baseline production + new tests: exit101; 1 passed, 2 failed, 0 ignored
both full-handoff cases: Transport(Error("the driver's admitted command slots are full"))
dropped-driver error control: passed

cargo test --offline -p shards-remote --lib managed::tests \
  --target x86_64-unknown-linux-gnu --target-dir PRIVATE/target --jobs 1 -- --test-threads=1
fixed production + same tests: exit0; 12 passed, 0 failed, 0 ignored

cargo clippy --offline -p shards-remote --lib --tests \
  --target x86_64-unknown-linux-gnu --target-dir PRIVATE/target --jobs 1 -- -D warnings
exit0
git diff --check -- crates/remote/src/managed.rs
exit0
python3 scripts/check-development-policy
exit0; evidence/coordinator review is still required
```

Pre-fix source SHA256 (baseline production plus remotely formatted test additions):
`96e3fe4c16783b79a16ff75ea5365f5840196fbef385f9b5f8f9055bc7a50c70`.
Initial fixed/tested source before the layout correction:
`f586f0ddd0f424a5755c72ca57282917448e17c19a797127028b62609b6642e8`.
The sparse source initially omitted rustfmt.toml, so that formatting introduced
unrelated line wrapping. It was caught before checkpoint. Restoring the exact
tracked `max_width = 110` configuration removed that noise; final r8 checks on
the final layout passed 12/12 managed tests, scoped Clippy, diff and policy again.
Only Control::finish and the three new tests differ from base. Final tested and
committed source SHA256:
`5782d2f9d9c83cf2db6d4d1baf3152cd1f3daf5aef9856ebce788bdcbf098bab`.
The canonical local format command requested by ROOT was also applied:
`rustfmt --config-path /Users/adalundhe/Projects/shards/.claude/worktrees/managed-main-landing/rustfmt.toml --config skip_children=true crates/remote/src/managed.rs`.
It retained those same final bytes. Prior source hashes and all r5/r6/r8 raw
records remain retained; no earlier layout is silently called the final result.
Resolved private
Cargo.lock SHA256:
`80f1121858b0e3a6e0bb8ea531c944e6781d2c0ea4b774bace18806e7877f1ec`.
Original red/green source snapshots, logs and private build remain retained remotely.

Full ROOT normal-exit repro,
guard-removal mutation, complete combined/supported-target/actual hardware gates
and resource/FIN/sibling/metadata outcomes remain owed to ROOT.

R01/R04/R06/R07/R13: one typed match at the existing seam, no new panic/index/
arithmetic path; existing allocation-error/provider limits remain unproven here.
R02/R03/R05/R12: primary contracts, exact reproduced caller and alternatives;
no numeric production literal, additional queue, wait/cache/OS adapter/interface.
The disconnected text reuses the existing Sender path's exact presentation.
R09/R10: ROOT actual cause precedes fix authorization; targeted red/green and
source hashes are required, with original workload/mutations ROOT-owned. R08/R14:
no profile, benchmark, allocation/fault or comparative result/claim. R11: native
GNU light checks only when actually reported; other targets/hardware unverified,
macOS arm64 only, Windows ARM paused. R15/R16: scoped signed checkpoint and
preserved prior branches/evidence, coordinator review/actual campaigns/main merge,
private admitted tiny target and no protected cache/source mutation.
