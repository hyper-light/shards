# Managed runtime: checked evidence ledger

Research record `01a1c33a30dd72afa53e3cbda62339cb`, retrieved 2026-11-10 UTC.
These are Kiln's automated citation-support grades, not security assurance or benchmark scores.
A found quotation verifies the cited text exists; it does not prove every inference or implementation guarantee.
Pinned historical protocol facts are scoped to that revision. Falcon architecture remains vendor-reported (F/26), with no independent implementation audit here.
Findings f13–f15 add the kernel-prerequisite and native-reaping obligations used by the implementation plan.

**Research:** Design native, typed, rootless-host Shards management of AGENT/HARNESS process trees at arbitrary depth, tool-call mediation and telemetry, mandatory buffered per-flow network/output interception and relay, safe output sanitization, and low overhead. Build upon Shards' Linux guest namespaces/cgroups/seccomp/Landlock, owned process lifecycle, per-domain MCP servers, existing network stack and typed host APIs. Compare Falco/Sysdig, CrowdStrike Falcon Sensor, Cilium Tetragon, kernel enforcement primitives and native Codex/Claude tool interfaces. Distinguish observable evidence, actual pre-action enforcement, after-the-fact detection, vendor claims, and unknowns; preserve TLS, one-binary delivery and all eight host targets. Capture primary-source evidence and an actionable validation/benchmark plan; no unsupported universal interception or performance claims.

15 findings (A 8, B 4, C 2, F 1) from 15 sources (8 internal, 7 external)

- **A · 90 (high)** The inspected Shards launcher creates domains directly into cgroups with private namespaces and applies identity, capability, Landlock and seccomp restrictions before exec. [1]
  _1 independent source (code); 1 of 1 quotes found where cited_
  - [1] "CLONE_INTO_CGROUP" (start and child) · quote found
- **A · 90 (high)** The inspected domain child duplicates the same output descriptor onto stdout and stderr. [1]
  _1 independent source (code); 1 of 1 quotes found where cited_
  - [1] "libc::dup2(out, 1) < 0 || libc::dup2(out, 2) < 0" (child stdio setup) · quote found
- **A · 90 (high)** Shards process connector history is explicitly a record, not an enforcement boundary; it excludes threads and represents lost or unavailable data. [2]
  _1 independent source (code); 2 of 2 quotes found where cited_
  - [2] "A record, not" (module contract) · quote found
  - [2] "Threads are left out" (module contract) · quote found
- **A · 90 (high)** The private per-domain server authenticates its domain and checks messaging/file-transfer grants in its dispatcher; external MCP discovery returns endpoints rather than proxying every external tool operation. [3]
  _1 independent source (code); 1 of 1 quotes found where cited_
  - [3] "that socket, which init made visible to its domain alone" (module contract) · quote found
- **B · 72 (good)** Guest Switch/Gate uses namespace routing and nftables filtering. Extending only the external host TCP relay cannot establish mediation of every internal domain flow. [4]
  _1 independent source (code); 1 of 1 quotes found where cited; inferred from them_
  - [4] "nftables, stateless, decide by link what may cross it" (module contract; Switch and Gate) · quote found
- **C · 68 (moderate)** Falco documents dropped events and consequent damage to reconstructed metadata; event export alone cannot establish complete pre-action admission. [5]
  _1 independent source (official docs); 1 of 1 quotes found where cited; inferred from them_
  - [5] "which in turn might affect the rules that depend on that metadata." (Introduction) · quote found
- **A · 85 (high)** Tetragon distinguishes denying through return override from SIGKILL; a signal during write does not guarantee that data was not written. [6]
  _1 independent source (official docs); 1 of 1 quotes found where cited_
  - [6] "does not guarantee that the data will not be written to the file." (Signals) · quote found
- **A · 85 (high)** Seccomp user-notification inspection followed by CONTINUE cannot safely authorize mutable pointer arguments without addressing target-memory TOCTOU. [7]
  _1 independent source (official docs); 1 of 1 quotes found where cited_
  - [7] "The memory may have been modified by another thread" (getTargetPathname example; CONTINUE security notes) · quote found
- **F · 26 (very low)** CrowdStrike's architecture account describes kernel presence continuing operation and restarting a tampered-with user-mode service; it remains a vendor claim, not independent proof of universal coverage. [8]
  _1 independent source (a vendor); 1 of 1 quotes found where cited; some out of date_
  - [8] "as well as attempt to restart it" (User-Mode-Only Security Products Could Be Tampered With) · quote found
