# inithex adversarial review

POLICY_READ — role: adversarial reviewer, area `inithex`; coordinator of this scoped read-only review. ROOT/Claude remains the overall validation coordinator. **No code or tracked files changed.**

- Head and origin/main: `2bc5f78f9e48d09ddbf4e848f9960133f6d19148`.
- Base: `79caf2d743d4c3c917f207bd650655a523847bf4`.
- Binding brief: `/tmp/g121-shot/review/brief.md`, read in full first.
- `AGENTS.md` SHA-256: `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41`.
- `docs/development-rules.md` SHA-256: `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
- `docs/development-evidence.md` SHA-256: `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63`.
- `CLAUDE.md` SHA-256: `588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010`.
- Checkout: `/Users/adalundhe/Projects/shards/.claude/worktrees/rv-inithex`.
- Method: inspect the assigned paths in `git diff 79caf2d74...2bc5f78f9`, trace head source, decisions and existing evidence; verify the relevant Linux permission checks against the kernel revision D234 cites. No builds, tests, scripts, daemons, VMs, profiling or benchmarks run, per the binding brief. Source verification below is **not live reproduction under R09**.
- Delegated read-only scopes: `names` (named network grants/spec), `lifecycle` (nesting lifecycle/device authority/standards), `mcp` (member bridge). Each read the brief and four required policy files and acknowledged HEAD/hash before reviewing. No implementation delegated.
- Ranking used: P0 critical; P1 high; P3 medium; P5 low. P2/P4 unused. Counts: **2 P0/critical, 1 P1/high, 1 P3/medium**.

## Findings, most severe first

### inithex-01 — P0 / critical — nesting workload can detach its own device ceiling

**Location at head:** `crates/init/src/run.rs:611`; supporting paths `crates/init/src/devices.rs:1316`, `crates/init/src/setup.rs:611`, `crates/shards/src/setup.rs:187`.

**Defect:** Shards attaches its only workload device filter to the same cgroup root it delegates to a nesting workload, while granting the initial-user-namespace capabilities and unrestricted BPF calls needed to remove that filter.

**Concrete scenario:** An image declares `NESTING`, runs as root, and receives `nesting=true`. In a GPU VM the command receives renderD128 alone, as D234's GPU test intends. It opens `/sys/fs/cgroup`, queries its `BPF_CGROUP_DEVICE` program IDs with `BPF_PROG_QUERY`, obtains their FDs with `BPF_PROG_GET_FD_BY_ID`, and detaches them with `BPF_PROG_DETACH`. It can then create the character node 226:0 and open the guest's ungranted card0 without the workload device check. The cgroup ceiling is gone for its descendants too. This is an intra-VM device-authorization break; an outer host escape is not claimed.

**Evidence and refutation:**

- Boot attaches `confine_by_default(WORKLOAD_CGROUP)` at run.rs:611. `WORKLOAD_CGROUP` is `/sys/fs/cgroup/run/workload` (run.rs:1236). `limit_in` replaces device policy at that same directory (run.rs:1908,1935–1939), never on an inaccessible ancestor.
- The workload enters that cgroup before `CLONE_NEWCGROUP` (run.rs:1354). Its hierarchy is writable and command moves into `/init` beneath it (init/setup.rs:611–619). The delegated root itself remains openable.
- `nesting_capabilities` is `capabilities(run) | 1 << CAP_SYS_ADMIN | 1 << CAP_NET_ADMIN` (shards/setup.rs:188). Production workload setup creates no user namespace; root applies these effective capabilities (init/run.rs:6752–6763). D234 also expressly describes initial-user-namespace authority.
- The default seccomp rule allows `bpf` with CAP_SYS_ADMIN without argument constraints (`crates/seccomp/data/default.json:633`). The nesting profile retains this rule. The attachment uses `BPF_F_ALLOW_MULTI` and `BPF_PROG_ATTACH`, without a protective BPF link (devices.rs:1316–1324).
- Linux's query and get-FD permission checks are satisfied by those grants; the device detach dispatch accepts the matching FD. See pinned [kernel/bpf/syscall.c](https://raw.githubusercontent.com/torvalds/linux/79eb6679ef6a0d5ea68da53a8321fb1dc72299b9/kernel/bpf/syscall.c) functions `bpf_prog_query` (4510), `bpf_prog_get_fd_by_id` (4681), `bpf_prog_detach` (4444).
- `find_detach_entry` accepts a matching program FD for a MULTI attachment; `cgroup_bpf_prog_detach` obtains the target cgroup from its FD and removes the program without refusing a namespace root. `nsdelegate` resource-file restrictions do not mediate this syscall path. See pinned [kernel/bpf/cgroup.c](https://raw.githubusercontent.com/torvalds/linux/79eb6679ef6a0d5ea68da53a8321fb1dc72299b9/kernel/bpf/cgroup.c), lines 1184–1216 and 1476–1495.
- No ancestor device filter was found in this path. Keeping the program FD alive in init does not prevent another FD holder from detaching it. A caller's custom restrictive seccomp profile could block the attack; the documented default permits it.
- The repository's existing `crates/testguest/src/nesting.rs:513–547` identifies card0 as the forbidden target, and `crates/devcgroup/src/lib.rs:464` permits character-device mknod. The current test only adds child programs; it never removes the authoritative one.

**Contract:** D234, “Its GPUs' identities,” says the workload's device filter “still bounds any device programs” it puts on boxes. Research f20 establishes composition with child programs but does not establish protection against detachment of the parent program. This violates the user's named-device requirement.

**Confidence:** High in the source-level authorization gap. Actual detach/open results on the shipped guest kernel, LSM variants and GPU fixture are **unverified**, not reproduced.

**Fix direction:** Attach the immutable ceiling to an ancestor outside the delegated cgroup namespace, delegate a child, and keep authoritative device updates at the ancestor. Preserve child programs for nested runtimes. Keeping an FD or changing only MULTI flags is insufficient.

**Catching test:** In ROOT's disposable GPU VM, run the default grant profile as root; verify ungranted card0 open is denied; query/get-FD/detach all visible device programs; require card0 open still fails while renderD128 works and child stricter programs still attach. Record actual detach results, remaining attachment state, forbidden open and cleanup. Existing `a_granted_runs_boxes_...` GPU case should include this hostile variant. A GPU-free twin can attempt an ungranted real guest block device.

### inithex-02 — P0 / critical — per-member narrowing overlooks reach through image-wide ports

**Location at head:** `crates/shards/src/api_run.rs:814`; supporting gap `crates/agentfile/src/agentfile.rs:2291`.

**Defect:** `held_apart` compares a member's explicit names only, ignoring that the member's unchanged image-wide port grants also reach retained names belonging to other members.

**Concrete inputs:**

```dockerfile
FROM alpine
AGENT a FROM ./a
AGENT b FROM ./b
NETWORK --egress=api.example:443 named
EXPOSE 443 AS egress FOR named
CONNECT a WITH a ON named
NETWORK --egress=443 wide
EXPOSE 443 AS egress FOR wide
CONNECT a WITH a ON wide
CONNECT b WITH b ON wide
```

Request `destinations=Only(["api.example:443"])`, `egress=Only([])`, `mcp=Only([])`, DNS unrestricted=false, and `members=Some([a -> ["api.example:443"], b -> []])`. A resolves the provider legitimately. B connects to the same known IP on 443. Expected: b has no provider grant. Actual source path: the request is accepted, b's image port rule admits its SYN, and the shared host policy admits the learned provider IP:443.

**Evidence and refutation:**

- `tcp_reach` retains a's named grant and both wide port grants separately (agentfile.rs:2194–2222).
- `held` gives wide ports precedence: `return Some((r.member, None))` (2291–2292). Both members therefore have `None`, no by-name member is found, and lines 2304–2305 skip this port. Build and daemon revalidation both pass.
- `named_reach` returns only `r.named` (2253–2257). The run host records a with the provider and b with no names. `held_apart` filters and compares only these names (api_run.rs:814–830), which match the request exactly.
- The guest keeps the image's policy; narrowing is enforced only in the shared host network process (api_run.rs:849–855). Init's b.egress therefore still includes TCP443 (`crates/init/src/netplan.rs:563–564,593–608`; forwarding rules in links.rs:2125–2135,2385–2399).
- The request really drops all unrestricted host port grants (api_run.rs:594–624), but retains the named provider. DNS learning adds its IP:443 (net/lib.rs:2403–2413), and `Config::allows` admits it (613–628). The network process has no per-member identity to distinguish b.
- B need not spoof a source, bypass DNS permissions, or keep an unrestricted host grant: a known provider IP is enough. `Agentfile::refuse` checks command options, not this effective member reach. Single-member wide-plus-named reach is intentionally allowed by existing tests, so a blanket parser ban is not the only fix.

**Contract:** D246, “Held,” promises that every member, including one omitted or granted nothing, reaches exactly its requested retained names. Its reasoning assumes `held` makes all members granted a port by any route comparable; the all-wide branch breaks that assumption. CLAUDE's default-deny rule is also violated.

**Confidence:** High, deterministic source path. The actual forbidden connection has **not** been exercised.

**Fix direction:** Compute every member's effective reach over the retained union, including kept endpoints covered by its image-wide port ranges, before comparing to `MemberReach`; otherwise refuse combinations the shared policy cannot enforce.

**Catching test sketch:**

```rust
let image = verified_agentfile_for_the_fixture_above();
let narrowing = keep_provider_but_no_wide_ports_and_give_b_nothing();
assert!(matches!(held_apart(Some(&image), &narrowing),
    Err(No(Cause::Widening, _))));
