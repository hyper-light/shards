# M401's approved, two-reboot IOMMU experiment. Run each action separately; state and
# logs survive a disconnected client. The original loader and default are never set.
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('Prepare', 'BootProbe', 'Inspect', 'Return', 'Verify')]
    [string]$Action,
    [string]$StateDir = (Join-Path $env:USERPROFILE 'g121-p13-boot-20261103'),
    [switch]$Reboot
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$original = '{dffce7cc-f00e-11f0-9721-831b2d8db458}'
$readerId = 'PCI\VEN_17A0&DEV_9755&SUBSYS_202F1043&REV_01\4&632F499&0&0012'
$guid = '\{[0-9a-fA-F]{8}-(?:[0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}\}'
[void](New-Item -ItemType Directory -Path $StateDir -Force)
$statePath = Join-Path $StateDir 'state.json'
$logPath = Join-Path $StateDir 'experiment.log'

function Log([string]$Text) {
    $line = "[$([DateTime]::UtcNow.ToString('o'))] $Text"
    [IO.File]::AppendAllText($logPath, $line + [Environment]::NewLine)
    [Console]::WriteLine($line)
}
function Bcd([string[]]$BcdArgs) {
    $text = (& bcdedit.exe @BcdArgs | Out-String).TrimEnd()
    $code = $LASTEXITCODE
    Log ('bcdedit ' + ($BcdArgs -join ' ') + " -> $code`n$text")
    if ($code -ne 0) { throw "bcdedit failed ($code): $($BcdArgs -join ' ')" }
    return $text
}
function Field([string]$Text, [string]$Name) {
    $m = [regex]::Match($Text, '(?m)^' + [regex]::Escape($Name) + '[ \t]+([^\r\n]+)')
    if ($m.Success) { return $m.Groups[1].Value.Trim() }
    return $null
}
function Save($State) {
    $tmp = $statePath + '.tmp'
    $State | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $tmp -Encoding UTF8
    Move-Item -LiteralPath $tmp -Destination $statePath -Force
}
function Facts {
    $os = Get-CimInstance Win32_OperatingSystem
    $current = Bcd -BcdArgs @('/enum', '{current}', '/v')
    $manager = Bcd -BcdArgs @('/enum', '{bootmgr}', '/v')
    $bl = Get-BitLockerVolume -MountPoint 'C:'
    $reader = Get-PnpDevice -PresentOnly -InstanceId $readerId
    $f = [pscustomobject]@{
        Utc = [DateTime]::UtcNow.ToString('o')
        HostName = $env:COMPUTERNAME
        Os = $os.Caption
        BootTime = $os.LastBootUpTime.ToUniversalTime().ToString('o')
        Current = Field $current 'identifier'
        Default = Field $manager 'default'
        DisplayOrder = Field $manager 'displayorder'
        BootSequence = Field $manager 'bootsequence'
        IommuPolicy = Field $current 'hypervisoriommupolicy'
        BitLockerProtection = [int]$bl.ProtectionStatus
        BitLockerVolume = [int]$bl.VolumeStatus
        SecureBoot = [bool](Confirm-SecureBootUEFI)
        ReaderStatus = [string]$reader.Status
        CurrentText = $current
        ManagerText = $manager
    }
    Log ($f | ConvertTo-Json -Depth 5 -Compress)
    return $f
}
function Protected($Facts) {
    if ($Facts.BitLockerProtection -ne 1 -or -not $Facts.SecureBoot) {
        throw 'BitLocker protection and Secure Boot must stay enabled'
    }
    if ($Facts.ReaderStatus -ne 'OK') { throw 'SD reader is not healthy' }
}
function Idle {
    $vms = @(Get-VM | Where-Object { $_.State -ne 'Off' })
    # vmms.exe is Hyper-V's management service, not a running guest.
    $names = '^(cargo|rustc|rust-lld|clang|clang-cl|cl|link|lld-link|cmake|ninja|msbuild|dotnet|qemu-system.*|shards.*|vmm(?:[-_].*)?|boot[-_].*|containers[-_].*|budget[-_].*|pci[-_].*)[.]exe$'
    $busy = @(Get-CimInstance Win32_Process | Where-Object { $_.Name -match $names } |
        Select-Object Name, ProcessId, ParentProcessId, SessionId)
    Log ('Active VMs: ' + ($vms | Select-Object Name, State | ConvertTo-Json -Compress))
    Log ('Build/VM/test processes: ' + ($busy | ConvertTo-Json -Compress))
    if ($vms.Count -ne 0 -or $busy.Count -ne 0) { throw 'Host has live VM/build/test work' }
}
function Schedule($State, [string]$Stage) {
    if (-not $Reboot) { throw 'This action requires -Reboot' }
    $State.Stage = $Stage
    Save $State
    Log 'Scheduling the approved reboot in 20 seconds'
    & shutdown.exe /r /t 20 /c 'shards step 13 approved IOMMU boot-policy experiment'
    if ($LASTEXITCODE -ne 0) { throw "shutdown failed: $LASTEXITCODE" }
}

