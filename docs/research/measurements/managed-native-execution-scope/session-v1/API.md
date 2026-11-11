# Actual lifetime acquisition and parent integration

This is implementation source, not an activated owner producer or a passing
kernel cgroup result. The original parent/primary files are unchanged. The sole
product leaf is `crates/init/src/domains/admission/execution.rs`.

## Shared parent dependency (UNAPPLIED)

Apply `PARENT-ACQUISITION-UNAPPLIED-v2.patch` to the exact parent admission source
SHA-256 `a5188b304b7c35668c2c035329269ba1437f675dcaa956fb2a035b4b810151d6`.
The resulting full parent source is `candidate-v2/admission.rs`, SHA-256
`de247243e4fae1ce2218b6263e23af8a1b412ad633e2ecfd2e5c2adff4a635ec`;
the patch SHA-256 is
`9b668c542cb6c74d7d06a33af0f32aa6f0b5254d104aab2b71c107ed661edd28`.

The new private `Group::acquire` uses the SAME component validation, fallible
allocation, parent duplication, mkdir, open and Removal implementation. It
returns `Created(Group)`, `Refused(Refusal)`, or
`Retained { refusal, removal }`. `Group::create` remains the existing
Refusal-only compatibility wrapper. `prepare_removal` and `open` split that same
acquisition, retaining its actual object on failed open plus failed rollback.
There is no second create/remove implementation. Three existing error types
become crate-visible for typed consumers. Constructors/fields stay private.
The patch includes only the `pub(crate) mod execution;` registration token in
addition to that shared acquisition change. It is not applied to the parent
worktree or this checkout's parent admission module.

The parent also owns the narrow re-export at `domains.rs`:

```rust
pub(crate) use admission::execution;
```

No fake parent/helper caller or `Launch` constructor was added. The exact
`managed::Controls` slice is retained in `before/controls.rs`, SHA-256
`1eb53ce882570d45d75e534886c1617c5a30025ae22f5870b9b18375d43e948d`.

## Callable leaf

```text
Prepared::acquire(&File actual_held_member, NonZeroU64 checked_Worker)
    -> Acquisition::{Prepared, Refused(existing Refusal), Retained}
Prepared::{job, directory}()              // actual held facts only
Prepared::publish() -> (Published, managed::Controls)
Prepared::cancel_before_birth() -> Cleanup::{Removed, Retained}
Published::{job, directory}()
Published::remove_after_reap() -> Cleanup
Retained::{job, directory, preparation_failure, removal_failure}()
Retained::retry_remove() -> Cleanup
```

The actual authoritative owner must check its original current Scope/Epoch,
process permission and selected object/audit/resource inputs before acquisition.
This leaf checks cgroup-v2 filesystem identity and exclusively creates a single
decimal component from the existing nonzero Worker ID under the held member.
It opens existing `cgroup.kill` (write-only) and `cgroup.events` (read-only)
through the same NOFOLLOW/CLOEXEC opener before returning Prepared. It performs
no control write, task migration, clone, signal, wait or event read. It neither
enables controllers nor sets limits/promises/quotas. There is one leaf per real
job, never per reported logical agent/depth.

The owner MUST call `publish` while it still holds the private object, **before
transmitting the Group FD to the helper or enabling any possible birth**. Store
Published and Controls in that SAME actual owner's job state. Send the held
directory from Published for atomic CLONE_INTO_CGROUP. The helper/parent's actual
pidfd/association/ACK/current/audit protocol remains its existing ownership, not
an API fabricated here. After any possible or uncertain birth, never call the
pre-birth cancellation path. Loss/unknown retains the Published scope, actual
pidfd, Controls and charges; it does not refund or blindly replay.

Before `remove_after_reap` or a post-birth retry, the actual owner must establish
both matching actual reap and kernel `populated 0`. The method only attempts
rmdir through the SAME held parent/name. Kernel removal is not a wait receipt,
an effect result or permission to release charges. Failures return the SAME
holding object and exact rollback code; Unknown/NotCreated is never success.

Prepared keeps the existing unused Group guard armed until publication. For a
known no-birth cancellation, use `cancel_before_birth` to obtain a typed removal
outcome; an implicit best-effort Drop is not a receipt. Published and Retained
guards are disarmed on Drop; neither silently retries unlink nor claims that
the job ended. The authoritative owner remains responsible for retaining them.

## Validation boundary

The isolated fixture compiles the exact resulting parent **production prefix**,
the exact Controls slice and the actual new leaf. Original parent tests require
the real Launch graph and are excluded from this projection. No Launch stub or
fake actual producer was introduced. It runs only the new pure/regular-file
cases. Regular-directory `finish`/Group calls are private test projections;
public acquisition refuses that filesystem before creating anything. They do
not create a fake cgroup or establish kernel population/reap/controller facts.
All projection unused warnings remain visible; whole actual owner graph strict
and integration are owed to ROOT/parent.