```

ROOT's live twin should let a resolve/dial a controlled provider, attempt b's direct-IP dial, and observe refusal; removing the effective-reach guard must expose that exact forbidden connection.

### inithex-03 — P1 / high — failed MCP bridge waits forever for open harness stdin

**Status (2026-11-13):** fixed in f7d83f73a (red `bridge_peer_end_releases_open_stdin`, `bridge_output_failure_releases_open_stdin`; Linux x86_64 r2/r4; [evidence](../../research/measurements/inithex-fixes/README.md)).

**Location at head:** `crates/init/src/mcp.rs:201–205` (join at 203); entry at 182.

**Defect:** The bridge joins its blocking stdin-copy thread before closing stdout or returning a socket/output error, so a harness waiting for a response cannot observe the connection's end.

**Concrete scenario:** A granted harness starts `/run/shards/mcp` with pipes, writes an MCP request and keeps stdin open awaiting the response. The controller aborts/drops its lane, or the host rejects the connection for capacity (`daemon/managed/mcp.rs:147–160`). The transport ends; socket-to-stdout copy ends or errors. The bridge then waits in `up.join()` while the helper waits for more harness stdin. Stdout remains open and exit status remains unavailable; the harness receives neither the expected answer nor EOF and keeps stdin open. A stdout BrokenPipe while stdin is open similarly prevents the promised failure return.

**Evidence:**

```rust
// crates/init/src/mcp.rs:197–205
io::copy(&mut input, &mut up_socket)?;
// ... main thread:
let down = io::copy(&mut down_socket, &mut output).and_then(|_| output.flush());
let up = up.join()
    .unwrap_or_else(|_| Err(io::Error::other("its input's copy ended unwound")));
