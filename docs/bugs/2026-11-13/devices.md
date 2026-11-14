# Devices adversarial review

POLICY_READ: role **adversarial reviewer**, area **devices**. Head and current local `origin/main`: `2bc5f78f9e48d09ddbf4e848f9960133f6d19148`; base `79caf2d74`. Read `/tmp/g121-shot/review/brief.md` in full first, followed by all four policy files in full.

| Policy file at head | SHA-256 |
|---|---|
| `AGENTS.md` | `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41` |
| `docs/development-rules.md` | `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3` |
| `docs/development-evidence.md` | `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63` |
| `CLAUDE.md` | `588b00202791c500679393b9e6cda023d4cc233995198fb575806c25e5438010` |

ROOT/Claude g121 (`01a148884a4770fdaf0111f3f792a248`) remains the main Shards coordinator and owns live reproduction, validation and fixes. This reviewer used three scoped read-only reviewers for VFIO/IOMMU, GPU/device rules, and independent disk-path refutation; each acknowledged the same policy/head before review. The review changed no code, tracked files, commits or branches. At the user's subsequent request, this report was saved to `docs/bugs/2026-11-13/devices.md`; no production code was changed.

Priority mapping for the user's requested notification: **P0 critical; P1 high; P2 medium; P5 low**. No P3/P4/P5 findings are assigned. Counts: **1 critical, 4 high, 5 medium, 0 low** (10 findings).

**Evidence limit:** these are source-verified defects/policy violations with concrete inputs or interleavings, not live reproductions. No builds, tests, VMs, daemons, policy-check scripts, benchmarks or fault campaigns were run, as the binding brief expressly prohibits them. Suggested tests below are sketches for ROOT, not executed tests. File/line references are at head. Review commands were read-only `git diff 79caf2d74..2bc5f78f9`, `git show`, `rg`, `sed`, `nl`, `cat`, `shasum` and `git status --short` in the assigned worktree.

**Notification status:** the main Shards agent's ID/name was confirmed through Kiln. Repeated `agents_send` attempts (steered result, then a queued result) were rejected with HTTP 429 `backpressure`, `pending=3`, `authority=2`. No acceptance/delivery receipt was returned, so notification is **not claimed delivered**. The full report is retained here for ROOT. The user will notify Shards; no further notification attempts were made. The source worktree was clean before this documentation file was added.

## Findings, in severity order

### devices-01 — P0 / critical — Concurrent disk growth can truncate a live filesystem

**Location:** `crates/shards/src/daemon/devices.rs:696` and `:704`.

**Defect:** the monotonic-growth check and `set_len` are not serialized, so two individually valid growth requests can shrink the backing file after the larger request completes.

**Failure scenario:** the plugged disk starts at 64 MiB. Client A requests 128 MiB, reads metadata length 64 MiB, and is descheduled before `set_len`. Client B requests 256 MiB, reads 64 MiB, sets 256 MiB, and completes device and ext4 growth; the guest can write into the new upper half. A resumes and executes `set_len(128 MiB)`, destroying that upper half. The VM's later shrink refusal cannot recover the truncated bytes.

**Evidence/refutation:** each daemon client is handled on its own `spawn_scoped` thread (`daemon.rs:2722`–`:2729`), and the CONTAINER handler directly calls `device_command` (`:2982`). `grow` clones the plugged entry (`devices.rs:685`–`:690`), releasing the map lock before opening/checking/modifying its file. The code is `let now = opened.metadata()?.len()`; `if bytes < now { return Err(...) }`; then `.set_len(bytes)`; only afterward `ask_device(...GROW...)` and guest filesystem growth (`:696`–`:711`). There is no disk transaction lock in this path. The VM dispatcher serializes its own DEVICE steps, which does not serialize the preceding host file truncations. D214 promises a shrink is refused because it takes sectors from under a filesystem; this schedule violates that promise. Introduced in the review range.

**Confidence:** high for the race and destructive effect; no live reproduction.

**Fix direction:** serialize grow/unplug/replug by stable disk identity across the metadata check, host resize, device update and guest filesystem update; retain the backing file's identity instead of relying only on its stored path.

**Catching test:** pause A immediately after metadata acquisition, complete B, write a sentinel above 128 MiB, resume A, and assert both file length and sentinel survive. Also verify A is refused or resolves to the current larger size.

### devices-02 — P1 / high — A full-span IOMMU mapping makes translation loop until allocation failure

