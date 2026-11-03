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
`C:\shards-step10\git`; only its `cmd` directory is added to PATH.

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
The workspace run is still in progress; no complete-suite success is claimed.
