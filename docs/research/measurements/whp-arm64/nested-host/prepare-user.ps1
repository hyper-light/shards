# Apply the test fixture's prerequisites as the Windows host's administrator, before
# rebooting and starting the complete suite in a fresh standard-user logon.
param([string]$User = 'runner')
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Prepare the fixture from an elevated administrator logon'
}
$account = Get-LocalUser -Name $User
$admins = Get-LocalGroup -SID 'S-1-5-32-544'
if (@(Get-LocalGroupMember -Group $admins.Name | Where-Object { $_.SID.Value -eq $account.SID.Value }).Count) {
    throw 'The test account must remain a standard user'
}
# CreateSymbolicLinkW's unprivileged flag requires Developer Mode. This permits the
# existing bind-mount test to create its host symlink without elevating the test.
$key = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock'
if (-not (Test-Path $key)) { New-Item -Path $key | Out-Null }
New-ItemProperty -Path $key -Name AllowDevelopmentWithoutDevLicense -PropertyType DWord -Value 1 -Force | Out-Null
# ETW providers need no elevation. The test's consumer also opens a trace session,
# which Windows permits to members of Performance Log Users (the localized group SID).
$trace = Get-LocalGroup -SID 'S-1-5-32-559'
if (-not @(Get-LocalGroupMember -Group $trace.Name | Where-Object { $_.SID.Value -eq $account.SID.Value }).Count) {
    Add-LocalGroupMember -Group $trace.Name -Member $account
}
Write-Output "Fixture prepared: user=$($account.Name) SID=$($account.SID.Value); Developer Mode=1; trace group=$($trace.SID.Value)"
Write-Output 'Reboot, then verify the fixture with probe-user.ps1 in a fresh standard-user logon.'
