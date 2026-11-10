# Native observer retained-cursor fixture checkpoint

Implementation/light checkpoint only. Full caller typechecking and all actual
endpoint, VM, reconnect, adversarial, mutation, resource and cleanup gates remain
ROOT-owned and unrun here. P0–P13/G01–G24 remain open.

## Identity and selected sources

POLICY_READ implementer `/root/observer_reconnect`; coordinator `/root`.
Worktree `managed-output-exit`, branch `p/native-observer-reconnect-v1`, based on
`b02e8592c547553c788702ac6c52ea6b633e875d`. Current canonical main/origin/main at
readback: `eb5654d6f5356cacaca536ff51f14a632fa7e801`. Full AGENTS.md,
R01–R16, development-evidence.md, CLAUDE.md, managed-runtime-plan.md and
`/Users/adalundhe/g121-handoff.md` were read before edits. Current origin/main
AGENTS.md and rules were also read in full. Policy last-change revision:
`664541da5ec488446b481ff6509b60510912c231`; rules SHA-256:
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.

Only these prepared pieces were selected; no old branch was merged:

- `0f0b140ebb4c572fdbb9644cbb193712814bdc7a`: proposed initial-offset seam.
- `d699430f81c9cc47ae8bd8cb38d8ad61e9a8de41`: retained Data-record witness.
- `8a13515e753f8bf1ee772770b55e4f3dbae353ef`: reconnect body.
- `148f679ff672e4702d41f8cf6f898da7abe63136`: selected leaf versions and five pure checks.

Owned changes are the Client/Observer constructor/subscribe and Fixture observe
region in `crates/shards/tests/managed_output.rs`, the two selected reconnect
leaves, and this new evidence directory. Fixture start/publication, tenant_exit
and tenant_stdio bodies, allocator/cost registration, production remote/daemon,
ABI and workloads are unchanged. [Hashes](raw/code-source.sha256) identify the
exact current sources, including excluded paths.

ROOT must apply [register-reconnect.patch](register-reconnect.patch) to register
`tenant_exit::reconnect`; this patch is deliberately unapplied. Current existing
test registration and the external inherited-writer lane remain untouched.

## Contract and alternatives

`Client.from` is supplied by Observer::start and encoded through the existing
`managed::subscribe(scope, from)` handshake. Existing Fixture::observe calls
Fixture::observe_at with zero, preserving every existing initial observer. The
second real caller supplies the observed start of the final unACKed Data record.
No production resume, restart or generation API is added.

The production primary source at the base revision is
[crates/remote/src/managed.rs](../../../../crates/remote/src/managed.rs):
subscribe/control/check_scope/disconnect and the existing tests
`observer_loss_retains_only_the_unacknowledged_record_for_the_same_cursor`,
`replay_and_forward_cursors_do_not_subscribe_to_a_new_stream`,
`only_a_contiguous_application_ack_returns_credit` and
`refusal_categories_preserve_scope_and_stale_generation`. Retained cursors name
canonical wire bytes, while ABI StreamState offsets name payload bytes. A ready
cursor or the one retained record's exact start is admissible; arbitrary old or
forward history is not. Invalid cursor controls map to Reason::Protocol; wrong
run/member/channel to Reason::Scope, wrong generation to Reason::Stale. These
are source contracts here, not newly executed endpoint refusal results.

