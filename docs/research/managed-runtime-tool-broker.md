# Native tool mediation for hostile agents and harnesses

Research date: 2026-11-09. Shards source inspected at `ae2acbdbbc2defaa9e3ff70c4b07939d3307a9c8`. This is a research and design proposal, not an implementation or a security certification. No builds, provider requests, credential reads or performance measurements were performed for this note. Local `codex --version`, `codex --help` and `codex app-server --help` reported `codex-cli 0.167.0` and experimental app-server support.

## The contract Shards can actually enforce

The useful promise has two parts:

1. **Every permitted external effect crosses a Shards-controlled boundary.** Kernel/object confinement applies to the complete workload, including descendants that do not identify themselves as agents. Shards admits file, process, IPC, network and device access according to the domain's grants. Its trusted brokers own the operations they perform and the resources they return.
2. **Every brokered tool operation has typed, owned semantics.** When Shards is the tool executor, it knows the validated request, the principal whose grant admitted it, the objects it accessed, its outputs and its end state. Harness adapters can additionally supply useful conversation/subagent correlations, with their provenance preserved.

These are proposed invariants. An arbitrary binary can implement a function in its own address space, use a file it may already read, or encode an application request inside an allowed encrypted connection. No fetched harness interface establishes a universal semantic interceptor for those actions. A strict semantic policy must therefore refuse the unmediated external route or explicitly declare that route outside semantic coverage. It must not describe an opaque TLS flow, a guessed command-line label or a missing telemetry event as a verified tool call. Codex itself warns that child environment call IDs are mutable correlation labels, while Shards' existing process recorder explicitly calls itself a record rather than a boundary. [Codex executor metadata](https://github.com/openai/codex/blob/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a/codex-rs/exec-server/README.md), [Shards process recorder](../../crates/init/src/procevents.rs)

The public launch contract remains the approved simple root `agent.json` or `harness.json`, containing `entrypoint` and optional `options`. This note introduces no required public fields, no Docker `ENTRYPOINT` override, and no installation through `RUN` or `VOLUME`. Adapter selection, coverage evidence and enforcement state belong to Shards' internal checked launch plan. A user-provided string must never silently acquire shell-evaluation semantics; startup parsing/argument compatibility belongs in the manifest implementation. A native owned launch also does not need `nohup` to detach it from its owner.

## Evidence levels

Use these levels in implementation and exported records:

| Level | What establishes it | What it does not establish |
| --- | --- | --- |
| Broker-enforced | A trusted Shards broker authorizes and executes the operation through owned objects | That returned content is truthful, harmless or a new authorization |
| Kernel-observed | A kernel event/object identity supplies process or effect information | A model's tool name, conversation intent or completeness when events are lost |
| Adapter-reported | A pinned native protocol reports a tool/subagent/result | That a hostile harness used this protocol for every action |
| Unattributed | A domain stream or granted object carries bytes without an owned semantic context | Which descendant, tool or in-process function produced them |

These are proposed distinctions, not protocol wire values. Trust must not increase merely because text contains a tool ID, a trace header, a JSON object, an MCP annotation or a recognizable log prefix.

## Fit with the existing Shards architecture

Shards already has the right place for a trusted broker: the per-domain in-VM server. Its private socket is visible to one domain; each instance has its own mutual-TLS identity, separate UID and confinement. Its `call` dispatcher checks grants before messaging or reading a handed file, and `answer` checks ownership of the outstanding request. The actor can read its own client certificate/key, so that credential authenticates the **domain's capability**, not an honest executable or a unique subprocess within it. [server identity and dispatcher](../../crates/server/src/server.rs), [D60 and D139](../design/architecture.md)

Code mode already demonstrates the deep-module approach: a confined QuickJS worker has its program socket, delegates allowed tool operations to the instance, and does not inherit the instance's keys. The helper is an embedded Shards component, not a user-installed wrapper. Retain this ownership pattern when adding process/filesystem/network tools; validate the concrete operation in the broker rather than trusting JavaScript or a model to enforce grants. [sandbox protocol and worker confinement](../../crates/server/src/sandbox.rs)

Two current limits matter. The `mcp` discovery tool returns external MCP endpoints/commands for the caller to use; it is not presently a general proxy for every external tool invocation. Domain stdout and stderr currently share one pipe that init forwards as prefixed lines. That pipe cannot preserve their channel distinction, binary framing or exact writer identity, and cannot safely become a multiplexed control protocol by recognizing JSON in output. These require explicit implementation changes. [MCP discovery and result construction](../../crates/server/src/server.rs), [domain launch/output](../../crates/init/src/domains.rs)

