# Selective relay carry on current main

Base a920c85b5c0c9d40aafc4f4cce32432c7c0aed79, policy SHA256
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Implementer + assigned adversarial relay tester; ROOT owns evidence review,
combined KVM/seven-target validation, all profiling and main integration.
Full current AGENTS/R01–R16/evidence/CLAUDE/plan/handoff readback preceded this
carry. Latest explicit main a920 supersedes older handoff33df references.

This branch starts at current main. It carries seven exact reviewed source files
from signed37ee895ed27b7e5e91f061f4ab2496229b24781b, with hashes/lengths in
[source-identities.json](source-identities.json). The three existing main files
were byte-identical to their cbd merge-base versions; four new paths were absent
on main. Applying only that named-path diff needed no conflicts. The current
metrics, ABI, observer, broker, admission and policy changes are retained. The
old branch, sources, targets and every frozen campaign remain intact; no rebase,
whole old-branch merge, additional local worktree or mid-campaign rebuild occurs.

The required pieces are narrow engine caller wiring; one new shared Linux guest
pump; testguest parser/original Deadline relocation; the existing Worker's typed
completion parameter; the new TCP/engine fixture; its new host leaf; and the897-byte
cfg(test) baseline include containing605 exact old-body bytes. Parent host
managed_network.rs is untouched. [host-registration.patch](host-registration.patch)
is deliberately unapplied for ROOT's registration. The real runtime caller is
Standby::start → engine::start/serve/relay(UnixStream,&[u8;TOKEN]); testguest uses
the same pump. Dial, HELLO/token, grants and controller are preserved.

Production engine SHA2561f32faae6fb63c2beeb5accf0292d90f0608207f32bfe09f9495a0f4edd22de7;
shared pump083bce5ec7f0a96c74d1e44c0aa3d882344a36fce1401b017f8e114a8841fe27.
The existing EndPolicy applies full Unix shutdown at drained host EOF after
actual old-body/shared late-write mismatch; independent TCP uses write-only
half-close. Fixed caller/OS-page slots, original Deadline/cancel admission,
typed error/end/progress and already consumed/sent facts remain one implementation.

## Original evidence, unchanged provenance

The original evidence remains at immutable signed revisions; this carry does not
copy hundreds of historical proof files or inherit a current-main passing gate.

- [Reviewed callers and primary Unix/VSOCK/Rust alternatives](https://github.com/hyper-light/shards/blob/37ee895ed27b7e5e91f061f4ab2496229b24781b/docs/research/measurements/managed-tcp-forward/implementation/RUNTIME.md).
- [Actual native EOF red/correction/mutation/restoration and normal corrected init build](https://github.com/hyper-light/shards/blob/37ee895ed27b7e5e91f061f4ab2496229b24781b/docs/research/measurements/managed-tcp-forward/implementation/engine-native/README.md): normal static-musl init build/strict lint0, GNU/musl projection strict lint0 and16 final native cases0. Original raw archives43d9f44a…/b7b7fca1… retain verified152/120 regular members and exact original byte hashes.
- [Native pressure, held-backend completion/cancellation and byte-corruption mutation](https://github.com/hyper-light/shards/blob/37ee895ed27b7e5e91f061f4ab2496229b24781b/docs/research/measurements/managed-tcp-forward/implementation/native-followup/README.md).
- [Actual backend reset red/green/mutation and one old-engine KVM client-FIN-first baseline](https://github.com/hyper-light/shards/blob/37ee895ed27b7e5e91f061f4ab2496229b24781b/docs/research/measurements/managed-tcp-forward/implementation/reset/README.md).
- [Client-origin reset](https://github.com/hyper-light/shards/blob/37ee895ed27b7e5e91f061f4ab2496229b24781b/docs/research/measurements/managed-tcp-forward/implementation/client-reset/README.md) and [actual extra-client-byte guard removal](https://github.com/hyper-light/shards/blob/37ee895ed27b7e5e91f061f4ab2496229b24781b/docs/research/measurements/managed-tcp-forward/implementation/extra-client-byte/README.md).

[frozen-pressure.json](frozen-pressure.json) closes the already assigned r139
campaign at37ee without rebuilding. Four GNU/musl × close-order cases pass0;
each actual backend receives1048576 exact bytes and client131080, both EOFs,
pending/extra0, real short sends12–23 and send EAGAIN23–64, child/parentFD4→4,
held pidfd exit/reap and unchanged binaries. Caller supplies65536-byte slots and
4096 queue inputs, actual readback8192; these are finite test inputs. Source
fixture0e346df4…/shared083bce5e… and binaries e2eb1fe6…/d251660f… stay frozen.
Full original stdout/observed bytes remain under private Fedora
~/relay-tcp-corrected-pressure-v1; the compact record retains their exact lengths/
hashes and independent payload hashes. This is socket correctness, with physical/
kernel return unknown and no comparative performance claim.

## Owed ordinary VSOCK caller

ROOT will run normal carried KVM engine cases centrally after composing. Unix
socketpairs are Unix proof. Actual before/after VSOCK, host-finish compatibility
and current selected revision's combined gates remain owed. No second VM was
launched by this lane after its approved original client-FIN-first pilot.

After registering the host leaf, its exact ordinary filter is
`tcp_forward::managed_engine_half_close_records_actual_host_finish`, run with
`--exact --ignored --nocapture --test-threads=1`. It executes ordinary
`run --use-api-socket`, with source-matched normal carried init and static guest.
The [original normal-launch driver](https://github.com/hyper-light/shards/blob/37ee895ed27b7e5e91f061f4ab2496229b24781b/docs/research/measurements/managed-tcp-forward/implementation/reset/commands/engine-before-normal.py)
retains all actual caller inputs, owned paths and cleanup, one finite mode only.
ROOT selects its current binary/records/kernel/config paths and each close mode;
the driver must not reuse its existing record root or assert an old source hash
against the new source. Case30s/boot120s/parent240s, VM1CPU/256MiB and fixed GET
/_ping, suffixOK, host-lateX are explicit inputs. Normal sparse RAM/DAX launch
uses no guessed AS cap. Admission is not a hard physical/kernel cap.
The observational host leaf records both Accepted and Failed late-write outcomes;
test0 alone does not prove equivalence. ROOT compares the actual old/new outcome.

The [current-selection validation record](validation/README.md) now retains actual
GNU/musl normal init check/strict lint, module projection checks and the existing
finite native corpus at exact1be. It preserves r141's harness layout failure and
r142's19 passes plus one pressure-witness failure, followed by one unchanged
separately retained passing invocation. The original batch stays failed; no
source/assertion change or inherited gate is claimed.

All R01–R16 original accounts and uncertainties are linked above. No new production mechanism,
constant, cache, polling interval, grant, profile, helper-control or allocation-
fault campaign is added. Mandatory route coverage, producer/kernel budgets and
management P0–P13/G25 remain open. macOS remains ARM64 only; paused WinARM,
ROOT caches and all held campaigns remain untouched. No active LOCAL target/cache.
