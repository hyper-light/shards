# Exact-source validation ledger

Status: signed implementation checkpoint for coordinator review, not main adoption
or P7 completion. Actual original-main peer red and selected candidate green are
retained. No full VM/OOM/mutation/performance/platform claim.

## R09 chronology and frozen prototype

The checked peer-ID draft was written before live baseline reproduction. This
**violated R09's before-code chronology**; the later red run does not erase it.
Coordinator notified immediately when accounting for the chronology. The source
observation of `wrapping_add` was not described as a reproduced defect. The draft
was frozen before original-main receiver evidence and re-evaluated after that
actual symptom. Root's Programs RUN/FD evidence is distinct from peer IDs.

Frozen pre-reproduction source SHA-256:

```text
5dcdb5efd9546353368c2d72d76ae7083e6405c6843eb1821b7c7cc5c365163b  crates/server/src/main.rs
3c178a0ce05f144d5eebddcb26d7ad291432acd67acc09b05ea89989cb182639  crates/server/src/server.rs
af97b7c3d7ed8347b8036f42f3c3779fba52c3d5841ca3c3f03dc45ed733e9dc  crates/server/src/broker.rs
e058101069144333fae8d28552c8a5721ac80cf201c901a62666155079819546  before-case.patch (uncompressed)
```

Prototype `/tmp/native-effect-broker-source-v3.tgz`, 41,523 bytes, SHA-256
`b6eb88436199d2699671fb5fa4ea692f45778a30d7b36cc647e281b188e8a7e6` is preserved.
Subsequent test-order fixes and metadata/lint review do not retroactively make
before-live drafting compliant. No main merge or bug-fixed adoption claim.

## Actual original-main receiver reproduction

`r25`, Fedora8fadc, exact cbd main/server plus **only**
[before-case.patch.gz](before-case.patch.gz), one actual SOCK_SEQPACKET receiver.
Baseline main does not register the new broker module. Exact hashes:

```text
ef51da42ff1780d03548386fad4f2516d9429cc9d8d62f1b0abae4cd38dfdb3d  crates/server/src/main.rs
3c19e9792556a3de93af442b8ca461fabd12e1d9f78e0c9dabe3f7a05dcbd73b  crates/server/src/server.rs + fixture
```

Command, from the admitted private source with private Cargo home/target:

```sh
cargo +1.98.1 test -p shards-server --locked --profile test-release -j1 \
  broker_exhausted_caller_identity_refuses_before_packet -- \
  --exact server::tests::broker_exhausted_caller_identity_refuses_before_packet --nocapture
```

Native case **101**, expected-red driver **0** after checking the actual receiver
line and assertion cause. [Raw baseline](raw/baseline-id.log),
[exit](raw/baseline-id.exit), [source hashes](raw/baseline-source-hashes.log):

```text
BROKER_EXHAUSTED last=0 actual_packet_id=Some(0) caller_reply={"value":{"id":0}}
assertion `left == right` failed: an exhausted identity emitted a packet
```

This is an actual received packet, not a mock/counter observation. Re-evaluation:
checked successor allocation before reservation/send directly prevents the observed
wrapped ID0. No transport replacement, retry, larger bound or warmed failure.
Meaningful single-guard mutation/restored pass is still root-owned and owed.

## Final selected source and scoped gates

Final code SHA-256 (same files as native/lint `r31`):

```text
5dcdb5efd9546353368c2d72d76ae7083e6405c6843eb1821b7c7cc5c365163b  crates/server/src/main.rs
47f187092fbac37ca048ea77f414ad65bc57c02c51b9af87797a55a3063c8255  crates/server/src/server.rs
dd8636601f9210315254ab9c77c9526cfcf2f07ff7d6aef81ce496b14f61eb01  crates/server/src/broker.rs
```

Private exact source archive `overlay-v7.tgz` / local
`/tmp/native-effect-broker-source-v7.tgz`, SHA-256
`3657cbc33420d83083240313ce22139f48ba24c90e8ad0ccb198b9d5626d8310`.
No production edits followed these gates.

| Actual command (`cargo +1.98.1`, private source, `-j1`) | Result | Raw |
| --- | --- | --- |
| `clippy -p shards-server --locked --all-targets --profile test-release -- -D warnings` | 0; no new allow/expect suppression | [clippy](raw/clippy-v7.log) |
| `test -p shards-server --locked --profile test-release broker_ -- --nocapture` | 0; 8 passed, 0 failed/ignored | [targeted](raw/targeted-v7.log.gz) |
| `test -p shards-server --locked --profile test-release only_a_regular_file_of_the_scratch_is_handed -- --nocapture` | 0; 1 passed | [held file](raw/held-file-v7.log.gz) |
| `test -p shards-server --locked --profile test-release a_programs_call_is_its_callers -- --nocapture` | 0; 1 passed | [Program caller](raw/program-caller-v7.log.gz) |

The actual exhaustion receiver now observes `actual_packet_id=None`, retains
`last=18446744073709551615`, and receives a refused/authorized/identity_exhausted
Program response with local effect not_started. Actual shared caller sends
Program ID1, HTTP ID2, real scratch file ID3 and one answer ID9; duplicate,
malformed optional ID and exhausted new ID emit no packet. HTTP/Program tests
parse actual typed metadata and assert its principal/operation/request/local
accepted-byte fields, alongside exact receiver message bodies.

