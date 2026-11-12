# ring-pins-v1: an engine ring's pins bounded by use and by the run's room

The implementer's evidence for D230's protocol version 2: the decision and its numbers are
section 16 of [engine-ring-channel-decision-v1](../../engine-ring-channel-decision-v1.md),
D230 in [architecture.md](../../../design/architecture.md).

- **Identity.** Branch `p/ring-pins-v1`: 3480f9299 (the first-request fix, on main as
  cc1b52e61), 946eaa687 (version 2), e0b04ab84 (origin/main c4ae59ee0 merged in), and the
  commit adding this record and the pin test. Role: implementer for ROOT
  (01a148884a4770fdaf0111f3f792a248); policy receipt [POLICY_READ.json](POLICY_READ.json)
  (development-rules.md sha256 df2acf47…, re-read on c4ae59ee0 after a restart, unchanged).
  Runner: adacachy, linux/x86_64, CachyOS 7.2.9, rustc 1.98.1. Merged to main at the
  person's instruction (POLICY_READ.json `superseding_instruction`).
- **Owned paths.** crates/init/src/ring.rs, the ring parts of crates/init/src/engine.rs and
  run.rs (the `ROOM` and `RECLAIM` hooks), crates/abi/src/ring.rs,
  crates/vmm/src/devices/virtio/vsock/{ring.rs,conn/ringed.rs},
  crates/shards/tests/engine_ring.rs, the D230 record, this directory; `pub(crate)` on
  init's `plug::cap`, `plug::working_room` and `domains::available_memory`.

## Logs

