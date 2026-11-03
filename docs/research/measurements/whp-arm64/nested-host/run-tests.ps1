# Run the cross-built workspace's tests as a standard Windows user.
param(
    [Parameter(Mandatory=$true)][string]$Kernel,
    [string]$Kit = 'C:\shards-step10',
    [string]$Tag = 'first',
    [ValidateRange(0,2147483647)][int]$StartAt = 0,
    [string]$Binary = '',
    [ValidatePattern('^[A-Za-z0-9_:]*$')][string]$Test = ''
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$env:SHARDS_REQUIRE_VMS = '1'
$env:SHARDS_TEST_GUESTS = Join-Path $Kit 'test-guests'
$env:SHARDS_TEST_KERNEL = $Kernel
$env:PATH = (Join-Path $Kit 'git\cmd') + ';' + $env:PATH
Remove-Item Env:SHARDS_STORM_CPUS -ErrorAction SilentlyContinue
$tests = Get-Content (Join-Path $Kit 'test-manifest.json') -Raw | ConvertFrom-Json
if ($tests.Count -eq 0 -or $tests[0].exe -isnot [string]) { throw 'Malformed test manifest' }
Write-Output "MANIFEST binaries=$($tests.Count)"
$logs = Join-Path $Kit "full-$Tag"
New-Item -ItemType Directory -Force -Path $logs | Out-Null
$failed = 0
$ran = 0
for ($i = $StartAt; $i -lt $tests.Count; $i++) {
    $t = $tests[$i]
    if ($Binary -and $t.name -ne $Binary) { continue }
    $ran++
    $label = ('{0:D3}-{1}' -f $i, $t.name)
    $start = [DateTime]::UtcNow
    Write-Output "START $i/$($tests.Count) $($t.package) $($t.name) $($start.ToString('o'))"
    $stdout = Join-Path $logs "$label.stdout.txt"
    $stderr = Join-Path $logs "$label.stderr.txt"
    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = $t.exe
    $info.WorkingDirectory = $t.cwd
    $info.Arguments = '--nocapture --test-threads=1'
    if ($Test) { $info.Arguments = "$Test --exact --nocapture --test-threads=1" }
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $p = New-Object System.Diagnostics.Process
    $p.StartInfo = $info
    if (-not $p.Start()) { throw "Starting $label failed" }
    $outFile = [System.IO.File]::Open($stdout, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write, [System.IO.FileShare]::ReadWrite)
    $errFile = [System.IO.File]::Open($stderr, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write, [System.IO.FileShare]::ReadWrite)
    try {
        $outCopy = $p.StandardOutput.BaseStream.CopyToAsync($outFile)
        $errCopy = $p.StandardError.BaseStream.CopyToAsync($errFile)
        $p.WaitForExit()
        $outCopy.Wait()
        $errCopy.Wait()
    } finally {
        $outFile.Dispose()
        $errFile.Dispose()
    }
    $code = $p.ExitCode
    if ($null -eq $code) { throw "No exit status for $label" }
    $record = [ordered]@{index=$i; package=$t.package; name=$t.name; exe=$t.exe; start=$start.ToString('o'); elapsed=([DateTime]::UtcNow-$start).TotalSeconds; exit=$code}
    $record | ConvertTo-Json -Compress | Add-Content (Join-Path $logs 'results.jsonl')
    Get-Content $stdout | Select-Object -Last 5 | Write-Output
    if ($code -ne 0) {
        $failed++
        Write-Output "FAILED $label exit=$code"
        Get-Content $stderr | Select-Object -Last 30 | Write-Output
    }
    Write-Output "END $label exit=$code elapsed=$($record.elapsed)"
}
Write-Output "COMPLETE binaries=$ran manifest=$($tests.Count) failed=$failed filter=$Binary/$Test start=$StartAt"
if (-not $ran) { throw 'No test executables selected' }
if ($failed) { exit 1 }
exit 0
