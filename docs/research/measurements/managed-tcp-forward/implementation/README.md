# TCP pump implementation checkpoints

Current follow-up: [NATIVE.md](NATIVE.md) records the single shared extraction
and first eight actual native GNU/static-musl TCP passes, exact bytes/half-close/
pidfd/FD/hash witnesses and remaining gates. Production engine wiring is still
held separately for actual Unix/VSOCK compatibility. The record below describes
the first signed6ab checkpoint and its then-pending scope.

[Native follow-up](native-followup/README.md) adds actual pressure/short writes,
early held-backend completion/cancellation and wrong-byte mutation/restoration.
That checkpoint's engine compatibility case was still unrun. The
[reset/engine follow-up](reset/README.md) now records the actual native reset
mismatch/correction/mutation and first old-engine client-FIN-first KVM pass.
Old host-finish and production consumer wiring remain pending.

[Reverse reset](client-reset/README.md) adds actual GNU/static-musl client-origin
reset cases, each with direct and mediated independent backend error/prefix
witnesses and owned descriptor/child return. It changes no production pump bytes.

[Client byte boundary](extra-client-byte/README.md) adds actual GNU/static-musl
extra-byte refusal, actual independent extra-byte delivery with only that ceiling
removed, exact restoration and six restored native controls.

Base `cbd39284937d024640c56db6f8d8ef2bd9b0ee24`; reviewed design
`c584e877f56d97736e325eae2cb9fa2be97fdfc0`. Role: implementer and assigned
adversarial relay tester, Kiln `01a1c5d2ee0e7248982c95a59f1fd905`; ROOT
`01a148884a4770fdaf0111f3f792a248` retains audits, profiling, benchmarking,
combined validation and main merges. Full current policy/plan/handoff readback
preceded code. Rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.

The first code piece is a real native socket pump, not a response synthesizer:
`tcp_forward::forward(listener, upstream, destination, slots, limits, deadline)
-> Completion`. It has explicit connect/half/terminal states, fallibly reserved
fixed directional slots, partial native sends, nonempty EOF probes, drain-before-
SHUT_WR and one joint readiness wait. Both directions advance without waiting
for a complete request. Each attempt retains the original Deadline admission;
actual native errors remain distinct from cancellation/expiry. The existing
Worker now has a typed completion parameter and one shared spawn/cancel/join
implementation. No second manager, growing queue, copy thread, runtime or cache.

The new parser, independent endpoint scripts, host leaf, unapplied host
registration and actual native/KVM campaigns are still being implemented. This
is a code progress checkpoint, not a claimed forwarding/cleanup/matrix pass.

## Production caller and the single implementation to extract next

The exact existing init byte-relay caller is `Standby::start` in
`crates/init/src/run.rs:2555`: at line2764 it calls `engine::start`, then hands
the held listener to `Relay::give`. In `crates/init/src/engine.rs`, `serve`
invokes `relay(conn, token)` at line115, which authenticates the host VSOCK
connection with the existing HELLO/token and presently uses two `io::copy`
directions. Its Unix/VSOCK pair is a real second native-socket consumer for the
same borrowed-FD pump. No authentication/HELLO change is proposed.

Next production extraction must move this implementation, its fixed-slot and
absolute-deadline admission interface to one Linux guest shared module, proposed
`crates/init/src/relay.rs`, with testguest path-including that same source.
`engine::relay` then calls that one pump after its existing handshake rather
than gaining a second forwarding implementation. This checkpoint does not edit
init outside its assigned paths; extraction and caller wiring need the next
concrete ownership assignment and source-matching runtime evidence.

Internal granted TCP is a different existing caller chain:
`domains` network setup at `crates/init/src/domains.rs:1307` calls
`links::Switch::new`, installs its current grant policies, and attaches per-domain
links/gates. It currently routes packets; it has no native guest TCP pump caller
to name as already implemented. Installing managed interception/flow admission
there must preserve that controller, default-deny grants and fallback DROP.
The initial one-flow experiment does not settle mandatory all-route coverage,
native backend source presentation, aggregate/kernel charges or generations.

## Bounds, witnesses and pending checks

Initial light check scope: new private Fedora
`/home/adalundhe/relay-tcp-forward-cbd3-light-v1/{source,check,target,cargo-home}`,
runner `8fadc1ec213332e16cb0d915a3fb913f`, fresh Kiln admission
1CPU/1GiB/2GiB, no repository sync or ROOT cache access. No LOCAL target/cache.
The formatted core and narrow Worker integration compile0 for GNU and musl,
with pinned Rust1.98.1/libc0.2.189; formatting and policy checks also pass0.
[CHECKS.json](CHECKS.json) retains commands, logs, identities and the exact private
source/target. No native or test operation ran in these check-only compilations.

The subsequent private full caller/KVM scope is
`/home/adalundhe/relay-tcp-forward-cbd3-v1/{source,target,cargo-home,record}`;
each run requests fresh admission for 4CPU/8GiB/20GiB. Caller supplies VM1vCPU,
256MiB, original operation30000ms, boot120000ms and finite outer timeouts.
Native bytes, per-connection tuples, each half-close, committed prefixes,
actual cancellation/reset results and acquired-owner cleanup require independent
backend/client witnesses. Partial-write coverage requires actual short writes or
WouldBlock, not a guessed buffer allowance. No tests are yet reported passed.

R01/R04/R06/R07/R13: checked tails/arithmetic, fallible reservation, typed
states/errors and retained native effects are implemented but still need compiler,
caller and transition validation. R02/R03/R05/R12: reviewed c584 primary sources
and alternatives apply; slot/byte/time limits are supplied test inputs, no
production size, polling timer/cache or new manager. R08/R14: no performance,
allocation/fault or comparative claim; ROOT owns those measurements. R09/R10/R15:
this builds the approved missing behavior, not a speculative production bug fix;
actual red/green/mutation/restoration and owned cleanup remain required work.
R11: no new target/native/VM pass yet; all seven targets and unavailable hardware
remain explicit obligations, macOS ARM64 only and WinARM stays paused. R16:
existing local worktree reused, remote paths private/admitted, no source/evidence,
ROOT caches, final artifacts or paused machines deleted or mutated.
