$ProgressPreference='SilentlyContinue'
$ErrorActionPreference='Stop'
$b='C:\Users\corph\native-borrowed-handles-3b-light-v1'
$r=Get-Content -Raw -LiteralPath (Join-Path $b 'build-receipt.json') | ConvertFrom-Json
if($r.binary_sha256 -notmatch '^[0-9a-f]{64}$'){throw 'actual binary receipt absent'}
if((Get-FileHash -Algorithm SHA256 -LiteralPath $r.binary).Hash.ToLowerInvariant() -cne $r.binary_sha256){throw 'actual frozen executable mismatch'}
$metadata=@{binary=$r.binary;binary_sha256=$r.binary_sha256;binary_bytes=$r.binary_bytes;source_revision=$r.source_revision;policy_revision=$r.policy_revision;source_context=$r.source_context}
$metadata | ConvertTo-Json -Compress | Write-Output
foreach($mode in @('success','wrong-start','withheld')){
 $out=Join-Path $b ('native\'+$mode)
 & powershell.exe -NoProfile -NonInteractive -File (Join-Path $b 'caller.ps1') -Binary $r.binary -BinarySha256 $r.binary_sha256 -BuildReceipt (Join-Path $b 'build-receipt.json') -Mode $mode -Repetitions 16 -CaseMs 90000 -CallMs 1000 -CleanupMs 5000 -OuterMs 120000 -Output $out
 Write-Output ('CASE_DRIVER mode='+$mode+' exit='+$LASTEXITCODE)
}
$changed=@()
foreach($entry in $r.source_sha256.PSObject.Properties){
 $p=Join-Path $b $entry.Name
 if((Get-FileHash -Algorithm SHA256 -LiteralPath $p).Hash.ToLowerInvariant() -cne $entry.Value){$changed+=$entry.Name}
}
@{source_changed_after_native=$changed;binary_sha256_after=(Get-FileHash -Algorithm SHA256 -LiteralPath $r.binary).Hash.ToLowerInvariant();native_modes_attempted=3;repetitions_per_mode=16;case_ms=90000;call_ms=1000;cleanup_ms=5000;outer_ms=120000;handle_stability='raw unadjusted point counts only'} | ConvertTo-Json -Depth 8 | Set-Content -Encoding UTF8 (Join-Path $b 'native-summary.json')
if($changed.Count -ne 0){throw 'source identity changed during native cases'}
