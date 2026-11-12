# A log stream's write that meets its receiver's end (D163, PM M527)

CI's unit tests (`cargo test -p shards --bin shards --profile test-release`) failed
`daemon::ship::fluentd::tests::a_part_refused_goes_again_in_its_lines_place` now and then:
the close panicked in `gelf::tests::close_until_ended` (gelf.rs:304) with
`Broken pipe (os error 32)`, where D163 says a receiver's end before the stream's is
followed and the close says `ConnectionAborted`.

| run | commit | target | job log |
| --- | --- | --- | --- |
| 43183804136 | b51253c18 | x86_64-unknown-linux-gnu | `ci-logs/131065942941.txt` |
| 43189239973 | ac87190d3 | x86_64-unknown-linux-gnu | `ci-logs/131084056805.txt` |
| 43250570050 | 7ad28fab1 | aarch64-unknown-linux-gnu | `ci-logs/131294914710.txt` |

Fetched with `gh api --allow-escape-sequences repos/hyper-light/shards/actions/jobs/JOB/logs`,
colours stripped; each file keeps the failing test's lines.

## Identity

- Policy: origin/main 87bf86b31's `docs/development-rules.md`, SHA-256
  `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`; role implementer.
- Owned paths: `crates/shards/src/daemon/ship/stream.rs`, this directory, D163's stream
  bullets in `docs/design/architecture.md`, PM M527.
- Hosts: Adas-MacBook-Pro-2.local (macOS 26 / Darwin 25.4.0, arm64, 18 cores, shared with
  other sessions: load 15-211 during these runs); adacachy2 (CachyOS, Linux 7.3.3,
  x86_64, 32 cores), reached through kiln, every build and run bounded by a transient
  user unit (MemoryMax=16G, CPUQuota=400%, `-j4`).
- Toolchain: rust 1.98.1 (rust-toolchain.toml); unit tests built `SHARDS_HELPERS=skip`
  (the ship tests start no VM).

## Root cause

`Stream` (`crates/shards/src/daemon/ship/stream.rs`), which gelf, fluentd without acks and
syslog send over TCP, TLS and unix sockets, follows its receiver's end of a connection
where `settle` finds it by looking at the socket: what is held stays held, the receiver is
asked (the probe, then the oldest alone), and `send` and `close` say `ConnectionAborted`
(D163). Where a write met that end first (EPIPE, a reset), the stream:

1. said the write's own error, not `ConnectionAborted` (a write on an existing connection,
   and `send`'s own), though it followed the end there;
2. followed nothing of the end on a new connection (`pump`'s writes there);
3. gave the message being written back to its caller unheld, so that a receiver that ends a
   connection once more than its bound has come (Fluent Bit's forward input closes one as
   its buffer is full at `buffer_max_size`: fluent-bit f5a3cd20,
   plugins/in_forward/fw_conn.c `fw_conn_event`) ended it on a message no end could point
   at: never held, never cut, and its run's log stopped there, the shipper's `until_done`
   giving it again for ever.

The flaky test met (1): after the line's first part went alone and was delivered, the
close's `pump` wrote the 3,000-byte part and the third on the same connection; its receiver
ended the connection as it read the long part, and the third's write met that end (on a unix
socket EPIPE at once; over TCP once the first write after the receiver's end drew its
reset): `close` said `Broken pipe`, and `close_until_ended`, which takes only
`ConnectionAborted`, panicked. The shipper takes any error and tries again, and the stream
had followed the end there, so (1) alone lost nothing in production; (2) and (3) can stop a
run's log, as the evidence test shows.

## Fix (production: stream.rs)

- `Stream::write_failed`: a write that fails, but for its timeout, has met its connection's
  end; the connection goes, the end is followed as one `settle` finds (a reset), and the
  failure says `ConnectionAborted` where it ended with messages held. Every write path:
  `send`, `pump` on an existing connection and on a new one (where a connection that ended
  before it carried any of what is held, its TLS handshake refused, says nothing of them, as
  one not made). A write its timeout gave up stays as it was in every path.
