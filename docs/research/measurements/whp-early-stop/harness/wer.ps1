# Crash records of shards' processes in the Application log (Windows Error Reporting):
#   powershell -NoProfile -File wer.ps1 [DAYS] [PATTERN]
param([int]$Days = 3, [string]$Pattern = 'shards')
$since = (Get-Date).AddDays(-$Days)
$all = Get-WinEvent -FilterHashtable @{ LogName = 'Application'; StartTime = $since } -ErrorAction SilentlyContinue
"application events since ${since}: $($all.Count)"
$errs = $all | Where-Object { $_.ProviderName -in @('Application Error', 'Windows Error Reporting', 'Application Hang') }
"crash/hang/WER events: $($errs.Count)"
$errs | Group-Object ProviderName | ForEach-Object { "  $($_.Name): $($_.Count)" }
$ours = $errs | Where-Object { $_.Message -match $Pattern }
"matching '${Pattern}': $($ours.Count)"
foreach ($e in ($ours | Select-Object -First 60)) {
  $m = ($e.Message -replace '\s+', ' ')
  if ($m.Length -gt 900) { $m = $m.Substring(0, 900) }
  "--- $($e.TimeCreated.ToString('yyyy-MM-dd HH:mm:ss.fff')) id=$($e.Id) $($e.ProviderName)"
  $m
}
