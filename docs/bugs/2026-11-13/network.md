# Network adversarial review

POLICY_READ: head `2bc5f78f9e48d09ddbf4e848f9960133f6d19148`; role **adversarial reviewer**, area **network**. Project coordinator: ROOT, Kiln agent `01a148884a4770fdaf0111f3f792a248`. Review author: Kiln agent `01a1d7f054aa70d2836606caacc32692`. Owned review tree: `/Users/adalundhe/Projects/shards/.claude/worktrees/rv-network`, detached at head. `origin/main` resolves to this same revision.

Read in full before review/delegation:

| File | SHA-256 |
| --- | --- |
| AGENTS.md | f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41 |
| docs/development-rules.md | df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3 |
| docs/development-evidence.md | 61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63 |
| CLAUDE.md | 588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010 |
| /tmp/g121-shot/review/brief.md | 13d6cd439d0eca844ec8306858dfe7919a821af7b60f0326948100059d6862de |

All three read-only review delegates returned the same head/policy readback before inspection. No delegate ran builds/tests or edited files. The binding brief's static-only restrictions supersede the older permission to run adversarial campaigns. No tracked files were changed, no code was patched, no tests/builds/daemons/VMs were run, and no commits were made. This report is the only artifact written.

Comparison: `git diff 79caf2d743d4c3c917f207bd650655a523847bf4..2bc5f78f9e48d09ddbf4e848f9960133f6d19148`, plus current callers and older enforcement code still used at head. Evidence here is **source verification**, not live reproduction; R09/R10 runtime reproduction, mutation, platform and resource measurements remain ROOT's work. Test descriptions below are proposed tests, never claimed passing. The code-review skill supplied parallel compliance/behavior lanes; the user's brief determines this report's format and excludes style findings.

Ranking: P0 critical; P1 high security/resource priority; P2 high correctness priority; P3 medium; P4/P5 lower priority. There are **12 findings: 0 critical, 8 high, 4 medium, 0 low**. No P0 established. All file:line references below are at head, relative to the review tree.

## Findings, ranked

### network-01 — P1 / high: an empty stale FIN can terminate another TCP flow without either sequence number

**Location:** `crates/net/src/tcp.rs:1028`, `:1094`; effects at `:606`. Present before the base and still present at head.

**Defect:** the receive path rewrites an old segment's sequence to `rcv_nxt` even after discarding all its payload, then accepts its FIN at that rewritten position.

**Concrete scenario:** an established connection has `rcv_nxt=5005`, empty `to_host`, and no injector. Deliver a segment on its four-tuple with `seq=1`, flags FIN, no ACK and no payload. Lines 1028–1031 set `data=[]; seq=rcv_nxt`. The FIN condition `seq.wrapping_add(data.len() as u32) == self.rcv_nxt` succeeds, sets `guest_fin` and advances the receive sequence. `flush_to_host` then executes `sock.shutdown(Shutdown::Write)`, sending EOF upstream and refusing subsequent guest data. A raw-capable workload knowing the victim tuple can send two empty FINs numbered 0 and `0x80000000`; for any receive sequence, one is equal or before it under `before()`'s signed wrapping comparison. No ACK or correct sequence guess is required.

**Evidence and contract:** ACK handling at :1003 is optional; the acceptability check never rejects this FIN. The :1088 comment itself cites RFC 9293 §3.10.7.4 for rejecting segments before the receive position. `crates/net/src/lib.rs:1663–1665` explicitly says unpredictable initial sequences prevent a raw-socket workload injecting segments into other guest processes' connections. The exact-sequence RST check at :968 does not protect FIN. This proves connection-integrity failure within a guest; no cross-microVM packet injection or ability of a capability-stripped domain to create raw packets is claimed.

**Confidence:** high in the state transition; live attack unrun.

**Fix direction:** validate the original segment against the receive window before processing ACK/FIN, require legal flags, and preserve FIN's original sequence position when trimming old payload.

**Catching test sketch:** `let mut c = opened(); let before=c.rcv_nxt; c.on_segment(empty_fin(before.wrapping_sub(4096)), out, None); assert!(!c.guest_fin); assert!(!c.host_shut); assert_eq!(c.rcv_nxt,before);` Also exercise the two blind FIN sequence values and prove the host socket receives no EOF.