- `Held::write`: a message whose write met the end once its connection carried messages
  (over TLS, its handshake done: `Conn::handshaken`) is one of the messages written on it;
  `send` holds it then (no longer its caller's to give, so `Ok`), where the stream follows
  its receiver's ends (not with fluentd's acks, whose driver follows its own).
- Whether any of the message went is not asked: macOS's write(2) that put part of it on
  the connection and then met the end says the error alone (`onesend.py`). A first version
  asked it by the bytes written; the new test failed with it on macOS 50 times of 50 (10
  alone, 40 at 8 at once: `runs/mac-fix-went-50.txt`) and passed on Linux, whose write(2)
  says the part.

main took a fix of the test meanwhile (ee6347ef8, PM M525): its helper, now
`gelf::tests::again_while_ended`, tries a send or a close again after the kernel's words for
an end (EPIPE, ECONNRESET, ENOTCONN) as after `ConnectionAborted`, as the shipper tries
again after any error. That is right of the failure itself, which lost nothing in
production; it does not reach (2) and (3), which are the stream's. This change keeps that
helper as main has it: the stream now says `ConnectionAborted` for an end a write meets, and
the kernel's words remain for the close's last step (`Conn::finish`: its shutdown and reads),
which this does not change. The new test holds the stream itself to `ConnectionAborted`.

## New test

`daemon::ship::stream::tests::a_write_that_meets_its_receivers_end_is_followed_as_one_seen`
forces the write path over TCP, TLS and a unix socket: its receiver (`ends_past_100`) ends
the connection, the rest unread, once more than 100 bytes of a message have come, and the
message is 16 MiB, six times the most a connection here took before its writer waited
(`absorbed.py`), so the receiver's end comes while the stream writes it and the write meets
it. Expected: the message held, each close until it is pointed at saying
`ConnectionAborted`, it pointed at once two connections have ended on it alone, and, put
back as two pieces, every message received.

## Runs

