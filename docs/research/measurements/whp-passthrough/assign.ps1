# Whether this host's hypervisor assigns a host's PCI function to a partition (D154, PM M401),
# tried with one: the function named by its location path, disabled and dismounted from the
# host (Discrete Device Assignment's first step, Dismount-VMHostAssignableDevice), allocated
# as a virtual PCI resource by its dismounted instance ID (WHvAllocateVpciResource), and asked
# for as a partition's device in each way WHP offers: the partition allowed device assignment
# before its setup and after, a device physically backed and one not
# (WHvCreateVpciDevice). Where one is made, what of the function it reaches: its IDs, its
# configuration space, its BARs mapped, an MSI mapped to vCPU 0. Then Hyper-V's own Discrete
# Device Assignment, the function given a VM of its own (Add-VMAssignableDevice, Start-VM).
# The function is mounted back and enabled whatever happens. Needs an administrator, Hyper-V's
# PowerShell module, and a function the host can spare: PM M401 used jupiter's SD card
# reader, with no card in it.
param(
    [Parameter(Mandatory = $true)][string]$LocationPath
)
$ErrorActionPreference = 'Continue'
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class W {
    [DllImport("WinHvPlatform.dll")] public static extern int WHvCreatePartition(out IntPtr p);
    [DllImport("WinHvPlatform.dll")] public static extern int WHvSetPartitionProperty(IntPtr p, uint code, byte[] value, uint size);
    [DllImport("WinHvPlatform.dll")] public static extern int WHvGetPartitionProperty(IntPtr p, uint code, byte[] value, uint size, out uint written);
    [DllImport("WinHvPlatform.dll")] public static extern int WHvSetupPartition(IntPtr p);
    [DllImport("WinHvPlatform.dll")] public static extern int WHvDeletePartition(IntPtr p);
    [DllImport("WinHvPlatform.dll")] public static extern int WHvCreateVirtualProcessor(IntPtr p, uint index, uint flags);
    [DllImport("WinHvPlatform.dll")] public static extern int WHvDeleteVirtualProcessor(IntPtr p, uint index);
    [DllImport("WinHvPlatform.dll")] public static extern int WHvAllocateVpciResource(IntPtr provider, uint flags, byte[] desc, uint size, out IntPtr h);
    [DllImport("WinHvPlatform.dll")] public static extern int WHvCreateVpciDevice(IntPtr p, ulong id, IntPtr res, uint flags, IntPtr ev);
    [DllImport("WinHvPlatform.dll")] public static extern int WHvDeleteVpciDevice(IntPtr p, ulong id);
    [DllImport("WinHvPlatform.dll")] public static extern int WHvGetVpciDeviceProperty(IntPtr p, ulong id, uint code, byte[] buf, uint size, out uint written);
    [DllImport("WinHvPlatform.dll")] public static extern int WHvReadVpciDeviceRegister(IntPtr p, ulong id, byte[] reg, byte[] data);
    [DllImport("WinHvPlatform.dll")] public static extern int WHvSetVpciDevicePowerState(IntPtr p, ulong id, int state);
    [DllImport("WinHvPlatform.dll")] public static extern int WHvMapVpciDeviceMmioRanges(IntPtr p, ulong id, out uint count, out IntPtr mappings);
    [DllImport("WinHvPlatform.dll")] public static extern int WHvUnmapVpciDeviceMmioRanges(IntPtr p, ulong id);
    [DllImport("WinHvPlatform.dll")] public static extern int WHvMapVpciDeviceInterrupt(IntPtr p, ulong id, uint index, uint count, byte[] target, out ulong addr, out uint data);
    [DllImport("WinHvPlatform.dll")] public static extern int WHvUnmapVpciDeviceInterrupt(IntPtr p, ulong id, uint index);
    [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
}
"@
function hx($hr) { '0x{0:X8}' -f $hr }
$clock = [Diagnostics.Stopwatch]::StartNew()
function at { '{0,8:n1} ms' -f $clock.Elapsed.TotalMilliseconds }
$one = [BitConverter]::GetBytes([uint32]1)
# WHV_VPCI_DEVICE_REGISTER: its space (-1 configuration, 0-5 a BAR), size and offset.
function register($space, $size, $offset) {
    $r = New-Object byte[] 16
    [BitConverter]::GetBytes([int32]$space).CopyTo($r, 0)
    [BitConverter]::GetBytes([uint32]$size).CopyTo($r, 4)
    [BitConverter]::GetBytes([uint64]$offset).CopyTo($r, 8)
    $r
}
# A partition of one vCPU, its APIC emulated as shards' are, `$allow` setting
# AllowDeviceAssignment (0x0c) before its setup, after it, or not at all; the function's
# resource made its device with `$flags` (1 physically backed, 0 not); what that device
# reaches, where it is made.
function Attempt($label, $allow, $flags, $instance) {
    $p = [IntPtr]::Zero; $h = [IntPtr]::Zero; $line = "  ${label}:"
    [void][W]::WHvCreatePartition([ref]$p)
    [void][W]::WHvSetPartitionProperty($p, 0x1fff, $one, 4)
    [void][W]::WHvSetPartitionProperty($p, 0x1005, $one, 4)
    if ($allow -eq 'before') { $line += " AllowDeviceAssignment before setup $(hx ([W]::WHvSetPartitionProperty($p, 0x0c, $one, 4)));" }
    $line += " setup $(hx ([W]::WHvSetupPartition($p)));"
    [void][W]::WHvCreateVirtualProcessor($p, 0, 0)
    if ($allow -eq 'after') { $line += " AllowDeviceAssignment after setup $(hx ([W]::WHvSetPartitionProperty($p, 0x0c, $one, 4)));" }
    $got = New-Object byte[] 4; $written = [uint32]0
    $hr = [W]::WHvGetPartitionProperty($p, 0x0c, $got, 4, [ref]$written)
    $line += " reads back $(hx $hr) $([BitConverter]::ToUInt32($got, 0));"
    $descriptor = New-Object byte[] 412
    $name = [Text.Encoding]::Unicode.GetBytes($instance)
    [Array]::Copy($name, $descriptor, [Math]::Min($name.Length, 398))
    $hr = [W]::WHvAllocateVpciResource([IntPtr]::Zero, 0, $descriptor, 412, [ref]$h)
    $line += " WHvAllocateVpciResource $(hx $hr);"
    if ($hr -eq 0) {
        $hr = [W]::WHvCreateVpciDevice($p, 1, $h, $flags, [IntPtr]::Zero)
        $line += " WHvCreateVpciDevice(flags $flags) $(hx $hr)"
        $line
        if ($hr -eq 0) { Reach $p }
        [void][W]::WHvDeleteVpciDevice($p, 1)
        [void][W]::CloseHandle($h)
    } else {
        $line
    }
    [void][W]::WHvDeleteVirtualProcessor($p, 0)
    [void][W]::WHvDeletePartition($p)
}
# What device 1 of partition `$p` reaches of the function.
function Reach($p) {
    $ids = New-Object byte[] 12; $written = [uint32]0
    $hr = [W]::WHvGetVpciDeviceProperty($p, 1, 1, $ids, 12, [ref]$written)
    "    HardwareIDs $(hx $hr): vendor 0x{0:X4} device 0x{1:X4} class 0x{2:X2}{3:X2}{4:X2}" -f [BitConverter]::ToUInt16($ids, 0), [BitConverter]::ToUInt16($ids, 2), $ids[7], $ids[6], $ids[5]
    $dword = New-Object byte[] 4
    $hr = [W]::WHvReadVpciDeviceRegister($p, 1, (register -1 4 0), $dword)
    "    configuration space at 0: $(hx $hr) 0x{0:X8}" -f [BitConverter]::ToUInt32($dword, 0)
    if ($hr -ne 0) { return }
    "    D0 $(hx ([W]::WHvSetVpciDevicePowerState($p, 1, 1)))"
    $count = [uint32]0; $mappings = [IntPtr]::Zero
    $hr = [W]::WHvMapVpciDeviceMmioRanges($p, 1, [ref]$count, [ref]$mappings)
    "    WHvMapVpciDeviceMmioRanges $(hx $hr): $count ranges"
    for ($i = 0; $i -lt $count; $i++) {
        $e = [IntPtr]::Add($mappings, $i * 32)
        "      BAR {0}, flags {1}, 0x{2:X} bytes at 0x{3:X}" -f [Runtime.InteropServices.Marshal]::ReadInt32($e, 0), [Runtime.InteropServices.Marshal]::ReadInt32($e, 4), [Runtime.InteropServices.Marshal]::ReadInt64($e, 8), [Runtime.InteropServices.Marshal]::ReadInt64($e, 16)
    }
    if ($hr -eq 0) { [void][W]::WHvUnmapVpciDeviceMmioRanges($p, 1) }
    # WHV_VPCI_INTERRUPT_TARGET: vector 0x40, no flags, one processor, vCPU 0.
    $target = New-Object byte[] 16
    [BitConverter]::GetBytes([uint32]0x40).CopyTo($target, 0)
    [BitConverter]::GetBytes([uint32]1).CopyTo($target, 8)
    $address = [uint64]0; $data = [uint32]0
    $hr = [W]::WHvMapVpciDeviceInterrupt($p, 1, 0, 1, $target, [ref]$address, [ref]$data)
    "    WHvMapVpciDeviceInterrupt $(hx $hr): address 0x{0:X} data 0x{1:X}" -f $address, $data
    if ($hr -eq 0) { [void][W]::WHvUnmapVpciDeviceInterrupt($p, 1, 0) }
    [void][W]::WHvSetVpciDevicePowerState($p, 1, 4)
}
$device = Get-PnpDevice -PresentOnly | Where-Object { ($_ | Get-PnpDeviceProperty DEVPKEY_Device_LocationPaths -ErrorAction SilentlyContinue).Data -contains $LocationPath } | Select-Object -First 1
if (-not $device) { "no present device at $LocationPath"; exit 1 }
if ($device.Status -ne 'OK') { "device at $LocationPath is not enabled and healthy; leaving it unchanged"; exit 1 }
$pci = $device.InstanceId
$vm = 'shards-passthrough-' + [guid]::NewGuid().ToString('N')
$vmdir = Join-Path $env:TEMP $vm
$disabled = $false; $dismounted = $false; $created = $false
"$(at) $($device.FriendlyName) ($pci): $($device.Status)"
try {
    Disable-PnpDevice -InstanceId $pci -Confirm:$false -ErrorAction Stop
    $disabled = $true
    "$(at) disabled"
    Dismount-VMHostAssignableDevice -LocationPath $LocationPath -Force -ErrorAction Stop
    $dismounted = $true
    "$(at) dismounted"
    $instance = (Get-VMHostAssignableDevice | Where-Object { $_.LocationPath -eq $LocationPath }).InstanceID
    "$(at) dismounted as $instance"
    "== WHP"
    Attempt 'allowed before setup, physically backed' 'before' 1 $instance
    Attempt 'allowed after setup, physically backed' 'after' 1 $instance
    Attempt 'not allowed, physically backed' 'none' 1 $instance
    Attempt 'not allowed, not physically backed' 'none' 0 $instance
    "$(at) WHP asked"
    "== Hyper-V's own Discrete Device Assignment"
    New-VM -Name $vm -Generation 2 -MemoryStartupBytes 512MB -NoVHD -Path $vmdir -ErrorAction Stop | Out-Null
    $created = $true
    Set-VM -Name $vm -AutomaticStopAction TurnOff -StaticMemory -LowMemoryMappedIoSpace 512MB -HighMemoryMappedIoSpace 1GB -ErrorAction Stop
    Add-VMAssignableDevice -VMName $vm -LocationPath $LocationPath -ErrorAction Stop
    "$(at) Add-VMAssignableDevice: done"
    try { Start-VM -Name $vm -ErrorAction Stop; Start-Sleep -Seconds 3; "$(at) Start-VM: $((Get-VM -Name $vm).State)" } catch { "$(at) Start-VM: $($_.Exception.Message -replace '\s+', ' ')" }
} catch {
    "$(at) $($_.Exception.Message)"
} finally {
    if ($created) {
        Stop-VM -Name $vm -TurnOff -Force -ErrorAction SilentlyContinue
        Get-VMAssignableDevice -VMName $vm -ErrorAction SilentlyContinue | Remove-VMAssignableDevice -ErrorAction SilentlyContinue
        Remove-VM -Name $vm -Force -ErrorAction SilentlyContinue
        "$(at) VM removed"
    }
    Remove-Item -Recurse -Force $vmdir -ErrorAction SilentlyContinue
    if ($dismounted -and (Get-VMHostAssignableDevice | Where-Object { $_.LocationPath -eq $LocationPath })) {
        try { Mount-VMHostAssignableDevice -LocationPath $LocationPath -ErrorAction Stop; "$(at) mounted" } catch { "$(at) mounting: $($_.Exception.Message)" }
    }
    if ($disabled) {
        try { Enable-PnpDevice -InstanceId $pci -Confirm:$false -ErrorAction Stop; "$(at) enabled" } catch { "$(at) enabling: $($_.Exception.Message)" }
    }
    "$(at) $((Get-PnpDevice -InstanceId $pci).Status)"
}