[Managed-runtime ownership](../../../design/managed-runtime-plan.md#4-fix-merged-output-and-ownership)
assigns lifetime to the authoritative owner, independently of borrowed observers.
[RFC 9000 §3](https://www.rfc-editor.org/rfc/rfc9000.html#section-3) describes
transport sending/receiving states; it does not establish actor lifetime.
[Rust's recv_timeout contract](https://doc.rust-lang.org/1.98.1/std/sync/mpsc/struct.Receiver.html#method.recv_timeout)
describes the existing notification/deadline wait. The consulted stable online
render was Rust 1.99.1; actual compilation below used pinned 1.98.1.

| Alternative | Decision and concrete tradeoff |
| --- | --- |
| Supply the initial cursor at the existing constructor/observe seam | Selected: initial-zero and retained-record callers share the unchanged authenticated scope, admission and transport path. |
| Add Observer::start_at plus an unused start wrapper | Omitted: Fixture is the only Observer::start caller; the existing Fixture::observe wrapper preserves compatibility. |
| Change server retention/history or infer a new generation | Rejected: production already implements the required finite retained-record contract; this task supplies a missing test input. Broader semantics require separate evidence. |
| Recreate the transport in test stubs | Not used for evidence: the light check compiles exact ABI/witness/reader regions only and makes no whole-caller or endpoint claim. |

The leaf is selected from 148f with the current publication argument
`Some(&[base.port])`, removal of its obsolete proposed-seam comment and ignore
wording, and normal repository formatting. Its canonical End/ACK/transport-EOF
and strict terminal metadata assertions are retained.

## Fixture sequence and waits

The prepared fixture completes member1 with exact canonical End, ACK and EOF on
both channels; holds the final member2 Data record unACKed on each channel; saves
its actual wire start, exact bytes and prior canonical state; cancels/joins both
borrowed observers; and exercises independent main stdout/stderr. It reconnects
with the same authenticated scope and exact held cursor and requires identical
retained bytes/state before ACK and owner gate release. Fresh member2 fd1/fd2
suffixes and another main execution precede owner stop/remove, daemon stop,
observer joins, FD-return checks and strict terminal assertions.

The owner never transfers to an observer. Local observer join is explicitly not
a remote close-processing barrier. No retry, polling loop, guessed sleep or
assertion weakening masks an actual typed refusal or timeout. Those outcomes
remain ROOT evidence to collect. ACK enqueue alone is not acknowledged
consumption; later new production supplies that witness. Expected stopped
metadata stays member1 `ended/complete`, owner-stopped member2 `aborted/unknown`.

No affected cache was added. Existing transport readiness, mpsc notifications
and caller-supplied finite deadlines remain; the only retained witness is finite
controlled fixture data, not a runtime cache. Actual wait races and endpoint costs
remain unverified here.

## Executed light checks

Host: macOS arm64, Darwin 25.4.0. Compiler: rustc 1.98.1
`48a229ceaefd4985c50990b14116b6d856af0985`, LLVM 22.1.8. The exact ABI crate,
retained witness and existing reader regions were compiled with rustc directly;
no Cargo target/cache, host stand-in, socket, VM or guest helper was run.

```sh
python3 docs/research/measurements/managed-native-reconnect/prepare-light.py \
  . /tmp/shards-native-reconnect-pure-b02e-v1
env -u DYLD_LIBRARY_PATH -u DYLD_FALLBACK_LIBRARY_PATH \
  -u DYLD_FRAMEWORK_PATH -u DYLD_FALLBACK_FRAMEWORK_PATH \
  -u DYLD_INSERT_LIBRARIES \
  sh docs/research/measurements/managed-native-reconnect/run-light.sh \
  /tmp/shards-native-reconnect-pure-b02e-v1 \
  /tmp/shards-native-reconnect-pure-output-b02e-v1 1.98.1
```

Result: exit 0; **5 passed, 0 failed, 0 ignored, 0 filtered out**.
[Raw output](raw/pure-tests.log.gz) includes the expected caught shifted-cursor
assertion, followed by that test passing with unchanged state/payload. These tests
exercise nonzero wire vs payload offsets, exact retained replay and later new
production, zero/shifted/forward-cursor typed witness errors, changed/truncated
bytes, identity/channel mismatch, canonical offset mismatch and wire overflow.
They do not prove network refusal or a generation transition.

Exact code-region hashes, source-copy hashes, compiler/host, retained private
source/output paths and output-binary hashes are in [raw](raw/artifacts.json).
Source copy: 192 KiB; output: 3356 KiB. Free space before the run was
34,522,308,608 bytes, above the 30 GiB floor; the after value is recorded. No
source, credentials, prior evidence or artifacts were deleted. Fedora was only
discovered through Kiln under 1 CPU/1 GiB/2 GiB admission; no remote build or
fixture ran and no root target/cache was accessed.

Rustfmt check, `git diff --check`, unchanged excluded-path comparison and
`scripts/check-development-policy` all exited 0. [Commands/results](raw/checks.json)
and [policy output](raw/policy.log) are retained. The first staged whitespace
check rejected literal trailing spaces in the raw test log and blank context
lines in the patch artifact; [its exact output](raw/staged-check-before.json)
is retained. The log is now losslessly compressed (original bytes remain in the
private output directory), and the patch uses nonblank context. The final staged
check passes; source/test behavior is unchanged. Full caller cargo check/clippy
and all seven-target/real-VM checks are **unrun**, not passing skips.

## R01–R16 accounting and ROOT handoff

| Rule | Applicable evidence and remaining limit |
| --- | --- |
| R01 | Test-only seam/leaves; production and panic lints unchanged. Test assertions are intentional. Universal production panic freedom is not claimed. |
| R02 | Pinned selected source, current endpoint contract and external primary sources/alternatives above; no broader mechanism selected. |
| R03 | No production literals added. Zero is the existing initial cursor; reconnect uses an observed cursor. Port/work/phase/cleanup are explicit positive finite caller inputs. |
| R04 | Existing ABI state/refusal types preserved; witness ReplayError remains typed. Actual wire Protocol/Scope/Stale validation is owed. |
| R05 | Two real cursor callers share one constructor and existing owner/handshake. Wait/cache analysis above; local join uncertainty preserved. |
| R06 | Parent seam is 18 added lines/one replacement; prepared focused reader/witness leaves selected. Full coordinator source review remains required. |
| R07 | Removed obsolete seam proposal comment; preserved ACK, offset, ownership and uncertainty rationale. |
| R08 | No speed/resource comparison or 4–5× claim; coordinator baselines remain owed. |
| R09 | Authorized seam/fixture composition, not a guessed production bug fix. No live reconnect defect is claimed reproduced. |
| R10 | Reproducer, exact hashes/raw five-test results retained. Actual live before/after, guard-removal, cleanup and forbidden effects remain owed. |
| R11 | Light result is macOS arm64 only; other six targets and real KVM/HVF/WHP are unverified here. Windows ARM VM remains paused. |
| R12 | Shared test transport/ABI; no platform adapter, installed binary, root/admin, trust or crypto change. |
| R13 | Minimum approved constructor input/default wrapper; no speculative production resume state or new runtime abstraction. |
| R14 | No profiling/benchmark/cost/fault/leak claim; all such work belongs to ROOT. |
| R15 | Scoped implementer, no children, no endpoint/native/VM/adversarial campaign or main merge. Signed named-path checkpoint returned to ROOT. |
| R16 | Tiny private rustc outputs, no new full worktree/cache, floor checked, prior source/data/evidence preserved; no cleanup performed. |

ROOT owes registration plus exact full-caller type/clippy checks, ordinary remote
managed state/refusal tests, the ignored reconnect fixture and ordinary owner
fixtures with matched source/artifacts. New reconnect test path after registration:
`tenant_exit::reconnect::fixture::observer_disconnect_replays_retained_record_without_losing_owner`.
Required explicit inputs: `SHARDS_TENANT_RECONNECT_{PORT,DEADLINE_MS,WORK_MS,CLEANUP_MS,RECORDS}`;
`SHARDS_TENANT_RECONNECT_SOURCE_REVISION` records source identity. RECORDS must be
a new absolute directory; the other values positive/finite. ROOT chooses actual
resource/deadline inputs and supported VM environment.

ROOT's decisive gates include actual typed refusals for wrong/obsolete/shifted/
forward cursors (Protocol), stale generations (Stale), wrong scopes (Scope), exact
retained bytes/one logical payload copy, disconnect/reconnect races without
hidden retries, sibling/main progress, meaningful guard removals with actual
wrong bytes/scope/ownership effects, exact restoration, strict canonical terminal
metadata, independent population/reaping/kernel/handle/resource return, and the
supported matrix. Wrong-owner TLS cause remains erased by the existing public
Driver, as the ordinary owner fixture already records; it is not relabelled a
new passing typed Auth result. Unknown cleanup remains unknown.
