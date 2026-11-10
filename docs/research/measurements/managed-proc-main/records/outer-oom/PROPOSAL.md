# Narrow propagation proposal — not selected or implemented

Scope renewed under f5/d634 POLICY_READ implementer after preserving/pushing
[actual r222 reproduction](README.md) at `ababb39ad3a6153542bf1f739def33582b3e0946`.
The test source6db, executable and remote evidence remain frozen. This note is
read-only inspection of specific callers for root's propagation/design review;
no new run, production edit, error framework or attribution stack was added.

## Actual route and affected paths

References below are against6db (the production bytes are unchanged from1cbd).

| Path/symbol | Actual behavior and proposed narrow boundary |
| --- | --- |
| `crates/api/src/taken.rs:33`, `Held::find` | Sole internal operation being migrated: open held process → read start → compare expected/observed start → check held exit → return held process. Each error currently formats into String. Return a typed domain failure with the same checked ordering/handle ownership. |
| `crates/api/src/client.rs:89`, Unix `take_back` | Only production Held::find caller found. Its map_err only moves the existing String into Gone, without a second format. Replace only that wrap with a public typed Refusal variant. Leave later success Arc/thread/clone/lifecycle setup alone. |
| `crates/api/src/client.rs:71`, `Guests for Shards::attach` | Existing `take_back(identity,spec)?` runs before the outer Box::new. A typed early failure can reach the existing public Refusal result; no API signature change or new public operation is needed. This does not prove the entire attach path allocation-safe. |
| `crates/api/src/lib.rs:186/219/291` | Refusal variant/cause type, Display/Error implementation and attach's documented failure contract. Keep existing variants rather than changing Gone.why's String type throughout simulator callers. |
| `crates/api/src/conformance.rs:345/377`, `client.rs:1510/1519/1651` | Existing wrong-start/ended attach checks expect Gone. A new typed alternative needs exact semantic assertions, including PID/start/stage; do not make OOM/unknown observation satisfy a gone assertion. Simulator/Windows legacy Gone branches can stay unchanged. |

The workspace Refusal exhaustive production match found is its own Display.
The external workspace consumer is shards' test-only API/conformance dependency;
real/simulator test callers use matches or public conformance. External consumers
are unknown. This is a specific consumer inventory, not a broad reporting audit.

## Minimal typed route and the compatibility decision

Proposed public envelope, subject to root selection:

```rust
Refusal::Process {
    pid: u32,
    expected_started: u64,
    failure: ProcessFailure,
}
```

Domain cases are **Open**, **ReadStart**, **CheckExit** with their actual I/O cause;
**DifferentStart { observed: u64 }**; and **Exited**. A different observed start
comes only from the actual comparison, Exited only from the held process's event
check. An I/O failure, especially allocation/permission/observation failure, does
not become a proof of gone or an authority decision based on errno. The envelope
keeps the affected PID and expected start; each domain case identifies the stage.

Two constrained representations need an explicit coordinator choice:

1. **Own each actual io::Error directly in the domain case.** This is the smallest
   complete-cause migration: moving an already-created error requires no new Box
   or String, preserves OS code and any actual custom payload, and can expose the
   original cause through Error::source. The obstacle is public compatibility:
   io::Error implements neither Clone nor PartialEq/Eq, while Refusal currently
   derives all three. Removing those traits is an additional public breaking
   change, not an invisible implementation detail. No shared-owner allocation or
   hand-written lossy clone/equality substitute is proposed.
2. **A closed cause model at these specific producers, retaining those traits.**
   Preserve native OS kind plus raw OS code, and explicitly type the actual
   non-OS causes (reader allocation refusal, invalid start data and concrete
   conversion failures). This must be defined from these producer contracts;
   a generic kind/errno snapshot of arbitrary io::Error is not lossless and is
   not proposed as an authority representation. Existing conversion-error boxing
   prevents calling this alternative complete without additional reviewed scope
   and exact error-path evidence. It is not approved for implementation here.

The established standard error representation itself distinguishes OS, simple,
static-message and custom-payload causes; raw_os_error is absent for custom
payloads. Pinned [Rust1.98.1 core I/O error source](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/core/src/io/error.rs)
supports that distinction. The pinned
[alloc I/O error implementation](https://raw.githubusercontent.com/rust-lang/rust/48a229ceaefd4985c50990b14116b6d856af0985/library/alloc/src/io/error.rs)
shows why Error::other is an allocating construction rather than a safe OOM
propagation mechanism. These sources motivate the compatibility decision; no
new generic error library, thiserror, Box or bespoke refcount is justified.

Keeping Gone's fields/traits plus formatting a domain failure back into its String
would recreate the demonstrated failure. An empty String, format-later bridge or
allocation-only variant that leaves the same error-string construction in place
is not a complete proposed migration. The control result must already own typed
scope/cause at the Refusal boundary. Human Display is a separate presentation
operation and cannot be used as the propagation representation.

Adding Process to current exhaustive Refusal also breaks downstream exhaustive
matches, even if Clone/Eq remain. The
[Cargo semver guidance](https://doc.rust-lang.org/cargo/reference/semver.html#enum-variant-new)
classifies that change explicitly. Native attach failures moving from Gone to
Process change caller matching behavior and report formatting; old Gone variants,
simulator and Windows branches need not change. Dropping public traits, if chosen,
would additionally break clones, equality assertions and trait-bound users.
No trait/version/compatibility consequence is waived by the current0.1.0 version.

## Allocating boundaries and unproven limits

- `ShardsGuest::identity` at `client.rs:891/900` separately calls started and
  formats its error into Gone. This is a source observation, not a live identity
  reproduction from r222; do not silently fix it under this Held::find result.
- Linux `taken::linux::Held::open` has existing io::Error::other wrapping for
  pid_t/c_int conversion failure; macOS started has a similar pid conversion.
  A custom cause can be created/abort before a later typed wrapper sees it.
  The valid-own-TGID reproduction does not exercise those conversion failures.
  This proposal does not replace them based only on theory.
- Attach's valid-path success setup still uses Arc, thread naming/spawn, path/port
  clones and the public Box. Invalid-spec reporting before take_back also remains
  outside this specific fix. No whole attach or library panic-free claim follows.
- Conformance/reporting callers format Refusal into their own Failure strings;
  preserving a typed API result does not certify their OOM presentation behavior.
  No such caller is executed by the focused r222 test.

The exact28B real Held::find abort, prior inner std-reader32B abort and historical
Mac owned OnceBox64B abort are separate outcomes. No stack from this batch assigns
the28B request to a particular formatting instruction. Root's independent error
audit/selection, including identity evidence if desired, remains authoritative.

After selection/ownership approval, the minimum light proof should reach the
existing real Held::find refusal test and the public attach error boundary,
preserve phase/PID/expected start/cause, verify the held descriptor closes on an
ordinary returned error, and keep normal wrong-start/ended semantics precise.
Further mutations, allocation-site traces, matrix, full corpus, costs and final
adoption remain root-owned. None is claimed complete by this proposal.
