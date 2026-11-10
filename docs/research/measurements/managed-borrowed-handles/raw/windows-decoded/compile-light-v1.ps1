param([Parameter(Mandatory=$true)][string]$Base,
      [Parameter(Mandatory=$true)][string]$ArchiveSha256)
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Continue'
$PSDefaultParameterValues['Out-File:Encoding'] = 'utf8'
$archive = Join-Path $Base 'compile-source.tar.gz'
$encoded = [IO.File]::ReadAllText((Join-Path $Base 'source.b64'))
[IO.File]::WriteAllBytes($archive, [Convert]::FromBase64String($encoded))
if ((Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant() -ne $ArchiveSha256) {
    throw 'archive identity mismatch; no extraction/compile'
}
& tar.exe -xzf $archive -C $Base
if ($LASTEXITCODE -ne 0) { throw 'trusted source archive extraction failed' }
$raw = Join-Path $Base 'raw'
[IO.Directory]::CreateDirectory($raw) | Out-Null
$env:CARGO_HOME = Join-Path $Base 'cargo-home'
$env:CARGO_TARGET_DIR = Join-Path $Base 'target'
$env:CARGO_BUILD_JOBS = '1'
$env:CARGO_INCREMENTAL = '0'
$env:CARGO_PROFILE_DEV_DEBUG = '0'
$env:CARGO_PROFILE_TEST_DEBUG = '0'
& rustc.exe -Vv *> (Join-Path $raw 'rustc.log')
& cargo.exe -Vv *> (Join-Path $raw 'cargo.log')
$manifest = Join-Path $Base 'fixture\Cargo.toml'
$check = @('check','--manifest-path',$manifest,'--tests','--target','x86_64-pc-windows-msvc','-j1')
& cargo.exe @check *> (Join-Path $raw 'check.log')
$checked = $LASTEXITCODE
@{argv=$check;exit_code=$checked;scope='compile-only; native cases executed zero'} |
    ConvertTo-Json -Depth 8 | Set-Content -Encoding UTF8 (Join-Path $raw 'check.json')
Write-Output "COMPILE_ONLY check_exit=$checked native_cases_executed=0"
if ($checked -ne 0) { exit $checked }
$clippy = @('clippy','--manifest-path',$manifest,'--tests','--no-deps','--target','x86_64-pc-windows-msvc','-j1','--','-D','warnings')
& cargo.exe @clippy *> (Join-Path $raw 'clippy.log')
$linted = $LASTEXITCODE
@{argv=$clippy;exit_code=$linted;scope='compile-only; native cases executed zero'} |
    ConvertTo-Json -Depth 8 | Set-Content -Encoding UTF8 (Join-Path $raw 'clippy.json')
Write-Output "COMPILE_ONLY clippy_exit=$linted native_cases_executed=0"
exit $linted
