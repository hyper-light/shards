param(
    [Parameter(Mandatory=$true)][string]$Bundle,
    [Parameter(Mandatory=$true)][string]$Root,
    [Parameter(Mandatory=$true)][ValidateRange(1,870)][int]$Seconds
)
$ErrorActionPreference = 'Stop'
$clock = [Diagnostics.Stopwatch]::StartNew()
$encoding = New-Object Text.UTF8Encoding($false)
$controller = [Diagnostics.Process]::GetCurrentProcess()
try {
    $controller.Refresh()
    $available = $controller.ProcessorAffinity.ToInt64()
    $selected = [long]0
    $count = 0
    for ($i=0; $i -lt ([IntPtr]::Size*8) -and $count -lt 2; $i++) {
        $bit = [long]1 -shl $i
        if (($available -band $bit) -ne 0) { $selected = $selected -bor $bit; $count++ }
    }
    if ($count -ne 2) { throw 'two already allowed CPUs required' }
    $controller.ProcessorAffinity = [IntPtr]$selected
    $controller.Refresh()
    if ($controller.ProcessorAffinity.ToInt64() -ne $selected) { throw 'own affinity readback mismatch' }
} finally { $controller.Dispose() }

function Write-New([string]$Path, [byte[]]$Bytes) {
    [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($Path)) | Out-Null
    $stream = [IO.File]::Open($Path, [IO.FileMode]::CreateNew)
    try { $stream.Write($Bytes, 0, $Bytes.Length) } finally { $stream.Dispose() }
}
function Save-Json([string]$Path, $Value) {
    Write-New $Path $encoding.GetBytes(($Value | ConvertTo-Json -Depth 40) + "`n")
}
function Hash-Bytes([byte[]]$Bytes) {
    $hash = [Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($hash.ComputeHash($Bytes))).Replace('-','').ToLowerInvariant() }
    finally { $hash.Dispose() }
}
function Run-Command([string]$Name, [string[]]$Arguments) {
    $remaining = [Math]::Floor(($Seconds - $clock.Elapsed.TotalSeconds) * 1000)
    if ($remaining -le 0) { return @{stage=$Name;unrun='original deadline exhausted'} }
    $outPath = Join-Path $raw ($Name + '.stdout')
    $errPath = Join-Path $raw ($Name + '.stderr')
    $out = [IO.File]::Open($outPath,[IO.FileMode]::CreateNew)
    $err = [IO.File]::Open($errPath,[IO.FileMode]::CreateNew)
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = $cargo
    # All argv below are fixed flags without spaces; paths use native properties.
    $info.Arguments = [string]::Join(' ', $Arguments)
    $info.WorkingDirectory = $source
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $process = New-Object Diagnostics.Process
    $process.StartInfo = $info
    $row = [ordered]@{stage=$Name;file=$cargo;argv=$Arguments;cwd=$source;timed_out=$false;
                     original_driver_seconds=$Seconds;cleanup=@()}
    try {
        if (!$process.Start()) { throw 'native process did not start' }
        $held = $process.Handle
        $row.pid = $process.Id
        $outTask = $process.StandardOutput.BaseStream.CopyToAsync($out)
        $errTask = $process.StandardError.BaseStream.CopyToAsync($err)
        if (!$process.WaitForExit([int]$remaining)) {
            $row.timed_out = $true
            # Only the native process whose handle this Process still holds.
            # No job modification, taskkill/tree scan or unrelated PID signalling.
            $process.Kill()
            $row.cleanup += 'owned command killed; descendant cleanup unknown'
            if (!$process.WaitForExit(10000)) { throw 'owned command reap unverified' }
        }
        $row.exit = $process.ExitCode
        $captureLeft = [Math]::Max(0,[Math]::Min(10000,($Seconds-$clock.Elapsed.TotalSeconds)*1000))
        $row.raw_capture_complete = [Threading.Tasks.Task]::WaitAll(
            [Threading.Tasks.Task[]]@($outTask,$errTask), [int]$captureLeft)
        if (!$row.raw_capture_complete) { $row.cleanup += 'raw pipe closure unverified; no whole-child cleanup claim' }
    } finally {
        $process.Dispose()
        $out.Dispose()
        $err.Dispose()
    }
    $row.stdout_sha256 = Hash-Bytes ([IO.File]::ReadAllBytes($outPath))
    $row.stderr_sha256 = Hash-Bytes ([IO.File]::ReadAllBytes($errPath))
    Save-Json (Join-Path $raw ($Name+'.json')) $row
    return $row
}