| Log | What it shows |
|---|---|
| [r1-red-first-request.txt](logs/r1-red-first-request.txt) | Red (R09): version 1's device never heard a ringed connection's first request (0 of 34 bytes), reproduced before its fix; ROOT's real-VM r521 shows the same hang. |
| [r2-green-first-request.txt](logs/r2-green-first-request.txt) | Its fix green, and its guard (the flag's store) removed failing both of its tests. |
| [r3-gate-e0b04ab84.txt](logs/r3-gate-e0b04ab84.txt) | The merged revision: fmt; unit tests (abi, the vsock device, init); clippy -D warnings on x86_64 glibc and x86_64 and aarch64 musl; the policy check; guard mutations M1-M3, each failing for its reason; the E2E file compiled. |
| [r4-gate-final.txt](logs/r4-gate-final.txt) | The pushed revision, with the io_uring pin test: the same gate. |

Guard mutations (r3): M1, the relay freeing its slot without hearing the device's `DETACH`,
fails `the_relay_frees_a_slot_only_after_the_devices_detach` ("the relay let go of its slot
before the device's DETACH"), or, as the timing falls, kills the test process (a core
dump in the gate unit's journal): the device stand-in read and wrote the slot the relay had
unmapped, the use after free the guard exists to prevent. M2, the device saying `DETACH`
with the slot's pins still held, fails five device tests of `DETACH`, swaps and resets. M3,
a slot made whatever the room, fails `a_stream_past_the_room_goes_over_plain_vsock` and
`with_no_budget_pins_are_bounded_by_the_available_memory`. Under M3 the stand-in also met a
`SWAP` after its own `DETACH`, a race the protocol allows (the device refuses such a swap,
`Refusal::Ended`) and the stand-in did not answer: it answers it since r4.

What the unit tests do not reach: the relay's tests run it against a device stand-in, its
slots unpinned in the test's own memory, so pagemap's frame numbers (CAP_SYS_ADMIN only),
the leaf join on a real cgroup, the shared page across init's fork and the daemon's words
are covered only by the real-VM E2E owed below; `a_slots_pins_count_to_the_instances_maker_and_go_with_its_entry`
runs the pin and unpin calls themselves on the runner's kernel.

## Owed to ROOT (none run here)

- E2E, `crates/shards/tests/engine_ring.rs`, on KVM x86_64 and arm64, HVF, WHP x86_64 and
  arm64: `an_idle_microvm_pins_nothing_and_each_streams_pins_go_as_it_ends`,
  `a_give_back_lets_an_idle_slot_go`,
  `a_shrinking_room_lets_an_idle_slot_go_and_a_stream_past_it_goes_plain` (Linux, a user
  systemd manager), `a_microvms_engine_carries_bulk_whole_both_ways`; the engine suite
  (D181), isolation's engine-port token test, the budget and reclaim suites.
- Their reds: the idle-pins test on 032b63b1c (version 1, which pins slots as its relay
  starts), and mutations: `told` removed from `ROOM` (the shrinking-room test must fail), the
  leaf join removed (`read metrics` of the run must lose the slots' bytes).
- Measurements: a slot's making and freeing by ring; connection setup and first-message
  latency, 64 B to 1 MiB, fresh and kept connections, against plain vsock; a bulk stream's
  ramp against a fixed 2 MiB ring; 256 MiB each way; the device worker's CPU; the run's
  memory with pins at 1, 2 and many vCPUs.
- Lint of the other supported targets (macOS arm64, Windows x86_64 and arm64, Linux arm64
  glibc) for the device's code.

## R01–R16

| Rule | Proof, and its gap |
|---|---|
| R01 | Production code returns its errors: `?`, `get`, checked and saturating arithmetic on positions the host writes (each read once and believed only within the ring), poison-tolerant locks, `thread::Builder::spawn`, `try_reserve` for a swap's offer; the panic lints pass with clippy -D warnings (r3, r4). Reviewed by hand for the casts the lints allow (`as` on lengths bounded by `MAX_RING`). Tests assert. |
| R02 | Section 16's alternatives (version 1's pool, pooling freed slots, freeing without the device's word, an idle timeout, pins outside the run or in the workload's cgroup, one ring length, a socket from init), with Linux v6.18's io_uring, cgroup-v2, pagemap and `MemAvailable` sources. |
| R03 | Section 16's numbers: every literal derived (the ring's bounds from a page and the iovec limit, the learned length's doubling, the table from the descriptor limit and `IORING_MAX_REG_BUFFERS`, the control bytes' reads from what each side says between them) or a protocol constant; none chosen. The learned ring's cold start and its adversarial bound (the run's own room) are recorded. |
| R04 | Typed states and outcomes: the device's `Ringing`, `Why`, `Guest`, `Owed`, `Next`, `Bytes`; the relay's `Turned`, `Swapped`, `Disposal`, `Offered`, `Up`, `Joined`, `Unmade`; refusals typed (`Refusal::Ended` new). Tested by name (r3): the device's attach, refusal, the relay's `DETACH`, the host's end, a swap (whole, in pieces, crossing the host's end), a broken byte; the relay's hello (ringed, refused, plain), its let-go for the room and for a give-back, a swap, a slot with no word of its device. Gap: the device's per-direction flags in `Ringed` (version 1's) stay booleans. |
| R05 | No cache: freed slots are not pooled (no measured benefit). Waits are notifications: the relay's watcher sleeps on init's eventfd, a stream on poll(2) of its sockets and its own eventfd; a refused growth is asked again only once room may have come back (`room_gen`), not on a timer. The learned ring length is one value the pool owns. |
| R06 | The relay's pump is one loop with explicit outcomes; the swap and the settle are their own functions. Reviewed; the pump stays long (one turn of two directions). |
| R07 | Comments carry the safety obligations, the sources and the derivations. |
| R08 | No performance claim; the measurements are owed. |
| R09 | The first-request hang reproduced before its fix (r1); version 1's idle pins are its source's behavior, their live red owed with the E2E (above). |
| R10 | r1-r4, the guard mutations, this record. |
| R11 | Verified on x86_64 Linux glibc (tests, clippy) and x86_64 and aarch64 musl (clippy, the guest's crates). The device's code on macOS arm64 and Windows, and every real VM, unverified. |
| R12 | One device for every hypervisor (`GuestMemory`'s pins); the relay is the Linux guest's. No platform copy. |
| R13 | Version 1's slot pool removed; the E2E reuses the engine tests' run and emitter helpers. |
| R14 | No profiling or benchmark (ROOT's). |
| R15 | Implementer's light tests only; real VMs and measurements owed. Merged to main on the person's explicit instruction, which supersedes R15's "do not merge themselves"; the owed checks above stay owed. |
| R16 | Nothing of anyone else's deleted. This work's scratch on adacachy (~/g121-tmp/ring-pins-*: a target directory, a clone, a bundle) is removed as it ends. The Mac's disk filled while this ran (not from this work's writes); ROOT reclaimed it. |
