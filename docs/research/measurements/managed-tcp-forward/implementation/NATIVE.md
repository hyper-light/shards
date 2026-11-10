# First actual native TCP cases

Policy cbd392/df2acf; implementer + assigned adversarial relay tester. The
shared implementation is **one** `crates/init/src/relay.rs`, imported by
testguest. Actual runtime caller remains `init::engine::relay`; its wiring
candidate is held separately, unsigned, pending old Unix/VSOCK observations.

Current interface:

```rust
fn pump(
    streams: [BorrowedFd<'_>; 2], slots: Slots, limits: [ByteLimit; 2],
    deadline: Control<'_>, end: EndPolicy, report: &mut Report,
) -> Result<Termination, Fault>
```

`Until(&Deadline)` retains the existing original absolute deadline, real worker
cancel FD and deadline-before-cancel precedence. TCP uses `Independent` halves;
actual source EOF drains its accepted tail before SHUT_WR. Native errors retain
leg, operation, kind and errno; socket ECONNABORTED is not controller cancellation.
The second caller's `ProcessOwned`/`EnginePeerEnd` branches compile but have **no
actual engine compatibility result** at this checkpoint. They describe the
existing engine's lifetime difference; they do not establish a new promise.

## Actual results and identities

On Fedora8fadc, Rust1.98.1/libc0.2.189, source projection
`~/relay-tcp-forward-cbd3-light-v5`, fresh1CPU/1GiB/2GiB:
policy0, GNU/musl library checks0 and GNU/musl cfg(test) builds0. The minimal
projection's exact manifests/lock are retained in [harness.json](native-v1/harness.json).
Its root allows dead code because only the selected native test invokes the
otherwise private guest fixture; this is **not** a full normal guest clippy gate.
No dependency/provider/crypto feature or syscall allowlist was changed.

Native run `~/relay-tcp-forward-cbd3-native-v1` completed0 under a separate fresh
1CPU/1GiB/2GiB reservation. Each of eight fresh processes ran this exact filter,
with1test passed and0failed:

```text
managed_network::intercept::tcp_forward::native_tests::native_tcp_pump_both_half_closes
```

| libc | Storage | Server-first | Backend-FIN-first |
| --- | --- | --- | --- |
| static musl | caller3/5-byte slots | 0 | 0 |
| static musl | minimum OS mapping units | 0 | 0 |
| GNU | caller3/5-byte slots | 0 | 0 |
| GNU | minimum OS mapping units | 0 | 0 |

Each backend independently read the exact13-byte request, each client the exact
14-byte greeting+tail. The server-first client reads greeting before completing
its request; backend-FIN-first sends the whole response and half-closes before
receiving the request. Both endpoints observe EOF and retain successful write
shutdowns. Both flow directions finish `WriteClosed(Eof)`, pending0/extra0;
actual upstream peer differs from actual client peer. All eight child FD checks
are4→4. Launcher holds the unreaped child's pidfd before its private release;
all eight become exit-ready and are reaped, parent FDs4→4. Every binary's
before/after SHA is identical. These are descriptor/owned-child facts;
kernel-cost return remains `Unknown`.

Primary static musl binary SHA256:
`6f32195fd21b60a69e38ca950f45a997d8cab65297d0f592cdec633c915153ef`.
GNU binary SHA256:
`368773742645eef2d46555fee2f13319e3c24b4a9712c3e689d6ce3af858a068`.
Shared source SHA256:
`d2d62359ef054e321e7e742b964a6267f77086ccd8b9f8cbe30012d33423feb2`.
All exact inputs, raw stdout/stderr, parsed byte/tuple/terminal observations,
pidfd/reap/FD/hash results and source identities are retained in
[native-v1](native-v1/CHECKS.json). Use `native_cases.py --binary ABS_BINARY
--inputs CASE/inputs.json --record NEW_PRIVATE_PATH` under fresh admission to
repeat one case; its child deadline is30000ms, parent45000ms. These are explicit
finite fixture inputs, not production capacities or timing thresholds.

The interrupted-job follow-up found the old baseline clean at6ab, unchanged
engine SHA and retained full engine test binary. All16 owned Cargo locks were
available. The eight actual pidfd/reap receipts and these limited lock facts are
in [cleanup-and-baseline.json](native-v1/cleanup-and-baseline.json). No deletion
or protected ROOT cache write occurred. This does not prove all-host emptiness.

## Retained failures and remaining gates

r41 failed frontend visibility; the narrow declarations were corrected. r51
library checks passed, but the private projection omitted an unchanged
`include_str!` fixture. r52 included that exact fixture and all four builds
passed. Its generated input contained a literal backslash-n, so the launcher
failed JSON parsing **before starting a socket child**. Corrected input-only
execution used those unchanged binaries for the eight actual cases. The
original logs/artifacts remain in light-v3/v4/v5. CHECKS records r52's malformed
result suffix and raw SHA rather than silently rewriting the original artifact.

No actual partial send was observed (`partial_writes=0` in these tiny cases).
Recorded native WouldBlock events do not establish the required controlled
backpressure campaign. Reset/cancel/prefix/extra-data and meaningful guard-removal
mutations, exact restoration, KVM interception and old Unix/VSOCK half-close
compatibility remain next work. The new host leaf/parent registration are not
part of these native passes. Broader mandatory routes, hard parent/kernel
charges, generation handling and normal guest/helper/matrix gates stay open.

R01/R04/R06/R07/R13: checked fixed tails, typed native/control faults, explicit
connect/half/terminal states; production lint/full caller checks still owed.
R02/R03/R05/R12: reviewed c584 study plus [RUNTIME.md](RUNTIME.md) primary source
identities; one shared pump, no grow/cache/timer/copy threads or new manager.
Two directional slots follow the algorithm; caller sizes/byte ceilings and
finite times are explicit. Minimum-mapping storage has native correctness data,
not measured production adoption or aggregate entitlement. R08/R14: no speed,
allocation/fault/leak or4–5× claim; ROOT retains profiling and benchmarks.
R09/R10/R15: implemented approved missing forwarding behavior, retained actual
independent bytes/terminal/owned cleanup and failures; mutation/broader live
coverage remains explicit. R11: actual Linux amd64 GNU/musl TCP only; no new
seven-target/VM/platform pass, macOS ARM64 only, paused WinARM untouched.
R16: existing worktree, admitted private remote paths, no local target/cache,
new full local worktree, source/evidence loss or protected cache mutation.

This is a progress checkpoint. Production extraction/wiring remains required;
ROOT owns final evidence review, combined validation and main integration.
