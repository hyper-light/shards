# Proposed native cancellation admission witnesses

IMPLEMENTER `01a1c5d2ee0e7248982c95a59f1fd905`; ROOT coordinator
`01a148884a4770fdaf0111f3f792a248`. This documentation-only continuation starts at
signed correction `cd5d7212cc61e9196b5afe7a3e2891ba60a74529`, separate from prepared
notification `f8d643c59`, backend-end `47912a11c` and extra-datagram `a343f772d`.
Full canonical policy/plan/handoff readback remains loaded; policy files
`f5cec365`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
No runtime/source edit, new defect or actual descriptor result is reported here.

ROOT reported r626's actual pending-cancel correction/guard-removal/restoration
and original controls passing, with independent backend bytes on the mutant.
The [correction record](README.md) retains its exact original live red and scoped
pure evidence. This proposal supplies remaining native error/precedence evidence;
it does not replace the actual socket/VM results or establish cost/closure.

## Concrete bounded scope

ROOT review key: `relay-next-native-readiness-proposal-v1`. Add ignored Linux
native tests only in existing guest udp::tests; implementer compiles/pure-tests,
ROOT alone executes actual eventfd/pipe cases. No new CLI, worker manager, OS
adapter, thread, signal handler, grant or installed artifact. Use the actual
Deadline::io implementation and existing notification/cancellation consumer.

Every native case requires explicit positive `SHARDS_CANCEL_CASE_MS`; no built-in
wait/default, sleep or invented timing tolerance. All checks remain within that
same original absolute deadline. Test-only pipe2 setup owns both successful FDs
as OwnedFd immediately and deliberately drops only its own peer. No other
process/cotenant descriptors or closed descriptor-number reuse are involved.

| Case | Actual input | Decisive witness |
| --- | --- | --- |
| Already pending | Existing notification() eventfd, one successful notify(), Deadline with Some(ownedFD). | Actual io call returns ConnectionAborted before its operation closure; closure attempt counter remains0. Existing cancellation_observed still consumes the pending token afterward, establishing that admission did not consume it. |
| Expiry plus pending | Re-notify the same owned eventfd, then Deadline at Instant::now with that same Some(FD). | Actual io call returns TimedOut before closure/cancel validation; counter0 and token still readable. No substitute deadline or grace. |
| Native HUP | Own nonblocking/CLOEXEC pipe read end while its owned writer has dropped. | Actual cancel readiness returns BrokenPipe before the operation closure, counter0; neither permission nor Cancelled. |
| Native ERR | Own nonblocking/CLOEXEC pipe write end while its owned reader has dropped. | Actual cancel readiness returns Other before the operation closure, counter0; neither permission nor Cancelled. |

The operation closure performs only a finite in-process witness increment, not a
network effect. ROOT's existing actual send/backend-receipt mutation remains the
primary effect proof; these tests specifically exercise native readiness/error
handling and nonconsumption. No actual descriptor/syscall case is an implementer
light pass. Tests are ignored by default; skipped cases remain unverified.

Do not create an invalid FD by racing a dropped descriptor number with other
threads. Native NVAL remains explicitly unverified in this proposal; pure
NVAL/readable/failure/unknown-bit precedence tests already pass. ROOT may choose
a separately justified isolated owned failure fixture. Native EINTR/simultaneous
signal races are likewise not claimed by the existing pure Interrupted closure
test or these cases. No new signal/syscall permission is proposed.

## Basis and alternatives

[poll(2)](https://man7.org/linux/man-pages/man2/poll.2.html) specifies returned
HUP/ERR/NVAL conditions, including ERR on a pipe writer after reader closure;
[Linux v6.18 select.c](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/select.c)
provides the actual native readiness seam, and
[eventfd.c](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/eventfd.c)
separates counter readiness from consumption. These sources justify selecting
owned pipe endpoints and the existing eventfd rather than a socket/network setup.
Actual results remain ROOT work; documentation is not a reproduction.

Alternatives rejected for this fixture: sleeping near expiry adds timing noise;
new helper threads/signal handlers enlarge scope; a simulated mask alone does
not validate native event delivery; an unknown/closed FD number introduces
unnecessary ownership ambiguity. Existing pure flag tests remain useful and
are not represented as native tests.

## Gates and R01–R16 accounting

Before source implementation, ROOT reviews this additional scope. Proposed native
test functions stay ignored, preserve production source semantics, and reuse
existing pure/light wrappers with NEW private source/target under Kiln admission
at most1CPU/1GiB/2GiB. ROOT runs exact named native tests under a supplied caller
budget, retains toolchain/source/binary/command/raw outcomes and actual FD/error
facts, and owns mutations/combined gates. This proposal closes no lane goal.

| Rule | Evidence / remaining gate |
| --- | --- |
| R01 | Documentation only. Proposed fallible owned setup/no unowned FD; actual error/cleanup proof pending. |
| R02 | Primary native contracts and explicit alternatives above; no new architecture selected. |
| R03 | FD pairs/one event/zero attempts derive from the declared cases and ABI; caller deadline required. |
| R04 | Typed pending/expiry/HUP/ERR outcomes and owned peer-drop transitions; native states unverified. |
| R05 | Existing Deadline/eventfd consumer, no timer loop/cache/manager. ROOT audits actual waits. |
| R06 | Four finite witnesses at one existing seam, not a framework. Source review pending. |
| R07 | Explicitly distinguishes in-process witness, actual socket proof and unrun native cases. |
| R08 | No comparative/performance claim; cost remains ROOT measurement. |
| R09 | No new defect/fix. Known live pending-cancel defect already reproduced by ROOT before cd5. |
| R10 | Proposal scope, exact existing checkpoint and primary references retained; source/native evidence pending. |
| R11 | Proposed native scope Linux guest code on supported GNU/musl architectures; no new hardware pass. macOS ARM64 only, Windows ARM paused. |
| R12 | Test-only owned endpoints at existing native seam, no production adapter/installed artifact/root/grant/trust/crypto change. |
| R13 | Minimal explicit native inputs; no FD-reuse trick, new signal handler or relaxed strict case. |
| R14 | No implementer audit/profile/benchmark; added cost/resource distributions remain ROOT owed. |
| R15 | IMPLEMENTER proposal only; actual native execution/mutation/combined gates and main merges ROOT-owned. |
| R16 | Same worktree/no local builds or large copy/archive/deletion; all protected ROOT/evidence/paused VM ownership preserved. |
