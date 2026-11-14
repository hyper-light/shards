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

# inithex-04: absolute-FQDN MCP URLs agree with DNS

Same implementer/policy receipt and host as above. Red source 8e8e1c439, kiln r7:
`cargo test -p shards-agentfile mcp_absolute_name -j 4` and
`cargo test -p shards-init mcp_absolute_name -j 4` each fail the same exact mismatch,
`mcp.example.` versus DNS `mcp.example`. The init test runs the real netplan reader,
keeps a second member ungranted and checks no unrestricted DNS was granted.
The Agentfile test compares both HTTPS/default-port and HTTP/explicit-port endpoints
against an actual RFC1035 wire question. names-red.log retains the output.

Fix 79019fa55 removes one terminal root label before the existing lowercase operation
in both existing URL readers. RFC1034 section 3.1 defines the final printed dot as
that root label; query_name joins the wire labels without it. The source URL remains
unchanged for the MCP client; the policy host agrees with DNS. Existing explicit
ports, scoped recipients and IPv6 bracket handling are preserved. The existing
paired readers receive the same small normalization rule, with no new manager or
public abstraction. Rejecting valid absolute names was considered; canonicalizing
the root label supports their existing accepted URL syntax without widening any
port or adding a DNS permission. Source:
[RFC1034 section 3.1](https://www.rfc-editor.org/rfc/rfc1034#section-3.1).
There is no new size, timeout, cache, retry, estimate or cold-start dependency.

The earlier carrier run r6 was stopped: the signer temporarily rejected its red
commit so the carrier had b1415acec, not the regression tests. It establishes no
red/green result. Signed red 8e8e1c439 and green 79019fa55 are the relevant revisions.

ROOT owes a terminal-dot remote-MCP URL case through real member DNS and host address
learning, with only its declared port reachable and the ungranted member still
denied. Extend the existing destination fixture with that variant, then run:
`cargo test -p shards --profile test-release --test containers a_networks_egress_is_a_grant_of_its_destinations_alone`.
This fixture currently covers destinations; this lane did not add or run its
real-VM MCP variant. Seven-target CI and hypervisor runs remain ROOT's.

At 79019fa55, r8 on Linux x86_64: 3 Agentfile MCP tests and 8 init netplan tests
passed; `cargo clippy -p shards-agentfile -p shards-init --all-targets -j 4 -- -D warnings`
passed. names-green.log and names-green-final.log retain output excerpts; the first
file records the counts from the earlier streamed output. Clippy waited on the
shared package cache and took 2m45s; no performance conclusion follows.
The final merged run also repeats the 7 MCP termination tests. A follow-up socket
POLLERR branch preserves errno even when a pending response prevents another read;
its backpressured-error branch has source reasoning, not a separate fault witness.