down.and(up)
```

`bridge()` supplies actual process stdin/stdout at 182; its failure status at 184 is unreachable until the join returns. Shutting the socket cannot wake a read on the separate stdin pipe. Independent normal half-close semantics do not require withholding stdout EOF; errors/cuts also require cancellation. The existing bridge test supplies a byte slice that reaches EOF immediately (mcp.rs:289–302), so it cannot detect this cycle. This entire module is new in the reviewed range.

**Contract:** D254 “In the member” says each direction ends as its source ends (architecture.md:29275–29277); the pairing says connection termination ends the member bridge and a dropped lane hangs it up (29406–29410). R04/R05 also require completing failure/cancel transitions and examining waits.

**Confidence:** High in the source-level wait cycle. No live subprocess or VM reproduction performed.

**Fix direction:** Use descriptor-based cancellable copying; propagate stdout EOF immediately and make faults/cuts wake the opposite operation, preserving normal independent half-closes. An uncancellable scoped thread join cannot implement this contract.

**Catching test:** Start the actual bridge with piped stdio; the server accepts and reads one request, then ends the connection while the parent retains the stdin writer. Require stdout EOF and terminal failure/cut behavior without the parent first closing stdin. Twin: close the parent's stdout reader while retaining stdin and require bridge failure. Bound through ROOT's external judged wait; retain cleanup results.

### inithex-04 — P3 / medium — accepted absolute-FQDN MCP grant never matches DNS

**Status (2026-11-13):** fixed in 79019fa55 (red `mcp_absolute_name_matches_its_dns_question` in shards-agentfile and shards-init; [evidence](../../research/measurements/inithex-fixes/README.md)).

**Location at head:** `crates/agentfile/src/agentfile.rs:1487`; twin `crates/init/src/netplan.rs:132`.

**Defect:** Remote MCP URLs preserve a hostname's terminal root dot while DNS permission and address-learning checks compare the dotless wire name, so an accepted grant cannot be used.

**Concrete scenario:** `MCP tools FROM https://api.example./mcp FOR a`, with no unrestricted DNS/egress grant. Build and runtime accept it and grant `api.example.:443`. An ordinary A/AAAA question for that absolute hostname becomes `api.example` when decoded, is refused by the member resolver, and yields no learned destination.