**Location:** `crates/vmm/src/devices/virtio/iommu.rs:508`.

**Defect:** `last - at + 1` can overflow to zero for a guest-accepted mapping, leaving the translation loop making no progress while continually growing a host vector.

**Failure scenario:** attach an endpoint to a domain, then submit MAP with `first=0`, `last=u64::MAX`, `phys=0`, flags READ|WRITE|MMIO. Program its MSI address to zero and trigger an interrupt. `translate(endpoint, 0, 4, true)` repeatedly pushes `(0, 0)`. A normal release build exhausts memory; a build with overflow checks panics.

**Evidence/refutation:** MAP checks alignment using `last.wrapping_add(1)` (`:602`–`:604`), which makes this range aligned, and `phys.checked_add(last - first)` succeeds (`:608`). `mirror` returns `Ok(())` for MMIO before calculating its span (`:206`–`:211`), so even a real VFIO follower does not reject this request. MAP inserts it (`:628`). `msi_target` calls `translate(..., address, 4, true)` before destination validation (`:481`); `Msis::send` reaches it at `:711`, as does VFIO's translated route. At `:508`–`:510`, `take=0`, `parts.push` repeats, and `at` stays zero. A hot-plugged emulated device behind the IOMMU also supplies a reachable endpoint without a DMA follower. Arithmetic predates base; the new hot-plug MSI translation exposes another route to it. No arbitrary host interrupt injection is needed for the failure.

**Confidence:** high; source arithmetic and callers verified, no live reproduction.

**Fix direction:** make span arithmetic checked or express the bounded remaining span without overflow, require positive progress, and consistently handle/reject a full-address-space mapping before publishing it.

**Catching test sketch:** `attach(1, endpoint, 0); map(1, 0, u64::MAX, 0, READ|WRITE|MMIO); translate(endpoint, 0, 4, true)`. Assert a typed refusal or bounded correct result. Run the old path only in a resource-bounded child with a completion watchdog; do not allow the infinite allocation into the shared test process.

**Status (2026-11-13):** fixed in 2d17282b6765a51ccc70572a30a0ce552ebcc3a0 (red `full_span_mapping_translates_an_msi_without_overflow`, Kiln r4; green IOMMU tests 11/11 and VMM clippy, r6, macOS arm64). Evidence: `docs/research/measurements/device-fixes/README.md`. Real-VM MSI validation owed to ROOT.

### devices-03 — P1 / high — IOMMU response descriptors control an infallible host allocation

**Location:** `crates/vmm/src/devices/virtio/iommu.rs:813` and `:827`.

**Defect:** the device allocates the guest's entire offered writable capacity before validating those buffers or bounding the allocation by the actual protocol response.

**Failure scenario:** on the 64-entry request queue, supply one short readable request and 63 writable descriptors, each `len=u32::MAX`, with addresses outside a small guest's memory. The device requests roughly 252 GiB of zero-filled host storage before the first invalid write is rejected. Under a finite address-space/commit allowance, allocation failure aborts the VM process rather than returning a device error.

**Evidence/refutation:** `total` sums writable lengths at `:813`; `vec![0u8; total]` occurs at `:827`; `mem.write` validates descriptor memory only at `:840`. Queue construction bounds the rings/table and chain count, but `ChainBuilder::push` copies data `addr` and `len` without validating the described buffer (`queue.rs:244`–`:261`). The reply needs only protocol-defined properties/tail; excess offered capacity need not be materialized as one host buffer. Present at base as well as head.

**Confidence:** high for the unbounded infallible allocation and order of checks. Demand-zero allocation can reserve virtual space without touching every page: **no Linux/macOS RSS spike or inevitable OOM is claimed**. OS-specific allocation outcomes were not run.

**Fix direction:** validate data spans before processing and scatter the small protocol response/required padding in bounded chunks, using fallible allocations where needed.

**Catching test:** feed oversized invalid writable descriptors under an allocation observer or bounded child-process allocation budget. Assert no allocation proportional to offered capacity and a device error without process termination.

**Status (2026-11-13):** fixed in ed428676f16b70033b168a842e1b147048e508a3 (red `invalid_response_is_rejected_before_applying_the_request`, r9; green 13 IOMMU tests and VMM clippy, r10, macOS arm64). The red proves late buffer validation/state mutation; the original allocation-abort/RSS scenario remains unrun and owed to ROOT. Staging is fallibly bounded by this host's page size, independently of offered response capacity.