**Status (2026-11-13):** fixed in 50716d9465c42d9cbabbdd8e725134ba6f80bffe (red `tcp::tests::stale_fins_do_not_end_the_host_stream`); 20 focused TCP tests and crate clippy passed on macOS arm64. Merged-source check and ROOT's real-VM coverage tracked in `docs/research/measurements/network-fixes-v1/README.md`.

### network-02 — P1 / high: the network process silently omits syscall confinement on supported arm64 Linux

**Location:** `crates/net-process/src/main.rs:245`; `crates/net/src/confine.rs:209`. Present before the base; current supported arm64 Linux execution still takes it.

**Defect:** the process installs its seccomp allowlist only under `cfg(all(target_os="linux", target_arch="x86_64"))` and proceeds into `serve` on aarch64 without a filter or typed refusal.

**Concrete scenario:** start a real network process on supported aarch64 Linux. It executes `confine::at_start`, skips `confine::filter`, then reads hostile frames. Unlike amd64, its syscall surface is unrestricted by the documented allowlist: the process has no seccomp denial for process creation, exec, or other calls the amd64 process explicitly excludes. Landlock handles filesystem/TCP/scope rights; it does not install this syscall allowlist.

**Evidence and contract:** `confine.rs` expressly documents “elsewhere nothing is installed yet,” and `filter`, `program`, and `install` are x86_64-only. D31 promises the network process is confined before reading guest frames; R11/R12 and CLAUDE.md require supported architecture parity rather than a silent missing twin. No memory-corruption exploit is asserted: the verified defect is the missing containment layer on an advertised supported target.

**Confidence:** high, compile-time path is explicit; arm64 runtime unrun.

**Fix direction:** share allowlist construction using architecture-specific syscall identities, install it on arm64, or return an explicit unsupported-confinement refusal until parity exists.

**Catching test:** in a disposable network-process child on both Linux architectures, observe seccomp state and attempt an excluded harmless syscall; require the same typed refusal/termination outcome. Test the actual carried helper, not just a library function.

### network-03 — P1 / high: DNS-derived authority accumulates forever in host memory

**Location:** `crates/net/src/lib.rs:2412`, `:2868`, `:613`; `crates/dns/src/lib.rs:291`. Learning machinery predates the base; D192/D236 add new production uses in the review range.

**Defect:** every address ever answered for a granted name is added to a process-lifetime `HashSet<(IpAddr,u16)>`, without TTL, replacement, revocation or a resource bound.

**Concrete scenario:** a long-lived VM is granted `allowed.example:443`; its resolver returns a new unique A or AAAA address on each query. Reuse one UDP DNS socket to avoid growing the transport tables. Each answer permanently grows `cfg.learned`, regardless of TTL, including TTL 0. Subsequent replies naming other addresses or no addresses never remove old tuples. The same happens over TCP. A guest repeatedly querying a rotating granted domain can grow host-side state without a corresponding live-flow/guest-memory bound. After a TTL-1 answer A is replaced by B, A:443 also remains permitted for the VM's life.

**Evidence and contract:** both learning paths use only `learned.extend(...)`; `Config::allows` trusts set membership. All uses of `learned` were checked: there is no expiry/removal path. `a_records` does not return TTL information at all. R05 explicitly requires cache invalidation, resource/lifetime bounds and consistency proof; D192/D236's destination authority is held by the resolver path. D46 already recognizes address reassignment as a reason historical answers must not outlive their transaction. PM M210 measures finite concurrent transport connections, not this unlimited DNS history, so it supplies no bound for it.

**Confidence:** high for unlimited retained history; the precise intended post-TTL security semantics need ROOT's decision. No host OOM was induced.

**Fix direction:** retain name/address associations with expiry and an explicit admitted resource envelope; retire stale authority while handling currently established connections deliberately. Do not fix this by inventing a cache capacity or TTL floor.

**Catching test:** give the same granted name N distinct TTL-0/TTL-1 answers over UDP and TCP, advance a controlled clock, and require learned storage to return to the currently authorized set. Check that an expired address cannot open a new flow and unrelated names never grant authority. ROOT should measure actual host memory against a fixed live transport count.

### network-04 — P1 / high: UDP holder changes can reuse a still-live conntrack zone and restore an old destination

**Location:** `crates/init/src/chooser.rs:465`; `crates/init/src/links.rs:1561`. New chooser behavior in the review range.

