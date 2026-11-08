# Exercise the permissions the full suite needs, from its standard-user logon.
param([string]$User = 'runner')
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if ($identity.User.Value -ne (Get-LocalUser -Name $User).SID.Value) { throw 'Unexpected test user' }
if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Tests must run without elevation' }
if (-not @($identity.Groups | Where-Object { $_.Value -eq 'S-1-5-32-559' }).Count) {
    throw 'This logon lacks Performance Log Users membership; use a fresh logon after preparation'
}
$key = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock'
if ((Get-ItemProperty -Path $key -Name AllowDevelopmentWithoutDevLicense).AllowDevelopmentWithoutDevLicense -ne 1) {
    throw 'Developer Mode is required for the host symlink fixture'
}
Write-Output "Standard user=$($identity.Name) SID=$($identity.User.Value); elevated=False; trace group present; Developer Mode=1"
Add-Type -TypeDefinition @'
using System.Runtime.InteropServices;
public static class ShardsUserProbeLink {
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    [return: MarshalAs(UnmanagedType.U1)]
    public static extern bool CreateSymbolicLinkW(string link, string target, uint flags);
}
'@
$session = 'shards-user-probe-' + [Guid]::NewGuid().ToString('N')
$dir = Join-Path $env:TEMP $session
$started = $false
New-Item -ItemType Directory -Path $dir | Out-Null
try {
    $target = Join-Path $dir 'target'
    $link = Join-Path $dir 'link'
    [IO.File]::WriteAllText($target, 'shards-host-symlink')
    if (-not [ShardsUserProbeLink]::CreateSymbolicLinkW($link, $target, 2)) {
        throw "Unprivileged file symlink failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())"
    }
    if ([IO.File]::ReadAllText($link) -ne 'shards-host-symlink') { throw 'Host symlink did not resolve to its file' }
    Write-Output 'Unprivileged CreateSymbolicLinkW and read through its link passed'
    # Enable shards' provider in an actual short-lived session. The unchanged ETW E2E
    # subsequently verifies the events a real microVM writes to this provider.
    & "$env:SystemRoot\System32\logman.exe" create trace $session -o (Join-Path $dir 'trace.etl') -p '{a3693192-9ed6-46d2-a981-f8226c8363bd}' -ets
    $code = $LASTEXITCODE
    $started = $code -eq 0
    if (-not $started) { throw "Native ETW session creation failed: $code" }
    & "$env:SystemRoot\System32\logman.exe" stop $session -ets
    if ($LASTEXITCODE -ne 0) { throw "Native ETW session stop failed: $LASTEXITCODE" }
    $started = $false
    Write-Output 'Native ETW session create/enable/stop passed as the standard user'
} finally {
    if ($started) {
        & "$env:SystemRoot\System32\logman.exe" stop $session -ets
        if ($LASTEXITCODE -ne 0) { Write-Error "ETW cleanup failed for $session" -ErrorAction Continue }
    }
    Remove-Item -LiteralPath $dir -Recurse -Force
}
