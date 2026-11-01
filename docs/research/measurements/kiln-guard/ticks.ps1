# PM M418: how long a process kiln runs is frozen at a time. A thread sleeps a millisecond at a
# time for three minutes and says the gaps between its wakings: their p50 to max, how many
# pass each bound, the time spent frozen past 50 ms, and the first 40 such gaps with when
# each began (UTC). Run it as a kiln run (`kiln run --on jupiter --no-sync`), beside the
# work the guard paces.
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$last = 0.0
$gaps = New-Object System.Collections.Generic.List[double]
$big = New-Object System.Collections.Generic.List[string]
$t0 = (Get-Date).ToUniversalTime()
while ($sw.Elapsed.TotalMilliseconds -lt 180000) {
  [System.Threading.Thread]::Sleep(1)
  $now = $sw.Elapsed.TotalMilliseconds
  $g = $now - $last
  $gaps.Add($g)
  if ($g -gt 50) { $big.Add(("{0} +{1:N0} ms" -f $t0.AddMilliseconds($last).ToString("HH:mm:ss.fff"), $g)) }
  $last = $now
}
$s = $gaps | Sort-Object
$n = $s.Count
"n={0} p50={1:N2} p90={2:N2} p99={3:N2} p99.9={4:N2} max={5:N1} ms" -f $n, $s[[int]($n*0.5)], $s[[int]($n*0.9)], $s[[int]($n*0.99)], $s[[int]($n*0.999)], $s[$n-1]
foreach ($b in @(16, 50, 100, 250, 500, 1000)) { "gaps over {0} ms: {1}" -f $b, ($gaps | Where-Object { $_ -gt $b }).Count }
"frozen ms in all: {0:N0} of {1:N0}" -f (($gaps | Where-Object { $_ -gt 50 } | Measure-Object -Sum).Sum), $sw.Elapsed.TotalMilliseconds
$big | Select-Object -First 40
