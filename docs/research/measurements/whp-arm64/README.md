# M414: arm64 Windows Hypervisor Platform

The probe is independent of shards' backend. It makes a two-vCPU partition, runs its
own guest, delivers an SPI, exercises PSCI, reads and sets the GIC's state, and resets
the virtual counter. Build on the Mac with:

```sh
PATH="$(brew --prefix llvm)/bin:$PATH" RUSTFLAGS='-C target-feature=+crt-static' \
  cargo xwin build --release --target aarch64-pc-windows-msvc
```

Copy the executable to Windows and run it there. `host.ps1`, run elevated, records the
optional features, boot configuration, Hyper-V events and WHP capability/partition calls.
`Get-ComputerInfo` saying `HyperVisorPresent=True` alone does not prove WHP works: it
said so before Windows' hypervisor started, too.

## Host, 2026-11-01 to 2026-11-03 UTC

Apple M5 Max, macOS 26.4.1. Windows 11 Pro arm64, build 26300.9457, installed from
Microsoft's `Windows11_Client_arm64_en-us_26300_9457.iso`, SHA-256
`8992f2d2cfd2fa647e579e9eb31fc21f34da8333c29b6170fde17d2ed6623e9c`.
HypervisorPlatform, VirtualMachinePlatform and Microsoft-Hyper-V-All enabled;
`hypervisorlaunchtype Auto`. QEMU 11.1.3, tag commit `005c61c`, four CPUs, 8 GiB RAM,
GICv3, a sparse 96 GiB NVMe disk. Each experiment used an overlay of one installed disk.
The ISO was downloaded through Microsoft's normal browser flow after its scripted API
refused a request; no bot check was bypassed.

| Outer machine | Windows result |
|---|---|
| HVF, EL2 disabled | Desktop; Hyper-V System event 43, EL2 not present; WHP present 0 |
| HVF, `virtualization=on` | The same: the default PFR0.EL2 field remains zero (M422) |
| HVF, EL2 enabled and PFR0.EL2 set to 1 | Desktop; event 26, BalStatus 0x19, sub-status 0xC; WHP present 0 |
| TCG, VHE available, EL3 absent | The same event 26; diagnostic tracing names rejected PFR0.EL3=0 |
| TCG, TF-A at EL3, default pointer authentication | Stops on `MSR APIBKeyHi_EL1`, ESR_EL1 0x02000000; HCR_EL2.APK=0 |
| TCG, TF-A, pointer authentication disabled | HAL initialization bugcheck 0x5C: ACPI advertises no PSCI |
| TCG, TF-A, pointer authentication disabled, ACPI describes TF-A's PSCI | Desktop; event 1, hypervisor successfully started; WHP present 1 and partition creation succeeds |

The successful CPU is `max,sve=off,sme=off,pauth=off`, accelerator `tcg,thread=multi`,
machine `virt,gic-version=3,highmem=on,secure=on,virtualization=on`. This is software
emulation of the Windows host. It is Windows' real hypervisor and WHP partition, not a
replacement for WHP; its timings are not native Windows hardware timings.

## Firmware

