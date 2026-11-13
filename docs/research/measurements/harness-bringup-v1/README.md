# Harness bring-up v1: a real Codex CLI and a real Claude Code as Shards harnesses

The managed-runtime plan's G24 and P10, through the startup contract of
[D54](../../../design/architecture.md) ("Supplied agent and harness startup"): each
publisher's own Linux binary, supplied as a directory whose root `harness.json` holds
`entrypoint` and literal `options`, declared by `HARNESS <name> FROM <path>`, built with
`shards build image` and run with `shards run`. No `RUN` or `VOLUME` installs a harness and
no credential enters an image. Implementer record; the coordinator owns suites, lint gates,
multi-platform validation and review.

## Identity

| | |
|---|---|
| Policy | `origin/main` `bff3015182ad754274f1048ce393ac12cb6cd91f`; `docs/development-rules.md` sha256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`; role implementer ([POLICY_READ.json](POLICY_READ.json)) |
| Source | branch `p/harness-bringup-v1`, from `origin/p/integrate-engine-20261111` at `ef687bc77d1e17848c10a0922c7f965368e5ceb2`; the runs used that base plus this change, whose files' hashes are in [records/sources.txt](records/sources.txt) |
| Host | kiln runner adacachy (`451397fcc585f7f7cb9aaf00e42064bb`): CachyOS, Linux 7.2.9-2-cachyos, x86_64, 32 CPUs, KVM |
| Toolchain | Rust 1.98.1 (`rust-toolchain.toml`), zig 0.16.0 |
| shards | `cargo build --profile test-release -p shards` at the base (3 min 02 s, cold) |
| Guest | shards' pinned kernel `vmlinux-6.18.48-x86_64-17c3ba205400` (sha256 `48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7`), downloaded and checked by shards itself on the user path; the tests boot the same pin (`tests/common` takes `src/kernel.rs`) with the test build's shards-init |
| Base image | `alpine:3.24.4@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf` (Docker Hub's index digest, 2026-11-08) |

## The publishers' binaries

[fetch.sh](fetch.sh) `OUTDIR` fetches and verifies each binary as its publisher documents;
nothing it fetches is committed ([fetch/fetched.json](fetch/fetched.json)).

| | Codex CLI | Claude Code |
|---|---|---|
| Version | 0.167.2 (tag `rust-v0.167.2`, published 2026-11-11T22:44:46Z, the latest) | 2.1.322 (`latest`; `stable` was 2.1.311) |
| Artifact | `codex-x86_64-unknown-linux-musl.tar.gz` (static-pie musl) and `codex-code-mode-host-x86_64-unknown-linux-musl.tar.gz`, from `github.com/openai/codex/releases/download/rust-v0.167.2/` | `linux-x64-musl/claude` from `downloads.claude.ai/claude-code-releases/2.1.322/` (what `claude.ai/install.sh` picks on Alpine) |
| Archive sha256 (the release's digests) | codex `8246e846…5b15f`; host `189475a6…c39a6` | — |
| Binary sha256 | codex `a9672d0fa0dd8fcf8cad136fa0d056b914bc046f5ac3d78f4a5cf390655af56b` (320,248,296 B); host `007c62795a7215c27ee85486ecb95e714b33118a2d18c6d7b09b0d9793c72ff4` (79,213,840 B) | `9bed6cfc5d2420b673a0b9fe41c71533a02904935cbc3b4cd110302d475fee6f` (328,374,856 B) |
| Publisher's check | each binary against its keyless cosign bundle (`.sigstore`): certificate identity `https://github.com/openai/codex/.github/workflows/rust-release.yml@refs/tags/rust-v0.167.2`, issuer `https://token.actions.githubusercontent.com`, verified by cosign v3.1.4 (OpenAI's signing version, `cosign-linux-amd64` sha256 `424e2c97…bbfa` from sigstore's `cosign_checksums.txt`) | GnuPG `VALIDSIG` of `manifest.json.sig` by the documented primary key `31DD DE24 DDFA B679 F42D 7BD2 BAA9 29FF 1A7E CACE`, then the binary's SHA-256 in that manifest ([code.claude.com/docs/en/setup](https://code.claude.com/docs/en/setup), "Binary integrity and code signing") |
| Guard checks | the same bundle refused with the identity of tag `rust-v0.167.1` ("none of the expected identities matched") and against the other binary ("matching bundle to payload": bundle `a9672d0f…`, payload `007c6279…`) | not exercised: fetch.sh refuses a status without `VALIDSIG` by that fingerprint, and a manifest of another `version`, but no bad signature was tried |

