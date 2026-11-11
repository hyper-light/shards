# Owned payload entrypoint for the actual Write stage

FROZEN SOURCE ONLY. This delta has not been compiled, formatted by Rust tools or
executed. The seven passes and strict checks at bb5548066 remain evidence of
that earlier source only. No runtime, old-case replay or held audit graph/case
was dispatched for this entrypoint.

Native requested the concrete ownership entrypoint for its actual CONTROL3/
helper Write Stage, which must retain one Prepared and its acquired initialized
payload through owner Prepare, audit intent, current Commit and partial pwrite.
Native retains the wire/resolver/current-owner/Stage/metadata caller; audit owns
canonical Records. This leaf does not create that producer, authority or backing.

## Exact API

```text
Request::new(offset: u64, bytes: &'a [u8]) -> Result<Request<'a>, Failure>
Request::owned(offset: u64, bytes: Vec<u8>)
    -> Result<Request<'static>, RejectedPayload>
Request<'a>::into_payload(self) -> std::borrow::Cow<'a, [u8]>

RejectedPayload { pub bytes: Vec<u8>, pub failure: Failure }
```

The consuming accessor preserves the request's original lifetime. Existing
Prepared::existing/identity/offset/requested/accepted/cancel/commit and Step stay
unchanged. An owned Request yields Prepared/Step with the static lifetime and
retains the Vec inside that state. The implementation obtains each temporary
slice only during commit; it stores no reference into its own Vec.

Both constructors share the original checked offset+length/native range logic.
On bounds refusal, RejectedPayload returns the same Vec; its Debug reports only
Failure. No descriptor enters Request::owned, so a caller's descriptor remains
with that caller on this refusal. On object refusal, existing Rejected returns
the same descriptor and Request. Consuming request.into_payload returns
Cow::Owned(Vec) for the owned constructor, preserving ownership of the original
initialized allocation, capacity and bytes. No allocating into_owned/to_mut,
clone, copy, resize, extra payload queue or new acquisition is called here.
Terminal/cancel still consume and drop the request and descriptor under the
existing contract; this is not a new terminal payload-return or refund promise.

The retained state is std::borrow::Cow, whose borrowed and owned forms and
immutable dereference are documented in the official [Rust Cow API](https://doc.rust-lang.org/std/borrow/enum.Cow.html)
(observed std1.99.1,58cf8f9c3,2026-10-13; variants stable since1.0). A private
duplicate enum was unnecessary for these same two concrete forms. Storing a Vec
beside a reference into itself was rejected; no unsafe pinning/self-reference or
second state owner is introduced. This is a source representation choice for
the explicit caller request, not a measured cost or allocation-free claim.

## Source and evidence

Preimage is bb5548066be4208019e933565a9f5b8547194d36, write.rs6696B/3bfda368
and tests7065B/a6d24583. PREIMAGE.json and before/ retain the complete bytes.
MANIFEST.json pins the complete after/ leaves and the exact6137B patch
a023ae43094d9ebe3b926d146652bb9d5fc60fbb5ae1d3409df2b690a8dc086f.
Native and ROOT received that frozen delta at receipts13065/13066 before any
consumer completion was claimed. Only the owned writer paths changed.

Two targeted tests are authored, UNRUN: a bounds refusal checks the original
initialized Vec pointer/capacity/bytes, and an actual directory-object refusal
checks the same returned descriptor/dev/inode and owned payload pointer/
capacity/bytes. The original seven cases keep their names and predicates; one
private byte comparison uses as_ref for the new storage type. No fake fixture
constructor, grant, use or suppression was added to consume the API.

The only syscall-region diff is its safety explanation now covering selected
bytes of either storage form. Object checks, mode/append refusal, accepted-prefix
cursor, one pwrite attempt, Partial ownership, terminal facts and current/audit
obligations are unchanged. Live owned success/partial/cancellation/refusal,
cleanup and mutation sensitivity remain unverified at this new source.

## R01–R16 account

| Rule | This delta and remaining evidence |
| --- | --- |
| R01 | Existing checked arithmetic is shared; new owned error returns payload, Debug hides content, no production unwrap/assert/panic or unsafe operation is added. Compiler/implicit-path validation is UNRUN. |
| R02 | Explicit actual Stage need, existing writer seam, official Cow source contract and alternatives above; no broader architecture or new manager is selected. Caller compatibility remains UNCOMPILED. |
| R03 | No production literal or quota/deadline changes. New test lengths/capacities/offsets are fixture-only; they authorize no storage. |
| R04 | Borrowed/Owned storage is explicit; bounds refusal returns Vec, object refusal returns FD+Request, Partial retains the same state, terminal consumes it. No new current-owner bool/cursor constructor or permission. Live transitions are UNRUN. |
| R05 | One existing Request/Prepared owns the payload and descriptor. No queue/cache/wait/poller/timer/registry/trait/manager is added. Resource acquisition and charging remain the real caller's obligation. |
| R06 | Two constructors share one original bounds check; one consuming ownership accessor and redacted error type. No syscall/control-flow branch expansion in commit. |
| R07 | Ownership and unsafe lifetime rationale are explicit; no narration or content exposure is introduced. |
| R08 | No performance or4–5× claim; ROOT comparable cost gates remain open. |
| R09 | New requested ownership capability, not a claimed reproduced legacy defect. No semantics fix is presented as live reproduction. |
| R10 | Exact before/after/patch and two authored ownership tests are retained; no compiler/runtime/mutation/cleanup pass is claimed. Native/ROOT own actual caller and live evidence. |
| R11 | Current support is seven targets, macOSarm64 only. Every new-source target/compiler/runtime check is UNRUN; WindowsARM VM remains paused. |
| R12 | Shared standard storage representation at the existing leaf; no platform adapter, duplicate executor or installed binary. |
| R13 | Retains exactly borrowed or already-acquired owned bytes; no self-reference, extra acquisition, speculative framework, fake caller or getter removal. |
| R14 | No profile/benchmark, cost, allocation-free or leak-free claim. Pointer/capacity tests are only authored; physical backing/parent/kernel cost remains ROOT-owned. |
| R15 | Sole implementer, current0d261/df2 receipt13045, no children. Native retains real consumer ownership. Compiler/formatter/runtime jobs0; original seven are not replayed. Both rejected audit4819 graph and five record cases remain held. No main merge. |
| R16 | All prior source/raw/targets/frozen binaries and untracked evidence preserved. No cleanup, host root/security action, Mac Rust command, VM or cgroup action. |

POLICY-CHECK.json records the unchanged mechanical policy script executed against
ten inputs byte-equal to current0d261 origin/main. This uses the existing owned
read-only projection because the worktree is sparse; it is not a new source
compiler, readiness, compliance, resource or integration gate. Actual Session/
backing/admission/current-owner, helper/actor/route, all-seven-target and heavy
E2E/VM/chaos/restore/move/mutation/cost work remains ROOT/peer-owned.
