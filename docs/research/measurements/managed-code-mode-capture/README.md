# Legacy code-mode E2E migration to private canonical capture

The migrated ordinary E2E passed in actual KVM r86. [Results](RESULTS.md) retain
all original outcomes, source/binary associations,112-member raw archive and
remaining limits. No production change or restored line rewriting.

Role: implementer and assigned adversarial tester `/root/terminal_order_fixture`.
Source `0219ab2eb2466ee3e9b08064434dc9efd70053d5`; loaded policy from that source
and canonical origin/main `cbd39284937d024640c56db6f8d8ef2bd9b0ee24` (identical
policy bytes), rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Full AGENTS/R01–R16/evidence/CLAUDE/plan/current handoff read before edits.
Root's current task overrides stale job listings in the handoff.

Root retained an actual3b legacy failure in
`managed-program-admission/native/kvm-legacy-red`: readiness3/3/3/0 passed,
but the first expected `[agent a]` line was absent from main stderr. The root's
new metadata closure may carry those raw files; this0219 checkout contains only
the admission README/allocation source, so the raw directory is not present here.
The received coordinator's actual record is the baseline, not a new local
reproduction or proof that its unobserved tool outputs passed.
At closing, canonical main `33df95b08ad8f95bddeaa1c48acf0c64386e0eac`
contains that raw record. Its identity/exit/log were read through Git and verified:
actual source3b, exit101, first missing prefixed line; log SHA256
`72805b3e49b1b12cd829ac9e2810df5ee3344a77d632f2a0be80f922cc9e512c`.
Current AGENTS/rules/evidence/CLAUDE are unchanged from loaded0219, rules hash
stilldf2 above; current plan status/G22 updates were loaded. The owned task and
constraints are unchanged.

## Existing seam and exact scope

Move Event/Ask/private Client+Handler/Observer and their existing
Metadata/fingerprint/scope implementation into test-only
`crates/shards/tests/common/managed_capture.rs`. Both `managed_output.rs` and
the named legacy function in `build.rs` load that one source. No duplicate
transport/parser, production interface, second endpoint or alternate state owner.

Only the necessary types, Metadata fields/methods, Observer scope/sender/events
and called methods become `pub(super)`. Client buffers/config/partial writes,
Observer completion receiver/thread, authentication, signatures, deadlines,
credit and framing bodies are unchanged. The module's dead_code allowance is
test-only: separate integration binaries use different existing operations.
Parent Fixture, payloads, auth/member/terminal assertions and child tests remain
byte-identical; only their moved span/imports change. `move-check.json` verifies
the moved bodies after visibility/final-newline normalization, the complete parent
suffix and all `build.rs` bytes outside the named function.

The legacy test remains an ordinary `#[test]`. All three agents, programs,
CONNECT grant, positive-send destination witness, denied-send result, unsupported
globals, TypeError/SyntaxError/RangeError, timeout, stranger/own-session cancellation,
abandonment and original kernel3/3/3/0 conditions remain. The expectation strings'
bracketed labels select actual authenticated member output; actual bytes are never
prefixed or rewritten. “Stranger” is a different session of the same principal.

Detached start publishes actual protected metadata before six owner subscriptions
begin. The actual ABI Record/StreamState validates member, channel and offset;
controls ACK each wire record through the moved pending/partial-write path.
The controlled `confined net=` footer is an output event cut, followed by one real
kernel process snapshot. No old periodic run_watching loop, retry, sleep, warmup or
cache workaround. The reused guest `srv-receive 30` still has its existing50ms
receive loop, and cancellation/abandonment their existing300ms helper delays;
these are disclosed unchanged helpers, not claimed readiness barriers.

The main retains its original `await` command. The same SIGUSR1 caught-mask check
precedes its owner signal, actual wait status0 and explicit run removal. The
replacement closes the daemon and joins all six observers on success or panic.
One original120s TIMEOUT covers setup/work phases; cleanup gets at most15s,
capped at the original case+15s. Trace retention is optional for ordinary suites
via SHARDS_CODE_MODE_CAPTURE_RECORDS. Controlled payloads are bounded per channel
by the existing run protocol MAX_PAYLOAD, not a new production policy.

