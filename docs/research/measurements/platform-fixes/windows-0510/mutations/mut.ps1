# platform-05..10 takeover: each guard's mutation applied alone on the fixed tree, its test run.
param([string]$sha)
$ErrorActionPreference = 'Continue'
$base = 'C:\tkplatwin'
Set-Location "$base\src"
$env:PATH = "C:\g121\nasm-3.02;C:\g121\zig-x86_64-windows-0.16.0;C:\g121\git\cmd;$env:PATH"
$env:CARGO_TARGET_DIR = "$base\target"
$env:CARGO_INCREMENTAL = '0'
$env:SHARDS_HELPERS = 'skip'
git fetch -q origin p/tk-platwin-v1 2>&1 | Out-String
foreach ($line in Get-Content "$base\mut\list.txt") {
  if ($line -eq '') { continue }
  $name, $run = $line.Split('#')
  $crate, $filter = $run.Split('|')
  git checkout -q -f --detach $sha 2>&1 | Out-String
  git apply "$base\mut\$name.patch" 2>&1 | Out-String
  $applied = $LASTEXITCODE
  $log = "$base\mut-$name.log"
  & cargo test -p $crate --lib -- $filter *> $log
  $code = $LASTEXITCODE
  "== $name (applied $applied): cargo test -p $crate $filter exit $code"
  Select-String -Path $log -Pattern '^test |^test result|panicked|^error\[|^error:' | ForEach-Object { $_.Line } | Select-Object -First 8
}
git checkout -q -f --detach $sha 2>&1 | Out-String
"== done " + (Get-Date).ToUniversalTime().ToString('o')
