# PM M414's host: what Windows reports before and after enabling Hyper-V under QEMU/HVF.
# Read-only except for the output file. Run elevated to read the optional features and
# hypervisor event logs. Keep this evidence even when no partition can run.
param([string]$Output = 'C:\shards-step10\host.txt')

$ErrorActionPreference = 'Continue'
Start-Transcript -Path $Output -Append
Get-Date -Format o

'== Windows and virtualization'
Get-ComputerInfo -Property WindowsProductName, WindowsVersion, OsName, OsVersion,
    OsBuildNumber, OsArchitecture, HyperV* | Format-List | Out-String
Get-CimInstance Win32_OperatingSystem |
    Select-Object Caption, Version, BuildNumber, OSArchitecture, LastBootUpTime |
    Format-List | Out-String
Get-CimInstance Win32_ComputerSystem |
    Select-Object Manufacturer, Model, HypervisorPresent | Format-List | Out-String
Get-CimInstance Win32_Processor |
    Select-Object Name, Architecture, VirtualizationFirmwareEnabled,
        VMMonitorModeExtensions, SecondLevelAddressTranslationExtensions |
    Format-List | Out-String

'== optional features'
foreach ($feature in 'HypervisorPlatform', 'VirtualMachinePlatform', 'Microsoft-Hyper-V-All') {
    Get-WindowsOptionalFeature -Online -FeatureName $feature |
        Select-Object FeatureName, State | Format-List | Out-String
}

'== boot configuration'
bcdedit /enum '{current}'

'== hypervisor events in System'
Get-WinEvent -FilterHashtable @{
    LogName = 'System'; ProviderName = 'Microsoft-Windows-Hyper-V-Hypervisor'
} -MaxEvents 30 | Select-Object TimeCreated, Id, LevelDisplayName, Message |
    Format-List | Out-String

'== hypervisor event channels'
Get-WinEvent -ListLog '*Hyper-V-Hypervisor*' | ForEach-Object {
    $_ | Select-Object LogName, IsEnabled, RecordCount | Format-List | Out-String
    if ($_.IsEnabled -and $_.RecordCount -gt 0) {
        Get-WinEvent -LogName $_.LogName -MaxEvents 30 |
            Select-Object TimeCreated, Id, LevelDisplayName, Message |
            Format-List | Out-String
    }
}

'== Windows Hypervisor Platform'
try {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class Step10Whp {
    [DllImport("WinHvPlatform.dll")]
    public static extern int WHvGetCapability(uint code, byte[] buffer, uint size, out uint written);
    [DllImport("WinHvPlatform.dll")]
    public static extern int WHvCreatePartition(out IntPtr partition);
    [DllImport("WinHvPlatform.dll")]
    public static extern int WHvDeletePartition(IntPtr partition);
}
'@
    $buffer = New-Object byte[] 64
    $written = [uint32]0
    $hr = [Step10Whp]::WHvGetCapability(0, $buffer, 64, [ref]$written)
    'WHvGetCapability(HypervisorPresent): hr=0x{0:X8} written={1} present={2}' -f $hr, $written, $buffer[0]
    $hr = [Step10Whp]::WHvGetCapability(1, $buffer, 64, [ref]$written)
    'WHvGetCapability(Features): hr=0x{0:X8} written={1} bits=0x{2:X16}' -f $hr, $written, ([BitConverter]::ToUInt64($buffer, 0))
    $partition = [IntPtr]::Zero
    $hr = [Step10Whp]::WHvCreatePartition([ref]$partition)
    'WHvCreatePartition: hr=0x{0:X8}' -f $hr
    if ($hr -eq 0) { [void][Step10Whp]::WHvDeletePartition($partition) }
} catch {
    "WinHvPlatform: $_"
}

Stop-Transcript