Canonical bytes authenticate tenant/channel, not a semantic tool execution or
per-PID writer. Controlled-client RPC reports plus the real recipient/process
witnesses remain different evidence. Empty stderr is only an Open receipt cut,
not End/future-zero. A point zero-worker snapshot and successful owner stop do
not establish every kernel charge, thread history or wait status returned.

## Primary contracts and alternatives

- [Rust Reference modules](https://doc.rust-lang.org/reference/items/modules.html):
  block modules and file paths permit the shared module inclusion inside only
  the named function, avoiding unrelated build.rs/common/mod registration edits.
- [Rust Reference visibility](https://doc.rust-lang.org/reference/visibility-and-privacy.html):
  parent-restricted visibility keeps this helper within its integration test crate.
- [Rust Receiver](https://doc.rust-lang.org/std/sync/mpsc/struct.Receiver.html):
  actual completion/record notifications with bounded recv_timeout; no state probes
  substituted for a result. Existing Observer join logic is moved without change.
- Source0219 `common::run_watching`, `testguest::await_process/confined`, canonical
  ABI and owner-output fixture: original signal contract and event/state behavior.
  A Rust1.98.1 metadata-only block-module/visibility probe passed; its tiny inert
  source is language evidence, not a network/VM/producer success.

Alternatives rejected: copy Client/Observer or Metadata; alter Observer signatures
and Fixture bodies; reintroduce product line rewriting; skip/delete assertions;
poll/retry for output; broaden test registration outside the owned function.
Root explicitly approved the parser move/visibility and event+snapshot scope.
This is a shared test caller migration, not a production architecture change.

## Build/caller and status

New owned Fedora8fadc scope `~/native-code-mode-capture-0219-v1` contains source,
target, Cargo home, Zig global/local, XDG/Shards caches and evidence. Root protected
cache/targets, old3ead/7bca artifacts and paused WinARM remain untouched. No host
privilege/trust/crypto change or local heavy build/new full worktree.
Fresh admission2CPU/4GiB/24GiB; normal two-consumer build<=3600s/3700s outer.
Both integration binaries are compiled; then scoped Clippy, metadata parser's pure
check and actual static guest/kernel preparation precede one fresh actual legacy
case. Native caller outer<=240s plus one15s reap bound; native test itself uses
the stricter original120s work+15s cleanup. Admission is not a hard cgroup claim.
Root owns combined7-target/heavy validation, audits, benchmarks and main merging.

Normal full carried-helper/init/server build r83 **passed0**, compiling both
integration consumers in10m02s. Shared source SHA256
`431fe6da8999d0cd13822a4b59e52312c1ae07ffd7835a0428b07cf78402367d`,
parent `be5fa582b7d691a88a127e179412cba1e7609101f5bd34e2baeb156ff55881a1`,
legacy `343814e9bb65005fbc61a9df2acf2ecc24b5c84987b38416746ba73371ba0ee7`.
Local format/parser/shell/policy checks and the recorded structural checks passed.
The first signed compiled checkpoint still has **native validation pending**.
Fresh r85 is active for the scoped lint/pure check/static artifacts under the
remaining original3600s compilation budget. Full raw logs remain owned on Fedora;
submitted commands/source payloads are retained here. No actual green claimed yet.

R01 test-only assertions, production unchanged; no universal panic-freedom proof.
R02/R03 primary/alternative contracts and explicit inherited/test bounds above.
R04 real ABI/driver plus explicit case/cleanup outcomes; unknowns retained.
R05 one existing observer/parser; no new cache/state owner; frozen old helper waits
disclosed. R06/R07/R13 focused move and one test; byte/scope checks above.
R08/R14 no performance/profile claim or comparative evidence; root-owned target
unproven. R09/R10 actual root legacy red retained, new actual green pending.
R11/R12 shared standard transport; Linux amd64 KVM campaign only; new combined
seven-target/other native host/libc/architecture gates remain root-owed.
R15 scoped signed implementation plus assigned one-case campaign, no children or
main merge. R16 fresh guarded private paths; preserve interruptions/data; no cache
deletion. Full P7, audits/mutations, resource accounting, durable pre-effect audit,
per-execution provenance, full population/kernel return and performance remain open.
