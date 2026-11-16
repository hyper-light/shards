# win-p0 (platform-03, memory-01): focused unit tests on the Windows 11 guest, x86_64-pc-windows-msvc.
param([string]$sha, [string]$what = 'test')
$ErrorActionPreference = 'Continue'
$base = 'C:\g121\win-p0'
New-Item -ItemType Directory -Force $base | Out-Null
Set-Location $base
$env:PATH = "C:\g121\nasm-3.02;C:\g121\zig-x86_64-windows-0.16.0;C:\g121\git\cmd;$env:PATH"
for ($i = 0; $i -lt 6 -and -not (Test-Path "$base\src\.git"); $i++) {
  git -c core.autocrlf=false -c core.longpaths=true clone -q --filter=blob:none https://github.com/hyper-light/shards.git src 2>&1 | Out-String
  if (-not (Test-Path "$base\src\.git")) { Start-Sleep 30 }
}
Set-Location "$base\src"
git config core.autocrlf false; git config core.longpaths true
for ($i = 0; $i -lt 6; $i++) { git fetch -q origin p/win-p0-v1 2>&1 | Out-String; if ($LASTEXITCODE -eq 0) { break }; Start-Sleep 30 }
git checkout -q -f --detach $sha 2>&1 | Out-String
"== " + (git log --oneline -1) + " " + (Get-Date).ToUniversalTime().ToString('o') + " " + [System.Environment]::OSVersion.Version.ToString()
$env:CARGO_TARGET_DIR = "$base\target"
$env:CARGO_INCREMENTAL = '0'
$env:SHARDS_HELPERS = 'skip'
New-Item -ItemType File -Force "$base\fake-init" | Out-Null
New-Item -ItemType File -Force "$base\fake-server" | Out-Null
$env:SHARDS_INIT_BINARY = "$base\fake-init"
$env:SHARDS_SERVER_BINARY = "$base\fake-server"
$status = 0
if ($what -eq 'test' -or $what -eq 'all' -or $what -eq 'quick') {
  $runs = @(
    @('-p','shards-vmm','--lib','--','devices::virtio::fs::window::tests'),
    @('-p','shards','--bin','shards','--','grant_answer::windows_tests')
  )
  if ($what -eq 'quick') { $runs = @(,@('-p','shards-vmm','--lib','--','devices::virtio::fs::window::tests')) }
  foreach ($r in $runs) {
    $log = "$base\test-" + $r[1] + ".log"
    & cargo test -j 6 @r *> $log
    $code = $LASTEXITCODE; if ($code -ne 0) { $status = 1 }
    "== cargo test " + ($r -join ' ') + " exit $code"
    Select-String -Path $log -Pattern '^test |^test result|panicked|^error|-->|assertion|left:|right:' | ForEach-Object { $_.Line } | Select-Object -First 60
  }
}
if ($what -eq 'mutate' -or $what -eq 'all') {
  # Each guard removed (C:\g121\win-p0-patches\*.patch): its test must fail.
  $tests = @{
    'm1-no-seal' = @('-p','shards','--bin','shards','--','grant_answer::windows_tests::a_vm_that_stages');
    'm4-broker-no-refusal' = @('-p','shards','--bin','shards','--','grant_answer::windows_tests::a_vm_that_stages');
    'm2-no-upgrade' = @('-p','shards-vmm','--lib','--','devices::virtio::fs::window::tests::a_range_');
    'm3-section-falls-through' = @('-p','shards-vmm','--lib','--','devices::virtio::fs::window::tests::a_range_')
  }
  foreach ($m in $tests.Keys | Sort-Object) {
    git apply "C:\g121\win-p0-patches\$m.patch" 2>&1 | Out-String
    $r = $tests[$m]
    $log = "$base\mutant-$m.log"
    & cargo test -j 6 @r *> $log
    $code = $LASTEXITCODE
    "== mutant ${m}: cargo test " + ($r -join ' ') + " exit $code (must fail)"
    if ($code -eq 0) { $status = 1 }
    Select-String -Path $log -Pattern '^test |^test result|panicked|assertion|left:|right:|^error' | ForEach-Object { $_.Line } | Select-Object -First 30
    git checkout -q -f -- crates 2>&1 | Out-String
  }
}
if ($what -eq 'clippy' -or $what -eq 'all' -or $what -eq 'quick') {
  foreach ($p in @('shards-vmm','shards-vm-process','shards')) {
    $log = "$base\clippy-$p.log"
    & cargo clippy -j 6 -p $p --all-targets -- -D warnings *> $log
    $code = $LASTEXITCODE; if ($code -ne 0) { $status = 1 }
    "== cargo clippy -p $p --all-targets exit $code"
    Select-String -Path $log -Pattern '^(warning|error)|-->|Finished' | ForEach-Object { $_.Line } | Select-Object -First 30
  }
}
"== done " + (Get-Date).ToUniversalTime().ToString('o') + " status $status"