**Defect:** conntrack zone allocation wraps from 65535 to 1 without establishing that the previous zone's NAT entries have expired.

**Concrete scenario:** a contested UDP port's client tuple first maps to member A in zone 1. Change the holder to B, then churn accepted holder choices until zone 1 is allocated again while its original entry remains alive. Arrange the final holder as B. The same client tuple is again classified into zone 1; conntrack finds its old entry and applies the A DNAT binding, rather than the chooser's new B destination. The allocator's entire implementation is `self.zone = self.zone.checked_add(1).unwrap_or(1)`. There is no tracked active-zone set, retirement time, allocation rate bound or refusal at wrap.

**Evidence and contract:** D122 introduced zones precisely because a preexisting UDP tuple otherwise keeps its previous NAT holder; `program_chosen` sets the chosen zone before conntrack for both directions. The `next_zone` comment assumes flows “have long expired” by wrap, but no code enforces elapsed time, and 65535 choices need not take 120 seconds. This can send traffic to a previous candidate instead of the selected listener; all candidates may be granted the port, so an ungranted-domain escape is not claimed.

**Confidence:** high in the reuse/state collision; live kernel wrap campaign unrun.

**Fix direction:** reuse a zone only after proving its prior bindings are retired; handle generation-space exhaustion explicitly, with no guessed safe churn interval.

**Catching test:** retain an actual zone-1 tuple to A, force the allocator close to wrap in a disposable fixture, hand the port to B, then resend the same client tuple. Require B to receive it and A to observe nothing. Also test a non-forced full-cycle campaign under ROOT's resource limits.

**Status (2026-11-13):** owed to ROOT: `cargo test -p shards --test build a_udp_flow_goes_to_its_ports_next_holder -- --exact --nocapture`, extended with a retained zone-1 tuple and forced allocator wrap. The existing one-handoff fixture does not reproduce wrap. Retirement design and owner-failure requirements: `docs/research/measurements/network-fixes-v1/README.md#network-04-conntrack-zone-retirement`. No allocator patch or passing reproduction is claimed.

### network-05 — P2 / high: Hex/API-run never programs the IPv6 uplink that daemon runs receive

**Location:** `crates/shards/src/api_run.rs:2544`, `:3072`; supporting paths `:2841`, `crates/init/src/links.rs:1213`. API-run was added in the review range.

**Defect:** API-created image runs grant IPv6 egress but omit both guest eth0 IPv6 setup and the network process's IPv6 address configuration.

**Concrete scenario:** boot through `ImageShards` an image with an IPv6-enabled agent network and a granted IPv6-only remote MCP/provider. `network()` selects bridge and encodes its policy; `plan()` adds `confine-eth0` but never an `uplink6=`/`address6=` entry, then replaces the spec's setup. Launch starts a network process from `Config::on_bridge`, whose IPv6 fields are `None`. `tell_net` sends PUBLISH/NET_POLICY and optional credential messages, never NET_ADDRESS. Init's `current6()` remains None, so no `Uplink6` is created and the agent's IPv6 default route is omitted. The explicitly granted IPv6 destination is unreachable.

**Evidence and contract:** D99 part four and goal #23 promise agents' IPv6 past a microVM on the default bridge; goal #60 includes Hex's per-VM programming. The daemon contains the required paired steps at `daemon.rs:3837–3850` (`uplink6=`) and `:5639–5655` (NET_ADDRESS). These are not shared with API-run. `run::prepare`, launch/netproc and setup composition were checked for a hidden equivalent; none supplies it. At `links.rs:1213–1215`, a default IPv6 route requires `self.uplink6`.

**Confidence:** high; actual Hex boot unrun.

**Fix direction:** share the daemon's IPv6 derivation and paired guest/network-process programming with API runs, preserving the same narrowed grants and confinement ordering.

**Catching test:** launch the D99 IPv6-only MCP fixture through the orchestrator API, fresh and restored. Require granted reach plus refusal of an ungranted address/port, and confirm the command cannot use its agent's egress grant.

### network-06 — P2 / high: one listener captures every port in a contested range

**Location:** `crates/init/src/chooser.rs:440`, `:477`; `crates/init/src/netplan.rs:759`; `crates/init/src/links.rs:1589`. New in the review range.

**Defect:** chooser ownership is per exposure range, while listening is true when any one port in that range has a listener.

