# Step 9 (p/s9-grants-v1) on the Windows 11 guest: the light unit tests of the crates the
# change touches, x86_64-pc-windows-msvc. Empty placeholders stand for the guest binaries
# build.rs embeds, which these tests never read; no VM process is built.
$ErrorActionPreference = 'Continue'
$base = 'C:\g121\s9'
New-Item -ItemType Directory -Force $base | Out-Null
Set-Location $base
if (-not (Test-Path src)) { git -c core.autocrlf=false -c core.longpaths=true clone -q https://github.com/hyper-light/shards.git src 2>&1 | Out-String }
Set-Location "$base\src"
git config core.autocrlf false; git config core.longpaths true
git fetch -q origin p/s9-grants-v1 2>&1 | Out-String
git checkout -q -B p/s9-grants-v1 FETCH_HEAD 2>&1 | Out-String
"== " + (git log --oneline -1) + " " + (Get-Date).ToUniversalTime().ToString('o')
$env:CARGO_TARGET_DIR = "$base\target"
$env:SHARDS_HELPERS = 'skip'
New-Item -ItemType File -Force "$base\fake-init" | Out-Null
New-Item -ItemType File -Force "$base\fake-server" | Out-Null
$env:SHARDS_INIT_BINARY = "$base\fake-init"
$env:SHARDS_SERVER_BINARY = "$base\fake-server"
$env:PATH = "C:\g121\nasm-3.02;C:\g121\zig-x86_64-windows-0.16.0;C:\g121\git\cmd;$env:PATH"
$status = 0
$runs = @(
  @('-p','shards-dns'),
  @('-p','shards-agentfile'),
  @('-p','shards-net'),
  @('-p','shards-api'),
  @('-p','shards','--bin','shards','--','api_run::','agentfile::','daemon::commands','networks::')
)
foreach ($r in $runs) {
  $log = "$base\test-" + $r[1] + ".log"
  & cargo test @r *> $log
  $code = $LASTEXITCODE
  if ($code -ne 0) { $status = 1 }
  "== cargo test " + ($r -join ' ') + " exit $code"
  Select-String -Path $log -Pattern '^test result|FAILED|panicked|^error' | ForEach-Object { $_.Line } | Select-Object -First 30
}
"== done " + (Get-Date).ToUniversalTime().ToString('o') + " status $status"
