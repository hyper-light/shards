# inithex-03: MCP bridge ends without waiting for live stdin

POLICY_READ: implementer; Shards base 9efde72524dd626f3ff84e6b9891852d7c810a9b;
AGENTS f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41;
rules df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3;
evidence 61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63;
CLAUDE 588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010.
Owned finding: inithex-03, D254; inithex-01/02 remain another lane's.

The real-pipe regressions keep the harness stdin writer alive while the socket
peer closes or stdout fails. The old scoped thread join cannot return before that
writer closes. Tests always close it for cleanup before asserting their verdict.
Red e74696f22: 2 failures for the exact wait; green f7d83f73a: all 7 MCP tests pass,
including a normal socket half-close that closes stdout and still delivers later
stdin. Clippy of shards-init all targets passes on Linux x86_64, adacachy2. Commands,
source identities and exact relevant output excerpts are in mcp-red.log and
mcp-green.log, captured from kiln r2/r4. Kiln collected its original full outputs
before they could be fetched again; these are excerpts retained from the tool reply.

One process now owns the descriptors and both explicit Reading/Draining/Closed
halves. Nonblocking I/O plus readiness notifications permits either fault to
return without joining a thread blocked on a different descriptor. Pending bytes
are drained before normal EOF closes stdout or half-closes the socket. POLLHUP
ends stdin when the socket peer is gone. No cache, polling interval, retry or
chosen runtime deadline. The only buffer bound is Linux PIPE_BUF, the atomic pipe
write bound (pipe(7)); no write is larger. The poll array's three entries are the
three owned descriptors; -1 means disabled descriptor/infinite wait per poll(2).
The one-second regression cleanup bound is test-only, not a runtime setting.

Alternatives: keeping scoped io::copy threads cannot cancel a pipe read; shutting
the socket alone cannot cancel stdin. The existing native relay shares bidirectional
socket endpoints and socket shutdown semantics, while this bridge has two distinct
pipe endpoints. A descriptor-owned loop here avoids changing that shared engine
and managed-TCP module. Sources: Linux [poll(2)](https://man7.org/linux/man-pages/man2/poll.2.html),
[pipe(7)](https://man7.org/linux/man-pages/man7/pipe.7.html), and existing
crates/init/src/relay.rs and input.rs. Cold start requires no learning or calibration.
Failures retain io::ErrorKind/errno, not parsed strings.

R01/R04/R06/R07/R13: checked pending ranges/arithmetic; no production panic; explicit
halves and descriptor ownership; I/O errors returned and process status preserved.
R02/R03/R05/R12: alternatives and syscall invariants above; existing Linux guest
seam preserved, no new installed binary or cache. R09/R10: red/green real-pipe unit
proof retained. R11: Linux guest code shared by all hosts; other guest architectures
and host/hypervisor combinations unrun, CI/ROOT owed. R08/R14: no performance or
allocation/fault claim; coordinator measurements unrun. R15/R16: no subdelegation,
VMs or broad tests; isolated remote target removed at task end. ROOT owns integration,
measurement and evidence review. Policy script is a mechanical check, not this proof.

ROOT owes real-VM D254 coverage, especially controller abort/rejection and normal
half-close through the complete relay. Exact existing fixture command:
`cargo test -p shards --profile test-release --test api_image an_orchestrators_members_are_signaled_and_their_mcp_connections_reached_on_its_endpoint`.
No real-VM test or mutation campaign run in this implementer lane.
