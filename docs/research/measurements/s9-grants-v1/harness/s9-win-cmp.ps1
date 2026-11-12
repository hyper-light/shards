$base = 'C:\g121\s9'
Set-Location "$base\src"
$env:CARGO_TARGET_DIR = "$base\target"; $env:SHARDS_HELPERS = 'skip'
$env:PATH = "C:\g121\nasm-3.02;C:\g121\zig-x86_64-windows-0.16.0;C:\g121\git\cmd;$env:PATH"
foreach ($rev in 'a7dbf1446', 'p/s9-grants-v1') {
  git -c advice.detachedHead=false checkout -q $rev 2>&1 | Out-String
  "== at " + (git log --oneline -1)
  foreach ($i in 1..3) {
    & cargo test -p shards-api --lib *> "$base\cmp.log"
    "full lib run $i exit $LASTEXITCODE"
    Select-String -Path "$base\cmp.log" -Pattern 'FAILED|^test result' | ForEach-Object { $_.Line }
  }
  & cargo test -p shards-api --lib -- physical::report::tests::private_report_refuses_missing_torn_and_over_bound_bytes *> "$base\cmp1.log"
  "alone exit $LASTEXITCODE"
}
git checkout -q p/s9-grants-v1 2>&1 | Out-String