Codex starts `codex-code-mode-host` from its own directory, as OpenAI's package archive
carries it: without it every session's first event was
`Code Mode is unavailable because failed to spawn code-mode host
/harness/codex/bin/codex-code-mode-host` (the runs before fetch.sh fetched it; their
records are not kept); with it, none.

## The example build contexts

[docs/examples/harness-codex](../../../examples/harness-codex) and
[docs/examples/harness-claude](../../../examples/harness-claude): an Agentfile, and the
harness directory with its `harness.json`; the binaries go in its `bin` (gitignored).

```dockerfile
FROM alpine:3.24.4@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf
HARNESS codex FROM ./codex
NETWORK --dns --egress=443/tcp provider FOR codex
EXPOSE --harnesses=codex 443/tcp AS egress FOR provider
CONNECT codex WITH codex ON provider
```

```json
{"entrypoint": "codex exec", "options": ["--json", "--skip-git-repo-check", "--ephemeral", "Reply with the single word: ready"]}
```

Claude Code's is the same with `claude`, `{"entrypoint": "claude -p", "options":
["--output-format", "stream-json", "--verbose", "Reply with the single word: ready"]}`,
and `RUN apk add --no-cache libgcc libstdc++`: the two system libraries Anthropic documents
its musl build needing on Alpine. They are the base system's, not the harness; `ripgrep`,
which Anthropic also lists, is used by Claude Code's search tools, not by this run.

Every grant is named (AGENTFILE_ARCH.md §4.6, §4.7, §9.7): a network for this harness
alone, joined with no flow to any member (`CONNECT codex WITH codex`); its names past the
microVM through the microVM's resolver (`--dns`); TCP 443 out, open at both the network's
boundary (`--egress`) and the microVM's (`EXPOSE … AS egress`). Nothing comes in; no other
port or protocol goes out. What the grants cannot say is finding H2.

## Runs

User path ([bringup.sh](bringup.sh), [user-path/](user-path)): the integration build's
`shards`, its own pinned kernel and carried init, a fresh `SHARDS_HOME`, an empty
`DOCKER_CONFIG`, `SHARDS_LOCAL_STORE=none`:

    bringup.sh target/test-release/shards docs/examples/harness-codex OUTDIR/codex WORKDIR

The harness's output is private to the run's owner (an authenticated QUIC endpoint per
channel, `shards read ports`: `managed-0-1/udp … authenticated owner output`); no command of
the CLI reads it (finding H3). The real-VM test
[crates/shards/tests/harness_bringup.rs](../../../../crates/shards/tests/harness_bringup.rs)
is that owner: it builds each case from the examples, runs it detached, subscribes to the
harness's stdout and stderr through the test fixture's observer (`common/managed_capture.rs`),
reads each to its end exactly as delivered, and reads `shards read processes`, the kernel's
record (process events connector, D123). Its tests are ignored: they need the binaries, and
those that reach a provider need the network.

    SHARDS_HARNESS_CODEX=OUTDIR/codex SHARDS_HARNESS_CLAUDE=OUTDIR/claude \
    SHARDS_HARNESS_RECORDS=/abs/new/dir \
    cargo test --profile test-release -p shards --test harness_bringup -- --ignored --test-threads=1

Final run: [records/](records) ([test-output.log](records/test-output.log), each case's
directory: Agentfile, harness.json, build, run, raw `stdout.bin` and `stderr.bin`,
`ends.txt`, the process record, ports, transport outcomes).

| Test (record) | Result | Owner's stdout read for | What it shows |
|---|---|---|---|
| `a_real_codex_starts_as_a_harness_and_reaches_its_provider` (`codex`) | pass | 15.2 s, End | the example as published: Codex exec'd as declared, OpenAI's 401 |
| `a_real_codex_without_its_grants_reaches_no_provider` (`codex-airgapped`) | pass | 70.4 s, until it said `waiting for network` | the control: the same image less its three network directives reaches no provider |
| `a_runs_environment_reaches_no_harness` (`codex-run-env`) | pass | 14.7 s, End | `run -e` keys never reach the harness |
| `a_real_codex_takes_its_login_from_a_volume_given_at_run_time` (`codex-login-volume`) | pass | 16.3 s, End | a run-time login cache reaches the harness and the provider |
| `a_real_claude_code_starts_as_a_harness_and_refuses_without_a_login` (`claude`) | pass | 0.78 s, End | the example as published: Claude Code exec'd as declared, its local refusal |
| `a_real_claude_code_takes_a_key_from_a_volume_given_at_run_time` (`claude-key-volume`) | pass | 247.2 s, End | a run-time key reaches the harness and Anthropic's 401 |
| `a_volume_given_at_run_time_is_the_harness_own` (`probe-volume`) | pass | 0.01 s, End | how a run-time host directory appears to the harness |
| `an_owners_subscription_outlives_a_silence_within_its_idle_bound` (`probe-within`) | pass | 75.0 s, End | H1's twin |
| `an_owners_subscription_outlives_a_quiet_harness` (`probe-quiet`) | **fails, red by design** | 102.9 s, `transport Lost(None)` | finding H1 |

Every passing case read both channels to their ends with no transport error; every
stdout line of Codex and Claude Code parsed as JSON as written.

### Codex

**Startup.** The kernel's record ([user-path/codex/processes.log](user-path/codex/processes.log)):

    harness codex   914  exec   shards-server "harness codex" /proc/self/fd/6 200000 3 4 "" [] 7
    harness codex   919  exec   shards-server sandbox
    harness codex   923  exec   shards-init managed-reaper
    harness codex   925  exec   /harness/codex/bin/codex exec --json --skip-git-repo-check --ephemeral "Reply with the single word: ready"
    …
    harness codex   925  exit   code 1

The harness's in-VM server and its sandbox, init's reaper for the domain, then Codex
itself: the bare `codex` found in the harness's own `bin`, its options as literal
arguments, no shell between (D54). Its children follow (`getconf LONG_BIT`, its
shell-environment snapshot through Alpine's `/bin/sh`, `env -0`; three that exit 127 with
no exec, programs it probes for that Alpine lacks). It exits 1 after 14.4 s; its server
and sandbox are then ended (`signal 9`). Build 11.9 s, `run vm -d` 1.2 s with the pinned
kernel's first download ([user-path/codex](user-path/codex)).

**Network reach and the provider's answer.** Codex resolves `api.openai.com` through the
microVM's resolver and opens TLS on 443, first as a WebSocket, then over HTTPS, and every
request is OpenAI's to answer: the first refusal is logged 0.63 s after its exec (exec
03:10:34.602, refusal 03:10:35.232 in the test's run). Its JSONL, as the owner read it
([records/codex/stdout.bin](records/codex/stdout.bin)):

    {"type":"thread.started","thread_id":"01a1cdbc-5807-7fe3-ac34-17efd56e4596"}
    {"type":"turn.started"}
    {"type":"error","message":"Reconnecting... 2/5 (unexpected status 401 Unauthorized: Missing bearer or basic authentication in header, url: wss://api.openai.com/v1/responses, cf-ray: a59258315c13ba10-SEA)"}
    … (WebSocket tries 3/5 to 5/5, then "Falling back from WebSockets to HTTPS transport")
    {"type":"error","message":"Reconnecting... 1/5 (unexpected status 401 Unauthorized: Missing bearer or basic authentication in header, url: https://api.openai.com/v1/responses, cf-ray: a592585aff2b8d41-SEA, request id: req_5afe84e2a808426f945a12548463745f)"}
    … (HTTPS tries 2/5 to 5/5)
    {"type":"error","message":"unexpected status 401 Unauthorized: Missing bearer or basic authentication in header, url: https://api.openai.com/v1/responses, cf-ray: a5925888aec22c7a-SEA, request id: req_143b698388164397a07a776e288f3723"}
    {"type":"turn.failed","error":{"message":"unexpected status 401 Unauthorized: Missing bearer or basic authentication in header, url: https://api.openai.com/v1/responses, cf-ray: a5925888aec22c7a-SEA, request id: req_143b698388164397a07a776e288f3723"}}

Each `cf-ray` is Cloudflare's edge in front of OpenAI (`SEA`), each `request id` OpenAI's
API's. Its stderr ([records/codex/stderr.bin](records/codex/stderr.bin)):

    WARNING: proceeding, even though we could not create PATH aliases: Refusing to create helper binaries under temporary dir "/tmp" (codex_home: AbsolutePathBuf("/tmp/.codex"))
    Reading additional input from stdin...
    2026-11-12T03:10:35.231899Z ERROR codex_api::endpoint::responses_websocket: failed to connect to Responses WebSocket stage=Unknown
    … (one such line for each WebSocket try)

**Control** ([records/codex-airgapped/stdout.bin](records/codex-airgapped/stdout.bin)): the
same image without its NETWORK, EXPOSE and CONNECT lines; the harness has only its
loopback, finds no name, and waits:

    {"type":"error","message":"Reconnecting... 2/5 (stream disconnected before completion: failed to lookup address information: Try again)"}
    … (3/5 to 5/5 alike)
    {"type":"item.completed","item":{"id":"item_0","type":"error","message":"Falling back from WebSockets to HTTPS transport. stream disconnected before completion: failed to lookup address information: Try again"}}
    {"type":"error","message":"Reconnecting... waiting for network (C

No status, request id or edge appears. The record ends mid-line where the test stopped
reading, at the words it waited for (70.4 s); Codex goes on waiting until the run is
removed.

The key from a run-time volume (`codex-login-volume`, credentials below) changes only who
OpenAI says is asking:

    {"type":"turn.failed","error":{"message":"unexpected status 401 Unauthorized: Incorrect API key provided: sk-shard*******************-key. You can find your API key at https://platform.openai.com/account/api-keys., url: https://api.openai.com/v1/responses, cf-ray: a59259393c2cc22a-SEA, request id: req_88b482828b8d4c5591a0909348cb040f, auth error: 401, auth error code: invalid_api_key"}}

and the same keys given by `run -e` change nothing (`codex-run-env`):

    {"type":"turn.failed","error":{"message":"unexpected status 401 Unauthorized: Missing bearer or basic authentication in header, url: https://api.openai.com/v1/responses, cf-ray: a5925be2bc5bc078-SEA, request id: req_c9d301cd3b9448799d859b687b3592fe"}}

### Claude Code

**Startup.** The kernel's record ([user-path/claude/processes.log](user-path/claude/processes.log)):

    harness claude   923  exec   /harness/claude/bin/claude -p --output-format stream-json --verbose "Reply with the single word: ready"
    harness claude  1731  exec   rg --no-config --files --hidden /harness/claude
    harness claude  1746  exec   sh -c "uname -s\nuname -r\nuname -m\n…"
    …
    harness claude   923  exit   code 1

It exits 1 after 1.27 s. Build 12.8 s (the `apk add` step 1.4 s), `run vm -d` 0.7 s.

**Without a credential it makes no request.** Its stream-json
([records/claude/stdout.bin](records/claude/stdout.bin)) opens with its `system` `init`
(`"apiKeySource":"none"`, `"claude_code_version":"2.1.322"`, `"cwd":"/harness/claude"`, its
state under `/tmp/.claude`), then:

    {"type":"assistant","message":{…,"model":"<synthetic>",…,"content":[{"type":"text","text":"Not logged in · Please run /login"}],…},…,"error":"authentication_failed","is_api_error_message":true}
    {"duration_api_ms":0,…,"terminal_reason":"api_error",…,"is_error":true,"num_turns":1,"subtype":"success","api_error_status":null,"result":"Not logged in · Please run /login","type":"result","duration_ms":130,…}

A `<synthetic>` message, no API time and no status: Claude Code's own refusal, decided
before any network use. So this run proves startup and output, not reach.

**Reach, with a key given at run time** ([records/claude-key-volume](records/claude-key-volume)):
the placeholder key from a run-time volume (credentials, below). `"apiKeySource":
"apiKeyHelper"`, then every try answered 401 by Anthropic's API:

    {"type":"system","subtype":"api_retry","attempt":3,"max_retries":10,"retry_delay_ms":2011,"error_status":401,"error":"authentication_failed",…}
    … (attempts 4 to 10, delays rising to about 35 s, each "error_status":401)
    {"type":"assistant","message":{…,"model":"<synthetic>",…,"content":[{"type":"text","text":"Invalid API key · Fix external API key"}],…},…,"error":"authentication_failed","is_api_error_message":true}
    {…,"api_error_status":401,"result":"Invalid API key · Fix external API key","type":"result","duration_ms":246515,…}

Anthropic's answer is the status; Claude Code's stream-json carries no request id, and its
`duration_api_ms` stays 0 for failed calls. The probe below shows the harness reads the key
file, so the 401s answer that key.

## Credentials: how one reaches a harness at run time today (step 4)

No credential of the person was read, copied or used: every key below is the placeholder
`sk-shards-placeholder-not-a-key`, given at run time. G18's labels: **native sign-in
delegation** gives the harness the secret (it, and every process it starts, can read it);
an **opaque broker** holds it where the harness cannot and makes the provider's requests
for it.

| Way | Today | Verified | G18 label |
|---|---|---|---|
| A run's environment (`shards run -e`) | Reaches the run's command only. A harness's environment is `PATH` (its directory and `bin` first), `HOME=/tmp`, `SHARDS_DOMAIN`, and its image's OSI `run.env` (`crates/init/src/domains.rs:490-498`) | `a_runs_environment_reaches_no_harness`: `-e CODEX_API_KEY=… -e OPENAI_API_KEY=…` and Codex still sends no key (`401 … Missing bearer or basic authentication in header`) | would be delegation; not available |
| A host directory given at run time to a volume declared for the harness alone (`VOLUME --target-kind=harness DIR FOR <harness>` in the Agentfile, `shards run -v HOSTDIR:DIR[:ro]`), the harness pointed at it by a path in its image (a `--settings` option, or `run.env` in harness.json's full OSI form) | **Works.** The share is the harness's alone (D109: no other member mounts it; the run's own root does not), its host-made files shown owned by the harness's uid, the host's modes kept (`domains.rs:1472-1489`) | Codex: `CODEX_HOME=/run/codex-home` (full-form `run.env`) over `-v HOSTDIR:/run/codex-home`, its own `auth.json` (`{"auth_mode": "apikey", "OPENAI_API_KEY": …}`, as `codex login --with-api-key` writes it) mode 0600 in a 0700 directory: OpenAI answers `401 Unauthorized: Incorrect API key provided: sk-shard*******************-key … auth error code: invalid_api_key`. Claude Code: `--settings /run/claude-auth/settings.json` (`{"apiKeyHelper": "cat /run/claude-auth/api-key"}`) over `-v HOSTDIR:/run/claude-auth:ro`, both files 0600 in a 0700 directory: `apiKeySource: "apiKeyHelper"`, every request `error_status: 401`, `Invalid API key · Fix external API key`. Probe: the harness is `uid=200000 gid=200000`, the directory `drwx------ 200000 200000`, the file `-rw------- 200000 200000`, read | **native delegation**: the harness, its tools and its children can read the key; Codex writes its token refreshes back there (the share must be writable for a ChatGPT sign-in, and for Codex's own state) |
| A secret mount (Docker-style `--secret`, a tmpfs file per member) | None at run time; BuildKit's build secrets reach `RUN` steps only | — | would be delegation unless brokered |
| An opaque broker | Not built. Its shape from today's primitives, untested: the key in a separate `AGENT` (for example OpenAI's own `codex-responses-api-proxy`, published in the same release; OpenAI's Codex GitHub Action "is designed to reduce API key exposure by installing Codex, starting a Responses API proxy, and running Codex", developers.openai.com/codex/noninteractive) with the only egress to the provider, the harness granted a flow to it alone (`CONNECT --port=… codex TO proxy ON …`) and pointed at it by options (`-c model_providers…`); for Claude Code a gateway named by `ANTHROPIC_BASE_URL` in its settings. Whether the build's transitive-reach closure (§9.5) admits that topology, and how the broker itself receives its key, are untested | — | **opaque** for the harness (it never holds the key), though it can still spend the key's authority through the broker |

What is missing, for the person's choice:

1. A run-time secret primitive for members: per-member, tmpfs-backed, never in the image,
   the store or a host directory the run shares; today's working way is a host directory
   shared over virtio-fs for the run's life.
2. The simple `harness.json` cannot point a harness at its credential by environment
   (`CODEX_HOME`, `CLAUDE_CONFIG_DIR`); its full OSI form can, and a `--settings`-style
   option can where the harness has one.
3. A volume shared by more than one domain is root's (`domains.rs:1476`), so a credential
   share must name one harness.
4. No refresh, rotation or revocation contract (G18): a ChatGPT or Claude sign-in writes its
   refreshed tokens wherever the harness keeps them.
5. No opaque broker (P7/P8/P10).

## Findings

**H1, red, not fixed (outside this task's paths and D54).** An owner's subscription to a
harness's channel ends when the harness says nothing for 100 s: the owner endpoint's QUIC
`max_idle_timeout` is its patience, `shards_remote::tls::QUIET` = 100 s
(`crates/shards/src/daemon/managed.rs:832`, `crates/remote/src/managed.rs:655-673`), and its
transport sets no keep-alive, where `tls.rs`'s other transports keep theirs alive every
quarter of their bound (`crates/remote/src/tls.rs:79`). Reproduced live with a probe harness
(`sh -c "sleep N; echo after-quiet"`), bracketed:

| Silence before the harness's one line | Run | Owner's stdout subscription |
|---|---|---|
| 75 s (`QUIET − QUIET/4`) | final (`probe-within`) | End at 75.00 s, `after-quiet` delivered |
| 75 s | earlier, the same test code (comments edited since) | End at 75.00 s |
| 125 s (`QUIET + QUIET/4`) | final (`probe-quiet`) | `transport Lost(None)` at 102.86 s, nothing delivered |
| 125 s | earlier | `transport Lost(None)` at 100.40 s |

Each subscription opened before the silence and was acknowledged to its last record; the
harness's `after-quiet` and End were then never delivered to its owner. A model's long
turn, or a harness whose stderr stays quiet while its stdout works, is such a silence:
Claude Code's own 401 retries left its stderr silent for 263 s and lost that subscription
(an earlier run, records not kept; the test now subscribes each channel when it reads it).
`an_owners_subscription_outlives_a_quiet_harness` is the red test; a keep-alive within the
bound on the managed transport is the likely fix, not tried here.

**H2: egress names ports, not destinations.** `--egress=443/tcp` with `EXPOSE … AS egress`
lets the harness open TCP 443 to any address past the microVM, and `--dns` lets it resolve
any name (`crates/init/src/netplan.rs:87-98`). Codex used two provider hosts,
`api.openai.com` and `chatgpt.com` (its model list); no Agentfile form grants "TCP 443 to
those hosts alone". Only a remote `MCP` server's grant is host-scoped (its resolved
addresses, its one name). The person decides whether provider egress needs a destination
form.

**H3: no command reads a member's output.** It goes only to the owner endpoint
(authenticated, per channel, D180); `shards logs` is the command's. A person running a
harness today sees its process record, not what it says.

**H4: `HOME=/tmp` and Codex.** Every domain's `HOME` is its scratch `/tmp`
(`domains.rs:493`). Codex then warns `Refusing to create helper binaries under temporary
dir "/tmp" (codex_home: AbsolutePathBuf("/tmp/.codex"))` and makes none of its helper
aliases; with `CODEX_HOME` on a volume the warning is gone (`codex-login-volume`).

**H5: input.** A domain's stdin is `/dev/null`; Codex reads it (`Reading additional input
from stdin...`), finds it empty and goes on. The interactive input and TTY contract (G24)
remains open.

**H6: the harnesses' own behavior without a credential or a network.** Claude Code refuses
locally (`Not logged in · Please run /login`, `duration_api_ms: 0`, no request), so its reach
is shown with a placeholder key. With a key it retries a 401 ten times with backoff
(2 s rising to about 35 s; 246.5 s in all in the final run, 263 s in an earlier one)
before giving up. Codex without a network waits for
one as long as it runs (`Reconnecting... waiting for network`, more than 9 minutes in an
earlier run); the owner's `shards rm -f` ends it.

## R01–R16

| Rule | This change |
|---|---|
| R01 | No production code changed. The test may unwrap and assert. |
| R02 | No architecture decided. Sources: the publishers' release workflow, installer and docs (cited above); the base image and the musl libraries Anthropic documents; each CLI's own `--help` and docs for its flags and credential inputs. Credential options are reported for the person, not chosen. |
| R03 | No production literal. Test patience `WITHIN` = 600 s and the H1 silences, `QUIET ± QUIET/4` (one keep-alive interval as `tls.rs` sets it), are test-only and documented in the test. The review (plan step 2, [../harness-bringup-v2/review.md](../harness-bringup-v2/review.md)) removed `WITHIN`, the run's `sleep 600` and `bringup.sh`'s 300 s deadline: the test and the script wait on facts with no clock. |
| R04 | No state machine added; the observer's records pass the existing `StreamState` checks. |
| R05 | No production interface; the test's `Case` serves its nine cases. No cache or wait added to production. |
| R06, R07, R13 | One test file, a shared bring-up, comments for intent and sources. |
| R08, R14 | No performance claim. Durations are single observations (n = 1), not measurements. |
| R09 | Each finding was reproduced live; H1 is bracketed by a passing twin; the volume ownership was predicted wrong from source, then measured. |
| R10 | Harnesses, records and the red test are here; guard checks for the binary verification are recorded. No production guard changed, so there is no guard-removal mutation to run. |
| R11 | Verified: Linux x86_64 on KVM only. Unverified: macOS arm64 (HVF), Windows (WHP), Linux arm64, musl hosts; the publishers ship arm64 builds of both (`aarch64-unknown-linux-musl`, `linux-arm64-musl`), which fetch.sh does not fetch yet. |
| R12 | Nothing platform-specific. |
| R15 | Implementer scope: single-VM bring-up runs and light checks; suites, lint and platforms are the coordinator's. |
| R16 | adacachy keeps about 2 GB of regenerable downloads and work directories under `~/.cache/shards-harness-bringup-v1`; nothing was built on the Mac (26 GiB free, below its floor). |

## Owed to the coordinator

- `scripts/lint` (all seven targets) and the workspace suites with the new test target
  (it compiles everywhere `build.rs`'s use of the observer does; its tests are ignored).
  Run here: `rustfmt --check` of the new file, `cargo clippy --profile test-release -p shards
  --test harness_bringup -- -D warnings` on x86_64 Linux ([records/clippy.log](records/clippy.log)),
  `scripts/check-development-policy`.
- The bring-up on macOS arm64, Windows and Linux arm64 hosts, with each publisher's arm64
  build.
- H1's fix and its green run; H2's and the credential choices are the person's.
- The integration branch has moved to `032b63b1c` (D230's engine ring and its shards-vmm
  fix); this branch is on `ef687bc77`, which neither break nor fix touches, and adds no
  production source.
