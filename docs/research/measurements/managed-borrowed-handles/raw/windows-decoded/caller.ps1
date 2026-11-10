param(
    [Parameter(Mandatory=$true)][string]$Binary,
    [Parameter(Mandatory=$true)][string]$BinarySha256,
    [Parameter(Mandatory=$true)][ValidateSet('success','wrong-start','withheld')][string]$Mode,
    [Parameter(Mandatory=$true)][UInt64]$Repetitions,
    [Parameter(Mandatory=$true)][int]$CaseMs,
    [Parameter(Mandatory=$true)][int]$CallMs,
    [Parameter(Mandatory=$true)][int]$CleanupMs,
    [Parameter(Mandatory=$true)][int]$OuterMs,
    [Parameter(Mandatory=$true)][string]$Output,
    [Parameter(Mandatory=$true)][string]$BuildReceipt
)
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
if ($Repetitions -eq 0 -or $CleanupMs -le 0 -or $CleanupMs -ge $CaseMs -or
    $CallMs -le 0 -or $CallMs -gt ($CaseMs-$CleanupMs) -or $OuterMs -le $CaseMs) {
    throw 'explicit positive case/call/cleanup/outer envelope required'
}
if (Test-Path -LiteralPath $Output) { throw 'existing evidence directory preserved' }
$actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $Binary).Hash.ToLowerInvariant()
if ($actual -cne $BinarySha256) { throw 'actual binary hash mismatch' }
$build = Get-Content -Raw -LiteralPath $BuildReceipt | ConvertFrom-Json
if ($build.binary_sha256 -cne $BinarySha256) { throw 'source-matching build receipt mismatch' }
$socket = Join-Path $Output 'owned\v'
if ([Text.Encoding]::UTF8.GetByteCount($socket) -ge 108) { throw 'AF_UNIX sun_path needs shorter owned path' }
[IO.Directory]::CreateDirectory((Join-Path $Output 'owned')) | Out-Null
$names = @{success='native_success_handles';'wrong-start'='native_wrong_start_handles';withheld='native_withheld_reply_handles'}
$testName = 'client::borrowed_handle_tests::'+$names[$Mode]
$arguments = @($testName, '--exact','--ignored','--nocapture','--test-threads=1')
$record = @{mode=$Mode;repetitions=$Repetitions;case_ms=$CaseMs;call_ms=$CallMs;cleanup_ms=$CleanupMs;
    outer_ms=$OuterMs;binary=$Binary;binary_sha256=$BinarySha256;argv=$arguments;build_receipt=$BuildReceipt;
    source_revision=$build.source_revision;source_context=$build.source_context;
    handle_stability='unverified; raw point counts only';process='not started'}
$utf8 = New-Object Text.UTF8Encoding($false)
function Save-Receipt {
    [IO.File]::WriteAllText((Join-Path $Output 'receipt.json'), ($record | ConvertTo-Json -Depth 12), $utf8)
}
Save-Receipt
$env:SHARDS_BORROWED_HANDLES_REPETITIONS = [string]$Repetitions
$env:SHARDS_BORROWED_HANDLES_CASE_MS = [string]$CaseMs
$env:SHARDS_BORROWED_HANDLES_CALL_MS = [string]$CallMs
$env:SHARDS_BORROWED_HANDLES_CLEANUP_MS = [string]$CleanupMs
$env:SHARDS_BORROWED_HANDLES_SOCKET = $socket
$watch = [Diagnostics.Stopwatch]::StartNew()
$process = $null
try {
    $process = Start-Process -FilePath $Binary -ArgumentList $arguments -PassThru -NoNewWindow `
        -RedirectStandardOutput (Join-Path $Output 'stdout.log') -RedirectStandardError (Join-Path $Output 'stderr.log')
    # Cache the owned process handle before a fast exit. The first actual caller
    # run observed HasExited with a null ExitCode when this handle was uncached.
    $ownedProcessHandle = $process.Handle
    $record.process_id = $process.Id
    # Reserve the supplied cleanup slice inside the original outer deadline.
    $wait = [Math]::Max(0, ($OuterMs-$CleanupMs)-[int]$watch.ElapsedMilliseconds)
    if (-not $process.WaitForExit($wait)) {
        $record.process = 'deadline expired; owned process termination requested'
        try { $process.Kill() } catch { $record.kill_error = $_.ToString() }
        $left = [Math]::Max(0, $OuterMs-[int]$watch.ElapsedMilliseconds)
        if ($process.WaitForExit($left)) { $record.process = 'killed; actual exit observed' }
        else { $record.process = 'cleanup pending/unknown at original outer deadline' }
        $record.timed_out = $true
    } else {
        $record.process = 'actual owned process exit observed'
        $record.timed_out = $false
    }
    if ($process.HasExited) {
        $process.Refresh()
        $record.exit_code = $process.ExitCode
        if ($null -eq $record.exit_code) { $record.exit_observation = 'unknown; no numeric exit code returned' }
    }
    $record.binary_sha256_after = (Get-FileHash -Algorithm SHA256 -LiteralPath $Binary).Hash.ToLowerInvariant()
    $record.byte_identity_unchanged = $record.binary_sha256_after -ceq $BinarySha256
} catch {
    $record.launch_or_observation_error = $_.ToString()
    $record.process = 'native execution or cleanup unverified; retained exact available logs'
} finally {
    if ($null -ne $process) { $process.Dispose() }
    $record.native_elapsed_ms = $watch.ElapsedMilliseconds
    Save-Receipt
}
# Parsing is deliberately separate, with caller.py on a coordinator that has
# Python. PowerShell does not convert process exit0 into a handle-stability pass.
Write-Output ($record | ConvertTo-Json -Depth 12 -Compress)
if ($record.timed_out -or $record.exit_code -ne 0 -or -not $record.byte_identity_unchanged) { exit 1 }
exit 0
