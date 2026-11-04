"""Run targeted client coverage after the existing full/snapshot/builder tasks.

This does not schedule a combined validation pass. Every prerequisite is checked
against native task state before deployment, and no launcher exit implies a VM pass.
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
candidate=kit/'client-coverage-candidate01'
record=json.loads((candidate/'candidate.json').read_text())
assert record['host_executables_unchanged']==141
for name,key in [('run-tests.ps1','runner_sha256'),('test-manifest.json','manifest_sha256')]:
    assert hashlib.file_digest((candidate/name).open('rb'),'sha256').hexdigest()==record[key]
assert hashlib.file_digest((candidate/'shards-testguest').open('rb'),'sha256').hexdigest()==record['guest_candidate_sha256']
changed=record['changed_host_binary']
assert hashlib.file_digest((candidate/changed['file']).open('rb'),'sha256').hexdigest()==changed['sha256']
phase=kit/'client-coverage-queue01.json'
def state(value):
    phase.write_text(json.dumps({'phase':value,'utc':datetime.datetime.now(datetime.timezone.utc).isoformat()},indent=2)+'\n')
with phase.open('x') as output:
    json.dump({'phase':'waiting for existing builder diagnostic to launch; no deployment'},output)

# Use the local phase only to decide when to observe Windows. It is never proof that
# a native task completed. Avoid adding SSH polling load during the original full run.
while True:
    assert lib['current_pid']()==22354,'Fixture changed; inspect native state, no deployment'
    prior=json.loads((kit/'builder-phase-queue01.json').read_text())
    if prior['phase'].startswith('builder phase diagnostic launched;'):break
    handle=subprocess.run(['launchctl','print','gui/501/'+plistlib.loads((kit/'queue-builder-phases.plist').read_bytes())['Label']],
                          text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
    if handle.returncode or '\tstate = running' not in handle.stdout:
        raise SystemExit('Builder queue ended before its launch phase; inspect actual native tasks')
    time.sleep(45)

body=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
$s=New-Object -ComObject Schedule.Service;$s.Connect();$f=$s.GetFolder('\ShardsStep10')
foreach($name in @('FullReal04','StormReport01','BuilderPhase01')){
 if($f.GetTask($name).State -ne 3){'WAIT prerequisite task '+$name;exit 0}
}
$boot=(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime()
if($boot -lt [DateTimeOffset]::Parse('2026-11-04T02:23:00Z').UtcDateTime -or $boot -gt [DateTimeOffset]::Parse('2026-11-04T02:25:00Z').UtcDateTime){throw 'Host rebooted; inspect native evidence'}
$rows=@(Get-Content C:\shards-step10-builder-phase01\full-builder-phase01\results.jsonl | Where-Object {$_} | ForEach-Object {$_ | ConvertFrom-Json})
if($rows.Count -ne 1 -or $rows[0].name -ne 'build_windows'){throw 'Builder diagnostic incomplete'}
if(-not (Select-String -Path C:\shards-step10\builder-phase01-controller.txt -Pattern '^COMPLETE binaries=1 manifest=142 failed=\d+ filter=build_windows/a_builders_memory_is_plugged_as_fast_as_its_jobs_take_it start=0\s*$')){throw 'Builder diagnostic footer missing'}
if(-not (Test-Path C:\shards-step10\builder-phase01-child.exit)){throw 'Native builder child exit missing'}
$active=@(Get-CimInstance Win32_Process | Where-Object {$_.Name -match '^(shards|containers|daemon|boot|budget|snapshot|build_windows|buildx|compose|engine).*\.exe$'})
if($active.Count){'WAIT native workload cleanup';exit 0}
'PREREQUISITES TERMINAL; builder native exit='+$rows[0].exit
exit 0
'''
while True:
    assert lib['current_pid']()==22354,'Fixture changed; no deployment'
    try:status=lib['remote'](body,'client-coverage-prerequisite-latest',600)
    except subprocess.TimeoutExpired:
        lib['log']('Observation timeout; retain native tasks and reobserve')
        time.sleep(45)
        continue
    text=(evidence/'client-coverage-prerequisite-latest.txt').read_text(errors='replace')
    if status.returncode==255:
     lib['log']('SSH transport observation failed; retain native tasks and reobserve')
     time.sleep(300)
     continue
    if status.returncode:raise SystemExit('Native prerequisite guard failed; no deployment')
    if 'PREREQUISITES TERMINAL;' in text:break
    if 'WAIT ' not in text:raise SystemExit('Unrecognized prerequisite state; no deployment')
    time.sleep(45)

for archive_name,audit_name in [('full-real-04-complete.tar','full-real-04-completion-audit.json'),
                              ('storm-report01-complete.tar','storm-report01-completion-audit.json')]:
    audit=json.loads((evidence/audit_name).read_text())
    assert hashlib.file_digest((evidence/archive_name).open('rb'),'sha256').hexdigest()==audit['archive_sha256']
state('prerequisites terminal; archiving builder before client deployment')
status=lib['remote'](r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
if(Test-Path C:\shards-step10\builder-phase01-complete.tar){throw 'Existing builder archive; inspect rather than overwrite'}
tar -cf C:\shards-step10\builder-phase01-complete.tar -C C:\shards-step10-builder-phase01 full-builder-phase01 -C C:\shards-step10 builder-phase01-controller.txt builder-phase01-child.exit builder-phase01-broker.txt builder-phase01-broker.exit
if($LASTEXITCODE){throw 'Builder archive failed'}
exit 0
''','client-coverage-builder-archive01',900)
assert status.returncode==0
archive=evidence/'builder-phase01-complete.tar'
subprocess.run(['scp',*lib['options'][:-2],'-P','2222','shards@127.0.0.1:C:/shards-step10/builder-phase01-complete.tar',str(archive)],check=True,timeout=1200)
with tarfile.open(archive) as archived:
    item=next(m for m in archived.getmembers() if m.name.endswith('results.jsonl'))
    rows=[json.loads(line) for line in archived.extractfile(item).read().decode('utf-8-sig').splitlines() if line.strip()]
    assert len(rows)==1 and rows[0]['name']=='build_windows' and isinstance(rows[0]['exit'],int)
    assert all(any(m.name.endswith('-build_windows.'+stream+'.txt') for m in archived.getmembers()) for stream in ['stdout','stderr'])
(evidence/'builder-phase01-completion-audit.json').write_text(json.dumps({
    'complete_selected':True,'native_exit':rows[0]['exit'],
    'archive_sha256':hashlib.file_digest(archive.open('rb'),'sha256').hexdigest(),
    'qualification':'instrumented builder diagnostic only; original timing assertion remains unqualified',
},indent=2)+'\n')

status=lib['remote'](r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
$root='C:\shards-step10-clients01'
if(Test-Path $root){throw 'Existing client kit; inspect rather than overwrite'}
foreach($dir in @('clients','tests','test-guests')){New-Item -ItemType Directory -Path "$root\$dir" | Out-Null}
Copy-Item C:\shards-step10\test-guests\* "$root\test-guests" -Recurse
$user=Get-LocalUser shards-e2e
if($user.Description -ne 'Temporary Shards step10 native E2E fixture'){throw 'Unexpected test account'}
& icacls.exe $root /grant ('*'+$user.SID.Value+':(OI)(CI)M') /T /C /Q
if($LASTEXITCODE){throw 'Client kit ACL failed'}
exit 0
''','client-coverage-kit-prepare01',600)
assert status.returncode==0
transfers=[(candidate/'test-manifest.json','test-manifest.json'),(candidate/'run-tests.ps1','run-tests.ps1'),
           (candidate/'shards-testguest','test-guests/guest/shards-testguest'),
           (candidate/changed['file'],'tests/'+changed['file'])]
transfers.extend((path,'clients/'+path.name) for path in (candidate/'clients').iterdir() if path.is_file())
for local,relative in transfers:
    subprocess.run(['scp',*lib['options'][:-2],'-P','2222',str(local),
                    'shards@127.0.0.1:C:/shards-step10-clients01/'+relative],check=True,timeout=600)
body=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
$root='C:\shards-step10-clients01'
if((Get-FileHash "$root\run-tests.ps1" -Algorithm SHA256).Hash -ne 'RUNNER_HASH'){throw 'Runner hash mismatch'}
if((Get-FileHash "$root\test-manifest.json" -Algorithm SHA256).Hash -ne 'MANIFEST_HASH'){throw 'Manifest hash mismatch'}
if((Get-FileHash "$root\test-guests\guest\shards-testguest" -Algorithm SHA256).Hash -ne 'NEW_GUEST_HASH'){throw 'Guest hash mismatch'}
if((Get-FileHash C:\shards-step10\test-guests\guest\shards-testguest -Algorithm SHA256).Hash -ne 'OLD_GUEST_HASH'){throw 'Original full guest changed'}
$tests=Get-Content "$root\test-manifest.json" -Raw | ConvertFrom-Json
if($tests.Count -ne 142){throw 'Manifest must retain142'}
foreach($test in $tests){if((Get-FileHash $test.exe -Algorithm SHA256).Hash -ne $test.sha256){throw 'Host test executable hash mismatch'}}
$clients=Get-Content "$root\clients\clients.json" -Raw | ConvertFrom-Json
foreach($pair in @(@('docker.exe',$clients.docker_cli.sha256),@('docker-compose.exe',$clients.compose.sha256),@('docker-buildx.exe',$clients.buildx.sha256))){
 if((Get-FileHash "$root\clients\$($pair[0])" -Algorithm SHA256).Hash -ne $pair[1]){throw 'Client hash mismatch'}
}
'Client kit verified;141 original host tests unchanged,one new Buildx test,original full guest unchanged.'
exit 0
'''
for key,value in [('RUNNER_HASH',record['runner_sha256']),('MANIFEST_HASH',record['manifest_sha256']),
                  ('NEW_GUEST_HASH',record['guest_candidate_sha256']),('OLD_GUEST_HASH',record['guest_original_sha256'])]:
    body=body.replace(key,value)
assert lib['remote'](body,'client-coverage-kit-verify01',1200).returncode==0

child=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue';$script:result=2
try{
 & {
  Get-Date -Format o
  if([Diagnostics.Process]::GetCurrentProcess().PriorityClass -ne [Diagnostics.ProcessPriorityClass]::Normal){throw 'Normal priority required'}
  & C:\shards-step10\probe-user.ps1 -User shards-e2e
  if(-not $?){throw 'Standard-user fixture failed'}
  $env:PATH='C:\shards-step10\git\cmd;'+$env:PATH+';C:\shards-step10\git\usr\bin'
  Remove-Item Env:SHARDS_STORM_CPUS -ErrorAction SilentlyContinue
  Remove-Item Env:SHARDS_STORM_MS -ErrorAction SilentlyContinue
  $root='C:\shards-step10-clients01';$script:result=0
  $cases=@(
   @('clients-compose01','compose',''),
   @('clients-buildx01','buildx',''),
   @('clients-engine-cp01','engine','the_docker_cli_checkpoints_and_restores_microvms'),
   @('clients-engine-run01','engine','the_docker_cli_runs_microvms_through_the_engine'),
   @('clients-engine-own01','engine','the_docker_clis_use_api_socket_is_the_microvms_own_engine')
  )
  foreach($case in $cases){
   & "$root\run-tests.ps1" -Kit $root -Kernel C:\shards-step10\Image-6.18.48-aarch64-64a9427a8997 -Tag $case[0] -Binary $case[1] -Test $case[2] -HeartbeatSeconds 30
   $native=$LASTEXITCODE
   if($native -ne 0){$script:result=1}
   if(Select-String -Path "$root\full-$($case[0])\*.stderr.txt" -Pattern '^SKIP:.*(docker|buildx)' -Quiet){throw 'Client-related skip invalidates this selected coverage'}
   'SELECTED CLIENT RESULT tag={0} native={1}' -f $case[0],$native
  }
  'Targeted client checks only; combined/full real-VM qualification still required.'
 } *> C:\shards-step10\client-coverage01-controller.txt
}catch{$_ | Out-String | Add-Content C:\shards-step10\client-coverage01-controller.txt;$script:result=2}
[IO.File]::WriteAllText('C:\shards-step10\client-coverage01-child.exit',[string]$script:result)
exit $script:result
'''
broker=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue';$script:result=2
try{
 & {
  if((Get-FileHash C:\shards-step10-admin\local-user-broker.cs -Algorithm SHA256).Hash -ne 'BROKER_SOURCE_HASH'){throw 'Broker source hash mismatch'}
  Add-Type -Path C:\shards-step10-admin\local-user-broker.cs
  $credential=Import-Clixml C:\shards-step10-admin\local-user.credential.xml
  $user=Get-LocalUser shards-e2e
  if($user.Description -ne 'Temporary Shards step10 native E2E fixture'){throw 'Unexpected user'}
  $script:result=[Step10LocalUserBroker]::Run($user.Name,$credential.GetNetworkCredential().Password,$user.SID.Value,'ENCODED_CHILD',[uint32]::MaxValue)
 } *> C:\shards-step10\client-coverage01-broker.txt
}catch{$_ | Out-String | Add-Content C:\shards-step10\client-coverage01-broker.txt;$script:result=2}
[IO.File]::WriteAllText('C:\shards-step10\client-coverage01-broker.exit',[string]$script:result)
exit $script:result
'''.replace('BROKER_SOURCE_HASH',hashlib.sha256((kit/'local-user-broker.cs').read_bytes()).hexdigest()).replace('ENCODED_CHILD',base64.b64encode(child.encode('utf-16le')).decode())
script=kit/'client-coverage01-broker.ps1'
script.write_text(broker)
subprocess.run(['scp',*lib['options'][:-2],'-P','2222',str(script),
                'shards@127.0.0.1:C:/shards-step10-admin/client-coverage01-broker.ps1'],check=True,timeout=300)
body=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
$s=New-Object -ComObject Schedule.Service;$s.Connect();$f=$s.GetFolder('\ShardsStep10')
foreach($name in @('FullReal04','StormReport01','BuilderPhase01')){if($f.GetTask($name).State -ne 3){throw 'Prerequisite no longer terminal'}}
if(Get-ScheduledTask -TaskName ClientCoverage01 -TaskPath '\ShardsStep10\' -ErrorAction SilentlyContinue){throw 'Existing selected task; no duplicate'}
if(Test-Path C:\shards-step10\client-coverage01-controller.txt){throw 'Existing selected output; no duplicate'}
if((Get-FileHash C:\shards-step10-admin\client-coverage01-broker.ps1 -Algorithm SHA256).Hash -ne 'BROKER_SCRIPT_HASH'){throw 'Broker script hash mismatch'}
$action=New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -Argument '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File C:\shards-step10-admin\client-coverage01-broker.ps1' -WorkingDirectory C:\shards-step10
$principal=New-ScheduledTaskPrincipal -UserId SYSTEM -LogonType ServiceAccount -RunLevel Highest
$settings=New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
$settings.Priority=4
Register-ScheduledTask -TaskName ClientCoverage01 -TaskPath '\ShardsStep10\' -Action $action -Principal $principal -Settings $settings | Select-Object TaskName,State | Format-List
Start-ScheduledTask -TaskName ClientCoverage01 -TaskPath '\ShardsStep10\'
'Targeted client task launched after three native prerequisites were terminal and archived; no combined pass scheduled.'
exit 0
'''.replace('BROKER_SCRIPT_HASH',hashlib.sha256(script.read_bytes()).hexdigest())
assert lib['remote'](body,'client-coverage01-launch',600).returncode==0
record.update(deployed=True,native_result='targeted task launched; observe native results, not this launcher exit')
(candidate/'candidate.json').write_text(json.dumps(record,indent=2)+'\n')
state('targeted client coverage launched; all prerequisites archived first')
lib['log']('Targeted client task launched only; no native pass or combined qualification inferred')
