"""Resume the prepared account without resetting it; test actual user permissions first."""
import base64
import hashlib
import runpy
import subprocess
lib=runpy.run_path('/Users/adalundhe/g121-a4-winarm/trial-vbsoff.py',run_name='fixture_library')
kit=lib['k'];source=kit/'local-user-broker.cs'
child=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
try{
 & {
  Get-Date -Format o
  'PROFILE='+$env:USERPROFILE
  Add-Type -Path C:\shards-step10\local-user-policy.cs
  [Step10LocalUserBroker]::Policy()
  & C:\shards-step10\probe-user.ps1 -User shards-e2e
  if(-not $?){throw 'Standard-user permission probe failed'}
  & C:\shards-step10\probe-gzip.ps1 -Runner C:\shards-step10\run-tests.ps1
  if(-not $?){throw 'gzip probe failed'}
  $root='C:\Users\adalundhe\Projects\shards\.claude\worktrees\g121-wa64\target'
  $dir=Join-Path $root ('permission-probe-'+[Guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory -Path $dir | Out-Null
  try{[IO.File]::WriteAllText((Join-Path $dir 'file'),'standard-user-write');if([IO.File]::ReadAllText((Join-Path $dir 'file')) -ne 'standard-user-write'){throw 'Target read-back differed'}}finally{Remove-Item $dir -Force -Recurse}
  'LOCAL STANDARD-USER PREFLIGHT PASSED: symlink, ETW, gzip and actual target write.'
 } *> C:\shards-step10\local-user-preflight02.txt
 [IO.File]::WriteAllText('C:\shards-step10\local-user-preflight02.exit','0');exit 0
}catch{$_ | Out-String | Add-Content C:\shards-step10\local-user-preflight02.txt;[IO.File]::WriteAllText('C:\shards-step10\local-user-preflight02.exit','1');exit 1}
'''
broker=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
try{
 & {
  Get-Date -Format o
  if((Get-Content C:\shards-step10\local-user-broker01.exit -Raw).Trim() -ne '1'){throw 'Expected failed preparation recorded'}
  $user=Get-LocalUser shards-e2e
  if($user.Description -ne 'Temporary Shards step10 native E2E fixture'){throw 'Unexpected account; no reset'}
  if((Get-FileHash C:\shards-step10-admin\local-user-broker.cs -Algorithm SHA256).Hash -ne 'EXPECTED_SOURCE_SHA256'){throw 'Broker source hash mismatch'}
  Add-Type -Path C:\shards-step10-admin\local-user-broker.cs
  $credential=Import-Clixml C:\shards-step10-admin\local-user.credential.xml
  $code=[Step10LocalUserBroker]::Run($user.Name,$credential.GetNetworkCredential().Password,$user.SID.Value,'EXPECTED_CHILD',600000)
  if($code){throw "Native child exit=$code"}
 } *> C:\shards-step10\local-user-broker02.txt
 [IO.File]::WriteAllText('C:\shards-step10\local-user-broker02.exit','0');exit 0
}catch{$_ | Out-String | Add-Content C:\shards-step10\local-user-broker02.txt;[IO.File]::WriteAllText('C:\shards-step10\local-user-broker02.exit','1');exit 1}
'''.replace('EXPECTED_SOURCE_SHA256',hashlib.sha256(source.read_bytes()).hexdigest()).replace('EXPECTED_CHILD',base64.b64encode(child.encode('utf-16le')).decode())
path=kit/'local-user-broker02.ps1';path.write_text(broker)
subprocess.run(['scp',*lib['options'][:-2],'-P','2222',str(path),'shards@127.0.0.1:C:/shards-step10-admin/local-user-broker02.ps1'],check=True,timeout=300)
body=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
if(Get-ScheduledTask -TaskName LocalUserBroker02 -TaskPath '\ShardsStep10\' -ErrorAction SilentlyContinue){throw 'Existing broker; no duplicate'}
if(Test-Path C:\shards-step10\local-user-broker02.txt){throw 'Previous output exists'}
if((Get-FileHash C:\shards-step10-admin\local-user-broker02.ps1 -Algorithm SHA256).Hash -ne 'EXPECTED_SCRIPT_SHA256'){throw 'Broker script hash mismatch'}
$action=New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -Argument '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File C:\shards-step10-admin\local-user-broker02.ps1' -WorkingDirectory C:\shards-step10
$principal=New-ScheduledTaskPrincipal -UserId SYSTEM -LogonType ServiceAccount -RunLevel Highest
$settings=New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 20) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName LocalUserBroker02 -TaskPath '\ShardsStep10\' -Action $action -Principal $principal -Settings $settings | Select-Object TaskName,State | Format-List
Start-ScheduledTask -TaskName LocalUserBroker02 -TaskPath '\ShardsStep10\'
'Actual limited-user preflight launched; recursive build-output ACL traversal is not a prerequisite.'
exit 0
'''.replace('EXPECTED_SCRIPT_SHA256',hashlib.sha256(path.read_bytes()).hexdigest())
status=lib['remote'](body,'local-user-broker-launch02',600)
assert status.returncode==0
print('Resumed existing account through normal batch logon; no account reset',flush=True)
