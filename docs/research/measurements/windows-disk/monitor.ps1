# What the tests take of a Windows host's disk as they run (PM M377): every second, the temp
# disk's free space; the page files' allocated size and the system's commit charge and limit
# (a system-managed page file grows on its disk as commit nears its limit); and the files
# under %TEMP%\shards-tests that take the most of the disk, by the clusters they hold
# (GetCompressedFileSizeW: a sparse file's holes hold none), with their lengths; until
# OUT.stop exists. Usage: monitor.ps1 OUT
param([string]$Out)
Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices;
public static class Held {
  [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
  static extern uint GetCompressedFileSizeW(string name, out uint high);
  public static long Of(string name) {
    uint high; uint low = GetCompressedFileSizeW(name, out high);
    if (low == 0xFFFFFFFF && Marshal.GetLastWin32Error() != 0) return -1;
    return ((long)high << 32) | low;
  }
}
"@
$root = Join-Path $env:TEMP 'shards-tests'
$drive = $env:TEMP.Substring(0, 1)
while (-not (Test-Path "$Out.stop")) {
  $free = (Get-PSDrive $drive).Free / 1GB
  $all = @()
  if (Test-Path $root) {
    $all = @(Get-ChildItem $root -Recurse -File -Force -ErrorAction SilentlyContinue |
      ForEach-Object { [pscustomobject]@{ Held = [Held]::Of($_.FullName); Len = $_.Length; Name = $_.FullName.Substring($root.Length) } })
  }
  # Every file's clusters, and the files whose clusters this user may not read, with their lengths.
  $held = ($all | Where-Object Held -ge 0 | Measure-Object Held -Sum).Sum / 1GB
  $unread = @($all | Where-Object Held -lt 0)
  $unreadLen = ($unread | Measure-Object Len -Sum).Sum / 1GB
  $files = $all | Sort-Object Held -Descending | Select-Object -First 3
  $top = "files {0}, holding {1:N1} GB; {2} unreadable, {3:N1} GB long; " -f $all.Count, $held, $unread.Count, $unreadLen
  $top += ($files | ForEach-Object { "{0:N0}MB/{1:N0}MB {2}" -f ($_.Held / 1MB), ($_.Len / 1MB), $_.Name }) -join '; '
  $page = (Get-CimInstance Win32_PageFileUsage | Measure-Object AllocatedBaseSize -Sum).Sum
  $os = Get-CimInstance Win32_OperatingSystem
  $limit = $os.TotalVirtualMemorySize / 1MB
  $commit = ($os.TotalVirtualMemorySize - $os.FreeVirtualMemory) / 1MB
  "{0:HH:mm:ss} free {1:N1} GB; page files {2} MB; commit {3:N1} of {4:N1} GB; {5}" -f (Get-Date).ToUniversalTime(), $free, $page, $commit, $limit, $top | Add-Content $Out
  Start-Sleep 1
}
