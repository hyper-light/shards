# hex-upstream-v1's GPU half: review (plan step 2, kiln goal #52, part 2)

Role: implementer, goal 52's GPU child (continuing aaaff3eda43b6cb7d and add157a0923971dbb). Policy: origin/main
f64a75cae, AGENTS.md sha256 f66d304c…, development-rules.md df2acf47…, development-evidence.md 61613a24…, CLAUDE.md
588b0020…. Reviewed on main f64a75cae: 08bb8fda1 f2fe241a3 6accac1e5 bbd221699 83b09843a f57f13582 002ade01a
b20e593c8 1cd728678 8b5fb57c6 1f86d092a, D232's text (55d7bbdcd), every unsafe block they add with its SAFETY
comment, and the identity bridge's C and the renderer patches (resources/gpu).

## Fixed (d7c9938ec, d27cd59b0; red on the code as it was, green on the merged tree, red again with the guard removed)

| Finding | Rule | Red | Green | Guard removed |
|---|---|---|---|---|
| `GpuProbe::parse` (crates/abi/src/gpu/probe.rs, from 84274050f) reserved for every device a receipt's `devices N` line said before reading one: a 59-byte receipt saying `MAX_PHYSICAL_GPUS` asked for 22,020,096 bytes (65,536 × 336-byte `ResolvedGpu`) and was refused a line later. A device line holds at least its UUID's 32 hex digits and its end, so a count the receipt's own bytes cannot hold lines for is refused (`Syntax`) before anything is reserved | R14, R01 | `a_gpu_receipt_reserves_for_no_device_its_bytes_cannot_hold` (crates/abi/tests/allocations.rs, its thread-scoped counter keeping the largest request): 22020096 bytes asked for | passes: no request above one device's 336 bytes | 22020096 again |
| The VM-flag grammar (`has_vm_flag`, f57f13582; `CarriedVmIo::read`, b20e593c8) read a restore's words with run's (`RUN_VALUED`), which lacks restore's own valued flags: `restore DIR --backing --host-managed` contained the helpers, and in `--backing --gpu-report 999` the word after the backing file was taken for the report writer's descriptor. `spec::RESTORE_VALUED` lists them, `cli::vm_options` reads a run's or a restore's options with its own grammar for both, and `parse_restore` takes every value through `spec::next_vm_word`, so a valued flag missing from the list is refused there | R04, R12 | `a_restores_own_values_are_no_vm_flags`, `a_restores_own_values_carry_no_pipe`: both fail | both pass, with `a_restores_own_valued_flags_take_their_values` (vm_run) | `--backing` out of the list: both fail |
| `unsafe impl Send for Venus`'s SAFETY comment predates the held driver (6accac1e5), whose instance and loader pointers it did not cover. Now it says why they may drop on any thread (vkDestroyInstance's external synchronization; dlclose(3), FreeLibrary) | R07 | (comment) | | |
| `selection::Properties`' 308 bytes, which native-identity.c's `_Static_assert` holds and the bridge writes through its pointer, were checked by one unit test; now as the crate compiles, on every target | R01, R07 | (no live defect: the layouts agree) | const assert | |

## Cured after this review (goal 52, by ROOT's word)

Goal 52's GPU-defects child cured the findings this review recorded as owed. D232's review paragraph lists them by
SHA:
- (5) `start_reported`'s writer, taken twice, says `ReportTaken`: cb8b92f1b.
- (6) `--gpu-device`'s selection is checked once, after every flag: cfc504f25.
- (3) said.rs reads a restore's words with restore's grammar: 5a377965a.
- (2) `imports_host_memory`'s count is taken by `try_reserve`, and each name within its array: 2aabfeb7d.
- (4) a selection's refusal is typed across the VM process: `GpuProbeError::Refused`/`Unreported`, read by the API.
  That's 18f59d497, landed by merge ba9879a66.
- (1) build.rs's bridge watch: given to ROOT as a hunk for its build.rs change (p/helpers-zig-v1).

The "Owed" and "Outside this lane's paths" sections below are kept as this review found them.

## MAX_PHYSICAL_GPUS (R03)

