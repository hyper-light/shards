"""Prepare a dedicated standard batch logon; credential never leaves Windows."""
import base64
import hashlib
import runpy
import subprocess

lib=runpy.run_path('/Users/adalundhe/g121-a4-winarm/trial-vbsoff.py',run_name='fixture_library')
kit=lib['k']
source=kit/'local-user-broker.cs'
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
  'LOCAL STANDARD-USER PREFLIGHT PASSED'
 } *> C:\shards-step10\local-user-preflight01.txt
 [IO.File]::WriteAllText('C:\shards-step10\local-user-preflight01.exit','0');exit 0
}catch{$_ | Out-String | Add-Content C:\shards-step10\local-user-preflight01.txt;[IO.File]::WriteAllText('C:\shards-step10\local-user-preflight01.exit','1');exit 1}
'''
encoded=base64.b64encode(child.encode('utf-16le')).decode()
broker=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
try{
 & {
  Get-Date -Format o
  if([Security.Principal.WindowsIdentity]::GetCurrent().User.Value -ne 'S-1-5-18'){throw 'SYSTEM fixture broker required'}
  if((Get-FileHash C:\shards-step10-admin\local-user-broker.cs -Algorithm SHA256).Hash -ne 'EXPECTED_SOURCE_SHA256'){throw 'Broker source hash mismatch'}
  if(Get-LocalUser shards-e2e -ErrorAction SilentlyContinue){throw 'Dedicated fixture account already exists; do not reset it'}
  $random=New-Object byte[] 32
  $rng=[Security.Cryptography.RandomNumberGenerator]::Create()
  try{$rng.GetBytes($random)}finally{$rng.Dispose()}
  $secret=[Convert]::ToBase64String($random)+'!aA9'
  $secure=ConvertTo-SecureString $secret -AsPlainText -Force
  $user=New-LocalUser -Name shards-e2e -Description 'Temporary Shards step10 native E2E fixture' -Password $secure -PasswordNeverExpires
  Add-LocalGroupMember -Group (Get-LocalGroup -SID 'S-1-5-32-545').Name -Member $user
  Add-LocalGroupMember -Group (Get-LocalGroup -SID 'S-1-5-32-559').Name -Member $user
  if(@(Get-LocalGroupMember -Group (Get-LocalGroup -SID 'S-1-5-32-544').Name | Where-Object {$_.SID.Value -eq $user.SID.Value}).Count){throw 'Fixture account unexpectedly in Administrators'}
  $credential=[Management.Automation.PSCredential]::new("$env:COMPUTERNAME\shards-e2e",$secure)
  $credential | Export-Clixml C:\shards-step10-admin\local-user.credential.xml
  $grant='*'+$user.SID.Value+':(OI)(CI)M'
  foreach($dir in @('C:\shards-step10','C:\Users\adalundhe\Projects\shards\.claude\worktrees\g121-wa64\target')){
   & icacls.exe $dir /grant $grant /T /C /Q
   if($LASTEXITCODE){throw 'Fixture directory ACL failed'}
  }
  'Created dedicated standard account='+$user.Name+' SID='+$user.SID.Value+'; generated credential DPAPI-protected inside Windows only.'
  Add-Type -Path C:\shards-step10-admin\local-user-broker.cs
  $code=[Step10LocalUserBroker]::Run($user.Name,$secret,$user.SID.Value,'EXPECTED_CHILD',600000)
  $secret=$null;$secure=$null;$credential=$null
  if($code){throw "Native child exit=$code"}
 } *> C:\shards-step10\local-user-broker01.txt
 [IO.File]::WriteAllText('C:\shards-step10\local-user-broker01.exit','0');exit 0
}catch{$_ | Out-String | Add-Content C:\shards-step10\local-user-broker01.txt;[IO.File]::WriteAllText('C:\shards-step10\local-user-broker01.exit','1');exit 1}
'''.replace('EXPECTED_SOURCE_SHA256',hashlib.sha256(source.read_bytes()).hexdigest()).replace('EXPECTED_CHILD',encoded)
path=kit/'local-user-broker01.ps1';path.write_text(broker)
for path,remote in [(source,'C:/shards-step10-admin/local-user-broker.cs'),(path,'C:/shards-step10-admin/local-user-broker01.ps1'),(source,'C:/shards-step10/local-user-policy.cs')]:
 subprocess.run(['scp',*lib['options'][:-2],'-P','2222',str(path),'shards@127.0.0.1:'+remote],check=True,timeout=300)
body=r'''$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue'
if(Get-ScheduledTask -TaskName LocalUserBroker01 -TaskPath '\ShardsStep10\' -ErrorAction SilentlyContinue){throw 'Existing broker; no duplicate'}
if(Test-Path C:\shards-step10\local-user-broker01.txt){throw 'Previous output exists'}
if((Get-FileHash C:\shards-step10-admin\local-user-broker01.ps1 -Algorithm SHA256).Hash -ne 'EXPECTED_SCRIPT_SHA256'){throw 'Broker script hash mismatch'}
$action=New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -Argument '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File C:\shards-step10-admin\local-user-broker01.ps1' -WorkingDirectory C:\shards-step10
$principal=New-ScheduledTaskPrincipal -UserId SYSTEM -LogonType ServiceAccount -RunLevel Highest
$settings=New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 20) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName LocalUserBroker01 -TaskPath '\ShardsStep10\' -Action $action -Principal $principal -Settings $settings | Select-Object TaskName,State | Format-List
Start-ScheduledTask -TaskName LocalUserBroker01 -TaskPath '\ShardsStep10\'
'Dedicated standard-logon preflight started; original runner identity unchanged.'
exit 0
'''.replace('EXPECTED_SCRIPT_SHA256',hashlib.sha256((kit/'local-user-broker01.ps1').read_bytes()).hexdigest())
status=lib['remote'](body,'local-user-broker-launch01',600)
assert status.returncode==0
print('Dedicated standard-user probe launched; inspect actual permissions and native exit',flush=True)
