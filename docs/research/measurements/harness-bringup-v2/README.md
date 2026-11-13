# Harness bring-up v2: a real turn of Codex CLI and of Claude Code, each signed in, in a Shards microVM

The managed-runtime plan's G18 (native sign-in delegation) and P10, after
[v1](../harness-bringup-v1/README.md). v1 started each publisher's binary as a harness from
`HARNESS` and its `harness.json` alone ([D54](../../../design/architecture.md)) and showed it
reach its provider over the grants its Agentfile names, refused for want of a credential.
Here each harness is given the person's own sign-in, at run time and to it alone, and does
one real turn: it runs a tool in its microVM, `ls` over a read-only project volume, and
answers with what it found. The person asked, through the coordinator, that their Codex
and Claude Code sign-ins be used for this test. Implementer record; the coordinator owns
suites, lint gates, multi-platform validation and review.

## Identity

| | |
|---|---|
| Policy | `docs/development-rules.md` sha256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`, read at `origin/main` `bff301518` and again, unchanged, at `c4ae59ee0` after the session's restart; role implementer ([POLICY_READ.json](POLICY_READ.json)) |
| Source | branch `p/harness-bringup-v2`, from `origin/p/integrate-engine-20261111` at `5b3e8f784` (which holds v1's merge `1da9132b7`). Both runs used `df1d34af0`: that base and this change's first four signed commits. Later commits change only comments, records and this file |
| Host | kiln runner adacachy (`451397fcc585f7f7cb9aaf00e42064bb`): CachyOS, Linux 7.2.9-2-cachyos, x86_64, 32 CPUs, KVM. Its user holds both sign-ins, which never left it |
| Toolchain | Rust 1.98.1 (`rust-toolchain.toml`), zig 0.16.0 |
| shards | `cargo build --profile test-release -p shards` at `4c2fcfb08`, cold: 2 min 04 s; the test binary 1 min 24 s more |
| Guest | shards' pinned kernel `vmlinux-6.18.48-x86_64-17c3ba205400` and the test build's shards-init, as in v1 |
| Base image | `alpine:3.24.4@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf` |
| Harnesses | v1's verified binaries ([fetch.sh](../harness-bringup-v1/fetch.sh)), their SHA-256s checked against v1's `fetched.json` again before reuse: Codex CLI 0.167.2 (`codex` `a9672d0f…`, `codex-code-mode-host` `007c6279…`) and Claude Code 2.1.322 (`claude` `9bed6cfc…`) |

## The examples

[docs/examples/harness-codex-signed-in](../../../examples/harness-codex-signed-in) and
[docs/examples/harness-claude-signed-in](../../../examples/harness-claude-signed-in) are
v1's, with three changes.

**Two volumes, each for the harness alone.** One is the harness's home or config
directory; the other is the project. A run stands in host directories for both:

```dockerfile
VOLUME --target-kind=harness /run/codex-home FOR codex
VOLUME --target-kind=harness /work FOR codex
```

**`harness.json`'s full OSI form, which sets the harness's environment** (v1's credential
gap 2). Codex has `"env": ["CODEX_HOME=/run/codex-home"]`; Claude Code has
`"env": ["CLAUDE_CONFIG_DIR=/run/claude-home"]`.

**A prompt that needs a tool, about files whose names no prompt holds.**

    codex exec --json --skip-git-repo-check --ephemeral --dangerously-bypass-approvals-and-sandbox -C /work \
      "Run `ls -1` in the working directory once, then reply with exactly the file names it printed, one per line, and nothing else."
    claude -p "Use the Bash tool to run `ls -1 /work` once, then reply with exactly the file names it printed, one per line, and nothing else." \
      --output-format stream-json --verbose --add-dir /work --allowedTools "Bash(ls:*)"

- **Codex's flags.** Its own sandbox is off: the microVM and its grants are the boundary
  under test. `-C` sets its working root.
- **Claude Code's flags.** Print mode runs only the tools it is allowed, so `ls` is allowed.
  Its prompt comes straight after `-p` (finding S1).
- **Claude Code's image.** It adds `bash` (`RUN apk add --no-cache bash libgcc libstdc++`),
  because its Bash tool runs commands with `/bin/bash -c` (the kernel's record below).

Each test makes the project for its run: two files, `shards-turn-alpha.txt` and
`shards-turn-beta.md`.

## Credentials

[signed-in.sh](signed-in.sh) `codex|claude BIN_ROOT WORK` runs on the machine that holds the
sign-in, from this repository; [credentials.py](credentials.py) does each step in-process:

1. **copy.** The sign-in file is copied into a fresh 0700 directory on the user's tmpfs
   (`$XDG_RUNTIME_DIR`, here `/run/user/1000`), as a 0600 file, never to a disk:
   - Codex: `~/.codex/auth.json`, a ChatGPT sign-in (`auth_mode`, `tokens`, no API key).
   - Claude Code: `~/.claude/.credentials.json`, a Claude.ai OAuth sign-in (`claudeAiOauth`).

   The original's path, SHA-256 and mode are kept beside the copy, in a file the harness is
   not given. The step prints how long the access token stays valid and, for Codex, when it
   was last refreshed (minutes and days, no value). A harness refreshes a token that is
   about to expire, and a refresh can retire the refresh token that every other holder of
   the original still has.
2. **run.** The test (`*_signed_in_*` in
   [harness_bringup.rs](../../../../crates/shards/tests/harness_bringup.rs)) gives the
   harness the copy's directory as its home: `shards run vm -d -v COPY:/run/codex-home -v
   PROJECT:/work:ro …`, on the volume declared for that harness alone.
3. **Afterwards, whatever the test did,** the exit trap runs three steps:
   - **leakcheck** reads every file under `WORK` and looks for every secret of the original
     and of the copy: each string held under a name with `token`, `key`, `secret` or
     `account_id` in it, whatever its length. A file is a leak if it holds a whole secret, or
     a run of a secret's consecutive bytes one longer than the most a provider shows of a key
     (OpenAI's refusal shows a key's first 8 and last 4 characters: v1's
     `records/codex-login-volume/stdout.bin`). Every 9-byte run of each secret is kept, and
     looked up at every offset of every file. The step prints found or not found, and a leak's
     path, never a value. The runs below used its first form, which skipped values under 16
     characters and promised runs of 31 bytes; the review (plan step 2,
     [review.md](review.md)) replaced both.
   - **settle** compares the copy with its original by SHA-256. A copy whose tokens the
     harness refreshed is written back over the original atomically: a temporary file in the
     original's directory, fsync, rename, then the directory fsync'd, with the original's
     mode kept. It is not written back if the original changed meanwhile; that is a
     conflict, and is reported. Any other change, such as a newer format of the same tokens,
     is not written back.
   - **scrub** overwrites every file in the private directory with zeros, fsyncs it and
     removes it, then removes the directory.

Every line the script prints is a status, a count, a duration or a path.

**Self-tests** ran on fake sign-ins on the Mac, with no real credential (committed since the
review as [credentials_test.py](credentials_test.py)):
- **copy:** done as specified.
- **leakcheck:** found a planted whole value, and planted unaligned pieces of 31 bytes (and,
  in that run, of 30, below what it promises). Its first version looked at fixed-stride
  pieces and could miss unaligned runs; the planted-piece test found that, and the window
  lookup above fixes it.
- **settle:** all four outcomes behaved as specified. An unchanged copy and a format-only
  change were left alone. A refresh was written back, its mode kept and no temporary file
  left. A conflict was not written back.
- **scrub and validity:** scrub removed everything; validity printed minutes, expired and
  unknown as specified.

**Limits.**
- **This is native delegation (G18), not an opaque credential.** The harness and every
  process it starts can read the copy, and could send it anywhere its grants reach (any
  address on TCP 443: v1's H2). The leak check sees only the files the test keeps.
- **The leak check promised runs of 31 bytes or more** in these runs (9 since the review). The committed records were also
  scanned for the shapes credentials take: Anthropic and OpenAI key prefixes, JWT headers,
  bearer headers and token field names. None was found. No email address, and no account
  or organization field, is in them.
- **A write-back reaches only the file.** Processes already running on the host keep the
  tokens they read: adacachy was running 5 Codex and 55 Claude Code processes. Neither turn
  refreshed.

## Runs

On adacachy, from the checkout at `df1d34af0`:

    docs/research/measurements/harness-bringup-v2/signed-in.sh codex BIN_ROOT WORK
    # runs: cargo test --profile test-release -p shards --test harness_bringup -- \
    #   --ignored --exact a_real_codex_signed_in_runs_a_tool_and_answers
    docs/research/measurements/harness-bringup-v2/signed-in.sh claude BIN_ROOT WORK
    # runs: … --exact a_real_claude_code_signed_in_answers

The records are in [records/](records):
- **Per case:** the same files as v1's ([codex-signed-in](records/codex-signed-in),
  [claude-signed-in](records/claude-signed-in)): Agentfile, harness.json, build, run, the
  owner's `stdout.bin` and `stderr.bin`, `ends.txt`, the kernel's process record, ports, and
  transport outcomes.
- **Per run:** the script's own output ([codex-run.log](records/codex-run.log),
  [claude-run.log](records/claude-run.log)).
- **Not kept:** the test's raw output. It repeats those records, because the test writes
  them to its stdout directly, and it carries the account fields below unredacted.

| Test (record) | Result | Test | Owner's stdout | Harness exec → its tool's exec → its exit (kernel's record) | The copy afterwards |
|---|---|---|---|---|---|
| `a_real_codex_signed_in_runs_a_tool_and_answers` (`codex-signed-in`) | pass | 35.6 s | 16.3 s, End | 06:52:35.070 → +9.19 s `ls -1` → +16.29 s code 0 | unchanged: nothing written back; 438 files scrubbed |
| `a_real_claude_code_signed_in_answers` (`claude-signed-in`) | pass | 30.0 s | 11.0 s, End | 07:08:39.987 → +6.79 s `ls -1 /work` → +10.99 s code 0 | rewritten with the same tokens: not written back; 240 files scrubbed |

- **Both cases:** read both channels to their ends with no transport error, and every
  stdout line parsed as JSON as written.
- **Leak check:** not found, in each run's 12 files (50,197 and 147,674 bytes).
- **Durations:** each case ran once (n = 1), so its durations are single observations.
- **Build and start:** image builds took 12.6 s and 15.4 s, and `run vm -d` took 4.5 s and
  2.5 s, for Codex and Claude Code in that order.
- **What each case asserts:**
  - the turn ended without an error: Codex's last event is `turn.completed`, and Claude
    Code's `result` has `is_error` false;
  - a tool ran: for Codex, an `ls` `command_execution` with exit code 0 whose output holds
    both file names; for Claude Code, a `Bash` `tool_use`;
  - the answer holds both file names;
  - the kernel saw `ls` exec'd in the harness's own domain.

### Codex

Its JSONL, whole ([records/codex-signed-in/stdout.bin](records/codex-signed-in/stdout.bin)):

    {"type":"thread.started","thread_id":"01a1ce87-9a28-7be1-a8d4-bca9b270d6fc"}
    {"type":"turn.started"}
    {"type":"item.started","item":{"id":"item_0","type":"command_execution","command":"/bin/sh -lc 'ls -1'","aggregated_output":"","exit_code":null,"status":"in_progress"}}
    {"type":"item.completed","item":{"id":"item_0","type":"command_execution","command":"/bin/sh -lc 'ls -1'","aggregated_output":"shards-turn-alpha.txt\nshards-turn-beta.md\n","exit_code":0,"status":"completed"}}
    {"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"shards-turn-alpha.txt\nshards-turn-beta.md"}}
    {"type":"turn.completed","usage":{"input_tokens":29344,"cached_input_tokens":26752,"cache_write_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":0}}

Its stderr is `Reading additional input from stdin...` (v1's H5) and nothing else. v1's
warning about helper binaries under `/tmp` is gone, as its home is the volume (v1's H4).
The kernel's record ([processes.log](records/codex-signed-in/processes.log)) shows the tool
running in the harness's own domain:

    harness codex   960  exec   /harness/codex/bin/codex exec --json --skip-git-repo-check --ephemeral --dangerously-bypass-approvals-and-sandbox -C /work "Run `ls -1` in the working directory once, …"
    harness codex  1846  exec   /harness/codex/bin/codex-code-mode-host
    harness codex  1868  exec   /proc/self/exe --codex-run-as-process-setup 54 2 pipe "" /work /bin/sh /bin/sh -c "…"
    harness codex  1868  exec   /bin/sh -c "__CODEX_SNAPSHOT_OVERRIDE_SET_0=…"
    harness codex  1868  exec   /bin/sh -c "ls -1"
    harness codex  1868  exec   ls -1
    harness codex  1868  exit   code 0
    harness codex  1846  exit   signal 9 (SIGKILL)
    harness codex   960  exit   code 0

### Claude Code

Its `system` `init` says `"apiKeySource":"none"`: no key, so the sign-in was in use. Its
stream-json ([records/claude-signed-in/stdout.bin](records/claude-signed-in/stdout.bin)),
shortened:

    {"type":"system","subtype":"init",…,"model":"claude-opus-5-5",…,"apiKeySource":"none","claude_code_version":"2.1.322",…}
    {"type":"assistant","message":{"model":"claude-opus-5-5","id":"msg_011CgxcYnC5kuwMUU25TdWUc","type":"message","role":"assistant","content":[{"type":"tool_use",…,"name":"Bash","input":{"command":"ls -1 /work","description":"List files in work directory"}}],…},…}
    {"type":"user","message":{"role":"user","content":[{…,"type":"tool_result","content":"shards-turn-alpha.txt\nshards-turn-beta.md","is_error":false}]},…}
    {"type":"assistant","message":{"model":"claude-opus-5-5","id":"msg_011CgxcZ2ejnGhD1qh1ggagw","type":"message","role":"assistant","content":[{"type":"text","text":"shards-turn-alpha.txt\nshards-turn-beta.md"}],…},…}
    {"duration_api_ms":6756,"stop_reason":"end_turn",…,"total_cost_usd":0.09632299999999999,…}

Each message `id` is Anthropic's API's. The `result` event, in full in the record, has
`"subtype":"success"`, `"is_error":false`, `"num_turns":2`, `"duration_ms":7888`,
`"permission_denials":[]` and `"result":"shards-turn-alpha.txt\nshards-turn-beta.md"`.
Its stderr is empty. The kernel's record
([processes.log](records/claude-signed-in/processes.log)):

    harness claude   953  exec   /harness/claude/bin/claude -p "Use the Bash tool to run `ls -1 /work` once, …" --output-format stream-json --verbose --add-dir /work --allowedTools Bash(ls:*)
    harness claude  1795  exec   /bin/bash -c -l "SNAPSHOT_FILE=/run/claude-home/shell-snapshots/snapshot-bash-…"
    harness claude  1798  exec   /bin/bash -c "source /run/claude-home/shell-snapshots/snapshot-bash-….sh 2>/dev/null || true && …"
    harness claude  1799  exec   ls -1 /work
    harness claude  1799  exit   code 0
    harness claude   953  exit   code 0

**Redacted.** Three fields of the committed `stdout.bin` describe the account, not the
harness. Each value is replaced by a string that says what it held:
- line 1, `mcp_servers`: the account's claude.ai connectors;
- line 4, `rate_limit_event`'s `rate_limit_info`: the account's rate-limit state;
- line 8, `credential_expires_at`: the sign-in's expiry.

Every other line is byte for byte as the owner read it. The raw stream is 61,020 bytes,
sha256 `5e949c3b7cffb5effdda8ab6544f5791df245da9ef7d21c1e1af87bf2a67887b`; it is kept on
adacachy and not committed. The committed stream is 60,783 bytes, sha256
`98002b6bf20e8e9fccc2d36c1faa43a578a6e0e7ee840415726175a738542f7b`.

## Findings

**S1: a harness's literal arguments are argv as its CLI parses them.** Claude Code declares
`--add-dir <directories...>` and `--allowedTools, --allowed-tools <tools...>`, which are
variadic lists. That is from `claude --help` of 2.1.296, adacachy's own install; the musl
2.1.322 build does not start on that glibc host. A prompt after either option is taken as
one more of its values, which leaves `-p` with no prompt. So the descriptor puts the prompt
straight after `-p`. Not reproduced live: the descriptor was corrected before its first run.
A `harness.json` author has no way to see this but the CLI's own parsing, which G24's
descriptor documentation could warn of.

**S2: native delegation and refresh.** Neither harness refreshed:
- Codex 0.167.2 left the ChatGPT sign-in's tokens as they were. They had last been
  refreshed 9 days before, and the access token was valid for 17.7 more hours.
- Claude Code rewrote its credential file with the same tokens, and settle kept that out of
  the original.

v1's open G18 item stands: there is no refresh, rotation or revocation contract. A refresh
in a microVM can retire the refresh token that the host's running processes still hold, and
the write-back reaches the file but not them.

**S3: a harness's home holds its whole state as well as its credential.** Given a directory
as `CODEX_HOME` or `CLAUDE_CONFIG_DIR`, each harness wrote its state there: 438 and 240
files, including shell snapshots (the kernel's record shows Claude Code's under
`/run/claude-home/shell-snapshots`). Settle reads only the credential file, and scrub
removes everything. v1's missing per-member secret primitive would hold the credential
alone. The harness would still want a writable home of its own for its state.

**Observed, not investigated.**
- **A new run warning.** Both runs warn on stderr: `CAP_NET_RAW withheld from the command:
  the image's Agentfile runs agents and harnesses beside the command, and CAP_NET_RAW reads
  and writes the packets of their flows past the microVM, which cross the command's network
  namespace`. v1's records have no such warning.
- **A build progress label.** The build's progress numbers the harness's second step
  `[3/2]`, as v1's builds did.
- **H1 not exercised.** v1's H1 did not come into play: neither harness was silent for long,
  and each stdout ended by 16.3 s. The coordinator reports its fix on main (`4e291dfc3`),
  which these runs predate.

## R01–R16

| Rule | This change |
|---|---|
| R01 | No production code changed: one test file, the examples and these scripts. `credentials.py` never raises with a credential in its message: `validity` answers "unknown". |
| R02 | No architecture decided. Sources: the publishers' CLIs (`codex exec --help`, `claude --help`) and their documented homes (`CODEX_HOME`, `CLAUDE_CONFIG_DIR`); v1's measured volume ownership (a run-time share shows as the harness's own). Native delegation is G18's label; the opaque broker remains the person's choice (v1). |
| R03 | No production literal. The leak check's 24-byte window, 8-byte stride and 16-character floor were chosen test parameters; the review replaced them with every value whatever its length and every run one longer than a provider shows of a key ([review.md](review.md)). |
| R04 | No state machine added. Settle's outcomes are named in its output: unchanged, format only, refresh written back, conflict. |
| R05 | No production interface, wait or cache. Two cases join v1's `Case` and `bring_up`. |
| R06, R07, R13 | Two cases and two small scripts. Comments state safety obligations and sources; an unused helper was removed. |
| R08, R14 | No performance claim: the durations are single observations. |
| R09 | Each pass is a live run. S1 was corrected before any run, not reproduced (said so above). The leak check's alignment gap was found by a planted-piece test, fixed and tested again. |
| R10 | The harness, records and self-tests are here. No production guard changed. The cases' guards are the kernel's record of `ls` in the harness's domain, and both names in the tool's output and in the answer. Their removal mutation (a prompt asking for no tool must fail) costs a real turn and was not run (owed). |
| R11 | Verified: Linux x86_64 on KVM only. Unverified: macOS arm64 (HVF), Windows (WHP), Linux arm64. Both publishers ship arm64 builds, which fetch.sh does not fetch yet. |
| R12 | Nothing platform-specific in shards. The script's private store is Linux's per-user tmpfs; another host needs another. |
| R15 | Implementer scope: two single-VM turns and light checks. The person's rule of 2026-11-12, relayed by the coordinator, has the implementer merge its work to main; the checks it leaves owed are below. |
| R16 | adacachy keeps `~/.cache/shards-harness-bringup-v2`: v1's verified binaries, this build's target, and the work directories with the raw Claude stream (for the redaction's check). v1's 6.9 GB there was removed. Nothing was built on this Mac. |

## Owed to the coordinator

- **Lint and suites.** `scripts/lint` (all seven targets) and the workspace suites with this
  test file. Run here:
  - `rustfmt --check` of the test file;
  - `cargo clippy --profile test-release -p shards --test harness_bringup -- -D warnings` on
    adacachy (x86_64 Linux): clean, 1 min 29 s;
  - `scripts/check-development-policy`: passed.
- **Other hosts.** The signed-in turns on macOS arm64, Windows and Linux arm64 hosts.
- **The mutation.** R10's check: a prompt asking for no tool must fail a case. Done without a
  prompt by the review ([review.md](review.md), F7): the recorded turns less their tool fail
  their cases. A real turn of such a prompt stays ROOT's to decide.
- **A long turn.** A turn with a silence past the owner endpoint's former 100 s bound, now
  that H1 is fixed on main. The test keeps no clock of its own since the review (F2, F4).
- **The person's choices (G18).** A refresh, rotation and revocation contract; a per-member
  secret primitive; an opaque broker.
- **The merge.** This branch merges `origin/main` before its push, but the runs above used
  `df1d34af0`, without that merge.