TF-A v2.15.0, `da738d5eae93af342fdc4995dd3c05acb4c9d757`, and EDK2 stable202608,
`2970e5699ba6267f3384ffab20f96647578aebc8`. Build EDK2's
`ArmVirtPkg/ArmVirtQemuKernel.dsc` for AARCH64/DEBUG/CLANGDWARF, then TF-A's `PLAT=qemu
DEBUG=1 QEMU_USE_GIC_DRIVER=QEMU_GICV3`, with that `QEMU_EFI.fd` as BL33, `all fip`.
TF-A's `qemu_fw.rom` has BL1 at zero and the FIP at 0x40000; use it as the first pflash.
The commands and output hashes are in `nested-host/`. The second pflash is writable
UEFI variables. `start.sh` starts an installed disk; the disk and credentials stay local.

With secure firmware, QEMU disables its own PSCI implementation. Its generated FADT
therefore has ArmBootArch=0, though TF-A supplies PSCI over SMC. The local fixture patch
`qemu-tfa-psci.patch` describes that handler in ACPI. It is specific to this TF-A fixture.
It changes no Windows instruction or check. TF-A already adds PSCI to the device tree.

The guest's RTC was initially interpreted eight hours ahead. Raw initial logs retain
those timestamps. Before E2E, the guest clock was set from the Mac's UTC time; subsequent
boots use `-rtc base=localtime,clock=host`.

## Results

Committed transcripts normalize line endings and trailing whitespace; the original
captures remain in the local host kit.

`results/m414.txt` is the revised probe's complete output, exit 0. Setting vCPU 0's
CNTVCT_EL0 to zero changed its counter alone. Its local interrupt state was 200 bytes per
vCPU, the global state 4096 bytes; all set back successfully. The guest took INTID 33,
PSCI_VERSION returned 1.1, CPU_ON started vCPU 1 with X0=0x42, and SYSTEM_OFF produced
Arm64Reset/PowerOff. GICD_TYPER reports 960 SPIs. Partitions of 64 and 128 vCPUs were set
up and their last vCPU created; ProcessorCount=256 was refused.

Dynamic code forbidden after setup still allowed RAM mapping, vCPU creation and execution.
The first probe kept its original mapped partition alive and its second partition's map
failed with STATUS_VID_PARTITION_ALREADY_EXISTS. The revised probe drops the original
before this control. This control does not establish how multiple mapped partitions in
one process behave. SuspendPartitionTime was refused with WHV_E_INVALID_VP_STATE while
vCPU 1 was running. With every vCPU stopped it succeeds: both counters held their values
over 100 ms, and setting each to 1,000,000,000 left both at that value after another
100 ms. ResumePartitionTime then advanced both (`results/clock-stopped.txt`).

WHP's Populate advice succeeded on two memory ranges for both read and write access
(`results/populate.txt`). The backend shares x86_64 WHP's bulk working-set population
and mapped-page recorder. Its snapshot E2E recorded and prefetched a working set.

MMIO writes, n=999: p50 455, p90 608, p99 900, max 1606 microseconds on this emulated
host during development. These numbers describe this run only.

`results/shards-boot.txt` is shards booting its pinned Linux kernel as the standard
`runner` user, through its confined VM process. With no init/root disk it reaches the
expected VFS panic and requests a reset. The boot E2E passed in 27.81 s. The core snapshot
E2E passed in 36.76 s: three processes restored the same two-vCPU machine, continued its
state and received fresh randomness (`results/snapshot-e2e.txt`).

The first full snapshot run passed nine tests but found a 59 microsecond backward step
in CLOCK_MONOTONIC in the busy-machine test. Sampling the host counter separately as
each vCPU was released gave them different epochs. The backend now suspends partition
time before creating the restored vCPUs, sets each counter before loading its timer
registers, and lets the first WHvRunVirtualProcessor resume partition time. The unchanged
busy-machine test passed separately at two and eight vCPUs, including all three restores
and its clock comparisons (`results/clock-order-2cpu.txt`, 29.35 s;
`results/clock-order-8cpu.txt`, 24.34 s).

On the four-CPU outer host the unfiltered test reached its 64-vCPU case, then timed out
after 120 s while the original guest was streaming, before its snapshot. That failure
is retained in `results/storm-64cpu-timeout.txt`. The host was resized to 16 CPUs and
24 GiB for the full suite. No assertion or deadline was relaxed. The complete snapshot
and feature E2E results remain required before step 10 is closed.

## Full workspace suite

Build the workspace with static CRT so a fresh Windows ISO needs no extra runtime:

```sh
PATH="$(brew --prefix llvm)/bin:$PATH" RUSTFLAGS='-C target-feature=+crt-static' \
  cargo xwin test --locked --workspace --profile test-release \
  --target aarch64-pc-windows-msvc --no-run --message-format=json > tests.jsonl
