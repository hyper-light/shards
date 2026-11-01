# Whether this host's hypervisor gives a VM a host's PCI function (D154, PM M401, M402):
# Discrete Device Assignment, a function dismounted from its host and assigned to a
# partition, which WHP does through its virtual PCI devices (WHvAllocateVpciResource, then
# WHvCreateVpciDevice, physically backed) in a partition allowed device assignment
# (WHvPartitionPropertyCodeAllowDeviceAssignment). What this host answers, read-only: its
# Windows, WHP's features, an empty virtual PCI resource, the property set on a partition of
# its own (then deleted), Hyper-V's own word where it is installed, the boot entry's
# hypervisor settings, and each PCI function's assignability as Microsoft's survey-dda.ps1
# reads it. assign.ps1 goes on where a host has a function to spare.
$ErrorActionPreference = 'Continue'
"== os"
$os = Get-CimInstance Win32_OperatingSystem
$cv = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
"{0}, build {1}.{2}, EditionID {3}, ProductType {4}" -f $os.Caption, $os.BuildNumber, $cv.UBR, $cv.EditionID, $os.ProductType
(Get-CimInstance Win32_Processor | Select-Object -First 1).Name
"== WHP"
try {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class Whp {
    [DllImport("WinHvPlatform.dll")]
    public static extern int WHvGetCapability(int code, out ulong buffer, uint size, out uint written);
    [DllImport("WinHvPlatform.dll")]
    public static extern int WHvCreatePartition(out IntPtr partition);
    [DllImport("WinHvPlatform.dll")]
    public static extern int WHvSetPartitionProperty(IntPtr partition, int code, byte[] value, uint size);
    [DllImport("WinHvPlatform.dll")]
    public static extern int WHvDeletePartition(IntPtr partition);
    [DllImport("WinHvPlatform.dll")]
    public static extern int WHvAllocateVpciResource(IntPtr provider, int flags, IntPtr descriptor, uint size, out IntPtr resource);
    [DllImport("kernel32.dll")]
    public static extern bool CloseHandle(IntPtr handle);
}
"@
    $value = [uint64]0; $written = [uint32]0
    $hr = [Whp]::WHvGetCapability(1, [ref]$value, 8, [ref]$written)
    # WHV_CAPABILITY_FEATURES (WinHvPlatformDefs.h): VirtualPciDeviceSupport bit 7,
    # IommuSupport bit 8.
    "WHvCapabilityCodeFeatures: hr=0x{0:X8} value=0x{1:X} VirtualPciDeviceSupport={2} IommuSupport={3}" -f $hr, $value, (($value -shr 7) -band 1), (($value -shr 8) -band 1)
    $hr = [Whp]::WHvGetCapability(2, [ref]$value, 8, [ref]$written)
    # WHV_EXTENDED_VM_EXITS: RetargetUnknownVpciDevice bit 11.
    "WHvCapabilityCodeExtendedVmExits: hr=0x{0:X8} value=0x{1:X} RetargetUnknownVpciDevice={2}" -f $hr, $value, (($value -shr 11) -band 1)
    $resource = [IntPtr]::Zero
    $hr = [Whp]::WHvAllocateVpciResource([IntPtr]::Zero, 0, [IntPtr]::Zero, 0, [ref]$resource)
    "WHvAllocateVpciResource(an empty resource): hr=0x{0:X8}" -f $hr
    if ($resource -ne [IntPtr]::Zero) { [void][Whp]::CloseHandle($resource) }
    $partition = [IntPtr]::Zero
    $hr = [Whp]::WHvCreatePartition([ref]$partition)
    "WHvCreatePartition: hr=0x{0:X8}" -f $hr
    if ($hr -eq 0) {
        # WHvPartitionPropertyCodeAllowDeviceAssignment (0x0c), a BOOL, before setup.
        $hr = [Whp]::WHvSetPartitionProperty($partition, 0x0c, [BitConverter]::GetBytes([int32]1), 4)
        "WHvSetPartitionProperty(AllowDeviceAssignment = TRUE): hr=0x{0:X8}" -f $hr
        [void][Whp]::WHvDeletePartition($partition)
    }
} catch {
    "WinHvPlatform: $($_.Exception.Message)"
}
"== Hyper-V"
if (Get-Command Get-VMHost -ErrorAction SilentlyContinue) {
    $vmhost = Get-VMHost
    "IovSupport: $($vmhost.IovSupport)"
    $vmhost.IovSupportReasons | ForEach-Object { "  $_" }
} else {
    "no Hyper-V module"
}
"== boot entry's hypervisor settings"
bcdedit /enum '{current}' | Select-String -Pattern 'hypervisor'
"== device guard"
try {
    $dg = Get-CimInstance -Namespace root\Microsoft\Windows\DeviceGuard -ClassName Win32_DeviceGuard
    "AvailableSecurityProperties: $($dg.AvailableSecurityProperties -join ','); SecurityServicesRunning: $($dg.SecurityServicesRunning -join ',')"
} catch {
    "device guard: $($_.Exception.Message)"
}
"== PCI functions (survey-dda.ps1's checks: BIOS-reserved memory, ACS upstream, the device's type)"
$key = '{3AB22E31-8264-4b4e-9AF5-A8D2D8E33E62}'
$types = @{ 0 = 'conventional PCI'; 1 = 'PCI-X'; 2 = 'PCIe endpoint'; 3 = 'PCIe legacy endpoint'; 4 = 'root complex integrated endpoint'; 5 = 'PCIe treated as PCI'; 8 = 'root port'; 9 = 'upstream switch port'; 10 = 'downstream switch port' }
foreach ($d in (Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like 'PCI*' })) {
    $type = ($d | Get-PnpDeviceProperty "$key 1" -ErrorAction SilentlyContinue).Data
    $rmrr = ($d | Get-PnpDeviceProperty "$key 34" -ErrorAction SilentlyContinue).Data
    $acs = ($d | Get-PnpDeviceProperty "$key 31" -ErrorAction SilentlyContinue).Data
    $location = ($d | Get-PnpDeviceProperty DEVPKEY_Device_LocationPaths -ErrorAction SilentlyContinue).Data | Select-Object -First 1
    $kind = if ($null -eq $type) { 'no PCI type' } else { $types[[int]$type] }
    "{0} [{1}]: {2}, reserved memory {3}, ACS upstream {4}, {5}" -f $d.FriendlyName, $d.Class, $kind, $rmrr, $acs, $location
}
