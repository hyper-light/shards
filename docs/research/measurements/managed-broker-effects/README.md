# Managed broker effects: canonical output caller

Implementer and assigned adversarial tester: `/root/terminal_order_fixture`.
Policy: canonical `cbd39284937d024640c56db6f8d8ef2bd9b0ee24`, full
`AGENTS.md`, R01–R16, evidence, CLAUDE, plan and current handoff read;
rules SHA256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Production source: exact `3b1967838b140466ff3367637541492a77be0459`.
Own only the new test leaf and this record. Registration is **unapplied** here;
only the disposable native source applies `register.patch`. No production fix.

The root's retained r751 legacy test failed101 because it expected rewritten
`[agent a]` lines on main stderr. Its ready predicate passed, but it did not
verify the tool results. Preserve `~/root-managed-program-admission-kvm-v1`;
this new case does not waive or silently migrate that failure.

## Caller and observations

One fresh process/one real KVM VM, using the existing `Fixture`, private owner
Identity, metadata, `Observer`, QUIC driver and canonical ABI `StreamState`.
Files-only declaration0 remains inactive, helper AGENT1 and driver HARNESS2
retain stable IDs. All four member/channel subscriptions use actual authenticated
scope and contiguous wire offsets. Raw wire records and payloads are retained.
ACK receipts mean enqueued consumed controls; they are not invented durable audit.
The envelope authenticates tenant/channel, not a logical tool execution. The
controlled native testguest reports actual server RPC results into that stream;
these are client-reported result bytes checked against the executed fixture, not
new supervisor-created per-execution receipts or proof against a tenant forging
its own output. No per-PID/per-tool attribution is inferred from inherited fd1/fd2.

The helper calls the real server for own identity and native QuickJS programs:
success, thrown error, finite timeout, stranger/own-session cancellation,
session abandonment and a final successful program. `ATTACH helper FOR driver`
creates an actual peer channel with driver→helper request authority only.
Helper's code-mode send to driver must refuse. Driver's existing `hear` verb
blocks on a private host TCP connection until the owner has verified helper
canonical results and independent main exec progress; its subsequent actual
`srv-receive 0` must return an empty inbox. Driver also runs its own successful
program and verifies the real reverse-direction peer capability. This is an
independent recipient observation at that causal cut, not a sender-only mutation
proof: the recipient also independently filters request kinds.

One actual `top`/guest `/proc` snapshot after both markers/main progress must
show two ready actors, two servers, two sandbox helpers and zero named native
program workers. No repeated readiness probes or new sleeps. The reused testguest
cancellation and abandon verbs contain existing300ms sleeps: these are disclosed
helper timing, not readiness evidence. Zero stderr data is only as of the receipt
cut; no End or future-zero claim is made. A point process/FD observation does not
prove full population, wait status, allocation, peak or kernel resource return.

The case uses original caller-supplied wholecase and per-call deadlines, reserving
cleanup **inside** the original case bound. Parent startup shares one per-call
budget. The listener is ready before actor release; its one accept uses a bounded
readiness event (`poll`, retry EINTR only), never periodic state probing. No extra
manager, cache, worker protocol, observer implementation or network success model.
The per-call budget applies to host startup/capture/control/readiness operations.
The frozen testguest `Session` sets no Unix-socket deadline: a stalled guest RPC
is bounded externally by the host capture deadline and owner stop, not by a
fabricated per-RPC timeout in that helper. The explicit timeout-mode program has
its actual server deadline; abandon's infinite program depends on ownership.
On assertion failure, the same owner stop/observer join runs before rethrowing;
cleanup failure/deadline remains unknown. The outer caller bounds its fresh test
process group; that alone cannot certify an escaped daemon or VM was reaped.

## Research and alternatives

Primary sources inspected 2026-11-10:

