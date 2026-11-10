param([Parameter(Mandatory=$true)][string]$Base)
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Continue'
$PSDefaultParameterValues['Out-File:Encoding'] = 'utf8'
$raw = Join-Path $Base 'raw-v2'
[IO.Directory]::CreateDirectory($raw) | Out-Null
$env:CARGO_HOME = Join-Path $Base 'cargo-home'
$env:CARGO_TARGET_DIR = Join-Path $Base 'target'
$env:CARGO_BUILD_JOBS = '1'
$env:CARGO_INCREMENTAL = '0'
$env:CARGO_PROFILE_DEV_DEBUG = '0'
$env:CARGO_PROFILE_TEST_DEBUG = '0'
$manifest = Join-Path $Base 'fixture\Cargo.toml'
function Sources {
    $values = @{}
    Get-ChildItem -LiteralPath (Join-Path $Base 'source') -Recurse -File | ForEach-Object {
        $name = $_.FullName.Substring($Base.Length+1)
        $values[$name] = (Get-FileHash -Algorithm SHA256 -LiteralPath $_.FullName).Hash.ToLowerInvariant()
    }
    return $values
}
$before = Sources
$before | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 (Join-Path $raw 'source-before.json')
$check = @('check','--manifest-path',$manifest,'--tests','--target','x86_64-pc-windows-msvc','-j1')
& cargo.exe @check *> (Join-Path $raw 'check.log')
$rc = $LASTEXITCODE
@{argv=$check;exit_code=$rc} | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 (Join-Path $raw 'check.json')
Write-Output "CHECK=$rc"
if ($rc -ne 0) { exit $rc }
$clippy = @('clippy','--manifest-path',$manifest,'--tests','--no-deps','--target','x86_64-pc-windows-msvc','-j1','--','-D','warnings')
& cargo.exe @clippy *> (Join-Path $raw 'clippy.log')
$rc = $LASTEXITCODE
@{argv=$clippy;exit_code=$rc} | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 (Join-Path $raw 'clippy.json')
Write-Output "CLIPPY=$rc"
if ($rc -ne 0) { exit $rc }
$build = @('test','--manifest-path',$manifest,'--lib','--no-run','--target','x86_64-pc-windows-msvc','-j1','--message-format=json')
& cargo.exe @build 1> (Join-Path $raw 'build-json.log') 2> (Join-Path $raw 'build-stderr.log')
$rc = $LASTEXITCODE
@{argv=$build;exit_code=$rc} | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 (Join-Path $raw 'build.json')
Write-Output "BUILD_NO_RUN=$rc"
if ($rc -ne 0) { exit $rc }
$executables = @()
foreach ($line in (Get-Content -LiteralPath (Join-Path $raw 'build-json.log'))) {
    try {
        $event = $line | ConvertFrom-Json -ErrorAction Stop
        if ($event.reason -eq 'compiler-artifact' -and $event.profile.test -and $event.executable) {
            $executables += [string]$event.executable
        }
    } catch { }
}
if ($executables.Count -ne 1) { throw "exact one test executable required, got $($executables.Count)" }
$artifacts = Join-Path $Base 'artifacts'
[IO.Directory]::CreateDirectory($artifacts) | Out-Null
$binary = Join-Path $artifacts 'borrowed-handles.exe'
if (Test-Path -LiteralPath $binary) { throw 'existing frozen executable preserved' }
Copy-Item -LiteralPath $executables[0] -Destination $binary
$after = Sources
$changed = @($before.Keys | Where-Object { $before[$_] -cne $after[$_] })
if ($changed.Count -ne 0) { throw "source changed during build: $changed" }
$record = @{source_revision='3b3817eea489d107fb9820f0f46c4fe429709fa3';
    policy_revision='cbd39284937d024640c56db6f8d8ef2bd9b0ee24';
    policy_sha256='df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3';
    source_context='exact API/ABI/afunix/testdir sources; NEW generated lib/client module contexts only; new focused fixture v2';
    source_sha256=$after;binary=$binary;binary_bytes=(Get-Item -LiteralPath $binary).Length;
    binary_sha256=(Get-FileHash -Algorithm SHA256 -LiteralPath $binary).Hash.ToLowerInvariant();
    native_cases_executed=0;build_argv=$build}
$record | ConvertTo-Json -Depth 8 | Set-Content -Encoding UTF8 (Join-Path $Base 'build-receipt.json')
Write-Output ($record | Select-Object binary,binary_sha256,binary_bytes,source_revision,native_cases_executed | ConvertTo-Json -Compress)
