# Whether this host can run a VM under the Windows Hypervisor Platform (WHP), the API a
# Windows backend of shards' VMM would be built on (Microsoft's WinHvPlatform.h): the
# processor's virtualization as Windows reports it, the optional features' states, and
# what WHP itself answers: WHvGetCapability's WHvCapabilityCodeHypervisorPresent (0), and
# whether a partition can be made. Then the feature enabled without a restart, which a
# hosted runner cannot take, and WHP asked again. Read-only but for that feature.
$ErrorActionPreference = 'Continue'
"== os"
[System.Environment]::OSVersion | Format-List | Out-String
"== processor"
Get-CimInstance Win32_Processor |
    Select-Object Name, VirtualizationFirmwareEnabled, VMMonitorModeExtensions, SecondLevelAddressTranslationExtensions |
    Format-List | Out-String
"== hypervisor present (Win32_ComputerSystem)"
(Get-CimInstance Win32_ComputerSystem).HypervisorPresent
"== features"
foreach ($f in 'HypervisorPlatform', 'Microsoft-Hyper-V', 'Microsoft-Hyper-V-Hypervisor', 'VirtualMachinePlatform') {
    try {
        Get-WindowsOptionalFeature -Online -FeatureName $f | Select-Object FeatureName, State | Format-List | Out-String
    } catch {
        "$f : $_"
    }
}
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class Whp {
    [DllImport("WinHvPlatform.dll")]
    public static extern int WHvGetCapability(int code, byte[] buffer, uint size, out uint written);
    [DllImport("WinHvPlatform.dll")]
    public static extern int WHvCreatePartition(out IntPtr partition);
    [DllImport("WinHvPlatform.dll")]
    public static extern int WHvDeletePartition(IntPtr partition);
}
"@
function Ask-Whp {
    "WinHvPlatform.dll present: " + (Test-Path "$env:SystemRoot\System32\WinHvPlatform.dll")
    try {
        $buf = New-Object byte[] 8
        $written = [uint32]0
        $hr = [Whp]::WHvGetCapability(0, $buf, 8, [ref]$written)
        "WHvGetCapability(HypervisorPresent): hr=0x{0:X8} written={1} present={2}" -f $hr, $written, $buf[0]
        $partition = [IntPtr]::Zero
        $hr = [Whp]::WHvCreatePartition([ref]$partition)
        "WHvCreatePartition: hr=0x{0:X8}" -f $hr
        if ($hr -eq 0) { [void][Whp]::WHvDeletePartition($partition) }
    } catch {
        "WinHvPlatform: $_"
    }
}
"== WHP as found"
Ask-Whp
"== HypervisorPlatform enabled without a restart"
try {
    Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform -NoRestart -All |
        Select-Object RestartNeeded | Format-List | Out-String
} catch {
    "enabling: $_"
}
Ask-Whp
