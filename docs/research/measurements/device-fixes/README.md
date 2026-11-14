# Devices review fixes

POLICY_READ: implementer, devices-02 through devices-10; base
`9efde72524dd626f3ff84e6b9891852d7c810a9b`; rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
The four policy hashes are recorded in `docs/bugs/2026-11-13/devices.md`.
Task authority: `/tmp/g121-shot/review/fix-brief.md`, then
`/tmp/g121-shot/brief-common.md`. devices-01 belongs to ROOT's Claude lane.

## devices-02

Red revision `f7cbffbb3543c41eaa1a9141cd79d04485561790`, Kiln run r4,
adas-mac-studio-3.local (Darwin arm64). Command:
`CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=$HOME/g121-tmp/fix-devices/target cargo test -p shards-vmm full_span_mapping_translates_an_msi_without_overflow --lib`.
One test failed at iommu.rs:508:24 with `attempt to add with overflow`;
488 filtered out; exit 101. This drives the accepted MAP and MSI translation.
The release zero-progress loop was not executed because it would exhaust memory.

The fix changes only inclusive span arithmetic: while `at < end`,
`end-at-1` is defined; after `last >= at`, `last-at` is defined. Their minimum
is at most `end-at-1`, so adding one is representable and strictly positive.
The mapped physical offset remains bounded by MAP's existing checked physical
end. Source: virtio 1.3 §5.13.6.6. Green revision `2d17282b6765a51ccc70572a30a0ce552ebcc3a0`, Kiln r6 on the same Mac: `cargo test -p shards-vmm devices::virtio::iommu::tests --lib`, 11 passed, 478 filtered out, exit 0; `cargo clippy -p shards-vmm --all-targets -- -D warnings`, exit 0. Both use the same CARGO_INCREMENTAL/target settings as red. Merged-tree r7 (6211b3a0e) and r8 (e3254587c after merging main b1415acec) both passed the same 11 tests and clippy. Slice landed ff as e3254587c. ROOT owes `cargo test -p shards --test hotplug a_hosts_function_is_plugged_into_a_running_microvm` on an assigned KVM VFIO fixture, plus malicious full-span MAP/MSI from the guest. No release OOM loop was run.

Policy accounting: R01 checked arithmetic invariants above; R02/R03 protocol
representation and bounded arithmetic, no new runtime tuning values; R04 existing
MAP status and Option translation preserved; R05 no wait/cache change; R06/R07/R13
local arithmetic change with invariant comment; R08/R14 no performance claim or
profiling (ROOT owns these); R09/R10 focused unit red/green; R11/R12 shared
implementation, one allowed host tested, other matrix and real-VM runs owed;
R15 no subagents, signed commits and focused tests only; R16 own target removed
at completion. No unrelated cleanup, benchmark, chaos or E2E run.

## devices-03

Red revision `c844626be2e2abca940971e550ad7ac787e9e53e`, Kiln r9 on
adas-mac-studio-3.local Darwin arm64:
`cargo test -p shards-vmm invalid_response_is_rejected_before_applying_the_request --lib`.
One failed: the invalid writable span had already attached endpoint 8 to a domain;
489 filtered out. This is a bounded red for validation order, not an allocator/RSS
measurement. We deliberately did not allocate gigabytes on a shared host.
The original infallible guest-sized allocation is unambiguous in source; its
actual commit-limit abort remains owed to ROOT with an allocation-constrained
child fixture invoking `answer` with a 64-descriptor hostile chain.

Green `ed428676f16b70033b168a842e1b147048e508a3`, Kiln r10 on the same host:
`cargo test -p shards-vmm devices::virtio::iommu::tests --lib`: 13 passed,
478 filtered out; `cargo clippy -p shards-vmm --all-targets -- -D warnings`: exit 0.
All commands use CARGO_INCREMENTAL=0 and the lane's target directory.
New positive coverage streams PROBE properties, padding and status across a
split descriptor and a host-page boundary. The used length is checked as u32
(virtio §2.7.8); every nonempty writable span is validated through `io_ptr`
before state changes; scratch reservation is fallible and no larger than the
host's queried page size (platform::page_size). This covers RAM and device-memory
spans; Access::write retains fault-checked device-memory copies.
No selected chunk constant, cache, timeout, new state or OS-specific twin.
The R01–R16 accounting above applies; source establishes the staging bound,
not a measured allocation-free or performance claim. Merged source checks at c503ec56 and 54dcaa49 also passed all 13 tests and clippy. The pushes were overtaken by concurrent main landings; no force push was attempted.

