from pathlib import Path
import hashlib,subprocess,json,os,difflib
r=Path.home()/'root-managed-sockaddr-setns-step-v1'
p=Path('crates/testguest/src/managed_network/intercept/sock_addr.rs');original=p.read_bytes();text=original.decode();begin=text.index('fn in_mount')
needle='''                if rc != 0 {
                    return Err(io::Error::last_os_error());
                }
                f()
'''
assert text[begin:].count(needle)==1
replacement='''                let failure = (rc != 0).then(io::Error::last_os_error);
                let _ = record(format_args!(
                    "{{\\\"type\\\":\\\"MountNamespaceStep\\\",\\\"operation\\\":\\\"{}\\\",\\\"return\\\":{},\\\"errno\\\":{}}}",
                    if namespace.is_some() { "SetnsHeldMount" } else { "UnshareNewMount" },
                    rc,
                    failure.as_ref().and_then(io::Error::raw_os_error).unwrap_or(0)
                ));
                if let Some(error) = failure {
                    return Err(error);
                }
                f()
'''
updated=(text[:begin]+text[begin:].replace(needle,replacement,1)).encode()
(r/'original.rs').write_bytes(original)
(r/'instrumentation.patch').write_text(''.join(difflib.unified_diff(text.splitlines(True),updated.decode().splitlines(True),fromfile=str(p),tofile=str(p))))
(r/'source.json').write_text(json.dumps({'head':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),'original_sha256':hashlib.sha256(original).hexdigest(),'instrumented_sha256':hashlib.sha256(updated).hexdigest()},indent=2)+'\n')
cmd=json.loads((Path.home()/'root-managed-sockaddr-2228-runtime-v2/connected/command.json').read_text());inputs=json.loads((Path.home()/'root-managed-sockaddr-2228-runtime-v2/connected/inputs.json').read_text());inputs['SHARDS_NETWORK_RECORDS']=str(r/'connected');(r/'connected').mkdir()
(r/'inputs.json').write_text(json.dumps(inputs,indent=2)+'\n');(r/'command.json').write_text(json.dumps(cmd)+'\n')
env=os.environ.copy();env.update(inputs);env['SHARDS_REQUIRE_VMS']='1';env['CARGO_TARGET_DIR']=str(Path.home()/'managed-capture-target');env.pop('SHARDS_HELPERS',None)
try:
 p.write_bytes(updated);p.touch()
 with (r/'driver.log').open('wb') as out:result=subprocess.run(cmd,env=env,stdout=out,stderr=subprocess.STDOUT)
 (r/'cargo.exit').write_text(str(result.returncode)+'\n')
 stdout=r/'connected/sock-addr-udp4-connected.stdout'
 print('actual instrumented cargo exit',result.returncode)
 if stdout.exists():
  for line in stdout.read_text(errors='replace').splitlines():
   if 'MountNamespaceStep' in line or 'RemoveCgroup' in line or '"type":"Cleanup"' in line:print(line)
finally:
 p.write_bytes(original);p.touch();assert p.read_bytes()==original
 (r/'restored.sha256').write_text(hashlib.sha256(p.read_bytes()).hexdigest()+'\n')
 (r/'restored.diff').write_bytes(subprocess.check_output(['git','diff','--',str(p)]))