**Evidence/refutation:** Source parsing accepts HTTP(S) URLs (`agentfile.rs:385–386`). Both endpoint parsers only ASCII-lowercase the host (`host.to_ascii_lowercase()` at the locations above). `dns::query_name` joins labels without a terminal root dot (`crates/dns/src/lib.rs:268–273`). Init exact-compares that name to its granted list (`init/agentdns.rs:56–57`); host `Config::asks` repeats the check (net/lib.rs:577–580). Even adding unrestricted DNS does not fix learning: net/lib.rs:2411 uses `host.eq_ignore_ascii_case(&name)`, which does not remove the dot. This is undergrant, not an isolation escape. NETWORK destinations explicitly reject this spelling, but remote MCP URLs do not. The parser predates the base; current D236/D246 continue to derive and enforce grants from it, and the brief includes head behavior.

**Contract:** D236's “Names” and “Held” promise the member can resolve/reach its declared remote MCP endpoint, checked at build and run alike. An unsupported spelling must at least be refused consistently instead of accepted as an unusable grant.

**Confidence:** High, source-level; no live DNS exercise.

**Fix direction:** Canonicalize supported DNS URL hostnames consistently at every boundary, or refuse unsupported forms at both build and run. Keep IP-literal parsing separate; do not turn normalization into broader authority.

**Catching test:** Compare the parsed endpoint of `https://api.example./mcp` to the supported canonical form, then compare the member's granted names with `query_name` of a real wire question. An E2E fixture should reach only that host's granted port using an absolute-FQDN URL (or fail at build with the documented typed refusal if intentionally unsupported).

## Checked and found sound

These are source-review observations, not executed passing gates.