### devices-03 scratch-failure follow-up

Red `03ccb5eab`: `cargo test -p shards-vmm padded_probe_response_streams_across_descriptors --lib`
on the same Mac, exit 101: a deliberately impossible test-only reservation
returned zero bytes instead of the 16,388-byte padded reply. This exercises
reservation failure without allocating gigabytes or changing the production
chunk bound. Captured output: `reply-scratch-red.log`.

Green `18f3a90ad`, Kiln r16: 13 IOMMU tests passed and VMM clippy clean.
Output: `reply-scratch-green.log`. A protocol-derived stack buffer of PROBE's
properties plus request tail completes replies if page-size lookup or scratch
reservation fails. It introduces no new failure/refusal or selected fallback
size. The malformed-span guard and split/padded replies also pass.
Source policy accounting remains as above; host allocator/RSS measurements and
real-guest invalid-buffer/MSI tests remain ROOT's.

## devices-04

Red `74924eba3`: `cargo test -p shards-vmm reset_cancels_only_its_own_limiter_wait --lib`, 1 failed (reset still waiting). The worker consumes an actual published block read; the test waits for limiter admission, resets on another thread, and releases the wait on watchdog failure before joining every actor. No disk large enough for resource exhaustion is allocated.

Green `de1da2168`: `cargo test -p shards-vmm devices::virtio::block::tests --lib` 7 passed; `cargo test -p shards-vmm devices::virtio::limit::tests --lib` 4 passed; `cargo clippy -p shards-vmm --all-targets -- -D warnings` clean. Host adas-mac-studio-3.local, Darwin arm64; CARGO_INCREMENTAL=0, lane target. Logs reset-red.log/reset-green.log. Cancellation publishes under the condvar mutex, wakes waiters and checks an owned atomic token; shared debt and pause count stay unchanged. A new activation clears its token only after the old worker joined. Invalid read/write spans and disk bounds are checked before charging.

R01/R04 cancellation returns a typed bool internally and block status IOERR on cancelled work; R03 no production deadline or chosen number added; R05 affected condvar wait no longer depends on debt expiry; R06/07/13 local methods, ownership comments; R09/10 real unit red/green; R11/12 shared code, no platform fork; R08/14 no performance claim; R15 bounded focused tests, no subagents/VMs; R16 lane target removed at finish. ROOT owes the hotplug real-VM test in the finding status and reset during active limited I/O on KVM/HVF.

## devices-05

Red `91dfe52ad`, `cargo test -p shards-vmm capacity_and_generation_are_published_together --lib`: 1 failed; reader observed capacity 2 with generation 0 while the writer paused before generation publication. The fixture uses a one-sector disk grown to two sectors, not a multi-terabyte file. Every thread is released/joined before assertion.
Green `a8628ac21`, `cargo test -p shards-vmm devices::virtio::block::tests --lib`: 8 passed; VMM clippy all-targets `-D warnings` clean. Host adas-mac-studio-3.local Darwin arm64, CARGO_INCREMENTAL=0, lane target. Logs capacity-red.log/capacity-green.log. The state mutex spans file metadata acquisition, monotonicity check and capacity/generation publication. Capacity and generation reads each take it; a reader spanning the whole update sees a changed generation, not an intermediate publication. Generation wraps per virtio config-generation semantics; no odd-generation protocol is introduced. R01–R16 accounting: shared simple state, no new tuning values/waits/cache/platform forks; red/green as above, no performance/VM claim. ROOT owes real split config reads during online disk growth on KVM/HVF (`cargo test -p shards --test hotplug devices_are_plugged_into_a_running_microvm_and_its_disks_grow` plus concurrent config-reader fixture).

## devices-06

