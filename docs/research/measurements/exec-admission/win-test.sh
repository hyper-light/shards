#!/bin/bash
W="ssh -o BatchMode=yes -o ConnectTimeout=8 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ServerAliveInterval=30 -p 2222 shards@127.0.0.1"
log=~/g121-tmp/xa-exec/logs/windows-tests.log
echo "# Windows 11 guest on adacachy (x86_64-pc-windows-msvc); $(date -u +%FT%TZ)" > $log
$W '$env:CARGO_TARGET_DIR="C:\xa-exec\target"; Set-Location C:\xa-exec\src; [System.Environment]::OSVersion.VersionString; rustc --version; cargo test -p shards-vm-process --bin shards-vm --profile test-release -j 6 -- workload:: 2>&1 | Select-String -Pattern "^test |test result|panicked|error|warning: unused" | ForEach-Object { $_.Line }; "## vm-process exit $LASTEXITCODE"; cargo test -p shards-vmm --lib --profile test-release -j 6 -- vsock 2>&1 | Select-String -Pattern "test result|panicked|FAILED|error\[" | ForEach-Object { $_.Line }; "## vmm vsock exit $LASTEXITCODE"' < /dev/null >> $log 2>&1
echo "# done $(date -u +%FT%TZ)" >> $log
