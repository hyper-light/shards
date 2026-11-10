$ErrorActionPreference = 'Stop'
$SourceRevision = '122332036eb689c33fd52b4a065c53c7dc5e4530'
$SyncedSource = (Get-Location).Path
$UserRoot = [Environment]::GetFolderPath('UserProfile')
$PrivateSource = Join-Path $UserRoot 'managed-borrowed-windows-check'
$PrivateTarget = Join-Path $UserRoot 'managed-borrowed-windows-target'
$EvidenceRoot = Join-Path $UserRoot 'managed-borrowed-windows-evidence'
$Evidence = Join-Path $EvidenceRoot '12233203-powershell'
New-Item -ItemType Directory -Force $Evidence | Out-Null
Copy-Item $PSCommandPath (Join-Path $Evidence 'driver.ps1')

function Invoke-Recorded {
    param([string]$Name, [string]$Program, [string[]]$ToolArgs, [string]$Directory = $PrivateSource)
    $Executable = (Get-Command $Program -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
    $PreviousLocation = Get-Location
    $PreviousPreference = $ErrorActionPreference
    Set-Location $Directory
    try {
        # Windows PowerShell5 treats native stderr as an error stream even for
        # successful tools. Preserve it, then use the actual native exit status.
        $ErrorActionPreference = 'Continue'
        & $Executable @ToolArgs *> (Join-Path $Evidence ($Name + '.log'))
        $ResultCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $PreviousPreference
        Set-Location $PreviousLocation
    }
    @{ args = @($Executable) + $ToolArgs; cwd = $Directory; exit = $ResultCode } |
        ConvertTo-Json -Depth 5 | Set-Content -Encoding UTF8 (Join-Path $Evidence ($Name + '.json'))
    Write-Host ($Name + ': exit=' + $ResultCode)
    return $ResultCode
}

if (-not (Test-Path $PrivateSource)) {
    $ResultCode = Invoke-Recorded -Name 'clone' -Program 'git' -ToolArgs @('clone', '--no-hardlinks', '--no-checkout', $SyncedSource, $PrivateSource) -Directory $SyncedSource
    if ($ResultCode -ne 0) { exit $ResultCode }
    $ResultCode = Invoke-Recorded -Name 'checkout' -Program 'git' -ToolArgs @('checkout', '--detach', $SourceRevision)
    if ($ResultCode -ne 0) { exit $ResultCode }
}
$ActualRevision = (& git -C $PrivateSource rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0) { throw 'Private source revision query failed' }
$DirtySource = & git -C $PrivateSource status --porcelain
if ($LASTEXITCODE -ne 0 -or $ActualRevision -ne $SourceRevision -or $DirtySource) {
    throw 'Private clone is not the assigned clean source; refusing mutation'
}
$env:CARGO_TARGET_DIR = $PrivateTarget
@{ head = $ActualRevision; source = $PrivateSource; target = $PrivateTarget; evidence = $Evidence;
    os = [Environment]::OSVersion.VersionString; architecture = $env:PROCESSOR_ARCHITECTURE;
    powershell = $PSVersionTable.PSVersion.ToString() } |
    ConvertTo-Json | Set-Content -Encoding UTF8 (Join-Path $Evidence 'identity.json')
Write-Host ('source=' + $ActualRevision + ' evidence=' + $Evidence)
Invoke-Recorded -Name 'rustc' -Program 'rustc' -ToolArgs @('-Vv') | Out-Null
Invoke-Recorded -Name 'cargo' -Program 'cargo' -ToolArgs @('-Vv') | Out-Null
$SourcePaths = @('Cargo.lock', 'crates/api/src/borrowed.rs', 'crates/api/src/channel.rs', 'crates/api/src/stream.rs', 'crates/afunix/src/lib.rs')
$SourceHashes = foreach ($RelativePath in $SourcePaths) {
    $Digest = (Get-FileHash -Algorithm SHA256 (Join-Path $PrivateSource $RelativePath)).Hash.ToLowerInvariant()
    $Digest + '  ' + $RelativePath
}
$SourceHashes | Set-Content -Encoding UTF8 (Join-Path $Evidence 'source.sha256')
$ResultCode = Invoke-Recorded -Name 'build' -Program 'cargo' -ToolArgs @('test', '--locked', '-j4', '--profile', 'test-release', '-p', 'shards-api', '--lib', '--no-run', '--message-format=json-render-diagnostics')
if ($ResultCode -ne 0) { Write-Host 'COMPILE_RED: inspect build.log'; exit $ResultCode }
$Binaries = @(foreach ($Line in Get-Content (Join-Path $Evidence 'build.log')) {
    if (-not $Line.StartsWith('{')) { continue }
    $Record = $Line | ConvertFrom-Json
    if ($Record.reason -eq 'compiler-artifact' -and $Record.target.name -eq 'shards_api' -and $Record.executable) {
        @{ path = $Record.executable; sha256 = (Get-FileHash -Algorithm SHA256 $Record.executable).Hash.ToLowerInvariant() }
    }
})
if ($Binaries.Count -eq 0) { throw 'No exact test executable reported by actual compiler' }
$Binaries | ConvertTo-Json | Set-Content -Encoding UTF8 (Join-Path $Evidence 'binaries.json')
$Results = @{}
foreach ($TestName in @('borrowed', 'stream')) {
    $Results[$TestName] = Invoke-Recorded -Name $TestName -Program 'cargo' -ToolArgs @('test', '--locked', '-j4', '--profile', 'test-release', '-p', 'shards-api', '--lib', ($TestName + '::tests'), '--', '--test-threads=1', '--nocapture')
}
$Results['clippy'] = Invoke-Recorded -Name 'clippy' -Program 'cargo' -ToolArgs @('clippy', '--locked', '-j4', '-p', 'shards-api', '-p', 'shards-afunix', '--all-targets', '--', '-D', 'warnings')
$Results | ConvertTo-Json | Set-Content -Encoding UTF8 (Join-Path $Evidence 'summary.json')
foreach ($ResultCode in $Results.Values) { if ($ResultCode -ne 0) { exit 1 } }
exit 0