### devices-04 — P1 / high — Reset can strand a vCPU and teardown behind years of I/O debt

**Location:** `crates/vmm/src/devices/virtio/block.rs:885`; charging at `:982`.

**Defect:** reset joins a worker waiting on the I/O limiter without cancelling or waking that limiter wait.

**Failure scenario:** with a 1-byte/s I/O limit, submit a T_IN claiming a 1 GiB data buffer outside guest memory/disk range, plus a valid status byte. It accrues about 34 years of debt before failing range validation. A second request enters the limiter's wait. Another guest vCPU writes device status zero: reset waits for the worker, and host VM teardown then waits for the stuck vCPU. A valid first request also creates the issue with a shorter delay.

**Evidence/refutation:** `execute` calls `limit.take(sum(descriptor.len))` before `transfer` checks range/memory (`block.rs:982`, `:1084`–`:1087`, `:1113`). `Limiter::take` waits on `Condvar::wait_timeout` while no pause exists (`limit.rs:122`–`:131`). `Block::reset` calls `stop` immediately (`block.rs:884`–`:889`); `Worker::stop` only sets a flag, unparks its thread, then joins (`worker.rs:71`–`:74`). An unpark does not notify this condvar. In contrast, `Block::pause` explicitly calls `limit.pause` first (`block.rs:897`–`:901`). Teardown joins vCPUs before `bus.pause` (`vm/runtime.rs:1151`–`:1165`), so the later pause cannot rescue this schedule. Reset/limiter behavior predates base; plugged PCI disks inherit it via the new status-write path in `virtio/pci.rs:397`.

**Confidence:** high for the wait dependency and debt amplification; no live reproduction.

**Fix direction:** give limiter waits cancellation/readiness notifications, validate charges before accruing debt, and make reset wake in-flight waits before joining workers without broadening another device's authority.

**Catching test:** deterministically accrue debt, observe the worker entering the limiter wait, issue status reset, and assert reset and subsequent VM teardown complete without waiting for the debt deadline. Include ordinary valid I/O and hostile invalid lengths.

### devices-05 — P1 / high — Capacity and generation are not published as one coherent configuration

**Location:** `crates/vmm/src/devices/virtio/block.rs:119`–`:120`.

**Defect:** publishing capacity before its generation allows a guest to accept a torn 64-bit capacity; independent concurrent growers can also overwrite a newer capacity with an older one.

**Failure scenario:** capacity changes from `0x00000000ffffffff` to `0x0000000100000000` sectors. The guest reads generation 0 and the old low word. `grow` stores the new capacity, then pauses before incrementing `grown`. The guest reads the new high word and generation 0, accepting `0x00000001ffffffff` sectors, almost twice the real capacity. No concurrent host growers are needed for this case.

**Evidence/refutation:** the two separate atomics are updated by `capacity.store(...Release)` then `grown.fetch_add(...AcqRel)` (`:119`–`:120`). Config reads obtain capacity through `Backend::capacity`, while `config_generation` separately loads `grown` (`:855`–`:860`); MMIO exposes their sum with its own generation (`mmio.rs:272`). The transport mutex does not cover `DiskSize::grow`. Acquire/release ordering does not eliminate the interval between the stores. D214 specifically promises generation detects split capacity reads across a growth. Additionally, public cloned `DiskSize` handles can both load old capacity 1 at `:110`, then store 3 followed by stale 2 at `:119`; the CLI's serial VM dispatcher prevents that particular library race in its own caller, not the guest-read race. Introduced in range.

**Confidence:** high; no live reproduction.

**Fix direction:** use a configuration publication protocol that coordinates data reads with generation changes and serializes monotonic capacity changes. A monotonic atomic update alone does not solve the torn-read protocol.

**Catching test:** barriers around capacity publication and generation update, with the guest's generation/low/high/generation sequence arranged as above; assert it retries or reads a coherent capacity. Separately race cloned `DiskSize` growers and assert capacity never decreases.

### devices-06 — P2 / medium — VFIO hot-unplug retains MSI routes

**Location:** `crates/vmm/src/devices/pci/vfio.rs:2114`.

**Defect:** unplug removes BAR mappings and the VFIO file without unregistering active MSI/MSI-X routes.

**Failure scenario:** a guest enables MSI/MSI-X, clears COMMAND_MASTER while leaving interrupts enabled, and the host unplugs the function. Repeated authorized plug/unplug cycles accumulate stale routes and eventually consume the running VM's interrupt-routing capacity.