- [Linux man-pages poll contract](https://man7.org/linux/man-pages/man2/poll.2.html):
  readiness wait, timeout, EINTR and error flags. One listener needs no new epoll
  registry; its poll deadline is derived from the supplied original call budget.
- [Rust TcpListener1.99.1](https://doc.rust-lang.org/std/net/struct.TcpListener.html):
  OS-selected port, ready listener, nonblocking accept and Drop. Drop's native
  close return is unobserved; actual harness hear output witnesses delivered gate
  bytes. Blocking accept alone lacks this caller's deadline; a server thread would
  add an unnecessary join/ownership path, so neither is used.
- [Linux /proc documentation](https://www.kernel.org/doc/html/latest/filesystems/proc.html)
  and [Linux v6.18 proc implementation](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/proc/array.c):
  process identity/state snapshots have limited provenance; names are not per-tool
  authorization or proof of future emptiness. The existing top seam is reused.
- [MCP2025-06-18 cancellation](https://modelcontextprotocol.io/specification/2025-06-18/basic/utilities/cancellation):
  cancellation can race completion. This fixture checks current Shards typed RPC
  results; it does not turn cancellation into rollback or generic exactly-once.

Existing alternatives inspected at source3b196: legacy `build.rs` code-mode test,
`managed_output/tenant_exit.rs`, `testguest::confined`, init netplan channels and
server Programs/call dispatch. Existing `run_watching` periodically probes/sleeps;
the new caller instead waits on canonical output and the explicit host gate, then
takes one process snapshot. No production abstraction decision is introduced.
The frozen parent retains its existing digest-named E2E executable cache and the
shared QUIC driver's readiness/state; this fixture adds no cache or alternative
state owner. Their full performance/allocation audit remains root-owned.

## Bounds, recipe and status

Approved private scope on Fedora8fadc:
`~/native-managed-broker-effects-3b-v1/{source,target,evidence}`.
Kiln admission2CPU/4GiB/24GiB, normal build deadline3600s. These are operational
campaign envelopes, not hard cgroup guarantees or measured production constants.
Case180000ms, percall120000ms, cleanup reserved15000ms, timeout300ms,
cancel workload1500ms, capture65536bytes per channel, outer240000ms and
outer reap15000ms. Case count1. Values are explicit caller-selected test budgets;
300ms timeout exercises the existing finite timeout presentation,1500ms exceeds
the reused cancellation helper's300ms delay. No production values change.

Build normal carried VM/net/init/server binaries, the managed_output test and
actual static testguest from this exact source; never `SHARDS_HELPERS=skip` for
the native case. Apply only the registration patch in the private source.
Use pinned Rust1.98.1 and zig0.16.0, `CARGO_BUILD_JOBS=2`, a private target;
`SHARDS_TEST_GUESTS` must point to the matching actual guest artifact directory.
Normal compilation **passed0** in actual r70: full carried VM/net/init/server
and `managed_output-87d4844abdafeb5c`,14m29s. Exact leaf SHA256
`74d459d44f11f5bbe4dfe46611aa35c7e276c150001c7d6f198456b0adeabcaf`.
Full raw log remains in the owned `evidence/test-build.log`; the exact submitted
command/initial payload is `build.submitted.sh`. Local rustfmt, Python parse,
registration apply-check, shell syntax, policy and whitespace checks passed0.
The source is compiled, not yet a passing actual VM result.

Follow-up r74 is **active** at this first signed compiled checkpoint: scoped
Clippy, matching actual static testguest, pinned kernel and binary/hash map in
`artifacts.sh`/`artifacts.submitted.sh`. It received fresh2CPU/4GiB/24GiB admission
and uses the remaining original3600s compilation budget. Its logs/artifacts remain
exclusive while active. The actual campaign is **pending**; a later evidence
checkpoint must retain all raw results and associations before any passing claim.

Initial buildr70 isolated source, CARGO_HOME and CARGO_TARGET_DIR, but omitted
Zig/XDG cache overrides. `scripts/zig-cc` uses Zig's defaults, so writes to the
shared per-user Zig cache are possible/unverified. This was reported immediately;
root instructed preserving the active build and existing cache data. No claim
that every compiler cache was private, and no cache deletion. Root's protected
named Cargo clone/target was not used. Subsequent builds/runs explicitly bind
Zig/XDG/SHARDS caches under the owned scope. A later read-only progress request
was refused for0 idle Fedora cores; it did not run and was not bypassed.

R01 tests may assert; production frozen. R02 primary/alternative research above.
R03 all new limits are explicit test inputs; protocol/fixture literals explained.
R04 real ABI state machine, explicit work→cleanup and error propagation.
R05 existing authority/Observer reused; one event wait, no cache/probe loop.
R06/R07/R13 focused one-case leaf and local receipt functions, safety comment on
poll borrow. R08/R14 no performance/profile claims; root owns comparisons/audits.
R09 legacy actual failure retained; no speculative production fix.
R10 raw source/wire/results retained with exact hashes. R11 Linux KVM only;
other native VM targets unverified and seven-target gate owed root.
R12 existing shared capture/broker seams; only assigned Linux fixture readiness.
R15 assigned bounded campaign, root integration/mutation review and merge owed.
R16 private paths, Mac33GiB free before edits, no heavy local build/protected cache
or paused WinARM touch. Full P7/G01–G24, durable pre-effect audit, budget/population
accounting and meaningful single-guard effect mutations remain open.