- **C · 68 (moderate)** MCP 2025-11-25 says tool annotations are untrusted unless they come from a trusted server; its tool results include rich content types. [9]
  _1 independent source (a standard); 1 of 1 quotes found where cited; some out of date_
  - [9] "clients MUST consider tool annotations to be untrusted unless they come from trusted servers." (Tool annotations) · quote found
- **A · 85 (high)** OWASP logging guidance calls for input validation, context-appropriate encoding, secret minimization and testing resource exhaustion. [10]
  _1 independent source (official docs); 1 of 1 quotes found where cited_
  - [10] "Encode data correctly for the output (logged) format" (Event collection) · quote found
- **A · 90 (high)** OpenTelemetry's stable log model distinguishes the event occurrence Timestamp from the collection-system ObservedTimestamp. [11]
  _1 independent source (a standard); 1 of 1 quotes found where cited_
  - [11] "Time when the event was observed by the collection system." (ObservedTimestamp) · quote found
- **B · 79 (good)** Both checked-in kernel base configs disable NFT_TPROXY and NFT_SOCKET. A transparent-relay plan must verify effective kernel features rather than assume these mechanisms are available. [12][13]
  _2 independent sources (code); 2 of 2 quotes found where cited; inferred from them_
  - [12] "# CONFIG_NFT_TPROXY is not set" (Netfilter config) · quote found
  - [13] "# CONFIG_NFT_TPROXY is not set" (Netfilter config) · quote found
- **B · 72 (good)** Linux 6.18 nft_tproxy validates the PREROUTING hook and handles TCP/UDP. A switch rule alone does not demonstrate same-domain loopback interception. [14]
  _1 independent source (code); 2 of 2 quotes found where cited; inferred from them_
  - [14] "return nft_chain_validate_hooks(ctx->chain, 1 << NF_INET_PRE_ROUTING);" (nft_tproxy_validate) · quote found
  - [14] "pkt->tprot != IPPROTO_TCP" (nft_tproxy_eval_v4) · quote found
- **B · 72 (good)** The current domain launcher creates a PID namespace and directly execs the selected actor there. Independent orphan reaping is therefore a management proof obligation, not an established property of an arbitrary harness. [15]
  _1 independent source (code); 2 of 2 quotes found where cited; inferred from them_
  - [15] "libc::CLONE_NEWPID" (start clone_args) · quote found
  - [15] "libc::execve(" (child final exec) · quote found

**Sources**

1. Shards ae2acbdbb: crates/init/src/domains.rs (unchanged through ccf718164) (code, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/managed-runtime-research/crates/init/src/domains.rs · 2026-11-09
2. Shards ae2acbdbb: crates/init/src/procevents.rs (unchanged through ccf718164) (code, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/managed-runtime-research/crates/init/src/procevents.rs · 2026-11-09
3. Shards ae2acbdbb: crates/server/src/server.rs (unchanged through ccf718164) (code, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/managed-runtime-research/crates/server/src/server.rs · 2026-11-09
4. Shards ae2acbdbb: crates/init/src/links.rs (unchanged through ccf718164) (code, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/managed-runtime-research/crates/init/src/links.rs · 2026-11-09
5. Falco Actions for Dropped System Call Events (official docs, external) — https://falco.org/docs/concepts/event-sources/kernel/dropped-events/
6. Tetragon enforcement: override versus signal (official docs, external) — https://tetragon.io/docs/concepts/enforcement/
7. Linux seccomp user notification manual (official docs, external) — https://man7.org/linux/man-pages/man2/seccomp_unotify.2.html
8. CrowdStrike's Kernel Access and Security Architecture (a vendor, external) — https://www.crowdstrike.com/en-us/blog/tech-analysis-kernel-access-security-architecture/ · 2024-08-09 · out of date
9. MCP 2025-11-25 tools content and trust rules (a standard, external) — https://modelcontextprotocol.io/specification/2025-11-25/server/tools · 2025-11-25 · out of date
10. OWASP Logging Cheat Sheet (official docs, external) — https://cheatsheetseries.owasp.org/cheatsheets/Logging_Cheat_Sheet.html
11. OpenTelemetry stable logs data model (a standard, external) — https://opentelemetry.io/docs/specs/otel/logs/data-model/
12. Shards 9d562993 checked-in x86_64 kernel base (code, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/g121-i7/resources/kernel/firecracker-x86_64-6.18.config
13. Shards 9d562993 checked-in aarch64 kernel base (code, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/g121-i7/resources/kernel/firecracker-aarch64-6.18.config
14. Linux 6.18 nft_tproxy implementation (code, internal) — https://raw.githubusercontent.com/torvalds/linux/v6.18/net/netfilter/nft_tproxy.c
15. Shards 9d562993 domain clone and direct exec (code, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/g121-i7/crates/init/src/domains.rs
