#!/usr/bin/env bash
# PM M621: runs a Windows test binary <count> times, each in a process WMI creates (Win32_Process.Create),
# outside the runner's process tree: on jupiter, processes kiln starts run at BELOW_NORMAL_PRIORITY_CLASS and
# raise no thread above THREAD_PRIORITY_NORMAL. From Git Bash, in the worktree the binary was built in.
# usage: wmi-run.sh <test-exe> <out-file> <count> <test args...>
set -u
EXE=$1; OUT=$2; N=$3; shift 3
WEXE=$(cygpath -w "$EXE"); WDIR=$(cygpath -w "$PWD")
ONEU=$(mktemp); ONE=$(cygpath -w "$ONEU")
ARGS="$*"
: > "$OUT"
for i in $(seq 1 "$N"); do
  rm -f "$ONEU"
  powershell -NoProfile -Command "\$r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = 'cmd /c \"\"$WEXE\" $ARGS > $ONE 2>&1\"'; CurrentDirectory = '$WDIR' }; if (\$r.ReturnValue -ne 0) { 'create failed ' + \$r.ReturnValue; exit 1 }; \$p = \$r.ProcessId; while (Get-Process -Id \$p -ErrorAction SilentlyContinue) { Start-Sleep -Milliseconds 100 }" | tr -d '\r'
  echo "=== run $i" >> "$OUT"
  tr -d '\r' < "$ONEU" >> "$OUT"
done
rm -f "$ONEU"
