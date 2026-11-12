# whp-early-stop: WHP microVMs that "stopped before the command ended"

The record of [PM M530](../../platform-measurements.md) and of D185's "Whose end a run's client
heard" ([architecture.md](../../../design/architecture.md)): two runs of ROOT's engine_ring tests
on jupiter (WHP) ended with status 125, "the command's microVM stopped before the command
ended", their daemons' logs silent. kiln's between-turns sweep had ended their daemons, and
the microVMs with them; the client named the microVM. The fix makes the client say which end
it heard.

- **Identity.** Branch `p/whp-early-stop-v1` from origin/main a7dbf1446 (fast-forwarded to
  3103d2eab, then c2c4547c1, before any change): 037fe95d4 (the fix and its tests), 3708533ea
  (origin/main fc71859bc merged in), 31f6dd5d9 (its comments name M530, main's M528 being
  another's), and the commits adding this record. Role: assigned adversarial (debugging)
  agent under R15 for ROOT (01a148884a4770fdaf0111f3f792a248); receipt
  [POLICY_READ.json](POLICY_READ.json) (development-rules.md sha256 df2acf47…, re-read after
  ROOT's restart, unchanged). Merged to main as the person's instruction of 2026-11-12 says
  (POLICY_READ.json `superseding_instruction`).
- **Hosts.** jupiter (Windows 11 Pro 10.0.26200, AMD64, 32 CPUs, WHP): the reproduction, the
  live red/green and Windows' unit tests; adacachy (CachyOS, Linux 7.2.9, x86_64): Linux's
  unit tests and clippy; adas-mac-studio.local-2 (macOS, arm64): macOS's unit tests and
  `scripts/lint` for every supported target. rustc 1.98.1 (rust-toolchain.toml). No build on
  the coordinating Mac.
- **Owned paths.** crates/shards/src/daemon/slots.rs (`pass_on`), crates/shards/src/cli/client.rs
  (`ran_on`, its test), crates/shards/src/daemon.rs (the relay's tests),
  crates/ipc/src/lib.rs (`kind::UNANSWERED`), D185's bullet, M530, this directory.

## Logs

| Log | What it shows |
|---|---|
| [r1-kiln-sweep.txt](logs/r1-kiln-sweep.txt) | The cause: jupiter's kiln.log ending the failing tests' daemons (48108, 50456), their VMs (47156, 36004) and network processes, "now that they're between turns", and kiln's source for it. |
| [r2-the-symptom.txt](logs/r2-the-symptom.txt) | The symptom as the tests said it (the tester's transcript): status 125 after 1.302 s and 4.858 s, each client having printed its microVM's ID, daemon logs silent. |
| [r3-negative-controls.txt](logs/r3-negative-controls.txt) | Without the sweep, nothing stopped early: 628 runs (plain, detached, fresh homes, 1 to 8 at once) and 3 engine_ring rounds. |
| [r4-kill-red-green.txt](logs/r4-kill-red-green.txt) | R09's live example: a daemon ended mid-run with `taskkill /F` gives the exact symptom (6 of 6 on main), its microVM's end the same words (4 of 4); with the fix the daemon's end is said as the daemon's (6 of 6) and the VM's as the microVM's (3 of 3); the diagnostic build's view of each end. |
| [r5-unit-linux.txt](logs/r5-unit-linux.txt) | Linux: the fix's tests green; each guard removed, red for its reason; the binary's unit suite in test-release; clippy. |
| [r5-unit-windows.txt](logs/r5-unit-windows.txt) | The same on Windows (jupiter, 31f6dd5d9): green, both guards red, the suite 709 of 709. |
| [r5-unit-macos.txt](logs/r5-unit-macos.txt) | The same on macOS (adas-mac-studio.local-2, 5564278e3): green, both guards red, the suite 730 of 730. |
| [r6-lint.txt](logs/r6-lint.txt) | `scripts/lint` on all seven supported targets: clean but for another change's test on both Windows targets (fixed on main since), where this change's crates are clean alone. |

## Harness

- `harness/repro.sh`, `harness/fresh.sh`: repeated plain runs in a reused home, and a fresh
  home's first run, as engine_ring's were (r3).
- `harness/e2e.sh`: engine_ring as the tester ran it (r3).
- `harness/kill.sh`: a run's daemon, or its VM, ended mid-run (r4).
- `harness/build.sh`: a test-release build on jupiter from a clone over HTTPS, outside any
  synced copy.
- `harness/unit.sh`: the fix's tests, each guard removed in turn, the unit suite (r5);
  `harness/lint.sh`: `scripts/lint` from a Mac, zig installed by `scripts/install-zig`, and
  `harness/lint2.sh`: the targets it had yet to reach, and this change's crates on Windows (r6).
- `harness/diag.patch`: the diagnostic build's lines (not merged): which side of a slot ended.
- `harness/wer.ps1`: Windows Error Reporting's records of a crashed process (none of shards'
  in the 3 days before); `harness/tx.js`: a Claude Code transcript's tool calls and results.

## The rules (R01–R16)

- **R01 (no panics).** The changed paths return or write; the relay's word is sent with its
  error ignored, as every answer there is (`let _ =`), the client reached or not. Tests alone
  unwrap.
- **R02 (research).** The cause read from its primary records (kiln.log, the tester's
  transcript) and kiln's own source (origin/main ed0c9d3bd: harness/host.rs, survivors.rs,
  os/contain_windows.rs; sysinfo 0.39.6's Windows `kill_with`). Alternatives for telling the
  ends apart: the client asking whether its daemon still lives (`daemon_lives`) races its
  daemon's end (a process's handles close before it is signalled; a killed Unix daemon is a
  zombie until reaped), and a wait for it needs a bound picked; the daemon's own word, sent
  before its side's end, needs neither, so it was chosen.
- **R03 (constants).** `kind::UNANSWERED = 91`, the next free message kind
  (`every_message_kind_has_its_own_number`); no other literal.
- **R04 (state and typed errors).** The relay's end is one of two, the daemon's side or the
  client's (`ended_here`), with whether the last word went (`answered`); the end is typed on
  the wire (`UNANSWERED`), matched by the client, which renders the text.
- **R05 (abstractions, waits, caches).** No new interface, wait or cache: one more message at
  a relay's end; the client's read loop unchanged.
- **R06, R07, R13.** Two small branches in each of two functions; comments say what the code
  cannot (why a wordless end is the daemon's).
- **R08, R14 (performance).** One message of 5 bytes at the end of an unanswered session
  only; nothing on any answered path. Not measured: the coordinator's.
- **R09 (live first).** Reproduced before the fix: r4 (red).
- **R10 (durable evidence).** This directory, M530 and D185; red and mutants in r4 and r5.
- **R11, R12 (platforms).** The change is the slots' own, shared by every OS; its unit tests
  green, and red with each guard removed, on Linux, macOS and Windows, and the binary's suite
  green on each (r5); clippy clean on all seven targets for its crates (r6); the live example
  on WHP alone (r4): the sweep's own end of a daemon on Linux and macOS is unverified
  (SIGTERM there, which the daemon takes no handler for).
- **R15 (roles).** A bounded campaign on jupiter in ~/g121-whp-stop-* dirs; builds there and on
  the runners above, none on the coordinating Mac; no agents started.
- **R16 (space).** jupiter's C: at 95% used: two cargo `target` directories untouched for two
  days in ~/.cache (kiln-agent-steering's and kiln-skills-check's, 58 GB) removed, their
  evidence files kept; this campaign's dirs removed when done.

## Owed

- ROOT's real-VM suites with the fix (daemon.rs, containers.rs, engine_ring) on each host.
- The sweep's own end of a daemon on Linux and macOS, live.
- The dev profile's stack overflow in `build::policy::tests::the_deepest_policy_runs_on_the_policy_thread`
  ('policy' thread; Linux SIGABRT, Windows STATUS_STACK_OVERFLOW; test-release passes):
  the Rego owner's, as ROOT assigned it; not this change's code.
