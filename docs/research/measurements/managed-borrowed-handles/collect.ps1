param([Parameter(Mandatory=$true)][string]$Base)
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
$names = @('build-receipt.json','native-summary.json','native-v2-summary.json','BUNDLE-MANIFEST.json',
    'compile-light-v1.ps1','compile-light.ps1','finish-build.ps1','caller-v1.ps1','caller.ps1','native-driver.ps1',
    'focused-v1.rs','fixture/Cargo.toml','fixture/Cargo.lock',
    'source/docs/research/measurements/managed-borrowed-handles/focused.rs')
foreach ($folder in @('raw','raw-v2')) {
    Get-ChildItem -LiteralPath (Join-Path $Base $folder) -Recurse -File | ForEach-Object {
        $names += $_.FullName.Substring($Base.Length+1).Replace('\','/')
    }
}
foreach ($group in @('native','native-v2')) {
    foreach ($mode in @('success','wrong-start','withheld')) {
        foreach ($file in @('receipt.json','stdout.log','stderr.log')) { $names += "$group/$mode/$file" }
    }
}
$manifest = @{}
foreach ($name in $names) {
    $path = Join-Path $Base $name
    $manifest[$name] = @{bytes=(Get-Item -LiteralPath $path).Length;
        sha256=(Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant()}
}
$utf8 = New-Object Text.UTF8Encoding($false)
[IO.File]::WriteAllText((Join-Path $Base 'ARTIFACT-MANIFEST.json'), ($manifest | ConvertTo-Json -Depth 8), $utf8)
$names += 'ARTIFACT-MANIFEST.json'
[IO.File]::WriteAllText((Join-Path $Base 'members.txt'), ($names -join "`n")+"`n", $utf8)
Push-Location $Base
try { & tar.exe -czf 'records.tar.gz' -T 'members.txt' }
finally { Pop-Location }
if ($LASTEXITCODE -ne 0) { throw 'artifact archive creation failed' }
$archive = Join-Path $Base 'records.tar.gz'
$storage = (Get-ChildItem -LiteralPath $Base -Recurse -File | Measure-Object Length -Sum).Sum
@{archive=$archive;bytes=(Get-Item $archive).Length;sha256=(Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant();
    members=$names.Count;private_directory_logical_file_bytes=$storage;scope='storage bookkeeping, not a peak/resource-return measurement'} |
    ConvertTo-Json -Compress | Write-Output