$Root = [IO.Path]::GetFullPath($Root)
New-Item -ItemType Directory -Path $Root -ErrorAction Stop | Out-Null
$source = Join-Path $Root 'source'
$raw = Join-Path $Root 'raw'
New-Item -ItemType Directory -Path $source,$raw -ErrorAction Stop | Out-Null
$version = ([IO.File]::ReadAllText($Bundle) | ConvertFrom-Json).version
if ($version.revision -ne 'f08cc06b009158a5eafce00d3bc806b702a8aff7') { throw 'wrong frozen revision' }
foreach ($property in $version.files.PSObject.Properties) {
    $name = $property.Name
    if ([IO.Path]::IsPathRooted($name) -or (($name -split '[/\\]') -contains '..')) { throw 'unsafe source path' }
    $body = $encoding.GetBytes($property.Value.text)
    if ((Hash-Bytes $body) -ne $property.Value.sha256) { throw ('source hash mismatch: '+$name) }
    Write-New (Join-Path $source $name) $body
}
$temporary = Join-Path $env:USERPROFILE ('csw-'+[guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $temporary -ErrorAction Stop | Out-Null
foreach ($entry in @(Get-ChildItem Env:)) {
    if ($entry.Name -match '^(CARGO_|SHARDS_|DYLD_|SCCACHE_)' -or
        $entry.Name -in @('RUSTFLAGS','RUSTDOCFLAGS','RUSTC_WRAPPER','RUSTC_WORKSPACE_WRAPPER','RUSTC','RUSTDOC','RUSTUP_TOOLCHAIN')) {
        Remove-Item ('Env:'+ $entry.Name)
    }
}
$env:CARGO_HOME = Join-Path $Root 'cargo-home'
$env:CARGO_TARGET_DIR = Join-Path $Root 'target'
$env:CARGO_BUILD_JOBS = '2'
$env:CARGO_INCREMENTAL = '0'
$env:CARGO_PROFILE_TEST_RELEASE_CODEGEN_UNITS = '1'
$env:CARGO_TERM_COLOR = 'never'
$env:RUSTFLAGS = ''
$env:TEMP = $temporary
$env:TMP = $temporary
$cargo = (Get-Command cargo.exe).Source
$record = [ordered]@{revision=$version.revision;source=$source;temporary=$temporary;
    controller_affinity_before=$available;controller_affinity_selected=$selected;
    admission='2CPU/4GiB/12GiB, not an enforced RSS/disk quota';seconds=$Seconds;
    environment=@{CARGO_HOME=$env:CARGO_HOME;CARGO_TARGET_DIR=$env:CARGO_TARGET_DIR;
        CARGO_BUILD_JOBS=$env:CARGO_BUILD_JOBS;CARGO_INCREMENTAL=$env:CARGO_INCREMENTAL;
        CARGO_PROFILE_TEST_RELEASE_CODEGEN_UNITS=$env:CARGO_PROFILE_TEST_RELEASE_CODEGEN_UNITS;
        RUSTFLAGS=$env:RUSTFLAGS;TEMP=$env:TEMP;TMP=$env:TMP};commands=@();ordinary=@{};binaries=@()}
Save-Json (Join-Path $Root 'startup.json') $record
$common = @('+1.98.1','--locked','-j2','--profile','test-release','--target','x86_64-pc-windows-msvc')
# Cargo's subcommand precedes the shared flags; the +toolchain precedes it.
$flags = $common[1..($common.Length-1)]
$stages = @(
    @{name='compile';args=@('+1.98.1','test')+$flags+@('-p','shards-api','--lib','--test','sim','--no-run','--message-format=json')},
    @{name='list';args=@('+1.98.1','test')+$flags+@('-p','shards-api','--lib','--test','sim','--','--list')},
    @{name='lib';args=@('+1.98.1','test')+$flags+@('-p','shards-api','--lib','--','--test-threads=1','--nocapture')},
    @{name='sim';args=@('+1.98.1','test')+$flags+@('-p','shards-api','--test','sim','--','--test-threads=1','--nocapture')},
    @{name='clippy';args=@('+1.98.1','clippy')+$flags+@('-p','shards-api','-p','shards-afunix','--all-targets','--','-D','warnings')}
)
foreach ($stage in $stages) {
    $result = Run-Command $stage.name $stage.args
    $record.commands += $result
    if ($stage.name -in @('lib','sim') -and $result.Contains('exit')) {
        $text = [IO.File]::ReadAllText((Join-Path $raw ($stage.name+'.stdout')))
        $record.ordinary[$stage.name] = @{
            summaries=@([regex]::Matches($text,'test result: [^\r\n]+') | ForEach-Object {$_.Value});
            ignored=@(($text -split "`r?`n") | Where-Object {$_ -match ' \.\.\. ignored'});
            skip=@(($text -split "`r?`n") | Where-Object {$_ -match 'SKIP'})}
    }
    Save-Json (Join-Path $Root ('after-'+$stage.name+'.json')) $record
    if ($result.timed_out) { break }
}
$frozen = Join-Path $Root 'frozen'
New-Item -ItemType Directory -Path $frozen -ErrorAction Stop | Out-Null
foreach ($line in [IO.File]::ReadAllLines((Join-Path $raw 'compile.stdout'))) {
    if (!$line.StartsWith('{')) { continue }
    try { $item = $line | ConvertFrom-Json } catch { continue }
    if ($item.reason -eq 'compiler-artifact' -and $item.executable) {
        $body = [IO.File]::ReadAllBytes($item.executable)
        $path = Join-Path $frozen ([IO.Path]::GetFileName($item.executable))
        Write-New $path $body
        $record.binaries += @{original=$item.executable;frozen=$path;sha256=(Hash-Bytes $body)}
    }
}
$record.temporary_remaining = @(Get-ChildItem -LiteralPath $temporary -Recurse | ForEach-Object {$_.FullName})
$record.original_source_hashes = $version.files
$record.elapsed_seconds = $clock.Elapsed.TotalSeconds
Save-Json (Join-Path $Root 'summary.json') $record
([pscustomobject]@{revision=$record.revision;source=$record.source;ordinary=$record.ordinary;
    commands=$record.commands;binaries=$record.binaries;temporary=$record.temporary;
    temporary_remaining=$record.temporary_remaining;controller_affinity_selected=$selected}) | ConvertTo-Json -Depth 12
if (@($record.commands | Where-Object {!$_.Contains('exit') -or $_.exit -ne 0 -or $_.timed_out -or !$_.raw_capture_complete}).Count -ne 0) { exit 1 }
