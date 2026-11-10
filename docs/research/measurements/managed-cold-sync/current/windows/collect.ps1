param(
    [Parameter(Mandatory=$true)][string]$Scope,
    [Parameter(Mandatory=$true)][string]$Out
)
$ErrorActionPreference = 'Stop'
$encoding = New-Object Text.UTF8Encoding($false)
$payloads = @()
function Add-Original([IO.FileInfo]$File, [string]$Name) {
    $body = [IO.File]::ReadAllBytes($File.FullName)
    $hash = [Security.Cryptography.SHA256]::Create()
    try { $sha = ([BitConverter]::ToString($hash.ComputeHash($body))).Replace('-','').ToLowerInvariant() }
    finally { $hash.Dispose() }
    return @{path=$Name.Replace('\','/');original=$File.FullName;bytes=$body.Length;sha256=$sha;
             data_base64=[Convert]::ToBase64String($body)}
}
$Scope = [IO.Path]::GetFullPath($Scope)
foreach ($file in Get-ChildItem -LiteralPath $Scope -File) {
    if ($file.Name -match '\.(json|ps1|stdout|stderr|exit)$') {
        $payloads += Add-Original $file $file.Name
    }
}
foreach ($name in @('execution','execution-v2')) {
    $directory = Join-Path $Scope $name
    foreach ($file in Get-ChildItem -LiteralPath $directory -File) {
        if ($file.Extension -eq '.json') { $payloads += Add-Original $file ($name+'/'+$file.Name) }
    }
    foreach ($file in Get-ChildItem -LiteralPath (Join-Path $directory 'raw') -File) {
        $payloads += Add-Original $file ($name+'/raw/'+$file.Name)
    }
}
$record = @{scope=$Scope;format='original bytes in checked base64 payloads';payloads=$payloads;
    excluded='source bytes in original input.json; source, targets, cargo homes, temporary paths and frozen executables retained on runner'}
$body = $encoding.GetBytes(($record | ConvertTo-Json -Depth 12) + "`n")
$stream = [IO.File]::Open($Out,[IO.FileMode]::CreateNew)
$gzip = New-Object IO.Compression.GZipStream($stream,[IO.Compression.CompressionMode]::Compress)
try { $gzip.Write($body,0,$body.Length) } finally { $gzip.Dispose();$stream.Dispose() }
$hash = [Security.Cryptography.SHA256]::Create()
try { $sha=([BitConverter]::ToString($hash.ComputeHash([IO.File]::ReadAllBytes($Out)))).Replace('-','').ToLowerInvariant() }
finally { $hash.Dispose() }
[pscustomobject]@{archive=$Out;bytes=([IO.FileInfo]$Out).Length;sha256=$sha;payloads=$payloads.Count} | ConvertTo-Json -Compress