```

Build `shards-testguest` for `aarch64-unknown-linux-musl` with the `guest` profile, and
`shards-init` with `SHARDS_ABI_IDENTITY=1` into a separate target directory for the
foreign-contract test. `nested-host/bundle-tests.py WORKSPACE tests.jsonl tests.zip
--guest TESTGUEST --foreign-init FOREIGN_INIT` packages every test executable, its
per-package working directory, compiled helpers and the tracked fixture tree. It keeps
the compiled Mac paths under `C:\` because the binaries contain those absolute paths.
It copies each build output directory's files and helpers, excluding its nested cargo
build trees, whose long paths are unnecessary on Windows.

Verify the bundle's SHA-256 and extract it at `C:\` with Windows' `tar`. Give the standard
test user access to the fixture tree and `C:\shards-step10`. Supply the pinned kernel
above and Git for Windows arm64. This run used
[portable Git 2.56.0.windows.4](https://github.com/git-for-windows/git/releases/tag/v2.56.0.windows.4)
(`PortableGit-2.56.0.4-arm64.7z.exe`, SHA-256
`044110f9d951858452ddb5e4b5415ea0f2b5b2598e794d6fec77fe28b9aa8cc0`), extracted at
`C:\shards-step10\git`. The runner prepends its `cmd` directory and appends `usr\bin`
to PATH: the archive-loading E2E uses GNU `gzip` to make its independent fixture.

Run `nested-host/run-tests.ps1 -Kernel KERNEL -Tag NAME` as the standard user. It sets
`SHARDS_REQUIRE_VMS=1`, runs every executable with one test thread, preserves raw stdout,
stderr and the actual native exit status for each, and continues after test failures.
The first binaries are snapshot and boot. A nonzero exit or an incomplete manifest is
not a passing suite. The runner uses .NET's process API: PowerShell 5.1's Start-Process
returned an empty exit code in this fixture; a deliberately failing native child
verified that the replacement captures exit 1.

`-Binary NAME` and `-Test NAME` select diagnostic subsets; their reports include the
selection and original manifest size. The reusable runner's Windows trial passed four
tests in two `allocations` binaries (`results/runner-trial.txt`). That is a runner
check, not the workspace result. The Windows-target doctest run found zero tests in
all 35 library groups (`results/doctests.txt`).

The first run on the resized host passed eight snapshot tests and failed both 64-vCPU
storm cases, with Linux soft-lockup messages before the snapshot (`results/snapshot-16cpu.txt`). Boot passed 19 tests
and failed two: the maximum-vCPU boot produced no console before its 60 s deadline, and
the nested-virtualization refusal test expected x86_64's error text on arm64 Windows.
The latter assertion has been corrected to require WHP arm64's refusal and exit 1.
The 16-CPU workspace diagnostic run completed 80 of 141 binaries: 76 passed and four
failed (snapshot, boot, Rego's wide-policy compilation deadline and Windows builders).
It was stopped after more than two hours in the container binary, whose partial failure
log was retained. This was an aborted, incomplete run, not a qualifying suite. Windows
then applied updates during shutdown. Targeted checks on a four-CPU, 24 GiB host precede
another complete run; no complete-suite success is claimed. The completed binary records
are in `results/full-real-02-partial.jsonl`, with the abort marker in
`results/full-real-02-aborted.txt`. The unfinished container binary is absent from those
completed records.

## Fixture comparisons, 2026-11-03 UTC

Windows Update changed the installed host from build 26300.9457 to 26300.9711 during
that shutdown. The following checks use source `360ad0cd`, four outer CPUs and 24 GiB.
That OS change prevents attributing a difference from the earlier 16-CPU run solely to
CPU count. Each command below selects one test, with its original assertions and
deadlines, through `run-tests.ps1 -Binary BINARY -Test TEST -Tag NAME`.

| Test | Multithreaded TCG | Single-threaded TCG |
|---|---|---|
| `boot/a_nested_guest_is_a_kvm_host` (the expected arm64 Windows refusal) | pass | pass |
| `compile/a_wide_policy_without_cycles_compiles_in_time` | 10 s deadline failure | 10 s deadline failure |
| `build_windows/run_steps_run_in_a_confined_builder_and_their_changes_are_built` | pass | pass |
| `boot/boots_the_hosts_maximum_vcpu_count` | 60 s deadline failure | 60 s deadline failure |
| `snapshot/storm::restores_of_a_busy_machine_lose_no_interrupt_and_repeat_no_write` | 64-vCPU original timed out before snapshot | the same, with no echo bytes |
| `containers/a_groups_microvms_are_held_to_its_cpu_max_together` | pass | pass |
| `build_windows/a_builders_memory_is_plugged_as_its_build_needs_it` | see follow-up below | 180 s deadline failure |
| `build_windows/a_builders_memory_is_plugged_as_fast_as_its_jobs_take_it` | see follow-up below | 180 s deadline failure |

`results/cpu4-checks.txt` and `results/single4-checks.txt` retain these diagnostic
subsets. Single-threaded TCG provided no improvement, so multithreaded TCG was restored.
On that next boot, maximum-vCPU boot passed in 56.66 s and the first memory-growth case
passed in 149.34 s; the faster-growth case still failed its 180 s deadline
(`results/multi4-followup.txt`). A pass near a deadline does not establish a stable
qualifying configuration. The basic build was not one of the earlier two failing
memory-growth cases.

A separate direct boot measured where time went, using the same pinned kernel and
`SHARDS_LOG=info shards run --kernel KERNEL --cpus 123 --memory 1024 --cmdline
"console=ttyS0 panic=-1"`, without an init or root disk. All 123 vCPUs were ready after
917,717 microseconds. Linux brought its configured maximum of 64 CPUs online at guest
time 15.723242 s and reached its expected VFS panic at 88.161977 s. Native exit was 3
(guest reset), after 97.52255 s wall time (`results/direct-boot123.txt`). This measures
quick vCPU preparation and a slow guest boot; it does not establish a counter-frequency
defect. The guest reported a 1 GHz architectural counter.

## VBS settings and an updated HVF boot, 2026-11-03 UTC

This diagnostic used a sparse child disk made only after cleanly shutting down the
working TCG host, with a separate copy of its UEFI variables. The baseline disk was
preserved. First, the documented
[VSM boot setting](https://learn.microsoft.com/en-us/windows-hardware/drivers/devtest/bcdedit--set)
was changed while leaving `hypervisorlaunchtype Auto`:

```powershell
bcdedit /set '{current}' vsmlaunchtype Off
```

After a clean reboot, BCD showed `vsmlaunchtype Off`, but Win32_DeviceGuard still
reported VirtualizationBasedSecurityStatus=2, services configured/running `{0}`. WHP
reported present=1 and created a partition. A second experiment set
`EnableVirtualizationBasedSecurity=0` as a DWORD under both
`HKLM\SYSTEM\CurrentControlSet\Control\DeviceGuard` and
`HKLM\SOFTWARE\Policies\Microsoft\Windows\DeviceGuard`, then rebooted again. The
[documented policy](https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-deviceguard)
value remained zero at both paths; VBS status remained 2 and WHP still worked. No locked,
mandatory or signed policy was altered. These settings did not provide a measured
VBS-off/WHP-running control, so they establish no VBS performance result.

The updated disk with these settings was then booted in its own child under HVF, real
EL2 enabled and truthfully advertised, four CPUs and 24 GiB, using the original EDK2
firmware. Windows reached the desktop. The current boot's Hyper-V event was 26:
BalStatus 0x19, sub-status 0xC; WHvGetCapability returned present=0 and
WHvCreatePartition returned 0xC0351000. Get-ComputerInfo still said HyperVisorPresent=True.
VBS status was zero on this failed-hypervisor boot; that is not a WHP-running control.
No EL3 support was fabricated and no Windows binary or check was modified. This measures
another failed HVF launch, without establishing VHE's absence as its cause. The complete
settings and host transcripts are retained in `results/vbs-and-hvf-settings.txt`.

## Memory-growth profile

After restoring the preserved multithreaded TCG baseline (four CPUs, 24 GiB,
26300.9711), the faster-growth builder test was repeated with `SHARDS_LOG=info`. Its
four vCPUs were ready after 341,920 microseconds and its startup marker arrived after
4,898,482 microseconds. It again failed its 180 s deadline while running the step that
writes 1 GiB to tmpfs and allocates/touches 3 GiB. This narrows the elapsed time to after
guest startup; it does not establish whether the memory-growth mechanism itself is
incorrect.

The existing selected-test runner drove the workload. At 35, 85 and 145 s after its SSH
launch, the Mac ran `/usr/bin/sample QEMU_PID 5 10 -file OUTPUT`: five-second samples at
10 ms intervals. `results/memory-growth-profile.txt` retains the native test report and
the profiler's stack summaries. Full call graphs remain in the local kit. The later
samples include address translation (`get_phys_addr`, `arm_cpu_tlb_fill_align`) and
mutex waits as well as emitted guest code. After this test, QEMU's `info jit` reported
22,347,718 partial TLB flushes and four translation-buffer flushes, cumulative since
the Windows host boot. These observations justify testing emulator overhead; they
do not identify a backend defect or prove which cost caused the timeout. Sampling
adds overhead, so this run is diagnostic; the uninstrumented failure is retained above.

A separate one-CPU, 24 GiB child of the same baseline started Windows' hypervisor
(current-boot event 1). Its selected compiler test again failed the 10 s deadline, and
maximum-vCPU boot failed its 60 s deadline after Linux had started CPU20. That boot
executed a real WHP guest, but did not complete. A diagnostic tag containing an underscore
was then refused by the local helper, so the planned memory-growth and storm checks did
not start. The helper's tag construction was corrected; those unrun checks are not VM
results. The two completed failures do not provide a qualifying configuration
(`results/onecpu-checks.txt`).

## Further controls and the next complete attempt

The wide-policy compiler test was also built alone with the production `release`
profile (fat LTO, one codegen unit, static CRT):

```sh
RUSTFLAGS='-C target-feature=+crt-static' cargo xwin test --locked -p shards-rego \
  --test compile --release --target aarch64-pc-windows-msvc --no-run