**Evidence/refutation:** `held` checks only COMMAND_MASTER (`:2098`–`:2104`), so this state is accepted. `VfioFunction::unplugged` calls only `map_windows(false)` (`:2114`–`:2116`); its wrapper removes the hypervisor's file (`:2173`–`:2180`). No relevant destructor invokes `intx_off`/`msi_off`/`msix_off`. Per-slot route objects outlive the function. Routes are appended at `vm/x86_64.rs:312` and `vm/aarch64.rs:336`, and removed only by `msi_off` (`x86_64.rs:316`–`:323`; `aarch64.rs:340`–`:346`). VM-wide MSI routing vectors retain the entries (`hv/kvm/x86_64.rs:388`–`:404`). Closing an eventfd may remove its kernel irqfd registration, but does not remove these userspace route vectors; no claim of a permanent kernel eventfd leak is made. New hot-unplug behavior.

**Confidence:** high for route retention, unmeasured time to exhaustion.

**Fix direction:** quiesce and explicitly unroute all interrupt modes while their descriptors remain valid, then remove DMA/BAR mappings and the device file; propagate cleanup failures with a defined retained/refused state.

**Catching test:** enable MSI/MSI-X, clear bus mastering only, unplug and replug repeatedly. Assert the route inventory returns to baseline each time, including FD-number reuse and masked-vector cases.

### devices-07 — P2 / medium — Deep joins size /dev/shm from the wrong VM

**Location:** `crates/shards/src/daemon/inspect.rs:632` and `:648`.

**Defect:** VM-owner traversal silently stops after 64 links and uses the intermediate run's memory budget for the actual /dev/shm mount.

**Failure scenario:** start c0 with a 512 MiB memory limit; start c1 through c65 with `--network container:c(previous)`, default private IPC and no own memory limit. `microvm_shm(c65)` follows 64 links to c1, then uses the host-derived unlimited maximum, instead of c0's VM maximum. The guest mount is sized to half the host maximum although it lives in c0's limited VM.

**Evidence/refutation:** stored requests preserve their original `network` (`daemon.rs:3687`–`:3694`); live joins flatten `Tracked.joined` to the actual provider (`:7607`–`:7609`), permitting this chain to run. Default private IPC bypasses `shared_ipc` traversal (`:7414`), and ordinary CLI runs bypass the Engine-only `joiners_net` call (`:3801`). The `for _ in 0..64` in `microvm_shm` ends without a refusal, then calls `sizes` on the intermediate resources. Its result reaches actual setup (`daemon.rs:3773`, `setup.rs:657`, `crates/init/src/setup.rs:557`), not just inspect output. `run::sizes_for` uses host memory for a run without a memory limit (`run.rs:1006`–`:1009`). D156 requires half the owning microVM's maximum. The arbitrary traversal cutoff also violates R03, and silently unresolved ownership violates R04. Introduced in range.

**Confidence:** high for incorrect sizing; no host-memory isolation escape claimed, because VM/cgroup accounting still constrains actual writes. No live chain was started.

**Fix direction:** resolve the authoritative VM owner directly, or traverse with cycle detection and a container-count-derived bound, returning a typed error if ownership cannot be established.

**Catching test:** persist 66 linked requests with unequal root/intermediate limits and assert c65's result equals `shm_bytes(sizes(c0).2)`; ROOT's E2E should compare guest statfs with c0's maximum.

### devices-08 — P2 / medium — Device paths retain fixed budgets without required derivation

**Locations:** `crates/shards/src/daemon/devices.rs:97`, `:334`; `crates/shards/src/daemon.rs:134`; `crates/vmm/src/devices/virtio/gpu/mod.rs:53`; `crates/vmm/src/devices/virtio/iommu.rs:45`; `crates/vmm/src/devices/virtio/block.rs:186` (`THROUGH_MAX`).

**Defect:** these paths use fixed deadlines and chosen capacities rather than the specification/data-derived or online-learned values the binding R03 requires.

**Evidence/scenarios:**

