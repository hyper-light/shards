# Native effect and fallback ledger

NEW requirements inventory. Source observations below are not live bypass
reproductions or completed closure. Policy/role/source identity is in README.md.
ROOT owns the actual strict route, disabled-hook, dangerous-flag, credential,
signed-in, restore, external-effect and mutation campaigns. An unsupported codec
route cannot prevent a hostile binary from making an unrelated syscall: that
requires the canonical object/process/network owners to close alternate rights.

## Pinned Codex0.167.0

All source links in this section use
[`086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a`](https://github.com/openai/codex/tree/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a).
Exact fetched hashes, original missing-path404s and corrected Git-tree identity
are in `raw/source-fetch-*`. These are literal source/schema observations, not
claims that every model/config exposes every tool.

| Surface | Required operation / current disposition |
| --- | --- |
| App-server `item/tool/call`, namespace `shards` | send/answer/hand decode to the existing broker tool and literal borrowed arguments. Broker authorization/resources/effects remain canonical; caller registration pending ROOT. |
| Dynamic tools in another namespace, unknown tools | Explicit typed unsupported namespace/external-tool outcome. Descriptions, annotations and reported IDs cannot create authority. |
| Command/file approval requests | Explicit unsupported approval-as-execution outcome. They do not redirect execution, even when cooperation would pause it. |
| `item/started`, output/completed/collaboration notifications | Observation only. Not accepted as effect requests or authority. |
| Exec `initialize`, `initialized`, environment metadata/status | Separate native executor connection protocol, not dynamic tools. No fabricated handshake/capability advertisement in this codec. |
| `process/start` | Actual native owner must resolve executable/workdir objects and literal argv/env. Metadata is correlation only. TTY, pipeStdin, arg0, envPolicy, shellSnapshot, sandbox, managedNetwork and networkProxy need explicit support/refusal. Currently typed unsupported native execution. |
| `process/read` | Owned process plus afterSeq/maxBytes/waitMs and exact stdout/stderr/pty byte chunks/terminal state. Currently unsupported; no second output manager. |
| `process/write`, `process/signal`, `process/terminate` | Ownership-scoped input/signal/lifetime. `writeId` is required by source despite the README example's omission. Only Interrupt is in ProcessSignal. No invented EOF/resize RPC or empty-chunk convention. Currently unsupported; input/native owners retain implementation. |
| `process/output`, `process/exited`, `process/closed` | Native executor observations, never new requests/grants. Keep channel bytes separate and exact. |
| `fs/readFile`, `fs/open`, `fs/readBlock`, `fs/writeBlock`, `fs/close`, `fs/writeFile` | Held-object filesystem authority and handles; path URIs and base64 do not confer rights. Read/replace mode, followSymlinks, offsets/lengths and sandbox fields require checked literal semantics. Currently unsupported. |
| `fs/createDirectory`, `fs/getMetadata`, `fs/watchMissing`, `fs/canonicalize`, `fs/readDirectory`, `fs/walk`, `fs/remove`, `fs/copy` | Native filesystem owner must cover each independently; no fallback to actor-controlled local paths. Currently unsupported. |
| `capabilityRoots/discoverV1`, `capabilities/discoverV2`, `environmentConfig/read` | Filesystem/config discovery itself accesses objects. Manifest contents/roots are not grants. Currently unsupported. |
| `http/request` and streamed bodyDelta | Checked network/credential/destination/redirect/result ownership; no automatic direct HTTP client or provider change. Currently unsupported. |
| Noise/remote forwarding | An opaque authenticated transport does not make this codec an executor or give it semantic visibility through encryption. No Noise/TLS interception. |
| Code Mode execute/wait and nested tool calls | A real cell-host seam delegates tools back to the owner. It must call the same broker for each supported effect; no local function fallback or claim that cell hosting replaces exec-server. Unimplemented here. |
| MCP tools/resources/discovery (local stdio and remote) | Registry-resolved concrete endpoint/process plus the same checked capabilities and rich-result leases. Existing Shards `mcp` discovery is not a universal executor. Unknown dynamic tool routes refuse. |
| Shell exec / write_stdin / one-shot exec | UnifiedExec may be disabled while a one-shot command remains registered. Thus disabling a resumable tool is not process-effect closure. Native execution owner + kernel rights remain required. |
| apply_patch and view_image | Native filesystem paths/objects require mediation even when no shell is used. Adapter label matching is insufficient. |
| Collaboration, plan, request permissions/input, context management, clock/sleep | In-process logical state/reporting is not a new principal. Any delegated process/file/network effect still needs the corresponding owner. These tools are unsupported by this peer-effect codec. |
| Hosted web search / extension tools, plugins, browser/computer integrations | Provider-hosted and extension effects have distinct owners. An allowed encrypted inference flow cannot be described as semantically intercepted. No universal support claimed. |
| Startup/config, skills/plugins, hook commands, shell snapshots, auth helpers | These can access files or spawn processes outside model-tool callbacks. Required direct routes remain subject to independent confinement and checked launch grants. No shell wrapper or automatic helper execution introduced. |

The pinned
[executor protocol](https://github.com/openai/codex/blob/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a/codex-rs/exec-server-protocol/src/protocol.rs),
[capability discovery](https://github.com/openai/codex/blob/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a/codex-rs/exec-server-protocol/src/capabilities.rs),
[tool registry construction](https://github.com/openai/codex/blob/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a/codex-rs/core/src/tools/spec_plan.rs),
[environment provider](https://github.com/openai/codex/blob/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a/codex-rs/exec-server/src/environment_provider.rs)
and [handlers](https://github.com/openai/codex/tree/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a/codex-rs/core/src/tools/handlers)
establish this inventory. The default environment provider selects a local
environment when the executor URL is missing/empty and no disabled/remote
selection exists; a configured remote entry omits the local entry. This is a
source observation, not a tested universal remote-failure policy. Unavailable
remote transport must not be replaced with local execution by Shards.

## Pinned Claude Code2.1.321 / Python SDK0.2.180

The official SDK main API read resolved to
[`3cf3446b0fa6c97042e85b245bba07459f22f946`](https://github.com/anthropics/claude-agent-sdk-python/tree/3cf3446b0fa6c97042e85b245bba07459f22f946).
Its pyproject reports0.2.180. Raw source pins and native binary/version/empty
account initialization receipts are retained. Claude native implementation is
closed source; documented tool inventory does not prove native branches live.

| Surface | Required operation / current disposition |
| --- | --- |
| SDK `mcp_message` for server `shards`, inner tools/call | send/answer/hand decode to the same broker. Outer control success describes message delivery; inner isError comes from canonical completion, not output text. Caller registration pending. |
| `can_use_tool`, `hook_callback` | Typed unsupported approvals/hooks as execution. Earlier approval modes/rules may skip a callback; disabling it cannot alter Shards authority. |
| Initialize/ping/tools/list/initialized inside SDK MCP | Must use canonical MCP negotiation/catalog handling before a full registered native SDK server is advertised. This first codec accepts only explicit tool calls; no second session manager. Local CLI control initialization alone is verified. |
| MCP stdio/HTTP/SSE/SDK, reconnect/toggle/resource/tool-search | Concrete registered endpoint/process/object grants; no URL/name/annotation or reconnection may select a new principal. Unimplemented external routes refuse. |
| Native command/file/network builtins | Require owned executors and denied alternate OS routes. Hook approvals never execute them here. |
| Logical agents, tasks, teams, skills, workflows, scheduling | Reported graph/state stays distinct from actual process/lifetime rights. No detached ownership or new scope from a harness label. |
| Hosted effects and remote control | Separate network/account contracts. Existing provider rights cannot be silently rewritten or claimed as full semantic brokerage. |
| File insertion, compaction/session persistence, checkpoints/rewind, memory/skill/plugin loading, hooks, settings and credential helpers | Direct local/config effects need independent object/process/input/credential policy, including paths outside model callbacks. No opaque-token claim. |

The current official [tool reference](https://code.claude.com/docs/en/tools-reference),
read2026-11-10, identifies these factual tool identifiers, grouped for coverage:

- Process/filesystem: Bash, PowerShell, Monitor, Read, Write, Edit, NotebookEdit,
  Glob, Grep, LSP, EnterWorktree, ExitWorktree.
- Delegation/control: Agent, ListAgents, SendMessage, SubagentHandback, Workflow,
  TaskCreate, TaskGet, TaskList, TaskUpdate, TaskStop, TodoWrite; TaskOutput is
  documented removed. AskUserQuestion, EnterPlanMode, ExitPlanMode,
  EndConversation, ReportFindings, Skill.
- Scheduling: CronCreate, CronDelete, CronList, ScheduleWakeup, RemoteTrigger.
- Network/resources: WebFetch, WebSearch, ListMcpResourcesTool,
  ReadMcpResourceTool, ToolSearch, WaitForMcpServers, Artifact, PushNotification,
  SendUserFile, ShareOnboardingGuide, SendFeedback.

This factual inventory is not an allowlist. All routes beyond the three checked
peer operations remain unimplemented/unverified in this adapter. Optional tool
availability varies by native configuration/provider and must be negotiated.
Unknown future names/methods/versions always refuse rather than select local
execution. No fixed list of tool names can establish arbitrary native-binary
syscall confinement.

## Ownership and mandatory remaining proof

Native-effect lane owns held read/write and the actual actor-view process
producer; credentials/input lane owns grants, refresh state and input/TTY;
epoch lane owns admission/cut/generation; route lane owns alternate routing;
audit lane owns canonical broker/audit and response reservation; ROOT approves
shared source spans and integrates actual producers. This lane owns the native
translation and compatibility fixtures only. Root manifests stay entrypoint plus
optional literal options; supplied path/tar AGENT/HARNESS remains the only harness
installation route. NETWORK/EXPOSE and existing declarations retain their rights.

Open tests: actual broker peer effect/cross-principal refusal through each native
envelope; actual CLI SDK MCP negotiation; all native builtin/external/optional
field semantics; disabled hooks and dangerous flags with a real forbidden-effect
observer; closed local fallback after remote failure; literal argv/env/path
semantics; raw stdout/stderr; cancellation/result/replay/epoch transition cases;
rich content and leased artifacts; typed input/TTY/credential modes; full real
signed-in supplied HARNESS with no RUN/VOLUME installation. Local initialization
and codec tests do not pass these tests by implication.
