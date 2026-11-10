$ErrorActionPreference = "Stop"
$root = Join-Path $env:USERPROFILE "managed-capture-windows-check"
$records = Join-Path $env:USERPROFILE "root-managed-native-windows-1533-v1"
if (Test-Path -LiteralPath $records) { throw "Evidence directory already exists" }
New-Item -ItemType Directory -Path $records | Out-Null
Set-Location -LiteralPath $root
$status = git status --porcelain
if ($LASTEXITCODE -ne 0 -or $status) { throw "Source is not clean" }
git fetch origin main p/managed-combined *> (Join-Path $records "fetch.log")
if ($LASTEXITCODE -ne 0) { throw "Fetch failed" }
$rules = ((git show origin/main:docs/development-rules.md) -join "`n") + "`n"
if ($LASTEXITCODE -ne 0) { throw "Policy read failed" }
$policy = ((git show origin/main:AGENTS.md) -join "`n") + "`n"
if ($LASTEXITCODE -ne 0) { throw "AGENTS read failed" }
$sha = [Security.Cryptography.SHA256]::Create()
$hash = -join ($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($rules)) | ForEach-Object { $_.ToString("x2") })
if ($hash -ne "d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242") { throw "Changed policy requires coordinator review" }
"POLICY_READ coordinator current origin/main AGENTS and R01-R16; rules SHA256 $hash; native remote managed unit tests only" | Set-Content (Join-Path $records "policy-read.txt")
git switch --detach 15330d9bd34cde2649e0a36f8e44f76fee520c2c *> (Join-Path $records "checkout.log")
if ($LASTEXITCODE -ne 0) { throw "Checkout failed" }
git rev-parse HEAD | Set-Content (Join-Path $records "source.txt")
rustc --version --verbose | Set-Content (Join-Path $records "rustc.txt")
cargo --version | Set-Content (Join-Path $records "cargo.txt")
$env:CARGO_BUILD_JOBS = "4"
$env:CARGO_TARGET_DIR = Join-Path $env:USERPROFILE "managed-capture-windows-target"
$nasm = Join-Path $env:USERPROFILE "managed-native-tools\nasm-3.02"
if (Test-Path -LiteralPath (Join-Path $nasm "nasm.exe")) { $env:PATH = $nasm + ";" + $env:PATH }
$argv = @("test","--locked","-j4","-p","shards-remote","--lib","managed::tests","--","--test-threads=1")
@{args=$argv;target=$env:CARGO_TARGET_DIR;root=$root;scope="native Windows amd64 only; no VMs or system setting changes"} | ConvertTo-Json | Set-Content (Join-Path $records "command.json")
& cargo @argv *> (Join-Path $records "test.log")
$rc = $LASTEXITCODE
$rc | Set-Content (Join-Path $records "test.exit")
Get-Content (Join-Path $records "test.log") -Tail 25
exit $rc
