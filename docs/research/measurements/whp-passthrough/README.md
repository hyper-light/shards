# Whether a Windows host's hypervisor gives a VM a host's PCI function

D154's Windows part, PM M401 and M402: Discrete Device Assignment (DDA) is how Windows gives
a VM one of its host's PCI functions, dismounted from the host first. WHP does it with its
virtual PCI devices (`WHvAllocateVpciResource`, then `WHvCreateVpciDevice`, physically
backed) in a partition allowed device assignment
(`WHvPartitionPropertyCodeAllowDeviceAssignment`).

These are host API probes. They do not boot a shards guest or verify a passed-through
function's DMA and interrupts. Windows DDA support in shards and its real hardware E2E
remain outstanding; a host's refusal here does not establish support on another host.

- `probe.ps1` asks a host, read-only: its Windows, WHP's features (VirtualPciDeviceSupport,
  IommuSupport), an empty virtual PCI resource, AllowDeviceAssignment set on a partition of
  its own (then deleted), Hyper-V's `Get-VMHost` IovSupport and its reasons, the boot
  entry's hypervisor settings, Device Guard's security properties, and each PCI function's
  assignability as Microsoft's survey-dda.ps1 reads it (BIOS-reserved memory, ACS upstream,
  the device's type).

      pwsh docs/research/measurements/whp-passthrough/probe.ps1

  `.github/workflows/whp-passthrough.yml` runs it on GitHub's windows-2022, windows-2025 and
  windows-11-arm whenever it changes: `ci.txt` holds run 41363714971's answers.

- `assign.ps1` tries a function the host can spare, named by its location path: disabled and
  dismounted (`Disable-PnpDevice`, `Dismount-VMHostAssignableDevice -Force`), allocated as a
  resource by its dismounted instance ID, asked for as a partition's device each way WHP
  offers (the property before setup, after it, not at all; a device physically backed and
  one not, and what one that is made reaches of the function), then given a Hyper-V VM of
  its own (`Add-VMAssignableDevice`, `Start-VM`). The VM is removed and the function mounted
  back and enabled whatever happens. It needs an administrator and Hyper-V's PowerShell
  module, and changes the host while it runs: the function is gone from it until it is
  mounted back. Pick one nothing uses (survey it with probe.ps1 first): never the host's
  network, display, or a controller its keyboard is on.

      powershell -ExecutionPolicy Bypass -File assign.ps1 -LocationPath 'PCIROOT(0)#PCI(0202)#PCI(0000)'

  `jupiter.txt` holds both scripts' output on jupiter (Windows 11 Pro 26200.9711), its SD
  card reader the function, 2026-11-01.

The assignment script now gives its temporary VM a unique name, requires an initially
healthy device, and undoes only the disable, dismount and VM creation it performed.
`jupiter.txt` records the earlier version at the hashes printed there.

`macos-api.py` inventories the installed Hypervisor.framework headers and their hashes,
including both architectures. `macos-api.txt` is the macOS 26.4 SDK inventory for M403.
Run it on a Mac with `python3 macos-api.py`.

`boot-policy.md` records M401's approved 2026-11-03 IOMMU boot-policy experiment.
`boot-policy.ps1` ran a temporary entry and two reboots, then verified restoration
of the original loader and boot manager and deletion of the temporary GUID.
Enabling the policy did not change WHP's `IommuSupport=0` or its assignment refusal
`0x80070032`. BitLocker and Secure Boot stayed on; the reader stayed attached and
healthy. Raw output, hashes, and state are in `jupiter-boot-policy.txt` and
`jupiter-boot-policy.json`. The earlier preflight is `jupiter-boot-preflight.txt`.

Primary references: Microsoft's [DDA hardware and device requirements](https://learn.microsoft.com/en-us/windows-server/virtualization/hyper-v/plan/plan-for-deploying-devices-using-discrete-device-assignment),
[resource allocation](https://learn.microsoft.com/en-us/virtualization/api/hypervisor-platform/funcs/whvallocatevpciresource),
and [assignment to a WHP partition](https://learn.microsoft.com/en-us/virtualization/api/hypervisor-platform/funcs/whvcreatevpcidevice).
