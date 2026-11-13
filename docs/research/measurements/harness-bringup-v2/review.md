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

### F5. bringup.sh's 300 s deadline, polled every 2 s (R03, R05): fixed

`bringup.sh` (lines 66–82) read `shards read processes` every 2 s for 300 s. It now follows
`shards watch processes` through a FIFO, row by row, until the harness's exit by its exec's
PID, or a `lost` row, or the record's end. Then it ends the follow it started and reads the
record whole.

`bringup-test.sh` holds it to v1's record of Codex's run, with no VM, binary or network. A
stand-in `shards` gives that record through `watch processes`, then keeps the follow open as a
real one does. Its `read processes` gives the record without Codex's exit. The script runs
twice: once with a stand-in `date` that moves 301 s at each call, then with the host's own.

A correction. Slice B also changed `step`'s timing, on the belief that macOS's `date` prints
`%N` as a literal `N`, and its commit message (`da5c744b8`) says so. On the runner, macOS
26.2's `/bin/date +%s%N` gives nanoseconds (`1794557386924763000`), so that was never
reproduced (R09). This slice puts `step` back as it was. Older macOS releases were not
checked.

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
| `bringup-test.sh`, `ecd19d38c`'s `bringup.sh` | red: "the harness's process did not end within 300 s", no follow |
| MB: `bringup.sh`'s exit taken whatever its PID | red: the follow ended at child 1741's exit (`code 127`) |
| `cargo fmt -p shards -- --check` | clean |
| `cargo clippy -p shards --all-targets -- -D warnings` | clean (aarch64-apple-darwin) |

Merged with main twice before landing (`9b9da26c7`, then `5b91d43de`), the same checks ran on
each merged tree (`~/g121-tmp/s2-8-slice-b-merged.log`, `-merged2.log`): 6 passed, 12 ignored;
`bringup-test.sh` passed and exited 0; fmt and clippy clean; `credentials_test.py` 14 passed.

### Slice C's runs

`bringup-test.sh` now runs the script twice, with the stand-in clock and with the host's own
`date`. Slice B's clock change is taken back (F5). At `29dde4f4c` plus this slice
(`~/g121-tmp/s2-8-slice-c.log`):

| Check | Result |
|---|---|
| this slice's `bringup.sh` | both runs: the follow ended at Codex's own exit (86 rows), its watch ended |
| `ecd19d38c`'s `bringup.sh` | red, both runs: no follow; under the stand-in clock, "the harness's process did not end within 300 s" |
| MB: the harness's exit taken whatever its PID | red, both runs: the follow ended at child 1741's exit |
| this slice's again | both runs passed |

### Owed, with their evidence (not fixed here)

These need what this step's one runner lacks (gpg, a Linux host, ~700 MB of the publishers'
downloads, a real VM), or a decision that isn't this step's. The child brief's rule is to
record such a finding with its evidence and its exact command, not to fix it blind (R09).

