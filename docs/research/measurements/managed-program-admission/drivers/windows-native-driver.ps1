$ErrorActionPreference = 'Stop'
$repo = Join-Path $HOME 'managed-capture-windows-check'
$record = Join-Path $HOME 'root-managed-borrowed-3b196-native-v2'
if (Test-Path $record) { throw 'new record directory required' }
New-Item -ItemType Directory -Path $record | Out-Null
Set-Location $repo
$dirty = & git diff --name-only
if ($LASTEXITCODE -ne 0 -or $dirty) { throw 'tracked source dirty' }
$staged = & git diff --cached --name-only
if ($LASTEXITCODE -ne 0 -or $staged) { throw 'index dirty' }
$log = Join-Path $record 'fetch.log'
& cmd.exe /d /c "git fetch origin p/managed-layer-production-v1 > $log 2>&1"
if ($LASTEXITCODE -ne 0) { throw 'fetch failed' }
$log = Join-Path $record 'checkout.log'
& cmd.exe /d /c "git checkout --detach 3b1967838b140466ff3367637541492a77be0459 > $log 2>&1"
if ($LASTEXITCODE -ne 0) { throw 'checkout failed' }
& git rev-parse HEAD | Set-Content (Join-Path $record 'source.txt')
& rustc --version --verbose *> (Join-Path $record 'rustc.log')
$env:CARGO_TARGET_DIR = Join-Path $HOME 'managed-capture-windows-target'
$env:CARGO_BUILD_JOBS = '2'
$docker = Join-Path $record 'empty-docker'
New-Item -ItemType Directory -Path $docker | Out-Null
$env:DOCKER_CONFIG = $docker
@('cargo','test','--locked','-j','2','-p','shards-api','--lib','--','--test-threads=1','--nocapture') | ConvertTo-Json | Set-Content (Join-Path $record 'command.json')
$log = Join-Path $record 'native.log'
& cmd.exe /d /c "cargo test --locked -j 2 -p shards-api --lib -- --test-threads=1 --nocapture > $log 2>&1"
$rc = $LASTEXITCODE
$rc | Set-Content (Join-Path $record 'native.exit')
Write-Output "native-api-exit $rc"
Get-Content (Join-Path $record 'native.log') -Tail 90
exit $rc