Every command ran from this repository's root (Mac) or `~/ci-fluentd-epipe` (Linux);
`repeat.sh BINARY TEST N P LOG` runs a test N times, P at a time, each alone in its
process. Builds: `cargo test -p shards --bin shards --profile test-release --no-run`,
`SHARDS_HELPERS=skip` (Mac: in this worktree, `-j8`; Linux: `linux-build.sh NAME REV`, the
variant's stream.rs laid over hyper-light/shards at REV, `-j4`). Variants, by stream.rs's
SHA-256: red (main's code, the new test and `evidence-test.rs`) 180724d8…, the fix
61ddaf61…, the mutant eb8e0dcc….

| what | host | command | result | record |
|---|---|---|---|---|
| ends a write meets | both | `python3 -I endmet.py` ×3 | table in PM M527 | `runs/*-endmet.txt` |
| one send(2) of 16 MiB | both | `python3 -I onesend.py` | macOS: EPIPE, none said sent; Linux: the part, then the error | `runs/*-onesend.txt` |
| what a connection takes | both | `python3 -I absorbed.py 5` | Linux TCP 2,720,256, unix 233,152; macOS TCP 548,188-1,465,848, unix 12,288 | `runs/*-absorbed.txt` |
| main's ship tests | Mac 87bf86b31 | `shards daemon::ship` | 29 passed, 1 ignored | `runs/mac-87bf86b31-ship.txt` |
| main's ship tests | Linux fe44bc8ca | the same | 28 passed, 1 ignored | `runs/linux-base.ship.txt` |
| flaky test, main | Mac 87bf86b31, load 86-211 | `repeat.sh … 200 8` | 3 of 200 failed, each `gelf.rs:304:27: Broken pipe (os error 32)` | `runs/mac-87bf86b31-flaky-200.txt` |
| flaky test, main | Linux, load 3-4 | `repeat.sh … 200 8` | 0 of 200 | `runs/linux-base.flaky.txt` |
| flaky test, main | Linux | `repeat.sh … 200 32` | 1 of 200, `Broken pipe (os error 32)` | `runs/linux-base32.flaky.txt` |
| flaky test, main | Linux | `repeat.sh … 1000 32` | 17 of 1,000, each `Broken pipe (os error 32)` | `runs/linux-base1000.flaky.txt` |
| new test, before the fix | Mac | `shards-red --exact NEW` | failed: its send said `Broken pipe (os error 32)` | `runs/mac-red.txt` |
| new test, before the fix | Linux | the same | failed: its send said `Connection reset by peer (os error 104)` | `runs/linux-red.newtest.txt` |
| a stream driven as a shipper drives it, before the fix | both | `--exact evidence_a_write_meeting_its_receivers_end --ignored --nocapture` | 40 tries each over TCP, TLS, unix: never taken, held or pointed at | `runs/mac-red.txt`, `runs/linux-red.evidence.txt` |
| ship tests, the fix | Linux | `shards daemon::ship` | 29 passed, 1 ignored | `runs/linux-fix-ship.txt` |
| ship tests, the fix | Mac | the same | 30 passed, 1 ignored | `runs/mac-fix-ship.txt` |
| flaky test, main and the fix interleaved | Linux, load up to 76 | `linux-validate.sh base fix2` | main 19 of 1,000 failed (each `Broken pipe`); the fix 0 of 1,000 | `runs/linux-ab-flaky.txt` |
| new test, the fix | Linux | `repeat.sh … 200 32` | 0 of 200 | `runs/linux-fix-new-200.txt` |
| flaky test, main and the fix interleaved | Mac, load 59-117 | `mac-validate.sh … 8`, stopped at 312 and 309 runs | 0 and 0 failed | `runs/mac-ab-flaky.txt` |
| new test, the fix | Mac, load 110-129 | `repeat.sh … 200 8` | 0 of 200 | `runs/mac-fix-new-200.txt` |
| mutant | Mac | `shards-mutant --exact NEW` | failed at its close: `tcp: Broken pipe (os error 32)`, `BrokenPipe` for `ConnectionAborted` | `runs/mac-mutant.txt` |
| mutant | Linux | `shards-mutant2 --exact NEW` | failed at its close: `tcp: Connection reset by peer (os error 104)`, `ConnectionReset` for `ConnectionAborted` | `runs/linux-mutant.txt` |
| clippy | Linux | `cargo clippy -p shards --all-targets -j4 -- -D warnings` (`SHARDS_HELPERS=skip`, the fix over fe44bc8ca) | clean | `runs/linux-clippy.txt` |
| format | Mac | `cargo fmt --all --check` | clean | |

The mutant is the fix with one text removed: in `pump`'s write on an existing connection,
`Err(self.write_failed(e))` as `Err(e)` (`mutant.diff`), the path the flaky test met.

## The merged revisions

- 23aeed57c, this branch merged with main 5333c2747, which brought main's ship-test change
  (ee6347ef8): on Linux, main 5333c2747 with this branch's stream.rs (its only compiled
  difference from main; `linux-build.sh merged 5333c2747`, then `linux-merged.sh merged`):
  `daemon::ship` 29 passed, 1 ignored; the flaky test 200 of 200 and the new test 200 of
  200, 32 at once; clippy clean (`runs/linux-merged.txt`).
- Main then moved to ad600dd52 (rego work, and its own PM M526): this record renumbered
  M527, and main merged again. FINAL-ROWS

## Owed and unverified

- Windows: not run here (no Windows host in this task). The new test runs on CI's Windows
  lanes; where a Windows connection holds 16 MiB the test passes through the stream's look
  at the socket rather than its write, which this record has not measured.
- aarch64 Linux (CI 43250570050's lane) and musl: not run here; CI runs them.
- clippy on macOS (the changed code has no platform's own lines), and the Mac A/B past 312
  and 309 runs (stopped for the new test's repeats).
- The coordinator's checks (R14/R15): profiling, benchmarks, real-VM E2E, audits and the
  seven-target lint were not run, as the implementer's scope.
