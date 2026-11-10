# Native backend reset propagation and first actual engine half-close

Policy cbd392/df2acf; implementer + assigned adversarial relay tester. This is a
scoped correctness checkpoint, with ROOT retaining review/integration, audits,
profiles/benchmarks and main. No rebase of active proof sources, no child/goals
wrappers, no held helper campaigns or reserved root D229 work.

## Reproduced reset mismatch and narrow correction

r82, private `~/relay-tcp-forward-cbd3-light-v8`, passed GNU/musl library and
cfg(test) builds0, then static-musl strict reset test101. Its direct and mediated
conversations independently delivered the same13-byte request to their actual
backend, and8-byte greeting to their actual client. Both backend write halves
were held open after their workers joined. The fixture requested native
abortive close on each owned backend socket. Direct client observed native
`ECONNRESET`; the pump also retained native backend/read `ECONNRESET`, but the
mediated client observed clean EOF. Both route FD checks4→4 and parent
pidfd exit/reap/FD4→4 completed before the strict mismatch assertion.

The correction is at the existing TCP owner `run_accepted` after the one shared
pump returns. An actual native `ConnectionReset` requests zero `SO_LINGER` on
the opposite owned TCP leg before its descriptor closes. Original `Fault`
remains intact. Typed `Abort::{NotRequired,Requested,Failed}` preserves the
target leg and any native option failure; option acceptance is a request, not a
claim that a peer received a reset. There is no new application I/O, retry,
allowance, grant, syscall allowlist, timer, cache, framework or second pump.
Cancellation/expiry and normal EOF do not select this finalizer. Already
released bytes cannot be rolled back.

The [pinned Linux primary source](https://github.com/gregkh/linux/blob/v6.18.48/net/ipv4/tcp.c)
`__tcp_close`, blobdb80b81a74cbb759791949ce34f1088ca8c54671, distinguishes
zero-linger abortive close from a drained normal close's FIN. The native linger
flag1 and delay0 select that protocol behavior; they are not a runtime sizing
or performance choice. [SOURCE.json](SOURCE.json) records the checked interface.
Putting a universal abort in the borrowed-FD pump would also change Unix/VSOCK;
packet injection would require extra transport machinery/authority. The actual
TCP owner is the smaller existing seam and leaves the shared pump unchanged.

r84, NEW private light-v9, passed GNU/musl library and cfg(test) builds0 and
two exact native reset tests0, one per libc. Each compares a direct baseline
with a mediated flow; both actual receiving clients now observe
`ConnectionReset`, with original13/8 prefixes intact. The actual kernel/client
result supplies the effect proof. Descriptor returns, held pidfd exit/reap and
binary before/after hashes pass; kernel resource return remains `Unknown`.

r87, NEW private `~/relay-tcp-reset-mutation-v1`, removes **only** the new
finalizer call. Strict101 returns with the same actual direct-reset versus
mediated-EOF mismatch and retained independent prefixes; both route FD checks
and parent pidfd/reap/FD return complete. `finally` restores TCP-owner SHA256
`bbdd8c9af3291872744775a40e62675677aa5021003fa6ba3329be89bfddc12c`,
all copied source hash differences empty and named-file `git diff --no-index`0.
This is a private projection, not an empty HEAD-diff claim. Frozen mutant
`390cc99e22e9194f9abc5bab2d52a64ed6acf4d9bd97429506f08cab5349ab15`
is preserved. Restored static-musl binary
`5df0f79e030e4266058602ec9866f03d5b1dc0492b71d508535af15aff43f675`
then passes reset, both half-close orders and receipt-cancel in four fresh tests.

Client-origin reset, option-failure behavior, further partial-prefix races,
KVM reset interception and all-route semantics remain unverified. This is not
a universal TCP compatibility, cleanup, hard-budget or performance claim.

## Actual production caller baseline

Normal old-engine host/helpers and full static-musl testguest built0 in r61,
private `~/relay-tcp-engine-compat-6b8-v1`, exact6b8d+owned probe composition.
The unchanged old engine source SHA256 is
`e2e4e6e6ae014ddb82aba96362801bd14bf4a69a2955ac44941bd481f6563eea`.
Host registration is only a NEW private test root plus the separately supplied
unapplied patch; the shared host parent is untouched.

Initial actual attempts with process AS limits failed before VM readiness at
existing layer/RAM/DAX virtual reservations. No engine probe ran. These are
retained **HARNESS** outcomes, with unchanged source/binaries and strict tests;
they are not reproduced engine defects or reasons to change product mappings.
One preflight regex mistake also ended before any actor. ROOT subsequently
authorized ONE ordinary finite normal launch without `RLIMIT_AS`, matching its
actual normal caller and sparse RAM/DAX reservation behavior.

That one client-FIN-first KVM case passes0 with1test passed, using guarded
2CPU/4GiB/24GiB admission, requested VM1CPU/256MiB, fixed56-byte GET request,
30000ms operation/120000ms boot/240000ms parent bounds. The actual old engine
returns the242-byte HTTP response ending `OK` after client SHUT_WR, followed by
actual response EOF. Guest worker joins, guest FDs4→4, owner daemon stop0,
parent held pidfd exit/reap and FDs9→9, unchanged binary hash. Admission is not a
hard cap; physical/kernel resource limits and complete population return are
explicitly unverified. Automatically emitted timing stderr is retained raw and
not used for a performance claim.

The approved one-case method was consumed by that pilot. The concrete next old
host-FIN-first/late-write case is prepared and its normal-launch extension was
requested from ROOT. Production `engine.rs` shared-consumer wiring remains
uncommitted until that host-finish behavior is measured. There is one new pump
at `crates/init/src/relay.rs`; no alternate production implementation is added.
Engine before/after, ordinary KVM forwarding, strict normal caller/clippy and
combined matrix still remain required. Missing route classes remain mandatory
open work.

## Evidence and rule accounting

[CHECKS.json](CHECKS.json), `before`, `after`, `mutation_cases`, `engine-normal`
and `engine-as-failures` retain exact source/build/binary identities, raw input/
stdout/stderr/observed results, actual receiver errors and descriptor facts.
The exact submitted command bodies are under `commands`; existing paths refuse
overwrite, and any reproduction needs a NEW owned path and fresh admission.
Frozen binaries/source and full original logs remain in the private paths.

R01/R04/R06/R07/R13: fallible native setup, typed reset request/failure, original
cause and progress retained, one existing TCP owner; normal updated caller and
full lint still owed. R02/R03/R05/R12: pinned primary plus actual differential,
existing seam, protocol-defined zero linger, explicit finite caller bounds,
one pump/no manager/cache/poll timer. R08/R14: no comparative/allocation/fault/
latency claim; ROOT profiles. R09/R10/R15: actual red before correction, green,
meaningful guard removal/restoration, independent byte/error/limited cleanup
witnesses and unrun gates explicit. R11: native amd64 GNU/musl and one actual
amd64 KVM baseline only; other host/guest targets unverified, macOS ARM64 only,
paused WinARM untouched. R16: admitted private remote scopes, no local builds/
new full worktrees or target/cache, no protected root or other-source mutation,
no source/evidence/final binary deletion. Full management closure remains OPEN.