- New hot-plug `GUEST_TIMEOUT = Duration::from_secs(30)` governs discovery, mount, remove and resize via `exec_quietly`; the same call fixes captured output at `1 << 12` bytes. D214 supplies no derivation or cold-start/failure model for either. A correct guest operation taking over 30 seconds on a contended/cold host is rejected based on this fixed elapsed time.
- Every run defaults to four PCI slots (`daemon.rs:134`). D214/M430 measure a four-slot bus's cost but do not derive why every run gets exactly four or provide demand-based sizing. A fifth concurrently plugged device is refused despite free PCI bus capacity. An environment override does not derive the default.
- GPU's 256-entry control queue is justified only as QEMU/libkrun's choice (`gpu/mod.rs:50`–`:53`); IOMMU's 64-entry request queue has no derivation. The architecture's queue audit explicitly lists these, block's and fs's 256-entry queues as “unchanged: owed” (`architecture.md:3768`). These inherited-at-head values are not specification maxima or learned traffic needs.
- The new block relay's `THROUGH_MAX = 1 << 20` chooses a 1 MiB payload ceiling. Its protocol is locally defined rather than an external invariant, and PM M442's batching measurement does not derive this chosen value or learn it where it runs.

**Confidence:** high as a binding-policy violation; deadline/resource effects are source-based, not measured. This is an inventory of the fixed choices identified in the reviewed paths, not a claim that every literal throughout the repository was audited.

**Fix direction:** derive actual protocol/storage bounds where they exist, and use measured/learned operation progress and demand for the remaining budgets. Record cold-start, uncertainty and adversarial behavior in the applicable D/PM entries; coordinate changes affecting snapshots and renderer queues.

**Catching tests:** a slow-but-progressing cold plug should not fail at an unexplained elapsed time; queue/slot choices should respond to the documented demand model; relay chunk bounds should have overflow/protocol-bound tests. No arbitrary replacement timeout or capacity is recommended.

### devices-09 — P2 / medium — GPU catalogs have an explicitly unjustified 65,536-device ceiling

**Location:** `crates/abi/src/gpu.rs:22`; enforced at `crates/vmm/src/devices/virtio/gpu/virgl.rs:649`.

**Defect:** GPU selection enforces a copied scheduler-index ceiling despite the design explicitly acknowledging that it lacks the required derivation.

**Failure scenario:** Vulkan reports 65,537 physical devices; startup refuses the catalog before examining identities, although Vulkan count fields are u32 and this is below the transport's own checked payload-count limit.

**Evidence/refutation:** `MAX_PHYSICAL_GPUS = 1 << u16::BITS`; `virgl.rs:649` rejects counts above it. D232 (`architecture.md:25260`–`:25274`) says the renderer figure comes from Hex's u16 scheduler index and is “still owed its derivation (R03).” It identifies the transport's actual count bound as `UINT32_MAX / 33`, rather than 65,536. Matching the C patches prevents a Rust/C disagreement, but copying an unjustified bound into both is not R03 evidence. Introduced in range.

**Confidence:** high as a policy violation and source-proven refusal; no such large device catalog was available or simulated live.

**Fix direction:** derive bounds consistently from the actual queried catalog, checked allocations and transport encoding in both Rust and renderer patches; ROOT must validate the resulting renderer with real GPUs.

**Catching test:** enumerate counts at the genuinely derived transport/allocation boundary with overflow witnesses; do not merely mirror the existing u16 literal in an assertion.

### devices-10 — P2 / medium — New hot-plug refusals lose their types across the control wire

**Location:** `crates/shards/src/vm_run.rs:935`; `crates/shards/src/warm.rs:848`–`:850`.

**Defect:** new device failures are formatted strings carried as a generic failed-status byte rather than typed causes through the control/wire path, contrary to R04.

**Failure scenario:** slot exhaustion, device-still-driven refusal and an actual host/VFIO failure all reach a caller as generic failure plus prose; the caller cannot match stable cause, identity or retry semantics. Changes to presentation text change the only available discriminator. The new tests consequently check substrings for refusal kinds (`tests/hotplug.rs` and `tests/plug.rs`).

**Evidence/refutation:** `vm_run::device` returns `Result<String, String>`; `Hotplug::plug/unplug`, disk `grow` and new runtime plug methods also return String errors. The DEVICE handler serializes every error as `[number, 1, e.as_bytes()]`, losing any internal error distinctions. `ask_device` reconstructs a string error; `guest_device` similarly collapses nonzero guest status into text (`daemon/devices.rs:333`–`:339`). Existing `PassthroughRefusal` variants do not repair the other new device refusal paths. These are new interfaces, not merely presentation code over a retained typed error. D214 describes behavior but does not authorize waiving R04.

**Confidence:** high for the explicit policy violation; no unproven claim that callers currently parse every message.