- NESTING needs image declaration and run consent. Build/daemon refuse AGENT/HARNESS coexistence; init rechecks the kept image. `--pid host`, `--cgroupns host` and joining runs are separately refused. Execs enter the delegated leaf, preserving the empty root required for controllers.
- Init reads grants and domain config in the kept built layer through in-root resolution. `cp` into the workload's writable `/.agentfile.json`/osi.json does not directly change those grants. Daemon rereads the opened EROFS root and digest instead of a stale activation cache.
- `-v`/`--mount` replacement volume paths reject a tmpfs/file where domains need a directory; guest volume destinations resolve within the image and reject system/scratch collisions. `commit` can preserve edited files/labels, but the next labeled image run revalidates its opened root/digest; no bypass of the named/nesting run checks was established through commit.
- D234 preserves Hex's direct-boot PID1 as an alternative VM boot path. No code removal of it was found in this checkout. Workload namespace PID1 death kills namespace descendants; with `--init`, Shards' reaper forwards signals to the command and the run's terminal path kills the remaining processes. These conclusions still need the requested death/fault campaigns.
- Readiness has explicit states, checked clock conversion, BOOTTIME timer and sender credentials/cgroup authorization. PIPE_BUF and the one-nanosecond timer floor are tied to system-call contracts. GPU metadata uses the shared identity grammar and a no-follow/create-new file path.
- Ordinary differing named grants on one TCP port are refused at build and daemon load. ASCII case is normalized. NETWORK destination wildcard, IPv6 and terminal-dot forms are refused. IPv4-mapped addresses are canonicalized; host loopback/gateway access stays port-specific. D236 deliberately enforces DNS-derived addresses, rather than TLS SNI/HTTP Host identity.
- D254 bridge sockets are member-owned mode0600 in separately mounted member directories; no sibling socket/FD route was found. Init prepends the token/member identity; the run host checks granted members. Member bridge requests require control and explicit member selection; the bridge adds no implicit network grant.

## Not covered

- **No live reproduction, builds, tests, policy script, real E2E, chaos, sweeps, benchmarks or profiling.** The brief forbids them. Proposed tests above were not written or run. `git status --short` was empty after the review. ROOT owns validation/fixes; nothing here is readiness/merge evidence.
- Hex's source is outside the allowed checkout and was not read. Its PID1/node/relay behavior and interoperability with shards-init are verified here only against Shards' recorded contract, not Hex's executable behavior.
- No hardware/kernel/LSM confirmation of device detachment, real card0/block-device access, process reaping/signal/death behavior, restore/commit effects, GPU identity files, or provider/MCP flow on HVF, KVM or WHP. All seven target results remain unverified.
- D234/D236/D246/D254 explicitly retain real-VM evidence owed. No completion/performance claim is inferred from their unit tests or policy receipts. R08/R14 measurements, allocations/faults and platform parity were not measured in this read-only assignment.
- DNS TTL invalidation, RR-owner validation and repointed answers were inspected only partially: D236 currently says learned addresses are admitted from then on. No stronger origin/authentication guarantee or independent rebinding finding is claimed. Full punycode/IDNA client normalization compatibility and IPv6 remote-MCP variants were not exhaustively verified.
- No exhaustive R01/R03/R04 audit of every changed path; no all-code panic/constant/error-free claim. Guest MCP relay resource exhaustion from connection-driven thread creation needs a separate bounded coordinator campaign and is not counted as a verified finding.
- Supporting read-only notes: `/tmp/g121-shot/review/inithex-lifecycle-notes.md`, `/tmp/g121-shot/review/inithex-names-notes.md`, `/tmp/g121-shot/review/inithex-mcp-notes.md`.

## Coordinator notification

Attempted to send the ranked results to main Shards agent `01a148884a4770fdaf0111f3f792a248` through Kiln `agents_send`, immutable key `inithex-review-2bc5f78f9-final-20261113`. All three attempts (including a delayed retry) were rejected with HTTP429, `backpressure`, `pending:3`, `authority:2`. Kiln has **not accepted custody or confirmed delivery**. The report remains available at this path for the coordinator; no queue entries were removed or bypassed.
