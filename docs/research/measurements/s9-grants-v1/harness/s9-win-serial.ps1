$base = 'C:\g121\s9'
Set-Location "$base\src"
$env:CARGO_TARGET_DIR = "$base\target"; $env:SHARDS_HELPERS = 'skip'
$env:PATH = "C:\g121\nasm-3.02;C:\g121\zig-x86_64-windows-0.16.0;C:\g121\git\cmd;$env:PATH"
"== at " + (git log --oneline -1)
& cargo test -p shards-api -- --test-threads=1 *> "$base\serial.log"
"serial exit $LASTEXITCODE"
Select-String -Path "$base\serial.log" -Pattern 'FAILED|^test result|image::' | ForEach-Object { $_.Line } | Select-Object -First 40
