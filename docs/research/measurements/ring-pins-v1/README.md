# ring-pins-v1: an engine ring's pins bounded by use and by the run's room

The implementer's evidence for D230's protocol version 2: the decision and its numbers are
section 16 of [engine-ring-channel-decision-v1](../../engine-ring-channel-decision-v1.md),
D230 in [architecture.md](../../../design/architecture.md).

- **Identity.** Branch `p/ring-pins-v1`: 3480f9299 (the first-request fix, on main as
  cc1b52e61), 946eaa687 (version 2), e0b04ab84 (origin/main c4ae59ee0 merged in), 02350b8d3
  (the pin test, the stand-in's answer to a crossing swap, this record), dcddaf476
  (origin/main d59e0705b merged in, r4), 18a1dc2b6 (two test fixes r4 found, r5), and the
  commit adding r5, which changes no source; then branch `p/ring-e2e-v1` from ROOT's
  `p/whp-parity-fixes` (3d49939ca): 026dbc846 (the E2E's fixes), 4a3861e4a (origin/main
  8f3cd5b74 merged in, r13, r14), 33cc69d1e and ea3482851 (the two errors Windows' lint found,
  r15), a1fc36a84 (vsock/ring.rs's tests on macOS, r15), 0ba8fca9f (r15's records); then
  037c69cc8, f674182bb and bdd0e9583 (the give-back and shrinking-room tests on every host, r16),
  716dc45fb (origin/main 87bf86b31 merged in, r16), 38d857d88 (origin/main fe44bc8ca merged in:
  softvirt's records, harnesses and workflow, no Rust source, Cargo file or script), 6d9219874
  (origin/main 451672250 merged in: tests of shards-api and shards-vmm, scripts/gpu-libs), and
  the commit adding r16, which changes no source. Role: implementer for ROOT (01a148884a4770fdaf0111f3f792a248); policy receipt
  [POLICY_READ.json](POLICY_READ.json) (development-rules.md sha256 df2acf47…, re-read on
  c4ae59ee0 after a restart, unchanged on 87bf86b31). Runners: adacachy (linux/x86_64, CachyOS
  7.2.9) for r1-r12; kiln-a094a12d (linux/x86_64, Fedora, Linux 6.19.10), this Mac (macOS 26.4.1,
  Apple M5 Max) and jupiter (Windows 11 Pro, x86_64) for r13-r16; rustc 1.98.1. Merged to main at
  the person's instruction (POLICY_READ.json `superseding_instruction`).
- **Owned paths.** crates/init/src/ring.rs, the ring parts of crates/init/src/engine.rs and
  run.rs (the `ROOM` and `RECLAIM` hooks), crates/abi/src/ring.rs,
  crates/vmm/src/devices/virtio/vsock/{ring.rs,conn/ringed.rs},
  crates/shards/tests/engine_ring.rs, tests/common's `Told` (moved from containers.rs), the D230
  record, this directory; `pub(crate)` on
  init's `plug::cap`, `plug::working_room` and `domains::available_memory`.

## Logs

| Log | What it shows |
|---|---|
| [r1-red-first-request.txt](logs/r1-red-first-request.txt) | Red (R09): version 1's device never heard a ringed connection's first request (0 of 34 bytes), reproduced before its fix; ROOT's real-VM r521 shows the same hang. |
| [r2-green-first-request.txt](logs/r2-green-first-request.txt) | Its fix green, and its guard (the flag's store) removed failing both of its tests. |
| [r3-gate-e0b04ab84.txt](logs/r3-gate-e0b04ab84.txt) | The merged revision: fmt; unit tests (abi, the vsock device, init); clippy -D warnings on x86_64 glibc and x86_64 and aarch64 musl; the policy check; guard mutations M1-M3, each failing for its reason; the E2E file compiled. |
| [r4-gate-dcddaf476.txt](logs/r4-gate-dcddaf476.txt) | dcddaf476 (the pin test, the stand-in's answer to a crossing swap, origin/main d59e0705b): the same gate, the pin test, the relay's tests ten times under load, the mutations again. Found a test-harness race (one run in ten) and an unsafe block's comment, fixed in 18a1dc2b6. |
| [r5-gate-final.txt](logs/r5-gate-final.txt) | 18a1dc2b6, whose source is the pushed revision's: the same gate, the relay's tests twenty times. |

## The E2E on real VMs (2026-11-12, branch `p/ring-e2e-v1`)

ROOT ran `crates/shards/tests/engine_ring.rs` on real VMs for the first time (Linux KVM and
WHP): all four tests failed. Each failure's cause, shown live on adacachy (Linux KVM, x86_64),
and which was wrong:

| Test | Cause, with its evidence | Wrong |
|---|---|---|
| `a_microvms_engine_carries_bulk_whole_both_ways` | bulk-sum's create named the image's entrypoint again (`Cmd ["/bin/testguest","bulk-sum"]`, mode "/bin/testguest", exit 2); then the test guest's HTTP client read the start's 204 as content delimited by its connection's end, on a kept connection (r9: the same hang with rings and with none; the daemon's 204 written whole) | the test guest |
| `a_give_back_lets_an_idle_slot_go`, `a_shrinking_room_…` | the connection they held open, `GET /events`, is one a microVM's own engine does not serve (engine/scope.rs answers 501 and closes: its ring let go `HostEnded` 1.8 ms after it attached, r6); they hold a wait on a sleeping joiner | the test |
| `an_idle_microvm_pins_nothing_…` | it read the guest's shared memory once, right after its streams, which stood 12 to 36 kB over where it came back to within 3 s (r8), and which steps by 28 to 32 kB at times of its own, in runs with no engine too (r10, r12): it counts the guest's pinned pages now (FOLL_PIN acquired less released, exact) | the test |

Fixed in 026dbc846 (with the bulk test's stdin arm ending at once, with its container's exit,
where the container ends early, as ROOT asked after WHP). No D230 product code was wrong.

Found, not fixed (ROOT's goal, "Shards exec hang root-caused and fixed"): of 64 concurrent
`shards exec` into a 32-vCPU microVM with no engine, 2 never ended, their CLIs blocked on their
exec's pipe (r11). The pins test drives each set of V and 2V streams from one exec since.

| Log | What it shows |
|---|---|
| [r6-e2e-first-run-root.txt](logs/r6-e2e-first-run-root.txt) | ROOT's first run (3d49939ca): the four failures, the held connection's ring let go 1.8 ms after it attached. |
| [r7-e2e-rerun-adacachy.txt](logs/r7-e2e-rerun-adacachy.txt) | The first fixes: give-back and shrinking room pass; the stdin arm hangs; the pins test's +80 kB, 0 kB in the run without the engine. |
| [r8-e2e-shmem-per-stream.txt](logs/r8-e2e-shmem-per-stream.txt) | A 1-vCPU microVM's shared memory after each stream: 12 to 36 kB over, back within 3 s, nothing kept. |
| [r9-e2e-stdin-diagnosis.txt](logs/r9-e2e-stdin-diagnosis.txt) | The stdin hang with rings, without rings, with stdin beside the start, and the daemon's own steps: the 204 written; the client's body() the cause. |
| [r10-e2e-three-green.txt](logs/r10-e2e-three-green.txt) | Three tests green; the pins test's 2-vCPU idle reading below its run without the engine's. |
| [r11-exec-hang-64-at-once.txt](logs/r11-exec-hang-64-at-once.txt) | The exec hang at 64 at once (wchan, elapsed, the commands). |
| [r12-e2e-shmem-step.txt](logs/r12-e2e-shmem-step.txt) | Shared memory's 32 kB step at 32 vCPUs after its streams, the device having let go of every ring: why the test counts pinned pages. |
| [r13-e2e-kvm-and-gates.txt](logs/r13-e2e-kvm-and-gates.txt) | The pushed source (4a3861e4a): the four E2E tests on Linux KVM x86_64 (kiln-a094a12d, Linux 6.19), green: 0 pages pinned idle with and without the engine's socket, 443 rings at 32 vCPUs attached and let go, 0 pages pinned after; fmt and unit tests on Linux x86_64 (abi 49, vsock 66, init 197 with the relay's 20). |
| [r14-e2e-failfast-and-witness.txt](logs/r14-e2e-failfast-and-witness.txt) | A sum container that ends at once fails the bulk test in 28 s with its exit, no hang; the pinned-page witness reads 0, then 3 (a held connection's slot), then 0. |
| [r15-lint-and-macos.txt](logs/r15-lint-and-macos.txt) | scripts/lint of all seven supported targets, LINT-EXIT 0 (after the two errors Windows' found, one of them main's); unit tests on macOS arm64, where three vsock/ring.rs tests hung on 8 KiB Unix socket buffers until their pairs held what they write (a1fc36a84), then 66 of 66; the vsock device's 39 on Windows natively (jupiter); the Windows `shards` unit tests not built there for want of NASM, owed. |
| [r16-e2e-every-host.txt](logs/r16-e2e-every-host.txt) | The four E2E tests with the give-back and shrinking-room tests on every host (the host's word through its own pipe; the room through a slice, root's own cgroup, or a group shards keeps): green on Linux KVM x86_64, HVF arm64 and Linux arm64 KVM in L1 (no SKIP); on WHP x86_64 (jupiter, natively) give-back and shrinking room green twice, the bulk test once in two (its microVM stopped, as a run with no engine did too), the pins test stopped at 32 streams at once by the daemon's Windows bound (13 connections a microVM's engine); `served_pipes_tests` 5 of 5 and the vsock device's 39 natively on Windows; scripts/lint of all seven targets on the merge with main (716dc45fb, 6d9219874), where the E2E is green again on Linux KVM x86_64 and HVF; main's Windows CI, which never refused main's engine_ring.rs (the one refusal 037c69cc8's, on this branch, fixed in f674182bb). |

Guard mutations (r3): M1, the relay freeing its slot without hearing the device's `DETACH`,
fails `the_relay_frees_a_slot_only_after_the_devices_detach` ("the relay let go of its slot
before the device's DETACH"), or, as the timing falls, kills the test process (a core
dump in the gate unit's journal): the device stand-in read and wrote the slot the relay had
unmapped, the use after free the guard exists to prevent. M2, the device saying `DETACH`
with the slot's pins still held, fails five device tests of `DETACH`, swaps and resets. M3,
a slot made whatever the room, fails `a_stream_past_the_room_goes_over_plain_vsock` and
`with_no_budget_pins_are_bounded_by_the_available_memory`. Under M3 the stand-in also met a
`SWAP` after its own `DETACH`, a race the protocol allows (the device refuses such a swap,
`Refusal::Ended`) and the stand-in did not answer: it answers it since 02350b8d3, and in r4
M3 fails those two tests alone.

What the unit tests do not reach: the relay's tests run it against a device stand-in, its
slots unpinned in the test's own memory, so pagemap's frame numbers (CAP_SYS_ADMIN only),
the leaf join on a real cgroup, the shared page across init's fork and the daemon's words
are covered only by the real-VM E2E (r13, r16); `a_slots_pins_count_to_the_instances_maker_and_go_with_its_entry`
runs the pin and unpin calls themselves on the runner's kernel.

## Owed to ROOT

- E2E, `crates/shards/tests/engine_ring.rs`: on WHP arm64 (no Windows arm64 host with its
  hypervisor reached); on WHP x86_64, the pins test past its first 32 streams at once, which the
  daemon's Windows bound refuses (below), and the bulk test's microVM stop, root-caused with the
  daemon's and VM's logs kept (r16: exit 125 after 1.3 s; a run with no engine stopped the same
  way after 4.9 s, so not the ring's; the logs went with their test directories). Ran: Linux KVM
  x86_64 (r13, r16), HVF arm64, Linux arm64 KVM in L1, and WHP x86_64 (r16).
- Not D230's, found by it: on Windows the daemon holds a microVM's own engine to 13
  connections at once (`share(27, 0)`, daemon/engine.rs) and its clients to 27
  (`clients_for(POSIX_THREADS)`, daemon.rs), as `thread_limit()` says none there; Linux and macOS
  derive theirs from their limits. A Windows bound from what limits its threads (the process's
  virtual memory, CreateThread's documentation), measured, is D181's admission's to decide.
- The engine suite (D181), isolation's engine-port token test, the budget and reclaim suites,
  on real VMs.
- Their reds: the idle-pins test on 032b63b1c (version 1, which pins slots as its relay
  starts), and mutations: `told` removed from `ROOM` (the shrinking-room test must fail), the
  leaf join removed (`read metrics` of the run must lose the slots' bytes).
- Measurements: a slot's making and freeing by ring; connection setup and first-message
  latency, 64 B to 1 MiB, fresh and kept connections, against plain vsock; a bulk stream's
  ramp against a fixed 2 MiB ring; 256 MiB each way; the device worker's CPU; the run's
  memory with pins at 1, 2 and many vCPUs.

## R01–R16

| Rule | Proof, and its gap |
|---|---|
| R01 | Production code returns its errors: `?`, `get`, checked and saturating arithmetic on positions the host writes (each read once and believed only within the ring), poison-tolerant locks, `thread::Builder::spawn`, `try_reserve` for a swap's offer; the panic lints pass with clippy -D warnings (r3, r5). Reviewed by hand for the casts the lints allow (`as` on lengths bounded by `MAX_RING`). Tests assert. |
| R02 | Section 16's alternatives (version 1's pool, pooling freed slots, freeing without the device's word, an idle timeout, pins outside the run or in the workload's cgroup, one ring length, a socket from init), with Linux v6.18's io_uring, cgroup-v2, pagemap and `MemAvailable` sources. |
| R03 | Section 16's numbers: every literal derived (the ring's bounds from a page and the iovec limit, the learned length's doubling, the table from the descriptor limit and `IORING_MAX_REG_BUFFERS`, the control bytes' reads from what each side says between them) or a protocol constant; none chosen. The learned ring's cold start and its adversarial bound (the run's own room) are recorded. |
| R04 | Typed states and outcomes: the device's `Ringing`, `Why`, `Guest`, `Owed`, `Next`, `Bytes`; the relay's `Turned`, `Swapped`, `Disposal`, `Offered`, `Up`, `Joined`, `Unmade`; refusals typed (`Refusal::Ended` new). Tested by name (r3): the device's attach, refusal, the relay's `DETACH`, the host's end, a swap (whole, in pieces, crossing the host's end), a broken byte; the relay's hello (ringed, refused, plain), its let-go for the room and for a give-back, a swap, a slot with no word of its device. Gap: the device's per-direction flags in `Ringed` (version 1's) stay booleans. |
| R05 | No cache: freed slots are not pooled (no measured benefit). Waits are notifications: the relay's watcher sleeps on init's eventfd, a stream on poll(2) of its sockets and its own eventfd; a refused growth is asked again only once room may have come back (`room_gen`), not on a timer. The learned ring length is one value the pool owns. |
| R06 | The relay's pump is one loop with explicit outcomes; the swap and the settle are their own functions. Reviewed; the pump stays long (one turn of two directions). |
| R07 | Comments carry the safety obligations, the sources and the derivations. |
| R08 | No performance claim; the measurements are owed. |
| R09 | The first-request hang reproduced before its fix (r1); each E2E failure reproduced live and its cause shown before its fix (r6 to r12), the stdin hang by an A/B with rings off and the daemon's own steps (r9); version 1's idle pins are its source's behavior, their live red owed with the E2E (above). |
| R10 | r1-r16, the guard mutations, this record. |
| R11 | Verified: the four E2E tests on Linux KVM x86_64 (r13, r16), HVF arm64 and Linux arm64 KVM in L1 (r16), each test on its host's own pipe and budget, none skipped; on WHP x86_64 give-back and shrinking room twice, the bulk test once in two (r16); unit tests on Linux x86_64 (abi, the vsock device, init and its relay), macOS arm64 (abi, the vsock device, init's non-Linux part; the relay is the Linux guest's alone) and Windows x86_64 natively (the vsock device, 39; `served_pipes_tests`, 5); scripts/lint of all seven supported targets (r15, r16). Unverified: WHP arm64; on WHP x86_64 the pins test past 13 connections and the bulk test's stop; owed above. |
| R12 | One device for every hypervisor (`GuestMemory`'s pins); the relay is the Linux guest's. No platform copy. |
| R13 | Version 1's slot pool removed; the E2E reuses the engine tests' run and emitter helpers. |
| R14 | No profiling or benchmark (ROOT's). |
| R15 | Implementer's light tests, and the real-VM E2E ROOT gave this work (r13, r16) through a run-only agent on jupiter for WHP; measurements owed. Merged to main on the person's explicit instruction, which supersedes R15's "do not merge themselves"; the owed checks above stay owed. |
| R16 | Nothing of anyone else's deleted. This work's scratch on adacachy and kiln-a094a12d (~/g121-tmp/ring-pins-* and ringe2e*: target directories, clones, bundles, logs), on jupiter (the agent's clone, target and logs) and on this Mac (target directories, a transfer repository, scratch logs) removed as it ends. The Mac's disk filled while r1-r5 ran (not from this work's writes); ROOT reclaimed it; this round kept it above 30 GiB free. |

## Review (plan step 2)

The coordinator's review of this lane, its findings (F1 fixed in b81d707a9; F2, F3 and F4 owed), its
R01-R16 account and the real-VM runs owed: [review.md](review.md).