**Concrete scenario:** a network grants ingress 7000–7001 and `EXPOSE 7000-7001 AS ingress FOR web api` makes both members candidates. Web listens only on 7000, api only on 7001. `contested` keeps one Contest for the entire range. `listening`/`takes` returns true for web's 7000; `choose_all` retains web as the range's sole holder; `program()` emits the whole `c.from` range to web. New connections to 7001 go to web's closed 7001 rather than api's listening socket.

**Evidence and contract:** D122's user decision says each port goes to the member listening on it. The parser refuses *mapped* ranges, but permits unmapped ranges, which `netplan::ingress_of` preserves whole at :759–763. There is no expansion into individual contests. `takes` tests only `lo <= sport <= hi` and `program_chosen` matches the entire range. An implementation comment describing whole-range ownership is not an explicit user-facing refusal or a replacement for the binding contract.

**Confidence:** high; live split-listener fixture unrun.

**Fix direction:** choose holders independently for individual ports (coalescing equal adjacent results afterward if justified), or explicitly refuse unsupported contested ranges rather than accept incorrect behavior.

**Catching test:** publish both ports with web/api's split listeners under one exposure range; each port must return its own member's payload. Repeat with UDP and with one listener closing while the other remains.

### network-07 — P2 / high: an IPv6-only listener can win an IPv4 published port

**Location:** `crates/init/src/chooser.rs:623`; `:534`; `crates/init/src/links.rs:1590`. New in the review range.

**Defect:** the chooser counts any IPv6 wildcard listener as accepting IPv4 without reading IPV6_V6ONLY.

**Concrete scenario:** web and api are candidates for published TCP 7000. Web first listens on `[::]:7000` with `IPV6_V6ONLY=1`; api listens on `0.0.0.0:7000`. `listening` dumps both families and `takes` accepts web's unspecified IPv6 source unconditionally. Web wins; published IPv4 traffic is DNATed to web's IPv4 member address, where its IPv6-only socket cannot accept it. Api's valid IPv4 listener is never chosen.

**Evidence and contract:** `listening`'s own :531–533 comment says an IPv6 wildcard takes IPv4 “unless it is IPV6_V6ONLY.” The function requests no relevant socket option and `takes` only reads ports/source address; no later chooser path checks V6ONLY. D122/D99 explicitly deliver published agent ingress over IPv4. This is separate from host publication's correctly configured IPv6 listeners.

**Confidence:** high; live socket selection unrun.

**Fix direction:** use actual IPv4 acceptance when selecting a holder, including V6ONLY for IPv6 sockets, and exclude an IPv6-only socket from an IPv4 contest.

**Catching test:** two candidates as above, publish from an IPv4 client, require api's payload; then change web to a dual-stack socket and exercise the intended first-listener rule.

### network-08 — P2 / high: a persistent TCP DNS client monopolizes 127.0.0.11 for the whole VM

**Location:** `crates/init/src/dnsrelay.rs:87`, `:107`. Predates the base and remains the D46 path at head.

**Defect:** the only relay loop serves a complete TCP connection inline, never returning to UDP or other TCP clients while that connection remains active.

**Concrete scenario:** a normal DNS client connects to 127.0.0.11:53 and keeps its TCP session open, sending valid queries periodically. `relay` calls `serve_tcp`, whose `while read_message` continues until close/error. Each read timeout is per I/O, so continuing queries or partial reads within the five-second timeout keep it occupied indefinitely. All other processes' UDP DNS questions and TCP connections wait unserved; even an idle persistent connection can stall local name resolution for five seconds. UDP `ask()` similarly stalls the loop for each lost upstream answer.

**Evidence and contract:** D46 describes per-query UDP/TCP relay and immediate answers/refusals; PM M247 measures only serial UDP lookups and does not exercise concurrent TCP clients. There is no separate worker/multiplexer for this relay. This is a VM-wide availability defect, not a claim that one microVM blocks another.

**Confidence:** high in the blocking path; runtime timing unrun.

**Fix direction:** multiplex pending UDP asks and TCP sessions with bounded connection ownership; one client's connection lifetime must not own the relay's loop.

**Catching test:** keep one TCP DNS session open, then resolve a local peer from a second UDP and TCP client; both must progress before the first session closes. Repeat while the first client drips a partial length-prefixed message and while an upstream UDP answer is withheld.

