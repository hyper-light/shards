# Harness bring-up v1 and v2: review (plan step 2)

Goal 52's plan step 8, a child of goal 52's agent (`a88ba9336b2a5c118`) for ROOT's plan step 2:
harness-bringup-v1 and -v2 (a real Codex CLI and a real Claude Code as `HARNESS` members),
read against R01–R16 and AGENTS.md's review rules, its defects cured on main and the rest
recorded as owed. No vendor CLI was sent a prompt, and no credential was read.

## Identity

| | |
|---|---|
| Role | implementer (goal 52's child for step 8); coordinator ROOT |
| Policy | read at origin/main `ecd19d38cd756219238650111eb9278a312fa97e`: AGENTS.md sha256 `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41`, docs/development-rules.md `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`, docs/development-evidence.md `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63`, CLAUDE.md `588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010`; with ROOT's briefs (`/tmp/g121-shot/goal-agent-brief.md`, `brief-common.md`) and goal 52's (`/tmp/s2/child-brief.md`, its addendum of 2026-11-13, `steps/step-8.txt`, `steps/step-8-findings-so-far.md`) |
| Worktree | `.claude/worktrees/s2-harness`, branch `p/s2-harness-v1`, landed on main fast-forward, signed |
| Owned | `crates/shards/tests/harness_bringup.rs`; `docs/examples/harness-{codex,claude,codex-signed-in,claude-signed-in}`; `docs/research/measurements/harness-bringup-v1/` and `-v2/`; in `docs/design/architecture.md`, D236's list of examples to move and this review's pointer |
| Runner | adas-mac-studio-3.local (kiln runner `2768d0d5…`, hostname Adas-Mac-Studio-4.local): macOS 26.2 (Darwin 25.2.0), arm64, 14 cores, 36 GiB; Python 3.9.6, rustc 1.98.1. No gpg, zig or cargo-xwin there. Every run went through a carrier of this step's (a patch over main applied to a clone of its own, `~/g121-tmp/s2-8-src` and `-py`) |

## Commits reviewed

Each read whole (`git show`), on main since before this review:

| Commit | What it landed |
|---|---|
| `6e17b36b9` | v1: the examples `harness-codex`, `harness-claude`; `harness_bringup.rs`'s nine cases; `fetch.sh`, `bringup.sh`; the records |
| `4c2fcfb08` | v2 step 1: the `-signed-in` examples, `credentials.py`, `signed-in.sh`, two signed-in cases |
| `4c1f595f9` | v2: a tool's run is its program's exec in the kernel's record |
| `7da26ccf4` | v2: Claude Code's prompt before its variadic options; token validity in minutes |
| `df1d34af0` | v2: settle writes back only a refresh |
| `ab73c9628` | v2: the two real signed-in turns recorded, README |

## Secrets: none committed

Goal 52's agent scanned the 162 files these six commits touch (at main `308895ef4`, with
redaction) for API keys (`sk-…`), JWTs, bearer tokens, token or key fields, GitHub and AWS
keys, private keys, emails, and account, organization and user IDs
(`/tmp/s2/steps/step-8-findings-so-far.md`). Every hit was benign: the placeholder
`sk-shards-placeholder-not-a-key` (`harness_bringup.rs`' `PLACEHOLDER` and its uses;
`records/codex-run-env/run.log`'s `-e …_API_KEY=` is it), and v1's
`fetch/verify.status`, a GnuPG `GOODSIG` of Anthropic's public release-signing identity. By
key name, v2's records hold no token, key, password, account, organization or email value,
and `credential_expires_at` is redacted. Claude Code's stream
(`records/claude-signed-in/stdout.bin`) carries request identifiers: one `session_id`, 8
message `uuid`s and 2 `request_id`s. These are not credentials or account IDs, and they are
left as recorded. This review did not scan again.

## Findings

R-numbers are the rules each finding is held to. "Fixed" names the landing commit; each fix
was red before, green after, and red again with its guard removed, each its one focused test.

### F1. The leak check skipped short values and chose its run length (R03): fixed

`credentials.py`'s `secrets` (line 68 at `ecd19d38c`) took a value under a secret's name only
at 16 characters or more, so a shorter secret was never looked for. Its fragment check
(lines 38–39) kept every 24-byte window of a secret and looked at every 8th offset of a file,
which promised runs of 31 bytes. All three numbers were chosen.

Now every value under a secret's name is a secret, whatever its length; an empty one holds
none. A file leaks where it holds a whole secret, or a run of one longer than the most a
provider itself shows of a credential. That length comes from data: OpenAI's refusal shows a
key's first 8 and last 4 characters, the rest masked one for one
(`harness-bringup-v1/records/codex-login-volume/stdout.bin`: `sk-shard*******************-key`
for the 31-character placeholder). So the run is 9 bytes, looked up at every offset.
`credentials_test.py` reads the 8 from that record itself, not from the script.

v2's self-tests were never committed. `credentials_test.py` now holds them (copy, settle's
four outcomes, scrub, validity), with the leak check's. Every fake secret is a digest of a
label, and the leak check also runs over v1's and v2's committed records.

