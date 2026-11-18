# The WHP halt-clock probe (README.md beside this script): shards-vmm's ignored test
# hv::whp::tests::halt_clock_probe, run once on this Windows x86_64 host, its CSV written to
# halt-clock.csv and this host's identity to host.txt, both beside this script. Run from the
# repository's root, nothing else loading the host:
#   powershell -ExecutionPolicy Bypass -File docs\research\measurements\whp-halt-clock-v1\run.ps1
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
$os = Get-CimInstance Win32_OperatingSystem
@(
  "revision $(git rev-parse HEAD)"
  "host $env:COMPUTERNAME"
  "cpu $($cpu.Name), $($cpu.NumberOfLogicalProcessors) logical processors"
  "os $($os.Caption) $($os.Version) build $($os.BuildNumber)"
  "rustc $(rustc --version)"
) | Set-Content -Path (Join-Path $here 'host.txt')
$env:SHARDS_HALT_PROBE_CSV = Join-Path $here 'halt-clock.csv'
cargo test -p shards-vmm --profile test-release --lib -- hv::whp::tests::halt_clock_probe --exact --ignored --nocapture
exit $LASTEXITCODE
