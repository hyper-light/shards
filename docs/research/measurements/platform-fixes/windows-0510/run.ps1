# platform-05..10 takeover: focused Windows unit tests, x86_64-pc-windows-msvc, NTFS.
# Usage: run.ps1 -sha <commit> -tag <name> -runs "crate|filter filter;crate|filter"
param([string]$sha, [string]$tag, [string]$runs, [string]$mode = 'test')
$ErrorActionPreference = 'Continue'
$base = 'C:\tkplatwin'
Set-Location "$base\src"
$env:PATH = "C:\g121\nasm-3.02;C:\g121\zig-x86_64-windows-0.16.0;C:\g121\git\cmd;$env:PATH"
git fetch -q origin p/tk-platwin-v1 2>&1 | Out-String
git checkout -q -f --detach $sha 2>&1 | Out-String
"== " + (git log --oneline -1) + " " + (Get-Date).ToUniversalTime().ToString('o')
$env:CARGO_TARGET_DIR = "$base\target"
$env:CARGO_INCREMENTAL = '0'
$env:SHARDS_HELPERS = 'skip'
$status = 0
foreach ($r in $runs.Split(';')) {
  $parts = $r.Split('|')
  $crate = $parts[0]
  $filters = @()
  if ($parts.Length -gt 1 -and $parts[1] -ne '') { $filters = $parts[1].Split(' ') }
  $log = "$base\$tag-$crate.log"
  if ($mode -eq 'clippy') {
    & cargo clippy -p $crate --all-targets -- -D warnings *> $log
  } else {
    & cargo test -p $crate --lib -- @filters *> $log
  }
  $code = $LASTEXITCODE
  if ($code -ne 0) { $status = 1 }
  "== cargo $mode -p $crate $($filters -join ' ') exit $code"
  Select-String -Path $log -Pattern '^test |^test result|FAILED|panicked|^error|^warning|-->|assert' | ForEach-Object { $_.Line } | Select-Object -First 60
}
"== done " + (Get-Date).ToUniversalTime().ToString('o') + " status $status"
