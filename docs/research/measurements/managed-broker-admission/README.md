# Checked peer-effect admission: implementation checkpoint

P7 / G03, G07, G13–G15, G17, G19. This is the existing per-domain server's
`send`/`answer`/`hand` slice, not completion of P7 or native-execution authority.

## Identity and scope

Implementer `/root/borrowed_connector`; coordinator `/root`. Base and policy
revision `cbd39284937d024640c56db6f8d8ef2bd9b0ee24`, rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Full AGENTS, R01–R16, evidence checklist, CLAUDE, managed plan and current
`/Users/adalundhe/g121-handoff.md` read, including after compaction.

Owned source: new `crates/server/src/broker.rs`; existing `server.rs` call seam,
its two actual callers, approved held-file helper movement and focused tests;
`main.rs` module registration. The run-tool error presentation uses the same
fallible presentation writer; Programs/start/helper/reaping are untouched.
Prior API `3b3817eea489d107fb9820f0f46c4fe429709fa3` and reconnect fixture
`419c4d218a616b06ee2ac8fff897be6f1469ee86` remain preserved and unchanged.

## D-entry: deepen the existing dispatcher

The trusted instance's `Prepared.label` and init-supplied peer descriptor/grants
remain authority. Request JSON cannot select the principal. The existing held
scratch directory is the only file root. One checked dispatcher serves HTTP
`tools/call` and QuickJS `call_for_program`; no second transport or manager.

The approved research is [managed-runtime-tool-broker](../../managed-runtime-tool-broker.md),
especially its existing-seam, provenance, audit and native-executor distinctions.
Alternatives inspected: retaining the old String dispatcher has no owned resource
reservation; harness hook approval cannot authorize arbitrary native effects;
generic executor/audit traits have no actual executor/sink here. A separate
process/file/network capability would invent authority: the per-domain server
is outside the actor's namespaces and has no checked native executable/root.
This slice therefore executes only the three already-granted peer operations.

External primary contracts rechecked 2026-11-10:

- [Linux send(2)](https://man7.org/linux/man-pages/man2/send.2.html): one
  nonblocking, no-SIGPIPE local send. Returned bytes establish local kernel
  acceptance, not delivery or remote execution. Errors retain raw OS cause;
  an unexpected short result is explicitly uncertain and is never replayed.
- [Linux openat2(2)](https://man7.org/linux/man-pages/man2/openat2.2.html): retain
  the existing held-directory `BENEATH|NO_SYMLINKS|NO_MAGICLINKS|NO_XDEV`
  resolution, flags and regular-file check. One existing implementation moves
  into the broker; its original focused tests use that same helper.
- [Rust Vec reservation](https://doc.rust-lang.org/std/vec/struct.Vec.html#method.try_reserve_exact):
  fallible reservation precedes filling/appending. Actual allocation may exceed
  the requested size; no allocator-size or zero-allocation claim is made.
- [MCP 2025-11-25 tools](https://modelcontextprotocol.io/specification/2025-11-25/server/tools):
  tool errors remain caller-format tool results; schemas/annotations are not
  principal authority. This preserves the existing Program and MCP envelopes.

Request validation distinguishes malformed IDs from an absent optional hand ID.
Authorization checks the actual peer grant and ownership of the answer ID before
file access. `Authorized -> Reserved` owns actual packet capacity, the complete response
buffer counted from the same encoder and the pending-ID vector slot. `execute()`
consumes that token once and returns typed completion, local failure or uncertain
acceptance. `Terminal::present()` actually consumes Receipt/Outcome to encode
MCP `structuredContent`: operation, request, actual principal/peer, outcome,
local-effect kind/accepted bytes, phase and typed cause/raw OS code. Program
replies retain `value`/`error` and carry the same `broker` object: the actual
sandbox bootstrap reads only `r.error` and `r.value` (sandbox.rs lines 58–59).
The entire encoded success/error plus metadata capacity is reserved before the
effect; the fixed-capacity encoder then uses actual completion facts. No unused
receipt hook, lint suppression, sink trait or ceremonial state comparison remains.
Pre-reservation refusals have null operation/request because no checked effect
identity was assigned. The metadata is returned to the caller, not ordinary logs.
Received/validated/authorized/executing errors retain their phase and typed cause.

New request IDs use checked addition. Once fully reserved, an ID is consumed even
if its local send fails. IDs never wrap or get reused. A successful answer removes
only its owned pending request; possible partial acceptance also consumes ownership
to prevent an unsafe retry. No new grant, PID authority or durable identity is minted.

Receipts borrow stable trusted labels during the synchronous call. Constructors
are private; callers do not redundantly compare copied labels or ceremonial
phase markers. Human refusals preserve meaningful caller/peer/ownership context.
Reply Debug reports byte length, not private payload.

`MemoryOnly` explicitly preserves the legacy evidence contract. Returning local
facts in a response does not retain them durably. `RequiredDurable` refuses before file read/send because
there is no actual retained sink. Strict durable execution is unavailable.

### Bounds and literals

There are no new runtime policy numbers. The unchanged message header is one
kind byte plus the existing u64 ID; file-name length is the existing u16 BE field.
Text reserves checked `MESSAGE_HEAD + text.len()`, checked against the actual
peer's existing kernel-derived `most`, rather than reserving its whole sendbuf.
Hand reserves that ceiling plus one probe byte because the file may grow; the
payload limit subtracts the actual encoded name/header. Paths reserve their actual
byte length plus the C NUL. Response capacity is the maximum complete encoded size of the actual possible
execution outcomes and typed metadata with actual escaped scope labels and
maximum native integer representations, not an arbitrary formatting buffer.
JSON's control cutoff and four-digit hex escaping are its existing encoding
contract. Test socket sizes, payloads, IDs and labels are explicit fixture inputs,
not production defaults. Checked overflow/capacity failures refuse before effects.

### Wait/cache and ownership review

No new cache, periodic probe, lock, watcher, worker or wait. Peer admission is
synchronous and its mutable borrowed ledger has one owner. The send is one
`MSG_DONTWAIT` attempt; no retry/sleep/poll masks a full queue or interruption.
The existing regular-file read uses nonblocking-open and retries interrupted reads;
this is not a new finite disk-I/O deadline guarantee. Existing HTTP parsing,
`take`/receive, outer HTTP/session framing and sandbox result queues still have
their prior allocations and behavior. No whole-wire panic/OOM claim follows from
the broker's reserves. Arbitrary Display producers can themselves allocate; the
fallible outer formatter does not certify those producers.

## Reproduction and validation

Targeted harness source is in the actual server unit tests, not a mocked transport.
`broker_http_and_program_callers_share_the_actual_peer_admission` drives the actual
HTTP `request()` parser and Program caller against one real SOCK_SEQPACKET peer.
It observes actual send/hand/answer bytes, the shared checked ledger and actual
MCP/Program typed metadata. The broken-peer test checks actual OS cause and
not-accepted JSON; an encoder-only case covers uncertain short-send metadata,
not a claim that the native sequenced-packet transport returned a short send. It does
not authenticate a new TLS connection or launch a VM/worker. The ID-boundary test
prints actual receiver ID and caller reply before asserting, so the baseline's
forbidden packet can be retained independently of the assertion.

Run [run-light.sh](run-light.sh) from an admitted private source/target/Cargo home.
Results and exact source hashes are recorded in `results.md`; absent results are
pending, not passed. Root owns combined-source integration, full native/VM/fault/
OOM/mutation/resource campaigns, all platform gates and performance comparisons.

## R01–R16 accounting

| Rule | Applicable proof and remaining gap |
| --- | --- |
| R01 | New production reserves/checks arithmetic, indexing, path storage and reply/pending-ID capacity; no unwrap/assert/panic or poisoned locks. Actual OOM injection, full transitive/caller audit and existing outer allocations remain root gates. |
| R02 | Existing research, pinned base source, primary OS/Rust/MCP contracts and alternatives above; live scoped results belong in results.md. Broader native executor/sink decisions remain open. |
| R03 | Derived encoded sizes/OS facts/protocol fields above; no learned/arbitrary runtime threshold. Allocation may exceed requested capacity. |
| R04 | Real authorize/reserve/execute tokens; Terminal drives actual MCP/Program typed JSON metadata, human text renders at presentation. No ceremonial checks or unused future hooks. Restart/generation/revocation/cancel contracts remain open. |
| R05 | One existing seam, two real callers; no speculative traits/managers/caches or new probes. Wait review above. |
| R06 | Focused validators, one held-file path, one executor; formatting has counted/fallible/fixed writers to serve the two actual envelopes before/after effects. Coordinator complexity review remains required. |
| R07 | Comments retain unsafe ownership, derived limits and local-acceptance uncertainty; no private content logging. |
| R08 | No comparative performance result or 4–5× claim; root owes equivalent baselines/distributions. |
| R09 | Actual original-main ID0 packet red and candidate no-packet green retained. The draft preceded baseline reproduction: before-code chronology was violated, disclosed and frozen, then re-evaluated against actual data. It is not retrospectively marked compliant; root mutation/adoption review remains owed. |
| R10 | Committed repeatable caller/tests, source hashes, raw results and honest gaps; root mutation and broader cleanup evidence still owed. |
| R11 | Production server is Linux guest code; scoped native amd64/glibc proof only when recorded. GNU/musl on both architectures, seven host targets and KVM/HVF/WHP remain root gates; Intel Mac unsupported. |
| R12 | Existing held root/shared parser/peer protocol, no platform parallelism, new installed binary, root requirement or crypto change. |
| R13 | Existing helper moved rather than copied; typed tokens own real resources, text reserves exact encoded request; broader features not simulated. |
| R14 | No profiling/benchmarking by implementer; root owns induced parent/kernel allocations/copies/faults and fair-neighbor results. |
| R15 | Scoped implementer/light checks only, no children/goals/main merge; root owns final integration/evidence review. Signed checkpoint listed in results/handoff. |
| R16 | Local edits/format/policy only, Mac floor monitored; remote NEW private source/target/Cargo home admitted 1CPU/1GiB/2GiB, no root caches or paused WinARM access. Preserve source/logs and record actual disk totals. |

Next concrete component: root's actual retained pre-effect audit reservation and
held native actor-root/executable authority, followed by exact-source caller OOM
and guard-removal evidence. These are required P7/P9 work, not optional guarantees.

The exact baseline-only fixture is [before-case.patch.gz](before-case.patch.gz).
In a private checkout at cbd, run `gzip -dc before-case.patch.gz | git apply`,
then the exact filtered test command in results.md. Gzip preserves the actual
patch bytes, including unified-diff blank context lines. The complete raw archive
also retains the exact validation-manifest patch; its expanded copy is omitted
from `raw/` so protocol context spaces are not rewritten to satisfy source checks.

Native Cargo logs ending in an extra blank line are individually gzip-compressed
in raw/ rather than trimming the original stdout. `gzip -dc raw/targeted-v7.log.gz`
returns the exact bytes named by raw-sha256.json. No whitespace rule is disabled.
