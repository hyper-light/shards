$ErrorActionPreference = 'Stop'
$result = @{}
$result.scope = 'Read-only adapter/loader/Vulkan summary; no VM, device queues or performance claim'
$result.os = [System.Environment]::OSVersion.VersionString
$result.process_arch = $env:PROCESSOR_ARCHITECTURE
$result.adapters = @(Get-CimInstance Win32_VideoController | Select-Object Name, DriverVersion, VideoProcessor)
$loader = Join-Path $env:WINDIR 'System32/vulkan-1.dll'
if (Test-Path -LiteralPath $loader) {
    $item = Get-Item -LiteralPath $loader
    $result.vulkan_loader = @{
        path = $loader
        version = $item.VersionInfo.FileVersion
        sha256 = (Get-FileHash -LiteralPath $loader -Algorithm SHA256).Hash
    }
} else {
    $result.vulkan_loader = 'unavailable: no System32 Vulkan loader'
}
$info = Get-Command vulkaninfo.exe -ErrorAction SilentlyContinue
if ($info) {
    $result.tool = @{
        path = $info.Source
        sha256 = (Get-FileHash -LiteralPath $info.Source -Algorithm SHA256).Hash
    }
    $start = New-Object System.Diagnostics.ProcessStartInfo
    $start.FileName = $info.Source
    $start.Arguments = '--summary'
    $start.UseShellExecute = $false
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $start
    try {
        if (!$process.Start()) { throw 'vulkaninfo did not start' }
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (!$process.WaitForExit(45000)) {
            $process.Kill()
            throw 'vulkaninfo exceeded the explicit 45s probe timeout'
        }
        $result.vulkan_stdout = $stdout.Result
        $result.vulkan_stderr = $stderr.Result
        $result.vulkan_exit = $process.ExitCode
    } finally {
        $process.Dispose()
    }
} else {
    $result.vulkan_summary = 'unavailable: vulkaninfo not installed; no API enumeration or installation attempted'
}
$result | ConvertTo-Json -Depth 5 -Compress
