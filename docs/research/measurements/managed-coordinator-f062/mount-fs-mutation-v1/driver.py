from pathlib import Path
import os,json,subprocess,hashlib,difflib
r=Path.home()/'root-managed-mount-fs-mutation-f062-v1';p=Path('crates/testguest/src/managed_network/intercept/sock_addr.rs');original=p.read_bytes();assert hashlib.sha256(original).hexdigest()=='52b4c270623abac18894c98cfd1bca880fd091804dac56488706f12c9ec4a5f5'
needle=b'''                            if libc::unshare(libc::CLONE_FS) != 0 {
                                return Err(io::Error::last_os_error());
                            }
''';assert original.count(needle)==1;mutated=original.replace(needle,b'',1)
(r/'original.rs').write_bytes(original);(r/'mutation.patch').write_text(''.join(difflib.unified_diff(original.decode().splitlines(True),mutated.decode().splitlines(True),fromfile=str(p),tofile=str(p))))
(r/'source.json').write_text(json.dumps({'head':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),'fixed_sha256':hashlib.sha256(original).hexdigest(),'mutant_sha256':hashlib.sha256(mutated).hexdigest()},indent=2)+'\n')
binary=json.loads((Path.home()/'root-managed-combined-f062-v1/test-artifacts.json').read_text())['managed_network']['path'];inputs=json.loads((Path.home()/'root-managed-sockaddr-2228-runtime-v2/connected/inputs.json').read_text());env=os.environ.copy();env.update(inputs);env['CARGO_TARGET_DIR']=str(Path.home()/'managed-capture-target');env['SHARDS_REQUIRE_VMS']='1';env.pop('SHARDS_HELPERS',None)
cmd=[binary,'direct_guest_socket_address_udp4_requires_original_native_tuple','--exact','--ignored','--nocapture','--test-threads=1'];(r/'command.json').write_text(json.dumps(cmd)+'\n')
def run(name,expected_cleanup):
 dest=r/name;dest.mkdir();env['SHARDS_NETWORK_RECORDS']=str(dest);(dest/'environment.json').write_text(json.dumps({k:env[k] for k in inputs}|{'SHARDS_NETWORK_RECORDS':str(dest)},indent=2)+'\n')
 with (dest/'test.log').open('wb') as out:result=subprocess.run(cmd,env=env,stdout=out,stderr=subprocess.STDOUT)
 (dest/'test.exit').write_text(str(result.returncode)+'\n')
 records=[]
 for line in (dest/'sock-addr-udp4-connected.stdout').read_text(errors='replace').splitlines():
  if line.startswith(('managed-sock-addr ','managed-intercept ')):records.append(json.loads(line.split(' ',1)[1]))
 (dest/'observed.json').write_text(json.dumps(records,indent=2)+'\n')
 cleanup=[i for i in records if i['type']=='Cleanup' and i['state']=='Observed'];assert len(cleanup)==1 and cleanup[0]['cgroup_removed']==expected_cleanup,cleanup
 assert all(cleanup[0][k] for k in ['backend_joined','relay_joined','link_detached']),cleanup
 reaped=[i for i in records if i['type']=='ChildReaped'];assert len(reaped)==2 and all(i['reaped'] for i in reaped)
 failures=[i for i in records if i['type']=='Error' and i['stage']=='RemoveCgroup'];assert (not failures if expected_cleanup else all(i['errno']==22 for i in failures) and failures),failures
 clients=[i for i in records if i['type']=='ClientCompletion'];assert clients[-1]['compatibility']=='Mismatch' and result.returncode!=0
 print(name,'cgroup_removed',cleanup[0]['cgroup_removed'],'tuple compatibility remains Mismatch; actual exit',result.returncode,flush=True)
try:
 p.write_bytes(mutated);p.touch();run('removed-fs-unshare',False)
finally:
 p.write_bytes(original);p.touch();assert p.read_bytes()==original
 (r/'restored.sha256').write_text(hashlib.sha256(p.read_bytes()).hexdigest()+'\n');(r/'restored.diff').write_bytes(subprocess.check_output(['git','diff','--',str(p)]))
run('restored',True)
