"""Check that missing clients abort qualification before any executable starts."""
import base64
from pathlib import Path
import runpy

root=Path('/Users/adalundhe/Projects/shards/.claude/worktrees/g121-wa64')
path=root/'docs/research/measurements/whp-arm64/nested-host/run-tests.ps1'
encoded=base64.b64encode(path.read_bytes()).decode()
lib=runpy.run_path('/Users/adalundhe/g121-a4-winarm/trial-vbsoff.py',run_name='fixture_library')
body=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
$source=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('EXPECTED_SOURCE'))
$unused='C:\shards-step10-missing-clients-control-do-not-create'
if(Test-Path $unused){throw 'Control path unexpectedly exists; no execution'}
$rejected=$false
try{
 & ([ScriptBlock]::Create($source)) -Kernel C:\shards-step10\Image-6.18.48-aarch64-64a9427a8997 -Kit $unused -Binary compose
}catch{
 $message=$_.Exception.Message
 if($message -notmatch '^Missing qualification client: '){throw}
 'EXPECTED PREFLIGHT REJECTION: '+$message
 $rejected=$true
}
if(-not $rejected){throw 'Missing clients were not rejected'}
if(Test-Path $unused){throw 'Control created an unexpected path'}
'PASS: missing clients rejected before manifest access or test launch; no fixture path created.'
exit 0
'''.replace('EXPECTED_SOURCE',encoded)
status=lib['remote'](body,'client-coverage-preflight-negative01',600)
raise SystemExit(status.returncode)
