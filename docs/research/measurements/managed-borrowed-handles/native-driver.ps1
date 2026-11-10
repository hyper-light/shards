param([Parameter(Mandatory=$true)][string]$Base,
      [Parameter(Mandatory=$true)][string]$Group)
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
$buildPath = Join-Path $Base 'build-receipt.json'
$build = Get-Content -Raw -LiteralPath $buildPath | ConvertFrom-Json
if ($build.binary_sha256 -notmatch '^[0-9a-f]{64}$') { throw 'source-matching frozen binary missing' }
$results = @()
foreach ($mode in @('success','wrong-start','withheld')) {
    $out = Join-Path $Base ($Group+'\'+$mode)
    & powershell.exe -NoProfile -NonInteractive -File (Join-Path $Base 'caller.ps1') `
        -Binary $build.binary -BinarySha256 $build.binary_sha256 -BuildReceipt $buildPath -Mode $mode `
        -Repetitions 16 -CaseMs 90000 -CallMs 1000 -CleanupMs 5000 -OuterMs 120000 -Output $out
    $results += @{mode=$mode;driver_exit_code=$LASTEXITCODE;receipt=(Join-Path $out 'receipt.json')}
}
$changed = @()
foreach ($entry in $build.source_sha256.PSObject.Properties) {
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $Base $entry.Name)).Hash.ToLowerInvariant() -cne $entry.Value) {
        $changed += $entry.Name
    }
}
$after = (Get-FileHash -Algorithm SHA256 -LiteralPath $build.binary).Hash.ToLowerInvariant()
$summary = @{source_changed_after_native=$changed;binary_sha256_after=$after;source_revision=$build.source_revision;
    cases=$results;repetitions_per_mode=16;case_ms=90000;call_ms=1000;cleanup_ms=5000;outer_ms=120000;
    handle_stability='not established by raw point counts';process_and_kernel_resource_return='not inferred'}
$summary | ConvertTo-Json -Depth 10 | Set-Content -Encoding UTF8 (Join-Path $Base ($Group+'-summary.json'))
Write-Output ($summary | ConvertTo-Json -Depth 10 -Compress)
if ($changed.Count -ne 0 -or $after -cne $build.binary_sha256 -or @($results | Where-Object {$_.driver_exit_code -ne 0}).Count -ne 0) { exit 1 }
exit 0
