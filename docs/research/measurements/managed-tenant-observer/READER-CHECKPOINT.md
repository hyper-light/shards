# Reconnect reader boundaries: source and five pure checks per libc

IMPLEMENTER `01a1c5d28cea74a5b0241200190d6c54`; ROOT coordinator
`01a148884a4770fdaf0111f3f792a248`. Full canonical policy/CLAUDE/plan/evidence/
handoff re-read on the continued goal at main
`eb5654d6f5356cacaca536ff51f14a632fa7e801`; rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242` unchanged.
No new root authority or actual fixture result is inferred from that receipt.

The previous turn was source/light progress through signed8a13515e7 and the
separate inherited/stdout-closure candidates. This continuation found the own
worktree clean at8a13515e7 and ROOT working, with no cursor-seam approval in its
last reply. It therefore leaves that parent edit unapplied. This checkpoint adds
targeted pure execution of the new reader, which had previously only compiled;
it does not relabel the integration dependency as complete or start another actor.

## Source and boundary exercised

Only the owned `tenant_observer.rs` reader and this evidence change. Its existing
accept function now takes the member/channel pair it actually checks, rather
than the entire Observer. Both actual callers still derive that pair from the
authenticated Observer's scope. No guard or behavior is weakened, no caller can
derive its expected identity from the received record, and run/generation/
authentication remain the existing endpoint's responsibility. This narrow pure
seam allows the actual reader to run without creating an Observer/thread/socket.

Two additional pure tests execute the actual reader's byte/state/wire tracking:

- For each actual channel, encode Open and two fragments of the controlled
  harness prefix with the real ABI. Hold the final fragment at its observed
  nonzero wire start, replay that exact canonical record through RetainedRecord,
  then accept genuinely new controlled suffix Data and End. Require exact logical
  capture bytes once, canonical state, and final wire cursor equal to the sum of
  actual encoded lengths, distinct from the payload length. This checks the new
  reader's Capture.bytes and wire bookkeeping alongside the retained-state guard.
- After a valid Open, supply otherwise valid Data at a shifted observed transport
  cursor. The real reader assertion must reject before changing wire, canonical
  state or capture bytes. The test catches that expected test-only panic and
  asserts the original state/bytes remain. The raw assertion at31 versus30 is
  retained in both logs; it is not a production panic or a failing test result.

The original three retained-record pure tests remain selected too. No native
helper, endpoint, actual observer, host fixture body, socket, VM, namespace, BPF,
mutation campaign, audit, profile or benchmark was executed. Fragmentation and
the shifted record are bounded deterministic test inputs, not fuzz/stress coverage.
They do not prove ACK delivery, remote close processing or actual owner lifetime.

Parent `managed_output.rs`, native workload and strict normal terminal function
are unchanged from b324. The proposed Client/Observer/Fixture initial cursor
patch is still only an artifact outside owned parent scope. ROOT approval/
application and module wiring remain necessary before full actual host integration.
The existing typed transport refusal/deadline remains a failure; no retry, sleep,
poll, extra queue or manager is introduced. Strict r692 metadata101 remains
refactor-owned and is not waived by these pure passes.

## Exact light result and reproduction

Fedora `8fadc1ec213332e16cb0d915a3fb913f`, r36, NEW private
`~/managed-tenant-reconnect-light-01a1c5d2-v2/{source,target}`, Kiln admission
`linux/amd64,cpus=1,mem=1g,disk=2g`, Cargo jobs1/offline. Rust/cargo1.99.1,
Fedora44/kernel6.19.10. Source372KiB, target209,696KiB. Headroom is not hard
aggregate CPU/memory/parent/kernel accounting. No local compilation/target/cache.

GNU/musl check --tests and clippy --tests -Dwarnings all0; formatter and policy0.
GNU5/5 and musl5/5 selected pure tests,0fail/0ignore/2filtered, with the actual
normal/reconnect VM fixtures excluded. Surrounding host integration and the
proposed observe_at interface remain explicit inert compile stubs. Full actual
host/helper builds and effects are not established.

```sh
cargo test --lib --offline -j1 tenant_exit::reconnect:: -- \
  --skip observer_disconnect_replays_retained_record_without_losing_owner \
  --nocapture --test-threads=1
cargo test --lib --offline -j1 --target x86_64-unknown-linux-musl \
  tenant_exit::reconnect:: -- \
  --skip observer_disconnect_replays_retained_record_without_losing_owner \
  --nocapture --test-threads=1
```

Run only in the private source prepared by the existing prepare-host-light.py,
with private CARGO_TARGET_DIR/jobs1 and Kiln admission; these are not invocations
of the actual ignored integration fixture. [Exact command](reader-kiln-v2.command),
[raw logs/exits/compiler/lock/checked hashes](host-raw/v2.json) and
[new source manifest](reader-source.sha256) retain the final check. New leaf SHA256
`6c54aa912211978e0e08060f266f73d5a7def37e1e7602e7ff0a10fd9f4f65aa`.
Configured formatted bytes were hash-matched on adoption. Historical8a hashes
and its3/3 result remain in BODY-CHECKPOINT/host-source.sha256, not overwritten
or substituted as a new revision's gate.

## R01–R16 and remaining ROOT work

| Rule | Scoped result and gap |
| --- | --- |
| R01 | No product/native edit; expected panic is caught in a pure test of an existing test assertion. No universal implicit-panic/allocation proof. |
| R02 | Existing actual reader/ABI/retained-record contract and its pinned sources from BODY/PROPOSAL; no new architectural decision. |
| R03 | Actual encoded lengths/captured prefix and two existing channels derive expectations. No production/default number added. |
| R04 | Canonical state, wire cursor and logical bytes verified together; shifted input cannot partially change them. Remote/cleanup uncertainty stays explicit. |
| R05 | Pure input narrowed to checked identity; actual callers retain authenticated scope. No cache, manager, polling or retry. |
| R06 | Two focused reader tests plus existing three; exact four compile/clippy0 and two5/5 corpora. ROOT combined audit owed. |
| R07 | Comments/source continue to explain retained credit and local/remote boundary; no new product narration. |
| R08 | No comparative/4–5×/performance claim. |
| R09 | Test coverage of new fixture code, no speculative product correction or actual repro claim. ROOT r692 remains strict. |
| R10 | Exact source/final logs, expected assertion, commands and historical evidence retained. Actual effects/mutations/cleanup unrun. |
| R11 | GNU/musl amd64 pure/compile only. Seven targets and real KVM/HVF/WHP unverified for this source; macOS ARM64 only. |
| R12 | Shared existing ABI/reader; no platform adapter, installed binary or privilege/grant/profile change. |
| R13 | Existing reader plus two tests, no replacement protocol or lifecycle owner. |
| R14 | No allocation/copy/fault/leak/resource-distribution claim or profiling; all parent/kernel/cost work remains ROOT. |
| R15 | No children/auto dispatch/audits/heavy/actual helpers/main merge; signed own source/light record. Cursor authority and actual campaigns remain ROOT. |
| R16 | Existing worktree and small snapshots, NEW private remote source/target; no local target/cache or deletion; ROOT paths and paused Windows ARM preserved. |

Unchanged owed gates: ROOT's parent cursor approval/application/full build;
actual strict normal metadata green; actual observer disconnect/replay/new
sibling/main bytes; typed cursor/identity negatives and guard/restoration cases;
owned stop/remove/observer joins and independent population/FD/kernel return;
seven-target/real-host gates. Neither this step's actual integration nor the
overall managed-runtime lane is marked complete.