- **O1. fetch.sh trusts the cosign the release serves (R10 for its verification).** Lines
  65–72 fetch `cosign-linux-amd64` and `cosign_checksums.txt` from the same release and check
  one against the other, so the verifier's identity rests on that release alone. The Codex
  archives and bundles, by contrast, are pinned in the script (lines 37–41). The value to pin
  is on record: v1's `fetch/fetched.json` has `424e2c97d0b45fd7a6b608a87f090f35633f4c23ece5f092d48bba11aaf4bbfa`,
  which GitHub's own digest of the asset matches (`gh api
  repos/sigstore/cosign/releases/tags/v3.1.4`, read 2026-11-13; it also lists
  `cosign-linux-arm64` `ade83b7a…dad2` and `cosign-darwin-arm64` `9f9b8cd5…d3a4`). The four Codex
  pins match GitHub's digests too.
- **O2. fetch.sh takes a signature GnuPG doesn't call good (R10).** Lines 112–121 accept any
  `VALIDSIG` whose primary-key fingerprint is Anthropic's. GnuPG writes `VALIDSIG` whenever the
  signature verifies (`!rc`), beside `EXPSIG`, `EXPKEYSIG` or `REVKEYSIG` as well as `GOODSIG`
  (gnupg `g10/mainproc.c` at master `ab9c746e9`, lines 2363–2372 and 2581–2594). So a manifest
  signed by an expired or revoked key, or carrying an expired signature, passes; Anthropic's
  documented check reads "Good signature". The cure is to require `GOODSIG` beside the
  fingerprinted `VALIDSIG`. Live reproduction (gpg on a Linux host): a throwaway key signs a
  file, `gpg --gen-revoke` revokes it and the revocation is imported, then `gpg --status-fd 1
  --verify` shows `REVKEYSIG` and `VALIDSIG`, which lines 117–121's awk accepts.
- **O3. fetch.sh fetches x86_64 alone (R11).** The ignored cases need the publisher's build for
  the guest's architecture: arm64 on macOS (HVF) and on Linux arm64 (L1). Both publishers ship
  them: Codex `rust-v0.167.2`'s `codex-aarch64-unknown-linux-musl.tar.gz`
  (`ed81fa2c…4add`, its bundle `cd67e2bc…88b2`) and `codex-code-mode-host-aarch64-…` (`3c8843f1…82ce`,
  bundle `95bef991…d344`), from GitHub's digests; Claude Code 2.1.322's `linux-arm64-musl`
  (`b8a07ebb…c40c`), in the signed manifest v1 committed (`fetch/manifest.json`). fetch.sh
  also runs on Linux alone (`sha256sum`, `stat -c`, `cosign-linux-amd64`): until it takes an
  architecture, the arm64 builds are fetched and checked by hand on a Linux host, and copied.
- **O4. signed-in.sh runs on Linux alone (R11, R12).** Its private copy goes on the user's
  tmpfs (`$XDG_RUNTIME_DIR`, else `/run/user/$UID`, line 42) and it names the file system with
  GNU `stat -f -c %T` (line 45). On macOS there is no such directory, so `mktemp` fails and the
  script stops before copying anything. v2's R12 row said so; a store for macOS (and Windows)
  is the person's choice with G18's.
- **O5. settle's last check and its write are two steps.** Between reading the original's
  hash (`credentials.py` 220–227) and `os.replace` (in `write_back`), a host process that
  refreshes the same login would be overwritten. The window is the time between those two
  lines; nothing coordinates a harness's own writes to its login file. Not reproduced. Like
  v2's S2, it stays with G18's refresh contract (the person's).
- **O6. The run-time volume probe expects Unix modes on every host.**
  `a_volume_given_at_run_time_is_the_harness_own` asserts `drwx------` and `-rw-------`, and
  `set_mode` does nothing off Unix. What a WHP run shows a harness of a host directory has
  not been measured.
- **O7. Goal 58's call C.** It removes `tls::QUIET`'s fixed window from the managed transport.
  H1's two cases take their silences from `tls::QUIET` (`QUIET ± QUIET/4`). When call C
  lands, goal 58 restates them, and it carries the observers' call sites (F2) if
  `Observer::start` changes.

Also observed, and left as recorded: the signed-in Codex example runs
`--dangerously-bypass-approvals-and-sandbox` (Codex's own sandbox off, "the microVM and its
grants are the boundary under test", v2's README), with its login on a writable volume and
the project read-only.

## Default deny

All four examples grant the harness alone (`FOR <harness>`, `--harnesses=<harness>`), TCP 443
only, outward only (`EXPOSE … AS egress`, `NETWORK --egress=443/tcp`), and names through the
microVM's resolver (`--dns`). What none names is to whom: any host on 443, any name (v1's H2).
D236 (built, its real-VM validation owed to ROOT) gives `--egress=HOST:PORT` for that, and
moves the bring-up's examples to destinations once it is run. That list now holds all four
(the two `-signed-in` examples added), with their hosts to come from that run's resolver
answers. The records' host mentions (api.openai.com, claude.ai, platform.openai.com,
chatgpt.com, code.claude.com, downloads.claude.ai) include help-text URLs, so they don't
show where the harnesses connect.

## R01–R16

| Rule | Account |
|---|---|
| R01 | No production code in this lane. The test may panic. `no_clock` panics with the platform's name where an `Instant` can't hold the far instant: every supported platform's can (std's `Instant` is a `Timespec` on Unix and a `Duration` on Windows). |
| R02 | No architecture decided. Sources: D215 (the no-clock idiom), D123 (`watch processes`), RFC 9000 §10.1, busybox `edca770d11`, Alpine's package index, GnuPG's `mainproc.c`, GitHub's release digests, v1's records. |
| R03 | F1 to F5 removed every chosen number in the lane's test and scripts: `WITHIN`, the 200 ms and 2 s polls, the 300 s deadline, `sleep 600`, the 16/24/8 of the leak check. What stays is derived: the leak run (one longer than a provider shows of a key, from v1's record), `NO_CLOCK` (u64's bound, D215's idiom), H1's silences (`QUIET ± QUIET/4`, the transport's own; O7). |
| R04 | No state machine added. The follow ends at one of three named facts; settle's outcomes are named in its output (unchanged, format only, refresh written back, conflict). |
| R05 | Waits: the 200 ms and 2 s polls of the record became a follow of its change stream (F3, F5); every other wait ends at its fact (F2). No cache. Abstractions: two check functions serve the real cases and the light tests; `follow` and `followed_far_enough` serve one caller each, the second also the light test. |
| R06, R07, R13 | Small functions; comments carry sources and derivations. H1's stale "red" text corrected (F6). |
| R08, R14 | No performance claim. |
| R09 | F1 reproduced red by the new self-test against main's script; F5 by the stand-in test against `ecd19d38c`'s `bringup.sh` (its 300 s deadline). F2 to F4 remove literals, so their red is the literal at `ecd19d38c`, not a run. O1 to O6 were not reproduced, for the reasons given. A clock cure slice B made without reproducing it is taken back (F5's correction). |
| R10 | Each fix red, green, and red with its guard removed, each one focused test (F1: three; F3/F7: five; F5: one). The harnesses are committed (`credentials_test.py`, `bringup-test.sh`, the light tests). |
| R11 | Light checks on macOS arm64 only (`bringup.sh` under its stand-in there, with the host's `date` too). Linux and Windows clippy of the test target, and the cases on every backend, are owed below. |
| R12 | Nothing platform-specific added. |
| R15 | Implementer: light tests on one runner; landed fast-forward as ROOT directs. |
| R16 | The runner's `~/g121-tmp/s2-8-src`, `-py`, `-target` and `-logs` are removed at the step's end. |

## What ran where

All on adas-mac-studio-3.local (macOS 26.2, arm64), through the step's carrier:
`python3 -I …/credentials_test.py`; `cargo test -p shards --test harness_bringup`;
`bash …/bringup-test.sh`; `cargo fmt -p shards -- --check`; `cargo clippy -p shards
--all-targets -- -D warnings`. Results are in F1 and slice B's tables. Not run: anything on
Linux or Windows, any real VM, scripts/lint, scripts/check-development-policy, and any vendor
CLI.

## Landed

| Commit | What |
|---|---|
| `037fa7322`, merge `4e53fcdfe` | F1: the leak check, `credentials_test.py`, this record begun |
| `da5c744b8`, merges `fe816ed63`, `29dde4f4c` | F2 to F7: the test and `bringup.sh` without a clock, the follow, `sleep infinity`, H1's text, R10's light tests, `bringup-test.sh` |
| this commit | this record's rest; D236's list of examples and its pointer here; v1's and v2's READMEs pointed here; `bringup.sh`'s `step` as it was (F5's correction); `bringup-test.sh` runs the script with the host's `date` too |

## Owed to ROOT

Each command runs from a checkout at or after this record's landing.

- **The lane's real-VM cases** (`crates/shards/tests/harness_bringup.rs`, all ignored), on
  every backend: KVM x86_64 (Linux), HVF arm64 (macOS), KVM arm64 through L1 (an M3 or later
  Mac), WHP x86_64 (jupiter). WHP arm64 is unverified (no host). With the publishers'
  binaries for the guest's architecture (O3) in `OUTDIR`:

      SHARDS_HARNESS_CODEX=OUTDIR/codex SHARDS_HARNESS_CLAUDE=OUTDIR/claude \
      SHARDS_HARNESS_RECORDS=/abs/new/dir \
      cargo test --profile test-release -p shards --test harness_bringup -- --ignored --test-threads=1

  The probes alone (no publisher's binary, no vendor CLI):

      cargo test --profile test-release -p shards --test harness_bringup -- --ignored --exact \
        a_volume_given_at_run_time_is_the_harness_own \
        an_owners_subscription_outlives_a_quiet_harness \
        an_owners_subscription_outlives_a_silence_within_its_idle_bound --test-threads=1

  Every Codex and Claude Code case starts its CLI with a prompt: without a credential, or with
  the placeholder, the provider (or Claude Code itself) refuses it. Whether to run them is
  ROOT's, as are the signed-in turns (`a_real_codex_signed_in_runs_a_tool_and_answers`,
  `a_real_claude_code_signed_in_answers`), which run through
  `docs/research/measurements/harness-bringup-v2/signed-in.sh codex|claude BIN_ROOT WORK` on the
  Linux host that holds the sign-in (O4).
- **A long turn** past the owner endpoint's patience: the H1 pair above, and a real harness
  silent for longer; now that the test keeps no clock (F2, F4), nothing but the run's facts
  bound it.
- **The user path** on each host: `docs/research/measurements/harness-bringup-v1/bringup.sh
  target/test-release/shards docs/examples/harness-codex OUTDIR/codex WORKDIR`, and `…-claude`.
- **The light gates off this runner:** the test target's clippy on Linux x86_64 (`cargo clippy -p
  shards --all-targets --target x86_64-unknown-linux-gnu -- -D warnings`, with scripts/lint's zig
  CC) and Windows x86_64 (`cargo xwin clippy -p shards --all-targets --target
  x86_64-pc-windows-msvc -- -D warnings`); `scripts/check-development-policy`; `scripts/lint`.
  The runner has no zig or cargo-xwin. CI on main runs the matrix.
- **O1 to O3's cures in fetch.sh**, with a Linux run that fetches and verifies both
  architectures, and O2's reproduction first.
