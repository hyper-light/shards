# Owner keep-alive v1: finding H1, a quiet subscription lost at its patience

An owner's subscription to a managed output channel ended when the harness said nothing
for 100 s: the managed transport held each connection to its patience (QUIC's idle
timeout, `QUIET` for the daemon's endpoints) and sent no keep-alive, so a model's long
turn, a silence of the channel and not of either peer, lost the owner its output
([harness-bringup-v1](../harness-bringup-v1/README.md), finding H1: bracketed live on
adacachy, 75 s of silence kept, 125 s lost at 100–103 s). Each side of the managed
transport now keeps its connection alive at a quarter of its own patience, the derivation
D180's contexts take from the same function.

## Identity

| | |
|---|---|
| Task | H1 of harness-bringup-v1; ROOT's brief of 2026-11-12 |
| Role | implementer; coordinator ROOT (kiln agent 01a148884a4770fdaf0111f3f792a248, session g121-7b) |
| Policy | origin/main bff3015182ad754274f1048ce393ac12cb6cd91f, `docs/development-rules.md` sha256 df2acf47…; re-read after the session's restart, unchanged on main c4ae59ee0 ([POLICY_READ.json](POLICY_READ.json)) |
| Base | origin/p/integrate-engine-20261111 1da9132b7; merged with origin/main before landing |
| Owned paths | `crates/remote/src/managed.rs` (transport and tests), `crates/remote/src/tls.rs` (`keep_alive`, D180's call of it; the crypto provider untouched), `docs/design/architecture.md` (the managed-output paragraph), this directory |
| Runner | adacachy (451397fcc585f7f7cb9aaf00e42064bb): Linux 7.2.9-2-cachyos x86_64, 32 CPUs, load average 41–72 during the runs, rustc 1.98.1 |

## Where the transports were configured, and what changed

- **The managed owner-output transport**: `crates/remote/src/managed.rs` `fn transport`,
  used by both sides: `Endpoint::bind` (the daemon's endpoint, through `tls::owner_server`)
  and `client_config` (an owner, through `tls::client_with`). It set
  `max_idle_timeout(Admitted.patience)` and no keep-alive. The daemon admits every endpoint
  with `patience: shards_remote::tls::QUIET` (`crates/shards/src/daemon/managed.rs:832`).
  Now it also sets `keep_alive_interval(tls::keep_alive(patience))`, 25 s for the daemon's
  endpoints, and refuses a patience under 1 ms (`AdmissionError::Patience`).
- **D180's contexts**: `crates/remote/src/tls.rs` `fn transport(serves)`:
  `max_idle_timeout(QUIET)` and `keep_alive_interval(IDLE / 4)`. Now
  `keep_alive_interval(keep_alive(IDLE))`, the same 2.5 s.
- `tls::keep_alive(bound) = bound / 4`, its derivation in its comment and below.

## Derivation (R02, R03)

Sources: RFC 9000 (rfc-editor.org/rfc/rfc9000.txt, sha256 f88aae47…e22, read 2026-11-12);
hyper-quic at the pinned 233b7fc (quinn-proto 0.11.18, conformed: `crates/hyper-quic`);
focal f1e761f9 `crates/focal-wire/src/transport.rs:31-36, 325-336`; D180 (7fc3c25cf) and
D176 part 6 (65e3f462d).

- **The idle timeout.** "the effective value at an endpoint is computed as the minimum of
  the two advertised values" (§10.1). "An endpoint restarts its idle timer when a packet
  from its peer is received and processed successfully. An endpoint also restarts its idle
  timer when sending an ack-eliciting packet if no other ack-eliciting packets have been
  sent since last receiving and processing a packet." "endpoints MUST increase the idle
  timeout period to be at least three times the current Probe Timeout (PTO). This allows
  for multiple PTOs to expire, and therefore multiple probes to be sent and lost, prior to
  idle timeout." hyper-quic: `negotiate_max_idle_timeout` takes the lesser
  (`connection/mod.rs:5594`); `reset_idle_timeout` floors it at three PTOs (`:2782-2797`)
  and runs on each packet received (`:2742-2744`) and on the first ack-eliciting packet
  sent after one (`packet_builder.rs:261-264`).
- **The keep-alive.** "An endpoint might need to send ack-eliciting packets to avoid an
  idle timeout if it is expecting response data but does not have or is unable to send
  application data. … With this option, an endpoint could send a PING frame (Section 19.2)
  periodically, which will cause the peer to restart its idle timeout period." "Unnecessary
  sending of PING frames could have a detrimental effect on performance." (§10.1.2).
  hyper-quic restarts the keep-alive interval at each packet sent (`packet_builder.rs:255`)
  or received (`connection/mod.rs:2742`), only once established (`:2799-2808`), and sends
  a PING when it runs out (`:1700-1703`). Its documentation: "Only one side of any given
  connection needs keep-alive enabled for the connection to be preserved. Must be set lower
  than the idle_timeout of both peers to be effective." (`config/transport.rs:299-301`).
- **A quarter.** §10.1 sizes an idle timeout to hold three probe periods, for "multiple
  probes to be sent and lost". Held to that rule, a bound `T` takes three keep-alives, at
  `T/4`, `T/2` and `3T/4` after the last packet either way, and no more: the longest
  interval four of which `T` holds (`three_keep_alives_fall_within_every_idle_bound_quic_carries`
  pins it). A live peer is lost only if all three are (with the probes their losses bring),
  or a stall of `3T/4` holds either side up; a dead one is let go between `T` and `T + T/4`
  after it was last heard, as only the first ack-eliciting packet sent since then restarts
  the survivor's timer. focal keeps its connections at a quarter of its idle bound ("a live
  peer is heard from several times in every window however late its scheduler runs it"),
  and D180 has since its first build.
- **Whose patience.** Each side keeps alive at a quarter of its own. The connection takes
  the lesser patience (§10.1); that side's keep-alive is under it, so it holds the
  connection whatever the other side's configuration. Two sides keeping alive add no
  PINGs to one: each packet restarts the other's interval, so the shorter interval alone
  fires (source reading, not measured).
- **The 1 ms floor.** "The maximum idle timeout is a value in milliseconds … Idle timeout is
  disabled when both endpoints omit this transport parameter or specify a value of 0"
  (§18.2), and hyper-quic converts a `Duration` by `as_millis` (`config/transport.rs:821-824`).
  A patience under 1 ms was carried as no idle timeout at all, and a quarter of a patience
  under 4 ns is a zero interval, a PING loop as fast as the host runs; both are refused.
- **D180's input stays `IDLE`.** Its keep-alive was a quarter of its first bound, `IDLE`,
  and D176 part 6 kept it at 2.5 s when connections became held to `QUIET` ("more often
  than the 30 s it finds most middleboxes keep a UDP flow's state for", §10.1.2). It now
  comes from the same function, unchanged. Moving it to `keep_alive(QUIET)` = 25 s would
  cut D180's PINGs tenfold and stay under 30 s, but that is D180's decision, over real
  links: not taken here.

Alternatives, rejected:

| Alternative | Why not |
|---|---|
| A longer patience | No finite patience bounds a model's turn, and a dead owner is held as long. |
| No idle timeout | A dead owner is never let go (mutation m3). |
| A heartbeat record on the stream | QUIC's PING does it (§19.2) without changing managed-output/1's wire. |
| The owner alone keeps alive (§10.1.2's endpoint "expecting response data") | The endpoint would depend on every owner's configuration; owners through `client_config` keep alive anyway, at no extra PINGs. |
| One fixed interval (D180's 2.5 s, or 25 s) | Not tied to the patience: a side whose patience is shorter is lost (mutation m2: 25 s against 10 s), and a longer one pings more than three per bound. |

**The owner's side.** An owner's transport comes from `client_config`, the same
`fn transport`: it keeps alive at a quarter of its own patience with no setting of its own.
An owner as patient as the endpoint or more needs none (the endpoint's holds the
connection: `a_quiet_subscription_outlives_its_endpoints_patience`); a less patient one
holds it by its own (`a_quiet_subscription_outlives_its_owners_patience`). The tests'
observer (`crates/shards/tests/common/managed_capture.rs`, patience = its deadline,
`WITHIN` = 600 s in `harness_bringup.rs`, keep-alive 150 s) is held by the endpoint's 25 s.
It now fails at `client_config` for a deadline under 1 ms away.

**Is `QUIET` derived?** For D180's contexts, yes: RFC 1122 §4.2.3.5's R2 ("at least 100
seconds") and the Wi-Fi moves of D176 part 6. The managed endpoint reuses it as its
patience (`daemon/managed.rs:832`) without a derivation of its own for a loopback owner:
not derived for this use. Left unchanged. With the keep-alive it no longer bounds how long
a harness may stay quiet; it bounds how long a dead owner holds its endpoint, 100 to 125 s
after it was last heard (was 100 s). Meanwhile the endpoint's one connection
(`DriverLimits.connections` 1, `max_incoming(1)`) refuses a restarted owner (`net.rs`
`receive`, source reading).

## Tests and results (R09, R10)

Light tests over loopback, through the production path: `Endpoint::bind` and
`Endpoint::run` on one driver, an owner through `client_config` on another
(`managed.rs` tests: `Owner`, `Pair`). The driver reads the real clock, so a short patience
stands in for `QUIET`: `tls::IDLE` (10 s, the shortest bound shards' transports hold a peer
to, which no loaded host's stall was found to reach), the other side `tls::QUIET`.

| Test | What it holds |
|---|---|
| `a_quiet_subscription_outlives_its_endpoints_patience` | silence `T + keep_alive(T)` (H1's live red bracket, `QUIET + QUIET/4`, at 10 s), then one record: the subscription is still `Ready` and the owner receives the record's frame whole; the endpoint's keep-alive alone |
| `a_quiet_subscription_outlives_its_owners_patience` | the same, the owner's keep-alive alone |
| `a_vanished_owner_is_let_go_within_its_endpoints_patience` | an owner whose driver ends without a word is let go within `T + 2·keep_alive(T)` (the RFC's `T + T/4`, and an interval for the host to run the endpoint's turn) |
| `a_patience_under_a_millisecond_is_refused` | 0, 1 ns and 999 µs refused on either side; 1 ms taken |
| `three_keep_alives_fall_within_every_idle_bound_quic_carries` | the quarter, from 1 ms to 2^62−1 ms |

Runs on adacachy (`records/pre-merge/`; each log has the files' sha256 and ends `exit N`):

| Phase | Source | Result |
|---|---|---|
| red | the base's transports with the new tests (`managed.rs` a75e16ef, `tls.rs` 002da7d3) | 3 failed: both quiet tests ("let go in 12.5s of silence: Pending(…); its owner heard [\"its connection lost: timed out\"]"), the floor at 0 ns; a vanished owner let go after 10.08 s |
| green | e2eda2f0, 03ed6a6c | 23 passed; a vanished owner let go after 10.04 s |
| m1, keep-alive removed | b7011379 | both quiet tests fail, H1's symptom |
| m2, every side at `keep_alive(QUIET)` | 794f5014 | both quiet tests fail: 25 s against a 10 s patience |
| m3, no idle timeout | e4683e23 | the vanished-owner test fails: "still held its endpoint after 15s: Err(Ready(…))" |
| m4, the 1 ms floor removed | 5feeafbb | the floor test fails at 0 ns |
| m5, a half instead of a quarter | `tls.rs` f4a48af3 | the quarter's test fails; the loopback tests pass (they hold liveness, the derivation test the interval) |
| fmt | e2eda2f0 | failed: three statements of the new tests, rustfmt's wrapping; applied as printed (2bc20a5b), re-run passed (`fmt-rerun.log`) |
| clippy `-p shards-remote --all-targets -D warnings` | e2eda2f0 | passed |
| `cargo test -p shards-remote` | e2eda2f0 | 33 unit and 17 loopback passed |
| `scripts/check-development-policy` | the worktree | passed |

A vanished owner was let go after 10.04 to 10.09 s in every run but m3, not the worst
case's 12.5 s: it vanished before acknowledging the endpoint's post-handshake packets, which
were already the endpoint's first ack-eliciting packets since it last heard the owner, so
its keep-alives restarted nothing (§10.1).

The merged revision's runs, through [run.sh](run.sh) on the merged tree, are in
`records/merged/`. `run.sh TREE LOGS` reproduces every phase from a checkout: its red phase
takes the fix back (m1 and m4 together), which leaves the base's `fn transport` code
exactly. The recorded pre-merge runs staged the tree as `git archive 1da9132b7` from an
adacachy clone plus this change's red diff, as kiln's sync of the new worktree timed out
(60 s) on kiln-a094a12d and adacachy; each log records the files' hashes.

## Remaining rules

| Rule | Account |
|---|---|
| R01 | No new panic path in production: `bound / 4` divides a `Duration` by a nonzero constant; `as_millis` is a `u128`. Tests may unwrap. |
| R04 | No new state. The refusal is the existing typed `AdmissionError::Patience`. |
| R05 | The keep-alive is a protocol timer (§10.1.2), not a state probe: a quiet subscribed endpoint's driver wakes once per interval (25 s) as well as at its idle deadline. No cache. The tests wait on the slot's change notifications. |
| R06, R07, R13 | One function and two transport lines; comments carry the derivation. |
| R08, R14 | No performance claim. Per quiet subscribed channel: one PING and one ACK per 25 s on loopback, and a driver wake; not measured (ROOT's). |
| R11, R12 | Shared code, no platform branch. Verified on Linux x86_64 only; macOS arm64, Windows, Linux arm64 and musl unverified. |
| R15 | Implementer: light tests only. Lands on main by the person's rule relayed by ROOT on 2026-11-12 ("Once complete, they should merge their work to main and end."), which replaces R15's coordinator merge for this lane. |
| R16 | `$HOME/owner-keepalive-v1` on adacachy (staged tree and its target directory) is removed after the merged run. |

## Owed to ROOT

- The live H1 pair in `crates/shards/tests/harness_bringup.rs` (ignored):
  `an_owners_subscription_outlives_a_quiet_harness` (red before, expected green now; its
  `#[ignore]` reason still says red) and `an_owners_subscription_outlives_a_silence_within_its_idle_bound`.
- `crates/shards`'s managed tests over the real transport (`daemon/managed.rs`,
  `tests/managed_output.rs`), `scripts/lint` on the seven targets, and the other platforms.
- The keep-alive's cost (R14), and two decisions left open: D180's keep-alive at
  `keep_alive(QUIET)`, and a patience of the managed endpoint's own for a loopback owner.
