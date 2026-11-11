# Event-driven readiness for the context-reuse regression

Completed finite R05 test synchronization change, **cfg(test) only**. The
existing regression now parks for notifications instead of sleeping/polling
every1ms. Production bytes before `#[cfg(test)]` are unchanged. All original
phase predicates, the explicit5s transition guard, held-head8 assertion and
used-head assertions remain. This is new test synchronization work, not a new
product defect or first-late-after-destroy reproduction.

POLICY_READ current main `749c3ca0602c43dac74976e8db713102f5b15874`, full
AGENTS/R01–R16/evidence/CLAUDE/current plan/latest handoff; rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Implementer01a1c76784eb72ccaf9096c88182efb7, ROOT01a148884a4770fdaf0111f3f792a248.
Branch `p/managed-virtio-gpu-event-wait-v1` starts at the exact tested leaf
`5b13458e19b32f233792ea5fe6e1a754243c6c89`. Its mod.rs preimage matches ROOT's
frozen candidate `8aceafd2cd9b9c62319ec0e06423e5d33d7f59b5` byte-for-byte:
SHA256 `68f22fdbce595b0b14624a40a5140754a05b4fbb18d43c2e000ee62357afa15e`.
[source-inputs.json](source-inputs.json) and [baseline-mod.rs](baseline-mod.rs)
freeze this identity before edits. ROOT candidate and L2 files were not edited.

## Notification and lock contract

Primary source is the actual installed Rust1.98.1 compiler commit
`48a229ceaefd4985c50990b14116b6d856af0985`:
[park/park_timeout contract](https://github.com/rust-lang/rust/blob/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/thread/functions.rs#L405),
[Thread::unpark](https://github.com/rust-lang/rust/blob/48a229ceaefd4985c50990b14116b6d856af0985/library/std/src/thread/thread.rs#L124).
An unpark token remains available across a check-to-park race; tokens coalesce,
and a wake can be spurious, so the predicate must be rechecked. The primitive
provides release/acquire synchronization. Its source warns against intervening
unrelated parking, including logging. Retained files/hashes and source-fetch
limits are in [source-research.json](source-research.json).

The test registers its observer Thread in the existing Log **before activation**.
It adds no readiness counter, condition boolean, channel or event queue:

- Fake::note pushes the real call under the Log mutex, then unparks the observer.
- Fake::signals drains actual queued signals and updates its existing
  drained_signals field under that same mutex, then unparks. Empty drains add no
  notification. Other tests default to no observer.
- A test-local interrupt line unparks that same observer. The production worker
  publishes used entries before requesting the interrupt (round:630–633).
  DeviceInterrupt::used_buffer always raises its line (virtio/mod.rs:95–98).
  The test Ring constructs queues with VERSION_1 only, so EVENT_IDX is disabled;
  zero avail flags request interrupts (queue.rs:468–478).

Each original predicate reads only the existing Log or actual used ring. Its
guard is released before parking; no Log or GuestMemory Access guard spans the
wait. Log publishers hold only Log while notifying; IRQ notification acquires
neither guard. GuestMemory::access uses a separate mutex and drops its guard when
used_idx returns (memory.rs:588–602,1070–1081). No new cross-lock ordering is
introduced. The pinned Darwin std mutex uses pthread locking, not this Thread's
parking token; the predicate-to-park path contains only the existing deadline
check and Instant call, with no logging/callback. Unpark before the check makes
the actual state observable; unpark after a false check supplies the token to
the ensuing park. Spurious/obsolete notifications only cause rechecking.

The retained5s end is created once per original wait. park_timeout receives its
remaining duration, never a renewed interval. Platform scheduling/timeout
precision is not a measured latency guarantee. The legacy worker test's separate
wait is unchanged; this slice claims R05 completion only for the NEW regression.

Alternatives: a Condvar requires its predicate/notifications to share a mutex
([official contract](https://doc.rust-lang.org/stable/std/sync/struct.Condvar.html)).
Here used-ring publication and Log updates have separate owners, so that option
would require additional synchronization at the IRQ seam. A channel would add a
separate notification queue. The already used Thread park/unpark mechanism
serves these two real notification sources with one observer handle.

## Actual native validation and cleanup

[run.py](run.py) was normally admitted on this Mac e30189ea29182644fe5a4b2e06c7dac2,
private2CPU/2GiB/4GiB/600s/-j2, under `/tmp/g121-mac.lock`. New target:
`target/managed-virtio-gpu-event-wait-v1`; no old target reused. The original
product Cargo.lock remains SHA256
`1ecb41e738c4c4133a1968873e424efce5b2a65899dd8f40f37ac6aaffec8133`.

| Actual stage | Result |
| --- | --- |
| Policy / checked rustfmt |0 /0 |
| Cargo --locked -j2 VMM lib --no-run |0; actual executable found from compiler JSON |
| Existing hvf-run test listing/signing |0;14 GPU tests listed |
| Frozen binary native GPU corpus |14 passed/0 failed/0 ignored |
| Full VMM lib/tests Clippy with -D warnings |0 |

The normal runner signs the built test executable during listing; its exact
signed bytes were copied outside Cargo output before execution. Frozen binary
`target/managed-virtio-gpu-event-wait-v1/frozen/shards-vmm-gpu-tests`:
18,506,928B, SHA256
`b625af811ce0063d80ee714be90f75c3a58d215787bb25667065fe3c4b4e5692`.
Compiled mod.rs SHA256
`084920513d4689ab7683f2f1d027d653da08d7ccba98eb2394b07c2ab85d6352`.

[run.json](run.json) records all eight commands/PIDs, exit0, each direct child
actually reaped, and controller FDs10→10 at every stage. The GPU corpus passed
after the test's unchanged reset/join path returned. Wider population/kernel
resource return is Unknown, not leak-free proof. Free space stayed above30GiB
(first48,373,846,016B; final47,878,705,152B). Source/lock/original286manifest hashes
were checked between stages. No runtime retry occurred.

Actual regression stdout remains:

```text
context_id=1 old_signals_drained=1 before_new_signal=[0, 2, 4, 6] after_new_signal=[0, 2, 4, 6, 8]
```

It still tests an old pending fence whose callback queues during destroy before
return. A first callback after destroy returns is distinct/unverified. No
physical renderer, VM, driver, restore, fault/mutation or benchmark ran. Other
platforms/new combined ROOT gates remain unrun for this test delta. Original
queued-case red/green/raw and the original286-entry manifest are untouched.

Only the test delta [test-only.patch](test-only.patch), new evidence and its own
manifest are returned; old source/closure data is not restaged. Current own
signer is absent: ROOT's normal signer receives the exact tested unsigned
selection, without borrowing a socket/key or disabling signing.

R01/R06/R07/R13: cfg(test)-only focused notification change; production prefix
byte-equal. R02/R03: pinned primary contract, actual caller review, original5s
bound; no production literal. R04/R05: authoritative predicates and notification
order above, no duplicate readiness state/cache/poll. R08/R14: no performance
claim or profile. R09/R10: explicitly new work, original proofs preserved, actual
new native results/raw/source/binary retained. R11/R12: portable std/test seam,
nativeMac only; other target gates ROOT-owned. R15/R16: admitted scoped tests,
no children/main merge/held effect/cache cleanup or headroom bypass. No jobs remain.