Its figure, 65,536, is the renderer's patches' `1 + UINT16_MAX`, from Hex's scheduler's u16 device index. The
patches hold to it a context's selection (`VIRGL_DEVICE_UUID_MAX_COUNT`, physical-transport-*.patch) and, for a
selected context, the host's Vulkan devices and device groups (`VKR_PHYSICAL_DEVICE_FILTER_MAX_COUNT`, physical-*.patch:
`vkr_instance_enumerate_physical_devices`, `_selected_groups`, `_filter_devices`, `_filter_map`). Rust must not take
what the renderer refuses, so `check_selection`, `probe_bytes`, `selection::resolve`'s and `host_visible`'s host
counts, format 29's reader and `--gpu-device` hold the same figure. The protocol's own bounds are Vulkan's `uint32_t`
counts, for which every patch already allocates overflow-checked, and the transport's `uint32_t byte_count` over its
33-byte stride (2 × VK_UUID_SIZE + NUL): `UINT32_MAX / 33` = 130,150,524, which `virgl_device_selection_payload_size`
already checks. Moving to them changes the C patches, which only a renderer build and a real GPU check: stated as the
gap in D232's Literals and owed to ROOT, `MAX_PHYSICAL_GPUS` following in the same change. Below the figure a count is
now bounded by what carries it before the host's GPUs are enumerated (a receipt's lines, as above; a snapshot's bytes,
`codec::Reader::seq`), and after, by those GPUs, each named once (`resolve`'s Missing/Aliased). The probe's fixed-line
bound (`FIXED_BYTES`) is derived from `MAX_PHYSICAL_GPUS`'s digits where it held a copy of them, and
`the_byte_bound_is_the_longest_encoding_of_its_count` pins the bound to the longest encoding exactly.

## Read and kept

- 08bb8fda1: one grammar, exact field widths, hex digits only before each radix parse, PCI device and function in
  range, a selection's identities distinct with fallible storage; Display round-trips.
- f2fe241a3: test only, correct.
- 6accac1e5, bbd221699: the teardown order. `Drop for Venus` resets, cleans the renderer up and joins the render
  server; then its fields drop, `_driver` last: `HeldDriver` destroys the instance before its `Vulkan` closes the
  loader. `STARTED` clears before `_driver` drops; a renderer started in that gap makes its own instance, Vulkan's
  instances are independent and the loader is reference-counted, so nothing is shared. The test's dynamic-section
  walk is test code.
- 83b09843a: the join (sorted keys, one key per identity, Ambiguous, Aliased, nonzero and unique UUIDs; fallible
  storage), `uuid_text` (NUL-terminated 33 bytes), the binding in `create_context` before the guest hears (destroyed
  on refusal), snapshot 29 (each selector at least 8 bytes, `check_selection` and a GPU required on both sides, 28
  still read), build.rs's link lines. The C: the transport's count, `byte_count` and stride checks with overflow
  guards, exact frames, hex and NUL validated, a state machine (NEW, READY, SELECTED, STARTED, INVALID) that poisons
  the context on any failure; the filter's sorted UUIDs, one handle per identity, groups refused rather than split.
- native-identity.c: the extension count's overflow guard, `written > count` refused, VK_INCOMPLETE refused (nonzero),
  `memchr` before every `strcmp` and on `deviceName`, the struct's size static-asserted.
- f57f13582: the probe needs a selection and the renderer's acknowledgment of it (context 1) before it reports;
  `--host-managed` only chooses whether helpers lead sessions of their own.
- 002ade01a: names NUL-terminated UTF-8 within 256 bytes, device-local heaps summed checked, memory never inferred.
- b20e593c8: `CarriedVmIo` (access mode, FIFO, aliasing refused before adoption, CLOEXEC until the final exec), the
  Windows report writer (`served_pipe`'s checks, then sole ownership), `start_gpu_report`'s typed refusal of an empty
  selection, one report written and closed before any vCPU runs.
- 8b5fb57c6: a SAFETY comment's place (test).
- 1cd728678, 1f86d092a (scripts/gpu-libs, step 3's, not edited): meson names the bridge `lib*.a` on Windows too, and
  `-Db_vscrt=md` matches the C runtime the renderer's objects ask for. The bridge's project names no buildtype, so on
  Linux and macOS it builds at meson's default (debug), where the renderer's and libepoxy's default to release
  (1f86d092a's own reading): correct, noted for step 3.

## Owed, with evidence (not fixed here)

- R04: a selection's refusal stays typed inside the VM process only. `host_visible` and `gpu_renderer` turn
  `JoinError`, `PhysicalGpuError` and `SelectionReportError` into text (virgl.rs `host_visible`, vm/mod.rs
  `gpu_renderer`) and a boot reports `StartError::Legacy`. Across the process boundary a failed probe reaches
  shards-api as `End::Failed(String)` (api client.rs `probe_gpu`), and a managed launch's report pipe closed unwritten
  reads as `GpuProbeError::Syntax` (api physical/report.rs `take`, then `GpuReport::parse` of nothing), naming a
  missing GPU a malformed receipt. Typed, it needs a refusal frame on the report and probe channels (shards-abi) that
  the API reads (step 1's lane): a wire change across lanes, ROOT's to schedule.
- R04, minor: vm_run.rs `start_reported`'s writer, taken a second time, says `GpuProbeError::Scope` where
  `ReportTaken` is meant; the renderer calls it once, so no test reaches it.
- R14: `--gpu-device` runs `check_selection` (a sorted copy) after every flag, quadratic in the flags given, which
  only argv's size bounds. One check after parsing suffices; the cost is unmeasured, ROOT's to measure (R09, R14).

## R01–R16

R01: the reviewed production code returns every failure; checked arithmetic and fallible allocation throughout;
the receipt's reservation now bounded (fixed). R02: no decision of this review's; D232's rests on Hex's research and
the managed-virtio-gpu lane's patch check. R03: as above, the figure's derivation owed; `FIXED_BYTES` and
`DEVICE_LINE_LEAST` derived from the format; `GPU_NAME_BYTES` Vulkan's; 308 the C struct's. R04: owed as above; the
C transport's states explicit. R05: no cache or wait added; `MachineGpu` carries a boot's window and writer as one
argument (clippy's too_many_arguments). R06: `host_visible` is one long closure; unchanged, no defect. R07: every
SAFETY comment of these commits read; one gap fixed. R08: no performance claim. R09: each fix red on the code as it
was (r198). R10: this record, the tests, the mutations. R11: the changed code is every target's; run on macOS arm64
alone (one host), CI's matrix is ROOT's. R12: one scan (`vm_options`) for both callers. R13: `has_vm_flag` one line.
R14: the reservation measured and fixed; the quadratic check owed. R15: implementer; no VM, GPU or CI run. R16: this
child's runner directories removed.

## What ran where

kiln run r198 on adas-mac-studio-3.local (Darwin arm64, cargo 1.98.1), a clone at main 1389dc0b6 with this change
applied (its tree the merged tree, b637dfbec's):

- red, the tests alone on 1389dc0b6: `cargo test -p shards-abi a_gpu_receipt` 1 failed (22020096 bytes);
  `cargo test -p shards --bin shards restores_own_values` 2 failed;
- green: `cargo test -p shards-abi gpu` 8 + 1 passed; `-p shards-vmm gpu_selection` 8, `snapshot::` 31;
  `-p shards --bin shards` `vm_flags` 2, `carried_io` 3, `said_options` 1, `spec::` 7; `-p shards-vm-process`
  `vm_run::` 12, `spec::` 7; `cargo clippy -p shards-abi -p shards-vmm -p shards -p shards-vm-process --all-targets
  -- -D warnings` and the same for shards-vmm and shards-vm-process `--features gpu`: clean;
- guards removed: the receipt test 1 failed (22020096 bytes); the restore tests 2 failed.

shards-vmm's `--features gpu` tests were not run: they link the renderer, which needs scripts/gpu-libs.

## Owed to ROOT

- The renderer's tests with the identity bridge, on each GPU host class (Linux x86_64 NVIDIA, AMD and Intel; macOS
  arm64 Metal; Windows x86_64): `scripts/gpu-libs "$L" && SHARDS_GPU_LIBS=$L cargo test --profile test-release
  -p shards-vmm --features gpu --lib -- devices::virtio::gpu:: vm::gpu_selection::` (`L` an absolute directory).
- A GPU run in a real microVM on the same classes: `SHARDS_GPU_LIBS=$L cargo test --profile test-release -p shards
  --test gpu`. Windows arm64: unverified (no host; D232's "Windows on arm64").
- D232's evidence owed, for which no committed test exists: a GPU VM with `--gpu-device` rendering on the selected
  GPU and no other, a wrong identity refused, `--gpu-probe`'s and `--gpu-report`'s receipts on a real GPU, a format-29
  snapshot restored, and 30 Vulkan programs in one GPU VM with no 19th-context failure.
- The C patches' derivation (above): `VIRGL_DEVICE_UUID_MAX_COUNT` to `UINT32_MAX / VIRGL_DEVICE_UUID_STRIDE`, the
  filter's selection bound the same and its host counts to Vulkan's `uint32_t`, `MAX_PHYSICAL_GPUS` with them; then
  the two runs above.
- CI's eight targets for d7c9938ec and d27cd59b0 (cli/mod.rs and spec.rs carry Windows and Unix paths).

## Outside this lane's paths

- crates/shards/build.rs `compile_gpu_helper` (the goal agent's reading, which holds): its `rerun-if-changed` lines
  name the renderer's libraries but not the identity bridge's (`libshards_gpu_selection.a`,
  `shards_gpu_selection.lib`), which 83b09843a added to crates/vmm/build.rs only, so a bridge replaced alone rebuilds
  no carried `shards-vm-gpu`. gpu-libs rewrites every library, so only a hand-replaced bridge meets it; reproducing
  it needs a renderer build (R09): owed.
- virgl.rs `Vulkan::imports_host_memory` (c8447ed88, D202, Windows): an infallible `collect` of a driver-given count
  of 260-byte entries, and `CStr::from_ptr` on each driver-filled name without the NUL check native-identity.c makes.
  Windows only, untestable here: owed to D202's lane.
- said.rs `quotes` reads a restore's words with run's grammar too (`restore DIR --backing --no-console` takes the
  backing file for `--no-console`): D12's code; `spec::next_vm_word` with `restore` serves it.
