"""Wait for full142 and original snapshot regression, archive both, then profile the original builder case.

Never replace the running full suite's guest or launch a second full suite. A polling
failure is not evidence that the native task terminated.
"""
import base64
import datetime
import hashlib
import json
import plistlib
from pathlib import Path
import runpy
import subprocess
import tarfile
import time

lib=runpy.run_path('/Users/adalundhe/g121-a4-winarm/trial-vbsoff.py',run_name='fixture_library')
kit,evidence=lib['k'],lib['e']
candidate=kit/'builder-phase-candidate01'
record=json.loads((candidate/'candidate.json').read_text())
reference=kit/'storm-report-candidate01'
baseline=json.loads((reference/'candidate.json').read_text())
record.update(host_executables_unchanged=142,
 guest_candidate_sha256=record['guest_sha256'],
 guest_original_sha256=baseline['guest_original_sha256'],runner_sha256=baseline['runner_sha256'])
import shutil
shutil.copyfile(reference/'run-tests.ps1',candidate/'run-tests.ps1')
shutil.copyfile(reference/'test-manifest.json',candidate/'test-manifest.json')
assert record['host_executables_unchanged']==142
assert hashlib.file_digest((candidate/'shards-testguest').open('rb'),'sha256').hexdigest()==record['guest_candidate_sha256']
phase=kit/'builder-phase-queue01.json'
with phase.open('x') as output:
 json.dump({'phase':'waiting for existing full and snapshot regression; no deployment','utc':datetime.datetime.now(datetime.timezone.utc).isoformat()},output)

