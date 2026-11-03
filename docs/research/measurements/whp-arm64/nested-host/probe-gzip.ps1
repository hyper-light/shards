# Verify the external fixture encoder before the complete workspace suite.
param([string]$Kit = 'C:\shards-step10', [string]$Runner = '')
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$env:PATH = (Join-Path $Kit 'git\cmd') + ';' + $env:PATH + ';' + (Join-Path $Kit 'git\usr\bin')
if ($Runner) {
    $tokens = $null; $errors = $null
    [void][System.Management.Automation.Language.Parser]::ParseFile($Runner, [ref]$tokens, [ref]$errors)
    if ($errors.Count) { throw ($errors | Out-String) }
    Write-Output "Runner parsed; SHA256=$((Get-FileHash $Runner -Algorithm SHA256).Hash)"
}
$gzip = (Get-Command gzip.exe -CommandType Application).Source
Write-Output "gzip=$gzip SHA256=$((Get-FileHash $gzip -Algorithm SHA256).Hash)"
$info = New-Object System.Diagnostics.ProcessStartInfo
$info.FileName = $gzip
$info.Arguments = '-c'
$info.UseShellExecute = $false
$info.RedirectStandardInput = $true
$info.RedirectStandardOutput = $true
$info.RedirectStandardError = $true
$p = New-Object System.Diagnostics.Process
$p.StartInfo = $info
$compressed = New-Object System.IO.MemoryStream
try {
    if (-not $p.Start()) { throw 'gzip did not start' }
    $plain = 'shards-gzip-fixture'
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($plain)
    $p.StandardInput.BaseStream.Write($bytes, 0, $bytes.Length)
    $p.StandardInput.Close()
    $p.StandardOutput.BaseStream.CopyTo($compressed)
    $stderr = $p.StandardError.ReadToEnd()
    $p.WaitForExit()
    if ($p.ExitCode -ne 0) { throw "gzip exit $($p.ExitCode): $stderr" }
    $encoded = $compressed.ToArray()
    if ($encoded.Length -lt 2 -or $encoded[0] -ne 31 -or $encoded[1] -ne 139) { throw 'No gzip header' }
    $compressed.Position = 0
    $inflater = [System.IO.Compression.GZipStream]::new($compressed, [System.IO.Compression.CompressionMode]::Decompress, $true)
    $reader = [System.IO.StreamReader]::new($inflater, [System.Text.Encoding]::UTF8)
    if ($reader.ReadToEnd() -ne $plain) { throw 'GNU gzip/.NET round trip differed' }
    Write-Output "Native gzip exit=0; compressed bytes=$($encoded.Length); independent .NET round trip passed"
} finally {
    if ($reader) { $reader.Dispose() }
    if ($inflater) { $inflater.Dispose() }
    $compressed.Dispose()
    $p.Dispose()
}