`python3 -I docs/research/measurements/harness-bringup-v2/credentials_test.py`, on the runner
(`~/g121-tmp/s2-8-slice-a.log`):

| Tree | Result |
|---|---|
| this change | 14 passed |
| red: `credentials.py` as on main, the new test | 2 failed: a 9-byte run not found at offset 0; a 12-character secret not found (`0 != 3`) |
| M1: the 16-character floor back | `test_a_secret_however_short_is_looked_for_whole` failed |
| M2: every 8th offset alone looked up | `test_a_run_one_longer_than_a_provider_shows_is_found_at_every_offset` failed, at offset 1 |
| M3: an empty value taken for a secret | `test_an_empty_value_is_no_secret` failed: FOUND in every file |
| this change again | 14 passed |

### F2. The real-VM test kept a 600 s clock over every wait (R03, R05): fixed

`harness_bringup.rs`'s `WITHIN` (line 34 at `ecd19d38c`) bounded every wait: each `shards`
command (`run_shards_env`'s timeout), each channel's read (`recv_timeout`, failing "past the
test's patience"), the owner observers' deadlines and their joins. v1's slowest case already
read for 247.2 s (`claude-key-volume`), and a model's long turn has no bound of its own. ROOT
ruled that the tests' bounds go (D215; s37 decision inventory, row 2): no clock, wait on
facts.

Now every wait ends at its fact. `read` waits on `recv()` alone: the channel's end record, its
subscription's transport event, or its driver's end. A command is waited on to its exit, and
the observers are started and joined with `until` = now + `NO_CLOCK`, u64's most nanoseconds,
because those fixtures take a deadline that is finite by contract (D215's tests give theirs
the same; `checked_add`, so a platform whose `Instant` can't hold it fails with its name).
An observer's subscription still ends at its connection's typed end, held to the endpoint's
own liveness. Today the owner's patience is `until`'s, far past the endpoint's `tls::QUIET`,
and a connection takes the lesser of its two sides' idle timeouts (RFC 9000 §10.1). Goal 58's
call C (D180, not on main at this review) drops `Admitted.patience` and
`DriverLimits.handshake_patience`: neither side then advertises an idle timeout, and the
driver ends a connection that stops making progress as `TimedOut`. Its agent was told this
test's two call sites. No QUIET-based value was put in their place, as ROOT asked.

### F3. The test polled the process record every 200 ms (R03, R05): fixed

Once the harness's output had ended, the test read `shards read processes` again every 200 ms
(line 262) until the harness's exit row came, or its patience ran out: a periodic probe of a
record that announces its own growth. `shards watch processes` (D123) prints the record, then
each row as the daemon's file watch sees it, until the run ends. The test now follows that
stream until `followed_far_enough`: the exit of the harness's own exec, by that exec's PID
(not a child's), or a `lost` row (init's, member `-` and PID 0, past which the record waits for
no process's end). It also stops if the record ends with the run. Then it reads the record
whole once, as the last poll did.

### F4. The run's own command ended at 600 s (R03): fixed

Every case ran its microVM's command as `sleep 600`, as did `bringup.sh` and the four
examples' usage comments. The run ends with its command and its members with it, so that was
a second 600 s bound on any harness. Now it is `sleep infinity`: busybox's `sleep` takes "inf"
for no end, as GNU's does, since busybox `edca770d11` (2019-01-07, release 1.31.0). Alpine
v3.24's main repository ships busybox 1.37.0-r31 (pkgs.alpinelinux.org, read 2026-11-13). The test or the script removes the run when it is done.

### F5. bringup.sh's 300 s deadline, polled every 2 s (R03, R05), and its GNU-only clock (R11): fixed

`bringup.sh` (lines 66–82) read `shards read processes` every 2 s for 300 s. It now follows
`shards watch processes` through a FIFO, row by row, until the harness's exit by its exec's
PID, or a `lost` row, or the record's end. Then it ends the follow it started and reads the
record whole. Its `step` timed each command with `date +%s%N`, which BSD's `date` (macOS's)
prints as a literal `N`, so the script failed in its first arithmetic there. It now times in
milliseconds where `date` gives nanoseconds, and in whole seconds where it doesn't, saying
which.

`bringup-test.sh` holds it to v1's record of Codex's run, with no VM, binary or network. A
stand-in `shards` gives that record through `watch processes`, then keeps the follow open as a
real one does. Its `read processes` gives the record without Codex's exit, and a stand-in
`date` moves 301 s at each call.

### F6. H1's live test still said it was red (R07): fixed

`an_owners_subscription_outlives_a_quiet_harness` was ignored as "red: finding H1, the owner
endpoint's transport keeps no quiet subscription alive". H1 is fixed on main
(owner-keepalive-v1, `4e291dfc3`'s merge; D180's managed-output paragraph), so the test is
expected green. Its text now says so, and both of H1's tests say they need a real VM (ROOT's).
After goal 58's call C the managed transport keeps no idle timeout at all. The silences these
tests take from `tls::QUIET` are then goal 58's to restate; the tests' claim, that a quiet
subscription lives, stands.

### F7. R10's owed check of the signed-in turns, with no prompt sent: done

v2 owed a mutation: "a prompt asking for no tool must fail a case". That costs a real turn,
which is ROOT's to decide. The cases' checks now live in `codex_ran_its_tool` and
`claude_ran_its_tool`, which the real cases call. Light tests hold them to v2's two recorded
turns (`records/*-signed-in`): each recorded turn passes. Each fails with what a turn that ran
no tool leaves: its stream without the tool's events (Codex's `command_execution` items,
Claude Code's `tool_use` call), though the answer still names the project's files, or its
kernel record without the `ls` exec in the harness's domain. A sixth light test holds the
follow's end (F3) to v1's record of Codex's run: Codex's own exit ends it, at its exec's PID,
after its children's exits, and a `lost` row ends it too.

### Slice B's runs

On the runner, at `4e53fcdfe` plus this slice (`~/g121-tmp/s2-8-slice-b.log`):

| Check | Result |
|---|---|
| `cargo test -p shards --test harness_bringup` | 6 passed, 12 ignored (the 11 real-VM cases and `common::port_probe`) |
| MT1: Codex's tool check takes any item | `a_codex_turn_that_ran_no_tool_fails_its_case` failed: its panic was not "no ls ran" |
| MT2: Claude's tool check takes any content | `a_claude_turn_that_ran_no_tool_fails_its_case` failed: no panic |
| MT3: Codex's domain check removed | `a_codex_tool_the_kernel_never_saw_fails_its_case` failed: no panic |
| MT4: Claude's domain check removed | `a_claude_tool_the_kernel_never_saw_fails_its_case` failed: no panic |
| MF: a row's event taken whatever its PID (`row`) | `a_follow_ends_at_the_harness_own_exit` failed |
| `bringup-test.sh`, this slice's `bringup.sh` | the follow ended at Codex's own exit (86 rows), its watch ended. The first run exited 1 after its PASS: the test's own exit trap ran `kill` under `set -e` on a watch already ended. Fixed (`\|\| true`) before landing |
| `bringup-test.sh`, main's `bringup.sh` | red: "the harness's process did not end within 300 s", no follow |
| MB: `bringup.sh`'s exit taken whatever its PID | red: the follow ended at child 1741's exit (`code 127`) |
| `cargo fmt -p shards -- --check` | clean |
| `cargo clippy -p shards --all-targets -- -D warnings` | clean (aarch64-apple-darwin) |

The other findings land in the slices that follow, and this record is extended at each.