body=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
$s=New-Object -ComObject Schedule.Service;$s.Connect();$folder=$s.GetFolder('\ShardsStep10')
if($folder.GetTask('FullReal04').State -ne 3){'WAIT prerequisite full still active';exit 0}
$boot=(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime()
if($boot -lt [DateTimeOffset]::Parse('2026-11-04T02:23:00Z').UtcDateTime -or $boot -gt [DateTimeOffset]::Parse('2026-11-04T02:25:00Z').UtcDateTime){throw 'Host rebooted; inspect native evidence'}
$full=@(Get-Content C:\shards-step10\full-real-04\results.jsonl | Where-Object {$_} | ForEach-Object {$_ | ConvertFrom-Json})
if($full.Count -ne 142 -or @($full.index | Sort-Object -Unique).Count -ne 142){throw 'Full task incomplete; no diagnostic'}
if(-not (Select-String -Path C:\shards-step10\full-real04-normal-controller.txt -Pattern '^COMPLETE binaries=142 manifest=142 failed=\d+ filter=/ start=0\s*$')){throw 'Full footer missing'}
$task=Get-ScheduledTask -TaskName StormReport01 -TaskPath '\ShardsStep10\' -ErrorAction SilentlyContinue
if(-not $task -or $folder.GetTask('StormReport01').State -ne 3){'WAIT original snapshot regression not terminal';exit 0}
$rows=@(Get-Content C:\shards-step10-storm-report01\full-storm-report01\results.jsonl | Where-Object {$_} | ForEach-Object {$_ | ConvertFrom-Json})
if($rows.Count -ne 1 -or $rows[0].name -ne 'snapshot'){throw 'Snapshot regression incomplete'}
if(-not (Select-String -Path C:\shards-step10\storm-report01-controller.txt -Pattern '^COMPLETE binaries=1 manifest=142 failed=\d+ filter=snapshot/storm::restores_of_a_busy_machine_lose_no_interrupt_and_repeat_no_write start=0\s*$')){throw 'Snapshot selected footer missing'}
if(-not (Test-Path C:\shards-step10\storm-report01-child.exit)){throw 'Snapshot native child result missing'}
$active=@(Get-CimInstance Win32_Process | Where-Object {$_.Name -match '^(shards|containers|daemon|boot|budget|snapshot|build_windows).*\.exe$'})
if($active.Count){'WAIT native workload cleanup';exit 0}
'PREREQUISITES TERMINAL; snapshot native exit='+$rows[0].exit
exit 0
'''
# The local launch phase only permits observing Windows; native guards below
# still prove completion. Keep one SSH prerequisite observer active at a time.
while True:
 assert lib['current_pid']()==22354,'Fixture changed; no deployment'
 prior=json.loads((kit/'storm-report-queue01.json').read_text())
 if prior['phase'].startswith('candidate regression launched;'):break
 label=plistlib.loads((kit/'queue-storm-report.plist').read_bytes())['Label']
 handle=subprocess.run(['launchctl','print','gui/501/'+label],text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
 if handle.returncode or '\tstate = running' not in handle.stdout:
  raise SystemExit('Snapshot queue ended before its launch phase; inspect native tasks')
 time.sleep(45)

attempt=0
while True:
 attempt+=1
 assert lib['current_pid']()==22354,'Fixture changed; no candidate deployed'
 try:
  status=lib['remote'](body,'builder-phase-prerequisite-latest',600)
 except subprocess.TimeoutExpired:
  lib['log']('Observation timeout; retain existing tasks and reobserve')
  time.sleep(45)
  continue
 text=(evidence/'builder-phase-prerequisite-latest.txt').read_text(errors='replace')
 if status.returncode==255:
  lib['log']('SSH transport observation failed; retain native tasks and reobserve')
  time.sleep(300)
  continue
 if status.returncode!=0:raise SystemExit('Native prerequisite guard failed; no deployment')
 if 'PREREQUISITES TERMINAL;' in text:break
 if 'WAIT ' not in text:raise SystemExit('Unrecognized observation; no deployment')
 if attempt==1 or attempt%10==0:lib['log']('Waiting for existing full/snapshot tasks; no deployment, observation '+str(attempt))
 time.sleep(45)

full_archive=evidence/'full-real-04-complete.tar'
audit=json.loads((evidence/'full-real-04-completion-audit.json').read_text())
assert audit['complete'] and audit['native_binaries']==142
assert hashlib.file_digest(full_archive.open('rb'),'sha256').hexdigest()==audit['archive_sha256']
status=lib['remote'](r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
if(Test-Path C:\shards-step10\storm-report01-complete.tar){throw 'Existing snapshot archive; inspect rather than overwrite'}
tar -cf C:\shards-step10\storm-report01-complete.tar -C C:\shards-step10-storm-report01 full-storm-report01 -C C:\shards-step10 storm-report01-controller.txt storm-report01-child.exit storm-report01-broker.txt storm-report01-broker.exit
if($LASTEXITCODE){throw 'Snapshot archive failed'}
exit 0
''','builder-phase-snapshot-archive01',900)
assert status.returncode==0
archive=evidence/'storm-report01-complete.tar'
subprocess.run(['scp',*lib['options'][:-2],'-P','2222','shards@127.0.0.1:C:/shards-step10/storm-report01-complete.tar',str(archive)],check=True,timeout=1200)
with tarfile.open(archive) as archived:
 member=next(m for m in archived.getmembers() if m.name.endswith('results.jsonl'))
 rows=[json.loads(line) for line in archived.extractfile(member).read().decode('utf-8-sig').splitlines() if line.strip()]
 assert len(rows)==1 and rows[0]['name']=='snapshot' and isinstance(rows[0]['exit'],int)
 for stream in ['stdout','stderr']:
  assert any(m.name.endswith('-snapshot.'+stream+'.txt') for m in archived.getmembers())
(evidence/'storm-report01-completion-audit.json').write_text(json.dumps({
 'complete_selected':True,'native_binaries':1,'native_exit':rows[0]['exit'],
 'archive_sha256':hashlib.file_digest(archive.open('rb'),'sha256').hexdigest(),
 'qualification':'selected original regression only; full real-VM suite still required'
},indent=2)+'\n')
phase.write_text(json.dumps({'phase':'prerequisites terminal and archived; preparing diagnostic','utc':datetime.datetime.now(datetime.timezone.utc).isoformat()},indent=2)+'\n')