### network-09 — P3 / medium: existing DNS TCP sessions retain obsolete per-member names

**Location:** `crates/init/src/agentdns.rs:465`, `:483`, `:509`; `crates/init/src/members.rs:426`. Dynamic member/name paths are active at head; stale clone predates the base.

**Defect:** the TCP handler clones an asker's entire authority/name table once at accept and never observes later member updates.

**Concrete scenario:** engine members A, B and C share a network. A opens TCP DNS and resolves B. B then leaves; `members::regrant` updates A's live Asker and its gates while A/C keep DNS available. A sends the same query on its existing TCP session: the worker still reads its cloned `asker.local` and returns B's old address/name with TTL 0. A new member's name is likewise absent from the retained clone. Repeated queries within the per-read timeout preserve this stale view indefinitely.

**Evidence and contract:** `serve_tcp` clones at :465 and passes `&asker` to the worker; `serve_connection` uses only that snapshot. `admit`/`release` update the RwLock, not existing workers. D181 promises names/grants rebuilt as members join and leave, including that a peer alias stops resolving once its member ends; D46 requires names be current grants. Reprogrammed flow gates limit impact, so this report does not claim stale names bypass a revoked flow grant.

**Confidence:** high; dynamic live run unrun.

**Fix direction:** resolve each message against current identity/authority, or invalidate affected sessions on membership changes while handling in-flight queries explicitly.

**Catching test:** retain A's TCP DNS session across B's departure and D's arrival; require old TCP and fresh UDP to agree that B is absent and D is present. Keep C alive to avoid conflating this with closing the entire DNS grant.

### network-10 — P3 / medium: host-network bind/close cycles retain unlimited listener history

**Location:** `crates/net/src/lib.rs:1705`, `:1717`, `:1369`. Predates the base and is also used by the Windows host-network work in this range.

**Defect:** each publication appends a listener slot and unpublication only replaces it with None; emptied slots are never reused or removed during a run.

**Concrete scenario:** a long-lived host-network guest repeatedly binds/listens/closes one port. The daemon's hostnet control sends NET_PUBLISHED/NET_UNPUBLISHED each cycle. `publish` assigns token/index from `published.len()` and pushes a slot; `unpublish` does `slot.take()`. At most one real listener/FD is live, but the vector grows by one slot every cycle until whole-run UNPUBLISH clears it. This growth has no FD/port concurrency bound and ultimately consumes host memory.

**Evidence and contract:** hosted IDs are removed, inbound UDP mappings are cleaned, and sockets are correctly unregistered, but none reclaims the vector slot. The only `published.clear()` is :1583 on full run release. R05/R10 require resource/lifetime bounds and cleanup; PM M210's fixed connection-count experiments do not test historical publication churn.

**Confidence:** high; memory growth was not stress-measured.

**Fix direction:** reuse free listener slots with generation-safe poll tokens; keep storage proportional to active/owned capacity and reclaim retired capacity deliberately.

**Catching test:** perform N publish/unpublish cycles with at most one live listener and require no listeners/hosted IDs remain and slot storage is independent of N. ROOT should repeat actual guest host-network binds and verify host resource return on Unix/Windows.

### network-11 — P3 / medium: the DNS grant learner treats unrelated answer records as the granted name's addresses

**Location:** `crates/dns/src/lib.rs:291`, `:302`, `:315`; consumers `crates/net/src/lib.rs:2408`, `:2865`. Parser predates the base; D192/D236 increase its use.

**Defect:** `a_records` assigns every answer-section A/AAAA record to the question name without checking its owner/CNAME chain, class or response status.

**Concrete scenario:** an upstream response has QR=1, QDCOUNT=1, question `allowed.example IN A`, ANCOUNT=1, but its only answer is `forbidden.example CH A 203.0.113.9`. `a_records` skips the owner and class, returns `("allowed.example", [203.0.113.9])`, and the policy learner authorizes .9 at allowed.example's granted TCP port. No IN address record or CNAME chain resolved the granted name there. An error response with such records is accepted too.

