# Safety candidate: Rust API migration

These source changes apply to the current-main safety candidate based on
`66dc1d2cae7d6cf0db234b31b20b36a79dd3b0d9`. They preserve typed failures at
the affected startup and Unix process-identity boundaries. They do not establish
whole-library allocation-failure safety or complete the managed-runtime plan.

## VM startup: propagate StartError

Both public functions change their error type:

| Function | Previous error | New error |
| --- | --- | --- |
| `shards_vmm::vm::start(&Config)` | `String` | `vm::StartError` |
| `shards_vmm::vm::run(&Config)` | `String` | `vm::StartError` |

Update caller return annotations and `?` propagation together. For an existing
caller that starts or runs a VM, the resulting signatures are:

```rust
use shards_vmm::vm::{self, Config, ExitReason, Handle, Running, StartError};

fn start(cfg: &Config) -> Result<(Handle, Running), StartError> {
    vm::start(cfg)
}

fn run(cfg: &Config) -> Result<ExitReason, StartError> {
    vm::run(cfg)
}
```

`StartError::CommandLine(error)` retains the real x86 `BootError` from command-line
composition, including `DoesNotFit { what, at, len }`. Match the variant to inspect
that cause. `StartError::Legacy(message)` owns an existing startup String;
`From<String>` moves it without rendering another message. It is not a conversion
of the command-line error back into text. Keep the enum through internal callers
and render through `Display` at the presentation boundary. Reintroducing
`.map_err(|error| error.to_string())` reintroduces an allocation at the failure.
Transparent `Display`/`Error` handling avoids duplicating the same cause in a
source chain; the typed command-line payload is available in the enum variant.

Existing examples: [`vm_run::start` and its supervision callbacks](../../crates/shards/src/vm_run.rs)
carry `StartError` to presentation; the
[lifecycle consumers](../../crates/shards/tests/lifecycle.rs) use both public
functions. The test-only panic assertions there are not production error handlers.
VMM `restore`/`restore_from` retain their existing String error signatures. Shared
CLI supervision moves those existing restore errors into `Legacy`.

## Refusal: add the Process match arm

Exhaustive matches on `shards_api::Refusal` must handle this additional variant:

```rust
Refusal::Process {
    pid,
    expected_started,
    failure, // ProcessFailure
}
```

Unix `Shards::attach` uses it when it cannot hold or observe the requested process
identity. Retain the original refusal, requested PID/start pair and typed failure;
do not replace it with a display String or a generic `Gone`/`Ended` outcome.
`expected_started` uses the same native units as the supplied `Identity::started`.
`Refusal` and `ProcessFailure` retain `Clone`, `PartialEq` and `Eq`.

| Typed cause | What it establishes |
| --- | --- |
| `DifferentStart { observed }` | A different native start was observed for the requested PID. Check the requested identity and observed value together. |
| `Exited` | The held process was observed exited; this does not supply its parent's exit status or a guest `End` value. |
| `Os { operation, code }` | The native observation stage and error code. The code alone is not a terminal guest state. |
| `CapacityUnavailable`, bounds/record errors | The observation could not be completed. Capacity refusal does not establish host exhaustion or process exit. |
| `UnrepresentedIo { operation, kind }` | An I/O representation without a native code; its payload is not represented, so uncertainty remains. |

The existing
[`a_guest_its_client_lost_is_taken_back_by_attach` consumer](../../crates/api/src/conformance.rs)
matches `DifferentStart` only with the requested PID, expected start and original
observed start. Its post-stop cases additionally have a separately observed stop
and qualified native operation. Those test-specific checks are not a general
`errno == ESRCH` rule for declaring a guest ended. The
[native attach consumers](../../crates/api/src/client.rs) retain the same identity
fields and explicit failure variants. Do not add polling or blind retry to hide
an uncertain refusal.

Simulator `Gone`/`Ended` behavior remains available. Windows owning attachment
is unchanged: its VM's job ends with the client that booted it, so `Shards::attach`
returns `Gone` rather than taking ownership of a surviving guest.

## Remaining limits

The Unix borrowed-identity change does not migrate every owned-client or legacy
String error path. The observed cold macOS owned-client mutex allocation failure
and the separate cold standard-stderr allocation failure remain unresolved.
Rendering a typed failure therefore does not by itself establish safe reporting
under allocation refusal. The native C reporting prototype is unadopted: root
observed mixed Rust/C output interleaving even though it survived Rust allocation
refusal. Warming, substituting text or adopting that prototype is not part of
this migration.