Process fork/exec/exit recording covers domain membership independently of harness terminology, but it documents dropped-event accounting, fallback argv sources and exclusion of threads. Correlate its facts with broker launches; do not treat it as proof that every semantic tool/subagent was identified. The domain namespace/cgroup model must remain effective when the recorder is unavailable or saturated. [process recorder](../../crates/init/src/procevents.rs), [domain confinement](../../crates/init/src/domains.rs)

## Codex 0.167.0: native seams and their limits

The locally installed version and the published `rust-v0.167.0` source tree agree on app-server's remote Code Mode host option. The fetched tag resolved to source tree `086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a`. Current documentation is mutable; pin the deployed binary, protocol/schema revision and compatibility tests, not only the product name. [published source tree](https://github.com/openai/codex/tree/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a), [app-server argument validation](https://github.com/openai/codex/blob/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a/codex-rs/app-server/src/code_mode_host.rs)

### App-server integration

App-server has a native structured request/notification protocol. Experimental dynamic tools are client-executed: `item/tool/call` requests can be answered by a Shards implementation. Command/file approval requests can pause the corresponding action when the configured approval path is reached. Conversely `item/started`, completed items, output deltas and collaboration items are reported observations; their existence is not an admission barrier. Neither a completed-item record nor streamed command output reverses an effect. [official app-server protocol](https://learn.chatgpt.com/docs/app-server)

The supplied Codex dangerous flag is significant: local 0.167.0 help says `--dangerously-bypass-approvals-and-sandbox` skips confirmation and command sandboxing for an externally sandboxed deployment. Consequently Shards must continue enforcing its own grants and must not depend on receiving approval requests for all actions. Dynamic-tool execution may still be brokered, but that does not make every built-in operation a dynamic tool. Native protocol integration is compatible with strong external isolation; it is not a replacement for it. [official CLI reference](https://learn.chatgpt.com/docs/developer-commands?surface=cli)

### Code Mode host is not the general process executor

The separate HTTP/2 gRPC CodeModeHost service owns JavaScript sessions/cells and delegates nested tool calls **back to the session owner**. `SubscribeToToolCalls` supplies invocation, execution and cell IDs; `CompleteToolCall` provides its answer. The delegate waits for completion/cancellation. Moving the JavaScript host therefore does not, by itself, move or intercept every shell/file/network operation. It supplies a genuine typed seam that a compatible Shards integration could implement, with the tool execution endpoint still explicitly mediated. [pinned gRPC schema](https://github.com/openai/codex/blob/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a/codex-rs/code-mode-protocol/src/grpc/codex.code_mode.v1.proto), [pinned delegate implementation](https://github.com/openai/codex/blob/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a/codex-rs/code-mode-host/src/grpc/delegate.rs)

The schema deliberately separates large tool responses from session control. Cancellation can arrive before its tool request on another HTTP/2 stream; cell closure includes a final tool-call sequence. An adapter must handle these orders without reviving canceled calls or retaining unlimited early-cancellation tombstones. The remote-host URL accepts a host and optional port, not embedded credentials/path/query. App-server client authentication and Code Mode host authentication are separate surfaces; the inspected URL/parser alone does not prove that a remote host is authenticated. [gRPC lifecycle/schema](https://github.com/openai/codex/blob/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a/codex-rs/code-mode-protocol/src/grpc/codex.code_mode.v1.proto), [URL parser](https://github.com/openai/codex/blob/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a/codex-rs/app-server/src/code_mode_host.rs)

### Native execution protocol is a stronger effect seam

Codex also publishes a separate exec-server protocol for managed processes and filesystem operations, including buffered process reads and output/exit notifications. Its process launch takes an argument vector. A Shards-native compatible adapter could execute these requests through owned child jobs and checked file objects, instead of supervising a shell CLI. This is a candidate integration, not a verified drop-in implementation. Inventory every method and any local fallback before claiming full executor coverage. [pinned executor implementation/exported types](https://github.com/openai/codex/blob/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a/codex-rs/exec-server/src/lib.rs), [pinned protocol envelope](https://github.com/openai/codex/blob/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a/codex-rs/exec-server-protocol/src/rpc.rs)

Executor child metadata `CODEX_THREAD_ID`/`CODEX_TOOL_CALL_ID` is explicitly correlation-only and can be changed by commands or descendants. Shards must bind its own launch record to the actual child object before executing it, not authorize by those environment variables. Remote executor forwarding can transport encrypted Noise payloads without parsing the RPC; being on that forwarding path does not itself confer semantic visibility. [executor metadata and relay contract](https://github.com/openai/codex/blob/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a/codex-rs/exec-server/README.md)

## Claude Code / Agent SDK: useful gates, bounded coverage

Claude's current permission flow evaluates hooks, deny/ask rules, modes and allow rules before the final `canUseTool` fallback. `bypassPermissions` and whole-tool allow rules can approve before that callback. Explicit denials and some interactive/critical cases still apply, so calling the dangerous flag either a universal bypass or a universal callback is inaccurate. A `PreToolUse` denial can gate a harness tool call even in bypass mode. [official SDK permissions](https://code.claude.com/docs/en/agent-sdk/permissions)

`PreToolUse` sees parameters before processing model tool calls, including built-in and MCP tools. It does not fire for automatic `@file` insertion or `EndConversation`; `FileChanged` is after the write and has no blocking decision. Hooks observe the harness's own tool machinery, not arbitrary syscalls, writes inside a Bash child, or hidden networking inside a tool executable. HTTP/MCP hook mechanisms permit native integration without a Bash supervisory script, but remain behavior of the configured harness. [official hooks reference](https://code.claude.com/docs/en/hooks)

Failure behavior must be version-tested. Command/HTTP hooks generally continue on failure unless configured to block; current docs require Claude Code 2.1.295+ for `onFailure: "block"`. SDK callback exceptions/invalid returns also ordinarily continue. SDK callback `PreToolUse` **timeouts** block the call; that is different from an exception. SDK `on_failure="block"` requires Python 0.2.177+ or TypeScript 0.3.317+, and some lifecycle events ignore it. No callback configuration establishes a kernel boundary against a hostile harness that omits it. [hook failures](https://code.claude.com/docs/en/hooks#block-the-action-when-a-hook-fails), [SDK callback failure and timeout rules](https://code.claude.com/docs/en/agent-sdk/hooks)

Published Python SDK types corroborate the distinction: its `can_use_tool` documentation and shadowing warning say earlier auto-approval can skip it; `HookMatcher` has explicit failure policy. These are interface/source observations, not runs against a pinned Claude binary. The inspected SDK commit was `3cf3446b0fa6c97042e85b245bba07459f22f946`. [pinned SDK types](https://github.com/anthropics/claude-agent-sdk-python/blob/3cf3446b0fa6c97042e85b245bba07459f22f946/src/claude_agent_sdk/types.py)

Current Claude documentation supports nested subagents, configurable depth, resumed/background agents and forks with different reporting behavior. Shards must therefore not depend on an old claim that subagents cannot delegate, nor mistake one harness's current depth cap for the OS boundary. A logical subagent may also be an internal context rather than an OS child. Only an adapter knows that logical edge; the kernel knows actual processes/objects. [official subagent lifecycle](https://code.claude.com/docs/en/sub-agents)

## MCP is an interoperability protocol, not grant authority

The broker should validate tool input against the negotiated schema and retain typed errors. Tool annotations are hints from their source and are not inherently trustworthy. A remote tool claiming read-only/idempotent cannot grant itself filesystem/network authority or bypass Shards' policy. Descriptions, errors and results remain untrusted model input. [MCP 2025-11-25 tools](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)

Keep protocol revisions explicit. The fetched 2026-07-28 tools specification permits any JSON value in `structuredContent`, while 2025-11-25 describes an object. Do not deserialize both into a lossy string-only result. The newer specification also explains that an application state handle is not itself authorization; validate ownership on every call. Internal broker sessions may exist regardless of the negotiated MCP transport model. [MCP 2026-07-28 tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)

Preserve all content variants: text, image, audio, resource link, embedded text/blob resource, structured value, annotations and error state. A resource link is a reference to a second access, not permission to fetch it. Resolve it through a fresh checked operation; never allow `file:`/internal HTTP references to read the broker's own namespace. Validate encoding and declared/actual lengths before expensive decoding. [MCP content types](https://modelcontextprotocol.io/specification/2025-11-25/server/tools), [MCP resource-security guidance](https://modelcontextprotocol.io/docs/2025-11-25/tutorials/security/security_best_practices)

HTTP bearer tokens must be intended for the recipient MCP server; MCP explicitly rejects accepting/passing unrelated upstream tokens as a shortcut. Authentication at one connection cannot become permission to act as another domain or contact another service. [MCP authorization](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization), [token-passthrough guidance](https://modelcontextprotocol.io/docs/2025-11-25/tutorials/security/security_best_practices)

## Proposed owned broker model

The following is an internal contract proposal, not a decided public API.

### Identity and admission

An admitted `ToolCall` contains broker-issued call identity; immutable run/domain identity; an optional broker-owned parent call/delegation handle; pinned adapter/protocol identity; resolved tool capability; validated arguments; policy revision; acquired resource credits; and typed cancellation state. Application `tool_use_id`, thread IDs, names and trace headers occupy a separate **reported correlation** field. A peer cannot set its principal, replace the tool registry, invent a parent grant or complete another peer's call by choosing a matching string.

Resolve an operation to a concrete capability before its effect: an already checked directory/file handle, a broker-managed connection, a process-launch plan, or a tool endpoint owned by an approved broker. A grant is checked against the actual destination/object, not a display name or an earlier pathname check. Kernel peer identity and the private endpoint select the domain; payload identity does not. No secrets/control handles are inherited by the launched child. Close surplus descriptors before execution.

Admission is `received → validated → authorized → resources-reserved → executing`; refusal is terminal and produces no effect. Resource reservation, including result credit, precedes work that could otherwise saturate the parent. Revocation prevents new work immediately and follows an explicit rule for already committed effects. Retries carry operation-specific idempotency semantics; cancellation does not imply rollback of a write or remote request already committed.

### Execution, output and completion

The broker owns child lifetime, captures stdout and stderr independently, and emits a typed result union. Child bytes never enter the control parser. A child may write fake JSON-RPC, ANSI/OSC, invalid UTF-8 or misleading tool IDs; all remain data. Rendering uses a safe derived display view, while authorized raw-byte retrieval remains distinct. Do not silently modify tool arguments or results to make them look safe.

Use byte sequences and monotonic ordering per stream, plus explicit end/gap/truncation records. Shared inherited stdout yields domain/channel provenance only: do not infer an exact writer PID. For exact tool attribution, the broker must create that tool's dedicated capture pipes or own its protocol request. A subprocess can intentionally redirect output to an allowed file/socket instead; its effect is covered by object/network policy and only semantically attributed when that route is brokered.

Results include text/bytes, exit status, structured JSON, images/audio, files/resources, and multiple streams/attachments. Large objects use broker-owned, quota-charged artifact handles with authorization checked on each access. Hashes aid integrity/deduplication, not permission. Treat MIME types, filenames, URI strings and annotations as untrusted. Decompression, image parsing and preview rendering need their own checked budgets and isolation. An error result must preserve already committed outputs and explicitly distinguish cancellation, refusal, worker loss and partial delivery.

`finished`, `failed`, `canceled`, `lost` and `refused` are distinct terminal outcomes. Complete each admitted call at most once. Reject stale/duplicate completions, cross-domain cancellations and results for retired executions. On owner death, revoke its handles and stop owned work according to the launch contract; externally committed remote work may require a typed uncertain outcome. The Codex independent-stream cancellation case above is a concrete regression target, not a reason to assume network ordering.

### Delegation at any depth

Broker-created delegation mints a child capability no broader than its parent's, charges the same subtree budgets, and registers ownership before child work starts. Closing the parent's owned scope cancels descendants according to explicit lifetime semantics. Walking arbitrary depth uses checked iterative data structures, not recursive trust or an arbitrary security depth limit. A parent cannot enlarge its subtree allowance by minting many siblings or spawning a second harness.

A harness may create logical subagents entirely in process and lie about their names/depth. Attribute their API events as adapter-reported; the domain's OS/object constraints still apply to the aggregate. Stronger per-subagent grants require broker-created principals/contexts and separate objects, not just a `parent_tool_id` field. Daemonization, double-fork, exec, new threads, detached sessions and closed stdout must not release workload membership.

### Buffering and saturation

Buffering means each controlled connection/stream is admitted, charged and relayed through owned queues. It does not require collecting an infinite response before release. Use credit-based bounded chunks, backpressure, explicit half-close/cancel behavior and fair scheduling across control, outputs and bulk artifacts. Derive limits from protocol representations, negotiated stream credit, domain memory/storage budgets and measured queue/service behavior. Do not introduce fixed magic chunk sizes, queues or timeouts.

Control must remain available when a consumer stops reading, a child writes endlessly, or a result declares impossible lengths. Never allocate from an untrusted length before checking representation overflow and remaining credit. Exhaustion must cause backpressure or a typed refusal/loss outcome; it must not silently discard required evidence or block lifecycle control forever. A policy requiring retained evidence must stop admitting effects when retention cannot be secured. This is stronger than best-effort telemetry and must be tested independently.

## Credentials: access is delegation

Official Codex documentation says file/keyring credentials are cached, offers an ephemeral mode, and treats `auth.json` as password-equivalent. Copying/mounting a readable auth cache into a hostile domain grants that domain the tokens; mode 0600, a read-only mount, encrypted transport and output masking do not make a token unreadable by the process authorized to read it. No credential contents were inspected for this research. [official authentication and storage](https://learn.chatgpt.com/docs/auth)

Three designs have different guarantees:

| Design | What it can promise | Remaining boundary |
| --- | --- | --- |
| Explicit raw credential delegation to isolated harness | The user's selected sign-in behavior can remain native | The hostile domain has the secret and can use allowed egress to disclose/use it; lifetime/refresh access must be explicit |
| Trusted native harness/control process keeps credentials; untrusted tools use broker-owned remote execution | Tool children need not receive auth cache/tokens | The credential-holding harness becomes part of the trusted computing base; an arbitrary hostile harness cannot be put there under the same claim |
| Opaque inference capability to a trusted broker | Child can ask for scoped inference without obtaining the upstream token | Requires a documented compatible provider/client contract, grants, metering, refresh/revocation and correct terms; it is not a transparent replacement for every signed-in CLI |

Codex experimental external-token auth hands an access token to app-server and delegates refresh responsibility to the host. It can move refresh ownership but does not hide that supplied token from an untrusted app-server. A credential helper that returns a bearer token to a hostile process likewise releases it. Official custom providers support explicit gateway/helper configurations; changing endpoints, authentication method, account scope or trust roots must be a visible supported integration, not an automatic rewrite. [app-server authentication modes](https://learn.chatgpt.com/docs/app-server), [custom-provider authentication](https://learn.chatgpt.com/docs/auth#custom-model-providers)

Do not TLS-intercept OpenAI/Anthropic traffic to manufacture semantic visibility, disable certificate verification, or weaken cryptography. An opaque relay can buffer ciphertext and enforce connection policy; it cannot inspect protected request content. Native inference brokering terminates a deliberately chosen API at its own authenticated endpoint, with the user knowing the changed trust boundary. Provider/account compatibility and auth-refresh behavior remain acceptance work; no source here establishes an off-the-shelf secretless ChatGPT-sign-in path for an arbitrary hostile Codex binary.

## Telemetry and export

OpenTelemetry's current GenAI execute-tool conventions are **Development** and have moved into a separate repository. The inspected revision `e6658f14c9d9ea0760825f029cb82e7673f48fab` describes `execute_tool` spans, optional sensitive argument/result capture, provider-supplied call IDs, and specialized agent-transfer spans only when the transfer actually is a tool call. It does not confer authority or prove completeness. Pin the exporter mapping/schema version; retain Shards' authoritative internal identities separately. [pinned execute-tool conventions](https://github.com/open-telemetry/semantic-conventions-genai/blob/e6658f14c9d9ea0760825f029cb82e7673f48fab/docs/gen-ai/execute-tool.md), [repository/version context](https://github.com/open-telemetry/semantic-conventions-genai/blob/e6658f14c9d9ea0760825f029cb82e7673f48fab/README.md)

Proposed export policy: low-cardinality outcome/tool classes by default; content capture opt-in and authorized separately from effect permission; no token/environment/attachment bytes in ordinary spans. Keep harness-reported trace context as an untrusted link, not the root of a broker-owned authorization tree. Bound attribute count/size, exporter queues and unique labels. Report telemetry gaps and exporter failure without changing enforcement. Internal provenance is essential even if an OTel backend cannot represent all result types.

## Bypass and coverage matrix

All rows below are design acceptance requirements, not passed tests.

| Attempt / surface | Required native control | Semantic coverage / limit |
| --- | --- | --- |
| Supplied binary disables hooks or starts a second harness | Domain confinement plus closed broker grants | OS/object effects stay constrained; unreported in-process tool names are unknown |
| Codex dangerous flag skips approvals | Shards executes allowed native tool/executor requests; denies alternate effects | Conditional approval notifications alone are insufficient |
| Claude pre-tool callback crashes/blocks forever | Tested failure policy plus broker admission and lifecycle control | Hook failure must not grant OS/object authority |
| Tool prints forged results, IDs, approvals or terminal controls | Separate data/control descriptors and broker-issued identity | Bytes are data; no state transition from output parsing |
| Child uses inherited auth/key/control FD | Exact descriptor inheritance allowlist; explicit credential delegation | Domain client key proves domain only; upstream secret exposure cannot be rebranded opaque |
| Tool writes to files/shared memory/Unix sockets instead of stdout | Concrete object grants and brokered access where semantic policy requires it | No promise every allowed write is a harness-labelled tool result |
| Local MCP process makes hidden network calls | Tool process gets only its own constrained objects/egress | MCP schema/annotation is not isolation of the implementation |
| TLS/QUIC/Noise tunnel hides payload | Default-deny routes; owned buffered relay; explicit endpoint policy | Ciphertext can be transported, not universally semantically inspected |
| Malicious resource URL or redirect targets host/internal services | New checked fetch capability, destination/redirect validation | Links and original hostnames never grant the redirected destination |
| Nested subagents/double fork/exec/close pipes | Domain/subtree ownership survives process mechanics | Logical agent attribution requires adapter evidence; confinement does not |
| Huge/fragmented/deep JSON, base64, image or archive | Checked framing/credits, isolated decoding, schema validation | Typed protocol validation is required before expensive work |
| Slow consumers, output flood, full artifact disk | Backpressure and fair control capacity; typed exhaustion | No unbounded parent retention or silent loss |
| Duplicate completion/cancel-before-call/reconnect replay | Owned call-state machine, generation and principal checks | Never replay non-idempotent effects on ambiguous transport failure |

## Validation, mutation checks and measurements still required

1. Build protocol adapters against the pinned installed versions and generated schemas; exercise real Codex/Claude local operations without provider calls first. Assert each supported built-in/external tool route and any local fallback. An unknown method/version/shape must produce an explicit unsupported result under strict mediation.
2. Run hostile arbitrary binaries that bypass every harness hook, recursively spawn, forge environment IDs, redirect output, close descriptors and issue direct file/IPC/network operations. They must remain within aggregate grants on every host backend. This verifies independence from cooperation, not semantic detection of every function.
3. Mutation-check admission before effect: remove one grant/ownership check, enable an alternate socket/file path, move resource reservation after launch, allow inherited control FD, trust payload principal, or parse control from stdout. Each mutation must cause its corresponding test to fail by observing a real forbidden effect or ownership violation.
4. Test rich results byte-for-byte, including images/audio/blob resources/attachments, invalid encodings, incomplete chunks, mismatched schemas, extreme nesting and malicious metadata. Test display sanitization separately from raw capture and authorization; prove no false tool completion is accepted.
5. Test cancellation before request, cell-close before call, duplicate/stale completion, reconnect, broker/worker death and committed remote effects. Verify clean handle/FD/process/artifact ownership after each path; use mock endpoints to count actual side effects.
6. Saturate every queue while lifecycle cancellation runs; stop consumers, fill the evidence store, flood telemetry labels and exhaust resource credits. Assert bounded memory/FD/storage use, fair control progress, explicit evidence gaps and fail-closed policy where required. Fuzz protocol parsers/state machines with the same ownership invariants.
7. Benchmark native empty tool calls, process launch, filesystem read/write, concurrent calls, separate stdout/stderr, large/rich results and slow receivers against the same isolated workload without the semantic adapter. Record CPU/allocation/copy counts, resident/retained memory, FD/handle count, queue residence, bytes per second, latency distribution and cancellation latency. Vary concurrency/payload size and offered load to find saturation; report host contention and uncertainty rather than embedding a measured constant in policy.
8. Run real guest E2E on KVM/HVF/WHP, both architectures, with all eight host build targets. Preserve the single-binary packaging rule; do not require a separately installed SDK/runtime sensor for the confinement guarantee. Native integration changes and kernel/object confinement changes need separate regression evidence.

Open questions before implementation can claim coverage: the exhaustive Codex exec-server/built-in fallback inventory; remote Code Mode authentication/policy mapping; a pinned Claude binary's precise hook/MCP/native transport behavior; exact per-process versus per-domain output provenance; rich-result framing/retention compatibility; the supported signed-in credential contract; and benchmarked overhead on each backend. These are evidence gaps, not grounds to assume compatibility or claim the goal met.