status=lib['remote'](r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
$root='C:\shards-step10-builder-phase01'
if(Test-Path $root){throw 'Candidate kit already exists; no duplicate deployment'}
New-Item -ItemType Directory -Path "$root\test-guests\guest" | Out-Null
Copy-Item C:\shards-step10\test-manifest.json "$root\test-manifest.json"
$user=Get-LocalUser shards-e2e
& icacls.exe $root /grant ('*'+$user.SID.Value+':(OI)(CI)M') /T /C /Q
if($LASTEXITCODE){throw 'Candidate kit ACL failed'}
exit 0''','builder-phase-kit-prepare01',600)
assert status.returncode==0
for local,remote in [(candidate/'shards-testguest','C:/shards-step10-builder-phase01/test-guests/guest/shards-testguest'),
                     (candidate/'run-tests.ps1','C:/shards-step10-builder-phase01/run-tests.ps1')]:
 subprocess.run(['scp',*lib['options'][:-2],'-P','2222',str(local),'shards@127.0.0.1:'+remote],check=True,timeout=600)
body=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
if((Get-FileHash C:\shards-step10-builder-phase01\test-guests\guest\shards-testguest -Algorithm SHA256).Hash -ne 'EXPECTED_NEW_GUEST'){throw 'Candidate guest hash mismatch'}
if((Get-FileHash C:\shards-step10\test-guests\guest\shards-testguest -Algorithm SHA256).Hash -ne 'EXPECTED_OLD_GUEST'){throw 'Original full guest was changed'}
if((Get-FileHash C:\shards-step10-builder-phase01\run-tests.ps1 -Algorithm SHA256).Hash -ne 'EXPECTED_RUNNER'){throw 'Original runner hash mismatch'}
$manifest=Get-Content C:\shards-step10-builder-phase01\test-manifest.json -Raw | ConvertFrom-Json
if($manifest.Count -ne 142){throw 'Manifest count mismatch'}
foreach($test in $manifest){if((Get-FileHash $test.exe -Algorithm SHA256).Hash -ne $test.sha256){throw 'Shared host executable changed'}}
'Separate candidate kit verified; original full guest unchanged;142 host hashes valid.'
exit 0
'''.replace('EXPECTED_NEW_GUEST',record['guest_candidate_sha256']).replace('EXPECTED_OLD_GUEST',record['guest_original_sha256']).replace('EXPECTED_RUNNER',record['runner_sha256'])
status=lib['remote'](body,'builder-phase-kit-verify01',1200)
assert status.returncode==0
child=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue';$script:result=2
try{
 & {
  Get-Date -Format o
  if([Diagnostics.Process]::GetCurrentProcess().PriorityClass -ne [Diagnostics.ProcessPriorityClass]::Normal){throw 'Normal priority required'}
  & C:\shards-step10\probe-user.ps1 -User shards-e2e
  if(-not $?){throw 'Standard-user permissions failed'}
  Remove-Item Env:SHARDS_STORM_MS -ErrorAction SilentlyContinue
  Remove-Item Env:SHARDS_STORM_CPUS -ErrorAction SilentlyContinue
  'Phase diagnostic only: unchanged builder alloc-par workload,1job/3072MiB over1024MiB tmpfs,8192MiB limit,180s assertion; instrumentation cannot qualify timing.'
  & C:\shards-step10-builder-phase01\run-tests.ps1 -Kit C:\shards-step10-builder-phase01 -Kernel C:\shards-step10\Image-6.18.48-aarch64-64a9427a8997 -Tag builder-phase01 -Binary build_windows -Test a_builders_memory_is_plugged_as_fast_as_its_jobs_take_it -HeartbeatSeconds 30
  $script:result=$LASTEXITCODE
 } *> C:\shards-step10\builder-phase01-controller.txt
}catch{$_ | Out-String | Add-Content C:\shards-step10\builder-phase01-controller.txt;$script:result=2}
[IO.File]::WriteAllText('C:\shards-step10\builder-phase01-child.exit',[string]$script:result)
exit $script:result
'''
source=kit/'local-user-broker.cs'
broker=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue';$script:result=2
try{
 & {
  Get-Date -Format o
  if((Get-FileHash C:\shards-step10-admin\local-user-broker.cs -Algorithm SHA256).Hash -ne 'EXPECTED_SOURCE_SHA256'){throw 'Broker source hash mismatch'}
  Add-Type -Path C:\shards-step10-admin\local-user-broker.cs
  $credential=Import-Clixml C:\shards-step10-admin\local-user.credential.xml
  $user=Get-LocalUser shards-e2e
  if($user.Description -ne 'Temporary Shards step10 native E2E fixture'){throw 'Unexpected account'}
  $script:result=[Step10LocalUserBroker]::Run($user.Name,$credential.GetNetworkCredential().Password,$user.SID.Value,'EXPECTED_CHILD',[uint32]::MaxValue)
 } *> C:\shards-step10\builder-phase01-broker.txt
}catch{$_ | Out-String | Add-Content C:\shards-step10\builder-phase01-broker.txt;$script:result=2}
[IO.File]::WriteAllText('C:\shards-step10\builder-phase01-broker.exit',[string]$script:result)
exit $script:result
'''.replace('EXPECTED_SOURCE_SHA256',hashlib.sha256(source.read_bytes()).hexdigest()).replace('EXPECTED_CHILD',base64.b64encode(child.encode('utf-16le')).decode())
script=kit/'builder-phase01-broker.ps1';script.write_text(broker)
subprocess.run(['scp',*lib['options'][:-2],'-P','2222',str(script),'shards@127.0.0.1:C:/shards-step10-admin/builder-phase01-broker.ps1'],check=True,timeout=300)
body=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
$s=New-Object -ComObject Schedule.Service;$s.Connect();$f=$s.GetFolder('\ShardsStep10')
if($f.GetTask('FullReal04').State -ne 3 -or $f.GetTask('StormReport01').State -ne 3){throw 'Prerequisite task no longer terminal; no diagnostic'}
if(Get-ScheduledTask -TaskName BuilderPhase01 -TaskPath '\ShardsStep10\' -ErrorAction SilentlyContinue){throw 'Existing selected task; no duplicate'}
if(Test-Path C:\shards-step10\builder-phase01-controller.txt){throw 'Existing selected output; no duplicate'}
if((Get-FileHash C:\shards-step10-admin\builder-phase01-broker.ps1 -Algorithm SHA256).Hash -ne 'EXPECTED_BROKER_SHA256'){throw 'Broker script hash mismatch'}
$action=New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -Argument '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File C:\shards-step10-admin\builder-phase01-broker.ps1' -WorkingDirectory C:\shards-step10
$principal=New-ScheduledTaskPrincipal -UserId SYSTEM -LogonType ServiceAccount -RunLevel Highest
$settings=New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
$settings.Priority=4
Register-ScheduledTask -TaskName BuilderPhase01 -TaskPath '\ShardsStep10\' -Action $action -Principal $principal -Settings $settings | Select-Object TaskName,State | Format-List
Start-ScheduledTask -TaskName BuilderPhase01 -TaskPath '\ShardsStep10\'
'Builder phase diagnostic launched only after full142 and the snapshot regression were archived.'
exit 0
'''.replace('EXPECTED_BROKER_SHA256',hashlib.sha256(script.read_bytes()).hexdigest())
status=lib['remote'](body,'builder-phase01-launch',600)
assert status.returncode==0
record.update(deployed=True,native_candidate_result='running; observe actual terminal records',
    broker_script_sha256=hashlib.sha256(script.read_bytes()).hexdigest())
(candidate/'candidate.json').write_text(json.dumps(record,indent=2)+'\n')
phase.write_text(json.dumps({'phase':'builder phase diagnostic launched; full142 and snapshot regression archived first','utc':datetime.datetime.now(datetime.timezone.utc).isoformat()},indent=2)+'\n')
lib['log']('Selected regression launched; no pass inferred and no complete-suite qualification')