**Evidence and contract:** the only initial checks are QR and QDCOUNT. The owner loop only skips bytes; TYPE and RDLENGTH are the only record fields used. D192/D236 promise addresses the resolver answered **for a destination's name**. TCP and UDP callers both trust this result. The connected UDP socket and trusted resolver path prevent this from being evidence of off-path DNS spoofing; this finding requires the stated malformed/unrelated upstream response. Its operational exploitability through the user's recursive resolver was not verified, so it is medium rather than a critical default-deny claim.

**Confidence:** high for parser/policy effect, conditional for adversarial delivery.

**Fix direction:** validate response semantics and question correlation, class and applicable owner/CNAME chain before deriving authority; handle expiry as network-03 describes.

**Catching test sketch:** `assert!(!a_records(reply_with_unrelated_ch_a()).unwrap().1.contains(&forbidden));` Also test an unrelated IN owner, NXDOMAIN with records, valid compressed names/CNAME chains, A/AAAA, and propagation into `Config::allows` over both transports.

### network-12 — P3 / medium: production network controls still use chosen constants forbidden by current R03

**Location:** representative `crates/net/src/tcp.rs:44`, `crates/net/src/lib.rs:66`, `crates/init/src/dnsrelay.rs:17`, `crates/shards/src/daemon/publish.rs:290`. Mostly predates base; some paths are new/reused in this range.

**Defect:** the runtime uses fixed waits/retries/capacities/floors copied from defaults or chosen once, rather than protocol bounds or values learned where they run.

**Verified inventory in inspected production paths (one grouped policy finding, not one count per literal):**

| Code | Fixed control |
| --- | --- |
| net/src/tcp.rs:23–24, :34 | 4 MiB each-way queue ceilings, window scale 7 |
| net/src/tcp.rs:44–45, :54 | 200 ms RTO, eight retries, queue-idle retention tied to that fixed RTO |
| net/src/tcp.rs:616 | window-update threshold multiplier 2 |
| net/src/lib.rs:66 | per-turn budget 64, copied Linux dev_weight default |
| net/src/lib.rs:72–74 | UDP lifetimes 30/120 seconds and two-second stream threshold, copied conntrack defaults |
| net/src/lib.rs:102, :704 | backlog 1024 frames; Windows pending UDP 16 datagrams |
| net/src/lib.rs:2116 | 64 KiB peer-read scratch choice |
| net/src/poll.rs:67, :271 | readiness batch 256 |
| net/src/wire.rs:161 | at most eight IPv6 extension headers (no RFC-derived eight-header bound) |
| net/src/wire.rs:385, :420, :441, :443 | chosen outgoing TTL/hop limit 64 |
| netring/src/lib.rs:82, :677 | 1 MiB ring; 16 shared-memory name attempts |
| net/src/ring.rs:87, :95 | guessed 4096-byte page fallback on OS/conversion failure |
| init/src/dnsrelay.rs:17; init/src/agentdns.rs:36 | five-second waits copied from libc resolver defaults |
| shards/src/daemon/publish.rs:290 | ten bind attempts copied from dockerd |
| init/src/chooser.rs:414, :514, :537 | chosen 8192-byte diagnostic scratch, privilege-port fallback 1024 on failed observation, 16 KiB diagnostic dump buffer |

**Concrete scenarios/evidence:** a ninth otherwise valid supported IPv6 extension header is dropped by the chosen cutoff; publication gives up after ten collisions even when later ports could be available; an answered-too-late DNS ask is discarded at a borrowed five seconds; TCP retransmission/cache timing remains 200 ms regardless of observed latency/cold-start state. The code comments document several upstream defaults, but the binding brief explicitly says those are insufficient. D31/PM M210 provide useful comparisons for releasing/reusing queue memory, not derivations of these values or cold-start/uncertainty behavior. No full production-literal audit of injector/HTTP2/sign-in was completed.

**Refutation:** protocol tags/header widths, ND hop limit 255, IPv6 minimum MTU 1280, the RFC 6335 ephemeral range, RFC 7323 window-scale maximum 14 and justified alignment/power-of-two arithmetic were excluded. This finding does not require replacing true protocol invariants with learned values.

**Confidence:** high that the listed controls are fixed; exhaustive derivation research for every historical literal is not claimed.

**Fix direction:** ROOT should separate researched protocol invariants from runtime choices, derive/admit capacities from actual resources, learn timing/fairness where used with explicit cold-start uncertainty, and preserve authority independently of learning. Replace guessed fallback observations with typed failures.