**Fix direction:** introduce scoped device refusal/result variants with affected device identity and cause/retry semantics; encode and decode them on DEVICE/DEVICED, formatting text only at presentation.

**Catching test:** round-trip `NoSlots`, `StillDriven`, `ShrinkRefused` and host failure variants across the actual codec, then assert callers match variants while presentation wording can change independently.

## Checked and found sound

- Split virtqueues: sizes/alignment and ring spans checked at construction; direct and indirect descriptor walks bounded by queue size, indirect nesting rejected, each descriptor copied once; available-index jumps bounded and used/event indices use explicit wrapping arithmetic. This does not validate every device consumer, as devices-03 demonstrates.
- ECAM: supported access widths and register alignment checked before dispatch, bus constrained to zero, missing functions read all ones. Movable BAR registers mask addresses to BAR size; plugged virtio BAR claims use checked subtraction/end. Overlapping emulated BARs are resolved within the VM's window and no direct host-address escape was established.
- Emulated hot-unplug retains Arc ownership across dispatched BAR/config accesses, avoiding a straightforward freed-object dereference. Normal virtio reset joins its worker; cancellation gaps are separately identified above. This is source inspection, not a lifetime stress result.
- VFIO DMA followers resolve GPA/length through `GuestMemory::host_ptr` before obtaining a host pointer for the mapper. No path examined lets a guest directly supply an arbitrary host virtual address to host DMA mapping.
- Passed-through MSI-X capability spans are validated; whole pages containing MSI-X table/PBA are excluded from direct BAR windows. x86 interrupt routes validate the guest APIC window, and arm64 uses guest ITS/requester identity. No raw host interrupt injection was established.
- Hot-plug disk/share requests use existing I/O limits. A plugged share explicitly has no DAX window. VFIO without a guest IOMMU refuses memory hot-plug/free reporting configurations that would invalidate its pinned all-RAM mapping.
- GPU frontend parsing checks copied request lengths, entry counts and submit sizes; backing entries pass through guest-memory bounds. Blob maps check alignment, checked end, window containment and overlap. Linux blob ownership validates descriptor kind/page-rounded backing length and retains OwnedFd.
- D44: reviewed CDI build-step rules add actual character/block devices with their requested permissions; FIFO nodes receive no rule. Partial `a` rules split into corresponding char/block rules, avoiding runc wildcard expansion. No wider-than-requested rule was established in those reviewed changes.
- D214/M430's hotplug and plug tests exercise sequential disk/share lifecycle, real read/write persistence, growth and normal removal; their source assertions are substantive. They do not exercise the concurrent growth schedule, torn publication, hostile reset debt or enabled-interrupt unplug schedules above. Historical PM results were read as historical evidence, not rerun or attributed to head.

## Not covered

- Every live symptom, allocator/kernel outcome, cleanup count, forbidden-effect witness, mutation check and proposed test remains unrun. R09/R10 live reproduction is owed to ROOT; this review implements no fixes and claims no passing validation. The brief's explicit no-tests rule also prevented `scripts/check-development-policy`.
- No live KVM, Hypervisor.framework or WHP execution; no Linux glibc/musl, Windows amd64/arm64 or macOS arm64 matrix runs. Intel Mac remains unsupported. Windows assigned-function paths were not exhaustively reviewed.
- No hardware VFIO/IOMMU failure injection, interrupt-remapper behavior, mapping-unmap failure recovery, in-flight DMA teardown race, fd/mapping census, leak benchmark, or route-exhaustion threshold measurement. No conclusion of leak-free or escape-free operation.
- The upstream Venus C decoder/driver attack surface and every bundled renderer patch were not exhaustively audited. GPU `Served::unmap/reset` log and discard window-unmap failures (`gpu/mod.rs:562`, `:575`); an actual guest-triggerable backend failure and its consequences were not established, so this is a fault-validation gap, not an additional claimed escape.
- Full virtio-fs server pathname/DAX-window security lies beyond the focused plug review. Slow or never-answering share-server reset/teardown needs live validation; no additional finding is claimed from a speculative blocked server.
- No comparative performance/profiling, allocations/page-fault measurements, chaos campaigns, seven-target builds, or blanket R01–R16 compliance assertion. R02/R03 evidence gaps found are reported above; R04 state/error gaps are reported; R05 waits were traced; R08/R14 measurements and R11 platform gates remain coordinator work. R06/R07/R12/R13 were not converted into subjective style findings. R15 work remained read-only; R16 required no cleanup.
