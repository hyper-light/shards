# Network review fixes, 2026-11-13

Role: implementer, area network, ROOT coordinator. Base
`9efde72524dd626f3ff84e6b9891852d7c810a9b`, branch `p/fix-network-v1`.
Read full fix/common briefs and origin/main's policies before edits.
POLICY_READ SHA-256 receipts:

| Policy | SHA-256 |
| --- | --- |
| AGENTS.md | f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41 |
| docs/development-rules.md | df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3 |
| docs/development-evidence.md | 61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63 |
| CLAUDE.md | 588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010 |

Scope/order: network-01, 04, 02, 03, 05–11, then clearly derived pieces of 12.
No subagents, local builds, E2E, VM runs, benchmarking or cross-target fleet checks.
The review report retains its original review-time assertions; subsequent statuses
record implementation outcomes separately.

## network-01: received segments must overlap the receive window

The actual `Conn::on_segment` plus its connected host socket is the agreed regression
seam. Red commit `a6ca2403ebb4f79ce8acb8b11aebb8f13ab0d452`, fix commit
`50716d9465c42d9cbabbdd8e725134ba6f80bffe`. The direct deterministic red reproduced
the reported state transition; no speculative alternate-cause investigation or
temporary instrumentation was needed for this small protocol check.

Source: [RFC 9293 §3.10.7.4](https://www.rfc-editor.org/rfc/rfc9293.html#section-3.10.7.4),
table 6 and its fifth/eighth checks. The receive sequence must overlap the advertised
window before ACK/FIN processing; established segments require ACK. A FIN retains
its original position after duplicate bytes are trimmed. Existing exact-sequence
RST/challenge handling remains separate. Window positions use wrapping TCP sequence
arithmetic; zero-window/zero-length ACKs retain their protocol allowance. Length
conversion/addition is checked, and subtraction is guarded by nonzero length.

No chosen timing, capacity, cache, new state representation or new public interface
is introduced. The focused addition uses the existing connection state owner and
socket lifecycle. Existing numeric TCP tuning is network-12, not changed here.

Validation: `adas-mac-studio.local`, Darwin arm64, Rust 1.98.1, one cargo at a time,
`CARGO_INCREMENTAL=0`, jobs 4, debug info disabled, own cold target at
`~/g121-tmp/network-fix-v1/target`. Carrier contains a git bundle, not the big local
worktree; remote source is an independent GitHub clone. Kiln runs:

- r4: carrier setup failure (bundle advertised HEAD rather than the named branch);
  no tests ran. Corrected the bundle ref; not counted as a red reproduction.
- r5: `cargo test -p shards-net stale_fins_do_not_end_the_host_stream` — one test
  failed with `FIN 0 flags 1 was accepted`, exit 101. [Retained output](red01.log).
- r6: `cargo test -p shards-net tcp::tests` — 20 passed, zero failed;
  `cargo clippy -p shards-net --all-targets -- -D warnings` — exit 0.
  Includes stale FINs with/without ACK, valid FIN after duplicate payload, actual
  host-stream continuity, existing retransmission/window/reset/injector tests.
  [Clippy completion](green01-clippy.log).

Merged main `b8da5eca33a57c7b6231725135b7adc8c29ea23f` into
`aaa4f87b4ca360e6675c97948dc35edc602ff687`; r7 rechecked source
`0f5f80816` with 20 TCP tests and clippy passing ([output](merged01.log)). The first
ff push was rejected because main advanced; this is not a landing. The next merged
source is rechecked before retrying. Signing receipts require exactly one gpgsig
on each commit/merge.

### Policy/evidence accounting for network-01

| Rules | Evidence / remaining gap |
| --- | --- |
| R01/R04 | Checked length conversion/addition, guarded subtraction, sequence-space wrapping; existing connection states and refusal/challenge behavior. No production assertion or panic added. |
| R02/R03 | RFC table 6 and ACK/FIN processing define the algorithm; no picked bound/wait/floor introduced. |
| R05/R06/R07/R13 | One connection owns the check; no cache/wait change; small explicit predicate and early refusal, comments cite the protocol obligation. |
| R08/R14 | No performance claim or measurements; profiling and comparative costs remain ROOT's. |
| R09/R10 | Real unit-test red/green at the existing socket seam, revisions/commands/output retained. Heavy live campaigns not run. |
| R11/R12 | Shared protocol code, focused macOS arm64 result only; Linux glibc/musl, Windows amd64/arm64 and real KVM/HVF/WHP validation owed to ROOT/CI. |
| R15/R16 | Exact scoped branch and remote owned source/target; signed commits; no local cargo or new agents. Remote target cleanup after landing, recorded below. |

ROOT owes real-VM known-tuple FIN injection with both blind sequence values, packet
capture proving no premature upstream EOF, and normal close/retransmission on
KVM/HVF/WHP. Existing relevant baseline: `cargo test -p shards --test boot net`;
add the adversarial FIN packet fixture at this socket/VM boundary before claiming
full trust-domain isolation coverage. No such VM campaign was run here.