```

On the restored four-CPU host, WHP returned present=1 immediately before the test.
The same assertion failed after 10.09 s. This was one selected test, not a production
release workspace run (`results/release-rego.txt`). Fat LTO did not remove this failure.

A Cortex-A76 CPU-model control kept four CPUs, 24 GiB and the counter at 1 GHz. It
stopped in TF-A BL1 before Windows: CPU0 was at EL3h PC 0x4b30, which the existing BL1
ELF resolves to `plat_panic_handler`; LR 0x580c resolves to `SynchronousExceptionSP0`.
This identifies the exception path, not the original faulting instruction. The serial
log and symbol resolution are in `results/cpu-model-controls.txt`. The trial was stopped;
no Windows or WHP test ran.

A separate control used the working `max` model with `pmu=off`, keeping the other CPU
settings. It reached Windows Boot Manager but had not reached SSH readiness after more
than eight minutes. Its screen still showed TianoCore/Windows Boot Manager and CPU0's
PC was 0x63caa5a0c in the loaded boot-manager image, at NS EL2h. It was stopped before
any WHP probe or test. This incomplete boot control establishes no PMU performance
result or Windows failure cause. Both controls used disposable child disks; the working
host's disk was retained.

The complete retry uses the working four-CPU `max` configuration, 24 GiB, build
26300.9711 and the original 141 `test-release` executables. Use `-HeartbeatSeconds 30`
with the runner to print the current binary's PID and elapsed time while it runs. The
default zero disables those reports. They add no native test filter or timeout; native
stdout, stderr and exit status are still retained separately. Preflight parses the
PowerShell runner, checks its hash, requires the full manifest and verifies WHP before
starting. Only a completed manifest with no failures can qualify Step 10.

## Wide-policy compiler diagnostic

The unchanged 6000-rule compiler test failed its 10 s assertion on Windows. A native
sample of the same workload found repeated `Node::extent` object construction under
reference type queries. The candidate obtains string key types without building the
object and answers dynamic references directly when the fixed values are scalars.
Function values retain the general path: their union with `any` is not `any`.

`wide-policy.py WORKSPACE BASELINE_REV NEW_SCRATCH --n 30` copies both sources, builds
isolated production-release binaries, warms their file pages once, and alternates
before/after execution order. It extracts `host()` and `layers(500,12)` from the
existing test, keeps its 123 MiB worker stack, and asserts that compilation has no
errors. Scratch must not exist. Python 3.12+, Cargo and cached dependencies are needed.
Run it at the user's normal scheduling priority (an Interactive launchd job on this
Mac). It does not instrument or edit the qualification checkout.

On 2026-11-03, M5 Max, macOS 26.4.1, standalone rustc 1.99.1, fat LTO and one codegen
unit, against source 360ad0cd and the retained candidate patch (milliseconds):

| Phase/build | n | p50 | p90 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| Compile/before | 30 | 757.394 | 782.753 | 801.536 | 801.536 |
| Compile/after | 30 | 276.298 | 285.846 | 289.854 | 289.854 |
| Parse + compile/before | 30 | 779.528 | 806.984 | 825.200 | 825.200 |
| Parse + compile/after | 30 | 299.800 | 308.567 | 313.776 | 313.776 |

The four-CPU Windows fixture and its full suite were running, as were cross-build
jobs; these are this native workload's measurements under that load. They establish
neither Windows performance nor a passing Windows deadline. Root Rego tests, including
the OPA comparisons and a scalar/function reference regression, and all eight matrix
lint targets passed with rustc 1.98.1. The full suite's deployed binaries remain
source 360ad0cd; separate candidate checks below do not replace that suite.

Raw rows, source hashes, patch, summaries and corrected compiler metadata are in
`results/wide-policy/`. The original metadata queried rustc from the parent checkout;
it is retained beside the correction to both standalone builds' actual rustc 1.99.1.

## Deep-policy bindings and Windows checks

The original Windows deep-`every` test took 63.65 s and failed its 60 s assertion.
A native sample found binding lookups scanning the whole set after it grew past
16 variables. The evaluator now adds an index at that threshold, keeping its existing
iteration order and undo behavior. A regression exercises lookups, middle removals,
replacement and reinsertion, including after the set shrinks below 16 entries.

`deep-policy.py BEFORE_TEST_BINARY AFTER_TEST_BINARY NEW_OUTPUT --before-source ID
--after-source ID --rustc VERSION --n 20` runs the exact existing
`every_statements_nested_deep_answer_in_time` case, one unrecorded warm-up per build,
then alternates execution order and requires exactly one test to pass each time. It
retains raw output and binary hashes. Both builds here used rustc 1.98.1 and
`test-release` (no LTO, 16 codegen units), on the M5 Max/macOS 26.4.1 with the Windows
fixture and cross-build jobs active. Entire test-process times, milliseconds:

| Build | n | p50 | p90 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| Before index | 20 | 3371.999 | 3533.665 | 3598.273 | 3598.273 |
| After index | 20 | 1735.963 | 1894.770 | 1908.657 | 1908.657 |

`results/deep-policy/` retains the exact measured source patch, rows and outputs.
Both the scratch prototype and the promoted source passed the Rego tests and OPA
comparisons. All eight matrix lint targets and the complete arm64 Windows cross-build
passed. During the already-failing `real-03` run, a separate standard-user kit ran the
candidate's original Windows assertions: the wide policy passed in 5.28 s (10 s bound),
the deep policy in 38.47 s (60 s bound), and all ten Rego library tests passed. Raw
native outputs, selected-test records, executable hashes and source-patch identity are
in `results/policy-hotpaths-windows/`. These checks overlap the container binary and
are targeted regressions, not a complete-suite result.

The WHP register hot path now uses aligned stack storage for single-register accesses
and single/pair writes; an MMIO load sets its destination and PC in one call. The M414
guest was rebuilt to exercise the paired stack write. As the standard user it returned
0x5678 from that load, took SPI 33, started its secondary through PSCI, set its GIC
state back, and powered off through Arm64Reset, with native exit 0. Its raw output is
`results/mmio-batch.txt`. This probe also overlapped the already-failing full run; its
latencies are diagnostic and do not establish native performance or qualify all features.

## Credential commands on Windows

The continuing container run reached its CloudWatch fixture and reported that `printf`
was unavailable. The fixture now uses Windows' `echo` there. A native command probe
also found a production quoting problem: passing configured shell text through ordinary
`Command::args` inserted backslashes into its JSON and failed a quoted program path.
`src/bin/credential-process.rs` compares that call with literal command text; its raw
control uses `/D /S /C`. Both the JSON and quoted-program controls then succeeded, and
exit code 3 was preserved. Build with static CRT and `--bin credential-process`, as
for the main probe. An earlier probe failed its control too; its output is retained
but supplies no quoting conclusion.

The production provider keeps `/C` and encloses the configured command through
`CommandExt::raw_arg`, following the
[Rust documentation](https://doc.rust-lang.org/stable/std/os/windows/process/trait.CommandExt.html#tymethod.raw_arg).
The previously Unix-only credential regression now also runs on Windows with its own
shell fixtures. It verifies the listed environment, expiration, rejection of Version 2,
exit code 3, and a `.cmd` helper with a space in its filename. The exact production path
passed on this standard-user arm64 Windows host in 1.20 s, with one test run and native
exit 0. The Mac test and all eight lint targets also passed, as did the complete arm64
Windows cross-build. Raw output, source patch and hashes are in
`results/credential-process/`. The selected Windows test overlapped the already-failing
full run; the corrected CloudWatch VM E2E and the full suite remain to pass.

## Archive fixture prerequisite

`load_reads_archives` completed its uncompressed load/run phases, then could not start
the external `gzip` that makes its compressed fixture. The portable Git bundle already
contains `usr\bin\gzip.exe`; that directory was absent from the runner's PATH. The
runner now appends it, preserving existing command precedence. No archive test or
assertion changes. `nested-host/probe-gzip.ps1` resolved the bundled executable, ran it
with native exit 0 and checked its 39-byte output with .NET's independent decompressor.
It also parsed the candidate runner on Windows PowerShell. The result and executable
hash are in `results/gzip-fixture.txt`. The candidate script was copied under a separate
name; the active full run keeps its original script and environment. Its archive
failure remains a failure, and the compressed-image E2E still needs the next full run.

## Standard-user fixture prerequisites

The original `real-03` container binary finished with 126 passed, 14 failed and eight
ignored. Its bind-mount case failed while creating its host file symlink, with Windows
error 1314 (required privilege not held). The ETW case returned after its explicit
trace-session permission skip. Neither result verifies those features. The relevant
native output is in `results/user-fixture-baseline.txt`; the remaining full suite was
still running when these prerequisites were prepared.

`nested-host/prepare-user.ps1` runs as the fixture administrator before a reboot. It
keeps `runner` outside Administrators, enables Developer Mode, and adds that local user
to Performance Log Users by its well-known group SID. Microsoft documents Developer
Mode as the prerequisite for
[unprivileged symbolic-link creation](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-createsymboliclinkw),
provides the
[registry setting](https://learn.microsoft.com/en-us/windows/advanced-settings/developer-mode),
and names Performance Log Users as a group allowed to
[control ETW sessions](https://learn.microsoft.com/en-us/windows/win32/etw/controlling-event-tracing-sessions).

After reboot, a fresh `runner` logon runs `nested-host/probe-user.ps1`. It refuses an
elevated or unexpected user, verifies the current token's trace group, creates a file
symlink through the unprivileged Windows API and reads through it, then creates,
enables and stops a native ETW session for shards' provider. The original mount and
ETW E2E assertions remain unchanged; this preflight does not replace either test.
Preparation is deferred until the original full attempt has finished and its complete
results have been archived. Native permission checks and the next full suite remain
pending; syntax validation alone does not verify those permissions.

Both scripts parsed on the native Windows PowerShell 5.1 host, and all eight lint
targets passed. `results/user-fixture-parser.txt` retains the syntax check and source
hashes. No Rust source or native test assertion changes in this fixture update.

## Observing files as the standard user

The continuing original run finished its daemon binary with 28 passed, nine failed and
three ignored. `a_pool_of_none_serves_every_run` failed in the Windows file-usage
observer after its three command runs and their existing timing checks. Its
`RmGetList` result was error 5, not a count of processes holding the snapshot file.

`src/bin/file-users.rs` isolates the observer from any VM. It queries a temporary file
closed, open, mapped, mapped with its file handle closed, and released; a child process
also holds a mapping until the parent tells it to release it. Build this standalone
probe with the same static-CRT `cargo xwin build` command as M414, adding
`--bin file-users`. It creates and removes only its own temporary directory; optional
path arguments are inspected without modification.

On this Windows 26300.9711 host, in SSH session 0, `RmStartSession` and
`RmRegisterResources` succeeded as `runner`, but `RmGetList` returned 5 even for its
own ordinary file. The elevated control succeeded. A direct
`NtQueryInformationFile(FileProcessIdsUsingFileInformation)` query succeeded without
elevation and found both an open file and a mapping after its file handle closed.
It found the child's PID while held and stopped reporting that PID after release.
The elevated Restart Manager control missed the mapped-only state. These are
functional controls, not a performance measurement or a complete-suite result.

The Windows test observer now uses the file query, grows its buffer on the API's
size errors, and still fails the test on other errors. Its regression checks the
closed, open, mapped-only and released states through the actual observer. The daemon
E2E assertions and deadlines remain unchanged. Microsoft's
[information-class reference](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/ne-wdm-_file_information_class)
identifies class 47 and marks it reserved for system use; this dependency is confined
to the test observer. The shipped VMM and daemon acquire no new native API dependency.
The complete updated real-VM suite remains outstanding.

All eight lint targets and the fresh 142-binary Windows cross-build passed. The
integrated probe passed as both the standard user and the elevated control. The new
regression ran one case and passed as `runner`; its raw output, hashes and the
independent controls are in `results/file-users/`. These checks exercised file
observation and created no VM.

The original full attempt was interrupted by a Windows host bugcheck, `0x3B` with an
access violation, followed by reboot at 2026-11-04 00:34:58 UTC (System event 1001).
Its native result file contains 101 of 141 binaries, through index 100 (`search`);
index 101 (`shards`) was in progress. It is incomplete and already had test failures.
Windows remains build 26300.9711. The crash's cause is not yet attributed. The
integrated file-observer checks above ran after this reboot; the earlier independent
controls overlapped the already-failing attempt. The crash dump is kept locally for
diagnosis, and is not a repository artifact.