The actual closed-peer send returns failed/executing/not_accepted/peer_io with
its typed raw OS cause, consumes the checked new ID and creates no pending reply
identity. The escaped/max-value encoder-only case validates uncertain/short_send
metadata and bounded output storage; **it is not a native short-send result**.
Required-durable test observes the early typed refusal with a missing file and no
peer bytes; absence of an open syscall is source-order evidence, not a retained
syscall trace. No durable sink exists.

The actual Program helper bootstrap consumes `r.error` and `r.value`, preserving
those existing keys while the extra broker object is returned. The scoped test
calls `call_for_program` directly; no actual QuickJS worker/VM is launched here.
The HTTP test calls actual request parsing/dispatch, not a fresh TLS connection.

## Environment, locks and raw provenance

Implementer `/root/borrowed_connector`, branch `p/native-effect-broker-v1`, base
`cbd39284937d024640c56db6f8d8ef2bd9b0ee24`, policy rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
[Environment](raw/environment.json): Fedora Linux 6.19.10, x86_64, glibc2.43,
Rust1.98.1. All remote actions asked Kiln for 1CPU/1GiB/2GiB. NEW private scope
`/home/adalundhe/native-effect-broker-v1`, private source/Cargo home/target, no root
cache/source access. Scope after r31: **522,962,881 bytes**, below 2GiB; remote
free disk **1,926,716,628,992 bytes**. No active jobs remain in this lane. Source,
logs, archives and targets preserved; no heavy local build or paused WinARM use.

A shallow filtered sparse clone held only actual server/testdir/init JSON and
published patched vendor sources, plus repository metadata. Validation Cargo
workspace members were restricted to server/testdir; exact manifest patch is in
[raw-logs.tgz](raw-logs.tgz). This is **not** a whole-workspace gate. Isolated lock
pruning retains **83** packages, removes **129** unused packages, adds/changes
**zero** versions/source/checksums. Production Cargo.toml/Cargo.lock unchanged.
Original lock SHA `4fc91bf96949709475a6cfadf6bfd32a961e21d181a48368c0e5305e329a1bd1`;
isolated lock SHA `346caed7dc97666905232bcf5258f1dc64d0a02eb530eeba09eb29a945be554c`.

Complete raw log archive SHA-256
`e9fe2ca99d5eec169adbbceb8d609e38dcd4465cba5183558b41d21b3096b9c0`,
7,707 bytes / 21 verified regular files, preserved byte-for-byte. Individual hashes
are [raw-sha256.json](raw-sha256.json). Expanded raw manifest patch omitted from
raw/; its unified-diff context bytes remain inside the archive.

### Earlier failed/unadopted runs retained

- `r22`, [first](raw/targeted-first.log): 101 before compilation; --locked rejected
  isolated workspace pruning. Repository lock untouched.
- `r23`, [second](raw/targeted-second.log): 101 compiling first immutable snapshot;
  test helper shadowing, test closure borrow lifetime, refusal Terminal JSON
  borrow lifetime, unused fmt import. No native test result.
- `r26`, [v3](raw/targeted-v3.log): 101, test inspected ledger before consuming its
  borrowed terminal reply. Fixed only test assertion order; frozen production
  prefix stayed byte-identical (`30362b119b9c4c2fe79f09183a479f384ef66fde1b25295ce1af7dd6b2895404`).
- `r27` native8 passed; `r28` migrated2 passed but [clippy-v4](raw/clippy-v4.log)
  rejected unused future receipt fields/method and collapsible_if.
- `r29` [clippy-v5](raw/clippy-v5.log): 101, unsafe comments sat outside assert
  macro tokens. Moved comments immediately before unsafe blocks in tests.
- `r30` v6 clippy/native8/regressions2 passed, but that draft contained three
  dead-code expect attributes. Coordinator rejected their design, so **v6 is
  unadopted** despite its passing gate. Replaced unused hooks with actual typed
  response presentation; no dead-code attribute remains in the new broker.
- A zero-idle-core Kiln refusal prevented one v3 update/baseline submission from
  starting. Headroom later returned; no requirement was lowered or bypassed.

Local rustfmt, shell syntax, diff whitespace and policy checker pass. Policy
checker explicitly does not replace evidence/coordinator review. The two actual
unified-diff artifacts retain their context spaces compressed, not rewritten.

## Owed work and concrete next component

Root review/select/adopt decision still required, including acknowledged R09
chronology, actual single-guard mutation/restored pass, fault/OOM reservation and
outer HTTP/sandbox-delivery allocations, full cleanup/resource accounting, combined
root Programs source, native worker/TLS/real-VM regressions and seven-target gates.
No pids/cgroups/kernel-budget/audit guarantees are established by this peer slice.

Next component: actual required pre-effect durable audit admission, and checked
held native actor-root/executable authority. The current server has neither that
sink nor arbitrary exec authority. RequiredDurable remains unavailable; broader
resource/generation/revocation/cancel and comparative profiling remain required.