$state = $null
try {
    Log "ACTION $Action"
    $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
    if (-not $admin) { throw 'Administrator required' }
    if ($Action -eq 'Prepare') {
        if (Test-Path -LiteralPath $statePath) { throw 'Existing experiment state: inspect it before preparing again' }
        $before = Facts
        Protected $before
        Idle
        $fvePolicy = Test-Path 'HKLM:\SOFTWARE\Policies\Microsoft\FVE'
        Log ('FVE policy key present: ' + $fvePolicy)
        if ($fvePolicy) { throw 'FVE policy key differs from the approved preflight; inspect before changing boot policy' }
        if ($before.HostName -ne 'JUPITER' -or $before.Current -ne $original -or
            $before.Default -ne $original -or $before.DisplayOrder -ne $original -or
            $before.BootSequence -or $before.IommuPolicy) {
            throw 'Baseline differs from the approved original entry/default/policy/sequence'
        }
        $state = [pscustomobject]@{
            Experiment = 'shards-step13-20261103'
            Description = ('shards DDA probe ' + [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss'))
            Stage = 'Preparing'
            OriginalGuid = $original
            ProbeGuid = $null
            Before = $before
            Probe = $null
            Restored = $null
            Failure = $null
        }
        Save $state
        $null = Bcd -BcdArgs @('/export', (Join-Path $StateDir 'bcd-before.bak'))
        $copied = Bcd -BcdArgs @('/copy', '{current}', '/d', $state.Description)
        $m = [regex]::Match($copied, $guid)
        if (-not $m.Success -or $m.Value -eq $original) { throw 'No unique copied loader GUID' }
        $state.ProbeGuid = $m.Value
        Save $state
        $null = Bcd -BcdArgs @('/set', $state.ProbeGuid, 'hypervisoriommupolicy', 'enable')
        $null = Bcd -BcdArgs @('/displayorder', $state.ProbeGuid, '/remove')
        $null = Bcd -BcdArgs @('/bootsequence', $state.ProbeGuid)
        $copy = Bcd -BcdArgs @('/enum', $state.ProbeGuid, '/v')
        $after = Facts
        if ((Field $copy 'hypervisoriommupolicy') -ne 'Enable' -or
            (Field $copy 'description') -ne $state.Description -or
            $after.Current -ne $original -or $after.Default -ne $original -or
            $after.DisplayOrder -ne $before.DisplayOrder -or $after.BootSequence -ne $state.ProbeGuid) {
            throw 'Prepared loader or boot-manager verification failed'
        }
        $state.Stage = 'Prepared'
        Save $state
        Log ('PREPARED ' + $state.ProbeGuid)
    } else {
        $state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
        if ($state.Experiment -ne 'shards-step13-20261103' -or
            $state.OriginalGuid -ne $original -or $state.ProbeGuid -notmatch ('^' + $guid + '$') -or
            $state.ProbeGuid -eq $original) { throw 'Invalid experiment state' }
        $now = Facts
        Protected $now
        if ($now.Default -ne $original -or $now.DisplayOrder -ne $state.Before.DisplayOrder) {
            throw 'Original default/display order changed'
        }
        switch ($Action) {
            'BootProbe' {
                Idle
                if ($now.Current -ne $original -or $now.BootSequence -ne $state.ProbeGuid) {
                    throw 'Probe is not the original loader next one-time boot'
                }
                Schedule $state 'ProbeBootRequested'
            }
            'Inspect' {
                if ($now.Current -ne $state.ProbeGuid -or $now.IommuPolicy -ne 'Enable' -or
                    $now.BootTime -eq $state.Before.BootTime) { throw 'Not booted into the enabled probe entry' }
                $state.Probe = $now
                $state.Stage = 'ProbeBootVerified'
                Save $state
                Log 'PROBE BOOT VERIFIED; run probe.ps1 and, only if assignment is accepted, the spare-reader probe'
            }
            'Return' {
                Idle
                if ($now.Current -ne $state.ProbeGuid) { throw 'Return requires the probe loader to be current' }
                if ($now.BootSequence) {
                    if ($now.BootSequence -ne $state.ProbeGuid) { throw 'Unexpected one-time sequence' }
                    $null = Bcd -BcdArgs @('/bootsequence', $state.ProbeGuid, '/remove')
                }
                Schedule $state 'ReturnBootRequested'
            }
            'Verify' {
                if ($now.Current -ne $original -or $now.IommuPolicy -ne $state.Before.IommuPolicy -or
                    ($state.Probe -and $now.BootTime -eq $state.Probe.BootTime)) {
                    throw 'Original boot entry/policy not restored by a fresh boot'
                }
                $copy = Bcd -BcdArgs @('/enum', $state.ProbeGuid, '/v')
                if ((Field $copy 'description') -ne $state.Description) { throw 'Temporary entry identity changed' }
                if ($now.BootSequence) {
                    if ($now.BootSequence -ne $state.ProbeGuid) { throw 'Unexpected one-time sequence' }
                    $null = Bcd -BcdArgs @('/bootsequence', $state.ProbeGuid, '/remove')
                }
                $null = Bcd -BcdArgs @('/delete', $state.ProbeGuid, '/cleanup')
                $restored = Facts
                Protected $restored
                if ($restored.ManagerText -ne $state.Before.ManagerText -or
                    $restored.CurrentText -ne $state.Before.CurrentText) {
                    throw 'Restored BCD listings differ from the saved baseline'
                }
                $state.Restored = $restored
                $state.Stage = 'Restored'
                Save $state
                Log 'RESTORED: original loader and boot manager match baseline; reader, BitLocker and Secure Boot healthy'
            }
        }
    }
} catch {
    $failure = $_.Exception.Message
    Log "FAILED: $failure"
    # A preparation failure is unwound before any reboot, touching only our copy.
    if ($Action -eq 'Prepare' -and $state -and $state.ProbeGuid) {
        try {
            $current = Bcd -BcdArgs @('/enum', '{current}', '/v')
            if ((Field $current 'identifier') -eq $state.ProbeGuid) { throw 'Cannot delete the current loader' }
            $null = Bcd -BcdArgs @('/delete', $state.ProbeGuid, '/cleanup')
            $state.Stage = 'PreparationAbortedAndCopyRemoved'
        } catch {
            Log ('PREPARATION CLEANUP NEEDS ATTENTION: ' + $_.Exception.Message)
            $state.Stage = 'PreparationCleanupNeeded'
        }
    }
    if ($state) { $state.Failure = $failure; Save $state }
    throw
}
