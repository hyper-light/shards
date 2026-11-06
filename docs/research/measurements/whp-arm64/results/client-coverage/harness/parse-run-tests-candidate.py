"""Parse the candidate launcher using native PowerShell5.1; do not execute it."""
import base64
from pathlib import Path
import runpy

root=Path('/Users/adalundhe/Projects/shards/.claude/worktrees/g121-wa64')
path=root/'docs/research/measurements/whp-arm64/nested-host/run-tests.ps1'
contents=base64.b64encode(path.read_bytes()).decode()
lib=runpy.run_path('/Users/adalundhe/g121-a4-winarm/trial-vbsoff.py',run_name='fixture_library')
body=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
$source=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('EXPECTED_SOURCE'))
$tokens=$null;$errors=$null
$null=[Management.Automation.Language.Parser]::ParseInput($source,[ref]$tokens,[ref]$errors)
if($errors.Count){$errors | Format-List;exit 1}
'Native PowerShell5.1 parsed candidate launcher; script was not executed.'
$PSVersionTable.PSVersion.ToString()
exit 0
'''.replace('EXPECTED_SOURCE',contents)
status=lib['remote'](body,'client-coverage-runner-parse01',600)
raise SystemExit(status.returncode)