Linux-only test, adacachy2 x86_64, nice -n 10, cold lane target; no KVM/VM/hardware used. Red 57673d129: `cargo test -p shards-vmm unplug_releases_interrupt_routes_before_closing_eventfds --lib`, 1 failed: removed 0 routes, expected 3. The actual unplug entrypoint runs with real eventfds and a recording Route adapter. A regular-file device fixture intentionally returns ENOTTY for VFIO ioctls: this confirms host route cleanup is attempted even on that ioctl error, not that real hardware stops generating interrupts.
Green e06c9bc9f plus test-adapter correction 9a4be9126: `cargo test -p shards-vmm devices::pci::vfio::tests --lib`, 2 passed; `cargo clippy -p shards-vmm --all-targets -- -D warnings`, passed. Initial test adapter clippy failed on assertion in Result method and missing safety comment; corrected without suppressing lint. Log vfio-green.log. Cleanup calls existing INTx/MSI/MSI-X disable/unroute paths before eventfds close, idempotently. Existing off-path ioctl/routing errors are logged; this does not establish successful teardown under arbitrary host failures (owed to ROOT). R01–R16 accounting follows above; no new runtime values, platform branch or cache, no performance claim. ROOT owes real-device route-inventory and clear-master-only unplug tests, exact existing command in bug status.

## devices-07

Red `0d6a24c37`: `cargo test -p shards shm_owner_follows_every_join_to_the_vm_budget --bin shards -j 4`: 1 failed, left 0 vs expected 536870912. Green `4eaefdf9c`: `cargo test -p shards daemon::inspect::tests --bin shards -j 4`, 3 passed; `cargo clippy -p shards --all-targets -j 4 -- -D warnings`, passed. Host adas-mac-studio-3.local Darwin arm64. Same CARGO_INCREMENTAL=0 and lane target; SHARDS_HELPERS=skip, SHARDS_INIT_BINARY=/usr/bin/true, SHARDS_SERVER_BINARY=/usr/bin/true disable guest/helper builds for these host-only unit tests. Initial invocation used --lib incorrectly (package has only a bin); corrected before collecting red evidence. No daemon, VM, server or guest started. Logs shm-red.log/shm-green.log.

Takeover (Claude, 2026-11-14): the red and fix were cherry-picked onto main 97f404be8 as 6ea0a8463 and ad8308bb8; 8df184aa2 keeps listings whole: a summary (ps, system df, a scope's list) shows no ShmSize, so it no longer fails as a whole for one run whose owner record went, while start and inspect refuse it typed. Rerun on adas-mac-studio-3.local Darwin arm64 (kiln r1768, `devtk-07-11.log`): red 6ea0a8463 1 failed (0 vs 536870912); green 4 inspect tests and `cargo clippy -p shards --all-targets -- -D warnings` clean. The red is also the guard's mutation: a walk with a 64-link bound in place of the visited set fails the same test.

Traversal follows the real production callback for Engine/network ownership with a visited set scoped to the lookup; its capacity is fallibly reserved from encountered owners, not a picked link count. IPC ownership uses the same walk. Cycles/missing records/storage errors are typed through sizing and converted at inspect/start presentation. Start and inspect propagate failure; a listing's summary computes no ShmSize; no default host budget on traversal failure. The test fixture deliberately uses 66 actual requests. ROOT owes CLI chain E2E: create c0 with --memory 512m, c1..c65 --network container:c(previous), private IPC; compare statfs /dev/shm in c65 against c0's microVM maximum. A pure test proves traversal, not live mount setup. R01–R16 accounting above applies; no runtime deadline/cache/performance claim or platform fork.

## devices-11

Claude takeover, 2026-11-14, implementer, policy AGENTS f66d304c, rules df2acf47, evidence 61613a24. Host adas-mac-studio-3.local Darwin arm64, kiln r1768 (`devtk-07-11.log`), same env as devices-07. Green 84fa0a5e5: `cargo test -p shards --bin shards daemon::devices::tests -j 8`, 9 passed (two new: `a_disk_file_is_plugged_writable_into_one_run_at_a_time`, `a_growth_refuses_a_file_put_in_its_disks_place`); `cargo clippy -p shards --all-targets -j 8 -- -D warnings` clean. Mutation e9427c934 (unsigned, never landed): the conflict check `find(|c| c.writable || writable)` made `find(|c| c.writable && writable && false)`: the first test fails, a second writable claim through a hard link given (`unwrap_err()` on `Ok`). There was no code to be red before: plug had no ownership at all (any number of runs plugged one file writable), shown by reading `plug_one` at 97f404be8; the mutation stands for the red. Windows `os::file_id` (FILE_ID_INFO) is compiled by CI's Windows legs only; jupiter was offline. Real plugs into two microVMs are ROOT's (D214's amendment names them). R01: no new panics (lock poison-tolerant, no indexing); R03: no new literal; R04: typed `DiskHeld`; R05: the claims are the one owner of disk-file holds, keyed by identity, freed as plugs fail, unplug or runs end; no cache.