**Catching validation:** check source-to-D-entry derivations for each row, then exercise different queue pressure, RTT, resolver latency and cold-start conditions; show runtime controls follow the documented observations. Include an otherwise valid nine-extension packet and a collision stream exceeding ten attempts. These runs and before/after/mutation evidence are unrun here.

## Checked and found sound

- IPv4 fragments (MF/offset) and IPv6 fragment headers are dropped before transport handling. Inspected parser reads use checked slices; the selected malformed-length paths showed no proved bounds panic.
- TCP window-scale offers are clamped to the protocol maximum 14. Established RSTs require the exact expected sequence or issue/drop a challenge; the FIN problem is distinct.
- Egress address checks canonicalize IPv4-mapped IPv6 and exclude gateway/loopback/unspecified/link-local/multicast/broadcast forms under ordinary port/name grants. The explicit host grant is restricted to its TCP port; its gateway translation is deliberate.
- Outgoing UDP sockets are connected, so arbitrary unsolicited remote sources cannot answer them. Published inbound UDP mapping count is bounded by the 16384-port ephemeral range. This does not establish a bound on all outgoing flow resources.
- Peer frame ingress checks the paired peer's source addresses and the receiving guest's destination addresses. `enable_icc=false` omits peer links and peer names.
- Explicit host publication addresses are honored; IPv4-mapped addresses are canonicalized; Unix IPv6 host listeners use V6ONLY. No proved bind-interface widening emerged from those paths. This is distinct from chooser's missing V6ONLY check.
- Ports parsing/decoding rejects zero and reversed ranges. Named EXPOSE egress filters and per-member `held`/`held_apart` checks prevent the examined ingress-as-egress and incompatible name-union configurations.
- API-run narrows the network-process union before launch; original guest member grants remain enforced. No independently proved widening arose from the inspected narrowing path.
- PM M210 (finite TCP memory/throughput/start costs), M247 (serial local DNS latency), and relevant D31/D46/D59/D61/D99/D122/D192/D236 claims were inspected. Their measured workloads do not cover the failure scenarios above; old measurements were not treated as head validation.

## Not covered

- No live reproduction, tests, policy-check script, builds, VMs, daemons, fault/chaos/stress/mutation campaigns, benchmarks or platform execution, per the binding brief. No passing readiness, performance, leak-freedom or complete R01–R16 compliance claim is made. ROOT owns those checks and the implementation evidence checklist.
- No verified P0 escape, credential exposure, cross-microVM unauthorized flow or unsolicited-UDP reply bypass was established. Absence of a reported bypass is not an exhaustive proof of default deny.
- Credential injection, CA/lease/sign-in/HTTP2 internals were not exhaustively reviewed; gateway's mostly unchanged build-protocol internals were only scoped for network relevance. TLS/HTTP credential work may need a separate assigned review.
- Guest kernel nftables/conntrack behavior is traced against the repository's pinned design/source descriptions, not executed on KVM/HVF/WHP. Zone-wrap NAT behavior specifically needs ROOT's real-kernel witness. No external source was fetched or vendor service invoked.
- Outgoing TCP/UDP table admission, TCP-slot high-water retention, Windows dialer backpressure and maximum-size IPv6 UDP reply handling merit additional bounded campaigns. OS FD/port exhaustion complicates a blanket unbounded-flow claim; network-03 and network-10 provide narrower retained-history proofs instead.
- Arbitrary constants inventory is limited to the inspected network paths and is not a certification of every production literal. API/Hex IPv6 and new chooser regressions need fresh and restored VM coverage on the supported hypervisors. D236 itself labels real-VM validation owed.

Repository status was clean when checked during review. No remediation was attempted.

## Delivery status

The user explicitly requested delivery to the main Shards agent in Kiln. The recipient was verified in Kiln as Shards, agent `01a148884a4770fdaf0111f3f792a248`, working in g121. Sending the ranked findings with `agents_send` was attempted in steer and queue modes, including idempotent retries after two separate 30-second waits. Every send returned HTTP 429 with `{"code":"backpressure","pending":3,"authority":2}`. An `agents_messages` read returned no accepted messages from this reviewer to that agent. Therefore **notification was not accepted/delivery is unconfirmed**; no claim of successful notification is made. This report retains all findings for ROOT to read when its inbox becomes available. No attempt was made to bypass Kiln's backpressure or change another agent's state.
