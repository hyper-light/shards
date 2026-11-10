# D-HA3: exclusive native stdin and typed turn control

NEW work, source base0342 with current policy origin/main
ca3ab10c151aa5539e757f1cc76029746ccbb561/rulesdf2. Full current policy,
evidence/CLAUDE/managed plan and490-line coordinator handoff read; implementer
POLICY_READ ROOT7806. Main's newer cap-observation evidence does not grant native
input rights. Owned source here is `crates/server/src/harness/control.rs` and its
module/test tokens; input owns the actual target gate and existing queue.

## Pinned methods and exact source identities

Codex0.167.0, source086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a:

- `turn/start`: threadId plus input containing one literal text item; explicit
  empty text_elements means no UI spans. This adapter accepts no model, workspace,
  permission, sandbox, environment, config or arbitrary JSON override object.
- `turn/interrupt`: threadId plus turnId. An empty response acknowledges the
  native request; final turn notification is separate from that acknowledgement
  and from every canonical broker receipt.

The current [official OpenAI documentation](https://learn.chatgpt.com/docs/app-server)
was searched and fetched using OpenAI Docs before inspecting the new protocol
facts. The [immutable parameter schema](https://github.com/openai/codex/blob/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a/codex-rs/app-server-protocol/schema/json/v2/TurnInterruptParams.json)
and response schema exactly match the original installed export.

The FULL TurnStart schema does **not** match. Tagged source is24058B/SHA256
4e25cd48986f73e600d9949a055ddb994a75b4a38790e935274646f0fece00ad;
installed experimental export27411B/SHA256
d13cb408b906669e8d3e8694b42148a75180d4dcd0eeac6867271add8dfa1864.
`raw/source-fetch-09.json` compares complete parsed schemas: exactly nine optional
properties are present only in the experimental export; required threadId/input
and the full UserInput definitions are equal. Both exact schemas are retained
separately. No version guessing, full-schema parity, new CLI export or hidden
override/default substitution is claimed. These additional properties are not
silently accepted by this narrow typed interface.

Claude native2.1.321/SDK0.2.180, source3cf3446b0fa6c97042e85b245bba07459f22f946:

- [Public client query](https://github.com/anthropics/claude-agent-sdk-python/blob/3cf3446b0fa6c97042e85b245bba07459f22f946/src/claude_agent_sdk/client.py)
  writes type=user, message.role=user, literal message.content, caller session_id
  and parent_tool_use_id=null. This adapter takes session explicitly; it does not
  substitute the SDK's default session string or configure provider/settings.
- The same public client's interrupt delegates to the pinned internal Query,
  which writes a control_request with caller request_id and subtype=interrupt.
  It is a protocol request, not a PID signal or control character. Current
  [Python SDK reference](https://code.claude.com/docs/en/agent-sdk/python) and
  [streaming-input documentation](https://code.claude.com/docs/en/agent-sdk/streaming-vs-single-mode)
  were actually fetched. They identify the streaming query/interrupt contract.
  Already-produced responses remain separate data; no rollback is inferred.

Only a plain text turn and protocol interrupt are encoded. Native actor stdin
EOF, resize and arbitrary raw bytes return distinct typed Unsupported values.
This says nothing about a different native command/exec child's PTY or file API.
Physical transport close remains the existing trusted owner's lifecycle work;
it is not represented as a normal native EOF method. No empty chunk=EOF, Ctrl-C,
Ctrl-D, guessed byte sequence, actor PID lookup or reaper-as-actor substitution.

## Finite codec API

```rust
encode_into(protocol: Protocol, command: &Command<'_>, storage: &mut [u8])
    -> Result<usize, Failure>
encoded_len(protocol: Protocol, command: &Command<'_>) -> Result<usize, Failure>
Command::operation(&self) -> Operation
```

Command variants are CodexTurn{id,thread,text}, CodexInterrupt{id,thread,turn},
ClaudeTurn{session,text}, ClaudeInterrupt{id}, Eof, Resize and RawBytes. No
command/payload/ID Debug implementation or ordinary logging is added. Protocol
selection and signed64/string Codex correlation are checked first. A Count
pass derives the exact COMPLETE JSONL size, including LF; insufficient storage
fails before touching it. Filling borrows only the selected initialized slice,
then the existing owner calls stage(length). Escaped multiline/control-looking
prompt text stays one JSON string and one outer packet. No queue, buffer growth,
native descriptor, thread, request-ID allocator, wait, cache, scope or grant
constructor is introduced. Result length is not a reservation/charge by itself.

## Existing-owner binding and caller responsibility

ROOT specifically forbids a generic raw Pipe/Tty SESSION sharing the actor's
native protocol stdin. The proposed concrete input-owned runtime binding is
Target::{Raw,Native(NativeAdapter)}, derived only from the SAME protected
MemberRequest.adapter. Scope53, Request15 and root manifests remain unchanged.
Native+Tty refuses before attach. Native scope is the original MemberActor;
no second CLI/Worker launch, manager, queue or alternate input region.

The input lane owns Owner attach/opening/enqueue/vacant/control and raw Session
resolution. Every generic raw entry must refuse a Native target before touching
its endpoint/storage, including a generic EOF/cancel/revoke that would otherwise
close protocol stdin. Actual trusted lifecycle revoke/owner-ended remains that
same owner. The private native entry checks current Epoch/member/execution,
target and admitted region, then fills/stages the SAME native Input storage.
Turn uses the existing WRITE right; native Interrupt must also check the
separate existing SIGNAL right. This mapping and the exact runtime target API
were sent input/ROOT7807–7808/7842–7843; the owning lane supplies the concrete gate
delta. No target or right is inferred from request IDs, method names or bytes.

This checkpoint tests the codec, NOT that gate or actual protected producer.
The required next light caller witness is an actual disposable input pipe:
raw Session against Native target emits zero bytes; an explicitly target-checked
typed command fills/stages the existing region and preserves native results.
No provider/login/user turn/VM/heavy campaign is needed or authorized here.
Canonical peer dispatcher/supervisor startup remain audit-owned; actual epoch
admission and resource grant producers remain unimplemented/unverified as stated
in PENDING-CALLER.md. A source-level target enum cannot manufacture those rights.

## Checks, retained failures and R01–R16

`raw/light-native-control-01`:17tests passed/0failed/0ignored/1unrelated JSON
test filtered, scoped production/test Clippy0, named format0 and policy0. It
checks both literal turn/interrupt encodings, escaped injection-looking input,
only required fields, unchanged untouched suffix and storage identity, typed
protocol/correlation/capacity/unsupported refusal before writes, plus all prior
capture/request/result cases. Exact source+Cargo.lock hashes/commands/raw are
retained. Mac ARM64/Rust1.98.1, normal2CPU/2GiB/4GiB/600s/serial compiler,
lockf/tmp/g121-mac.lock and152227823616B free. Elapsed1.569s is a check receipt,
not a latency/throughput measurement. No native CLI or provider was invoked.

Research failure `source-fetch-08` returned1 on full TurnStart equality after
HTTP200 and correct immutable blob identity. Its first mismatching bytes were
NOT saved before assertion; original metadata/hash and tool output are retained.
A separately labelled immutable re-read09 has the identical original hash and
saves both full inputs before comparison. No original-byte-preservation claim
is made for that first fetch. Other fetch08 files succeeded. One documentation
search found unrelated Managed Agents pages and another returned no matches;
neither was used as SDK evidence. Exact official SDK pages were then opened.
A patch failed on module-order context and applied nothing; the corrected patch
and subsequent source checks passed. Those tool-only attempts have no invented
separate raw originals. Frozen earlier receipts/manifests remain untouched.

R01 checked arithmetic/slices, typed errors and fixed output; existing parser
and full owner allocation proof still open. R02 actual installed schema delta,
pinned official sources and established native SDK/app-server alternatives;
generic raw forwarding conflicts with the protocol's one writer. R03 literals
are pinned method/field/JSON syntax or exact derived packet length, not picked
resource quantities/defaults. R04 explicit request/refusal vs reported completion;
owner state/target/epoch checks belong to the real owner. R05 no second manager/
queue/cache/poll. R06–7 focused codec and ownership/count comments. R08/R14 no
performance, allocation/fault or4–5x claim. R09 NEW contract, no old-main bug claim.
R10 original failed/unknown results retained; actual gate effect proof still owed.
R11 Mac component only; GNU/musl/Windows/matrix and real VM unrun. R12 shared
protocol code, no platform duplication/binary/root/crypto change. R13 supplied
literal argv/manifests stay unchanged; full fallback closure remains required.
R15 light scope only, no children/main merge/heavy test; policy0 is not compliance
proof. R16 no cleanup or other-lane/ROOT target access; no owned check remains active.
